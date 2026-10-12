//! fuzz · law — gamut-color's `Planar16::from_gray8_view` definition, searched rather than sampled
//! (#800).
//!
//! This is the *same* function `gamut_color::invariants` exposes to the pinned-seed `proptest`
//! property in `crates/gamut-color/src/invariants.rs`. Nothing is restated here: a law written
//! twice is a law that can disagree with itself, and both copies would keep passing.
//!
//! The law: `from_gray8_view` equals `from_gray16_view` applied to the lossless `Gray8 -> Gray16`
//! widening through `gamut_core::convert`, on every sample and on the geometry, subsampling and
//! depth. The property samples images of at most 8 × 8; this tier drives every width up to
//! `MAX_WIDTH` and as many rows as the input carries.
//!
//! A crash found here is **minimised and promoted into a named deterministic test** in
//! `gamut-color`'s own suite. The corpus is a search aid, not the regression record.

#![no_main]

use gamut_color::BitDepth;
use gamut_color::invariants::gray8_view_is_lossless_widening_then_gray16_view;
use gamut_core::{Dimensions, Gray8, ImageRef};
use libfuzzer_sys::fuzz_target;

/// The widest image row the driver builds, in pixels.
///
/// The law is per sample, so width only moves where row boundaries fall; 64 covers short and
/// long rows without spreading the engine's mutations over widths that add nothing new.
const MAX_WIDTH: usize = 64;

/// Every modelled depth, chosen by a selector byte.
const DEPTHS: [BitDepth; 4] = [
    BitDepth::Eight,
    BitDepth::Ten,
    BitDepth::Twelve,
    BitDepth::Sixteen,
];

fuzz_target!(|data: &[u8]| {
    // One depth byte, one width byte, then the grey samples.
    let [depth_byte, width_byte, samples @ ..] = data else {
        return;
    };
    let depth = DEPTHS[usize::from(*depth_byte) % DEPTHS.len()];
    let width = usize::from(*width_byte) % MAX_WIDTH + 1;

    // The law's input is a whole image; normalising the bytes to one is this driver's job.
    // Trailing samples that do not fill a row are dropped rather than rejected, so every input
    // with at least one row exercises the law.
    let height = samples.len() / width;
    if height == 0 {
        return;
    }
    let (Ok(w), Ok(h)) = (u32::try_from(width), u32::try_from(height)) else {
        return;
    };
    let Ok(dims) = Dimensions::new(w, h) else {
        return;
    };
    let Ok(img) = ImageRef::<Gray8>::new(&samples[..width * height], dims) else {
        return;
    };

    if let Err(violation) = gray8_view_is_lossless_widening_then_gray16_view(img, depth) {
        panic!("{violation}");
    }
});
