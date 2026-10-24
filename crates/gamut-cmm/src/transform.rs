//! The object-safe entry trait every runnable colour transform implements, and
//! [`IccTransform`] — the first end-to-end transform type: two ICC profiles linked through
//! their common PCS, with the rendering-intent adjustments (ICC-absolute white scaling,
//! black-point compensation) applied at the seam.

use gamut_icc::{ColorSpace, DeviceClass, IccProfile, RenderingIntent};

use crate::chain;
use crate::error::{CmmError, Result};
use crate::optimize::PipelineOptimization;
use crate::pipeline::{MAX_CHANNELS, Pipeline};

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

    /// Transforms interleaved **16-bit** samples: each code `v` is a device value `v / 65535`.
    ///
    /// The entry point [`crate::transform_interleaved_u16`] and its planar sibling run colour
    /// channels through. The provided implementation is exactly that normalization, then
    /// [`transform`](Transform::transform), then lcms2's `_cmsQuickSaturateWord` re-encoding
    /// (`floor(v · 65535 + 0.5)`, saturating, NaN → 0), so a transform that overrides nothing
    /// keeps its `f64` numerics. A transform with a native integer path overrides it — a
    /// pipeline resampled at
    /// [`PipelineOptimization::PrecalculateFixed16`](crate::PipelineOptimization::PrecalculateFixed16)
    /// interpolates the codes directly, with no float conversion on either side.
    ///
    /// # Errors
    ///
    /// The buffer contract of [`transform`](Transform::transform), in samples:
    /// [`CmmError::BufferLength`] if `src` is not whole pixels or `dst` does not hold the
    /// matching pixel count; plus anything `transform` itself returns.
    fn transform_u16(&self, src: &[u16], dst: &mut [u16]) -> Result<()> {
        through_f64(self, src, dst, 65535.0, encode_u16)
    }

    /// Transforms interleaved **8-bit** samples: each code `v` is a device value `v / 255`.
    ///
    /// As [`transform_u16`](Transform::transform_u16) at 8 bits: the provided implementation
    /// normalizes by 255, calls [`transform`](Transform::transform), and re-encodes with
    /// `floor(v · 255 + 0.5)`, saturating. A
    /// [`PrecalculateFixed16`](crate::PipelineOptimization::PrecalculateFixed16) pipeline
    /// instead widens each code to 16 bits and narrows the result back the way lcms2's 8-bit
    /// formatters do (`FROM_8_TO_16`, `FROM_16_TO_8`).
    ///
    /// # Errors
    ///
    /// As [`transform_u16`](Transform::transform_u16).
    fn transform_u8(&self, src: &[u8], dst: &mut [u8]) -> Result<()> {
        through_f64(self, src, dst, 255.0, encode_u8)
    }

    /// The number of samples this transform consumes per pixel.
    #[must_use]
    fn input_channels(&self) -> u8;

    /// The number of samples this transform produces per pixel.
    #[must_use]
    fn output_channels(&self) -> u8;
}

/// Pixels per refill of the stack scratch the provided integer entry points convert through.
const THROUGH_F64_PIXELS: usize = 64;

/// Checks the [`Transform`] buffer contract for `src_len` input and `dst_len` output samples
/// of a transform with the given per-pixel channel counts.
///
/// # Errors
///
/// [`CmmError::BufferLength`] if `src_len` is not a whole number of input pixels, or
/// `dst_len` is not the matching number of output samples.
pub(crate) fn check_buffers(
    input_channels: u8,
    output_channels: u8,
    src_len: usize,
    dst_len: usize,
) -> Result<()> {
    let n_in = usize::from(input_channels);
    if !src_len.is_multiple_of(n_in) {
        return Err(CmmError::BufferLength {
            channels: input_channels,
            found: src_len,
        });
    }
    if dst_len != (src_len / n_in) * usize::from(output_channels) {
        return Err(CmmError::BufferLength {
            channels: output_channels,
            found: dst_len,
        });
    }
    Ok(())
}

/// The provided integer entry points: normalize each code by `full_scale`, run the `f64`
/// [`Transform::transform`] over a stack chunk, and re-encode with `encode`. Constant extra
/// memory (two `THROUGH_F64_PIXELS × MAX_CHANNELS` `f64` arrays, 16 KiB), no allocation.
///
/// # Errors
///
/// As [`Transform::transform_u16`].
pub(crate) fn through_f64<T, S>(
    transform: &T,
    src: &[S],
    dst: &mut [S],
    full_scale: f64,
    encode: fn(f64) -> S,
) -> Result<()>
where
    T: Transform + ?Sized,
    S: Copy + Into<f64>,
{
    let (n_in, n_out) = (transform.input_channels(), transform.output_channels());
    check_buffers(n_in, n_out, src.len(), dst.len())?;
    let mut fin = [0.0_f64; THROUGH_F64_PIXELS * MAX_CHANNELS as usize];
    let mut fout = [0.0_f64; THROUGH_F64_PIXELS * MAX_CHANNELS as usize];
    let src_chunks = src.chunks(THROUGH_F64_PIXELS * usize::from(n_in));
    let dst_chunks = dst.chunks_mut(THROUGH_F64_PIXELS * usize::from(n_out));
    for (s, d) in src_chunks.zip(dst_chunks) {
        for (f, &code) in fin.iter_mut().zip(s) {
            *f = code.into() / full_scale;
        }
        transform.transform(&fin[..s.len()], &mut fout[..d.len()])?;
        for (code, &f) in d.iter_mut().zip(fout.iter()) {
            *code = encode(f);
        }
    }
    Ok(())
}

/// lcms2's `_cmsQuickSaturateWord` rounding at 8 bits: `floor(v · 255 + 0.5)`, saturating to
/// the sample range (NaN saturates to 0).
pub(crate) fn encode_u8(v: f64) -> u8 {
    // `as` saturates (NaN → 0), so the clamp only needs the rounding to be done first.
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "clamped to the u8 range"
    )]
    {
        (v * 255.0 + 0.5).floor().clamp(0.0, 255.0) as u8
    }
}

/// lcms2's `_cmsQuickSaturateWord`: `floor(v · 65535 + 0.5)`, saturating (NaN → 0) — the
/// same encoding a [`NodePrecision::Fixed16`](crate::clut::NodePrecision::Fixed16) table
/// quantizes its nodes with.
pub(crate) fn encode_u16(v: f64) -> u16 {
    crate::clut::quantize16(v)
}

/// lcms2's `FROM_8_TO_16` (`lcms2_internal.h`): `v · 257`, the exact 8 → 16-bit widening
/// (`v / 255 = v·257 / 65535`).
pub(crate) fn widen_u8(v: u8) -> u16 {
    u16::from(v) * 257
}

/// lcms2's `FROM_16_TO_8` (`lcms2_internal.h`): `(v · 65281 + 8388608) >> 24`, the
/// round-to-nearest 16 → 8-bit narrowing lcms2's 8-bit formatters apply.
pub(crate) fn narrow_u16(v: u16) -> u8 {
    // In range: (65535 · 65281 + 8388608) >> 24 = 255.
    ((u32::from(v) * 65281 + 8_388_608) >> 24) as u8
}

/// Options for building an [`IccTransform`].
///
/// `Default` matches lcms2's `cmsCreateTransform` defaults in the two knobs lcms2 also
/// defaults: [`RenderingIntent::Perceptual`] and black-point compensation off (though see the
/// v4 forcing rule on [`IccTransform::between`]). Pipeline optimization deliberately does
/// **not** follow lcms2's default — it defaults *off*, so this crate's documented
/// stage-by-stage numerics are what an unconfigured transform produces. The adaptation state
/// is fixed at lcms2's default `1.0` (observer fully adapted) — the only state this crate
/// implements, which is also what makes the `chad` tag irrelevant to the absolute intent
/// here.
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
    /// How far to collapse the linked pipeline before evaluating it ([`crate::optimize`]).
    /// [`PipelineOptimization::None`] — the default — evaluates the link stage by stage,
    /// exactly as this crate's conformance gate pins it; the other levels trade a stated
    /// precision budget for pixel-buffer throughput.
    pub optimization: PipelineOptimization,
    /// Evaluate a matrix/TRC profile's tone curves in **closed form** from the transfer
    /// characteristics its `cicp` tag (ICC.1:2022 §10.3) names, instead of from its
    /// `rTRC`/`gTRC`/`bTRC`/`kTRC` tags. Default **off**.
    ///
    /// A curve is replaced only when all three hold: the profile carries a `cicp` tag;
    /// `gamut-color` evaluates its `TransferCharacteristics` code point
    /// ([`gamut_color::transfer::standard_eotf_for`] — today 1, 6, 8, 13, 14, 15, 16 and 18); and the
    /// tag's curve agrees with the closed form to within `2⁻⁸` at every probe of a
    /// 4096-point grid. The last condition is §10.3's own requirement that the tag name an
    /// encoding *equivalent* to the profile's, checked rather than assumed: a profile whose
    /// `cicp` tag contradicts its TRC renders through the TRC either way. LUT-based profiles
    /// are untouched — their curves are not the profile's transfer function.
    ///
    /// What it buys is precision in the **PCS → device** direction of a sampled curve, whose
    /// inverse otherwise comes from lcms2's numeric table reversal. Measured on the BT.2100 PQ
    /// built-in profile (`gamut_icc::BuiltinProfile::Bt2020Pq`, a 1024-entry `curveType`):
    /// the reversed table misses ST 2084's inverse by up to `5.5e-2` of signal at black (the
    /// table's leading run quantizes to zero, so linear `0` encodes to `≈ 0.055`), `3.6e-2`
    /// around 1 cd/m², `3.7e-4` above 10 cd/m² — and the closed form is exact. Device → PCS
    /// gains only the table's `1.5e-5`, and a parametric sRGB or BT.709 curve under `1e-5`
    /// either way.
    ///
    /// It does **not** buy speed. On a 256×256 8-bit buffer (`cargo bench -p gamut-cmm --
    /// cicp_transfer`), sRGB → PQ runs 1.2–1.7× slower with the option on and PQ → sRGB no
    /// faster: ST 2084's closed form costs two `powf` per sample where the sampled curve
    /// costs one interpolation. Hence the default: off keeps every transform built exactly as
    /// lcms2 builds it — the oracle reads the TRC tags — and spends nothing a caller did not
    /// ask for.
    pub prefer_cicp_transfer: bool,
}

impl Default for TransformOptions {
    fn default() -> Self {
        Self {
            intent: RenderingIntent::Perceptual,
            black_point_compensation: false,
            optimization: PipelineOptimization::None,
            prefer_cicp_transfer: false,
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

/// Rejects the profile classes `between` cannot link (they chain via
/// [`IccTransform::chain`], where lcms2 reads them devicelink-style: the A2B family in
/// *both* directions).
fn reject_unchainable_class(profile: &IccProfile) -> Result<()> {
    match profile.header.device_class {
        DeviceClass::DeviceLink => Err(CmmError::UnsupportedProfile(
            "device-link profiles run via IccTransform::device_link or IccTransform::chain",
        )),
        DeviceClass::Abstract => Err(CmmError::UnsupportedProfile(
            "abstract profiles chain via IccTransform::chain",
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
    /// [`crate::Stage::LabToXyz`]/[`crate::Stage::XyzToLab`] as needed (`AddConversion`'s
    /// four cases), and
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
    /// (the first two run via [`IccTransform::device_link`]/[`IccTransform::chain`]) and for
    /// a PCS that is neither XYZ nor Lab; otherwise
    /// whatever [`device_to_pcs`](crate::link::device_to_pcs) /
    /// [`pcs_to_device`](crate::link::pcs_to_device) raise for the two halves
    /// (missing/mistyped tags, singular colorant matrices, non-invertible TRCs, …).
    pub fn between(src: &IccProfile, dst: &IccProfile, options: TransformOptions) -> Result<Self> {
        reject_unchainable_class(src)?;
        reject_unchainable_class(dst)?;
        connection_space(src)?;
        connection_space(dst)?;
        // `between` IS the two-profile chain: one seam implementation for pairs, chains,
        // and proofing transforms alike (crate::chain, lcms2's DefaultICCintents).
        let intents = [options.intent; 2];
        let bpc_flags = [options.black_point_compensation; 2];
        let pipeline = chain::link_chain(
            &[src, dst],
            &intents,
            &bpc_flags,
            options.prefer_cicp_transfer,
        )?;
        Self::from_pipeline(pipeline, options.optimization)
    }

    /// Wraps an internally-built pipeline (the chain/devicelink/proofing constructors in
    /// [`crate::chain`]), applying the requested optimization level.
    ///
    /// The **one funnel** every [`IccTransform`] constructor passes through, so the
    /// optimization knob cannot be forgotten by a new one.
    ///
    /// # Errors
    ///
    /// Whatever [`Pipeline::optimized`] reports.
    pub(crate) fn from_pipeline(
        pipeline: Pipeline,
        optimization: PipelineOptimization,
    ) -> Result<Self> {
        Ok(Self {
            pipeline: pipeline.optimized(optimization)?,
        })
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

    fn transform_u16(&self, src: &[u16], dst: &mut [u16]) -> Result<()> {
        self.pipeline.transform_u16(src, dst)
    }

    fn transform_u8(&self, src: &[u8], dst: &mut [u8]) -> Result<()> {
        self.pipeline.transform_u8(src, dst)
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
    fn rounding_is_half_up_with_saturation() {
        use super::{encode_u8, encode_u16};
        assert_eq!(encode_u8(0.5), 128, "127.5 rounds up");
        assert_eq!(encode_u8(1.0), 255);
        assert_eq!(encode_u8(1.5), 255, "saturates high");
        assert_eq!(encode_u8(-0.2), 0, "saturates low");
        assert_eq!(encode_u8(f64::NAN), 0, "NaN saturates to 0");
        assert_eq!(encode_u16(0.5), 32768, "32767.5 rounds up");
        assert_eq!(encode_u16(1.0), 65535);
        assert_eq!(encode_u16(2.0), 65535);
        assert_eq!(encode_u16(-1.0), 0);
        assert_eq!(encode_u16(f64::NAN), 0, "NaN saturates to 0");
    }

    #[test]
    fn widen_and_narrow_are_lcms2_from_8_to_16_and_from_16_to_8() {
        use super::{narrow_u16, widen_u8};
        for v in 0..=u8::MAX {
            assert_eq!(
                widen_u8(v),
                u16::from(v) << 8 | u16::from(v),
                "FROM_8_TO_16({v})"
            );
            assert_eq!(narrow_u16(widen_u8(v)), v, "the round trip is the identity");
        }
        // FROM_16_TO_8 is round-to-nearest of v · 255 / 65535 for every code.
        for code in 0..=u16::MAX {
            let nearest = (u32::from(code) * 255 * 2 + 65535) / (65535 * 2);
            assert_eq!(u32::from(narrow_u16(code)), nearest, "FROM_16_TO_8({code})");
        }
    }

    /// A transform overriding nothing but the required methods: `out = in / 2 + 1/4` on one
    /// channel in, two out (the second channel inverted), so the provided integer entry
    /// points are exercised exactly as a third-party `Transform` sees them.
    struct HalfAndInverse;

    impl Transform for HalfAndInverse {
        fn transform(&self, src: &[f64], dst: &mut [f64]) -> crate::Result<()> {
            super::check_buffers(1, 2, src.len(), dst.len())?;
            for (v, out) in src.iter().zip(dst.as_chunks_mut::<2>().0) {
                *out = [v / 2.0 + 0.25, 1.0 - v];
            }
            Ok(())
        }

        fn input_channels(&self) -> u8 {
            1
        }

        fn output_channels(&self) -> u8 {
            2
        }
    }

    #[test]
    fn provided_integer_entry_points_normalize_transform_and_round() {
        use super::{encode_u8, encode_u16};
        // 150 pixels: two full 64-pixel stack chunks and a partial one, with the input and
        // output chunks at different widths.
        let codes16: Vec<u16> = (0..150_u16).map(|i| i * 437).collect();
        let mut got16 = vec![0_u16; 300];
        HalfAndInverse.transform_u16(&codes16, &mut got16).unwrap();
        let codes8: Vec<u8> = (0..150_u8).collect();
        let mut got8 = vec![0_u8; 300];
        HalfAndInverse.transform_u8(&codes8, &mut got8).unwrap();
        for i in 0..150 {
            let v = f64::from(codes16[i]) / 65535.0;
            assert_eq!(got16[2 * i], encode_u16(v / 2.0 + 0.25), "pixel {i}");
            assert_eq!(got16[2 * i + 1], encode_u16(1.0 - v), "pixel {i}");
            let v = f64::from(codes8[i]) / 255.0;
            assert_eq!(got8[2 * i], encode_u8(v / 2.0 + 0.25), "pixel {i}");
            assert_eq!(got8[2 * i + 1], encode_u8(1.0 - v), "pixel {i}");
        }
    }

    #[test]
    fn provided_integer_entry_points_enforce_the_buffer_contract() {
        use crate::CmmError;
        let mut dst = [0_u16; 3];
        assert!(matches!(
            HalfAndInverse.transform_u16(&[1, 2], &mut dst),
            Err(CmmError::BufferLength {
                channels: 2,
                found: 3
            })
        ));
        let mut dst = [0_u8; 4];
        assert!(matches!(
            HalfAndInverse.transform_u8(&[1, 2, 3], &mut dst),
            Err(CmmError::BufferLength {
                channels: 2,
                found: 4
            })
        ));
        // Whole pixels are checked against the input channel count.
        assert!(matches!(
            super::check_buffers(3, 1, 4, 1),
            Err(CmmError::BufferLength {
                channels: 3,
                found: 4
            })
        ));
        super::check_buffers(3, 4, 6, 8).unwrap();
        // Empty buffers are zero pixels, not an error.
        HalfAndInverse.transform_u16(&[], &mut []).unwrap();
    }

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

    use super::{IccTransform, PipelineOptimization, Transform, TransformOptions, is_empty_layer};
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
        assert!(!options.prefer_cicp_transfer);
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
                optimization: PipelineOptimization::None,
                prefer_cicp_transfer: false,
            },
        )
        .unwrap();
        let relative = IccTransform::between(
            &src,
            &dst,
            TransformOptions {
                intent: RenderingIntent::MediaRelativeColorimetric,
                black_point_compensation: false,
                optimization: PipelineOptimization::None,
                prefer_cicp_transfer: false,
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
                optimization: PipelineOptimization::None,
                prefer_cicp_transfer: false,
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
                optimization: PipelineOptimization::None,
                prefer_cicp_transfer: false,
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
                optimization: PipelineOptimization::None,
                prefer_cicp_transfer: false,
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
                optimization: PipelineOptimization::None,
                prefer_cicp_transfer: false,
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
            optimization: PipelineOptimization::None,
            prefer_cicp_transfer: false,
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
            optimization: PipelineOptimization::None,
            prefer_cicp_transfer: false,
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
                    optimization: PipelineOptimization::None,
                    prefer_cicp_transfer: false,
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
                    optimization: PipelineOptimization::None,
                    prefer_cicp_transfer: false,
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
                optimization: PipelineOptimization::None,
                prefer_cicp_transfer: false,
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
                optimization: PipelineOptimization::None,
                prefer_cicp_transfer: false,
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
                optimization: PipelineOptimization::None,
                prefer_cicp_transfer: false,
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
                optimization: PipelineOptimization::None,
                prefer_cicp_transfer: false,
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
                optimization: PipelineOptimization::None,
                prefer_cicp_transfer: false,
            },
        )
        .unwrap();
        let off = IccTransform::between(
            &src,
            &dst,
            TransformOptions {
                intent: RenderingIntent::MediaRelativeColorimetric,
                black_point_compensation: false,
                optimization: PipelineOptimization::None,
                prefer_cicp_transfer: false,
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
                optimization: PipelineOptimization::None,
                prefer_cicp_transfer: false,
            },
        )
        .unwrap();
        let off = IccTransform::between(
            &src,
            &dst,
            TransformOptions {
                intent: RenderingIntent::MediaRelativeColorimetric,
                black_point_compensation: false,
                optimization: PipelineOptimization::None,
                prefer_cicp_transfer: false,
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
                "cmm: unsupported profile (device-link profiles run via IccTransform::device_link or IccTransform::chain)",
            ),
            (
                DeviceClass::Abstract,
                "cmm: unsupported profile (abstract profiles chain via IccTransform::chain)",
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
                optimization: PipelineOptimization::None,
                prefer_cicp_transfer: false,
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

    /// Where an sRGB → BT.2100 PQ transform (built-ins, both carrying `cicp` tags) sends
    /// device black, by the option. Media-relative, so no BPC moves black.
    fn srgb_black_into_pq(prefer_cicp_transfer: bool) -> [f64; 3] {
        let src = IccProfile::builtin(gamut_icc::BuiltinProfile::Srgb).unwrap();
        let dst = IccProfile::builtin(gamut_icc::BuiltinProfile::Bt2020Pq).unwrap();
        let transform = IccTransform::between(
            &src,
            &dst,
            TransformOptions {
                intent: RenderingIntent::MediaRelativeColorimetric,
                prefer_cicp_transfer,
                ..TransformOptions::default()
            },
        )
        .unwrap();
        eval(&transform, [0.0; 3])
    }

    #[test]
    fn between_honours_prefer_cicp_transfer() {
        // Off, the PQ TRC's sampled reversal lifts black to ≈ 0.0547 of signal; on, ST 2084's
        // closed-form inverse puts it at c1^m2.
        let closed = gamut_color::transfer::pq_oetf_normalized(0.0);
        assert_eq!(srgb_black_into_pq(true), [closed; 3]);
        for channel in srgb_black_into_pq(false) {
            assert!((channel - 0.0547).abs() < 1e-4, "{channel}");
        }
    }
}
