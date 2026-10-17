//! fuzz · law — the tone-curve contract, searched rather than sampled.
//!
//! These are the *same* functions `gamut_tonemap::invariants` exposes to the pinned-seed
//! `proptest` properties in the per-PR gate. Nothing is restated here: a law written twice is a
//! law that can disagree with itself, and both disagreeing copies would keep passing.
//!
//! `gamut-tonemap` has **no oracle** — `docs/testing.md`'s per-crate table names its primary
//! technique as property, because the `ToneCurve` contract is the only authority there is. That
//! makes this tier worth more here than in a crate with a reference implementation to differ
//! against: an unbounded search is the closest thing to a second opinion the crate can have.
//!
//! A crash found here is **minimised and promoted into a named deterministic test** in
//! `gamut-tonemap`'s own suite. The corpus is a search aid, not the regression record.

#![no_main]

use gamut_tonemap::invariants::{
    map_slice_is_elementwise_map, map_slice_is_order_independent, monotonic_non_decreasing,
    normalise_input, output_is_non_negative_and_never_nan,
};
use gamut_tonemap::{Aces, Clamp, Drago, Exposure, Hable, Linear, Reinhard, ReinhardExtended};
use libfuzzer_sys::fuzz_target;

/// A constructed operator, kept behind an enum rather than a `Box<dyn ToneCurve>`.
///
/// Same shape, and the same reason, as the property's `Operator`: the built-ins are `Copy` and the
/// blanket `Fn(f32) -> f32` impl forecloses `impl ToneCurve for Box<T>` forever, so a boxed curve
/// is not itself a curve. Dispatch is through `&dyn ToneCurve` at the call site instead.
enum Operator {
    Linear(Linear),
    Reinhard(Reinhard),
    Aces(Aces),
    Clamp(Clamp),
    Exposure(Exposure),
    ReinhardExtended(ReinhardExtended),
    Hable(Hable),
    Drago(Drago),
}

impl Operator {
    fn curve(&self) -> &dyn gamut_tonemap::ToneCurve {
        match self {
            Self::Linear(c) => c,
            Self::Reinhard(c) => c,
            Self::Aces(c) => c,
            Self::Clamp(c) => c,
            Self::Exposure(c) => c,
            Self::ReinhardExtended(c) => c,
            Self::Hable(c) => c,
            Self::Drago(c) => c,
        }
    }
}

/// Builds an operator from a selector byte and four parameter bytes.
///
/// Parameters go through `normalise_input`, the same mapping the property's strategies use, so
/// both tiers search the same space rather than merely calling the same functions. A constructor
/// that rejects its parameter yields `None` and the input is skipped: the rejected ranges are
/// pinned by the `*_rejects_invalid_*` unit tests in `operators.rs`, and a law about curve *shape*
/// has nothing to say about a curve that was never built.
fn build(selector: u8, param: f32) -> Option<Operator> {
    Some(match selector % 8 {
        0 => Operator::Linear(Linear),
        1 => Operator::Reinhard(Reinhard),
        2 => Operator::Aces(Aces),
        3 => Operator::Clamp(Clamp::new(param).ok()?),
        4 => Operator::Exposure(Exposure::new(param).ok()?),
        5 => Operator::ReinhardExtended(ReinhardExtended::new(param).ok()?),
        6 => Operator::Hable(Hable::new(param).ok()?),
        // Drago's monotonicity is conditional on its parameters -- `is_monotonic` states the
        // condition (#439) -- so the law below asks it first rather than skipping the operator.
        _ => Operator::Drago(Drago::new(param).ok()?),
    })
}

fuzz_target!(|data: &[u8]| {
    // One selector byte, four parameter bytes, then four bytes per sample.
    let Some((&selector, rest)) = data.split_first() else {
        return;
    };
    if rest.len() < 4 {
        return;
    }
    let (param_bytes, sample_bytes) = rest.split_at(4);
    let param = normalise_input(u32::from_le_bytes([
        param_bytes[0],
        param_bytes[1],
        param_bytes[2],
        param_bytes[3],
    ]));
    let Some(operator) = build(selector, param) else {
        return;
    };
    let curve = operator.curve();

    // `Drago` is the one operator whose laws are stated over a *parameterised* domain rather than
    // the whole f32 range: `monotonic_non_decreasing`'s contract says the samples must lie inside
    // `[0, world_max]`, and outside it the curve is documented as no longer tracking the model.
    // So its samples are drawn as fractions of `world_max`, exactly as the property's strategy
    // does. Feeding it `normalise_input`'s full range instead reports the operator's
    // out-of-domain behaviour as a violation — which is what the first draft of this target did.
    let mut xs: Vec<f32> = match &operator {
        Operator::Drago(d) => {
            let world_max = d.world_max();
            sample_bytes
                .as_chunks::<4>()
                .0
                .iter()
                .map(|c| {
                    let fraction = f64::from(u32::from_le_bytes(*c)) / f64::from(u32::MAX);
                    (fraction as f32) * world_max
                })
                .collect()
        }
        _ => sample_bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| normalise_input(u32::from_le_bytes(*c)))
            .collect(),
    };
    if xs.is_empty() {
        return;
    }

    // Law 1: finite non-negative input gives non-negative, never-NaN output. The half of the
    // contract that holds for every operator at every accepted parameter, including Drago.
    if let Err(violation) = output_is_non_negative_and_never_nan(curve, &xs) {
        panic!("{violation}");
    }

    // Law 2: `map_slice` is elementwise `map` …
    if let Err(violation) = map_slice_is_elementwise_map(curve, &xs) {
        panic!("{violation}");
    }
    // … and does not depend on the order it visits elements in.
    if let Err(violation) = map_slice_is_order_independent(curve, &xs) {
        panic!("{violation}");
    }

    // Law 3: the curve never decreases. Sorted first, because the law is about adjacent pairs of
    // an ascending sequence, not about the order the engine happened to emit bytes in.
    //
    // Drago is asked whether it claims monotonicity for these parameters before being held to it:
    // it transcribes a published formula that is not monotonic everywhere its constructors accept,
    // and `is_monotonic` is where that condition lives (#439). Every other operator promises it
    // unconditionally.
    xs.sort_by(f32::total_cmp);
    let claims_monotonic = match &operator {
        Operator::Drago(d) => d.is_monotonic(),
        _ => true,
    };
    if claims_monotonic && let Err(violation) = monotonic_non_decreasing(curve, &xs) {
        panic!("{violation}");
    }
});
