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
//! lossless common form, so [`DecodedRaster`] is an enum.
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

use std::sync::{Arc, Mutex};

use gamut_codec_abi as abi;
use gamut_color::Yuv420;
use gamut_core::{Dimensions, Error, Result};
use gamut_riff::FourCc;

use crate::config::Effort;

/// Which of WebP's two codestreams a job concerns — the discriminant that lets one trait pair serve
/// both.
///
/// The value is exactly the FourCC of the RIFF chunk carrying the codestream, so it round-trips
/// through [`fourcc`](Self::fourcc) and through the `codec_id` field of the
/// [`gamut_codec_abi`] descriptors ([`codec_id`](Self::codec_id)).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WebpCodestream {
    /// The lossy intra key-frame codestream of a `VP8 ` chunk (RFC 6386).
    Vp8,
    /// The lossless codestream of a `VP8L` chunk (RFC 9649 §3).
    Vp8l,
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
    /// [`gamut_codec_abi::EncodeConfig`]: the chunk FourCC read as a little-endian `u32`, so the two
    /// codestreams are distinct ids that a C backend can compare against `'VP8 '` / `'VP8L'`.
    #[must_use]
    pub const fn codec_id(self) -> u32 {
        u32::from_le_bytes(*self.fourcc().as_bytes())
    }
}

/// The `pixel_format` tag used for a planar YCbCr 4:2:0 [`gamut_codec_abi::ImageDesc`]: three planes
/// (Y, then U, then V), 8 bits per sample.
///
/// This is deliberately **not** a [`gamut_core::PixelFormat`] discriminant — that enum has no planar
/// YUV or packed-ARGB member — but it shares the same `u32` field, so the value is chosen well
/// outside `PixelFormat`'s (small, contiguous) range: it is the FourCC `I420` read little-endian.
pub const PIXEL_FORMAT_YUV420: u32 = u32::from_le_bytes(*b"I420");

/// The `pixel_format` tag used for a packed-ARGB [`gamut_codec_abi::ImageDesc`]: one plane, four
/// bytes per pixel in **memory order B, G, R, A** — the little-endian encoding of the `0xAARRGGBB`
/// word this crate's VP8L code uses. The FourCC `BGRA` read little-endian; see
/// [`PIXEL_FORMAT_YUV420`] for why these are not `gamut_core::PixelFormat` values.
pub const PIXEL_FORMAT_ARGB: u32 = u32::from_le_bytes(*b"BGRA");

/// What a decode backend is being asked to decode: which codestream, and at what size.
///
/// The dimensions are read from the codestream's own header before any backend is consulted (the
/// VP8 key-frame header, RFC 6386 §9.1; the VP8L header, RFC 9649 §3.4), so a backend can size its
/// output buffers from `info` alone. If those bytes cannot be parsed, no backend is offered the job
/// at all and the built-in tail runs, producing the format's own parse error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodestreamInfo {
    codestream: WebpCodestream,
    dimensions: Dimensions,
}

impl CodestreamInfo {
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
/// contract violation, which the host rejects with an [`Error::InvalidInput`] rather than
/// converting between the two.
#[derive(Debug, Clone)]
pub enum DecodedRaster {
    /// A `VP8 ` reconstruction: limited-range BT.601 YCbCr 4:2:0 at visible resolution.
    Yuv420(Yuv420),
    /// A `VP8L` reconstruction: `0xAARRGGBB` pixels in scan order.
    Argb {
        /// The image dimensions; `pixels` holds `width * height` entries.
        dimensions: Dimensions,
        /// The packed `0xAARRGGBB` pixels, row-major.
        pixels: Vec<u32>,
    },
}

impl DecodedRaster {
    /// The codestream this raster is the native output of.
    #[must_use]
    pub const fn codestream(&self) -> WebpCodestream {
        match self {
            DecodedRaster::Yuv420(_) => WebpCodestream::Vp8,
            DecodedRaster::Argb { .. } => WebpCodestream::Vp8l,
        }
    }

    /// The raster's dimensions.
    #[must_use]
    pub fn dimensions(&self) -> Dimensions {
        match self {
            DecodedRaster::Yuv420(yuv) => Dimensions {
                width: yuv.width(),
                height: yuv.height(),
            },
            DecodedRaster::Argb { dimensions, .. } => *dimensions,
        }
    }
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
/// natively — the encode-side mirror of [`DecodedRaster`].
#[derive(Debug, Clone, Copy)]
pub enum RasterRef<'a> {
    /// Limited-range BT.601 YCbCr 4:2:0, the `VP8 ` encoder's input.
    Yuv420(&'a Yuv420),
    /// `0xAARRGGBB` pixels in scan order, the `VP8L` encoder's input.
    Argb {
        /// The image dimensions; `pixels` holds `width * height` entries.
        dimensions: Dimensions,
        /// The packed `0xAARRGGBB` pixels, row-major.
        pixels: &'a [u32],
    },
}

impl RasterRef<'_> {
    /// The codestream this raster is the native input of.
    #[must_use]
    pub const fn codestream(&self) -> WebpCodestream {
        match self {
            RasterRef::Yuv420(_) => WebpCodestream::Vp8,
            RasterRef::Argb { .. } => WebpCodestream::Vp8l,
        }
    }

    /// The raster's dimensions.
    #[must_use]
    pub fn dimensions(&self) -> Dimensions {
        match self {
            RasterRef::Yuv420(yuv) => Dimensions {
                width: yuv.width(),
                height: yuv.height(),
            },
            RasterRef::Argb { dimensions, .. } => *dimensions,
        }
    }
}

/// A pluggable decoder for one raw WebP codestream payload.
///
/// Implement this to route `VP8 ` and/or `VP8L` chunks to a hardware or alternate software decoder,
/// then install it with [`WebpDecoder::push_backend`](crate::WebpDecoder::push_backend). The
/// selection rules are the crate-level fallback contract: decline via
/// [`supports`](Self::supports); once you accept, your error propagates.
///
/// `Send` is required because the registry is shared behind an [`Arc`] when a
/// [`WebpDecoder`](crate::WebpDecoder) is cloned.
pub trait WebpCodestreamDecoder: Send {
    /// Reports whether this backend will decode the described codestream. Returning `false` is the
    /// only way to fall through to the next backend (and ultimately to the built-in tail).
    fn supports(&mut self, info: &CodestreamInfo) -> bool;

    /// Decodes `payload` — the raw RIFF chunk payload, header byte included — into the raster
    /// native to [`info.codestream()`](CodestreamInfo::codestream).
    ///
    /// # Errors
    ///
    /// Any error propagates to the caller of the enclosing decode; the built-in decoder is not
    /// retried.
    fn decode(&mut self, info: &CodestreamInfo, payload: &[u8]) -> Result<DecodedRaster>;
}

/// A pluggable encoder for one WebP codestream.
///
/// Implement this to route encoding to a hardware or alternate software encoder, then install it
/// with [`WebpEncoder::push_backend`](crate::WebpEncoder::push_backend). Same fallback contract and
/// same `Send` requirement as [`WebpCodestreamDecoder`].
pub trait WebpCodestreamEncoder: Send {
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
    fn encode(&mut self, req: &WebpEncodeRequest, raster: &RasterRef<'_>) -> Result<Vec<u8>>;
}

/// A decode registry entry: shared so that cloning a [`WebpDecoder`](crate::WebpDecoder) shares the
/// backend rather than duplicating it, and locked so `&self` decode methods can call `&mut self`
/// backend methods.
pub(crate) type SharedDecoder = Arc<Mutex<dyn WebpCodestreamDecoder + Send>>;

/// An encode registry entry; see [`SharedDecoder`].
pub(crate) type SharedEncoder = Arc<Mutex<dyn WebpCodestreamEncoder + Send>>;

/// The error a poisoned registry lock yields: another thread panicked inside a backend, so the
/// backend's state cannot be trusted.
fn poisoned() -> Error {
    Error::invalid_input(
        env!("CARGO_PKG_NAME"),
        "WebP: a codestream backend panicked (registry lock poisoned)",
    )
}

/// Offers the decode job to each backend in push order, returning the first acceptance's outcome, or
/// `None` when every backend declines (the caller then runs the built-in tail).
pub(crate) fn dispatch_decode(
    backends: &[SharedDecoder],
    info: &CodestreamInfo,
    payload: &[u8],
) -> Option<Result<DecodedRaster>> {
    for backend in backends {
        let mut guard = match backend.lock() {
            Ok(guard) => guard,
            Err(_) => return Some(Err(poisoned())),
        };
        if !guard.supports(info) {
            continue;
        }
        return Some(guard.decode(info, payload));
    }
    None
}

/// Offers the encode job to each backend in push order, returning the first acceptance's outcome, or
/// `None` when every backend declines (the caller then runs the built-in tail).
pub(crate) fn dispatch_encode(
    backends: &[SharedEncoder],
    req: &WebpEncodeRequest,
    raster: &RasterRef<'_>,
) -> Option<Result<Vec<u8>>> {
    for backend in backends {
        let mut guard = match backend.lock() {
            Ok(guard) => guard,
            Err(_) => return Some(Err(poisoned())),
        };
        if !guard.supports(req) {
            continue;
        }
        return Some(guard.encode(req, raster));
    }
    None
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
/// a later backend once one has accepted), so it surfaces as [`Error::Unsupported`]; every other
/// non-OK status is a backend failure and surfaces as [`Error::InvalidInput`].
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
/// - `VP8 ` — [`PIXEL_FORMAT_YUV420`], three 8-bit planes (Y, U, V), tightly packed at visible
///   resolution (strides `width`, `chroma_width`, `chroma_width`);
/// - `VP8L` — [`PIXEL_FORMAT_ARGB`], one plane, stride `width * 4`, bytes in B, G, R, A order.
#[derive(Debug, Clone)]
pub struct AbiDecoderBackend<D> {
    inner: D,
}

impl<D: abi::Decoder> AbiDecoderBackend<D> {
    /// Wraps an ABI decoder backend.
    #[must_use]
    pub const fn new(inner: D) -> Self {
        Self { inner }
    }

    /// Returns the wrapped backend.
    #[must_use]
    pub fn into_inner(self) -> D {
        self.inner
    }
}

impl<D: abi::Decoder + Send> WebpCodestreamDecoder for AbiDecoderBackend<D> {
    fn supports(&mut self, info: &CodestreamInfo) -> bool {
        self.inner.supports(&stream_config(info))
    }

    fn decode(&mut self, info: &CodestreamInfo, payload: &[u8]) -> Result<DecodedRaster> {
        let cfg = stream_config(info);
        let dims = info.dimensions();
        let (w, h) = (dims.width as usize, dims.height as usize);
        match info.codestream() {
            WebpCodestream::Vp8 => {
                let (cw, ch) = (
                    Yuv420::chroma_width(dims.width) as usize,
                    Yuv420::chroma_height(dims.height) as usize,
                );
                let mut y = vec![0u8; w * h];
                let mut u = vec![0u8; cw * ch];
                let mut v = vec![0u8; cw * ch];
                let desc = abi::ImageDesc::new(
                    PIXEL_FORMAT_YUV420,
                    dims.width,
                    dims.height,
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
                Ok(DecodedRaster::Yuv420(Yuv420::new(
                    dims.width,
                    dims.height,
                    y,
                    u,
                    v,
                )?))
            }
            WebpCodestream::Vp8l => {
                let mut bytes = vec![0u8; w * h * 4];
                let desc = abi::ImageDesc::new(
                    PIXEL_FORMAT_ARGB,
                    dims.width,
                    dims.height,
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
                Ok(DecodedRaster::Argb {
                    dimensions: dims,
                    pixels,
                })
            }
        }
    }
}

/// Adapts a [`gamut_codec_abi::Encoder`] into a [`WebpCodestreamEncoder`], mirroring
/// [`AbiDecoderBackend`]: the job's [`codec_id`](WebpCodestream::codec_id) and quality go into the
/// [`abi::EncodeConfig`], the source picture into an [`abi::ImageDesc`] with the same plane layout
/// the decode adapter uses, and the streamed output chunks are concatenated into the returned chunk
/// payload.
#[derive(Debug, Clone)]
pub struct AbiEncoderBackend<E> {
    inner: E,
}

impl<E: abi::Encoder> AbiEncoderBackend<E> {
    /// Wraps an ABI encoder backend.
    #[must_use]
    pub const fn new(inner: E) -> Self {
        Self { inner }
    }

    /// Returns the wrapped backend.
    #[must_use]
    pub fn into_inner(self) -> E {
        self.inner
    }
}

impl<E: abi::Encoder + Send> WebpCodestreamEncoder for AbiEncoderBackend<E> {
    fn supports(&mut self, req: &WebpEncodeRequest) -> bool {
        self.inner.supports(&encode_config(req))
    }

    fn encode(&mut self, req: &WebpEncodeRequest, raster: &RasterRef<'_>) -> Result<Vec<u8>> {
        let cfg = encode_config(req);
        let dims = raster.dimensions();
        // Owned byte staging for the ARGB case; the YUV case borrows the planes in place.
        let mut argb_bytes = Vec::new();
        let desc = match raster {
            RasterRef::Yuv420(yuv) => {
                let cw = Yuv420::chroma_width(dims.width) as usize;
                abi::ImageDesc::new(
                    PIXEL_FORMAT_YUV420,
                    dims.width,
                    dims.height,
                    8,
                    3,
                    [
                        yuv.y().as_ptr().cast_mut(),
                        yuv.u().as_ptr().cast_mut(),
                        yuv.v().as_ptr().cast_mut(),
                        core::ptr::null_mut(),
                    ],
                    [dims.width as usize, cw, cw, 0],
                )
            }
            RasterRef::Argb { pixels, .. } => {
                argb_bytes.extend(pixels.iter().flat_map(|p| p.to_le_bytes()));
                abi::ImageDesc::new(
                    PIXEL_FORMAT_ARGB,
                    dims.width,
                    dims.height,
                    8,
                    1,
                    [
                        argb_bytes.as_mut_ptr(),
                        core::ptr::null_mut(),
                        core::ptr::null_mut(),
                        core::ptr::null_mut(),
                    ],
                    [dims.width as usize * 4, 0, 0, 0],
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
fn stream_config(info: &CodestreamInfo) -> abi::StreamConfig {
    let dims = info.dimensions();
    abi::StreamConfig::new(info.codestream().codec_id(), dims.width, dims.height, 8)
}

/// Builds the ABI encode descriptor for an encode job (no options blob).
fn encode_config(req: &WebpEncodeRequest) -> abi::EncodeConfig {
    abi::EncodeConfig::new(req.codestream().codec_id(), u32::from(req.quality()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dims(width: u32, height: u32) -> Dimensions {
        Dimensions { width, height }
    }

    #[test]
    fn codestream_ids_are_the_chunk_fourccs_and_are_distinct() {
        assert_eq!(WebpCodestream::Vp8.fourcc(), FourCc::VP8);
        assert_eq!(WebpCodestream::Vp8l.fourcc(), FourCc::VP8L);
        assert_eq!(WebpCodestream::Vp8.codec_id(), u32::from_le_bytes(*b"VP8 "));
        assert_eq!(
            WebpCodestream::Vp8l.codec_id(),
            u32::from_le_bytes(*b"VP8L")
        );
        assert_ne!(
            WebpCodestream::Vp8.codec_id(),
            WebpCodestream::Vp8l.codec_id()
        );
    }

    #[test]
    fn pixel_format_tags_are_fourccs_outside_the_pixelformat_range() {
        assert_eq!(PIXEL_FORMAT_YUV420, u32::from_le_bytes(*b"I420"));
        assert_eq!(PIXEL_FORMAT_ARGB, u32::from_le_bytes(*b"BGRA"));
        assert_ne!(PIXEL_FORMAT_YUV420, PIXEL_FORMAT_ARGB);
        // Well clear of gamut_core::PixelFormat's 0..=10 discriminants.
        const { assert!(PIXEL_FORMAT_YUV420 > 1000) };
        const { assert!(PIXEL_FORMAT_ARGB > 1000) };
    }

    #[test]
    fn info_and_request_expose_their_fields() {
        let info = CodestreamInfo::new(WebpCodestream::Vp8l, dims(7, 5));
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
        let yuv = Yuv420::new(2, 2, vec![0; 4], vec![0; 1], vec![0; 1]).unwrap();
        let raster = DecodedRaster::Yuv420(yuv.clone());
        assert_eq!(raster.codestream(), WebpCodestream::Vp8);
        assert_eq!(raster.dimensions(), dims(2, 2));

        let argb = DecodedRaster::Argb {
            dimensions: dims(3, 1),
            pixels: vec![1, 2, 3],
        };
        assert_eq!(argb.codestream(), WebpCodestream::Vp8l);
        assert_eq!(argb.dimensions(), dims(3, 1));

        let yref = RasterRef::Yuv420(&yuv);
        assert_eq!(yref.codestream(), WebpCodestream::Vp8);
        assert_eq!(yref.dimensions(), dims(2, 2));
        let pixels = [1u32, 2, 3];
        let aref = RasterRef::Argb {
            dimensions: dims(3, 1),
            pixels: &pixels,
        };
        assert_eq!(aref.codestream(), WebpCodestream::Vp8l);
        assert_eq!(aref.dimensions(), dims(3, 1));
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
        let cfg = stream_config(&CodestreamInfo::new(WebpCodestream::Vp8, dims(64, 48)));
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
