//! Dev-only differential oracle around a vendored, statically-linked **libtiff**.
//!
//! gamut's TIFF encoder must produce files that the canonical reference reader decodes back to the
//! same pixels, and its decoder must read files the reference writer produces. This crate wraps a
//! libtiff built from the `third_party/libtiff` submodule against vendored zlib (all other optional
//! codecs disabled) behind a small, safe API:
//! [`decode_tiff`], [`encode_rgb8`], and [`encode_gray8`].
//!
//! libtiff's public API is file-based, so each call round-trips through a temporary file. All
//! `unsafe` FFI is confined to this crate.

#![allow(non_upper_case_globals, non_camel_case_types, non_snake_case)]

use std::ffi::CString;
use std::os::raw::{c_int, c_void};
use std::path::Path;

mod sys {
    #![allow(dead_code)]
    include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
}

/// A TIFF compression scheme the oracle can write (a subset of libtiff's built-in schemes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compression {
    /// Uncompressed (`COMPRESSION_NONE`).
    None,
    /// PackBits run-length (`COMPRESSION_PACKBITS`).
    PackBits,
    /// LZW (`COMPRESSION_LZW`).
    Lzw,
    /// Adobe Deflate (`COMPRESSION_ADOBE_DEFLATE`).
    Deflate,
    /// CCITT Group 3 1-D Modified Huffman (`COMPRESSION_CCITTRLE`).
    CcittRle,
    /// CCITT Group 4 (T.6) fax (`COMPRESSION_CCITTFAX4`).
    CcittGroup4Fax,
}

impl Compression {
    fn code(self) -> u16 {
        match self {
            Compression::None => sys::COMPRESSION_NONE as u16,
            Compression::PackBits => sys::COMPRESSION_PACKBITS as u16,
            Compression::Lzw => sys::COMPRESSION_LZW as u16,
            Compression::Deflate => sys::COMPRESSION_ADOBE_DEFLATE as u16,
            Compression::CcittRle => sys::COMPRESSION_CCITTRLE as u16,
            Compression::CcittGroup4Fax => sys::COMPRESSION_CCITTFAX4 as u16,
        }
    }
}

/// An image decoded by libtiff: interleaved 8-bit samples in raster order (no row padding).
pub struct DecodedImage {
    /// Image width in pixels.
    pub width: u32,
    /// Image height in pixels.
    pub height: u32,
    /// Samples per pixel (1 for grayscale, 3 for RGB).
    pub samples_per_pixel: u16,
    /// `width * height * samples_per_pixel` interleaved 8-bit samples.
    pub pixels: Vec<u8>,
}

/// Decodes a TIFF byte stream with libtiff into interleaved 8-bit samples.
///
/// # Errors
///
/// Returns a message if the file cannot be written to a temp file, parsed, or is not 8-bit.
pub fn decode_tiff(bytes: &[u8]) -> Result<DecodedImage, String> {
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let path = dir.path().join("oracle.tiff");
    std::fs::write(&path, bytes).map_err(|e| e.to_string())?;
    let cpath = c_path(&path)?;
    // SAFETY: `cpath` is a valid NUL-terminated path; the TIFF handle is closed on every path.
    unsafe { decode_inner(&cpath) }
}

/// An image decoded by libtiff as 16-bit samples, in raster order (no row padding).
pub struct DecodedImage16 {
    /// Image width in pixels.
    pub width: u32,
    /// Image height in pixels.
    pub height: u32,
    /// Samples per pixel (1 for grayscale, 3 for RGB, 4 for RGBA/CMYK).
    pub samples_per_pixel: u16,
    /// `width * height * samples_per_pixel` interleaved samples, in **host** order.
    ///
    /// libtiff swaps a file whose byte order differs from the host's as it reads, so comparing
    /// these against gamut's `ImageBuf::as_samples()` is byte-order independent — which is exactly
    /// what makes this the right cross-check for a big-endian file.
    pub samples: Vec<u16>,
}

/// Decodes a 16-bit TIFF byte stream with libtiff into interleaved `u16` samples.
///
/// Separate from [`decode_tiff`] rather than folded into it: that function's 8-bit contract is
/// relied on by every existing test, and widening its return type would churn all of them.
///
/// # Errors
///
/// Returns a message if the file cannot be written to a temp file, parsed, or is not 16-bit.
pub fn decode_tiff16(bytes: &[u8]) -> Result<DecodedImage16, String> {
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let path = dir.path().join("oracle.tiff");
    std::fs::write(&path, bytes).map_err(|e| e.to_string())?;
    let cpath = c_path(&path)?;
    // SAFETY: `cpath` is a valid NUL-terminated path; the TIFF handle is closed on every path.
    unsafe {
        let mode = CString::new("r").map_err(|e| e.to_string())?;
        let t = sys::TIFFOpen(cpath.as_ptr(), mode.as_ptr());
        if t.is_null() {
            return Err("TIFFOpen (read) failed".into());
        }
        let out = read_scanlines16(t);
        sys::TIFFClose(t);
        out
    }
}

unsafe fn read_scanlines16(t: *mut sys::TIFF) -> Result<DecodedImage16, String> {
    let mut width: u32 = 0;
    let mut height: u32 = 0;
    let mut spp: u16 = 1;
    let mut bps: u16 = 1;
    unsafe {
        if sys::TIFFGetField(t, sys::TIFFTAG_IMAGEWIDTH, &mut width as *mut u32) != 1 {
            return Err("missing ImageWidth".into());
        }
        if sys::TIFFGetField(t, sys::TIFFTAG_IMAGELENGTH, &mut height as *mut u32) != 1 {
            return Err("missing ImageLength".into());
        }
        sys::TIFFGetFieldDefaulted(t, sys::TIFFTAG_SAMPLESPERPIXEL, &mut spp as *mut u16);
        sys::TIFFGetFieldDefaulted(t, sys::TIFFTAG_BITSPERSAMPLE, &mut bps as *mut u16);
    }
    if bps != 16 {
        return Err(format!("expected 16 bits per sample, found {bps}"));
    }
    // `TIFFReadScanline` refuses a tiled file outright, so tiles need the tile reader. (The 8-bit
    // suites sidestep this by cross-checking tiled files through `decode_rgba`, but that narrows to
    // 8-bit RGBA and so cannot serve a 16-bit differential.)
    if unsafe { sys::TIFFIsTiled(t) } != 0 {
        return unsafe { read_tiles16(t, width, height, spp) };
    }
    let scanline = unsafe { sys::TIFFScanlineSize(t) } as usize;
    let samples_per_row = (width as usize) * (spp as usize);
    let row_bytes = samples_per_row * 2;
    // libtiff writes whole scanlines, so the buffer must be at least what it reports even if the
    // image's own row is shorter.
    let mut buf = vec![0u16; (scanline.max(row_bytes).max(2)).div_ceil(2)];
    let mut samples = Vec::with_capacity(samples_per_row * height as usize);
    for row in 0..height {
        let rc = unsafe { sys::TIFFReadScanline(t, buf.as_mut_ptr() as *mut c_void, row, 0) };
        if rc != 1 {
            return Err(format!("TIFFReadScanline failed at row {row}"));
        }
        samples.extend_from_slice(&buf[..samples_per_row]);
    }
    Ok(DecodedImage16 {
        width,
        height,
        samples_per_pixel: spp,
        samples,
    })
}

/// Reads a tiled 16-bit image, cropping each tile's right/bottom padding back to the image grid.
unsafe fn read_tiles16(
    t: *mut sys::TIFF,
    width: u32,
    height: u32,
    spp: u16,
) -> Result<DecodedImage16, String> {
    let mut tile_w: u32 = 0;
    let mut tile_h: u32 = 0;
    unsafe {
        if sys::TIFFGetField(t, sys::TIFFTAG_TILEWIDTH, &mut tile_w as *mut u32) != 1
            || sys::TIFFGetField(t, sys::TIFFTAG_TILELENGTH, &mut tile_h as *mut u32) != 1
        {
            return Err("missing tile dimensions".into());
        }
    }
    if tile_w == 0 || tile_h == 0 {
        return Err("zero tile dimension".into());
    }
    let (w, h, spp_us) = (width as usize, height as usize, spp as usize);
    let (tw, th) = (tile_w as usize, tile_h as usize);
    let tile_samples = tw * th * spp_us;
    let mut tile = vec![0u16; tile_samples];
    let mut samples = vec![0u16; w * h * spp_us];
    for ty in (0..h).step_by(th) {
        for tx in (0..w).step_by(tw) {
            let index = unsafe { sys::TIFFComputeTile(t, tx as u32, ty as u32, 0, 0) };
            let rc = unsafe {
                sys::TIFFReadEncodedTile(
                    t,
                    index,
                    tile.as_mut_ptr() as *mut c_void,
                    (tile_samples * 2) as i64,
                )
            };
            if rc < 0 {
                return Err(format!("TIFFReadEncodedTile failed at tile ({tx},{ty})"));
            }
            let copy_cols = tw.min(w - tx);
            for r in 0..th.min(h - ty) {
                let src = r * tw * spp_us;
                let dst = (ty + r) * w * spp_us + tx * spp_us;
                samples[dst..dst + copy_cols * spp_us]
                    .copy_from_slice(&tile[src..src + copy_cols * spp_us]);
            }
        }
    }
    Ok(DecodedImage16 {
        width,
        height,
        samples_per_pixel: spp,
        samples,
    })
}

/// Decodes a TIFF with libtiff's high-level RGBA reader, returning `(width, height, RGBA bytes)`.
///
/// Unlike [`decode_tiff`] (which returns raw samples), this resolves the colour map and
/// photometric interpretation, so it validates palette/colour handling against the reference.
///
/// # Errors
///
/// Returns a message if the file cannot be written to a temp file or decoded.
pub fn decode_rgba(bytes: &[u8]) -> Result<(u32, u32, Vec<u8>), String> {
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let path = dir.path().join("oracle.tiff");
    std::fs::write(&path, bytes).map_err(|e| e.to_string())?;
    let cpath = c_path(&path)?;
    // SAFETY: `cpath` is valid; the handle is closed on every path.
    unsafe { decode_rgba_inner(&cpath) }
}

unsafe fn decode_rgba_inner(cpath: &CString) -> Result<(u32, u32, Vec<u8>), String> {
    let mode = CString::new("r").map_err(|e| e.to_string())?;
    let t = unsafe { sys::TIFFOpen(cpath.as_ptr(), mode.as_ptr()) };
    if t.is_null() {
        return Err("TIFFOpen (read) failed".into());
    }
    let out = unsafe { read_rgba(t) };
    unsafe { sys::TIFFClose(t) };
    out
}

unsafe fn read_rgba(t: *mut sys::TIFF) -> Result<(u32, u32, Vec<u8>), String> {
    let mut width: u32 = 0;
    let mut height: u32 = 0;
    unsafe {
        if sys::TIFFGetField(t, sys::TIFFTAG_IMAGEWIDTH, &mut width as *mut u32) != 1
            || sys::TIFFGetField(t, sys::TIFFTAG_IMAGELENGTH, &mut height as *mut u32) != 1
        {
            return Err("missing dimensions".into());
        }
    }
    let n = (width as usize) * (height as usize);
    let mut raster = vec![0u32; n.max(1)];
    let rc = unsafe {
        sys::TIFFReadRGBAImageOriented(
            t,
            width,
            height,
            raster.as_mut_ptr(),
            sys::ORIENTATION_TOPLEFT as c_int,
            0,
        )
    };
    if rc != 1 {
        return Err("TIFFReadRGBAImageOriented failed".into());
    }
    let mut rgba = Vec::with_capacity(n * 4);
    for &px in &raster[..n] {
        // libtiff packs each pixel as ABGR (R is the low byte; see the TIFFGetR/G/B/A macros).
        rgba.push((px & 0xff) as u8);
        rgba.push(((px >> 8) & 0xff) as u8);
        rgba.push(((px >> 16) & 0xff) as u8);
        rgba.push(((px >> 24) & 0xff) as u8);
    }
    Ok((width, height, rgba))
}

/// Encodes interleaved 8-bit RGB with libtiff at the given compression, returning the TIFF bytes.
///
/// # Errors
///
/// Returns a message if `pixels` does not match the dimensions or libtiff fails to write.
pub fn encode_rgb8(
    pixels: &[u8],
    width: u32,
    height: u32,
    compression: Compression,
) -> Result<Vec<u8>, String> {
    encode_packed(
        pixels,
        width,
        height,
        3,
        8,
        sys::PHOTOMETRIC_RGB as u16,
        (width as usize) * 3,
        compression,
        1,
    )
}

/// Encodes interleaved 8-bit RGBA with libtiff (`ExtraSamples = unassociated alpha`).
///
/// # Errors
///
/// Returns a message if `pixels` does not match the dimensions or libtiff fails to write.
pub fn encode_rgba8(
    pixels: &[u8],
    width: u32,
    height: u32,
    compression: Compression,
) -> Result<Vec<u8>, String> {
    if pixels.len() != (width as usize) * (height as usize) * 4 {
        return Err("pixel buffer does not match dimensions".into());
    }
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let path = dir.path().join("oracle.tiff");
    let cpath = c_path(&path)?;
    // SAFETY: `cpath` is valid; the handle is closed before we read the file back.
    unsafe {
        let mode = CString::new("w").map_err(|e| e.to_string())?;
        let t = sys::TIFFOpen(cpath.as_ptr(), mode.as_ptr());
        if t.is_null() {
            return Err("TIFFOpen (write) failed".into());
        }
        let result = write_rgba(t, pixels, width, height, compression.code());
        sys::TIFFClose(t);
        result?;
    }
    std::fs::read(&path).map_err(|e| e.to_string())
}

unsafe fn write_rgba(
    t: *mut sys::TIFF,
    pixels: &[u8],
    width: u32,
    height: u32,
    compression: u16,
) -> Result<(), String> {
    let extra: [u16; 1] = [sys::EXTRASAMPLE_UNASSALPHA as u16];
    unsafe {
        sys::TIFFSetField(t, sys::TIFFTAG_IMAGEWIDTH, width);
        sys::TIFFSetField(t, sys::TIFFTAG_IMAGELENGTH, height);
        sys::TIFFSetField(t, sys::TIFFTAG_BITSPERSAMPLE, 8 as c_int);
        sys::TIFFSetField(t, sys::TIFFTAG_SAMPLESPERPIXEL, 4 as c_int);
        sys::TIFFSetField(t, sys::TIFFTAG_PHOTOMETRIC, sys::PHOTOMETRIC_RGB as c_int);
        sys::TIFFSetField(
            t,
            sys::TIFFTAG_PLANARCONFIG,
            sys::PLANARCONFIG_CONTIG as c_int,
        );
        sys::TIFFSetField(t, sys::TIFFTAG_COMPRESSION, compression as c_int);
        sys::TIFFSetField(t, sys::TIFFTAG_EXTRASAMPLES, 1 as c_int, extra.as_ptr());
        let rps = sys::TIFFDefaultStripSize(t, 0);
        sys::TIFFSetField(t, sys::TIFFTAG_ROWSPERSTRIP, rps);
    }
    let row_bytes = (width as usize) * 4;
    let mut scratch = vec![0u8; row_bytes];
    for row in 0..height as usize {
        scratch.copy_from_slice(&pixels[row * row_bytes..(row + 1) * row_bytes]);
        let rc = unsafe {
            sys::TIFFWriteScanline(t, scratch.as_mut_ptr() as *mut c_void, row as u32, 0)
        };
        if rc != 1 {
            return Err(format!("TIFFWriteScanline failed at row {row}"));
        }
    }
    Ok(())
}

/// Encodes interleaved 8-bit CMYK with libtiff (`PhotometricInterpretation = Separated`).
///
/// # Errors
///
/// Returns a message if `pixels` does not match the dimensions or libtiff fails to write.
pub fn encode_cmyk8(
    pixels: &[u8],
    width: u32,
    height: u32,
    compression: Compression,
) -> Result<Vec<u8>, String> {
    encode_packed(
        pixels,
        width,
        height,
        4,
        8,
        sys::PHOTOMETRIC_SEPARATED as u16,
        (width as usize) * 4,
        compression,
        1,
    )
}

/// Packs 16-bit samples for libtiff in **native** byte order.
///
/// Native, never little-endian: libtiff swaps on write when the file's order differs from the
/// host's, so it expects host-order input. Hard-coding `to_le_bytes` would make this oracle agree
/// with a byte-order-buggy decoder on a little-endian host — the differential would pass while
/// both sides were wrong together, which defeats the entire point of an oracle.
fn native_bytes(samples: &[u16]) -> Vec<u8> {
    samples.iter().flat_map(|v| v.to_ne_bytes()).collect()
}

/// Encodes 16-bit grayscale (`MINISBLACK`) with libtiff at the given compression.
///
/// `predictor` is `1` (none) or `2` (horizontal differencing); `big_endian` writes an `MM` file.
///
/// # Errors
///
/// Returns a message if `samples` does not match the dimensions or libtiff fails to write.
pub fn encode_gray16(
    samples: &[u16],
    width: u32,
    height: u32,
    compression: Compression,
    predictor: u16,
    big_endian: bool,
) -> Result<Vec<u8>, String> {
    encode_packed_mode(
        &native_bytes(samples),
        width,
        height,
        1,
        16,
        sys::PHOTOMETRIC_MINISBLACK as u16,
        (width as usize) * 2,
        compression,
        predictor,
        false,
        big_endian,
    )
}

/// Encodes interleaved 16-bit RGB with libtiff at the given compression.
///
/// `predictor` is `1` (none) or `2` (horizontal differencing); `big_endian` writes an `MM` file.
///
/// # Errors
///
/// Returns a message if `samples` does not match the dimensions or libtiff fails to write.
pub fn encode_rgb16(
    samples: &[u16],
    width: u32,
    height: u32,
    compression: Compression,
    predictor: u16,
    big_endian: bool,
) -> Result<Vec<u8>, String> {
    encode_packed_mode(
        &native_bytes(samples),
        width,
        height,
        3,
        16,
        sys::PHOTOMETRIC_RGB as u16,
        (width as usize) * 6,
        compression,
        predictor,
        false,
        big_endian,
    )
}

/// Encodes 16-bit grayscale as `MINISWHITE` — the inverted photometric, where `0` is white.
///
/// gamut's encoder only ever writes `MINISBLACK`, so this is the only way to obtain a 16-bit
/// WhiteIsZero page to decode against.
///
/// # Errors
///
/// Returns a message if `samples` does not match the dimensions or libtiff fails to write.
pub fn encode_gray16_miniswhite(
    samples: &[u16],
    width: u32,
    height: u32,
    compression: Compression,
) -> Result<Vec<u8>, String> {
    encode_packed_mode(
        &native_bytes(samples),
        width,
        height,
        1,
        16,
        sys::PHOTOMETRIC_MINISWHITE as u16,
        (width as usize) * 2,
        compression,
        1,
        false,
        false,
    )
}

/// Encodes interleaved 16-bit RGB as a **tiled** TIFF with `tile_w × tile_h` tiles.
///
/// # Errors
///
/// Returns a message if `samples` does not match the dimensions or libtiff fails to write.
pub fn encode_rgb16_tiled(
    samples: &[u16],
    width: u32,
    height: u32,
    tile_w: u32,
    tile_h: u32,
    compression: Compression,
    predictor: u16,
) -> Result<Vec<u8>, String> {
    encode_tiled_packed(
        &native_bytes(samples),
        width,
        height,
        3,
        16,
        sys::PHOTOMETRIC_RGB as u16,
        tile_w,
        tile_h,
        compression,
        predictor,
    )
}

/// Encodes 8-bit grayscale (`MINISBLACK`) with libtiff at the given compression.
///
/// # Errors
///
/// Returns a message if `pixels` does not match the dimensions or libtiff fails to write.
pub fn encode_gray8(
    pixels: &[u8],
    width: u32,
    height: u32,
    compression: Compression,
) -> Result<Vec<u8>, String> {
    encode_packed(
        pixels,
        width,
        height,
        1,
        8,
        sys::PHOTOMETRIC_MINISBLACK as u16,
        width as usize,
        compression,
        1,
    )
}

/// Encodes interleaved 8-bit RGB as **BigTIFF** (64-bit offsets) at the given compression.
///
/// libtiff reads classic TIFF and BigTIFF through the same API, so it is the decode side that is
/// transparent; this wrapper exercises the gamut decoder against a libtiff-produced BigTIFF file.
///
/// # Errors
///
/// Returns a message if `pixels` does not match the dimensions or libtiff fails to write.
pub fn encode_rgb8_bigtiff(
    pixels: &[u8],
    width: u32,
    height: u32,
    compression: Compression,
) -> Result<Vec<u8>, String> {
    encode_packed_mode(
        pixels,
        width,
        height,
        3,
        8,
        sys::PHOTOMETRIC_RGB as u16,
        (width as usize) * 3,
        compression,
        1,
        true,
        false,
    )
}

/// Encodes 8-bit grayscale (`MINISBLACK`) as **BigTIFF** (64-bit offsets) at the given compression.
///
/// # Errors
///
/// Returns a message if `pixels` does not match the dimensions or libtiff fails to write.
pub fn encode_gray8_bigtiff(
    pixels: &[u8],
    width: u32,
    height: u32,
    compression: Compression,
) -> Result<Vec<u8>, String> {
    encode_packed_mode(
        pixels,
        width,
        height,
        1,
        8,
        sys::PHOTOMETRIC_MINISBLACK as u16,
        width as usize,
        compression,
        1,
        true,
        false,
    )
}

/// Encodes a 1-bit bilevel image (`MINISBLACK`) from one byte per pixel (0 = black, non-zero =
/// white), packing the bits MSB-first.
///
/// # Errors
///
/// Returns a message if `pixels` does not match the dimensions or libtiff fails to write.
pub fn encode_bilevel(
    pixels: &[u8],
    width: u32,
    height: u32,
    compression: Compression,
) -> Result<Vec<u8>, String> {
    if pixels.len()
        != (width as usize)
            .checked_mul(height as usize)
            .ok_or("overflow")?
    {
        return Err("pixel buffer does not match dimensions".into());
    }
    let stored = (width as usize).div_ceil(8);
    let mut packed = vec![0u8; stored * height as usize];
    for y in 0..height as usize {
        let row = &pixels[y * width as usize..(y + 1) * width as usize];
        let dst = &mut packed[y * stored..(y + 1) * stored];
        for (x, &p) in row.iter().enumerate() {
            if p != 0 {
                dst[x / 8] |= 0x80 >> (x % 8);
            }
        }
    }
    encode_packed(
        &packed,
        width,
        height,
        1,
        1,
        sys::PHOTOMETRIC_MINISBLACK as u16,
        stored,
        compression,
        1,
    )
}

/// Encodes interleaved 8-bit RGB with the horizontal-differencing predictor (`Predictor = 2`).
///
/// # Errors
///
/// Returns a message if `pixels` does not match the dimensions or libtiff fails to write.
pub fn encode_rgb8_predictor(
    pixels: &[u8],
    width: u32,
    height: u32,
    compression: Compression,
) -> Result<Vec<u8>, String> {
    encode_packed(
        pixels,
        width,
        height,
        3,
        8,
        sys::PHOTOMETRIC_RGB as u16,
        (width as usize) * 3,
        compression,
        2,
    )
}

/// Encodes 8-bit grayscale with the horizontal-differencing predictor (`Predictor = 2`).
///
/// # Errors
///
/// Returns a message if `pixels` does not match the dimensions or libtiff fails to write.
pub fn encode_gray8_predictor(
    pixels: &[u8],
    width: u32,
    height: u32,
    compression: Compression,
) -> Result<Vec<u8>, String> {
    encode_packed(
        pixels,
        width,
        height,
        1,
        8,
        sys::PHOTOMETRIC_MINISBLACK as u16,
        width as usize,
        compression,
        2,
    )
}

#[allow(clippy::too_many_arguments)]
fn encode_packed(
    packed: &[u8],
    width: u32,
    height: u32,
    spp: u16,
    bps: u16,
    photometric: u16,
    stored_row_bytes: usize,
    compression: Compression,
    predictor: u16,
) -> Result<Vec<u8>, String> {
    encode_packed_mode(
        packed,
        width,
        height,
        spp,
        bps,
        photometric,
        stored_row_bytes,
        compression,
        predictor,
        false,
        false,
    )
}

#[allow(clippy::too_many_arguments)]
fn encode_packed_mode(
    packed: &[u8],
    width: u32,
    height: u32,
    spp: u16,
    bps: u16,
    photometric: u16,
    stored_row_bytes: usize,
    compression: Compression,
    predictor: u16,
    bigtiff: bool,
    big_endian: bool,
) -> Result<Vec<u8>, String> {
    if packed.len()
        != stored_row_bytes
            .checked_mul(height as usize)
            .ok_or("dimensions overflow")?
    {
        return Err("packed buffer does not match dimensions".into());
    }
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let path = dir.path().join("oracle.tiff");
    let cpath = c_path(&path)?;
    // SAFETY: `cpath` is valid; the handle is closed before we read the file back.
    unsafe {
        encode_inner(
            &cpath,
            packed,
            width,
            height,
            spp,
            bps,
            photometric,
            stored_row_bytes,
            compression.code(),
            predictor,
            bigtiff,
            big_endian,
        )?;
    }
    std::fs::read(&path).map_err(|e| e.to_string())
}

/// Writes a single-strip 8-bit grayscale (`MINISBLACK`) TIFF whose `Compression` tag is
/// `compression_code` and whose one strip is `strip`, verbatim, through `TIFFWriteRawStrip`.
///
/// This reaches the schemes this build of libtiff has no codec for (ZSTD, WebP, LERC, JPEG 2000 are
/// configured off): libtiff still writes their code and lays out the directory exactly as it would
/// for an encoded strip, so the file is libtiff-written in every byte but the strip payload, which
/// the caller supplies and libtiff never inspects.
///
/// # Errors
///
/// Returns a message if libtiff refuses the code or fails to write.
pub fn encode_gray8_raw_strip(
    width: u32,
    height: u32,
    compression_code: u16,
    strip: &[u8],
) -> Result<Vec<u8>, String> {
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let path = dir.path().join("oracle.tiff");
    let cpath = c_path(&path)?;
    let mode = CString::new("w").map_err(|e| e.to_string())?;
    // SAFETY: `cpath` and `mode` are valid NUL-terminated strings; the handle is closed before the
    // file is read back, and `strip` outlives the `TIFFWriteRawStrip` call that copies from it.
    unsafe {
        let t = sys::TIFFOpen(cpath.as_ptr(), mode.as_ptr());
        if t.is_null() {
            return Err("TIFFOpen (write) failed".into());
        }
        // uint32 fields take a `u32` vararg; uint16 fields are promoted to `c_int`.
        sys::TIFFSetField(t, sys::TIFFTAG_IMAGEWIDTH, width);
        sys::TIFFSetField(t, sys::TIFFTAG_IMAGELENGTH, height);
        sys::TIFFSetField(t, sys::TIFFTAG_BITSPERSAMPLE, 8 as c_int);
        sys::TIFFSetField(t, sys::TIFFTAG_SAMPLESPERPIXEL, 1 as c_int);
        sys::TIFFSetField(
            t,
            sys::TIFFTAG_PHOTOMETRIC,
            sys::PHOTOMETRIC_MINISBLACK as c_int,
        );
        let set = sys::TIFFSetField(t, sys::TIFFTAG_COMPRESSION, compression_code as c_int);
        sys::TIFFSetField(
            t,
            sys::TIFFTAG_PLANARCONFIG,
            sys::PLANARCONFIG_CONTIG as c_int,
        );
        sys::TIFFSetField(t, sys::TIFFTAG_ROWSPERSTRIP, height);
        let written = if set == 1 {
            sys::TIFFWriteRawStrip(t, 0, strip.as_ptr() as *mut c_void, strip.len() as _)
        } else {
            -1
        };
        sys::TIFFClose(t);
        if set != 1 {
            return Err(format!(
                "TIFFSetField(Compression = {compression_code}) failed"
            ));
        }
        if written < 0 {
            return Err("TIFFWriteRawStrip failed".into());
        }
    }
    std::fs::read(&path).map_err(|e| e.to_string())
}

/// Encodes 8-bit RGB as a **tiled** TIFF with `tile_w × tile_h` tiles at the given compression.
///
/// # Errors
///
/// Returns a message if `pixels` does not match the dimensions or libtiff fails to write.
pub fn encode_rgb8_tiled(
    pixels: &[u8],
    width: u32,
    height: u32,
    tile_w: u32,
    tile_h: u32,
    compression: Compression,
) -> Result<Vec<u8>, String> {
    encode_rgb8_tiled_with_predictor(pixels, width, height, tile_w, tile_h, compression, 1)
}

/// Encodes 8-bit RGB as a tiled TIFF with horizontal differencing (`Predictor = 2`).
///
/// # Errors
///
/// Returns a message if `pixels` does not match the dimensions or libtiff fails to write.
pub fn encode_rgb8_tiled_predictor(
    pixels: &[u8],
    width: u32,
    height: u32,
    tile_w: u32,
    tile_h: u32,
    compression: Compression,
) -> Result<Vec<u8>, String> {
    encode_rgb8_tiled_with_predictor(pixels, width, height, tile_w, tile_h, compression, 2)
}

#[allow(clippy::too_many_arguments)]
fn encode_rgb8_tiled_with_predictor(
    pixels: &[u8],
    width: u32,
    height: u32,
    tile_w: u32,
    tile_h: u32,
    compression: Compression,
    predictor: u16,
) -> Result<Vec<u8>, String> {
    encode_tiled_packed(
        pixels,
        width,
        height,
        3,
        8,
        sys::PHOTOMETRIC_RGB as u16,
        tile_w,
        tile_h,
        compression,
        predictor,
    )
}

/// Encodes already-packed sample bytes as a tiled TIFF at the given depth and photometric.
#[allow(clippy::too_many_arguments)]
fn encode_tiled_packed(
    packed: &[u8],
    width: u32,
    height: u32,
    spp: u16,
    bps: u16,
    photometric: u16,
    tile_w: u32,
    tile_h: u32,
    compression: Compression,
    predictor: u16,
) -> Result<Vec<u8>, String> {
    let pixel_bytes = spp as usize * (bps as usize / 8);
    if packed.len() != (width as usize) * (height as usize) * pixel_bytes {
        return Err("pixel buffer does not match dimensions".into());
    }
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let path = dir.path().join("oracle.tiff");
    let cpath = c_path(&path)?;
    // SAFETY: `cpath` is valid; the handle is closed before we read the file back.
    unsafe {
        let mode = CString::new("w").map_err(|e| e.to_string())?;
        let t = sys::TIFFOpen(cpath.as_ptr(), mode.as_ptr());
        if t.is_null() {
            return Err("TIFFOpen (write) failed".into());
        }
        let result = write_tiles(
            t,
            packed,
            width,
            height,
            spp,
            bps,
            photometric,
            tile_w,
            tile_h,
            compression.code(),
            predictor,
        );
        sys::TIFFClose(t);
        result?;
    }
    std::fs::read(&path).map_err(|e| e.to_string())
}

#[allow(clippy::too_many_arguments)]
unsafe fn write_tiles(
    t: *mut sys::TIFF,
    pixels: &[u8],
    width: u32,
    height: u32,
    spp: u16,
    bps: u16,
    photometric: u16,
    tile_w: u32,
    tile_h: u32,
    compression: u16,
    predictor: u16,
) -> Result<(), String> {
    unsafe {
        sys::TIFFSetField(t, sys::TIFFTAG_IMAGEWIDTH, width);
        sys::TIFFSetField(t, sys::TIFFTAG_IMAGELENGTH, height);
        sys::TIFFSetField(t, sys::TIFFTAG_BITSPERSAMPLE, bps as c_int);
        sys::TIFFSetField(t, sys::TIFFTAG_SAMPLESPERPIXEL, spp as c_int);
        sys::TIFFSetField(t, sys::TIFFTAG_PHOTOMETRIC, photometric as c_int);
        sys::TIFFSetField(
            t,
            sys::TIFFTAG_PLANARCONFIG,
            sys::PLANARCONFIG_CONTIG as c_int,
        );
        sys::TIFFSetField(t, sys::TIFFTAG_COMPRESSION, compression as c_int);
        if predictor != 1 {
            sys::TIFFSetField(t, sys::TIFFTAG_PREDICTOR, predictor as c_int);
        }
        sys::TIFFSetField(t, sys::TIFFTAG_TILEWIDTH, tile_w);
        sys::TIFFSetField(t, sys::TIFFTAG_TILELENGTH, tile_h);
    }
    // Every offset below is a *byte* offset, so the per-pixel stride carries the sample width too.
    let spp = spp as usize * (bps as usize / 8);
    let (w, h, tw, th) = (
        width as usize,
        height as usize,
        tile_w as usize,
        tile_h as usize,
    );
    let tile_row = tw * spp;
    let tile_size = th * tile_row;
    let across = w.div_ceil(tw);
    let down = h.div_ceil(th);
    let mut buf = vec![0u8; tile_size];
    for ty in 0..down {
        for tx in 0..across {
            buf.iter_mut().for_each(|b| *b = 0);
            let copy_cols = tw.min(w - tx * tw);
            for r in 0..th {
                let src_row = ty * th + r;
                if src_row >= h {
                    break;
                }
                let src = src_row * w * spp + tx * tw * spp;
                let dst = r * tile_row;
                buf[dst..dst + copy_cols * spp]
                    .copy_from_slice(&pixels[src..src + copy_cols * spp]);
            }
            let tile = unsafe { sys::TIFFComputeTile(t, (tx * tw) as u32, (ty * th) as u32, 0, 0) };
            let rc = unsafe {
                sys::TIFFWriteEncodedTile(
                    t,
                    tile,
                    buf.as_mut_ptr() as *mut c_void,
                    tile_size as i64,
                )
            };
            if rc < 0 {
                return Err(format!("TIFFWriteEncodedTile failed at tile ({tx},{ty})"));
            }
        }
    }
    Ok(())
}

/// Encodes several 8-bit RGB images as the pages of one multi-page TIFF.
///
/// Each page is `(pixels, width, height)` with `pixels` of length `width * height * 3`.
///
/// # Errors
///
/// Returns a message if a page's buffer does not match its dimensions or libtiff fails to write.
pub fn encode_pages_rgb8(
    pages: &[(&[u8], u32, u32)],
    compression: Compression,
) -> Result<Vec<u8>, String> {
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let path = dir.path().join("oracle.tiff");
    let cpath = c_path(&path)?;
    // SAFETY: `cpath` is valid; the handle is closed before we read the file back.
    unsafe {
        let mode = CString::new("w").map_err(|e| e.to_string())?;
        let t = sys::TIFFOpen(cpath.as_ptr(), mode.as_ptr());
        if t.is_null() {
            return Err("TIFFOpen (write) failed".into());
        }
        let result = (|| {
            for &(pixels, w, h) in pages {
                if pixels.len() != (w as usize) * (h as usize) * 3 {
                    return Err("pixel buffer does not match dimensions".to_string());
                }
                write_scanlines(
                    t,
                    pixels,
                    w,
                    h,
                    3,
                    8,
                    sys::PHOTOMETRIC_RGB as u16,
                    (w as usize) * 3,
                    compression.code(),
                    1,
                )?;
                if sys::TIFFWriteDirectory(t) != 1 {
                    return Err("TIFFWriteDirectory failed".to_string());
                }
            }
            Ok(())
        })();
        sys::TIFFClose(t);
        result?;
    }
    std::fs::read(&path).map_err(|e| e.to_string())
}

/// Decodes page `page` of a multi-page TIFF with libtiff into interleaved 8-bit samples.
///
/// # Errors
///
/// Returns a message if the file cannot be parsed or the page is out of range.
pub fn decode_page(bytes: &[u8], page: u32) -> Result<DecodedImage, String> {
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let path = dir.path().join("oracle.tiff");
    std::fs::write(&path, bytes).map_err(|e| e.to_string())?;
    let cpath = c_path(&path)?;
    // SAFETY: `cpath` is valid; the handle is closed on every path.
    unsafe {
        let mode = CString::new("r").map_err(|e| e.to_string())?;
        let t = sys::TIFFOpen(cpath.as_ptr(), mode.as_ptr());
        if t.is_null() {
            return Err("TIFFOpen (read) failed".into());
        }
        let out = if sys::TIFFSetDirectory(t, page) != 1 {
            Err("TIFFSetDirectory failed".into())
        } else {
            read_scanlines(t)
        };
        sys::TIFFClose(t);
        out
    }
}

fn c_path(path: &Path) -> Result<CString, String> {
    CString::new(path.to_str().ok_or("non-UTF-8 temp path")?).map_err(|e| e.to_string())
}

unsafe fn decode_inner(cpath: &CString) -> Result<DecodedImage, String> {
    let mode = CString::new("r").map_err(|e| e.to_string())?;
    let t = unsafe { sys::TIFFOpen(cpath.as_ptr(), mode.as_ptr()) };
    if t.is_null() {
        return Err("TIFFOpen (read) failed".into());
    }
    let out = unsafe { read_scanlines(t) };
    unsafe { sys::TIFFClose(t) };
    out
}

unsafe fn read_scanlines(t: *mut sys::TIFF) -> Result<DecodedImage, String> {
    let mut width: u32 = 0;
    let mut height: u32 = 0;
    let mut spp: u16 = 1;
    let mut bps: u16 = 1;
    unsafe {
        if sys::TIFFGetField(t, sys::TIFFTAG_IMAGEWIDTH, &mut width as *mut u32) != 1 {
            return Err("missing ImageWidth".into());
        }
        if sys::TIFFGetField(t, sys::TIFFTAG_IMAGELENGTH, &mut height as *mut u32) != 1 {
            return Err("missing ImageLength".into());
        }
        sys::TIFFGetFieldDefaulted(t, sys::TIFFTAG_SAMPLESPERPIXEL, &mut spp as *mut u16);
        sys::TIFFGetFieldDefaulted(t, sys::TIFFTAG_BITSPERSAMPLE, &mut bps as *mut u16);
    }
    let mut photometric: u16 = sys::PHOTOMETRIC_MINISBLACK as u16;
    unsafe {
        sys::TIFFGetFieldDefaulted(t, sys::TIFFTAG_PHOTOMETRIC, &mut photometric as *mut u16);
    }
    let scanline = unsafe { sys::TIFFScanlineSize(t) } as usize;

    let pixels = match bps {
        8 => {
            let row_bytes = (width as usize) * (spp as usize);
            let mut buf = vec![0u8; scanline.max(row_bytes).max(1)];
            let mut pixels = Vec::with_capacity(row_bytes * height as usize);
            for row in 0..height {
                let rc =
                    unsafe { sys::TIFFReadScanline(t, buf.as_mut_ptr() as *mut c_void, row, 0) };
                if rc != 1 {
                    return Err(format!("TIFFReadScanline failed at row {row}"));
                }
                pixels.extend_from_slice(&buf[..row_bytes]);
            }
            pixels
        }
        1 => {
            // 1-bit: unpack each MSB-first bit to a 0/255 sample, matching gamut's gray output.
            let white_is_zero = photometric == sys::PHOTOMETRIC_MINISWHITE as u16;
            let stored = (width as usize).div_ceil(8);
            let mut buf = vec![0u8; scanline.max(stored).max(1)];
            let mut pixels = Vec::with_capacity((width as usize) * (height as usize));
            for row in 0..height {
                let rc =
                    unsafe { sys::TIFFReadScanline(t, buf.as_mut_ptr() as *mut c_void, row, 0) };
                if rc != 1 {
                    return Err(format!("TIFFReadScanline failed at row {row}"));
                }
                for x in 0..width as usize {
                    let bit = (buf[x / 8] >> (7 - (x % 8))) & 1;
                    let white = if white_is_zero { bit == 0 } else { bit == 1 };
                    pixels.push(if white { 255 } else { 0 });
                }
            }
            return Ok(DecodedImage {
                width,
                height,
                samples_per_pixel: 1,
                pixels,
            });
        }
        _ => return Err(format!("unsupported bits-per-sample {bps}")),
    };
    Ok(DecodedImage {
        width,
        height,
        samples_per_pixel: spp,
        pixels,
    })
}

#[allow(clippy::too_many_arguments)]
unsafe fn encode_inner(
    cpath: &CString,
    packed: &[u8],
    width: u32,
    height: u32,
    spp: u16,
    bps: u16,
    photometric: u16,
    stored_row_bytes: usize,
    compression: u16,
    predictor: u16,
    bigtiff: bool,
    big_endian: bool,
) -> Result<(), String> {
    // libtiff picks the container and byte order from the open mode: "w8" is BigTIFF, "wb" forces
    // big-endian (`MM`) classic TIFF, and bare "w" writes classic TIFF in the *host's* order.
    let mode = CString::new(match (bigtiff, big_endian) {
        (true, _) => "w8",
        (false, true) => "wb",
        (false, false) => "w",
    })
    .map_err(|e| e.to_string())?;
    let t = unsafe { sys::TIFFOpen(cpath.as_ptr(), mode.as_ptr()) };
    if t.is_null() {
        return Err("TIFFOpen (write) failed".into());
    }
    let result = unsafe {
        write_scanlines(
            t,
            packed,
            width,
            height,
            spp,
            bps,
            photometric,
            stored_row_bytes,
            compression,
            predictor,
        )
    };
    unsafe { sys::TIFFClose(t) };
    result
}

#[allow(clippy::too_many_arguments)]
unsafe fn write_scanlines(
    t: *mut sys::TIFF,
    packed: &[u8],
    width: u32,
    height: u32,
    spp: u16,
    bps: u16,
    photometric: u16,
    stored_row_bytes: usize,
    compression: u16,
    predictor: u16,
) -> Result<(), String> {
    // uint32 fields take a `u32` vararg; uint16 fields are promoted to `c_int`.
    unsafe {
        sys::TIFFSetField(t, sys::TIFFTAG_IMAGEWIDTH, width);
        sys::TIFFSetField(t, sys::TIFFTAG_IMAGELENGTH, height);
        sys::TIFFSetField(t, sys::TIFFTAG_BITSPERSAMPLE, bps as c_int);
        sys::TIFFSetField(t, sys::TIFFTAG_SAMPLESPERPIXEL, spp as c_int);
        sys::TIFFSetField(t, sys::TIFFTAG_PHOTOMETRIC, photometric as c_int);
        sys::TIFFSetField(t, sys::TIFFTAG_COMPRESSION, compression as c_int);
        sys::TIFFSetField(
            t,
            sys::TIFFTAG_PLANARCONFIG,
            sys::PLANARCONFIG_CONTIG as c_int,
        );
        // Predictor must be set after compression; libtiff applies it for LZW/Deflate.
        if predictor != 1 {
            sys::TIFFSetField(t, sys::TIFFTAG_PREDICTOR, predictor as c_int);
        }
        let rps = sys::TIFFDefaultStripSize(t, 0);
        sys::TIFFSetField(t, sys::TIFFTAG_ROWSPERSTRIP, rps);
    }

    let row_bytes = stored_row_bytes;
    let mut scratch = vec![0u8; row_bytes];
    for row in 0..height as usize {
        scratch.copy_from_slice(&packed[row * row_bytes..(row + 1) * row_bytes]);
        let rc = unsafe {
            sys::TIFFWriteScanline(t, scratch.as_mut_ptr() as *mut c_void, row as u32, 0)
        };
        if rc != 1 {
            return Err(format!("TIFFWriteScanline failed at row {row}"));
        }
    }
    Ok(())
}
