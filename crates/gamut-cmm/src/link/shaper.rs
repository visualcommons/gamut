//! Matrix/TRC ("shaper") pipeline builders — the RGB and gray profile forms of ICC.1:2022
//! §8.3.4/§8.4.3, shaped after lcms2's `BuildRGBInputMatrixShaper` /
//! `BuildRGBOutputMatrixShaper` / `BuildGrayInputMatrixPipeline` / `BuildGrayOutputPipeline`
//! (`cmsio1.c`).
//!
//! One deliberate difference from the lcms2 transcription: lcms2 pipelines run in **encoded**
//! XYZ (`[0, 1]`, 1.15 fixed-point range), so its matrices carry the `InpAdj`/`OutpAdj`
//! factors (`1/MAX_ENCODEABLE_XYZ` and its reciprocal). This crate's PCS seams are **decoded**
//! colorimetry (XYZ with D50 `Y = 1.0`), so the factors do not apply — an lcms2 transform
//! bracketed by `TYPE_XYZ_DBL` formatters produces the same decoded values, which is what the
//! differential tests compare.

use gamut_color::lab::D50_XYZ;
use gamut_color::linalg::mat_inv_3x3;
use gamut_icc::{ColorSpace, CurveOrParametric, IccProfile, KnownTag, Signature, TagData};

use crate::curve::ToneCurve;
use crate::error::{CmmError, Result};
use crate::pipeline::{Pipeline, Stage};

/// Reads one colorant tag as a decoded XYZ triple (the first — per §10.31 only — element of
/// its `XYZType`).
fn read_colorant(profile: &IccProfile, tag: KnownTag) -> Result<[f64; 3]> {
    let sig = Signature::from(tag);
    match profile.get(tag) {
        None => Err(CmmError::MissingTag(sig)),
        Some(TagData::Xyz(values)) => values
            .first()
            .map(|xyz| xyz.to_f64())
            .ok_or(CmmError::BadTagType(sig)),
        Some(_) => Err(CmmError::BadTagType(sig)),
    }
}

/// Reads one TRC tag (`curveType` or `parametricCurveType`) as a [`ToneCurve`].
fn read_trc(profile: &IccProfile, tag: KnownTag) -> Result<ToneCurve> {
    let sig = Signature::from(tag);
    let curve = match profile.get(tag) {
        None => return Err(CmmError::MissingTag(sig)),
        Some(TagData::Curve(curve)) => CurveOrParametric::Curve(curve.clone()),
        Some(TagData::ParametricCurve(curve)) => CurveOrParametric::Parametric(curve.clone()),
        Some(_) => return Err(CmmError::BadTagType(sig)),
    };
    ToneCurve::new(&curve)
}

/// Assembles the linear-RGB → PCSXYZ colorant matrix: **one column per primary** (`m[i][j]` is
/// colorant `j`'s component `i`, so `m · [r, g, b]ᵀ` sums the scaled colorant vectors) — the
/// layout of lcms2's `ReadICCMatrixRGB2XYZ`. The colorants are used exactly as tagged: the
/// spec requires them already D50-adapted, and the `chad` tag is never consulted here (see
/// the module docs of [`super`]).
fn colorant_matrix(profile: &IccProfile) -> Result<[[f64; 3]; 3]> {
    let r = read_colorant(profile, KnownTag::RedColorant)?;
    let g = read_colorant(profile, KnownTag::GreenColorant)?;
    let b = read_colorant(profile, KnownTag::BlueColorant)?;
    Ok([[r[0], g[0], b[0]], [r[1], g[1], b[1]], [r[2], g[2], b[2]]])
}

/// Reads the three RGB TRC tags, in channel order.
fn rgb_trcs(profile: &IccProfile) -> Result<Vec<ToneCurve>> {
    Ok(vec![
        read_trc(profile, KnownTag::RedTrc)?,
        read_trc(profile, KnownTag::GreenTrc)?,
        read_trc(profile, KnownTag::BlueTrc)?,
    ])
}

/// Inverts the colorant matrix, mapping every failure to [`CmmError::SingularMatrix`].
///
/// The conditioning threshold is deliberately `mat_inv_3x3`'s own: exact zero (or non-finite)
/// determinant, no epsilon — a nearly-collinear but numerically invertible colorant set still
/// inverts (as it does in lcms2, whose `MATRIX_DET_TOLERANCE` guards only translation
/// offsets, not this inverse). The extra finiteness sweep catches the pathological case of a
/// denormal determinant whose reciprocal overflows the cofactors to infinity.
fn invert_colorants(m: &[[f64; 3]; 3]) -> Result<[[f64; 3]; 3]> {
    let inverse = mat_inv_3x3(m).ok_or(CmmError::SingularMatrix)?;
    if inverse.iter().flatten().any(|v| !v.is_finite()) {
        return Err(CmmError::SingularMatrix);
    }
    Ok(inverse)
}

/// RGB shaper, device → PCS: `Curves(TRCs) → Matrix(colorants)` (lcms2's
/// `BuildRGBInputMatrixShaper`, without the encoded-domain `InpAdj` factor — module docs),
/// plus a trailing [`Stage::XyzToLab`] when the PCS is Lab (lcms2 appends
/// `_cmsStageAllocXYZ2Lab` there — the colorant matrix always lands in XYZ, so a Lab PCS
/// needs the colorimetric bridge to end at the profile's decoded PCS).
pub(super) fn rgb_device_to_pcs(profile: &IccProfile) -> Result<Pipeline> {
    let m = colorant_matrix(profile)?;
    let trcs = rgb_trcs(profile)?;
    let mut stages = vec![
        Stage::Curves(trcs),
        Stage::Matrix {
            m,
            offset: [0.0; 3],
        },
    ];
    if profile.header.pcs == ColorSpace::Lab {
        stages.push(Stage::XyzToLab);
    }
    Pipeline::new(3, 3, stages)
}

/// RGB shaper, PCS → device: `Matrix(colorants⁻¹) → Curves(inverted TRCs)` (lcms2's
/// `BuildRGBOutputMatrixShaper` without `OutpAdj`; the TRC inverses are analytic where the
/// parameterization permits, sharper than lcms2's 4096-entry reversal tables), with a
/// leading [`Stage::LabToXyz`] when the PCS is Lab (lcms2 prepends `_cmsStageAllocLab2XYZ`).
pub(super) fn rgb_pcs_to_device(profile: &IccProfile) -> Result<Pipeline> {
    let inverse = invert_colorants(&colorant_matrix(profile)?)?;
    let inverted = rgb_trcs(profile)?
        .iter()
        .map(ToneCurve::inverse)
        .collect::<Result<Vec<_>>>()?;
    let mut stages = Vec::new();
    if profile.header.pcs == ColorSpace::Lab {
        stages.push(Stage::LabToXyz);
    }
    stages.push(Stage::Matrix {
        m: inverse,
        offset: [0.0; 3],
    });
    stages.push(Stage::Curves(inverted));
    Pipeline::new(3, 3, stages)
}

/// Gray shaper, device → PCS.
///
/// XYZ PCS: `out = kTRC(g) · D50` — `Curves([kTRC])` then a 3×1 [`Stage::MatrixN`] whose
/// column is the D50 white (lcms2's `GrayInputMatrix` sans `InpAdj`). The D50 used is
/// [`gamut_color::lab::D50_XYZ`], the s15Fixed16-encoded PCS illuminant ICC.1:2022 §7.2.16
/// mandates; lcms2's `cmsD50X/Z` constants are the truncated `0.9642`/`0.8249`, ≤ 6e-6 away
/// (the differential bound accounts for it).
///
/// Lab PCS: `out = [100 · kTRC(g), 0, 0]` — decoded `L*` carries the curve, `a* = b* = 0`
/// (lcms2 builds `{1,1,1}` replication into encoded Lab with constant-0.5 `a`/`b` curves;
/// decoded, that is exactly this matrix).
pub(super) fn gray_device_to_pcs(profile: &IccProfile) -> Result<Pipeline> {
    let ktrc = read_trc(profile, KnownTag::GrayTrc)?;
    let column: [f64; 3] = if profile.header.pcs == ColorSpace::Lab {
        [100.0, 0.0, 0.0]
    } else {
        D50_XYZ
    };
    Pipeline::new(
        1,
        3,
        vec![
            Stage::Curves(vec![ktrc]),
            Stage::MatrixN {
                rows: 3,
                cols: 1,
                m: column.to_vec(),
                offset: vec![0.0; 3],
            },
        ],
    )
}

/// Gray shaper, PCS → device.
///
/// XYZ PCS: pick `Y` (a 1×3 `[0, 1, 0]` [`Stage::MatrixN`] — decoded `Y / D50_Y` with
/// `D50_Y = 1`, lcms2's `PickYMatrix` sans `OutpAdj`), then `Curves([kTRC⁻¹])`.
///
/// Lab PCS: pick `L* / 100` (a 1×3 `[1/100, 0, 0]` matrix — lcms2's `PickLstarMatrix` with
/// the decoded-Lab scaling folded in), then `Curves([kTRC⁻¹])`.
pub(super) fn gray_pcs_to_device(profile: &IccProfile) -> Result<Pipeline> {
    let inverse = read_trc(profile, KnownTag::GrayTrc)?.inverse()?;
    let row: [f64; 3] = if profile.header.pcs == ColorSpace::Lab {
        [1.0 / 100.0, 0.0, 0.0]
    } else {
        [0.0, 1.0, 0.0]
    };
    Pipeline::new(
        3,
        1,
        vec![
            Stage::MatrixN {
                rows: 1,
                cols: 3,
                m: row.to_vec(),
                offset: vec![0.0],
            },
            Stage::Curves(vec![inverse]),
        ],
    )
}

#[cfg(test)]
mod tests {
    use gamut_icc::{Curve, DeviceClass, ProfileHeader, U8Fixed8, XyzNumber};

    use super::*;
    use crate::transform::Transform as _;

    /// `u8Fixed8` 2.19921875 (`0x0233` = 563/256) — exactly representable, so curve pins are
    /// exact.
    const GAMMA: U8Fixed8 = U8Fixed8(0x0233);

    fn xyz_tag(v: [f64; 3]) -> TagData {
        TagData::Xyz(vec![XyzNumber::from_f64(v)])
    }

    fn gamma_tag() -> TagData {
        TagData::Curve(Curve::Gamma(GAMMA))
    }

    /// A hand-built RGB shaper over explicit colorant columns and per-channel TRC tags.
    fn rgb_profile(r: [f64; 3], g: [f64; 3], b: [f64; 3], trcs: [TagData; 3]) -> IccProfile {
        let [r_trc, g_trc, b_trc] = trcs;
        IccProfile {
            header: ProfileHeader::new(DeviceClass::Display, ColorSpace::Rgb),
            tags: vec![
                (Signature(*b"rXYZ"), xyz_tag(r)),
                (Signature(*b"gXYZ"), xyz_tag(g)),
                (Signature(*b"bXYZ"), xyz_tag(b)),
                (Signature(*b"rTRC"), r_trc),
                (Signature(*b"gTRC"), g_trc),
                (Signature(*b"bTRC"), b_trc),
            ],
        }
    }

    /// A hand-built gray shaper with the given PCS.
    fn gray_profile(pcs: ColorSpace, ktrc: TagData) -> IccProfile {
        let mut header = ProfileHeader::new(DeviceClass::Display, ColorSpace::Gray);
        header.pcs = pcs;
        IccProfile {
            header,
            tags: vec![(Signature(*b"kTRC"), ktrc)],
        }
    }

    /// Distinct, s15Fixed16-exact colorant columns: every entry is a multiple of 2⁻¹⁶, so the
    /// tag round-trip and the matrix pins below are exact.
    const R_COL: [f64; 3] = [0.5, 0.25, 0.0625];
    const G_COL: [f64; 3] = [0.375, 0.625, 0.125];
    const B_COL: [f64; 3] = [0.089_996_337_890_625, 0.125, 0.637_496_948_242_187_5];

    #[test]
    fn colorant_matrix_is_column_per_primary() {
        // The assembled matrix must place colorant j in column j — a transposed assembly
        // cannot pass this exact per-entry pin (the columns are pairwise distinct).
        let profile = rgb_profile(R_COL, G_COL, B_COL, [gamma_tag(), gamma_tag(), gamma_tag()]);
        let pipeline = rgb_device_to_pcs(&profile).unwrap();
        let Stage::Matrix { m, offset } = &pipeline.stages()[1] else {
            panic!("stage 1 must be the colorant matrix");
        };
        for i in 0..3 {
            assert_eq!(m[i][0], R_COL[i], "red column entry {i}");
            assert_eq!(m[i][1], G_COL[i], "green column entry {i}");
            assert_eq!(m[i][2], B_COL[i], "blue column entry {i}");
        }
        assert_eq!(*offset, [0.0; 3]);
        // And the stage order is curves first: primaries at full scale come out as the
        // colorant columns themselves (gamma leaves 0 and 1 fixed).
        let mut out = [0.0; 3];
        pipeline.eval(&[0.0, 1.0, 0.0], &mut out).unwrap();
        assert_eq!(out, G_COL);
    }

    #[test]
    fn rgb_forward_applies_trc_before_matrix() {
        let profile = rgb_profile(R_COL, G_COL, B_COL, [gamma_tag(), gamma_tag(), gamma_tag()]);
        let pipeline = rgb_device_to_pcs(&profile).unwrap();
        let mut out = [0.0; 3];
        pipeline.eval(&[0.5, 0.0, 0.0], &mut out).unwrap();
        let lin = 0.5_f64.powf(2.199_218_75);
        for i in 0..3 {
            assert!(
                (out[i] - R_COL[i] * lin).abs() < 1e-15,
                "channel {i}: {} vs {}",
                out[i],
                R_COL[i] * lin
            );
        }
    }

    #[test]
    fn rgb_round_trip_through_both_directions_is_tight() {
        let profile = rgb_profile(R_COL, G_COL, B_COL, [gamma_tag(), gamma_tag(), gamma_tag()]);
        let forward = rgb_device_to_pcs(&profile).unwrap();
        let reverse = rgb_pcs_to_device(&profile).unwrap();
        let round_trip = forward.compose(reverse).unwrap();
        for rgb in [[0.0, 0.0, 0.0], [1.0, 1.0, 1.0], [0.25, 0.5, 0.75]] {
            let mut out = [0.0; 3];
            round_trip.eval(&rgb, &mut out).unwrap();
            for i in 0..3 {
                assert!(
                    (out[i] - rgb[i]).abs() < 1e-12,
                    "round trip {rgb:?} → {out:?}"
                );
            }
        }
    }

    #[test]
    fn singular_colorants_refuse_the_reverse_direction_only() {
        // Green collinear with red ⇒ zero determinant.
        let collinear = [1.0, 0.5, 0.125];
        let profile = rgb_profile(
            collinear,
            collinear,
            B_COL,
            [gamma_tag(), gamma_tag(), gamma_tag()],
        );
        assert!(
            rgb_device_to_pcs(&profile).is_ok(),
            "forward needs no inverse"
        );
        let err = rgb_pcs_to_device(&profile).unwrap_err();
        assert_eq!(
            err.to_string(),
            "cmm: colorant matrix is singular; no PCS-to-device transform exists"
        );
    }

    #[test]
    fn denormal_determinant_with_overflowing_inverse_is_singular() {
        // det = 1e-309 (denormal, nonzero and finite, so `mat_inv_3x3` proceeds) but
        // 1/det overflows to infinity: the finiteness sweep must catch the non-finite
        // cofactor products.
        let m = [[1e-103, 0.0, 0.0], [0.0, 1e-103, 0.0], [0.0, 0.0, 1e-103]];
        assert!(matches!(
            invert_colorants(&m).unwrap_err(),
            CmmError::SingularMatrix
        ));
    }

    #[test]
    fn missing_and_mistyped_tags_report_their_signature() {
        // Missing bTRC.
        let mut profile = rgb_profile(R_COL, G_COL, B_COL, [gamma_tag(), gamma_tag(), gamma_tag()]);
        profile.tags.retain(|(sig, _)| sig.0 != *b"bTRC");
        let err = rgb_device_to_pcs(&profile).unwrap_err();
        assert_eq!(err.to_string(), "cmm: profile is missing required tag bTRC");

        // Colorant tag holding a curve.
        let mut profile = rgb_profile(R_COL, G_COL, B_COL, [gamma_tag(), gamma_tag(), gamma_tag()]);
        profile.tags[0].1 = gamma_tag();
        let err = rgb_device_to_pcs(&profile).unwrap_err();
        assert_eq!(
            err.to_string(),
            "cmm: tag rXYZ holds an unusable element type"
        );

        // Colorant tag with an empty XYZType.
        let mut profile = rgb_profile(R_COL, G_COL, B_COL, [gamma_tag(), gamma_tag(), gamma_tag()]);
        profile.tags[1].1 = TagData::Xyz(Vec::new());
        let err = rgb_device_to_pcs(&profile).unwrap_err();
        assert_eq!(
            err.to_string(),
            "cmm: tag gXYZ holds an unusable element type"
        );

        // TRC tag holding an XYZ.
        let mut profile = rgb_profile(
            R_COL,
            G_COL,
            B_COL,
            [gamma_tag(), gamma_tag(), xyz_tag(B_COL)],
        );
        profile.tags[3].1 = xyz_tag(R_COL);
        let err = rgb_device_to_pcs(&profile).unwrap_err();
        assert_eq!(
            err.to_string(),
            "cmm: tag rTRC holds an unusable element type"
        );

        // Missing kTRC on the gray paths.
        let profile = IccProfile {
            header: ProfileHeader::new(DeviceClass::Display, ColorSpace::Gray),
            tags: Vec::new(),
        };
        for result in [gray_device_to_pcs(&profile), gray_pcs_to_device(&profile)] {
            assert_eq!(
                result.unwrap_err().to_string(),
                "cmm: profile is missing required tag kTRC"
            );
        }
    }

    #[test]
    fn lab_pcs_rgb_shaper_appends_xyz_to_lab() {
        use gamut_color::lab::xyz_to_lab;
        let mut profile = rgb_profile(R_COL, G_COL, B_COL, [gamma_tag(), gamma_tag(), gamma_tag()]);
        profile.header.pcs = ColorSpace::Lab;
        let forward = rgb_device_to_pcs(&profile).unwrap();
        // Stage shape: Curves → Matrix → XyzToLab (lcms2's BuildRGBInputMatrixShaper order,
        // with the Lab bridge LAST — after the colorant matrix).
        assert_eq!(forward.stages().len(), 3);
        assert!(matches!(forward.stages()[0], Stage::Curves(_)));
        assert!(matches!(forward.stages()[1], Stage::Matrix { .. }));
        assert!(matches!(forward.stages()[2], Stage::XyzToLab));
        // Output pin: the full-scale green primary lands on xyz_to_lab(G_COL, D50) exactly
        // (gamma fixes 1.0, the matrix reproduces the colorant column).
        let mut out = [0.0; 3];
        forward.eval(&[0.0, 1.0, 0.0], &mut out).unwrap();
        assert_eq!(out, xyz_to_lab(G_COL, D50_XYZ));
    }

    #[test]
    fn lab_pcs_rgb_shaper_prepends_lab_to_xyz_in_reverse() {
        let mut profile = rgb_profile(R_COL, G_COL, B_COL, [gamma_tag(), gamma_tag(), gamma_tag()]);
        profile.header.pcs = ColorSpace::Lab;
        let reverse = rgb_pcs_to_device(&profile).unwrap();
        // Stage shape: LabToXyz → Matrix(inverse) → Curves(inverses) — the bridge FIRST
        // (lcms2's BuildRGBOutputMatrixShaper order).
        assert_eq!(reverse.stages().len(), 3);
        assert!(matches!(reverse.stages()[0], Stage::LabToXyz));
        assert!(matches!(reverse.stages()[1], Stage::Matrix { .. }));
        assert!(matches!(reverse.stages()[2], Stage::Curves(_)));
        // Round trip through both Lab-PCS directions returns the device values.
        let forward = rgb_device_to_pcs(&profile).unwrap();
        let round_trip = forward.compose(reverse).unwrap();
        for rgb in [[0.25, 0.5, 0.75], [1.0, 1.0, 1.0], [0.1, 0.9, 0.4]] {
            let mut out = [0.0; 3];
            round_trip.eval(&rgb, &mut out).unwrap();
            for ch in 0..3 {
                assert!(
                    (out[ch] - rgb[ch]).abs() < 1e-9,
                    "round trip {rgb:?} → {out:?}"
                );
            }
        }
    }

    #[test]
    fn gray_xyz_pcs_scales_the_d50_white() {
        let profile = gray_profile(ColorSpace::Xyz, gamma_tag());
        let forward = gray_device_to_pcs(&profile).unwrap();
        assert_eq!(forward.input_channels(), 1);
        assert_eq!(forward.output_channels(), 3);
        let mut out = [0.0; 3];
        // Full white maps to exactly the D50 illuminant (gamma fixes 1.0)...
        forward.eval(&[1.0], &mut out).unwrap();
        assert_eq!(out, D50_XYZ);
        // ...and mid-gray to curve(g)·D50, component-wise.
        forward.eval(&[0.5], &mut out).unwrap();
        let lin = 0.5_f64.powf(2.199_218_75);
        for i in 0..3 {
            assert!(
                (out[i] - lin * D50_XYZ[i]).abs() < 1e-15,
                "component {i}: {} vs {}",
                out[i],
                lin * D50_XYZ[i]
            );
        }
    }

    #[test]
    fn gray_xyz_reverse_picks_y_and_inverts_the_curve() {
        let profile = gray_profile(ColorSpace::Xyz, gamma_tag());
        let reverse = gray_pcs_to_device(&profile).unwrap();
        assert_eq!(reverse.input_channels(), 3);
        assert_eq!(reverse.output_channels(), 1);
        let mut out = [0.0; 1];
        // X and Z must not leak into the picked Y (decoded Y / D50_Y with D50_Y = 1).
        let y = 0.5_f64.powf(2.199_218_75);
        reverse.eval(&[0.9, y, 0.1], &mut out).unwrap();
        assert!((out[0] - 0.5).abs() < 1e-12, "picked {}", out[0]);
    }

    #[test]
    fn gray_lab_pcs_carries_the_curve_in_lstar() {
        let profile = gray_profile(ColorSpace::Lab, gamma_tag());
        let forward = gray_device_to_pcs(&profile).unwrap();
        let mut out = [0.0; 3];
        forward.eval(&[0.5], &mut out).unwrap();
        let lin = 0.5_f64.powf(2.199_218_75);
        assert!((out[0] - 100.0 * lin).abs() < 1e-12, "L* = {}", out[0]);
        assert_eq!(out[1], 0.0, "a* must be exactly 0");
        assert_eq!(out[2], 0.0, "b* must be exactly 0");
        // The reverse picks L*/100 (a*/b* ignored) and inverts the curve.
        let reverse = gray_pcs_to_device(&profile).unwrap();
        let mut g = [0.0; 1];
        reverse.eval(&[100.0 * lin, 25.0, -30.0], &mut g).unwrap();
        assert!((g[0] - 0.5).abs() < 1e-12, "gray = {}", g[0]);
    }

    #[test]
    fn shaper_pipelines_run_through_the_transform_trait() {
        // The public consumption route: interleaved buffers through `Transform::transform`.
        let profile = gray_profile(ColorSpace::Xyz, gamma_tag());
        let forward = gray_device_to_pcs(&profile).unwrap();
        let src = [0.0, 1.0];
        let mut dst = [0.0; 6];
        forward.transform(&src, &mut dst).unwrap();
        assert_eq!(dst[..3], [0.0; 3]);
        assert_eq!(dst[3..], D50_XYZ);
    }
}
