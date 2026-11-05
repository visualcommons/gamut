//! VP8L transforms: reversible pixel-decorrelation passes applied before entropy coding and undone
//! in reverse order on decode (RFC 9649 §3.5).
//!
//! This module owns both the [`Vp8lTransform`] representation (the type + the per-transform data a
//! decoder reads) and the per-pixel math (predictors, color-transform deltas). Forward application
//! lands with the encoder (issue #22 commit series); the inverse application here is what the
//! decoder runs, last-read transform first.

use gamut_dsp::math::clip_pixel8;

use crate::vp8l::div_round_up;

/// Transform type code for the 2-bit on-the-wire tag: spatial predictor (RFC 9649 §4.1).
pub const PREDICTOR_TRANSFORM: u8 = 0;
/// Transform type code: color decorrelation (§4.2).
pub const COLOR_TRANSFORM: u8 = 1;
/// Transform type code: subtract-green (§4.3).
pub const SUBTRACT_GREEN_TRANSFORM: u8 = 2;
/// Transform type code: color indexing / palette (§4.4).
pub const COLOR_INDEXING_TRANSFORM: u8 = 3;

/// Number of spatial predictor modes (RFC 9649 §4.1).
pub const NUM_PREDICTOR_MODES: u8 = 14;

/// A VP8L transform together with the data needed to invert it. Up to four transforms may be
/// chained; on decode they are inverted in the reverse of the order they were read (RFC 9649 §3.5).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Vp8lTransform {
    /// Predictor (spatial) transform: each pixel is predicted from its neighbors using one of 14
    /// modes; `blocks` is the sub-resolution image whose green channel selects the mode per
    /// `2^size_bits` block.
    Predictor {
        /// Block-size exponent (`block edge = 1 << size_bits`).
        size_bits: u8,
        /// Sub-resolution mode image (one ARGB pixel per block; mode = green channel).
        blocks: Vec<u32>,
    },
    /// Color transform: decorrelates red/blue from green per block; `blocks` carries the
    /// `ColorTransformElement` per block.
    Color {
        /// Block-size exponent (`block edge = 1 << size_bits`).
        size_bits: u8,
        /// Sub-resolution element image (one ARGB pixel per block).
        blocks: Vec<u32>,
    },
    /// Subtract-green transform: green is subtracted from red and blue (no associated data).
    SubtractGreen,
    /// Color-indexing (palette) transform: pixels are indices into `table`; `expanded_width` is the
    /// image width before pixel-bundling reduced it.
    ColorIndexing {
        /// The color table (already subtraction-decoded).
        table: Vec<u32>,
        /// Image width before bundling (the width the inverse expands back to).
        expanded_width: u32,
    },
}

impl Vp8lTransform {
    /// Applies this transform's inverse to `pixels` (an ARGB image of `width` × `height`), returning
    /// the image width afterwards (unchanged except for color-indexing, which expands it).
    pub fn apply_inverse(&self, pixels: &mut Vec<u32>, width: u32, height: u32) -> u32 {
        match self {
            Self::SubtractGreen => {
                inverse_subtract_green(pixels);
                width
            }
            Self::Predictor { size_bits, blocks } => {
                inverse_predictor(pixels, width, height, *size_bits, blocks);
                width
            }
            Self::Color { size_bits, blocks } => {
                inverse_color(pixels, width, height, *size_bits, blocks);
                width
            }
            Self::ColorIndexing {
                table,
                expanded_width,
            } => {
                *pixels = inverse_color_indexing(pixels, width, height, *expanded_width, table);
                *expanded_width
            }
        }
    }
}

// --- ARGB channel helpers (shared with the decoder/encoder) ---------------------------------------

/// Extracts the alpha channel of an `0xAARRGGBB` pixel.
#[inline]
#[must_use]
pub const fn alpha(p: u32) -> u8 {
    (p >> 24) as u8
}
/// Extracts the red channel of an `0xAARRGGBB` pixel.
#[inline]
#[must_use]
pub const fn red(p: u32) -> u8 {
    (p >> 16) as u8
}
/// Extracts the green channel of an `0xAARRGGBB` pixel.
#[inline]
#[must_use]
pub const fn green(p: u32) -> u8 {
    (p >> 8) as u8
}
/// Extracts the blue channel of an `0xAARRGGBB` pixel.
#[inline]
#[must_use]
pub const fn blue(p: u32) -> u8 {
    p as u8
}
/// Packs four channels into an `0xAARRGGBB` pixel.
#[inline]
#[must_use]
pub const fn make_argb(a: u8, r: u8, g: u8, b: u8) -> u32 {
    ((a as u32) << 24) | ((r as u32) << 16) | ((g as u32) << 8) | (b as u32)
}

/// Adds two pixels channel-wise modulo 256 (used to subtraction-decode/encode the color table).
#[inline]
#[must_use]
pub const fn add_pixels(a: u32, b: u32) -> u32 {
    make_argb(
        alpha(a).wrapping_add(alpha(b)),
        red(a).wrapping_add(red(b)),
        green(a).wrapping_add(green(b)),
        blue(a).wrapping_add(blue(b)),
    )
}

/// Subtracts two pixels channel-wise modulo 256 (used to subtraction-encode the color table).
#[inline]
#[must_use]
pub const fn subtract_pixels(a: u32, b: u32) -> u32 {
    make_argb(
        alpha(a).wrapping_sub(alpha(b)),
        red(a).wrapping_sub(red(b)),
        green(a).wrapping_sub(green(b)),
        blue(a).wrapping_sub(blue(b)),
    )
}

// --- Subtract-green (RFC 9649 §4.3) ---------------------------------------------------------------

/// Inverse subtract-green: add the green value back into red and blue for every pixel.
pub fn inverse_subtract_green(pixels: &mut [u32]) {
    for p in pixels.iter_mut() {
        let g = green(*p);
        let r = red(*p).wrapping_add(g);
        let b = blue(*p).wrapping_add(g);
        *p = make_argb(alpha(*p), r, g, b);
    }
}

/// Forward subtract-green: subtract the green value from red and blue for every pixel.
pub fn forward_subtract_green(pixels: &mut [u32]) {
    for p in pixels.iter_mut() {
        let g = green(*p);
        let r = red(*p).wrapping_sub(g);
        let b = blue(*p).wrapping_sub(g);
        *p = make_argb(alpha(*p), r, g, b);
    }
}

// --- Predictor transform (RFC 9649 §4.1) ----------------------------------------------------------

#[inline]
fn average2(a: u32, b: u32) -> u32 {
    make_argb(
        ((u16::from(alpha(a)) + u16::from(alpha(b))) / 2) as u8,
        ((u16::from(red(a)) + u16::from(red(b))) / 2) as u8,
        ((u16::from(green(a)) + u16::from(green(b))) / 2) as u8,
        ((u16::from(blue(a)) + u16::from(blue(b))) / 2) as u8,
    )
}

/// The `Select` predictor: returns whichever of `l`/`t` is closer (Manhattan) to `l + t - tl`.
fn select(l: u32, t: u32, tl: u32) -> u32 {
    let pred = |cl: u8, ct: u8, ctl: u8| i32::from(cl) + i32::from(ct) - i32::from(ctl);
    let pa = pred(alpha(l), alpha(t), alpha(tl));
    let pr = pred(red(l), red(t), red(tl));
    let pg = pred(green(l), green(t), green(tl));
    let pb = pred(blue(l), blue(t), blue(tl));
    let dist = |p: u32, a: i32, r: i32, g: i32, b: i32| {
        (a - i32::from(alpha(p))).abs()
            + (r - i32::from(red(p))).abs()
            + (g - i32::from(green(p))).abs()
            + (b - i32::from(blue(p))).abs()
    };
    if dist(l, pa, pr, pg, pb) < dist(t, pa, pr, pg, pb) {
        l
    } else {
        t
    }
}

#[inline]
fn clamp_add_subtract_full(a: u32, b: u32, c: u32) -> u32 {
    let f = |ca: u8, cb: u8, cc: u8| clip_pixel8(i32::from(ca) + i32::from(cb) - i32::from(cc));
    make_argb(
        f(alpha(a), alpha(b), alpha(c)),
        f(red(a), red(b), red(c)),
        f(green(a), green(b), green(c)),
        f(blue(a), blue(b), blue(c)),
    )
}

#[inline]
fn clamp_add_subtract_half(a: u32, b: u32) -> u32 {
    let f = |ca: u8, cb: u8| {
        let ca = i32::from(ca);
        clip_pixel8(ca + (ca - i32::from(cb)) / 2)
    };
    make_argb(
        f(alpha(a), alpha(b)),
        f(red(a), red(b)),
        f(green(a), green(b)),
        f(blue(a), blue(b)),
    )
}

/// Predicts a pixel value from neighbors `l` (left), `t` (top), `tl` (top-left), `tr` (top-right)
/// under one of the 14 predictor modes (RFC 9649 §4.1). Modes out of range fall back to mode 0.
#[must_use]
pub fn predict(mode: u8, l: u32, t: u32, tl: u32, tr: u32) -> u32 {
    match mode {
        0 => 0xff00_0000,
        1 => l,
        2 => t,
        3 => tr,
        4 => tl,
        5 => average2(average2(l, tr), t),
        6 => average2(l, tl),
        7 => average2(l, t),
        8 => average2(tl, t),
        9 => average2(t, tr),
        10 => average2(average2(l, tl), average2(t, tr)),
        11 => select(l, t, tl),
        12 => clamp_add_subtract_full(l, t, tl),
        13 => clamp_add_subtract_half(average2(l, t), tl),
        _ => 0xff00_0000,
    }
}

/// Adds the per-channel residual `res` to a predicted pixel `pred` (mod 256 per channel).
#[inline]
fn add_residual(res: u32, pred: u32) -> u32 {
    make_argb(
        alpha(res).wrapping_add(alpha(pred)),
        red(res).wrapping_add(red(pred)),
        green(res).wrapping_add(green(pred)),
        blue(res).wrapping_add(blue(pred)),
    )
}

/// Subtracts a predicted pixel `pred` from a value `value` (mod 256 per channel) — the forward of
/// [`add_residual`].
#[inline]
fn sub_residual(value: u32, pred: u32) -> u32 {
    make_argb(
        alpha(value).wrapping_sub(alpha(pred)),
        red(value).wrapping_sub(red(pred)),
        green(value).wrapping_sub(green(pred)),
        blue(value).wrapping_sub(blue(pred)),
    )
}

/// The predicted value for pixel `(x, y)` from already-known neighbors in `img` (RFC 9649 §4.1),
/// applied identically by both the forward and inverse passes so they always agree. The border
/// rules (top-left, top row, left column, rightmost-column TR wrap) are handled here; `mode` is used
/// only for interior pixels.
#[inline]
fn predicted_value(img: &[u32], width: usize, x: usize, y: usize, mode: u8) -> u32 {
    let idx = y * width + x;
    if x == 0 && y == 0 {
        0xff00_0000
    } else if y == 0 {
        img[idx - 1] // top row: predict from left
    } else if x == 0 {
        img[idx - width] // left column: predict from top
    } else {
        let l = img[idx - 1];
        let t = img[idx - width];
        let tl = img[idx - width - 1];
        // TR is the pixel above-right, except on the rightmost column where it wraps to the first
        // pixel of the current row (the contiguous-buffer behavior, RFC 9649 §4.1).
        let tr = if x + 1 < width {
            img[idx - width + 1]
        } else {
            img[y * width]
        };
        predict(mode, l, t, tl, tr)
    }
}

/// Looks up the predictor mode for pixel `(x, y)` from the sub-resolution mode image.
#[inline]
fn block_mode(blocks: &[u32], transform_width: usize, size_bits: u8, x: usize, y: usize) -> u8 {
    let block = (y >> size_bits) * transform_width + (x >> size_bits);
    blocks.get(block).map_or(0, |&b| green(b))
}

/// Inverse predictor transform: reconstructs `pixels` (holding residuals) in scan order, adding back
/// each pixel's predicted value. `blocks` is the sub-resolution mode image (RFC 9649 §4.1).
pub fn inverse_predictor(
    pixels: &mut [u32],
    width: u32,
    height: u32,
    size_bits: u8,
    blocks: &[u32],
) {
    if width == 0 || height == 0 {
        return;
    }
    let w = width as usize;
    let transform_width = div_round_up(width, 1 << size_bits) as usize;
    for y in 0..height as usize {
        for x in 0..w {
            let mode = block_mode(blocks, transform_width, size_bits, x, y);
            let pred = predicted_value(pixels, w, x, y, mode);
            let idx = y * w + x;
            pixels[idx] = add_residual(pixels[idx], pred);
        }
    }
}

/// Forward predictor transform: chooses a per-block predictor mode and replaces each pixel with its
/// residual `pixel - prediction`. Returns `(residuals, mode_sub_image)`; the sub-image's green
/// channel carries the chosen mode per block (RFC 9649 §4.1). Mode selection minimizes the summed
/// residual magnitude — a simple heuristic; optimal selection is deferred to issue #31.
#[must_use]
pub fn forward_predictor(
    pixels: &[u32],
    width: u32,
    height: u32,
    size_bits: u8,
) -> (Vec<u32>, Vec<u32>) {
    let w = width as usize;
    let h = height as usize;
    let transform_width = div_round_up(width, 1 << size_bits) as usize;
    let transform_height = div_round_up(height, 1 << size_bits) as usize;

    let mut sub_image = vec![0xff00_0000u32; transform_width * transform_height];
    for by in 0..transform_height {
        for bx in 0..transform_width {
            let mode = best_predictor_mode(pixels, w, h, size_bits, bx, by);
            sub_image[by * transform_width + bx] = make_argb(0xff, 0, mode, 0);
        }
    }

    let mut residuals = vec![0u32; pixels.len()];
    for y in 0..h {
        for x in 0..w {
            let mode = block_mode(&sub_image, transform_width, size_bits, x, y);
            let pred = predicted_value(pixels, w, x, y, mode);
            let idx = y * w + x;
            residuals[idx] = sub_residual(pixels[idx], pred);
        }
    }
    (residuals, sub_image)
}

/// Channel-wise "distance from zero modulo 256" — a cheap proxy for residual entropy.
#[inline]
fn residual_cost(res: u32) -> u64 {
    let c = |v: u8| u64::from(v.min(v.wrapping_neg()));
    c(alpha(res)) + c(red(res)) + c(green(res)) + c(blue(res))
}

/// Picks the predictor mode that minimizes the summed residual magnitude over a block.
fn best_predictor_mode(
    pixels: &[u32],
    width: usize,
    height: usize,
    size_bits: u8,
    bx: usize,
    by: usize,
) -> u8 {
    let block_size = 1usize << size_bits;
    let x0 = bx * block_size;
    let y0 = by * block_size;
    let mut best_mode = 0u8;
    let mut best_cost = u64::MAX;
    for mode in 0..NUM_PREDICTOR_MODES {
        let mut cost = 0u64;
        for y in y0..(y0 + block_size).min(height) {
            for x in x0..(x0 + block_size).min(width) {
                let pred = predicted_value(pixels, width, x, y, mode);
                cost += residual_cost(sub_residual(pixels[y * width + x], pred));
            }
        }
        if cost < best_cost {
            best_cost = cost;
            best_mode = mode;
        }
    }
    best_mode
}

// --- Color transform (RFC 9649 §4.2) --------------------------------------------------------------

/// The color-transform delta: a 3.5 fixed-point multiply of a transform coefficient `t` and a signed
/// color channel `c`, keeping only the arithmetic-shifted product (RFC 9649 §4.2).
#[inline]
#[must_use]
pub fn color_transform_delta(t: i8, c: i8) -> i32 {
    (i32::from(t) * i32::from(c)) >> 5
}

/// Inverse color transform: adds the per-block color deltas back into red and blue. `blocks` carries
/// one `ColorTransformElement` per block, packed as `A=255, R=red_to_blue, G=green_to_blue,
/// B=green_to_red` (RFC 9649 §4.2).
pub fn inverse_color(pixels: &mut [u32], width: u32, height: u32, size_bits: u8, blocks: &[u32]) {
    if width == 0 || height == 0 {
        return;
    }
    let w = width as usize;
    let transform_width = div_round_up(width, 1 << size_bits) as usize;
    for y in 0..height as usize {
        for x in 0..w {
            let idx = y * w + x;
            let p = pixels[idx];
            let block = (y >> size_bits) * transform_width + (x >> size_bits);
            let cte = blocks.get(block).copied().unwrap_or(0xff00_0000);
            let green_to_red = blue(cte) as i8;
            let green_to_blue = green(cte) as i8;
            let red_to_blue = red(cte) as i8;
            let g = green(p);
            let mut tmp_red = i32::from(red(p));
            let mut tmp_blue = i32::from(blue(p));
            tmp_red += color_transform_delta(green_to_red, g as i8);
            tmp_blue += color_transform_delta(green_to_blue, g as i8);
            tmp_blue += color_transform_delta(red_to_blue, (tmp_red & 0xff) as i8);
            pixels[idx] = make_argb(alpha(p), (tmp_red & 0xff) as u8, g, (tmp_blue & 0xff) as u8);
        }
    }
}

/// Packs a `ColorTransformElement` into a sub-image pixel as the spec prescribes: `A=255,
/// R=red_to_blue, G=green_to_blue, B=green_to_red` (RFC 9649 §4.2).
#[inline]
#[must_use]
fn pack_color_element(green_to_red: i8, green_to_blue: i8, red_to_blue: i8) -> u32 {
    make_argb(
        0xff,
        red_to_blue as u8,
        green_to_blue as u8,
        green_to_red as u8,
    )
}

/// Forward color transform: subtracts the per-block color deltas from red and blue. Returns the
/// sub-resolution element image. The forward uses the **original** red for the `red_to_blue` term,
/// matching the inverse which adds it back using the reconstructed (== original) red (RFC 9649
/// §4.2). Per-block elements are estimated by simple least squares; optimal selection is deferred to
/// issue #31.
#[must_use]
pub fn forward_color(pixels: &mut [u32], width: u32, height: u32, size_bits: u8) -> Vec<u32> {
    let w = width as usize;
    let h = height as usize;
    let transform_width = div_round_up(width, 1 << size_bits) as usize;
    let transform_height = div_round_up(height, 1 << size_bits) as usize;
    let block_size = 1usize << size_bits;

    let mut sub_image = vec![pack_color_element(0, 0, 0); transform_width * transform_height];
    for by in 0..transform_height {
        for bx in 0..transform_width {
            let (g2r, g2b, r2b) = estimate_color_element(pixels, w, h, block_size, bx, by);
            sub_image[by * transform_width + bx] = pack_color_element(g2r, g2b, r2b);
            for y in (by * block_size)..((by + 1) * block_size).min(h) {
                for x in (bx * block_size)..((bx + 1) * block_size).min(w) {
                    let p = pixels[y * w + x];
                    let g = green(p) as i8;
                    let orig_red = red(p);
                    let new_red = (i32::from(orig_red) - color_transform_delta(g2r, g)) & 0xff;
                    let new_blue = (i32::from(blue(p))
                        - color_transform_delta(g2b, g)
                        - color_transform_delta(r2b, orig_red as i8))
                        & 0xff;
                    pixels[y * w + x] =
                        make_argb(alpha(p), new_red as u8, green(p), new_blue as u8);
                }
            }
        }
    }
    sub_image
}

/// Estimates the three color-transform coefficients for one block by least squares against the
/// channel pairs `(red, green)`, `(blue, green)`, and `(blue, red)`.
fn estimate_color_element(
    pixels: &[u32],
    width: usize,
    height: usize,
    block_size: usize,
    bx: usize,
    by: usize,
) -> (i8, i8, i8) {
    let mut rg = 0i64; // sum(red * green)
    let mut gg = 0i64; // sum(green * green)
    let mut bg = 0i64; // sum(blue * green)
    let mut br = 0i64; // sum(blue * red)
    let mut rr = 0i64; // sum(red * red)
    for y in (by * block_size)..((by + 1) * block_size).min(height) {
        for x in (bx * block_size)..((bx + 1) * block_size).min(width) {
            let p = pixels[y * width + x];
            let r = i64::from(red(p) as i8);
            let g = i64::from(green(p) as i8);
            let b = i64::from(blue(p) as i8);
            rg += r * g;
            gg += g * g;
            bg += b * g;
            br += b * r;
            rr += r * r;
        }
    }
    // delta(t, c) ≈ t * c / 32, so t ≈ 32 * sum(target * basis) / sum(basis^2).
    let coeff = |num: i64, den: i64| -> i8 {
        if den == 0 {
            0
        } else {
            ((num * 32) / den).clamp(-128, 127) as i8
        }
    };
    (coeff(rg, gg), coeff(bg, gg), coeff(br, rr))
}

// --- Color indexing (RFC 9649 §4.4) ---------------------------------------------------------------

/// The pixel-bundling exponent for a color table of `table_size` entries: how many pixels are packed
/// into one (RFC 9649 §4.4).
#[must_use]
pub fn color_index_width_bits(table_size: usize) -> u8 {
    if table_size <= 2 {
        3
    } else if table_size <= 4 {
        2
    } else if table_size <= 16 {
        1
    } else {
        0
    }
}

/// Forward color-indexing: maps each pixel to its `palette` index and packs the indices into the
/// green channel (pixel bundling), returning `(bundled_image, bundled_width)` (RFC 9649 §4.4).
/// Callers must guarantee every pixel appears in `palette`.
#[must_use]
pub fn forward_color_indexing(
    pixels: &[u32],
    width: u32,
    height: u32,
    palette: &[u32],
) -> (Vec<u32>, u32) {
    use std::collections::HashMap;
    let width_bits = color_index_width_bits(palette.len());
    let pixels_per = 1usize << width_bits;
    let bits_per = 8 / pixels_per as u32;
    let bundled_width = div_round_up(width, pixels_per as u32);
    let index_of: HashMap<u32, u32> = palette
        .iter()
        .enumerate()
        .map(|(i, &c)| (c, i as u32))
        .collect();
    let w = width as usize;
    let bw = bundled_width as usize;
    let mut bundled = vec![0xff00_0000u32; bw * height as usize];
    for y in 0..height as usize {
        for x in 0..w {
            let index = index_of.get(&pixels[y * w + x]).copied().unwrap_or(0);
            let pos = y * bw + x / pixels_per;
            let shift = (x % pixels_per) as u32 * bits_per;
            let packed = u32::from(green(bundled[pos])) | (index << shift);
            bundled[pos] = make_argb(0xff, 0, packed as u8, 0);
        }
    }
    (bundled, bundled_width)
}

/// Inverse color-indexing: expands the bundled index image (`bundled_width` × `height`) back to
/// `expanded_width` columns, replacing each green-channel index with its `table` color (or
/// transparent black when the index is out of range) (RFC 9649 §4.4).
#[must_use]
pub fn inverse_color_indexing(
    bundled: &[u32],
    bundled_width: u32,
    height: u32,
    expanded_width: u32,
    table: &[u32],
) -> Vec<u32> {
    let width_bits = color_index_width_bits(table.len());
    let bw = bundled_width as usize;
    let ew = expanded_width as usize;
    let mut out = vec![0u32; ew * height as usize];
    let pixels_per = 1usize << width_bits; // 1, 2, 4, or 8
    let bits_per = 8 / pixels_per; // 8, 4, 2, or 1
    let mask = (1u32 << bits_per) - 1;
    for y in 0..height as usize {
        for x in 0..ew {
            let index = if width_bits == 0 {
                u32::from(green(bundled[y * bw + x]))
            } else {
                let packed = u32::from(green(bundled[y * bw + x / pixels_per]));
                (packed >> ((x % pixels_per) * bits_per)) & mask
            };
            out[y * ew + x] = table.get(index as usize).copied().unwrap_or(0);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subtract_green_inverse_adds_green() {
        // green=10; red stored as (50-10)=40, blue stored as (200-10)=190 → reconstruct 50/200.
        let mut px = [make_argb(0xff, 40, 10, 190)];
        inverse_subtract_green(&mut px);
        assert_eq!(px[0], make_argb(0xff, 50, 10, 200));
        // wraps mod 256
        let mut wrap = [make_argb(0, 250, 10, 0)];
        inverse_subtract_green(&mut wrap);
        assert_eq!(red(wrap[0]), 250u8.wrapping_add(10));
    }

    #[test]
    fn predict_basic_modes() {
        let l = make_argb(1, 2, 3, 4);
        let t = make_argb(5, 6, 7, 8);
        let tl = make_argb(9, 10, 11, 12);
        let tr = make_argb(13, 14, 15, 16);
        assert_eq!(predict(0, l, t, tl, tr), 0xff00_0000);
        assert_eq!(predict(1, l, t, tl, tr), l);
        assert_eq!(predict(2, l, t, tl, tr), t);
        assert_eq!(predict(3, l, t, tl, tr), tr);
        assert_eq!(predict(4, l, t, tl, tr), tl);
        assert_eq!(predict(7, l, t, tl, tr), average2(l, t));
        // Out-of-range mode falls back to opaque black.
        assert_eq!(predict(99, l, t, tl, tr), 0xff00_0000);
    }

    #[test]
    fn predict_modes_pin_exact_values() {
        // Absolute outputs for every predictor mode (RFC 9649 §4.1) with fixed, channel-diverse
        // neighbours. predict and its helpers (average2 for modes 5-10, select for 11, and
        // clamp_add_subtract_full/half for 12-13) are shared by the forward and inverse passes, so a
        // symmetric round-trip — and the `== average2(...)` style checks above — cannot catch a
        // mutation in them; a position-weighted checksum over all 14 outputs does.
        let l = make_argb(10, 200, 30, 40);
        let t = make_argb(250, 60, 70, 200);
        let tl = make_argb(90, 100, 110, 120);
        let tr = make_argb(130, 20, 240, 16);
        let sum: i128 = (0..14u8)
            .map(|m| (i128::from(m) + 1) * i128::from(predict(m, l, t, tl, tr)))
            .sum();
        assert_eq!(sum, 227_728_720_966);
    }

    #[test]
    fn predicted_value_pins_borders_and_metric() {
        // predicted_value owns the border rules (top-left / top / left / rightmost-column TR-wrap) and
        // routes interior pixels through predict. Checking a small image at every (x, y) for a
        // top-right-using mode (3) and the Select mode (11) pins the TR-wrap arithmetic
        // `x + 1 < width` and Select's `a - b` distance metric — both shared by the forward and
        // inverse passes, so neither a symmetric round-trip nor the mode-only predict golden catches.
        let w = 4usize;
        let img: Vec<u32> = (0..16)
            .map(|i| {
                make_argb(
                    i as u8,
                    (i * 17) as u8,
                    (i * 7 + 3) as u8,
                    255u8.wrapping_sub((i * 11) as u8),
                )
            })
            .collect();
        let sum: i128 = (0..16usize)
            .flat_map(|i| [3u8, 11].into_iter().map(move |m| (i, m)))
            .map(|(i, m)| {
                let (x, y) = (i % w, i / w);
                (i as i128 + 1)
                    * (i128::from(m) + 1)
                    * i128::from(predicted_value(&img, w, x, y, m))
            })
            .sum();
        assert_eq!(sum, 381_797_375_768);
    }

    #[test]
    fn select_predictor_uses_manhattan_distance() {
        // Select returns whichever of l/t is closer (Manhattan) to the gradient l + t - tl. These
        // cases are crafted (equal non-alpha channels, so the alpha term decides) so the spec metric
        // `a - b` picks one neighbour while a mutated `a + b` / `a / b` would pick the other — pinning
        // select's per-channel distance arithmetic, which is shared by the forward and inverse passes.
        let g = |a: u8| make_argb(a, 128, 128, 128);
        assert_eq!(select(g(10), g(200), g(100)), g(200)); // `+` would pick g(10)
        assert_eq!(select(g(10), g(100), g(95)), g(10)); // `/` would pick g(100)
    }

    #[test]
    fn average2_rounds_down_per_channel() {
        let a = make_argb(10, 20, 30, 41);
        let b = make_argb(11, 21, 31, 40);
        assert_eq!(average2(a, b), make_argb(10, 20, 30, 40));
    }

    #[test]
    fn select_picks_closer_neighbor() {
        // When L equals the L+T-TL estimate exactly, Select returns L.
        let l = make_argb(100, 100, 100, 100);
        let t = make_argb(50, 50, 50, 50);
        let tl = make_argb(50, 50, 50, 50);
        // estimate = L + T - TL = L, so distance to L is 0 → returns L.
        assert_eq!(select(l, t, tl), l);
    }

    #[test]
    fn clamp_predictors_saturate() {
        let a = make_argb(200, 200, 200, 200);
        let b = make_argb(200, 200, 200, 200);
        let c = make_argb(0, 0, 0, 0);
        // 200 + 200 - 0 = 400 → clamps to 255 per channel.
        assert_eq!(
            clamp_add_subtract_full(a, b, c),
            make_argb(255, 255, 255, 255)
        );
        // half: 200 + (200-0)/2 = 300 → 255.
        assert_eq!(clamp_add_subtract_half(a, c), make_argb(255, 255, 255, 255));
    }

    #[test]
    fn color_transform_delta_sign_extends() {
        // t = -1 (0xff as i8), c = 64 → (-1 * 64) >> 5 = -2.
        assert_eq!(color_transform_delta(-1, 64), -2);
        assert_eq!(color_transform_delta(0, 100), 0);
        assert_eq!(color_transform_delta(32, 64), (32 * 64) >> 5);
    }

    /// A test-local forward predictor mirroring `inverse_predictor`, to validate the inverse end to
    /// end (scan order + every border rule) without depending on the encoder.
    fn forward_predictor_fixed_mode(
        orig: &[u32],
        width: usize,
        height: usize,
        mode: u8,
    ) -> Vec<u32> {
        let mut res = vec![0u32; orig.len()];
        for y in 0..height {
            for x in 0..width {
                let idx = y * width + x;
                let pred = if x == 0 && y == 0 {
                    0xff00_0000
                } else if y == 0 {
                    orig[idx - 1]
                } else if x == 0 {
                    orig[idx - width]
                } else {
                    let tr = if x + 1 < width {
                        orig[idx - width + 1]
                    } else {
                        orig[y * width]
                    };
                    predict(
                        mode,
                        orig[idx - 1],
                        orig[idx - width],
                        orig[idx - width - 1],
                        tr,
                    )
                };
                res[idx] = make_argb(
                    alpha(orig[idx]).wrapping_sub(alpha(pred)),
                    red(orig[idx]).wrapping_sub(red(pred)),
                    green(orig[idx]).wrapping_sub(green(pred)),
                    blue(orig[idx]).wrapping_sub(blue(pred)),
                );
            }
        }
        res
    }

    #[test]
    fn predictor_round_trips_every_mode_with_borders() {
        let (w, h) = (5usize, 4usize);
        let orig: Vec<u32> = (0..(w * h))
            .map(|i| {
                make_argb(
                    (i * 3) as u8,
                    (i * 7 + 1) as u8,
                    (i ^ 0x5a) as u8,
                    (i * 13) as u8,
                )
            })
            .collect();
        for mode in 0..NUM_PREDICTOR_MODES {
            // One block covering the whole image: size_bits large enough that transform_width == 1.
            let blocks = vec![make_argb(0xff, 0, mode, 0)];
            let mut res = forward_predictor_fixed_mode(&orig, w, h, mode);
            inverse_predictor(&mut res, w as u32, h as u32, 3, &blocks);
            assert_eq!(res, orig, "mode {mode} failed to round-trip");
        }
    }

    #[test]
    fn color_transform_round_trips() {
        let (w, h) = (3usize, 2usize);
        let orig: Vec<u32> = (0..(w * h))
            .map(|i| make_argb(0xff, (i * 17) as u8, (i * 5 + 3) as u8, (i * 29) as u8))
            .collect();
        let elem = make_argb(0xff, 7, 200, 30); // red_to_blue=7, green_to_blue=200, green_to_red=30
        let blocks = vec![elem];
        // Forward: subtract the same deltas the inverse adds.
        let mut transformed = orig.clone();
        for p in transformed.iter_mut() {
            let g = green(*p);
            let green_to_red = blue(elem) as i8;
            let green_to_blue = green(elem) as i8;
            let red_to_blue = red(elem) as i8;
            // Forward subtracts deltas; the red_to_blue term uses the ORIGINAL red (the inverse
            // adds it back using the reconstructed red, which equals the original).
            let orig_red = red(*p);
            let new_red =
                (i32::from(orig_red) - color_transform_delta(green_to_red, g as i8)) & 0xff;
            let new_blue = (i32::from(blue(*p))
                - color_transform_delta(green_to_blue, g as i8)
                - color_transform_delta(red_to_blue, orig_red as i8))
                & 0xff;
            *p = make_argb(alpha(*p), new_red as u8, g, new_blue as u8);
        }
        inverse_color(&mut transformed, w as u32, h as u32, 3, &blocks);
        assert_eq!(transformed, orig);
    }

    #[test]
    fn color_indexing_unbundles() {
        // width_bits = 0 (no bundling) needs a table larger than 16 entries.
        assert_eq!(color_index_width_bits(32), 0);
        let table: Vec<u32> = (0..32)
            .map(|i| make_argb(0xff, i as u8, i as u8, 0))
            .collect();
        let bundled: Vec<u32> = [0u8, 31, 1, 2]
            .iter()
            .map(|&i| make_argb(0xff, 0, i, 0))
            .collect();
        let out = inverse_color_indexing(&bundled, 4, 1, 4, &table);
        assert_eq!(out, vec![table[0], table[31], table[1], table[2]]);

        // Out-of-range index (>= table size) → transparent black.
        let oob = vec![make_argb(0xff, 0, 200, 0)];
        let out2 = inverse_color_indexing(&oob, 1, 1, 1, &table);
        assert_eq!(out2, vec![0]);
    }

    #[test]
    fn color_indexing_bundled_width_bits() {
        // 2-color table → width_bits = 3 → 8 indices packed per pixel (1 bit each), low bit first.
        assert_eq!(color_index_width_bits(2), 3);
        let table = vec![0xff00_0000, 0xffff_ffff];
        // packed green = 0b1010_1010 → indices for x=0..8: 0,1,0,1,0,1,0,1
        let bundled = vec![make_argb(0xff, 0, 0b1010_1010, 0)];
        let out = inverse_color_indexing(&bundled, 1, 1, 8, &table);
        let expected: Vec<u32> = (0..8)
            .map(|x| if x % 2 == 1 { table[1] } else { table[0] })
            .collect();
        assert_eq!(out, expected);

        // 16-color table → width_bits = 1 → 2 nibble indices per pixel.
        assert_eq!(color_index_width_bits(16), 1);
        let table16: Vec<u32> = (0..16).map(|i| make_argb(0xff, 0, 0, i as u8)).collect();
        // green = 0x53 → low nibble 3 (x=0), high nibble 5 (x=1)
        let bundled16 = vec![make_argb(0xff, 0, 0x53, 0)];
        let out16 = inverse_color_indexing(&bundled16, 1, 1, 2, &table16);
        assert_eq!(out16, vec![table16[3], table16[5]]);
    }

    #[test]
    fn forward_subtract_green_inverts() {
        let mut px: Vec<u32> = (0..20)
            .map(|i| make_argb(i as u8, (i * 3) as u8, (i * 7 + 1) as u8, (i * 11) as u8))
            .collect();
        let orig = px.clone();
        forward_subtract_green(&mut px);
        inverse_subtract_green(&mut px);
        assert_eq!(px, orig);
    }

    #[test]
    fn forward_predictor_inverts() {
        let (w, h) = (6u32, 4u32);
        let orig: Vec<u32> = (0..(w * h))
            .map(|i| make_argb(0xff, (i * 3) as u8, (i ^ 0x2a) as u8, (i * 5) as u8))
            .collect();
        let (residual, sub) = forward_predictor(&orig, w, h, 2);
        let mut recon = residual;
        inverse_predictor(&mut recon, w, h, 2, &sub);
        assert_eq!(recon, orig);
    }

    #[test]
    fn forward_color_inverts() {
        let (w, h) = (5u32, 3u32);
        let orig: Vec<u32> = (0..(w * h))
            .map(|i| make_argb(0xff, (i * 9) as u8, (i * 5 + 3) as u8, (i * 29) as u8))
            .collect();
        let mut px = orig.clone();
        let sub = forward_color(&mut px, w, h, 2);
        inverse_color(&mut px, w, h, 2, &sub);
        assert_eq!(px, orig);
    }

    #[test]
    fn forward_color_indexing_inverts() {
        // 3-color palette -> width_bits 2 -> 4 indices packed per pixel.
        let palette = vec![
            make_argb(0xff, 10, 20, 30),
            make_argb(0x80, 40, 50, 60),
            make_argb(0xff, 0, 0, 0),
        ];
        let (w, h) = (5u32, 2u32);
        let orig: Vec<u32> = (0..(w * h)).map(|i| palette[(i % 3) as usize]).collect();
        let (bundled, bundled_width) = forward_color_indexing(&orig, w, h, &palette);
        assert_eq!(bundled_width, 2); // div_round_up(5, 4)
        let restored = inverse_color_indexing(&bundled, bundled_width, h, w, &palette);
        assert_eq!(restored, orig);
    }

    #[test]
    fn forward_color_indexing_two_color_bundles_eight() {
        let palette = vec![make_argb(0xff, 1, 2, 3), make_argb(0xff, 9, 9, 9)];
        let (w, h) = (10u32, 1u32);
        let orig: Vec<u32> = (0..w).map(|i| palette[(i % 2) as usize]).collect();
        let (bundled, bundled_width) = forward_color_indexing(&orig, w, h, &palette);
        assert_eq!(bundled_width, 2); // div_round_up(10, 8)
        let restored = inverse_color_indexing(&bundled, bundled_width, h, w, &palette);
        assert_eq!(restored, orig);
    }

    #[test]
    fn apply_inverse_dispatches_and_expands_width() {
        let table = vec![0xff00_0000, 0xffaa_bbcc];
        let mut pixels = vec![make_argb(0xff, 0, 0b0000_0001, 0)]; // 8 bundled bits: 1,0,0,0,0,0,0,0
        let t = Vp8lTransform::ColorIndexing {
            table: table.clone(),
            expanded_width: 8,
        };
        let new_width = t.apply_inverse(&mut pixels, 1, 1);
        assert_eq!(new_width, 8);
        assert_eq!(pixels.len(), 8);
        assert_eq!(pixels[0], table[1]);
        assert_eq!(pixels[1], table[0]);
    }
}
