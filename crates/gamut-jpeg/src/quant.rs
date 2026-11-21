//! Quantization tables: the T.81 Annex K.1 base tables and the IJG quality→scale mapping.
//!
//! [`LUMINANCE`] and [`CHROMINANCE`] are the Annex K.1 / K.2 example tables verbatim, in **natural**
//! (row-major) order. [`scale`] applies the de-facto-standard IJG/libjpeg quality mapping to a base
//! table, and [`emit_dqt`] serializes the scaled tables into a DQT marker segment (§B.2.4.1) in
//! zig-zag order.
//!
//! # Frozen quality contract
//!
//! For a given `quality` the scaled table bytes are **SemVer-frozen**: the mapping is
//! `scale = if q < 50 { 5000 / q } else { 200 - 2·q }`, then each base entry becomes
//! `clamp((base·scale + 50) / 100, 1, 255)` (§A.3.4 requires 8-bit `Q` values in `1..=255` for
//! baseline `Pq = 0`). Quality 50 reproduces the Annex K tables exactly; quality 100 collapses every
//! entry to 1 (`scale = 0` → `(0 + 50)/100 = 0`, clamped up to 1). Changing these bytes for a fixed
//! `(quality, subsampling)` is a breaking change.
//!
//! The contract governs the default quality path only: caller-supplied [`QuantTables`]
//! ([`JpegEncoder::with_quant_tables`](crate::JpegEncoder::with_quant_tables)) bypass the mapping
//! without changing it.
//!
//! # 16-bit tables
//!
//! Table entries are `u16`. A table whose entries all fit `1..=255` is written with `Pq = 0`, one
//! byte per entry; a table holding any entry above 255 is written with `Pq = 1`, two big-endian
//! bytes per entry, and the encoder then marks the sequential frame **SOF1** (extended
//! sequential, T.81 Table B.4) instead of SOF0 — baseline allows only `Pq = 0`. A progressive
//! frame stays SOF2, whose Table B.4 column admits both precisions. This is libjpeg's behaviour
//! (`jcmarker.c`: `emit_dqt` picks the precision per table, `write_frame_header` drops to SOF1),
//! and [`scale_extended`] / [`QuantTables::scaled_extended`] reproduce the `cjpeg` default
//! (`force_baseline = FALSE`) mapping that produces such tables from quality 23 down.
//!
//! T.81 §B.2.4.1 says "`Pq` shall be zero for 8 bit sample precision", which a 16-bit table under an
//! 8-bit frame does not honour; the libjpeg family has always written (and read) them, so the
//! encoder follows it rather than the clause. Every table that fits eight bits — every default
//! quality path, and every caller table with no entry above 255 — stays `Pq = 0` under SOF0.

use gamut_core::{Error, Result};

use crate::marker::{self, code, pack_nibbles};
use crate::zigzag::ZIGZAG;

/// The largest entry an 8-bit (`Pq = 0`) table carries (§B.2.4.1, Table B.4) — the ceiling of the
/// frozen baseline quality mapping.
const BASELINE_MAX: u32 = 255;

/// The largest entry the unforced quality mapping produces: libjpeg's `jpeg_add_quant_table` caps
/// `force_baseline = FALSE` tables at 32767 (`jcparam.c`), and [`scale_extended`] matches it.
const EXTENDED_MAX: u32 = 32767;

/// T.81 Table K.1 — the example **luminance** quantization table, natural (row-major) order.
///
/// Suitable, per Annex K, for 8-bit luminance components; used for the Y component and for
/// grayscale images.
pub const LUMINANCE: [u16; 64] = [
    16, 11, 10, 16, 24, 40, 51, 61, //
    12, 12, 14, 19, 26, 58, 60, 55, //
    14, 13, 16, 24, 40, 57, 69, 56, //
    14, 17, 22, 29, 51, 87, 80, 62, //
    18, 22, 37, 56, 68, 109, 103, 77, //
    24, 35, 55, 64, 81, 104, 113, 92, //
    49, 64, 78, 87, 103, 121, 120, 101, //
    72, 92, 95, 98, 112, 100, 103, 99, //
];

/// T.81 Table K.2 — the example **chrominance** quantization table, natural (row-major) order.
///
/// Suitable, per Annex K, for 8-bit chrominance (Cb/Cr) components.
pub const CHROMINANCE: [u16; 64] = [
    17, 18, 24, 47, 99, 99, 99, 99, //
    18, 21, 26, 66, 99, 99, 99, 99, //
    24, 26, 56, 99, 99, 99, 99, 99, //
    47, 66, 99, 99, 99, 99, 99, 99, //
    99, 99, 99, 99, 99, 99, 99, 99, //
    99, 99, 99, 99, 99, 99, 99, 99, //
    99, 99, 99, 99, 99, 99, 99, 99, //
    99, 99, 99, 99, 99, 99, 99, 99, //
];

/// A validated pair of baseline quantization tables in **natural** (row-major) order: one for the
/// luminance destination (`Tq = 0`; also the only table a grayscale frame uses) and one for the
/// chrominance destination (`Tq = 1`), plus an optional third table for the X channel of the XYB
/// colour mode ([`Self::with_xyb_x`], destination `Tq = 2`).
///
/// Every entry is in the `1..=65535` range Table B.4 gives a quantization value — `u16` caps the
/// top and [`Self::new`] rejects zero — so an encoder holding a `QuantTables` never divides by
/// zero and never emits a DQT segment its own decoder would refuse. A table whose entries all fit
/// `1..=255` is written as an 8-bit (`Pq = 0`) table under a baseline (SOF0) frame; any entry above
/// 255 makes the table 16-bit (`Pq = 1`) and a sequential frame SOF1, libjpeg's convention where
/// T.81 §B.2.4.1 would keep `Pq = 0` at 8-bit precision (`STATUS.md` records the deviation).
///
/// Handed to [`JpegEncoder::with_quant_tables`](crate::JpegEncoder::with_quant_tables), the tables
/// are used verbatim, replacing the frozen quality→scale mapping; [`Self::annex_k`] and
/// [`Self::scaled`] recover that mapping over arbitrary base tables, and
/// [`Self::scaled_extended`] the `cjpeg` default that lets entries exceed 255.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuantTables {
    luma: [u16; 64],
    chroma: [u16; 64],
    xyb_x: Option<[u16; 64]>,
}

/// Rejects a table holding a zero entry as `InvalidInput` (Table B.4: `Q` is `1..=255` for 8-bit
/// and `1..=65535` for 16-bit tables).
fn reject_zero(table: &[u16; 64]) -> Result<()> {
    if table.contains(&0) {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "JPEG: quantization table entries must be in 1..=65535 (zero is illegal)",
        ));
    }
    Ok(())
}

impl QuantTables {
    /// Builds a table pair from natural-order entries, rejecting any zero entry as
    /// `InvalidInput` (Table B.4 gives `Q` the range `1..=65535`). Entries above 255 are legal and
    /// make the stream extended sequential (SOF1) rather than baseline — see [`QuantTables`].
    pub fn new(luma: [u16; 64], chroma: [u16; 64]) -> Result<Self> {
        reject_zero(&luma)?;
        reject_zero(&chroma)?;
        Ok(Self {
            luma,
            chroma,
            xyb_x: None,
        })
    }

    /// The T.81 Annex K.1/K.2 example tables verbatim — the pair the default quality path scales.
    #[must_use]
    pub fn annex_k() -> Self {
        Self {
            luma: LUMINANCE,
            chroma: CHROMINANCE,
            xyb_x: None,
        }
    }

    /// Adds a separate natural-order table for the **X** channel of the XYB colour mode
    /// ([`JpegColorMode::Xyb`](crate::JpegColorMode::Xyb)), rejecting any zero entry as
    /// `InvalidInput` like [`Self::new`].
    ///
    /// Without it, XYB mode quantizes X and Y together with the luminance table (destination
    /// `Tq = 0`). With it, X quantizes with this table at destination `Tq = 2` — a third DQT
    /// entry — so X can be quantized more coarsely (or finely) than Y; Y keeps the luminance
    /// table and the stored B−Y the chrominance table. The table is ignored outside XYB mode,
    /// where the YCbCr and grayscale frames have no X channel.
    pub fn with_xyb_x(mut self, x: [u16; 64]) -> Result<Self> {
        reject_zero(&x)?;
        self.xyb_x = Some(x);
        Ok(self)
    }

    /// These tables re-scaled by the frozen IJG quality mapping (`quality` clamped to `1..=100`,
    /// matching [`JpegEncoder::with_quality`](crate::JpegEncoder::with_quality)). Infallible: the
    /// mapping clamps every output entry into `1..=255`, so the result is always baseline.
    /// `QuantTables::annex_k().scaled(q)` reproduces exactly the tables `with_quality(q)` uses. A
    /// [`Self::with_xyb_x`] table is scaled by the same mapping.
    #[must_use]
    pub fn scaled(&self, quality: u8) -> Self {
        self.map(|t| scale(t, quality.clamp(1, 100)))
    }

    /// These tables re-scaled by the IJG quality mapping as `cjpeg` applies it by default —
    /// libjpeg's `jpeg_set_quality(.., force_baseline = FALSE)` — clamping each entry to
    /// `1..=32767` instead of `1..=255`. Above quality 23 every Annex K
    /// entry still fits eight bits, so the result equals [`Self::scaled`]; from 23 down the coarsest
    /// entries exceed 255 and the encode becomes extended sequential (SOF1) with 16-bit tables,
    /// byte-for-byte the DQT `cjpeg -quality q` writes. `quality` is clamped to `1..=100`.
    #[must_use]
    pub fn scaled_extended(&self, quality: u8) -> Self {
        self.map(|t| scale_extended(t, quality.clamp(1, 100)))
    }

    /// Applies `f` to every table this pair holds (the optional X table included).
    fn map(&self, f: impl Fn(&[u16; 64]) -> [u16; 64]) -> Self {
        Self {
            luma: f(&self.luma),
            chroma: f(&self.chroma),
            xyb_x: self.xyb_x.as_ref().map(&f),
        }
    }

    /// The luminance (`Tq = 0`) table, natural order.
    #[must_use]
    pub fn luma(&self) -> &[u16; 64] {
        &self.luma
    }

    /// The chrominance (`Tq = 1`) table, natural order.
    #[must_use]
    pub fn chroma(&self) -> &[u16; 64] {
        &self.chroma
    }

    /// The separate XYB **X** (`Tq = 2`) table, natural order, or `None` when X shares the
    /// luminance table ([`Self::with_xyb_x`]).
    #[must_use]
    pub fn xyb_x(&self) -> Option<&[u16; 64]> {
        self.xyb_x.as_ref()
    }
}

/// The IJG quality→scale percentage for `quality` in `1..=100`: `5000/q` below 50, `200 − 2·q`
/// at 50 and above. A larger scale means coarser quantization (smaller files, lower fidelity).
fn quality_scale(quality: u8) -> u32 {
    let q = u32::from(quality);
    if q < 50 { 5000 / q } else { 200 - 2 * q }
}

/// Scales a base quantization table for `quality` (`1..=100`) per the frozen contract above,
/// clamping each entry to the baseline 8-bit range `1..=255`.
#[must_use]
pub fn scale(base: &[u16; 64], quality: u8) -> [u16; 64] {
    scale_clamped(base, quality, BASELINE_MAX)
}

/// Scales a base quantization table for `quality` (`1..=100`) by the same IJG mapping as
/// [`scale`], but clamping each entry to `1..=32767` as libjpeg does with
/// `force_baseline = FALSE` (`jcparam.c`, `jpeg_add_quant_table`) — `cjpeg`'s default. Entries
/// above 255 are written as a 16-bit table under an SOF1 frame.
#[must_use]
pub fn scale_extended(base: &[u16; 64], quality: u8) -> [u16; 64] {
    scale_clamped(base, quality, EXTENDED_MAX)
}

/// The IJG mapping with each entry clamped to `1..=max` (`max` is at most [`EXTENDED_MAX`], so the
/// narrowing cast is exact).
fn scale_clamped(base: &[u16; 64], quality: u8, max: u32) -> [u16; 64] {
    let s = quality_scale(quality);
    // (b·scale + 50) / 100 rounds to nearest. `b ≤ 65535` and `scale ≤ 5000`, so the product
    // stays below 2^32.
    base.map(|b| ((u32::from(b) * s + 50) / 100).clamp(1, max) as u16)
}

/// Whether `table` needs 16-bit (`Pq = 1`) precision: some entry exceeds the 8-bit ceiling of 255.
pub(crate) fn is_16bit(table: &[u16; 64]) -> bool {
    table.iter().any(|&v| u32::from(v) > BASELINE_MAX)
}

/// Appends a DQT marker segment (§B.2.4.1) carrying `tables`, each an `(id, values)` pair whose
/// `values` are in **natural** order and are re-emitted in zig-zag order, and returns whether any
/// table was written with 16-bit precision — the signal that the frame must be extended
/// sequential (SOF1) rather than baseline.
///
/// Each table takes the narrowest precision that holds it: `Pq = 0` (one byte per value) when
/// every entry fits 255, otherwise `Pq = 1` (two big-endian bytes per value), as libjpeg's
/// `emit_dqt` decides per table.
pub fn emit_dqt(out: &mut Vec<u8>, tables: &[(u8, &[u16; 64])]) -> bool {
    // Lq = 2 length bytes + per table (1 precision/id byte + 64·(Pq + 1) value bytes).
    let len: usize = 2 + tables
        .iter()
        .map(|&(_, values)| 1 + 64 * (1 + usize::from(is_16bit(values))))
        .sum::<usize>();
    marker::write_segment_header(out, code::DQT, len);
    let mut extended = false;
    for &(id, values) in tables {
        let wide = is_16bit(values);
        extended |= wide;
        // Pq (high nibble) = table precision; Tq (low nibble) = destination id.
        out.push(pack_nibbles(u8::from(wide), id));
        for &k in &ZIGZAG {
            let [hi, lo] = values[k].to_be_bytes();
            if wide {
                out.push(hi);
            }
            out.push(lo);
        }
    }
    extended
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quality_50_is_the_annex_k_tables_verbatim() {
        // Quality 50 has scale 100, so (b·100 + 50)/100 == b for every entry: the mapping's fixed
        // point is exactly the Annex K base tables. Spot-checking a few would let a scale-mapping
        // mutant hide, so assert the whole 64 for both tables.
        assert_eq!(scale(&LUMINANCE, 50), LUMINANCE);
        assert_eq!(scale(&CHROMINANCE, 50), CHROMINANCE);
    }

    #[test]
    fn quality_100_collapses_to_all_ones() {
        // scale(100) = 200 - 200 = 0 → (0 + 50)/100 = 0 → clamped up to the minimum legal 1.
        assert_eq!(scale(&LUMINANCE, 100), [1u16; 64]);
        assert_eq!(scale(&CHROMINANCE, 100), [1u16; 64]);
    }

    #[test]
    fn quality_1_saturates_high() {
        // scale(1) = 5000 → luminance[0]=16 → (16·5000 + 50)/100 = 800 → clamped down to 255.
        let q1 = scale(&LUMINANCE, 1);
        assert_eq!(q1[0], 255);
        assert!(q1.iter().all(|&v| v == 255), "coarsest quality saturates");
    }

    #[test]
    fn hand_computed_scaled_entries() {
        // q=75: scale = 200 - 150 = 50. luminance[0]=16 → (16·50 + 50)/100 = (800+50)/100 = 8.
        // luminance[5]=40 → (40·50 + 50)/100 = 20. Pins the round-to-nearest numerator (the +50).
        let q75 = scale(&LUMINANCE, 75);
        assert_eq!(q75[0], 8);
        assert_eq!(q75[5], 20);
        // q=25: scale = 5000/25 = 200. luminance[0]=16 → (16·200 + 50)/100 = (3200+50)/100 = 32.
        // chrominance[0]=17 → (17·200 + 50)/100 = (3400+50)/100 = 34.
        assert_eq!(scale(&LUMINANCE, 25)[0], 32);
        assert_eq!(scale(&CHROMINANCE, 25)[0], 34);
    }

    #[test]
    fn quant_tables_reject_a_zero_entry_in_either_table() {
        // Zero must be caught in whichever table carries it — a validator that checks only one
        // array would let the other's zero through to a division and an illegal DQT.
        let mut bad = LUMINANCE;
        bad[63] = 0;
        assert!(QuantTables::new(bad, CHROMINANCE).is_err());
        assert!(QuantTables::new(LUMINANCE, bad).is_err());
    }

    #[test]
    fn quant_tables_accept_the_full_legal_range() {
        // 1 and 65535 are the Table B.4 boundaries; both all-boundary pairs must construct.
        let one = QuantTables::new([1; 64], [65535; 64]).expect("boundary tables are legal");
        assert_eq!(one.luma(), &[1; 64]);
        assert_eq!(one.chroma(), &[65535; 64]);
    }

    #[test]
    fn extended_scale_is_the_baseline_scale_without_the_8bit_ceiling() {
        // Over the whole quality range the two mappings agree once the extended table is capped
        // at 255: they differ only in the ceiling, never in the rounding.
        for q in 1..=100 {
            for base in [&LUMINANCE, &CHROMINANCE] {
                assert_eq!(scale_extended(base, q).map(|v| v.min(255)), scale(base, q));
            }
        }
        // q=10: scale = 500, so luminance[0]=16 → (8000 + 50)/100 = 80 and the K.1 maximum
        // 121 → 605, above the baseline ceiling; q=1 puts luminance[0] at 800, not 255.
        assert_eq!(scale_extended(&LUMINANCE, 10)[0], 80);
        assert_eq!(scale_extended(&LUMINANCE, 10).iter().max(), Some(&605));
        assert_eq!(scale_extended(&LUMINANCE, 1)[0], 800);
        // The extended ceiling is libjpeg's 32767, not the u16 maximum.
        assert_eq!(scale_extended(&[65535; 64], 1), [32767; 64]);
        assert_eq!(scale_extended(&LUMINANCE, 100), [1; 64]);
    }

    #[test]
    fn annex_k_tables_turn_16bit_exactly_below_quality_24() {
        // The libjpeg threshold for the Annex K tables: q=24 keeps every entry ≤ 255 (121·208
        // rounds to 252), q=23 does not (121·217 rounds to 263).
        let k = QuantTables::annex_k();
        assert_eq!(k.scaled_extended(24), k.scaled(24));
        assert!(!is_16bit(k.scaled_extended(24).luma()));
        assert!(is_16bit(k.scaled_extended(23).luma()));
        assert!(!is_16bit(k.scaled_extended(23).chroma()));
        // Quality clamps like `scaled`, and an X table is scaled by the same mapping.
        assert_eq!(k.scaled_extended(0), k.scaled_extended(1));
        assert_eq!(k.scaled_extended(200), k.scaled_extended(100));
        let t = k.with_xyb_x(LUMINANCE).unwrap().scaled_extended(10);
        assert_eq!(t.xyb_x(), Some(&scale_extended(&LUMINANCE, 10)));
        assert_eq!(t.chroma(), &scale_extended(&CHROMINANCE, 10));
    }

    #[test]
    fn a_table_is_16bit_only_above_255() {
        assert!(!is_16bit(&[255; 64]));
        let mut t = [1u16; 64];
        t[63] = 256;
        assert!(is_16bit(&t));
    }

    #[test]
    fn annex_k_is_the_annex_k_tables_and_scaling_matches_the_frozen_mapping() {
        // `annex_k()` must be the K.1/K.2 constants verbatim, and `scaled()` must reuse the frozen
        // mapping on both tables: quality 50 is the fixed point, 75 reproduces the hand-computed
        // anchors pinned above, and the luma/chroma halves must not be swapped (asserting whole
        // distinct arrays catches a swap).
        let k = QuantTables::annex_k();
        assert_eq!(k.luma(), &LUMINANCE);
        assert_eq!(k.chroma(), &CHROMINANCE);
        assert_eq!(k.scaled(50), k);
        let q75 = k.scaled(75);
        assert_eq!(q75.luma(), &scale(&LUMINANCE, 75));
        assert_eq!(q75.chroma(), &scale(&CHROMINANCE, 75));
        assert_eq!(q75.luma()[0], 8);
        assert_eq!(q75.chroma()[0], 9);
    }

    #[test]
    fn xyb_x_table_is_absent_by_default_validated_when_added_and_scaled_with_the_pair() {
        // `new` and `annex_k` carry no X table, so XYB keeps its two-table default.
        assert_eq!(QuantTables::annex_k().xyb_x(), None);
        assert_eq!(
            QuantTables::new(LUMINANCE, CHROMINANCE).unwrap().xyb_x(),
            None
        );
        // A zero entry is rejected like the pair's; a legal table is stored verbatim.
        let mut bad = [7u16; 64];
        bad[63] = 0;
        assert!(QuantTables::annex_k().with_xyb_x(bad).is_err());
        let x = scale(&LUMINANCE, 30);
        let t = QuantTables::annex_k().with_xyb_x(x).unwrap();
        assert_eq!(t.xyb_x(), Some(&x));
        assert_eq!((t.luma(), t.chroma()), (&LUMINANCE, &CHROMINANCE));
        // `scaled` applies the frozen mapping to the X table too (75 → scale 50 halves it).
        assert_eq!(t.scaled(75).xyb_x(), Some(&scale(&x, 75)));
        assert_eq!(QuantTables::annex_k().scaled(75).xyb_x(), None);
    }

    #[test]
    fn scaled_clamps_quality_like_with_quality() {
        // 0 clamps up to 1 and 200 clamps down to 100, mirroring `with_quality`'s documented
        // clamping so the two quality expressions can never disagree.
        let k = QuantTables::annex_k();
        assert_eq!(k.scaled(0), k.scaled(1));
        assert_eq!(k.scaled(200), k.scaled(100));
    }

    #[test]
    fn dqt_emits_length_id_and_zigzag_order() {
        // One luminance table at destination 0. Length = 2 + (1 + 64) = 67; the precision/id byte is
        // 0x00 (Pq=0, Tq=0); the first two payload values are the zig-zag lead of the table —
        // natural[0]=16 then natural[1]=11 (ZIGZAG[1]==1), proving zig-zag (not raster) emission.
        let mut out = Vec::new();
        assert!(!emit_dqt(&mut out, &[(0, &LUMINANCE)]), "8-bit table");
        assert_eq!(&out[..2], &[0xFF, 0xDB]); // DQT marker
        assert_eq!(&out[2..4], &[0x00, 67]); // Lq = 67
        assert_eq!(out[4], 0x00); // Pq=0, Tq=0
        assert_eq!(out[5], 16); // ZIGZAG[0] -> natural 0 = 16
        assert_eq!(out[6], 11); // ZIGZAG[1] -> natural 1 = 11
        assert_eq!(out.len(), 2 + 67);
        // Two tables (colour): length = 2 + 2·65 = 132, and the second table's id nibble is 1.
        let mut c = Vec::new();
        assert!(!emit_dqt(&mut c, &[(0, &LUMINANCE), (1, &CHROMINANCE)]));
        assert_eq!(&c[2..4], &[0x00, 132]);
        assert_eq!(c[4 + 65], 0x01); // second precision/id byte: Tq=1
    }

    #[test]
    fn dqt_writes_a_table_above_255_as_16bit_big_endian() {
        // An 8-bit luma table beside a 16-bit chroma table: Lq = 2 + 65 + (1 + 128) = 196, the
        // second table's Pq/Tq byte is 0x11, and each of its values is two big-endian bytes —
        // natural[0] = 0x0102 then natural[1] = 3 (ZIGZAG[1] == 1).
        let mut wide = [3u16; 64];
        wide[0] = 0x0102;
        let mut out = Vec::new();
        assert!(emit_dqt(&mut out, &[(0, &LUMINANCE), (1, &wide)]));
        assert_eq!(&out[2..4], &[0x00, 196]);
        assert_eq!(out.len(), 2 + 196);
        let second = 4 + 65;
        assert_eq!(out[second], 0x11); // Pq=1, Tq=1
        assert_eq!(&out[second + 1..second + 5], &[0x01, 0x02, 0x00, 0x03]);
        // Two 16-bit tables still report a 16-bit segment (the flag accumulates, never toggles).
        let mut two = Vec::new();
        assert!(emit_dqt(&mut two, &[(0, &wide), (1, &wide)]));
        assert_eq!(&two[2..4], &[0x01, 0x04]); // 2 + 2·129 = 260
        assert_eq!(two[4], 0x10); // Pq=1, Tq=0
    }
}
