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
  describing different byte runs.
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
