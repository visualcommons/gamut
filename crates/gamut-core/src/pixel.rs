//! Compile-time pixel vocabulary: the [`Sample`] storage primitives, the [`ColorModel`] tag, and
//! the sealed [`Pixel`] trait with one zero-sized marker per supported interleaved layout.
//!
//! These types brand the otherwise-opaque sample buffers in [`crate::ImageRef`] / [`crate::ImageBuf`]
//! so a layout mismatch — handing CMYK bytes to an RGBA encoder, or grayscale luminance where
//! palette indices are expected — is a compile error rather than a runtime length check. The markers
//! carry no data; they exist only to select an encoder/decoder impl and to expose the layout
//! constants ([`Pixel::CHANNELS`], [`Pixel::MODEL`], [`Pixel::BYTES_PER_PIXEL`]). The
//! [`PixelFormat`] tag is the runtime mirror of that closed marker set for boundaries that
//! dispatch dynamically (FFI, plugin registries).

mod sample_sealed {
    pub trait Sealed {}
    impl Sealed for u8 {}
    impl Sealed for u16 {}
}

/// A pixel-sample storage primitive: `u8` (8-bit) or `u16` (10/12/16-bit, high-bit-depth).
///
/// Sealed — only `u8` and `u16` implement it. The supertrait bounds are chosen so that `P::Sample`
/// transitively gives buffer types everything they need (copy, zero-fill via `Default`, ordering)
/// without callers repeating `where P::Sample: …` clauses. The storage width is available as
/// `size_of::<Self>()`; a stream's *coded* bit depth (e.g. 10 or 12) is a separate codec concern
/// carried elsewhere (see `gamut_color::BitDepth`).
///
/// [`Sample::MAX_VALUE`] and the two full-range conversions are what lets [`crate::convert`] do
/// depth-generic arithmetic: every sample width maps onto one canonical 16-bit working range, so
/// the conversion engine is written once rather than once per width.
pub trait Sample:
    sample_sealed::Sealed + Copy + Default + Ord + core::fmt::Debug + 'static
{
    /// The all-ones value for this width: fully opaque alpha, or white.
    const MAX_VALUE: Self;

    /// Widens this sample onto the canonical full-range 16-bit working scale.
    ///
    /// Exact and reversible: [`Sample::MAX_VALUE`] maps to `u16::MAX` and `0` maps to `0`, with no
    /// value left unreachable. For `u8` this is the `× 0x101` rescale PNG specifies for sample-depth
    /// expansion (3rd edition, §13.12); for `u16` it is the identity.
    fn to_full_range_u16(self) -> u16;

    /// Narrows a canonical full-range 16-bit working value back to this width, rounding to nearest.
    ///
    /// The inverse of [`Sample::to_full_range_u16`] — lossless for values that round-trip, and
    /// half-up rounded otherwise. For `u16` it is the identity.
    fn from_full_range_u16(value: u16) -> Self;
}

impl Sample for u8 {
    const MAX_VALUE: Self = u8::MAX;

    fn to_full_range_u16(self) -> u16 {
        // PNG 3rd edition §13.12 sample-depth rescaling: replicating the byte (v * 0x101) spreads
        // 0..=255 exactly onto 0..=65535, so 0xFF becomes 0xFFFF rather than 0xFF00.
        u16::from(self) * 0x101
    }

    fn from_full_range_u16(value: u16) -> Self {
        // round(value * 255 / 65535), half up. The +HALF addend before the division is what makes
        // this round to nearest rather than truncate, so 8-bit values survive a widen/narrow
        // round-trip unchanged.
        ((u32::from(value) * 255 + 32_767) / 65_535) as u8
    }
}

impl Sample for u16 {
    const MAX_VALUE: Self = u16::MAX;

    fn to_full_range_u16(self) -> u16 {
        self
    }

    fn from_full_range_u16(value: u16) -> Self {
        value
    }
}

/// The colour interpretation of a pixel's channels.
///
/// Distinguishes layouts that share a channel count: [`ColorModel::Rgba`] and [`ColorModel::Cmyk`]
/// are both four channels but must never be interchanged, and [`ColorModel::Gray`],
/// [`ColorModel::Bilevel`], and [`ColorModel::Indexed`] are all one channel with different meanings.
///
/// Discriminants are explicit and permanent — they are C ABI values (issue #242); new variants
/// append.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
#[repr(u32)]
pub enum ColorModel {
    /// Single luminance channel.
    Gray = 0,
    /// Luminance plus an alpha channel.
    GrayAlpha = 1,
    /// Red, green, blue.
    Rgb = 2,
    /// Red, green, blue, alpha (unassociated).
    Rgba = 3,
    /// Cyan, magenta, yellow, black ink separations.
    Cmyk = 4,
    /// One channel, `0` = black and any non-zero value = white (a 1-bit image carried as one byte
    /// per pixel).
    Bilevel = 5,
    /// One channel of indices into a separate colour palette.
    Indexed = 6,
}

/// Runtime tag for the closed set of [`Pixel`] marker types — one variant per marker.
///
/// The FFI/dispatch mirror of the compile-time `Pixel` matrix (issue #242): where the type
/// system selects a codec impl through the marker type, a boundary that only has runtime
/// information (a C caller, a plugin registry) selects the same layout by tag and dispatches to
/// the monomorphized impl. [`PixelFormat::ALL`] enumerates the matrix, as a slice, for tooling
/// that generates such dispatch.
///
/// Discriminants are explicit and permanent — they are C ABI values; new variants append.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
#[repr(u32)]
pub enum PixelFormat {
    /// Tag for [`Gray8`].
    Gray8 = 0,
    /// Tag for [`Bilevel`].
    Bilevel = 1,
    /// Tag for [`Indexed8`].
    Indexed8 = 2,
    /// Tag for [`Rgb8`].
    Rgb8 = 3,
    /// Tag for [`Rgba8`].
    Rgba8 = 4,
    /// Tag for [`Cmyk8`].
    Cmyk8 = 5,
    /// Tag for [`GrayAlpha8`].
    GrayAlpha8 = 6,
    /// Tag for [`Gray16`].
    Gray16 = 7,
    /// Tag for [`Rgb16`].
    Rgb16 = 8,
    /// Tag for [`Rgba16`].
    Rgba16 = 9,
    /// Tag for [`GrayAlpha16`].
    GrayAlpha16 = 10,
}

impl PixelFormat {
    /// Every pixel format, in discriminant order — the full codec × layout monomorphization
    /// matrix for tooling that enumerates it (each entry is some marker's [`Pixel::FORMAT`]).
    ///
    /// A slice rather than a fixed-length array, like every other `ALL` in the workspace: the
    /// enum is `#[non_exhaustive]` and appends variants, and appending to a slice changes its
    /// contents, never its type. Read the count from `ALL.len()` at runtime.
    pub const ALL: &'static [PixelFormat] = &[
        PixelFormat::Gray8,
        PixelFormat::Bilevel,
        PixelFormat::Indexed8,
        PixelFormat::Rgb8,
        PixelFormat::Rgba8,
        PixelFormat::Cmyk8,
        PixelFormat::GrayAlpha8,
        PixelFormat::Gray16,
        PixelFormat::Rgb16,
        PixelFormat::Rgba16,
        PixelFormat::GrayAlpha16,
    ];

    /// Samples per pixel; the runtime mirror of [`Pixel::CHANNELS`].
    #[must_use]
    pub const fn channels(self) -> usize {
        match self {
            PixelFormat::Gray8
            | PixelFormat::Bilevel
            | PixelFormat::Indexed8
            | PixelFormat::Gray16 => 1,
            PixelFormat::GrayAlpha8 | PixelFormat::GrayAlpha16 => 2,
            PixelFormat::Rgb8 | PixelFormat::Rgb16 => 3,
            PixelFormat::Rgba8 | PixelFormat::Cmyk8 | PixelFormat::Rgba16 => 4,
        }
    }

    /// The colour interpretation of the samples; the runtime mirror of [`Pixel::MODEL`].
    #[must_use]
    pub const fn color_model(self) -> ColorModel {
        match self {
            PixelFormat::Gray8 | PixelFormat::Gray16 => ColorModel::Gray,
            PixelFormat::Bilevel => ColorModel::Bilevel,
            PixelFormat::Indexed8 => ColorModel::Indexed,
            PixelFormat::Rgb8 | PixelFormat::Rgb16 => ColorModel::Rgb,
            PixelFormat::Rgba8 | PixelFormat::Rgba16 => ColorModel::Rgba,
            PixelFormat::Cmyk8 => ColorModel::Cmyk,
            PixelFormat::GrayAlpha8 | PixelFormat::GrayAlpha16 => ColorModel::GrayAlpha,
        }
    }

    /// Bytes per sample of the storage primitive (`1` for `u8` layouts, `2` for `u16`); the
    /// runtime mirror of `size_of::<`[`Pixel::Sample`]`>()`.
    #[must_use]
    pub const fn bytes_per_sample(self) -> usize {
        match self {
            PixelFormat::Gray8
            | PixelFormat::Bilevel
            | PixelFormat::Indexed8
            | PixelFormat::Rgb8
            | PixelFormat::Rgba8
            | PixelFormat::Cmyk8
            | PixelFormat::GrayAlpha8 => 1,
            PixelFormat::Gray16
            | PixelFormat::Rgb16
            | PixelFormat::Rgba16
            | PixelFormat::GrayAlpha16 => 2,
        }
    }

    /// Bytes one pixel occupies in an interleaved buffer; the runtime mirror of
    /// [`Pixel::BYTES_PER_PIXEL`].
    #[must_use]
    pub const fn bytes_per_pixel(self) -> usize {
        self.channels() * self.bytes_per_sample()
    }
}

// C ABI values — permanent, append-only (issue #242). A change here is an ABI break, not a
// refactor, so the pins are compile-time: editing a discriminant fails the defining crate's
// build rather than a later test run.
const _: () = {
    assert!(ColorModel::Gray as u32 == 0);
    assert!(ColorModel::GrayAlpha as u32 == 1);
    assert!(ColorModel::Rgb as u32 == 2);
    assert!(ColorModel::Rgba as u32 == 3);
    assert!(ColorModel::Cmyk as u32 == 4);
    assert!(ColorModel::Bilevel as u32 == 5);
    assert!(ColorModel::Indexed as u32 == 6);

    assert!(PixelFormat::Gray8 as u32 == 0);
    assert!(PixelFormat::Bilevel as u32 == 1);
    assert!(PixelFormat::Indexed8 as u32 == 2);
    assert!(PixelFormat::Rgb8 as u32 == 3);
    assert!(PixelFormat::Rgba8 as u32 == 4);
    assert!(PixelFormat::Cmyk8 as u32 == 5);
    assert!(PixelFormat::GrayAlpha8 as u32 == 6);
    assert!(PixelFormat::Gray16 as u32 == 7);
    assert!(PixelFormat::Rgb16 as u32 == 8);
    assert!(PixelFormat::Rgba16 as u32 == 9);
    assert!(PixelFormat::GrayAlpha16 as u32 == 10);
};

mod pixel_sealed {
    pub trait Sealed {}
}

/// Compile-time description of one interleaved pixel layout.
///
/// Sealed: implemented only by the zero-sized marker types in this module. A buffer is branded with
/// a `Pixel` type so its channel count, sample primitive, and colour model are known statically;
/// codecs implement [`crate::EncodeImage<P>`] / [`crate::DecodeImage<P>`] for exactly the `P` they
/// support, making an unsupported format a compile error.
pub trait Pixel: pixel_sealed::Sealed + Copy + 'static {
    /// The storage primitive of each sample (`u8` or `u16`).
    type Sample: Sample;
    /// The runtime tag identifying this layout (see [`PixelFormat`]).
    const FORMAT: PixelFormat;
    /// Samples per pixel.
    const CHANNELS: usize;
    /// The colour interpretation of those samples.
    const MODEL: ColorModel;
    /// Bytes one pixel occupies in an interleaved buffer (`CHANNELS * size_of::<Sample>()`).
    const BYTES_PER_PIXEL: usize = Self::CHANNELS * core::mem::size_of::<Self::Sample>();
}

/// Defines a zero-sized pixel marker and its [`Pixel`] impl from a compact table.
macro_rules! define_pixels {
    ($(
        $(#[$meta:meta])*
        $name:ident => $sample:ty, $channels:expr, $model:expr;
    )*) => {
        $(
            $(#[$meta])*
            #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
            pub struct $name;

            impl pixel_sealed::Sealed for $name {}
            impl Pixel for $name {
                type Sample = $sample;
                const FORMAT: PixelFormat = PixelFormat::$name;
                const CHANNELS: usize = $channels;
                const MODEL: ColorModel = $model;
            }
        )*
    };
}

define_pixels! {
    /// 8-bit grayscale: one luminance byte per pixel.
    Gray8 => u8, 1, ColorModel::Gray;
    /// 8-bit bilevel: one byte per pixel, `0` = black and non-zero = white. Distinct from [`Gray8`]
    /// so a grayscale buffer cannot be mistaken for a 1-bit image.
    Bilevel => u8, 1, ColorModel::Bilevel;
    /// 8-bit palette indices: one index byte per pixel into a separate colour table. Distinct from
    /// [`Gray8`] so indices cannot be mistaken for luminance.
    Indexed8 => u8, 1, ColorModel::Indexed;
    /// 8-bit RGB: three interleaved bytes per pixel, row-major.
    Rgb8 => u8, 3, ColorModel::Rgb;
    /// 8-bit RGBA: four interleaved bytes per pixel (unassociated alpha).
    Rgba8 => u8, 4, ColorModel::Rgba;
    /// 8-bit CMYK: four interleaved ink bytes per pixel. Distinct from [`Rgba8`] despite the shared
    /// channel count.
    Cmyk8 => u8, 4, ColorModel::Cmyk;
    /// 8-bit grayscale + alpha: two interleaved bytes per pixel (luminance, then unassociated
    /// alpha). The PNG "greyscale with alpha" colour type.
    GrayAlpha8 => u8, 2, ColorModel::GrayAlpha;
    /// 16-bit grayscale: one `u16` luminance sample per pixel (high-bit-depth).
    Gray16 => u16, 1, ColorModel::Gray;
    /// 16-bit RGB: three interleaved `u16` samples per pixel (high-bit-depth).
    Rgb16 => u16, 3, ColorModel::Rgb;
    /// 16-bit RGBA: four interleaved `u16` samples per pixel (high-bit-depth).
    Rgba16 => u16, 4, ColorModel::Rgba;
    /// 16-bit grayscale + alpha: two interleaved `u16` samples per pixel (luminance, then
    /// unassociated alpha; high-bit-depth).
    GrayAlpha16 => u16, 2, ColorModel::GrayAlpha;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pixel_layout_constants() {
        assert_eq!((Gray8::CHANNELS, Gray8::BYTES_PER_PIXEL), (1, 1));
        assert_eq!(Gray8::MODEL, ColorModel::Gray);
        assert_eq!((Bilevel::CHANNELS, Bilevel::BYTES_PER_PIXEL), (1, 1));
        assert_eq!(Bilevel::MODEL, ColorModel::Bilevel);
        assert_eq!((Indexed8::CHANNELS, Indexed8::BYTES_PER_PIXEL), (1, 1));
        assert_eq!(Indexed8::MODEL, ColorModel::Indexed);
        assert_eq!((Rgb8::CHANNELS, Rgb8::BYTES_PER_PIXEL), (3, 3));
        assert_eq!(Rgb8::MODEL, ColorModel::Rgb);
        assert_eq!((Rgba8::CHANNELS, Rgba8::BYTES_PER_PIXEL), (4, 4));
        assert_eq!(Rgba8::MODEL, ColorModel::Rgba);
        assert_eq!((Cmyk8::CHANNELS, Cmyk8::BYTES_PER_PIXEL), (4, 4));
        assert_eq!(Cmyk8::MODEL, ColorModel::Cmyk);
        assert_eq!((GrayAlpha8::CHANNELS, GrayAlpha8::BYTES_PER_PIXEL), (2, 2));
        assert_eq!(GrayAlpha8::MODEL, ColorModel::GrayAlpha);
        assert_eq!((Gray16::CHANNELS, Gray16::BYTES_PER_PIXEL), (1, 2));
        assert_eq!((Rgb16::CHANNELS, Rgb16::BYTES_PER_PIXEL), (3, 6));
        assert_eq!((Rgba16::CHANNELS, Rgba16::BYTES_PER_PIXEL), (4, 8));
        assert_eq!(
            (GrayAlpha16::CHANNELS, GrayAlpha16::BYTES_PER_PIXEL),
            (2, 4)
        );
        assert_eq!(GrayAlpha16::MODEL, ColorModel::GrayAlpha);
    }

    /// `P::FORMAT`'s runtime accessors must agree with `P`'s compile-time constants. The 11
    /// markers vary in every dimension, so any constant-replacement mutant in an accessor is
    /// killed by at least one instantiation.
    fn assert_format_matches<P: Pixel>() {
        assert_eq!(P::FORMAT.channels(), P::CHANNELS);
        assert_eq!(P::FORMAT.color_model(), P::MODEL);
        assert_eq!(P::FORMAT.bytes_per_pixel(), P::BYTES_PER_PIXEL);
        assert_eq!(
            P::FORMAT.bytes_per_sample(),
            core::mem::size_of::<P::Sample>()
        );
    }

    #[test]
    fn pixel_format_mirrors_marker_constants() {
        assert_format_matches::<Gray8>();
        assert_format_matches::<Bilevel>();
        assert_format_matches::<Indexed8>();
        assert_format_matches::<Rgb8>();
        assert_format_matches::<Rgba8>();
        assert_format_matches::<Cmyk8>();
        assert_format_matches::<GrayAlpha8>();
        assert_format_matches::<Gray16>();
        assert_format_matches::<Rgb16>();
        assert_format_matches::<Rgba16>();
        assert_format_matches::<GrayAlpha16>();
    }

    #[test]
    fn pixel_format_all_is_the_distinct_marker_set() {
        let formats = [
            Gray8::FORMAT,
            Bilevel::FORMAT,
            Indexed8::FORMAT,
            Rgb8::FORMAT,
            Rgba8::FORMAT,
            Cmyk8::FORMAT,
            GrayAlpha8::FORMAT,
            Gray16::FORMAT,
            Rgb16::FORMAT,
            Rgba16::FORMAT,
            GrayAlpha16::FORMAT,
        ];
        assert_eq!(PixelFormat::ALL, formats.as_slice());
        for (i, a) in PixelFormat::ALL.iter().enumerate() {
            for b in &PixelFormat::ALL[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }

    #[test]
    fn pixel_format_all_is_a_slice_of_every_variant_in_discriminant_order() {
        // Ascription: reverting `ALL` to a fixed-length array fails to compile here (#629).
        let all: &'static [PixelFormat] = PixelFormat::ALL;
        // Contiguous from 0 in order: no gap, duplicate or reordering.
        for (i, format) in all.iter().enumerate() {
            assert_eq!(*format as usize, i, "{format:?} is out of place");
        }
        // Discriminants append, so the newest variant has the highest one and must close the
        // slice. In-crate this match is exhaustive despite `#[non_exhaustive]`: appending a
        // variant fails to compile until it is named newest, and then fails above until listed.
        let is_newest = |format: PixelFormat| match format {
            PixelFormat::GrayAlpha16 => true,
            PixelFormat::Gray8
            | PixelFormat::Bilevel
            | PixelFormat::Indexed8
            | PixelFormat::Rgb8
            | PixelFormat::Rgba8
            | PixelFormat::Cmyk8
            | PixelFormat::GrayAlpha8
            | PixelFormat::Gray16
            | PixelFormat::Rgb16
            | PixelFormat::Rgba16 => false,
        };
        assert!(all.last().is_some_and(|&last| is_newest(last)));
    }

    #[test]
    fn u8_widens_by_byte_replication() {
        // PNG §13.12: the byte is replicated, not shifted. Endpoints and a midpoint pin the scale.
        assert_eq!(0u8.to_full_range_u16(), 0);
        assert_eq!(1u8.to_full_range_u16(), 0x0101);
        assert_eq!(128u8.to_full_range_u16(), 0x8080);
        assert_eq!(u8::MAX.to_full_range_u16(), u16::MAX);
        assert_eq!(u8::MAX_VALUE, 255);
    }

    #[test]
    fn u8_narrows_round_to_nearest() {
        // The rounding boundary sits between 128 and 129 (128/257 = 0.498, 129/257 = 0.502), so a
        // truncating implementation and an off-by-one addend both fail here.
        assert_eq!(u8::from_full_range_u16(0), 0);
        assert_eq!(u8::from_full_range_u16(128), 0);
        assert_eq!(u8::from_full_range_u16(129), 1);
        assert_eq!(u8::from_full_range_u16(257), 1);
        assert_eq!(u8::from_full_range_u16(u16::MAX), 255);
    }

    #[test]
    fn u8_survives_a_widen_narrow_round_trip() {
        // Every 8-bit value must be recovered exactly; this is what makes an 8 -> 16 -> 8 decode
        // path lossless and is the property the two constants above jointly guarantee.
        for value in 0..=u8::MAX {
            assert_eq!(u8::from_full_range_u16(value.to_full_range_u16()), value);
        }
    }

    #[test]
    fn u16_conversions_are_the_identity() {
        for value in [0u16, 1, 0x8080, 40_000, u16::MAX] {
            assert_eq!(value.to_full_range_u16(), value);
            assert_eq!(u16::from_full_range_u16(value), value);
        }
        assert_eq!(u16::MAX_VALUE, 65_535);
    }

    #[test]
    fn distinct_models_share_channel_count() {
        // Exactly the footgun the type system now prevents: same shape, different meaning.
        assert_eq!(Rgba8::CHANNELS, Cmyk8::CHANNELS);
        assert_ne!(Rgba8::MODEL, Cmyk8::MODEL);
        assert_eq!(Gray8::CHANNELS, Indexed8::CHANNELS);
        assert_ne!(Gray8::MODEL, Indexed8::MODEL);
        assert_ne!(Gray8::MODEL, Bilevel::MODEL);
    }
}
