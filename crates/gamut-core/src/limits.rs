//! [`DecodeLimits`]: the resource bounds a decoder enforces on hostile input.

/// The resource bounds a decoder enforces before it allocates for an image.
///
/// One shape for every format crate's decoder: a width bound and a height bound (each side
/// checked on its own), a bound on the pixel count `width × height`, which caps the allocation
/// independently of the aspect ratio, and an opt-in bound on the decoded image's size in bytes. A
/// limit is a policy the caller chooses, not a property of the format, so the defaults are
/// deliberately the same for every decoder, and they are libavif's: each side at most
/// [`DecodeLimits::DEFAULT_MAX_DIMENSION`] (32768, libavif's `AVIF_DEFAULT_IMAGE_DIMENSION_LIMIT`)
/// and at most [`DecodeLimits::DEFAULT_MAX_PIXELS`] (16384 × 16384, libavif's
/// `AVIF_DEFAULT_IMAGE_SIZE_LIMIT`) pixels in all. The byte budget is unbounded by default
/// ([`usize::MAX`]); a caller that wants one sets it with [`DecodeLimits::with_max_image_bytes`].
///
/// The default admits every ordinary photograph — a 6000 × 4000 RGB8 image is 24 million pixels,
/// well inside the 268 million the pixel cap allows — and refuses a header that declares a side
/// over 32768 or more than 16384² pixels, whatever the format's own field width would permit.
/// That is a narrowing for formats whose headers carry wider sides (JPEG and PNG up to 65535 and
/// 2³¹ − 1, TIFF and JPEG XL further): a file with a side past 32768, or past 16384² pixels, is
/// refused by default and needs explicitly widened limits.
///
/// Built with [`DecodeLimits::new`] (or [`Default`]) and the `const` `with_*` builders:
///
/// ```
/// use gamut_core::DecodeLimits;
///
/// const LIMITS: DecodeLimits = DecodeLimits::new().with_max_width(8192).with_max_height(8192);
/// assert_eq!(LIMITS.max_width(), 8192);
/// assert_eq!(LIMITS.max_pixels(), DecodeLimits::DEFAULT_MAX_PIXELS);
/// assert!(LIMITS.admits_dimensions(8192, 4096));
/// assert!(!LIMITS.admits_dimensions(8193, 1));
/// assert_eq!(LIMITS.max_image_bytes(), usize::MAX);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct DecodeLimits {
    max_width: u32,
    max_height: u32,
    max_pixels: u64,
    max_image_bytes: usize,
}

impl DecodeLimits {
    /// The default bound on each side, in pixels: 32768, libavif's
    /// `AVIF_DEFAULT_IMAGE_DIMENSION_LIMIT`.
    pub const DEFAULT_MAX_DIMENSION: u32 = 1 << 15;

    /// The default bound on the pixel count `width × height`: 16384 × 16384, libavif's
    /// `AVIF_DEFAULT_IMAGE_SIZE_LIMIT`.
    pub const DEFAULT_MAX_PIXELS: u64 = 1 << 28;

    /// The byte budget the default carried before it became opt-in: 64 MiB.
    #[deprecated(note = "the byte budget is opt-in; set one with `with_max_image_bytes`")]
    pub const DEFAULT_MAX_IMAGE_BYTES: usize = 64 << 20;

    /// The default limits: each side at most [`DecodeLimits::DEFAULT_MAX_DIMENSION`], at most
    /// [`DecodeLimits::DEFAULT_MAX_PIXELS`] pixels, and no bound on the decoded bytes.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            max_width: Self::DEFAULT_MAX_DIMENSION,
            max_height: Self::DEFAULT_MAX_DIMENSION,
            max_pixels: Self::DEFAULT_MAX_PIXELS,
            max_image_bytes: usize::MAX,
        }
    }

    /// Bounds the accepted image width, in pixels.
    #[must_use]
    pub const fn with_max_width(mut self, max_width: u32) -> Self {
        self.max_width = max_width;
        self
    }

    /// Bounds the accepted image height, in pixels.
    #[must_use]
    pub const fn with_max_height(mut self, max_height: u32) -> Self {
        self.max_height = max_height;
        self
    }

    /// Bounds the accepted pixel count, `width × height`.
    #[must_use]
    pub const fn with_max_pixels(mut self, max_pixels: u64) -> Self {
        self.max_pixels = max_pixels;
        self
    }

    /// Bounds the decoded image's size, in bytes. Unbounded ([`usize::MAX`]) unless set: what a
    /// byte counts as (the native samples, or the presented pixels) is the decoder's to document.
    #[must_use]
    pub const fn with_max_image_bytes(mut self, max_image_bytes: usize) -> Self {
        self.max_image_bytes = max_image_bytes;
        self
    }

    /// The largest accepted image width, in pixels.
    #[must_use]
    pub const fn max_width(self) -> u32 {
        self.max_width
    }

    /// The largest accepted image height, in pixels.
    #[must_use]
    pub const fn max_height(self) -> u32 {
        self.max_height
    }

    /// The largest accepted pixel count, `width × height`.
    #[must_use]
    pub const fn max_pixels(self) -> u64 {
        self.max_pixels
    }

    /// The largest accepted decoded image size, in bytes ([`usize::MAX`] when unbounded).
    #[must_use]
    pub const fn max_image_bytes(self) -> usize {
        self.max_image_bytes
    }

    /// Whether a `width × height` image fits every dimensional bound: the width bound, the height
    /// bound, and the pixel-count bound. The byte budget is not consulted — the bytes a pixel
    /// costs are the decoder's to count.
    ///
    /// A zero side has no pixels, so it passes the pixel-count bound; a decoder that learns the
    /// height late (JPEG's DNL) checks the width alone this way.
    #[must_use]
    pub const fn admits_dimensions(self, width: u32, height: u32) -> bool {
        // The product of two `u32`s always fits a `u64`.
        width <= self.max_width
            && height <= self.max_height
            && (width as u64) * (height as u64) <= self.max_pixels
    }
}

impl Default for DecodeLimits {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_libavifs_dimension_and_pixel_caps_with_no_byte_budget() {
        let limits = DecodeLimits::default();
        assert_eq!(limits, DecodeLimits::new());
        assert_eq!(limits.max_width(), 32768);
        assert_eq!(limits.max_height(), 32768);
        assert_eq!(limits.max_pixels(), 16384 * 16384);
        assert_eq!(limits.max_image_bytes(), usize::MAX);
    }

    #[test]
    fn each_builder_sets_only_its_own_bound() {
        let parts = |l: DecodeLimits| {
            (
                l.max_width(),
                l.max_height(),
                l.max_pixels(),
                l.max_image_bytes(),
            )
        };
        let (w, h, p) = (
            DecodeLimits::DEFAULT_MAX_DIMENSION,
            DecodeLimits::DEFAULT_MAX_DIMENSION,
            DecodeLimits::DEFAULT_MAX_PIXELS,
        );
        assert_eq!(
            parts(DecodeLimits::new().with_max_width(7)),
            (7, h, p, usize::MAX)
        );
        assert_eq!(
            parts(DecodeLimits::new().with_max_height(9)),
            (w, 9, p, usize::MAX)
        );
        assert_eq!(
            parts(DecodeLimits::new().with_max_pixels(13)),
            (w, h, 13, usize::MAX)
        );
        assert_eq!(
            parts(DecodeLimits::new().with_max_image_bytes(11)),
            (w, h, p, 11)
        );
    }

    #[test]
    fn admits_dimensions_checks_each_side_at_its_bound() {
        let limits = DecodeLimits::new().with_max_width(8).with_max_height(4);
        assert!(limits.admits_dimensions(8, 4));
        assert!(!limits.admits_dimensions(9, 4));
        assert!(!limits.admits_dimensions(8, 5));
    }

    #[test]
    fn admits_dimensions_checks_the_pixel_count_at_its_bound() {
        let limits = DecodeLimits::new().with_max_pixels(12);
        assert!(limits.admits_dimensions(4, 3));
        assert!(limits.admits_dimensions(3, 4));
        assert!(!limits.admits_dimensions(5, 3));
        assert!(!limits.admits_dimensions(13, 1));
        assert!(!limits.admits_dimensions(1, 13));
        // A zero side holds no pixels.
        assert!(limits.admits_dimensions(8, 0));
    }

    #[test]
    fn the_default_refuses_one_past_each_default_bound() {
        let limits = DecodeLimits::new();
        // A 6000 × 4000 photograph decodes under the default.
        assert!(limits.admits_dimensions(6000, 4000));
        assert!(limits.admits_dimensions(32768, 8192));
        assert!(!limits.admits_dimensions(32769, 1));
        assert!(!limits.admits_dimensions(1, 32769));
        assert!(limits.admits_dimensions(16384, 16384));
        assert!(!limits.admits_dimensions(16384, 16385));
        // The product never wraps: two maximal sides exceed every `u32`-sized cap.
        let open = DecodeLimits::new()
            .with_max_width(u32::MAX)
            .with_max_height(u32::MAX)
            .with_max_pixels(u64::from(u32::MAX));
        assert!(!open.admits_dimensions(u32::MAX, 2));
    }
}
