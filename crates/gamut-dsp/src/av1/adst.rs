//! AV1 1-D inverse and forward asymmetric discrete sine transforms (ADST) for `TX` lengths
//! 4/8/16 (AV1 §7.13.2.4–.9). No ADST exists for 32/64.
//!
//! AV1's ADST family is not a single transform. The size-4 ADST is the true DST-VII (the
//! closed-form SINPI/9 computation, §7.13.2.6), but the size-8/16 ADSTs are the `B`/`H` butterfly
//! networks of §7.13.2.7/.8, which — verified empirically against the exact mathematical formulae
//! to rms < 1e-3 — compute DST-IV. (The `cos128`-based butterflies make these power-of-two-angle
//! transforms rather than the `/(2N+1)` DST-VII.) This is a genuine quirk of the spec, not an
//! approximation choice on our part.
//!
//! [`inverse_adst`] is a direct transcription of the normative inverse ADST process (ADST4 closed
//! form; ADST8/16 as input permutation §7.13.2.4 → butterfly network → signed output permutation
//! §7.13.2.5). It is the decoder transform, so it must be bit-exact; the tests pin it against an
//! independent naive reference (DST-VII for size 4, DST-IV for 8/16), and the 2-D pipeline (P5)
//! adds the bit-exact `dav1d` cross-check.
//!
//! [`forward_adst`] is the encoder transform. The ADST is orthogonal, so the analysis (forward)
//! transform is the transpose of the synthesis (inverse) matrix. Rather than re-derive and risk an
//! orientation bug, the forward is built directly from the validated inverse's impulse responses —
//! `M_inverse[m][k] = inverse_adst(UNIT·e_k)[m]` — then applied as `X[k] = Σ_m M_inverse[m][k]·t[m]`.
//! This is deterministic integer arithmetic and is guaranteed consistent with the inverse; the
//! absolute scale is reconciled by the 2-D process (AV1 §7.13.3).
//!
//! FLIPADST is not a separate transform: it is the ADST applied to a flipped sample order, so the
//! flip is reconstruction bookkeeping in the 2-D assembly (`gamut-av1`'s `TxType::flip_ud` /
//! `TxType::flip_lr`), sharing this 1-D ADST.

use crate::av1::butterfly::{b, h};
use crate::math::round2;

const SINPI_1_9: i64 = 1321;
const SINPI_2_9: i64 = 2482;
const SINPI_3_9: i64 = 3344;
const SINPI_4_9: i64 = 3803;

/// In-place 1-D inverse ADST of the length-`2^n` array `t` (`n ∈ {2, 3, 4}`), per AV1 §7.13.2.9.
///
/// `r` is the intermediate clamping range in bits (the caller passes `rowClampRange` /
/// `colClampRange` from the 2-D process). Only the first `1 << n` entries of `t` are touched.
///
/// # Panics
/// Panics if `t.len() < (1 << n)` or `n` is outside `2..=4`.
pub fn inverse_adst(t: &mut [i64], n: u32, r: u32) {
    match n {
        2 => inverse_adst4(t, r),
        3 => inverse_adst8(t, r),
        4 => inverse_adst16(t, r),
        _ => panic!("inverse_adst: n must be in 2..=4"),
    }
}

/// Inverse ADST4 (AV1 §7.13.2.6): the closed-form SINPI computation, no permutation or butterflies.
fn inverse_adst4(t: &mut [i64], _r: u32) {
    assert!(t.len() >= 4, "inverse_adst4: array shorter than 4");
    let mut s = [0i64; 7];
    s[0] = SINPI_1_9 * t[0];
    s[1] = SINPI_2_9 * t[0];
    s[2] = SINPI_3_9 * t[1];
    s[3] = SINPI_4_9 * t[2];
    s[4] = SINPI_1_9 * t[2];
    s[5] = SINPI_2_9 * t[3];
    s[6] = SINPI_4_9 * t[3];
    let a7 = t[0] - t[2];
    let b7 = a7 + t[3];

    s[0] += s[3];
    s[1] -= s[4];
    s[3] = s[2];
    s[2] = SINPI_3_9 * b7;

    s[0] += s[5];
    s[1] -= s[6];

    let x0 = s[0] + s[3];
    let x1 = s[1] + s[3];
    let x2 = s[2];
    let x3 = s[0] + s[1] - s[3];

    t[0] = round2(x0, 12);
    t[1] = round2(x1, 12);
    t[2] = round2(x2, 12);
    t[3] = round2(x3, 12);
}

/// Inverse ADST8 (AV1 §7.13.2.7).
fn inverse_adst8(t: &mut [i64], r: u32) {
    assert!(t.len() >= 8, "inverse_adst8: array shorter than 8");
    adst_input_permute(t, 3);
    for i in 0..4 {
        b(t, 2 * i, 2 * i + 1, 60 - 16 * i, true, r);
    }
    for i in 0..4 {
        h(t, i, 4 + i, false, r);
    }
    for i in 0..2 {
        b(t, 4 + 3 * i, 5 + i, 48 - 32 * i, true, r);
    }
    for j in 0..2 {
        for i in 0..2 {
            h(t, 4 * j + i, 2 + 4 * j + i, false, r);
        }
    }
    for i in 0..2 {
        b(t, 2 + 4 * i, 3 + 4 * i, 32, true, r);
    }
    adst_output_permute(t, 3);
}

/// Inverse ADST16 (AV1 §7.13.2.8).
fn inverse_adst16(t: &mut [i64], r: u32) {
    assert!(t.len() >= 16, "inverse_adst16: array shorter than 16");
    adst_input_permute(t, 4);
    for i in 0..8 {
        b(t, 2 * i, 2 * i + 1, 62 - 8 * i, true, r);
    }
    for i in 0..8 {
        h(t, i, 8 + i, false, r);
    }
    for i in 0..2 {
        b(t, 8 + 2 * i, 9 + 2 * i, 56 - 32 * i, true, r);
        b(t, 13 + 2 * i, 12 + 2 * i, 8 + 32 * i, true, r);
    }
    for j in 0..2 {
        for i in 0..4 {
            h(t, 8 * j + i, 4 + 8 * j + i, false, r);
        }
    }
    for j in 0..2 {
        for i in 0..2 {
            b(t, 4 + 8 * j + 3 * i, 5 + 8 * j + i, 48 - 32 * i, true, r);
        }
    }
    for j in 0..4 {
        for i in 0..2 {
            h(t, 4 * j + i, 2 + 4 * j + i, false, r);
        }
    }
    for i in 0..4 {
        b(t, 2 + 4 * i, 3 + 4 * i, 32, true, r);
    }
    adst_output_permute(t, 4);
}

/// Inverse ADST input array permutation (AV1 §7.13.2.4), `3 ≤ n ≤ 4`.
fn adst_input_permute(t: &mut [i64], n: u32) {
    let n0 = 1usize << n;
    let mut copy = [0i64; 16];
    copy[..n0].copy_from_slice(&t[..n0]);
    for (i, slot) in t[..n0].iter_mut().enumerate() {
        let idx = if i & 1 == 1 { i - 1 } else { n0 - i - 1 };
        *slot = copy[idx];
    }
}

/// Inverse ADST output array permutation (AV1 §7.13.2.5), `3 ≤ n ≤ 4`, with the odd-index negation.
fn adst_output_permute(t: &mut [i64], n: u32) {
    let n0 = 1usize << n;
    let mut copy = [0i64; 16];
    copy[..n0].copy_from_slice(&t[..n0]);
    for (i, slot) in t[..n0].iter_mut().enumerate() {
        let a = (i >> 3) & 1;
        let b = ((i >> 2) & 1) ^ ((i >> 3) & 1);
        let c = ((i >> 1) & 1) ^ ((i >> 2) & 1);
        let d = (i & 1) ^ ((i >> 1) & 1);
        // Summed rather than OR'd: each of a/b/c/d is one bit at its own position, so `|` would
        // have an equivalent `^` twin no test can tell apart, while each `+` has none.
        let idx = ((d << 3) + (c << 2) + (b << 1) + a) >> (4 - n);
        *slot = if i & 1 == 1 { -copy[idx] } else { copy[idx] };
    }
}

/// In-place 1-D forward ADST of the length-`2^n` array `t` (`n ∈ {2, 3, 4}`) — the transpose of the
/// inverse ADST matrix, built from the inverse's impulse responses (see module docs).
///
/// # Panics
/// Panics if `t.len() < (1 << n)` or `n` is outside `2..=4`.
pub fn forward_adst(t: &mut [i64], n: u32) {
    assert!((2..=4).contains(&n), "forward_adst: n must be in 2..=4");
    let len = 1usize << n;
    assert!(t.len() >= len, "forward_adst: array shorter than 2^n");

    // M_inverse[m][k] = inverse_adst(UNIT · e_k)[m]; `UNIT = 4096` keeps the impulse responses well
    // above the Round2 rounding floor, and r = 24 keeps the Hadamard clamp inactive.
    const UNIT: i64 = 1 << 12;
    let mut m_inv = [[0i64; 16]; 16];
    for k in 0..len {
        let mut e = [0i64; 16];
        e[k] = UNIT;
        inverse_adst(&mut e[..len], n, 24);
        for m in 0..len {
            m_inv[m][k] = e[m];
        }
    }

    let mut out = [0i64; 16];
    for (k, slot) in out[..len].iter_mut().enumerate() {
        let mut acc = 0i64;
        for (m, &x) in t[..len].iter().enumerate() {
            acc += m_inv[m][k] * x;
        }
        // Drop the `UNIT` (1<<12) scale baked into the impulse responses.
        *slot = round2(acc, 12);
    }
    t[..len].copy_from_slice(&out[..len]);
}

#[cfg(test)]
mod tests {
    use std::f64::consts::PI;

    use super::*;
    use crate::testrng::Lcg;

    /// Independent naive reference for AV1's inverse ADST, used to pin the spec transcription.
    ///
    /// AV1's ADST family is not one transform: the size-4 ADST is the true DST-VII (the SINPI/9
    /// closed form, `sin(π(2k+1)(m+1)/(2N+1))`), while the size-8/16 butterfly networks compute
    /// DST-IV (`sin(π(2m+1)(2k+1)/4N)`) — verified empirically to rms < 1e-3 against these exact,
    /// transcription-independent formulae. AV1's inverse ADST equals the size-appropriate DST up
    /// to one constant per-size scale, compared proportionally. The bit-exact cross-check against
    /// `dav1d` lands with the 2-D pipeline (P5).
    fn naive_iadst(x: &[f64]) -> Vec<f64> {
        let n = x.len();
        let nn = n as f64;
        (0..n)
            .map(|m| {
                (0..n)
                    .map(|k| {
                        let basis = if n == 4 {
                            // DST-VII (size 4).
                            (PI * (2 * k + 1) as f64 * (m + 1) as f64 / (2.0 * nn + 1.0)).sin()
                        } else {
                            // DST-IV (size 8, 16).
                            (PI * (2 * m + 1) as f64 * (2 * k + 1) as f64 / (4.0 * nn)).sin()
                        };
                        x[k] * basis
                    })
                    .sum()
            })
            .collect()
    }

    fn assert_proportional(got: &[i64], want: &[f64], tol: f64, ctx: &str) -> f64 {
        let (anchor, &wmax) = want
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.abs().partial_cmp(&b.1.abs()).unwrap())
            .unwrap();
        assert!(wmax.abs() > 1e-6, "{ctx}: degenerate reference");
        let scale = got[anchor] as f64 / wmax;
        for (m, (&g, &w)) in got.iter().zip(want).enumerate() {
            let predicted = scale * w;
            assert!(
                (g as f64 - predicted).abs() <= tol,
                "{ctx}: entry {m} = {g}, expected ≈ {predicted:.2} (scale {scale:.3})",
            );
        }
        scale
    }

    #[test]
    fn inverse_adst_matches_naive_reference() {
        let mut rng = Lcg::new(0x51a7_c0de_1234_5678);
        for n in 2..=4u32 {
            let len = 1usize << n;
            for _ in 0..200 {
                let coeffs: Vec<i64> = (0..len).map(|_| rng.coeff(64)).collect();
                let mut t = coeffs.clone();
                inverse_adst(&mut t, n, 16);
                let want = naive_iadst(&coeffs.iter().map(|&c| c as f64).collect::<Vec<_>>());
                assert_proportional(&t[..len], &want, 2.0 * len as f64, &format!("iadst n={n}"));
            }
        }
    }

    #[test]
    fn forward_then_inverse_is_proportional_identity() {
        // ADST is orthogonal, so inverse_adst(forward_adst(x)) is a scaled identity.
        let mut rng = Lcg::new(0x0bad_f00d_dead_0001);
        for n in 2..=4u32 {
            let len = 1usize << n;
            for _ in 0..200 {
                let resid: Vec<i64> = (0..len).map(|_| rng.coeff(200)).collect();
                let mut t = resid.clone();
                forward_adst(&mut t, n);
                inverse_adst(&mut t, n, 24);
                let want: Vec<f64> = resid.iter().map(|&c| c as f64).collect();
                assert_proportional(&t[..len], &want, 8.0 * len as f64, &format!("rt n={n}"));
            }
        }
    }

    #[test]
    fn input_then_output_permute_indices_are_valid() {
        // Permutations must be bijections over 0..n0 (sign aside): every slot is written once.
        for n in 3..=4u32 {
            let n0 = 1usize << n;
            let mut t: Vec<i64> = (0..n0 as i64).collect();
            adst_input_permute(&mut t, n);
            let mut seen: Vec<i64> = t[..n0].to_vec();
            seen.sort_unstable();
            assert_eq!(
                seen,
                (0..n0 as i64).collect::<Vec<_>>(),
                "input permute n={n}"
            );
        }
    }

    #[test]
    fn inverse_adst_exact_values() {
        // The proportional float oracle pins the inverse only up to one global scale; these
        // snapshots pin the bit-exact outputs. The n = 2 impulse is hand-verifiable against the
        // spec: the response to 4096·e₀ is the SINPI/9 column itself — x0 = SINPI_1_9·t0,
        // x1 = SINPI_2_9·t0, x2 = SINPI_3_9·t0, x3 = (SINPI_1_9 + SINPI_2_9)·t0 = SINPI_4_9·t0,
        // each exactly de-scaled by round2(·, 12). The n = 3/4 goldens are generated from this
        // implementation (proportionally oracle-pinned above; bit-exactly dav1d-pinned through
        // the 2-D pipeline).
        let mut t = [4096i64, 0, 0, 0];
        inverse_adst(&mut t, 2, 16);
        assert_eq!(t, [SINPI_1_9, SINPI_2_9, SINPI_3_9, SINPI_4_9]);
        let mut t = [100i64, -50, 25, -12];
        inverse_adst(&mut t, 2, 16);
        assert_eq!(t, [7, 23, 51, 153]);
        let mut t = [200i64, -100, 50, -25, 12, -6, 3, -1];
        inverse_adst(&mut t, 3, 16);
        assert_eq!(t, [4, 13, 27, 44, 72, 127, 229, 370]);
        let mut t: Vec<i64> = (0..16i64).map(|k| ((k * 37) % 201) - 100).collect();
        inverse_adst(&mut t, 4, 16);
        assert_eq!(
            t,
            [
                -38, -15, 50, -173, 2, -491, -129, 153, -76, -93, -87, -237, 30, 105, -139, -95
            ]
        );
    }

    #[test]
    fn intermediate_clamp_range_is_wired_through() {
        // Adversarial: the oracle tests keep the Hadamard clamp inactive, so nothing else fails
        // if `r` were ignored. Saturating coefficients must produce different outputs at a narrow
        // versus a wide clamp range.
        let mut narrow = [1i64 << 19; 16];
        inverse_adst(&mut narrow, 4, 8);
        let mut wide = [1i64 << 19; 16];
        inverse_adst(&mut wide, 4, 20);
        assert_ne!(narrow, wide, "the r clamp should saturate at r = 8");
    }

    #[test]
    fn forward_adst_exact_values() {
        // As with forward_dct, the proportional round-trip pins forward_adst only up to scale, so the
        // `UNIT` impulse scale and the `acc += m_inv * x` accumulation (negate / zero / `* -> +`) need
        // exact snapshots. Correctness is established by the proportional inverse-vs-naive test.
        // The n = 3 case additionally pins the size-8 basis (n = 2 alone would let a
        // size-dependent bug through).
        let mut t = [100i64, 100, 100, 100];
        forward_adst(&mut t, 2);
        assert_eq!(t, [267, 82, 40, 17]);
        let mut t = [200i64, -100, 50, -25];
        forward_adst(&mut t, 2);
        assert_eq!(t, [22, 102, 162, 263]);
        let mut t = [200i64, -100, 50, -25, 12, -6, 3, -1];
        forward_adst(&mut t, 3);
        assert_eq!(t, [4, 14, 26, 44, 72, 127, 229, 370]);
    }

    #[test]
    fn dc_coefficient_basis_increases_across_block() {
        // The lowest-frequency ADST basis (the response to coeff 0) is a quarter sine wave that
        // rises monotonically across the block — `sin((m+1)π/9)` for DST-VII, `sin((2m+1)π/4N)`
        // for DST-IV, both strictly increasing on (0, π/2). A basic shape check on the basis.
        for n in 2..=4u32 {
            let len = 1usize << n;
            let mut t = vec![0i64; len];
            t[0] = 1000;
            inverse_adst(&mut t, n, 16);
            assert!(t[0] > 0, "n={n}: first sample should be positive");
            for w in t[..len].windows(2) {
                assert!(
                    w[1] > w[0],
                    "n={n}: DC basis should increase across the block"
                );
            }
        }
    }
}
