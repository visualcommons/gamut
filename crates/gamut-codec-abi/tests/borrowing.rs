//! integration · law — a `&mut` reference to a backend is itself a backend, so a host can lend an
//! owned one to a per-job adapter without moving it.
//!
//! The blanket `impl Decoder for &mut D` exists for one documented reason: a long-lived backend
//! (typically a [`ForeignDecoder`] holding a C context) must be usable by a generic per-job
//! adapter that takes its backend *by value*, without that move running the owner's
//! destroy-on-drop teardown. So each claim here is checked twice over — that the call forwards to
//! the owner, and that the owner is still alive after the loan ends.
//!
//! `run_one_job` is the shape of the caller this impl serves: generic over `impl Decoder`, taking
//! it by value. Passing `&mut owned` to it is what selects the blanket impl.

mod common;

use core::ptr;
use std::sync::atomic::Ordering;

use common::{TEST_CODEC, counted_decoder, counted_encoder, one_plane};
use gamut_codec_abi::bridge::{
    ForeignDecoder, ForeignEncoder, Lowered, lower_decoder, lower_encoder,
};
use gamut_codec_abi::{Decoder, EncodeConfig, Encoder, ImageDesc, Status, StreamConfig};

/// A per-job adapter: generic over the backend and taking it **by value**, the shape that forces
/// `&mut D` to satisfy `Decoder` in its own right.
///
/// It also models the host contract itself — ask first, and only hand over the job to a backend
/// that accepted it — so a rejected job never reaches `decode`.
fn run_one_job<D: Decoder>(
    mut backend: D,
    cfg: &StreamConfig,
    codestream: &[u8],
    out: &ImageDesc,
) -> (bool, Status) {
    if !backend.supports(cfg) {
        return (false, Status::UNSUPPORTED);
    }
    (true, backend.decode(cfg, codestream, out))
}

/// The encoder counterpart of [`run_one_job`].
fn encode_one_job<E: Encoder>(
    mut backend: E,
    cfg: &EncodeConfig,
    image: &ImageDesc,
    sink: &mut dyn FnMut(&[u8]) -> Status,
) -> (bool, Status) {
    if !backend.supports(cfg) {
        return (false, Status::UNSUPPORTED);
    }
    (true, backend.encode(cfg, image, sink))
}

#[test]
fn a_borrowed_decoder_forwards_supports_and_decode_to_its_owner() {
    let (_destroyed, decoder) = counted_decoder();
    let Lowered { vtable, ctx } = lower_decoder(decoder);
    // SAFETY: `vtable` outlives `owned` and pairs with `ctx`.
    let mut owned = unsafe { ForeignDecoder::new(&vtable, ctx) }.expect("ABI version matches");

    let cfg = StreamConfig::new(TEST_CODEC, 6, 2, 8);
    let mut buf = [0u8; 2];
    let out = one_plane(6, 2, buf.as_mut_ptr(), 6);

    // `&mut owned` is the backend here, not `owned` itself.
    let (supported, status) = run_one_job(&mut owned, &cfg, b"stream", &out);

    assert!(supported);
    assert_eq!(status, Status::OK);
    // The owner really ran the job: these are the bytes its `decode` derives from the arguments.
    assert_eq!(buf, [6, 6]);
}

#[test]
fn a_borrowed_decoder_forwards_a_rejection_rather_than_accepting_for_its_owner() {
    let (_destroyed, decoder) = counted_decoder();
    let Lowered { vtable, ctx } = lower_decoder(decoder);
    // SAFETY: as above.
    let mut owned = unsafe { ForeignDecoder::new(&vtable, ctx) }.expect("ABI version matches");

    // A codec the owner does not claim. Were the forwarding to answer on its own behalf, this
    // would read as supported and the host would hand it a job no backend can do.
    let foreign_codec = StreamConfig::new(TEST_CODEC + 1, 1, 1, 8);
    let out = one_plane(1, 1, ptr::null_mut(), 0);
    let (supported, _status) = run_one_job(&mut owned, &foreign_codec, b"stream", &out);

    assert!(!supported);
}

#[test]
fn lending_a_decoder_does_not_destroy_it() {
    let (destroyed, decoder) = counted_decoder();
    let Lowered { vtable, ctx } = lower_decoder(decoder);
    // SAFETY: as above.
    let mut owned = unsafe { ForeignDecoder::new(&vtable, ctx) }.expect("ABI version matches");

    let cfg = StreamConfig::new(TEST_CODEC, 6, 2, 8);
    let mut buf = [0u8; 2];
    let out = one_plane(6, 2, buf.as_mut_ptr(), 6);

    // Two successive loans. If the by-value parameter had moved the owner, the first would have
    // run its teardown and the second would be a use-after-destroy.
    let _ = run_one_job(&mut owned, &cfg, b"stream", &out);
    assert_eq!(destroyed.load(Ordering::SeqCst), 0);
    let _ = run_one_job(&mut owned, &cfg, b"stream", &out);
    assert_eq!(destroyed.load(Ordering::SeqCst), 0);

    // Teardown happens once, when the owner itself goes out of scope.
    drop(owned);
    assert_eq!(destroyed.load(Ordering::SeqCst), 1);
}

#[test]
fn a_borrowed_encoder_forwards_supports_and_encode_to_its_owner() {
    let (_destroyed, encoder) = counted_encoder();
    let Lowered { vtable, ctx } = lower_encoder(encoder);
    // SAFETY: `vtable` outlives `owned` and pairs with `ctx`.
    let mut owned = unsafe { ForeignEncoder::new(&vtable, ctx) }.expect("ABI version matches");

    let cfg = EncodeConfig::new(TEST_CODEC, 80);
    let image = one_plane(3, 2, ptr::null_mut(), 0);
    let mut written = Vec::new();
    let (supported, status) = encode_one_job(&mut owned, &cfg, &image, &mut |chunk| {
        written.extend_from_slice(chunk);
        Status::OK
    });

    assert!(supported);
    assert_eq!(status, Status::OK);
    assert_eq!(written, vec![3, 2, 80]);
}

#[test]
fn a_borrowed_encoder_forwards_a_rejection_rather_than_accepting_for_its_owner() {
    let (_destroyed, encoder) = counted_encoder();
    let Lowered { vtable, ctx } = lower_encoder(encoder);
    // SAFETY: as above.
    let mut owned = unsafe { ForeignEncoder::new(&vtable, ctx) }.expect("ABI version matches");

    let foreign_codec = EncodeConfig::new(TEST_CODEC + 1, 80);
    let image = one_plane(1, 1, ptr::null_mut(), 0);
    let (supported, _status) =
        encode_one_job(&mut owned, &foreign_codec, &image, &mut |_chunk| Status::OK);

    assert!(!supported);
}

#[test]
fn lending_an_encoder_does_not_destroy_it() {
    let (destroyed, encoder) = counted_encoder();
    let Lowered { vtable, ctx } = lower_encoder(encoder);
    // SAFETY: as above.
    let mut owned = unsafe { ForeignEncoder::new(&vtable, ctx) }.expect("ABI version matches");

    let cfg = EncodeConfig::new(TEST_CODEC, 80);
    let image = one_plane(3, 2, ptr::null_mut(), 0);

    let _ = encode_one_job(&mut owned, &cfg, &image, &mut |_chunk| Status::OK);
    assert_eq!(destroyed.load(Ordering::SeqCst), 0);
    let _ = encode_one_job(&mut owned, &cfg, &image, &mut |_chunk| Status::OK);
    assert_eq!(destroyed.load(Ordering::SeqCst), 0);

    drop(owned);
    assert_eq!(destroyed.load(Ordering::SeqCst), 1);
}
