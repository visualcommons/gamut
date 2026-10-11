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

## Conformance

Implementations are held to the standard via differential oracles (`exiv2`/`libexif`, C FFI) for
binary round-trip parity, plus **exiftool** as a golden tag-breadth reference (committed JSON
fixtures, regenerated offline). See [`gamut-exif/STATUS.md`](../../crates/gamut-exif/STATUS.md).
