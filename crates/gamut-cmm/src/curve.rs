//! 1-D tone curves: evaluation and inversion of the curves ICC profiles carry.
//!
//! [`ToneCurve`] wraps a parsed [`gamut_icc::CurveOrParametric`] — a `curveType` or
//! `parametricCurveType` element (ICC.1:2022 §10.6/§10.18) — and adds what a CMM needs beyond
//! parsing: forward evaluation over `[0, 1]`, monotonicity detection, and functional inversion
//! (analytic closed forms where the parameterization permits, an lcms2-shaped numeric table
//! reversal otherwise). [`Stage::Curves`](crate::Stage::Curves) applies one [`ToneCurve`] per
//! channel inside a [`Pipeline`](crate::Pipeline).

use gamut_color::cicp::TransferCharacteristics;
use gamut_color::transfer::{standard_eotf_for, standard_oetf_for};
use gamut_icc::{Curve, CurveOrParametric};

use crate::error::{CmmError, Result};

/// Entry count of a numerically reversed curve table — lcms2's `cmsReverseToneCurve` default
/// (`cmsgamma.c`), so numeric inverses resolve identically to the oracle's.
const INVERSE_SAMPLES: usize = 4096;

/// Probe count for [`ToneCurve::is_monotonic`]'s dense sampling — matched to
/// [`INVERSE_SAMPLES`] so monotonicity is judged at exactly the resolution the numeric inverse
/// resolves.
const MONOTONICITY_PROBES: usize = 4096;

/// Sample count of a curve table built by [`ToneCurve::joined`] — lcms2's
/// `PRELINEARIZATION_POINTS` (`cmsopt.c:418`), the resolution its own curve-joining
/// optimization tabulates at, and the same 4096 this crate already reverses curves at
/// ([`INVERSE_SAMPLES`]).
const JOIN_POINTS: usize = 4096;

/// The internal representation of a [`ToneCurve`].
#[derive(Debug, Clone)]
enum Repr {
    /// A forward curve as parsed: evaluation delegates to `gamut-icc` (never resampled).
    Icc(CurveOrParametric),
    /// `y = x^exponent` with a full-precision `f64` exponent — the analytic inverse of a
    /// pure-gamma curve. Deliberately *not* re-encoded through `u8Fixed8`/`s15Fixed16`, which
    /// would quantize the reciprocal exponent.
    Power {
        /// The exponent applied to the clamped input.
        exponent: f64,
    },
    /// The closed-form inverse of a parametric type 1–4 curve, evaluating lcms2's negated-type
    /// formulas over the *forward* curve's parameters.
    InverseParametric {
        /// The forward curve's ICC function type (1–4).
        function_type: u16,
        /// The forward curve's parameters `[g, a, b, c, d, e, f]` (absent ones defaulted as in
        /// `gamut_icc::ParametricCurve::eval`: `g = a = 1`, the rest `0`).
        params: [f64; 7],
    },
    /// A numerically reversed curve: entry `i` holds the `x` with `f(x) ≈ i / (len − 1)`,
    /// evaluated by linear interpolation.
    SampledInverse {
        /// The [`INVERSE_SAMPLES`]-entry inverse table, values in `[0, 1]`.
        table: Vec<f64>,
    },
    /// A curve tabulated from computed samples at uniform positions, evaluated by linear
    /// interpolation — what [`ToneCurve::joined`] builds when it collapses a curve pair.
    /// Distinct from [`SampledInverse`](Repr::SampledInverse), which shares the evaluation
    /// but carries the inverse-specific construction contract.
    Table {
        /// The uniformly spaced sample table (≥ 2 entries), values in `[0, 1]`.
        table: Vec<f64>,
    },
    /// A transfer function evaluated in closed form — an ITU-T H.273 code point's curve from
    /// `gamut-color`, built by [`ToneCurve::cicp_transfer`]. Carries its own inverse, so
    /// inverting it is a swap: exact in both directions, with no table in either.
    Closed {
        /// The curve this representation evaluates.
        forward: fn(f64) -> f64,
        /// Its exact inverse.
        inverse: fn(f64) -> f64,
    },
}

/// The largest absolute difference, anywhere on [`ToneCurve::agrees_with`]'s probe grid, at
/// which a profile's tone curve still counts as an encoding of the transfer function its `cicp`
/// tag names.
///
/// `2⁻⁸` sits between the two populations it separates. Below it: every *encoding* of a named
/// curve — measured, the BT.2100 PQ built-in's 1024-entry `curveType` misses ST 2084 by 1.5e-5,
/// the `parametricCurveType` encodings of sRGB and BT.709 by 4.2e-6 and 1.3e-6, and even a
/// 16-entry sampled sRGB table by 1.7e-3. Above it: a *different* curve — the nearest
/// confusions are pure gamma 2.2 and Adobe RGB's `x^(563/256)` labelled as sRGB, 8.5e-3 and
/// 8.6e-3 away.
const CICP_AGREEMENT: f64 = 1.0 / 256.0;

/// A one-dimensional tone curve: a monotonicity-aware, invertible view of a parsed ICC curve.
///
/// Built from a [`gamut_icc::CurveOrParametric`] by [`ToneCurve::new`]; evaluated by
/// [`ToneCurve::eval`]; inverted by [`ToneCurve::inverse`], which yields another `ToneCurve`.
/// Both evaluation directions clamp domain **and** range to `[0, 1]` (the convention of
/// `gamut_icc::Curve::eval`, extended to parametric curves whose raw closed forms can leave the
/// range).
///
/// # Example
///
/// ```
/// use gamut_cmm::ToneCurve;
/// use gamut_icc::{Curve, CurveOrParametric, U8Fixed8};
///
/// // A pure-gamma 2.0 curve (u8Fixed8 0x0200 is exactly 2.0).
/// let gamma = ToneCurve::new(&CurveOrParametric::Curve(Curve::Gamma(U8Fixed8(0x0200))))?;
/// assert_eq!(gamma.eval(0.5), 0.25);
/// let inverse = gamma.inverse()?;
/// assert!((inverse.eval(0.25) - 0.5).abs() < 1e-12);
/// # Ok::<(), gamut_cmm::CmmError>(())
/// ```
#[derive(Debug, Clone)]
pub struct ToneCurve {
    repr: Repr,
}

impl ToneCurve {
    /// Wraps a parsed curve for evaluation and inversion.
    ///
    /// # Errors
    ///
    /// [`CmmError::UnsupportedParametricType`] for a parametric `function_type > 4`.
    /// ICC.1:2022 §10.18 defines exactly the types 0–4 and `gamut-icc`'s *parser* rejects
    /// anything else, but a hand-built [`gamut_icc::ParametricCurve`] can carry any type — and
    /// its `eval` silently treats unknown types as the identity, a trap this constructor turns
    /// into a typed error.
    pub fn new(curve: &CurveOrParametric) -> Result<Self> {
        if let CurveOrParametric::Parametric(parametric) = curve
            && parametric.function_type > 4
        {
            return Err(CmmError::UnsupportedParametricType(
                parametric.function_type,
            ));
        }
        Ok(Self {
            repr: Repr::Icc(curve.clone()),
        })
    }

    /// Evaluates the curve at `x`, clamping the input and the output to `[0, 1]`.
    ///
    /// Forward curves delegate to `gamut-icc`'s evaluators exactly (no resampling); inverse
    /// curves evaluate their analytic closed form, or linearly interpolate their
    /// numerically reversed table.
    #[must_use]
    pub fn eval(&self, x: f64) -> f64 {
        let x = x.clamp(0.0, 1.0);
        let y = match &self.repr {
            Repr::Icc(curve) => curve.eval(x),
            Repr::Power { exponent } => x.powf(*exponent),
            Repr::InverseParametric {
                function_type,
                params,
            } => eval_inverse_parametric(*function_type, params, x),
            Repr::SampledInverse { table } | Repr::Table { table } => interpolate(table, x),
            Repr::Closed { forward, .. } => forward(x),
        };
        y.clamp(0.0, 1.0)
    }

    /// Whether the curve is monotonic: non-decreasing **or** non-increasing over its domain.
    ///
    /// Sampled tables are checked on their exact entries; every other representation is probed
    /// at 4096 evenly spaced points (a wiggle confined strictly between adjacent probes is below
    /// this CMM's resolution — the same posture as lcms2, whose `cmsIsToneCurveMonotonic`
    /// inspects a shadow table of the same size). A constant curve is monotonic under this
    /// definition (it is non-decreasing and non-increasing at once), but [`ToneCurve::inverse`]
    /// still rejects it.
    ///
    /// ```
    /// use gamut_cmm::ToneCurve;
    /// use gamut_icc::{Curve, CurveOrParametric};
    ///
    /// let up = ToneCurve::new(&CurveOrParametric::Curve(Curve::Sampled(vec![0, 30000, 65535])))?;
    /// assert!(up.is_monotonic());
    /// let wiggle =
    ///     ToneCurve::new(&CurveOrParametric::Curve(Curve::Sampled(vec![0, 40000, 20000, 65535])))?;
    /// assert!(!wiggle.is_monotonic());
    /// # Ok::<(), gamut_cmm::CmmError>(())
    /// ```
    #[must_use]
    pub fn is_monotonic(&self) -> bool {
        match &self.repr {
            Repr::Icc(CurveOrParametric::Curve(Curve::Sampled(table))) => {
                samples_monotonic(table.iter().map(|&v| f64::from(v)))
            }
            Repr::SampledInverse { table } | Repr::Table { table } => {
                samples_monotonic(table.iter().copied())
            }
            _ => samples_monotonic(probes().map(|x| self.eval(x))),
        }
    }

    /// The functional inverse: a curve `g` with `g(self(x)) ≈ x` wherever `self` is strictly
    /// monotonic (over flat runs the inverse picks one preimage; see below).
    ///
    /// Analytic closed forms are used where the parameterization permits: identity → identity,
    /// pure gamma `g` → `x^(1/g)` (with a full-precision `f64` exponent, **not** re-encoded
    /// through `u8Fixed8` — the reciprocal rarely stays representable), and parametric types
    /// 1–4 with `g > 0`, `a > 0` (plus `c > 0` and `d ∈ [0, 1]` for types 3–4) → the
    /// corresponding lcms2 negated-type formulas. Degenerate parameterizations that are still
    /// monotonic (e.g. `a < 0`, `c ≤ 0`) fall back to the numeric path below, as does inverting
    /// an already-inverted non-gamma curve.
    ///
    /// Sampled tables (and the fallbacks above) are reversed numerically into a 4096-entry
    /// table, shaped after lcms2's `cmsReverseToneCurveEx`:
    ///
    /// - the interval scan runs from the **high** end downward for ascending tables and from the
    ///   low end upward for descending ones, which fixes the preimage choice when a value
    ///   recurs;
    /// - **flat runs**: the flat value maps to the run edge adjoining the curve's *larger*
    ///   values (lcms2's `y2`-for-ascending / `y1`-for-descending convention);
    /// - a target value outside the curve's range clamps to the domain end on that side. lcms2's
    ///   coefficient-carrying quirk agrees except below a descending table's minimum, where it
    ///   emits `0` instead of the correct `1` — this implementation keeps the correct
    ///   pseudo-inverse. (The quirk itself needs no replicating: for a monotonic table the
    ///   segments tile the curve's full range, so every in-range target is bracketed and
    ///   out-of-range targets are the only no-interval case.)
    ///
    /// # Errors
    ///
    /// [`CmmError::NonMonotonicCurve`] if the curve is not monotonic per
    /// [`is_monotonic`](Self::is_monotonic), or is **constant** (equal endpoints — including
    /// one-entry tables and parameterizations that collapse to a constant after `[0, 1]` range
    /// clamping): a constant curve has no functional inverse.
    pub fn inverse(&self) -> Result<ToneCurve> {
        if !self.is_monotonic() {
            return Err(CmmError::NonMonotonicCurve);
        }
        // A monotonic curve is constant iff its endpoints agree — and a constant has no inverse.
        if self.eval(0.0) == self.eval(1.0) {
            return Err(CmmError::NonMonotonicCurve);
        }
        let repr = match &self.repr {
            Repr::Icc(CurveOrParametric::Curve(Curve::Identity)) => {
                Repr::Icc(CurveOrParametric::Curve(Curve::Identity))
            }
            // The endpoint check above rejected g == 0 (a constant curve), so 1/g is finite.
            Repr::Icc(CurveOrParametric::Curve(Curve::Gamma(g))) => Repr::Power {
                exponent: 1.0 / g.to_f64(),
            },
            Repr::Power { exponent } => Repr::Power {
                exponent: 1.0 / exponent,
            },
            // An empty table evaluates as the identity (see `gamut_icc::Curve::eval`); a
            // one-entry table is constant and was rejected above, so `reverse_table` always
            // sees at least two entries.
            Repr::Icc(CurveOrParametric::Curve(Curve::Sampled(table))) => {
                if table.is_empty() {
                    Repr::Icc(CurveOrParametric::Curve(Curve::Identity))
                } else {
                    let forward: Vec<f64> = table.iter().map(|&v| f64::from(v) / 65535.0).collect();
                    Repr::SampledInverse {
                        table: reverse_table(&forward),
                    }
                }
            }
            Repr::Icc(CurveOrParametric::Parametric(parametric)) => {
                let params = expand_params(&parametric.params);
                parametric_inverse_repr(parametric.function_type, &params, self)
            }
            Repr::InverseParametric { .. } | Repr::SampledInverse { .. } | Repr::Table { .. } => {
                numeric_inverse_repr(self)
            }
            Repr::Closed { forward, inverse } => Repr::Closed {
                forward: *inverse,
                inverse: *forward,
            },
        };
        Ok(ToneCurve { repr })
    }

    /// The transfer function ITU-T H.273 assigns to `TransferCharacteristics` code point
    /// `code_point`, evaluated in closed form — the curve a profile's `cicp` tag (ICC.1:2022
    /// §10.3) names for its own encoding. `None` where `gamut-color` evaluates no curve for the
    /// code point ([`gamut_color::transfer::standard_eotf_for`] declines it).
    ///
    /// Linear (code point 8) is returned as the structural identity rather than as a closed
    /// form of `x ↦ x`: the two evaluate identically, and only the structural one is elided
    /// by [`crate::optimize`] ([`is_identity`](Self::is_identity)).
    pub(crate) fn cicp_transfer(code_point: u8) -> Option<ToneCurve> {
        let tc = TransferCharacteristics::from_code_point(u16::from(code_point))?;
        if tc == TransferCharacteristics::Linear {
            return Some(ToneCurve {
                repr: Repr::Icc(CurveOrParametric::Curve(Curve::Identity)),
            });
        }
        let (forward, inverse) = (standard_eotf_for(tc)?, standard_oetf_for(tc)?);
        Some(ToneCurve {
            repr: Repr::Closed { forward, inverse },
        })
    }

    /// Whether `self` and `other` evaluate within [`CICP_AGREEMENT`] of each other at every
    /// point of the probe grid [`is_monotonic`](Self::is_monotonic) samples — the test of
    /// ICC.1:2022 §10.3's requirement that a `cicp` tag name an encoding *equivalent* to the
    /// profile's own, applied before the tag's closed form may stand in for the profile's curve.
    pub(crate) fn agrees_with(&self, other: &ToneCurve) -> bool {
        probes().all(|x| (self.eval(x) - other.eval(x)).abs() <= CICP_AGREEMENT)
    }

    /// Whether this curve is the ICC identity element (`curveType` with zero entries — the
    /// `Curve::Identity` `gamut-icc` parses a count-0 curve into).
    ///
    /// Deliberately narrow: a gamma-1.0 or a ramp-shaped sampled table also *evaluates* as
    /// the identity, but only the structural identity is recognised, so the answer never
    /// depends on floating-point luck. Used by [`joined`](Self::joined) to keep the
    /// identity leg of a curve join exact.
    pub(crate) fn is_identity(&self) -> bool {
        matches!(
            self.repr,
            Repr::Icc(CurveOrParametric::Curve(Curve::Identity))
        )
    }

    /// The composition `second(first(x))` as one curve — the two-curve collapse
    /// [`crate::optimize`] applies to an adjacent [`Stage::Curves`](crate::Stage::Curves)
    /// pair.
    ///
    /// Either leg being the structural identity ([`is_identity`](Self::is_identity)) returns
    /// the *other* curve unchanged, which is exact: both legs clamp their input and their
    /// output to `[0, 1]`, so composing with a clamp-only identity cannot move a sample.
    /// Otherwise the composition is tabulated at [`JOIN_POINTS`] uniform positions and
    /// evaluated by linear interpolation — **lossy**, exactly as lcms2's own curve joining
    /// is (`cmsopt.c`'s `OptimizeByJoiningCurves` tabulates at the same 4096 points). The
    /// error is the sampled table's chord error, which is largest where the composition is
    /// steepest: an inverse-gamma toe near black is the worst case (see the crate's
    /// `STATUS.md` precision budget).
    #[must_use]
    pub(crate) fn joined(first: &ToneCurve, second: &ToneCurve) -> ToneCurve {
        if first.is_identity() {
            return second.clone();
        }
        if second.is_identity() {
            return first.clone();
        }
        let last = (JOIN_POINTS - 1) as f64;
        let table = (0..JOIN_POINTS)
            .map(|i| second.eval(first.eval(i as f64 / last)))
            .collect();
        ToneCurve {
            repr: Repr::Table { table },
        }
    }
}

/// Expands a parametric parameter list to the full `[g, a, b, c, d, e, f]`, defaulting absent
/// entries exactly as `gamut_icc::ParametricCurve::eval` does (`g = a = 1`, the rest `0`).
fn expand_params(params: &[gamut_icc::S15Fixed16]) -> [f64; 7] {
    let p = |i: usize, default: f64| params.get(i).map_or(default, |v| v.to_f64());
    [
        p(0, 1.0),
        p(1, 1.0),
        p(2, 0.0),
        p(3, 0.0),
        p(4, 0.0),
        p(5, 0.0),
        p(6, 0.0),
    ]
}

/// Picks the inverse representation for a parametric curve: the analytic closed form when the
/// parameterization is well-behaved, the numeric reversal otherwise.
///
/// The analytic conditions are *sufficient*, not necessary: `g > 0` and `a > 0` keep the power
/// branch strictly increasing and its inverse well-defined; types 3–4 additionally need a
/// strictly increasing linear toe (`c > 0`) over a real split point (`d ∈ [0, 1]`). Everything
/// else — already vetted as monotonic and non-constant by the caller — reverses numerically.
fn parametric_inverse_repr(function_type: u16, params: &[f64; 7], forward: &ToneCurve) -> Repr {
    let [g, a, _b, c, d, _e, _f] = *params;
    match function_type {
        // No guard needed: with g ≤ 0 the range-clamped x^g is constant (`powf` maps every
        // base in [0, 1] to a value ≥ 1, and 0^0 == 1), which the caller already rejected.
        0 => Repr::Power { exponent: 1.0 / g },
        1..=4
            if g > 0.0
                && a > 0.0
                && (function_type < 3 || (c > 0.0 && (0.0..=1.0).contains(&d))) =>
        {
            Repr::InverseParametric {
                function_type,
                params: *params,
            }
        }
        _ => numeric_inverse_repr(forward),
    }
}

/// Numerically inverts `forward` by sampling it at [`INVERSE_SAMPLES`] points and reversing the
/// sampled table.
fn numeric_inverse_repr(forward: &ToneCurve) -> Repr {
    let samples: Vec<f64> = (0..INVERSE_SAMPLES)
        .map(|i| forward.eval(i as f64 / (INVERSE_SAMPLES - 1) as f64))
        .collect();
    Repr::SampledInverse {
        table: reverse_table(&samples),
    }
}

/// Evaluates the closed-form inverse of parametric type `function_type` (1–4) at `y ∈ [0, 1]`,
/// over the *forward* parameters `[g, a, b, c, d, e, f]`.
///
/// These are transcriptions of lcms2's negated-type evaluators (`DefaultEvalParametricFn`,
/// cases `-2`/`-3`/`-4`/`-5` — lcms2 numbers types ICC + 1), including their threshold
/// conventions: type 2's inverse returns `0` (not `-b/a`) exactly at `y == c`, and type 4's
/// split point is the *linear* branch value `c·d + f` where type 3's is the *power* branch
/// value `(a·d + b)^g`. Range clamping is left to the caller ([`ToneCurve::eval`]).
fn eval_inverse_parametric(function_type: u16, params: &[f64; 7], y: f64) -> f64 {
    let [g, a, b, c, d, e, f] = *params;
    match function_type {
        // Forward: Y = (aX + b)^g. Inverse: X = (Y^(1/g) − b) / a.
        1 => (y.powf(1.0 / g) - b) / a,
        // Forward: Y = (aX + b)^g + c. Inverse: X = ((Y − c)^(1/g) − b) / a for Y > c,
        // 0 at Y == c (lcms2's convention), −b/a below.
        2 => {
            if y >= c {
                let t = y - c;
                if t > 0.0 {
                    (t.powf(1.0 / g) - b) / a
                } else {
                    0.0
                }
            } else {
                -b / a
            }
        }
        // Forward: Y = (aX + b)^g for X ≥ d, else cX. Split at the power-branch value
        // (a·d + b)^g — clamping the base realizes lcms2's 0-for-negative-base rule, since
        // 0^g == 0 for the g > 0 this repr guarantees.
        3 => {
            let disc = (a * d + b).max(0.0).powf(g);
            if y >= disc {
                (y.powf(1.0 / g) - b) / a
            } else {
                y / c
            }
        }
        // Forward: Y = (aX + b)^g + e for X ≥ d, else cX + f. Split at the linear-branch
        // value c·d + f (lcms2's -5 convention, unlike -4's power-branch split).
        4 => {
            let disc = c * d + f;
            if y >= disc {
                let t = y - e;
                if t < 0.0 {
                    0.0
                } else {
                    (t.powf(1.0 / g) - b) / a
                }
            } else {
                (y - f) / c
            }
        }
        // Unreachable: `parametric_inverse_repr` only builds this repr for types 1–4.
        _ => y,
    }
}

/// The [`MONOTONICITY_PROBES`] evenly spaced points of `[0, 1]`, both ends included — the grid
/// [`ToneCurve::is_monotonic`] and [`ToneCurve::agrees_with`] judge a curve on.
fn probes() -> impl Iterator<Item = f64> {
    (0..MONOTONICITY_PROBES).map(|i| i as f64 / (MONOTONICITY_PROBES - 1) as f64)
}

/// Linearly interpolates a uniformly spaced `f64` table (≥ 2 entries) at `x ∈ [0, 1]`.
fn interpolate(table: &[f64], x: f64) -> f64 {
    let last = table.len() - 1;
    let pos = x * last as f64;
    let lower = pos.floor() as usize;
    if lower >= last {
        return table[last];
    }
    let frac = pos - lower as f64;
    table[lower] + (table[lower + 1] - table[lower]) * frac
}

/// Whether a sample sequence is monotonic: never both rising and falling.
fn samples_monotonic(samples: impl IntoIterator<Item = f64>) -> bool {
    let mut iter = samples.into_iter();
    let Some(mut prev) = iter.next() else {
        return true;
    };
    let (mut rose, mut fell) = (false, false);
    for v in iter {
        if v > prev {
            rose = true;
        }
        if v < prev {
            fell = true;
        }
        prev = v;
    }
    !(rose && fell)
}

/// Numerically reverses a monotonic, non-constant sampled curve into an
/// [`INVERSE_SAMPLES`]-entry table — the shape of lcms2's `cmsReverseToneCurveEx`.
///
/// `table` holds ≥ 2 forward samples in `[0, 1]` at uniform positions; entry `i` of the result
/// is the `x ∈ [0, 1]` whose forward value is `i / (INVERSE_SAMPLES − 1)`. See
/// [`ToneCurve::inverse`] for the scan-direction, flat-run, and out-of-range conventions.
fn reverse_table(table: &[f64]) -> Vec<f64> {
    let last = table.len() - 1;
    let ascending = table[0] <= table[last];
    let coord = |j: usize| j as f64 / last as f64;
    let mut out = Vec::with_capacity(INVERSE_SAMPLES);
    for i in 0..INVERSE_SAMPLES {
        let y = i as f64 / (INVERSE_SAMPLES - 1) as f64;
        // Bracket `y`: ascending tables scan from the high end downward, descending tables from
        // the low end upward (lcms2 `GetInterval`), fixing which preimage a repeated value gets.
        let interval = if ascending {
            (0..last)
                .rev()
                .find(|&j| table[j] <= y && y <= table[j + 1])
        } else {
            (0..last).find(|&j| table[j + 1] <= y && y <= table[j])
        };
        let x = match interval {
            Some(j) => {
                let (x1, x2) = (table[j], table[j + 1]);
                if x1 == x2 {
                    // Flat segment: the run edge adjoining the curve's larger values —
                    // lcms2's y2-for-ascending / y1-for-descending choice.
                    if ascending { coord(j + 1) } else { coord(j) }
                } else {
                    // lcms2's interpolation form: x = slope·y + (y2 − slope·x2).
                    let slope = (coord(j + 1) - coord(j)) / (x2 - x1);
                    slope * y + (coord(j + 1) - slope * x2)
                }
            }
            // `y` outside the curve's range: clamp to the domain end on that side (see
            // `ToneCurve::inverse` for the one deliberate deviation from lcms2 this implies).
            None => {
                if ascending {
                    if y < table[0] { 0.0 } else { 1.0 }
                } else if y > table[0] {
                    0.0
                } else {
                    1.0
                }
            }
        };
        out.push(x.clamp(0.0, 1.0));
    }
    out
}

#[cfg(test)]
mod tests {
    use gamut_icc::{ParametricCurve, S15Fixed16, U8Fixed8};

    use super::*;

    fn s15(v: f64) -> S15Fixed16 {
        S15Fixed16::from_f64(v)
    }

    fn parametric(function_type: u16, params: &[f64]) -> ToneCurve {
        ToneCurve::new(&CurveOrParametric::Parametric(ParametricCurve {
            function_type,
            params: params.iter().copied().map(s15).collect(),
        }))
        .unwrap()
    }

    fn sampled(table: Vec<u16>) -> ToneCurve {
        ToneCurve::new(&CurveOrParametric::Curve(Curve::Sampled(table))).unwrap()
    }

    #[test]
    fn samples_monotonic_covers_all_shapes() {
        // Strictly rising / falling, with and without flats: monotonic.
        assert!(samples_monotonic([0.0, 0.5, 1.0]));
        assert!(samples_monotonic([1.0, 0.5, 0.0]));
        assert!(samples_monotonic([0.0, 0.5, 0.5, 1.0]));
        assert!(samples_monotonic([1.0, 0.5, 0.5, 0.0]));
        // Constant and trivial sequences: monotonic.
        assert!(samples_monotonic([0.5, 0.5, 0.5]));
        assert!(samples_monotonic([0.5]));
        assert!(samples_monotonic([]));
        // Rise-then-fall and fall-then-rise: not monotonic.
        assert!(!samples_monotonic([0.0, 0.6, 0.3, 1.0]));
        assert!(!samples_monotonic([1.0, 0.3, 0.6, 0.0]));
    }

    #[test]
    fn reverse_table_of_identity_ramp_is_the_identity_grid() {
        let inv = reverse_table(&[0.0, 1.0]);
        assert_eq!(inv.len(), INVERSE_SAMPLES);
        // slope = 1, offset = 0 ⇒ every entry equals its own grid position, exactly.
        for (i, &x) in inv.iter().enumerate() {
            assert_eq!(x, i as f64 / (INVERSE_SAMPLES - 1) as f64, "entry {i}");
        }
    }

    #[test]
    fn reverse_table_interpolates_with_the_lcms2_form() {
        // Table [0, 1/4, 1] over x ∈ {0, 1/2, 1}. For y = 1/2 the bracketing segment is
        // j = 1: slope = (1 − 1/2)/(1 − 1/4) = 2/3, x = 2/3·y + (1 − 2/3) = 2/3.
        let inv = reverse_table(&[0.0, 0.25, 1.0]);
        let i = (INVERSE_SAMPLES - 1) / 2 + 1; // odd grid: y = 2048/4095, not exactly 1/2
        let y = i as f64 / (INVERSE_SAMPLES - 1) as f64;
        let slope = 0.5 / 0.75;
        assert_eq!(inv[i], slope * y + (1.0 - slope));
        // y = 1/4 sits on the seam of segments 0 and 1; both interpolate to x = 1/2, so the
        // scan direction is unobservable here (flat-run tests below pin the direction).
        let quarter = interpolate(&inv, 0.25);
        assert!((quarter - 0.5).abs() < 3e-4, "got {quarter}");
    }

    #[test]
    fn reverse_table_flat_run_maps_to_edge_adjoining_larger_values() {
        // Ascending with an interior flat at 1/2 (indices 1..=2 of 4): the scan from the high
        // end finds segment j = 2 first, whose interpolation lands on the run's right edge 2/3.
        let inv = reverse_table(&[0.0, 0.5, 0.5, 1.0]);
        let mid = INVERSE_SAMPLES / 2; // y = 2048/4095, a hair above 1/2 ⇒ still segment j = 2
        assert!(
            (inv[mid] - 2.0 / 3.0).abs() < 1e-3,
            "ascending flat: {}",
            inv[mid]
        );
        // Descending with the same flat: the scan from the low end lands on the run's left
        // edge 1/3 — which likewise adjoins the larger values.
        let inv = reverse_table(&[1.0, 0.5, 0.5, 0.0]);
        assert!(
            (inv[mid] - 1.0 / 3.0).abs() < 1e-3,
            "descending flat: {}",
            inv[mid]
        );
    }

    #[test]
    fn reverse_table_flat_segment_arm_picks_the_documented_edge() {
        // A flat segment at the very top is bracketed by the flat arm itself (no non-flat
        // segment above it catches the value first). Ascending: y = 1 → the segment's right
        // edge 3/3 = 1, NOT the left edge 2/3.
        let inv = reverse_table(&[0.0, 0.5, 1.0, 1.0]);
        assert_eq!(inv[INVERSE_SAMPLES - 1], 1.0);
        // Descending: the flat run sits at indices 0..=1; y = 1 → left edge 0/3 = 0, NOT 1/3.
        let inv = reverse_table(&[1.0, 1.0, 0.5, 0.0]);
        assert_eq!(inv[INVERSE_SAMPLES - 1], 0.0);
    }

    #[test]
    fn reverse_table_out_of_range_targets_clamp_to_domain_ends() {
        // Ascending table spanning [1/4, 3/4]: y below the range → 0, above → 1.
        let inv = reverse_table(&[0.25, 0.75]);
        assert_eq!(inv[0], 0.0);
        assert_eq!(inv[INVERSE_SAMPLES - 1], 1.0);
        // Descending table spanning the same range: y below → 1 (the deliberate deviation from
        // lcms2's carried-coefficient 0), y above → 0.
        let inv = reverse_table(&[0.75, 0.25]);
        assert_eq!(inv[0], 1.0);
        assert_eq!(inv[INVERSE_SAMPLES - 1], 0.0);
    }

    #[test]
    fn interpolate_endpoints_midpoints_and_upper_clamp() {
        let table = [0.0, 0.5, 0.75];
        assert_eq!(interpolate(&table, 0.0), 0.0);
        assert_eq!(interpolate(&table, 0.25), 0.25);
        assert_eq!(interpolate(&table, 0.5), 0.5);
        assert_eq!(interpolate(&table, 0.75), 0.625);
        assert_eq!(interpolate(&table, 1.0), 0.75);
    }

    #[test]
    fn unknown_parametric_type_is_rejected_at_construction() {
        let err = ToneCurve::new(&CurveOrParametric::Parametric(ParametricCurve {
            function_type: 5,
            params: vec![s15(2.0)],
        }))
        .unwrap_err();
        assert!(matches!(err, CmmError::UnsupportedParametricType(5)));
        assert_eq!(
            err.to_string(),
            "cmm: parametric curve function type 5 is not supported"
        );
    }

    #[test]
    fn forward_eval_delegates_and_clamps() {
        // Type 2 with c = 0.5 exceeds 1 at the top: (1·1 + 0)^1 + 0.5 = 1.5, clamped to 1.
        let curve = parametric(2, &[1.0, 1.0, 0.0, 0.5]);
        assert_eq!(curve.eval(0.25), 0.75);
        assert_eq!(curve.eval(1.0), 1.0);
        // Input clamps into [0, 1] on both sides.
        assert_eq!(curve.eval(-2.0), 0.5);
        assert_eq!(curve.eval(3.0), 1.0);
    }

    #[test]
    fn analytic_inverse_forms_are_chosen_for_well_behaved_params() {
        assert!(matches!(
            parametric(0, &[2.5]).inverse().unwrap().repr,
            Repr::Power { exponent } if (exponent - 0.4).abs() < 1e-9
        ));
        let srgb = [2.4, 1.0 / 1.055, 0.055 / 1.055, 1.0 / 12.92, 0.04045];
        for (ty, params) in [
            (1, vec![2.0, 1.5, 0.25]),
            (2, vec![2.0, 1.5, 0.0, 0.125]),
            (3, srgb.to_vec()),
            (4, {
                let mut p = srgb.to_vec();
                p.extend([0.002, 0.001]); // e > f keeps the branch seam a (monotone) step up
                p
            }),
        ] {
            let inv = parametric(ty, &params).inverse().unwrap();
            assert!(
                matches!(inv.repr, Repr::InverseParametric { function_type, .. } if function_type == ty),
                "type {ty} should invert analytically"
            );
        }
    }

    #[test]
    fn degenerate_params_fall_back_to_the_numeric_inverse() {
        // Type 1 with a < 0: descending (1 − x)^2 — monotonic but outside the analytic
        // conditions, so the numeric path handles it.
        let curve = parametric(1, &[2.0, -1.0, 1.0]);
        let inv = curve.inverse().unwrap();
        assert!(matches!(inv.repr, Repr::SampledInverse { .. }));
        for i in 0..=100 {
            let x = f64::from(i) / 100.0;
            let y = curve.eval(x);
            let rt = curve.eval(inv.eval(y));
            assert!((rt - y).abs() < 1e-3, "round trip at x={x}: {rt} vs {y}");
        }
        // Type 3 with d > 1 (the linear toe covers the whole domain): numeric fallback.
        let toe_only = parametric(3, &[2.0, 1.0, 0.0, 0.5, 1.5]);
        assert!(matches!(
            toe_only.inverse().unwrap().repr,
            Repr::SampledInverse { .. }
        ));
    }

    #[test]
    fn constant_curves_are_rejected_by_inverse() {
        // One-entry table, constant table, and a parameterization collapsing to a constant
        // after range clamping (gamma 0 ⇒ x^0 = 1) all report NonMonotonicCurve.
        for curve in [
            sampled(vec![32768]),
            sampled(vec![20000, 20000, 20000]),
            ToneCurve::new(&CurveOrParametric::Curve(Curve::Gamma(U8Fixed8(0)))).unwrap(),
            parametric(0, &[0.0]),
        ] {
            assert!(
                matches!(curve.inverse().unwrap_err(), CmmError::NonMonotonicCurve),
                "constant curve must have no inverse"
            );
        }
        assert_eq!(
            CmmError::NonMonotonicCurve.to_string(),
            "cmm: tone curve is not monotonic; no inverse exists"
        );
    }

    #[test]
    fn empty_table_is_identity_and_inverts_to_identity() {
        let identity = sampled(vec![]);
        assert_eq!(identity.eval(0.42), 0.42);
        let inv = identity.inverse().unwrap();
        assert!(matches!(
            inv.repr,
            Repr::Icc(CurveOrParametric::Curve(Curve::Identity))
        ));
        assert_eq!(inv.eval(0.42), 0.42);
    }

    #[test]
    fn identity_inverts_to_identity() {
        let identity = ToneCurve::new(&CurveOrParametric::Curve(Curve::Identity)).unwrap();
        assert!(identity.is_monotonic());
        let inv = identity.inverse().unwrap();
        assert_eq!(inv.eval(0.3), 0.3);
        assert!(matches!(
            inv.repr,
            Repr::Icc(CurveOrParametric::Curve(Curve::Identity))
        ));
    }

    #[test]
    fn double_inversion_of_gamma_recovers_the_exponent() {
        let gamma =
            ToneCurve::new(&CurveOrParametric::Curve(Curve::Gamma(U8Fixed8(0x0233)))).unwrap(); // 2.19921875
        let inv = gamma.inverse().unwrap();
        let back = inv.inverse().unwrap();
        assert!(matches!(
            back.repr,
            Repr::Power { exponent } if (exponent - 2.199_218_75).abs() < 1e-12
        ));
        // And inverting an inverse-parametric or sampled-inverse repr goes numeric.
        let srgb_inv = parametric(3, &[2.4, 1.0 / 1.055, 0.055 / 1.055, 1.0 / 12.92, 0.04045])
            .inverse()
            .unwrap();
        assert!(matches!(
            srgb_inv.inverse().unwrap().repr,
            Repr::SampledInverse { .. }
        ));
    }

    #[test]
    fn inverse_parametric_closed_forms_match_hand_derivations() {
        // Type 1: y = (1.5x + 0.25)^2 ⇒ x = (√y − 0.25)/1.5. At y = 1: (1 − 0.25)/1.5 = 0.5.
        let p1 = [2.0, 1.5, 0.25, 0.0, 0.0, 0.0, 0.0];
        assert!((eval_inverse_parametric(1, &p1, 1.0) - 0.5).abs() < 1e-12);
        // At y = 0.25: (0.5 − 0.25)/1.5 = 1/6.
        assert!((eval_inverse_parametric(1, &p1, 0.25) - 1.0 / 6.0).abs() < 1e-12);

        // Type 2: y = (2x)^2 + 0.19 ⇒ x = √(y − 0.19)/2. At y = 0.44: √0.25/2 = 0.25.
        let p2 = [2.0, 2.0, 0.0, 0.19, 0.0, 0.0, 0.0];
        assert!((eval_inverse_parametric(2, &p2, 0.44) - 0.25).abs() < 1e-12);
        // Exactly at y == c the lcms2 convention returns 0, not −b/a.
        let p2b = [2.0, 2.0, 0.5, 0.19, 0.0, 0.0, 0.0];
        assert_eq!(eval_inverse_parametric(2, &p2b, 0.19), 0.0);
        // Below c: −b/a = −0.25.
        assert_eq!(eval_inverse_parametric(2, &p2b, 0.1), -0.25);

        // Type 3, sRGB-shaped: split at disc = (a·d + b)^g.
        let srgb: [f64; 7] = [
            2.4,
            1.0 / 1.055,
            0.055 / 1.055,
            1.0 / 12.92,
            0.04045,
            0.0,
            0.0,
        ];
        let disc = (srgb[1] * srgb[4] + srgb[2]).powf(2.4);
        // Below the split: linear inverse y/c.
        let y = disc / 2.0;
        assert!((eval_inverse_parametric(3, &srgb, y) - y * 12.92).abs() < 1e-12);
        // Above: power inverse (y^(1/2.4) − b)/a = 1.055·y^(1/2.4) − 0.055.
        let y: f64 = 0.25;
        let want = 1.055 * y.powf(1.0 / 2.4) - 0.055;
        assert!((eval_inverse_parametric(3, &srgb, y) - want).abs() < 1e-12);
        // Negative power base at the split (a·d + b < 0) clamps disc to 0.
        let neg = [2.0, 1.0, -0.5, 1.0, 0.25, 0.0, 0.0];
        // disc = 0 so even tiny y uses the power inverse: (y^(1/2) + 0.5)/1.
        assert!((eval_inverse_parametric(3, &neg, 0.0) - 0.5).abs() < 1e-12);

        // Type 4 with distinct e and f: y = (x)^2 + 0.25 above d = 0.5, else 0.4x + 0.05.
        // Split at c·d + f = 0.25 (the linear-branch value, lcms2 -5).
        let p4 = [2.0, 1.0, 0.0, 0.4, 0.5, 0.25, 0.05];
        // Below the split: (y − f)/c. At y = 0.21: 0.16/0.4 = 0.4.
        assert!((eval_inverse_parametric(4, &p4, 0.21) - 0.4).abs() < 1e-12);
        // Above: ((y − e)^(1/2) − 0)/1. At y = 0.5: √0.25 = 0.5.
        assert!((eval_inverse_parametric(4, &p4, 0.5) - 0.5).abs() < 1e-12);
        // y − e < 0 (y at or above the split c·d + f but below e) yields 0: split at
        // 0.4·0.5 + 0 = 0.2, e = 0.3, so y = 0.25 hits the negative-base guard.
        let p4b = [2.0, 1.0, 0.0, 0.4, 0.5, 0.3, 0.0];
        assert_eq!(eval_inverse_parametric(4, &p4b, 0.25), 0.0);
        // Exactly y − e == 0 takes the power branch: (0^(1/g) − b)/a = −b/a = 0.25 with
        // b = −0.25 — the guard is strict, matching lcms2's `if (e < 0)`.
        let p4c = [2.0, 1.0, -0.25, 0.1, 0.1, 0.05, 0.0];
        assert_eq!(eval_inverse_parametric(4, &p4c, 0.05), 0.25);
    }

    #[test]
    fn numeric_inverse_agrees_with_the_analytic_inverse() {
        // The same x² curve through both paths: analytic (type 0) and numeric (its 4096-point
        // sampling reversed). The numeric table resolves to ~1/4096 in x; √ has unbounded slope
        // at 0, so compare away from the origin.
        let analytic = parametric(0, &[2.0]).inverse().unwrap();
        let square = parametric(0, &[2.0]);
        let numeric = ToneCurve {
            repr: numeric_inverse_repr(&square),
        };
        let mut worst: f64 = 0.0;
        for i in 0..=1000 {
            let y = f64::from(i) / 1000.0;
            worst = worst.max((analytic.eval(y) - numeric.eval(y)).abs());
        }
        assert!(worst < 5e-3, "worst |analytic − numeric| = {worst}");
        // Away from the singular origin the agreement is much tighter.
        let mut worst: f64 = 0.0;
        for i in 100..=1000 {
            let y = f64::from(i) / 1000.0;
            worst = worst.max((analytic.eval(y) - numeric.eval(y)).abs());
        }
        assert!(worst < 1e-4, "worst tail |analytic − numeric| = {worst}");
    }

    #[test]
    fn descending_table_is_monotonic_and_round_trips() {
        let down = sampled(vec![65535, 0]);
        assert!(down.is_monotonic());
        let inv = down.inverse().unwrap();
        // The inverse of 1 − x is 1 − y; endpoints exactly, interior within table resolution.
        assert_eq!(inv.eval(0.0), 1.0);
        assert_eq!(inv.eval(1.0), 0.0);
        for i in 0..=100 {
            let y = f64::from(i) / 100.0;
            assert!((inv.eval(y) - (1.0 - y)).abs() < 1e-3, "at {y}");
        }
    }

    #[test]
    fn sampled_monotonicity_is_judged_on_entries_not_probes() {
        // A 16385-entry ramp with a single-entry dip at index 402 — chosen to sit strictly
        // between the dense probe positions (per-probe stride 16384/4095 ≈ 4.0, and the probes
        // around it interpolate entries 400/401 and 404/405), so a probe-based check would
        // miss it. The table-entry check must not.
        let mut table: Vec<u16> = (0..16385u32).map(|i| (i * 3) as u16).collect();
        table[402] = 0;
        assert!(!sampled(table).is_monotonic());
    }

    #[test]
    fn dense_probing_reaches_the_domain_endpoint() {
        // Monotone toe over almost the whole domain, then a drop confined to x ≥ 0.9998 —
        // only the final probe at exactly x = 1.0 sees it, so a probe grid that fell short of
        // the endpoint would call this curve monotonic.
        let cliff = parametric(4, &[1.0, -1.0, 1.0, 1.0, 0.9998, 0.0, 0.0]);
        assert!(!cliff.is_monotonic());
    }

    #[test]
    fn boundary_zero_params_route_types_3_and_4_to_the_numeric_inverse() {
        // Each parameter of the analytic guard pinned exactly at its 0.0 boundary, with a
        // curve that is still monotonic and non-constant (so `inverse` succeeds and the
        // dispatch itself is what is under test): the closed forms would divide by zero
        // (`1/g`, `/a`, `y/c`), so these must reverse numerically.
        for (label, ty, params) in [
            // g = 0: toe 0.5x, then (a·x + b)^0 == 1 above d — rise, jump, flat.
            ("g", 3, vec![0.0, 1.0, 0.0, 0.5, 0.5]),
            // a = 0: toe 0.5x, then constant 0.6^2 above d.
            ("a", 3, vec![2.0, 0.0, 0.6, 0.5, 0.5]),
            // c = 0: flat 0 toe, then x^2 above d.
            ("c", 3, vec![2.0, 1.0, 0.0, 0.0, 0.5]),
            // Same boundaries through the type-4 arm.
            ("g", 4, vec![0.0, 1.0, 0.0, 0.5, 0.5, 0.0, 0.0]),
            ("c", 4, vec![2.0, 1.0, 0.0, 0.0, 0.5, 0.0, 0.0]),
        ] {
            let inv = parametric(ty, &params).inverse().unwrap();
            assert!(
                matches!(inv.repr, Repr::SampledInverse { .. }),
                "type {ty} with {label} == 0 must invert numerically"
            );
        }
    }

    #[test]
    fn inverse_reprs_report_monotonicity() {
        // A numerically reversed curve is table-backed and checked on its entries...
        let ascending = sampled(vec![0, 20000, 65535]).inverse().unwrap();
        assert!(matches!(ascending.repr, Repr::SampledInverse { .. }));
        assert!(ascending.is_monotonic());
        let descending = sampled(vec![65535, 20000, 0]).inverse().unwrap();
        assert!(descending.is_monotonic());
        // ...while analytic inverse reprs go through the dense probe.
        let power = parametric(0, &[2.5]).inverse().unwrap();
        assert!(power.is_monotonic());
    }

    #[test]
    fn non_monotonic_parametric_is_detected_by_dense_probing() {
        // Type 4 whose toe descends (c = −1, f = 1) then power branch ascends: down-up.
        let wiggle = parametric(4, &[1.0, 1.0, 0.0, -1.0, 0.5, 0.0, 1.0]);
        assert!(!wiggle.is_monotonic());
        assert!(matches!(
            wiggle.inverse().unwrap_err(),
            CmmError::NonMonotonicCurve
        ));
    }

    /// Each modelled H.273 code point builds the `gamut-color` curve for it, forward, and
    /// its exact inverse once inverted.
    #[test]
    fn cicp_transfer_builds_the_code_points_closed_form() {
        use gamut_color::transfer::{
            bt709_eotf, bt709_oetf, hlg_inverse_oetf, hlg_oetf, pq_eotf_normalized,
            pq_oetf_normalized, srgb_eotf, srgb_oetf,
        };
        type Transfer = fn(f64) -> f64;
        let cases: [(u8, Transfer, Transfer); 7] = [
            (1, bt709_eotf, bt709_oetf),
            (6, bt709_eotf, bt709_oetf),
            (13, srgb_eotf, srgb_oetf),
            (14, bt709_eotf, bt709_oetf),
            (15, bt709_eotf, bt709_oetf),
            (16, pq_eotf_normalized, pq_oetf_normalized),
            (18, hlg_inverse_oetf, hlg_oetf),
        ];
        for (code_point, forward, inverse) in cases {
            let curve = ToneCurve::cicp_transfer(code_point).expect("modelled code point");
            let inverted = curve.inverse().expect("a transfer function inverts");
            for x in [0.1, 0.5, 0.9] {
                assert_eq!(curve.eval(x), forward(x), "code point {code_point} at {x}");
                assert_eq!(
                    inverted.eval(x),
                    inverse(x),
                    "code point {code_point} at {x}"
                );
            }
        }
    }

    #[test]
    fn cicp_linear_transfer_is_the_structural_identity() {
        let linear = ToneCurve::cicp_transfer(8).expect("code point 8 is modelled");
        assert!(linear.is_identity());
    }

    #[test]
    fn cicp_transfer_declines_what_gamut_color_does_not_evaluate() {
        // 2 is Unspecified (modelled, no curve); 4 is a Table 3 curve with no variant; 200 is
        // reserved.
        for code_point in [2, 4, 200] {
            assert!(
                ToneCurve::cicp_transfer(code_point).is_none(),
                "code point {code_point}"
            );
        }
    }

    fn table(table: Vec<f64>) -> ToneCurve {
        ToneCurve {
            repr: Repr::Table { table },
        }
    }

    /// The agreement bound is inclusive: a curve exactly [`CICP_AGREEMENT`] away agrees, the
    /// next representable distance does not. The ramps differ by `(1 − x) · d`, so the largest
    /// gap is `d` itself, at `x = 0`, computed without rounding.
    #[test]
    fn agreement_bound_is_inclusive() {
        let ramp = table(vec![0.0, 1.0]);
        let at_bound = table(vec![CICP_AGREEMENT, 1.0]);
        assert!(at_bound.agrees_with(&ramp));
        assert!(ramp.agrees_with(&at_bound), "the test is symmetric");
        let past = table(vec![CICP_AGREEMENT.next_up(), 1.0]);
        assert!(!past.agrees_with(&ramp));
    }

    /// The probe grid reaches `x = 1`: a gap that grows linearly to just past the bound at the
    /// top end, and is inside it everywhere short of the last probe, still disagrees.
    #[test]
    fn agreement_probes_the_top_endpoint() {
        let ramp = table(vec![0.0, 1.0]);
        let gap = CICP_AGREEMENT * (1.0 + 1e-6);
        let sagging = table(vec![0.0, 1.0 - gap]);
        let second_last = (MONOTONICITY_PROBES - 2) as f64 / (MONOTONICITY_PROBES - 1) as f64;
        assert!(
            second_last * gap <= CICP_AGREEMENT,
            "only x = 1 is past the bound"
        );
        assert!(!sagging.agrees_with(&ramp));
    }
}
