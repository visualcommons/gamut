//! The [`JxlDecoder`]: a typed front end over a stack of JPEG XL codestream decoders — any backend
//! added with [`JxlDecoder::with_backend`], and, last, the built-in pure-Rust jxl-rs wrapper
//! ([`crate::jxlrs`]).

use std::sync::{Arc, Mutex};

#[cfg(feature = "decode")]
use gamut_core::RawMetadata;
use gamut_core::convert::ConvertPolicy;
use gamut_core::{
    DecodeImage, DecodeLimits, Error, Gray8, Gray16, GrayAlpha8, GrayAlpha16, ImageBuf, Pixel,
    PixelFormat, Result, Rgb8, Rgb16, Rgba8, Rgba16,
};

use crate::backend::{
    self, DecoderRegistry, JxlCodestreamDecoder, JxlDecoded, JxlFraming, JxlOwnedSamples,
    JxlStreamInfo,
};
#[cfg(feature = "decode")]
pub use crate::jxlrs::{JxlInfo, JxlPartialReport, JxlRender};

/// The refusal returned when no backend can decode: nothing was pushed, and the built-in jxl-rs
/// tail is not compiled into this build (no `decode` feature).
///
/// Always compiled (and unit-tested) even where the tail *is* present, so its message stays pinned
/// on every build rather than only on the targets that can return it.
#[cfg_attr(feature = "decode", allow(dead_code))]
fn no_decode_backend() -> Error {
    Error::unsupported(
        env!("CARGO_PKG_NAME"),
        "JXL: no decode backend (enable the `decode` feature or push a codestream backend)",
    )
}

/// The error for a backend that returned a raster in a layout other than the one requested.
fn wrong_backend_layout() -> Error {
    Error::invalid_input(
        env!("CARGO_PKG_NAME"),
        "JXL: backend returned a raster in the wrong pixel layout",
    )
}

/// Locates the `Exif` and `xml ` boxes of an ISO BMFF `.jxl` container: the TIFF stream behind
/// the `Exif` box's tiff-header offset, and the `xml ` payload verbatim. For a duplicated box the
/// first wins (the workspace's JPEG convention).
///
/// jxl-rs consumes auxiliary boxes without exposing them, so the walk is this crate's own: the
/// plain top-level box sequence of ISO/IEC 14496-12 §4.2 (32-bit size, `size == 1` with a 64-bit
/// `largesize`, `size == 0` meaning "to the end of the file"). Box *contents* are never
/// interpreted beyond the two metadata types, so a codestream (`jxlc`/`jxlp`), `jbrd`, or unknown
/// box is stepped over by length.
///
/// # Errors
///
/// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error for a
/// truncated or malformed box header, a box overrunning the stream, or an `Exif` payload shorter
/// than its offset field or with the offset past its end; an
/// [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) error for a Brotli-compressed (`brob`) `Exif`/`xml ` box, which this crate
/// cannot decompress. Any other `brob` box is skipped.
#[cfg(feature = "decode")]
fn container_metadata_boxes(data: &[u8]) -> Result<MetadataBoxes> {
    let mut exif = None;
    let mut xmp = None;
    let mut rest = data;
    while !rest.is_empty() {
        let (box_type, header_len, box_len) = parse_box_header(rest)?;
        if box_len > rest.len() {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "JXL: box overruns the stream",
            ));
        }
        // §4.2: a box's `size` counts its own header, so it is never below `MIN_BOX_HEADER`. The
        // walk enforces that here rather than trusting the header parser, which also makes the
        // loop's progress local: every iteration consumes at least `MIN_BOX_HEADER` bytes of
        // `rest`, so the walk terminates whatever the parser reports.
        if box_len < MIN_BOX_HEADER {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "JXL: malformed box size",
            ));
        }
        // `header_len > box_len` is the 64-bit form declaring a `largesize` smaller than the
        // header it is part of; the body is then not a range at all.
        let Some(body) = rest.get(header_len..box_len) else {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "JXL: malformed box size",
            ));
        };
        match &box_type {
            b"brob" => {
                let Some(inner) = body.get(..4) else {
                    return Err(Error::invalid_input(
                        env!("CARGO_PKG_NAME"),
                        "JXL: truncated brob box",
                    ));
                };
                if inner == b"Exif" || inner == b"xml " {
                    return Err(Error::unsupported(
                        env!("CARGO_PKG_NAME"),
                        "JXL: Brotli-compressed metadata box (brob) is not supported",
                    ));
                }
            }
            b"Exif" if exif.is_none() => exif = Some(exif_box_tiff_stream(body)?.to_vec()),
            b"xml " if xmp.is_none() => xmp = Some(body.to_vec()),
            _ => {}
        }
        rest = &rest[box_len..];
    }
    Ok((exif, xmp))
}

/// The located `Exif` TIFF stream and `xml ` payload of a container, each `None` when absent.
#[cfg(feature = "decode")]
type MetadataBoxes = (Option<Vec<u8>>, Option<Vec<u8>>);

/// The smallest ISO BMFF box header (§4.2): a 4-byte `size` and a 4-byte type.
#[cfg(feature = "decode")]
const MIN_BOX_HEADER: usize = 8;

/// The 64-bit box header: [`MIN_BOX_HEADER`] plus the 8-byte `largesize` that follows the type.
#[cfg(feature = "decode")]
const LARGE_BOX_HEADER: usize = 16;

/// Parses the box header at the start of `data`, returning its type, the length of the header
/// itself, and the length of the whole box (header included) as the header declares it.
///
/// Nothing here is validated against `data`'s length, and nothing is sliced: the returned lengths
/// are what the header *claims*, and the caller — which owns the walk's progress — is what checks
/// them (see [`container_metadata_boxes`]).
///
/// # Errors
///
/// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if `data` is
/// too short to hold the header it announces.
#[cfg(feature = "decode")]
fn parse_box_header(data: &[u8]) -> Result<([u8; 4], usize, usize)> {
    let [s0, s1, s2, s3, t0, t1, t2, t3, tail @ ..] = data else {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "JXL: truncated box header",
        ));
    };
    let box_type = [*t0, *t1, *t2, *t3];
    match u32::from_be_bytes([*s0, *s1, *s2, *s3]) {
        // `size == 0`: the box extends to the end of the file.
        0 => Ok((box_type, MIN_BOX_HEADER, data.len())),
        // `size == 1`: a 64-bit `largesize` follows the type.
        1 => {
            let [l0, l1, l2, l3, l4, l5, l6, l7, ..] = tail else {
                return Err(Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "JXL: truncated box header",
                ));
            };
            let large = u64::from_be_bytes([*l0, *l1, *l2, *l3, *l4, *l5, *l6, *l7]);
            // A length no address space can hold cannot be a length within `data` either, so it
            // saturates and the caller's overrun check reports it.
            Ok((
                box_type,
                LARGE_BOX_HEADER,
                usize::try_from(large).unwrap_or(usize::MAX),
            ))
        }
        size => Ok((box_type, MIN_BOX_HEADER, size as usize)),
    }
}

/// The TIFF stream of an `Exif` box payload: skips the 4-byte big-endian `exif_tiff_header_offset`
/// and then `offset` further bytes (ISO/IEC 23008-12 §A.2.1).
#[cfg(feature = "decode")]
fn exif_box_tiff_stream(payload: &[u8]) -> Result<&[u8]> {
    let [o0, o1, o2, o3, rest @ ..] = payload else {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "JXL: truncated Exif box",
        ));
    };
    usize::try_from(u32::from_be_bytes([*o0, *o1, *o2, *o3]))
        .ok()
        .and_then(|offset| rest.get(offset..))
        .ok_or_else(|| {
            Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "JXL: Exif box tiff-header offset out of range",
            )
        })
}

/// A JPEG XL decoder.
///
/// Decodes both JPEG XL framings — a bare codestream and the ISO BMFF `.jxl` container — into any of
/// the eight supported pixel layouts (8/16-bit grayscale, gray+alpha, RGB, RGBA) through the
/// [`DecodeImage`](gamut_core::DecodeImage) trait. Where the requested layout and the stream differ,
/// [`gamut_core::convert`] reconciles them: grayscale expands to RGB and a missing alpha channel is
/// padded opaque, since neither loses anything.
///
/// Anything **lossy** — dropping a present alpha channel, reducing colour to grayscale — is refused
/// by default and needs [`JxlDecoder::with_convert_policy`]. Animated input and premultiplied
/// (associated) alpha are rejected outright, each an
/// [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) error a later version may relax.
///
/// Construct it with [`JxlDecoder::new`] or [`Default`], then optionally set
/// [`JxlDecoder::with_codestream_bit_depth`], [`JxlDecoder::with_convert_policy`], and the
/// hostile-input [`DecodeLimits`] through [`JxlDecoder::with_limits`] (by default
/// [`DecodeLimits::DEFAULT_MAX_IMAGE_BYTES`], 64 MiB, of decoded samples, beneath a fixed
/// `1 << 28`-sample ceiling in the built-in tail).
///
/// # Backends
///
/// The codestream itself is decoded by a [`JxlCodestreamDecoder`]. With the `decode` feature the
/// pure-Rust jxl-rs wrapper is the implicit **last** backend, so the default decoder needs no
/// wiring. [`JxlDecoder::with_backend`] inserts a platform or alternate decoder *ahead* of it; with
/// neither an added backend nor the built-in tail, decoding reports an
/// [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) error. See
/// [the crate docs](crate#backends) for the fallback contract.
///
/// The type is `Clone` but **not `Copy`**: it owns a backend list. Cloning copies the list,
/// sharing the backend instances themselves (see [`with_backend`](Self::with_backend)).
/// `PartialEq`/`Eq` compare the **configuration** only, since backends are opaque.
#[derive(Debug, Clone, Default)]
pub struct JxlDecoder {
    /// Whether integer output carries the codestream's declared bit depth.
    codestream_bit_depth: bool,
    /// Which lossy layout conversions a typed decode may perform.
    policy: ConvertPolicy,
    /// The caller's dimension and byte budget for a decoded raster.
    limits: DecodeLimits,
    /// Added codestream backends, tried in push order ahead of the built-in jxl-rs tail.
    backends: DecoderRegistry,
}

impl PartialEq for JxlDecoder {
    /// Compares the decoder **configuration**; the backend registries are ignored, since a
    /// [`JxlCodestreamDecoder`] is an opaque trait object with no notion of equality.
    fn eq(&self, other: &Self) -> bool {
        self.codestream_bit_depth == other.codestream_bit_depth
            && self.policy == other.policy
            && self.limits == other.limits
    }
}

impl Eq for JxlDecoder {}

/// Refuses a `width` × `height` raster of `bytes_per_pixel`-byte pixels that exceeds `limits`,
/// checked against the stream's parsed image header before any pixel buffer is sized. A byte count
/// that overflows `usize` exceeds every budget.
pub(crate) fn check_limits(
    limits: DecodeLimits,
    width: u32,
    height: u32,
    bytes_per_pixel: usize,
) -> Result<()> {
    if width > limits.max_width() || height > limits.max_height() {
        return Err(Error::unsupported(
            env!("CARGO_PKG_NAME"),
            "JXL: image exceeds the dimension limit",
        ));
    }
    let bytes = (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(bytes_per_pixel));
    if bytes.is_none_or(|bytes| bytes > limits.max_image_bytes()) {
        return Err(Error::unsupported(
            env!("CARGO_PKG_NAME"),
            "JXL: image exceeds the size limit",
        ));
    }
    Ok(())
}

/// What [`JxlDecoder::decode_partial`] returns: the best-effort image and how far the decode got.
#[cfg(feature = "decode")]
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct JxlPartialImage<P: Pixel> {
    /// The decoded image — complete, partly drawn, or blank; [`report`](Self::report) says which.
    pub image: ImageBuf<P>,
    /// How much of the image [`image`](Self::image) carries.
    pub report: JxlPartialReport,
}

impl JxlDecoder {
    /// Creates a decoder with the default configuration.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the hostile-input [`DecodeLimits`]: an image wider, taller, or larger in decoded bytes
    /// than they allow is refused with an
    /// [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) error once its header is
    /// parsed, before any pixel buffer is allocated. Returns the updated decoder for chaining.
    ///
    /// The default is [`DecodeLimits::new`]: any width and height, and
    /// [`DecodeLimits::DEFAULT_MAX_IMAGE_BYTES`] (64 MiB) of decoded samples. The budgeted
    /// quantity is `width × height × bytes per pixel` of the wider of the stream's natural layout
    /// and the requested one — the largest raster the decode materialises. It bounds the
    /// zero-filled `JxlRender::HeaderOnly` buffer the best-effort `decode_partial` returns for a
    /// stream cut short after its headers, too.
    ///
    /// The limits only ever tighten what the decoder accepts: the built-in tail's fixed jxl-rs
    /// pixel limit of `1 << 28` samples still applies on top. A raster an added backend returns is
    /// held to the same limits.
    #[must_use]
    pub fn with_limits(mut self, limits: DecodeLimits) -> Self {
        self.limits = limits;
        self
    }

    /// The configured [`DecodeLimits`] (see [`JxlDecoder::with_limits`]).
    #[must_use]
    pub fn limits(&self) -> DecodeLimits {
        self.limits
    }

    /// Selects whether integer output carries the **codestream's declared bit depth** instead of
    /// the output type's full range. Off by default.
    ///
    /// A JPEG XL stream declares its samples' bit depth N (e.g. 10-bit). By default a
    /// 16-bit decode scales samples to full-range `0 ..= 65535`; with this set, samples keep
    /// their coded range `0 ..= 2^N - 1` — the reading a raw-code-value consumer (e.g. an N-bit
    /// DNG tile) needs. Streams with a float sample type are unaffected. Returns the updated
    /// decoder for chaining.
    #[must_use]
    pub fn with_codestream_bit_depth(mut self, enabled: bool) -> Self {
        self.codestream_bit_depth = enabled;
        self
    }

    /// Whether integer output carries the codestream's declared bit depth (see
    /// [`JxlDecoder::with_codestream_bit_depth`]).
    #[must_use]
    pub fn codestream_bit_depth(&self) -> bool {
        self.codestream_bit_depth
    }

    /// Selects which lossy conversions a typed decode may perform when the stream's layout differs
    /// from the requested one.
    ///
    /// Defaults to [`ConvertPolicy::lossless`]: a stream carrying alpha cannot be decoded as an
    /// alpha-less layout, and a colour stream cannot be decoded as grayscale. Grayscale still
    /// widens into RGB and opaque alpha is still added, since neither loses anything. Returns the
    /// updated decoder for chaining.
    #[must_use]
    pub fn with_convert_policy(mut self, policy: ConvertPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// The conversion policy typed decodes apply (see [`JxlDecoder::with_convert_policy`]).
    pub fn convert_policy(&self) -> ConvertPolicy {
        self.policy
    }

    /// Adds a [`JxlCodestreamDecoder`] to the end of this decoder's backend list, ahead of the
    /// built-in jxl-rs tail. Returns the updated decoder for chaining.
    ///
    /// Backends are tried in push order; the first whose
    /// [`supports`](JxlCodestreamDecoder::supports) returns `true` produces the raster, and the
    /// built-in wrapper (when compiled in) is tried last. `supports` is the only fall-through: a
    /// backend that accepts a stream and then fails propagates its error — see
    /// [the crate docs](crate#backends) for the full contract. `Send` is bound here, where the
    /// backend is inserted, rather than on the trait, so the decoder stays `Send + Sync`.
    ///
    /// A backend must return **exactly** the layout
    /// [`JxlStreamInfo::format`](crate::JxlStreamInfo::format) asks for; the host does not reshape
    /// its output, and a mismatch is a typed error rather than a silent conversion.
    ///
    /// # Cloning shares backends
    ///
    /// Backends are stored behind `Arc<Mutex<..>>`, so **cloning a `JxlDecoder` shares them with
    /// the clone** rather than duplicating them: a stateful backend sees the decodes of every clone,
    /// and its lock serializes concurrent ones. A backend added to one decoder after the clone is
    /// not seen by the other.
    #[must_use]
    pub fn with_backend(mut self, backend: impl JxlCodestreamDecoder + Send + 'static) -> Self {
        self.backends.push(Arc::new(Mutex::new(backend)));
        self
    }

    /// Parses the stream's headers and returns its basic properties without decoding any pixels.
    ///
    /// Always uses the built-in jxl-rs header parser — an added backend is not consulted, so the
    /// answer is the crate's own reading of the stream.
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if the
    /// data is not a decodable JPEG XL stream or is truncated before the image headers.
    #[cfg(feature = "decode")]
    pub fn info(&self, data: &[u8]) -> Result<JxlInfo> {
        crate::jxlrs::info(data)
    }

    /// Returns the ICC profile **embedded** in the stream's metadata, or `None` when the stream
    /// signals its colour as a structured (enumerated) encoding — sRGB, PQ, HLG, and friends —
    /// instead of carrying profile bytes.
    ///
    /// Only the stream's headers are parsed; no pixels are decoded. The returned bytes are exactly
    /// the attached profile (what
    /// [`JxlEncoder::with_icc_profile`](crate::JxlEncoder::with_icc_profile) set at encode time,
    /// when the stream was produced by gamut). This is a metadata accessor: the pixel-decoding paths apply no ICC
    /// transform, so decoded samples are in this profile only when
    /// [`JxlInfo::uses_original_profile`] is `true`; an XYB-coded stream decodes to sRGB instead.
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if the
    /// data is not a decodable JPEG XL stream or is truncated before the colour metadata.
    #[cfg(feature = "decode")]
    pub fn embedded_icc_profile(&self, data: &[u8]) -> Result<Option<Vec<u8>>> {
        crate::jxlrs::embedded_icc_profile(data)
    }

    /// Reads the stream's embedded metadata without decoding any pixels: the container's `Exif`
    /// and `xml ` boxes (what [`JxlEncoder::with_exif`](crate::JxlEncoder::with_exif) /
    /// [`with_xmp`](crate::JxlEncoder::with_xmp) wrote) plus the codestream's ICC profile (as
    /// [`embedded_icc_profile`](Self::embedded_icc_profile)). A bare codestream has no boxes, so
    /// only the ICC field can be set for one.
    ///
    /// The boxes are located by this crate's own walk of the container's top-level box sequence —
    /// the pure-Rust decode tail does not expose them — and an added backend is not consulted. The
    /// `Exif` payload's 4-byte tiff-header offset is applied (the JPEG XL container reuses HEIF's
    /// `ExifDataBlock`, ISO/IEC 23008-12 §A.2.1; `references/jxl/format_overview.md`), so
    /// [`RawMetadata::exif`] is the TIFF stream itself; for a duplicated box the first wins. The
    /// XMP packet is the `xml ` payload verbatim, and [`RawMetadata::icc`] is `None` for a
    /// structured colour encoding. A Brotli-compressed (`brob`) `Exif` / `xml ` box is refused
    /// rather than silently skipped. With the `metadata` feature the facade's `TypedMetadata`
    /// impl on [`RawMetadata`] parses the result into a unified `Metadata`.
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if the
    /// data carries neither signature, is truncated before the colour metadata, or has a malformed
    /// box sequence (a truncated header, a box overrunning the stream, an `Exif` payload shorter
    /// than its offset field or with the offset past its end); an
    /// [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) error for a `brob`-wrapped
    /// `Exif` / `xml ` box.
    ///
    /// The call is strict: a codestream fault fails it even when the boxes were located, so a
    /// caller recovering what a damaged file still carries reads
    /// [`metadata_boxes`](Self::metadata_boxes) instead.
    #[cfg(feature = "decode")]
    pub fn metadata(&self, data: &[u8]) -> Result<RawMetadata> {
        let mut metadata = self.metadata_boxes(data)?;
        metadata.icc = crate::jxlrs::embedded_icc_profile(data)?;
        Ok(metadata)
    }

    /// Reads only the container's `Exif` and `xml ` boxes, never parsing the codestream: the
    /// box-level half of [`metadata`](Self::metadata), with the same box walk and the same
    /// refusals, and [`RawMetadata::icc`] always `None` (not consulted, rather than "a structured
    /// colour encoding"). A bare codestream has no boxes, so it yields an empty [`RawMetadata`].
    ///
    /// This is the reader for a file whose codestream is damaged, truncated, or uses a feature the
    /// built-in decoder refuses: the boxes it carries are still returned. To add the ICC profile
    /// where the codestream allows, assign
    /// [`embedded_icc_profile`](Self::embedded_icc_profile)'s answer to the `icc` field.
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if the
    /// data carries neither signature or has a malformed box sequence (a truncated header, a box
    /// overrunning the stream, an `Exif` payload shorter than its offset field or with the offset
    /// past its end); an [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) error for
    /// a `brob`-wrapped `Exif` / `xml ` box.
    #[cfg(feature = "decode")]
    pub fn metadata_boxes(&self, data: &[u8]) -> Result<RawMetadata> {
        let (exif, xmp) = match JxlFraming::detect(data) {
            JxlFraming::IsoBmff => container_metadata_boxes(data)?,
            JxlFraming::Codestream => (None, None),
            JxlFraming::Unknown => {
                return Err(Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "JXL: neither the codestream nor the container signature",
                ));
            }
        };
        let mut metadata = RawMetadata::default();
        metadata.exif = exif;
        metadata.xmp = xmp;
        Ok(metadata)
    }

    /// The stream's dimensions when the built-in header parser can determine them, else `None`.
    ///
    /// Used only to populate [`JxlStreamInfo::dimensions`](crate::JxlStreamInfo::dimensions), and
    /// only when a backend has actually been added, so the default decode path never pays for it.
    fn probe_dimensions(&self, data: &[u8]) -> Option<gamut_core::Dimensions> {
        #[cfg(feature = "decode")]
        {
            crate::jxlrs::info(data).ok().map(|info| info.dimensions)
        }
        #[cfg(not(feature = "decode"))]
        {
            let _ = data;
            None
        }
    }

    /// Runs the added backends over `data` in push order, returning the first accepted raster or
    /// `None` when every backend declined (so the caller falls through to the built-in tail).
    fn dispatch_backends(&self, data: &[u8], format: PixelFormat) -> Result<Option<JxlDecoded>> {
        if self.backends.is_empty() {
            return Ok(None);
        }
        let info = JxlStreamInfo::new(
            format,
            JxlFraming::detect(data),
            self.probe_dimensions(data),
            self.codestream_bit_depth,
        );
        let Some(decoded) = backend::decode_with_backends(&self.backends, &info, data)? else {
            return Ok(None);
        };
        if decoded.format() != format {
            return Err(wrong_backend_layout());
        }
        let dims = decoded.dimensions();
        check_limits(
            self.limits,
            dims.width(),
            dims.height(),
            format.bytes_per_pixel(),
        )?;
        Ok(Some(decoded))
    }

    /// Decodes a possibly-incomplete JPEG XL stream into layout `P`, returning the best-effort
    /// image together with a [`JxlPartialReport`] saying how far the decode got.
    ///
    /// The mirror of [`DecodeImage::decode_image`] for input that may be cut short — a
    /// partly-downloaded file, a stream still in flight, a salvage attempt on a damaged one.
    /// [`DecodeImage::decode_image`] still rejects every truncation; this is the opt-in relaxation,
    /// and it relaxes **truncation only**: animation, premultiplied alpha, colour-as-grayscale, the
    /// pixel limit and the caller's [`DecodeLimits`] stay the same typed refusals. `P` is any of
    /// the eight layouts [`DecodeImage`] decodes.
    ///
    /// # What you actually get
    ///
    /// Best effort means exactly that, and JPEG XL's coding structure sets the ceiling:
    ///
    /// - Truncation **before the image headers** is still an
    ///   [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error — without
    ///   dimensions there is no buffer to hand back.
    /// - Truncation **before the frame header** yields a zero-filled buffer at the declared
    ///   dimensions ([`JxlRender::HeaderOnly`]).
    /// - Truncation **mid-frame** yields whatever groups arrived ([`JxlRender::BestEffort`]): for a
    ///   lossy (VarDCT) stream, groups with no detail pass are drawn from the upsampled DC image, so
    ///   the result is a full-size coarse preview sharpening towards the front of the stream; for a
    ///   lossless (Modular) stream, delivered groups are exact and the remainder stays zero.
    /// - An image small enough to be coded as a **single group** — roughly 256×256 or below — has
    ///   no partially-decodable structure at all, and comes back blank.
    /// - Not every truncation is even recoverable: some cut points are indistinguishable from
    ///   corruption to the decoder and still return an
    ///   [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error.
    ///
    /// So always consult [`JxlPartialReport::is_complete`]; never assume pixels are present.
    ///
    /// # Backends
    ///
    /// Unlike [`DecodeImage`], this is **always answered by the built-in jxl-rs tail** — a backend
    /// added with [`JxlDecoder::with_backend`] is not consulted, exactly as for
    /// [`JxlDecoder::info`] and [`JxlDecoder::embedded_icc_profile`]. The shared `gamut-codec-abi`
    /// seam has no notion of a partial result, so a backend could neither report one nor be asked
    /// for one; routing partial decode through the seam would mean extending that crate, and is
    /// additive if it ever happens.
    ///
    /// There is deliberately no `_into` counterpart: reusing a destination allocation pays off
    /// across a decode loop, which a one-shot salvage is not. Adding one later is additive.
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if `data`
    /// is malformed, or truncated before the image headers, or truncated at a point jxl-rs cannot
    /// tell from corruption; an [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported)
    /// error if the stream uses a feature that is not implemented, exceeds the limits, or cannot be
    /// presented as `P`.
    #[cfg(feature = "decode")]
    pub fn decode_partial<P: Pixel>(&self, data: &[u8]) -> Result<JxlPartialImage<P>>
    where
        Self: DecodeImage<P>,
    {
        let (image, report) = crate::jxlrs::decode_partial_to_buf::<P>(
            data,
            self.codestream_bit_depth,
            self.policy,
            self.limits,
        )?;
        Ok(JxlPartialImage { image, report })
    }
}

/// Implements [`DecodeImage`] for each supported layout: the added backends first, then the
/// built-in jxl-rs tail (or a typed refusal where it is not compiled in). The macro names only the
/// owned-sample variant a layout's storage width implies; every other layout fact comes from
/// [`Pixel::FORMAT`] via the crate's single layout table. These eight impls are also what bounds
/// [`JxlDecoder::decode_partial`]'s layouts.
macro_rules! impl_decode_image {
    ($($pixel:ty => $variant:ident;)*) => {$(
        impl DecodeImage<$pixel> for JxlDecoder {
            fn decode_image(&self, data: &[u8]) -> Result<ImageBuf<$pixel>> {
                if let Some(decoded) =
                    self.dispatch_backends(data, <$pixel as Pixel>::FORMAT)?
                {
                    let dimensions = decoded.dimensions();
                    return match decoded.into_samples() {
                        JxlOwnedSamples::$variant(samples) => {
                            ImageBuf::<$pixel>::new(samples, dimensions)
                        }
                        // Unreachable in practice: `JxlDecoded::new` already ties the sample
                        // variant to the format, and the format was checked above.
                        _ => Err(wrong_backend_layout()),
                    };
                }
                #[cfg(feature = "decode")]
                {
                    crate::jxlrs::decode_to_buf::<$pixel>(
                        data,
                        self.codestream_bit_depth,
                        self.policy,
                        self.limits,
                    )
                }
                #[cfg(not(feature = "decode"))]
                {
                    Err(no_decode_backend())
                }
            }

            fn decode_image_into(&self, data: &[u8], dst: &mut ImageBuf<$pixel>) -> Result<()> {
                if let Some(decoded) =
                    self.dispatch_backends(data, <$pixel as Pixel>::FORMAT)?
                {
                    let dimensions = decoded.dimensions();
                    return match decoded.into_samples() {
                        JxlOwnedSamples::$variant(samples) => {
                            *dst = ImageBuf::<$pixel>::new(samples, dimensions)?;
                            Ok(())
                        }
                        _ => Err(wrong_backend_layout()),
                    };
                }
                #[cfg(feature = "decode")]
                {
                    crate::jxlrs::decode_into_buf::<$pixel>(
                        data,
                        self.codestream_bit_depth,
                        self.policy,
                        self.limits,
                        dst,
                    )
                }
                #[cfg(not(feature = "decode"))]
                {
                    let _ = dst;
                    Err(no_decode_backend())
                }
            }
        }
    )*};
}

impl_decode_image! {
    Gray8       => U8;
    GrayAlpha8  => U8;
    Rgb8        => U8;
    Rgba8       => U8;
    Gray16      => U16;
    GrayAlpha16 => U16;
    Rgb16       => U16;
    Rgba16      => U16;
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use gamut_core::Dimensions;

    use super::*;

    #[test]
    fn new_and_default_are_equal_and_configuration_compares() {
        assert_eq!(JxlDecoder::new(), JxlDecoder::default());
        assert!(!JxlDecoder::new().codestream_bit_depth());
        assert!(
            JxlDecoder::new()
                .with_codestream_bit_depth(true)
                .codestream_bit_depth()
        );
        assert_ne!(
            JxlDecoder::new(),
            JxlDecoder::new().with_codestream_bit_depth(true)
        );
    }

    #[test]
    fn the_limits_default_to_the_workspace_budget_and_take_part_in_equality() {
        let default = JxlDecoder::new();
        assert_eq!(default.limits(), DecodeLimits::new());
        assert_eq!(default.limits().max_image_bytes(), 64 << 20);

        let limits = DecodeLimits::new().with_max_width(3).with_max_height(5);
        let capped = JxlDecoder::new().with_limits(limits);
        assert_eq!(capped.limits(), limits);
        assert_ne!(capped, default);
    }

    /// The error kind and message a limit refusal carries.
    fn refusal(result: Result<()>) -> (gamut_core::ErrorKind, Option<&'static str>) {
        let error = result.expect_err("over the limit");
        (error.kind(), error.static_message())
    }

    const DIMENSION: (gamut_core::ErrorKind, Option<&str>) = (
        gamut_core::ErrorKind::Unsupported,
        Some("JXL: image exceeds the dimension limit"),
    );
    const SIZE: (gamut_core::ErrorKind, Option<&str>) = (
        gamut_core::ErrorKind::Unsupported,
        Some("JXL: image exceeds the size limit"),
    );

    #[test]
    fn decode_limits_admit_their_exact_bounds_and_refuse_one_past() {
        let limits = DecodeLimits::new()
            .with_max_width(4)
            .with_max_height(3)
            .with_max_image_bytes(4 * 3 * 2);
        assert!(check_limits(limits, 4, 3, 2).is_ok());
        assert_eq!(refusal(check_limits(limits, 5, 3, 1)), DIMENSION);
        assert_eq!(refusal(check_limits(limits, 4, 4, 1)), DIMENSION);
        // Every factor of the byte count is weighed: one more of any exceeds the budget.
        assert_eq!(refusal(check_limits(limits, 4, 3, 3)), SIZE);
        let one_short = limits.with_max_image_bytes(4 * 3 * 2 - 1);
        assert_eq!(refusal(check_limits(one_short, 4, 3, 2)), SIZE);
    }

    #[test]
    fn decode_limits_refuse_a_byte_count_that_overflows() {
        // Unbounded caps still cannot admit a raster whose size `usize` cannot even express.
        let unbounded = DecodeLimits::new().with_max_image_bytes(usize::MAX);
        assert_eq!(
            refusal(check_limits(unbounded, u32::MAX, u32::MAX, usize::MAX)),
            SIZE
        );
    }

    #[test]
    fn a_backend_raster_is_held_to_the_callers_limits() {
        // `FixedBackend::returning` answers with a 2×2 `Gray8` raster: 4 bytes.
        let decode = |limits: DecodeLimits| {
            let decoder = JxlDecoder::new()
                .with_limits(limits)
                .with_backend(FixedBackend::returning(1));
            let result: Result<ImageBuf<Gray8>> = decoder.decode_image(&STREAM);
            result.map(|_| ())
        };
        let base = DecodeLimits::new();
        assert!(decode(base.with_max_width(2).with_max_height(2)).is_ok());
        assert!(decode(base.with_max_image_bytes(4)).is_ok());
        assert_eq!(
            refusal(decode(base.with_max_width(1).with_max_height(2))),
            DIMENSION
        );
        assert_eq!(refusal(decode(base.with_max_image_bytes(3))), SIZE);
    }

    #[test]
    fn the_refusal_errors_are_pinned() {
        let wrong = wrong_backend_layout();
        assert_eq!(wrong.kind(), gamut_core::ErrorKind::InvalidInput);
        assert_eq!(
            wrong.static_message(),
            Some("JXL: backend returned a raster in the wrong pixel layout")
        );
        let missing = no_decode_backend();
        assert_eq!(missing.kind(), gamut_core::ErrorKind::Unsupported);
        assert_eq!(
            missing.static_message(),
            Some(
                "JXL: no decode backend (enable the `decode` feature or push a codestream backend)"
            )
        );
    }

    /// A backend answering `supports` from a flag and `decode` with a canned outcome, counting both.
    struct FixedBackend {
        supported: bool,
        outcome: Result<JxlDecoded>,
        supports_calls: Arc<AtomicUsize>,
        decode_calls: Arc<AtomicUsize>,
        /// The info the last call saw.
        seen: Arc<std::sync::Mutex<Option<JxlStreamInfo>>>,
    }

    impl FixedBackend {
        fn with(supported: bool, outcome: Result<JxlDecoded>) -> Self {
            Self {
                supported,
                outcome,
                supports_calls: Arc::new(AtomicUsize::new(0)),
                decode_calls: Arc::new(AtomicUsize::new(0)),
                seen: Arc::new(std::sync::Mutex::new(None)),
            }
        }

        /// A backend that accepts everything and returns a `Gray8` raster of `fill`.
        fn returning(fill: u8) -> Self {
            Self::with(
                true,
                JxlDecoded::new(
                    PixelFormat::Gray8,
                    Dimensions::new(2, 2).unwrap(),
                    JxlOwnedSamples::U8(vec![fill; 4]),
                ),
            )
        }

        fn counters(&self) -> (Arc<AtomicUsize>, Arc<AtomicUsize>) {
            (
                Arc::clone(&self.supports_calls),
                Arc::clone(&self.decode_calls),
            )
        }

        fn seen(&self) -> Arc<std::sync::Mutex<Option<JxlStreamInfo>>> {
            Arc::clone(&self.seen)
        }
    }

    impl JxlCodestreamDecoder for FixedBackend {
        fn supports(&mut self, info: &JxlStreamInfo) -> bool {
            self.supports_calls.fetch_add(1, Ordering::SeqCst);
            *self.seen.lock().expect("test lock") = Some(*info);
            self.supported
        }

        fn decode(&mut self, _info: &JxlStreamInfo, _codestream: &[u8]) -> Result<JxlDecoded> {
            self.decode_calls.fetch_add(1, Ordering::SeqCst);
            match &self.outcome {
                Ok(decoded) => Ok(decoded.clone()),
                Err(error) if error.kind() == gamut_core::ErrorKind::Unsupported => {
                    Err(Error::unsupported(
                        env!("CARGO_PKG_NAME"),
                        error
                            .static_message()
                            .unwrap_or("JXL: test backend refusal"),
                    ))
                }
                Err(error) if error.kind() == gamut_core::ErrorKind::InvalidInput => {
                    Err(Error::invalid_input(
                        env!("CARGO_PKG_NAME"),
                        error
                            .static_message()
                            .unwrap_or("JXL: test backend failure"),
                    ))
                }
                Err(_) => Err(Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "JXL: test backend failure",
                )),
            }
        }
    }

    /// A minimal bare codestream signature, enough for framing detection.
    const STREAM: [u8; 2] = [0xFF, 0x0A];

    #[test]
    fn first_supporting_backend_wins_and_later_ones_are_untouched() {
        let first = FixedBackend::returning(7);
        let second = FixedBackend::returning(9);
        let (second_supports, second_decodes) = second.counters();

        let dec = JxlDecoder::new().with_backend(first).with_backend(second);

        let image: ImageBuf<Gray8> = dec.decode_image(&STREAM).expect("decode");
        assert_eq!(image.as_samples(), &[7, 7, 7, 7]);
        assert_eq!(image.dimensions(), Dimensions::new(2, 2).unwrap());
        assert_eq!(second_supports.load(Ordering::SeqCst), 0);
        assert_eq!(second_decodes.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_declining_backend_is_skipped_in_favour_of_the_next() {
        let first = FixedBackend::with(
            false,
            JxlDecoded::new(
                PixelFormat::Gray8,
                Dimensions::new(2, 2).unwrap(),
                JxlOwnedSamples::U8(vec![1; 4]),
            ),
        );
        let (first_supports, first_decodes) = first.counters();
        let second = FixedBackend::returning(3);
        let (second_supports, second_decodes) = second.counters();

        let dec = JxlDecoder::new().with_backend(first).with_backend(second);

        let image: ImageBuf<Gray8> = dec.decode_image(&STREAM).expect("decode");
        assert_eq!(image.as_samples(), &[3, 3, 3, 3]);
        assert_eq!(first_supports.load(Ordering::SeqCst), 1);
        assert_eq!(first_decodes.load(Ordering::SeqCst), 0);
        assert_eq!(second_supports.load(Ordering::SeqCst), 1);
        assert_eq!(second_decodes.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_late_unsupported_is_terminal() {
        // `supports` is the only fall-through: an accepting backend's `Unsupported` is the
        // caller's error, and the next backend is never asked.
        let first = FixedBackend::with(
            true,
            Err(Error::unsupported(
                env!("CARGO_PKG_NAME"),
                "changed its mind",
            )),
        );
        let (_, first_decodes) = first.counters();
        let second = FixedBackend::returning(5);
        let (second_supports, second_decodes) = second.counters();

        let dec = JxlDecoder::new().with_backend(first).with_backend(second);

        let result: Result<ImageBuf<Gray8>> = dec.decode_image(&STREAM);
        let error = result.unwrap_err();
        assert_eq!(error.kind(), gamut_core::ErrorKind::Unsupported);
        assert_eq!(error.static_message(), Some("changed its mind"));
        assert_eq!(first_decodes.load(Ordering::SeqCst), 1);
        assert_eq!(second_supports.load(Ordering::SeqCst), 0);
        assert_eq!(second_decodes.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn an_accepted_then_failed_backend_propagates_and_stops_the_chain() {
        let first = FixedBackend::with(
            true,
            Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "JXL: test backend failure",
            )),
        );
        let second = FixedBackend::returning(6);
        let (second_supports, second_decodes) = second.counters();

        let dec = JxlDecoder::new().with_backend(first).with_backend(second);

        let result: Result<ImageBuf<Gray8>> = dec.decode_image(&STREAM);
        let error = result.unwrap_err();
        assert_eq!(error.kind(), gamut_core::ErrorKind::InvalidInput);
        assert_eq!(error.static_message(), Some("JXL: test backend failure"));
        // Neither a later backend nor the built-in tail was reached.
        assert_eq!(second_supports.load(Ordering::SeqCst), 0);
        assert_eq!(second_decodes.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_backend_returning_the_wrong_layout_is_a_typed_error() {
        // The backend answers a Gray8 request with an Rgb8 raster; the host refuses to reshape it.
        let backend = FixedBackend::with(
            true,
            JxlDecoded::new(
                PixelFormat::Rgb8,
                Dimensions::new(2, 2).unwrap(),
                JxlOwnedSamples::U8(vec![1; 12]),
            ),
        );
        let dec = JxlDecoder::new().with_backend(backend);
        let result: Result<ImageBuf<Gray8>> = dec.decode_image(&STREAM);
        let error = result.unwrap_err();
        assert_eq!(error.kind(), gamut_core::ErrorKind::InvalidInput);
        assert_eq!(
            error.static_message(),
            Some("JXL: backend returned a raster in the wrong pixel layout")
        );
    }

    #[test]
    fn the_backend_sees_the_requested_layout_framing_and_policy() {
        let backend = FixedBackend::returning(1);
        let seen = backend.seen();
        let dec = JxlDecoder::new()
            .with_codestream_bit_depth(true)
            .with_backend(backend);

        let _: Result<ImageBuf<Gray8>> = dec.decode_image(&STREAM);
        let info = seen
            .lock()
            .expect("test lock")
            .expect("supports was called");
        assert_eq!(info.format(), PixelFormat::Gray8);
        assert_eq!(info.framing(), JxlFraming::Codestream);
        assert!(info.codestream_bit_depth());

        // A container-framed stream is reported as such; junk is Unknown.
        let container = [
            0x00, 0x00, 0x00, 0x0C, 0x4A, 0x58, 0x4C, 0x20, 0x0D, 0x0A, 0x87, 0x0A,
        ];
        let _: Result<ImageBuf<Gray8>> = dec.decode_image(&container);
        assert_eq!(
            seen.lock().expect("test lock").expect("info").framing(),
            JxlFraming::IsoBmff
        );
        let _: Result<ImageBuf<Gray8>> = dec.decode_image(&[0x01, 0x02]);
        assert_eq!(
            seen.lock().expect("test lock").expect("info").framing(),
            JxlFraming::Unknown
        );
    }

    #[test]
    fn decode_image_into_replaces_the_destination_from_a_backend() {
        let dec = JxlDecoder::new().with_backend(FixedBackend::returning(8));
        let mut dst: ImageBuf<Gray8> =
            ImageBuf::new(vec![0u8; 9], Dimensions::new(3, 3).unwrap()).unwrap();
        dec.decode_image_into(&STREAM, &mut dst).expect("decode");
        assert_eq!(dst.dimensions(), Dimensions::new(2, 2).unwrap());
        assert_eq!(dst.as_samples(), &[8, 8, 8, 8]);
    }

    #[test]
    fn sixteen_bit_backend_output_reaches_the_caller() {
        let dec = JxlDecoder::new().with_backend(FixedBackend::with(
            true,
            JxlDecoded::new(
                PixelFormat::Gray16,
                Dimensions::new(2, 2).unwrap(),
                JxlOwnedSamples::U16(vec![0xBEEF; 4]),
            ),
        ));
        let image: ImageBuf<Gray16> = dec.decode_image(&STREAM).expect("decode");
        assert_eq!(image.as_samples(), &[0xBEEF; 4]);
    }

    #[test]
    fn with_no_backend_the_builtin_tail_decides() {
        // The wasm-shaped story asserted on the dispatcher: an empty registry means the built-in
        // tail answers, and its absence is a typed refusal rather than a panic.
        let dec = JxlDecoder::new();
        let result: Result<ImageBuf<Gray8>> = dec.decode_image(&STREAM);
        // A two-byte signature is never a decodable image either way, so both builds error; only
        // the *kind* differs.
        let error = result.expect_err("a bare signature is not an image");
        if cfg!(feature = "decode") {
            assert_eq!(error.kind(), gamut_core::ErrorKind::InvalidInput);
        } else {
            assert_eq!(error.kind(), gamut_core::ErrorKind::Unsupported);
            assert_eq!(
                error.static_message(),
                Some(
                    "JXL: no decode backend (enable the `decode` feature or push a codestream backend)"
                )
            );
        }
    }

    #[test]
    fn all_backends_declining_falls_through_to_the_tail() {
        let first = FixedBackend::with(
            false,
            Err(Error::unsupported(env!("CARGO_PKG_NAME"), "never called")),
        );
        let (first_supports, _) = first.counters();
        let second = FixedBackend::with(
            false,
            Err(Error::unsupported(env!("CARGO_PKG_NAME"), "never called")),
        );
        let (second_supports, second_decodes) = second.counters();

        let dec = JxlDecoder::new().with_backend(first).with_backend(second);

        let result: Result<ImageBuf<Gray8>> = dec.decode_image(&STREAM);
        assert_eq!(first_supports.load(Ordering::SeqCst), 1);
        assert_eq!(second_supports.load(Ordering::SeqCst), 1);
        assert_eq!(second_decodes.load(Ordering::SeqCst), 0);
        // Reaching the tail with a two-byte signature errors, but as the tail's error.
        let error = result.expect_err("a bare signature is not an image");
        if cfg!(feature = "decode") {
            assert_eq!(error.kind(), gamut_core::ErrorKind::InvalidInput);
        } else {
            assert_eq!(error.kind(), gamut_core::ErrorKind::Unsupported);
        }
    }

    #[cfg(feature = "decode")]
    #[test]
    fn the_partial_path_never_consults_an_added_backend() {
        // A backend that would win the ordinary decode is not even asked about the partial one:
        // the codec-abi seam cannot express a partial result, so the built-in tail answers alone.
        let backend = FixedBackend::returning(4);
        let (supports, decodes) = backend.counters();
        let dec = JxlDecoder::new().with_backend(backend);

        // The ordinary path does go through it, so the counters below mean "vetoed", not "unused".
        let image: ImageBuf<Gray8> = dec.decode_image(&STREAM).expect("decode");
        assert_eq!(image.as_samples(), &[4, 4, 4, 4]);
        assert_eq!(supports.load(Ordering::SeqCst), 1);
        assert_eq!(decodes.load(Ordering::SeqCst), 1);

        // A bare signature is not a decodable image to the tail either, so this errors — but as
        // the tail's error, with the backend never consulted a second time.
        let result = dec.decode_partial::<Gray8>(&STREAM);
        assert!(result.is_err(), "a bare signature carries no image headers");
        assert_eq!(supports.load(Ordering::SeqCst), 1);
        assert_eq!(decodes.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_clone_keeps_its_own_backend_list() {
        let dec = JxlDecoder::new();
        let clone = dec.clone();
        let dec = dec.with_backend(FixedBackend::returning(2));
        // A backend added after the clone is not seen by it.
        assert!(clone.backends.is_empty());
        assert_eq!(dec.backends.len(), 1);
        // Equality still compares configuration only.
        assert_eq!(dec, clone);
        assert!(format!("{dec:?}").contains("backends: 1"));
    }
}

/// Unit tests for the container box walk behind [`JxlDecoder::metadata`]: the located payloads,
/// the three box-size forms, and the hostile-input refusals. `container_metadata_boxes` is
/// private, so these live beside it.
#[cfg(all(test, feature = "decode"))]
mod box_tests {
    use gamut_core::ErrorKind;

    use super::*;

    /// The 12-byte container signature box.
    const SIGNATURE: [u8; 12] = [
        0x00, 0x00, 0x00, 0x0C, 0x4A, 0x58, 0x4C, 0x20, 0x0D, 0x0A, 0x87, 0x0A,
    ];
    /// A TIFF-shaped EXIF stream.
    const TIFF: &[u8] = b"II\x2A\x00\x08\x00\x00\x00\x00\x00";

    /// One box in the 32-bit size form.
    fn bx(ty: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut out = (8 + body.len() as u32).to_be_bytes().to_vec();
        out.extend_from_slice(ty);
        out.extend_from_slice(body);
        out
    }

    /// One box in the `size == 1` / 64-bit `largesize` form.
    fn bx64(ty: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut out = 1u32.to_be_bytes().to_vec();
        out.extend_from_slice(ty);
        out.extend_from_slice(&(16 + body.len() as u64).to_be_bytes());
        out.extend_from_slice(body);
        out
    }

    /// One box in the `size == 0` (to end of file) form.
    fn bx0(ty: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut out = 0u32.to_be_bytes().to_vec();
        out.extend_from_slice(ty);
        out.extend_from_slice(body);
        out
    }

    /// The signature followed by `boxes`.
    fn container(boxes: &[Vec<u8>]) -> Vec<u8> {
        let mut out = SIGNATURE.to_vec();
        for b in boxes {
            out.extend_from_slice(b);
        }
        out
    }

    /// An `Exif` box payload: the big-endian offset, `gap` filler bytes, then the TIFF stream.
    fn exif_payload(offset: u32, gap: usize) -> Vec<u8> {
        let mut out = offset.to_be_bytes().to_vec();
        out.extend(std::iter::repeat_n(0xEE, gap));
        out.extend_from_slice(TIFF);
        out
    }

    #[test]
    fn exif_box_yields_the_tiff_stream_behind_the_offset() {
        // Offset 0 (what the encoder writes) and a non-zero offset skipping filler bytes.
        for (offset, gap) in [(0, 0), (6, 6)] {
            let data = container(&[
                bx(b"ftyp", b"jxl "),
                bx(b"Exif", &exif_payload(offset, gap)),
            ]);
            let (exif, xmp) = container_metadata_boxes(&data).unwrap();
            assert_eq!(exif.as_deref(), Some(TIFF), "offset {offset}");
            assert_eq!(xmp, None);
        }
    }

    #[test]
    fn xml_box_is_verbatim_and_the_first_of_a_kind_wins() {
        let data = container(&[
            bx(b"xml ", b"<x:xmpmeta>first</x:xmpmeta>"),
            bx(b"jxlc", &[0xFF, 0x0A]),
            bx(b"xml ", b"<x:xmpmeta>second</x:xmpmeta>"),
            bx(b"Exif", &exif_payload(0, 0)),
            bx(b"Exif", b"\0\0\0\0MM\0*"),
        ]);
        let (exif, xmp) = container_metadata_boxes(&data).unwrap();
        assert_eq!(xmp.as_deref(), Some(&b"<x:xmpmeta>first</x:xmpmeta>"[..]));
        assert_eq!(exif.as_deref(), Some(TIFF));
    }

    #[test]
    fn largesize_and_to_end_of_file_boxes_are_walked() {
        let data = container(&[bx64(b"Exif", &exif_payload(0, 0)), bx0(b"xml ", b"<x/>")]);
        let (exif, xmp) = container_metadata_boxes(&data).unwrap();
        assert_eq!(exif.as_deref(), Some(TIFF));
        assert_eq!(xmp.as_deref(), Some(&b"<x/>"[..]));
    }

    #[test]
    fn an_empty_box_is_walked_over_rather_than_rejected() {
        // A box whose `size` is exactly the 8-byte header carries no payload and is legal §4.2
        // framing: the walk must step over it and keep going, so the minimum-size rule is
        // `box_len < 8`, not `<= 8`. It is also the smallest step the walk can take, which is what
        // makes the loop's progress local to it.
        let data = container(&[
            bx(b"free", b""),
            bx(b"Exif", &exif_payload(0, 0)),
            bx(b"free", b""),
            bx(b"xml ", b"<x/>"),
        ]);
        let (exif, xmp) = container_metadata_boxes(&data).unwrap();
        assert_eq!(exif.as_deref(), Some(TIFF));
        assert_eq!(xmp.as_deref(), Some(&b"<x/>"[..]));

        // An empty box of a kind the walk *does* read is an empty payload, not an absent one.
        let data = container(&[bx(b"xml ", b"")]);
        let (_, xmp) = container_metadata_boxes(&data).unwrap();
        assert_eq!(xmp.as_deref(), Some(&b""[..]));
    }

    #[test]
    fn a_container_without_metadata_boxes_yields_nothing() {
        let data = container(&[bx(b"ftyp", b"jxl "), bx(b"jxlc", &[0xFF, 0x0A])]);
        assert_eq!(container_metadata_boxes(&data).unwrap(), (None, None));
    }

    #[test]
    fn brob_wrapping_a_metadata_box_is_unsupported() {
        for inner in [b"Exif", b"xml "] {
            let mut body = inner.to_vec();
            body.extend_from_slice(b"\x0b\x02\x80compressed");
            let data = container(&[bx(b"brob", &body)]);
            let err = container_metadata_boxes(&data).unwrap_err();
            assert_eq!(err.kind(), ErrorKind::Unsupported);
            assert_eq!(
                err.static_message(),
                Some("JXL: Brotli-compressed metadata box (brob) is not supported")
            );
        }
    }

    #[test]
    fn brob_wrapping_another_box_is_skipped() {
        let data = container(&[
            bx(b"brob", b"jumb\x0b\x02\x80compressed"),
            bx(b"xml ", b"<x/>"),
        ]);
        let (exif, xmp) = container_metadata_boxes(&data).unwrap();
        assert_eq!(exif, None);
        assert_eq!(xmp.as_deref(), Some(&b"<x/>"[..]));
    }

    #[test]
    fn malformed_box_sequences_are_invalid_input_with_the_named_fault() {
        let cases: [(Vec<u8>, &str); 7] = [
            // Seven bytes where a header should be.
            (
                container(&[vec![0, 0, 0, 9, b'x', b'm', b'l']]),
                "JXL: truncated box header",
            ),
            // `size == 1` but no `largesize`.
            (
                container(&[vec![0, 0, 0, 1, b'E', b'x', b'i', b'f', 0, 0]]),
                "JXL: truncated box header",
            ),
            // A 32-bit size below the header's own length.
            (
                container(&[vec![0, 0, 0, 4, b'x', b'm', b'l', b' ']]),
                "JXL: malformed box size",
            ),
            // A `largesize` below its header's own length.
            (
                container(&[vec![
                    0, 0, 0, 1, b'x', b'm', b'l', b' ', 0, 0, 0, 0, 0, 0, 0, 8,
                ]]),
                "JXL: malformed box size",
            ),
            // A box claiming more bytes than remain.
            (
                container(&[vec![
                    0, 0, 0, 20, b'x', b'm', b'l', b' ', b'<', b'x', b'/', b'>',
                ]]),
                "JXL: box overruns the stream",
            ),
            // A `largesize` no address space can hold.
            (
                container(&[vec![
                    0, 0, 0, 1, b'x', b'm', b'l', b' ', 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
                    0xFF,
                ]]),
                "JXL: box overruns the stream",
            ),
            // A `brob` box too short to name the box it wraps.
            (container(&[bx(b"brob", b"xm")]), "JXL: truncated brob box"),
        ];
        for (data, message) in cases {
            let err = container_metadata_boxes(&data).unwrap_err();
            assert_eq!(err.kind(), ErrorKind::InvalidInput, "{message}");
            assert_eq!(err.static_message(), Some(message));
        }
    }

    #[test]
    fn malformed_exif_payloads_are_invalid_input_with_the_named_fault() {
        let cases: [(&[u8], &str); 2] = [
            (b"\0\0\0", "JXL: truncated Exif box"),
            (
                b"\0\0\0\x0bII*\0",
                "JXL: Exif box tiff-header offset out of range",
            ),
        ];
        for (payload, message) in cases {
            let data = container(&[bx(b"Exif", payload)]);
            let err = container_metadata_boxes(&data).unwrap_err();
            assert_eq!(err.kind(), ErrorKind::InvalidInput, "{message}");
            assert_eq!(err.static_message(), Some(message));
        }
        // An offset landing exactly at the end is an empty (not out-of-range) stream.
        assert_eq!(exif_box_tiff_stream(b"\0\0\0\x02ab").unwrap(), b"");
    }

    #[test]
    fn metadata_boxes_survive_a_codestream_the_decoder_refuses() {
        // A `jxlc` box whose payload is not a codestream: the strict call fails on it, the
        // box-only reader still returns both located boxes.
        let data = container(&[
            bx(b"ftyp", b"jxl "),
            bx(b"Exif", &exif_payload(0, 0)),
            bx(b"xml ", b"<x/>"),
            bx(b"jxlc", b"\xFF\x0Agarbage"),
        ]);
        let dec = JxlDecoder::new();
        assert!(dec.metadata(&data).is_err());
        let boxes = dec.metadata_boxes(&data).unwrap();
        assert_eq!(boxes.exif.as_deref(), Some(TIFF));
        assert_eq!(boxes.xmp.as_deref(), Some(&b"<x/>"[..]));
        assert_eq!(boxes.icc, None);
    }

    #[test]
    fn metadata_boxes_of_a_bare_codestream_is_empty_without_parsing_it() {
        // Truncated straight after the signature: only the framing is read.
        let boxes = JxlDecoder::new().metadata_boxes(b"\xFF\x0A").unwrap();
        assert_eq!(boxes, RawMetadata::default());
    }

    #[test]
    fn metadata_of_a_stream_with_neither_signature_is_invalid_input() {
        let err = JxlDecoder::new().metadata(b"not a jxl").unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
        assert_eq!(
            err.static_message(),
            Some("JXL: neither the codestream nor the container signature")
        );
    }
}
