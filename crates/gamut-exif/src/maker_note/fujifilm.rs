//! A read-only view of a Fujifilm `MakerNote` (issue #981).
//!
//! A Fujifilm note opens with `FUJIFILM` and the IFD's offset as a little-endian `u32` at byte 8.
//! The IFD at that offset is little-endian whatever the outer stream's byte order, ends in a
//! next-IFD pointer, and counts its value offsets from the note's own start
//! ([`MakerNoteDialect::Fujifilm`]). The view decodes that IFD through [`gamut_ifd`] at the layout
//! [`MakerNoteLayout::detect`] resolves, keeps every entry as a [`Value`]
//! ([`FujifilmMakerNote::ifd`]), and adds typed accessors for the tags staked in
//! `references/exif/fujifilm/README.md`, after exiv2's `FujiMakerNote::tagInfo_[]`
//! (`third_party/exiv2/src/fujimn_int.cpp`), which is also the differential oracle.
//!
//! The same note in Leica's Fujifilm-built bodies resolves to the same dialect and reads through
//! the same view.
//!
//! Values are returned raw: `film_mode` is the number exiv2 prints as a film simulation, and the
//! arrays (`version`, `white_balance_fine_tune`) come back whole.
//!
//! The view never writes. The block it was read from re-emits byte-identically, because nothing
//! here changes it; its note-relative offsets stay valid wherever the writer places it (see the
//! [module docs](super)).

use gamut_ifd::{Ifd, ReadAt, Value};

use super::{MakerNoteDialect, MakerNoteLayout, read_note_ifd, single, slong, text};
use crate::error::Result;
use crate::value::{Rational, SRational};

/// `Version`: an `UNDEFINED` array.
const VERSION: u16 = 0x0000;
/// `SerialNumber`: text.
const SERIAL_NUMBER: u16 = 0x0010;
/// `Quality`: text.
const QUALITY: u16 = 0x1000;
/// `Sharpness`: a `SHORT`.
const SHARPNESS: u16 = 0x1001;
/// `WhiteBalance`: a `SHORT`.
const WHITE_BALANCE: u16 = 0x1002;
/// `Color`: a `SHORT`.
const COLOR: u16 = 0x1003;
/// `Tone`: a `SHORT`.
const TONE: u16 = 0x1004;
/// `ColorTemperature`: a `SHORT`, in kelvin.
const COLOR_TEMPERATURE: u16 = 0x1005;
/// `Contrast`: a `SHORT`.
const CONTRAST: u16 = 0x1006;
/// `WhiteBalanceFineTune`: an `SLONG` array.
const WHITE_BALANCE_FINE_TUNE: u16 = 0x100a;
/// `NoiseReduction`: a `SHORT`.
const NOISE_REDUCTION: u16 = 0x100b;
/// `HighIsoNoiseReduction`: a `SHORT`.
const HIGH_ISO_NOISE_REDUCTION: u16 = 0x100e;
/// `FlashMode`: a `SHORT`.
const FLASH_MODE: u16 = 0x1010;
/// `FlashStrength`: an `SRATIONAL`, in EV.
const FLASH_STRENGTH: u16 = 0x1011;
/// `Macro`: a `SHORT`.
const MACRO: u16 = 0x1020;
/// `FocusMode`: a `SHORT`.
const FOCUS_MODE: u16 = 0x1021;
/// `SlowSync`: a `SHORT`.
const SLOW_SYNC: u16 = 0x1030;
/// `PictureMode`: a `SHORT`.
const PICTURE_MODE: u16 = 0x1031;
/// `ShadowTone`: an `SLONG`.
const SHADOW_TONE: u16 = 0x1040;
/// `HighlightTone`: an `SLONG`.
const HIGHLIGHT_TONE: u16 = 0x1041;
/// `Continuous`: a `SHORT`.
const CONTINUOUS: u16 = 0x1100;
/// `SequenceNumber`: a `SHORT`.
const SEQUENCE_NUMBER: u16 = 0x1101;
/// `BlurWarning`: a `SHORT`.
const BLUR_WARNING: u16 = 0x1300;
/// `FocusWarning`: a `SHORT`.
const FOCUS_WARNING: u16 = 0x1301;
/// `ExposureWarning`: a `SHORT`.
const EXPOSURE_WARNING: u16 = 0x1302;
/// `DynamicRange`: a `SHORT`.
const DYNAMIC_RANGE: u16 = 0x1400;
/// `FilmMode`: a `SHORT`.
const FILM_MODE: u16 = 0x1401;
/// `DynamicRangeSetting`: a `SHORT`.
const DYNAMIC_RANGE_SETTING: u16 = 0x1402;
/// `MinFocalLength`: a `RATIONAL`, in millimetres.
const MIN_FOCAL_LENGTH: u16 = 0x1404;
/// `MaxFocalLength`: a `RATIONAL`, in millimetres.
const MAX_FOCAL_LENGTH: u16 = 0x1405;
/// `MaxApertureAtMinFocal`: a `RATIONAL` f-number.
const MAX_APERTURE_AT_MIN_FOCAL: u16 = 0x1406;
/// `MaxApertureAtMaxFocal`: a `RATIONAL` f-number.
const MAX_APERTURE_AT_MAX_FOCAL: u16 = 0x1407;
/// `Rating`: a `LONG`.
const RATING: u16 = 0x1431;

/// The decoded IFD of a Fujifilm `MakerNote`: every entry, plus typed accessors for the tags the
/// [module docs](self) name.
///
/// Built by [`MakerNote::fujifilm`](super::MakerNote::fujifilm) from the note's own bytes, or by
/// [`read`](Self::read) from the whole TIFF stream.
#[derive(Debug, Clone, PartialEq)]
pub struct FujifilmMakerNote {
    ifd: Ifd,
    unreadable: Vec<u16>,
}

impl FujifilmMakerNote {
    /// Decodes the Fujifilm note laid out as `layout` that starts at `note_at` in `stream` — the
    /// TIFF stream the note sits in (the bytes after any `Exif\0\0` marker), or any [`ReadAt`] in
    /// that stream's offset space.
    ///
    /// `Ok(None)` when `layout` is not [`MakerNoteDialect::Fujifilm`]. An entry whose value cannot
    /// be read is left out of [`ifd`](Self::ifd) and named by
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
        if layout.dialect != MakerNoteDialect::Fujifilm {
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

    /// `Version` (`0x0000`): the whole `UNDEFINED` value (four ASCII digits, such as `b"0130"`, in
    /// Fujifilm's bodies).
    #[must_use]
    pub fn version(&self) -> Option<&[u8]> {
        match self.ifd.get(VERSION)? {
            Value::Undefined(values) => Some(values),
            _ => None,
        }
    }

    /// `SerialNumber` (`0x0010`): Fujifilm's internal serial, not the one printed on the body.
    #[must_use]
    pub fn serial_number(&self) -> Option<&str> {
        text(&self.ifd, SERIAL_NUMBER)
    }

    /// `Quality` (`0x1000`), e.g. `"NORMAL"` or `"FINE"`.
    #[must_use]
    pub fn quality(&self) -> Option<&str> {
        text(&self.ifd, QUALITY)
    }

    /// `Sharpness` (`0x1001`): Fujifilm's sharpness code.
    #[must_use]
    pub fn sharpness(&self) -> Option<u16> {
        self.short(SHARPNESS)
    }

    /// `WhiteBalance` (`0x1002`): Fujifilm's white-balance code.
    #[must_use]
    pub fn white_balance(&self) -> Option<u16> {
        self.short(WHITE_BALANCE)
    }

    /// `Color` (`0x1003`): Fujifilm's chroma-saturation code.
    #[must_use]
    pub fn color(&self) -> Option<u16> {
        self.short(COLOR)
    }

    /// `Tone` (`0x1004`): Fujifilm's tone (contrast) code.
    #[must_use]
    pub fn tone(&self) -> Option<u16> {
        self.short(TONE)
    }

    /// `ColorTemperature` (`0x1005`), in kelvin.
    #[must_use]
    pub fn color_temperature(&self) -> Option<u16> {
        self.short(COLOR_TEMPERATURE)
    }

    /// `Contrast` (`0x1006`): Fujifilm's contrast code.
    #[must_use]
    pub fn contrast(&self) -> Option<u16> {
        self.short(CONTRAST)
    }

    /// `WhiteBalanceFineTune` (`0x100a`): the whole `SLONG` array (red, then blue, in Fujifilm's
    /// bodies), unscaled — exiv2 prints each divided by 20.
    #[must_use]
    pub fn white_balance_fine_tune(&self) -> Option<&[i32]> {
        match self.ifd.get(WHITE_BALANCE_FINE_TUNE)? {
            Value::SLong(values) => Some(values),
            _ => None,
        }
    }

    /// `NoiseReduction` (`0x100b`): Fujifilm's noise-reduction code.
    #[must_use]
    pub fn noise_reduction(&self) -> Option<u16> {
        self.short(NOISE_REDUCTION)
    }

    /// `HighIsoNoiseReduction` (`0x100e`): Fujifilm's high-ISO noise-reduction code.
    #[must_use]
    pub fn high_iso_noise_reduction(&self) -> Option<u16> {
        self.short(HIGH_ISO_NOISE_REDUCTION)
    }

    /// `FlashMode` (`0x1010`): Fujifilm's flash-mode code.
    #[must_use]
    pub fn flash_mode(&self) -> Option<u16> {
        self.short(FLASH_MODE)
    }

    /// `FlashStrength` (`0x1011`): flash compensation in EV, the `SRATIONAL` exactly as stored
    /// (see [`SRational::to_f64`] for the value).
    #[must_use]
    pub fn flash_strength(&self) -> Option<SRational> {
        match self.ifd.get(FLASH_STRENGTH)? {
            Value::SRational(values) => single(values).map(SRational::from),
            _ => None,
        }
    }

    /// `Macro` (`0x1020`): 0 off, 1 on.
    #[must_use]
    pub fn macro_mode(&self) -> Option<u16> {
        self.short(MACRO)
    }

    /// `FocusMode` (`0x1021`): Fujifilm's focus-mode code.
    #[must_use]
    pub fn focus_mode(&self) -> Option<u16> {
        self.short(FOCUS_MODE)
    }

    /// `SlowSync` (`0x1030`): 0 off, 1 on.
    #[must_use]
    pub fn slow_sync(&self) -> Option<u16> {
        self.short(SLOW_SYNC)
    }

    /// `PictureMode` (`0x1031`): Fujifilm's exposure-program code.
    #[must_use]
    pub fn picture_mode(&self) -> Option<u16> {
        self.short(PICTURE_MODE)
    }

    /// `ShadowTone` (`0x1040`).
    #[must_use]
    pub fn shadow_tone(&self) -> Option<i32> {
        slong(&self.ifd, SHADOW_TONE)
    }

    /// `HighlightTone` (`0x1041`).
    #[must_use]
    pub fn highlight_tone(&self) -> Option<i32> {
        slong(&self.ifd, HIGHLIGHT_TONE)
    }

    /// `Continuous` (`0x1100`): Fujifilm's continuous-shooting or bracketing code.
    #[must_use]
    pub fn continuous(&self) -> Option<u16> {
        self.short(CONTINUOUS)
    }

    /// `SequenceNumber` (`0x1101`).
    #[must_use]
    pub fn sequence_number(&self) -> Option<u16> {
        self.short(SEQUENCE_NUMBER)
    }

    /// `BlurWarning` (`0x1300`): 0 none, 1 blur warning.
    #[must_use]
    pub fn blur_warning(&self) -> Option<u16> {
        self.short(BLUR_WARNING)
    }

    /// `FocusWarning` (`0x1301`): 0 good focus, 1 out of focus.
    #[must_use]
    pub fn focus_warning(&self) -> Option<u16> {
        self.short(FOCUS_WARNING)
    }

    /// `ExposureWarning` (`0x1302`): 0 good exposure, 1 bad exposure.
    #[must_use]
    pub fn exposure_warning(&self) -> Option<u16> {
        self.short(EXPOSURE_WARNING)
    }

    /// `DynamicRange` (`0x1400`): Fujifilm's dynamic-range code.
    #[must_use]
    pub fn dynamic_range(&self) -> Option<u16> {
        self.short(DYNAMIC_RANGE)
    }

    /// `FilmMode` (`0x1401`): the film-simulation code (0 is PROVIA).
    #[must_use]
    pub fn film_mode(&self) -> Option<u16> {
        self.short(FILM_MODE)
    }

    /// `DynamicRangeSetting` (`0x1402`): Fujifilm's dynamic-range-setting code.
    #[must_use]
    pub fn dynamic_range_setting(&self) -> Option<u16> {
        self.short(DYNAMIC_RANGE_SETTING)
    }

    /// `MinFocalLength` (`0x1404`): the lens's shortest focal length in millimetres.
    #[must_use]
    pub fn min_focal_length(&self) -> Option<Rational> {
        self.rational(MIN_FOCAL_LENGTH)
    }

    /// `MaxFocalLength` (`0x1405`): the lens's longest focal length in millimetres.
    #[must_use]
    pub fn max_focal_length(&self) -> Option<Rational> {
        self.rational(MAX_FOCAL_LENGTH)
    }

    /// `MaxApertureAtMinFocal` (`0x1406`): the widest f-number at the shortest focal length.
    #[must_use]
    pub fn max_aperture_at_min_focal(&self) -> Option<Rational> {
        self.rational(MAX_APERTURE_AT_MIN_FOCAL)
    }

    /// `MaxApertureAtMaxFocal` (`0x1407`): the widest f-number at the longest focal length.
    #[must_use]
    pub fn max_aperture_at_max_focal(&self) -> Option<Rational> {
        self.rational(MAX_APERTURE_AT_MAX_FOCAL)
    }

    /// `Rating` (`0x1431`).
    #[must_use]
    pub fn rating(&self) -> Option<u32> {
        self.ifd.get_u32(RATING)
    }

    /// `tag` as a single `SHORT`, or `None` if absent, of another type, or not one value.
    fn short(&self, tag: u16) -> Option<u16> {
        match self.ifd.get(tag)? {
            Value::Short(values) => single(values),
            _ => None,
        }
    }

    /// `tag` as a single `RATIONAL`, or `None` if absent, of another type, or not one value.
    fn rational(&self, tag: u16) -> Option<Rational> {
        match self.ifd.get(tag)? {
            Value::Rational(values) => single(values).map(Rational::from),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use gamut_ifd::ByteOrder::{BigEndian as BE, LittleEndian as LE};
    use gamut_ifd::FieldType;

    use super::super::{MakerNote, MakerNoteVendor};
    use super::*;

    /// One IFD entry to lay out: tag, field type, count, and the value's little-endian bytes.
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

    fn shorts(tag: u16, values: &[u16]) -> Entry {
        let bytes = values.iter().flat_map(|&v| LE.pack_u16(v)).collect();
        entry(tag, FieldType::Short, values.len(), bytes)
    }

    fn longs(tag: u16, values: &[u32]) -> Entry {
        let bytes = values.iter().flat_map(|&v| LE.pack_u32(v)).collect();
        entry(tag, FieldType::Long, values.len(), bytes)
    }

    fn slongs(tag: u16, values: &[i32]) -> Entry {
        let bytes = values.iter().flat_map(|&v| LE.pack_u32(v as u32)).collect();
        entry(tag, FieldType::SLong, values.len(), bytes)
    }

    fn rationals(tag: u16, values: &[(u32, u32)]) -> Entry {
        let bytes = values
            .iter()
            .flat_map(|&(n, d)| [LE.pack_u32(n), LE.pack_u32(d)])
            .flatten()
            .collect();
        entry(tag, FieldType::Rational, values.len(), bytes)
    }

    fn srationals(tag: u16, values: &[(i32, i32)]) -> Entry {
        let bytes = values
            .iter()
            .flat_map(|&(n, d)| [LE.pack_u32(n as u32), LE.pack_u32(d as u32)])
            .flatten()
            .collect();
        entry(tag, FieldType::SRational, values.len(), bytes)
    }

    fn undefined(tag: u16, values: &[u8]) -> Entry {
        entry(tag, FieldType::Undefined, values.len(), values.to_vec())
    }

    fn ascii(tag: u16, text: &[u8]) -> Entry {
        entry(tag, FieldType::Ascii, text.len(), text.to_vec())
    }

    /// A Fujifilm note: the `FUJIFILM` header naming its IFD at byte 12, the little-endian IFD with
    /// a zero next-IFD pointer, then each out-of-line value, addressed from the note's start.
    fn fujifilm_note(entries: &[Entry]) -> Vec<u8> {
        let count = entries.len() as u32;
        let mut data_at = 12 + 2 + 12 * count + 4;
        let mut note = b"FUJIFILM".to_vec();
        note.extend_from_slice(&LE.pack_u32(12));
        note.extend_from_slice(&LE.pack_u16(count as u16));
        let mut data = Vec::new();
        for entry in entries {
            note.extend_from_slice(&LE.pack_u16(entry.tag));
            note.extend_from_slice(&LE.pack_u16(entry.ty.code()));
            note.extend_from_slice(&LE.pack_u32(entry.count));
            if entry.bytes.len() <= 4 {
                let mut word = entry.bytes.clone();
                word.resize(4, 0);
                note.extend_from_slice(&word);
            } else {
                note.extend_from_slice(&LE.pack_u32(data_at));
                data.extend_from_slice(&entry.bytes);
                data_at += entry.bytes.len() as u32;
            }
        }
        note.extend_from_slice(&[0; 4]);
        note.extend_from_slice(&data);
        note
    }

    fn maker(bytes: Vec<u8>) -> MakerNote {
        MakerNote {
            vendor: MakerNoteVendor::Fujifilm,
            bytes,
        }
    }

    fn view(entries: &[Entry]) -> FujifilmMakerNote {
        maker(fujifilm_note(entries))
            .fujifilm(LE, 0)
            .expect("reads")
            .expect("is Fujifilm")
    }

    /// One entry per accessor, each holding a value no other accessor's tag holds, so reading the
    /// wrong tag is always visible.
    fn every_tag() -> Vec<Entry> {
        vec![
            undefined(VERSION, b"0130"),
            ascii(SERIAL_NUMBER, b"FC  A1234567\0"),
            ascii(QUALITY, b"FINE\0\0\0\0"),
            shorts(SHARPNESS, &[3]),
            shorts(WHITE_BALANCE, &[0x100]),
            shorts(COLOR, &[0x200]),
            shorts(TONE, &[0x300]),
            shorts(COLOR_TEMPERATURE, &[5_600]),
            shorts(CONTRAST, &[0x400]),
            slongs(WHITE_BALANCE_FINE_TUNE, &[-40, 60]),
            shorts(NOISE_REDUCTION, &[0x40]),
            shorts(HIGH_ISO_NOISE_REDUCTION, &[0x280]),
            shorts(FLASH_MODE, &[0x8120]),
            srationals(FLASH_STRENGTH, &[(-2, 3)]),
            shorts(MACRO, &[12]),
            shorts(FOCUS_MODE, &[2]),
            shorts(SLOW_SYNC, &[4]),
            shorts(PICTURE_MODE, &[5]),
            slongs(SHADOW_TONE, &[-16]),
            slongs(HIGHLIGHT_TONE, &[32]),
            shorts(CONTINUOUS, &[6]),
            shorts(SEQUENCE_NUMBER, &[7]),
            shorts(BLUR_WARNING, &[8]),
            shorts(FOCUS_WARNING, &[9]),
            shorts(EXPOSURE_WARNING, &[10]),
            shorts(DYNAMIC_RANGE, &[11]),
            shorts(FILM_MODE, &[0x600]),
            shorts(DYNAMIC_RANGE_SETTING, &[0x200 + 1]),
            rationals(MIN_FOCAL_LENGTH, &[(18, 1)]),
            rationals(MAX_FOCAL_LENGTH, &[(55, 1)]),
            rationals(MAX_APERTURE_AT_MIN_FOCAL, &[(28, 10)]),
            rationals(MAX_APERTURE_AT_MAX_FOCAL, &[(4, 1)]),
            longs(RATING, &[3_000]),
        ]
    }

    #[test]
    fn every_accessor_reads_its_own_tag() {
        let view = view(&every_tag());
        assert_eq!(view.version(), Some(&b"0130"[..]));
        assert_eq!(view.serial_number(), Some("FC  A1234567"));
        assert_eq!(view.quality(), Some("FINE"));
        assert_eq!(view.sharpness(), Some(3));
        assert_eq!(view.white_balance(), Some(0x100));
        assert_eq!(view.color(), Some(0x200));
        assert_eq!(view.tone(), Some(0x300));
        assert_eq!(view.color_temperature(), Some(5_600));
        assert_eq!(view.contrast(), Some(0x400));
        assert_eq!(view.white_balance_fine_tune(), Some(&[-40, 60][..]));
        assert_eq!(view.noise_reduction(), Some(0x40));
        assert_eq!(view.high_iso_noise_reduction(), Some(0x280));
        assert_eq!(view.flash_mode(), Some(0x8120));
        assert_eq!(view.flash_strength(), Some(SRational::from((-2, 3))));
        assert_eq!(view.macro_mode(), Some(12));
        assert_eq!(view.focus_mode(), Some(2));
        assert_eq!(view.slow_sync(), Some(4));
        assert_eq!(view.picture_mode(), Some(5));
        assert_eq!(view.shadow_tone(), Some(-16));
        assert_eq!(view.highlight_tone(), Some(32));
        assert_eq!(view.continuous(), Some(6));
        assert_eq!(view.sequence_number(), Some(7));
        assert_eq!(view.blur_warning(), Some(8));
        assert_eq!(view.focus_warning(), Some(9));
        assert_eq!(view.exposure_warning(), Some(10));
        assert_eq!(view.dynamic_range(), Some(11));
        assert_eq!(view.film_mode(), Some(0x600));
        assert_eq!(view.dynamic_range_setting(), Some(0x201));
        assert_eq!(view.min_focal_length(), Some(Rational::from((18, 1))));
        assert_eq!(view.max_focal_length(), Some(Rational::from((55, 1))));
        assert_eq!(
            view.max_aperture_at_min_focal(),
            Some(Rational::from((28, 10)))
        );
        assert_eq!(
            view.max_aperture_at_max_focal(),
            Some(Rational::from((4, 1)))
        );
        assert_eq!(view.rating(), Some(3_000));
        assert_eq!(view.ifd().fields().len(), 33);
        assert!(view.unreadable_tags().is_empty());
    }

    /// The note is little-endian whatever the outer stream's order, so the outer order changes
    /// nothing.
    #[test]
    fn the_outer_byte_order_is_ignored() {
        let note = maker(fujifilm_note(&every_tag()));
        let big = note.fujifilm(BE, 0).expect("reads").expect("is Fujifilm");
        assert_eq!(big.film_mode(), Some(0x600));
        assert_eq!(Some(big), note.fujifilm(LE, 0).expect("reads"));
    }

    /// Each typed accessor whose type is fixed refuses the entry under another type, and the raw
    /// value stays reachable through `ifd()`.
    #[test]
    fn a_retyped_tag_reads_as_none() {
        let view = view(&[
            ascii(VERSION, b"0130"),
            shorts(QUALITY, &[1]),
            longs(SHARPNESS, &[1]),
            shorts(WHITE_BALANCE_FINE_TUNE, &[1, 2]),
            rationals(FLASH_STRENGTH, &[(1, 3)]),
            longs(SHADOW_TONE, &[1]),
            srationals(MIN_FOCAL_LENGTH, &[(18, 1)]),
        ]);
        assert_eq!(view.version(), None);
        assert_eq!(view.quality(), None);
        assert_eq!(view.sharpness(), None);
        assert_eq!(view.white_balance_fine_tune(), None);
        assert_eq!(view.flash_strength(), None);
        assert_eq!(view.shadow_tone(), None);
        assert_eq!(view.min_focal_length(), None);
        assert_eq!(view.ifd().get(SHARPNESS), Some(&Value::Long(vec![1])));
    }

    #[test]
    fn an_absent_tag_reads_as_none() {
        let view = view(&[shorts(SHARPNESS, &[1])]);
        assert_eq!(view.version(), None);
        assert_eq!(view.quality(), None);
        assert_eq!(view.white_balance(), None);
        assert_eq!(view.white_balance_fine_tune(), None);
        assert_eq!(view.flash_strength(), None);
        assert_eq!(view.highlight_tone(), None);
        assert_eq!(view.max_focal_length(), None);
        assert_eq!(view.rating(), None);
    }

    /// A scalar accessor of a fixed type takes exactly one value; exiv2 would print them all.
    #[test]
    fn a_scalar_with_more_than_one_value_reads_as_none() {
        let view = view(&[
            shorts(FILM_MODE, &[1, 2]),
            srationals(FLASH_STRENGTH, &[(1, 3), (2, 3)]),
            rationals(MAX_FOCAL_LENGTH, &[(55, 1), (56, 1)]),
        ]);
        assert_eq!(view.film_mode(), None);
        assert_eq!(view.flash_strength(), None);
        assert_eq!(view.max_focal_length(), None);
    }

    #[test]
    fn text_keeps_the_padding_in_the_raw_value() {
        let view = view(&[ascii(QUALITY, b"NORMAL\0\0")]);
        assert_eq!(view.quality(), Some("NORMAL"));
        assert_eq!(
            view.ifd().get(QUALITY),
            Some(&Value::Ascii("NORMAL\0".into()))
        );
    }

    /// Offsets count from the note's start, so where the note stands changes nothing, and the
    /// whole stream reads the same as the note alone.
    #[test]
    fn offsets_count_from_the_note() {
        let bytes = fujifilm_note(&every_tag());
        let alone = maker(bytes.clone()).fujifilm(LE, 0).expect("reads");
        let placed = maker(bytes.clone()).fujifilm(LE, 300).expect("reads");
        assert_eq!(placed, alone);
        let mut stream = vec![0; 300];
        stream.extend_from_slice(&bytes);
        let layout = maker(bytes).layout(LE).expect("Fujifilm layout");
        let read = FujifilmMakerNote::read(&stream[..], 300, layout).expect("reads");
        assert_eq!(read, alone);
        // Read at the wrong position, the IFD itself is somewhere else.
        let wrong = FujifilmMakerNote::read(&stream[..], 200, layout).expect("reads");
        assert_ne!(wrong, alone);
    }

    /// A value whose bytes run past the note is left out and named, and the rest still reads.
    #[test]
    fn a_value_past_the_note_is_unreadable() {
        let mut bytes = fujifilm_note(&[shorts(SHARPNESS, &[1]), ascii(QUALITY, b"FINE\0\0\0\0")]);
        bytes.truncate(bytes.len() - 1);
        let view = maker(bytes)
            .fujifilm(LE, 0)
            .expect("reads")
            .expect("is Fujifilm");
        assert_eq!(view.unreadable_tags(), &[QUALITY]);
        assert_eq!(view.quality(), None);
        assert_eq!(view.sharpness(), Some(1));
    }

    #[test]
    fn a_note_without_its_next_ifd_pointer_is_an_error() {
        let mut bytes = fujifilm_note(&[shorts(SHARPNESS, &[1])]);
        bytes.truncate(12 + 14);
        assert!(maker(bytes.clone()).layout(LE).is_some());
        assert!(maker(bytes).fujifilm(LE, 0).is_err());
    }

    /// Leica's Fujifilm-built bodies write the same note, and the Leica dispatch routes it here.
    #[test]
    fn a_leica_note_in_the_fujifilm_dialect_reads() {
        let note = MakerNote {
            vendor: MakerNoteVendor::Leica,
            bytes: fujifilm_note(&[shorts(FILM_MODE, &[0x600])]),
        };
        let view = note.fujifilm(LE, 0).expect("reads").expect("is Fujifilm");
        assert_eq!(view.film_mode(), Some(0x600));
    }

    #[test]
    fn only_a_fujifilm_layout_is_read() {
        let bytes = fujifilm_note(&[shorts(SHARPNESS, &[1])]);
        let sony = MakerNote {
            vendor: MakerNoteVendor::Sony,
            bytes: bytes.clone(),
        };
        assert!(sony.layout(LE).is_some());
        assert_eq!(sony.fujifilm(LE, 0).expect("not an error"), None);
        let unknown = MakerNote {
            vendor: MakerNoteVendor::Unknown,
            bytes,
        };
        assert_eq!(unknown.fujifilm(LE, 0).expect("not an error"), None);
    }

    #[test]
    fn rating_accepts_any_unsigned_integer() {
        let view = view(&[shorts(RATING, &[4])]);
        assert_eq!(view.rating(), Some(4));
    }
}
