//! The pluggable **codestream backend** seam (issue #276): the typed traits, plain-data descriptors
//! and push-order registry by which an alternate JPEG XL codestream implementation is tried ahead of
//! the crate's built-in ones.
//!
//! # Where the seam cuts
//!
//! The boundary is the **bare JPEG XL codestream** — the byte string that starts with the two-byte
//! signature `FF 0A`. A backend receives a raster plus a plain-data description of the job and
//! returns codestream bytes ([`JxlCodestreamEncoder`]); or receives codestream bytes and returns a
//! raster ([`JxlCodestreamDecoder`]). Everything *outside* the codestream — ISO BMFF container
//! framing, `Exif`/`xml ` metadata boxes, and `jbrd` JPEG-reconstruction metadata — stays with the
//! host and is **never** delegated (see [the container veto](#container-feature-veto)).
//!
//! # The built-ins are the tail
//!
//! gamut-jxl's own wrappers are ordinary backends that sit implicitly **last**:
//!
//! - with the `encode` feature (and an encoder-capable target), libjxl is the last encode backend;
//! - with the `decode` feature, jxl-rs is the last decode backend.
//!
//! [`JxlEncoder::with_backend`](crate::JxlEncoder::with_backend) /
//! [`JxlDecoder::with_backend`](crate::JxlDecoder::with_backend) insert a backend *ahead* of that
//! tail. A pushed encode backend is therefore also what **supplies encode on `wasm32`**, where the
//! libjxl tail cannot be built at all. With no pushed backend and no built-in tail, the direction
//! reports an [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) error.
//!
//! # Fallback contract
//!
//! Backends are tried in **push order** through the workspace's one
//! [`Registry`](gamut_codec_abi::Registry), the built-in tail last.
//! [`supports`](JxlCodestreamEncoder::supports) returning `false` is the **only** fall-through: the
//! backend declines before doing any work. A backend that accepts a job and then fails — whatever
//! the error's kind — propagates that error to the caller unchanged: it may have produced a partial
//! result, which must never be silently masked by retrying a later backend.
//!
//! # Container-feature veto
//!
//! Container-dependent encoder features are pinned to the built-in path, and the veto is applied
//! **host-side** — a backend is not even asked. When
//! [`Container::IsoBmff`](crate::Container) output, [`with_exif`](crate::JxlEncoder::with_exif) /
//! [`with_xmp`](crate::JxlEncoder::with_xmp) embedding, or
//! [`recompress_jpeg`](crate::JxlEncoder::recompress_jpeg) is requested, the registry is skipped
//! entirely. The seam is the bare codestream, so a backend has no way to express container framing;
//! consulting it would either lose the request or invite a silently mis-framed stream.

use gamut_codec_abi::{Registry, RegistryError};
use gamut_core::{Dimensions, Error, Orientation, PixelFormat, Result};

use crate::config::{ColorSpec, Distance, Effort, FrameSettings, ModularMode};

/// Borrowed interleaved samples handed to an encode backend, tagged with their storage width.
///
/// Multi-byte samples are in **native** byte order — the in-memory representation of the caller's
/// `&[u16]`, unmodified.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JxlSamples<'a> {
    /// 8-bit samples, one byte each.
    U8(&'a [u8]),
    /// 16-bit samples, native byte order.
    U16(&'a [u16]),
}

impl JxlSamples<'_> {
    /// The number of samples in the buffer.
    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            Self::U8(s) => s.len(),
            Self::U16(s) => s.len(),
        }
    }

    /// Whether the buffer holds no samples.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Bits per sample of this buffer's storage width: `8` for [`JxlSamples::U8`], `16` for
    /// [`JxlSamples::U16`].
    #[must_use]
    pub fn bits_per_sample(&self) -> u32 {
        match self {
            Self::U8(_) => 8,
            Self::U16(_) => 16,
        }
    }
}

/// Owned interleaved samples returned by a decode backend, tagged with their storage width.
///
/// The owned counterpart of [`JxlSamples`]; multi-byte samples are in native byte order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JxlOwnedSamples {
    /// 8-bit samples, one byte each.
    U8(Vec<u8>),
    /// 16-bit samples, native byte order.
    U16(Vec<u16>),
}

impl JxlOwnedSamples {
    /// The number of samples in the buffer.
    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            Self::U8(s) => s.len(),
            Self::U16(s) => s.len(),
        }
    }

    /// Whether the buffer holds no samples.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Bits per sample of this buffer's storage width: `8` for [`JxlOwnedSamples::U8`], `16` for
    /// [`JxlOwnedSamples::U16`].
    #[must_use]
    pub fn bits_per_sample(&self) -> u32 {
        match self {
            Self::U8(_) => 8,
            Self::U16(_) => 16,
        }
    }
}

/// The channel layout of one of the eight [`PixelFormat`]s gamut-jxl codes: colour channels, alpha
/// presence, and storage width in bits. `None` for any other format.
///
/// This is the single place the crate turns a pixel-format tag into a JPEG XL frame description, so
/// the encoder, the decoder and the [`crate::abi`] adapters cannot disagree about a layout.
pub(crate) fn layout_of(format: PixelFormat) -> Option<(u32, bool, u32)> {
    Some(match format {
        PixelFormat::Gray8 => (1, false, 8),
        PixelFormat::GrayAlpha8 => (1, true, 8),
        PixelFormat::Rgb8 => (3, false, 8),
        PixelFormat::Rgba8 => (3, true, 8),
        PixelFormat::Gray16 => (1, false, 16),
        PixelFormat::GrayAlpha16 => (1, true, 16),
        PixelFormat::Rgb16 => (3, false, 16),
        PixelFormat::Rgba16 => (3, true, 16),
        _ => return None,
    })
}

/// The error for a pixel format outside gamut-jxl's eight coded layouts.
fn unsupported_layout() -> Error {
    Error::unsupported(
        env!("CARGO_PKG_NAME"),
        "JXL: pixel format is not a JPEG XL coded layout",
    )
}

/// The error for a sample buffer whose length or storage width contradicts its declared layout.
fn mismatched_samples() -> Error {
    Error::invalid_input(
        env!("CARGO_PKG_NAME"),
        "JXL: sample buffer does not match the declared layout",
    )
}

/// Total interleaved sample count for `dimensions` at `channels` samples per pixel, or `None` on
/// overflow.
fn sample_count(dimensions: Dimensions, channels: u32) -> Option<usize> {
    dimensions.sample_count(channels as usize)
}

/// A borrowed raster handed to an encode backend: the pixel layout, its dimensions, and the
/// interleaved samples.
///
/// Constructed by the host from a validated [`gamut_core::ImageRef`], so the fields are mutually
/// consistent; [`JxlImageRef::new`] enforces the same consistency for a caller building one directly
/// (in a backend's own tests, say).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JxlImageRef<'a> {
    /// The pixel layout of `samples`.
    format: PixelFormat,
    /// The image dimensions.
    dimensions: Dimensions,
    /// Colour samples per pixel: 1 (grayscale) or 3 (RGB); excludes alpha.
    color_channels: u32,
    /// Whether an interleaved alpha sample follows the colour samples.
    has_alpha: bool,
    /// Storage width of one sample, in bits: 8 or 16.
    bits_per_sample: u32,
    /// The interleaved samples, row-major.
    samples: JxlSamples<'a>,
}

impl<'a> JxlImageRef<'a> {
    /// Describes a raster for an encode backend, deriving the channel layout from `format`.
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) error if `format`
    /// is not one of the eight layouts gamut-jxl codes (8/16-bit gray, gray+alpha, RGB, RGBA), and
    /// an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if `samples`'
    /// storage width or length contradicts `format` and `dimensions`.
    pub fn new(
        format: PixelFormat,
        dimensions: Dimensions,
        samples: JxlSamples<'a>,
    ) -> Result<Self> {
        let (color_channels, has_alpha, bits_per_sample) =
            layout_of(format).ok_or_else(unsupported_layout)?;
        let channels = color_channels + u32::from(has_alpha);
        let expected = sample_count(dimensions, channels)
            .ok_or_else(|| Error::invalid_input(env!("CARGO_PKG_NAME"), "JXL: image too large"))?;
        if samples.bits_per_sample() != bits_per_sample || samples.len() != expected {
            return Err(mismatched_samples());
        }
        Ok(Self {
            format,
            dimensions,
            color_channels,
            has_alpha,
            bits_per_sample,
            samples,
        })
    }

    /// The pixel layout of the samples.
    #[must_use]
    pub fn format(&self) -> PixelFormat {
        self.format
    }

    /// The image dimensions.
    #[must_use]
    pub fn dimensions(&self) -> Dimensions {
        self.dimensions
    }

    /// Colour samples per pixel: 1 (grayscale) or 3 (RGB); excludes alpha.
    #[must_use]
    pub fn color_channels(&self) -> u32 {
        self.color_channels
    }

    /// Whether an interleaved alpha sample follows the colour samples of each pixel.
    #[must_use]
    pub fn has_alpha(&self) -> bool {
        self.has_alpha
    }

    /// Total interleaved samples per pixel, alpha included.
    #[must_use]
    pub fn channels(&self) -> u32 {
        self.color_channels + u32::from(self.has_alpha)
    }

    /// Storage width of one sample, in bits: 8 or 16.
    #[must_use]
    pub fn bits_per_sample(&self) -> u32 {
        self.bits_per_sample
    }

    /// The interleaved samples, row-major.
    #[must_use]
    pub fn samples(&self) -> JxlSamples<'a> {
        self.samples
    }
}

/// A raster produced by a decode backend: the pixel layout, its dimensions, and the owned
/// interleaved samples.
///
/// A backend must return **exactly** the layout the host asked for
/// ([`JxlStreamInfo::format`]) — the host does not reshape a backend's output, so any
/// grayscale→RGB expansion or alpha reconciliation is the backend's own business.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JxlDecoded {
    /// The pixel layout of `samples`.
    format: PixelFormat,
    /// The decoded dimensions.
    dimensions: Dimensions,
    /// The interleaved samples, row-major.
    samples: JxlOwnedSamples,
}

impl JxlDecoded {
    /// Describes a decoded raster, validating `samples` against `format` and `dimensions`.
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) error if `format`
    /// is not one of the eight layouts gamut-jxl codes, and an
    /// [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if `samples`' storage
    /// width or length contradicts `format` and `dimensions`.
    pub fn new(
        format: PixelFormat,
        dimensions: Dimensions,
        samples: JxlOwnedSamples,
    ) -> Result<Self> {
        let (color_channels, has_alpha, bits_per_sample) =
            layout_of(format).ok_or_else(unsupported_layout)?;
        let channels = color_channels + u32::from(has_alpha);
        let expected = sample_count(dimensions, channels)
            .ok_or_else(|| Error::invalid_input(env!("CARGO_PKG_NAME"), "JXL: image too large"))?;
        if samples.bits_per_sample() != bits_per_sample || samples.len() != expected {
            return Err(mismatched_samples());
        }
        Ok(Self {
            format,
            dimensions,
            samples,
        })
    }

    /// The pixel layout of the samples.
    #[must_use]
    pub fn format(&self) -> PixelFormat {
        self.format
    }

    /// The decoded dimensions.
    #[must_use]
    pub fn dimensions(&self) -> Dimensions {
        self.dimensions
    }

    /// The interleaved samples, row-major.
    #[must_use]
    pub fn samples(&self) -> &JxlOwnedSamples {
        &self.samples
    }

    /// Consumes the descriptor and returns the owned samples.
    #[must_use]
    pub fn into_samples(self) -> JxlOwnedSamples {
        self.samples
    }
}

/// The plain-data description of one codestream encode job, as a backend sees it.
///
/// It carries exactly the knobs that shape the **codestream**: the lossless/lossy mode and its
/// Butteraugli [`Distance`], the [`Effort`] dial, the [`ModularMode`] coding-tool selection, the
/// [`FrameSettings`] progression and decoding-speed overrides, the coded bit depth, the colour
/// encoding (a [`ColorSpec`], or an embedded ICC profile borrowed from the encoder) and the
/// [`Orientation`] metadata. Container-level requests are deliberately absent — they never reach a
/// backend (see the [container-feature veto](crate#container-feature-veto)).
///
/// Requests are produced by the host and handed to a backend by reference; there is no public
/// constructor.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct JxlEncodeRequest<'a> {
    /// The lossy Butteraugli distance, or `None` for mathematically lossless.
    distance: Option<Distance>,
    /// The speed/density effort level.
    effort: Effort,
    /// Which coding tool (VarDCT or Modular) the caller asked for, if either.
    modular: ModularMode,
    /// The progression and decoding-speed overrides the caller set, if any.
    frame_settings: FrameSettings,
    /// The declared coded bit depth, in bits per sample.
    coded_bit_depth: u32,
    /// The structured colour encoding, signalled when no ICC profile is set.
    color: ColorSpec,
    /// The embedded ICC profile, which is the colour encoding when present.
    icc_profile: Option<&'a [u8]>,
    /// The display orientation signalled for the coded samples.
    orientation: Orientation,
}

impl<'a> JxlEncodeRequest<'a> {
    /// Builds a request from the host's resolved configuration.
    #[expect(clippy::too_many_arguments, reason = "one argument per request field")]
    pub(crate) fn new(
        distance: Option<Distance>,
        effort: Effort,
        modular: ModularMode,
        frame_settings: FrameSettings,
        coded_bit_depth: u32,
        color: ColorSpec,
        icc_profile: Option<&'a [u8]>,
        orientation: Orientation,
    ) -> Self {
        Self {
            distance,
            effort,
            modular,
            frame_settings,
            coded_bit_depth,
            color,
            icc_profile,
            orientation,
        }
    }

    /// Whether the job is mathematically lossless (the decoded image must be bit-exact).
    #[must_use]
    pub fn is_lossless(&self) -> bool {
        self.distance.is_none()
    }

    /// The lossy Butteraugli [`Distance`], or `None` in lossless mode.
    #[must_use]
    pub fn distance(&self) -> Option<Distance> {
        self.distance
    }

    /// The requested [`Effort`] (speed/density trade-off).
    #[must_use]
    pub fn effort(&self) -> Effort {
        self.effort
    }

    /// The requested [`ModularMode`] (which coding tool to use, if the caller pinned one).
    ///
    /// [`ModularMode::Auto`] — the default — means the backend is free to choose, exactly as libjxl
    /// would. A backend that cannot honour a pinned mode should decline the job rather than encode
    /// with the other tool.
    #[must_use]
    pub fn modular(&self) -> ModularMode {
        self.modular
    }

    /// The requested [`FrameSettings`] (progression and decoding-speed overrides).
    ///
    /// [`FrameSettings::default`] means every setting is left to the backend, exactly as libjxl
    /// leaves an unsent setting to itself. A backend that cannot honour a set field should decline
    /// the job rather than encode without it.
    #[must_use]
    pub fn frame_settings(&self) -> FrameSettings {
        self.frame_settings
    }

    /// The coded bit depth to declare in the stream, in bits per sample.
    ///
    /// Equal to the raster's storage width unless
    /// [`JxlEncoder::with_bit_depth`](crate::JxlEncoder::with_bit_depth) narrowed it (an N-bit image
    /// carried in a 16-bit buffer).
    #[must_use]
    pub fn coded_bit_depth(&self) -> u32 {
        self.coded_bit_depth
    }

    /// The structured [`ColorSpec`] to signal for the samples when
    /// [`icc_profile`](Self::icc_profile) is `None`.
    #[must_use]
    pub fn color(&self) -> ColorSpec {
        self.color
    }

    /// The ICC profile to embed as the stream's colour encoding, or `None` to signal
    /// [`color`](Self::color) instead.
    #[must_use]
    pub fn icc_profile(&self) -> Option<&'a [u8]> {
        self.icc_profile
    }

    /// The display [`Orientation`] to signal for the coded samples.
    #[must_use]
    pub fn orientation(&self) -> Orientation {
        self.orientation
    }
}

/// How a JPEG XL byte stream is framed, as recognised from its leading bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum JxlFraming {
    /// A bare codestream, signature `FF 0A`.
    Codestream,
    /// The ISO BMFF `.jxl` container, signature `00 00 00 0C 4A 58 4C 20 0D 0A 87 0A`.
    IsoBmff,
    /// Neither signature matched (the stream is malformed, or too short to tell).
    Unknown,
}

/// The 12-byte ISO BMFF `.jxl` container signature: a `JXL ` box holding `0D 0A 87 0A`.
const ISOBMFF_SIGNATURE: [u8; 12] = [
    0x00, 0x00, 0x00, 0x0C, 0x4A, 0x58, 0x4C, 0x20, 0x0D, 0x0A, 0x87, 0x0A,
];

/// The 2-byte bare-codestream signature.
const CODESTREAM_SIGNATURE: [u8; 2] = [0xFF, 0x0A];

impl JxlFraming {
    /// Classifies a stream by its leading bytes.
    #[must_use]
    pub fn detect(data: &[u8]) -> Self {
        if data.starts_with(&ISOBMFF_SIGNATURE) {
            Self::IsoBmff
        } else if data.starts_with(&CODESTREAM_SIGNATURE) {
            Self::Codestream
        } else {
            Self::Unknown
        }
    }
}

/// The plain-data description of one codestream decode job, as a backend sees it.
///
/// It states what the host *wants* — the target pixel [`format`](Self::format) and the
/// codestream-bit-depth policy — plus what it could cheaply learn about the stream: its
/// [`framing`](Self::framing) and, when the built-in header parser is available, its
/// [`dimensions`](Self::dimensions).
///
/// Infos are produced by the host and handed to a backend by reference; there is no public
/// constructor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JxlStreamInfo {
    /// The pixel layout the decoded raster must be returned in.
    format: PixelFormat,
    /// The stream's framing, from its signature.
    framing: JxlFraming,
    /// The stream's dimensions when the host could determine them, else `None`.
    dimensions: Option<Dimensions>,
    /// Whether integer output must carry the codestream's declared bit depth.
    codestream_bit_depth: bool,
}

impl JxlStreamInfo {
    /// Builds an info from the host's resolved decode request.
    pub(crate) fn new(
        format: PixelFormat,
        framing: JxlFraming,
        dimensions: Option<Dimensions>,
        codestream_bit_depth: bool,
    ) -> Self {
        Self {
            format,
            framing,
            dimensions,
            codestream_bit_depth,
        }
    }

    /// The pixel layout the decoded raster must be returned in.
    ///
    /// A backend that cannot produce exactly this layout must decline: the host does not reshape a
    /// backend's output.
    #[must_use]
    pub fn format(&self) -> PixelFormat {
        self.format
    }

    /// The stream's framing, recognised from its signature.
    #[must_use]
    pub fn framing(&self) -> JxlFraming {
        self.framing
    }

    /// The stream's dimensions when the host could determine them without decoding pixels, else
    /// `None`.
    ///
    /// `None` means only that the host has no header parser compiled in (no `decode` feature) or
    /// that parsing failed — not that the stream lacks dimensions. A backend that needs them either
    /// parses the headers itself or declines.
    #[must_use]
    pub fn dimensions(&self) -> Option<Dimensions> {
        self.dimensions
    }

    /// Whether integer output must carry the codestream's declared bit depth rather than the output
    /// type's full range (see
    /// [`JxlDecoder::with_codestream_bit_depth`](crate::JxlDecoder::with_codestream_bit_depth)).
    #[must_use]
    pub fn codestream_bit_depth(&self) -> bool {
        self.codestream_bit_depth
    }
}

/// A pluggable JPEG XL **codestream encoder**.
///
/// Implement this to put a platform or alternate encoder ahead of gamut-jxl's built-in libjxl tail —
/// or, on `wasm32` where that tail cannot be built, to supply the encode direction at all. Register
/// it with [`JxlEncoder::with_backend`](crate::JxlEncoder::with_backend).
///
/// `Send` is not a supertrait: [`JxlEncoder::with_backend`](crate::JxlEncoder::with_backend) bounds
/// it where the backend is inserted, which is what keeps the encoder `Send + Sync`.
pub trait JxlCodestreamEncoder {
    /// Reports whether this backend can satisfy `req`. Returning `false` is a decline — the only
    /// one: the host moves on to the next backend, and finally to the built-in tail.
    ///
    /// Called before every encode, so the decision may depend on the distance, effort, coded bit
    /// depth, colour encoding and orientation the request carries.
    fn supports(&mut self, req: &JxlEncodeRequest<'_>) -> bool;

    /// Encodes `image` under `req`, returning a **bare JPEG XL codestream** (signature `FF 0A`).
    ///
    /// # Errors
    ///
    /// Any error propagates to the caller unchanged; neither a later backend nor the built-in tail
    /// is tried. Declining is [`supports`](Self::supports)' job alone.
    fn encode(&mut self, req: &JxlEncodeRequest<'_>, image: &JxlImageRef<'_>) -> Result<Vec<u8>>;
}

/// A pluggable JPEG XL **codestream decoder**.
///
/// Implement this to put a platform or alternate decoder ahead of gamut-jxl's built-in jxl-rs tail,
/// or to supply the decode direction where that tail is not compiled in. Register it with
/// [`JxlDecoder::with_backend`](crate::JxlDecoder::with_backend).
///
/// `Send` is not a supertrait, as on [`JxlCodestreamEncoder`].
pub trait JxlCodestreamDecoder {
    /// Reports whether this backend can decode the stream described by `info`. Returning `false` is
    /// a decline — the only one: the host moves on to the next backend, and finally to the built-in
    /// tail.
    fn supports(&mut self, info: &JxlStreamInfo) -> bool;

    /// Decodes `codestream` into the layout `info` requests.
    ///
    /// `codestream` is the caller's stream verbatim — bare codestream or ISO BMFF container, as
    /// [`JxlStreamInfo::framing`] reports.
    ///
    /// # Errors
    ///
    /// Any error propagates to the caller unchanged; neither a later backend nor the built-in tail
    /// is tried. Declining is [`supports`](Self::supports)' job alone.
    fn decode(&mut self, info: &JxlStreamInfo, codestream: &[u8]) -> Result<JxlDecoded>;
}

/// The encode registry: pushed backends, each `Send` (bound at insertion).
pub(crate) type EncoderRegistry = Registry<dyn JxlCodestreamEncoder + Send>;

/// The decode registry: pushed backends, each `Send` (bound at insertion).
pub(crate) type DecoderRegistry = Registry<dyn JxlCodestreamDecoder + Send>;

/// Maps a registry run onto the codec's `Result<Option<_>>`: `None` when every backend declined
/// (the caller then runs the built-in tail), the accepting backend's error unchanged, and an error
/// when a backend panicked while holding its lock.
fn registry_result<R>(run: core::result::Result<R, RegistryError<Error>>) -> Result<Option<R>> {
    match run {
        Ok(out) => Ok(Some(out)),
        Err(RegistryError::Unsupported) => Ok(None),
        Err(RegistryError::Backend(err)) => Err(err),
        Err(RegistryError::Poisoned) => Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "JXL: a backend poisoned the registry lock",
        )),
    }
}

/// Runs the encode registry in push order, returning the first accepted codestream, or `None` when
/// every backend declined (the caller then runs the built-in tail).
pub(crate) fn encode_with_backends(
    backends: &EncoderRegistry,
    req: &JxlEncodeRequest<'_>,
    image: &JxlImageRef<'_>,
) -> Result<Option<Vec<u8>>> {
    registry_result(backends.run(|b| b.supports(req), |b| b.encode(req, image)))
}

/// Runs the decode registry in push order, returning the first accepted raster, or `None` when
/// every backend declined (the caller then runs the built-in tail).
pub(crate) fn decode_with_backends(
    backends: &DecoderRegistry,
    info: &JxlStreamInfo,
    codestream: &[u8],
) -> Result<Option<JxlDecoded>> {
    registry_result(backends.run(|b| b.supports(info), |b| b.decode(info, codestream)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ProgressiveDc;

    #[test]
    fn layout_of_covers_the_eight_coded_formats_and_rejects_the_rest() {
        assert_eq!(layout_of(PixelFormat::Gray8), Some((1, false, 8)));
        assert_eq!(layout_of(PixelFormat::GrayAlpha8), Some((1, true, 8)));
        assert_eq!(layout_of(PixelFormat::Rgb8), Some((3, false, 8)));
        assert_eq!(layout_of(PixelFormat::Rgba8), Some((3, true, 8)));
        assert_eq!(layout_of(PixelFormat::Gray16), Some((1, false, 16)));
        assert_eq!(layout_of(PixelFormat::GrayAlpha16), Some((1, true, 16)));
        assert_eq!(layout_of(PixelFormat::Rgb16), Some((3, false, 16)));
        assert_eq!(layout_of(PixelFormat::Rgba16), Some((3, true, 16)));
        // Everything else is outside the JPEG XL coded set gamut-jxl exposes.
        assert_eq!(layout_of(PixelFormat::Bilevel), None);
        assert_eq!(layout_of(PixelFormat::Indexed8), None);
        assert_eq!(layout_of(PixelFormat::Cmyk8), None);
    }

    #[test]
    fn samples_report_their_width_and_length() {
        assert_eq!(JxlSamples::U8(&[1, 2, 3]).bits_per_sample(), 8);
        assert_eq!(JxlSamples::U8(&[1, 2, 3]).len(), 3);
        assert!(!JxlSamples::U8(&[1]).is_empty());
        assert!(JxlSamples::U8(&[]).is_empty());
        assert_eq!(JxlSamples::U16(&[1, 2]).bits_per_sample(), 16);
        assert_eq!(JxlSamples::U16(&[1, 2]).len(), 2);
        assert!(JxlSamples::U16(&[]).is_empty());

        assert_eq!(JxlOwnedSamples::U8(vec![1, 2, 3]).bits_per_sample(), 8);
        assert_eq!(JxlOwnedSamples::U8(vec![1, 2, 3]).len(), 3);
        assert!(!JxlOwnedSamples::U8(vec![1]).is_empty());
        assert!(JxlOwnedSamples::U8(Vec::new()).is_empty());
        assert_eq!(JxlOwnedSamples::U16(vec![1, 2]).bits_per_sample(), 16);
        assert_eq!(JxlOwnedSamples::U16(vec![1, 2]).len(), 2);
        assert!(JxlOwnedSamples::U16(Vec::new()).is_empty());
    }

    #[test]
    fn image_ref_derives_the_layout_from_the_format() {
        let dims = Dimensions::new(2, 3).unwrap();
        let img =
            JxlImageRef::new(PixelFormat::Rgba8, dims, JxlSamples::U8(&[0u8; 24])).expect("valid");
        assert_eq!(img.format(), PixelFormat::Rgba8);
        assert_eq!(img.dimensions(), dims);
        assert_eq!(img.color_channels(), 3);
        assert!(img.has_alpha());
        assert_eq!(img.channels(), 4);
        assert_eq!(img.bits_per_sample(), 8);
        assert_eq!(img.samples(), JxlSamples::U8(&[0u8; 24]));

        let gray = JxlImageRef::new(PixelFormat::Gray16, dims, JxlSamples::U16(&[0u16; 6]))
            .expect("valid");
        assert_eq!(gray.color_channels(), 1);
        assert!(!gray.has_alpha());
        assert_eq!(gray.channels(), 1);
        assert_eq!(gray.bits_per_sample(), 16);
    }

    #[test]
    fn image_ref_rejects_bad_layout_width_and_length() {
        let dims = Dimensions::new(2, 3).unwrap();
        // A format outside the coded eight.
        let error =
            JxlImageRef::new(PixelFormat::Cmyk8, dims, JxlSamples::U8(&[0u8; 24])).unwrap_err();
        assert_eq!(error.kind(), gamut_core::ErrorKind::Unsupported);
        assert_eq!(
            error.static_message(),
            Some("JXL: pixel format is not a JPEG XL coded layout")
        );
        // Right length, wrong storage width.
        let error =
            JxlImageRef::new(PixelFormat::Rgba8, dims, JxlSamples::U16(&[0u16; 24])).unwrap_err();
        assert_eq!(error.kind(), gamut_core::ErrorKind::InvalidInput);
        assert_eq!(
            error.static_message(),
            Some("JXL: sample buffer does not match the declared layout")
        );
        // Right storage width, wrong length (one sample short, and one too many).
        assert!(JxlImageRef::new(PixelFormat::Rgba8, dims, JxlSamples::U8(&[0u8; 23])).is_err());
        assert!(JxlImageRef::new(PixelFormat::Rgba8, dims, JxlSamples::U8(&[0u8; 25])).is_err());
        // The exact length is accepted.
        assert!(JxlImageRef::new(PixelFormat::Rgba8, dims, JxlSamples::U8(&[0u8; 24])).is_ok());
    }

    #[test]
    fn decoded_validates_and_exposes_its_raster() {
        let dims = Dimensions::new(2, 2).unwrap();
        let dec = JxlDecoded::new(
            PixelFormat::Gray8,
            dims,
            JxlOwnedSamples::U8(vec![1, 2, 3, 4]),
        )
        .expect("valid");
        assert_eq!(dec.format(), PixelFormat::Gray8);
        assert_eq!(dec.dimensions(), dims);
        assert_eq!(dec.samples(), &JxlOwnedSamples::U8(vec![1, 2, 3, 4]));
        assert_eq!(
            dec.clone().into_samples(),
            JxlOwnedSamples::U8(vec![1, 2, 3, 4])
        );

        assert!(matches!(
            JxlDecoded::new(PixelFormat::Bilevel, dims, JxlOwnedSamples::U8(vec![0; 4])),
            Err(error) if error.kind() == gamut_core::ErrorKind::Unsupported
        ));
        assert!(matches!(
            JxlDecoded::new(PixelFormat::Gray8, dims, JxlOwnedSamples::U8(vec![0; 5])),
            Err(error) if error.kind() == gamut_core::ErrorKind::InvalidInput
        ));
        assert!(matches!(
            JxlDecoded::new(PixelFormat::Gray8, dims, JxlOwnedSamples::U16(vec![0; 4])),
            Err(error) if error.kind() == gamut_core::ErrorKind::InvalidInput
        ));
    }

    #[test]
    fn framing_detects_both_signatures_and_nothing_else() {
        assert_eq!(
            JxlFraming::detect(&[0xFF, 0x0A, 0x00]),
            JxlFraming::Codestream
        );
        let mut iso = ISOBMFF_SIGNATURE.to_vec();
        iso.push(0);
        assert_eq!(JxlFraming::detect(&iso), JxlFraming::IsoBmff);
        // A truncated container signature is not a container.
        assert_eq!(
            JxlFraming::detect(&ISOBMFF_SIGNATURE[..11]),
            JxlFraming::Unknown
        );
        assert_eq!(JxlFraming::detect(&[0xFF]), JxlFraming::Unknown);
        assert_eq!(JxlFraming::detect(&[]), JxlFraming::Unknown);
        assert_eq!(JxlFraming::detect(&[0x0A, 0xFF]), JxlFraming::Unknown);
    }

    #[test]
    fn encode_request_reports_its_configuration() {
        let d = Distance::new(2.0).unwrap();
        let lossy = JxlEncodeRequest::new(
            Some(d),
            Effort::Kitten,
            ModularMode::Modular,
            FrameSettings::default().with_progressive_dc(ProgressiveDc::OneLevel),
            10,
            ColorSpec::Pq,
            Some(&[1, 2, 3]),
            Orientation::Rotate90Cw,
        );
        assert!(!lossy.is_lossless());
        assert_eq!(lossy.distance(), Some(d));
        assert_eq!(lossy.effort(), Effort::Kitten);
        assert_eq!(lossy.modular(), ModularMode::Modular);
        assert_eq!(
            lossy.frame_settings().progressive_dc(),
            ProgressiveDc::OneLevel
        );
        assert_eq!(lossy.coded_bit_depth(), 10);
        assert_eq!(lossy.color(), ColorSpec::Pq);
        assert_eq!(lossy.icc_profile(), Some(&[1u8, 2, 3][..]));
        assert_eq!(lossy.orientation(), Orientation::Rotate90Cw);

        let lossless = JxlEncodeRequest::new(
            None,
            Effort::Squirrel,
            ModularMode::Auto,
            FrameSettings::default(),
            8,
            ColorSpec::Srgb,
            None,
            Orientation::Identity,
        );
        assert!(lossless.is_lossless());
        assert_eq!(lossless.distance(), None);
        assert_eq!(lossless.modular(), ModularMode::Auto);
        assert_eq!(lossless.icc_profile(), None);
    }

    #[test]
    fn stream_info_reports_its_request() {
        let dims = Dimensions::new(4, 5).unwrap();
        let info = JxlStreamInfo::new(PixelFormat::Rgb16, JxlFraming::IsoBmff, Some(dims), true);
        assert_eq!(info.format(), PixelFormat::Rgb16);
        assert_eq!(info.framing(), JxlFraming::IsoBmff);
        assert_eq!(info.dimensions(), Some(dims));
        assert!(info.codestream_bit_depth());

        let unknown = JxlStreamInfo::new(PixelFormat::Gray8, JxlFraming::Unknown, None, false);
        assert_eq!(unknown.dimensions(), None);
        assert!(!unknown.codestream_bit_depth());
    }

    #[test]
    fn registry_result_maps_each_outcome() {
        assert_eq!(registry_result::<u8>(Ok(7)).unwrap(), Some(7));
        // Every backend declined: the caller runs the built-in tail.
        assert_eq!(
            registry_result::<u8>(Err(RegistryError::Unsupported)).unwrap(),
            None
        );
        // An accepting backend's error is the caller's, unchanged — even an `Unsupported` one.
        let backend = Error::unsupported("backend", "late refusal");
        assert_eq!(
            registry_result::<u8>(Err(RegistryError::Backend(backend)))
                .unwrap_err()
                .static_message(),
            Some("late refusal")
        );
        let poisoned = registry_result::<u8>(Err(RegistryError::Poisoned)).unwrap_err();
        assert_eq!(poisoned.kind(), gamut_core::ErrorKind::InvalidInput);
        assert_eq!(
            poisoned.static_message(),
            Some("JXL: a backend poisoned the registry lock")
        );
    }
}
