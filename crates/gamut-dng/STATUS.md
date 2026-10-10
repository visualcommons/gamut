# gamut-dng — DNG 1.7.1 implementation status

Tracking GitHub issue #109: a feature-complete **DNG (Digital Negative) 1.7.1** encoder **and**
decoder (`references/dng/DNG_Spec_1_7_1_0.pdf`). DNG is a TIFF/EP-based raw-image format, so the
container spine is the shared `gamut-ifd` primitive; this crate adds the DNG-specific tags, raw
photometry, compression, colour calibration, and metadata on top. Delivered as a stack of small,
individually-green phases; each is green (`mise run test`/`lint`/`fmt-check`/`coverage` ≥80%).

**Keystone:** DNG's defining structure is an IFD *tree* — IFD0 (a preview/thumbnail) points, via
the `SubIFDs` tag (330), at the full-resolution raw image in a **sub-IFD**, with EXIF in another
(`ExifIFD` 34665). `gamut-ifd`'s writer only linked a flat IFD *chain*, so the first job (P2) was
sub-IFD **tree layout** — recursive two-pass absolute-offset assignment with pointer-tag patching.

**Oracle:** the authoritative **Adobe DNG SDK 1.7.1** (`references/dng/`), built headless via the
`cc` crate into the dev-only `tooling/gamut-dng-oracle` (XMP stubbed; system zlib; **real libjxl
0.12.0** statically linked via `gamut-jxl-sys`, so the SDK genuinely decodes JPEG XL DNGs),
exposing `extern "C"` validate / read-stage-1 / read-stage-2 / digest entry points around
`dng_validate`'s call sequence, plus the SDK ZIP's 14 official `sample_files/*.dng` as
decode-conformance inputs. gamut-encode → `dng_validate` must accept the file (raw digest
included); Adobe sample DNGs → gamut-decode must agree with the SDK. A DNG is also a valid TIFF,
so the existing `libtiff-oracle` cross-checks the container/strips **pixel-exactly**, and
internal encode→decode round-trips guard every lossless path.

## Phases

| Phase | DNG § | Scope | Status |
| ----- | ----- | ----- | ------ |
| P1  | —       | Scaffold: crate, workspace + umbrella wiring, README, region-free skeleton | ✅ done |
| P2  | —       | **Keystone** `gamut-ifd`: sub-IFD tree writer + pointer patching + `read_ifd_at` | ✅ done |
| P3  | Ch3     | DNG tag + value tables (`tags`, `values`) from the SDK headers | ✅ done |
| P4  | Ch2–5   | **Keystone** uncompressed CFA DNG: IFD0 preview + raw sub-IFD, mandatory tags, strips, II/MM | ✅ done |
| P5  | —       | `tooling/gamut-dng-oracle`: auto-extract + `cc`-build SDK + `extern "C"` shim | ✅ done |
| P6  | —       | Adobe oracle gate on: gamut-encode → `dng_validate`; libtiff IFD-0 cross-check | ✅ done |
| P7  | Ch4     | `LinearRaw` photometric (demosaiced RGB), samples-per-pixel / photometric handling | ✅ done |
| P8  | Ch6     | Colour & calibration: ColorMatrix1/2, CameraCalibration, ForwardMatrix, illuminants, AnalogBalance, BaselineExposure, profile name/policy + `CameraProfile` API | ✅ done |
| P9  | Ch5     | Levels (Black/White) + ActiveArea + DefaultCrop + **bit-depth packing 8/10/12/14/16** (MSB-first, Adobe-verified pixel-exact). Completed by #253: the full spec level model (`RawLevels`) and the chapter-5 mapping itself (`RawImage::to_linear`, gated ±1 LSB against the Adobe SDK's stage-2 image) | ✅ done |
| P10 | Ch2     | Embedded uncompressed RGB preview in IFD 0 (JPEG preview + size cap deferred) | ✅ done |
| P11 | Ch2–5   | **Decoder**: walk the tree (SubIFDs → raw), unpack samples, reconstruct RawImage + CameraProfile; round-trips & agrees with Adobe. Finalization hardened it: full IFD-forest raw search, per-strip sub-byte alignment, `SampleFormat` validation, 64-bit offset reads | ✅ done |
| P12 | Ch4     | Deflate/ZIP (8) encode+decode (zlib format; encode `gamut-deflate`, decode `miniz_oxide`) — CFA + LinearRaw, Adobe-validated; encode limited to 8/16-bit (the SDK reader's constraint) | ✅ done |
| P13 | Ch4     | Lossless JPEG (7) encode+decode (SOF3) — CFA + LinearRaw, Adobe decodes pixel-exact. #253 hardened decode to the full T.81 process-14 reader envelope and published the `lossless_jpeg` module; per-chunk geometry follows the spec's total-sample-count rule | ✅ done |
| P14 | Ch2     | Tiled raw layout (`TileWidth`/`TileLength`/`TileOffsets`/`TileByteCounts`): decode with edge-crop reassembly + `with_tiling` encode (zero-padded edge tiles), all schemes, Adobe pixel-exact | ✅ done |
| P15 | Ch2     | BigTIFF DNG (1.7, 64-bit offsets) — encode + decode, Adobe-validated | ✅ done |
| P16 | Ch8–9   | Metadata: EXIF sub-IFD + XMP (700) / IPTC (33723) / ICC (34675) — embed + decode, Adobe-validated. #353 replaced the crate-local models with the workspace facade's: `DngMetadata::exif` is `gamut-exif`'s `Exif`, and the byte carriers hand over as `gamut-metadata` `MetadataBlock`s | ✅ done |
| P17 | Ch2     | Digests: the encoder writes `NewRawImageDigest` (51111), bit-matching the SDK's `FindNewRawImageDigest` (256×256 digest tiles, planar LE serialisation, the ≤256-entry-table byte mode); `RawImage::new_raw_image_digest` is public and the decoder surfaces the stored digest | ✅ done |
| P18 | Ch7     | `OpcodeList1/2/3` container + standard opcode library. Container done via #253: typed `OpcodeList`/`Opcode` parse + pass-through write + `DNGBackwardVersion` raising; the standard opcode *processing* library remains deferred | ◑ partial |
| P19 | —       | Finalization: JPEG XL, sub-images, gain maps, extra-tag explicitness, version auto-computation, docs + API freeze (v1.0.0) | ✅ done |
| P20 | Ch3–4   | **JPEG XL** (Compression 52546, DNG 1.7): decode always available (pure-Rust jxl-rs; bare codestream + container, full-range 16-bit per the SDK's semantics, fp16 rejected typed); encode behind the `jxl-encode` feature (libjxl; lossless or lossy, `JXLDistance`/`JXLEffort` written); `RowInterleaveFactor`/`ColumnInterleaveFactor` de-interleave on decode | ✅ done |
| P21 | Ch2,4   | **Sub-images**: every non-raw image IFD as a typed `SubImage` (previews, transparency masks, **semantic masks** with `SemanticName`/`SemanticInstanceID`/`MaskSubArea`, depth maps + `DepthInfo`), best-effort decoded with verbatim-chunk fallback | ✅ done |
| P22 | Ch4     | **Gain maps**: typed `ProfileGainTableMap` (52525) + `ProfileGainTableMap2` (52544) — parse, byte-exact re-serialise, embed on encode; gated against Adobe's PGTM sample files | ✅ done |
| P23 | —       | **Explicitness**: every unmodelled IFD field surfaces verbatim as a typed `RawTag` (`ifd0_extra`/`raw_extra`/per-sub-image), via a consumption-tracking reader — issue #109's "all metadata explicitly represented" clause. The EXIF sub-IFD needs no extras list of its own since #353: it arrives whole, inside `Exif` | ✅ done |
| P24 | Ch2-4   | **Real camera conformance** (#174): the `gamut-dng-samples` corpus + `tooling/gamut-dng-real-conformance`; `Predictor`/`PlanarConfiguration` honoured, byte accounting and the preserving rewrite fixed for real files, optional camera profile | ✅ done |
| P25 | Ch6     | **Colour projection** (#353): the camera-profile colour tags `CameraProfile` does not model — hue/sat and look tables (dims/data/encoding), `ProfileToneCurve`, `BaselineExposureOffset`, the DNG 1.6 third calibration set, `ReductionMatrix1/2/3` — as a typed read-direction `ColorProfileInfo`, plus the raw IFD's `NoiseProfile` as a typed `NoiseProfile`; a value outside the spec's domain stays in the extras | ✅ done |
| P26 | C2PA §A.3.6, §18.5.5 | **C2PA manifest store** (#442): typed on both sides (`DngMetadata::c2pa`), entry in IFD 0 with the value last in the file, a zero-filled reservation (`with_c2pa_reserved`), both exclusion ranges reported (`encode_with_report` / `DecodedDng::c2pa_exclusions`), bytes verbatim in either byte order, Adobe-validated | ✅ done |

## Apple ProRAW (DNG 1.7 + JPEG XL): fully covered for decode

A ProRAW-with-JXL DNG (iPhone 15/16 Pro era) is a DNG 1.7.0.0 linear DNG. Every ingredient maps
to a shipped, oracle-gated feature:

| ProRAW ingredient | Coverage |
| ----------------- | -------- |
| DNG 1.7.0.0 / backward 1.3+ | version parsing + typed `backward_version` (P11/P23) |
| `LinearRaw`, 3 samples/pixel | P7 |
| Tiled layout (e.g. 2016×2016) | P14 |
| Compression 52546 (JPEG XL), bare codestreams | P20 — full-range 16-bit decode, matching the reference SDK (Apple pairs `BitsPerSample = 10` with `WhiteLevel = 65535`) |
| `LinearizationTable`, per-plane Black/WhiteLevel | P9 |
| Semantic-mask sub-IFDs (PhotometricMask 52527, JXL) | P21 (+P20) |
| `ProfileGainTableMap` | P22 |
| JPEG preview in IFD 0 | P21 (decoded when in scope, verbatim chunks otherwise) |
| Apple maker tags | P23 (verbatim typed `RawTag`s) |
| `NewRawImageDigest` | P17 |

Conformance uses Adobe's official JPEG XL sample DNGs (tiled, interleaved, lossy) — gamut's
decode agrees with the SDK's own real-libjxl decode within one code (JXL conformance tolerance
for lossy streams; lossless is bit-exact) — plus ProRAW-shaped synthetic goldens through the
full encode → SDK-validate → decode → digest loop. Since #174 a **real iPhone 12 Pro ProRAW
file** is gated too (below), which is what turned the table above from a mapping argument into a
measurement.

## Real camera conformance (issue #174)

Every input above is either synthetic or Adobe-authored. **Real camera files are a different
population**, and running six of them through the decoder found four defects nothing else could
have: two produced silently wrong output, one dropped 651 KB, one refused a valid file outright.

**Corpus:** the `gamut-dng-samples` submodule at `third_party/gamut-dng-samples` — six CC0 files
from raw.pixls.us, each verified CC0 by SHA-256 against the upstream index, kept byte-identical
to upstream (cropping would destroy the byte-completeness and digest properties they exist to
test). `MANIFEST.toml` carries provenance plus *measured* expectations, so drift fails rather
than passes quietly.

**Harness:** `tooling/gamut-dng-real-conformance`, excluded from the workspace so
`cargo test --workspace` never pulls in ~178 MiB of camera files. Run it with `mise run
fetch-dng-samples && mise run test-dng-real`; CI runs it in `extended.yml`'s `real-dng` job, not
on the per-PR path. Five layers per file: byte accounting (including the exact inventory of
unaccounted runs), decode against the manifest, the stored digest under the storage-correct rule,
the Adobe SDK stage-2 differential at ±1 code, and the preserving rewrite.

| File | What it alone proves |
| ---- | -------------------- |
| Apple iPhone 12 Pro (ProRAW) | Big-endian, tiled 504×378 12-bit `LinearRaw`, `LinearizationTable`, PGTM, semantic mask, a real MakerNote pinned across a rewrite — and a 10-byte `APPLEDNG` vendor preamble |
| Canon 5D3 uncompressed | `Compression = 1`, one 5920×3950 16-bit CFA strip |
| Canon 5D3 lossless | `Compression = 7` CFA, 384 tiles |
| Canon 5D3 lossy | `Compression = 34892`, a deferral that must be a *typed* refusal; the only file whose digest uses the compressed-chunk rule, so its integrity verifies even though its image does not decode |
| Leica M Monochrom | DNG 1.0.0.0 monochrome carrying **no colour calibration at all** — must decode with no profile rather than fail |
| Leica M10 | Raw in IFD 0 itself, previews with no `RowsPerStrip`, and a **651 KB appended trailer** the rewrite must carry |

What the corpus fixed:

- **`Predictor` (317)** was parsed and then ignored — a `Predictor = 2` file decoded to garbage
  with no error. Now undone per chunk following the SDK's `DecodeDelta8/16/32` exactly (rows
  independent, back-reference `samples_per_pixel × x_factor`, wrapping at the *container* width),
  with the float predictors and sub-byte depths refused typed. Self-predicting schemes (lossless
  JPEG, JPEG XL) ignore the tag, as the SDK reader does.
- **`PlanarConfiguration` (284)** was never read, so planar storage would have been misread as
  chunky. Now validated: chunky accepted, planar refused typed.
- **Byte accounting** did not hold for real files. `gamut-ifd` gained `SpanKind::{Preamble,
  Interstitial, Trailer}` and an explicit `classify_unclaimed` pass, so every byte of every real
  file classifies *and* the report still says what each run was. The pass is skipped when the walk
  admits a `SkippedSubIfd`, so it can never mask a parser defect, and the dual-ledger invariants
  are untouched.
- **`DngRewrite` dropped those bytes.** It now carries every unaccounted run through verbatim and
  reports each in `RewrittenDng::preserved`. Bytes survive; original absolute offsets generally do
  not, because the directory layout is rebuilt — the runs are appended after the payload region in
  file order, which leaves a trailer last.

## v2.0.0: what changed and why

### Embedded metadata is the workspace facade's, not this crate's (#353)

`gamut-dng` now depends on **gamut-metadata** and carries the facade's models instead of a
DNG-local restatement of them. A DNG's `ExifIFD` (34665) *is* an EXIF sub-IFD, and `gamut-exif`
already models one — over the very same `gamut_ifd::Ifd` this crate speaks — so the five-field
`ExifMetadata` was a subset of a model the workspace already ships. It is gone:

- **`ExifMetadata` is removed.** `DngMetadata::exif` is `Option<gamut_metadata::exif::Exif>`.
  Its rationals are `gamut_exif::Rational`, not bare `(u32, u32)` tuples, and *every* entry of
  the directory is carried — the previous five (`ExposureTime`, `FNumber`, `ISOSpeedRatings`,
  `DateTimeOriginal`, `FocalLength`) plus anything else the file holds or a caller sets.
- **`DecodedDng::exif_extra` is removed.** It existed only because the typed EXIF view was a
  subset; with the whole sub-IFD carried inside `Exif`, an "extras" list beside it would be the
  same data twice. P23's explicitness clause is stronger, not weaker: nothing in the EXIF
  directory is dropped, and it arrives typed rather than as loose `RawTag`s.
- **XMP (700), IPTC-IIM (33723) and ICC (34675) stay verbatim `Vec<u8>`** — a DNG holds each as
  one opaque payload, so there is no structure here for this crate to duplicate, exactly as
  `gamut-png` and `gamut-webp` carry theirs. `DngMetadata::blocks` presents them as the facade's
  `MetadataBlock`s, so `Metadata::from_blocks` turns them into the unified model.
  `iptc` deliberately keeps its own field even though `gamut_metadata::Metadata` has no IPTC
  carrier: reconciling a legacy IIM block into an XMP graph is a `ConflictPolicy` decision that
  belongs to the caller, and a container that made it silently could not give the file's bytes
  back.

The boundary: only the model's **Exif sub-IFD** crosses into the file. Its 0th IFD, GPS sub-IFD
and thumbnail name directories the DNG container builds itself (IFD 0, previews as `SubIFDs`
entries), so the encoder does not write them — see the deferral below.

### Three further breaking changes, all forced by real files

- **`DecodedDng::profile` is `Option<CameraProfile>`.** A monochrome camera has no colour to
  calibrate and legitimately writes no `ColorMatrix1`, `CalibrationIlluminant1` or
  `AsShotNeutral`; the Leica M Monochrom does exactly that and previously failed the whole decode.
  Absent calibration now yields `None` — nothing is invented. Calibration that is *present and
  malformed* is still an error.
- **`PhotometricInterpretation::YCbCr` (6)** is modelled, so the baseline-JPEG previews every real
  camera embeds stop raising an anomaly per preview.
- **`DngDecoder::verify_new_raw_image_digest`** is new, returning `DigestCheck`. It picks the rule
  the file's storage demands — sample-domain for lossless, compressed-chunk for lossy/JXL — which
  a caller could not previously do, because `lossy_compressed_digest` is crate-private.

`AsShotWhiteXY` (50729) is now typed, both directions (#349). A profile records its as-shot white
balance either way — `CameraProfile::with_as_shot_white_xy` selects the chromaticity form, and the
encoder writes exactly one of the two mutually-exclusive tags — and a file carrying only the
chromaticity decodes to a full profile rather than to `None`. The camera neutral it implies comes
from the DNG 1.7.1 §6 conversion "Translating White Balance xy Coordinates to Camera Neutral
Coordinates": interpolate the colour calibration by the white point's correlated colour temperature
(Robertson's method, `gamut_color::cct_from_xy`), then apply `XYZtoCamera = AB · CC · CM`. The
deferral said there was nothing to gate that conversion against beyond synthetic cases; there is
now — `gamut-dng-oracle` exposes the reference implementation's own derivation
(`dng_color_spec::SetWhiteXY`), and the single-illuminant, interpolated and clamped cases are all
required to agree with it.

## C2PA manifest store (issue #442, epic #239) — a `DngMetadata` break

The store (C2PA 2.4 §A.3.6: tag 52545 / `0xCD41`, type `UNDEFINED`) was already *visible* — a
decoded file carried it as an untyped `RawTag` — but had no name, no placement rule and no
exclusion ranges. Now:

- **`DngMetadata::c2pa: Option<Vec<u8>>`** is the fifth carrier, on the same terms as XMP /
  IPTC-IIM / ICC: opaque bytes, verbatim in both directions, handed over by `blocks()` as
  `MetadataBlock::C2pa`. `DngMetadata` is deliberately exhaustive (see the freeze decisions),
  so this is **semver-major**: every struct literal gains `c2pa: None`. Unlike the other
  carriers, a store is bound to one exact file — it is a signed hash over the bytes around it —
  so the only valid input is one an external signer computed over *this* encoder's output;
  `gamut_metadata::Metadata::encode` never hands one back (`C2paPolicy::Drop`).
- **Placement** is `gamut_ifd::c2pa`'s (the shared statement of §A.3.6, reused by #446): the
  entry goes in IFD 0 — the last and only IFD of this crate's main chain, the form the Adobe
  SDK reads without surprise — as an inline placeholder while the tree is laid out, and the
  store's bytes are appended **after the image data, last in the file**, with only the entry's
  count/offset words patched. The alternative §A.3.6 form, a trailing IFD holding only the
  entry, is *read* (the decoder consults the last main-chain IFD) but not written.
- **A reservation.** `DngEncoder::with_c2pa_reserved(len)` writes `len` zero bytes where the
  store goes; `encode_with_report` returns `DngEncodeReport { len, c2pa: Option<C2paExclusions> }`
  with the **two disjoint ranges** §18.5.5 needs — the store, and the entry's `count` field (4
  bytes classic, 8 BigTIFF) — so a signer hashes around them and overwrites the reservation in
  place. A reservation and a same-sized store produce byte-identical files outside the store
  range, and a store of a different size changes the count field and nothing else: the epic's
  "nothing after placement moves a byte" criterion, tested exactly. `encode` is unchanged (it
  delegates), and `EncodeImage` is untouched. A store and a reservation together, or a store
  shorter than a JUMBF box header (8 bytes), are typed errors before any pixel work.
- **Decode.** `DecodedDng::c2pa_exclusions` carries the ranges `gamut_ifd::c2pa::locate` finds,
  alongside the bytes in `metadata.c2pa`. A tag-52545 entry of another type is not a store and
  stays in `ifd0_extra`; so does one in IFD 0 when the main chain continues past it.
- **Byte accounting.** `deconstruct` claims the store as IFD 0's `Value { tag: 52545 }` span and
  the alignment filler before it as `Padding` — a store at the end of the file is never a
  `Trailer` — so a store-carrying file is fully classified. 52545 is in `tags::KNOWN_TAGS`
  (aliasing `gamut_ifd::c2pa::C2PA_MANIFEST_STORE`, where the clause is stated), so
  `is_fully_accounted()` stays **true** for a file this encoder writes: a manifest store gamut
  itself embedded is not a private tag.
- **Lengths.** A store shorter than a JUMBF box header (8 bytes) is refused by the encoder and
  read as *absent* by the decoder — the split `references/c2pa/README.md` prescribes, and what
  makes decode → encode of a foreign file carrying a stub value work. In **BigTIFF** the inline
  threshold is those same 8 bytes, so a store must exceed them or it would pack into the entry
  instead of landing at the end of the file; the encoder refuses that case with its own message
  rather than writing a file whose exclusion ranges cover bytes no reader reads back.
- **Duplicates.** Two tag-52545 entries in the last main IFD name no single store (§A.3.6: one
  per asset), so both `metadata.c2pa` and `c2pa_exclusions` report absence rather than
  describing different byte runs. The two surfaces cannot drift apart: the ranges are located
  first and the bytes are taken *only* if that succeeded, so one rule decides both. (Reading the
  bytes independently is what made them disagree — the eager `Ifd` keeps the last duplicate, so
  the bytes surface reported that entry while the ranges reported none, and re-encoding produced
  a one-entry file carrying only the last duplicate.) On the write side, `append_store` names a
  duplicated entry as the problem instead of claiming the entry is missing.
- **A declined field still reaches the caller.** A tag-52545 field the decoder declines to read
  as a store — wrong type, or too short — arrives verbatim. Where it lands depends on the
  directory: IFD 0's go to `ifd0_extra`, and the last main-chain directory's to the new
  `DecodedDng::trailing_extra`. **A duplicated entry is the one partial case**: the typed
  channels carry `gamut-ifd`'s eager `Ifd`, which keeps the *last* of several entries under one
  tag, so the last duplicate's bytes arrive and the earlier ones do not. Carrying both would
  mean changing that last-wins model, which every consumer of the IFD core shares; a file with
  two stores is malformed under §A.3.6 in any case, and `deconstruct` still accounts for every
  byte of both. That field exists because §A.3.6's other lawful placement (the
  store as "the only entity within a new IFD following the existing one") produces a directory
  with no image, which is therefore neither IFD 0, nor the raw IFD, nor a `SubImage` — so before
  it, such a directory's fields reached no surface at all. It is empty for every file this crate
  writes, which puts the entry in IFD 0.
- **Version.** Carrying the tag raises neither `DNGVersion` nor `DNGBackwardVersion`: like XMP
  and ICC it is metadata a reader may ignore, and the tag is C2PA's, not a DNG feature a
  reader must implement (the SDK's tag table names it as `tcC2PAManifest` and validates a file
  carrying it).
- **A store and a reservation together is an error, deliberately.** The signer flow is reserve →
  sign → re-encode with the store, which sets one at a time; setting both is a caller mistake,
  and letting either silently win would hide it.
- **Writing §A.3.6's trailing-IFD form is out of scope, deliberately.** For a single-main-IFD
  asset the clause permits the entry either in that IFD or as the only entity of a new IFD
  following it. This crate writes one main IFD and uses the in-IFD form, which is lawful and is
  what the Adobe SDK reads without surprise. The **decoder** reads both, since it consults the
  last IFD of the chain whatever its shape.
- **The one directory still not surfaced.** An *interior* main-chain page — neither the first
  nor the last — that carries no image data is no `SubImage` either, so its fields reach no
  verbatim channel. That predates #442 and is not what §A.3.6 creates (the store's own placement
  is the *last* directory, which `trailing_extra` now covers); `deconstruct` still accounts for
  its bytes. Filed as #525.
- **Not done here.** The store is never parsed; a `DngRewrite` of a file carrying one relocates
  it into the value pool like any other value (a rewrite invalidates the binding regardless);
  the behavioural `c2pa-rs` oracle is #447's.

## Bridge surface for external RAW pipelines (issue #253)

Downstream raw *processors* (e.g. rawshift) consume gamut-dng's decode as their DNG front end and
run their own develop pipeline on top. #253 completed the standard-compliant surface they bridge
to: the typed `RawLevels` model (P9), the chapter-5 `RawImage::to_linear` mapping (stage-2
oracle-gated, so downstreams call it instead of reimplementing the spec), typed opcode-list
containers (P18, processing still ours to do later), and the hardened, now-public
`lossless_jpeg` module. The typed encode/decode path deliberately exposes no opaque tag blobs —
it parses spec structures into typed values and writes them back; **preservation** is a
separate path (below).

## Byte completeness and the preserving rewrite (issue #263)

#263 verified — and, where verification failed, fixed — the byte-completeness story end to end:

- **`deconstruct`** is rebuilt on `gamut_ifd::audit`'s dual-ledger engine: every byte of the
  file classifies into typed segments (u64-native, so >4 GiB BigTIFF strips no longer
  false-flag), embedded camera-profile streams (`ExtraCameraProfiles` → `.dcp`-form,
  magic `0x4352`, stream-relative offsets) are walked and claimed at physical positions, and
  the strict verdict is the zero-tolerance `SegmentReport::is_fully_classified`. Gated over the
  Adobe SDK's full `sample_files` corpus (`tests/corpus.rs`): all fourteen Adobe-authored DNGs
  — JXL tiles, PGTM2, ImageStats, ImageSequenceInfo, HDR/SDR profiles — classify to the last
  byte with the parser cross-check holding.
- **`DngRewrite`** is the preservation path the typed codec deliberately is not: open the whole
  tree losslessly (unknown/vendor tags and unknown field types survive as data), edit it
  surgically, and write it back with every tag value byte-exact, every strip/tile/embedded-JPEG
  payload copied verbatim (never re-encoded), and the `MakerNote` **pinned at its original
  absolute offset** whenever the new layout permits (`MakerNotePreservation` reports the
  outcome). Intentional drops, in full: declared dead space (`FreeOffsets`/`FreeByteCounts` —
  the tags name explicitly-dead bytes) is dropped; a file carrying `ExtraCameraProfiles` is
  refused (`Unsupported`, deferred) rather than rewritten lossily. Corpus-gated: every
  rewritable Adobe sample survives open → write fully classified, with its unknown-tag
  inventory unchanged and `dng_validate` accepting the result wherever it accepts the original.

## v1.0.0 freeze decisions

- Spec-coded enums (`Compression`, `PhotometricInterpretation`, `CalibrationIlluminant`,
  `CfaLayout`, `Predictor`, `SampleFormat`, `ProfileEmbedPolicy`, `PreviewColorSpace`,
  `SubImageKind`), report enums (`Severity`, `Anomaly`), `RawPhotometry`, and decoder-output
  structs (`DecodedDng`, `SubImage`, `LinearImage`, `LosslessJpeg`, `DeconstructReport`,
  `UnknownTag`, `SemanticMaskInfo`, `DepthInfo`, `SubImageData`) are `#[non_exhaustive]` —
  future spec codes and fields are additive.
- Encoder-input data structs (`DngMetadata`, `Opcode`, `ProfileGainTableMap`, `RawTag`,
  `MaskSubArea`) keep literal construction (no `non_exhaustive`); adding a field there — or, as
  #353 did to `DngMetadata::exif`, changing one's type — is accepted as semver-major.
  `ExifMetadata` was on this list until #353 retired it in favour of `gamut-exif`'s `Exif`, whose
  own construction is `Exif::new(order)` plus `set_tag`/`exif_ifd_mut`; `DngMetadata` itself is
  still a struct literal, so the carriers (five since #442 added `c2pa`) stay visible at the
  point of use. `GainValues`
  stays exhaustive — its four variants are the spec's closed `DataType` set.
- Re-export closure: everything on the crate root, including `RawPhotometry`, `cfa_color`,
  `opcode_id`, `new_subfile_type`, `gamut_ifd::Value` (the `RawTag` payload type), the
  metadata surface's own types — `Exif`, `ExifTag`, `Rational` and `MetadataBlock` — so a caller
  builds and reads `DngMetadata` without a direct `gamut-metadata` dependency, and (since #442)
  the whole C2PA surface this crate's docs name: `C2paExclusions`, `C2PA_MANIFEST_STORE` and
  `MIN_STORE_LEN`, so a signer needs no direct `gamut-ifd` dependency either;
  `lossless_jpeg::{encode, decode}` stay module-scoped deliberately (a codec namespace).
- `Compression::is_supported` became `is_decodable` (every decodable scheme encodes, with the
  documented `jxl-encode`/Deflate-depth caveats).
- JPEG XL range semantics are frozen to the reference SDK's: decoded JXL data is full-range
  16-bit; a JXL IFD's `BitsPerSample` records codestream precision; encode requires 16-bit
  input.

## Deflate codec choice (#196)

Encode uses `gamut-deflate` at `Level::Default`; decode uses `miniz_oxide`, bounded to the packed
length the chunk geometry implies. `gamut-deflate` is deliberately encoder-only (inflating is
solved and security-sensitive), so the split is permanent, not a staging post — the same one
`gamut-tiff` and `gamut-png` make.

Measured by `cargo bench -p gamut-dng --bench compression`, against the `miniz_oxide` level 6 this
crate encoded with before:

- **Ratio is a wash.** On packed raw payloads `Level::Default` lands within ±0.6% of miniz-6 —
  slightly better tiled, slightly worse as one strip. Raw sensor noise leaves DEFLATE modelling
  almost nothing to work with (a 16-bit frame compresses ~3%), so the entropy-coding differences
  that separate these encoders on text do not show up here.
- **Encode is ~17% slower** (≈46 MB/s vs ≈56 MB/s); `Level::Best` is ~12× slower again.
- **`Level::Best` only pays off tiled** (−0.3% to −0.5% on real raw, −5.5% on 8-bit), because
  `gamut-deflate` applies its optimal parse at 1 MiB or below and the untiled encoder writes
  `RowsPerStrip = ImageLength` — one strip for the whole image, above the threshold. Raising that
  limit is tracked upstream rather than worked around here, which is why the shipped level stays
  `Default`.

The Adobe DNG SDK validates the output on every fixture the oracle covers (CFA and LinearRaw,
8- and 16-bit, strips and tiles), so the migration is correctness-neutral.

## Codec benchmark harness (#163)

`cargo bench -p gamut-dng --bench codec` measures **encode and decode throughput across the whole
shipped codec matrix** — uncompressed, Deflate and lossless JPEG, each for CFA and `LinearRaw`
photometry — and puts gamut's decode next to the **Adobe DNG SDK's**. (The older `--bench
compression` is narrower and stays as it is: it answers the #196 question, "which DEFLATE encoder
should the ZIP path use", on packed payloads.) Fixtures are synthesised in-process, so the harness
needs no sample corpus and runs by default; the ~178 MiB real-camera submodule behind `mise run
fetch-dng-samples` is deliberately not a prerequisite.

**What is timed.** The codec call, the allocation and growth of the buffer it produces, and that
buffer's teardown — the last of those explicitly, because divan would otherwise defer a returned
value's drop past the timed region, which would charge gamut nothing for freeing a decoded image
while the SDK's `dng_negative` destructor runs inside its own call. Fixture synthesis, the
`RawImage`/`CameraProfile` build, and the encode that produces the bytes a decode benchmark reads
are all outside it. Nothing touches the filesystem.

**Whether the comparison is fair.** Every asymmetry between the two implementations is either
removed or measured; none is left as an adjective.

- **Neither side pays for the FFI boundary on the way in.** The oracle gained a timed entry point,
  `decode_dng_in_memory`, which hands the SDK a `dng_stream` over the caller's own bytes: no
  temporary file, no import copy, the same buffer gamut parses.
- **Neither side pays for it on the way out, in the container comparison.** That entry point
  reports the decoded image's extent and exports no samples, so the reference implementation is not
  charged for a `malloc` + `memcpy` that exists only because the caller is in Rust.
- **The one asymmetry left in `decode_dng` is the IFD-0 preview** (plus the metadata
  reconstruction), which `DngDecoder::decode` performs and `ReadStage1Image` does not. Its volume
  is exact, and it is the volume the *decoder materialises*, not the one the file stores: the
  preview is written at 8 bits, but every sub-image is surfaced as `SubImageData::Decoded(
  Vec<u16>)`, so the buffer gamut allocates, fills and frees is `⌊w/2⌋ × ⌊h/2⌋ × 3 × 2` bytes
  against the raw's `w × h × planes × 2` — 75 % of a 16-bit CFA frame and 25 % of a `LinearRaw`
  one. On the **uncompressed** rows that volume goes into gamut's divan counter, so the
  **median-time** column is the uncorrected ratio and the **throughput** column the corrected one.
  On the **compressed** rows it does not. Correcting there charges preview bytes at the raw path's
  per-byte rate, and under Deflate or lossless JPEG a raw byte carries entropy-coding work a
  preview byte does not, so the arithmetic yields a lower bound on gamut's ratio rather than a
  measurement of it; the harness prints no number for it and says so, in the fixture table and in
  the epilogue below it. Read a compressed row as: gamut's figure includes preview and metadata
  work the reference arm does not do, by an amount this harness does not measure. It is not
  normalised away by changing the codec: gamut exposes no raw-image-only decode entry point, and
  adding one so a benchmark reads better would be the wrong direction of causation.
- **The one asymmetry left in `decode_lossless_jpeg` is the FFI export path, and a third arm
  bounds it.** `adobe-sdk-no-export` runs the identical `DecodeLosslessJPEG<Scalar>` into the
  identical spool buffer and stops before the `malloc`/`memcpy`/`Vec` copies. Across sixteen
  case-runs the gap between the two SDK arms spans −4 % to +64 %: an effect below this harness's
  run-to-run spread on a shared machine, whose *sign* is not resolved. What that supports is a
  **bound** — on the runs where neither SDK arm was disturbed the gap is under 3 %, and the
  fairness claim needs only that the export path cannot account for a 30×-plus ratio — not a
  figure for what the export path costs. Earlier revisions of this section quoted 0.3–1.9 % as
  though it were the cost; it was two samples of a quantity at the noise floor.
- **On the Deflate rows neither arm's inflate is gamut-authored, and only one of the two is
  pinned.** `gamut-deflate` is deliberately encoder-only, so this crate inflates with
  `miniz_oxide`; the oracle's `build.rs` links the system libz dynamically (`-lz`), because the
  SDK includes `<zlib.h>` unconditionally. A `*/deflate` row is therefore **`miniz_oxide` against
  whatever libz the loader resolved** — not gamut's own codec against the SDK's. The distinction
  that decides whether the row is reproducible is **pinning**, not authorship: `miniz_oxide` is
  pinned by `Cargo.lock` to one version and one checksum, so every run of this harness anywhere
  inflates with the same code, while the system libz is pinned by nothing. Not by a version: the
  loader chooses between a copy a dev oracle built under `target/` and whatever the platform
  installed, and `zlibVersion()` separates those two only when the platform's build renamed itself.
  This box's did — it answers `"1.3.1.zlib-ng"` where the build-tree copy answers `"1.3.1"` — but a
  box shipping stock zlib 1.3.1 gives two resolutions that answer identically, so what identifies
  the loaded library cannot be the version string.
  And not even by the machine: `cargo bench` puts every build script's native search path on
  `LD_LIBRARY_PATH`, so it resolves whichever stock zlib a dev oracle in the graph has built under
  `target/` (`gamut-dng` dev-depends on `libtiff-oracle`, which builds one, so `cargo bench -p
  gamut-dng` on its own is enough), while running the same binary directly resolves the platform's.
  Measured here, that choice moves the reference arm by 1.2–1.3× and moves gamut's arm not at all —
  enough to reverse which side of 1.0 a Deflate row falls on, with no defect in either
  implementation. The harness therefore prints the resolved library above its divan output
  (`zlibVersion()` plus the path `dladdr` reports; the path is what identifies the resolution,
  because two stock builds of one version are indistinguishable by version string) and **warns
  when that path lies inside a build directory**, because a resolution
  that came from the build graph rather than from the platform is one nobody else reproduces. A
  Deflate figure below travels with the library it was taken against or not at all. Pinning that
  library for the benchmark while keeping `-lz` for conformance is filed as **#618** and not taken
  here: it is a build-system change to a crate every `gamut-dng` test links, and it belongs to its
  own change rather than to the one that added the benchmark.

**Each pair is one benchmark, not two.** `decode_dng` and `decode_lossless_jpeg` take the
implementation as a divan *argument* rather than living in a benchmark each. Separate benchmarks
run in name order, which measures every reference case minutes away from its counterpart; on a
shared machine that drifts, a ratio measured minutes apart is not a ratio. As arguments the pair
members run back to back under the same instantaneous load, and the argument names are ordered so
divan's own name sort keeps them adjacent.

Interleaving is kept on that argument alone. An earlier revision of this section also credited it
with a 25–30 % shift in the two Deflate ratios; that attribution is **withdrawn**. Those are the two
rows now known to depend on which libz the machine resolves, an effect of the same magnitude and the
same sign, and this round did not re-run the non-interleaved arrangement under a pinned library, so
the shift is not this section's to explain.

There is no `encode` arm for the SDK: the oracle shim wraps the SDK's *reader*, not its writer, so
no reference encode number exists and none is invented. Encode is reported for gamut alone.

**Alternate the arm order between runs.** Adjacent is not simultaneous: divan cannot interleave a
pair *per sample*, so one arm always runs first and inherits nothing while the second inherits the
caches and the frequency governor the first left. divan's sort is reversible, so the control
already exists — `--sortr name` runs the gamut arm first — and a published ratio is the mean of one
run each way. Measured across the eight runs below, the order is worth about a percent, well under
the run-to-run spread; it is corrected for because it is one-directional, not because it is large.

**No absolute figures are pinned here.** Unlike the #196 numbers above — a ratio comparison between
two encoders in the same process, which is robust to a loaded machine — throughput in MB/s is a
property of the machine that produced it. Run the harness on the box you care about.

**What the harness measured.** Eight runs, 100 samples each, 512×384 at 16 bits, on a shared
machine at one-minute load averages of 15 to 38 (bracketed by `uptime` per run): two repetitions of
`{stock zlib, zlib-ng} × {reference arm first, gamut arm first}`. Ratios only, medians unless
marked; every row of the matrix is here, including the ones that do not fit a tidy story, and the
raw divan output for all eight is published with the pull request rather than summarised into these
cells.

Whole-file decode, gamut ÷ Adobe DNG SDK, median time — uncorrected, which is what the harness now
prints on the compressed rows:

| `decode_dng` case          | stock zlib 1.3.1            | zlib-ng 2.3.3               |
| -------------------------- | --------------------------- | --------------------------- |
| `cfa/uncompressed`         | 2.17, 2.27, 2.23, 2.21      | 2.52, 2.26, 2.14, 2.34      |
| `cfa/deflate`              | 0.94, 0.94, 0.94, (2.71)    | 1.23, 1.25, 1.26, 1.17      |
| `cfa/lossless-jpeg`        | 83, 50, 89, (18)            | 87, 45, 66, 92              |
| `linear-raw/uncompressed`  | 1.73, 1.72, 1.79, 1.79      | 1.84, 1.70, 1.78, 1.82      |
| `linear-raw/deflate`       | 1.00, 0.90, 0.97, 0.84      | 1.41, 1.28, 1.28, 1.28      |
| `linear-raw/lossless-jpeg` | 35, 101, 60, 47             | 47, 47, 75, 57              |

The four cells per column are, in order, `{rep 1, rep 2} × {reference arm first, gamut arm first}`.
The parenthesised `cfa` figures come from the noisiest run in the set (load 36.8); its
*fastest*-sample ratios are 0.97 and 27, in line with the rest. The `uncompressed` rows are quoted
uncorrected here; with the preview correction the harness applies to them, they read 1.24–1.44 and
1.36–1.47 respectively.

Bare codestream decode, which carries no container asymmetry — same SOF3 stream in, same samples
out, one counter for all three arms. Medians are unusable here (gamut's arm is ~100 ms, long enough
to swallow a scheduling event whole), so the fastest-sample ratio is given alongside:

| `decode_lossless_jpeg` case | gamut ÷ SDK, median | gamut ÷ SDK, fastest sample |
| --------------------------- | ------------------- | --------------------------- |
| `cfa`                       | 45–92               | 34–61                       |
| `linear-raw`                | 42–91               | 41–63                       |

**The two Deflate rows depend on a library this repository does not build**, and that is the whole
of a discrepancy an independent re-measurement raised against an earlier revision of this section.
The earlier figures (0.94–0.97, gamut faster) and the independent ones (1.20–1.26, the SDK faster)
are **both correct**, and the eight runs above reproduce both: 0.94 under stock zlib 1.3.1, 1.17–1.26
under zlib-ng 2.3.3, at every load from 15 to 38 and in both arm orders. The isolating evidence is
that the *gamut* arm does not move between the two — its `cfa/deflate` median is 1.04–1.05 ms under
either library — while the reference arm moves from 0.83 ms to 0.97–1.11 ms. Neither measurement was
wrong; the harness failed to say which inflate implementation it had measured, so two correct runs
looked like a contradiction. It now prints it.

**What the harness found.** Two defects, both filed rather than fixed here — a benchmark that
measures the codec is not the place to change it:

- **#583, lossless-JPEG decode speed.** The isolating evidence is the **codestream pair** above,
  which carries no container asymmetry and whose one residual bias — the FFI export path — is
  bounded well below the effect: there gamut is **one and a half to two orders of magnitude
  slower** than the reference implementation. Across eight runs, on both photometries and in both
  arm orders, no fastest-sample ratio is below **34×** and the medians centre near 50–60×; the
  earlier "56–59×" was a two-run figure and is not reproducible to that precision on a loaded box,
  but nothing in the eight runs brings the effect near parity. `lossless_jpeg::decode_symbol` scans
  the whole 256-entry code table once per candidate bit length, so a symbol costs ~1000 comparisons
  where the reference implementation spends one table probe.

  The whole-file rows are published above in full rather than filtered to the ones that agree. Two
  of them are not close to parity: `cfa/uncompressed` at 2.1–2.5× and `linear-raw/uncompressed` at
  1.7–1.8×. Those are the rows the preview correction applies to, and corrected they read 1.2–1.4×
  and 1.4–1.5×; the remainder is the fixed IFD and metadata reconstruction, which does not scale
  with the frame and therefore dominates exactly where the raw path is little more than a `memcpy`.
  This harness measures that gap and does not attribute it further — and #583's isolation does not
  rest on it.
- **#584, CFA lossless-JPEG size.** Quoted throughout against **one** denominator, the raw sample
  volume (393 216 bytes for this fixture): `cfa/uncompressed` writes a 541 440-byte file (137.7 %)
  and `cfa/lossless-jpeg` a 618 800-byte one (157.4 %), so turning compression on makes the file
  **14.3 % larger**. The encoder hands the mosaic to `lossless_jpeg::encode` as one full-width
  component, so predictor 1 differences a red photosite against its green neighbour. Declaring the
  same samples as `(width / 2, height, 2)` — the reshape DNG 1.7.1.0 p. 20 describes, which needs
  no sample reordering and which this crate's decoder already reads — takes the codestream from
  470 576 bytes to 359 888. Both files carry an identical 148 224 bytes of preview and directory,
  so the reshaped file would be 508 112 bytes, **129.2 %** of raw: **6.2 % smaller than the
  uncompressed file**, not the ~33 % a reader gets by chaining the codestream ratio onto the file
  ratio.

**The fixture table is not a codec gate, and should not become one.** #584's verification section
proposes pinning the encoder to the sizes this harness prints. It should not be: the margin is a
property of frame-uniform synthetic gains — one gain per CFA colour across the entire frame, which
is what makes the interleaved-component reshape win so cleanly — and pinning an encoder requirement
to a single synthetic fixture is precisely the failure a benchmark harness exists to avoid.
Re-measure on the real-camera corpus (`mise run fetch-dng-samples`, then `mise run test-dng-real`)
before the encoder changes, and gate on that if anything is to be gated.

#584 is a byte quantity and reproduces anywhere. #583 is a ratio, and the only outside code in its
measured path is the SDK's own lossless-JPEG decoder, built here from the committed SDK source — so
unlike the Deflate rows it does not depend on what the machine has installed.

**Both issues were filed from the first revision of this section and still quote figures it has
since withdrawn**: #583 asserts the two Deflate ratios and rests its localisation argument on them,
omitting the uncompressed rows that falsify it, and its verification command names benchmarks that
no longer exist (the arms were merged into one benchmark taking the implementation as an argument —
`cargo bench -p gamut-dng --bench codec -- decode_lossless_jpeg`); #584 quotes a fixture-table
column this harness no longer prints, whose preview model has since doubled. **#617** states
precisely which figure in each is withdrawn and what replaced it. This section is that replacement
text; the two findings themselves stand.

## Deferred / out of scope

Each deferred item plugs into the same IFD-tree/chunk pipeline and oracles the shipped features
use; additions are semver-additive.

- **Lossy JPEG** (`Compression = 34892`) — needs a baseline DCT codec (`gamut-tiff` likewise
  deferred JPEG-in-TIFF). Decode surfaces such images as verbatim chunks today; a lossy *raw*
  IFD is refused with a typed `Unsupported`, and its `NewRawImageDigest` still verifies (the
  compressed-chunk rule needs no pixels).
- **A third colour calibration** (`CalibrationIlluminant3` / `ColorMatrix3`, DNG 1.6.0.0) — the
  §6 white-balance interpolation weights over the first two calibrations, which is what the
  pre-1.6 rule prescribes; a profile carrying a third set decodes typed onto `ColorProfileInfo`
  (P25) but is not blended with it.
- **Writing the P25 colour tags** — the projection is read-direction only: `DngEncoder` builds
  IFD 0 from a `CameraProfile`, so emitting the rendering tables, tone curve, noise profile and
  third calibration set means widening that encoder input, which is its own feature.
- **Restoring *interior* unaccounted bytes to their original offsets** — #350 landed the leading
  case: a vendor preamble now keeps its offset, because `gamut-ifd`'s writer reserves the
  header/first-directory gap for it (`WriteOptions::with_preamble`), which is the position that
  matters — Apple ProRAW's `APPLEDNG` sits there and a vendor tool looks for it there. Interstitial
  filler and an appended trailer still keep only their bytes: an interstitial run's original
  position is interior to a payload layout the rewrite does not reproduce (the strips it sat
  between are re-packed), so there is no offset to restore it to. `PreservedSpan` reports both
  offsets, so a caller can see which happened.
- **Floating-point samples** (`SampleFormat = 3`, fp16 JPEG XL, the float predictors
  34894/34895) — rejected with typed errors on decode; the u16 sample model would need a float
  sibling.
- **The standard opcode processing library** (P18) — *executing* `WarpRectilinear`, `GainMap`,
  `FixVignetteRadial`, … The typed containers round-trip; processing is a raw-developer concern.
- **Applying gain maps / rendering** — `ProfileGainTableMap` parses typed (and `gain_at`
  decodes entries); applying it in RIMM space is rendering-pipeline work.
- **Writing mask/depth/enhanced sub-IFDs** — decode-only today; the encoder writes the raw +
  preview tree.
- **4-colour CFA encoding** (RGBW/CYGM) — `CameraProfile` is a 3×3 model; widening it (4×3
  matrices, `ReductionMatrix`) is its own feature. Decode of 4-colour patterns works.
- **PGTM2 inside Camera Profile IFDs** (`ExtraCameraProfiles`) — surfaced via extras; typed
  parse covers the IFD0/raw-IFD placements.
- **JPEG-compressed previews as pixels** — surfaced as verbatim chunks (`SubImageData::
  Undecoded`); decoding them needs the baseline DCT codec above.
- **Advanced 1.7 metadata without a typed surface** (`RGBTables`, `ImageStats`,
  `ImageSequenceInfo`, `ProfileDynamicRange`) — explicitly surfaced as typed `RawTag`s. (The
  C2PA manifest store left this list in #442: `DngMetadata::c2pa`.)
- **The GPS sub-IFD (`GPSInfo`, 34853) and an EXIF 0th IFD / thumbnail** — `DngMetadata::exif`
  is a whole `gamut_exif::Exif`, so those directories are *expressible* in the encoder input,
  but only its Exif sub-IFD is written: IFD 0 and the preview sub-IFDs are the DNG container's
  own, built from the raw image and the camera profile, and merging a caller's TIFF tags into
  them could contradict the file's geometry. A GPS pointer in a decoded file therefore still
  surfaces in `ifd0_extra`. Wiring `GPSInfo` through symmetrically is additive.
- **Pluggable codestream backends** (#241) — no hardware acceleration exists for the DNG
  compression schemes (Uncompressed/Deflate/lossless-JPEG/JPEG XL); gamut's software
  implementation is always used, so no backend seam is exposed.
