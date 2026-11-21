//! A small RGB preview/thumbnail for IFD 0, derived from the raw image.
//!
//! DNG stores a viewable preview in IFD 0 and the full-resolution raw in a sub-IFD. This produces a
//! simple, correct preview by collapsing each `2 × 2` block into one RGB pixel — for a CFA mosaic
//! that doubles as a trivial demosaic (averaging the sensels of each colour in the repeat tile),
//! for a linear image it box-averages the planes. An optional size cap ([`DngPreview`]) widens
//! that block by a whole factor until the preview fits. It is intentionally basic (no proper
//! demosaic or gamma); a JPEG-compressed preview is a later phase (see `STATUS.md`).

use gamut_core::{Dimensions, Error, Result};

use crate::raw::{RawImage, RawPhotometry, cfa_color};

/// The IFD 0 preview [`DngEncoder::with_preview`](crate::DngEncoder::with_preview) writes.
///
/// "DNG recommends, but does not require, that the first IFD contain a low-resolution thumbnail"
/// (DNG 1.7.1.0 Chapter 2, "SubIFD Trees"); without one the raw image itself is IFD 0. A JPEG-compressed preview is deferred (see `STATUS.md`). The size cap is reachable
/// through [`max_dimension`](Self::max_dimension).
///
/// `#[non_exhaustive]`: a preview kind the encoder learns later is added without a breaking
/// change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum DngPreview {
    /// No preview: IFD 0 is the full-resolution raw image (`NewSubFileType = 0`), carrying the
    /// DNG and profile tags itself, and the file has no `SubIFDs` raw child.
    None,
    /// An uncompressed 8-bit RGB preview in IFD 0, the raw image in its `SubIFDs` child. The
    /// preview has one pixel per CFA repeat tile (per `2 × 2` block for `LinearRaw`), so half
    /// resolution for a Bayer mosaic. With `max_dimension: Some(n)` that block is widened by the
    /// smallest whole factor that brings the preview's longer side to at most `n` pixels; `n`
    /// must be at least 1, checked at encode time. `None` applies no cap.
    Uncompressed {
        /// The largest preview width or height, in pixels; `None` for no cap.
        max_dimension: Option<u32>,
    },
}

/// An uncapped [`Uncompressed`](DngPreview::Uncompressed) preview, the one
/// [`DngEncoder::new`](crate::DngEncoder::new) writes.
impl Default for DngPreview {
    fn default() -> Self {
        Self::Uncompressed {
            max_dimension: None,
        }
    }
}

impl DngPreview {
    /// The size cap of an [`Uncompressed`](Self::Uncompressed) preview; `None` when it has no cap
    /// and for [`None`](Self::None).
    #[must_use]
    pub fn max_dimension(&self) -> Option<u32> {
        match self {
            Self::Uncompressed { max_dimension } => *max_dimension,
            Self::None => None,
        }
    }

    /// Rejects a zero size cap, which no preview of at least one pixel can meet.
    pub(crate) fn check(&self) -> Result<()> {
        if self.max_dimension() == Some(0) {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "DNG: a preview size cap must be at least 1 pixel",
            ));
        }
        Ok(())
    }
}

/// Builds an 8-bit RGB preview from `raw`, returning its dimensions and interleaved `RGB8` samples.
///
/// The uncapped preview is `floor(width / rc) × floor(height / rr)` pixels for a CFA repeat of
/// `rr × rc` (`floor(width / 2) × floor(height / 2)` for `LinearRaw`), at least `1 × 1`;
/// `max_dimension` shrinks it by [`block_factor`].
///
/// Fallible only because [`Dimensions::new`] is; the `1 × 1` floor means it never fails.
pub(crate) fn raw_preview(
    raw: &RawImage,
    max_dimension: Option<u32>,
) -> Result<(Dimensions, Vec<u8>)> {
    match raw.photometry() {
        RawPhotometry::Cfa {
            repeat, pattern, ..
        } => cfa_preview(raw, *repeat, pattern, max_dimension),
        RawPhotometry::LinearRaw { planes } => linear_preview(raw, *planes, max_dimension),
    }
}

/// The whole factor `k` by which a `width × height` uncapped preview is shrunk so its longer side
/// is at most `max_dimension`: `ceil(longer / max_dimension)`, and `1` without a cap (or when it
/// already fits). The shrunk preview is `floor(width / k) × floor(height / k)`, which is at most
/// `longer / k <= max_dimension` on each side. `max_dimension` is non-zero
/// ([`DngPreview::check`]).
fn block_factor(width: usize, height: usize, max_dimension: Option<u32>) -> usize {
    max_dimension.map_or(1, |cap| width.max(height).div_ceil(cap as usize))
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

/// Collapses each block of `k × k` CFA repeat tiles ([`block_factor`]) into one RGB pixel by
/// averaging each colour's sensels.
fn cfa_preview(
    raw: &RawImage,
    repeat: (u16, u16),
    pattern: &[u8],
    max_dimension: Option<u32>,
) -> Result<(Dimensions, Vec<u8>)> {
    let w = raw.dimensions().width() as usize;
    let h = raw.dimensions().height() as usize;
    let (rr, rc) = (repeat.0 as usize, repeat.1 as usize);
    let samples = raw.samples();
    let (tiles_across, tiles_down) = ((w / rc).max(1), (h / rr).max(1));
    let k = block_factor(tiles_across, tiles_down, max_dimension);
    let (pw, ph) = ((tiles_across / k).max(1), (tiles_down / k).max(1));
    // Each preview pixel covers a block of whole repeat tiles, so a sensel's colour is the
    // pattern entry at its offset within the block modulo the repeat.
    let (block_rows, block_cols) = (rr * k, rc * k);
    // One plane per mosaic sensel; the single-plane scale serves all preview channels.
    let (black, range) = plane_scale(raw, 0);

    let mut out = Vec::with_capacity(pw * ph * 3);
    for py in 0..ph {
        for px in 0..pw {
            let mut sum = [0u64; 3];
            let mut count = [0u64; 3];
            for ty in 0..block_rows {
                let sy = py * block_rows + ty;
                if sy >= h {
                    break;
                }
                for tx in 0..block_cols {
                    let sx = px * block_cols + tx;
                    if sx >= w {
                        break;
                    }
                    let channel = match pattern[ty % rr * rc + tx % rc] {
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

/// Box-averages each `2k × 2k` block of a linear image ([`block_factor`]) into one RGB pixel.
fn linear_preview(
    raw: &RawImage,
    planes: u16,
    max_dimension: Option<u32>,
) -> Result<(Dimensions, Vec<u8>)> {
    let w = raw.dimensions().width() as usize;
    let h = raw.dimensions().height() as usize;
    let p = usize::from(planes);
    let samples = raw.samples();
    let (half_w, half_h) = ((w / 2).max(1), (h / 2).max(1));
    let k = block_factor(half_w, half_h, max_dimension);
    let (pw, ph) = ((half_w / k).max(1), (half_h / k).max(1));
    let block = 2 * k;
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
                for dy in 0..block {
                    let sy = py * block + dy;
                    if sy >= h {
                        break;
                    }
                    for dx in 0..block {
                        let sx = px * block + dx;
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
        let (dims, rgb) = raw_preview(&raw, None).unwrap();
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
        let (dims, rgb) = raw_preview(&raw, None).unwrap();
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
        let (dims, rgb) = raw_preview(&raw, None).unwrap();
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
        let (dims, rgb) = raw_preview(&raw, None).unwrap();
        assert_eq!((dims.width(), dims.height()), (2, 2));
        assert_eq!(rgb, vec![10, 10, 10, 20, 20, 20, 30, 30, 30, 40, 40, 40]);
    }

    /// The factor is the ceiling of the longer side over the cap — exact division, one past it,
    /// and the shorter side's irrelevance — and 1 without a cap or when the preview already fits.
    #[test]
    fn block_factor_is_the_ceiling_of_the_longer_side_over_the_cap() {
        assert_eq!(block_factor(1000, 10, None), 1);
        assert_eq!(block_factor(100, 50, Some(100)), 1);
        assert_eq!(block_factor(100, 50, Some(50)), 2);
        assert_eq!(block_factor(101, 50, Some(50)), 3);
        assert_eq!(block_factor(50, 101, Some(50)), 3);
    }

    /// A zero cap is the only invalid preview; the accessor reads each variant's cap.
    #[test]
    fn a_zero_size_cap_is_rejected_and_the_cap_reads_back() {
        let capped = |n| DngPreview::Uncompressed {
            max_dimension: Some(n),
        };
        assert_eq!(capped(7).max_dimension(), Some(7));
        assert_eq!(DngPreview::default().max_dimension(), None);
        assert_eq!(DngPreview::None.max_dimension(), None);
        assert!(capped(1).check().is_ok());
        assert!(DngPreview::default().check().is_ok());
        assert!(DngPreview::None.check().is_ok());
        assert_eq!(
            capped(0).check().unwrap_err().static_message(),
            Some("DNG: a preview size cap must be at least 1 pixel")
        );
    }

    /// An 8×4 Bayer mosaic is 4×2 repeat tiles; a cap of 2 merges them two by two, so each
    /// preview pixel averages a 4×4 block of two tiles' worth of each colour. Distinct per-tile
    /// values pin the widened block's row and column indexing and the colour lookup modulo the
    /// repeat (red = 10·tile + 1, green = 10·tile + 2, blue = 10·tile + 3).
    #[test]
    fn capped_cfa_preview_averages_whole_tile_blocks() {
        let pattern = vec![
            cfa_color::RED,
            cfa_color::GREEN,
            cfa_color::GREEN,
            cfa_color::BLUE,
        ];
        let (w, h) = (8usize, 4usize);
        let mut samples = vec![0u16; w * h];
        for y in 0..h {
            for x in 0..w {
                let tile = (y / 2 * 4 + x / 2) as u16;
                let colour = match pattern[y % 2 * 2 + x % 2] {
                    cfa_color::RED => 1,
                    cfa_color::GREEN => 2,
                    _ => 3,
                };
                samples[y * w + x] = 10 * tile + colour;
            }
        }
        let raw = RawImage::new_cfa(
            Dimensions::new(w as u32, h as u32).unwrap(),
            8,
            (2, 2),
            pattern,
            samples,
        )
        .unwrap();
        let (dims, rgb) = raw_preview(&raw, Some(2)).unwrap();
        assert_eq!((dims.width(), dims.height()), (2, 1));
        // Block 0 holds tiles 0, 1, 4, 5 (mean tile 2.5); block 1 holds tiles 2, 3, 6, 7 (4.5).
        assert_eq!(rgb, vec![26, 27, 28, 46, 47, 48]);
    }

    /// A 6×2 single-plane linear image halves to 3×1; a cap of 2 needs factor 2, so the preview
    /// is 1×1 over the leading 4×2 block (the trailing columns a whole factor leaves out are
    /// dropped, as the uncapped preview drops an odd last column).
    #[test]
    fn capped_linear_preview_averages_a_widened_block() {
        #[rustfmt::skip]
        let samples = vec![
            10, 20, 30, 40, 200, 200,
            50, 60, 70, 80, 200, 200,
        ];
        let raw = RawImage::new_linear_raw(Dimensions::new(6, 2).unwrap(), 8, 1, samples).unwrap();
        let (dims, rgb) = raw_preview(&raw, Some(2)).unwrap();
        assert_eq!((dims.width(), dims.height()), (1, 1));
        assert_eq!(rgb, vec![45, 45, 45]);
    }
}
