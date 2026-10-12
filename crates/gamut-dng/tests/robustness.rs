//! Robustness: the raw decoder sizes its buffers from the bytes a file holds, never from the
//! geometry its tags declare (issue #564).
//!
//! `ImageWidth`/`ImageLength` are two tag values. Patched to `60000` each over a tiny encoded raw,
//! they used to make the decoder reserve `60000 × 60000` samples (7.2 GB) before noticing the data
//! was not there; on an overcommitting kernel that reservation is untouched virtual memory, so the
//! decode still returned a clean `Err` and only a malloc-limited caller (libFuzzer, a wasm heap, a
//! cgroup) saw the abort.
//!
//! This binary therefore installs a global allocator that **refuses** any single request above
//! [`CAP`], the way libFuzzer's `-malloc_limit_mb` does: a regression aborts the test binary on
//! every host, instead of passing wherever the kernel happens to overcommit. The allocator needs
//! `unsafe`, which `gamut-dng`'s `#![forbid(unsafe_code)]` keeps out of the lib target — the
//! linkage reason these cases live here rather than inline (`docs/testing.md`).

mod common;

use std::alloc::{GlobalAlloc, Layout, System};

use gamut_core::ErrorKind;
use gamut_dng::{DngDecoder, DngEncoder, DngRewrite, Value, tags};

/// The largest single allocation this binary grants: far above anything decoding an 8×6 raw
/// needs, and far below what the patched geometry would request.
const CAP: usize = 64 << 20;

/// Forwards to [`System`], refusing (null, so the caller aborts) any request past [`CAP`].
struct CappedAllocator;

// SAFETY: every method forwards to `System` with the caller's own arguments, or returns null,
// which `GlobalAlloc` permits as the allocation-failure signal.
unsafe impl GlobalAlloc for CappedAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if layout.size() > CAP {
            return std::ptr::null_mut();
        }
        // SAFETY: the caller's `alloc` contract is passed through unchanged.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if layout.size() > CAP {
            return std::ptr::null_mut();
        }
        // SAFETY: the caller's `alloc_zeroed` contract is passed through unchanged.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` came from this allocator, i.e. from `System`, with this `layout`.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if new_size > CAP {
            return std::ptr::null_mut();
        }
        // SAFETY: `ptr`/`layout` came from `System` via this allocator; the contract is unchanged.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: CappedAllocator = CappedAllocator;

/// Encodes a small 12-bit CFA raw (the issue's 8×6 reproducer) with `encoder`, then applies `edit`
/// to the raw sub-IFD through the preserving rewrite — the image data itself is untouched.
fn patched_raw_ifd(encoder: DngEncoder, edit: impl FnOnce(&mut gamut_ifd::Ifd)) -> Vec<u8> {
    let raw = common::sample_raw(8, 6, 12);
    let mut dng = Vec::new();
    encoder
        .encode(&raw, &common::sample_profile(), &mut dng)
        .expect("encode");
    let mut rewrite = DngRewrite::open(&dng).expect("open");
    edit(&mut rewrite.file_mut().ifds[0].sub_ifds_mut()[0].ifds[0]);
    rewrite.write().expect("write").bytes
}

fn assert_truncated(dng: &[u8]) {
    let error = DngDecoder::new()
        .decode(dng)
        .expect_err("declared geometry exceeds the data");
    assert_eq!(error.kind(), ErrorKind::InvalidInput);
    assert_eq!(
        error.static_message(),
        Some("DNG: raw image data is truncated")
    );
}

/// The issue's reproducer: one strip declared `60000 × 60000` over the original few dozen bytes
/// of samples. Pins the strip path — the output buffer and the per-strip unpack — to the data.
#[test]
fn a_strip_declaring_huge_geometry_is_rejected_without_a_geometry_sized_allocation() {
    let dng = patched_raw_ifd(DngEncoder::new(), |raw| {
        raw.set(tags::IMAGE_WIDTH, Value::Short(vec![60000]));
        raw.set(tags::IMAGE_LENGTH, Value::Short(vec![60000]));
        raw.set(tags::ROWS_PER_STRIP, Value::Short(vec![60000]));
    });
    assert_truncated(&dng);
}

/// The tiled twin: one `60000 × 60000` tile over a 16×16 tile's bytes. Pins the tile path — the
/// assembled image and the per-tile unpack — to the data.
#[test]
fn a_tile_declaring_huge_geometry_is_rejected_without_a_geometry_sized_allocation() {
    let dng = patched_raw_ifd(DngEncoder::new().with_tiling(16, 16), |raw| {
        raw.set(tags::IMAGE_WIDTH, Value::Short(vec![60000]));
        raw.set(tags::IMAGE_LENGTH, Value::Short(vec![60000]));
        raw.set(tags::TILE_WIDTH, Value::Short(vec![60000]));
        raw.set(tags::TILE_LENGTH, Value::Short(vec![60000]));
    });
    assert_truncated(&dng);
}
