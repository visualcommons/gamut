//! ICC-absolute colorimetric rendering: the media-white-point scaling applied at the PCS
//! seam between two linked profiles.
//!
//! Perceptual, media-relative, and saturation renderings are **profile-baked**: the per-intent
//! tag selection in [`crate::link`] already did all the work, and this CMM applies no further
//! gamut mapping of its own (for v4 perceptual that means the profile's own mapping to the
//! Perceptual Reference Medium Gamut is trusted as-is — see
//! `references/cmm/ICCSpecRevision_22_02_05_PRMG.pdf` and `references/cmm/render.pdf`).
//! ICC-absolute is the one intent that adds CMM-side math: relative colorimetry rescaled so
//! the *media* whites of the two profiles map onto each other instead of both collapsing to
//! the adopted D50 white.
//!
//! # The transcribed lcms2 behaviour
//!
//! Little-CMS computes the absolute adjustment in `ComputeAbsoluteIntent`
//! (`cmscnvrt.c:249-325`), transcribed as `absolute_conversion`, applied in XYZ at the seam
//! between the two profiles' pipelines. Every branch starts from the media-white scaling
//!
//! ```text
//! Scale = diag(WhiteIn.X / WhiteOut.X, WhiteIn.Y / WhiteOut.Y, WhiteIn.Z / WhiteOut.Z)
//! ```
//!
//! and the [`AdaptationState`] picks what surrounds it:
//!
//! | state | matrix | `chad` read? |
//! | --- | --- | --- |
//! | `1` (the default) | `Scale` | no |
//! | `0` | `ChadOut · Scale · ChadIn⁻¹` | both |
//! | in between | `ChadIn⁻¹ · Scale · Bradford(daylight(T) → D50)`, `T = (1 − s)·T_out + s·T_in` | both |
//!
//! where `T_in`/`T_out` are the correlated colour temperatures of the whites each `chad` adapts
//! from (Robertson's method), and the in-between branch returns the identity outright when
//! `Scale` is within `1/65535` of it and the two temperatures within 0.01 K. Each row is
//! lcms2's, operand order included — the `0` row in particular is what lcms2's code computes
//! (it overwrites an earlier `m2 · ChadOut`), not a re-derivation. `chad` is read by lcms2's
//! `_cmsReadCHAD` rules (`read_chad`): the tag, else for a v2 display profile the Bradford
//! adaptation from its `wtpt` to D50, else the identity. At state 1, the default, the `chad`
//! tag is **never read** (see `references/cmm/README.md` and STATUS.md).
//!
//! The white points come from each profile's `wtpt` tag via the [`media_white_point`] quirk
//! rules below. Like lcms2, the ratio is formed with no zero guard — a (nonsensical) zero
//! white-point component produces a non-finite scale, exactly as the oracle would.

use gamut_color::cct::{cct_from_xy, daylight_xy};
use gamut_color::linalg::{mat_inv_3x3, mat_mul3, matvec3};
use gamut_color::matrix::bradford_adapt;
use gamut_icc::{DeviceClass, IccProfile, KnownTag, TagData};

use crate::error::{CmmError, Result};
use crate::transform::AdaptationState;

/// A row-major 3×3 matrix acting on column vectors — lcms2's `cmsMAT3` convention.
type Matrix3 = [[f64; 3]; 3];

/// Little-CMS's D50 white: the **rounded literals** `cmsD50X/Y/Z = 0.9642, 1.0, 0.8249`
/// (`lcms2.h:292-294`), not this workspace's exact s15Fixed16 rational
/// [`gamut_color::lab::D50_XYZ`] (they differ by ≤ 5.5e-6 in X/Z).
///
/// Everywhere this crate replicates an lcms2 *algorithm* whose output feeds a differential
/// comparison — the [`media_white_point`] defaults, the black-point detection Lab↔XYZ
/// conversions, and the BPC scaling anchor ([`crate::bpc`]) — the rounded literals are used so
/// the oracle is matched exactly; the pipeline *stages* ([`crate::pipeline::Stage::XyzToLab`]
/// and friends) keep the exact PCS illuminant, as settled in P4.
pub(crate) const LCMS_D50: [f64; 3] = [0.9642, 1.0, 0.8249];

/// Reads a profile's media white point with lcms2's `_cmsReadMediaWhitePoint` quirk rules
/// (`cmsio1.c:64-90`), verbatim:
///
/// - no usable `wtpt` tag (absent, wrong element type, or an empty `XYZType`) → [`LCMS_D50`];
/// - a **v2** (header major version < 4) **display-class** profile → [`LCMS_D50`], ignoring
///   the tag entirely (many legacy display profiles stored the *measured* white here, which
///   would wrongly re-scale colorants that are already D50-adapted);
/// - otherwise → the tag's first XYZ value, as-is.
pub(crate) fn media_white_point(profile: &IccProfile) -> [f64; 3] {
    let tagged = match profile.get(KnownTag::MediaWhitePoint) {
        Some(TagData::Xyz(values)) => values.first().map(|xyz| xyz.to_f64()),
        _ => None,
    };
    let Some(tagged) = tagged else {
        return LCMS_D50;
    };
    if profile.header.version.major < 4 && profile.header.device_class == DeviceClass::Display {
        return LCMS_D50;
    }
    tagged
}

/// The ICC-absolute white-scaling matrix `diag(wIn / wOut)` between two profiles' media
/// whites (lcms2's `Scale` in `ComputeAbsoluteIntent`, and the whole conversion at adaptation
/// state 1.0 — module docs), acting on decoded PCSXYZ.
pub(crate) fn absolute_scaling(src: &IccProfile, dst: &IccProfile) -> Matrix3 {
    let w_in = media_white_point(src);
    let w_out = media_white_point(dst);
    let mut m = [[0.0; 3]; 3];
    for (i, row) in m.iter_mut().enumerate() {
        row[i] = w_in[i] / w_out[i];
    }
    m
}

/// The ICC-absolute conversion matrix between two profiles at `adaptation` — lcms2's
/// `ComputeAbsoluteIntent` (`cmscnvrt.c:249-325`), verbatim (module docs). Acts on decoded
/// PCSXYZ and is offset-free; the caller decides stage placement and the empty-layer skip.
///
/// # Errors
///
/// [`CmmError::UnsupportedProfile`] below full adaptation when a `chad` matrix the branch
/// inverts is singular, when an adopted white has no correlated colour temperature, or when the
/// mixed temperature falls outside the CIE daylight locus — the cases where lcms2's
/// constructor fails (or, for the last, reads an uninitialised white).
pub(crate) fn absolute_conversion(
    src: &IccProfile,
    dst: &IccProfile,
    adaptation: AdaptationState,
) -> Result<Matrix3> {
    let scale = absolute_scaling(src, dst);
    let state = adaptation.get();
    if state == 1.0 {
        // Fully adapted: keep the chromatic adaptation, the standard v4 behaviour.
        return Ok(scale);
    }
    let chad_in = read_chad(src)?;
    let chad_out = read_chad(dst)?;
    let chad_in_inverse = invert(&chad_in)?;
    if state == 0.0 {
        // Not adapted: undo the chromatic adaptation. lcms2 computes `m2 · chadOut` into `m`
        // first and overwrites it, so the result is `chadOut · Scale · chadIn⁻¹`.
        return Ok(mat_mul3(&mat_mul3(&chad_out, &scale), &chad_in_inverse));
    }
    let undone = mat_mul3(&chad_in_inverse, &scale);
    let temp_src = chad_temperature(&chad_in)?;
    let temp_dst = chad_temperature(&chad_out)?;
    if is_identity(&scale) && same_temperature(temp_src, temp_dst) {
        return Ok(IDENTITY);
    }
    // Interpolated in kelvin, as lcms2 does, then a CHAD from that daylight white to D50
    // replaces the output CHAD.
    let temp = (1.0 - state) * temp_dst + state * temp_src;
    Ok(mat_mul3(&undone, &temperature_chad(temp)?))
}

/// The 3×3 identity.
const IDENTITY: Matrix3 = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];

/// lcms2's `_cmsMAT3isIdentity` (`cmsmtrx.c:98-110`): every entry within `1/65535` of the
/// identity's.
fn is_identity(m: &Matrix3) -> bool {
    m.iter()
        .flatten()
        .zip(IDENTITY.iter().flatten())
        .all(|(a, b)| (b - a).abs() < 1.0 / 65535.0)
}

/// lcms2's `fabs(TempSrc - TempDest) < 0.01` in `ComputeAbsoluteIntent`: two adopted whites
/// within a hundredth of a kelvin, the partial-state branch's identity shortcut. A function of
/// its own so the boundary is reachable with exact operands (`0.01 - 0.0` is the literal
/// itself), which a pair of `chad`-derived temperatures cannot hit.
fn same_temperature(a: f64, b: f64) -> bool {
    (a - b).abs() < 0.01
}

/// `m⁻¹`, or the error lcms2's failed `_cmsMAT3inverse` becomes.
fn invert(m: &Matrix3) -> Result<Matrix3> {
    mat_inv_3x3(m).ok_or(CmmError::UnsupportedProfile(
        "chromatic adaptation (chad) matrix is singular",
    ))
}

/// The CIE 1931 chromaticity of a tristimulus value (lcms2's `cmsXYZ2xyY`, without `Y`).
fn chromaticity([x, y, z]: [f64; 3]) -> [f64; 2] {
    let sum = x + y + z;
    [x / sum, y / sum]
}

/// lcms2's `_cmsAdaptationMatrix` with the Bradford cone matrix, between two **tristimulus**
/// whites: `gamut-color`'s [`bradford_adapt`] takes chromaticities, i.e. both whites at
/// `Y = 1`, and the tristimulus form is that matrix scaled by `to.Y / from.Y` (the cone
/// ratios scale with the whites).
fn bradford(from: [f64; 3], to: [f64; 3]) -> Result<Matrix3> {
    let unscaled = bradford_adapt(chromaticity(from), chromaticity(to));
    let m = unscaled.map(|m| m.map(|row| row.map(|v| v * to[1] / from[1])));
    m.filter(|m| m.iter().flatten().all(|v| v.is_finite()))
        .ok_or(CmmError::UnsupportedProfile(
            "media white point admits no chromatic adaptation",
        ))
}

/// A profile's chromatic-adaptation matrix with lcms2's `_cmsReadCHAD` rules
/// (`cmsio1.c:94-128`), verbatim:
///
/// - a `chad` tag lcms2 can read — an `s15Fixed16ArrayType` of at least nine values (lcms2
///   rejects fewer) — → its first nine, row-major;
/// - else a **v2 display-class** profile → the Bradford adaptation from its tagged `wtpt` (the
///   raw tag, not [`media_white_point`]'s quirk) to [`LCMS_D50`], or the identity when it has no
///   usable `wtpt`;
/// - else the identity.
fn read_chad(profile: &IccProfile) -> Result<Matrix3> {
    if let Some(TagData::S15Fixed16Array(values)) = profile.get(KnownTag::ChromaticAdaptation)
        && let [a, b, c, d, e, f, g, h, k, ..] = values.as_slice()
    {
        return Ok([[a, b, c], [d, e, f], [g, h, k]].map(|row| row.map(|v| v.to_f64())));
    }
    if profile.header.version.major < 4
        && profile.header.device_class == DeviceClass::Display
        && let Some(TagData::Xyz(values)) = profile.get(KnownTag::MediaWhitePoint)
        && let Some(white) = values.first()
    {
        return bradford(white.to_f64(), LCMS_D50);
    }
    Ok(IDENTITY)
}

/// lcms2's `CHAD2Temp` (`cmscnvrt.c:205-233`): the correlated colour temperature of the white
/// a `chad` adapts *from* — [`LCMS_D50`] carried back through the inverse matrix — by
/// Robertson's method (`gamut-color`'s [`cct_from_xy`], lcms2's `cmsTempFromWhitePoint` over
/// the same Wyszecki & Stiles table).
///
/// A white beyond either end of the table is refused, as lcms2 refuses it:
/// `cmsTempFromWhitePoint` (`cmswtpnt.c:143-185`) finds no isotherm crossing and fails the
/// transform, where [`cct_from_xy`] clamps to the end line's temperature. The clamp is exactly
/// `1e6 / 600` or `1e6 / 10` K, so a result at either value is the clamp (or a white exactly on
/// the end isotherm, where lcms2's `di / dj < 0` test fails too) and is rejected here. One
/// residual divergence remains at the hot end: a white between the table's 0- and 10-mired
/// isotherms (hotter than 100 000 K) gets a temperature from lcms2, which interpolates toward
/// infinity, and an error here.
fn chad_temperature(chad: &Matrix3) -> Result<f64> {
    let white = matvec3(&invert(chad)?, LCMS_D50);
    cct_from_xy(chromaticity(white))
        .filter(|&temp| ROBERTSON_COOLEST < temp && temp < ROBERTSON_HOTTEST)
        .ok_or(CmmError::UnsupportedProfile(
            "chromatic adaptation (chad) adopted white has no colour temperature",
        ))
}

/// The temperature [`cct_from_xy`] clamps a too-red white to: its table's 600-mired end line.
const ROBERTSON_COOLEST: f64 = 1.0e6 / 600.0;

/// The temperature [`cct_from_xy`] clamps a too-blue white to: its table's 10-mired line, the
/// hottest it interpolates to.
const ROBERTSON_HOTTEST: f64 = 1.0e6 / 10.0;

/// lcms2's `Temp2CHAD` (`cmscnvrt.c:235-245`): the Bradford adaptation from the CIE daylight
/// white at `temp` kelvin (`gamut-color`'s [`daylight_xy`], lcms2's `cmsWhitePointFromTemp`)
/// to [`LCMS_D50`].
fn temperature_chad(temp: f64) -> Result<Matrix3> {
    let [x, y] = daylight_xy(temp).ok_or(CmmError::UnsupportedProfile(
        "partial adaptation temperature is outside the CIE daylight locus",
    ))?;
    bradford([x / y, 1.0, (1.0 - x - y) / y], LCMS_D50)
}

#[cfg(test)]
mod tests {
    use gamut_icc::{ColorSpace, Curve, IccProfile, ProfileHeader, Tag, XyzNumber};

    use super::*;

    /// D65 at unit Y, s15Fixed16-quantized by the tag round trip below.
    const D65: [f64; 3] = [0.9504, 1.0, 1.0889];

    fn profile_with_wtpt(class: DeviceClass, major: u8, wtpt: Option<TagData>) -> IccProfile {
        let mut header = ProfileHeader::new(class, ColorSpace::Rgb);
        header.version.major = major;
        let mut tags = Vec::new();
        if let Some(data) = wtpt {
            tags.push(Tag::new(*b"wtpt", data));
        }
        IccProfile { header, tags }
    }

    fn xyz_tag(v: [f64; 3]) -> TagData {
        TagData::Xyz(vec![XyzNumber::from_f64(v)])
    }

    #[test]
    fn missing_or_unusable_wtpt_defaults_to_lcms2_rounded_d50() {
        // Absent tag.
        let p = profile_with_wtpt(DeviceClass::Output, 4, None);
        assert_eq!(media_white_point(&p), LCMS_D50);
        // Wrong element type under the wtpt signature.
        let p = profile_with_wtpt(
            DeviceClass::Output,
            4,
            Some(TagData::Curve(Curve::Identity)),
        );
        assert_eq!(media_white_point(&p), LCMS_D50);
        // Present but empty XYZType.
        let p = profile_with_wtpt(DeviceClass::Output, 4, Some(TagData::Xyz(Vec::new())));
        assert_eq!(media_white_point(&p), LCMS_D50);
        // And the default is the ROUNDED lcms2 D50, not the exact PCS illuminant.
        assert_eq!(LCMS_D50, [0.9642, 1.0, 0.8249]);
        assert_ne!(LCMS_D50, gamut_color::lab::D50_XYZ);
    }

    #[test]
    fn v2_display_wtpt_is_forced_to_d50_but_v4_display_and_v2_output_are_not() {
        let quantized_d65 = XyzNumber::from_f64(D65).to_f64();
        // THE quirk: v2 (major < 4) display class ignores its tagged white.
        let p = profile_with_wtpt(DeviceClass::Display, 2, Some(xyz_tag(D65)));
        assert_eq!(media_white_point(&p), LCMS_D50);
        // v4 display: tag honoured.
        let p = profile_with_wtpt(DeviceClass::Display, 4, Some(xyz_tag(D65)));
        assert_eq!(media_white_point(&p), quantized_d65);
        // v2 non-display (output class): tag honoured — the force is display-only.
        let p = profile_with_wtpt(DeviceClass::Output, 2, Some(xyz_tag(D65)));
        assert_eq!(media_white_point(&p), quantized_d65);
        // v3 counts as "< 4" (lcms2 compares the encoded version against 0x4000000).
        let p = profile_with_wtpt(DeviceClass::Display, 3, Some(xyz_tag(D65)));
        assert_eq!(media_white_point(&p), LCMS_D50);
    }

    #[test]
    fn absolute_scaling_is_the_diagonal_white_ratio() {
        let src = profile_with_wtpt(DeviceClass::Display, 4, Some(xyz_tag(D65)));
        let dst = profile_with_wtpt(DeviceClass::Output, 4, None); // → D50
        let w = XyzNumber::from_f64(D65).to_f64();
        let m = absolute_scaling(&src, &dst);
        for (i, row) in m.iter().enumerate() {
            for (j, cell) in row.iter().enumerate() {
                let want = if i == j { w[i] / LCMS_D50[i] } else { 0.0 };
                assert_eq!(*cell, want, "entry [{i}][{j}]");
            }
        }
        // Diagonal per-channel, not a common factor: X and Z ratios differ from Y's 1.0.
        assert_ne!(m[0][0], m[1][1]);
        assert_ne!(m[2][2], m[1][1]);
    }

    #[test]
    fn absolute_scaling_between_equal_whites_is_the_exact_identity() {
        // Both profiles default to D50 (the everyday case): the ratio is exactly 1.0 per
        // channel, so the layer is empty and `between` skips the stage entirely.
        let src = profile_with_wtpt(DeviceClass::Display, 4, None);
        let dst = profile_with_wtpt(DeviceClass::Output, 4, None);
        assert_eq!(
            absolute_scaling(&src, &dst),
            [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]]
        );
        // The v2-display force makes a D65-tagged v2 display profile scale as D50 too.
        let v2_display = profile_with_wtpt(DeviceClass::Display, 2, Some(xyz_tag(D65)));
        assert_eq!(
            absolute_scaling(&v2_display, &dst),
            [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]]
        );
    }

    // --- adaptation state ----------------------------------------------------------------

    fn state(degree: f64) -> AdaptationState {
        AdaptationState::new(degree).expect("in [0, 1]")
    }

    fn chad_tag(m: Matrix3) -> Tag {
        Tag::new(
            *b"chad",
            TagData::S15Fixed16Array(
                m.iter()
                    .flatten()
                    .map(|&v| gamut_icc::S15Fixed16::from_f64(v))
                    .collect(),
            ),
        )
    }

    /// A v4 output profile with an optional `wtpt` and an optional `chad`.
    fn profile_with(wtpt: Option<[f64; 3]>, chad: Option<Matrix3>) -> IccProfile {
        let mut p = profile_with_wtpt(DeviceClass::Output, 4, wtpt.map(xyz_tag));
        p.tags.extend(chad.map(chad_tag));
        p
    }

    /// The D65 → D50 Bradford matrix, quantized through the `chad` tag the way a profile
    /// stores it.
    fn d65_chad() -> Matrix3 {
        let m = bradford(D65, LCMS_D50).expect("adaptable");
        read_chad(&profile_with(None, Some(m))).expect("readable")
    }

    fn assert_close(got: &Matrix3, want: &Matrix3, tolerance: f64) {
        for (g, w) in got.iter().flatten().zip(want.iter().flatten()) {
            assert!((g - w).abs() < tolerance, "{got:?} vs {want:?}");
        }
    }

    #[test]
    fn full_adaptation_is_the_scaling_and_reads_no_chad() {
        // A singular chad is unusable — and irrelevant at state 1, where lcms2's branch
        // never touches it.
        let src = profile_with(Some(D65), Some([[0.0; 3]; 3]));
        let dst = profile_with(None, None);
        assert_eq!(
            absolute_conversion(&src, &dst, AdaptationState::FULL).unwrap(),
            absolute_scaling(&src, &dst)
        );
        for degree in [0.0, 0.5] {
            assert!(matches!(
                absolute_conversion(&src, &dst, state(degree)),
                Err(CmmError::UnsupportedProfile(_))
            ));
        }
    }

    #[test]
    fn no_adaptation_undoes_both_chads_around_the_scaling() {
        let chad_in = d65_chad();
        let chad_out = [[1.1, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 0.9]];
        let src = profile_with(Some(D65), Some(chad_in));
        let dst = profile_with(None, Some(chad_out));
        let chad_out = read_chad(&dst).unwrap();
        let scale = absolute_scaling(&src, &dst);
        let want = mat_mul3(
            &mat_mul3(&chad_out, &scale),
            &mat_inv_3x3(&chad_in).unwrap(),
        );
        assert_eq!(
            absolute_conversion(&src, &dst, AdaptationState::NONE).unwrap(),
            want
        );
    }

    #[test]
    fn partial_adaptation_between_equal_whites_is_the_exact_identity() {
        // Scale is the identity (both whites default to D50) and both chads adapt from the
        // same temperature: lcms2 returns the identity outright.
        let src = profile_with(None, Some(d65_chad()));
        let dst = profile_with(None, Some(d65_chad()));
        assert_eq!(
            absolute_conversion(&src, &dst, state(0.5)).unwrap(),
            IDENTITY
        );
    }

    #[test]
    fn partial_adaptation_mixes_the_temperatures_in_kelvin() {
        // Scale is still the identity, but the chads adapt from D65 and from D50: no shortcut,
        // and the replacement CHAD comes from the daylight white at the kelvin midpoint.
        let src = profile_with(None, Some(d65_chad()));
        let dst = profile_with(None, None);
        let t_in = chad_temperature(&d65_chad()).unwrap();
        let t_out = chad_temperature(&IDENTITY).unwrap();
        assert!((t_in - 6504.0).abs() < 15.0, "D65 chad reads as {t_in} K");
        assert!(
            (t_out - 5003.0).abs() < 10.0,
            "identity chad reads as {t_out} K"
        );
        for degree in [0.25, 0.5] {
            let mixed = (1.0 - degree) * t_out + degree * t_in;
            let want = mat_mul3(
                &mat_inv_3x3(&d65_chad()).unwrap(),
                &temperature_chad(mixed).unwrap(),
            );
            let got = absolute_conversion(&src, &dst, state(degree)).unwrap();
            assert_close(&got, &want, 1e-12);
            assert!(!is_identity(&got), "{degree}: {got:?}");
        }
    }

    #[test]
    fn same_temperature_is_strictly_within_a_hundredth_of_a_kelvin() {
        assert!(same_temperature(6504.0, 6504.0));
        assert!(same_temperature(0.0099, 0.0));
        // `0.01 - 0.0` is the bound itself: lcms2's `<` excludes it, in either order.
        assert!(!same_temperature(0.01, 0.0));
        assert!(!same_temperature(0.0, 0.01));
    }

    #[test]
    fn chad_whites_beyond_robertsons_table_have_no_temperature() {
        // Whites past the 600-mired (too red) and the 0-mired (too blue) isotherms:
        // `cct_from_xy` clamps both to its end temperatures, lcms2's `cmsTempFromWhitePoint`
        // finds no crossing and fails.
        for [x, y] in [[0.6, 0.38], [0.24, 0.22]] {
            let white = [x / y, 1.0, (1.0 - x - y) / y];
            let clamped = cct_from_xy([x, y]).unwrap();
            assert!(
                clamped == ROBERTSON_COOLEST || clamped == ROBERTSON_HOTTEST,
                "{white:?} → {clamped} K"
            );
            let chad = bradford(white, LCMS_D50).unwrap();
            assert!(
                matches!(
                    chad_temperature(&chad),
                    Err(CmmError::UnsupportedProfile(_))
                ),
                "{white:?}"
            );
        }
    }

    #[test]
    fn identity_test_is_one_sixteen_bit_quantum_wide() {
        let mut m = IDENTITY;
        m[2][1] = 0.9 / 65535.0;
        assert!(is_identity(&m));
        m[2][1] = 1.0 / 65535.0;
        assert!(!is_identity(&m));
        m[2][1] = -0.9 / 65535.0;
        m[0][0] = 1.0 + 1.1 / 65535.0;
        assert!(!is_identity(&m));
    }

    #[test]
    fn chad_is_read_by_lcms2s_rules() {
        let chad = [[1.5, 0.25, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 0.5]];
        // A nine-value sf32 tag is the matrix, row-major.
        assert_eq!(read_chad(&profile_with(None, Some(chad))).unwrap(), chad);
        // Fewer than nine values: lcms2 rejects the tag, so the identity default.
        let mut short = profile_with(None, None);
        short.tags.push(Tag::new(
            *b"chad",
            TagData::S15Fixed16Array(vec![gamut_icc::S15Fixed16::from_f64(1.0); 8]),
        ));
        assert_eq!(read_chad(&short).unwrap(), IDENTITY);
        // No chad on a v2 display profile: Bradford from its *raw* wtpt (not the D50 that
        // `media_white_point`'s v2-display quirk substitutes) to lcms2's D50.
        let v2_display = profile_with_wtpt(DeviceClass::Display, 2, Some(xyz_tag(D65)));
        let quantized_d65 = XyzNumber::from_f64(D65).to_f64();
        assert_eq!(
            read_chad(&v2_display).unwrap(),
            bradford(quantized_d65, LCMS_D50).unwrap()
        );
        // … and the identity when it has no wtpt; v4 display and v2 output never adapt.
        let bare = profile_with_wtpt(DeviceClass::Display, 2, None);
        assert_eq!(read_chad(&bare).unwrap(), IDENTITY);
        let v4_display = profile_with_wtpt(DeviceClass::Display, 4, Some(xyz_tag(D65)));
        assert_eq!(read_chad(&v4_display).unwrap(), IDENTITY);
        let v2_output = profile_with_wtpt(DeviceClass::Output, 2, Some(xyz_tag(D65)));
        assert_eq!(read_chad(&v2_output).unwrap(), IDENTITY);
    }

    #[test]
    fn bradford_carries_the_source_tristimulus_onto_the_destination() {
        // The law of a von Kries adaptation: the source white lands exactly on the
        // destination white — including its luminance, which the chromaticity-only
        // `bradford_adapt` drops and the `to.Y / from.Y` factor restores.
        for (from, to) in [
            (D65, LCMS_D50),
            ([1.9, 2.0, 2.2], LCMS_D50),
            (D65, [0.5, 0.6, 0.4]),
        ] {
            let adapted = matvec3(&bradford(from, to).unwrap(), from);
            for k in 0..3 {
                assert!(
                    (adapted[k] - to[k]).abs() < 1e-12,
                    "{from:?} → {to:?}: {adapted:?}"
                );
            }
        }
        // A white with no chromaticity has no adaptation.
        assert!(matches!(
            bradford([0.0; 3], LCMS_D50),
            Err(CmmError::UnsupportedProfile(_))
        ));
    }

    #[test]
    fn temperatures_outside_what_the_daylight_locus_covers_are_refused() {
        assert!(temperature_chad(4000.0).is_ok());
        assert!(matches!(
            temperature_chad(3999.0),
            Err(CmmError::UnsupportedProfile(_))
        ));
        // A singular chad has no adopted white to take a temperature of.
        assert!(matches!(
            chad_temperature(&[[0.0; 3]; 3]),
            Err(CmmError::UnsupportedProfile(_))
        ));
        // The daylight CHAD at D65's temperature is (near) the D65 Bradford chad.
        let t = chad_temperature(&d65_chad()).unwrap();
        assert_close(&temperature_chad(t).unwrap(), &d65_chad(), 2e-3);
    }
}
