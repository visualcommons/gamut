# Canon MakerNote (not a specification; issue #977)

Canon has never published the layout of its `MakerNote` (`0x927C`) payload. This directory stakes
what `gamut-exif`'s Canon view (`gamut_exif::maker_note::canon`) is held to, before any tag is
decoded, as the parent table in [`../README.md`](../README.md#makernote-dialects-not-a-specification-issue-418)
asks of every vendor.

## Sources

- **exiv2** — `third_party/exiv2/src/canonmn_int.cpp` at the pinned submodule commit
  `2cd987a731236037b6b78cbff897d08685a8ef49`: the `CanonMakerNote::tagInfo_[]` table (from line
  459) names each tag of the note's IFD and the type exiv2 expects. The same exiv2 is what
  `tooling/exiv2-oracle` builds, so it is both the written source and the differential oracle:
  its `exif_get` reads any `Exif.Canon.*` key, and the elements of the binary arrays below under
  `Exif.CanonCs.*` / `Exif.CanonSi.*`.
- **exiv2's dispatch** — `third_party/exiv2/src/makernote_int.cpp` line 109 registers `Canon` with
  `newIfdMn` / `newIfdMn2`: a bare IFD at byte 0 of the note, the outer stream's byte order, and
  value offsets counted from the outer TIFF header. That is the `Canon` row of the parent table,
  which `MakerNoteLayout::detect` resolves.
- **ExifTool** — `lib/Image/ExifTool/Canon.pm` (not vendored; the tag tables are published at
  <https://exiftool.org/TagNames/Canon.html>). Used only to cross-read a name or type where exiv2's
  table is silent; no tag here depends on it alone.

## What the view decodes

The view keeps every entry of the note's IFD as a `gamut_ifd::Value` (`CanonMakerNote::ifd`), so
any tag is reachable by number. These eleven have typed accessors; each is checked against exiv2
tag by tag in `crates/gamut-exif/tests/maker_note_canon.rs`:

| Tag | exiv2 key | exiv2 type (`canonmn_int.cpp`) | Accessor |
| --- | --- | --- | --- |
| `0x0001` | `Exif.Canon.CameraSettings` → `Exif.CanonCs.*` | `unsignedShort` array (line 461) | `camera_settings` |
| `0x0002` | `Exif.Canon.FocalLength` | `unsignedShort` array (line 463) | `focal_length` |
| `0x0004` | `Exif.Canon.ShotInfo` → `Exif.CanonSi.*` | `unsignedShort` array (line 467) | `shot_info` |
| `0x0006` | `Exif.Canon.ImageType` | `asciiString` (line 471) | `image_type` |
| `0x0007` | `Exif.Canon.FirmwareVersion` | `asciiString` (line 473) | `firmware_version` |
| `0x0008` | `Exif.Canon.FileNumber` | `unsignedLong` (line 475) | `file_number` |
| `0x0009` | `Exif.Canon.OwnerName` | `asciiString` (line 477) | `owner_name` |
| `0x000c` | `Exif.Canon.SerialNumber` | `unsignedLong` (line 481) | `serial_number` |
| `0x0010` | `Exif.Canon.ModelID` | `unsignedLong` (line 489) | `model_id` |
| `0x0095` | `Exif.Canon.LensModel` | `asciiString` (line 541) | `lens_model` |
| `0x0096` | `Exif.Canon.InternalSerialNumber` | `asciiString` (line 543) | `internal_serial_number` |

Notes on reading them:

- **Text** fields are fixed-width and NUL-padded in real files (`OwnerName` is 32 bytes). The
  accessors return the text before the first NUL, which is what exiv2 reports; the raw value with
  its padding stays in `ifd()`. **Divergence from exiv2:** `gamut-ifd` decodes an `ASCII` value
  whole and requires it to be UTF-8 (`Value::decode`, `crates/gamut-ifd/src/value.rs`), so a text
  field holding any non-UTF-8 byte — a Latin-1 `OwnerName`, or stale bytes in the padding after
  the first NUL — is left out of `ifd()` and named by `unreadable_tags()`, and its accessor
  returns `None`. exiv2 still reports the bytes before the first NUL for such a field. Returning
  that prefix would need the raw field bytes, which only a `gamut-ifd` change can supply; it is
  not promised by this view.
- **`CameraSettings` and `ShotInfo`** are binary arrays: exiv2 splits them into one key per
  `SHORT` (`tiffimage_int.cpp`, `canonCsCfg` and `canonSiCfg`, both "with size element", so
  element 0 is the array's own byte length). The view returns the whole array unsplit; naming
  each element is not part of this view. exiv2 also merges `CameraSettings` elements 23–25 into
  one `Exif.CanonCs.Lens` key (`canonCsDef`), which the view does not do either.
- **Offsets** are file-absolute. A view built from the note's bytes alone (`MakerNote::canon`)
  can read only values that lie inside the note itself — every Canon body observed so far keeps
  them there — while `CanonMakerNote::read` reads through the whole TIFF stream, as exiv2 does.

Not decoded, and not promised: the interpretation of any value (exiv2's `exif_print` names
`ModelID` 0x80000421 "EOS R5"; the view returns the number), the other binary arrays exiv2 splits
(`PictureInfo`, `FileInfo`, `ProcessingInfo`, …), `AFInfo2`'s special decoder, and `ColorData`.
