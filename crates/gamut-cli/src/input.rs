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
//! carries, and it reads the file's ICC profile and Exif alongside.

use std::path::Path;

use gamut::core::convert::{ConvertPolicy, convert};
use gamut::core::{
    DecodeImage, Dimensions, ErrorKind, Gray8, ImageBuf, Rgb8, Rgb16, Rgba8, Rgba16,
};
use gamut::jpeg::JpegDecoder;
use gamut::jxl::JxlDecoder;
use gamut::png::{ColorType as PngColorType, PngDecoder};
use gamut::webp::WebpDecoder;

use crate::error::CliError;

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

/// The input metadata an encoder can carry: the ICC profile and the Exif TIFF stream, each
/// verbatim from the input file.
#[derive(Default)]
pub(crate) struct InputMetadata {
    /// The embedded ICC profile.
    pub(crate) icc: Option<Vec<u8>>,
    /// The Exif payload as a bare TIFF stream (starting `II`/`MM`, no `Exif\0\0` prefix).
    pub(crate) exif: Option<Vec<u8>>,
}

impl InputMetadata {
    /// The metadata as it describes the decoded image in `layout`, given whether the file's
    /// samples are `grey`.
    ///
    /// A grey image widened to an RGB(A) layout keeps its pixels but not its ICC profile: a grey
    /// profile's data colour space is `GRAY` (ICC.1:2022 §7.2.6, Table 19), which cannot describe
    /// three-channel data, and a reader may ignore or reject the image for it. The profile is
    /// dropped rather than replaced — the widened image is then untagged (sRGB by default), which
    /// is how every grey image left this CLI before ICC was carried at all.
    fn for_layout(mut self, grey: bool, layout: Layout) -> Self {
        if grey && layout != Layout::Gray8 {
            self.icc = None;
        }
        self
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

/// Decodes `decoder`'s view of `bytes` in `layout`. A macro because each arm instantiates
/// `DecodeImage` at a different pixel type; only decoders implementing all five can take it.
macro_rules! decode_as {
    ($decoder:expr, $bytes:expr, $layout:expr) => {{
        let decoder = $decoder;
        match $layout {
            Layout::Gray8 => NativeImage::Gray8(decoder.decode_image($bytes)?),
            Layout::Rgb8 => NativeImage::Rgb8(decoder.decode_image($bytes)?),
            Layout::Rgba8 => NativeImage::Rgba8(decoder.decode_image($bytes)?),
            Layout::Rgb16 => NativeImage::Rgb16(decoder.decode_image($bytes)?),
            Layout::Rgba16 => NativeImage::Rgba16(decoder.decode_image($bytes)?),
        }
    }};
}

/// Decodes a supported image file (PNG, JPEG, PPM/P6, WebP, or JPEG XL) in the narrowest layout
/// that holds it without loss, and reads its ICC profile and Exif.
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
/// Metadata is best-effort: the pixels have decoded by the time it is read, so a payload that
/// cannot be read is simply absent rather than failing the conversion. An ICC profile is returned
/// only when it describes the decoded pixels: not for an XYB-coded JPEG XL (decoded to sRGB), and
/// not for a grey image widened to an RGB(A) layout.
///
/// `bytes` is the file already read (see [`read_input`]); `path` only labels errors.
pub(crate) fn decode_native(
    path: &Path,
    bytes: &[u8],
) -> Result<(NativeImage, InputMetadata), CliError> {
    // The layout is chosen so no conversion loses anything; the permissive policy only matters
    // where no layout can (a float JPEG XL narrowing to 16 bits), as on the fixed-layout path.
    let lossy = ConvertPolicy::permissive();

    if is_webp(bytes) {
        // Ask for RGB under the *lossless* policy: the decoder refuses it exactly when the file
        // carries alpha, and only then is RGBA decoded.
        let image = match WebpDecoder::new().decode_image(bytes) {
            Ok(rgb) => NativeImage::Rgb8(rgb),
            Err(err) if err.kind() == ErrorKind::Unsupported => NativeImage::Rgba8(
                WebpDecoder::new()
                    .convert_policy(lossy)
                    .decode_image(bytes)?,
            ),
            Err(err) => return Err(err.into()),
        };
        let metadata = gamut::webp::metadata(bytes)
            .map(|m| InputMetadata {
                icc: m.icc,
                exif: m.exif,
            })
            .unwrap_or_default();
        return Ok((image, metadata));
    }

    if is_jxl(bytes) {
        let decoder = JxlDecoder::new().with_convert_policy(lossy);
        let info = decoder.info(bytes)?;
        let grey = info.color_channels == 1;
        let layout = Layout::pick(
            grey,
            info.has_alpha,
            info.is_float || info.bits_per_sample > 8,
        );
        // An XYB-coded stream decodes to sRGB, not into its embedded ICC profile, so the profile
        // describes the original image rather than these pixels and is not carried.
        let metadata = decoder
            .metadata(bytes)
            .map(|m| InputMetadata {
                icc: m.icc.filter(|_| info.uses_original_profile),
                exif: m.exif,
            })
            .unwrap_or_default()
            .for_layout(grey, layout);
        return Ok((decode_as!(decoder, bytes, layout), metadata));
    }

    if is_png(bytes) {
        let decoder = PngDecoder::new().convert_policy(lossy);
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
        let metadata = InputMetadata {
            icc: decoded.icc_profile.map(|icc| icc.profile),
            exif: decoded.exif,
        }
        .for_layout(grey, layout);
        return Ok((decode_as!(decoder, bytes, layout), metadata));
    }

    if is_jpeg(bytes) {
        let decoder = JpegDecoder::new().convert_policy(lossy);
        let image = if gamut::jpeg::info(bytes)?.components == 1 {
            NativeImage::Gray8(decoder.decode_image(bytes)?)
        } else {
            NativeImage::Rgb8(decoder.decode_image(bytes)?)
        };
        let metadata = gamut::jpeg::metadata(bytes)
            .map(|m| InputMetadata {
                icc: m.icc,
                exif: m.exif,
            })
            .unwrap_or_default();
        return Ok((image, metadata));
    }

    let decoded = image::load_from_memory(bytes).map_err(|source| CliError::Decode {
        path: path.to_path_buf(),
        source,
    })?;
    let color = decoded.color();
    let channel_bits = color.bits_per_pixel() / u16::from(color.channel_count());
    let dims = Dimensions {
        width: decoded.width(),
        height: decoded.height(),
    };
    let image = match Layout::pick(!color.has_color(), color.has_alpha(), channel_bits > 8) {
        Layout::Gray8 => NativeImage::Gray8(ImageBuf::new(decoded.to_luma8().into_raw(), dims)?),
        Layout::Rgb8 => NativeImage::Rgb8(ImageBuf::new(decoded.to_rgb8().into_raw(), dims)?),
        Layout::Rgba8 => NativeImage::Rgba8(ImageBuf::new(decoded.to_rgba8().into_raw(), dims)?),
        Layout::Rgb16 => NativeImage::Rgb16(ImageBuf::new(decoded.to_rgb16().into_raw(), dims)?),
        Layout::Rgba16 => NativeImage::Rgba16(ImageBuf::new(decoded.to_rgba16().into_raw(), dims)?),
    };
    Ok((image, InputMetadata::default()))
}

/// Unwraps a gamut decode into the flat `(samples, dimensions)` pair the encoders take, choosing
/// the RGB or RGBA layout by `want_alpha`.
///
/// A macro rather than a function because the two arms instantiate `DecodeImage` at different
/// pixel types, which no single call can express.
macro_rules! decode_with {
    ($decoder:expr, $bytes:expr, $want_alpha:expr) => {{
        let decoder = $decoder;
        // `?` maps `gamut::core::Error` to `CliError::Codec` via the existing `#[from]` impl.
        if $want_alpha {
            let img: ImageBuf<Rgba8> = decoder.decode_image($bytes)?;
            let dims = img.dimensions();
            (img.into_samples(), dims)
        } else {
            let img: ImageBuf<Rgb8> = decoder.decode_image($bytes)?;
            let dims = img.dimensions();
            (img.into_samples(), dims)
        }
    }};
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
    decode_bytes(path, &bytes, false)
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

/// Decodes an already-read image (PNG, JPEG, PPM/P6, WebP, or JPEG XL) to interleaved RGB
/// (`want_alpha == false`) or RGBA (`want_alpha == true`, fully opaque when the source has none).
///
/// Format-dispatching core: routes every format gamut can decode to gamut's own decoder, and the
/// remainder (PPM) to the `image` crate. The format is detected from the contents, never from
/// `path`, which only labels errors.
pub(crate) fn decode_bytes(
    path: &Path,
    bytes: &[u8],
    want_alpha: bool,
) -> Result<(Vec<u8>, Dimensions), CliError> {
    let lossy = ConvertPolicy::permissive();

    if is_webp(bytes) {
        return Ok(decode_with!(
            WebpDecoder::new().convert_policy(lossy),
            bytes,
            want_alpha
        ));
    }

    if is_jxl(bytes) {
        return Ok(decode_with!(
            JxlDecoder::new().with_convert_policy(lossy),
            bytes,
            want_alpha
        ));
    }

    if is_png(bytes) {
        return Ok(decode_with!(
            PngDecoder::new().convert_policy(lossy),
            bytes,
            want_alpha
        ));
    }

    if is_jpeg(bytes) {
        // A JPEG never carries alpha, so gamut-jpeg implements no `DecodeImage<Rgba8>`. Decode the
        // colour it does carry, then let the same conversion engine add the opaque alpha channel
        // rather than padding it by hand here.
        let img: ImageBuf<Rgb8> = JpegDecoder::new()
            .convert_policy(lossy)
            .decode_image(bytes)?;
        if !want_alpha {
            let dims = img.dimensions();
            return Ok((img.into_samples(), dims));
        }
        let rgba: ImageBuf<Rgba8> = convert(img.as_ref(), lossy)?;
        let dims = rgba.dimensions();
        return Ok((rgba.into_samples(), dims));
    }

    let decoded = image::load_from_memory(bytes).map_err(|source| CliError::Decode {
        path: path.to_path_buf(),
        source,
    })?;
    let (buf, width, height) = if want_alpha {
        let rgba = decoded.to_rgba8();
        let (w, h) = rgba.dimensions();
        (rgba.into_raw(), w, h)
    } else {
        let rgb = decoded.to_rgb8();
        let (w, h) = rgb.dimensions();
        (rgb.into_raw(), w, h)
    };
    Ok((buf, Dimensions { width, height }))
}

/// Returns `true` if `bytes` begins with a RIFF/WebP container signature (`RIFF`…`WEBP`), matching
/// how the `image` crate detects formats by content rather than trusting the file extension.
fn is_webp(bytes: &[u8]) -> bool {
    bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP"
}

/// Returns `true` if `bytes` begins with the 8-byte PNG signature (§5.2). Sniffs by content like
/// [`is_webp`], so the file extension need not be accurate.
fn is_png(bytes: &[u8]) -> bool {
    bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A])
}

/// Returns `true` if `bytes` begins with the JPEG SOI marker (`FF D8 FF`). Sniffs by content like
/// [`is_webp`], so the file extension need not be accurate.
fn is_jpeg(bytes: &[u8]) -> bool {
    bytes.starts_with(&[0xFF, 0xD8, 0xFF])
}

/// Returns `true` if `bytes` begins with a JPEG XL signature — either the 2-byte bare codestream
/// signature `FF 0A` or the 12-byte ISO BMFF container box signature
/// (`00 00 00 0C 4A 58 4C 20 0D 0A 87 0A`). Sniffs by content like [`is_webp`], so the file
/// extension need not be accurate.
fn is_jxl(bytes: &[u8]) -> bool {
    /// The ISO BMFF `.jxl` container signature: a 12-byte JXL box.
    const CONTAINER: [u8; 12] = [
        0x00, 0x00, 0x00, 0x0C, 0x4A, 0x58, 0x4C, 0x20, 0x0D, 0x0A, 0x87, 0x0A,
    ];
    bytes.starts_with(&[0xFF, 0x0A]) || bytes.starts_with(&CONTAINER)
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
                ImageRef::<Rgba8>::new(rgba, Dimensions { width, height }).unwrap(),
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
                ImageRef::<Rgba8>::new(rgba, Dimensions { width, height }).unwrap(),
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
        assert!(is_webp(&webp));
        let (rgb, dims) = decode_bytes(Path::new("mem.webp"), &webp, false).unwrap();
        assert_eq!(
            dims,
            Dimensions {
                width: 2,
                height: 2
            }
        );
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
        let (out, dims) = decode_bytes(Path::new("mem.webp"), &webp, true).unwrap();
        assert_eq!(
            dims,
            Dimensions {
                width: 2,
                height: 2
            }
        );
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
        assert!(is_jxl(&jxl));
        let (rgb, dims) = decode_bytes(Path::new("mem.jxl"), &jxl, false).unwrap();
        assert_eq!(
            dims,
            Dimensions {
                width: 2,
                height: 2
            }
        );
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
        assert!(is_jxl(&jxl));
        let (out, dims) = decode_bytes(Path::new("mem.jxl"), &jxl, true).unwrap();
        assert_eq!(
            dims,
            Dimensions {
                width: 2,
                height: 2
            }
        );
        // Lossless JPEG XL carries alpha natively, so the round-trip is bit-exact.
        assert_eq!(out, rgba);
    }

    #[test]
    fn jxl_container_signature_is_sniffed() {
        // The 12-byte ISO BMFF `.jxl` box signature is recognised as JPEG XL.
        let container = [
            0x00, 0x00, 0x00, 0x0C, 0x4A, 0x58, 0x4C, 0x20, 0x0D, 0x0A, 0x87, 0x0A,
        ];
        assert!(is_jxl(&container));
        // The bare-codestream signature is also recognised; unrelated bytes are not.
        assert!(is_jxl(&[0xFF, 0x0A, 0x00]));
        assert!(!is_jxl(&[0xFF, 0x0B]));
        assert!(!is_jxl(b"RIFF"));
    }

    #[test]
    fn sixteen_bit_grayscale_png_is_decoded_by_gamut() {
        use gamut::core::Gray16;
        use gamut::png::PngEncoder;

        // Two things gamut-png refuses by default happen here at once: 16-bit samples narrow to 8,
        // and a single grey channel replicates into three. The decode therefore succeeds only
        // because this module opts into the loss -- and only because the PNG path now goes through
        // gamut-png at all instead of detouring through the `image` crate.
        let dims = Dimensions {
            width: 4,
            height: 1,
        };
        let gray16: [u16; 4] = [0, 0x8080, 0xFFFF, 0x0101];
        let mut png = Vec::new();
        PngEncoder::new()
            .encode_image(ImageRef::<Gray16>::new(&gray16, dims).unwrap(), &mut png)
            .expect("encode png");
        assert!(is_png(&png));

        let (rgb, got) = decode_bytes(Path::new("mem.png"), &png, false).unwrap();
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
        let dims = Dimensions {
            width: 8,
            height: 8,
        };
        let gray = vec![0x40u8; 64];
        let mut jpeg = Vec::new();
        JpegEncoder::new()
            .encode_image(ImageRef::<Gray8>::new(&gray, dims).unwrap(), &mut jpeg)
            .expect("encode jpeg");
        assert!(is_jpeg(&jpeg));

        let (rgba, got) = decode_bytes(Path::new("mem.jpg"), &jpeg, true).unwrap();
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
        match decode_native(Path::new("mem"), bytes).unwrap().0 {
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

        let dims = Dimensions {
            width: 8,
            height: 8,
        };
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
        use gamut::png::{PngEncoder, PngPalette};

        let dims = Dimensions {
            width: 2,
            height: 1,
        };
        let encode = |palette: PngPalette| {
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
        let opaque = PngPalette::new(&colours).unwrap();
        let keyed = PngPalette::with_transparency(&colours, &[0]).unwrap();
        assert_eq!(native_layout(&encode(opaque)), Layout::Rgb8);
        assert_eq!(native_layout(&encode(keyed)), Layout::Rgba8);
    }

    #[test]
    fn native_sixteen_bit_grey_png_widens_to_rgb16() {
        use gamut::core::Gray16;
        use gamut::png::PngEncoder;

        let dims = Dimensions {
            width: 2,
            height: 1,
        };
        let png = PngEncoder::new()
            .encode_to_vec(ImageRef::<Gray16>::new(&[0, 0x8080], dims).unwrap())
            .unwrap();
        assert_eq!(native_layout(&png), Layout::Rgb16);
    }

    /// The ICC profile `decode_native` read for `bytes`.
    fn native_icc(bytes: &[u8]) -> Option<Vec<u8>> {
        decode_native(Path::new("mem"), bytes).unwrap().1.icc
    }

    #[test]
    fn metadata_for_layout_drops_icc_only_from_widened_grey() {
        let tagged = || InputMetadata {
            icc: Some(vec![1]),
            exif: Some(vec![2]),
        };
        assert!(tagged().for_layout(true, Layout::Gray8).icc.is_some());
        let widened = tagged().for_layout(true, Layout::Rgb16);
        assert!(widened.icc.is_none());
        assert!(widened.exif.is_some(), "Exif is layout-independent");
        assert!(tagged().for_layout(false, Layout::Rgb16).icc.is_some());
    }

    #[test]
    fn native_png_drops_a_grey_profile_when_widened() {
        use gamut::core::Gray16;
        use gamut::icc::IccProfile;
        use gamut::png::PngEncoder;

        let grey_icc = IccProfile::gray_with_gamma(2.2)
            .unwrap()
            .to_bytes()
            .unwrap();
        let dims = Dimensions {
            width: 2,
            height: 1,
        };
        let encoder = || PngEncoder::new().with_icc_profile("grey", &grey_icc);
        let grey8 = encoder()
            .encode_to_vec(ImageRef::<Gray8>::new(&[0, 0x80], dims).unwrap())
            .unwrap();
        let grey16 = encoder()
            .encode_to_vec(ImageRef::<Gray16>::new(&[0, 0x8080], dims).unwrap())
            .unwrap();
        assert_eq!(native_icc(&grey8), Some(grey_icc.clone()));
        assert_eq!(native_icc(&grey16), None);
    }

    #[test]
    fn native_jxl_carries_its_icc_only_when_pixels_decode_into_it() {
        use gamut::icc::{BuiltinProfile, Curve, IccProfile, KnownTag, TagData};
        use gamut::jxl::{ColorSpec, Distance};

        // libjxl re-signals a profile it can express as a structured encoding (one with a `cicp`
        // tag, or a matrix/TRC profile with an analytic curve) instead of embedding it, so the
        // `cicp` tag goes and the TRCs become a sampled smoothstep (3x^2 - 2x^3), which no gamma
        // or standard transfer function fits.
        let mut profile = IccProfile::builtin(BuiltinProfile::DisplayP3).unwrap();
        profile
            .tags
            .retain(|(signature, _)| *signature != KnownTag::Cicp.into());
        let table: Vec<u16> = (0..=16u32)
            .map(|i| {
                let x = f64::from(i) / 16.0;
                ((3.0 * x * x - 2.0 * x * x * x) * 65535.0).round() as u16
            })
            .collect();
        for (signature, data) in &mut profile.tags {
            if [KnownTag::RedTrc, KnownTag::GreenTrc, KnownTag::BlueTrc]
                .into_iter()
                .any(|trc| *signature == trc.into())
            {
                *data = TagData::Curve(Curve::Sampled(table.clone()));
            }
        }
        let icc = profile.to_bytes().unwrap();
        let rgb: Vec<u8> = (0..8 * 8 * 3).map(|i| (i * 5) as u8).collect();
        let encode = |encoder: JxlEncoder| {
            let mut out = Vec::new();
            encoder
                .with_color(ColorSpec::Icc(icc.clone()))
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
    }

    #[test]
    fn non_riff_bytes_take_the_image_path() {
        // Not a RIFF/WebP container, so it must NOT be routed to the webp decoder; the image path
        // rejects it with a decode error rather than panicking.
        let junk = [0u8; 32];
        assert!(!is_webp(&junk));
        let err = decode_bytes(Path::new("junk.bin"), &junk, false).unwrap_err();
        assert!(matches!(err, CliError::Decode { .. }));
    }
}
