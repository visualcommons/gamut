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

use gamut_core::ErrorKind;
use gamut_ifd::{Ifd, IfdReader, RawIfd, ReadAt, tags as ifd_tags};

use crate::error::{ExifError, Result};
use crate::exif::{EXIF_IFD_POINTER, Exif, MARKER};
use crate::reader::ExifReader;
use crate::report::{DropReason, Dropped, DroppedRegion, ReadReport};
use crate::tag::ExifTag;
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
    pub fn parse_from<S: ReadAt>(&self, source: S) -> Result<Exif> {
        self.parse_source(source, &mut ReadReport::new())
    }

    /// Parses EXIF from a positioned byte source and reports what a lenient parse discarded.
    ///
    /// The streaming twin of [`parse_with_report`](Self::parse_with_report). See [`ReadReport`].
    ///
    /// # Errors
    ///
    /// As [`parse_from`](Self::parse_from).
    pub fn parse_from_with_report<S: ReadAt>(&self, source: S) -> Result<(Exif, ReadReport)> {
        let mut report = ReadReport::new();
        let exif = self.parse_source(source, &mut report)?;
        Ok((exif, report))
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

        let file = reader.read_file()?;
        let has_trailing = file.ifds.len() > 2;
        let mut ifds = file.ifds.into_iter();
        let mut image = ifds.next().ok_or(ExifError::Truncated)?;
        // The next-IFD chain's second entry is the thumbnail directory (1st IFD), if any.
        let thumbnail = match ifds.next() {
            Some(ifd) => Some(self.read_thumbnail(ifd, &mut reader, report)?),
            None => None,
        };
        // EXIF defines exactly two top-level directories, so anything further down the chain has
        // nowhere to go in the model. Name it rather than letting the iterator drop it silently.
        record_trailing_ifds(&mut reader, has_trailing, report)?;

        // The Exif sub-IFD's own offset, captured before `follow` strips the pointer: the
        // maker-note pin needs the note value's absolute source position.
        let exif_ifd_at = image.get_u32(EXIF_IFD_POINTER).map(u64::from);
        let exif = self.follow(&mut image, &mut reader, DroppedRegion::ExifIfd, report)?;
        let gps = self.follow(&mut image, &mut reader, DroppedRegion::GpsIfd, report)?;
        let maker_note_at = match (&exif, exif_ifd_at) {
            (Some(_), Some(at)) => maker_note_offset(&mut reader, at)?,
            _ => None,
        };

        // The Interoperability directory is reached from *inside* the Exif sub-IFD, not the 0th IFD.
        let (exif, interop) = match exif {
            Some(mut e) => {
                let interop =
                    self.follow(&mut e, &mut reader, DroppedRegion::InteropIfd, report)?;
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
    fn follow<S: ReadAt>(
        &self,
        parent: &mut Ifd,
        reader: &mut IfdReader<S>,
        region: DroppedRegion,
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
        let followed = match reader.read_ifd(offset) {
            Ok(raw) => reader.decode_ifd(&raw),
            Err(e) => Err(e),
        };
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
    /// lenient mode an out-of-bounds range yields a thumbnail without bytes and a recorded drop;
    /// in strict mode it errors.
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
            _ => None,
        };
        // The JPEGInterchangeFormat offset is structural — the bytes are captured above and the
        // writer re-synthesises the offset — so drop it from the stored directory (mirroring how the
        // sub-IFD pointer tags are stripped), leaving a value the model can't carry stale.
        let mut ifd = ifd;
        if jpeg.is_some() {
            ifd.remove(ptr);
        }
        Ok(Thumbnail::from_parts(ifd, jpeg))
    }
}

/// Records the top-level directories past the 1st IFD, which parse cleanly but have nowhere to go
/// in the [`Exif`] model.
///
/// The offsets come from a second walk of the next-IFD chain, which is the single source of truth
/// for *which* directories are trailing — `has_trailing` only says whether the walk is worth
/// starting. That costs a re-read of the directory bodies, so it runs **only** when there is
/// something to report: a well-formed EXIF blob has one or two directories and never reaches it,
/// leaving the lazy read bound untouched.
///
/// A source that dies between the two walks turns a would-be success into an error. That is the
/// same rule the rest of this module follows — a transport failure is propagated, never swallowed —
/// and swallowing it only here would be the inconsistency.
fn record_trailing_ifds<S: ReadAt>(
    reader: &mut IfdReader<S>,
    has_trailing: bool,
    report: &mut ReadReport,
) -> Result<()> {
    if !has_trailing {
        return Ok(());
    }
    let mut offsets = Vec::new();
    for raw in reader.ifds().skip(2) {
        offsets.push(raw?.offset);
    }
    for offset in offsets {
        report.record(Dropped::new(
            DroppedRegion::TrailingIfd,
            offset,
            DropReason::Unrepresentable,
        ));
    }
    Ok(())
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
    let mut buf = vec![0u8; length as usize];
    reader
        .source_mut()
        .read_exact_at(u64::from(offset), &mut buf)?;
    Ok(Some(buf))
}

/// The absolute offset of the Exif sub-IFD's out-of-line `MakerNote` value in the TIFF stream, or
/// `None` if the note is absent or inline.
///
/// A failure here is propagated rather than folded into `None`. The offset is what
/// [`ExifWriter`](crate::ExifWriter) uses to *pin* the note in place on a rewrite, so losing it
/// silently re-emits a vendor MakerNote unpinned — wrong bytes, no error, and nothing in the
/// report. No later read is guaranteed to resurface the failure either: `follow` returns before
/// reading at all when the GPS and Interop pointers are absent, which is the common case.
fn maker_note_offset<S: ReadAt>(
    reader: &mut IfdReader<S>,
    exif_ifd_at: u64,
) -> Result<Option<u64>> {
    // Every error propagates, with no lenient arm — deliberately. This is reached only when
    // `follow` has already read and decoded this exact directory at this exact offset, so a
    // deterministic source cannot fail here for a reason the *bytes* explain. A malformed-input
    // arm would therefore be unreachable, and an unreachable arm is a branch no test can falsify.
    let raw: RawIfd = reader.read_ifd(exif_ifd_at)?;
    let Some(entry) = raw.entry(ifd_tags::MAKER_NOTE) else {
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

    /// A structurally perfect blob that reaches the **maker-note** read site.
    ///
    /// `healthy_blob` cannot: it carries no `MakerNote`, and its Interop pointer means a failure
    /// inside `maker_note_offset` is always resurfaced by the later Interop read. Here the GPS and
    /// Interop pointers are both absent — so `follow` returns without reading at all — and the
    /// out-of-line `MakerNote` makes the pin's offset something a caller can lose.
    fn maker_note_blob() -> Vec<u8> {
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

    /// The maker-note pin is real in the fixture the sweep uses, so losing it is observable.
    ///
    /// Without this the sweep below could pass against a blob that never had a pin to lose.
    #[test]
    fn the_maker_note_fixture_has_a_pin_to_lose() {
        let exif = ExifReader::new()
            .parse_from(&maker_note_blob()[..])
            .expect("parse");
        assert!(
            exif.maker_note_offset().is_some(),
            "the fixture must pin an out-of-line MakerNote"
        );
        assert!(
            exif.gps_ifd().is_none(),
            "no GPS pointer to rescue a failure"
        );
        assert!(
            exif.interop_ifd().is_none(),
            "no Interop pointer to rescue a failure"
        );
    }

    /// A failing source is propagated, never reported as a malformed file.
    ///
    /// Leniency exists to tolerate corrupt *bytes*. If the transport fails instead, the data may be
    /// perfect, so silently returning `Ok` with the sub-IFDs missing — and a report blaming the
    /// file — would be a lie, and the worst case is the network-backed source this entry point
    /// exists to enable. The whole parse is swept one read at a time over two fixtures, between
    /// them reaching every read site: the marker probe, the header, each directory body, each
    /// out-of-line value, and the maker-note pin — which only `maker_note_blob` reaches.
    ///
    /// A silent loss shows up here as `Ok` from a source that failed, and the assertions cover both
    /// shapes it can take: a spurious report entry blaming the file, or — as the maker-note pin did
    /// — an `Exif` quietly missing something with nothing in the report at all.
    #[test]
    fn a_failing_source_is_propagated_not_reported_as_a_malformed_file() {
        for (name, data) in [
            ("healthy", healthy_blob()),
            ("maker-note", maker_note_blob()),
        ] {
            let clean = ExifReader::new()
                .parse_from(&data[..])
                .expect("clean parse");
            let (mut failures, mut successes) = (0, 0);
            for budget in 0..40 {
                let source = FailingAfter {
                    inner: &data[..],
                    budget,
                };
                match ExifReader::new().parse_from_with_report(source) {
                    Ok((exif, report)) => {
                        successes += 1;
                        assert!(
                            report.is_empty(),
                            "{name} budget {budget}: a transport failure was blamed on the file: \
                             {:?}",
                            report.dropped()
                        );
                        // An `Ok` from a failing source must be the *whole* answer, not a quietly
                        // diminished one: the maker-note pin went missing exactly this way.
                        assert_eq!(
                            exif.maker_note_offset(),
                            clean.maker_note_offset(),
                            "{name} budget {budget}: the maker-note pin was silently lost"
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
