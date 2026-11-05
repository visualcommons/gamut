//! [`DecodeLimits`]: the resource bounds a decoder enforces on hostile input.

/// The resource bounds a decoder enforces before it allocates for an image.
///
/// One shape for every format crate's decoder: a width bound, a height bound, and a bound on the
/// decoded image's size in bytes, which caps the allocation independently of the aspect ratio. A
/// limit is a policy the caller chooses, not a property of the format, so the defaults are
/// deliberately the same for every decoder: no dimension bound beyond the `u32` a header can
/// carry, and [`DecodeLimits::DEFAULT_MAX_IMAGE_BYTES`] (64 MiB) of decoded samples.
///
/// Built with [`DecodeLimits::new`] (or [`Default`]) and the `const` `with_*` builders:
///
/// ```
/// use gamut_core::DecodeLimits;
///
/// const LIMITS: DecodeLimits = DecodeLimits::new().with_max_width(8192).with_max_height(8192);
/// assert_eq!(LIMITS.max_width(), 8192);
/// assert_eq!(LIMITS.max_image_bytes(), DecodeLimits::DEFAULT_MAX_IMAGE_BYTES);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DecodeLimits {
    max_width: u32,
    max_height: u32,
    max_image_bytes: usize,
}

impl DecodeLimits {
    /// The default bound on a decoded image's size in bytes: 64 MiB.
    pub const DEFAULT_MAX_IMAGE_BYTES: usize = 64 << 20;

    /// The default limits: any width and height a `u32` holds, and
    /// [`DecodeLimits::DEFAULT_MAX_IMAGE_BYTES`] of decoded samples.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            max_width: u32::MAX,
            max_height: u32::MAX,
            max_image_bytes: Self::DEFAULT_MAX_IMAGE_BYTES,
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

    /// Bounds the decoded image's size, in bytes.
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

    /// The largest accepted decoded image size, in bytes.
    #[must_use]
    pub const fn max_image_bytes(self) -> usize {
        self.max_image_bytes
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
    fn defaults_bound_only_the_image_bytes() {
        let limits = DecodeLimits::default();
        assert_eq!(limits, DecodeLimits::new());
        assert_eq!(limits.max_width(), u32::MAX);
        assert_eq!(limits.max_height(), u32::MAX);
        assert_eq!(limits.max_image_bytes(), 64 * 1024 * 1024);
    }

    #[test]
    fn each_builder_sets_only_its_own_bound() {
        let width = DecodeLimits::new().with_max_width(7);
        assert_eq!(
            (
                width.max_width(),
                width.max_height(),
                width.max_image_bytes()
            ),
            (7, u32::MAX, DecodeLimits::DEFAULT_MAX_IMAGE_BYTES)
        );
        let height = DecodeLimits::new().with_max_height(9);
        assert_eq!(
            (
                height.max_width(),
                height.max_height(),
                height.max_image_bytes()
            ),
            (u32::MAX, 9, DecodeLimits::DEFAULT_MAX_IMAGE_BYTES)
        );
        let bytes = DecodeLimits::new().with_max_image_bytes(11);
        assert_eq!(
            (
                bytes.max_width(),
                bytes.max_height(),
                bytes.max_image_bytes()
            ),
            (u32::MAX, u32::MAX, 11)
        );
    }
}
