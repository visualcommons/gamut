//! Colour transforms (`cmsHTRANSFORM`): pixel-format codes, transform flags, the RAII
//! [`Transform`] over `cmsCreateTransform`/`cmsCreateMultiprofileTransform`/
//! `cmsCreateProofingTransform`, black-point detection, and the global error/alarm hooks.
//!
//! bindgen does not expand lcms2's `TYPE_*`/`cmsFLAGS_*` function-like macros, so the format
//! packing and the flag words are transcribed here from `include/lcms2.h` (lcms2 2.19), with a
//! unit test pinning the composed values against the header's expansions.

use std::ptr;

pub use sys::{
    INTENT_ABSOLUTE_COLORIMETRIC, INTENT_PERCEPTUAL, INTENT_RELATIVE_COLORIMETRIC,
    INTENT_SATURATION,
};

use crate::{Profile, sys};

/// Composes an lcms2 pixel-format code, replicating the `lcms2.h` shift macros
/// (`FLOAT_SH(a)<<22 | COLORSPACE_SH(s)<<16 | CHANNELS_SH(c)<<3 | BYTES_SH(b)`;
/// `lcms2.h:711-736`). `float_flag` is 1 for float/double layouts; `bytes` is the size of one
/// channel sample, with **0 meaning 8 bytes** (`double`) in lcms2's 3-bit field.
const fn format(float_flag: u32, colorspace: u32, channels: u32, bytes: u32) -> u32 {
    (float_flag << 22) | (colorspace << 16) | (channels << 3) | bytes
}

/// `PT_GRAY` colorspace code (`lcms2.h:744`).
const PT_GRAY: u32 = 3;
/// `PT_RGB` colorspace code (`lcms2.h:745`).
const PT_RGB: u32 = 4;
/// `PT_CMYK` colorspace code (`lcms2.h:747`).
const PT_CMYK: u32 = 6;
/// `PT_XYZ` colorspace code (`lcms2.h:750`).
const PT_XYZ: u32 = 9;
/// `PT_Lab` colorspace code (`lcms2.h:751`).
const PT_LAB: u32 = 10;
/// `PT_LabV2` colorspace code — identical to `PT_Lab` but using the legacy v2 encoding
/// (`lcms2.h:769`).
const PT_LAB_V2: u32 = 30;

/// `TYPE_GRAY_8` (`lcms2.h:776`).
pub const TYPE_GRAY_8: u32 = format(0, PT_GRAY, 1, 1);
/// `TYPE_GRAY_16` (`lcms2.h:778`).
pub const TYPE_GRAY_16: u32 = format(0, PT_GRAY, 1, 2);
/// `TYPE_GRAY_DBL` (`lcms2.h:971`).
pub const TYPE_GRAY_DBL: u32 = format(1, PT_GRAY, 1, 0);
/// `TYPE_RGB_8` (`lcms2.h:789`).
pub const TYPE_RGB_8: u32 = format(0, PT_RGB, 3, 1);
/// `TYPE_RGB_16` (`lcms2.h:793`).
pub const TYPE_RGB_16: u32 = format(0, PT_RGB, 3, 2);
/// `TYPE_RGB_DBL` (`lcms2.h:972`).
pub const TYPE_RGB_DBL: u32 = format(1, PT_RGB, 3, 0);
/// `TYPE_RGB_FLT` (`lcms2.h:953`).
pub const TYPE_RGB_FLT: u32 = format(1, PT_RGB, 3, 4);
/// `TYPE_CMYK_8` (`lcms2.h:835`).
pub const TYPE_CMYK_8: u32 = format(0, PT_CMYK, 4, 1);
/// `TYPE_CMYK_16` (`lcms2.h:840`).
pub const TYPE_CMYK_16: u32 = format(0, PT_CMYK, 4, 2);
/// `TYPE_CMYK_DBL` (`lcms2.h:974`). Double CMYK is in **ink percentages 0..100**, not 0..1.
pub const TYPE_CMYK_DBL: u32 = format(1, PT_CMYK, 4, 0);
/// `TYPE_CMYK_FLT` (`lcms2.h:965`). Float CMYK is in **ink percentages 0..100**, not 0..1.
pub const TYPE_CMYK_FLT: u32 = format(1, PT_CMYK, 4, 4);
/// `TYPE_XYZ_16` (`lcms2.h:905`).
pub const TYPE_XYZ_16: u32 = format(0, PT_XYZ, 3, 2);
/// `TYPE_XYZ_DBL` (`lcms2.h:969`). Double XYZ is unnormalized tristimulus (D50 Y = 1).
pub const TYPE_XYZ_DBL: u32 = format(1, PT_XYZ, 3, 0);
/// `TYPE_Lab_16` (`lcms2.h:911`) — the ICC v4 16-bit PCSLAB encoding.
pub const TYPE_Lab_16: u32 = format(0, PT_LAB, 3, 2);
/// `TYPE_Lab_DBL` (`lcms2.h:970`) — Lab as plain doubles (L 0..100, a/b −128..127).
pub const TYPE_Lab_DBL: u32 = format(1, PT_LAB, 3, 0);
/// `TYPE_LabV2_16` (`lcms2.h:912`) — the legacy ICC v2 16-bit PCSLAB encoding.
pub const TYPE_LabV2_16: u32 = format(0, PT_LAB_V2, 3, 2);

/// `cmsFLAGS_NOCACHE` — inhibit the 1-pixel transform cache (`lcms2.h:1745`).
pub const FLAGS_NOCACHE: u32 = 0x0040;
/// `cmsFLAGS_NOOPTIMIZE` — inhibit pipeline optimization, keep full precision (`lcms2.h:1746`).
pub const FLAGS_NOOPTIMIZE: u32 = 0x0100;
/// `cmsFLAGS_HIGHRESPRECALC` — use more CLUT points for precalculated transforms
/// (`lcms2.h:1756`).
pub const FLAGS_HIGHRESPRECALC: u32 = 0x0400;
/// `cmsFLAGS_LOWRESPRECALC` — use fewer CLUT points for precalculated transforms
/// (`lcms2.h:1757`).
pub const FLAGS_LOWRESPRECALC: u32 = 0x0800;
/// `cmsFLAGS_GAMUTCHECK` — out-of-gamut pixels are flagged with the alarm codes
/// (`lcms2.h:1750`).
pub const FLAGS_GAMUTCHECK: u32 = 0x1000;
/// `cmsFLAGS_BLACKPOINTCOMPENSATION` (`lcms2.h:1754`).
pub const FLAGS_BLACKPOINTCOMPENSATION: u32 = 0x2000;
/// `cmsFLAGS_SOFTPROOFING` — simulate the proofing device (`lcms2.h:1751`).
pub const FLAGS_SOFTPROOFING: u32 = 0x4000;
/// `cmsFLAGS_NOWHITEONWHITEFIXUP` — don't force the input white point onto the output white
/// ("scum dot" fixup) in precalculated (default-flag, 16-bit) transforms (`lcms2.h:1755`).
pub const FLAGS_NOWHITEONWHITEFIXUP: u32 = 0x0004;

/// An owned lcms2 colour transform (`cmsHTRANSFORM`); deleted on drop.
///
/// The pixel formats passed at construction are the caller's contract with the apply methods:
/// each `apply_*` reinterprets the input slice per `in_format` and sizes its output per the
/// `out_channels` argument, neither of which lcms2 can verify.
pub struct Transform {
    raw: sys::cmsHTRANSFORM,
}

impl Drop for Transform {
    fn drop(&mut self) {
        if !self.raw.is_null() {
            // SAFETY: `raw` is a live transform from an lcms2 constructor, deleted exactly once.
            unsafe { sys::cmsDeleteTransform(self.raw) };
        }
    }
}

fn wrap_transform(raw: sys::cmsHTRANSFORM) -> Transform {
    assert!(!raw.is_null(), "lcms2 returned a null transform handle");
    Transform { raw }
}

impl Transform {
    /// A two-profile transform (`cmsCreateTransform`) from `src` in `in_format` to `dst` in
    /// `out_format`, at rendering `intent` (an `INTENT_*` value) with `flags` (`FLAGS_*` bits).
    #[must_use]
    pub fn new(
        src: &Profile,
        in_format: u32,
        dst: &Profile,
        out_format: u32,
        intent: u32,
        flags: u32,
    ) -> Transform {
        // SAFETY: both handles are live for the call; lcms copies what it needs from them.
        wrap_transform(unsafe {
            sys::cmsCreateTransform(src.raw, in_format, dst.raw, out_format, intent, flags)
        })
    }

    /// A single-profile **devicelink** transform (`cmsCreateTransform` with a NULL output
    /// profile — lcms2's documented devicelink spelling): applies `link`'s A2B pipeline from
    /// its device space (`in_format`) to its "PCS" field, which for a link-class profile holds
    /// the *output* device space (`out_format`).
    ///
    /// Caution: the `TYPE_*_DBL`/`_FLT` CMYK formats carry **ink percentages 0..100**, not
    /// 0..1 (see [`TYPE_CMYK_DBL`]).
    #[must_use]
    pub fn devicelink(
        link: &Profile,
        in_format: u32,
        out_format: u32,
        intent: u32,
        flags: u32,
    ) -> Transform {
        // SAFETY: `link` is a live handle; a NULL output profile selects the one-profile
        // (devicelink) path in `cmsCreateTransformTHR`.
        wrap_transform(unsafe {
            sys::cmsCreateTransform(
                link.raw,
                in_format,
                ptr::null_mut(),
                out_format,
                intent,
                flags,
            )
        })
    }

    /// A chained transform over two or more profiles (`cmsCreateMultiprofileTransform`), applying
    /// `intent` at every hop.
    #[must_use]
    pub fn multiprofile(
        profiles: &[&Profile],
        in_format: u32,
        out_format: u32,
        intent: u32,
        flags: u32,
    ) -> Transform {
        assert!(profiles.len() >= 2, "a transform chain needs ≥ 2 profiles");
        let mut handles: Vec<sys::cmsHPROFILE> = profiles.iter().map(|p| p.raw).collect();
        let n = u32::try_from(handles.len()).expect("profile count fits u32");
        // SAFETY: `handles` holds `n` live profile handles; lcms reads the array during the call
        // only and does not take ownership of the profiles.
        wrap_transform(unsafe {
            sys::cmsCreateMultiprofileTransform(
                handles.as_mut_ptr(),
                n,
                in_format,
                out_format,
                intent,
                flags,
            )
        })
    }

    /// A proofing transform (`cmsCreateProofingTransform`): `src` → `dst` while simulating
    /// `proof` at `proofing_intent`. Pass [`FLAGS_SOFTPROOFING`] and/or [`FLAGS_GAMUTCHECK`] in
    /// `flags`, otherwise lcms degenerates this to a plain two-profile transform.
    #[expect(clippy::too_many_arguments, reason = "mirrors the lcms2 constructor")]
    #[must_use]
    pub fn proofing(
        src: &Profile,
        in_format: u32,
        dst: &Profile,
        out_format: u32,
        proof: &Profile,
        intent: u32,
        proofing_intent: u32,
        flags: u32,
    ) -> Transform {
        // SAFETY: all three handles are live for the call.
        wrap_transform(unsafe {
            sys::cmsCreateProofingTransform(
                src.raw,
                in_format,
                dst.raw,
                out_format,
                proof.raw,
                intent,
                proofing_intent,
                flags,
            )
        })
    }

    /// Transform `pixels` pixels of `f64` samples (`cmsDoTransform`), returning
    /// `pixels · out_channels` output samples.
    ///
    /// The caller contracts that `src` is laid out per the transform's input format (so
    /// `src.len() / pixels` is that format's channel count — divisibility is asserted, the
    /// channel count itself cannot be) and that `out_channels` matches the output format.
    #[must_use]
    pub fn apply_f64(&self, src: &[f64], pixels: usize, out_channels: usize) -> Vec<f64> {
        self.apply(src, pixels, out_channels)
    }

    /// [`Transform::apply_f64`] for 16-bit samples.
    #[must_use]
    pub fn apply_u16(&self, src: &[u16], pixels: usize, out_channels: usize) -> Vec<u16> {
        self.apply(src, pixels, out_channels)
    }

    /// [`Transform::apply_f64`] for 8-bit samples.
    #[must_use]
    pub fn apply_u8(&self, src: &[u8], pixels: usize, out_channels: usize) -> Vec<u8> {
        self.apply(src, pixels, out_channels)
    }

    /// Shared `cmsDoTransform` body over any sample scalar (zero is a valid fill for all three).
    fn apply<T: Copy + Default>(&self, src: &[T], pixels: usize, out_channels: usize) -> Vec<T> {
        assert!(
            pixels > 0 && src.len().is_multiple_of(pixels),
            "input length {} is not a whole number of {pixels} pixels",
            src.len()
        );
        let mut out = vec![T::default(); pixels * out_channels];
        let n = u32::try_from(pixels).expect("pixel count fits u32");
        // SAFETY: `src` holds `pixels` pixels in the transform's input format and `out` has room
        // for `pixels` pixels of `out_channels` samples in its output format — the documented
        // caller contract; lcms reads/writes exactly those ranges.
        unsafe {
            sys::cmsDoTransform(self.raw, src.as_ptr().cast(), out.as_mut_ptr().cast(), n);
        }
        out
    }
}

/// A bare lcms2 pipeline holding a single **float** CLUT stage
/// (`cmsStageAllocCLutFloatGranular`), evaluated through `cmsPipelineEvalFloat` — the most
/// direct window onto lcms2's float interpolators (`LinLerp1Dfloat`/`Eval1InputFloat`,
/// `BilinearInterpFloat`, `TetrahedralInterpFloat`, `Eval4InputsFloat`…`Eval15InputsFloat`)
/// with no profile, transform, formatter, or optimization machinery in between. This is the
/// tight CLUT-interpolation oracle; a CLUT embedded in a profile is evaluated through lcms2's
/// 16-bit fixed-point interpolators instead (`EvaluateCLUTfloatIn16` quantizes even float
/// transforms), so profile-borne comparisons are only 16-bit-tight.
pub struct ClutPipeline {
    raw: *mut sys::cmsPipeline,
    in_ch: usize,
    out_ch: usize,
}

impl Drop for ClutPipeline {
    fn drop(&mut self) {
        // SAFETY: `raw` is a live pipeline from `cmsPipelineAlloc`, freed exactly once.
        unsafe { sys::cmsPipelineFree(self.raw) };
    }
}

impl ClutPipeline {
    /// Builds the pipeline over a float CLUT with per-axis `grid_points`, `samples` in grid
    /// order (last input axis fastest, output channels interleaved per node; values normalized
    /// to `[0, 1]`), and `out_ch` outputs. lcms2 copies the table. The input channel count is
    /// `grid_points.len()` (at most 15, lcms2's `MAX_INPUT_DIMENSIONS`).
    #[must_use]
    pub fn new(grid_points: &[u8], samples: &[f32], out_ch: u32) -> ClutPipeline {
        let in_ch = u32::try_from(grid_points.len()).expect("axis count fits u32");
        let nodes: usize = grid_points.iter().map(|&n| usize::from(n)).product();
        assert_eq!(
            samples.len(),
            nodes * out_ch as usize,
            "sample count must be prod(grid) x out_ch"
        );
        let points: Vec<sys::cmsUInt32Number> = grid_points.iter().map(|&n| u32::from(n)).collect();
        // SAFETY: the points/samples pointers are live for the call and copied by lcms2; the
        // stage moves into the pipeline on insert.
        unsafe {
            let raw = sys::cmsPipelineAlloc(ptr::null_mut(), in_ch, out_ch);
            assert!(!raw.is_null(), "cmsPipelineAlloc failed");
            let stage = sys::cmsStageAllocCLutFloatGranular(
                ptr::null_mut(),
                points.as_ptr(),
                in_ch,
                out_ch,
                samples.as_ptr(),
            );
            assert!(!stage.is_null(), "cmsStageAllocCLutFloatGranular failed");
            assert!(sys::cmsPipelineInsertStage(raw, sys::cmsAT_END, stage) != 0);
            ClutPipeline {
                raw,
                in_ch: in_ch as usize,
                out_ch: out_ch as usize,
            }
        }
    }

    /// Evaluates one pixel (`cmsPipelineEvalFloat`): `input` holds one sample per grid axis.
    #[must_use]
    pub fn eval(&self, input: &[f32]) -> Vec<f32> {
        assert_eq!(input.len(), self.in_ch, "one sample per input channel");
        let mut out = vec![0.0_f32; self.out_ch];
        // SAFETY: `input` and `out` hold the pipeline's declared channel counts; lcms2
        // reads/writes exactly those ranges.
        unsafe { sys::cmsPipelineEvalFloat(input.as_ptr(), out.as_mut_ptr(), self.raw) };
        out
    }
}

/// The source black point lcms2 detects for `profile` at `intent`
/// (`cmsDetectBlackPoint`) as XYZ, or `None` when lcms returns FALSE (link/abstract/named
/// classes, or absolute intent).
#[must_use]
pub fn detect_black_point(profile: &Profile, intent: u32) -> Option<[f64; 3]> {
    let mut xyz = sys::cmsCIEXYZ {
        X: 0.0,
        Y: 0.0,
        Z: 0.0,
    };
    // SAFETY: `raw` is live; lcms writes the out-param before returning.
    let ok = unsafe { sys::cmsDetectBlackPoint(&mut xyz, profile.raw, intent, 0) };
    (ok != 0).then_some([xyz.X, xyz.Y, xyz.Z])
}

/// The destination black point lcms2 detects for `profile` at `intent`
/// (`cmsDetectDestinationBlackPoint` — the round-trip-ramp estimator for CLUT output profiles),
/// or `None` when lcms returns FALSE.
#[must_use]
pub fn detect_destination_black_point(profile: &Profile, intent: u32) -> Option<[f64; 3]> {
    let mut xyz = sys::cmsCIEXYZ {
        X: 0.0,
        Y: 0.0,
        Z: 0.0,
    };
    // SAFETY: `raw` is live; lcms writes the out-param before returning.
    let ok = unsafe { sys::cmsDetectDestinationBlackPoint(&mut xyz, profile.raw, intent, 0) };
    (ok != 0).then_some([xyz.X, xyz.Y, xyz.Z])
}

/// The no-op handler installed by [`set_quiet_log_handler`].
unsafe extern "C" fn quiet_log_handler(
    _context: sys::cmsContext,
    _error_code: sys::cmsUInt32Number,
    _text: *const std::os::raw::c_char,
) {
}

/// Install a no-op `cmsSetLogErrorHandler`, silencing lcms2's default stderr chatter for
/// deliberately-exercised error paths. Tests call this once (idempotent, process-global) at the
/// top of each test that can trip an lcms2 diagnostic.
pub fn set_quiet_log_handler() {
    // SAFETY: installing a global handler is unconditionally valid; the handler itself does
    // nothing with its raw arguments.
    unsafe { sys::cmsSetLogErrorHandler(Some(quiet_log_handler)) };
}

/// Set the first three global out-of-gamut alarm codes (`cmsSetAlarmCodes`). lcms reads a
/// 16-entry (`cmsMAXCHANNELS`) array; the remaining thirteen are set to zero.
pub fn set_alarm_codes(codes: [u16; 3]) {
    let mut all = [0u16; 16];
    all[..3].copy_from_slice(&codes);
    // SAFETY: `all` holds the 16 entries (`cmsMAXCHANNELS`) lcms copies out of the pointer.
    unsafe { sys::cmsSetAlarmCodes(all.as_ptr()) };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::cie2000_delta_e;
    use crate::synth::{display_p3_srgb_trc, srgb};

    /// Pins the transcribed `format` packing to the dossier-verified expansions of the lcms2
    /// header macros — the whole point of transcribing rather than binding them.
    #[test]
    fn type_codes_match_lcms2_header_expansions() {
        assert_eq!(TYPE_RGB_DBL, 0x0044_0018);
        assert_eq!(TYPE_Lab_DBL, 0x004A_0018);
        assert_eq!(TYPE_CMYK_FLT, 0x0046_0024);
        assert_eq!(TYPE_GRAY_8, 0x0003_0009);
        assert_eq!(TYPE_Lab_16, 0x000A_001A);
        assert_eq!(TYPE_XYZ_DBL, 0x0049_0018);
        // The remaining composed values, from the same lcms2.h expansion rule.
        assert_eq!(TYPE_GRAY_16, 0x0003_000A);
        assert_eq!(TYPE_GRAY_DBL, 0x0043_0008);
        assert_eq!(TYPE_RGB_8, 0x0004_0019);
        assert_eq!(TYPE_RGB_16, 0x0004_001A);
        assert_eq!(TYPE_RGB_FLT, 0x0044_001C);
        assert_eq!(TYPE_CMYK_8, 0x0006_0021);
        assert_eq!(TYPE_CMYK_16, 0x0006_0022);
        assert_eq!(TYPE_CMYK_DBL, 0x0046_0020);
        assert_eq!(TYPE_XYZ_16, 0x0009_001A);
        assert_eq!(TYPE_LabV2_16, 0x001E_001A);
    }

    /// sRGB → Display P3 → sRGB round trip in double precision: P3 contains the sRGB gamut and
    /// both are analytic matrix/TRC profiles, so the round trip must land within a small ΔE₀₀.
    #[test]
    fn srgb_display_p3_round_trip_is_tight() {
        set_quiet_log_handler();
        let srgb = srgb();
        let p3 = display_p3_srgb_trc();
        let lab = crate::synth::lab4();
        let fwd = Transform::new(
            &srgb,
            TYPE_RGB_DBL,
            &p3,
            TYPE_RGB_DBL,
            INTENT_RELATIVE_COLORIMETRIC,
            FLAGS_NOCACHE,
        );
        let back = Transform::new(
            &p3,
            TYPE_RGB_DBL,
            &srgb,
            TYPE_RGB_DBL,
            INTENT_RELATIVE_COLORIMETRIC,
            FLAGS_NOCACHE,
        );
        let to_lab = Transform::new(
            &srgb,
            TYPE_RGB_DBL,
            &lab,
            TYPE_Lab_DBL,
            INTENT_RELATIVE_COLORIMETRIC,
            FLAGS_NOCACHE,
        );
        let colours: [[f64; 3]; 6] = [
            [1.0, 0.0, 0.0],
            [0.0, 1.0, 0.0],
            [0.0, 0.0, 1.0],
            [0.2, 0.5, 0.8],
            [0.95, 0.9, 0.1],
            [0.5, 0.5, 0.5],
        ];
        for rgb in colours {
            let p3_rgb = fwd.apply_f64(&rgb, 1, 3);
            let rt = back.apply_f64(&p3_rgb, 1, 3);
            let lab_in = to_lab.apply_f64(&rgb, 1, 3);
            let lab_rt = to_lab.apply_f64(&rt, 1, 3);
            let de = cie2000_delta_e(
                [lab_in[0], lab_in[1], lab_in[2]],
                [lab_rt[0], lab_rt[1], lab_rt[2]],
                1.0,
                1.0,
                1.0,
            );
            assert!(de < 0.5, "ΔE00 {de} for {rgb:?} → {rt:?}");
        }
    }

    /// Black-point detection: a matrix-shaper display profile yields a black point for the
    /// colorimetric/perceptual intents but `None` for absolute (lcms returns FALSE there by
    /// contract).
    #[test]
    fn black_point_detection_signatures() {
        set_quiet_log_handler();
        let srgb = srgb();
        let bp = detect_black_point(&srgb, INTENT_RELATIVE_COLORIMETRIC)
            .expect("sRGB has a detectable black point");
        assert!(bp[1] >= 0.0 && bp[1] < 0.1, "black Y = {}", bp[1]);
        assert!(detect_black_point(&srgb, INTENT_ABSOLUTE_COLORIMETRIC).is_none());
        let dst = detect_destination_black_point(&srgb, INTENT_PERCEPTUAL)
            .expect("destination black point for perceptual");
        assert!(dst[1] >= 0.0 && dst[1] < 0.1, "dest black Y = {}", dst[1]);
    }

    /// `set_alarm_codes` reaches lcms2's global state: read back via `cmsGetAlarmCodes`.
    #[test]
    fn alarm_codes_round_trip() {
        set_alarm_codes([0x1234, 0x5678, 0x9ABC]);
        let mut got = [0u16; 16];
        // SAFETY: lcms writes exactly cmsMAXCHANNELS (16) entries.
        unsafe { sys::cmsGetAlarmCodes(got.as_mut_ptr()) };
        assert_eq!(&got[..3], &[0x1234, 0x5678, 0x9ABC]);
        // Restore the lcms2 default (0x7F00 in every channel) for other tests.
        set_alarm_codes([0x7F00, 0x7F00, 0x7F00]);
    }
}
