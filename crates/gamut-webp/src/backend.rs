//! The codestream backend seam: pluggable VP8 / VP8L encode and decode backends, and the fallback
//! contract that governs how [`WebpEncoder`](crate::WebpEncoder) / [`WebpDecoder`](crate::WebpDecoder)
//! select among them (issue #275, under the shared [`gamut_codec_abi`] contract of issue #241).
//!
//! # What the seam carries
//!
//! WebP is a **container** (RIFF) wrapping exactly one coded still picture, in one of two
//! codestreams: a lossy `VP8 ` intra key frame (RFC 6386) or a lossless `VP8L` stream (RFC 9649 §3).
//! The seam datum is therefore the **raw RIFF chunk payload** of that one chunk — nothing above it
//! (the RIFF header, `VP8X` feature bits) and nothing beside it.
//!
//! There is **one trait pair**, not one pair per codestream: [`WebpCodestreamDecoder`] and
//! [`WebpCodestreamEncoder`] both carry a [`WebpCodestream`] discriminant, so a backend that handles
//! only one of the two simply declines the other in `supports`. The decoded raster, by contrast,
//! *is* split: VP8 reconstructs natively to YCbCr 4:2:0 and VP8L to packed ARGB, and there is no
//! lossless common form, so [`DecodedWebp`] is an enum.
//!
//! # `ALPH` is not part of the seam
//!
//! An extended (`VP8X`) lossy file stores alpha in its own `ALPH` chunk, which is a *container*
//! feature, not a codestream: it is produced and consumed by [`crate::alpha`] on both sides,
//! before/after the seam. A VP8 backend never sees it and never needs to.
//!
//! # Hardware reality
//!
//! The two codestreams differ in what a backend can plausibly be. VP8 has real hardware decoders
//! (stateless V4L2 and friends), so a `VP8 ` backend is often a device. VP8L has none — a `VP8L`
//! backend is always an alternate *software* implementation (libwebp, typically). The registry does
//! not care: both flow through the same push-order + [`supports`](WebpCodestreamDecoder::supports)
//! selection with no special-casing.
//!
//! # Fallback contract
//!
//! Backends are tried in **push order**, and the crate's own `vp8`/`vp8l` implementations are the
//! implicit **tails** — they are never pushed and can never be removed:
//!
//! 1. Each backend is offered the job; `supports() == false` is the *only* signal that falls through
//!    to the next one.
//! 2. If every backend declines, the built-in tail runs.
//! 3. Once a backend accepts, it owns the job: an error it returns **propagates** to the caller and
//!    the tail is *not* consulted, so a partial or wrong result is never silently masked.
//!
//! The list itself is [`gamut_codec_abi::Registry`], which implements those rules once for every
//! format crate. Cloning a [`WebpDecoder`](crate::WebpDecoder) or
//! [`WebpEncoder`](crate::WebpEncoder) copies the list: the clone shares the backend instances
//! already added, but a backend added to one afterwards is not seen by the other.

use gamut_codec_abi::{self as abi, Registry, RegistryError, codec_id, pixel_format};
use gamut_color::{ChromaSubsampling, Planar8};
use gamut_core::{Dimensions, Error, Result};
use gamut_riff::FourCc;

use crate::config::Effort;

/// The `codec_id` of a lossy `VP8 ` codestream in a [`gamut_codec_abi::StreamConfig`] /
/// [`gamut_codec_abi::EncodeConfig`]: the chunk FourCC, built big-endian by
/// [`gamut_codec_abi::codec_id`].
pub const VP8_CODEC_ID: u32 = codec_id(*b"VP8 ");

/// The `codec_id` of a lossless `VP8L` codestream; see [`VP8_CODEC_ID`].
pub const VP8L_CODEC_ID: u32 = codec_id(*b"VP8L");

/// Which of WebP's two codestreams a job concerns — the discriminant that lets one trait pair serve
/// both.
///
/// Each value names the RIFF chunk carrying the codestream, so it round-trips through
/// [`fourcc`](Self::fourcc) and through the `codec_id` field of the [`gamut_codec_abi`]
/// descriptors ([`codec_id`](Self::codec_id)). The set is closed — RFC 9649 defines exactly these
/// two — and the discriminants are permanent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum WebpCodestream {
    /// The lossy intra key-frame codestream of a `VP8 ` chunk (RFC 6386).
    Vp8 = 0,
    /// The lossless codestream of a `VP8L` chunk (RFC 9649 §3).
    Vp8l = 1,
}

impl WebpCodestream {
    /// The FourCC of the RIFF chunk that carries this codestream (`VP8 ` or `VP8L`).
    #[must_use]
    pub const fn fourcc(self) -> FourCc {
        match self {
            WebpCodestream::Vp8 => FourCc::VP8,
            WebpCodestream::Vp8l => FourCc::VP8L,
        }
    }

    /// The `codec_id` this codestream uses in a [`gamut_codec_abi::StreamConfig`] /
    /// [`gamut_codec_abi::EncodeConfig`]: [`VP8_CODEC_ID`] or [`VP8L_CODEC_ID`], so the two
    /// codestreams are distinct ids and a backend that handles one can decline the other in
    /// `supports`.
    #[must_use]
    pub const fn codec_id(self) -> u32 {
        match self {
            WebpCodestream::Vp8 => VP8_CODEC_ID,
            WebpCodestream::Vp8l => VP8L_CODEC_ID,
        }
    }
}

/// What a decode backend is being asked to decode: which codestream, and at what size.
///
/// The dimensions are read from the codestream's own header before any backend is consulted (the
/// VP8 key-frame header, RFC 6386 §9.1; the VP8L header, RFC 9649 §3.4), so a backend can size its
/// output buffers from `info` alone. If those bytes cannot be parsed, no backend is offered the job
/// at all and the built-in tail runs, producing the format's own parse error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WebpStreamInfo {
    codestream: WebpCodestream,
    dimensions: Dimensions,
}

impl WebpStreamInfo {
    /// Describes a `codestream` of `dimensions`.
    #[must_use]
    pub const fn new(codestream: WebpCodestream, dimensions: Dimensions) -> Self {
        Self {
            codestream,
            dimensions,
        }
    }

    /// Which codestream the payload holds.
    #[must_use]
    pub const fn codestream(&self) -> WebpCodestream {
        self.codestream
    }

    /// The coded image dimensions, from the codestream header.
    #[must_use]
    pub const fn dimensions(&self) -> Dimensions {
        self.dimensions
    }
}

/// A decoded picture in the raster the codestream reconstructs natively.
///
/// The two are irreconcilable without a lossy conversion — VP8 reconstructs YCbCr 4:2:0 (limited
/// range, BT.601) and VP8L reconstructs exact ARGB — so a backend returns whichever its codestream
/// defines. Returning the variant that does not match the requested [`WebpCodestream`] is a
/// contract violation, which the host rejects with an
/// [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error rather than
/// converting between the two.
#[derive(Debug, Clone)]
pub enum DecodedWebp {
    /// A `VP8 ` reconstruction: limited-range BT.601 YCbCr planes at visible resolution, laid out
    /// [`ChromaSubsampling::Cs420`] (any other layout is refused by
    /// [`dimensions`](Self::dimensions)).
    Yuv420(Planar8),
    /// A `VP8L` reconstruction: `0xAARRGGBB` pixels in scan order.
    Argb {
        /// The image dimensions; `pixels` holds `width * height` entries.
        dimensions: Dimensions,
        /// The packed `0xAARRGGBB` pixels, row-major.
        pixels: Vec<u32>,
    },
}

impl DecodedWebp {
    /// The codestream this raster is the native output of.
    #[must_use]
    pub const fn codestream(&self) -> WebpCodestream {
        match self {
            DecodedWebp::Yuv420(_) => WebpCodestream::Vp8,
            DecodedWebp::Argb { .. } => WebpCodestream::Vp8l,
        }
    }

    /// The raster's dimensions.
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if a
    /// [`Yuv420`](DecodedWebp::Yuv420) raster's planes are not 4:2:0, the only layout VP8
    /// codes.
    pub fn dimensions(&self) -> Result<Dimensions> {
        match self {
            DecodedWebp::Yuv420(yuv) => yuv420_dimensions(yuv),
            DecodedWebp::Argb { dimensions, .. } => Ok(*dimensions),
        }
    }
}

/// The dimensions of a VP8 raster's planes, refusing any layout but 4:2:0.
///
/// The one place a [`Planar8`] is admitted as a VP8 raster: the codec-abi encode adapter hands
/// these planes to a foreign backend with 4:2:0 strides, so a raster of any other layout must be
/// stopped here rather than read past the end of its chroma planes.
fn yuv420_dimensions(yuv: &Planar8) -> Result<Dimensions> {
    if yuv.subsampling() != ChromaSubsampling::Cs420 {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "WebP: a VP8 raster must be 4:2:0 planes",
        ));
    }
    Dimensions::new(yuv.width(), yuv.height())
}

/// What an encode backend is being asked to produce: which codestream, at what size and quality.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WebpEncodeRequest {
    codestream: WebpCodestream,
    dimensions: Dimensions,
    quality: u8,
    effort: Effort,
}

impl WebpEncodeRequest {
    /// Describes an encode job for `codestream` at `dimensions` and `quality` (`0..=100`), at the
    /// default [`Effort`]. Chain [`with_effort`](Self::with_effort) to select another.
    #[must_use]
    pub const fn new(codestream: WebpCodestream, dimensions: Dimensions, quality: u8) -> Self {
        Self {
            codestream,
            dimensions,
            quality,
            // Named literally rather than via `Effort::default()`, which is not const-callable.
            effort: Effort::Default,
        }
    }

    /// Sets the compression [`Effort`] for this job, returning the updated request so calls chain.
    #[must_use]
    pub const fn with_effort(mut self, effort: Effort) -> Self {
        self.effort = effort;
        self
    }

    /// The requested compression effort — a **hint**. Every level produces a conformant stream, so
    /// a backend that ignores it is still correct; one that honours it must not change what the
    /// codestream decodes to beyond what its mode already allows.
    #[must_use]
    pub const fn effort(&self) -> Effort {
        self.effort
    }

    /// The codestream to produce.
    #[must_use]
    pub const fn codestream(&self) -> WebpCodestream {
        self.codestream
    }

    /// The image dimensions.
    #[must_use]
    pub const fn dimensions(&self) -> Dimensions {
        self.dimensions
    }

    /// The requested quality on the `0..=100` scale. Ignored by the lossless codestream, which
    /// always reproduces its input exactly.
    #[must_use]
    pub const fn quality(&self) -> u8 {
        self.quality
    }

    /// Whether the requested codestream is the lossless one (`VP8L`).
    #[must_use]
    pub const fn is_lossless(&self) -> bool {
        matches!(self.codestream, WebpCodestream::Vp8l)
    }
}

/// The source picture handed to an encode backend, borrowed in the raster its codestream consumes
/// natively — the encode-side mirror of [`DecodedWebp`].
#[derive(Debug, Clone, Copy)]
pub enum WebpRasterRef<'a> {
    /// Limited-range BT.601 YCbCr planes laid out [`ChromaSubsampling::Cs420`], the `VP8 `
    /// encoder's input (any other layout is refused by [`dimensions`](Self::dimensions)).
    Yuv420(&'a Planar8),
    /// `0xAARRGGBB` pixels in scan order, the `VP8L` encoder's input.
    Argb {
        /// The image dimensions; `pixels` holds `width * height` entries.
        dimensions: Dimensions,
        /// The packed `0xAARRGGBB` pixels, row-major.
        pixels: &'a [u32],
    },
}

impl WebpRasterRef<'_> {
    /// The codestream this raster is the native input of.
    #[must_use]
    pub const fn codestream(&self) -> WebpCodestream {
        match self {
            WebpRasterRef::Yuv420(_) => WebpCodestream::Vp8,
            WebpRasterRef::Argb { .. } => WebpCodestream::Vp8l,
        }
    }

    /// The raster's dimensions.
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if a
    /// [`Yuv420`](WebpRasterRef::Yuv420) raster's planes are not 4:2:0, the only layout VP8 codes.
    pub fn dimensions(&self) -> Result<Dimensions> {
        match self {
            WebpRasterRef::Yuv420(yuv) => yuv420_dimensions(yuv),
            WebpRasterRef::Argb { dimensions, .. } => Ok(*dimensions),
        }
    }
}

/// A pluggable decoder for one raw WebP codestream payload.
///
/// Implement this to route `VP8 ` and/or `VP8L` chunks to a hardware or alternate software decoder,
/// then add it with [`WebpDecoder::with_backend`](crate::WebpDecoder::with_backend). The
/// selection rules are the crate-level fallback contract: decline via
/// [`supports`](Self::supports); once you accept, your error propagates.
///
/// `Send` is not a supertrait: the decoder bounds it where a backend is added, so the trait itself
/// stays implementable by a single-threaded backend.
pub trait WebpCodestreamDecoder {
    /// Reports whether this backend will decode the described codestream. Returning `false` is the
    /// only way to fall through to the next backend (and ultimately to the built-in tail).
    fn supports(&mut self, info: &WebpStreamInfo) -> bool;

    /// Decodes `payload` — the raw RIFF chunk payload, header byte included — into the raster
    /// native to [`info.codestream()`](WebpStreamInfo::codestream).
    ///
    /// # Errors
    ///
    /// Any error propagates to the caller of the enclosing decode; the built-in decoder is not
    /// retried.
    fn decode(&mut self, info: &WebpStreamInfo, payload: &[u8]) -> Result<DecodedWebp>;
}

/// A pluggable encoder for one WebP codestream.
///
/// Implement this to route encoding to a hardware or alternate software encoder, then install it
/// with [`WebpEncoder::with_backend`](crate::WebpEncoder::with_backend). Same fallback contract as
/// [`WebpCodestreamDecoder`], and likewise no `Send` supertrait.
pub trait WebpCodestreamEncoder {
    /// Reports whether this backend will satisfy the described encode job. Returning `false` is the
    /// only way to fall through to the next backend (and ultimately to the built-in tail).
    fn supports(&mut self, req: &WebpEncodeRequest) -> bool;

    /// Encodes `raster`, returning the **raw RIFF chunk payload** for
    /// [`req.codestream()`](WebpEncodeRequest::codestream) — the bytes that go inside the `VP8 ` or
    /// `VP8L` chunk, with no RIFF framing. The container layer (and any `ALPH` chunk) is added by
    /// the caller.
    ///
    /// # Errors
    ///
    /// Any error propagates to the caller of the enclosing encode; the built-in encoder is not
    /// retried.
    fn encode(&mut self, req: &WebpEncodeRequest, raster: &WebpRasterRef<'_>) -> Result<Vec<u8>>;
}

/// The decoder's backend list; `Send` is bound here, where a backend is inserted.
pub(crate) type DecoderRegistry = Registry<dyn WebpCodestreamDecoder + Send>;

/// The encoder's backend list; see [`DecoderRegistry`].
pub(crate) type EncoderRegistry = Registry<dyn WebpCodestreamEncoder + Send>;

/// Maps a registry run onto the host's result: `Ok(None)` when every backend declined (the caller
/// then runs the built-in tail), the accepting backend's error unchanged, and a lock a panicking
/// backend poisoned as an error of its own.
fn registry_result<R>(run: core::result::Result<R, RegistryError<Error>>) -> Result<Option<R>> {
    match run {
        Ok(out) => Ok(Some(out)),
        Err(RegistryError::Unsupported) => Ok(None),
        Err(RegistryError::Backend(err)) => Err(err),
        Err(RegistryError::Poisoned) => Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "WebP: a codestream backend panicked (registry lock poisoned)",
        )),
    }
}

/// Offers the decode job to each backend in push order: the first acceptance's outcome, or
/// `Ok(None)` when every backend declines.
pub(crate) fn decode_with_backends(
    backends: &DecoderRegistry,
    info: &WebpStreamInfo,
    payload: &[u8],
) -> Result<Option<DecodedWebp>> {
    registry_result(backends.run(|b| b.supports(info), |b| b.decode(info, payload)))
}

/// Offers the encode job to each backend in push order: the first acceptance's outcome, or
/// `Ok(None)` when every backend declines.
pub(crate) fn encode_with_backends(
    backends: &EncoderRegistry,
    req: &WebpEncodeRequest,
    raster: &WebpRasterRef<'_>,
) -> Result<Option<Vec<u8>>> {
    registry_result(backends.run(|b| b.supports(req), |b| b.encode(req, raster)))
}

/// Reads the coded dimensions out of a codestream payload without decoding it: the VP8 key-frame
/// header (RFC 6386 §9.1) or the VP8L header (RFC 9649 §3.4). `None` when the header is malformed or
/// truncated — the host then skips the registry entirely and lets the built-in decoder report the
/// error.
pub(crate) fn peek_dimensions(codestream: WebpCodestream, payload: &[u8]) -> Option<Dimensions> {
    let (width, height) = match codestream {
        WebpCodestream::Vp8 => {
            let chunk = crate::vp8::header::read_uncompressed_chunk(payload).ok()?;
            (u32::from(chunk.width), u32::from(chunk.height))
        }
        WebpCodestream::Vp8l => {
            let mut reader = crate::vp8l::bit_io::BitReader::new(payload);
            let header = crate::vp8l::header::Vp8lHeader::read(&mut reader).ok()?;
            (u32::from(header.width), u32::from(header.height))
        }
    };
    Dimensions::new(width, height).ok()
}

// ================================================================================================
// gamut-codec-abi adapters
// ================================================================================================

/// Maps a non-OK [`abi::Status`] onto a typed error.
///
/// A late [`Status::UNSUPPORTED`](abi::Status::UNSUPPORTED) — returned from `decode`/`encode` after
/// `supports` already accepted the job — cannot re-open the fallback (the contract forbids retrying
/// a later backend once one has accepted), so it surfaces as an
/// [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) error; every other non-OK status
/// is a backend failure and surfaces as an
/// [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error.
fn status_error(status: abi::Status, unsupported: &'static str, failed: &'static str) -> Error {
    let classified = if status.is_unsupported() {
        Error::unsupported(env!("CARGO_PKG_NAME"), unsupported)
    } else {
        Error::invalid_input(env!("CARGO_PKG_NAME"), failed)
    };
    classified.with_detail(format!("codec-abi status {}", status.0))
}

/// Adapts a [`gamut_codec_abi::Decoder`] (the shared ABI's Rust twin, and hence — via
/// [`gamut_codec_abi::bridge::ForeignDecoder`] — any C backend) into a [`WebpCodestreamDecoder`].
///
/// The adapter is codestream-agnostic: it forwards the job's
/// [`codec_id`](WebpCodestream::codec_id) in the [`abi::StreamConfig`] and lets the wrapped backend
/// decide, so one adapter serves `VP8 `-only, `VP8L`-only, and both-codestream backends alike.
///
/// The `out` [`abi::ImageDesc`] it hands the backend is:
/// - `VP8 ` — [`pixel_format::I420`], three 8-bit planes (Y, U, V), tightly packed at visible
///   resolution (strides `width`, `chroma_width`, `chroma_width`);
/// - `VP8L` — [`pixel_format::BGRA`], one plane, stride `width * 4`: each `0xAARRGGBB` pixel
///   little-endian, so the bytes are B, G, R, A.
#[derive(Debug, Clone)]
pub struct AbiWebpDecoder<D> {
    inner: D,
}

impl<D> AbiWebpDecoder<D> {
    /// Wraps an ABI decoder backend.
    #[must_use]
    pub const fn new(inner: D) -> Self {
        Self { inner }
    }

    /// Borrows the wrapped backend.
    #[must_use]
    pub const fn get_ref(&self) -> &D {
        &self.inner
    }

    /// Mutably borrows the wrapped backend.
    #[must_use]
    pub fn get_mut(&mut self) -> &mut D {
        &mut self.inner
    }

    /// Returns the wrapped backend.
    #[must_use]
    pub fn into_inner(self) -> D {
        self.inner
    }
}

impl<D: abi::Decoder> WebpCodestreamDecoder for AbiWebpDecoder<D> {
    fn supports(&mut self, info: &WebpStreamInfo) -> bool {
        self.inner.supports(&stream_config(info))
    }

    fn decode(&mut self, info: &WebpStreamInfo, payload: &[u8]) -> Result<DecodedWebp> {
        let cfg = stream_config(info);
        let dims = info.dimensions();
        let (w, h) = (dims.width() as usize, dims.height() as usize);
        match info.codestream() {
            WebpCodestream::Vp8 => {
                let (cw, ch) =
                    ChromaSubsampling::Cs420.chroma_dimensions(dims.width(), dims.height());
                let (cw, ch) = (cw as usize, ch as usize);
                let mut y = vec![0u8; w * h];
                let mut u = vec![0u8; cw * ch];
                let mut v = vec![0u8; cw * ch];
                let desc = abi::ImageDesc::new(
                    pixel_format::I420,
                    dims.width(),
                    dims.height(),
                    8,
                    3,
                    [
                        y.as_mut_ptr(),
                        u.as_mut_ptr(),
                        v.as_mut_ptr(),
                        core::ptr::null_mut(),
                    ],
                    [w, cw, cw, 0],
                );
                let status = self.inner.decode(&cfg, payload, &desc);
                if !status.is_ok() {
                    return Err(status_error(
                        status,
                        "WebP: codec-abi decode backend declined after accepting the job",
                        "WebP: codec-abi decode backend failed",
                    ));
                }
                Ok(DecodedWebp::Yuv420(Planar8::from_planes_subsampled(
                    dims,
                    ChromaSubsampling::Cs420,
                    [y, u, v],
                )?))
            }
            WebpCodestream::Vp8l => {
                let mut bytes = vec![0u8; w * h * 4];
                let desc = abi::ImageDesc::new(
                    pixel_format::BGRA,
                    dims.width(),
                    dims.height(),
                    8,
                    1,
                    [
                        bytes.as_mut_ptr(),
                        core::ptr::null_mut(),
                        core::ptr::null_mut(),
                        core::ptr::null_mut(),
                    ],
                    [w * 4, 0, 0, 0],
                );
                let status = self.inner.decode(&cfg, payload, &desc);
                if !status.is_ok() {
                    return Err(status_error(
                        status,
                        "WebP: codec-abi decode backend declined after accepting the job",
                        "WebP: codec-abi decode backend failed",
                    ));
                }
                let pixels = bytes
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|p| u32::from_le_bytes([p[0], p[1], p[2], p[3]]))
                    .collect();
                Ok(DecodedWebp::Argb {
                    dimensions: dims,
                    pixels,
                })
            }
        }
    }
}

/// Adapts a [`gamut_codec_abi::Encoder`] into a [`WebpCodestreamEncoder`], mirroring
/// [`AbiWebpDecoder`]: the job's [`codec_id`](WebpCodestream::codec_id) and quality go into the
/// [`abi::EncodeConfig`], the source picture into an [`abi::ImageDesc`] with the same plane layout
/// the decode adapter uses, and the streamed output chunks are concatenated into the returned chunk
/// payload.
#[derive(Debug, Clone)]
pub struct AbiWebpEncoder<E> {
    inner: E,
}

impl<E> AbiWebpEncoder<E> {
    /// Wraps an ABI encoder backend.
    #[must_use]
    pub const fn new(inner: E) -> Self {
        Self { inner }
    }

    /// Borrows the wrapped backend.
    #[must_use]
    pub const fn get_ref(&self) -> &E {
        &self.inner
    }

    /// Mutably borrows the wrapped backend.
    #[must_use]
    pub fn get_mut(&mut self) -> &mut E {
        &mut self.inner
    }

    /// Returns the wrapped backend.
    #[must_use]
    pub fn into_inner(self) -> E {
        self.inner
    }
}

impl<E: abi::Encoder> WebpCodestreamEncoder for AbiWebpEncoder<E> {
    fn supports(&mut self, req: &WebpEncodeRequest) -> bool {
        self.inner.supports(&encode_config(req))
    }

    fn encode(&mut self, req: &WebpEncodeRequest, raster: &WebpRasterRef<'_>) -> Result<Vec<u8>> {
        let cfg = encode_config(req);
        let dims = raster.dimensions()?;
        // Owned byte staging for the ARGB case; the YUV case borrows the planes in place.
        let mut argb_bytes = Vec::new();
        let desc = match raster {
            WebpRasterRef::Yuv420(yuv) => {
                // `raster.dimensions()` above refused every layout but 4:2:0, so these strides
                // describe the planes exactly.
                let (cw, _) =
                    ChromaSubsampling::Cs420.chroma_dimensions(dims.width(), dims.height());
                let cw = cw as usize;
                let [y, u, v] = yuv.planes();
                abi::ImageDesc::new(
                    pixel_format::I420,
                    dims.width(),
                    dims.height(),
                    8,
                    3,
                    [
                        y.as_ptr().cast_mut(),
                        u.as_ptr().cast_mut(),
                        v.as_ptr().cast_mut(),
                        core::ptr::null_mut(),
                    ],
                    [dims.width() as usize, cw, cw, 0],
                )
            }
            WebpRasterRef::Argb { pixels, .. } => {
                argb_bytes.extend(pixels.iter().flat_map(|p| p.to_le_bytes()));
                abi::ImageDesc::new(
                    pixel_format::BGRA,
                    dims.width(),
                    dims.height(),
                    8,
                    1,
                    [
                        argb_bytes.as_mut_ptr(),
                        core::ptr::null_mut(),
                        core::ptr::null_mut(),
                        core::ptr::null_mut(),
                    ],
                    [dims.width() as usize * 4, 0, 0, 0],
                )
            }
        };
        let mut out = Vec::new();
        let mut sink = |chunk: &[u8]| {
            out.extend_from_slice(chunk);
            abi::Status::OK
        };
        let status = self.inner.encode(&cfg, &desc, &mut sink);
        if !status.is_ok() {
            return Err(status_error(
                status,
                "WebP: codec-abi encode backend declined after accepting the job",
                "WebP: codec-abi encode backend failed",
            ));
        }
        Ok(out)
    }
}

/// Builds the ABI stream descriptor for a decode job (no extradata: a WebP codestream is
/// self-contained, with no out-of-band parameter sets).
fn stream_config(info: &WebpStreamInfo) -> abi::StreamConfig {
    let dims = info.dimensions();
    abi::StreamConfig::new(info.codestream().codec_id(), dims.width(), dims.height(), 8)
}

/// Builds the ABI encode descriptor for an encode job (no options blob).
fn encode_config(req: &WebpEncodeRequest) -> abi::EncodeConfig {
    abi::EncodeConfig::new(req.codestream().codec_id(), u32::from(req.quality()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dims(width: u32, height: u32) -> Dimensions {
        Dimensions::new(width, height).unwrap()
    }

    #[test]
    fn codestream_ids_are_the_big_endian_chunk_fourccs() {
        assert_eq!(WebpCodestream::Vp8.fourcc(), FourCc::VP8);
        assert_eq!(WebpCodestream::Vp8l.fourcc(), FourCc::VP8L);
        assert_eq!(VP8_CODEC_ID, 0x5650_3820);
        assert_eq!(VP8L_CODEC_ID, 0x5650_384C);
        assert_eq!(WebpCodestream::Vp8.codec_id(), VP8_CODEC_ID);
        assert_eq!(WebpCodestream::Vp8l.codec_id(), VP8L_CODEC_ID);
        assert_eq!(WebpCodestream::Vp8 as u8, 0);
        assert_eq!(WebpCodestream::Vp8l as u8, 1);
    }

    #[test]
    fn registry_result_maps_each_outcome() {
        assert_eq!(registry_result::<u8>(Ok(7)).unwrap(), Some(7));
        assert_eq!(
            registry_result::<u8>(Err(RegistryError::Unsupported)).unwrap(),
            None
        );
        let refusal = Error::unsupported("test", "a backend's own refusal");
        let err = registry_result::<u8>(Err(RegistryError::Backend(refusal))).unwrap_err();
        assert_eq!(err.kind(), gamut_core::ErrorKind::Unsupported);
        assert_eq!(err.static_message(), Some("a backend's own refusal"));
        let err = registry_result::<u8>(Err(RegistryError::Poisoned)).unwrap_err();
        assert_eq!(err.kind(), gamut_core::ErrorKind::InvalidInput);
        assert_eq!(
            err.static_message(),
            Some("WebP: a codestream backend panicked (registry lock poisoned)")
        );
    }

    #[test]
    fn info_and_request_expose_their_fields() {
        let info = WebpStreamInfo::new(WebpCodestream::Vp8l, dims(7, 5));
        assert_eq!(info.codestream(), WebpCodestream::Vp8l);
        assert_eq!(info.dimensions(), dims(7, 5));

        let req = WebpEncodeRequest::new(WebpCodestream::Vp8, dims(9, 3), 42);
        assert_eq!(req.codestream(), WebpCodestream::Vp8);
        assert_eq!(req.dimensions(), dims(9, 3));
        assert_eq!(req.quality(), 42);
        assert!(!req.is_lossless());
        assert!(WebpEncodeRequest::new(WebpCodestream::Vp8l, dims(1, 1), 0).is_lossless());
    }

    #[test]
    fn rasters_report_their_codestream_and_dimensions() {
        let yuv = Planar8::from_planes_subsampled(
            dims(2, 2),
            ChromaSubsampling::Cs420,
            [vec![0; 4], vec![0; 1], vec![0; 1]],
        )
        .unwrap();
        let raster = DecodedWebp::Yuv420(yuv.clone());
        assert_eq!(raster.codestream(), WebpCodestream::Vp8);
        assert_eq!(raster.dimensions().unwrap(), dims(2, 2));

        let argb = DecodedWebp::Argb {
            dimensions: dims(3, 1),
            pixels: vec![1, 2, 3],
        };
        assert_eq!(argb.codestream(), WebpCodestream::Vp8l);
        assert_eq!(argb.dimensions().unwrap(), dims(3, 1));

        let yref = WebpRasterRef::Yuv420(&yuv);
        assert_eq!(yref.codestream(), WebpCodestream::Vp8);
        assert_eq!(yref.dimensions().unwrap(), dims(2, 2));
        let pixels = [1u32, 2, 3];
        let aref = WebpRasterRef::Argb {
            dimensions: dims(3, 1),
            pixels: &pixels,
        };
        assert_eq!(aref.codestream(), WebpCodestream::Vp8l);
        assert_eq!(aref.dimensions().unwrap(), dims(3, 1));
    }

    #[test]
    fn yuv420_rasters_of_any_other_layout_have_no_dimensions() {
        // A `Planar8` cannot be zero-sided, but it can be the wrong layout; the codec-abi adapter
        // would hand such planes to a foreign backend with 4:2:0 strides, so both raster types
        // refuse them.
        for (subsampling, chroma) in [
            (ChromaSubsampling::Cs444, 4),
            (ChromaSubsampling::Cs422, 2),
            (ChromaSubsampling::Cs440, 2),
            (ChromaSubsampling::Cs400, 0),
        ] {
            let yuv = Planar8::from_planes_subsampled(
                dims(2, 2),
                subsampling,
                [vec![0; 4], vec![0; chroma], vec![0; chroma]],
            )
            .unwrap();
            for err in [
                DecodedWebp::Yuv420(yuv.clone()).dimensions().unwrap_err(),
                WebpRasterRef::Yuv420(&yuv).dimensions().unwrap_err(),
            ] {
                assert_eq!(
                    err.kind(),
                    gamut_core::ErrorKind::InvalidInput,
                    "{subsampling:?}"
                );
            }
        }
    }

    #[test]
    fn encode_requests_default_to_effort_4_and_carry_an_override() {
        // `new` stays a 3-arg const fn, so effort arrives through the chainable setter; a request
        // that never mentions effort must report libwebp's default method rather than 0.
        let base = WebpEncodeRequest::new(WebpCodestream::Vp8, dims(8, 8), 50);
        assert_eq!(base.effort(), Effort::Default);
        let slow = base.with_effort(Effort::Slowest);
        assert_eq!(slow.effort(), Effort::Slowest);
        // The override must leave the rest of the job description alone.
        assert_eq!(slow.codestream(), base.codestream());
        assert_eq!(slow.dimensions(), base.dimensions());
        assert_eq!(slow.quality(), base.quality());
    }

    #[test]
    fn status_error_separates_late_decline_from_failure() {
        let declined = status_error(abi::Status::UNSUPPORTED, "declined", "failed");
        assert_eq!(declined.kind(), gamut_core::ErrorKind::Unsupported);
        assert_eq!(declined.static_message(), Some("declined"));
        assert_eq!(declined.detail(), Some("codec-abi status -1"));

        let failed = status_error(abi::Status(7), "declined", "failed");
        assert_eq!(failed.kind(), gamut_core::ErrorKind::InvalidInput);
        assert_eq!(failed.static_message(), Some("failed"));
        assert_eq!(failed.detail(), Some("codec-abi status 7"));
    }

    #[test]
    fn configs_carry_the_codec_id_quality_and_depth() {
        let cfg = stream_config(&WebpStreamInfo::new(WebpCodestream::Vp8, dims(64, 48)));
        assert_eq!(cfg.codec_id, WebpCodestream::Vp8.codec_id());
        assert_eq!(cfg.width, 64);
        assert_eq!(cfg.height, 48);
        assert_eq!(cfg.bit_depth, 8);
        assert_eq!(cfg.extradata_len, 0);
        assert!(cfg.is_abi_current());

        let enc = encode_config(&WebpEncodeRequest::new(
            WebpCodestream::Vp8l,
            dims(4, 4),
            88,
        ));
        assert_eq!(enc.codec_id, WebpCodestream::Vp8l.codec_id());
        assert_eq!(enc.quality, 88);
        assert_eq!(enc.extra_len, 0);
        assert!(enc.is_abi_current());
    }

    #[test]
    fn peek_reads_dimensions_from_both_codestream_headers() {
        // A minimal VP8 key-frame header: tag, start code, 16x16.
        let vp8 = [0x00u8, 0, 0, 0x9d, 0x01, 0x2a, 16, 0, 16, 0];
        assert_eq!(
            peek_dimensions(WebpCodestream::Vp8, &vp8),
            Some(dims(16, 16))
        );
        // Truncated / inter-frame payloads yield None, so the registry is skipped.
        assert_eq!(peek_dimensions(WebpCodestream::Vp8, &vp8[..4]), None);
        assert_eq!(peek_dimensions(WebpCodestream::Vp8l, &[]), None);

        // VP8L: signature byte then 14-bit (width - 1) and (height - 1), LSB-first.
        let mut w = crate::vp8l::bit_io::BitWriter::new();
        crate::vp8l::header::Vp8lHeader::from_dimensions(dims(5, 9), false)
            .unwrap()
            .write(&mut w);
        assert_eq!(
            peek_dimensions(WebpCodestream::Vp8l, &w.finish()),
            Some(dims(5, 9))
        );
    }
}
