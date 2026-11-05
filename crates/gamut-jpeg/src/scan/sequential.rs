//! Sequential-process scans (Annex F, §F.2.2): the MCU walk (§A.2) that decodes each block's
//! differential DC and run-length AC coefficients and reconstructs it straight into its
//! component's sample plane, including the DNL-deferred (`Y = 0`) height case.

use gamut_core::{Error, Result};
use gamut_dsp::jpeg::inverse_dct8x8_int_dc;

use super::reader::{BitReader, decode_coded};
use super::{Plane, write_block, write_flat};
use crate::huffman::DecTable;
use crate::syntax::{Frame, ScanHeader, Tables};
use crate::zigzag::ZIGZAG;

/// The result of decoding one scan: each coded component's plane (paired with its frame index) and
/// the position of the marker that terminated the entropy data, where the segment parser resumes.
pub struct ScanResult {
    /// `(frame_component_index, plane)` for every component coded by this scan.
    pub planes: Vec<(usize, Plane)>,
    /// Byte offset of the terminating marker's `0xFF` prefix, where the segment parser resumes.
    pub marker_offset: usize,
}

/// Allocation ceiling for a sequential scan whose frame height is deferred to DNL.
#[derive(Clone, Copy)]
pub struct McuRowLimit {
    /// Maximum number of MCU rows the configured limits can permit.
    pub rows: usize,
    /// Static [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) message identifying
    /// the limit that supplied this ceiling.
    pub message: &'static str,
}

/// Per-scan-component decode context: the tables it uses, its sampling, and its running DC predictor
/// and output plane.
struct Ctx<'a> {
    frame_index: usize,
    h: u8,
    v: u8,
    dc: &'a DecTable,
    ac: &'a DecTable,
    quant: &'a [u16; 64],
    pred: i32,
    plane: Plane,
}

impl Ctx<'_> {
    /// Decodes one 8×8 block (§F.2.2): the differential DC coefficient then the run/size AC
    /// coefficients, dequantizes into natural order, inverse-transforms, level-shifts (+128) and
    /// clamps to `0..=255`, and writes the samples into the plane at block position `(bx, by)`
    /// (§A.3.1).
    fn decode_block(&mut self, r: &mut BitReader, bx: usize, by: usize) -> Result<()> {
        let mut zz = [0i32; 64];
        // DC: differential against the running predictor (§F.2.2.1).
        let (t, diff) = decode_coded(self.dc, r)?;
        if t > 11 {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "JPEG: DC magnitude category > 11",
            ));
        }
        r.consume(u32::from(t))?;
        self.pred = self.pred.wrapping_add(diff);
        zz[0] = self.pred.wrapping_mul(i32::from(self.quant[0]));
        // AC: run/size symbols in zig-zag order (§F.2.2.2). `last` is the zig-zag index of the last
        // coded AC coefficient, `0` while none has been: the IDCT's sparsity hint.
        let mut last = 0usize;
        let mut k = 1usize;
        while k < 64 {
            let (rs, coeff) = decode_coded(self.ac, r)?;
            let run = usize::from(rs >> 4);
            let size = rs & 0x0F;
            if size == 0 {
                if run == 15 {
                    k += 16; // ZRL: sixteen zero coefficients
                    continue;
                }
                break; // EOB: the rest of the block is zero
            }
            k += run;
            if k > 63 {
                return Err(Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "JPEG: AC coefficient index past 63",
                ));
            }
            r.consume(u32::from(size))?;
            let natural = ZIGZAG[k];
            zz[natural] = coeff.wrapping_mul(i32::from(self.quant[natural]));
            last = k;
            k += 1;
        }
        // Inverse DCT, level shift and clamp (§A.3.1). A block that coded no AC coefficient is
        // flat, and its one value comes from the kernel's DC shortcut without a transform.
        if last == 0 {
            write_flat(inverse_dct8x8_int_dc(zz[0]), &mut self.plane, bx, by);
        } else {
            write_block(&mut zz, &mut self.plane, bx, by);
        }
        Ok(())
    }
}

/// Decodes the entropy data of one scan starting at byte `start`, per §A.2 and Annex F.
///
/// `frame` gives the component geometry (Hmax/Vmax, X/Y); `scan` selects the coded components and
/// their tables; `tables` holds the current dequantization/Huffman tables and restart interval. When
/// `frame.y == 0` the MCU-row count is unknown up front and rows are decoded until the entropy data
/// ends at a marker (a `Y = 0` frame's height arrives in the following DNL segment).
///
/// # Errors
///
/// An [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error for an undefined
/// referenced table, a corrupt Huffman code, an out-of-range coefficient, a bad restart sequence, or a truncated stream.
pub fn decode_scan(
    data: &[u8],
    start: usize,
    frame: &Frame,
    scan: &ScanHeader,
    tables: &Tables,
    row_limit: Option<McuRowLimit>,
) -> Result<ScanResult> {
    let hmax = usize::from(frame.hmax());
    let vmax = usize::from(frame.vmax());
    let x = usize::from(frame.x);
    let interleaved = scan.interleaved();

    // Horizontal MCU count is always known (X ≠ 0). The vertical count is known only when Y ≠ 0.
    let mcus_x = if interleaved {
        x.div_ceil(8 * hmax)
    } else {
        // Non-interleaved: MCU = one block; blocks span the component's own width (§A.2.2).
        let fc = &frame.components[scan.components[0].frame_index];
        let comp_w = (x * usize::from(fc.h)).div_ceil(hmax);
        comp_w.div_ceil(8)
    };
    let mcus_y: Option<usize> = if frame.y == 0 {
        None
    } else {
        let y = usize::from(frame.y);
        Some(if interleaved {
            y.div_ceil(8 * vmax)
        } else {
            let fc = &frame.components[scan.components[0].frame_index];
            let comp_h = (y * usize::from(fc.v)).div_ceil(vmax);
            comp_h.div_ceil(8)
        })
    };

    // Build a decode context per scan component, sized to its block layout in this scan.
    let mut ctxs: Vec<Ctx> = Vec::with_capacity(scan.components.len());
    for sc in &scan.components {
        let fc = &frame.components[sc.frame_index];
        let dc = tables.dc[usize::from(sc.td)].as_ref().ok_or_else(|| {
            Error::invalid_input(env!("CARGO_PKG_NAME"), "JPEG: scan uses undefined DC table")
        })?;
        let ac = tables.ac[usize::from(sc.ta)].as_ref().ok_or_else(|| {
            Error::invalid_input(env!("CARGO_PKG_NAME"), "JPEG: scan uses undefined AC table")
        })?;
        let quant = tables.quant[usize::from(fc.tq)].as_ref().ok_or_else(|| {
            Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "JPEG: scan uses undefined quant table",
            )
        })?;
        let blk_cols = if interleaved { usize::from(fc.h) } else { 1 };
        let blocks_per_line = mcus_x * blk_cols;
        ctxs.push(Ctx {
            frame_index: sc.frame_index,
            h: fc.h,
            v: fc.v,
            dc,
            ac,
            quant,
            pred: 0,
            plane: Plane {
                data: Vec::new(),
                stride: blocks_per_line * 8,
                h: fc.h,
                v: fc.v,
            },
        });
    }

    let mut reader = BitReader::new(data, start);
    let restart = usize::from(tables.restart_interval);
    let mut mcus_done = 0usize;
    let mut expected_rst = 0u8;
    let mut mcu_y = 0usize;

    loop {
        match mcus_y {
            Some(rows) if mcu_y >= rows => break,
            None if reader.at_data_end() => break,
            None if row_limit.is_some_and(|limit| mcu_y >= limit.rows) => {
                return Err(Error::unsupported(
                    env!("CARGO_PKG_NAME"),
                    row_limit.map_or("JPEG: image exceeds a decoder limit", |limit| limit.message),
                ));
            }
            _ => {}
        }
        for mcu_x in 0..mcus_x {
            if restart != 0 && mcus_done != 0 && mcus_done.is_multiple_of(restart) {
                reader.take_restart(expected_rst)?;
                expected_rst = (expected_rst + 1) & 7;
                for ctx in &mut ctxs {
                    ctx.pred = 0;
                }
            }
            for ctx in &mut ctxs {
                if interleaved {
                    let (h, v) = (usize::from(ctx.h), usize::from(ctx.v));
                    for by in 0..v {
                        for bx in 0..h {
                            ctx.decode_block(&mut reader, mcu_x * h + bx, mcu_y * v + by)?;
                        }
                    }
                } else {
                    ctx.decode_block(&mut reader, mcu_x, mcu_y)?;
                }
            }
            mcus_done += 1;
        }
        mcu_y += 1;
    }

    let (_marker, marker_offset) = reader.end_marker()?;
    let planes = ctxs.into_iter().map(|c| (c.frame_index, c.plane)).collect();
    Ok(ScanResult {
        planes,
        marker_offset,
    })
}
