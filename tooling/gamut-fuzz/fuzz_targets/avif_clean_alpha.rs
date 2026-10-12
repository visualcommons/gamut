//! fuzz · law — gamut-avif's clean-alpha composite law, searched rather than sampled (#701).
//!
//! These are the *same* functions `gamut_avif::invariants` exposes to the pinned-seed `proptest`
//! properties in `crates/gamut-avif/src/clean_alpha.rs`. Nothing is restated here: a law written
//! twice is a law that can disagree with itself, and both copies would keep passing.
//!
//! The law: cleaning the colour under non-opaque pixels before a lossy encode leaves every pixel's
//! premultiplied composite unchanged. The property samples 6 × 6 images whose alpha is biased
//! towards fully transparent and fully opaque; this tier searches the whole byte space, where the
//! interesting cases — an alpha one step off either end, a semi-transparent pixel whose
//! premultiplied interval is a single colour — are sparse.
//!
//! A crash found here is **minimised and promoted into a named deterministic test** in
//! `gamut-avif`'s own suite. The corpus is a search aid, not the regression record.

#![no_main]

use gamut_avif::invariants::{
    cleaning_8_bit_rgba_keeps_the_composite, cleaning_16_bit_rgba_keeps_the_composite,
};
use libfuzzer_sys::fuzz_target;

/// The widest image row the driver builds, in pixels.
///
/// The cleaner's neighbourhood is 3 × 3, so every distinct shape of window clipping is reached by
/// a width of 3; 16 leaves room for runs of interior pixels without spreading the engine's
/// mutations over widths that add nothing new.
const MAX_WIDTH: usize = 16;

fuzz_target!(|data: &[u8]| {
    // One width byte, then the samples. Both laws see the same bytes: 8-bit as one sample per
    // byte, 16-bit as one little-endian sample per byte pair.
    let Some((&width_byte, samples)) = data.split_first() else {
        return;
    };
    let width = usize::from(width_byte) % MAX_WIDTH + 1;
    let row = 4 * width;

    // The laws' precondition is a whole number of rows; normalising to one is this driver's job.
    // Trailing samples that do not fill a row are dropped rather than rejected, so every input
    // with at least one row exercises the law.
    let rgba8 = &samples[..samples.len() / row * row];
    if !rgba8.is_empty() {
        if let Err(violation) = cleaning_8_bit_rgba_keeps_the_composite(width, rgba8) {
            panic!("8-bit: {violation}");
        }
    }

    let rgba16: Vec<u16> = samples
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    let rgba16 = &rgba16[..rgba16.len() / row * row];
    if !rgba16.is_empty() {
        if let Err(violation) = cleaning_16_bit_rgba_keeps_the_composite(width, rgba16) {
            panic!("16-bit: {violation}");
        }
    }
});
