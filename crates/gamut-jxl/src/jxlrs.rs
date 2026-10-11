//! The **built-in decode tail**: a safe front end over the pure-Rust jxl-rs decoder (the [`jxl`]
//! crate), tried last by [`JxlDecoder`](crate::JxlDecoder) after every pushed backend.
//!
//! Decoding drives jxl-rs's typestate API (`Initialized → WithImageInfo → WithFrameInfo`) once per
//! call, requesting the stream's *natural* colour layout at the caller's requested bit width, then
//! reconciling channels to the requested [`gamut_core::Pixel`] layout in [`crate::convert`]. All of
//! that is 100% safe Rust: this module contains no `unsafe`.

use gamut_core::convert::{ConvertPolicy, LumaPolicy, RawImage, convert_from_raw};
use gamut_core::{Dimensions, Error, ImageBuf, Pixel, PixelFormat, Result};
use jxl::api::states::Initialized;
use jxl::api::{
    Endianness, JxlBitDepth, JxlColorProfile, JxlColorType, JxlDataFormat,
    JxlDecoder as JxlRsDecoder, JxlDecoderOptions, JxlOutputBuffer, JxlPixelFormat,
    ProcessingResult,
};
use jxl::headers::extra_channels::ExtraChannel;

use crate::backend::layout_of;
use crate::convert::{ConvSample, native_format, reassemble};
use crate::error::map_decode_error;

/// Upper bound on decoded size, in **pixels × channels**, enforced by jxl-rs's
/// [`JxlDecoderOptions::pixel_limit`]. `1 << 28` (≈268M) samples caps a hostile stream's memory use
/// (a 16-bit RGBA image at this limit is ~512 MiB of output) while comfortably admitting every
/// realistic still image. The limit is deliberately generous and additive to lower later.
const PIXEL_LIMIT: usize = 1 << 28;

/// Parses the stream's headers and returns its basic properties without decoding any pixels.
pub(crate) fn info(data: &[u8]) -> Result<JxlInfo> {
    let mut options = JxlDecoderOptions::default();
    options.pixel_limit = Some(PIXEL_LIMIT);
    let mut input: &[u8] = data;

    let decoder = JxlRsDecoder::<Initialized>::new(options);
    let decoder = match decoder.process(&mut input).map_err(map_decode_error)? {
        ProcessingResult::Complete { result } => result,
        ProcessingResult::NeedsMoreInput { .. } => return Err(truncated()),
    };

    let basic = decoder.basic_info();
    let (Ok(width), Ok(height)) = (u32::try_from(basic.size.0), u32::try_from(basic.size.1)) else {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "JXL: image dimensions overflow",
        ));
    };
    let has_alpha = basic
        .extra_channels
        .iter()
        .any(|c| c.ec_type == ExtraChannel::Alpha);
    Ok(JxlInfo {
        dimensions: Dimensions::new(width, height)?,
        bits_per_sample: basic.bit_depth.bits_per_sample(),
        is_float: matches!(basic.bit_depth, JxlBitDepth::Float { .. }),
        color_channels: if decoder.current_pixel_format().color_type.is_grayscale() {
            1
        } else {
            3
        },
        has_alpha,
        animated: basic.animation.is_some(),
        uses_original_profile: basic.uses_original_profile,
    })
}

/// Returns the ICC profile embedded in the stream's metadata, or `None` for a structured
/// (enumerated) colour encoding.
pub(crate) fn embedded_icc_profile(data: &[u8]) -> Result<Option<Vec<u8>>> {
    // `JxlDecoderOptions` is `#[non_exhaustive]`; build from `Default` and set what we use.
    let mut options = JxlDecoderOptions::default();
    options.pixel_limit = Some(PIXEL_LIMIT);
    let mut input: &[u8] = data;

    // Initialized -> WithImageInfo: parse the file headers, which include the colour profile.
    let decoder = JxlRsDecoder::<Initialized>::new(options);
    let decoder = match decoder.process(&mut input).map_err(map_decode_error)? {
        ProcessingResult::Complete { result } => result,
        ProcessingResult::NeedsMoreInput { .. } => return Err(truncated()),
    };

    match decoder.embedded_color_profile() {
        JxlColorProfile::Icc(bytes) => Ok(Some(bytes.clone())),
        JxlColorProfile::Simple(_) => Ok(None),
    }
}

/// The jxl-rs output format and destination channel layout for one of gamut-jxl's eight coded
/// pixel formats: the native-endian data format, whether the destination is in the grayscale
/// family, its colour-sample count, and whether it carries alpha.
///
/// Derived from the single [`layout_of`] table so the decoder cannot disagree with the encoder or
/// the backend seam about what a layout brand means.
fn output_layout(format: PixelFormat) -> Result<(JxlDataFormat, bool, usize, bool)> {
    let (color_channels, has_alpha, bits) = layout_of(format).ok_or_else(|| {
        Error::unsupported(
            env!("CARGO_PKG_NAME"),
            "JXL: pixel format is not a JPEG XL coded layout",
        )
    })?;
    let data_format = if bits == 8 { u8_format() } else { u16_format() };
    Ok((
        data_format,
        color_channels == 1,
        color_channels as usize,
        has_alpha,
    ))
}

/// Basic properties of a JPEG XL stream, read from its headers by [`JxlDecoder::info`] without
/// decoding pixels.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JxlInfo {
    /// The image dimensions (display orientation).
    pub dimensions: Dimensions,
    /// The declared bits per sample (integer precision, or the float format's total width).
    pub bits_per_sample: u32,
    /// Whether the samples are floating point (e.g. fp16 HDR) rather than integers.
    pub is_float: bool,
    /// Colour samples per pixel: 1 (grayscale) or 3 (RGB).
    pub color_channels: u8,
    /// Whether an alpha channel is present.
    pub has_alpha: bool,
    /// Whether the stream is animated (which pixel decoding rejects).
    pub animated: bool,
    /// Whether the samples are coded in the stream's original colour space, so decoded pixels are
    /// in the colour encoding the stream embeds — the inverse of the image header's `xyb_encoded`
    /// flag (`references/jxl/format_overview.md`, "Color Management").
    ///
    /// `false` for an XYB-coded (lossy) stream, whose signalled colour space is only a suggested
    /// target: decoded pixels are rendered to the embedded *structured* encoding where it has one,
    /// and to sRGB when it embeds an ICC profile (no colour-management transform is applied), so
    /// that profile describes the original image, not the decoded pixels — re-tagging them with it
    /// mislabels them.
    pub uses_original_profile: bool,
}

/// How much of the image a best-effort decode of a possibly-truncated stream produced.
///
/// The discriminants are explicit and permanent so the value can cross the C ABI boundary as-is;
/// the enum is `#[non_exhaustive]` and new variants are appended.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum JxlRender {
    /// The image headers were read but no frame data was: the returned buffer carries the stream's
    /// declared dimensions and is entirely zero.
    HeaderOnly = 0,
    /// A best-effort render of a truncated frame. Some — or all — samples may still be zero: how
    /// much jxl-rs is willing to draw from an incomplete frame is its own heuristic, and a frame
    /// small enough to be coded as a single group yields nothing at all.
    BestEffort = 1,
    /// The frame decoded to completion; the samples are identical to
    /// [`DecodeImage::decode_image`](gamut_core::DecodeImage::decode_image)'s.
    Complete = 2,
}

/// What a [`DecodePartialImage`](crate::DecodePartialImage) decode managed to reconstruct.
///
/// [`is_complete`](JxlPartialReport::is_complete) is the completeness flag; the remaining fields
/// are diagnostics, and each carries a caveat worth reading before relying on it.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JxlPartialReport {
    /// How much of the image the returned buffer carries.
    pub render: JxlRender,
    /// Passes fully decoded for the frame, as jxl-rs counts them after the flush.
    ///
    /// This is the **minimum over all groups**, so it is a progressive-refinement counter rather
    /// than a coverage metric: a single-pass stream (what this crate's encoder produces) reports
    /// `0` for every truncation, and a non-zero count does not mean every pixel is final. It is
    /// also `0` on a [`JxlRender::Complete`] decode, where jxl-rs no longer exposes the count.
    /// Never infer completeness from it — use [`is_complete`](JxlPartialReport::is_complete).
    pub completed_passes: u32,
    /// jxl-rs's estimate of how many further bytes it wanted, or `None` on a complete decode.
    ///
    /// An estimate for sizing the next read — not a byte offset into the input, and not a promise
    /// that that many more bytes would finish the frame.
    pub additional_bytes_hint: Option<u64>,
}

impl JxlPartialReport {
    /// Whether the whole codestream was decoded — the completeness flag.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        matches!(self.render, JxlRender::Complete)
    }

    /// The report for a decode that consumed the whole stream.
    fn complete() -> Self {
        Self {
            render: JxlRender::Complete,
            completed_passes: 0,
            additional_bytes_hint: None,
        }
    }
}

/// What [`decode_raw`] does when jxl-rs reports that it needs more input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Truncation {
    /// Truncation is a typed error — the [`DecodeImage`](gamut_core::DecodeImage) contract.
    Reject,
    /// Render what jxl-rs has and report how far it got — the
    /// [`DecodePartialImage`](crate::DecodePartialImage) contract.
    BestEffort,
}

/// The decode policies [`decode_raw`] applies, bundled so its signature stays readable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RawOptions {
    /// Whether integer output carries the codestream's declared bit depth.
    codestream_bit_depth: bool,
    /// What to do with a stream that ends early.
    truncation: Truncation,
}

/// Narrows a jxl-rs `usize` count to `u32`, saturating rather than wrapping.
///
/// Only ever applied to diagnostic counters, where a saturated value is a better answer than a
/// wrapped one and neither is worth failing a decode over.
fn saturating_u32(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// Widens (or saturates) a jxl-rs `usize` byte hint to `u64`.
fn saturating_u64(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

/// The raw output of one decoded frame: jxl-rs's interleaved byte buffer in the stream's natural
/// colour layout, plus the metadata [`convert_into`] needs to reshape it.
struct RawFrame {
    /// The decoded image dimensions (display orientation).
    dims: Dimensions,
    /// The interleaved sample bytes, native byte order, occupying `bytes[offset..offset + len]`.
    bytes: Vec<u8>,
    /// Start of the used region within `bytes` (0 or 1; a parity offset that guarantees 2-byte row
    /// alignment for 16-bit output — see [`aligned_backing`]).
    offset: usize,
    /// Length of the used region within `bytes`.
    len: usize,
    /// Colour samples per pixel in `bytes`: 1 (grayscale) or 3 (RGB).
    src_color: usize,
    /// Whether an interleaved alpha sample follows the colour samples in `bytes`.
    src_alpha: bool,
}

impl RawFrame {
    /// The used byte region.
    fn samples(&self) -> &[u8] {
        &self.bytes[self.offset..self.offset + self.len]
    }
}

/// Whether a decoded output buffer holding `bytes_per_sample`-wide samples needs a 2-byte-aligned
/// base address.
///
/// jxl-rs's interleaved 16-bit output fast path reinterprets the byte buffer as `&mut [u16]` only
/// when it is 2-byte aligned, and **silently writes nothing** into an unaligned buffer (it returns a
/// zero written-length), so a decoded 16-bit image would come back as all-zero garbage. 8-bit output
/// is written byte-wise and has no alignment requirement. Hence exactly the 2-byte sample width needs
/// alignment. Factored out so the decision is unit-tested rather than buried in a call argument.
fn needs_row_alignment(bytes_per_sample: usize) -> bool {
    bytes_per_sample == 2
}

/// Allocates a zeroed byte buffer of `total` bytes and returns it with an offset into it such that
/// `buf[offset..]` begins at an even address when `align2` is set.
///
/// jxl-rs requires each output row of a 16-bit (2-byte) pixel format to start on a 2-byte boundary,
/// or it may panic. Our rows are tightly packed at an even stride (`width × channels × 2`), so it
/// suffices to make the buffer's *base* even: we over-allocate by one byte and skip the first byte
/// when the allocation happens to land on an odd address. Reading the pointer's address is a plain
/// `as` cast — no `unsafe`, no dereference — so the crate stays `#![deny(unsafe_code)]`.
fn aligned_backing(total: usize, align2: bool) -> (Vec<u8>, usize) {
    if !align2 {
        return (vec![0u8; total], 0);
    }
    let buf = vec![0u8; total + 1];
    // `base & 1` is 1 for an odd base (skip one byte to reach the next even address) and 0 for an
    // even base (already aligned).
    let offset = (buf.as_ptr() as usize) & 1;
    (buf, offset)
}

/// Decodes `data` into jxl-rs's natural interleaved layout at `S`'s bit width, applying the decode
/// policies (pixel limit, animation/premultiplied/color-as-gray rejection, truncation) before any
/// channel reshaping.
///
/// `dst_is_gray_family` is the requested layout's colour family; a grayscale request against a colour
/// stream is rejected here (refusing to guess a luminance). `data_format` selects the output bit
/// width (8- or 16-bit, native endianness) and must agree with `S`; when
/// [`RawOptions::codestream_bit_depth`] is set, an integer stream's declared depth replaces the
/// format's full-range depth.
///
/// [`RawOptions::truncation`] decides what a stream that ends early yields — a typed error, or the
/// best-effort frame plus the [`JxlPartialReport`] describing it. Every *other* refusal (animation,
/// premultiplied alpha, colour-as-grayscale, the pixel limit) stays a hard error under both
/// policies: this relaxes truncation, nothing else.
fn decode_raw<S: ConvSample>(
    data: &[u8],
    dst_is_gray_family: bool,
    data_format: JxlDataFormat,
    options: RawOptions,
) -> Result<(RawFrame, JxlPartialReport)> {
    // `JxlDecoderOptions` is `#[non_exhaustive]`, so it is built from its `Default` and then the
    // one public field we care about is set. The rest of the default stands deliberately: in
    // particular `progressive_mode` stays `Pass`, so a complete stream decodes identically whether
    // it arrived through the rejecting or the best-effort policy.
    let mut jxl_options = JxlDecoderOptions::default();
    jxl_options.pixel_limit = Some(PIXEL_LIMIT);

    let mut input: &[u8] = data;

    // Initialized -> WithImageInfo: parse the file/frame headers.
    let decoder = JxlRsDecoder::<Initialized>::new(jxl_options);
    let mut decoder = match decoder.process(&mut input).map_err(map_decode_error)? {
        ProcessingResult::Complete { result } => result,
        ProcessingResult::NeedsMoreInput { .. } => return Err(truncated()),
    };

    // Read everything we need from the image info, then drop the borrows before reconfiguring.
    let (size, num_extra, stream_is_gray, stream_has_alpha, premultiplied, stream_int_bits) = {
        let basic = decoder.basic_info();
        if basic.animation.is_some() {
            return Err(Error::unsupported(
                env!("CARGO_PKG_NAME"),
                "JXL: animated JPEG XL is not supported",
            ));
        }
        let alpha = basic
            .extra_channels
            .iter()
            .find(|c| c.ec_type == ExtraChannel::Alpha);
        let premultiplied = alpha.is_some_and(|c| c.alpha_associated);
        // The default pixel format's colour type reflects the stream's colour space (grayscale vs
        // RGB) and never carries alpha, so it is the cleanest signal of "is this a colour image?".
        let stream_is_gray = decoder.current_pixel_format().color_type.is_grayscale();
        // The declared integer precision (None for float streams, which keep full-range output).
        let stream_int_bits = match basic.bit_depth {
            JxlBitDepth::Int { bits_per_sample } => Some(bits_per_sample),
            JxlBitDepth::Float { .. } => None,
        };
        (
            basic.size,
            basic.extra_channels.len(),
            stream_is_gray,
            alpha.is_some(),
            premultiplied,
            stream_int_bits,
        )
    };

    // With the codestream-bit-depth policy on, an integer stream's declared depth replaces the
    // requested format's full-range depth (clamped to the output type's width), so samples keep
    // their coded `0 ..= 2^N - 1` range.
    let data_format = match (options.codestream_bit_depth, stream_int_bits, data_format) {
        (true, Some(bits), JxlDataFormat::U8 { .. }) => JxlDataFormat::U8 {
            bit_depth: bits.min(8) as u8,
        },
        (true, Some(bits), JxlDataFormat::U16 { endianness, .. }) => JxlDataFormat::U16 {
            endianness,
            bit_depth: bits.min(16) as u8,
        },
        (_, _, format) => format,
    };

    if premultiplied {
        return Err(Error::unsupported(
            env!("CARGO_PKG_NAME"),
            "JXL: premultiplied (associated) alpha is not supported",
        ));
    }
    if dst_is_gray_family && !stream_is_gray {
        return Err(Error::unsupported(
            env!("CARGO_PKG_NAME"),
            "JXL: cannot decode a color image as grayscale",
        ));
    }

    // jxl-rs reports usize dimensions; gamut carries u32. A stream claiming a dimension beyond u32
    // is malformed for gamut's buffers (and far past the pixel limit anyway).
    let (Ok(width), Ok(height)) = (u32::try_from(size.0), u32::try_from(size.1)) else {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "JXL: image dimensions overflow",
        ));
    };
    let dims = Dimensions::new(width, height)?;

    // Request the stream's natural colour layout; `convert_into` reshapes it to the caller's layout.
    let src_color = if stream_is_gray { 1 } else { 3 };
    let src_alpha = stream_has_alpha;
    let color_type = match (stream_is_gray, stream_has_alpha) {
        (true, false) => JxlColorType::Grayscale,
        (true, true) => JxlColorType::GrayscaleAlpha,
        (false, false) => JxlColorType::Rgb,
        (false, true) => JxlColorType::Rgba,
    };
    decoder.set_pixel_format(JxlPixelFormat {
        color_type,
        color_data_format: Some(data_format),
        // Every extra channel is ignored (`None`): alpha, when wanted, arrives interleaved via the
        // colour type above. The vector length must still match the stream's extra-channel count.
        extra_channel_format: vec![None; num_extra],
    });

    // Size the single interleaved output buffer, guarding the byte arithmetic against overflow.
    let src_channels = src_color + usize::from(src_alpha);
    let bytes_per_row = (width as usize)
        .checked_mul(src_channels)
        .and_then(|n| n.checked_mul(S::BYTES))
        .ok_or_else(|| {
            Error::invalid_input(env!("CARGO_PKG_NAME"), "JXL: image dimensions overflow")
        })?;
    let total = bytes_per_row.checked_mul(height as usize).ok_or_else(|| {
        Error::invalid_input(env!("CARGO_PKG_NAME"), "JXL: image dimensions overflow")
    })?;
    let (mut backing, offset) = aligned_backing(total, needs_row_alignment(S::BYTES));

    // WithImageInfo -> WithFrameInfo: parse the frame header.
    let decoder = match decoder.process(&mut input).map_err(map_decode_error)? {
        ProcessingResult::Complete { result } => result,
        ProcessingResult::NeedsMoreInput { size_hint, .. } => {
            if options.truncation == Truncation::Reject {
                return Err(truncated());
            }
            // No frame header yet, so there is nothing to draw: flushing here would re-enter
            // jxl-rs's header parser on empty input, hit its out-of-input error, and write
            // nothing. The buffer `aligned_backing` zeroed is the honest answer, and skipping
            // the call keeps the flush machinery out of a stage that cannot benefit from it.
            return Ok((
                RawFrame {
                    dims,
                    bytes: backing,
                    offset,
                    len: total,
                    src_color,
                    src_alpha,
                },
                JxlPartialReport {
                    render: JxlRender::HeaderOnly,
                    completed_passes: 0,
                    additional_bytes_hint: Some(saturating_u64(size_hint)),
                },
            ));
        }
    };

    // WithFrameInfo -> WithImageInfo: render pixels into our buffer.
    let report = {
        let buf = JxlOutputBuffer::new(
            &mut backing[offset..offset + total],
            height as usize,
            bytes_per_row,
        );
        let mut buffers = [buf];
        match decoder
            .process(&mut input, &mut buffers)
            .map_err(map_decode_error)?
        {
            ProcessingResult::Complete { .. } => JxlPartialReport::complete(),
            ProcessingResult::NeedsMoreInput {
                size_hint,
                mut fallback,
            } => {
                if options.truncation == Truncation::Reject {
                    return Err(truncated());
                }
                // Draw every group the stream did deliver. A failure here means jxl-rs refused
                // the render outright, which is a corrupt stream rather than a short one —
                // propagate it instead of laundering it into a valid-looking blank image.
                fallback
                    .flush_pixels(&mut buffers)
                    .map_err(map_decode_error)?;
                JxlPartialReport {
                    render: JxlRender::BestEffort,
                    // Read after the flush: it re-runs the section parser, which can complete a
                    // pass from data already buffered.
                    completed_passes: saturating_u32(fallback.num_completed_passes()),
                    additional_bytes_hint: Some(saturating_u64(size_hint)),
                }
            }
        }
    };

    Ok((
        RawFrame {
            dims,
            bytes: backing,
            offset,
            len: total,
            src_color,
            src_alpha,
        },
        report,
    ))
}

/// The error for a codestream that ends before a full image could be decoded. jxl-rs signals this as
/// `NeedsMoreInput`; since the decoder is fed the whole buffer at once, needing more means the input
/// was truncated.
fn truncated() -> Error {
    Error::invalid_input(env!("CARGO_PKG_NAME"), "JXL: truncated codestream")
}

/// Decodes `data` and reassembles it into typed samples plus the layout tag describing them, and
/// the report describing how complete the frame is.
///
/// The shared prologue of every entry point below: everything up to the point where
/// [`gamut_core::convert`] takes over.
fn decode_samples<P: Pixel>(
    data: &[u8],
    codestream_bit_depth: bool,
    policy: ConvertPolicy,
    truncation: Truncation,
) -> Result<(Vec<P::Sample>, PixelFormat, Dimensions, JxlPartialReport)>
where
    P::Sample: ConvSample,
{
    let (data_format, dst_is_gray_family, _, _) = output_layout(P::FORMAT)?;
    // A grayscale request against a colour stream can only be served by reducing colour to luma, so
    // the caller must have chosen the weights. Checked before decoding: the request jxl-rs is given
    // depends on the answer, and a refusal should not cost an entropy decode.
    let reduce_to_luma = dst_is_gray_family && policy.luma() != LumaPolicy::Reject;
    let (raw, report) = decode_raw::<P::Sample>(
        data,
        dst_is_gray_family && !reduce_to_luma,
        data_format,
        RawOptions {
            codestream_bit_depth,
            truncation,
        },
    )?;
    let format =
        native_format(raw.src_color, raw.src_alpha, P::Sample::BYTES).ok_or_else(|| {
            Error::unsupported(
                env!("CARGO_PKG_NAME"),
                "JXL: stream layout is not a gamut pixel format",
            )
        })?;
    Ok((
        reassemble::<P::Sample>(raw.samples()),
        format,
        raw.dims,
        report,
    ))
}

/// Decodes `data` into a fresh [`ImageBuf`] of layout `P`.
pub(crate) fn decode_to_buf<P: Pixel>(
    data: &[u8],
    codestream_bit_depth: bool,
    policy: ConvertPolicy,
) -> Result<ImageBuf<P>>
where
    P::Sample: ConvSample,
{
    let (samples, format, dims, _) =
        decode_samples::<P>(data, codestream_bit_depth, policy, Truncation::Reject)?;
    convert_from_raw(RawImage::new(&samples, format, dims)?, policy)
}

/// Decodes `data` best-effort into a fresh [`ImageBuf`] of layout `P`, tolerating truncation.
pub(crate) fn decode_partial_to_buf<P: Pixel>(
    data: &[u8],
    codestream_bit_depth: bool,
    policy: ConvertPolicy,
) -> Result<(ImageBuf<P>, JxlPartialReport)>
where
    P::Sample: ConvSample,
{
    let (samples, format, dims, report) =
        decode_samples::<P>(data, codestream_bit_depth, policy, Truncation::BestEffort)?;
    let image = convert_from_raw(RawImage::new(&samples, format, dims)?, policy)?;
    Ok((image, report))
}

/// Decodes `data` into `dst`, reusing its sample allocation when the decoded dimensions match.
pub(crate) fn decode_into_buf<P: Pixel>(
    data: &[u8],
    codestream_bit_depth: bool,
    policy: ConvertPolicy,
    dst: &mut ImageBuf<P>,
) -> Result<()>
where
    P::Sample: ConvSample,
{
    let (samples, format, dims, _) =
        decode_samples::<P>(data, codestream_bit_depth, policy, Truncation::Reject)?;
    let src = RawImage::new(&samples, format, dims)?;
    if dst.dimensions() == dims {
        // Same geometry: convert straight into the existing storage (its length is invariant).
        gamut_core::convert::convert_from_raw_into::<_, P>(src, policy, dst.as_mut_samples())
    } else {
        *dst = convert_from_raw(src, policy)?;
        Ok(())
    }
}

/// The native-endian 8-bit output format.
fn u8_format() -> JxlDataFormat {
    JxlDataFormat::U8 { bit_depth: 8 }
}

/// The native-endian 16-bit output format.
fn u16_format() -> JxlDataFormat {
    JxlDataFormat::U16 {
        endianness: Endianness::native(),
        bit_depth: 16,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_layout_matches_the_layout_table() {
        let (format, gray, color, alpha) = output_layout(PixelFormat::Gray8).unwrap();
        assert_eq!(format, u8_format());
        assert!(gray);
        assert_eq!(color, 1);
        assert!(!alpha);

        let (format, gray, color, alpha) = output_layout(PixelFormat::Rgba16).unwrap();
        assert_eq!(format, u16_format());
        assert!(!gray);
        assert_eq!(color, 3);
        assert!(alpha);

        let (_, gray, color, alpha) = output_layout(PixelFormat::GrayAlpha8).unwrap();
        assert!(gray);
        assert_eq!(color, 1);
        assert!(alpha);

        // A format outside the coded eight is refused rather than guessed at.
        assert!(matches!(
            output_layout(PixelFormat::Cmyk8),
            Err(error) if error.kind() == gamut_core::ErrorKind::Unsupported
        ));
    }

    #[test]
    fn needs_row_alignment_only_for_two_byte_samples() {
        // 16-bit (2-byte) output must be aligned; 8-bit (1-byte) must not force alignment. This pins
        // the decision that guards jxl-rs's alignment-sensitive interleaved-u16 fast path.
        assert!(!needs_row_alignment(1), "8-bit needs no alignment");
        assert!(needs_row_alignment(2), "16-bit needs 2-byte alignment");
    }

    #[test]
    fn aligned_backing_u8_is_tight_and_zero_offset() {
        let (buf, off) = aligned_backing(10, false);
        assert_eq!(buf.len(), 10);
        assert_eq!(off, 0);
    }

    #[test]
    fn aligned_backing_u16_starts_even() {
        let (buf, off) = aligned_backing(10, true);
        // One spare byte for the parity shift, and the used region begins on an even address.
        assert_eq!(buf.len(), 11);
        assert!(off <= 1);
        assert_eq!((buf.as_ptr() as usize + off) % 2, 0);
        assert!(off + 10 <= buf.len());
    }

    #[test]
    fn truncated_is_invalid_input() {
        assert_eq!(truncated().kind(), gamut_core::ErrorKind::InvalidInput);
    }

    #[test]
    fn embedded_icc_profile_errors_on_junk() {
        // The metadata accessor applies the same typed-error policy as pixel decoding.
        assert!(embedded_icc_profile(&[]).is_err());
        assert!(embedded_icc_profile(&[0x00, 0x01, 0x02, 0x03]).is_err());
        // A bare signature with nothing behind it is truncated, not panicking.
        assert!(embedded_icc_profile(&[0xFF, 0x0A]).is_err());
    }

    #[test]
    fn empty_and_garbage_input_error_without_panicking() {
        // Enough of the decode entry path to prove it returns typed errors on junk rather than
        // panicking; full corpus coverage lives in the robustness test unit.
        assert!(decode_to_buf::<gamut_core::Rgba8>(&[], false, ConvertPolicy::lossless()).is_err());
        assert!(
            decode_to_buf::<gamut_core::Rgb8>(
                &[0x00, 0x01, 0x02, 0x03],
                false,
                ConvertPolicy::lossless(),
            )
            .is_err()
        );
        assert!(info(&[]).is_err());
    }

    #[test]
    fn the_completeness_flag_tracks_the_render() {
        let complete = JxlPartialReport::complete();
        assert!(complete.is_complete());
        assert_eq!(complete.render, JxlRender::Complete);
        assert_eq!(complete.completed_passes, 0);
        assert_eq!(complete.additional_bytes_hint, None);

        for render in [JxlRender::HeaderOnly, JxlRender::BestEffort] {
            let partial = JxlPartialReport {
                render,
                completed_passes: 0,
                additional_bytes_hint: Some(1),
            };
            assert!(!partial.is_complete(), "{render:?} is not complete");
        }
    }

    #[test]
    fn the_diagnostic_narrowings_saturate_rather_than_wrap() {
        assert_eq!(saturating_u32(7), 7);
        assert_eq!(saturating_u64(7), 7);
        // A count wider than the field saturates; on a 32-bit host `usize::MAX` is exactly
        // `u32::MAX`, which is the saturation point either way.
        assert_eq!(saturating_u32(usize::MAX), u32::MAX);
        assert_eq!(saturating_u64(usize::MAX), usize::MAX as u64);
    }

    #[test]
    fn partial_decode_still_rejects_input_it_cannot_size_a_buffer_for() {
        // Truncation before the image headers has no dimensions to report, so the best-effort
        // policy is no more permissive than the rejecting one. Junk is likewise still an error.
        assert!(
            decode_partial_to_buf::<gamut_core::Rgba8>(&[], false, ConvertPolicy::lossless())
                .is_err()
        );
        assert!(
            decode_partial_to_buf::<gamut_core::Rgba8>(
                &[0xFF, 0x0A],
                false,
                ConvertPolicy::lossless()
            )
            .is_err()
        );
        assert!(
            decode_partial_to_buf::<gamut_core::Rgb8>(
                &[0x00, 0x01, 0x02, 0x03],
                false,
                ConvertPolicy::lossless()
            )
            .is_err()
        );
    }

    #[test]
    fn the_two_policies_differ_only_in_their_truncation_arm() {
        // The policy is a single field; nothing else about the decode configuration changes with
        // it, which is what lets a complete stream decode identically through both entry points.
        let reject = RawOptions {
            codestream_bit_depth: true,
            truncation: Truncation::Reject,
        };
        let best_effort = RawOptions {
            truncation: Truncation::BestEffort,
            ..reject
        };
        assert_ne!(reject, best_effort);
        assert_eq!(
            reject.codestream_bit_depth,
            best_effort.codestream_bit_depth
        );
    }
}
