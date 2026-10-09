//! Branded, length-validated interleaved image buffers: [`ImageRef`] (borrowed) and [`ImageBuf`]
//! (owned).
//!
//! Both wrap a flat sample slice/vec of `P::Sample` (`u8` or `u16`) plus [`Dimensions`], branded
//! with a [`Pixel`] marker `P`. The invariant `len == width * height * P::CHANNELS` (with non-zero
//! dimensions) is checked **once**, at construction, so codecs receive a buffer that is already
//! known-good and can pull the raw slice back out via [`ImageRef::as_samples`] with zero overhead —
//! their hot loops are byte-identical to operating on a bare `&[u8]`. The brand is what makes a
//! format mismatch (e.g. feeding [`crate::Cmyk8`] data to an [`crate::Rgba8`] encoder) a compile
//! error instead of a silent reinterpretation.

use core::marker::PhantomData;

use crate::convert::RawImage;
use crate::{Dimensions, Error, Pixel, Result};

/// The required sample count for `dims` at `P`'s channel count, rejecting empty or overflowing
/// dimensions. The single validation shared by every buffer constructor.
fn expected_len<P: Pixel>(dims: Dimensions) -> Result<usize> {
    if dims.is_empty() {
        return Err(Error::InvalidInput("zero-sized image").with_origin(env!("CARGO_PKG_NAME")));
    }
    dims.sample_count(P::CHANNELS).ok_or_else(|| {
        Error::InvalidInput("image dimensions overflow usize").with_origin(env!("CARGO_PKG_NAME"))
    })
}

/// Checks the length invariant `data_len == width * height * P::CHANNELS` (with non-empty,
/// non-overflowing `dims`). The single source of truth both buffer constructors call, so the
/// rejection messages never drift between the borrowed and owned paths.
fn validate_len<P: Pixel>(data_len: usize, dims: Dimensions) -> Result<()> {
    if data_len != expected_len::<P>(dims)? {
        return Err(
            Error::InvalidInput("image buffer length does not match dimensions")
                .with_origin(env!("CARGO_PKG_NAME")),
        );
    }
    Ok(())
}

/// A borrowed, length-validated view of an interleaved image of pixel type `P`.
///
/// Cheap to copy (a slice + [`Dimensions`] + a zero-sized marker). Construct one at an API boundary
/// with [`ImageRef::new`]; pass it to an [`crate::EncodeImage`] implementation.
#[derive(Debug, Clone, Copy)]
#[must_use]
pub struct ImageRef<'a, P: Pixel> {
    data: &'a [P::Sample],
    dims: Dimensions,
    _p: PhantomData<P>,
}

impl<'a, P: Pixel> ImageRef<'a, P> {
    /// Brands `data` as an image of `dims`, validating that its length is exactly
    /// `width * height * P::CHANNELS`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidInput`] if `dims` is zero-sized, if `width * height * channels`
    /// overflows `usize`, or if `data.len()` does not equal that product.
    pub fn new(data: &'a [P::Sample], dims: Dimensions) -> Result<Self> {
        validate_len::<P>(data.len(), dims)?;
        Ok(Self {
            data,
            dims,
            _p: PhantomData,
        })
    }

    /// The image dimensions.
    #[must_use]
    pub fn dimensions(self) -> Dimensions {
        self.dims
    }

    /// Image width in pixels.
    #[must_use]
    pub fn width(self) -> u32 {
        self.dims.width
    }

    /// Image height in pixels.
    #[must_use]
    pub fn height(self) -> u32 {
        self.dims.height
    }

    /// The raw interleaved samples (`width * height * P::CHANNELS` of them, row-major). The
    /// zero-cost escape hatch a codec uses to feed its existing slice-based hot path.
    #[must_use]
    pub fn as_samples(self) -> &'a [P::Sample] {
        self.data
    }

    /// Row `y` as a `width * P::CHANNELS`-sample slice.
    ///
    /// # Panics
    ///
    /// Panics if `y >= height`.
    #[must_use]
    pub fn row(self, y: u32) -> &'a [P::Sample] {
        let row_len = self.dims.width as usize * P::CHANNELS;
        let start = y as usize * row_len;
        &self.data[start..start + row_len]
    }

    /// Iterates the rows top to bottom, each a `width * P::CHANNELS`-sample slice.
    pub fn rows(self) -> impl ExactSizeIterator<Item = &'a [P::Sample]> {
        let row_len = self.dims.width as usize * P::CHANNELS;
        self.data.chunks_exact(row_len)
    }

    /// The `P::CHANNELS` samples of the pixel at `(x, y)`.
    ///
    /// # Panics
    ///
    /// Panics if `x >= width` or `y >= height`.
    #[must_use]
    pub fn pixel(self, x: u32, y: u32) -> &'a [P::Sample] {
        let i = (y as usize * self.dims.width as usize + x as usize) * P::CHANNELS;
        &self.data[i..i + P::CHANNELS]
    }

    /// Re-describes this view with `P`'s layout carried as a runtime [`crate::PixelFormat`] tag
    /// instead of a brand, for [`crate::convert`]'s raw entry points.
    ///
    /// Infallible: the brand guarantees everything [`RawImage::new`] would otherwise check.
    pub fn as_raw(self) -> RawImage<'a, P::Sample> {
        RawImage::from_branded(self.data, P::FORMAT, self.dims)
    }
}

/// Two views are equal when they have the same dimensions and the same samples. Hand-written rather
/// than derived so the brand `P` carries no `PartialEq` bound (only the samples and dimensions are
/// compared). Comparing dimensions first short-circuits the sample comparison on a size mismatch.
impl<P: Pixel> PartialEq for ImageRef<'_, P> {
    fn eq(&self, other: &Self) -> bool {
        self.dims == other.dims && self.data == other.data
    }
}

impl<P: Pixel> Eq for ImageRef<'_, P> {}

/// An owned, length-validated interleaved image of pixel type `P`.
///
/// The owning counterpart of [`ImageRef`]; the natural return of a decoder, carrying its
/// dimensions, samples, and layout brand as one unit so a caller can never misinterpret the result.
#[derive(Debug, Clone)]
#[must_use]
pub struct ImageBuf<P: Pixel> {
    data: Vec<P::Sample>,
    dims: Dimensions,
    _p: PhantomData<P>,
}

impl<P: Pixel> ImageBuf<P> {
    /// Takes ownership of `data` as an image of `dims`, validating its length the same way as
    /// [`ImageRef::new`].
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidInput`] if `dims` is zero-sized, if the sample count overflows
    /// `usize`, or if `data.len()` does not equal `width * height * P::CHANNELS`.
    pub fn new(data: Vec<P::Sample>, dims: Dimensions) -> Result<Self> {
        // Same length invariant as `ImageRef::new`, without building a throwaway borrowed view.
        validate_len::<P>(data.len(), dims)?;
        Ok(Self {
            data,
            dims,
            _p: PhantomData,
        })
    }

    /// An all-zero image of `dims` (every sample `P::Sample::default()`).
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidInput`] if `dims` is zero-sized or the sample count overflows `usize`.
    pub fn zeroed(dims: Dimensions) -> Result<Self> {
        let want = expected_len::<P>(dims)?;
        Ok(Self {
            data: vec![P::Sample::default(); want],
            dims,
            _p: PhantomData,
        })
    }

    /// Borrows this image as an [`ImageRef`]. Infallible — the invariant already holds.
    pub fn as_ref(&self) -> ImageRef<'_, P> {
        ImageRef {
            data: &self.data,
            dims: self.dims,
            _p: PhantomData,
        }
    }

    /// The image dimensions.
    #[must_use]
    pub fn dimensions(&self) -> Dimensions {
        self.dims
    }

    /// Image width in pixels.
    #[must_use]
    pub fn width(&self) -> u32 {
        self.dims.width
    }

    /// Image height in pixels.
    #[must_use]
    pub fn height(&self) -> u32 {
        self.dims.height
    }

    /// The raw interleaved samples, row-major.
    #[must_use]
    pub fn as_samples(&self) -> &[P::Sample] {
        &self.data
    }

    /// The raw interleaved samples as a mutable slice, row-major.
    ///
    /// Lets a caller edit pixels in place — post-process a decoded image, or refill an existing
    /// buffer (see [`DecodeImage::decode_image_into`](crate::DecodeImage::decode_image_into)) —
    /// without the [`ImageBuf::into_samples`] + [`ImageBuf::new`] round-trip. The slice length is
    /// fixed at `width * height * P::CHANNELS`, so the length invariant is preserved: values may
    /// change, the count cannot.
    #[must_use]
    pub fn as_mut_samples(&mut self) -> &mut [P::Sample] {
        &mut self.data
    }

    /// Consumes the image, returning its backing sample vector.
    #[must_use]
    pub fn into_samples(self) -> Vec<P::Sample> {
        self.data
    }
}

/// Two images are equal when they have the same dimensions and the same samples. Hand-written
/// rather than derived so the brand `P` carries no `PartialEq` bound (only the samples and
/// dimensions are compared). Comparing dimensions first short-circuits the sample comparison on a
/// size mismatch.
impl<P: Pixel> PartialEq for ImageBuf<P> {
    fn eq(&self, other: &Self) -> bool {
        self.dims == other.dims && self.data == other.data
    }
}

impl<P: Pixel> Eq for ImageBuf<P> {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Gray8, Rgb8, Rgb16};

    fn dims(w: u32, h: u32) -> Dimensions {
        Dimensions {
            width: w,
            height: h,
        }
    }

    #[test]
    fn new_validates_length() {
        let rgb = vec![0u8; 2 * 2 * 3];
        let img = ImageRef::<Rgb8>::new(&rgb, dims(2, 2)).unwrap();
        assert_eq!(img.dimensions(), dims(2, 2));
        assert_eq!((img.width(), img.height()), (2, 2));
        assert_eq!(img.as_samples().len(), 12);
        // Too short and too long both rejected.
        assert!(ImageRef::<Rgb8>::new(&rgb[..11], dims(2, 2)).is_err());
        let long = vec![0u8; 13];
        assert!(ImageRef::<Rgb8>::new(&long, dims(2, 2)).is_err());
    }

    #[test]
    fn new_rejects_zero_sized() {
        let empty: [u8; 0] = [];
        assert!(ImageRef::<Rgb8>::new(&empty, dims(0, 4)).is_err());
        assert!(ImageRef::<Rgb8>::new(&empty, dims(4, 0)).is_err());
    }

    #[test]
    fn rejects_overflowing_dimensions() {
        // width*height fits usize on 64-bit but *3 channels overflows; on 32-bit width*height
        // already overflows. Either way expected_len returns the overflow error.
        let big = dims(u32::MAX, u32::MAX);
        assert!(ImageRef::<Rgb8>::new(&[], big).is_err());
        assert!(ImageBuf::<Rgb8>::zeroed(big).is_err());
    }

    #[test]
    fn row_and_pixel_access() {
        // 3x2 RGB, pixel (x,y) tagged so we can spot misindexing.
        let mut rgb = vec![0u8; 3 * 2 * 3];
        for y in 0..2u32 {
            for x in 0..3u32 {
                let i = (y as usize * 3 + x as usize) * 3;
                rgb[i] = x as u8;
                rgb[i + 1] = y as u8;
                rgb[i + 2] = 0xAA;
            }
        }
        let img = ImageRef::<Rgb8>::new(&rgb, dims(3, 2)).unwrap();
        // Assert row *contents*, not just length: `row(y)` length is `row_len` regardless of the
        // start offset, so a mutated offset (e.g. `y * row_len` → `y / row_len`, which aliases
        // row 0) is caught only by checking the actual samples.
        assert_eq!(img.row(0), &[0, 0, 0xAA, 1, 0, 0xAA, 2, 0, 0xAA]);
        assert_eq!(img.row(1), &[0, 1, 0xAA, 1, 1, 0xAA, 2, 1, 0xAA]);
        assert_eq!(img.pixel(2, 1), &[2, 1, 0xAA]);
        assert_eq!(img.pixel(0, 0), &[0, 0, 0xAA]);
        // `rows()` must yield the same per-row samples in order (not merely two slices of length 9).
        let rows: Vec<_> = img.rows().collect();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0], img.row(0));
        assert_eq!(rows[1], img.row(1));
    }

    #[test]
    fn high_bit_depth_samples() {
        // 2x1 RGB16 = 2 pixels * 3 channels = 6 u16 samples.
        let rgb16 = vec![1000u16; 6];
        let img = ImageRef::<Rgb16>::new(&rgb16, dims(2, 1)).unwrap();
        assert_eq!(img.as_samples().len(), 6);
        assert_eq!(img.pixel(1, 0), &[1000, 1000, 1000]);
        // A u16 buffer of the wrong length is still rejected.
        assert!(ImageRef::<Rgb16>::new(&[0u16; 5], dims(2, 1)).is_err());
    }

    #[test]
    fn owned_buffer_roundtrips() {
        let buf = ImageBuf::<Rgb8>::new(vec![7u8; 12], dims(2, 2)).unwrap();
        assert_eq!(buf.dimensions(), dims(2, 2));
        assert_eq!(buf.as_samples().len(), 12);
        assert_eq!(buf.as_ref().pixel(0, 0), &[7, 7, 7]);
        assert_eq!(buf.into_samples(), vec![7u8; 12]);
        // Wrong length rejected on the owned path too.
        assert!(ImageBuf::<Rgb8>::new(vec![0u8; 11], dims(2, 2)).is_err());
    }

    #[test]
    fn equality_compares_both_dimensions_and_samples() {
        let a = ImageBuf::<Rgb8>::new(vec![1, 2, 3, 4, 5, 6], dims(2, 1)).unwrap();
        let b = ImageBuf::<Rgb8>::new(vec![1, 2, 3, 4, 5, 6], dims(2, 1)).unwrap();
        assert_eq!(a, b);
        assert_eq!(b, a); // symmetric

        // Same dimensions, different samples -> not equal (catches "compares only dims").
        let diff_data = ImageBuf::<Rgb8>::new(vec![1, 2, 3, 4, 5, 7], dims(2, 1)).unwrap();
        assert_ne!(a, diff_data);

        // Same samples and same total count, different dimensions -> not equal (catches "compares
        // only data", and the `&&` collapsing to a single clause). 2x3 and 3x2 are both 6 Gray8
        // samples.
        let portrait = ImageBuf::<Gray8>::new(vec![0, 1, 2, 3, 4, 5], dims(2, 3)).unwrap();
        let landscape = ImageBuf::<Gray8>::new(vec![0, 1, 2, 3, 4, 5], dims(3, 2)).unwrap();
        assert_eq!(portrait.as_samples(), landscape.as_samples());
        assert_ne!(portrait, landscape);

        // The borrowed view mirrors the owned equality through `as_ref()`.
        assert_eq!(a.as_ref(), b.as_ref());
        assert_ne!(a.as_ref(), diff_data.as_ref());
        assert_ne!(portrait.as_ref(), landscape.as_ref());
    }

    #[test]
    fn width_height_report_each_dimension() {
        // Non-square so a width<->height swap is observable; assert exact values, not just non-zero.
        let buf = ImageBuf::<Rgb8>::zeroed(dims(4, 3)).unwrap();
        assert_eq!(buf.width(), 4);
        assert_eq!(buf.height(), 3);
        assert_eq!(buf.width(), buf.dimensions().width);
        assert_eq!(buf.height(), buf.dimensions().height);
    }

    #[test]
    fn as_mut_samples_edits_in_place() {
        let mut buf = ImageBuf::<Rgb8>::zeroed(dims(2, 2)).unwrap();
        // Distinct per-index sentinels (not a uniform fill) so a slice that aliases the wrong
        // region or length is caught, not just a no-op write.
        let samples = buf.as_mut_samples();
        assert_eq!(samples.len(), 2 * 2 * 3);
        for (i, s) in samples.iter_mut().enumerate() {
            *s = i as u8;
        }
        // The edits are observable through the shared accessor...
        assert_eq!(buf.as_samples(), &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11]);
        // ...and persist into the owned vector (i.e. we mutated the backing storage, not a copy).
        assert_eq!(
            buf.into_samples(),
            vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11]
        );
    }

    #[test]
    fn zeroed_is_all_default_and_correct_length() {
        let buf = ImageBuf::<Rgb8>::zeroed(dims(4, 3)).unwrap();
        assert_eq!(buf.as_samples().len(), 4 * 3 * 3);
        assert!(buf.as_samples().iter().all(|&s| s == 0));
        assert!(ImageBuf::<Rgb8>::zeroed(dims(0, 3)).is_err());
    }
}
