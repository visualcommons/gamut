//! What a lenient parse discarded.
//!
//! [`ExifReader`](crate::ExifReader) is lenient by default: a malformed Exif/GPS/Interop sub-IFD or
//! an out-of-bounds thumbnail range is dropped so the rest of the blob still parses. That is the
//! right default for real-world files, but on its own it is *silent* — a caller cannot tell a blob
//! that never carried GPS from one whose GPS pointer was dangling.
//!
//! A [`ReadReport`], returned by
//! [`ExifReader::parse_with_report`](crate::ExifReader::parse_with_report), names each discarded
//! region: **where** it was (a [`DroppedRegion`] and the tag that addressed it), **what offset**
//! addressed it, and **why** it went ([`DropReason`]).
//!
//! # What this report does and does not claim
//!
//! It covers exactly the regions [`DroppedRegion`] enumerates — the three pointer-addressed
//! sub-IFDs, the thumbnail's JPEG bytes, and any top-level directory past the 1st IFD. Within that
//! set it is complete: nothing in it is discarded without an entry.
//!
//! It is **not** a byte-completeness verdict over the blob, and
//! [`is_empty`](ReadReport::is_empty) does not mean "this parse lost nothing". Two known losses sit
//! outside it, both below this crate in [`gamut_ifd`]:
//!
//! * a **duplicate tag** within one directory keeps the last occurrence and discards the earlier
//!   one. This crate *could* detect that a directory lost an entry — `RawIfd::entries` is public
//!   and in on-disk order, so comparing its length against the decoded `Ifd::fields()` finds it in
//!   three lines — but it could not say what was lost without re-decoding the shadowed entry
//!   itself. The signal belongs at the layer that does the discarding, which three crates share
//!   (issue #528);
//! * a single unparseable **entry** fails its whole directory rather than being skipped, so the
//!   report's granularity is the directory, never the individual tag (issue #521).
//!
//! Bytes that no parsed structure ever claimed are likewise not reported; that verdict would need
//! `gamut-ifd`'s audit engine and is tracked in #521.

use core::fmt;

use crate::exif::{EXIF_IFD_POINTER, GPS_IFD_POINTER, INTEROP_IFD_POINTER};
use crate::tag::ExifTag;

/// A region of an EXIF blob that a lenient parse can discard.
///
/// Fieldless with an explicit `repr` and append-only discriminants, so the value crosses an FFI
/// boundary as a plain integer. `#[non_exhaustive]`: regions can be added post-1.0 without a
/// breaking change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
#[non_exhaustive]
pub enum DroppedRegion {
    /// The Exif sub-IFD, addressed by the `ExifIFD` pointer (`0x8769`) in the 0th IFD.
    ExifIfd = 0,
    /// The GPS sub-IFD, addressed by the `GPSInfo` pointer (`0x8825`) in the 0th IFD.
    GpsIfd = 1,
    /// The Interoperability sub-IFD, addressed by the `Interoperability` pointer (`0xA005`)
    /// *inside* the Exif sub-IFD.
    InteropIfd = 2,
    /// The 1st IFD's embedded JPEG thumbnail bytes, addressed by `JPEGInterchangeFormat`
    /// (`0x0201`) and sized by `JPEGInterchangeFormatLength` (`0x0202`). The thumbnail's own
    /// directory survives; only its bytes are lost.
    ///
    /// Reported when the range lies outside the blob ([`DropReason::OutOfBounds`]) and when the
    /// offset has no length beside it ([`DropReason::ThumbnailLengthMissing`]) — Exif 3.0 §4.6.9.2 Table 21
    /// marks both tags mandatory for a compressed thumbnail, so half the pair addresses bytes
    /// nothing can size.
    ThumbnailJpeg = 3,
    /// A top-level directory past the 1st IFD.
    ///
    /// EXIF defines exactly two: the 0th IFD (primary image) and the 1st (thumbnail). A stream
    /// whose next-IFD chain runs on has more, and the [`Exif`](crate::Exif) model has nowhere to
    /// put them — so they parse cleanly and are then discarded. No tag addresses one (the chain is
    /// followed through the structural next-IFD pointer), so [`Dropped::tag`] is `None`.
    ///
    /// Reported in [`strict`](crate::ExifReader::strict) mode too: strictness rejects *malformed*
    /// regions, and nothing about a trailing directory is malformed — it is well-formed and
    /// unrepresentable. A strict report is therefore empty of everything *but* this.
    TrailingIfd = 4,
}

impl DroppedRegion {
    /// The region's short name, as it appears in [`Dropped`]'s `Display`.
    ///
    /// For the three sub-IFDs this is also the spelling
    /// [`ExifError::InvalidIfd`](crate::ExifError::InvalidIfd) uses in strict mode. The other two
    /// have no such error: a strict out-of-bounds thumbnail is
    /// [`ExifError::BadThumbnail`](crate::ExifError::BadThumbnail), and a trailing directory is
    /// discarded in strict mode exactly as in lenient mode, since nothing about it is malformed.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::ExifIfd => "Exif",
            Self::GpsIfd => "GPS",
            Self::InteropIfd => "Interop",
            Self::ThumbnailJpeg => "Thumbnail",
            Self::TrailingIfd => "TrailingIFD",
        }
    }

    /// The tag whose value addressed this region, or `None` when no tag does.
    pub(crate) const fn tag(self) -> Option<u16> {
        match self {
            Self::ExifIfd => Some(EXIF_IFD_POINTER),
            Self::GpsIfd => Some(GPS_IFD_POINTER),
            Self::InteropIfd => Some(INTEROP_IFD_POINTER),
            Self::ThumbnailJpeg => Some(ExifTag::JpegInterchangeFormat.tag_id()),
            Self::TrailingIfd => None,
        }
    }
}

/// Why a region was discarded.
///
/// Fieldless with an explicit `repr` and append-only discriminants; `#[non_exhaustive]` so reasons
/// can be distinguished more finely post-1.0 without a breaking change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
#[non_exhaustive]
pub enum DropReason {
    /// The address itself lay outside the blob — a pointer at or past the end of the TIFF stream,
    /// or a byte range that is not wholly inside it. Nothing could have been read there.
    OutOfBounds = 0,
    /// The address was inside the blob, but the structure at it did not parse: a bad entry count,
    /// an unreadable entry, or a value offset the directory could not resolve.
    Malformed = 1,
    /// Nothing was wrong with the region — it parsed cleanly — but the EXIF model has no place to
    /// put it, so it could not be carried across.
    Unrepresentable = 2,
    /// The 1st IFD carried a `JPEGInterchangeFormat` offset with no `JPEGInterchangeFormatLength`
    /// beside it, so there was no range to read and the JPEG behind the offset is lost.
    ///
    /// Deliberately named for that one site rather than for the shape of the defect: it is the
    /// only thing this reason ever means, and a generic name on a single-site variant invites
    /// unrelated reuse that a `#[non_exhaustive]` enum can add a *new* variant for instead.
    ///
    /// Distinct from [`OutOfBounds`](Self::OutOfBounds) — the address may be perfectly valid — and
    /// from [`Malformed`](Self::Malformed), which is about bytes that *were* read and did not
    /// parse. The repair is different in each case, which is why they are different reasons.
    ThumbnailLengthMissing = 3,
}

impl DropReason {
    /// The clause [`Dropped`]'s `Display` uses for this reason.
    const fn clause(self) -> &'static str {
        match self {
            Self::OutOfBounds => "addresses bytes outside the EXIF blob",
            Self::Malformed => "is not a well-formed directory",
            Self::Unrepresentable => "parsed cleanly but has no place in the EXIF model",
            Self::ThumbnailLengthMissing => "has no JPEGInterchangeFormatLength to size the read",
        }
    }
}

/// One region a lenient parse discarded, named by where it was and why it went.
///
/// `Copy` plain data reachable through accessors, so it is representable across an FFI boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Dropped {
    region: DroppedRegion,
    tag: Option<u16>,
    offset: u64,
    reason: DropReason,
}

impl Dropped {
    /// Records a drop of `region`, addressed by its tag at `offset`, for `reason`.
    pub(crate) const fn new(region: DroppedRegion, offset: u64, reason: DropReason) -> Self {
        Self {
            region,
            tag: region.tag(),
            offset,
            reason,
        }
    }

    /// Which region was discarded.
    #[must_use]
    pub const fn region(self) -> DroppedRegion {
        self.region
    }

    /// The tag whose value addressed the discarded region — the pointer tag for a sub-IFD,
    /// `JPEGInterchangeFormat` for the thumbnail bytes.
    ///
    /// `None` when no tag addresses the region, which today means only
    /// [`DroppedRegion::TrailingIfd`]: a top-level directory is reached through the structural
    /// next-IFD pointer, not through a tag. This is an `Option` rather than a `0` sentinel because
    /// `0` is a real tag number (`GPSVersionID`), and the region set is `#[non_exhaustive]` — the
    /// next region without an addressing tag might well be one inside a GPS directory.
    #[must_use]
    pub const fn tag(self) -> Option<u16> {
        self.tag
    }

    /// The offset of the discarded region, relative to the start of the TIFF stream (i.e. *after*
    /// any `Exif\0\0` marker) — the same frame of reference EXIF's own offsets use.
    ///
    /// For a sub-IFD or the thumbnail bytes this is the value the addressing tag carried; for a
    /// [`TrailingIfd`](DroppedRegion::TrailingIfd) it is the directory's own position in the
    /// stream.
    ///
    /// This is **not** the frame the crate's *error* messages use. An [`ExifError`](crate::ExifError)
    /// carries the offset of the byte the reader could not read in the source the caller handed in,
    /// so for a marked blob it is 6 bytes (`MARKER.len()`) larger than the same position expressed
    /// here. The two frames are deliberately different: a diagnostic points into the caller's own
    /// buffer, while a report offset addresses the TIFF structure the report describes and matches
    /// every offset stored inside the file.
    #[must_use]
    pub const fn offset(self) -> u64 {
        self.offset
    }

    /// Why the region was discarded.
    #[must_use]
    pub const fn reason(self) -> DropReason {
        self.reason
    }
}

impl fmt::Display for Dropped {
    /// One grammar for every drop — `dropped <region> (tag <t>) at offset <n>: <reason>` — with
    /// `none` as the explicit absent marker rather than a second shape. A caller that scrapes this
    /// line should not have to recognise two forms, and an absent tag is a fact worth stating.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = self.region.name();
        let clause = self.reason.clause();
        match self.tag {
            Some(tag) => write!(
                f,
                "dropped {name} (tag {tag:#06x}) at offset {}: {clause}",
                self.offset
            ),
            None => write!(
                f,
                "dropped {name} (tag none) at offset {}: {clause}",
                self.offset
            ),
        }
    }
}

/// What a lenient parse discarded, over the regions [`DroppedRegion`] enumerates.
///
/// Obtained from [`ExifReader::parse_with_report`](crate::ExifReader::parse_with_report) or
/// [`ExifReader::parse_from_with_report`](crate::ExifReader::parse_from_with_report). In
/// [`strict`](crate::ExifReader::strict) mode the first *malformed* region fails the parse instead
/// of being reported — but a strict report is **not** therefore always empty:
/// [`DroppedRegion::TrailingIfd`] is well-formed and merely unrepresentable, so strictness has no
/// grounds to reject it and it is reported in both modes.
///
/// Read the module documentation for what this report deliberately does **not** cover: it is not a
/// byte-completeness verdict, and losses inside a single directory belong to [`gamut_ifd`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReadReport {
    dropped: Vec<Dropped>,
}

impl ReadReport {
    /// An empty report — nothing discarded.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Every discarded region, in the order the reader met it.
    #[must_use]
    pub fn dropped(&self) -> &[Dropped] {
        &self.dropped
    }

    /// Whether any of the regions this report covers was discarded.
    ///
    /// **Not** a "this parse lost nothing" verdict — see the module documentation. An empty report
    /// means no sub-IFD, thumbnail range or trailing directory was dropped; it says nothing about
    /// a shadowed duplicate tag (#528) or about source bytes no structure claimed (#521).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.dropped.is_empty()
    }

    /// Appends a discarded region.
    pub(crate) fn record(&mut self, dropped: Dropped) {
        self.dropped.push(dropped);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each region reports the tag that actually addresses it — the value a caller uses to find
    /// the pointer back in the source directory — and a region no tag addresses reports `None`.
    #[test]
    fn each_region_carries_the_tag_that_addresses_it() {
        for (region, tag) in [
            (DroppedRegion::ExifIfd, Some(0x8769)),
            (DroppedRegion::GpsIfd, Some(0x8825)),
            (DroppedRegion::InteropIfd, Some(0xA005)),
            (DroppedRegion::ThumbnailJpeg, Some(0x0201)),
            (DroppedRegion::TrailingIfd, None),
        ] {
            assert_eq!(
                Dropped::new(region, 0, DropReason::OutOfBounds).tag(),
                tag,
                "wrong addressing tag for {region:?}"
            );
        }
    }

    /// The rendered form names the region, the tag, the offset and the reason — the four facts a
    /// caller reporting a lossy parse needs, and the strings are part of the contract.
    #[test]
    fn the_rendered_drop_names_region_tag_offset_and_reason() {
        assert_eq!(
            Dropped::new(DroppedRegion::GpsIfd, 65_535, DropReason::OutOfBounds).to_string(),
            "dropped GPS (tag 0x8825) at offset 65535: addresses bytes outside the EXIF blob"
        );
        assert_eq!(
            Dropped::new(DroppedRegion::ExifIfd, 26, DropReason::Malformed).to_string(),
            "dropped Exif (tag 0x8769) at offset 26: is not a well-formed directory"
        );
        assert_eq!(
            Dropped::new(DroppedRegion::InteropIfd, 8, DropReason::Malformed).to_string(),
            "dropped Interop (tag 0xa005) at offset 8: is not a well-formed directory"
        );
        assert_eq!(
            Dropped::new(DroppedRegion::ThumbnailJpeg, 1, DropReason::OutOfBounds).to_string(),
            "dropped Thumbnail (tag 0x0201) at offset 1: addresses bytes outside the EXIF blob"
        );
        assert_eq!(
            Dropped::new(
                DroppedRegion::ThumbnailJpeg,
                42,
                DropReason::ThumbnailLengthMissing
            )
            .to_string(),
            "dropped Thumbnail (tag 0x0201) at offset 42: has no JPEGInterchangeFormatLength to \
             size the read"
        );
    }

    /// A region no tag addresses says so explicitly, in the same grammar as every other drop —
    /// rather than claiming tag `0x0000`, which is a real tag number (`GPSVersionID`) and would
    /// read as a fact about the source.
    #[test]
    fn a_drop_with_no_addressing_tag_says_so_in_the_same_grammar() {
        assert_eq!(
            Dropped::new(DroppedRegion::TrailingIfd, 120, DropReason::Unrepresentable).to_string(),
            "dropped TrailingIFD (tag none) at offset 120: parsed cleanly but has no place in the \
             EXIF model"
        );
    }

    /// A fresh report is empty and stays consistent with what has been recorded — `is_empty` is
    /// the verdict over the covered regions, so it must not be independent of the contents.
    #[test]
    fn a_report_is_empty_until_something_is_recorded() {
        let mut report = ReadReport::new();
        assert!(report.is_empty());
        assert_eq!(report.dropped(), &[]);

        let drop = Dropped::new(DroppedRegion::ExifIfd, 7, DropReason::Malformed);
        report.record(drop);
        assert!(!report.is_empty());
        assert_eq!(report.dropped(), &[drop]);
        assert_eq!(report.dropped()[0].region(), DroppedRegion::ExifIfd);
        assert_eq!(report.dropped()[0].offset(), 7);
        assert_eq!(report.dropped()[0].reason(), DropReason::Malformed);
    }
}
