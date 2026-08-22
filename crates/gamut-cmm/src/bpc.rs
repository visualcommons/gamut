//! Black-point compensation (BPC): black-point **detection** for both ends of a transform and
//! the linear XYZ **compensation** applied between them.
//!
//! BPC maps the source profile's darkest reproducible colour onto the destination's instead of
//! letting shadows clip, via a per-channel linear scaling in XYZ that fixes the D50 white:
//! `v′ = a·v + b` with `a = (bpOut − D50)/(bpIn − D50)` and
//! `b = −D50·(bpOut − bpIn)/(bpIn − D50)` (so `bpIn ↦ bpOut` and `D50 ↦ D50`). The method is
//! Adobe's, standardized as ISO 18619:2015 and described by ICC white paper WP40 — both the
//! formula and the detection algorithm below are transcribed in
//! [`references/cmm/README.md`](../../../references/cmm/README.md) with the vendored sources.
//! It applies to the **non-absolute** intents only: ICC-absolute and BPC are mutually
//! exclusive (lcms2 forces the BPC flag off for absolute hops, `cmscnvrt.c:1126-1127`), and
//! [`IccTransform::between`](crate::IccTransform::between) documents both that exclusion and
//! the v4 perceptual/saturation *forcing* rule.
//!
//! # Detection, transcribed from Little-CMS
//!
//! ICC profiles carry a `bkpt` tag, but it is bogus in enough real profiles that lcms2
//! deliberately ignores it (its `CMS_USE_PROFILE_BLACK_POINT_TAG` branch is compiled out by
//! default) and **estimates** the black point instead — this crate does the same, never
//! reading `bkpt`. [`detect_black_point`] transcribes `cmsDetectBlackPoint` and
//! [`detect_destination_black_point`] transcribes `cmsDetectDestinationBlackPoint`
//! (`cmssamp.c`, lcms2 2.19), including the Adobe round-trip ramp estimator. Because the
//! estimators run *transforms this crate builds* (f64) where lcms2 runs its own (f32 with
//! 16-bit curve/CLUT quantization), estimator-path results agree with the oracle to a
//! tolerance, not bitwise — the fixed-black and gate paths are exact. All Lab↔XYZ conversions
//! here use lcms2's **rounded D50 literals** (`LCMS_D50` in the crate's `intent` module, matching
//! `cmsXYZ2Lab(NULL, …)`), not the exact PCS illuminant.
//!
//! # The zero-black convention (no `Result`)
//!
//! Both detectors return a plain XYZ triple: **`[0, 0, 0]` doubles as "no black point"** — for
//! device-link/abstract/named-colour classes, for the absolute intent, and for every internal
//! failure (unbuildable pipeline, unsupported device space, degenerate ramp). This is exactly
//! the value lcms2's consumer sees (`ComputeConversion` pre-zeroes the out-parameters and
//! ignores the detectors' boolean), so no error variant is introduced: a profile pair whose
//! black points cannot be estimated simply gets no compensation, as in lcms2.

use gamut_color::lab::{lab_to_xyz, xyz_to_lab};
use gamut_icc::{ColorSpace, DeviceClass, IccProfile, RenderingIntent};

use crate::intent::LCMS_D50;
use crate::link::{DEVICE_TO_PCS_16, PCS_TO_DEVICE_16, device_to_pcs, intent_index, pcs_to_device};
use crate::pipeline::{Pipeline, Stage};

/// lcms2's fixed v4 perceptual black `cmsPERCEPTUAL_BLACK_X/Y/Z` (`lcms2.h:297-299`): the
/// PCS black of the v4 Perceptual Reference Medium, returned verbatim for v4 CLUT profiles
/// under the perceptual/saturation intents.
pub(crate) const PERCEPTUAL_BLACK: [f64; 3] = [0.00336, 0.003_473_1, 0.00287];

/// The "no black point" value (module docs).
const ZERO: [f64; 3] = [0.0; 3];

/// Estimates the black point `profile` produces when used as the **source** (input side) of a
/// transform at `intent`, as D50-relative XYZ — a faithful transcription of lcms2's
/// `cmsDetectBlackPoint` (`cmssamp.c:238-323`; the `bkpt` tag is deliberately ignored, module
/// docs). Returns `[0, 0, 0]` when no black point exists or can be estimated.
///
/// The paths, in gate order:
///
/// 1. device-link, abstract, or named-colour class → zero; ICC-absolute intent → zero.
/// 2. **v4 profile + perceptual/saturation**: a matrix/TRC shaper profile probes its darkest
///    colorant at *media-relative* (shapers share one colorimetry across those intents);
///    anything else returns the fixed `cmsPERCEPTUAL_BLACK` of the v4 reference medium.
/// 3. **media-relative on an output-class ink profile** (CMY/CMYK/nCLR): the
///    perceptual-black round trip — `Lab(0,0,0)` through PCS→device at perceptual and back
///    at media-relative, chroma zeroed, `L*` clipped to 50 — discounting any ink limiting.
/// 4. otherwise the **darker-colorant probe**: the space's darkest device tuple (all-0 for
///    RGB/gray, all-100% for ink spaces) through device→PCS at `intent`, clipped to
///    `L* ≤ 50` (`L* > 95` or `L* < 0` → 0, chroma kept), back to XYZ.
#[must_use]
pub fn detect_black_point(profile: &IccProfile, intent: RenderingIntent) -> [f64; 3] {
    source_black(profile, intent).unwrap_or(ZERO)
}

/// [`detect_black_point`] with lcms2's success flag kept internal: `None` mirrors every
/// `cmsDetectBlackPoint` path that returns `FALSE` (all of which also zero the out-param), so
/// [`detect_destination_black_point`]'s abort-on-failure gate matches the oracle exactly even
/// where a *successful* zero black would have continued into the ramp estimator.
fn source_black(profile: &IccProfile, intent: RenderingIntent) -> Option<[f64; 3]> {
    class_and_intent_gates(profile, intent)?;
    if let Some(fixed) = v4_perceptual_fixed_black(profile, intent) {
        return fixed;
    }
    if intent == RenderingIntent::MediaRelativeColorimetric
        && profile.header.device_class == DeviceClass::Output
        && is_ink_space(profile.header.data_color_space)
    {
        return perceptual_black_round_trip(profile);
    }
    darker_colorant(profile, intent)
}

/// Estimates the black point `profile` reproduces when used as the **destination** (output
/// side) of a transform at `intent` — lcms2's `cmsDetectDestinationBlackPoint`
/// (`cmssamp.c:399-598`), the round-trip ramp estimator from Adobe's BPC paper. Returns
/// `[0, 0, 0]` when no black point exists or can be estimated.
///
/// Gates 1–2 are [`detect_black_point`]'s. Then: a profile that does **not** carry the
/// intent's own `B2Ax` tag (no perceptual fallback here — lcms2's `cmsIsCLUT`), or whose
/// device space is not gray/RGB/ink, delegates to [`detect_black_point`]. The remaining CLUT
/// output profiles get the estimator:
///
/// 1. initial black: the source estimate (media-relative) or `Lab(0,0,0)`
///    (perceptual/saturation);
/// 2. a 256-step `L*` ramp (chroma clamped to ±50) through PCS→device at `intent` and back
///    at media-relative, made monotonic from the top; a ramp whose ends do not ascend → zero;
/// 3. media-relative only: if the mid-range round-trips nearly straight (each ramp point is
///    within the bottom 20% or within 4.0 `L*` of its input), the initial black is kept;
/// 4. else a least-squares quadratic is fitted to the normalized shadow section (`y` in
///    `[0.1, 0.5)` for media-relative, `[0.03, 0.25)` for perceptual/saturation; fewer than 3
///    points → zero) and its root, clipped to `0 ≤ L* ≤ 50`, becomes the black's `L*` (the
///    initial black's chroma is kept). lcms2's fitter returns `L* = 0` for fewer than 4
///    points while its caller only rejects below 3 — so exactly 3 points yield `L* = 0` with
///    the initial chroma, a quirk replicated as-is (see `fit_root`).
#[must_use]
pub fn detect_destination_black_point(profile: &IccProfile, intent: RenderingIntent) -> [f64; 3] {
    if class_and_intent_gates(profile, intent).is_none() {
        return ZERO;
    }
    if let Some(fixed) = v4_perceptual_fixed_black(profile, intent) {
        return fixed.unwrap_or(ZERO);
    }
    let space = profile.header.data_color_space;
    let is_output_clut = profile
        .get(PCS_TO_DEVICE_16[intent_index(intent)])
        .is_some();
    let space_fits = space == ColorSpace::Gray || space == ColorSpace::Rgb || is_ink_space(space);
    if !is_output_clut || !space_fits {
        // "Handle as input case" (cmssamp.c:449-458).
        return detect_black_point(profile, intent);
    }

    let initial_lab = if intent == RenderingIntent::MediaRelativeColorimetric {
        // A source-detection *failure* aborts (lcms2 returns FALSE); a successful zero black
        // continues into the estimator as Lab(0,0,0).
        let Some(initial_xyz) = source_black(profile, intent) else {
            return ZERO;
        };
        xyz_to_lab(initial_xyz, LCMS_D50)
    } else {
        [0.0; 3]
    };

    let Some(round_trip) = RoundTrip::new(profile, intent) else {
        return ZERO;
    };
    let mut in_ramp = [0.0_f64; 256];
    let mut out_ramp = [0.0_f64; 256];
    let a = initial_lab[1].clamp(-50.0, 50.0);
    let b = initial_lab[2].clamp(-50.0, 50.0);
    for l in 0..256 {
        #[expect(clippy::cast_precision_loss, reason = "l is at most 255")]
        let lab_l = (l as f64) * 100.0 / 255.0;
        let Some(out) = round_trip.eval([lab_l, a, b]) else {
            return ZERO;
        };
        in_ramp[l] = lab_l;
        out_ramp[l] = out[0];
    }
    // Make monotonic, sweeping down from the top (cmssamp.c:506-509).
    for l in (1..=254).rev() {
        out_ramp[l] = out_ramp[l].min(out_ramp[l + 1]);
    }
    #[expect(
        clippy::neg_cmp_op_on_partial_ord,
        reason = "verbatim lcms2 guard `!(outRamp[0] < outRamp[255])` (cmssamp.c:512)"
    )]
    if !(out_ramp[0] < out_ramp[255]) {
        return ZERO;
    }
    let (min_l, max_l) = (out_ramp[0], out_ramp[255]);

    if intent == RenderingIntent::MediaRelativeColorimetric {
        let nearly_straight_midrange = (0..256).all(|l| {
            in_ramp[l] <= min_l + 0.2 * (max_l - min_l)
                || within_straightness_tolerance(in_ramp[l], out_ramp[l])
        });
        if nearly_straight_midrange {
            return lab_to_xyz(initial_lab, LCMS_D50);
        }
    }

    let (lo, hi) = if intent == RenderingIntent::MediaRelativeColorimetric {
        (0.1, 0.5)
    } else {
        // Perceptual and saturation.
        (0.03, 0.25)
    };
    let mut x = Vec::new();
    let mut y = Vec::new();
    for l in 0..256 {
        let ff = (out_ramp[l] - min_l) / (max_l - min_l);
        if in_fit_window(ff, lo, hi) {
            x.push(in_ramp[l]);
            y.push(ff);
        }
    }
    if x.len() < 3 {
        return ZERO;
    }
    let l_star = fit_root(&x, &y).max(0.0);
    lab_to_xyz([l_star, initial_lab[1], initial_lab[2]], LCMS_D50)
}

/// The straightness test's per-point tolerance: `|inRamp − outRamp| < 4.0` `L*`
/// (`cmssamp.c:527`). Isolated so its float-boundary mutation twin (`<` vs `<=`, decided
/// only at a difference of exactly 4.0) can be excluded narrowly.
fn within_straightness_tolerance(in_l: f64, out_l: f64) -> bool {
    (in_l - out_l).abs() < 4.0
}

/// The fit-region membership test `lo ≤ ff < hi` (`cmssamp.c:568`). Isolated for the same
/// reason as [`within_straightness_tolerance`]: the exclusive upper bound's float-boundary
/// twin is only decidable at `ff == hi` exactly.
fn in_fit_window(ff: f64, lo: f64, hi: f64) -> bool {
    ff >= lo && ff < hi
}

/// The strict-threshold comparison shared by the fitter's `MATRIX_DET_TOLERANCE` and
/// degenerate-coefficient cutoffs and by [`compensation`]'s D50-anchor guard. Isolated so
/// the boundary twin (`<` vs `<=` at `magnitude == tolerance` exactly) is a single
/// narrowly-excludable mutant, while broad mutations of the comparison stay visible at
/// every call site.
fn below_tolerance(magnitude: f64, tolerance: f64) -> bool {
    magnitude < tolerance
}

/// The darker-colorant probe's `L*` clip (`cmssamp.c:129-134`): `> 95` (synthetic negative
/// profiles) and `< 0` zero the black, everything else caps at 50. Isolated because two of
/// its comparisons are unobservable in this crate: `L* == 95` exactly is unreachable from
/// any 16-bit Lab encoding, and the `L* < 0` arm is dead in decoded pipelines (every PCS
/// decode here yields `L* ≥ 0`) — kept as transcription fidelity.
#[expect(
    clippy::manual_range_contains,
    reason = "verbatim lcms2 comparisons (cmssamp.c:129-134), kept explicit so the excluded \
              boundary mutants match the documented shape"
)]
fn clip_probe_l(l: f64) -> f64 {
    if l > 95.0 || l < 0.0 {
        0.0
    } else {
        l.min(50.0)
    }
}

/// The per-channel BPC scaling `(m, offset)` in decoded XYZ, or `None` when no compensation
/// applies: equal black points (lcms2's exact component-wise `!=` check,
/// `cmscnvrt.c:396-400`), or a black-point component coinciding with the D50 anchor.
///
/// The formula is `ComputeBlackPointCompensation` (`cmscnvrt.c:166-201`):
/// `a = (bpOut − D50)/(bpIn − D50)`, `b = −D50·(bpOut − bpIn)/(bpIn − D50)`, diagonal, with
/// [`LCMS_D50`] as the anchor. Two deliberate decoded-domain differences from the verbatim
/// source, both documented in STATUS.md:
///
/// - lcms2 divides the offset by `MAX_ENCODEABLE_XYZ` because its pipelines carry *encoded*
///   XYZ; this crate's PCS seams are decoded, so the offset is used as derived (the caller's
///   empty-layer test re-applies the factor so the *skip decision* still matches lcms2).
/// - lcms2 has **no guard** on the `bpIn − D50` division: a black-point component equal to
///   the anchor produces ±inf/NaN that silently poisons the transform. Here that case
///   (`|bpIn − D50| < 1e-12` in any channel) is treated as equal-blacks — no compensation —
///   because a plausible-looking non-finite cascade is strictly worse than a no-op. A real
///   black point never sits at the white anchor, so the divergence is unobservable on real
///   profiles.
pub(crate) fn compensation(
    black_in: [f64; 3],
    black_out: [f64; 3],
) -> Option<([[f64; 3]; 3], [f64; 3])> {
    if black_in == black_out {
        return None;
    }
    if (0..3).any(|k| below_tolerance((black_in[k] - LCMS_D50[k]).abs(), 1e-12)) {
        return None;
    }
    let mut m = [[0.0; 3]; 3];
    let mut off = [0.0; 3];
    for k in 0..3 {
        let t = black_in[k] - LCMS_D50[k];
        m[k][k] = (black_out[k] - LCMS_D50[k]) / t;
        off[k] = -LCMS_D50[k] * (black_out[k] - black_in[k]) / t;
    }
    Some((m, off))
}

/// The shared class/intent gates of both detectors (`cmssamp.c:242-257/407-428`): `None` for
/// device-link/abstract/named-colour classes and for any intent other than
/// perceptual/media-relative/saturation (i.e. ICC-absolute).
fn class_and_intent_gates(profile: &IccProfile, intent: RenderingIntent) -> Option<()> {
    match profile.header.device_class {
        DeviceClass::DeviceLink | DeviceClass::Abstract | DeviceClass::NamedColor => return None,
        _ => {}
    }
    if intent == RenderingIntent::IccAbsoluteColorimetric {
        return None;
    }
    Some(())
}

/// The v4 + perceptual/saturation branch shared by both detectors (`cmssamp.c:259-274`):
/// `Some(result)` when it applies — the darker-colorant probe **at media-relative** for
/// matrix/TRC shaper profiles, the fixed [`PERCEPTUAL_BLACK`] otherwise — else `None` (the
/// caller continues with the v2-era paths).
fn v4_perceptual_fixed_black(
    profile: &IccProfile,
    intent: RenderingIntent,
) -> Option<Option<[f64; 3]>> {
    let v4 = profile.header.version.major >= 4;
    let perceptual_like = matches!(
        intent,
        RenderingIntent::Perceptual | RenderingIntent::Saturation
    );
    if !(v4 && perceptual_like) {
        return None;
    }
    if is_matrix_shaper(profile) {
        return Some(darker_colorant(
            profile,
            RenderingIntent::MediaRelativeColorimetric,
        ));
    }
    Some(Some(PERCEPTUAL_BLACK))
}

/// lcms2's `cmsIsMatrixShaper` (`cmsio1.c:806-827`): a gray profile with `kTRC`, or an RGB
/// profile with all three colorants and all three TRCs. Tag *presence* only — usability is
/// the link builders' concern.
pub(crate) fn is_matrix_shaper(profile: &IccProfile) -> bool {
    use gamut_icc::KnownTag as T;
    match profile.header.data_color_space {
        ColorSpace::Gray => profile.get(T::GrayTrc).is_some(),
        ColorSpace::Rgb => [
            T::RedColorant,
            T::GreenColorant,
            T::BlueColorant,
            T::RedTrc,
            T::GreenTrc,
            T::BlueTrc,
        ]
        .iter()
        .all(|&tag| profile.get(tag).is_some()),
        _ => false,
    }
}

/// lcms2's `isInkColorspace` (`cmssamp.c:191-232`): CMY, CMYK, and every multi-colorant
/// space (`nCLR`; lcms2 also lists its legacy `MCHx` aliases, which `gamut-icc` folds into
/// the same family).
fn is_ink_space(space: ColorSpace) -> bool {
    matches!(
        space,
        ColorSpace::Cmy | ColorSpace::Cmyk | ColorSpace::NColor(_)
    )
}

/// The darkest device tuple per space, as encoded `[0, 1]` channels — lcms2's
/// `_cmsEndPointsBySpace` black endpoints (`cmspcs.c:707-756`): all-zero for gray/RGB,
/// all-ones (100% ink, no ink limit assumed) for CMY/CMYK, and the v4-encoded `Lab(0, 0, 0)`
/// for a Lab device space. `None` for every other space (which zeroes the probe, as in
/// lcms2).
fn darkest_device_tuple(space: ColorSpace) -> Option<Vec<f64>> {
    /// Encoded v4 `a* = b* = 0`: `0x8080 / 65535`.
    const LAB_AB_ZERO: f64 = 32896.0 / 65535.0;
    match space {
        ColorSpace::Gray => Some(vec![0.0]),
        ColorSpace::Rgb => Some(vec![0.0; 3]),
        ColorSpace::Lab => Some(vec![0.0, LAB_AB_ZERO, LAB_AB_ZERO]),
        ColorSpace::Cmyk => Some(vec![1.0; 4]),
        ColorSpace::Cmy => Some(vec![1.0; 3]),
        _ => None,
    }
}

/// Whether `profile` supports `intent` in the device→PCS direction — lcms2's
/// `cmsIsIntentSupported(…, LCMS_USED_AS_INPUT)` (`cmsio1.c:864-876`): the intent's own
/// `A2Bx` tag is present (**no** perceptual fallback in this test, unlike the link builders),
/// or the profile is a matrix/TRC shaper (which serves every intent).
fn intent_supported_as_input(profile: &IccProfile, intent: RenderingIntent) -> bool {
    profile
        .get(DEVICE_TO_PCS_16[intent_index(intent)])
        .is_some()
        || is_matrix_shaper(profile)
}

/// The darker-colorant probe, `BlackPointAsDarkerColorant` (`cmssamp.c:64-150`): the space's
/// darkest device tuple through this crate's device→PCS pipeline at `intent`, to Lab (via
/// the rounded D50 when the PCS is XYZ), `L*` clipped (`> 95` → 0 for synthetic negative
/// profiles, `< 0` → 0, `> 50` → 50, chroma kept), back to XYZ. `None` for an unsupported
/// intent/space or an unbuildable pipeline.
fn darker_colorant(profile: &IccProfile, intent: RenderingIntent) -> Option<[f64; 3]> {
    if !intent_supported_as_input(profile, intent) {
        return None;
    }
    let black = darkest_device_tuple(profile.header.data_color_space)?;
    let pipeline = device_to_pcs(profile, intent).ok()?;
    // A channel-count mismatch between the header space and the pipeline (lcms2's
    // `nChannels != T_CHANNELS` check) surfaces as an eval BufferLength error → None.
    let mut pcs = [0.0; 3];
    pipeline.eval(&black, &mut pcs).ok()?;
    let lab = match profile.header.pcs {
        ColorSpace::Lab => pcs,
        ColorSpace::Xyz => xyz_to_lab(pcs, LCMS_D50),
        _ => return None,
    };
    Some(lab_to_xyz([clip_probe_l(lab[0]), lab[1], lab[2]], LCMS_D50))
}

/// The ink-limit-discounting probe, `BlackPointUsingPerceptualBlack` (`cmssamp.c:155-192`):
/// `Lab(0, 0, 0)` through the PCS→device (perceptual) → device→PCS (media-relative) round
/// trip, output `L*` clipped to 50, chroma forced to zero, back to XYZ. A profile that does
/// not support perceptual as input yields zero *successfully* (lcms2 returns `TRUE` there);
/// only an unbuildable round trip is a failure.
fn perceptual_black_round_trip(profile: &IccProfile) -> Option<[f64; 3]> {
    if !intent_supported_as_input(profile, RenderingIntent::Perceptual) {
        return Some(ZERO);
    }
    let round_trip = RoundTrip::new(profile, RenderingIntent::Perceptual)?;
    let out = round_trip.eval([0.0; 3])?;
    Some(lab_to_xyz([out[0].min(50.0), 0.0, 0.0], LCMS_D50))
}

/// The Lab→device→Lab round trip of one profile — lcms2's `CreateRoundtripXForm`
/// (`cmssamp.c:39-59`), whose 4-profile chain collapses to: PCS→device at `intent`, then
/// device→PCS at **media-relative** (the return leg is always relative), bridged to Lab with
/// the rounded D50 when the profile's PCS is XYZ. Adaptation state 1.0.
///
/// **The hidden forced-BPC layer:** `CreateRoundtripXForm` passes `BPC = {FALSE, …}`, but the
/// chain still runs through `_cmsLinkProfiles` (`cmscnvrt.c:1119-1135`), which *forces*
/// `BPC[1] = TRUE` when the probed profile is v4 and the forward intent is
/// perceptual/saturation — so lcms2's detection round trip silently carries a compensation
/// layer from the Lab endpoint's zero black (the Lab4 profile is abstract-class → zero) to
/// the profile's own destination black, inserted in XYZ at the Lab→PCS seam. Replicated
/// here; without it the ink round-trip probe on v4 profiles misses the oracle by ≈ 2 `L*`.
struct RoundTrip {
    to_device: Pipeline,
    to_pcs: Pipeline,
    xyz_pcs: bool,
    /// The forced-BPC conversion at the Lab→PCS seam (see above) as a [`Stage::Matrix`]
    /// (reusing the pipeline evaluator rather than re-deriving the affine math), already
    /// empty-layer-filtered.
    layer: Option<Stage>,
}

impl RoundTrip {
    fn new(profile: &IccProfile, intent: RenderingIntent) -> Option<Self> {
        let xyz_pcs = match profile.header.pcs {
            ColorSpace::Xyz => true,
            ColorSpace::Lab => false,
            _ => return None,
        };
        let to_device = pcs_to_device(profile, intent).ok()?;
        let to_pcs = device_to_pcs(profile, RenderingIntent::MediaRelativeColorimetric).ok()?;
        if to_device.output_channels() != to_pcs.input_channels()
            || to_device.input_channels() != 3
            || to_pcs.output_channels() != 3
        {
            return None;
        }
        let forced = matches!(
            intent,
            RenderingIntent::Perceptual | RenderingIntent::Saturation
        ) && profile.header.version.major >= 4;
        let layer = if forced {
            // ComputeConversion(1, …): source black of the abstract Lab endpoint is zero;
            // destination black of the probed profile at the forward intent.
            compensation(ZERO, detect_destination_black_point(profile, intent))
                .filter(|(m, off)| !crate::transform::is_empty_layer(m, off))
                .map(|(m, off)| Stage::Matrix { m, offset: off })
        } else {
            None
        };
        Some(Self {
            to_device,
            to_pcs,
            xyz_pcs,
            layer,
        })
    }

    /// One decoded Lab pixel around the loop.
    fn eval(&self, lab: [f64; 3]) -> Option<[f64; 3]> {
        let mut pcs_in = if self.xyz_pcs || self.layer.is_some() {
            lab_to_xyz(lab, LCMS_D50)
        } else {
            lab
        };
        if let Some(stage) = &self.layer {
            let mut adjusted = [0.0; 3];
            stage.eval(&pcs_in, &mut adjusted);
            pcs_in = adjusted;
        }
        if !self.xyz_pcs && self.layer.is_some() {
            pcs_in = xyz_to_lab(pcs_in, LCMS_D50);
        }
        let mut device = [0.0_f64; crate::MAX_CHANNELS as usize];
        let device = &mut device[..usize::from(self.to_device.output_channels())];
        self.to_device.eval(&pcs_in, device).ok()?;
        let mut pcs_out = [0.0; 3];
        self.to_pcs.eval(device, &mut pcs_out).ok()?;
        Some(if self.xyz_pcs {
            xyz_to_lab(pcs_out, LCMS_D50)
        } else {
            pcs_out
        })
    }
}

/// The root of the least-squares quadratic fit `y = a·x² + b·x + c` —
/// `RootOfLeastSquaresFitQuadraticCurve` (`cmssamp.c:330-394`), verbatim quirks included:
///
/// - fewer than **4** points → 0 (the caller admits 3, so `n == 3` nets `L* = 0`);
/// - the 3×3 normal equations are solved by matrix inversion with lcms2's
///   `MATRIX_DET_TOLERANCE = 1e-4` singularity cutoff (`cmsmtrx.c:139`) → 0 when singular;
/// - `|a| < 1e-10` degrades to the linear root `−c/b` (`|b| < 1e-10` too → 0);
/// - a non-positive discriminant → 0; otherwise the `+√` root `(−b + √(b²−4ac))/(2a)`;
/// - non-zero results clamp into `[0, 50]`.
fn fit_root(x: &[f64], y: &[f64]) -> f64 {
    let n = x.len();
    if n < 4 {
        return 0.0;
    }
    let (mut sum_x, mut sum_x2, mut sum_x3, mut sum_x4) = (0.0, 0.0, 0.0, 0.0);
    let (mut sum_y, mut sum_yx, mut sum_yx2) = (0.0, 0.0, 0.0);
    for (&xn, &yn) in x.iter().zip(y) {
        sum_x += xn;
        sum_x2 += xn * xn;
        sum_x3 += xn * xn * xn;
        sum_x4 += xn * xn * xn * xn;
        sum_y += yn;
        sum_yx += yn * xn;
        sum_yx2 += yn * xn * xn;
    }
    #[expect(clippy::cast_precision_loss, reason = "n is at most 256")]
    let m = [
        [n as f64, sum_x, sum_x2],
        [sum_x, sum_x2, sum_x3],
        [sum_x2, sum_x3, sum_x4],
    ];
    // lcms2's _cmsMAT3inverse refuses |det| < MATRIX_DET_TOLERANCE (1e-4) — replicate the
    // cutoff so near-singular fits collapse to zero exactly as in the oracle. The system is
    // then solved by Cramer's rule over the same determinant expression, so the one `det3`
    // transcription both gates and produces the solution.
    let det = det3(&m);
    if below_tolerance(det.abs(), 1e-4) {
        return 0.0;
    }
    let v = [sum_y, sum_yx, sum_yx2];
    let mut res = [0.0; 3];
    for (k, r) in res.iter_mut().enumerate() {
        let mut mk = m;
        for (row, &value) in mk.iter_mut().zip(&v) {
            row[k] = value;
        }
        *r = det3(&mk) / det;
    }
    let (c, b, a) = (res[0], res[1], res[2]);
    if below_tolerance(a.abs(), 1e-10) {
        if below_tolerance(b.abs(), 1e-10) {
            return 0.0;
        }
        return (-c / b).clamp(0.0, 50.0);
    }
    let d = b * b - 4.0 * a * c;
    if d <= 0.0 {
        return 0.0;
    }
    ((-b + d.sqrt()) / (2.0 * a)).clamp(0.0, 50.0)
}

/// The cofactor-expansion determinant of a 3×3 matrix — the expression lcms2's
/// `_cmsMAT3inverse`/`_cmsMAT3solve` are built on, shared by [`fit_root`]'s singularity gate
/// and its Cramer solution.
fn det3(m: &[[f64; 3]; 3]) -> f64 {
    m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1])
        - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
        + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0])
}

#[cfg(test)]
mod tests {
    use gamut_icc::{
        Clut, ClutPrecision, Curve, CurveOrParametric, LutAToB, LutBToA, ProfileHeader, Signature,
        TagData, U8Fixed8, XyzNumber,
    };

    use super::*;

    // ---- compensation formula ------------------------------------------------------------

    #[test]
    fn compensation_maps_black_in_to_black_out_and_fixes_d50() {
        // The defining property of the scaling: [m]·bpIn + off = bpOut and [m]·D50 + off = D50.
        let bp_in = [0.002, 0.0025, 0.0015];
        let bp_out = [0.015, 0.02, 0.012];
        let (m, off) = compensation(bp_in, bp_out).expect("distinct blacks compensate");
        for k in 0..3 {
            let at_black = m[k][k] * bp_in[k] + off[k];
            let at_white = m[k][k] * LCMS_D50[k] + off[k];
            assert!((at_black - bp_out[k]).abs() < 1e-14, "black ch {k}");
            assert!((at_white - LCMS_D50[k]).abs() < 1e-14, "white ch {k}");
        }
        // Diagonal only.
        for (i, row) in m.iter().enumerate() {
            for (j, cell) in row.iter().enumerate() {
                if i != j {
                    assert_eq!(*cell, 0.0, "off-diagonal [{i}][{j}]");
                }
            }
        }
    }

    #[test]
    fn compensation_hand_computed_pin() {
        // bpIn = (0, 0, 0), bpOut = (0.01, 0.02, 0.04): a = (bpOut − D50)/(0 − D50)
        // = 1 − bpOut/D50, b = −D50·(bpOut − 0)/(0 − D50) = bpOut. Hand-derived literals.
        let (m, off) = compensation([0.0; 3], [0.01, 0.02, 0.04]).unwrap();
        assert!((m[0][0] - (1.0 - 0.01 / 0.9642)).abs() < 1e-15);
        assert!((m[1][1] - 0.98).abs() < 1e-15);
        assert!((m[2][2] - (1.0 - 0.04 / 0.8249)).abs() < 1e-15);
        for k in 0..3 {
            let want = [0.01, 0.02, 0.04][k];
            assert!((off[k] - want).abs() < 1e-15, "offset ch {k}");
        }
        // And the anchor is the rounded lcms2 D50, not the exact PCS illuminant: with the
        // exact anchor the Y column would be (1 − 0.02/1.0) too, but X would differ.
        let exact = 1.0 - 0.01 / gamut_color::lab::D50_XYZ[0];
        assert!((m[0][0] - exact).abs() > 1e-9, "anchor must be rounded D50");
    }

    #[test]
    fn equal_black_points_and_d50_coincidence_skip_compensation() {
        let bp = [0.003, 0.004, 0.002];
        assert!(compensation(bp, bp).is_none(), "equal blacks: do nothing");
        // The documented guard divergence: a bpIn component equal to the D50 anchor would
        // divide by zero in lcms2 (inf/NaN cascade); here it is a no-op.
        let at_anchor = [LCMS_D50[0], 0.004, 0.002];
        assert!(compensation(at_anchor, [0.0; 3]).is_none(), "X at anchor");
        let near_anchor = [LCMS_D50[0] - 1e-13, 0.004, 0.002];
        assert!(
            compensation(near_anchor, [0.0; 3]).is_none(),
            "within 1e-12"
        );
        // Just outside the guard the formula proceeds (and is finite).
        let outside = [LCMS_D50[0] - 1e-9, 0.004, 0.002];
        let (m, off) = compensation(outside, [0.0; 3]).expect("outside the guard");
        assert!(m[0][0].is_finite() && off[0].is_finite());
    }

    // ---- fit_root ------------------------------------------------------------------------

    #[test]
    fn fit_root_recovers_the_positive_root_of_an_exact_quadratic() {
        // y = (x² − 100)/2400: roots ±10; the (−b + √d)/(2a) choice picks +10.
        let x: Vec<f64> = vec![5.0, 15.0, 25.0, 35.0, 45.0];
        let y: Vec<f64> = x.iter().map(|&v| (v * v - 100.0) / 2400.0).collect();
        let root = fit_root(&x, &y);
        assert!((root - 10.0).abs() < 1e-9, "root = {root}");
    }

    #[test]
    fn fit_root_below_four_points_is_zero() {
        // The lcms2 quirk: the caller admits n == 3, the fitter refuses n < 4 → net L* = 0.
        // The 3-point data has a decisively non-zero root (10), so a weakened count gate
        // would return 10 instead of 0.
        let x = [5.0, 15.0, 25.0];
        let y: Vec<f64> = x.iter().map(|&v| (v - 10.0) / 90.0).collect();
        assert_eq!(fit_root(&x, &y), 0.0);
        assert_eq!(fit_root(&x[..2], &y[..2]), 0.0);
        // And exactly 4 points is ACCEPTED (the gate is < 4, not ≤ 4): same line, root 10.
        let x4 = [5.0, 15.0, 25.0, 35.0];
        let y4: Vec<f64> = x4.iter().map(|&v| (v - 10.0) / 90.0).collect();
        let root = fit_root(&x4, &y4);
        assert!((root - 10.0).abs() < 1e-9, "n = 4 root = {root}");
    }

    #[test]
    fn fit_root_replicates_the_lcms2_determinant_cutoff() {
        // Tiny x values make the normal-equation determinant ≈ 8e-11 — far below lcms2's
        // MATRIX_DET_TOLERANCE (1e-4) yet numerically invertible, and the underlying line
        // has a clearly non-zero root (0.02): only the transcribed cutoff forces 0 here.
        let x = [0.01, 0.02, 0.03, 0.04];
        let y: Vec<f64> = x.iter().map(|&v| (v - 0.02) / 0.02).collect();
        assert_eq!(fit_root(&x, &y), 0.0);
        // The same shape scaled into the well-conditioned regime fits normally, proving the
        // zero above came from the cutoff and not the data.
        let x = [10.0, 20.0, 30.0, 40.0];
        let y: Vec<f64> = x.iter().map(|&v| (v - 20.0) / 20.0).collect();
        let root = fit_root(&x, &y);
        assert!((root - 20.0).abs() < 1e-6, "root = {root}");
    }

    #[test]
    fn fit_root_degenerate_cases() {
        // Near-zero quadratic term degrades to the linear root −c/b.
        let x = [5.0, 15.0, 25.0, 35.0, 45.0];
        let y: Vec<f64> = x.iter().map(|&v| (v - 10.0) / 90.0).collect();
        let root = fit_root(&x, &y);
        assert!((root - 10.0).abs() < 1e-9, "linear root = {root}");
        // Constant data: a ≈ 0 and b ≈ 0 → 0.
        assert_eq!(fit_root(&x, &[0.25; 5]), 0.0);
        // All-positive parabola (double root at the vertex): discriminant ≤ 0 → 0.
        let y: Vec<f64> = x
            .iter()
            .map(|&v| (v - 20.0) * (v - 20.0) / 900.0 + 0.1)
            .collect();
        assert_eq!(fit_root(&x, &y), 0.0);
        // Clamping: linear root at 60 clamps to 50, at −20 clamps to 0.
        let y: Vec<f64> = x.iter().map(|&v| (v - 60.0) / 90.0).collect();
        assert_eq!(fit_root(&x, &y), 50.0);
        let y: Vec<f64> = x.iter().map(|&v| (v + 20.0) / 90.0).collect();
        assert_eq!(fit_root(&x, &y), 0.0);
        // Singular normal equations (all x identical) → 0 via the 1e-4 determinant cutoff.
        assert_eq!(fit_root(&[10.0; 5], &[0.1, 0.2, 0.3, 0.4, 0.5]), 0.0);
    }

    // ---- gates and fixed paths -------------------------------------------------------------

    /// A v4 CMYK→Lab output profile carrying the given LUT tags.
    fn cmyk_output(tags: Vec<(Signature, TagData)>) -> IccProfile {
        let mut header = ProfileHeader::new(DeviceClass::Output, ColorSpace::Cmyk);
        header.pcs = ColorSpace::Lab;
        IccProfile { header, tags }
    }

    /// A minimal 4→3 `mAB ` whose CLUT is constant at `value` (encoded 16-bit).
    fn constant_a2b(value: u16) -> TagData {
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
            b_curves: vec![CurveOrParametric::Curve(Curve::Identity); 3],
        })
    }

    #[test]
    fn link_abstract_named_classes_and_absolute_intent_are_zero() {
        for class in [
            DeviceClass::DeviceLink,
            DeviceClass::Abstract,
            DeviceClass::NamedColor,
        ] {
            let mut profile = cmyk_output(vec![(Signature(*b"A2B0"), constant_a2b(1000))]);
            profile.header.device_class = class;
            assert_eq!(
                detect_black_point(&profile, RenderingIntent::Perceptual),
                ZERO,
                "{class:?}"
            );
            assert_eq!(
                detect_destination_black_point(&profile, RenderingIntent::Perceptual),
                ZERO,
                "{class:?}"
            );
        }
        let profile = cmyk_output(vec![(Signature(*b"A2B0"), constant_a2b(1000))]);
        assert_eq!(
            detect_black_point(&profile, RenderingIntent::IccAbsoluteColorimetric),
            ZERO
        );
        assert_eq!(
            detect_destination_black_point(&profile, RenderingIntent::IccAbsoluteColorimetric),
            ZERO
        );
    }

    #[test]
    fn v4_clut_profile_returns_fixed_perceptual_black_for_perceptual_and_saturation() {
        let profile = cmyk_output(vec![(Signature(*b"A2B0"), constant_a2b(1000))]);
        assert_eq!(profile.header.version.major, 4);
        for intent in [RenderingIntent::Perceptual, RenderingIntent::Saturation] {
            assert_eq!(
                detect_black_point(&profile, intent),
                PERCEPTUAL_BLACK,
                "source {intent:?}"
            );
            assert_eq!(
                detect_destination_black_point(&profile, intent),
                PERCEPTUAL_BLACK,
                "destination {intent:?}"
            );
        }
        // The pinned constants themselves (lcms2.h:297-299).
        assert_eq!(PERCEPTUAL_BLACK, [0.00336, 0.0034731, 0.00287]);
    }

    #[test]
    fn v2_profile_never_takes_the_fixed_black_branch() {
        // Same tags, header downgraded to v2: perceptual routes to the darker-colorant probe
        // (constant CLUT at encoded L* = c/65535·100.390625 — v2? no: mAB stays v4-decoded;
        // the probe sees a mid-L CLUT, clips to 50, keeps chroma).
        let mut profile = cmyk_output(vec![(Signature(*b"A2B0"), constant_a2b(1000))]);
        profile.header.version.major = 2;
        let got = detect_black_point(&profile, RenderingIntent::Perceptual);
        assert_ne!(got, PERCEPTUAL_BLACK);
        // Constant CLUT 1000 → encoded (1000/65535) → decoded L* ≈ 1.526, a* = b* ≈ −124.1:
        // darker-colorant result = Lab(1.526, −124.1, −124.1) → XYZ, which is NOT zero.
        assert!(got[1] > 0.0, "probe result has positive Y: {got:?}");
    }

    #[test]
    fn v4_matrix_shaper_probes_darker_colorant_instead_of_fixed_black() {
        // A v4 RGB shaper under perceptual: cmsIsMatrixShaper → darker-colorant at RELATIVE.
        // Device black (0,0,0) → XYZ (0,0,0) → exactly zero, never PERCEPTUAL_BLACK.
        let xyz_tag = |v: [f64; 3]| TagData::Xyz(vec![XyzNumber::from_f64(v)]);
        let gamma = || TagData::Curve(Curve::Gamma(U8Fixed8(0x0233)));
        let profile = IccProfile {
            header: ProfileHeader::new(DeviceClass::Display, ColorSpace::Rgb),
            tags: vec![
                (Signature(*b"rXYZ"), xyz_tag([0.436, 0.2225, 0.0139])),
                (Signature(*b"gXYZ"), xyz_tag([0.3851, 0.7169, 0.0971])),
                (Signature(*b"bXYZ"), xyz_tag([0.1431, 0.0606, 0.7141])),
                (Signature(*b"rTRC"), gamma()),
                (Signature(*b"gTRC"), gamma()),
                (Signature(*b"bTRC"), gamma()),
            ],
        };
        for intent in [RenderingIntent::Perceptual, RenderingIntent::Saturation] {
            assert_eq!(detect_black_point(&profile, intent), ZERO, "{intent:?}");
            assert_eq!(
                detect_destination_black_point(&profile, intent),
                ZERO,
                "{intent:?}"
            );
        }
        // Removing one TRC breaks the matrix-shaper test → the fixed black returns.
        let mut incomplete = profile;
        incomplete.tags.retain(|(sig, _)| sig.0 != *b"bTRC");
        assert_eq!(
            detect_black_point(&incomplete, RenderingIntent::Perceptual),
            PERCEPTUAL_BLACK
        );
    }

    #[test]
    fn unsupported_intent_or_space_zeroes_the_probe() {
        // No A2B1 tag and not a matrix shaper: media-relative is unsupported as input → zero
        // (cmsIsIntentSupported has NO perceptual fallback, unlike the link builders).
        let profile = cmyk_output(vec![(Signature(*b"A2B0"), constant_a2b(1000))]);
        assert_eq!(
            detect_black_point(&profile, RenderingIntent::MediaRelativeColorimetric),
            ZERO
        );
        // An XYZ device space has no endpoint tuple in lcms2 → zero even with a v2 header
        // (routes to the darker-colorant probe first).
        let mut odd = cmyk_output(vec![(Signature(*b"A2B0"), constant_a2b(1000))]);
        odd.header.version.major = 2;
        odd.header.data_color_space = ColorSpace::Xyz;
        assert_eq!(detect_black_point(&odd, RenderingIntent::Perceptual), ZERO);
    }

    // ---- destination estimator control flow ------------------------------------------------

    /// A hand-built CMYK↔Lab round-trip vehicle: the `B2Ax` tags map encoded Lab to a
    /// 4-channel device tuple whose first channel *is* the encoded `L*` (a 2-node
    /// pass-through CLUT), and the `A2Bx` tags map that channel back into `L*` through a
    /// per-slot curve (with `a* = b* = 0` exactly) — `curve0` under the perceptual tags,
    /// `curve1` under the media-relative ones, so the two detection legs are independently
    /// steerable.
    fn roundtrip_profile(version_major: u8, curve0: Curve, curve1: Curve) -> IccProfile {
        // B2A: device = (L, a, b, 0) — each CLUT corner stores its own coordinates.
        let mut b2a_samples = Vec::new();
        for l in 0..2u16 {
            for a in 0..2u16 {
                for b in 0..2u16 {
                    b2a_samples.extend([l * 65535, a * 65535, b * 65535, 0]);
                }
            }
        }
        let b2a = || {
            TagData::LutBToA(LutBToA {
                input_channels: 3,
                output_channels: 4,
                b_curves: vec![CurveOrParametric::Curve(Curve::Identity); 3],
                matrix: None,
                m_curves: None,
                clut: Some(Clut {
                    grid_points: vec![2; 3],
                    output_channels: 4,
                    precision: ClutPrecision::U16,
                    samples: b2a_samples.clone(),
                }),
                a_curves: None,
            })
        };
        // A2B: L' = curve(device ch 0), a' = b' = encoded zero (0x8080 = 128·257, which
        // decodes to exactly 0.0 in the v4 Lab seam). Channel 0 is the slowest CLUT axis.
        let mut a2b_samples = Vec::new();
        for corner in 0..16u16 {
            let c0 = (corner >> 3) & 1;
            a2b_samples.extend([c0 * 65535, 0x8080, 0x8080]);
        }
        let a2b = |curve: Curve| {
            TagData::LutAToB(LutAToB {
                input_channels: 4,
                output_channels: 3,
                a_curves: Some(vec![
                    CurveOrParametric::Curve(curve),
                    CurveOrParametric::Curve(Curve::Identity),
                    CurveOrParametric::Curve(Curve::Identity),
                    CurveOrParametric::Curve(Curve::Identity),
                ]),
                clut: Some(Clut {
                    grid_points: vec![2; 4],
                    output_channels: 3,
                    precision: ClutPrecision::U16,
                    samples: a2b_samples.clone(),
                }),
                m_curves: None,
                matrix: None,
                b_curves: vec![CurveOrParametric::Curve(Curve::Identity); 3],
            })
        };
        let mut profile = cmyk_output(vec![
            (Signature(*b"A2B0"), a2b(curve0)),
            (Signature(*b"A2B1"), a2b(curve1)),
            (Signature(*b"B2A0"), b2a()),
            (Signature(*b"B2A1"), b2a()),
        ]);
        profile.header.version.major = version_major;
        profile
    }

    #[test]
    fn destination_without_the_intents_b2a_tag_delegates_to_source_detection() {
        // v2 profile at perceptual with A2B0 only: cmsIsCLUT checks the intent's OWN B2A tag
        // (no fallback) — absent, so the destination delegates to source detection, whose
        // darker-colorant probe yields a distinctly non-zero value here.
        let mut profile = cmyk_output(vec![(Signature(*b"A2B0"), constant_a2b(1000))]);
        profile.header.version.major = 2;
        let source = detect_black_point(&profile, RenderingIntent::Perceptual);
        assert_ne!(source, ZERO, "probe value must be observable");
        assert_eq!(
            detect_destination_black_point(&profile, RenderingIntent::Perceptual),
            source
        );
    }

    #[test]
    fn non_ascending_ramp_is_rejected_to_zero() {
        // The relative-slot A2B collapses every device value to L* = 0 (CLUT ch0 zeroed):
        // outRamp is constant → !(outRamp[0] < outRamp[255]) → zero.
        let mut profile = roundtrip_profile(2, Curve::Identity, Curve::Identity);
        if let Some((_, TagData::LutAToB(lut))) =
            profile.tags.iter_mut().find(|(sig, _)| sig.0 == *b"A2B1")
            && let Some(clut) = lut.clut.as_mut()
        {
            for chunk in clut.samples.chunks_exact_mut(3) {
                chunk[0] = 0;
            }
        }
        assert_eq!(
            detect_destination_black_point(&profile, RenderingIntent::MediaRelativeColorimetric),
            ZERO
        );
    }

    #[test]
    fn straight_midrange_keeps_the_initial_black_at_relative_intent() {
        // Lift the PERCEPTUAL B2A's black to device ch0 = 0.15: the initial black (the
        // source estimate via the perceptual-black round trip, whose forward leg is B2A0 and
        // return leg the relative A2B1) becomes Lab(15, 0, 0) — distinctly non-zero. The
        // RELATIVE legs stay the identity, so outRamp == inRamp and the mid-range
        // straightness shortcut fires, returning the initial black unchanged; the estimator
        // would have fitted ≈ 0 instead, so this pin dies if the shortcut (or its 0.2 / 4.0
        // constants) is broken.
        let mut profile = roundtrip_profile(2, Curve::Identity, Curve::Identity);
        if let Some((_, TagData::LutBToA(lut))) =
            profile.tags.iter_mut().find(|(sig, _)| sig.0 == *b"B2A0")
        {
            let clut = lut.clut.as_mut().unwrap();
            // The L axis is slowest: the first 4 of 8 corners carry L = 0.
            for corner in 0..4 {
                clut.samples[corner * 4] = 9830; // ≈ 0.15 encoded
            }
        }
        let got =
            detect_destination_black_point(&profile, RenderingIntent::MediaRelativeColorimetric);
        let want = lab_to_xyz([15.0, 0.0, 0.0], LCMS_D50);
        for ch in 0..3 {
            // 9830/65535 ≈ 0.149992 → L* ≈ 14.9992: ~1e-5-tight in XYZ.
            assert!(
                (got[ch] - want[ch]).abs() < 1e-3,
                "shortcut keeps Lab(15,0,0): {got:?} vs {want:?}"
            );
        }
        // Under perceptual there is NO straightness shortcut: the same profile runs the
        // estimator (forward leg B2A0 with the lifted black, return leg the identity A2B1 —
        // outRamp = 15 + 0.85·inRamp), fitting the straight normalized ramp to a root at
        // L* ≈ 0 → near-zero XYZ, nowhere near the shortcut's value.
        let via_estimator = detect_destination_black_point(&profile, RenderingIntent::Perceptual);
        assert!(
            via_estimator[1].abs() < 2e-3,
            "estimator black stays near zero: {via_estimator:?}"
        );
    }

    #[test]
    fn shadow_toe_roots_the_fit_at_the_toe_break() {
        // A toe curve on the return leg — flat to L* = 10, then linear to 100 — makes the
        // round trip visibly non-straight, so the relative-intent estimator must run the
        // quadratic fit; the fit region (y ∈ [0.1, 0.5)) sits entirely on the linear section
        // whose root is the toe break L* = 10 → detected black Y ≈ Y(L* = 10) = 0.0113.
        // The bracket [Y(9), Y(11)] pins the fit-region bounds: widening the region into the
        // flat toe (a lo/hi mutation) biases the fitted root visibly out of it.
        let samples: Vec<u16> = (0..256u32)
            .map(|i| {
                let x = f64::from(i) / 255.0;
                let y = ((x - 0.1) / 0.9).clamp(0.0, 1.0);
                #[expect(clippy::cast_possible_truncation, reason = "clamped to u16 range")]
                #[expect(clippy::cast_sign_loss, reason = "y is non-negative")]
                {
                    (y * 65535.0 + 0.5) as u16
                }
            })
            .collect();
        let profile = roundtrip_profile(2, Curve::Identity, Curve::Sampled(samples));
        let got =
            detect_destination_black_point(&profile, RenderingIntent::MediaRelativeColorimetric);
        assert!(
            got[1] > 0.0095 && got[1] < 0.0135,
            "toe-break black Y ≈ 0.0113, got {got:?}"
        );
        // And it is not the fixed perceptual black (wrong path).
        assert_ne!(got, PERCEPTUAL_BLACK);
    }

    #[test]
    fn round_trip_composes_pcs_to_device_then_device_to_pcs() {
        let profile = roundtrip_profile(2, Curve::Identity, Curve::Identity);
        let rt = RoundTrip::new(&profile, RenderingIntent::MediaRelativeColorimetric)
            .expect("round trip builds");
        // The vehicle preserves L* end to end (2-node CLUT pass-through is linear-exact,
        // the Lab seams cancel) and pins a* = b* to exactly 0 on the return leg — so the
        // loop is the identity in L and the constant 0 in chroma.
        let lab = [37.5, 10.0, -20.0];
        let out = rt.eval(lab).expect("evaluates");
        assert!((out[0] - lab[0]).abs() < 1e-9, "L preserved: {out:?}");
        assert_eq!(out[1], 0.0, "a* pinned to zero: {out:?}");
        assert_eq!(out[2], 0.0, "b* pinned to zero: {out:?}");
    }

    #[test]
    fn relative_on_a_non_ink_output_profile_probes_the_colorant_not_the_round_trip() {
        // The ink gate is space-keyed: an OUTPUT-class RGB shaper at media-relative must take
        // the darker-colorant probe (chroma kept), never the ink round trip (which zeroes
        // chroma). A pedestal TRC lifts the black so the two paths differ observably: the
        // probe's black has non-zero a*/b* (the colorant sums are chromatic), the round trip
        // would have forced a* = b* = 0.
        let samples: Vec<u16> = (0..256u32)
            .map(|i| {
                let y = 0.1 + 0.9 * f64::from(i) / 255.0;
                #[expect(clippy::cast_possible_truncation, reason = "y is in [0, 1]")]
                #[expect(clippy::cast_sign_loss, reason = "y is non-negative")]
                {
                    (y * 65535.0 + 0.5) as u16
                }
            })
            .collect();
        let trc = || TagData::Curve(Curve::Sampled(samples.clone()));
        let xyz_tag = |v: [f64; 3]| TagData::Xyz(vec![XyzNumber::from_f64(v)]);
        // Exact-dyadic colorants (s15Fixed16-lossless) whose column sums (1.0, 1.0, 0.8125)
        // sit visibly off the D50 axis, so the probed black is decisively chromatic.
        let profile = IccProfile {
            header: ProfileHeader::new(DeviceClass::Output, ColorSpace::Rgb),
            tags: vec![
                (Signature(*b"rXYZ"), xyz_tag([0.5, 0.25, 0.0625])),
                (Signature(*b"gXYZ"), xyz_tag([0.375, 0.625, 0.125])),
                (Signature(*b"bXYZ"), xyz_tag([0.125, 0.125, 0.625])),
                (Signature(*b"rTRC"), trc()),
                (Signature(*b"gTRC"), trc()),
                (Signature(*b"bTRC"), trc()),
            ],
        };
        let got = detect_black_point(&profile, RenderingIntent::MediaRelativeColorimetric);
        // Hand-derived expectation: device black → the (quantized) 0.1 pedestal per channel
        // → XYZ = pedestal · column sums → Lab (chroma kept, L ≤ 50) → XYZ.
        let pedestal = f64::from(samples[0]) / 65535.0;
        let xyz = [pedestal, pedestal, 0.8125 * pedestal];
        let lab = xyz_to_lab(xyz, LCMS_D50);
        let want = lab_to_xyz([lab[0].min(50.0), lab[1], lab[2]], LCMS_D50);
        for ch in 0..3 {
            assert!(
                (got[ch] - want[ch]).abs() < 1e-9,
                "colorant probe with chroma: {got:?} vs {want:?}"
            );
        }
        // The chroma really is non-zero — the ink round trip's forced a* = b* = 0 would
        // land elsewhere.
        let achromatic = lab_to_xyz([lab[0].min(50.0), 0.0, 0.0], LCMS_D50);
        assert!(
            (got[0] - achromatic[0]).abs() > 1e-5,
            "the probe keeps chroma: {got:?} vs achromatic {achromatic:?}"
        );
    }

    #[test]
    fn lab_device_space_probes_the_v4_encoded_lab_black() {
        // A Lab *device* space (an output-class Lab printer): the darkest tuple is the
        // v4-encoded Lab(0, 0, 0) — (0, 0x8080, 0x8080), whose a*/b* decode to exactly 0.
        // A pedestal B-curve on the L channel lifts the probed black to a decisively
        // non-zero, exactly-achromatic value: a wrong darkest tuple (all-zero, decoding to
        // a* = b* = −128, or a dropped Lab endpoint entirely) cannot reproduce it.
        let pedestal: Vec<u16> = (0..256u32)
            .map(|i| {
                let y = 0.1 + 0.9 * f64::from(i) / 255.0;
                #[expect(clippy::cast_possible_truncation, reason = "y is in [0, 1]")]
                #[expect(clippy::cast_sign_loss, reason = "y is non-negative")]
                {
                    (y * 65535.0 + 0.5) as u16
                }
            })
            .collect();
        let mab = TagData::LutAToB(LutAToB {
            input_channels: 3,
            output_channels: 3,
            a_curves: None,
            clut: None,
            m_curves: None,
            matrix: None,
            b_curves: vec![
                CurveOrParametric::Curve(Curve::Sampled(pedestal.clone())),
                CurveOrParametric::Curve(Curve::Identity),
                CurveOrParametric::Curve(Curve::Identity),
            ],
        });
        let mut header = ProfileHeader::new(DeviceClass::Output, ColorSpace::Lab);
        header.pcs = ColorSpace::Lab;
        header.version.major = 2; // keep clear of the v4 fixed-black branch
        let profile = IccProfile {
            header,
            tags: vec![(Signature(*b"A2B1"), mab)],
        };
        let got = detect_black_point(&profile, RenderingIntent::MediaRelativeColorimetric);
        // L = pedestal(0) · 100 (v4 Lab decode), a* = b* = 0 exactly.
        let l = f64::from(pedestal[0]) / 65535.0 * 100.0;
        let want = lab_to_xyz([l, 0.0, 0.0], LCMS_D50);
        for ch in 0..3 {
            assert!(
                (got[ch] - want[ch]).abs() < 1e-12,
                "Lab-device probe: {got:?} vs {want:?}"
            );
        }
        assert_ne!(got, ZERO);
    }

    #[test]
    fn gray_and_cmy_device_endpoints_probe_their_own_darkest_tuple() {
        // Gray: a pedestal kTRC display shaper — device black 0 → pedestal → Y ≈ 0.1 · D50,
        // clipped through Lab. A dropped gray endpoint would zero this.
        let pedestal: Vec<u16> = (0..256u32)
            .map(|i| {
                let y = 0.1 + 0.9 * f64::from(i) / 255.0;
                #[expect(clippy::cast_possible_truncation, reason = "y is in [0, 1]")]
                #[expect(clippy::cast_sign_loss, reason = "y is non-negative")]
                {
                    (y * 65535.0 + 0.5) as u16
                }
            })
            .collect();
        let gray = IccProfile {
            header: ProfileHeader::new(DeviceClass::Display, ColorSpace::Gray),
            tags: vec![(
                Signature(*b"kTRC"),
                TagData::Curve(Curve::Sampled(pedestal.clone())),
            )],
        };
        let got = detect_black_point(&gray, RenderingIntent::MediaRelativeColorimetric);
        assert!(got[1] > 0.05, "gray pedestal black must be lifted: {got:?}");

        // CMY: the darkest tuple is all-100% ink. A v2 header + perceptual routes to the
        // darker-colorant probe; a constant CLUT makes any reached probe non-zero, so a
        // dropped CMY endpoint (→ zero) is visible.
        let cmy_lut = TagData::LutAToB(LutAToB {
            input_channels: 3,
            output_channels: 3,
            a_curves: None,
            clut: Some(Clut {
                grid_points: vec![2; 3],
                output_channels: 3,
                precision: ClutPrecision::U16,
                samples: vec![1000; 8 * 3],
            }),
            m_curves: None,
            matrix: None,
            b_curves: vec![CurveOrParametric::Curve(Curve::Identity); 3],
        });
        let mut header = ProfileHeader::new(DeviceClass::Output, ColorSpace::Cmy);
        header.pcs = ColorSpace::Lab;
        header.version.major = 2;
        let cmy = IccProfile {
            header,
            tags: vec![(Signature(*b"A2B0"), cmy_lut)],
        };
        let got = detect_black_point(&cmy, RenderingIntent::Perceptual);
        assert_ne!(got, ZERO, "CMY darkest tuple must reach the probe");
    }

    #[test]
    fn gray_shaper_arm_gates_the_v4_fixed_black() {
        // cmsIsMatrixShaper's gray arm: a v4 gray kTRC profile at perceptual takes the
        // darker-colorant probe (black = exactly 0 for a gamma TRC), never the fixed
        // perceptual black.
        let gray = IccProfile {
            header: ProfileHeader::new(DeviceClass::Display, ColorSpace::Gray),
            tags: vec![(
                Signature(*b"kTRC"),
                TagData::Curve(Curve::Gamma(U8Fixed8(0x0233))),
            )],
        };
        assert_eq!(gray.header.version.major, 4);
        assert_eq!(detect_black_point(&gray, RenderingIntent::Perceptual), ZERO);
        // Without the kTRC it is no longer a shaper → the fixed black returns.
        let bare = IccProfile {
            header: gray.header.clone(),
            tags: Vec::new(),
        };
        assert_eq!(
            detect_black_point(&bare, RenderingIntent::Perceptual),
            PERCEPTUAL_BLACK
        );
    }

    #[test]
    fn support_gate_blocks_the_probe_before_the_tag_fallback() {
        // A Display-class CMYK profile with only A2B0, probed at media-relative:
        // cmsIsIntentSupported has NO perceptual fallback, so the probe is refused (zero) —
        // even though the link builder itself WOULD fall back to A2B0 and produce the
        // decisively non-zero constant-CLUT value if the gate were bypassed.
        let mut profile = cmyk_output(vec![(Signature(*b"A2B0"), constant_a2b(1000))]);
        profile.header.device_class = DeviceClass::Display; // not Output: no ink round trip
        profile.header.version.major = 2; // keep perceptual on the probe path below
        assert_eq!(
            detect_black_point(&profile, RenderingIntent::MediaRelativeColorimetric),
            ZERO
        );
        // Proof the fallback would have produced non-zero: the same profile probed at
        // perceptual (its own tag) is non-zero.
        assert_ne!(
            detect_black_point(&profile, RenderingIntent::Perceptual),
            ZERO
        );
    }

    #[test]
    fn high_l_probe_clips_to_zero_not_fifty() {
        // The darker-colorant clip: L* > 95 ("synthetic negative profiles") zeroes the
        // black's L*, it does not clamp to 50 — pin the exact value so a broken clip
        // cascade (`>` vs `min(50)`) cannot pass. Constant CLUT 65000 → L ≈ 99.18 > 95.
        let mut profile = cmyk_output(vec![(Signature(*b"A2B0"), constant_a2b(65000))]);
        profile.header.version.major = 2; // darker-colorant path at perceptual
        let got = detect_black_point(&profile, RenderingIntent::Perceptual);
        // Expected: L → 0, chroma kept (a = b = 65000/65535·255 − 128).
        let ab = f64::from(65000u16) / 65535.0 * 255.0 - 128.0;
        let want = lab_to_xyz([0.0, ab, ab], LCMS_D50);
        for ch in 0..3 {
            assert!(
                (got[ch] - want[ch]).abs() < 1e-12,
                "high-L clip: {got:?} vs {want:?}"
            );
        }
    }
}
