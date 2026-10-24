//! A read-only view of a Canon `MakerNote` (issue #977).
//!
//! A Canon note is a bare IFD at byte 0 of the block, in the outer stream's byte order, whose value
//! offsets count from the outer TIFF header ([`MakerNoteDialect::Canon`]). The view decodes that IFD
//! through [`gamut_ifd`] at the layout [`MakerNoteLayout::detect`] resolves, keeps every entry as a
//! [`Value`] ([`CanonMakerNote::ifd`]), and adds typed accessors for the tags staked in
//! `references/exif/canon/README.md`, after exiv2's `CanonMakerNote::tagInfo_[]`
//! (`third_party/exiv2/src/canonmn_int.cpp`), which is also the differential oracle.
//!
//! Values are returned raw: `model_id` is the number exiv2 prints as a camera name, and the binary
//! arrays (`camera_settings`, `shot_info`) are returned whole, not split into the per-element keys
//! exiv2 gives them.
//!
//! The view never writes. The block it was read from re-emits byte-identically, because nothing
//! here changes it; see the [module docs](super) on how the writer keeps Canon's file-absolute
//! offsets valid.

use gamut_ifd::{Ifd, ReadAt, Value};

use super::{MakerNoteDialect, MakerNoteLayout, read_note_ifd};
use crate::error::Result;
use crate::value::as_text;

/// `CameraSettings`: a `SHORT` array whose element 0 is its own byte length.
const CAMERA_SETTINGS: u16 = 0x0001;
/// `FocalLength`: a `SHORT` array (focal type, focal length, focal-plane sizes).
const FOCAL_LENGTH: u16 = 0x0002;
/// `ShotInfo`: a `SHORT` array whose element 0 is its own byte length.
const SHOT_INFO: u16 = 0x0004;
/// `ImageType`: text.
const IMAGE_TYPE: u16 = 0x0006;
/// `FirmwareVersion`: text.
const FIRMWARE_VERSION: u16 = 0x0007;
/// `FileNumber`: a `LONG`.
const FILE_NUMBER: u16 = 0x0008;
/// `OwnerName`: text.
const OWNER_NAME: u16 = 0x0009;
/// `SerialNumber`: a `LONG`.
const SERIAL_NUMBER: u16 = 0x000c;
/// `ModelID`: a `LONG`.
const MODEL_ID: u16 = 0x0010;
/// `LensModel`: text.
const LENS_MODEL: u16 = 0x0095;
/// `InternalSerialNumber`: text.
const INTERNAL_SERIAL_NUMBER: u16 = 0x0096;

/// The decoded IFD of a Canon `MakerNote`: every entry, plus typed accessors for the tags the
/// [module docs](self) name.
///
/// Built by [`MakerNote::canon`](super::MakerNote::canon) from the note's own bytes, or by
/// [`read`](Self::read) from the whole TIFF stream.
#[derive(Debug, Clone, PartialEq)]
pub struct CanonMakerNote {
    ifd: Ifd,
    unreadable: Vec<u16>,
}

impl CanonMakerNote {
    /// Decodes the Canon note laid out as `layout` that starts at `note_at` in `stream` — the TIFF
    /// stream the note sits in (the bytes after any `Exif\0\0` marker), or any [`ReadAt`] in that
    /// stream's offset space.
    ///
    /// `Ok(None)` when `layout` is not [`MakerNoteDialect::Canon`]. An entry whose value cannot be
    /// read is left out of [`ifd`](Self::ifd) and named by
    /// [`unreadable_tags`](Self::unreadable_tags), as exiv2 skips it too.
    ///
    /// # Errors
    ///
    /// [`ExifError::Ifd`](crate::ExifError::Ifd) when the note's IFD — entry count, entries and
    /// next-IFD pointer — does not fit in `stream`, or `stream` fails;
    /// [`ExifError::BadOffset`](crate::ExifError::BadOffset) when `note_at` plus the IFD's offset
    /// overflows.
    pub fn read<S: ReadAt>(
        stream: S,
        note_at: u64,
        layout: MakerNoteLayout,
    ) -> Result<Option<Self>> {
        if layout.dialect != MakerNoteDialect::Canon {
            return Ok(None);
        }
        let (ifd, unreadable) = read_note_ifd(stream, note_at, layout)?;
        Ok(Some(Self { ifd, unreadable }))
    }

    /// Every entry of the note's IFD that could be read, by tag.
    #[must_use]
    pub fn ifd(&self) -> &Ifd {
        &self.ifd
    }

    /// The tags of the entries whose values could not be read (an offset past the source, or a
    /// malformed value), in on-disk order. Empty for a well-formed note.
    #[must_use]
    pub fn unreadable_tags(&self) -> &[u16] {
        &self.unreadable
    }

    /// `CameraSettings` (`0x0001`): the whole `SHORT` array, element 0 being its byte length.
    #[must_use]
    pub fn camera_settings(&self) -> Option<&[u16]> {
        self.shorts(CAMERA_SETTINGS)
    }

    /// `FocalLength` (`0x0002`): the whole `SHORT` array.
    #[must_use]
    pub fn focal_length(&self) -> Option<&[u16]> {
        self.shorts(FOCAL_LENGTH)
    }

    /// `ShotInfo` (`0x0004`): the whole `SHORT` array, element 0 being its byte length.
    #[must_use]
    pub fn shot_info(&self) -> Option<&[u16]> {
        self.shorts(SHOT_INFO)
    }

    /// `ImageType` (`0x0006`), e.g. `"Canon EOS R5"`.
    #[must_use]
    pub fn image_type(&self) -> Option<&str> {
        self.text(IMAGE_TYPE)
    }

    /// `FirmwareVersion` (`0x0007`), e.g. `"Firmware Version 1.8.1"`.
    #[must_use]
    pub fn firmware_version(&self) -> Option<&str> {
        self.text(FIRMWARE_VERSION)
    }

    /// `FileNumber` (`0x0008`).
    #[must_use]
    pub fn file_number(&self) -> Option<u32> {
        self.ifd.get_u32(FILE_NUMBER)
    }

    /// `OwnerName` (`0x0009`).
    #[must_use]
    pub fn owner_name(&self) -> Option<&str> {
        self.text(OWNER_NAME)
    }

    /// `SerialNumber` (`0x000c`).
    #[must_use]
    pub fn serial_number(&self) -> Option<u32> {
        self.ifd.get_u32(SERIAL_NUMBER)
    }

    /// `ModelID` (`0x0010`): Canon's camera-model number (`0x80000421` is the EOS R5).
    #[must_use]
    pub fn model_id(&self) -> Option<u32> {
        self.ifd.get_u32(MODEL_ID)
    }

    /// `LensModel` (`0x0095`).
    #[must_use]
    pub fn lens_model(&self) -> Option<&str> {
        self.text(LENS_MODEL)
    }

    /// `InternalSerialNumber` (`0x0096`).
    #[must_use]
    pub fn internal_serial_number(&self) -> Option<&str> {
        self.text(INTERNAL_SERIAL_NUMBER)
    }

    /// `tag` as a `SHORT` array, or `None` if absent or of another type.
    fn shorts(&self, tag: u16) -> Option<&[u16]> {
        match self.ifd.get(tag)? {
            Value::Short(values) => Some(values),
            _ => None,
        }
    }

    /// `tag` as text up to its first NUL. Canon writes its text fields fixed-width and NUL-padded
    /// (`OwnerName` is 32 bytes), and exiv2 reports the same prefix; the padding stays in
    /// [`ifd`](Self::ifd). The prefix is taken from the decoded value, so a field holding any
    /// non-UTF-8 byte — padding after the NUL included — is already in
    /// [`unreadable_tags`](Self::unreadable_tags) and reads as `None`, where exiv2 still reports
    /// the prefix (the divergence `references/exif/canon/README.md` records).
    fn text(&self, tag: u16) -> Option<&str> {
        as_text(self.ifd.get(tag)?)?.split('\0').next()
    }
}

#[cfg(test)]
mod tests {
    use gamut_ifd::ByteOrder::{BigEndian as BE, LittleEndian as LE};
    use gamut_ifd::{ByteOrder, FieldType};

    use super::super::{MakerNote, MakerNoteVendor, OffsetBase};
    use super::*;

    /// One IFD entry to lay out: tag, field type, count, and the value's bytes in the note's order.
    struct Entry {
        tag: u16,
        ty: FieldType,
        count: u32,
        bytes: Vec<u8>,
    }

    fn shorts(tag: u16, values: &[u16], order: ByteOrder) -> Entry {
        let bytes = values.iter().flat_map(|&v| order.pack_u16(v)).collect();
        Entry {
            tag,
            ty: FieldType::Short,
            count: values.len() as u32,
            bytes,
        }
    }

    fn long(tag: u16, value: u32, order: ByteOrder) -> Entry {
        Entry {
            tag,
            ty: FieldType::Long,
            count: 1,
            bytes: order.pack_u32(value).to_vec(),
        }
    }

    fn ascii(tag: u16, text: &[u8]) -> Entry {
        Entry {
            tag,
            ty: FieldType::Ascii,
            count: text.len() as u32,
            bytes: text.to_vec(),
        }
    }

    /// A Canon note: the IFD at byte 0, a zero next-IFD pointer, then each out-of-line value,
    /// addressed as if the note stood at `note_at` in its TIFF stream.
    fn canon_note(entries: &[Entry], order: ByteOrder, note_at: u32) -> Vec<u8> {
        let count = entries.len() as u32;
        let mut data_at = 2 + 12 * count + 4;
        let (mut ifd, mut data) = (order.pack_u16(count as u16).to_vec(), Vec::new());
        for entry in entries {
            ifd.extend_from_slice(&order.pack_u16(entry.tag));
            ifd.extend_from_slice(&order.pack_u16(entry.ty.code()));
            ifd.extend_from_slice(&order.pack_u32(entry.count));
            if entry.bytes.len() <= 4 {
                let mut word = entry.bytes.clone();
                word.resize(4, 0);
                ifd.extend_from_slice(&word);
            } else {
                ifd.extend_from_slice(&order.pack_u32(note_at + data_at));
                data.extend_from_slice(&entry.bytes);
                data_at += entry.bytes.len() as u32;
            }
        }
        ifd.extend_from_slice(&[0; 4]);
        ifd.extend_from_slice(&data);
        ifd
    }

    fn maker(bytes: Vec<u8>) -> MakerNote {
        MakerNote {
            vendor: MakerNoteVendor::Canon,
            bytes,
        }
    }

    /// One entry per accessor, each holding a value no other accessor's tag holds, so reading the
    /// wrong tag is always visible.
    fn every_tag(order: ByteOrder) -> Vec<Entry> {
        vec![
            shorts(CAMERA_SETTINGS, &[8, 2, 0, 3], order),
            shorts(FOCAL_LENGTH, &[1, 50, 0, 0], order),
            shorts(SHOT_INFO, &[6, 160, 192], order),
            ascii(IMAGE_TYPE, b"Canon EOS R5\0"),
            ascii(FIRMWARE_VERSION, b"Firmware Version 1.8.1\0"),
            long(FILE_NUMBER, 1_000_123, order),
            ascii(OWNER_NAME, b"Ada\0\0\0\0\0\0\0\0\0\0\0\0\0"),
            long(SERIAL_NUMBER, 22_334_455, order),
            long(MODEL_ID, 0x8000_0421, order),
            ascii(LENS_MODEL, b"RF24-105mm F4 L IS USM\0"),
            ascii(INTERNAL_SERIAL_NUMBER, b"XA1234567\0"),
        ]
    }

    #[test]
    fn every_accessor_reads_its_own_tag() {
        for order in [LE, BE] {
            let note = maker(canon_note(&every_tag(order), order, 200));
            let view = note.canon(order, 200).expect("reads").expect("is Canon");
            assert_eq!(view.camera_settings(), Some(&[8, 2, 0, 3][..]));
            assert_eq!(view.focal_length(), Some(&[1, 50, 0, 0][..]));
            assert_eq!(view.shot_info(), Some(&[6, 160, 192][..]));
            assert_eq!(view.image_type(), Some("Canon EOS R5"));
            assert_eq!(view.firmware_version(), Some("Firmware Version 1.8.1"));
            assert_eq!(view.file_number(), Some(1_000_123));
            assert_eq!(view.owner_name(), Some("Ada"));
            assert_eq!(view.serial_number(), Some(22_334_455));
            assert_eq!(view.model_id(), Some(0x8000_0421));
            assert_eq!(view.lens_model(), Some("RF24-105mm F4 L IS USM"));
            assert_eq!(view.internal_serial_number(), Some("XA1234567"));
            assert_eq!(view.ifd().fields().len(), 11);
            assert!(view.unreadable_tags().is_empty());
        }
    }

    #[test]
    fn an_absent_or_retyped_tag_reads_as_none() {
        let order = LE;
        let note = maker(canon_note(
            &[
                long(CAMERA_SETTINGS, 7, order),
                shorts(IMAGE_TYPE, &[1, 2, 3], order),
            ],
            order,
            0,
        ));
        let view = note.canon(order, 0).expect("reads").expect("is Canon");
        assert_eq!(view.camera_settings(), None);
        assert_eq!(view.image_type(), None);
        assert_eq!(view.lens_model(), None);
        // The raw value is still there under its tag.
        assert_eq!(view.ifd().get(CAMERA_SETTINGS), Some(&Value::Long(vec![7])));
    }

    #[test]
    fn text_keeps_the_padding_in_the_raw_value() {
        let order = BE;
        let note = maker(canon_note(&[ascii(OWNER_NAME, b"Ada\0\0\0\0\0")], order, 0));
        let view = note.canon(order, 0).expect("reads").expect("is Canon");
        assert_eq!(view.owner_name(), Some("Ada"));
        assert_eq!(
            view.ifd().get(OWNER_NAME),
            Some(&Value::Ascii("Ada\0\0\0\0".into()))
        );
    }

    /// The recorded divergence from exiv2 (`references/exif/canon/README.md`): `gamut-ifd` decodes
    /// an `ASCII` value whole, so a byte that is not UTF-8 anywhere in the field — even in the
    /// padding after the first NUL — makes the entry unreadable rather than yielding the prefix.
    #[test]
    fn non_utf8_padding_makes_the_text_entry_unreadable() {
        let order = LE;
        let note = maker(canon_note(
            &[
                ascii(OWNER_NAME, b"Ada\0\xff\xfe\0\0"),
                long(FILE_NUMBER, 1, order),
            ],
            order,
            0,
        ));
        let view = note.canon(order, 0).expect("reads").expect("is Canon");
        assert_eq!(view.owner_name(), None);
        assert_eq!(view.unreadable_tags(), &[OWNER_NAME]);
        assert_eq!(view.ifd().get(OWNER_NAME), None);
        // The rest of the note is still read.
        assert_eq!(view.file_number(), Some(1));
    }

    #[test]
    fn offsets_count_from_the_tiff_header() {
        let order = LE;
        let bytes = canon_note(&every_tag(order), order, 200);
        // Read as if the note stood at the stream's start, every out-of-line value points past it.
        let view = maker(bytes.clone())
            .canon(order, 0)
            .expect("reads")
            .expect("is Canon");
        assert_eq!(view.image_type(), None);
        assert_eq!(view.unreadable_tags().first(), Some(&CAMERA_SETTINGS));
        // An inline value has no offset and reads wherever the note stands.
        assert_eq!(view.file_number(), Some(1_000_123));
        // The whole stream, with the note at 200, reads the same as the note alone.
        let mut stream = vec![0; 200];
        stream.extend_from_slice(&bytes);
        let layout = maker(bytes.clone()).layout(order).expect("Canon layout");
        let read = CanonMakerNote::read(&stream[..], 200, layout).expect("reads");
        assert_eq!(read, maker(bytes).canon(order, 200).expect("reads"));
    }

    #[test]
    fn a_value_before_the_note_is_unreadable_from_its_bytes_alone() {
        let order = LE;
        // The value claims offset 50, before a note standing at 200.
        let mut bytes = canon_note(&[ascii(IMAGE_TYPE, b"Canon EOS R5\0")], order, 200);
        bytes[10..14].copy_from_slice(&order.pack_u32(50));
        let view = maker(bytes)
            .canon(order, 200)
            .expect("reads")
            .expect("is Canon");
        assert_eq!(view.unreadable_tags(), &[IMAGE_TYPE]);
        assert!(view.ifd().fields().is_empty());
    }

    #[test]
    fn a_note_without_its_next_ifd_pointer_is_an_error() {
        let order = LE;
        let mut bytes = canon_note(&[long(FILE_NUMBER, 1, order)], order, 0);
        bytes.truncate(14);
        assert!(maker(bytes).canon(order, 0).is_err());
    }

    #[test]
    fn only_a_canon_layout_is_read() {
        let order = LE;
        let bytes = canon_note(&[long(FILE_NUMBER, 1, order)], order, 0);
        let nikon = MakerNote {
            vendor: MakerNoteVendor::Nikon,
            bytes: bytes.clone(),
        };
        assert_eq!(nikon.canon(order, 0).expect("not an error"), None);
        let unknown = MakerNote {
            vendor: MakerNoteVendor::Unknown,
            bytes,
        };
        assert_eq!(unknown.canon(order, 0).expect("not an error"), None);
    }

    #[test]
    fn the_ifd_position_must_fit_and_follow_its_base() {
        let order = LE;
        let bytes = canon_note(&[long(FILE_NUMBER, 1, order)], order, 0);
        let layout = maker(bytes.clone()).layout(order).expect("Canon layout");
        let shifted = MakerNoteLayout {
            ifd_offset: 1,
            ..layout
        };
        assert!(matches!(
            CanonMakerNote::read(&bytes[..], u64::MAX, shifted),
            Err(crate::ExifError::BadOffset(u64::MAX))
        ));
        let late_base = MakerNoteLayout {
            offset_base: OffsetBase::MakerNote { at: 1 },
            ..layout
        };
        assert!(matches!(
            CanonMakerNote::read(&bytes[..], u64::MAX, late_base),
            Err(crate::ExifError::BadOffset(u64::MAX))
        ));
        let after_ifd = MakerNoteLayout {
            offset_base: OffsetBase::MakerNote { at: 1 },
            ..layout
        };
        assert!(matches!(
            CanonMakerNote::read(&bytes[..], 0, after_ifd),
            Err(crate::ExifError::BadOffset(0))
        ));
    }

    #[test]
    fn a_note_relative_base_reads_through_a_rebased_view() {
        let order = BE;
        // Values addressed from the note's own start, the note standing at 64 in the stream.
        let bytes = canon_note(&[ascii(LENS_MODEL, b"EF50mm f/1.8 STM\0")], order, 0);
        let mut stream = vec![0; 64];
        stream.extend_from_slice(&bytes);
        let layout = maker(bytes).layout(order).expect("Canon layout");
        let relative = MakerNoteLayout {
            offset_base: OffsetBase::MakerNote { at: 0 },
            ..layout
        };
        let view = CanonMakerNote::read(&stream[..], 64, relative)
            .expect("reads")
            .expect("Canon");
        assert_eq!(view.lens_model(), Some("EF50mm f/1.8 STM"));
    }
}
