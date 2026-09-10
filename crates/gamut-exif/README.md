# gamut-exif

`gamut-exif` is a pure-Rust **EXIF** (Exif 3.0 / CIPA DC-008) image-metadata parser and serializer.

## Goals

Part of the [gamut](../../README.md) workspace, this crate models the EXIF blob embedded in images
(the JPEG `APP1` payload, the WebP `EXIF` chunk, the PNG `eXIf` chunk, the AVIF/HEIF `Exif` item) so
the format crates can read, preserve, and embed camera/capture metadata. It is:

- **Memory-safe on hostile input.** `#![forbid(unsafe_code)]` — EXIF is offset-driven TIFF from
  untrusted files.
- **Spec-faithful.** Implemented from **Exif 3.0** (CIPA DC-008;
  [`../../references/exif`](../../references/exif)), with 2.32 legacy tag compatibility, and
  differentially tested against **exiv2** plus byte-level golden fixtures.
- **Layered on the shared IFD core.** EXIF *is* a constrained TIFF stream, so the IFD structure,
  byte order, value model, and offset machinery come from [`gamut-ifd`](../gamut-ifd) (whose
  [`Value`] is re-exported here rather than duplicated); this crate adds the EXIF tag dictionary, the
  typed GPS/thumbnail projections, the Exif/GPS/Interop sub-IFD layout, and MakerNote handling.

## Usage

[`Exif::parse`] reads a blob (with or without the `Exif\0\0` marker) and [`Exif::to_bytes`]
re-serialises it, preserving the source byte order. Read tags through the typed accessors or the
[`ExifTag`] catalogue; reach the raw directories as [`gamut_ifd::Ifd`]s when you need them.

```rust
use gamut_exif::{Exif, ExifTag, Value};

# fn demo(bytes: &[u8]) -> Result<(), gamut_exif::ExifError> {
let exif = Exif::parse(bytes)?;
println!("{:?} {:?}", exif.make(), exif.model());
if let Some(gps) = exif.gps() {
    println!("{:?}, {:?}", gps.latitude_deg(), gps.longitude_deg());
}
let jpeg = exif.thumbnail_bytes();          // the embedded JPEG thumbnail, if any

let mut edited = exif;
edited.set_tag(ExifTag::Software, Value::Ascii("gamut".into()));
let out = edited.to_bytes();                // Exif\0\0 + TIFF, ready to re-embed
# let _ = (jpeg, out);
# Ok(())
# }
```

For a bare TIFF stream (PNG `eXIf` / WebP `EXIF`) or a byte-order override, use [`ExifWriter`];
[`ExifReader`] carries the read-side options (`require_marker`, `strict`) and two further entry
points:

- **`parse_from`** reads through [`gamut_ifd::ReadAt`] instead of a slice, so the EXIF of a
  300 MB raw file costs a few hundred bytes of I/O rather than the whole file. `parse` is the
  `&[u8]` case of it — one parse engine, two entry points. It is deliberately synchronous: an
  async caller drives the source itself, which keeps a runtime dependency out of the crate.
- **`parse_with_report`** (and its `parse_from_with_report` twin) returns a `ReadReport` alongside
  the `Exif`, naming each region the lenient reader discarded — a malformed Exif/GPS/Interop
  sub-IFD, an unusable thumbnail range, or a top-level directory past the 1st IFD — with the tag
  that addressed it, the offset it carried, and a typed reason. `parse` stays silent, as before.
  The report is complete over those regions but is **not** a byte-completeness verdict: an empty
  report does not mean the parse lost nothing (see the deferred items below). A `strict` report is
  not always empty either: strictness rejects *malformed* regions, and a trailing directory is
  well-formed and merely unrepresentable, so it is reported in both modes.

```rust
# use gamut_exif::ExifReader;
# fn demo(bytes: &[u8]) -> Result<(), gamut_exif::ExifError> {
let (exif, report) = ExifReader::new().parse_with_report(bytes)?;
for dropped in report.dropped() {
    // e.g. "dropped GPS (tag 0x8825) at offset 65535: addresses bytes outside the EXIF blob"
    eprintln!("{dropped}");
}
# let _ = exif;
# Ok(())
# }
```

Enable the optional `geocoordinates` feature (also included by `full`) to convert a complete
[`GpsInfo`] with `TryFrom` into `geocoordinates::Wgs84` or `geocoordinates::Coordinate`. The latter
preserves EXIF sea-level altitude as an orthometric height; the 2D `Wgs84` newtype intentionally
drops altitude. Malformed references, rationals, DMS components, and out-of-range positions return
the typed [`GpsConversionError`].

## Compatibility

**One read verdict changed after 1.0.0.** A 1st IFD carrying `JPEGInterchangeFormat` with no
`JPEGInterchangeFormatLength` used to parse as a thumbnail that simply had no bytes; it is a
**loss** — named in the report as `DropReason::ThumbnailLengthMissing`, and **rejected by
`strict`** with `ExifError::BadThumbnail`. A caller running `strict` over blobs 1.0.0 accepted
should know the verdict moved.

The rule is about **readability**: an offset with nothing to size it addresses bytes that cannot be
read, which is what `strict` is for. It is not about a support level, and the reader does not
consult `Compression` at all.

**Conformance** is the separate question of whether the move is a *fix* or a *redefinition*, and it
is a fix, so no major version is forced. Exif 3.0 §4.6.9.2 Table 21 states each 1st IFD tag's
support level per thumbnail-format column: three uncompressed ones distinguished by photometric
interpretation and planar configuration (Chunky, Planar, YCC), plus **Compressed**. That axis is not
the two-valued `Compression` tag. The table gives `JPEGInterchangeFormat` and
`JPEGInterchangeFormatLength` the *same* level in each column: "not allowed to record" under the
three uncompressed columns, mandatory under Compressed. An offset with no length is therefore
non-conformant under every column, and no conformant 1st IFD changes verdict. That grounding is what
this repository ships — the table is vendored under `references/exif/`; the before/after comparison
measured behind it is recorded in the pull request that introduced the report, not committed here as
a harness.

## Scope

v1 covers the **standard CIPA DC-008 tag dictionary** ([`ExifTag`]), full read/write round-trips
over `gamut-ifd`, the typed [`GpsInfo`] projection, and JPEG thumbnails. Intentionally deferred (and
designed to be added without breaking the 1.0 API — the catalogue and vendor enums are
`#[non_exhaustive]`):

- **Per-vendor MakerNote decoding.** The `MakerNote` block is preserved verbatim and its vendor
  detected ([`MakerNoteVendor`]), but not decoded. Since issue #263 a parsed model records the
  note's absolute source offset and the writer **pins** the byte range there on a rewrite, so
  vendor TIFF-absolute internal offsets stay valid; only an unsatisfiable pin (the directory
  region outgrew the old position) falls back to relocation, with the bytes still value-exact.
- **exiftool-parity tag breadth** beyond the standard dictionary (unknown tags still round-trip
  losslessly via the raw `Ifd`).
- **Uncompressed strip-based thumbnails** are read but not re-embedded (JPEG thumbnails are).
- **Per-tag error recovery inside one directory.** A single unparseable entry fails its whole
  directory in `gamut-ifd`, so the report's granularity is the sub-IFD, not the individual tag
  (issue #521).
- **A signal for a shadowed duplicate tag.** Two entries for one tag decode to the last, and the
  earlier one is discarded a layer below this crate. `gamut-exif` could *detect* the loss (compare
  `RawIfd::entries` against the decoded `Ifd::fields()`), but not describe it without re-decoding
  the shadowed entry, so the signal belongs where the discarding happens — a layer three crates
  share (issue #528).
- **A byte-completeness verdict** over the whole blob (which source bytes no parsed structure
  claims). `gamut-ifd`'s audit engine has the machinery; `ReadReport` today reports only what was
  dropped, not what was never reached (issue #521).

## Status

Implemented and released as **v1** (issue #194). See [STATUS.md](STATUS.md).

## License

Licensed under either of MIT or Apache-2.0 at your option.
