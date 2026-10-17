//! Huffman code tables: the T.81 Annex K.3–K.6 typical tables, the encode-side symbol→code lookup
//! over `gamut-bitstream`'s canonical codes (Annex C), the decode-side lookup, and DHT emission
//! (§B.2.4.2).
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
//! instead fit a table to the image: [`gamut_bitstream::build_optimal_table`] runs the §K.2
//! construction over measured symbol frequencies and [`emit_dht_dynamic`] writes the result —
//! mandatory for progressive (the typical AC tables cannot code an `EOBn` symbol) and opt-in for
//! baseline via [`JpegEncoder::with_optimized_tables`](crate::JpegEncoder::with_optimized_tables).
//!
//! The §K.2 construction and the §C.2 assignment are shared with lossless JPEG, so
//! `gamut-bitstream` owns both ([`gamut_bitstream::build_optimal_table`],
//! [`gamut_bitstream::CanonicalCodes`]); this module keeps what is JPEG's own: the Annex K tables,
//! DHT emission, and the decode-side lookup.

use gamut_bitstream::CanonicalCodes;
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
/// by the canonical §C.2 procedure, which [`CanonicalCodes`] owns; this type adds only the
/// construction from a [`TableSpec`].
#[derive(Debug, Clone)]
pub struct EncTable(CanonicalCodes);

impl EncTable {
    /// Builds the canonical codes for `spec` (T.81 §C.2, Figures C.1–C.3): symbols are assigned
    /// consecutive code values in HUFFVAL order, the running value doubling whenever the code length
    /// increases.
    #[must_use]
    pub fn from_spec(spec: &TableSpec) -> Self {
        Self::from_bits_values(&spec.bits, spec.values)
    }

    /// Builds the canonical codes from a raw `(BITS, HUFFVAL)` pair (§C.2), the general form of
    /// [`Self::from_spec`] that also serves the optimized per-scan tables of
    /// [`gamut_bitstream::build_optimal_table`].
    #[must_use]
    pub fn from_bits_values(bits: &[u8; 16], values: &[u8]) -> Self {
        Self(CanonicalCodes::from_bits_values(bits, values))
    }

    /// The `(code, length)` a JPEG encoder emits for `symbol`, or `None` if `symbol` is not in the
    /// table (a caller bug — every symbol the entropy coder produces is present in the standard
    /// tables).
    #[must_use]
    pub fn lookup(&self, symbol: u8) -> Option<(u16, u8)> {
        self.0.lookup(symbol)
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
    /// is a single bit, so the addition fills exactly that vacant bit. The walk is a `for` over the
    /// sixteen lengths, so it ends after at most sixteen steps however its arithmetic is perturbed.
    pub(crate) fn walk(&self, bits: u32) -> (u32, Result<u8>) {
        let mut code = 0i32;
        for length in 1..=16usize {
            code = (code << 1) + ((bits >> (16 - length)) & 1) as i32;
            if code <= self.maxcode(length) {
                let symbol = self.value_at(length, code).ok_or_else(|| {
                    Error::invalid_input(env!("CARGO_PKG_NAME"), "JPEG: Huffman value out of table")
                });
                return (length as u32, symbol);
            }
        }
        (
            16,
            Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "JPEG: undecodable Huffman code",
            )),
        )
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
/// per-scan tables ([`gamut_bitstream::build_optimal_table`]). `class` is 0 for DC / 1 for AC; `id` is the destination.
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

    #[test]
    fn dectable_inverts_an_optimal_table() {
        // A broad multiset (all 162 valid AC symbols with varied counts) builds a table, through
        // gamut-bitstream's §K.2 builder, that a DecTable inverts exactly: the decode side accepts
        // what the optimized-table encoders write.
        let mut freq = [0u32; 256];
        for (i, sym) in STD_LUMA_AC.values.iter().enumerate() {
            freq[usize::from(*sym)] = (i as u32 % 13) + 1;
        }
        let (bits, values) = gamut_bitstream::build_optimal_table(&freq);
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
