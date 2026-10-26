# Fujifilm MakerNote (not a specification; issue #981)

Fujifilm has never published the layout of its `MakerNote` (`0x927C`) payload. This directory
stakes what `gamut-exif`'s Fujifilm view (`gamut_exif::maker_note::fujifilm`) is held to, before
any tag is decoded, as the parent table in
[`../README.md`](../README.md#makernote-dialects-not-a-specification-issue-418) asks of every
vendor.

## Sources

- **exiv2** — `third_party/exiv2/src/fujimn_int.cpp` at the pinned submodule commit
  `2cd987a731236037b6b78cbff897d08685a8ef49`: the `FujiMakerNote::tagInfo_[]` table (from line
  366) names each tag of the note's IFD and the type exiv2 expects. `third_party/exiv2/src/tags_int.cpp`
  line 86 names the group `Fujifilm`, so its keys are `Exif.Fujifilm.*`. The same exiv2 is what
  `tooling/exiv2-oracle` builds, so it is both the written source and the differential oracle: its
  `exif_get` reads any `Exif.Fujifilm.*` key.
- **exiv2's dispatch** — `third_party/exiv2/src/makernote_int.cpp`: line 111 registers any `Make`
  starting `FUJI` with `newFujiMn` (line 700), which asks for the 12-byte header plus an 18-byte
  one-entry IFD (line 703). `FujiMnHeader` (lines 283–324) checks the 8-byte signature `FUJIFILM`,
  reads the IFD's offset as a little-endian `u32` at byte 8, fixes the note's byte order to
  little-endian whatever the outer stream's (line 284, "we ignore the byteOrder argument"), and
  counts value offsets from the note's own start (`baseOffset` returns `mnOffset`, line 306).
  That is the Fujifilm row of the parent table, which `MakerNoteLayout::detect` resolves. exiv2
  reads the IFD's next-IFD pointer and does not follow it (`tiffimage_int.cpp` line 1671,
  `ignoreTiffComponent` for `Tag::next` in the Fujifilm group).
- **ExifTool** — `lib/Image/ExifTool/FujiFilm.pm` (not vendored; the tag tables are published at
  <https://exiftool.org/TagNames/FujiFilm.html>). Used only to cross-read a name or meaning where
  exiv2's table is silent; no tag here depends on it alone.

## What the view decodes

The view reads any note whose layout is the `Fujifilm` dialect — a `FUJIFILM` note under a `Make`
naming Fujifilm, and the same note in Leica's Fujifilm-built bodies, which the Leica dispatch
routes to this dialect (`../leica/README.md`). exiv2 decodes only the former, since its registry
keys on `Make`.

The view keeps every entry of the note's IFD as a `gamut_ifd::Value` (`FujifilmMakerNote::ifd`),
so any tag is reachable by number. These thirty-three have typed accessors; each is checked against
exiv2 tag by tag in `crates/gamut-exif/tests/oracle_fujifilm.rs`:

| Tag | exiv2 key (`Exif.Fujifilm.` +) | exiv2 type (`fujimn_int.cpp`) | Accessor |
| --- | --- | --- | --- |
| `0x0000` | `Version` | `undefined` (line 367) | `version` |
| `0x0010` | `SerialNumber` | `asciiString` (line 369) | `serial_number` |
| `0x1000` | `Quality` | `asciiString` (line 373) | `quality` |
| `0x1001` | `Sharpness` | `unsignedShort` (line 375) | `sharpness` |
| `0x1002` | `WhiteBalance` | `unsignedShort` (line 377) | `white_balance` |
| `0x1003` | `Color` | `unsignedShort` (line 379) | `color` |
| `0x1004` | `Tone` | `unsignedShort` (line 381) | `tone` |
| `0x1005` | `ColorTemperature` | `unsignedShort` (line 383) | `color_temperature` |
| `0x1006` | `Contrast` | `unsignedShort` (line 385) | `contrast` |
| `0x100a` | `WhiteBalanceFineTune` | `signedLong` (line 387) | `white_balance_fine_tune` |
| `0x100b` | `NoiseReduction` | `unsignedShort` (line 389) | `noise_reduction` |
| `0x100e` | `HighIsoNoiseReduction` | `unsignedShort` (line 391) | `high_iso_noise_reduction` |
| `0x1010` | `FlashMode` | `unsignedShort` (line 395) | `flash_mode` |
| `0x1011` | `FlashStrength` | `signedRational` (line 397) | `flash_strength` |
| `0x1020` | `Macro` | `unsignedShort` (line 399) | `macro_mode` |
| `0x1021` | `FocusMode` | `unsignedShort` (line 401) | `focus_mode` |
| `0x1030` | `SlowSync` | `unsignedShort` (line 413) | `slow_sync` |
| `0x1031` | `PictureMode` | `unsignedShort` (line 415) | `picture_mode` |
| `0x1040` | `ShadowTone` | `signedLong` (line 425) | `shadow_tone` |
| `0x1041` | `HighlightTone` | `signedLong` (line 427) | `highlight_tone` |
| `0x1100` | `Continuous` | `unsignedShort` (line 453) | `continuous` |
| `0x1101` | `SequenceNumber` | `unsignedShort` (line 455) | `sequence_number` |
| `0x1300` | `BlurWarning` | `unsignedShort` (line 471) | `blur_warning` |
| `0x1301` | `FocusWarning` | `unsignedShort` (line 473) | `focus_warning` |
| `0x1302` | `ExposureWarning` | `unsignedShort` (line 475) | `exposure_warning` |
| `0x1400` | `DynamicRange` | `unsignedShort` (line 477) | `dynamic_range` |
| `0x1401` | `FilmMode` | `unsignedShort` (line 479) | `film_mode` |
| `0x1402` | `DynamicRangeSetting` | `unsignedShort` (line 481) | `dynamic_range_setting` |
| `0x1404` | `MinFocalLength` | `unsignedRational` (line 485) | `min_focal_length` |
| `0x1405` | `MaxFocalLength` | `unsignedRational` (line 487) | `max_focal_length` |
| `0x1406` | `MaxApertureAtMinFocal` | `unsignedRational` (line 489) | `max_aperture_at_min_focal` |
| `0x1407` | `MaxApertureAtMaxFocal` | `unsignedRational` (line 491) | `max_aperture_at_max_focal` |
| `0x1431` | `Rating` | `unsignedLong` (line 499) | `rating` |

Notes on reading them:

- **Types.** `rating` accepts any unsigned integer type with a count of one, as `Ifd::get_u32`
  does. Every other accessor returns its value only when the entry has exiv2's type and, for a
  scalar, a count of one; anything else reads as `None` and stays in `ifd()`. The rationals are the
  raw numerator/denominator pairs (`Rational`/`SRational`), not floats.
- **`Version`** is the whole `UNDEFINED` value (four ASCII digits such as `0130` in Fujifilm's
  bodies), returned as bytes; exiv2's `exif_get` writes it as space-separated decimals.
- **`WhiteBalanceFineTune`** is the whole `SLONG` array (two values, red then blue shift, in
  Fujifilm's bodies), unreordered; exiv2's `exif_print` scales it, and the view does not.
- **Text** fields return the text before the first NUL, which is what exiv2 reports; the raw value
  with any padding stays in `ifd()`. As for Canon (`../canon/README.md`), a text field holding any
  non-UTF-8 byte is unreadable through `gamut-ifd` and its accessor returns `None` where exiv2
  still reports the prefix.
- **Offsets** count from the note's start, so a view built from the note's bytes alone
  (`MakerNote::fujifilm`) reads every value a well-formed note holds, and
  `FujifilmMakerNote::read` reads the same view through the whole TIFF stream.

Not decoded, and not promised: the interpretation of any value (exiv2's `exif_print` names
`FilmMode` 0 "PROVIA (F0/Standard)"; the view returns the number), the tags exiv2 lists that are
not typed here (they stay in `ifd()`), the face-detection text tags (`0x4005`–`0x4282`), and the
RAF-only `FujiIFD` subdirectory (`0xf000`–`0xf010`), which exiv2 reads only when parsing a RAF's
embedded TIFF. exiv2's table lists `0x1105` twice (`PixelShiftShots` and `PixelShiftOfffset`, lines
459 and 461), so its key for that tag is ambiguous; it is not typed here.
