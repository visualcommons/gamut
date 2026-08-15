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
//! (`cmscnvrt.c:249-325`). At the **default adaptation state 1.0** — the only state this
//! crate implements — the matrix is exactly
//!
//! ```text
//! m = diag(WhiteIn.X / WhiteOut.X, WhiteIn.Y / WhiteOut.Y, WhiteIn.Z / WhiteOut.Z)
//! ```
//!
//! applied in XYZ at the seam between the two profiles' pipelines, and the `chad` tag is
//! **never read** (its only consumer is the non-default adaptation-state branch of the same
//! function; see `references/cmm/README.md` and STATUS.md). The white points come from each
//! profile's `wtpt` tag via the [`media_white_point`] quirk rules below. Like lcms2, the
//! ratio is formed with no zero guard — a (nonsensical) zero white-point component produces a
//! non-finite scale, exactly as the oracle would.

use gamut_icc::{DeviceClass, IccProfile, KnownTag, TagData};

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
/// whites (lcms2's `ComputeAbsoluteIntent` at adaptation state 1.0 — module docs), acting on
/// decoded PCSXYZ. Offset-free; the caller decides stage placement and the empty-layer skip.
pub(crate) fn absolute_scaling(src: &IccProfile, dst: &IccProfile) -> [[f64; 3]; 3] {
    let w_in = media_white_point(src);
    let w_out = media_white_point(dst);
    let mut m = [[0.0; 3]; 3];
    for (i, row) in m.iter_mut().enumerate() {
        row[i] = w_in[i] / w_out[i];
    }
    m
}

#[cfg(test)]
mod tests {
    use gamut_icc::{ColorSpace, Curve, IccProfile, ProfileHeader, Signature, XyzNumber};

    use super::*;

    /// D65 at unit Y, s15Fixed16-quantized by the tag round trip below.
    const D65: [f64; 3] = [0.9504, 1.0, 1.0889];

    fn profile_with_wtpt(class: DeviceClass, major: u8, wtpt: Option<TagData>) -> IccProfile {
        let mut header = ProfileHeader::new(class, ColorSpace::Rgb);
        header.version.major = major;
        let mut tags = Vec::new();
        if let Some(data) = wtpt {
            tags.push((Signature(*b"wtpt"), data));
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
}
