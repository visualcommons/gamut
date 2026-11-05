//! The baseline entropy coder (Annex F §F.1.2): the two-mode [`BaselineCoder`] sink, the
//! differential-DC / run-length-AC block coder, the interleaved MCU walk ([`encode_scan`]), and the
//! fixed (Annex K.3–K.6) or optimized (Annex K.2) Huffman tables it codes with.

use super::block::{Plane, quantize_block_rd};
use crate::bitwriter::BitWriter;
use crate::huffman::{self, EncTable, TableSpec};
use crate::marker;
use crate::rd::RdCtx;
use crate::zigzag::ZIGZAG;

/// One frame component paired with the sampling, quantization table and entropy-table destination
/// used to code it. The entropy *tables* themselves live in [`ScanTables`], not here, because the
/// optimized path only knows them after the gather pass has walked these same components.
pub(super) struct Component<'a> {
    /// Horizontal sampling factor `Hi`.
    pub(super) h: u8,
    /// Vertical sampling factor `Vi`.
    pub(super) v: u8,
    pub(super) plane: &'a Plane,
    pub(super) quant: &'a [u8; 64],
    /// Entropy-table destination (the SOS `Tdj` = `Taj`): 0 = luma, 1 = chroma.
    pub(super) dest: usize,
    /// The rate–distortion context for this component's class, when RD optimization is enabled.
    pub(super) rd: Option<&'a RdCtx>,
}

/// The entropy tables a baseline scan codes with, indexed by destination (0 = luma, 1 = chroma).
///
/// A destination is `None` when the scan never references it — a grayscale frame has no chroma
/// destination, and an optimized table is omitted entirely when its histogram came back empty.
#[derive(Default)]
pub(super) struct ScanTables {
    /// DC-class tables (DHT `Tc = 0`).
    dc: [Option<EncTable>; 2],
    /// AC-class tables (DHT `Tc = 1`).
    ac: [Option<EncTable>; 2],
}

/// Per-destination symbol counts for one baseline scan, the input to the Annex K.2 optimal-table
/// construction. Only Huffman *symbols* are counted; the magnitude/sign bits that follow them are
/// raw, not coded, so they contribute nothing.
pub(super) struct Histograms {
    /// DC-class symbol counts, indexed by entropy-table destination.
    dc: [[u32; 256]; 2],
    /// AC-class symbol counts, indexed by entropy-table destination.
    ac: [[u32; 256]; 2],
}

impl Default for Histograms {
    fn default() -> Self {
        Self {
            dc: [[0; 256]; 2],
            ac: [[0; 256]; 2],
        }
    }
}

/// The two-mode baseline entropy sink, mirroring [`crate::progressive`]'s `ProgCoder`: a **gather**
/// pass accumulates per-destination symbol frequencies, an **emit** pass writes Huffman codes and
/// the raw magnitude bits. Both passes run the identical control flow ([`encode_scan`]), so every
/// symbol the emit pass writes was counted by the gather pass that built its table.
pub(super) enum BaselineCoder<'a, 'o> {
    /// Counting only: no output is produced and raw bits are ignored.
    Gather(&'a mut Histograms),
    /// Writing: Huffman codes from `ScanTables` plus raw bits, into the entropy bit writer.
    Emit(BitWriter<'o>, &'a ScanTables),
}

impl<'a, 'o> BaselineCoder<'a, 'o> {
    /// A gather pass accumulating into `freq`.
    pub(super) fn gather(freq: &'a mut Histograms) -> Self {
        Self::Gather(freq)
    }

    /// An emit pass appending entropy bytes to `out`, coding with `tables`.
    pub(super) fn emit(out: &'o mut Vec<u8>, tables: &'a ScanTables) -> Self {
        Self::Emit(BitWriter::new(out), tables)
    }

    /// Counts (gather) or emits (emit) one DC-class symbol at entropy destination `dest`.
    fn dc_symbol(&mut self, dest: usize, symbol: u8) {
        match self {
            Self::Gather(freq) => freq.dc[dest][usize::from(symbol)] += 1,
            Self::Emit(writer, tables) => emit_symbol(writer, tables.dc[dest].as_ref(), symbol),
        }
    }

    /// Counts (gather) or emits (emit) one AC-class symbol at entropy destination `dest`.
    fn ac_symbol(&mut self, dest: usize, symbol: u8) {
        match self {
            Self::Gather(freq) => freq.ac[dest][usize::from(symbol)] += 1,
            Self::Emit(writer, tables) => emit_symbol(writer, tables.ac[dest].as_ref(), symbol),
        }
    }

    /// Emits (emit pass only) `n` raw bits of `value`, MSB-first; a no-op while gathering.
    fn raw_bits(&mut self, value: u16, n: u8) {
        if let Self::Emit(writer, _) = self {
            writer.write_bits(value, n);
        }
    }

    /// Writes (emit pass only) restart marker `RSTm`, flushing the segment first.
    fn restart(&mut self, m: u8) {
        if let Self::Emit(writer, _) = self {
            writer.restart(m);
        }
    }

    /// Pads and flushes the final entropy byte (emit pass only).
    pub(super) fn finish(&mut self) {
        if let Self::Emit(writer, _) = self {
            writer.flush();
        }
    }
}

/// The magnitude category `SSSS` of `value` (Annex F §F.1.2): the number of bits needed for
/// `|value|`, and `0` for `value == 0`.
pub(crate) fn magnitude_category(value: i32) -> u8 {
    (32 - value.unsigned_abs().leading_zeros()) as u8
}

/// The `SSSS` additional bits appended after a DC/AC Huffman code (Annex F §F.1.2.1): the low
/// `category` bits of `value` for a positive value, or of `value - 1` (the "one lower precision"
/// negative encoding) for a negative value.
pub(crate) fn additional_bits(value: i32, category: u8) -> u16 {
    let v = if value < 0 { value - 1 } else { value };
    (v as u32 & ((1u32 << category) - 1)) as u16
}

/// Emits the Huffman code for `symbol` from `table`. Every symbol the entropy coder produces is
/// present in the table it codes with — the standard tables cover the whole baseline alphabet (DC
/// categories 0..=11; AC run/size, EOB `0x00`, ZRL `0xF0`), and an optimized table is built from
/// the very symbols the emit pass then writes — so a missing table or symbol is a logic error,
/// asserted in debug builds.
fn emit_symbol(writer: &mut BitWriter, table: Option<&EncTable>, symbol: u8) {
    match table.and_then(|t| t.lookup(symbol)) {
        Some((code, length)) => writer.write_bits(code, length),
        None => debug_assert!(false, "Huffman symbol {symbol:#x} absent from table"),
    }
}

/// Codes one 8×8 block (§A.3): level-shift → FDCT → quantize, then hands the natural-order
/// quantized coefficients to [`encode_quantized_block`] for entropy coding.
fn encode_block(
    comp: &Component,
    block_x: usize,
    block_y: usize,
    dc_pred: &mut i32,
    coder: &mut BaselineCoder,
) {
    let q = quantize_block_rd(comp.plane, comp.quant, block_x, block_y, comp.rd);
    encode_quantized_block(&q, dc_pred, comp.dest, coder);
}

/// Entropy-codes one block of quantized coefficients (natural order) per §F.1.2: the DC difference
/// against the running predictor (§F.1.2.1, updating it), then the run-length AC symbols in zig-zag
/// order (§F.1.2.2) — ZRL for zero runs of 16, EOB unless the last zig-zag coefficient is nonzero.
fn encode_quantized_block(
    q: &[i32; 64],
    dc_pred: &mut i32,
    dest: usize,
    coder: &mut BaselineCoder,
) {
    // DC: differential coding against the running predictor (§F.1.2.1).
    let diff = q[0] - *dc_pred;
    *dc_pred = q[0];
    let cat = magnitude_category(diff);
    coder.dc_symbol(dest, cat);
    coder.raw_bits(additional_bits(diff, cat), cat);

    // AC: run-length of zeros then (run, size) symbols in zig-zag order (§F.1.2.2).
    let mut run = 0u8;
    for &natural in &ZIGZAG[1..] {
        let coeff = q[natural];
        if coeff == 0 {
            run += 1;
            continue;
        }
        while run >= 16 {
            coder.ac_symbol(dest, 0xF0); // ZRL: 16 zeros
            run -= 16;
        }
        let cat = magnitude_category(coeff);
        coder.ac_symbol(dest, marker::pack_nibbles(run, cat));
        coder.raw_bits(additional_bits(coeff, cat), cat);
        run = 0;
    }
    if run > 0 {
        coder.ac_symbol(dest, 0x00); // EOB: block ends in zeros
    }
}

/// Codes the interleaved scan over all components (§A.2.3): walk MCUs row-major, and within each MCU
/// walk each component's `Vi×Hi` blocks. Restart markers are inserted every `restart_interval` MCUs
/// (predictors reset). A single-component (gray) scan degenerates to one 8×8 block per MCU — the
/// non-interleaved order of §A.2.2.
///
/// Shared verbatim by the gather and emit passes of the optimized-table path (and run once, in emit
/// mode, by the fixed-table path), so the frequency counts always match the emitted symbols. The
/// caller flushes the coder afterwards, padding the final entropy byte before the next marker.
pub(super) fn encode_scan(
    components: &[Component],
    width: u32,
    height: u32,
    restart_interval: u16,
    coder: &mut BaselineCoder,
) {
    let hmax = components.iter().map(|c| c.h).max().unwrap_or(1);
    let vmax = components.iter().map(|c| c.v).max().unwrap_or(1);
    let mcu_w = 8 * u32::from(hmax);
    let mcu_h = 8 * u32::from(vmax);
    let mcus_x = width.div_ceil(mcu_w);
    let mcus_y = height.div_ceil(mcu_h);

    let mut dc_pred = vec![0i32; components.len()];
    let mut mcu_index = 0u32;
    let mut restart_m = 0u8;

    for my in 0..mcus_y {
        for mx in 0..mcus_x {
            if restart_interval != 0
                && mcu_index != 0
                && mcu_index.is_multiple_of(u32::from(restart_interval))
            {
                coder.restart(restart_m);
                restart_m = restart_m.wrapping_add(1);
                dc_pred.iter_mut().for_each(|p| *p = 0);
            }
            for (ci, comp) in components.iter().enumerate() {
                for by in 0..u32::from(comp.v) {
                    for bx in 0..u32::from(comp.h) {
                        let block_x = (mx * u32::from(comp.h) + bx) as usize;
                        let block_y = (my * u32::from(comp.v) + by) as usize;
                        encode_block(comp, block_x, block_y, &mut dc_pred[ci], coder);
                    }
                }
            }
            mcu_index += 1;
        }
    }
}

/// Emits the DHT segment for a scan: luma DC/AC (destinations 0) always, plus chroma DC/AC
/// (destinations 1) when `color`.
pub(super) fn emit_huffman_tables(out: &mut Vec<u8>, color: bool) {
    let luma: [(u8, u8, &TableSpec); 2] =
        [(0, 0, &huffman::STD_LUMA_DC), (1, 0, &huffman::STD_LUMA_AC)];
    if color {
        huffman::emit_dht(
            out,
            &[
                luma[0],
                luma[1],
                (0, 1, &huffman::STD_CHROMA_DC),
                (1, 1, &huffman::STD_CHROMA_AC),
            ],
        );
    } else {
        huffman::emit_dht(out, &luma);
    }
}

/// The fixed Annex K.3–K.6 tables as encode tables, laid out by destination to match the segment
/// [`emit_huffman_tables`] writes: luma at destination 0, chroma at destination 1 when `color`.
pub(super) fn standard_tables(color: bool) -> ScanTables {
    ScanTables {
        dc: [
            Some(EncTable::from_spec(&huffman::STD_LUMA_DC)),
            color.then(|| EncTable::from_spec(&huffman::STD_CHROMA_DC)),
        ],
        ac: [
            Some(EncTable::from_spec(&huffman::STD_LUMA_AC)),
            color.then(|| EncTable::from_spec(&huffman::STD_CHROMA_AC)),
        ],
    }
}

/// Builds the Annex K.2 optimal table for each destination the scan actually used, emits them as
/// **one** DHT segment — the same segment count and position the fixed-table path occupies, in the
/// same `(luma DC, luma AC, chroma DC, chroma AC)` order — and returns them as encode tables.
///
/// A destination whose histogram is empty is omitted from both the segment and the returned tables:
/// nothing in the scan references it, so writing a zero-length table would only cost bytes.
pub(super) fn optimized_tables(out: &mut Vec<u8>, freq: &Histograms) -> ScanTables {
    // `(Tc, Th, BITS, HUFFVAL)`, in the emission order above.
    let mut built: Vec<(u8, u8, [u8; 16], Vec<u8>)> = Vec::new();
    for dest in 0..2usize {
        for (class, hist) in [(0u8, &freq.dc[dest]), (1u8, &freq.ac[dest])] {
            if hist.iter().all(|&n| n == 0) {
                continue;
            }
            let gamut_bitstream::HuffmanTable { bits, values } =
                gamut_bitstream::build_optimal_table(hist);
            built.push((class, dest as u8, bits, values));
        }
    }

    let segment: Vec<(u8, u8, &[u8; 16], &[u8])> = built
        .iter()
        .map(|(class, dest, bits, values)| (*class, *dest, bits, values.as_slice()))
        .collect();
    huffman::emit_dht_dynamic(out, &segment);

    let mut tables = ScanTables::default();
    for (class, dest, bits, values) in &built {
        let slot = if *class == 0 {
            &mut tables.dc[usize::from(*dest)]
        } else {
            &mut tables.ac[usize::from(*dest)]
        };
        *slot = Some(EncTable::from_bits_values(bits, values));
    }
    tables
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn optimized_tables_omit_destinations_the_scan_never_used() {
        // Only the luma DC destination carries symbols, so the DHT must hold exactly one table —
        // writing empty tables for the other three destinations would be pure overhead, and the
        // returned `ScanTables` must leave them `None` so nothing can code against them.
        let mut freq = Histograms::default();
        freq.dc[0][3] = 7;
        let mut dht = Vec::new();
        let tables = optimized_tables(&mut dht, &freq);

        assert!(tables.dc[0].is_some(), "luma DC was used");
        assert!(tables.ac[0].is_none(), "luma AC saw no symbols");
        assert!(
            tables.dc[1].is_none() && tables.ac[1].is_none(),
            "no chroma"
        );
        // DHT: FFC4, 2-byte length, then one table (1 byte Tc/Th + 16 counts + 1 value).
        assert_eq!(&dht[..2], &[0xFF, marker::code::DHT]);
        assert_eq!(
            u16::from_be_bytes([dht[2], dht[3]]) as usize,
            2 + 1 + 16 + 1,
            "one table, one symbol"
        );
        assert_eq!(dht[4], 0x00, "Tc = 0 (DC), Th = 0 (luma)");
    }

    #[test]
    fn optimized_tables_are_emitted_in_the_standard_destination_order() {
        // The optimized DHT must list (luma DC, luma AC, chroma DC, chroma AC) — the same order the
        // fixed-table segment uses — so a reader sees no structural difference between the two.
        let mut freq = Histograms::default();
        freq.dc[0][1] = 1;
        freq.ac[0][2] = 1;
        freq.dc[1][3] = 1;
        freq.ac[1][4] = 1;
        let mut dht = Vec::new();
        let _ = optimized_tables(&mut dht, &freq);

        // Each single-symbol table occupies 1 + 16 + 1 bytes after the 4-byte marker+length header.
        let tc_th: Vec<u8> = (0..4).map(|i| dht[4 + i * 18]).collect();
        assert_eq!(tc_th, vec![0x00, 0x10, 0x01, 0x11]);
    }

    #[test]
    fn magnitude_category_matches_f_1_2() {
        // Category = bit length of the magnitude; 0 → 0 (F.1.2). Boundaries pin the ">> until zero".
        assert_eq!(magnitude_category(0), 0);
        assert_eq!(magnitude_category(1), 1);
        assert_eq!(magnitude_category(-1), 1);
        assert_eq!(magnitude_category(2), 2);
        assert_eq!(magnitude_category(-2), 2);
        assert_eq!(magnitude_category(7), 3);
        assert_eq!(magnitude_category(-8), 4);
        assert_eq!(magnitude_category(1023), 10);
        assert_eq!(magnitude_category(2047), 11);
    }

    #[test]
    fn additional_bits_positive_and_negative() {
        // Positive: the value's own low bits. Negative: (value − 1)'s low bits (the F.1.2.1 "one
        // lower precision" complement). For category 3: +5 → 0b101 = 5; −5 → (−6) & 0b111 = 0b010.
        assert_eq!(additional_bits(5, 3), 0b101);
        assert_eq!(additional_bits(-5, 3), 0b010);
        // +1 → 1, −1 → 0 (category 1): the canonical smallest pair.
        assert_eq!(additional_bits(1, 1), 1);
        assert_eq!(additional_bits(-1, 1), 0);
        // Category 0 (DC diff of 0) yields no bits.
        assert_eq!(additional_bits(0, 0), 0);
        // Zero is *non-negative*: its own low bits (0), not the negative complement — pins the
        // strict `< 0` test (a `<= 0` mutant would take the −1 branch and yield 0b111).
        assert_eq!(additional_bits(0, 3), 0);
    }

    // --- Direct §F.1.2 entropy-coder tests: feed `encode_quantized_block` hand-built coefficient
    // arrays and assert the exact emitted bytes against a hand-listed (code, length) sequence from
    // the standard tables (K.3/K.5 anchors are pinned in `huffman`'s own tests). ---

    /// Entropy-codes one hand-built block with the standard luma tables, flushing at the end.
    fn encode_one(q: &[i32; 64], dc_pred: &mut i32) -> Vec<u8> {
        let tables = standard_tables(false);
        let mut out = Vec::new();
        let mut coder = BaselineCoder::emit(&mut out, &tables);
        encode_quantized_block(q, dc_pred, 0, &mut coder);
        coder.finish();
        out
    }

    /// Packs a hand-listed `(bits, length)` sequence with 1-padding — the expected-stream builder.
    fn expect_bits(seq: &[(u16, u8)]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut w = BitWriter::new(&mut out);
        for &(v, n) in seq {
            w.write_bits(v, n);
        }
        w.flush();
        out
    }

    /// A block whose only nonzero AC sits after exactly `zeros` zig-zag zeros, with value 1.
    fn block_with_zero_run(zeros: usize) -> [i32; 64] {
        let mut q = [0i32; 64];
        q[ZIGZAG[zeros + 1]] = 1;
        q
    }

    // Standard-table codes used below (see huffman.rs tests for their Annex C derivations):
    //   luma DC cat 0 = 00₂;  luma AC ZRL (F/0) = 11111111001₂ (11 bits);
    //   0/1 = 00₂;  1/1 = 1100₂;  4/1 = 111011₂ (6 bits);  0/2 = 01₂;  EOB = 1010₂.
    const DC0: (u16, u8) = (0b00, 2);
    const ZRL: (u16, u8) = (0b111_1111_1001, 11);
    const EOB: (u16, u8) = (0b1010, 4);

    #[test]
    fn zero_run_of_exactly_16_is_one_zrl() {
        // 16 zeros then +1: ZRL, then 0/1 (run 0 after the ZRL) with one magnitude bit, then EOB.
        let got = encode_one(&block_with_zero_run(16), &mut 0);
        assert_eq!(got, expect_bits(&[DC0, ZRL, (0b00, 2), (1, 1), EOB]));
        // Literal anchor, fully hand-packed: 00|11111111001|00|1|1010|1111 → 3F C9 AF.
        assert_eq!(got, vec![0x3F, 0xC9, 0xAF]);
    }

    #[test]
    fn zero_runs_of_17_and_20_leave_a_remainder_run() {
        // 17 zeros: ZRL eats 16, the remaining run of 1 joins the symbol → 1/1 = 1100₂.
        assert_eq!(
            encode_one(&block_with_zero_run(17), &mut 0),
            expect_bits(&[DC0, ZRL, (0b1100, 4), (1, 1), EOB])
        );
        // 20 zeros: ZRL then run 4 → 4/1 = 111011₂. Distinguishes `run -= 16` from `run /= 16`
        // (both give run 1 at 17 zeros — 20 zeros is the case where they diverge: 4 vs 1).
        assert_eq!(
            encode_one(&block_with_zero_run(20), &mut 0),
            expect_bits(&[DC0, ZRL, (0b111011, 6), (1, 1), EOB])
        );
    }

    #[test]
    fn zero_run_of_33_is_two_zrls() {
        // 33 zeros: two ZRLs (32 zeros) then run 1 → 1/1. A `run /= 16` mutant emits only one ZRL.
        assert_eq!(
            encode_one(&block_with_zero_run(33), &mut 0),
            expect_bits(&[DC0, ZRL, ZRL, (0b1100, 4), (1, 1), EOB])
        );
    }

    #[test]
    fn trailing_zeros_end_in_eob() {
        // Natural index 1 (zig-zag position 1) holds +3 (category 2 → 0/2 = 01₂, bits 11₂); the 62
        // trailing zeros collapse into a single EOB.
        let mut q = [0i32; 64];
        q[1] = 3;
        assert_eq!(
            encode_one(&q, &mut 0),
            expect_bits(&[DC0, (0b01, 2), (0b11, 2), EOB])
        );
    }

    #[test]
    fn nonzero_last_coefficient_suppresses_eob() {
        // Every AC coefficient +1: 63 consecutive 0/1 symbols and NO EOB (§F.1.2.2 — EOB is sent
        // only when the block ends in zeros). A `run > 0` → `run >= 0` mutant appends a spurious
        // EOB, changing the bytes.
        let mut q = [1i32; 64];
        q[0] = 0;
        let mut seq = vec![DC0];
        for _ in 0..63 {
            seq.push((0b00, 2)); // 0/1
            seq.push((1, 1)); // magnitude +1
        }
        assert_eq!(encode_one(&q, &mut 0), expect_bits(&seq));
    }

    #[test]
    fn dc_differences_across_blocks() {
        // Three blocks with absolute DCs 5, 2, 2 sharing one predictor: diffs +5 (cat 3, DC code
        // 100₂, bits 101₂), −3 (cat 2, DC code 011₂, bits (−3−1)&11₂ = 00₂), 0 (cat 0, no bits).
        let mut out = Vec::new();
        let tables = standard_tables(false);
        let mut coder = BaselineCoder::emit(&mut out, &tables);
        let mut pred = 0i32;
        for dc_value in [5, 2, 2] {
            let mut q = [0i32; 64];
            q[0] = dc_value;
            encode_quantized_block(&q, &mut pred, 0, &mut coder);
        }
        coder.finish();
        assert_eq!(pred, 2, "predictor tracks the last absolute DC");
        assert_eq!(
            out,
            expect_bits(&[
                (0b100, 3),
                (0b101, 3),
                EOB, // block 1: cat 3, +5
                (0b011, 3),
                (0b00, 2),
                EOB,       // block 2: cat 2, −3
                (0b00, 2), // block 3: cat 0, no magnitude bits
                EOB,
            ])
        );
    }
}
