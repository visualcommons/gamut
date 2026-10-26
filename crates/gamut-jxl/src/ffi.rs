//! The single `unsafe` module: a thin, RAII-wrapped driver over the reference libjxl encoder
//! (`gamut_jxl_sys::encode`).
//!
//! Everything that touches a raw pointer lives here, behind `#![allow(unsafe_code)]`, so the rest of
//! `gamut-jxl` stays under the crate-wide `#![deny(unsafe_code)]`. The public entry point is
//! [`encode`]: it takes a fully-described [`JxlImageRef`] plus the caller's [`JxlEncoder`] config,
//! runs libjxl's create → configure → add-frame → process-output sequence with **every** status
//! checked, and appends the encoded stream to the caller's buffer.
#![allow(unsafe_code)]

use core::ffi::c_void;

use gamut_core::{Error, Result};
use gamut_jxl_sys::{encode as sys_enc, types as sys_ty};

use crate::backend::{JxlImageRef, JxlSamples};
use crate::config::{ColorSpec, Mode, ModularMode, validate_icc};
use crate::encoder::{JxlEncoder, resolve_coded_bits};
use crate::error::map_encoder_error;

/// RAII owner of a libjxl encoder handle. [`Drop`] frees it (and, with it, every frame-settings
/// object libjxl created for it), so an early `?` return can never leak the encoder.
struct Encoder(*mut sys_enc::JxlEncoder);

impl Drop for Encoder {
    fn drop(&mut self) {
        // SAFETY: `self.0` is a non-null handle returned by `JxlEncoderCreate` (checked in `encode`),
        // owned solely by this wrapper and not yet destroyed; `Drop` runs exactly once.
        unsafe { sys_enc::JxlEncoderDestroy(self.0) };
    }
}

impl Encoder {
    /// Translates a libjxl status into a `Result`, reading the detailed error on failure.
    ///
    /// Used for the setup calls, none of which return `NEED_MORE_OUTPUT`; any non-`SUCCESS` status is
    /// therefore an `ERROR`, whose detail comes from `JxlEncoderGetError`.
    fn check(&self, status: sys_enc::JxlEncoderStatus) -> Result<()> {
        if status == sys_enc::JxlEncoderStatus::SUCCESS {
            Ok(())
        } else {
            // SAFETY: `self.0` is a valid, live encoder handle.
            let err = unsafe { sys_enc::JxlEncoderGetError(self.0) };
            Err(map_encoder_error(err))
        }
    }
}

/// Initial per-iteration output-buffer growth for the `ProcessOutput` loop: 64 KiB. libjxl only
/// requires `avail_out >= 32`; a 64 KiB first chunk covers small images in one pass, and doubling
/// (capped) amortises large ones without an unbounded single allocation. (Written as literals, not
/// `64 * 1024` arithmetic: the values are free choices, and literals generate no arithmetic
/// mutants to justify.)
const OUTPUT_CHUNK_INIT: usize = 65_536;
/// Upper bound on a single growth step: 64 MiB, so a pathological stream can't request one
/// enormous reservation.
const OUTPUT_CHUNK_MAX: usize = 67_108_864;

/// Encodes one frame described by `image`, using `cfg`'s mode/effort/container, appending the JPEG XL
/// stream to `out` and returning the number of bytes appended.
///
/// [`JxlImageRef`] already guarantees the samples have the right length and storage width, so this
/// does not re-validate them; it does defensively check that the frame's byte size does not overflow
/// `usize`.
///
/// # Errors
///
/// Returns [`Error::InvalidInput`] on a null encoder/frame-settings handle, a byte-size overflow, or
/// any libjxl error whose detail maps to invalid input; [`Error::Unsupported`] if libjxl reports the
/// configuration is unsupported.
pub(crate) fn encode(
    cfg: &JxlEncoder,
    image: &JxlImageRef<'_>,
    out: &mut Vec<u8>,
) -> Result<usize> {
    // Metadata boxes only exist in the ISO BMFF container. Rejecting the combination up front
    // keeps the configured framing authoritative instead of letting libjxl silently force the
    // container on.
    let has_boxes = cfg.exif().is_some() || cfg.xmp().is_some();
    if has_boxes && cfg.container() != crate::Container::IsoBmff {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "JXL: Exif/XMP metadata requires the ISO BMFF container",
        ));
    }
    if cfg.exif().is_some_and(<[u8]>::is_empty) || cfg.xmp().is_some_and(<[u8]>::is_empty) {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "JXL: empty metadata payload",
        ));
    }

    // The coded bit depth: the pixel layout's width, unless overridden to a narrower depth (an
    // N-bit image carried in a 16-bit buffer). A zero or wider-than-the-buffer override cannot
    // mean anything coherent, so it is a typed error.
    let coded_bits = resolve_coded_bits(cfg, image.bits_per_sample())?;

    let alpha_channels = u32::from(image.has_alpha());
    let total_channels = image.channels();
    let bytes_per_sample = (image.bits_per_sample() / 8) as usize;

    // Defensive overflow guard on the raw frame byte count. `ImageRef::new` validated the *sample*
    // length against the dimensions, but the FFI hands libjxl a byte length; compute it through the
    // same checked `Dimensions` arithmetic so an overflow is a typed error, never a wrap.
    let dims = image.dimensions();
    let num_samples = dims.sample_count(total_channels as usize).ok_or_else(|| {
        Error::invalid_input(env!("CARGO_PKG_NAME"), "JXL: image dimensions overflow")
    })?;
    let byte_len = num_samples.checked_mul(bytes_per_sample).ok_or_else(|| {
        Error::invalid_input(env!("CARGO_PKG_NAME"), "JXL: image dimensions overflow")
    })?;

    // The sample buffer pointer + data type. u16 crosses as native-endian bytes: the `&[u16]` is
    // already stored in native byte order and 2-byte aligned, and `JxlPixelFormat`'s NATIVE
    // endianness tells libjxl to read it exactly that way, so the reinterpretation is lossless.
    let (buffer, data_type) = match image.samples() {
        JxlSamples::U8(s) => {
            debug_assert_eq!(s.len(), num_samples);
            (s.as_ptr().cast::<c_void>(), sys_ty::JxlDataType::UINT8)
        }
        JxlSamples::U16(s) => {
            debug_assert_eq!(s.len(), num_samples);
            (s.as_ptr().cast::<c_void>(), sys_ty::JxlDataType::UINT16)
        }
    };

    // SAFETY: null memory manager selects libjxl's default allocator (documented in the sys crate).
    let handle = unsafe { sys_enc::JxlEncoderCreate(core::ptr::null()) };
    if handle.is_null() {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "JXL: encoder ran out of memory",
        ));
    }
    let enc = Encoder(handle);

    // Container framing. Must be set before any output is produced.
    let use_container = if cfg.container() == crate::Container::IsoBmff {
        sys_ty::JxlBool::TRUE
    } else {
        sys_ty::JxlBool::FALSE
    };
    // SAFETY: `enc.0` is a valid, freshly created encoder; no output has been produced.
    enc.check(unsafe { sys_enc::JxlEncoderUseContainer(enc.0, use_container) })?;

    if has_boxes {
        // SAFETY: `enc.0` is valid and no output has been produced yet.
        enc.check(unsafe { sys_enc::JxlEncoderUseBoxes(enc.0) })?;
    }

    // Basic info: initialise to defaults, then set only the fields we control.
    // Start from zeroed storage: all-zero is a valid bit pattern for every field of this POD
    // struct, so `assume_init` is sound even if a future libjxl leaves some field unwritten — our
    // soundness must not depend on the C library's internal memset behaviour.
    let mut info = core::mem::MaybeUninit::<sys_ty::JxlBasicInfo>::zeroed();
    // SAFETY: the storage is zeroed (a valid value), and `JxlEncoderInitBasicInfo` then writes the
    // libjxl defaults through the pointer.
    let mut info = unsafe {
        sys_enc::JxlEncoderInitBasicInfo(info.as_mut_ptr());
        info.assume_init()
    };
    info.xsize = dims.width;
    info.ysize = dims.height;
    info.bits_per_sample = coded_bits;
    info.exponent_bits_per_sample = 0;
    info.num_color_channels = image.color_channels();
    info.num_extra_channels = alpha_channels;
    info.alpha_bits = if image.has_alpha() { coded_bits } else { 0 };
    info.alpha_exponent_bits = 0;
    // The display orientation, as an EXIF 1..=8 value; the samples stay in coded order.
    info.orientation = sys_ty::JxlOrientation(cfg.orientation().exif_value().into());
    // Lossless must retain the original (non-XYB) profile; lossy re-encodes through XYB.
    info.uses_original_profile = match cfg.mode() {
        Mode::Lossless => sys_ty::JxlBool::TRUE,
        Mode::Lossy(_) => sys_ty::JxlBool::FALSE,
    };
    // SAFETY: `enc.0` is valid; `info` is fully initialised and copied internally.
    enc.check(unsafe { sys_enc::JxlEncoderSetBasicInfo(enc.0, &info) })?;

    // Colour signalling: the configured ColorSpec, gray or colour to match the channel count. Must
    // follow SetBasicInfo.
    set_color(&enc, cfg.color(), image.color_channels() == 1)?;

    // Frame settings are owned by the encoder (freed on destroy), so they need no separate RAII.
    // SAFETY: `enc.0` is valid; a null source means "copy from defaults".
    let frame_settings =
        unsafe { sys_enc::JxlEncoderFrameSettingsCreate(enc.0, core::ptr::null()) };
    if frame_settings.is_null() {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "JXL: encoder ran out of memory",
        ));
    }

    match cfg.mode() {
        Mode::Lossless => {
            // SAFETY: `frame_settings` is a valid pointer for the live encoder.
            enc.check(unsafe {
                sys_enc::JxlEncoderSetFrameLossless(frame_settings, sys_ty::JxlBool::TRUE)
            })?;
        }
        Mode::Lossy(distance) => {
            // SAFETY: as above.
            enc.check(unsafe {
                sys_enc::JxlEncoderSetFrameDistance(frame_settings, distance.get())
            })?;
        }
    }

    // SAFETY: `frame_settings` is valid; EFFORT takes an integer level in `1..=10`.
    enc.check(unsafe {
        sys_enc::JxlEncoderFrameSettingsSetOption(
            frame_settings,
            sys_enc::JxlEncoderFrameSettingId::EFFORT,
            i64::from(cfg.effort().level()),
        )
    })?;

    // The coding tool, only when the caller pinned one: leaving MODULAR unset keeps a default
    // encoder's bytes identical to what gamut produced before this knob existed. A lossless
    // encoder can never reach `VarDct` here — `JxlEncoder::dispatch_encode` rejects that
    // combination up front, because libjxl's `SetLossless` would silently override it.
    if cfg.modular() != ModularMode::Auto {
        // SAFETY: `frame_settings` is valid; MODULAR takes -1 (auto), 0 (VarDCT) or 1 (modular).
        enc.check(unsafe {
            sys_enc::JxlEncoderFrameSettingsSetOption(
                frame_settings,
                sys_enc::JxlEncoderFrameSettingId::MODULAR,
                i64::from(cfg.modular().option_value()),
            )
        })?;
    }

    // The progression and decoding-speed overrides, each only when the caller set it: an unset
    // field keeps the libjxl encoder default by never reaching libjxl, the same way an unset
    // MODULAR does. libjxl accepts every value these enums can produce (tri-states in -1..=1,
    // PROGRESSIVE_DC in -1..=2, DECODING_SPEED in 0..=4), and reconciles their interplay itself.
    // Written inline over the raw handle rather than as a helper taking `*mut` (a safe fn could not
    // enforce the pointer's validity); with no helper there is also no whole-body mutant here — the
    // forwarding is pinned by tests/frame_settings.rs instead.
    let fs = cfg.frame_settings();
    for (id, value) in [
        (
            sys_enc::JxlEncoderFrameSettingId::PROGRESSIVE_AC,
            fs.progressive_ac().option_value(),
        ),
        (
            sys_enc::JxlEncoderFrameSettingId::QPROGRESSIVE_AC,
            fs.qprogressive_ac().option_value(),
        ),
        (
            sys_enc::JxlEncoderFrameSettingId::PROGRESSIVE_DC,
            fs.progressive_dc().option_value(),
        ),
        (
            sys_enc::JxlEncoderFrameSettingId::RESPONSIVE,
            fs.responsive().option_value(),
        ),
        (
            sys_enc::JxlEncoderFrameSettingId::DECODING_SPEED,
            fs.decoding_speed().option_value(),
        ),
    ] {
        if let Some(value) = value {
            // SAFETY: `frame_settings` is valid for the live `enc` (null-checked above).
            enc.check(unsafe {
                sys_enc::JxlEncoderFrameSettingsSetOption(frame_settings, id, i64::from(value))
            })?;
        }
    }

    // With a coded-depth override, tell libjxl to read the integer input buffer at the basic
    // info's declared depth (from-codestream) instead of the pixel format's full range — without
    // this, a 10-bit image handed over as u16 would be rescaled from 16-bit.
    if cfg.bit_depth().is_some() {
        let bit_depth = sys_ty::JxlBitDepth {
            r#type: sys_ty::JxlBitDepthType::FROM_CODESTREAM,
            bits_per_sample: 0,
            exponent_bits_per_sample: 0,
        };
        // SAFETY: `frame_settings` is valid; `bit_depth` is a fully initialised value copied by
        // the call.
        enc.check(unsafe { sys_enc::JxlEncoderSetFrameBitDepth(frame_settings, &bit_depth) })?;
    }

    let format = sys_ty::JxlPixelFormat {
        num_channels: total_channels,
        data_type,
        endianness: sys_ty::JxlEndianness::NATIVE,
        align: 0,
    };
    // SAFETY: `frame_settings` is valid; `format` matches the basic-info dimensions; `buffer` points
    // to `byte_len` readable bytes (the caller's validated sample slice, reinterpreted as bytes).
    enc.check(unsafe {
        sys_enc::JxlEncoderAddImageFrame(frame_settings, &format, buffer, byte_len)
    })?;

    // Metadata boxes, appended after the frame. The `Exif` box format requires a 4-byte
    // big-endian offset to the tiff header before the payload; gamut takes the raw EXIF data and
    // prepends the standard zero offset itself. XMP goes verbatim into an `xml ` box. Neither is
    // Brotli-compressed (`compress_box = FALSE`): the bytes must stay byte-exact for tests and
    // external readers that do not implement `brob` unwrapping.
    if let Some(exif) = cfg.exif() {
        let mut payload = Vec::with_capacity(4 + exif.len());
        payload.extend_from_slice(&[0, 0, 0, 0]);
        payload.extend_from_slice(exif);
        add_box(&enc, b"Exif", &payload)?;
    }
    if let Some(xmp) = cfg.xmp() {
        add_box(&enc, b"xml ", xmp)?;
    }

    // SAFETY: `enc.0` is valid; no further frames or boxes will be added.
    unsafe { sys_enc::JxlEncoderCloseInput(enc.0) };

    drain(&enc, out)
}

/// Adds one uncompressed metadata box of the given 4-byte type to the container output.
fn add_box(enc: &Encoder, box_type: &[u8; 4], contents: &[u8]) -> Result<()> {
    // SAFETY: `enc.0` is valid with `JxlEncoderUseBoxes` enabled; `box_type` points to exactly 4
    // readable bytes and `contents` to `contents.len()` readable bytes, both copied internally.
    enc.check(unsafe {
        sys_enc::JxlEncoderAddBox(
            enc.0,
            box_type.as_ptr().cast::<core::ffi::c_char>(),
            contents.as_ptr(),
            contents.len(),
            sys_ty::JxlBool::FALSE,
        )
    })
}

/// Losslessly transcodes a complete JPEG codestream into a JPEG XL container with JPEG
/// reconstruction metadata (the `jbrd` box), appending to `out` and returning the number of bytes
/// appended.
///
/// The output is always ISO BMFF container framing: `JxlEncoderStoreJPEGMetadata` requires the
/// container to carry the `jbrd` box, so the caller's [`crate::Container`] choice does not apply
/// here. Image parameters (dimensions, bit depth, colour encoding) are implied by libjxl from the
/// JPEG itself; only the configured [`crate::Effort`] is forwarded.
///
/// # Errors
///
/// Returns [`Error::InvalidInput`] on an empty or rejected JPEG codestream (progressive features
/// libjxl cannot represent reversibly surface as the JBRD-specific [`Error::Unsupported`]), and
/// [`Error::Unsupported`] if reconstruction metadata cannot represent the input.
pub(crate) fn recompress(cfg: &JxlEncoder, jpeg: &[u8], out: &mut Vec<u8>) -> Result<usize> {
    if jpeg.is_empty() {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "JXL: empty JPEG input",
        ));
    }

    // SAFETY: null memory manager selects libjxl's default allocator (documented in the sys crate).
    let handle = unsafe { sys_enc::JxlEncoderCreate(core::ptr::null()) };
    if handle.is_null() {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "JXL: encoder ran out of memory",
        ));
    }
    let enc = Encoder(handle);

    // Reconstruction metadata lives in a `jbrd` container box, so container framing is mandatory
    // on this path; set it explicitly rather than relying on libjxl forcing it implicitly.
    // SAFETY: `enc.0` is a valid, freshly created encoder; no output has been produced.
    enc.check(unsafe { sys_enc::JxlEncoderUseContainer(enc.0, sys_ty::JxlBool::TRUE) })?;
    // SAFETY: as above; must be set before encoding starts.
    enc.check(unsafe { sys_enc::JxlEncoderStoreJPEGMetadata(enc.0, sys_ty::JxlBool::TRUE) })?;

    // Frame settings are owned by the encoder (freed on destroy), so they need no separate RAII.
    // SAFETY: `enc.0` is valid; a null source means "copy from defaults".
    let frame_settings =
        unsafe { sys_enc::JxlEncoderFrameSettingsCreate(enc.0, core::ptr::null()) };
    if frame_settings.is_null() {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "JXL: encoder ran out of memory",
        ));
    }

    // SAFETY: `frame_settings` is valid; EFFORT takes an integer level in `1..=10`.
    enc.check(unsafe {
        sys_enc::JxlEncoderFrameSettingsSetOption(
            frame_settings,
            sys_enc::JxlEncoderFrameSettingId::EFFORT,
            i64::from(cfg.effort().level()),
        )
    })?;

    // Basic info and colour encoding are implied from the JPEG frame by libjxl.
    // SAFETY: `frame_settings` is valid; `jpeg` points to `jpeg.len()` readable bytes, copied
    // internally.
    enc.check(unsafe {
        sys_enc::JxlEncoderAddJPEGFrame(frame_settings, jpeg.as_ptr(), jpeg.len())
    })?;

    // SAFETY: `enc.0` is valid; no further frames or boxes will be added.
    unsafe { sys_enc::JxlEncoderCloseInput(enc.0) };

    drain(&enc, out)
}

/// Signals the frame's colour interpretation to libjxl: a structured encoding for the built-in
/// [`ColorSpec`] variants, or the verbatim ICC profile bytes for [`ColorSpec::Icc`]. Must be called
/// after `JxlEncoderSetBasicInfo`.
fn set_color(enc: &Encoder, spec: &ColorSpec, gray: bool) -> Result<()> {
    let is_gray = if gray {
        sys_ty::JxlBool::TRUE
    } else {
        sys_ty::JxlBool::FALSE
    };

    let color = match spec {
        ColorSpec::Srgb | ColorSpec::LinearSrgb => {
            // Zeroed for soundness: all-zero is a valid bit pattern for every field, so no field's
            // initialisation depends on the C call's coverage.
            let mut color = core::mem::MaybeUninit::<sys_ty::JxlColorEncoding>::zeroed();
            // SAFETY: the storage is zeroed (a valid value), and the libjxl helper then writes the
            // requested sRGB/linear-sRGB profile through the pointer.
            unsafe {
                if *spec == ColorSpec::Srgb {
                    sys_enc::JxlColorEncodingSetToSRGB(color.as_mut_ptr(), is_gray);
                } else {
                    sys_enc::JxlColorEncodingSetToLinearSRGB(color.as_mut_ptr(), is_gray);
                }
                color.assume_init()
            }
        }
        ColorSpec::Pq | ColorSpec::Hlg => {
            // BT.2100: D65 white point, BT.2100 primaries (ignored for gray), and the PQ or HLG
            // transfer function. Built by hand — libjxl has no SetTo* helper for HDR encodings.
            sys_ty::JxlColorEncoding {
                color_space: if gray {
                    sys_ty::JxlColorSpace::GRAY
                } else {
                    sys_ty::JxlColorSpace::RGB
                },
                white_point: sys_ty::JxlWhitePoint::D65,
                white_point_xy: [0.0; 2],
                primaries: sys_ty::JxlPrimaries::BT2100,
                primaries_red_xy: [0.0; 2],
                primaries_green_xy: [0.0; 2],
                primaries_blue_xy: [0.0; 2],
                transfer_function: if *spec == ColorSpec::Pq {
                    sys_ty::JxlTransferFunction::PQ
                } else {
                    sys_ty::JxlTransferFunction::HLG
                },
                gamma: 0.0,
                rendering_intent: sys_ty::JxlRenderingIntent::RELATIVE,
            }
        }
        ColorSpec::Icc(icc) => {
            validate_icc(icc, gray)?;
            // SAFETY: `enc.0` is valid; `icc` points to `icc.len()` readable bytes, copied
            // internally.
            return enc.check(unsafe {
                sys_enc::JxlEncoderSetICCProfile(enc.0, icc.as_ptr(), icc.len())
            });
        }
    };

    // SAFETY: `enc.0` is valid; `color` is fully initialised and copied internally.
    enc.check(unsafe { sys_enc::JxlEncoderSetColorEncoding(enc.0, &color) })
}

/// Drains all pending encoder output into `out`, appending after the current contents and
/// returning the number of bytes appended.
///
/// `out` may already hold caller data, so every offset is relative to its length on entry, and on
/// any error the buffer is truncated back to exactly that length.
fn drain(enc: &Encoder, out: &mut Vec<u8>) -> Result<usize> {
    let start = out.len();
    let mut chunk = OUTPUT_CHUNK_INIT;
    loop {
        let offset = out.len();
        out.resize(offset + chunk, 0);
        // Take the pointer *after* the resize, which may have reallocated the buffer.
        // SAFETY: `offset <= out.len()`, so the pointer is in-bounds of the live allocation.
        let mut next_out = unsafe { out.as_mut_ptr().add(offset) };
        let mut avail_out = chunk;
        // SAFETY: `enc.0` is valid; `next_out`/`avail_out` describe a writable region of `chunk`
        // (>= 32) bytes and are updated in place.
        let status =
            unsafe { sys_enc::JxlEncoderProcessOutput(enc.0, &mut next_out, &mut avail_out) };
        let produced = chunk - avail_out;
        out.truncate(offset + produced);
        match status {
            sys_enc::JxlEncoderStatus::SUCCESS => break,
            sys_enc::JxlEncoderStatus::NEED_MORE_OUTPUT => {
                chunk = chunk.saturating_mul(2).min(OUTPUT_CHUNK_MAX);
            }
            // ERROR (or any unexpected status): surface the detailed encoder error.
            _ => {
                // SAFETY: `enc.0` is a valid, live encoder handle.
                let err = unsafe { sys_enc::JxlEncoderGetError(enc.0) };
                out.truncate(start);
                return Err(map_encoder_error(err));
            }
        }
    }

    Ok(out.len() - start)
}
