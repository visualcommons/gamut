# gamut-jpeg — JPEG-1 codec status

Tracking GitHub issue #28: a spec-compliant JPEG-1 (ISO/IEC 10918-1 | ITU-T T.81) still-image
**encoder + decoder**. Delivered as small, individually green phases (each `mise run test`/`lint`/
`fmt-check`/`coverage` ≥80%).

**Keystone:** the SOI → APP0(JFIF) → DQT → SOF0 → DHT → SOS → entropy → EOI pipeline with the
baseline sequential DCT Huffman encoder (P1) — libjpeg-turbo now decodes that to the source pixels
within the lossy tolerance (measured across the P3 battery: gray/4:4:4 within **11 codes** at
q ∈ {50,75,90}, **5** at q90; subsampled **> 30 dB** PSNR, **> 34 dB** at q90), and each later phase
adds a process (progressive) or a direction (decode) behind the same marker spine, entropy coder,
and table machinery.

**Oracle:** differential vs **libjpeg-turbo** (a vendored, dev-only static build of 3.2.0 in
`tooling/libjpeg-oracle`, landed in P3), cross-checked against the vendored **T.873 reference
software** (`references/jpeg`, ISO/IEC 10918-7) for spec-exact behaviour. The gate runs both
directions in `tests/oracle.rs`: **encode** (gamut encodes → libjpeg-turbo decodes → matches source
within tolerance) and **decode** (libjpeg-turbo encodes → gamut decodes → matches libjpeg-turbo's
own decode of the same stream, gray/4:4:4 within **3 codes** of IDCT rounding, subsampled bounded by
the documented replication-vs-fancy upsampling divergence). The decode gate runs the **sequential**
battery and, from P4, the **progressive** one (libjpeg-turbo's standard 10-scan `jpeg_simple_progression`
script across gray/colour × 4:4:4/4:2:2/4:2:0 × q{40,75,95} × restart{0,2} × optimize{off,on}); the
progressive parity numbers are **identical** to the sequential ones (gray/4:4:4 max-diff **3**,
subsampled PSNR **22.9 dB**), confirming the progressive coefficients match exactly — only the shared
IDCT/upsampling remains as a source of divergence. From P5 the **encode** gate adds a **progressive
exactness** direction (`tests/oracle.rs`): gamut's SOF2 stream and its baseline stream of the same
image carry the same quantized coefficients, so libjpeg-turbo's decode of each must be **byte-for-byte
identical** — asserted as exact equality (not a tolerance) across the dims × mode × q{40,75,95} ×
restart{0,2} battery. gamut's **own** decoder is held to the same exact bar (`tests/progressive.rs`):
`decode(progressive) == decode(baseline)`. Before P2 the encoder was additionally pinned by byte-exact
micro-goldens hand-derived from T.81 Annex F/K and a structural stream walker; the progressive decoder
adds hand-built minimal streams (DC-then-AC, a successive-approximation pair, an EOBRUN-spanning case,
a DC refinement) checked against sequential twins, plus a scan-ordering validation corpus; the
progressive encoder adds direct §G.1.2 entropy-model unit tests (ZRL boundary, point transform, DC
refinement bit, AC-refinement sign/EOB fold), an exact Annex K.2 optimal-table golden, and a
progressive-stream walker (scan script, per-scan DHTs, restart cadence, EOBn-run presence).

## Scope ledger

**In scope** (issue #28, across the phases below):

- Baseline sequential DCT, Huffman, 8-bit (SOF0) **encode** — grayscale + YCbCr 4:4:4/4:2:2/4:2:0,
  JFIF interchange format (this phase).
- Sequential DCT, Huffman, 8-bit (SOF0 baseline / SOF1 extended) **decode**.
- Progressive DCT, Huffman (SOF2) **decode and encode** (spectral selection + successive
  approximation).
- Restart markers (DRI/RSTn).
- Colour-space handling: JFIF (APP0) and Adobe (APP14) transform flags; CMYK / YCCK **decode**.
- APP-segment metadata (P7): APP1 EXIF + XMP and multi-segment APP2 `ICC_PROFILE`, **read**
  (`metadata()`) and **write** (`with_exif`/`with_xmp`/`with_icc_profile`), raw-bytes payloads that
  feed `gamut-metadata`'s `MetadataBlock` directly (the default dependency edge stays jpeg ← core,
  color, dsp).
- Typed metadata (P14, issue #420) behind the opt-in **`metadata`** feature (a normal, optional
  dependency on `gamut-metadata`): `JpegMetadata::blocks` (EXIF = the TIFF stream, `Exif\0\0`
  stripped; XMP = the `xpacket`; ICC = the reassembled profile) and `JpegMetadata::metadata`
  (`Metadata::from_blocks`); `JpegEncoder::with_metadata(&Metadata)` (default `MetadataEmbedder`)
  and `with_encoded_metadata(&EncodedMetadata)` (caller-chosen policies), routing each carrier to
  the raw setter above. IPTC-IIM and C2PA blocks are typed `Unsupported` on the encoder (no APP13 /
  APP11 carriage — see below), and a C2PA store is never copied forward (facade policy).
- Pluggable codestream backends (P8, issue #277): the `backend` module's `JpegStreamDecoder` /
  `JpegStreamEncoder` traits, `JpegDecoder::push_backend` / `JpegEncoder::push_backend`, and the
  `gamut-codec-abi` adapters in both directions.
- Opt-in decoder resource guards (P9, issue #306): `JpegDecoder::with_max_dimensions` and
  `with_max_image_bytes`, enforced before built-in frame allocations and backend selection, while
  bounding sequential DNL-deferred sample-plane growth until the exact height arrives.
- XYB colour mode (P13, issue #334): `with_color_mode(JpegColorMode::Xyb)` — jpegli-style
  scaled-XYB samples (via `gamut_color::xyb`; stored channels X, Y, B−Y), a no-JFIF-APP0 / Adobe
  APP14 `transform = 0` prologue with component ids `R`,`G`,`B` (either signal alone keeps a
  decoder from applying a YCbCr inverse), and the static vendored `XYB_ICC_PROFILE` embedded so
  any ICC-aware consumer reproduces sRGB. The profile is regenerated and byte-pinned from
  `gamut-icc`+`gamut-color` by an umbrella-layer test (`crates/gamut/tests/xyb_icc.rs`) and
  validated end-to-end against the lcms2 oracle — the runtime dependency edge stays
  jpeg ← core, color, dsp. Always 4:4:4; grayscale and caller ICC profiles rejected; backends
  vetoed; progressive/restarts/optimized-tables/custom-tables/RD compose.
- Rate–distortion optimized coefficient selection (P12, issue #333): `with_rd_optimization` —
  per-block AC trellis over the exact §F.1.2.2 run/size cost of the typical Annex K.5/K.6 tables
  (a documented rate-proxy free choice that keeps baseline and progressive coefficients
  identical), step-normalized distortion with a tuned dimensionless λ, and per-block adaptive λ
  modulation from block activity relative to the quantization resolution. DC keeps plain rounding
  (DC trellis deferred: a cross-block DP through the §F.1.2.1 predictor and restart resets —
  mozjpeg ships it separately too). Default off (byte-identical); backends vetoed.
- Caller-supplied quantization tables (P11, issue #332): the validated `QuantTables` pair
  (natural order, entries `1..=255` by construction) used **verbatim** via
  `JpegEncoder::with_quant_tables`, bypassing — without changing — the frozen quality mapping;
  `QuantTables::annex_k()`/`scaled()` recover the frozen mapping over arbitrary base tables.
  Custom tables pin the encode to the built-in path (a `JpegEncodeRequest` cannot carry them, so
  backends are not consulted — the host-side-veto convention).
- Opt-in optimized baseline Huffman tables (P10, issue #331): `JpegEncoder::with_optimized_tables`,
  the Annex K.2 construction the progressive encoder already uses, reached from the sequential path.

**Deferred / out of scope** (documented, with reasons):

- **A finer-grained (per-scan / entropy-segment) codestream seam.** JPEG-1's marker segments and
  entropy-coded data interleave in one stream (§B.1.1.5), and the Huffman/bit layer is intrinsic to
  the frame structure it codes, so there is no sub-stream boundary a real accelerator consumes:
  nvJPEG, V4L2 stateful/stateless JPEG, and libjpeg-turbo all take the **whole SOI..EOI interchange
  stream**, which is where the P8 seam is drawn. Exposing a per-scan entropy seam would publicize
  the crate's internal DCT-coefficient, quantizer, and Huffman state as public API for zero
  consumers. The accepted consequence is that "the crate owns the container" degenerates, for JPEG,
  to the crate owning **metadata and validation** — APPn EXIF/XMP/ICC stays crate-owned in both
  directions and is patched into backend-produced streams. Not planned.

- **12-bit precision (P=12).** Baseline is 8-bit only; 12-bit sample precision is an extended-DCT
  feature with little real-world corpus. The DCT kernel (`gamut-dsp`) already handles it, so this is
  additive later if demand appears.
- **Arithmetic coding (SOF9/SOF10, DAC).** Patent-era, near-absent in the wild; Huffman covers all
  practical JPEG-1. Not planned.
- **Lossless process (SOF3).** The predictive lossless mode is unrelated to the DCT pipeline;
  `gamut-dng` covers the only in-workspace consumer (lossless-JPEG inside DNG). Not planned here.
- **Hierarchical mode (SOF5–7, DHP, EXP).** No real-world still-image corpus. Not planned.
- **SPIFF and other T.84 extensions; T.872 printing conventions.** Niche container/printing layers
  atop the codec, tracked in `references/jpeg` but not implemented.
- **ExtendedXMP (XMP Part 3 §1.1.3.1).** An XMP packet exceeding the single-APP1 StandardXMP cap
  (65502 bytes, spec-stated) is rejected at encode as `Unsupported`, and ExtendedXMP continuation
  segments (the `xmp/extension/` GUID scheme) are skipped on read. The GUID/MD5 chunking is a
  separate mini-protocol with near-zero write-side demand — real packets are ~2 KB (the spec's own
  observation); additive later if demand appears.
- **APP13 IPTC-IIM (Photoshop 3.0 PSIR).** The legacy IPTC carrier; modern IPTC rides inside XMP
  (which `gamut-metadata` models as the single home). `JpegMetadata` is `#[non_exhaustive]` so the
  carrier can be added without a breaking change.
- **XYB-tuned quantization tables.** The XYB mode quantizes X/Y with Annex K.1 and the stored
  B−Y with K.2 — an honest placeholder (those tables are YCbCr-tuned). jpegli's
  `kBaseQuantMatrixXYB` needs a quality→distance mapping and a vendored citation; additive later
  through the `QuantTables` plumbing.
- **jpegli B-only subsampling for XYB.** jpegli optionally subsamples only the B channel
  (2,2/2,2/1,1); gamut's XYB mode is 4:4:4-only for now (`ChromaSubsampling` is ignored — its
  chroma semantics would subsample X, destroying opponent-colour detail). The scan machinery is
  sampling-generic, so this is additive.
- **XYB-mode byte reproducibility.** The XYB sample path is `f64` colour math under gamut-color's
  Tier-1 determinism, so — unlike the frozen default path — XYB output bytes are not
  bit-reproducible across platforms; XYB tests are tolerance/structure based and the vendored ICC
  bytes are static. Not a contract change: the frozen quality/byte contracts bind the default
  path only.
- **Alternate built-in base tables (flat, mozjpeg/jpegli psychovisual).** Every built-in constant
  table is a citation obligation under the `references/` policy, and no such table has a vendored
  source here yet. `QuantTables`' inherent constructors are append-only, so tuned built-ins are
  additive later; until then callers supply their own bytes.
- **CLI metadata passthrough.** `gamut convert` now decodes JPEG input with `JpegDecoder` rather
  than the third-party `image` crate (issue #268), but still through the `DecodeImage` pixel path,
  which carries no APP segments; a passthrough needs the metadata-bearing decode entry point and
  belongs to a broader CLI metadata story, not issue #28.

**Decoder-specific notes:**

- **DNL (define number of lines, §B.2.5).** The **decoder** parses DNL and resolves a *sequential*
  frame with `Y = 0` by decoding MCU rows until the entropy data ends at a marker (the `Y = 0`
  frame's height then arrives in the following DNL). A DNL after a `Y ≠ 0` frame is advisory and
  ignored. An opt-in dimension or byte cap derives an MCU-row ceiling before this dynamic decode,
  then the exact resolved height is checked when DNL arrives. The encoder never emits `Y = 0` (it
  always knows its height and writes it in SOF0).
- **Resource-budget accounting.** `with_max_image_bytes` budgets the native 8-bit interleaved
  raster (`width × height × frame components`), independent of chroma subsampling or requested
  `Gray8`/`Rgb8`/`Cmyk8` presentation. Defaults remain unrestricted for compatibility. SOF limits
  run before built-in frame-sized allocation or backend selection; accepted backend output is
  checked again, although the host cannot cap memory internal to an accepted foreign backend.
- **Progressive + `Y = 0` (deferred height) is rejected as `Unsupported`.** The progressive
  coefficient buffers are sized to each component's full block grid before the first scan, so a
  height deferred to a trailing DNL cannot be accommodated without a two-pass scan or dynamically
  growing every component's buffer across all scans — a disproportionate complication for a case the
  libjpeg-turbo oracle (and real-world encoders) never emit. A DNL inside a `Y ≠ 0` progressive
  frame is advisory and ignored, as for sequential.
- **Partial progressive streams render generously** (the libjpeg convention). A progressive frame
  that ends (EOI) before every band is complete is reconstructed from the coefficients delivered so
  far — missing AC bands stay zero, incomplete refinements stay at their coarser approximation —
  **provided every component received its DC first pass** (§G.1.1.1.1); otherwise the frame has no
  baseline and is rejected as `InvalidInput`. A stream truncated mid-scan (EOF with no terminating
  marker) is an `InvalidInput` error, never a panic.
- **Progressive quantization-table binding (§B.2.4.1).** Each component latches its dequantization
  table at its **first** scan; a later DQT redefinition of that destination does not retroactively
  change an already-latched component, matching the reference decoder.
- **Upsampling filter.** Subsampled chroma is upsampled by **sample replication** (nearest); T.81
  leaves the reconstruction filter open (§A.2 NOTE), so this is the decoder's documented free choice.
- **Trailing bytes after EOI** are ignored (the libjpeg convention).
- **CMYK is presented verbatim** (no Adobe inversion); **YCCK** applies the inverse YCbCr transform to
  the first three channels with `K` passed through (Adobe TN #5116).
- **`metadata()` stops at the first SOS** (or EOI). The APP1/APP2 metadata conventions place their
  segments before the scan data (XMP Part 3 §1.1.3 requires placement before the first SOF, with
  reader tolerance up to SOS), so no entropy decoding ever runs; segments after the scan are
  unreachable by design.
- **Duplicate EXIF/XMP APP1: first wins** (the libjpeg-family convention). ICC APP2 chunks are
  reassembled by their 1-based index regardless of segment order; an inconsistent chunk sequence
  (index/count out of range, duplicate, mismatched, or missing chunks) is `InvalidInput`.
- **`decode_image_into` reuses the destination.** When the decoded dimensions match the
  destination's, the presentation stage writes into its existing sample storage (no allocation);
  otherwise the buffer is replaced. Error paths fire before any byte is written.

**Encoder-specific notes:**

- **Progressive scan script (frozen).** The progressive encoder ([`JpegEncoder::with_progressive`])
  emits libjpeg's `jpeg_simple_progression` script verbatim (transcribed from libjpeg-turbo's
  `jcparam.c`), SemVer-frozen: **grayscale** is 6 scans — DC `Al=1`; luma AC `1–5` `Al=2`; luma AC
  `6–63` `Al=2`; AC refine `Ah=2 Al=1`; DC refine `Ah=1 Al=0`; AC refine `Ah=1 Al=0` — and **YCbCr**
  is the 10-scan colour variant: interleaved DC `Al=1`; Y AC `1–5` `Al=2`; Cr AC `1–63` `Al=1`;
  Cb AC `1–63` `Al=1`; Y AC `6–63` `Al=2`; Y AC refine `Ah=2 Al=1`; interleaved DC refine `Ah=1 Al=0`;
  Cr, Cb, then Y AC refine `Ah=1 Al=0` (Cr before Cb, exactly as libjpeg orders them).
- **Optimized per-scan Huffman tables (Annex K.2).** The standard Annex K AC tables cannot code a
  progressive AC scan (the `EOBn` run/size bytes are absent), so — like libjpeg, which forces
  `optimize_coding` for progressive — each scan builds its own table(s) from its symbol frequencies
  (a two-pass gather/emit design) via the §K.2 procedure (reserved all-ones pseudo-symbol; 16-bit
  length-limiting). Each scan uses one optimized table at destination 0 for its class (a documented
  free choice; a DC-refinement scan carries no table); this changes only compression density, never
  the decoded coefficients, which are identical to the baseline encoding of the same input.
  **Baseline** takes the same construction only when asked
  ([`JpegEncoder::with_optimized_tables`], P10) — its default output stays the Annex K.3–K.6 typical
  tables, byte-identical to before. Unlike progressive it keeps the two luma/chroma destinations the
  SOS already references, so up to four tables are built and emitted in the one DHT segment the
  fixed path writes; a destination the scan never used (chroma in a grayscale frame) is omitted
  rather than emitted empty. The two passes share `encode_scan`, so a counted symbol and an emitted
  symbol cannot diverge; the forward DCT therefore runs twice and no coefficient buffer is retained.
- **EOBRUN cap / correction-bit buffering.** The EOB-run accumulator is forced out at its 15-bit
  maximum (`0x7FFF`, §G.1.2.2); successive-approximation correction bits (§G.1.2.3) are buffered in a
  growable vector and emitted after the `EOBn`/run-size/ZRL symbol they attach to, so the reference's
  fixed-buffer overflow flush is unnecessary.
- **Metadata segment order: APP0, then EXIF APP1, XMP APP1, ICC APP2, before DQT/SOF.** JFIF
  mandates its APP0 first while Exif 3.0 §4.7.2.1 wants its APP1 immediately after SOI and neither
  spec references the other; APP0-then-APP1 is the libjpeg-family convention that XMP Part 3
  §1.1.3 records readers must accept. Sizes are validated before any bytes are written (EXIF
  ≤ 65527, XMP ≤ 65502, ICC ≤ 255 × 65519 with 1-based chunk indices; empty payloads rejected).
  The framing constants are pinned in `references/jpeg/README.md`.

## Phases

| Phase | Spec | Scope | Status |
| ----- | ---- | ----- | ------ |
| P1 | T.81 Annex A/B/C/F/K; T.871 | **Keystone:** scaffold + workspace wiring; marker/syntax layer; baseline SOF0 Huffman **encoder** (gray + YCbCr 4:4:4/4:2:2/4:2:0), Annex K tables, quality scaling, restart intervals, JFIF | ✅ done |
| P2 | T.81 Annex A/B/C/F; T.871; TN #5116 | Sequential SOF0/SOF1 8-bit Huffman **decoder**: full marker/table parsing (DQT 8/16-bit, DHT with Annex C validation, DRI, DNL, APP0/APP14), interleaved + non-interleaved multi-scan entropy decode (§F.2), restart processing, gray/YCbCr/RGB/CMYK/YCCK colour, sample-replication upsampling, `info()` | ✅ done |
| P3 | T.83 / oracle | libjpeg-turbo differential oracle (vendored, dev-only) + round-trip gate (`tests/oracle.rs`, both directions) | ✅ done |
| P4 | T.81 §G | Progressive SOF2 **decode**: spectral selection + successive approximation — per-component i32 coefficient accumulators filled across scans (interleaved DC + single-component AC), first-pass Huffman + EOBn runs (§G.1.2.2), DC/AC successive-approximation refinement (§G.1.2.3), point transform (§A.4), scan-band ordering validation (§G.1.1.1), restarts, dequant+IDCT once at EOI reusing the sequential reconstruction tail | ✅ done |
| P5 | T.81 §G, §K.2 | Progressive SOF2 **encode**: `JpegEncoder::with_progressive(bool)` — the frozen `jpeg_simple_progression` scan script (6-scan gray / 10-scan YCbCr), coefficients materialized once via the shared gather→FDCT→quantize path, two-pass optimized per-scan Huffman tables (Annex K.2, all-ones-reserved, 16-bit length-limited), DC/AC first-pass + successive-approximation refinement with EOBRUN accumulation and the §G.1.2.3 deferred correction-bit protocol, restarts | ✅ done |
| P6 | — | Hardening: CMYK/YCCK + Adobe APP14 (landed with P2), CLI `gamut convert → .jpg` (`--quality`/`--jpeg-subsampling`/`--jpeg-restart-interval`/`--jpeg-progressive`), umbrella `jpeg` feature audit, facade mutants scoping, full-workspace gate re-run | ✅ done |
| P7 | Exif 3.0 §4.7.2; XMP Part 3 §1.1.3; ICC.1:2001-04 Annex B.4 | APP-segment metadata (rawshift requirements, issue #28 follow-up): `metadata()` header-only read of APP1 EXIF/XMP and index-reassembled multi-segment APP2 ICC; `with_exif`/`with_xmp`/`with_icc_profile` encoder builders with pre-write size validation; bidirectional libjpeg-turbo interop (`jpeg_read_icc_profile`/`jpeg_write_icc_profile`/marker capture) and a dev-only `gamut-metadata` `MetadataBlock` round-trip; `decode_image_into` destination reuse | ✅ done |
| P8 | T.81 §B.1.1.5; issue #277 (seam #272) | **Pluggable codestream backends:** the `backend` module — `JpegStreamInfo`/`DecodedJpeg`/`RasterRef`/`JpegEncodeRequest`, the `JpegStreamDecoder`/`JpegStreamEncoder` traits over the **whole SOI..EOI interchange stream**, `push_backend` push-order registries (`Arc<Mutex<..>>`, so `Clone` shares backends), the `backend_declined` late-decline sentinel, `JPEG_CODEC_ID`, and the `gamut-codec-abi` adapters both ways. APPn metadata + stream validation stay crate-owned: the crate parses the marker layer before consulting a backend and patches its EXIF/XMP/ICC into whatever a backend produces | ✅ done |
| P9 | issue #306 | **Decoder resource limits:** opt-in dimension and native-raster byte builders; checked before built-in frame allocation and backend selection, rechecked on accepted backend output, and converted into a safe sequential DNL MCU-row ceiling | ✅ done |
| P10 | T.81 §K.2; issue #331 | **Optimized baseline Huffman tables:** `JpegEncoder::with_optimized_tables(bool)` — the baseline scan is walked twice through one shared coder (`BaselineCoder::gather`/`::emit`), per-destination symbol histograms drive the §K.2 construction, and the resulting luma/chroma DC+AC tables replace the Annex K.3–K.6 typical ones in the same single DHT segment. Default off, so every previously-encodable configuration stays byte-identical | ✅ done |
| P11 | T.81 §A.3.4, §B.2.4.1; issue #332 | **Caller-supplied quantization tables:** the public `QuantTables` pair — natural order, every entry `1..=255` **by construction** (`new` rejects zero, so the encoder never divides by zero and never emits a DQT its own decoder refuses) — used verbatim via `JpegEncoder::with_quant_tables`, with `annex_k()`/`scaled()` recovering the frozen IJG mapping over arbitrary bases. Quality becomes inert while set; the frozen quality contract still governs the default path; backends are vetoed (a `JpegEncodeRequest` cannot carry tables). Alternate built-in base tables deferred (citation obligation) | ✅ done |
| P12 | T.81 §F.1.2, Annex K.5/K.6; issue #333 | **RD-optimized coefficient selection:** the `rd` module's per-block AC trellis — dynamic program over (position, {v, v−1}) nodes with the exact run/size + ZRL + EOB bit cost of the typical AC tables as rate proxy, step-normalized distortion (the quantization table as the perceptual weighting), tuned dimensionless λ (pinned; measured 8.7% battery-wide saving at ≤ 0.35 dB) — plus `TrellisAdaptive` per-block λ modulation `√(energy/Σstep²)` clamped `[¼, 4]`. Opt-in `with_rd_optimization`; DC plain (deferred: cross-block DP through the DC predictor/restarts); default byte-identical; backends vetoed; progressive carries identical coefficients by the shared `quantize_block_rd` seam + configuration-only rate model | ✅ done |
| P13 | Adobe TN #5116; ISO/IEC 18181-1 (XYB, via the vendored CD + pinned libjxl 0.12.0 constants); issue #334 | **XYB colour mode:** `with_color_mode(JpegColorMode::Xyb)` — sRGB → linear (EOTF LUT) → opsin XYB → scaled-XYB u8 planes (X, Y, B−Y) coded 4:4:4 under SOI → APP14 `transform=0` → EXIF/XMP → APP2 `XYB_ICC_PROFILE` → DQT, SOF ids `R`,`G`,`B`, X/Y on the luminance table and B−Y on the chrominance table (placeholder pairing, ledgered above). The 768-byte profile (input class, D50 XYZ PCS, `desc`/`cprt`/`wtpt`/`chad`/`A2B0` mAB with a 2×2×2 CLUT + cube-root parametric M curves + the frozen 0.5·XYZ(D50)·OpsinInverse matrix/`B2A0` no-op) is vendored static and umbrella-pinned; the decoder needs zero changes (RGB passthrough via APP14/ids). Differential: libjpeg-turbo decodes + test-side XYB inversion ≥ 40 dB on the battery; lcms2 reproduces sRGB from real samples (worst 25 codes at the documented near-black X amplification, mean 2.46) | ✅ done |
| P14 | issue #420; `gamut-metadata` | **Typed metadata wiring** behind the opt-in `metadata` feature: `JpegMetadata::blocks` / `metadata`, `JpegEncoder::with_metadata` / `with_encoded_metadata`; IPTC-IIM / C2PA typed `Unsupported`; pinned by the typed extract → embed → extract equality through the stream. **Oracle cell not covered:** `tooling/exiv2-oracle` is block-level and in-memory (no JPEG reader), so "exiv2 reads the embedded APP1/APP2 payloads out of the JPEG" is untested; the located bytes are pinned byte-exact against libjpeg-turbo (ICC, `tests/oracle.rs`) and the leaf crates pin the payloads against exiv2 (#510) | ✅ done |
