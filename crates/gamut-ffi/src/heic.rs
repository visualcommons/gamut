//! HEIC seam handle: [`GamutHeicDecoder`] and its C entry points.
//!
//! The HEIC seam is the HEVC intra codestream (`HEVC_CODEC_ID`, decode direction). Each pushed
//! backend is wrapped in an `AbiHevcDecoder` and added to the handle's `HeifDecoder`, tried in
//! push order; gamut ships **no** software HEVC tail (issue #18), so an empty registry decodes
//! nothing. The adapter sizes each picture from the codestream's SPS per decode call, so one
//! adapter serves every item a file holds.

use core::ffi::c_void;

use gamut_codec_abi::DecoderVTable;

use crate::seam::seam_handle;
use crate::status::GamutStatus;

/// Opaque handle over a `gamut::heic::HeifDecoder` and its HEVC backend registry (push order).
pub struct GamutHeicDecoder {
    inner: gamut::heic::HeifDecoder,
}

seam_handle! {
    handle = GamutHeicDecoder,
    host = gamut::heic::HeifDecoder,
    new = gamut::heic::HeifDecoder::new(),
    seam = decoder,
    push = |host, backend| {
        *host = core::mem::take(host).with_backend(gamut::heic::AbiHevcDecoder::new(backend));
    },
    fns = (gamut_heic_decoder_new, gamut_heic_decoder_free, gamut_heic_decoder_push_backend),
    tests = heic_decoder_tests,
}

/// Creates a HEIC decoder with an empty backend registry.
///
/// gamut ships no built-in HEVC decoder: until a backend is pushed, the handle can parse
/// containers (issue #242) but decodes nothing. Returns `NULL` only if construction panics.
/// Free with `gamut_heic_decoder_free`.
#[unsafe(no_mangle)]
pub extern "C" fn gamut_heic_decoder_new() -> *mut GamutHeicDecoder {
    GamutHeicDecoder::ffi_new()
}

/// Frees a decoder created by `gamut_heic_decoder_new`, running each adopted backend's
/// `destroy` exactly once. No-op on `NULL`.
///
/// # Safety
///
/// `decoder` is `NULL` or a pointer returned by `gamut_heic_decoder_new` that has not already
/// been freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gamut_heic_decoder_free(decoder: *mut GamutHeicDecoder) {
    unsafe { GamutHeicDecoder::ffi_free(decoder) }
}

/// Pushes an HEVC intra-decode backend; backends are consulted in push order (there is no
/// built-in tail).
///
/// Returns `GAMUT_OK` when the handle adopts `ctx` (its `destroy` then runs exactly once, at
/// free), `GAMUT_STATUS_NULL_ARGUMENT` on a `NULL` decoder or vtable, or
/// `GAMUT_STATUS_ABI_MISMATCH` when `vtable->abi_version != GAMUT_CODEC_ABI_VERSION`; on any
/// non-OK status the caller keeps ownership of `ctx` and no callback has run.
///
/// # Safety
///
/// `decoder` as in `gamut_heic_decoder_free`; a non-`NULL` `vtable` points to a table that
/// stays valid for the decoder's lifetime. Calling this asserts the `(vtable, ctx)` backend
/// may be used from any thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gamut_heic_decoder_push_backend(
    decoder: *mut GamutHeicDecoder,
    vtable: *const DecoderVTable,
    ctx: *mut c_void,
) -> GamutStatus {
    unsafe { GamutHeicDecoder::ffi_push_backend(decoder, vtable, ctx) }
}
