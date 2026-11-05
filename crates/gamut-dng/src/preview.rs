//! A small RGB preview/thumbnail for IFD 0, derived from the raw image.
//!
//! DNG stores a viewable preview in IFD 0 and the full-resolution raw in a sub-IFD. This produces a
//! simple, correct preview by collapsing each `2 × 2` block into one RGB pixel — for a CFA mosaic
//! that doubles as a trivial demosaic (averaging the sensels of each colour in the repeat tile),
//! for a linear image it box-averages the planes. It is intentionally basic (no proper demosaic,
//! gamma, or size cap); a higher-quality / JPEG-compressed preview is a later phase (see
//! `STATUS.md`).

use gamut_core::{Dimensions, Result};

use crate::raw::{RawImage, RawPhotometry, cfa_color};

/// Builds an 8-bit RGB preview from `raw`, returning its dimensions and interleaved `RGB8` samples.
///
/// The preview is `floor(width / 2) × floor(height / 2)` pixels (at least `1 × 1`).
///
/// Fallible only because [`Dimensions::new`] is; the `1 × 1` floor means it never fails.
pub(crate) fn raw_preview(raw: &RawImage) -> Result<(Dimensions, Vec<u8>)> {
    match raw.photometry() {
        RawPhotometry::Cfa {
            repeat, pattern, ..
        } => cfa_preview(raw, *repeat, pattern),
        RawPhotometry::LinearRaw { planes } => linear_preview(raw, *planes),
    }
}

/// 8-bit scaling of a linear sample value given the black level and range.
fn scale8(value: u32, black: u32, range: u32) -> u8 {
    let scaled = u64::from(value.saturating_sub(black)) * 255 / u64::from(range);
    scaled.min(255) as u8
}

/// A `(black, range)` pair for basic 8-bit preview scaling of `plane`: the plane's minimum
/// pattern black (deltas ignored — this is a thumbnail, not the Chapter-5 mapping) and its
/// white-to-black range, rounded to integers.
fn plane_scale(raw: &RawImage, plane: usize) -> (u32, u32) {
    let levels = raw.levels();
    let spp = usize::from(levels.samples_per_pixel());
    let black = levels
        .black()
        .iter()
        .skip(plane)
        .step_by(spp)
        .fold(f64::INFINITY, |acc, &b| acc.min(b)) as u32;
    let white = levels.white()[plane] as u32;
    (black, white.saturating_sub(black).max(1))
}

/// Collapses each CFA repeat tile into one RGB pixel by averaging each colour's sensels.
fn cfa_preview(
    raw: &RawImage,
    repeat: (u16, u16),
    pattern: &[u8],
) -> Result<(Dimensions, Vec<u8>)> {
    let w = raw.dimensions().width() as usize;
    let h = raw.dimensions().height() as usize;
    let (rr, rc) = (repeat.0 as usize, repeat.1 as usize);
    let samples = raw.samples();
    let pw = (w / rc).max(1);
    let ph = (h / rr).max(1);
    // One plane per mosaic sensel; the single-plane scale serves all preview channels.
    let (black, range) = plane_scale(raw, 0);

    let mut out = Vec::with_capacity(pw * ph * 3);
    for py in 0..ph {
        for px in 0..pw {
            let mut sum = [0u64; 3];
            let mut count = [0u64; 3];
            for ty in 0..rr {
                let sy = py * rr + ty;
                if sy >= h {
                    break;
                }
                for tx in 0..rc {
                    let sx = px * rc + tx;
                    if sx >= w {
                        break;
                    }
                    let channel = match pattern[ty * rc + tx] {
                        cfa_color::RED => 0,
                        cfa_color::GREEN => 1,
                        cfa_color::BLUE => 2,
                        _ => continue, // non-RGB CFA colours don't map to the RGB preview
                    };
                    sum[channel] += u64::from(samples[sy * w + sx]);
                    count[channel] += 1;
                }
            }
            for ch in 0..3 {
                let avg = sum[ch].checked_div(count[ch]).unwrap_or(0) as u32;
                out.push(scale8(avg, black, range));
            }
        }
    }
    Ok((Dimensions::new(pw as u32, ph as u32)?, out))
}

/// Box-averages each `2 × 2` block of a linear image into one RGB pixel.
fn linear_preview(raw: &RawImage, planes: u16) -> Result<(Dimensions, Vec<u8>)> {
    let w = raw.dimensions().width() as usize;
    let h = raw.dimensions().height() as usize;
    let p = usize::from(planes);
    let samples = raw.samples();
    let pw = (w / 2).max(1);
    let ph = (h / 2).max(1);
    // Map preview R/G/B to source planes (the first three, clamped for <3-plane images), each
    // scaled by its own plane's levels.
    let chan = [0usize, 1.min(p - 1), 2.min(p - 1)];
    let scales = chan.map(|plane| plane_scale(raw, plane));

    let mut out = Vec::with_capacity(pw * ph * 3);
    for py in 0..ph {
        for px in 0..pw {
            for (&plane, &(black, range)) in chan.iter().zip(&scales) {
                let mut sum = 0u64;
                let mut count = 0u64;
                for dy in 0..2 {
                    let sy = py * 2 + dy;
                    if sy >= h {
                        break;
                    }
                    for dx in 0..2 {
                        let sx = px * 2 + dx;
                        if sx >= w {
                            break;
                        }
                        sum += u64::from(samples[(sy * w + sx) * p + plane]);
                        count += 1;
                    }
                }
                let avg = sum.checked_div(count).unwrap_or(0) as u32;
                out.push(scale8(avg, black, range));
            }
        }
    }
    Ok((Dimensions::new(pw as u32, ph as u32)?, out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cfa_preview_halves_a_bayer_tile() {
        let pattern = vec![
            cfa_color::RED,
            cfa_color::GREEN,
            cfa_color::GREEN,
            cfa_color::BLUE,
        ];
        let mut samples = vec![0u16; 16];
        for ty in 0..2usize {
            for tx in 0..2usize {
                for r in 0..2usize {
                    for c in 0..2usize {
                        let (y, x) = (ty * 2 + r, tx * 2 + c);
                        samples[y * 4 + x] = match pattern[r * 2 + c] {
                            cfa_color::RED => 200,
                            cfa_color::GREEN => 100,
                            _ => 50,
                        };
                    }
                }
            }
        }
        let raw =
            RawImage::new_cfa(Dimensions::new(4, 4).unwrap(), 8, (2, 2), pattern, samples).unwrap();
        let (dims, rgb) = raw_preview(&raw).unwrap();
        assert_eq!((dims.width(), dims.height()), (2, 2));
        assert_eq!(&rgb[0..3], &[200, 100, 50]);
        assert_eq!(rgb.len(), 2 * 2 * 3);
    }

    #[test]
    fn linear_preview_box_averages() {
        // 2x2 RGB image, white level 255; each pixel a flat colour, so the single preview pixel is
        // the average of the four.
        let samples = vec![
            100, 0, 0, 200, 0, 0, // row 0: two reds (100, 200)
            0, 60, 0, 0, 100, 0, // row 1: two greens
        ];
        let raw = RawImage::new_linear_raw(Dimensions::new(2, 2).unwrap(), 8, 3, samples).unwrap();
        let (dims, rgb) = raw_preview(&raw).unwrap();
        assert_eq!((dims.width(), dims.height()), (1, 1));
        // R = avg(100,200,0,0)=75, G = avg(0,0,60,100)=40, B = 0.
        assert_eq!(rgb, vec![75, 40, 0]);
    }

    /// Each 2x2 tile holds a distinct flat value, so the preview pixel for tile `(py, px)` reads that
    /// value — pinning the `sy = py*rr + ty` / `sx = px*rc + tx` source indexing (white level 255
    /// makes `scale8` the identity here).
    #[test]
    fn cfa_preview_indexes_each_tile_distinctly() {
        let pattern = vec![
            cfa_color::RED,
            cfa_color::GREEN,
            cfa_color::GREEN,
            cfa_color::BLUE,
        ];
        #[rustfmt::skip]
        let samples = vec![
            10, 10, 20, 20,
            10, 10, 20, 20,
            30, 30, 40, 40,
            30, 30, 40, 40,
        ];
        let raw =
            RawImage::new_cfa(Dimensions::new(4, 4).unwrap(), 8, (2, 2), pattern, samples).unwrap();
        let (dims, rgb) = raw_preview(&raw).unwrap();
        assert_eq!((dims.width(), dims.height()), (2, 2));
        // Tiles map to [v, v, v]; distinct values catch any misindexing of rows/cols.
        assert_eq!(rgb, vec![10, 10, 10, 20, 20, 20, 30, 30, 30, 40, 40, 40]);
    }

    /// A single-plane linear image with one distinct value per 2x2 block pins both the block indexing
    /// (`sy = py*2 + dy`, `sx = px*2 + dx`), the `width/2`/`height/2` downscale, and the
    /// `chan = [0, 1.min(p-1), 2.min(p-1)]` plane clamp (a wrong clamp reads a non-existent plane).
    #[test]
    fn linear_preview_single_plane_indexes_each_block() {
        #[rustfmt::skip]
        let samples = vec![
            10, 10, 20, 20,
            10, 10, 20, 20,
            30, 30, 40, 40,
            30, 30, 40, 40,
        ];
        let raw = RawImage::new_linear_raw(Dimensions::new(4, 4).unwrap(), 8, 1, samples).unwrap();
        let (dims, rgb) = raw_preview(&raw).unwrap();
        assert_eq!((dims.width(), dims.height()), (2, 2));
        assert_eq!(rgb, vec![10, 10, 10, 20, 20, 20, 30, 30, 30, 40, 40, 40]);
    }
}
