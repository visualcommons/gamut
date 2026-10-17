//! DEFLATE length and distance symbol tables (RFC 1951 §3.2.5).
//!
//! A back-reference `(length, distance)` is coded as a Huffman symbol plus a fixed number of raw
//! "extra" bits. Lengths 3..=258 map to literal/length symbols 257..=285; distances 1..=32768 map to
//! distance symbols 0..=29. These tables and the [`length_code`] / [`distance_code`] lookups are the
//! shared front end for both the fixed- and dynamic-Huffman block writers.

/// Base length for each length symbol 257..=285 (index `sym - 257`).
const LENGTH_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
/// Extra bits for each length symbol 257..=285.
const LENGTH_EXTRA: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];

/// Base distance for each distance symbol 0..=29.
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
/// Extra bits for each distance symbol 0..=29.
const DIST_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];

/// The smallest and largest back-reference lengths DEFLATE can code.
pub(crate) const MIN_MATCH: usize = 3;
/// The largest back-reference length DEFLATE can code.
pub(crate) const MAX_MATCH: usize = 258;
/// The largest back-reference distance DEFLATE can code (the 32 KiB window).
pub(crate) const MAX_DISTANCE: usize = 32768;

/// Maps a match `length` (3..=258) to `(symbol, extra_bits, extra_value)`.
///
/// `symbol` is in 257..=285. The caller emits the Huffman code for `symbol`, then `extra_value` as
/// `extra_bits` raw LSB-first bits.
pub(crate) fn length_code(length: u16) -> (u16, u32, u32) {
    debug_assert!((MIN_MATCH as u16..=MAX_MATCH as u16).contains(&length));
    let idx = last_base_at_or_below(&LENGTH_BASE, length);
    (
        257 + idx as u16,
        u32::from(LENGTH_EXTRA[idx]),
        u32::from(length - LENGTH_BASE[idx]),
    )
}

/// Maps a match `distance` (1..=32768) to `(symbol, extra_bits, extra_value)`.
///
/// `symbol` is in 0..=29. The caller emits the Huffman code for `symbol`, then `extra_value` as
/// `extra_bits` raw LSB-first bits.
pub(crate) fn distance_code(distance: u16) -> (u16, u32, u32) {
    debug_assert!((1..=MAX_DISTANCE as u32).contains(&u32::from(distance)));
    let idx = last_base_at_or_below(&DIST_BASE, distance);
    (
        idx as u16,
        u32::from(DIST_EXTRA[idx]),
        u32::from(distance - DIST_BASE[idx]),
    )
}

/// Index of the last entry of the ascending `bases` that is `<= value`.
///
/// `value` must be at least `bases[0]`, which both callers' domains guarantee (`LENGTH_BASE[0]` is
/// `MIN_MATCH`, `DIST_BASE[0]` is 1). A binary search with no cursor: the linear
/// `while … { idx += 1 }` scan it replaced stalled forever under an `idx *= 1` mutant, scorable only
/// as a TIMEOUT (#850).
fn last_base_at_or_below(bases: &[u16], value: u16) -> usize {
    bases.partition_point(|&base| base <= value) - 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn length_codes_are_self_consistent() {
        for length in MIN_MATCH as u16..=MAX_MATCH as u16 {
            let (sym, extra_bits, extra_val) = length_code(length);
            assert!((257..=285).contains(&sym), "length {length} -> sym {sym}");
            assert!(extra_bits <= 5);
            assert!(extra_val < (1u32 << extra_bits), "extra value out of range");
            // The symbol's base plus the extra value must reconstruct the length exactly.
            assert_eq!(
                u32::from(LENGTH_BASE[(sym - 257) as usize]) + extra_val,
                u32::from(length)
            );
        }
    }

    #[test]
    fn distance_codes_are_self_consistent() {
        for distance in 1..=MAX_DISTANCE as u32 {
            let d = distance as u16;
            let (sym, extra_bits, extra_val) = distance_code(d);
            assert!(sym <= 29, "distance {distance} -> sym {sym}");
            assert!(extra_bits <= 13);
            assert!(extra_val < (1u32 << extra_bits), "extra value out of range");
            assert_eq!(
                u32::from(DIST_BASE[sym as usize]) + extra_val,
                distance,
                "distance {distance}"
            );
        }
    }

    #[test]
    fn spec_boundary_examples() {
        assert_eq!(length_code(3), (257, 0, 0));
        assert_eq!(length_code(258), (285, 0, 0));
        assert_eq!(length_code(257), (284, 5, 30)); // 227 + 30
        assert_eq!(length_code(11), (265, 1, 0));
        assert_eq!(distance_code(1), (0, 0, 0));
        assert_eq!(distance_code(32768), (29, 13, 32768 - 24577));
        assert_eq!(distance_code(5), (4, 1, 0));
    }
}
