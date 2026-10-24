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

use gamut_core::{Error, Result};

use crate::marker::{self, code};
use crate::zigzag::ZIGZAG;

/// T.81 Table K.1 — the example **luminance** quantization table, natural (row-major) order.
///
/// Suitable, per Annex K, for 8-bit luminance components; used for the Y component and for
/// grayscale images.
pub const LUMINANCE: [u8; 64] = [
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
pub const CHROMINANCE: [u8; 64] = [
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
/// Every entry is in the baseline `Pq = 0` range `1..=255` (§A.3.4) — `u8` caps the top and
/// [`Self::new`] rejects zero — so an encoder holding a `QuantTables` never divides by zero and
/// never emits a DQT segment its own decoder (or any conformant decoder) would refuse.
///
/// Handed to [`JpegEncoder::with_quant_tables`](crate::JpegEncoder::with_quant_tables), the tables
/// are used verbatim, replacing the frozen quality→scale mapping; [`Self::annex_k`] and
/// [`Self::scaled`] recover that mapping over arbitrary base tables.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuantTables {
    luma: [u8; 64],
    chroma: [u8; 64],
    xyb_x: Option<[u8; 64]>,
}

/// Rejects a table holding a zero entry as `InvalidInput` (§A.3.4: baseline `Q` is `1..=255`).
fn reject_zero(table: &[u8; 64]) -> Result<()> {
    if table.contains(&0) {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "JPEG: quantization table entries must be in 1..=255 (zero is illegal)",
        ));
    }
    Ok(())
}

impl QuantTables {
    /// Builds a table pair from natural-order entries, rejecting any zero entry as
    /// `InvalidInput` (§A.3.4 requires baseline `Q` values in `1..=255`).
    pub fn new(luma: [u8; 64], chroma: [u8; 64]) -> Result<Self> {
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
    pub fn with_xyb_x(mut self, x: [u8; 64]) -> Result<Self> {
        reject_zero(&x)?;
        self.xyb_x = Some(x);
        Ok(self)
    }

    /// These tables re-scaled by the frozen IJG quality mapping (`quality` clamped to `1..=100`,
    /// matching [`JpegEncoder::with_quality`](crate::JpegEncoder::with_quality)). Infallible: the
    /// mapping clamps every output entry into `1..=255`. `QuantTables::annex_k().scaled(q)`
    /// reproduces exactly the tables `with_quality(q)` uses. A [`Self::with_xyb_x`] table is
    /// scaled by the same mapping.
    #[must_use]
    pub fn scaled(&self, quality: u8) -> Self {
        let q = quality.clamp(1, 100);
        Self {
            luma: scale(&self.luma, q),
            chroma: scale(&self.chroma, q),
            xyb_x: self.xyb_x.map(|x| scale(&x, q)),
        }
    }

    /// The luminance (`Tq = 0`) table, natural order.
    #[must_use]
    pub fn luma(&self) -> &[u8; 64] {
        &self.luma
    }

    /// The chrominance (`Tq = 1`) table, natural order.
    #[must_use]
    pub fn chroma(&self) -> &[u8; 64] {
        &self.chroma
    }

    /// The separate XYB **X** (`Tq = 2`) table, natural order, or `None` when X shares the
    /// luminance table ([`Self::with_xyb_x`]).
    #[must_use]
    pub fn xyb_x(&self) -> Option<&[u8; 64]> {
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
pub fn scale(base: &[u8; 64], quality: u8) -> [u8; 64] {
    let s = quality_scale(quality);
    let mut out = [0u8; 64];
    for (dst, &b) in out.iter_mut().zip(base.iter()) {
        // (b·scale + 50) / 100 rounds to nearest; clamp into the 8-bit Pq=0 range.
        let v = (u32::from(b) * s + 50) / 100;
        *dst = v.clamp(1, 255) as u8;
    }
    out
}

/// Appends a DQT marker segment (§B.2.4.1) carrying `tables`, each an `(id, values)` pair whose
/// `values` are in **natural** order and are re-emitted in zig-zag order. Baseline precision
/// (`Pq = 0`, one byte per value) is always used.
pub fn emit_dqt(out: &mut Vec<u8>, tables: &[(u8, &[u8; 64])]) {
    // Lq = 2 length bytes + per table (1 precision/id byte + 64 value bytes).
    let len = 2 + tables.len() * (1 + 64);
    marker::write_segment_header(out, code::DQT, len);
    for &(id, values) in tables {
        // Pq (high nibble) = 0 for 8-bit; Tq (low nibble) = destination id.
        out.push(id & 0x0F);
        for &k in &ZIGZAG {
            out.push(values[k]);
        }
    }
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
        assert_eq!(scale(&LUMINANCE, 100), [1u8; 64]);
        assert_eq!(scale(&CHROMINANCE, 100), [1u8; 64]);
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
        // 1 and 255 are the §A.3.4 boundaries; both all-boundary pairs must construct.
        let one = QuantTables::new([1; 64], [255; 64]).expect("boundary tables are legal");
        assert_eq!(one.luma(), &[1; 64]);
        assert_eq!(one.chroma(), &[255; 64]);
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
        let mut bad = [7u8; 64];
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
        emit_dqt(&mut out, &[(0, &LUMINANCE)]);
        assert_eq!(&out[..2], &[0xFF, 0xDB]); // DQT marker
        assert_eq!(&out[2..4], &[0x00, 67]); // Lq = 67
        assert_eq!(out[4], 0x00); // Pq=0, Tq=0
        assert_eq!(out[5], 16); // ZIGZAG[0] -> natural 0 = 16
        assert_eq!(out[6], 11); // ZIGZAG[1] -> natural 1 = 11
        assert_eq!(out.len(), 2 + 67);
        // Two tables (colour): length = 2 + 2·65 = 132, and the second table's id nibble is 1.
        let mut c = Vec::new();
        emit_dqt(&mut c, &[(0, &LUMINANCE), (1, &CHROMINANCE)]);
        assert_eq!(&c[2..4], &[0x00, 132]);
        assert_eq!(c[4 + 65], 0x01); // second precision/id byte: Tq=1
    }
}
