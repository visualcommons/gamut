//! Dev-only differential oracle around a vendored, statically-linked **libpng** (v1.6.43).
//!
//! gamut-png's encoder must produce files the canonical reference reader decodes back to the same
//! pixels, and gamut-png's decoder must decode libpng-written files to the same pixels libpng
//! reads back. This crate wraps libpng (built from `third_party/libpng` against `third_party/zlib`)
//! behind a small, safe [`decode`] that returns raw samples at the file's native colour type and bit
//! depth, applying no colour transforms — exactly what a pixel-for-pixel cross-check needs — and a
//! matching [`encode`] that generates conformance fixtures (interlaced, sub-byte, ancillary-laden)
//! the gamut encoder cannot produce itself.
//!
//! All `unsafe` FFI is confined to this crate. libpng signals fatal errors through a callback that
//! must not return; for a dev oracle fed the encoder's own output (or test-constructed encode
//! parameters), an error means a real bug, so the callback prints and aborts (no `setjmp`
//! gymnastics). Corrupt-input behaviour is never tested through this oracle.

#![allow(non_upper_case_globals, non_camel_case_types, non_snake_case)]

use std::os::raw::{c_char, c_void};

mod sys {
    #![allow(dead_code)]
    include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
}

/// PNG colour type: greyscale (no alpha).
pub const COLOR_GRAY: u8 = 0;
/// PNG colour type: truecolour RGB.
pub const COLOR_RGB: u8 = 2;
/// PNG colour type: palette (indexed).
pub const COLOR_PALETTE: u8 = 3;
/// PNG colour type: greyscale with alpha.
pub const COLOR_GRAY_ALPHA: u8 = 4;
/// PNG colour type: truecolour with alpha (RGBA).
pub const COLOR_RGBA: u8 = 6;

/// libpng filter-selection mask bit: allow the None filter (`PNG_FILTER_NONE`).
pub const FILTER_NONE: u8 = 0x08;
/// libpng filter-selection mask bit: allow the Sub filter (`PNG_FILTER_SUB`).
pub const FILTER_SUB: u8 = 0x10;
/// libpng filter-selection mask bit: allow the Up filter (`PNG_FILTER_UP`).
pub const FILTER_UP: u8 = 0x20;
/// libpng filter-selection mask bit: allow the Average filter (`PNG_FILTER_AVG`).
pub const FILTER_AVG: u8 = 0x40;
/// libpng filter-selection mask bit: allow the Paeth filter (`PNG_FILTER_PAETH`).
pub const FILTER_PAETH: u8 = 0x80;
/// libpng filter-selection mask allowing every filter (`PNG_ALL_FILTERS`).
pub const FILTER_ALL: u8 = 0xF8;

/// A PNG decoded by libpng into raw samples.
pub struct DecodedImage {
    /// Image width in pixels.
    pub width: u32,
    /// Image height in pixels.
    pub height: u32,
    /// Bits per sample as stored (1, 2, 4, 8, or 16).
    pub bit_depth: u8,
    /// PNG colour-type code (one of the `COLOR_*` constants).
    pub color_type: u8,
    /// Whether the file is Adam7-interlaced (the pixels are always returned de-interlaced).
    pub interlace: bool,
    /// Bytes per row of [`pixels`](Self::pixels).
    pub rowbytes: usize,
    /// Raw sample rows, tightly packed (`rowbytes * height`). Sub-byte depths are unpacked to one
    /// byte per sample (values unscaled); 16-bit samples are big-endian, as in the file.
    pub pixels: Vec<u8>,
    /// The `bKGD` libpng **accepted**, re-serialised in the chunk's own byte layout for the file's
    /// colour type (PNG §11.3.4.1): one palette index, one big-endian 16-bit grey sample, or three
    /// big-endian 16-bit RGB samples. `None` when the file carries none *or* libpng dropped it
    /// (wrong length for the colour type, index past the palette, value past the bit depth) —
    /// [`warnings`](Self::warnings) tells the two apart.
    pub bkgd: Option<Vec<u8>>,
    /// The `sBIT` libpng **accepted**, re-serialised in the chunk's own byte layout for the file's
    /// colour type (PNG §11.3.2.4): `[grey]`, `[grey, alpha]`, `[r, g, b]` (truecolour and
    /// indexed), or `[r, g, b, alpha]`. `None` when absent or dropped, as for [`bkgd`](Self::bkgd).
    pub sbit: Option<Vec<u8>>,
    /// How many warnings libpng raised while reading the file. Under `png_set_benign_errors` an
    /// ancillary chunk libpng rejects and drops is reported here rather than aborting, so a
    /// non-zero count is how a test observes that libpng discarded something the file carried.
    pub warnings: usize,
    /// Every `tEXt`, `zTXt` and `iTXt` libpng accepted (`png_get_text`), before and after `IDAT`,
    /// in the order libpng stored them — file order.
    pub texts: Vec<OracleText>,
    /// The `iCCP` libpng accepted (`png_get_iCCP`): the profile name decoded from Latin-1
    /// (§11.3.2.3) and the **inflated** profile bytes. libpng 1.6.43 drops, with a warning, an
    /// `iCCP` chunk shorter than 92 bytes (`png_handle_iCCP` reads up to 81 keyword bytes, then
    /// wants 11 more for the zlib stream), so a fixture profile has to deflate past that to be seen
    /// here at all.
    pub iccp: Option<(String, Vec<u8>)>,
    /// The `sRGB` rendering intent libpng accepted (`png_get_sRGB`).
    pub srgb: Option<u8>,
    /// The gamma libpng reports (`png_get_gAMA_fixed`), × 100 000. libpng's colour-space model
    /// reports one for an `sRGB` chunk too (its 45 455), so a value here does not by itself say the
    /// file carried a `gAMA` chunk.
    pub gamma: Option<u32>,
    /// The chromaticities libpng reports (`png_get_cHRM_fixed`) as white, red, green, blue x,y
    /// pairs × 100 000 — the order [`EncodeOpts::chromaticities`] takes. As for
    /// [`gamma`](Self::gamma), libpng also reports the sRGB primaries for an `sRGB` chunk.
    pub chromaticities: Option<[u32; 8]>,
    /// The `eXIf` payload libpng accepted (`png_get_eXIf_1`), verbatim.
    pub exif: Option<Vec<u8>>,
    /// The chunks of a type in [`KEPT_UNKNOWN_CHUNKS`] (`png_get_unknown_chunks`), as
    /// `(type, payload)` in file order. libpng 1.6.43 predates both, so it reads them as unknown
    /// chunks and keeps their payloads verbatim.
    pub unknown_chunks: Vec<([u8; 4], Vec<u8>)>,
}

/// The chunk types [`decode`] asks libpng to keep as unknown chunks, which this libpng does not
/// parse itself: `cICP` (PNG 3rd edition §11.3.2.6) and the C2PA manifest store `caBX`.
pub const KEPT_UNKNOWN_CHUNKS: [[u8; 4]; 2] = [*b"cICP", *b"caBX"];

/// One text chunk as libpng read it back (see [`DecodedImage::texts`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OracleText {
    /// The keyword, decoded from Latin-1 (§11.3.3.1).
    pub keyword: String,
    /// The text: decoded from Latin-1 for `tEXt`/`zTXt` (§11.3.3.2), from UTF-8 for `iTXt`
    /// (§11.3.3.4). For a compressed chunk this is the inflated text.
    pub text: String,
    /// Which chunk carried it, from libpng's `compression` field.
    pub kind: OracleTextKind,
}

/// Which chunk an [`OracleText`] came from — libpng's `png_text::compression`, with the `iTXt`
/// fields it carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OracleTextKind {
    /// `tEXt` (`PNG_TEXT_COMPRESSION_NONE`).
    Text,
    /// `zTXt` (`PNG_TEXT_COMPRESSION_zTXt`).
    ZTxt,
    /// `iTXt` (`PNG_ITXT_COMPRESSION_NONE` or `_zTXt`).
    ITxt {
        /// The language tag (may be empty).
        language: String,
        /// The translated keyword, decoded from UTF-8 (may be empty).
        translated: String,
        /// Whether the text was stored compressed (`PNG_ITXT_COMPRESSION_zTXt`).
        compressed: bool,
    },
}

/// Cursor over the in-memory PNG, handed to libpng's read callback via its IO pointer.
struct ReadCursor {
    data: *const u8,
    len: usize,
    pos: usize,
}

unsafe extern "C" fn read_callback(png: sys::png_structp, out: sys::png_bytep, count: usize) {
    unsafe {
        let cursor = &mut *(sys::png_get_io_ptr(png) as *mut ReadCursor);
        if cursor.pos + count > cursor.len {
            eprintln!("libpng-oracle: read past end of input");
            std::process::abort();
        }
        std::ptr::copy_nonoverlapping(cursor.data.add(cursor.pos), out, count);
        cursor.pos += count;
    }
}

unsafe extern "C" fn error_callback(_png: sys::png_structp, msg: sys::png_const_charp) {
    let text = if msg.is_null() {
        "unknown".to_string()
    } else {
        unsafe { std::ffi::CStr::from_ptr(msg).to_string_lossy().into_owned() }
    };
    eprintln!("libpng-oracle: fatal libpng error: {text}");
    std::process::abort();
}

/// Counts each warning into the `usize` the read struct's error pointer names; a struct created
/// with a null error pointer (the encoder's) discards them.
unsafe extern "C" fn warn_callback(png: sys::png_structp, _msg: sys::png_const_charp) {
    unsafe {
        let count = sys::png_get_error_ptr(png).cast::<usize>();
        if !count.is_null() {
            *count += 1;
        }
    }
}

/// The accepted `bKGD` in its chunk layout for `color_type` (see [`DecodedImage::bkgd`]).
unsafe fn read_bkgd(
    png: sys::png_structp,
    info: sys::png_infop,
    color_type: u8,
) -> Option<Vec<u8>> {
    let mut background: sys::png_color_16p = std::ptr::null_mut();
    if unsafe { sys::png_get_bKGD(png, info, &raw mut background) } == 0 || background.is_null() {
        return None;
    }
    let background = unsafe { *background };
    Some(match color_type {
        COLOR_PALETTE => vec![background.index],
        COLOR_GRAY | COLOR_GRAY_ALPHA => background.gray.to_be_bytes().to_vec(),
        _ => [background.red, background.green, background.blue]
            .iter()
            .flat_map(|sample| sample.to_be_bytes())
            .collect(),
    })
}

/// The accepted `sBIT` in its chunk layout for `color_type` (see [`DecodedImage::sbit`]).
unsafe fn read_sbit(
    png: sys::png_structp,
    info: sys::png_infop,
    color_type: u8,
) -> Option<Vec<u8>> {
    let mut bits: sys::png_color_8p = std::ptr::null_mut();
    if unsafe { sys::png_get_sBIT(png, info, &raw mut bits) } == 0 || bits.is_null() {
        return None;
    }
    let bits = unsafe { *bits };
    Some(match color_type {
        COLOR_GRAY => vec![bits.gray],
        COLOR_GRAY_ALPHA => vec![bits.gray, bits.alpha],
        COLOR_RGB | COLOR_PALETTE => vec![bits.red, bits.green, bits.blue],
        _ => vec![bits.red, bits.green, bits.blue, bits.alpha],
    })
}

/// Decodes Latin-1 bytes: each byte is the code point of the same value.
fn from_latin1(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| char::from(b)).collect()
}

/// Decodes UTF-8 bytes, panicking (naming `what`) on bytes that are not UTF-8: a lossy decode
/// would hide exactly the encoding defect a read-back exists to show.
fn from_utf8(bytes: &[u8], what: &str) -> String {
    String::from_utf8(bytes.to_vec())
        .unwrap_or_else(|e| panic!("libpng-oracle: {what} {bytes:?} is not UTF-8: {e}"))
}

/// The `len` bytes at `ptr`, or none for a null pointer.
unsafe fn bytes_at<'a>(ptr: *const c_char, len: usize) -> &'a [u8] {
    if ptr.is_null() {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(ptr.cast::<u8>(), len) }
    }
}

/// The NUL-terminated string at `ptr`, or none for a null pointer.
unsafe fn c_bytes<'a>(ptr: *const c_char) -> &'a [u8] {
    if ptr.is_null() {
        &[]
    } else {
        unsafe { std::ffi::CStr::from_ptr(ptr).to_bytes() }
    }
}

/// Every text chunk libpng stored in `info` (see [`DecodedImage::texts`]).
unsafe fn read_texts(png: sys::png_structp, info: sys::png_infop) -> Vec<OracleText> {
    let mut texts: sys::png_textp = std::ptr::null_mut();
    let mut count = 0i32;
    unsafe { sys::png_get_text(png, info, &raw mut texts, &raw mut count) };
    if texts.is_null() {
        return Vec::new();
    }
    let texts = unsafe { std::slice::from_raw_parts(texts, count as usize) };
    texts
        .iter()
        .map(|t| unsafe {
            let keyword = from_latin1(c_bytes(t.key));
            let itxt = |compressed| OracleTextKind::ITxt {
                language: from_latin1(c_bytes(t.lang)),
                translated: from_utf8(c_bytes(t.lang_key), "iTXt translated keyword"),
                compressed,
            };
            match t.compression {
                sys::PNG_TEXT_COMPRESSION_NONE => OracleText {
                    keyword,
                    text: from_latin1(bytes_at(t.text, t.text_length)),
                    kind: OracleTextKind::Text,
                },
                c if c == sys::PNG_TEXT_COMPRESSION_zTXt as i32 => OracleText {
                    keyword,
                    text: from_latin1(bytes_at(t.text, t.text_length)),
                    kind: OracleTextKind::ZTxt,
                },
                c => OracleText {
                    keyword,
                    text: from_utf8(bytes_at(t.text, t.itxt_length), "iTXt text"),
                    kind: itxt(c == sys::PNG_ITXT_COMPRESSION_zTXt as i32),
                },
            }
        })
        .collect()
}

/// The accepted `iCCP` (see [`DecodedImage::iccp`]).
unsafe fn read_iccp(png: sys::png_structp, info: sys::png_infop) -> Option<(String, Vec<u8>)> {
    let mut name: sys::png_charp = std::ptr::null_mut();
    let mut compression = 0i32;
    let mut profile: sys::png_bytep = std::ptr::null_mut();
    let mut len = 0u32;
    let valid = unsafe {
        sys::png_get_iCCP(
            png,
            info,
            &raw mut name,
            &raw mut compression,
            &raw mut profile,
            &raw mut len,
        )
    };
    (valid != 0).then(|| unsafe {
        (
            from_latin1(c_bytes(name)),
            bytes_at(profile.cast(), len as usize).to_vec(),
        )
    })
}

/// The accepted `cHRM`, white/red/green/blue x,y × 100 000 (see [`DecodedImage::chromaticities`]).
unsafe fn read_chrm(png: sys::png_structp, info: sys::png_infop) -> Option<[u32; 8]> {
    let mut c = [0 as sys::png_fixed_point; 8];
    let [wx, wy, rx, ry, gx, gy, bx, by] = &mut c;
    let valid = unsafe { sys::png_get_cHRM_fixed(png, info, wx, wy, rx, ry, gx, gy, bx, by) };
    (valid != 0).then(|| c.map(|v| v as u32))
}

/// The kept unknown chunks (see [`DecodedImage::unknown_chunks`]).
unsafe fn read_unknown(png: sys::png_structp, info: sys::png_infop) -> Vec<([u8; 4], Vec<u8>)> {
    let mut chunks: sys::png_unknown_chunkp = std::ptr::null_mut();
    let count = unsafe { sys::png_get_unknown_chunks(png, info, &raw mut chunks) };
    if chunks.is_null() {
        return Vec::new();
    }
    unsafe { std::slice::from_raw_parts(chunks, count as usize) }
        .iter()
        .map(|c| {
            let ty = [c.name[0], c.name[1], c.name[2], c.name[3]];
            (ty, unsafe { bytes_at(c.data.cast(), c.size) }.to_vec())
        })
        .collect()
}

/// Decodes a PNG byte stream with libpng into raw samples (no colour transforms).
///
/// Aborts the process if libpng reports the stream is malformed — for this dev oracle the input is
/// always the encoder's own output, so a failure is a genuine bug to surface loudly.
#[must_use]
pub fn decode(bytes: &[u8]) -> DecodedImage {
    // Outlives the read struct, whose error pointer names it for `warn_callback`.
    let mut warnings = 0usize;
    unsafe {
        let version = sys::PNG_LIBPNG_VER_STRING.as_ptr() as *const c_char;
        let png = sys::png_create_read_struct(
            version,
            (&raw mut warnings).cast::<c_void>(),
            Some(error_callback),
            Some(warn_callback),
        );
        assert!(!png.is_null(), "png_create_read_struct failed");
        let mut info = sys::png_create_info_struct(png);
        assert!(!info.is_null(), "png_create_info_struct failed");

        let mut cursor = ReadCursor {
            data: bytes.as_ptr(),
            len: bytes.len(),
            pos: 0,
        };
        sys::png_set_read_fn(png, (&raw mut cursor).cast::<c_void>(), Some(read_callback));
        // Treat recoverable (benign) errors in ancillary chunks as warnings: gamut-png frames
        // metadata chunks (eXIf/iCCP/...) but does not validate their payloads, so an oracle
        // checking the *image* should not abort on third-party metadata content. Critical errors
        // (IHDR, IDAT, CRC) still abort.
        sys::png_set_benign_errors(png, 1);
        // Keep the chunk types this libpng cannot parse, as 5-byte NUL-terminated names.
        let kept: Vec<u8> = KEPT_UNKNOWN_CHUNKS
            .iter()
            .flat_map(|ty| ty.iter().copied().chain([0]))
            .collect();
        sys::png_set_keep_unknown_chunks(
            png,
            sys::PNG_HANDLE_CHUNK_ALWAYS as i32,
            kept.as_ptr(),
            KEPT_UNKNOWN_CHUNKS.len() as i32,
        );
        sys::png_read_info(png, info);

        let width = sys::png_get_image_width(png, info);
        let height = sys::png_get_image_height(png, info);
        let bit_depth = sys::png_get_bit_depth(png, info) as u8;
        let color_type = sys::png_get_color_type(png, info) as u8;
        let interlace = sys::png_get_interlace_type(png, info) != 0;
        // Both chunks precede IDAT (§5.6), so png_read_info has already accepted or dropped them.
        let bkgd = read_bkgd(png, info, color_type);
        let sbit = read_sbit(png, info, color_type);

        // Unpack 1/2/4-bit samples to one byte each (values left unscaled); leave 16-bit big-endian.
        if bit_depth < 8 {
            sys::png_set_packing(png);
        }
        sys::png_set_interlace_handling(png); // de-interlace within png_read_image (1 pass if none)
        sys::png_read_update_info(png, info);

        let rowbytes = sys::png_get_rowbytes(png, info) as usize;
        let mut pixels = vec![0u8; rowbytes * height as usize];
        let mut rows: Vec<sys::png_bytep> = (0..height as usize)
            .map(|y| pixels.as_mut_ptr().add(y * rowbytes))
            .collect();
        sys::png_read_image(png, rows.as_mut_ptr());
        // Into `info`, so text and kept chunks after IDAT are stored beside those before it.
        sys::png_read_end(png, info);

        let texts = read_texts(png, info);
        let iccp = read_iccp(png, info);
        let mut intent = 0i32;
        let srgb = (sys::png_get_sRGB(png, info, &raw mut intent) != 0).then_some(intent as u8);
        let mut gamma_fixed: sys::png_fixed_point = 0;
        let gamma = (sys::png_get_gAMA_fixed(png, info, &raw mut gamma_fixed) != 0)
            .then_some(gamma_fixed as u32);
        let chromaticities = read_chrm(png, info);
        let mut exif_ptr: sys::png_bytep = std::ptr::null_mut();
        let mut exif_len = 0u32;
        let exif = (sys::png_get_eXIf_1(png, info, &raw mut exif_len, &raw mut exif_ptr) != 0)
            .then(|| bytes_at(exif_ptr.cast(), exif_len as usize).to_vec());
        let unknown_chunks = read_unknown(png, info);

        let mut png = png;
        sys::png_destroy_read_struct(&raw mut png, &raw mut info, std::ptr::null_mut());

        DecodedImage {
            width,
            height,
            bit_depth,
            color_type,
            interlace,
            rowbytes,
            pixels,
            bkgd,
            sbit,
            warnings,
            texts,
            iccp,
            srgb,
            gamma,
            chromaticities,
            exif,
            unknown_chunks,
        }
    }
}

/// How a [`TextChunk`] is stored in the file.
pub enum TextKind<'a> {
    /// Latin-1 `tEXt`: [`encode`] transcodes [`TextChunk::text`] to ISO 8859-1 (PNG §11.3.3.2).
    Text,
    /// zlib-compressed Latin-1 `zTXt`: transcoded like [`TextKind::Text`] (PNG §11.3.3.3).
    ZTxt,
    /// UTF-8 `iTXt` with a language tag and translated keyword (either may be empty); the text
    /// and translated keyword are written as UTF-8 unchanged (PNG §11.3.3.4).
    ITxt {
        /// RFC 3066 language tag (may be empty).
        language: &'a str,
        /// Keyword translated into `language` (may be empty).
        translated: &'a str,
        /// Whether the text payload is zlib-compressed.
        compressed: bool,
    },
}

/// One text chunk for [`EncodeOpts::text`].
///
/// Fields are Rust (UTF-8) strings; [`encode`] writes each in the character set its chunk
/// carries, and panics on a character that set cannot represent rather than writing its UTF-8
/// bytes, which a conforming reader would render as different Latin-1 characters.
pub struct TextChunk<'a> {
    /// The registered or ad-hoc keyword (1–79 Latin-1 characters in every text chunk kind,
    /// PNG §11.3.3.1); transcoded to ISO 8859-1.
    pub keyword: &'a str,
    /// The text payload: transcoded to ISO 8859-1 for `tEXt`/`zTXt`, UTF-8 for `iTXt`.
    pub text: &'a str,
    /// The chunk flavour to write.
    pub kind: TextKind<'a>,
}

/// Options for [`encode`]; the default writes a bare image with libpng's own choices.
#[derive(Default)]
pub struct EncodeOpts<'a> {
    /// Write the image Adam7-interlaced.
    pub interlace: bool,
    /// Restrict libpng's per-row filter choice to this `FILTER_*` mask.
    pub filters: Option<u8>,
    /// zlib compression level (0–9).
    pub compression_level: Option<i32>,
    /// PLTE entries; required for colour type 3, a suggested palette otherwise.
    pub palette: Option<&'a [[u8; 3]]>,
    /// Per-palette-entry tRNS alpha values (may be shorter than the palette).
    pub trns_palette: Option<&'a [u8]>,
    /// tRNS colour key for greyscale images, in native sample range.
    pub trns_gray: Option<u16>,
    /// tRNS colour key for RGB images, in native sample range.
    pub trns_rgb: Option<[u16; 3]>,
    /// gAMA value × 100 000.
    pub gamma: Option<u32>,
    /// cHRM white/red/green/blue x,y pairs, each × 100 000.
    pub chromaticities: Option<[u32; 8]>,
    /// sRGB rendering intent (0–3).
    pub srgb_intent: Option<i32>,
    /// eXIf payload (a TIFF stream), written verbatim.
    pub exif: Option<&'a [u8]>,
    /// iCCP profile as (name, raw ICC bytes); libpng deflates the profile itself.
    pub icc: Option<(&'a str, &'a [u8])>,
    /// Text chunks to embed.
    pub text: &'a [TextChunk<'a>],
    /// Extra raw chunks written verbatim (type, payload) right after IHDR — used for chunk types
    /// this libpng predates (e.g. cICP). libpng frames them and computes the CRC.
    pub extra_chunks: &'a [([u8; 4], &'a [u8])],
}

/// Channels per pixel for a PNG colour-type code.
fn channels(color_type: u8) -> usize {
    match color_type {
        COLOR_GRAY | COLOR_PALETTE => 1,
        COLOR_GRAY_ALPHA => 2,
        COLOR_RGB => 3,
        COLOR_RGBA => 4,
        other => panic!("libpng-oracle: invalid colour type {other}"),
    }
}

/// Transcodes `s` to ISO 8859-1, one byte per `char` (U+0000–U+00FF map to themselves).
///
/// Panics, naming `what`, on a character Latin-1 cannot represent: writing its UTF-8 bytes
/// instead would yield a chunk a conforming reader decodes to different text.
fn latin1(s: &str, what: &str) -> Vec<u8> {
    s.chars()
        .map(|c| {
            u8::try_from(c).unwrap_or_else(|_| {
                panic!(
                    "libpng-oracle: {what} {s:?} holds {c:?} (U+{:04X}), outside Latin-1",
                    u32::from(c)
                )
            })
        })
        .collect()
}

/// A [`TextChunk`]'s strings as NUL-terminated bytes in the character sets its chunk carries:
/// keyword, text, language tag, translated keyword.
fn text_strings(
    t: &TextChunk,
) -> (
    std::ffi::CString,
    std::ffi::CString,
    std::ffi::CString,
    std::ffi::CString,
) {
    let (text, language, translated) = match t.kind {
        TextKind::Text | TextKind::ZTxt => (latin1(t.text, "tEXt/zTXt text"), "", ""),
        TextKind::ITxt {
            language,
            translated,
            ..
        } => (t.text.as_bytes().to_vec(), language, translated),
    };
    (
        std::ffi::CString::new(latin1(t.keyword, "keyword")).expect("keyword without NUL"),
        std::ffi::CString::new(text).expect("text without NUL"),
        std::ffi::CString::new(language).expect("language without NUL"),
        std::ffi::CString::new(translated).expect("translated without NUL"),
    )
}

unsafe extern "C" fn write_callback(png: sys::png_structp, data: sys::png_bytep, count: usize) {
    unsafe {
        let out = &mut *(sys::png_get_io_ptr(png) as *mut Vec<u8>);
        out.extend_from_slice(std::slice::from_raw_parts(data, count));
    }
}

unsafe extern "C" fn flush_callback(_png: sys::png_structp) {}

/// Encodes raw samples into a PNG with libpng — the fixture generator for gamut-png's *decoder*
/// differential tests, able to produce streams the gamut encoder cannot (Adam7 interlace, forced
/// filters, arbitrary extra chunks).
///
/// `pixels` uses the same layout [`decode`] returns: one byte per sample for bit depths < 8
/// (values unscaled; libpng packs them), big-endian byte pairs for depth 16, rows tightly packed.
///
/// Aborts the process on any libpng error: the inputs are always test-constructed valid
/// parameters, so a failure is a harness bug to surface loudly.
#[must_use]
pub fn encode(
    pixels: &[u8],
    width: u32,
    height: u32,
    color_type: u8,
    bit_depth: u8,
    opts: &EncodeOpts,
) -> Vec<u8> {
    let bytes_per_sample = if bit_depth == 16 { 2 } else { 1 };
    let rowbytes = width as usize * channels(color_type) * bytes_per_sample;
    assert_eq!(
        pixels.len(),
        rowbytes * height as usize,
        "pixel buffer does not match {width}x{height} colour type {color_type} depth {bit_depth}"
    );
    if color_type == COLOR_PALETTE {
        assert!(
            opts.palette.is_some(),
            "palette images need EncodeOpts::palette"
        );
    }
    // Transcoded before libpng is touched, so an unrepresentable character panics cleanly. The
    // CStrings live until after png_set_text, which copies everything into `info`.
    let text_owned: Vec<_> = opts.text.iter().map(text_strings).collect();

    unsafe {
        let version = sys::PNG_LIBPNG_VER_STRING.as_ptr() as *const c_char;
        let png = sys::png_create_write_struct(
            version,
            std::ptr::null_mut(),
            Some(error_callback),
            Some(warn_callback),
        );
        assert!(!png.is_null(), "png_create_write_struct failed");
        let mut info = sys::png_create_info_struct(png);
        assert!(!info.is_null(), "png_create_info_struct failed");

        let mut out: Vec<u8> = Vec::new();
        sys::png_set_write_fn(
            png,
            (&raw mut out).cast::<c_void>(),
            Some(write_callback),
            Some(flush_callback),
        );

        sys::png_set_IHDR(
            png,
            info,
            width,
            height,
            i32::from(bit_depth),
            i32::from(color_type),
            if opts.interlace {
                sys::PNG_INTERLACE_ADAM7 as i32
            } else {
                sys::PNG_INTERLACE_NONE as i32
            },
            sys::PNG_COMPRESSION_TYPE_BASE as i32,
            sys::PNG_FILTER_TYPE_BASE as i32,
        );
        if let Some(mask) = opts.filters {
            sys::png_set_filter(png, sys::PNG_FILTER_TYPE_BASE as i32, i32::from(mask));
        }
        if let Some(level) = opts.compression_level {
            sys::png_set_compression_level(png, level);
        }

        // png_set_* below copy their arguments into `info`, so temporaries may drop afterwards.
        if let Some(palette) = opts.palette {
            let colors: Vec<sys::png_color> = palette
                .iter()
                .map(|&[red, green, blue]| sys::png_color { red, green, blue })
                .collect();
            sys::png_set_PLTE(png, info, colors.as_ptr(), colors.len() as i32);
        }
        if let Some(alphas) = opts.trns_palette {
            sys::png_set_tRNS(
                png,
                info,
                alphas.as_ptr(),
                alphas.len() as i32,
                std::ptr::null(),
            );
        }
        if opts.trns_gray.is_some() || opts.trns_rgb.is_some() {
            let [red, green, blue] = opts.trns_rgb.unwrap_or_default();
            let key = sys::png_color_16 {
                index: 0,
                red,
                green,
                blue,
                gray: opts.trns_gray.unwrap_or_default(),
            };
            sys::png_set_tRNS(png, info, std::ptr::null(), 0, &raw const key);
        }
        if let Some(gamma) = opts.gamma {
            sys::png_set_gAMA_fixed(png, info, gamma as sys::png_fixed_point);
        }
        if let Some([wx, wy, rx, ry, gx, gy, bx, by]) = opts.chromaticities {
            sys::png_set_cHRM_fixed(
                png,
                info,
                wx as sys::png_fixed_point,
                wy as sys::png_fixed_point,
                rx as sys::png_fixed_point,
                ry as sys::png_fixed_point,
                gx as sys::png_fixed_point,
                gy as sys::png_fixed_point,
                bx as sys::png_fixed_point,
                by as sys::png_fixed_point,
            );
        }
        if let Some(intent) = opts.srgb_intent {
            sys::png_set_sRGB(png, info, intent);
        }
        if let Some(exif) = opts.exif {
            sys::png_set_eXIf_1(png, info, exif.len() as u32, exif.as_ptr().cast_mut());
        }
        if let Some((name, profile)) = opts.icc {
            let name = std::ffi::CString::new(name).expect("iCCP name without NUL");
            sys::png_set_iCCP(
                png,
                info,
                name.as_ptr(),
                sys::PNG_COMPRESSION_TYPE_BASE as i32,
                profile.as_ptr(),
                profile.len() as u32,
            );
        }
        if !opts.text.is_empty() {
            let texts: Vec<sys::png_text> = opts
                .text
                .iter()
                .zip(&text_owned)
                .map(|(t, (key, text, lang, lang_key))| {
                    let (compression, text_length, itxt_length, lang_ptr, lang_key_ptr) =
                        match t.kind {
                            TextKind::Text => (
                                sys::PNG_TEXT_COMPRESSION_NONE,
                                text.as_bytes().len(),
                                0,
                                std::ptr::null_mut(),
                                std::ptr::null_mut(),
                            ),
                            TextKind::ZTxt => (
                                sys::PNG_TEXT_COMPRESSION_zTXt as i32,
                                text.as_bytes().len(),
                                0,
                                std::ptr::null_mut(),
                                std::ptr::null_mut(),
                            ),
                            TextKind::ITxt { compressed, .. } => (
                                if compressed {
                                    sys::PNG_ITXT_COMPRESSION_zTXt as i32
                                } else {
                                    sys::PNG_ITXT_COMPRESSION_NONE as i32
                                },
                                0,
                                text.as_bytes().len(),
                                lang.as_ptr().cast_mut(),
                                lang_key.as_ptr().cast_mut(),
                            ),
                        };
                    sys::png_text {
                        compression,
                        key: key.as_ptr().cast_mut(),
                        text: text.as_ptr().cast_mut(),
                        text_length,
                        itxt_length,
                        lang: lang_ptr,
                        lang_key: lang_key_ptr,
                    }
                })
                .collect();
            sys::png_set_text(png, info, texts.as_ptr(), texts.len() as i32);
        }
        if !opts.extra_chunks.is_empty() {
            // Unsafe-to-copy unknown chunks are only written when the default handling is ALWAYS.
            sys::png_set_keep_unknown_chunks(
                png,
                sys::PNG_HANDLE_CHUNK_ALWAYS as i32,
                std::ptr::null(),
                0,
            );
            let unknowns: Vec<sys::png_unknown_chunk> = opts
                .extra_chunks
                .iter()
                .map(|(ty, data)| {
                    let mut name = [0u8; 5];
                    name[..4].copy_from_slice(ty);
                    sys::png_unknown_chunk {
                        name,
                        data: data.as_ptr().cast_mut(),
                        size: data.len(),
                        location: sys::PNG_HAVE_IHDR as u8,
                    }
                })
                .collect();
            sys::png_set_unknown_chunks(png, info, unknowns.as_ptr(), unknowns.len() as i32);
        }

        sys::png_write_info(png, info);
        if bit_depth < 8 {
            sys::png_set_packing(png); // callers pass one byte per sample; libpng packs on write
        }
        let mut samples = pixels.to_vec(); // libpng wants mutable row pointers
        let mut rows: Vec<sys::png_bytep> = (0..height as usize)
            .map(|y| samples.as_mut_ptr().add(y * rowbytes))
            .collect();
        sys::png_write_image(png, rows.as_mut_ptr()); // handles Adam7 internally
        sys::png_write_end(png, info);

        let mut png = png;
        sys::png_destroy_write_struct(&raw mut png, &raw mut info);
        out
    }
}

/// Decodes a PNG to 8-bit RGBA via libpng's simplified API, resolving palette and tRNS to actual
/// colours. Returns `(width, height, rgba)`. Useful for verifying that palette entries and
/// transparency resolve to the colours the encoder intended.
#[must_use]
pub fn decode_rgba8(bytes: &[u8]) -> (u32, u32, Vec<u8>) {
    unsafe {
        let mut image: sys::png_image = std::mem::zeroed();
        image.version = sys::PNG_IMAGE_VERSION;
        let ok = sys::png_image_begin_read_from_memory(
            &raw mut image,
            bytes.as_ptr().cast::<c_void>(),
            bytes.len(),
        );
        assert!(ok != 0, "png_image_begin_read_from_memory failed");
        image.format = sys::PNG_FORMAT_RGBA;
        let (width, height) = (image.width, image.height);
        let mut rgba = vec![0u8; width as usize * height as usize * 4];
        let ok = sys::png_image_finish_read(
            &raw mut image,
            std::ptr::null(),
            rgba.as_mut_ptr().cast::<c_void>(),
            0, // row stride: 0 = packed (width * 4)
            std::ptr::null_mut(),
        );
        assert!(ok != 0, "png_image_finish_read failed");
        (width, height, rgba)
    }
}

/// The libpng version number the oracle links against (e.g. `10643` for 1.6.43).
#[must_use]
pub fn version() -> u32 {
    unsafe { sys::png_access_version_number() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_libpng_1_6() {
        // Confirms the static lib, bindgen FFI, and link line are all wired correctly.
        assert!(
            version() >= 10600,
            "expected libpng 1.6.x, got {}",
            version()
        );
    }

    #[test]
    fn encode_round_trips_a_4bit_interlaced_palette_image() {
        // Proves the write-side FFI end to end before gamut-png builds fixtures on it: sub-byte
        // packing, Adam7, PLTE/tRNS, and an unknown chunk all survive an encode → decode round
        // trip through libpng itself.
        let (w, h) = (11u32, 6u32);
        let palette: Vec<[u8; 3]> = (0..16).map(|i| [i * 16, 255 - i * 16, i]).collect();
        let indices: Vec<u8> = (0..(w * h) as usize).map(|i| (i % 16) as u8).collect();
        let cicp = [9u8, 16, 0, 1];
        let png = encode(
            &indices,
            w,
            h,
            COLOR_PALETTE,
            4,
            &EncodeOpts {
                interlace: true,
                palette: Some(&palette),
                trns_palette: Some(&[0, 128, 255]),
                extra_chunks: &[(*b"cICP", &cicp)],
                ..EncodeOpts::default()
            },
        );

        let dec = decode(&png);
        assert_eq!((dec.width, dec.height), (w, h));
        assert_eq!((dec.color_type, dec.bit_depth), (COLOR_PALETTE, 4));
        assert!(dec.interlace);
        assert_eq!(dec.pixels, indices);

        // The unknown chunk was framed verbatim right after IHDR.
        let ihdr_end = 8 + 12 + 13; // signature + framed 13-byte IHDR
        assert_eq!(&png[ihdr_end + 4..ihdr_end + 8], b"cICP");
        assert_eq!(&png[ihdr_end + 8..ihdr_end + 12], &cicp);
    }

    /// The payload of the first `ty` chunk in `png`.
    fn chunk_payload<'p>(png: &'p [u8], ty: &[u8; 4]) -> &'p [u8] {
        let mut rest = &png[8..];
        loop {
            let len = u32::from_be_bytes([rest[0], rest[1], rest[2], rest[3]]) as usize;
            if &rest[4..8] == ty {
                return &rest[8..8 + len];
            }
            assert_ne!(&rest[4..8], b"IEND", "no {ty:?} chunk");
            rest = &rest[12 + len..];
        }
    }

    /// A 1x1 greyscale PNG carrying `text`.
    fn with_text(text: &[TextChunk]) -> Vec<u8> {
        encode(
            &[0],
            1,
            1,
            COLOR_GRAY,
            8,
            &EncodeOpts {
                text,
                ..EncodeOpts::default()
            },
        )
    }

    #[test]
    fn text_keyword_and_payload_are_written_as_latin1() {
        let png = with_text(&[TextChunk {
            keyword: "Café",
            text: "naïve ÿ",
            kind: TextKind::Text,
        }]);
        assert_eq!(chunk_payload(&png, b"tEXt"), b"Caf\xE9\0na\xEFve \xFF");
    }

    #[test]
    fn ztxt_keyword_is_written_as_latin1() {
        let png = with_text(&[TextChunk {
            keyword: "Café",
            text: "é",
            kind: TextKind::ZTxt,
        }]);
        // Keyword, NUL separator, compression method 0, then the zlib stream.
        assert_eq!(&chunk_payload(&png, b"zTXt")[..6], b"Caf\xE9\0\0");
    }

    #[test]
    fn itxt_keeps_utf8_text_but_latin1_keyword() {
        let png = with_text(&[TextChunk {
            keyword: "Café",
            text: "é€",
            kind: TextKind::ITxt {
                language: "fr",
                translated: "Café",
                compressed: false,
            },
        }]);
        assert_eq!(
            chunk_payload(&png, b"iTXt"),
            b"Caf\xE9\0\0\0fr\0Caf\xC3\xA9\0\xC3\xA9\xE2\x82\xAC"
        );
    }

    #[test]
    #[should_panic(expected = "'€' (U+20AC), outside Latin-1")]
    fn a_text_character_outside_latin1_panics() {
        let _ = with_text(&[TextChunk {
            keyword: "Title",
            text: "5 €",
            kind: TextKind::ZTxt,
        }]);
    }

    #[test]
    #[should_panic(expected = "keyword \"Tıtle\" holds 'ı' (U+0131), outside Latin-1")]
    fn a_keyword_character_outside_latin1_panics_for_itxt_too() {
        let _ = with_text(&[TextChunk {
            keyword: "Tıtle",
            text: "fine",
            kind: TextKind::ITxt {
                language: "",
                translated: "",
                compressed: false,
            },
        }]);
    }

    /// CRC-32 (ISO 3309, as PNG §5.3 uses it) over `bytes`, bitwise.
    fn crc32(bytes: &[u8]) -> u32 {
        !bytes.iter().fold(!0u32, |crc, &byte| {
            (0..8).fold(crc ^ u32::from(byte), |c, _| {
                if c & 1 == 1 {
                    (c >> 1) ^ 0xEDB8_8320
                } else {
                    c >> 1
                }
            })
        })
    }

    /// A 2x2 image of `color_type` (8-bit; a two-entry palette for indexed) with the given
    /// ancillary chunks framed right after IHDR — where `bKGD`/`sBIT` may sit for every colour
    /// type except indexed, whose `bKGD` must follow PLTE, so callers pass PLTE first there.
    fn with_chunks(color_type: u8, chunks: &[(&[u8; 4], &[u8])]) -> Vec<u8> {
        let palette = [[0u8, 0, 0], [255, 255, 255]];
        let samples = vec![0u8; 4 * channels(color_type)];
        let bare = encode(
            &samples,
            2,
            2,
            color_type,
            8,
            &EncodeOpts {
                palette: (color_type == COLOR_PALETTE).then_some(&palette[..]),
                ..EncodeOpts::default()
            },
        );
        let ihdr_end = 8 + 12 + 13;
        let mut png = bare[..ihdr_end].to_vec();
        for (ty, data) in chunks {
            png.extend_from_slice(&(data.len() as u32).to_be_bytes());
            let start = png.len();
            png.extend_from_slice(*ty);
            png.extend_from_slice(data);
            let crc = crc32(&png[start..]);
            png.extend_from_slice(&crc.to_be_bytes());
        }
        // An indexed bare file already carries its PLTE; skip it when the caller framed one.
        let mut rest = &bare[ihdr_end..];
        if chunks.iter().any(|(ty, _)| *ty == b"PLTE") {
            let len = u32::from_be_bytes([rest[0], rest[1], rest[2], rest[3]]) as usize;
            assert_eq!(&rest[4..8], b"PLTE");
            rest = &rest[12 + len..];
        }
        png.extend_from_slice(rest);
        png
    }

    #[test]
    fn accepted_bkgd_and_sbit_come_back_in_chunk_layout_per_colour_type() {
        let plte: &[u8] = &[0, 0, 0, 255, 255, 255];
        let cases: [(u8, &[u8], &[u8]); 5] = [
            (COLOR_GRAY, &[0, 77], &[5]),
            (COLOR_GRAY_ALPHA, &[0, 78], &[5, 3]),
            (COLOR_RGB, &[0, 1, 0, 2, 0, 3], &[5, 6, 7]),
            (COLOR_RGBA, &[0, 4, 0, 5, 0, 6], &[5, 6, 7, 3]),
            (COLOR_PALETTE, &[1], &[4, 5, 6]),
        ];
        for (color_type, bkgd, sbit) in cases {
            let mut chunks: Vec<(&[u8; 4], &[u8])> = vec![(b"sBIT", sbit)];
            if color_type == COLOR_PALETTE {
                chunks.push((b"PLTE", plte));
            }
            chunks.push((b"bKGD", bkgd));
            let dec = decode(&with_chunks(color_type, &chunks));
            assert_eq!(dec.bkgd.as_deref(), Some(bkgd), "colour type {color_type}");
            assert_eq!(dec.sbit.as_deref(), Some(sbit), "colour type {color_type}");
            assert_eq!(dec.warnings, 0, "colour type {color_type}");
        }
    }

    #[test]
    fn absent_chunks_read_as_none_without_a_warning() {
        let dec = decode(&with_chunks(COLOR_RGB, &[]));
        assert_eq!((dec.bkgd, dec.sbit, dec.warnings), (None, None, 0));
    }

    #[test]
    fn a_bkgd_of_the_wrong_length_is_dropped_with_a_warning() {
        // One byte is a palette index; under truecolour libpng rejects it (`png_handle_bKGD`).
        let dec = decode(&with_chunks(COLOR_RGB, &[(b"bKGD", &[1])]));
        assert_eq!((dec.bkgd, dec.warnings), (None, 1));
    }

    #[test]
    fn every_metadata_chunk_written_reads_back_in_its_own_field() {
        // A header, an empty tag table, then 128 bytes that do not deflate away: libpng drops an
        // iCCP chunk shorter than 92 bytes (see `DecodedImage::iccp`).
        let mut icc = vec![0u8; 132];
        icc.extend((0u32..128).map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8));
        let size = icc.len() as u32;
        icc[0..4].copy_from_slice(&size.to_be_bytes());
        icc[8..12].copy_from_slice(&0x0210_0000u32.to_be_bytes());
        icc[12..16].copy_from_slice(b"mntr");
        icc[16..20].copy_from_slice(b"RGB ");
        icc[20..24].copy_from_slice(b"XYZ ");
        icc[36..40].copy_from_slice(b"acsp");
        icc[68..80].copy_from_slice(&[0, 0, 0xF6, 0xD6, 0, 1, 0, 0, 0, 0, 0xD3, 0x2D]);
        let chrm = [
            31_270, 32_900, 64_000, 33_000, 30_000, 60_000, 15_000, 6_000,
        ];
        let exif = [0x49, 0x49, 0x2A, 0, 8, 0, 0, 0, 0, 0];
        let png = encode(
            &[0; 3],
            1,
            1,
            COLOR_RGB,
            8,
            &EncodeOpts {
                gamma: Some(45_000),
                chromaticities: Some(chrm),
                exif: Some(&exif),
                icc: Some(("Tiny", &icc)),
                text: &[
                    TextChunk {
                        keyword: "Author",
                        text: "caf\u{e9}",
                        kind: TextKind::Text,
                    },
                    TextChunk {
                        keyword: "Comment",
                        text: "na\u{ef}ve",
                        kind: TextKind::ZTxt,
                    },
                    TextChunk {
                        keyword: "Note",
                        text: "g\u{e4}mut",
                        kind: TextKind::ITxt {
                            language: "de",
                            translated: "Notiz",
                            compressed: true,
                        },
                    },
                ],
                extra_chunks: &[(*b"cICP", &[9, 16, 0, 1]), (*b"caBX", b"store")],
                ..EncodeOpts::default()
            },
        );
        let dec = decode(&png);
        assert_eq!(dec.warnings, 0);
        assert_eq!(dec.gamma, Some(45_000));
        assert_eq!(dec.chromaticities, Some(chrm));
        assert_eq!(dec.exif.as_deref(), Some(&exif[..]));
        assert_eq!(dec.iccp, Some(("Tiny".to_string(), icc)));
        assert_eq!(dec.srgb, None);
        let text = |keyword: &str, text: &str, kind| OracleText {
            keyword: keyword.into(),
            text: text.into(),
            kind,
        };
        assert_eq!(
            dec.texts,
            [
                text("Author", "caf\u{e9}", OracleTextKind::Text),
                text("Comment", "na\u{ef}ve", OracleTextKind::ZTxt),
                text(
                    "Note",
                    "g\u{e4}mut",
                    OracleTextKind::ITxt {
                        language: "de".into(),
                        translated: "Notiz".into(),
                        compressed: true,
                    }
                ),
            ]
        );
        assert_eq!(
            dec.unknown_chunks,
            [(*b"cICP", vec![9, 16, 0, 1]), (*b"caBX", b"store".to_vec())]
        );
    }

    #[test]
    fn an_srgb_intent_reads_back_with_libpngs_implied_gamma() {
        let png = encode(
            &[0; 3],
            1,
            1,
            COLOR_RGB,
            8,
            &EncodeOpts {
                srgb_intent: Some(2),
                ..EncodeOpts::default()
            },
        );
        let dec = decode(&png);
        assert_eq!((dec.srgb, dec.gamma), (Some(2), Some(45_455)));
        assert!(dec.texts.is_empty() && dec.unknown_chunks.is_empty());
        assert_eq!((dec.iccp, dec.exif), (None, None));
    }

    #[test]
    fn an_sbit_past_the_bit_depth_is_dropped_with_a_warning() {
        // 9 significant bits in an 8-bit channel (`png_handle_sBIT`).
        let dec = decode(&with_chunks(COLOR_GRAY, &[(b"sBIT", &[9])]));
        assert_eq!((dec.sbit, dec.warnings), (None, 1));
    }
}
