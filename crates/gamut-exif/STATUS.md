# gamut-exif — EXIF implementation status

Part of the **image metadata primitives** campaign (GitHub issue #34). Implements EXIF
(`references/exif`, Exif 3.0 / CIPA DC-008) on top of the shared [`gamut-ifd`](../gamut-ifd)
TIFF/IFD core. Released as **v1** (issue #194).

**Keystone (done):** the **writer round-trip** — re-emitting a valid `Exif\0\0` + TIFF blob through
`gamut-ifd`'s offset-patching writer with the Exif/GPS/Interop sub-IFD pointers, JPEG thumbnail, and
source byte order intact. `parse → to_bytes → parse` is value-level identical in both byte orders.

**Oracle:** differential vs **exiv2** (`tooling/exiv2-oracle`, dev-only FFI over
`Exiv2::ExifParser::decode/encode`) for read/round-trip parity, plus committed **byte-level golden
fixtures** (`tests/fixtures/`, regenerate with `GAMUT_REGEN_GOLDEN=1`).

## Phases

| Phase | Spec § | Scope | Status |
| ----- | ------ | ----- | ------ |
| P1 | — | Scaffold: crate, workspace wiring, docs, region-free data-model skeleton | ✅ |
| P2 | §4.6 | Marker + IFD traversal over `gamut-ifd`: 0th IFD + Exif/GPS/Interop sub-IFD pointers | ✅ |
| P3 | §4.6 | Typed value access (`Rational`/`SRational`/`as_text`) + Exif 3.0 UTF-8 (type 129) | ✅ |
| P4 | §4.6 | Standard CIPA DC-008 tag dictionary (`ExifTag`, macro-table-driven) | ✅ |
| P5 | §4.6.6 | GPS typed model + thumbnail (1st IFD) extraction & JPEG re-embed | ✅ |
| P6 | §4.6 | **Keystone** — writer round-trip (endianness/pointers/thumbnail preserved) | ✅ |
| P7 | §4.6 | MakerNote: opaque passthrough + vendor detection (no per-vendor decode) | ✅ |
| P8 | — | exiv2 differential gate + golden fixtures | ✅ |
| P9 | §4.6 | `ReadAt` streaming entry point + lenient-drop report (`ReadReport`, scoped to the regions `DroppedRegion` names) | ✅ |

## Intentionally deferred (additive under the `#[non_exhaustive]` surface)

- **Per-vendor MakerNote decoding** (Canon/Nikon/Sony/…). The crate preserves the block verbatim
  — and, since issue #263, pins its byte range at the source offset on rewrites so
  vendor-absolute internal offsets stay valid — and detects the vendor from `Make`, but does not
  decode the block (documented on `MakerNote`).
- **exiftool-parity tag breadth** beyond the standard dictionary. Unknown and MakerNote tags still
  round-trip losslessly because the raw `gamut_ifd::Ifd` is retained.
- **Uncompressed strip-based thumbnails** are read (as their directory) but not re-embedded; JPEG
  thumbnails round-trip fully.
- **Per-tag error recovery inside a directory.** `ExifReader::parse_with_report` names the
  sub-IFDs, thumbnail ranges and trailing top-level directories the lenient reader discards (issue
  #419), but the granularity is the directory: a single unparseable entry fails its whole IFD in
  `gamut-ifd`, which is the layer that would have to recover per entry. nom-exif's
  `entry.into_result()` is finer-grained here.
- **A signal for a shadowed duplicate tag.** `gamut_ifd::IfdReader::decode_ifd` builds a directory
  with `Ifd::set`, which is last-wins, so two entries for one tag decode to the second and the
  first is discarded. `ReadReport::is_empty()` is therefore a verdict over the regions it covers,
  **not** "this parse lost nothing"; both the report's module docs and the README say so.

  The deferral is a layering decision, **not** an inability to observe: `RawIfd::entries` is public
  and in on-disk order and `follow` already holds the `RawIfd`, so `raw.entries.len() !=
  ifd.fields().len()` would detect a shadowed tag here in three lines. What `gamut-exif` cannot do
  is say *what* was lost without re-decoding the shadowed entry — and `gamut-tiff` and `gamut-dng`
  need the same signal, so it belongs in the shared layer (issue #528). Issue #528's own body
  states the weaker, incorrect reason; read it with this correction.
- **A byte-completeness verdict.** `ReadReport` says what was *dropped*, not which source bytes no
  parsed structure claims. `gamut-ifd`'s audit engine (`Tracked`, `SegmentMap`, `read_audited`) is
  the machinery for it and is already used by `gamut-dng` and `gamut-tiff`; wiring it behind a
  toggle on `ExifReader` stays a non-breaking future addition.
- **An async entry point.** Declined rather than deferred: `parse_from` is synchronous over
  `gamut_ifd::ReadAt`, and an async caller drives that source itself. A `tokio` feature would put a
  runtime dependency in a crate that has none and constrain the public shape against the
  C-portability convention, for a capability the caller can supply.
