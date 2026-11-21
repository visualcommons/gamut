//! Shared backends and descriptor helpers for the seam's integration tests.
//!
//! The crate under test is pure interface: it codes nothing, so every behavioural claim needs a
//! backend to drive. These two implement the [`Decoder`]/[`Encoder`] twins with just enough
//! behaviour to make each contract observable — a codec id to accept or reject, a payload derived
//! from the arguments so a value that failed to cross is visible, a terminal status distinct from
//! `UNSUPPORTED`, and a destruction counter.

#![allow(dead_code)] // each integration-test binary uses a different subset

use core::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use gamut_codec_abi::{
    Decoder, EncodeConfig, Encoder, ImageDesc, MAX_PLANES, Status, StreamConfig,
};

/// The codec id the test backends claim.
pub const TEST_CODEC: u32 = 0x0C0D_E001;
/// A terminal (accepted-then-failed) backend error, distinct from `UNSUPPORTED`.
pub const BACKEND_FAILURE: Status = Status(7);
/// The status the aborting test sink returns.
pub const SINK_ABORT: Status = Status(9);

/// The colour signalling [`TestDecoder`] reports: BT.2020 primaries, PQ transfer, BT.2020 NCL
/// matrix, full range — every field distinct from [`ImageDesc::UNSIGNALLED`].
pub const REPORTED_CICP: (u16, u16, u16, bool) = (9, 16, 9, true);

/// A decoder that accepts only [`TEST_CODEC`], writes two derived bytes into plane 0, and reports
/// [`REPORTED_CICP`] when `out` carries the colour group.
///
/// The two bytes are the codestream length and `cfg.width`, so a config or codestream that failed
/// to cross the vtable is visible in the output rather than merely producing a wrong status.
pub struct TestDecoder {
    pub destroyed: Arc<AtomicUsize>,
}

impl Decoder for TestDecoder {
    fn supports(&mut self, cfg: &StreamConfig) -> bool {
        cfg.codec_id == TEST_CODEC
    }

    fn decode(&mut self, cfg: &StreamConfig, codestream: &[u8], out: &mut ImageDesc) -> Status {
        // Accepted-then-failed: a terminal status the host must propagate, not fall through on.
        if codestream != b"stream" {
            return BACKEND_FAILURE;
        }
        // SAFETY: the test allocates a >= 2-byte buffer behind `out.planes[0]`.
        unsafe {
            *out.planes[0] = codestream.len() as u8;
            *out.planes[0].add(1) = cfg.width as u8;
        }
        if out.has_cicp() {
            let (primaries, transfer, matrix, full) = REPORTED_CICP;
            *out = out.with_cicp(primaries, transfer, matrix, full);
        }
        Status::OK
    }
}

impl Drop for TestDecoder {
    fn drop(&mut self) {
        self.destroyed.fetch_add(1, Ordering::SeqCst);
    }
}

/// An encoder that emits `[width, height]` then `[quality]`, stopping if the sink says so.
pub struct TestEncoder {
    pub destroyed: Arc<AtomicUsize>,
}

impl Encoder for TestEncoder {
    fn supports(&mut self, cfg: &EncodeConfig) -> bool {
        cfg.codec_id == TEST_CODEC
    }

    fn encode(
        &mut self,
        cfg: &EncodeConfig,
        image: &ImageDesc,
        sink: &mut dyn FnMut(&[u8]) -> Status,
    ) -> Status {
        let status = sink(&[image.width as u8, image.height as u8]);
        if !status.is_ok() {
            return status;
        }
        sink(&[cfg.quality as u8])
    }
}

impl Drop for TestEncoder {
    fn drop(&mut self) {
        self.destroyed.fetch_add(1, Ordering::SeqCst);
    }
}

/// A fresh destruction counter, and the decoder that increments it when dropped.
pub fn counted_decoder() -> (Arc<AtomicUsize>, Box<TestDecoder>) {
    let destroyed = Arc::new(AtomicUsize::new(0));
    let decoder = Box::new(TestDecoder {
        destroyed: Arc::clone(&destroyed),
    });
    (destroyed, decoder)
}

/// A fresh destruction counter, and the encoder that increments it when dropped.
pub fn counted_encoder() -> (Arc<AtomicUsize>, Box<TestEncoder>) {
    let destroyed = Arc::new(AtomicUsize::new(0));
    let encoder = Box::new(TestEncoder {
        destroyed: Arc::clone(&destroyed),
    });
    (destroyed, encoder)
}

/// An [`ImageDesc`] over a single plane at `ptr` with `stride`.
pub fn one_plane(width: u32, height: u32, ptr: *mut u8, stride: usize) -> ImageDesc {
    let mut planes = [ptr::null_mut(); MAX_PLANES];
    planes[0] = ptr;
    let mut strides = [0usize; MAX_PLANES];
    strides[0] = stride;
    ImageDesc::new(0, width, height, 8, 1, planes, strides)
}
