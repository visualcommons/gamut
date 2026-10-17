//! Huffman coding for DEFLATE: bit-reversal, the fixed code tables (RFC 1951 §3.2.6), and the
//! length-limited canonical-code builder for dynamic blocks (§3.2.2).
//!
//! DEFLATE packs Huffman codes most-significant-bit-of-the-code first, but [`BitWriter`] is
//! LSB-first, so every code is bit-reversed via [`reverse_bits`] before emission. The length-limited
//! builder and canonical-code assignment are ported from `gamut-webp`'s VP8L prefix coder (the
//! algorithms are format-independent). Length-limiting uses a count-floor heuristic that lands within
//! ~1% of the entropy-optimal code; a package-merge limiter is an optional future refinement.
//!
//! [`BitWriter`]: crate::bitwriter::BitWriter

use crate::bitwriter::BitWriter;

/// Maximum DEFLATE Huffman code length, in bits (RFC 1951): 15 for the literal/length and distance
/// alphabets, and the cap the canonical builder assigns within.
const MAX_CODE_LEN: usize = 15;

/// Reverses the low `len` bits of `value`, discarding higher bits.
///
/// A Huffman code's canonical value is defined MSB-first; reversing its low `len` bits lets the
/// LSB-first [`BitWriter`](crate::bitwriter::BitWriter) emit it in the correct on-wire order.
pub(crate) fn reverse_bits(value: u32, len: u32) -> u32 {
    let mut v = value;
    let mut r = 0u32;
    for _ in 0..len {
        r = (r << 1) | (v & 1);
        v >>= 1;
    }
    r
}

/// The fixed Huffman code `(code, bit_length)` for a literal/length symbol (RFC 1951 §3.2.6).
///
/// Covers the whole literal/length alphabet 0..=287; callers pass only symbols that occur
/// (literals 0..=255, end-of-block 256, length symbols 257..=285). `code` is the canonical value
/// read MSB-first; pass it through [`reverse_bits`] before emitting.
pub(crate) fn fixed_litlen(sym: u16) -> (u32, u32) {
    match sym {
        0..=143 => (0x30 + u32::from(sym), 8),
        144..=255 => (0x190 + (u32::from(sym) - 144), 9),
        256..=279 => (u32::from(sym) - 256, 7),
        _ => (0xC0 + (u32::from(sym) - 280), 8), // 280..=287
    }
}

/// The fixed Huffman code `(code, bit_length)` for a distance symbol 0..=29: a 5-bit code equal to
/// the symbol number (RFC 1951 §3.2.6). MSB-first; reverse before emitting.
pub(crate) fn fixed_distance(sym: u16) -> (u32, u32) {
    (u32::from(sym), 5)
}

/// A canonical Huffman code: per-symbol code lengths plus the matching emit-ready (bit-reversed)
/// codes.
pub(crate) struct CanonicalCode {
    lengths: Vec<u8>,
    codes: Vec<u16>,
}

impl CanonicalCode {
    /// Builds a length-limited (`<= max_len`) canonical code from a symbol `histogram`.
    pub(crate) fn from_histogram(histogram: &[u32], max_len: u8) -> Self {
        let lengths = build_lengths(histogram, max_len);
        let codes = canonical_codes(&lengths);
        Self { lengths, codes }
    }

    /// Per-symbol code lengths (`0` = symbol unused / absent from the code).
    pub(crate) fn lengths(&self) -> &[u8] {
        &self.lengths
    }

    /// Emits the code for `sym` (nothing if `sym` is unused, i.e. has length 0).
    ///
    /// An unused symbol needs no guard of its own: [`BitWriter::write_bits`] writes nothing for a
    /// zero width. A `len > 0` guard here was an unkillable `>=` twin of that contract (#493).
    pub(crate) fn emit(&self, w: &mut BitWriter, sym: usize) {
        w.write_bits(u32::from(self.codes[sym]), u32::from(self.lengths[sym]));
    }
}

/// Derives the canonical, emit-ready (bit-reversed) code for each symbol from its `lengths`.
///
/// Unused symbols (length 0) get code 0. Implements the RFC 1951 §3.2.2 `bl_count`/`next_code`
/// assignment.
fn canonical_codes(lengths: &[u8]) -> Vec<u16> {
    let mut bl_count = [0u32; MAX_CODE_LEN + 1];
    for &len in lengths {
        if len > 0 && usize::from(len) <= MAX_CODE_LEN {
            bl_count[usize::from(len)] += 1;
        }
    }
    let mut next_code = [0u32; MAX_CODE_LEN + 1];
    let mut code = 0u32;
    for bits in 1..=MAX_CODE_LEN {
        code = (code + bl_count[bits - 1]) << 1;
        next_code[bits] = code;
    }
    let mut codes = vec![0u16; lengths.len()];
    for (sym, &len) in lengths.iter().enumerate() {
        if len > 0 && usize::from(len) <= MAX_CODE_LEN {
            let c = next_code[usize::from(len)];
            next_code[usize::from(len)] += 1;
            codes[sym] = reverse_bits(c, u32::from(len)) as u16;
        }
    }
    codes
}

/// Builds length-limited (`<= max_len`) canonical Huffman code lengths from a symbol `histogram`.
///
/// Returns one length per symbol (`0` = unused). A single nonzero symbol gets length 1. The maximum
/// length is bounded by raising the counts toward a common floor and rebuilding until the tree fits
/// (libwebp's approach) — a valid code within ~1% of the entropy-optimal length-limited code; a
/// package-merge limiter would close that last fraction of a percent but is not required.
///
/// An empty or single-symbol histogram needs no case of its own: [`huffman_pass`] already returns
/// all zeros for the first and a lone length 1 for the second, both within any `max_len >= 1`. The
/// duplicate early returns that used to stand here were where the survivor of #493 lived — their
/// `histogram[i] > 0` search could be weakened to `>=` because every caller that reaches a single
/// symbol either forces a second one first (`ensure_two`) or reads the result only as a cost.
fn build_lengths(histogram: &[u32], max_len: u8) -> Vec<u8> {
    let mut count_min = 1u32;
    loop {
        let depths = huffman_pass(histogram, count_min);
        let max_depth = depths.iter().copied().max().unwrap_or(0);
        if max_depth <= u32::from(max_len) {
            return depths.iter().map(|&d| d as u8).collect();
        }
        count_min = count_min.saturating_mul(2);
    }
}

/// One Huffman construction pass; returns per-symbol depths with each present symbol's weight floored
/// to `count_min` (raising the floor flattens the tree, capping its depth). Deterministic tie-breaks.
fn huffman_pass(histogram: &[u32], count_min: u32) -> Vec<u32> {
    use std::cmp::Reverse;
    use std::collections::BinaryHeap;

    /// A node in the Huffman tree (`sym >= 0` marks a leaf).
    struct Node {
        left: i32,
        right: i32,
        sym: i32,
    }

    let n = histogram.len();
    let mut lengths = vec![0u32; n];
    let mut nodes: Vec<Node> = Vec::new();
    let mut heap: BinaryHeap<Reverse<(u64, usize)>> = BinaryHeap::new();
    for (sym, &count) in histogram.iter().enumerate() {
        if count > 0 {
            let idx = nodes.len();
            nodes.push(Node {
                left: -1,
                right: -1,
                sym: sym as i32,
            });
            heap.push(Reverse((u64::from(count.max(count_min)), idx)));
        }
    }
    if nodes.len() == 1 {
        if let Some(sym) = (0..n).find(|&i| histogram[i] > 0) {
            lengths[sym] = 1;
        }
        return lengths;
    }
    while heap.len() > 1 {
        let (Some(Reverse((wa, a))), Some(Reverse((wb, b)))) = (heap.pop(), heap.pop()) else {
            break;
        };
        let idx = nodes.len();
        nodes.push(Node {
            left: a as i32,
            right: b as i32,
            sym: -1,
        });
        heap.push(Reverse((wa + wb, idx)));
    }
    let Some(Reverse((_, root))) = heap.pop() else {
        return lengths;
    };
    let mut stack = vec![(root, 0u32)];
    while let Some((idx, depth)) = stack.pop() {
        let Some(node) = nodes.get(idx) else { continue };
        if node.sym >= 0 {
            if let Some(slot) = lengths.get_mut(node.sym as usize) {
                *slot = depth;
            }
        } else {
            stack.push((node.left as usize, depth + 1));
            stack.push((node.right as usize, depth + 1));
        }
    }
    lengths
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reverse_bits_examples() {
        assert_eq!(reverse_bits(0b001, 3), 0b100);
        assert_eq!(reverse_bits(0b1011, 4), 0b1101);
        assert_eq!(reverse_bits(0, 7), 0);
        assert_eq!(reverse_bits(0b1, 1), 0b1);
        assert_eq!(reverse_bits(0b1_0000_0001, 1), 0b1);
    }

    #[test]
    fn fixed_litlen_boundaries() {
        assert_eq!(fixed_litlen(0), (0x30, 8));
        assert_eq!(fixed_litlen(143), (0xBF, 8));
        assert_eq!(fixed_litlen(144), (0x190, 9));
        assert_eq!(fixed_litlen(255), (0x1FF, 9));
        assert_eq!(fixed_litlen(256), (0, 7));
        assert_eq!(fixed_litlen(279), (23, 7));
        assert_eq!(fixed_litlen(280), (0xC0, 8));
        assert_eq!(fixed_litlen(285), (0xC5, 8));
    }

    #[test]
    fn fixed_distance_is_five_bit_identity() {
        assert_eq!(fixed_distance(0), (0, 5));
        assert_eq!(fixed_distance(29), (29, 5));
    }

    #[test]
    fn build_lengths_caps_at_limit() {
        // A Fibonacci distribution drives natural Huffman depth well past 15; the limiter must cap.
        let mut hist = vec![0u32; 64];
        let (mut a, mut b) = (1u32, 1u32);
        for h in hist.iter_mut() {
            *h = a;
            let next = a.saturating_add(b);
            a = b;
            b = next;
        }
        let lengths = build_lengths(&hist, 15);
        assert!(lengths.iter().all(|&l| l <= 15));
        // Lengths must satisfy the Kraft equality for a complete code.
        let kraft: u64 = lengths
            .iter()
            .filter(|&&l| l > 0)
            .map(|&l| 1u64 << (15 - l))
            .sum();
        assert_eq!(kraft, 1u64 << 15);
    }

    #[test]
    fn single_symbol_gets_length_one() {
        let mut hist = vec![0u32; 30];
        hist[7] = 42;
        let lengths = build_lengths(&hist, 15);
        assert_eq!(lengths[7], 1);
        assert!(
            lengths
                .iter()
                .enumerate()
                .all(|(i, &l)| (i == 7) == (l > 0))
        );
    }

    #[test]
    fn canonical_codes_are_prefix_free_lengths() {
        // Two length-1 codes -> codes 0 and 1 (reversed 1-bit are themselves).
        let codes = canonical_codes(&[1, 1]);
        assert_ne!(codes[0], codes[1]);
    }

    #[test]
    fn from_histogram_builds_usable_code() {
        let hist = [3u32, 0, 5];
        let code = CanonicalCode::from_histogram(&hist, 15);
        // Two used symbols -> both length 1; the unused symbol stays length 0.
        assert_eq!(code.lengths(), &[1, 0, 1]);
    }
}
