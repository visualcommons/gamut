//! Embedded metadata: the `ICCP` colour profile and the `EXIF` / `XMP ` chunks of a WebP file
//! (RFC 9649 §2.7.2-§2.7.3), plus the `C2PA` manifest store of C2PA 2.4 §A.3.7.
//!
//! Payloads cross this boundary **verbatim**. The crate neither parses nor re-serializes them, so a
//! profile or packet read by [`metadata`] is byte-for-byte the one a writer embedded — the property
//! the typed metadata crates (`gamut-exif`, `gamut-icc`, `gamut-xmp`, and the `gamut-metadata`
//! facade) need in order to borrow the bytes without a copy or a re-frame. Encoding is the mirror
//! image: [`WebpEncoder::with_exif`](crate::WebpEncoder::with_exif),
//! [`with_xmp`](crate::WebpEncoder::with_xmp),
//! [`with_icc_profile`](crate::WebpEncoder::with_icc_profile) and
//! [`with_c2pa`](crate::WebpEncoder::with_c2pa).

use core::ops::Range;

use gamut_core::Result;
use gamut_riff::MetadataChunks;

/// Embedded metadata read from a WebP file by [`metadata`].
///
/// Each payload is the raw chunk content, in the form the dedicated metadata crates parse (and
/// [`gamut-metadata`](https://crates.io/crates/gamut-metadata)'s `MetadataBlock` borrows) directly.
/// Marked `#[non_exhaustive]` so a further carrier can be added without a breaking change.
///
/// # Example
///
/// The extracted payload is byte-for-byte identical to the one given to the encoder, so it can be
/// borrowed straight into a typed metadata facade:
///
/// ```
/// use gamut_core::{Dimensions, EncodeImage, ImageRef, Rgb8};
/// use gamut_webp::WebpEncoder;
///
/// let icc = b"opaque ICC profile bytes";
/// let pixels = [10u8, 20, 30];
/// let image = ImageRef::<Rgb8>::new(&pixels, Dimensions::new(1, 1)?)?;
/// let mut file = Vec::new();
/// WebpEncoder::lossless()
///     .with_icc_profile(icc)
///     .encode_image(image, &mut file)?;
///
/// let meta = gamut_webp::metadata(&file)?;
/// assert_eq!(meta.icc.as_deref(), Some(icc.as_slice()));
/// # Ok::<(), gamut_core::Error>(())
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct WebpMetadata {
    /// The `EXIF` chunk payload: Exif metadata, carried bare (a WebP file stores no `"Exif\0\0"`
    /// signature, unlike a JPEG APP1 segment). Feed as `gamut_metadata::MetadataBlock::Exif`.
    pub exif: Option<Vec<u8>>,
    /// The `XMP ` chunk payload: an XMP packet. Feed as `MetadataBlock::Xmp`.
    pub xmp: Option<Vec<u8>>,
    /// The `ICCP` chunk payload: an ICC colour profile. `None` means sRGB is assumed (§2.7.2). Feed
    /// as `MetadataBlock::Icc`.
    pub icc: Option<Vec<u8>>,
    /// The `C2PA` chunk payload: a C2PA manifest store, carried opaquely (C2PA 2.4 §A.3.7). Nothing
    /// in gamut parses, hashes, signs or validates it; [`c2pa_span`] reports the byte range a
    /// `c2pa.hash.data` assertion excludes so a C2PA implementation can.
    pub c2pa: Option<Vec<u8>>,
}

/// Reads a WebP file's embedded metadata chunks without decoding any pixels.
///
/// Walks the top-level RIFF chunks and collects the four metadata payloads — `EXIF` and `XMP `
/// metadata (§2.7.3), the `ICCP` colour profile (§2.7.2) and the `C2PA` manifest store (C2PA 2.4
/// §A.3.7) — copying each out verbatim. The spec
/// permits at most one chunk of each kind and lets readers keep only the first, which is what this
/// does; the `VP8X` feature flags are advisory, so a payload is surfaced because its chunk is
/// present rather than because a flag advertises it. A simple (single-bitstream) file, which cannot
/// conformantly carry metadata at all, yields [`WebpMetadata::default`].
///
/// # Errors
///
/// Returns [`Error::InvalidInput`](gamut_core::Error::InvalidInput) if `data` is not a valid
/// RIFF/WebP file, or if a chunk's declared size runs past the end of the data.
///
/// # Example
///
/// ```
/// use gamut_core::{Dimensions, EncodeImage, ImageRef, Rgb8};
/// use gamut_webp::{WebpEncoder, WebpMetadata};
///
/// let pixels = [1u8, 2, 3];
/// let image = ImageRef::<Rgb8>::new(&pixels, Dimensions::new(1, 1)?)?;
/// let mut file = Vec::new();
/// WebpEncoder::lossless().encode_image(image, &mut file)?;
/// assert_eq!(gamut_webp::metadata(&file)?, WebpMetadata::default());
/// # Ok::<(), gamut_core::Error>(())
/// ```
pub fn metadata(data: &[u8]) -> Result<WebpMetadata> {
    let chunks = MetadataChunks::read(data)?;
    Ok(WebpMetadata {
        exif: chunks.exif.map(<[u8]>::to_vec),
        xmp: chunks.xmp.map(<[u8]>::to_vec),
        icc: chunks.icc.map(<[u8]>::to_vec),
        c2pa: chunks.c2pa.map(<[u8]>::to_vec),
    })
}

/// Reports the byte range the `C2PA` chunk occupies in `data`, or `None` if the file carries no
/// manifest store.
///
/// The range covers the chunk's **whole** span — the four identifier bytes, the four-byte size
/// field and the payload — because that is what a `c2pa.hash.data` assertion excludes (C2PA 2.4
/// §18.5): an update manifest may resize the store, which changes the size field's value as well as
/// the bytes after it. The RIFF pad byte that follows an odd-length store (RFC 9649 §2.3) is
/// outside the range; it is framing the container adds, not store.
///
/// This is the read-side twin of the range
/// [`WebpEncoder::encode_with_report`](crate::WebpEncoder::encode_with_report) returns — the same
/// walk over the same bytes — so a file's exclusion range does not depend on who computed it. No
/// pixels are decoded.
///
/// # Errors
///
/// Returns [`Error::InvalidInput`](gamut_core::Error::InvalidInput) if `data` is not a valid
/// RIFF/WebP file, or if a chunk's declared size runs past the end of the data.
///
/// # Example
///
/// ```
/// use gamut_core::{Dimensions, EncodeImage, ImageRef, Rgb8};
/// use gamut_webp::WebpEncoder;
///
/// let store = b"an opaque C2PA manifest store";
/// let pixels = [1u8, 2, 3];
/// let image = ImageRef::<Rgb8>::new(&pixels, Dimensions::new(1, 1)?)?;
/// let mut file = Vec::new();
/// WebpEncoder::lossless()
///     .with_c2pa(store)
///     .encode_image(image, &mut file)?;
///
/// let span = gamut_webp::c2pa_span(&file)?.expect("the store was embedded");
/// assert_eq!(&file[span.start + 8..span.end], store.as_slice());
/// # Ok::<(), gamut_core::Error>(())
/// ```
pub fn c2pa_span(data: &[u8]) -> Result<Option<Range<usize>>> {
    gamut_riff::c2pa_span(data)
}
