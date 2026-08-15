//! Profile linking: builds runnable [`Pipeline`]s from parsed ICC profiles.
//!
//! [`device_to_pcs`] and [`pcs_to_device`] turn one [`gamut_icc::IccProfile`] into one half of
//! a colour conversion, over the crate's sample convention (device channels encoded `[0, 1]`,
//! PCS seams **decoded** colorimetry — XYZ with D50 `Y = 1.0`, Lab with `L*` in `0..=100`).
//! Linking a profile *pair* is composing the two halves
//! ([`Pipeline::compose`](crate::Pipeline::compose)).
//!
//! # Dispatch: per-intent LUT tags first, shaper fallback second (the lcms2 rule)
//!
//! Little-CMS resolves a profile to a pipeline by trying the requested intent's 16-bit LUT
//! tag, then falling back (`_cmsReadInputLUT`/`_cmsReadOutputLUT`, `cmsio1.c:304-397` /
//! `578-651`). This module transcribes that dispatch:
//!
//! 1. The intent indexes lcms2's verbatim `Device2PCS16`/`PCS2Device16` tables
//!    (`cmsio1.c:31-50`): perceptual → `A2B0`/`B2A0`, media-relative → `A2B1`/`B2A1`,
//!    saturation → `A2B2`/`B2A2`, and **ICC-absolute → the media-relative tag**
//!    (`A2B1`/`B2A1` — absolute is relative plus a white-point scaling, which
//!    [`IccTransform::between`](crate::IccTransform::between) applies at the PCS seam, so a
//!    single profile's half-pipeline is identical for absolute and relative).
//! 2. If that tag is absent, fall back to the **perceptual** tag (`A2B0`/`B2A0`).
//! 3. If neither exists, fall back to the matrix/TRC ("shaper") tag set — RGB and gray
//!    profiles, XYZ or Lab PCS.
//! 4. A profile that supports none of the above fails with [`CmmError::MissingTag`] carrying
//!    the requested intent's primary LUT tag (for a non-RGB/gray device space) or the first
//!    missing shaper tag.
//!
//! The float `DToBx`/`BToDx` tags, which lcms2 would consult *before* the 16-bit tables, are
//! out of scope with the rest of `multiProcessElementsType` (`gamut-icc` preserves them as
//! raw bytes; see the crate docs' iccMAX/mpet deferral) — profiles carrying only float
//! transform tags dispatch as if they were absent, exactly like a pre-2.6 lcms2.
//!
//! # Chromatic-adaptation convention (the v2/v4 `chad` decision)
//!
//! Colorant tags (`rXYZ`/`gXYZ`/`bXYZ`) are consumed **as-is**, for v2 and v4 profiles alike:
//! ICC.1:2022 §8.3.4 requires them to be already D50-adapted, and the `chad` tag is **never
//! read** on this relative-colorimetric path — matching lcms2, whose only `chad` consumer is
//! the absolute-intent white-point scaling (`cmscnvrt.c`), itself inert at the default
//! adaptation state. A strict reading of some v2 profiles (colorants relative to the actual
//! white, `chad` meant to adapt them) would disagree; this crate deliberately follows lcms2.
//! The `wtpt` tag is likewise reserved to absolute intent (#329). The full audit and the
//! differential tests pinning it live in `STATUS.md` ("Settled decisions (P4)") and
//! `tests/oracle_shaper.rs`.

mod lut;
mod shaper;

use gamut_icc::{ColorSpace, IccProfile, KnownTag, RenderingIntent, TagData};
use lut::Direction;

use crate::error::{CmmError, Result};
use crate::pipeline::Pipeline;

/// lcms2's `Device2PCS16` intent→tag table (`cmsio1.c:31-38`), verbatim: indexed by
/// [`intent_index`]. Absolute colorimetric (index 3) deliberately reuses the
/// media-relative tag `A2B1`. Shared with [`crate::bpc`]'s intent-support gates.
pub(crate) const DEVICE_TO_PCS_16: [KnownTag; 4] = [
    KnownTag::AToB0, // Perceptual
    KnownTag::AToB1, // Relative colorimetric
    KnownTag::AToB2, // Saturation
    KnownTag::AToB1, // Absolute colorimetric
];

/// lcms2's `PCS2Device16` intent→tag table (`cmsio1.c:43-46`), verbatim.
pub(crate) const PCS_TO_DEVICE_16: [KnownTag; 4] = [
    KnownTag::BToA0, // Perceptual
    KnownTag::BToA1, // Relative colorimetric
    KnownTag::BToA2, // Saturation
    KnownTag::BToA1, // Absolute colorimetric
];

/// The intent's index into the tag tables — the ICC intent numbering (perceptual 0,
/// media-relative 1, saturation 2, ICC-absolute 3), which is also lcms2's `INTENT_*` order.
pub(crate) fn intent_index(intent: RenderingIntent) -> usize {
    match intent {
        RenderingIntent::Perceptual => 0,
        RenderingIntent::MediaRelativeColorimetric => 1,
        RenderingIntent::Saturation => 2,
        RenderingIntent::IccAbsoluteColorimetric => 3,
    }
}

/// Selects the LUT tag for the intent with lcms2's fallback: the requested intent's tag if
/// present, else the perceptual tag (`table[0]`), else `None`.
fn select_lut_tag<'a>(
    profile: &'a IccProfile,
    table: &[KnownTag; 4],
    intent: RenderingIntent,
) -> Option<(KnownTag, &'a TagData)> {
    let requested = table[intent_index(intent)];
    if let Some(data) = profile.get(requested) {
        return Some((requested, data));
    }
    let fallback = table[0];
    profile.get(fallback).map(|data| (fallback, data))
}

/// Rejects a PCS the shaper fallback cannot decode into: the shaper builders produce decoded
/// XYZ or Lab, so the header must claim one of the two ICC connection spaces.
fn check_pcs(profile: &IccProfile) -> Result<()> {
    match profile.header.pcs {
        ColorSpace::Xyz | ColorSpace::Lab => Ok(()),
        _ => Err(CmmError::UnsupportedProfile(
            "shaper linking requires an XYZ or Lab PCS",
        )),
    }
}

/// The shared tail of [`device_to_pcs`]/[`pcs_to_device`]: LUT tag if any (with the intent
/// fallback already applied by the caller), else the shaper set, else the appropriate error.
fn dispatch(
    profile: &IccProfile,
    intent: RenderingIntent,
    table: &[KnownTag; 4],
    direction: Direction,
) -> Result<Pipeline> {
    if let Some((tag, data)) = select_lut_tag(profile, table, intent) {
        return lut::build(tag.into(), data, direction, profile.header.pcs);
    }
    check_pcs(profile)?;
    match (profile.header.data_color_space, direction) {
        (ColorSpace::Rgb, Direction::DeviceToPcs) => shaper::rgb_device_to_pcs(profile),
        (ColorSpace::Rgb, Direction::PcsToDevice) => shaper::rgb_pcs_to_device(profile),
        (ColorSpace::Gray, Direction::DeviceToPcs) => shaper::gray_device_to_pcs(profile),
        (ColorSpace::Gray, Direction::PcsToDevice) => shaper::gray_pcs_to_device(profile),
        // No LUT tags and no shaper-capable device space: report the intent's primary tag —
        // the first tag the lcms2 dispatch would have looked for.
        _ => Err(CmmError::MissingTag(table[intent_index(intent)].into())),
    }
}

/// Builds the device → decoded-PCS half of a conversion for `profile` at `intent`.
///
/// The pipeline consumes one pixel of encoded `[0, 1]` device channels and produces decoded
/// PCS colorimetry (XYZ with D50 `Y = 1.0`, or Lab with `L*` in `0..=100`). The intent
/// selects the LUT tag per lcms2's `Device2PCS16` table with the perceptual fallback (module
/// docs): `A2B0`/`A2B1`/`A2B2` for perceptual/media-relative/saturation, with ICC-absolute
/// reusing `A2B1` — the absolute white-point scaling is a *pair* concern applied at the PCS
/// seam by [`IccTransform::between`](crate::IccTransform::between), so this half builds the
/// same pipeline for absolute as for media-relative. A profile without LUT tags falls back
/// to the matrix/TRC shaper set (RGB or gray; intent-invariant — a shaper profile has no
/// per-intent tables).
///
/// LUT tags whose "PCS" is itself a device space (a devicelink/abstract profile) build with
/// no PCS seam: the pipeline runs encoded `[0, 1]` end to end.
///
/// # Errors
///
/// [`CmmError::BadTagType`] for a LUT tag holding a non-LUT element;
/// [`CmmError::MissingTag`] when neither LUT nor shaper tags exist (the intent's primary LUT
/// tag for non-RGB/gray device spaces, the first missing colorant/TRC tag otherwise);
/// [`CmmError::UnsupportedProfile`] for a shaper profile whose PCS is neither XYZ nor Lab;
/// [`CmmError::StageChannelMismatch`]/[`CmmError::PipelineEndsMismatch`] for a LUT element
/// whose stage channel counts cannot chain; and any
/// [`ToneCurve::new`](crate::ToneCurve::new) or [`ClutTable`](crate::ClutTable) construction
/// error for malformed curve or CLUT data.
pub fn device_to_pcs(profile: &IccProfile, intent: RenderingIntent) -> Result<Pipeline> {
    dispatch(profile, intent, &DEVICE_TO_PCS_16, Direction::DeviceToPcs)
}

/// Builds the decoded-PCS → device half of a conversion for `profile` at `intent`.
///
/// The mirror of [`device_to_pcs`]: consumes decoded PCS colorimetry, produces encoded
/// `[0, 1]` device channels, selecting `B2A0`/`B2A1`/`B2A2` per lcms2's `PCS2Device16`
/// table (absolute reuses `B2A1`; perceptual fallback as in the module docs). CLUTs of a
/// Lab-PCS profile interpolate trilinearly in this direction (lcms2's Lab-indexed-CLUT
/// rule, `ChangeInterpolationToTrilinear`).
///
/// # Errors
///
/// As [`device_to_pcs`], plus — on the shaper fallback path —
/// [`CmmError::SingularMatrix`] if the colorant matrix has no finite inverse and
/// [`CmmError::NonMonotonicCurve`] if a TRC has no functional inverse.
pub fn pcs_to_device(profile: &IccProfile, intent: RenderingIntent) -> Result<Pipeline> {
    dispatch(profile, intent, &PCS_TO_DEVICE_16, Direction::PcsToDevice)
}

#[cfg(test)]
mod tests {
    use gamut_icc::{
        Clut, ClutPrecision, ColorSpace, Curve, CurveOrParametric, DeviceClass, IccProfile,
        LutAToB, ProfileHeader, Signature, TagData,
    };

    use super::*;

    /// A profile skeleton over the given spaces (no tags at all).
    fn bare_profile(device: ColorSpace, pcs: ColorSpace) -> IccProfile {
        let mut header = ProfileHeader::new(DeviceClass::Output, device);
        header.pcs = pcs;
        IccProfile {
            header,
            tags: Vec::new(),
        }
    }

    fn identity_curves(n: usize) -> Vec<CurveOrParametric> {
        vec![CurveOrParametric::Curve(Curve::Identity); n]
    }

    /// A minimal 4→3 `mAB ` element whose CLUT is constant at `value` (2 grid nodes per
    /// axis, 16-bit), so pipelines built from different tags are distinguishable by output.
    fn constant_mab(value: u16) -> TagData {
        TagData::LutAToB(LutAToB {
            input_channels: 4,
            output_channels: 3,
            a_curves: None,
            clut: Some(Clut {
                grid_points: vec![2; 4],
                output_channels: 3,
                precision: ClutPrecision::U16,
                samples: vec![value; 16 * 3],
            }),
            m_curves: None,
            matrix: None,
            b_curves: identity_curves(3),
        })
    }

    /// A CMYK→Lab profile carrying the given `(signature, element)` LUT tags.
    fn cmyk_profile(tags: Vec<(Signature, TagData)>) -> IccProfile {
        let mut profile = bare_profile(ColorSpace::Cmyk, ColorSpace::Lab);
        profile.tags = tags;
        profile
    }

    fn eval3(pipeline: &Pipeline, input: &[f64]) -> [f64; 3] {
        let mut out = [0.0; 3];
        pipeline.eval(input, &mut out).unwrap();
        out
    }

    #[test]
    fn intent_selects_its_own_tag_and_absolute_reuses_relative() {
        // Three distinct per-intent A2B tags: each intent must pick its own (distinguishable
        // by the constant CLUT value), and absolute must land on the *relative* tag.
        let profile = cmyk_profile(vec![
            (Signature(*b"A2B0"), constant_mab(0)),
            (Signature(*b"A2B1"), constant_mab(32768)),
            (Signature(*b"A2B2"), constant_mab(65535)),
        ]);
        let l_of = |intent| {
            let pipeline = device_to_pcs(&profile, intent).unwrap();
            eval3(&pipeline, &[0.5; 4])[0]
        };
        // Constant CLUT c decodes to L* = (c/65535)·100 through the v4 Lab seam (exact
        // expected values computed by the same float expression).
        let l = |c: u16| f64::from(c) / 65535.0 * 100.0;
        assert_eq!(l_of(RenderingIntent::Perceptual), l(0));
        assert_eq!(l_of(RenderingIntent::MediaRelativeColorimetric), l(32768));
        assert_eq!(l_of(RenderingIntent::Saturation), l(65535));
        // Absolute == relative: the SAME tag (lcms2's Device2PCS16[3] = A2B1).
        assert_eq!(l_of(RenderingIntent::IccAbsoluteColorimetric), l(32768));
    }

    #[test]
    fn absent_intent_tag_falls_back_to_perceptual() {
        // Only A2B0 exists: every intent must resolve to it (lcms2's tag16 fallback).
        let profile = cmyk_profile(vec![(Signature(*b"A2B0"), constant_mab(32768))]);
        for intent in [
            RenderingIntent::Perceptual,
            RenderingIntent::MediaRelativeColorimetric,
            RenderingIntent::Saturation,
            RenderingIntent::IccAbsoluteColorimetric,
        ] {
            let pipeline = device_to_pcs(&profile, intent).unwrap();
            assert_eq!(
                eval3(&pipeline, &[0.25; 4])[0],
                32768.0 / 65535.0 * 100.0,
                "intent {intent:?}"
            );
        }
    }

    #[test]
    fn missing_lut_tags_on_non_shaper_space_report_the_primary_tag() {
        // A bare CMYK profile has no LUT tags and cannot fall back to a shaper: the error
        // must name the *requested intent's* primary tag, per direction.
        let profile = bare_profile(ColorSpace::Cmyk, ColorSpace::Lab);
        let cases = [
            (RenderingIntent::Perceptual, *b"A2B0", *b"B2A0"),
            (
                RenderingIntent::MediaRelativeColorimetric,
                *b"A2B1",
                *b"B2A1",
            ),
            (RenderingIntent::Saturation, *b"A2B2", *b"B2A2"),
            // Absolute's primary tag IS the relative one (table index 3).
            (RenderingIntent::IccAbsoluteColorimetric, *b"A2B1", *b"B2A1"),
        ];
        for (intent, a2b, b2a) in cases {
            let err = device_to_pcs(&profile, intent).unwrap_err();
            assert_eq!(
                err.to_string(),
                format!(
                    "cmm: profile is missing required tag {}",
                    core::str::from_utf8(&a2b).unwrap()
                ),
                "device_to_pcs {intent:?}"
            );
            let err = pcs_to_device(&profile, intent).unwrap_err();
            assert_eq!(
                err.to_string(),
                format!(
                    "cmm: profile is missing required tag {}",
                    core::str::from_utf8(&b2a).unwrap()
                ),
                "pcs_to_device {intent:?}"
            );
        }
    }

    #[test]
    fn lut_tag_with_non_lut_element_is_a_bad_tag_type() {
        // A LUT tag holding a curve (or raw bytes, e.g. a float DToBx-style mpet payload
        // stored under A2B0) is BadTagType with the tag's signature — LUT precedence still
        // holds, so the shaper tags a profile may also carry are never silently used.
        for data in [
            TagData::Curve(Curve::Identity),
            TagData::Raw {
                type_sig: Signature(*b"mpet"),
                bytes: Vec::new(),
            },
        ] {
            let mut profile = bare_profile(ColorSpace::Rgb, ColorSpace::Xyz);
            profile.tags = vec![(Signature(*b"A2B0"), data.clone())];
            let err = device_to_pcs(&profile, RenderingIntent::Perceptual).unwrap_err();
            assert_eq!(
                err.to_string(),
                "cmm: tag A2B0 holds an unusable element type"
            );
            // The opposite direction ignores A2B tags and proceeds to the shaper fallback
            // (which then reports its own missing colorant tag).
            let err = pcs_to_device(&profile, RenderingIntent::Perceptual).unwrap_err();
            assert!(matches!(err, CmmError::MissingTag(_)), "got {err}");
        }
    }

    #[test]
    fn directions_use_their_own_tag_family() {
        // A B2A0-only profile: pcs_to_device builds from it, device_to_pcs must not (and
        // reports its own family's missing tag).
        let b2a = TagData::LutBToA(gamut_icc::LutBToA {
            input_channels: 3,
            output_channels: 4,
            b_curves: identity_curves(3),
            matrix: None,
            m_curves: None,
            clut: Some(Clut {
                grid_points: vec![2; 3],
                output_channels: 4,
                precision: ClutPrecision::U16,
                samples: vec![32768; 8 * 4],
            }),
            a_curves: None,
        });
        let profile = cmyk_profile(vec![(Signature(*b"B2A0"), b2a)]);
        assert!(pcs_to_device(&profile, RenderingIntent::Perceptual).is_ok());
        let err = device_to_pcs(&profile, RenderingIntent::Perceptual).unwrap_err();
        assert_eq!(err.to_string(), "cmm: profile is missing required tag A2B0");
    }

    #[test]
    fn shaper_fallback_still_applies_without_lut_tags() {
        // A gray shaper with no LUT tags builds under every intent, and the built pipelines
        // are identical (shaper profiles are intent-invariant; per-intent renderings only
        // exist in LUT tags, and the absolute white scaling is applied by
        // IccTransform::between, not by a single profile's half).
        let mut profile = bare_profile(ColorSpace::Gray, ColorSpace::Xyz);
        profile.tags.push((
            Signature(*b"kTRC"),
            TagData::Curve(Curve::Gamma(gamut_icc::U8Fixed8(0x0233))),
        ));
        let baseline = device_to_pcs(&profile, RenderingIntent::MediaRelativeColorimetric).unwrap();
        for intent in [
            RenderingIntent::Perceptual,
            RenderingIntent::Saturation,
            RenderingIntent::IccAbsoluteColorimetric,
        ] {
            let other = device_to_pcs(&profile, intent).unwrap();
            for g in [0.0, 0.25, 0.5, 1.0] {
                let (mut a, mut b) = ([0.0; 3], [0.0; 3]);
                baseline.eval(&[g], &mut a).unwrap();
                other.eval(&[g], &mut b).unwrap();
                assert_eq!(a, b, "intent {intent:?} diverged at {g}");
            }
        }
    }

    #[test]
    fn lut_tags_take_precedence_over_shaper_tags() {
        // A profile carrying BOTH a usable shaper set and an A2B0 LUT: the LUT wins (lcms2
        // consults the intent tables before the matrix-shaper fallback), observable because
        // the constant CLUT ignores its input where the shaper would not.
        let xyz_tag = |v: f64| TagData::Xyz(vec![gamut_icc::XyzNumber::from_f64([v, v, v])]);
        let trc = || TagData::Curve(Curve::Identity);
        let lut = TagData::LutAToB(LutAToB {
            input_channels: 3,
            output_channels: 3,
            a_curves: None,
            clut: Some(Clut {
                grid_points: vec![2; 3],
                output_channels: 3,
                precision: ClutPrecision::U16,
                samples: vec![13107; 8 * 3],
            }),
            m_curves: None,
            matrix: None,
            b_curves: identity_curves(3),
        });
        let mut profile = bare_profile(ColorSpace::Rgb, ColorSpace::Xyz);
        profile.tags = vec![
            (Signature(*b"rXYZ"), xyz_tag(0.25)),
            (Signature(*b"gXYZ"), xyz_tag(0.5)),
            (Signature(*b"bXYZ"), xyz_tag(0.125)),
            (Signature(*b"rTRC"), trc()),
            (Signature(*b"gTRC"), trc()),
            (Signature(*b"bTRC"), trc()),
            (Signature(*b"A2B0"), lut),
        ];
        let pipeline = device_to_pcs(&profile, RenderingIntent::Perceptual).unwrap();
        // Constant CLUT through the XYZ decode: (13107/65535)·(65535/32768) for ANY input —
        // a shaper pipeline could not produce identical output at two distinct inputs.
        let want = 13107.0 / 65535.0 * (65535.0 / 32768.0);
        for input in [[0.0, 0.0, 0.0], [1.0, 0.3, 0.7]] {
            for (ch, &v) in eval3(&pipeline, &input).iter().enumerate() {
                assert_eq!(v, want, "input {input:?} channel {ch}");
            }
        }
    }

    #[test]
    fn non_connection_space_pcs_without_lut_tags_is_refused() {
        // A device-link-style header (device space in the PCS field) without LUT tags cannot
        // be a shaper seam.
        let profile = bare_profile(ColorSpace::Rgb, ColorSpace::Rgb);
        for build in [device_to_pcs, pcs_to_device] {
            let err = build(&profile, RenderingIntent::MediaRelativeColorimetric).unwrap_err();
            assert_eq!(
                err.to_string(),
                "cmm: unsupported profile (shaper linking requires an XYZ or Lab PCS)"
            );
        }
    }

    #[test]
    fn devicelink_lut_builds_with_no_pcs_seam() {
        // An RGB→RGB devicelink: the A2B0 pipeline has no PCS end, so it runs encoded [0, 1]
        // end to end — a constant CLUT of 32768 yields exactly 0.5 (no ×100 Lab decode, no
        // ×1.99997 XYZ decode).
        let link = TagData::LutAToB(LutAToB {
            input_channels: 3,
            output_channels: 3,
            a_curves: None,
            clut: Some(Clut {
                grid_points: vec![2; 3],
                output_channels: 3,
                precision: ClutPrecision::U16,
                samples: vec![32768; 8 * 3],
            }),
            m_curves: None,
            matrix: None,
            b_curves: identity_curves(3),
        });
        let mut profile = bare_profile(ColorSpace::Rgb, ColorSpace::Rgb);
        profile.header.device_class = DeviceClass::DeviceLink;
        profile.tags = vec![(Signature(*b"A2B0"), link)];
        let pipeline = device_to_pcs(&profile, RenderingIntent::Perceptual).unwrap();
        let out = eval3(&pipeline, &[0.3, 0.6, 0.9]);
        for ch in 0..3 {
            assert!((out[ch] - 32768.0 / 65535.0).abs() < 1e-12, "{out:?}");
        }
    }
}
