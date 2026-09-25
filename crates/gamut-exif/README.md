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

Every tag in the five CIPA DC-008 tables that define one (Table 6, Tables 8 and 9, Table 14 and
Table 16) carries the field type and component count that table mandates for it
([`ExifTag::field_types`], [`ExifTag::component_count`]) — the three IFD-pointer tags DC-008 defines
outside those tables are not catalogued (see [Scope](#scope)); the nine carried from
other specifications claim no constraint, and their `field_types` is empty. [`set_tag_checked`] is
the conformant setter — it refuses a value that contradicts them — and it is the crate's only
checked door. The boundary is doors that admit a **field**: there are nine, and all nine stay
lenient — [`Exif::set_tag`], [`Exif::set`], `image_mut`, `exif_ifd_mut`, `gps_ifd_mut`,
`interop_ifd_mut`, `set_exif_ifd`, `set_gps_ifd` and `set_interop_ifd` — as does the whole read
path, because a caller reproducing a non-conformant source file must still be able to.
`set_thumbnail` sits outside that boundary: it admits bytes rather than a field, and does not check
that they are a JPEG.

```rust
use gamut_exif::{ByteOrder, Exif, ExifTag, Value, set_tag_checked};

let mut exif = Exif::new(ByteOrder::LittleEndian);
let wrong = set_tag_checked(&mut exif, ExifTag::FNumber, Value::Short(vec![28]));
assert_eq!(
    wrong.unwrap_err().to_string(),
    "FNumber: CIPA DC-008 requires RATIONAL, not SHORT",
);
```

Enable the optional `describe` feature (also included by `full`) for the enumerated tags' meanings
in the specification's own wording — `describe(ExifTag::ResolutionUnit, 2)` is `Some("inches")`,
`described_values` gives a tag's whole defined domain, and `flash` decomposes the `Flash` bitfield.
It is off by default: a display table is several kilobytes of static strings that a consumer which
only writes or round-trips metadata never reads. Only a crate that depends on `gamut-exif`
directly can turn it on — neither the `gamut-metadata` facade nor the `gamut` umbrella forwards it
yet (issue #543).

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

v1 covers the **standard CIPA DC-008 tag dictionary** ([`ExifTag`]) — every tag in the five tables
that define one (Table 6 for the 0th IFD, Tables 8 and 9 for the Exif sub-IFD, Table 14 for GPS and
Table 16 for Interoperability), with the field type and component count that table mandates for
each, plus nine carried from other specifications for compatibility. The three IFD-pointer tags
(`Exif IFD Pointer`, `GPS Info IFD Pointer`, `Interoperability IFD Pointer`), which DC-008 gives
their own sections outside those tables, are deliberately **not** catalogued: the writer
synthesises them from the tree it is given and drops any hand-set in the directory the
specification puts it in — the Exif and GPS pointers from the 0th IFD, the Interoperability pointer
from the Exif sub-IFD — so a name for them would only invite a write that is silently discarded.
(One written into some other directory is not one the writer looks for: it survives as an ordinary
field that no accessor reads.) The crate also gives full read/write
round-trips over `gamut-ifd`, the typed [`GpsInfo`] projection, and JPEG thumbnails. Intentionally
deferred (and designed to be added without breaking the 1.0 API — the catalogue and vendor enums
are `#[non_exhaustive]`):

- **Per-vendor MakerNote decoding.** The `MakerNote` block is preserved verbatim and its vendor
  detected ([`MakerNoteVendor`]), but not decoded. Since issue #263 a parsed model records the
  note's absolute source offset and the writer **pins** the byte range there on a rewrite, so
  vendor TIFF-absolute internal offsets stay valid; only an unsatisfiable pin (the directory
  region outgrew the old position) falls back to relocation, with the bytes still value-exact.
- **Tag breadth beyond any vendored specification.** exiv2 knows 408 standard tags to gamut's 160;
  the difference is entirely tags CIPA DC-008 does not define (TIFF/EP, DNG, and other lineages),
  which would have to be transcribed from *their* specifications, not copied out of an oracle.
  Unknown tags still round-trip losslessly via the raw `Ifd`.
- **A re-derivation of the per-tag `Type`/`Count` columns.** They are transcribed by hand from the
  vendored specification and guarded structurally, but nothing recomputes them from the spec, so a
  *corrected-but-different* value would compile and pass (issue #544).
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
