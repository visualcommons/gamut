//! Sub-image decoding: semantic masks, undecodable auxiliary images, and the encoder's own
//! preview, surfaced on `DecodedDng::sub_images`.
//!
//! gamut's encoder does not write mask IFDs, so the semantic-mask fixtures are hand-built over
//! `gamut-ifd`'s tree writer with the same two-pass offset trick the real writer uses: write the
//! tree with placeholder offsets to learn its length, patch the strip offsets, write again
//! (byte-stable), and append the pixel bytes.

mod common;

use gamut_dng::{DngDecoder, DngEncoder, SubImageData, SubImageKind};
use gamut_ifd::{ByteOrder, Ifd, TiffFile, Value, Variant, write};

/// Builds a DNG whose IFD 0 is a 4x4 8-bit CFA raw with three mask sub-IFDs:
/// a valid semantic mask, one with an invalid `MaskSubArea`, and one with an undecodable
/// compression scheme.
fn build_masked_dng() -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let raw_pixels: Vec<u8> = (0..16).collect();
    let mask_pixels: Vec<u8> = vec![0, 255, 128, 64];

    let mut ifd0 = Ifd::new();
    ifd0.set(gamut_dng::tags::NEW_SUBFILE_TYPE, Value::Long(vec![0]));
    ifd0.set(gamut_dng::tags::IMAGE_WIDTH, Value::Short(vec![4]));
    ifd0.set(gamut_dng::tags::IMAGE_LENGTH, Value::Short(vec![4]));
    ifd0.set(gamut_dng::tags::BITS_PER_SAMPLE, Value::Short(vec![8]));
    ifd0.set(gamut_dng::tags::COMPRESSION, Value::Short(vec![1]));
    ifd0.set(
        gamut_dng::tags::PHOTOMETRIC_INTERPRETATION,
        Value::Short(vec![32803]),
    );
    ifd0.set(gamut_dng::tags::SAMPLES_PER_PIXEL, Value::Short(vec![1]));
    ifd0.set(gamut_dng::tags::ROWS_PER_STRIP, Value::Short(vec![4]));
    ifd0.set(
        gamut_dng::tags::CFA_REPEAT_PATTERN_DIM,
        Value::Short(vec![2, 2]),
    );
    ifd0.set(gamut_dng::tags::CFA_PATTERN, Value::Byte(vec![0, 1, 1, 2]));
    ifd0.set(gamut_dng::tags::DNG_VERSION, Value::Byte(vec![1, 6, 0, 0]));
    ifd0.set(
        gamut_dng::tags::UNIQUE_CAMERA_MODEL,
        Value::Ascii("gamut MaskCam".into()),
    );
    ifd0.set(
        gamut_dng::tags::COLOR_MATRIX1,
        Value::SRational(vec![
            (1, 1),
            (0, 1),
            (0, 1),
            (0, 1),
            (1, 1),
            (0, 1),
            (0, 1),
            (0, 1),
            (1, 1),
        ]),
    );
    ifd0.set(
        gamut_dng::tags::CALIBRATION_ILLUMINANT1,
        Value::Short(vec![21]),
    );
    ifd0.set(
        gamut_dng::tags::AS_SHOT_NEUTRAL,
        Value::Rational(vec![(1, 2), (1, 1), (2, 3)]),
    );
    ifd0.set(gamut_dng::tags::STRIP_OFFSETS, Value::Long(vec![0]));
    ifd0.set(gamut_dng::tags::STRIP_BYTE_COUNTS, Value::Long(vec![16]));
    // Unmodelled content the decoder must surface verbatim: a private maker tag and a known but
    // untyped DNG tag.
    ifd0.set(0x9999, Value::Ascii("maker secret".into()));
    ifd0.set(
        gamut_dng::tags::BASELINE_SHARPNESS,
        Value::Rational(vec![(3, 2)]),
    );

    // A 2x2 8-bit mask IFD; per-variant tags are set below.
    let mask_base = {
        let mut m = Ifd::new();
        m.set(
            gamut_dng::tags::NEW_SUBFILE_TYPE,
            Value::Long(vec![0x0001_0004]),
        );
        m.set(gamut_dng::tags::IMAGE_WIDTH, Value::Short(vec![2]));
        m.set(gamut_dng::tags::IMAGE_LENGTH, Value::Short(vec![2]));
        m.set(gamut_dng::tags::BITS_PER_SAMPLE, Value::Short(vec![8]));
        m.set(gamut_dng::tags::COMPRESSION, Value::Short(vec![1]));
        m.set(
            gamut_dng::tags::PHOTOMETRIC_INTERPRETATION,
            Value::Short(vec![52527]),
        );
        m.set(gamut_dng::tags::SAMPLES_PER_PIXEL, Value::Short(vec![1]));
        m.set(gamut_dng::tags::ROWS_PER_STRIP, Value::Short(vec![2]));
        m.set(gamut_dng::tags::STRIP_OFFSETS, Value::Long(vec![0]));
        m.set(gamut_dng::tags::STRIP_BYTE_COUNTS, Value::Long(vec![4]));
        m
    };

    let mut valid = mask_base.clone();
    valid.set(0x9AAA, Value::Short(vec![7, 8])); // private tag on the mask IFD
    valid.set(
        gamut_dng::tags::SEMANTIC_NAME,
        Value::Ascii("Person".into()),
    );
    valid.set(
        gamut_dng::tags::SEMANTIC_INSTANCE_ID,
        Value::Ascii("person_a".into()),
    );
    // 2x2 mask at (1, 1) inside a 4x4 full mask: fits.
    valid.set(
        gamut_dng::tags::MASK_SUB_AREA,
        Value::Long(vec![1, 1, 4, 4]),
    );

    let mut invalid_area = mask_base.clone();
    invalid_area.set(gamut_dng::tags::SEMANTIC_NAME, Value::Ascii("Sky".into()));
    // 2x2 mask at (4, 4) inside a 4x4 full mask: out of bounds — must be ignored.
    invalid_area.set(
        gamut_dng::tags::MASK_SUB_AREA,
        Value::Long(vec![4, 4, 4, 4]),
    );

    let mut undecodable = mask_base;
    // Lossy JPEG (34892) is out of decode scope; the chunk must be carried verbatim.
    undecodable.set(gamut_dng::tags::COMPRESSION, Value::Short(vec![34892]));

    ifd0.set_sub_ifd(
        gamut_dng::tags::SUB_IFDS,
        vec![valid, invalid_area, undecodable],
    );

    // Two-pass layout: learn the tree length, patch real offsets, re-write, append pixels.
    let file = TiffFile {
        order: ByteOrder::LittleEndian,
        variant: Variant::Classic,
        ifds: vec![ifd0],
    };
    let base = write(&file).expect("first pass").len();
    let mut file = file;
    let ifd0 = &mut file.ifds[0];
    ifd0.set(
        gamut_dng::tags::STRIP_OFFSETS,
        Value::Long(vec![base as u32]),
    );
    let mut masks = ifd0.sub_ifds()[0].ifds.clone();
    for (i, mask) in masks.iter_mut().enumerate() {
        mask.set(
            gamut_dng::tags::STRIP_OFFSETS,
            Value::Long(vec![(base + 16 + 4 * i) as u32]),
        );
    }
    ifd0.set_sub_ifd(gamut_dng::tags::SUB_IFDS, masks);

    let mut bytes = write(&file).expect("second pass");
    assert_eq!(bytes.len(), base, "two-pass layout must be byte-stable");
    bytes.extend_from_slice(&raw_pixels);
    for _ in 0..3 {
        bytes.extend_from_slice(&mask_pixels);
    }
    (bytes, raw_pixels, mask_pixels)
}

#[test]
fn semantic_masks_decode_with_typed_info() {
    let (dng, raw_pixels, mask_pixels) = build_masked_dng();
    let decoded = DngDecoder::new().decode(&dng).expect("decode");

    // The main raw decodes from IFD 0 itself.
    let raw_samples: Vec<u16> = raw_pixels.iter().map(|&b| u16::from(b)).collect();
    assert_eq!(decoded.raw.samples(), &raw_samples[..]);

    assert_eq!(decoded.sub_images.len(), 3, "all three masks surfaced");
    let mask_samples: Vec<u16> = mask_pixels.iter().map(|&b| u16::from(b)).collect();

    // Mask 1: fully decoded with the complete semantic info.
    let m1 = &decoded.sub_images[0];
    assert_eq!(m1.kind, SubImageKind::SemanticMask);
    assert_eq!(m1.photometric, 52527);
    assert_eq!(
        m1.interpretation(),
        Some(gamut_dng::PhotometricInterpretation::PhotometricMask)
    );
    assert_eq!(m1.data, SubImageData::Decoded(mask_samples.clone()));
    let s1 = m1.semantic.as_ref().expect("semantic info");
    assert_eq!(s1.name.as_deref(), Some("Person"));
    assert_eq!(s1.instance_id.as_deref(), Some("person_a"));
    let area = s1.sub_area.expect("valid MaskSubArea");
    assert_eq!(
        (area.top, area.left, area.full_width, area.full_height),
        (1, 1, 4, 4)
    );

    // Mask 2: the out-of-bounds MaskSubArea is ignored, the rest survives — and the rejected
    // value still surfaces verbatim in the extras instead of vanishing.
    let m2 = &decoded.sub_images[1];
    let s2 = m2.semantic.as_ref().expect("semantic");
    assert_eq!(s2.name.as_deref(), Some("Sky"));
    assert_eq!(s2.sub_area, None, "invalid MaskSubArea must be ignored");
    assert_eq!(
        m2.extra_tags,
        vec![gamut_dng::RawTag {
            tag: gamut_dng::tags::MASK_SUB_AREA,
            value: gamut_dng::Value::Long(vec![4, 4, 4, 4]),
        }]
    );

    // Mask 3: undecodable compression — the chunk is carried verbatim, not dropped.
    let m3 = &decoded.sub_images[2];
    assert_eq!(
        m3.data,
        SubImageData::Undecoded {
            compression: 34892,
            chunks: vec![mask_pixels],
        }
    );
    assert!(m3.semantic.is_some(), "kind alone attaches (default) info");

    // Unmodelled tags surface verbatim, with their typed values, exactly once.
    assert_eq!(
        decoded.ifd0_extra.iter().map(|t| t.tag).collect::<Vec<_>>(),
        vec![0x9999, gamut_dng::tags::BASELINE_SHARPNESS],
        "IFD 0 extras: the private maker tag and the untyped DNG tag (tag order), nothing else"
    );
    assert_eq!(
        decoded.ifd0_extra[0].value,
        gamut_dng::Value::Ascii("maker secret".into())
    );
    assert_eq!(
        decoded.ifd0_extra[1].value,
        gamut_dng::Value::Rational(vec![(3, 2)])
    );
    // The raw lives in IFD 0 here, so raw_extra is folded into ifd0_extra.
    assert!(decoded.raw_extra.is_empty());
    // The valid mask's private tag lands on that sub-image alone.
    assert_eq!(
        m1.extra_tags,
        vec![gamut_dng::RawTag {
            tag: 0x9AAA,
            value: gamut_dng::Value::Short(vec![7, 8]),
        }]
    );
    // For an undecoded image the layout tags the decode pipeline never consumed stay visible —
    // a consumer of the verbatim chunks needs them.
    assert_eq!(
        decoded.sub_images[2].extra_tags,
        vec![gamut_dng::RawTag {
            tag: gamut_dng::tags::ROWS_PER_STRIP,
            value: Value::Short(vec![2]),
        }]
    );
}

/// Every encoder output carries its RGB preview in IFD 0 — now surfaced as a decoded sub-image.
#[test]
fn encoder_preview_is_surfaced_as_a_decoded_sub_image() {
    let raw = common::sample_raw(32, 24, 12);
    let mut dng = Vec::new();
    DngEncoder::new()
        .encode(&raw, &common::sample_profile(), &mut dng)
        .expect("encode");
    let decoded = DngDecoder::new().decode(&dng).expect("decode");
    assert_eq!(decoded.sub_images.len(), 1);
    let preview = &decoded.sub_images[0];
    assert_eq!(preview.kind, SubImageKind::Preview);
    assert_eq!(preview.photometric, 2, "RGB preview");
    assert_eq!(preview.samples_per_pixel, 3);
    let SubImageData::Decoded(samples) = &preview.data else {
        panic!("uncompressed preview must decode");
    };
    assert_eq!(
        samples.len(),
        preview.dimensions.width as usize * preview.dimensions.height as usize * 3
    );
    assert!(decoded.depth_info.is_none());
}

/// The crate's own encoder output decodes with an empty `sub_ifd_extra`: every `SubIFDs` child
/// it writes carries an image, so none is an image-less directory (issue #814).
#[test]
fn encoder_output_has_no_sub_ifd_extra() {
    let raw = common::sample_raw(32, 24, 12);
    let mut dng = Vec::new();
    DngEncoder::new()
        .encode(&raw, &common::sample_profile(), &mut dng)
        .expect("encode");
    let decoded = DngDecoder::new().decode(&dng).expect("decode");
    assert_eq!(decoded.sub_ifd_extra, []);
}

/// An image-bearing `SubIFDs` child — here a 1x1 RGB preview beside the raw — becomes a
/// sub-image carrying its own unmodelled fields, and so stays out of `sub_ifd_extra`, which
/// covers only image-less directories (issue #814).
#[test]
fn an_image_bearing_sub_ifd_preview_stays_out_of_sub_ifd_extra() {
    use gamut_dng::tags;
    // Both images read their strip from the file's own first bytes, so the fixture needs no
    // offset fix-up after writing.
    let image = |photometric: u16, spp: u16| {
        let mut ifd = Ifd::new();
        ifd.set(tags::IMAGE_WIDTH, Value::Short(vec![1]));
        ifd.set(tags::IMAGE_LENGTH, Value::Short(vec![1]));
        ifd.set(
            tags::BITS_PER_SAMPLE,
            Value::Short(vec![8; usize::from(spp)]),
        );
        ifd.set(tags::SAMPLES_PER_PIXEL, Value::Short(vec![spp]));
        ifd.set(
            tags::PHOTOMETRIC_INTERPRETATION,
            Value::Short(vec![photometric]),
        );
        ifd.set(tags::STRIP_OFFSETS, Value::Long(vec![0]));
        ifd.set(tags::STRIP_BYTE_COUNTS, Value::Long(vec![u32::from(spp)]));
        ifd
    };
    let raw = image(34892, 1);
    let mut preview = image(2, 3);
    preview.set(tags::NEW_SUBFILE_TYPE, Value::Long(vec![1]));
    preview.set(0x9AAA, Value::Short(vec![7]));
    let mut ifd0 = Ifd::new();
    ifd0.set(tags::DNG_VERSION, Value::Byte(vec![1, 7, 1, 0]));
    ifd0.set_sub_ifd(tags::SUB_IFDS, vec![raw, preview]);
    let dng = write(&TiffFile {
        order: ByteOrder::LittleEndian,
        variant: Variant::Classic,
        ifds: vec![ifd0],
    })
    .expect("write");

    let decoded = DngDecoder::new().decode(&dng).expect("decode");
    assert_eq!(decoded.sub_ifd_extra, []);
    let [preview] = &decoded.sub_images[..] else {
        panic!("one sub-image expected: {:?}", decoded.sub_images);
    };
    assert_eq!(preview.kind, SubImageKind::Preview);
    assert_eq!(
        preview.extra_tags,
        [gamut_dng::RawTag {
            tag: 0x9AAA,
            value: Value::Short(vec![7]),
        }]
    );
}
