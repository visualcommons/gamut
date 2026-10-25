# Leica MakerNote (not a specification; issue #978)

Leica has never published the layout of its `MakerNote` (`0x927C`) payload, and exiv2 has no
Leica module (`TiffMnCreator::registry_` in `third_party/exiv2/src/makernote_int.cpp` has no Leica
row), so `tooling/exiv2-oracle` cannot serve as the oracle. This directory stakes what
`gamut-exif`'s Leica dialects (`MakerNoteLayout::detect`) and its Leica view
(`gamut_exif::maker_note::leica`) are held to, as the parent table in
[`../README.md`](../README.md#makernote-dialects-not-a-specification-issue-418) asks of every
vendor.

## Sources

- **ExifTool 13.59**, commit `2200871d9cef988051d2a99d67df3bda6cbb30a8` of
  <https://github.com/exiftool/exiftool>. Not vendored: ExifTool is distributed under the Perl
  Artistic License or the GPL, and only the facts below (signatures, offsets, tag numbers, types)
  are needed, so they are transcribed rather than copied. The two files read, with the SHA-256 of
  each at that commit:
  - `lib/Image/ExifTool/MakerNotes.pm`
    (`5c78d72ce8f07142ce4e9e400616725480495c3c85514b2861cadd18077c119c`): the `MakerNoteLeica`
    … `MakerNoteLeica10` entries (lines 599–731), which ExifTool tries after `MakerNoteFujiFilm` (line 121), which decide the dialect, and `FixLeicaBase`
    (line 1669).
  - `lib/Image/ExifTool/Panasonic.pm`
    (`b99cedc10429748d75330d85417b4993907204ab19da0f0b5f9110403e663be7`), which holds ExifTool's
    Leica tag tables: `Leica4` (line 1736) with its `Subdir` table (line 1773), and `Leica9`
    (line 2193).
- **The corpus**: the two Leica notes in `third_party/gamut-dng-samples` (CC0-1.0), extracted
  byte for byte into `crates/gamut-exif/tests/fixtures/`:

  | Fixture | Source file (corpus SHA-256) | Note at (TIFF offset) | Length | ExifTool entry |
  | --- | --- | --- | --- | --- |
  | `leica_m10_maker_note.bin` | `leica/m10/f5381888.dng` (`26be5e06…f95e`) | 6144 | 4096 | `MakerNoteLeica9` |
  | `leica_m_monochrom_maker_note.bin` | `leica/m-monochrom/L1000622.DNG` (`149d5b3f…0e4f`) | 1152 | 4934 | `MakerNoteLeica4` |

  Both DNGs are TIFF files whose header is at file offset 0, so the note's file offset and its
  TIFF offset are the same number. Re-extract with
  `dd if=FILE of=FIXTURE bs=1 skip=AT count=LENGTH`.
- **ExifTool's reading of those two files**, committed beside this file as
  `exiftool-13.59-m10-f5381888.txt` and `exiftool-13.59-m-monochrom-L1000622.txt`. Regenerate
  each with `exiftool -a -G1 -H -n -s -MakerNotes:all FILE`. `-n` turns off print conversion but
  not ExifTool's value conversion, so `LensType`, `MeasuredLV`, `FNumber` and the like appear
  there scaled. The raw stored values the view returns are the ones `exiftool -v3 FILE` prints,
  and the table below lists them.

## Dialects

ExifTool tries its `MakerNoteLeica*` entries in order, and each one decides on `Make`, sometimes
`Model`, and the note's leading bytes. `MakerNoteLayout::detect` sees the vendor (any `Make`
containing `LEICA`, case-insensitively) and the note's bytes, so it resolves the entries the
leading bytes decide, and leaves the rest without a layout:

| Dialect | ExifTool entry (bodies) | Signature | IFD at | Offsets count from | Tag table |
| --- | --- | --- | --- | --- | --- |
| `Fujifilm` | `MakerNoteFujiFilm` (Fujifilm-built bodies) | `FUJIFILM` | as Fujifilm | as Fujifilm | Fujifilm |
| `Leica3` | `MakerNoteLeica3` (R8, R9) | none (anything not starting `LEICA` or `FUJIFILM`) | 0 | TIFF header | `Leica3` |
| `Leica4` | `MakerNoteLeica4` (M9, M Monochrom) | `LEICA0` | 8 | note start | `Leica4` |
| `Leica5` | `MakerNoteLeica5` (X1, X2, X Vario, T, X, X-U) | `LEICA\0` + one of `01 04 05 06 07 10 1a` + `\0` | 8 | note start | `Leica5` |
| `Leica8` | `MakerNoteLeica8` (Q, SL, CL) | `LEICA\0` + one of `08 09 0a` + `\0` | 8 | TIFF header | `Leica5` |
| `Leica9` | `MakerNoteLeica9` (M10, S) | `LEICA\0\x02\0` | 8 | TIFF header | `Leica9` |
| `Leica10` | `MakerNoteLeica10` (D-Lux 7) | `LEICA CAMERA AG\0` | 18, no next pointer | TIFF header | Panasonic `Main` |

"Note start" is ExifTool's `Base => '$start - 8'`: the IFD starts 8 bytes in and its offsets count
from the note's first byte. "TIFF header" is no `Base` at all. Every dialect inherits the outer
stream's byte order. `Leica10` follows the Panasonic layout its tag table comes from (exiv2 reads
Panasonic notes with no next-IFD pointer); the others end in one.

Left without a layout, because the bytes alone cannot decide them:

- **`LEICA\0\0\0`** is `MakerNoteLeica` (the Panasonic-built bodies, `Make` exactly `LEICA`:
  Panasonic `Main` table, TIFF-header offsets) or `MakerNoteLeica2` (the M8, `Make`
  `Leica Camera AG`: note-relative offsets whose base `FixLeicaBase` picks from the data, IFD start
  or note start). The two differ in their offset base, and this reader does not see `Make`.
- **`LEICA\0\x02\xff`** is `MakerNoteLeica6` (S2, M (Typ 240), S (Typ 006), decided by `Model`;
  TIFF-header offsets, or absolute file offsets when the note sits in a JPEG trailer) or
  `MakerNoteLeica7` (M Monochrom (Typ 246): absolute file offsets, `Base => '-$base'`). File
  offsets are TIFF offsets only when the TIFF header is at file offset 0, and `OffsetBase` has no
  way to say "the file's start" for the JPEG case.
- **Any other note starting `LEICA`**, which no ExifTool entry names.

Where this reader deliberately differs from ExifTool:

- **`Make` is not consulted beyond the vendor.** ExifTool's first entry sends *every* note under
  `Make` exactly `LEICA` to the Panasonic table at byte 8 with TIFF-header offsets, whatever its
  signature. Here the signature decides, so a `Leica5`-signed note under that `Make` counts its
  offsets from the note start. ExifTool's comments record only `LEICA\0\0\0` notes from those
  bodies. Likewise `MakerNoteLeica3` excludes the S2 and M (Typ 240), which ExifTool reads at byte
  8 whatever their signature; here a bare IFD is `Leica3` for every Leica body.
- **Only Fujifilm's signature outranks Leica's here.** ExifTool tries many signature-only entries
  before any Leica one (`Nikon\0\x02`, `OLYMP\0`, `QVC\0`, …), whatever `Make` says. Here only
  `FUJIFILM`, the one Leica has shipped (its Fujifilm-built bodies), is taken from another
  vendor; a Leica note under any other foreign signature would read as `Leica3`.
- **Byte order is inherited, not guessed.** ExifTool's `ByteOrder => 'Unknown'` picks whichever
  order makes the entry count plausible. Here the IFD follows the outer stream, as exiv2 does for
  every dialect without a byte-order mark. Both corpus notes are little-endian inside
  little-endian files.

## What the view decodes

`LeicaMakerNote` keeps every entry of the note's IFD as a `gamut_ifd::Value`
(`LeicaMakerNote::ifd`), so any tag is reachable by number, for every dialect except `Leica10`
(a Panasonic-format note, which the Panasonic view will read) and a `FUJIFILM`-signed note (which
follows Fujifilm's table). Typed accessors (`Leica4Tags`, `Leica9Tags`) exist for the two
tables the corpus holds, each checked against ExifTool's reading of its fixture in
`crates/gamut-exif/tests/maker_note_leica.rs`. Values are raw, as stored: the "raw" column is
what `exiftool -v3` prints for the fixture, and the "ExifTool" column is its value conversion
(not applied by the view).

### `Leica9` (`LeicaMakerNote::leica9`, `Panasonic.pm` line 2193)

| Tag | ExifTool name | Type | Accessor | Fixture: raw | ExifTool |
| --- | --- | --- | --- | --- | --- |
| `0x0304` | `FocusDistance` | `int32u` | `focus_distance` | 16279 | 16279 |
| `0x0311` | `ExternalSensorBrightnessValue` | `rational64s` | `external_sensor_brightness_value` | 0/1 | 0 |
| `0x0312` | `MeasuredLV` | `rational64s` | `measured_lv` | 0/1 | 0 |
| `0x034c` | `UserProfile` | `string` | `user_profile` | `""` (five NULs) | `""` |
| `0x0359` | `ISOSelected` | `int32s` | `iso_selected` | 100 | 100 |
| `0x035a` | `FNumber` | `int32s` | `f_number` | 5600 | 5.6 (`/ 1000`) |
| `0x035b` | `CorrelatedColorTemp` | `int16u` | `correlated_color_temp` | 5200 | 5200 |
| `0x035c` | `ColorTint` | `int16s` | `color_tint` | 6 | 6 |
| `0x035d` | `WhitePoint` | `rational64u[2]` | `white_point` | 3399/10000, 3509/10000 | 0.3399 0.3509 |
| `0x0370` | `LensProfileName` | `string` | `lens_profile_name` | absent | absent |

ExifTool forces `0x0311` and `0x0312` to `rational64s` ("may be incorrectly unsigned in JPEG
images"), so the view reads either `SRATIONAL` or `RATIONAL` there as signed, bit for bit.

### `Leica4` (`LeicaMakerNote::leica4`, `Panasonic.pm` lines 1736 and 1773)

The `Leica4` IFD holds four `UNDEFINED` entries, `0x3000`, `0x3100`, `0x3400` and `0x3900`, each
an IFD of its own (ExifTool's `Subdir` table) whose offsets count from the same base as the
note's. The view reads each one (`LeicaMakerNote::subdirectory`) and the accessors look in the one
their tag lives in:

| Tag | In | ExifTool name | Type | Accessor | Fixture: raw | ExifTool |
| --- | --- | --- | --- | --- | --- | --- |
| `0x300a` | `0x3000` | `Contrast` | `int32u` | `contrast` | 2 | 2 |
| `0x300b` | `0x3000` | `Sharpening` | `int32u` | `sharpening` | 2 | 2 |
| `0x300d` | `0x3000` | `Saturation` | `int32u` | `saturation` | 2 | 2 |
| `0x3033` | `0x3000` | `WhiteBalance` | `int32u` | `white_balance` | 0 | 0 |
| `0x3034` | `0x3000` | `JPEGQuality` | `int32u` | `jpeg_quality` | 97 | 97 |
| `0x3036` | `0x3000` | `WB_RGBLevels` | `rational64u[3]` | `wb_rgb_levels` | 16384/16384 ×3 | 1 1 1 |
| `0x3038` | `0x3000` | `UserProfile` | `string` | `user_profile` | `""` | `""` |
| `0x303a` | `0x3000` | `JPEGSize` | `int32u` | `jpeg_size` | 0 | 0 |
| `0x3103` | `0x3100` | `SerialNumber` | `string` | `serial_number` | `"4348886"` | 4348886 |
| `0x3109` | `0x3100` | `FirmwareVersion` | `string` | `firmware_version` | `"1.008"` | 1.008 |
| `0x312a` | `0x3100` | `BaseISO` | `int32u` | `base_iso` | 320 | 320 |
| `0x312b` | `0x3100` | `SensorWidth` | `int32u` | `sensor_width` | 5270 | 5270 |
| `0x312c` | `0x3100` | `SensorHeight` | `int32u` | `sensor_height` | 3516 | 3516 |
| `0x312d` | `0x3100` | `SensorBitDepth` | `int32u` | `sensor_bit_depth` | 14 | 14 |
| `0x3402` | `0x3400` | `CameraTemperature` | `int32s` | `camera_temperature` | 11 | 11 |
| `0x3405` | `0x3400` | `LensType` | `int32u` | `lens_type` | 135 | 33 3 (`>> 2`, `& 3`) |
| `0x3406` | `0x3400` | `ApproximateFNumber` | `rational64u` | `approximate_f_number` | 425984/65536 | 6.5 |
| `0x3407` | `0x3400` | `MeasuredLV` | `int32s` | `measured_lv` | 264448 | 2.64448 (`/ 1e5`) |
| `0x3408` | `0x3400` | `ExternalSensorBrightnessValue` | `int32s` | `external_sensor_brightness_value` | 378880 | 3.7888 (`/ 1e5`) |

Text accessors return the text before the first NUL; the raw value with its padding stays in
`ifd()` or `subdirectory()`. As for Canon, `gamut-ifd` decodes an `ASCII` value whole and requires
it to be UTF-8, so a text field with any non-UTF-8 byte is unreadable (named by
`unreadable_tags()`) and its accessor returns `None`.

Not decoded, and not promised: the interpretation of any value (lens names, the ExifTool scalings
above, print conversions), `0x3900`'s `Data1`/`Data2` binary blocks, the tags ExifTool leaves
unnamed, and typed accessors for the `Leica3`, `Leica5` and `Leica8` dialects' tables
(`Panasonic.pm` lines 1706 and 1997), which no corpus note exercises. Those notes are still
decoded into `ifd()`.
