//! Differential tests of rendering intents and black-point compensation
//! (`gamut_cmm::{IccTransform, bpc}`, phase P6/#329) against Little-CMS.
//!
//! Methodology as in `tests/oracle_lut.rs`: every oracle profile is synthesized in memory by
//! `tooling/lcms2-oracle`, serialized once, and **both** sides read the same bytes, so tag
//! quantization is identical and the comparison isolates evaluation semantics. The lcms2
//! side runs end-to-end `cmsCreateTransform`s (`NOOPTIMIZE|NOCACHE`, double formatters).
//!
//! # Expected agreement, per family
//!
//! - **Absolute white scaling** is closed-form on both sides (`diag(wIn/wOut)` from the same
//!   serialized `wtpt` bytes), so matrix-shaper pairs agree to lcms2's f32 evaluation noise
//!   (plus the near-black gamma-inverse amplification the shaper phase documented).
//! - **Black-point detection** has exact paths (gates, the fixed v4 perceptual black) and
//!   *estimator* paths (colorant probe, round-trip ramp) — the estimators run this crate's
//!   f64 pipelines where lcms2 runs f32 transforms over 16-bit CLUT interpolation, so those
//!   agree to a documented tolerance, not bitwise (the issue's research answer: lcms2's
//!   detection is itself an estimate, so assertions are tolerance-based).
//! - **End-to-end BPC** feeds each side's own detected blacks into the same compensation
//!   formula: the tolerance is the detection tolerance amplified through the transform.
//!
//! Measured worst values are recorded at each assert.

use gamut_cmm::{IccTransform, Transform as _, TransformOptions, bpc};
use gamut_icc::{IccProfile, RenderingIntent};
use lcms2_oracle::{
    FLAGS_BLACKPOINTCOMPENSATION, FLAGS_NOCACHE, FLAGS_NOOPTIMIZE, INTENT_ABSOLUTE_COLORIMETRIC,
    INTENT_PERCEPTUAL, INTENT_RELATIVE_COLORIMETRIC, INTENT_SATURATION, Profile, TYPE_CMYK_DBL,
    TYPE_Lab_DBL, TYPE_RGB_DBL, Transform, cie2000_delta_e, cmyk_prtr_v2, cmyk_prtr_v4,
    detect_black_point, detect_destination_black_point, gray, lab4, rgb_matrix_shaper_d65_wtpt,
    rgb_matrix_shaper_v2, set_quiet_log_handler, srgb,
};

/// D65 chromaticity and sRGB primaries, shared by the synthesized shaper profiles.
const D65_XY: [f64; 2] = [0.3127, 0.3290];
const SRGB_PRIMARIES: [[f64; 2]; 3] = [[0.64, 0.33], [0.30, 0.60], [0.15, 0.06]];

/// A deterministic 64-bit LCG (Knuth's MMIX constants) for seeded sweeps.
struct Lcg(u64);

impl Lcg {
    fn next_unit(&mut self) -> f64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        f64::from((self.0 >> 33) as u32) / f64::from(u32::MAX)
    }
}

/// Serializes an oracle-synthesized profile once and hands the same bytes to both sides.
fn reopen(profile: &Profile) -> (IccProfile, Profile) {
    let bytes = profile.to_bytes();
    let parsed = IccProfile::parse(&bytes).expect("gamut-icc parses the lcms2-written profile");
    let oracle = Profile::from_bytes(&bytes).expect("lcms2 reopens its own bytes");
    (parsed, oracle)
}

/// RGB device sweep: corners, gray ramp, seeded random fill, in `[0, 1]`.
fn rgb_sweep(seed: u64) -> Vec<[f64; 3]> {
    let mut points: Vec<[f64; 3]> = Vec::new();
    for corner in 0..8u32 {
        points.push([
            f64::from(corner & 1),
            f64::from((corner >> 1) & 1),
            f64::from((corner >> 2) & 1),
        ]);
    }
    for i in 0..=8 {
        let v = f64::from(i) / 8.0;
        points.push([v; 3]);
    }
    let mut lcg = Lcg(seed);
    while points.len() < 160 {
        points.push([lcg.next_unit(), lcg.next_unit(), lcg.next_unit()]);
    }
    points
}

/// CMYK device sweep in `[0, 1]`: ink corners, gray ramp, seeded random fill.
fn cmyk_sweep(seed: u64) -> Vec<[f64; 4]> {
    let mut points: Vec<[f64; 4]> = Vec::new();
    for corner in 0..16u32 {
        points.push([
            f64::from(corner & 1),
            f64::from((corner >> 1) & 1),
            f64::from((corner >> 2) & 1),
            f64::from((corner >> 3) & 1),
        ]);
    }
    for i in 0..=8 {
        let v = f64::from(i) / 8.0;
        points.push([v; 4]);
    }
    let mut lcg = Lcg(seed);
    while points.len() < 150 {
        points.push([
            lcg.next_unit(),
            lcg.next_unit(),
            lcg.next_unit(),
            lcg.next_unit(),
        ]);
    }
    points
}

fn eval3(transform: &IccTransform, input: &[f64]) -> [f64; 3] {
    let mut out = [0.0; 3];
    transform.transform(input, &mut out).unwrap();
    out
}

/// Clamps lcms2's device output into `[0, 1]` before comparing. lcms2's `TYPE_*_DBL`
/// formatters write the pipeline's raw float result — out-of-gamut PCS values emerge as
/// device samples below 0 or above 1 — while this crate's `ToneCurve` clamps both sides to
/// `[0, 1]` by settled convention (P2; lcms2's own integer formatters saturate identically).
/// Clamping the oracle aligns the two conventions without masking in-range disagreements.
fn clamp01(v: Vec<f64>) -> Vec<f64> {
    v.into_iter().map(|s| s.clamp(0.0, 1.0)).collect()
}

/// Worst per-channel |Δ| and worst ΔE₀₀ (through a shared sRGB→Lab lens) between our
/// RGB→RGB transform and the lcms2 one, over an RGB sweep.
fn rgb_worst(ours: &IccTransform, lcms: &Transform, lens: &Transform, seed: u64) -> (f64, f64) {
    let (mut worst_dev, mut worst_de) = (0.0_f64, 0.0_f64);
    for point in rgb_sweep(seed) {
        let got = eval3(ours, &point);
        let want = clamp01(lcms.apply_f64(&point, 1, 3));
        for ch in 0..3 {
            worst_dev = worst_dev.max((got[ch] - want[ch]).abs());
        }
        let lab_got = lens.apply_f64(&got, 1, 3);
        let lab_want = lens.apply_f64(&want, 1, 3);
        let de = cie2000_delta_e(
            [lab_got[0], lab_got[1], lab_got[2]],
            [lab_want[0], lab_want[1], lab_want[2]],
            1.0,
            1.0,
            1.0,
        );
        worst_de = worst_de.max(de);
    }
    (worst_dev, worst_de)
}

// ---------------------------------------------------------------------------------------------
// ICC-absolute colorimetric
// ---------------------------------------------------------------------------------------------

#[test]
fn absolute_on_a_non_d50_white_matches_lcms2() {
    set_quiet_log_handler();
    // THE acceptance case: a v4 shaper whose wtpt really is D65 (the synthesizer overwrites
    // the D50 that cmsCreateRGBProfile would store) against built-in sRGB (wtpt = D50).
    let (src, src_oracle) = reopen(&rgb_matrix_shaper_d65_wtpt(
        D65_XY,
        SRGB_PRIMARIES,
        [2.2, 2.2, 2.2],
    ));
    let (dst, dst_oracle) = reopen(&srgb());
    let ours = IccTransform::between(
        &src,
        &dst,
        TransformOptions {
            intent: RenderingIntent::IccAbsoluteColorimetric,
            black_point_compensation: false,
        },
    )
    .unwrap();
    let lcms = Transform::new(
        &src_oracle,
        TYPE_RGB_DBL,
        &dst_oracle,
        TYPE_RGB_DBL,
        INTENT_ABSOLUTE_COLORIMETRIC,
        FLAGS_NOOPTIMIZE | FLAGS_NOCACHE,
    );
    let lens = Transform::new(
        &dst_oracle,
        TYPE_RGB_DBL,
        &lab4(),
        TYPE_Lab_DBL,
        INTENT_RELATIVE_COLORIMETRIC,
        FLAGS_NOCACHE,
    );
    let (worst_dev, worst_de) = rgb_worst(&ours, &lcms, &lens, 61);
    // Closed-form scaling from the same wtpt bytes on both sides — the residual is lcms2's
    // float evaluation noise only. Measured: worst device |Δ| 1.6e-7, worst ΔE₀₀ 1.7e-5.
    assert!(
        worst_dev < 5e-6,
        "absolute: worst device |Δ| = {worst_dev:e}"
    );
    assert!(worst_de < 5e-4, "absolute: worst ΔE₀₀ = {worst_de:e}");

    // And absolute must differ measurably from relative on this pair: the D65 white no
    // longer collapses onto the destination white.
    let relative = IccTransform::between(
        &src,
        &dst,
        TransformOptions {
            intent: RenderingIntent::MediaRelativeColorimetric,
            black_point_compensation: false,
        },
    )
    .unwrap();
    let abs_white = eval3(&ours, &[1.0; 3]);
    let rel_white = eval3(&relative, &[1.0; 3]);
    let gap: f64 = (0..3).map(|ch| (abs_white[ch] - rel_white[ch]).abs()).sum();
    // Measured gap 8.0e-2 (the blue channel pulled down by the D65→D50 Z ratio).
    assert!(
        gap > 0.05,
        "absolute ≡ relative on a non-D50 white? gap = {gap}"
    );
}

#[test]
fn absolute_between_d50_wtpt_profiles_is_exactly_relative() {
    set_quiet_log_handler();
    // Both profiles store wtpt = D50 (the cmsCreateRGBProfile default): the scaling is the
    // exact identity, the empty layer is skipped, and absolute equals relative bit for bit
    // — matching lcms2's own behaviour on the same pair.
    let (src, _) = reopen(&rgb_matrix_shaper_v2(
        true,
        D65_XY,
        SRGB_PRIMARIES,
        [2.2; 3],
    ));
    let (dst, _) = reopen(&srgb());
    let absolute = IccTransform::between(
        &src,
        &dst,
        TransformOptions {
            intent: RenderingIntent::IccAbsoluteColorimetric,
            black_point_compensation: false,
        },
    )
    .unwrap();
    let relative = IccTransform::between(
        &src,
        &dst,
        TransformOptions {
            intent: RenderingIntent::MediaRelativeColorimetric,
            black_point_compensation: false,
        },
    )
    .unwrap();
    for point in rgb_sweep(67).into_iter().take(60) {
        assert_eq!(eval3(&absolute, &point), eval3(&relative, &point));
    }
}

// ---------------------------------------------------------------------------------------------
// Black-point detection differentials
// ---------------------------------------------------------------------------------------------

/// Compares both detectors against the oracle for one profile/intent, asserting worst
/// per-component |Δ| under `tol`. The oracle wrapper returns `None` where lcms2 reports no
/// black point — which must coincide with our zero convention.
fn assert_detection(
    parsed: &IccProfile,
    oracle: &Profile,
    our_intent: RenderingIntent,
    lcms_intent: u32,
    tol: f64,
    label: &str,
) {
    let cases = [
        (
            bpc::detect_black_point(parsed, our_intent),
            detect_black_point(oracle, lcms_intent),
            "source",
        ),
        (
            bpc::detect_destination_black_point(parsed, our_intent),
            detect_destination_black_point(oracle, lcms_intent),
            "destination",
        ),
    ];
    for (got, want, side) in cases {
        match want {
            None => {
                assert_eq!(
                    got, [0.0; 3],
                    "{label} {side}: oracle FALSE, ours must be zero"
                );
            }
            Some(want) => {
                for ch in 0..3 {
                    let delta = (got[ch] - want[ch]).abs();
                    assert!(
                        delta < tol,
                        "{label} {side} ch {ch}: {got:?} vs {want:?} (|Δ| = {delta:e})"
                    );
                }
            }
        }
    }
}

#[test]
fn detection_on_v4_matrix_shaper_takes_the_colorant_probe() {
    set_quiet_log_handler();
    // sRGB: v4 matrix shaper. Perceptual/saturation route to the darker-colorant probe at
    // relative (not the fixed black); relative probes directly. sRGB's black is 0: ours
    // exactly, lcms2's to its float noise. Measured worst |Δ| 4.0e-9.
    let (parsed, oracle) = reopen(&srgb());
    for (ours, lcms) in [
        (RenderingIntent::Perceptual, INTENT_PERCEPTUAL),
        (
            RenderingIntent::MediaRelativeColorimetric,
            INTENT_RELATIVE_COLORIMETRIC,
        ),
        (RenderingIntent::Saturation, INTENT_SATURATION),
    ] {
        assert_detection(&parsed, &oracle, ours, lcms, 1e-7, "srgb");
    }
    // Sanity against the fixed-black constant: the shaper path must NOT return it.
    assert_ne!(
        bpc::detect_black_point(&parsed, RenderingIntent::Perceptual),
        [0.00336, 0.0034731, 0.00287]
    );
}

#[test]
fn detection_on_gray_shaper_matches() {
    set_quiet_log_handler();
    let (parsed, oracle) = reopen(&gray(D65_XY, 2.2));
    // Gray gamma shaper: black = 0 (to lcms2's float noise). Measured worst |Δ| 4.0e-9.
    assert_detection(
        &parsed,
        &oracle,
        RenderingIntent::MediaRelativeColorimetric,
        INTENT_RELATIVE_COLORIMETRIC,
        1e-7,
        "gray",
    );
}

#[test]
fn detection_on_v4_clut_profile_is_the_fixed_perceptual_black() {
    set_quiet_log_handler();
    let (parsed, oracle) = reopen(&cmyk_prtr_v4(9));
    // v4 CLUT profile, perceptual/saturation: the fixed cmsPERCEPTUAL_BLACK on both sides —
    // exact (the constants are literals). Measured worst |Δ| 0.0 (bitwise).
    for (ours, lcms) in [
        (RenderingIntent::Perceptual, INTENT_PERCEPTUAL),
        (RenderingIntent::Saturation, INTENT_SATURATION),
    ] {
        assert_detection(&parsed, &oracle, ours, lcms, 1e-12, "cmyk v4 fixed");
    }
    assert_eq!(
        bpc::detect_black_point(&parsed, RenderingIntent::Perceptual),
        [0.00336, 0.0034731, 0.00287]
    );
}

#[test]
fn detection_on_v4_clut_profile_at_relative_runs_the_estimators() {
    set_quiet_log_handler();
    let (parsed, oracle) = reopen(&cmyk_prtr_v4(9));
    // Relative: source = the ink round-trip probe (including the hidden forced-BPC layer
    // lcms2's CreateRoundtripXForm carries for v4 profiles — see `bpc`'s RoundTrip docs),
    // destination = the ramp estimator. Both run each side's own transforms (ours f64,
    // lcms2 float): tolerance-based. Measured worst |Δ| 1.9e-6 (source), 2.5e-7
    // (destination); without the hidden layer the source would miss by 7.2e-3.
    assert_detection(
        &parsed,
        &oracle,
        RenderingIntent::MediaRelativeColorimetric,
        INTENT_RELATIVE_COLORIMETRIC,
        5e-5,
        "cmyk v4 relative",
    );
}

#[test]
fn detection_on_v2_clut_profile_runs_the_estimators_at_every_intent() {
    set_quiet_log_handler();
    let (parsed, oracle) = reopen(&cmyk_prtr_v2(9));
    // v2 skips the fixed-black branch entirely: perceptual/saturation take the darker-
    // colorant probe (source) and the perceptual-region ramp estimator (destination);
    // relative takes the ink round trip (source) and the relative-region ramp estimator.
    // Measured worst |Δ| across intents/sides: 8.1e-7 — small enough that a fit-region or
    // ramp-index mutation (which moves the fitted root by whole L* tenths) is caught.
    for (ours, lcms) in [
        (RenderingIntent::Perceptual, INTENT_PERCEPTUAL),
        (
            RenderingIntent::MediaRelativeColorimetric,
            INTENT_RELATIVE_COLORIMETRIC,
        ),
        (RenderingIntent::Saturation, INTENT_SATURATION),
    ] {
        assert_detection(&parsed, &oracle, ours, lcms, 5e-5, "cmyk v2");
    }
}

#[test]
fn detection_refusals_match_the_oracle() {
    set_quiet_log_handler();
    // Abstract class (the Lab identity): no black point, both sides.
    let (parsed, oracle) = reopen(&lab4());
    assert!(detect_black_point(&oracle, INTENT_PERCEPTUAL).is_none());
    assert_eq!(
        bpc::detect_black_point(&parsed, RenderingIntent::Perceptual),
        [0.0; 3]
    );
    // Absolute intent: no black point, both sides, on an otherwise detectable profile.
    let (parsed, oracle) = reopen(&srgb());
    assert!(detect_black_point(&oracle, INTENT_ABSOLUTE_COLORIMETRIC).is_none());
    assert_eq!(
        bpc::detect_black_point(&parsed, RenderingIntent::IccAbsoluteColorimetric),
        [0.0; 3]
    );
    assert_eq!(
        bpc::detect_destination_black_point(&parsed, RenderingIntent::IccAbsoluteColorimetric),
        [0.0; 3]
    );
}

// ---------------------------------------------------------------------------------------------
// End-to-end black-point compensation
// ---------------------------------------------------------------------------------------------

/// Worst per-channel |Δ| between our CMYK→RGB transform and the lcms2 one (oracle output
/// clamped — see [`clamp01`]).
fn cmyk_to_rgb_worst(ours: &IccTransform, lcms: &Transform, seed: u64) -> f64 {
    let mut worst = 0.0_f64;
    for point in cmyk_sweep(seed) {
        let got = eval3(ours, &point);
        let ink: Vec<f64> = point.iter().map(|&v| v * 100.0).collect();
        let want = clamp01(lcms.apply_f64(&ink, 1, 3));
        for ch in 0..3 {
            worst = worst.max((got[ch] - want[ch]).abs());
        }
    }
    worst
}

#[test]
fn bpc_on_a_v2_pair_matches_lcms2_and_differs_from_no_bpc() {
    set_quiet_log_handler();
    // All-v2 pair so lcms2's v4 forcing cannot confound the flag comparison: the CMYK v2
    // printer (whose detected blacks are non-zero) into a v2 RGB shaper (black = 0).
    let (src, src_oracle) = reopen(&cmyk_prtr_v2(9));
    let (dst, dst_oracle) = reopen(&rgb_matrix_shaper_v2(
        true,
        D65_XY,
        SRGB_PRIMARIES,
        [2.2; 3],
    ));
    let build = |bpc_on: bool| {
        IccTransform::between(
            &src,
            &dst,
            TransformOptions {
                intent: RenderingIntent::MediaRelativeColorimetric,
                black_point_compensation: bpc_on,
            },
        )
        .unwrap()
    };
    let lcms = |flags: u32| {
        Transform::new(
            &src_oracle,
            TYPE_CMYK_DBL,
            &dst_oracle,
            TYPE_RGB_DBL,
            INTENT_RELATIVE_COLORIMETRIC,
            FLAGS_NOOPTIMIZE | FLAGS_NOCACHE | flags,
        )
    };
    let ours_on = build(true);
    let ours_off = build(false);
    // BPC off vs lcms2 without the flag: the plain LUT-pair route. Measured worst 4.0e-5.
    let worst_off = cmyk_to_rgb_worst(&ours_off, &lcms(0), 71);
    assert!(worst_off < 5e-4, "BPC off: worst |Δ| = {worst_off:e}");
    // BPC on vs lcms2 with FLAGS_BLACKPOINTCOMPENSATION: each side compensates with its own
    // detected blacks (the detection differentials' tolerance family, propagated through
    // the transform). Measured worst 4.8e-5.
    let worst_on = cmyk_to_rgb_worst(&ours_on, &lcms(FLAGS_BLACKPOINTCOMPENSATION), 73);
    assert!(worst_on < 5e-4, "BPC on: worst |Δ| = {worst_on:e}");
    // And the flag is observable: the two builds differ measurably where blacks differ.
    let mut gap = 0.0_f64;
    for point in cmyk_sweep(79).into_iter().take(60) {
        let a = eval3(&ours_on, &point);
        let b = eval3(&ours_off, &point);
        gap = gap.max((0..3).map(|ch| (a[ch] - b[ch]).abs()).fold(0.0, f64::max));
    }
    // Measured gap 2.1e-1 (deep shadows lifted by the compensation).
    assert!(gap > 1e-2, "BPC must be observable: gap = {gap:e}");
}

#[test]
fn v4_perceptual_default_options_match_lcms2s_forced_bpc() {
    set_quiet_log_handler();
    // The end-to-end differential PR7 could not write: a v4 pair at perceptual with DEFAULT
    // options on both sides. lcms2 silently forces BPC (v4 destination); our `between`
    // replicates the forcing, so the outputs must track — and must ALSO track lcms2 with
    // the explicit BPC flag (proving the flag is already in effect).
    let (src, src_oracle) = reopen(&cmyk_prtr_v4(9));
    let (dst, dst_oracle) = reopen(&srgb());
    let ours = IccTransform::between(&src, &dst, TransformOptions::default()).unwrap();
    let lcms_default = Transform::new(
        &src_oracle,
        TYPE_CMYK_DBL,
        &dst_oracle,
        TYPE_RGB_DBL,
        INTENT_PERCEPTUAL,
        FLAGS_NOOPTIMIZE | FLAGS_NOCACHE,
    );
    let lcms_explicit = Transform::new(
        &src_oracle,
        TYPE_CMYK_DBL,
        &dst_oracle,
        TYPE_RGB_DBL,
        INTENT_PERCEPTUAL,
        FLAGS_NOOPTIMIZE | FLAGS_NOCACHE | FLAGS_BLACKPOINTCOMPENSATION,
    );
    let worst_default = cmyk_to_rgb_worst(&ours, &lcms_default, 83);
    let worst_explicit = cmyk_to_rgb_worst(&ours, &lcms_explicit, 83);
    // Measured worst 3.2e-5 — identical for both oracle transforms, which is lcms2's
    // forcing made visible on its own side (the explicit flag changes nothing).
    assert!(
        worst_default < 5e-4,
        "default options vs lcms2 default flags: worst |Δ| = {worst_default:e}"
    );
    assert!(
        worst_explicit < 5e-4,
        "default options vs lcms2 explicit BPC: worst |Δ| = {worst_explicit:e}"
    );

    // Control, isolating the forcing itself: downgrade only the destination header to v2
    // (a matrix shaper carries no LUT tags, so at perceptual the version gates nothing but
    // the forcing) — the unforced build must diverge visibly from the forced one, proving
    // the default-options agreement above really rides on the replicated forcing.
    let mut dst_v2 = dst.clone();
    dst_v2.header.version.major = 2;
    let unforced = IccTransform::between(&src, &dst_v2, TransformOptions::default()).unwrap();
    let mut gap = 0.0_f64;
    for point in cmyk_sweep(89).into_iter().take(40) {
        let a = eval3(&ours, &point);
        let b = eval3(&unforced, &point);
        gap = gap.max((0..3).map(|ch| (a[ch] - b[ch]).abs()).fold(0.0, f64::max));
    }
    // Measured gap 4.3e-2 (the fixed perceptual black of the v4 source pulled to the
    // shaper's zero black only when forcing applies).
    assert!(gap > 1e-2, "forced BPC must be observable: {gap:e}");
}

// ---------------------------------------------------------------------------------------------
// Pathological-profile zoo: destination-estimator differentials
// ---------------------------------------------------------------------------------------------
//
// Hand-built profiles whose 256-sample return curves give **direct control of the round-trip
// ramp** (`inRamp[l]` lands exactly on sample `l` on both sides), steering the estimator into
// every control-flow corner: the straightness band (with and without the protected window),
// the top-down monotonization, the non-ascending rejection, and the exactly-3-points fit
// quirk. Each profile is serialized once by gamut-icc and read back by BOTH sides, and the
// assertion is a plain detection differential — so any behavioural drift in those corners is
// caught against the oracle without hand-derived expectations.

/// A 256-sample u16 curve from a closure over `x ∈ [0, 1]`.
fn sampled(f: impl Fn(f64) -> f64) -> gamut_icc::Curve {
    let samples: Vec<u16> = (0..256u32)
        .map(|i| {
            let y = f(f64::from(i) / 255.0).clamp(0.0, 1.0);
            #[expect(clippy::cast_possible_truncation, reason = "y is clamped to [0, 1]")]
            #[expect(clippy::cast_sign_loss, reason = "y is clamped to [0, 1]")]
            {
                (y * 65535.0 + 0.5) as u16
            }
        })
        .collect();
    gamut_icc::Curve::Sampled(samples)
}

/// A gray↔Lab output-class v2 profile: `B2A0/1` pass `L` through to the single device
/// channel; `A2B0/1` map the device channel through `curve` into `L*` with constant chroma
/// `0x9000` (a* = b* ≈ 15.44) — so `outRamp[l] = curve(l/255) · 100` exactly, on both sides.
fn gray_ramp_profile(curve: &gamut_icc::Curve) -> IccProfile {
    use gamut_icc::{
        Clut, ClutPrecision, ColorSpace, CurveOrParametric, DeviceClass, LutAToB, LutBToA,
        ProfileHeader, Signature, TagData,
    };
    let b2a = || {
        // 3→1: CLUT corners store the L coordinate.
        let mut samples = Vec::new();
        for l in 0..2u16 {
            for _a in 0..2u16 {
                for _b in 0..2u16 {
                    samples.push(l * 65535);
                }
            }
        }
        TagData::LutBToA(LutBToA {
            input_channels: 3,
            output_channels: 1,
            b_curves: vec![CurveOrParametric::Curve(gamut_icc::Curve::Identity); 3],
            matrix: None,
            m_curves: None,
            clut: Some(Clut {
                grid_points: vec![2; 3],
                output_channels: 1,
                precision: ClutPrecision::U16,
                samples,
            }),
            a_curves: None,
        })
    };
    let a2b = || {
        TagData::LutAToB(LutAToB {
            input_channels: 1,
            output_channels: 3,
            a_curves: Some(vec![CurveOrParametric::Curve(curve.clone())]),
            clut: Some(Clut {
                grid_points: vec![2],
                output_channels: 3,
                precision: ClutPrecision::U16,
                samples: vec![0, 0x9000, 0x9000, 65535, 0x9000, 0x9000],
            }),
            m_curves: None,
            matrix: None,
            b_curves: vec![CurveOrParametric::Curve(gamut_icc::Curve::Identity); 3],
        })
    };
    let mut header = ProfileHeader::new(DeviceClass::Output, ColorSpace::Gray);
    header.pcs = ColorSpace::Lab;
    header.version.major = 2;
    IccProfile {
        header,
        tags: vec![
            (Signature(*b"A2B0"), a2b()),
            (Signature(*b"A2B1"), a2b()),
            (Signature(*b"B2A0"), b2a()),
            (Signature(*b"B2A1"), b2a()),
        ],
    }
}

/// A CMYK↔Lab output-class v2 profile whose `A2B` legs mix `0.5·L + 0.25·a + 0.25·b`
/// (encoded) through `curve` into `L*` (chroma pinned to 0) — the return ramp *shape*
/// depends on the ramp's chroma inputs, so the ±50 chroma clamps are observable. `B2A` legs
/// pass `(L, a, b, 0)` through.
fn cmyk_mix_profile(curve: &gamut_icc::Curve) -> IccProfile {
    use gamut_icc::{
        Clut, ClutPrecision, ColorSpace, CurveOrParametric, DeviceClass, LutAToB, LutBToA,
        ProfileHeader, Signature, TagData,
    };
    let b2a = || {
        let mut samples = Vec::new();
        for l in 0..2u16 {
            for a in 0..2u16 {
                for b in 0..2u16 {
                    samples.extend([l * 65535, a * 65535, b * 65535, 0]);
                }
            }
        }
        TagData::LutBToA(LutBToA {
            input_channels: 3,
            output_channels: 4,
            b_curves: vec![CurveOrParametric::Curve(gamut_icc::Curve::Identity); 3],
            matrix: None,
            m_curves: None,
            clut: Some(Clut {
                grid_points: vec![2; 3],
                output_channels: 4,
                precision: ClutPrecision::U16,
                samples,
            }),
            a_curves: None,
        })
    };
    let a2b = || {
        // CLUT ch0 = 0.5·c0 + 0.25·c1 + 0.25·c2 (multilinear-exact), chroma constant 0x8080.
        let mut samples = Vec::new();
        for c0 in 0..2u32 {
            for c1 in 0..2u32 {
                for c2 in 0..2u32 {
                    for _c3 in 0..2u32 {
                        let mixed =
                            0.5 * f64::from(c0) + 0.25 * f64::from(c1) + 0.25 * f64::from(c2);
                        #[expect(clippy::cast_possible_truncation, reason = "mixed <= 1")]
                        #[expect(clippy::cast_sign_loss, reason = "mixed >= 0")]
                        samples.extend([(mixed * 65535.0 + 0.5) as u16, 0x8080, 0x8080]);
                    }
                }
            }
        }
        TagData::LutAToB(LutAToB {
            input_channels: 4,
            output_channels: 3,
            a_curves: None,
            clut: Some(Clut {
                grid_points: vec![2; 4],
                output_channels: 3,
                precision: ClutPrecision::U16,
                samples,
            }),
            m_curves: Some(vec![
                CurveOrParametric::Curve(curve.clone()),
                CurveOrParametric::Curve(gamut_icc::Curve::Identity),
                CurveOrParametric::Curve(gamut_icc::Curve::Identity),
            ]),
            matrix: None,
            b_curves: vec![CurveOrParametric::Curve(gamut_icc::Curve::Identity); 3],
        })
    };
    let mut header = ProfileHeader::new(DeviceClass::Output, ColorSpace::Cmyk);
    header.pcs = ColorSpace::Lab;
    header.version.major = 2;
    IccProfile {
        header,
        tags: vec![
            (Signature(*b"A2B0"), a2b()),
            (Signature(*b"A2B1"), a2b()),
            (Signature(*b"B2A0"), b2a()),
            (Signature(*b"B2A1"), b2a()),
        ],
    }
}

/// An RGB↔XYZ output-class v2 profile: `B2A0` passes encoded XYZ to the device channels,
/// `A2B0` crushes them through a hard toe (`max(0, x − 0.15)/0.85`) back to XYZ — an
/// XYZ-PCS estimator vehicle (the round trip bridges Lab↔XYZ on both sides) whose fitted
/// root clamps to exactly `L* = 50`, so the estimator's success is decisively non-zero.
fn rgb_xyz_toe_profile() -> IccProfile {
    use gamut_icc::{
        Clut, ClutPrecision, ColorSpace, CurveOrParametric, DeviceClass, LutAToB, LutBToA,
        ProfileHeader, Signature, TagData,
    };
    let pass3 = |out_of: fn(u16, u16, u16) -> [u16; 3]| {
        let mut samples = Vec::new();
        for x in 0..2u16 {
            for y in 0..2u16 {
                for z in 0..2u16 {
                    samples.extend(out_of(x * 65535, y * 65535, z * 65535));
                }
            }
        }
        samples
    };
    let b2a = TagData::LutBToA(LutBToA {
        input_channels: 3,
        output_channels: 3,
        b_curves: vec![CurveOrParametric::Curve(gamut_icc::Curve::Identity); 3],
        matrix: None,
        m_curves: None,
        clut: Some(Clut {
            grid_points: vec![2; 3],
            output_channels: 3,
            precision: ClutPrecision::U16,
            samples: pass3(|x, y, z| [x, y, z]),
        }),
        a_curves: None,
    });
    let toe = sampled(|x| (x - 0.15) / 0.85);
    let a2b = TagData::LutAToB(LutAToB {
        input_channels: 3,
        output_channels: 3,
        a_curves: Some(vec![CurveOrParametric::Curve(toe.clone()); 3]),
        clut: Some(Clut {
            grid_points: vec![2; 3],
            output_channels: 3,
            precision: ClutPrecision::U16,
            samples: pass3(|x, y, z| [x, y, z]),
        }),
        m_curves: None,
        matrix: None,
        b_curves: vec![CurveOrParametric::Curve(gamut_icc::Curve::Identity); 3],
    });
    let mut header = ProfileHeader::new(DeviceClass::Output, ColorSpace::Rgb);
    header.pcs = ColorSpace::Xyz;
    header.version.major = 2;
    IccProfile {
        header,
        tags: vec![(Signature(*b"A2B0"), a2b), (Signature(*b"B2A0"), b2a)],
    }
}

/// Serializes a hand-built profile and asserts both detectors against the oracle, per
/// intent. `None` from the oracle must coincide with our zero.
fn assert_zoo_differential(
    profile: &IccProfile,
    cases: &[(RenderingIntent, u32)],
    tol: f64,
    label: &str,
) {
    let bytes = profile
        .to_bytes()
        .expect("gamut-icc serializes the vehicle");
    let reparsed = IccProfile::parse(&bytes).expect("vehicle round-trips");
    let oracle = Profile::from_bytes(&bytes).expect("lcms2 opens the vehicle");
    for &(ours, lcms) in cases {
        assert_detection(&reparsed, &oracle, ours, lcms, tol, label);
    }
}

const RELATIVE: (RenderingIntent, u32) = (
    RenderingIntent::MediaRelativeColorimetric,
    INTENT_RELATIVE_COLORIMETRIC,
);
const PERCEPTUAL: (RenderingIntent, u32) = (RenderingIntent::Perceptual, INTENT_PERCEPTUAL);

#[test]
fn zoo_straightness_band_profiles_match_lcms2() {
    set_quiet_log_handler();
    // "Straight only through the protected band": a 30→37 pedestal ramp whose low region
    // deviates ≥ 4 L* but sits inside the bottom-20% band, plateauing to in = 41 (< the
    // band edge 43.4) — the shortcut keeps the initial black (L* = 30, chromatic). Breaking
    // the band arm or its arithmetic sends this through the quadratic fit instead, whose
    // root lands near zero — far from the oracle. Measured worst |Δ| 5.3e-9 (the value is
    // the chromatic initial black, Lab(30, 15.4, 15.4)).
    let plateau_41 = sampled(|x| {
        let l = x * 100.0;
        let out = if l <= 10.0 {
            30.0 + 0.7 * l
        } else if l <= 41.0 {
            37.0
        } else {
            (l - 3.0).max(37.0)
        };
        out / 100.0
    });
    assert_zoo_differential(
        &gray_ramp_profile(&plateau_41),
        &[RELATIVE],
        1e-5,
        "plateau-41",
    );

    // The same ramp with the plateau pushed past the band edge (to in = 50): no longer
    // straight, so the estimator runs and lands near zero — while a widened band (the
    // `max − min` → `max + min` mutation) would flip it back to the initial black. Also the
    // vehicle for the gray arm of the estimator's space gate (a delegating mutant returns
    // the source black, L* = 30, instead of the estimator's L* = 0 with kept chroma).
    // Measured worst |Δ| 1.8e-11.
    let plateau_50 = sampled(|x| {
        let l = x * 100.0;
        let out = if l <= 10.0 {
            30.0 + 0.7 * l
        } else if l <= 50.0 {
            37.0
        } else {
            (l - 3.0).max(37.0)
        };
        out / 100.0
    });
    assert_zoo_differential(
        &gray_ramp_profile(&plateau_50),
        &[RELATIVE],
        1e-5,
        "plateau-50",
    );
}

#[test]
fn zoo_chroma_clamp_and_monotonization_match_lcms2() {
    set_quiet_log_handler();
    // The chroma-mixing toe: the return L* depends on the ramp's (clamped) a*/b* inputs, so
    // the ±50 clamps shift the toe break (root 19.8 → 9.9 under a broken clamp, an XYZ
    // shift of ≈ 1.8e-2). Measured worst |Δ| 3.5e-6.
    let toe = sampled(|t| (t - 0.35) / 0.4);
    assert_zoo_differential(&cmyk_mix_profile(&toe), &[RELATIVE], 1e-4, "chroma-mix toe");

    // The same toe with a late dip (out(70) = 40): top-down monotonization flattens
    // everything in (51.8, 70) down to 40, pulling a plateau into the fit window — skipping
    // the pass leaves a lone dip point instead and the fitted root moves by ≈ 1 L*
    // (≈ 3.3e-3 in XYZ, past this bound). Measured worst |Δ| 1.3e-5.
    let dipped = sampled(|t| {
        let l = t * 100.0;
        if (69.5..70.5).contains(&l) {
            0.40
        } else {
            (t - 0.35) / 0.4
        }
    });
    assert_zoo_differential(&cmyk_mix_profile(&dipped), &[RELATIVE], 1e-3, "dipped toe");
}

#[test]
fn zoo_degenerate_ramps_match_lcms2() {
    set_quiet_log_handler();
    // A constant ramp (every L* → 100): the ends never ascend, so detection reports no
    // black point on both sides — while a weakened rejection would fall through to the
    // straightness shortcut and return the (non-zero) initial black.
    let constant = sampled(|_| 1.0);
    assert_zoo_differential(
        &cmyk_mix_profile(&constant),
        &[RELATIVE],
        1e-9,
        "constant ramp",
    );

    // Exactly three fit points ({0.15, 0.25, 0.35} of the normalized range): admitted by
    // the n < 3 gate, refused by the n < 4 fitter — net L* = 0 with the initial black's
    // CHROMA kept (the gray vehicle's constant a* = b* ≈ 15.44), so the result is decisively
    // non-zero and a broken count gate (→ pure zero) cannot fake it. Measured worst
    // |Δ| 1.8e-11.
    let three_points = sampled(|x| {
        let l = (x * 255.0).round();
        match l as u32 {
            251 => 0.15,
            252 => 0.25,
            253 => 0.35,
            254 => 0.60,
            255 => 1.0,
            _ => 0.0,
        }
    });
    assert_zoo_differential(
        &gray_ramp_profile(&three_points),
        &[RELATIVE],
        1e-5,
        "n = 3",
    );
}

#[test]
fn zoo_xyz_pcs_estimator_matches_lcms2() {
    set_quiet_log_handler();
    // An XYZ-PCS RGB output CLUT: the round trip bridges Lab↔XYZ with the rounded D50 on
    // both sides. The toe pushes the fitted root past the clamp, so the estimator returns
    // exactly Lab(50, 0, 0) — any path flip (a delegating space-gate mutant → the zero
    // source black; a broken Lab↔XYZ bridge → a constant, rejected ramp) lands on zero
    // instead. Perceptual = the 0.03/0.25 fit region on a v2 profile; relative (no B2A1)
    // delegates to the refused source probe on both sides. Measured worst |Δ| 4.0e-9.
    assert_zoo_differential(
        &rgb_xyz_toe_profile(),
        &[PERCEPTUAL, RELATIVE],
        1e-5,
        "rgb xyz toe",
    );
}
