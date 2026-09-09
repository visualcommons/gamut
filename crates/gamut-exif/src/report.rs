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
//!   one, with no signal this crate can observe (issue #528);
//! * a single unparseable **entry** fails its whole directory rather than being skipped, so the
//!   report's granularity is the directory, never the individual tag (issue #521).
//!
//! Bytes that no parsed structure ever claimed are likewise not reported; that verdict would need
//! `gamut-ifd`'s audit engine and is tracked in #521.

use core::fmt;

use crate::exif::{EXIF_IFD_POINTER, GPS_IFD_POINTER, INTEROP_IFD_POINTER};
use crate::tag::ExifTag;

/// The [`Dropped::tag`] value for a region that no tag addresses.
///
/// Zero is a real tag number in a GPS directory (`GPSVersionID`), but never a *pointer* tag, and
/// [`Dropped::tag`] only ever carries a pointer or offset tag — so it is unambiguous here.
const NO_TAG: u16 = 0;

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
    ThumbnailJpeg = 3,
    /// A top-level directory past the 1st IFD.
    ///
    /// EXIF defines exactly two: the 0th IFD (primary image) and the 1st (thumbnail). A stream
    /// whose next-IFD chain runs on has more, and the [`Exif`](crate::Exif) model has nowhere to
    /// put them — so they parse cleanly and are then discarded. No tag addresses one (the chain is
    /// followed through the structural next-IFD pointer), so [`Dropped::tag`] is `0`.
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

    /// The tag whose value addressed this region, or [`NO_TAG`] when none does.
    pub(crate) const fn tag(self) -> u16 {
        match self {
            Self::ExifIfd => EXIF_IFD_POINTER,
            Self::GpsIfd => GPS_IFD_POINTER,
            Self::InteropIfd => INTEROP_IFD_POINTER,
            Self::ThumbnailJpeg => ExifTag::JpegInterchangeFormat.tag_id(),
            Self::TrailingIfd => NO_TAG,
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
}

impl DropReason {
    /// The clause [`Dropped`]'s `Display` uses for this reason.
    const fn clause(self) -> &'static str {
        match self {
            Self::OutOfBounds => "addresses bytes outside the EXIF blob",
            Self::Malformed => "is not a well-formed directory",
            Self::Unrepresentable => "parsed cleanly but has no place in the EXIF model",
        }
    }
}

/// One region a lenient parse discarded, named by where it was and why it went.
///
/// `Copy` plain data reachable through accessors, so it is representable across an FFI boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Dropped {
    region: DroppedRegion,
    tag: u16,
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
    /// `0` when no tag addresses the region, which today means only
    /// [`DroppedRegion::TrailingIfd`]: a top-level directory is reached through the structural
    /// next-IFD pointer, not through a tag.
    #[must_use]
    pub const fn tag(self) -> u16 {
        self.tag
    }

    /// The offset of the discarded region, relative to the start of the TIFF stream (i.e. *after*
    /// any `Exif\0\0` marker) — the same frame of reference EXIF's own offsets use.
    ///
    /// For a sub-IFD or the thumbnail bytes this is the value the addressing tag carried; for a
    /// [`TrailingIfd`](DroppedRegion::TrailingIfd) it is the directory's own position in the
    /// stream.
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
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = self.region.name();
        let clause = self.reason.clause();
        if self.tag == NO_TAG {
            write!(f, "dropped {name} at offset {}: {clause}", self.offset)
        } else {
            write!(
                f,
                "dropped {name} at tag {:#06x}, offset {}: {clause}",
                self.tag, self.offset
            )
        }
    }
}

/// What a lenient parse discarded, over the regions [`DroppedRegion`] enumerates.
///
/// Obtained from [`ExifReader::parse_with_report`](crate::ExifReader::parse_with_report) or
/// [`ExifReader::parse_from_with_report`](crate::ExifReader::parse_from_with_report). In
/// [`strict`](crate::ExifReader::strict) mode the first *malformed* region fails the parse instead,
/// so a strict report can still be non-empty only for regions strictness does not reject (a
/// trailing directory is discarded either way).
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
    /// the pointer back in the source directory — and a region no tag addresses reports `0`.
    #[test]
    fn each_region_carries_the_tag_that_addresses_it() {
        for (region, tag) in [
            (DroppedRegion::ExifIfd, 0x8769),
            (DroppedRegion::GpsIfd, 0x8825),
            (DroppedRegion::InteropIfd, 0xA005),
            (DroppedRegion::ThumbnailJpeg, 0x0201),
            (DroppedRegion::TrailingIfd, NO_TAG),
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
            "dropped GPS at tag 0x8825, offset 65535: addresses bytes outside the EXIF blob"
        );
        assert_eq!(
            Dropped::new(DroppedRegion::ExifIfd, 26, DropReason::Malformed).to_string(),
            "dropped Exif at tag 0x8769, offset 26: is not a well-formed directory"
        );
        assert_eq!(
            Dropped::new(DroppedRegion::InteropIfd, 8, DropReason::Malformed).to_string(),
            "dropped Interop at tag 0xa005, offset 8: is not a well-formed directory"
        );
        assert_eq!(
            Dropped::new(DroppedRegion::ThumbnailJpeg, 1, DropReason::OutOfBounds).to_string(),
            "dropped Thumbnail at tag 0x0201, offset 1: addresses bytes outside the EXIF blob"
        );
    }

    /// A region no tag addresses renders without a tag clause, rather than claiming tag `0x0000` —
    /// which is a real tag number (`GPSVersionID`) and would read as a fact about the source.
    #[test]
    fn a_drop_with_no_addressing_tag_renders_without_one() {
        assert_eq!(
            Dropped::new(DroppedRegion::TrailingIfd, 120, DropReason::Unrepresentable).to_string(),
            "dropped TrailingIFD at offset 120: parsed cleanly but has no place in the EXIF model"
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
