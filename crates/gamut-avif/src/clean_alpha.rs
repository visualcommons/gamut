//! Clean colour under transparent pixels before a lossy encode (issue #657).
//!
//! A fully transparent pixel is invisible whatever colour it carries, but a lossy coder still
//! spends bits on that colour, and a sharp jump between a visible edge and the arbitrary colour
//! hidden behind it rings into the visible side. [`clean_rgba`] replaces the colour of every
//! non-opaque pixel with something smooth and close to its visible neighbours, under one rule:
//! **the premultiplied composite does not change.** Every pixel keeps its alpha, an opaque pixel
//! keeps its colour, and a semi-transparent pixel's new colour stays inside the interval of
//! colours that premultiply to the same value its old one did (`round(c·a / MAX)`). Only a fully
//! transparent pixel's colour is free.
//!
//! The steps, each over the 3×3 neighbourhood clipped to the image:
//!
//! 1. **Fallback colour.** The average colour of the visible pixels that border a transparent one,
//!    each weighted by `MAX + 1 − a` so the most transparent edge pixels — the ones whose colour is
//!    least pinned down — count most. Black when there are none.
//! 2. **Bleed.** Every non-opaque pixel takes the alpha-weighted average colour of its
//!    neighbourhood; where that neighbourhood is entirely transparent, the fallback colour. So
//!    visible colour bleeds one pixel outward and the interior of a transparent region is flat.
//! 3. **Blur.** Every pixel takes the unweighted average of the bled neighbourhood.
//! 4. **Clamp.** Each new colour is clamped to the interval that keeps its premultiplied value.
//!    For an opaque pixel that interval is the single original colour, which is how step 4 alone
//!    leaves opaque pixels untouched rather than a separate branch doing so.
//!
//! The averages truncate. Which colour a hidden pixel takes is the encoder's free choice — the
//! law above is the whole contract — so rounding them would buy nothing a reader can see.
//!
//! This is a clean-room implementation of the approach ravif names `UnassociatedClean`, written
//! from the description in issue #657; no code is shared with it. It applies to unassociated
//! alpha only: under premultiplied alpha the stored colour *is* the composite, so there is nothing
//! hidden to clean, and [`AvifEncoder`](crate::AvifEncoder) does not call it there.

/// A colour sample the cleaner reads and writes: 8-bit or 16-bit, on its full scale.
pub(crate) trait Sample: Copy {
    /// The sample value of a fully opaque alpha, and the top of the colour scale.
    const MAX: u32;
    /// The sample widened to `u32`.
    fn get(self) -> u32;
    /// A `u32` known to be `<= MAX`, narrowed back to the sample type.
    fn narrow(value: u32) -> Self;
}

impl Sample for u8 {
    const MAX: u32 = 0xFF;
    fn get(self) -> u32 {
        u32::from(self)
    }
    fn narrow(value: u32) -> Self {
        value as u8
    }
}

impl Sample for u16 {
    const MAX: u32 = 0xFFFF;
    fn get(self) -> u32 {
        u32::from(self)
    }
    fn narrow(value: u32) -> Self {
        value as u16
    }
}

/// The in-bounds rows (or columns) of the 3×3 neighbourhood centred on `i` along an axis of
/// length `len`.
fn window(i: usize, len: usize) -> core::ops::Range<usize> {
    i.saturating_sub(1)..(i + 2).min(len)
}

/// Returns a copy of the packed RGBA samples `rgba`, `width` pixels per row, with the colour of
/// every non-opaque pixel cleaned as the module documentation describes. Alpha is copied
/// unchanged, so the result composites exactly as `rgba` does.
///
/// `rgba.len()` is a whole number of rows of `4 * width` samples, as an
/// [`ImageRef`](gamut_core::ImageRef) guarantees.
pub(crate) fn clean_rgba<S: Sample>(rgba: &[S], width: usize) -> Vec<S> {
    let height = rgba.len() / (4 * width);
    let at = |x: usize, y: usize| -> &[S] {
        let i = (y * width + x) * 4;
        &rgba[i..i + 4]
    };

    // Step 1: the fallback colour. 64-bit sums: a 16-bit colour times a 16-bit weight is already
    // 32 bits before a single pixel has been added.
    let mut edge_sum = [0u64; 3];
    let mut edge_weight = 0u64;
    for y in 0..height {
        for x in 0..width {
            let px = at(x, y);
            let a = px[3].get();
            let borders_transparent =
                window(y, height).any(|ny| window(x, width).any(|nx| at(nx, ny)[3].get() == 0));
            if a > 0 && borders_transparent {
                let weight = u64::from(S::MAX + 1 - a);
                for (sum, c) in edge_sum.iter_mut().zip(px) {
                    *sum += u64::from(c.get()) * weight;
                }
                edge_weight += weight;
            }
        }
    }
    // With no edge pixel every sum is zero, so dividing by 1 instead yields black.
    let fallback = edge_sum.map(|sum| (sum / edge_weight.max(1)) as u32);

    // Step 2: bleed visible colour into the non-opaque pixels.
    let mut bled = Vec::with_capacity(width * height);
    for y in 0..height {
        for x in 0..width {
            let px = at(x, y);
            if px[3].get() == S::MAX {
                bled.push([px[0].get(), px[1].get(), px[2].get()]);
                continue;
            }
            let mut sum = [0u64; 3];
            let mut weight = 0u64;
            for ny in window(y, height) {
                for nx in window(x, width) {
                    let n = at(nx, ny);
                    let w = u64::from(n[3].get());
                    for (s, c) in sum.iter_mut().zip(n) {
                        *s += u64::from(c.get()) * w;
                    }
                    weight += w;
                }
            }
            bled.push(if weight == 0 {
                fallback
            } else {
                sum.map(|s| (s / weight) as u32)
            });
        }
    }

    // Steps 3 and 4: blur the bled colour, then clamp it back inside the premultiplied interval.
    let mut out = Vec::with_capacity(rgba.len());
    for y in 0..height {
        for x in 0..width {
            let mut sum = [0u32; 3];
            let mut count = 0u32;
            for ny in window(y, height) {
                for nx in window(x, width) {
                    for (s, c) in sum.iter_mut().zip(bled[ny * width + nx]) {
                        *s += c;
                    }
                    count += 1;
                }
            }
            let px = at(x, y);
            let a = px[3].get();
            for (s, c) in sum.iter().zip(px) {
                let (lo, hi) = premultiplied_interval::<S>(c.get(), a);
                out.push(S::narrow((s / count).clamp(lo, hi)));
            }
            out.push(px[3]);
        }
    }
    out
}

/// The closed interval of colours `c'` for which `round(c'·a / MAX)` equals `round(c·a / MAX)`:
/// the colours a pixel of alpha `a` can take without changing its premultiplied value. Every
/// colour when `a` is zero; exactly `c` when `a` is `MAX`.
///
/// With `MAX` odd, `c·a / MAX` is never exactly half-way, so `round(v / MAX)` is `(v + h) / MAX`
/// with `h = (MAX − 1) / 2`, and the premultiplied value `p` is kept exactly when
/// `p·MAX ≤ c'·a + h ≤ p·MAX + MAX − 1`.
fn premultiplied_interval<S: Sample>(c: u32, a: u32) -> (u32, u32) {
    if a == 0 {
        return (0, S::MAX);
    }
    let (c, a, max) = (u64::from(c), u64::from(a), u64::from(S::MAX));
    let h = max / 2;
    let p = (c * a + h) / max;
    let lo = (p * max).saturating_sub(h).div_ceil(a);
    let hi = ((p * max + max - 1 - h) / a).min(max);
    (lo as u32, hi as u32)
}

#[cfg(test)]
mod tests {
    use proptest::collection;
    use proptest::prelude::{Strategy, prop_assert_eq, proptest};
    use proptest::test_runner::{Config, RngSeed};

    use super::{Sample, clean_rgba, premultiplied_interval};
    use crate::invariants::{
        cleaning_8_bit_rgba_keeps_the_composite, cleaning_16_bit_rgba_keeps_the_composite,
        premultiply,
    };

    /// The workspace property-test configuration (`docs/testing.md`): the seed is pinned so the
    /// blocking mutation gate is reproducible, shrinking is bounded, and failure persistence is
    /// off because cargo-mutants reuses one tree across mutants.
    fn config() -> Config {
        Config {
            cases: 512,
            max_shrink_iters: 2048,
            max_shrink_time: 10_000,
            rng_seed: RngSeed::Fixed(0x6761_6D75_745F_6361),
            failure_persistence: None,
            ..Config::default()
        }
    }

    /// An image of `1..=6` × `1..=6` pixels whose alphas are drawn mostly from the three classes
    /// the cleaner treats differently — transparent, opaque, in between — so every image mixes
    /// them, and whose colours are arbitrary.
    fn image<S: Sample + core::fmt::Debug>(
        sample: impl Strategy<Value = S> + Clone,
        transparent: S,
        opaque: S,
    ) -> impl Strategy<Value = (usize, Vec<S>)> {
        (1usize..=6, 1usize..=6).prop_flat_map(move |(w, h)| {
            let alpha = proptest::prop_oneof![
                proptest::strategy::Just(transparent),
                proptest::strategy::Just(opaque),
                sample.clone(),
            ];
            let px = (sample.clone(), sample.clone(), sample.clone(), alpha)
                .prop_map(|(r, g, b, a)| [r, g, b, a]);
            collection::vec(px, w * h).prop_map(move |pxs| (w, pxs.concat()))
        })
    }

    proptest! {
        #![proptest_config(config())]

        #[test]
        fn clean_rgba_keeps_the_8_bit_composite((width, rgba) in image(0u8..=255, 0, 255)) {
            prop_assert_eq!(cleaning_8_bit_rgba_keeps_the_composite(width, &rgba), Ok(()));
        }

        #[test]
        fn clean_rgba_keeps_the_16_bit_composite(
            (width, rgba) in image(0u16..=u16::MAX, 0, u16::MAX)
        ) {
            prop_assert_eq!(cleaning_16_bit_rgba_keeps_the_composite(width, &rgba), Ok(()));
        }

        /// The interval is exact, not merely safe: one colour past either end premultiplies
        /// differently, so a clamp narrower than the law requires is caught as well as a wider
        /// one.
        #[test]
        fn the_premultiplied_interval_is_exactly_the_colours_that_keep_p(
            c in 0u32..=255, a in 1u32..=255
        ) {
            let (lo, hi) = premultiplied_interval::<u8>(c, a);
            let p = premultiply::<u8>(c, a);
            prop_assert_eq!(premultiply::<u8>(lo, a), p);
            prop_assert_eq!(premultiply::<u8>(hi, a), p);
            if lo > 0 {
                prop_assert_eq!(premultiply::<u8>(lo - 1, a) + 1, p);
            }
            if hi < 255 {
                prop_assert_eq!(premultiply::<u8>(hi + 1, a), p + 1);
            }
        }
    }

    #[test]
    fn a_transparent_pixel_may_take_any_colour_and_an_opaque_one_only_its_own() {
        assert_eq!(premultiplied_interval::<u8>(77, 0), (0, 255));
        assert_eq!(premultiplied_interval::<u16>(77, 0), (0, 0xFFFF));
        assert_eq!(premultiplied_interval::<u8>(77, 255), (77, 77));
        assert_eq!(premultiplied_interval::<u16>(77, 0xFFFF), (77, 77));
    }

    #[test]
    fn the_16_bit_interval_is_computed_on_the_16_bit_scale() {
        // At alpha 1 of 65535 every colour premultiplies to 0 or 1, the switch at c = 32768.
        assert_eq!(premultiplied_interval::<u16>(0, 1), (0, 32767));
        assert_eq!(premultiplied_interval::<u16>(65535, 1), (32768, 65535));
    }

    /// One row: an opaque red pixel, then two fully transparent pixels hiding green and blue.
    #[test]
    fn transparent_colour_is_replaced_by_the_bled_and_blurred_edge_colour() {
        let rgba: [u8; 12] = [200, 0, 0, 255, 0, 255, 0, 0, 0, 0, 255, 0];
        let out = clean_rgba(&rgba, 3);
        // Fallback: only the red pixel is visible and borders a transparent one -> (200, 0, 0).
        // Bleed: pixel 1 sees red (weight 255) -> (200, 0, 0); pixel 2's neighbourhood holds only
        // transparent pixels -> the fallback, (200, 0, 0).
        // Blur: pixel 1 averages three reds, pixel 2 two. All red; opaque pixel 0 is unchanged.
        assert_eq!(out, [200, 0, 0, 255, 200, 0, 0, 0, 200, 0, 0, 0]);
    }

    #[test]
    fn the_fallback_weights_the_most_transparent_edge_pixels_most() {
        // Row: [A 100,0,0 a=255] [T] [T] [T] [C 0,100,0 a=255]. A and C are the edge pixels, at
        // equal weight 1, so the fallback is (50, 50, 0); only pixel 2, whose window is entirely
        // transparent, bleeds it.
        let rgba: [u8; 20] = [
            100, 0, 0, 255, 9, 9, 9, 0, 9, 9, 9, 0, 9, 9, 9, 0, 0, 100, 0, 255,
        ];
        let out = clean_rgba(&rgba, 5);
        // Bled: A, A, (50,50,0), C, C. Blur at 2 over pixels 1..=3: (150/3, 150/3, 0).
        assert_eq!(&out[8..12], &[50, 50, 0, 0]);
        // Blur at 1 over pixels 0..=2: (250/3, 50/3, 0).
        assert_eq!(&out[4..8], &[83, 16, 0, 0]);

        // A at alpha 1 instead: edge weight 256 − 1 = 255 against C's 1, so the fallback leans to
        // A — (100·255 / 256, 100·1 / 256, 0) = (99, 0, 0). Pixel 1 still bleeds A's colour (A is
        // its only visible neighbour); blur at 2 = ((100 + 99 + 0) / 3, (0 + 0 + 100) / 3, 0).
        let mut rgba = rgba;
        rgba[3] = 1;
        let out = clean_rgba(&rgba, 5);
        assert_eq!(&out[8..12], &[66, 33, 0, 0]);
    }

    /// The row case at 16 bits: the same image on the 16-bit scale cleans to the same picture,
    /// so the 16-bit sample is read and written on its own scale.
    #[test]
    fn transparent_colour_is_replaced_on_the_16_bit_scale() {
        let rgba: [u16; 12] = [51400, 0, 0, 65535, 0, 65535, 0, 0, 0, 0, 65535, 0];
        assert_eq!(
            clean_rgba(&rgba, 3),
            [51400, 0, 0, 65535, 51400, 0, 0, 0, 51400, 0, 0, 0]
        );
    }

    #[test]
    fn only_visible_pixels_that_border_transparency_set_the_fallback() {
        // Row: [D 0,0,200 a=255] [A 100,0,0 a=255] [T] [T] [T]. D's window (D, A) holds no
        // transparent pixel, so the fallback is A's colour alone. Pixels 3 and 4 have all-
        // transparent windows, so both bleed it and pixel 4 blurs to it.
        let rgba: [u8; 20] = [
            0, 0, 200, 255, 100, 0, 0, 255, 9, 9, 9, 0, 9, 9, 9, 0, 9, 9, 9, 0,
        ];
        assert_eq!(&clean_rgba(&rgba, 5)[16..20], &[100, 0, 0, 0]);
    }

    #[test]
    fn the_blur_reads_the_neighbourhood_across_rows() {
        // 2×2: opaque red over opaque blue in column 0, transparent column 1. Both edge pixels
        // weigh 1, so the fallback — and both transparent pixels' bleed — is (100, 0, 100). The
        // blur at (1, 0) averages all four bled pixels: red, blue and two (100, 0, 100).
        let rgba: [u8; 16] = [200, 0, 0, 255, 9, 9, 9, 0, 0, 0, 200, 255, 9, 9, 9, 0];
        let out = clean_rgba(&rgba, 2);
        assert_eq!(&out[4..8], &[100, 0, 100, 0]);
        assert_eq!(&out[12..16], &[100, 0, 100, 0]);
    }

    #[test]
    fn a_fully_transparent_image_becomes_black() {
        let rgba: [u8; 16] = [1, 2, 3, 0, 4, 5, 6, 0, 7, 8, 9, 0, 10, 11, 12, 0];
        assert_eq!(clean_rgba(&rgba, 2), [0u8; 16]);
    }

    #[test]
    fn the_bleed_weights_neighbours_by_alpha() {
        // Row: [K 0,0,0 a=255] [T] [W 255,255,255 a=85]. Pixel 1 bleeds
        // (0·255 + 255·85) / (255 + 85) = 63; pixel 2's window (pixels 1..=2) sees only W, 255.
        let rgba: [u8; 12] = [0, 0, 0, 255, 50, 50, 50, 0, 255, 255, 255, 85];
        let out = clean_rgba(&rgba, 3);
        // Blur at 1: (0 + 63 + 255) / 3; transparent, so it stands.
        assert_eq!(&out[4..8], &[106, 106, 106, 0]);
        // Blur at 2: (63 + 255) / 2 = 159, clamped into [254, 255] — the colours that keep W's
        // premultiplied value of 85 at alpha 85.
        assert_eq!(&out[8..12], &[254, 254, 254, 85]);
    }

    #[test]
    fn the_neighbourhood_spans_rows_as_well_as_columns() {
        // A 1-wide column: opaque blue above two transparent pixels. Same shape as the row case,
        // transposed, so a window that only looked along rows would leave the hidden colour.
        let rgba: [u8; 12] = [0, 0, 200, 255, 0, 255, 0, 0, 255, 0, 0, 0];
        assert_eq!(
            clean_rgba(&rgba, 1),
            [0, 0, 200, 255, 0, 0, 200, 0, 0, 0, 200, 0]
        );
    }
}
