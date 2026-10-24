# EXIF (Exchangeable image file format)

Reference specifications for the `gamut-exif` crate (and the shared `gamut-ifd` TIFF/IFD core it
builds on).

## Authoritative editions (vendored)

- `exif-3.0-dc-008-translation-2023.pdf` — **Exif 3.0 — CIPA DC-008-Translation-2023-E / JEITA
  CP-3461** — the current standard and the edition `gamut-exif` targets. This is the freely-published
  English translation of CIPA DC-008-2023; the later DC-008-2024 is an editorial-corrected reprint of
  the same Exif 3.0. Source: Camera & Imaging Products Association,
  <https://www.cipa.jp/std/std-sec_e.html> (English translation also mirrored at the Internet
  Archive).
- `exif-2.32-dc-008-2019.pdf` — **Exif 2.32 — CIPA DC-008-Translation-2019-E** — the long-deployed
  prior edition; retained as the reference for legacy tag compatibility (most cameras in the wild emit
  2.2–2.32). Source: <https://www.cipa.jp/std/documents/e/DC-X008-Translation-2019-E.pdf>.
- `exif-xmp-dc-010-2026.pdf` — **Exif metadata for XMP — CIPA DC-010-2026** (June 2026; Exif 3.1
  mapping) — the sibling of DC-008 that maps the Exif/TIFF tags onto XMP properties, and the
  document Adobe XMP Part 2 §3.4 points at for the `exif`/`tiff` schemas. It is the source of the
  namespace URIs `gamut-xmp` registers for that mapping: §5.1 Table 1, §7.2 Table 6 and §10.1
  Table 20 bind the recommended prefix `exifEX` to `http://cipa.jp/exif/1.0/` (Exif 2.21 or later),
  beside `exif` = `http://ns.adobe.com/exif/1.0/` (Exif 2.2 or earlier) and `tiff` =
  `http://ns.adobe.com/tiff/1.0/`. The NOTE under Table 6 retires `http://cipa.jp/exif/2.32/`,
  which DC-010-2020 had used for the picture-taking-condition properties of Table 13
  (`CompositeImage`, `SourceImageNumberOfCompositeImage`, `SourceExposureTimesOfCompositeImage`):
  those now live under `…/exif/1.0/`, and same-named properties in the two namespaces are declared
  to have no logical connection. (The Exif 3.0 DC-008 text above still shows `…/exif/2.32/` in its
  Annex J.2–J.3 annotation examples; DC-010-2026's own annotation samples, Annex C.1.2–C.1.3, bind `exifEX` to `…/exif/1.0/` there too.)
  Source: Camera & Imaging Products Association, <https://www.cipa.jp/e/std/std-sec.html>, entry
  "CIPA DC-010-2026 Exif metadata for XMP" (download behind a disclaimer form; earlier editions
  2012, 2017, 2020 and 2024 are listed there as history).

Exif's structure **is** a constrained profile of TIFF: an `Exif\0\0` marker followed by a TIFF
stream (byte-order header + IFD chain). The JPEG `APP1` carriage of that blob (DC-008 §4.7.2) is
implemented by `gamut-jpeg` — see [`../jpeg`](../jpeg/README.md#appn-metadata-embedding-exif--xmp--icc). The 0th/1st IFDs plus the Exif, GPS, and Interoperability
sub-IFDs are all parsed through the shared [`gamut-ifd`](../../crates/gamut-ifd) primitive, whose
structural reference is **TIFF 6.0** (see [`../tiff`](../tiff)).

## Not vendored (cross-checked via oracle)

- **TIFF/EP — ISO 12234-2** — paywalled. The EP-specific tags are cross-checked against the
  `exiv2`/`libexif` differential oracle rather than vendored, following the same policy as the
  paywalled ISOBMFF/HEIF specs.

## MakerNote dialects (not a specification; issue #418)

No standard defines the `MakerNote` (`0x927C`) payload. DC-008 calls it manufacturer-specific and
says nothing more. The dispatch layer in `gamut-exif` (`MakerNoteLayout`, issue #968) therefore
follows the closest thing to a documented table. That is **exiv2's** dispatch: `TiffMnCreator::registry_`
and the `*MnHeader` classes in `third_party/exiv2/src/makernote_int.cpp` (the pinned submodule
the `exiv2-oracle` builds), summarised at <https://exiv2.org/makernote.html>. Apple is not in exiv2,
so its row comes from ExifTool's `MakerNoteApple` (`lib/Image/ExifTool/MakerNotes.pm`, not
vendored) and was checked against the iPhone 12 Pro sample in `third_party/gamut-dng-samples`.
The vendor is taken from `Make`, and the note's leading bytes then choose among that vendor's
dialects:

| Dialect | Signature | IFD at | Byte order | Offsets count from |
| --- | --- | --- | --- | --- |
| Canon, Minolta, Nikon 1, Sony 2, Casio 1 | none | 0 | outer | TIFF header |
| Samsung | none (an `AOC\0` note is Pentax) | 0 | outer | note start |
| Nikon 2 | `Nikon\0` with no TIFF header at byte 10 | 8 | outer | TIFF header |
| Nikon 3 | `Nikon\0` + 4 bytes + TIFF header | 10 + header offset | embedded header | note byte 10 |
| Sony 1 | `SONY DSC \0\0\0` | 12, no next pointer | outer | TIFF header |
| Fujifilm | `FUJIFILM` + LE u32 IFD offset | that offset | little-endian | note start |
| Olympus 1 | `OLYMP\0` | 8 | outer | TIFF header |
| Olympus 2 | `OLYMPUS\0` + `II`/`MM` | 12 | the mark | note start |
| OM System | `OM SYSTEM\0\0\0` + `II`/`MM` | 16 | the mark | note start |
| Panasonic | `Panasonic` | 12, no next pointer | outer | TIFF header |
| Pentax (also Ricoh, Samsung) | `AOC\0` | 6 | mark at 4, else outer | TIFF header |
| Pentax DNG (also Ricoh, Samsung) | `PENTAX \0` | 10 | mark at 8, else outer | note start |
| Sigma | `SIGMA\0\0\0` or `FOVEON\0\0` | 10 | outer | TIFF header |
| Casio 2 | `QVC\0\0\0` | 6 | big-endian | TIFF header |
| Apple | `Apple iOS\0` | 14 | mark at 12, else outer | note start |

A note resolves only if it holds its header plus a one-entry IFD (2 + 12 bytes) at the IFD
offset. Otherwise it has no layout and stays opaque, as does a note whose vendor has no row here.

Where this reader deliberately differs from exiv2:

- **Byte-order marks are read, not ignored.** exiv2 inherits the outer order for Olympus 2, OM
  System and both Pentax headers even though each one carries `II`/`MM`. Its Olympus 2 and OM
  System checks accept only `II`. Here the mark decides. An Olympus 2 or OM System header without
  a valid mark matches nothing, and a Pentax mark that is neither `II` nor `MM` falls back to the
  outer order. ExifTool reads these marks the same way.
- **Minimum size is one rule, not nineteen.** exiv2 asks most dialects for an 18-byte IFD (a
  next pointer included) and some for 14. Here every dialect needs a one-entry IFD at its offset,
  so this reader also accepts notes that end without a next pointer. It rejects one thing exiv2
  admits: a Casio 2 note under 20 bytes. exiv2 accepts that on the signature alone and then has
  no IFD to read in it.
- **A Nikon 3 header whose IFD offset overflows** (`10 + offset` past `u32`) has no layout here.
  exiv2 would build the reader and fail at the IFD.
- **Samsung under a `PENTAX \0` header** is read as Pentax DNG. exiv2 checks only `AOC\0` for
  Samsung.

The vendor tags themselves are staked per vendor in a subdirectory, before that vendor's view is
implemented: [`canon/`](canon/README.md) (issue #977).

Notes that exiv2 does not cover, Leica's being the one the sample corpus holds, have no
dialect yet. Each one is staked here by its vendor's sub-issue of #418 before it is implemented
(Leica: #978). A Canon note that Adobe DNG Converter moved into `DNGPrivateData` (`Adobe\0MakN`)
is outside the `0x927C` tag. That is its own sub-issue, #989.

## Conformance

Implementations are held to the standard via differential oracles (`exiv2`/`libexif`, C FFI) for
binary round-trip parity, plus **exiftool** as a golden tag-breadth reference (committed JSON
fixtures, regenerated offline). See [`gamut-exif/STATUS.md`](../../crates/gamut-exif/STATUS.md).
