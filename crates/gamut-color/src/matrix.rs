//! Derivation of RGB→XYZ and OKLab `M1` matrices from CIE 1931 chromaticities,
//! with Bradford chromatic adaptation.
//!
//! These builders let the per-gamut `M1` tables in [`crate::oklab`] be *computed
//! from first principles* rather than trusted as hand-copied literals: the
//! `derived_matrices_match_literals` test checks
//! `derive_m1(gamut) ≈ oklab::Gamut::m1_matrix()` to `1e-7`, so a typo in either
//! place is caught. The factorization follows Ottosson:
//! `M1[gamut] = M_LMS · M_XYZ[gamut]`, where `M_LMS = M1[sRGB] · M_XYZ[sRGB]⁻¹`
//! is implied by Ottosson's published sRGB matrix.
//!
//! Chromaticities and the Bradford matrix are documented in
//! `references/color/README.md`.

use crate::linalg::{mat_inv_3x3, mat_mul3, matvec3};
use crate::oklab::{Gamut, M1_SRGB};

/// CIE standard illuminant **D65** chromaticity (x, y).
pub const D65: [f64; 2] = [0.3127, 0.3290];
/// CIE standard illuminant **D50** chromaticity (x, y).
pub const D50: [f64; 2] = [0.3457, 0.3585];

/// Bradford chromatic-adaptation cone-response matrix (CIE).
pub const BRADFORD: [[f64; 3]; 3] = [
    [0.8951, 0.2664, -0.1614],
    [-0.7502, 1.7135, 0.0367],
    [0.0389, -0.0685, 1.0296],
];

// Primaries as [R, G, B] chromaticity pairs (see references/color/README.md).
pub(crate) const SRGB_PRIMARIES: [[f64; 2]; 3] =
    [[0.6400, 0.3300], [0.3000, 0.6000], [0.1500, 0.0600]];
pub(crate) const DISPLAY_P3_PRIMARIES: [[f64; 2]; 3] =
    [[0.6800, 0.3200], [0.2650, 0.6900], [0.1500, 0.0600]];
pub(crate) const ADOBE_RGB_PRIMARIES: [[f64; 2]; 3] =
    [[0.6400, 0.3300], [0.2100, 0.7100], [0.1500, 0.0600]];
pub(crate) const BT2020_PRIMARIES: [[f64; 2]; 3] =
    [[0.7080, 0.2920], [0.1700, 0.7970], [0.1310, 0.0460]];
pub(crate) const PROPHOTO_PRIMARIES: [[f64; 2]; 3] = [
    [0.734699, 0.265301],
    [0.159597, 0.840403],
    [0.036598, 0.000105],
];

// The two H.273 primary sets with no colour-science `Gamut` counterpart. Both are D65, and they
// are *not* interchangeable despite both being called "BT.601": code point 5 is the 625-line
// (PAL/SECAM) set from EBU Tech. 3213-E, code point 6 the 525-line (NTSC SMPTE 170M) set.
// ITU-T H.273 Table 2; see references/color/.
pub(crate) const BT601_625_PRIMARIES: [[f64; 2]; 3] =
    [[0.6400, 0.3300], [0.2900, 0.6000], [0.1500, 0.0600]];
pub(crate) const BT601_525_PRIMARIES: [[f64; 2]; 3] =
    [[0.6300, 0.3400], [0.3100, 0.5950], [0.1550, 0.0700]];

/// Convert a CIE 1931 chromaticity `(x, y)` to tristimulus `XYZ` at unit `Y`.
/// Precondition: `y != 0` (a zero `y` yields non-finite components).
#[must_use]
pub fn xy_to_xyz(x: f64, y: f64) -> [f64; 3] {
    [x / y, 1.0, (1.0 - x - y) / y]
}

/// Build the linear-RGB → CIE XYZ matrix from `primaries` (`[R, G, B]` xy pairs)
/// and the `white` chromaticity. Returns `None` if the primaries are degenerate
/// (singular). Per Bruce Lindbloom's RGB/XYZ construction.
#[must_use]
pub fn rgb_to_xyz_matrix(primaries: &[[f64; 2]; 3], white: [f64; 2]) -> Option<[[f64; 3]; 3]> {
    let r = xy_to_xyz(primaries[0][0], primaries[0][1]);
    let g = xy_to_xyz(primaries[1][0], primaries[1][1]);
    let b = xy_to_xyz(primaries[2][0], primaries[2][1]);
    let m = [[r[0], g[0], b[0]], [r[1], g[1], b[1]], [r[2], g[2], b[2]]];
    let w = xy_to_xyz(white[0], white[1]);
    let s = matvec3(&mat_inv_3x3(&m)?, w);
    // Scale each column j of `m` by s[j].
    Some([
        [m[0][0] * s[0], m[0][1] * s[1], m[0][2] * s[2]],
        [m[1][0] * s[0], m[1][1] * s[1], m[1][2] * s[2]],
        [m[2][0] * s[0], m[2][1] * s[1], m[2][2] * s[2]],
    ])
}

/// Build the Bradford chromatic-adaptation matrix from `src_white` to
/// `dst_white` (both chromaticity xy). Returns `None` only if the Bradford
/// matrix is non-invertible (it always is, so this never happens in practice).
#[must_use]
pub fn bradford_adapt(src_white: [f64; 2], dst_white: [f64; 2]) -> Option<[[f64; 3]; 3]> {
    let src = xy_to_xyz(src_white[0], src_white[1]);
    let dst = xy_to_xyz(dst_white[0], dst_white[1]);
    let inv = mat_inv_3x3(&BRADFORD)?;
    let src_lms = matvec3(&BRADFORD, src);
    let dst_lms = matvec3(&BRADFORD, dst);
    let diag = [
        [dst_lms[0] / src_lms[0], 0.0, 0.0],
        [0.0, dst_lms[1] / src_lms[1], 0.0],
        [0.0, 0.0, dst_lms[2] / src_lms[2]],
    ];
    Some(mat_mul3(&inv, &mat_mul3(&diag, &BRADFORD)))
}

/// Derive the linear-RGB → LMS (`M1`) matrix for `gamut` from first principles,
/// adapting non-D65 white points to D65 with Bradford. Returns `None` only for
/// degenerate primaries (never for a [`Gamut`] variant).
///
/// Use this to regenerate the literal `M1_*` tables in [`crate::oklab`]; the test
/// suite checks the two agree.
#[must_use]
pub fn derive_m1(gamut: Gamut) -> Option<[[f64; 3]; 3]> {
    // M_LMS = M1[sRGB] · M_XYZ[sRGB]⁻¹  (implied by Ottosson's published sRGB M1).
    let m_xyz_srgb = rgb_to_xyz_matrix(&SRGB_PRIMARIES, D65)?;
    let m_lms = mat_mul3(&M1_SRGB, &mat_inv_3x3(&m_xyz_srgb)?);

    let (primaries, white) = gamut.chromaticities();
    let mut m_xyz = rgb_to_xyz_matrix(&primaries, white)?;
    if white != D65 {
        let m_adapt = bradford_adapt(white, D65)?;
        m_xyz = mat_mul3(&m_adapt, &m_xyz);
    }
    Some(mat_mul3(&m_lms, &m_xyz))
}

#[cfg(test)]
mod tests {
    use super::*;

    // `xy_to_xyz` needs no direct test: it feeds both Lindbloom oracles below through every
    // primary and white point, so any mutated term shifts those matrices far past their 5e-4 gate.

    /// External oracle: Bruce Lindbloom's published linear-sRGB → XYZ (D65) matrix
    /// (<http://www.brucelindbloom.com/Eqn_RGB_XYZ_Matrix.html>) is an independent computation of
    /// the same Lindbloom construction. Our matrix must reproduce it (to within the crate's 4-place
    /// chromaticity rounding vs Lindbloom's 5-place white point). This subsumes a white-point check:
    /// `M·(1,1,1)` equals the D65 tristimulus only if the columns are scaled correctly.
    #[test]
    fn srgb_rgb_to_xyz_matches_lindbloom() {
        const LINDBLOOM_SRGB_D65: [[f64; 3]; 3] = [
            [0.4124564, 0.3575761, 0.1804375],
            [0.2126729, 0.7151522, 0.0721750],
            [0.0193339, 0.1191920, 0.9503041],
        ];
        let m = rgb_to_xyz_matrix(&SRGB_PRIMARIES, D65).expect("non-degenerate");
        let mut err = 0.0_f64;
        for (row, lrow) in m.iter().zip(LINDBLOOM_SRGB_D65.iter()) {
            for (&v, &l) in row.iter().zip(lrow.iter()) {
                err = err.max((v - l).abs());
            }
        }
        assert!(err < 5e-4, "sRGB→XYZ vs Lindbloom max err = {err}");
    }

    /// External oracle: Lindbloom's published Bradford D65→D50 adaptation matrix
    /// (<http://www.brucelindbloom.com/Eqn_ChromAdapt.html>) — an independent computation of the
    /// same CAT. `bradford_adapt(D65, D50)` must reproduce it (to within the crate's 4-place
    /// chromaticity rounding); a wrong cone matrix or a flipped src/dst direction is off by >0.05.
    #[test]
    fn bradford_d65_to_d50_matches_lindbloom() {
        const LINDBLOOM_D65_TO_D50: [[f64; 3]; 3] = [
            [1.0478112, 0.0228866, -0.0501270],
            [0.0295424, 0.9904844, -0.0170491],
            [-0.0092345, 0.0150436, 0.7521316],
        ];
        let m = bradford_adapt(D65, D50).expect("invertible");
        let mut err = 0.0_f64;
        for (row, lrow) in m.iter().zip(LINDBLOOM_D65_TO_D50.iter()) {
            for (&v, &l) in row.iter().zip(lrow.iter()) {
                err = err.max((v - l).abs());
            }
        }
        assert!(err < 5e-4, "Bradford D65→D50 vs Lindbloom max err = {err}");
    }

    /// The whole point of this module: the literal `M1_*` tables in `oklab` must
    /// equal the from-first-principles derivation. Catches transcription typos in
    /// either the matrices or the chromaticities.
    #[test]
    fn derived_matrices_match_literals() {
        for g in [
            Gamut::Srgb,
            Gamut::DisplayP3,
            Gamut::AdobeRgb,
            Gamut::Bt2020,
            Gamut::ProPhotoRgb,
        ] {
            let derived = derive_m1(g).expect("derivable");
            let literal = g.m1_matrix();
            let mut err = 0.0_f64;
            for (dr, lr) in derived.iter().zip(literal.iter()) {
                for (&d, &l) in dr.iter().zip(lr.iter()) {
                    err = err.max((d - l).abs());
                }
            }
            assert!(err < 1e-7, "M1[{g:?}] derived vs literal max err = {err}");
        }
    }
}
