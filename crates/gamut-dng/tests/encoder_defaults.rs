//! What `DngEncoder::new()` writes by default — a lossless-JPEG raw sub-IFD under an uncompressed
//! preview — and the `with_preview` knob that changes or removes that IFD 0 preview, each checked
//! against gamut's decoder and the Adobe SDK.

mod common;

use gamut_dng::{DngDecoder, DngEncoder};

/// The default encoder writes what the DNG SDK writes by default: the raw sub-IFD is lossless
/// JPEG (`Compression = 7`) in one strip, a single SOF3 frame with predictor 1 whose samples are
/// the mosaic, IFD 0 keeps the uncompressed preview, and the backward version stays at 1.1 (no
/// newer reader is needed). gamut's decoder and the Adobe SDK both read the raw back exactly.
#[test]
fn the_default_encoder_writes_a_lossless_jpeg_raw_ifd() {
    use gamut_dng::tags;
    let raw = common::sample_raw(64, 48, 14);
    let mut dng = Vec::new();
    DngEncoder::new()
        .encode(&raw, &common::sample_profile(), &mut dng)
        .expect("encode");

    let file = gamut_ifd::read(&dng).expect("parse");
    let ifd0 = &file.ifds[0];
    assert_eq!(ifd0.get_u32(tags::NEW_SUBFILE_TYPE), Some(1));
    assert_eq!(ifd0.get_u32(tags::COMPRESSION), Some(1), "the preview");
    assert_eq!(
        ifd0.get(tags::DNG_BACKWARD_VERSION),
        Some(&gamut_ifd::Value::Byte(vec![1, 1, 0, 0]))
    );
    let raw_off = ifd0.get_u64(tags::SUB_IFDS).expect("SubIFDs");
    let raw_ifd = gamut_ifd::read_ifd_at(&dng, raw_off, file.order, file.variant).expect("raw IFD");
    assert_eq!(raw_ifd.get_u32(tags::NEW_SUBFILE_TYPE), Some(0));
    assert_eq!(raw_ifd.get_u32(tags::COMPRESSION), Some(7));
    assert_eq!(raw_ifd.get_u32(tags::BITS_PER_SAMPLE), Some(14));
    assert_eq!(raw_ifd.get_u32(tags::ROWS_PER_STRIP), Some(48));
    let offsets = raw_ifd.get_u32_vec(tags::STRIP_OFFSETS).expect("offsets");
    let counts = raw_ifd
        .get_u32_vec(tags::STRIP_BYTE_COUNTS)
        .expect("counts");
    assert_eq!((offsets.len(), counts.len()), (1, 1));
    let strip = &dng[offsets[0] as usize..(offsets[0] + counts[0]) as usize];
    let frame = gamut_jpeg::lossless::decode(strip).expect("one SOF3 frame");
    assert_eq!(frame.precision, 14);
    assert_eq!(frame.samples, raw.samples());

    assert_eq!(DngDecoder::new().decode(&dng).expect("decode").raw, raw);
    gamut_dng_oracle::validate_dng(&dng).expect("Adobe must accept the default DNG");
    let adobe = gamut_dng_oracle::read_raw_dng(&dng).expect("Adobe decode");
    assert_eq!(adobe.samples, raw.samples());
}

/// `DngPreview::None` makes IFD 0 the raw image itself — the one IFD of the file, the main image
/// (`NewSubFileType = 0`) with the DNG tags beside it, and no `SubIFDs` child — which gamut and
/// the Adobe SDK both read back to the input samples, stripped or tiled, CFA or LinearRaw,
/// uncompressed or lossless JPEG.
#[test]
fn no_preview_puts_the_raw_image_in_ifd0() {
    use gamut_dng::{DngCompression, DngPreview, LosslessPredictor};
    for compression in [
        DngCompression::Uncompressed,
        DngCompression::LosslessJpeg(LosslessPredictor::Left),
    ] {
        for tiling in [None, Some((32, 32))] {
            for raw in [
                common::sample_raw(48, 40, 12),
                common::sample_linear_raw(40, 24, 16),
            ] {
                let mut encoder = DngEncoder::new()
                    .with_compression(compression)
                    .with_preview(DngPreview::None);
                if let Some((tw, th)) = tiling {
                    encoder = encoder.with_tiling(tw, th);
                }
                let mut dng = Vec::new();
                encoder
                    .encode(&raw, &common::sample_profile(), &mut dng)
                    .expect("encode");

                let file = gamut_ifd::read(&dng).expect("parse");
                assert_eq!(file.ifds.len(), 1);
                let ifd0 = &file.ifds[0];
                assert_eq!(ifd0.get_u32(gamut_dng::tags::NEW_SUBFILE_TYPE), Some(0));
                assert_eq!(ifd0.get(gamut_dng::tags::SUB_IFDS), None);
                assert_eq!(
                    ifd0.get_u32(gamut_dng::tags::IMAGE_WIDTH),
                    Some(raw.dimensions().width())
                );
                assert_eq!(
                    ifd0.get_u32(gamut_dng::tags::COMPRESSION),
                    Some(u32::from(u16::from(compression.code())))
                );
                assert!(ifd0.get(gamut_dng::tags::DNG_VERSION).is_some());

                let decoded = DngDecoder::new().decode(&dng).expect("gamut decode");
                assert_eq!(decoded.raw, raw, "{compression:?} tiled {tiling:?}");
                gamut_dng_oracle::validate_dng(&dng).unwrap_or_else(|e| {
                    panic!("Adobe must accept a preview-less {compression:?} DNG: {e}")
                });
                let adobe = gamut_dng_oracle::read_raw_dng(&dng).expect("Adobe decode");
                assert_eq!(adobe.samples, raw.samples(), "{compression:?} {tiling:?}");
            }
        }
    }
}

/// A capped preview's longer side is at most the cap — a whole-factor shrink of the uncapped
/// half-resolution preview — while the raw image is untouched: gamut and the Adobe SDK both still
/// read it back exactly.
#[test]
fn a_capped_preview_fits_the_cap_and_the_raw_is_untouched() {
    use gamut_dng::DngPreview;
    // An 80x48 Bayer mosaic previews at 40x24 uncapped; a cap of 16 needs factor ceil(40/16) = 3,
    // giving 13x8.
    let raw = common::sample_raw(80, 48, 16);
    let mut dng = Vec::new();
    DngEncoder::new()
        .with_preview(DngPreview::Uncompressed {
            max_dimension: Some(16),
        })
        .encode(&raw, &common::sample_profile(), &mut dng)
        .expect("encode");
    let file = gamut_ifd::read(&dng).expect("parse");
    let ifd0 = &file.ifds[0];
    assert_eq!(ifd0.get_u32(gamut_dng::tags::NEW_SUBFILE_TYPE), Some(1));
    assert_eq!(
        (
            ifd0.get_u32(gamut_dng::tags::IMAGE_WIDTH),
            ifd0.get_u32(gamut_dng::tags::IMAGE_LENGTH)
        ),
        (Some(13), Some(8))
    );
    assert_eq!(
        ifd0.get_u32_vec(gamut_dng::tags::STRIP_BYTE_COUNTS),
        Some(vec![13 * 8 * 3])
    );
    assert_eq!(DngDecoder::new().decode(&dng).expect("decode").raw, raw);
    gamut_dng_oracle::validate_dng(&dng).expect("Adobe must accept a capped-preview DNG");
    let adobe = gamut_dng_oracle::read_raw_dng(&dng).expect("Adobe decode");
    assert_eq!(adobe.samples, raw.samples());
}

/// A zero preview cap can be met by no preview, so the encoder refuses it as invalid input.
#[test]
fn a_zero_preview_cap_is_refused() {
    use gamut_dng::DngPreview;
    let err = DngEncoder::new()
        .with_preview(DngPreview::Uncompressed {
            max_dimension: Some(0),
        })
        .encode(
            &common::sample_raw(8, 8, 16),
            &common::sample_profile(),
            &mut Vec::new(),
        )
        .expect_err("a zero cap is invalid");
    assert_eq!(
        err.static_message(),
        Some("DNG: a preview size cap must be at least 1 pixel")
    );
}
