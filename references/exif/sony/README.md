# Sony MakerNote (not a specification; issue #980)

Sony has never published the layout of its `MakerNote` (`0x927C`) payload. This directory stakes
what `gamut-exif`'s Sony view (`gamut_exif::maker_note::sony`) is held to, before any tag is
decoded, as the parent table in [`../README.md`](../README.md#makernote-dialects-not-a-specification-issue-418)
asks of every vendor.

## Sources

- **exiv2** — `third_party/exiv2/src/sonymn_int.cpp` at the pinned submodule commit
  `2cd987a731236037b6b78cbff897d08685a8ef49`: the `SonyMakerNote::tagInfo_[]` table (from line
  1422) names each tag of the note's IFD and the type exiv2 expects. exiv2 uses that one table for
  both of its Sony groups (`third_party/exiv2/src/tags_int.cpp` lines 152–153 map `Sony1` and
  `Sony2` to `SonyMakerNote::tagList`). The same exiv2 is what `tooling/exiv2-oracle` builds, so it
  is both the written source and the differential oracle: its `exif_get` reads any
  `Exif.Sony1.*` / `Exif.Sony2.*` key.
- **exiv2's dispatch** — `third_party/exiv2/src/makernote_int.cpp`: line 122 registers `SONY` with
  `newSonyMn` (line 814), which picks `Sony1` for a note opening `SONY DSC \0\0\0` (the
  `SonyMnHeader` of line 571: IFD at byte 12, built with `hasNext = false`, so the IFD ends without
  a next-IFD pointer) and `Sony2` for any other note of at least 18 bytes (a bare IFD at byte 0 with
  a next-IFD pointer). Both inherit the outer byte order and count value offsets from the outer TIFF
  header. These are the two Sony rows of the parent table, which `MakerNoteLayout::detect`
  resolves. exiv2 reads the next-IFD pointer of a `Sony2` note and does not follow it
  (`tiffimage_int.cpp` line 1900, `ignoreTiffComponent` for `Tag::next`).
- **ExifTool** — `lib/Image/ExifTool/Sony.pm` (not vendored; the tag tables are published at
  <https://exiftool.org/TagNames/Sony.html>). Used only to cross-read a name or type where exiv2's
  table is silent; no tag here depends on it alone.

## What the view decodes

The view reads the `Sony2` dialect only. A `Sony1` note gets its layout but no view yet:
`gamut-ifd` cannot read a directory that has no next-IFD pointer (#1032), and the `Sony1` view is
issue #1033.

The view keeps every entry of the note's IFD as a `gamut_ifd::Value` (`SonyMakerNote::ifd`), so
any tag is reachable by number. These sixteen have typed accessors; each is checked against exiv2
tag by tag in `crates/gamut-exif/tests/oracle_sony.rs`:

| Tag | exiv2 key (`Exif.Sony2.` +) | exiv2 type (`sonymn_int.cpp`) | Accessor |
| --- | --- | --- | --- |
| `0x0102` | `Quality` | `unsignedLong` (line 1423) | `quality` |
| `0x0104` | `FlashExposureComp` | `signedRational` (line 1425) | `flash_exposure_comp` |
| `0x0115` | `WhiteBalance` | `unsignedLong` (line 1431) | `white_balance` |
| `0x2002` | `Rating` | `unsignedLong` (line 1441) | `rating` |
| `0x2004` | `Contrast` | `signedLong` (line 1442) | `contrast` |
| `0x2005` | `Saturation` | `signedLong` (line 1443) | `saturation` |
| `0x2006` | `Sharpness` | `signedLong` (line 1445) | `sharpness` |
| `0x2007` | `Brightness` | `signedLong` (line 1447) | `brightness` |
| `0x2031` | `SerialNumber` | `asciiString` (line 1519) | `serial_number` |
| `0xb000` | `FileFormat` | `unsignedByte` (line 1533) | `file_format` |
| `0xb001` | `SonyModelID` | `unsignedShort` (line 1535) | `model_id` |
| `0xb020` | `CreativeStyle` | `asciiString` (line 1538) | `creative_style` |
| `0xb021` | `ColorTemperature` | `unsignedLong` (line 1540) | `color_temperature` |
| `0xb027` | `LensID` | `unsignedLong` (line 1553) | `lens_id` |
| `0xb02a` | `LensSpec` | `unsignedByte` (line 1557) | `lens_spec` |
| `0xb02b` | `FullImageSize` | `unsignedLong` (line 1561) | `full_image_size` |

Notes on reading them:

- **Types are held exactly.** An accessor returns its value only when the entry has exiv2's type
  (and, for the scalars, a count of one); anything else reads as `None` and stays in `ifd()`.
  `FlashExposureComp` is the raw `SRATIONAL` numerator/denominator pair, not a float.
- **Text** fields return the text before the first NUL, which is what exiv2 reports; the raw value
  with any padding stays in `ifd()`. As for Canon (`../canon/README.md`), a text field holding any
  non-UTF-8 byte is unreadable through `gamut-ifd` and its accessor returns `None` where exiv2
  still reports the prefix.
- **Arrays** (`FileFormat`, `LensSpec`, `FullImageSize`) come back whole. ExifTool documents
  `FullImageSize` as height then width; the view does not reorder or name the elements.
- **Offsets** are file-absolute. A view built from the note's bytes alone (`MakerNote::sony`) can
  read only values that lie inside the note itself, while `SonyMakerNote::read` reads through the
  whole TIFF stream, as exiv2 does.

Not decoded, and not promised: the interpretation of any value (exiv2's `exif_print` names
`SonyModelID` 358 "ILCE-9"; the view returns the number), the `Sony1` dialect (#1033), the binary
arrays exiv2 splits (`CameraSettings` `0x0114`, `0x2010`, `0x3000`, and `0x9400`–`0x9404`, which
Sony enciphers; `tiffimage_int.cpp` lines 1876–1899), the Minolta-format subdirectory at `0xb028`, and the embedded `PreviewImage`
(`0x2001`).
