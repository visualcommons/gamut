//! Dev-only differential oracle around a vendored, statically-linked **jpegli** (libjxl v0.11.2's
//! `lib/jpegli`, compiled from the `jpegxl-src` 0.11.4 package; see `Cargo.toml` for why that
//! release).
//!
//! gamut-jpeg's XYB colour mode follows jpegli's, and an XYB JPEG is only useful if jpegli's
//! decoder reads it: this crate wraps jpegli's encoder ([`encode`], including the XYB mode `cjpegli
//! --xyb` drives) and decoder ([`decode`]) behind a safe API returning owned buffers, plus the XYB
//! ICC profile libjxl itself generates ([`xyb_icc_profile`]).
//!
//! jpegli reports fatal errors through `longjmp`, so the C++ surface lives behind the
//! `src/shim.cc` bridge, which owns the `setjmp` guard and reports failure through return codes; a
//! malformed input yields an `Err` rather than aborting the process. All `unsafe` FFI is confined
//! to this crate.
//!
//! jpegli, like every libjpeg-API decoder, applies **no colour management**: an XYB stream decodes
//! to its stored scaled-XYB samples with the profile beside them, and turning those into sRGB is a
//! CMS's job (libjxl's own tools hand the pair to skcms/lcms2). The gamut tests do that step with
//! the lcms2 oracle.

use std::os::raw::{c_int, c_uint};

unsafe extern "C" {
    fn oracle_jpegli_free(p: *mut u8);
    fn oracle_jpegli_xyb_icc(out: *mut *mut u8, out_len: *mut usize) -> c_int;
    #[allow(clippy::too_many_arguments)]
    fn oracle_jpegli_encode(
        rgb: *const u8,
        width: c_uint,
        height: c_uint,
        quality: c_int,
        xyb: c_int,
        xyb_full_res: c_int,
        out: *mut *mut u8,
        out_len: *mut usize,
    ) -> c_int;
    #[allow(clippy::too_many_arguments)]
    fn oracle_jpegli_decode(
        data: *const u8,
        len: usize,
        width: *mut c_uint,
        height: *mut c_uint,
        channels: *mut c_uint,
        pixels: *mut *mut u8,
        pixels_len: *mut usize,
        icc: *mut *mut u8,
        icc_len: *mut usize,
    ) -> c_int;
}

/// Copies a shim-allocated buffer into a `Vec` and frees the original.
///
/// # Safety
///
/// `ptr` must be null or a `malloc` allocation of at least `len` bytes that nothing else owns.
unsafe fn take(ptr: *mut u8, len: usize) -> Option<Vec<u8>> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: the caller guarantees `ptr` addresses `len` initialized bytes it owns.
    let v = unsafe { std::slice::from_raw_parts(ptr, len) }.to_vec();
    // SAFETY: `ptr` came from `malloc` in the shim and is released exactly once here.
    unsafe { oracle_jpegli_free(ptr) };
    Some(v)
}

/// The XYB colour mode's channel sampling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XybSampling {
    /// jpegli's default: X and Y at 2x2, B at 1x1 — only B is subsampled, 2:1 both ways.
    SubsampledB,
    /// Every component at 1x1 (4:4:4).
    Full,
}

/// Reference-encoder parameters.
#[derive(Debug, Clone, Copy)]
pub struct EncodeParams {
    /// jpegli quality (1..=100), mapped to a butteraugli distance as `cjpegli -q` maps it.
    pub quality: i32,
    /// `Some` selects jpegli's XYB mode with that sampling; `None` encodes YCbCr 4:4:4.
    pub xyb: Option<XybSampling>,
}

/// Encodes interleaved sRGB `rgb` (`width * height * 3` bytes) with jpegli.
///
/// In XYB mode the stream is what `cjpegli --xyb` writes: libjxl's sRGB → scaled-XYB transform,
/// libjxl's XYB ICC profile embedded, jpegli's three XYB quantization tables, adaptive
/// quantization, optimized Huffman coding and progressive level 2.
///
/// # Errors
///
/// A message when the buffer size does not match the dimensions or jpegli fails.
pub fn encode(
    rgb: &[u8],
    width: u32,
    height: u32,
    params: EncodeParams,
) -> Result<Vec<u8>, String> {
    let expected = width as usize * height as usize * 3;
    if rgb.len() != expected {
        return Err(format!(
            "pixel buffer is {} bytes, expected {expected}",
            rgb.len()
        ));
    }
    let mut out = std::ptr::null_mut();
    let mut out_len = 0usize;
    // SAFETY: `rgb` holds `width * height * 3` bytes (checked above); the out-params are valid.
    let rc = unsafe {
        oracle_jpegli_encode(
            rgb.as_ptr(),
            width,
            height,
            params.quality,
            c_int::from(params.xyb.is_some()),
            c_int::from(params.xyb == Some(XybSampling::Full)),
            &mut out,
            &mut out_len,
        )
    };
    if rc != 0 {
        return Err(format!("jpegli encode failed (code {rc})"));
    }
    // SAFETY: on success the shim hands over a malloc'd buffer of `out_len` bytes.
    unsafe { take(out, out_len) }.ok_or_else(|| "jpegli produced no output".to_owned())
}

/// A JPEG decoded by jpegli.
#[derive(Debug, Clone)]
pub struct DecodedImage {
    /// Image width in pixels.
    pub width: u32,
    /// Image height in pixels.
    pub height: u32,
    /// Samples per pixel (1 = grayscale, 3 = RGB or stored XYB, 4 = CMYK).
    pub channels: u32,
    /// Interleaved samples, tightly packed.
    pub pixels: Vec<u8>,
    /// The embedded ICC profile, reassembled from its APP2 chunks.
    pub icc: Option<Vec<u8>>,
}

/// Decodes `data` with jpegli's default settings (fancy upsampling, its own IDCT). No colour
/// management is applied; see the crate docs.
///
/// # Errors
///
/// A message when jpegli rejects the stream.
pub fn decode(data: &[u8]) -> Result<DecodedImage, String> {
    let (mut w, mut h, mut ch) = (0, 0, 0);
    let (mut px, mut px_len) = (std::ptr::null_mut(), 0usize);
    let (mut icc, mut icc_len) = (std::ptr::null_mut(), 0usize);
    // SAFETY: `data` is a valid slice; every out-param points to a live local.
    let rc = unsafe {
        oracle_jpegli_decode(
            data.as_ptr(),
            data.len(),
            &mut w,
            &mut h,
            &mut ch,
            &mut px,
            &mut px_len,
            &mut icc,
            &mut icc_len,
        )
    };
    if rc != 0 {
        return Err(format!("jpegli decode failed (code {rc})"));
    }
    // SAFETY: on success both buffers (the profile may be null) are the shim's malloc'd output.
    let pixels = unsafe { take(px, px_len) }.unwrap_or_default();
    // SAFETY: as above.
    let icc = unsafe { take(icc, icc_len) };
    Ok(DecodedImage {
        width: w,
        height: h,
        channels: ch,
        pixels,
        icc,
    })
}

/// The ICC profile libjxl generates for the scaled-XYB encoding — the bytes `cjpegli --xyb` embeds.
#[must_use]
pub fn xyb_icc_profile() -> Vec<u8> {
    let mut out = std::ptr::null_mut();
    let mut len = 0usize;
    // SAFETY: both out-params point to live locals.
    let rc = unsafe { oracle_jpegli_xyb_icc(&mut out, &mut len) };
    assert_eq!(rc, 0, "libjxl failed to create its XYB profile");
    // SAFETY: on success the shim hands over a malloc'd buffer of `len` bytes.
    unsafe { take(out, len) }.expect("profile buffer")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xyb_stream_carries_libjxls_profile_and_decodes_to_its_geometry() {
        let (w, h) = (19u32, 13u32);
        let rgb: Vec<u8> = (0..w * h * 3).map(|i| (i * 7 % 256) as u8).collect();
        for sampling in [XybSampling::SubsampledB, XybSampling::Full] {
            let jpeg = encode(
                &rgb,
                w,
                h,
                EncodeParams {
                    quality: 90,
                    xyb: Some(sampling),
                },
            )
            .unwrap();
            let decoded = decode(&jpeg).unwrap();
            assert_eq!((decoded.width, decoded.height, decoded.channels), (w, h, 3));
            assert_eq!(decoded.icc, Some(xyb_icc_profile()));
        }
    }

    #[test]
    fn malformed_input_is_an_error() {
        assert!(decode(&[0xFF, 0xD8, 0xFF]).is_err());
        assert!(
            encode(
                &[0; 5],
                2,
                2,
                EncodeParams {
                    quality: 90,
                    xyb: None
                }
            )
            .is_err()
        );
    }
}
