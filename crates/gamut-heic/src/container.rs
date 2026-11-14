//! The total, byte-accounting representation of a HEIF file ([`HeifContainer`]).
//!
//! Issue #238's core requirement is that it be *structurally impossible to ignore any bits* in the
//! container: every byte of the input is mapped to exactly one [`Segment`], the segments are
//! contiguous and non-overlapping, and together they cover `0..data.len()` exactly. Unknown
//! top-level boxes are surfaced verbatim (never dropped), an appended foreign stream (a second
//! top-level `ftyp`, e.g. a Samsung motion-photo MP4) is retained as an opaque byte range, and any
//! trailing non-box bytes become an explicit [`SegmentKind::Trailer`].
//!
//! The top-level walk is [`gamut_isobmff::walk_segments`], shared with the sibling container
//! crate (#436), and stops on exactly the same rules as
//! [`IsoBmffImage::parse`] — the first `ftyp` wins, a second top-level `ftyp` begins the appended
//! stream, and a malformed trailing box is tolerated (as a trailer) only once both `ftyp` and
//! `meta` have been seen — so the byte-accounting walk and the semantic parse never disagree.

use gamut_core::{Error, Result};
use gamut_heif::{Codestream, HeifImage, ItemKind};
// `Segment`, `SegmentKind` and `UnknownBox` are `gamut-isobmff`'s: the walk that produces them is
// shared with `gamut-avif` (#436), and the crate root re-exports them.
use gamut_isobmff::{IsoBmffImage, Segment, SegmentKind, UnknownBox};

use crate::hvcc::HevcConfig;

/// The four-character HEIF brands that denote an HEVC still image (`references/heif` §7). `heim`/
/// `heis` are layered-HEVC brands, read as their single (base) layer.
const HEVC_BRANDS: [[u8; 4]; 4] = [*b"heic", *b"heix", *b"heim", *b"heis"];

/// A HEIF file decomposed into a byte-exact list of [`Segment`]s plus the semantic [`HeifImage`]
/// — the role-typed item model `gamut-heif` shares between AVIF and HEIC.
///
/// # The every-byte invariant
///
/// [`segments`](Self::segments) is contiguous, non-overlapping, and covers `0..data.len()`
/// exactly: `segments[0].range.start == 0`, each `segments[i].range.end == segments[i+1].range.start`,
/// and the last `range.end == data.len()` (the shared [`gamut_core::segment::covers`] predicate).
/// This is the "structurally impossible to ignore any bits" guarantee of issue #238 — it holds by
/// construction and is pinned by the crate's tests. Every primary-stream top-level box (including
/// unrecognised ones such as a Google `mpvd`) is a [`SegmentKind::Box`]; an appended foreign
/// stream is a single [`SegmentKind::AppendedStream`]; any trailing non-box bytes are a single
/// [`SegmentKind::Trailer`].
///
/// The [`image`](Self::image) field is the role-typed semantic view of the *primary* still-image
/// stream. Boxes inside `meta` that [`IsoBmffImage::parse`] does not consume (e.g. a `dinf`/`dref`
/// pair or a `uuid` box) are surfaced in [`unknown_meta_boxes`](Self::unknown_meta_boxes) so that,
/// at the meta level too, nothing is silently dropped.
#[derive(Debug)]
pub struct HeifContainer<'a> {
    data: &'a [u8],
    segments: Vec<Segment<'a>>,
    image: HeifImage,
    unknown_meta_boxes: Vec<UnknownBox<'a>>,
}

impl<'a> HeifContainer<'a> {
    /// Parses `data` into the total byte-accounting representation and the role-typed semantic view.
    ///
    /// The semantic layer is [`IsoBmffImage::parse`] wrapped in a [`HeifImage`], which additionally
    /// validates the primary item at parse time (the `pitm` id must name an existing, non-hidden
    /// item — ISO/IEC 23008-12). The byte-accounting layer walks the top-level boxes into
    /// [`segments`](Self::segments) covering every byte, and shadow-walks `meta` for boxes `read`
    /// does not consume ([`unknown_meta_boxes`](Self::unknown_meta_boxes)).
    ///
    /// # Errors
    ///
    /// Propagates every [`IsoBmffImage::parse`] error (a missing/truncated required box, an
    /// out-of-scope feature such as a sequence-only file, …) and returns an
    /// [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if the
    /// primary item is missing or hidden. A malformed top-level box *before* both `ftyp` and `meta`
    /// are seen is a parse error; after that it is retained as a [`SegmentKind::Trailer`]. A
    /// `moov`/`trak` beside the `meta` still is not an error: it is skipped, and its bytes are one
    /// opaque [`segments`](Self::segments) entry. The primary-item errors are raised by
    /// `gamut-heif`, which owns [`HeifImage`].
    pub fn parse(data: &'a [u8]) -> Result<Self> {
        // Semantic parse first: it enforces the stricter rules (refusing a sequence-only file,
        // missing required boxes, …). Because it succeeds only once `ftyp`+`meta` are seen and uses
        // the identical `BoxReader` walk, the byte-accounting walk below cannot then disagree.
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

    /// The role-typed semantic view of the primary still-image stream. Decode any of its items
    /// with `gamut-heif`'s surfaces and the [`Hevc`](crate::Hevc) binding, or the primary one with
    /// [`HeifDecoder`](crate::HeifDecoder).
    #[must_use]
    pub fn image(&self) -> &HeifImage {
        &self.image
    }

    /// Whether this file is an HEVC still image.
    ///
    /// True when the major brand or any compatible brand is one of `heic`/`heix`/`heim`/`heis`
    /// (`references/heif` §7), or — for the generic structural brand `mif1` — when the primary
    /// item is an `hvc1`/`hev1` coded item. Image-*sequence* brands (`msf1`/`hevc`/`hevx`) are out
    /// of scope and are not treated as stills here, but a valid still-image `meta` is not rejected
    /// for carrying one alongside `mif1` ([`IsoBmffImage::parse`] skips a track beside `meta` and
    /// refuses a sequence-only file).
    #[must_use]
    pub fn is_hevc_still(&self) -> bool {
        if HEVC_BRANDS.iter().any(|brand| self.image.has_brand(brand)) {
            return true;
        }
        self.image.has_brand(b"mif1")
            && matches!(
                self.image.primary_item().kind(),
                ItemKind::CodedImage { codec } if crate::Hevc::is_coded_item(&codec)
            )
    }

    /// The parsed `hvcC` configuration of coded item `item_id` ([`HevcConfig::parse`],
    /// `references/heif` §1).
    ///
    /// # Errors
    ///
    /// An [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if `item_id`
    /// names no item or the item carries no `hvcC`, and [`HevcConfig::parse`]'s errors.
    pub fn hevc_config(&self, item_id: u32) -> Result<HevcConfig> {
        let item = self.image.item(item_id).ok_or_else(|| {
            Error::invalid_input(env!("CARGO_PKG_NAME"), "HEIC: item id names no item")
        })?;
        let (_, data) = item
            .codec_configuration()
            .filter(|(kind, _)| *kind == b"hvcC")
            .ok_or_else(|| {
                Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "HEIC: item has no hvcC configuration",
                )
            })?;
        HevcConfig::parse(data)
    }

    /// For an HEVC coded item, whether parameter sets (VPS/SPS/PPS) may appear **inband** in its
    /// payload: `true` for `hev1`, `false` for `hvc1` (which keeps them only in `hvcC`). `None`
    /// when `item_id` names no item or a non-HEVC one. See `references/heif` §2.
    #[must_use]
    pub fn hevc_inband_parameter_sets_allowed(&self, item_id: u32) -> Option<bool> {
        match &self.image.item(item_id)?.as_isobmff_item().item_type {
            b"hev1" => Some(true),
            b"hvc1" => Some(false),
            _ => None,
        }
    }

    /// The boxes inside `meta`/`iprp` that the semantic parse did not consume, surfaced verbatim.
    #[must_use]
    pub fn unknown_meta_boxes(&self) -> &[UnknownBox<'a>] {
        &self.unknown_meta_boxes
    }
}
