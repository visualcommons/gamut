//! Authoritative conformance: every DNG gamut-dng writes must be accepted by the **Adobe DNG SDK**
//! (the `gamut-dng-oracle`, which runs the SDK's parse → build-negative → read-stage-1 flow), and
//! `RawImage::to_linear` must reproduce the SDK's stage-2 (chapter-5) linearization.

mod common;

use gamut_dng::{ByteOrder, DngDecoder, DngEncoder, RawImage, RawLevels};

fn encode(order: ByteOrder, width: u32, height: u32, bits: u16) -> Vec<u8> {
    let raw = common::sample_raw(width, height, bits);
    let profile = common::sample_profile();
    let mut dng = Vec::new();
    DngEncoder::new()
        .with_byte_order(order)
        .encode(&raw, &profile, &mut dng)
        .expect("encode");
    dng
}

#[test]
fn adobe_sdk_validates_le_16bit_cfa() {
    let dng = encode(ByteOrder::LittleEndian, 64, 48, 16);
    gamut_dng_oracle::validate_dng(&dng)
        .expect("Adobe DNG SDK must accept gamut's little-endian DNG");
}

#[test]
fn adobe_sdk_validates_be_16bit_cfa() {
    let dng = encode(ByteOrder::BigEndian, 48, 32, 16);
    gamut_dng_oracle::validate_dng(&dng).expect("Adobe DNG SDK must accept gamut's big-endian DNG");
}

#[test]
fn adobe_sdk_validates_8bit_cfa() {
    let dng = encode(ByteOrder::LittleEndian, 32, 24, 8);
    gamut_dng_oracle::validate_dng(&dng).expect("Adobe DNG SDK must accept gamut's 8-bit DNG");
}

#[test]
fn adobe_sdk_validates_linear_raw() {
    let raw = common::sample_linear_raw(48, 36, 16);
    let mut dng = Vec::new();
    DngEncoder::new()
        .encode(&raw, &common::sample_profile(), &mut dng)
        .expect("encode");
    gamut_dng_oracle::validate_dng(&dng).expect("Adobe DNG SDK must accept gamut's LinearRaw DNG");
}

#[test]
fn adobe_sdk_validates_full_calibration_profile() {
    let raw = common::sample_raw(48, 32, 16);
    let mut dng = Vec::new();
    DngEncoder::new()
        .encode(&raw, &common::sample_profile_full(), &mut dng)
        .expect("encode");
    gamut_dng_oracle::validate_dng(&dng)
        .expect("Adobe DNG SDK must accept a dual-illuminant / forward-matrix profile");
}

/// The Adobe SDK must decode the stage-1 samples back to exactly what we packed — the definitive
/// check that gamut's bit-packing (12/14/16-bit, MSB-first, byte-aligned rows) matches DNG.
#[test]
fn adobe_decodes_packed_cfa_samples_exactly() {
    for bits in [12u16, 14, 16] {
        let raw = common::sample_raw(64, 48, bits);
        let mut dng = Vec::new();
        DngEncoder::new()
            .encode(&raw, &common::sample_profile(), &mut dng)
            .expect("encode");
        let decoded = gamut_dng_oracle::read_raw_dng(&dng).expect("Adobe reads raw");
        assert_eq!((decoded.width, decoded.height, decoded.planes), (64, 48, 1));
        assert_eq!(
            decoded.samples,
            raw.samples(),
            "Adobe stage-1 must match the {bits}-bit input mosaic"
        );
    }
}

#[test]
fn adobe_decodes_linear_raw_samples_exactly() {
    let raw = common::sample_linear_raw(48, 36, 16);
    let mut dng = Vec::new();
    DngEncoder::new()
        .encode(&raw, &common::sample_profile(), &mut dng)
        .expect("encode");
    let decoded = gamut_dng_oracle::read_raw_dng(&dng).expect("Adobe reads raw");
    assert_eq!((decoded.width, decoded.height, decoded.planes), (48, 36, 3));
    assert_eq!(
        decoded.samples,
        raw.samples(),
        "Adobe stage-1 LinearRaw must match input"
    );
}

/// The tiled layout under every compression scheme: the Adobe SDK must validate the container
/// and decode the stage-1 samples back exactly — the definitive check that gamut's tile grid,
/// edge padding, and per-tile packing match DNG (the 80x48 image over 32x32 tiles leaves 16
/// padding columns and rows on the edge tiles).
#[test]
fn adobe_validates_and_decodes_tiled_dngs_exactly() {
    use gamut_dng::Compression;
    for compression in [Compression::None, Compression::Deflate, Compression::Jpeg] {
        // 12-bit exercises per-tile sub-byte packing where the scheme allows it; Deflate is
        // limited to whole-byte depths (the SDK reader's constraint, enforced at encode).
        let cfa_bits = if compression == Compression::Deflate {
            16
        } else {
            12
        };
        for raw in [
            common::sample_raw(80, 48, cfa_bits),
            common::sample_linear_raw(48, 40, 16),
        ] {
            let mut dng = Vec::new();
            DngEncoder::new()
                .with_compression(compression)
                .with_tiling(32, 32)
                .encode(&raw, &common::sample_profile(), &mut dng)
                .expect("encode");
            gamut_dng_oracle::validate_dng(&dng)
                .unwrap_or_else(|e| panic!("Adobe must accept a tiled {compression:?} DNG: {e}"));
            let decoded = gamut_dng_oracle::read_raw_dng(&dng).expect("Adobe reads tiled raw");
            assert_eq!(
                decoded.samples,
                raw.samples(),
                "Adobe stage-1 must match the tiled {compression:?} input"
            );
        }
    }
}

/// `NewRawImageDigest` (tag 51111) must bit-match the SDK's own `FindNewRawImageDigest` — the
/// definitive gate on the digest-tile grid, planar little-endian serialisation, and the
/// byte-mode (≤ 256-entry linearization table) branch. The multi-tile case (300x280 > 256)
/// exercises clipped edge tiles.
#[test]
fn new_raw_image_digest_matches_adobe() {
    let table: Vec<u16> = (0..256u32).map(|v| (v * 257) as u16).collect();
    let cases = [
        ("single-tile CFA", common::sample_raw(64, 48, 16)),
        ("multi-tile CFA", common::sample_raw(300, 280, 16)),
        ("LinearRaw 3-plane", common::sample_linear_raw(48, 36, 16)),
        ("byte-mode (8-bit image)", common::sample_raw(32, 24, 8)),
        (
            "byte-mode (12-bit with a small linearization table)",
            // Deeper than 8 bits, so *only* the <= 256-entry-table rule can select byte mode;
            // sample values stay below 256 so the 8-bit narrowing is lossless on both sides.
            RawImage::new_cfa(
                gamut_dng::Dimensions::new(32, 24).expect("dims"),
                12,
                (2, 2),
                vec![0, 1, 1, 2],
                (0..32u16 * 24).map(|i| i % 251).collect(),
            )
            .expect("raw")
            .with_levels(
                gamut_dng::RawLevels::uniform(1, 0.0, 4095.0)
                    .expect("levels")
                    .with_linearization_table(table),
            )
            .expect("levels"),
        ),
    ];
    for (what, raw) in cases {
        let mut dng = Vec::new();
        DngEncoder::new()
            .encode(&raw, &common::sample_profile(), &mut dng)
            .expect("encode");
        let adobe = gamut_dng_oracle::new_raw_image_digest(&dng).expect("adobe digest");
        assert_eq!(
            raw.new_raw_image_digest(),
            adobe,
            "{what}: digest must bit-match the SDK"
        );
        // The stored tag round-trips through decode, and validate (which runs the SDK's
        // ValidateRawImageDigest over the written tag) accepts the file.
        let decoded = DngDecoder::new().decode(&dng).expect("decode");
        assert_eq!(decoded.new_raw_image_digest, Some(adobe));
        gamut_dng_oracle::validate_dng(&dng).expect("validate with digest");
    }
}

/// The digest gate is genuine: corrupting one byte of the raw data after the digest was written
/// must make the SDK reject the file as damaged (and shift the recomputed digest).
#[test]
fn adobe_rejects_a_corrupted_raw_digest() {
    let raw = common::sample_raw(64, 48, 16);
    let mut dng = Vec::new();
    DngEncoder::new()
        .encode(&raw, &common::sample_profile(), &mut dng)
        .expect("encode");
    let good_digest = gamut_dng_oracle::new_raw_image_digest(&dng).expect("digest");
    // Flip one bit of the last raw byte (the raw strips are laid out after the preview, at the
    // end of the file), leaving the stored digest stale.
    let last = dng.len() - 1;
    dng[last] ^= 0x01;
    let err = gamut_dng_oracle::validate_dng(&dng).expect_err("stale digest must reject");
    assert!(err.contains("(error code 1)"), "{err}");
    // And the SDK's recomputed digest moves with the data.
    assert_ne!(
        gamut_dng_oracle::new_raw_image_digest(&dng).expect("digest"),
        good_digest
    );
}

/// Encodes `raw`, re-decodes it, and requires gamut's `to_linear` to match the Adobe SDK's
/// stage-2 image within ±1 of the 16-bit encoding (`round(linear * 65535)`) per sample.
///
/// The tolerance covers only rounding at `.5` boundaries (both sides compute in doubles); any
/// anchoring, phase, delta, or scale bug shifts values by whole black-level units — orders of
/// magnitude above one 16-bit step — so this is a genuine differential gate on the chapter-5
/// pipeline, exercised through the full encode → decode → linearize path.
fn assert_to_linear_matches_adobe(raw: &RawImage, what: &str) {
    let mut dng = Vec::new();
    DngEncoder::new()
        .encode(raw, &common::sample_profile(), &mut dng)
        .expect("encode");
    let decoded = DngDecoder::new().decode(&dng).expect("gamut decode");
    let gamut = decoded.raw.to_linear().expect("gamut to_linear");
    let adobe = gamut_dng_oracle::read_linear_dng(&dng).expect("Adobe stage-2");
    assert_eq!(
        (adobe.width, adobe.height, adobe.planes),
        (
            gamut.width,
            gamut.height,
            u32::from(gamut.samples_per_pixel)
        ),
        "{what}: stage-2 geometry must agree (active-area crop)"
    );
    assert_eq!(gamut.samples.len(), adobe.samples.len());
    for (i, (&ours, &theirs)) in gamut.samples.iter().zip(&adobe.samples).enumerate() {
        let ours16 = (f64::from(ours) * 65535.0).round() as i32;
        let diff = (ours16 - i32::from(theirs)).abs();
        assert!(
            diff <= 1,
            "{what}: sample {i} diverges — gamut {ours16} vs Adobe {theirs}"
        );
    }
}

#[test]
fn to_linear_matches_adobe_for_uniform_black() {
    let raw = common::sample_raw(32, 24, 12)
        .with_black_level(64.0)
        .expect("black");
    assert_to_linear_matches_adobe(&raw, "uniform black");
}

/// The anchoring gate: a 2x2 fractional black pattern under a non-full active area whose origin
/// has odd row/column parity, so anchoring the pattern at the image origin (instead of the
/// active-area origin) swaps every phase.
#[test]
fn to_linear_matches_adobe_for_black_pattern_with_active_area() {
    let levels =
        RawLevels::new(1, (2, 2), vec![62.25, 63.0, 64.5, 65.75], vec![4095.0]).expect("levels");
    let raw = common::sample_raw(16, 12, 12)
        .with_active_area([3, 5, 11, 13])
        .with_default_crop([0, 0], [8, 8])
        .with_levels(levels)
        .expect("levels");
    assert_to_linear_matches_adobe(&raw, "2x2 pattern + active area");
}

#[test]
fn to_linear_matches_adobe_for_black_deltas() {
    let levels = RawLevels::uniform(1, 64.0, 4095.0)
        .expect("levels")
        .with_black_delta_h((0..16).map(|c| f64::from(c) * 0.25 - 2.0).collect())
        .with_black_delta_v((0..12).map(|r| 1.5 - f64::from(r) * 0.5).collect());
    let raw = common::sample_raw(16, 12, 12)
        .with_levels(levels)
        .expect("levels");
    assert_to_linear_matches_adobe(&raw, "delta H+V");
}

#[test]
fn to_linear_matches_adobe_for_linearization_table() {
    // A square-law table over the full 12-bit input domain.
    let table: Vec<u16> = (0..4096u32)
        .map(|v| ((v * v) >> 8).min(65535) as u16)
        .collect();
    let levels = RawLevels::uniform(1, 100.0, 65535.0)
        .expect("levels")
        .with_linearization_table(table);
    let raw = common::sample_raw(24, 16, 12)
        .with_levels(levels)
        .expect("levels");
    assert_to_linear_matches_adobe(&raw, "linearization table");
}

#[test]
fn to_linear_matches_adobe_for_per_plane_whites() {
    let levels = RawLevels::new(
        3,
        (1, 1),
        vec![16.0, 32.0, 48.0],
        vec![4000.0, 4050.0, 4095.0],
    )
    .expect("levels");
    let raw = common::sample_linear_raw(24, 18, 12)
        .with_levels(levels)
        .expect("levels");
    assert_to_linear_matches_adobe(&raw, "per-plane whites");
}

/// The DNG 1.7.1 §6 conversion, gated against the reference implementation that defines it.
///
/// A file whose white balance is `AsShotWhiteXY` (50729) carries no camera neutral; both gamut-dng
/// and the Adobe SDK have to derive one from the chromaticity through the profile's calibration.
/// This encodes such a file and requires the two derivations to agree.
///
/// The comparison uses profiles with an identity `CameraCalibration` and unit `AnalogBalance`.
/// The spec's formula is `XYZtoCamera = AB · CC · CM`, but `dng_color_spec::SetWhiteXY` folds `CC`
/// and `AB` into its later PCS transforms rather than into the camera white, so the two agree
/// exactly wherever those are the identity — which is every profile this crate writes and every one
/// in Adobe's own sample corpus.
fn assert_neutral_matches_adobe(profile: &gamut_dng::CameraProfile, xy: [f64; 2], what: &str) {
    let profile = profile
        .clone()
        .with_as_shot_white_xy(xy)
        .expect("the chromaticity is convertible through this calibration");
    let raw = common::sample_raw(32, 24, 16);
    let mut dng = Vec::new();
    DngEncoder::new()
        .encode(&raw, &profile, &mut dng)
        .expect("encode");
    gamut_dng_oracle::validate_dng(&dng)
        .unwrap_or_else(|e| panic!("Adobe DNG SDK must accept the {what} AsShotWhiteXY DNG: {e}"));

    let adobe = gamut_dng_oracle::camera_neutral_from_white_xy(&dng)
        .unwrap_or_else(|e| panic!("Adobe DNG SDK must derive a neutral for {what}: {e}"));
    let ours = profile.as_shot_neutral();
    // A neutral of all ones would mean the calibration was never applied, and would match Adobe's
    // for the wrong reason; every fixture here has a genuinely coloured response.
    assert!(
        ours.iter().any(|&c| c < 0.999),
        "{what}: the derivation must apply the calibration, got {ours:?}"
    );
    assert_eq!(adobe.len(), ours.len(), "{what}: colour channel count");
    for (got, want) in ours.iter().zip(adobe.iter()) {
        assert!(
            (got - want).abs() < 1e-4,
            "{what}: gamut derived {ours:?}, Adobe derived {adobe:?}"
        );
    }
}

#[test]
fn as_shot_white_xy_matches_adobe_single_illuminant() {
    // D65 — the calibration illuminant itself, so no interpolation is in play.
    assert_neutral_matches_adobe(
        &common::sample_profile(),
        [0.3127, 0.3290],
        "single-illuminant",
    );
}

#[test]
fn as_shot_white_xy_matches_adobe_between_two_illuminants() {
    // The sample full profile calibrates at D65 and Standard Light A; ~4200 K sits between them,
    // so the SDK and gamut must agree on the *interpolated* colour matrix, not just on one end.
    assert_neutral_matches_adobe(
        &common::sample_profile_full(),
        [0.3730, 0.3750],
        "interpolated",
    );
}

#[test]
fn as_shot_white_xy_matches_adobe_outside_both_illuminants() {
    // Cooler than Standard Light A's 2850 K: the second calibration is taken whole.
    assert_neutral_matches_adobe(
        &common::sample_profile_full(),
        [0.4900, 0.4100],
        "clamped-warm",
    );
    // Hotter than D65's 6500 K: the first calibration is taken whole.
    assert_neutral_matches_adobe(
        &common::sample_profile_full(),
        [0.2830, 0.2930],
        "clamped-cool",
    );
}
