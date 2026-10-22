//! PNG seam handles: [`GamutPngDecoder`] / [`GamutPngEncoder`] and their C entry points.
//!
//! The PNG seam is the zlib/IDAT layer: a pushed decoder backend inflates IDAT zlib streams, a
//! pushed encoder backend deflates them (`CODEC_ID_ZLIB`). Backends are tried in push order;
//! the built-in implementations (`miniz_oxide` inflate / `gamut-deflate`) are the implicit
//! tail.
//!
//! Config setters (`gamut_png_encoder_set_*`) map the Rust `with_*` builders one-to-one. There is
//! no PNG encode entry point yet; a setter configures the handle the consumer-boundary slice will
//! encode through, as DESIGN.md's opaque-handle rule maps it.

use core::ffi::c_void;

use gamut::png::PaletteOrder;
use gamut_codec_abi::{DecoderVTable, EncoderVTable};

use crate::guard;
use crate::seam::seam_handle;
use crate::status::{
    GAMUT_OK, GAMUT_STATUS_INVALID_INPUT, GAMUT_STATUS_NULL_ARGUMENT, GamutStatus,
};

/// `gamut_png_encoder_set_palette_order` value: a caller-supplied palette keeps the order it
/// was given in (the default).
pub const GAMUT_PNG_PALETTE_ORDER_AS_SUPPLIED: u8 = 0;
/// `gamut_png_encoder_set_palette_order` value: a caller-supplied palette is reordered by
/// modified-Zeng, entries that neighbour each other in the picture placed side by side.
pub const GAMUT_PNG_PALETTE_ORDER_MODIFIED_ZENG: u8 = 1;

// The constants are `PaletteOrder`'s own discriminants, so a C value and the Rust variant can
// never be renumbered apart: a change on either side fails this build.
const _: () = {
    assert!(PaletteOrder::AsSupplied as u8 == GAMUT_PNG_PALETTE_ORDER_AS_SUPPLIED);
    assert!(PaletteOrder::ModifiedZeng as u8 == GAMUT_PNG_PALETTE_ORDER_MODIFIED_ZENG);
};

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
    push = |host, backend| { host.push_backend(gamut::png::AbiInflater::new(backend)); },
    fns = (gamut_png_decoder_new, gamut_png_decoder_free, gamut_png_decoder_push_backend),
    tests = png_decoder_tests,
}

seam_handle! {
    handle = GamutPngEncoder,
    host = gamut::png::PngEncoder,
    new = gamut::png::PngEncoder::new(),
    seam = encoder,
    push = |host, backend| { host.push_backend(gamut::png::AbiDeflater::new(backend)); },
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

/// Selects the order a caller-supplied palette is written in, one of the
/// `GAMUT_PNG_PALETTE_ORDER_*` constants (`PngEncoder::with_palette_order`). Lossless in what a
/// decoder renders: reordering renumbers the entries and rewrites the indices and the `bKGD`
/// index to follow. Has no effect on a palette the encoder derives itself.
///
/// Returns `GAMUT_STATUS_INVALID_INPUT` for a value that is not one of the constants (the
/// encoder is then unchanged), `GAMUT_STATUS_NULL_ARGUMENT` on a `NULL` encoder.
///
/// # Safety
///
/// `encoder` is `NULL` or a live encoder handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gamut_png_encoder_set_palette_order(
    encoder: *mut GamutPngEncoder,
    order: u8,
) -> GamutStatus {
    let value = match order {
        GAMUT_PNG_PALETTE_ORDER_AS_SUPPLIED => Some(PaletteOrder::AsSupplied),
        GAMUT_PNG_PALETTE_ORDER_MODIFIED_ZENG => Some(PaletteOrder::ModifiedZeng),
        _ => None,
    };
    guard::status(|| {
        // SAFETY: null is rejected here; a non-null handle is live per contract.
        let Some(this) = (unsafe { encoder.as_mut() }) else {
            return GAMUT_STATUS_NULL_ARGUMENT;
        };
        let Some(value) = value else {
            return GAMUT_STATUS_INVALID_INPUT;
        };
        // The Rust builder consumes and returns the encoder (backends included), so it is moved
        // out and back; the `Default` left behind for that instant is never observed, because no
        // `with_*` builder can panic.
        this.inner = core::mem::take(&mut this.inner).with_palette_order(value);
        GAMUT_OK
    })
}

#[cfg(test)]
mod palette_order_tests {
    use gamut::core::{Dimensions, ImageRef, Indexed8};
    use gamut::png::{PngEncoder, PngPalette};

    use super::*;

    /// What `encoder` writes for a picture whose first-seen palette order differs from its
    /// modified-Zeng order, so the two settings give different files.
    fn encode(encoder: &PngEncoder) -> Vec<u8> {
        let palette = PngPalette::new(&[[0, 0, 0], [1, 1, 1], [2, 2, 2], [3, 3, 3]]).unwrap();
        let indices = [0u8, 2, 0, 2, 0, 2, 3, 2, 3, 1];
        let image = ImageRef::<Indexed8>::new(&indices, Dimensions::new(10, 1).unwrap()).unwrap();
        let mut out = Vec::new();
        encoder.encode_indexed8(image, &palette, &mut out).unwrap();
        out
    }

    /// Each constant configures the handle exactly as the Rust builder it names does, and an
    /// unknown value is refused without touching the handle.
    #[test]
    fn palette_order_setter_maps_each_constant() {
        let handle = gamut_png_encoder_new();
        let zeng = encode(&PngEncoder::new().with_palette_order(PaletteOrder::ModifiedZeng));
        let supplied = encode(&PngEncoder::new());
        assert_ne!(zeng, supplied, "the fixture must tell the two orders apart");
        unsafe {
            assert_eq!(
                gamut_png_encoder_set_palette_order(handle, GAMUT_PNG_PALETTE_ORDER_MODIFIED_ZENG),
                GAMUT_OK
            );
            assert_eq!(encode(&(*handle).inner), zeng);
            assert_eq!(
                gamut_png_encoder_set_palette_order(handle, 2),
                GAMUT_STATUS_INVALID_INPUT
            );
            assert_eq!(
                encode(&(*handle).inner),
                zeng,
                "a refused value changes nothing"
            );
            assert_eq!(
                gamut_png_encoder_set_palette_order(handle, GAMUT_PNG_PALETTE_ORDER_AS_SUPPLIED),
                GAMUT_OK
            );
            assert_eq!(encode(&(*handle).inner), supplied);
            assert_eq!(
                gamut_png_encoder_set_palette_order(core::ptr::null_mut(), 0),
                GAMUT_STATUS_NULL_ARGUMENT
            );
            gamut_png_encoder_free(handle);
        }
    }
}
