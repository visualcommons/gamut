# gamut-exif — EXIF implementation status

Part of the **image metadata primitives** campaign (GitHub issue #34). Implements EXIF
(`references/exif`, Exif 3.0 / CIPA DC-008) on top of the shared [`gamut-ifd`](../gamut-ifd)
TIFF/IFD core. Released as **v1** (issue #194).

**Keystone (done):** the **writer round-trip** — re-emitting a valid `Exif\0\0` + TIFF blob through
`gamut-ifd`'s offset-patching writer with the Exif/GPS/Interop sub-IFD pointers, JPEG or
uncompressed strip thumbnail, and source byte order intact. `parse → to_bytes → parse` is value-level identical in both byte orders.

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
| P11 | §4.6 | Read-report depth (issue #521): opt-in per-entry recovery (`ExifReader::recover_entries` → `ReadReport::dropped_entries`, over `gamut_ifd::IfdReader::decode_ifd_lenient`), opt-in byte-completeness verdict (`ExifReader::audit_bytes` → `ReadReport::byte_audit`), real-camera-DNG laziness case (`tooling/gamut-dng-real-conformance/tests/exif_laziness.rs`, extended tier) | ✅ |
| P12 | §4.6 | Shadowed duplicate tags (issue #719): every directory the model keeps is decoded through `gamut_ifd::IfdReader::decode_ifd_reporting` (or `decode_ifd_lenient` under recovery), and each entry a repeated tag shadowed is a `ReadReport::dropped_entries` record with `EntryDropReason::Shadowed`, in every mode | ✅ |
| P13 | §4.6.5.2, §4.6.9.2 | Unreadable strip thumbnails (issue #868): an integer `StripOffsets` naming strips that cannot be read is a `DroppedRegion::ThumbnailStrips` drop (`DropReason::StripsUnsized` / `OutOfBounds` / `StripsOverlap`) and a strict-mode `ExifError::BadThumbnail`, and `StripOffsets` leaves the model whenever its strips do not, so the writer never re-emits offsets addressing nothing | ✅ |
| P10 | §4.6 | Full DC-008 tag breadth + per-tag value shape + value descriptions (issue #417) | ✅ |
| P14 | — | MakerNote dispatch layer (issue #968, phase 0 of #418): `MakerNoteLayout::detect` / `Exif::maker_note_layout` resolve the dialect, IFD byte order and offset, next-IFD pointer and `OffsetBase` for 19 dialects across 13 vendors, after exiv2's `TiffMnCreator` table plus Apple (`references/exif/README.md`); no vendor tag decoded | ✅ |
| P15 | — | Canon MakerNote view (issue #977, a vendor phase of #418): `MakerNote::canon` / `CanonMakerNote::read` decode the note's IFD through `gamut-ifd` at the `Canon` layout (file-absolute offsets), keep every entry, and type the eleven tags staked in `references/exif/canon/README.md`; `tests/maker_note_canon.rs` holds each to exiv2's `exif_get`, the binary arrays element by element; values are raw (no `exif_print`-style interpretation) and the other binary arrays stay unsplit | ✅ |

## P10 — what the catalogue now covers

`ExifTag` carries **160** tags: every one of the **151** in the five CIPA DC-008 tables that define
one — Table 6 (30, 0th IFD), Tables 8 and 9 (88, Exif sub-IFD), Table 14 (32, GPS), Table 16 (1,
Interoperability) — but not the three IFD-pointer tags DC-008 defines in their own sections outside
those tables (§4.6.3.1.1, §4.6.3.2.1, §4.6.3.3.1; see `tag.rs` for why), plus
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
simply "every tag with a table". Five tags whose sections print one are **excluded**, because what
those tables enumerate is not a scalar code: `Flash` §4.6.6.7.21 (a bitfield, decomposed by `flash`
instead), `GPSVersionID` §4.6.7.1.1 and `FlashpixVersion` §4.6.6.1.2 (fixed multi-byte versions,
not domains), `YCbCrSubSampling` §4.6.5.1.12 (a pair whose meaning belongs to its two elements
jointly) and `InteroperabilityIndex` §4.6.8.1.1 (multi-character ASCII codes). One tag with no
table of its own is **included**, the deliberate exception: `FocalPlaneResolutionUnit` §4.6.6.7.28,
whose section defines it as "the same as the ResolutionUnit" (§4.6.5.1.11) rather than restating
the values, so it shares that arm. A test names the resulting set exactly, so a table added for a
tag the spec leaves open, or an arm that loses its rows, fails. Only a crate depending on
`gamut-exif` directly can enable the feature: neither
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

### Where exiv2 and CIPA DC-008 disagree on a value

`tests/oracle_describe.rs` (issue #533) checks the `describe` tables against exiv2's interpreted
rendering, `Exifdatum::print` through `exiv2_oracle::exif_print`. For each of the 39 described tags
it writes every value the tag's field can hold — each 7-bit character, byte or `SHORT` — and
records which codes exiv2 interprets and how it words them. DC-008 wins wherever the two differ, as
for names above; the test pins both kinds of difference exactly, so a transcription that invents,
drops or swaps a code fails, and so does a divergence that goes away.

**Domain** — the codes only one side defines:

| Tag | exiv2 only | DC-008 only | Why |
| --- | --- | --- | --- |
| `Compression` | 32 codes | — | exiv2 reads it as TIFF's tag (CCITT, LZW, deflate, JBIG, DNG, vendor raw); §4.6.5.1.4 restricts Exif to 1 and 6 |
| `PhotometricInterpretation` | 14 codes | — | likewise TIFF's and DNG's; §4.6.5.1.5 restricts Exif to RGB and YCbCr |
| `ResolutionUnit`, `FocalPlaneResolutionUnit` | 1 (`none`) | — | TIFF 6.0's value; DC-008 §4.6.5.1.11 reserves it |
| `ColorSpace` | 2 (`Adobe RGB`) | — | assigned by neither vendored edition |
| `SensitivityType` | — | 0–7 | exiv2 has no interpreter for the tag and prints the number |
| `LightSource` | — | 16 (`Warm white fluorescent`) | in both vendored editions (2.32, 3.0); exiv2's table stops at 15 |
| `FileSource` | — | 0 (`others`) | in both vendored editions; exiv2's table starts at 1 |
| `GPSAltitudeRef` | — | 2, 3 (sea-level references) | new in Exif 3.0; exiv2 0.28 keeps 2.32's two codes |

**Wording** — 59 codes both define, worded differently even ignoring case. Most are exiv2
abbreviating (`inch`, `km/h`, `North`) or dropping DC-008's gloss (`Creative program`); the
substantive ones are `Orientation` (exiv2 names the corner, `top, left`, where §4.6.5.1.6 states the
row and column in full), `GPSAltitudeRef` 0 and 1 (exiv2 keeps 2.32's sea-level reading; 3.0 made
them ellipsoidal), and `LightSource` 13–15, whose colour-temperature ranges in exiv2 match neither
vendored edition. The full list, each with its reason, is `WORDING_DIVERGENCES` in the test.

What the differential cannot see is a swap between two codes exiv2 already words differently (say
`Orientation` 1 and 2): both stay divergent against the same exiv2 text. Those rows rest on the
transcription from DC-008 and the structural checks in `src/describe.rs`.

## Intentionally deferred (additive under the `#[non_exhaustive]` surface)

- **Per-vendor MakerNote tag decoding** beyond Canon (Nikon/Sony/…; the per-vendor sub-issues of
  #418, #976 and #978–#988, plus #989 for the note Adobe DNG Converter moves into `DNGPrivateData`,
  which is also what Canon's DNG corpus coverage waits on).
  The crate preserves the block verbatim — and, since issue #263, pins its byte range at the
  source offset on rewrites so vendor-absolute internal offsets stay valid — detects the vendor
  from `Make`, and since P14 resolves the block's dispatch layout; since P15 it decodes Canon's
  tags, and no other vendor's (documented on `MakerNote`). Within Canon, the interpretation of
  values and the binary arrays beyond `CameraSettings`/`ShotInfo` are not promised
  (`references/exif/canon/README.md`). Leica, Kodak and the other vendors outside exiv2's table have no
  dialect yet, so their notes have no layout and stay opaque.
- **Tag breadth beyond any vendored specification.** exiv2 knows 408 standard tags. The catalogue
  is complete for CIPA DC-008 (P10 above), and since issue #532 the 0th IFD's TIFF 6.0 (44 tags
  beyond DC-008 Table 6) and DNG 1.7.1.0 (118) tags are named through `IfdKind::registry_spec`
  from the shared `gamut_ifd::tags` registry, which gamut-tiff and gamut-dng read too;
  `tests/oracle.rs` holds those rows to exiv2's names (one spelling divergence, `Threshholding`,
  and four baseline TIFF 6.0 tags exiv2 has no entry for). The remaining difference is tags no
  vendored specification defines — TIFF/EP, the Adobe technical notes, and the vendor lineages —
  and stays deferred until a specification is staked for each; a table copied out of the oracle
  is not a source. Unknown and MakerNote tags still round-trip losslessly because the raw
  `gamut_ifd::Ifd` is retained.
- **A derivation of the `Type`/`Count` columns from the PDF itself.** Since issue #544 the
  catalogue's `Hex`/`Type`/`Count` columns are checked in both directions against
  `tests/fixtures/dc008_tag_columns.txt`, a second, independent transcription of DC-008 Tables 6,
  8, 9, 14 and 16 in the spec's own layout (`tests/tag_spec_columns.rs`), with the one table/prose
  contradiction (`GainControl`: Table 9 `RATIONAL`, §4.6.6.7.41 `SHORT`) held in a `DEPARTURES`
  list that fails once it stops being live. A *corrected-but-different* value in either copy is
  therefore a test failure. What remains is that the fixture is itself a hand transcription:
  nothing re-reads the vendored PDF, so an error made identically in both copies would still pass.
- **An async entry point.** Declined rather than deferred: `parse_from` is synchronous over
  `gamut_ifd::ReadAt`, and an async caller drives that source itself. A `tokio` feature would put a
  runtime dependency in a crate that has none and constrain the public shape against the
  C-portability convention, for a capability the caller can supply.
