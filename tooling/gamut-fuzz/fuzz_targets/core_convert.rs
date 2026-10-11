//! fuzz · law — the pixel-conversion laws, searched rather than sampled.
//!
//! These are the *same* functions `gamut_core::invariants` exposes to the pinned-seed `proptest`
//! properties in the per-PR gate. Nothing is restated here: a law written twice is a law that can
//! disagree with itself, and both copies would keep passing against their own version of it.
//!
//! `gamut_core::convert` is the one place any `Pixel` layout converts to any other, so every
//! format crate inherits whatever is wrong with it. That is what makes an unbounded search worth
//! running over a 7 × 7 target matrix whose interesting cases — a zero alpha, a palette index past
//! the end, a threshold exactly at the midpoint — are sparse in a uniform sample.
//!
//! A crash found here is **minimised and promoted into a named deterministic test** in
//! `gamut-core`'s own suite. The corpus is a search aid, not the regression record.

#![no_main]

use gamut_core::convert::{AlphaPolicy, ConvertPolicy, DepthPolicy, LumaPolicy};
use gamut_core::invariants::{
    NARROW_FORMATS, Violation, acceptance_is_independent_of_the_samples,
    converting_a_layout_to_itself_changes_nothing, normalise_dims, opaque_pixels_are_fixed_points,
    output_shape_matches_the_target_layout, palette_and_cmyk_convert_only_to_themselves,
    premultiply_round_trip_is_bounded_and_idempotent, premultiply_rounds_to_nearest,
    the_in_place_door_matches_the_allocating_door, unpremultiply_rounds_to_nearest_or_clamps,
};
use gamut_core::{
    Bilevel, Cmyk8, Dimensions, Gray8, GrayAlpha8, GrayAlpha16, Indexed8, Rgb8, Rgba8, Rgba16,
};
use libfuzzer_sys::fuzz_target;

/// Runs a two-parameter law for every 8-bit target layout, stopping at the first violation.
///
/// The twin of the property's macro of the same name. It is a **dispatch table over the sealed
/// `Pixel` matrix, not a law** — the seven arms are exactly `NARROW_FORMATS`, which is shared
/// between the tiers as a constant — so the thing `docs/testing.md` forbids duplicating (the
/// specification) still has one home in `gamut_core::invariants`.
macro_rules! for_each_narrow_target {
    ($law:ident, $($arg:expr),* $(,)?) => {{
        let mut result: Result<(), Violation> = Ok(());
        if result.is_ok() { result = $law::<u8, Gray8>($($arg),*); }
        if result.is_ok() { result = $law::<u8, Bilevel>($($arg),*); }
        if result.is_ok() { result = $law::<u8, Indexed8>($($arg),*); }
        if result.is_ok() { result = $law::<u8, Rgb8>($($arg),*); }
        if result.is_ok() { result = $law::<u8, Rgba8>($($arg),*); }
        if result.is_ok() { result = $law::<u8, Cmyk8>($($arg),*); }
        if result.is_ok() { result = $law::<u8, GrayAlpha8>($($arg),*); }
        result
    }};
}

/// Builds a policy from five bytes, spanning the same space as the property's strategy.
fn policy(b: &[u8]) -> ConvertPolicy {
    let alpha = match b[0] % 3 {
        0 => AlphaPolicy::Reject,
        1 => AlphaPolicy::Drop,
        _ => AlphaPolicy::CompositeOver,
    };
    let depth = if b[1] % 2 == 0 {
        DepthPolicy::Reject
    } else {
        DepthPolicy::Rescale
    };
    let luma = match b[2] % 4 {
        0 => LumaPolicy::Reject,
        1 => LumaPolicy::Bt601,
        2 => LumaPolicy::Bt709,
        _ => LumaPolicy::Bt2020,
    };
    // The background and threshold are the two knobs whose *values* change results rather than
    // merely selecting a path, so they get a full 16 bits each from the engine.
    let background = [
        u16::from(b[3]) << 8 | u16::from(b[4]),
        u16::from(b[5]) << 8 | u16::from(b[6]),
        u16::from(b[7]) << 8 | u16::from(b[8]),
    ];
    let threshold = u16::from(b[9]) << 8 | u16::from(b[10]);
    ConvertPolicy::lossless()
        .with_alpha(alpha)
        .with_depth(depth)
        .with_luma(luma)
        .with_background(background)
        .with_threshold(threshold)
}

/// The header this target consumes before the sample bytes: format, dims, policy.
const HEADER: usize = 1 + 2 + 11;

fuzz_target!(|data: &[u8]| {
    if data.len() <= HEADER {
        return;
    }
    let format = NARROW_FORMATS[usize::from(data[0]) % NARROW_FORMATS.len()];
    // Normalising here rather than inside the laws is the same rule the property follows, and for
    // the same reason: a law that normalised its own inputs would feed the identical value to the
    // model and to the code under test, so a defect in the normaliser would cancel on both sides.
    let (width, height) = normalise_dims(u32::from(data[1]), u32::from(data[2]));
    let dims = Dimensions::new(width, height).expect("normalise_dims yields a non-zero edge");
    let policy = policy(&data[3..HEADER]);

    let samples = &data[HEADER..];
    // The laws that take two sample buffers need them the same length and layout, so the tail is
    // split in half rather than drawn twice.
    let (first, second) = samples.split_at(samples.len() / 2);

    // Law 1: whether a conversion is accepted depends on the layouts and the policy, never on the
    // sample values — the property that stops "rejects only the buffers we happened to test".
    if let Err(violation) = for_each_narrow_target!(
        acceptance_is_independent_of_the_samples,
        format,
        dims,
        first,
        &second[..first.len().min(second.len())],
        policy,
    ) {
        panic!("{violation}");
    }

    // Law 2: an accepted conversion's output has the source's dimensions under the target layout.
    if let Err(violation) = for_each_narrow_target!(
        output_shape_matches_the_target_layout,
        format,
        dims,
        first,
        policy
    ) {
        panic!("{violation}");
    }

    // Law 3: `Indexed8` and `Cmyk8` are closed — they convert only to themselves, at any policy.
    if let Err(violation) = for_each_narrow_target!(
        palette_and_cmyk_convert_only_to_themselves,
        format,
        dims,
        first,
        policy
    ) {
        panic!("{violation}");
    }

    // Law 4: the borrowing door and the allocating door agree. Two entry points, one conversion.
    if let Err(violation) = for_each_narrow_target!(
        the_in_place_door_matches_the_allocating_door,
        format,
        dims,
        first,
        policy
    ) {
        panic!("{violation}");
    }

    // Law 5: converting a layout to itself is the identity, even under a permissive policy.
    macro_rules! identity {
        ($($p:ty),* $(,)?) => {$(
            if let Err(violation) =
                converting_a_layout_to_itself_changes_nothing::<$p>(dims, first, policy)
            {
                panic!("{violation}");
            }
        )*};
    }
    identity!(Gray8, Bilevel, Indexed8, Rgb8, Rgba8, Cmyk8, GrayAlpha8);

    // Laws 6-9: the associated-alpha laws, for every alpha-carrying layout at both widths. They
    // say nothing about a buffer of the wrong length, so the sample bytes are cycled to exactly
    // the length `dims` needs rather than leaving most inputs vacuous; the 16-bit layouts read
    // them as big-endian pairs.
    macro_rules! associated_alpha {
        ($($p:ty => $read:expr),* $(,)?) => {$(
            let wanted = dims
                .sample_count(<$p as gamut_core::Pixel>::CHANNELS)
                .expect("normalise_dims bounds the sample count");
            let buffer: Vec<<$p as gamut_core::Pixel>::Sample> = $read(wanted);
            for result in [
                opaque_pixels_are_fixed_points::<$p>(dims, &buffer),
                premultiply_rounds_to_nearest::<$p>(dims, &buffer),
                unpremultiply_rounds_to_nearest_or_clamps::<$p>(dims, &buffer),
                premultiply_round_trip_is_bounded_and_idempotent::<$p>(dims, &buffer),
            ] {
                if let Err(violation) = result {
                    panic!("{violation}");
                }
            }
        )*};
    }
    let narrow = |wanted: usize| {
        samples
            .iter()
            .copied()
            .cycle()
            .take(wanted)
            .collect::<Vec<u8>>()
    };
    let wide = |wanted: usize| {
        let mut bytes = samples.iter().copied().cycle();
        (0..wanted)
            .map(|_| {
                let high = bytes.next().unwrap_or(0);
                u16::from(high) << 8 | u16::from(bytes.next().unwrap_or(0))
            })
            .collect::<Vec<u16>>()
    };
    associated_alpha!(Rgba8 => narrow, GrayAlpha8 => narrow, Rgba16 => wide, GrayAlpha16 => wide);
});
