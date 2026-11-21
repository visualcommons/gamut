//! A read-only view of a Nikon `MakerNote` (issue #979).

mod record;

use gamut_ifd::{ByteOrder, Ifd, ReadAt, Value};
pub use record::{NikonRecord, NikonRecordKind};

use super::{MakerNote, MakerNoteDialect, MakerNoteLayout, Placed, read_note_ifd, single, text};
use crate::error::Result;
use crate::value::{Rational, SRational};

/// `Version` (`Nikon3`, `Nikon1`): an `UNDEFINED` array.
const VERSION: u16 = 0x0001;
/// `ISOSpeed` (`Nikon3`, `Nikon1`): a `SHORT` array.
const ISO_SPEED: u16 = 0x0002;
/// `ColorMode` (`Nikon3`, `Nikon1`): text.
const COLOR_MODE: u16 = 0x0003;
/// `Quality` (`Nikon3`, `Nikon1`): text.
const QUALITY: u16 = 0x0004;
/// `WhiteBalance` (`Nikon3`, `Nikon1`): text.
const WHITE_BALANCE: u16 = 0x0005;
/// `Sharpening` (`Nikon3`, `Nikon1`): text.
const SHARPENING: u16 = 0x0006;
/// `Focus` (`Nikon3`, `Nikon1`): text.
const FOCUS: u16 = 0x0007;
/// `FlashSetting` (`Nikon3`, `Nikon1`): text.
const FLASH_SETTING: u16 = 0x0008;
/// `FlashDevice`: text.
const FLASH_DEVICE: u16 = 0x0009;
/// `WhiteBalanceBias`: an `SSHORT` array.
const WHITE_BALANCE_BIAS: u16 = 0x000b;
/// `WB_RBLevels`: a `RATIONAL` array.
const WB_RB_LEVELS: u16 = 0x000c;
/// `ISOSelection` (`Nikon3`, `Nikon1`): text.
const ISO_SELECTION: u16 = 0x000f;
/// `ExposureBracketComp`: an `SRATIONAL`.
const EXPOSURE_BRACKET_COMP: u16 = 0x0019;
/// `SerialNumber`: text.
const SERIAL_NUMBER: u16 = 0x001d;
/// `ColorSpace`: a `SHORT`.
const COLOR_SPACE: u16 = 0x001e;
/// `ActiveDLighting`: a `SHORT`.
const ACTIVE_D_LIGHTING: u16 = 0x0022;
/// `VignetteControl`: a `SHORT`.
const VIGNETTE_CONTROL: u16 = 0x002a;
/// `ImageAdjustment` (`Nikon3`, `Nikon1`): text.
const IMAGE_ADJUSTMENT: u16 = 0x0080;
/// `ToneComp`: text.
const TONE_COMP: u16 = 0x0081;
/// `AuxiliaryLens` (`Nikon3`, `Nikon1`): text.
const AUXILIARY_LENS: u16 = 0x0082;
/// `LensType`: a `BYTE`.
const LENS_TYPE: u16 = 0x0083;
/// `Lens`: a `RATIONAL` array.
const LENS: u16 = 0x0084;
/// `FocusDistance` (`Nikon3`, `Nikon1`): a `RATIONAL`.
const FOCUS_DISTANCE: u16 = 0x0085;
/// `DigitalZoom` (`Nikon3`, `Nikon1`): a `RATIONAL`.
const DIGITAL_ZOOM: u16 = 0x0086;
/// `FlashMode`: a `BYTE`.
const FLASH_MODE: u16 = 0x0087;
/// `ShootingMode`: a `SHORT`.
const SHOOTING_MODE: u16 = 0x0089;
/// `SceneMode`: text.
const SCENE_MODE: u16 = 0x008f;
/// `LightSource`: text.
const LIGHT_SOURCE: u16 = 0x0090;
/// `ShotInfo`: an `UNDEFINED` record.
const SHOT_INFO: u16 = 0x0091;
/// `HueAdjustment`: an `SSHORT`.
const HUE_ADJUSTMENT: u16 = 0x0092;
/// `NEFCompression`: a `SHORT`.
const NEF_COMPRESSION: u16 = 0x0093;
/// `NoiseReduction`: text.
const NOISE_REDUCTION: u16 = 0x0095;
/// `ColorBalance`: an `UNDEFINED` record.
const COLOR_BALANCE: u16 = 0x0097;
/// `LensData`: an `UNDEFINED` record.
const LENS_DATA: u16 = 0x0098;
/// `ImageCount`: a `LONG`.
const IMAGE_COUNT: u16 = 0x00a5;
/// `ShutterCount`: a `LONG`.
const SHUTTER_COUNT: u16 = 0x00a7;
/// `ImageStabilization`: text.
const IMAGE_STABILIZATION: u16 = 0x00ac;
/// `HighISONoiseReduction`: a `SHORT`.
const HIGH_ISO_NOISE_REDUCTION: u16 = 0x00b1;

// exiv2's `Nikon2` table, numbered apart from the two above; `Focus` and `DigitalZoom` are
// `RATIONAL`s and the rest `SHORT`s.
const N2_QUALITY: u16 = 0x0003;
const N2_COLOR_MODE: u16 = 0x0004;
const N2_IMAGE_ADJUSTMENT: u16 = 0x0005;
const N2_ISO_SPEED: u16 = 0x0006;
const N2_WHITE_BALANCE: u16 = 0x0007;
const N2_FOCUS: u16 = 0x0008;
const N2_DIGITAL_ZOOM: u16 = 0x000a;
const N2_AUXILIARY_LENS: u16 = 0x000b;

impl MakerNote {
    /// The block decoded as a Nikon note, given `outer` and `note_at` as for
    /// [`canon`](Self::canon). A [`Nikon3`](MakerNoteDialect::Nikon3) note counts its value offsets
    /// from its own embedded TIFF header, so it reads completely from these bytes; a
    /// [`Nikon1`](MakerNoteDialect::Nikon1) or [`Nikon2`](MakerNoteDialect::Nikon2) note counts
    /// them from the outer TIFF header, so `note_at` places the bytes in the stream's offset space
    /// and a value outside the note is unreadable here.
    ///
    /// Such a value is named by [`NikonMakerNote::unreadable_tags`];
    /// [`NikonMakerNote::read`] reads through the whole stream instead.
    ///
    /// `Ok(None)` when the block has no Nikon layout.
    ///
    /// # Errors
    ///
    /// [`ExifError::Core`](crate::ExifError::Core) when the note's IFD itself — its entry count,
    /// entries and next-IFD pointer — does not fit in the note.
    pub fn nikon(&self, outer: ByteOrder, note_at: u64) -> Result<Option<NikonMakerNote>> {
        let Some(layout) = self.layout(outer) else {
            return Ok(None);
        };
        let placed = Placed {
            bytes: &self.bytes,
            at: note_at,
        };
        NikonMakerNote::read(placed, note_at, layout)
    }
}

/// The decoded IFD of a Nikon `MakerNote`: every entry, plus typed accessors for the tags staked
/// in `references/exif/nikon/README.md` (issue #979).
///
/// Nikon writes three dialects ([`MakerNoteLayout::detect`] tells them apart): a bare IFD at byte
/// 0 ([`Nikon1`](MakerNoteDialect::Nikon1)); `Nikon\0` and two version bytes, then the IFD at
/// byte 8 ([`Nikon2`](MakerNoteDialect::Nikon2)) — both in the outer byte order with offsets from
/// the outer TIFF header — and `Nikon\0`, four version bytes and an embedded TIFF header at byte
/// 10 whose byte order the IFD follows and from which its offsets count
/// ([`Nikon3`](MakerNoteDialect::Nikon3), every Nikon body since the early 2000s). The view
/// decodes the IFD through [`gamut_ifd`] at that layout and keeps every entry as a [`Value`]
/// ([`NikonMakerNote::ifd`]). Typed accessors follow exiv2's tables
/// (`third_party/exiv2/src/nikonmn_int.cpp`), which are also the differential oracle:
/// [`nikon3`](Self::nikon3) for a `Nikon3` or `Nikon1` note, whose numbering `Nikon1` shares,
/// and [`nikon2`](Self::nikon2) for a `Nikon2` note.
///
/// Values are returned raw. The enciphered records (`ShotInfo`, `ColorBalance`, `LensData`) come
/// back opaque, as a [`NikonRecord`] naming the record and its version.
///
/// The view never writes. The block it was read from re-emits byte-identically, because nothing
/// here changes it; see [`MakerNote`] on how the writer keeps file-absolute offsets valid.
///
/// Built by [`MakerNote::nikon`] from the note's own bytes, or by [`read`](Self::read) from the
/// whole TIFF stream.
#[derive(Debug, Clone, PartialEq)]
pub struct NikonMakerNote {
    dialect: MakerNoteDialect,
    ifd: Ifd,
    unreadable: Vec<u16>,
}

impl NikonMakerNote {
    /// Decodes the Nikon note laid out as `layout` that starts at `note_at` in `stream` — the TIFF
    /// stream the note sits in (the bytes after any `Exif\0\0` marker), or any [`ReadAt`] in that
    /// stream's offset space.
    ///
    /// `Ok(None)` when `layout` is not one of the three Nikon dialects. An entry whose value cannot
    /// be read is left out of [`ifd`](Self::ifd) and named by
    /// [`unreadable_tags`](Self::unreadable_tags), as exiv2 skips it too.
    ///
    /// # Errors
    ///
    /// [`ExifError::Core`](crate::ExifError::Core) when the note's IFD — entry count, entries and
    /// next-IFD pointer — does not fit in `stream`, or `stream` fails;
    /// [`ExifError::BadOffset`](crate::ExifError::BadOffset) when `note_at` plus the IFD's offset
    /// overflows.
    pub fn read<S: ReadAt>(
        stream: S,
        note_at: u64,
        layout: MakerNoteLayout,
    ) -> Result<Option<Self>> {
        use MakerNoteDialect as D;
        if !matches!(layout.dialect, D::Nikon1 | D::Nikon2 | D::Nikon3) {
            return Ok(None);
        }
        let (ifd, unreadable) = read_note_ifd(stream, note_at, layout)?;
        Ok(Some(Self {
            dialect: layout.dialect,
            ifd,
            unreadable,
        }))
    }

    /// The dialect the note was read as.
    #[must_use]
    pub fn dialect(&self) -> MakerNoteDialect {
        self.dialect
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

    /// The tags of exiv2's `Nikon3` table, for a `Nikon3` note and for a `Nikon1` note — every
    /// `Nikon1` tag has the same number, name and type in that table — or `None` for a `Nikon2`
    /// note.
    #[must_use]
    pub fn nikon3(&self) -> Option<Nikon3Tags<'_>> {
        (self.dialect != MakerNoteDialect::Nikon2).then_some(Nikon3Tags { ifd: &self.ifd })
    }

    /// The tags of exiv2's `Nikon2` table, or `None` for a note of another dialect.
    #[must_use]
    pub fn nikon2(&self) -> Option<Nikon2Tags<'_>> {
        (self.dialect == MakerNoteDialect::Nikon2).then_some(Nikon2Tags { ifd: &self.ifd })
    }
}

/// The tags of exiv2's `Nikon3` table (`Nikon3MakerNote::tagInfo_[]`) that a
/// [`NikonMakerNote`] types, raw as stored. Every accessor returns `None` for an absent tag or one
/// stored in another type; a scalar also for a count other than one.
#[derive(Debug, Clone, Copy)]
pub struct Nikon3Tags<'a> {
    ifd: &'a Ifd,
}

impl<'a> Nikon3Tags<'a> {
    /// `Version` (`0x0001`): the whole `UNDEFINED` array, usually four ASCII digits (`"0210"`).
    #[must_use]
    pub fn version(&self) -> Option<&'a [u8]> {
        undefined(self.ifd, VERSION)
    }

    /// `ISOSpeed` (`0x0002`): the whole `SHORT` array (exiv2 prints its second value).
    #[must_use]
    pub fn iso_speed(&self) -> Option<&'a [u16]> {
        shorts(self.ifd, ISO_SPEED)
    }

    /// `ColorMode` (`0x0003`), e.g. `"COLOR"`.
    #[must_use]
    pub fn color_mode(&self) -> Option<&'a str> {
        text(self.ifd, COLOR_MODE)
    }

    /// `Quality` (`0x0004`), e.g. `"FINE"`.
    #[must_use]
    pub fn quality(&self) -> Option<&'a str> {
        text(self.ifd, QUALITY)
    }

    /// `WhiteBalance` (`0x0005`), e.g. `"AUTO"`.
    #[must_use]
    pub fn white_balance(&self) -> Option<&'a str> {
        text(self.ifd, WHITE_BALANCE)
    }

    /// `Sharpening` (`0x0006`).
    #[must_use]
    pub fn sharpening(&self) -> Option<&'a str> {
        text(self.ifd, SHARPENING)
    }

    /// `Focus` (`0x0007`), e.g. `"AF-S"`.
    #[must_use]
    pub fn focus(&self) -> Option<&'a str> {
        text(self.ifd, FOCUS)
    }

    /// `FlashSetting` (`0x0008`).
    #[must_use]
    pub fn flash_setting(&self) -> Option<&'a str> {
        text(self.ifd, FLASH_SETTING)
    }

    /// `FlashDevice` (`0x0009`).
    #[must_use]
    pub fn flash_device(&self) -> Option<&'a str> {
        text(self.ifd, FLASH_DEVICE)
    }

    /// `WhiteBalanceBias` (`0x000b`): the whole `SSHORT` array.
    #[must_use]
    pub fn white_balance_bias(&self) -> Option<&'a [i16]> {
        match self.ifd.get(WHITE_BALANCE_BIAS)? {
            Value::SShort(values) => Some(values),
            _ => None,
        }
    }

    /// `WB_RBLevels` (`0x000c`): the whole `RATIONAL` array, red then blue (then any further
    /// values the body writes).
    #[must_use]
    pub fn wb_rb_levels(&self) -> Option<&'a [(u32, u32)]> {
        rationals(self.ifd, WB_RB_LEVELS)
    }

    /// `ISOSelection` (`0x000f`).
    #[must_use]
    pub fn iso_selection(&self) -> Option<&'a str> {
        text(self.ifd, ISO_SELECTION)
    }

    /// `ExposureBracketComp` (`0x0019`), in EV, the `SRATIONAL` exactly as stored.
    #[must_use]
    pub fn exposure_bracket_comp(&self) -> Option<SRational> {
        match self.ifd.get(EXPOSURE_BRACKET_COMP)? {
            Value::SRational(values) => single(values).map(SRational::from),
            _ => None,
        }
    }

    /// `SerialNumber` (`0x001d`): the body's serial number, one of the two keys of the record
    /// cipher (see [`NikonRecord`]).
    #[must_use]
    pub fn serial_number(&self) -> Option<&'a str> {
        text(self.ifd, SERIAL_NUMBER)
    }

    /// `ColorSpace` (`0x001e`): 1 sRGB, 2 Adobe RGB.
    #[must_use]
    pub fn color_space(&self) -> Option<u16> {
        short(self.ifd, COLOR_SPACE)
    }

    /// `ActiveDLighting` (`0x0022`).
    #[must_use]
    pub fn active_d_lighting(&self) -> Option<u16> {
        short(self.ifd, ACTIVE_D_LIGHTING)
    }

    /// `VignetteControl` (`0x002a`).
    #[must_use]
    pub fn vignette_control(&self) -> Option<u16> {
        short(self.ifd, VIGNETTE_CONTROL)
    }

    /// `ImageAdjustment` (`0x0080`).
    #[must_use]
    pub fn image_adjustment(&self) -> Option<&'a str> {
        text(self.ifd, IMAGE_ADJUSTMENT)
    }

    /// `ToneComp` (`0x0081`).
    #[must_use]
    pub fn tone_comp(&self) -> Option<&'a str> {
        text(self.ifd, TONE_COMP)
    }

    /// `AuxiliaryLens` (`0x0082`).
    #[must_use]
    pub fn auxiliary_lens(&self) -> Option<&'a str> {
        text(self.ifd, AUXILIARY_LENS)
    }

    /// `LensType` (`0x0083`): a bit mask (exiv2 prints `MF`, `D`, `G`, `VR`, … from its bits).
    #[must_use]
    pub fn lens_type(&self) -> Option<u8> {
        byte(self.ifd, LENS_TYPE)
    }

    /// `Lens` (`0x0084`): the whole `RATIONAL` array — shortest and longest focal length, then
    /// the widest aperture at each.
    #[must_use]
    pub fn lens(&self) -> Option<&'a [(u32, u32)]> {
        rationals(self.ifd, LENS)
    }

    /// `FocusDistance` (`0x0085`), the `RATIONAL` exactly as stored.
    #[must_use]
    pub fn focus_distance(&self) -> Option<Rational> {
        rational(self.ifd, FOCUS_DISTANCE)
    }

    /// `DigitalZoom` (`0x0086`), the `RATIONAL` exactly as stored.
    #[must_use]
    pub fn digital_zoom(&self) -> Option<Rational> {
        rational(self.ifd, DIGITAL_ZOOM)
    }

    /// `FlashMode` (`0x0087`).
    #[must_use]
    pub fn flash_mode(&self) -> Option<u8> {
        byte(self.ifd, FLASH_MODE)
    }

    /// `ShootingMode` (`0x0089`): a bit mask.
    #[must_use]
    pub fn shooting_mode(&self) -> Option<u16> {
        short(self.ifd, SHOOTING_MODE)
    }

    /// `SceneMode` (`0x008f`).
    #[must_use]
    pub fn scene_mode(&self) -> Option<&'a str> {
        text(self.ifd, SCENE_MODE)
    }

    /// `LightSource` (`0x0090`).
    #[must_use]
    pub fn light_source(&self) -> Option<&'a str> {
        text(self.ifd, LIGHT_SOURCE)
    }

    /// `ShotInfo` (`0x0091`), opaque: see [`NikonRecord`].
    #[must_use]
    pub fn shot_info(&self) -> Option<NikonRecord<'a>> {
        NikonRecord::read(self.ifd, NikonRecordKind::ShotInfo)
    }

    /// `HueAdjustment` (`0x0092`).
    #[must_use]
    pub fn hue_adjustment(&self) -> Option<i16> {
        match self.ifd.get(HUE_ADJUSTMENT)? {
            Value::SShort(values) => single(values),
            _ => None,
        }
    }

    /// `NEFCompression` (`0x0093`).
    #[must_use]
    pub fn nef_compression(&self) -> Option<u16> {
        short(self.ifd, NEF_COMPRESSION)
    }

    /// `NoiseReduction` (`0x0095`).
    #[must_use]
    pub fn noise_reduction(&self) -> Option<&'a str> {
        text(self.ifd, NOISE_REDUCTION)
    }

    /// `ColorBalance` (`0x0097`), opaque: see [`NikonRecord`].
    #[must_use]
    pub fn color_balance(&self) -> Option<NikonRecord<'a>> {
        NikonRecord::read(self.ifd, NikonRecordKind::ColorBalance)
    }

    /// `LensData` (`0x0098`), opaque: see [`NikonRecord`].
    #[must_use]
    pub fn lens_data(&self) -> Option<NikonRecord<'a>> {
        NikonRecord::read(self.ifd, NikonRecordKind::LensData)
    }

    /// `ImageCount` (`0x00a5`).
    #[must_use]
    pub fn image_count(&self) -> Option<u32> {
        self.ifd.get_u32(IMAGE_COUNT)
    }

    /// `ShutterCount` (`0x00a7`): the body's actuation count, the other key of the record
    /// cipher (see [`NikonRecord`]).
    #[must_use]
    pub fn shutter_count(&self) -> Option<u32> {
        self.ifd.get_u32(SHUTTER_COUNT)
    }

    /// `ImageStabilization` (`0x00ac`).
    #[must_use]
    pub fn image_stabilization(&self) -> Option<&'a str> {
        text(self.ifd, IMAGE_STABILIZATION)
    }

    /// `HighISONoiseReduction` (`0x00b1`).
    #[must_use]
    pub fn high_iso_noise_reduction(&self) -> Option<u16> {
        short(self.ifd, HIGH_ISO_NOISE_REDUCTION)
    }
}

/// The tags of exiv2's `Nikon2` table (`Nikon2MakerNote::tagInfo_[]`), raw as stored. Every
/// accessor returns `None` for an absent tag or one stored in another type; a scalar also for a
/// count other than one.
#[derive(Debug, Clone, Copy)]
pub struct Nikon2Tags<'a> {
    ifd: &'a Ifd,
}

impl Nikon2Tags<'_> {
    /// `Quality` (`0x0003`): Nikon's quality code.
    #[must_use]
    pub fn quality(&self) -> Option<u16> {
        short(self.ifd, N2_QUALITY)
    }

    /// `ColorMode` (`0x0004`): 1 colour, 2 monochrome.
    #[must_use]
    pub fn color_mode(&self) -> Option<u16> {
        short(self.ifd, N2_COLOR_MODE)
    }

    /// `ImageAdjustment` (`0x0005`).
    #[must_use]
    pub fn image_adjustment(&self) -> Option<u16> {
        short(self.ifd, N2_IMAGE_ADJUSTMENT)
    }

    /// `ISOSpeed` (`0x0006`): a code (exiv2 prints 0 as ISO 80, 2 as 160, 4 as 320, 5 as 100).
    #[must_use]
    pub fn iso_speed(&self) -> Option<u16> {
        short(self.ifd, N2_ISO_SPEED)
    }

    /// `WhiteBalance` (`0x0007`).
    #[must_use]
    pub fn white_balance(&self) -> Option<u16> {
        short(self.ifd, N2_WHITE_BALANCE)
    }

    /// `Focus` (`0x0008`), the `RATIONAL` exactly as stored.
    #[must_use]
    pub fn focus(&self) -> Option<Rational> {
        rational(self.ifd, N2_FOCUS)
    }

    /// `DigitalZoom` (`0x000a`), the `RATIONAL` exactly as stored (0 when unused).
    #[must_use]
    pub fn digital_zoom(&self) -> Option<Rational> {
        rational(self.ifd, N2_DIGITAL_ZOOM)
    }

    /// `AuxiliaryLens` (`0x000b`).
    #[must_use]
    pub fn auxiliary_lens(&self) -> Option<u16> {
        short(self.ifd, N2_AUXILIARY_LENS)
    }
}

/// `tag` of `ifd` as a whole `UNDEFINED` value, or `None` if absent or of another type.
fn undefined(ifd: &Ifd, tag: u16) -> Option<&[u8]> {
    match ifd.get(tag)? {
        Value::Undefined(values) => Some(values),
        _ => None,
    }
}

/// `tag` of `ifd` as a whole `SHORT` array, or `None` if absent or of another type.
fn shorts(ifd: &Ifd, tag: u16) -> Option<&[u16]> {
    match ifd.get(tag)? {
        Value::Short(values) => Some(values),
        _ => None,
    }
}

/// `tag` of `ifd` as a single `SHORT`, or `None` if absent, of another type, or not one value.
fn short(ifd: &Ifd, tag: u16) -> Option<u16> {
    single(shorts(ifd, tag)?)
}

/// `tag` of `ifd` as a single `BYTE`, or `None` if absent, of another type, or not one value.
fn byte(ifd: &Ifd, tag: u16) -> Option<u8> {
    match ifd.get(tag)? {
        Value::Byte(values) => single(values),
        _ => None,
    }
}

/// `tag` of `ifd` as a whole `RATIONAL` array, or `None` if absent or of another type.
fn rationals(ifd: &Ifd, tag: u16) -> Option<&[(u32, u32)]> {
    match ifd.get(tag)? {
        Value::Rational(values) => Some(values),
        _ => None,
    }
}

/// `tag` of `ifd` as a single `RATIONAL`, or `None` if absent, of another type, or not one value.
fn rational(ifd: &Ifd, tag: u16) -> Option<Rational> {
    single(rationals(ifd, tag)?).map(Rational::from)
}

#[cfg(test)]
mod tests {
    use gamut_ifd::ByteOrder::{BigEndian as BE, LittleEndian as LE};
    use gamut_ifd::FieldType;

    use super::super::MakerNoteVendor;
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

    fn shorts(tag: u16, values: &[u16], order: ByteOrder) -> Entry {
        let bytes = values.iter().flat_map(|&v| order.pack_u16(v)).collect();
        entry(tag, FieldType::Short, values.len(), bytes)
    }

    fn sshorts(tag: u16, values: &[i16], order: ByteOrder) -> Entry {
        let bytes = values
            .iter()
            .flat_map(|&v| order.pack_u16(v as u16))
            .collect();
        entry(tag, FieldType::SShort, values.len(), bytes)
    }

    fn longs(tag: u16, values: &[u32], order: ByteOrder) -> Entry {
        let bytes = values.iter().flat_map(|&v| order.pack_u32(v)).collect();
        entry(tag, FieldType::Long, values.len(), bytes)
    }

    fn rationals(tag: u16, values: &[(u32, u32)], order: ByteOrder) -> Entry {
        let bytes = values
            .iter()
            .flat_map(|&(n, d)| [order.pack_u32(n), order.pack_u32(d)])
            .flatten()
            .collect();
        entry(tag, FieldType::Rational, values.len(), bytes)
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

    fn undefined(tag: u16, values: &[u8]) -> Entry {
        entry(tag, FieldType::Undefined, values.len(), values.to_vec())
    }

    fn ascii(tag: u16, text: &[u8]) -> Entry {
        entry(tag, FieldType::Ascii, text.len(), text.to_vec())
    }

    /// The IFD, a zero next-IFD pointer, then each out-of-line value, addressed as if the IFD
    /// stood at `at` in the offset space its offsets count in.
    fn ifd(entries: &[Entry], order: ByteOrder, at: u32) -> Vec<u8> {
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
                ifd.extend_from_slice(&order.pack_u32(data_at));
                data.extend_from_slice(&entry.bytes);
                data_at += entry.bytes.len() as u32;
            }
        }
        ifd.extend_from_slice(&[0; 4]);
        ifd.extend_from_slice(&data);
        ifd
    }

    /// A `Nikon3` note in `order`: the 10-byte signature, an embedded TIFF header naming that
    /// order and an IFD at its byte 8, then the IFD with offsets counted from that header.
    fn nikon3_note(entries: &[Entry], order: ByteOrder) -> Vec<u8> {
        let mut note = b"Nikon\0\x02\x10\0\0".to_vec();
        note.extend_from_slice(match order {
            LE => b"II*\0\x08\0\0\0",
            BE => b"MM\0*\0\0\0\x08",
        });
        note.extend_from_slice(&ifd(entries, order, 8));
        note
    }

    /// A `Nikon2` note: the 8-byte signature, then the IFD with offsets counted from the TIFF
    /// header, the note standing at `note_at`.
    fn nikon2_note(entries: &[Entry], order: ByteOrder, note_at: u32) -> Vec<u8> {
        let mut note = b"Nikon\0\x01\0".to_vec();
        note.extend_from_slice(&ifd(entries, order, note_at + 8));
        note
    }

    fn maker(bytes: Vec<u8>) -> MakerNote {
        MakerNote {
            vendor: MakerNoteVendor::Nikon,
            bytes,
        }
    }

    /// One entry per `Nikon3Tags` accessor, each holding a value no other accessor's tag holds,
    /// so reading the wrong tag is always visible.
    fn every_nikon3_tag(order: ByteOrder) -> Vec<Entry> {
        vec![
            undefined(VERSION, b"0211"),
            shorts(ISO_SPEED, &[0, 400], order),
            ascii(COLOR_MODE, b"COLOR\0"),
            ascii(QUALITY, b"FINE   \0"),
            ascii(WHITE_BALANCE, b"AUTO1\0"),
            ascii(SHARPENING, b"NORMAL\0"),
            ascii(FOCUS, b"AF-S  \0"),
            ascii(FLASH_SETTING, b"NORMAL-2\0"),
            ascii(FLASH_DEVICE, b"SB-800\0"),
            sshorts(WHITE_BALANCE_BIAS, &[-3, 0], order),
            rationals(
                WB_RB_LEVELS,
                &[(512, 256), (397, 256), (1, 1), (1, 1)],
                order,
            ),
            ascii(ISO_SELECTION, b"MANUAL\0"),
            srationals(EXPOSURE_BRACKET_COMP, &[(-1, 3)], order),
            ascii(SERIAL_NUMBER, b"3001234\0"),
            shorts(COLOR_SPACE, &[2], order),
            shorts(ACTIVE_D_LIGHTING, &[3], order),
            shorts(VIGNETTE_CONTROL, &[5], order),
            ascii(IMAGE_ADJUSTMENT, b"AUTO\0"),
            ascii(TONE_COMP, b"HIGH\0"),
            ascii(AUXILIARY_LENS, b"OFF\0"),
            bytes(LENS_TYPE, &[14]),
            rationals(LENS, &[(180, 10), (550, 10), (35, 10), (56, 10)], order),
            rationals(FOCUS_DISTANCE, &[(7, 2)], order),
            rationals(DIGITAL_ZOOM, &[(9, 4)], order),
            bytes(FLASH_MODE, &[6]),
            shorts(SHOOTING_MODE, &[16], order),
            ascii(SCENE_MODE, b"PORTRAIT\0"),
            ascii(LIGHT_SOURCE, b"NATURAL\0"),
            undefined(SHOT_INFO, b"0221 enciphered"),
            sshorts(HUE_ADJUSTMENT, &[-9], order),
            shorts(NEF_COMPRESSION, &[4], order),
            ascii(NOISE_REDUCTION, b"FPNR\0"),
            undefined(COLOR_BALANCE, b"0100 levels"),
            undefined(LENS_DATA, b"0204"),
            longs(IMAGE_COUNT, &[1_234], order),
            longs(SHUTTER_COUNT, &[56_789], order),
            ascii(IMAGE_STABILIZATION, b"VR-ON\0"),
            shorts(HIGH_ISO_NOISE_REDUCTION, &[6], order),
        ]
    }

    #[test]
    fn every_nikon3_accessor_reads_its_own_tag() {
        for order in [LE, BE] {
            let note = maker(nikon3_note(&every_nikon3_tag(order), order));
            // The embedded header decides the order, so the outer stream's never matters.
            let view = note.nikon(BE, 300).expect("reads").expect("is Nikon");
            assert_eq!(view.dialect(), MakerNoteDialect::Nikon3);
            let tags = view.nikon3().expect("Nikon3 tags");
            assert_eq!(tags.version(), Some(&b"0211"[..]));
            assert_eq!(tags.iso_speed(), Some(&[0, 400][..]));
            assert_eq!(tags.color_mode(), Some("COLOR"));
            assert_eq!(tags.quality(), Some("FINE   "));
            assert_eq!(tags.white_balance(), Some("AUTO1"));
            assert_eq!(tags.sharpening(), Some("NORMAL"));
            assert_eq!(tags.focus(), Some("AF-S  "));
            assert_eq!(tags.flash_setting(), Some("NORMAL-2"));
            assert_eq!(tags.flash_device(), Some("SB-800"));
            assert_eq!(tags.white_balance_bias(), Some(&[-3, 0][..]));
            assert_eq!(
                tags.wb_rb_levels(),
                Some(&[(512, 256), (397, 256), (1, 1), (1, 1)][..])
            );
            assert_eq!(tags.iso_selection(), Some("MANUAL"));
            assert_eq!(tags.exposure_bracket_comp(), Some(SRational::from((-1, 3))));
            assert_eq!(tags.serial_number(), Some("3001234"));
            assert_eq!(tags.color_space(), Some(2));
            assert_eq!(tags.active_d_lighting(), Some(3));
            assert_eq!(tags.vignette_control(), Some(5));
            assert_eq!(tags.image_adjustment(), Some("AUTO"));
            assert_eq!(tags.tone_comp(), Some("HIGH"));
            assert_eq!(tags.auxiliary_lens(), Some("OFF"));
            assert_eq!(tags.lens_type(), Some(14));
            assert_eq!(
                tags.lens(),
                Some(&[(180, 10), (550, 10), (35, 10), (56, 10)][..])
            );
            assert_eq!(tags.focus_distance(), Some(Rational::from((7, 2))));
            assert_eq!(tags.digital_zoom(), Some(Rational::from((9, 4))));
            assert_eq!(tags.flash_mode(), Some(6));
            assert_eq!(tags.shooting_mode(), Some(16));
            assert_eq!(tags.scene_mode(), Some("PORTRAIT"));
            assert_eq!(tags.light_source(), Some("NATURAL"));
            assert_eq!(tags.hue_adjustment(), Some(-9));
            assert_eq!(tags.nef_compression(), Some(4));
            assert_eq!(tags.noise_reduction(), Some("FPNR"));
            assert_eq!(tags.image_count(), Some(1_234));
            assert_eq!(tags.shutter_count(), Some(56_789));
            assert_eq!(tags.image_stabilization(), Some("VR-ON"));
            assert_eq!(tags.high_iso_noise_reduction(), Some(6));
            assert_eq!(view.ifd().fields().len(), 38);
            assert!(view.unreadable_tags().is_empty());
            assert!(view.nikon2().is_none());
        }
    }

    #[test]
    fn each_record_names_itself_its_version_and_its_verbatim_bytes() {
        let order = LE;
        let note = maker(nikon3_note(&every_nikon3_tag(order), order));
        let view = note.nikon(order, 0).expect("reads").expect("is Nikon");
        let tags = view.nikon3().expect("Nikon3 tags");
        let shot = tags.shot_info().expect("ShotInfo");
        assert_eq!(shot.kind(), NikonRecordKind::ShotInfo);
        assert_eq!(shot.version(), *b"0221");
        assert_eq!(shot.bytes(), b"0221 enciphered");
        let balance = tags.color_balance().expect("ColorBalance");
        assert_eq!(balance.kind(), NikonRecordKind::ColorBalance);
        assert_eq!(balance.version(), *b"0100");
        assert_eq!(balance.bytes(), b"0100 levels");
        // Exactly the four version bytes is still a record, with nothing after them.
        let lens = tags.lens_data().expect("LensData");
        assert_eq!(lens.kind(), NikonRecordKind::LensData);
        assert_eq!(lens.version(), *b"0204");
        assert_eq!(lens.bytes(), b"0204");
    }

    /// Each typed accessor shape refuses the entry under another type, and the raw value stays
    /// reachable through `ifd()`.
    #[test]
    fn a_retyped_tag_reads_as_none() {
        let order = LE;
        let note = maker(nikon3_note(
            &[
                bytes(VERSION, b"0211"),
                longs(ISO_SPEED, &[0, 400], order),
                shorts(WHITE_BALANCE_BIAS, &[3], order),
                srationals(WB_RB_LEVELS, &[(1, 2)], order),
                rationals(EXPOSURE_BRACKET_COMP, &[(1, 3)], order),
                longs(COLOR_SPACE, &[1], order),
                shorts(LENS_TYPE, &[1], order),
                shorts(HUE_ADJUSTMENT, &[1], order),
                srationals(FOCUS_DISTANCE, &[(1, 1)], order),
                undefined(SCENE_MODE, b"PORTRAIT"),
                sshorts(SHUTTER_COUNT, &[1], order),
            ],
            order,
        ));
        let view = note.nikon(order, 0).expect("reads").expect("is Nikon");
        let tags = view.nikon3().expect("Nikon3 tags");
        assert_eq!(tags.version(), None);
        assert_eq!(tags.iso_speed(), None);
        assert_eq!(tags.white_balance_bias(), None);
        assert_eq!(tags.wb_rb_levels(), None);
        assert_eq!(tags.exposure_bracket_comp(), None);
        assert_eq!(tags.color_space(), None);
        assert_eq!(tags.lens_type(), None);
        assert_eq!(tags.hue_adjustment(), None);
        assert_eq!(tags.focus_distance(), None);
        assert_eq!(tags.scene_mode(), None);
        assert_eq!(tags.shutter_count(), None);
        assert_eq!(view.ifd().get(COLOR_SPACE), Some(&Value::Long(vec![1])));
    }

    #[test]
    fn an_absent_tag_reads_as_none() {
        let order = BE;
        let note = maker(nikon3_note(&[longs(IMAGE_COUNT, &[1], order)], order));
        let view = note.nikon(order, 0).expect("reads").expect("is Nikon");
        let tags = view.nikon3().expect("Nikon3 tags");
        assert_eq!(tags.version(), None);
        assert_eq!(tags.iso_speed(), None);
        assert_eq!(tags.white_balance_bias(), None);
        assert_eq!(tags.lens(), None);
        assert_eq!(tags.exposure_bracket_comp(), None);
        assert_eq!(tags.nef_compression(), None);
        assert_eq!(tags.flash_mode(), None);
        assert_eq!(tags.hue_adjustment(), None);
        assert_eq!(tags.digital_zoom(), None);
        assert_eq!(tags.quality(), None);
    }

    /// A scalar accessor takes exactly one value; exiv2 would print them all.
    #[test]
    fn a_scalar_with_more_than_one_value_reads_as_none() {
        let order = LE;
        let note = maker(nikon3_note(
            &[
                srationals(EXPOSURE_BRACKET_COMP, &[(1, 3), (2, 3)], order),
                shorts(COLOR_SPACE, &[1, 2], order),
                bytes(LENS_TYPE, &[1, 2]),
                sshorts(HUE_ADJUSTMENT, &[1, 2], order),
                rationals(DIGITAL_ZOOM, &[(1, 1), (2, 1)], order),
            ],
            order,
        ));
        let view = note.nikon(order, 0).expect("reads").expect("is Nikon");
        let tags = view.nikon3().expect("Nikon3 tags");
        assert_eq!(tags.exposure_bracket_comp(), None);
        assert_eq!(tags.color_space(), None);
        assert_eq!(tags.lens_type(), None);
        assert_eq!(tags.hue_adjustment(), None);
        assert_eq!(tags.digital_zoom(), None);
    }

    fn every_nikon2_tag(order: ByteOrder) -> Vec<Entry> {
        vec![
            shorts(N2_QUALITY, &[2], order),
            shorts(N2_COLOR_MODE, &[1], order),
            shorts(N2_IMAGE_ADJUSTMENT, &[3], order),
            shorts(N2_ISO_SPEED, &[5], order),
            shorts(N2_WHITE_BALANCE, &[6], order),
            rationals(N2_FOCUS, &[(11, 10)], order),
            rationals(N2_DIGITAL_ZOOM, &[(15, 10)], order),
            shorts(N2_AUXILIARY_LENS, &[4], order),
        ]
    }

    #[test]
    fn every_nikon2_accessor_reads_its_own_tag() {
        for order in [LE, BE] {
            let note = maker(nikon2_note(&every_nikon2_tag(order), order, 200));
            let view = note.nikon(order, 200).expect("reads").expect("is Nikon");
            assert_eq!(view.dialect(), MakerNoteDialect::Nikon2);
            let tags = view.nikon2().expect("Nikon2 tags");
            assert_eq!(tags.quality(), Some(2));
            assert_eq!(tags.color_mode(), Some(1));
            assert_eq!(tags.image_adjustment(), Some(3));
            assert_eq!(tags.iso_speed(), Some(5));
            assert_eq!(tags.white_balance(), Some(6));
            assert_eq!(tags.focus(), Some(Rational::from((11, 10))));
            assert_eq!(tags.digital_zoom(), Some(Rational::from((15, 10))));
            assert_eq!(tags.auxiliary_lens(), Some(4));
            assert_eq!(view.ifd().fields().len(), 8);
            assert!(view.unreadable_tags().is_empty());
            assert!(view.nikon3().is_none());
        }
    }

    /// `Nikon1` shares `Nikon3`'s numbering, so its tags read through `Nikon3Tags`.
    #[test]
    fn a_nikon1_note_reads_through_nikon3_tags() {
        let order = BE;
        let entries = [
            ascii(QUALITY, b"FINE\0"),
            rationals(FOCUS_DISTANCE, &[(0, 1)], order),
        ];
        let note = maker(ifd(&entries, order, 64));
        let view = note.nikon(order, 64).expect("reads").expect("is Nikon");
        assert_eq!(view.dialect(), MakerNoteDialect::Nikon1);
        let tags = view.nikon3().expect("Nikon3 tags");
        assert_eq!(tags.quality(), Some("FINE"));
        assert_eq!(tags.focus_distance(), Some(Rational::from((0, 1))));
        assert!(view.nikon2().is_none());
    }

    /// `Nikon3` offsets count from the embedded header, so the note reads the same wherever it
    /// stands, from its own bytes or through the stream around it.
    #[test]
    fn nikon3_offsets_count_from_the_embedded_header() {
        let order = BE;
        let bytes = nikon3_note(&every_nikon3_tag(order), order);
        let here = maker(bytes.clone()).nikon(LE, 0).expect("reads");
        assert_eq!(here, maker(bytes.clone()).nikon(LE, 4_096).expect("reads"));
        let mut stream = vec![0; 300];
        stream.extend_from_slice(&bytes);
        let layout = maker(bytes).layout(LE).expect("Nikon layout");
        let read = NikonMakerNote::read(&stream[..], 300, layout).expect("reads");
        assert_eq!(read, here);
    }

    /// `Nikon2` (and `Nikon1`) offsets count from the TIFF header: placed wrongly, an out-of-line
    /// value points outside the note.
    #[test]
    fn nikon2_offsets_count_from_the_tiff_header() {
        let order = LE;
        let bytes = nikon2_note(&every_nikon2_tag(order), order, 200);
        let view = maker(bytes.clone())
            .nikon(order, 0)
            .expect("reads")
            .expect("is Nikon");
        let tags = view.nikon2().expect("Nikon2 tags");
        assert_eq!(tags.focus(), None);
        assert_eq!(view.unreadable_tags(), &[N2_FOCUS, N2_DIGITAL_ZOOM]);
        // An inline value has no offset and reads wherever the note stands.
        assert_eq!(tags.quality(), Some(2));
        let mut stream = vec![0; 200];
        stream.extend_from_slice(&bytes);
        let layout = maker(bytes.clone()).layout(order).expect("Nikon layout");
        let read = NikonMakerNote::read(&stream[..], 200, layout).expect("reads");
        assert_eq!(read, maker(bytes).nikon(order, 200).expect("reads"));
    }

    #[test]
    fn only_a_nikon_layout_is_read() {
        let order = LE;
        let canon = MakerNote {
            vendor: MakerNoteVendor::Canon,
            bytes: nikon3_note(&[longs(IMAGE_COUNT, &[1], order)], order),
        };
        assert_eq!(canon.nikon(order, 0).expect("not an error"), None);
    }
}
