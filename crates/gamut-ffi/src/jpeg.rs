//! JPEG seam handles: [`GamutJpegDecoder`] / [`GamutJpegEncoder`] and their C entry points.
//!
//! The JPEG seam is the whole `SOI..EOI` stream (`JPEG_CODEC_ID`). Backends are tried in push
//! order; the built-in gamut-jpeg codec is the implicit tail.

use core::ffi::c_void;

use gamut_codec_abi::{DecoderVTable, EncoderVTable};

use crate::seam::seam_handle;
use crate::status::GamutStatus;

/// Opaque handle over a `gamut::jpeg::JpegDecoder` and its backend registry.
pub struct GamutJpegDecoder {
    inner: gamut::jpeg::JpegDecoder,
}

/// Opaque handle over a `gamut::jpeg::JpegEncoder` and its backend registry.
pub struct GamutJpegEncoder {
    inner: gamut::jpeg::JpegEncoder,
}

seam_handle! {
    handle = GamutJpegDecoder,
    host = gamut::jpeg::JpegDecoder,
    new = gamut::jpeg::JpegDecoder::new(),
    seam = decoder,
    push = |host, backend| {
        *host = core::mem::take(host).with_backend(gamut::jpeg::AbiJpegDecoder::new(backend));
    },
    fns = (gamut_jpeg_decoder_new, gamut_jpeg_decoder_free, gamut_jpeg_decoder_push_backend),
    tests = jpeg_decoder_tests,
}

seam_handle! {
    handle = GamutJpegEncoder,
    host = gamut::jpeg::JpegEncoder,
    new = gamut::jpeg::JpegEncoder::new(),
    seam = encoder,
    push = |host, backend| {
        *host = core::mem::take(host).with_backend(gamut::jpeg::AbiJpegEncoder::new(backend));
    },
    fns = (gamut_jpeg_encoder_new, gamut_jpeg_encoder_free, gamut_jpeg_encoder_push_backend),
    tests = jpeg_encoder_tests,
}

/// Creates a JPEG decoder with default limits and an empty backend registry.
///
/// Returns `NULL` only if construction panics. Free with `gamut_jpeg_decoder_free`.
#[unsafe(no_mangle)]
pub extern "C" fn gamut_jpeg_decoder_new() -> *mut GamutJpegDecoder {
    GamutJpegDecoder::ffi_new()
}

/// Frees a decoder created by `gamut_jpeg_decoder_new`, running each adopted backend's
/// `destroy` exactly once. No-op on `NULL`.
///
/// # Safety
///
/// `decoder` is `NULL` or a pointer returned by `gamut_jpeg_decoder_new` that has not already
/// been freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gamut_jpeg_decoder_free(decoder: *mut GamutJpegDecoder) {
    unsafe { GamutJpegDecoder::ffi_free(decoder) }
}

/// Pushes a JPEG stream-decode backend; backends are tried in push order, before the built-in
/// decoder.
///
/// Returns `GAMUT_OK` when the handle adopts `ctx` (its `destroy` then runs exactly once, at
/// free), `GAMUT_STATUS_NULL_ARGUMENT` on a `NULL` decoder or vtable, or
/// `GAMUT_STATUS_ABI_MISMATCH` when `vtable->abi_version != GAMUT_CODEC_ABI_VERSION`; on any
/// non-OK status the caller keeps ownership of `ctx` and no callback has run.
///
/// # Safety
///
/// `decoder` as in `gamut_jpeg_decoder_free`; a non-`NULL` `vtable` points to a table that
/// stays valid for the decoder's lifetime. Calling this asserts the `(vtable, ctx)` backend
/// may be used from any thread (the registry requires `Send`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gamut_jpeg_decoder_push_backend(
    decoder: *mut GamutJpegDecoder,
    vtable: *const DecoderVTable,
    ctx: *mut c_void,
) -> GamutStatus {
    unsafe { GamutJpegDecoder::ffi_push_backend(decoder, vtable, ctx) }
}

/// Creates a JPEG encoder with default settings and an empty backend registry.
///
/// Returns `NULL` only if construction panics. Free with `gamut_jpeg_encoder_free`.
#[unsafe(no_mangle)]
pub extern "C" fn gamut_jpeg_encoder_new() -> *mut GamutJpegEncoder {
    GamutJpegEncoder::ffi_new()
}

/// Frees an encoder created by `gamut_jpeg_encoder_new`, running each adopted backend's
/// `destroy` exactly once. No-op on `NULL`.
///
/// # Safety
///
/// `encoder` is `NULL` or a pointer returned by `gamut_jpeg_encoder_new` that has not already
/// been freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gamut_jpeg_encoder_free(encoder: *mut GamutJpegEncoder) {
    unsafe { GamutJpegEncoder::ffi_free(encoder) }
}

/// Pushes a JPEG stream-encode backend; backends are tried in push order, before the built-in
/// encoder.
///
/// Status and ownership contract as for `gamut_jpeg_decoder_push_backend`.
///
/// # Safety
///
/// `encoder` as in `gamut_jpeg_encoder_free`; a non-`NULL` `vtable` points to a table that
/// stays valid for the encoder's lifetime. Calling this asserts the `(vtable, ctx)` backend
/// may be used from any thread (the registry requires `Send`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gamut_jpeg_encoder_push_backend(
    encoder: *mut GamutJpegEncoder,
    vtable: *const EncoderVTable,
    ctx: *mut c_void,
) -> GamutStatus {
    unsafe { GamutJpegEncoder::ffi_push_backend(encoder, vtable, ctx) }
}
