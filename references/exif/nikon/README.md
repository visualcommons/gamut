# Nikon MakerNote (not a specification; issue #979)

Nikon has never published the layout of its `MakerNote` (`0x927C`) payload. This directory stakes
what `gamut-exif`'s Nikon view (`gamut_exif::NikonMakerNote`) is held to, before any tag is
decoded, as the parent table in
[`../README.md`](../README.md#makernote-dialects-not-a-specification-issue-418) asks of every
vendor.

## Sources

- **exiv2** — `third_party/exiv2/src/nikonmn_int.cpp` at the pinned submodule commit
  `2cd987a731236037b6b78cbff897d08685a8ef49`: the tables `Nikon1MakerNote::tagInfo_[]` (from line
  112), `Nikon2MakerNote::tagInfo_[]` (from line 314) and `Nikon3MakerNote::tagInfo_[]` (from line
  361) name each tag of the note's IFD and the type exiv2 expects. `third_party/exiv2/src/tags_int.cpp`
  lines 92–94 name the groups `Nikon1`, `Nikon2` and `Nikon3`, so the keys are `Exif.Nikon1.*`,
  `Exif.Nikon2.*` and `Exif.Nikon3.*`. The same exiv2 is what `tooling/exiv2-oracle` builds, so it
  is both the written source and the differential oracle: its `exif_get` reads any of those keys.
- **exiv2's dispatch** — `third_party/exiv2/src/makernote_int.cpp`: line 114 registers `NIKON`
  with `newNikonMn` (line 712), which picks `Nikon1` for a note not opening `Nikon\0` (a bare IFD
  at byte 0), `Nikon2` for a `Nikon\0` note with no classic TIFF header at byte 10
  (`Nikon2MnHeader`, lines 326–358: the IFD at byte 8), and `Nikon3` for one with it
  (`Nikon3MnHeader`, lines 360–415: the IFD at `10 +` the header's first-IFD offset, in the
  header's byte order, value offsets counted from note byte 10, `baseOffset` at line 384). `Nikon1`
  and `Nikon2` inherit the outer byte order and count value offsets from the outer TIFF header.
  These are the three Nikon rows of the parent table, which `MakerNoteLayout::detect` resolves.
  exiv2 reads each dialect's next-IFD pointer and does not follow it (`tiffimage_int.cpp` lines
  1719, 1723 and 1727, `ignoreTiffComponent` for `Tag::next`).
- **exiv2's records** — `third_party/exiv2/src/tiffimage_int.cpp` lines 1734–1736 split the
  `Nikon3` tags `ShotInfo` (`0x0091`), `ColorBalance` (`0x0097`) and `LensData` (`0x0098`) into
  binary arrays, choosing the layout by the record's four-byte version through `nikonSelector` and
  the `nikonArrayIdx` table (`makernote_int.cpp` lines 852–924). For most versions the bytes after a
  given start are enciphered: `nikonCrypt` (lines 926–965) deciphers them with `ncrypt` (from line
  1069), keyed by the note's `SerialNumber` (`0x001d`) and `ShutterCount` (`0x00a7`).
- **ExifTool** — `lib/Image/ExifTool/Nikon.pm` (not vendored; the tag tables are published at
  <https://exiftool.org/TagNames/Nikon.html>). Used only to cross-read a name or meaning where
  exiv2's table is silent; no tag here depends on it alone.

## What the view decodes

The view reads all three dialects. It keeps every entry of the note's IFD as a `gamut_ifd::Value`
(`NikonMakerNote::ifd`), so any tag is reachable by number, and names the dialect it read
(`NikonMakerNote::dialect`). The typed accessors come in two sets, one per numbering:

- **`Nikon3Tags`** (`NikonMakerNote::nikon3`) for a `Nikon3` note and for a `Nikon1` note: every
  tag of exiv2's `Nikon1` table that the view types (`Version`, `ISOSpeed`, `ColorMode`,
  `Quality`, `WhiteBalance`, `Sharpening`, `Focus`, `FlashSetting`, `ISOSelection`,
  `ImageAdjustment`, `AuxiliaryLens`, `FocusDistance`, `DigitalZoom`) has the same number, name and
  type in the `Nikon3` table, so one set serves both; a `Nikon1` note simply carries none of the
  others.
- **`Nikon2Tags`** (`NikonMakerNote::nikon2`) for a `Nikon2` note (the E700/E800/E900-era Coolpix),
  whose table numbers its tags differently.

Each accessor is checked against exiv2 tag by tag in `crates/gamut-exif/tests/oracle_nikon.rs`.

### `Nikon3Tags`

| Tag | exiv2 key (`Exif.Nikon3.` +) | exiv2 type (`nikonmn_int.cpp`) | Accessor |
| --- | --- | --- | --- |
| `0x0001` | `Version` | `undefined` (line 362) | `version` |
| `0x0002` | `ISOSpeed` | `unsignedShort` (line 364) | `iso_speed` |
| `0x0003` | `ColorMode` | `asciiString` (line 366) | `color_mode` |
| `0x0004` | `Quality` | `asciiString` (line 368) | `quality` |
| `0x0005` | `WhiteBalance` | `asciiString` (line 370) | `white_balance` |
| `0x0006` | `Sharpening` | `asciiString` (line 372) | `sharpening` |
| `0x0007` | `Focus` | `asciiString` (line 374) | `focus` |
| `0x0008` | `FlashSetting` | `asciiString` (line 376) | `flash_setting` |
| `0x0009` | `FlashDevice` | `asciiString` (line 378) | `flash_device` |
| `0x000b` | `WhiteBalanceBias` | `signedShort` (line 382) | `white_balance_bias` |
| `0x000c` | `WB_RBLevels` | `unsignedRational` (line 384) | `wb_rb_levels` |
| `0x000f` | `ISOSelection` | `asciiString` (line 390) | `iso_selection` |
| `0x0019` | `ExposureBracketComp` | `signedRational` (line 406) | `exposure_bracket_comp` |
| `0x001d` | `SerialNumber` | `asciiString` (line 414) | `serial_number` |
| `0x001e` | `ColorSpace` | `unsignedShort` (line 416) | `color_space` |
| `0x0022` | `ActiveDLighting` | `unsignedShort` (line 421) | `active_d_lighting` |
| `0x002a` | `VignetteControl` | `unsignedShort` (line 429) | `vignette_control` |
| `0x0080` | `ImageAdjustment` | `asciiString` (line 437) | `image_adjustment` |
| `0x0081` | `ToneComp` | `asciiString` (line 439) | `tone_comp` |
| `0x0082` | `AuxiliaryLens` | `asciiString` (line 441) | `auxiliary_lens` |
| `0x0083` | `LensType` | `unsignedByte` (line 443) | `lens_type` |
| `0x0084` | `Lens` | `unsignedRational` (line 445) | `lens` |
| `0x0085` | `FocusDistance` | `unsignedRational` (line 447) | `focus_distance` |
| `0x0086` | `DigitalZoom` | `unsignedRational` (line 449) | `digital_zoom` |
| `0x0087` | `FlashMode` | `unsignedByte` (line 451) | `flash_mode` |
| `0x0089` | `ShootingMode` | `unsignedShort` (line 454) | `shooting_mode` |
| `0x008f` | `SceneMode` | `asciiString` (line 464) | `scene_mode` |
| `0x0090` | `LightSource` | `asciiString` (line 466) | `light_source` |
| `0x0091` | `ShotInfo` | `undefined` (line 468) | `shot_info` (a `NikonRecord`) |
| `0x0092` | `HueAdjustment` | `signedShort` (line 470) | `hue_adjustment` |
| `0x0093` | `NEFCompression` | `unsignedShort` (line 472) | `nef_compression` |
| `0x0095` | `NoiseReduction` | `asciiString` (line 476) | `noise_reduction` |
| `0x0097` | `ColorBalance` | `undefined` (line 480) | `color_balance` (a `NikonRecord`) |
| `0x0098` | `LensData` | `undefined` (line 482) | `lens_data` (a `NikonRecord`) |
| `0x00a5` | `ImageCount` | `unsignedLong` (line 499) | `image_count` |
| `0x00a7` | `ShutterCount` | `unsignedLong` (line 503) | `shutter_count` |
| `0x00ac` | `ImageStabilization` | `asciiString` (line 513) | `image_stabilization` |
| `0x00b1` | `HighISONoiseReduction` | `unsignedShort` (line 519) | `high_iso_noise_reduction` |

### `Nikon2Tags`

| Tag | exiv2 key (`Exif.Nikon2.` +) | exiv2 type (`nikonmn_int.cpp`) | Accessor |
| --- | --- | --- | --- |
| `0x0003` | `Quality` | `unsignedShort` (line 316) | `quality` |
| `0x0004` | `ColorMode` | `unsignedShort` (line 318) | `color_mode` |
| `0x0005` | `ImageAdjustment` | `unsignedShort` (line 320) | `image_adjustment` |
| `0x0006` | `ISOSpeed` | `unsignedShort` (line 322) | `iso_speed` |
| `0x0007` | `WhiteBalance` | `unsignedShort` (line 324) | `white_balance` |
| `0x0008` | `Focus` | `unsignedRational` (line 326) | `focus` |
| `0x000a` | `DigitalZoom` | `unsignedRational` (line 329) | `digital_zoom` |
| `0x000b` | `AuxiliaryLens` | `unsignedShort` (line 331) | `auxiliary_lens` |

Notes on reading them:

- **Types.** A `LONG` scalar accessor (`image_count`, `shutter_count`) accepts any unsigned
  integer type with a count of one, as `Ifd::get_u32` does; every other accessor returns its value
  only when the entry has exiv2's type (and, for a scalar, a count of one). Anything else reads as
  `None` and stays in `ifd()`. Rationals are the raw numerator/denominator pairs, not floats.
- **Arrays** (`Version`, `ISOSpeed`, `WhiteBalanceBias`, `WB_RBLevels`, `Lens`) come back whole;
  exiv2 types them with count `-1`, so the view does not enforce a count either.
- **Text** fields return the text before the first NUL, which is what exiv2 reports; the raw value
  with any padding stays in `ifd()`. As for Canon (`../canon/README.md`), a text field holding any
  non-UTF-8 byte is unreadable through `gamut-ifd` and its accessor returns `None` where exiv2
  still reports the prefix.
- **Offsets.** A `Nikon3` note's are note-relative (from its embedded TIFF header), so
  `MakerNote::nikon` reads it completely from the note's own bytes. A `Nikon1` or `Nikon2` note's
  are file-absolute: a view built from the note's bytes alone can read only values that lie inside
  the note itself, while `NikonMakerNote::read` reads through the whole TIFF stream, as exiv2 does.

## The enciphered records (decision)

`ShotInfo`, `ColorBalance` and `LensData` are `UNDEFINED` records whose first four bytes are an
ASCII version (`0100`, `0204`, …). Which layout the rest follows, and whether it is enciphered, is
gated on that version (and, for `ShotInfo` `0210`, on the record's length). exiv2's
`nikonArrayIdx` enciphers, from the byte it names, every record of these versions:

| Record | Enciphered versions (from byte) | Plain versions |
| --- | --- | --- |
| `ShotInfo` | `02*` (4) | `01*` |
| `ColorBalance` | `0205` (4); `0209`, `0212`, `0214` and other `02*` (284) | `0100`, `0102`, `0103` |
| `LensData` | `0201`–`0204`, `0800`–`0802` (4) | `0100`, `0101` |

**Decision (issue #979): the view keeps these records opaque.** Each accessor (`shot_info`,
`color_balance`, `lens_data`) returns a `NikonRecord` — a typed marker naming the record
(`NikonRecordKind`), its four version bytes, and the whole value verbatim, version included —
and deciphers nothing. Deciphering is only useful together with the per-version field layouts
(exiv2 carries sixteen, ExifTool more), which is a separate body of work: issue #1283. A value shorter than its four version bytes is no record:
the accessor returns `None`, as exiv2's `nikonSelector` refuses it, and the bytes stay in `ifd()`.

The record always re-emits byte-identically when unmodified: the view never writes, and the
`Exif` model keeps the note's bytes verbatim (`tests/oracle_nikon.rs` checks a stream carrying
enciphered records re-emits byte for byte).

The oracle test checks each record's version against exiv2's own reading of the same record:
`Exif.NikonSi02xx.Version`, `Exif.NikonCb1.Version` and `Exif.NikonLd3.Version` for the versions
it writes (`0221`, `0100`, `0204`), which exiv2 reaches only after selecting, and for the
enciphered ones deciphering, the record.

Not decoded, and not promised: the interpretation of any value (exiv2's `exif_print` names
`LensType` bits and `ColorSpace` codes; the view returns the numbers), the record contents and
their deciphering, the other binary arrays exiv2 splits (`VRInfo` `0x001f`, `PictureControl`
`0x0023`, `WorldTime` `0x0024`, `ISOInfo` `0x0025`, `AFInfo` `0x0088`, `FlashInfo` `0x00a8`,
`MultiExposure` `0x00b0`, `AFInfo2` `0x00b7`, `FileInfo` `0x00b8`, `AFTune` `0x00b9`; reachable as
raw bytes through `ifd()`), the `Preview` subdirectory (`0x0011`), and the Nikon Capture tags
(`0x0e00`–`0x0e1e`).
