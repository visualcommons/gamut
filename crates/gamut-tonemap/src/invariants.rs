//! Executable laws for the [`ToneCurve`] contract, shared by the property tests and the fuzz tier.
//!
//! Each function takes plain data, checks one law, and returns a [`Violation`] naming what broke
//! it. No test framework is involved and nothing here panics, so the same body runs under a
//! pinned-seed `proptest` in the per-PR gate and under a corpus-guided driver in extended CI
//! (issues #264, #311). A property is the specification a fuzzer checks, and writing it once is
//! what keeps the two lanes from drifting apart.
//!
//! The laws here are the trait's own documented contract, made executable. [`ToneCurve`] states
//! it in prose — "for a finite, non-negative input, every built-in operator returns a
//! non-negative output and **never NaN** … every built-in is monotonic non-decreasing in `x` up
//! to f32 rounding" — and until now nothing quantified over the input domain to check it. The
//! per-operator tests in `operators.rs` pin fixed points and reference values, which is a
//! different and complementary claim: they say *where the curve goes*, these say *what shape it
//! has everywhere*.
//!
//! Inputs are normalised into a bounded, finite, non-negative range by [`normalise_input`], so an
//! arbitrary byte string from a fuzz driver maps to a cheap case. Normalising is the **caller's**
//! job, deliberately: a law that normalised its own inputs would feed the same value to the model
//! and to the code under test, and a defect in the normaliser would cancel on both sides.
//!
//! This module is the test oracle, not the system under test, so `.cargo/mutants.toml` excludes
//! `crates/*/src/invariants.rs` from mutation.

use crate::{CurveToneMapper, ToneCurve, ToneMapper};

/// The largest magnitude a normalised input can take.
///
/// Chosen past the point where the operators' internal arithmetic would overflow if it were
/// written naively — `Aces` documents an `inf/inf → NaN` hazard beyond roughly `1e19`, and
/// `ReinhardExtended` documents a numerator that overflows in the unfactored form — so the domain
/// covers the cases the implementations explicitly guard rather than stopping short of them.
pub const MAX_INPUT: f32 = 1.0e20;

/// Maps arbitrary bits to a finite, non-negative input no larger than [`MAX_INPUT`].
///
/// The mapping is deliberately non-uniform: tone curves are interesting near zero, around `1.0`,
/// and at the extremes, so the exponent is spread across the whole representable range rather
/// than sampling a linear interval that would put essentially every case in the far tail.
#[must_use]
pub fn normalise_input(bits: u32) -> f32 {
    // Low bits pick a mantissa in [1, 2), high bits pick an exponent in [-40, 66].
    let mantissa = 1.0 + f32::from(bits as u16) / f32::from(u16::MAX);
    let exponent = i32::from((bits >> 16) as u16 % 107) - 40;
    let value = mantissa * 2.0_f32.powi(exponent);
    if !value.is_finite() {
        return MAX_INPUT;
    }
    value.clamp(0.0, MAX_INPUT)
}

/// A law that did not hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    law: &'static str,
    detail: String,
}

impl Violation {
    /// Builds a violation of `law`.
    fn new(law: &'static str, detail: String) -> Self {
        Self { law, detail }
    }

    /// The name of the law that was broken.
    #[must_use]
    pub fn law(&self) -> &'static str {
        self.law
    }

    /// What broke it, in terms the failing case can be read from.
    #[must_use]
    pub fn detail(&self) -> &str {
        &self.detail
    }
}

impl core::fmt::Display for Violation {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}: {}", self.law, self.detail)
    }
}

/// Law: for every finite, non-negative input, the output is non-negative and never NaN.
///
/// `+∞` is permitted: the trait's contract allows saturation where the operator's exact value
/// exceeds the f32 range, which `Exposure` with a large gain reaches by design. NaN is not
/// permitted anywhere, and it is the failure these operators actually risk — every one of them
/// divides, and two of them documented a specific `inf/inf` or overflow hazard they guard against.
///
/// # Errors
///
/// Returns the first input whose output is NaN or negative.
pub fn output_is_non_negative_and_never_nan(
    curve: &dyn ToneCurve,
    xs: &[f32],
) -> Result<(), Violation> {
    const LAW: &str = "output_is_non_negative_and_never_nan";

    for &x in xs {
        let y = curve.map(x);
        if y.is_nan() {
            return Err(Violation::new(LAW, format!("map({x:e}) is NaN")));
        }
        if y < 0.0 {
            return Err(Violation::new(LAW, format!("map({x:e}) == {y:e} < 0")));
        }
    }
    Ok(())
}

/// The magnitude the monotonicity tolerance is measured against: how large this curve's outputs
/// get over the domain where it is doing real work.
///
/// **Not** the maximum of the sampled outputs alone, which is the bug the fuzz tier found within
/// its first minute of running (#264). Several operators end in a subtraction that cancels to a
/// residual near `x == 0` — `Hable`'s `- E/F` is the clearest — and if *every* sample lands in
/// that region then the sampled maximum **is** the noise. The tolerance derived from it collapses
/// to about `1e-13`, and the law reports the noise it was written to tolerate:
///
/// ```text
/// monotonic_non_decreasing: map(9.360123e-10) == 2.1779319e-7 but map(3.8715506e-9) == 0e0
/// ```
///
/// A pinned-seed property draws its samples across the whole input range, so an all-tiny set is
/// vanishingly unlikely in 512 cases and none was ever drawn. A coverage-guided engine finds the
/// class in seconds, which is the argument for the tier in one line.
///
/// So the scale is floored by probing the curve over a fixed ladder spanning the meaningful
/// domain. Measured across every built-in and a range of parameters, the worst near-zero
/// backwards step is **0.1 ULP of the curve's range** (`Hable`, at every white point tested;
/// every other operator's is exactly zero), against the `8` ULP the caller allows — 80× of
/// margin, and no real defect is masked: the genuine `Drago` non-monotonicity that #439 records
/// shows drops of `1e-3` to `1e-2`, five decades above this floor.
fn curve_output_scale(curve: &dyn ToneCurve, xs: &[f32]) -> f32 {
    /// Spans the domain an operator is specified over: deep shadow to well past SDR white.
    const LADDER: [f32; 7] = [1e-3, 1e-2, 0.1, 0.5, 1.0, 2.0, 10.0];

    xs.iter()
        .copied()
        .chain(LADDER)
        .map(|x| curve.map(x))
        .filter(|y| y.is_finite())
        .fold(0.0_f32, |acc, y| acc.max(y.abs()))
}

/// Law: the curve never decreases as its input rises.
///
/// `xs` must be sorted ascending and lie within the operator's documented monotonic domain —
/// `Drago` promises this only on `[0, world_max]`, and only for the parameters
/// [`Drago::is_monotonic`](crate::Drago::is_monotonic) accepts, so a caller checking
/// `Drago` supplies samples from that interval and asks that question first.
///
/// The contract allows monotonicity to hold only "up to f32 rounding", and that has to be
/// measured against the **curve's output scale**, not against the local value. Several operators
/// end in a subtraction that cancels to a residual near `x == 0` — `Hable`'s `- E/F` is the
/// clearest — so their outputs there are numerical noise of order `1e-8` while the curve's range
/// is order `1`. A purely relative tolerance collapses to about `1e-15` in that region and reports
/// that noise as a violation, which says nothing about the curve's shape. The tolerance is
/// therefore the larger of four ULPs of the local value and eight ULPs of the curve's output
/// scale — see `curve_output_scale`, and note that the scale is *not* taken from the samples
/// alone, because a sample set drawn entirely from the cancellation region would then measure the
/// noise against itself.
///
/// # Errors
///
/// Returns the first adjacent pair whose outputs decrease by more than that tolerance.
pub fn monotonic_non_decreasing(curve: &dyn ToneCurve, xs: &[f32]) -> Result<(), Violation> {
    const LAW: &str = "monotonic_non_decreasing";

    // The scale the curve actually reaches over these inputs, which is what "f32 rounding" is
    // relative to. Infinities are skipped: a saturated output carries no scale information.
    let scale = curve_output_scale(curve, xs);

    for pair in xs.windows(2) {
        let (lo, hi) = (pair[0], pair[1]);
        let (y_lo, y_hi) = (curve.map(lo), curve.map(hi));
        if y_lo.is_nan() || y_hi.is_nan() {
            // NaN is the other law's business; ordering is not defined against it.
            continue;
        }
        let local = y_lo.abs().max(y_hi.abs()) * 4.0 * f32::EPSILON;
        let tolerance = local.max(scale * 8.0 * f32::EPSILON).max(f32::MIN_POSITIVE);
        if y_hi < y_lo - tolerance {
            return Err(Violation::new(
                LAW,
                format!("map({lo:e}) == {y_lo:e} but map({hi:e}) == {y_hi:e}"),
            ));
        }
    }
    Ok(())
}

/// Law: [`ToneCurve::map_slice`] is exactly elementwise [`ToneCurve::map`].
///
/// The default `map_slice` is *defined* in terms of `map`, so `map` is the reference here rather
/// than a second opinion. The law is what makes an override safe: an implementor that vectorises
/// `map_slice` must not change the result, and the ways to get that wrong — skipping the last
/// element, applying the curve twice, reading a neighbouring index — all show up as a
/// disagreement with the elementwise reference.
///
/// # Errors
///
/// Returns the first index at which the two disagree, or a length mismatch.
pub fn map_slice_is_elementwise_map(curve: &dyn ToneCurve, xs: &[f32]) -> Result<(), Violation> {
    const LAW: &str = "map_slice_is_elementwise_map";

    let expected: Vec<f32> = xs.iter().map(|&x| curve.map(x)).collect();
    let mut actual = xs.to_vec();
    curve.map_slice(&mut actual);

    if actual.len() != expected.len() {
        return Err(Violation::new(
            LAW,
            format!(
                "map_slice changed the length: {} -> {}",
                expected.len(),
                actual.len()
            ),
        ));
    }
    for (index, (&got, &want)) in actual.iter().zip(expected.iter()).enumerate() {
        // Bit equality, not approximate: the two paths must run the identical computation.
        if got.to_bits() != want.to_bits() {
            return Err(Violation::new(
                LAW,
                format!("at index {index}: map_slice gave {got:e}, map gave {want:e}"),
            ));
        }
    }
    Ok(())
}

/// Law: [`ToneCurve::map_slice`] treats each element independently of its neighbours.
///
/// Reversing the input must reverse the output. A `map_slice` that carried state between
/// elements, or indexed relative to the buffer, would agree with
/// [`map_slice_is_elementwise_map`] on one ordering and fail here.
///
/// # Errors
///
/// Returns the first index at which mapping the reversed buffer disagrees with reversing the
/// mapped buffer.
pub fn map_slice_is_order_independent(curve: &dyn ToneCurve, xs: &[f32]) -> Result<(), Violation> {
    const LAW: &str = "map_slice_is_order_independent";

    let mut forward = xs.to_vec();
    curve.map_slice(&mut forward);
    forward.reverse();

    let mut reversed: Vec<f32> = xs.iter().rev().copied().collect();
    curve.map_slice(&mut reversed);

    for (index, (&got, &want)) in reversed.iter().zip(forward.iter()).enumerate() {
        if got.to_bits() != want.to_bits() {
            return Err(Violation::new(
                LAW,
                format!("at reversed index {index}: {got:e} != {want:e}"),
            ));
        }
    }
    Ok(())
}

// ---- the `ToneMapper` contract ---------------------------------------------------------------
//
// The same discipline for the RGB-aware trait (#1236): `ToneMapper`'s documented contract, made
// executable. The ffmpeg golden test (`tests/ffmpeg_desat_golden.rs`) pins *where* max-RGB mode
// goes against a published formula; these pin what holds in every mode, at every parameter.

/// Law: for every pixel of finite, non-negative channels, every output channel is non-negative
/// and never NaN.
///
/// `+∞` is permitted where the curve itself saturates, as for [`ToneCurve`]. NaN is the hazard
/// this law exists for: the mapper divides by the pixel's luma and by its driving signal, and
/// multiplies a gain into channels that may be zero.
///
/// # Errors
///
/// Returns the first pixel with a NaN or negative output channel.
pub fn rgb_output_is_non_negative_and_never_nan(
    mapper: &dyn ToneMapper,
    pixels: &[[f32; 3]],
) -> Result<(), Violation> {
    const LAW: &str = "rgb_output_is_non_negative_and_never_nan";

    for &pixel in pixels {
        let out = mapper.map_rgb(pixel);
        if out.iter().any(|c| c.is_nan() || *c < 0.0) {
            return Err(Violation::new(
                LAW,
                format!("map_rgb({pixel:?}) == {out:?}"),
            ));
        }
    }
    Ok(())
}

/// Law: [`ToneMapper::map_rgb_slice`] is exactly pixelwise [`ToneMapper::map_rgb`], bit for bit.
///
/// The pixel-level twin of [`map_slice_is_elementwise_map`], for the same reason: it is what makes
/// a vectorised override safe.
///
/// # Errors
///
/// Returns the first pixel at which the two disagree.
pub fn map_rgb_slice_is_pixelwise_map_rgb(
    mapper: &dyn ToneMapper,
    pixels: &[[f32; 3]],
) -> Result<(), Violation> {
    const LAW: &str = "map_rgb_slice_is_pixelwise_map_rgb";

    let mut actual = pixels.to_vec();
    mapper.map_rgb_slice(&mut actual);
    for (index, (&pixel, got)) in pixels.iter().zip(&actual).enumerate() {
        let want = mapper.map_rgb(pixel);
        if got.map(f32::to_bits) != want.map(f32::to_bits) {
            return Err(Violation::new(
                LAW,
                format!("at pixel {index}: map_rgb_slice gave {got:?}, map_rgb gave {want:?}"),
            ));
        }
    }
    Ok(())
}

/// Law: a mapper that scales every channel by one gain keeps the pixel's channel ratios — its hue
/// and saturation.
///
/// This is what [`ApplyMode::Luminance`](crate::ApplyMode::Luminance) and
/// [`ApplyMode::MaxRgb`](crate::ApplyMode::MaxRgb) promise over
/// [`ApplyMode::PerChannel`](crate::ApplyMode::PerChannel), so the caller checks it only for those
/// modes, with desaturation off (desaturation changes the ratios by design). A zero channel must
/// stay zero, and every channel whose output is a normal, finite `f32` must imply the same gain
/// `out / in` to within four ULPs: one rounding on the way out, one in the quotient. Subnormal
/// and saturated outputs have no relative digits to compare and are skipped.
///
/// # Errors
///
/// Returns the first pixel whose channels were scaled by different gains.
pub fn common_gain_preserves_channel_ratios(
    mapper: &dyn ToneMapper,
    pixels: &[[f32; 3]],
) -> Result<(), Violation> {
    const LAW: &str = "common_gain_preserves_channel_ratios";

    for &pixel in pixels {
        let out = mapper.map_rgb(pixel);
        let mut gains = Vec::with_capacity(3);
        for (&c, &o) in pixel.iter().zip(&out) {
            if c == 0.0 {
                if o != 0.0 {
                    return Err(Violation::new(
                        LAW,
                        format!("map_rgb({pixel:?}) == {out:?}: a zero channel moved"),
                    ));
                }
            } else if o.is_normal() {
                gains.push(o / c);
            }
        }
        let lo = gains.iter().copied().fold(f32::INFINITY, f32::min);
        let hi = gains.iter().copied().fold(0.0_f32, f32::max);
        if hi - lo > hi * 4.0 * f32::EPSILON {
            return Err(Violation::new(
                LAW,
                format!("map_rgb({pixel:?}) == {out:?}: gains {gains:?} differ"),
            ));
        }
    }
    Ok(())
}

/// Law: a grey pixel at the source peak maps to the target peak in every channel, in every mode
/// and at every desaturation strength.
///
/// The normalization's defining property. A grey pixel's luma and brightest channel are both its
/// value, and desaturating a grey is a no-op, so whatever the mode, the curve sees the source peak
/// and the normalization must return the target peak — up to the eight ULPs the luma sum, the mix
/// and the gain round through.
///
/// "Sees the source peak" holds to an ULP: the `f32` luma of a grey can differ from its value by
/// one rounding. So the caller holds the law only at source peaks where the curve is well
/// conditioned, which for the built-ins is `source_peak ≥ 1`. Below that, `Hable`'s `partial` is a
/// cancelling difference, and a one-ULP step in its input moves its output by more than eight
/// ULPs.
///
/// # Errors
///
/// Returns the channel that missed the target peak.
pub fn source_peak_grey_maps_to_target_peak<C: ToneCurve>(
    mapper: &CurveToneMapper<C>,
) -> Result<(), Violation> {
    const LAW: &str = "source_peak_grey_maps_to_target_peak";

    let peak = mapper.source_peak();
    let target = mapper.target_peak();
    let out = mapper.map_rgb([peak; 3]);
    if out
        .iter()
        .any(|c| (c - target).abs() > target * 8.0 * f32::EPSILON)
    {
        return Err(Violation::new(
            LAW,
            format!("map_rgb([{peak:e}; 3]) == {out:?}, want {target:e}"),
        ));
    }
    Ok(())
}

// The laws' own proptest drivers and failure-path tests, in a child file so this module stays
// under the workspace file-length limit (#1258). `#[cfg(test)]`, so cargo-mutants skips it.
#[cfg(test)]
mod tests;
