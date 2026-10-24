//! Robustness: the `#![forbid(unsafe_code)]` decoder must reject malformed input cleanly — never
//! panic, never allocate unboundedly — on hostile data (P19).
//!
//! "Never allocate unboundedly" is enforced, not hoped for: on an overcommitting kernel an
//! oversized request can succeed as untouched (or even touched) virtual memory, so the decode
//! still returns and only a malloc-limited caller (libFuzzer, a wasm heap, a cgroup) sees the
//! abort. Issue #949's 16 GiB request passed this suite that way. This binary therefore installs
//! a global allocator that **refuses** any single request above [`CAP`], the way libFuzzer's
//! `-malloc_limit_mb` does, so a regression aborts the test binary on every host. The allocator
//! needs `unsafe`, which `gamut-tiff`'s `#![forbid(unsafe_code)]` keeps out of the lib target —
//! the linkage reason these cases live here rather than inline (`docs/testing.md`), as in
//! `gamut-dng`'s `tests/robustness.rs`.

use std::alloc::{GlobalAlloc, Layout, System};

use gamut_core::{Dimensions, EncodeImage, ImageRef, Rgb8, Rgb16};
use gamut_tiff::{Compression, Ifd, Predictor, TiffDecoder, TiffEncoder, TiffMetadata, Value};

/// The largest single allocation this binary grants: four times the decoder's 64 MiB
/// `MAX_IMAGE_BYTES`, so no decode that limit admits (nor its widening to RGB output) is refused,
/// and far below the gibibyte-scale requests a declared count or geometry produces.
const CAP: usize = 256 << 20;

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

fn valid_lzw_tiff() -> Vec<u8> {
    let dims = Dimensions {
        width: 12,
        height: 9,
    };
    let rgb: Vec<u8> = (0..12 * 9 * 3).map(|i| (i * 7) as u8).collect();
    let mut out = Vec::new();
    TiffEncoder::new()
        .with_compression(Compression::Lzw)
        .encode_image(ImageRef::<Rgb8>::new(&rgb, dims).unwrap(), &mut out)
        .expect("encode");
    out
}

/// A 16-bit LZW+predictor file: the deepest of the new sample paths, since a mutated header can
/// send the byte-order deserialisation and the `u16` predictor over a buffer whose length no longer
/// matches the declared depth.
fn valid_rgb16_tiff() -> Vec<u8> {
    let dims = Dimensions {
        width: 12,
        height: 9,
    };
    let rgb: Vec<u16> = (0..12 * 9 * 3).map(|i| (i * 2711 % 65536) as u16).collect();
    TiffEncoder::new()
        .with_compression(Compression::Lzw)
        .with_predictor(Predictor::HorizontalDifferencing)
        .encode_to_vec(ImageRef::<Rgb16>::new(&rgb, dims).unwrap())
        .expect("encode")
}

/// A file carrying every metadata block, an `ExifIFD` sub-IFD and a C2PA manifest store: the
/// structures `TiffDecoder::metadata` and `c2pa_exclusions` walk, and the only ones in this crate
/// reached by following a pointer tag out of IFD 0 or an offset/count pair out of the last IFD.
fn valid_metadata_tiff() -> Vec<u8> {
    let dims = Dimensions {
        width: 12,
        height: 9,
    };
    let rgb: Vec<u8> = (0..12 * 9 * 3).map(|i| (i * 7) as u8).collect();
    let mut exif = Ifd::new();
    exif.set(33434, Value::Rational(vec![(1, 250)]));
    TiffEncoder::new()
        .with_tiff_metadata(
            TiffMetadata::new()
                .with_exif(exif)
                .with_xmp(b"<x:xmpmeta><rdf:RDF/></x:xmpmeta>".to_vec())
                .with_iptc(vec![0x1c, 0x02, 0x05, 0x00, 0x04, b't', b'e', b's', b't'])
                .with_icc(vec![0, 0, 0, 12, b'a', b'c', b's', b'p', 1, 2, 3, 4])
                .with_c2pa(b"\0\0\0\x16jumb\x01\x02\x03\x04\x05\x06".to_vec()),
        )
        .encode_to_vec(ImageRef::<Rgb8>::new(&rgb, dims).unwrap())
        .expect("encode")
}

#[test]
fn hostile_input_to_the_metadata_entry_points_does_not_panic() {
    // `metadata` follows the `ExifIFD` pointer into a second directory and `c2pa_exclusions`
    // walks the chain to its end to read an offset/count pair — offset-driven reads of untrusted
    // bytes on paths `decode_page` never takes, so the fuzz corpus above cannot reach them.
    let dec = TiffDecoder::new();
    let valid = valid_metadata_tiff();
    for len in 0..=valid.len() {
        let _ = dec.metadata(&valid[..len]);
        let _ = gamut_tiff::c2pa_exclusions(&valid[..len]);
    }
    byte_flip_fuzz(&valid, |data| {
        let _ = dec.metadata(data);
        let _ = gamut_tiff::c2pa_exclusions(data);
    });
}

/// Issue #949's reproducer, verbatim from the `tiff_decode` fuzz run that found it: a 6×4 RGB page
/// whose duplicate `SamplesPerPixel` (a directory keeps a tag's last occurrence) declares
/// 4 294 967 209 samples, with `BitsPerSample` absent.
const ISSUE_949_OOM: &[u8] = b"\
    \x49\x49\x2a\x00\x08\x00\x00\x00\x0c\x00\x00\x01\x03\x00\x01\x00\x00\x00\x06\x00\x00\x00\
    \x01\x01\x03\x00\x01\x00\x00\x00\x04\x00\x00\x00\x42\x01\x02\xff\xfd\x00\x00\x00\x9e\x00\
    \x00\x00\x03\x01\x03\x00\x01\x00\x00\x00\x05\x00\x00\x00\x06\x01\x01\x00\x01\x00\x00\x00\
    \x02\x00\x00\x00\x11\x01\x04\x00\x01\x00\x00\x00\xb4\x00\x00\x00\x15\x01\x03\x00\x01\x00\
    \x00\x00\x03\x00\x00\x00\x16\x01\x0b\x00\x01\x00\x00\x00\x04\x00\x00\x00\x15\x01\x04\x00\
    \x01\x00\x00\x00\xa9\xff\xff\xff\xe5\xfe\xfa\xff\x01\x00\x00\x00\xa4\x00\x00\x00\x16\x01\
    \x1b\x01\x05\x00\x01\x00\x00\x00\xac\x00\x00\x00\x03\x00\x01\x00\x00\x00\x02\x00\x00\x00\
    \x00\x00\x00\x00\x08\x00\x08\x00\x08\x03\x01\x03\x00\x01\x00\x00\x00\x48\x00";

/// #949: the absent `BitsPerSample`'s default depth is not sized by the declared sample count.
/// It used to be materialised as a `SamplesPerPixel`-long list, a 16 GiB request [`CAP`] refuses;
/// the page is now described as declared — one bit per sample — and refused by the decoder.
#[test]
fn absent_bits_per_sample_is_not_sized_by_samples_per_pixel() {
    let dec = TiffDecoder::new();
    let info = dec.info(ISSUE_949_OOM).expect("the page is describable");
    assert_eq!(
        (info.bits_per_sample, info.samples_per_pixel),
        (1, 4_294_967_209)
    );
    assert!(dec.decode_page(ISSUE_949_OOM, 0).is_err());
}

#[test]
fn specific_malformed_inputs_error_without_panic() {
    let dec = TiffDecoder::new();
    let cases: &[&[u8]] = &[
        b"",
        b"II",
        b"II\x2a\x00",
        b"XX\x2a\x00\x08\x00\x00\x00",     // bad byte-order mark
        b"II\x00\x00\x08\x00\x00\x00",     // bad magic
        b"II\x2a\x00\xff\xff\xff\x7f",     // first-IFD offset past EOF
        b"II\x2a\x00\x08\x00\x00\x00\xff", // truncated IFD
        // A 1-entry IFD whose ImageWidth claims a huge value, then truncated.
        b"II\x2a\x00\x08\x00\x00\x00\x01\x00\x00\x01\x04\x00\x01\x00\x00\x00\xff\xff\xff\x7f\x00\x00\x00\x00",
        // #563: a 1x1 page with SamplesPerPixel = 0 and no BitsPerSample — the per-sample depth
        // list defaulted to empty and was then indexed.
        b"II\x2a\x00\x08\x00\x00\x00\x04\x00\x00\x01\x03\x00\x01\x00\x00\x00\x01\x00\x00\x00\x01\x01\x03\x00\x01\x00\x00\x00\x01\x00\x00\x00\x06\x01\x03\x00\x01\x00\x00\x00\x01\x00\x00\x00\x15\x01\x03\x00\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00",
    ];
    for &c in cases {
        // Must return without panicking (Ok or Err either way).
        let _ = dec.info(c);
        let _ = dec.decode_page(c, 0);
    }
}

#[test]
fn truncations_do_not_panic() {
    let dec = TiffDecoder::new();
    for valid in [valid_lzw_tiff(), valid_rgb16_tiff()] {
        for len in 0..=valid.len() {
            let _ = dec.decode_page(&valid[..len], 0);
        }
    }
}

#[test]
fn byte_flip_fuzz_does_not_panic() {
    let dec = TiffDecoder::new();
    for valid in [valid_lzw_tiff(), valid_rgb16_tiff()] {
        byte_flip_fuzz(&valid, |data| {
            let _ = dec.decode_page(data, 0);
        });
    }
}

/// Feeds `consume` 5000 deterministically mutated copies of `valid`; the caller names which entry
/// point is under test.
fn byte_flip_fuzz(valid: &[u8], mut consume: impl FnMut(&[u8])) {
    // Deterministic LCG (no RNG dependency) drives the mutations.
    let mut state: u64 = 0x1234_5678_9abc_def0;
    let mut next = || {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (state >> 33) as u32
    };
    for _ in 0..5000 {
        let mut data = valid.to_vec();
        let flips = 1 + next() % 4;
        for _ in 0..flips {
            let pos = next() as usize % data.len();
            data[pos] ^= (next() & 0xff) as u8;
        }
        consume(&data);
    }
}
