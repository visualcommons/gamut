//! Planar 8-bit image buffers and the RGB → plane mappings: identity (`mc = 0`) and the CICP
//! luma–chroma matrices.

use gamut_core::{Error, Gray8, ImageRef, Result, Rgb8, Rgba8};

use crate::format::ChromaSubsampling;
use crate::ycbcr::RgbToYcbcr;

/// The pixel count `width * height` of a plane, which may be zero.
///
/// The one place the plane constructors turn a `u32` pair into a `usize` sample count: zero sides
/// are allowed here (unlike [`gamut_core::Dimensions`]), so only the overflow is an error.
pub(crate) fn pixel_count(width: u32, height: u32) -> Result<usize> {
    (width as usize)
        .checked_mul(height as usize)
        .ok_or_else(|| {
            Error::invalid_input(env!("CARGO_PKG_NAME"), "image dimensions overflow usize")
        })
}

/// Maps an interleaved buffer of `n` pixels, `stride` bytes apart with `R, G, B` first, to identity
/// GBR planes (`Y=G, U=B, V=R`).
///
/// `stride` is what lets RGB (3) and RGBA (4) share one mapping: the alpha byte of an RGBA source
/// is simply never read, so the colour planes are built without materializing an RGB copy first.
fn rgb_to_gbr_planes(px: &[u8], n: usize, stride: usize) -> [Vec<u8>; 3] {
    let mut g = vec![0u8; n];
    let mut b = vec![0u8; n];
    let mut r = vec![0u8; n];
    for i in 0..n {
        r[i] = px[i * stride];
        g[i] = px[i * stride + 1];
        b[i] = px[i * stride + 2];
    }
    [g, b, r]
}

/// Maps an interleaved buffer of `n` pixels, `stride` bytes apart with `R, G, B` first, to
/// `Y/Cb/Cr` planes through `matrix`. `stride` carries RGBA exactly as it does for
/// [`rgb_to_gbr_planes`].
fn rgb_to_ycbcr_planes(px: &[u8], n: usize, stride: usize, matrix: RgbToYcbcr) -> [Vec<u8>; 3] {
    let mut y = vec![0u8; n];
    let mut cb = vec![0u8; n];
    let mut cr = vec![0u8; n];
    for i in 0..n {
        let (yy, u, v) = matrix.from_rgb(
            u16::from(px[i * stride]),
            u16::from(px[i * stride + 1]),
            u16::from(px[i * stride + 2]),
        );
        // `matrix` is built at `BitDepth::Eight`, so every output is already in `0..=255`.
        y[i] = yy as u8;
        cb[i] = u as u8;
        cr[i] = v as u8;
    }
    [y, cb, cr]
}

/// A sample type [`downsample_box`] can average: widened to `u32` for the sum, narrowed back after.
///
/// Implemented for `u8` and `u16` so the 8-bit and high-bit-depth planar buffers share one
/// averaging filter rather than keeping a copy each. Narrowing is a plain truncating cast, which is
/// lossless here: a box average never exceeds the largest input sample.
pub(crate) trait BoxSample: Copy {
    /// Widens a sample for accumulation.
    fn widen(self) -> u32;
    /// Narrows an averaged sample back. `value` is always within the type's range.
    fn narrow(value: u32) -> Self;
}

impl BoxSample for u8 {
    fn widen(self) -> u32 {
        u32::from(self)
    }

    fn narrow(value: u32) -> Self {
        value as Self
    }
}

impl BoxSample for u16 {
    fn widen(self) -> u32 {
        u32::from(self)
    }

    fn narrow(value: u32) -> Self {
        value as Self
    }
}

/// Box-averages `plane` (`width` x `height`, row-major) down to `cw` x `ch` by `(sx, sy)`.
///
/// Partial edge boxes average only the samples that exist, which is edge replication. The filter is
/// the encoder's free choice — AV1 signals *where* the chroma sample sits
/// (`chroma_sample_position`), not how it was produced — and a symmetric box places it at the centre
/// of the luma group it covers. `gamut-jpeg` makes the same choice for the same reason with its own
/// private equivalent.
///
/// Generic over [`BoxSample`] so [`Planar8`] and [`Planar16`](crate::Planar16) average identically:
/// a 10/12-bit plane must not drift from the 8-bit filter that the `Rgb8` goldens pin.
pub(crate) fn downsample_box<T: BoxSample>(
    plane: &[T],
    width: usize,
    height: usize,
    cw: usize,
    ch: usize,
    sx: usize,
    sy: usize,
) -> Vec<T> {
    let mut out = vec![T::narrow(0); cw * ch];
    for cy in 0..ch {
        for cx in 0..cw {
            let (mut sum, mut count) = (0u32, 0u32);
            for dy in 0..sy {
                for dx in 0..sx {
                    let px = cx * sx + dx;
                    let py = cy * sy + dy;
                    if px < width && py < height {
                        sum += plane[py * width + px].widen();
                        count += 1;
                    }
                }
            }
            // `count` is at least one: `cx * sx < width` and `cy * sy < height` hold for every
            // output sample, because `cw`/`ch` are ceiling divisions of exactly those extents.
            out[cy * cw + cx] = T::narrow((sum + count / 2) / count);
        }
    }
    out
}

/// Three full-resolution (4:4:4) 8-bit planes, each `width * height` samples, row-major.
///
/// For identity matrix coefficients (CICP `mc = 0`) AV1 carries RGB directly with the plane order
/// **Y = G, U = B, V = R** ("GBR"); [`Planar8::from_rgb8_identity`] performs that mapping and
/// [`Planar8::to_rgb8_identity`] reverses it. Keeping the convention in one place means the
/// end-to-end round-trip (decode via `avifdec`) is the single source of truth for its correctness.
///
/// For any other matrix the planes are **Y, Cb, Cr** in AV1's `Y/U/V` order;
/// [`Planar8::from_rgb8_matrix`] applies the H.273 transform of [`RgbToYcbcr`].
///
/// The buffer carries its [`ChromaSubsampling`], so the chroma planes need not be full resolution.
/// Every RGB constructor produces [`Cs444`](ChromaSubsampling::Cs444); subsampled buffers are built
/// with [`from_planes_subsampled`](Self::from_planes_subsampled). The *format* is stored rather than
/// three plane sizes, so there is no inconsistent state to keep in sync —
/// [`plane_dimensions`](Self::plane_dimensions) derives each plane's geometry from
/// `(width, height, subsampling)`.
#[derive(Debug, Clone)]
pub struct Planar8 {
    width: u32,
    height: u32,
    subsampling: ChromaSubsampling,
    planes: [Vec<u8>; 3],
}

impl Planar8 {
    /// Maps an interleaved 8-bit RGB buffer to identity planes (`Y=G, U=B, V=R`).
    ///
    /// # Examples
    ///
    /// ```
    /// use gamut_color::Planar8;
    /// let rgb = [10, 20, 30, 40, 50, 60]; // two RGB pixels
    /// let planes = Planar8::from_rgb8_identity(&rgb, 2, 1).expect("valid length");
    /// assert_eq!(planes.plane(0), &[20u8, 50]); // Y carries G
    /// assert_eq!(planes.to_rgb8_identity(), rgb); // round-trips
    /// ```
    ///
    /// Zero dimensions are allowed (an empty buffer): rejecting empty images is the encoder's
    /// decision, made at its own boundary.
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if
    /// `rgb.len() != width * height * 3`, or if that product overflows `usize`.
    pub fn from_rgb8_identity(rgb: &[u8], width: u32, height: u32) -> Result<Self> {
        let n = pixel_count(width, height)?;
        if n.checked_mul(3) != Some(rgb.len()) {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "rgb buffer length != width * height * 3",
            ));
        }
        Ok(Self {
            width,
            height,
            subsampling: ChromaSubsampling::Cs444,
            planes: rgb_to_gbr_planes(rgb, n, 3),
        })
    }

    /// Like [`Planar8::from_rgb8_identity`] but takes a pre-validated [`ImageRef`], so it is
    /// infallible — the view already guarantees `rgb.len() == width * height * 3`. This is the
    /// boundary an encoder uses to turn a typed RGB image into AV1 identity planes.
    #[must_use]
    pub fn from_rgb8_identity_view(img: ImageRef<'_, Rgb8>) -> Self {
        let (width, height) = (img.width(), img.height());
        // No overflow check needed: `ImageRef` already validated that width * height * 3 fits
        // `usize` (it equals the sample slice's length).
        let n = width as usize * height as usize;
        Self {
            width,
            height,
            subsampling: ChromaSubsampling::Cs444,
            planes: rgb_to_gbr_planes(img.as_samples(), n, 3),
        }
    }

    /// Maps an interleaved 8-bit RGB buffer to 4:4:4 `Y/Cb/Cr` planes through `matrix`.
    ///
    /// The counterpart of [`Planar8::from_rgb8_identity`] for every CICP matrix that *does* apply a
    /// luma–chroma transform; build `matrix` once with
    /// [`RgbToYcbcr::new`](crate::RgbToYcbcr::new) at [`BitDepth::Eight`](crate::BitDepth::Eight),
    /// which is where an unsupported matrix is rejected.
    ///
    /// # Examples
    ///
    /// ```
    /// use gamut_color::{BitDepth, ColorRange, MatrixCoefficients, Planar8, RgbToYcbcr};
    ///
    /// let m = RgbToYcbcr::new(MatrixCoefficients::Bt709, ColorRange::Full, BitDepth::Eight)?;
    /// let rgb = [255, 255, 255, 0, 0, 0]; // white then black
    /// let planes = Planar8::from_rgb8_matrix(&rgb, 2, 1, m)?;
    /// assert_eq!(planes.plane(0), &[255u8, 0]); // luma
    /// assert_eq!(planes.plane(1), &[128u8, 128]); // neutral chroma
    /// # Ok::<(), gamut_core::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if
    /// `rgb.len() != width * height * 3`, or if that product overflows `usize`.
    pub fn from_rgb8_matrix(
        rgb: &[u8],
        width: u32,
        height: u32,
        matrix: RgbToYcbcr,
    ) -> Result<Self> {
        let n = pixel_count(width, height)?;
        if n.checked_mul(3) != Some(rgb.len()) {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "rgb buffer length != width * height * 3",
            ));
        }
        Ok(Self {
            width,
            height,
            subsampling: ChromaSubsampling::Cs444,
            planes: rgb_to_ycbcr_planes(rgb, n, 3, matrix),
        })
    }

    /// Like [`Planar8::from_rgb8_matrix`] but takes a pre-validated [`ImageRef`], so it is
    /// infallible — the view already guarantees `rgb.len() == width * height * 3`. This is the
    /// boundary an encoder uses to turn a typed RGB image into AV1 YCbCr planes.
    #[must_use]
    pub fn from_rgb8_matrix_view(img: ImageRef<'_, Rgb8>, matrix: RgbToYcbcr) -> Self {
        let (width, height) = (img.width(), img.height());
        // No overflow check needed: `ImageRef` already validated that width * height * 3 fits
        // `usize` (it equals the sample slice's length).
        let n = width as usize * height as usize;
        Self {
            width,
            height,
            subsampling: ChromaSubsampling::Cs444,
            planes: rgb_to_ycbcr_planes(img.as_samples(), n, 3, matrix),
        }
    }

    /// Maps the colour channels of an interleaved RGBA image to 4:4:4 identity GBR planes, ignoring
    /// alpha — the [`Planar8::from_rgb8_identity_view`] of a four-channel source.
    ///
    /// Alpha is a separate coded plane in every format that carries it planar (an AVIF alpha
    /// auxiliary item, a WebP `ALPH` chunk), so it is extracted on its own with
    /// [`from_rgba8_alpha_view`](Self::from_rgba8_alpha_view) rather than returned here. Reading
    /// the colour channels straight out of the RGBA buffer avoids materializing an intermediate
    /// RGB copy.
    #[must_use]
    pub fn from_rgba8_identity_view(img: ImageRef<'_, Rgba8>) -> Self {
        let (width, height) = (img.width(), img.height());
        // No overflow check needed: `ImageRef` already validated that width * height * 4 fits
        // `usize` (it equals the sample slice's length).
        let n = width as usize * height as usize;
        Self {
            width,
            height,
            subsampling: ChromaSubsampling::Cs444,
            planes: rgb_to_gbr_planes(img.as_samples(), n, 4),
        }
    }

    /// Maps the colour channels of an interleaved RGBA image to 4:4:4 `Y/Cb/Cr` planes through
    /// `matrix`, ignoring alpha — the [`Planar8::from_rgb8_matrix_view`] of a four-channel source.
    #[must_use]
    pub fn from_rgba8_matrix_view(img: ImageRef<'_, Rgba8>, matrix: RgbToYcbcr) -> Self {
        let (width, height) = (img.width(), img.height());
        let n = width as usize * height as usize;
        Self {
            width,
            height,
            subsampling: ChromaSubsampling::Cs444,
            planes: rgb_to_ycbcr_planes(img.as_samples(), n, 4, matrix),
        }
    }

    /// Extracts the **alpha** channel of an interleaved RGBA image as monochrome planes
    /// ([`ChromaSubsampling::Cs400`]): one luma plane carrying alpha verbatim, and no chroma.
    ///
    /// Alpha is opacity, not colour: it goes through no matrix and no range scaling, whatever the
    /// colour planes use. Monochrome is also what the formats require of it — AVIF v1.2.0 §4.1
    /// makes `mono_chrome = 1` and full range a *shall* for an AV1 alpha auxiliary item.
    #[must_use]
    pub fn from_rgba8_alpha_view(img: ImageRef<'_, Rgba8>) -> Self {
        let px = img.as_samples();
        Self {
            width: img.width(),
            height: img.height(),
            subsampling: ChromaSubsampling::Cs400,
            planes: [
                px.iter().skip(3).step_by(4).copied().collect(),
                Vec::new(),
                Vec::new(),
            ],
        }
    }

    /// Wraps an 8-bit grayscale image as **monochrome** planes ([`ChromaSubsampling::Cs400`]): one
    /// luma plane carrying the samples verbatim, and no chroma.
    ///
    /// Grayscale *is* the luma plane, so there is nothing for a matrix to decorrelate; encoding it
    /// as three equal planes would code two constant chroma planes for no information. Infallible
    /// for the same reason the other `_view` constructors are — the view already guarantees
    /// `len() == width * height`.
    #[must_use]
    pub fn from_gray8_view(img: ImageRef<'_, Gray8>) -> Self {
        Self {
            width: img.width(),
            height: img.height(),
            subsampling: ChromaSubsampling::Cs400,
            planes: [img.as_samples().to_vec(), Vec::new(), Vec::new()],
        }
    }

    /// Maps an interleaved 8-bit RGB image to `Y/Cb/Cr` planes through `matrix`, box-averaging the
    /// chroma planes down to `subsampling`.
    ///
    /// Luma keeps full resolution. The chroma planes are the [`ChromaSubsampling::chroma_dimensions`]
    /// of the image, so an odd axis keeps its half-covering edge sample and that sample averages
    /// only the pixels that exist.
    ///
    /// [`ChromaSubsampling::Cs444`] is the no-op case and produces the same planes as
    /// [`from_rgb8_matrix_view`](Self::from_rgb8_matrix_view). [`ChromaSubsampling::Cs400`] is not
    /// accepted here — a monochrome encode drops chroma rather than averaging it.
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) error for
    /// [`ChromaSubsampling::Cs400`].
    pub fn from_rgb8_matrix_subsampled(
        img: ImageRef<'_, Rgb8>,
        matrix: RgbToYcbcr,
        subsampling: ChromaSubsampling,
    ) -> Result<Self> {
        if subsampling == ChromaSubsampling::Cs400 {
            return Err(Error::unsupported(
                env!("CARGO_PKG_NAME"),
                "monochrome has no chroma planes to subsample",
            ));
        }
        let full = Self::from_rgb8_matrix_view(img, matrix);
        if subsampling == ChromaSubsampling::Cs444 {
            return Ok(full);
        }
        let (width, height) = (full.width as usize, full.height as usize);
        let (cw, ch) = subsampling.chroma_dimensions(full.width, full.height);
        let (sx, sy) = subsampling.subsampling();
        let (sx, sy) = (1usize << sx, 1usize << sy);
        let [y, u, v] = full.planes;
        let chroma = |p: &[u8]| downsample_box(p, width, height, cw as usize, ch as usize, sx, sy);
        Ok(Self {
            width: full.width,
            height: full.height,
            subsampling,
            planes: [y, chroma(&u), chroma(&v)],
        })
    }

    /// Builds a `Planar8` directly from three `width * height` planes (`Y/U/V`, already in the
    /// identity GBR order). Used by the encoder to wrap a horizontally-downscaled source for superres.
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if any
    /// plane's length is not `width * height`, or if that product overflows `usize`.
    pub fn from_planes(width: u32, height: u32, planes: [Vec<u8>; 3]) -> Result<Self> {
        Self::from_planes_subsampled(width, height, ChromaSubsampling::Cs444, planes)
    }

    /// Builds a `Planar8` from three planes whose chroma is subsampled by `subsampling`.
    ///
    /// Plane 0 must be `width * height` samples; planes 1 and 2 must each match
    /// [`ChromaSubsampling::chroma_dimensions`] for `(width, height)` — **ceiling** division on the
    /// subsampled axes, so a 5×3 image in 4:2:0 needs 3×2 chroma planes.
    /// [`ChromaSubsampling::Cs400`] has no chroma, so both chroma planes must be empty.
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if any
    /// plane's length does not match its own dimensions, or if `width * height` overflows `usize`.
    pub fn from_planes_subsampled(
        width: u32,
        height: u32,
        subsampling: ChromaSubsampling,
        planes: [Vec<u8>; 3],
    ) -> Result<Self> {
        let n = pixel_count(width, height)?;
        if planes[0].len() != n {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "luma plane length != width * height",
            ));
        }
        let (cw, ch) = subsampling.chroma_dimensions(width, height);
        // Checked per plane, not as a pair: a length mismatch confined to U or to V must not be
        // masked by the other being right.
        let chroma_len = cw as usize * ch as usize;
        if planes[1].len() != chroma_len {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "u plane length != chroma dimensions",
            ));
        }
        if planes[2].len() != chroma_len {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "v plane length != chroma dimensions",
            ));
        }
        Ok(Self {
            width,
            height,
            subsampling,
            planes,
        })
    }

    /// Reverses [`Planar8::from_rgb8_identity`], producing an interleaved 8-bit RGB buffer.
    ///
    /// For a subsampled buffer the chroma planes are expanded by **nearest-neighbour replication**
    /// (each output sample reads `plane[(y >> ss_y) * cw + (x >> ss_x)]`). Identity coefficients
    /// require 4:4:4 in a conformant AV1 stream (§6.4.2), so this path exists to keep the accessor
    /// total, not as a resampling filter. [`ChromaSubsampling::Cs400`] has no chroma planes and
    /// yields gray (`R = G = B = Y`).
    #[must_use]
    pub fn to_rgb8_identity(&self) -> Vec<u8> {
        let n = self.width as usize * self.height as usize;
        let (g, b, r) = (&self.planes[0], &self.planes[1], &self.planes[2]);
        let mut out = vec![0u8; n * 3];
        if self.subsampling == ChromaSubsampling::Cs444 {
            // Fast path: every plane shares the luma index, so this is a straight de-interleave.
            for i in 0..n {
                out[i * 3] = r[i];
                out[i * 3 + 1] = g[i];
                out[i * 3 + 2] = b[i];
            }
            return out;
        }
        if self.subsampling == ChromaSubsampling::Cs400 {
            for i in 0..n {
                out[i * 3] = g[i];
                out[i * 3 + 1] = g[i];
                out[i * 3 + 2] = g[i];
            }
            return out;
        }
        let (sx, sy) = self.subsampling.subsampling();
        let (cw, _) = self.subsampling.chroma_dimensions(self.width, self.height);
        let (w, cw) = (self.width as usize, cw as usize);
        for y in 0..self.height as usize {
            // In bounds without a runtime check: for `x < width`, `x >> sx` is at most
            // `(width - 1) >> sx`, which is strictly less than `ceil(width / (1 << sx))` = `cw`.
            // The same argument bounds the row against the chroma height.
            let crow = (y >> sy) * cw;
            for x in 0..w {
                let i = y * w + x;
                let ci = crow + (x >> sx);
                out[i * 3] = r[ci];
                out[i * 3 + 1] = g[i];
                out[i * 3 + 2] = b[ci];
            }
        }
        out
    }

    /// Image width in samples.
    #[must_use]
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Image height in samples.
    #[must_use]
    pub fn height(&self) -> u32 {
        self.height
    }

    /// The chroma subsampling of the coded planes.
    #[must_use]
    pub fn subsampling(&self) -> ChromaSubsampling {
        self.subsampling
    }

    /// The dimensions of plane `index`: `(width, height)` for luma, and
    /// [`ChromaSubsampling::chroma_dimensions`] for the two chroma planes.
    ///
    /// # Panics
    ///
    /// Panics if `index >= 3`, matching [`plane`](Self::plane).
    #[must_use]
    pub fn plane_dimensions(&self, index: usize) -> (u32, u32) {
        match index {
            0 => (self.width, self.height),
            1 | 2 => self.subsampling.chroma_dimensions(self.width, self.height),
            _ => panic!("plane index {index} out of range (0..3)"),
        }
    }

    /// The row-major samples of plane `index` (`0 = Y/G, 1 = U/B, 2 = V/R`).
    ///
    /// The slice is `w * h` samples for the plane's own
    /// [`plane_dimensions`](Self::plane_dimensions), which for a subsampled buffer is smaller than
    /// the luma plane.
    ///
    /// # Panics
    ///
    /// Panics if `index >= 3`, like slice indexing — an out-of-range plane index is a
    /// programmer error, not a data error.
    #[must_use]
    pub fn plane(&self, index: usize) -> &[u8] {
        &self.planes[index]
    }
}

#[cfg(test)]
mod tests {
    use gamut_core::Dimensions;

    use super::*;

    #[test]
    fn rgb_identity_round_trips_with_gbr_order() {
        // One pixel (R=10, G=20, B=30) pins the plane order directly: Y=G=20, U=B=30, V=R=10. The
        // round-trip alone is order-agnostic (a consistent from/to swap would still pass), and the
        // GBR convention is the module's whole point.
        let p = Planar8::from_rgb8_identity(&[10, 20, 30], 1, 1).unwrap();
        assert_eq!(p.plane(0), &[20]);
        assert_eq!(p.plane(1), &[30]);
        assert_eq!(p.plane(2), &[10]);

        let rgb: Vec<u8> = (0..=200u8).cycle().take(2 * 3 * 3).collect(); // 3x2 image
        let p = Planar8::from_rgb8_identity(&rgb, 3, 2).unwrap();
        assert_eq!(p.width(), 3);
        assert_eq!(p.height(), 2);
        assert_eq!(p.to_rgb8_identity(), rgb);
    }

    #[test]
    fn wrong_length_errors() {
        assert!(Planar8::from_rgb8_identity(&[0, 1, 2, 3], 1, 1).is_err());
    }

    #[test]
    fn from_planes_validates_and_wraps() {
        // 3x2 ⇒ n = 6. Three distinct planes, each length 6.
        let g: Vec<u8> = (0..6).collect();
        let b: Vec<u8> = (10..16).collect();
        let r: Vec<u8> = (20..26).collect();
        let p = Planar8::from_planes(3, 2, [g.clone(), b.clone(), r.clone()]).unwrap();
        assert_eq!((p.width(), p.height()), (3, 2));
        assert_eq!(p.plane(0), &g[..]);
        assert_eq!(p.plane(1), &b[..]);
        assert_eq!(p.plane(2), &r[..]);
        // The valid case above only passes when `n == 3 * 2`: a mutated `width * height` (3 + 2 = 5,
        // 3 / 2 = 1) or an inverted `!=` length check would reject these correctly-sized planes.
        assert!(Planar8::from_planes(3, 2, [vec![0; 6], vec![0; 6], vec![0; 5]]).is_err());
        assert!(Planar8::from_planes(3, 2, [vec![0; 5], vec![0; 6], vec![0; 6]]).is_err());
    }

    #[test]
    fn rejects_overflowing_dimensions() {
        // Near-max dimensions must yield Err, not an overflow panic (debug) or a wrapped length
        // check (32-bit release): width * height * 3 exceeds usize even on 64-bit targets.
        assert!(Planar8::from_rgb8_identity(&[], u32::MAX, u32::MAX).is_err());
        assert!(Planar8::from_planes(u32::MAX, u32::MAX, [vec![], vec![], vec![]]).is_err());
    }

    #[test]
    fn matrix_ctor_writes_y_cb_cr_in_plane_order() {
        use crate::cicp::{ColorRange, MatrixCoefficients};
        use crate::format::BitDepth;
        use crate::ycbcr::RgbToYcbcr;

        // Pure red under BT.709 full range: Y = 54, Cb = 99, Cr = 255 (pinned in `ycbcr`).
        // Asserting per plane pins the Y/Cb/Cr order — the opposite of the identity path's GBR.
        let m =
            RgbToYcbcr::new(MatrixCoefficients::Bt709, ColorRange::Full, BitDepth::Eight).unwrap();
        let p = Planar8::from_rgb8_matrix(&[255, 0, 0], 1, 1, m).unwrap();
        assert_eq!(
            (p.plane(0), p.plane(1), p.plane(2)),
            (&[54u8][..], &[99u8][..], &[255u8][..])
        );

        // The view constructor must agree sample for sample. Saturated, strongly-coloured pixels,
        // so the luma plane cannot coincidentally equal the green channel the identity path uses.
        let rgb: Vec<u8> = vec![
            255, 0, 0, 0, 255, 0, 0, 0, 255, 10, 200, 30, 250, 5, 60, 128, 128, 128,
        ];
        let from_slice = Planar8::from_rgb8_matrix(&rgb, 3, 2, m).unwrap();
        let view = ImageRef::<Rgb8>::new(&rgb, Dimensions::new(3, 2).unwrap()).unwrap();
        let from_view = Planar8::from_rgb8_matrix_view(view, m);
        for i in 0..3 {
            assert_eq!(from_view.plane(i), from_slice.plane(i));
        }
        // …and the transform really ran: the planes are not the identity mapping.
        let identity = Planar8::from_rgb8_identity(&rgb, 3, 2).unwrap();
        assert_ne!(from_slice.plane(0), identity.plane(0));

        assert!(Planar8::from_rgb8_matrix(&[0, 1, 2, 3], 1, 1, m).is_err());
        assert!(Planar8::from_rgb8_matrix(&[], u32::MAX, u32::MAX, m).is_err());
    }

    #[test]
    fn view_ctor_matches_slice_ctor() {
        let rgb: Vec<u8> = (0..=200u8).cycle().take(3 * 2 * 3).collect(); // 3x2 image
        let from_slice = Planar8::from_rgb8_identity(&rgb, 3, 2).unwrap();
        let view = ImageRef::<Rgb8>::new(&rgb, gamut_core::Dimensions::new(3, 2).unwrap()).unwrap();
        let from_view = Planar8::from_rgb8_identity_view(view);
        assert_eq!((from_view.width(), from_view.height()), (3, 2));
        for i in 0..3 {
            assert_eq!(from_view.plane(i), from_slice.plane(i));
        }
    }

    #[test]
    fn plane_dimensions_follow_the_subsampling() {
        // 4:4:4 from an RGB constructor: every plane is full resolution.
        let p = Planar8::from_rgb8_identity(&[0; 5 * 3 * 3], 5, 3).unwrap();
        assert_eq!(p.subsampling(), ChromaSubsampling::Cs444);
        for i in 0..3 {
            assert_eq!(p.plane_dimensions(i), (5, 3));
        }

        // Odd dimensions are the interesting case: 5x3 in 4:2:0 has 3x2 chroma (ceiling), and in
        // 4:2:2 has 3x3. A mutant that floors either axis produces 2x1 / 2x3 and dies here.
        let p420 = Planar8::from_planes_subsampled(
            5,
            3,
            ChromaSubsampling::Cs420,
            [vec![0; 15], vec![0; 6], vec![0; 6]],
        )
        .unwrap();
        assert_eq!(p420.plane_dimensions(0), (5, 3));
        assert_eq!(p420.plane_dimensions(1), (3, 2));
        assert_eq!(p420.plane_dimensions(2), (3, 2));

        let p422 = Planar8::from_planes_subsampled(
            5,
            3,
            ChromaSubsampling::Cs422,
            [vec![0; 15], vec![0; 9], vec![0; 9]],
        )
        .unwrap();
        assert_eq!(p422.plane_dimensions(1), (3, 3));
    }

    #[test]
    fn from_planes_subsampled_rejects_a_wrong_luma_plane() {
        // 5x3 in 4:2:0 ⇒ luma 15, chroma 3x2 = 6 each.
        let err = Planar8::from_planes_subsampled(
            5,
            3,
            ChromaSubsampling::Cs420,
            [vec![0; 14], vec![0; 6], vec![0; 6]],
        )
        .expect_err("short luma plane");
        // Asserting the specific diagnostic, not `is_err()`: the chroma checks below would reject a
        // great many wrong inputs too, so only the message distinguishes this guard from them.
        assert_eq!(
            err.static_message(),
            Some("luma plane length != width * height")
        );
    }

    #[test]
    fn from_planes_subsampled_rejects_a_wrong_u_plane() {
        let err = Planar8::from_planes_subsampled(
            5,
            3,
            ChromaSubsampling::Cs420,
            [vec![0; 15], vec![0; 15], vec![0; 6]],
        )
        .expect_err("full-resolution u plane");
        assert_eq!(
            err.static_message(),
            Some("u plane length != chroma dimensions")
        );
    }

    #[test]
    fn from_planes_subsampled_rejects_a_wrong_v_plane() {
        // Separate from the U case on purpose: a guard that validated only `planes[1]` would pass
        // the test above and fail only here.
        let err = Planar8::from_planes_subsampled(
            5,
            3,
            ChromaSubsampling::Cs420,
            [vec![0; 15], vec![0; 6], vec![0; 15]],
        )
        .expect_err("full-resolution v plane");
        assert_eq!(
            err.static_message(),
            Some("v plane length != chroma dimensions")
        );
    }

    #[test]
    fn from_planes_is_the_four_four_four_case_of_from_planes_subsampled() {
        let planes = [vec![1u8; 6], vec![2; 6], vec![3; 6]];
        let a = Planar8::from_planes(3, 2, planes.clone()).unwrap();
        let b = Planar8::from_planes_subsampled(3, 2, ChromaSubsampling::Cs444, planes).unwrap();
        assert_eq!(a.subsampling(), ChromaSubsampling::Cs444);
        assert_eq!(a.subsampling(), b.subsampling());
        for i in 0..3 {
            assert_eq!(a.plane(i), b.plane(i));
            assert_eq!(a.plane_dimensions(i), b.plane_dimensions(i));
        }
    }

    #[test]
    fn to_rgb8_identity_replicates_subsampled_chroma() {
        // 3x3 in 4:2:0 ⇒ 2x2 chroma. Distinct chroma values per quadrant pin the replication map
        // and, at the odd right column and bottom row, the edge clamp implied by ceiling division.
        let luma: Vec<u8> = (0..9).collect();
        let u = vec![10u8, 20, 30, 40];
        let v = vec![50u8, 60, 70, 80];
        let p =
            Planar8::from_planes_subsampled(3, 3, ChromaSubsampling::Cs420, [luma, u, v]).unwrap();
        let rgb = p.to_rgb8_identity();
        // Identity order is R=V, G=Y, B=U. Chroma index is (y >> 1) * 2 + (x >> 1).
        let at = |x: usize, y: usize| {
            let i = (y * 3 + x) * 3;
            (rgb[i], rgb[i + 1], rgb[i + 2])
        };
        assert_eq!(at(0, 0), (50, 0, 10));
        assert_eq!(at(1, 0), (50, 1, 10)); // same chroma sample as (0,0)
        assert_eq!(at(2, 0), (60, 2, 20)); // odd right column reads chroma column 1
        assert_eq!(at(0, 2), (70, 6, 30)); // odd bottom row reads chroma row 1
        assert_eq!(at(2, 2), (80, 8, 40));
    }

    #[test]
    fn to_rgb8_identity_is_unchanged_on_the_four_four_four_fast_path() {
        // The general path must agree with the fast path wherever both are defined, so a 4:4:4
        // buffer built through `from_planes_subsampled` round-trips identically.
        let rgb: Vec<u8> = (0..(4 * 3 * 3) as u8).collect();
        let p = Planar8::from_rgb8_identity(&rgb, 4, 3).unwrap();
        assert_eq!(p.to_rgb8_identity(), rgb);
    }

    #[test]
    fn to_rgb8_identity_of_monochrome_is_gray() {
        let p = Planar8::from_planes_subsampled(
            2,
            2,
            ChromaSubsampling::Cs400,
            [vec![7, 8, 9, 10], Vec::new(), Vec::new()],
        )
        .unwrap();
        assert_eq!(
            p.to_rgb8_identity(),
            vec![7, 7, 7, 8, 8, 8, 9, 9, 9, 10, 10, 10]
        );
    }

    /// A 3x2 RGBA image whose four channels are pairwise distinct at every pixel, so a mapping
    /// that read the wrong channel or the wrong stride cannot produce the expected planes.
    ///
    /// 3x2 rather than a square: the pixel count is `width * height`, and for 2x2 that is
    /// indistinguishable from `width + height`.
    fn rgba_3x2() -> Vec<u8> {
        vec![
            10, 20, 30, 40, // (0,0)
            11, 21, 31, 41, // (1,0)
            12, 22, 32, 42, // (2,0)
            13, 23, 33, 43, // (0,1)
            14, 24, 34, 44, // (1,1)
            15, 25, 35, 45, // (2,1)
        ]
    }

    #[test]
    fn rgba_colour_planes_ignore_alpha_and_match_the_rgb_mapping() {
        let px = rgba_3x2();
        let img = ImageRef::<Rgba8>::new(&px, Dimensions::new(3, 2).unwrap()).unwrap();
        let p = Planar8::from_rgba8_identity_view(img);
        // GBR order, exactly as the three-channel constructor produces: Y=G, U=B, V=R. Every plane
        // is the full 6 samples, which is what fails if the pixel count is computed wrongly.
        assert_eq!(p.plane(0), &[20u8, 21, 22, 23, 24, 25]);
        assert_eq!(p.plane(1), &[30u8, 31, 32, 33, 34, 35]);
        assert_eq!(p.plane(2), &[10u8, 11, 12, 13, 14, 15]);
        assert_eq!(p.subsampling(), ChromaSubsampling::Cs444);

        // The same colour values fed through the three-channel path give the same planes, which is
        // the property that makes an RGBA colour item byte-identical to the RGB one.
        let rgb: Vec<u8> = px
            .as_chunks::<4>()
            .0
            .iter()
            .flat_map(|c| c[..3].to_vec())
            .collect();
        let matrix = RgbToYcbcr::new(
            crate::MatrixCoefficients::Bt709,
            crate::ColorRange::Full,
            crate::BitDepth::Eight,
        )
        .unwrap();
        let from_rgba = Planar8::from_rgba8_matrix_view(img, matrix);
        let from_rgb = Planar8::from_rgb8_matrix(&rgb, 3, 2, matrix).unwrap();
        for i in 0..3 {
            assert_eq!(from_rgba.plane(i), from_rgb.plane(i), "plane {i}");
            assert_eq!(from_rgba.plane(i).len(), 6, "plane {i} covers every pixel");
        }
    }

    #[test]
    fn alpha_and_gray_views_are_monochrome() {
        let px = rgba_3x2();
        let img = ImageRef::<Rgba8>::new(&px, Dimensions::new(3, 2).unwrap()).unwrap();
        let a = Planar8::from_rgba8_alpha_view(img);
        assert_eq!(a.subsampling(), ChromaSubsampling::Cs400);
        // The fourth channel of each pixel, in raster order — not the first, and not every fourth
        // *byte* from the start.
        assert_eq!(a.plane(0), &[40u8, 41, 42, 43, 44, 45]);
        assert!(a.plane(1).is_empty() && a.plane(2).is_empty());
        assert_eq!(a.plane_dimensions(0), (3, 2));
        assert_eq!(a.plane_dimensions(1), (0, 0));

        let gray = [5u8, 6, 7, 8, 9, 10];
        let g = Planar8::from_gray8_view(
            ImageRef::<Gray8>::new(&gray, Dimensions::new(3, 2).unwrap()).unwrap(),
        );
        assert_eq!(g.subsampling(), ChromaSubsampling::Cs400);
        assert_eq!((g.width(), g.height()), (3, 2));
        assert_eq!(g.plane(0), &gray[..]);
        assert!(g.plane(1).is_empty() && g.plane(2).is_empty());
        // Grayscale is luma, so expanding it back gives R=G=B.
        assert_eq!(g.to_rgb8_identity()[..6], [5, 5, 5, 6, 6, 6]);
    }

    #[test]
    #[should_panic(expected = "plane index 3 out of range")]
    fn plane_dimensions_rejects_an_out_of_range_index() {
        let p = Planar8::from_rgb8_identity(&[0; 3], 1, 1).unwrap();
        let _ = p.plane_dimensions(3);
    }

    #[test]
    fn downsample_box_averages_each_group_with_rounding() {
        // 4x4 ramp: value = y * 4 + x.
        let plane: Vec<u8> = (0..16u8).collect();
        // 2x2 boxes: each output is the rounded mean of its four inputs.
        // (0,1,4,5) -> 10/4 = 2.5 -> 3;  (2,3,6,7) -> 18/4 = 4.5 -> 5
        // (8,9,12,13) -> 42/4 = 10.5 -> 11; (10,11,14,15) -> 50/4 = 12.5 -> 13
        assert_eq!(downsample_box(&plane, 4, 4, 2, 2, 2, 2), vec![3, 5, 11, 13]);
        // Horizontal only (4:2:2): pairs across x. (0,1) -> 0.5 -> 1, (2,3) -> 2.5 -> 3, ...
        assert_eq!(
            downsample_box(&plane, 4, 4, 2, 4, 2, 1),
            vec![1, 3, 5, 7, 9, 11, 13, 15]
        );
        // 1x1 is an exact copy.
        assert_eq!(downsample_box(&plane, 4, 4, 4, 4, 1, 1), plane);
    }

    #[test]
    fn downsample_box_partial_edge_group_averages_only_real_samples() {
        // 3x3, so the right column and bottom row have half-width / half-height boxes. Chroma is
        // 2x2. Values: 0..8 row-major.
        //   box (0,0) = mean(0,1,3,4) = 2
        //   box (1,0) = mean(2,5)     = 3.5 -> 4   (only two samples exist)
        //   box (0,1) = mean(6,7)     = 6.5 -> 7
        //   box (1,1) = mean(8)       = 8
        // A `count` fixed at sx*sy instead of the number of real samples would give 1/1/3/2.
        let plane: Vec<u8> = (0..9u8).collect();
        assert_eq!(downsample_box(&plane, 3, 3, 2, 2, 2, 2), vec![2, 4, 7, 8]);
    }

    #[test]
    fn downsample_box_rounds_half_away_from_zero() {
        // Two samples averaging exactly .5 must round up, which is what the `+ count / 2` addend
        // does; truncation would give 10 and 12.
        let plane = vec![10u8, 11, 12, 13];
        assert_eq!(downsample_box(&plane, 4, 1, 2, 1, 2, 1), vec![11, 13]);
    }

    #[test]
    fn matrix_subsampled_halves_only_the_subsampled_axes() {
        // A vertical stripe at period 2: the 4:2:2 and 4:2:0 box averages both collapse it in x,
        // while 4:4:4 keeps it. Distinguishes an x/y transposition, which a square image and a
        // symmetric pattern cannot.
        let (w, h) = (4u32, 2u32);
        let mut rgb = vec![0u8; (w * h * 3) as usize];
        for y in 0..h {
            for x in 0..w {
                let i = ((y * w + x) * 3) as usize;
                // Coloured, not grey: a black/white stripe has neutral chroma either way, so it
                // could not tell an averaged plane from an un-averaged one.
                let px = if x % 2 == 0 {
                    [255u8, 0, 0]
                } else {
                    [0, 0, 255]
                };
                rgb[i..i + 3].copy_from_slice(&px);
            }
        }
        let img = ImageRef::<Rgb8>::new(&rgb, Dimensions::new(w, h).unwrap()).unwrap();
        let m = RgbToYcbcr::new(
            crate::cicp::MatrixCoefficients::Bt709,
            crate::cicp::ColorRange::Full,
            crate::BitDepth::Eight,
        )
        .unwrap();

        let p444 = Planar8::from_rgb8_matrix_subsampled(img, m, ChromaSubsampling::Cs444).unwrap();
        assert_eq!(p444.subsampling(), ChromaSubsampling::Cs444);
        assert_eq!(p444.plane_dimensions(1), (4, 2));

        let p422 = Planar8::from_rgb8_matrix_subsampled(img, m, ChromaSubsampling::Cs422).unwrap();
        assert_eq!(p422.plane_dimensions(1), (2, 2));
        assert_eq!(p422.plane(0).len(), 8, "luma keeps full resolution");

        let p420 = Planar8::from_rgb8_matrix_subsampled(img, m, ChromaSubsampling::Cs420).unwrap();
        assert_eq!(p420.plane_dimensions(1), (2, 1));

        // Averaging a red/blue pair gives the same chroma in both subsampled layouts (both halve
        // x, and the stripe is constant in y), and it differs from the un-averaged red sample.
        assert_eq!(p422.plane(1)[0], p420.plane(1)[0]);
        assert_ne!(p444.plane(1)[0], p422.plane(1)[0]);
        // Luma is untouched by subsampling, so the first sample is the same in all three.
        assert_eq!(p444.plane(0)[0], p422.plane(0)[0]);
        assert_eq!(p444.plane(0)[0], p420.plane(0)[0]);
    }

    #[test]
    fn matrix_subsampled_rejects_monochrome() {
        let rgb = [0u8; 3];
        let img = ImageRef::<Rgb8>::new(&rgb, Dimensions::new(1, 1).unwrap()).unwrap();
        let m = RgbToYcbcr::new(
            crate::cicp::MatrixCoefficients::Bt709,
            crate::cicp::ColorRange::Full,
            crate::BitDepth::Eight,
        )
        .unwrap();
        let err = Planar8::from_rgb8_matrix_subsampled(img, m, ChromaSubsampling::Cs400)
            .expect_err("monochrome has no chroma to subsample");
        assert_eq!(
            err.static_message(),
            Some("monochrome has no chroma planes to subsample")
        );
    }
}
