//! Pins the composition the C seam (issue #280 → #242) depends on: one long-lived
//! [`ForeignDecoder`] — pushed once, `destroy` deferred to its drop — lent to an
//! [`AbiHevcDecoder`] via `gamut-codec-abi`'s `&mut D: Decoder` blanket impl. The adapter sizes
//! each picture from its own SPS (issue #1086), so one adapter serves pictures of any size.

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};

use common::{hvcc_bytes, sps_nal};
use gamut_codec_abi::bridge::ForeignDecoder;
use gamut_codec_abi::{ABI_VERSION, DecoderVTable, ImageDesc, Status, StreamConfig};
use gamut_heic::{AbiHevcDecoder, HEVC_CODEC_ID, HevcCodestreamDecoder, HevcConfig};

/// A monochrome 8-bit `hvcC` whose one SPS (NAL unit type 33) states a `width` × `height`
/// picture.
fn hvcc_mono_8bit(width: u32, height: u32) -> HevcConfig {
    HevcConfig::parse(&hvcc_bytes(&[(33, &sps_nal(0, (width, height), None))]))
        .expect("minimal hvcC parses")
}

unsafe extern "C" fn supports_shim(
    _ctx: *mut core::ffi::c_void,
    cfg: *const StreamConfig,
) -> Status {
    // SAFETY: the host passes a valid StreamConfig for the duration of the call.
    if unsafe { (*cfg).codec_id } == HEVC_CODEC_ID {
        Status::OK
    } else {
        Status::UNSUPPORTED
    }
}

/// Writes the stream's width into the first luma sample, proving the backend saw the picture's
/// own dimensions.
unsafe extern "C" fn decode_shim(
    _ctx: *mut core::ffi::c_void,
    cfg: *const StreamConfig,
    _codestream: *const u8,
    _codestream_len: usize,
    out: *const ImageDesc,
) -> Status {
    // SAFETY: the adapter allocated plane 0 as `width * height` u16 samples and both
    // descriptors are valid for the duration of the call.
    unsafe {
        *(*out).planes[0].cast::<u16>() = (*cfg).width as u16;
    }
    Status::OK
}

unsafe extern "C" fn destroy_shim(ctx: *mut core::ffi::c_void) {
    // SAFETY: `ctx` is the test's AtomicUsize destroy counter, which outlives the backend.
    unsafe { &*ctx.cast_const().cast::<AtomicUsize>() }.fetch_add(1, Ordering::SeqCst);
}

const VTABLE: DecoderVTable = DecoderVTable {
    abi_version: ABI_VERSION,
    supports: Some(supports_shim),
    decode: Some(decode_shim),
    destroy: Some(destroy_shim),
};

#[test]
fn one_borrowing_adapter_sees_each_pictures_own_dimensions() {
    let destroyed = AtomicUsize::new(0);
    let ctx = std::ptr::from_ref(&destroyed).cast_mut().cast();

    // SAFETY: VTABLE is valid for the program lifetime and ctx outlives the backend.
    let mut foreign = unsafe { ForeignDecoder::new(&VTABLE, ctx) }.expect("current ABI_VERSION");
    let mut adapter = AbiHevcDecoder::new(&mut foreign);

    // Two pictures of different sizes go through the one adapter. Each must carry its own SPS
    // size through to the backend, or a file whose items differ in size would decode every item
    // at the first one's dimensions.
    for (width, height) in [(4u32, 2u32), (8, 3)] {
        let config = hvcc_mono_8bit(width, height);
        assert!(adapter.supports(&config));
        let frame = adapter.decode_still(&config, &[]).expect("stub decodes");
        assert_eq!((frame.width(), frame.height()), (width, height));
        assert_eq!(frame.y().len(), (width * height) as usize);
        assert_eq!(
            frame.y()[0],
            width as u16,
            "backend saw this picture's width"
        );
    }
}

#[test]
fn the_adapter_borrows_the_backend_rather_than_owning_it() {
    let destroyed = AtomicUsize::new(0);
    let ctx = std::ptr::from_ref(&destroyed).cast_mut().cast();

    // SAFETY: VTABLE is valid for the program lifetime and ctx outlives the backend.
    let mut foreign = unsafe { ForeignDecoder::new(&VTABLE, ctx) }.expect("current ABI_VERSION");

    for (width, height) in [(4u32, 2u32), (8, 3)] {
        let mut adapter = AbiHevcDecoder::new(&mut foreign);
        adapter
            .decode_still(&hvcc_mono_8bit(width, height), &[])
            .expect("stub decodes");
    }

    // An adapter that took ownership would tear the shared backend down when the first was
    // dropped, and the second would be decoding through a destroyed context.
    assert_eq!(
        destroyed.load(Ordering::SeqCst),
        0,
        "no adapter ran destroy"
    );

    drop(foreign);
    assert_eq!(
        destroyed.load(Ordering::SeqCst),
        1,
        "the owner destroys exactly once"
    );
}
