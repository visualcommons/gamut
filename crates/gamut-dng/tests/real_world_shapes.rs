//! Shapes that only real camera files exhibit, reproduced synthetically so they are covered by
//! the default suite (issue #174).
//!
//! The real files that motivated each case live in the `gamut-dng-samples` corpus and are checked
//! by `tooling/gamut-dng-real-conformance`, which is deliberately outside the workspace — a
//! 178 MiB submodule has no business on the per-PR path. These tests are the workspace-side proof
//! of the same behaviour, built by encoding a synthetic DNG and then editing it through
//! [`DngRewrite`], the one path that can produce a structurally valid file the encoder would
//! never write.

mod common;

use gamut_core::ErrorKind;
use gamut_dng::{
    DigestCheck, DngDecoder, DngEncoder, DngRewrite, SpanKind, Value, deconstruct, tags,
};

/// Encodes a synthetic DNG, applies `edit` to IFD 0 through the preserving rewrite, and returns
/// the rewritten stream — the way to build a file with a real-world quirk the encoder cannot emit.
fn dng_with_ifd0_edit(edit: impl FnOnce(&mut gamut_ifd::Ifd)) -> Vec<u8> {
    let raw = common::sample_raw(16, 16, 16);
    let mut dng = Vec::new();
    DngEncoder::new()
        .encode(&raw, &common::sample_profile(), &mut dng)
        .expect("encode");
    let mut rewrite = DngRewrite::open(&dng).expect("open");
    edit(&mut rewrite.file_mut().ifds[0]);
    rewrite.write().expect("write").bytes
}

/// A monochrome camera has no colour to calibrate, so it writes no `ColorMatrix1` at all. The raw
/// image must still decode — with no profile, rather than a fabricated one. (Leica M Monochrom.)
#[test]
fn a_file_without_colour_calibration_decodes_with_no_profile() {
    let dng = dng_with_ifd0_edit(|ifd0| {
        ifd0.remove(tags::COLOR_MATRIX1);
        ifd0.remove(tags::CALIBRATION_ILLUMINANT1);
        ifd0.remove(tags::AS_SHOT_NEUTRAL);
    });

    let decoded = DngDecoder::new().decode(&dng).expect("decode");
    assert!(
        decoded.profile.is_none(),
        "no colour calibration must yield no profile, not an invented one"
    );
    assert_eq!(decoded.raw.dimensions().width(), 16);
    assert_eq!(decoded.raw.dimensions().height(), 16);
}

/// Calibration that is *present but malformed* is a broken file, not an absent feature — it must
/// still fail rather than silently degrade to "no profile".
#[test]
fn a_malformed_colour_matrix_is_still_an_error() {
    let dng = dng_with_ifd0_edit(|ifd0| {
        ifd0.set(tags::COLOR_MATRIX1, Value::SRational(vec![(1, 1), (2, 1)]));
    });
    let error = DngDecoder::new()
        .decode(&dng)
        .expect_err("malformed matrix");
    assert_eq!(error.kind(), ErrorKind::InvalidInput);
}

/// `PlanarConfiguration` (284) is chunky on every real DNG and the interleaved sample model
/// assumes it. Planar storage must be refused, never read as chunky — that is silently wrong
/// pixels rather than an error.
#[test]
fn planar_component_storage_is_refused() {
    let raw = common::sample_raw(16, 16, 16);
    let mut dng = Vec::new();
    DngEncoder::new()
        .encode(&raw, &common::sample_profile(), &mut dng)
        .expect("encode");
    let mut rewrite = DngRewrite::open(&dng).expect("open");
    let sub = &mut rewrite.file_mut().ifds[0].sub_ifds_mut()[0];
    sub.ifds[0].set(tags::PLANAR_CONFIGURATION, Value::Short(vec![2]));
    let planar = rewrite.write().expect("write").bytes;

    let error = DngDecoder::new().decode(&planar).expect_err("planar");
    assert_eq!(error.kind(), ErrorKind::Unsupported);
    assert_eq!(
        error.static_message(),
        Some("DNG: planar component storage is not supported")
    );
}

/// An out-of-spec `PlanarConfiguration` is a malformed file rather than an unimplemented feature.
#[test]
fn an_out_of_range_planar_configuration_is_invalid_input() {
    let raw = common::sample_raw(16, 16, 16);
    let mut dng = Vec::new();
    DngEncoder::new()
        .encode(&raw, &common::sample_profile(), &mut dng)
        .expect("encode");
    let mut rewrite = DngRewrite::open(&dng).expect("open");
    let sub = &mut rewrite.file_mut().ifds[0].sub_ifds_mut()[0];
    sub.ifds[0].set(tags::PLANAR_CONFIGURATION, Value::Short(vec![7]));
    let broken = rewrite.write().expect("write").bytes;

    let error = DngDecoder::new().decode(&broken).expect_err("bad value");
    assert_eq!(error.kind(), ErrorKind::InvalidInput);
}

/// Chunky storage is the norm and must decode unremarkably — the guard above must not reject the
/// tag real files actually carry.
#[test]
fn chunky_component_storage_decodes() {
    let raw = common::sample_raw(16, 16, 16);
    let mut dng = Vec::new();
    DngEncoder::new()
        .encode(&raw, &common::sample_profile(), &mut dng)
        .expect("encode");
    let mut rewrite = DngRewrite::open(&dng).expect("open");
    let sub = &mut rewrite.file_mut().ifds[0].sub_ifds_mut()[0];
    sub.ifds[0].set(tags::PLANAR_CONFIGURATION, Value::Short(vec![1]));
    let chunky = rewrite.write().expect("write").bytes;

    let decoded = DngDecoder::new().decode(&chunky).expect("decode");
    assert_eq!(decoded.raw, raw);
}

/// The digest verb picks the storage-correct rule and reports a clean file as a match.
#[test]
fn digest_verification_confirms_an_intact_file() {
    let raw = common::sample_raw(16, 16, 16);
    let mut dng = Vec::new();
    DngEncoder::new()
        .encode(&raw, &common::sample_profile(), &mut dng)
        .expect("encode");
    assert_eq!(
        DngDecoder::new()
            .verify_new_raw_image_digest(&dng)
            .expect("verify"),
        DigestCheck::Match
    );
}

/// A file carrying no digest tag has nothing to verify, which is a verdict of its own rather than
/// an error or a false pass. Most real cameras write no digest at all.
#[test]
fn digest_verification_reports_an_absent_tag() {
    let dng = dng_with_ifd0_edit(|ifd0| {
        ifd0.remove(tags::NEW_RAW_IMAGE_DIGEST);
    });
    assert_eq!(
        DngDecoder::new()
            .verify_new_raw_image_digest(&dng)
            .expect("verify"),
        DigestCheck::Absent
    );
}

/// A digest that does not describe the raw data is reported as a mismatch, carrying both values
/// so a caller can say what it expected.
#[test]
fn digest_verification_detects_a_wrong_digest() {
    let dng = dng_with_ifd0_edit(|ifd0| {
        ifd0.set(tags::NEW_RAW_IMAGE_DIGEST, Value::Byte(vec![0xAB; 16]));
    });
    match DngDecoder::new()
        .verify_new_raw_image_digest(&dng)
        .expect("verify")
    {
        DigestCheck::Mismatch { stored, computed } => {
            assert_eq!(stored, [0xAB; 16]);
            assert_ne!(computed, stored);
        }
        other => panic!("expected a mismatch, got {other:?}"),
    }
}

/// The preserving rewrite must carry bytes the file's structures do not account for, rather than
/// dropping them. A Leica M10 sample loses 651 KB without this.
#[test]
fn the_rewrite_carries_an_unaccounted_trailer_through() {
    let raw = common::sample_raw(16, 16, 16);
    let mut dng = Vec::new();
    DngEncoder::new()
        .encode(&raw, &common::sample_profile(), &mut dng)
        .expect("encode");
    let trailer: Vec<u8> = (0..64u16).map(|i| (i * 7 % 251) as u8).collect();
    dng.extend_from_slice(&trailer);

    let rewrite = DngRewrite::open(&dng).expect("open");
    assert_eq!(rewrite.unaccounted_spans().len(), 1);
    let out = rewrite.write().expect("write");

    assert_eq!(out.preserved.len(), 1, "{:?}", out.preserved);
    let span = out.preserved[0];
    assert_eq!(span.kind, SpanKind::Trailer);
    assert_eq!(span.len, trailer.len() as u64);
    let at = usize::try_from(span.offset).expect("offset");
    assert_eq!(
        &out.bytes[at..at + trailer.len()],
        &trailer[..],
        "the trailer's bytes must survive verbatim"
    );
    // And the rewritten file still accounts for every one of its own bytes.
    let report = deconstruct(&out.bytes).expect("deconstruct");
    assert!(report.is_fully_classified(), "{report:?}");
    // The decoded image is unaffected by the carried bytes.
    assert_eq!(
        DngDecoder::new().decode(&out.bytes).expect("decode").raw,
        raw
    );
}

/// A file whose structures account for all of its bytes carries nothing extra — the preservation
/// path must not invent spans.
#[test]
fn a_clean_file_preserves_nothing_extra() {
    let raw = common::sample_raw(16, 16, 16);
    let mut dng = Vec::new();
    DngEncoder::new()
        .encode(&raw, &common::sample_profile(), &mut dng)
        .expect("encode");
    let out = DngRewrite::open(&dng)
        .expect("open")
        .write()
        .expect("write");
    assert!(out.preserved.is_empty(), "{:?}", out.preserved);
}
