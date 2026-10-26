//! Dev-only differential oracle around a vendored, statically-linked **libjpeg-turbo** (v3.2.0).
//!
//! gamut-jpeg's encoder must produce files the canonical reference decoder reads back to the same
//! pixels, and gamut-jpeg's decoder must reproduce what libjpeg-turbo decodes. This crate wraps the
//! reference encoder ([`encode`]) and decoder ([`decode`], [`decode_forced_rgb`]) behind a small,
//! safe API returning owned buffers.
//!
//! libjpeg reports fatal errors via `setjmp`/`longjmp`, which cannot be driven from Rust, so the
//! whole C surface lives behind the `src/shim.c` bridge (built by `build.rs` with the `cc` crate);
//! the shim owns the `setjmp` guard and reports failure through return codes, so a malformed input
//! yields an `Err` instead of aborting the process. All `unsafe` FFI is confined to this crate.
//!
//! Decode keeps libjpeg's default colour-space decision and default IDCT / fancy-upsampling
//! settings — the gamut tests own their pixel tolerances.

use std::ffi::CStr;
use std::os::raw::{c_char, c_int, c_uint};

unsafe extern "C" {
    fn oracle_jpeg_version() -> *const c_char;
    fn oracle_free(p: *mut u8);
    fn oracle_jpeg_decode(
        data: *const u8,
        len: usize,
        force_rgb: c_int,
        out_width: *mut c_uint,
        out_height: *mut c_uint,
        out_channels: *mut c_uint,
        out_pixels: *mut *mut u8,
        out_len: *mut usize,
    ) -> c_int;
    fn oracle_jpeg_read_icc(
        data: *const u8,
        len: usize,
        out: *mut *mut u8,
        out_len: *mut usize,
    ) -> c_int;
    fn oracle_jpeg_sampling_factors(
        data: *const u8,
        len: usize,
        h: *mut u8,
        v: *mut u8,
        cap: c_uint,
        out_components: *mut c_uint,
    ) -> c_int;
    fn oracle_jpeg_read_app1(
        data: *const u8,
        len: usize,
        out: *mut *mut u8,
        out_len: *mut usize,
    ) -> c_int;
    #[allow(clippy::too_many_arguments)]
    fn oracle_jpeg_encode_meta(
        pixels: *const u8,
        width: c_uint,
        height: c_uint,
        gray: c_int,
        quality: c_int,
        h_samp: c_int,
        v_samp: c_int,
        progressive: c_int,
        restart_interval: c_uint,
        optimize_coding: c_int,
        app1: *const u8,
        app1_len: usize,
        icc: *const u8,
        icc_len: usize,
        out: *mut *mut u8,
        out_len: *mut usize,
    ) -> c_int;
    #[allow(clippy::too_many_arguments)]
    fn oracle_jpeg_encode(
        pixels: *const u8,
        width: c_uint,
        height: c_uint,
        gray: c_int,
        quality: c_int,
        h_samp: c_int,
        v_samp: c_int,
        progressive: c_int,
        restart_interval: c_uint,
        optimize_coding: c_int,
        out: *mut *mut u8,
        out_len: *mut usize,
    ) -> c_int;
}

/// The libjpeg-turbo version string the oracle links against, e.g. `"3.2.0"`.
#[must_use]
pub fn version() -> String {
    // SAFETY: the shim returns a pointer to a static, NUL-terminated string literal.
    unsafe { CStr::from_ptr(oracle_jpeg_version()) }
        .to_string_lossy()
        .into_owned()
}

/// A JPEG decoded by libjpeg-turbo into interleaved 8-bit samples.
pub struct DecodedImage {
    /// Image width in pixels.
    pub width: u32,
    /// Image height in pixels.
    pub height: u32,
    /// Samples per pixel (1 = grayscale, 3 = RGB, 4 = CMYK).
    pub channels: u32,
    /// Interleaved samples, tightly packed (`width * height * channels` bytes).
    pub pixels: Vec<u8>,
}

/// Chroma subsampling for the luma component (chroma stays 1x1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Subsampling {
    /// 4:4:4 — no chroma subsampling (luma 1x1).
    S444,
    /// 4:2:2 — horizontal chroma subsampling (luma 2x1).
    S422,
    /// 4:2:0 — horizontal + vertical chroma subsampling (luma 2x2). libjpeg's default.
    #[default]
    S420,
    /// 4:4:0 — vertical chroma subsampling (luma 1x2).
    S440,
}

impl Subsampling {
    /// The luma `(h_samp_factor, v_samp_factor)` for this subsampling mode.
    #[must_use]
    fn factors(self) -> (i32, i32) {
        match self {
            Subsampling::S444 => (1, 1),
            Subsampling::S422 => (2, 1),
            Subsampling::S420 => (2, 2),
            Subsampling::S440 => (1, 2),
        }
    }
}

/// Reference-encoder parameters.
#[derive(Debug, Clone, Copy)]
pub struct EncodeParams {
    /// Quality (1..=100), applied with `force_baseline = TRUE`.
    pub quality: i32,
    /// Encode as single-channel grayscale (input is treated as 1 byte/pixel) rather than RGB.
    pub gray: bool,
    /// Chroma subsampling (ignored when `gray`).
    pub subsampling: Subsampling,
    /// Emit a progressive JPEG (default progression script) rather than a baseline sequential one.
    pub progressive: bool,
    /// Restart-marker interval in MCUs (0 = no restart markers).
    pub restart_interval: u16,
    /// Run the optional second pass that optimises the entropy-coding (Huffman) tables.
    pub optimize_coding: bool,
}

impl Default for EncodeParams {
    fn default() -> Self {
        // libjpeg's own defaults: quality 75, 4:2:0, baseline sequential, no restarts, no optimize.
        Self {
            quality: 75,
            gray: false,
            subsampling: Subsampling::S420,
            progressive: false,
            restart_interval: 0,
            optimize_coding: false,
        }
    }
}

/// Copies a shim-allocated buffer into an owned `Vec`, then frees the C allocation.
///
/// # Safety
///
/// `ptr` must be a non-null pointer to `len` bytes allocated by the shim.
unsafe fn take_owned(ptr: *mut u8, len: usize) -> Vec<u8> {
    let bytes = unsafe { std::slice::from_raw_parts(ptr, len) }.to_vec();
    unsafe { oracle_free(ptr) };
    bytes
}

/// Decodes a JPEG with libjpeg-turbo, keeping its default output colour space (grayscale stays 1
/// channel, colour images decode to RGB, CMYK/YCCK to 4 channels).
///
/// # Errors
///
/// Returns an error message if libjpeg-turbo rejects the stream (malformed / truncated input).
pub fn decode(data: &[u8]) -> Result<DecodedImage, String> {
    decode_impl(data, false)
}

/// Decodes a JPEG with libjpeg-turbo, forcing the output colour space to RGB (grayscale is
/// replicated across the three channels) — convenient for channel-agnostic parity tests.
///
/// # Errors
///
/// Returns an error message if libjpeg-turbo rejects the stream (malformed / truncated input).
pub fn decode_forced_rgb(data: &[u8]) -> Result<DecodedImage, String> {
    decode_impl(data, true)
}

fn decode_impl(data: &[u8], force_rgb: bool) -> Result<DecodedImage, String> {
    let mut width: c_uint = 0;
    let mut height: c_uint = 0;
    let mut channels: c_uint = 0;
    let mut pixels_ptr: *mut u8 = std::ptr::null_mut();
    let mut len: usize = 0;

    // SAFETY: `data` is a valid slice; all out-params are valid pointers. On success the shim sets
    // `pixels_ptr`/`len` to a malloc'd buffer that `take_owned` copies out and frees.
    let rc = unsafe {
        oracle_jpeg_decode(
            data.as_ptr(),
            data.len(),
            c_int::from(force_rgb),
            &raw mut width,
            &raw mut height,
            &raw mut channels,
            &raw mut pixels_ptr,
            &raw mut len,
        )
    };

    match rc {
        0 => {
            let pixels = unsafe { take_owned(pixels_ptr, len) };
            Ok(DecodedImage {
                width,
                height,
                channels,
                pixels,
            })
        }
        2 => Err("libjpeg-oracle: allocation failed while decoding".to_string()),
        _ => Err("libjpeg-oracle: libjpeg-turbo rejected the JPEG stream".to_string()),
    }
}

/// Encodes interleaved 8-bit `pixels` (`width * height * channels`, where channels is 1 when
/// `params.gray` else 3) to a JPEG with libjpeg-turbo.
///
/// # Errors
///
/// Returns an error message if the input is too small for the given dimensions, or if
/// libjpeg-turbo reports an encoding error.
pub fn encode(
    pixels: &[u8],
    width: u32,
    height: u32,
    params: &EncodeParams,
) -> Result<Vec<u8>, String> {
    let channels = if params.gray { 1usize } else { 3usize };
    let needed = width as usize * height as usize * channels;
    if pixels.len() < needed {
        return Err(format!(
            "libjpeg-oracle: pixel buffer holds {} bytes, need {needed} for {width}x{height}x{channels}",
            pixels.len()
        ));
    }

    let (h_samp, v_samp) = params.subsampling.factors();
    let mut out_ptr: *mut u8 = std::ptr::null_mut();
    let mut out_len: usize = 0;

    // SAFETY: `pixels` covers `needed` bytes (checked above); out-params are valid pointers. On
    // success the shim sets `out_ptr`/`out_len` to a malloc'd buffer copied+freed by `take_owned`.
    let rc = unsafe {
        oracle_jpeg_encode(
            pixels.as_ptr(),
            width,
            height,
            c_int::from(params.gray),
            params.quality,
            h_samp,
            v_samp,
            c_int::from(params.progressive),
            c_uint::from(params.restart_interval),
            c_int::from(params.optimize_coding),
            &raw mut out_ptr,
            &raw mut out_len,
        )
    };

    if rc == 0 {
        Ok(unsafe { take_owned(out_ptr, out_len) })
    } else {
        Err("libjpeg-oracle: libjpeg-turbo failed to encode the image".to_string())
    }
}

/// Reads the ICC profile of a JPEG via libjpeg-turbo's `jpeg_read_icc_profile` (the reference
/// reassembly of the APP2 `ICC_PROFILE` chunk sequence). `Ok(None)` when the stream carries none.
///
/// # Errors
///
/// Returns an error message if libjpeg-turbo rejects the stream.
pub fn read_icc_profile(data: &[u8]) -> Result<Option<Vec<u8>>, String> {
    let mut ptr: *mut u8 = std::ptr::null_mut();
    let mut len: usize = 0;
    // SAFETY: `data` is a valid slice and the out-params are valid pointers. On success with a
    // profile present the shim sets `ptr`/`len` to a malloc'd buffer `take_owned` copies + frees.
    let rc = unsafe { oracle_jpeg_read_icc(data.as_ptr(), data.len(), &raw mut ptr, &raw mut len) };
    match rc {
        0 if ptr.is_null() => Ok(None),
        0 => Ok(Some(unsafe { take_owned(ptr, len) })),
        _ => Err("libjpeg-oracle: libjpeg-turbo rejected the JPEG stream".to_string()),
    }
}

/// The per-component sampling factors `(Hi, Vi)` libjpeg-turbo parsed from the frame header, in
/// frame order — what the reference decoder will upsample by, read independently of gamut's own
/// marker parser.
///
/// # Errors
///
/// Returns an error message if libjpeg-turbo rejects the stream's header.
pub fn sampling_factors(data: &[u8]) -> Result<Vec<(u8, u8)>, String> {
    // libjpeg caps a frame at MAX_COMPONENTS (10) components.
    const CAP: usize = 10;
    let (mut h, mut v) = ([0u8; CAP], [0u8; CAP]);
    let mut components: c_uint = 0;
    // SAFETY: `data` is a valid slice, `h`/`v` hold `CAP` bytes each (the shim writes at most
    // `cap` entries), and `components` is a valid out-param.
    let rc = unsafe {
        oracle_jpeg_sampling_factors(
            data.as_ptr(),
            data.len(),
            h.as_mut_ptr(),
            v.as_mut_ptr(),
            CAP as c_uint,
            &raw mut components,
        )
    };
    if rc != 0 {
        return Err("libjpeg-oracle: libjpeg-turbo rejected the JPEG header".to_string());
    }
    let n = (components as usize).min(CAP);
    Ok(h[..n].iter().copied().zip(v[..n].iter().copied()).collect())
}

/// Returns the raw payload of the first APP1 marker segment (e.g. `"Exif\0\0"` + TIFF), or
/// `Ok(None)` when the stream carries no APP1.
///
/// # Errors
///
/// Returns an error message if libjpeg-turbo rejects the stream or allocation fails.
pub fn read_first_app1(data: &[u8]) -> Result<Option<Vec<u8>>, String> {
    let mut ptr: *mut u8 = std::ptr::null_mut();
    let mut len: usize = 0;
    // SAFETY: as for `read_icc_profile`; the shim malloc's the copied marker payload.
    let rc =
        unsafe { oracle_jpeg_read_app1(data.as_ptr(), data.len(), &raw mut ptr, &raw mut len) };
    match rc {
        0 if ptr.is_null() => Ok(None),
        0 => Ok(Some(unsafe { take_owned(ptr, len) })),
        2 => Err("libjpeg-oracle: allocation failed while reading APP1".to_string()),
        _ => Err("libjpeg-oracle: libjpeg-turbo rejected the JPEG stream".to_string()),
    }
}

/// [`encode`] plus embedded metadata: an optional raw APP1 payload (written verbatim via
/// `jpeg_write_marker`) and an optional ICC profile (written via `jpeg_write_icc_profile`, the
/// reference producer of the APP2 `ICC_PROFILE` chunk sequence).
///
/// # Errors
///
/// Returns an error message if the input is too small for the given dimensions, or if
/// libjpeg-turbo reports an encoding error.
pub fn encode_with_metadata(
    pixels: &[u8],
    width: u32,
    height: u32,
    params: &EncodeParams,
    app1: Option<&[u8]>,
    icc: Option<&[u8]>,
) -> Result<Vec<u8>, String> {
    let channels = if params.gray { 1usize } else { 3usize };
    let needed = width as usize * height as usize * channels;
    if pixels.len() < needed {
        return Err(format!(
            "libjpeg-oracle: pixel buffer holds {} bytes, need {needed} for {width}x{height}x{channels}",
            pixels.len()
        ));
    }

    let (h_samp, v_samp) = params.subsampling.factors();
    let (app1_ptr, app1_len) = app1.map_or((std::ptr::null(), 0), |b| (b.as_ptr(), b.len()));
    let (icc_ptr, icc_len) = icc.map_or((std::ptr::null(), 0), |b| (b.as_ptr(), b.len()));
    let mut out_ptr: *mut u8 = std::ptr::null_mut();
    let mut out_len: usize = 0;

    // SAFETY: `pixels` covers `needed` bytes (checked above); the metadata pointers are either
    // null (absent) or cover their stated lengths; out-params are valid pointers. On success the
    // shim sets `out_ptr`/`out_len` to a malloc'd buffer copied + freed by `take_owned`.
    let rc = unsafe {
        oracle_jpeg_encode_meta(
            pixels.as_ptr(),
            width,
            height,
            c_int::from(params.gray),
            params.quality,
            h_samp,
            v_samp,
            c_int::from(params.progressive),
            c_uint::from(params.restart_interval),
            c_int::from(params.optimize_coding),
            app1_ptr,
            app1_len,
            icc_ptr,
            icc_len,
            &raw mut out_ptr,
            &raw mut out_len,
        )
    };

    if rc == 0 {
        Ok(unsafe { take_owned(out_ptr, out_len) })
    } else {
        Err("libjpeg-oracle: libjpeg-turbo failed to encode the image".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A smooth `width x height` RGB gradient — JPEG-friendly (low high-frequency content) so the
    /// round-trip tolerances stay tight.
    fn rgb_gradient(width: u32, height: u32) -> Vec<u8> {
        let mut px = Vec::with_capacity((width * height * 3) as usize);
        for y in 0..height {
            for x in 0..width {
                px.push((x * 255 / width.max(1)) as u8);
                px.push((y * 255 / height.max(1)) as u8);
                px.push((((x + y) * 255) / (width + height).max(1)) as u8);
            }
        }
        px
    }

    /// The single-channel luma of [`rgb_gradient`].
    fn gray_gradient(width: u32, height: u32) -> Vec<u8> {
        let mut px = Vec::with_capacity((width * height) as usize);
        for y in 0..height {
            for x in 0..width {
                px.push(((x * 255 / width.max(1)) + (y * 255 / height.max(1))) as u8 / 2);
            }
        }
        px
    }

    fn max_abs_diff(a: &[u8], b: &[u8]) -> u32 {
        assert_eq!(a.len(), b.len(), "compared buffers differ in length");
        a.iter()
            .zip(b)
            .map(|(&x, &y)| u32::from(x.abs_diff(y)))
            .max()
            .unwrap_or(0)
    }

    #[test]
    fn reports_libjpeg_turbo_3() {
        // Confirms the static lib, the shim, and the link line are all wired correctly.
        let v = version();
        assert!(v.starts_with("3."), "expected libjpeg-turbo 3.x, got {v:?}");
    }

    #[test]
    fn rgb_roundtrip_high_quality() {
        let (w, h) = (32, 24);
        let src = rgb_gradient(w, h);
        let params = EncodeParams {
            quality: 95,
            subsampling: Subsampling::S444, // keep chroma so the tolerance stays tight
            ..EncodeParams::default()
        };
        let jpeg = encode(&src, w, h, &params).expect("encode RGB");
        let img = decode(&jpeg).expect("decode RGB");
        assert_eq!((img.width, img.height, img.channels), (w, h, 3));
        assert_eq!(img.pixels.len(), src.len());
        let diff = max_abs_diff(&img.pixels, &src);
        assert!(diff <= 12, "q95 4:4:4 round-trip diff too large: {diff}");
    }

    #[test]
    fn gray_roundtrip() {
        let (w, h) = (40, 16);
        let src = gray_gradient(w, h);
        let params = EncodeParams {
            quality: 95,
            gray: true,
            ..EncodeParams::default()
        };
        let jpeg = encode(&src, w, h, &params).expect("encode gray");
        let img = decode(&jpeg).expect("decode gray");
        assert_eq!((img.width, img.height, img.channels), (w, h, 1));
        let diff = max_abs_diff(&img.pixels, &src);
        assert!(diff <= 12, "q95 gray round-trip diff too large: {diff}");
    }

    #[test]
    fn forced_rgb_replicates_grayscale() {
        let (w, h) = (16, 16);
        let src = gray_gradient(w, h);
        let jpeg = encode(
            &src,
            w,
            h,
            &EncodeParams {
                quality: 90,
                gray: true,
                ..EncodeParams::default()
            },
        )
        .expect("encode gray");

        let img = decode_forced_rgb(&jpeg).expect("decode forced RGB");
        assert_eq!(img.channels, 3, "forced RGB must yield 3 channels");
        // A grayscale source forced to RGB has R == G == B in every pixel.
        for px in img.pixels.as_chunks::<3>().0 {
            assert_eq!(px[0], px[1]);
            assert_eq!(px[1], px[2]);
        }
    }

    #[test]
    fn progressive_matches_sequential() {
        // Same quantised coefficients either way, so the decoded pixels must match closely; the two
        // encodings differ only in entropy-coding layout. This is the P4 progressive fixture path.
        let (w, h) = (48, 32);
        let src = rgb_gradient(w, h);
        let base = EncodeParams {
            quality: 85,
            subsampling: Subsampling::S444,
            ..EncodeParams::default()
        };
        let sequential = encode(&src, w, h, &base).expect("encode sequential");
        let progressive = encode(
            &src,
            w,
            h,
            &EncodeParams {
                progressive: true,
                ..base
            },
        )
        .expect("encode progressive");

        let seq_img = decode(&sequential).expect("decode sequential");
        let prog_img = decode(&progressive).expect("decode progressive");
        assert_eq!(seq_img.pixels.len(), prog_img.pixels.len());
        let diff = max_abs_diff(&seq_img.pixels, &prog_img.pixels);
        assert!(
            diff <= 2,
            "progressive vs sequential diff too large: {diff}"
        );
    }

    #[test]
    fn restart_interval_still_roundtrips() {
        let (w, h) = (32, 32);
        let src = rgb_gradient(w, h);
        let jpeg = encode(
            &src,
            w,
            h,
            &EncodeParams {
                quality: 90,
                subsampling: Subsampling::S444,
                restart_interval: 4,
                optimize_coding: true,
                ..EncodeParams::default()
            },
        )
        .expect("encode with restarts + optimized coding");
        let img = decode(&jpeg).expect("decode restarts");
        assert_eq!((img.width, img.height, img.channels), (w, h, 3));
        let diff = max_abs_diff(&img.pixels, &src);
        assert!(
            diff <= 12,
            "restart-interval round-trip diff too large: {diff}"
        );
    }

    #[test]
    fn sampling_factors_report_each_mode_and_gray() {
        let (w, h) = (16, 16);
        let src = rgb_gradient(w, h);
        for (mode, luma) in [
            (Subsampling::S444, (1, 1)),
            (Subsampling::S422, (2, 1)),
            (Subsampling::S420, (2, 2)),
            (Subsampling::S440, (1, 2)),
        ] {
            let params = EncodeParams {
                subsampling: mode,
                ..EncodeParams::default()
            };
            let jpeg = encode(&src, w, h, &params).expect("encode");
            assert_eq!(
                sampling_factors(&jpeg).expect("header"),
                vec![luma, (1, 1), (1, 1)],
                "{mode:?}"
            );
        }
        let gray = EncodeParams {
            gray: true,
            ..EncodeParams::default()
        };
        let jpeg = encode(&gray_gradient(w, h), w, h, &gray).expect("encode gray");
        assert_eq!(sampling_factors(&jpeg).expect("header"), vec![(1, 1)]);
        assert!(sampling_factors(&jpeg[..8]).is_err());
    }

    #[test]
    fn garbage_input_is_err_not_abort() {
        let garbage = [0xDEu8, 0xAD, 0xBE, 0xEF, 0x00, 0x11, 0x22, 0x33];
        assert!(
            decode(&garbage).is_err(),
            "garbage bytes must decode to Err"
        );
    }

    #[test]
    fn truncated_jpeg_is_err_not_abort() {
        let (w, h) = (16, 16);
        let jpeg = encode(&rgb_gradient(w, h), w, h, &EncodeParams::default()).expect("encode");
        // Keep only the SOI + a sliver of the header: not enough to read_header successfully.
        let truncated = &jpeg[..8.min(jpeg.len())];
        assert!(
            decode(truncated).is_err(),
            "truncated stream must decode to Err"
        );
    }
}
