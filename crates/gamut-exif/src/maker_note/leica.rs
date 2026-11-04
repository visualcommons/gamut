//! A read-only view of a Leica `MakerNote` (issue #978).
//!
//! Leica has written several note layouts, which [`MakerNoteLayout::detect`] tells apart by their
//! leading bytes ([`MakerNoteDialect::Leica3`] … [`MakerNoteDialect::Leica10`], after ExifTool's
//! `MakerNoteLeica*` entries). The view decodes the note's IFD through [`gamut_ifd`] at that layout
//! for the five IFD dialects `Leica3`, `Leica4`, `Leica5`, `Leica8` and `Leica9`, keeps every entry
//! as a [`Value`] ([`LeicaMakerNote::ifd`]), and adds typed accessors for the two tag tables the
//! sample corpus holds, after ExifTool's `Panasonic.pm`, as staked in
//! `references/exif/leica/README.md`:
//!
//! - **`Leica9`** (M10, S) — [`LeicaMakerNote::leica9`], [`Leica9Tags`].
//! - **`Leica4`** (M9, M Monochrom) — [`LeicaMakerNote::leica4`], [`Leica4Tags`]. Its four
//!   entries `0x3000`, `0x3100`, `0x3400` and `0x3900` each hold an IFD of their own whose offsets
//!   count from the note's base; the view reads them too ([`LeicaMakerNote::subdirectory`]).
//!
//! `Leica10` (D-Lux 7) is a Panasonic-format note and a `Fujifilm`-signed Leica note follows
//! Fujifilm's table, so neither is read here. Values are returned raw, as stored: ExifTool's value
//! conversions (`LensType`'s split, `FNumber / 1000`, `MeasuredLV / 1e5`, …) are not applied.
//!
//! The view never writes. The block it was read from re-emits byte-identically, because nothing
//! here changes it; see the [module docs](super) on how the writer keeps file-absolute offsets
//! valid.

use gamut_core::ErrorKind;
use gamut_ifd::{Ifd, IfdReader, RawEntry, ReadAt, Value};

use super::{
    MakerNoteDialect, MakerNoteLayout, decode_lenient, open_note_ifd, single, slong, text,
};
use crate::error::Result;
use crate::value::{Rational, SRational};

/// The `Leica4` entries that each hold a subdirectory, in the order the view stores them.
const SUBDIRECTORIES: [u16; 4] = [0x3000, 0x3100, 0x3400, 0x3900];

/// The decoded IFD of a Leica `MakerNote`: every entry, the `Leica4` subdirectories, and typed
/// accessors for the tags the [module docs](self) name.
///
/// Built by [`MakerNote::leica`](super::MakerNote::leica) from the note's own bytes, or by
/// [`read`](Self::read) from the whole TIFF stream.
#[derive(Debug, Clone, PartialEq)]
pub struct LeicaMakerNote {
    dialect: MakerNoteDialect,
    ifd: Ifd,
    /// One slot per [`SUBDIRECTORIES`] tag, filled only for a `Leica4` note.
    subdirectories: [Option<Ifd>; 4],
    unreadable: Vec<u16>,
}

impl LeicaMakerNote {
    /// Decodes the Leica note laid out as `layout` that starts at `note_at` in `stream` — the TIFF
    /// stream the note sits in (the bytes after any `Exif\0\0` marker), or any [`ReadAt`] in that
    /// stream's offset space.
    ///
    /// `Ok(None)` unless `layout` is one of the IFD dialects [`MakerNoteDialect::Leica3`],
    /// [`Leica4`](MakerNoteDialect::Leica4), [`Leica5`](MakerNoteDialect::Leica5),
    /// [`Leica8`](MakerNoteDialect::Leica8) or [`Leica9`](MakerNoteDialect::Leica9). An entry
    /// whose value cannot be read is left out of [`ifd`](Self::ifd) (or its subdirectory) and
    /// named by [`unreadable_tags`](Self::unreadable_tags), and so is a `Leica4` subdirectory
    /// entry whose IFD cannot be read.
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
        use MakerNoteDialect as D;
        let dialect = layout.dialect;
        if !matches!(
            dialect,
            D::Leica3 | D::Leica4 | D::Leica5 | D::Leica8 | D::Leica9
        ) {
            return Ok(None);
        }
        let (mut reader, raw) = open_note_ifd(stream, note_at, layout)?;
        let (ifd, mut unreadable) = decode_lenient(&mut reader, &raw)?;
        let mut subdirectories = [None, None, None, None];
        if dialect == D::Leica4 {
            for entry in &raw.entries {
                let Some(slot) = SUBDIRECTORIES.iter().position(|&tag| tag == entry.tag) else {
                    continue;
                };
                match read_subdirectory(&mut reader, entry)? {
                    Some((sub, inner)) => {
                        subdirectories[slot] = Some(sub);
                        unreadable.extend(inner);
                    }
                    None => unreadable.push(entry.tag),
                }
            }
        }
        Ok(Some(Self {
            dialect,
            ifd,
            subdirectories,
            unreadable,
        }))
    }

    /// The dialect the note was read as.
    #[must_use]
    pub fn dialect(&self) -> MakerNoteDialect {
        self.dialect
    }

    /// Every entry of the note's IFD that could be read, by tag. A `Leica4` subdirectory entry is
    /// here as its raw `UNDEFINED` bytes; its decoded IFD is [`subdirectory`](Self::subdirectory).
    #[must_use]
    pub fn ifd(&self) -> &Ifd {
        &self.ifd
    }

    /// The IFD a `Leica4` note's entry `tag` (`0x3000`, `0x3100`, `0x3400` or `0x3900`) holds.
    ///
    /// `None` for any other tag, for a note of another dialect, and for a subdirectory that is
    /// absent or could not be read (then named by [`unreadable_tags`](Self::unreadable_tags)).
    /// Where the note repeats a subdirectory tag, the last readable one is kept.
    #[must_use]
    pub fn subdirectory(&self, tag: u16) -> Option<&Ifd> {
        let slot = SUBDIRECTORIES.iter().position(|&known| known == tag)?;
        self.subdirectories[slot].as_ref()
    }

    /// The tags whose values could not be read (an offset past the source, or a malformed value):
    /// the note's own entries first, in on-disk order; then, for each `Leica4` subdirectory entry
    /// in the order the note lists them, either its IFD's unreadable entries or, when that IFD
    /// itself could not be read, the subdirectory's own tag. Empty for a well-formed note.
    #[must_use]
    pub fn unreadable_tags(&self) -> &[u16] {
        &self.unreadable
    }

    /// The `Leica4` tags (M9, M Monochrom), or `None` for a note of another dialect.
    #[must_use]
    pub fn leica4(&self) -> Option<Leica4Tags<'_>> {
        (self.dialect == MakerNoteDialect::Leica4).then_some(Leica4Tags { note: self })
    }

    /// The `Leica9` tags (M10, S), or `None` for a note of another dialect.
    #[must_use]
    pub fn leica9(&self) -> Option<Leica9Tags<'_>> {
        (self.dialect == MakerNoteDialect::Leica9).then_some(Leica9Tags { ifd: &self.ifd })
    }
}

/// Reads the subdirectory `entry` points at, in `reader`'s offset space (the note's base), with
/// the tags of its unreadable entries. `Ok(None)` when it cannot be read: a value small enough to
/// sit inline, which holds no IFD, or a directory the bytes do not hold.
///
/// # Errors
///
/// [`ExifError::Ifd`](crate::ExifError::Ifd) only when the source fails (an I/O-kind error).
fn read_subdirectory<S: ReadAt>(
    reader: &mut IfdReader<S>,
    entry: &RawEntry,
) -> Result<Option<(Ifd, Vec<u16>)>> {
    let Some(at) = reader.value_offset(entry) else {
        return Ok(None);
    };
    let raw = match reader.read_ifd(at) {
        Ok(raw) => raw,
        Err(error) if error.kind() == ErrorKind::InvalidInput => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    decode_lenient(reader, &raw).map(Some)
}

/// The `Leica9` tags of a [`LeicaMakerNote`] (ExifTool's `Panasonic::Leica9` table), raw as
/// stored. Every accessor returns `None` for an absent tag or one stored in another type.
#[derive(Debug, Clone, Copy)]
pub struct Leica9Tags<'a> {
    ifd: &'a Ifd,
}

impl Leica9Tags<'_> {
    /// `FocusDistance` (`0x0304`, `LONG`).
    #[must_use]
    pub fn focus_distance(&self) -> Option<u32> {
        self.ifd.get_u32(0x0304)
    }

    /// `ExternalSensorBrightnessValue` (`0x0311`): signed, whether stored as `SRATIONAL` or as
    /// `RATIONAL` (ExifTool: "may be incorrectly unsigned in JPEG images"), bit for bit.
    #[must_use]
    pub fn external_sensor_brightness_value(&self) -> Option<SRational> {
        signed_rational(self.ifd, 0x0311)
    }

    /// `MeasuredLV` (`0x0312`): signed, as for
    /// [`external_sensor_brightness_value`](Self::external_sensor_brightness_value).
    #[must_use]
    pub fn measured_lv(&self) -> Option<SRational> {
        signed_rational(self.ifd, 0x0312)
    }

    /// `UserProfile` (`0x034c`), the text before its first NUL.
    #[must_use]
    pub fn user_profile(&self) -> Option<&str> {
        text(self.ifd, 0x034c)
    }

    /// `ISOSelected` (`0x0359`, `SLONG`).
    #[must_use]
    pub fn iso_selected(&self) -> Option<i32> {
        slong(self.ifd, 0x0359)
    }

    /// `FNumber` (`0x035a`, `SLONG`): the f-number times 1000 (`5600` is f/5.6).
    #[must_use]
    pub fn f_number(&self) -> Option<i32> {
        slong(self.ifd, 0x035a)
    }

    /// `CorrelatedColorTemp` (`0x035b`, `SHORT`), in kelvin.
    #[must_use]
    pub fn correlated_color_temp(&self) -> Option<u16> {
        match self.ifd.get(0x035b)? {
            Value::Short(values) => single(values),
            _ => None,
        }
    }

    /// `ColorTint` (`0x035c`, `SSHORT`).
    #[must_use]
    pub fn color_tint(&self) -> Option<i16> {
        match self.ifd.get(0x035c)? {
            Value::SShort(values) => single(values),
            _ => None,
        }
    }

    /// `WhitePoint` (`0x035d`, `RATIONAL`): the chromaticity `x`, `y` pairs, all of them as stored.
    #[must_use]
    pub fn white_point(&self) -> Option<&[(u32, u32)]> {
        self.ifd.get(0x035d)?.as_rationals()
    }

    /// `LensProfileName` (`0x0370`), the text before its first NUL.
    #[must_use]
    pub fn lens_profile_name(&self) -> Option<&str> {
        text(self.ifd, 0x0370)
    }
}

/// The `Leica4` tags of a [`LeicaMakerNote`] (ExifTool's `Panasonic::Subdir` table, read in the
/// subdirectory each tag lives in), raw as stored. Every accessor returns `None` for an absent
/// tag, one stored in another type, or a subdirectory that is absent or unreadable.
#[derive(Debug, Clone, Copy)]
pub struct Leica4Tags<'a> {
    note: &'a LeicaMakerNote,
}

impl<'a> Leica4Tags<'a> {
    /// The subdirectory `0x3000` (image settings).
    fn settings(&self) -> Option<&'a Ifd> {
        self.note.subdirectory(0x3000)
    }

    /// The subdirectory `0x3100` (camera).
    fn camera(&self) -> Option<&'a Ifd> {
        self.note.subdirectory(0x3100)
    }

    /// The subdirectory `0x3400` (exposure).
    fn exposure(&self) -> Option<&'a Ifd> {
        self.note.subdirectory(0x3400)
    }

    /// `Contrast` (`0x300a` in `0x3000`, `LONG`).
    #[must_use]
    pub fn contrast(&self) -> Option<u32> {
        self.settings()?.get_u32(0x300a)
    }

    /// `Sharpening` (`0x300b` in `0x3000`, `LONG`).
    #[must_use]
    pub fn sharpening(&self) -> Option<u32> {
        self.settings()?.get_u32(0x300b)
    }

    /// `Saturation` (`0x300d` in `0x3000`, `LONG`).
    #[must_use]
    pub fn saturation(&self) -> Option<u32> {
        self.settings()?.get_u32(0x300d)
    }

    /// `WhiteBalance` (`0x3033` in `0x3000`, `LONG`).
    #[must_use]
    pub fn white_balance(&self) -> Option<u32> {
        self.settings()?.get_u32(0x3033)
    }

    /// `JPEGQuality` (`0x3034` in `0x3000`, `LONG`).
    #[must_use]
    pub fn jpeg_quality(&self) -> Option<u32> {
        self.settings()?.get_u32(0x3034)
    }

    /// `WB_RGBLevels` (`0x3036` in `0x3000`, `RATIONAL`): the red, green and blue levels, all of
    /// them as stored.
    #[must_use]
    pub fn wb_rgb_levels(&self) -> Option<&'a [(u32, u32)]> {
        self.settings()?.get(0x3036)?.as_rationals()
    }

    /// `UserProfile` (`0x3038` in `0x3000`), the text before its first NUL.
    #[must_use]
    pub fn user_profile(&self) -> Option<&'a str> {
        text(self.settings()?, 0x3038)
    }

    /// `JPEGSize` (`0x303a` in `0x3000`, `LONG`).
    #[must_use]
    pub fn jpeg_size(&self) -> Option<u32> {
        self.settings()?.get_u32(0x303a)
    }

    /// `SerialNumber` (`0x3103` in `0x3100`), the text before its first NUL.
    #[must_use]
    pub fn serial_number(&self) -> Option<&'a str> {
        text(self.camera()?, 0x3103)
    }

    /// `FirmwareVersion` (`0x3109` in `0x3100`), the text before its first NUL.
    #[must_use]
    pub fn firmware_version(&self) -> Option<&'a str> {
        text(self.camera()?, 0x3109)
    }

    /// `BaseISO` (`0x312a` in `0x3100`, `LONG`).
    #[must_use]
    pub fn base_iso(&self) -> Option<u32> {
        self.camera()?.get_u32(0x312a)
    }

    /// `SensorWidth` (`0x312b` in `0x3100`, `LONG`).
    #[must_use]
    pub fn sensor_width(&self) -> Option<u32> {
        self.camera()?.get_u32(0x312b)
    }

    /// `SensorHeight` (`0x312c` in `0x3100`, `LONG`).
    #[must_use]
    pub fn sensor_height(&self) -> Option<u32> {
        self.camera()?.get_u32(0x312c)
    }

    /// `SensorBitDepth` (`0x312d` in `0x3100`, `LONG`).
    #[must_use]
    pub fn sensor_bit_depth(&self) -> Option<u32> {
        self.camera()?.get_u32(0x312d)
    }

    /// `CameraTemperature` (`0x3402` in `0x3400`, `SLONG`).
    #[must_use]
    pub fn camera_temperature(&self) -> Option<i32> {
        slong(self.exposure()?, 0x3402)
    }

    /// `LensType` (`0x3405` in `0x3400`, `LONG`): the raw word, whose bits above the low two are
    /// the lens ID and whose low two ExifTool reads as a frame-selector code.
    #[must_use]
    pub fn lens_type(&self) -> Option<u32> {
        self.exposure()?.get_u32(0x3405)
    }

    /// `ApproximateFNumber` (`0x3406` in `0x3400`, `RATIONAL`).
    #[must_use]
    pub fn approximate_f_number(&self) -> Option<Rational> {
        match self.exposure()?.get(0x3406)?.as_rationals()? {
            [pair] => Some(Rational::from(*pair)),
            _ => None,
        }
    }

    /// `MeasuredLV` (`0x3407` in `0x3400`, `SLONG`): the light value times 100 000.
    #[must_use]
    pub fn measured_lv(&self) -> Option<i32> {
        slong(self.exposure()?, 0x3407)
    }

    /// `ExternalSensorBrightnessValue` (`0x3408` in `0x3400`, `SLONG`): the value times 100 000.
    #[must_use]
    pub fn external_sensor_brightness_value(&self) -> Option<i32> {
        slong(self.exposure()?, 0x3408)
    }
}

/// `tag` as a single signed rational, an unsigned `RATIONAL` reinterpreted bit for bit.
fn signed_rational(ifd: &Ifd, tag: u16) -> Option<SRational> {
    match ifd.get(tag)? {
        Value::SRational(pairs) => single(pairs).map(SRational::from),
        Value::Rational(pairs) => {
            let (num, den) = single(pairs)?;
            Some(SRational::from((num.cast_signed(), den.cast_signed())))
        }
        _ => None,
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

    fn entry(tag: u16, ty: FieldType, count: u32, bytes: Vec<u8>) -> Entry {
        Entry {
            tag,
            ty,
            count,
            bytes,
        }
    }

    fn long(tag: u16, value: u32, order: ByteOrder) -> Entry {
        entry(tag, FieldType::Long, 1, order.pack_u32(value).to_vec())
    }

    fn slong(tag: u16, value: i32, order: ByteOrder) -> Entry {
        let bytes = order.pack_u32(value.cast_unsigned()).to_vec();
        entry(tag, FieldType::SLong, 1, bytes)
    }

    fn rationals(tag: u16, ty: FieldType, pairs: &[(u32, u32)], order: ByteOrder) -> Entry {
        let bytes = pairs
            .iter()
            .flat_map(|&(n, d)| [order.pack_u32(n), order.pack_u32(d)])
            .flatten()
            .collect();
        entry(tag, ty, pairs.len() as u32, bytes)
    }

    fn ascii(tag: u16, text: &[u8]) -> Entry {
        entry(tag, FieldType::Ascii, text.len() as u32, text.to_vec())
    }

    /// Lays `entries` out as an IFD at `at` within `buf` (growing it), with a zero next-IFD
    /// pointer and every out-of-line value appended after it, each addressed as `base + position`.
    fn lay_out(buf: &mut Vec<u8>, entries: &[Entry], order: ByteOrder, base: u32) -> u32 {
        let at = buf.len() as u32;
        let count = entries.len() as u32;
        let mut data_at = at + 2 + 12 * count + 4;
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
                ifd.extend_from_slice(&order.pack_u32(base + data_at));
                data.extend_from_slice(&entry.bytes);
                data_at += entry.bytes.len() as u32;
            }
        }
        ifd.extend_from_slice(&[0; 4]);
        buf.extend_from_slice(&ifd);
        buf.extend_from_slice(&data);
        at
    }

    /// A `header`-signed note whose IFD at byte `header.len()` holds `entries`, its values
    /// addressed from `base` (the note's own start is `0`).
    fn note_with(header: &[u8], entries: &[Entry], order: ByteOrder, base: u32) -> Vec<u8> {
        let mut bytes = header.to_vec();
        lay_out(&mut bytes, entries, order, base);
        bytes
    }

    fn maker(bytes: Vec<u8>) -> MakerNote {
        MakerNote {
            vendor: MakerNoteVendor::Leica,
            bytes,
        }
    }

    fn leica9_entries(order: ByteOrder) -> Vec<Entry> {
        vec![
            long(0x0304, 16_279, order),
            rationals(
                0x0311,
                FieldType::SRational,
                &[((-3_i32).cast_unsigned(), 2)],
                order,
            ),
            rationals(0x0312, FieldType::Rational, &[(0xffff_fff9, 4)], order),
            ascii(0x034c, b"Studio\0"),
            slong(0x0359, 200, order),
            slong(0x035a, 5_600, order),
            entry(0x035b, FieldType::Short, 1, order.pack_u16(5_200).to_vec()),
            entry(
                0x035c,
                FieldType::SShort,
                1,
                order.pack_u16((-6_i16).cast_unsigned()).to_vec(),
            ),
            rationals(
                0x035d,
                FieldType::Rational,
                &[(3_399, 10_000), (3_509, 10_000)],
                order,
            ),
            ascii(0x0370, b"Summilux 35\0\0\0"),
        ]
    }

    #[test]
    fn every_leica9_accessor_reads_its_own_tag() {
        for order in [LE, BE] {
            // `Leica9` offsets count from the TIFF header: the note stands at 300.
            let note = maker(note_with(
                b"LEICA\0\x02\0",
                &leica9_entries(order),
                order,
                300,
            ));
            let view = note.leica(order, 300).expect("reads").expect("Leica");
            assert_eq!(view.dialect(), MakerNoteDialect::Leica9);
            assert!(view.leica4().is_none());
            let tags = view.leica9().expect("Leica9");
            assert_eq!(tags.focus_distance(), Some(16_279));
            assert_eq!(
                tags.external_sensor_brightness_value(),
                Some(SRational { num: -3, den: 2 })
            );
            assert_eq!(tags.measured_lv(), Some(SRational { num: -7, den: 4 }));
            assert_eq!(tags.user_profile(), Some("Studio"));
            assert_eq!(tags.iso_selected(), Some(200));
            assert_eq!(tags.f_number(), Some(5_600));
            assert_eq!(tags.correlated_color_temp(), Some(5_200));
            assert_eq!(tags.color_tint(), Some(-6));
            assert_eq!(
                tags.white_point(),
                Some(&[(3_399, 10_000), (3_509, 10_000)][..])
            );
            assert_eq!(tags.lens_profile_name(), Some("Summilux 35"));
            assert_eq!(view.ifd().fields().len(), 10);
            assert!(view.unreadable_tags().is_empty());
        }
    }

    #[test]
    fn leica9_accessors_refuse_another_type_or_count() {
        let order = LE;
        let entries = [
            long(0x0311, 1, order),
            rationals(0x0312, FieldType::SRational, &[(1, 2), (3, 4)], order),
            long(0x0359, 100, order),
            entry(0x035a, FieldType::SLong, 2, vec![0; 8]),
            long(0x035b, 5_200, order),
            entry(0x035c, FieldType::Short, 1, order.pack_u16(6).to_vec()),
            long(0x035d, 1, order),
        ];
        let note = maker(note_with(b"LEICA\0\x02\0", &entries, order, 0));
        let view = note.leica(order, 0).expect("reads").expect("Leica");
        let tags = view.leica9().expect("Leica9");
        assert_eq!(tags.external_sensor_brightness_value(), None);
        assert_eq!(tags.measured_lv(), None);
        assert_eq!(tags.iso_selected(), None);
        assert_eq!(tags.f_number(), None);
        assert_eq!(tags.correlated_color_temp(), None);
        assert_eq!(tags.color_tint(), None);
        assert_eq!(tags.white_point(), None);
        assert_eq!(tags.focus_distance(), None);
        assert_eq!(tags.user_profile(), None);
    }

    #[test]
    fn a_multi_valued_unsigned_rational_is_no_signed_rational() {
        let order = BE;
        let entries = [rationals(
            0x0311,
            FieldType::Rational,
            &[(1, 2), (3, 4)],
            order,
        )];
        let note = maker(note_with(b"LEICA\0\x02\0", &entries, order, 0));
        let view = note.leica(order, 0).expect("reads").expect("Leica");
        assert_eq!(
            view.leica9()
                .expect("Leica9")
                .external_sensor_brightness_value(),
            None
        );
    }

    /// A `Leica4` note: the four subdirectory entries, each an `UNDEFINED` value holding an IFD,
    /// every offset counting from the note's start.
    fn leica4_note(order: ByteOrder, sub: [&[Entry]; 4]) -> Vec<u8> {
        // Lay the subdirectories out first, after a placeholder header and main IFD, then point
        // the main IFD's entries at them.
        let main_len = 8 + 2 + 12 * 4 + 4;
        let mut bytes = vec![0; main_len];
        bytes[..8].copy_from_slice(b"LEICA0\x03\0");
        let mut spans = Vec::new();
        for entries in sub {
            let at = lay_out(&mut bytes, entries, order, 0);
            spans.push((at, bytes.len() as u32 - at));
        }
        let mut main = order.pack_u16(4).to_vec();
        for (tag, (at, len)) in SUBDIRECTORIES.into_iter().zip(spans) {
            main.extend_from_slice(&order.pack_u16(tag));
            main.extend_from_slice(&order.pack_u16(FieldType::Undefined.code()));
            main.extend_from_slice(&order.pack_u32(len));
            main.extend_from_slice(&order.pack_u32(at));
        }
        main.extend_from_slice(&[0; 4]);
        bytes[8..main_len].copy_from_slice(&main);
        bytes
    }

    fn settings(order: ByteOrder) -> Vec<Entry> {
        vec![
            long(0x300a, 1, order),
            long(0x300b, 2, order),
            long(0x300d, 3, order),
            long(0x3033, 4, order),
            long(0x3034, 97, order),
            rationals(
                0x3036,
                FieldType::Rational,
                &[(1, 2), (3, 4), (5, 6)],
                order,
            ),
            ascii(0x3038, b"Night\0\0"),
            long(0x303a, 5, order),
        ]
    }

    fn camera(order: ByteOrder) -> Vec<Entry> {
        vec![
            ascii(0x3103, b"4348886\0"),
            ascii(0x3109, b"1.008\0"),
            long(0x312a, 320, order),
            long(0x312b, 5_270, order),
            long(0x312c, 3_516, order),
            long(0x312d, 14, order),
        ]
    }

    fn exposure(order: ByteOrder) -> Vec<Entry> {
        vec![
            slong(0x3402, -11, order),
            long(0x3405, 135, order),
            rationals(0x3406, FieldType::Rational, &[(425_984, 65_536)], order),
            slong(0x3407, -264_448, order),
            slong(0x3408, 378_880, order),
        ]
    }

    #[test]
    fn every_leica4_accessor_reads_its_own_tag_in_its_own_subdirectory() {
        for order in [LE, BE] {
            let data = [long(0x3901, 9, order)];
            let bytes = leica4_note(
                order,
                [&settings(order), &camera(order), &exposure(order), &data],
            );
            // Note-relative offsets: the note reads the same wherever it stands.
            for note_at in [0, 1_152] {
                let view = maker(bytes.clone())
                    .leica(order, note_at)
                    .expect("reads")
                    .expect("Leica");
                assert_eq!(view.dialect(), MakerNoteDialect::Leica4);
                assert!(view.leica9().is_none());
                let tags = view.leica4().expect("Leica4");
                assert_eq!(tags.contrast(), Some(1));
                assert_eq!(tags.sharpening(), Some(2));
                assert_eq!(tags.saturation(), Some(3));
                assert_eq!(tags.white_balance(), Some(4));
                assert_eq!(tags.jpeg_quality(), Some(97));
                assert_eq!(tags.wb_rgb_levels(), Some(&[(1, 2), (3, 4), (5, 6)][..]));
                assert_eq!(tags.user_profile(), Some("Night"));
                assert_eq!(tags.jpeg_size(), Some(5));
                assert_eq!(tags.serial_number(), Some("4348886"));
                assert_eq!(tags.firmware_version(), Some("1.008"));
                assert_eq!(tags.base_iso(), Some(320));
                assert_eq!(tags.sensor_width(), Some(5_270));
                assert_eq!(tags.sensor_height(), Some(3_516));
                assert_eq!(tags.sensor_bit_depth(), Some(14));
                assert_eq!(tags.camera_temperature(), Some(-11));
                assert_eq!(tags.lens_type(), Some(135));
                assert_eq!(
                    tags.approximate_f_number(),
                    Some(Rational {
                        num: 425_984,
                        den: 65_536
                    })
                );
                assert_eq!(tags.measured_lv(), Some(-264_448));
                assert_eq!(tags.external_sensor_brightness_value(), Some(378_880));
                assert_eq!(
                    view.subdirectory(0x3900).and_then(|d| d.get_u32(0x3901)),
                    Some(9)
                );
                assert!(view.unreadable_tags().is_empty());
            }
        }
    }

    #[test]
    fn a_leica4_tag_is_read_only_in_its_own_subdirectory() {
        let order = LE;
        // Every subdirectory holds every tag, each with a value naming the subdirectory it is in.
        let all = |marker: u32| -> Vec<Entry> {
            let mut tags: Vec<u16> = [settings(order), camera(order), exposure(order)]
                .iter()
                .flatten()
                .map(|entry| entry.tag)
                .collect();
            tags.sort_unstable();
            tags.into_iter()
                .map(|tag| long(tag, marker, order))
                .collect()
        };
        let bytes = leica4_note(order, [&all(0), &all(1), &all(2), &all(3)]);
        let view = maker(bytes).leica(order, 0).expect("reads").expect("Leica");
        let tags = view.leica4().expect("Leica4");
        for value in [
            tags.contrast(),
            tags.sharpening(),
            tags.saturation(),
            tags.white_balance(),
            tags.jpeg_quality(),
            tags.jpeg_size(),
        ] {
            assert_eq!(value, Some(0));
        }
        for value in [
            tags.base_iso(),
            tags.sensor_width(),
            tags.sensor_height(),
            tags.sensor_bit_depth(),
        ] {
            assert_eq!(value, Some(1));
        }
        assert_eq!(tags.lens_type(), Some(2));
    }

    #[test]
    fn leica4_accessors_refuse_another_type_or_count() {
        let order = BE;
        let settings = [
            long(0x3036, 1, order),
            long(0x3038, 1, order),
            entry(0x303a, FieldType::SLong, 1, order.pack_u32(1).to_vec()),
        ];
        let exposure = [
            long(0x3402, 1, order),
            rationals(0x3406, FieldType::Rational, &[(1, 2), (3, 4)], order),
            long(0x3407, 1, order),
            entry(0x3408, FieldType::SLong, 2, vec![0; 8]),
        ];
        let bytes = leica4_note(order, [&settings, &[], &exposure, &[]]);
        let view = maker(bytes).leica(order, 0).expect("reads").expect("Leica");
        let tags = view.leica4().expect("Leica4");
        assert_eq!(tags.wb_rgb_levels(), None);
        assert_eq!(tags.user_profile(), None);
        assert_eq!(tags.jpeg_size(), None);
        assert_eq!(tags.camera_temperature(), None);
        assert_eq!(tags.approximate_f_number(), None);
        assert_eq!(tags.measured_lv(), None);
        assert_eq!(tags.external_sensor_brightness_value(), None);
        assert_eq!(tags.serial_number(), None);
    }

    #[test]
    fn an_unreadable_subdirectory_is_named_and_the_rest_still_read() {
        let order = LE;
        let mut bytes = leica4_note(order, [&settings(order), &camera(order), &[], &[]]);
        // Point `0x3100` past the note, and make `0x3400` an inline value, which holds no IFD.
        let entry_at = |index: usize| 8 + 2 + 12 * index;
        let (camera_at, exposure_at) = (entry_at(1), entry_at(2));
        bytes[camera_at + 8..camera_at + 12].copy_from_slice(&order.pack_u32(0x00ff_ffff));
        bytes[exposure_at + 4..exposure_at + 8].copy_from_slice(&order.pack_u32(4));
        let view = maker(bytes).leica(order, 0).expect("reads").expect("Leica");
        assert_eq!(view.subdirectory(0x3100), None);
        assert_eq!(view.subdirectory(0x3400), None);
        assert!(view.subdirectory(0x3900).is_some());
        // `0x3100`'s value is unreadable too, so it is named twice: once as an entry, once as a
        // subdirectory.
        assert_eq!(view.unreadable_tags(), &[0x3100, 0x3100, 0x3400]);
        assert_eq!(view.leica4().expect("Leica4").contrast(), Some(1));
        assert!(view.ifd().get(0x3400).is_some());
    }

    #[test]
    fn a_subdirectory_s_unreadable_entries_are_named() {
        let order = LE;
        let settings = [
            ascii(0x3038, b"\xff\xfeNot UTF-8\0"),
            long(0x3034, 97, order),
        ];
        let bytes = leica4_note(order, [&settings, &[], &[], &[]]);
        let view = maker(bytes).leica(order, 0).expect("reads").expect("Leica");
        assert_eq!(view.unreadable_tags(), &[0x3038]);
        let tags = view.leica4().expect("Leica4");
        assert_eq!(tags.user_profile(), None);
        assert_eq!(tags.jpeg_quality(), Some(97));
    }

    #[test]
    fn only_the_four_subdirectory_tags_name_a_subdirectory() {
        let order = LE;
        let bytes = leica4_note(order, [&[], &[], &[], &[]]);
        let view = maker(bytes).leica(order, 0).expect("reads").expect("Leica");
        for tag in SUBDIRECTORIES {
            assert!(view.subdirectory(tag).is_some(), "{tag:#06x}");
        }
        assert_eq!(view.subdirectory(0x3200), None);
    }

    #[test]
    fn a_repeated_subdirectory_tag_keeps_the_last_readable_one() {
        let order = LE;
        let mut bytes = leica4_note(
            order,
            [&settings(order), &[long(0x300a, 7, order)], &[], &[]],
        );
        // Retag the `0x3100` entry as a second `0x3000`.
        let second = 8 + 2 + 12;
        bytes[second..second + 2].copy_from_slice(&order.pack_u16(0x3000));
        let view = maker(bytes).leica(order, 0).expect("reads").expect("Leica");
        assert_eq!(view.leica4().expect("Leica4").contrast(), Some(7));
    }

    /// Only a `Leica4` note's subdirectory entries are followed: the same entries in a `Leica5`
    /// note stay raw values.
    #[test]
    fn other_dialects_read_no_subdirectory() {
        let order = LE;
        let mut bytes = leica4_note(order, [&settings(order), &[], &[], &[]]);
        bytes[..8].copy_from_slice(b"LEICA\0\x01\0");
        let view = maker(bytes).leica(order, 0).expect("reads").expect("Leica");
        assert_eq!(view.dialect(), MakerNoteDialect::Leica5);
        assert_eq!(view.subdirectory(0x3000), None);
        assert!(view.leica4().is_none());
        assert!(view.leica9().is_none());
        assert!(view.unreadable_tags().is_empty());
        assert_eq!(view.ifd().fields().len(), 4);
    }

    #[test]
    fn each_ifd_dialect_is_read_and_no_other() {
        let order = LE;
        let entries = [long(0x0001, 1, order)];
        for header in [
            &b""[..],
            b"LEICA\0\x01\0",
            b"LEICA\0\x08\0",
            b"LEICA\0\x02\0",
        ] {
            let view = maker(note_with(header, &entries, order, 0)).leica(order, 0);
            let view = view.expect("reads").expect("an IFD dialect");
            assert_eq!(view.ifd().get_u32(0x0001), Some(1), "{header:?}");
        }
        let fuji = maker(note_with(b"FUJIFILM\x0c\0\0\0", &entries, order, 0));
        assert_eq!(fuji.leica(order, 0).expect("not an error"), None);
        let mut d_lux = note_with(b"LEICA CAMERA AG\0\0\0", &entries, order, 0);
        d_lux.extend_from_slice(&[0; 4]);
        assert_eq!(maker(d_lux).leica(order, 0).expect("not an error"), None);
        assert_eq!(maker(vec![0; 4]).leica(order, 0).expect("no layout"), None);
        let canon = MakerNote {
            vendor: MakerNoteVendor::Canon,
            bytes: note_with(b"", &entries, order, 0),
        };
        assert_eq!(canon.leica(order, 0).expect("not an error"), None);
    }

    #[test]
    fn a_leica3_note_counts_its_offsets_from_the_tiff_header() {
        let order = BE;
        let entries = [ascii(0x0001, b"R9 body\0")];
        let view = maker(note_with(b"", &entries, order, 500))
            .leica(order, 500)
            .expect("reads")
            .expect("Leica3");
        assert_eq!(view.dialect(), MakerNoteDialect::Leica3);
        assert_eq!(
            view.ifd().get(0x0001),
            Some(&Value::Ascii("R9 body".into()))
        );
    }

    #[test]
    fn a_leica5_note_counts_its_offsets_from_the_note_start() {
        let order = LE;
        let entries = [ascii(0x0001, b"X Vario\0")];
        let view = maker(note_with(b"LEICA\0\x04\0", &entries, order, 0))
            .leica(order, 700)
            .expect("reads")
            .expect("Leica5");
        assert_eq!(
            view.ifd().get(0x0001),
            Some(&Value::Ascii("X Vario".into()))
        );
    }

    /// A source that holds `bytes` but fails every read starting at or past `fail_at` as a
    /// transport error.
    struct Failing<'a> {
        bytes: &'a [u8],
        fail_at: u64,
    }

    impl ReadAt for Failing<'_> {
        fn read_exact_at(&mut self, offset: u64, buf: &mut [u8]) -> gamut_core::Result<()> {
            if offset >= self.fail_at {
                return Err(gamut_core::Error::Io(std::io::Error::other("transport")));
            }
            self.bytes.read_exact_at(offset, buf)
        }

        fn len(&mut self) -> gamut_core::Result<u64> {
            Ok(self.bytes.len() as u64)
        }
    }

    #[test]
    fn a_transport_failure_under_a_subdirectory_is_an_error() {
        let order = LE;
        let mut bytes = leica4_note(order, [&[], &[], &[], &[long(0x3901, 1, order)]]);
        let layout = maker(bytes.clone()).layout(order).expect("Leica4 layout");
        // The last subdirectory is its final 18 bytes (one inline entry). Shrink `0x3900`'s
        // `UNDEFINED` value to the 5 bytes from its start, and fail every read from 2 bytes in:
        // the value and the directory's entry count still read, the directory's entries do not.
        let count_at = 8 + 2 + 12 * 3 + 4;
        bytes[count_at..count_at + 4].copy_from_slice(&order.pack_u32(5));
        let fail_at = bytes.len() as u64 - 18 + 2;
        let read = LeicaMakerNote::read(
            Failing {
                bytes: &bytes,
                fail_at,
            },
            0,
            layout,
        );
        assert!(matches!(read, Err(crate::ExifError::Ifd(_))));
        // The same note over a source that never fails reads whole.
        let whole = LeicaMakerNote::read(&bytes[..], 0, layout).expect("reads");
        assert!(whole.is_some());
    }

    #[test]
    fn the_stream_and_the_note_alone_give_the_same_view() {
        let order = LE;
        let bytes = note_with(b"LEICA\0\x02\0", &leica9_entries(order), order, 64);
        let mut stream = vec![0; 64];
        stream.extend_from_slice(&bytes);
        let layout = maker(bytes.clone()).layout(order).expect("Leica9 layout");
        assert_eq!(layout.offset_base, OffsetBase::TiffHeader);
        let read = LeicaMakerNote::read(&stream[..], 64, layout).expect("reads");
        assert_eq!(read, maker(bytes).leica(order, 64).expect("reads"));
    }
}
