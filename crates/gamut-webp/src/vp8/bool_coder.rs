//! VP8 boolean entropy coder (RFC 6386 §7) and tree coding (§8).
//!
//! VP8 codes every header field and coefficient token with a binary arithmetic coder driven by
//! 8-bit probabilities `p` (the represented probability of a `0` is `p/256`) — a binary arithmetic
//! coder, distinct from AV1's multi-symbol range coder. [`BoolEncoder`] writes the compressed partitions
//! and [`BoolDecoder`] reads them; the two are exact inverses, so a decode of any encode reproduces
//! the original bools (the tier-1 round-trip oracle). The byte-exact agreement of this coder with
//! libwebp is locked transitively once whole VP8 frames are cross-checked against libwebp (P7).
//!
//! The implementation mirrors the reference C in RFC 6386 §7.3 (interval `bottom`/`range`,
//! byte-at-a-time renormalization, deferred carry propagation) and §8.1 (array-encoded trees).
//! Tracked in `../STATUS.md` section G.

/// An 8-bit node probability: the chance (out of 256) that the coded bool is `0`.
pub type Prob = u8;

/// A tree specification: an array of `i8` branch entries (RFC 6386 §8.1).
///
/// Each even index is an interior node; entry `i` and `i + 1` are its `0` (left) and `1` (right)
/// branches. A positive entry is the index of a deeper interior node; a non-positive entry `v` is a
/// leaf whose value is `-v`. The associated interior-node probabilities are indexed by `i >> 1`.
pub type Tree = [i8];

/// VP8 boolean entropy **encoder** (RFC 6386 §7.3).
///
/// Construct with [`BoolEncoder::new`], write bools/literals/tree symbols, then call
/// [`BoolEncoder::finish`] exactly once to flush the interval and obtain the partition bytes.
#[derive(Debug, Clone)]
pub struct BoolEncoder {
    /// Compressed output bytes written so far (carries propagate backward into these).
    output: Vec<u8>,
    /// Width of the current coding interval, kept in `128..=255` between bools.
    range: u32,
    /// Low end of the current coding interval (the value being built, high bits pending output).
    bottom: u32,
    /// Number of left-shifts remaining before the next output byte is available.
    bit_count: i32,
}

impl Default for BoolEncoder {
    fn default() -> Self {
        Self::new()
    }
}

impl BoolEncoder {
    /// Creates an encoder with the initial interval state (`range = 255`, `bottom = 0`).
    #[must_use]
    pub fn new() -> Self {
        Self {
            output: Vec::new(),
            range: 255,
            bottom: 0,
            bit_count: 24,
        }
    }

    /// Propagates a carry into the already-written output, per `add_one_to_output` (§7.3): the last
    /// non-`0xff` byte is incremented and any trailing `0xff` bytes are zeroed. The arithmetic
    /// guarantees the carry never reaches before the start of the output.
    fn add_carry(&mut self) {
        let mut i = self.output.len();
        while i > 0 {
            i -= 1;
            if self.output[i] == 0xff {
                self.output[i] = 0;
            } else {
                self.output[i] += 1;
                return;
            }
        }
    }

    /// Encodes one `bool_value` whose probability of being `0` is `prob / 256` (RFC 6386 §7.3
    /// `write_bool`).
    pub fn put_bool(&mut self, prob: Prob, bool_value: bool) {
        let split = 1 + (((self.range - 1) * u32::from(prob)) >> 8);
        if bool_value {
            self.bottom = self.bottom.wrapping_add(split);
            self.range -= split;
        } else {
            self.range = split;
        }
        while self.range < 128 {
            self.range <<= 1;
            if self.bottom & (1 << 31) != 0 {
                self.add_carry();
            }
            self.bottom = self.bottom.wrapping_shl(1);
            self.bit_count -= 1;
            if self.bit_count == 0 {
                self.output.push((self.bottom >> 24) as u8);
                self.bottom &= (1 << 24) - 1;
                self.bit_count = 8;
            }
        }
    }

    /// Encodes a one-bit flag (a bool at probability `128`, i.e. `1/2`) — the `F` / `L(1)` of §8.
    pub fn put_flag(&mut self, value: bool) {
        self.put_bool(128, value);
    }

    /// Encodes the low `num_bits` of `value` as an unsigned literal `L(num_bits)`: `num_bits` flags
    /// written high-order bit first (RFC 6386 §7.3 `read_literal`). `num_bits` must be `0..=32`.
    pub fn put_literal(&mut self, value: u32, num_bits: u32) {
        let mut n = num_bits;
        while n > 0 {
            n -= 1;
            self.put_flag((value >> n) & 1 != 0);
        }
    }

    /// Encodes `value` as a signed `num_bits`-bit literal in the §7.3 `read_signed_literal` form: a
    /// sign flag followed by `num_bits - 1` magnitude bits (the `num_bits`-bit two's-complement of
    /// `value`, written high-order bit first). `value` must fit in `num_bits` two's-complement bits.
    #[cfg(test)]
    pub fn put_signed_literal(&mut self, value: i32, num_bits: u32) {
        if num_bits == 0 {
            return;
        }
        let mask = if num_bits >= 32 {
            u32::MAX
        } else {
            (1u32 << num_bits) - 1
        };
        self.put_literal((value as u32) & mask, num_bits);
    }

    /// Encodes the tree-coded `value` from `tree` using interior-node probabilities `probs`, starting
    /// the descent at interior node `start` (use `0` for the root; a non-zero `start` skips earlier
    /// decisions, e.g. the DCT token tree's end-of-block branch).
    ///
    /// In a release build a `value` not reachable from `start` writes nothing (a caller bug — the
    /// trees and values are static); in a debug build it triggers a `debug_assert`.
    pub fn put_tree_start(&mut self, tree: &Tree, probs: &[Prob], value: usize, start: usize) {
        walk_tree(tree, value, start, |prob_idx, bit| {
            self.put_bool(probs[prob_idx], bit);
        });
    }

    /// Encodes the tree-coded `value` from the root (equivalent to
    /// [`put_tree_start`](Self::put_tree_start) with `start = 0`).
    pub fn put_tree(&mut self, tree: &Tree, probs: &[Prob], value: usize) {
        self.put_tree_start(tree, probs, value, 0);
    }

    /// Flushes the coder (RFC 6386 §7.3 `flush_bool_encoder`) and returns the completed partition
    /// bytes. Call exactly once, after the last symbol.
    #[must_use]
    pub fn finish(mut self) -> Vec<u8> {
        let c = self.bit_count;
        let mut v = self.bottom;
        if v & (1u32 << (32 - c) as u32) != 0 {
            self.add_carry();
        }
        v = v.wrapping_shl((c & 7) as u32);
        // `flush_bool_encoder`: shift the remaining buffered bytes up to the top, then emit four.
        for _ in 0..(c >> 3) {
            v = v.wrapping_shl(8);
        }
        for _ in 0..4 {
            self.output.push((v >> 24) as u8);
            v = v.wrapping_shl(8);
        }
        self.output
    }

    /// Number of output bytes written so far (before [`finish`](Self::finish)).
    #[must_use]
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.output.len()
    }

    /// Whether no output bytes have been written yet.
    #[must_use]
    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.output.is_empty()
    }
}

/// VP8 boolean entropy **decoder** (RFC 6386 §7.3).
///
/// Reads the bools/literals/tree symbols written by a [`BoolEncoder`], in the same order and with
/// the same probabilities. Reading past the end of the partition yields zero bits (matching the
/// reference decoders' zero-padding) rather than panicking; [`BoolDecoder::is_past_end`] reports
/// whether that has happened, so the codec layer can reject a truncated stream.
#[derive(Debug, Clone)]
pub struct BoolDecoder<'a> {
    /// The partition bytes being decoded.
    input: &'a [u8],
    /// Index of the next byte to pull into `value`.
    pos: usize,
    /// Width of the current coding interval, identical to the encoder's `range`.
    range: u32,
    /// The encoded number less the known left endpoint of the current interval.
    value: u32,
    /// Number of bits shifted into `value` since the last byte was pulled (`0..=7`).
    bit_count: i32,
    /// Set once a read has consumed a (virtual) byte beyond the end of `input`.
    past_end: bool,
}

impl<'a> BoolDecoder<'a> {
    /// Creates a decoder over `input`, priming `value` with the first two bytes (zero-padded if
    /// `input` is shorter), per RFC 6386 §7.3 `init_bool_decoder`.
    #[must_use]
    pub fn new(input: &'a [u8]) -> Self {
        let b0 = input.first().copied().unwrap_or(0);
        let b1 = input.get(1).copied().unwrap_or(0);
        Self {
            input,
            pos: 2,
            range: 255,
            value: (u32::from(b0) << 8) | u32::from(b1),
            bit_count: 0,
            past_end: input.len() < 2,
        }
    }

    /// Pulls the next input byte, returning `0` (and latching [`past_end`](Self::is_past_end)) once
    /// the input is exhausted.
    fn next_byte(&mut self) -> u32 {
        let byte = match self.input.get(self.pos) {
            Some(&b) => u32::from(b),
            None => {
                self.past_end = true;
                0
            }
        };
        self.pos += 1;
        byte
    }

    /// Decodes one bool encoded at probability `prob / 256` (RFC 6386 §7.3 `read_bool`).
    pub fn get_bool(&mut self, prob: Prob) -> bool {
        let split = 1 + (((self.range - 1) * u32::from(prob)) >> 8);
        let big_split = split << 8;
        let retval = if self.value >= big_split {
            self.range -= split;
            self.value -= big_split;
            true
        } else {
            self.range = split;
            false
        };
        while self.range < 128 {
            self.value <<= 1;
            self.range <<= 1;
            self.bit_count += 1;
            if self.bit_count == 8 {
                self.bit_count = 0;
                self.value |= self.next_byte();
            }
        }
        retval
    }

    /// Decodes a one-bit flag (a bool at probability `128`) — the `F` / `L(1)` of §8.
    pub fn get_flag(&mut self) -> bool {
        self.get_bool(128)
    }

    /// Decodes an unsigned `num_bits`-bit literal `L(num_bits)`, high-order bit first (RFC 6386 §7.3
    /// `read_literal`). `num_bits` must be `0..=32`.
    pub fn get_literal(&mut self, num_bits: u32) -> u32 {
        let mut v = 0u32;
        for _ in 0..num_bits {
            v = (v << 1) | u32::from(self.get_flag());
        }
        v
    }

    /// Decodes a signed `num_bits`-bit literal (RFC 6386 §7.3 `read_signed_literal`): a sign flag
    /// followed by `num_bits - 1` magnitude bits.
    #[cfg(test)]
    pub fn get_signed_literal(&mut self, num_bits: u32) -> i32 {
        if num_bits == 0 {
            return 0;
        }
        let mut v: i32 = if self.get_flag() { -1 } else { 0 };
        for _ in 1..num_bits {
            v = (v << 1) + i32::from(self.get_flag());
        }
        v
    }

    /// Decodes a tree-coded value from `tree` with interior-node probabilities `probs`, beginning
    /// the descent at interior node `start` (RFC 6386 §8.1 `treed_read`).
    pub fn get_tree_start(&mut self, tree: &Tree, probs: &[Prob], start: usize) -> usize {
        let mut i = start as i32;
        loop {
            let bit = usize::from(self.get_bool(probs[i as usize >> 1]));
            i = i32::from(tree[i as usize + bit]);
            if i <= 0 {
                return (-i) as usize;
            }
        }
    }

    /// Decodes a tree-coded value from the root (equivalent to
    /// [`get_tree_start`](Self::get_tree_start) with `start = 0`).
    pub fn get_tree(&mut self, tree: &Tree, probs: &[Prob]) -> usize {
        self.get_tree_start(tree, probs, 0)
    }

    /// Whether a read has consumed input beyond the end of the partition (zero-padded). A correct,
    /// untruncated stream never reads past its meaningful end by more than the coder's lookahead, so
    /// the codec layer can use this to detect a malformed or truncated partition.
    #[must_use]
    #[cfg(test)]
    pub fn is_past_end(&self) -> bool {
        self.past_end
    }
}

/// Maximum interior-node depth of any VP8 tree (the 12-value DCT token tree has depth 11); sizes the
/// fixed path buffer in [`walk_tree`].
const MAX_TREE_DEPTH: usize = 16;

/// Visits the `(probability index, bit)` pairs that code `value` in `tree` starting at node
/// `start`, in emission order (RFC 6386 §8).
///
/// Factored out of [`BoolEncoder::put_tree_start`] so that writing a symbol, counting its bits, and
/// costing it all traverse **one** definition of the tree walk rather than three that can drift.
pub fn walk_tree(tree: &Tree, value: usize, start: usize, mut visit: impl FnMut(usize, bool)) {
    let mut path = [(0usize, false); MAX_TREE_DEPTH];
    match find_tree_path(tree, start as i32, value, &mut path, 0) {
        Some(len) => {
            for &(prob_idx, bit) in &path[..len] {
                visit(prob_idx, bit);
            }
        }
        None => debug_assert!(false, "value {value} not reachable in tree from {start}"),
    }
}

/// Finds the root-to-leaf path to `value` in `tree`, starting at interior node `start`, recording
/// `(prob_index, bit)` pairs into `out` from depth `depth`. Returns the total path length, or `None`
/// if `value` is not a leaf reachable from `start`.
fn find_tree_path(
    tree: &Tree,
    start: i32,
    value: usize,
    out: &mut [(usize, bool); MAX_TREE_DEPTH],
    depth: usize,
) -> Option<usize> {
    for bit in 0..2 {
        let child = i32::from(tree[(start + bit) as usize]);
        out[depth] = (start as usize >> 1, bit == 1);
        if child <= 0 {
            if (-child) as usize == value {
                return Some(depth + 1);
            }
        } else if let Some(len) = find_tree_path(tree, child, value, out, depth + 1) {
            return Some(len);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Small deterministic PRNG (SplitMix64) — the test environment forbids `Math.random`-style
    /// nondeterminism, and a fixed seed keeps the round-trips reproducible.
    struct SplitMix64(u64);
    impl SplitMix64 {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^ (z >> 31)
        }
        fn bits(&mut self, n: u32) -> u32 {
            (self.next() >> (64 - n)) as u32
        }
    }

    // The three intra-mode trees from RFC 6386 §8.2, used as tree-coding fixtures.
    // DC_PRED=0, V_PRED=1, H_PRED=2, TM_PRED=3, B_PRED=4.
    const YMODE_TREE: [i8; 8] = [0, 2, 4, 6, -1, -2, -3, -4];
    const KF_YMODE_TREE: [i8; 8] = [-4, 2, 4, 6, 0, -1, -2, -3];
    const UV_MODE_TREE: [i8; 6] = [0, 2, -1, 4, -2, -3];

    #[test]
    fn add_carry_increments_last_non_ff_and_clears_trailing_ff() {
        // Pin the encoder's carry propagation (`add_one_to_output`, §7.3) directly: the last non-0xff
        // byte is incremented and trailing 0xff bytes are zeroed. Short differential streams rarely
        // trigger a carry, so this is the only thing that exercises the loop body.
        let mut enc = BoolEncoder::new();
        enc.output = vec![0x12, 0xff, 0xff];
        enc.add_carry();
        assert_eq!(enc.output, [0x13, 0x00, 0x00]);
        enc.output = vec![0x40, 0x41];
        enc.add_carry();
        assert_eq!(
            enc.output,
            [0x40, 0x42],
            "a carry with no trailing 0xff just bumps the last byte"
        );
        // All-0xff: the loop zeroes every byte and stops at index 0 (the `i > 0` bound); a `>= 0`
        // bound would step past the start and panic, pinning the loop bound.
        enc.output = vec![0xff, 0xff];
        enc.add_carry();
        assert_eq!(enc.output, [0x00, 0x00]);
    }

    #[test]
    fn carry_detection_fires_on_renorm_and_flush() {
        // White-box: the carry path only fires when `bottom`'s top bit is set during a
        // renormalization shift (`bottom & (1 << 31)`) or at the flush (`v & (1 << (32 - c))`) —
        // states synthetic streams rarely reach. Force them directly so a flipped detection shift,
        // which would silently drop the carry, is caught.
        let mut enc = BoolEncoder::new();
        enc.output = vec![0x00];
        enc.bottom = 1 << 31;
        enc.range = 1; // < 128, so put_bool's renormalization runs and inspects bit 31
        enc.bit_count = 8;
        enc.put_bool(128, false);
        assert_eq!(
            enc.output[0], 0x01,
            "renorm carry must reach the emitted byte"
        );

        let mut enc = BoolEncoder::new();
        enc.output = vec![0x00];
        enc.bottom = 1 << 24; // bit (32 - bit_count) for the flush carry check
        enc.bit_count = 8;
        assert_eq!(
            enc.finish()[0],
            0x01,
            "flush carry must reach the emitted byte"
        );
    }

    #[test]
    fn round_trips_carry_heavy_stream_and_wide_literals() {
        // A run of near-certain bools drives `bottom` past bit 31, exercising carry *detection* in
        // put_bool and finish (a flipped shift there silently drops the carry); the literals reach the
        // 32-bit boundary of the signed-literal mask. Any corrupted byte makes the independent decoder
        // mismatch, so this kills the carry/shift/mask mutants the short round-trips miss.
        let mut enc = BoolEncoder::new();
        let mut bools = Vec::new();
        // A long run of near-certain bools cycles `bottom` through the [2^23, 2^24) range many times,
        // so a renormalization shift sets bit 31 and the carry path (`bottom & (1 << 31)`) actually
        // fires — which a short stream never reaches.
        for _ in 0..20000 {
            enc.put_bool(255, true);
            bools.push((255u8, true));
        }
        let mut rng = SplitMix64(0xfeed_face);
        for _ in 0..2000 {
            let p = (rng.bits(8) as u8).max(1);
            let b = rng.bits(1) == 1;
            enc.put_bool(p, b);
            bools.push((p, b));
        }
        let lits: &[(u32, u32)] = &[(0xFFFF_FFFF, 32), (0, 0), (0xABCD, 16)];
        for &(v, n) in lits {
            enc.put_literal(v, n);
        }
        let signed: &[(i32, u32)] = &[(i32::MIN, 32), (i32::MAX, 32), (-1, 4), (7, 5)];
        for &(v, n) in signed {
            enc.put_signed_literal(v, n);
        }
        let bytes = enc.finish();

        let mut dec = BoolDecoder::new(&bytes);
        for (p, b) in bools {
            assert_eq!(dec.get_bool(p), b, "bool mismatch");
        }
        for &(v, n) in lits {
            assert_eq!(dec.get_literal(n), v, "literal {v:#x}/{n}");
        }
        for &(v, n) in signed {
            assert_eq!(dec.get_signed_literal(n), v, "signed {v}/{n}");
        }
    }

    #[test]
    fn bool_roundtrip_across_probabilities() {
        // Encode a long pseudo-random bool stream at a spread of probabilities, then decode it back.
        let mut rng = SplitMix64(0x1234_5678);
        let probs: Vec<u8> = (0..512).map(|_| (rng.bits(8) as u8).max(1)).collect();
        let bits: Vec<bool> = (0..512).map(|_| rng.bits(1) == 1).collect();

        let mut enc = BoolEncoder::new();
        for (p, &b) in probs.iter().zip(&bits) {
            enc.put_bool(*p, b);
        }
        let bytes = enc.finish();

        let mut dec = BoolDecoder::new(&bytes);
        for (p, &b) in probs.iter().zip(&bits) {
            assert_eq!(dec.get_bool(*p), b, "bool mismatch at prob {p}");
        }
        assert!(
            !dec.is_past_end(),
            "decode should not run past a complete stream"
        );
    }

    #[test]
    fn extreme_probabilities_roundtrip() {
        // prob = 1 and prob = 255 exercise the largest interval skews (near-certain bools).
        let bits: Vec<bool> = (0..200).map(|i| i % 3 == 0).collect();
        for &p in &[1u8, 2, 254, 255] {
            let mut enc = BoolEncoder::new();
            for &b in &bits {
                enc.put_bool(p, b);
            }
            let bytes = enc.finish();
            let mut dec = BoolDecoder::new(&bytes);
            for &b in &bits {
                assert_eq!(dec.get_bool(p), b, "mismatch at prob {p}");
            }
        }
    }

    #[test]
    fn literal_roundtrip_all_widths() {
        let mut rng = SplitMix64(0xfeed_face);
        let mut enc = BoolEncoder::new();
        let mut expected = Vec::new();
        for n in 1..=32u32 {
            let v = if n == 32 {
                rng.next() as u32
            } else {
                rng.bits(n)
            };
            enc.put_literal(v, n);
            expected.push((v, n));
        }
        let bytes = enc.finish();
        let mut dec = BoolDecoder::new(&bytes);
        for (v, n) in expected {
            assert_eq!(dec.get_literal(n), v, "literal width {n}");
        }
    }

    #[test]
    fn signed_literal_roundtrip() {
        let mut enc = BoolEncoder::new();
        let cases = [
            (0i32, 1u32),
            (-1, 1),
            (3, 4),
            (-8, 4),
            (-128, 8),
            (127, 8),
            (-1, 16),
        ];
        for &(v, n) in &cases {
            enc.put_signed_literal(v, n);
        }
        let bytes = enc.finish();
        let mut dec = BoolDecoder::new(&bytes);
        for &(v, n) in &cases {
            assert_eq!(
                dec.get_signed_literal(n),
                v,
                "signed literal {v} in {n} bits"
            );
        }
    }

    #[test]
    fn tree_roundtrip_uniform_and_skewed() {
        // Round-trip every leaf of each §8.2 tree, with uniform (128) and skewed node probabilities.
        let trees: &[(&[i8], usize)] = &[(&YMODE_TREE, 5), (&KF_YMODE_TREE, 5), (&UV_MODE_TREE, 4)];
        for &(tree, n_values) in trees {
            for probs in [vec![128u8; 4], vec![10u8, 200, 64, 250]] {
                let mut enc = BoolEncoder::new();
                for v in 0..n_values {
                    enc.put_tree(tree, &probs, v);
                }
                let bytes = enc.finish();
                let mut dec = BoolDecoder::new(&bytes);
                for v in 0..n_values {
                    assert_eq!(dec.get_tree(tree, &probs), v, "tree leaf {v}");
                }
            }
        }
    }

    #[test]
    fn tree_start_index_skips_initial_branch() {
        // Starting the descent at interior node 2 of KF_YMODE_TREE restricts the alphabet to the
        // "1" subtree {DC_PRED, V_PRED, H_PRED, TM_PRED} — the mechanism the DCT token tree uses to
        // skip its end-of-block branch after a zero token (P5).
        let probs = [128u8; 4];
        let reachable = [0usize, 1, 2, 3];
        let mut enc = BoolEncoder::new();
        for &v in &reachable {
            enc.put_tree_start(&KF_YMODE_TREE, &probs, v, 2);
        }
        let bytes = enc.finish();
        let mut dec = BoolDecoder::new(&bytes);
        for &v in &reachable {
            assert_eq!(dec.get_tree_start(&KF_YMODE_TREE, &probs, 2), v);
        }
    }

    #[test]
    fn mixed_stream_roundtrip() {
        // Interleave every symbol kind in one partition and decode in the same order.
        let mut enc = BoolEncoder::new();
        enc.put_literal(0b1011_0010, 8);
        enc.put_bool(30, true);
        enc.put_tree(&UV_MODE_TREE, &[200, 50, 90], 3);
        enc.put_flag(false);
        enc.put_signed_literal(-5, 6);
        enc.put_bool(220, false);
        let bytes = enc.finish();

        let mut dec = BoolDecoder::new(&bytes);
        assert_eq!(dec.get_literal(8), 0b1011_0010);
        assert!(dec.get_bool(30));
        assert_eq!(dec.get_tree(&UV_MODE_TREE, &[200, 50, 90]), 3);
        assert!(!dec.get_flag());
        assert_eq!(dec.get_signed_literal(6), -5);
        assert!(!dec.get_bool(220));
    }

    #[test]
    fn encoding_is_deterministic() {
        let encode = || {
            let mut e = BoolEncoder::new();
            for i in 0..100u32 {
                e.put_bool((i % 254 + 1) as u8, i % 2 == 0);
            }
            e.finish()
        };
        assert_eq!(
            encode(),
            encode(),
            "the coder must be a pure function of its inputs"
        );
    }

    #[test]
    fn empty_encoder_flushes_to_zero_padding() {
        // Hand-traceable golden: with bottom = 0 and bit_count = 24, `flush_bool_encoder` writes
        // four zero bytes. This pins the flush/byte-count behavior that partition sizes depend on.
        assert_eq!(BoolEncoder::new().finish(), [0, 0, 0, 0]);
    }

    #[test]
    fn decoder_zero_pads_past_end() {
        // A valid 2-byte partition exhausted by reads must keep returning 0 (not panic) and latch
        // the past-end flag once `next_byte` runs off the end.
        let mut dec = BoolDecoder::new(&[0x00, 0x00]);
        assert!(
            !dec.is_past_end(),
            "two bytes prime the decoder without overrun"
        );
        for _ in 0..64 {
            let _ = dec.get_flag();
        }
        assert!(dec.is_past_end());
    }

    #[test]
    fn carry_propagation_chain() {
        // A run of true bools at low zero-probability stresses carry propagation across 0xff bytes.
        let mut enc = BoolEncoder::new();
        for _ in 0..50 {
            enc.put_bool(1, true);
        }
        let bytes = enc.finish();
        let mut dec = BoolDecoder::new(&bytes);
        for _ in 0..50 {
            assert!(dec.get_bool(1));
        }
    }

    #[test]
    fn encoder_len_tracks_output_and_default_matches_new() {
        // `len`/`is_empty` report the output byte count partition sizing (P6/P7) reads; `Default`
        // must produce the same initial state as `new`.
        let mut enc = BoolEncoder::default();
        assert!(enc.is_empty());
        let before = enc.len();
        // Enough bools to force at least one renormalization byte out of the interval.
        for i in 0..64 {
            enc.put_bool(8, i % 2 == 0);
        }
        assert!(!enc.is_empty());
        assert!(enc.len() > before);
        assert_eq!(before, 0);
    }
}
