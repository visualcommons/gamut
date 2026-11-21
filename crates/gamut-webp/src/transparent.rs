//! Cleaning the colour under fully transparent pixels before coding — libwebp's `exact = 0`
//! (`WebPEncode` in `src/enc/webp_enc.c`), selected by [`Exact`](crate::Exact).
//!
//! The colour of an `alpha = 0` pixel is invisible once composited, yet the codestreams spend bits
//! on it all the same. Neither RFC 9649 nor RFC 6386 constrains it, so an encoder may rewrite it
//! freely; this module ports the two rewrites libwebp makes, one per codestream:
//!
//! - **Lossy** ([`cleanup_yuva`]): `WebPCleanupTransparentArea`'s YUV branch
//!   (`src/enc/picture_tools_enc.c`), on the 4:2:0 planes after the RGB→YUV conversion.
//! - **Lossless** ([`clear_transparent_argb`]): `WebPReplaceTransparentPixels(pic, 0x000000)`, on
//!   the ARGB pixels before VP8L coding.
//!
//! Neither ever touches a pixel whose alpha is non-zero. The lossy rewrite moves only luma and
//! chroma samples *under* transparency, but the VP8 transform and loop filter mix neighbouring
//! samples, so a lossy file's visible pixels can still decode slightly differently with and without
//! cleaning — as they do in libwebp. The lossless rewrite leaves every visible pixel bit-exact.

/// The side of libwebp's square cleanup block, in luma samples (`SIZE` in `picture_tools_enc.c`).
const BLOCK: usize = 8;

/// Replaces the luma of every transparent sample in the `w`×`h` block at (`x0`, `y0`) with the
/// truncated mean luma of the block's non-transparent samples — libwebp's `SmoothenBlock`. Returns
/// whether the whole block is transparent, in which case there is no mean and nothing moves.
///
/// libwebp also skips the rewrite when *no* sample is transparent; here that case runs the loop and
/// finds nothing to replace, which is the same result.
fn smoothen_block(
    alpha: &[u8],
    luma: &mut [u8],
    stride: usize,
    (x0, y0): (usize, usize),
    (w, h): (usize, usize),
) -> bool {
    let rows = || (y0..y0 + h).flat_map(|y| (x0..x0 + w).map(move |x| y * stride + x));
    let (sum, count) = rows()
        .filter(|&i| alpha[i] != 0)
        .fold((0u32, 0u32), |(sum, count), i| {
            (sum + u32::from(luma[i]), count + 1)
        });
    if count == 0 {
        return true;
    }
    // A mean of `u8` samples is itself in `0..=255`, so the narrowing cannot truncate.
    let mean = (sum / count) as u8;
    for i in rows().filter(|&i| alpha[i] == 0) {
        luma[i] = mean;
    }
    false
}

/// Fills the `size`×`size` square at (`x0`, `y0`) of a plane of row length `stride` with `value`
/// — libwebp's `Flatten`.
fn flatten(plane: &mut [u8], stride: usize, (x0, y0): (usize, usize), size: usize, value: u8) {
    for y in y0..y0 + size {
        plane[y * stride + x0..y * stride + x0 + size].fill(value);
    }
}

/// Cleans the colour under fully transparent pixels of a 4:2:0 picture in place — a port of
/// libwebp's `WebPCleanupTransparentArea` for a YUVA picture.
///
/// `planes` are `[Y, U, V]` at `width`×`height` and its ceiling-halved chroma size, and `alpha` is
/// the full-resolution alpha plane. The picture is walked in 8×8 luma blocks, row by row:
///
/// - a whole 8×8 block that is fully transparent is flattened — its luma and its 4×4 chroma squares
///   set to one value each. The value is the block's own top-left samples for the first such block
///   of a run, and is carried along the run of consecutive fully transparent blocks in that row, so
///   a transparent stretch codes as one flat area;
/// - every other block, including the partial blocks on the right and bottom edges, has the luma of
///   its transparent samples set to the mean luma of its visible ones (chroma untouched).
pub(crate) fn cleanup_yuva(planes: &mut [Vec<u8>; 3], alpha: &[u8], width: usize, height: usize) {
    let [y_plane, u_plane, v_plane] = planes;
    let uv_stride = width.div_ceil(2);
    for y0 in (0..height).step_by(BLOCK) {
        let h = BLOCK.min(height - y0);
        // The value a run of flattened blocks shares; a run never crosses a block row.
        let mut run: Option<[u8; 3]> = None;
        for x0 in (0..width).step_by(BLOCK) {
            let w = BLOCK.min(width - x0);
            let transparent = smoothen_block(alpha, y_plane, width, (x0, y0), (w, h));
            if !(transparent && w == BLOCK && h == BLOCK) {
                run = None;
                continue;
            }
            let (cx, cy) = (x0 / 2, y0 / 2);
            let [yv, uv, vv] = *run.get_or_insert([
                y_plane[y0 * width + x0],
                u_plane[cy * uv_stride + cx],
                v_plane[cy * uv_stride + cx],
            ]);
            flatten(y_plane, width, (x0, y0), BLOCK, yv);
            flatten(u_plane, uv_stride, (cx, cy), BLOCK / 2, uv);
            flatten(v_plane, uv_stride, (cx, cy), BLOCK / 2, vv);
        }
    }
}

/// Sets every fully transparent ARGB pixel to `0x0000_0000` — transparent black, libwebp's
/// `WebPReplaceTransparentPixels(pic, 0x000000)`. Pixels with non-zero alpha are left alone.
pub(crate) fn clear_transparent_argb(argb: &mut [u32]) {
    for px in argb.iter_mut().filter(|px| **px >> 24 == 0) {
        *px = 0;
    }
}

#[cfg(test)]
mod tests {
    use gamut_color::ChromaUpsampling;
    use gamut_core::{DecodeImage, Dimensions, EncodeImage, ImageBuf, ImageRef, Rgba8};
    use libwebp_oracle::libwebp_cleanup_transparent_area_yuva;

    use super::*;
    use crate::Exact;

    /// A deterministic 32-bit LCG (Numerical Recipes constants), so every run sees the same inputs.
    struct Lcg(u32);

    impl Lcg {
        fn next(&mut self) -> u32 {
            self.0 = self.0.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            self.0 >> 8
        }

        fn byte(&mut self) -> u8 {
            self.next() as u8
        }
    }

    /// An alpha plane built from 8×8 tiles that are each fully transparent, fully opaque, or mixed
    /// (a random mask of zeros and non-zero values), with the tile kind drawn per tile. Long
    /// transparent runs, runs broken by a visible tile, and runs reaching the right edge all occur.
    fn tiled_alpha(rng: &mut Lcg, width: usize, height: usize) -> Vec<u8> {
        let tiles_x = width.div_ceil(BLOCK);
        let kinds: Vec<u32> = (0..tiles_x * height.div_ceil(BLOCK))
            .map(|_| rng.next() % 4)
            .collect();
        let mut alpha = vec![0u8; width * height];
        for y in 0..height {
            for x in 0..width {
                alpha[y * width + x] = match kinds[(y / BLOCK) * tiles_x + x / BLOCK] {
                    0 | 1 => 0,
                    2 => 1 + rng.byte() % 255,
                    _ if rng.next() % 3 == 0 => 0,
                    _ => rng.byte(),
                };
            }
        }
        alpha
    }

    /// The cleanup is libwebp's, sample for sample: random noisy planes under tiled alpha masks,
    /// across sizes that are and are not multiples of the 8×8 block (so the right and bottom edge
    /// blocks are only smoothed, never flattened), match `WebPCleanupTransparentArea`.
    #[test]
    fn cleanup_matches_libwebp_cleanup_transparent_area() {
        let mut rng = Lcg(0x1207);
        for (width, height) in [
            (1, 1),
            (7, 9),
            (8, 8),
            (9, 8),
            (16, 16),
            (17, 9),
            (33, 20),
            (64, 48),
            (71, 40),
        ] {
            for _ in 0..4 {
                let alpha = tiled_alpha(&mut rng, width, height);
                let chroma = width.div_ceil(2) * height.div_ceil(2);
                let mut planes = [width * height, chroma, chroma]
                    .map(|len| (0..len).map(|_| rng.byte()).collect::<Vec<u8>>());
                let want = libwebp_cleanup_transparent_area_yuva(
                    [&planes[0], &planes[1], &planes[2]],
                    &alpha,
                    width as u32,
                    height as u32,
                );
                cleanup_yuva(&mut planes, &alpha, width, height);
                assert_eq!(planes[0], want.y, "Y at {width}x{height}");
                assert_eq!(planes[1], want.u, "U at {width}x{height}");
                assert_eq!(planes[2], want.v, "V at {width}x{height}");
            }
        }
    }

    /// A run of flattened blocks restarts on every block row: the second row's run takes its own
    /// top-left value rather than the one the row above ended on.
    #[test]
    fn a_flattened_run_restarts_on_each_block_row() {
        let (width, height) = (16, 16);
        let alpha = vec![0u8; width * height];
        let mut planes = [
            (0..width * height).map(|i| i as u8).collect::<Vec<u8>>(),
            vec![10; 64],
            vec![20; 64],
        ];
        cleanup_yuva(&mut planes, &alpha, width, height);
        assert!(planes[0][..8 * width].iter().all(|&s| s == 0));
        assert!(
            planes[0][8 * width..]
                .iter()
                .all(|&s| s == (8 * width) as u8)
        );
    }

    /// Only `alpha = 0` pixels move, and they become transparent black; alpha `1` with black colour
    /// bits, the closest non-transparent value, is kept.
    #[test]
    fn clear_transparent_argb_zeroes_only_fully_transparent_pixels() {
        let mut argb = [
            0x00ff_8040,
            0x0000_0001,
            0x0100_0000,
            0x01ff_ffff,
            0xff12_3456,
            0x8000_0000,
        ];
        clear_transparent_argb(&mut argb);
        assert_eq!(
            argb,
            [0, 0, 0x0100_0000, 0x01ff_ffff, 0xff12_3456, 0x8000_0000]
        );
    }

    /// Encodes `rgba` at `w`×`h` with `enc` and decodes it back with nearest chroma upsampling.
    fn round_trip(enc: &crate::WebpEncoder, rgba: &[u8], w: u32, h: u32) -> Vec<u8> {
        let dims = Dimensions::new(w, h).unwrap();
        let file = enc
            .encode_to_vec(ImageRef::<Rgba8>::new(rgba, dims).unwrap())
            .unwrap();
        let decoded: ImageBuf<Rgba8> = crate::WebpDecoder::new()
            .with_chroma_upsampling(ChromaUpsampling::Nearest)
            .decode_image(&file)
            .unwrap();
        decoded.as_samples().to_vec()
    }

    /// The encoder's lossless RGBA path clears the colour under `alpha = 0` exactly when
    /// [`Exact`](crate::Exact) resolves to clean for the mode: the transparent pixel decodes as
    /// transparent black then and as its source colour otherwise; the visible pixel round-trips
    /// either way.
    #[test]
    fn the_lossless_encoder_clears_transparent_colour_only_when_cleaning() {
        let rgba = [10u8, 20, 30, 0, 40, 50, 60, 255];
        let lossless = crate::WebpEncoder::lossless();
        assert_eq!(
            round_trip(&lossless, &rgba, 2, 1),
            rgba,
            "preserved by default"
        );
        let clean = lossless.with_exact(Exact::Clean);
        assert_eq!(
            round_trip(&clean, &rgba, 2, 1),
            [0, 0, 0, 0, 40, 50, 60, 255]
        );
    }

    /// The encoder's lossy RGBA path hands the cleanup its source alpha exactly when
    /// [`Exact`](crate::Exact) resolves to clean: on a frame whose only visible pixel sits in one
    /// corner, the cleaned frame decodes flat across a fully transparent macroblock, and the
    /// preserved frame carries the source noise there.
    #[test]
    fn the_lossy_encoder_cleans_under_transparency_only_when_cleaning() {
        let (w, h) = (32u32, 32u32);
        let mut rgba: Vec<u8> = (0..w * h)
            .flat_map(|i| [(i * 37) as u8, (i * 11) as u8, (i * 5) as u8, 0])
            .collect();
        rgba[3] = 0xff;
        let green_spread = |enc: crate::WebpEncoder| -> u8 {
            let px = &round_trip(&enc, &rgba, w, h);
            // The bottom-right 16×16 macroblock is four fully transparent 8×8 blocks.
            let tile = (16..32).flat_map(|r| (16..32).map(move |c| px[(r * 32 + c) * 4 + 1]));
            tile.clone().max().unwrap() - tile.min().unwrap()
        };
        let lossy = || crate::WebpEncoder::lossy(90);
        assert_eq!(green_spread(lossy()), 0, "cleaned by default");
        assert!(green_spread(lossy().with_exact(Exact::Preserve)) > 16);
    }
}
