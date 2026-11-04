//! The parsed EXIF model: the IFD chain that makes up an EXIF blob.

use gamut_ifd::{ByteOrder, Ifd, Value};

use crate::gps::GpsInfo;
use crate::maker_note::{MakerNote, MakerNoteLayout, MakerNoteVendor};
use crate::tag::{ExifTag, IfdKind};
use crate::thumbnail::Thumbnail;
use crate::value::{Rational, as_text};

/// The 6-byte identifier that precedes the TIFF stream in a JPEG `APP1` EXIF segment.
pub(crate) const MARKER: &[u8] = b"Exif\x00\x00";
/// `ExifIFD` pointer (0th IFD → Exif sub-IFD), Exif 3.0 §4.6.3.
pub(crate) const EXIF_IFD_POINTER: u16 = gamut_ifd::tags::EXIF_IFD;
/// `GPSInfo` pointer (0th IFD → GPS sub-IFD).
pub(crate) const GPS_IFD_POINTER: u16 = gamut_ifd::tags::GPS_INFO;
/// `Interoperability` pointer (Exif sub-IFD → Interop sub-IFD).
pub(crate) const INTEROP_IFD_POINTER: u16 = gamut_ifd::tags::INTEROPERABILITY_IFD;

/// A parsed EXIF blob — the directories of the TIFF stream that follows the optional `Exif\0\0`
/// marker.
///
/// The 0th IFD (`image`) holds the primary-image tags and the pointer tags that reach the
/// Exif/GPS/Interop sub-IFDs; the 1st IFD holds the thumbnail. The byte order is preserved so a
/// re-serialised blob can match the source's endianness.
///
/// Read a blob with [`Exif::parse`] and re-emit it with [`Exif::to_bytes`]; build one from scratch
/// with [`Exif::new`]. Fields are private so the representation can evolve without breaking the 1.0
/// API — reach the directories through the accessors, which hand back the underlying
/// [`gamut_ifd::Ifd`] that consumers already speak.
///
/// The pointer tags — `ExifIFD` (`0x8769`), `GPSInfo` (`0x8825`), and `Interoperability`
/// (`0xA005`) — are **managed by the crate**: the writer synthesises them from the typed sub-IFDs,
/// so set the sub-IFDs (e.g. [`set_exif_ifd`](Self::set_exif_ifd)), never the pointer fields.
#[derive(Debug, Clone)]
pub struct Exif {
    order: ByteOrder,
    image: Ifd,
    exif: Option<Ifd>,
    gps: Option<Ifd>,
    interop: Option<Ifd>,
    thumbnail: Option<Thumbnail>,
    /// The absolute offset the out-of-line `MakerNote` value was read from in the source
    /// stream, if any — provenance the writer uses to pin the note in place on a rewrite.
    maker_note_at: Option<u64>,
}

impl PartialEq for Exif {
    fn eq(&self, other: &Self) -> bool {
        // `maker_note_at` is source provenance (where the note happened to sit in the parsed
        // stream), not content: models differing only there are equal.
        self.order == other.order
            && self.image == other.image
            && self.exif == other.exif
            && self.gps == other.gps
            && self.interop == other.interop
            && self.thumbnail == other.thumbnail
    }
}

impl Exif {
    /// Creates an EXIF blob with an empty 0th IFD and the given byte order.
    #[must_use]
    pub fn new(order: ByteOrder) -> Self {
        Self {
            order,
            image: Ifd::new(),
            exif: None,
            gps: None,
            interop: None,
            thumbnail: None,
            maker_note_at: None,
        }
    }

    /// Assembles an [`Exif`] from already-parsed directories (used by the reader).
    pub(crate) fn from_parts(
        order: ByteOrder,
        image: Ifd,
        exif: Option<Ifd>,
        gps: Option<Ifd>,
        interop: Option<Ifd>,
        thumbnail: Option<Thumbnail>,
        maker_note_at: Option<u64>,
    ) -> Self {
        Self {
            order,
            image,
            exif,
            gps,
            interop,
            thumbnail,
            maker_note_at,
        }
    }

    /// The absolute offset (within the source TIFF stream) the out-of-line `MakerNote` value
    /// was read from — recorded at parse time so a rewrite can pin the note at its original
    /// position, keeping vendor-internal absolute offsets valid. `None` for a model built from
    /// scratch, or when the note was absent or inline.
    #[must_use]
    pub fn maker_note_offset(&self) -> Option<u64> {
        self.maker_note_at
    }

    /// Parses an EXIF blob (with or without the `Exif\0\0` marker) with default options.
    ///
    /// Equivalent to [`ExifReader::new().parse(bytes)`](crate::ExifReader::parse). For control over
    /// marker handling or strictness, use [`ExifReader`](crate::ExifReader) directly.
    ///
    /// # Errors
    ///
    /// Returns an [`ExifError`](crate::ExifError) if the TIFF stream is malformed.
    pub fn parse(bytes: &[u8]) -> crate::Result<Self> {
        crate::ExifReader::new().parse(bytes)
    }

    /// Serialises this EXIF blob to a valid `Exif\0\0` + TIFF stream with default options.
    ///
    /// Preserves the source byte order and re-synthesises the Exif/GPS/Interop pointer tags. For a
    /// bare TIFF stream (PNG `eXIf` / WebP `EXIF`) or a byte-order override, use
    /// [`ExifWriter`](crate::ExifWriter).
    ///
    /// # Errors
    ///
    /// Returns [`ExifError::Ifd`](crate::ExifError::Ifd) if the model is not representable in
    /// classic-TIFF widths (see [`ExifWriter::write`](crate::ExifWriter::write)).
    pub fn to_bytes(&self) -> crate::Result<Vec<u8>> {
        crate::ExifWriter::new().write(self)
    }

    /// The byte order of the underlying TIFF stream (preserved for round-tripping).
    #[must_use]
    pub fn byte_order(&self) -> ByteOrder {
        self.order
    }

    /// The 0th IFD — primary-image / TIFF tags.
    #[must_use]
    pub fn image(&self) -> &Ifd {
        &self.image
    }

    /// The 0th IFD, mutably.
    pub fn image_mut(&mut self) -> &mut Ifd {
        &mut self.image
    }

    /// The Exif sub-IFD (capture parameters), if present.
    #[must_use]
    pub fn exif_ifd(&self) -> Option<&Ifd> {
        self.exif.as_ref()
    }

    /// The Exif sub-IFD, mutably, creating an empty one if absent.
    pub fn exif_ifd_mut(&mut self) -> &mut Ifd {
        self.exif.get_or_insert_with(Ifd::new)
    }

    /// The GPS sub-IFD, if present.
    #[must_use]
    pub fn gps_ifd(&self) -> Option<&Ifd> {
        self.gps.as_ref()
    }

    /// The GPS sub-IFD, mutably, creating an empty one if absent.
    pub fn gps_ifd_mut(&mut self) -> &mut Ifd {
        self.gps.get_or_insert_with(Ifd::new)
    }

    /// The Interoperability sub-IFD, if present.
    #[must_use]
    pub fn interop_ifd(&self) -> Option<&Ifd> {
        self.interop.as_ref()
    }

    /// The Interoperability sub-IFD, mutably, creating an empty one if absent.
    pub fn interop_ifd_mut(&mut self) -> &mut Ifd {
        self.interop.get_or_insert_with(Ifd::new)
    }

    /// The embedded thumbnail (1st IFD), if present.
    #[must_use]
    pub fn thumbnail(&self) -> Option<&Thumbnail> {
        self.thumbnail.as_ref()
    }

    /// The 1st IFD — the embedded thumbnail's directory, if present.
    #[must_use]
    pub fn thumbnail_ifd(&self) -> Option<&Ifd> {
        self.thumbnail.as_ref().map(Thumbnail::ifd)
    }

    /// The embedded JPEG thumbnail's compressed bytes, if there is a JPEG thumbnail.
    #[must_use]
    pub fn thumbnail_bytes(&self) -> Option<&[u8]> {
        self.thumbnail.as_ref().and_then(Thumbnail::jpeg)
    }

    /// Sets (or replaces) the embedded thumbnail to a JPEG image.
    pub fn set_thumbnail(&mut self, jpeg: Vec<u8>) {
        self.thumbnail = Some(Thumbnail::from_jpeg(jpeg));
    }

    /// Removes the embedded thumbnail, if any.
    pub fn clear_thumbnail(&mut self) {
        self.thumbnail = None;
    }

    /// Replaces the Exif sub-IFD.
    pub fn set_exif_ifd(&mut self, ifd: Ifd) {
        self.exif = Some(ifd);
    }

    /// Replaces the GPS sub-IFD.
    pub fn set_gps_ifd(&mut self, ifd: Ifd) {
        self.gps = Some(ifd);
    }

    /// Replaces the Interoperability sub-IFD.
    pub fn set_interop_ifd(&mut self, ifd: Ifd) {
        self.interop = Some(ifd);
    }

    /// Returns the value of `tag` in directory `ifd`, or `None` if that directory or tag is absent.
    #[must_use]
    pub fn get(&self, ifd: IfdKind, tag: u16) -> Option<&Value> {
        self.directory(ifd).and_then(|d| d.get(tag))
    }

    /// Sets `tag` to `value` in directory `ifd`, creating the sub-IFD if it does not yet exist.
    pub fn set(&mut self, ifd: IfdKind, tag: u16, value: Value) {
        self.directory_mut(ifd).set(tag, value);
    }

    /// Returns the value of a catalogued [`ExifTag`], looked up in its home directory.
    #[must_use]
    pub fn get_tag(&self, tag: ExifTag) -> Option<&Value> {
        self.get(tag.ifd(), tag.tag_id())
    }

    /// Sets a catalogued [`ExifTag`] to `value` in its home directory, creating the sub-IFD if
    /// needed.
    pub fn set_tag(&mut self, tag: ExifTag, value: Value) {
        self.set(tag.ifd(), tag.tag_id(), value);
    }

    /// The camera manufacturer (`Make`, 0th IFD).
    #[must_use]
    pub fn make(&self) -> Option<&str> {
        self.get_tag(ExifTag::Make).and_then(as_text)
    }

    /// The camera model (`Model`, 0th IFD).
    #[must_use]
    pub fn model(&self) -> Option<&str> {
        self.get_tag(ExifTag::Model).and_then(as_text)
    }

    /// The creating software (`Software`, 0th IFD).
    #[must_use]
    pub fn software(&self) -> Option<&str> {
        self.get_tag(ExifTag::Software).and_then(as_text)
    }

    /// The image orientation (`Orientation`, 0th IFD): 1–8 per the TIFF/Exif convention.
    #[must_use]
    pub fn orientation(&self) -> Option<u16> {
        self.get_tag(ExifTag::Orientation)
            .and_then(Value::as_u32)
            .and_then(|v| u16::try_from(v).ok())
    }

    /// The capture time (`DateTimeOriginal`, Exif IFD), as its raw `YYYY:MM:DD HH:MM:SS` string.
    #[must_use]
    pub fn datetime_original(&self) -> Option<&str> {
        self.get_tag(ExifTag::DateTimeOriginal).and_then(as_text)
    }

    /// The exposure time in seconds (`ExposureTime`, Exif IFD).
    #[must_use]
    pub fn exposure_time(&self) -> Option<Rational> {
        self.get_tag(ExifTag::ExposureTime).and_then(first_rational)
    }

    /// The lens f-number (`FNumber`, Exif IFD).
    #[must_use]
    pub fn f_number(&self) -> Option<Rational> {
        self.get_tag(ExifTag::FNumber).and_then(first_rational)
    }

    /// The ISO sensitivity (`PhotographicSensitivity`, Exif IFD).
    #[must_use]
    pub fn iso(&self) -> Option<u32> {
        self.get_tag(ExifTag::PhotographicSensitivity)
            .and_then(Value::as_u32)
    }

    /// The focal length in millimetres (`FocalLength`, Exif IFD).
    #[must_use]
    pub fn focal_length(&self) -> Option<Rational> {
        self.get_tag(ExifTag::FocalLength).and_then(first_rational)
    }

    /// The lens model (`LensModel`, Exif IFD).
    #[must_use]
    pub fn lens_model(&self) -> Option<&str> {
        self.get_tag(ExifTag::LensModel).and_then(as_text)
    }

    /// The GPS position as a typed [`GpsInfo`], or `None` if there is no GPS sub-IFD or it holds no
    /// positioning tags. The full GPS directory is always available via [`gps_ifd`](Self::gps_ifd).
    #[must_use]
    pub fn gps(&self) -> Option<GpsInfo> {
        GpsInfo::from_ifd(self.gps_ifd()?)
    }

    /// The `MakerNote` block as its opaque bytes plus the vendor detected from `Make`, or `None` if
    /// absent. Vendor tags are decoded only through a vendor view, [`MakerNote::canon`],
    /// [`MakerNote::leica`] or [`MakerNote::sony`]; see [`MakerNote`] for the round-trip caveat.
    #[must_use]
    pub fn maker_note(&self) -> Option<MakerNote> {
        let bytes = match self.get_tag(ExifTag::MakerNote)? {
            Value::Undefined(bytes) => bytes.clone(),
            _ => return None,
        };
        Some(MakerNote {
            vendor: MakerNoteVendor::detect(self.make()),
            bytes,
        })
    }

    /// The `MakerNote` block's dispatch layout — its dialect, IFD byte order and position, and
    /// offset base — resolved against this stream's [`byte_order`](Self::byte_order). `None` if
    /// there is no note or no dialect recognises it; see [`MakerNoteLayout::detect`].
    #[must_use]
    pub fn maker_note_layout(&self) -> Option<MakerNoteLayout> {
        self.maker_note()?.layout(self.order)
    }

    /// The directory for `kind`, if present.
    fn directory(&self, kind: IfdKind) -> Option<&Ifd> {
        match kind {
            IfdKind::Image => Some(&self.image),
            IfdKind::Exif => self.exif.as_ref(),
            IfdKind::Gps => self.gps.as_ref(),
            IfdKind::Interop => self.interop.as_ref(),
            IfdKind::Thumbnail => self.thumbnail.as_ref().map(Thumbnail::ifd),
        }
    }

    /// The directory for `kind`, creating (vivifying) it if absent.
    fn directory_mut(&mut self, kind: IfdKind) -> &mut Ifd {
        match kind {
            IfdKind::Image => &mut self.image,
            IfdKind::Exif => self.exif.get_or_insert_with(Ifd::new),
            IfdKind::Gps => self.gps.get_or_insert_with(Ifd::new),
            IfdKind::Interop => self.interop.get_or_insert_with(Ifd::new),
            IfdKind::Thumbnail => self
                .thumbnail
                .get_or_insert_with(|| Thumbnail::from_parts(Ifd::new(), None, None))
                .ifd_mut(),
        }
    }
}

/// Extracts the first element of a `RATIONAL` value as a [`Rational`], or `None` for other types.
fn first_rational(value: &Value) -> Option<Rational> {
    match value {
        Value::Rational(rs) => rs.first().copied().map(Rational::from),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The manual `PartialEq` compares every *content* field (each must independently break
    /// equality) while ignoring the recorded maker-note source offset (provenance).
    #[test]
    fn equality_covers_every_content_field() {
        let base = Exif::new(ByteOrder::LittleEndian);
        assert_eq!(base, base.clone());

        // Each content field flips inequality on its own, so none is silently excluded from the
        // comparison.
        let order = Exif::new(ByteOrder::BigEndian);
        assert_ne!(base, order);
        let mut image = base.clone();
        image.set_tag(ExifTag::Make, Value::Ascii("Canon".into()));
        assert_ne!(base, image);
        let mut exif_ifd = base.clone();
        exif_ifd.set_tag(ExifTag::FNumber, Value::Rational(vec![(28, 10)]));
        assert_ne!(base, exif_ifd);
        let mut gps = base.clone();
        gps.set_tag(ExifTag::GpsVersionId, Value::Byte(vec![2, 3, 0, 0]));
        assert_ne!(base, gps);
        let mut interop = base.clone();
        interop.set_tag(ExifTag::InteroperabilityIndex, Value::Ascii("R98".into()));
        assert_ne!(base, interop);
        let mut thumb = base.clone();
        thumb.set_thumbnail(vec![0xFF, 0xD8, 0xFF, 0xD9]);
        assert_ne!(base, thumb);
    }

    #[test]
    fn the_gps_accessor_lifts_the_sub_ifd() {
        // `GpsInfo::from_ifd` is well covered; `Exif::gps`, the accessor that reaches it, was not
        // -- it could return `None` unconditionally and every test still passed (#110). The two
        // are different claims: one is that the parse works, the other that this crate hands the
        // caller the parsed thing rather than nothing.
        let mut exif = Exif::new(ByteOrder::LittleEndian);
        assert_eq!(exif.gps(), None, "no GPS sub-IFD yet");

        exif.set_tag(ExifTag::GpsLatitudeRef, Value::Ascii("N".into()));
        exif.set_tag(
            ExifTag::GpsLatitude,
            Value::Rational(vec![(48, 1), (51, 1), (296, 10)]),
        );

        let gps = exif
            .gps()
            .expect("the accessor reaches the sub-IFD it just wrote");
        assert!((gps.latitude_deg().expect("latitude") - 48.858_222).abs() < 1e-5);
    }

    #[test]
    fn image_mut_edits_the_zeroth_ifd_in_place() {
        // The mutable accessor had no test that observed an edit through it, so returning a
        // detached directory (which is what the mutant does) was invisible: writes went somewhere
        // and the caller never checked they went *here*.
        let mut exif = Exif::new(ByteOrder::LittleEndian);
        exif.image_mut()
            .set(ExifTag::Make.tag_id(), Value::Ascii("Canon".into()));

        assert_eq!(
            exif.image().get(ExifTag::Make.tag_id()),
            Some(&Value::Ascii("Canon".into())),
            "the edit landed in the 0th IFD this Exif owns"
        );
    }

    #[test]
    fn equality_ignores_provenance() {
        // Where a model came from is not part of what it says. A parsed model equals a
        // from-scratch model with the same content even though only the former records a
        // maker-note offset -- otherwise round-tripping a file would appear to change it.
        let mut with_note = Exif::new(ByteOrder::LittleEndian);
        with_note.set_tag(
            ExifTag::MakerNote,
            Value::Undefined((0..32u8).collect::<Vec<u8>>()),
        );
        let parsed = Exif::parse(&with_note.to_bytes().expect("write")).expect("parse");

        assert!(parsed.maker_note_offset().is_some());
        assert!(with_note.maker_note_offset().is_none());
        assert_eq!(parsed, with_note);
    }

    #[test]
    fn typed_accessors_read_their_tags() {
        let mut exif = Exif::new(ByteOrder::LittleEndian);
        exif.set_tag(ExifTag::Make, Value::Ascii("Canon".into()));
        exif.set_tag(ExifTag::Model, Value::Ascii("R5".into()));
        exif.set_tag(ExifTag::Software, Value::Ascii("gamut".into()));
        exif.set_tag(ExifTag::Orientation, Value::Short(vec![6]));
        exif.set_tag(
            ExifTag::DateTimeOriginal,
            Value::Ascii("2024:06:14 09:30:00".into()),
        );
        exif.set_tag(ExifTag::ExposureTime, Value::Rational(vec![(1, 500)]));
        exif.set_tag(ExifTag::FNumber, Value::Rational(vec![(40, 10)]));
        exif.set_tag(ExifTag::PhotographicSensitivity, Value::Short(vec![800]));
        exif.set_tag(ExifTag::FocalLength, Value::Rational(vec![(85, 1)]));
        exif.set_tag(ExifTag::LensModel, Value::Utf8("RF85mm F1.2".into()));

        assert_eq!(exif.make(), Some("Canon"));
        assert_eq!(exif.model(), Some("R5"));
        assert_eq!(exif.software(), Some("gamut"));
        assert_eq!(exif.orientation(), Some(6));
        assert_eq!(exif.datetime_original(), Some("2024:06:14 09:30:00"));
        assert_eq!(exif.exposure_time(), Some(Rational { num: 1, den: 500 }));
        assert_eq!(exif.f_number(), Some(Rational { num: 40, den: 10 }));
        assert_eq!(exif.iso(), Some(800));
        assert_eq!(exif.focal_length(), Some(Rational { num: 85, den: 1 }));
        assert_eq!(exif.lens_model(), Some("RF85mm F1.2"));

        // Absent tags read back as None.
        let empty = Exif::new(ByteOrder::LittleEndian);
        assert_eq!(empty.make(), None);
        assert_eq!(empty.orientation(), None);
        assert_eq!(empty.f_number(), None);
        assert_eq!(empty.iso(), None);
        assert_eq!(empty.gps(), None);
        assert_eq!(empty.maker_note(), None);
        assert_eq!(empty.maker_note_layout(), None);
    }

    /// The layout inherits the stream's own byte order, and resolving it leaves the note — and
    /// the re-serialised blob — byte-identical.
    #[test]
    fn maker_note_layout_inherits_the_stream_order_and_changes_no_byte() {
        for order in [ByteOrder::LittleEndian, ByteOrder::BigEndian] {
            let mut exif = Exif::new(order);
            exif.set_tag(ExifTag::Make, Value::Ascii("Canon".into()));
            let note = vec![0x5A; 32];
            exif.set_tag(ExifTag::MakerNote, Value::Undefined(note.clone()));
            let bytes = exif.to_bytes().expect("serialise");
            let parsed = Exif::parse(&bytes).expect("parse");

            let layout = parsed.maker_note_layout().expect("Canon note has a layout");
            assert_eq!(layout.byte_order, order);
            assert_eq!(parsed.maker_note().expect("note").bytes, note);
            assert_eq!(parsed.to_bytes().expect("re-serialise"), bytes);
        }
    }

    #[test]
    fn sub_ifd_setters_and_mut_accessors() {
        let mut exif = Exif::new(ByteOrder::LittleEndian);

        let mut e = Ifd::new();
        e.set(ExifTag::FNumber.tag_id(), Value::Rational(vec![(28, 10)]));
        exif.set_exif_ifd(e);
        assert!(exif.exif_ifd().is_some());

        // interop_ifd / interop_ifd_mut / set_interop_ifd
        assert!(exif.interop_ifd().is_none());
        exif.interop_ifd_mut().set(
            ExifTag::InteroperabilityIndex.tag_id(),
            Value::Ascii("R98".into()),
        );
        assert!(exif.interop_ifd().is_some());
        let mut interop = Ifd::new();
        interop.set(
            ExifTag::InteroperabilityIndex.tag_id(),
            Value::Ascii("THM".into()),
        );
        exif.set_interop_ifd(interop);
        assert_eq!(
            exif.get(IfdKind::Interop, ExifTag::InteroperabilityIndex.tag_id()),
            Some(&Value::Ascii("THM".into()))
        );

        // gps_ifd_mut vivifies
        exif.gps_ifd_mut().set(
            ExifTag::GpsVersionId.tag_id(),
            Value::Byte(vec![2, 3, 0, 0]),
        );
        assert!(exif.gps_ifd().is_some());

        // thumbnail set / clear
        exif.set_thumbnail(vec![0xFF, 0xD8, 0xFF, 0xD9]);
        assert!(exif.thumbnail().is_some());
        exif.clear_thumbnail();
        assert!(exif.thumbnail().is_none());
        assert!(exif.thumbnail_bytes().is_none());
    }

    #[test]
    fn new_starts_empty_with_the_given_order() {
        let exif = Exif::new(ByteOrder::BigEndian);
        assert_eq!(exif.byte_order(), ByteOrder::BigEndian);
        assert!(exif.image().fields().is_empty());
        assert!(exif.exif_ifd().is_none());
        assert!(exif.gps_ifd().is_none());
        assert!(exif.interop_ifd().is_none());
        assert!(exif.thumbnail_ifd().is_none());
    }

    #[test]
    fn set_vivifies_sub_ifds_and_get_reads_back() {
        let mut exif = Exif::new(ByteOrder::LittleEndian);
        exif.set(IfdKind::Image, 0x010F, Value::Ascii("NIKON".into()));
        exif.set(IfdKind::Exif, 0x8827, Value::Short(vec![400]));
        exif.set(IfdKind::Gps, 0x0000, Value::Byte(vec![2, 3, 0, 0]));

        assert_eq!(
            exif.get(IfdKind::Image, 0x010F),
            Some(&Value::Ascii("NIKON".into()))
        );
        assert_eq!(
            exif.get(IfdKind::Exif, 0x8827),
            Some(&Value::Short(vec![400]))
        );
        // The Exif and GPS sub-IFDs were created on first write.
        assert!(exif.exif_ifd().is_some());
        assert!(exif.gps_ifd().is_some());
        // Absent directory / tag both read back as None.
        assert_eq!(exif.get(IfdKind::Interop, 0x0001), None);
        assert_eq!(exif.get(IfdKind::Image, 0x0100), None);
    }

    #[test]
    fn mut_accessors_vivify_and_set_replaces() {
        let mut exif = Exif::new(ByteOrder::LittleEndian);
        exif.exif_ifd_mut()
            .set(0x9000, Value::Undefined(b"0300".to_vec()));
        assert_eq!(
            exif.get(IfdKind::Exif, 0x9000),
            Some(&Value::Undefined(b"0300".to_vec()))
        );

        let mut replacement = Ifd::new();
        replacement.set(0x0001, Value::Byte(vec![b'N']));
        exif.set_gps_ifd(replacement);
        assert_eq!(
            exif.get(IfdKind::Gps, 0x0001),
            Some(&Value::Byte(vec![b'N']))
        );
    }

    #[test]
    fn set_tag_and_get_tag_route_by_home_ifd() {
        let mut exif = Exif::new(ByteOrder::LittleEndian);
        exif.set_tag(ExifTag::Make, Value::Ascii("Canon".into()));
        exif.set_tag(ExifTag::FNumber, Value::Rational(vec![(28, 10)]));

        // Make lives in the 0th IFD; FNumber in the Exif sub-IFD.
        assert_eq!(
            exif.get_tag(ExifTag::Make),
            Some(&Value::Ascii("Canon".into()))
        );
        assert_eq!(
            exif.get(IfdKind::Image, 0x010F),
            Some(&Value::Ascii("Canon".into()))
        );
        assert_eq!(
            exif.get_tag(ExifTag::FNumber),
            Some(&Value::Rational(vec![(28, 10)]))
        );
        assert!(exif.exif_ifd().is_some());
        assert_eq!(exif.get_tag(ExifTag::Model), None);
    }
}
