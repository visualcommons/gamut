//! WebP seam handles: [`GamutWebpDecoder`] / [`GamutWebpEncoder`] and their C entry points.
//!
//! The WebP seam is the raw RIFF chunk payload, identified per codestream by its FourCC
//! (`VP8 ` lossy / `VP8L` lossless). Backends are tried in push order; the built-in codec is
//! the implicit tail.

use core::ffi::c_void;

use gamut_codec_abi::{DecoderVTable, EncoderVTable};

use crate::seam::seam_handle;
use crate::status::GamutStatus;

/// Opaque handle over a `gamut::webp::WebpDecoder` and its backend registry.
pub struct GamutWebpDecoder {
    inner: gamut::webp::WebpDecoder,
}

/// Opaque handle over a `gamut::webp::WebpEncoder` and its backend registry.
pub struct GamutWebpEncoder {
    inner: gamut::webp::WebpEncoder,
}

seam_handle! {
    handle = GamutWebpDecoder,
    host = gamut::webp::WebpDecoder,
    new = gamut::webp::WebpDecoder::new(),
    seam = decoder,
    push = |host, backend| {
        *host = core::mem::take(host).with_backend(gamut::webp::AbiWebpDecoder::new(backend));
    },
    fns = (gamut_webp_decoder_new, gamut_webp_decoder_free, gamut_webp_decoder_push_backend),
    tests = webp_decoder_tests,
}

seam_handle! {
    handle = GamutWebpEncoder,
    host = gamut::webp::WebpEncoder,
    new = gamut::webp::WebpEncoder::new(),
    seam = encoder,
    push = |host, backend| {
        *host = core::mem::take(host).with_backend(gamut::webp::AbiWebpEncoder::new(backend));
    },
    fns = (gamut_webp_encoder_new, gamut_webp_encoder_free, gamut_webp_encoder_push_backend),
    tests = webp_encoder_tests,
}

/// Creates a WebP decoder with default settings and an empty backend registry.
///
/// Returns `NULL` only if construction panics. Free with `gamut_webp_decoder_free`.
#[unsafe(no_mangle)]
pub extern "C" fn gamut_webp_decoder_new() -> *mut GamutWebpDecoder {
    GamutWebpDecoder::ffi_new()
}

/// Frees a decoder created by `gamut_webp_decoder_new`, running each adopted backend's
/// `destroy` exactly once. No-op on `NULL`.
///
/// # Safety
///
/// `decoder` is `NULL` or a pointer returned by `gamut_webp_decoder_new` that has not already
/// been freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gamut_webp_decoder_free(decoder: *mut GamutWebpDecoder) {
    unsafe { GamutWebpDecoder::ffi_free(decoder) }
}

/// Pushes a WebP codestream-decode backend; backends are tried in push order, before the
/// built-in decoder.
///
/// Returns `GAMUT_OK` when the handle adopts `ctx` (its `destroy` then runs exactly once, at
/// free), `GAMUT_STATUS_NULL_ARGUMENT` on a `NULL` decoder or vtable, or
/// `GAMUT_STATUS_ABI_MISMATCH` when `vtable->abi_version != GAMUT_CODEC_ABI_VERSION`; on any
/// non-OK status the caller keeps ownership of `ctx` and no callback has run.
///
/// # Safety
///
/// `decoder` as in `gamut_webp_decoder_free`; a non-`NULL` `vtable` points to a table that
/// stays valid for the decoder's lifetime. Calling this asserts the `(vtable, ctx)` backend
/// may be used from any thread (the registry requires `Send`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gamut_webp_decoder_push_backend(
    decoder: *mut GamutWebpDecoder,
    vtable: *const DecoderVTable,
    ctx: *mut c_void,
) -> GamutStatus {
    unsafe { GamutWebpDecoder::ffi_push_backend(decoder, vtable, ctx) }
}

/// Creates a WebP encoder with default settings and an empty backend registry.
///
/// Returns `NULL` only if construction panics. Free with `gamut_webp_encoder_free`.
#[unsafe(no_mangle)]
pub extern "C" fn gamut_webp_encoder_new() -> *mut GamutWebpEncoder {
    GamutWebpEncoder::ffi_new()
}

/// Frees an encoder created by `gamut_webp_encoder_new`, running each adopted backend's
/// `destroy` exactly once. No-op on `NULL`.
///
/// # Safety
///
/// `encoder` is `NULL` or a pointer returned by `gamut_webp_encoder_new` that has not already
/// been freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gamut_webp_encoder_free(encoder: *mut GamutWebpEncoder) {
    unsafe { GamutWebpEncoder::ffi_free(encoder) }
}

/// Pushes a WebP codestream-encode backend; backends are tried in push order, before the
/// built-in encoder.
///
/// Status and ownership contract as for `gamut_webp_decoder_push_backend`.
///
/// # Safety
///
/// `encoder` as in `gamut_webp_encoder_free`; a non-`NULL` `vtable` points to a table that
/// stays valid for the encoder's lifetime. Calling this asserts the `(vtable, ctx)` backend
/// may be used from any thread (the registry requires `Send`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gamut_webp_encoder_push_backend(
    encoder: *mut GamutWebpEncoder,
    vtable: *const EncoderVTable,
    ctx: *mut c_void,
) -> GamutStatus {
    unsafe { GamutWebpEncoder::ffi_push_backend(encoder, vtable, ctx) }
}
