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
//! addressed it, and **why** it went ([`DropReason`]). A well-formed blob reports nothing, so
//! `report.is_empty()` is the "this parse lost nothing" verdict.
//!
//! The granularity is the directory, not the individual tag: a single unparseable entry fails its
//! whole directory in the underlying TIFF/IFD reader, so the sub-IFD it sat in is what gets named.

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
    ThumbnailJpeg = 3,
}

impl DroppedRegion {
    /// The region's short name — the same spelling
    /// [`ExifError::InvalidIfd`](crate::ExifError::InvalidIfd) uses in strict mode.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::ExifIfd => "Exif",
            Self::GpsIfd => "GPS",
            Self::InteropIfd => "Interop",
            Self::ThumbnailJpeg => "Thumbnail",
        }
    }

    /// The tag whose value addressed this region.
    pub(crate) const fn tag(self) -> u16 {
        match self {
            Self::ExifIfd => EXIF_IFD_POINTER,
            Self::GpsIfd => GPS_IFD_POINTER,
            Self::InteropIfd => INTEROP_IFD_POINTER,
            Self::ThumbnailJpeg => ExifTag::JpegInterchangeFormat.tag_id(),
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
}

impl DropReason {
    /// The clause [`Dropped`]'s `Display` uses for this reason.
    const fn clause(self) -> &'static str {
        match self {
            Self::OutOfBounds => "addresses bytes outside the EXIF blob",
            Self::Malformed => "is not a well-formed directory",
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
    #[must_use]
    pub const fn tag(self) -> u16 {
        self.tag
    }

    /// The offset that tag carried, relative to the start of the TIFF stream (i.e. *after* any
    /// `Exif\0\0` marker) — the same frame of reference EXIF's own offsets use.
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
        write!(
            f,
            "dropped {} at tag {:#06x}, offset {}: {}",
            self.region.name(),
            self.tag,
            self.offset,
            self.reason.clause()
        )
    }
}

/// What a lenient parse discarded — empty when the blob was carried across in full.
///
/// Obtained from [`ExifReader::parse_with_report`](crate::ExifReader::parse_with_report) or
/// [`ExifReader::parse_from_with_report`](crate::ExifReader::parse_from_with_report). In
/// [`strict`](crate::ExifReader::strict) mode the first malformed region fails the parse instead,
/// so a report from a strict reader is always empty.
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

    /// Whether the parse lost nothing.
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
    /// the pointer back in the source directory.
    #[test]
    fn each_region_carries_the_tag_that_addresses_it() {
        for (region, tag) in [
            (DroppedRegion::ExifIfd, 0x8769),
            (DroppedRegion::GpsIfd, 0x8825),
            (DroppedRegion::InteropIfd, 0xA005),
            (DroppedRegion::ThumbnailJpeg, 0x0201),
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

    /// A fresh report is empty and stays consistent with what has been recorded — `is_empty` is
    /// the "this parse lost nothing" verdict, so it must not be independent of the contents.
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
