//! Progressive-process scans (Annex G, §G.2): per-component coefficient accumulators, the
//! scan-order validation of spectral selection and successive approximation (§G.1.1.1), and the
//! DC/AC first and refinement passes, reconstructed once through the shared tail at EOI. See the
//! parent module's [progressive coefficient model](super#progressive-coefficient-model-g11).

use gamut_core::{Error, Result};

use super::reader::{BitReader, decode_coded, decode_symbol};
use super::{Plane, write_block};
use crate::huffman::DecTable;
use crate::syntax::{Frame, ScanHeader, Tables};
use crate::zigzag::ZIGZAG;

/// One frame component's progressive DCT-coefficient accumulator (§G.1.1). It owns the component's
/// entire block grid of natural-order coefficients, filled incrementally by the progressive scans
/// and dequantized + inverse-transformed only once, at frame completion, by [`Self::into_plane`].
pub struct ProgComp {
    /// Horizontal sampling factor `Hi` (carried through to the reconstructed [`Plane`]).
    h: u8,
    /// Vertical sampling factor `Vi`.
    v: u8,
    /// Width of the component's block grid in blocks, `ceil(comp_w / 8)` (§A.2.2).
    bw: usize,
    /// Height of the component's block grid in blocks, `ceil(comp_h / 8)`.
    bh: usize,
    /// `bw · bh` contiguous 64-coefficient blocks in natural (raster) order, zero-initialised.
    coeffs: Vec<i32>,
    /// The dequantization table, bound at the component's **first** scan (§B.2.4.1) and thereafter
    /// immune to DQT redefinition, mirroring the reference decoder's latch.
    quant: [u16; 64],
    /// Whether [`Self::quant`] has been bound yet.
    quant_bound: bool,
    /// Per zig-zag position, the successive-approximation bit position `Al` of the most recent scan
    /// that coded it, or `None` if never coded — the band bookkeeping (§G.1.1.1) that rejects
    /// out-of-order and overlapping scans.
    coded_al: [Option<u8>; 64],
}

impl ProgComp {
    /// Allocates an all-zero coefficient buffer for a component with the given sampling factors and
    /// block-grid dimensions.
    #[must_use]
    pub fn new(h: u8, v: u8, bw: usize, bh: usize) -> Self {
        Self {
            h,
            v,
            bw,
            bh,
            coeffs: vec![0i32; bw * bh * 64],
            quant: [0u16; 64],
            quant_bound: false,
            coded_al: [None; 64],
        }
    }

    /// Whether the component's DC first pass has been delivered (§G.1.1.1.1 requires it before any
    /// AC scan, and the partial-render policy requires it before reconstruction).
    #[must_use]
    pub fn has_dc(&self) -> bool {
        self.coded_al[0].is_some()
    }

    /// The natural-order coefficients of block `(bx, by)`.
    fn block_mut(&mut self, bx: usize, by: usize) -> &mut [i32] {
        let off = (by * self.bw + bx) * 64;
        &mut self.coeffs[off..off + 64]
    }

    /// Dequantizes (§A.3.4), inverse-transforms (§A.3.3) and level-shifts (§A.3.1) every block into
    /// a reconstructed sample [`Plane`] — the once-only progressive reconstruction at EOI.
    #[must_use]
    pub fn into_plane(self) -> Plane {
        let stride = self.bw * 8;
        let mut plane = Plane {
            data: vec![0u8; stride * self.bh * 8],
            stride,
            h: self.h,
            v: self.v,
        };
        for by in 0..self.bh {
            for bx in 0..self.bw {
                let off = (by * self.bw + bx) * 64;
                let mut zz = [0i32; 64];
                for (i, cell) in zz.iter_mut().enumerate() {
                    *cell = self.coeffs[off + i].wrapping_mul(i32::from(self.quant[i]));
                }
                write_block(&mut zz, &mut plane, bx, by);
            }
        }
        plane
    }
}

/// Verifies the scan's band ordering against each component's history and updates it (§G.1.1.1).
/// A first pass (`Ah = 0`) requires every band position to be previously uncoded; a refinement
/// (`Ah ≠ 0`) requires every position to have been coded at exactly `Al = Ah` in a prior scan; an AC
/// scan additionally requires the component's DC first pass to have already happened (§G.1.1.1.1).
fn validate_progression(scan: &ScanHeader, comps: &mut [ProgComp]) -> Result<()> {
    let (ss, se) = (usize::from(scan.ss), usize::from(scan.se));
    for sc in &scan.components {
        let c = &mut comps[sc.frame_index];
        let (lo, hi) = if ss == 0 { (0, 0) } else { (ss, se) };
        if ss != 0 && !c.has_dc() {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "JPEG: progressive AC scan before the component's DC scan",
            ));
        }
        for &coded in &c.coded_al[lo..=hi] {
            match (scan.ah, coded) {
                // First pass of a band: the position must not have been coded yet (no overlap).
                (0, None) => {}
                (0, Some(_)) => {
                    return Err(Error::invalid_input(
                        env!("CARGO_PKG_NAME"),
                        "JPEG: progressive band coded twice at the first pass (overlap)",
                    ));
                }
                // Refinement: the position's history must be exactly Ah (its previous Al).
                (ah, Some(prev)) if prev == ah => {}
                (_, _) => {
                    return Err(Error::invalid_input(
                        env!("CARGO_PKG_NAME"),
                        "JPEG: progressive refinement without a matching first pass",
                    ));
                }
            }
        }
        for slot in c.coded_al[lo..=hi].iter_mut() {
            *slot = Some(scan.al);
        }
    }
    Ok(())
}

/// A resolved scan component for the progressive entropy loop: its frame index, sampling factors,
/// and the one entropy table the scan's band codes with — its DC table for a DC band (`Ss = 0`),
/// its AC table otherwise (§G.1.2). A band never needs both, so the table is resolved once, when
/// the scan is set up, and the loop has no absent table to handle.
struct ProgSc<'a> {
    frame_index: usize,
    h: usize,
    v: usize,
    table: &'a DecTable,
}

/// Decodes one progressive scan's entropy data starting at byte `start`, accumulating coefficients
/// into `comps` (§G.2). Returns the byte offset of the marker that terminates the entropy data.
///
/// The scan carries one band `[Ss..=Se]` at successive-approximation precision `(Ah, Al)` (validated
/// by [`crate::syntax::parse_sos`] for progressive). DC scans (`Ss = 0`) may interleave up to four
/// components; AC scans (`Ss ≥ 1`) are single-component (§B.2.3). First passes (`Ah = 0`) Huffman-code
/// the band scaled by the point transform `2^Al` (§A.4); refinements (`Ah ≠ 0`) append correction /
/// newly-nonzero bits (§G.1.2.3). At each restart boundary the DC predictors and the EOB run counter
/// are reset (§E.2.5, §G.1.2.2).
///
/// # Errors
///
/// An [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error for an undefined
/// referenced table, an out-of-order or overlapping scan,
/// a corrupt Huffman code, an out-of-band coefficient index, a bad restart sequence, or a truncated
/// stream.
pub fn decode_progressive_scan(
    data: &[u8],
    start: usize,
    frame: &Frame,
    scan: &ScanHeader,
    tables: &Tables,
    comps: &mut [ProgComp],
) -> Result<usize> {
    let (ss, se) = (usize::from(scan.ss), usize::from(scan.se));
    let (ah, al) = (scan.ah, scan.al);
    let interleaved = scan.interleaved();

    // Bind each scan component's quantization table at its first reference (§B.2.4.1), then check
    // and update the band-progression bookkeeping.
    for sc in &scan.components {
        let c = &mut comps[sc.frame_index];
        if !c.quant_bound {
            let fc = &frame.components[sc.frame_index];
            c.quant = tables.quant[usize::from(fc.tq)].ok_or_else(|| {
                Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "JPEG: scan uses undefined quant table",
                )
            })?;
            c.quant_bound = true;
        }
    }
    validate_progression(scan, comps)?;

    // Resolve the entropy table each scan component needs for this band.
    let mut scs: Vec<ProgSc> = Vec::with_capacity(scan.components.len());
    for sc in &scan.components {
        let fc = &frame.components[sc.frame_index];
        let table = if ss == 0 {
            tables.dc[usize::from(sc.td)].as_ref().ok_or_else(|| {
                Error::invalid_input(env!("CARGO_PKG_NAME"), "JPEG: scan uses undefined DC table")
            })?
        } else {
            tables.ac[usize::from(sc.ta)].as_ref().ok_or_else(|| {
                Error::invalid_input(env!("CARGO_PKG_NAME"), "JPEG: scan uses undefined AC table")
            })?
        };
        scs.push(ProgSc {
            frame_index: sc.frame_index,
            h: usize::from(fc.h),
            v: usize::from(fc.v),
            table,
        });
    }

    // MCU grid: interleaved (DC) scans use the padded whole-image MCU grid; a non-interleaved scan
    // (AC, or a single-component DC scan) walks the component's own block grid, one block per MCU
    // (§A.2.2).
    let (mcus_x, mcus_y) = if interleaved {
        let hmax = usize::from(frame.hmax());
        let vmax = usize::from(frame.vmax());
        (
            usize::from(frame.x).div_ceil(8 * hmax),
            usize::from(frame.y).div_ceil(8 * vmax),
        )
    } else {
        (comps[scs[0].frame_index].bw, comps[scs[0].frame_index].bh)
    };

    let mut reader = BitReader::new(data, start);
    let restart = usize::from(tables.restart_interval);
    let mut preds = vec![0i32; scs.len()];
    let mut eobrun = 0u32;
    let mut mcus_done = 0usize;
    let mut expected_rst = 0u8;

    for my in 0..mcus_y {
        for mx in 0..mcus_x {
            if restart != 0 && mcus_done != 0 && mcus_done.is_multiple_of(restart) {
                reader.take_restart(expected_rst)?;
                expected_rst = (expected_rst + 1) & 7;
                preds.fill(0);
                eobrun = 0; // the EOB run does not cross a restart boundary (§G.1.2.2)
            }
            for (si, sc) in scs.iter().enumerate() {
                if ss == 0 {
                    // DC band: one block per (Hi, Vi) sub-position in an interleaved MCU, else one.
                    let (nh, nv) = if interleaved { (sc.h, sc.v) } else { (1, 1) };
                    for by in 0..nv {
                        for bx in 0..nh {
                            let (bcol, brow) = if interleaved {
                                (mx * sc.h + bx, my * sc.v + by)
                            } else {
                                (mx, my)
                            };
                            let c = &mut comps[sc.frame_index];
                            // Padding blocks in a partial rightmost/bottom MCU are decoded (to keep
                            // the entropy stream and DC predictor in sync) but not stored (§A.2.2).
                            let real = bcol < c.bw && brow < c.bh;
                            if ah == 0 {
                                let (t, diff) = decode_coded(sc.table, &mut reader)?;
                                if t > 11 {
                                    return Err(Error::invalid_input(
                                        env!("CARGO_PKG_NAME"),
                                        "JPEG: DC magnitude category > 11",
                                    ));
                                }
                                reader.consume(u32::from(t))?;
                                preds[si] = preds[si].wrapping_add(diff);
                                if real {
                                    // First pass: scale by the point transform 2^Al (§A.4, §G.1.2.1).
                                    c.block_mut(bcol, brow)[0] = preds[si] << al;
                                }
                            } else {
                                // Refinement: append the next lower bit of the DC coefficient (§G.1.2.3).
                                let bit = reader.read_bit()?;
                                if real && bit != 0 {
                                    c.block_mut(bcol, brow)[0] |= 1i32 << al;
                                }
                            }
                        }
                    }
                } else {
                    // AC band: single component, one block per MCU (non-interleaved).
                    let c = &mut comps[sc.frame_index];
                    let block = c.block_mut(mx, my);
                    if ah == 0 {
                        decode_ac_first(&mut reader, block, ss, se, al, sc.table, &mut eobrun)?;
                    } else {
                        decode_ac_refine(&mut reader, block, ss, se, al, sc.table, &mut eobrun)?;
                    }
                }
            }
            mcus_done += 1;
        }
    }

    let (_marker, marker_offset) = reader.end_marker()?;
    Ok(marker_offset)
}

/// Decodes one block's AC band in a first-pass progressive scan (`Ah = 0`, §G.1.2.2, Figure G.3
/// mirrored for decode). Coefficients are Huffman run/size coded within `[Ss..=Se]` and scaled by
/// the point transform `2^Al` (§A.4); `EOBRUN` counts whole all-zero-band blocks and persists across
/// blocks within the restart interval.
fn decode_ac_first(
    r: &mut BitReader,
    block: &mut [i32],
    ss: usize,
    se: usize,
    al: u8,
    ac: &DecTable,
    eobrun: &mut u32,
) -> Result<()> {
    // This block is inside a still-running EOB run: it is entirely zero in this band.
    if *eobrun > 0 {
        *eobrun -= 1;
        return Ok(());
    }
    let mut k = ss;
    while k <= se {
        let (rs, coeff) = decode_coded(ac, r)?;
        let run = usize::from(rs >> 4);
        let size = rs & 0x0F;
        if size == 0 {
            if run != 15 {
                // EOBn: run length is 2^run plus `run` appended bits; this block is the first of
                // the run, so decrement once (§G.1.2.2, Table G.1).
                *eobrun = 1u32 << run;
                if run != 0 {
                    *eobrun += r.read_bits(run as u32)?;
                }
                *eobrun -= 1;
                return Ok(());
            }
            k += 16; // ZRL: sixteen zero coefficients
        } else {
            k += run;
            if k > se {
                return Err(Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "JPEG: progressive AC run past band end",
                ));
            }
            r.consume(u32::from(size))?;
            block[ZIGZAG[k]] = coeff << al;
            k += 1;
        }
    }
    Ok(())
}

/// Decodes one block's AC band in a successive-approximation refinement scan (`Ah ≠ 0`, §G.1.2.3 /
/// §G.2.2). For every coefficient with a non-zero history a correction bit is read (a 1-bit adds one
/// to the scaled magnitude); newly non-zero coefficients (`SSSS = 1`) are inserted per the run/size
/// symbols; EOB-run blocks still consume correction bits for their non-zero-history coefficients.
fn decode_ac_refine(
    r: &mut BitReader,
    block: &mut [i32],
    ss: usize,
    se: usize,
    al: u8,
    ac: &DecTable,
    eobrun: &mut u32,
) -> Result<()> {
    let p1: i32 = 1i32 << al; // +1 in the bit position being refined
    let m1: i32 = -1i32 << al; // −1 in the bit position being refined
    let mut k = ss;
    if *eobrun == 0 {
        while k <= se {
            let rs = decode_symbol(ac, r)?;
            let mut run = i32::from(rs >> 4);
            let size = rs & 0x0F;
            let mut new_val = 0i32;
            if size != 0 {
                // In a refinement scan a newly non-zero coefficient always has magnitude 1; the one
                // appended bit is its sign (1 → positive, 0 → negative) (§G.1.2.3 rule a).
                if size != 1 {
                    return Err(Error::invalid_input(
                        env!("CARGO_PKG_NAME"),
                        "JPEG: progressive AC refinement coefficient size ≠ 1",
                    ));
                }
                new_val = if r.read_bit()? != 0 { p1 } else { m1 };
            } else if run != 15 {
                // EOBn: the rest of this block (and following blocks) is handled by the EOB tail.
                *eobrun = 1u32 << run;
                if run != 0 {
                    *eobrun += r.read_bits(run as u32)?;
                }
                break;
            }
            // else run == 15 (ZRL): skip sixteen zero-history coefficients, new_val stays 0.

            // Advance over the band, appending a correction bit to each non-zero-history coefficient
            // and counting down `run` zero-history coefficients to the target insertion position.
            // A `for` over the band's indices rather than a cursor stepped until it passes `se`, so
            // the walk ends with the band whatever the arithmetic on the cursor does. The rewrite
            // took the old cursor's killable siblings with it: of the two `k += 1` steps' four
            // mutants only the in-loop `*=` was a TIMEOUT, and the two `k > se` tests' six (`==`,
            // `<`, `>=`) were caught -- nine killable mutants gone, against two new ones on `t + 1`. With the EOB-run walk below,
            // `decode_ac_refine` went from 46 mutants to 35, smaller here, not stronger (#924).
            let mut target = None;
            for (i, &nat) in ZIGZAG[..=se].iter().enumerate().skip(k) {
                if block[nat] != 0 {
                    if r.read_bit()? != 0 {
                        block[nat] = refine(block[nat], p1, m1);
                    }
                } else {
                    run -= 1;
                    if run < 0 {
                        target = Some(i); // reached the target zero-history coefficient
                        break;
                    }
                }
            }
            let Some(t) = target else {
                // The band ran out first: an error for a new coefficient, which has nowhere to go;
                // a ZRL's sixteen zeros merely end the band, as nothing is written for them.
                if new_val != 0 {
                    return Err(Error::invalid_input(
                        env!("CARGO_PKG_NAME"),
                        "JPEG: progressive AC refinement past band end",
                    ));
                }
                break;
            };
            // The target has a zero history, so for a ZRL (`new_val == 0`) this write is a no-op.
            block[ZIGZAG[t]] = new_val;
            k = t + 1;
        }
    }
    if *eobrun > 0 {
        // A block within an EOB run: no new coefficients, but non-zero-history coefficients in the
        // remaining band still receive their correction bits (§G.1.2.3). The `for` replaced a
        // `while k <= se` cursor, and took its caught `<=` → `>` and `k += 1` → `-=` with it (its `*=`
        // was a TIMEOUT) (#924).
        for &nat in ZIGZAG.get(k..=se).unwrap_or_default() {
            if block[nat] != 0 && r.read_bit()? != 0 {
                block[nat] = refine(block[nat], p1, m1);
            }
        }
        *eobrun -= 1;
    }
    Ok(())
}

/// Applies one successive-approximation correction bit to a non-zero coefficient (§G.1.2.3): the
/// scaled magnitude increases by one bit — `+p1` for a positive coefficient, `+m1` (= `−p1`) for a
/// negative one. The refined bit position is always previously zero (the prior scan coded bit
/// `Ah = Al + 1` and above), so no guard against a double-set is needed.
///
/// The sign is read with `is_positive` rather than `coeff > 0`: `coeff` is never zero here, so
/// `> 0` and its `>= 0` mutant always agreed. That rewrite also took the comparison's two killable
/// siblings (`<`, `==`) out of the survey (#924).
fn refine(coeff: i32, p1: i32, m1: i32) -> i32 {
    if coeff.is_positive() {
        coeff.wrapping_add(p1)
    } else {
        coeff.wrapping_add(m1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The table `decode_ac_refine`'s tests decode with: `0x01` (a new coefficient) = `00`,
    /// `0xF0` (ZRL) = `01`, `0x00` (EOB0) = `10`, `0x21` (two zeros, then a new coefficient) =
    /// `110`.
    fn refine_table() -> DecTable {
        let mut bits = [0u8; 16];
        bits[1] = 3;
        bits[2] = 1;
        DecTable::from_bits(&bits, &[0x01, 0xF0, 0x00, 0x21]).unwrap()
    }

    /// `decode_ac_refine` (§G.1.2.3) over band 1..=3 holding `5` at 1 and zeros at 2 and 3: a new
    /// coefficient (sign bit `0`, so `−1`) is placed after the correction bit (`1`) that refines
    /// the `5` it passes, and the EOB0 that follows leaves 3 untouched and reads nothing more.
    /// Bits: `00` `0` `1` `10`, then `1`-padding.
    #[test]
    fn decode_ac_refine_inserts_after_correcting_what_it_passes() {
        let mut block = [0i32; 64];
        block[ZIGZAG[1]] = 5;
        let mut r = BitReader::new(&[0b0001_1011, 0xFF, 0xD9], 0);
        let mut eobrun = 0;
        decode_ac_refine(&mut r, &mut block, 1, 3, 0, &refine_table(), &mut eobrun).unwrap();
        assert_eq!(
            [block[ZIGZAG[1]], block[ZIGZAG[2]], block[ZIGZAG[3]]],
            [6, -1, 0]
        );
        assert_eq!(eobrun, 0);
        assert_eq!(r.read_bits(2).unwrap(), 0b11);
    }

    /// `decode_ac_refine`: a new coefficient whose run of zeros passes the band's end has nowhere
    /// to go. `0x21` (`110`) needs a third zero in a two-coefficient band.
    #[test]
    fn decode_ac_refine_rejects_a_new_coefficient_past_the_band() {
        let mut block = [0i32; 64];
        let mut r = BitReader::new(&[0b1101_1111, 0xFF, 0xD9], 0);
        let err =
            decode_ac_refine(&mut r, &mut block, 1, 2, 0, &refine_table(), &mut 0).unwrap_err();
        assert_eq!(
            err.static_message(),
            Some("JPEG: progressive AC refinement past band end")
        );
    }

    /// `decode_ac_refine`: a ZRL whose sixteen zeros pass the band's end only ends the band -- no
    /// error, nothing written, and no further symbol read (the six bits after its `01` are left).
    #[test]
    fn decode_ac_refine_ends_the_band_at_a_zrl_past_it() {
        let mut block = [0i32; 64];
        let mut r = BitReader::new(&[0b0110_1010, 0xFF, 0xD9], 0);
        let mut eobrun = 0;
        decode_ac_refine(&mut r, &mut block, 1, 2, 0, &refine_table(), &mut eobrun).unwrap();
        assert_eq!(block, [0; 64]);
        assert_eq!(eobrun, 0);
        assert_eq!(r.read_bits(6).unwrap(), 0b10_1010);
    }

    /// `decode_ac_refine` inside an EOB run: no symbol is decoded, and each non-zero-history
    /// coefficient of the band takes one correction bit in band order -- `0` leaves the `3`, `1`
    /// takes the `−2` to `−3` -- before the run counts down.
    #[test]
    fn decode_ac_refine_in_an_eob_run_only_corrects() {
        let mut block = [0i32; 64];
        block[ZIGZAG[1]] = 3;
        block[ZIGZAG[2]] = -2;
        let mut r = BitReader::new(&[0b0100_0000, 0xFF, 0xD9], 0);
        let mut eobrun = 1;
        decode_ac_refine(&mut r, &mut block, 1, 2, 0, &refine_table(), &mut eobrun).unwrap();
        assert_eq!([block[ZIGZAG[1]], block[ZIGZAG[2]]], [3, -3]);
        assert_eq!(eobrun, 0);
        assert_eq!(r.read_bits(6).unwrap(), 0);
    }
}
