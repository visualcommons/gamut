//! Reading an EXIF blob into the typed [`Exif`] model.
//!
//! An EXIF blob is an optional `Exif\0\0` marker followed by a TIFF stream. The 0th IFD and (when
//! present) the 1st IFD are the top-level chain; the Exif, GPS, and Interoperability directories
//! hang off pointer *tags* that the generic TIFF reader cannot follow (it cannot know which
//! `LONG`s are offsets), so this reader chases those pointers explicitly and removes them,
//! representing each sub-IFD structurally on [`Exif`] instead.
//!
//! This module holds the reader's options and its `&[u8]` entry points. The parse itself is
//! generic over [`gamut_ifd::ReadAt`] and lives in the crate's private `stream` module; a slice is
//! simply one such source, so there is exactly **one** parse engine and the two entry points cannot
//! drift.

use crate::error::Result;
use crate::exif::Exif;
use crate::report::ReadReport;

/// Reads an EXIF blob into an [`Exif`], with options for how the parse is bounded.
///
/// The default ([`ExifReader::new`]) accepts a blob with or without the `Exif\0\0` marker and is
/// lenient: a malformed Exif/GPS/Interop sub-IFD is dropped rather than failing the whole parse.
#[derive(Debug, Clone, Default)]
pub struct ExifReader {
    pub(crate) require_marker: bool,
    pub(crate) strict: bool,
}

impl ExifReader {
    /// A reader with default options (marker optional, lenient sub-IFD handling).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Requires the `Exif\0\0` marker; a bare TIFF stream is then rejected with
    /// [`ExifError::MissingMarker`](crate::ExifError::MissingMarker).
    ///
    /// Off by default: the JPEG `APP1` segment carries the marker, but the WebP `EXIF` and PNG
    /// `eXIf` chunks carry a bare TIFF stream.
    #[must_use]
    pub fn require_marker(mut self, yes: bool) -> Self {
        self.require_marker = yes;
        self
    }

    /// In strict mode a malformed Exif/GPS/Interop sub-IFD fails the parse; by default it is
    /// dropped and the rest of the blob is returned.
    #[must_use]
    pub fn strict(mut self, yes: bool) -> Self {
        self.strict = yes;
        self
    }

    /// Parses an EXIF blob into an [`Exif`].
    ///
    /// The `&[u8]` case of [`parse_from`](Self::parse_from) — a slice is a
    /// [`ReadAt`](gamut_ifd::ReadAt) source — so the two share one parse engine.
    ///
    /// # Errors
    ///
    /// Returns [`ExifError::MissingMarker`](crate::ExifError::MissingMarker) when the marker is
    /// required but absent, an [`ExifError::Ifd`](crate::ExifError::Ifd) when the TIFF stream is
    /// malformed, or (in [`strict`](Self::strict) mode)
    /// [`ExifError::InvalidIfd`](crate::ExifError::InvalidIfd) when a sub-IFD pointer addresses a
    /// malformed directory.
    ///
    /// An offset inside an error message is a position in `bytes` — the buffer the caller handed
    /// in — so for a marked blob it counts the six-byte `Exif\0\0` marker. That is deliberately a
    /// different frame from [`Dropped::offset`](crate::Dropped::offset), which is relative to the
    /// start of the TIFF stream and therefore six smaller for the same position: a diagnostic
    /// points into the caller's own bytes, while a report offset addresses the TIFF structure.
    pub fn parse(&self, bytes: &[u8]) -> Result<Exif> {
        self.parse_from(bytes)
    }

    /// Parses an EXIF blob and reports what a lenient parse discarded.
    ///
    /// [`parse`](Self::parse) is silent about what leniency drops; this returns the same [`Exif`]
    /// alongside a [`ReadReport`] naming each discarded region, so a caller can tell a blob that
    /// never carried GPS from one whose GPS pointer was dangling.
    ///
    /// The report covers the regions [`DroppedRegion`](crate::DroppedRegion) enumerates and is
    /// complete over them. It is **not** a byte-completeness verdict, and an empty report does not
    /// mean the parse lost nothing — see the [`report`](crate::report) module for the two known
    /// losses below this crate.
    ///
    /// ```
    /// # use gamut_exif::{ByteOrder, Exif, ExifReader};
    /// # let bytes = Exif::new(ByteOrder::LittleEndian).to_bytes()?;
    /// let (exif, report) = ExifReader::new().parse_with_report(&bytes)?;
    /// assert!(report.is_empty()); // no covered region was discarded
    /// for dropped in report.dropped() {
    ///     eprintln!("{dropped}");
    /// }
    /// # let _ = exif;
    /// # Ok::<(), gamut_exif::ExifError>(())
    /// ```
    ///
    /// # Errors
    ///
    /// As [`parse`](Self::parse). In [`strict`](Self::strict) mode the first *malformed* region
    /// fails the parse instead of being reported — but a strict report is **not** therefore always
    /// empty: [`DroppedRegion::TrailingIfd`](crate::DroppedRegion::TrailingIfd) is well-formed and
    /// merely unrepresentable, so strictness has no grounds to reject it and it is reported in both
    /// modes.
    pub fn parse_with_report(&self, bytes: &[u8]) -> Result<(Exif, ReadReport)> {
        self.parse_from_with_report(bytes)
    }
}

#[cfg(test)]
mod tests {
    use gamut_ifd::{ByteOrder, Ifd, TiffFile, Value, Variant, write};

    use super::*;
    use crate::error::ExifError;
    use crate::exif::{EXIF_IFD_POINTER, GPS_IFD_POINTER, INTEROP_IFD_POINTER, MARKER};
    use crate::tag::{ExifTag, IfdKind};

    /// Builds a small but structurally complete EXIF TIFF stream (0th IFD with Make/Orientation,
    /// an Exif sub-IFD with FNumber + a nested Interop sub-IFD, a GPS sub-IFD, and a thumbnail
    /// 1st IFD), optionally prefixed with the `Exif\0\0` marker.
    fn sample_blob(order: ByteOrder, with_marker: bool) -> Vec<u8> {
        let mut image = Ifd::new();
        image.set(0x010F, Value::Ascii("Canon".into())); // Make
        image.set(0x0112, Value::Short(vec![1])); // Orientation

        let mut interop = Ifd::new();
        interop.set(0x0001, Value::Ascii("R98".into())); // InteroperabilityIndex

        let mut exif = Ifd::new();
        exif.set(0x829D, Value::Rational(vec![(28, 10)])); // FNumber
        exif.set(0x8827, Value::Short(vec![400])); // PhotographicSensitivity (ISO)
        exif.set_sub_ifd(INTEROP_IFD_POINTER, vec![interop]);

        let mut gps = Ifd::new();
        gps.set(0x0000, Value::Byte(vec![2, 3, 0, 0])); // GPSVersionID

        image.set_sub_ifd(EXIF_IFD_POINTER, vec![exif]);
        image.set_sub_ifd(GPS_IFD_POINTER, vec![gps]);

        let mut thumb = Ifd::new();
        thumb.set(0x0103, Value::Short(vec![6])); // Compression = JPEG

        let bytes = write(&TiffFile {
            order,
            variant: Variant::Classic,
            ifds: vec![image, thumb],
        })
        .expect("write");
        if with_marker {
            let mut out = MARKER.to_vec();
            out.extend(bytes);
            out
        } else {
            bytes
        }
    }

    fn assert_parsed(exif: &Exif, order: ByteOrder) {
        assert_eq!(exif.byte_order(), order);
        assert_eq!(exif.make(), Some("Canon"));
        assert_eq!(exif.orientation(), Some(1));
        // Sub-IFDs were followed and typed accessors reach into them.
        assert_eq!(exif.f_number(), Some(crate::Rational { num: 28, den: 10 }));
        assert_eq!(exif.iso(), Some(400));
        assert!(exif.gps_ifd().is_some());
        assert_eq!(
            exif.get(IfdKind::Interop, 0x0001),
            Some(&Value::Ascii("R98".into()))
        );
        assert!(exif.thumbnail_ifd().is_some());
        // The pointer tags were stripped — they are represented structurally, not as data.
        assert_eq!(exif.get(IfdKind::Image, EXIF_IFD_POINTER), None);
        assert_eq!(exif.get(IfdKind::Image, GPS_IFD_POINTER), None);
        assert_eq!(exif.get(IfdKind::Exif, INTEROP_IFD_POINTER), None);
    }

    #[test]
    fn parses_both_byte_orders_with_marker() {
        for order in [ByteOrder::LittleEndian, ByteOrder::BigEndian] {
            let exif = Exif::parse(&sample_blob(order, true)).expect("parse");
            assert_parsed(&exif, order);
        }
    }

    #[test]
    fn parses_bare_tiff_without_marker() {
        let exif = Exif::parse(&sample_blob(ByteOrder::LittleEndian, false)).expect("parse");
        assert_parsed(&exif, ByteOrder::LittleEndian);
    }

    #[test]
    fn require_marker_rejects_bare_tiff() {
        let bare = sample_blob(ByteOrder::LittleEndian, false);
        let err = ExifReader::new()
            .require_marker(true)
            .parse(&bare)
            .expect_err("bare TIFF must be rejected");
        assert!(matches!(err, ExifError::MissingMarker));
        // ...but the same reader still accepts the marked form.
        let marked = sample_blob(ByteOrder::LittleEndian, true);
        assert!(
            ExifReader::new()
                .require_marker(true)
                .parse(&marked)
                .is_ok()
        );
    }

    #[test]
    fn malformed_stream_errors() {
        assert!(Exif::parse(b"not tiff at all").is_err());
        assert!(Exif::parse(&[]).is_err());
    }

    /// The thumbnail's JPEG range is bounds-checked, and the two modes disagree about it.
    ///
    /// `read_thumbnail` documents that "in lenient mode an out-of-bounds JPEG range yields a
    /// thumbnail without bytes; in strict mode it errors" -- and **neither branch had a test**
    /// (#110). Both mutation directions of that `if self.strict` guard survived, which is what
    /// "documented behaviour, zero coverage" looks like from the outside.
    ///
    /// This is a decode path fed untrusted input, so the bound is the point: without it the slice
    /// would be taken from a hostile offset.
    #[test]
    fn an_out_of_bounds_thumbnail_jpeg_is_dropped_leniently_and_rejected_strictly() {
        let mut image = Ifd::new();
        image.set(0x010F, Value::Ascii("Canon".into()));
        let mut thumb = Ifd::new();
        // A JPEG that claims to start far past the end of the stream.
        thumb.set(
            ExifTag::JpegInterchangeFormat.tag_id(),
            Value::Long(vec![0xFFFF]),
        );
        thumb.set(
            ExifTag::JpegInterchangeFormatLength.tag_id(),
            Value::Long(vec![16]),
        );
        let bytes = write(&TiffFile {
            order: ByteOrder::LittleEndian,
            variant: Variant::Classic,
            ifds: vec![image, thumb],
        })
        .expect("write");

        // Lenient: the thumbnail survives, minus the bytes it could not have.
        let lenient = ExifReader::new().parse(&bytes).expect("lenient parse");
        let t = lenient
            .thumbnail()
            .expect("the 1st IFD is still a thumbnail");
        assert_eq!(t.jpeg(), None, "no bytes, rather than bytes from nowhere");
        assert_eq!(
            lenient.make(),
            Some("Canon"),
            "the rest of the file survives"
        );

        // Strict: the same input is refused.
        let err = ExifReader::new()
            .strict(true)
            .parse(&bytes)
            .expect_err("strict must reject an out-of-bounds thumbnail");
        assert!(
            matches!(err, ExifError::BadThumbnail(_)),
            "wrong error for an out-of-bounds thumbnail: {err:?}"
        );
    }

    /// A thumbnail offset with no length is an unreadable range, and strict mode says so.
    ///
    /// A `JPEGInterchangeFormat` with nothing to size the read by addresses bytes that cannot be
    /// fetched, so it fails strictness for the same reason an out-of-bounds range does — the
    /// sibling case above — rather than passing as a thumbnail that simply has no bytes. The
    /// message is pinned because it must state that structural fact and *not* claim a missing
    /// mandatory tag: Exif 3.0 §4.6.9.2 Table 21 makes the pair mandatory only under
    /// `Compression = Compressed`, and forbids recording either tag under the uncompressed
    /// columns (issue #574). The lenient half of the contract is the report, pinned in
    /// `tests/report.rs`.
    #[test]
    fn a_thumbnail_offset_without_a_length_is_rejected_strictly() {
        let mut image = Ifd::new();
        image.set(0x010F, Value::Ascii("Canon".into()));
        let mut thumb = Ifd::new();
        thumb.set(ExifTag::Compression.tag_id(), Value::Short(vec![6]));
        thumb.set(
            ExifTag::JpegInterchangeFormat.tag_id(),
            Value::Long(vec![4]),
        );
        // ...and deliberately no JpegInterchangeFormatLength.
        let bytes = write(&TiffFile {
            order: ByteOrder::LittleEndian,
            variant: Variant::Classic,
            ifds: vec![image, thumb],
        })
        .expect("write");

        let err = ExifReader::new()
            .strict(true)
            .parse(&bytes)
            .expect_err("strict must reject half a thumbnail pair");
        assert_eq!(
            err.to_string(),
            "invalid thumbnail: JPEGInterchangeFormat offset with no length to size it",
            "the message must name the unreadable range, not a missing mandatory tag"
        );
    }

    #[test]
    fn lenient_drops_a_dangling_sub_ifd_pointer_that_strict_rejects() {
        // An ExifIFD pointer that addresses far past the end of the stream.
        let mut image = Ifd::new();
        image.set(0x010F, Value::Ascii("Canon".into()));
        image.set(EXIF_IFD_POINTER, Value::Long(vec![0xFFFF]));
        let bytes = write(&TiffFile {
            order: ByteOrder::LittleEndian,
            variant: Variant::Classic,
            ifds: vec![image],
        })
        .expect("write");

        // Lenient: the bad pointer is dropped, the rest survives.
        let lenient = ExifReader::new().parse(&bytes).expect("lenient parse");
        assert_eq!(lenient.make(), Some("Canon"));
        assert!(lenient.exif_ifd().is_none());
        assert_eq!(lenient.get(IfdKind::Image, EXIF_IFD_POINTER), None);

        // Strict: the malformed sub-IFD fails the parse.
        let err = ExifReader::new()
            .strict(true)
            .parse(&bytes)
            .expect_err("strict must reject");
        assert!(matches!(err, ExifError::InvalidIfd("Exif")));
    }
}
