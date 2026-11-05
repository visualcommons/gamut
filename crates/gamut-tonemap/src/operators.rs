//! Built-in tone-mapping operators implementing [`ToneCurve`].
//!
//! Every operator maps non-negative linear-light input to a tone-mapped output. Parameterless
//! operators ([`Linear`], [`Reinhard`], [`Aces`]) are zero-sized; parameterized operators
//! ([`Clamp`], [`Exposure`], [`ReinhardExtended`], [`Hable`], [`Drago`]) validate their parameters
//! at construction so an out-of-range value can never reach
//! [`map`](crate::curve::ToneCurve::map). Each operator's exact formula and primary source are
//! recorded in `references/tonemap/README.md`.

use gamut_core::{Error, Result};

use crate::constants::{
    DEFAULT_DRAGO_BIAS, DEFAULT_HABLE_WHITE, DEFAULT_REINHARD_WHITE, SDR_WHITE_NORMALIZED,
};
use crate::curve::ToneCurve;

/// Identity passthrough: `map(x) == x`.
///
/// Useful as a no-op default in a generic pipeline parameterized over a [`ToneCurve`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Linear;

impl ToneCurve for Linear {
    #[inline]
    fn map(&self, x: f32) -> f32 {
        x
    }
}

/// Hard-clamp curve: `map(x) == x.clamp(0.0, max)`.
///
/// The simplest way to fit an out-of-range signal into `[0, max]`; unlike the Reinhard operators
/// it preserves values below `max` exactly and flatly discards everything above.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Clamp {
    max: f32,
}

impl Clamp {
    /// Create a clamp curve with upper bound `max`.
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if `max`
    /// is not finite or is not strictly positive.
    pub fn new(max: f32) -> Result<Self> {
        if !max.is_finite() || max <= 0.0 {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "Clamp max must be finite and greater than zero",
            ));
        }
        Ok(Self { max })
    }

    /// The upper bound this curve clamps to.
    #[must_use]
    pub fn max(self) -> f32 {
        self.max
    }
}

impl Default for Clamp {
    fn default() -> Self {
        Self {
            max: SDR_WHITE_NORMALIZED,
        }
    }
}

impl ToneCurve for Clamp {
    #[inline]
    fn map(&self, x: f32) -> f32 {
        x.clamp(0.0, self.max)
    }
}

/// Reinhard tone mapping: `map(x) == x / (1 + x)`.
///
/// The canonical parameterless global operator; maps `[0, ∞)` into `[0, 1)`. Highlights roll off
/// smoothly but never quite reach `1.0`, so very bright detail is compressed rather than clipped.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Reinhard;

impl ToneCurve for Reinhard {
    #[inline]
    fn map(&self, x: f32) -> f32 {
        x / (1.0 + x)
    }
}

/// Extended Reinhard with a white point: `map(x) == x * (1 + x / white²) / (1 + x)`.
///
/// `white` is the smallest linear input that maps exactly to `1.0`; inputs above it are pushed
/// past `1.0`, so callers typically pick `white` at or above the brightest sample. As
/// `white → ∞` the operator reduces to plain [`Reinhard`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ReinhardExtended {
    white: f32,
    /// `white * white`, the divisor `map` uses — validated normal at construction.
    white_sq: f32,
}

impl ReinhardExtended {
    /// Create an extended-Reinhard curve whose white point is `white`.
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if
    /// `white` is not finite, is not strictly positive, or if `white * white` is not a normal f32 — squares that underflow (white ≲ 1.1e-19) would make
    /// `map` divide by zero or a subnormal, and squares that overflow (white ≳ 1.8e19) would break
    /// the `map(white) == 1` fixed point.
    pub fn new(white: f32) -> Result<Self> {
        if !white.is_finite() || white <= 0.0 {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "Reinhard white point must be finite and greater than zero",
            ));
        }
        let white_sq = white * white;
        if !white_sq.is_normal() {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "Reinhard white point squared must be a normal f32 (white roughly within [1.1e-19, 1.8e19])",
            ));
        }
        Ok(Self { white, white_sq })
    }

    /// The white point: the linear input that maps to display white (`1.0`).
    #[must_use]
    pub fn white(self) -> f32 {
        self.white
    }
}

impl Default for ReinhardExtended {
    fn default() -> Self {
        Self {
            white: DEFAULT_REINHARD_WHITE,
            white_sq: DEFAULT_REINHARD_WHITE * DEFAULT_REINHARD_WHITE,
        }
    }
}

impl ToneCurve for ReinhardExtended {
    #[inline]
    fn map(&self, x: f32) -> f32 {
        // Factored form of Eq (4), associated so that nothing overflows ahead of the result.
        //
        // `t = x / (1 + x)` is bounded by 1, and the algebra
        //
        //     x · (1 + x/white²) / (1 + x)  =  t + (x·t) / white²
        //
        // divides `x·t = x²/(1 + x) < x` by white² rather than `x` itself. That matters at the
        // bottom of the admitted white range: the earlier form computed `x / white²` on its own,
        // which reaches +inf for white just above the `white_sq.is_normal()` threshold even when
        // the whole expression is comfortably inside f32 (#471). It also lands `map(white)` on
        // exactly 1.0 at extreme whites, where the earlier order returned 0.999999940.
        //
        // What remains is only the contract's permitted saturation: this form overflows just when
        // the exact value does. See references/tonemap/README.md.
        let t = x / (1.0 + x);
        t + (x * t) / self.white_sq
    }
}

/// Exposure pre-scaling: `map(x) == x * scale`.
///
/// A composable linear gain applied before another curve — the explicit form of the exposure /
/// key-value step several operators fold in (Reinhard's key scaling, the ACES `×0.6` input,
/// Hable's `×2.0` bias). There is no `Default`: there is no canonical exposure — use [`Linear`] for
/// a no-op. See `references/tonemap/README.md`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Exposure {
    scale: f32,
}

impl Exposure {
    /// Create an exposure curve with linear gain `scale`.
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if
    /// `scale` is not finite or is not strictly positive.
    pub fn new(scale: f32) -> Result<Self> {
        if !scale.is_finite() || scale <= 0.0 {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "Exposure scale must be finite and greater than zero",
            ));
        }
        Ok(Self { scale })
    }

    /// Create an exposure curve from a photographic stop count: `scale == 2^stops` (one stop
    /// doubles exposure).
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if
    /// `2^stops` is not a finite, strictly positive gain: when
    /// `stops` is NaN, large enough that the gain overflows to infinity (≥ 128), or negative
    /// enough that it underflows to zero (≲ −150).
    pub fn from_stops(stops: f32) -> Result<Self> {
        Self::new(stops.exp2())
    }

    /// The linear gain this curve multiplies by.
    #[must_use]
    pub fn scale(self) -> f32 {
        self.scale
    }
}

impl ToneCurve for Exposure {
    #[inline]
    fn map(&self, x: f32) -> f32 {
        x * self.scale
    }
}

/// ACES filmic tone-mapping curve — the Narkowicz (2016) approximation:
/// `saturate((x·(2.51x + 0.03)) / (x·(2.43x + 0.59) + 0.14))`.
///
/// A luminance-only rational fit to the ACES `ODT(RRT(x))` for Rec.709/D65 output; **not** the full
/// ACES transform (which is colour-space-coupled and out of scope for a scalar curve). Maps
/// `[0, ∞) → [0, 1]`. The input is taken as pre-exposed (so `1 → ≈0.8`); apply
/// [`Exposure::new`]`(0.6)` first to match the original ACES curve. See
/// `references/tonemap/README.md`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Aces;

impl ToneCurve for Aces {
    #[inline]
    fn map(&self, x: f32) -> f32 {
        // Narkowicz 2016 coefficients (see references/tonemap/README.md).
        const A: f32 = 2.51;
        const B: f32 = 0.03;
        const C: f32 = 2.43;
        const D: f32 = 0.59;
        const E: f32 = 0.14;
        // The unclamped rational is >= 1 for every x >= 7.2417 (see references/tonemap/README.md
        // for the derivation), so under the saturate, evaluating at min(x, 8) is exact — and it
        // keeps the quadratics from overflowing to inf/inf = NaN for x beyond ~1e19. An argument
        // clamp rather than an early return so the hot loop stays branchless.
        let x = x.min(8.0);
        ((x * (A * x + B)) / (x * (C * x + D) + E)).clamp(0.0, 1.0)
    }
}

// Hable / Uncharted 2 filmic constants (John Hable, 2010; see references/tonemap/README.md).
const HABLE_A: f32 = 0.15; // shoulder strength
const HABLE_B: f32 = 0.50; // linear strength
const HABLE_C: f32 = 0.10; // linear angle
const HABLE_D: f32 = 0.20; // toe strength
const HABLE_E: f32 = 0.02; // toe numerator
const HABLE_F: f32 = 0.30; // toe denominator

/// Largest argument `hable_partial` evaluates directly. Beyond it the curve sits within one f32
/// ULP of its horizontal asymptote (see references/tonemap/README.md), while the unguarded
/// quadratics would overflow to inf/inf = NaN near ~1.5e19 — so clamping is sub-ULP-exact.
const HABLE_EVAL_MAX: f32 = 1e18;

/// The un-normalized Uncharted 2 partial curve `partial(x)` (see [`Hable`]).
fn hable_partial(x: f32) -> f32 {
    let x = x.min(HABLE_EVAL_MAX);
    ((x * (HABLE_A * x + HABLE_C * HABLE_B) + HABLE_D * HABLE_E)
        / (x * (HABLE_A * x + HABLE_B) + HABLE_D * HABLE_F))
        - HABLE_E / HABLE_F
}

/// Hable / Uncharted 2 filmic operator (John Hable, 2010), white-point normalized so
/// `map(white) == 1` and `map(0) == 0`.
///
/// `map(x) = partial(x) / partial(white)` with the published shoulder/toe constants. The original
/// presentation additionally applied a `×2.0` exposure bias (compose [`Exposure::new`]`(2.0)`) and a
/// final `pow(·, 1/2.2)` display encode (the target transfer function's job, e.g. via
/// `gamut-color`) — both excluded here. See `references/tonemap/README.md`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Hable {
    white: f32,
    /// `hable_partial(white)`, the normalizing divisor `map` uses. Always positive: `partial` is
    /// increasing and its value at 0 is a positive f32 residual (see `references/tonemap`).
    partial_white: f32,
}

impl Hable {
    /// Create a Hable curve whose linear white point is `white` (`map(white) == 1`).
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if
    /// `white` is not finite, is not strictly positive, or if the derived divisor `hable_partial(white)` is not a normal f32. That last case is not a
    /// range: `hable_partial` ends in a subtraction that cancels, and at isolated white points the
    /// cancellation lands on exactly zero, which would make every `map` call divide by it —
    /// `map(white)` returning `0 / 0 = NaN` and every other input `+∞`. Found by the fuzz tier
    /// (#264) at `white = 3.738137e-9`; the guard mirrors the one
    /// [`ReinhardExtended::new`] already applies to `white²` and [`Drago::new`] to its prefactor.
    pub fn new(white: f32) -> Result<Self> {
        if !white.is_finite() || white <= 0.0 {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "Hable white point must be finite and greater than zero",
            ));
        }
        let partial_white = hable_partial(white);
        if !partial_white.is_normal() || partial_white <= 0.0 {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "Hable white point must yield a normal, positive curve divisor",
            ));
        }
        Ok(Self {
            white,
            partial_white,
        })
    }

    /// The linear white point: the input that maps to display white (`1.0`).
    #[must_use]
    pub fn white(self) -> f32 {
        self.white
    }
}

impl Default for Hable {
    fn default() -> Self {
        Self {
            white: DEFAULT_HABLE_WHITE,
            partial_white: hable_partial(DEFAULT_HABLE_WHITE),
        }
    }
}

impl ToneCurve for Hable {
    #[inline]
    fn map(&self, x: f32) -> f32 {
        // `map(white) == 1.0` stays exact: `hable_partial(white)` here reproduces the stored
        // divisor bit-for-bit, and t / t == 1.0.
        hable_partial(x) / self.partial_white
    }
}

// Drago et al. 2003 reference display max (cd/m²); `Ldmax · 0.01 = 1` normalizes output to [0, 1].
const DRAGO_LDMAX_NITS: f32 = 100.0;

/// Drago et al. (2003) adaptive logarithmic operator, mapping scene luminance to display-relative
/// `[0, 1]` (`Ldmax` normalized to 100 cd/m²).
///
/// Needs the scene's maximum luminance, so there is no `Default`. `map(0) == 0` and
/// `map(world_max) == 1`. For `bias < 0.7` the output may exceed `1.0` (the display clamps to
/// `Ldmax`); `map` is a faithful, clamp-free transcription of the paper's Eq. (4). Inputs are
/// meaningful on the paper's domain `[0, world_max]` — far beyond it the output stays finite,
/// non-negative, and non-NaN, but the curve no longer tracks the model.
///
/// # Monotonicity is conditional
///
/// Eq. (4) is **not** monotonic non-decreasing for every `(world_max, bias)` pair the
/// constructors accept, not even across `[0, world_max]`. That is a property of the published
/// formula, not of this transcription, and it is [`Drago::is_monotonic`] that answers it for a
/// given curve. Ask before relying on the ordering; the ceiling rises steeply with `bias`:
///
/// | `bias` | monotonic for `world_max` up to |
/// |---|---|
/// | 0.50 | 13.6 |
/// | 0.70 | 262 |
/// | 0.80 | 7.6 × 10³ |
/// | 0.85 ([`DEFAULT_DRAGO_BIAS`]) | 2.1 × 10⁵ |
/// | 0.90 | 1.7 × 10⁸ |
/// | 0.95 | 7.8 × 10¹⁶ |
///
/// The paper's own worked example is `Lwmax = 230 cd/m²`, so the default configuration is
/// monotonic with three orders of magnitude to spare. See `references/tonemap/README.md` for the
/// derivation, and issue #439 for how this was found.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Drago {
    world_max: f32,
    bias: f32,
    /// The Eq. (3) bias exponent `ln(bias) / ln(0.5)` — recomputed whenever `bias` changes.
    exponent: f32,
    /// The Eq. (4) prefactor `(Ldmax · 0.01) / log10(world_max + 1)` — validated finite and
    /// positive at construction (a too-small `world_max` makes the `log10` collapse to zero).
    scale: f32,
}

impl Drago {
    /// Create a Drago curve for a scene whose maximum luminance is `world_max`, with the default
    /// bias ([`DEFAULT_DRAGO_BIAS`]). Tune it with [`Drago::with_bias`].
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if
    /// `world_max` is not finite or is not strictly positive,
    /// or if it is so small (≲ 6e-8) that `world_max + 1` rounds to `1.0` in f32, collapsing the
    /// curve's `log10(world_max + 1)` normalizer to zero.
    pub fn new(world_max: f32) -> Result<Self> {
        if !world_max.is_finite() || world_max <= 0.0 {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "Drago world_max must be finite and greater than zero",
            ));
        }
        let scale = (DRAGO_LDMAX_NITS * 0.01) / (world_max + 1.0).log10();
        if !scale.is_finite() || scale <= 0.0 {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "Drago world_max must be large enough that log10(world_max + 1) is positive in f32",
            ));
        }
        Ok(Self {
            world_max,
            bias: DEFAULT_DRAGO_BIAS,
            exponent: Self::bias_exponent(DEFAULT_DRAGO_BIAS),
            scale,
        })
    }

    /// Set the bias, which steers contrast in dark vs. bright regions; any value in the open
    /// interval `(0, 1)` is accepted, the paper's useful range is `[0.5, 1.0]`, default `0.85`.
    ///
    /// Bias also decides how much of the domain stays monotonic — see [`Drago::is_monotonic`],
    /// which is worth re-asking after changing it.
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if `bias`
    /// is not finite or is not in the open interval `(0, 1)`.
    pub fn with_bias(self, bias: f32) -> Result<Self> {
        if !bias.is_finite() || bias <= 0.0 || bias >= 1.0 {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "Drago bias must be finite and in the open interval (0, 1)",
            ));
        }
        Ok(Self {
            bias,
            exponent: Self::bias_exponent(bias),
            ..self
        })
    }

    /// The Eq. (3) bias exponent `ln(bias) / ln(0.5)` (see `references/tonemap/README.md`).
    fn bias_exponent(bias: f32) -> f32 {
        bias.ln() / 0.5_f32.ln()
    }

    /// The scene's maximum luminance.
    #[must_use]
    pub fn world_max(self) -> f32 {
        self.world_max
    }

    /// The bias parameter.
    #[must_use]
    pub fn bias(self) -> f32 {
        self.bias
    }

    /// Whether [`map`](ToneCurve::map) is monotonic non-decreasing across the whole of this
    /// curve's documented domain `[0, world_max]`.
    ///
    /// Eq. (4)'s numerator grows like `ln(x + 1)` while its denominator grows like
    /// `ln(2 + 8·(x/world_max)^k)`, with `k = ln(bias) / ln(0.5)`. For a large enough domain the
    /// denominator outruns the numerator over a band of `x` and the quotient falls. Differentiating
    /// and collecting terms, `map` is non-decreasing at `x` exactly when
    ///
    /// ```text
    /// x · (2 + w) · ln(2 + w)  ≥  (x + 1) · ln(x + 1) · k · w,     w = 8·(x/world_max)^k
    /// ```
    ///
    /// and the tightest point of that is the top of the domain, `x = world_max`, where `w = 8`:
    ///
    /// ```text
    /// world_max · 10 · ln(10)  ≥  8 · k · (world_max + 1) · ln(world_max + 1)
    /// ```
    ///
    /// which is what this evaluates. See the table on [`Drago`] for what it means in practice, and
    /// `references/tonemap/README.md` for the derivation and the numerical corroboration.
    ///
    /// Computed in `f64` from `bias` rather than from the stored `f32` exponent, so the answer
    /// does not flip on rounding for parameters sitting right at the boundary.
    #[must_use]
    pub fn is_monotonic(self) -> bool {
        let world_max = f64::from(self.world_max);
        let k = f64::from(self.bias).ln() / 0.5_f64.ln();
        8.0 * k * (world_max + 1.0) * (world_max + 1.0).ln() <= world_max * 10.0 * 10.0_f64.ln()
    }
}

impl ToneCurve for Drago {
    #[inline]
    fn map(&self, x: f32) -> f32 {
        // Drago et al. 2003 Eq. (4) with the Eq. (3) bias (see references/tonemap/README.md).
        // `exponent` and `scale` are hoisted to construction — same expressions, identical values.
        let ratio = (x / self.world_max).powf(self.exponent);
        let denom = (2.0 + ratio * 8.0).ln();
        self.scale * (x + 1.0).ln() / denom
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Absolute tolerance for f32 comparisons of derived (non-exact) values.
    const EPS: f32 = 1e-6;

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() <= EPS
    }

    fn close_eps(a: f32, b: f32, eps: f32) -> bool {
        (a - b).abs() <= eps
    }

    /// Assert `curve` is strictly increasing across a sampled grid (kills sign/operator mutants).
    fn assert_monotonic_increasing(curve: &impl ToneCurve, start: f32, step: f32, samples: u32) {
        let mut prev = curve.map(start);
        for i in 1..=samples {
            let y = curve.map(start + i as f32 * step);
            assert!(y > prev, "not increasing at sample {i}");
            prev = y;
        }
    }

    #[test]
    fn linear_is_identity() {
        let c = Linear;
        for x in [0.0, 0.5, 1.0, 42.0, 1e6] {
            assert_eq!(c.map(x), x);
        }
    }

    #[test]
    fn clamp_bounds_input() {
        let c = Clamp::new(1.0).expect("1.0 is a valid bound");
        assert_eq!(c.map(-1.0), 0.0);
        assert_eq!(c.map(0.5), 0.5);
        assert_eq!(c.map(2.0), 1.0);
        assert_eq!(c.max(), 1.0);
        assert_eq!(Clamp::default().max(), SDR_WHITE_NORMALIZED);
        // A non-1.0 bound pins `max()`: the cases above all equal 1.0, which a constant-1.0 mutant
        // also returns.
        let c2 = Clamp::new(2.5).expect("valid bound");
        assert_eq!(c2.max(), 2.5);
        assert_eq!(c2.map(3.0), 2.5);
    }

    #[test]
    fn clamp_rejects_invalid_max() {
        assert!(Clamp::new(0.0).is_err());
        assert!(Clamp::new(-1.0).is_err());
        assert!(Clamp::new(f32::NAN).is_err());
        assert!(Clamp::new(f32::INFINITY).is_err());
    }

    #[test]
    fn reinhard_fixed_points_and_monotonic() {
        let c = Reinhard;
        assert_eq!(c.map(0.0), 0.0);
        assert_eq!(c.map(1.0), 0.5);
        // Independent interior values pin x/(1+x) beyond the 0 and 1 fixed points.
        assert!(close(c.map(3.0), 0.75));
        assert!(close(c.map(0.25), 0.2));
        assert_monotonic_increasing(&c, 0.0, 0.1, 1000);
        // Asymptotically approaches 1 from below.
        assert!(c.map(1e7) < 1.0);
        assert!(close(c.map(1e7), 1.0));
    }

    #[test]
    fn reinhard_extended_white_point_and_convergence() {
        let white = 4.0;
        let c = ReinhardExtended::new(white).expect("positive white point");
        assert_eq!(c.map(0.0), 0.0);
        assert!(close(c.map(white), 1.0));
        assert_eq!(c.white(), white);
        // An interior value pins the white² term beyond the 0 and white fixed points.
        assert!(close(c.map(2.0), 0.75));
        assert_monotonic_increasing(&c, 0.0, 0.01, 1000);

        // As white → ∞ it converges to plain Reinhard.
        let wide = ReinhardExtended::new(1e6).expect("positive white point");
        let plain = Reinhard;
        for x in [0.1, 1.0, 5.0, 50.0] {
            assert!(close(wide.map(x), plain.map(x)), "mismatch at {x}");
        }

        // The default white point is the documented HDR/SDR reference ratio; pin the literal value
        // (203 / 100 = 2.03) so an accidental edit is observable.
        assert_eq!(ReinhardExtended::default().white(), DEFAULT_REINHARD_WHITE);
        assert!(close(DEFAULT_REINHARD_WHITE, 2.03));
    }

    #[test]
    fn reinhard_extended_rejects_invalid_white() {
        assert!(ReinhardExtended::new(0.0).is_err());
        assert!(ReinhardExtended::new(-2.0).is_err());
        assert!(ReinhardExtended::new(f32::NAN).is_err());
        assert!(ReinhardExtended::new(f32::INFINITY).is_err());
        // Degenerate whites whose square leaves the normal f32 range: underflow to zero (map
        // would divide by 0), subnormal (map overflows at moderate x), and overflow to infinity
        // (map(white) would no longer be 1).
        assert!(ReinhardExtended::new(1e-25).is_err());
        assert!(ReinhardExtended::new(1e-20).is_err());
        assert!(ReinhardExtended::new(1e30).is_err());
    }

    #[test]
    fn reinhard_extended_holds_at_the_bottom_of_its_admitted_range() {
        // #471. The guard admits any white whose square is a normal f32, which reaches down to
        // about 1.1e-19 -- but the evaluation order used to compute `x / white^2` on its own, and
        // that quotient overflows to +inf just above the threshold even where the whole
        // expression fits f32 with room to spare.
        //
        // 1.7141993e-19 is the measured worst case: the largest admitted white that used to
        // return +inf. At x = 10 the exact value is about 3.09e38, inside f32's range.
        let c = ReinhardExtended::new(1.7141993e-19).expect("white^2 is normal, so it is admitted");
        let y = c.map(10.0);
        assert!(y.is_finite(), "map(10.0) must not overflow, got {y}");
        assert!(y > 3.0e38, "and it must be the real value, got {y}");

        // The white point still maps to display white, which is the property the guard exists to
        // protect in the first place.
        assert!(close_eps(c.map(c.white()), 1.0, 1e-5));

        // Genuine saturation is still allowed: at a white this small the exact value at x = 100
        // really does exceed f32, and +inf is the contract's answer, not a defect.
        assert!(c.map(100.0).is_infinite());
    }

    #[test]
    fn exposure_scales_by_gain() {
        let c = Exposure::new(2.0).expect("positive gain");
        assert_eq!(c.map(0.0), 0.0);
        assert_eq!(c.map(3.0), 6.0);
        assert_eq!(c.scale(), 2.0);
        // from_stops: one stop doubles, zero stops is unity, minus one stop halves.
        assert_eq!(Exposure::from_stops(1.0).expect("finite").scale(), 2.0);
        assert_eq!(Exposure::from_stops(0.0).expect("finite").scale(), 1.0);
        assert_eq!(Exposure::from_stops(-1.0).expect("finite").scale(), 0.5);
    }

    #[test]
    fn exposure_rejects_invalid() {
        assert!(Exposure::new(0.0).is_err());
        assert!(Exposure::new(-1.0).is_err());
        assert!(Exposure::new(f32::NAN).is_err());
        assert!(Exposure::new(f32::INFINITY).is_err());
        assert!(Exposure::from_stops(f32::NAN).is_err());
        assert!(Exposure::from_stops(f32::INFINITY).is_err()); // 2^inf = inf
    }

    #[test]
    fn aces_fixed_points_and_reference() {
        let c = Aces;
        assert_eq!(c.map(0.0), 0.0);
        // Independent golden values for the Narkowicz curve (see references/tonemap/README.md).
        assert!(close_eps(c.map(0.5), 0.616_307, 1e-5));
        assert!(close_eps(c.map(1.0), 0.803_797, 1e-5));
        // Saturates to 1 for large inputs (unclamped limit a/c ≈ 1.033).
        assert_eq!(c.map(1e6), 1.0);
        // Strictly increasing below saturation (the curve reaches 1.0 near x ≈ 7.24).
        assert_monotonic_increasing(&c, 0.01, 0.01, 200);
    }

    #[test]
    fn hable_fixed_points_and_reference() {
        let c = Hable::default();
        assert_eq!(c.white(), DEFAULT_HABLE_WHITE);
        // partial(0) cancels to ~0 in exact math; f32 leaves a sub-ULP residual.
        assert!(close_eps(c.map(0.0), 0.0, 1e-6));
        assert_eq!(c.map(c.white()), 1.0); // partial(W)/partial(W) is exactly 1
        // Independent golden values at W = 11.2 (see references/tonemap/README.md).
        assert!(close_eps(c.map(1.0), 0.304_300, 1e-4));
        assert!(close_eps(c.map(4.0), 0.713_240, 1e-4));
        assert_monotonic_increasing(&c, 0.05, 0.05, 220);
        // A custom white point pins white() and the unit-output input.
        let c2 = Hable::new(6.0).expect("positive white");
        assert_eq!(c2.white(), 6.0);
        assert_eq!(c2.map(6.0), 1.0);
    }

    #[test]
    fn hable_rejects_invalid_white() {
        assert!(Hable::new(0.0).is_err());
        assert!(Hable::new(-1.0).is_err());
        assert!(Hable::new(f32::NAN).is_err());
        assert!(Hable::new(f32::INFINITY).is_err());
    }

    #[test]
    fn drago_fixed_points_and_reference() {
        let c = Drago::new(100.0).expect("positive world max");
        assert_eq!(c.world_max(), 100.0);
        assert_eq!(c.bias(), DEFAULT_DRAGO_BIAS);
        assert_eq!(c.map(0.0), 0.0);
        assert!(close_eps(c.map(c.world_max()), 1.0, 1e-5));
        // Independent golden value at world_max = 100, bias = 0.85 (see references/tonemap/README.md).
        assert!(close_eps(c.map(10.0), 0.630_858, 1e-4));
        assert_monotonic_increasing(&c, 0.1, 0.1, 500);
        // bias = 0.5 makes the bias exponent exactly 1; the world-max fixed point still holds.
        let c2 = c.with_bias(0.5).expect("bias in (0,1)");
        assert_eq!(c2.bias(), 0.5);
        assert!(close_eps(c2.map(c2.world_max()), 1.0, 1e-5));
        // Interior golden through with_bias: map(10) = ln(11)/ln(2.8)/log10(101) ≈ 1.161946 (see
        // references/tonemap/README.md; > 1 is expected for bias < 0.7). The fixed points above
        // are bias-exponent-insensitive (1^e = 1, and map(0) = 0 for any exponent), so only this
        // value catches with_bias failing to recompute the hoisted exponent.
        assert!(close_eps(c2.map(10.0), 1.161_946, 1e-4));
    }

    #[test]
    fn operators_never_return_nan_at_f32_extremes() {
        // The trait contract's never-NaN clause, probed where the naive math would overflow to
        // inf/inf = NaN (Aces and Hable near 1e19+ without their guards). Outputs may legitimately
        // saturate to +inf where the exact value exceeds f32 range (e.g. Exposure), but never NaN,
        // never negative. Drago at 1e20 is deliberately far beyond world_max — out-of-domain, so
        // only the never-NaN/non-negative clauses apply there.
        let curves: [&dyn ToneCurve; 8] = [
            &Linear,
            &Clamp::default(),
            &Exposure::new(2.0).expect("valid gain"),
            &Reinhard,
            &ReinhardExtended::default(),
            &Aces,
            &Hable::default(),
            &Drago::new(100.0).expect("valid world max"),
        ];
        for (i, curve) in curves.iter().enumerate() {
            for x in [0.0, 1e20, f32::MAX] {
                let y = curve.map(x);
                assert!(!y.is_nan(), "curve {i} returned NaN at {x}");
                assert!(y >= 0.0, "curve {i} returned {y} at {x}");
            }
        }
        // Exact values pin the guards themselves — a deleted or boundary-shifted guard survives
        // the NaN checks alone: the unclamped Aces rational is >= 1 for every x >= 7.2417, and
        // Hable evaluates at its clamp bound for any larger input.
        assert_eq!(Aces.map(1e20), 1.0);
        let h = Hable::default();
        assert_eq!(h.map(f32::MAX), h.map(1e18));
    }

    /// The published monotonicity ceilings, straddled from both sides.
    ///
    /// These are the numbers the `Drago` rustdoc and `references/tonemap/README.md` print, so
    /// they are pinned where a reader can check them rather than asserted in prose. Each bias is
    /// tested just inside and just outside its ceiling, which is what makes the pair sensitive to
    /// the constants in `is_monotonic` -- a one-sided assertion passes for a predicate that always
    /// returns `true`.
    #[test]
    fn drago_monotonicity_ceilings() {
        // (bias, largest monotonic world_max) -- from the derivation in references/tonemap.
        let ceilings = [
            (0.5_f32, 13.6_f32),
            (0.7, 262.0),
            (0.8, 7_625.0),
            (DEFAULT_DRAGO_BIAS, 214_400.0),
            (0.9, 1.673e8),
            (0.95, 7.79e16),
        ];
        for (bias, ceiling) in ceilings {
            let inside = Drago::new(ceiling * 0.99)
                .expect("positive world max")
                .with_bias(bias)
                .expect("bias in (0,1)");
            assert!(
                inside.is_monotonic(),
                "bias {bias} should be monotonic just below its ceiling {ceiling:e}"
            );

            let outside = Drago::new(ceiling * 1.01)
                .expect("positive world max")
                .with_bias(bias)
                .expect("bias in (0,1)");
            assert!(
                !outside.is_monotonic(),
                "bias {bias} should not be monotonic just above its ceiling {ceiling:e}"
            );
        }
    }

    /// The paper's own worked example sits well inside the default configuration's ceiling.
    ///
    /// Drago et al. Figure 5 uses `Lwmax = 230 cd/m²`; `Drago::new` defaults to bias 0.85, whose
    /// ceiling is ~2.1e5. This is the case that matters to a caller who never touches `with_bias`,
    /// and it is why #439 is a documentation defect rather than a numerical one.
    #[test]
    fn drago_default_configuration_is_monotonic_for_a_real_scene() {
        assert!(
            Drago::new(230.0)
                .expect("positive world max")
                .is_monotonic()
        );
        assert!(Drago::new(1e5).expect("positive world max").is_monotonic());
        // And it does run out, which is the whole point of the predicate existing.
        assert!(!Drago::new(1e6).expect("positive world max").is_monotonic());
    }

    /// A white point whose derived divisor cancels to zero is refused, not accepted and broken.
    ///
    /// The named, deterministic form of the second thing the fuzz tier found (#264). At
    /// `white = 3.738137e-9` the subtraction ending `hable_partial` lands on exactly zero, so
    /// every `map` call divided by it: `map(white)` was `0 / 0 = NaN` and every other input
    /// `+∞`. `Hable::new` accepted it, because it was the one parameterised constructor checking
    /// only its argument and not the value the hot loop actually divides by --
    /// `ReinhardExtended::new` has always checked `white²`, and `Drago::new` its prefactor.
    ///
    /// This is not a range, which is why a boundary test would have missed it: the offending
    /// whites are isolated points where f32 rounding happens to cancel exactly. Swept one ULP at
    /// a time across all 55.4 million f32 values in `[1e-9, 1e-7)`, the guard rejects 2 097 151 of
    /// them, leaves no accepted-but-degenerate curve behind, and rejects none whose divisor is
    /// sound.
    #[test]
    fn hable_rejects_a_white_whose_divisor_cancels_to_zero() {
        assert!(Hable::new(3.738137e-9).is_err());

        // The neighbours are fine, so the guard is not a blanket ban on small white points --
        // `Hable::new(1e-30)` is still accepted and still maps `white` to 1.
        let tiny = Hable::new(1e-30).expect("a tiny white with a sound divisor is still valid");
        assert!(close_eps(tiny.map(tiny.white()), 1.0, 1e-5));
        assert!(tiny.map(1.0).is_finite());
    }

    #[test]
    fn drago_rejects_invalid_params() {
        assert!(Drago::new(0.0).is_err());
        assert!(Drago::new(-1.0).is_err());
        assert!(Drago::new(f32::NAN).is_err());
        assert!(Drago::new(f32::INFINITY).is_err());
        // world_max so small that (world_max + 1) rounds to 1.0 in f32, collapsing the log10
        // normalizer to zero (map(0) would be inf · 0 = NaN); f32::MAX stays constructible.
        assert!(Drago::new(1e-8).is_err());
        assert!(Drago::new(f32::MAX).is_ok());
        let c = Drago::new(100.0).expect("valid world max");
        assert!(c.with_bias(0.0).is_err());
        assert!(c.with_bias(1.0).is_err());
        assert!(c.with_bias(-0.1).is_err());
        assert!(c.with_bias(1.5).is_err());
        assert!(c.with_bias(f32::NAN).is_err());
    }
}
