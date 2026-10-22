//! The progressive DCT Huffman encoder (T.81 Annex G §G.1): the SOF2 frame, the frozen scan script,
//! optimized per-scan Huffman tables (Annex K.2), and the two-pass entropy coder for the DC/AC
//! first-pass and successive-approximation refinement models.
//!
//! # Why optimized tables are mandatory
//!
//! The standard Annex K AC tables cannot code a progressive AC scan: an EOB-run symbol `EOBn` for
//! `n ≥ 1` is `(n << 4) | 0` with `n` in `1..=14`, and those run/size bytes are simply **absent**
//! from Tables K.5/K.6. So — exactly as libjpeg forces `optimize_coding` for progressive — every
//! scan builds its own Huffman table(s) from the symbols it will emit: a first **gather** pass counts
//! symbol frequencies (including the ones the EOB-run and correction-bit machinery produce), those
//! frequencies drive the Annex K.2 optimal-table construction ([`build_optimal_table`]), a DHT is
//! written immediately before the scan, and a second **emit** pass writes the entropy data. The two
//! passes share one control-flow ([`run_scan`]) so every symbol the emit pass writes was counted.
//!
//! # Table layout (a documented free choice)
//!
//! Each scan uses a **single** optimized table at destination 0 for its class (DC for `Ss = 0`, AC
//! otherwise), shared by all of the scan's components; a DC-refinement scan (`Ss = 0`, `Ah ≠ 0`)
//! carries no table at all (it emits raw bits). Huffman coding is lossless, so this differs from
//! libjpeg's separate luma/chroma destinations only in compression density, never in the decoded
//! coefficients — which are byte-for-byte identical to the baseline encoding of the same input.
//!
//! # Frozen scan script
//!
//! The scan order is libjpeg's `jpeg_simple_progression` (transcribed from `jcparam.c`), SemVer-frozen
//! here: a 6-scan grayscale layout and the 10-scan YCbCr layout (see [`scan_script`]).

use gamut_bitstream::build_optimal_table;

use crate::bitwriter::BitWriter;
use crate::encoder::{Plane, additional_bits, magnitude_category, quantize_block_rd};
use crate::huffman::{EncTable, emit_dht_dynamic};
use crate::marker;
use crate::rd::RdCtx;
use crate::zigzag::ZIGZAG;

/// The largest EOB run an `EOBn` symbol can carry (§G.1.2.2): `EOBn` codes `n` in `0..=14`, so the
/// run is at most `2^15 − 1`. The accumulator is forced out at this value to keep it in range.
const MAX_EOBRUN: u32 = 0x7FFF;

/// One frame component handed to the progressive encoder: its identity, sampling factors, the
/// quantization-table destination it was written under (the SOF2 `Tqi`), and the sample plane and
/// (natural-order) quantization table used to materialize its coefficients.
pub(crate) struct ProgComponent<'a> {
    /// Component identifier `Ci` (the SOS `Csj` that selects it).
    pub id: u8,
    /// Horizontal sampling factor `Hi`.
    pub h: u8,
    /// Vertical sampling factor `Vi`.
    pub v: u8,
    /// Quantization-table destination `Tqi` (0 = luma, 1 = chroma), matching the emitted DQT.
    pub tq: u8,
    /// The component's sample plane at its own resolution.
    pub plane: &'a Plane,
    /// The component's natural-order quantization table.
    pub quant: &'a [u8; 64],
    /// The rate–distortion context for this component's class, when RD optimization is enabled.
    /// Shared with the baseline path through the same quantization seam, so the materialized
    /// coefficients — and hence the decoded image — stay identical between the two processes.
    pub rd: Option<&'a RdCtx>,
}

/// One frame component's fully-materialized quantized coefficients (§A.3.4), the source the scan
/// script reads. Coefficients are stored over the **padded** block grid — `pbw × pbh`, the geometry a
/// DC-interleaved scan walks (edge-replicated padding blocks and all) — while `bw × bh` is the
/// component's real block grid that the non-interleaved AC scans and the decoder's reconstruction
/// use.
struct CoeffComp {
    id: u8,
    h: u8,
    v: u8,
    tq: u8,
    /// Padded block-grid width (the row stride of `coeffs`).
    pbw: usize,
    /// Real block-grid width and height, `ceil(comp_dim / 8)`.
    bw: usize,
    bh: usize,
    /// `pbw · pbh` blocks of natural-order quantized coefficients, row-major.
    coeffs: Vec<[i32; 64]>,
}

impl CoeffComp {
    /// The natural-order coefficients of the block at block coordinates `(bx, by)` in the padded grid.
    fn block(&self, bx: usize, by: usize) -> &[i32; 64] {
        &self.coeffs[by * self.pbw + bx]
    }
}

/// The whole-frame MCU geometry an interleaved scan walks (§A.2.3): the padded MCU counts.
struct Geom {
    mcus_x: usize,
    mcus_y: usize,
}

/// One scan of the progressive script: the frame-component indices it codes (in interleave order)
/// and its spectral-selection band `[Ss..=Se]` with successive-approximation precision `(Ah, Al)`.
struct Scan {
    comps: Vec<usize>,
    ss: u8,
    se: u8,
    ah: u8,
    al: u8,
}

impl Scan {
    fn new(comps: &[usize], ss: u8, se: u8, ah: u8, al: u8) -> Self {
        Self {
            comps: comps.to_vec(),
            ss,
            se,
            ah,
            al,
        }
    }
}

/// The frozen `jpeg_simple_progression` scan script (libjpeg `jcparam.c`), transcribed faithfully.
///
/// - **Grayscale** (`ncomps == 1`) — the 6-scan all-purpose script: DC first pass (`Al = 1`), the
///   low (`1..=5`) and high (`6..=63`) luma AC bands (`Al = 2`), one AC refinement (`Ah = 2, Al = 1`),
///   the DC final pass (`Ah = 1, Al = 0`), and the final AC refinement (`Ah = 1, Al = 0`).
/// - **YCbCr colour** (`ncomps == 3`) — the 10-scan custom script: an interleaved DC first pass; a
///   quick low luma AC band; full Cr then Cb AC bands (`Al = 1`); the high luma AC band; a luma AC
///   refinement; the interleaved DC final pass; Cr, Cb, then luma AC final refinements. Component
///   order (Cr before Cb) is exactly libjpeg's.
fn scan_script(ncomps: usize) -> Vec<Scan> {
    if ncomps == 1 {
        // Component index 0 is the single (luma) component.
        vec![
            Scan::new(&[0], 0, 0, 0, 1),  // DC first pass
            Scan::new(&[0], 1, 5, 0, 2),  // low AC, first pass
            Scan::new(&[0], 6, 63, 0, 2), // high AC, first pass
            Scan::new(&[0], 1, 63, 2, 1), // AC refinement
            Scan::new(&[0], 0, 0, 1, 0),  // DC final pass
            Scan::new(&[0], 1, 63, 1, 0), // AC final refinement
        ]
    } else {
        // Component indices: 0 = Y, 1 = Cb, 2 = Cr (the SOF order).
        vec![
            Scan::new(&[0, 1, 2], 0, 0, 0, 1), // interleaved DC first pass
            Scan::new(&[0], 1, 5, 0, 2),       // quick low luma AC
            Scan::new(&[2], 1, 63, 0, 1),      // full Cr AC
            Scan::new(&[1], 1, 63, 0, 1),      // full Cb AC
            Scan::new(&[0], 6, 63, 0, 2),      // high luma AC
            Scan::new(&[0], 1, 63, 2, 1),      // luma AC refinement
            Scan::new(&[0, 1, 2], 0, 0, 1, 0), // interleaved DC final pass
            Scan::new(&[2], 1, 63, 1, 0),      // Cr AC final refinement
            Scan::new(&[1], 1, 63, 1, 0),      // Cb AC final refinement
            Scan::new(&[0], 1, 63, 1, 0),      // luma AC final refinement (usually the largest)
        ]
    }
}

/// Materializes every component's quantized coefficients (§A.3.1/§A.3.3/§A.3.4) over its padded block
/// grid and returns them with the whole-frame MCU geometry.
fn build_coeffs(input: &[ProgComponent], x: usize, y: usize) -> (Vec<CoeffComp>, Geom) {
    let hmax = usize::from(input.iter().map(|c| c.h).max().unwrap_or(1));
    let vmax = usize::from(input.iter().map(|c| c.v).max().unwrap_or(1));
    let mcus_x = x.div_ceil(8 * hmax);
    let mcus_y = y.div_ceil(8 * vmax);
    let comps = input
        .iter()
        .map(|c| {
            let (h, v) = (usize::from(c.h), usize::from(c.v));
            let pbw = mcus_x * h;
            let pbh = mcus_y * v;
            let comp_w = (x * h).div_ceil(hmax);
            let comp_h = (y * v).div_ceil(vmax);
            let bw = comp_w.div_ceil(8);
            let bh = comp_h.div_ceil(8);
            let mut coeffs = vec![[0i32; 64]; pbw * pbh];
            for by in 0..pbh {
                for bx in 0..pbw {
                    coeffs[by * pbw + bx] = quantize_block_rd(c.plane, c.quant, bx, by, c.rd);
                }
            }
            CoeffComp {
                id: c.id,
                h: c.h,
                v: c.v,
                tq: c.tq,
                pbw,
                bw,
                bh,
                coeffs,
            }
        })
        .collect();
    (comps, Geom { mcus_x, mcus_y })
}

/// Encodes a grayscale (1) or YCbCr (3) image as a progressive (SOF2) JPEG: writes the frame header,
/// an optional DRI, and the scan script's DHT+SOS+entropy for each scan. The caller has already
/// written the SOI/APP0/DQT prologue and appends EOI afterward.
pub(crate) fn encode(
    out: &mut Vec<u8>,
    width: u16,
    height: u16,
    components: &[ProgComponent],
    restart_interval: u16,
) {
    let (comps, geom) = build_coeffs(components, usize::from(width), usize::from(height));

    let sof: Vec<(u8, u8, u8, u8)> = comps.iter().map(|c| (c.id, c.h, c.v, c.tq)).collect();
    marker::write_sof2(out, width, height, &sof);
    if restart_interval != 0 {
        marker::write_dri(out, restart_interval);
    }

    for scan in &scan_script(comps.len()) {
        encode_one_scan(out, scan, &comps, &geom, restart_interval);
    }
}

/// Encodes one scan: the gather pass (unless it needs no table), the DHT and SOS headers, and the
/// entropy emit pass.
fn encode_one_scan(
    out: &mut Vec<u8>,
    scan: &Scan,
    comps: &[CoeffComp],
    geom: &Geom,
    restart_interval: u16,
) {
    let is_dc = scan.ss == 0;
    // A DC-refinement scan (Ss = 0, Ah ≠ 0) emits only raw bits, so it needs — and defines — no table.
    let needs_table = !(is_dc && scan.ah != 0);

    let table = if needs_table {
        let mut freq = [0u32; 256];
        let mut coder = ProgCoder::gather(&mut freq);
        run_scan(scan, comps, geom, restart_interval, &mut coder);
        coder.finish();
        let (bits, values) = build_optimal_table(&freq);
        let class = if is_dc { 0 } else { 1 };
        emit_dht_dynamic(out, &[(class, 0u8, &bits, &values)]);
        Some(EncTable::from_bits_values(&bits, &values))
    } else {
        None
    };

    // Every scan component references entropy-table destination 0 for its class.
    let sos: Vec<(u8, u8, u8)> = scan.comps.iter().map(|&ci| (comps[ci].id, 0, 0)).collect();
    marker::write_sos_bands(out, &sos, scan.ss, scan.se, scan.ah, scan.al);

    let mut coder = ProgCoder::emit(out, table.as_ref());
    run_scan(scan, comps, geom, restart_interval, &mut coder);
    coder.finish();
}

/// Walks the scan's MCUs, driving `coder` through the §G.1.2 encoding models for each block. Shared
/// verbatim by the gather and emit passes, so the frequency counts always match the emitted symbols.
/// Restart markers reset the DC predictors and flush the EOB-run/correction-bit state (§E.2.5).
fn run_scan(scan: &Scan, comps: &[CoeffComp], geom: &Geom, restart: u16, coder: &mut ProgCoder) {
    let interleaved = scan.comps.len() > 1;
    let (mcus_x, mcus_y) = if interleaved {
        (geom.mcus_x, geom.mcus_y)
    } else {
        let c = &comps[scan.comps[0]];
        (c.bw, c.bh)
    };
    let restart = usize::from(restart);
    let mut preds = vec![0i32; scan.comps.len()];
    let mut mcus_done = 0usize;
    let mut rst_m = 0u8;

    for my in 0..mcus_y {
        for mx in 0..mcus_x {
            if restart != 0 && mcus_done != 0 && mcus_done.is_multiple_of(restart) {
                coder.restart(rst_m);
                rst_m = (rst_m + 1) & 7;
                preds.iter_mut().for_each(|p| *p = 0);
            }
            for (si, &ci) in scan.comps.iter().enumerate() {
                let c = &comps[ci];
                if scan.ss == 0 {
                    let (nh, nv) = if interleaved {
                        (usize::from(c.h), usize::from(c.v))
                    } else {
                        (1, 1)
                    };
                    for by in 0..nv {
                        for bx in 0..nh {
                            let (bcol, brow) = if interleaved {
                                (mx * usize::from(c.h) + bx, my * usize::from(c.v) + by)
                            } else {
                                (mx, my)
                            };
                            let dc = c.block(bcol, brow)[0];
                            if scan.ah == 0 {
                                coder.dc_first(dc, scan.al, &mut preds[si]);
                            } else {
                                coder.dc_refine(dc, scan.al);
                            }
                        }
                    }
                } else {
                    let block = c.block(mx, my);
                    let (ss, se) = (usize::from(scan.ss), usize::from(scan.se));
                    if scan.ah == 0 {
                        coder.ac_first(block, ss, se, scan.al);
                    } else {
                        coder.ac_refine(block, ss, se, scan.al);
                    }
                }
            }
            mcus_done += 1;
        }
    }
}

/// The two-mode entropy sink: a **gather** pass accumulates per-symbol frequencies; an **emit** pass
/// writes Huffman codes (via the optimized table) and raw bits (magnitudes, signs, correction bits,
/// EOB-run extension bits). Both passes run the identical control flow so every emitted symbol was
/// counted. The EOB-run accumulator and the deferred correction-bit buffer (§G.1.2.3) live here.
struct ProgCoder<'a, 'o> {
    /// Present in the emit pass: the entropy output bit writer.
    writer: Option<BitWriter<'o>>,
    /// Present in the gather pass: the per-symbol frequency counts.
    freq: Option<&'a mut [u32; 256]>,
    /// Present in the emit pass when the scan uses a Huffman table (absent for a DC-refinement scan).
    table: Option<&'a EncTable>,
    /// The pending EOB run being accumulated (§G.1.2.2), `0` when none.
    eobrun: u32,
    /// Buffered successive-approximation correction bits for the pending EOB run (§G.1.2.3), emitted
    /// after the next `EOBn` symbol.
    pending: Vec<u8>,
}

impl<'a, 'o> ProgCoder<'a, 'o> {
    fn gather(freq: &'a mut [u32; 256]) -> Self {
        Self {
            writer: None,
            freq: Some(freq),
            table: None,
            eobrun: 0,
            pending: Vec::new(),
        }
    }

    fn emit(out: &'o mut Vec<u8>, table: Option<&'a EncTable>) -> Self {
        Self {
            writer: Some(BitWriter::new(out)),
            freq: None,
            table,
            eobrun: 0,
            pending: Vec::new(),
        }
    }

    /// Counts (gather) or emits (emit) one Huffman symbol.
    fn symbol(&mut self, s: u8) {
        if let Some(freq) = self.freq.as_deref_mut() {
            freq[usize::from(s)] += 1;
        } else if let (Some(table), Some(writer)) = (self.table, self.writer.as_mut()) {
            match table.lookup(s) {
                Some((code, len)) => writer.write_bits(code, len),
                None => debug_assert!(
                    false,
                    "progressive symbol {s:#x} absent from optimized table"
                ),
            }
        }
    }

    /// Emits (emit pass only) `n` raw bits of `value`, MSB-first; a no-op while gathering.
    fn raw_bits(&mut self, value: u16, n: u8) {
        if let Some(writer) = self.writer.as_mut() {
            writer.write_bits(value, n);
        }
    }

    /// Emits (emit pass only) a single raw bit.
    fn raw_bit(&mut self, bit: u8) {
        self.raw_bits(u16::from(bit), 1);
    }

    /// Flushes the pending EOB run (§G.1.2.2): the `EOBn` symbol, its `n` extension bits, then any
    /// buffered correction bits (§G.1.2.3). A no-op when no run is pending.
    fn flush_eobrun(&mut self) {
        if self.eobrun == 0 {
            return;
        }
        // n = ⌊log2(EOBRUN)⌋; the symbol is n in the run nibble, size 0.
        let n = (31 - self.eobrun.leading_zeros()) as u8;
        self.symbol(n << 4);
        // The `n` low bits of EOBRUN (a no-op write when `n == 0`, i.e. an EOB0).
        self.raw_bits((self.eobrun & ((1u32 << n) - 1)) as u16, n);
        self.eobrun = 0;
        let bits = std::mem::take(&mut self.pending);
        for b in bits {
            self.raw_bit(b);
        }
    }

    /// Counts one all-zero band block toward the EOB run, forcing the run out if it reaches its
    /// 15-bit maximum (§G.1.2.2).
    fn bump_eobrun(&mut self) {
        self.eobrun += 1;
        if self.eobrun == MAX_EOBRUN {
            self.flush_eobrun();
        }
    }

    /// DC first pass (§G.1.2.1): the point-transformed DC value's difference against the predictor,
    /// coded like a baseline DC coefficient.
    fn dc_first(&mut self, dc: i32, al: u8, pred: &mut i32) {
        let t = dc >> al; // point transform: arithmetic (floor) right shift
        let diff = t - *pred;
        *pred = t;
        let size = magnitude_category(diff);
        self.symbol(size);
        self.raw_bits(additional_bits(diff, size), size);
    }

    /// DC refinement (§G.1.2.3): the `Al`-th bit of the DC coefficient, as a single raw bit.
    fn dc_refine(&mut self, dc: i32, al: u8) {
        self.raw_bit(((dc >> al) & 1) as u8);
    }

    /// AC first pass (§G.1.2.2, Figure G.3): run/size-coded coefficients within `[ss..=se]`, scaled by
    /// the point transform (division toward zero by `2^al`), with all-zero bands accumulated as an EOB
    /// run.
    fn ac_first(&mut self, block: &[i32; 64], ss: usize, se: usize, al: u8) {
        let div = 1i32 << al;
        let mut run: u32 = 0;
        for k in ss..=se {
            let t = block[ZIGZAG[k]] / div; // point transform (round toward zero)
            if t == 0 {
                run += 1;
                continue;
            }
            // The first coded coefficient of a content block flushes any pending EOB run (§G.1.2.2):
            // an all-zero-band run must be closed before this block's symbols so it never absorbs the
            // block. Folded into the emit loop (rather than a separate content pre-scan) so the point
            // transform is computed exactly once per coefficient.
            self.flush_eobrun();
            while run > 15 {
                self.symbol(0xF0); // ZRL: 16 zeros
                run -= 16;
            }
            let size = magnitude_category(t);
            self.symbol(marker::pack_nibbles(run as u8, size));
            self.raw_bits(additional_bits(t, size), size);
            run = 0;
        }
        if run > 0 {
            // The band ended in zeros (or is entirely zero): this block joins the EOB run.
            self.bump_eobrun();
        }
    }

    /// AC refinement (§G.1.2.3, Figure G.7): a correction bit for each already-nonzero coefficient and
    /// a run/size + sign for each newly-nonzero coefficient in `[ss..=se]`. Correction bits are
    /// **buffered** and emitted only after the run/size, ZRL, or `EOBn` symbol they attach to.
    fn ac_refine(&mut self, block: &[i32; 64], ss: usize, se: usize, al: u8) {
        // The last position that becomes newly-nonzero this scan (|coef| >> al == 1). Runs of zeros
        // past it fold into the EOB rather than a ZRL.
        let eob_pos = (ss..=se)
            .rev()
            .find(|&k| (block[ZIGZAG[k]].abs() >> al) == 1);

        let mut run: u32 = 0; // run of never-yet-coded (zero-history) coefficients
        let mut br: Vec<u8> = Vec::new(); // this block's buffered correction bits
        for k in ss..=se {
            let coeff = block[ZIGZAG[k]];
            let abs = coeff.abs() >> al;
            if abs == 0 {
                run += 1;
                continue;
            }
            let before_eob = eob_pos.is_some_and(|e| k <= e);
            if abs > 1 {
                // Already nonzero at a coarser scan: emit any ZRLs the run has earned (unless they
                // would fold past the EOB), then buffer this coefficient's correction bit.
                while run > 15 && before_eob {
                    self.flush_eobrun();
                    self.symbol(0xF0);
                    run -= 16;
                    self.flush_br(&mut br);
                }
                br.push((abs & 1) as u8);
                continue;
            }
            // Newly nonzero (abs == 1): flush any EOB run, emit the run/size and sign, then the
            // buffered correction bits that must ride behind this code.
            while run > 15 {
                self.flush_eobrun();
                self.symbol(0xF0);
                run -= 16;
                self.flush_br(&mut br);
            }
            self.flush_eobrun();
            self.symbol(marker::pack_nibbles(run as u8, 1)); // run/size, size = 1 (newly nonzero)
            // The sign as `is_positive` rather than `coeff > 0`: `abs == 1` here makes `coeff`
            // nonzero, so `> 0` and its `>= 0` mutant always agreed. The rewrite also took the
            // comparison's two killable siblings (`<`, `==`) out of the survey (#924).
            self.raw_bit(u8::from(coeff.is_positive()));
            self.flush_br(&mut br);
            run = 0;
        }
        if run > 0 || !br.is_empty() {
            // Trailing zero-history coefficients and/or buffered corrections: this block joins the EOB
            // run, carrying its correction bits with it.
            self.eobrun += 1;
            self.pending.append(&mut br);
            if self.eobrun == MAX_EOBRUN {
                self.flush_eobrun();
            }
        }
    }

    /// Emits this block's buffered correction bits immediately after their attaching symbol, clearing
    /// the buffer.
    fn flush_br(&mut self, br: &mut Vec<u8>) {
        let bits = std::mem::take(br);
        for b in bits {
            self.raw_bit(b);
        }
    }

    /// Flushes the EOB run and correction bits, then pads and writes a restart marker `RSTm`
    /// (§E.2.5). The caller resets the DC predictors.
    fn restart(&mut self, m: u8) {
        self.flush_eobrun();
        if let Some(writer) = self.writer.as_mut() {
            writer.restart(m);
        }
    }

    /// Ends the scan: flushes the pending EOB run and pads the final entropy byte.
    fn finish(&mut self) {
        self.flush_eobrun();
        if let Some(writer) = self.writer.as_mut() {
            writer.flush();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An MSB-first, de-stuffing bit reader over a single scan's entropy bytes.
    struct Bits<'a> {
        data: &'a [u8],
        pos: usize,
        bit: u8,
    }

    impl Bits<'_> {
        fn bit(&mut self) -> u32 {
            let b = self.data[self.pos];
            let out = u32::from((b >> (7 - self.bit)) & 1);
            self.bit += 1;
            if self.bit == 8 {
                self.bit = 0;
                self.pos += 1;
                if b == 0xFF {
                    self.pos += 1; // skip the stuffed 0x00
                }
            }
            out
        }

        fn bits(&mut self, n: u8) -> u32 {
            (0..n).fold(0u32, |acc, _| (acc << 1) | self.bit())
        }

        /// Decodes one symbol against a canonical `(code, len, symbol)` list.
        fn symbol(&mut self, table: &[(u16, u8, u8)]) -> u8 {
            let mut code = 0u16;
            for len in 1..=16u8 {
                code = (code << 1) | self.bit() as u16;
                if let Some(&(_, _, s)) = table.iter().find(|&&(c, l, _)| l == len && c == code) {
                    return s;
                }
            }
            panic!("no symbol");
        }
    }

    /// Sign-extends a `t`-bit magnitude (the decoder's `EXTEND`), the inverse of `additional_bits`.
    fn extend(v: u32, t: u8) -> i32 {
        let v = v as i32;
        if t == 0 || v >= (1 << (t - 1)) {
            v
        } else {
            v - (1 << t) + 1
        }
    }

    /// The inverted `(code, len, symbol)` list of a `(BITS, HUFFVAL)` pair.
    fn invert(bits: &[u8; 16], values: &[u8]) -> Vec<(u16, u8, u8)> {
        let t = EncTable::from_bits_values(bits, values);
        (0..=255u16)
            .filter_map(|s| t.lookup(s as u8).map(|(c, l)| (c, l, s as u8)))
            .collect()
    }

    /// Gathers, builds the optimal table, and emits `f` over a scratch coder — the exact two-pass an
    /// AC scan runs — returning the entropy bytes and the built `(BITS, HUFFVAL)`.
    fn code<F: Fn(&mut ProgCoder)>(f: F) -> (Vec<u8>, [u8; 16], Vec<u8>) {
        let mut freq = [0u32; 256];
        {
            let mut g = ProgCoder::gather(&mut freq);
            f(&mut g);
            g.finish();
        }
        let (bits, values) = build_optimal_table(&freq);
        let mut out = Vec::new();
        {
            let table = EncTable::from_bits_values(&bits, &values);
            let mut e = ProgCoder::emit(&mut out, Some(&table));
            f(&mut e);
            e.finish();
        }
        (out, bits, values)
    }

    /// Decodes an AC first-pass single-block scan (the inverse of [`ProgCoder::ac_first`] for one
    /// block), returning the natural-order coefficients (scaled by `2^al`).
    fn decode_ac_first(
        block_entropy: &(Vec<u8>, [u8; 16], Vec<u8>),
        ss: usize,
        se: usize,
        al: u8,
    ) -> [i32; 64] {
        let (entropy, bits, values) = block_entropy;
        let table = invert(bits, values);
        let mut r = Bits {
            data: entropy,
            pos: 0,
            bit: 0,
        };
        let mut out = [0i32; 64];
        let mut k = ss;
        while k <= se {
            let rs = r.symbol(&table);
            let (run, size) = (usize::from(rs >> 4), rs & 0x0F);
            if size == 0 {
                if run == 15 {
                    k += 16; // ZRL
                    continue;
                }
                break; // EOBn — remaining band is zero for this block
            }
            k += run;
            out[ZIGZAG[k]] = extend(r.bits(size), size) << al;
            k += 1;
        }
        out
    }

    #[test]
    fn ac_first_zrl_boundary_at_exactly_15_and_16_zeros() {
        // A block whose only nonzero band coefficient sits after a run of zeros pins the ZRL boundary
        // (`run > 15`): 15 leading zeros must code as a single run/size symbol (run nibble 15), while
        // 16 zeros must code as ZRL + run/size(run 0). Decoding recovers the coefficient at its exact
        // zig-zag position, so a `>`→`>=` mutant (which would emit a ZRL at 15 and under-run) diverges.
        for (zeros, al) in [(15usize, 0u8), (16, 0), (15, 1), (20, 2)] {
            let mut block = [0i32; 64];
            let pos = 1 + zeros; // zig-zag position of the nonzero (ss = 1, so `zeros` zeros precede)
            block[ZIGZAG[pos]] = 3 << al; // survives the point transform to value 3
            let coded = code(|c| c.ac_first(&block, 1, 63, al));
            let decoded = decode_ac_first(&coded, 1, 63, al);
            let mut expected = [0i32; 64];
            expected[ZIGZAG[pos]] = 3 << al;
            assert_eq!(decoded, expected, "zeros={zeros} al={al}");
        }
    }

    #[test]
    fn ac_first_point_transform_rounds_toward_zero() {
        // The AC point transform is division toward zero by 2^al. A coefficient of −7 at al=1 codes as
        // −3 (‖−7‖ >> 1 = 3, negative), reconstructing to −3<<1 = −6; +7 → +3 → +6. Pins the `/ div`
        // transform and the signed magnitude bits.
        let mut block = [0i32; 64];
        block[ZIGZAG[1]] = -7;
        block[ZIGZAG[2]] = 7;
        let coded = code(|c| c.ac_first(&block, 1, 5, 1));
        let decoded = decode_ac_first(&coded, 1, 5, 1);
        assert_eq!(decoded[ZIGZAG[1]], -6);
        assert_eq!(decoded[ZIGZAG[2]], 6);
    }

    #[test]
    fn dc_refine_emits_bit_al_of_the_coefficient() {
        // DC refinement emits bit `al` of the DC coefficient. With al = 1, coefficient 2 (binary 10)
        // has bit 1 set → a `1` bit, and coefficient 1 (binary 01) has bit 1 clear → a `0` bit. The
        // stream is `1 0` padded with 1-bits → 0b10111111 = 0xBF. A `>>`→`<<` mutant left-shifts,
        // forcing bit 0 to zero, so both coefficients would emit `0` (0x3F) — a distinct byte.
        let mut out = Vec::new();
        {
            let mut e = ProgCoder::emit(&mut out, None);
            e.dc_refine(2, 1);
            e.dc_refine(1, 1);
            e.finish();
        }
        assert_eq!(out, vec![0xBF]);
    }

    #[test]
    fn dc_first_codes_the_point_transformed_difference() {
        // DC first pass point-transforms by an arithmetic (floor) shift and differentially codes the
        // result. With al=1 and predictor 0, DC 5 → 5>>1 = 2 (diff +2); the next DC 9 → 9>>1 = 4
        // (diff +2). Decoding the two DC symbols recovers the running values 2 then 4.
        let coded = code(|c| {
            let mut pred = 0;
            c.dc_first(5, 1, &mut pred);
            c.dc_first(9, 1, &mut pred);
        });
        let (entropy, bits, values) = &coded;
        let table = invert(bits, values);
        let mut r = Bits {
            data: entropy,
            pos: 0,
            bit: 0,
        };
        let mut pred = 0i32;
        for expected in [2i32, 4] {
            let size = r.symbol(&table);
            pred += extend(r.bits(size), size);
            assert_eq!(pred, expected);
        }
    }

    #[test]
    fn ac_refine_sign_and_eob_fold_round_trip() {
        // AC refinement over a band that mixes an already-nonzero coefficient (magnitude > 1 after the
        // transform → a correction bit), a newly-nonzero negative coefficient (magnitude 1 → run/size
        // + sign 0), and trailing zeros that fold into the EOB run. Decoding must recover the newly
        // nonzero coefficient with its correct sign and the correction applied to the existing one —
        // pinning the eob-position search (`== 1`) and the sign test (`is_positive`).
        let al = 1u8;
        let p1 = 1i32 << al;
        // Prior-scan block state: position 1 already nonzero at magnitude 2<<al (a "history" coef);
        // position 3 becomes newly nonzero this scan at −(1<<al); the rest are zero.
        let mut block = [0i32; 64];
        block[ZIGZAG[1]] = 2 << al; // already nonzero (abs>>al = 2)
        block[ZIGZAG[3]] = -(1 << al); // newly nonzero (abs>>al = 1), negative
        let (entropy, bits, values) = code(|c| c.ac_refine(&block, 1, 6, al));
        let table = invert(&bits, &values);
        let mut r = Bits {
            data: &entropy,
            pos: 0,
            bit: 0,
        };
        // Decode mirroring scan::decode_ac_refine for a single block over band [1..=6].
        let mut out = [0i32; 64];
        out[ZIGZAG[1]] = 2 << al; // decoder already holds the coarse value
        let (mut k, mut eobrun) = (1usize, 0u32);
        while k <= 6 {
            let rs = r.symbol(&table);
            let (mut run, size) = (i32::from(rs >> 4), rs & 0x0F);
            let mut newv = 0i32;
            if size != 0 {
                newv = if r.bit() != 0 { p1 } else { -p1 };
            } else if rs >> 4 != 15 {
                eobrun = 1 << (rs >> 4);
                if rs >> 4 != 0 {
                    eobrun += r.bits(rs >> 4);
                }
                break;
            }
            loop {
                let nat = ZIGZAG[k];
                if out[nat] != 0 {
                    if r.bit() != 0 {
                        out[nat] += if out[nat] > 0 { p1 } else { -p1 };
                    }
                } else {
                    run -= 1;
                    if run < 0 {
                        break;
                    }
                }
                k += 1;
                if k > 6 {
                    break;
                }
            }
            if newv != 0 {
                out[ZIGZAG[k]] = newv;
            }
            k += 1;
        }
        let _ = eobrun;
        assert_eq!(
            out[ZIGZAG[3]],
            -(1 << al),
            "newly-nonzero negative coefficient"
        );
        assert_eq!(
            out[ZIGZAG[1]],
            2 << al,
            "history coefficient (correction bit was 0)"
        );
    }
}
