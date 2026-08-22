//! Differential tests of the matrix/TRC shaper linking (`gamut_cmm::link`) against Little-CMS.
//!
//! Every oracle profile is synthesized in memory by `tooling/lcms2-oracle` and serialized with
//! `Profile::to_bytes`; **both** sides then read those same bytes — `gamut-icc` parses them
//! and lcms2 reopens them (`Profile::from_bytes`) — so tag quantization (s15Fixed16
//! colorants, encoded curves) is identical on both sides and the comparison isolates
//! evaluation semantics. The lcms2 side runs end-to-end transforms (`cmsCreateTransform`,
//! `NOOPTIMIZE|NOCACHE`) against the built-in XYZ identity profile with `TYPE_XYZ_DBL`
//! formatters, which produce **decoded** XYZ (D50 `Y = 1.0`) — directly comparable to this
//! crate's decoded-PCS pipelines, with no `InpAdj`/`OutpAdj` bookkeeping on either side.
//!
//! Measured-and-asserted bounds (worst over the sweeps; rationale at each assert):
//!
//! - device→PCS raw XYZ: lcms2's float transform path evaluates in `f32`, so agreement is
//!   f32-rounding-tight — measured ≤ 8.8e-8 for the RGB shapers; gray adds the documented
//!   D50-constant split (lcms2's truncated `cmsD50X/Z` vs this crate's s15Fixed16 PCS
//!   illuminant, ≤ 5.5e-6 apart; measured 5.4e-6).
//! - PCS→device: both sides invert these single-segment parametric TRCs *analytically*
//!   (lcms2 via `cmsReverseToneCurveEx`'s negated-type path), so away from black agreement
//!   is again f32-tight (measured ≤ 8.4e-8); near black a pure-gamma inverse's unbounded
//!   slope amplifies lcms2's f32 rounding (measured ≤ 4.2e-4 for γ ≈ 2.2).
//! - our own round trips (device→PCS→device) are analytic end to end: ≤ ~1.5e-15 for
//!   the piecewise (toe-limited) TRCs; a pure-gamma TRC amplifies the f64 `M·M⁻¹` residue
//!   at an exactly-zero channel to `ε^(1/γ)` ≈ 3e-8, which sets the asserted envelope.

use gamut_cmm::{Pipeline, Stage, device_to_pcs, pcs_to_device};
use gamut_color::lab::{D50_XYZ, delta_e_2000, xyz_to_lab};
use gamut_icc::{IccProfile, KnownTag, RenderingIntent};
use lcms2_oracle::{
    FLAGS_NOCACHE, FLAGS_NOOPTIMIZE, INTENT_RELATIVE_COLORIMETRIC, Profile, TYPE_GRAY_DBL,
    TYPE_RGB_DBL, TYPE_XYZ_DBL, Transform, display_p3_srgb_trc, gray, rgb_matrix_shaper,
    rgb_matrix_shaper_v2, set_quiet_log_handler, srgb, xyz,
};

/// The relative-colorimetric intent on our side of every differential.
const RELATIVE: RenderingIntent = RenderingIntent::MediaRelativeColorimetric;

/// `u8Fixed8`-exact gamma 563/256 = 2.19921875: identical after both the v2 `curv` gamma
/// encoding and the v4 `para` s15Fixed16 encoding, so v2-vs-v4 comparisons carry no
/// curve-quantization noise.
const EXACT_GAMMA: f64 = 563.0 / 256.0;

/// D65 chromaticity (the shaper synthesizers' white argument).
const D65_XY: [f64; 2] = [0.3127, 0.3290];

/// D50 chromaticity derived from lcms2's own `cmsD50` constants (X+Y+Z = 2.7891), so the
/// gray profile's `wtpt` lands on lcms2's D50 and the relative-intent adaptation between the
/// gray profile and the XYZ identity profile is numerically the identity.
const D50_XY: [f64; 2] = [0.9642 / 2.7891, 1.0 / 2.7891];

/// Display P3 primaries.
const P3_PRIMARIES: [[f64; 2]; 3] = [[0.680, 0.320], [0.265, 0.690], [0.150, 0.060]];

/// Adobe-RGB-ish primaries.
const ADOBE_PRIMARIES: [[f64; 2]; 3] = [[0.640, 0.330], [0.210, 0.710], [0.150, 0.060]];

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

/// Serializes an oracle-synthesized profile once and hands the **same bytes** to both sides:
/// parsed by `gamut-icc` for this crate, reopened by lcms2 for the oracle transform. Without
/// the reopen, lcms2 would evaluate its unserialized in-memory tag data (full-`f64`
/// colorants), and the differential would measure s15Fixed16 tag quantization (~1e-5)
/// instead of evaluation semantics.
fn reopen(profile: &Profile) -> (IccProfile, Profile) {
    let bytes = profile.to_bytes();
    let parsed = IccProfile::parse(&bytes).expect("gamut-icc parses the lcms2-written profile");
    let oracle = Profile::from_bytes(&bytes).expect("lcms2 reopens its own bytes");
    (parsed, oracle)
}

/// The RGB device sweep: corners, gray steps, sRGB-junction neighbourhoods (0.04045 is the
/// encoded-domain seam of the piecewise TRC), and seeded random fill to ≥ 220 points — all
/// strictly inside `[0, 1]` (lcms2 leaves parametric curves unclamped outside the unit
/// interval where this crate clamps; in-range behaviour is the contract under test).
fn rgb_sweep(seed: u64) -> Vec<[f64; 3]> {
    let mut points: Vec<[f64; 3]> = Vec::new();
    for corner in 0..8_u32 {
        points.push([
            f64::from(corner & 1),
            f64::from((corner >> 1) & 1),
            f64::from((corner >> 2) & 1),
        ]);
    }
    for i in 0..=10 {
        let g = f64::from(i) / 10.0;
        points.push([g, g, g]);
    }
    for junction in [0.04045 - 1e-6, 0.04045, 0.04045 + 1e-6, 0.0031308] {
        points.push([junction, 0.5, 1.0 - junction]);
        points.push([junction; 3]);
    }
    let mut lcg = Lcg(seed);
    while points.len() < 220 {
        points.push([lcg.next_unit(), lcg.next_unit(), lcg.next_unit()]);
    }
    points
}

/// The gray device sweep: endpoints, a uniform ramp, and seeded random fill to ≥ 200 points.
fn gray_sweep(seed: u64) -> Vec<f64> {
    let mut points: Vec<f64> = (0..=32).map(|i| f64::from(i) / 32.0).collect();
    let mut lcg = Lcg(seed);
    while points.len() < 200 {
        points.push(lcg.next_unit());
    }
    points
}

fn eval3(pipeline: &Pipeline, input: &[f64]) -> [f64; 3] {
    let mut out = [0.0; 3];
    pipeline.eval(input, &mut out).unwrap();
    out
}

/// Worst |Δ| per XYZ component and worst ΔE₀₀ (both sides converted D50-relative) between our
/// device→PCS pipeline and the lcms2 profile→XYZ transform, over `points`.
fn device_to_pcs_worst(
    oracle_profile: &Profile,
    ours: &Pipeline,
    in_format: u32,
    points: &[Vec<f64>],
) -> (f64, f64) {
    let to_xyz = Transform::new(
        oracle_profile,
        in_format,
        &xyz(),
        TYPE_XYZ_DBL,
        INTENT_RELATIVE_COLORIMETRIC,
        FLAGS_NOOPTIMIZE | FLAGS_NOCACHE,
    );
    let (mut worst_xyz, mut worst_de) = (0.0_f64, 0.0_f64);
    for point in points {
        let got = eval3(ours, point);
        let want = to_xyz.apply_f64(point, 1, 3);
        for ch in 0..3 {
            worst_xyz = worst_xyz.max((got[ch] - want[ch]).abs());
        }
        let de = delta_e_2000(
            xyz_to_lab(got, D50_XYZ),
            xyz_to_lab([want[0], want[1], want[2]], D50_XYZ),
        );
        worst_de = worst_de.max(de);
    }
    (worst_xyz, worst_de)
}

#[test]
fn rgb_device_to_pcs_matches_lcms2() {
    set_quiet_log_handler();
    let adobe = rgb_matrix_shaper(D65_XY, ADOBE_PRIMARIES, [EXACT_GAMMA; 3]);
    for (name, synthesized) in [
        ("sRGB", srgb()),
        ("Display P3", display_p3_srgb_trc()),
        ("Adobe-ish", adobe),
    ] {
        let (parsed, oracle_profile) = reopen(&synthesized);
        let ours = device_to_pcs(&parsed, RELATIVE).unwrap();
        let points: Vec<Vec<f64>> = rgb_sweep(11).into_iter().map(|p| p.to_vec()).collect();
        let (worst_xyz, worst_de) =
            device_to_pcs_worst(&oracle_profile, &ours, TYPE_RGB_DBL, &points);
        // lcms2 evaluates its float transforms in f32 (this crate in f64 over the same parsed
        // tag values); measured worst 8.8e-8 across the three profiles. 1e-6 = f32 headroom.
        assert!(worst_xyz < 1e-6, "{name}: worst XYZ |Δ| = {worst_xyz:e}");
        // ΔE₀₀ over the same pairs; measured ≤ 1.5e-5 (the near-black region inflates tiny
        // XYZ differences, so the perceptual bound is looser than the raw one).
        assert!(worst_de < 1e-3, "{name}: worst ΔE00 = {worst_de:e}");
    }
}

#[test]
fn gray_device_to_pcs_matches_lcms2() {
    set_quiet_log_handler();
    let (parsed, oracle_profile) = reopen(&gray(D50_XY, 2.2));
    let ours = device_to_pcs(&parsed, RELATIVE).unwrap();
    let points: Vec<Vec<f64>> = gray_sweep(13).into_iter().map(|g| vec![g]).collect();
    let (worst_xyz, worst_de) = device_to_pcs_worst(&oracle_profile, &ours, TYPE_GRAY_DBL, &points);
    // Dominated by the documented D50 constant split: lcms2's `cmsD50X/Z` are the truncated
    // 0.9642/0.8249 while this crate uses the s15Fixed16 PCS illuminant (≤ 5.5e-6 apart in
    // Z). Measured worst 5.4e-6 raw / 8.6e-4 ΔE00.
    assert!(worst_xyz < 2e-5, "gray: worst XYZ |Δ| = {worst_xyz:e}");
    assert!(worst_de < 5e-3, "gray: worst ΔE00 = {worst_de:e}");
}

/// The three-way chromatic-adaptation cases the settled convention demands, tested
/// separately: the `chad` tag must be observably inert on this relative path.
#[test]
fn chad_v2_with_and_without_are_bitwise_identical() {
    set_quiet_log_handler();
    let (with_chad, _) = reopen(&rgb_matrix_shaper_v2(
        true,
        D65_XY,
        P3_PRIMARIES,
        [EXACT_GAMMA; 3],
    ));
    let (without_chad, _) = reopen(&rgb_matrix_shaper_v2(
        false,
        D65_XY,
        P3_PRIMARIES,
        [EXACT_GAMMA; 3],
    ));
    // Guard against vacuity: the two profiles really differ by (exactly) the chad tag.
    assert!(with_chad.get(KnownTag::ChromaticAdaptation).is_some());
    assert!(without_chad.get(KnownTag::ChromaticAdaptation).is_none());
    for (ours_with, ours_without) in [
        (
            device_to_pcs(&with_chad, RELATIVE).unwrap(),
            device_to_pcs(&without_chad, RELATIVE).unwrap(),
        ),
        (
            pcs_to_device(&with_chad, RELATIVE).unwrap(),
            pcs_to_device(&without_chad, RELATIVE).unwrap(),
        ),
    ] {
        for point in rgb_sweep(17) {
            let a = eval3(&ours_with, &point);
            let b = eval3(&ours_without, &point);
            // Exact equality: the chad tag is never read, so the pipelines are built from
            // identical tag data and must agree bit for bit.
            assert_eq!(a, b, "chad changed the result at {point:?}");
        }
    }
}

#[test]
fn chad_v2_and_v4_of_the_same_colorimetry_agree() {
    set_quiet_log_handler();
    let (v2, _) = reopen(&rgb_matrix_shaper_v2(
        true,
        D65_XY,
        P3_PRIMARIES,
        [EXACT_GAMMA; 3],
    ));
    let (v4, _) = reopen(&rgb_matrix_shaper(D65_XY, P3_PRIMARIES, [EXACT_GAMMA; 3]));
    assert_eq!(v2.header.version.major, 2, "v2 profile really is v2");
    assert_eq!(v4.header.version.major, 4, "v4 profile really is v4");
    let ours_v2 = device_to_pcs(&v2, RELATIVE).unwrap();
    let ours_v4 = device_to_pcs(&v4, RELATIVE).unwrap();
    let mut worst = 0.0_f64;
    for point in rgb_sweep(19) {
        let a = eval3(&ours_v2, &point);
        let b = eval3(&ours_v4, &point);
        for ch in 0..3 {
            worst = worst.max((a[ch] - b[ch]).abs());
        }
    }
    // Same colorant tags (identical s15Fixed16 values) and a gamma exactly representable in
    // both the v2 `curv` u8Fixed8 and the v4 `para` s15Fixed16 encodings: only the parsed
    // representation differs, so agreement is f64-tight. Measured 0.0 exactly.
    assert!(worst < 1e-12, "v2 vs v4: worst |Δ| = {worst:e}");
}

#[test]
fn chad_v2_shapers_match_lcms2() {
    set_quiet_log_handler();
    for (name, with_chad) in [("with chad", true), ("without chad", false)] {
        let (parsed, oracle_profile) = reopen(&rgb_matrix_shaper_v2(
            with_chad,
            D65_XY,
            P3_PRIMARIES,
            [EXACT_GAMMA; 3],
        ));
        let ours = device_to_pcs(&parsed, RELATIVE).unwrap();
        let points: Vec<Vec<f64>> = rgb_sweep(23).into_iter().map(|p| p.to_vec()).collect();
        let (worst_xyz, worst_de) =
            device_to_pcs_worst(&oracle_profile, &ours, TYPE_RGB_DBL, &points);
        // Same bound/rationale as the v4 differential: lcms2 reads v2 colorants as-is too
        // (the settled convention this test pins). Measured 7.6e-8 raw / 8.3e-6 ΔE00, for
        // both variants.
        assert!(worst_xyz < 1e-6, "v2 {name}: worst XYZ |Δ| = {worst_xyz:e}");
        assert!(worst_de < 1e-3, "v2 {name}: worst ΔE00 = {worst_de:e}");
    }
}

#[test]
fn pcs_to_device_matches_lcms2() {
    set_quiet_log_handler();
    let adobe = rgb_matrix_shaper(D65_XY, ADOBE_PRIMARIES, [EXACT_GAMMA; 3]);
    for (name, synthesized) in [("sRGB", srgb()), ("Adobe-ish", adobe)] {
        let (parsed, oracle_profile) = reopen(&synthesized);
        let forward = device_to_pcs(&parsed, RELATIVE).unwrap();
        let reverse = pcs_to_device(&parsed, RELATIVE).unwrap();
        let from_xyz = Transform::new(
            &xyz(),
            TYPE_XYZ_DBL,
            &oracle_profile,
            TYPE_RGB_DBL,
            INTENT_RELATIVE_COLORIMETRIC,
            FLAGS_NOOPTIMIZE | FLAGS_NOCACHE,
        );
        let (mut worst, mut worst_bright) = (0.0_f64, 0.0_f64);
        for point in rgb_sweep(29) {
            // In-gamut XYZ inputs: the forward image of the device sweep.
            let pcs = eval3(&forward, &point);
            let got = eval3(&reverse, &pcs);
            let want = from_xyz.apply_f64(&pcs, 1, 3);
            for ch in 0..3 {
                let delta = (got[ch] - want[ch]).abs();
                worst = worst.max(delta);
                if point.iter().all(|&v| v >= 0.15) {
                    worst_bright = worst_bright.max(delta);
                }
            }
        }
        // Both sides invert these single-segment parametric TRCs analytically (lcms2 via
        // `cmsReverseToneCurveEx`'s negated-type path), so away from black the difference is
        // lcms2's f32 evaluation — measured ≤ 8.4e-8. Near black the pure-gamma inverse's
        // unbounded slope amplifies that f32 rounding — measured 4.2e-4 (Adobe-ish, γ≈2.2;
        // sRGB's linear toe keeps even the dark end at 8.2e-7).
        assert!(
            worst_bright < 1e-6,
            "{name}: worst device |Δ| away from black = {worst_bright:e}"
        );
        assert!(worst < 2e-3, "{name}: worst device |Δ| = {worst:e}");
    }
    // Gray, same shape: XYZ → gray device.
    let (parsed, oracle_gray) = reopen(&gray(D50_XY, 2.2));
    let forward = device_to_pcs(&parsed, RELATIVE).unwrap();
    let reverse = pcs_to_device(&parsed, RELATIVE).unwrap();
    let from_xyz = Transform::new(
        &xyz(),
        TYPE_XYZ_DBL,
        &oracle_gray,
        TYPE_GRAY_DBL,
        INTENT_RELATIVE_COLORIMETRIC,
        FLAGS_NOOPTIMIZE | FLAGS_NOCACHE,
    );
    let (mut worst, mut worst_bright) = (0.0_f64, 0.0_f64);
    for g in gray_sweep(31) {
        let pcs = eval3(&forward, &[g]);
        let mut got = [0.0; 1];
        reverse.eval(&pcs, &mut got).unwrap();
        let want = from_xyz.apply_f64(&pcs, 1, 1);
        let delta = (got[0] - want[0]).abs();
        worst = worst.max(delta);
        if g >= 0.15 {
            worst_bright = worst_bright.max(delta);
        }
    }
    // Same analytic-vs-analytic rationale; the Y pick keeps even the dark end tame.
    // Measured 2.3e-8 overall.
    assert!(
        worst_bright < 1e-6,
        "gray: worst device |Δ| away from black = {worst_bright:e}"
    );
    assert!(worst < 2e-3, "gray: worst device |Δ| = {worst:e}");
}

#[test]
fn round_trips_are_analytically_tight_and_track_lcms2() {
    set_quiet_log_handler();
    let adobe = rgb_matrix_shaper(D65_XY, ADOBE_PRIMARIES, [EXACT_GAMMA; 3]);
    for (name, synthesized) in [
        ("sRGB", srgb()),
        ("Display P3", display_p3_srgb_trc()),
        ("Adobe-ish", adobe),
    ] {
        let (parsed, oracle_profile) = reopen(&synthesized);
        let round_trip = device_to_pcs(&parsed, RELATIVE)
            .unwrap()
            .compose(pcs_to_device(&parsed, RELATIVE).unwrap())
            .unwrap();
        let lcms_round_trip = Transform::new(
            &oracle_profile,
            TYPE_RGB_DBL,
            &oracle_profile,
            TYPE_RGB_DBL,
            INTENT_RELATIVE_COLORIMETRIC,
            FLAGS_NOOPTIMIZE | FLAGS_NOCACHE,
        );
        let (mut worst_identity, mut worst_vs_lcms) = (0.0_f64, 0.0_f64);
        for point in rgb_sweep(37) {
            let ours = eval3(&round_trip, &point);
            let lcms = lcms_round_trip.apply_f64(&point, 1, 3);
            for ch in 0..3 {
                worst_identity = worst_identity.max((ours[ch] - point[ch]).abs());
                worst_vs_lcms = worst_vs_lcms.max((ours[ch] - lcms[ch]).abs());
            }
        }
        // Analytic inverses end to end: measured ≤ 1.5e-15 for the toe-limited piecewise
        // TRCs (sRGB/P3). A pure-gamma TRC (Adobe-ish) amplifies the f64 `M·M⁻¹` residue at
        // an exactly-zero channel through the inverse's unbounded slope at 0 — `ε^(1/γ)`
        // with ε ~ 1e-16 gives the measured worst, 3.0e-8.
        assert!(
            worst_identity < 1e-6,
            "{name}: round trip drift = {worst_identity:e}"
        );
        // lcms2's own round trip is analytic too, in f32; ours must track it within f32
        // headroom. Measured ≤ 3.0e-8.
        assert!(
            worst_vs_lcms < 1e-6,
            "{name}: vs lcms2 round trip = {worst_vs_lcms:e}"
        );
    }
    // Gray round trip.
    let (parsed, _) = reopen(&gray(D50_XY, 2.2));
    let round_trip = device_to_pcs(&parsed, RELATIVE)
        .unwrap()
        .compose(pcs_to_device(&parsed, RELATIVE).unwrap())
        .unwrap();
    let mut worst = 0.0_f64;
    for g in gray_sweep(41) {
        let mut out = [0.0; 1];
        round_trip.eval(&[g], &mut out).unwrap();
        worst = worst.max((out[0] - g).abs());
    }
    // Pure-gamma power inverse over a 1-D pipeline with no matrix residue: measured 5.6e-17.
    assert!(worst < 1e-12, "gray round trip drift = {worst:e}");
}

/// Pins the assembled sRGB colorant matrix against Bruce Lindbloom's published sRGB→XYZ
/// D50-adapted (Bradford) matrix — an independent external oracle that kills any
/// row/column-transposition mutant in the matrix assembly.
#[test]
fn srgb_colorant_matrix_matches_lindbloom() {
    set_quiet_log_handler();
    let (parsed, _) = reopen(&srgb());
    let ours = device_to_pcs(&parsed, RELATIVE).unwrap();
    let Stage::Matrix { m, offset } = &ours.stages()[1] else {
        panic!("stage 1 must be the colorant matrix");
    };
    let lindbloom = [
        [0.436_074_7, 0.385_064_9, 0.143_080_4],
        [0.222_504_5, 0.716_878_6, 0.060_616_9],
        [0.013_932_2, 0.097_104_5, 0.714_173_3],
    ];
    for r in 0..3 {
        for c in 0..3 {
            // Chromaticity/whitepoint rounding differences between lcms2's Bradford
            // derivation and Lindbloom's published values reach 2.7e-4 (blue-column Z);
            // s15Fixed16 tag quantization adds ~1.5e-5. 5e-4 still pins every entry to its
            // position — the smallest cross-position gap is > 3e-2.
            assert!(
                (m[r][c] - lindbloom[r][c]).abs() < 5e-4,
                "m[{r}][{c}] = {} vs Lindbloom {}",
                m[r][c],
                lindbloom[r][c]
            );
        }
    }
    assert_eq!(*offset, [0.0; 3]);
}

#[test]
fn shaper_dispatch_survives_the_lut_phase() {
    set_quiet_log_handler();
    // The shaper fallback is only reached when no LUT tag exists for the direction: sRGB
    // (shaper tags only) must still build under every intent, and — shaper profiles carrying
    // no per-intent tables — build the identical pipeline for each.
    let (parsed, _) = reopen(&srgb());
    let baseline = device_to_pcs(&parsed, RELATIVE).unwrap();
    for intent in [
        RenderingIntent::Perceptual,
        RenderingIntent::Saturation,
        RenderingIntent::IccAbsoluteColorimetric,
    ] {
        let other = device_to_pcs(&parsed, intent).unwrap();
        for point in rgb_sweep(43).into_iter().take(40) {
            assert_eq!(
                eval3(&baseline, &point),
                eval3(&other, &point),
                "intent {intent:?} diverged"
            );
        }
    }
}
