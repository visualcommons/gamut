//! Multi-profile transform chaining, device-link transforms, and soft-proofing — lcms2's
//! `DefaultICCintents` (`cmscnvrt.c:510-645`) transcribed into the crate's decoded-seam
//! convention, as **the one seam implementation** every [`IccTransform`] constructor uses
//! ([`IccTransform::between`] is the two-profile chain).
//!
//! # The chaining algorithm (lcms2 verbatim, decoded seams)
//!
//! The chain tracks a *current colour space*, starting at the first profile's device space.
//! Each profile then contributes one hop:
//!
//! - a **device-link or abstract** profile is read devicelink-style
//!   ([`crate::link`]'s `_cmsReadDevicelinkLUT` transcription): its A2B family in whatever
//!   direction it points, no shaper fallback. An abstract profile after the first slot also
//!   gets the `ComputeConversion` adjustment below;
//! - otherwise the profile is used in the **input direction**
//!   ([`device_to_pcs`](crate::link::device_to_pcs)) when the current space is not a PCS
//!   (always true for the first profile), and in the **output direction**
//!   ([`pcs_to_device`](crate::link::pcs_to_device)) when it is. Only output-direction hops
//!   get the `ComputeConversion` adjustment — the per-hop intent's ICC-absolute white
//!   scaling, or black-point compensation (requested or v4-forced), computed between the
//!   *previous* profile and this one;
//! - the hop's entry space must be compatible with the current space (lcms2's
//!   `ColorSpaceIsCompatible`: equal, the `4CLR`≡`CMYK` alias, or both connection spaces) —
//!   else [`CmmError::ChainMismatch`] — and any PCS mismatch is bridged per `AddConversion`:
//!   the adjustment matrix acts in XYZ, Lab ends get [`Stage::LabToXyz`]/[`Stage::XyzToLab`]
//!   bridges, and an adjustment within the empty-layer tolerance inserts no stage at all.
//!
//! Intents and BPC flags are **per-hop arrays** internally (what
//! [`IccTransform::proofing`]'s asymmetric hops need); the public [`IccTransform::chain`]
//! takes a single [`TransformOptions`] and replicates it per hop — exactly what lcms2's
//! `cmsCreateMultiprofileTransform` does with its single intent/flag set, so differentials
//! against that entry point are like-for-like. Per-hop intent arrays as *public* API
//! (lcms2's `cmsCreateExtendedTransform`) are deferred (STATUS.md).

use gamut_icc::{ColorSpace, DeviceClass, IccProfile, RenderingIntent};

use crate::error::{CmmError, Result};
use crate::pipeline::{Pipeline, Stage};
use crate::transform::{IccTransform, TransformOptions, is_empty_layer};
use crate::{bpc, intent, link};

/// The identity adjustment layer (skipped by the empty-layer test).
const IDENTITY: ([[f64; 3]; 3], [f64; 3]) = (
    [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
    [0.0; 3],
);

/// The PCS-seam adjustment for one output-direction (or abstract) hop — lcms2's
/// `ComputeConversion` (`cmscnvrt.c:352-418`) at adaptation state 1.0, in the decoded domain
/// (no `MAX_ENCODEABLE_XYZ` division of the offset — the pipelines here carry decoded XYZ):
/// absolute → the white scaling; otherwise BPC when requested **or forced** (the v4
/// perceptual/saturation rule, keyed on the hop's own profile `dst` — lcms2's
/// `_cmsLinkProfiles` forcing consumed at the output-direction slot); else identity.
pub(crate) fn conversion_layer(
    src: &IccProfile,
    dst: &IccProfile,
    hop_intent: RenderingIntent,
    black_point_compensation: bool,
) -> ([[f64; 3]; 3], [f64; 3]) {
    if hop_intent == RenderingIntent::IccAbsoluteColorimetric {
        // BPC and absolute are mutually exclusive: the flag is ignored here.
        return (intent::absolute_scaling(src, dst), [0.0; 3]);
    }
    // The v4 forcing rule (_cmsLinkProfiles, cmscnvrt.c:1119-1135): BPC "applies always on
    // V4 perceptual and saturation", keyed per profile slot — and only the slot whose hop
    // computes a conversion consumes its flag, so the hop's own (destination-side) profile
    // gates the force.
    let forced = matches!(
        hop_intent,
        RenderingIntent::Perceptual | RenderingIntent::Saturation
    ) && dst.header.version.major >= 4;
    if black_point_compensation || forced {
        let black_in = bpc::detect_black_point(src, hop_intent);
        let black_out = bpc::detect_destination_black_point(dst, hop_intent);
        if let Some((m, off)) = bpc::compensation(black_in, black_out) {
            return (m, off);
        }
    }
    IDENTITY
}

/// lcms2's `ColorSpaceIsCompatible` (`cmscnvrt.c:492-507`): equal spaces, the `4CLR`≡`CMYK`
/// substitution, or the two connection spaces (bridgeable one into the other).
fn compatible(a: ColorSpace, b: ColorSpace) -> bool {
    a == b
        || matches!(
            (a, b),
            (ColorSpace::NColor(4), ColorSpace::Cmyk) | (ColorSpace::Cmyk, ColorSpace::NColor(4))
        )
        || matches!(
            (a, b),
            (
                ColorSpace::Xyz | ColorSpace::Lab,
                ColorSpace::Xyz | ColorSpace::Lab
            )
        )
}

/// lcms2's `AddConversion` (`cmscnvrt.c:420-489`) in the decoded domain: the stages bridging
/// the chain's `current` space into the next hop's `entry` space, with the adjustment
/// `(m, off)` acting in XYZ. An empty layer (per [`is_empty_layer`], offset compared in
/// lcms2's encoded scale) inserts no matrix — and for Lab→Lab nothing at all.
///
/// # Errors
///
/// [`CmmError::ChainMismatch`] when the spaces cannot bridge: a connection space against a
/// device space, or two distinct device spaces (lcms2's `AddConversion` default arm — which
/// notably rejects even the `4CLR`≡`CMYK` alias [`compatible`] admits, a quirk kept as-is).
fn pcs_seam(
    current: ColorSpace,
    entry: ColorSpace,
    m: [[f64; 3]; 3],
    off: [f64; 3],
) -> Result<Vec<Stage>> {
    let empty = is_empty_layer(&m, &off);
    let adjust = Stage::Matrix { m, offset: off };
    Ok(match (current, entry) {
        (ColorSpace::Xyz, ColorSpace::Xyz) => {
            if empty {
                Vec::new()
            } else {
                vec![adjust]
            }
        }
        (ColorSpace::Xyz, ColorSpace::Lab) => {
            let mut stages = Vec::new();
            if !empty {
                stages.push(adjust);
            }
            stages.push(Stage::XyzToLab);
            stages
        }
        (ColorSpace::Lab, ColorSpace::Xyz) => {
            let mut stages = vec![Stage::LabToXyz];
            if !empty {
                stages.push(adjust);
            }
            stages
        }
        (ColorSpace::Lab, ColorSpace::Lab) => {
            if empty {
                Vec::new()
            } else {
                vec![Stage::LabToXyz, adjust, Stage::XyzToLab]
            }
        }
        _ => {
            if current == entry {
                Vec::new()
            } else {
                return Err(CmmError::ChainMismatch(
                    "device-space hop cannot bridge distinct colour spaces",
                ));
            }
        }
    })
}

/// The chain engine: composes `profiles` into one pipeline with per-hop `intents` and
/// `bpc_flags` (all three slices share their length; the crate-internal callers guarantee
/// it). See the module docs for the transcribed algorithm.
///
/// # Errors
///
/// [`CmmError::ChainMismatch`] for fewer than two profiles — checked here rather than left to
/// the callers, so the shortest chain is rejected before the first profile is read instead of
/// panicking on the index. Otherwise whatever the per-hop builders and the seam raise.
pub(crate) fn link_chain(
    profiles: &[&IccProfile],
    intents: &[RenderingIntent],
    bpc_flags: &[bool],
) -> Result<Pipeline> {
    debug_assert_eq!(profiles.len(), intents.len());
    debug_assert_eq!(profiles.len(), bpc_flags.len());
    let Some(first) = profiles.first() else {
        return Err(CmmError::ChainMismatch(
            "a transform chain needs at least two profiles",
        ));
    };
    let mut current_space = first.header.data_color_space;
    let mut result: Option<Pipeline> = None;
    for (i, &profile) in profiles.iter().enumerate() {
        let class = profile.header.device_class;
        let link_like = matches!(class, DeviceClass::DeviceLink | DeviceClass::Abstract);
        // First profile is used as input unless devicelink/abstract; later profiles are
        // input-direction exactly when the chain currently carries a device space.
        let is_input = if i == 0 && !link_like {
            true
        } else {
            !matches!(current_space, ColorSpace::Xyz | ColorSpace::Lab)
        };
        let hop_intent = intents[i];
        let (entry_space, exit_space) = if link_like || is_input {
            (profile.header.data_color_space, profile.header.pcs)
        } else {
            (profile.header.pcs, profile.header.data_color_space)
        };
        if !compatible(entry_space, current_space) {
            return Err(CmmError::ChainMismatch(
                "profile's entry colour space does not match the space the chain carries",
            ));
        }
        let hop = if link_like {
            let lut = link::device_link_pipeline(profile, hop_intent)?;
            // Abstract profiles after the first slot get the conversion layer; device links
            // (and any first-slot profile) do not (DefaultICCintents, cmscnvrt.c:583-594).
            let (m, off) = if class == DeviceClass::Abstract && i > 0 {
                conversion_layer(profiles[i - 1], profile, hop_intent, bpc_flags[i])
            } else {
                IDENTITY
            };
            with_seam(pcs_seam(current_space, entry_space, m, off)?, lut)?
        } else if is_input {
            link::device_to_pcs(profile, hop_intent)?
        } else {
            let lut = link::pcs_to_device(profile, hop_intent)?;
            let (m, off) = conversion_layer(profiles[i - 1], profile, hop_intent, bpc_flags[i]);
            with_seam(pcs_seam(current_space, entry_space, m, off)?, lut)?
        };
        result = Some(match result {
            None => hop,
            Some(chain) => chain.compose(hop)?,
        });
        current_space = exit_space;
    }
    // The empty case returned above, so the fold ran at least once.
    result.ok_or(CmmError::ChainMismatch(
        "a transform chain needs at least two profiles",
    ))
}

/// Prepends the (3→3) seam stages, if any, to a hop's pipeline.
fn with_seam(seam: Vec<Stage>, lut: Pipeline) -> Result<Pipeline> {
    if seam.is_empty() {
        return Ok(lut);
    }
    Pipeline::new(3, 3, seam)?.compose(lut)
}

/// Rejects the one profile class no chain hop can evaluate.
fn reject_named_color(profile: &IccProfile) -> Result<()> {
    if profile.header.device_class == DeviceClass::NamedColor {
        return Err(CmmError::UnsupportedProfile(
            "named-colour profiles have no continuous pixel transform",
        ));
    }
    Ok(())
}

/// Options for building an [`IccTransform::proofing`] transform.
///
/// The three knobs of lcms2's `cmsCreateProofingTransform`: `intent` renders source colours
/// onto the simulated (proof) device, `proofing_intent` renders the simulation onto the
/// actual destination, and `black_point_compensation` applies to the source→proof leg only
/// (as in lcms2 — BPC on the return legs would double-compensate).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProofingOptions {
    /// The rendering intent for the source→proof simulation (lcms2's `nIntent` — the intent
    /// whose rendition is being previewed).
    pub intent: RenderingIntent,
    /// The rendering intent for bringing the simulated colours to the destination device
    /// (lcms2's `ProofingIntent`; typically media-relative or ICC-absolute for a faithful
    /// preview).
    pub proofing_intent: RenderingIntent,
    /// Apply black-point compensation on the source→proof leg (hops 1–2 of the internal
    /// chain; the proof→destination legs never compensate, as in lcms2's
    /// `cmsCreateProofingTransform` BPC array).
    pub black_point_compensation: bool,
}

impl IccTransform {
    /// Links two or more profiles into one chained transform at a single intent —
    /// lcms2's `cmsCreateMultiprofileTransform` (which likewise replicates its one intent
    /// and BPC flag across every hop; see the module docs for the transcribed per-hop
    /// algorithm and the deferred per-hop-array API).
    ///
    /// `profiles[0]`'s device space is the transform input; the output space is wherever the
    /// last hop lands (a chain ending on an input-direction or link hop ends in that
    /// profile's PCS/output space — lcms2's semantics). Device-link and abstract profiles
    /// are read devicelink-style at any position; each output-direction hop applies the
    /// intent's PCS-seam adjustment (ICC-absolute white scaling, or BPC — requested or
    /// v4-forced) between its profile and the previous one, exactly as
    /// [`IccTransform::between`] does for a pair (`between` **is** the two-profile chain).
    ///
    /// # Errors
    ///
    /// [`CmmError::ChainMismatch`] for fewer than two profiles or a profile whose entry
    /// space does not connect to the space the chain carries at that hop;
    /// [`CmmError::UnsupportedProfile`] for a named-colour profile; otherwise whatever the
    /// per-hop builders raise ([`device_to_pcs`](crate::link::device_to_pcs) /
    /// [`pcs_to_device`](crate::link::pcs_to_device), or the devicelink read — which has
    /// **no** shaper fallback and reports missing A2B tags as [`CmmError::MissingTag`]).
    pub fn chain(profiles: &[&IccProfile], options: TransformOptions) -> Result<Self> {
        if profiles.len() < 2 {
            return Err(CmmError::ChainMismatch(
                "a transform chain needs at least two profiles",
            ));
        }
        for profile in profiles {
            reject_named_color(profile)?;
        }
        let intents = vec![options.intent; profiles.len()];
        let bpc_flags = vec![options.black_point_compensation; profiles.len()];
        Ok(Self::from_pipeline(link_chain(
            profiles, &intents, &bpc_flags,
        )?))
    }

    /// Builds the transform a **device-link profile** embodies: its A2B pipeline from its
    /// device space to its "PCS" header field (which for a link profile holds the *output*
    /// device space) — lcms2's one-profile `cmsCreateTransform` spelling over
    /// `_cmsReadDevicelinkLUT`.
    ///
    /// The A2B tag is selected by `intent` with the perceptual fallback; there is
    /// deliberately **no** matrix/TRC shaper fallback (a link without a usable A2B tag
    /// fails), no PCS-seam adjustment (a link is a finished rendering — intent math was
    /// baked in by whoever built it), and CLUTs interpolate trilinearly when the link's
    /// output space is Lab. Connection-space ends (an RGB→Lab link, say) carry the crate's
    /// decoded colorimetry; device ends are encoded `[0, 1]`.
    ///
    /// # Errors
    ///
    /// [`CmmError::UnsupportedProfile`] when `link`'s class is not
    /// [`DeviceClass::DeviceLink`] (abstract profiles chain via [`IccTransform::chain`]);
    /// [`CmmError::MissingTag`] when neither the intent's A2B tag nor `A2B0` exists;
    /// otherwise the usual LUT construction errors.
    pub fn device_link(link: &IccProfile, intent: RenderingIntent) -> Result<Self> {
        if link.header.device_class != DeviceClass::DeviceLink {
            return Err(CmmError::UnsupportedProfile(
                "device_link requires a link-class profile",
            ));
        }
        Ok(Self::from_pipeline(link::device_link_pipeline(
            link, intent,
        )?))
    }

    /// Builds a soft-proofing transform: `src` device → `dst` device, rendered **as the
    /// `proof` device would show it** — lcms2's `cmsCreateProofingTransform` with
    /// `cmsFLAGS_SOFTPROOFING`, which is exactly the four-profile chain
    /// `[src, proof, proof, dst]` at per-hop intents `[intent, intent, media-relative,
    /// proofing_intent]` with BPC on the first two hops only (`cmsxform.c:1365-1395`): the
    /// source is rendered onto the proof device at `options.intent`, read back
    /// colorimetrically (media-relative), and carried to the destination at
    /// `options.proofing_intent`.
    ///
    /// # Errors
    ///
    /// As [`IccTransform::chain`] over the four-profile chain (named-colour profiles
    /// rejected up front; `proof` is read in both directions, so it needs both an A2B and a
    /// B2A rendition — LUT tags or a shaper set).
    pub fn proofing(
        src: &IccProfile,
        dst: &IccProfile,
        proof: &IccProfile,
        options: ProofingOptions,
    ) -> Result<Self> {
        for profile in [src, dst, proof] {
            reject_named_color(profile)?;
        }
        let profiles = [src, proof, proof, dst];
        let intents = [
            options.intent,
            options.intent,
            RenderingIntent::MediaRelativeColorimetric,
            options.proofing_intent,
        ];
        let bpc_flags = [
            options.black_point_compensation,
            options.black_point_compensation,
            false,
            false,
        ];
        Ok(Self::from_pipeline(link_chain(
            &profiles, &intents, &bpc_flags,
        )?))
    }
}

#[cfg(test)]
mod tests {
    use gamut_icc::{
        Curve, CurveOrParametric, IccProfile, LutAToB, ProfileHeader, Signature, TagData, U8Fixed8,
        XyzNumber,
    };

    use super::*;
    use crate::transform::Transform as _;

    const D65: [f64; 3] = [0.9504, 1.0, 1.0889];

    /// A v4 RGB→XYZ matrix/TRC display shaper over exact-dyadic colorants.
    fn rgb_shaper(wtpt: Option<[f64; 3]>) -> IccProfile {
        let xyz_tag = |v: [f64; 3]| TagData::Xyz(vec![XyzNumber::from_f64(v)]);
        let gamma = || TagData::Curve(Curve::Gamma(U8Fixed8(0x0233)));
        let mut tags = vec![
            (Signature(*b"rXYZ"), xyz_tag([0.5, 0.25, 0.0625])),
            (Signature(*b"gXYZ"), xyz_tag([0.375, 0.625, 0.125])),
            (Signature(*b"bXYZ"), xyz_tag([0.125, 0.125, 0.625])),
            (Signature(*b"rTRC"), gamma()),
            (Signature(*b"gTRC"), gamma()),
            (Signature(*b"bTRC"), gamma()),
        ];
        if let Some(white) = wtpt {
            tags.push((Signature(*b"wtpt"), xyz_tag(white)));
        }
        IccProfile {
            header: ProfileHeader::new(DeviceClass::Display, ColorSpace::Rgb),
            tags,
        }
    }

    /// A Lab→Lab abstract identity profile (the shape of lcms2's `cmsCreateLab4Profile`):
    /// A2B0 holds identity B-curves only.
    fn abstract_lab() -> IccProfile {
        let mut header = ProfileHeader::new(DeviceClass::Abstract, ColorSpace::Lab);
        header.pcs = ColorSpace::Lab;
        IccProfile {
            header,
            tags: vec![(
                Signature(*b"A2B0"),
                TagData::LutAToB(LutAToB {
                    input_channels: 3,
                    output_channels: 3,
                    a_curves: None,
                    clut: None,
                    m_curves: None,
                    matrix: None,
                    b_curves: vec![CurveOrParametric::Curve(Curve::Identity); 3],
                }),
            )],
        }
    }

    /// An RGB→RGB device-link profile whose A2B0 halves every channel (2-node CLUT).
    fn halving_link() -> IccProfile {
        let mut header = ProfileHeader::new(DeviceClass::DeviceLink, ColorSpace::Rgb);
        header.pcs = ColorSpace::Rgb;
        let mut samples = Vec::new();
        for r in 0..2u16 {
            for g in 0..2u16 {
                for b in 0..2u16 {
                    samples.extend([r * 32768, g * 32768, b * 32768]);
                }
            }
        }
        IccProfile {
            header,
            tags: vec![(
                Signature(*b"A2B0"),
                TagData::LutAToB(LutAToB {
                    input_channels: 3,
                    output_channels: 3,
                    a_curves: None,
                    clut: Some(gamut_icc::Clut {
                        grid_points: vec![2; 3],
                        output_channels: 3,
                        precision: gamut_icc::ClutPrecision::U16,
                        samples,
                    }),
                    m_curves: None,
                    matrix: None,
                    b_curves: vec![CurveOrParametric::Curve(Curve::Identity); 3],
                }),
            )],
        }
    }

    fn eval3(transform: &IccTransform, input: &[f64]) -> [f64; 3] {
        let mut out = [0.0; 3];
        transform.transform(input, &mut out).unwrap();
        out
    }

    /// `link_chain` is crate-internal and every caller checks the count first, so nothing
    /// reaches it empty today — but it used to read `profiles[0]` before the guard that reports
    /// the too-short chain, which made that error unreachable and an empty slice a panic in a
    /// library path. The guard now comes first, and this pins it directly rather than through a
    /// caller that would have rejected the input anyway.
    #[test]
    fn an_empty_chain_is_reported_not_panicked() {
        let err = link_chain(&[], &[], &[]).expect_err("an empty chain has nothing to link");
        assert!(
            matches!(err, CmmError::ChainMismatch(m) if m.contains("at least two profiles")),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn chain_needs_at_least_two_profiles() {
        let p = rgb_shaper(None);
        for profiles in [Vec::new(), vec![&p]] {
            let err = IccTransform::chain(&profiles, TransformOptions::default()).unwrap_err();
            assert_eq!(
                err.to_string(),
                "cmm: profile chain mismatch (a transform chain needs at least two profiles)"
            );
        }
    }

    #[test]
    fn chain_rejects_named_colour_profiles() {
        let good = rgb_shaper(None);
        let mut named = rgb_shaper(None);
        named.header.device_class = DeviceClass::NamedColor;
        let err = IccTransform::chain(&[&good, &named], TransformOptions::default()).unwrap_err();
        assert_eq!(
            err.to_string(),
            "cmm: unsupported profile (named-colour profiles have no continuous pixel transform)"
        );
    }

    #[test]
    fn two_profile_chain_is_exactly_between() {
        // THE refactor guarantee: `between` == the two-profile chain, stage for stage and
        // value for value, across intents and BPC states.
        let src = rgb_shaper(Some(D65));
        let dst = rgb_shaper(None);
        for intent in [
            RenderingIntent::Perceptual,
            RenderingIntent::MediaRelativeColorimetric,
            RenderingIntent::Saturation,
            RenderingIntent::IccAbsoluteColorimetric,
        ] {
            for black_point_compensation in [false, true] {
                let options = TransformOptions {
                    intent,
                    black_point_compensation,
                };
                let between = IccTransform::between(&src, &dst, options).unwrap();
                let chain = IccTransform::chain(&[&src, &dst], options).unwrap();
                for rgb in [[0.0; 3], [1.0; 3], [0.3, 0.6, 0.9]] {
                    assert_eq!(
                        eval3(&between, &rgb),
                        eval3(&chain, &rgb),
                        "{intent:?} bpc={black_point_compensation}"
                    );
                }
            }
        }
    }

    #[test]
    fn abstract_identity_mid_chain_is_transparent_at_relative() {
        // [src, Lab-identity abstract, dst] at media-relative: the abstract hop bridges
        // XYZ→Lab→XYZ but changes nothing else, so the chain tracks the plain pair to the
        // Lab round-trip's f64 tightness.
        let src = rgb_shaper(None);
        let dst = rgb_shaper(None);
        let lab = abstract_lab();
        let options = TransformOptions {
            intent: RenderingIntent::MediaRelativeColorimetric,
            black_point_compensation: false,
        };
        let pair = IccTransform::between(&src, &dst, options).unwrap();
        let chained = IccTransform::chain(&[&src, &lab, &dst], options).unwrap();
        assert_eq!(chained.input_channels(), 3);
        assert_eq!(chained.output_channels(), 3);
        for rgb in [[0.0; 3], [1.0; 3], [0.25, 0.5, 0.75]] {
            let a = eval3(&pair, &rgb);
            let b = eval3(&chained, &rgb);
            for ch in 0..3 {
                assert!((a[ch] - b[ch]).abs() < 1e-9, "{a:?} vs {b:?}");
            }
        }
    }

    #[test]
    fn first_profile_is_always_input_direction_even_with_a_pcs_device_space() {
        // lcms2: `(i == 0) && !lIsDeviceLink` forces the FIRST profile into the input
        // direction regardless of its device space. A Lab-device-space output-class profile
        // carrying only an A2B tag therefore chains fine at slot 0 (device→PCS); a
        // direction flip would look for its (absent) B2A tag and fail.
        let mut lab_input = abstract_lab();
        lab_input.header.device_class = DeviceClass::Output;
        let dst = rgb_shaper(None);
        let chained =
            IccTransform::chain(&[&lab_input, &dst], TransformOptions::default()).unwrap();
        assert_eq!(chained.input_channels(), 3);
        assert_eq!(chained.output_channels(), 3);
        // And it evaluates: the *encoded* Lab device white (a Lab device space stays in the
        // tag's native [0, 1] encoding — the crate's device convention) maps to near-white
        // RGB.
        let ab_zero = 32896.0 / 65535.0; // encoded a* = b* = 0
        let out = eval3(&chained, &[1.0, ab_zero, ab_zero]);
        assert!(out.iter().all(|v| *v > 0.9), "white stays white: {out:?}");
    }

    #[test]
    fn chain_space_continuity_is_enforced() {
        // A gray profile cannot follow an RGB output hop: entry space Gray vs current Rgb.
        let src = rgb_shaper(None);
        let dst = rgb_shaper(None);
        let mut gray = rgb_shaper(None);
        gray.header.data_color_space = ColorSpace::Gray;
        let err =
            IccTransform::chain(&[&src, &dst, &gray], TransformOptions::default()).unwrap_err();
        assert_eq!(
            err.to_string(),
            "cmm: profile chain mismatch (profile's entry colour space does not match the \
             space the chain carries)"
        );
    }

    #[test]
    fn device_link_requires_link_class() {
        let not_link = rgb_shaper(None);
        let err = IccTransform::device_link(&not_link, RenderingIntent::Perceptual).unwrap_err();
        assert_eq!(
            err.to_string(),
            "cmm: unsupported profile (device_link requires a link-class profile)"
        );
    }

    #[test]
    fn device_link_missing_a2b_has_no_shaper_fallback() {
        // A link-class profile carrying a full shaper set but no A2B tag: `device_link`
        // must NOT fall back to the shaper (lcms2's devicelink read has no such path).
        let mut link = rgb_shaper(None);
        link.header.device_class = DeviceClass::DeviceLink;
        link.header.pcs = ColorSpace::Rgb;
        let err = IccTransform::device_link(&link, RenderingIntent::Saturation).unwrap_err();
        assert_eq!(err.to_string(), "cmm: profile is missing required tag A2B2");
        // The perceptual fallback still applies when only A2B0 exists (checked end to end
        // below and in the oracle suite).
    }

    #[test]
    fn device_link_runs_encoded_end_to_end() {
        let link = halving_link();
        let transform = IccTransform::device_link(&link, RenderingIntent::Perceptual).unwrap();
        assert_eq!(transform.input_channels(), 3);
        assert_eq!(transform.output_channels(), 3);
        // Corner value 32768 halves to 32768/65535 (≈ 0.5 to 16-bit quantization); the
        // mid-axis input interpolates to half of that. No PCS decode touches the values —
        // an RGB→RGB link runs encoded end to end.
        let out = eval3(&transform, &[1.0, 0.5, 0.0]);
        let half = 32768.0 / 65535.0;
        for (got, want) in out.iter().zip([half, half / 2.0, 0.0]) {
            assert!((got - want).abs() < 1e-12, "{out:?}");
        }
        // Every intent falls back to the sole A2B0.
        let saturation = IccTransform::device_link(&link, RenderingIntent::Saturation).unwrap();
        assert_eq!(eval3(&transform, &[1.0; 3]), eval3(&saturation, &[1.0; 3]));
    }

    #[test]
    fn chain_accepts_device_links_mid_chain() {
        // [link, dst]: the link's RGB output feeds dst input-direction... which then ends
        // at dst's PCS. Pin the direction logic: after a device-space hop the next profile
        // is INPUT-direction (lcms2's rule), so the chain output is 3-channel XYZ here.
        let link = halving_link();
        let dst = rgb_shaper(None);
        let chained = IccTransform::chain(&[&link, &dst], TransformOptions::default()).unwrap();
        assert_eq!(chained.input_channels(), 3);
        assert_eq!(chained.output_channels(), 3);
        // The halved device value goes through dst's device→PCS half: equal to feeding the
        // halved value (32768/65535, the link CLUT's corner) directly.
        let direct = crate::link::device_to_pcs(&dst, RenderingIntent::Perceptual).unwrap();
        let mut want = [0.0; 3];
        let half = 32768.0 / 65535.0;
        direct.eval(&[half, half, half], &mut want).unwrap();
        assert_eq!(eval3(&chained, &[1.0; 3]), want);
    }

    #[test]
    fn proofing_transform_shapes_and_identity_case() {
        // Proofing through the SAME profile as source and proof at relative intent with no
        // BPC collapses to (numerically) the plain src→dst transform.
        let src = rgb_shaper(None);
        let dst = rgb_shaper(Some(D65));
        let options = ProofingOptions {
            intent: RenderingIntent::MediaRelativeColorimetric,
            proofing_intent: RenderingIntent::MediaRelativeColorimetric,
            black_point_compensation: false,
        };
        let proofed = IccTransform::proofing(&src, &dst, &src, options).unwrap();
        let plain = IccTransform::between(
            &src,
            &dst,
            TransformOptions {
                intent: RenderingIntent::MediaRelativeColorimetric,
                black_point_compensation: false,
            },
        )
        .unwrap();
        assert_eq!(proofed.input_channels(), 3);
        assert_eq!(proofed.output_channels(), 3);
        for rgb in [[0.1, 0.5, 0.9], [1.0; 3]] {
            let a = eval3(&proofed, &rgb);
            let b = eval3(&plain, &rgb);
            for ch in 0..3 {
                assert!((a[ch] - b[ch]).abs() < 1e-6, "{a:?} vs {b:?}");
            }
        }
        // A named-colour proof is rejected up front.
        let mut named = rgb_shaper(None);
        named.header.device_class = DeviceClass::NamedColor;
        let err = IccTransform::proofing(&src, &dst, &named, options).unwrap_err();
        assert_eq!(
            err.to_string(),
            "cmm: unsupported profile (named-colour profiles have no continuous pixel transform)"
        );
    }

    #[test]
    fn compatible_matches_lcms2s_table() {
        assert!(compatible(ColorSpace::Rgb, ColorSpace::Rgb));
        assert!(compatible(ColorSpace::Cmyk, ColorSpace::NColor(4)));
        assert!(compatible(ColorSpace::NColor(4), ColorSpace::Cmyk));
        assert!(compatible(ColorSpace::Xyz, ColorSpace::Lab));
        assert!(compatible(ColorSpace::Lab, ColorSpace::Xyz));
        assert!(!compatible(ColorSpace::Rgb, ColorSpace::Cmyk));
        assert!(!compatible(ColorSpace::NColor(3), ColorSpace::Cmyk));
        assert!(!compatible(ColorSpace::Lab, ColorSpace::Rgb));
    }

    #[test]
    fn pcs_seam_rejects_distinct_device_spaces() {
        // The AddConversion default-arm quirk: 4CLR≡CMYK passes `compatible` but cannot
        // bridge a conversion hop.
        let (m, off) = IDENTITY;
        let err = pcs_seam(ColorSpace::Cmyk, ColorSpace::NColor(4), m, off).unwrap_err();
        assert_eq!(
            err.to_string(),
            "cmm: profile chain mismatch (device-space hop cannot bridge distinct colour spaces)"
        );
        assert!(
            pcs_seam(ColorSpace::Cmyk, ColorSpace::Cmyk, m, off)
                .unwrap()
                .is_empty()
        );
    }
}
