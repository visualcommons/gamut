# Apple MakerNote (not a specification; issue #976)

Apple has never published the layout of its `MakerNote` (`0x927C`) payload, and exiv2 has no
Apple module (`TiffMnCreator::registry_` in `third_party/exiv2/src/makernote_int.cpp` has no Apple
row), so `tooling/exiv2-oracle` cannot serve as the oracle. This directory stakes what
`gamut-exif`'s Apple view (`gamut_exif::AppleMakerNote`) is held to, as the parent table in
[`../README.md`](../README.md#makernote-dialects-not-a-specification-issue-418) asks of every
vendor.

## Sources

- **ExifTool 13.59**, commit `2200871d9cef988051d2a99d67df3bda6cbb30a8` of
  <https://github.com/exiftool/exiftool>, the same commit [`../leica/`](../leica/README.md) stakes.
  Not vendored: ExifTool is distributed under the Perl Artistic License or the GPL, and only the
  facts below (signature, offsets, tag numbers, types) are needed, so they are transcribed rather
  than copied. The two files read, with the SHA-256 of each at that commit:
  - `lib/Image/ExifTool/MakerNotes.pm`
    (`5c78d72ce8f07142ce4e9e400616725480495c3c85514b2861cadd18077c119c`): the `MakerNoteApple`
    entry (lines 37–46), the first one ExifTool tries. It matches `Apple iOS\0`, starts the IFD at
    byte 14 (`Start => '$valuePtr + 14'`) and counts its offsets from the note's first byte
    (`Base => '$start - 14'`). That is the Apple row of the parent table, which
    `MakerNoteLayout::detect` resolves.
  - `lib/Image/ExifTool/Apple.pm`
    (`c4fc860e3216907e3fb2a063fcd64093410a70d18ecb1d30beeeccbfab15b0b7`, module version 1.15):
    the `%Image::ExifTool::Apple::Main` tag table (lines 24–320), which names each tag and, where
    it has one, its `Writable` type.
- **The corpus**: the one Apple note in `third_party/gamut-dng-samples` (CC0-1.0), extracted byte
  for byte into `crates/gamut-exif/tests/fixtures/`:

  | Fixture | Source file (corpus SHA-256) | Note at (TIFF offset) | Length | Fixture SHA-256 |
  | --- | --- | --- | --- | --- |
  | `apple_iphone_12_pro_maker_note.bin` | `apple/iphone-12-pro/IMG_1361.DNG` (`e91e77a4…dfc2`) | 3276 | 1268 | `c94a09a0…7c5a` |

  The DNG is a big-endian TIFF file whose header is at file offset 0, so the note's file offset and
  its TIFF offset are the same number. Re-extract with
  `dd if=FILE of=FIXTURE bs=1 skip=3276 count=1268`. (The file holds a second `Apple iOS` note at
  64938, in another IFD; it is not the `0x927C` value of the Exif IFD and is not the fixture.)
- **ExifTool's reading of that file**, committed beside this file as
  `exiftool-13.59-iphone-12-pro-IMG_1361.txt`. Regenerate it with
  `exiftool -a -G1 -H -n -s -U -MakerNotes:all FILE` (`-U` adds the tags `Apple.pm` marks
  `Unknown`). The raw stored values the view returns are the ones `exiftool -v3 -U FILE` prints,
  and the table below lists them.

## Layout

The note opens with `Apple iOS\0`, two version bytes (`00 01` in the fixture) and a byte-order mark
at byte 12 (`MM` in the fixture), then the IFD at byte 14, ending in a next-IFD pointer, whose value
offsets count from the note's first byte. `MakerNoteLayout::detect` takes the IFD's byte order from
the mark, and from the outer stream when the mark is neither `II` nor `MM`.

Where this reader deliberately differs from ExifTool: **the byte order is read, not guessed.**
ExifTool's `ByteOrder => 'Unknown'` picks whichever order makes the entry count plausible; here the
mark decides, as for every other dialect that carries one. The fixture's mark and ExifTool's guess
agree (big-endian).

## What the view decodes

`AppleMakerNote` keeps every entry of the note's IFD as a `gamut_ifd::Value`
(`AppleMakerNote::ifd`), so any tag is reachable by number. The thirty-four tags below have typed
accessors. Each accessor returns its value only when the entry has the type in the "Type" column
and, for a scalar, a count of one; anything else reads as `None` and stays in `ifd()`. The "Type"
column is `Apple.pm`'s `Writable` (`int32s` is `SLONG`, `rational64s` is `SRATIONAL`, `string` is
`ASCII`); where `Apple.pm` gives none, it is the type the fixture stores. Each tag the fixture
holds is checked against ExifTool's reading of it in `crates/gamut-exif/tests/maker_note_apple.rs`;
the "Fixture: raw" column is what `exiftool -v3 -U` prints, and "—" marks a tag the fixture does
not hold, which only the view's own synthetic tests reach.

| Tag | ExifTool name (`Apple.pm` line) | Type | Accessor | Fixture: raw |
| --- | --- | --- | --- | --- |
| `0x0001` | `MakerNoteVersion` (30) | `int32s` | `maker_note_version` | 12 |
| `0x0002` | `AEMatrix` (34) | `undef` (binary plist) | `ae_matrix` | 558 bytes, `bplist00…` |
| `0x0003` | `RunTime` (40) | `undef` (binary plist) | `run_time` | 104 bytes, `bplist00…` |
| `0x0004` | `AEStable` (44) | `int32s` | `ae_stable` | 1 |
| `0x0005` | `AETarget` (49) | `int32s` | `ae_target` | 186 |
| `0x0006` | `AEAverage` (53) | `int32s` | `ae_average` | 180 |
| `0x0007` | `AFStable` (57) | `int32s` | `af_stable` | 1 |
| `0x0008` | `AccelerationVector` (62) | `rational64s[3]` | `acceleration_vector` | -1702/415935, -6172/6145, -4786/445081 |
| `0x000a` | `HDRImageType` (80) | `int32s` | `hdr_image_type` | — |
| `0x000b` | `BurstUUID` (89) | `string` | `burst_uuid` | — |
| `0x000c` | `FocusDistanceRange` (94) | `rational64s[2]` | `focus_distance_range` | 67/256, 97/256 |
| `0x000f` | `OISMode` (106) | `int32s` | `ois_mode` | — |
| `0x0011` | `ContentIdentifier` (112) | `string` | `content_identifier` | — |
| `0x0014` | `ImageCaptureType` (122) | `int32s` | `image_capture_type` | 10 |
| `0x0015` | `ImageUniqueID` (134) | `string` | `image_unique_id` | — |
| `0x0017` | `LivePhotoVideoIndex` (139) | `int32s` (none given) | `live_photo_video_index` | 1112547584 |
| `0x0019` | `ImageProcessingFlags` (144) | `int32s` | `image_processing_flags` | 65536 |
| `0x001a` | `QualityHint` (150) | `string` | `quality_hint` | `"q900n"` |
| `0x001d` | `LuminanceNoiseAmplitude` (159) | `rational64s` | `luminance_noise_amplitude` | — |
| `0x001f` | `PhotosAppFeatureFlags` (164) | `int32s` | `photos_app_feature_flags` | 0 |
| `0x0020` | `ImageCaptureRequestID` (169) | `string` | `image_capture_request_id` | `"6D45E6A1-BE56-43A0-B1A8-0175E380A06A"` |
| `0x0021` | `HDRHeadroom` (174) | `rational64s` | `hdr_headroom` | 63809/37837 |
| `0x0023` | `AFPerformance` (179) | `int32s[2]` | `af_performance` | 954, 268435495 |
| `0x0025` | `SceneFlags` (192) | `int32s` | `scene_flags` | 134 |
| `0x0026` | `SignalToNoiseRatioType` (198) | `int32s` | `signal_to_noise_ratio_type` | 3 |
| `0x0027` | `SignalToNoiseRatio` (203) | `rational64s` | `signal_to_noise_ratio` | 466861/7493 |
| `0x002b` | `PhotoIdentifier` (210) | `string` | `photo_identifier` | `"C465536F-F5F5-4458-A135-EF72CDEA1BE5"` |
| `0x002d` | `ColorTemperature` (216) | `int32s` | `color_temperature` | 5358 |
| `0x002e` | `CameraType` (221) | `int32s` | `camera_type` | 1 |
| `0x002f` | `FocusPosition` (231) | `int32s` | `focus_position` | 55 |
| `0x0030` | `HDRGain` (235) | `rational64s` | `hdr_gain` | 0/1 |
| `0x0038` | `AFMeasuredDepth` (248) | `int32s` | `af_measured_depth` | 403 |
| `0x003d` | `AFConfidence` (259) | `int32s` | `af_confidence` | 0 |
| `0x003f` | `GreenGhostMitigationStatus` (268) | `int32s` | `green_ghost_mitigation_status` | — |

Notes on reading them:

- **Arrays** (`acceleration_vector`, `focus_distance_range`, `af_performance`) come back whole and
  unreordered, whatever their count; `Apple.pm`'s `Count` is not enforced. The rationals are the raw
  numerator/denominator pairs, not floats (ExifTool prints `AccelerationVector` as
  `-0.004091985527 -1.004393816 -0.01075309887`).
- **Binary plists** (`ae_matrix`, `run_time`) are the whole `UNDEFINED` value, `bplist00` header
  included. ExifTool decodes them (`ConvertPLIST`; `RunTime` into the `RunTimeFlags`,
  `RunTimeValue`, `RunTimeScale` and `RunTimeEpoch` of a CMTime: 1, 4905158086875, 1000000000 and 0
  in the fixture); `gamut-exif` has no plist decoder, and the view does not decode them.
- **`LivePhotoVideoIndex`** has no `Writable` in `Apple.pm`; the view reads it as the fixture stores
  it, one `SLONG`, and returns the number ExifTool prints.
- **Text** fields return the text before the first NUL, which is what ExifTool reports; the raw value
  with its padding stays in `ifd()`. As for Canon (`../canon/README.md`), a text field holding any
  non-UTF-8 byte is unreadable through `gamut-ifd` and its accessor returns `None`.
- **Offsets** count from the note's start, so a view built from the note's bytes alone
  (`MakerNote::apple`) reads every value a well-formed note holds, and `AppleMakerNote::read` reads
  the same view through the whole TIFF stream.

Not decoded, and not promised: the interpretation of any value (ExifTool's print conversions, such as
`ImageCaptureType` 10 "Photo" or `CameraType` 1 "Back Normal"), the binary-plist contents, the
binary-plist tags with no fixture entry (`ColorCorrectionMatrix` `0x003e`, `SemanticStyle` `0x0040`,
`SemanticStyleRenderingVer` `0x0041`, `SemanticStylePreset` `0x0042`, and `Apple_0x004e`,
`Apple_0x004f`, `Apple_0x0054`, `Apple_0x005a`), and the tags `Apple.pm` leaves unnamed (`0x000d`,
`0x000e`, `0x0010`, `0x001c`, `0x0036`, `0x0037`, `0x0039`, `0x003b` and `0x003c` in the fixture).
All of them stay in `ifd()`.
