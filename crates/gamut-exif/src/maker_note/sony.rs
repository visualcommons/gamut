//! A read-only view of a Sony `MakerNote` (issue #980).
//!
//! A Sony note without the `SONY DSC \0\0\0` header is a bare IFD at byte 0 of the block, in the
//! outer stream's byte order, ending in a next-IFD pointer, whose value offsets count from the
//! outer TIFF header ([`MakerNoteDialect::Sony2`]). The view decodes that IFD through [`gamut_ifd`]
//! at the layout [`MakerNoteLayout::detect`] resolves, keeps every entry as a [`Value`]
//! ([`SonyMakerNote::ifd`]), and adds typed accessors for the tags staked in
//! `references/exif/sony/README.md`, after exiv2's `SonyMakerNote::tagInfo_[]`
//! (`third_party/exiv2/src/sonymn_int.cpp`), which is also the differential oracle.
//!
//! A note *with* that header ([`MakerNoteDialect::Sony1`]) has the same tags, but its IFD ends
//! without a next-IFD pointer, which `gamut-ifd` cannot read yet (issue #1032); it gets its
//! layout and no view (issue #1033).
//!
//! Values are returned raw: `model_id` is the number exiv2 prints as a camera name, and the arrays
//! (`file_format`, `lens_spec`, `full_image_size`) come back whole.
//!
//! The view never writes. The block it was read from re-emits byte-identically, because nothing
//! here changes it; see the [module docs](super) on how the writer keeps Sony's file-absolute
//! offsets valid.

use gamut_ifd::{Ifd, ReadAt, Value};

use super::{MakerNoteDialect, MakerNoteLayout, read_note_ifd};
use crate::error::Result;
use crate::value::as_text;

/// `Quality`: a `LONG`.
const QUALITY: u16 = 0x0102;
/// `FlashExposureComp`: an `SRATIONAL`, in EV.
const FLASH_EXPOSURE_COMP: u16 = 0x0104;
/// `WhiteBalance`: a `LONG`.
const WHITE_BALANCE: u16 = 0x0115;
/// `Rating`: a `LONG`.
const RATING: u16 = 0x2002;
/// `Contrast`: an `SLONG`.
const CONTRAST: u16 = 0x2004;
/// `Saturation`: an `SLONG`.
const SATURATION: u16 = 0x2005;
/// `Sharpness`: an `SLONG`.
const SHARPNESS: u16 = 0x2006;
/// `Brightness`: an `SLONG`.
const BRIGHTNESS: u16 = 0x2007;
/// `SerialNumber`: text.
const SERIAL_NUMBER: u16 = 0x2031;
/// `FileFormat`: a `BYTE` array.
const FILE_FORMAT: u16 = 0xb000;
/// `SonyModelID`: a `SHORT`.
const MODEL_ID: u16 = 0xb001;
/// `CreativeStyle`: text.
const CREATIVE_STYLE: u16 = 0xb020;
/// `ColorTemperature`: a `LONG`, in kelvin.
const COLOR_TEMPERATURE: u16 = 0xb021;
/// `LensID`: a `LONG`.
const LENS_ID: u16 = 0xb027;
/// `LensSpec`: a `BYTE` array.
const LENS_SPEC: u16 = 0xb02a;
/// `FullImageSize`: a `LONG` array.
const FULL_IMAGE_SIZE: u16 = 0xb02b;

/// The decoded IFD of a Sony `MakerNote`: every entry, plus typed accessors for the tags the
/// [module docs](self) name.
///
/// Built by [`MakerNote::sony`](super::MakerNote::sony) from the note's own bytes, or by
/// [`read`](Self::read) from the whole TIFF stream.
#[derive(Debug, Clone, PartialEq)]
pub struct SonyMakerNote {
    ifd: Ifd,
    unreadable: Vec<u16>,
}

impl SonyMakerNote {
    /// Decodes the Sony note laid out as `layout` that starts at `note_at` in `stream` — the TIFF
    /// stream the note sits in (the bytes after any `Exif\0\0` marker), or any [`ReadAt`] in that
    /// stream's offset space.
    ///
    /// `Ok(None)` when `layout` is not [`MakerNoteDialect::Sony2`] — including a
    /// [`Sony1`](MakerNoteDialect::Sony1) note, which this view does not read yet (issue #1033).
    /// An entry whose value cannot be read is left out of [`ifd`](Self::ifd) and named by
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
        if layout.dialect != MakerNoteDialect::Sony2 {
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

    /// `Quality` (`0x0102`): Sony's image-quality code.
    #[must_use]
    pub fn quality(&self) -> Option<u32> {
        self.ifd.get_u32(QUALITY)
    }

    /// `FlashExposureComp` (`0x0104`): flash exposure compensation in EV, as the raw `SRATIONAL`
    /// `(numerator, denominator)`.
    #[must_use]
    pub fn flash_exposure_comp(&self) -> Option<(i32, i32)> {
        match self.ifd.get(FLASH_EXPOSURE_COMP)? {
            Value::SRational(values) => single(values),
            _ => None,
        }
    }

    /// `WhiteBalance` (`0x0115`): Sony's white-balance code.
    #[must_use]
    pub fn white_balance(&self) -> Option<u32> {
        self.ifd.get_u32(WHITE_BALANCE)
    }

    /// `Rating` (`0x2002`).
    #[must_use]
    pub fn rating(&self) -> Option<u32> {
        self.ifd.get_u32(RATING)
    }

    /// `Contrast` (`0x2004`).
    #[must_use]
    pub fn contrast(&self) -> Option<i32> {
        self.slong(CONTRAST)
    }

    /// `Saturation` (`0x2005`).
    #[must_use]
    pub fn saturation(&self) -> Option<i32> {
        self.slong(SATURATION)
    }

    /// `Sharpness` (`0x2006`).
    #[must_use]
    pub fn sharpness(&self) -> Option<i32> {
        self.slong(SHARPNESS)
    }

    /// `Brightness` (`0x2007`).
    #[must_use]
    pub fn brightness(&self) -> Option<i32> {
        self.slong(BRIGHTNESS)
    }

    /// `SerialNumber` (`0x2031`).
    #[must_use]
    pub fn serial_number(&self) -> Option<&str> {
        self.text(SERIAL_NUMBER)
    }

    /// `FileFormat` (`0xb000`): the whole `BYTE` array (four bytes in Sony's bodies).
    #[must_use]
    pub fn file_format(&self) -> Option<&[u8]> {
        self.bytes(FILE_FORMAT)
    }

    /// `SonyModelID` (`0xb001`): Sony's camera-model number (358 is the ILCE-9).
    #[must_use]
    pub fn model_id(&self) -> Option<u16> {
        match self.ifd.get(MODEL_ID)? {
            Value::Short(values) => single(values),
            _ => None,
        }
    }

    /// `CreativeStyle` (`0xb020`), e.g. `"Standard"`.
    #[must_use]
    pub fn creative_style(&self) -> Option<&str> {
        self.text(CREATIVE_STYLE)
    }

    /// `ColorTemperature` (`0xb021`), in kelvin (0 for automatic white balance).
    #[must_use]
    pub fn color_temperature(&self) -> Option<u32> {
        self.ifd.get_u32(COLOR_TEMPERATURE)
    }

    /// `LensID` (`0xb027`): the lens-type number exiv2 prints as a lens name.
    #[must_use]
    pub fn lens_id(&self) -> Option<u32> {
        self.ifd.get_u32(LENS_ID)
    }

    /// `LensSpec` (`0xb02a`): the whole `BYTE` array (eight bytes in Sony's bodies).
    #[must_use]
    pub fn lens_spec(&self) -> Option<&[u8]> {
        self.bytes(LENS_SPEC)
    }

    /// `FullImageSize` (`0xb02b`): the whole `LONG` array, unreordered (ExifTool names its two
    /// elements height, then width).
    #[must_use]
    pub fn full_image_size(&self) -> Option<&[u32]> {
        match self.ifd.get(FULL_IMAGE_SIZE)? {
            Value::Long(values) => Some(values),
            _ => None,
        }
    }

    /// `tag` as a single `SLONG`, or `None` if absent, of another type, or not one value.
    fn slong(&self, tag: u16) -> Option<i32> {
        match self.ifd.get(tag)? {
            Value::SLong(values) => single(values),
            _ => None,
        }
    }

    /// `tag` as a `BYTE` array, or `None` if absent or of another type.
    fn bytes(&self, tag: u16) -> Option<&[u8]> {
        match self.ifd.get(tag)? {
            Value::Byte(values) => Some(values),
            _ => None,
        }
    }

    /// `tag` as text up to its first NUL, which is what exiv2 reports; any padding stays in
    /// [`ifd`](Self::ifd). As for Canon, a field holding a non-UTF-8 byte is already in
    /// [`unreadable_tags`](Self::unreadable_tags) and reads as `None`.
    fn text(&self, tag: u16) -> Option<&str> {
        as_text(self.ifd.get(tag)?)?.split('\0').next()
    }
}

/// The one element of `values`, or `None` if it holds any other number of them.
fn single<T: Copy>(values: &[T]) -> Option<T> {
    match values {
        [value] => Some(*value),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use gamut_ifd::ByteOrder::{BigEndian as BE, LittleEndian as LE};
    use gamut_ifd::{ByteOrder, FieldType};

    use super::super::{MakerNote, MakerNoteVendor};
    use super::*;

    /// One IFD entry to lay out: tag, field type, count, and the value's bytes in the note's order.
    struct Entry {
        tag: u16,
        ty: FieldType,
        count: u32,
        bytes: Vec<u8>,
    }

    fn entry(tag: u16, ty: FieldType, count: usize, bytes: Vec<u8>) -> Entry {
        Entry {
            tag,
            ty,
            count: count as u32,
            bytes,
        }
    }

    fn longs(tag: u16, values: &[u32], order: ByteOrder) -> Entry {
        let bytes = values.iter().flat_map(|&v| order.pack_u32(v)).collect();
        entry(tag, FieldType::Long, values.len(), bytes)
    }

    fn slongs(tag: u16, values: &[i32], order: ByteOrder) -> Entry {
        let bytes = values
            .iter()
            .flat_map(|&v| order.pack_u32(v as u32))
            .collect();
        entry(tag, FieldType::SLong, values.len(), bytes)
    }

    fn shorts(tag: u16, values: &[u16], order: ByteOrder) -> Entry {
        let bytes = values.iter().flat_map(|&v| order.pack_u16(v)).collect();
        entry(tag, FieldType::Short, values.len(), bytes)
    }

    fn srationals(tag: u16, values: &[(i32, i32)], order: ByteOrder) -> Entry {
        let bytes = values
            .iter()
            .flat_map(|&(n, d)| [order.pack_u32(n as u32), order.pack_u32(d as u32)])
            .flatten()
            .collect();
        entry(tag, FieldType::SRational, values.len(), bytes)
    }

    fn bytes(tag: u16, values: &[u8]) -> Entry {
        entry(tag, FieldType::Byte, values.len(), values.to_vec())
    }

    fn ascii(tag: u16, text: &[u8]) -> Entry {
        entry(tag, FieldType::Ascii, text.len(), text.to_vec())
    }

    /// A Sony type-2 note: the IFD at byte 0, a zero next-IFD pointer, then each out-of-line
    /// value, addressed as if the note stood at `note_at` in its TIFF stream.
    fn sony_note(entries: &[Entry], order: ByteOrder, note_at: u32) -> Vec<u8> {
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
            vendor: MakerNoteVendor::Sony,
            bytes,
        }
    }

    /// One entry per accessor, each holding a value no other accessor's tag holds, so reading the
    /// wrong tag is always visible.
    fn every_tag(order: ByteOrder) -> Vec<Entry> {
        vec![
            longs(QUALITY, &[2], order),
            srationals(FLASH_EXPOSURE_COMP, &[(-2, 3)], order),
            longs(WHITE_BALANCE, &[16], order),
            longs(RATING, &[4], order),
            slongs(CONTRAST, &[-1], order),
            slongs(SATURATION, &[2], order),
            slongs(SHARPNESS, &[-3], order),
            slongs(BRIGHTNESS, &[5], order),
            ascii(SERIAL_NUMBER, b"04523117\0\0\0\0"),
            bytes(FILE_FORMAT, &[4, 0, 0, 0]),
            shorts(MODEL_ID, &[358], order),
            ascii(CREATIVE_STYLE, b"Standard\0"),
            longs(COLOR_TEMPERATURE, &[5_500], order),
            longs(LENS_ID, &[65_535], order),
            bytes(LENS_SPEC, &[0, 0x24, 0x70, 0x28, 0x28, 0, 0x80, 0]),
            longs(FULL_IMAGE_SIZE, &[4_000, 6_000], order),
        ]
    }

    #[test]
    fn every_accessor_reads_its_own_tag() {
        for order in [LE, BE] {
            let note = maker(sony_note(&every_tag(order), order, 200));
            let view = note.sony(order, 200).expect("reads").expect("is Sony");
            assert_eq!(view.quality(), Some(2));
            assert_eq!(view.flash_exposure_comp(), Some((-2, 3)));
            assert_eq!(view.white_balance(), Some(16));
            assert_eq!(view.rating(), Some(4));
            assert_eq!(view.contrast(), Some(-1));
            assert_eq!(view.saturation(), Some(2));
            assert_eq!(view.sharpness(), Some(-3));
            assert_eq!(view.brightness(), Some(5));
            assert_eq!(view.serial_number(), Some("04523117"));
            assert_eq!(view.file_format(), Some(&[4, 0, 0, 0][..]));
            assert_eq!(view.model_id(), Some(358));
            assert_eq!(view.creative_style(), Some("Standard"));
            assert_eq!(view.color_temperature(), Some(5_500));
            assert_eq!(view.lens_id(), Some(65_535));
            assert_eq!(
                view.lens_spec(),
                Some(&[0, 0x24, 0x70, 0x28, 0x28, 0, 0x80, 0][..])
            );
            assert_eq!(view.full_image_size(), Some(&[4_000, 6_000][..]));
            assert_eq!(view.ifd().fields().len(), 16);
            assert!(view.unreadable_tags().is_empty());
        }
    }

    /// Each typed accessor whose type is fixed refuses the entry under another type, and the raw
    /// value stays reachable through `ifd()`.
    #[test]
    fn a_retyped_tag_reads_as_none() {
        let order = LE;
        let note = maker(sony_note(
            &[
                longs(FLASH_EXPOSURE_COMP, &[1], order),
                longs(CONTRAST, &[1], order),
                slongs(SERIAL_NUMBER, &[1], order),
                shorts(FILE_FORMAT, &[1], order),
                longs(MODEL_ID, &[1], order),
                shorts(FULL_IMAGE_SIZE, &[1, 2], order),
            ],
            order,
            0,
        ));
        let view = note.sony(order, 0).expect("reads").expect("is Sony");
        assert_eq!(view.flash_exposure_comp(), None);
        assert_eq!(view.contrast(), None);
        assert_eq!(view.serial_number(), None);
        assert_eq!(view.file_format(), None);
        assert_eq!(view.model_id(), None);
        assert_eq!(view.full_image_size(), None);
        assert_eq!(view.ifd().get(MODEL_ID), Some(&Value::Long(vec![1])));
    }

    #[test]
    fn an_absent_tag_reads_as_none() {
        let order = BE;
        let note = maker(sony_note(&[longs(QUALITY, &[1], order)], order, 0));
        let view = note.sony(order, 0).expect("reads").expect("is Sony");
        assert_eq!(view.flash_exposure_comp(), None);
        assert_eq!(view.contrast(), None);
        assert_eq!(view.lens_spec(), None);
        assert_eq!(view.creative_style(), None);
        assert_eq!(view.model_id(), None);
        assert_eq!(view.full_image_size(), None);
    }

    /// A scalar accessor of a fixed type takes exactly one value; exiv2 would print them all.
    #[test]
    fn a_scalar_with_more_than_one_value_reads_as_none() {
        let order = LE;
        let note = maker(sony_note(
            &[
                srationals(FLASH_EXPOSURE_COMP, &[(1, 3), (2, 3)], order),
                slongs(BRIGHTNESS, &[1, 2], order),
                shorts(MODEL_ID, &[358, 359], order),
            ],
            order,
            0,
        ));
        let view = note.sony(order, 0).expect("reads").expect("is Sony");
        assert_eq!(view.flash_exposure_comp(), None);
        assert_eq!(view.brightness(), None);
        assert_eq!(view.model_id(), None);
    }

    #[test]
    fn single_takes_exactly_one_value() {
        assert_eq!(single::<u8>(&[]), None);
        assert_eq!(single(&[7]), Some(7));
        assert_eq!(single(&[7, 8]), None);
    }

    #[test]
    fn text_keeps_the_padding_in_the_raw_value() {
        let order = BE;
        let note = maker(sony_note(
            &[ascii(CREATIVE_STYLE, b"Vivid\0\0\0")],
            order,
            0,
        ));
        let view = note.sony(order, 0).expect("reads").expect("is Sony");
        assert_eq!(view.creative_style(), Some("Vivid"));
        assert_eq!(
            view.ifd().get(CREATIVE_STYLE),
            Some(&Value::Ascii("Vivid\0\0".into()))
        );
    }

    #[test]
    fn offsets_count_from_the_tiff_header() {
        let order = LE;
        let bytes = sony_note(&every_tag(order), order, 200);
        // Read as if the note stood at the stream's start, every out-of-line value points past it.
        let view = maker(bytes.clone())
            .sony(order, 0)
            .expect("reads")
            .expect("is Sony");
        assert_eq!(view.creative_style(), None);
        assert_eq!(view.unreadable_tags().first(), Some(&FLASH_EXPOSURE_COMP));
        // An inline value has no offset and reads wherever the note stands.
        assert_eq!(view.quality(), Some(2));
        // The whole stream, with the note at 200, reads the same as the note alone.
        let mut stream = vec![0; 200];
        stream.extend_from_slice(&bytes);
        let layout = maker(bytes.clone()).layout(order).expect("Sony layout");
        let read = SonyMakerNote::read(&stream[..], 200, layout).expect("reads");
        assert_eq!(read, maker(bytes).sony(order, 200).expect("reads"));
    }

    #[test]
    fn a_note_without_its_next_ifd_pointer_is_an_error() {
        let order = LE;
        let mut bytes = sony_note(&[longs(QUALITY, &[1], order)], order, 0);
        bytes.truncate(14);
        assert!(maker(bytes).sony(order, 0).is_err());
    }

    /// `Sony1` resolves a layout, but its pointer-less IFD is not read yet (issue #1033).
    #[test]
    fn a_sony_type_1_note_has_no_view_yet() {
        let order = LE;
        let mut bytes = b"SONY DSC \0\0\0".to_vec();
        bytes.extend_from_slice(&sony_note(&[longs(QUALITY, &[1], order)], order, 0));
        let note = maker(bytes);
        let layout = note.layout(order).expect("a layout");
        assert_eq!(layout.dialect, MakerNoteDialect::Sony1);
        assert_eq!(note.sony(order, 0).expect("not an error"), None);
    }

    #[test]
    fn only_a_sony_type_2_layout_is_read() {
        let order = LE;
        let bytes = sony_note(&[longs(QUALITY, &[1], order)], order, 0);
        let canon = MakerNote {
            vendor: MakerNoteVendor::Canon,
            bytes: bytes.clone(),
        };
        assert_eq!(canon.sony(order, 0).expect("not an error"), None);
        let unknown = MakerNote {
            vendor: MakerNoteVendor::Unknown,
            bytes,
        };
        assert_eq!(unknown.sony(order, 0).expect("not an error"), None);
    }
}
