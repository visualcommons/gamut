//! Input decoding: turn an image file into interleaved 8-bit RGB(A) for the gamut encoders.
//!
//! **PNG, JPEG, WebP, and JPEG XL are decoded by gamut's own decoders.** Only PPM still goes
//! through the third-party [`image`] crate, because gamut has no PPM decoder. Everything
//! downstream — the actual encode — is produced by the gamut crates regardless of input format.
//!
//! Asking every decoder for a fixed `Rgb8`/`Rgba8` buffer is a *lossy* request: a 16-bit PNG has to
//! narrow, a grayscale JPEG has to replicate, a transparent WebP asked for RGB has to drop its
//! alpha. gamut's decoders refuse that by default, which is the right default for a library and the
//! wrong one for this CLI — so every decoder here is given
//! [`ConvertPolicy::permissive`](gamut::core::convert::ConvertPolicy::permissive), the CLI opting
//! into the loss explicitly on the user's behalf.
//!
//! [`decode_native`] is the other door, for an encoder that takes more than one layout (AVIF): it
//! asks each decoder for the narrowest of five layouts that holds the file *without* loss — grey,
//! RGB or RGBA, at 8 or 16 bits — so the encoder sees the alpha and the precision the file
//! carries.
//!
//! [`read_metadata`] reads what the file carries besides its pixels, as the `gamut-metadata`
//! facade's blocks, for `gamut convert` to map into the output.

use std::path::Path;

use gamut::core::convert::{ConvertPolicy, convert};
use gamut::core::{
    DecodeImage, Dimensions, ErrorKind, Gray8, ImageBuf, Pixel, Rgb8, Rgb16, Rgba8, Rgba16,
};
use gamut::jpeg::JpegDecoder;
use gamut::jxl::JxlDecoder;
use gamut::metadata::{MetadataBlock, TypedMetadata};
use gamut::png::{ColorType as PngColorType, PngDecoder, PngMetadata};
use gamut::webp::WebpDecoder;

use crate::error::CliError;
use crate::sniff::{Container, sniff};

/// A decoded image in the narrowest layout that holds the input without loss.
///
/// The five layouts are the ones `AvifEncoder` codes: a grey image with 16-bit samples has no
/// `Gray16` encode to go to, so it widens (losslessly) to [`Rgb16`].
pub(crate) enum NativeImage {
    /// 8-bit greyscale without alpha.
    Gray8(ImageBuf<Gray8>),
    /// 8-bit colour without alpha (or a palette without transparency).
    Rgb8(ImageBuf<Rgb8>),
    /// 8-bit samples with alpha, grey or colour.
    Rgba8(ImageBuf<Rgba8>),
    /// Samples wider than 8 bits, without alpha.
    Rgb16(ImageBuf<Rgb16>),
    /// Samples wider than 8 bits, with alpha.
    Rgba16(ImageBuf<Rgba16>),
}

impl NativeImage {
    /// The image's dimensions.
    pub(crate) fn dimensions(&self) -> Dimensions {
        match self {
            Self::Gray8(img) => img.dimensions(),
            Self::Rgb8(img) => img.dimensions(),
            Self::Rgba8(img) => img.dimensions(),
            Self::Rgb16(img) => img.dimensions(),
            Self::Rgba16(img) => img.dimensions(),
        }
    }

    /// The size of the decoded sample buffer in bytes.
    pub(crate) fn byte_len(&self) -> usize {
        match self {
            Self::Gray8(img) => img.as_samples().len(),
            Self::Rgb8(img) => img.as_samples().len(),
            Self::Rgba8(img) => img.as_samples().len(),
            Self::Rgb16(img) => img.as_samples().len() * 2,
            Self::Rgba16(img) => img.as_samples().len() * 2,
        }
    }
}

/// The metadata an input file carries, located as `gamut-metadata` [`MetadataBlock`]s and held
/// verbatim, plus the facts about the decode a conversion needs to keep it truthful.
#[derive(Default)]
pub(crate) struct InputMetadata {
    /// The Exif payload, as the reader located it (with or without the `Exif\0\0` marker).
    pub(crate) exif: Option<Vec<u8>>,
    /// The XMP packet.
    pub(crate) xmp: Option<Vec<u8>>,
    /// The embedded ICC profile — absent when the decoder renders the pixels into another colour
    /// space rather than into it (see [`decoded_to_srgb`](Self::decoded_to_srgb)).
    pub(crate) icc: Option<Vec<u8>>,
    /// A legacy IPTC-IIM dataset stream.
    pub(crate) iptc_iim: Option<Vec<u8>>,
    /// Whether the file holds a C2PA manifest store. Only its presence is kept: a store is never
    /// carried into a rewritten file.
    pub(crate) c2pa: bool,
    /// Blocks of a kind this command does not know, by count: a carrier `gamut-metadata` added
    /// after this command was written.
    pub(crate) unknown: usize,
    /// Whether the decoder already applied the file's orientation to the pixels (JPEG XL does), so
    /// the decoded pixels are display-oriented.
    pub(crate) oriented: bool,
    /// Whether the decoder rendered the pixels into sRGB rather than into the file's own colour
    /// encoding (an XYB-coded JPEG XL), so a profile the file embeds describes the original image
    /// and not these pixels.
    pub(crate) decoded_to_srgb: bool,
    /// A PNG input's whole metadata, for a PNG output: the text annotations and colour chunks
    /// have no `gamut-metadata` block, so only a PNG output holds them.
    pub(crate) png: Option<PngMetadata>,
}

impl InputMetadata {
    /// Owns the payloads `blocks` borrow, one field per carrier.
    fn from_blocks(blocks: &[MetadataBlock<'_>]) -> Self {
        let mut meta = Self::default();
        for block in blocks {
            match *block {
                MetadataBlock::Exif(bytes) => meta.exif = Some(bytes.to_vec()),
                MetadataBlock::Xmp(bytes) => meta.xmp = Some(bytes.to_vec()),
                MetadataBlock::Icc(bytes) => meta.icc = Some(bytes.to_vec()),
                MetadataBlock::IptcIim(bytes) => meta.iptc_iim = Some(bytes.to_vec()),
                MetadataBlock::C2pa(_) => meta.c2pa = true,
                // `MetadataBlock` is `#[non_exhaustive]`: a later carrier is counted, so the
                // conversion reports it as not carried rather than passing over it in silence.
                _ => meta.unknown += 1,
            }
        }
        meta
    }

    /// The blocks a [`TypedMetadata`] reader hands over; none when it cannot locate them.
    fn from_typed(typed: &dyn TypedMetadata) -> Self {
        typed
            .blocks()
            .map(|blocks| Self::from_blocks(&blocks))
            .unwrap_or_default()
    }
}

/// Reads the metadata a supported input file carries, as the [`TypedMetadata`] blocks of each
/// format's reader (PNG, JPEG, WebP, JPEG XL). PPM carries none.
///
/// Best-effort and total: the pixels decode separately, so a payload that cannot be read is simply
/// absent rather than failing the conversion.
pub(crate) fn read_metadata(bytes: &[u8]) -> InputMetadata {
    match sniff(bytes) {
        Some(Container::Webp) => {
            let Ok(raw) = WebpDecoder::new().metadata(bytes) else {
                return InputMetadata::default();
            };
            let mut meta = InputMetadata::from_typed(&raw);
            // `WebpDecoder::metadata` reads the three carriers, which hold no manifest store; the
            // store is the `C2PA` chunk's payload, the span `c2pa_span` reports less its 8-byte
            // chunk header.
            meta.c2pa = gamut::webp::c2pa_span(bytes)
                .ok()
                .flatten()
                .and_then(|span| {
                    let start = usize::try_from(span.start).ok()?.checked_add(8)?;
                    bytes.get(start..usize::try_from(span.end).ok()?)
                })
                .is_some();
            meta
        }
        Some(Container::Jxl) => {
            let decoder = JxlDecoder::new();
            let mut meta = decoder
                .metadata(bytes)
                .map(|m| InputMetadata::from_typed(&m))
                .unwrap_or_default();
            // The decoder orients the pixels for display (the `jxl` crate's `adjust_orientation`
            // default), and an XYB-coded stream decodes to sRGB, not into its embedded profile.
            meta.oriented = true;
            if decoder
                .info(bytes)
                .is_ok_and(|info| !info.uses_original_profile)
            {
                meta.decoded_to_srgb = true;
                meta.icc = None;
            }
            meta
        }
        Some(Container::Png) => gamut::png::metadata(bytes)
            .map(|m| {
                let mut meta = InputMetadata::from_typed(&m);
                meta.png = Some(m);
                meta
            })
            .unwrap_or_default(),
        Some(Container::Jpeg) => JpegDecoder::new()
            .metadata(bytes)
            .map(|m| InputMetadata::from_typed(&m))
            .unwrap_or_default(),
        Some(Container::Isobmff) | None => InputMetadata::default(),
    }
}

/// Which of [`NativeImage`]'s layouts holds an image without loss.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Layout {
    Gray8,
    Rgb8,
    Rgba8,
    Rgb16,
    Rgba16,
}

impl Layout {
    /// The narrowest layout for an image that is grey or colour, has alpha or not, and has samples
    /// wider than 8 bits or not. Grey with alpha or wide grey has no grey layout to go to, so it
    /// takes the RGB(A) one.
    fn pick(grey: bool, alpha: bool, wide: bool) -> Self {
        match (alpha, wide) {
            (false, false) if grey => Self::Gray8,
            (false, false) => Self::Rgb8,
            (true, false) => Self::Rgba8,
            (false, true) => Self::Rgb16,
            (true, true) => Self::Rgba16,
        }
    }
}

/// Decodes `decoder`'s view of `bytes` in `layout`; only decoders implementing all five layouts
/// can take it.
fn decode_as<D>(decoder: &D, bytes: &[u8], layout: Layout) -> Result<NativeImage, CliError>
where
    D: DecodeImage<Gray8>
        + DecodeImage<Rgb8>
        + DecodeImage<Rgba8>
        + DecodeImage<Rgb16>
        + DecodeImage<Rgba16>,
{
    Ok(match layout {
        Layout::Gray8 => NativeImage::Gray8(decoder.decode_image(bytes)?),
        Layout::Rgb8 => NativeImage::Rgb8(decoder.decode_image(bytes)?),
        Layout::Rgba8 => NativeImage::Rgba8(decoder.decode_image(bytes)?),
        Layout::Rgb16 => NativeImage::Rgb16(decoder.decode_image(bytes)?),
        Layout::Rgba16 => NativeImage::Rgba16(decoder.decode_image(bytes)?),
    })
}

/// Decodes a supported image file (PNG, JPEG, PPM/P6, WebP, or JPEG XL) in the narrowest layout
/// that holds it without loss.
///
/// What decides the layout, per format:
///
/// - **PNG**: the colour type and bit depth, with a `tRNS` chunk (palette alpha or a colour key)
///   counting as alpha. Learning about `tRNS` takes a full decode, so a PNG is decoded twice.
/// - **JPEG**: one component is grey, anything else colour; always 8-bit, never alpha.
/// - **WebP**: always 8-bit colour; alpha when the decoder reports the file carries any, which it
///   decides from the pixels of a lossless file — so a transparent WebP is decoded twice.
/// - **JPEG XL**: the image header's channel count, alpha flag and bits per sample (a float stream
///   is wide).
/// - **PPM** (through the `image` crate): its colour type.
///
/// `bytes` is the file already read (see [`read_input`]); `path` only labels errors.
pub(crate) fn decode_native(path: &Path, bytes: &[u8]) -> Result<NativeImage, CliError> {
    // The layout is chosen so no conversion loses anything; the permissive policy only matters
    // where no layout can (a float JPEG XL narrowing to 16 bits), as on the fixed-layout path.
    let lossy = ConvertPolicy::permissive();

    match sniff(bytes) {
        Some(Container::Webp) => {
            // Ask for RGB under the *lossless* policy: the decoder refuses it exactly when the
            // file carries alpha, and only then is RGBA decoded.
            let image = match WebpDecoder::new().decode_image(bytes) {
                Ok(rgb) => NativeImage::Rgb8(rgb),
                Err(err) if err.kind() == ErrorKind::Unsupported => NativeImage::Rgba8(
                    WebpDecoder::new()
                        .with_convert_policy(lossy)
                        .decode_image(bytes)?,
                ),
                Err(err) => return Err(err.into()),
            };
            return Ok(image);
        }
        Some(Container::Jxl) => {
            let decoder = JxlDecoder::new().with_convert_policy(lossy);
            let info = decoder.info(bytes)?;
            let grey = info.color_channels == 1;
            let layout = Layout::pick(
                grey,
                info.has_alpha,
                info.is_float || info.bits_per_sample > 8,
            );
            return decode_as(&decoder, bytes, layout);
        }
        Some(Container::Png) => {
            let decoder = PngDecoder::new().with_convert_policy(lossy);
            let decoded = decoder.decode(bytes)?;
            let color = decoded.header.color_type;
            let alpha = matches!(
                color,
                PngColorType::GrayscaleAlpha | PngColorType::TruecolorAlpha
            ) || decoded.transparency.is_some()
                || decoded
                    .palette
                    .as_ref()
                    .is_some_and(|palette| palette.has_transparency());
            let grey = matches!(
                color,
                PngColorType::Grayscale | PngColorType::GrayscaleAlpha
            );
            let layout = Layout::pick(grey, alpha, decoded.header.bit_depth == 16);
            return decode_as(&decoder, bytes, layout);
        }
        Some(Container::Jpeg) => {
            let decoder = JpegDecoder::new().with_convert_policy(lossy);
            let image = if decoder.info(bytes)?.components == 1 {
                NativeImage::Gray8(decoder.decode_image(bytes)?)
            } else {
                NativeImage::Rgb8(decoder.decode_image(bytes)?)
            };
            return Ok(image);
        }
        Some(Container::Isobmff) | None => {}
    }

    let decoded = image::load_from_memory(bytes).map_err(|source| CliError::Decode {
        path: path.to_path_buf(),
        source,
    })?;
    let color = decoded.color();
    let channel_bits = color.bits_per_pixel() / u16::from(color.channel_count());
    let dims = Dimensions::new(decoded.width(), decoded.height())?;
    let image = match Layout::pick(!color.has_color(), color.has_alpha(), channel_bits > 8) {
        Layout::Gray8 => NativeImage::Gray8(ImageBuf::new(decoded.to_luma8().into_raw(), dims)?),
        Layout::Rgb8 => NativeImage::Rgb8(ImageBuf::new(decoded.to_rgb8().into_raw(), dims)?),
        Layout::Rgba8 => NativeImage::Rgba8(ImageBuf::new(decoded.to_rgba8().into_raw(), dims)?),
        Layout::Rgb16 => NativeImage::Rgb16(ImageBuf::new(decoded.to_rgb16().into_raw(), dims)?),
        Layout::Rgba16 => NativeImage::Rgba16(ImageBuf::new(decoded.to_rgba16().into_raw(), dims)?),
    };
    Ok(image)
}

/// The interleaved 8-bit layout a fixed-layout encoder takes, which [`decode_bytes`] decodes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Interleaved {
    /// Three samples per pixel; any alpha the input carries is dropped.
    Rgb8,
    /// Four samples per pixel; fully opaque when the input carries no alpha.
    Rgba8,
}

/// A decoded image as the flat `(samples, dimensions)` pair the encoders take.
fn flat<P: Pixel>(image: ImageBuf<P>) -> (Vec<P::Sample>, Dimensions) {
    let dims = image.dimensions();
    (image.into_samples(), dims)
}

/// Decodes `decoder`'s view of `bytes` in the `target` layout; only decoders implementing both
/// can take it.
fn decode_interleaved<D>(
    decoder: &D,
    bytes: &[u8],
    target: Interleaved,
) -> Result<(Vec<u8>, Dimensions), CliError>
where
    D: DecodeImage<Rgb8> + DecodeImage<Rgba8>,
{
    Ok(match target {
        Interleaved::Rgb8 => flat::<Rgb8>(decoder.decode_image(bytes)?),
        Interleaved::Rgba8 => flat::<Rgba8>(decoder.decode_image(bytes)?),
    })
}

/// Decodes a supported image file (PNG, JPEG, PPM/P6, WebP, or JPEG XL) into interleaved 8-bit RGB.
///
/// Returns the pixel buffer (`width * height * 3` bytes, row-major, no padding) and its
/// dimensions. Alpha is dropped and grayscale is expanded so the buffer is always 3 bytes per
/// pixel, matching the gamut encoders' input contract. The format is detected from the file
/// contents, so the extension need not be accurate.
pub(crate) fn decode_rgb8(path: &Path) -> Result<(Vec<u8>, Dimensions), CliError> {
    let bytes = std::fs::read(path).map_err(|source| CliError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    decode_bytes(path, &bytes, Interleaved::Rgb8)
}

/// Reads an input file, or all of standard input when `path` is [`STDIO`] (`-`).
pub(crate) fn read_input(path: &Path) -> Result<Vec<u8>, CliError> {
    let io_error = |source| CliError::Io {
        path: path.to_path_buf(),
        source,
    };
    if path == Path::new(STDIO) {
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut std::io::stdin().lock(), &mut bytes).map_err(io_error)?;
        Ok(bytes)
    } else {
        std::fs::read(path).map_err(io_error)
    }
}

/// The path that names standard input (as an input) or standard output (as an output).
pub(crate) const STDIO: &str = "-";

/// Decodes an already-read image (PNG, JPEG, PPM/P6, WebP, or JPEG XL) to the interleaved
/// `target` layout.
///
/// Format-dispatching core: routes every format gamut can decode to gamut's own decoder, and the
/// remainder (PPM) to the `image` crate. The format is detected from the contents, never from
/// `path`, which only labels errors.
pub(crate) fn decode_bytes(
    path: &Path,
    bytes: &[u8],
    target: Interleaved,
) -> Result<(Vec<u8>, Dimensions), CliError> {
    let lossy = ConvertPolicy::permissive();

    match sniff(bytes) {
        Some(Container::Webp) => {
            return decode_interleaved(
                &WebpDecoder::new().with_convert_policy(lossy),
                bytes,
                target,
            );
        }
        Some(Container::Jxl) => {
            return decode_interleaved(
                &JxlDecoder::new().with_convert_policy(lossy),
                bytes,
                target,
            );
        }
        Some(Container::Png) => {
            return decode_interleaved(
                &PngDecoder::new().with_convert_policy(lossy),
                bytes,
                target,
            );
        }
        Some(Container::Jpeg) => {
            // A JPEG never carries alpha, so gamut-jpeg implements no `DecodeImage<Rgba8>`. Decode
            // the colour it does carry, then let the same conversion engine add the opaque alpha
            // channel rather than padding it by hand here.
            let img: ImageBuf<Rgb8> = JpegDecoder::new()
                .with_convert_policy(lossy)
                .decode_image(bytes)?;
            return Ok(match target {
                Interleaved::Rgb8 => flat(img),
                Interleaved::Rgba8 => flat::<Rgba8>(convert(img.as_ref(), lossy)?),
            });
        }
        Some(Container::Isobmff) | None => {}
    }

    let decoded = image::load_from_memory(bytes).map_err(|source| CliError::Decode {
        path: path.to_path_buf(),
        source,
    })?;
    let (buf, width, height) = match target {
        Interleaved::Rgb8 => {
            let rgb = decoded.to_rgb8();
            let (w, h) = rgb.dimensions();
            (rgb.into_raw(), w, h)
        }
        Interleaved::Rgba8 => {
            let rgba = decoded.to_rgba8();
            let (w, h) = rgba.dimensions();
            (rgba.into_raw(), w, h)
        }
    };
    Ok((buf, Dimensions::new(width, height)?))
}

#[cfg(test)]
mod tests {
    use gamut::core::{EncodeImage, ImageRef};
    use gamut::jxl::JxlEncoder;
    use gamut::webp::WebpEncoder;

    use super::*;

    /// Encodes `rgba` as a lossless (so bit-exact) WebP file for the round-trip tests.
    fn lossless_webp(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        WebpEncoder::lossless()
            .encode_image(
                ImageRef::<Rgba8>::new(rgba, Dimensions::new(width, height).unwrap()).unwrap(),
                &mut out,
            )
            .expect("encode webp");
        out
    }

    /// Encodes `rgba` as a lossless (so bit-exact) JPEG XL codestream for the round-trip tests.
    fn lossless_jxl(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        JxlEncoder::lossless()
            .encode_image(
                ImageRef::<Rgba8>::new(rgba, Dimensions::new(width, height).unwrap()).unwrap(),
                &mut out,
            )
            .expect("encode jxl");
        out
    }

    #[test]
    fn sniffs_and_decodes_opaque_webp_to_rgb8() {
        let rgba = [
            0x10, 0x20, 0x30, 0xff, // px (0,0)
            0x40, 0x50, 0x60, 0xff, // px (1,0)
            0x70, 0x80, 0x90, 0xff, // px (0,1)
            0xa0, 0xb0, 0xc0, 0xff, // px (1,1)
        ];
        let webp = lossless_webp(2, 2, &rgba);
        assert_eq!(sniff(&webp), Some(Container::Webp));
        let (rgb, dims) = decode_bytes(Path::new("mem.webp"), &webp, Interleaved::Rgb8).unwrap();
        assert_eq!(dims, Dimensions::new(2, 2).unwrap());
        // Lossless: RGB survives exactly; alpha is dropped.
        assert_eq!(
            rgb,
            [
                0x10, 0x20, 0x30, 0x40, 0x50, 0x60, 0x70, 0x80, 0x90, 0xa0, 0xb0, 0xc0
            ]
        );
    }

    #[test]
    fn decodes_transparent_webp_to_rgba8() {
        // One pixel per row; alpha ranges from fully transparent to fully opaque.
        #[rustfmt::skip]
        let rgba = [
            0x11, 0x22, 0x33, 0x00,
            0x44, 0x55, 0x66, 0x80,
            0x77, 0x88, 0x99, 0xc0,
            0xaa, 0xbb, 0xcc, 0xff,
        ];
        let webp = lossless_webp(2, 2, &rgba);
        let (out, dims) = decode_bytes(Path::new("mem.webp"), &webp, Interleaved::Rgba8).unwrap();
        assert_eq!(dims, Dimensions::new(2, 2).unwrap());
        // Lossless VP8L carries alpha natively, so the round-trip is bit-exact.
        assert_eq!(out, rgba);
    }

    #[test]
    fn sniffs_and_decodes_opaque_jxl_to_rgb8() {
        let rgba = [
            0x10, 0x20, 0x30, 0xff, // px (0,0)
            0x40, 0x50, 0x60, 0xff, // px (1,0)
            0x70, 0x80, 0x90, 0xff, // px (0,1)
            0xa0, 0xb0, 0xc0, 0xff, // px (1,1)
        ];
        let jxl = lossless_jxl(2, 2, &rgba);
        // A bare codestream starts with the 2-byte JXL signature.
        assert_eq!(&jxl[0..2], &[0xFF, 0x0A]);
        assert_eq!(sniff(&jxl), Some(Container::Jxl));
        let (rgb, dims) = decode_bytes(Path::new("mem.jxl"), &jxl, Interleaved::Rgb8).unwrap();
        assert_eq!(dims, Dimensions::new(2, 2).unwrap());
        // Lossless: RGB survives exactly; alpha is dropped.
        assert_eq!(
            rgb,
            [
                0x10, 0x20, 0x30, 0x40, 0x50, 0x60, 0x70, 0x80, 0x90, 0xa0, 0xb0, 0xc0
            ]
        );
    }

    #[test]
    fn decodes_transparent_jxl_to_rgba8() {
        // One pixel per row; alpha ranges from fully transparent to fully opaque.
        #[rustfmt::skip]
        let rgba = [
            0x11, 0x22, 0x33, 0x00,
            0x44, 0x55, 0x66, 0x80,
            0x77, 0x88, 0x99, 0xc0,
            0xaa, 0xbb, 0xcc, 0xff,
        ];
        let jxl = lossless_jxl(2, 2, &rgba);
        assert_eq!(sniff(&jxl), Some(Container::Jxl));
        let (out, dims) = decode_bytes(Path::new("mem.jxl"), &jxl, Interleaved::Rgba8).unwrap();
        assert_eq!(dims, Dimensions::new(2, 2).unwrap());
        // Lossless JPEG XL carries alpha natively, so the round-trip is bit-exact.
        assert_eq!(out, rgba);
    }

    #[test]
    fn sixteen_bit_grayscale_png_is_decoded_by_gamut() {
        use gamut::core::Gray16;
        use gamut::png::PngEncoder;

        // Two things gamut-png refuses by default happen here at once: 16-bit samples narrow to 8,
        // and a single grey channel replicates into three. The decode therefore succeeds only
        // because this module opts into the loss -- and only because the PNG path now goes through
        // gamut-png at all instead of detouring through the `image` crate.
        let dims = Dimensions::new(4, 1).unwrap();
        let gray16: [u16; 4] = [0, 0x8080, 0xFFFF, 0x0101];
        let mut png = Vec::new();
        PngEncoder::new()
            .encode_image(ImageRef::<Gray16>::new(&gray16, dims).unwrap(), &mut png)
            .expect("encode png");
        assert_eq!(sniff(&png), Some(Container::Png));

        let (rgb, got) = decode_bytes(Path::new("mem.png"), &png, Interleaved::Rgb8).unwrap();
        assert_eq!(got, dims);
        // 0x8080 -> 128 and 0x0101 -> 1 are the round-to-nearest narrowing; a truncating `>> 8`
        // would give 255 -> 255 but 0x8080 -> 128 and 0xFFFF -> 255 only by luck, so the endpoints
        // plus 0x0101 pin the scale.
        assert_eq!(rgb, [0, 0, 0, 128, 128, 128, 255, 255, 255, 1, 1, 1]);
    }

    #[test]
    fn grayscale_jpeg_is_decoded_by_gamut_and_gains_opaque_alpha() {
        use gamut::core::Gray8;
        use gamut::jpeg::JpegEncoder;

        // gamut-jpeg has no `DecodeImage<Rgba8>` (a JPEG carries no alpha), so this exercises the
        // decode-then-widen path: grey replicates into RGB and the shared engine appends opaque
        // alpha.
        let dims = Dimensions::new(8, 8).unwrap();
        let gray = vec![0x40u8; 64];
        let mut jpeg = Vec::new();
        JpegEncoder::new()
            .encode_image(ImageRef::<Gray8>::new(&gray, dims).unwrap(), &mut jpeg)
            .expect("encode jpeg");
        assert_eq!(sniff(&jpeg), Some(Container::Jpeg));

        let (rgba, got) = decode_bytes(Path::new("mem.jpg"), &jpeg, Interleaved::Rgba8).unwrap();
        assert_eq!(got, dims);
        assert_eq!(rgba.len(), 64 * 4);
        for px in rgba.as_chunks::<4>().0 {
            // Lossy JPEG, so the grey level is approximate -- but it must stay grey (R == G == B)
            // and every pixel must be fully opaque.
            assert_eq!(px[0], px[1]);
            assert_eq!(px[1], px[2]);
            assert_eq!(px[3], 0xff);
        }
    }

    /// The layout `decode_native` chose for `bytes`.
    fn native_layout(bytes: &[u8]) -> Layout {
        match decode_native(Path::new("mem"), bytes).unwrap() {
            NativeImage::Gray8(_) => Layout::Gray8,
            NativeImage::Rgb8(_) => Layout::Rgb8,
            NativeImage::Rgba8(_) => Layout::Rgba8,
            NativeImage::Rgb16(_) => Layout::Rgb16,
            NativeImage::Rgba16(_) => Layout::Rgba16,
        }
    }

    #[test]
    fn layout_pick_is_the_narrowest_lossless_layout() {
        assert_eq!(Layout::pick(true, false, false), Layout::Gray8);
        assert_eq!(Layout::pick(false, false, false), Layout::Rgb8);
        assert_eq!(Layout::pick(true, true, false), Layout::Rgba8);
        assert_eq!(Layout::pick(false, true, false), Layout::Rgba8);
        assert_eq!(Layout::pick(true, false, true), Layout::Rgb16);
        assert_eq!(Layout::pick(false, true, true), Layout::Rgba16);
    }

    #[test]
    fn native_webp_keeps_alpha_only_when_the_file_has_it() {
        let opaque = [
            1, 2, 3, 0xff, 4, 5, 6, 0xff, 7, 8, 9, 0xff, 10, 11, 12, 0xff,
        ];
        let transparent = [
            1, 2, 3, 0x00, 4, 5, 6, 0x80, 7, 8, 9, 0xff, 10, 11, 12, 0xff,
        ];
        assert_eq!(native_layout(&lossless_webp(2, 2, &opaque)), Layout::Rgb8);
        assert_eq!(
            native_layout(&lossless_webp(2, 2, &transparent)),
            Layout::Rgba8
        );
    }

    #[test]
    fn native_jxl_keeps_alpha() {
        let rgba = [
            1, 2, 3, 0x00, 4, 5, 6, 0x80, 7, 8, 9, 0xff, 10, 11, 12, 0xff,
        ];
        assert_eq!(native_layout(&lossless_jxl(2, 2, &rgba)), Layout::Rgba8);
    }

    #[test]
    fn native_grey_jpeg_stays_grey() {
        use gamut::jpeg::JpegEncoder;

        let dims = Dimensions::new(8, 8).unwrap();
        let mut jpeg = Vec::new();
        JpegEncoder::new()
            .encode_image(
                ImageRef::<Gray8>::new(&[0x40; 64], dims).unwrap(),
                &mut jpeg,
            )
            .unwrap();
        assert_eq!(native_layout(&jpeg), Layout::Gray8);
    }

    #[test]
    fn native_png_counts_a_trns_palette_as_alpha() {
        use gamut::core::Indexed8;
        use gamut::png::{Palette, PngEncoder};

        let dims = Dimensions::new(2, 1).unwrap();
        let encode = |palette: Palette| {
            let mut png = Vec::new();
            PngEncoder::new()
                .encode_indexed8(
                    ImageRef::<Indexed8>::new(&[0, 1], dims).unwrap(),
                    &palette,
                    &mut png,
                )
                .unwrap();
            png
        };
        let colours = [[0, 0, 0], [255, 255, 255]];
        let opaque = Palette::new(&colours).unwrap();
        let keyed = Palette::with_alpha(&colours, &[0]).unwrap();
        assert_eq!(native_layout(&encode(opaque)), Layout::Rgb8);
        assert_eq!(native_layout(&encode(keyed)), Layout::Rgba8);
    }

    #[test]
    fn native_sixteen_bit_grey_png_widens_to_rgb16() {
        use gamut::core::Gray16;
        use gamut::png::PngEncoder;

        let dims = Dimensions::new(2, 1).unwrap();
        let png = PngEncoder::new()
            .encode_to_vec(ImageRef::<Gray16>::new(&[0, 0x8080], dims).unwrap())
            .unwrap();
        assert_eq!(native_layout(&png), Layout::Rgb16);
    }

    /// Every carrier a block can name lands in its own field, and a C2PA store only as its
    /// presence.
    #[test]
    fn from_blocks_keeps_each_carrier_in_its_field() {
        let meta = InputMetadata::from_blocks(&[
            MetadataBlock::Exif(b"exif"),
            MetadataBlock::Xmp(b"xmp"),
            MetadataBlock::Icc(b"icc"),
            MetadataBlock::IptcIim(b"iim"),
            MetadataBlock::C2pa(b"jumb"),
        ]);
        assert_eq!(meta.exif.as_deref(), Some(&b"exif"[..]));
        assert_eq!(meta.xmp.as_deref(), Some(&b"xmp"[..]));
        assert_eq!(meta.icc.as_deref(), Some(&b"icc"[..]));
        assert_eq!(meta.iptc_iim.as_deref(), Some(&b"iim"[..]));
        assert!(meta.c2pa);
        assert_eq!(meta.unknown, 0);
        assert!(!meta.oriented && !meta.decoded_to_srgb);
    }

    /// A WebP's and a JPEG's carriers are read through their `TypedMetadata` impls, and a WebP's
    /// `C2PA` chunk as its presence.
    #[test]
    fn read_metadata_reads_webp_and_jpeg_blocks() {
        use gamut::jpeg::JpegEncoder;

        let rgba = [
            1, 2, 3, 0xff, 4, 5, 6, 0xff, 7, 8, 9, 0xff, 10, 11, 12, 0xff,
        ];
        let mut webp = Vec::new();
        WebpEncoder::lossless()
            .with_exif(b"MM\0*webp")
            .with_xmp(b"<x/>")
            .with_c2pa(b"jumb")
            .encode_image(
                ImageRef::<Rgba8>::new(&rgba, Dimensions::new(2, 2).unwrap()).unwrap(),
                &mut webp,
            )
            .unwrap();
        let meta = read_metadata(&webp);
        assert_eq!(meta.exif.as_deref(), Some(&b"MM\0*webp"[..]));
        assert_eq!(meta.xmp.as_deref(), Some(&b"<x/>"[..]));
        assert!(meta.c2pa);

        let mut jpeg = Vec::new();
        JpegEncoder::new()
            .with_xmp(b"<y/>")
            .encode_image(
                ImageRef::<Gray8>::new(&[0x40; 64], Dimensions::new(8, 8).unwrap()).unwrap(),
                &mut jpeg,
            )
            .unwrap();
        let meta = read_metadata(&jpeg);
        assert_eq!(meta.xmp.as_deref(), Some(&b"<y/>"[..]));
        assert!(meta.png.is_none());
    }

    /// The ICC profile `read_metadata` kept for `bytes`.
    fn native_icc(bytes: &[u8]) -> Option<Vec<u8>> {
        read_metadata(bytes).icc
    }

    #[test]
    fn native_jxl_carries_its_icc_only_when_pixels_decode_into_it() {
        use gamut::icc::{BuiltinProfile, Curve, IccProfile, KnownTag, TagData};
        use gamut::jxl::Distance;

        // libjxl re-signals a profile it can express as a structured encoding (one with a `cicp`
        // tag, or a matrix/TRC profile with an analytic curve) instead of embedding it, so the
        // `cicp` tag goes and the TRCs become a sampled smoothstep (3x^2 - 2x^3), which no gamma
        // or standard transfer function fits.
        let mut profile = IccProfile::builtin(BuiltinProfile::DisplayP3).unwrap();
        profile
            .tags
            .retain(|tag| tag.signature != KnownTag::Cicp.into());
        let table: Vec<u16> = (0..=16u32)
            .map(|i| {
                let x = f64::from(i) / 16.0;
                ((3.0 * x * x - 2.0 * x * x * x) * 65535.0).round() as u16
            })
            .collect();
        for tag in &mut profile.tags {
            if [KnownTag::RedTrc, KnownTag::GreenTrc, KnownTag::BlueTrc]
                .into_iter()
                .any(|trc| tag.signature == trc.into())
            {
                tag.data = TagData::Curve(Curve::Sampled(table.clone()));
            }
        }
        let icc = profile.to_bytes().unwrap();
        let rgb: Vec<u8> = (0..8 * 8 * 3).map(|i| (i * 5) as u8).collect();
        let encode = |encoder: JxlEncoder| {
            let mut out = Vec::new();
            encoder
                .with_icc_profile(&icc)
                .encode_image(
                    ImageRef::<Rgb8>::new(&rgb, Dimensions::new(8, 8).unwrap()).unwrap(),
                    &mut out,
                )
                .unwrap();
            out
        };
        let lossless = encode(JxlEncoder::lossless());
        let xyb = encode(JxlEncoder::lossy(Distance::new(1.0).unwrap()));
        // Both streams embed the profile; only the XYB one decodes to sRGB instead of into it.
        assert!(
            JxlDecoder::new()
                .embedded_icc_profile(&xyb)
                .unwrap()
                .is_some()
        );
        assert_eq!(native_icc(&lossless), Some(icc));
        assert_eq!(native_icc(&xyb), None);
        assert!(!read_metadata(&lossless).decoded_to_srgb);
        let xyb = read_metadata(&xyb);
        assert!(xyb.decoded_to_srgb);
        assert!(xyb.oriented, "the JPEG XL decoder orients its pixels");
    }

    #[test]
    fn non_riff_bytes_take_the_image_path() {
        // Not a RIFF/WebP container, so it must NOT be routed to the webp decoder; the image path
        // rejects it with a decode error rather than panicking.
        let junk = [0u8; 32];
        assert_eq!(sniff(&junk), None);
        let err = decode_bytes(Path::new("junk.bin"), &junk, Interleaved::Rgb8).unwrap_err();
        assert!(matches!(err, CliError::Decode { .. }));
    }
}
