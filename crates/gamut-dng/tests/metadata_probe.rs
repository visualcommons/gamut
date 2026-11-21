//! `DngDecoder::metadata` (issue #1108): the metadata-only read agrees with the full decode,
//! reaches files whose raw image the full decode refuses, and refuses what is not a DNG.

mod common;

use gamut_core::ErrorKind;
use gamut_dng::{
    ByteOrder, DngCompression, DngDecoder, DngEncoder, DngMetadata, Exif, ExifTag, Ifd, TiffFile,
    Value, Variant,
};

/// A little-endian DNG carrying all four metadata carriers, and the metadata it was encoded with.
fn metadata_rich_dng() -> (Vec<u8>, DngMetadata) {
    let mut exif = Exif::new(ByteOrder::LittleEndian);
    exif.set_tag(ExifTag::ExposureTime, Value::Rational(vec![(1, 250)]));
    exif.set_tag(ExifTag::LensModel, Value::Ascii("gamut 50mm".to_owned()));
    let meta = DngMetadata::new()
        .with_exif(exif)
        .with_xmp(br#"<x:xmpmeta xmlns:x="adobe:ns:meta/"></x:xmpmeta>"#.to_vec())
        .with_iptc(vec![0x1c, 0x02, 0x05, 0x00, 0x03, b'a', b'b', b'c'])
        .with_icc(vec![0u8; 16]);
    let mut dng = Vec::new();
    // Uncompressed, so every image directory carries the `Compression = 1` entry the lossy-JPEG
    // test relabels in place.
    DngEncoder::new()
        .with_compression(DngCompression::Uncompressed)
        .with_dng_metadata(meta.clone())
        .encode(
            &common::sample_raw(32, 24, 16),
            &common::sample_profile(),
            &mut dng,
        )
        .expect("encode");
    (dng, meta)
}

/// The metadata-only read returns exactly what the full decode does.
#[test]
fn metadata_probe_equals_the_full_decodes_metadata() {
    let (dng, meta) = metadata_rich_dng();
    let probed = DngDecoder::new().metadata(&dng).expect("metadata");
    assert_eq!(
        probed,
        DngDecoder::new().decode(&dng).expect("decode").metadata
    );
    // Not vacuously: every carrier came through.
    assert_eq!(probed.xmp, meta.xmp);
    assert_eq!(probed.iptc, meta.iptc);
    assert_eq!(probed.icc, meta.icc);
    assert_eq!(
        probed.exif.expect("EXIF model").get_tag(ExifTag::LensModel),
        Some(&Value::Ascii("gamut 50mm".to_owned()))
    );
}

/// A DNG whose raw image is lossy JPEG (34892), which the full decode refuses as unsupported,
/// still yields its metadata.
#[test]
fn metadata_probe_reads_a_lossy_jpeg_dng_the_full_decode_refuses() {
    let (mut dng, meta) = metadata_rich_dng();
    // Relabel every uncompressed directory as lossy JPEG in place: the little-endian IFD entry
    // `Compression` (259), `SHORT`, count 1, value 1 becomes value 34892. The strip bytes stay
    // as they are; neither read below gets as far as them.
    let uncompressed = [0x03, 0x01, 0x03, 0x00, 0x01, 0, 0, 0, 0x01, 0x00, 0, 0];
    let starts: Vec<usize> = dng
        .windows(uncompressed.len())
        .enumerate()
        .filter(|(_, w)| *w == uncompressed)
        .map(|(i, _)| i)
        .collect();
    assert!(!starts.is_empty(), "the encoder writes Compression = 1");
    for start in starts {
        dng[start + 8..start + 10].copy_from_slice(&34892u16.to_le_bytes());
    }

    let err = DngDecoder::new()
        .decode(&dng)
        .expect_err("lossy JPEG is not decodable");
    assert_eq!(err.kind(), ErrorKind::Unsupported, "{err}");
    let probed = DngDecoder::new().metadata(&dng).expect("metadata");
    assert_eq!(probed.xmp, meta.xmp);
    assert_eq!(probed.iptc, meta.iptc);
    assert_eq!(probed.icc, meta.icc);
    assert!(probed.exif.is_some());
}

/// Bytes that are not a TIFF container are refused, not read as empty metadata.
#[test]
fn metadata_probe_rejects_a_malformed_container() {
    let (dng, _) = metadata_rich_dng();
    for bad in [&b"not a tiff"[..], &dng[..6]] {
        let err = DngDecoder::new().metadata(bad).expect_err("malformed");
        assert_eq!(err.kind(), ErrorKind::InvalidInput, "{err}");
    }
}

/// A well-formed TIFF without `DNGVersion` is not a DNG, so the probe refuses it as the full
/// decode does, rather than reporting its blocks.
#[test]
fn metadata_probe_rejects_a_tiff_without_dng_version() {
    let mut ifd0 = Ifd::new();
    ifd0.set(700, Value::Byte(b"<x:xmpmeta/>".to_vec())); // XMP
    let tiff = gamut_ifd::write(&TiffFile::new(
        ByteOrder::LittleEndian,
        Variant::Classic,
        vec![ifd0],
    ))
    .expect("write");
    let err = DngDecoder::new().metadata(&tiff).expect_err("not a DNG");
    assert_eq!(err.kind(), ErrorKind::InvalidInput, "{err}");
    assert!(
        err.static_message()
            .is_some_and(|message| message.contains("DNGVersion")),
        "{err}"
    );
}
