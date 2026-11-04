//! The pluggable **whole-interchange-stream** codestream seam: [`JpegStreamDecoder`],
//! [`JpegStreamEncoder`], and their [`gamut_codec_abi`] adapters.
//!
//! # Why the seam is the whole SOI..EOI stream
//!
//! Every other gamut format crate splits cleanly into *container* (crate-owned) and *codestream*
//! (delegable): AVIF hands an OBU sequence to an AV1 decoder, HEIC hands a length-prefixed NAL
//! payload to an HEVC decoder. **JPEG-1 has no such split to invert.** Its marker segments and its
//! entropy-coded data interleave in one byte stream (T.81 §B.1.1.5 — entropy-coded segments are
//! delimited by markers, with `0xFF00` byte stuffing inside them), and the Huffman/bit layer
//! (the crate-private `huffman` and `bitwriter` modules) is intrinsic to the frame structure it codes: a scan
//! cannot be lifted out of its SOF/DHT/DQT context and still mean anything.
//!
//! So the seam is defined at the **whole SOI..EOI interchange stream** — the unit every real JPEG
//! engine actually consumes: nvJPEG (`nvjpegDecode` takes the JPEG file bytes), V4L2 stateful and
//! stateless JPEG (`V4L2_PIX_FMT_JPEG`/`JPEG_RAW` take the interchange stream), and libjpeg-turbo
//! (`jpeg_mem_src`). A finer per-scan entropy seam would publicize the crate's internal DCT
//! coefficient, quantizer, and Huffman state as public API — a sub-stream boundary **no** real
//! accelerator consumes, for zero consumers.
//!
//! # The explicit consequence: metadata + validation ownership
//!
//! Because the seam is the whole stream, the workspace convention that "the crate owns the
//! container" degenerates, for JPEG, to the crate owning **metadata and validation**:
//!
//! - **Decode** — the crate still parses the marker layer *first*, into a [`JpegStreamInfo`]
//!   (dimensions, process, per-component sampling, precision), and only then hands the **full**
//!   stream to the first backend whose [`JpegStreamDecoder::supports`] returns `true`. APPn metadata
//!   (EXIF / XMP / multi-segment ICC) is **never** delegated: [`crate::metadata`] is a crate-side
//!   marker walk that no backend can observe, intercept, or alter.
//! - **Encode** — a backend produces the **full** JFIF stream from the raster and the
//!   [`JpegEncodeRequest`]; the crate then validates it (SOI..EOI framing, parsable marker layer)
//!   and **owns the APPn metadata segments in it**: any EXIF / XMP / `ICC_PROFILE` segment the
//!   backend emitted is stripped and the encoder's configured metadata is written in its place, at
//!   the crate's canonical position (after the leading APP0 run). Metadata is therefore *patched*,
//!   never double-written, and the [`crate::JpegEncoder`] caps (`with_exif` / `with_xmp` /
//!   `with_icc_profile`) are validated before any backend is consulted.
//!
//! This is an accepted degradation, not an oversight: any narrower ownership boundary for JPEG
//! would be fictional.
//!
//! # Fallback contract
//!
//! Backends are tried in **push order** ([`crate::JpegDecoder::push_backend`] /
//! [`crate::JpegEncoder::push_backend`]); the crate's own built-in codec is the implicit tail and
//! runs last. `supports() == false` is the only signal that falls through to the next backend. A
//! backend that accepts a job and *then* fails **propagates** its error — the built-in tail is not
//! retried, because a partially-produced result must never be silently masked. The one exception is
//! the codec-abi adapters' *late* [`Status::UNSUPPORTED`](gamut_codec_abi::Status::UNSUPPORTED),
//! which is a decline expressed after acceptance; it is carried as the
//! [`backend_declined`] sentinel error and re-enters the fall-through path.

use std::sync::{Arc, Mutex};

use gamut_codec_abi::{
    Decoder as AbiDecode, EncodeConfig, Encoder as AbiEncode, ImageDesc, MAX_PLANES, Status,
    StreamConfig,
};
use gamut_core::{Error, PixelFormat, Result};

use crate::decoder::{JpegProcess, frame_header};
use crate::encoder::ChromaSubsampling;

/// The codec identifier gamut uses for JPEG-1 on the [`gamut_codec_abi`] seam: the big-endian
/// FourCC `"JPEG"` (`0x4A50_4547`).
///
/// It is written to [`StreamConfig::codec_id`] and [`EncodeConfig::codec_id`] by the adapters in
/// this module, so a foreign backend registered for several formats can dispatch on it.
pub const JPEG_CODEC_ID: u32 = u32::from_be_bytes(*b"JPEG");

/// The message carried by the [`backend_declined`] sentinel error.
const DECLINED_MSG: &str = "JPEG: backend declined the stream after accepting it";

/// The error a backend returns to **decline after accepting** — the late counterpart of
/// `supports() == false`.
///
/// The typed traits report a normal decline through `supports`, so this sentinel exists for
/// backends that can only discover they cannot handle a job once they have started it — notably a
/// [`gamut_codec_abi`] backend that returns [`Status::UNSUPPORTED`] from `decode`/`encode` after
/// having answered its `supports` affirmatively. Returning it from
/// [`JpegStreamDecoder::decode`] / [`JpegStreamEncoder::encode`] resumes the push-order fall-through
/// at the next backend; **every other** error is terminal and propagates to the caller unchanged.
#[must_use]
pub fn backend_declined() -> Error {
    Error::unsupported(env!("CARGO_PKG_NAME"), DECLINED_MSG)
}

/// Returns `true` iff `err` is the [`backend_declined`] sentinel, i.e. the registry treats it as a
/// fall-through rather than a failure.
#[must_use]
pub fn is_backend_declined(err: &Error) -> bool {
    err.kind() == gamut_core::ErrorKind::Unsupported && err.static_message() == Some(DECLINED_MSG)
}

/// The pixel layouts the JPEG seam exchanges: [`PixelFormat::Gray8`] (1 component),
/// [`PixelFormat::Rgb8`] (3), and [`PixelFormat::Cmyk8`] (4) — interleaved 8-bit samples, the forms
/// the crate's own [`crate::JpegDecoder`] presents and its [`crate::JpegEncoder`] accepts.
const SEAM_FORMATS: [PixelFormat; 3] = [PixelFormat::Gray8, PixelFormat::Rgb8, PixelFormat::Cmyk8];

/// Rejects a pixel format outside [`SEAM_FORMATS`].
fn check_format(format: PixelFormat) -> Result<()> {
    if SEAM_FORMATS.contains(&format) {
        Ok(())
    } else {
        Err(Error::unsupported(
            env!("CARGO_PKG_NAME"),
            "JPEG: seam pixel format must be Gray8, Rgb8, or Cmyk8",
        ))
    }
}

/// The exact interleaved sample count for `width × height` pixels of `format`, or `None` on
/// overflow.
fn sample_len(width: u32, height: u32, format: PixelFormat) -> Option<usize> {
    usize::try_from(width)
        .ok()?
        .checked_mul(usize::try_from(height).ok()?)?
        .checked_mul(format.channels())
}

/// What the crate tells a [`JpegStreamDecoder`] about a stream before handing it over: everything
/// the marker layer declares, parsed **by the crate** (T.81 §B.2.2).
///
/// A backend uses it to decide [`JpegStreamDecoder::supports`] without parsing the stream itself,
/// and to size its output. Marked `#[non_exhaustive]`; read through the getters.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct JpegStreamInfo {
    /// Samples per line `X`.
    width: u32,
    /// Number of lines `Y`; `0` when the frame defers its height to a DNL segment (§B.2.5).
    height: u32,
    /// Sample precision `P` in bits, as declared — **not** clamped to what the built-in supports, so
    /// a backend may accept a 12-bit frame the built-in decoder rejects.
    precision: u8,
    /// The DCT process, from the SOFn marker.
    process: JpegProcess,
    /// Per-component sampling factors `(Hi, Vi)` in frame-declaration order (§B.2.2, Table B.2).
    sampling: Vec<(u8, u8)>,
}

impl JpegStreamInfo {
    /// Parses a JPEG stream's marker layer up to and including its frame header — no entropy
    /// decoding — producing exactly what the crate hands a backend.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidInput`] if the stream is malformed or has no frame header, or
    /// [`Error::Unsupported`] if the frame uses a process other than baseline / extended-sequential
    /// / progressive DCT.
    pub fn parse(data: &[u8]) -> Result<Self> {
        let (process, payload) = frame_header(data)?;
        let precision = *payload.first().ok_or_else(crate::decoder::trunc_sof)?;
        let height = u16::from_be_bytes([
            *payload.get(1).ok_or_else(crate::decoder::trunc_sof)?,
            *payload.get(2).ok_or_else(crate::decoder::trunc_sof)?,
        ]);
        let width = u16::from_be_bytes([
            *payload.get(3).ok_or_else(crate::decoder::trunc_sof)?,
            *payload.get(4).ok_or_else(crate::decoder::trunc_sof)?,
        ]);
        let nf = usize::from(*payload.get(5).ok_or_else(crate::decoder::trunc_sof)?);
        let mut sampling = Vec::with_capacity(nf);
        for i in 0..nf {
            // Each component is 3 bytes: Ci, Hi<<4|Vi, Tqi (§B.2.2).
            let hv = *payload
                .get(6 + i * 3 + 1)
                .ok_or_else(crate::decoder::trunc_sof)?;
            sampling.push((hv >> 4, hv & 0x0F));
        }
        Ok(Self {
            width: u32::from(width),
            height: u32::from(height),
            precision,
            process,
            sampling,
        })
    }

    /// Samples per line `X` (§B.2.2).
    #[must_use]
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Number of lines `Y`, or `0` when a DNL segment defines the height (§B.2.5).
    #[must_use]
    pub fn height(&self) -> u32 {
        self.height
    }

    /// Number of frame components `Nf`.
    #[must_use]
    pub fn components(&self) -> u8 {
        self.sampling.len() as u8
    }

    /// Sample precision `P` in bits, exactly as the frame header declares it.
    #[must_use]
    pub fn precision(&self) -> u8 {
        self.precision
    }

    /// The DCT process the frame uses.
    #[must_use]
    pub fn process(&self) -> JpegProcess {
        self.process
    }

    /// Per-component sampling factors `(Hi, Vi)` in frame-declaration order.
    #[must_use]
    pub fn sampling_factors(&self) -> &[(u8, u8)] {
        &self.sampling
    }

    /// The named [`ChromaSubsampling`] the frame uses, or `None` when it is not a three-component
    /// frame whose chroma components are both `1×1` with a luma factor of `1×1`, `2×1`, `2×2`, or
    /// `1×2`.
    ///
    /// `None` therefore covers grayscale, CMYK, and any exotic sampling grid — a backend that cares
    /// about those reads [`sampling_factors`](Self::sampling_factors) instead.
    #[must_use]
    pub fn subsampling(&self) -> Option<ChromaSubsampling> {
        let [luma, cb, cr] = *self.sampling.as_slice() else {
            return None;
        };
        if cb != (1, 1) || cr != (1, 1) {
            return None;
        }
        match luma {
            (1, 1) => Some(ChromaSubsampling::Ycbcr444),
            (2, 1) => Some(ChromaSubsampling::Ycbcr422),
            (2, 2) => Some(ChromaSubsampling::Ycbcr420),
            (1, 2) => Some(ChromaSubsampling::Ycbcr440),
            _ => None,
        }
    }
}

/// The owned output of a [`JpegStreamDecoder`]: one interleaved 8-bit raster.
///
/// Backends return *presented* samples — the form nvJPEG, V4L2, and libjpeg-turbo all emit — so the
/// crate does not re-run colour conversion on backend output. [`PixelFormat::Gray8`] is replicated
/// across channels when the caller asks for [`gamut_core::Rgb8`], matching the built-in decoder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedJpeg {
    width: u32,
    height: u32,
    format: PixelFormat,
    samples: Vec<u8>,
}

impl DecodedJpeg {
    /// Builds a decoded raster, validating it is internally consistent.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Unsupported`] if `format` is not [`PixelFormat::Gray8`],
    /// [`PixelFormat::Rgb8`], or [`PixelFormat::Cmyk8`], and [`Error::InvalidInput`] if either
    /// dimension is `0` or `samples.len()` is not exactly `width * height * channels`.
    pub fn new(width: u32, height: u32, format: PixelFormat, samples: Vec<u8>) -> Result<Self> {
        check_format(format)?;
        if width == 0 || height == 0 {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "JPEG: backend raster has a zero extent",
            ));
        }
        if sample_len(width, height, format) != Some(samples.len()) {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "JPEG: backend raster sample count does not match its dimensions",
            ));
        }
        Ok(Self {
            width,
            height,
            format,
            samples,
        })
    }

    /// Image width in pixels.
    #[must_use]
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Image height in pixels.
    #[must_use]
    pub fn height(&self) -> u32 {
        self.height
    }

    /// The interleaved layout of [`samples`](Self::samples).
    #[must_use]
    pub fn format(&self) -> PixelFormat {
        self.format
    }

    /// The interleaved samples, row-major, `width * height * channels` bytes.
    #[must_use]
    pub fn samples(&self) -> &[u8] {
        &self.samples
    }

    /// Consumes the raster, yielding its samples without a copy.
    #[must_use]
    pub fn into_samples(self) -> Vec<u8> {
        self.samples
    }
}

/// A borrowed interleaved 8-bit raster handed to a [`JpegStreamEncoder`].
///
/// The crate builds it from the [`gamut_core::ImageRef`] the caller passed to
/// [`gamut_core::EncodeImage::encode_image`], so a backend reads the caller's buffer directly with
/// no intermediate copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RasterRef<'a> {
    width: u32,
    height: u32,
    format: PixelFormat,
    samples: &'a [u8],
}

impl<'a> RasterRef<'a> {
    /// Borrows a raster, validating it is internally consistent.
    ///
    /// # Errors
    ///
    /// Same contract as [`DecodedJpeg::new`]: [`Error::Unsupported`] for a format outside
    /// `Gray8`/`Rgb8`/`Cmyk8`, [`Error::InvalidInput`] for a zero extent or a sample count that is
    /// not exactly `width * height * channels`.
    pub fn new(width: u32, height: u32, format: PixelFormat, samples: &'a [u8]) -> Result<Self> {
        check_format(format)?;
        if width == 0 || height == 0 {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "JPEG: backend raster has a zero extent",
            ));
        }
        if sample_len(width, height, format) != Some(samples.len()) {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "JPEG: backend raster sample count does not match its dimensions",
            ));
        }
        Ok(Self {
            width,
            height,
            format,
            samples,
        })
    }

    /// Image width in pixels.
    #[must_use]
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Image height in pixels.
    #[must_use]
    pub fn height(&self) -> u32 {
        self.height
    }

    /// The interleaved layout of [`samples`](Self::samples).
    #[must_use]
    pub fn format(&self) -> PixelFormat {
        self.format
    }

    /// The interleaved samples, row-major, `width * height * channels` bytes.
    #[must_use]
    pub fn samples(&self) -> &[u8] {
        self.samples
    }
}

/// The encode job handed to a [`JpegStreamEncoder`]: the raster's shape plus the
/// [`crate::JpegEncoder`] settings that shape the coded stream.
///
/// Metadata is deliberately **absent**: APPn segments are crate-owned and are patched into whatever
/// the backend produces (see the module docs), so a backend never has to frame them. Marked
/// `#[non_exhaustive]`; read through the getters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct JpegEncodeRequest {
    width: u32,
    height: u32,
    format: PixelFormat,
    quality: u8,
    subsampling: ChromaSubsampling,
    progressive: bool,
    restart_interval: u16,
}

impl JpegEncodeRequest {
    /// Builds a request. Crate-internal: the encoder is the only producer, and it always builds one
    /// whose `width`/`height`/`format` equal the accompanying [`RasterRef`]'s.
    pub(crate) fn new(
        width: u32,
        height: u32,
        format: PixelFormat,
        quality: u8,
        subsampling: ChromaSubsampling,
        progressive: bool,
        restart_interval: u16,
    ) -> Self {
        Self {
            width,
            height,
            format,
            quality,
            subsampling,
            progressive,
            restart_interval,
        }
    }

    /// Image width in pixels; equals the accompanying [`RasterRef`]'s.
    #[must_use]
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Image height in pixels; equals the accompanying [`RasterRef`]'s.
    #[must_use]
    pub fn height(&self) -> u32 {
        self.height
    }

    /// The input raster layout; equals the accompanying [`RasterRef`]'s.
    #[must_use]
    pub fn format(&self) -> PixelFormat {
        self.format
    }

    /// The requested quality, already clamped to `1..=100` by
    /// [`JpegEncoder::with_quality`](crate::JpegEncoder::with_quality).
    #[must_use]
    pub fn quality(&self) -> u8 {
        self.quality
    }

    /// The requested chroma subsampling. Meaningless for a [`PixelFormat::Gray8`] raster, which has
    /// no chroma components; the crate still reports the configured value.
    #[must_use]
    pub fn subsampling(&self) -> ChromaSubsampling {
        self.subsampling
    }

    /// `true` when the progressive DCT process (SOF2) was requested.
    #[must_use]
    pub fn progressive(&self) -> bool {
        self.progressive
    }

    /// The requested restart interval in MCUs; `0` disables restarts.
    #[must_use]
    pub fn restart_interval(&self) -> u16 {
        self.restart_interval
    }
}

/// A pluggable JPEG **decode** backend, consuming a whole SOI..EOI interchange stream.
///
/// `Send` is a supertrait because the crate stores backends behind an `Arc<Mutex<..>>` so
/// [`crate::JpegDecoder`] stays `Clone` with `&self` decode methods; a `!Send` backend can still be
/// wrapped by the caller.
pub trait JpegStreamDecoder: Send {
    /// Reports whether this backend can decode the stream the crate has just parsed. Returning
    /// `false` is the primary signal that lets the crate fall through to the next backend, and
    /// finally to its own built-in decoder.
    fn supports(&mut self, info: &JpegStreamInfo) -> bool;

    /// Decodes `jpeg` — the **complete** SOI..EOI interchange stream, exactly as the caller supplied
    /// it, including any APPn segments — to an interleaved raster.
    ///
    /// # Errors
    ///
    /// Any error propagates to the caller unchanged and the built-in decoder is **not** tried,
    /// except [`backend_declined`], which resumes the fall-through at the next backend.
    fn decode(&mut self, info: &JpegStreamInfo, jpeg: &[u8]) -> Result<DecodedJpeg>;
}

/// A pluggable JPEG **encode** backend, producing a whole SOI..EOI interchange stream.
///
/// The returned stream is validated by the crate, and its APPn metadata segments are replaced by
/// the encoder's configured EXIF / XMP / ICC (see the module docs) — a backend neither has to emit
/// them nor can override them.
pub trait JpegStreamEncoder: Send {
    /// Reports whether this backend can satisfy the encode job. Returning `false` falls through to
    /// the next backend, and finally to the crate's own built-in encoder.
    fn supports(&mut self, req: &JpegEncodeRequest) -> bool;

    /// Encodes `image` to a **complete** JFIF interchange stream (`FFD8` … `FFD9`).
    ///
    /// # Errors
    ///
    /// Any error propagates to the caller unchanged and the built-in encoder is **not** tried,
    /// except [`backend_declined`], which resumes the fall-through at the next backend.
    fn encode(&mut self, req: &JpegEncodeRequest, image: &RasterRef<'_>) -> Result<Vec<u8>>;
}

/// One registry slot. `Arc` so cloning a codec **shares** its backends; `Mutex` so the codec's
/// `&self` methods can call the backends' `&mut self` ones.
pub(crate) type DecoderSlot = Arc<Mutex<dyn JpegStreamDecoder>>;

/// One registry slot; see [`DecoderSlot`].
pub(crate) type EncoderSlot = Arc<Mutex<dyn JpegStreamEncoder>>;

/// Raised when a backend panicked while holding its registry lock.
fn poisoned() -> Error {
    Error::invalid_input(
        env!("CARGO_PKG_NAME"),
        "JPEG: a backend poisoned the registry lock",
    )
}

/// Runs the decode registry in push order, returning the first accepted result, or `None` when
/// every backend declined (the caller then runs the built-in decoder).
pub(crate) fn decode_with_backends(
    backends: &[DecoderSlot],
    info: &JpegStreamInfo,
    jpeg: &[u8],
) -> Result<Option<DecodedJpeg>> {
    for slot in backends {
        let mut backend = slot.lock().map_err(|_| poisoned())?;
        if !backend.supports(info) {
            continue;
        }
        match backend.decode(info, jpeg) {
            Ok(decoded) => return Ok(Some(decoded)),
            Err(err) if is_backend_declined(&err) => continue,
            Err(err) => return Err(err),
        }
    }
    Ok(None)
}

/// Runs the encode registry in push order, returning the first accepted stream, or `None` when
/// every backend declined (the caller then runs the built-in encoder).
pub(crate) fn encode_with_backends(
    backends: &[EncoderSlot],
    req: &JpegEncodeRequest,
    image: &RasterRef<'_>,
) -> Result<Option<Vec<u8>>> {
    for slot in backends {
        let mut backend = slot.lock().map_err(|_| poisoned())?;
        if !backend.supports(req) {
            continue;
        }
        match backend.encode(req, image) {
            Ok(stream) => return Ok(Some(stream)),
            Err(err) if is_backend_declined(&err) => continue,
            Err(err) => return Err(err),
        }
    }
    Ok(None)
}

/// The [`StreamConfig`] the decode adapter builds from a [`JpegStreamInfo`].
fn stream_config(info: &JpegStreamInfo) -> StreamConfig {
    // No extradata: a JPEG carries its tables inline, so the codestream is self-contained.
    StreamConfig::new(
        JPEG_CODEC_ID,
        info.width(),
        info.height(),
        u32::from(info.precision()),
    )
}

/// The [`EncodeConfig`] the encode adapter builds from a [`JpegEncodeRequest`].
fn encode_config(req: &JpegEncodeRequest) -> EncodeConfig {
    EncodeConfig::new(JPEG_CODEC_ID, u32::from(req.quality()))
}

/// The interleaved output layout the decode adapter allocates for an `Nf`-component frame: 1 → gray,
/// 3 → RGB, 4 → CMYK, mirroring what the built-in decoder presents.
fn abi_output_format(info: &JpegStreamInfo) -> Result<PixelFormat> {
    match info.components() {
        1 => Ok(PixelFormat::Gray8),
        3 => Ok(PixelFormat::Rgb8),
        4 => Ok(PixelFormat::Cmyk8),
        _ => Err(Error::unsupported(
            env!("CARGO_PKG_NAME"),
            "JPEG: only 1, 3, or 4 component streams are supported",
        )),
    }
}

/// Builds a single-plane [`ImageDesc`] over an interleaved 8-bit raster.
fn image_desc(width: u32, height: u32, format: PixelFormat, base: *mut u8) -> ImageDesc {
    let mut planes = [std::ptr::null_mut(); MAX_PLANES];
    let mut strides = [0usize; MAX_PLANES];
    planes[0] = base;
    strides[0] = width as usize * format.channels();
    ImageDesc::new(format as u32, width, height, 8, 1, planes, strides)
}

/// Adapts a [`gamut_codec_abi::Decoder`] — a foreign C backend reached through
/// [`gamut_codec_abi::bridge::ForeignDecoder`], or any Rust implementation of the shared twin — into
/// a [`JpegStreamDecoder`].
///
/// The adapter allocates the interleaved output buffer (sized from the frame's component count, see
/// the crate's presentation rules), fills a single-plane [`ImageDesc`] over it, and calls the ABI
/// `decode` with the **whole** interchange stream as the codestream. A late
/// [`Status::UNSUPPORTED`] becomes [`backend_declined`] — the registry then tries the next backend;
/// any other non-OK status becomes a terminal [`Error::InvalidInput`] that propagates. The numeric
/// status is retained as diagnostic detail.
#[derive(Debug, Clone)]
pub struct AbiStreamDecoder<D> {
    inner: D,
}

impl<D> AbiStreamDecoder<D> {
    /// Wraps an ABI decoder backend.
    #[must_use]
    pub fn new(inner: D) -> Self {
        Self { inner }
    }
}

impl<D: AbiDecode + Send> JpegStreamDecoder for AbiStreamDecoder<D> {
    fn supports(&mut self, info: &JpegStreamInfo) -> bool {
        self.inner.supports(&stream_config(info))
    }

    fn decode(&mut self, info: &JpegStreamInfo, jpeg: &[u8]) -> Result<DecodedJpeg> {
        let format = abi_output_format(info)?;
        let (width, height) = (info.width(), info.height());
        let len = sample_len(width, height, format)
            .filter(|&n| n != 0)
            .ok_or_else(|| {
                Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "JPEG: frame dimensions cannot be decoded into a raster",
                )
            })?;
        let mut samples = vec![0u8; len];
        let desc = image_desc(width, height, format, samples.as_mut_ptr());
        let status = self.inner.decode(&stream_config(info), jpeg, &desc);
        if status.is_ok() {
            DecodedJpeg::new(width, height, format, samples)
        } else if status.is_unsupported() {
            Err(backend_declined())
        } else {
            Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "JPEG: codec-abi decode backend returned a failure status",
            )
            .with_detail(format!("codec-abi status {}", status.0)))
        }
    }
}

/// Adapts a [`gamut_codec_abi::Encoder`] — a foreign C backend reached through
/// [`gamut_codec_abi::bridge::ForeignEncoder`], or any Rust implementation of the shared twin —
/// into a [`JpegStreamEncoder`].
///
/// The adapter describes the caller's raster as a single-plane [`ImageDesc`] and concatenates the
/// chunks the backend streams through the sink into the interchange stream. Late-status handling
/// mirrors [`AbiStreamDecoder`].
#[derive(Debug, Clone)]
pub struct AbiStreamEncoder<E> {
    inner: E,
}

impl<E> AbiStreamEncoder<E> {
    /// Wraps an ABI encoder backend.
    #[must_use]
    pub fn new(inner: E) -> Self {
        Self { inner }
    }
}

impl<E: AbiEncode + Send> JpegStreamEncoder for AbiStreamEncoder<E> {
    fn supports(&mut self, req: &JpegEncodeRequest) -> bool {
        self.inner.supports(&encode_config(req))
    }

    fn encode(&mut self, req: &JpegEncodeRequest, image: &RasterRef<'_>) -> Result<Vec<u8>> {
        // `ImageDesc` is one descriptor for both directions, so its plane pointers are `*mut` even
        // for an encode input the backend only reads.
        let desc = image_desc(
            image.width(),
            image.height(),
            image.format(),
            image.samples().as_ptr().cast_mut(),
        );
        let mut stream = Vec::new();
        let status = self
            .inner
            .encode(&encode_config(req), &desc, &mut |chunk: &[u8]| {
                stream.extend_from_slice(chunk);
                Status::OK
            });
        if status.is_ok() {
            Ok(stream)
        } else if status.is_unsupported() {
            Err(backend_declined())
        } else {
            Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "JPEG: codec-abi encode backend returned a failure status",
            )
            .with_detail(format!("codec-abi status {}", status.0)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codec_id_is_the_jpeg_fourcc() {
        assert_eq!(JPEG_CODEC_ID, 0x4A50_4547);
    }

    #[test]
    fn declined_sentinel_round_trips_and_nothing_else_matches() {
        let err = backend_declined();
        assert!(is_backend_declined(&err));
        assert_eq!(
            err.to_string(),
            "unsupported: JPEG: backend declined the stream after accepting it [origin: gamut-jpeg]"
        );
        assert!(!is_backend_declined(&Error::unsupported(
            env!("CARGO_PKG_NAME"),
            "JPEG: something else"
        )));
        assert!(!is_backend_declined(&Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            DECLINED_MSG
        )));
    }

    #[test]
    fn seam_formats_are_the_three_presented_layouts() {
        for f in SEAM_FORMATS {
            assert!(check_format(f).is_ok(), "{f:?}");
        }
        assert_eq!(
            check_format(PixelFormat::Rgba8).unwrap_err().to_string(),
            "unsupported: JPEG: seam pixel format must be Gray8, Rgb8, or Cmyk8 [origin: gamut-jpeg]"
        );
        assert_eq!(
            check_format(PixelFormat::Gray16).unwrap_err().to_string(),
            "unsupported: JPEG: seam pixel format must be Gray8, Rgb8, or Cmyk8 [origin: gamut-jpeg]"
        );
    }

    #[test]
    fn sample_len_multiplies_channels_and_detects_overflow() {
        assert_eq!(sample_len(4, 3, PixelFormat::Gray8), Some(12));
        assert_eq!(sample_len(4, 3, PixelFormat::Rgb8), Some(36));
        assert_eq!(sample_len(4, 3, PixelFormat::Cmyk8), Some(48));
        assert_eq!(sample_len(u32::MAX, u32::MAX, PixelFormat::Cmyk8), None);
    }

    #[test]
    fn raster_and_decoded_validate_extent_and_length() {
        assert_eq!(
            DecodedJpeg::new(0, 2, PixelFormat::Gray8, Vec::new())
                .unwrap_err()
                .to_string(),
            "invalid input: JPEG: backend raster has a zero extent [origin: gamut-jpeg]"
        );
        assert_eq!(
            DecodedJpeg::new(2, 0, PixelFormat::Gray8, Vec::new())
                .unwrap_err()
                .to_string(),
            "invalid input: JPEG: backend raster has a zero extent [origin: gamut-jpeg]"
        );
        assert_eq!(
            DecodedJpeg::new(2, 2, PixelFormat::Gray8, vec![0; 3])
                .unwrap_err()
                .to_string(),
            "invalid input: JPEG: backend raster sample count does not match its dimensions [origin: gamut-jpeg]"
        );
        assert_eq!(
            DecodedJpeg::new(2, 2, PixelFormat::Gray8, vec![0; 5])
                .unwrap_err()
                .to_string(),
            "invalid input: JPEG: backend raster sample count does not match its dimensions [origin: gamut-jpeg]"
        );
        let ok = DecodedJpeg::new(2, 2, PixelFormat::Gray8, vec![1, 2, 3, 4]).unwrap();
        assert_eq!((ok.width(), ok.height()), (2, 2));
        assert_eq!(ok.format(), PixelFormat::Gray8);
        assert_eq!(ok.samples(), &[1, 2, 3, 4]);
        assert_eq!(ok.into_samples(), vec![1, 2, 3, 4]);

        let buf = [0u8; 12];
        assert_eq!(
            RasterRef::new(0, 2, PixelFormat::Rgb8, &buf)
                .unwrap_err()
                .to_string(),
            "invalid input: JPEG: backend raster has a zero extent [origin: gamut-jpeg]"
        );
        assert_eq!(
            RasterRef::new(2, 0, PixelFormat::Rgb8, &buf)
                .unwrap_err()
                .to_string(),
            "invalid input: JPEG: backend raster has a zero extent [origin: gamut-jpeg]"
        );
        assert_eq!(
            RasterRef::new(2, 3, PixelFormat::Rgb8, &buf)
                .unwrap_err()
                .to_string(),
            "invalid input: JPEG: backend raster sample count does not match its dimensions [origin: gamut-jpeg]"
        );
        let r = RasterRef::new(2, 2, PixelFormat::Rgb8, &buf).unwrap();
        assert_eq!((r.width(), r.height()), (2, 2));
        assert_eq!(r.format(), PixelFormat::Rgb8);
        assert_eq!(r.samples().len(), 12);
    }

    #[test]
    fn abi_output_format_follows_component_count() {
        let info = |nf: usize| JpegStreamInfo {
            width: 4,
            height: 4,
            precision: 8,
            process: JpegProcess::Baseline,
            sampling: vec![(1, 1); nf],
        };
        assert_eq!(abi_output_format(&info(1)).unwrap(), PixelFormat::Gray8);
        assert_eq!(abi_output_format(&info(3)).unwrap(), PixelFormat::Rgb8);
        assert_eq!(abi_output_format(&info(4)).unwrap(), PixelFormat::Cmyk8);
        for nf in [2, 5] {
            assert_eq!(
                abi_output_format(&info(nf)).unwrap_err().to_string(),
                "unsupported: JPEG: only 1, 3, or 4 component streams are supported [origin: gamut-jpeg]"
            );
        }
    }

    #[test]
    fn descriptors_carry_the_jpeg_codec_id_and_frame_shape() {
        let info = JpegStreamInfo {
            width: 7,
            height: 5,
            precision: 12,
            process: JpegProcess::Progressive,
            sampling: vec![(2, 2), (1, 1), (1, 1)],
        };
        let cfg = stream_config(&info);
        assert_eq!(cfg.codec_id, JPEG_CODEC_ID);
        assert_eq!((cfg.width, cfg.height), (7, 5));
        assert_eq!(cfg.bit_depth, 12);
        assert_eq!(cfg.extradata_len, 0);
        assert!(cfg.extradata.is_null());
        assert!(cfg.is_abi_current());

        let req = JpegEncodeRequest::new(
            7,
            5,
            PixelFormat::Rgb8,
            83,
            ChromaSubsampling::Ycbcr422,
            true,
            9,
        );
        let ecfg = encode_config(&req);
        assert_eq!(ecfg.codec_id, JPEG_CODEC_ID);
        assert_eq!(ecfg.quality, 83);
        assert_eq!(ecfg.extra_len, 0);
        assert!(ecfg.is_abi_current());

        assert_eq!((req.width(), req.height()), (7, 5));
        assert_eq!(req.format(), PixelFormat::Rgb8);
        assert_eq!(req.quality(), 83);
        assert_eq!(req.subsampling(), ChromaSubsampling::Ycbcr422);
        assert!(req.progressive());
        assert_eq!(req.restart_interval(), 9);
    }

    #[test]
    fn image_desc_describes_one_interleaved_plane() {
        let mut buf = [0u8; 24];
        let base = buf.as_mut_ptr();
        let desc = image_desc(4, 2, PixelFormat::Rgb8, base);
        assert_eq!(desc.pixel_format, PixelFormat::Rgb8 as u32);
        assert_eq!((desc.width, desc.height), (4, 2));
        assert_eq!(desc.depth, 8);
        assert_eq!(desc.plane_count, 1);
        assert_eq!(desc.planes[0], base);
        assert_eq!(desc.strides[0], 12);
        for i in 1..MAX_PLANES {
            assert!(desc.planes[i].is_null());
            assert_eq!(desc.strides[i], 0);
        }
        assert!(desc.is_abi_current());

        assert_eq!(image_desc(4, 2, PixelFormat::Gray8, base).strides[0], 4);
        assert_eq!(image_desc(4, 2, PixelFormat::Cmyk8, base).strides[0], 16);
    }

    #[test]
    fn subsampling_maps_only_the_named_grids() {
        let info = |sampling: Vec<(u8, u8)>| JpegStreamInfo {
            width: 8,
            height: 8,
            precision: 8,
            process: JpegProcess::Baseline,
            sampling,
        };
        assert_eq!(
            info(vec![(1, 1), (1, 1), (1, 1)]).subsampling(),
            Some(ChromaSubsampling::Ycbcr444)
        );
        assert_eq!(
            info(vec![(2, 1), (1, 1), (1, 1)]).subsampling(),
            Some(ChromaSubsampling::Ycbcr422)
        );
        assert_eq!(
            info(vec![(2, 2), (1, 1), (1, 1)]).subsampling(),
            Some(ChromaSubsampling::Ycbcr420)
        );
        assert_eq!(
            info(vec![(1, 2), (1, 1), (1, 1)]).subsampling(),
            Some(ChromaSubsampling::Ycbcr440)
        );
        // Not three components.
        assert_eq!(info(vec![(1, 1)]).subsampling(), None);
        assert_eq!(info(vec![(1, 1); 4]).subsampling(), None);
        // Non-1×1 chroma.
        assert_eq!(info(vec![(2, 2), (2, 1), (1, 1)]).subsampling(), None);
        assert_eq!(info(vec![(2, 2), (1, 1), (1, 2)]).subsampling(), None);
        // Unnamed luma grid.
        assert_eq!(info(vec![(4, 1), (1, 1), (1, 1)]).subsampling(), None);
        assert_eq!(info(vec![(2, 4), (1, 1), (1, 1)]).subsampling(), None);
    }
}
