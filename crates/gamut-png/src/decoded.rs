//! The rich decode result ([`DecodedPng`]) and the ancillary-chunk payload parsers (PNG spec
//! §11.3) — the read-side twin of the `ancillary` writers.
//!
//! Metadata payloads are surfaced **raw** — `eXIf` bytes, the inflated ICC profile, the XMP
//! packet, the C2PA manifest store — precisely so callers can hand them to
//! `gamut_metadata::MetadataBlock` (`Exif`/`Icc`/`Xmp`/`C2pa`) without gamut-png depending on
//! the metadata stack. Fixed-layout colour-space chunks (`gAMA`/`cHRM`/`sRGB`/`cICP`) are
//! additionally parsed into values, in the same ×100 000 fixed-point units the encoder accepts,
//! and so are `sBIT`/`bKGD`/`pHYs`/`tIME`, in the units the encoder's setters for them take.
//! `sBIT` and `bKGD` are shaped by the colour type, so they are read against the IHDR.
//! Ancillary payloads are attacker territory: a malformed payload skips that chunk (§13.1)
//! rather than failing the image, and compressed payloads (iCCP/zTXt/iTXt) inflate under one
//! cumulative byte budget so a metadata zlib bomb cannot exhaust memory. The manifest store
//! (`caBX`, C2PA 2.4 §A.3.2) is uncompressed but attacker-sized, so its bytes are charged to the
//! same budget rather than copied beside it.

use gamut_core::{
    Gray8, Gray16, GrayAlpha8, GrayAlpha16, ImageBuf, Indexed8, Rgb8, Rgb16, Rgba8, Rgba16,
};

use crate::ancillary::{PhysicalUnit, SrgbIntent, sbit_for};
use crate::chunk::CABX;
use crate::color::ColorType;
use crate::decoder::{TransparencyKey, depth_mask};
use crate::ihdr::Ihdr;
use crate::inflate;
use crate::pack::gray8_scale;
use crate::palette::PngPalette;

/// The parsed image header (IHDR, §11.2.1), reported as stored in the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct PngHeader {
    /// Image width in pixels.
    pub width: u32,
    /// Image height in pixels.
    pub height: u32,
    /// The file's native bit depth (1, 2, 4, 8, or 16). The decoded image may present sub-byte
    /// greyscale widened to 8 bits; this field always reports the stored depth.
    pub bit_depth: u8,
    /// The file's colour type.
    pub color_type: ColorType,
    /// Whether the file was Adam7-interlaced (pixels are always returned de-interlaced).
    pub interlaced: bool,
}

/// Decoded pixels in the file's native layout: sub-byte greyscale is scaled exactly to
/// [`Gray8`] (§13.12), sub-byte palette indices are widened to [`Indexed8`] **unscaled**, and
/// 16-bit samples are native-endian `u16`.
#[derive(Debug)]
pub enum PngImage {
    /// Greyscale, stored at 1–8 bits (sub-byte samples scaled to 8 bits).
    Gray8(ImageBuf<Gray8>),
    /// 16-bit greyscale.
    Gray16(ImageBuf<Gray16>),
    /// 8-bit greyscale with alpha.
    GrayAlpha8(ImageBuf<GrayAlpha8>),
    /// 16-bit greyscale with alpha.
    GrayAlpha16(ImageBuf<GrayAlpha16>),
    /// 8-bit RGB.
    Rgb8(ImageBuf<Rgb8>),
    /// 16-bit RGB.
    Rgb16(ImageBuf<Rgb16>),
    /// 8-bit RGBA.
    Rgba8(ImageBuf<Rgba8>),
    /// 16-bit RGBA.
    Rgba16(ImageBuf<Rgba16>),
    /// Palette indices (unscaled); the palette rides in [`DecodedPng::palette`].
    Indexed8(ImageBuf<Indexed8>),
}

/// An embedded ICC profile (iCCP, §11.3.2.3), already inflated.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct IccProfile {
    /// The profile name (Latin-1, 1–79 bytes).
    pub name: String,
    /// The raw ICC profile bytes. Feed as `gamut_metadata::MetadataBlock::Icc`.
    pub profile: Vec<u8>,
}

/// White point and primary chromaticities (cHRM, §11.3.2.1), each coordinate × 100 000.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Chromaticities {
    /// White point (x, y).
    pub white: (u32, u32),
    /// Red primary (x, y).
    pub red: (u32, u32),
    /// Green primary (x, y).
    pub green: (u32, u32),
    /// Blue primary (x, y).
    pub blue: (u32, u32),
}

/// Coding-independent code points (cICP, §11.3.2.6) identifying the video-signal colour space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Cicp {
    /// ITU-T H.273 colour primaries code.
    pub color_primaries: u8,
    /// ITU-T H.273 transfer function code.
    pub transfer_function: u8,
    /// ITU-T H.273 matrix coefficients code (0 = RGB, the only value PNG allows).
    pub matrix_coefficients: u8,
    /// Whether the samples use the full value range.
    pub full_range: bool,
}

/// The intended pixel size or aspect ratio (pHYs, §11.3.4.3, Table 23) — the field a viewer
/// scales by, so a re-encode that dropped it would change the printed size of the image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct PhysicalDimensions {
    /// Pixels per unit, X axis.
    pub x_ppu: u32,
    /// Pixels per unit, Y axis.
    pub y_ppu: u32,
    /// The unit (Table 24): metres, or unknown — an aspect ratio only.
    pub unit: PhysicalUnit,
}

/// The time of the last image modification (tIME, §11.3.5.1, Table 26), which §11.3.5.1 says
/// "should be" UTC. Each field is within the range Table 26 gives it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct ModificationTime {
    /// The complete year ("1995, not 95").
    pub year: u16,
    /// Month, 1–12.
    pub month: u8,
    /// Day, 1–31.
    pub day: u8,
    /// Hour, 0–23.
    pub hour: u8,
    /// Minute, 0–59.
    pub minute: u8,
    /// Second, 0–60 (60 allows for a leap second).
    pub second: u8,
}

/// A default background colour (bKGD, §11.3.4.1, Table 22), in the form the file's colour type
/// gives it.
///
/// Samples are in the units of the **decoded pixels** ([`PngImage`]), so a caller can composite
/// the image against it directly: the stored value for every depth of 8 or more — the bits above
/// the depth masked to 0, as §11.3.4.1 tells a decoder to — and, for sub-byte greyscale, which
/// [`PngImage::Gray8`] presents scaled to 8 bits (§13.12), the background scaled by exactly the same
/// factor. Those are also the units the encoder's `with_background_*` setters take, so a re-encode
/// of the decoded pixels carries it unchanged.
///
/// Closed: Table 22 defines exactly these three forms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Background {
    /// A grey level (colour types 0 and 4).
    Gray(u16),
    /// An RGB colour (colour types 2 and 6).
    Rgb(u16, u16, u16),
    /// A palette index (colour type 3). Not checked against the palette: the pixel-free
    /// [`metadata`](crate::metadata) walk never reads `PLTE`, and the two read surfaces agree
    /// field for field.
    Index(u8),
}

/// Which of §11.3.3's three chunks carried an annotation, and whether its text was compressed.
///
/// The four combinations are the whole space PNG defines, so this enum is closed. It exists so a
/// re-encode can put an annotation back in the chunk it came out of: without it a `zTXt` is
/// indistinguishable from a `tEXt` once decoded, and rewriting a compressed 40-byte payload as an
/// uncompressed one can inflate it fortyfold — preservation that does not preserve.
///
/// `#[repr(u8)]` with explicit, permanent discriminants: the value crosses the C ABI as a plain
/// integer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum TextChunkKind {
    /// `tEXt`: uncompressed Latin-1 (§11.3.3.2).
    Text = 0,
    /// `zTXt`: zlib-compressed Latin-1 (§11.3.3.3).
    CompressedText = 1,
    /// `iTXt` with the compression flag clear: uncompressed UTF-8 (§11.3.3.4).
    International = 2,
    /// `iTXt` with the compression flag set: zlib-compressed UTF-8 (§11.3.3.4).
    CompressedInternational = 3,
}

/// How a file framed its XMP packet inside the `iTXt` chunk §11.3.3.1 Table 21 reserves for it.
///
/// The packet itself is [`DecodedPng::xmp`] / [`PngMetadata::xmp`]; this is everything *else* the
/// chunk carried, and it is `Some` exactly when the packet is. It exists for the same reason
/// [`TextChunkKind`] does — a re-encode has to put the packet back the way it came out — but the
/// packet is surfaced as its own field rather than as a [`TextChunk`], so the framing needs its
/// own home. Table 21 *recommends* the null framing (`compressed` clear, both strings empty) for
/// XMP compliance; it does not require it, and a file that frames it otherwise is still a file
/// whose bytes have to survive a re-encode.
///
/// Marked `#[non_exhaustive]`: consolidating the packet into [`PngMetadata::texts`] would retire
/// this type, and that is a decision of its own (issue #600). Until then the pairing is a
/// convention, not a type: a caller assembling a [`PngMetadata`] by hand can set one field
/// without the other, and the encoder then takes this type's [`Default`] framing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct XmpFraming {
    /// The chunk's language tag (§11.3.3.4), if it carried a non-empty one.
    pub language: Option<String>,
    /// The chunk's translated keyword (§11.3.3.4), if it carried a non-empty one.
    pub translated_keyword: Option<String>,
    /// Whether the packet was stored zlib-compressed (§11.3.3.4's compression flag). A packet
    /// stored compressed and rewritten uncompressed is the same words at many times the size.
    pub compressed: bool,
}

/// One text annotation (tEXt/zTXt/iTXt, §11.3.3), decompressed where stored compressed.
///
/// tEXt/zTXt hold Latin-1, mapped code-point-for-code-point into the `String` (lossless);
/// iTXt holds UTF-8. [`kind`](Self::kind) records which chunk it was, so a re-encode can put it
/// back in the same one. The XMP packet (`XML:com.adobe.xmp`) is surfaced as
/// [`DecodedPng::xmp`], not repeated here.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct TextChunk {
    /// The keyword (1–79 bytes).
    pub keyword: String,
    /// The text payload.
    pub text: String,
    /// The iTXt language tag, if the chunk carried one.
    pub language: Option<String>,
    /// The iTXt translated keyword, if the chunk carried one.
    pub translated_keyword: Option<String>,
    /// The chunk this annotation was stored in, and whether its text was compressed.
    pub kind: TextChunkKind,
}

/// Everything a PNG carries: the pixels in their native layout plus the ancillary payloads.
///
/// Metadata is exposed as raw bytes so a caller can borrow it straight into
/// `gamut_metadata::MetadataBlock`, e.g. `decoded.exif.as_deref().map(MetadataBlock::Exif)`.
#[derive(Debug)]
#[non_exhaustive]
pub struct DecodedPng {
    /// The parsed image header, as stored.
    pub header: PngHeader,
    /// The decoded pixels, in the file's native layout.
    pub image: PngImage,
    /// The palette of an indexed image (indices are in [`Self::image`]; per-entry alpha comes
    /// from tRNS).
    pub palette: Option<PngPalette>,
    /// The tRNS colour key of a greyscale/truecolour image, in native (unscaled) sample units.
    pub transparency: Option<TransparencyKey>,
    /// The eXIf payload verbatim: a TIFF stream starting with `II`/`MM` (§11.3.4.5). Feed as
    /// `gamut_metadata::MetadataBlock::Exif`.
    pub exif: Option<Vec<u8>>,
    /// The embedded ICC profile (iCCP), inflated. Feed as `MetadataBlock::Icc`.
    pub icc_profile: Option<IccProfile>,
    /// The XMP packet (the `XML:com.adobe.xmp` iTXt, §11.3.3.4), decompressed if stored
    /// compressed. Feed as `MetadataBlock::Xmp`.
    pub xmp: Option<Vec<u8>>,
    /// How the chunk that carried [`xmp`](Self::xmp) framed it: its compression flag, language
    /// tag and translated keyword (§11.3.3.4). `Some` exactly when `xmp` is.
    pub xmp_framing: Option<XmpFraming>,
    /// The C2PA manifest store (the `caBX` chunk, C2PA 2.4 §A.3.2) verbatim: the JUMBF bytes,
    /// uncompressed, exactly as the chunk carries them — opaque here, never parsed or judged.
    /// Feed as `MetadataBlock::C2pa`. The first CRC-valid `caBX` before the first `IDAT`, and
    /// only when it fits the metadata budget; see [`c2pa_ignored`](Self::c2pa_ignored).
    pub c2pa: Option<Vec<u8>>,
    /// How many CRC-valid `caBX` chunks **in the datastream** were not surfaced as the store:
    /// a chunk later than the first, a chunk positioned after `IDAT`, and the store-position
    /// chunk itself when it did not fit the metadata budget.
    ///
    /// A file carries exactly one manifest store — PNG has no multi-chunk store, unlike JPEG's
    /// APP11 run — so a non-zero count marks a file whose extra chunks were ignored rather than
    /// concatenated. It does **not** identify why: a store past the budget and a chunk appended
    /// after `IDAT` both land here, and `c2pa == None` with a non-zero count is either.
    ///
    /// A `caBX` after `IEND` is **not** counted. Bytes after `IEND` are a trailer rather than
    /// part of the datastream (§13.2), and neither metadata walk reads them. To see one — the
    /// shape of a chunk appended to a finished file — use
    /// [`deconstruct`](crate::deconstruct), whose report accounts the trailer as a segment.
    pub c2pa_ignored: usize,
    /// tEXt/zTXt/iTXt annotations in file order (the XMP packet is excluded).
    pub texts: Vec<TextChunk>,
    /// gAMA: image gamma × 100 000 (§11.3.2.2) — the unit the encoder's `with_gamma` writes.
    pub gamma: Option<u32>,
    /// cHRM chromaticities, each coordinate × 100 000.
    pub chromaticities: Option<Chromaticities>,
    /// sRGB rendering intent (§11.3.2.5).
    pub srgb: Option<SrgbIntent>,
    /// cICP video-signal code points.
    pub cicp: Option<Cicp>,
    /// sBIT: the original number of significant bits per channel (§11.3.2.4, Table 14), one
    /// entry per channel in the order Table 14 lists them — three for a palette image.
    pub significant_bits: Option<Vec<u8>>,
    /// bKGD: the default background colour (§11.3.4.1), in the decoded pixels' units.
    pub background: Option<Background>,
    /// pHYs: the intended pixel size or aspect ratio (§11.3.4.3).
    pub physical_dimensions: Option<PhysicalDimensions>,
    /// tIME: the time of the last image modification (§11.3.5.1).
    pub modification_time: Option<ModificationTime>,
}

/// The ancillary metadata a PNG carries, read without decoding any pixels by
/// [`metadata`](crate::metadata) or [`PngDecoder::metadata`](crate::PngDecoder::metadata).
///
/// Each payload is in the form the dedicated metadata crates parse (and
/// [`gamut-metadata`](https://crates.io/crates/gamut-metadata)'s `MetadataBlock` borrows)
/// directly, e.g. `meta.exif.as_deref().map(MetadataBlock::Exif)`. These are exactly the fields
/// [`DecodedPng`] exposes alongside its pixels, so the two entry points agree field for field on
/// the same file.
///
/// Unlike `gamut_jpeg::JpegMetadata` and `gamut_webp::WebpMetadata`, which carry only
/// EXIF/XMP/ICC, this also surfaces PNG's parsed colour chunks — `cICP`, `sRGB`, `gAMA`, `cHRM` —
/// because those are uncompressed and answer "what colour space is this?" on their own, the
/// C2PA manifest store (`caBX`), raw, and the remaining fixed-layout chunks a re-encode has to
/// carry — `sBIT`, `bKGD`, `pHYs` and `tIME`.
///
/// Marked `#[non_exhaustive]` so further ancillary chunks can be added without a breaking change.
///
/// # Example
///
/// ```
/// use gamut_core::{Dimensions, EncodeImage, ImageRef, Rgb8};
/// use gamut_png::PngEncoder;
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let pixels = vec![0u8; 3 * 4];
/// let image = ImageRef::<Rgb8>::new(&pixels, Dimensions::new(2, 2)?)?;
/// let png = PngEncoder::new().with_gamma(1.0 / 2.2).encode_to_vec(image)?;
///
/// // gAMA stores the encoding gamma × 100 000, uncompressed — so a probe can read a PNG's
/// // colour information without inflating anything.
/// let meta = gamut_png::metadata(&png)?;
/// assert_eq!(meta.gamma, Some(45_455));
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct PngMetadata {
    /// The eXIf payload verbatim: a TIFF stream starting with `II`/`MM` (§11.3.4.5). Feed as
    /// `gamut_metadata::MetadataBlock::Exif`.
    pub exif: Option<Vec<u8>>,
    /// The embedded ICC profile (iCCP), inflated. Feed as `MetadataBlock::Icc`.
    pub icc_profile: Option<IccProfile>,
    /// The XMP packet (the `XML:com.adobe.xmp` iTXt, §11.3.3.4), decompressed if stored
    /// compressed. Feed as `MetadataBlock::Xmp`.
    pub xmp: Option<Vec<u8>>,
    /// How the chunk that carried [`xmp`](Self::xmp) framed it: its compression flag, language
    /// tag and translated keyword (§11.3.3.4). `Some` exactly when `xmp` is.
    pub xmp_framing: Option<XmpFraming>,
    /// The C2PA manifest store (the `caBX` chunk, C2PA 2.4 §A.3.2) verbatim and uncompressed —
    /// opaque bytes, never parsed or judged. Feed as `MetadataBlock::C2pa`. The first CRC-valid
    /// `caBX` before the first `IDAT`, and only when it fits the metadata budget; see
    /// [`c2pa_ignored`](Self::c2pa_ignored).
    pub c2pa: Option<Vec<u8>>,
    /// How many CRC-valid `caBX` chunks **in the datastream** were not surfaced as the store:
    /// a chunk later than the first, a chunk positioned after `IDAT`, and the store-position
    /// chunk itself when it did not fit the metadata budget.
    ///
    /// A file carries exactly one manifest store — PNG has no multi-chunk store, unlike JPEG's
    /// APP11 run — so a non-zero count marks a file whose extra chunks were ignored rather than
    /// concatenated. It does **not** identify why: a store past the budget and a chunk appended
    /// after `IDAT` both land here, and `c2pa == None` with a non-zero count is either.
    ///
    /// A `caBX` after `IEND` is **not** counted. Bytes after `IEND` are a trailer rather than
    /// part of the datastream (§13.2), and neither metadata walk reads them. To see one — the
    /// shape of a chunk appended to a finished file — use
    /// [`deconstruct`](crate::deconstruct), whose report accounts the trailer as a segment.
    pub c2pa_ignored: usize,
    /// tEXt/zTXt/iTXt annotations in file order (the XMP packet is excluded).
    pub texts: Vec<TextChunk>,
    /// gAMA: image gamma × 100 000 (§11.3.2.2).
    pub gamma: Option<u32>,
    /// cHRM chromaticities, each coordinate × 100 000.
    pub chromaticities: Option<Chromaticities>,
    /// sRGB rendering intent (§11.3.2.5).
    pub srgb: Option<SrgbIntent>,
    /// cICP video-signal code points.
    pub cicp: Option<Cicp>,
    /// sBIT: the original number of significant bits per channel (§11.3.2.4, Table 14), one
    /// entry per channel in the order Table 14 lists them — three for a palette image.
    pub significant_bits: Option<Vec<u8>>,
    /// bKGD: the default background colour (§11.3.4.1), in the units
    /// [`DecodedPng::image`] presents its samples in — see [`Background`].
    pub background: Option<Background>,
    /// pHYs: the intended pixel size or aspect ratio (§11.3.4.3).
    pub physical_dimensions: Option<PhysicalDimensions>,
    /// tIME: the time of the last image modification (§11.3.5.1).
    pub modification_time: Option<ModificationTime>,
}

/// Parses the metadata-bearing ancillary chunks collected from the stream (in file order).
/// Malformed payloads skip their chunk (§13.1); compressed payloads — and the uncompressed but
/// attacker-sized `caBX` store — share `budget` bytes of output, and a payload that would bust
/// the remainder is skipped, not an error. Once-only chunks keep their first occurrence; every
/// CRC-valid `caBX` this walk does not surface as the store — a later one, or the first when it
/// busts the budget — is counted, since exactly one store is the rule (C2PA §A.3.2).
///
/// `chunks` holds only chunks in a position where a store may appear: the caller's walk drops a
/// `caBX` after `IDAT` before it gets here and counts it into
/// [`PngMetadata::c2pa_ignored`](PngMetadata::c2pa_ignored) itself, so this function never has to
/// know where in the file a chunk sat.
///
/// `header` is the file's IHDR, which `sBIT` and `bKGD` are shaped by: a payload whose length does
/// not match its colour type, or whose values the bit depth cannot hold, is malformed.
pub(crate) fn collect(chunks: &[([u8; 4], &[u8])], budget: usize, header: &Ihdr) -> PngMetadata {
    let mut meta = PngMetadata::default();
    let mut budget = budget;
    // Whether a `caBX` has been seen at all, admitted or not: the first is the store (or is
    // skipped for its size), every later one is a duplicate — never promoted into its place.
    let mut seen_c2pa = false;
    for &(chunk_type, data) in chunks {
        match &chunk_type {
            b"eXIf" if meta.exif.is_none() => meta.exif = Some(data.to_vec()),
            _ if chunk_type == CABX => {
                if seen_c2pa {
                    // A second store-position chunk: never the store, whatever became of the
                    // first, so an oversized store cannot be substituted by a smaller one.
                    meta.c2pa_ignored += 1;
                } else {
                    seen_c2pa = true;
                    if data.len() <= budget {
                        budget -= data.len();
                        meta.c2pa = Some(data.to_vec());
                    } else {
                        // In store position but not admitted: counted, like every other
                        // CRC-valid `caBX` this walk declines to surface.
                        meta.c2pa_ignored += 1;
                    }
                }
            }
            b"iCCP" if meta.icc_profile.is_none() => {
                meta.icc_profile = parse_iccp(data, &mut budget);
            }
            b"gAMA" if meta.gamma.is_none() => {
                if let Ok(bytes) = <&[u8; 4]>::try_from(data) {
                    meta.gamma = Some(u32::from_be_bytes(*bytes));
                }
            }
            b"cHRM" if meta.chromaticities.is_none() => {
                meta.chromaticities = parse_chrm(data);
            }
            b"sRGB" if meta.srgb.is_none() => {
                if let [code] = data {
                    meta.srgb = SrgbIntent::from_code(*code);
                }
            }
            b"cICP" if meta.cicp.is_none() => {
                if let [primaries, transfer, matrix, full_range @ (0 | 1)] = *data {
                    meta.cicp = Some(Cicp {
                        color_primaries: primaries,
                        transfer_function: transfer,
                        matrix_coefficients: matrix,
                        full_range: full_range == 1,
                    });
                }
            }
            b"sBIT" if meta.significant_bits.is_none() => {
                meta.significant_bits = parse_sbit(data, header);
            }
            b"bKGD" if meta.background.is_none() => {
                meta.background = parse_bkgd(data, header);
            }
            b"pHYs" if meta.physical_dimensions.is_none() => {
                meta.physical_dimensions = parse_phys(data);
            }
            b"tIME" if meta.modification_time.is_none() => {
                meta.modification_time = parse_time(data);
            }
            b"tEXt" => {
                if let Some(text) = parse_text(data) {
                    meta.texts.push(text);
                }
            }
            b"zTXt" => {
                if let Some(text) = parse_ztxt(data, &mut budget) {
                    meta.texts.push(text);
                }
            }
            b"iTXt" => match parse_itxt(data, &mut budget) {
                Some(ITxt::Xmp(packet, framing)) => {
                    if meta.xmp.is_none() {
                        meta.xmp = Some(packet);
                        meta.xmp_framing = Some(framing);
                    }
                }
                Some(ITxt::Text(text)) => meta.texts.push(text),
                None => {}
            },
            _ => {}
        }
    }
    meta
}

/// The standard iTXt keyword carrying an XMP packet (XMP Specification Part 3), reserved for it
/// by §11.3.3.1 Table 21. Shared with the encoder so the two sides cannot disagree on it.
pub(crate) const XMP_KEYWORD: &str = "XML:com.adobe.xmp";

/// A parsed iTXt: either the XMP packet and how its chunk framed it, or an ordinary text
/// annotation.
enum ITxt {
    Xmp(Vec<u8>, XmpFraming),
    Text(TextChunk),
}

/// Splits a payload at its first NUL and validates the keyword (1–79 bytes, §11.3.3.1).
fn split_keyword(data: &[u8]) -> Option<(String, &[u8])> {
    let nul = data.iter().position(|&b| b == 0)?;
    if nul == 0 || nul > 79 {
        return None;
    }
    Some((latin1(&data[..nul]), &data[nul + 1..]))
}

/// Decodes Latin-1 bytes code-point-for-code-point (lossless: byte n is U+00*n*).
fn latin1(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| char::from(b)).collect()
}

/// Inflates a compressed metadata payload under the shared budget; `None` (skip the chunk) if
/// the stream is corrupt or would bust the remaining budget.
fn inflate_metadata(data: &[u8], budget: &mut usize) -> Option<Vec<u8>> {
    let inflated = inflate::inflate_zlib(data, *budget).ok()?;
    *budget -= inflated.len();
    Some(inflated)
}

/// iCCP (§11.3.2.3): profile name, NUL, compression method 0, deflated profile.
fn parse_iccp(data: &[u8], budget: &mut usize) -> Option<IccProfile> {
    let (name, rest) = split_keyword(data)?;
    let (&method, compressed) = rest.split_first()?;
    if method != 0 {
        return None; // 0 (zlib) is the only defined compression method
    }
    Some(IccProfile {
        name,
        profile: inflate_metadata(compressed, budget)?,
    })
}

/// cHRM (§11.3.2.1): eight × 100 000 fixed-point coordinates, big-endian.
fn parse_chrm(data: &[u8]) -> Option<Chromaticities> {
    let data: &[u8; 32] = data.try_into().ok()?;
    let coord = |i: usize| {
        u32::from_be_bytes([
            data[i * 4],
            data[i * 4 + 1],
            data[i * 4 + 2],
            data[i * 4 + 3],
        ])
    };
    Some(Chromaticities {
        white: (coord(0), coord(1)),
        red: (coord(2), coord(3)),
        green: (coord(4), coord(5)),
        blue: (coord(6), coord(7)),
    })
}

/// sBIT (§11.3.2.4, Table 14): one entry per channel — three for a palette image, whose entries
/// are RGB — each "greater than zero and less than or equal to the sample depth (which is 8 for
/// indexed-color images, and the bit depth given in IHDR for other color types)".
///
/// The length is checked here, against the colour type the file declares; the range is
/// [`sbit_for`]'s, the rule the encoder writes by, so the two sides cannot disagree on it. The
/// length check comes first because `sbit_for` *converts* across colour types — it would read a
/// three-entry payload whose entries agree as a greyscale one — and a reader must not.
fn parse_sbit(data: &[u8], header: &Ihdr) -> Option<Vec<u8>> {
    let entries = match header.color {
        ColorType::Indexed => 3,
        color => color.channels(),
    };
    if data.len() != entries {
        return None;
    }
    sbit_for(data, header.color, header.bit_depth)
}

/// bKGD (§11.3.4.1, Table 22): a palette index, a grey level or an RGB triple, by colour type,
/// each sample two bytes "regardless of the image bit depth". Below 16 bits "decoders must mask the
/// other bits to 0 before the value is used", so the mask is applied rather than the chunk
/// refused; sub-byte grey is then scaled to the 8 bits [`PngImage::Gray8`] presents (§13.12), so
/// the background is in the decoded pixels' units.
fn parse_bkgd(data: &[u8], header: &Ihdr) -> Option<Background> {
    let sample = |hi: u8, lo: u8| u16::from_be_bytes([hi, lo]) & depth_mask(header.bit_depth);
    match (header.color, data) {
        (ColorType::Indexed, &[index]) => Some(Background::Index(index)),
        (ColorType::Grayscale | ColorType::GrayscaleAlpha, &[hi, lo]) => Some(Background::Gray(
            // The factor is 1 at every depth of 8 or more, so this is the identity there.
            sample(hi, lo) * u16::from(gray8_scale(header.bit_depth)),
        )),
        (ColorType::Truecolor | ColorType::TruecolorAlpha, &[r1, r0, g1, g0, b1, b0]) => Some(
            Background::Rgb(sample(r1, r0), sample(g1, g0), sample(b1, b0)),
        ),
        _ => None,
    }
}

/// pHYs (§11.3.4.3, Table 23): X and Y pixels per unit, big-endian, then the unit specifier,
/// which Table 24 defines only as 0 (unknown) or 1 (metre).
fn parse_phys(data: &[u8]) -> Option<PhysicalDimensions> {
    let [x3, x2, x1, x0, y3, y2, y1, y0, unit] = *data else {
        return None;
    };
    Some(PhysicalDimensions {
        x_ppu: u32::from_be_bytes([x3, x2, x1, x0]),
        y_ppu: u32::from_be_bytes([y3, y2, y1, y0]),
        unit: PhysicalUnit::from_code(unit)?,
    })
}

/// tIME (§11.3.5.1, Table 26): a two-byte year, then month, day, hour, minute and second, each
/// within the range Table 26 gives it. A field outside its range is a malformed payload (§13.1),
/// as libpng's `png_set_tIME` ("Ignoring invalid time value") also treats it.
fn parse_time(data: &[u8]) -> Option<ModificationTime> {
    let [y1, y0, month, day, hour, minute, second] = *data else {
        return None;
    };
    let in_range = (1..=12).contains(&month)
        && (1..=31).contains(&day)
        && hour <= 23
        && minute <= 59
        && second <= 60;
    in_range.then_some(ModificationTime {
        year: u16::from_be_bytes([y1, y0]),
        month,
        day,
        hour,
        minute,
        second,
    })
}

/// tEXt (§11.3.3.2): keyword, NUL, Latin-1 text.
fn parse_text(data: &[u8]) -> Option<TextChunk> {
    let (keyword, text) = split_keyword(data)?;
    Some(TextChunk {
        keyword,
        text: latin1(text),
        language: None,
        translated_keyword: None,
        kind: TextChunkKind::Text,
    })
}

/// zTXt (§11.3.3.3): keyword, NUL, compression method 0, deflated Latin-1 text.
fn parse_ztxt(data: &[u8], budget: &mut usize) -> Option<TextChunk> {
    let (keyword, rest) = split_keyword(data)?;
    let (&method, compressed) = rest.split_first()?;
    if method != 0 {
        return None;
    }
    let text = inflate_metadata(compressed, budget)?;
    Some(TextChunk {
        keyword,
        text: latin1(&text),
        language: None,
        translated_keyword: None,
        kind: TextChunkKind::CompressedText,
    })
}

/// iTXt (§11.3.3.4): keyword, NUL, compression flag, compression method, language tag, NUL,
/// translated keyword, NUL, UTF-8 text (deflated when the flag is 1).
fn parse_itxt(data: &[u8], budget: &mut usize) -> Option<ITxt> {
    let (keyword, rest) = split_keyword(data)?;
    let (&flag, rest) = rest.split_first()?;
    let (&method, rest) = rest.split_first()?;
    let lang_end = rest.iter().position(|&b| b == 0)?;
    let language = latin1(&rest[..lang_end]);
    let rest = &rest[lang_end + 1..];
    let translated_end = rest.iter().position(|&b| b == 0)?;
    let translated = String::from_utf8(rest[..translated_end].to_vec()).ok()?;
    let text_bytes = match flag {
        0 => rest[translated_end + 1..].to_vec(),
        1 if method == 0 => inflate_metadata(&rest[translated_end + 1..], budget)?,
        _ => return None,
    };
    if keyword == XMP_KEYWORD {
        // The packet leaves by its own field, so everything the chunk framed it with — the
        // compression flag above all — leaves beside it rather than with the annotation list.
        // Without the flag a 71-byte chunk is rewritten as thousands of uncompressed bytes.
        return Some(ITxt::Xmp(
            text_bytes,
            XmpFraming {
                language: Some(language).filter(|l| !l.is_empty()),
                translated_keyword: Some(translated).filter(|t| !t.is_empty()),
                compressed: flag == 1,
            },
        ));
    }
    Some(ITxt::Text(TextChunk {
        keyword,
        text: String::from_utf8(text_bytes).ok()?,
        language: Some(language).filter(|l| !l.is_empty()),
        translated_keyword: Some(translated).filter(|t| !t.is_empty()),
        kind: if flag == 1 {
            TextChunkKind::CompressedInternational
        } else {
            TextChunkKind::International
        },
    }))
}

#[cfg(test)]
mod tests {
    use gamut_deflate::DeflateEncoder;

    use super::*;

    fn deflated(payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        DeflateEncoder::new().zlib_compress(payload, &mut out);
        out
    }

    /// An IHDR of `color` at `bit_depth`, for the chunks shaped by it.
    fn header(color: ColorType, bit_depth: u8) -> Ihdr {
        Ihdr {
            width: 1,
            height: 1,
            bit_depth,
            color,
            interlaced: false,
        }
    }

    /// [`super::collect`] under an 8-bit truecolour header, for every chunk the header does not
    /// shape.
    fn collect(chunks: &[([u8; 4], &[u8])], budget: usize) -> PngMetadata {
        super::collect(chunks, budget, &header(ColorType::Truecolor, 8))
    }

    /// Table 14's length per colour type, and §11.3.2.4's range — "greater than zero and less than
    /// or equal to the sample depth (which is 8 for indexed-color images, and the bit depth given
    /// in IHDR for other color types)" — at both ends.
    ///
    /// Kills [`parse_sbit`]'s length check and its `Indexed` arm. The three-agreeing-entries case
    /// under greyscale is the one [`sbit_for`] alone would accept, by converting it.
    #[test]
    fn sbit_is_read_against_the_colour_type_and_depth() {
        let cases: [(ColorType, u8, &[u8], bool); 12] = [
            (ColorType::Grayscale, 2, &[2], true),
            (ColorType::Grayscale, 2, &[3], false),
            (ColorType::Grayscale, 2, &[0], false),
            (ColorType::Grayscale, 8, &[2, 2, 2], false),
            (ColorType::GrayscaleAlpha, 8, &[8, 1], true),
            (ColorType::GrayscaleAlpha, 8, &[8], false),
            (ColorType::Truecolor, 8, &[5, 6, 5], true),
            (ColorType::TruecolorAlpha, 16, &[16, 16, 16, 16], true),
            (ColorType::TruecolorAlpha, 16, &[16, 16, 16], false),
            // A palette's entries are 8-bit whatever the index depth.
            (ColorType::Indexed, 4, &[8, 8, 8], true),
            (ColorType::Indexed, 4, &[9, 8, 8], false),
            (ColorType::Indexed, 8, &[8], false),
        ];
        for (color, depth, payload, ok) in cases {
            let meta = super::collect(&[(*b"sBIT", payload)], 0, &header(color, depth));
            let expected = ok.then(|| payload.to_vec());
            assert_eq!(
                meta.significant_bits, expected,
                "{color:?} at {depth} bits: {payload:?}"
            );
        }
    }

    /// Table 22's form per colour type, each sample masked to the depth (§11.3.4.1: "decoders
    /// must mask the other bits to 0") and sub-byte grey scaled to the 8 bits the decoded pixels
    /// are presented at (§13.12). Every byte is distinct, so an index slip changes a sample.
    ///
    /// Kills each arm of [`parse_bkgd`], the mask and the scale.
    #[test]
    fn bkgd_is_read_in_the_decoded_pixels_units() {
        let cases: [(ColorType, u8, &[u8], Option<Background>); 9] = [
            (ColorType::Indexed, 4, &[7], Some(Background::Index(7))),
            (ColorType::Indexed, 8, &[0, 7], None),
            // 0xFF02 masked to two bits is 2, which §13.12 presents as 2 × 85.
            (
                ColorType::Grayscale,
                2,
                &[0xFF, 0x02],
                Some(Background::Gray(170)),
            ),
            (
                ColorType::Grayscale,
                16,
                &[0x12, 0x34],
                Some(Background::Gray(0x1234)),
            ),
            (
                ColorType::GrayscaleAlpha,
                8,
                &[0x01, 0x80],
                Some(Background::Gray(0x80)),
            ),
            (ColorType::Grayscale, 8, &[1, 2, 3, 4, 5, 6], None),
            (
                ColorType::Truecolor,
                8,
                &[1, 2, 3, 4, 5, 6],
                Some(Background::Rgb(2, 4, 6)),
            ),
            (
                ColorType::TruecolorAlpha,
                16,
                &[1, 2, 3, 4, 5, 6],
                Some(Background::Rgb(0x0102, 0x0304, 0x0506)),
            ),
            (ColorType::Truecolor, 8, &[0, 2], None),
        ];
        for (color, depth, payload, expected) in cases {
            let meta = super::collect(&[(*b"bKGD", payload)], 0, &header(color, depth));
            assert_eq!(
                meta.background, expected,
                "{color:?} at {depth} bits: {payload:?}"
            );
        }
    }

    /// Table 23's layout, every byte distinct, and Table 24's two unit specifiers; a third is a
    /// value the table does not define, and a payload of the wrong length is malformed.
    ///
    /// Kills [`parse_phys`]'s field mapping and [`PhysicalUnit::from_code`].
    #[test]
    fn phys_is_read_field_for_field() {
        let mut payload = vec![1, 2, 3, 4, 5, 6, 7, 8, 0];
        let read = |payload: &[u8]| collect(&[(*b"pHYs", payload)], 0).physical_dimensions;
        assert_eq!(
            read(&payload),
            Some(PhysicalDimensions {
                x_ppu: 0x0102_0304,
                y_ppu: 0x0506_0708,
                unit: PhysicalUnit::Unknown,
            })
        );
        payload[8] = 1;
        assert_eq!(read(&payload).map(|p| p.unit), Some(PhysicalUnit::Meter));
        payload[8] = 2;
        assert_eq!(read(&payload), None, "unit 2 is undefined");
        assert_eq!(read(&payload[..8]), None, "eight bytes is not a pHYs");
    }

    /// Table 26's layout and every field's range, at both ends: in range is read, one past it is
    /// a malformed payload.
    ///
    /// Kills each bound of [`parse_time`]'s range check, one field at a time.
    #[test]
    fn time_is_read_within_table_26s_ranges() {
        let read = |payload: [u8; 7]| collect(&[(*b"tIME", &payload)], 0).modification_time;
        let [y1, y0] = 2026u16.to_be_bytes();
        assert_eq!(
            read([y1, y0, 1, 1, 0, 0, 0]),
            Some(ModificationTime {
                year: 2026,
                month: 1,
                day: 1,
                hour: 0,
                minute: 0,
                second: 0,
            })
        );
        let latest = read([y1, y0, 12, 31, 23, 59, 60]).expect("the latest of every field");
        assert_eq!(
            (
                latest.month,
                latest.day,
                latest.hour,
                latest.minute,
                latest.second
            ),
            (12, 31, 23, 59, 60)
        );
        for (field, value) in [(2, 0), (2, 13), (3, 0), (3, 32), (4, 24), (5, 60), (6, 61)] {
            let mut payload = [y1, y0, 6, 15, 12, 30, 30];
            payload[field] = value;
            assert_eq!(read(payload), None, "byte {field} = {value}");
        }
        assert_eq!(collect(&[(*b"tIME", &[0; 6])], 0).modification_time, None);
    }

    /// Each of the four is a once-only chunk, like the colour chunks: the first is kept.
    ///
    /// Kills the `is_none` guard on each of their arms in [`collect`].
    #[test]
    fn once_only_shaped_chunks_keep_the_first() {
        let phys = |x: u8| [0, 0, 0, x, 0, 0, 0, x, 1];
        let time = |second: u8| [7, 234, 1, 1, 0, 0, second];
        let meta = collect(
            &[
                (*b"sBIT", &[5, 6, 5]),
                (*b"sBIT", &[8, 8, 8]),
                (*b"bKGD", &[0, 1, 0, 2, 0, 3]),
                (*b"bKGD", &[0, 4, 0, 5, 0, 6]),
                (*b"pHYs", &phys(1)),
                (*b"pHYs", &phys(2)),
                (*b"tIME", &time(1)),
                (*b"tIME", &time(2)),
            ],
            0,
        );
        assert_eq!(meta.significant_bits.as_deref(), Some(&[5, 6, 5][..]));
        assert_eq!(meta.background, Some(Background::Rgb(1, 2, 3)));
        assert_eq!(meta.physical_dimensions.map(|p| p.x_ppu), Some(1));
        assert_eq!(meta.modification_time.map(|t| t.second), Some(1));
    }

    #[test]
    fn parses_fixed_layout_colour_chunks() {
        let meta = collect(
            &[
                (*b"gAMA", &45455u32.to_be_bytes()),
                (*b"sRGB", &[0]),
                (*b"cICP", &[9, 16, 0, 1]),
            ],
            1024,
        );
        assert_eq!(meta.gamma, Some(45455));
        assert_eq!(meta.srgb, Some(SrgbIntent::Perceptual));
        let cicp = meta.cicp.unwrap();
        assert_eq!(
            (
                cicp.color_primaries,
                cicp.transfer_function,
                cicp.matrix_coefficients
            ),
            (9, 16, 0)
        );
        assert!(cicp.full_range);
    }

    #[test]
    fn malformed_payloads_are_skipped_not_errors() {
        let meta = collect(
            &[
                (*b"gAMA", &[1, 2, 3]),        // wrong length
                (*b"sRGB", &[9]),              // undefined intent
                (*b"cICP", &[9, 16, 0, 2]),    // full-range flag out of range
                (*b"tEXt", &[0, b'x']),        // empty keyword
                (*b"iCCP", b"name\0\x01abc"),  // compression method 1
                (*b"zTXt", b"kw\0\0not-zlib"), // corrupt stream
            ],
            1024,
        );
        assert!(meta.gamma.is_none());
        assert!(meta.srgb.is_none());
        assert!(meta.cicp.is_none());
        assert!(meta.texts.is_empty());
        assert!(meta.icc_profile.is_none());
    }

    #[test]
    fn text_flavours_round_trip() {
        let ztxt: Vec<u8> = [b"Comment\0\0".to_vec(), deflated(b"compressed body")].concat();
        let itxt = b"Author\0\0\0de\0Autor\0g\xC3\xA4mut".to_vec();
        let meta = collect(
            &[
                (*b"tEXt", b"Title\0plain body"),
                (*b"zTXt", &ztxt),
                (*b"iTXt", &itxt),
            ],
            1024,
        );
        assert_eq!(meta.texts.len(), 3);
        assert_eq!(meta.texts[0].keyword, "Title");
        assert_eq!(meta.texts[0].text, "plain body");
        assert_eq!(meta.texts[1].text, "compressed body");
        assert_eq!(meta.texts[2].text, "gämut");
        assert_eq!(meta.texts[2].language.as_deref(), Some("de"));
        assert_eq!(meta.texts[2].translated_keyword.as_deref(), Some("Autor"));
    }

    #[test]
    fn xmp_is_surfaced_separately() {
        let packet = b"<x:xmpmeta/>";
        let itxt: Vec<u8> = [b"XML:com.adobe.xmp\0\0\0\0\0".to_vec(), packet.to_vec()].concat();
        let meta = collect(&[(*b"iTXt", &itxt)], 1024);
        assert_eq!(meta.xmp.as_deref(), Some(&packet[..]));
        assert!(meta.texts.is_empty());
    }

    /// The framing §11.3.3.1 Table 21 recommends — "Compression Flag set to 0, and both Language
    /// Tag and Translated Keyword set to the null string" — reads back as exactly that, so a
    /// re-encode reproduces it rather than inventing one.
    ///
    /// Kills the framing arm of [`parse_itxt`] read the other way from
    /// `xmp_framing_carries_the_compression_flag`: a mutant that reports every packet compressed,
    /// or that keeps an empty tag as `Some("")`, would rewrite a Table 21-conforming chunk as
    /// something else.
    #[test]
    fn an_unframed_xmp_packet_reads_back_unframed() {
        let itxt = b"XML:com.adobe.xmp\0\0\0\0\0<x:xmpmeta/>";
        let meta = collect(&[(*b"iTXt", itxt)], 1024);
        assert_eq!(meta.xmp_framing, Some(XmpFraming::default()));
    }

    /// §11.3.3.4's compression flag, language tag and translated keyword belong to the XMP chunk
    /// as much as to any other `iTXt`, and the packet's own field cannot hold them. Losing the
    /// flag alone rewrites a compressed packet at many times its size.
    ///
    /// Kills each field of the `ITxt::Xmp` arm of [`parse_itxt`].
    #[test]
    fn xmp_framing_carries_the_compression_flag() {
        let mut itxt = b"XML:com.adobe.xmp\0\x01\0en-GB\0Metadata\0".to_vec();
        itxt.extend_from_slice(&deflated(b"<x:xmpmeta/>"));
        let meta = collect(&[(*b"iTXt", &itxt)], 1024);
        assert_eq!(meta.xmp.as_deref(), Some(&b"<x:xmpmeta/>"[..]));
        let framing = meta.xmp_framing.expect("framed");
        assert!(framing.compressed);
        assert_eq!(framing.language.as_deref(), Some("en-GB"));
        assert_eq!(framing.translated_keyword.as_deref(), Some("Metadata"));
    }

    #[test]
    fn metadata_budget_is_cumulative_and_skips_busting_chunks() {
        let body = vec![b'a'; 600];
        let ztxt: Vec<u8> = [b"One\0\0".to_vec(), deflated(&body)].concat();
        let ztxt2: Vec<u8> = [b"Two\0\0".to_vec(), deflated(&body)].concat();
        // Budget fits one payload, not both: the second is skipped, the first survives.
        let meta = collect(&[(*b"zTXt", &ztxt), (*b"zTXt", &ztxt2)], 1000);
        assert_eq!(meta.texts.len(), 1);
        assert_eq!(meta.texts[0].keyword, "One");
    }

    #[test]
    fn once_only_chunks_keep_the_first() {
        let iccp_a: Vec<u8> = [b"a\0\0".to_vec(), deflated(b"profile-a")].concat();
        let iccp_b: Vec<u8> = [b"b\0\0".to_vec(), deflated(b"profile-b")].concat();
        let chrm_a: Vec<u8> = (1u32..=8).flat_map(|v| v.to_be_bytes()).collect();
        let chrm_b: Vec<u8> = (11u32..=18).flat_map(|v| v.to_be_bytes()).collect();
        let meta = collect(
            &[
                (*b"gAMA", &45455u32.to_be_bytes()),
                (*b"gAMA", &10000u32.to_be_bytes()),
                (*b"eXIf", b"II*\0first"),
                (*b"eXIf", b"II*\0second"),
                (*b"iCCP", &iccp_a),
                (*b"iCCP", &iccp_b),
                (*b"cHRM", &chrm_a),
                (*b"cHRM", &chrm_b),
                (*b"sRGB", &[0]),
                (*b"sRGB", &[3]),
                (*b"cICP", &[9, 16, 0, 1]),
                (*b"cICP", &[1, 13, 0, 0]),
            ],
            1024,
        );
        assert_eq!(meta.gamma, Some(45455));
        assert_eq!(meta.exif.as_deref(), Some(&b"II*\0first"[..]));
        let icc = meta.icc_profile.unwrap();
        assert_eq!(
            (icc.name.as_str(), icc.profile.as_slice()),
            ("a", &b"profile-a"[..])
        );
        assert_eq!(meta.chromaticities.unwrap().white, (1, 2));
        assert_eq!(meta.srgb, Some(SrgbIntent::Perceptual));
        assert_eq!(meta.cicp.unwrap().color_primaries, 9);
    }

    /// Exactly one store per file: the first `caBX` is the store and every later one is counted,
    /// never concatenated onto it and never promoted into its place — PNG has no multi-chunk
    /// store, unlike JPEG's APP11 run (C2PA §A.3.2).
    #[test]
    fn the_first_cabx_is_the_store_and_later_ones_are_counted_not_concatenated() {
        let meta = collect(
            &[(CABX, b"first store"), (CABX, b"second"), (CABX, b"third")],
            1024,
        );
        assert_eq!(meta.c2pa.as_deref(), Some(&b"first store"[..]));
        assert_eq!(meta.c2pa_ignored, 2);

        let single = collect(&[(CABX, b"only")], 1024);
        assert_eq!(single.c2pa.as_deref(), Some(&b"only"[..]));
        assert_eq!(single.c2pa_ignored, 0);
        assert_eq!(collect(&[], 1024).c2pa, None);
    }

    /// The count is a plain `usize`: it reports what the file carries however many that is. A
    /// saturating `u8` here read 300 ignored chunks as 255, which is a number the file does not
    /// contain.
    #[test]
    fn the_ignored_count_reports_every_chunk_not_a_saturated_ceiling() {
        let chunks: Vec<([u8; 4], &[u8])> = (0..301).map(|_| (CABX, &b"s"[..])).collect();
        let meta = collect(&chunks, 1024);
        assert_eq!(meta.c2pa_ignored, 300);
    }

    /// `caBX` is attacker-sized like every other ancillary payload, so it is charged to the one
    /// cumulative budget rather than copied beside it: a store exactly the size of the remaining
    /// budget is admitted and leaves nothing for a compressed payload after it; one byte larger
    /// is skipped — and, skipped, it is still the file's one store, so a smaller `caBX` after it
    /// is a duplicate rather than a substitute.
    #[test]
    fn the_cabx_store_is_charged_to_the_metadata_budget() {
        let ztxt: Vec<u8> = [b"kw\0\0".to_vec(), deflated(b"x")].concat();
        let store = [7u8; 10];
        let fits = collect(&[(CABX, &store), (*b"zTXt", &ztxt)], 10);
        assert_eq!(
            fits.c2pa.as_deref(),
            Some(&store[..]),
            "exactly the budget fits"
        );
        assert!(
            fits.texts.is_empty(),
            "the store consumed the budget, so the zTXt after it is skipped"
        );

        let busts = collect(&[(CABX, &store), (CABX, b"tiny")], 9);
        assert_eq!(busts.c2pa, None, "one byte over the budget is skipped");
        assert_eq!(
            busts.c2pa_ignored, 2,
            "both are ignored: the store busted the budget, and the next is not a substitute"
        );
    }

    #[test]
    fn chrm_coordinates_map_position_for_position() {
        // Every byte distinct, so any index-arithmetic slip changes some coordinate.
        let payload: Vec<u8> = (0u8..32).collect();
        let meta = collect(&[(*b"cHRM", &payload)], 0);
        let chrm = meta.chromaticities.unwrap();
        assert_eq!(chrm.white, (0x0001_0203, 0x0405_0607));
        assert_eq!(chrm.red, (0x0809_0A0B, 0x0C0D_0E0F));
        assert_eq!(chrm.green, (0x1011_1213, 0x1415_1617));
        assert_eq!(chrm.blue, (0x1819_1A1B, 0x1C1D_1E1F));
    }

    #[test]
    fn keyword_length_boundary_is_79_bytes() {
        let mut ok = vec![b'k'; 79];
        ok.push(0);
        ok.extend_from_slice(b"body");
        let meta = collect(&[(*b"tEXt", &ok)], 0);
        assert_eq!(meta.texts.len(), 1, "79-byte keyword accepted");
        assert_eq!(meta.texts[0].keyword.len(), 79);
        let mut too_long = vec![b'k'; 80];
        too_long.push(0);
        too_long.extend_from_slice(b"body");
        let meta = collect(&[(*b"tEXt", &too_long)], 0);
        assert!(meta.texts.is_empty(), "80-byte keyword skipped");
    }

    #[test]
    fn metadata_budget_decrements_by_inflated_size() {
        // Two 400-byte payloads under a 1000-byte budget must BOTH survive — the budget shrinks
        // by what was actually inflated, nothing more aggressive.
        let body = vec![b'a'; 400];
        let one: Vec<u8> = [b"One\0\0".to_vec(), deflated(&body)].concat();
        let two: Vec<u8> = [b"Two\0\0".to_vec(), deflated(&body)].concat();
        let meta = collect(&[(*b"zTXt", &one), (*b"zTXt", &two)], 1000);
        assert_eq!(meta.texts.len(), 2);
    }

    #[test]
    fn compressed_itxt_with_unknown_method_is_skipped() {
        // Compression flag 1 with method 1: undefined, so the chunk is skipped even though the
        // trailing bytes happen to be a perfectly valid zlib stream.
        let itxt: Vec<u8> = [b"Key\0\x01\x01\0\0".to_vec(), deflated(b"valid stream")].concat();
        let meta = collect(&[(*b"iTXt", &itxt)], 1024);
        assert!(meta.texts.is_empty());
    }
}
