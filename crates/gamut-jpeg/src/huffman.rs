//! Huffman code tables: the T.81 Annex K.3–K.6 typical tables, canonical code generation
//! (Annex C / K.2), the encode-side symbol→code lookup, and DHT emission (§B.2.4.2).
//!
//! A JPEG Huffman table is specified on the wire by two lists (§B.2.4.2, Annex C):
//! - **BITS** — `BITS[i]` (for code length `i` in `1..=16`) is the number of codes of that length;
//! - **HUFFVAL** — the symbol values, ordered by increasing code length.
//!
//! From these the canonical (unique, prefix-free) codes are derived by the §C.2 procedure
//! (Figures C.1–C.3): assign consecutive integers to the symbols in HUFFVAL order, doubling the
//! running code each time the length increases. [`EncTable`] precomputes, per symbol, the
//! `(code, length)` an encoder emits; [`emit_dht`] writes the BITS/HUFFVAL lists into a DHT segment.
//!
//! The four constant tables are the Annex K "typical" tables, transcribed verbatim — what a baseline
//! encoder writes by default. Because they are fixed, [`STD_LUMA_DC`] etc. carry their own
//! BITS/HUFFVAL and the derived codes are checked against Annex C in the tests. Both encoders can
//! instead fit a table to the image: [`build_optimal_table`] runs the §K.2 construction over measured
//! symbol frequencies and [`emit_dht_dynamic`] writes the result — mandatory for progressive (the
//! typical AC tables cannot code an `EOBn` symbol) and opt-in for baseline via
//! [`JpegEncoder::with_optimized_tables`](crate::JpegEncoder::with_optimized_tables).

use gamut_core::{Error, Result};

use crate::marker::{self, code};

/// One Huffman table specification: the class/precision-independent `(BITS, HUFFVAL)` pair of
/// §B.2.4.2. `bits[i - 1]` is the number of codes of length `i` (`i` in `1..=16`); `values` lists
/// the symbols in canonical (increasing-length) order.
#[derive(Debug, Clone, Copy)]
pub struct TableSpec {
    /// `BITS`: count of codes of each length 1..=16.
    pub bits: [u8; 16],
    /// `HUFFVAL`: symbol values ordered by increasing code length.
    pub values: &'static [u8],
}

/// T.81 Table K.3 — typical **luminance DC** difference table. Categories 0..=11.
pub const STD_LUMA_DC: TableSpec = TableSpec {
    bits: [0, 1, 5, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0],
    values: &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11],
};

/// T.81 Table K.4 — typical **chrominance DC** difference table. Categories 0..=11.
pub const STD_CHROMA_DC: TableSpec = TableSpec {
    bits: [0, 3, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0],
    values: &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11],
};

/// T.81 Table K.5 — typical **luminance AC** table (run/size symbols `RRRRSSSS`).
pub const STD_LUMA_AC: TableSpec = TableSpec {
    bits: [0, 2, 1, 3, 3, 2, 4, 3, 5, 5, 4, 4, 0, 0, 1, 0x7d],
    values: &[
        0x01, 0x02, 0x03, 0x00, 0x04, 0x11, 0x05, 0x12, 0x21, 0x31, 0x41, 0x06, 0x13, 0x51, 0x61,
        0x07, 0x22, 0x71, 0x14, 0x32, 0x81, 0x91, 0xa1, 0x08, 0x23, 0x42, 0xb1, 0xc1, 0x15, 0x52,
        0xd1, 0xf0, 0x24, 0x33, 0x62, 0x72, 0x82, 0x09, 0x0a, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x25,
        0x26, 0x27, 0x28, 0x29, 0x2a, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x43, 0x44, 0x45,
        0x46, 0x47, 0x48, 0x49, 0x4a, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x63, 0x64,
        0x65, 0x66, 0x67, 0x68, 0x69, 0x6a, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x83,
        0x84, 0x85, 0x86, 0x87, 0x88, 0x89, 0x8a, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99,
        0x9a, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6,
        0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3, 0xc4, 0xc5, 0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xd2, 0xd3,
        0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda, 0xe1, 0xe2, 0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8,
        0xe9, 0xea, 0xf1, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8, 0xf9, 0xfa,
    ],
};

/// T.81 Table K.6 — typical **chrominance AC** table (run/size symbols `RRRRSSSS`).
pub const STD_CHROMA_AC: TableSpec = TableSpec {
    bits: [0, 2, 1, 2, 4, 4, 3, 4, 7, 5, 4, 4, 0, 1, 2, 0x77],
    values: &[
        0x00, 0x01, 0x02, 0x03, 0x11, 0x04, 0x05, 0x21, 0x31, 0x06, 0x12, 0x41, 0x51, 0x07, 0x61,
        0x71, 0x13, 0x22, 0x32, 0x81, 0x08, 0x14, 0x42, 0x91, 0xa1, 0xb1, 0xc1, 0x09, 0x23, 0x33,
        0x52, 0xf0, 0x15, 0x62, 0x72, 0xd1, 0x0a, 0x16, 0x24, 0x34, 0xe1, 0x25, 0xf1, 0x17, 0x18,
        0x19, 0x1a, 0x26, 0x27, 0x28, 0x29, 0x2a, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x43, 0x44,
        0x45, 0x46, 0x47, 0x48, 0x49, 0x4a, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x63,
        0x64, 0x65, 0x66, 0x67, 0x68, 0x69, 0x6a, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a,
        0x82, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89, 0x8a, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97,
        0x98, 0x99, 0x9a, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4,
        0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3, 0xc4, 0xc5, 0xc6, 0xc7, 0xc8, 0xc9, 0xca,
        0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda, 0xe2, 0xe3, 0xe4, 0xe5, 0xe6, 0xe7,
        0xe8, 0xe9, 0xea, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8, 0xf9, 0xfa,
    ],
};

/// An encode-side Huffman table: the `(code, length)` for every symbol value `0..=255`, derived
/// from a [`TableSpec`] by the canonical §C.2 procedure. `length[s] == 0` marks a symbol not in the
/// table.
#[derive(Debug, Clone)]
pub struct EncTable {
    codes: [u16; 256],
    lengths: [u8; 256],
}

impl EncTable {
    /// Builds the canonical codes for `spec` (T.81 §C.2, Figures C.1–C.3): symbols are assigned
    /// consecutive code values in HUFFVAL order, the running value doubling whenever the code length
    /// increases.
    #[must_use]
    pub fn from_spec(spec: &TableSpec) -> Self {
        Self::from_bits_values(&spec.bits, spec.values)
    }

    /// Builds the canonical codes from a raw `(BITS, HUFFVAL)` pair (§C.2), the general form of
    /// [`Self::from_spec`] that also serves the optimized per-scan tables of [`build_optimal_table`].
    #[must_use]
    pub fn from_bits_values(bits: &[u8; 16], values: &[u8]) -> Self {
        let mut codes = [0u16; 256];
        let mut lengths = [0u8; 256];
        let mut code: u16 = 0;
        let mut k = 0usize; // index into HUFFVAL
        for (length_index, &count) in bits.iter().enumerate() {
            let length = (length_index + 1) as u8;
            for _ in 0..count {
                // `values` always has exactly `sum(bits)` entries (both `from_spec`'s static tables
                // and `build_optimal_table`'s output guarantee it), so this index is in bounds.
                let symbol = values[k] as usize;
                codes[symbol] = code;
                lengths[symbol] = length;
                code += 1;
                k += 1;
            }
            // Advance to the next code length: append a 0 bit (double the value).
            code <<= 1;
        }
        Self { codes, lengths }
    }

    /// The `(code, length)` a JPEG encoder emits for `symbol`, or `None` if `symbol` is not in the
    /// table (a caller bug — every symbol the entropy coder produces is present in the standard
    /// tables).
    #[must_use]
    pub fn lookup(&self, symbol: u8) -> Option<(u16, u8)> {
        let length = self.lengths[symbol as usize];
        if length == 0 {
            None
        } else {
            Some((self.codes[symbol as usize], length))
        }
    }
}

/// A decode-side Huffman table: the canonical `MINCODE`/`MAXCODE`/`VALPTR` lookup arrays of T.81
/// Figure F.15, built from a wire `(BITS, HUFFVAL)` pair. [`crate::scan`]'s `DECODE` procedure
/// (Figure F.16) walks them to turn a bit sequence back into a symbol.
///
/// Indexed by code length `1..=16` (element 0 is unused). `maxcode[l] == -1` marks a length with no
/// codes; `mincode[l]`/`valptr[l]` are then unused for that length.
#[derive(Debug, Clone)]
pub struct DecTable {
    /// `MINCODE[l]`: the smallest canonical code of length `l` (Figure F.15).
    mincode: [i32; 17],
    /// `MAXCODE[l]`: the largest canonical code of length `l`, or `-1` if there are none.
    maxcode: [i32; 17],
    /// `VALPTR[l]`: the [`Self::values`] index of the first symbol of length `l`.
    valptr: [usize; 17],
    /// `HUFFVAL`: the symbol values ordered by increasing code length.
    values: Vec<u8>,
    /// The Figure F.16 outcome for every [`LOOKAHEAD`]-bit prefix, precomputed: `(length << 8) +
    /// value` when the procedure stops at a `length ≤ LOOKAHEAD` code that maps to `value`, and `0`
    /// otherwise (a longer code, or one the procedure rejects). The decoder's fast path; anything
    /// it marks `0` goes through the procedure itself, so the table changes speed, never outcome.
    /// §F.2.2.3 presents Figure F.16 as one implementation of `DECODE` among "many other[s]
    /// possible"; this is the usual one.
    lookup: Box<[u16; 1 << LOOKAHEAD]>,
}

/// Bits of lookahead [`DecTable::lookup`] resolves in one probe. Nine covers every DC code and the
/// common AC codes of the Annex K tables (libjpeg-turbo's `HUFF_LOOKAHEAD` is eight; zune-jpeg's
/// is nine) while keeping the table at 1 KiB.
pub(crate) const LOOKAHEAD: u32 = 9;

impl DecTable {
    /// Builds the decode tables from a wire `(bits, values)` pair (§B.2.4.2), rejecting a code space
    /// that is over-subscribed (Annex C: the codes would not be prefix-free / would need > 16 bits).
    ///
    /// `bits[i - 1]` is the number of codes of length `i` (`1..=16`) and `values` lists the
    /// `sum(bits)` symbols in canonical order (the caller guarantees `values.len() == sum(bits)` by
    /// construction from the segment). An **incomplete** table (fewer codes than the length budget
    /// allows) is accepted — real streams carry them; only an **overfull** one is rejected.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidInput`] if the code space is over-subscribed
    /// (`sum(bits[l] · 2^(16−l)) > 2^16`).
    pub fn from_bits(bits: &[u8; 16], values: &[u8]) -> Result<Self> {
        // Annex C code-space check: each length-l code claims 2^(16-l) of the depth-16 leaf space;
        // an over-subscribed table cannot be a prefix code.
        let mut used: u32 = 0;
        for (i, &count) in bits.iter().enumerate() {
            let length = (i + 1) as u32;
            used += u32::from(count) << (16 - length);
        }
        if used > (1u32 << 16) {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "JPEG: overfull Huffman code space (DHT)",
            ));
        }

        // Canonical code assignment (§C.2 / Figure F.15): consecutive codes in HUFFVAL order, the
        // running value doubling at each length increase.
        let mut mincode = [0i32; 17];
        let mut maxcode = [-1i32; 17];
        let mut valptr = [0usize; 17];
        let mut code: u32 = 0;
        let mut p: usize = 0;
        for length in 1..=16usize {
            let count = usize::from(bits[length - 1]);
            if count != 0 {
                valptr[length] = p;
                mincode[length] = code as i32;
                code += count as u32;
                maxcode[length] = (code - 1) as i32;
                p += count;
            }
            code <<= 1;
        }
        // The lookahead table, by the same canonical assignment: the code of length `l` claims the
        // `2^(LOOKAHEAD − l)` prefixes it heads. The code space is not overfull (checked above), so
        // the codes of length `≤ LOOKAHEAD` are distinct prefixes and their spans neither overlap
        // nor leave the table; an incomplete table leaves the unclaimed prefixes at `0`.
        let mut lookup = Box::new([0u16; 1 << LOOKAHEAD]);
        let mut code = 0usize;
        let mut symbols = values.iter();
        for length in 1..=LOOKAHEAD {
            let span = 1usize << (LOOKAHEAD - length);
            for _ in 0..bits[length as usize - 1] {
                let entry = (length as u16) * 256 + u16::from(*symbols.next().unwrap_or(&0));
                lookup
                    .iter_mut()
                    .skip(code * span)
                    .take(span)
                    .for_each(|e| *e = entry);
                code += 1;
            }
            code *= 2;
        }
        Ok(Self {
            mincode,
            maxcode,
            valptr,
            values: values.to_vec(),
            lookup,
        })
    }

    /// The [`Self::lookup`] entry for a [`LOOKAHEAD`]-bit prefix: `(length << 8) + value`, or `0`
    /// where the prefix does not resolve to a code of at most that length.
    #[must_use]
    pub(crate) fn lookup(&self, prefix: u32) -> u16 {
        self.lookup[prefix as usize]
    }

    /// `DECODE` (Figure F.16) over the MSB-first 16-bit window `bits`: grows the code a bit at a
    /// time until it is at most `MAXCODE` for its length, and returns how many bits the procedure
    /// read with its outcome -- the symbol, or the error a corrupt code earns. A code that matches
    /// no length up to 16 is undecodable (all 16 bits read); one whose `VALPTR` offset falls outside
    /// `HUFFVAL` is out of table.
    ///
    /// The code is grown with `+` rather than `|`: the shift just vacated the low bit and the addend
    /// is a single bit, so the addition fills exactly that vacant bit.
    pub(crate) fn walk(&self, bits: u32) -> (u32, Result<u8>) {
        let mut length = 1usize;
        let mut code = (bits >> 15) as i32;
        while code > self.maxcode(length) {
            if length == 16 {
                return (
                    16,
                    Err(Error::invalid_input(
                        env!("CARGO_PKG_NAME"),
                        "JPEG: undecodable Huffman code",
                    )),
                );
            }
            length += 1;
            code = (code << 1) + ((bits >> (16 - length)) & 1) as i32;
        }
        let symbol = self.value_at(length, code).ok_or_else(|| {
            Error::invalid_input(env!("CARGO_PKG_NAME"), "JPEG: Huffman value out of table")
        });
        (length as u32, symbol)
    }

    /// `MAXCODE[length]`, or `-1` if no code has that length.
    #[must_use]
    pub(crate) fn maxcode(&self, length: usize) -> i32 {
        self.maxcode[length]
    }

    /// The symbol at flat `HUFFVAL` index `valptr[length] + (code − mincode[length])`, or `None` if
    /// that index is past the end of `HUFFVAL` (a corrupt code that maps outside the table).
    #[must_use]
    pub(crate) fn value_at(&self, length: usize, code: i32) -> Option<u8> {
        let offset = (code - self.mincode[length]) as usize;
        self.values.get(self.valptr[length] + offset).copied()
    }
}

/// Appends a DHT marker segment (§B.2.4.2) carrying `tables`, each an `(class, id, spec)` triple:
/// `class` is 0 for DC / 1 for AC (the `Tc` nibble), `id` is the destination (`Th` nibble).
pub fn emit_dht(out: &mut Vec<u8>, tables: &[(u8, u8, &TableSpec)]) {
    // Lh = 2 length bytes + per table (1 Tc/Th byte + 16 BITS bytes + sum(BITS) HUFFVAL bytes).
    let len = 2 + tables
        .iter()
        .map(|(_, _, spec)| 1 + 16 + spec.values.len())
        .sum::<usize>();
    marker::write_segment_header(out, code::DHT, len);
    for &(class, id, spec) in tables {
        out.push(marker::pack_nibbles(class, id)); // Tc (high nibble) | Th (low nibble)
        out.extend_from_slice(&spec.bits);
        out.extend_from_slice(spec.values);
    }
}

/// Appends a DHT marker segment (§B.2.4.2) carrying dynamically-built `(class, id, BITS, HUFFVAL)`
/// tables — the owned counterpart of [`emit_dht`], used by the progressive encoder for its optimized
/// per-scan tables ([`build_optimal_table`]). `class` is 0 for DC / 1 for AC; `id` is the destination.
pub fn emit_dht_dynamic(out: &mut Vec<u8>, tables: &[(u8, u8, &[u8; 16], &[u8])]) {
    let len = 2 + tables
        .iter()
        .map(|(_, _, _, values)| 1 + 16 + values.len())
        .sum::<usize>();
    marker::write_segment_header(out, code::DHT, len);
    for &(class, id, bits, values) in tables {
        out.push(marker::pack_nibbles(class, id));
        out.extend_from_slice(bits);
        out.extend_from_slice(values);
    }
}

/// Builds an optimized Huffman table (T.81 Annex K.2, Figures K.1–K.3) from per-symbol frequency
/// counts, returning the `(BITS, HUFFVAL)` wire pair (§B.2.4.2).
///
/// `freq[s]` is how many times symbol `s` (`0..=255`) is emitted in the scan. The procedure is the
/// reference one (mirrored from libjpeg's `jpeg_gen_optimal_table`, which cites this clause):
///
/// 1. A **reserved pseudo-symbol** (value 256, frequency 1) is added to the alphabet. Because it is
///    placed last in the longest code-length category and then removed, no *real* symbol is ever
///    assigned the all-ones code of its length — the code word T.81 reserves (§K.2, Figure K.1).
/// 2. Huffman's algorithm assigns each symbol an initial code length by repeatedly merging the two
///    least-frequent subtrees (ties broken toward the larger symbol number, matching the reference).
/// 3. The **16-bit length-limiting** adjustment (Figure K.3) redistributes any code longer than 16
///    bits into the ≤ 16-bit budget, pairing symbols off the longest length two at a time.
/// 4. The pseudo-symbol's count is removed from the largest length, and the symbols are listed in
///    HUFFVAL ordered by their (pre-adjustment) code length.
///
/// The returned `BITS` has `sum(BITS)` equal to the number of distinct symbols with non-zero
/// frequency, and no code of any length is all-ones. An all-zero `freq` yields an empty table.
#[must_use]
pub fn build_optimal_table(freq: &[u32; 256]) -> ([u8; 16], Vec<u8>) {
    // Sentinels chosen so a merged frequency (≤ total coefficients coded ≪ 2^62) is always < LIVE_MAX
    // and a merged-away node (REMOVED) is always > LIVE_MAX, so neither is ever picked as a minimum.
    const LIVE_MAX: u64 = 1u64 << 62;
    const REMOVED: u64 = 1u64 << 63;
    // Longest code length the tree can produce for a 257-symbol alphabet is < 257 bits; sizing every
    // length-indexed array to 259 keeps the whole procedure panic-free for any frequency multiset.
    const N: usize = 259;

    // Group the non-zero frequencies (plus the reserved pseudo-symbol 256) together, remembering each
    // one's original symbol value. Ties later resolve toward the larger symbol number (as libjpeg's
    // grouped scan does), which the ordering of this pass preserves.
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
    nz_index[num_nz] = 256;
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

    // Count symbols of each code length, then the running count of shorter symbols (bit_pos), from the
    // *pre-adjustment* lengths — the HUFFVAL ordering below indexes by these original lengths (§K.2).
    let mut bits = [0i32; N];
    for &cl in codesize.iter().take(num_nz) {
        bits[cl as usize] += 1;
    }
    let mut bit_pos = [0i32; N];
    let mut p = 0i32;
    for i in 1..N {
        bit_pos[i] = p;
        p += bits[i];
    }

    // 16-bit length limiting (Figure K.3): pull symbol pairs off any length > 16 down into shorter
    // categories until nothing exceeds 16 bits.
    for i in (17..N).rev() {
        while bits[i] > 0 {
            let mut j = i - 2; // find the longest shorter length with a code to lend (skip i−1)
            while bits[j] == 0 {
                j -= 1;
            }
            bits[i] -= 2; // remove two symbols from length i
            bits[i - 1] += 1; // one moves up a level (its own prefix)
            bits[j + 1] += 2; // two new symbols one level below the borrowed prefix
            bits[j] -= 1; // the borrowed code becomes a prefix
        }
    }

    // Remove the reserved pseudo-symbol from the largest code length still in use (§K.2): it took the
    // all-ones code of that length, so dropping it leaves no real symbol with the reserved code word.
    if let Some(i) = (1..=16usize).rev().find(|&i| bits[i] > 0) {
        bits[i] -= 1;
    }

    // Emit BITS (lengths 1..=16) and HUFFVAL (every real symbol, ordered by its original code length;
    // the pseudo-symbol is the last grouped entry and is skipped).
    let mut out_bits = [0u8; 16];
    for (dst, &b) in out_bits.iter_mut().zip(bits[1..=16].iter()) {
        *dst = b as u8;
    }
    let mut values = vec![0u8; num_nz.saturating_sub(1)];
    for i in 0..num_nz.saturating_sub(1) {
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

    /// `sum(BITS)` must equal the HUFFVAL length for a well-formed table (each code names one
    /// symbol), and no length exceeds 16.
    fn assert_consistent(spec: &TableSpec, ctx: &str) {
        let total: usize = spec.bits.iter().map(|&b| usize::from(b)).sum();
        assert_eq!(total, spec.values.len(), "{ctx}: sum(BITS) != len(HUFFVAL)");
    }

    #[test]
    fn standard_tables_are_consistent() {
        assert_consistent(&STD_LUMA_DC, "luma DC");
        assert_consistent(&STD_CHROMA_DC, "chroma DC");
        assert_consistent(&STD_LUMA_AC, "luma AC");
        assert_consistent(&STD_CHROMA_AC, "chroma AC");
        // The AC tables carry 162 symbols each (Annex K.5/K.6); the DC tables 12 (categories 0..11).
        assert_eq!(STD_LUMA_AC.values.len(), 162);
        assert_eq!(STD_CHROMA_AC.values.len(), 162);
        assert_eq!(STD_LUMA_DC.values.len(), 12);
        assert_eq!(STD_CHROMA_DC.values.len(), 12);
        // Spot anchors straight from the spec: EOB (0x00) and ZRL (0xF0) are present in the AC
        // tables, and the luma-AC BITS[16] tail is 0x7d.
        assert!(STD_LUMA_AC.values.contains(&0x00) && STD_LUMA_AC.values.contains(&0xF0));
        assert_eq!(STD_LUMA_AC.bits[15], 0x7d);
        assert_eq!(STD_CHROMA_AC.bits[15], 0x77);
    }

    #[test]
    fn luma_dc_canonical_codes_match_annex_c() {
        // Hand-derived from Table K.3 via the §C.2 procedure:
        //   category 0 → length 2, code 00  (binary 0b00 = 0)
        //   categories 1..5 → length 3, codes 010,011,100,101,110 (0b010=2 .. 0b110=6)
        //   category 6 → length 4, code 1110 (0b1110 = 14)
        //   category 7 → length 5, code 11110 (0b11110 = 30)
        let t = EncTable::from_spec(&STD_LUMA_DC);
        assert_eq!(t.lookup(0), Some((0b00, 2)));
        assert_eq!(t.lookup(1), Some((0b010, 3)));
        assert_eq!(t.lookup(2), Some((0b011, 3)));
        assert_eq!(t.lookup(5), Some((0b110, 3)));
        assert_eq!(t.lookup(6), Some((0b1110, 4)));
        assert_eq!(t.lookup(7), Some((0b11110, 5)));
        assert_eq!(t.lookup(11), Some((0b111111110, 9)));
    }

    #[test]
    fn chroma_dc_canonical_codes_match_annex_c() {
        // Table K.4: three length-2 codes (00,01,10) then one per length 3..11.
        let t = EncTable::from_spec(&STD_CHROMA_DC);
        assert_eq!(t.lookup(0), Some((0b00, 2)));
        assert_eq!(t.lookup(1), Some((0b01, 2)));
        assert_eq!(t.lookup(2), Some((0b10, 2)));
        assert_eq!(t.lookup(3), Some((0b110, 3)));
        assert_eq!(t.lookup(11), Some((0b11111111110, 11)));
    }

    #[test]
    fn luma_ac_first_codes_match_annex_c() {
        // Table K.5 sheet 1: 0/1 and 0/2 are the two length-2 codes (00, 01); 0/3 the single
        // length-3 code (100); EOB (0/0), 0/4, 1/1 the three length-4 codes (1010, 1011, 1100).
        let t = EncTable::from_spec(&STD_LUMA_AC);
        assert_eq!(t.lookup(0x01), Some((0b00, 2)));
        assert_eq!(t.lookup(0x02), Some((0b01, 2)));
        assert_eq!(t.lookup(0x03), Some((0b100, 3)));
        assert_eq!(t.lookup(0x00), Some((0b1010, 4))); // EOB
        assert_eq!(t.lookup(0x04), Some((0b1011, 4)));
        assert_eq!(t.lookup(0x11), Some((0b1100, 4)));
        // ZRL (0xF0) sits at HUFFVAL index 31, in the length-11 bucket → 0b11111111001 = 2041.
        assert_eq!(t.lookup(0xF0), Some((0b11111111001, 11)));
    }

    #[test]
    fn the_standard_tables_are_prefix_free() {
        // No code is a prefix of another (the defining property of a valid Huffman code): for every
        // pair of used symbols, the shorter code must differ from the longer code's leading bits.
        //
        // This subsumes uniqueness, which the name used to claim separately: two identical codes of
        // equal length are a prefix collision, so the check below already rejects them.
        for spec in [&STD_LUMA_DC, &STD_CHROMA_DC, &STD_LUMA_AC, &STD_CHROMA_AC] {
            let t = EncTable::from_spec(spec);
            let used: Vec<(u16, u8)> = (0..=255u16).filter_map(|s| t.lookup(s as u8)).collect();
            for (i, &(ci, li)) in used.iter().enumerate() {
                for &(cj, lj) in used.iter().skip(i + 1) {
                    let (short, ls, long, ll) = if li <= lj {
                        (ci, li, cj, lj)
                    } else {
                        (cj, lj, ci, li)
                    };
                    let prefix = long >> (ll - ls);
                    assert_ne!(short, prefix, "prefix collision in a standard table");
                }
            }
        }
    }

    #[test]
    fn lookup_absent_symbol_is_none() {
        // The luma DC table has no symbol 12 (categories stop at 11).
        assert_eq!(EncTable::from_spec(&STD_LUMA_DC).lookup(12), None);
    }

    #[test]
    fn dht_emits_class_id_bits_and_values() {
        // One luma DC table, class 0, id 0. Length = 2 + (1 + 16 + 12) = 31.
        let mut out = Vec::new();
        emit_dht(&mut out, &[(0, 0, &STD_LUMA_DC)]);
        assert_eq!(&out[..2], &[0xFF, 0xC4]); // DHT
        assert_eq!(&out[2..4], &[0x00, 31]); // Lh
        assert_eq!(out[4], 0x00); // Tc=0, Th=0
        assert_eq!(&out[5..21], &STD_LUMA_DC.bits); // BITS
        assert_eq!(&out[21..33], STD_LUMA_DC.values); // HUFFVAL
        assert_eq!(out.len(), 2 + 31);
        // An AC table sets the Tc nibble to 1; its id nibble to the destination.
        let mut ac = Vec::new();
        emit_dht(&mut ac, &[(1, 1, &STD_CHROMA_AC)]);
        assert_eq!(ac[4], 0x11); // Tc=1 (AC), Th=1
    }

    /// Reference DECODE (Figure F.16) over a [`DecTable`], reading `code` from an explicit MSB-first
    /// bit iterator — the inverse of the canonical assignment, used only to pin `DecTable`.
    fn decode_one(table: &DecTable, bits: &mut impl Iterator<Item = u8>) -> Option<u8> {
        let mut length = 1usize;
        let mut code = i32::from(bits.next()?);
        while code > table.maxcode(length) {
            length += 1;
            if length > 16 {
                return None;
            }
            code = (code << 1) | i32::from(bits.next()?);
        }
        table.value_at(length, code)
    }

    #[test]
    fn dectable_lookup_agrees_with_the_walk_on_every_window() {
        // `DecTable::from_bits`'s lookahead table is the Figure F.16 walk, tabulated: for every
        // 16-bit window, a non-zero entry for its 9-bit prefix is exactly the walk's (length,
        // symbol), and a zero entry is exactly a window the walk resolves to a longer code or
        // rejects. Over the four Annex K tables plus one with a code of every length 1..=16 (so
        // every span size, both sides of the lookahead boundary, and an unassigned window).
        let every_length = ([1u8; 16], (1..=16).collect::<Vec<u8>>());
        let tables = [&STD_LUMA_DC, &STD_CHROMA_DC, &STD_LUMA_AC, &STD_CHROMA_AC]
            .map(|s| (s.bits, s.values.to_vec()))
            .into_iter()
            .chain([every_length]);
        for (bits, values) in tables {
            let dec = DecTable::from_bits(&bits, &values).unwrap();
            for window in 0..=u16::MAX {
                let window = u32::from(window);
                let entry = dec.lookup(window >> (16 - LOOKAHEAD));
                match dec.walk(window) {
                    (length, Ok(symbol)) if length <= LOOKAHEAD => {
                        assert_eq!(entry, (length as u16) * 256 + u16::from(symbol));
                    }
                    _ => assert_eq!(entry, 0, "window {window:#06x}"),
                }
            }
        }
    }

    #[test]
    fn dectable_walk_reports_the_bits_it_read() {
        // `DecTable::walk` on the every-length table: a length-12 code (eleven ones, a zero) reads
        // 12 bits, and sixteen ones -- the one unassigned window -- read all 16 and are rejected.
        let dec = DecTable::from_bits(&[1u8; 16], &(1..=16).collect::<Vec<u8>>()).unwrap();
        let (length, symbol) = dec.walk(0b1111_1111_1110_0101);
        assert_eq!((length, symbol.unwrap()), (12, 12));
        let (length, symbol) = dec.walk(0xFFFF);
        assert_eq!(length, 16);
        assert_eq!(
            symbol.unwrap_err().static_message(),
            Some("JPEG: undecodable Huffman code")
        );
    }

    #[test]
    fn dectable_inverts_the_canonical_encoder() {
        // For every standard table, DECODE of each symbol's canonical (code,length) bit sequence
        // recovers that symbol — DecTable is the exact inverse of EncTable's §C.2 assignment.
        for spec in [&STD_LUMA_DC, &STD_CHROMA_DC, &STD_LUMA_AC, &STD_CHROMA_AC] {
            let enc = EncTable::from_spec(spec);
            let dec = DecTable::from_bits(&spec.bits, spec.values).unwrap();
            for &sym in spec.values {
                let (code, len) = enc.lookup(sym).unwrap();
                let mut bits = (0..len).map(|i| ((code >> (len - 1 - i)) & 1) as u8);
                assert_eq!(decode_one(&dec, &mut bits), Some(sym), "symbol {sym:#x}");
            }
        }
    }

    /// Every symbol with a non-zero frequency must have a code, and no code may be all-ones at its
    /// length (the T.81 §K.2 reservation) — the two invariants every optimized table must satisfy.
    fn assert_optimal_invariants(freq: &[u32; 256], bits: &[u8; 16], values: &[u8]) {
        let total: usize = bits.iter().map(|&b| usize::from(b)).sum();
        assert_eq!(total, values.len(), "sum(BITS) != len(HUFFVAL)");
        let nz = freq.iter().filter(|&&f| f != 0).count();
        assert_eq!(total, nz, "one code per distinct symbol");
        let t = EncTable::from_bits_values(bits, values);
        for (s, &f) in freq.iter().enumerate() {
            if f != 0 {
                assert!(t.lookup(s as u8).is_some(), "symbol {s:#x} has no code");
            }
        }
        // No emitted code is all-ones at its own length (the reserved code word).
        for (s, &f) in freq.iter().enumerate() {
            if f != 0 {
                let (code, len) = t.lookup(s as u8).unwrap();
                let all_ones = ((1u32 << len) - 1) as u16; // u32 avoids overflow at len == 16
                assert_ne!(
                    code, all_ones,
                    "symbol {s:#x} got the reserved all-ones code"
                );
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
            EncTable::from_bits_values(&bits, &values).lookup(5),
            Some((0, 1))
        );
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
        let t = EncTable::from_bits_values(&bits, &values);
        assert_eq!(t.lookup(1), Some((0, 1)));
        assert_eq!(t.lookup(2), Some((0b10, 2)));
        assert_optimal_invariants(&freq, &bits, &values);
    }

    #[test]
    fn optimal_skewed_frequencies_force_16_bit_limiting() {
        // Frequencies doubling per symbol (`2^i`) each exceed the sum of all smaller ones, so
        // Huffman's pure algorithm builds a fully degenerate chain whose deepest code is ~20 bits —
        // well past JPEG's 16-bit limit — driving the Figure K.3 length-limiting adjustment. The
        // result must still be valid: every length ≤ 16, one code per symbol, no all-ones code.
        let mut freq = [0u32; 256];
        for (i, slot) in freq.iter_mut().take(20).enumerate() {
            *slot = 1u32 << i;
        }
        let (bits, values) = build_optimal_table(&freq);
        // Exact golden (a regression pin over the whole reduction): the degenerate chain of 20 symbols
        // + the reserved pseudo-symbol reduces to one code at each length 1..=13 and the seven deepest
        // pushed to length 16, with the symbols ordered least-frequent (deepest) last. Any mutation of
        // the Figure K.3 redistribution changes these bytes.
        assert_eq!(bits, [1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 7]);
        assert_eq!(
            values,
            vec![
                19, 18, 17, 16, 15, 14, 13, 12, 11, 10, 9, 8, 7, 6, 5, 4, 3, 2, 1, 0
            ]
        );
        // Length-16 codes exist (a 20-symbol *balanced* tree, max depth ~5, never needs them): a
        // witness that the >16 reduction ran. And the table is still a valid, never-overfull prefix.
        assert!(bits[15] > 0);
        assert_optimal_invariants(&freq, &bits, &values);
        assert!(DecTable::from_bits(&bits, &values).is_ok());
    }

    #[test]
    fn optimal_single_eob_symbol_scan() {
        // The empty-band edge: a scan whose only emitted symbol is EOB0 (0x00). One real symbol → a
        // one-code table, exactly as the single-symbol case, proving the builder handles a minimal
        // AC scan.
        let mut freq = [0u32; 256];
        freq[0x00] = 7;
        let (bits, values) = build_optimal_table(&freq);
        assert_eq!(values, vec![0x00]);
        assert_eq!(bits[0], 1);
        assert_optimal_invariants(&freq, &bits, &values);
    }

    #[test]
    fn optimal_empty_frequency_is_an_empty_table() {
        // No symbols at all (a degenerate input) yields an all-zero BITS and no HUFFVAL — never a
        // panic. (The encoder never calls the builder with an empty multiset, but the primitive is
        // total.)
        let (bits, values) = build_optimal_table(&[0u32; 256]);
        assert_eq!(bits, [0u8; 16]);
        assert!(values.is_empty());
    }

    #[test]
    fn optimal_many_symbols_round_trip_through_dectable() {
        // A broad multiset (all 162 valid AC symbols with varied counts) builds a table that a
        // DecTable inverts exactly — the strongest correctness check that BITS/HUFFVAL are coherent.
        let mut freq = [0u32; 256];
        for (i, sym) in STD_LUMA_AC.values.iter().enumerate() {
            freq[usize::from(*sym)] = (i as u32 % 13) + 1;
        }
        let (bits, values) = build_optimal_table(&freq);
        assert_optimal_invariants(&freq, &bits, &values);
        let enc = EncTable::from_bits_values(&bits, &values);
        let dec = DecTable::from_bits(&bits, &values).unwrap();
        for &sym in &values {
            let (code, len) = enc.lookup(sym).unwrap();
            let mut b = (0..len).map(|i| ((code >> (len - 1 - i)) & 1) as u8);
            assert_eq!(decode_one(&dec, &mut b), Some(sym), "symbol {sym:#x}");
        }
    }

    #[test]
    fn emit_dht_dynamic_matches_static_layout() {
        // The owned emitter must produce the same bytes as the static one for the same table.
        let bits = STD_LUMA_DC.bits;
        let values = STD_LUMA_DC.values;
        let mut dynamic = Vec::new();
        emit_dht_dynamic(&mut dynamic, &[(0, 0, &bits, values)]);
        let mut stat = Vec::new();
        emit_dht(&mut stat, &[(0, 0, &STD_LUMA_DC)]);
        assert_eq!(dynamic, stat);
    }

    #[test]
    fn dectable_rejects_overfull_code_space() {
        // Three codes of length 1 need 3·2^15 > 2^16 leaf slots — impossible for a prefix code.
        let mut bits = [0u8; 16];
        bits[0] = 3;
        assert!(DecTable::from_bits(&bits, &[0, 1, 2]).is_err());
        // Exactly two length-1 codes fill the space (2·2^15 = 2^16): the boundary is accepted.
        bits[0] = 2;
        assert!(DecTable::from_bits(&bits, &[0, 1]).is_ok());
        // An incomplete table (one length-2 code, three slots unused) is accepted — real streams
        // carry them.
        let mut sparse = [0u8; 16];
        sparse[1] = 1;
        assert!(DecTable::from_bits(&sparse, &[7]).is_ok());
    }
}
