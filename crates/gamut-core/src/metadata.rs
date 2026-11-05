//! [`RawMetadata`]: the EXIF, XMP and ICC payloads a container carries, verbatim.

/// The three de-facto metadata payloads an image container carries, as the raw bytes it stores
/// them in — before any parse.
///
/// One shape for every format crate that surfaces them; parsing them into typed models is the
/// `gamut-metadata` facade's job. Built from [`Default`] and filled field by field, because the
/// struct is `#[non_exhaustive]`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct RawMetadata {
    /// The EXIF TIFF stream (starting `II`/`MM`), with any container signature stripped.
    pub exif: Option<Vec<u8>>,
    /// The XMP packet, with any container signature stripped.
    pub xmp: Option<Vec<u8>>,
    /// The ICC profile, reassembled if the container split it.
    pub icc: Option<Vec<u8>>,
}
