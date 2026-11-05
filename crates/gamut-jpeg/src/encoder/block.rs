//! The per-block transform stage: sample planes with edge replication, chroma downsampling, and
//! level shift → forward DCT → quantization (§A.3.1 / §A.3.3 / §A.3.4), optionally through the
//! [`crate::rd`] trellis. Shared by the baseline and progressive processes.

use gamut_dsp::jpeg::forward_dct8x8;
use gamut_dsp::math::round_div_nearest;

use crate::rd::{self, RdCtx};

/// A single-channel sample plane at a component's own resolution (row-major, 8-bit).
pub(crate) struct Plane {
    pub(crate) data: Vec<u8>,
    pub(crate) width: usize,
    pub(crate) height: usize,
}

impl Plane {
    /// The sample at `(x, y)` with edge replication (clamping past the plane bounds), level-shifted
    /// to the signed baseline range by subtracting 128 (§A.3.1, `P = 8`). Edge replication is the
    /// encoder's free choice for padding partial edge blocks/MCUs to a whole 8×8 (§A.2.3): repeating
    /// the border minimizes spurious high-frequency energy versus zero-fill.
    pub(crate) fn level_shifted(&self, x: usize, y: usize) -> i32 {
        let cx = x.min(self.width - 1);
        let cy = y.min(self.height - 1);
        i32::from(self.data[cy * self.width + cx]) - 128
    }
}

/// Gathers and level-shifts one 8×8 block of `plane` at block coordinates `(bx, by)` and runs the
/// forward DCT (§A.3.1 / §A.3.3), returning the natural-order **unquantized** coefficients.
fn dct_block(plane: &Plane, bx: usize, by: usize) -> [i32; 64] {
    // Gather the level-shifted samples in natural (raster) order.
    let mut block = [0i32; 64];
    for row in 0..8usize {
        for col in 0..8usize {
            block[row * 8 + col] = plane.level_shifted(bx * 8 + col, by * 8 + row);
        }
    }
    forward_dct8x8(&mut block);
    block
}

/// Level-shifts, forward-transforms and quantizes one 8×8 block of `plane` at block coordinates
/// `(bx, by)` (§A.3.1 / §A.3.3 / §A.3.4), returning the natural-order quantized coefficients. Shared
/// by the baseline single-pass coder (the entropy stage's `encode_block`) and the progressive encoder
/// ([`crate::progressive`]), which materializes every block up front before running the scan script.
pub(crate) fn quantize_block(plane: &Plane, quant: &[u8; 64], bx: usize, by: usize) -> [i32; 64] {
    let block = dct_block(plane, bx, by);
    // Quantize (§A.3.4): round-to-nearest divide by the table entry (which is ≥ 1).
    let mut q = [0i32; 64];
    for (dst, (&coeff, &step)) in q.iter_mut().zip(block.iter().zip(quant.iter())) {
        *dst = round_div_nearest(coeff, i32::from(step));
    }
    q
}

/// [`quantize_block`] with an optional rate–distortion context: `None` is exactly the plain
/// nearest-rounding path (the frozen default, byte-for-byte), `Some` routes the unquantized DCT
/// output through the [`crate::rd`] trellis. The single quantization seam shared by the baseline
/// and progressive processes, so an RD choice is identical in both.
pub(crate) fn quantize_block_rd(
    plane: &Plane,
    quant: &[u8; 64],
    bx: usize,
    by: usize,
    rd: Option<&RdCtx>,
) -> [i32; 64] {
    match rd {
        None => quantize_block(plane, quant, bx, by),
        Some(ctx) => rd::trellis_quantize(&dct_block(plane, bx, by), quant, ctx),
    }
}

/// Box-averages `plane` (row-major, `width`×`height`) by `(sx, sy)`, producing a
/// `ceil(width/sx)`×`ceil(height/sy)` plane. Partial edge boxes average only the samples that
/// exist (equivalent to edge replication). With `sx == sy == 1` this is an exact copy (4:4:4).
///
/// Box averaging is the encoder's documented free choice; T.81 leaves the subsampling filter open,
/// and T.871 §9 NOTE 1 suggests a simple two-tap `(½, ½)` filter for 2:1.
pub(super) fn downsample(plane: &[u8], width: usize, height: usize, sx: usize, sy: usize) -> Plane {
    let cw = width.div_ceil(sx);
    let ch = height.div_ceil(sy);
    let mut data = vec![0u8; cw * ch];
    for cy in 0..ch {
        for cx in 0..cw {
            let (mut sum, mut count) = (0u32, 0u32);
            for dy in 0..sy {
                for dx in 0..sx {
                    let px = cx * sx + dx;
                    let py = cy * sy + dy;
                    if px < width && py < height {
                        sum += u32::from(plane[py * width + px]);
                        count += 1;
                    }
                }
            }
            data[cy * cw + cx] = ((sum + count / 2) / count) as u8;
        }
    }
    Plane {
        data,
        width: cw,
        height: ch,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downsample_444_is_identity() {
        let src = [10u8, 20, 30, 40];
        let p = downsample(&src, 2, 2, 1, 1);
        assert_eq!((p.width, p.height), (2, 2));
        assert_eq!(p.data, src);
    }

    #[test]
    fn downsample_420_box_averages_with_rounding() {
        // A 2×2 plane → one sample = round((10+20+30+40)/4) = round(25) = 25.
        let src = [10u8, 20, 30, 40];
        let p = downsample(&src, 2, 2, 2, 2);
        assert_eq!((p.width, p.height), (1, 1));
        assert_eq!(p.data, vec![25]);
        // Odd width → partial edge box averages only existing samples (edge replication).
        // 3×1 plane, sx=2: box0 = round((10+20)/2)=15, box1 = just 30.
        let odd = [10u8, 20, 30];
        let q = downsample(&odd, 3, 1, 2, 1);
        assert_eq!((q.width, q.height), (2, 1));
        assert_eq!(q.data, vec![15, 30]);
    }

    #[test]
    fn downsample_vertical_only() {
        // A 1×4 column with (sx, sy) = (1, 2): box 0 = rows 0–1 → round((10+20)/2) = 15, box 1 =
        // rows 2–3 → round((30+40)/2) = 35. A height-1 output cannot see a broken `cy·sy` source
        // row; box 1 (cy = 1) pins it — a `cy/sy` mutant re-reads rows 0–1 and yields 15, not 35.
        let src = [10u8, 20, 30, 40];
        let p = downsample(&src, 1, 4, 1, 2);
        assert_eq!((p.width, p.height), (1, 2));
        assert_eq!(p.data, vec![15, 35]);
    }

    #[test]
    fn level_shift_clamps_and_subtracts_128() {
        let plane = Plane {
            data: vec![200u8, 100, 50, 0],
            width: 2,
            height: 2,
        };
        assert_eq!(plane.level_shifted(0, 0), 200 - 128);
        assert_eq!(plane.level_shifted(1, 1), 0 - 128);
        // Past the right/bottom edge replicates the border sample (index 3 = 0 → −128).
        assert_eq!(plane.level_shifted(5, 5), 0 - 128);
        assert_eq!(plane.level_shifted(0, 9), 50 - 128); // clamps y to row 1, col 0
    }
}
