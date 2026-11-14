//! VP8 macroblock reconstruction into [`FrameBuffers`] (RFC 6386 §12, §14, §15), shared by the
//! encoder and the decoder: the per-segment quantizer factors, the whole-block and per-subblock
//! prediction neighbourhoods read from the reconstruction itself, dequantization plus inverse
//! transform written back as `pred + residue`, and the final whole-frame loop-filter pass.
//!
//! Both directions call exactly these functions, which is what makes the encoder's reconstruction
//! bit-identical to the decoder's output.

// The macroblock/block math indexes several fixed-size arrays in lock-step (and over partial ranges
// like `1..16`), where explicit indices read closer to the spec than iterator adaptors.
#![allow(clippy::needless_range_loop)]

use gamut_color::{ChromaSubsampling, Planar8};
use gamut_core::{Dimensions, Result};
use gamut_dsp::math::clip_pixel8;

use super::frame::MbLevels;
use super::header::{LoopFilterParams, Segmentation, Vp8FrameHeader};
use super::quant::{self, QuantFactors};
use super::transform::{idct4x4, iwht4x4};
use super::{loop_filter, prediction};

/// The clamped base quantizer index for segment `s` (RFC 6386 §9.3/§10): the absolute or
/// delta-adjusted value when segmentation is enabled, else the frame base.
fn segment_q_index(seg: &Segmentation, base_y_ac: u8, s: usize) -> i32 {
    if !seg.enabled {
        return i32::from(base_y_ac);
    }
    let q = if seg.abs_delta {
        i32::from(seg.quantizer[s])
    } else {
        i32::from(base_y_ac) + i32::from(seg.quantizer[s])
    };
    q.clamp(0, 127)
}

/// The four per-segment quantizer factor sets for a frame (all equal when segmentation is disabled).
pub(super) fn segment_quant_factors(header: &Vp8FrameHeader) -> [QuantFactors; 4] {
    core::array::from_fn(|s| {
        let base_q = segment_q_index(&header.segmentation, header.quant.y_ac, s);
        QuantFactors::new(base_q, &header.quant)
    })
}

/// Macroblock-aligned reconstructed YUV planes (luma `mb_cols*16 × mb_rows*16`, chroma half each).
pub struct FrameBuffers {
    width: u32,
    height: u32,
    pub(super) mb_cols: usize,
    pub(super) mb_rows: usize,
    pub(super) y: Vec<u8>,
    pub(super) u: Vec<u8>,
    pub(super) v: Vec<u8>,
}

impl FrameBuffers {
    pub(super) fn new(width: u32, height: u32) -> Self {
        let mb_cols = (width as usize).div_ceil(16);
        let mb_rows = (height as usize).div_ceil(16);
        Self {
            width,
            height,
            mb_cols,
            mb_rows,
            y: vec![0u8; mb_cols * 16 * mb_rows * 16],
            u: vec![0u8; mb_cols * 8 * mb_rows * 8],
            v: vec![0u8; mb_cols * 8 * mb_rows * 8],
        }
    }

    pub(super) fn y_stride(&self) -> usize {
        self.mb_cols * 16
    }

    pub(super) fn c_stride(&self) -> usize {
        self.mb_cols * 8
    }

    /// Crops the reconstruction to visible-resolution 4:2:0 [`Planar8`] planes.
    ///
    /// # Errors
    ///
    /// None in practice: a frame is non-empty (its header's 14-bit sides are checked when it is
    /// built) and its cropped planes match its dimensions by construction. The `Result` carries
    /// [`Planar8`]'s own validation out instead of panicking on it.
    pub fn to_yuv420(&self) -> Result<Planar8> {
        let (w, h) = (self.width as usize, self.height as usize);
        let (cw, ch) = ChromaSubsampling::Cs420.chroma_dimensions(self.width, self.height);
        let (cw, ch) = (cw as usize, ch as usize);
        let crop = |plane: &[u8], stride: usize, pw: usize, ph: usize| {
            let mut out = vec![0u8; pw * ph];
            for row in 0..ph {
                out[row * pw..row * pw + pw]
                    .copy_from_slice(&plane[row * stride..row * stride + pw]);
            }
            out
        };
        let y = crop(&self.y, self.y_stride(), w, h);
        let u = crop(&self.u, self.c_stride(), cw, ch);
        let v = crop(&self.v, self.c_stride(), cw, ch);
        Planar8::from_planes_subsampled(
            Dimensions::new(self.width, self.height)?,
            ChromaSubsampling::Cs420,
            [y, u, v],
        )
    }
}

/// The clamped loop-filter level for segment `s` (RFC 6386 §10/§15.4): the segment's absolute or
/// delta-adjusted filter strength when segmentation is enabled, else the frame base level.
fn segment_filter_level(base: u8, seg: &Segmentation, s: usize) -> u8 {
    if !seg.enabled {
        return base;
    }
    let level = if seg.abs_delta {
        i32::from(seg.filter_strength[s])
    } else {
        i32::from(base) + i32::from(seg.filter_strength[s])
    };
    level.clamp(0, 63) as u8
}

/// Applies the frame's configured loop filter to the reconstruction as a final whole-frame pass: the
/// simple filter deblocks luma only, the normal filter luma and chroma. Each macroblock is filtered at
/// its segment's level (uniform when segmentation is disabled); an all-zero level set is a no-op.
pub(super) fn apply_loop_filter(
    recon: &mut FrameBuffers,
    lf: &LoopFilterParams,
    seg: &Segmentation,
    segment_map: &[usize],
    filter_interior: &[bool],
    is_bpred: &[bool],
) {
    // A zero frame-level filter level disables the loop filter for the whole frame (RFC 6386 §9.4;
    // matches libwebp's `filter_type = 0`), regardless of any per-segment or per-macroblock delta.
    if lf.level == 0 {
        return;
    }
    // Per-macroblock level: the segment-adjusted base plus the §9.4 deltas. Key frames are all-intra,
    // so the intra reference-frame delta (`ref_frame[0]`) applies to every macroblock and the B_PRED
    // mode delta (`mode[0]`) to 4×4-predicted ones; the sum is clamped to [0, 63].
    let mb_level: Vec<u8> = segment_map
        .iter()
        .zip(is_bpred)
        .map(|(&s, &bpred)| {
            let mut level = i32::from(segment_filter_level(lf.level, seg, s));
            level += i32::from(lf.deltas.ref_frame[0]);
            if bpred {
                level += i32::from(lf.deltas.mode[0]);
            }
            level.clamp(0, 63) as u8
        })
        .collect();
    if mb_level.iter().all(|&l| l == 0) {
        return;
    }
    let (ys, cs, mbc, mbr) = (
        recon.y_stride(),
        recon.c_stride(),
        recon.mb_cols,
        recon.mb_rows,
    );
    if lf.simple {
        loop_filter::simple_filter_luma(
            &mut recon.y,
            ys,
            mbc,
            mbr,
            &mb_level,
            lf.sharpness,
            filter_interior,
        );
    } else {
        loop_filter::normal_filter(
            &mut recon.y,
            &mut recon.u,
            &mut recon.v,
            ys,
            cs,
            mbc,
            mbr,
            &mb_level,
            lf.sharpness,
            filter_interior,
        );
    }
}

/// Gathers the `n`-pixel row at `(x, y)` of `plane` into a fixed buffer (only `[..n]` is meaningful).
fn row_at(plane: &[u8], stride: usize, x: usize, y: usize, n: usize) -> [u8; 16] {
    let mut b = [0u8; 16];
    b[..n].copy_from_slice(&plane[y * stride + x..y * stride + x + n]);
    b
}

/// Gathers the `n`-pixel column at `(x, y)` of `plane` into a fixed buffer.
fn col_at(plane: &[u8], stride: usize, x: usize, y: usize, n: usize) -> [u8; 16] {
    let mut b = [0u8; 16];
    for (r, slot) in b[..n].iter_mut().enumerate() {
        *slot = plane[(y + r) * stride + x];
    }
    b
}

/// Extracts the 4×4 sub-block at `(sub_x, sub_y)` of a `stride`-wide prediction block, as 16-bit.
pub(super) fn sub_pred(pred: &[u8], stride: usize, sub_x: usize, sub_y: usize) -> [i16; 16] {
    let mut out = [0i16; 16];
    for r in 0..4 {
        for c in 0..4 {
            out[r * 4 + c] = i16::from(pred[(sub_y + r) * stride + sub_x + c]);
        }
    }
    out
}

/// Writes `clip_pixel8(pred + residue)` into the 4×4 block at `(x, y)` of `plane`.
pub(super) fn write_block(
    plane: &mut [u8],
    stride: usize,
    x: usize,
    y: usize,
    pred: &[i16; 16],
    residue: &[i16; 16],
) {
    for r in 0..4 {
        for c in 0..4 {
            let v = i32::from(pred[r * 4 + c]) + i32::from(residue[r * 4 + c]);
            plane[(y + r) * stride + x + c] = clip_pixel8(v);
        }
    }
}

/// The above-left corner pixel for prediction: 127 on the top macroblock row, 129 on the left column,
/// otherwise the reconstructed pixel (RFC 6386 §12.2).
fn corner_pixel(plane: &[u8], stride: usize, px: usize, py: usize, mb_x: usize, mb_y: usize) -> u8 {
    if mb_y == 0 {
        127
    } else if mb_x == 0 {
        129
    } else {
        plane[(py - 1) * stride + px - 1]
    }
}

/// One reconstructed luma pixel, or its off-frame edge value (127 above the frame, 129 to the left).
fn luma_pixel(recon: &FrameBuffers, y: i32, x: i32) -> u8 {
    if y < 0 {
        127
    } else if x < 0 {
        129
    } else {
        recon.y[y as usize * recon.y_stride() + x as usize]
    }
}

/// The four above-right pixels of the macroblock's top-right subblock, shared by all right-column
/// subblocks (RFC 6386 §12.3 `copy_down`). Matching libwebp: 127 on the top row; the next
/// macroblock's top-left four pixels normally; or the current macroblock's last above pixel
/// replicated on the rightmost column (`frame_dec.c`: `memset(top_right, top[15])`).
pub(super) fn above_right_source(recon: &FrameBuffers, mb_x: usize, mb_y: usize) -> [u8; 4] {
    if mb_y == 0 {
        return [127; 4];
    }
    let stride = recon.y_stride();
    let row = (mb_y * 16 - 1) * stride;
    if mb_x + 1 >= recon.mb_cols {
        [recon.y[row + mb_x * 16 + 15]; 4]
    } else {
        let base = row + mb_x * 16 + 16;
        [
            recon.y[base],
            recon.y[base + 1],
            recon.y[base + 2],
            recon.y[base + 3],
        ]
    }
}

/// Gathers a 4×4 luma subblock's prediction neighbors from the in-place reconstruction: the eight
/// above pixels `A[0..8]` (four above, four above-right), the four left `L[0..4]`, and the above-left
/// corner. `(sx, sy)` is the subblock's top-left in frame coordinates and `c` its column within the
/// macroblock (the right column, `c == 3`, takes its above-right from the shared `above_right`).
pub(super) fn subblock_neighbors(
    recon: &FrameBuffers,
    sx: usize,
    sy: usize,
    c: usize,
    above_right: &[u8; 4],
) -> ([u8; 8], [u8; 4], u8) {
    let (xi, yi) = (sx as i32, sy as i32);
    let corner = luma_pixel(recon, yi - 1, xi - 1);
    let mut a = [0u8; 8];
    for k in 0..4 {
        a[k] = luma_pixel(recon, yi - 1, xi + k as i32);
    }
    if c == 3 {
        a[4..8].copy_from_slice(above_right);
    } else {
        for k in 0..4 {
            a[4 + k] = luma_pixel(recon, yi - 1, xi + 4 + k as i32);
        }
    }
    let mut l = [0u8; 4];
    for k in 0..4 {
        l[k] = luma_pixel(recon, yi + k as i32, xi - 1);
    }
    (a, l, corner)
}

/// Produces the 16×16 luma prediction for macroblock `(mb_x, mb_y)` under whole-block `mode`.
pub(super) fn predict_luma(
    recon: &FrameBuffers,
    mb_x: usize,
    mb_y: usize,
    mode: usize,
) -> [u8; 256] {
    let (px, py, stride) = (mb_x * 16, mb_y * 16, recon.y_stride());
    let above = (mb_y > 0).then(|| row_at(&recon.y, stride, px, py - 1, 16));
    let left = (mb_x > 0).then(|| col_at(&recon.y, stride, px - 1, py, 16));
    let corner = corner_pixel(&recon.y, stride, px, py, mb_x, mb_y);
    let mut pred = [0u8; 256];
    prediction::predict_block(
        mode,
        16,
        above.as_ref().map(|a| &a[..16]),
        left.as_ref().map(|l| &l[..16]),
        corner,
        &mut pred,
    );
    pred
}

/// Produces the 8×8 prediction for one chroma plane under whole-block `mode`.
pub(super) fn predict_chroma(
    plane: &[u8],
    stride: usize,
    mb_x: usize,
    mb_y: usize,
    mode: usize,
) -> [u8; 64] {
    let (px, py) = (mb_x * 8, mb_y * 8);
    let above = (mb_y > 0).then(|| row_at(plane, stride, px, py - 1, 8));
    let left = (mb_x > 0).then(|| col_at(plane, stride, px - 1, py, 8));
    let corner = corner_pixel(plane, stride, px, py, mb_x, mb_y);
    let mut pred = [0u8; 64];
    prediction::predict_block(
        mode,
        8,
        above.as_ref().map(|a| &a[..8]),
        left.as_ref().map(|l| &l[..8]),
        corner,
        &mut pred,
    );
    pred
}

/// Reconstructs the 16 luma sub-blocks of a macroblock: the Y2 inverse-WHT supplies each sub-block's
/// DC, the AC levels are dequantized, and `pred + idct` is written into the recon buffer. Shared by
/// the encoder and decoder.
pub(super) fn reconstruct_luma(
    recon: &mut FrameBuffers,
    mb_x: usize,
    mb_y: usize,
    pred: &[u8; 256],
    levels: &MbLevels,
    qf: &QuantFactors,
) {
    let mut y2_dq = [0i16; 16];
    y2_dq[0] = quant::dequantize(levels.y2[0], qf.y2_dc);
    for k in 1..16 {
        y2_dq[k] = quant::dequantize(levels.y2[k], qf.y2_ac);
    }
    let dc = iwht4x4(&y2_dq);

    let stride = recon.y_stride();
    for i in 0..16 {
        let mut dq = [0i16; 16];
        dq[0] = dc[i];
        for k in 1..16 {
            dq[k] = quant::dequantize(levels.y[i][k], qf.y1_ac);
        }
        let residue = idct4x4(&dq);
        let (sc, sr) = (i % 4, i / 4);
        write_block(
            &mut recon.y,
            stride,
            mb_x * 16 + sc * 4,
            mb_y * 16 + sr * 4,
            &sub_pred(pred, 16, sc * 4, sr * 4),
            &residue,
        );
    }
}

/// Reconstructs the four sub-blocks of one chroma plane from full (DC+AC) levels.
pub(super) fn reconstruct_chroma(
    plane: &mut [u8],
    stride: usize,
    mb_x: usize,
    mb_y: usize,
    pred: &[u8; 64],
    levels: &[[i16; 16]; 4],
    qf: &QuantFactors,
) {
    for i in 0..4 {
        let mut dq = [0i16; 16];
        dq[0] = quant::dequantize(levels[i][0], qf.uv_dc);
        for k in 1..16 {
            dq[k] = quant::dequantize(levels[i][k], qf.uv_ac);
        }
        let residue = idct4x4(&dq);
        let (sc, sr) = (i % 2, i / 2);
        write_block(
            plane,
            stride,
            mb_x * 8 + sc * 4,
            mb_y * 8 + sr * 4,
            &sub_pred(pred, 8, sc * 4, sr * 4),
            &residue,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segment_filter_level_applies_signed_delta() {
        // Delta mode: the per-segment level is `base + filter_strength`, clamped to [0, 63]. A
        // negative delta must lower the level (pins the `+`, which flipped to `-` would raise it).
        let seg = Segmentation {
            enabled: true,
            abs_delta: false,
            filter_strength: [10, -10, 0, 40],
            ..Default::default()
        };
        assert_eq!(segment_filter_level(30, &seg, 0), 40);
        assert_eq!(segment_filter_level(30, &seg, 1), 20);
        assert_eq!(segment_filter_level(30, &seg, 3), 63);
        // Absolute mode and the disabled fast path.
        let abs = Segmentation {
            enabled: true,
            abs_delta: true,
            filter_strength: [25, 0, 0, 0],
            ..Default::default()
        };
        assert_eq!(segment_filter_level(30, &abs, 0), 25);
        assert_eq!(segment_filter_level(30, &Segmentation::default(), 0), 30);
    }
}
