//! JPEG XL seam handles: [`GamutJxlDecoder`] / [`GamutJxlEncoder`] and their C entry points.
//!
//! The JXL seam is the bare `FF 0A` codestream (`JXL_CODEC_ID`). Backends are tried in push
//! order; the built-in tails (the jxl-rs decoder, and on non-wasm targets the libjxl encoder)
//! come last. Container-level features (ISOBMFF, Exif/XMP, jbrd) stay pinned to the built-in
//! path by the host-side veto.

use core::ffi::c_void;

use gamut_codec_abi::{DecoderVTable, EncoderVTable};

use crate::seam::seam_handle;
use crate::status::GamutStatus;

/// Opaque handle over a `gamut::jxl::JxlDecoder` and its backend registry.
pub struct GamutJxlDecoder {
    inner: gamut::jxl::JxlDecoder,
}

/// Opaque handle over a `gamut::jxl::JxlEncoder` and its backend registry.
pub struct GamutJxlEncoder {
    inner: gamut::jxl::JxlEncoder,
}

seam_handle! {
    handle = GamutJxlDecoder,
    host = gamut::jxl::JxlDecoder,
    new = gamut::jxl::JxlDecoder::new(),
    seam = decoder,
    push = |host, backend| {
        *host = core::mem::take(host).with_backend(gamut::jxl::AbiJxlDecoder::new(backend));
    },
    fns = (gamut_jxl_decoder_new, gamut_jxl_decoder_free, gamut_jxl_decoder_push_backend),
    tests = jxl_decoder_tests,
}

seam_handle! {
    handle = GamutJxlEncoder,
    host = gamut::jxl::JxlEncoder,
    new = gamut::jxl::JxlEncoder::new(),
    seam = encoder,
    push = |host, backend| {
        *host = core::mem::take(host).with_backend(gamut::jxl::AbiJxlEncoder::new(backend));
    },
    fns = (gamut_jxl_encoder_new, gamut_jxl_encoder_free, gamut_jxl_encoder_push_backend),
    tests = jxl_encoder_tests,
}

/// Creates a JPEG XL decoder with default settings and an empty backend registry.
///
/// Returns `NULL` only if construction panics. Free with `gamut_jxl_decoder_free`.
#[unsafe(no_mangle)]
pub extern "C" fn gamut_jxl_decoder_new() -> *mut GamutJxlDecoder {
    GamutJxlDecoder::ffi_new()
}

/// Frees a decoder created by `gamut_jxl_decoder_new`, running each adopted backend's
/// `destroy` exactly once. No-op on `NULL`.
///
/// # Safety
///
/// `decoder` is `NULL` or a pointer returned by `gamut_jxl_decoder_new` that has not already
/// been freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gamut_jxl_decoder_free(decoder: *mut GamutJxlDecoder) {
    unsafe { GamutJxlDecoder::ffi_free(decoder) }
}

/// Pushes a JXL codestream-decode backend; backends are tried in push order, before the
/// built-in decoder.
///
/// Returns `GAMUT_OK` when the handle adopts `ctx` (its `destroy` then runs exactly once, at
/// free), `GAMUT_STATUS_NULL_ARGUMENT` on a `NULL` decoder or vtable, or
/// `GAMUT_STATUS_ABI_MISMATCH` when `vtable->abi_version != GAMUT_CODEC_ABI_VERSION`; on any
/// non-OK status the caller keeps ownership of `ctx` and no callback has run.
///
/// # Safety
///
/// `decoder` as in `gamut_jxl_decoder_free`; a non-`NULL` `vtable` points to a table that
/// stays valid for the decoder's lifetime. Calling this asserts the `(vtable, ctx)` backend
/// may be used from any thread (the registry requires `Send`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gamut_jxl_decoder_push_backend(
    decoder: *mut GamutJxlDecoder,
    vtable: *const DecoderVTable,
    ctx: *mut c_void,
) -> GamutStatus {
    unsafe { GamutJxlDecoder::ffi_push_backend(decoder, vtable, ctx) }
}

/// Creates a JPEG XL encoder with default settings and an empty backend registry.
///
/// Returns `NULL` only if construction panics. Free with `gamut_jxl_encoder_free`.
#[unsafe(no_mangle)]
pub extern "C" fn gamut_jxl_encoder_new() -> *mut GamutJxlEncoder {
    GamutJxlEncoder::ffi_new()
}

/// Frees an encoder created by `gamut_jxl_encoder_new`, running each adopted backend's
/// `destroy` exactly once. No-op on `NULL`.
///
/// # Safety
///
/// `encoder` is `NULL` or a pointer returned by `gamut_jxl_encoder_new` that has not already
/// been freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gamut_jxl_encoder_free(encoder: *mut GamutJxlEncoder) {
    unsafe { GamutJxlEncoder::ffi_free(encoder) }
}

/// Pushes a JXL codestream-encode backend; backends are tried in push order, before the
/// built-in encoder.
///
/// Status and ownership contract as for `gamut_jxl_decoder_push_backend`.
///
/// # Safety
///
/// `encoder` as in `gamut_jxl_encoder_free`; a non-`NULL` `vtable` points to a table that
/// stays valid for the encoder's lifetime. Calling this asserts the `(vtable, ctx)` backend
/// may be used from any thread (the registry requires `Send`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gamut_jxl_encoder_push_backend(
    encoder: *mut GamutJxlEncoder,
    vtable: *const EncoderVTable,
    ctx: *mut c_void,
) -> GamutStatus {
    unsafe { GamutJxlEncoder::ffi_push_backend(encoder, vtable, ctx) }
}
