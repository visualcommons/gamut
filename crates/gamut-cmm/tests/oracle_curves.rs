//! Differential tests of [`gamut_cmm::IccCurve`] against Little-CMS.
//!
//! Type mapping: an lcms2 parametric curve type is the **ICC function type + 1**
//! (`cmsgamma.c`: "Type is the ICC type +1"), with parameters in the *same* order
//! `[g, a, b, c, d, e, f]` — so ICC type 0 (`X^g`) is lcms2 type 1, and ICC type 4 (the
//! seven-parameter form) is lcms2 type 5. Parameters are quantized through `s15Fixed16`
//! (respectively `u8Fixed8` for pure gamma) **before** being handed to either side, so both
//! evaluate the identical curve and the comparison measures evaluation, not encoding.

mod common;

use common::Lcg;
use gamut_cmm::IccCurve;
use gamut_icc::{Curve, CurveOrParametric, ParametricCurve, S15Fixed16, U8Fixed8};
use lcms2_oracle::ToneCurve as OracleCurve;

/// `2.2` as `u8Fixed8` (563/256 = 2.19921875) — the exact gamma both sides evaluate.
const GAMMA_2_2_U8F8: f64 = 563.0 / 256.0;

/// Realistic parameters for every ICC parametric type, all with range inside `[0, 1]` (lcms2
/// does not clamp parametric outputs, this crate does — in-range curves keep the comparison
/// meaningful). Type 3 is exactly sRGB; type 4 exercises all seven parameters with a seam that
/// steps *up* at `d` (monotone: power branch 0.1207 vs toe 0.0975) and tops out at ≈ 0.843.
fn parametric_cases() -> Vec<(u16, Vec<f64>)> {
    vec![
        (0, vec![2.4]),
        // A flat toe: (x − 0.1)^1.8 clips to 0 below x = 0.1.
        (1, vec![1.8, 1.0, -0.1]),
        (2, vec![2.0, 0.8, 0.1, 0.05]),
        (
            3,
            vec![2.4, 1.0 / 1.055, 0.055 / 1.055, 1.0 / 12.92, 0.04045],
        ),
        (4, vec![2.2, 0.8, 0.1, 0.35, 0.25, 0.05, 0.01]),
    ]
}

/// Builds the same ICC parametric curve for both sides: ours from the `s15Fixed16`-quantized
/// parameters, the oracle's from the identical quantized values at lcms2 type `icc_type + 1`.
fn parametric_pair(icc_type: u16, params: &[f64]) -> (IccCurve, OracleCurve) {
    let quantized: Vec<S15Fixed16> = params.iter().map(|&p| S15Fixed16::from_f64(p)).collect();
    let ours = IccCurve::new(&CurveOrParametric::Parametric(ParametricCurve {
        function_type: icc_type,
        params: quantized.clone(),
    }))
    .unwrap();
    let f64_params: Vec<f64> = quantized.iter().map(|p| p.to_f64()).collect();
    let oracle = OracleCurve::parametric(i32::from(icc_type) + 1, &f64_params);
    (ours, oracle)
}

/// A `n`-entry `u16` table sampling `x^g` — spans the full `[0, 65535]` range.
fn gamma_u16_table(n: usize, g: f64) -> Vec<u16> {
    (0..n)
        .map(|i| {
            let x = i as f64 / (n - 1) as f64;
            (x.powf(g) * 65535.0).round() as u16
        })
        .collect()
}

fn tone_curve(curve: Curve) -> IccCurve {
    IccCurve::new(&CurveOrParametric::Curve(curve)).unwrap()
}

/// Worst absolute difference between our curve and the oracle's over `points + 1` evenly
/// spaced inputs in `[0, 1]`.
fn worst_forward_diff(ours: &IccCurve, oracle: &OracleCurve, points: usize) -> f64 {
    let mut worst: f64 = 0.0;
    for i in 0..=points {
        let x = i as f64 / points as f64;
        let diff = (ours.eval(x) - f64::from(oracle.eval_f32(x as f32))).abs();
        worst = worst.max(diff);
    }
    worst
}

#[test]
fn forward_identity_matches_oracle() {
    lcms2_oracle::set_quiet_log_handler();
    let ours = tone_curve(Curve::Identity);
    let oracle = OracleCurve::gamma(1.0); // lcms2 type 1, g = 1: exact identity, float path
    let worst = worst_forward_diff(&ours, &oracle, 1000);
    assert!(worst < 1e-7, "identity: worst |ours − lcms2| = {worst:e}");
}

#[test]
fn forward_gamma_matches_oracle() {
    lcms2_oracle::set_quiet_log_handler();
    // u8Fixed8 encoding of 2.2 is 563/256; the oracle gets the identical quantized exponent.
    let ours = tone_curve(Curve::Gamma(U8Fixed8::from_f64(2.2)));
    let oracle = OracleCurve::gamma(GAMMA_2_2_U8F8);
    let worst = worst_forward_diff(&ours, &oracle, 1000);
    // Parametric-vs-parametric in float: only f32 return rounding separates the two.
    assert!(worst < 1e-6, "gamma 2.2: worst |ours − lcms2| = {worst:e}");
}

#[test]
fn forward_sampled_table_matches_oracle_within_16bit_quantization() {
    lcms2_oracle::set_quiet_log_handler();
    let table = gamma_u16_table(256, 2.2);
    let ours = tone_curve(Curve::Sampled(table.clone()));
    let oracle = OracleCurve::tabulated_u16(&table);
    let worst = worst_forward_diff(&ours, &oracle, 1000);
    // lcms2's float path quantizes table-curve *inputs* to 16 bits and returns word/65535;
    // ours interpolates in f64. Bound: input snap (0.5/65535 · max slope ≈ 2.2) plus output
    // rounding (1/65535) ≈ 3.2e-5 — asserted with headroom at the documented 2e-4.
    assert!(worst < 2e-4, "sampled: worst |ours − lcms2| = {worst:e}");
}

#[test]
fn forward_parametric_types_match_oracle() {
    lcms2_oracle::set_quiet_log_handler();
    for (icc_type, params) in parametric_cases() {
        let (ours, oracle) = parametric_pair(icc_type, &params);
        let worst = worst_forward_diff(&ours, &oracle, 1000);
        // Both sides evaluate the same closed form over identical quantized parameters in
        // double precision; only the oracle's f32 return separates them.
        assert!(
            worst < 1e-6,
            "ICC type {icc_type}: worst |ours − lcms2| = {worst:e}"
        );
    }
}

#[test]
fn inverse_of_gamma_matches_oracle_reversed() {
    lcms2_oracle::set_quiet_log_handler();
    let ours = tone_curve(Curve::Gamma(U8Fixed8::from_f64(2.2)))
        .inverse()
        .unwrap();
    // lcms2 reverses a single-segment parametric analytically (negated type): y^(1/g) exactly.
    let oracle = OracleCurve::gamma(GAMMA_2_2_U8F8).reversed(4096);
    let worst = worst_forward_diff(&ours, &oracle, 1000);
    assert!(
        worst < 1e-6,
        "gamma inverse: worst |ours − lcms2| = {worst:e}"
    );
}

#[test]
fn inverse_of_sampled_tables_matches_oracle_reversed() {
    lcms2_oracle::set_quiet_log_handler();
    // Range-spanning tables (0 → 65535), so every reversal target is bracketed and the
    // carried-coefficient corner lcms2 and this crate resolve differently never triggers.
    let ascending = gamma_u16_table(256, 2.2);
    let descending: Vec<u16> = ascending.iter().rev().copied().collect();
    for (name, table, bound) in [
        // Both sides reverse the identical 256-entry table into 4096 entries with the same
        // interval-scan and flat-run conventions; what separates them is the oracle's 16-bit
        // quantization of the reversed table and of the evaluation input, amplified by the
        // inverse's slope near y = 0 (measured worst 1.38e-4 on both orientations).
        ("ascending", ascending, 5e-4),
        ("descending", descending, 5e-4),
    ] {
        let ours = tone_curve(Curve::Sampled(table.clone())).inverse().unwrap();
        let oracle = OracleCurve::tabulated_u16(&table).reversed(4096);
        let worst = worst_forward_diff(&ours, &oracle, 1000);
        assert!(worst < bound, "{name}: worst |ours − lcms2| = {worst:e}");
    }
}

#[test]
fn analytic_parametric_inverse_agrees_with_numeric_table_inverse() {
    lcms2_oracle::set_quiet_log_handler();
    // The sRGB curve through both inversion paths: analytic (parametric type 3 closed form)
    // and numeric (a 4096-entry sampling of the same curve reversed as a table).
    let (srgb, _) = parametric_pair(3, &parametric_cases()[3].1);
    let analytic = srgb.inverse().unwrap();
    let sampled: Vec<u16> = (0..4096)
        .map(|i| (srgb.eval(f64::from(i) / 4095.0) * 65535.0).round() as u16)
        .collect();
    let numeric = tone_curve(Curve::Sampled(sampled)).inverse().unwrap();
    let mut worst: f64 = 0.0;
    for i in 0..=1000 {
        let y = f64::from(i) / 1000.0;
        worst = worst.max((analytic.eval(y) - numeric.eval(y)).abs());
    }
    // The numeric path resolves to the 4096-entry grid over a 16-bit-quantized source; the
    // inverse's steepest slope is 12.92 (the sRGB toe). Measured worst 7.9e-5.
    assert!(worst < 5e-4, "worst |analytic − numeric| = {worst:e}");
}

#[test]
fn round_trip_holds_for_strictly_monotonic_curves() {
    lcms2_oracle::set_quiet_log_handler();
    // Analytic inverses: x ↦ inverse(curve(x)) is identity to f64 rounding.
    let mut analytic = vec![
        tone_curve(Curve::Gamma(U8Fixed8::from_f64(0.45))),
        tone_curve(Curve::Gamma(U8Fixed8::from_f64(1.0))),
        tone_curve(Curve::Gamma(U8Fixed8::from_f64(2.2))),
        tone_curve(Curve::Gamma(U8Fixed8::from_f64(2.4))),
    ];
    // Strictly monotonic parametric types (type 1's flat-toe case is covered by the
    // range-side battery below): 0, a positive-base 1, 2, and the sRGB-shaped 3 and 4.
    for (icc_type, params) in [
        (0_u16, vec![2.4]),
        (1, vec![1.8, 0.9, 0.05]),
        (2, vec![2.0, 0.8, 0.1, 0.05]),
        (3, parametric_cases()[3].1.clone()),
        (4, parametric_cases()[4].1.clone()),
    ] {
        analytic.push(parametric_pair(icc_type, &params).0);
    }
    for (k, curve) in analytic.iter().enumerate() {
        let inverse = curve.inverse().unwrap();
        for i in 0..=1000 {
            let x = f64::from(i) / 1000.0;
            let rt = inverse.eval(curve.eval(x));
            assert!(
                (rt - x).abs() < 1e-9,
                "analytic curve {k}: round trip at {x} gave {rt}"
            );
        }
    }

    // Numeric inverses over seeded strictly increasing random tables (increments in
    // [100, 600), so slopes stay within a factor ~6 of 1 and the 4096-entry inverse grid
    // resolves x well below the 5e-4 bound; measured worst 1.5e-4 across the seeds).
    for seed in [1_u64, 7, 42] {
        let mut lcg = Lcg::new(seed);
        let mut acc: u64 = 0;
        let raw: Vec<u64> = (0..257)
            .map(|_| {
                acc += u64::from(100 + lcg.next_u32() % 500);
                acc
            })
            .collect();
        let table: Vec<u16> = raw
            .iter()
            .map(|&v| ((v - raw[0]) * 65535 / (raw[256] - raw[0])) as u16)
            .collect();
        let curve = tone_curve(Curve::Sampled(table));
        let inverse = curve.inverse().unwrap();
        for i in 0..=1000 {
            let x = f64::from(i) / 1000.0;
            let rt = inverse.eval(curve.eval(x));
            assert!(
                (rt - x).abs() < 5e-4,
                "seed {seed}: round trip at {x} gave {rt}"
            );
        }
    }
}

#[test]
fn range_side_round_trip_holds_for_curves_with_flat_runs() {
    lcms2_oracle::set_quiet_log_handler();
    // Curves with flat runs have no two-sided inverse: over a run the inverse picks one
    // preimage (the documented edge). The range-side identity curve(inverse(curve(x))) ≈
    // curve(x) still holds everywhere — including the flat-toe parametric type 1.
    let mut battery = vec![parametric_pair(1, &parametric_cases()[1].1).0];
    for seed in [3_u64, 11] {
        let mut lcg = Lcg::new(seed);
        let mut acc: u64 = 0;
        // Increments in [0, 500): roughly one in five is small enough to quantize into a
        // flat run at 16 bits, and genuine zeros produce exact flats.
        let raw: Vec<u64> = (0..257)
            .map(|_| {
                acc += u64::from(lcg.next_u32() % 500);
                acc
            })
            .collect();
        let table: Vec<u16> = raw
            .iter()
            .map(|&v| ((v - raw[0]) * 65535 / (raw[256] - raw[0])) as u16)
            .collect();
        battery.push(tone_curve(Curve::Sampled(table)));
    }
    for (k, curve) in battery.iter().enumerate() {
        let inverse = curve.inverse().unwrap();
        for i in 0..=1000 {
            let x = f64::from(i) / 1000.0;
            let y = curve.eval(x);
            let rt = curve.eval(inverse.eval(y));
            // Measured worst 7.8e-5 across the battery.
            assert!(
                (rt - y).abs() < 5e-4,
                "curve {k}: range round trip at x={x} gave {rt}, want {y}"
            );
        }
    }
}

#[test]
fn descending_curve_agrees_with_oracle_and_round_trips() {
    lcms2_oracle::set_quiet_log_handler();
    let table: Vec<u16> = vec![65535, 0];
    let ours = tone_curve(Curve::Sampled(table.clone()));
    let oracle = OracleCurve::tabulated_u16(&table);
    assert!(oracle.is_descending());
    assert!(ours.is_monotonic());
    let worst = worst_forward_diff(&ours, &oracle, 1000);
    assert!(worst < 2e-4, "1 − x forward: worst = {worst:e}");

    let inverse = ours.inverse().unwrap();
    for i in 0..=1000 {
        let x = f64::from(i) / 1000.0;
        let rt = inverse.eval(ours.eval(x));
        assert!((rt - x).abs() < 1e-3, "round trip at {x} gave {rt}");
    }
}

#[test]
fn non_monotonic_tables_are_rejected_in_both_directions() {
    // Rises then falls, and falls then rises: both must report NonMonotonicCurve.
    for table in [vec![0, 40000, 20000, 65535], vec![65535, 20000, 40000, 0]] {
        let curve = tone_curve(Curve::Sampled(table));
        assert!(!curve.is_monotonic());
        assert!(matches!(
            curve.inverse().unwrap_err(),
            gamut_cmm::CmmError::NonMonotonicCurve
        ));
    }
}

#[test]
fn unknown_parametric_type_is_a_typed_error() {
    let err = IccCurve::new(&CurveOrParametric::Parametric(ParametricCurve {
        function_type: 5,
        params: vec![S15Fixed16::from_f64(2.0); 7],
    }))
    .unwrap_err();
    assert!(matches!(
        err,
        gamut_cmm::CmmError::UnsupportedParametricType(5)
    ));
}
