//! `CurveToneMapper` against ffmpeg `tonemap`'s published per-pixel function.
//!
//! The oracle is `ffmpeg_tonemap` below: an `f64` transcription of `tonemap()` in ffmpeg's
//! `libavfilter/vf_tonemap.c` (commit `b8c699bfbebd889d7795fb07bd7679b84ab366fb`, transcribed in
//! `references/tonemap/README.md`), written from that source and not from this crate — desaturate
//! toward luma by `max(Y − desat, 1e-6) / max(Y, 1e-6)`, take `max(max(r, g, b), 1e-6)` as the
//! signal, map it through the selected curve, and scale every channel by `mapped / signal`. ffmpeg
//! has only the max-RGB mode, so that is the mode under test; target peak 1 is ffmpeg's implicit
//! SDR output.
//!
//! The literal goldens were computed from the same transcription in Python, independently of both
//! this file and the crate.
//!
//! This file is an integration test because the claim is about the public surface only.

use gamut_core::luminance::{BT709_LUMA_WEIGHTS, LUMA_ONE};
use gamut_tonemap::{
    ApplyMode, CurveToneMapper, Exposure, Hable, Linear, Reinhard, ToneCurve, ToneMapper,
};

/// ffmpeg's `AVCOL_SPC_BT709` luma coefficients (`libavutil/csp.c`, `AVR(0.2126)` …).
const FFMPEG_BT709: [f64; 3] = [0.2126, 0.7152, 0.0722];

/// `gamut_core`'s BT.709 weights as the exact fractions the mapper uses.
fn core_bt709() -> [f64; 3] {
    BT709_LUMA_WEIGHTS.map(|w| f64::from(w) / f64::from(LUMA_ONE))
}

/// ffmpeg's curves, with `param` already in the form `init()` leaves it.
#[derive(Debug, Clone, Copy)]
enum FfmpegCurve {
    /// `sig = hable(sig) / hable(peak)`.
    Hable,
    /// `sig = sig / (sig + param) * (peak + param) / peak`.
    Reinhard { param: f64 },
    /// `sig = sig * param / peak`.
    Linear { param: f64 },
    /// `sig = av_clipf(sig * param, 0, 1)`.
    Clip { param: f64 },
}

/// ffmpeg's `hable()`.
fn hable(x: f64) -> f64 {
    let (a, b, c, d, e, f) = (0.15, 0.50, 0.10, 0.20, 0.02, 0.30);
    (x * (x * a + b * c) + d * e) / (x * (x * a + b) + d * f) - e / f
}

/// ffmpeg's `MIX(x, y, a)`.
fn mix(x: f64, y: f64, a: f64) -> f64 {
    x * (1.0 - a) + y * a
}

/// ffmpeg's `tonemap()` for one pixel, in `f64`, with the given luma coefficients.
fn ffmpeg_tonemap(
    rgb: [f64; 3],
    curve: FfmpegCurve,
    peak: f64,
    desat: f64,
    coeffs: [f64; 3],
) -> [f64; 3] {
    let [mut r, mut g, mut b] = rgb;
    if desat > 0.0 {
        let luma = coeffs[0] * r + coeffs[1] * g + coeffs[2] * b;
        let overbright = (luma - desat).max(1e-6) / luma.max(1e-6);
        r = mix(r, luma, overbright);
        g = mix(g, luma, overbright);
        b = mix(b, luma, overbright);
    }
    let sig_orig = r.max(g).max(b).max(1e-6);
    let sig = match curve {
        FfmpegCurve::Hable => hable(sig_orig) / hable(peak),
        FfmpegCurve::Reinhard { param } => sig_orig / (sig_orig + param) * (peak + param) / peak,
        FfmpegCurve::Linear { param } => sig_orig * param / peak,
        FfmpegCurve::Clip { param } => (sig_orig * param).clamp(0.0, 1.0),
    };
    [r, g, b].map(|c| c * sig / sig_orig)
}

/// The mapper that reproduces `curve` at `peak`: the scalar curve whose `f(x) / f(peak)`
/// normalization is ffmpeg's expression, in max-RGB mode.
fn mapper_for(curve: FfmpegCurve, peak: f32, desat: f32) -> Box<dyn ToneMapper> {
    fn build<C: ToneCurve + 'static>(
        curve: C,
        peak: f32,
        target: f32,
        desat: f32,
    ) -> Box<dyn ToneMapper> {
        Box::new(
            CurveToneMapper::new(curve, peak)
                .and_then(|m| m.with_target_peak(target))
                .and_then(|m| m.with_desat(desat))
                .expect("valid parameters")
                .with_mode(ApplyMode::MaxRgb),
        )
    }
    match curve {
        FfmpegCurve::Hable => build(Hable::default(), peak, 1.0, desat),
        // x / (x + p) is Reinhard of x / p.
        FfmpegCurve::Reinhard { param } => {
            let p = param as f32;
            build(move |x: f32| Reinhard.map(x / p), peak, 1.0, desat)
        }
        // sig · param / peak is the identity normalized onto target peak `param`.
        FfmpegCurve::Linear { param } => build(Linear, peak, param as f32, desat),
        // min(sig · param, 1): its value at the peak is 1 whenever peak · param ≥ 1, which is
        // where the normalization is the identity and the two agree.
        FfmpegCurve::Clip { param } => {
            let gain = Exposure::new(param as f32).expect("valid gain");
            build(move |x: f32| gain.map(x).min(1.0), peak, 1.0, desat)
        }
    }
}

/// Channel values from black through a near-black luma under ffmpeg's `1e-6` floor, SDR, and
/// highlights past every peak swept.
const CHANNELS: [f32; 11] = [0.0, 1e-7, 0.01, 0.18, 0.5, 1.0, 2.0, 4.5, 10.0, 50.0, 120.0];
const PEAKS: [f32; 4] = [1.0, 4.0, 10.0, 100.0];
const DESATS: [f32; 4] = [0.0, 0.5, 2.0, 6.0];

fn curves() -> [FfmpegCurve; 5] {
    [
        FfmpegCurve::Hable,
        // ffmpeg's default (`param` unset leaves 1.0) and `param=0.6` (`(1 − 0.6) / 0.6`).
        FfmpegCurve::Reinhard { param: 1.0 },
        FfmpegCurve::Reinhard {
            param: f64::from((1.0_f32 - 0.6) / 0.6),
        },
        FfmpegCurve::Linear { param: 0.8 },
        FfmpegCurve::Clip { param: 1.0 },
    ]
}

/// Sweeps every pixel of `CHANNELS³` at every peak, desat and curve, and returns the largest
/// error of the mapper against the oracle at `coeffs`, measured as `|got − want| / (|want| + 1)`:
/// relative for bright outputs, and absolute against the SDR output scale (target peak ≤ 1) where
/// the output is near zero — Hable's `f32` evaluation cancels there, as `operators.rs` records.
fn worst_error(coeffs: [f64; 3]) -> (f64, String) {
    let mut worst = (0.0_f64, String::new());
    for curve in curves() {
        for peak in PEAKS {
            for desat in DESATS {
                let mapper = mapper_for(curve, peak, desat);
                for r in CHANNELS {
                    for g in CHANNELS {
                        for b in CHANNELS {
                            let got = mapper.map_rgb([r, g, b]);
                            let want = ffmpeg_tonemap(
                                [r, g, b].map(f64::from),
                                curve,
                                f64::from(peak),
                                f64::from(desat),
                                coeffs,
                            );
                            for (got, want) in got.iter().zip(want) {
                                let error = (f64::from(*got) - want).abs() / (want.abs() + 1.0);
                                if error > worst.0 || error.is_nan() {
                                    worst = (
                                        error,
                                        format!(
                                            "{curve:?} peak {peak} desat {desat} rgb \
                                             {:?}: got {got:e}, want {want:e}",
                                            [r, g, b]
                                        ),
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    worst
}

/// With the oracle at the exact weights the mapper uses, the only difference left is `f32`
/// arithmetic: 32 ULPs of the output scale bounds it with room for the four or five roundings the
/// chain (luma, mix, curve, normalization, gain) compounds.
#[test]
fn max_rgb_mapper_matches_ffmpegs_function_at_the_same_luma_weights() {
    let (error, case) = worst_error(core_bt709());
    assert!(
        error <= 32.0 * f64::from(f32::EPSILON),
        "error {error:e} at {case}"
    );
}

/// The one intended difference from ffmpeg: luma weights. ffmpeg reads BT.709's as the decimals
/// `0.2126 / 0.7152 / 0.0722`; the mapper uses `gamut_core`'s fixed-point weights, each the decimal
/// rounded to the nearest `1 / 65536`. The sweep above therefore runs the oracle at the fixed-point
/// weights, and this pins that they are ffmpeg's to that rounding.
///
/// There is no fixed relative bound on what the rounding does to the output — near `Y == desat`
/// the overbright ratio `(Y − desat) / Y` amplifies a luma error without limit — so the literal
/// goldens below, at the decimal weights, sit away from that knee.
#[test]
fn luma_weights_are_ffmpegs_to_the_nearest_fixed_point_step() {
    for (core, ffmpeg) in core_bt709().iter().zip(FFMPEG_BT709) {
        assert!(
            (core - ffmpeg).abs() <= 0.5 / f64::from(LUMA_ONE),
            "{core} is not {ffmpeg} rounded to 1/65536"
        );
    }
}

/// Five pixels evaluated from ffmpeg's formula in Python (decimal BT.709 weights), printed to nine
/// significant digits.
#[test]
fn literal_goldens() {
    /// Curve, peak, desat, input pixel, expected output.
    type Golden = (FfmpegCurve, f32, f32, [f32; 3], [f64; 3]);
    let cases: [Golden; 5] = [
        (
            FfmpegCurve::Hable,
            10.0,
            2.0,
            [8.0, 2.0, 0.5],
            [0.871023224, 0.340374991, 0.207712933],
        ),
        (
            FfmpegCurve::Hable,
            10.0,
            0.0,
            [8.0, 2.0, 0.5],
            [0.942617862, 0.235654466, 0.0589136164],
        ),
        (
            FfmpegCurve::Reinhard { param: 1.0 },
            4.0,
            2.0,
            [3.0, 0.25, 0.0],
            [0.937499791, 0.0781252691, 3.12500209e-07],
        ),
        (
            FfmpegCurve::Reinhard { param: 1.0 },
            4.0,
            2.0,
            [0.5, 0.2, 0.1],
            [0.41666614, 0.166666956, 0.0833338946],
        ),
        (
            FfmpegCurve::Linear { param: 0.5 },
            40.0,
            2.0,
            [20.0, 5.0, 1.0],
            [0.137042037, 0.0895748842, 0.0769169768],
        ),
    ];
    for (curve, peak, desat, rgb, want) in cases {
        let got = mapper_for(curve, peak, desat).map_rgb(rgb);
        for (got, want) in got.iter().zip(want) {
            assert!(
                (f64::from(*got) - want).abs() <= 1e-4 * want.abs(),
                "{curve:?} peak {peak} desat {desat} rgb {rgb:?}: got {got:e}, want {want:e}"
            );
        }
    }
}
