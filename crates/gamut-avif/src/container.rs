//! The total, byte-accounting representation of an AVIF file ([`AvifContainer`]).
//!
//! Mirroring the guarantee `gamut-heic` established for HEIF (and issue #250 asks of AVIF), it is
//! *structurally impossible to ignore any bits* in the container: every byte of the input is
//! mapped to exactly one [`Segment`], the segments are contiguous and non-overlapping, and
//! together they cover `0..data.len()` exactly. Unknown top-level boxes are surfaced verbatim
//! (never dropped), an appended foreign stream (a second top-level `ftyp`, e.g. a phone
//! motion-photo MP4) is retained as an opaque byte range, and any trailing non-box bytes become an
//! explicit [`SegmentKind::Trailer`].
//!
//! The top-level walk is [`gamut_isobmff::walk_segments`], shared with the sibling container
//! crate (#436), and stops on exactly the same rules as
//! [`gamut_isobmff::read`] — the first `ftyp` wins, a second top-level `ftyp` begins the appended
//! stream, and a malformed trailing box is tolerated (as a trailer) only once both `ftyp` and
//! `meta` have been seen — so the byte-accounting walk and the semantic parse never disagree.

use gamut_core::Result;
use gamut_isobmff::read;

use crate::image::AvifImage;

/// An AVIF file decomposed into a byte-exact list of [`Segment`]s plus the semantic [`AvifImage`].
///
/// # The every-byte invariant
///
/// [`segments`](Self::segments) is contiguous, non-overlapping, and covers `0..data.len()`
/// exactly: `segments[0].range.start == 0`, each `segments[i].range.end ==
/// segments[i+1].range.start`, and the last `range.end == data.len()` (the shared
/// [`gamut_core::segment::covers`] predicate). This is the "structurally impossible to ignore any
/// bits" guarantee issue #250 inherits from the HEIF surface — it holds by construction and is
/// pinned by the crate's tests. Every primary-stream top-level box (including unrecognised ones
/// such as a Google `mpvd`) is a [`SegmentKind::Box`]; an appended foreign stream is a single
/// [`SegmentKind::AppendedStream`]; any trailing non-box bytes are a single
/// [`SegmentKind::Trailer`].
///
/// The [`image`](Self::image) field is the role-typed semantic view of the *primary* still-image
/// stream. Boxes inside `meta` that [`gamut_isobmff::read`] does not consume (e.g. a `dinf`/`dref`
/// pair or a `uuid` box) are surfaced in [`unknown_meta_boxes`](Self::unknown_meta_boxes) so that,
/// at the meta level too, nothing is silently dropped.
#[derive(Debug)]
pub struct AvifContainer<'a> {
    data: &'a [u8],
    segments: Vec<Segment<'a>>,
    image: AvifImage,
    unknown_meta_boxes: Vec<UnknownBox<'a>>,
}

// `Segment`, `SegmentKind`, `UnknownBox` and `UnknownBoxLocation` are `gamut-isobmff`'s: the walk
// that produces them is shared with `gamut-heic`/`gamut-avif` (#436). Re-exported here, and from
// the crate root, so this crate's public surface is unchanged.
pub use gamut_isobmff::{Segment, SegmentKind, UnknownBox, UnknownBoxLocation};

impl<'a> AvifContainer<'a> {
    /// Parses `data` into the total byte-accounting representation and the role-typed semantic
    /// view.
    ///
    /// The semantic layer is [`gamut_isobmff::read`] wrapped in an [`AvifImage`], which
    /// additionally validates the primary item at parse time (the `pitm` id must name an existing,
    /// non-hidden item — ISO/IEC 23008-12). The byte-accounting layer walks the top-level boxes
    /// into [`segments`](Self::segments) covering every byte, and shadow-walks `meta` for boxes
    /// `read` does not consume ([`unknown_meta_boxes`](Self::unknown_meta_boxes)).
    ///
    /// # Errors
    ///
    /// Propagates every [`gamut_isobmff::read`] error (a missing/truncated required box, an
    /// out-of-scope feature such as an `avis` image sequence, …) and returns
    /// [`Error::InvalidInput`](gamut_core::Error::InvalidInput) if the primary item is missing or
    /// hidden. A malformed top-level box *before* both `ftyp` and `meta` are seen is a parse
    /// error; after that it is retained as a [`SegmentKind::Trailer`].
    pub fn parse(data: &'a [u8]) -> Result<Self> {
        // Semantic parse first: it enforces the stricter rules (rejecting out-of-scope
        // `moov`/`trak`, missing required boxes, …). Because it succeeds only once `ftyp`+`meta`
        // are seen and uses the identical `BoxReader` walk, the byte-accounting walk below cannot
        // then disagree.
        let image = AvifImage::new(read(data)?)?;
        let (segments, meta_body) = gamut_isobmff::walk_segments(data)?;
        let unknown_meta_boxes = match meta_body {
            Some(body) => gamut_isobmff::walk_meta_children(body)?,
            // Unreachable once `read` has succeeded (it requires `meta`); an empty list is the
            // honest answer if it ever were.
            None => Vec::new(),
        };
        Ok(Self {
            data,
            segments,
            image,
            unknown_meta_boxes,
        })
    }

    /// The whole input buffer this container was parsed from.
    #[must_use]
    pub fn data(&self) -> &'a [u8] {
        self.data
    }

    /// The byte-exact segment list. Contiguous, non-overlapping, and covering `0..data.len()`
    /// exactly (see the [type invariant](Self#the-every-byte-invariant)).
    #[must_use]
    pub fn segments(&self) -> &[Segment<'a>] {
        &self.segments
    }

    /// Iterates the primary-stream top-level boxes as `(type, body)` pairs, in file order — the
    /// [`SegmentKind::Box`] segments only.
    pub fn boxes(&self) -> impl Iterator<Item = ([u8; 4], &'a [u8])> + '_ {
        self.segments.iter().filter_map(|s| match &s.kind {
            SegmentKind::Box { ty, body } => Some((*ty, *body)),
            _ => None,
        })
    }

    /// The appended foreign stream (from a second top-level `ftyp` to EOF), if the file has one.
    #[must_use]
    pub fn appended_stream(&self) -> Option<&'a [u8]> {
        self.segments.iter().find_map(|s| match s.kind {
            SegmentKind::AppendedStream(bytes) => Some(bytes),
            _ => None,
        })
    }

    /// The trailing non-box bytes (a malformed/truncated box header to EOF), if the file has any.
    #[must_use]
    pub fn trailer(&self) -> Option<&'a [u8]> {
        self.segments.iter().find_map(|s| match s.kind {
            SegmentKind::Trailer(bytes) => Some(bytes),
            _ => None,
        })
    }

    /// The role-typed semantic view of the primary still-image stream.
    #[must_use]
    pub fn image(&self) -> &AvifImage {
        &self.image
    }

    /// The boxes inside `meta`/`iprp` that the semantic parse did not consume, surfaced verbatim.
    #[must_use]
    pub fn unknown_meta_boxes(&self) -> &[UnknownBox<'a>] {
        &self.unknown_meta_boxes
    }

    /// Decodes an item to a raw planar [`DecodedFrame`](crate::DecodedFrame) via `decoder`.
    /// Convenience forwarding to
    /// [`AvifImage::decode_item_planar`](crate::AvifImage::decode_item_planar).
    ///
    /// # Errors
    ///
    /// As [`AvifImage::decode_item_planar`](crate::AvifImage::decode_item_planar).
    pub fn decode_item_planar(
        &self,
        id: u32,
        decoder: &mut dyn crate::Av1StillDecoder,
    ) -> Result<crate::DecodedFrame> {
        self.image.decode_item_planar(id, decoder)
    }

    /// Decodes an item to a presentation-ready `ImageBuf<Rgba8>` via `decoder`. Convenience
    /// forwarding to [`AvifImage::decode_item_rgba8`](crate::AvifImage::decode_item_rgba8).
    ///
    /// # Errors
    ///
    /// As [`AvifImage::decode_item_rgba8`](crate::AvifImage::decode_item_rgba8).
    pub fn decode_item_rgba8(
        &self,
        id: u32,
        decoder: &mut dyn crate::Av1StillDecoder,
    ) -> Result<gamut_core::ImageBuf<gamut_core::Rgba8>> {
        self.image.decode_item_rgba8(id, decoder)
    }

    /// Decodes the primary item to a presentation-ready `ImageBuf<Rgba8>` via `decoder`.
    /// Convenience forwarding to
    /// [`AvifImage::decode_primary_rgba8`](crate::AvifImage::decode_primary_rgba8).
    ///
    /// # Errors
    ///
    /// As [`AvifImage::decode_primary_rgba8`](crate::AvifImage::decode_primary_rgba8).
    pub fn decode_primary_rgba8(
        &self,
        decoder: &mut dyn crate::Av1StillDecoder,
    ) -> Result<gamut_core::ImageBuf<gamut_core::Rgba8>> {
        self.image.decode_primary_rgba8(decoder)
    }

    /// Decodes an item to a presentation-ready high-bit-depth `ImageBuf<Rgba16>` via `decoder`.
    /// Convenience forwarding to
    /// [`AvifImage::decode_item_rgba16`](crate::AvifImage::decode_item_rgba16).
    ///
    /// # Errors
    ///
    /// As [`AvifImage::decode_item_rgba16`](crate::AvifImage::decode_item_rgba16).
    pub fn decode_item_rgba16(
        &self,
        id: u32,
        decoder: &mut dyn crate::Av1StillDecoder,
    ) -> Result<gamut_core::ImageBuf<gamut_core::Rgba16>> {
        self.image.decode_item_rgba16(id, decoder)
    }

    /// Decodes the primary item to a presentation-ready high-bit-depth `ImageBuf<Rgba16>` via
    /// `decoder`. Convenience forwarding to
    /// [`AvifImage::decode_primary_rgba16`](crate::AvifImage::decode_primary_rgba16).
    ///
    /// # Errors
    ///
    /// As [`AvifImage::decode_primary_rgba16`](crate::AvifImage::decode_primary_rgba16).
    pub fn decode_primary_rgba16(
        &self,
        decoder: &mut dyn crate::Av1StillDecoder,
    ) -> Result<gamut_core::ImageBuf<gamut_core::Rgba16>> {
        self.image.decode_primary_rgba16(decoder)
    }
}
