//! Differential cross-check: libtiff still reads a gamut TIFF that carries embedded metadata.
//!
//! The seam adds XMP (700), IPTC/NAA (33723), ICC (34675) and an `ExifIFD` (34665) to IFD 0, and
//! the C2PA manifest store (52545) after the pixel data. Those are out-of-line values, so they
//! move the pixel data's offsets — the risk this file exists for is a directory whose metadata is
//! well-formed but whose strip offsets no longer point where the pixels are, or a private tag
//! that turns a valid TIFF into one an established reader refuses. libtiff, not gamut's own
//! reader, is the judge.

use gamut_core::{Dimensions, EncodeImage, ImageRef, Rgb8};
use gamut_tiff::{Ifd, PhotometricInterpretation, TiffDecoder, TiffEncoder, TiffMetadata, Value};

mod common;

use common::rgb_pattern;

const SIZES: &[(u32, u32)] = &[(1, 1), (17, 13), (64, 100)];

/// A metadata set whose blocks are large enough to be stored out of line (past the 4-byte inline
/// threshold), which is what makes them displace the pixel data.
fn metadata() -> TiffMetadata {
    let mut exif = Ifd::new();
    exif.set(33434, Value::Rational(vec![(1, 250)])); // ExposureTime
    TiffMetadata::new()
        .with_xmp(b"<x:xmpmeta><rdf:RDF/></x:xmpmeta>".to_vec())
        .with_iptc(vec![0x1c, 0x02, 0x05, 0x00, 0x04, b't', b'e', b's', b't'])
        .with_icc(vec![0, 0, 0, 12, b'a', b'c', b's', b'p', 1, 2, 3, 4])
        .with_exif(exif)
}

#[test]
fn libtiff_decodes_a_gamut_rgb_image_carrying_metadata() {
    for &(w, h) in SIZES {
        let src = rgb_pattern(w, h);
        let tiff = TiffEncoder::new()
            .with_metadata(metadata())
            .encode_to_vec(
                ImageRef::<Rgb8>::new(
                    &src,
                    Dimensions {
                        width: w,
                        height: h,
                    },
                )
                .expect("image"),
            )
            .expect("gamut encode");
        let dec = libtiff_oracle::decode_tiff(&tiff).expect("libtiff decode");
        assert_eq!((dec.width, dec.height, dec.samples_per_pixel), (w, h, 3));
        assert_eq!(dec.pixels, src, "RGB mismatch at {w}x{h} with metadata");
    }
}

#[test]
fn libtiff_decodes_a_gamut_image_carrying_a_c2pa_manifest_store() {
    // The store is a private tag (52545) whose value sits *after* the pixel data, at the end of
    // the file (C2PA 2.4 §A.3.6). An established reader must be indifferent to both facts.
    let store = b"\0\0\0\x16jumb\x01\x02\x03\x04\x05\x06".to_vec();
    for &(w, h) in SIZES {
        let src = rgb_pattern(w, h);
        let tiff = TiffEncoder::new()
            .with_metadata(metadata().with_c2pa(store.clone()))
            .encode_to_vec(
                ImageRef::<Rgb8>::new(
                    &src,
                    Dimensions {
                        width: w,
                        height: h,
                    },
                )
                .expect("image"),
            )
            .expect("gamut encode");
        let dec = libtiff_oracle::decode_tiff(&tiff).expect("libtiff decode");
        assert_eq!((dec.width, dec.height, dec.samples_per_pixel), (w, h, 3));
        assert_eq!(dec.pixels, src, "RGB mismatch at {w}x{h} with a C2PA store");
    }
}

/// The 2x2 RGB pixel block the hand-built fixture below carries.
const REPEATED_TAG_PIXELS: [u8; 12] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];

/// A 2x2 8-bit RGB classic TIFF whose IFD 0 carries `PhotometricInterpretation` (262) **twice**:
/// `RGB` (2) first, then `BlackIsZero` (1).
///
/// Built byte by byte because no directory model can express it: `gamut_ifd::Ifd` collapses a
/// repeated tag, which is the very normalisation this fixture exists to look underneath.
fn tiff_repeating_photometric() -> Vec<u8> {
    // 11 entries; the directory occupies 2 + 11*12 + 4 = 138 bytes from offset 8.
    const ENTRIES: u16 = 11;
    let bits_per_sample = 8 + 2 + u32::from(ENTRIES) * 12 + 4;
    let strip = bits_per_sample + 6;

    let mut out = Vec::new();
    out.extend_from_slice(b"II");
    out.extend_from_slice(&42u16.to_le_bytes());
    out.extend_from_slice(&8u32.to_le_bytes());
    out.extend_from_slice(&ENTRIES.to_le_bytes());
    let mut entry = |tag: u16, ty: u16, count: u32, word: u32| {
        out.extend_from_slice(&tag.to_le_bytes());
        out.extend_from_slice(&ty.to_le_bytes());
        out.extend_from_slice(&count.to_le_bytes());
        out.extend_from_slice(&word.to_le_bytes());
    };
    entry(256, 3, 1, 2); // ImageWidth
    entry(257, 3, 1, 2); // ImageLength
    entry(258, 3, 3, bits_per_sample); // BitsPerSample, out of line
    entry(259, 3, 1, 1); // Compression = none
    entry(262, 3, 1, 2); // PhotometricInterpretation = RGB
    entry(262, 3, 1, 1); // PhotometricInterpretation = BlackIsZero -- the repeat
    entry(273, 4, 1, strip); // StripOffsets
    entry(277, 3, 1, 3); // SamplesPerPixel
    entry(278, 3, 1, 2); // RowsPerStrip
    entry(279, 4, 1, REPEATED_TAG_PIXELS.len() as u32); // StripByteCounts
    entry(284, 3, 1, 1); // PlanarConfiguration = chunky
    out.extend_from_slice(&0u32.to_le_bytes()); // no next IFD
    assert_eq!(
        out.len() as u32,
        bits_per_sample,
        "directory layout drifted"
    );
    for _ in 0..3 {
        out.extend_from_slice(&8u16.to_le_bytes());
    }
    assert_eq!(out.len() as u32, strip, "value layout drifted");
    out.extend_from_slice(&REPEATED_TAG_PIXELS);
    out
}

#[test]
fn libtiff_and_this_crate_resolve_a_repeated_tag_to_different_entries() {
    // Why `TiffMetadata::check` refuses a tag carrying both a field and a sub-IFD group, and why
    // `deconstruct` grades a repeated tag: the file is not a directory a reader can resolve
    // without choosing, and the two readers choose opposite ends. libtiff marks every occurrence
    // after the **first** to be ignored (`tif_dirread.c`, "Mark duplicates of any tag to be
    // ignored"); this crate's directory model keeps the **last**. A round trip through gamut
    // cannot see that -- it writes and reads by the same rule -- so the oracle is what makes the
    // disagreement observable at all.
    let bytes = tiff_repeating_photometric();

    let decoded = libtiff_oracle::decode_tiff(&bytes).expect("libtiff decode");
    assert_eq!(
        (decoded.width, decoded.height, decoded.samples_per_pixel),
        (2, 2, 3),
        "libtiff must take the first entry, which says RGB"
    );
    assert_eq!(decoded.pixels, REPEATED_TAG_PIXELS);

    let info = TiffDecoder::new().info(&bytes).expect("gamut info");
    assert_eq!(
        info.photometric,
        PhotometricInterpretation::BlackIsZero,
        "this crate must take the last entry, which says BlackIsZero"
    );
}
