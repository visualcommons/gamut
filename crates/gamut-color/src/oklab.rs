//! OKLab transforms and the per-gamut linear-RGB → LMS (`M1`) matrices.
//!
//! OKLab (Björn Ottosson) is reached by `linear_rgb → M1 → cube-root → M2`. The
//! `M2` / `M2⁻¹` cone-response constants and `M1[sRGB]` / `M1⁻¹[sRGB]` are
//! Ottosson's published values; the per-gamut `M1` tables fold each gamut's
//! primaries (and, for ProPhoto, Bradford D50→D65) into the LMS projection — see
//! [`crate::matrix`] for the derivation that verifies them, and
//! `references/color/README.md` for provenance.
//!
//! Tier-1 determinism: the cube root is `std` `f64::cbrt` (signed, correctly
//! rounded), not chromahash's `cbrt_halley`, so results match chromahash's
//! vectors within a small tolerance rather than bit-for-bit.

use crate::linalg::matvec3;
use crate::primaries::RgbGamut;

/// `M2`: cube-root LMS → OKLab `[L, a, b]` (Ottosson).
pub const M2: [[f64; 3]; 3] = [
    [0.2104542553, 0.7936177850, -0.0040720468],
    [1.9779984951, -2.4285922050, 0.4505937099],
    [0.0259040371, 0.7827717662, -0.8086757660],
];

/// `M2⁻¹`: OKLab `[L, a, b]` → cube-root LMS (Ottosson).
pub const M2_INV: [[f64; 3]; 3] = [
    [1.0000000000, 0.3963377774, 0.2158037573],
    [1.0000000000, -0.1055613458, -0.0638541728],
    [1.0000000000, -0.0894841775, -1.2914855480],
];

/// `M1[sRGB]`: linear sRGB → LMS (Ottosson published).
pub const M1_SRGB: [[f64; 3]; 3] = [
    [0.4122214708, 0.5363325363, 0.0514459929],
    [0.2119034982, 0.6806995451, 0.1073969566],
    [0.0883024619, 0.2817188376, 0.6299787005],
];

/// `M1[Display P3]`: linear Display P3 → LMS.
pub const M1_DISPLAY_P3: [[f64; 3]; 3] = [
    [0.4813798544, 0.4621183697, 0.0565017758],
    [0.2288319449, 0.6532168128, 0.1179512422],
    [0.0839457557, 0.2241652689, 0.6918889754],
];

/// `M1[Adobe RGB]`: linear Adobe RGB → LMS.
pub const M1_ADOBE_RGB: [[f64; 3]; 3] = [
    [0.5764322615, 0.3699132211, 0.0536545174],
    [0.2963164739, 0.5916761266, 0.1120073994],
    [0.1234782548, 0.2194986958, 0.6570230494],
];

/// `M1[BT.2020]`: linear BT.2020 → LMS.
pub const M1_BT2020: [[f64; 3]; 3] = [
    [0.6167557872, 0.3601983994, 0.0230458134],
    [0.2651330640, 0.6358393641, 0.0990275718],
    [0.1001026342, 0.2039065194, 0.6959908464],
];

/// `M1[ProPhoto RGB]`: linear ProPhoto RGB → LMS (includes Bradford D50→D65).
pub const M1_PROPHOTO_RGB: [[f64; 3]; 3] = [
    [0.7154484635, 0.3527915480, -0.0682400115],
    [0.2744116551, 0.6677976408, 0.0577907040],
    [0.1097844385, 0.1861982875, 0.7040172740],
];

/// `M1⁻¹[sRGB]`: LMS → linear sRGB (Ottosson published).
pub const M1_INV_SRGB: [[f64; 3]; 3] = [
    [4.0767416621, -3.3077115913, 0.2309699292],
    [-1.2684380046, 2.6097574011, -0.3413193965],
    [-0.0041960863, -0.7034186147, 1.7076147010],
];

// `RgbGamut` lives in `crate::primaries`; its OKLab projection is defined here, beside the
// matrices it selects.
impl RgbGamut {
    /// The OKLab linear-RGB → LMS (`M1`) matrix for this gamut.
    #[must_use]
    pub fn m1_matrix(self) -> &'static [[f64; 3]; 3] {
        match self {
            RgbGamut::Srgb => &M1_SRGB,
            RgbGamut::DisplayP3 => &M1_DISPLAY_P3,
            RgbGamut::AdobeRgb => &M1_ADOBE_RGB,
            RgbGamut::Bt2020 => &M1_BT2020,
            RgbGamut::ProPhotoRgb => &M1_PROPHOTO_RGB,
        }
    }
}

/// Convert linear RGB to OKLab using the source `gamut`'s `M1` matrix.
///
/// # Examples
///
/// ```
/// use gamut_color::RgbGamut;
/// use gamut_color::oklab::linear_rgb_to_oklab;
/// // White maps to L≈1 with near-zero chroma.
/// let lab = linear_rgb_to_oklab([1.0, 1.0, 1.0], RgbGamut::Srgb);
/// assert!((lab[0] - 1.0).abs() < 1e-6);
/// assert!(lab[1].abs() < 1e-6 && lab[2].abs() < 1e-6);
/// ```
#[must_use]
pub fn linear_rgb_to_oklab(rgb: [f64; 3], gamut: RgbGamut) -> [f64; 3] {
    let lms = matvec3(gamut.m1_matrix(), rgb);
    // `f64::cbrt` is signed, so slightly-negative LMS (out-of-gamut) is handled.
    let lms_cbrt = [lms[0].cbrt(), lms[1].cbrt(), lms[2].cbrt()];
    matvec3(&M2, lms_cbrt)
}

/// Convert OKLab to **linear sRGB** (always sRGB, via `M1⁻¹[sRGB]`).
///
/// Intentionally sRGB-only: this is the inverse [`gamut_map`](crate::gamut_map) uses to test sRGB
/// gamut membership, and sRGB is the encode target. The forward [`linear_rgb_to_oklab`] takes a
/// [`RgbGamut`] because source pixels arrive in many gamuts; the reverse only needs the one target, so
/// per-gamut inverses are deferred until a use case needs them.
#[must_use]
pub fn oklab_to_linear_srgb(lab: [f64; 3]) -> [f64; 3] {
    let c = matvec3(&M2_INV, lab);
    let lms = [c[0] * c[0] * c[0], c[1] * c[1] * c[1], c[2] * c[2] * c[2]];
    matvec3(&M1_INV_SRGB, lms)
}

#[cfg(test)]
mod tests {
    use super::*;

    // The matrix literals need no relationship tests of their own: `derived_matrices_match_literals`
    // (matrix.rs) re-derives every `M1_*` table element-wise to 1e-7, and the golden vectors below
    // run dense values through `M2` / `M2_INV` / `M1_INV_SRGB` (forward and round-trip) at 1e-9.

    /// Golden vectors transcribed from chromahash `spec/test-vectors/unit-color.json`
    /// (MIT OR Apache-2.0). Tier-1 `std` math agrees with chromahash's `cbrt_halley`
    /// outputs to well within this tolerance (cross-implementation check).
    #[test]
    fn matches_chromahash_color_vectors() {
        struct Case {
            linear_rgb: [f64; 3],
            gamut: RgbGamut,
            oklab: [f64; 3],
            roundtrip_srgb: [f64; 3],
        }
        let cases = [
            Case {
                linear_rgb: [1.0, 1.0, 1.0],
                gamut: RgbGamut::Srgb,
                oklab: [
                    0.9999999934735462,
                    0.00000000008095285553011422,
                    0.00000003727390762708893,
                ],
                roundtrip_srgb: [1.000000069533121, 0.9999999802873053, 0.9999997387154016],
            },
            Case {
                linear_rgb: [0.0, 0.0, 0.0],
                gamut: RgbGamut::Srgb,
                oklab: [0.0, 0.0, 0.0],
                roundtrip_srgb: [0.0, 0.0, 0.0],
            },
            Case {
                linear_rgb: [1.0, 0.0, 0.0],
                gamut: RgbGamut::Srgb,
                oklab: [0.6279553606145517, 0.224863061065974, 0.12584629853073515],
                roundtrip_srgb: [
                    1.000000000403837,
                    -0.000000004965820570718149,
                    -0.00000003046650384752603,
                ],
            },
            Case {
                linear_rgb: [0.0, 1.0, 0.0],
                gamut: RgbGamut::Srgb,
                oklab: [0.8664396115356695, -0.2338875741879084, 0.17949847989672996],
                roundtrip_srgb: [
                    0.0000000727249208493097,
                    0.9999999708281623,
                    -0.00000010005049988492942,
                ],
            },
            Case {
                linear_rgb: [0.0, 0.0, 1.0],
                gamut: RgbGamut::Srgb,
                oklab: [
                    0.4520137183853429,
                    -0.03245698416876397,
                    -0.3115281476783752,
                ],
                roundtrip_srgb: [
                    0.00000000170972649926604,
                    0.000000007498798221261538,
                    0.9999999204714658,
                ],
            },
            Case {
                linear_rgb: [0.5, 0.5, 0.5],
                gamut: RgbGamut::Srgb,
                oklab: [
                    0.7937005208040498,
                    0.00000000006425243670449277,
                    0.00000002958431999378064,
                ],
                roundtrip_srgb: [0.500000034766561, 0.49999999014365226, 0.4999998693577007],
            },
            Case {
                linear_rgb: [1.0, 0.0, 0.0],
                gamut: RgbGamut::DisplayP3,
                oklab: [0.6485740719370451, 0.262041750679711, 0.1450019283152177],
                roundtrip_srgb: [
                    1.2249401729722074,
                    -0.04205695956623478,
                    -0.01963758459561718,
                ],
            },
            Case {
                linear_rgb: [1.0, 0.0, 0.0],
                gamut: RgbGamut::AdobeRgb,
                oklab: [0.7022115941248748, 0.251453301384477, 0.14072772598002087],
                roundtrip_srgb: [
                    1.3983557445296744,
                    -0.000000006902159496724458,
                    -0.0000000426282422139046,
                ],
            },
        ];
        for c in &cases {
            let lab = linear_rgb_to_oklab(c.linear_rgb, c.gamut);
            for (i, (&got, &want)) in lab.iter().zip(c.oklab.iter()).enumerate() {
                assert!(
                    (got - want).abs() < 1e-9,
                    "oklab[{i}] for {:?} {:?}: got {got}, want {want}",
                    c.linear_rgb,
                    c.gamut,
                );
            }
            let rt = oklab_to_linear_srgb(lab);
            for (i, (&got, &want)) in rt.iter().zip(c.roundtrip_srgb.iter()).enumerate() {
                assert!(
                    (got - want).abs() < 1e-9,
                    "roundtrip[{i}] for {:?} {:?}: got {got}, want {want}",
                    c.linear_rgb,
                    c.gamut,
                );
            }
        }
    }
}
