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

### Where the nine non-DC-008 tags come from

The workspace rule is that a row comes from a specification under `references/`. Three of the nine
do; six do not, and are **residuals** — carried because real files and every other reader use them,
with no vendored text fixing their type or count, which is exactly why `field_types` is empty.

| Tag | Vendored source |
| --- | --- |
| `ApplicationNotes` `0x02BC` | `references/xmp/xmp-part3.pdf` — its TIFF table gives `700 / 0x2BC — XMP packet` |
| `IPTC-NAA` `0x83BB` | `references/xmp/xmp-part3.pdf` — same table, `33723 / 0x83BB — IPTC dataset`; the payload's own format is `references/iptc/iim-4.2.pdf` |
| `InterColorProfile` `0x8773` | `references/icc/icc.1-2001-04.pdf` — "The TIFFTag that identifies the field = 34675(8773.H)" |
| `InteroperabilityVersion` `0x0002`, `RelatedImageFileFormat` `0x1000`, `RelatedImageWidth` `0x1001`, `RelatedImageLength` `0x1002` | **Residual.** DC-008 3.0 *names* all four in its original-preservation-image annex but delegates them to "Table 13, 'Interoperability IFD Description Support Levels,' of DCF[2], section 4.7". DCF (CIPA DC-009) is not vendored, so no text here fixes their type or count. |
| `Rating` `0x4746`, `RatingPercent` `0x4749` | **Residual.** Microsoft's Windows photo-metadata extension; no vendored specification at all. |

Vendoring DCF would settle four of the six. All nine predate this phase — P10 only made their
unspecified status explicit and machine-readable.

The `describe` feature (default **off**) renders the enumerated tags in DC-008's own wording, and
`flash` decomposes the `Flash` bitfield of §4.6.6.7.21 Figure 17. The rule it implements is that a
tag gets a table exactly when DC-008 fixes the meaning of a **single scalar code** — one integer,
or one ASCII character — that the value carries on its own, or, for `ComponentsConfiguration`, that
each of its four elements carries on its own. That selects **39** tags — 26 in the 0th IFD and Exif
sub-IFD, 13 in the GPS sub-IFD, of which eleven are the **character-coded** ASCII tags
(`GPSStatus`, `GPSMeasureMode` and the nine reference tags of §4.6.7.1) whose code is the letter's
byte rather than an integer.

The spec printing a table is neither sufficient nor necessary for membership, so the count is not
simply "every tag with a table". Four tags whose sections print one are **excluded**, because what
those tables enumerate is not a scalar code: `Flash` §4.6.6.7.21 (a bitfield, decomposed by `flash`
instead), `GPSVersionID` §4.6.7.1.1 and `FlashpixVersion` §4.6.6.1.2 (fixed multi-byte versions,
not domains), `YCbCrSubSampling` §4.6.5.1.12 (a pair whose meaning belongs to its two elements
jointly) and `InteroperabilityIndex` §4.6.8.1.1 (multi-character ASCII codes). One tag with no
table of its own is **included**, the deliberate exception: `FocalPlaneResolutionUnit` §4.6.6.7.28,
whose section defines it as "the same as the ResolutionUnit" (§4.6.5.1.11) rather than restating
the values, so it shares that arm. A test names the resulting set exactly, so a table added for a
tag the spec leaves open, or an arm that loses its rows, fails. Only a crate depending on `gamut-exif` directly can enable the feature: neither
`gamut-metadata` nor the `gamut` umbrella forwards it yet (issue #543).

### Where CIPA DC-008 disagrees with itself

`GainControl` (`0xA407`): Table 9's `Type` column says `RATIONAL`, but §4.6.6.7.41 says `SHORT`
and enumerates five integer codes (0–4). The tag's own section wins — a fraction cannot carry an
enumeration — and both exiv2 and ExifTool read it as `SHORT`. Every other row was transcribed from
the summary tables; a mechanical cross-check of all 149 specified rows against the per-tag
`Tag = … / Type = … / Count = …` blocks found this to be the only contradiction.

### Where exiv2 and CIPA DC-008 disagree on a name

`tests/oracle.rs` asks exiv2 for `Exif.<group>.<name>` for all 160 catalogued tags. Three do not
resolve under gamut's name. DC-008 is the specification this crate implements and exiv2 is the
oracle, not the source, so gamut keeps the DC-008 reading in each case; the test pins the set
exactly, and a second test proves each is a naming difference rather than a missing tag.

| Tag | gamut / this crate's source | exiv2 (vendored v0.28.8) |
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
  make that differential possible (issue #533).
- **A re-derivation of the `Type`/`Count` columns.** They were transcribed by hand from the
  vendored specification and cross-checked once against the per-tag prose blocks, but nothing in
  the repository recomputes them, and mutation testing cannot see a wrong constant in a macro
  invocation. A *missing* row is caught by the structural guards; a *corrected-but-different* value
  is caught by nothing (issue #544).
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
