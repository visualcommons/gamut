//! The object-safe entry trait every runnable colour transform implements, and
//! [`IccTransform`] — the first end-to-end transform type: two ICC profiles linked through
//! their common PCS, with the rendering-intent adjustments (ICC-absolute white scaling,
//! black-point compensation) applied at the seam.

use gamut_icc::{ColorSpace, DeviceClass, IccProfile, RenderingIntent};

use crate::error::{CmmError, Result};
use crate::pipeline::{Pipeline, Stage};
use crate::{bpc, intent, link};

/// A runnable colour transform: interleaved `f64` pixels in, interleaved `f64` pixels out.
///
/// The single entry point every CMM product implements — a [`Pipeline`] today;
/// linked profile transforms and chains in later phases. Object-safe by design (the
/// `gamut_heic::HevcDecoder` shape: one dispatchable method over borrowed data, plain data out),
/// so a transform can be boxed, held behind `&dyn Transform`, and later carried over the
/// C-portable seam.
///
/// # Buffer contract
///
/// `src` must hold `pixels × input_channels()` samples and `dst` exactly
/// `pixels × output_channels()` samples **for the same pixel count**; violations return
/// [`CmmError::BufferLength`]. Samples are interleaved per pixel
/// (e.g. `RGBRGB…`), never planar.
///
/// # Sample domain
///
/// Device channels are **encoded** values in `[0.0, 1.0]`; PCS seams are **decoded
/// colorimetry** — PCSXYZ carries XYZ with D50 luminance `Y = 1.0`, PCSLAB carries `L*` in
/// `0..=100` and `a*`/`b*` in their natural signed range. See the crate-level docs; every
/// stage added by later phases keeps this convention.
pub trait Transform {
    /// Transforms `src` into `dst`, pixel by pixel.
    ///
    /// # Errors
    ///
    /// Returns [`CmmError::BufferLength`] if `src` is not a
    /// whole number of `input_channels()`-sample pixels, or `dst` does not hold exactly the
    /// matching number of `output_channels()`-sample pixels.
    fn transform(&self, src: &[f64], dst: &mut [f64]) -> Result<()>;

    /// The number of samples this transform consumes per pixel.
    #[must_use]
    fn input_channels(&self) -> u8;

    /// The number of samples this transform produces per pixel.
    #[must_use]
    fn output_channels(&self) -> u8;
}

/// Options for building an [`IccTransform`].
///
/// `Default` matches lcms2's `cmsCreateTransform` defaults: [`RenderingIntent::Perceptual`],
/// black-point compensation off (though see the v4 forcing rule on
/// [`IccTransform::between`]). The adaptation state is fixed at lcms2's default `1.0`
/// (observer fully adapted) — the only state this crate implements, which is also what makes
/// the `chad` tag irrelevant to the absolute intent here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransformOptions {
    /// The rendering intent, selecting each profile's per-intent tables
    /// ([`crate::link`]'s tag dispatch) and the PCS-seam adjustment (ICC-absolute white
    /// scaling; BPC applies to the other three intents).
    pub intent: RenderingIntent,
    /// Apply black-point compensation ([`crate::bpc`]). Ignored for
    /// [`RenderingIntent::IccAbsoluteColorimetric`] (BPC and absolute rendering are mutually
    /// exclusive — lcms2 forces the flag off there, `cmscnvrt.c:1126-1127`), and forced *on*
    /// by the v4 rule documented on [`IccTransform::between`].
    pub black_point_compensation: bool,
}

impl Default for TransformOptions {
    fn default() -> Self {
        Self {
            intent: RenderingIntent::Perceptual,
            black_point_compensation: false,
        }
    }
}

/// A runnable colour transform between two ICC profiles: source device → PCS → destination
/// device, with the intent's PCS-seam adjustment applied in the middle. Built by
/// [`IccTransform::between`]; run through the [`Transform`] impl.
#[derive(Debug, Clone)]
#[must_use]
pub struct IccTransform {
    pipeline: Pipeline,
}

/// `MAX_ENCODEABLE_XYZ = 65535/32768` (lcms2 `lcms2_internal.h:71`): the factor between this
/// crate's decoded XYZ and lcms2's encoded pipelines, re-applied to the offset inside
/// [`is_empty_layer`] so the skip decision matches lcms2 exactly.
const MAX_ENCODEABLE_XYZ: f64 = 65535.0 / 32768.0;

/// lcms2's `IsEmptyLayer` (`cmscnvrt.c:329-348`): the adjustment is skipped when
/// `Σ|m − I| + Σ|off| < 0.002`. lcms2 evaluates the test *after* dividing the offset by
/// `MAX_ENCODEABLE_XYZ` for its encoded pipelines; this crate keeps the offset decoded, so
/// the same division is applied here (only here) to keep the decision bit-identical.
/// Shared with [`crate::bpc`]'s detection round trip, which carries the same layer.
pub(crate) fn is_empty_layer(m: &[[f64; 3]; 3], off: &[f64; 3]) -> bool {
    let mut diff = 0.0;
    for (i, row) in m.iter().enumerate() {
        for (j, cell) in row.iter().enumerate() {
            let id = if i == j { 1.0 } else { 0.0 };
            diff += (cell - id).abs();
        }
    }
    for component in off {
        diff += (component / MAX_ENCODEABLE_XYZ).abs();
    }
    diff < 0.002
}

/// The PCS-seam adjustment for the pair — lcms2's `ComputeConversion`
/// (`cmscnvrt.c:352-418`) at adaptation state 1.0, in the decoded domain (no
/// `MAX_ENCODEABLE_XYZ` division of the offset — the pipelines here carry decoded XYZ):
/// absolute → the white scaling; otherwise BPC when requested **or forced**; else identity.
fn conversion_layer(
    src: &IccProfile,
    dst: &IccProfile,
    options: TransformOptions,
) -> ([[f64; 3]; 3], [f64; 3]) {
    const IDENTITY: [[f64; 3]; 3] = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
    if options.intent == RenderingIntent::IccAbsoluteColorimetric {
        // BPC and absolute are mutually exclusive: the flag is ignored here.
        return (intent::absolute_scaling(src, dst), [0.0; 3]);
    }
    // The v4 forcing rule (_cmsLinkProfiles, cmscnvrt.c:1119-1135): BPC "applies always on
    // V4 perceptual and saturation", keyed per profile slot — and only the DESTINATION
    // slot's flag is consumed for a two-profile transform (ComputeConversion runs on the
    // output-direction hop), so the destination profile's version gates the force.
    let forced = matches!(
        options.intent,
        RenderingIntent::Perceptual | RenderingIntent::Saturation
    ) && dst.header.version.major >= 4;
    if options.black_point_compensation || forced {
        let black_in = bpc::detect_black_point(src, options.intent);
        let black_out = bpc::detect_destination_black_point(dst, options.intent);
        if let Some((m, off)) = bpc::compensation(black_in, black_out) {
            return (m, off);
        }
    }
    (IDENTITY, [0.0; 3])
}

/// Rejects the profile classes `between` cannot link (they need #330's transform-chaining
/// API, where lcms2 reads them devicelink-style: the A2B family in *both* directions).
fn reject_unchainable_class(profile: &IccProfile) -> Result<()> {
    match profile.header.device_class {
        DeviceClass::DeviceLink => Err(CmmError::UnsupportedProfile(
            "device-link profiles chain via issue #330's transform-chaining API",
        )),
        DeviceClass::Abstract => Err(CmmError::UnsupportedProfile(
            "abstract profiles chain via issue #330's transform-chaining API",
        )),
        DeviceClass::NamedColor => Err(CmmError::UnsupportedProfile(
            "named-colour profiles have no continuous pixel transform",
        )),
        _ => Ok(()),
    }
}

/// The profile's connection space, restricted to the two ICC PCSs.
fn connection_space(profile: &IccProfile) -> Result<ColorSpace> {
    match profile.header.pcs {
        pcs @ (ColorSpace::Xyz | ColorSpace::Lab) => Ok(pcs),
        _ => Err(CmmError::UnsupportedProfile(
            "profile linking requires an XYZ or Lab PCS",
        )),
    }
}

impl IccTransform {
    /// Links `src` and `dst` into one device→device transform at `options.intent`:
    /// `src`'s device→PCS half, the PCS bridge with the intent's adjustment, and `dst`'s
    /// PCS→device half ([`crate::link`] builds the halves; the assembly follows lcms2's
    /// `DefaultICCintents` + `AddConversion`, `cmscnvrt.c`).
    ///
    /// # The PCS seam
    ///
    /// The adjustment matrix acts in **XYZ**; Lab ends are bridged with
    /// [`Stage::LabToXyz`]/[`Stage::XyzToLab`] as needed (`AddConversion`'s four cases), and
    /// an adjustment within lcms2's empty-layer tolerance (`Σ|m − I| + Σ|off| < 0.002`, the
    /// offset compared in lcms2's encoded scale) inserts **no stage at all** — two profiles
    /// with equal media whites build the identical pipeline under absolute and
    /// media-relative, and a Lab→Lab pair with an empty layer gets no XYZ round trip.
    ///
    /// # Intents
    ///
    /// - **Perceptual / saturation:** no extra CMM math — the per-intent tag selection did
    ///   the work; for v4 profiles the perceptual tables already target the Perceptual
    ///   Reference Medium, and this CMM (like lcms2) applies no additional gamut mapping
    ///   (`references/cmm/ICCSpecRevision_22_02_05_PRMG.pdf`, `render.pdf`).
    /// - **ICC-absolute:** the media-white scaling `diag(wIn/wOut)` at the seam
    ///   (the crate's `intent` module); `options.black_point_compensation` is **ignored** (mutual
    ///   exclusion, as in lcms2). Implemented at adaptation state 1.0 only (the lcms2
    ///   default), where the `chad` tag is never read.
    /// - **BPC forcing:** black-point compensation is applied when requested — and **forced
    ///   on** for perceptual/saturation when the *destination* profile is v4 (encoded
    ///   version ≥ 4.0), replicating lcms2's `_cmsLinkProfiles` (`cmscnvrt.c:1119-1135`,
    ///   "BPC … applies always on V4 perceptual and saturation"; only the output-direction
    ///   hop's flag is consumed in a two-profile chain, hence the destination gates).
    ///   Detection failures compensate nothing (the zero-black convention, [`crate::bpc`]).
    ///
    /// # Errors
    ///
    /// [`CmmError::UnsupportedProfile`] for device-link, abstract, or named-colour profiles
    /// (chaining is issue #330's API) and for a PCS that is neither XYZ nor Lab; otherwise
    /// whatever [`device_to_pcs`](crate::link::device_to_pcs) /
    /// [`pcs_to_device`](crate::link::pcs_to_device) raise for the two halves
    /// (missing/mistyped tags, singular colorant matrices, non-invertible TRCs, …).
    pub fn between(src: &IccProfile, dst: &IccProfile, options: TransformOptions) -> Result<Self> {
        reject_unchainable_class(src)?;
        reject_unchainable_class(dst)?;
        let src_pcs = connection_space(src)?;
        let dst_pcs = connection_space(dst)?;
        let forward = link::device_to_pcs(src, options.intent)?;
        let reverse = link::pcs_to_device(dst, options.intent)?;

        let (m, off) = conversion_layer(src, dst, options);
        let mut seam = Vec::new();
        let adjust = Stage::Matrix { m, offset: off };
        let empty = is_empty_layer(&m, &off);
        // lcms2's AddConversion (cmscnvrt.c:420-489): the matrix acts in XYZ; Lab ends get
        // bridge stages, and an empty Lab→Lab layer inserts nothing at all.
        match (src_pcs == ColorSpace::Lab, dst_pcs == ColorSpace::Lab) {
            (false, false) => {
                if !empty {
                    seam.push(adjust);
                }
            }
            (false, true) => {
                if !empty {
                    seam.push(adjust);
                }
                seam.push(Stage::XyzToLab);
            }
            (true, false) => {
                seam.push(Stage::LabToXyz);
                if !empty {
                    seam.push(adjust);
                }
            }
            (true, true) => {
                if !empty {
                    seam.push(Stage::LabToXyz);
                    seam.push(adjust);
                    seam.push(Stage::XyzToLab);
                }
            }
        }
        let pipeline = forward
            .compose(Pipeline::new(3, 3, seam)?)?
            .compose(reverse)?;
        Ok(Self { pipeline })
    }

    /// The number of device channels consumed per source pixel.
    #[must_use]
    pub fn input_channels(&self) -> u8 {
        self.pipeline.input_channels()
    }

    /// The number of device channels produced per destination pixel.
    #[must_use]
    pub fn output_channels(&self) -> u8 {
        self.pipeline.output_channels()
    }
}

impl Transform for IccTransform {
    fn transform(&self, src: &[f64], dst: &mut [f64]) -> Result<()> {
        self.pipeline.transform(src, dst)
    }

    fn input_channels(&self) -> u8 {
        self.pipeline.input_channels()
    }

    fn output_channels(&self) -> u8 {
        self.pipeline.output_channels()
    }
}

#[cfg(test)]
mod tests {
    use super::Transform;
    use crate::{Pipeline, Stage};

    #[test]
    fn transform_is_object_safe() {
        let pipeline = Pipeline::new(3, 3, vec![Stage::Clamp { channels: 3 }]).unwrap();
        let dynamic: &dyn Transform = &pipeline;
        assert_eq!(dynamic.input_channels(), 3);
        assert_eq!(dynamic.output_channels(), 3);
        let mut dst = [0.0; 3];
        dynamic.transform(&[2.0, -1.0, 0.5], &mut dst).unwrap();
        assert_eq!(dst, [1.0, 0.0, 0.5]);
    }
}

#[cfg(test)]
mod icc_transform_tests {
    use gamut_icc::{
        ColorSpace, Curve, DeviceClass, IccProfile, ProfileHeader, RenderingIntent, Signature,
        TagData, U8Fixed8, XyzNumber,
    };

    use super::{IccTransform, Transform, TransformOptions, is_empty_layer};
    use crate::pipeline::Stage;

    const D65: [f64; 3] = [0.9504, 1.0, 1.0889];

    /// A v4 RGB→XYZ matrix/TRC display shaper over exact-dyadic colorants, with a hookable
    /// TRC and an optional `wtpt`.
    fn rgb_shaper(trc: TagData, wtpt: Option<[f64; 3]>) -> IccProfile {
        let xyz_tag = |v: [f64; 3]| TagData::Xyz(vec![XyzNumber::from_f64(v)]);
        let mut tags = vec![
            (Signature(*b"rXYZ"), xyz_tag([0.5, 0.25, 0.0625])),
            (Signature(*b"gXYZ"), xyz_tag([0.375, 0.625, 0.125])),
            (Signature(*b"bXYZ"), xyz_tag([0.125, 0.125, 0.625])),
            (Signature(*b"rTRC"), trc.clone()),
            (Signature(*b"gTRC"), trc.clone()),
            (Signature(*b"bTRC"), trc),
        ];
        if let Some(white) = wtpt {
            tags.push((Signature(*b"wtpt"), xyz_tag(white)));
        }
        IccProfile {
            header: ProfileHeader::new(DeviceClass::Display, ColorSpace::Rgb),
            tags,
        }
    }

    fn gamma() -> TagData {
        TagData::Curve(Curve::Gamma(U8Fixed8(0x0233)))
    }

    /// A monotonic pedestal TRC `y = 0.1 + 0.9·x`: its device black maps to a decisively
    /// non-zero XYZ, giving detection something to compensate.
    fn pedestal() -> TagData {
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
        TagData::Curve(Curve::Sampled(samples))
    }

    /// The stage-kind fingerprint of a transform's pipeline.
    fn kinds(transform: &IccTransform) -> Vec<&'static str> {
        transform
            .pipeline
            .stages()
            .iter()
            .map(|stage| match stage {
                Stage::Curves(_) => "curves",
                Stage::Matrix { .. } => "matrix",
                Stage::XyzToLab => "xyz2lab",
                Stage::LabToXyz => "lab2xyz",
                _ => "other",
            })
            .collect()
    }

    fn eval(transform: &IccTransform, rgb: [f64; 3]) -> [f64; 3] {
        let mut out = [0.0; 3];
        transform.transform(&rgb, &mut out).unwrap();
        out
    }

    #[test]
    fn options_default_is_perceptual_without_bpc() {
        let options = TransformOptions::default();
        assert_eq!(options.intent, RenderingIntent::Perceptual);
        assert!(!options.black_point_compensation);
    }

    #[test]
    fn empty_layer_threshold_boundaries_match_lcms2() {
        const I: [[f64; 3]; 3] = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
        assert!(is_empty_layer(&I, &[0.0; 3]), "exact identity is empty");
        // Matrix side: the summed deviation is compared against 0.002 directly.
        let mut m = I;
        m[0][0] = 1.0 + 0.0019;
        assert!(is_empty_layer(&m, &[0.0; 3]), "0.0019 < 0.002");
        m[0][0] = 1.0 + 0.0021;
        assert!(!is_empty_layer(&m, &[0.0; 3]), "0.0021 > 0.002");
        // Offset side: lcms2 tests the ENCODED offset (÷ 65535/32768), so a decoded offset
        // just under 0.004 still counts as empty — the divisor is load-bearing.
        assert!(is_empty_layer(&I, &[0.0039, 0.0, 0.0]), "0.00195 encoded");
        assert!(!is_empty_layer(&I, &[0.0041, 0.0, 0.0]), "0.00205 encoded");
        // The exact boundary is reachable in f64 — (0.002·C)/C round-trips to exactly 0.002
        // for C = 65535/32768 — and lcms2's `< 0.002` treats it as NOT empty.
        let boundary = 0.002 * (65535.0 / 32768.0);
        assert_eq!(boundary / (65535.0 / 32768.0), 0.002, "round-trip is exact");
        assert!(!is_empty_layer(&I, &[boundary, 0.0, 0.0]), "diff == 0.002");
    }

    #[test]
    fn absolute_with_equal_whites_builds_exactly_the_relative_pipeline() {
        // Both profiles default their media white to D50: diag(1,1,1) is an empty layer, so
        // the absolute pipeline is stage-for-stage the relative one (adjustment skipped).
        let src = rgb_shaper(gamma(), None);
        let dst = rgb_shaper(gamma(), None);
        let absolute = IccTransform::between(
            &src,
            &dst,
            TransformOptions {
                intent: RenderingIntent::IccAbsoluteColorimetric,
                black_point_compensation: false,
            },
        )
        .unwrap();
        let relative = IccTransform::between(
            &src,
            &dst,
            TransformOptions {
                intent: RenderingIntent::MediaRelativeColorimetric,
                black_point_compensation: false,
            },
        )
        .unwrap();
        // Curves → Matrix (src) → Matrix → Curves (dst): 4 stages, no seam stage.
        assert_eq!(kinds(&absolute), ["curves", "matrix", "matrix", "curves"]);
        assert_eq!(kinds(&absolute), kinds(&relative));
        for rgb in [[0.0; 3], [1.0; 3], [0.25, 0.5, 0.75]] {
            assert_eq!(eval(&absolute, rgb), eval(&relative, rgb));
        }
    }

    #[test]
    fn absolute_with_a_non_d50_white_inserts_the_diagonal_scaling() {
        let src = rgb_shaper(gamma(), Some(D65));
        let dst = rgb_shaper(gamma(), None);
        let absolute = IccTransform::between(
            &src,
            &dst,
            TransformOptions {
                intent: RenderingIntent::IccAbsoluteColorimetric,
                black_point_compensation: true, // must be ignored under absolute
            },
        )
        .unwrap();
        assert_eq!(
            kinds(&absolute),
            ["curves", "matrix", "matrix", "matrix", "curves"]
        );
        // The seam stage is diag(wIn/D50) with zero offset.
        let Stage::Matrix { m, offset } = &absolute.pipeline.stages()[2] else {
            panic!("stage 2 must be the white scaling");
        };
        let w = XyzNumber::from_f64(D65).to_f64();
        for i in 0..3 {
            assert!((m[i][i] - w[i] / crate::intent::LCMS_D50[i]).abs() < 1e-15);
        }
        assert_eq!(*offset, [0.0; 3]);
        // And it renders measurably differently from relative: white maps off-white.
        let relative = IccTransform::between(
            &src,
            &dst,
            TransformOptions {
                intent: RenderingIntent::MediaRelativeColorimetric,
                black_point_compensation: false,
            },
        )
        .unwrap();
        let abs_white = eval(&absolute, [1.0; 3]);
        let rel_white = eval(&relative, [1.0; 3]);
        let gap: f64 = (0..3).map(|ch| (abs_white[ch] - rel_white[ch]).abs()).sum();
        assert!(gap > 0.05, "absolute must diverge at white: {gap}");
    }

    #[test]
    fn lab_lab_seam_shapes() {
        // Lab-PCS shapers on both sides. Empty layer (relative): no seam stage at all — the
        // XyzToLab/LabToXyz already inside the halves stay, but no XYZ round trip between.
        let mut src = rgb_shaper(gamma(), Some(D65));
        src.header.pcs = ColorSpace::Lab;
        let mut dst = rgb_shaper(gamma(), None);
        dst.header.pcs = ColorSpace::Lab;
        let relative = IccTransform::between(
            &src,
            &dst,
            TransformOptions {
                intent: RenderingIntent::MediaRelativeColorimetric,
                black_point_compensation: false,
            },
        )
        .unwrap();
        assert_eq!(
            kinds(&relative),
            ["curves", "matrix", "xyz2lab", "lab2xyz", "matrix", "curves"]
        );
        // Non-empty layer (absolute, D65 vs D50 whites): Lab→Lab wraps the matrix in the
        // XYZ round trip — LabToXyz → adjust → XyzToLab at the seam.
        let absolute = IccTransform::between(
            &src,
            &dst,
            TransformOptions {
                intent: RenderingIntent::IccAbsoluteColorimetric,
                black_point_compensation: false,
            },
        )
        .unwrap();
        assert_eq!(
            kinds(&absolute),
            [
                "curves", "matrix", "xyz2lab", // src half
                "lab2xyz", "matrix", "xyz2lab", // seam
                "lab2xyz", "matrix", "curves" // dst half
            ]
        );
    }

    #[test]
    fn mixed_pcs_seam_shapes() {
        // XYZ→Lab, non-empty: adjust BEFORE the XyzToLab bridge.
        let src = rgb_shaper(gamma(), Some(D65));
        let mut dst = rgb_shaper(gamma(), None);
        dst.header.pcs = ColorSpace::Lab;
        let options = TransformOptions {
            intent: RenderingIntent::IccAbsoluteColorimetric,
            black_point_compensation: false,
        };
        let xyz_to_lab = IccTransform::between(&src, &dst, options).unwrap();
        assert_eq!(
            kinds(&xyz_to_lab),
            [
                "curves", "matrix", "matrix", "xyz2lab", "lab2xyz", "matrix", "curves"
            ]
        );
        // Lab→XYZ, non-empty: the LabToXyz bridge, THEN the adjust.
        let mut src_lab = rgb_shaper(gamma(), Some(D65));
        src_lab.header.pcs = ColorSpace::Lab;
        let dst_xyz = rgb_shaper(gamma(), None);
        let lab_to_xyz = IccTransform::between(&src_lab, &dst_xyz, options).unwrap();
        assert_eq!(
            kinds(&lab_to_xyz),
            [
                "curves", "matrix", "xyz2lab", "lab2xyz", "matrix", "matrix", "curves"
            ]
        );
        // Empty variants keep only the bridge stage(s).
        let rel = TransformOptions {
            intent: RenderingIntent::MediaRelativeColorimetric,
            black_point_compensation: false,
        };
        let xyz_to_lab = IccTransform::between(&src, &dst, rel).unwrap();
        assert_eq!(
            kinds(&xyz_to_lab),
            ["curves", "matrix", "xyz2lab", "lab2xyz", "matrix", "curves"]
        );
    }

    #[test]
    fn v4_destination_forces_bpc_under_perceptual_and_saturation() {
        // Source with a lifted black (pedestal TRC) so the black points differ and the
        // compensation layer is observable; destination a plain v4 shaper (black = 0).
        let src = rgb_shaper(pedestal(), None);
        let dst = rgb_shaper(gamma(), None);
        assert!(dst.header.version.major >= 4);
        let with_seam = ["curves", "matrix", "matrix", "matrix", "curves"];
        let without_seam = ["curves", "matrix", "matrix", "curves"];
        for intent in [RenderingIntent::Perceptual, RenderingIntent::Saturation] {
            // BPC not requested — forced anyway (v4 destination).
            let forced = IccTransform::between(
                &src,
                &dst,
                TransformOptions {
                    intent,
                    black_point_compensation: false,
                },
            )
            .unwrap();
            assert_eq!(kinds(&forced), with_seam, "{intent:?} forced");
            // Requesting it changes nothing (already on): identical stages and outputs.
            let requested = IccTransform::between(
                &src,
                &dst,
                TransformOptions {
                    intent,
                    black_point_compensation: true,
                },
            )
            .unwrap();
            assert_eq!(kinds(&requested), with_seam);
            for rgb in [[0.0; 3], [0.3, 0.6, 0.9]] {
                assert_eq!(eval(&forced, rgb), eval(&requested, rgb), "{intent:?}");
            }
        }
        // Media-relative is NEVER forced: default options build without the seam stage.
        let relative = IccTransform::between(
            &src,
            &dst,
            TransformOptions {
                intent: RenderingIntent::MediaRelativeColorimetric,
                black_point_compensation: false,
            },
        )
        .unwrap();
        assert_eq!(kinds(&relative), without_seam, "relative not forced");
        // A v2 destination is NOT forced under perceptual…
        let mut dst_v2 = rgb_shaper(gamma(), None);
        dst_v2.header.version.major = 2;
        let unforced = IccTransform::between(
            &src,
            &dst_v2,
            TransformOptions {
                intent: RenderingIntent::Perceptual,
                black_point_compensation: false,
            },
        )
        .unwrap();
        assert_eq!(kinds(&unforced), without_seam, "v2 destination not forced");
        // …but honours an explicit request.
        let requested_v2 = IccTransform::between(
            &src,
            &dst_v2,
            TransformOptions {
                intent: RenderingIntent::Perceptual,
                black_point_compensation: true,
            },
        )
        .unwrap();
        assert_eq!(kinds(&requested_v2), with_seam, "v2 explicit request");
        // The gate keys on the DESTINATION version: a v2 source before a v4 destination
        // still forces.
        let mut src_v2 = rgb_shaper(pedestal(), None);
        src_v2.header.version.major = 2;
        let forced = IccTransform::between(
            &src_v2,
            &dst,
            TransformOptions {
                intent: RenderingIntent::Perceptual,
                black_point_compensation: false,
            },
        )
        .unwrap();
        assert_eq!(kinds(&forced), with_seam, "v2 source, v4 destination");
    }

    #[test]
    fn equal_black_points_skip_the_compensation_stage() {
        // Two plain gamma shapers: both detected blacks are exactly zero, so BPC=true
        // compensates nothing and the pipeline is bitwise the BPC=false one.
        let src = rgb_shaper(gamma(), None);
        let dst = rgb_shaper(gamma(), None);
        let on = IccTransform::between(
            &src,
            &dst,
            TransformOptions {
                intent: RenderingIntent::MediaRelativeColorimetric,
                black_point_compensation: true,
            },
        )
        .unwrap();
        let off = IccTransform::between(
            &src,
            &dst,
            TransformOptions {
                intent: RenderingIntent::MediaRelativeColorimetric,
                black_point_compensation: false,
            },
        )
        .unwrap();
        assert_eq!(kinds(&on), kinds(&off));
        for rgb in [[0.0; 3], [0.7, 0.2, 0.5]] {
            assert_eq!(eval(&on, rgb), eval(&off, rgb));
        }
    }

    #[test]
    fn bpc_maps_the_lifted_source_black_to_the_destination_black() {
        // End-to-end property: with BPC on, the source's lifted black (pedestal TRC) lands
        // on the destination's true black — device black round-trips to device black.
        let src = rgb_shaper(pedestal(), None);
        let dst = rgb_shaper(gamma(), None);
        let on = IccTransform::between(
            &src,
            &dst,
            TransformOptions {
                intent: RenderingIntent::MediaRelativeColorimetric,
                black_point_compensation: true,
            },
        )
        .unwrap();
        let off = IccTransform::between(
            &src,
            &dst,
            TransformOptions {
                intent: RenderingIntent::MediaRelativeColorimetric,
                black_point_compensation: false,
            },
        )
        .unwrap();
        let black_on = eval(&on, [0.0; 3]);
        let black_off = eval(&off, [0.0; 3]);
        // Uncompensated: the pedestal black (Y = 0.1) is far from device 0. Compensated:
        // near it. (Detection clips the source black's L* to ≤ 50, so the mapping is exact
        // only in the toe — assert direction and magnitude, not identity.)
        let sum = |v: [f64; 3]| v.iter().sum::<f64>();
        assert!(
            sum(black_on) < 0.6 * sum(black_off),
            "BPC must pull the black down: on {black_on:?} vs off {black_off:?}"
        );
    }

    #[test]
    fn unchainable_classes_and_foreign_pcs_are_rejected() {
        let good = rgb_shaper(gamma(), None);
        let cases = [
            (
                DeviceClass::DeviceLink,
                "cmm: unsupported profile (device-link profiles chain via issue #330's transform-chaining API)",
            ),
            (
                DeviceClass::Abstract,
                "cmm: unsupported profile (abstract profiles chain via issue #330's transform-chaining API)",
            ),
            (
                DeviceClass::NamedColor,
                "cmm: unsupported profile (named-colour profiles have no continuous pixel transform)",
            ),
        ];
        for (class, message) in cases {
            let mut bad = rgb_shaper(gamma(), None);
            bad.header.device_class = class;
            let err = IccTransform::between(&bad, &good, TransformOptions::default()).unwrap_err();
            assert_eq!(err.to_string(), message, "{class:?} as source");
            let err = IccTransform::between(&good, &bad, TransformOptions::default()).unwrap_err();
            assert_eq!(err.to_string(), message, "{class:?} as destination");
        }
        // A non-connection-space PCS on a chainable class is its own refusal.
        let mut odd = rgb_shaper(gamma(), None);
        odd.header.pcs = ColorSpace::Rgb;
        let err = IccTransform::between(&odd, &good, TransformOptions::default()).unwrap_err();
        assert_eq!(
            err.to_string(),
            "cmm: unsupported profile (profile linking requires an XYZ or Lab PCS)"
        );
    }

    #[test]
    fn icc_transform_runs_through_the_transform_trait() {
        let src = rgb_shaper(gamma(), None);
        let dst = rgb_shaper(gamma(), None);
        let transform = IccTransform::between(
            &src,
            &dst,
            TransformOptions {
                intent: RenderingIntent::MediaRelativeColorimetric,
                black_point_compensation: false,
            },
        )
        .unwrap();
        assert_eq!(transform.input_channels(), 3);
        assert_eq!(transform.output_channels(), 3);
        let dynamic: &dyn Transform = &transform;
        assert_eq!(dynamic.input_channels(), 3);
        assert_eq!(dynamic.output_channels(), 3);
        // Identical profiles at relative: the round trip is the identity to f64 tightness.
        let src_pixels = [0.25, 0.5, 0.75, 1.0, 1.0, 1.0];
        let mut out = [0.0; 6];
        dynamic.transform(&src_pixels, &mut out).unwrap();
        for (got, want) in out.iter().zip(src_pixels) {
            assert!((got - want).abs() < 1e-9, "{out:?}");
        }
    }
}
