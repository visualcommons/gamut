//! RGB-aware tone mapping: the [`ToneMapper`] trait and [`CurveToneMapper`], which applies a scalar
//! [`ToneCurve`] to a whole pixel.
//!
//! A [`ToneCurve`] maps one value. The operators HDR-to-SDR pipelines are compared against need the
//! pixel: ffmpeg's `tonemap` filter desaturates overbright colours and then drives its curve from
//! the brightest channel, and libplacebo drives its curves from the luminance or the max-RGB of the
//! pixel, scaling all three channels by one gain so hue survives. Both also place the curve between
//! an explicit source and target peak. [`CurveToneMapper`] is that shape, built on the frozen scalar
//! trait so every built-in curve (and every closure) is usable as-is; [`ToneMapper`] is the trait a
//! pixel-level operator implements, which is the extension point operators that are not a scalar
//! curve in a mode (BT.2390's EETF, #1254; BT.2446 Method A, #1255) will implement.
//!
//! The formula, with the ffmpeg source it is transcribed from, is recorded in
//! `references/tonemap/README.md` ("ffmpeg `tonemap`: desaturation and max-RGB application").

use gamut_core::luminance::{BT709_LUMA_WEIGHTS, LUMA_ONE};
use gamut_core::{Error, Result};

use crate::{SDR_WHITE_NORMALIZED, ToneCurve};

/// Default desaturation strength for [`CurveToneMapper`]: `2.0`, ffmpeg `tonemap`'s `desat`
/// default.
///
/// The threshold is in the signal's own units, so with linear light normalized to reference
/// white (`1.0` = SDR diffuse white, ffmpeg's 100 cd/m²) a pixel starts desaturating once its luma
/// passes twice reference white.
pub const DEFAULT_DESAT: f32 = 2.0;

/// The floor ffmpeg's `tonemap` applies to the luma and to the driving signal before dividing by
/// them (`FFMAX(·, 1e-6)`), so a black pixel maps to black instead of `0 / 0`.
const SIGNAL_FLOOR: f32 = 1e-6;

/// A tone-mapping operator over a whole linear-light RGB pixel.
///
/// Where [`ToneCurve`] sees one value, a `ToneMapper` sees the pixel, so it can drive a curve from
/// the pixel's luminance or brightest channel, keep hue by scaling every channel by one gain, and
/// desaturate colours a curve would otherwise push out of gamut. [`CurveToneMapper`] builds one from
/// any [`ToneCurve`]; implement the trait directly for an operator that is not a scalar curve
/// applied in a mode.
///
/// Implementors provide only [`map_rgb`](ToneMapper::map_rgb);
/// [`map_rgb_slice`](ToneMapper::map_rgb_slice) is derived from it.
///
/// # Contract
///
/// Channels are linear light, finite and non-negative, in whatever unit the operator's peaks are
/// stated in. For such a pixel, [`CurveToneMapper`] over any built-in curve returns non-negative
/// channels and **never NaN**; a channel may saturate to `+∞` only where the curve itself does.
/// Behaviour on negative or NaN channels is operator-defined — linearize and clamp upstream.
///
/// # Precision
///
/// `f32`, as for [`ToneCurve`] (#1102, #1236): a caller holding `f64` linear values narrows them at
/// this boundary.
pub trait ToneMapper {
    /// Map one linear-light `[r, g, b]` pixel to its tone-mapped value.
    #[must_use]
    fn map_rgb(&self, rgb: [f32; 3]) -> [f32; 3];

    /// Apply [`map_rgb`](ToneMapper::map_rgb) to every pixel of `buf` in place.
    ///
    /// Each pixel is mapped independently, so the result is unaffected by `buf`'s length or the
    /// order of its pixels. An interleaved `&mut [f32]` buffer is viewed this way with
    /// `as_chunks_mut::<3>()`.
    fn map_rgb_slice(&self, buf: &mut [[f32; 3]]) {
        for pixel in buf {
            *pixel = self.map_rgb(*pixel);
        }
    }
}

/// Which signal a [`CurveToneMapper`] feeds its curve, and how the result reaches the channels.
///
/// Discriminants are explicit and permanent — they are C ABI values (#1103); new variants append.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
#[repr(u32)]
pub enum ApplyMode {
    /// Map each channel through the curve independently.
    ///
    /// The cheapest mode, and the one a scalar curve gives on its own; it shifts hue and
    /// saturation, since each channel is compressed by a different amount.
    PerChannel = 0,
    /// Map the pixel's luma (the mapper's luma weights, BT.709 by default) and scale every channel
    /// by the same gain.
    ///
    /// Preserves hue and the luminance relationship, but a saturated colour's brightest channel can
    /// still land above the target peak.
    Luminance = 1,
    /// Map the pixel's brightest channel and scale every channel by the same gain — ffmpeg
    /// `tonemap`'s mode, and the default.
    ///
    /// Preserves hue and keeps every channel within the curve's range.
    #[default]
    MaxRgb = 2,
}

/// A [`ToneMapper`] that applies a scalar [`ToneCurve`] to a pixel, between explicit source and
/// target peaks, with ffmpeg `tonemap`'s desaturation.
///
/// For each pixel:
///
/// 1. **Desaturate.** With `desat > 0` and `Y` the pixel's luma, every channel is mixed toward `Y`
///    by `overbright = max(Y − desat, 1e-6) / max(Y, 1e-6)`. A pixel whose luma is below `desat`
///    barely moves; a pixel far above it goes almost grey. The mix preserves `Y`. `desat = 0` skips
///    the step.
/// 2. **Map.** The curve `f` is normalized so the source peak lands on the target peak:
///    `T(x) = target_peak · f(x) / f(source_peak)`. [`ApplyMode::PerChannel`] sets each channel to
///    `T(channel)`; [`ApplyMode::Luminance`] and [`ApplyMode::MaxRgb`] take the driving signal
///    `s = max(Y, 1e-6)` or `s = max(max(r, g, b), 1e-6)` and multiply every channel by
///    `T(s) / s`.
///
/// With [`ApplyMode::MaxRgb`] and `target_peak = 1` this is ffmpeg `tonemap`'s per-pixel function
/// exactly for its `hable`, `reinhard`, `linear` and (for `peak · param ≥ 1`) `clip` curves, which
/// all have the `f(x) / f(peak)` form; `tests/ffmpeg_desat_golden.rs` holds it to an `f64`
/// transcription of that function. ffmpeg's `1e-6` floors are kept as written, including the one
/// consequence worth knowing: a pixel whose luma is below `1e-6` desaturates fully to grey.
///
/// Peaks and `desat` are in the signal's own unit. Normalizing linear light to reference white
/// (`1.0` = SDR diffuse white) matches ffmpeg, whose `peak` is in units of 100 cd/m²; a 1000-nit
/// master into SDR is then `source_peak = 10`, `target_peak = 1`.
///
/// Luma uses `gamut_core`'s fixed-point weights ([`BT709_LUMA_WEIGHTS`] unless
/// [`with_luma_weights`](Self::with_luma_weights) says otherwise), each exactly `W / 65536`. The
/// BT.709 defaults round to nearest, each within `2⁻¹⁷` of its decimal coefficient, so `Y` is
/// within `2⁻¹⁷ · (r + g + b)` of the decimal-weight luma ffmpeg computes. `gamut_core`'s other
/// tables are within one fixed-point unit, `2⁻¹⁶`, per weight (luma within `2⁻¹⁶ · (r + g + b)`):
/// [`BT2020_LUMA_WEIGHTS`](gamut_core::luminance::BT2020_LUMA_WEIGHTS) gives green the spare unit
/// that makes the triple sum to [`LUMA_ONE`], leaving it `0.59` units off.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CurveToneMapper<C> {
    curve: C,
    mode: ApplyMode,
    desat: f32,
    source_peak: f32,
    target_peak: f32,
    luma_weights: [u32; 3],
    /// `luma_weights` as `f32` fractions of one; exact, since every weight is below `2²⁴`.
    weights: [f32; 3],
    /// `target_peak / f(source_peak)`, validated normal and positive at construction.
    scale: f32,
}

/// `value` if it is finite and strictly positive, else an invalid-input error naming `what`.
fn positive_finite(value: f32, what: &'static str) -> Result<f32> {
    if !value.is_finite() || value <= 0.0 {
        return Err(Error::invalid_input(env!("CARGO_PKG_NAME"), what));
    }
    Ok(value)
}

/// The fixed-point weights as fractions of one: `W / 65536`, exact in `f32`.
fn weights_as_f32(weights: [u32; 3]) -> [f32; 3] {
    weights.map(|w| w as f32 / LUMA_ONE as f32)
}

impl<C: ToneCurve> CurveToneMapper<C> {
    /// Create a mapper that applies `curve` with `source_peak` mapping to [`SDR_WHITE_NORMALIZED`],
    /// in [`ApplyMode::MaxRgb`], with [`DEFAULT_DESAT`] and BT.709 luma weights — ffmpeg
    /// `tonemap`'s defaults.
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if
    /// `source_peak` is not finite and strictly positive, or if `curve` does not map it to a value
    /// that leaves `target_peak / curve.map(source_peak)` a normal, positive `f32` (a curve that
    /// sends the peak to zero, a negative value, or `+∞` has no normalization).
    pub fn new(curve: C, source_peak: f32) -> Result<Self> {
        let source_peak = positive_finite(
            source_peak,
            "tone mapper source peak must be finite and greater than zero",
        )?;
        Self {
            curve,
            mode: ApplyMode::default(),
            desat: DEFAULT_DESAT,
            source_peak,
            target_peak: SDR_WHITE_NORMALIZED,
            luma_weights: BT709_LUMA_WEIGHTS,
            weights: weights_as_f32(BT709_LUMA_WEIGHTS),
            scale: 0.0,
        }
        .normalized()
    }

    /// Set the peak the source peak maps to.
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if
    /// `target_peak` is not finite and strictly positive, or if the normalization
    /// `target_peak / curve.map(source_peak)` is not a normal, positive `f32`.
    pub fn with_target_peak(mut self, target_peak: f32) -> Result<Self> {
        self.target_peak = positive_finite(
            target_peak,
            "tone mapper target peak must be finite and greater than zero",
        )?;
        self.normalized()
    }

    /// Set which signal drives the curve.
    #[must_use]
    pub fn with_mode(mut self, mode: ApplyMode) -> Self {
        self.mode = mode;
        self
    }

    /// Set the desaturation threshold; `0.0` disables desaturation.
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if
    /// `desat` is negative or not finite (ffmpeg's own range is `[0, DBL_MAX]`).
    pub fn with_desat(mut self, desat: f32) -> Result<Self> {
        if !desat.is_finite() || desat < 0.0 {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "tone mapper desat must be finite and not negative",
            ));
        }
        self.desat = desat;
        Ok(self)
    }

    /// Set the luma weights the desaturation and [`ApplyMode::Luminance`] use, as `gamut_core`
    /// fixed-point weights — pass [`gamut_core::luminance::BT2020_LUMA_WEIGHTS`] for BT.2020 /
    /// BT.2100 RGB, the usual HDR source.
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if the
    /// weights do not sum to [`LUMA_ONE`]: luma weights sum to unity by definition, and the
    /// desaturation preserves luma only when they do.
    pub fn with_luma_weights(mut self, weights: [u32; 3]) -> Result<Self> {
        if weights.iter().map(|&w| u64::from(w)).sum::<u64>() != u64::from(LUMA_ONE) {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "tone mapper luma weights must sum to LUMA_ONE",
            ));
        }
        self.luma_weights = weights;
        self.weights = weights_as_f32(weights);
        Ok(self)
    }

    /// Recomputes `scale` from the curve and both peaks, refusing a normalization the map loop
    /// could not divide by.
    fn normalized(mut self) -> Result<Self> {
        let scale = self.target_peak / self.curve.map(self.source_peak);
        if !(scale.is_normal() && scale.is_sign_positive()) {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "tone curve must map the source peak to a value that normalizes to a normal, \
                 positive scale",
            ));
        }
        self.scale = scale;
        Ok(self)
    }

    /// Multiply every channel by `T(signal) / signal`, the signal floored as ffmpeg floors it.
    ///
    /// The gain is capped at `f32::MAX` so a curve that saturates to `+∞` cannot turn a zero
    /// channel into `0 · ∞ = NaN`.
    fn scale_by_signal(&self, rgb: [f32; 3], signal: f32) -> [f32; 3] {
        let signal = signal.max(SIGNAL_FLOOR);
        let gain = (self.scale * self.curve.map(signal) / signal).min(f32::MAX);
        rgb.map(|c| c * gain)
    }

    /// The curve.
    #[must_use]
    pub fn curve(&self) -> &C {
        &self.curve
    }

    /// The signal that drives the curve.
    #[must_use]
    pub fn mode(&self) -> ApplyMode {
        self.mode
    }

    /// The desaturation threshold (`0.0` when disabled).
    #[must_use]
    pub fn desat(&self) -> f32 {
        self.desat
    }

    /// The input value that maps to [`target_peak`](Self::target_peak).
    #[must_use]
    pub fn source_peak(&self) -> f32 {
        self.source_peak
    }

    /// The output value [`source_peak`](Self::source_peak) maps to.
    #[must_use]
    pub fn target_peak(&self) -> f32 {
        self.target_peak
    }

    /// The fixed-point luma weights, at [`gamut_core::luminance::LUMA_FIX`] fractional bits.
    #[must_use]
    pub fn luma_weights(&self) -> [u32; 3] {
        self.luma_weights
    }
}

impl<C: ToneCurve> ToneMapper for CurveToneMapper<C> {
    fn map_rgb(&self, rgb: [f32; 3]) -> [f32; 3] {
        let [wr, wg, wb] = self.weights;
        // Capped so a pixel at the top of the f32 range cannot round its luma to `+∞`, which
        // would make the overbright ratio `∞ / ∞`.
        let luma = (wr * rgb[0] + wg * rgb[1] + wb * rgb[2]).min(f32::MAX);
        // The mix toward `luma` preserves luma (the weights sum to one exactly), so the luma
        // computed here is also the desaturated pixel's, and `Luminance` reuses it.
        let rgb = if self.desat > 0.0 {
            let overbright = (luma - self.desat).max(SIGNAL_FLOOR) / luma.max(SIGNAL_FLOOR);
            rgb.map(|c| c * (1.0 - overbright) + luma * overbright)
        } else {
            rgb
        };
        match self.mode {
            ApplyMode::PerChannel => rgb.map(|c| self.scale * self.curve.map(c)),
            ApplyMode::Luminance => self.scale_by_signal(rgb, luma),
            ApplyMode::MaxRgb => self.scale_by_signal(rgb, rgb[0].max(rgb[1]).max(rgb[2])),
        }
    }
}

#[cfg(test)]
mod tests {
    use gamut_core::ErrorKind;
    use gamut_core::luminance::BT2020_LUMA_WEIGHTS;

    use super::*;
    use crate::{Clamp, Exposure, Hable, Linear, Reinhard};

    /// A mapper over `curve` with desaturation off, so a test sees the mode alone.
    fn plain<C: ToneCurve>(curve: C, source_peak: f32, mode: ApplyMode) -> CurveToneMapper<C> {
        CurveToneMapper::new(curve, source_peak)
            .and_then(|m| m.with_desat(0.0))
            .expect("valid parameters")
            .with_mode(mode)
    }

    fn assert_close(got: [f32; 3], want: [f64; 3]) {
        for (g, w) in got.iter().zip(want) {
            let tolerance = 8.0 * f64::from(f32::EPSILON) * w.abs().max(1e-30);
            assert!(
                (f64::from(*g) - w).abs() <= tolerance,
                "got {got:?}, want {want:?}"
            );
        }
    }

    fn assert_rejected<T>(result: Result<T>) {
        match result {
            Ok(_) => panic!("parameter must be rejected"),
            Err(error) => assert_eq!(error.kind(), ErrorKind::InvalidInput),
        }
    }

    #[test]
    fn defaults_are_ffmpegs() {
        let mapper = CurveToneMapper::new(Reinhard, 10.0).expect("valid");
        assert_eq!(mapper.mode(), ApplyMode::MaxRgb);
        assert_eq!(mapper.desat(), 2.0);
        assert_eq!(mapper.target_peak(), 1.0);
        assert_eq!(mapper.luma_weights(), BT709_LUMA_WEIGHTS);
        assert_eq!(mapper.source_peak(), 10.0);
    }

    #[test]
    fn builders_set_what_the_accessors_report() {
        let mapper = CurveToneMapper::new(Clamp::new(4.0).expect("valid"), 3.0)
            .and_then(|m| m.with_target_peak(0.5))
            .and_then(|m| m.with_desat(7.5))
            .and_then(|m| m.with_luma_weights(BT2020_LUMA_WEIGHTS))
            .expect("valid")
            .with_mode(ApplyMode::Luminance);
        assert_eq!(mapper.curve().max(), 4.0);
        assert_eq!(mapper.source_peak(), 3.0);
        assert_eq!(mapper.target_peak(), 0.5);
        assert_eq!(mapper.desat(), 7.5);
        assert_eq!(mapper.mode(), ApplyMode::Luminance);
        assert_eq!(mapper.luma_weights(), BT2020_LUMA_WEIGHTS);
    }

    #[test]
    fn apply_mode_discriminants_are_permanent() {
        assert_eq!(ApplyMode::PerChannel as u32, 0);
        assert_eq!(ApplyMode::Luminance as u32, 1);
        assert_eq!(ApplyMode::MaxRgb as u32, 2);
    }

    #[test]
    fn peaks_must_be_finite_and_positive() {
        for bad in [0.0, -1.0, f32::INFINITY, f32::NAN] {
            assert_rejected(CurveToneMapper::new(Linear, bad));
            let mapper = CurveToneMapper::new(Linear, 1.0).expect("valid");
            assert_rejected(mapper.with_target_peak(bad));
        }
    }

    #[test]
    fn a_curve_positive_everywhere_still_needs_a_positive_source_peak() {
        // With `Linear` a bad peak also fails the normalization check, so it cannot tell whether
        // the peak itself was validated. This curve normalizes fine at 0 and −1 (scale 1 and
        // 1/2), so only the peak's own check rejects them.
        let lifted = |x: f32| x.abs() + 1.0;
        for bad in [0.0, -1.0] {
            assert_rejected(CurveToneMapper::new(lifted, bad));
        }
    }

    #[test]
    fn desat_must_be_finite_and_not_negative() {
        let mapper = CurveToneMapper::new(Linear, 1.0).expect("valid");
        for bad in [-1e-3, f32::INFINITY, f32::NAN] {
            assert_rejected(mapper.with_desat(bad));
        }
        assert_eq!(mapper.with_desat(0.0).expect("zero disables").desat(), 0.0);
    }

    #[test]
    fn luma_weights_must_sum_to_one() {
        let mapper = CurveToneMapper::new(Linear, 1.0).expect("valid");
        assert_rejected(mapper.with_luma_weights([13_933, 46_871, 4_731]));
        // Sums to LUMA_ONE only modulo 2³², which a u32 sum would accept.
        assert_rejected(mapper.with_luma_weights([u32::MAX, 65_537, 0]));
    }

    #[test]
    fn a_curve_without_a_normalization_is_rejected() {
        // Zero at the peak (scale +∞), negative at the peak, and a peak so bright the scale
        // underflows below the normal range.
        assert_rejected(CurveToneMapper::new(|_x: f32| 0.0, 1.0));
        assert_rejected(CurveToneMapper::new(|x: f32| -x, 1.0));
        assert_rejected(
            CurveToneMapper::new(Linear, 1.0e30).and_then(|m| m.with_target_peak(1e-10)),
        );
        // A curve whose `+∞` peak leaves scale 0 is rejected too.
        assert_rejected(CurveToneMapper::new(
            Exposure::new(1e30).expect("valid gain"),
            1e30,
        ));
    }

    #[test]
    fn retargeting_renormalizes() {
        // Reinhard maps the peak 3 to 0.75; target 0.5 makes the scale 2/3.
        let mapper = plain(Reinhard, 3.0, ApplyMode::PerChannel)
            .with_target_peak(0.5)
            .expect("valid");
        assert_close(mapper.map_rgb([3.0, 1.0, 0.0]), [0.5, 1.0 / 3.0, 0.0]);
    }

    #[test]
    fn per_channel_maps_each_channel_through_the_normalized_curve() {
        // T(x) = (x / (1 + x)) / (4 / 5): channels 4, 1, 0.25 -> 1, 0.625, 0.25.
        let mapper = plain(Reinhard, 4.0, ApplyMode::PerChannel);
        assert_close(mapper.map_rgb([4.0, 1.0, 0.25]), [1.0, 0.625, 0.25]);
    }

    #[test]
    fn max_rgb_scales_every_channel_by_the_brightest_ones_gain() {
        // max = 4 -> T(4) = 1, gain 1/4.
        let mapper = plain(Reinhard, 4.0, ApplyMode::MaxRgb);
        assert_close(mapper.map_rgb([1.0, 4.0, 2.0]), [0.25, 1.0, 0.5]);
    }

    #[test]
    fn luminance_scales_every_channel_by_the_lumas_gain() {
        // BT.2020 weights make the green-only pixel's luma 2 · 44434/65536; Linear at peak 2
        // halves it, so the gain is exactly 1/2 whatever the luma.
        let mapper = plain(Linear, 2.0, ApplyMode::Luminance);
        assert_close(mapper.map_rgb([0.0, 2.0, 1.0]), [0.0, 1.0, 0.5]);

        // Reinhard at peak 1 doubles, so T(Y) / Y = 2 / (1 + Y): Y is the BT.2020 luma of the
        // pixel, which tells the weights apart from BT.709's.
        let mapper = plain(Reinhard, 1.0, ApplyMode::Luminance)
            .with_luma_weights(BT2020_LUMA_WEIGHTS)
            .expect("valid");
        let y = 17_216.0 / 65_536.0 + 3.0 * 44_434.0 / 65_536.0;
        let gain = 2.0 / (1.0 + y);
        assert_close(mapper.map_rgb([1.0, 3.0, 0.0]), [gain, 3.0 * gain, 0.0]);
    }

    #[test]
    fn black_maps_to_black_through_the_signal_floor() {
        for mode in [ApplyMode::Luminance, ApplyMode::MaxRgb] {
            let mapper = plain(Hable::default(), 10.0, mode);
            assert_eq!(mapper.map_rgb([0.0; 3]), [0.0; 3]);
        }
    }

    #[test]
    fn zero_desat_leaves_the_pixel_saturated() {
        // With desat 0 the step is skipped. Were it run, `overbright` would be Y / Y = 1 and the
        // pixel would turn fully grey.
        let mapper = plain(Linear, 1.0, ApplyMode::MaxRgb);
        assert_close(mapper.map_rgb([1.0, 0.0, 0.0]), [1.0, 0.0, 0.0]);
    }

    #[test]
    fn desaturation_mixes_toward_luma_by_the_overbright_ratio() {
        // BT.709, pixel (8, 0, 0): Y = 8 · 13933/65536, desat 1, so overbright = (Y − 1) / Y and
        // each channel becomes c(1 − o) + Y·o. PerChannel `Linear` at peak 1 is the identity, so
        // the output is the desaturated pixel itself.
        let mapper = plain(Linear, 1.0, ApplyMode::PerChannel)
            .with_desat(1.0)
            .expect("valid");
        let y = 8.0 * 13_933.0 / 65_536.0;
        let o = (y - 1.0) / y;
        let r = 8.0 * (1.0 - o) + y * o;
        let g = y * o;
        assert_close(mapper.map_rgb([8.0, 0.0, 0.0]), [r, g, g]);
    }

    #[test]
    fn desaturation_below_the_threshold_uses_the_floor() {
        // Y < desat: overbright = 1e-6 / Y, a near-zero mix — not zero.
        let mapper = plain(Linear, 1.0, ApplyMode::PerChannel)
            .with_desat(2.0)
            .expect("valid");
        let y = 13_933.0 / 65_536.0_f64;
        let o = 1e-6 / y;
        assert_close(
            mapper.map_rgb([1.0, 0.0, 0.0]),
            [1.0 - o + y * o, y * o, y * o],
        );
    }

    #[test]
    fn a_saturating_curve_does_not_make_nan_of_a_zero_channel() {
        // Exposure's gain overflows for this signal; the zero channel must stay zero.
        let mapper = plain(Exposure::new(1e30).expect("valid"), 1.0, ApplyMode::MaxRgb);
        let out = mapper.map_rgb([1e20, 0.0, 1.0]);
        assert!(out.iter().all(|c| !c.is_nan()), "{out:?}");
        assert_eq!(out[1], 0.0);
    }

    #[test]
    fn map_rgb_slice_maps_every_pixel() {
        let mapper = CurveToneMapper::new(Hable::default(), 10.0).expect("valid");
        let mut buf = [[0.5_f32, 1.0, 2.0], [10.0, 3.0, 0.0], [0.0; 3]];
        let expected = buf.map(|px| mapper.map_rgb(px));
        mapper.map_rgb_slice(&mut buf);
        assert_eq!(buf, expected);
    }
}
