//! Drives the crate through its public API only: each of the 12 `av1`/`math` public functions is
//! exercised via its `gamut_dsp::module::item` path, proving that surface self-sufficient — a
//! dropped export or re-flattened module breaks this file before it breaks a consumer crate. The
//! four `jpeg` functions are not driven here; their inline tests and doctests pin them.

use gamut_dsp::av1::{
    forward_adst, forward_dct, forward_identity, forward_wht4x4, inverse_adst, inverse_dct,
    inverse_identity, inverse_wht4x4,
};
use gamut_dsp::math::{clip3, round_div_nearest, round2, round2_signed};

/// Assert `got` is proportional to `want` with a positive scale, within `tol` per entry.
fn assert_proportional(got: &[i64], want: &[i64], tol: f64, ctx: &str) {
    let anchor = (0..want.len())
        .max_by_key(|&i| want[i].abs())
        .expect("non-empty");
    let scale = got[anchor] as f64 / want[anchor] as f64;
    assert!(scale > 0.0, "{ctx}: reconstruction flipped sign");
    for (i, (&g, &w)) in got.iter().zip(want).enumerate() {
        let predicted = scale * w as f64;
        assert!(
            (g as f64 - predicted).abs() <= tol,
            "{ctx}: entry {i} = {g}, expected ≈ {predicted:.1}"
        );
    }
}

#[test]
fn av1_transform_kernels_roundtrip() {
    let resid: [i64; 8] = [64, -32, 80, -8, 12, 40, -56, 24];

    // A miniature encode → decode pass: forward DCT, coarse quantize with the shared
    // forward-quantize rounding, dequantize, inverse DCT — reconstruction is proportional to
    // the input within the quantizer's coarseness.
    let mut t = resid;
    forward_dct(&mut t, 3);
    let q = 8i32;
    for c in &mut t {
        *c = i64::from(round_div_nearest(*c as i32, q)) * i64::from(q);
    }
    inverse_dct(&mut t, 3, 24);
    assert_proportional(&t, &resid, 80.0, "dct quantized roundtrip");

    let mut t = resid;
    forward_adst(&mut t, 3);
    inverse_adst(&mut t, 3, 24);
    assert_proportional(&t, &resid, 64.0, "adst roundtrip");

    let mut t = resid;
    forward_identity(&mut t, 3);
    inverse_identity(&mut t, 3);
    for (i, (&got, &want)) in t.iter().zip(&resid).enumerate() {
        assert!(
            (got - want).abs() <= 2,
            "identity entry {i}: {got} vs {want}"
        );
    }

    // The lossless WHT pair round-trips exactly.
    let block: [i32; 16] = [
        1, -2, 3, -4, 5, -6, 7, -8, 9, -10, 11, -12, 13, -14, 15, -16,
    ];
    assert_eq!(inverse_wht4x4(&forward_wht4x4(&block)), block);
}

#[test]
fn math_primitives_answer_exactly() {
    assert_eq!(round2(7, 1), 4);
    assert_eq!(round2_signed(-7, 1), -4);
    assert_eq!(clip3(0, 255, 300), 255);
    assert_eq!(round_div_nearest(-10, 4), -3);
}
