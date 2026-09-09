//! Robustness: the `#![forbid(unsafe_code)]` decoder must reject malformed input cleanly — never
//! panic, never allocate unboundedly — on hostile data (P19).

use gamut_core::{Dimensions, EncodeImage, ImageRef, Rgb8, Rgb16};
use gamut_tiff::{Compression, Ifd, Predictor, TiffDecoder, TiffEncoder, TiffMetadata, Value};

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
        .with_metadata(
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
    ];
    for &c in cases {
        // Must return without panicking (Ok or Err either way).
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
