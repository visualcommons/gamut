//! integration · law — the host-side fallback contract: `UNSUPPORTED` is the *only* signal that
//! lets a host try the next backend, and everything else is terminal.
//!
//! This is the seam's load-bearing rule, and the reason it needs its own tests is that violating
//! it is silent: a backend that accepts a job, fails, and has its status flattened to
//! `UNSUPPORTED` would be retried, and a partially-produced result masked by whatever the next
//! backend returns. The claims here are the vocabulary (`Status`'s pinned values and predicates),
//! the absent-callback case a C backend produces by leaving a function pointer null, and the push
//! order a host walks.

mod common;

use core::ptr;

use common::{BACKEND_FAILURE, TEST_CODEC, counted_decoder, one_plane};
use gamut_codec_abi::bridge::{ForeignDecoder, ForeignEncoder, Lowered, lower_decoder};
use gamut_codec_abi::{
    ABI_VERSION, Decoder, DecoderVTable, EncodeConfig, Encoder, EncoderVTable, ImageDesc, Status,
    StreamConfig,
};

#[test]
fn ok_and_unsupported_carry_their_pinned_wire_values() {
    // These two are the ABI: a C backend writes the integers directly, so they are exact-byte
    // constants rather than opaque tokens.
    assert_eq!(Status::OK.0, 0);
    assert_eq!(Status::UNSUPPORTED.0, -1);

    assert_eq!(Status(0), Status::OK);
    assert_eq!(Status(-1), Status::UNSUPPORTED);
    assert_ne!(Status(1), Status::OK);
    assert_ne!(Status(1), Status::UNSUPPORTED);
}

#[test]
fn only_unsupported_reads_as_fall_through() {
    assert!(Status::OK.is_ok());
    assert!(!Status::OK.is_unsupported());

    assert!(Status::UNSUPPORTED.is_unsupported());
    assert!(!Status::UNSUPPORTED.is_ok());

    // The case the contract exists for: a terminal backend error is neither success nor the
    // fall-through code, so a host must propagate it rather than try the next backend.
    assert!(!BACKEND_FAILURE.is_ok());
    assert!(!BACKEND_FAILURE.is_unsupported());
}

#[test]
fn a_decoder_vtable_with_no_supports_callback_accepts_nothing() {
    let empty = DecoderVTable {
        abi_version: ABI_VERSION,
        supports: None,
        decode: None,
        destroy: None,
    };
    // SAFETY: `empty` is a valid vtable with no callbacks, so the null ctx is never dereferenced.
    let mut foreign = unsafe { ForeignDecoder::new(&empty, ptr::null_mut()) }.expect("ABI matches");

    assert!(!foreign.supports(&StreamConfig::new(TEST_CODEC, 1, 1, 8)));
}

#[test]
fn an_absent_decode_callback_falls_through_rather_than_failing() {
    let empty = DecoderVTable {
        abi_version: ABI_VERSION,
        supports: None,
        decode: None,
        destroy: None,
    };
    // SAFETY: as above.
    let mut foreign = unsafe { ForeignDecoder::new(&empty, ptr::null_mut()) }.expect("ABI matches");

    let cfg = StreamConfig::new(TEST_CODEC, 1, 1, 8);
    let out = one_plane(1, 1, ptr::null_mut(), 0);

    // A null `decode` means "this backend cannot do it", which is the fall-through code — not a
    // terminal error that would strand the host with no backend tried.
    let status = foreign.decode(&cfg, b"", &out);
    assert_eq!(status, Status::UNSUPPORTED);
    assert!(status.is_unsupported());
}

#[test]
fn an_absent_destroy_callback_makes_drop_a_no_op() {
    let empty = DecoderVTable {
        abi_version: ABI_VERSION,
        supports: None,
        decode: None,
        destroy: None,
    };
    // SAFETY: as above.
    let foreign = unsafe { ForeignDecoder::new(&empty, ptr::null_mut()) }.expect("ABI matches");

    // The claim is that this does not call through a null pointer. It is pinned by the absence of
    // a crash: a regression here aborts the test binary rather than failing an assertion.
    drop(foreign);
}

#[test]
fn an_encoder_vtable_with_no_supports_callback_accepts_nothing() {
    let empty = EncoderVTable {
        abi_version: ABI_VERSION,
        supports: None,
        encode: None,
        destroy: None,
    };
    // SAFETY: `empty` is a valid vtable with no callbacks, so the null ctx is never dereferenced.
    let mut foreign = unsafe { ForeignEncoder::new(&empty, ptr::null_mut()) }.expect("ABI matches");

    assert!(!foreign.supports(&EncodeConfig::new(TEST_CODEC, 50)));
}

#[test]
fn an_absent_encode_callback_falls_through_without_invoking_the_sink() {
    let empty = EncoderVTable {
        abi_version: ABI_VERSION,
        supports: None,
        encode: None,
        destroy: None,
    };
    // SAFETY: as above.
    let mut foreign = unsafe { ForeignEncoder::new(&empty, ptr::null_mut()) }.expect("ABI matches");

    let cfg = EncodeConfig::new(TEST_CODEC, 50);
    let image = one_plane(1, 1, ptr::null_mut(), 0);
    let mut calls = 0usize;
    let status = foreign.encode(&cfg, &image, &mut |_chunk| {
        calls += 1;
        Status::OK
    });

    assert_eq!(status, Status::UNSUPPORTED);
    // Nothing was emitted, so a host is free to hand the same job to the next backend without
    // the caller having already received a partial stream.
    assert_eq!(calls, 0);
}

#[test]
fn a_registry_tries_backends_in_push_order_until_one_supports() {
    // Backend 0 accepts only TEST_CODEC; backend 1 is the "software tail" that accepts anything.
    let (_destroyed, decoder) = counted_decoder();
    let Lowered { vtable: vt, ctx } = lower_decoder(decoder);
    // SAFETY: `vt` outlives `picky` and pairs with `ctx`.
    let mut picky = unsafe { ForeignDecoder::new(&vt, ctx) }.expect("ABI matches");

    struct Tail;
    impl Decoder for Tail {
        fn supports(&mut self, _cfg: &StreamConfig) -> bool {
            true
        }
        fn decode(&mut self, _cfg: &StreamConfig, _bytes: &[u8], _out: &ImageDesc) -> Status {
            Status::OK
        }
    }
    let mut tail = Tail;

    let unsupported_by_first = StreamConfig::new(TEST_CODEC + 1, 1, 1, 8);
    let registry: [&mut dyn Decoder; 2] = [&mut picky, &mut tail];
    let mut chosen = None;
    for (index, backend) in registry.into_iter().enumerate() {
        if backend.supports(&unsupported_by_first) {
            chosen = Some(index);
            break;
        }
    }

    // The first backend fell through; the software tail took the job.
    assert_eq!(chosen, Some(1));
}

#[test]
fn a_registry_stops_at_the_first_backend_that_supports_the_job() {
    // The mirror of the previous test: with a job backend 0 *does* accept, the walk must stop
    // there rather than run on to the tail — otherwise push order would carry no meaning.
    let (_destroyed, decoder) = counted_decoder();
    let Lowered { vtable: vt, ctx } = lower_decoder(decoder);
    // SAFETY: `vt` outlives `picky` and pairs with `ctx`.
    let mut picky = unsafe { ForeignDecoder::new(&vt, ctx) }.expect("ABI matches");

    struct Tail;
    impl Decoder for Tail {
        fn supports(&mut self, _cfg: &StreamConfig) -> bool {
            true
        }
        fn decode(&mut self, _cfg: &StreamConfig, _bytes: &[u8], _out: &ImageDesc) -> Status {
            Status::OK
        }
    }
    let mut tail = Tail;

    let supported_by_first = StreamConfig::new(TEST_CODEC, 1, 1, 8);
    let registry: [&mut dyn Decoder; 2] = [&mut picky, &mut tail];
    let mut chosen = None;
    for (index, backend) in registry.into_iter().enumerate() {
        if backend.supports(&supported_by_first) {
            chosen = Some(index);
            break;
        }
    }

    assert_eq!(chosen, Some(0));
}
