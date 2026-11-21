//! integration · law — the C ↔ Rust bridge is a faithful conversion: a boxed Rust twin lowered to
//! a `repr(C)` vtable and adapted back behaves as the original, and owns its backend exactly once.
//!
//! This is the one place a round-trip is the right shape, because the claim *is* mutual
//! inversion: `lower_*` and `Foreign*` are each other's inverse, and a defect symmetric across
//! them is not a false negative here — a host that only ever crosses in both directions cannot
//! observe one. Each direction's payload is nonetheless derived from the arguments, so a value
//! that failed to cross shows up as a wrong byte rather than only a wrong status.

mod common;

use core::ptr;
use std::sync::atomic::Ordering;

use common::{
    BACKEND_FAILURE, REPORTED_CICP, SINK_ABORT, TEST_CODEC, counted_decoder, counted_encoder,
    one_plane,
};
use gamut_codec_abi::bridge::{
    ForeignDecoder, ForeignEncoder, Lowered, lower_decoder, lower_encoder,
};
use gamut_codec_abi::{Decoder, EncodeConfig, Encoder, ImageDesc, Status, StreamConfig};

#[test]
fn a_lowered_decoder_delegates_supports_across_the_vtable() {
    let (_destroyed, decoder) = counted_decoder();
    let Lowered { vtable, ctx } = lower_decoder(decoder);
    // SAFETY: `vtable`/`ctx` come from `lower_decoder`, and `vtable` (declared first) outlives
    // `foreign`; the boxed decoder is `Send`.
    let mut foreign = unsafe { ForeignDecoder::new(&vtable, ctx) }.expect("ABI version matches");

    // Both answers must cross, or a backend that accepts nothing would look identical.
    assert!(foreign.supports(&StreamConfig::new(TEST_CODEC, 6, 2, 8)));
    assert!(!foreign.supports(&StreamConfig::new(TEST_CODEC + 1, 6, 2, 8)));
}

#[test]
fn a_lowered_decoder_carries_the_codestream_and_config_across() {
    let (_destroyed, decoder) = counted_decoder();
    let Lowered { vtable, ctx } = lower_decoder(decoder);
    // SAFETY: as above.
    let mut foreign = unsafe { ForeignDecoder::new(&vtable, ctx) }.expect("ABI version matches");

    let mut buf = [0u8; 2];
    let mut out = one_plane(6, 2, buf.as_mut_ptr(), 6);

    let cfg = StreamConfig::new(TEST_CODEC, 6, 2, 8);
    assert_eq!(foreign.decode(&cfg, b"stream", &mut out), Status::OK);
    // The backend writes [codestream.len(), cfg.width], so both arguments crossed intact.
    assert_eq!(buf, [6, 6]);

    // Again with a different width, so the second byte is really read from `cfg` rather than
    // coinciding with the codestream length.
    let wider = StreamConfig::new(TEST_CODEC, 200, 2, 8);
    assert_eq!(foreign.decode(&wider, b"stream", &mut out), Status::OK);
    assert_eq!(buf, [6, 200]);
}

#[test]
fn a_lowered_decoder_reports_the_codestreams_colour_back_through_out() {
    let (_destroyed, decoder) = counted_decoder();
    let Lowered { vtable, ctx } = lower_decoder(decoder);
    // SAFETY: as above.
    let mut foreign = unsafe { ForeignDecoder::new(&vtable, ctx) }.expect("ABI version matches");

    let mut buf = [0u8; 2];
    // The host seeds the unsignalled default; the backend overwrites it with what it decoded.
    let mut out = one_plane(6, 2, buf.as_mut_ptr(), 6);
    let cfg = StreamConfig::new(TEST_CODEC, 6, 2, 8);
    assert_eq!(foreign.decode(&cfg, b"stream", &mut out), Status::OK);

    let reported = (
        out.color_primaries,
        out.transfer_characteristics,
        out.matrix_coefficients,
        out.is_full_range(),
    );
    assert_eq!(reported, REPORTED_CICP);
}

#[test]
fn a_lowered_decoder_propagates_a_terminal_status_verbatim() {
    let (_destroyed, decoder) = counted_decoder();
    let Lowered { vtable, ctx } = lower_decoder(decoder);
    // SAFETY: as above.
    let mut foreign = unsafe { ForeignDecoder::new(&vtable, ctx) }.expect("ABI version matches");

    let cfg = StreamConfig::new(TEST_CODEC, 6, 2, 8);
    let mut buf = [0u8; 2];
    let mut out = one_plane(6, 2, buf.as_mut_ptr(), 6);

    // The backend accepted the job and then failed. That status must arrive unchanged, and in
    // particular must not be flattened to UNSUPPORTED, which would let a host retry and mask a
    // partially-produced result.
    let failed = foreign.decode(&cfg, b"nope", &mut out);
    assert_eq!(failed, BACKEND_FAILURE);
    assert!(!failed.is_unsupported());
}

#[test]
fn a_lowered_decoder_destroys_its_boxed_backend_exactly_once() {
    let (destroyed, decoder) = counted_decoder();
    let Lowered { vtable, ctx } = lower_decoder(decoder);
    // SAFETY: as above.
    let foreign = unsafe { ForeignDecoder::new(&vtable, ctx) }.expect("ABI version matches");

    assert_eq!(destroyed.load(Ordering::SeqCst), 0);
    drop(foreign);
    assert_eq!(destroyed.load(Ordering::SeqCst), 1);
}

#[test]
fn a_lowered_encoder_delegates_supports_across_the_vtable() {
    let (_destroyed, encoder) = counted_encoder();
    let Lowered { vtable, ctx } = lower_encoder(encoder);
    // SAFETY: `vtable` outlives `foreign` and pairs with `ctx`.
    let mut foreign = unsafe { ForeignEncoder::new(&vtable, ctx) }.expect("ABI version matches");

    assert!(foreign.supports(&EncodeConfig::new(TEST_CODEC, 80)));
    assert!(!foreign.supports(&EncodeConfig::new(TEST_CODEC + 1, 80)));
}

#[test]
fn a_lowered_encoder_carries_each_chunk_across_in_order() {
    let (_destroyed, encoder) = counted_encoder();
    let Lowered { vtable, ctx } = lower_encoder(encoder);
    // SAFETY: as above.
    let mut foreign = unsafe { ForeignEncoder::new(&vtable, ctx) }.expect("ABI version matches");

    let cfg = EncodeConfig::new(TEST_CODEC, 80);
    let image = one_plane(3, 2, ptr::null_mut(), 0);
    let mut written = Vec::new();
    let status = foreign.encode(&cfg, &image, &mut |chunk| {
        written.extend_from_slice(chunk);
        Status::OK
    });

    assert_eq!(status, Status::OK);
    // Two separate sink calls, in order: [width, height] from the image, then [quality] from the
    // config. Concatenated, they prove both descriptors crossed and the chunks kept their order.
    assert_eq!(written, vec![3, 2, 80]);
}

#[test]
fn a_lowered_encoder_receives_the_images_colour_signalling() {
    /// Emits the image's colour group, one byte per field.
    struct CicpEcho;
    impl Encoder for CicpEcho {
        fn supports(&mut self, _cfg: &EncodeConfig) -> bool {
            true
        }
        fn encode(
            &mut self,
            _cfg: &EncodeConfig,
            image: &ImageDesc,
            sink: &mut dyn FnMut(&[u8]) -> Status,
        ) -> Status {
            sink(&[
                image.color_primaries as u8,
                image.transfer_characteristics as u8,
                image.matrix_coefficients as u8,
                image.full_range,
            ])
        }
    }
    let Lowered { vtable, ctx } = lower_encoder(Box::new(CicpEcho));
    // SAFETY: as above.
    let mut foreign = unsafe { ForeignEncoder::new(&vtable, ctx) }.expect("ABI version matches");

    // BT.709 primaries, sRGB transfer, BT.601 matrix, full range: four distinct values.
    let image = one_plane(1, 1, ptr::null_mut(), 0).with_cicp(1, 13, 6, true);
    let mut written = Vec::new();
    let status = foreign.encode(&EncodeConfig::new(TEST_CODEC, 50), &image, &mut |chunk| {
        written.extend_from_slice(chunk);
        Status::OK
    });

    assert_eq!(status, Status::OK);
    assert_eq!(written, vec![1, 13, 6, 1]);
}

#[test]
fn a_lowered_encoder_propagates_a_sink_abort_and_stops_emitting() {
    let (_destroyed, encoder) = counted_encoder();
    let Lowered { vtable, ctx } = lower_encoder(encoder);
    // SAFETY: as above.
    let mut foreign = unsafe { ForeignEncoder::new(&vtable, ctx) }.expect("ABI version matches");

    let cfg = EncodeConfig::new(TEST_CODEC, 55);
    let image = one_plane(9, 4, ptr::null_mut(), 0);
    let mut calls = 0usize;
    let status = foreign.encode(&cfg, &image, &mut |_chunk| {
        calls += 1;
        SINK_ABORT
    });

    assert_eq!(status, SINK_ABORT);
    // The sink's refusal crossed back into the backend, which stopped: had it not, the second
    // chunk would have been offered anyway and `calls` would be 2.
    assert_eq!(calls, 1);
}

#[test]
fn a_lowered_encoder_destroys_its_boxed_backend_exactly_once() {
    let (destroyed, encoder) = counted_encoder();
    let Lowered { vtable, ctx } = lower_encoder(encoder);
    // SAFETY: as above.
    let foreign = unsafe { ForeignEncoder::new(&vtable, ctx) }.expect("ABI version matches");

    assert_eq!(destroyed.load(Ordering::SeqCst), 0);
    drop(foreign);
    assert_eq!(destroyed.load(Ordering::SeqCst), 1);
}
