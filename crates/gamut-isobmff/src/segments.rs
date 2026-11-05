//! Total byte accounting for an ISOBMFF file: every input byte maps to exactly one [`Segment`].
//!
//! [`walk_segments`] decomposes a file into a contiguous, non-overlapping list of segments
//! covering `0..data.len()` exactly, so it is *structurally impossible to ignore any bits*.
//! Unknown top-level boxes are surfaced verbatim (never dropped), an appended foreign stream (a
//! second top-level `ftyp`, e.g. a phone motion-photo MP4) is retained as an opaque byte range,
//! and any trailing non-box bytes become an explicit [`SegmentKind::Trailer`].
//! [`walk_meta_children`] does the same one level down, for boxes inside `meta` that [`parse`](crate::IsoBmffImage::parse)
//! does not consume.
//!
//! # Why this lives here
//!
//! `gamut-avif` and `gamut-heic` each carried a copy of this walk. After normalising the format
//! names the two files were byte-identical in code -- 199 lines each -- with no format-specific
//! brand check, item type, codec-configuration handling or error type between them (#436). Two
//! copies of one walk is two things to keep in step by hand, and the thing they had to be kept in
//! step with is the accounting guarantee itself.
//!
//! # Relationship to [`parse`](crate::IsoBmffImage::parse)
//!
//! This walk stops on exactly the rules [`parse`](crate::IsoBmffImage::parse) stops on: the first `ftyp` wins, a second
//! top-level `ftyp` begins the appended stream, and a malformed trailing box is tolerated only
//! once both `ftyp` and `meta` have been seen. So the byte-accounting walk and the semantic parse
//! cannot disagree about *where the primary stream ends*.
//!
//! Since #443 [`parse`](crate::IsoBmffImage::parse) also *retains* every top-level box other than `ftyp`/`meta`/`mdat` in
//! [`IsoBmffImage::top_level_boxes`](crate::IsoBmffImage::top_level_boxes); this walk still
//! reports each as a [`SegmentKind::Box`] with its range, because accounting is about *where* the
//! bytes are and the model is about *what* they carry. A sequence track's `moov` (or a stray
//! top-level `trak`) is the one box [`parse`](crate::IsoBmffImage::parse) neither parses nor retains (#646): its chunk offsets
//! address this file's `mdat`, so it cannot round-trip through the model. This walk is therefore
//! the only place its bytes are accounted for, as a plain [`SegmentKind::Box`].
//!
//! They are deliberately **not** one function. [`parse`](crate::IsoBmffImage::parse) is strictly stricter: it requires a
//! `meta` and parses it, and a file carrying a track but no `meta` is a sequence with no still
//! item, which it refuses with an [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported)
//! error where this walk
//! accounts it without complaint. Callers wanting both get both: parse with [`parse`](crate::IsoBmffImage::parse), account with
//! [`walk_segments`].

use gamut_core::Result;

use crate::boxes::BoxReader;

/// One contiguous run of the input file, tagged by what it holds ([`SegmentKind`]).
///
/// The shared [`gamut_core::segment::Segment`] over this format's kind, so a
/// [`walk_segments`] result is checked by the one tiling predicate,
/// [`gamut_core::segment::covers`], rather than a copy of it here (#500).
pub type Segment<'a> = gamut_core::segment::Segment<SegmentKind<'a>>;

/// What a [`Segment`] holds.
///
/// Non-exhaustive: a future revision may add a variant (e.g. a typed motion-photo marker) without
/// a breaking change — match with a wildcard arm.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SegmentKind<'a> {
    /// A top-level box of the primary stream: its four-character type and its body (the bytes
    /// after the 8-byte header). Recognised (`ftyp`/`meta`/`mdat`/`free`/`idat`…) and unrecognised
    /// (e.g. a Google Motion Photo `mpvd`) boxes alike are surfaced here — the walk never drops
    /// one.
    Box {
        /// The box's four-character type.
        ty: [u8; 4],
        /// The box body (everything after the 8-byte size+type header).
        body: &'a [u8],
    },
    /// An appended foreign stream: everything from a *second* top-level `ftyp` to end of file,
    /// kept opaque. Real-world phones append a whole second file here (a motion-photo MP4 with its
    /// own `moov`, optionally followed by a proprietary trailer); its vendor semantics stay
    /// downstream.
    AppendedStream(&'a [u8]),
    /// Trailing non-box bytes: from the first malformed/truncated top-level box header to end of
    /// file. Only permitted after both `ftyp` and `meta` have been parsed (before that a malformed
    /// box is a parse error) — matching [`IsoBmffImage::parse`](crate::IsoBmffImage::parse)'s tolerance rule exactly.
    Trailer(&'a [u8]),
}

/// A box found inside `meta` (or `meta`'s `iprp`) that the semantic [`IsoBmffImage::parse`](crate::IsoBmffImage::parse) does
/// not consume, surfaced verbatim so meta-level accounting is complete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownBox<'a> {
    /// Where the box was found — a direct child of `meta` or of `meta`'s `iprp`.
    pub location: UnknownBoxLocation,
    /// The box's four-character type (e.g. `*b"uuid"`, `*b"dinf"`).
    pub ty: [u8; 4],
    /// The box body (everything after the 8-byte size+type header).
    pub body: &'a [u8],
}

/// The container level at which an [`UnknownBox`] was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnknownBoxLocation {
    /// A direct child of the `meta` box (siblings of `hdlr`/`pitm`/`iloc`/`iinf`/`iref`/`iprp`/
    /// `idat`/`grpl`).
    Meta,
    /// A direct child of `meta`'s `iprp` box (siblings of `ipco`/`ipma`).
    Iprp,
}

/// What [`walk_segments`] returns: the segment list tiling the whole file, and the body of the
/// `meta` box it met, for [`walk_meta_children`].
///
/// Non-exhaustive: a later revision may report more of what the walk saw without a breaking
/// change.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct SegmentWalk<'a> {
    /// The contiguous, gap-free segments covering `0..data.len()`, in file order.
    pub segments: Vec<Segment<'a>>,
    /// The body of the last top-level `meta` box of the primary stream (the one
    /// [`IsoBmffImage::parse`](crate::IsoBmffImage::parse) reads), or `None` when the walk met
    /// none.
    pub meta_body: Option<&'a [u8]>,
}

/// Walks the top-level boxes into a contiguous, gap-free segment list covering `0..data.len()`,
/// also returning the `meta` box body (the last one, matching
/// [`IsoBmffImage::parse`](crate::IsoBmffImage::parse)) for [`walk_meta_children`].
///
/// Stops — and closes out the remaining bytes as one segment — at a second top-level `ftyp`
/// (appended stream) or at a malformed trailing box once `ftyp`+`meta` are seen (trailer).
///
/// # Errors
///
/// Propagates the [`BoxReader`] error for a malformed or truncated top-level box met *before*
/// both `ftyp` and `meta` have been seen. After that point a malformed box is not an error — it
/// becomes a [`SegmentKind::Trailer`].
pub fn walk_segments(data: &[u8]) -> Result<SegmentWalk<'_>> {
    let mut segments = Vec::new();
    let mut reader = BoxReader::new(data);
    let mut seen_ftyp = false;
    let mut seen_meta = false;
    let mut meta_body = None;
    loop {
        let box_start = reader.position();
        match reader.next() {
            Some(Ok(b)) => {
                // A second top-level `ftyp` begins the appended foreign stream: everything from
                // this header to EOF is one opaque segment. The first `ftyp` wins.
                if &b.ty == b"ftyp" && seen_ftyp {
                    segments.push(Segment {
                        range: b.offset..data.len(),
                        kind: SegmentKind::AppendedStream(&data[b.offset..]),
                    });
                    break;
                }
                let end = reader.position();
                segments.push(Segment {
                    range: b.offset..end,
                    kind: SegmentKind::Box {
                        ty: b.ty,
                        body: b.body,
                    },
                });
                match &b.ty {
                    b"ftyp" => seen_ftyp = true,
                    b"meta" => {
                        seen_meta = true;
                        meta_body = Some(b.body);
                    }
                    _ => {}
                }
            }
            // Clean end of the box list: the last box tiled exactly to EOF, so the segments
            // already cover every byte.
            None => break,
            Some(Err(e)) => {
                // A malformed trailing box after both required boxes are seen is retained as a
                // trailer (matching `read`). Before that, the file itself is malformed: propagate.
                if seen_ftyp && seen_meta {
                    segments.push(Segment {
                        range: box_start..data.len(),
                        kind: SegmentKind::Trailer(&data[box_start..]),
                    });
                    break;
                }
                return Err(e);
            }
        }
    }
    Ok(SegmentWalk {
        segments,
        meta_body,
    })
}

/// Shadow-walks a `meta` box body for boxes the semantic parse does not consume.
///
/// `meta` is a `FullBox`, so the first 4 bytes are its version+flags; its children then follow.
/// The consumed direct children are `hdlr`/`pitm`/`iloc`/`iinf`/`iref`/`iprp`/`idat`/`grpl`; every
/// other direct child is captured with [`UnknownBoxLocation::Meta`]. Descending into `iprp` (which
/// is a plain box, not a `FullBox`), the consumed children are `ipco`/`ipma`; every other is
/// captured with [`UnknownBoxLocation::Iprp`]. `ipco`/`iinf`/`iref` children are already fully
/// modelled by the semantic layer (as properties/items/references) and are never double-reported.
///
/// # Errors
///
/// Propagates the [`BoxReader`] error for a malformed or truncated child box.
pub fn walk_meta_children(meta_body: &[u8]) -> Result<Vec<UnknownBox<'_>>> {
    let mut unknown = Vec::new();
    // Skip the FullBox version+flags. `read` already validated `meta`, so the header is present;
    // an empty child region is the honest fallback if not.
    let children = meta_body.get(4..).unwrap_or(&[]);
    for b in BoxReader::new(children) {
        let b = b?;
        match &b.ty {
            b"iprp" => {
                for c in BoxReader::new(b.body) {
                    let c = c?;
                    if !matches!(&c.ty, b"ipco" | b"ipma") {
                        unknown.push(UnknownBox {
                            location: UnknownBoxLocation::Iprp,
                            ty: c.ty,
                            body: c.body,
                        });
                    }
                }
            }
            b"hdlr" | b"pitm" | b"iloc" | b"iinf" | b"iref" | b"idat" | b"grpl" => {}
            _ => unknown.push(UnknownBox {
                location: UnknownBoxLocation::Meta,
                ty: b.ty,
                body: b.body,
            }),
        }
    }
    Ok(unknown)
}
