//! Integer bit-cost tables for VP8 encoder decisions (RFC 6386 §7.3).
//!
//! Every encoder decision that weighs "is this worth the bits" needs the cost of coding a bool at a
//! given probability. That cost is `-log2(p / 256)` bits, which is a logarithm — and **no encoder
//! decision in this crate is allowed to touch floating point**. A float would make the encoder's
//! output depend on the target's FMA contraction, x87 excess precision, and the optimiser's
//! reassociation, so the same input could produce different bytes on different machines. The
//! ladder's determinism test would surface that as flakiness rather than as the portability bug it
//! is.
//!
//! So the logarithm is precomputed into a table, in units of **1/256 of a bit**, and every cost is
//! integer arithmetic from there.

/// Cost of coding the *zero* branch at probability `p`, in 1/256 of a bit: `-256 * log2(p / 256)`,
/// rounded to nearest. Index 0 is unused — probability 0 cannot be coded.
#[rustfmt::skip]
const BIT_COST: [u16; 256] = [
    0, 2048, 1792, 1642, 1536, 1454, 1386, 1329,
    1280, 1236, 1198, 1162, 1130, 1101, 1073, 1048,
    1024, 1002, 980, 961, 942, 924, 906, 890,
    874, 859, 845, 831, 817, 804, 792, 780,
    768, 757, 746, 735, 724, 714, 705, 695,
    686, 676, 668, 659, 650, 642, 634, 626,
    618, 611, 603, 596, 589, 582, 575, 568,
    561, 555, 548, 542, 536, 530, 524, 518,
    512, 506, 501, 495, 490, 484, 479, 474,
    468, 463, 458, 453, 449, 444, 439, 434,
    430, 425, 420, 416, 412, 407, 403, 399,
    394, 390, 386, 382, 378, 374, 370, 366,
    362, 358, 355, 351, 347, 343, 340, 336,
    333, 329, 326, 322, 319, 315, 312, 309,
    305, 302, 299, 296, 292, 289, 286, 283,
    280, 277, 274, 271, 268, 265, 262, 259,
    256, 253, 250, 247, 245, 242, 239, 236,
    234, 231, 228, 226, 223, 220, 218, 215,
    212, 210, 207, 205, 202, 200, 197, 195,
    193, 190, 188, 185, 183, 181, 178, 176,
    174, 171, 169, 167, 164, 162, 160, 158,
    156, 153, 151, 149, 147, 145, 143, 140,
    138, 136, 134, 132, 130, 128, 126, 124,
    122, 120, 118, 116, 114, 112, 110, 108,
    106, 104, 102, 101, 99, 97, 95, 93,
    91, 89, 87, 86, 84, 82, 80, 78,
    77, 75, 73, 71, 70, 68, 66, 64,
    63, 61, 59, 58, 56, 54, 53, 51,
    49, 48, 46, 44, 43, 41, 40, 38,
    36, 35, 33, 32, 30, 28, 27, 25,
    24, 22, 21, 19, 18, 16, 15, 13,
    12, 10, 9, 7, 6, 4, 3, 1,
];

/// The cost, in 1/256 of a bit, of coding `bit` at probability `p`.
///
/// `p` is the probability of the **zero** branch, as everywhere in RFC 6386, so the one branch
/// costs what `256 - p` would cost as a zero. `p` is clamped into `1..=255` because a probability
/// at either extreme cannot code the opposite branch at all.
#[must_use]
pub fn bit_cost(bit: bool, p: u8) -> u32 {
    let p = u32::from(p).clamp(1, 255);
    let chance = if bit { 256 - p } else { p };
    u32::from(BIT_COST[chance as usize])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_matches_the_logarithm_it_stands_in_for() {
        // The table exists only so no encoder decision touches floating point. The float reference
        // is therefore confined to this test, where reproducibility does not matter.
        for (p, &entry) in BIT_COST.iter().enumerate().skip(1) {
            let want = -256.0 * (p as f64 / 256.0).log2();
            let got = f64::from(entry);
            assert!(
                (got - want).abs() <= 0.5,
                "BIT_COST[{p}] = {got}, want {want}"
            );
        }
    }

    #[test]
    fn costs_are_symmetric_and_anchored() {
        // An even probability costs exactly one bit either way; the two branches are mirror images.
        assert_eq!(bit_cost(false, 128), 256);
        assert_eq!(bit_cost(true, 128), 256);
        for p in 1..=255u8 {
            assert_eq!(
                bit_cost(true, p),
                bit_cost(false, 255 - p + 1),
                "asymmetry at p = {p}"
            );
        }
        // A near-certain branch is nearly free; its complement is expensive.
        assert!(bit_cost(false, 255) <= 2);
        assert!(bit_cost(true, 255) >= 2000);
    }

    #[test]
    fn cost_falls_as_the_branch_becomes_more_likely() {
        // Monotonicity is what makes the adopt/reject comparison meaningful; a table typo would
        // most likely break it here.
        for p in 2..=255u8 {
            assert!(
                bit_cost(false, p) <= bit_cost(false, p - 1),
                "cost rose from p = {} to p = {p}",
                p - 1
            );
        }
    }

    #[test]
    fn extremes_are_clamped_rather_than_indexing_out_of_range() {
        // Probability 0 is not codable; clamping keeps a stray value from panicking the encoder.
        assert_eq!(bit_cost(false, 0), bit_cost(false, 1));
        assert_eq!(bit_cost(true, 0), bit_cost(true, 1));
    }
}
