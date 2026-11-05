//! Shared integer arithmetic primitives for the gamut codecs.
//!
//! These are the small, algorithm-agnostic scalar operations that the codec crates would
//! otherwise each re-implement. Keeping them here gives every consumer one definition to
//! optimize. The first three are the AV1 §4.7 rounding/clamp operations (`Round2`,
//! `Round2Signed`, `Clip3`); [`round_div_nearest`] is the encoder's round-to-nearest divide
//! shared by the AV1 and VP8 forward quantizers; [`clip_pixel8`] / [`clip_pixel`] saturate a
//! reconstructed sample to its pixel range (AV1 `Clip1`).

/// `Round2(x, n)` (AV1 §4.7): rounding right shift, `x` for `n == 0` (arithmetic `>>`, ties up).
///
/// Total over its documented domain: `n` must be `< 64` and `x` must have headroom for the
/// `1 << (n - 1)` rounding bias; violations are arithmetic overflow (caught by Rust's debug
/// checks), not a checked precondition — this is the hottest primitive in the crate.
#[must_use]
pub fn round2(x: i64, n: u32) -> i64 {
    if n == 0 { x } else { (x + (1 << (n - 1))) >> n }
}

/// `Round2Signed(x, n)` (AV1 §4.7): symmetric rounding right shift — rounds the magnitude and
/// keeps the sign (ties away from zero), unlike [`round2`] which ties toward `+∞`.
///
/// `x` must have headroom for the `1 << (n - 1)` rounding bias (its magnitude, plus the bias,
/// must fit `i32`); violations are arithmetic overflow, caught by Rust's debug checks.
///
/// # Panics
/// Panics if `n == 0` (the rounding bias is `1 << (n - 1)`; the spec only invokes
/// `Round2Signed` with `n >= 1`).
#[must_use]
pub fn round2_signed(x: i32, n: u32) -> i32 {
    assert!(n >= 1, "round2_signed: n must be >= 1");
    if x >= 0 {
        (x + (1 << (n - 1))) >> n
    } else {
        -((-x + (1 << (n - 1))) >> n)
    }
}

/// `Clip3(low, high, x)` (AV1 §4.7): clamp `x` to the inclusive range `[low, high]`.
///
/// # Panics
/// Panics if `low > high` (via [`i64::clamp`]).
#[must_use]
pub fn clip3(low: i64, high: i64, x: i64) -> i64 {
    x.clamp(low, high)
}

/// Rounds `num / den` to the nearest integer, ties away from zero — the encoder forward-quantize
/// step shared by AV1 and VP8 (`level ≈ coeff / q`).
///
/// `num` must have headroom for the `den / 2` rounding bias (its magnitude, plus the bias, must
/// fit `i32`); violations are arithmetic overflow, caught by Rust's debug checks.
///
/// # Panics
/// Panics if `den <= 0` (a quantizer step is strictly positive; a negative `den` would silently
/// flip the rounding direction).
#[must_use]
pub fn round_div_nearest(num: i32, den: i32) -> i32 {
    assert!(den > 0, "round_div_nearest: den must be > 0");
    let half = den / 2;
    if num >= 0 {
        (num + half) / den
    } else {
        -((-num + half) / den)
    }
}

/// Saturates a computed integer sample to the unsigned 8-bit pixel range `0..=255`.
///
/// This is the single home for the `clip_pixel` / `clamp255` operation every codec performs when
/// writing a reconstructed sample (prediction + residual, loop-filter output, color conversion)
/// back to an 8-bit plane — one definition for later branchless/SIMD tuning. The `as u8` cast is
/// lossless because [`Ord::clamp`] has already constrained the value to `0..=255`.
#[must_use]
pub fn clip_pixel8(x: i32) -> u8 {
    x.clamp(0, 255) as u8
}

/// Saturates a computed integer sample to the unsigned `bit_depth`-bit pixel range
/// `0..=(1 << bit_depth) - 1` (AV1 `Clip1`, §3) — the high-bit-depth companion to [`clip_pixel8`].
///
/// `bit_depth` is 8, 10, 12, or 16; the result fits a `u16` for all of them. At `bit_depth == 8`
/// this equals `u16::from(clip_pixel8(x))`. AV1 itself only reaches 12 — 16 is here for the wider
/// still-image pipelines that share this vocabulary.
///
/// # Panics
///
/// Debug builds assert the `bit_depth` contract; release builds do not check it (this sits on
/// codec reconstruction hot paths), and depths outside 8/10/12/16 give meaningless results.
#[must_use]
pub fn clip_pixel(x: i32, bit_depth: u32) -> u16 {
    debug_assert!(
        matches!(bit_depth, 8 | 10 | 12 | 16),
        "clip_pixel bit_depth must be 8, 10, 12, or 16"
    );
    let max = (1i32 << bit_depth) - 1;
    x.clamp(0, max) as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clip_pixel_saturates_per_bit_depth() {
        // 8-bit: clip_pixel8 saturates at 0/255, and clip_pixel(x, 8) agrees with it exactly;
        // 10/12-bit use the wider ceilings.
        for &(x, expect8) in &[
            (-1000, 0u16),
            (-1, 0),
            (0, 0),
            (128, 128),
            (200, 200),
            (255, 255),
            (1000, 255),
        ] {
            assert_eq!(u16::from(clip_pixel8(x)), expect8);
            assert_eq!(clip_pixel(x, 8), expect8);
        }
        assert_eq!(clip_pixel(1023, 10), 1023);
        assert_eq!(clip_pixel(2000, 10), 1023); // saturates at 2^10 - 1
        assert_eq!(clip_pixel(-1, 10), 0);
        assert_eq!(clip_pixel(4095, 12), 4095);
        assert_eq!(clip_pixel(9000, 12), 4095); // saturates at 2^12 - 1
        assert_eq!(clip_pixel(65535, 16), 65535);
        assert_eq!(clip_pixel(100_000, 16), 65535); // saturates at 2^16 - 1, the u16 ceiling
        assert_eq!(clip_pixel(-1, 16), 0);
    }

    #[test]
    fn round2_matches_definition() {
        assert_eq!(round2(7, 0), 7);
        assert_eq!(round2(7, 1), 4); // (7 + 1) >> 1
        assert_eq!(round2(-1, 1), 0); // (-1 + 1) >> 1, ties toward +inf
        assert_eq!(round2(-3, 1), -1); // (-3 + 1) >> 1 = -1
        assert_eq!(round2(8, 2), 2); // (8 + 2) >> 2 = 2
    }

    #[test]
    fn round2_signed_rounds_magnitude_keeping_sign() {
        // Mirror image about zero: ties go away from zero in both directions.
        assert_eq!(round2_signed(7, 1), 4); // (7 + 1) >> 1
        assert_eq!(round2_signed(-7, 1), -4); // -((7 + 1) >> 1)
        assert_eq!(round2_signed(5, 2), 1); // (5 + 2) >> 2
        assert_eq!(round2_signed(-5, 2), -1);
        assert_eq!(round2_signed(0, 4), 0);
        // Differs from round2 on negative ties: round2(-1, 1) == 0, round2_signed(-1, 1) == -1.
        assert_eq!(round2_signed(-1, 1), -1);
        // A tie at n = 2 where the `1 << (n - 1)` rounding bias is load-bearing: without the +2
        // bias, 2 >> 2 == 0, so these pin the bias (and rule out `<< -> >>`).
        assert_eq!(round2_signed(2, 2), 1); // 0.5 -> 1
        assert_eq!(round2_signed(-2, 2), -1);
    }

    #[test]
    fn clip3_clamps_inclusive() {
        assert_eq!(clip3(0, 255, -5), 0);
        assert_eq!(clip3(0, 255, 0), 0);
        assert_eq!(clip3(0, 255, 128), 128);
        assert_eq!(clip3(0, 255, 255), 255);
        assert_eq!(clip3(0, 255, 1000), 255);
        assert_eq!(clip3(-128, 127, -200), -128);
    }

    #[test]
    #[should_panic(expected = "n must be >= 1")]
    fn round2_signed_zero_shift_panics() {
        let _ = round2_signed(1, 0);
    }

    #[test]
    #[should_panic(expected = "den must be > 0")]
    fn round_div_nearest_zero_den_panics() {
        let _ = round_div_nearest(1, 0);
    }

    #[test]
    #[should_panic(expected = "den must be > 0")]
    fn round_div_nearest_negative_den_panics() {
        let _ = round_div_nearest(1, -4);
    }

    #[test]
    fn round_div_nearest_ties_away_from_zero() {
        assert_eq!(round_div_nearest(10, 4), 3); // 2.5 -> 3
        assert_eq!(round_div_nearest(-10, 4), -3); // -2.5 -> -3
        assert_eq!(round_div_nearest(9, 4), 2); // 2.25 -> 2
        assert_eq!(round_div_nearest(-9, 4), -2);
        assert_eq!(round_div_nearest(0, 7), 0);
        assert_eq!(round_div_nearest(7, 7), 1);
        // Sign symmetry across a range.
        for num in -50..=50 {
            assert_eq!(round_div_nearest(-num, 6), -round_div_nearest(num, 6));
        }
    }
}
