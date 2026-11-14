//! PNG seam handles: [`GamutPngDecoder`] / [`GamutPngEncoder`] and their C entry points.
//!
//! The PNG seam is the zlib/IDAT layer: a pushed decoder backend inflates IDAT zlib streams, a
//! pushed encoder backend deflates them (`ZLIB_CODEC_ID`). Backends are tried in push order;
//! the built-in implementations (`miniz_oxide` inflate / `gamut-deflate`) are the implicit
//! tail.
//!
//! The encoder handle has no config setter: a `gamut_png_encoder_set_*` setter joins the surface
//! together with the PNG encode entry point it would configure (issue #242), so no C call
//! configures what no C call can encode.

use core::ffi::c_void;

use gamut_codec_abi::{DecoderVTable, EncoderVTable};

use crate::seam::seam_handle;
use crate::status::GamutStatus;

/// Opaque handle over a `gamut::png::PngDecoder` and its backend registry.
pub struct GamutPngDecoder {
    inner: gamut::png::PngDecoder,
}

/// Opaque handle over a `gamut::png::PngEncoder` and its backend registry.
pub struct GamutPngEncoder {
    inner: gamut::png::PngEncoder,
}

seam_handle! {
    handle = GamutPngDecoder,
    host = gamut::png::PngDecoder,
    new = gamut::png::PngDecoder::new(),
    seam = decoder,
    push = |host, backend| { host.push_backend(gamut::png::AbiZlibInflater::new(backend)); },
    fns = (gamut_png_decoder_new, gamut_png_decoder_free, gamut_png_decoder_push_backend),
    tests = png_decoder_tests,
}

seam_handle! {
    handle = GamutPngEncoder,
    host = gamut::png::PngEncoder,
    new = gamut::png::PngEncoder::new(),
    seam = encoder,
    push = |host, backend| { host.push_backend(gamut::png::AbiZlibDeflater::new(backend)); },
    fns = (gamut_png_encoder_new, gamut_png_encoder_free, gamut_png_encoder_push_backend),
    tests = png_encoder_tests,
}

/// Creates a PNG decoder with default limits and an empty backend registry.
///
/// Returns `NULL` only if construction panics. Free with `gamut_png_decoder_free`.
#[unsafe(no_mangle)]
pub extern "C" fn gamut_png_decoder_new() -> *mut GamutPngDecoder {
    GamutPngDecoder::ffi_new()
}

/// Frees a decoder created by `gamut_png_decoder_new`, running each adopted backend's
/// `destroy` exactly once. No-op on `NULL`.
///
/// # Safety
///
/// `decoder` is `NULL` or a pointer returned by `gamut_png_decoder_new` that has not already
/// been freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gamut_png_decoder_free(decoder: *mut GamutPngDecoder) {
    unsafe { GamutPngDecoder::ffi_free(decoder) }
}

/// Pushes an IDAT-inflate backend; backends are tried in push order, before the built-in
/// inflate.
///
/// Returns `GAMUT_OK` when the handle adopts `ctx` (its `destroy` then runs exactly once, at
/// free), `GAMUT_STATUS_NULL_ARGUMENT` on a `NULL` decoder or vtable, or
/// `GAMUT_STATUS_ABI_MISMATCH` when `vtable->abi_version != GAMUT_CODEC_ABI_VERSION`; on any
/// non-OK status the caller keeps ownership of `ctx` and no callback has run.
///
/// # Safety
///
/// `decoder` as in `gamut_png_decoder_free`; a non-`NULL` `vtable` points to a table that
/// stays valid for the decoder's lifetime. Calling this asserts the `(vtable, ctx)` backend
/// may be used from any thread (the registry requires `Send`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gamut_png_decoder_push_backend(
    decoder: *mut GamutPngDecoder,
    vtable: *const DecoderVTable,
    ctx: *mut c_void,
) -> GamutStatus {
    unsafe { GamutPngDecoder::ffi_push_backend(decoder, vtable, ctx) }
}

/// Creates a PNG encoder with default settings and an empty backend registry.
///
/// Returns `NULL` only if construction panics. Free with `gamut_png_encoder_free`.
#[unsafe(no_mangle)]
pub extern "C" fn gamut_png_encoder_new() -> *mut GamutPngEncoder {
    GamutPngEncoder::ffi_new()
}

/// Frees an encoder created by `gamut_png_encoder_new`, running each adopted backend's
/// `destroy` exactly once. No-op on `NULL`.
///
/// # Safety
///
/// `encoder` is `NULL` or a pointer returned by `gamut_png_encoder_new` that has not already
/// been freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gamut_png_encoder_free(encoder: *mut GamutPngEncoder) {
    unsafe { GamutPngEncoder::ffi_free(encoder) }
}

/// Pushes an IDAT-deflate backend; backends are tried in push order, before the built-in
/// deflate.
///
/// Status and ownership contract as for `gamut_png_decoder_push_backend`.
///
/// # Safety
///
/// `encoder` as in `gamut_png_encoder_free`; a non-`NULL` `vtable` points to a table that
/// stays valid for the encoder's lifetime. Calling this asserts the `(vtable, ctx)` backend
/// may be used from any thread (the registry requires `Send`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gamut_png_encoder_push_backend(
    encoder: *mut GamutPngEncoder,
    vtable: *const EncoderVTable,
    ctx: *mut c_void,
) -> GamutStatus {
    unsafe { GamutPngEncoder::ffi_push_backend(encoder, vtable, ctx) }
}
