//! Length-limited canonical Huffman tables in the ITU-T T.81 form
//! (`references/jpeg/itu-t81.pdf`): the Annex K.2 optimal-table construction over measured symbol
//! frequencies, and the Annex C canonical code assignment that turns its result into per-symbol
//! `(code, length)` pairs.
//!
//! A table is carried as the two lists of T.81 §B.2.4.2:
//! - **BITS** — `bits[l - 1]` is the number of codes of length `l`, for `l` in `1..=16`;
//! - **HUFFVAL** — the symbol values, ordered by increasing code length.
//!
//! Nothing here is specific to one alphabet. A symbol is a `u8`, so any alphabet of at most 256
//! symbols fits: the JPEG DC/AC run-size symbols (`0..=255`) and the lossless-JPEG difference
//! categories (`0..=16`, T.81 Table H.2) both use it. Writing the lists into a marker segment is the
//! format crate's business.

/// The canonical `(code, length)` of every symbol of a `(BITS, HUFFVAL)` table, assigned by the
/// T.81 Annex C procedure (Figures C.1–C.3): consecutive code values in HUFFVAL order, the running
/// value doubling each time the code length increases.
#[derive(Debug, Clone)]
pub struct CanonicalCodes {
    codes: [u16; 256],
    lengths: [u8; 256],
}

impl CanonicalCodes {
    /// Assigns the canonical codes for `bits` and `values` (T.81 Annex C).
    ///
    /// `values` should hold exactly `sum(bits)` symbols, as both a parsed table and
    /// [`build_optimal_table`]'s output do. The assignment is total over any input: codes beyond
    /// the end of `values` are assigned to no symbol, values beyond `sum(bits)` get no code, and a
    /// symbol listed twice keeps its later code. A `bits` that over-subscribes the code space
    /// (`sum(bits[l - 1] · 2^(16 − l)) > 2^16`) is not a prefix code, and its codes are truncated to
    /// 16 bits; validate a table read from a stream before building codes from it.
    #[must_use]
    pub fn from_bits_values(bits: &[u8; 16], values: &[u8]) -> Self {
        let mut codes = [0u16; 256];
        let mut lengths = [0u8; 256];
        // The code length of each HUFFVAL position, in order: `bits[l - 1]` copies of `l`.
        let code_lengths = (1u8..=16)
            .zip(bits)
            .flat_map(|(length, &count)| core::iter::repeat_n(length, usize::from(count)));
        // `code` stays far inside `u32`: at most 16 · 255 codes, shifted at most 15 places in all.
        let mut code = 0u32;
        let mut previous = 1u8;
        for (length, &symbol) in code_lengths.zip(values) {
            // Moving to a longer length appends one 0 bit per length step (doubles the value).
            code <<= length - previous;
            previous = length;
            codes[usize::from(symbol)] = code as u16;
            lengths[usize::from(symbol)] = length;
            code += 1;
        }
        Self { codes, lengths }
    }

    /// The `(code, length)` an encoder emits for `symbol`, or `None` if the table does not code
    /// `symbol`. The code occupies the low `length` bits, most significant bit first on the wire.
    #[must_use]
    pub fn lookup(&self, symbol: u8) -> Option<(u16, u8)> {
        let length = self.lengths[usize::from(symbol)];
        (length != 0).then(|| (self.codes[usize::from(symbol)], length))
    }
}

/// Builds the optimal 16-bit length-limited Huffman table (T.81 Annex K.2, Figures K.1–K.4) for
/// the per-symbol frequency counts `freq`, returning the `(BITS, HUFFVAL)` pair (§B.2.4.2).
///
/// `freq[s]` is how many times symbol `s` is coded; the alphabet is `0..N`, at most 256 symbols
/// (a larger `N` fails to compile, since HUFFVAL holds `u8`s). The procedure is the reference one
/// (libjpeg's `jpeg_gen_optimal_table` mirrors the same clause), so the output is byte-identical to
/// what a libjpeg-family encoder writes for the same counts:
///
/// 1. A **reserved pseudo-symbol** (frequency 1) is added after every real symbol. It lands last in
///    the longest code length and is then removed, so no real symbol is assigned the all-ones code
///    word of its length, which T.81 reserves (§K.2, Figure K.1).
/// 2. Huffman's algorithm assigns each symbol a code length by repeatedly merging the two least
///    frequent subtrees, ties going to the larger symbol number as in the reference.
/// 3. The **16-bit length limiting** of Figure K.3 moves every code longer than 16 bits into the
///    16-bit budget, two symbols at a time off the longest length.
/// 4. The pseudo-symbol's code is removed from the longest length still in use, and HUFFVAL lists
///    the symbols ordered by their code length before step 3 (Figure K.4).
///
/// The returned `sum(BITS)` equals the number of symbols with a non-zero count, and no code of any
/// length is all-ones. An all-zero `freq` yields an empty table.
#[must_use]
pub fn build_optimal_table<const N: usize>(freq: &[u32; N]) -> ([u8; 16], Vec<u8>) {
    const { assert!(N <= 256, "a HUFFVAL symbol is a u8: at most 256 symbols") };
    // Sentinels chosen so a merged frequency (at most 256 · u32::MAX < 2^41) is always < LIVE_MAX and
    // a merged-away node (REMOVED) is always > LIVE_MAX, so neither is ever picked as a minimum.
    const LIVE_MAX: u64 = 1u64 << 62;
    const REMOVED: u64 = 1u64 << 63;
    // The longest code the tree can produce for a 257-symbol alphabet is < 257 bits; sizing every
    // length-indexed array to 259 keeps the whole procedure panic-free for any frequency multiset.
    const LENGTHS: usize = 259;

    // Group the non-zero frequencies (plus the reserved pseudo-symbol) together, remembering each
    // one's symbol value. Ties later resolve toward the larger symbol number (as libjpeg's grouped
    // scan does), which the ordering of this pass preserves.
    let mut gfreq = [0u64; 257];
    let mut nz_index = [0usize; 257];
    let mut num_nz = 0usize;
    for (s, &f) in freq.iter().enumerate() {
        if f != 0 {
            nz_index[num_nz] = s;
            gfreq[num_nz] = u64::from(f);
            num_nz += 1;
        }
    }
    // The reserved pseudo-symbol is added last so it lands in the longest-code category.
    gfreq[num_nz] = 1;
    num_nz += 1;

    let mut codesize = [0i32; 257];
    let mut others = [-1i32; 257];

    // Huffman's algorithm: repeatedly merge the two least-frequent subtrees.
    loop {
        // The two smallest-frequency live nodes (`None` = not yet found). Ties resolve toward the
        // larger index, matching the reference: an equal frequency updates `c1` (and pushes the old
        // `c1` to `c2`), so a later node wins.
        let (mut c1, mut c2): (Option<usize>, Option<usize>) = (None, None);
        let (mut v, mut v2) = (LIVE_MAX, LIVE_MAX);
        for (i, &f) in gfreq.iter().take(num_nz).enumerate() {
            if f <= v2 {
                if f <= v {
                    c2 = c1;
                    v2 = v;
                    v = f;
                    c1 = Some(i);
                } else {
                    v2 = f;
                    c2 = Some(i);
                }
            }
        }
        let (Some(c1), Some(c2)) = (c1, c2) else {
            break; // only one live node remains — everything is merged into one tree
        };
        gfreq[c1] += gfreq[c2];
        gfreq[c2] = REMOVED;
        // Increment the code length of every symbol in c1's branch, then chain c2 onto its end.
        let mut a = c1;
        codesize[a] += 1;
        while others[a] >= 0 {
            a = others[a] as usize;
            codesize[a] += 1;
        }
        others[a] = c2 as i32;
        // Increment the code length of every symbol in c2's branch.
        let mut b = c2;
        codesize[b] += 1;
        while others[b] >= 0 {
            b = others[b] as usize;
            codesize[b] += 1;
        }
    }

    // Count symbols of each code length, then the running count of shorter symbols (bit_pos), from
    // the *pre-adjustment* lengths — the HUFFVAL ordering below indexes by these lengths (Figure K.4).
    let mut bits = [0i32; LENGTHS];
    for &cl in codesize.iter().take(num_nz) {
        bits[cl as usize] += 1;
    }
    let mut bit_pos = [0i32; LENGTHS];
    let mut p = 0i32;
    for i in 1..LENGTHS {
        bit_pos[i] = p;
        p += bits[i];
    }

    // 16-bit length limiting (Figure K.3): pull symbol pairs off any length > 16 down into shorter
    // categories until nothing exceeds 16 bits. The prefix lent to each pair is the longest shorter
    // length (skipping i − 1) that still has a code; one always exists, because the tree that put a
    // code at length i has an internal node at every shorter depth. The search is a bounded
    // iterator rather than a hand-stepped `j -= 1` descent, so no perturbation of it can spin.
    for i in (17..LENGTHS).rev() {
        while bits[i] > 0 {
            let Some(j) = (1..=i - 2).rev().find(|&j| bits[j] != 0) else {
                break;
            };
            bits[i] -= 2; // remove two symbols from length i
            bits[i - 1] += 1; // one moves up a level (its own prefix)
            bits[j + 1] += 2; // two new symbols one level below the borrowed prefix
            bits[j] -= 1; // the borrowed code becomes a prefix
        }
    }

    // Remove the reserved pseudo-symbol from the longest code length still in use (§K.2): it took
    // the all-ones code of that length, so dropping it leaves no real symbol with that code word.
    if let Some(i) = (1..=16usize).rev().find(|&i| bits[i] > 0) {
        bits[i] -= 1;
    }

    // Emit BITS (lengths 1..=16) and HUFFVAL (every real symbol, ordered by its original code
    // length; the pseudo-symbol is the last grouped entry and is skipped).
    let mut out_bits = [0u8; 16];
    for (dst, &b) in out_bits.iter_mut().zip(bits[1..=16].iter()) {
        *dst = b as u8;
    }
    let real = num_nz - 1;
    let mut values = vec![0u8; real];
    for i in 0..real {
        let slot = bit_pos[codesize[i] as usize];
        if let Some(cell) = values.get_mut(slot as usize) {
            *cell = nz_index[i] as u8;
        }
        bit_pos[codesize[i] as usize] += 1;
    }
    (out_bits, values)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// T.81 Table K.3, the typical luminance DC table: a fixed `(BITS, HUFFVAL)` the Annex C
    /// assignment can be checked against by hand.
    const K3_BITS: [u8; 16] = [0, 1, 5, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0];
    const K3_VALUES: [u8; 12] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];

    #[test]
    fn canonical_codes_match_annex_c_for_table_k3() {
        // Hand-derived from Table K.3 via Figures C.1–C.3:
        //   category 0 → length 2, code 00
        //   categories 1..=5 → length 3, codes 010..110
        //   category 6 → length 4, code 1110; category 7 → length 5, code 11110
        //   category 11 → length 9, code 111111110
        let t = CanonicalCodes::from_bits_values(&K3_BITS, &K3_VALUES);
        assert_eq!(t.lookup(0), Some((0b00, 2)));
        assert_eq!(t.lookup(1), Some((0b010, 3)));
        assert_eq!(t.lookup(2), Some((0b011, 3)));
        assert_eq!(t.lookup(5), Some((0b110, 3)));
        assert_eq!(t.lookup(6), Some((0b1110, 4)));
        assert_eq!(t.lookup(7), Some((0b11110, 5)));
        assert_eq!(t.lookup(11), Some((0b1_1111_1110, 9)));
    }

    #[test]
    fn canonical_codes_skip_empty_lengths() {
        // One code at each of lengths 1, 3 and 16: the empty lengths in between still double the
        // running value, so 3 bits after `0` is `100` and the 16-bit code is `1010…0`.
        let mut bits = [0u8; 16];
        bits[0] = 1;
        bits[2] = 1;
        bits[15] = 1;
        let t = CanonicalCodes::from_bits_values(&bits, &[7, 9, 200]);
        assert_eq!(t.lookup(7), Some((0b0, 1)));
        assert_eq!(t.lookup(9), Some((0b100, 3)));
        assert_eq!(t.lookup(200), Some((0b1010_0000_0000_0000, 16)));
    }

    #[test]
    fn canonical_codes_leave_unlisted_symbols_uncoded() {
        // Table K.3 codes categories 0..=11 only.
        let t = CanonicalCodes::from_bits_values(&K3_BITS, &K3_VALUES);
        assert_eq!(t.lookup(12), None);
    }

    #[test]
    fn canonical_codes_tolerate_a_short_huffval() {
        // BITS promises twelve codes but HUFFVAL names three: the three get the first three codes
        // and the rest are assigned to nobody — no panic, no stray code.
        let t = CanonicalCodes::from_bits_values(&K3_BITS, &[0, 1, 2]);
        assert_eq!(t.lookup(2), Some((0b011, 3)));
        assert_eq!(t.lookup(3), None);
    }

    /// The Annex C code-space check: `sum(bits[l - 1] · 2^(16 − l)) <= 2^16`.
    fn fits_code_space(bits: &[u8; 16]) -> bool {
        let used: u32 = (1u32..=16)
            .zip(bits)
            .map(|(length, &count)| u32::from(count) << (16 - length))
            .sum();
        used <= 1 << 16
    }

    /// The invariants every optimized table satisfies: one code per symbol with a non-zero count,
    /// a code space that is not over-subscribed, no prefix collision, and no code that is all-ones
    /// at its own length (the code word T.81 §K.2 reserves).
    fn assert_optimal_invariants(freq: &[u32], bits: &[u8; 16], values: &[u8]) {
        let total: usize = bits.iter().map(|&b| usize::from(b)).sum();
        assert_eq!(total, values.len(), "sum(BITS) != len(HUFFVAL)");
        assert_eq!(
            total,
            freq.iter().filter(|&&f| f != 0).count(),
            "one code per symbol"
        );
        assert!(fits_code_space(bits), "over-subscribed code space");
        let t = CanonicalCodes::from_bits_values(bits, values);
        let mut coded = Vec::new();
        for (s, &f) in freq.iter().enumerate() {
            if f != 0 {
                let (code, len) = t.lookup(s as u8).expect("every counted symbol has a code");
                assert_ne!(
                    u32::from(code),
                    (1u32 << len) - 1,
                    "symbol {s:#x} got the reserved all-ones code"
                );
                coded.push((code, len));
            }
        }
        for (i, &(ci, li)) in coded.iter().enumerate() {
            for &(cj, lj) in coded.iter().skip(i + 1) {
                let (short, ls, long, ll) = if li <= lj {
                    (ci, li, cj, lj)
                } else {
                    (cj, lj, ci, li)
                };
                assert_ne!(short, long >> (ll - ls), "prefix collision");
            }
        }
    }

    #[test]
    fn optimal_single_symbol_gets_a_one_bit_code() {
        // One symbol + the reserved pseudo-symbol → two length-1 codes, one removed with the pseudo:
        // the lone real symbol gets the single-bit code 0 (the all-ones "1" is reserved).
        let mut freq = [0u32; 256];
        freq[5] = 10;
        let (bits, values) = build_optimal_table(&freq);
        assert_eq!(bits[0], 1);
        assert_eq!(bits[1..], [0u8; 15]);
        assert_eq!(values, vec![5]);
        assert_eq!(
            CanonicalCodes::from_bits_values(&bits, &values).lookup(5),
            Some((0, 1))
        );
        assert_optimal_invariants(&freq, &bits, &values);
    }

    #[test]
    fn optimal_single_symbol_zero_gets_a_one_bit_code() {
        // The empty-band edge of a JPEG AC scan: its only emitted symbol is EOB0 (0x00). Symbol 0
        // is also the value an unwritten HUFFVAL slot holds, so this pins that the symbol is placed.
        let mut freq = [0u32; 256];
        freq[0x00] = 7;
        let (bits, values) = build_optimal_table(&freq);
        assert_eq!(values, vec![0x00]);
        assert_eq!(bits[0], 1);
        assert_optimal_invariants(&freq, &bits, &values);
    }

    #[test]
    fn optimal_two_equal_symbols_hand_derivation() {
        // Two equal-frequency symbols {1, 2} plus the pseudo-symbol (all frequency 1). Hand-tracing
        // the K.2 merge (ties toward the larger symbol number) pairs the pseudo with symbol 2 first,
        // giving BITS = [1,1,0,…] and HUFFVAL = [1, 2]: symbol 1 → code 0 (len 1), symbol 2 → code
        // 0b10 (len 2). This pins the tie-break direction and the pseudo-symbol removal.
        let mut freq = [0u32; 256];
        freq[1] = 1;
        freq[2] = 1;
        let (bits, values) = build_optimal_table(&freq);
        assert_eq!(&bits[..2], &[1, 1]);
        assert_eq!(&bits[2..], &[0u8; 14]);
        assert_eq!(values, vec![1, 2]);
        let t = CanonicalCodes::from_bits_values(&bits, &values);
        assert_eq!(t.lookup(1), Some((0, 1)));
        assert_eq!(t.lookup(2), Some((0b10, 2)));
        assert_optimal_invariants(&freq, &bits, &values);
    }

    #[test]
    fn optimal_skewed_frequencies_force_16_bit_limiting() {
        // Frequencies doubling per symbol (`2^i`) each exceed the sum of all smaller ones, so
        // Huffman's pure algorithm builds a fully degenerate chain whose deepest code is ~20 bits —
        // well past the 16-bit limit — driving the Figure K.3 length-limiting adjustment.
        let mut freq = [0u32; 256];
        for (i, slot) in freq.iter_mut().take(20).enumerate() {
            *slot = 1u32 << i;
        }
        let (bits, values) = build_optimal_table(&freq);
        // Exact golden (a regression pin over the whole reduction): the degenerate chain of 20
        // symbols + the reserved pseudo-symbol reduces to one code at each length 1..=13 and the
        // seven deepest pushed to length 16, with the symbols ordered least-frequent (deepest) last.
        // Any mutation of the Figure K.3 redistribution or its borrow search changes these bytes.
        assert_eq!(bits, [1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 7]);
        assert_eq!(
            values,
            vec![
                19, 18, 17, 16, 15, 14, 13, 12, 11, 10, 9, 8, 7, 6, 5, 4, 3, 2, 1, 0
            ]
        );
        assert_optimal_invariants(&freq, &bits, &values);
    }

    #[test]
    fn optimal_empty_frequency_is_an_empty_table() {
        // No symbols at all yields an all-zero BITS and no HUFFVAL — never a panic. (The JPEG
        // encoders never call the builder with an empty multiset, but the primitive is total.)
        let (bits, values) = build_optimal_table(&[0u32; 256]);
        assert_eq!(bits, [0u8; 16]);
        assert!(values.is_empty());
        let (bits, values) = build_optimal_table(&[]);
        assert_eq!(bits, [0u8; 16]);
        assert!(values.is_empty());
    }

    #[test]
    fn optimal_many_symbols_form_a_valid_prefix_code() {
        // A broad multiset (162 symbols with varied counts, the size of a JPEG AC alphabet).
        let mut freq = [0u32; 256];
        for (i, slot) in freq.iter_mut().enumerate().filter(|(s, _)| s % 16 <= 9) {
            *slot = (i as u32 % 13) + 1;
        }
        assert_eq!(freq.iter().filter(|&&f| f != 0).count(), 160);
        let (bits, values) = build_optimal_table(&freq);
        assert_optimal_invariants(&freq, &bits, &values);
    }

    #[test]
    fn optimal_table_over_a_seventeen_symbol_alphabet() {
        // The lossless-JPEG difference categories 0..=16 (T.81 Table H.2): an alphabet shorter than
        // 256 builds through the same procedure. Counts falling linearly from category 0 give a
        // monotone code-length ladder; the exact BITS/HUFFVAL pin the result.
        let mut freq = [0u32; 17];
        for (i, slot) in freq.iter_mut().enumerate() {
            *slot = 17 - i as u32;
        }
        let (bits, values) = build_optimal_table(&freq);
        assert_eq!(bits, [0, 0, 3, 8, 3, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(values, (0..=16).collect::<Vec<u8>>());
        assert_optimal_invariants(&freq, &bits, &values);
        let wide = {
            let mut wide = [0u32; 256];
            wide[..17].copy_from_slice(&freq);
            wide
        };
        // The alphabet size is not an input to the construction: padding with zero counts changes
        // nothing.
        assert_eq!(build_optimal_table(&wide), (bits, values));
    }
}
