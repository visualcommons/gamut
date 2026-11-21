//! The pinned-seed property drivers for the [`super`] laws, and the tests that show each law can
//! fail. Moved out of `invariants.rs` verbatim (#1258) to keep that file under the file-length
//! limit.

use gamut_core::luminance::{BT601_LUMA_WEIGHTS, BT709_LUMA_WEIGHTS, BT2020_LUMA_WEIGHTS};
use proptest::collection;
use proptest::prelude::{Just, Strategy, prop_assert, prop_oneof, proptest};
use proptest::test_runner::{Config, RngSeed};

use super::{
    MAX_INPUT, ToneCurve, Violation, common_gain_preserves_channel_ratios,
    map_rgb_slice_is_pixelwise_map_rgb, map_slice_is_elementwise_map,
    map_slice_is_order_independent, monotonic_non_decreasing, normalise_input,
    output_is_non_negative_and_never_nan, rgb_output_is_non_negative_and_never_nan,
    source_peak_grey_maps_to_target_peak,
};
use crate::{
    Aces, ApplyMode, Clamp, CurveToneMapper, Drago, Exposure, Hable, Linear, Reinhard,
    ReinhardExtended, ToneMapper,
};

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

// ---- `ToneMapper` ------------------------------------------------------------------------

/// A `CurveToneMapper`'s configuration, drawn as data for the same reason `Operator` is.
///
/// Every range keeps construction infallible: the smallest `curve(source_peak)` any operator
/// reaches over `source_peak ≥ 1e-2` is above `1e-3` and the largest (`Exposure` at `1e6`
/// times `1e4`) is `1e10`, so `target_peak / curve(source_peak)` stays a normal, positive
/// `f32` across the whole box.
#[derive(Debug, Clone, Copy)]
struct MapperConfig {
    operator: Operator,
    mode: ApplyMode,
    desat: f32,
    source_peak: f32,
    target_peak: f32,
    luma_weights: [u32; 3],
}

impl MapperConfig {
    fn build(self) -> CurveToneMapper<impl ToneCurve> {
        // `Box<dyn ToneCurve>` is not itself a curve (the blanket `Fn` impl forecloses it);
        // the closure is.
        let curve = self.operator.build();
        CurveToneMapper::new(move |x: f32| curve.map(x), self.source_peak)
            .and_then(|m| m.with_target_peak(self.target_peak))
            .and_then(|m| m.with_desat(self.desat))
            .and_then(|m| m.with_luma_weights(self.luma_weights))
            .expect("every drawn configuration is constructible")
            .with_mode(self.mode)
    }
}

fn mode() -> impl Strategy<Value = ApplyMode> {
    prop_oneof![
        Just(ApplyMode::PerChannel),
        Just(ApplyMode::Luminance),
        Just(ApplyMode::MaxRgb),
    ]
}

fn mapper_config() -> impl Strategy<Value = MapperConfig> {
    (
        operator(),
        mode(),
        prop_oneof![Just(0.0_f32), 0.0_f32..16.0],
        1e-2_f32..1e4,
        1e-2_f32..1e2,
        prop_oneof![
            Just(BT709_LUMA_WEIGHTS),
            Just(BT2020_LUMA_WEIGHTS),
            Just(BT601_LUMA_WEIGHTS),
        ],
    )
        .prop_map(
            |(operator, mode, desat, source_peak, target_peak, luma_weights)| MapperConfig {
                operator,
                mode,
                desat,
                source_peak,
                target_peak,
                luma_weights,
            },
        )
}

/// Pixels of finite, non-negative channels, each channel normalised exactly as the curve
/// laws' inputs are — and, as there, by the caller rather than the law.
fn pixels() -> impl Strategy<Value = Vec<[f32; 3]>> {
    collection::vec(
        proptest::array::uniform3(proptest::num::u32::ANY.prop_map(normalise_input)),
        1..16,
    )
}

proptest! {
    #![proptest_config(config())]

    /// No mode, curve or parameter produces a NaN or negative channel from a valid pixel.
    #[test]
    fn no_tone_mapper_produces_nan_or_a_negative_channel(
        config in mapper_config(),
        pixels in pixels(),
    ) {
        let result = rgb_output_is_non_negative_and_never_nan(&config.build(), &pixels);
        prop_assert!(result.is_ok(), "{}", describe(&result));
    }

    /// `map_rgb_slice` is pixelwise `map_rgb`, bit for bit.
    #[test]
    fn map_rgb_slice_agrees_with_map_rgb_on_every_pixel(
        config in mapper_config(),
        pixels in pixels(),
    ) {
        let result = map_rgb_slice_is_pixelwise_map_rgb(&config.build(), &pixels);
        prop_assert!(result.is_ok(), "{}", describe(&result));
    }

    /// Luminance and max-RGB modes scale a pixel by one gain, so they keep its hue.
    #[test]
    fn luminance_and_max_rgb_modes_preserve_channel_ratios(
        config in mapper_config(),
        luminance in proptest::bool::ANY,
        pixels in pixels(),
    ) {
        let mode = if luminance { ApplyMode::Luminance } else { ApplyMode::MaxRgb };
        let mapper = MapperConfig { mode, desat: 0.0, ..config }.build();
        let result = common_gain_preserves_channel_ratios(&mapper, &pixels);
        prop_assert!(result.is_ok(), "{}", describe(&result));
    }

    /// The source peak maps onto the target peak, whatever the curve, mode and desat.
    #[test]
    fn a_grey_source_peak_maps_to_the_target_peak(
        config in mapper_config(),
        source_peak in 1.0_f32..1e4,
    ) {
        let mapper = MapperConfig { source_peak, ..config }.build();
        let result = source_peak_grey_maps_to_target_peak(&mapper);
        prop_assert!(result.is_ok(), "{}", describe(&result));
    }
}

#[test]
fn per_channel_mode_violates_the_common_gain_law() {
    // The law must be able to fail: per-channel Reinhard compresses 4 by more than 1.
    let mapper = CurveToneMapper::new(Reinhard, 4.0)
        .and_then(|m| m.with_desat(0.0))
        .expect("valid")
        .with_mode(ApplyMode::PerChannel);
    let violation = common_gain_preserves_channel_ratios(&mapper, &[[4.0, 1.0, 0.0]])
        .expect_err("per-channel mapping changes channel ratios");
    assert_eq!(violation.law(), "common_gain_preserves_channel_ratios");
}

#[test]
fn a_mapper_that_lifts_a_zero_channel_violates_the_common_gain_law() {
    struct Lifts;
    impl ToneMapper for Lifts {
        fn map_rgb(&self, rgb: [f32; 3]) -> [f32; 3] {
            rgb.map(|c| c + 1.0)
        }
    }
    let violation = common_gain_preserves_channel_ratios(&Lifts, &[[1.0, 0.0, 0.0]])
        .expect_err("a zero channel became non-zero");
    assert!(violation.detail().contains("zero channel"), "{violation}");
}

#[test]
fn the_rgb_laws_can_fail() {
    struct Broken;
    impl ToneMapper for Broken {
        fn map_rgb(&self, rgb: [f32; 3]) -> [f32; 3] {
            [f32::NAN, -rgb[1], rgb[2]]
        }
        fn map_rgb_slice(&self, buf: &mut [[f32; 3]]) {
            buf.reverse();
        }
    }
    let pixels = [[1.0_f32, 2.0, 3.0], [4.0, 5.0, 6.0]];
    assert!(rgb_output_is_non_negative_and_never_nan(&Broken, &pixels).is_err());
    assert!(map_rgb_slice_is_pixelwise_map_rgb(&Broken, &pixels).is_err());

    // The normalization is measured once, at construction, so a curve that answers
    // differently afterwards is the one way to make a real `CurveToneMapper` miss its target.
    let jumps = std::cell::Cell::new(false);
    let curve = |x: f32| if jumps.replace(true) { 2.0 * x } else { x };
    let mapper = CurveToneMapper::new(curve, 1.0)
        .expect("valid")
        .with_mode(ApplyMode::MaxRgb);
    let violation = source_peak_grey_maps_to_target_peak(&mapper)
        .expect_err("a curve that changed since construction misses the target");
    assert_eq!(violation.law(), "source_peak_grey_maps_to_target_peak");
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
    for white in [1.0_f32, crate::DEFAULT_HABLE_WHITE, 0.1] {
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
