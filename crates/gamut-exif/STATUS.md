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
| P9 | §4.6 | `ReadAt` streaming entry point + lenient-drop report (`ReadReport`) | ✅ |
| P10 | §4.6 | Full DC-008 tag breadth + per-tag value shape + value descriptions (issue #417) | ✅ |

## P10 — what the catalogue now covers

`ExifTag` carries **160** tags: every one of the **151** CIPA DC-008 defines — Table 6 (30, 0th
IFD), Tables 8 and 9 (88, Exif sub-IFD), Table 14 (32, GPS), Table 16 (1, Interoperability) — plus
nine carried from other specifications for compatibility (`ApplicationNotes`, `IPTC-NAA`,
`InterColorProfile`, `Rating`, `RatingPercent`, and the four DCF-era Interoperability tags beyond
`InteroperabilityIndex`).

Each row also carries that table's `Type` and `Count` columns, as `ExifTag::field_types` and
`ExifTag::component_count`. For the nine tags DC-008 does not define, `field_types` is **empty** —
no constraint is claimed rather than one invented. `set_tag_checked` enforces them on the **write
path only**; the reader and `Exif::set_tag` are unchanged and stay lenient.

The `describe` feature (default **off**) renders the enumerated tags in DC-008's own wording, and
`flash` decomposes the `Flash` bitfield of §4.6.6.7.21 Figure 17.

### Where exiv2 and CIPA DC-008 disagree on a name

`tests/oracle.rs` asks exiv2 for `Exif.<group>.<name>` for all 160 catalogued tags. Three do not
resolve under gamut's name. DC-008 is the specification this crate implements and exiv2 is the
oracle, not the source, so gamut keeps the DC-008 reading in each case; the test pins the set
exactly, and a second test proves each is a naming difference rather than a missing tag.

| Tag | gamut / this crate's source | exiv2 0.28 |
| --- | --- | --- |
| `0x8827` | `PhotographicSensitivity` (DC-008, renamed in Exif 2.3) | `ISOSpeedRatings` (Exif 2.2) |
| `0x02BC` | `ApplicationNotes` (TIFF/EP lineage) | `XMLPacket` (the XMP specification's name) |
| `0x83BB` | `IPTC-NAA` (TIFF/EP lineage, hyphenated) | `IPTCNAA` |

## Intentionally deferred (additive under the `#[non_exhaustive]` surface)

- **Per-vendor MakerNote decoding** (Canon/Nikon/Sony/…). The crate preserves the block verbatim
  — and, since issue #263, pins its byte range at the source offset on rewrites so
  vendor-absolute internal offsets stay valid — and detects the vendor from `Make`, but does not
  decode the block (documented on `MakerNote`).
- **Tag breadth beyond any vendored specification.** exiv2 knows 408 standard tags to gamut's 160.
  The catalogue is complete for CIPA DC-008 (P10 above), so the whole difference is tags DC-008
  does not define — TIFF/EP, DNG, and other lineages. Adding them means vendoring *their*
  specifications and transcribing from those, not copying a table out of the oracle, so they are a
  separate deliverable. Unknown and MakerNote tags still round-trip losslessly because the raw
  `gamut_ifd::Ifd` is retained.
- **A rendered-value differential.** `tooling/exiv2-oracle` exposes `Exiv2::Exifdatum::toString()`
  (the raw value) but not `print()` (exiv2's *interpreted* value), so the `describe` tables are
  transcribed from CIPA DC-008 and checked structurally — each table ascending and distinct,
  `describe` exactly a lookup into `described_values`, and each `Flash` bit field a function of its
  own bits only — but not against exiv2's rendering. Exposing `print()` in the oracle shim would
  make that differential possible.
- **Uncompressed strip-based thumbnails** are read (as their directory) but not re-embedded; JPEG
  thumbnails round-trip fully.
- **Per-tag error recovery inside a directory.** `ExifReader::parse_with_report` names every
  sub-IFD and thumbnail range the lenient reader discards (issue #419), but the granularity is the
  directory: a single unparseable entry fails its whole IFD in `gamut-ifd`, which is the layer that
  would have to recover per entry. nom-exif's `entry.into_result()` is finer-grained here.
- **A byte-completeness verdict.** `ReadReport` says what was *dropped*, not which source bytes no
  parsed structure claims. `gamut-ifd`'s audit engine (`Tracked`, `SegmentMap`, `read_audited`) is
  the machinery for it and is already used by `gamut-dng` and `gamut-tiff`; wiring it behind a
  toggle on `ExifReader` stays a non-breaking future addition.
- **An async entry point.** Declined rather than deferred: `parse_from` is synchronous over
  `gamut_ifd::ReadAt`, and an async caller drives that source itself. A `tokio` feature would put a
  runtime dependency in a crate that has none and constrain the public shape against the
  C-portability convention, for a capability the caller can supply.
