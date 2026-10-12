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

use crate::ToneCurve;

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
/// [`Drago::is_monotonic`](crate::operators::Drago::is_monotonic) accepts, so a caller checking
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

#[cfg(test)]
mod tests {
    use proptest::collection;
    use proptest::prelude::{Just, Strategy, prop_assert, prop_oneof, proptest};
    use proptest::test_runner::{Config, RngSeed};

    use super::{
        MAX_INPUT, ToneCurve, Violation, map_slice_is_elementwise_map,
        map_slice_is_order_independent, monotonic_non_decreasing, normalise_input,
        output_is_non_negative_and_never_nan,
    };
    use crate::{Aces, Clamp, Drago, Exposure, Hable, Linear, Reinhard, ReinhardExtended};

    /// The workspace property-test configuration (`docs/testing.md`).
    ///
    /// The seed is **pinned**: the `--in-diff` mutation gate is blocking, so a test whose
    /// pass/fail depends on OS entropy would make cargo-mutants report CAUGHT or MISSED for the
    /// same mutant on different runs. Shrink bounds are finite because proptest's defaults are
    /// `u32::MAX` iterations and no wall-clock cap, and failure persistence is off because
    /// cargo-mutants reuses one tree copy across mutants, so a `proptest-regressions` file would
    /// leak into the next mutant's run and be replayed first.
    fn config() -> Config {
        Config {
            cases: 512,
            max_shrink_iters: 2048,
            max_shrink_time: 10_000,
            rng_seed: RngSeed::Fixed(0x6761_6D75_745F_746D),
            failure_persistence: None,
            ..Config::default()
        }
    }

    /// A built-in operator, named and parameterised.
    ///
    /// The strategy yields this rather than a `Box<dyn ToneCurve>` for two reasons: proptest
    /// requires `Debug` to print a counterexample, and a shrunk failure that says
    /// `Hable { white: 0.01 }` names the operator and the parameter, where a boxed trait object
    /// would say nothing at all.
    ///
    /// Parameters are drawn only from each constructor's accepted range — the rejected ranges are
    /// already pinned by the `*_rejects_invalid_*` unit tests in `operators.rs`, and a law about
    /// curve *shape* has nothing to say about a curve that was never built.
    ///
    /// `Drago` is absent deliberately: it promises monotonicity only on `[0, world_max]`, and
    /// only for some parameters within that, so it needs inputs tied to its own parameter and
    /// gets its own pair of properties below.
    #[derive(Debug, Clone, Copy)]
    enum Operator {
        Linear,
        Reinhard,
        Aces,
        Clamp { max: f32 },
        ReinhardExtended { white: f32 },
        Exposure { scale: f32 },
        Hable { white: f32 },
    }

    impl Operator {
        /// Builds the operator. Every parameter the strategy draws is inside the constructor's
        /// accepted range, so construction cannot fail.
        fn build(self) -> Box<dyn ToneCurve> {
            match self {
                Self::Linear => Box::new(Linear),
                Self::Reinhard => Box::new(Reinhard),
                Self::Aces => Box::new(Aces),
                Self::Clamp { max } => Box::new(Clamp::new(max).expect("max drawn > 0 and finite")),
                Self::ReinhardExtended { white } => Box::new(
                    ReinhardExtended::new(white).expect("white drawn > 0, finite, white^2 normal"),
                ),
                Self::Exposure { scale } => {
                    Box::new(Exposure::new(scale).expect("scale drawn > 0 and finite"))
                }
                Self::Hable { white } => {
                    Box::new(Hable::new(white).expect("white drawn > 0 and finite"))
                }
            }
        }
    }

    /// One of the built-in operators, with parameters inside its constructor's accepted range.
    fn operator() -> impl Strategy<Value = Operator> {
        prop_oneof![
            Just(Operator::Linear),
            Just(Operator::Reinhard),
            Just(Operator::Aces),
            (1e-3_f32..1e6).prop_map(|max| Operator::Clamp { max }),
            (1e-2_f32..1e6).prop_map(|white| Operator::ReinhardExtended { white }),
            (1e-3_f32..1e6).prop_map(|scale| Operator::Exposure { scale }),
            (1e-2_f32..1e4).prop_map(|white| Operator::Hable { white }),
        ]
    }

    /// Inputs inside the domain the trait contract covers: finite and non-negative.
    ///
    /// Normalising here rather than inside the laws is deliberate: a law that normalised its own
    /// inputs would feed the same value to the model and to the code under test, so a defect in
    /// the normaliser would cancel on both sides.
    fn inputs() -> impl Strategy<Value = Vec<f32>> {
        collection::vec(proptest::num::u32::ANY.prop_map(normalise_input), 1..24)
    }

    /// The same inputs, sorted — what [`monotonic_non_decreasing`] requires of its caller.
    fn ascending_inputs() -> impl Strategy<Value = Vec<f32>> {
        inputs().prop_map(|mut xs| {
            xs.sort_by(f32::total_cmp);
            xs
        })
    }

    /// Renders a law's result for a proptest failure message.
    fn describe(result: &Result<(), Violation>) -> String {
        match result {
            Ok(()) => String::new(),
            Err(violation) => violation.to_string(),
        }
    }

    proptest! {
        #![proptest_config(config())]

        /// The trait contract's hard guarantee: no operator produces NaN or a negative output
        /// anywhere in the domain, however extreme the input.
        #[test]
        fn no_built_in_operator_produces_nan_or_a_negative_output(
            operator in operator(),
            xs in inputs(),
        ) {
            let curve = operator.build();
            let result = output_is_non_negative_and_never_nan(curve.as_ref(), &xs);
            prop_assert!(result.is_ok(), "{}", describe(&result));
        }

        /// Every built-in is monotonic non-decreasing across its documented domain.
        #[test]
        fn every_built_in_operator_is_monotonic_non_decreasing(
            operator in operator(),
            xs in ascending_inputs(),
        ) {
            let curve = operator.build();
            let result = monotonic_non_decreasing(curve.as_ref(), &xs);
            prop_assert!(result.is_ok(), "{}", describe(&result));
        }

        /// `map_slice` is elementwise `map`, bit for bit.
        #[test]
        fn map_slice_agrees_with_map_on_every_element(
            operator in operator(),
            xs in inputs(),
        ) {
            let curve = operator.build();
            let result = map_slice_is_elementwise_map(curve.as_ref(), &xs);
            prop_assert!(result.is_ok(), "{}", describe(&result));
        }

        /// `map_slice` carries no state between elements: reversing the input reverses the output.
        #[test]
        fn map_slice_does_not_depend_on_element_order(
            operator in operator(),
            xs in inputs(),
        ) {
            let curve = operator.build();
            let result = map_slice_is_order_independent(curve.as_ref(), &xs);
            prop_assert!(result.is_ok(), "{}", describe(&result));
        }

        /// `Drago` stays non-negative and NaN-free across its whole accepted parameter range.
        ///
        /// The full range is used here deliberately: this half of the contract does hold
        /// everywhere, and it is the half the operator's `powf`/`ln` arithmetic could plausibly
        /// break.
        #[test]
        fn drago_output_is_non_negative_and_never_nan(
            world_max in 1e-2_f32..1e6,
            bias in 0.01_f32..0.99,
            fractions in collection::vec(0.0_f32..=1.0, 1..24),
        ) {
            let drago = Drago::new(world_max)
                .expect("world_max > 0 and finite")
                .with_bias(bias)
                .expect("bias in (0, 1)");
            let xs: Vec<f32> = fractions.iter().map(|f| f * world_max).collect();

            let result = output_is_non_negative_and_never_nan(&drago, &xs);
            prop_assert!(result.is_ok(), "{}", describe(&result));
        }

        /// `Drago` is monotonic on `[0, world_max]` wherever it says it is.
        ///
        /// This is the **soundness** half of `Drago::is_monotonic` (#439): the predicate may not
        /// promise an ordering the curve does not have, or every caller that trusts it is wrong.
        /// Parameters come from the constructors' whole accepted range rather than a hand-picked
        /// safe corner — the point is to catch a predicate that is too generous, and restricting
        /// the draw is exactly how the previous version of this property missed the defect.
        ///
        /// Cases where the predicate says `false` assert nothing here; that direction is the
        /// converse property below.
        #[test]
        fn drago_is_monotonic_wherever_it_claims_to_be(
            world_max in 1e-2_f32..1e6,
            bias in 0.01_f32..0.99,
            fractions in collection::vec(0.0_f32..=1.0, 1..24),
        ) {
            let drago = Drago::new(world_max)
                .expect("world_max > 0 and finite")
                .with_bias(bias)
                .expect("bias in (0, 1)");

            if drago.is_monotonic() {
                let mut xs: Vec<f32> = fractions.iter().map(|f| f * world_max).collect();
                xs.sort_by(f32::total_cmp);

                let result = monotonic_non_decreasing(&drago, &xs);
                prop_assert!(result.is_ok(), "{}", describe(&result));
            }
        }

        /// And where `Drago` says it is *not* monotonic, the decrease is really there.
        ///
        /// Without this the property above is vacuous: `is_monotonic` could return `false`
        /// unconditionally and still pass it. So this is not a second opinion on the same claim,
        /// it is the only thing standing between the predicate and a constant.
        ///
        /// The draw is deliberately well past the ceiling rather than just over it — at
        /// `bias <= 0.79` the condition already fails by `world_max ≈ 7.6e3` — so the drop is
        /// large compared with the law's ULP tolerance and the test does not hinge on rounding.
        /// Measured across this box the smallest drop is `2.1e-3` against a tolerance of `1.0e-6`,
        /// three orders of margin. Samples cover the top half of the domain, where the quotient
        /// turns over.
        #[test]
        fn drago_really_decreases_where_it_says_it_is_not_monotonic(
            world_max in 1e6_f32..1e12,
            bias in 0.5_f32..0.79,
        ) {
            let drago = Drago::new(world_max)
                .expect("world_max > 0 and finite")
                .with_bias(bias)
                .expect("bias in (0, 1)");
            prop_assert!(!drago.is_monotonic(), "the box was chosen to be past the ceiling");

            let xs: Vec<f32> = (0..=32)
                .map(|i| world_max * (0.5 + 0.5 * i as f32 / 32.0))
                .collect();

            let result = monotonic_non_decreasing(&drago, &xs);
            prop_assert!(
                result.is_err(),
                "is_monotonic() == false but no decrease was found on [0.5·world_max, world_max]"
            );
        }
    }

    /// A sample set drawn entirely from `Hable`'s near-zero cancellation region is not a
    /// monotonicity violation.
    ///
    /// The named, deterministic form of the first crash the fuzz tier found (#264). The corpus is
    /// a search aid, not the regression record, so the input is written down here rather than
    /// committed as a seed: a saved seed is reproducible only while the target's byte-to-input
    /// mapping is unchanged, and this is reproducible forever.
    ///
    /// Before `curve_output_scale` probed the curve, the tolerance was derived from the sampled
    /// outputs alone. Every output here is around `1e-7`, so the tolerance collapsed to about
    /// `1e-13` and the law reported the very float noise its doc comment says it exists to
    /// tolerate. A pinned-seed property never drew an all-tiny set in 512 cases; a coverage-guided
    /// engine found the class in under a minute.
    #[test]
    fn hable_near_zero_noise_is_not_a_monotonicity_violation() {
        // The region the crash landed in, at three white points including the default.
        for white in [1.0_f32, crate::constants::DEFAULT_HABLE_WHITE, 0.1] {
            let hable = Hable::new(white).expect("white > 0");
            let xs: Vec<f32> = (0..64).map(|i| 1e-10 + (i as f32) * 1e-8).collect();

            // Sanity: this really is the cancellation region — the outputs are noise-scale, far
            // below the curve's range, which is what made the old tolerance collapse.
            let biggest = xs
                .iter()
                .map(|&x| hable.map(x).abs())
                .fold(0.0_f32, f32::max);
            assert!(
                biggest < 1e-5,
                "the fixture must sit in the cancellation region, got {biggest:e}"
            );

            assert!(
                monotonic_non_decreasing(&hable, &xs).is_ok(),
                "white {white}: near-zero float noise reported as a shape violation"
            );
        }
    }

    /// And the fix does not blunt the law: a genuine decrease is still caught.
    ///
    /// Without this the widening is unfalsifiable — `curve_output_scale` could return `f32::MAX`
    /// and the test above would still pass. The step here is `1e-5` against a curve of range 1,
    /// which is **ten times** the `8`-ULP tolerance that scale allows (`9.5e-7`) and two decades
    /// *below* the smallest genuine non-monotonicity the crate knows of (`Drago`'s, at `1e-3`).
    /// So the law's sensitivity still brackets every real defect on record.
    #[test]
    fn the_widened_tolerance_still_catches_a_real_decrease() {
        struct Sagging;
        impl ToneCurve for Sagging {
            fn map(&self, x: f32) -> f32 {
                if x < 0.5 { 1.0 } else { 1.0 - 1e-5 }
            }
        }

        let xs = [0.0_f32, 0.25, 0.75, 1.0];
        assert!(
            monotonic_non_decreasing(&Sagging, &xs).is_err(),
            "a 1e-5 step down in a curve of range 1 must still be a violation"
        );
    }

    // ---- the laws' own failure paths -------------------------------------------------------
    //
    // `invariants.rs` is excluded from mutation, so these are not mutation-driven. They exist so
    // that a law which can no longer *fail* is caught: a law stuck at `Ok(())` would make every
    // property above vacuous while still passing.

    #[test]
    fn a_nan_producing_curve_violates_the_non_nan_law() {
        let nan = |_x: f32| f32::NAN;
        let violation = output_is_non_negative_and_never_nan(&nan, &[1.0])
            .expect_err("a curve returning NaN must violate the law");

        assert_eq!(violation.law(), "output_is_non_negative_and_never_nan");
        assert!(violation.detail().contains("NaN"), "{violation}");
    }

    #[test]
    fn a_negative_curve_violates_the_non_negative_law() {
        let negative = |x: f32| -x - 1.0;
        let violation = output_is_non_negative_and_never_nan(&negative, &[1.0])
            .expect_err("a curve returning a negative value must violate the law");

        assert!(violation.detail().contains("< 0"), "{violation}");
    }

    #[test]
    fn a_decreasing_curve_violates_the_monotonicity_law() {
        let decreasing = |x: f32| -x;
        let violation = monotonic_non_decreasing(&decreasing, &[1.0, 2.0])
            .expect_err("a decreasing curve must violate the law");

        assert_eq!(violation.law(), "monotonic_non_decreasing");
    }

    #[test]
    fn a_flat_curve_satisfies_the_monotonicity_law() {
        // Non-decreasing, not strictly increasing: equal adjacent outputs are permitted, which is
        // exactly what `Clamp` does above its maximum.
        let flat = |_x: f32| 0.5_f32;
        assert!(monotonic_non_decreasing(&flat, &[1.0, 2.0, 3.0]).is_ok());
    }

    #[test]
    fn a_map_slice_that_skips_an_element_violates_the_elementwise_law() {
        struct SkipsLast;
        impl ToneCurve for SkipsLast {
            fn map(&self, x: f32) -> f32 {
                x * 2.0
            }
            fn map_slice(&self, buf: &mut [f32]) {
                let len = buf.len();
                for x in buf.iter_mut().take(len.saturating_sub(1)) {
                    *x *= 2.0;
                }
            }
        }

        let violation = map_slice_is_elementwise_map(&SkipsLast, &[1.0, 2.0])
            .expect_err("skipping the last element must violate the law");

        assert_eq!(violation.law(), "map_slice_is_elementwise_map");
        assert!(violation.detail().contains("index 1"), "{violation}");
    }

    #[test]
    fn a_position_dependent_map_slice_violates_the_order_law() {
        struct AddsItsIndex;
        impl ToneCurve for AddsItsIndex {
            fn map(&self, x: f32) -> f32 {
                x
            }
            fn map_slice(&self, buf: &mut [f32]) {
                for (index, x) in buf.iter_mut().enumerate() {
                    *x += index as f32;
                }
            }
        }

        let violation = map_slice_is_order_independent(&AddsItsIndex, &[1.0, 2.0, 3.0])
            .expect_err("a position-dependent map_slice must violate the law");

        assert_eq!(violation.law(), "map_slice_is_order_independent");
    }

    #[test]
    fn normalise_input_always_lands_in_the_documented_domain() {
        // The generator's contract: whatever bits a fuzz driver supplies, the laws see a finite,
        // non-negative input no larger than MAX_INPUT.
        for bits in [
            0u32,
            1,
            0x7FFF_FFFF,
            0x8000_0000,
            u32::MAX,
            0xDEAD_BEEF,
            0x0001_0000,
        ] {
            let x = normalise_input(bits);
            assert!(x.is_finite(), "{bits:#x} gave {x:e}");
            assert!(x >= 0.0, "{bits:#x} gave {x:e}");
            assert!(x <= MAX_INPUT, "{bits:#x} gave {x:e}");
        }
    }

    #[test]
    fn normalise_input_reaches_both_ends_of_the_domain() {
        // A generator that collapsed to one magnitude would make every property above run on a
        // single point of the domain while still passing.
        let sampled: Vec<f32> = (0..4096_u32)
            .map(|i| normalise_input(i.wrapping_mul(0x9E37_79B9)))
            .collect();

        assert!(
            sampled.iter().any(|&x| x < 1e-6),
            "no sample near zero: min {:e}",
            sampled.iter().copied().fold(f32::INFINITY, f32::min)
        );
        assert!(
            sampled.iter().any(|&x| x > 1e6),
            "no large sample: max {:e}",
            sampled.iter().copied().fold(0.0_f32, f32::max)
        );
    }
}
