//! Reading EXIF from a positioned byte source rather than a slice.
//!
//! EXIF is usually small, but the file it is embedded in need not be: a raw `.NEF`/`.CR3` can be
//! hundreds of megabytes whose EXIF is a few kilobytes near the front. [`gamut_ifd`]'s
//! [`IfdReader`] already reads a TIFF stream through the positioned [`ReadAt`] trait, fetching only
//! the directory bodies and the values they reference, so this module lifts
//! [`ExifReader`](crate::ExifReader) onto the same source type: open a file, hand the reader a
//! [`gamut_ifd::StreamSource`], and pull the EXIF out without loading the image.
//!
//! This is the crate's **one** parse engine — [`ExifReader::parse`](crate::ExifReader::parse) is
//! the `&[u8]` case of it (`&[u8]` implements [`ReadAt`]), so the slice and streaming paths cannot
//! drift. It is deliberately synchronous: an async caller drives a [`ReadAt`] source itself, which
//! keeps a runtime dependency out of a crate that has none.

use std::collections::BTreeSet;

use gamut_core::ErrorKind;
use gamut_ifd::{
    AuditFinding, AuditSpec, Ifd, IfdReader, RawIfd, ReadAt, SegmentReport, tags as ifd_tags,
};

use crate::error::{ExifError, Result};
use crate::exif::{EXIF_IFD_POINTER, Exif, MARKER};
use crate::reader::ExifReader;
use crate::report::{
    DropReason, Dropped, DroppedEntry, DroppedRegion, EntryDropReason, ReadReport,
};
use crate::tag::{ExifTag, IfdKind};
use crate::thumbnail::Thumbnail;

impl ExifReader {
    /// Parses EXIF from a positioned byte source, reading only the parts it needs.
    ///
    /// The streaming twin of [`parse`](Self::parse), which is this method over a `&[u8]`. `source`
    /// is taken by value; `&mut S` and `&mut dyn ReadAt` both implement [`ReadAt`], so a caller
    /// that must keep its source can pass a borrow, and a caller that needs dynamic dispatch can
    /// erase the type.
    ///
    /// ```no_run
    /// use std::fs::File;
    ///
    /// use gamut_exif::ExifReader;
    /// use gamut_ifd::StreamSource;
    ///
    /// // The EXIF of a large raw file, without reading the image.
    /// let mut file = File::open("capture.dng")?;
    /// let exif = ExifReader::new().parse_from(StreamSource::new(&mut file))?;
    /// println!("{:?}", exif.make());
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns [`ExifError::MissingMarker`] when the marker is required but absent, an
    /// [`ExifError::Ifd`] when the TIFF stream is malformed or the source fails, or (in
    /// [`strict`](Self::strict) mode) [`ExifError::InvalidIfd`] /
    /// [`ExifError::BadThumbnail`] when a sub-IFD pointer or thumbnail range is unusable.
    ///
    /// An offset inside an error message is a position in `source` — the byte source the caller
    /// handed in — so for a marked source it counts the six-byte `Exif\0\0` marker. That is
    /// deliberately a different frame from [`Dropped::offset`](crate::Dropped::offset), which is
    /// relative to the start of the TIFF stream and therefore six smaller for the same position.
    /// It matters most here: a caller streaming from a file is the one likeliest to correlate an
    /// error offset against bytes on disk, and it can do so directly only for the error frame —
    /// a report offset must have the source's own start (and the marker) added back first.
    pub fn parse_from<S: ReadAt>(&self, source: S) -> Result<Exif> {
        self.parse_source(source, &mut ReadReport::new())
    }

    /// Parses EXIF from a positioned byte source and reports what a lenient parse discarded.
    ///
    /// The streaming twin of [`parse_with_report`](Self::parse_with_report). See [`ReadReport`].
    ///
    /// The two frames of [`parse_from`](Self::parse_from) meet here: every
    /// [`Dropped::offset`](crate::Dropped::offset) in the returned report is relative to the start
    /// of the TIFF stream, while an offset in a returned [`ExifError`] is a position in `source`
    /// and includes any `Exif\0\0` marker. A caller that renders both beside each other must
    /// normalise one of them. They *meet* rather than always arrive together: the return is a sum
    /// type, so a source whose strict-fatal defect is reached after a reportable one yields the
    /// error alone and no report.
    ///
    /// # Errors
    ///
    /// As [`parse_from`](Self::parse_from).
    pub fn parse_from_with_report<S: ReadAt>(&self, mut source: S) -> Result<(Exif, ReadReport)> {
        let mut report = ReadReport::new();
        let exif = self.parse_source(&mut source, &mut report)?;
        if self.audit_bytes {
            report.set_byte_audit(self.byte_audit(&mut source)?);
        }
        Ok((exif, report))
    }

    /// The byte-completeness verdict over the TIFF stream in `source`: `gamut-ifd`'s audit walk,
    /// following the three EXIF pointer tags. `None` when the walk cannot read the top-level chain
    /// strictly — after a successful parse, only an unreadable entry anywhere in that chain under
    /// [`recover_entries`](Self::recover_entries) does that: one it skipped in the 0th or 1st IFD,
    /// or one in a trailing (3rd or later) directory, which the recovering parse reads but never
    /// decodes and so names no skip for. A transport failure propagates.
    fn byte_audit<S: ReadAt>(&self, mut source: S) -> Result<Option<SegmentReport>> {
        let base = self.tiff_base(&mut source)?;
        match gamut_ifd::audit(source.rebased(base), &ExifAuditSpec) {
            Ok(audit) => Ok(Some(audit.report)),
            Err(e) if e.kind() == ErrorKind::InvalidInput => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Whether an unreadable entry is skipped rather than failing its directory: asked for, and
    /// not overruled by [`strict`](Self::strict).
    fn recovers(&self) -> bool {
        self.recover_entries && !self.strict
    }

    /// Decodes one directory the model keeps, with the findings naming what the decode discarded
    /// from it: every shadowed duplicate-tag entry, and — when [`recovers`](Self::recovers) — every
    /// entry whose value could not be read. Every directory this crate keeps is decoded here, so
    /// no decode drops an entry without [`record_entries`] seeing it.
    fn decode<S: ReadAt>(
        &self,
        reader: &mut IfdReader<S>,
        raw: &RawIfd,
    ) -> gamut_core::Result<(Ifd, Vec<AuditFinding>)> {
        if self.recovers() {
            reader.decode_ifd_lenient(raw)
        } else {
            reader.decode_ifd_reporting(raw)
        }
    }

    /// The top-level chain: the 0th and 1st IFDs decoded through [`decode`](Self::decode), each
    /// discarded entry recorded against its directory, plus the offsets of every trailing
    /// directory for [`record_trailing`].
    ///
    /// The chain is walked once, through [`IfdReader::ifds`] — the same loop and runaway guards as
    /// [`IfdReader::read_file`], whose directory-body failures stay fatal here as they are there.
    /// The walk reads every directory body before decoding any, so a file with both a broken chain
    /// and an unreadable value in an earlier directory reports the chain's error where `read_file`
    /// would report the value's; both are the same [`ExifError::Ifd`] kind.
    ///
    /// A trailing directory's entries are never reported: the model discards it whole, as
    /// [`DroppedRegion::TrailingIfd`]. Without recovery it is still decoded, so an unreadable value
    /// in it stays fatal exactly as under `read_file`; under recovery it is not, since a value of
    /// it has nothing to recover into.
    fn top_level<S: ReadAt>(
        &self,
        reader: &mut IfdReader<S>,
        report: &mut ReadReport,
    ) -> Result<(Vec<Ifd>, Vec<u64>)> {
        let raws = reader.ifds().collect::<gamut_core::Result<Vec<RawIfd>>>()?;
        if raws.is_empty() {
            // A chain with no directory at all: `read_file` owns that error.
            reader.read_file()?;
        }
        let mut pages = Vec::with_capacity(2);
        for (raw, kind) in raws.iter().zip([IfdKind::Image, IfdKind::Thumbnail]) {
            let (ifd, findings) = self.decode(reader, raw)?;
            record_entries(&findings, kind, report);
            pages.push(ifd);
        }
        let trailing = &raws[pages.len()..];
        if !self.recovers() {
            for raw in trailing {
                reader.decode_ifd(raw)?;
            }
        }
        Ok((pages, trailing.iter().map(|raw| raw.offset).collect()))
    }

    /// The crate's one parse engine: marker handling, the top-level chain, the thumbnail, and the
    /// three pointer-addressed sub-IFDs, recording into `report` whatever leniency discards.
    pub(crate) fn parse_source<S: ReadAt>(
        &self,
        mut source: S,
        report: &mut ReadReport,
    ) -> Result<Exif> {
        let base = self.tiff_base(&mut source)?;
        // Everything below addresses the TIFF stream, so offsets read out of it — and the offsets
        // this crate hands back — stay in EXIF's own frame of reference.
        let mut reader = IfdReader::open(source.rebased(base))?;
        let order = reader.order();

        let (pages, trailing) = self.top_level(&mut reader, report)?;
        let mut ifds = pages.into_iter();
        let mut image = ifds.next().ok_or(ExifError::Truncated)?;
        // The next-IFD chain's second entry is the thumbnail directory (1st IFD), if any.
        let thumbnail = match ifds.next() {
            Some(ifd) => Some(self.read_thumbnail(ifd, &mut reader, report)?),
            None => None,
        };
        // EXIF defines exactly two top-level directories, so anything further down the chain has
        // nowhere to go in the model. Name it rather than letting the iterator drop it silently.
        record_trailing(trailing, report);

        // The Exif sub-IFD's own offset, captured before `follow` strips the pointer: the
        // maker-note pin needs the note value's absolute source position.
        let exif_ifd_at = image.get_u32(EXIF_IFD_POINTER).map(u64::from);
        let exif = self.follow(
            &mut image,
            &mut reader,
            (DroppedRegion::ExifIfd, IfdKind::Exif),
            report,
        )?;
        let gps = self.follow(
            &mut image,
            &mut reader,
            (DroppedRegion::GpsIfd, IfdKind::Gps),
            report,
        )?;
        let maker_note_at = match (&exif, exif_ifd_at) {
            (Some(_), Some(at)) => maker_note_offset(&mut reader, at, report)?,
            _ => None,
        };

        // The Interoperability directory is reached from *inside* the Exif sub-IFD, not the 0th IFD.
        let (exif, interop) = match exif {
            Some(mut e) => {
                let interop = self.follow(
                    &mut e,
                    &mut reader,
                    (DroppedRegion::InteropIfd, IfdKind::Interop),
                    report,
                )?;
                (Some(e), interop)
            }
            None => (None, None),
        };

        Ok(Exif::from_parts(
            order,
            image,
            exif,
            gps,
            interop,
            thumbnail,
            maker_note_at,
        ))
    }

    /// The offset at which the TIFF stream starts in `source`: past the `Exif\0\0` marker when it
    /// is there, 0 when it is not.
    ///
    /// A source too short to hold the marker is treated as unmarked — the TIFF header that follows
    /// is longer than the marker, so such a source cannot parse either way.
    fn tiff_base<S: ReadAt>(&self, source: &mut S) -> Result<u64> {
        let mut head = [0u8; MARKER.len()];
        let marked = match source.read_exact_at(0, &mut head) {
            Ok(()) => head.as_slice() == MARKER,
            // Too few bytes to hold a marker: unmarked. Keyed on the error *kind*, never on the
            // source's length, so a slice keeps its exact behaviour while a transport failure
            // (a disk error, a dropped network mount) is not misread as "no marker".
            Err(e) if e.kind() == ErrorKind::InvalidInput => false,
            Err(e) => return Err(e.into()),
        };
        if marked {
            Ok(MARKER.len() as u64)
        } else if self.require_marker {
            Err(ExifError::MissingMarker)
        } else {
            Ok(0)
        }
    }

    /// Reads `region`'s pointer tag from `parent` and parses the sub-IFD it addresses, then removes
    /// the pointer (it is represented structurally, not as a data field).
    ///
    /// Returns `Ok(None)` when the pointer is absent, or — in lenient mode — when the pointed-at
    /// directory is malformed, in which case the drop is recorded in `report`. Either way the
    /// pointer is removed: preserving it would change what `to_bytes` emits for a malformed blob,
    /// which is beyond this crate's remit here — #419's "the pointer was lost too" is answered by
    /// *naming* the tag and offset in the report, not by keeping the entry.
    ///
    /// A failure that is not the *bytes* being wrong — a source whose transport failed — is
    /// propagated unchanged in both modes. Leniency exists to tolerate corrupt files, and
    /// reporting a structurally perfect directory as malformed because a disk read failed would be
    /// a lie about the file.
    ///
    /// `kind` names the directory for the entries [`decode`](Self::decode) discards from it; they
    /// are recorded only for a directory that survives.
    fn follow<S: ReadAt>(
        &self,
        parent: &mut Ifd,
        reader: &mut IfdReader<S>,
        (region, kind): (DroppedRegion, IfdKind),
        report: &mut ReadReport,
    ) -> Result<Option<Ifd>> {
        // A region no tag addresses has, by definition, no pointer to follow — so `Ok(None)` is
        // the answer, not a special case. Today `follow` is only ever called for the three
        // pointer-addressed sub-IFDs, all of which have one.
        let Some(ptr) = region.tag() else {
            return Ok(None);
        };
        let Some(offset) = parent.get_u32(ptr) else {
            return Ok(None);
        };
        let offset = u64::from(offset);
        let followed = reader.read_ifd(offset).and_then(|raw| {
            self.decode(reader, &raw).map(|(ifd, findings)| {
                record_entries(&findings, kind, report);
                ifd
            })
        });
        parent.remove(ptr);
        match followed {
            Ok(ifd) => Ok(Some(ifd)),
            // Not the file's fault: hand the transport failure back untouched.
            Err(e) if e.kind() != ErrorKind::InvalidInput => Err(e.into()),
            Err(_) if self.strict => Err(ExifError::InvalidIfd(region.name())),
            Err(_) => {
                let reason = address_reason(reader, offset)?;
                report.record(Dropped::new(region, offset, reason));
                Ok(None)
            }
        }
    }

    /// Builds a [`Thumbnail`] from the 1st IFD, fetching its JPEG bytes (from the
    /// `JPEGInterchangeFormat` offset / length) when the range is wholly inside the stream. In
    /// lenient mode an unusable range yields a thumbnail without bytes and a recorded drop; in
    /// strict mode it errors.
    ///
    /// An offset with no `JPEGInterchangeFormatLength` beside it addresses bytes nothing can size,
    /// so it is a loss rather than an absent thumbnail — the JPEG behind the offset is unreadable.
    /// A length with no offset addresses nothing at all, so nothing was dropped and nothing is
    /// reported. Both halves of that rule are structural and apply whatever the thumbnail's
    /// `Compression` says: Exif 3.0 §4.6.9.2 Table 21 gives the pair's support level *per
    /// thumbnail-format column* (mandatory under **Compressed**, `N` — not allowed to record —
    /// under all three uncompressed ones); that axis is not the two-valued `Compression` tag, which
    /// this reader does not consult.
    ///
    /// Both halves are decided, not pending (#574). Conditioning the offset rule on `Compression`
    /// would leave an uncompressed 1st IFD's offset stripped below with no report entry — a
    /// discard inside the region the report claims completeness over — or would have to keep the
    /// pointer, which `to_bytes` then re-emits leading nowhere (#548). Rejecting a lone length in
    /// strict mode would make strictness a Table 21 support-level check, which this reader is not
    /// anywhere else (it does not reject a `StripOffsets` under **Compressed**, also `N`), and a
    /// lone length addresses exactly what a **Compressed** 1st IFD with neither tag does: nothing.
    fn read_thumbnail<S: ReadAt>(
        &self,
        ifd: Ifd,
        reader: &mut IfdReader<S>,
        report: &mut ReadReport,
    ) -> Result<Thumbnail> {
        let ptr = ExifTag::JpegInterchangeFormat.tag_id();
        let offset = ifd.get_u32(ptr);
        let length = ifd.get_u32(ExifTag::JpegInterchangeFormatLength.tag_id());
        let jpeg = match (offset, length) {
            (Some(offset), Some(length)) => match read_range(reader, offset, length)? {
                Some(bytes) => Some(bytes),
                None if self.strict => {
                    return Err(ExifError::BadThumbnail("JPEG offset out of bounds"));
                }
                None => {
                    report.record(Dropped::new(
                        DroppedRegion::ThumbnailJpeg,
                        u64::from(offset),
                        DropReason::OutOfBounds,
                    ));
                    None
                }
            },
            (Some(_), None) if self.strict => {
                // States the structural fact — a range with no size — rather than naming a missing
                // mandatory tag: Table 21 makes the sibling mandatory only under `Compression =
                // Compressed`, and forbids recording it at all under the uncompressed columns.
                return Err(ExifError::BadThumbnail(
                    "JPEGInterchangeFormat offset with no length to size it",
                ));
            }
            (Some(offset), None) => {
                report.record(Dropped::new(
                    DroppedRegion::ThumbnailJpeg,
                    u64::from(offset),
                    DropReason::ThumbnailLengthMissing,
                ));
                None
            }
            (None, _) => None,
        };
        // The JPEGInterchangeFormat offset is structural — an address into the source blob, not a
        // property of the thumbnail — so it never enters the model, whatever became of the bytes
        // (mirroring how the sub-IFD pointer tags are stripped, and `Thumbnail::from_jpeg`, which
        // never stores one). When bytes were read the writer re-synthesises the offset; when they
        // were not (OutOfBounds, ThumbnailLengthMissing — both named in the report above) there is
        // nothing to address, so `to_bytes` emits no pointer rather than one the report already
        // says leads nowhere (#548). `JPEGInterchangeFormatLength` stays: it addresses nothing on
        // its own, and the reader ignores it without an offset.
        //
        // Removing unconditionally deletes the former `if jpeg.is_some()` guard and with it that
        // site's two condition mutants; the tests pinning the drop now kill nothing there.
        let mut ifd = ifd;
        ifd.remove(ptr);
        // `StripOffsets` is structural in the same way, and leaves the model on the same terms:
        // read strips are re-addressed by the writer, and unreadable ones — named in the report as
        // a `ThumbnailStrips` drop, or rejected in strict mode — leave nothing to address, so
        // `to_bytes` must not re-emit offsets the report says lead nowhere (#868, as #548 for the
        // JPEG pointer). A `StripOffsets` that names no strip (not an integer array, or an empty
        // one) addresses no bytes, so nothing is lost and nothing is reported; it is removed all
        // the same, as an unreadable `JPEGInterchangeFormat` is above.
        //
        // Removing unconditionally deletes the former `if strips.is_some()` guard and with it that
        // site's condition mutants.
        let strips = match read_strips(reader, &ifd)? {
            Strips::Absent => None,
            Strips::Read(strips) => Some(strips),
            Strips::Unreadable { why, .. } if self.strict => {
                return Err(ExifError::BadThumbnail(why));
            }
            Strips::Unreadable { offset, reason, .. } => {
                report.record(Dropped::new(
                    DroppedRegion::ThumbnailStrips,
                    u64::from(offset),
                    reason,
                ));
                None
            }
        };
        ifd.remove(ifd_tags::STRIP_OFFSETS);
        Ok(Thumbnail::from_parts(ifd, jpeg, strips))
    }
}

/// What the 1st IFD's `StripOffsets` / `StripByteCounts` pair yielded.
enum Strips {
    /// `StripOffsets` names no strip: it is absent, not an integer array, or empty. Nothing is
    /// addressed, so nothing is lost.
    Absent,
    /// Every strip, in `StripOffsets` order.
    Read(Vec<Vec<u8>>),
    /// `StripOffsets` addresses strips that cannot be read.
    Unreadable {
        /// The [`Dropped::offset`] to report: the strip that lies outside the stream, or the
        /// first strip's offset when the defect belongs to the set rather than to one strip.
        offset: u32,
        /// Why the set could not be read.
        reason: DropReason,
        /// The strict-mode [`ExifError::BadThumbnail`] message.
        why: &'static str,
    },
}

/// Fetches every strip of the 1st IFD's `StripOffsets` / `StripByteCounts` arrays (Exif 3.0
/// §4.6.9.2 Table 21, the uncompressed thumbnail columns).
///
/// Read whatever `Compression` says, for the reason `read_thumbnail` gives for the JPEG pair: the
/// rule is structural, not a Table 21 support-level check. An integer `StripOffsets` naming at
/// least one strip addresses bytes, so failing to read them is a loss
/// ([`Strips::Unreadable`]), checked in this order:
///
/// * [`DropReason::StripsUnsized`] — `StripByteCounts` is absent, not an integer array, or holds a
///   different number of entries. Exif 3.0 §4.6.5.2.1 and §4.6.5.2.3 give both tags the same
///   count, and Table 21 the same support level in every column, so no conformant 1st IFD has an
///   offset without its count.
/// * [`DropReason::OutOfBounds`] — a strip's range is not wholly inside the stream; the first such
///   strip's offset is reported.
/// * [`DropReason::StripsOverlap`] — every strip is inside the stream but their *total* length is
///   not. Like `read_range`, the bound comes before any allocation, since each strip alone fitting
///   would still let `n` overlapping strips allocate `n` times the stream. A conformant
///   thumbnail's strips are disjoint ranges of the blob, so no conformant input exceeds that
///   total; what it stops accepting is a 1st IFD whose strips overlap by more than the stream's
///   own length.
fn read_strips<S: ReadAt>(reader: &mut IfdReader<S>, ifd: &Ifd) -> Result<Strips> {
    let Some(offsets) = ifd.get_u32_vec(ifd_tags::STRIP_OFFSETS) else {
        return Ok(Strips::Absent);
    };
    let Some(&first) = offsets.first() else {
        return Ok(Strips::Absent);
    };
    let counts = match ifd.get_u32_vec(ifd_tags::STRIP_BYTE_COUNTS) {
        Some(counts) if counts.len() == offsets.len() => counts,
        _ => {
            return Ok(Strips::Unreadable {
                offset: first,
                reason: DropReason::StripsUnsized,
                why: "StripOffsets with no StripByteCounts entry to size each strip",
            });
        }
    };
    let len = reader.source_mut().len()?;
    let mut total = 0u64;
    for (&offset, &count) in offsets.iter().zip(&counts) {
        // Widened to 64 bits first: the sum of two `u32`s cannot overflow a `u64`.
        if u64::from(offset) + u64::from(count) > len {
            return Ok(Strips::Unreadable {
                offset,
                reason: DropReason::OutOfBounds,
                why: "strip range out of bounds",
            });
        }
        total += u64::from(count);
    }
    if total > len {
        return Ok(Strips::Unreadable {
            offset: first,
            reason: DropReason::StripsOverlap,
            why: "strips overlapping past the stream's length",
        });
    }
    let strips = offsets
        .into_iter()
        .zip(counts)
        .map(|(offset, length)| fetch(reader, offset, length))
        .collect::<Result<_>>()?;
    Ok(Strips::Read(strips))
}

/// Records each of `offsets` — the top-level directories past the 1st IFD, which parse cleanly but
/// have nowhere to go in the [`Exif`] model — as a [`DroppedRegion::TrailingIfd`], in chain order.
fn record_trailing(offsets: Vec<u64>, report: &mut ReadReport) {
    for offset in offsets {
        report.record(Dropped::new(
            DroppedRegion::TrailingIfd,
            offset,
            DropReason::Unrepresentable,
        ));
    }
}

/// Records every entry `findings` says a decode of directory `kind` discarded as a
/// [`DroppedEntry`], in the findings' order: an [`AuditFinding::UnreadableEntry`] as
/// [`EntryDropReason::Unreadable`], an [`AuditFinding::ShadowedEntry`] as
/// [`EntryDropReason::Shadowed`]. A directory decode emits no other finding.
fn record_entries(findings: &[AuditFinding], kind: IfdKind, report: &mut ReadReport) {
    for finding in findings {
        let (tag, offset, reason) = match *finding {
            AuditFinding::UnreadableEntry { tag, offset, .. } => {
                (tag, offset, EntryDropReason::Unreadable)
            }
            AuditFinding::ShadowedEntry { tag, offset, .. } => {
                (tag, offset, EntryDropReason::Shadowed)
            }
            _ => continue,
        };
        report.record_entry(DroppedEntry::new(kind, tag, offset, reason));
    }
}

/// The [`AuditSpec`] of an EXIF stream: the Exif, GPS and Interoperability pointers — not the
/// `SubIFDs` of [`gamut_ifd::tags::STANDARD_POINTER_TAGS`], which no EXIF directory defines and
/// this crate never follows — and the standard data extents, which cover the thumbnail's
/// `JPEGInterchangeFormat` range.
struct ExifAuditSpec;

impl AuditSpec for ExifAuditSpec {
    fn pointer_tags(&self) -> &[u16] {
        &[
            ifd_tags::EXIF_IFD,
            ifd_tags::GPS_INFO,
            ifd_tags::INTEROPERABILITY_IFD,
        ]
    }
}

/// Why an address that failed to parse failed: past the end of the stream, or inside it but
/// structurally bad. Separating the two is what makes a report actionable — a dangling pointer is
/// a different defect from a corrupt directory.
fn address_reason<S: ReadAt>(reader: &mut IfdReader<S>, offset: u64) -> Result<DropReason> {
    let len = reader.source_mut().len()?;
    if offset < len {
        Ok(DropReason::Malformed)
    } else {
        Ok(DropReason::OutOfBounds)
    }
}

/// Fetches `length` bytes at `offset`, or `None` when that range is not wholly inside the stream.
///
/// The bound is checked against the source's length *before* anything is allocated, so a hostile
/// `JPEGInterchangeFormatLength` cannot make the reader reserve more than the stream can hold.
fn read_range<S: ReadAt>(
    reader: &mut IfdReader<S>,
    offset: u32,
    length: u32,
) -> Result<Option<Vec<u8>>> {
    // Widened to 64 bits first: the sum of two `u32`s cannot overflow a `u64`.
    let end = u64::from(offset) + u64::from(length);
    if end > reader.source_mut().len()? {
        return Ok(None);
    }
    fetch(reader, offset, length).map(Some)
}

/// Fetches `length` bytes at `offset`. The caller has already bounded the range by the stream's
/// length, so the allocation is too.
fn fetch<S: ReadAt>(reader: &mut IfdReader<S>, offset: u32, length: u32) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; length as usize];
    reader
        .source_mut()
        .read_exact_at(u64::from(offset), &mut buf)?;
    Ok(buf)
}

/// The absolute offset of the Exif sub-IFD's out-of-line `MakerNote` value in the TIFF stream, or
/// `None` if the note is absent or inline.
///
/// The offset is taken from the **one** `MakerNote` entry the model holds, so the pin and the
/// note's bytes always describe the same record (#790). A directory may carry the tag more than
/// once, and its decode keeps only one occurrence: the last, or under
/// [`recover_entries`](ExifReader::recover_entries) the last *readable* one. Every other
/// occurrence the decode discarded — shadowed, or skipped as unreadable — is already named in
/// `report` as an Exif-IFD [`DroppedEntry`] at its record's offset, so the survivor is the entry
/// `report` does not name. Reading that answer off the decode's own findings, rather than
/// re-deriving "last" or "last readable" here, keeps the rule in one place. The match is by record
/// offset *within the Exif IFD*: a record offset is unique within one directory, but a hostile
/// file can overlap two directories so that one record is shadowed in one and the survivor in the
/// other.
///
/// A failure here is propagated rather than folded into `None`. The offset is what
/// [`ExifWriter`](crate::ExifWriter) uses to *pin* the note in place on a rewrite, so losing it
/// silently re-emits a vendor MakerNote unpinned — wrong bytes, no error, and nothing in the
/// report. No later read is guaranteed to resurface the failure either: `follow` returns before
/// reading at all when the GPS and Interop pointers are absent, which is the common case.
fn maker_note_offset<S: ReadAt>(
    reader: &mut IfdReader<S>,
    exif_ifd_at: u64,
    report: &ReadReport,
) -> Result<Option<u64>> {
    // Every error propagates, with no lenient arm — deliberately. This is reached only when
    // `follow` has already read and decoded this exact directory at this exact offset, so a
    // *deterministic* source cannot fail here for a reason the bytes explain: over such a source a
    // malformed-input arm is unreachable, and an unreachable arm is a branch no test can falsify.
    //
    // It is not unreachable in general. A `ReadAt` may answer differently on a second read — a file
    // rewritten underneath the reader is precisely the case `parse_from` exists to enable — and then
    // the directory really can fail here. A hard error is still the right answer for it: the pin is
    // what keeps a vendor MakerNote's TIFF-absolute internal offsets valid on a rewrite, so
    // continuing would re-emit the note unpinned, with wrong bytes and nothing in the report.
    let raw: RawIfd = reader.read_ifd(exif_ifd_at)?;
    // Collected once into a set: a hostile Exif IFD of 65 535 duplicate `MakerNote`s drops all but
    // one, and probing the report's list per candidate would make this pass quadratic.
    let discarded: BTreeSet<u64> = report
        .dropped_entries()
        .iter()
        .filter(|d| d.ifd() == IfdKind::Exif)
        .map(|d| d.offset())
        .collect();
    let Some(entry) = raw
        .entries
        .iter()
        .find(|e| e.tag == ifd_tags::MAKER_NOTE && !discarded.contains(&e.offset))
    else {
        return Ok(None);
    };
    Ok(reader.value_offset(entry))
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use gamut_ifd::{ByteOrder, StreamSource, TiffFile, Value, Variant, write};

    use super::*;
    use crate::exif::{GPS_IFD_POINTER, INTEROP_IFD_POINTER};

    /// A minimal marked EXIF blob whose 0th IFD carries `Make`.
    fn blob() -> Vec<u8> {
        let mut image = Ifd::new();
        image.set(0x010F, Value::Ascii("Canon".into()));
        let tiff = write(&TiffFile {
            order: ByteOrder::LittleEndian,
            variant: Variant::Classic,
            ifds: vec![image],
        })
        .expect("write");
        let mut out = MARKER.to_vec();
        out.extend(tiff);
        out
    }

    /// The streaming entry point reads a `Read + Seek` source that is not a slice at all — the
    /// capability the slice-only API could not offer.
    #[test]
    fn parse_from_reads_a_seekable_stream() {
        let source = StreamSource::new(Cursor::new(blob()));
        let exif = ExifReader::new().parse_from(source).expect("parse_from");
        assert_eq!(exif.make(), Some("Canon"));
    }

    /// The marker is detected through the source, not by slicing: the same `require_marker`
    /// contract `parse` has must hold for a stream, or the two entry points disagree.
    #[test]
    fn the_marker_is_detected_through_the_source() {
        let marked = blob();
        let bare = marked[MARKER.len()..].to_vec();

        assert!(
            ExifReader::new()
                .require_marker(true)
                .parse_from(StreamSource::new(Cursor::new(marked)))
                .is_ok()
        );
        let err = ExifReader::new()
            .require_marker(true)
            .parse_from(StreamSource::new(Cursor::new(bare.clone())))
            .expect_err("a bare TIFF stream must be rejected");
        assert!(matches!(err, ExifError::MissingMarker), "{err:?}");
        // ...and without the requirement the same bare stream parses.
        assert!(
            ExifReader::new()
                .parse_from(StreamSource::new(Cursor::new(bare)))
                .is_ok()
        );
    }

    /// A source shorter than the 6-byte marker cannot be read for one; it is unmarked, and
    /// `require_marker` says so rather than reporting a torn read.
    #[test]
    fn a_source_too_short_for_the_marker_is_unmarked() {
        let err = ExifReader::new()
            .require_marker(true)
            .parse_from(&b"Exi"[..])
            .expect_err("three bytes cannot carry the marker");
        assert!(matches!(err, ExifError::MissingMarker), "{err:?}");
    }

    /// A `ReadAt` whose *transport* fails after `budget` successful reads — a disk error, a
    /// dropped network mount. Distinct from a source whose bytes are merely wrong: `gamut-core`
    /// maps the former to `Error::Io` and the latter to `Error::InvalidInput`.
    struct FailingAfter<S> {
        inner: S,
        budget: usize,
    }

    impl<S: ReadAt> ReadAt for FailingAfter<S> {
        fn read_exact_at(&mut self, offset: u64, buf: &mut [u8]) -> gamut_core::Result<()> {
            let Some(left) = self.budget.checked_sub(1) else {
                return Err(gamut_core::Error::Io(std::io::Error::other(
                    "transport lost",
                )));
            };
            self.budget = left;
            self.inner.read_exact_at(offset, buf)
        }

        fn len(&mut self) -> gamut_core::Result<u64> {
            self.inner.len()
        }
    }

    /// A structurally perfect blob with both 0th-IFD sub-directories and a nested Interop, so
    /// every sub-IFD drop path is reachable and none of them *should* fire.
    fn healthy_blob() -> Vec<u8> {
        let mut interop = Ifd::new();
        interop.set(0x0001, Value::Ascii("R98".into()));
        let mut exif = Ifd::new();
        exif.set(0x829D, Value::Rational(vec![(28, 10)]));
        exif.set_sub_ifd(INTEROP_IFD_POINTER, vec![interop]);
        let mut gps = Ifd::new();
        gps.set(0x0000, Value::Byte(vec![2, 3, 0, 0]));
        let mut image = Ifd::new();
        image.set(0x010F, Value::Ascii("Canon".into()));
        image.set_sub_ifd(EXIF_IFD_POINTER, vec![exif]);
        image.set_sub_ifd(GPS_IFD_POINTER, vec![gps]);
        let tiff = write(&TiffFile {
            order: ByteOrder::LittleEndian,
            variant: Variant::Classic,
            ifds: vec![image],
        })
        .expect("write");
        let mut out = MARKER.to_vec();
        out.extend(tiff);
        out
    }

    /// The bytes `deep_blob`'s thumbnail range addresses, appended past the directories.
    const THUMB_BYTES: &[u8] = b"\xFF\xD8__jpg\xFF\xD9";
    /// A `JPEGInterchangeFormat` value patched to the real offset once the layout is known —
    /// `write` lays the directories out, so the payload's position is not knowable before it runs.
    const THUMB_SENTINEL: u32 = 0xDEAD_BEEF;

    /// A structurally perfect blob reaching every read site `healthy_blob` cannot.
    ///
    /// `healthy_blob` has no `MakerNote`, no thumbnail bytes and no trailing directory, and its
    /// Interop pointer means a failure inside `maker_note_offset` is always resurfaced by the later
    /// Interop read. Here the GPS and Interop pointers are both absent — so `follow` returns without
    /// reading at all — while three additions each open one otherwise-unswept read: an out-of-line
    /// `MakerNote` gives the pin an offset a caller can lose, an in-bounds thumbnail range makes
    /// `read_range` fetch, and a third top-level directory makes `record_trailing_ifds` re-walk the
    /// chain.
    fn deep_blob() -> Vec<u8> {
        let mut exif = Ifd::new();
        exif.set(0x829A, Value::Rational(vec![(1, 250)])); // ExposureTime
        // Nine bytes: too wide to sit inline in the entry, so it has a real source offset.
        exif.set(
            ifd_tags::MAKER_NOTE,
            Value::Undefined(b"Canon\0\0\0\0".to_vec()),
        );
        let mut image = Ifd::new();
        image.set(0x010F, Value::Ascii("Canon".into()));
        image.set_sub_ifd(EXIF_IFD_POINTER, vec![exif]);

        let mut thumb = Ifd::new();
        thumb.set(ExifTag::Compression.tag_id(), Value::Short(vec![6]));
        thumb.set(
            ExifTag::JpegInterchangeFormat.tag_id(),
            Value::Long(vec![THUMB_SENTINEL]),
        );
        thumb.set(
            ExifTag::JpegInterchangeFormatLength.tag_id(),
            Value::Long(vec![THUMB_BYTES.len() as u32]),
        );

        let mut trailing = Ifd::new();
        trailing.set(0x0131, Value::Ascii("trailing".into())); // Software

        let mut tiff = write(&TiffFile {
            order: ByteOrder::LittleEndian,
            variant: Variant::Classic,
            ifds: vec![image, thumb, trailing],
        })
        .expect("write");

        // The payload goes after the directories, so the sentinel is patched to where it lands.
        let at = u32::try_from(tiff.len()).expect("the fixture fits in 32 bits");
        let sentinel = THUMB_SENTINEL.to_le_bytes();
        let hits: Vec<usize> = tiff
            .windows(4)
            .enumerate()
            .filter(|(_, w)| *w == sentinel)
            .map(|(i, _)| i)
            .collect();
        assert_eq!(
            hits.len(),
            1,
            "the sentinel must name exactly one value field"
        );
        tiff[hits[0]..hits[0] + 4].copy_from_slice(&at.to_le_bytes());
        tiff.extend_from_slice(THUMB_BYTES);

        let mut out = MARKER.to_vec();
        out.extend(tiff);
        out
    }

    /// The deep fixture really carries every loss the sweep below claims to watch for.
    ///
    /// Without this the sweep could pass against a blob that never had a pin, thumbnail bytes or a
    /// trailing directory to lose — and against a clean report that already blamed the file, which
    /// would make the sweep's report-equality law vacuous.
    #[test]
    fn the_deep_fixture_has_a_pin_a_thumbnail_and_a_trailing_directory_to_lose() {
        let data = deep_blob();
        let (exif, report) = ExifReader::new()
            .parse_from_with_report(&data[..])
            .expect("parse");
        assert!(
            exif.maker_note_offset().is_some(),
            "the fixture must pin an out-of-line MakerNote"
        );
        assert_eq!(
            exif.thumbnail().and_then(Thumbnail::jpeg),
            Some(THUMB_BYTES),
            "the fixture must have thumbnail bytes that were really fetched"
        );
        assert!(
            exif.gps_ifd().is_none(),
            "no GPS pointer to rescue a failure"
        );
        assert!(
            exif.interop_ifd().is_none(),
            "no Interop pointer to rescue a failure"
        );
        assert_eq!(report.dropped().len(), 1, "{:?}", report.dropped());
        assert_eq!(report.dropped()[0].region(), DroppedRegion::TrailingIfd);
        assert_eq!(
            report.dropped()[0].reason(),
            DropReason::Unrepresentable,
            "the clean report must blame nothing on the file"
        );
    }

    /// A marked blob whose 1st IFD carries the given `(tag, LONG)` fields after `Compression`.
    fn thumbnail_blob(fields: &[(u16, u32)]) -> Vec<u8> {
        let fields: Vec<(u16, Value)> = fields
            .iter()
            .map(|&(tag, value)| (tag, Value::Long(vec![value])))
            .collect();
        thumbnail_blob_of(&fields)
    }

    /// A marked blob whose 1st IFD carries the given `(tag, value)` fields after `Compression`.
    fn thumbnail_blob_of(fields: &[(u16, Value)]) -> Vec<u8> {
        let mut image = Ifd::new();
        image.set(0x010F, Value::Ascii("Canon".into()));
        let mut thumb = Ifd::new();
        thumb.set(ExifTag::Compression.tag_id(), Value::Short(vec![6]));
        for (tag, value) in fields {
            thumb.set(*tag, value.clone());
        }
        let tiff = write(&TiffFile {
            order: ByteOrder::LittleEndian,
            variant: Variant::Classic,
            ifds: vec![image, thumb],
        })
        .expect("write");
        let mut out = MARKER.to_vec();
        out.extend(tiff);
        out
    }

    /// `build(len)` where `len` is the TIFF-stream length `build` itself produces: the length
    /// depends only on each field's type and count, never on its values, so an offset or byte
    /// count can name the stream's own end.
    fn sized(build: impl Fn(u32) -> Vec<u8>) -> Vec<u8> {
        let len = (build(0).len() - MARKER.len()) as u32;
        build(len)
    }

    /// A marked blob whose uncompressed 1st IFD addresses `strips`, appended contiguously past the
    /// directories, through `StripOffsets` and a SHORT `StripByteCounts`.
    fn strip_blob(strips: &[&[u8]]) -> Vec<u8> {
        let counts = Value::Short(strips.iter().map(|s| s.len() as u16).collect());
        let mut blob = sized(|base| {
            let mut at = base;
            let offsets = strips
                .iter()
                .map(|s| {
                    let o = at;
                    at += s.len() as u32;
                    o
                })
                .collect();
            thumbnail_blob_of(&[
                (ExifTag::Compression.tag_id(), Value::Short(vec![1])),
                (ifd_tags::STRIP_OFFSETS, Value::Long(offsets)),
                (ifd_tags::STRIP_BYTE_COUNTS, counts.clone()),
            ])
        });
        for strip in strips {
            blob.extend_from_slice(strip);
        }
        blob
    }

    /// An uncompressed thumbnail's strips are read in `StripOffsets` order, and the offsets leave
    /// the model as the JPEG pointer does: the writer re-synthesises them. `StripByteCounts`
    /// stays, as `JPEGInterchangeFormatLength` does.
    #[test]
    fn strip_thumbnail_bytes_are_read_and_their_offsets_leave_the_model() {
        let strips: [&[u8]; 2] = [b"abc", b"defgh"];
        let exif = ExifReader::new()
            .strict(true)
            .parse(&strip_blob(&strips))
            .expect("parse");
        let thumb = exif.thumbnail().expect("thumbnail");
        assert_eq!(
            thumb.strips(),
            Some(&[b"abc".to_vec(), b"defgh".to_vec()][..])
        );
        assert_eq!(thumb.ifd().get(ifd_tags::STRIP_OFFSETS), None);
        assert_eq!(
            thumb.ifd().get(ifd_tags::STRIP_BYTE_COUNTS),
            Some(&Value::Short(vec![3, 5]))
        );
    }

    /// `StripByteCounts`, if any, given the stream's own length.
    type StripCounts = fn(u32) -> Option<Value>;

    /// A marked blob whose 1st IFD carries `offsets` as `StripOffsets` and `counts(len)` — `len`
    /// being the TIFF stream's own length — as `StripByteCounts`.
    fn strip_pair_blob(offsets: &Value, counts: StripCounts) -> Vec<u8> {
        sized(|len| {
            let mut fields = vec![(ifd_tags::STRIP_OFFSETS, offsets.clone())];
            fields.extend(counts(len).map(|c| (ifd_tags::STRIP_BYTE_COUNTS, c)));
            thumbnail_blob_of(&fields)
        })
    }

    /// Every way an integer `StripOffsets` naming strips can fail to describe readable ones, with
    /// the report entry each must produce and the strict-mode message: `(case, StripOffsets,
    /// StripByteCounts, reported offset, reason, strict message)`.
    ///
    /// The out-of-bounds case puts the bad strip second, so the reported offset is the offending
    /// strip's rather than the first. The overlap case is the allocation bound: two strips each
    /// ending exactly at the stream's end — so neither is out of bounds — whose total is nearly
    /// twice its length; its first offset is not 0, so the report is seen to carry it.
    fn unreadable_strip_cases() -> [(
        &'static str,
        Value,
        StripCounts,
        u64,
        DropReason,
        &'static str,
    ); 5] {
        const UNSIZED: &str = "StripOffsets with no StripByteCounts entry to size each strip";
        [
            (
                "out of bounds",
                Value::Long(vec![8, 0xFFFF]),
                |_| Some(Value::Long(vec![1, 4])),
                0xFFFF,
                DropReason::OutOfBounds,
                "strip range out of bounds",
            ),
            (
                "no byte counts",
                Value::Long(vec![8]),
                |_| None,
                8,
                DropReason::StripsUnsized,
                UNSIZED,
            ),
            (
                "non-integer byte counts",
                Value::Short(vec![9]),
                |_| Some(Value::Ascii("x".into())),
                9,
                DropReason::StripsUnsized,
                UNSIZED,
            ),
            (
                "unequal lengths",
                Value::Long(vec![10, 8]),
                |_| Some(Value::Long(vec![1])),
                10,
                DropReason::StripsUnsized,
                UNSIZED,
            ),
            (
                "overlap past the stream's length",
                Value::Long(vec![2, 1]),
                // Saturating: `sized` first builds with `len = 0` to measure the stream.
                |len| {
                    Some(Value::Long(vec![
                        len.saturating_sub(2),
                        len.saturating_sub(1),
                    ]))
                },
                2,
                DropReason::StripsOverlap,
                "strips overlapping past the stream's length",
            ),
        ]
    }

    /// A lenient parse names each unreadable strip set as one `ThumbnailStrips` drop, at the
    /// offset and for the reason the case expects, and removes `StripOffsets` from the model —
    /// while `StripByteCounts` stays, as `JPEGInterchangeFormatLength` does beside a dropped JPEG
    /// range. The written blob then carries no strip pointer, so it is strictly valid and re-parses
    /// to the same thumbnail with nothing reported (#868, as #548 for the JPEG pointer).
    #[test]
    fn unreadable_strips_are_dropped_and_their_offsets_leave_the_model() {
        for (name, offsets, counts, at, reason, _) in unreadable_strip_cases() {
            let blob = strip_pair_blob(&offsets, counts);
            let (exif, report) = ExifReader::new()
                .parse_with_report(&blob)
                .expect("lenient parse");
            assert_eq!(
                report.dropped(),
                &[Dropped::new(DroppedRegion::ThumbnailStrips, at, reason)],
                "{name}"
            );
            let thumb = exif.thumbnail().expect("the 1st IFD is still a thumbnail");
            assert_eq!(thumb.strips(), None, "{name}");
            assert_eq!(thumb.ifd().get(ifd_tags::STRIP_OFFSETS), None, "{name}");
            let len = (blob.len() - MARKER.len()) as u32;
            assert_eq!(
                thumb.ifd().get(ifd_tags::STRIP_BYTE_COUNTS),
                counts(len).as_ref(),
                "{name}: StripByteCounts must stay as the source had it"
            );

            let emitted = exif.to_bytes().expect("write");
            let (reparsed, report) = ExifReader::new()
                .strict(true)
                .parse_with_report(&emitted)
                .expect("the emitted blob must be strictly valid");
            assert!(report.is_empty(), "{name}: {:?}", report.dropped());
            assert_eq!(reparsed.thumbnail(), exif.thumbnail(), "{name}");
        }
    }

    /// Strict mode rejects each unreadable strip set as the bad thumbnail it is, naming the defect.
    #[test]
    fn unreadable_strips_are_rejected_strictly() {
        for (name, offsets, counts, _, _, why) in unreadable_strip_cases() {
            let err = ExifReader::new()
                .strict(true)
                .parse(&strip_pair_blob(&offsets, counts))
                .expect_err(name);
            assert_eq!(
                err.to_string(),
                format!("invalid thumbnail: {why}"),
                "{name}"
            );
        }
    }

    /// A `StripOffsets` that names no strip — not an integer array, or an empty one — addresses no
    /// bytes, so nothing is reported and strict mode accepts it; it still leaves the model, so the
    /// writer emits no strip pointer.
    #[test]
    fn a_strip_pointer_naming_no_strip_is_removed_without_a_drop() {
        for (name, offsets) in [
            ("non-integer", Value::Ascii("x".into())),
            ("empty", Value::Long(vec![])),
        ] {
            let blob = strip_pair_blob(&offsets, |_| Some(Value::Long(vec![1])));
            let (exif, report) = ExifReader::new()
                .strict(true)
                .parse_with_report(&blob)
                .expect("strict parse");
            assert!(report.is_empty(), "{name}: {:?}", report.dropped());
            let thumb = exif.thumbnail().expect("thumbnail");
            assert_eq!(thumb.strips(), None, "{name}");
            assert_eq!(thumb.ifd().get(ifd_tags::STRIP_OFFSETS), None, "{name}");
        }
    }

    /// A strip set whose total is exactly the stream's length is inside the bound: one strip
    /// spanning the whole TIFF stream, header included.
    #[test]
    fn strips_totalling_the_stream_length_are_read() {
        let blob = sized(|len| {
            thumbnail_blob_of(&[
                (ifd_tags::STRIP_OFFSETS, Value::Long(vec![0])),
                (ifd_tags::STRIP_BYTE_COUNTS, Value::Long(vec![len])),
            ])
        });
        let exif = Exif::parse(&blob).expect("parse");
        assert_eq!(
            exif.thumbnail().and_then(Thumbnail::strips),
            Some(&[blob[MARKER.len()..].to_vec()][..])
        );
    }

    /// A thumbnail pointer whose bytes were dropped never reaches the model, so `to_bytes` cannot
    /// re-emit it (#548).
    ///
    /// Before the fix the pointer was stripped only when the bytes were read, so an out-of-bounds
    /// or unsized range survived into the model and the writer wrote it back verbatim: the emitted
    /// blob claimed a thumbnail the parse's own report said was dropped, and re-parsing it named
    /// the same drop again. Both drop reasons are swept — the OutOfBounds case the issue reports
    /// and the ThumbnailLengthMissing case it shares a cause with.
    #[test]
    fn a_dropped_thumbnail_range_leaves_no_pointer_to_re_emit() {
        let ptr = ExifTag::JpegInterchangeFormat.tag_id();
        let len = ExifTag::JpegInterchangeFormatLength.tag_id();
        for (fields, reason) in [
            (vec![(ptr, 0xFFFF), (len, 16)], DropReason::OutOfBounds),
            (vec![(ptr, 4)], DropReason::ThumbnailLengthMissing),
        ] {
            let (exif, report) = ExifReader::new()
                .parse_with_report(&thumbnail_blob(&fields))
                .expect("lenient parse");
            assert_eq!(report.dropped().len(), 1, "{:?}", report.dropped());
            assert_eq!(report.dropped()[0].reason(), reason);
            let thumb = exif.thumbnail().expect("the 1st IFD is still a thumbnail");
            assert_eq!(thumb.ifd().get(ptr), None, "{reason:?}: pointer kept");

            let emitted = exif.to_bytes().expect("write");
            let (reparsed, report) = ExifReader::new()
                .strict(true)
                .parse_with_report(&emitted)
                .expect("the emitted blob must be strictly valid");
            assert!(report.is_empty(), "{reason:?}: {:?}", report.dropped());
            assert_eq!(reparsed.thumbnail(), exif.thumbnail(), "{reason:?}");
        }
    }

    /// A `JPEGInterchangeFormat` that `get_u32` cannot read as one offset never reaches the model
    /// either, and is not reported as a drop (#548, decision 2).
    ///
    /// Such a pointer addresses nothing the reader can follow, so no thumbnail bytes were lost and
    /// the report stays empty; but before the fix it survived into the model (the strip ran only
    /// when bytes were read) and `to_bytes` wrote back a pointer field no reader can use. Swept
    /// over the two ways a value fails `as_u32`: a non-integer type, and an integer type with a
    /// count other than one.
    #[test]
    fn an_unreadable_thumbnail_pointer_is_stripped_without_a_drop() {
        let ptr = ExifTag::JpegInterchangeFormat.tag_id();
        let len = ExifTag::JpegInterchangeFormatLength.tag_id();
        for value in [Value::Ascii("x".into()), Value::Long(vec![4, 8])] {
            let blob = thumbnail_blob_of(&[(ptr, value.clone()), (len, Value::Long(vec![16]))]);
            let (exif, report) = ExifReader::new()
                .parse_with_report(&blob)
                .expect("lenient parse");
            assert!(report.is_empty(), "{value:?}: {:?}", report.dropped());
            let thumb = exif.thumbnail().expect("the 1st IFD is still a thumbnail");
            assert_eq!(thumb.ifd().get(ptr), None, "{value:?}: pointer kept");
            assert_eq!(thumb.jpeg(), None, "{value:?}");

            let emitted = exif.to_bytes().expect("write");
            let (reparsed, report) = ExifReader::new()
                .strict(true)
                .parse_with_report(&emitted)
                .expect("the emitted blob must be strictly valid");
            assert!(report.is_empty(), "{value:?}: {:?}", report.dropped());
            assert_eq!(reparsed.thumbnail(), exif.thumbnail(), "{value:?}");
        }
    }

    /// The writer's output is unchanged wherever no thumbnail bytes were dropped (#548's
    /// equivalence sweep): a thumbnail with no JPEG tags, with a lone length, and with a range that
    /// was read all re-serialise to a fixpoint that carries the same thumbnail model, and a read
    /// range is re-emitted with its bytes and a pointer that reaches them.
    #[test]
    fn a_thumbnail_with_nothing_dropped_round_trips_to_a_fixpoint() {
        let len = ExifTag::JpegInterchangeFormatLength.tag_id();
        for blob in [
            thumbnail_blob(&[]),
            thumbnail_blob(&[(len, 16)]),
            deep_blob(),
        ] {
            let (exif, report) = ExifReader::new().parse_with_report(&blob).expect("parse");
            assert!(
                report
                    .dropped()
                    .iter()
                    .all(|d| d.region() != DroppedRegion::ThumbnailJpeg),
                "{:?}",
                report.dropped()
            );
            let once = exif.to_bytes().expect("write");
            let reparsed = ExifReader::new().parse(&once).expect("reparse");
            assert_eq!(reparsed.thumbnail(), exif.thumbnail());
            assert_eq!(
                reparsed.to_bytes().expect("rewrite"),
                once,
                "not a fixpoint"
            );
        }
    }

    /// A failing source is propagated, never reported as a malformed file.
    ///
    /// Leniency exists to tolerate corrupt *bytes*. If the transport fails instead, the data may be
    /// perfect, so silently returning `Ok` with the sub-IFDs missing — and a report blaming the
    /// file — would be a lie, and the worst case is the network-backed source this entry point
    /// exists to enable. The whole parse is swept one read at a time over two fixtures, between
    /// them reaching every site that reads bytes: the marker probe, the header, each directory
    /// body, each out-of-line value, the three sub-IFD reads (`healthy_blob`), and the thumbnail
    /// fetch, the trailing-chain re-walk and the maker-note pin (`deep_blob`). The one `ReadAt`
    /// method left unswept is `len`, which answers a length rather than reading bytes.
    ///
    /// The law is that an `Ok` from a failing source is the **whole** answer — equal to the clean
    /// parse in report, pin and thumbnail bytes — which catches both shapes a silent loss takes: a
    /// spurious report entry blaming the file, or, as the maker-note pin did, an `Exif` quietly
    /// missing something with nothing in the report at all. Equality against the clean report
    /// rather than emptiness is what lets `deep_blob` be swept at all: its trailing directory is a
    /// legitimate drop that a clean parse reports too.
    ///
    /// Swept twice: with the default reader, and with one that recovers entries and audits bytes,
    /// whose lenient decodes and second walk must keep a transport failure's kind just the same.
    #[test]
    fn a_failing_source_is_propagated_not_reported_as_a_malformed_file() {
        let readers = [
            ("default", ExifReader::new()),
            (
                "recovering+audit",
                ExifReader::new().recover_entries(true).audit_bytes(true),
            ),
        ];
        for (mode, reader) in readers {
            for (blob, data) in [("healthy", healthy_blob()), ("deep", deep_blob())] {
                let name = format!("{mode}/{blob}");
                sweep_transport_failures(&name, &reader, &data);
            }
        }
    }

    /// Fails `data`'s source after every read budget up to well past a whole parse, and holds each
    /// outcome to the law above.
    fn sweep_transport_failures(name: &str, reader: &ExifReader, data: &[u8]) {
        {
            let (clean, clean_report) = reader.parse_from_with_report(data).expect("clean parse");
            let (mut failures, mut successes) = (0, 0);
            for budget in 0..200 {
                let source = FailingAfter {
                    inner: data,
                    budget,
                };
                match reader.parse_from_with_report(source) {
                    Ok((exif, report)) => {
                        successes += 1;
                        assert_eq!(
                            report, clean_report,
                            "{name} budget {budget}: a transport failure changed the report"
                        );
                        assert_eq!(
                            exif.maker_note_offset(),
                            clean.maker_note_offset(),
                            "{name} budget {budget}: the maker-note pin was silently lost"
                        );
                        assert_eq!(
                            exif.thumbnail().and_then(Thumbnail::jpeg),
                            clean.thumbnail().and_then(Thumbnail::jpeg),
                            "{name} budget {budget}: the thumbnail bytes were silently lost"
                        );
                    }
                    Err(ExifError::Ifd(e)) => {
                        failures += 1;
                        assert_eq!(
                            e.kind(),
                            ErrorKind::Io,
                            "{name} budget {budget}: a transport failure must keep its kind"
                        );
                    }
                    // `MissingMarker` here would mean the marker probe swallowed the error and
                    // decided the blob was unmarked; anything else is equally a misdiagnosis.
                    Err(other) => {
                        panic!("{name} budget {budget}: transport failure became {other:?}")
                    }
                }
            }
            assert!(
                failures > 0 && successes > 0,
                "{name}: the sweep proved nothing"
            );
        }
    }

    /// A transport failure while probing for the marker is not a *missing* marker.
    ///
    /// The marker probe is the one read that happens before any parsing, and its result is a
    /// three-way question — marked, unmarked, or unknown — collapsed onto a boolean. Deciding
    /// "unmarked" from a failed read makes `require_marker(true)` answer `MissingMarker` for a blob
    /// that may well carry one, which sends a caller to the wrong conclusion entirely. The split is
    /// keyed on the error kind, so a short slice still reads as genuinely unmarked — which
    /// `a_source_too_short_for_the_marker_is_unmarked` pins.
    #[test]
    fn a_transport_failure_probing_the_marker_is_not_a_missing_marker() {
        let data = healthy_blob();
        let source = FailingAfter {
            inner: &data[..],
            budget: 0,
        };
        let err = ExifReader::new()
            .require_marker(true)
            .parse_from(source)
            .expect_err("a source that cannot be read must not parse");
        match err {
            ExifError::Ifd(e) => assert_eq!(
                e.kind(),
                ErrorKind::Io,
                "the transport failure must keep its kind"
            ),
            other => panic!("transport failure became {other:?}"),
        }
    }

    /// The `ASCII` text a poisoned entry carries before [`poisoned_blob`] breaks its UTF-8.
    const POISON: &[u8] = b"POISONED-VALUE";

    /// One directory of a blob with every directory present, the entry each test poisons there,
    /// and a field that must survive beside it.
    const POISON_SITES: [(IfdKind, u16, u16); 5] = [
        (IfdKind::Image, 0x0110, 0x010F),     // Model, beside Make
        (IfdKind::Thumbnail, 0x010E, 0x0103), // ImageDescription, beside Compression
        (IfdKind::Exif, 0xA434, 0x829D),      // LensModel, beside FNumber
        (IfdKind::Gps, 0x0012, 0x0000),       // GPSMapDatum, beside GPSVersionID
        (IfdKind::Interop, 0x1000, 0x0001),   // RelatedImageFileFormat, beside the index
    ];

    /// A marked blob with all five directories, where `kind`'s directory also carries `tag` as an
    /// out-of-line `ASCII` value whose first byte is then overwritten with `0xFF` — no longer
    /// UTF-8, so that one entry is unreadable while its directory's body is intact.
    fn poisoned_blob(kind: IfdKind, tag: u16) -> Vec<u8> {
        let poison = Value::Ascii(String::from_utf8(POISON.to_vec()).expect("ascii"));
        let mut dirs: [Ifd; 5] = Default::default();
        let [image, thumb, exif, gps, interop] = &mut dirs;
        image.set(0x010F, Value::Ascii("Canon".into()));
        thumb.set(0x0103, Value::Short(vec![6]));
        exif.set(0x829D, Value::Rational(vec![(28, 10)]));
        gps.set(0x0000, Value::Byte(vec![2, 3, 0, 0]));
        interop.set(0x0001, Value::Ascii("R98".into()));
        let at = POISON_SITES
            .iter()
            .position(|&(k, _, _)| k == kind)
            .expect("a poison site");
        dirs[at].set(tag, poison);
        let [mut image, thumb, mut exif, gps, interop] = dirs;
        exif.set_sub_ifd(INTEROP_IFD_POINTER, vec![interop]);
        image.set_sub_ifd(EXIF_IFD_POINTER, vec![exif]);
        image.set_sub_ifd(GPS_IFD_POINTER, vec![gps]);
        let mut tiff = write(&TiffFile {
            order: ByteOrder::LittleEndian,
            variant: Variant::Classic,
            ifds: vec![image, thumb],
        })
        .expect("write");
        let hits: Vec<usize> = tiff
            .windows(POISON.len())
            .enumerate()
            .filter(|(_, w)| *w == POISON)
            .map(|(i, _)| i)
            .collect();
        assert_eq!(hits.len(), 1, "exactly one value carries the poison");
        tiff[hits[0]] = 0xFF;
        let mut out = MARKER.to_vec();
        out.extend(tiff);
        out
    }

    /// Without recovery a poisoned entry costs its whole directory — the 0th or 1st IFD fails the
    /// parse, a sub-IFD is dropped as `Malformed` — which is the baseline the recovering tests
    /// below improve on, and proof that each fixture really is unreadable.
    #[test]
    fn without_recovery_a_poisoned_entry_costs_its_whole_directory() {
        for (kind, tag, _) in POISON_SITES {
            let data = poisoned_blob(kind, tag);
            let parsed = ExifReader::new().parse_with_report(&data);
            match kind {
                IfdKind::Image | IfdKind::Thumbnail => {
                    assert!(parsed.is_err(), "{kind:?}: a top-level directory is fatal");
                }
                _ => {
                    let (_, report) = parsed.expect("a sub-IFD is dropped, not fatal");
                    assert_eq!(report.dropped().len(), 1, "{kind:?}: {report:?}");
                    assert_eq!(report.dropped()[0].reason(), DropReason::Malformed);
                    assert_eq!(report.dropped_entries(), &[]);
                }
            }
        }
    }

    /// With recovery the directory survives minus the poisoned entry, the field beside it is
    /// kept, no region is dropped, and the skip is named by directory, tag and the offset of the
    /// entry's own record in the TIFF stream.
    #[test]
    fn recovery_keeps_the_directory_and_names_the_skipped_entry() {
        for (kind, tag, kept) in POISON_SITES {
            let data = poisoned_blob(kind, tag);
            let (exif, report) = ExifReader::new()
                .recover_entries(true)
                .parse_with_report(&data)
                .unwrap_or_else(|e| panic!("{kind:?}: recovery must not fail: {e}"));
            assert!(exif.get(kind, kept).is_some(), "{kind:?}: neighbour lost");
            assert_eq!(exif.get(kind, tag), None, "{kind:?}: poisoned field kept");
            assert_eq!(report.dropped(), &[], "{kind:?}: a region was dropped");
            let [entry] = report.dropped_entries() else {
                panic!("{kind:?}: one skipped entry, got {report:?}");
            };
            assert_eq!((entry.ifd(), entry.tag()), (kind, tag));
            let record = MARKER.len() + usize::try_from(entry.offset()).expect("small");
            assert_eq!(
                data[record..record + 2],
                tag.to_le_bytes(),
                "{kind:?}: the offset names the entry's record in the TIFF frame"
            );
        }
    }

    /// The recovered model writes back as a well-formed blob that parses cleanly without
    /// recovery: the skipped field is simply absent, never re-emitted half-read.
    #[test]
    fn a_recovered_parse_round_trips_without_the_skipped_entry() {
        for (kind, tag, kept) in POISON_SITES {
            let data = poisoned_blob(kind, tag);
            let recovered = ExifReader::new()
                .recover_entries(true)
                .parse(&data)
                .expect("recover");
            let bytes = recovered.to_bytes().expect("to_bytes");
            let (again, report) = ExifReader::new()
                .parse_with_report(&bytes)
                .expect("the rewrite parses without recovery");
            assert!(report.is_empty(), "{kind:?}: {report:?}");
            assert_eq!(again.get(kind, kept), recovered.get(kind, kept));
            assert_eq!(again.get(kind, tag), None);
        }
    }

    /// Strictness overrules recovery: it rejects exactly what recovery would skip.
    #[test]
    fn strict_mode_ignores_recovery() {
        let reader = ExifReader::new().strict(true).recover_entries(true);
        let image = poisoned_blob(IfdKind::Image, 0x0110);
        assert!(matches!(reader.parse(&image), Err(ExifError::Ifd(_))));
        let exif = poisoned_blob(IfdKind::Exif, 0xA434);
        assert!(matches!(
            reader.parse(&exif),
            Err(ExifError::InvalidIfd("Exif"))
        ));
    }

    /// On blobs with nothing to recover, recovery changes nothing — including the trailing
    /// directory `deep_blob` carries, which the recovering walk names from its own single pass.
    #[test]
    fn recovery_on_a_readable_blob_reports_exactly_what_the_default_reader_does() {
        for data in [healthy_blob(), deep_blob()] {
            let plain = ExifReader::new().parse_with_report(&data).expect("plain");
            let recovering = ExifReader::new()
                .recover_entries(true)
                .parse_with_report(&data)
                .expect("recovering");
            assert_eq!(recovering, plain);
        }
    }

    /// A chain with no directory at all fails identically with and without recovery: the
    /// recovering walk defers that error to `read_file` rather than inventing its own.
    #[test]
    fn an_empty_chain_fails_identically_with_and_without_recovery() {
        let mut data = MARKER.to_vec();
        data.extend_from_slice(b"II\x2a\x00\x00\x00\x00\x00"); // first IFD offset = 0
        let plain = ExifReader::new().parse(&data).expect_err("no IFD");
        let recovering = ExifReader::new()
            .recover_entries(true)
            .parse(&data)
            .expect_err("no IFD");
        assert_eq!(recovering.to_string(), plain.to_string());
    }

    /// No audit is run, or reported, unless asked for.
    #[test]
    fn a_report_carries_no_byte_audit_unless_asked() {
        let (_, report) = ExifReader::new()
            .parse_with_report(&healthy_blob())
            .expect("parse");
        assert_eq!(report.byte_audit(), None);
    }

    /// A blob `write` produced accounts for every byte of its TIFF stream — header, directories,
    /// values, the MakerNote and the thumbnail's JPEG range — and leaves nothing unnamed.
    #[test]
    fn a_written_blob_audits_as_fully_classified() {
        for data in [healthy_blob(), deep_blob()] {
            let (_, report) = ExifReader::new()
                .audit_bytes(true)
                .parse_with_report(&data)
                .expect("parse");
            let audit = report.byte_audit().expect("asked for");
            assert!(audit.is_fully_classified(), "{audit:?}");
            assert_eq!(audit.unclaimed_spans(), Vec::new(), "{audit:?}");
        }
    }

    /// Bytes no structure claims are named, at their offset in the TIFF stream (the marker is
    /// outside the audited frame) — the verdict the drop lists cannot give.
    #[test]
    fn appended_bytes_are_named_by_the_byte_audit() {
        let mut data = healthy_blob();
        let tiff_len = (data.len() - MARKER.len()) as u64;
        data.extend_from_slice(&[0xAB; 16]);
        let (_, report) = ExifReader::new()
            .audit_bytes(true)
            .parse_with_report(&data)
            .expect("parse");
        assert!(report.is_empty(), "nothing was *dropped*: {report:?}");
        let spans = report.byte_audit().expect("asked for").unclaimed_spans();
        assert_eq!(spans.len(), 1, "{spans:?}");
        assert_eq!((spans[0].range.start, spans[0].range.len), (tiff_len, 16));
    }

    /// The audit is a second, strict walk: a sub-IFD recovery kept still fails it, so its bytes
    /// stay unclassified; a top-level entry recovery skipped leaves the walk nothing to stand on,
    /// so there is no verdict — but the parse itself still succeeds.
    #[test]
    fn a_recovered_parse_is_audited_by_the_strict_walk() {
        let reader = ExifReader::new().recover_entries(true).audit_bytes(true);
        let (_, report) = reader
            .parse_with_report(&poisoned_blob(IfdKind::Exif, 0xA434))
            .expect("parse");
        let audit = report.byte_audit().expect("the top-level chain reads");
        assert!(!audit.is_fully_classified(), "{audit:?}");

        let (exif, report) = reader
            .parse_with_report(&poisoned_blob(IfdKind::Image, 0x0110))
            .expect("parse");
        assert_eq!(exif.make(), Some("Canon"));
        assert_eq!(report.byte_audit(), None);
    }

    /// An unreadable entry in a trailing (3rd) directory also leaves the strict audit walk nothing
    /// to stand on, yet the recovering parse names no skip for it: that directory is read but never
    /// decoded, and is dropped whole as a `TrailingIfd` region. The audit's `None` is therefore not
    /// evidence of a skipped 0th/1st-IFD entry.
    #[test]
    fn an_unreadable_trailing_directory_entry_costs_the_audit_but_names_no_skip() {
        let mut image = Ifd::new();
        image.set(0x010F, Value::Ascii("Canon".into()));
        let mut thumb = Ifd::new();
        thumb.set(0x0103, Value::Short(vec![6]));
        let mut trailing = Ifd::new();
        trailing.set(
            0x0131,
            Value::Ascii(String::from_utf8(POISON.to_vec()).expect("ascii")),
        );
        let mut tiff = write(&TiffFile {
            order: ByteOrder::LittleEndian,
            variant: Variant::Classic,
            ifds: vec![image, thumb, trailing],
        })
        .expect("write");
        let hits: Vec<usize> = tiff
            .windows(POISON.len())
            .enumerate()
            .filter(|(_, w)| *w == POISON)
            .map(|(i, _)| i)
            .collect();
        assert_eq!(hits.len(), 1, "exactly one value carries the poison");
        tiff[hits[0]] = 0xFF;
        let mut data = MARKER.to_vec();
        data.extend(tiff);

        assert!(
            ExifReader::new().parse(&data).is_err(),
            "without recovery the trailing entry is fatal, so the fixture really is unreadable"
        );
        let (exif, report) = ExifReader::new()
            .recover_entries(true)
            .audit_bytes(true)
            .parse_with_report(&data)
            .expect("parse");
        assert_eq!(exif.make(), Some("Canon"));
        assert_eq!(report.dropped_entries(), &[], "no skip is named");
        assert_eq!(report.dropped().len(), 1, "{report:?}");
        assert_eq!(report.dropped()[0].region(), DroppedRegion::TrailingIfd);
        assert_eq!(report.byte_audit(), None);
    }

    /// The tag [`shadowed_blob`] writes twice in every directory.
    const DUPLICATE: u16 = 0xFDE8;

    /// A marked blob with all five directories, each carrying [`DUPLICATE`] twice: first as
    /// `SHORT 1`, then as `SHORT 2`. `write` cannot emit a duplicate, so each directory is written
    /// with `DUPLICATE` and `DUPLICATE + 1` — adjacent records, since every other tag is lower —
    /// and the second record's tag is then patched to `DUPLICATE`. Returns the blob and the offset
    /// of each shadowed (first) record in the TIFF stream, in file order.
    fn shadowed_blob() -> (Vec<u8>, Vec<u64>) {
        let mut dirs: [Ifd; 5] = Default::default();
        for dir in &mut dirs {
            dir.set(DUPLICATE, Value::Short(vec![1]));
            dir.set(DUPLICATE + 1, Value::Short(vec![2]));
        }
        let [mut image, thumb, mut exif, gps, interop] = dirs;
        exif.set_sub_ifd(INTEROP_IFD_POINTER, vec![interop]);
        image.set_sub_ifd(EXIF_IFD_POINTER, vec![exif]);
        image.set_sub_ifd(GPS_IFD_POINTER, vec![gps]);
        let mut tiff = write(&TiffFile {
            order: ByteOrder::LittleEndian,
            variant: Variant::Classic,
            ifds: vec![image, thumb],
        })
        .expect("write");
        let [lo, hi] = (DUPLICATE + 1).to_le_bytes();
        let survivor = [lo, hi, 3, 0, 1, 0, 0, 0, 2, 0, 0, 0]; // SHORT, count 1, value 2
        let hits: Vec<usize> = tiff
            .windows(survivor.len())
            .enumerate()
            .filter(|(_, w)| *w == survivor)
            .map(|(i, _)| i)
            .collect();
        assert_eq!(hits.len(), 5, "one survivor record per directory");
        for &at in &hits {
            tiff[at..at + 2].copy_from_slice(&DUPLICATE.to_le_bytes());
        }
        let mut data = MARKER.to_vec();
        data.extend(tiff);
        (data, hits.iter().map(|&at| at as u64 - 12).collect())
    }

    /// Every directory the model keeps names its shadowed duplicate — by directory, in the
    /// reader's directory order, at the offset of the shadowed record itself — while the model
    /// holds the survivor, and no region is dropped. The same report comes back strict, lenient
    /// and recovering: every entry was read, so no mode has grounds to treat it differently.
    #[test]
    fn every_kept_directory_names_its_shadowed_duplicate_in_every_mode() {
        let (data, records) = shadowed_blob();
        for reader in [
            ExifReader::new(),
            ExifReader::new().strict(true),
            ExifReader::new().recover_entries(true),
        ] {
            let (exif, report) = reader.parse_with_report(&data).expect("parse");
            assert_eq!(report.dropped(), &[], "{reader:?}");
            let entries = report.dropped_entries();
            assert_eq!(
                entries.iter().map(|e| e.ifd()).collect::<Vec<_>>(),
                [
                    IfdKind::Image,
                    IfdKind::Thumbnail,
                    IfdKind::Exif,
                    IfdKind::Gps,
                    IfdKind::Interop
                ],
                "{reader:?}"
            );
            let mut offsets = Vec::new();
            for entry in entries {
                assert_eq!(entry.tag(), DUPLICATE, "{reader:?}");
                assert_eq!(entry.reason(), EntryDropReason::Shadowed, "{reader:?}");
                assert_eq!(
                    exif.get(entry.ifd(), DUPLICATE),
                    Some(&Value::Short(vec![2])),
                    "{reader:?}: the last occurrence survives"
                );
                offsets.push(entry.offset());
            }
            offsets.sort_unstable();
            assert_eq!(offsets, records, "{reader:?}");
        }
    }

    /// The model holds one value per tag, so the rewrite carries only the survivors and parses
    /// back with nothing to report: the report was the only record of the shadowed entries.
    #[test]
    fn a_shadowed_parse_round_trips_without_the_shadowed_entries() {
        let (data, _) = shadowed_blob();
        let exif = ExifReader::new().parse(&data).expect("parse");
        let (again, report) = ExifReader::new()
            .parse_with_report(&exif.to_bytes().expect("to_bytes"))
            .expect("reparse");
        assert!(report.is_empty(), "{report:?}");
        assert_eq!(
            again.get(IfdKind::Interop, DUPLICATE),
            Some(&Value::Short(vec![2]))
        );
    }

    /// A trailing directory is discarded whole, so a duplicate inside it is not named a second
    /// time as an entry: the `TrailingIfd` region is the one record of it, in both walks.
    #[test]
    fn a_shadowed_entry_in_a_trailing_directory_is_not_named_as_an_entry() {
        let mut image = Ifd::new();
        image.set(0x010F, Value::Ascii("Canon".into()));
        let mut trailing = Ifd::new();
        trailing.set(DUPLICATE, Value::Short(vec![1]));
        trailing.set(DUPLICATE + 1, Value::Short(vec![2]));
        let mut tiff = write(&TiffFile {
            order: ByteOrder::LittleEndian,
            variant: Variant::Classic,
            ifds: vec![image, Ifd::new(), trailing],
        })
        .expect("write");
        let at = tiff
            .windows(2)
            .position(|w| w == (DUPLICATE + 1).to_le_bytes())
            .expect("the second record");
        tiff[at..at + 2].copy_from_slice(&DUPLICATE.to_le_bytes());
        for reader in [ExifReader::new(), ExifReader::new().recover_entries(true)] {
            let (_, report) = reader.parse_with_report(&tiff).expect("parse");
            assert_eq!(report.dropped_entries(), &[], "{reader:?}");
            assert_eq!(report.dropped().len(), 1, "{reader:?}: {report:?}");
            assert_eq!(report.dropped()[0].region(), DroppedRegion::TrailingIfd);
        }
    }

    /// The first and second `MakerNote` values of [`duplicate_maker_note_blob`]: nine bytes each,
    /// too wide to sit inline, so each has a real source offset.
    const FIRST_NOTE: &[u8] = b"first\0\0\0\0";
    const SECOND_NOTE: &[u8] = b"second\0\0\0";

    /// A marked blob whose Exif IFD carries two out-of-line `MakerNote` entries, [`FIRST_NOTE`]
    /// then [`SECOND_NOTE`]. `write` cannot emit a duplicate, so the second is written as
    /// `UserComment` (0x9286, the next tag up in that directory) and its record's tag patched to
    /// `MakerNote`. With `second_readable` false its value offset is also patched past the stream,
    /// so only a recovering reader keeps the directory. Returns the blob and each note's value
    /// offset in the TIFF stream, in file order.
    fn duplicate_maker_note_blob(second_readable: bool) -> (Vec<u8>, [u64; 2]) {
        let mut exif = Ifd::new();
        exif.set(ifd_tags::MAKER_NOTE, Value::Undefined(FIRST_NOTE.to_vec()));
        exif.set(0x9286, Value::Undefined(SECOND_NOTE.to_vec()));
        let mut image = Ifd::new();
        image.set(0x010F, Value::Ascii("Canon".into()));
        image.set_sub_ifd(EXIF_IFD_POINTER, vec![exif]);
        let mut tiff = write(&TiffFile {
            order: ByteOrder::LittleEndian,
            variant: Variant::Classic,
            ifds: vec![image],
        })
        .expect("write");
        let record = |tag: u16| {
            let [lo, hi] = tag.to_le_bytes();
            let head = [lo, hi, 7, 0, 9, 0, 0, 0]; // UNDEFINED, count 9
            tiff.windows(head.len())
                .position(|w| w == head)
                .expect("the record")
        };
        let (first, second) = (record(ifd_tags::MAKER_NOTE), record(0x9286));
        let word = |at: usize| {
            u64::from(u32::from_le_bytes(
                tiff[at + 8..at + 12].try_into().expect("four bytes"),
            ))
        };
        let offsets = [word(first), word(second)];
        tiff[second..second + 2].copy_from_slice(&ifd_tags::MAKER_NOTE.to_le_bytes());
        if !second_readable {
            tiff[second + 8..second + 12].copy_from_slice(&0xFFFF_0000u32.to_le_bytes());
        }
        let mut data = MARKER.to_vec();
        data.extend(tiff);
        (data, offsets)
    }

    /// With two `MakerNote` entries the model keeps the last, so the pin is the last one's value
    /// offset — not the shadowed first's, which would place the surviving note where its absolute
    /// internal offsets were never computed for (#790). Swept over every mode, since every entry
    /// is readable and no mode decodes the directory differently.
    #[test]
    fn a_duplicated_maker_note_pins_the_survivor() {
        let (data, [_, second]) = duplicate_maker_note_blob(true);
        for reader in [
            ExifReader::new(),
            ExifReader::new().strict(true),
            ExifReader::new().recover_entries(true),
        ] {
            let exif = reader.parse(&data).expect("parse");
            assert_eq!(
                exif.get(IfdKind::Exif, ifd_tags::MAKER_NOTE),
                Some(&Value::Undefined(SECOND_NOTE.to_vec())),
                "{reader:?}: the model keeps the last note"
            );
            assert_eq!(exif.maker_note_offset(), Some(second), "{reader:?}");
        }
    }

    /// Under recovery an unreadable last `MakerNote` is skipped and shadows nothing, so the model
    /// keeps the first — and the pin follows it, not the skipped entry's out-of-bounds offset.
    #[test]
    fn a_recovered_maker_note_pins_the_readable_survivor() {
        let (data, [first, _]) = duplicate_maker_note_blob(false);
        let exif = ExifReader::new()
            .recover_entries(true)
            .parse(&data)
            .expect("parse");
        assert_eq!(
            exif.get(IfdKind::Exif, ifd_tags::MAKER_NOTE),
            Some(&Value::Undefined(FIRST_NOTE.to_vec())),
            "the model keeps the readable note"
        );
        assert_eq!(exif.maker_note_offset(), Some(first));
    }

    /// `address_reason` splits the two defects the report distinguishes, and the boundary is the
    /// stream's length itself: the last byte is inside, the length is not.
    #[test]
    fn an_address_is_out_of_bounds_from_the_streams_length_onwards() {
        let data = [0u8; 8];
        let mut reader =
            IfdReader::with_layout(&data[..], ByteOrder::LittleEndian, Variant::Classic);
        assert_eq!(
            address_reason(&mut reader, 7).expect("reason"),
            DropReason::Malformed,
            "the last byte of the stream is inside it"
        );
        assert_eq!(
            address_reason(&mut reader, 8).expect("reason"),
            DropReason::OutOfBounds,
            "one past the last byte is outside it"
        );
    }

    /// A range is fetched only when it ends inside the stream — the bound that stops a hostile
    /// length from being allocated. The boundary case (a range ending exactly at the end) is the
    /// one an off-by-one would get wrong.
    #[test]
    fn a_range_is_fetched_only_when_it_ends_inside_the_stream() {
        let data = [1u8, 2, 3, 4, 5, 6, 7, 8];
        let mut reader =
            IfdReader::with_layout(&data[..], ByteOrder::LittleEndian, Variant::Classic);
        assert_eq!(
            read_range(&mut reader, 4, 4).expect("read"),
            Some(vec![5, 6, 7, 8]),
            "a range ending exactly at the end is inside"
        );
        assert_eq!(
            read_range(&mut reader, 4, 5).expect("read"),
            None,
            "one byte past the end is not"
        );
        assert_eq!(
            read_range(&mut reader, 0, u32::MAX).expect("read"),
            None,
            "a hostile length is refused before it is allocated"
        );
    }
}
