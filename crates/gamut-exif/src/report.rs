//! What a lenient parse discarded.
//!
//! [`ExifReader`](crate::ExifReader) is lenient by default: a malformed Exif/GPS/Interop sub-IFD or
//! an unreadable thumbnail range or strip set is dropped so the rest of the blob still parses. That is the
//! right default for real-world files, but on its own it is *silent* — a caller cannot tell a blob
//! that never carried GPS from one whose GPS pointer was dangling.
//!
//! A [`ReadReport`], returned by
//! [`ExifReader::parse_with_report`](crate::ExifReader::parse_with_report), names each discarded
//! region: **where** it was (a [`DroppedRegion`] and the tag that addressed it), **what offset**
//! addressed it, and **why** it went ([`DropReason`]).
//!
//! Two opt-in options on the reader deepen it:
//!
//! * [`recover_entries`](crate::ExifReader::recover_entries) makes the granularity the **entry**:
//!   a directory with one unreadable entry survives minus that entry, named by a
//!   [`DroppedEntry`] in [`dropped_entries`](ReadReport::dropped_entries), instead of failing whole;
//! * [`audit_bytes`](crate::ExifReader::audit_bytes) adds a **byte-completeness verdict**,
//!   [`byte_audit`](ReadReport::byte_audit): `gamut-ifd`'s audit engine classifying every byte of
//!   the TIFF stream, or naming precisely the ones no structure claims.
//!
//! Whatever the options, a **duplicate tag** within a surviving directory is named too: the model
//! keeps one value per tag — the last occurrence — so every earlier entry for that tag is a
//! [`DroppedEntry`] whose [`reason`](DroppedEntry::reason) is [`EntryDropReason::Shadowed`]. TIFF
//! 6.0 §2 gives a directory one entry per tag, so such a file is out of spec, but the signal is a
//! report entry rather than a [`strict`](crate::ExifReader::strict)-mode rejection.
//!
//! # What this report does and does not claim
//!
//! It covers exactly the regions [`DroppedRegion`] enumerates — the three pointer-addressed
//! sub-IFDs, the thumbnail's JPEG bytes and strips, and any top-level directory past the 1st IFD —
//! and the
//! entries of every directory the model keeps: each one a duplicate tag shadowed, and, under
//! `recover_entries`, each one whose value could not be read. Within that set it is complete:
//! nothing in it is discarded without an entry.
//!
//! The drop lists are **not** a byte-completeness verdict, and [`is_empty`](ReadReport::is_empty)
//! does not mean "every source byte reached the model": a gap between structures, a trailer, or a
//! region no pointer reaches is not a *discard* of anything the parse read, so the drop lists have
//! nothing to say about it. That is the byte audit's question. The converse holds too — the audit
//! cannot stand in for the drop lists: a shadowed entry's record lies inside its directory's body,
//! so its bytes count as claimed, and only [`EntryDropReason::Shadowed`] names it.

use core::fmt;

use gamut_ifd::SegmentReport;

use crate::exif::{EXIF_IFD_POINTER, GPS_IFD_POINTER, INTEROP_IFD_POINTER};
use crate::tag::{ExifTag, IfdKind};

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
    /// offset has no length beside it ([`DropReason::ThumbnailLengthMissing`]): an offset with
    /// nothing to size it addresses bytes that cannot be read, which is a loss rather than an
    /// absent thumbnail. That rule is structural and unconditional here — see
    /// [`ThumbnailLengthMissing`](DropReason::ThumbnailLengthMissing) — and is *not* derived from
    /// the pair's support level, which Exif 3.0 §4.6.9.2 Table 21 states only per
    /// thumbnail-format column. That axis is not the two-valued `Compression` tag, which this
    /// *reader* does not consult (a parsed [`Thumbnail`](crate::Thumbnail) does expose it, through
    /// [`compression`](crate::Thumbnail::compression)).
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
    /// The 1st IFD's uncompressed-thumbnail strips, addressed by `StripOffsets` (`0x0111`) and
    /// sized by `StripByteCounts` (`0x0117`). The thumbnail's own directory survives, minus its
    /// `StripOffsets`; only the strips' bytes are lost.
    ///
    /// Reported when an integer `StripOffsets` names at least one strip and the set cannot be
    /// read: [`DropReason::StripsUnsized`] when `StripByteCounts` does not give one count per
    /// strip, [`DropReason::OutOfBounds`] when a strip's range is not wholly inside the blob, and
    /// [`DropReason::StripsOverlap`] when every strip is inside the blob but their total length is
    /// not. The [offset](Dropped::offset) is the first out-of-bounds strip's, or — for the two
    /// defects of the set as a whole — the first strip's. A `StripOffsets` that names no
    /// strip — not an integer array, or an empty one — addresses no bytes, so nothing is reported
    /// for it. Like [`ThumbnailJpeg`](Self::ThumbnailJpeg), the rule is structural and does not
    /// consult `Compression`.
    ThumbnailStrips = 5,
}

impl DroppedRegion {
    /// The region's short name, as it appears in [`Dropped`]'s `Display`.
    ///
    /// For the three sub-IFDs this is also the spelling
    /// [`ExifError::InvalidIfd`](crate::ExifError::InvalidIfd) uses in strict mode. The others
    /// have no such error: an unreadable thumbnail range or strip set is a strict
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
            Self::ThumbnailStrips => "ThumbnailStrips",
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
            Self::ThumbnailStrips => Some(ExifTag::StripOffsets.tag_id()),
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
    ///
    /// Recorded whatever the thumbnail's `Compression` says, because the reason it is a loss is
    /// **readability**: the read has no length, not a tag is missing where the spec requires one.
    /// Exif 3.0 §4.6.9.2 Table 21 gives the pair's support level *per column*: mandatory under
    /// **Compressed**, and `N` (not allowed to record) under all three uncompressed columns.
    ///
    /// **Conformance** is a separate axis, and it is only what shows the unconditional rule names
    /// nothing it should not: the two tags carry the *same* level in every column, so an offset
    /// with no length is non-conformant under all four and there is no conformant 1st IFD this
    /// rule wrongly names. It is deliberately **not** conditioned on `Compression` (#574): the
    /// reader strips the offset from the model whatever became of the bytes, so an offset it
    /// declined to name under an uncompressed `Compression` would be a discard with no entry —
    /// exactly what this report promises never happens inside [`DroppedRegion::ThumbnailJpeg`].
    ///
    /// The mirror case — a length with no offset — is genuinely asymmetric and not merely
    /// unreached: an offset with no length *addresses bytes*, so something is lost, while a
    /// length with no offset addresses nothing, so nothing is. It is therefore neither reported
    /// nor rejected in strict mode, whatever `Compression` says (#574); under **Compressed** it is
    /// as non-conformant as a 1st IFD with neither tag, and strictness rejects unreadable regions,
    /// not Table 21 support levels.
    ThumbnailLengthMissing = 3,
    /// The 1st IFD's `StripOffsets` named strips that its `StripByteCounts` does not size one by
    /// one: the counts are absent, not an integer array, or a different number of entries. Only
    /// ever recorded for [`DroppedRegion::ThumbnailStrips`], and named for that one site for the
    /// reason [`ThumbnailLengthMissing`](Self::ThumbnailLengthMissing) gives.
    ///
    /// Exif 3.0 §4.6.5.2.1 and §4.6.5.2.3 give both tags the same count (`StripsPerImage`, or
    /// `SamplesPerPixel * StripsPerImage` when planar), and §4.6.9.2 Table 21 the same support
    /// level in every column, so no conformant 1st IFD carries one without the other matching it.
    StripsUnsized = 4,
    /// The 1st IFD's strips each lie inside the blob, but their total length does not, so at least
    /// two of them overlap. Only ever recorded for [`DroppedRegion::ThumbnailStrips`].
    ///
    /// The reader bounds what it allocates for the strips by the blob's own length — each strip
    /// fitting alone would still let `n` overlapping strips cost `n` times the blob — so such a
    /// set is not read. A conformant thumbnail's strips are disjoint ranges of the blob, so no
    /// conformant input reaches this; what the bound stops accepting is a set overlapping by more
    /// than the blob's length.
    StripsOverlap = 5,
}

impl DropReason {
    /// The clause [`Dropped`]'s `Display` uses for this reason.
    const fn clause(self) -> &'static str {
        match self {
            Self::OutOfBounds => "addresses bytes outside the EXIF blob",
            Self::Malformed => "is not a well-formed directory",
            Self::Unrepresentable => "parsed cleanly but has no place in the EXIF model",
            Self::ThumbnailLengthMissing => "has no JPEGInterchangeFormatLength to size the read",
            Self::StripsUnsized => "has no StripByteCounts entry to size each strip",
            Self::StripsOverlap => "names strips whose total length exceeds the EXIF blob",
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
    /// `JPEGInterchangeFormat` for the thumbnail bytes, `StripOffsets` for the thumbnail strips.
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
    /// For a sub-IFD or the thumbnail bytes this is the value the addressing tag carried (for the
    /// strips, one entry of the `StripOffsets` array —
    /// [`ThumbnailStrips`](DroppedRegion::ThumbnailStrips) says which); for a
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

/// Why a single directory entry was discarded while the rest of its directory survived.
///
/// Fieldless with an explicit `repr` and append-only discriminants; `#[non_exhaustive]` so reasons
/// can be added post-1.0 without a breaking change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
#[non_exhaustive]
pub enum EntryDropReason {
    /// The entry's value could not be read, and a
    /// [`recover_entries`](crate::ExifReader::recover_entries) parse skipped it: its count
    /// overflows, its value offset or span lies outside the TIFF stream, or it does not decode as
    /// its declared type (an `ASCII` field that is not UTF-8). Re-reading the entry record at
    /// [`DroppedEntry::offset`] through [`gamut_ifd::IfdReader::value`] reproduces the exact error.
    ///
    /// Never recorded without that option, whose absence makes such an entry fail its whole
    /// directory instead.
    Unreadable = 0,
    /// A later entry in the same directory repeats this entry's tag. The directory holds one value
    /// per tag and keeps the **last** occurrence (`gamut-ifd`'s rule; libtiff keeps the first), so
    /// this one is discarded — one record per shadowed entry, so a tag written three times yields
    /// two.
    ///
    /// Recorded in every mode, [`strict`](crate::ExifReader::strict) included. A duplicate tag
    /// breaks TIFF 6.0 §2's one-entry-per-tag rule, but every entry of it was read, so strictness —
    /// which rejects what cannot be read — has no grounds to refuse a file over it. The shadowed
    /// value was still decoded: a malformed one fails (or, under `recover_entries`, is skipped as
    /// [`Unreadable`](Self::Unreadable) and then shadows nothing) exactly as any other entry does.
    ///
    /// **Round trip.** The [`Exif`](crate::Exif) holds only the survivor, so
    /// [`to_bytes`](crate::Exif::to_bytes) writes the directory with one entry for the tag; this
    /// record is the only trace that the others existed.
    Shadowed = 1,
}

impl EntryDropReason {
    /// The clause [`DroppedEntry`]'s `Display` uses for this reason.
    const fn clause(self) -> &'static str {
        match self {
            Self::Unreadable => "its value could not be read",
            Self::Shadowed => "a later entry repeats its tag",
        }
    }
}

/// One directory entry a parse discarded while the rest of its directory survived, named by the
/// directory, the tag, the entry record's offset, and [why](EntryDropReason).
///
/// The report names the entry, not the defect behind it: for an
/// [`Unreadable`](EntryDropReason::Unreadable) entry, re-reading the record at
/// [`offset`](Self::offset) reproduces the exact error.
///
/// `Copy` plain data reachable through accessors, so it is representable across an FFI boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DroppedEntry {
    ifd: IfdKind,
    tag: u16,
    offset: u64,
    reason: EntryDropReason,
}

impl DroppedEntry {
    /// Records that `ifd`'s entry for `tag`, whose record sits at `offset`, was discarded for
    /// `reason`.
    pub(crate) const fn new(ifd: IfdKind, tag: u16, offset: u64, reason: EntryDropReason) -> Self {
        Self {
            ifd,
            tag,
            offset,
            reason,
        }
    }

    /// The directory the entry belonged to — which still parsed, minus this entry.
    #[must_use]
    pub const fn ifd(self) -> IfdKind {
        self.ifd
    }

    /// The discarded entry's tag. Look it up with
    /// [`ExifTag::from_id`](crate::ExifTag::from_id) against [`ifd`](Self::ifd).
    #[must_use]
    pub const fn tag(self) -> u16 {
        self.tag
    }

    /// The offset of the discarded entry's own 12-byte record, relative to the start of the TIFF
    /// stream — the frame [`Dropped::offset`] uses, not the frame of the crate's error messages.
    /// For a [`Shadowed`](EntryDropReason::Shadowed) entry this tells it apart from the survivor
    /// that shares its tag.
    #[must_use]
    pub const fn offset(self) -> u64 {
        self.offset
    }

    /// Why the entry was discarded.
    #[must_use]
    pub const fn reason(self) -> EntryDropReason {
        self.reason
    }
}

impl fmt::Display for DroppedEntry {
    /// `dropped entry (tag <t>) in <directory> at offset <n>: <reason>` — the same grammar as
    /// [`Dropped`]'s, with the directory the entry was discarded from.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let ifd = match self.ifd {
            IfdKind::Image => "0th IFD",
            IfdKind::Exif => "Exif IFD",
            IfdKind::Gps => "GPS IFD",
            IfdKind::Interop => "Interop IFD",
            IfdKind::Thumbnail => "1st IFD",
        };
        write!(
            f,
            "dropped entry (tag {:#06x}) in {ifd} at offset {}: {}",
            self.tag,
            self.offset,
            self.reason.clause()
        )
    }
}

/// What a lenient parse discarded, over the regions [`DroppedRegion`] enumerates and the entries
/// surviving directories lost — every [shadowed](EntryDropReason::Shadowed) duplicate, and under
/// [`recover_entries`](crate::ExifReader::recover_entries) every
/// [unreadable](EntryDropReason::Unreadable) entry — plus the opt-in byte-completeness verdict of
/// [`audit_bytes`](crate::ExifReader::audit_bytes).
///
/// Obtained from [`ExifReader::parse_with_report`](crate::ExifReader::parse_with_report) or
/// [`ExifReader::parse_from_with_report`](crate::ExifReader::parse_from_with_report). In
/// [`strict`](crate::ExifReader::strict) mode the first *malformed* region fails the parse instead
/// of being reported — but a strict report is **not** therefore always empty:
/// [`DroppedRegion::TrailingIfd`] is well-formed and merely unrepresentable, so strictness has no
/// grounds to reject it and it is reported in both modes.
///
/// Read the module documentation for what the drop lists deliberately do **not** cover: they are
/// not a byte-completeness verdict — [`byte_audit`](Self::byte_audit) is.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReadReport {
    dropped: Vec<Dropped>,
    entries: Vec<DroppedEntry>,
    byte_audit: Option<SegmentReport>,
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

    /// Every entry discarded from a directory the model kept, directory by directory in the order
    /// the reader met them — the 0th IFD, the 1st IFD, then the Exif, GPS and Interop sub-IFDs.
    /// Within one directory, its [`Unreadable`](EntryDropReason::Unreadable) entries come first,
    /// then its [`Shadowed`](EntryDropReason::Shadowed) ones, each group in on-disk order.
    ///
    /// Unreadable entries appear only under
    /// [`recover_entries`](crate::ExifReader::recover_entries); shadowed ones in every mode. A
    /// directory the parse dropped whole contributes none: it is named once, in
    /// [`dropped`](Self::dropped). Nor does a [`TrailingIfd`](DroppedRegion::TrailingIfd), which is
    /// discarded whole as well.
    #[must_use]
    pub fn dropped_entries(&self) -> &[DroppedEntry] {
        &self.entries
    }

    /// Whether anything this report's drop lists cover was discarded — a region of
    /// [`dropped`](Self::dropped) or an entry of [`dropped_entries`](Self::dropped_entries).
    ///
    /// **Not** a byte-completeness verdict — see the module documentation. Source bytes no
    /// structure claimed are [`byte_audit`](Self::byte_audit)'s question, and an audit is not a
    /// discard, so it never makes a report non-empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.dropped.is_empty() && self.entries.is_empty()
    }

    /// The byte-completeness verdict over the TIFF stream, when
    /// [`audit_bytes`](crate::ExifReader::audit_bytes) asked for one.
    ///
    /// [`SegmentReport::is_fully_classified`] answers "is every byte accounted for"; its
    /// [`unclaimed_spans`](SegmentReport::unclaimed_spans) name the ones no parsed structure
    /// claims. Offsets are relative to the start of the TIFF stream, like [`Dropped::offset`], so an
    /// `Exif\0\0` marker is outside the audited range.
    ///
    /// The verdict is `gamut-ifd`'s [`audit`](gamut_ifd::audit) walk of the stream, following the
    /// Exif, GPS and Interop pointers and the thumbnail's JPEG range: a second, structural reading
    /// of the same bytes, deliberately not this crate's parse. So it classifies a sub-IFD the parse
    /// dropped as unclassified bytes, and — the one case it cannot answer — it is `None` when the
    /// top-level chain has an entry no strict decode can read, which only a
    /// [`recover_entries`](crate::ExifReader::recover_entries) parse survives. `None` also when no
    /// audit was asked for.
    #[must_use]
    pub fn byte_audit(&self) -> Option<&SegmentReport> {
        self.byte_audit.as_ref()
    }

    /// Appends a discarded region.
    pub(crate) fn record(&mut self, dropped: Dropped) {
        self.dropped.push(dropped);
    }

    /// Appends a skipped entry.
    pub(crate) fn record_entry(&mut self, entry: DroppedEntry) {
        self.entries.push(entry);
    }

    /// Stores the byte-completeness verdict.
    pub(crate) fn set_byte_audit(&mut self, audit: Option<SegmentReport>) {
        self.byte_audit = audit;
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
            (DroppedRegion::ThumbnailStrips, Some(0x0111)),
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

    /// The strips render under their own region name and `StripOffsets`, with each strip-only
    /// reason as its own clause — an unsized set and an overlapping one are different repairs.
    #[test]
    fn the_rendered_strip_drop_names_its_region_and_reason() {
        assert_eq!(
            Dropped::new(DroppedRegion::ThumbnailStrips, 8, DropReason::StripsUnsized).to_string(),
            "dropped ThumbnailStrips (tag 0x0111) at offset 8: has no StripByteCounts entry to \
             size each strip"
        );
        assert_eq!(
            Dropped::new(DroppedRegion::ThumbnailStrips, 2, DropReason::StripsOverlap).to_string(),
            "dropped ThumbnailStrips (tag 0x0111) at offset 2: names strips whose total length \
             exceeds the EXIF blob"
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

    /// A discarded entry alone makes a report non-empty — it is a discard like any region — and is
    /// listed with the directory, tag, offset and reason it was recorded with.
    #[test]
    fn a_discarded_entry_alone_makes_a_report_non_empty() {
        let mut report = ReadReport::new();
        let entry = DroppedEntry::new(IfdKind::Gps, 2, 40, EntryDropReason::Shadowed);
        report.record_entry(entry);
        assert!(!report.is_empty());
        assert_eq!(report.dropped(), &[]);
        assert_eq!(report.dropped_entries(), &[entry]);
        assert_eq!(
            (entry.ifd(), entry.tag(), entry.offset(), entry.reason()),
            (IfdKind::Gps, 2, 40, EntryDropReason::Shadowed)
        );
    }

    /// A byte audit is a verdict, not a discard: storing one leaves the report empty.
    #[test]
    fn a_byte_audit_alone_leaves_a_report_empty() {
        let mut report = ReadReport::new();
        report.set_byte_audit(Some(gamut_ifd::SegmentMap::new(0).finish(None)));
        assert!(report.is_empty());
        assert!(report.byte_audit().is_some());
    }

    /// Each directory renders under its own name, in [`Dropped`]'s grammar — the strings are part
    /// of the contract, and the 0th/1st IFDs are named as the spec names them.
    #[test]
    fn the_rendered_entry_names_its_directory_tag_and_offset() {
        for (ifd, name) in [
            (IfdKind::Image, "0th IFD"),
            (IfdKind::Exif, "Exif IFD"),
            (IfdKind::Gps, "GPS IFD"),
            (IfdKind::Interop, "Interop IFD"),
            (IfdKind::Thumbnail, "1st IFD"),
        ] {
            assert_eq!(
                DroppedEntry::new(ifd, 0x010F, 22, EntryDropReason::Unreadable).to_string(),
                format!(
                    "dropped entry (tag 0x010f) in {name} at offset 22: its value could not be read"
                )
            );
        }
    }

    /// Each reason renders as its own clause in the one entry grammar — a shadowed duplicate is
    /// not described as an unreadable value, which would send a caller after the wrong defect.
    #[test]
    fn the_rendered_entry_states_its_reason() {
        assert_eq!(
            DroppedEntry::new(IfdKind::Exif, 0x9003, 26, EntryDropReason::Shadowed).to_string(),
            "dropped entry (tag 0x9003) in Exif IFD at offset 26: a later entry repeats its tag"
        );
    }
}
