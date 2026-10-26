//! Test-only oracle: a safe wrapper over the reference libjxl **decoder** exposed by
//! `gamut-jxl-sys`, used to read back what `gamut-jxl`'s encoder produced, plus direct libjxl
//! **encoder** helpers for streams to compare against or that gamut never writes.
//!
//! Integration-test code is a separate crate and is not bound by `gamut-jxl`'s
//! `#![deny(unsafe_code)]`, so the raw FFI calls live here directly; `#![allow(dead_code)]` keeps the
//! shared helper usable from test files that only need part of it.
#![allow(unsafe_code)]
#![allow(dead_code)]

use core::ffi::c_void;
use core::mem::MaybeUninit;

use gamut_jxl_sys::{decode as dec, encode as enc, types as ty};

/// Decoded sample storage, matching the encoded bit depth.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodedSamples {
    /// 8-bit interleaved samples.
    U8(Vec<u8>),
    /// 16-bit interleaved samples (native-endian, as libjxl wrote them).
    U16(Vec<u16>),
}

/// A libjxl-decoded image: its dimensions, interleaved channel count, and samples.
#[derive(Debug, Clone)]
pub struct Decoded {
    /// Image width in pixels.
    pub width: u32,
    /// Image height in pixels.
    pub height: u32,
    /// Interleaved channels per pixel (colour + alpha).
    pub num_channels: u32,
    /// The decoded samples.
    pub samples: DecodedSamples,
}

/// RAII owner of a libjxl decoder handle.
struct Decoder(*mut dec::JxlDecoder);

impl Drop for Decoder {
    fn drop(&mut self) {
        unsafe { dec::JxlDecoderDestroy(self.0) };
    }
}

/// Decodes any JPEG XL stream (bare codestream or ISO BMFF container) with the reference libjxl
/// decoder, returning the image in the natural layout of the codestream: `num_color_channels` plus a
/// single alpha channel if any extra channel is present, at `UINT16` if the coded bit depth exceeds 8
/// and `UINT8` otherwise, in native endianness.
///
/// # Panics
///
/// Panics (with a descriptive message) on any decoder error — appropriate for a test oracle.
pub fn decode(data: &[u8]) -> Decoded {
    let handle = unsafe { dec::JxlDecoderCreate(core::ptr::null()) };
    assert!(!handle.is_null(), "JxlDecoderCreate returned null");
    let decoder = Decoder(handle);

    let events = dec::JxlDecoderStatus::BASIC_INFO.0 | dec::JxlDecoderStatus::FULL_IMAGE.0;
    let st = unsafe { dec::JxlDecoderSubscribeEvents(decoder.0, events) };
    assert_eq!(st, dec::JxlDecoderStatus::SUCCESS, "SubscribeEvents failed");
    let st = unsafe { dec::JxlDecoderSetInput(decoder.0, data.as_ptr(), data.len()) };
    assert_eq!(st, dec::JxlDecoderStatus::SUCCESS, "SetInput failed");
    unsafe { dec::JxlDecoderCloseInput(decoder.0) };

    let mut width = 0u32;
    let mut height = 0u32;
    let mut num_channels = 0u32;
    let mut use_u16 = false;
    let mut format: Option<ty::JxlPixelFormat> = None;
    let mut out_u8: Vec<u8> = Vec::new();
    let mut out_u16: Vec<u16> = Vec::new();

    loop {
        let status = unsafe { dec::JxlDecoderProcessInput(decoder.0) };
        if status == dec::JxlDecoderStatus::SUCCESS {
            break;
        } else if status == dec::JxlDecoderStatus::BASIC_INFO {
            let mut bi = MaybeUninit::<ty::JxlBasicInfo>::uninit();
            let st = unsafe { dec::JxlDecoderGetBasicInfo(decoder.0, bi.as_mut_ptr()) };
            assert_eq!(st, dec::JxlDecoderStatus::SUCCESS, "GetBasicInfo failed");
            let bi = unsafe { bi.assume_init() };
            width = bi.xsize;
            height = bi.ysize;
            let alpha = u32::from(bi.num_extra_channels > 0);
            num_channels = bi.num_color_channels + alpha;
            use_u16 = bi.bits_per_sample > 8;
            let data_type = if use_u16 {
                ty::JxlDataType::UINT16
            } else {
                ty::JxlDataType::UINT8
            };
            format = Some(ty::JxlPixelFormat {
                num_channels,
                data_type,
                endianness: ty::JxlEndianness::NATIVE,
                align: 0,
            });
        } else if status == dec::JxlDecoderStatus::NEED_IMAGE_OUT_BUFFER {
            let fmt = format.expect("BASIC_INFO must precede NEED_IMAGE_OUT_BUFFER");
            let mut size = 0usize;
            let st = unsafe { dec::JxlDecoderImageOutBufferSize(decoder.0, &fmt, &mut size) };
            assert_eq!(
                st,
                dec::JxlDecoderStatus::SUCCESS,
                "ImageOutBufferSize failed"
            );
            let st = if use_u16 {
                out_u16 = vec![0u16; size / 2];
                unsafe {
                    dec::JxlDecoderSetImageOutBuffer(
                        decoder.0,
                        &fmt,
                        out_u16.as_mut_ptr().cast::<c_void>(),
                        size,
                    )
                }
            } else {
                out_u8 = vec![0u8; size];
                unsafe {
                    dec::JxlDecoderSetImageOutBuffer(
                        decoder.0,
                        &fmt,
                        out_u8.as_mut_ptr().cast::<c_void>(),
                        size,
                    )
                }
            };
            assert_eq!(
                st,
                dec::JxlDecoderStatus::SUCCESS,
                "SetImageOutBuffer failed"
            );
        } else if status == dec::JxlDecoderStatus::FULL_IMAGE {
            // The frame is fully written into our buffer; keep processing to reach SUCCESS.
        } else {
            panic!("unexpected decoder status: {status:?}");
        }
    }

    let samples = if use_u16 {
        DecodedSamples::U16(out_u16)
    } else {
        DecodedSamples::U8(out_u8)
    };
    Decoded {
        width,
        height,
        num_channels,
        samples,
    }
}

/// Reconstructs the original JPEG codestream from a JPEG XL container carrying `jbrd`
/// reconstruction metadata, using the reference libjxl decoder.
///
/// Drives the `JPEG_RECONSTRUCTION` event: once libjxl reports the reconstruction data, a JPEG
/// output buffer is attached and grown on `JPEG_NEED_MORE_OUTPUT` until decoding completes; the
/// written prefix is returned.
///
/// # Panics
///
/// Panics (with a descriptive message) on any decoder error, or if the stream carries no JPEG
/// reconstruction data — appropriate for a test oracle.
pub fn reconstruct_jpeg(data: &[u8]) -> Vec<u8> {
    let handle = unsafe { dec::JxlDecoderCreate(core::ptr::null()) };
    assert!(!handle.is_null(), "JxlDecoderCreate returned null");
    let decoder = Decoder(handle);

    let events = dec::JxlDecoderStatus::JPEG_RECONSTRUCTION.0 | dec::JxlDecoderStatus::FULL_IMAGE.0;
    let st = unsafe { dec::JxlDecoderSubscribeEvents(decoder.0, events) };
    assert_eq!(st, dec::JxlDecoderStatus::SUCCESS, "SubscribeEvents failed");
    let st = unsafe { dec::JxlDecoderSetInput(decoder.0, data.as_ptr(), data.len()) };
    assert_eq!(st, dec::JxlDecoderStatus::SUCCESS, "SetInput failed");
    unsafe { dec::JxlDecoderCloseInput(decoder.0) };

    let mut jpeg = vec![0u8; 64 * 1024];
    let mut buffer_set = false;

    loop {
        let status = unsafe { dec::JxlDecoderProcessInput(decoder.0) };
        if status == dec::JxlDecoderStatus::SUCCESS {
            break;
        } else if status == dec::JxlDecoderStatus::JPEG_RECONSTRUCTION {
            let st =
                unsafe { dec::JxlDecoderSetJPEGBuffer(decoder.0, jpeg.as_mut_ptr(), jpeg.len()) };
            assert_eq!(st, dec::JxlDecoderStatus::SUCCESS, "SetJPEGBuffer failed");
            buffer_set = true;
        } else if status == dec::JxlDecoderStatus::JPEG_NEED_MORE_OUTPUT {
            // Grow: release to learn how much of the buffer is still unwritten, then re-attach
            // the enlarged buffer's unwritten tail.
            let unwritten = unsafe { dec::JxlDecoderReleaseJPEGBuffer(decoder.0) };
            let written = jpeg.len() - unwritten;
            jpeg.resize(jpeg.len() * 2, 0);
            let st = unsafe {
                dec::JxlDecoderSetJPEGBuffer(
                    decoder.0,
                    jpeg.as_mut_ptr().add(written),
                    jpeg.len() - written,
                )
            };
            assert_eq!(
                st,
                dec::JxlDecoderStatus::SUCCESS,
                "SetJPEGBuffer (regrow) failed"
            );
        } else if status == dec::JxlDecoderStatus::FULL_IMAGE {
            // The reconstructed JPEG is fully written; keep processing to reach SUCCESS.
        } else {
            panic!("unexpected decoder status during JPEG reconstruction: {status:?}");
        }
    }

    assert!(
        buffer_set,
        "stream carried no JPEG reconstruction (jbrd) data"
    );
    let unwritten = unsafe { dec::JxlDecoderReleaseJPEGBuffer(decoder.0) };
    jpeg.truncate(jpeg.len() - unwritten);
    jpeg
}

/// Runs a libjxl decoder up to the `COLOR_ENCODING` event and hands the live decoder to `read`.
///
/// Shared driver for the colour-profile oracle queries below.
fn with_color_encoding_event<T>(data: &[u8], read: impl FnOnce(*mut dec::JxlDecoder) -> T) -> T {
    let handle = unsafe { dec::JxlDecoderCreate(core::ptr::null()) };
    assert!(!handle.is_null(), "JxlDecoderCreate returned null");
    let decoder = Decoder(handle);

    let st = unsafe {
        dec::JxlDecoderSubscribeEvents(decoder.0, dec::JxlDecoderStatus::COLOR_ENCODING.0)
    };
    assert_eq!(st, dec::JxlDecoderStatus::SUCCESS, "SubscribeEvents failed");
    let st = unsafe { dec::JxlDecoderSetInput(decoder.0, data.as_ptr(), data.len()) };
    assert_eq!(st, dec::JxlDecoderStatus::SUCCESS, "SetInput failed");
    unsafe { dec::JxlDecoderCloseInput(decoder.0) };

    loop {
        let status = unsafe { dec::JxlDecoderProcessInput(decoder.0) };
        if status == dec::JxlDecoderStatus::COLOR_ENCODING {
            return read(decoder.0);
        }
        assert!(
            status != dec::JxlDecoderStatus::SUCCESS && status != dec::JxlDecoderStatus::ERROR,
            "decoder finished without a COLOR_ENCODING event: {status:?}"
        );
    }
}

/// Reads the **structured** colour encoding libjxl reports for the stream's original profile, or
/// `None` if the stream carries only an ICC profile (libjxl returns an error status then).
///
/// # Panics
///
/// Panics if the stream cannot be decoded to the colour-encoding event.
pub fn encoded_color_profile(data: &[u8]) -> Option<ty::JxlColorEncoding> {
    with_color_encoding_event(data, |dec| {
        let mut color = MaybeUninit::<ty::JxlColorEncoding>::zeroed();
        let st = unsafe {
            dec::JxlDecoderGetColorAsEncodedProfile(
                dec,
                dec::JxlColorProfileTarget::ORIGINAL,
                color.as_mut_ptr(),
            )
        };
        (st == dec::JxlDecoderStatus::SUCCESS).then(|| unsafe { color.assume_init() })
    })
}

/// Reads the ICC profile libjxl reports for the stream's original colour profile — the exact
/// attached bytes when the stream embeds one, or a profile synthesized from the structured
/// encoding otherwise. Returns `None` if libjxl cannot produce one.
///
/// # Panics
///
/// Panics if the stream cannot be decoded to the colour-encoding event.
pub fn icc_profile(data: &[u8]) -> Option<Vec<u8>> {
    with_color_encoding_event(data, |dec| {
        let mut size = 0usize;
        let st = unsafe {
            dec::JxlDecoderGetICCProfileSize(dec, dec::JxlColorProfileTarget::ORIGINAL, &mut size)
        };
        if st != dec::JxlDecoderStatus::SUCCESS {
            return None;
        }
        let mut icc = vec![0u8; size];
        let st = unsafe {
            dec::JxlDecoderGetColorAsICCProfile(
                dec,
                dec::JxlColorProfileTarget::ORIGINAL,
                icc.as_mut_ptr(),
                icc.len(),
            )
        };
        assert_eq!(
            st,
            dec::JxlDecoderStatus::SUCCESS,
            "GetColorAsICCProfile failed after a successful size query"
        );
        Some(icc)
    })
}

/// RAII owner of a libjxl encoder handle.
struct Encoder(*mut enc::JxlEncoder);

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe { enc::JxlEncoderDestroy(self.0) };
    }
}

/// Encodes an **animated** RGB8 JPEG XL codestream directly with the reference libjxl encoder: it
/// sets `have_animation` in the basic info and appends two frames. gamut's own encoder never does
/// this (it is image-first), so this test-only helper is the sole way to obtain an animated stream
/// to prove the decoder rejects it.
///
/// Returns the encoded bytes. Panics on any encoder error (appropriate for a test helper).
pub fn encode_animated_rgb8(width: u32, height: u32, frames: &[Vec<u8>]) -> Vec<u8> {
    assert!(!frames.is_empty(), "need at least one frame");
    let handle = unsafe { enc::JxlEncoderCreate(core::ptr::null()) };
    assert!(!handle.is_null(), "JxlEncoderCreate returned null");
    let encoder = Encoder(handle);

    // Basic info with animation enabled.
    let mut info = MaybeUninit::<ty::JxlBasicInfo>::zeroed();
    let mut info = unsafe {
        enc::JxlEncoderInitBasicInfo(info.as_mut_ptr());
        info.assume_init()
    };
    info.xsize = width;
    info.ysize = height;
    info.bits_per_sample = 8;
    info.exponent_bits_per_sample = 0;
    info.num_color_channels = 3;
    info.num_extra_channels = 0;
    info.alpha_bits = 0;
    info.uses_original_profile = ty::JxlBool::TRUE;
    info.have_animation = ty::JxlBool::TRUE;
    info.animation = ty::JxlAnimationHeader {
        tps_numerator: 1,
        tps_denominator: 1,
        num_loops: 0,
        have_timecodes: ty::JxlBool::FALSE,
    };
    let st = unsafe { enc::JxlEncoderSetBasicInfo(encoder.0, &info) };
    assert_eq!(st, enc::JxlEncoderStatus::SUCCESS, "SetBasicInfo failed");

    let mut color = MaybeUninit::<ty::JxlColorEncoding>::zeroed();
    let color = unsafe {
        enc::JxlColorEncodingSetToSRGB(color.as_mut_ptr(), ty::JxlBool::FALSE);
        color.assume_init()
    };
    let st = unsafe { enc::JxlEncoderSetColorEncoding(encoder.0, &color) };
    assert_eq!(
        st,
        enc::JxlEncoderStatus::SUCCESS,
        "SetColorEncoding failed"
    );

    let frame_settings =
        unsafe { enc::JxlEncoderFrameSettingsCreate(encoder.0, core::ptr::null()) };
    assert!(
        !frame_settings.is_null(),
        "FrameSettingsCreate returned null"
    );
    let st = unsafe { enc::JxlEncoderSetFrameLossless(frame_settings, ty::JxlBool::TRUE) };
    assert_eq!(
        st,
        enc::JxlEncoderStatus::SUCCESS,
        "SetFrameLossless failed"
    );

    let format = ty::JxlPixelFormat {
        num_channels: 3,
        data_type: ty::JxlDataType::UINT8,
        endianness: ty::JxlEndianness::NATIVE,
        align: 0,
    };
    let expected = width as usize * height as usize * 3;
    for frame in frames {
        assert_eq!(frame.len(), expected, "frame length mismatch");
        let st = unsafe {
            enc::JxlEncoderAddImageFrame(
                frame_settings,
                &format,
                frame.as_ptr().cast::<c_void>(),
                frame.len(),
            )
        };
        assert_eq!(st, enc::JxlEncoderStatus::SUCCESS, "AddImageFrame failed");
    }
    unsafe { enc::JxlEncoderCloseInput(encoder.0) };

    let mut out = vec![0u8; 64 * 1024];
    let mut produced = 0usize;
    loop {
        let mut next_out = unsafe { out.as_mut_ptr().add(produced) };
        let mut avail_out = out.len() - produced;
        let status =
            unsafe { enc::JxlEncoderProcessOutput(encoder.0, &mut next_out, &mut avail_out) };
        produced = out.len() - avail_out;
        match status {
            enc::JxlEncoderStatus::SUCCESS => break,
            enc::JxlEncoderStatus::NEED_MORE_OUTPUT => {
                out.resize(out.len() * 2, 0);
            }
            other => panic!("animated encode ProcessOutput failed: {other:?}"),
        }
    }
    out.truncate(produced);
    out
}

/// Encodes an sRGB RGB8 image as a bare codestream **directly through the libjxl C API**, setting
/// exactly the given integer frame settings after the mode and effort — the reference a gamut
/// encode with the same configuration must match byte for byte.
///
/// The call sequence is the one a plain libjxl client writes, independent of gamut's own driver:
/// no container, basic info for the image (`uses_original_profile` only when lossless), the sRGB
/// colour encoding, then one frame-settings object carrying lossless-or-distance, `EFFORT` and
/// `options` in order. `distance = None` means lossless.
///
/// Panics on any encoder error (appropriate for a test helper).
pub fn encode_direct_rgb8(
    width: u32,
    height: u32,
    pixels: &[u8],
    distance: Option<f32>,
    effort: i64,
    options: &[(enc::JxlEncoderFrameSettingId, i64)],
) -> Vec<u8> {
    assert_eq!(pixels.len(), width as usize * height as usize * 3);
    let handle = unsafe { enc::JxlEncoderCreate(core::ptr::null()) };
    assert!(!handle.is_null(), "JxlEncoderCreate returned null");
    let encoder = Encoder(handle);

    let st = unsafe { enc::JxlEncoderUseContainer(encoder.0, ty::JxlBool::FALSE) };
    assert_eq!(st, enc::JxlEncoderStatus::SUCCESS, "UseContainer failed");

    let mut info = MaybeUninit::<ty::JxlBasicInfo>::zeroed();
    let mut info = unsafe {
        enc::JxlEncoderInitBasicInfo(info.as_mut_ptr());
        info.assume_init()
    };
    info.xsize = width;
    info.ysize = height;
    info.bits_per_sample = 8;
    info.exponent_bits_per_sample = 0;
    info.num_color_channels = 3;
    info.num_extra_channels = 0;
    info.alpha_bits = 0;
    info.uses_original_profile = if distance.is_none() {
        ty::JxlBool::TRUE
    } else {
        ty::JxlBool::FALSE
    };
    let st = unsafe { enc::JxlEncoderSetBasicInfo(encoder.0, &info) };
    assert_eq!(st, enc::JxlEncoderStatus::SUCCESS, "SetBasicInfo failed");

    let mut color = MaybeUninit::<ty::JxlColorEncoding>::zeroed();
    let color = unsafe {
        enc::JxlColorEncodingSetToSRGB(color.as_mut_ptr(), ty::JxlBool::FALSE);
        color.assume_init()
    };
    let st = unsafe { enc::JxlEncoderSetColorEncoding(encoder.0, &color) };
    assert_eq!(
        st,
        enc::JxlEncoderStatus::SUCCESS,
        "SetColorEncoding failed"
    );

    let frame_settings =
        unsafe { enc::JxlEncoderFrameSettingsCreate(encoder.0, core::ptr::null()) };
    assert!(
        !frame_settings.is_null(),
        "FrameSettingsCreate returned null"
    );
    let st = match distance {
        None => unsafe { enc::JxlEncoderSetFrameLossless(frame_settings, ty::JxlBool::TRUE) },
        Some(d) => unsafe { enc::JxlEncoderSetFrameDistance(frame_settings, d) },
    };
    assert_eq!(st, enc::JxlEncoderStatus::SUCCESS, "mode setter failed");
    let st = unsafe {
        enc::JxlEncoderFrameSettingsSetOption(
            frame_settings,
            enc::JxlEncoderFrameSettingId::EFFORT,
            effort,
        )
    };
    assert_eq!(st, enc::JxlEncoderStatus::SUCCESS, "EFFORT failed");
    for &(id, value) in options {
        let st = unsafe { enc::JxlEncoderFrameSettingsSetOption(frame_settings, id, value) };
        assert_eq!(
            st,
            enc::JxlEncoderStatus::SUCCESS,
            "{id:?} = {value} failed"
        );
    }

    let format = ty::JxlPixelFormat {
        num_channels: 3,
        data_type: ty::JxlDataType::UINT8,
        endianness: ty::JxlEndianness::NATIVE,
        align: 0,
    };
    let st = unsafe {
        enc::JxlEncoderAddImageFrame(
            frame_settings,
            &format,
            pixels.as_ptr().cast::<c_void>(),
            pixels.len(),
        )
    };
    assert_eq!(st, enc::JxlEncoderStatus::SUCCESS, "AddImageFrame failed");
    unsafe { enc::JxlEncoderCloseInput(encoder.0) };

    let mut out = vec![0u8; 64 * 1024];
    let mut produced = 0usize;
    loop {
        let mut next_out = unsafe { out.as_mut_ptr().add(produced) };
        let mut avail_out = out.len() - produced;
        let status =
            unsafe { enc::JxlEncoderProcessOutput(encoder.0, &mut next_out, &mut avail_out) };
        produced = out.len() - avail_out;
        match status {
            enc::JxlEncoderStatus::SUCCESS => break,
            enc::JxlEncoderStatus::NEED_MORE_OUTPUT => {
                out.resize(out.len() * 2, 0);
            }
            other => panic!("direct encode ProcessOutput failed: {other:?}"),
        }
    }
    out.truncate(produced);
    out
}

/// A deterministic per-sample value in `0..=max`, non-flat so lossless is meaningfully exercised.
///
/// A low-frequency gradient plus a coarse `(x/8) ^ (y/8)` block texture and a per-channel offset. The
/// canonical generator shared by the differential test files; the 16-bit spread (`× 251`) fills the
/// wider range without aliasing the 8-bit pattern.
pub fn raw(x: u32, y: u32, c: u32, max: u32) -> u32 {
    let gradient = x.wrapping_mul(4).wrapping_add(y.wrapping_mul(3));
    let texture = ((x / 8) ^ (y / 8)).wrapping_mul(5);
    let channel = c.wrapping_mul(37);
    let base = gradient.wrapping_add(texture).wrapping_add(channel);
    let scale = if max > 0xFF { 251 } else { 1 };
    base.wrapping_mul(scale) & max
}

/// Generates `w × h` interleaved 8-bit samples with `ch` channels using [`raw`].
pub fn gen_u8(w: u32, h: u32, ch: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(w as usize * h as usize * ch);
    for y in 0..h {
        for x in 0..w {
            for c in 0..ch as u32 {
                v.push(raw(x, y, c, 0xFF) as u8);
            }
        }
    }
    v
}

/// Generates `w × h` interleaved 16-bit samples with `ch` channels using [`raw`].
pub fn gen_u16(w: u32, h: u32, ch: usize) -> Vec<u16> {
    let mut v = Vec::with_capacity(w as usize * h as usize * ch);
    for y in 0..h {
        for x in 0..w {
            for c in 0..ch as u32 {
                v.push(raw(x, y, c, 0xFFFF) as u16);
            }
        }
    }
    v
}

/// Peak signal-to-noise ratio in dB over two equal-length `u8` sample sets, treating them as a flat
/// signal in `0..=255`. Returns `f64::INFINITY` when identical.
pub fn psnr_u8(a: &[u8], b: &[u8]) -> f64 {
    assert_eq!(a.len(), b.len(), "PSNR inputs differ in length");
    let mut sse = 0.0f64;
    for (&x, &y) in a.iter().zip(b) {
        let d = f64::from(x) - f64::from(y);
        sse += d * d;
    }
    if sse == 0.0 {
        return f64::INFINITY;
    }
    let mse = sse / a.len() as f64;
    20.0 * 255.0f64.log10() - 10.0 * mse.log10()
}
