//! fuzz · robustness — `gamut_cmm`'s transform constructors over a profile an image embedded.
//!
//! `docs/testing.md`'s per-crate table names the profile-linking constructors as this crate's
//! untrusted-input surface (#311). `gamut-icc`'s `IccProfile::parse` is the byte layer, and
//! `icc_parse` searches it; but a profile that *parses* still carries every curve, matrix, CLUT and
//! `chad`/white-point value its writer chose, and `gamut-cmm` turns those into a pipeline and
//! evaluates it — `gamut-jpeg`'s `decode_srgb` does exactly that with the profile an untrusted JPEG
//! carries. So this target parses the engine's bytes as a profile and, where that succeeds, drives
//! the linking, optimization and evaluation layers the parser never reaches:
//!
//! 1. [`IccTransform::embedded_to_srgb`] — the constructor a decoder calls (media-relative, no
//!    optimization, the profile in the **device → PCS** direction);
//! 2. [`IccTransform::between`] from the built-in sRGB to the profile — the **PCS → device**
//!    direction (curve inversion, `B2Ax` selection) at the profile's own header intent, with
//!    black-point compensation requested (the `bpc` estimators run) and
//!    [`PipelineOptimization::PrecalculateFixed16`] (CLUT resampling and the 8-bit matrix-shaper);
//! 3. [`IccTransform::device_link`] — a `link`-class profile's one constructor, which the two
//!    above refuse.
//!
//! Every transform that builds is then run over a fixed 3-level grid of device values in each
//! sample width the pixel-buffer entry points use (`f64`, `u16`, `u8`), so evaluation of a hostile
//! table — a non-monotonic curve inverted, a CLUT interpolated at its edges — is searched too.
//!
//! The crate is `#![deny(unsafe_code)]` and holds no exception, so a hostile profile must end in a
//! typed error, never a panic, a hang, or an allocation past libFuzzer's malloc limit. Like
//! `icc_parse`, this target lists **no check beyond the crash oracle**; a crash is minimised and
//! promoted into a named deterministic case in `gamut-cmm`'s own suite.
//!
//! Its seed is `icc_parse`'s, the vendored JPEG XL XYB profile: an `A2B0`-only LUT profile, so the
//! first constructor reaches a CLUT on the first execution and the second is refused until the
//! engine grows a `B2A0`.

#![no_main]

use gamut_cmm::{IccTransform, PipelineOptimization, Transform, TransformOptions};
use gamut_icc::{BuiltinProfile, DeviceClass, IccProfile};
use libfuzzer_sys::fuzz_target;

/// Device values every channel of the evaluation grid takes: both ends of the encoded range and its
/// midpoint, where a CLUT's interpolation cell boundaries and a curve's ends are.
const LEVELS: [f64; 3] = [0.0, 0.5, 1.0];

fuzz_target!(|data: &[u8]| {
    let Ok(profile) = IccProfile::parse(data) else {
        return;
    };

    if let Ok(transform) = IccTransform::embedded_to_srgb(&profile) {
        evaluate(&transform);
    }

    if let Some(srgb) = IccProfile::builtin(BuiltinProfile::Srgb) {
        let options = TransformOptions::default()
            .with_intent(profile.header.rendering_intent)
            .with_black_point_compensation(true)
            .with_optimization(PipelineOptimization::PrecalculateFixed16);
        if let Ok(transform) = IccTransform::between(&srgb, &profile, options) {
            evaluate(&transform);
        }
    }

    if profile.header.device_class == DeviceClass::DeviceLink
        && let Ok(transform) = IccTransform::device_link(&profile, TransformOptions::default())
    {
        evaluate(&transform);
    }
});

/// How many grid pixels each evaluation runs: every `LEVELS` combination of four channels, which
/// covers RGB, CMYK and grey whole. A profile with more channels sees its fifth onwards repeat the
/// first four's digits, so the buffers stay a fixed size however many channels the profile declares
/// (up to `gamut_cmm::MAX_CHANNELS`, 16, where a full `3^16` grid would be 43 million pixels).
const PIXELS: usize = 81;

/// Runs `transform` over the `LEVELS` grid in its input space, in `f64`, `u16` and `u8`.
fn evaluate(transform: &IccTransform) {
    let channels = usize::from(transform.input_channels());
    let out_channels = usize::from(transform.output_channels());

    let mut src = Vec::with_capacity(PIXELS * channels);
    for pixel in 0..PIXELS {
        for channel in 0..channels {
            let digit = pixel / LEVELS.len().pow((channel % 4) as u32) % LEVELS.len();
            src.push(LEVELS[digit]);
        }
    }

    let mut dst = vec![0.0; PIXELS * out_channels];
    let _ = transform.transform(&src, &mut dst);

    let src16: Vec<u16> = src.iter().map(|&v| (v * 65535.0) as u16).collect();
    let mut dst16 = vec![0; PIXELS * out_channels];
    let _ = transform.transform_u16(&src16, &mut dst16);

    let src8: Vec<u8> = src.iter().map(|&v| (v * 255.0) as u8).collect();
    let mut dst8 = vec![0; PIXELS * out_channels];
    let _ = transform.transform_u8(&src8, &mut dst8);
}
