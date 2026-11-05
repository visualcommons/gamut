//! Core traits, image buffers, dimensions, and error types shared across the gamut codecs.
//!
//! This crate is dependency-free with respect to the format crates: every codec in the
//! workspace builds on the [`EncodeImage`] / [`DecodeImage`] traits, the branded [`ImageRef`] /
//! [`ImageBuf`] buffers, and the [`Error`] type defined here, so that callers get a single,
//! consistent error surface regardless of format.
//!
//! # Scope and design notes
//!
//! The surface is deliberately small and stable within the current major; the following are
//! intentional decisions rather than omissions:
//!
//! - **Interleaved `u8` / `u16` layouts only.** [`Sample`] is sealed over `u8` and `u16`; planar
//!   layouts and codec-side concerns such as coded bit depth live in `gamut-color`
//!   (`gamut_color::Planar8`, `gamut_color::BitDepth`), not here.
//! - **Open where growth is additive, sealed where it must not be.** [`ErrorKind`] and
//!   [`ColorModel`] are `#[non_exhaustive]` so variants can be added without a breaking change,
//!   while [`Pixel`] and [`Sample`] are sealed — the set of pixel layouts is closed and only this
//!   crate defines it.
//! - **One opaque error, matched by kind.** [`Error`] keeps its representation private: callers
//!   match on [`Error::kind`] and read [`Error::origin`], [`Error::byte_offset`],
//!   [`Error::detail`] and `source()`, so a match never depends on what context a producer
//!   attached. It is built with [`Error::invalid_input`], [`Error::unsupported`] or
//!   `From<std::io::Error>`, the last keeping transport failures distinguishable from malformed
//!   input.
//! - **Dimensions are never empty.** [`Dimensions`] has private fields and one constructor,
//!   [`Dimensions::new`], which rejects a zero width or height; the buffers then enforce
//!   `len == width * height * channels` once, at [`ImageRef::new`] / [`ImageBuf::new`], so codecs
//!   receive a known-good buffer.
//! - **One vocabulary for what every format carries.** [`DecodeLimits`], [`Cicp`],
//!   [`Orientation`], [`Palette`], [`RawMetadata`] and the [`c2pa`] constants are defined here
//!   once, so format crates share them instead of each defining its own.
//! - **Byte accounting shares its shape and law, not its walks.** [`segment::Segment`] is generic
//!   over each format's own `kind` enum and [`segment::covers`] is the one "tiles `0..len`"
//!   predicate; the walks that produce the segments stay format-specific.
//!
//! # Example
//!
//! Drive any codec through the shared [`EncodeImage`] trait, handing it a branded, already-validated
//! [`ImageRef`]:
//!
//! ```
//! use gamut_core::{Dimensions, EncodeImage, ImageRef, Pixel, Result, Rgb8};
//!
//! // A toy encoder; a real codec would compress `image.as_samples()` instead of copying it.
//! struct RawEncoder;
//! impl EncodeImage<Rgb8> for RawEncoder {
//!     fn encode_image(&self, image: ImageRef<'_, Rgb8>, out: &mut Vec<u8>) -> Result<usize> {
//!         let start = out.len();
//!         out.extend_from_slice(image.as_samples()); // input is pre-validated by `ImageRef::new`
//!         Ok(out.len() - start)
//!     }
//! }
//!
//! let dims = Dimensions::new(2, 2).unwrap();
//! let pixels = vec![0u8; dims.sample_count(Rgb8::CHANNELS).unwrap()];
//! let image = ImageRef::<Rgb8>::new(&pixels, dims).unwrap();
//!
//! let mut out = Vec::new();
//! let written = RawEncoder.encode_image(image, &mut out).unwrap();
//! assert_eq!(written, 2 * 2 * 3);
//! ```
#![forbid(unsafe_code)]

// Compiles README.md's code blocks as doctests; exists only while rustdoc collects them.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;

pub mod c2pa;
mod cicp;
pub mod convert;
mod error;
mod image;
// Executable laws for the conversion engine, shared by this crate's property tests and the
// out-of-tree fuzz tier (`test-support`). Not part of the stable API; see `docs/testing.md`.
// A plain comment, not `///`: outer docs here would make rustdoc resolve the module's own `//!`
// intra-doc links from the crate root, where its items are not in scope.
#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub mod invariants;
mod limits;
pub mod luminance;
mod metadata;
mod orientation;
mod palette;
mod pixel;
pub mod segment;

pub use cicp::Cicp;
pub use convert::RawImage;
pub use error::{Error, ErrorKind, Result};
pub use image::{ImageBuf, ImageRef};
pub use limits::DecodeLimits;
pub use metadata::RawMetadata;
pub use orientation::Orientation;
pub use palette::Palette;
pub use pixel::{
    Bilevel, Cmyk8, ColorModel, Gray8, Gray16, GrayAlpha8, GrayAlpha16, Indexed8, Pixel,
    PixelFormat, Rgb8, Rgb16, Rgba8, Rgba16, Sample,
};

/// Width and height of an image, in pixels; neither is ever zero.
///
/// The fields are private so [`Dimensions::new`] is the only way to build one, and the non-zero
/// invariant holds for every value. `#[repr(C)]`: the layout — `width` then `height`, two `u32`s
/// — is a public guarantee so the value can cross the C ABI boundary as-is (issue #242).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(C)]
pub struct Dimensions {
    width: u32,
    height: u32,
}

impl Dimensions {
    /// Creates dimensions, rejecting a zero width or height.
    ///
    /// The only constructor, so an empty image is caught once, at the boundary, rather than
    /// re-checked in every encoder.
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorKind::InvalidInput`] error if either dimension is zero.
    pub fn new(width: u32, height: u32) -> Result<Self> {
        if width == 0 || height == 0 {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "zero-sized image",
            ));
        }
        Ok(Self { width, height })
    }

    /// The image width, in pixels (never zero).
    #[must_use]
    pub const fn width(self) -> u32 {
        self.width
    }

    /// The image height, in pixels (never zero).
    #[must_use]
    pub const fn height(self) -> u32 {
        self.height
    }

    /// The pixel count `width * height`, or `None` if it overflows `usize`.
    #[must_use]
    pub fn num_pixels(self) -> Option<usize> {
        (self.width as usize).checked_mul(self.height as usize)
    }

    /// The sample count for an interleaved buffer of `channels` samples per pixel
    /// (`width * height * channels`), or `None` on overflow. The length an [`crate::ImageRef`]
    /// validates against.
    #[must_use]
    pub fn sample_count(self, channels: usize) -> Option<usize> {
        self.num_pixels()?.checked_mul(channels)
    }
}

/// Encodes an [`ImageRef`] of a specific pixel layout `P` into a compressed byte stream.
///
/// A codec implements this once per pixel layout it supports (`impl EncodeImage<Rgb8> for …`,
/// `impl EncodeImage<Cmyk8> for …`, …), so asking it to encode an unsupported layout is a compile
/// error rather than a runtime `Unsupported`. The input is pre-validated by [`ImageRef::new`], so an
/// implementation never re-checks the buffer length. Bytes are appended to `out` to keep callers
/// that reuse a scratch buffer allocation-conscious.
pub trait EncodeImage<P: Pixel> {
    /// Encode `image` into `out` (appended), returning the number of bytes written.
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorKind::Unsupported`] error if the requested encoder configuration is not
    /// implemented, or an [`ErrorKind::InvalidInput`] one if the image violates a format
    /// constraint (e.g. a dimension limit).
    fn encode_image(&self, image: ImageRef<'_, P>, out: &mut Vec<u8>) -> Result<usize>;

    /// Encode `image` into a fresh [`Vec`], returning the encoded bytes.
    ///
    /// A convenience over [`EncodeImage::encode_image`] for callers that just want the bytes;
    /// reach for `encode_image` with a reused `&mut Vec<u8>` when encoding many images and you want
    /// to amortise the allocation.
    ///
    /// # Errors
    ///
    /// As [`EncodeImage::encode_image`].
    fn encode_to_vec(&self, image: ImageRef<'_, P>) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        self.encode_image(image, &mut out)?;
        Ok(out)
    }
}

/// Decodes a compressed byte stream into an owned [`ImageBuf`] of pixel layout `P`.
///
/// `P` selects the layout the caller wants back; a codec implements this for each layout it can
/// present (converting internally as needed, e.g. grayscale → [`Rgb8`]). Returning an owned
/// [`ImageBuf`] keeps the dimensions, samples, and layout brand together so the result can't be
/// misinterpreted.
pub trait DecodeImage<P: Pixel> {
    /// Decode `data` into a fresh [`ImageBuf`].
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorKind::InvalidInput`] error if `data` is malformed, or an
    /// [`ErrorKind::Unsupported`] one if it uses a feature that is not implemented or cannot be
    /// presented as `P`.
    fn decode_image(&self, data: &[u8]) -> Result<ImageBuf<P>>;

    /// Decode `data` into `dst`, reusing its allocation when possible.
    ///
    /// The default implementation always replaces the buffer (`*dst = self.decode_image(data)?`). A
    /// codec may override it to reuse `dst`'s sample storage — via [`ImageBuf::as_mut_samples`] —
    /// across repeated calls whose decoded dimensions match `dst`'s, falling back to replacement
    /// otherwise. Either way `dst` holds the decoded image on success.
    ///
    /// # Errors
    ///
    /// As [`DecodeImage::decode_image`].
    fn decode_image_into(&self, data: &[u8], dst: &mut ImageBuf<P>) -> Result<()> {
        *dst = self.decode_image(data)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dimensions_new_rejects_zero() {
        for (width, height) in [(0, 4), (4, 0), (0, 0)] {
            let err = Dimensions::new(width, height).unwrap_err();
            assert_eq!(err.kind(), ErrorKind::InvalidInput);
            assert_eq!(err.static_message(), Some("zero-sized image"));
            assert_eq!(err.origin(), Some("gamut-core"));
        }
        let d = Dimensions::new(4, 3).unwrap();
        assert_eq!((d.width(), d.height()), (4, 3));
    }

    #[test]
    fn dimensions_pixel_and_sample_counts() {
        let d = Dimensions::new(4, 3).unwrap();
        assert_eq!(d.num_pixels(), Some(12));
        assert_eq!(d.sample_count(3), Some(36));
        assert_eq!(d.sample_count(1), Some(12));
    }

    #[test]
    fn dimensions_sample_count_overflow_is_none() {
        // 65535*65535 fits in a 32-bit usize, so num_pixels is Some on every target...
        let d = Dimensions::new(0xFFFF, 0xFFFF).unwrap();
        assert_eq!(d.num_pixels(), Some(0xFFFF * 0xFFFF));
        // ...but scaling by usize::MAX channels overflows on any platform.
        assert_eq!(d.sample_count(usize::MAX), None);
    }
}

#[cfg(test)]
mod trait_tests {
    use super::*;

    /// A trivial codec: encodes by copying the samples out, decodes a fixed 1x1 gray pixel.
    /// Exists only to exercise the trait defaults and object-safety.
    struct Trivial;

    impl EncodeImage<Gray8> for Trivial {
        fn encode_image(&self, image: ImageRef<'_, Gray8>, out: &mut Vec<u8>) -> Result<usize> {
            out.extend_from_slice(image.as_samples());
            Ok(image.as_samples().len())
        }
    }

    impl DecodeImage<Gray8> for Trivial {
        fn decode_image(&self, _data: &[u8]) -> Result<ImageBuf<Gray8>> {
            ImageBuf::<Gray8>::new(vec![42u8], Dimensions::new(1, 1)?)
        }
    }

    /// A codec that always fails, to exercise error propagation through provided methods.
    struct Failing;

    impl EncodeImage<Gray8> for Failing {
        fn encode_image(&self, _image: ImageRef<'_, Gray8>, _out: &mut Vec<u8>) -> Result<usize> {
            Err(Error::unsupported("gamut-test", "nope"))
        }
    }

    #[test]
    fn encode_image_appends_and_counts() {
        let img = ImageBuf::<Gray8>::new(vec![1, 2, 3, 4], Dimensions::new(2, 2).unwrap()).unwrap();
        let mut out = vec![0xFF];
        let n = Trivial.encode_image(img.as_ref(), &mut out).unwrap();
        assert_eq!(n, 4);
        assert_eq!(out, vec![0xFF, 1, 2, 3, 4]);
    }

    #[test]
    fn encode_to_vec_returns_fresh_exact_bytes() {
        let img = ImageBuf::<Gray8>::new(vec![1, 2, 3, 4], Dimensions::new(2, 2).unwrap()).unwrap();
        // A fresh Vec holding exactly the encoded bytes — no leading scratch, unlike encode_image
        // which appends. Asserting exact contents kills an "Ok(Vec::new())" mutant.
        assert_eq!(
            Trivial.encode_to_vec(img.as_ref()).unwrap(),
            vec![1, 2, 3, 4]
        );
    }

    #[test]
    fn encode_to_vec_propagates_errors() {
        let img = ImageBuf::<Gray8>::new(vec![0], Dimensions::new(1, 1).unwrap()).unwrap();
        // The default must surface encode_image's error rather than swallow it into an empty Vec.
        assert!(Failing.encode_to_vec(img.as_ref()).is_err());
    }

    #[test]
    fn decode_image_into_default_forwards() {
        let mut dst = ImageBuf::<Gray8>::zeroed(Dimensions::new(1, 1).unwrap()).unwrap();
        Trivial.decode_image_into(&[], &mut dst).unwrap();
        assert_eq!(dst.as_samples(), &[42]);
    }

    #[test]
    fn traits_are_object_safe() {
        // Compiles and runs only while both traits stay object-safe (e.g. for `Box<dyn …>`).
        let enc: &dyn EncodeImage<Gray8> = &Trivial;
        let dec: &dyn DecodeImage<Gray8> = &Trivial;
        let img = ImageBuf::<Gray8>::new(vec![7u8], Dimensions::new(1, 1).unwrap()).unwrap();
        let mut out = Vec::new();
        assert_eq!(enc.encode_image(img.as_ref(), &mut out).unwrap(), 1);
        assert_eq!(dec.decode_image(&[]).unwrap().as_samples(), &[42]);
    }
}
