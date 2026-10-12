//! Core traits, image buffers, dimensions, and error types shared across the gamut codecs.
//!
//! This crate is dependency-free with respect to the format crates: every codec in the
//! workspace builds on the [`EncodeImage`] / [`DecodeImage`] traits, the branded [`ImageRef`] /
//! [`ImageBuf`] buffers, and the [`Error`] type defined here, so that callers get a single,
//! consistent error surface regardless of format.
//!
//! # Scope and design notes
//!
//! The surface is deliberately small and frozen; the following are intentional decisions rather
//! than omissions:
//!
//! - **Interleaved `u8` / `u16` layouts only.** [`Sample`] is sealed over `u8` and `u16`; planar
//!   layouts and codec-side concerns such as coded bit depth live in `gamut-color`
//!   (`gamut_color::Planar8`, `gamut_color::BitDepth`), not here.
//! - **Open where growth is additive, sealed where it must not be.** [`Error`] and [`ColorModel`]
//!   are `#[non_exhaustive]` so variants can be added without a breaking change, while [`Pixel`]
//!   and [`Sample`] are sealed — the set of pixel layouts is closed and only this crate defines it.
//! - **Static classifications, optional diagnostics.** [`Error::InvalidInput`] /
//!   [`Error::Unsupported`] retain their allocation-free `&'static str` payloads and public shape.
//!   Producers can wrap any error in [`Error::Context`] to attach an origin, byte offset, or owned
//!   detail without changing its [`ErrorKind`]. [`Error::Io`] preserves the underlying
//!   [`std::io::Error`] so transport failures stay distinguishable from malformed input.
//! - **The length invariant lives on the buffers, not on [`Dimensions`].** [`Dimensions`] is a plain
//!   value type with public fields; non-emptiness and `len == width * height * channels` are
//!   enforced once, at [`ImageRef::new`] / [`ImageBuf::new`], so codecs receive a known-good buffer.
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

pub mod convert;

mod image;
// Executable laws for the conversion engine, shared by this crate's property tests and the
// out-of-tree fuzz tier (`test-support`). Not part of the stable API; see `docs/testing.md`.
// A plain comment, not `///`: outer docs here would make rustdoc resolve the module's own `//!`
// intra-doc links from the crate root, where its items are not in scope.
#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub mod invariants;
pub mod luminance;
mod pixel;
pub mod segment;

pub use image::{ImageBuf, ImageRef};
pub use pixel::{
    Bilevel, Cmyk8, ColorModel, Gray8, Gray16, GrayAlpha8, GrayAlpha16, Indexed8, Pixel,
    PixelFormat, Rgb8, Rgb16, Rgba8, Rgba16, Sample,
};

/// Stable classification of a gamut error, independent of any diagnostic context around it.
///
/// The discriminants are permanent and append-only so the classification remains mechanically
/// portable to the C status surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
#[repr(u32)]
pub enum ErrorKind {
    /// The input data was malformed, truncated, or otherwise invalid.
    InvalidInput = 1,
    /// The requested format, profile, or feature is unsupported.
    Unsupported = 2,
    /// An underlying I/O operation failed.
    Io = 3,
}

const _: () = {
    assert!(ErrorKind::InvalidInput as u32 == 1);
    assert!(ErrorKind::Unsupported as u32 == 2);
    assert!(ErrorKind::Io as u32 == 3);
};

/// Structured diagnostic information attached to an [`Error`].
///
/// Callers normally inspect this through [`Error::origin`], [`Error::byte_offset`], and
/// [`Error::detail`]. The fields stay private so more optional context can be added without
/// changing the construction contract.
#[derive(Debug)]
#[non_exhaustive]
pub struct Diagnostic {
    source: Error,
    origin: Option<&'static str>,
    byte_offset: Option<u64>,
    detail: Option<Box<str>>,
}

impl Diagnostic {
    /// The underlying classified error.
    #[must_use]
    pub fn source_error(&self) -> &Error {
        &self.source
    }

    /// The Cargo package or format layer that produced the error, when known.
    #[must_use]
    pub fn origin(&self) -> Option<&'static str> {
        self.origin
    }

    /// The byte offset relative to the producing parser's immediate input, when known.
    #[must_use]
    pub fn byte_offset(&self) -> Option<u64> {
        self.byte_offset
    }

    /// Dynamic diagnostic detail retained from an underlying parser or backend, when available.
    #[must_use]
    pub fn detail(&self) -> Option<&str> {
        self.detail.as_deref()
    }
}

impl core::fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}", self.source)?;
        if self.origin.is_some() || self.byte_offset.is_some() {
            f.write_str(" [")?;
            if let Some(origin) = self.origin {
                write!(f, "origin: {origin}")?;
                if self.byte_offset.is_some() {
                    f.write_str(", ")?;
                }
            }
            if let Some(offset) = self.byte_offset {
                write!(f, "byte offset: {offset}")?;
            }
            f.write_str("]")?;
        }
        if let Some(detail) = &self.detail {
            write!(f, ": {detail}")?;
        }
        Ok(())
    }
}

impl std::error::Error for Diagnostic {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// Errors produced by gamut encoders and decoders.
///
/// Marked `#[non_exhaustive]` so additional variants can be added as formats land without a
/// breaking change.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The input data was malformed, truncated, or otherwise not valid for the format.
    #[error("invalid input: {0}")]
    InvalidInput(&'static str),
    /// The requested format, profile, or feature is not yet supported.
    #[error("unsupported: {0}")]
    Unsupported(&'static str),
    /// An underlying I/O operation failed while reading from a stream-backed source.
    ///
    /// Distinct from [`Error::InvalidInput`]: this reports the *transport* failing (a disk
    /// error, an interrupted read), not the bytes being malformed, so batch pipelines can retry
    /// or surface it instead of misclassifying the file as corrupt.
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    /// An existing error enriched with optional structured diagnostic context.
    ///
    /// The payload is boxed so adding context does not inflate the allocation-free legacy
    /// variants. Context is allocated only while constructing an error, never on a successful
    /// codec path.
    #[error("{0}")]
    Context(#[source] Box<Diagnostic>),
}

impl Error {
    /// Constructs an invalid-input error attributed to `origin`.
    ///
    /// Direct [`Error::InvalidInput`] construction remains the allocation-free compatibility
    /// path. This constructor is for producers that opt into structured diagnostics.
    #[must_use]
    pub fn invalid_input(origin: &'static str, message: &'static str) -> Self {
        Self::InvalidInput(message).with_origin(origin)
    }

    /// Constructs an unsupported-feature error attributed to `origin`.
    ///
    /// Direct [`Error::Unsupported`] construction remains the allocation-free compatibility path.
    #[must_use]
    pub fn unsupported(origin: &'static str, message: &'static str) -> Self {
        Self::Unsupported(message).with_origin(origin)
    }

    /// Returns the stable classification of this error, looking through diagnostic context.
    #[must_use]
    pub fn kind(&self) -> ErrorKind {
        match self {
            Self::InvalidInput(_) => ErrorKind::InvalidInput,
            Self::Unsupported(_) => ErrorKind::Unsupported,
            Self::Io(_) => ErrorKind::Io,
            Self::Context(diagnostic) => diagnostic.source.kind(),
        }
    }

    /// Returns the allocation-free static message carried by a legacy error, when present.
    ///
    /// Context does not change the message. [`Error::Io`] has no static message because its text
    /// belongs to the underlying [`std::io::Error`].
    #[must_use]
    pub fn static_message(&self) -> Option<&'static str> {
        match self {
            Self::InvalidInput(message) | Self::Unsupported(message) => Some(message),
            Self::Io(_) => None,
            Self::Context(diagnostic) => diagnostic.source.static_message(),
        }
    }

    /// Returns the Cargo package or format layer that produced the error, when known.
    #[must_use]
    pub fn origin(&self) -> Option<&'static str> {
        match self {
            Self::Context(diagnostic) => diagnostic.origin(),
            _ => None,
        }
    }

    /// Returns the byte offset relative to the producing parser's immediate input, when known.
    #[must_use]
    pub fn byte_offset(&self) -> Option<u64> {
        match self {
            Self::Context(diagnostic) => diagnostic.byte_offset(),
            _ => None,
        }
    }

    /// Returns dynamic diagnostic detail retained from an underlying parser or backend.
    #[must_use]
    pub fn detail(&self) -> Option<&str> {
        match self {
            Self::Context(diagnostic) => diagnostic.detail(),
            _ => None,
        }
    }

    /// Attaches the producing Cargo package or format layer if no origin is already present.
    #[must_use]
    pub fn with_origin(self, origin: &'static str) -> Self {
        self.with_diagnostic(|diagnostic| {
            if diagnostic.origin.is_none() {
                diagnostic.origin = Some(origin);
            }
        })
    }

    /// Attaches a byte offset if no offset is already present.
    ///
    /// The offset is relative to the immediate input accepted by the parser that reports it.
    #[must_use]
    pub fn with_byte_offset(self, byte_offset: u64) -> Self {
        self.with_diagnostic(|diagnostic| {
            if diagnostic.byte_offset.is_none() {
                diagnostic.byte_offset = Some(byte_offset);
            }
        })
    }

    /// Attaches owned dynamic detail if no detail is already present.
    ///
    /// This is the opt-in allocating path; static-only callers can keep constructing
    /// [`Error::InvalidInput`] and [`Error::Unsupported`] directly.
    #[must_use]
    pub fn with_detail(self, detail: impl Into<Box<str>>) -> Self {
        self.with_diagnostic(|diagnostic| {
            if diagnostic.detail.is_none() {
                diagnostic.detail = Some(detail.into());
            }
        })
    }

    fn with_diagnostic(self, update: impl FnOnce(&mut Diagnostic)) -> Self {
        match self {
            Self::Context(mut diagnostic) => {
                update(&mut diagnostic);
                Self::Context(diagnostic)
            }
            source => {
                let mut diagnostic = Diagnostic {
                    source,
                    origin: None,
                    byte_offset: None,
                    detail: None,
                };
                update(&mut diagnostic);
                Self::Context(Box::new(diagnostic))
            }
        }
    }
}

/// Convenience result type for gamut operations.
pub type Result<T> = core::result::Result<T, Error>;

/// Width and height of an image, in pixels.
///
/// `#[repr(C)]`: the layout — `width` then `height`, two `u32`s — is a public guarantee so the
/// value can cross the C ABI boundary as-is (issue #242).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(C)]
pub struct Dimensions {
    /// Image width in pixels.
    pub width: u32,
    /// Image height in pixels.
    pub height: u32,
}

impl Dimensions {
    /// Creates dimensions, rejecting a zero width or height.
    ///
    /// The fields stay public for ergonomic struct literals; this constructor is the validated
    /// path that buffer types ([`crate::ImageRef`]) and codecs use so an empty image is caught
    /// once, at the boundary, rather than re-checked in every encoder.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidInput`] if either dimension is zero.
    pub fn new(width: u32, height: u32) -> Result<Self> {
        if width == 0 || height == 0 {
            return Err(Error::InvalidInput("zero-sized image").with_origin(env!("CARGO_PKG_NAME")));
        }
        Ok(Self { width, height })
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

    /// Whether either dimension is zero.
    #[must_use]
    pub fn is_empty(self) -> bool {
        self.width == 0 || self.height == 0
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
    /// Returns [`Error::Unsupported`] if the requested encoder configuration is not implemented, or
    /// [`Error::InvalidInput`] if the image violates a format constraint (e.g. a dimension limit).
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
    /// Returns [`Error::InvalidInput`] if `data` is malformed, or [`Error::Unsupported`] if it uses
    /// a feature that is not implemented or cannot be presented as `P`.
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
    fn error_displays_and_dimensions_fields() {
        assert!(!Error::Unsupported("x").to_string().is_empty());
        assert!(!Error::InvalidInput("y").to_string().is_empty());
        let d = Dimensions {
            width: 1920,
            height: 1080,
        };
        assert_eq!(d.width, 1920);
        assert_eq!(d.height, 1080);
    }

    #[test]
    fn io_error_converts_and_displays() {
        // `#[from]` gives the `?`-friendly conversion; the Display must surface the source.
        let io = std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "short read");
        let err: Error = io.into();
        assert!(matches!(err, Error::Io(_)));
        assert!(err.to_string().contains("short read"));
    }

    #[test]
    fn legacy_error_variants_keep_their_public_shape() {
        let invalid = Error::InvalidInput("bad bytes");
        let unsupported = Error::Unsupported("future profile");
        assert!(matches!(invalid, Error::InvalidInput("bad bytes")));
        assert!(matches!(unsupported, Error::Unsupported("future profile")));
        assert_eq!(invalid.kind(), ErrorKind::InvalidInput);
        assert_eq!(unsupported.kind(), ErrorKind::Unsupported);
        assert_eq!(invalid.static_message(), Some("bad bytes"));
        assert_eq!(unsupported.static_message(), Some("future profile"));
        assert_eq!(invalid.origin(), None);
    }

    #[test]
    fn context_is_structured_first_write_wins_and_keeps_the_source() {
        use std::error::Error as _;

        let err = Error::InvalidInput("bad box")
            .with_origin("gamut-isobmff")
            .with_origin("gamut-avif")
            .with_byte_offset(17)
            .with_byte_offset(99)
            .with_detail("declared size exceeds input")
            .with_detail("replacement");

        assert_eq!(err.kind(), ErrorKind::InvalidInput);
        assert_eq!(err.static_message(), Some("bad box"));
        assert_eq!(err.origin(), Some("gamut-isobmff"));
        assert_eq!(err.byte_offset(), Some(17));
        assert_eq!(err.detail(), Some("declared size exceeds input"));
        assert_eq!(
            err.to_string(),
            "invalid input: bad box [origin: gamut-isobmff, byte offset: 17]: declared size exceeds input"
        );

        let diagnostic = match &err {
            Error::Context(diagnostic) => diagnostic,
            other => panic!("expected contextual error, got {other:?}"),
        };
        assert!(matches!(
            diagnostic.source_error(),
            Error::InvalidInput("bad box")
        ));
        assert!(err.source().is_some());
    }

    #[test]
    fn partial_context_displays_without_empty_separators() {
        assert_eq!(
            Error::Unsupported("future profile")
                .with_byte_offset(3)
                .to_string(),
            "unsupported: future profile [byte offset: 3]"
        );
        assert_eq!(
            Error::InvalidInput("bad bytes")
                .with_detail("decoder rejected marker")
                .to_string(),
            "invalid input: bad bytes: decoder rejected marker"
        );
        assert_eq!(
            Error::unsupported("gamut-test", "future profile").to_string(),
            "unsupported: future profile [origin: gamut-test]"
        );
    }

    #[test]
    fn contextual_io_keeps_its_kind_and_source_chain() {
        use std::error::Error as _;

        let err = Error::Io(std::io::Error::other("disk on fire"))
            .with_origin("gamut-ifd")
            .with_byte_offset(4096);
        assert_eq!(err.kind(), ErrorKind::Io);
        assert_eq!(err.static_message(), None);
        assert_eq!(err.origin(), Some("gamut-ifd"));
        assert_eq!(err.byte_offset(), Some(4096));
        let diagnostic = err.source().expect("diagnostic source");
        let io = diagnostic.source().expect("io source");
        assert_eq!(io.to_string(), "i/o error: disk on fire");
    }

    #[test]
    fn errors_remain_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Error>();
    }

    #[test]
    fn dimensions_new_rejects_zero() {
        assert!(Dimensions::new(0, 4).is_err());
        assert!(Dimensions::new(4, 0).is_err());
        assert!(Dimensions::new(0, 0).is_err());
        let d = Dimensions::new(4, 3).unwrap();
        assert_eq!((d.width, d.height), (4, 3));
    }

    #[test]
    fn dimensions_pixel_and_sample_counts() {
        let d = Dimensions {
            width: 4,
            height: 3,
        };
        assert_eq!(d.num_pixels(), Some(12));
        assert_eq!(d.sample_count(3), Some(36));
        assert_eq!(d.sample_count(1), Some(12));
        assert!(!d.is_empty());
    }

    #[test]
    fn dimensions_is_empty() {
        assert!(
            Dimensions {
                width: 0,
                height: 5
            }
            .is_empty()
        );
        assert!(
            Dimensions {
                width: 5,
                height: 0
            }
            .is_empty()
        );
        assert!(
            !Dimensions {
                width: 5,
                height: 5
            }
            .is_empty()
        );
    }

    #[test]
    fn dimensions_sample_count_overflow_is_none() {
        // 65535*65535 fits in a 32-bit usize, so num_pixels is Some on every target...
        let d = Dimensions {
            width: 0xFFFF,
            height: 0xFFFF,
        };
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
            Err(Error::Unsupported("nope"))
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
