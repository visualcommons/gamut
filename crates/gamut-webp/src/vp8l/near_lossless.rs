//! Near-lossless preprocessing for the VP8L path (issue #261).
//!
//! Near-lossless is not a bitstream feature — nothing here changes what VP8L can express. It is a
//! **deliberate, bounded quantization of the source pixels applied before lossless coding**, so the
//! coded stream still reproduces its input bit-exactly; that input is simply a quantized copy of
//! the caller's image. The point is that zeroing the low bits of smooth regions gives the spatial
//! predictors and the entropy coder far less residual to carry, for an error the eye does not see.
//!
//! # The rule
//!
//! Every colour channel is rounded to the nearest multiple of `2^bits`. Uniformly — not selectively.
//!
//! That is worth explaining, because the selective version is the intuitive one and it was tried
//! first: quantize only where the neighbourhood is busy, so the error hides in texture. It was
//! measured **worse than not quantizing at all** on noisy photographic content, and the reason is
//! specific to a *predictive* coder. VP8L codes each pixel as a residual against its spatial
//! prediction. When only some pixels in a region snap to the grid, neighbours end up on different
//! grids, and the residual across every such boundary is arbitrary — so the quantization destroys
//! more predictability than it removes detail. A uniform grid has the opposite effect: every
//! residual within a region becomes a multiple of the step, which is exactly what shrinks the
//! residual alphabet.
//!
//! Measured on a 200x150 gradient-plus-noise image, uniform quantization gives 42134 bytes exact,
//! 30192 at 3 bits, 15566 at 4 and 10926 at 5; the selective variant *grew* the file at every
//! strength.
//!
//! The trade this makes is the one near-lossless always makes: a strong setting will band a smooth
//! gradient. That is what the strength knob is for. And because a gentle setting can occasionally
//! cost a few bytes rather than save them, the encoder codes the image both ways and keeps the
//! smaller — so turning the knob on can never inflate a file (see
//! [`WebpEncoder`](crate::WebpEncoder)).
//!
//! # What is guaranteed
//!
//! - **Red, green and blue** move by at most half the quantization step — `1`, `2`, `4`, `8`, `16`
//!   for strengths mapping to 1..=5 bits.
//! - **Alpha is never modified**, so the crate's promise that alpha round-trips bit-exactly still
//!   holds and masks stay usable. libwebp quantizes all four channels.
//! - `bits == 0` is the identity, byte-for-byte.
//!
//! # Relationship to libwebp
//!
//! The strength **scale** is libwebp's (`near_lossless` `0..=100`, `100` = off, mapping to
//! `5 - strength / 20` bits) so a caller migrating from `cwebp` gets what they expect. The
//! quantization rule and its error bound are this crate's own and are stated above rather than
//! inherited; libwebp additionally quantizes alpha and skips the pass entirely on small images,
//! neither of which is done here. Output is therefore **not** byte-identical to libwebp's, so the
//! differential test asserts the bound rather than the bytes.

use crate::vp8l::transform::{alpha, blue, green, make_argb, red};

/// Rounds every colour channel of `argb` to the nearest multiple of `2^bits`, leaving alpha exact.
///
/// `bits` is the quantization depth from [`NearLossless::bits`](crate::NearLossless::bits); `0` is
/// the identity.
#[must_use]
pub(crate) fn apply(argb: &[u32], bits: u8) -> Vec<u32> {
    if bits == 0 {
        return argb.to_vec();
    }
    argb.iter().map(|&p| quantize_pixel(p, bits)).collect()
}

/// Rounds each of a pixel's RGB channels to the nearest multiple of `2^depth`, saturating at 255.
/// Alpha is copied through untouched.
fn quantize_pixel(pixel: u32, depth: u8) -> u32 {
    make_argb(
        alpha(pixel),
        quantize_channel(red(pixel), depth),
        quantize_channel(green(pixel), depth),
        quantize_channel(blue(pixel), depth),
    )
}

/// Rounds `value` to the nearest multiple of `2^depth` (halves up), saturating at 255 so the result
/// stays an 8-bit sample. The deviation is therefore at most `2^(depth - 1)`.
fn quantize_channel(value: u8, depth: u8) -> u8 {
    let step = 1u32 << depth;
    let rounded = ((u32::from(value) + step / 2) >> depth) << depth;
    rounded.min(255) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_bits_is_the_identity() {
        // `None` near-lossless must cost nothing and change nothing; this is what lets the encoder
        // call `apply` unconditionally.
        let pixels: Vec<u32> = (0..64)
            .map(|i| make_argb(i as u8, 3, i as u8, 200))
            .collect();
        assert_eq!(apply(&pixels, 0), pixels);
    }

    #[test]
    fn rgb_stays_within_the_bound_and_alpha_is_exact() {
        // The two halves of the public contract.
        let pixels: Vec<u32> = (0..32u32 * 32)
            .map(|i| {
                let (x, y) = (i % 32, i / 32);
                make_argb((i % 256) as u8, (x * 8) as u8, (y * 8) as u8, (x + y) as u8)
            })
            .collect();
        for bits in 1..=5u8 {
            let out = apply(&pixels, bits);
            let bound = 1u16 << (bits - 1);
            for (before, after) in pixels.iter().zip(&out) {
                assert_eq!(alpha(*before), alpha(*after), "alpha must never move");
                for (a, b) in [
                    (red(*before), red(*after)),
                    (green(*before), green(*after)),
                    (blue(*before), blue(*after)),
                ] {
                    assert!(
                        u16::from(a.abs_diff(b)) <= bound,
                        "bits {bits}: channel moved {}, bound {bound}",
                        a.abs_diff(b)
                    );
                }
            }
        }
    }

    #[test]
    fn every_channel_lands_on_the_grid() {
        // The property the whole design rests on: a *uniform* grid, so that every prediction
        // residual within a region is a multiple of the step. A selective rule would leave some
        // pixels off-grid, which is what made it lose to not quantizing at all.
        let pixels: Vec<u32> = (0..40u32 * 30)
            .map(|i| {
                let v = ((i * 37) % 256) as u8;
                make_argb(0x80, v, v.wrapping_add(11), v.wrapping_mul(3))
            })
            .collect();
        for bits in 1..=5u8 {
            let step = 1u16 << bits;
            for pixel in apply(&pixels, bits) {
                for channel in [red(pixel), green(pixel), blue(pixel)] {
                    // 255 is the one off-grid value allowed: saturation, so the result stays 8-bit.
                    assert!(
                        u16::from(channel) % step == 0 || channel == 255,
                        "bits {bits}: {channel} is not on the grid"
                    );
                }
            }
        }
    }

    #[test]
    fn quantization_is_a_pure_pointwise_function() {
        // Each pixel is quantized from its own value alone, so the result cannot depend on scan
        // order, neighbours, or image shape — and re-applying is a no-op.
        let pixels: Vec<u32> = (0..24u32 * 18)
            .map(|i| {
                let v = ((i * 61) % 256) as u8;
                make_argb(0xff, v, v.wrapping_add(17), v.wrapping_mul(5))
            })
            .collect();
        let once = apply(&pixels, 4);
        assert_eq!(apply(&once, 4), once, "re-applying must not drift");
        let reversed: Vec<u32> = pixels.iter().rev().copied().collect();
        let via_reverse: Vec<u32> = apply(&reversed, 4).into_iter().rev().collect();
        assert_eq!(once, via_reverse, "the result depends on order");
        assert_ne!(once, pixels, "the fixture must actually be quantized");
    }

    #[test]
    fn channel_quantization_rounds_to_the_nearest_step() {
        // Pins the rounding rule and the saturation, which the error bound is derived from.
        assert_eq!(quantize_channel(0, 3), 0);
        assert_eq!(quantize_channel(3, 3), 0); // 3 rounds down to 0
        assert_eq!(quantize_channel(4, 3), 8); // halves round up
        assert_eq!(quantize_channel(12, 3), 16);
        assert_eq!(quantize_channel(255, 3), 255); // saturates rather than wrapping to 256
        assert_eq!(quantize_channel(255, 5), 255);
        assert_eq!(quantize_channel(200, 1), 200);
        for depth in 1..=5u8 {
            for value in 0..=255u8 {
                let out = quantize_channel(value, depth);
                assert!(
                    u16::from(value.abs_diff(out)) <= 1 << (depth - 1),
                    "depth {depth} moved {value} to {out}"
                );
            }
        }
    }

    #[test]
    fn degenerate_buffers_are_handled() {
        assert!(apply(&[], 3).is_empty());
        assert_eq!(apply(&[make_argb(0xff, 100, 100, 100)], 3).len(), 1);
    }
}
