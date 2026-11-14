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
//! [`IsoBmffImage::parse`] — the first `ftyp` wins, a second top-level `ftyp` begins the appended
//! stream, and a malformed trailing box is tolerated (as a trailer) only once both `ftyp` and
//! `meta` have been seen — so the byte-accounting walk and the semantic parse never disagree.

use gamut_av1::SequenceHeader;
use gamut_core::{Error, Result};
use gamut_heif::{HeifImage, ItemKind};
// `Segment`, `SegmentKind` and `UnknownBox` are `gamut-isobmff`'s: the walk that produces them is
// shared with `gamut-heic` (#436), and the crate root re-exports them.
use gamut_isobmff::{IsoBmffImage, Segment, SegmentKind, UnknownBox};

use crate::av1c::Av1Config;

/// The four-character AVIF brands that denote an AV1 still image (AVIF v1.2.0 §8.3): `avif` for a
/// still image file, `avio` for a file whose primary item (or its sources) are intra-only AV1
/// image items. The sequence brand `avis` is deliberately absent — image sequences are out of
/// scope (gamut is image-first).
const AV1_STILL_BRANDS: [[u8; 4]; 2] = [*b"avif", *b"avio"];

/// An AVIF file decomposed into a byte-exact list of [`Segment`]s plus the semantic
/// [`HeifImage`] — the role-typed item model `gamut-heif` shares between AVIF and HEIC.
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
/// stream. Boxes inside `meta` that [`IsoBmffImage::parse`] does not consume (e.g. a `dinf`/`dref`
/// pair or a `uuid` box) are surfaced in [`unknown_meta_boxes`](Self::unknown_meta_boxes) so that,
/// at the meta level too, nothing is silently dropped.
#[derive(Debug)]
pub struct AvifContainer<'a> {
    data: &'a [u8],
    segments: Vec<Segment<'a>>,
    image: HeifImage,
    unknown_meta_boxes: Vec<UnknownBox<'a>>,
}

impl<'a> AvifContainer<'a> {
    /// Parses `data` into the total byte-accounting representation and the role-typed semantic
    /// view.
    ///
    /// The semantic layer is [`IsoBmffImage::parse`] wrapped in a [`HeifImage`], which
    /// additionally validates the primary item at parse time (the `pitm` id must name an existing,
    /// non-hidden item — ISO/IEC 23008-12). The byte-accounting layer walks the top-level boxes
    /// into [`segments`](Self::segments) covering every byte, and shadow-walks `meta` for boxes
    /// `read` does not consume ([`unknown_meta_boxes`](Self::unknown_meta_boxes)).
    ///
    /// # Errors
    ///
    /// Propagates every [`IsoBmffImage::parse`] error (a missing/truncated required box, an
    /// out-of-scope feature such as a sequence-only `avis` file, …) and returns an
    /// [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error (raised by
    /// `gamut-heif`) if the primary item is missing or hidden. A malformed top-level box *before* both `ftyp` and `meta` are seen is a parse
    /// error; after that it is retained as a [`SegmentKind::Trailer`]. A `moov`/`trak` beside the
    /// `meta` still is not an error: it is skipped, and its bytes are one opaque
    /// [`segments`](Self::segments) entry, so an `avis` image sequence that carries a primary
    /// still item parses to that still ([`has_image_sequence`](Self::has_image_sequence) reports
    /// the track).
    pub fn parse(data: &'a [u8]) -> Result<Self> {
        // Semantic parse first: it enforces the stricter rules (refusing a sequence-only file,
        // missing required boxes, …). Because it succeeds only once `ftyp`+`meta`
        // are seen and uses the identical `BoxReader` walk, the byte-accounting walk below cannot
        // then disagree.
        let image = HeifImage::new(IsoBmffImage::parse(data)?)?;
        let walk = gamut_isobmff::walk_segments(data)?;
        let unknown_meta_boxes = match walk.meta_body {
            Some(body) => gamut_isobmff::walk_meta_children(body)?,
            // Unreachable once `parse` has succeeded (it requires `meta`); an empty list is the
            // honest answer if it ever were.
            None => Vec::new(),
        };
        Ok(Self {
            data,
            segments: walk.segments,
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

    /// Whether the file also carries an image sequence: a top-level `moov` box in the primary
    /// stream (an `avis` file's track, AVIF v1.2.0 §7, whose `trak` boxes live inside it per
    /// ISO/IEC 14496-12 §8.3.1).
    ///
    /// The sequence itself is never decoded — gamut is image-first — but the file's `meta`
    /// primary still is parsed and decodes like any other still, so a caller can tell a plain
    /// still apart from a sequence's still image. A `moov` inside an
    /// [`appended_stream`](Self::appended_stream) (e.g. a motion-photo MP4) belongs to that
    /// foreign stream, not to this file's image, and does not count; neither does the file's
    /// `ftyp` brand list, which declares conformance rather than content.
    #[must_use]
    pub fn has_image_sequence(&self) -> bool {
        self.boxes().any(|(ty, _)| ty == *b"moov")
    }

    /// The role-typed semantic view of the primary still-image stream. Decode any of its items
    /// with `gamut-heif`'s surfaces and the [`Av1`](crate::Av1) binding, or the primary one with
    /// [`AvifDecoder`](crate::AvifDecoder).
    #[must_use]
    pub fn image(&self) -> &HeifImage {
        &self.image
    }

    /// Whether this file is an AV1 still image.
    ///
    /// True when the major brand or any compatible brand is `avif` or `avio` (AVIF v1.2.0 §8.3),
    /// or — for the generic structural brand `mif1` — when the primary item is an `av01` coded
    /// item. The image-*sequence* brand (`avis`) alone does not make a still, but a file carrying
    /// it is not rejected either: an `avis` file that also declares an image brand for its
    /// primary item (which AVIF v1.2.0 §6.3's NOTE permits but does not require) is an AV1 still
    /// by `avif` or by `mif1` plus `av01`, and the track beside `meta` is skipped
    /// ([`has_image_sequence`](Self::has_image_sequence) reports it).
    #[must_use]
    pub fn is_av1_still(&self) -> bool {
        if AV1_STILL_BRANDS
            .iter()
            .any(|brand| self.image.has_brand(brand))
        {
            return true;
        }
        self.image.has_brand(b"mif1")
            && self.image.primary_item().kind() == ItemKind::CodedImage { codec: *b"av01" }
    }

    /// The parsed `av1C` configuration of coded item `item_id`.
    ///
    /// # Errors
    ///
    /// An [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if `item_id`
    /// names no item or the item carries no `av1C`, and [`Av1Config::parse`]'s errors.
    pub fn av1_config(&self, item_id: u32) -> Result<Av1Config> {
        self.av1_item(item_id).map(|(config, _)| config)
    }

    /// Parses the AV1 sequence header (AV1 §5.5) of coded item `item_id` through `gamut-av1`: the
    /// colour, bit depth and dimensions its samples were coded at.
    ///
    /// Works on any coded item with an `av1C`: pass the primary item's id for the colour image,
    /// and [`HeifImage::alpha_auxiliary_of`]'s for its alpha plane. The header is the one
    /// [`Av1Config::sequence_header`] selects (the payload's, else `configOBUs`'). When the item has
    /// no `colr`, this header's `color_config` is what the RGBA surfaces de-matrix with.
    ///
    /// # Errors
    ///
    /// As [`av1_config`](Self::av1_config), and [`Av1Config::sequence_header`]'s errors.
    pub fn av1_sequence_header(&self, item_id: u32) -> Result<SequenceHeader> {
        let (config, payload) = self.av1_item(item_id)?;
        config.sequence_header(payload)
    }

    /// Item `item_id`'s parsed `av1C` and its payload.
    fn av1_item(&self, item_id: u32) -> Result<(Av1Config, &[u8])> {
        let item = self.image.item(item_id).ok_or_else(|| {
            Error::invalid_input(env!("CARGO_PKG_NAME"), "AVIF: item id names no item")
        })?;
        let config = item
            .codec_configuration()
            .filter(|(kind, _)| *kind == b"av1C")
            .ok_or_else(|| {
                Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "AVIF: item has no av1C configuration",
                )
            })?;
        Ok((Av1Config::parse(config.1)?, &item.as_isobmff_item().payload))
    }

    /// The boxes inside `meta`/`iprp` that the semantic parse did not consume, surfaced verbatim.
    #[must_use]
    pub fn unknown_meta_boxes(&self) -> &[UnknownBox<'a>] {
        &self.unknown_meta_boxes
    }
}
