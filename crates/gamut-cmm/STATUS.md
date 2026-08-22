# gamut-cmm — ICC colour management module status

**Epic: GitHub issue #323.** The colour management module (CMM) over the profiles
[`gamut-icc`](../gamut-icc) parses: builds runnable colour transforms (device→PCS→device) and
evaluates them over interleaved `f64` pixels. Data layouts follow **ICC.1:2022**
([`references/icc`](../../references/icc)); scope is the ICC v2/v4 still-image profile set.
Runtime dependencies: `gamut-icc`, `gamut-color`, `gamut-core`; `#![forbid(unsafe_code)]`.

**Keystone:** the **pipeline/stage model** — a colour transform as a validated chain of `Stage`s.
`Pipeline::new` is the validity boundary: every channel count (declared ends, per-stage
input/output, every adjacent seam) is checked exactly once at construction, so a constructed
pipeline always evaluates, allocation-free, by ping-ponging two `[f64; MAX_CHANNELS]` stack
buffers. Every later phase is an additive `Stage` variant plus its `eval` arm (the match is
deliberately exhaustive so the compiler forces both to land together) or a builder that emits
pipelines.

**Oracle:** **Little-CMS (lcms2)** via the dev-only FFI oracle `tooling/lcms2-oracle`
([`references/cmm`](../../references/cmm/README.md)). ICC.1 specifies data layouts, not CMM
behaviour, so where the spec is silent (interpolation, clamping — including `Clamp`'s
NaN → 0.0 choice) observable semantics follow lcms2; differential tests arrive with the phases
that add behaviour (#325 onward).

## Phases

| Phase | Issue | Scope | Status |
| ----- | ----- | ----- | ------ |
| P1 | #324 | Scaffold + keystone: `Pipeline`/`Stage` model, `Transform` entry trait, `CmmError`, workspace wiring | ✅ |
| P2 | #325 | Curve stages: `ToneCurve` (`curveType`/`parametricCurveType` evaluation, monotonicity detection, analytic + lcms2-shaped numeric inversion) + `Stage::Curves` | ✅ |
| P3 | #326 | CLUT stage: multi-dimensional interpolation (lcms2-matching) — `ClutTable`/`ClutInterpolation` + `Stage::Clut` | ✅ |
| P4 | #327 | Profile linking: matrix/TRC (shaper) profile pairs — `link::{device_to_pcs, pcs_to_device}` over RGB/gray v2+v4 shaper profiles, `Stage::MatrixN` | ✅ |
| P5 | #328 | Profile linking: LUT (`lut8`/`lut16`/`mAB `/`mBA `) profile pairs — per-intent tag selection with lcms2's fallback, PCS encode/decode seams, `Stage::XyzToLab`/`LabToXyz` + the Lab-PCS RGB shaper lift | ✅ |
| P6 | #329 | Rendering intents + black-point compensation: `IccTransform::between`/`TransformOptions` (the first end-to-end transform), ICC-absolute white scaling (`intent`), black-point detection + compensation (`bpc`) | ✅ |
| P7 | #330 | Transform chaining (`IccTransform::chain`), device links (`IccTransform::device_link`), soft proofing (`IccTransform::proofing`/`ProofingOptions`), gamut check (`GamutCheck`), typed pixel buffers (`image`), and the epic's max-ΔE₀₀ conformance gate | ✅ |

## Settled decisions (P2, tone curves)

- **Endpoint semantics:** `ToneCurve::eval` clamps domain **and** range to `[0, 1]` in every
  representation, forward and inverse — the convention of `gamut_icc::Curve::eval`, extended to
  parametric curves whose raw closed forms can leave the range.
- **Unknown parametric types:** `gamut_icc::ParametricCurve::eval` silently evaluates a
  `function_type > 4` as the identity (unreachable from parsed profiles, reachable from
  hand-built values); `ToneCurve::new` guards the trap with the typed
  `CmmError::UnsupportedParametricType`.
- **Inversion:** analytic closed forms (lcms2's negated-type formulas, at full `f64` precision —
  a gamma inverse is *not* re-encoded through `u8Fixed8`) for identity, pure gamma, and
  parametric types 1–4 with `g > 0`, `a > 0` (types 3–4 also `c > 0`, `d ∈ [0, 1]`); everything
  else — sampled tables, degenerate-but-monotonic parameterizations — reverses numerically into
  a 4096-entry table shaped after `cmsReverseToneCurveEx` (same entry count, interval-scan
  directions, and flat-run convention as the oracle). Non-monotonic and constant curves are
  rejected with `CmmError::NonMonotonicCurve`.
- **Flat segments:** a flat run's value maps to the run edge adjoining the curve's larger values
  (lcms2's `y2`-for-ascending / `y1`-for-descending choice). One deliberate deviation: a
  reversal target below a *descending* table's minimum maps to the correct domain end `1`,
  where lcms2's carried-coefficient quirk emits `0`; range-spanning tables (every table the
  differential tests share with the oracle) never hit the case.

## Settled decisions (P3, CLUT interpolation)

- **Interpolation modes:** `ClutInterpolation::Tetrahedral` (default for ≥ 3 input channels,
  lcms2's device-CLUT selection) is the exact six-branch `TetrahedralInterpFloat`
  decomposition — `>=` cascade in lcms2's order, so ties resolve identically (transcribed in
  [`references/cmm`](../../references/cmm/README.md)) — with ≥ 4 inputs recursing lcms2's
  `Eval4InputsFloat`… scheme (outermost-axis slice, two inner evaluations, linear blend) down
  to the 3-D tetrahedral base. `ClutInterpolation::Multilinear` (default and only mode for
  1–2 inputs, where the forms coincide) is classic 2ᴺ-corner multilinear at every dimension;
  requesting `Tetrahedral` below 3 inputs is a typed `ClutGeometry` error, not a silent
  fallback. The mode is carried per table (`ClutTable::with_interpolation`) because lcms2
  forces trilinear for **Lab-indexed** CLUTs at profile-read time
  (`ChangeInterpolationToTrilinear`) — #328's linking layer selects it there.
- **Input mapping:** lcms2's `fclamp` (NaN and everything below `1e-9` → `0.0`, above `1.0` →
  `1.0`), `px = fclamp(v)·(n−1)`, floor cell + fraction, and the exact-`1.0` rule (upper node
  = lower node when the clamped input is `≥ 1.0`).
- **Single-node axes (deliberate divergence):** an axis with one grid node interpolates as
  constant. lcms2's 2-D+ float routines have no `Domain == 0` guard and read one node past
  the end there; this crate pins the in-bounds semantics (the differential tests avoid 1-node
  axes).
- **Normalization:** samples normalize once at construction by the CLUT's precision full
  scale (255 for 8-bit data widened to `u16` at parse, 65535 for 16-bit) — never a blanket
  65535.
- **Bounds:** CLUT input dimensions cap at 15 (lcms2 `MAX_INPUT_DIMENSIONS`; ICC device
  spaces stop at 15 colorants), outputs at the pipeline-wide 16.

## Settled decisions (P4, shaper + chad)

- **THE convention — colorants as-is, `chad` never read (the epic's flagged v2/v4 risk):**
  the shaper builders consume `rXYZ`/`gXYZ`/`bXYZ` exactly as tagged, for **v2 and v4
  profiles alike**, and never read the `chad` tag on the relative-colorimetric path. Basis:
  (1) ICC.1:2022 requires colorant tag values to be **already D50-adapted** (§8.3.4's
  PCSXYZ relation and §9.2.44/.28/.11's colorant definitions are D50-relative; the
  measured-to-PCS adaptation is recorded *informatively* in `chad`, §9.2.15); (2) lcms2's
  shaper readers (`ReadICCMatrixRGB2XYZ`, `cmsio1.c:132–152`, feeding both
  `BuildRGBInputMatrixShaper` and `BuildRGBOutputMatrixShaper`) read no chad; and (3) the
  exhaustive audit of every `chad` consumer in lcms2 2.19 (`references/cmm`) finds exactly
  one — `ComputeConversion`'s absolute-colorimetric branch (`cmscnvrt.c:374/377`) — which the
  default adaptation state 1.0 leaves inert even there. A strict reading of *some* legacy v2
  profiles (colorants relative to the actual media white, chad meant to adapt them) would
  disagree; this crate **deliberately matches lcms2 over that strict v2 reading** — the
  documented divergence the issue demands — and the three-way differentials
  (`tests/oracle_shaper.rs`: v2-with-chad vs v2-without-chad bitwise identical; v2 vs v4 of
  the same colorimetry exact; each vs lcms2) pin it.
- **`wtpt` is reserved to absolute intent:** the media white point participates only in the
  ICC-absolute white scaling, which arrives with #329; the relative baseline never reads it
  (again matching lcms2, including its v2-display-class force-D50 quirk — irrelevant until
  #329).
- **Decoded-PCS pipelines — no `InpAdj`/`OutpAdj`:** lcms2 runs shaper pipelines in encoded
  XYZ (`[0, 1]`) and folds `1/MAX_ENCODEABLE_XYZ` (and its reciprocal) into the matrices;
  this crate's PCS seams are decoded colorimetry (crate convention), so the factors are
  omitted. End-to-end lcms2 transforms with `TYPE_XYZ_DBL` formatters produce decoded XYZ,
  so differentials compare directly.
- **Intent parameter inert on the shaper path:** shaper profiles carry no per-intent tables
  (per-intent renderings live in LUT tags — selected since P5) and absolute colorimetric's
  white scaling arrives with #329, so the shaper fallback builds the relative-colorimetric
  baseline for every intent — documented on the functions and pinned by a test.
- **LUT-tag precedence:** lcms2 consults LUT tags before the shaper fallback; a profile
  carrying the requested direction's LUT tags routes to the LUT path (P5, below) and never
  silently uses the shaper tags it may also carry. Lab-PCS *RGB* shapers build via the
  `XyzToLab`/`LabToXyz` bridge stages (landed with P5); the gray Lab-PCS form was supported
  from P4.
- **Gray pipelines:** XYZ PCS is `kTRC(g)·D50` forward and pick-`Y` → `kTRC⁻¹` reverse; Lab
  PCS is `[100·kTRC(g), 0, 0]` forward and pick-`L*/100` → `kTRC⁻¹` reverse (the decoded
  equivalents of lcms2's `GrayInputMatrix`/`PickYMatrix`/`PickLstarMatrix`). The D50 is
  `gamut_color::lab::D50_XYZ` — the s15Fixed16-encoded PCS illuminant §7.2.16 mandates —
  not lcms2's truncated `0.9642/0.8249` constants (≤ 5.5e-6 apart; the differential bound
  documents it).
- **Reverse-direction conditioning:** the colorant matrix inverts via
  `gamut_color::linalg::mat_inv_3x3`, whose exact-zero/non-finite-determinant `None` **is**
  the conditioning threshold (no epsilon — a nearly-collinear but invertible colorant set
  still inverts, as in lcms2); a non-finite inverse entry (overflowed cofactor over a
  denormal determinant) is also `SingularMatrix`. TRC inversion reuses `ToneCurve::inverse`:
  analytic for gamma and well-behaved parametric TRCs (as in lcms2's
  `cmsReverseToneCurveEx`), the lcms2-shaped 4096-entry numeric reversal for sampled tables.

## Settled decisions (P5, LUT-profile linking)

- **Intent→tag selection with the lcms2 fallback:** `device_to_pcs`/`pcs_to_device` index
  lcms2's verbatim `Device2PCS16`/`PCS2Device16` tables (`cmsio1.c:31-50`) — perceptual →
  `A2B0`/`B2A0`, media-relative → `A2B1`/`B2A1`, saturation → `A2B2`/`B2A2`, and
  **ICC-absolute → the media-relative tag** (`A2B1`/`B2A1`; the absolute white scaling is
  #329's, so absolute and relative currently build identical pipelines — pinned bit-for-bit).
  A missing intent tag falls back to the perceptual tag (`_cmsReadInputLUT`/
  `_cmsReadOutputLUT`), then to the matrix/TRC shaper set, then errors: `MissingTag` with
  the intent's primary LUT tag for non-RGB/gray device spaces, the shaper builders' own
  missing-tag errors otherwise. The float `DToBx`/`BToDx` tags, which lcms2 ≥ 2.6 would
  consult *first*, are out of scope with `mpet` (they dispatch as absent — a pre-2.6 lcms2).
- **Domain plan — encoded tag internals, one affine PCS seam:** lcms2 runs whole pipelines
  in encoded `[0, 1]`; this crate's PCS ends are decoded. The builders keep every LUT-tag
  stage (embedded matrix, tables/curves, CLUT, `mAB `/`mBA ` matrix **including its
  offsets**) in the tag's native encoded domain — exactly lcms2's arrangement — and attach a
  single diagonal-affine seam stage at the PCS end: decode appended for device→PCS, encode
  prepended for PCS→device. Constants (derivations in `link/lut.rs`): PCSXYZ scale
  `65535/32768` (= `MAX_ENCODEABLE_XYZ`); v4 Lab `L = v·100`, `a/b = v·255 − 128`; v2 Lab
  `L = v·100.390625` (`65535/652.8`), `a/b = v·255.99609375 − 128` (`65535/256`). A LUT tag
  whose header "PCS" is a device space (devicelink/abstract) gets no seam — encoded end to
  end, as in `_cmsReadDevicelinkLUT`.
- **The v2-Lab rule is keyed on the element type, `lut16Type` only:** lcms2 inserts its
  `LabV4ToV2`/`LabV2ToV4` fixups only when the tag's **true type** is `cmsSigLut16Type` and
  the PCS is Lab — never for `lut8` (whose `FROM_8_TO_16`-widened Lab encoding lands on the
  v4 scaling) or `mAB `/`mBA `, and never keyed on the header version. Here that collapses
  into the seam constants: lut16 + Lab ⇒ v2, everything else ⇒ v4. Pinned by unit
  constant pins, a hand-computed lut16 end-to-end (`0xFF00 → L* = 100` exactly), and the
  v2-vs-v4 profile differentials.
- **The lut8/lut16 embedded 3×3 matrix is *not* XYZ-gated (deliberate divergence from the
  spec's letter, following lcms2):** ICC.1:2022 §10.10/§10.11 say the matrix "shall be" the
  identity unless the input is PCSXYZ; lcms2 (`Type_LUT8_Read`/`Type_LUT16_Read`) applies
  whatever matrix a tag carries whenever `InputChannels == 3` and it is not the identity
  (tolerance `1/65535`, `_cmsMAT3isIdentity`'s `CloseEnough`). This crate follows lcms2.
- **Lab-indexed CLUTs interpolate trilinearly:** in the PCS→device direction of a Lab-PCS
  profile every CLUT is built `Multilinear` (lcms2's `ChangeInterpolationToTrilinear`);
  all other CLUTs keep the tetrahedral-from-3-inputs default. Pinned by a differential
  where the tetrahedral evaluation of the same table visibly misses lcms2 (3-node grid).
- **Lenient stage combinations:** any `mAB `/`mBA ` stage combination the offsets signal is
  accepted (matching `gamut-icc`'s parse and lcms2), with absent stages omitted; a
  combination whose channel counts cannot chain fails `Pipeline::new`'s seam validation
  with a typed error instead of being special-cased. Either LUT element family is accepted
  under either tag slot (lcms2 registers all four readers for both A2B and B2A) — the
  element type fixes the internal order, the direction fixes the PCS seam.
- **lcms2's implicit v4 BPC is #329's scope:** lcms2 *forces* black-point compensation for
  v4 profiles under perceptual/saturation (`_cmsLinkProfiles`, per Adobe's document), keyed
  on each hop's output-side/abstract profile. The P5 differentials neutralize it with a v2
  Lab endpoint (`lab2`) and, for PCS→device perceptual/saturation, a version-downgraded
  twin profile (header 2.4, `mAB `/`mBA ` bytes intact — also pinning that both sides key
  the v2-Lab rule on the true type, not the version); media-relative is additionally
  compared on the true v4 profile.

## Settled decisions (P6, rendering intents + BPC)

- **`IccTransform::between` is lcms2's two-profile assembly** (`DefaultICCintents` +
  `AddConversion`, `cmscnvrt.c`) in the decoded domain: `device_to_pcs(src, intent)` ∘
  [PCS seam] ∘ `pcs_to_device(dst, intent)`. The seam's adjustment matrix acts in XYZ; Lab
  ends are bridged with `LabToXyz`/`XyzToLab`, and an adjustment within lcms2's
  `IsEmptyLayer` tolerance (`Σ|m − I| + Σ|off| < 0.002`) inserts **no stage** — for the
  skip *decision* the offset is compared in lcms2's encoded scale (÷ `MAX_ENCODEABLE_XYZ`),
  keeping the decision bit-identical while the applied offset stays decoded (the offset
  division is the one place lcms2's encoded pipelines and this crate's decoded ones differ
  in the BPC math). Device-link, abstract, and named-colour classes are rejected with
  `UnsupportedProfile` — the first two run via `IccTransform::device_link`/`chain` (P7),
  which read them devicelink-style.
- **Perceptual/saturation add no CMM-side math**: the per-intent tag selection (P5) is the
  entire rendering; for v4 profiles the perceptual tables already target the Perceptual
  Reference Medium and this CMM — like lcms2 — applies **no additional gamut mapping**
  toward the PRMG (`references/cmm/ICCSpecRevision_22_02_05_PRMG.pdf`, `render.pdf`).
- **ICC-absolute = `diag(wIn/wOut)` at adaptation state 1.0 only** (lcms2's default and this
  crate's only state; `ComputeAbsoluteIntent`, `cmscnvrt.c:249-325`): the `chad` tag is
  consumed solely by the non-default adaptation-state branches, so it is **never read**
  here — closing the loop on P4's chad audit. Media whites come from `wtpt` via
  `_cmsReadMediaWhitePoint`'s quirks (`cmsio1.c:64-90`), replicated verbatim: missing or
  unusable tag → lcms2's **rounded** D50 literals (0.9642/1.0/0.8249 — not the exact
  s15Fixed16 PCS illuminant; everywhere P6 replicates an lcms2 *algorithm* the rounded
  anchor is used so the oracle is matched, while the pipeline stages keep the exact one),
  and **v2 display-class profiles are forced to D50** regardless of the tag. BPC and
  absolute are mutually exclusive: the options flag is ignored under absolute
  (`cmscnvrt.c:1126-1127`).
- **The v4 forced-BPC rule** (`_cmsLinkProfiles`, `cmscnvrt.c:1119-1135`): lcms2 forces
  `BPC[i] = TRUE` for every v4 profile slot under perceptual/saturation; only
  output-direction slots are consumed, so for a two-profile transform the **destination
  profile's version** gates the force — replicated in `between`, making default-options
  perceptual differentials against default-flag lcms2 match. The same forcing acts *inside*
  lcms2's own detection round trip (`CreateRoundtripXForm` passes `BPC = FALSE` but still
  runs `_cmsLinkProfiles`), so the ink round-trip probe on a v4 profile silently carries a
  zero-black → destination-black compensation layer — replicated in `bpc`'s `RoundTrip`
  (without it the probe misses the oracle by ≈ 2 `L*`).
- **The `bkpt` tag is deliberately ignored** (lcms2's `CMS_USE_PROFILE_BLACK_POINT_TAG` is
  compiled out by default — the tag is bogus in too many real profiles); black points are
  **estimated** by the transcribed `cmsDetectBlackPoint`/`cmsDetectDestinationBlackPoint`
  (`cmssamp.c`), public as `bpc::{detect_black_point, detect_destination_black_point}`
  (independently testable; #330's proofing needs them). Estimated ⇒ oracle assertions are
  **tolerance-based** on the probe/ramp paths (measured ≤ 2e-6 per component on the shared
  corpus) and exact on the gate/fixed-black paths. Detection quirks replicated as-is:
  absolute intent and link/abstract/named classes → zero; the destination gate checks the
  intent's own `B2Ax` tag with *no* perceptual fallback (`cmsIsCLUT`); the quadratic fitter
  returns `L* = 0` below 4 points while its caller admits 3 (net `L* = 0` at exactly 3);
  the fitter replicates lcms2's `MATRIX_DET_TOLERANCE = 1e-4` singularity cutoff.
- **Zero-black convention, no new error variants**: the detectors return `[0, 0, 0]` for
  "no black point" and for every internal failure — exactly the value lcms2's consumer
  sees (`ComputeConversion` pre-zeroes and ignores the boolean); a pair whose blacks cannot
  be estimated simply gets no compensation. Pipeline-construction failures inside `between`
  still surface as typed errors.
- **One guarded divergence:** lcms2's compensation divides by `bpIn − D50` with no guard —
  a black-point component equal to the anchor yields inf/NaN. This crate treats
  `|bpIn − D50| < 1e-12` (any channel) as equal-blacks/no-op: a plausible-looking
  non-finite cascade is worse than skipping, and no real black point sits at the white
  anchor. Documented on `bpc::compensation`.
- **Out-of-range comparison convention:** lcms2's `TYPE_*_DBL` formatters emit unclamped
  device values for out-of-gamut PCS input, while this crate's `ToneCurve` clamps to
  `[0, 1]` (P2, matching lcms2's own integer formatters); the P6 differentials clamp the
  oracle's doubles before comparing.

## Settled decisions (P7, chaining + conformance)

- **One seam implementation:** `crate::chain::link_chain` transcribes lcms2's
  `DefaultICCintents` (`cmscnvrt.c:510-645`) — per-hop input/output direction from the
  tracked current colour space, devicelink-style reads for link/abstract classes,
  `ComputeConversion` only on output-direction (and abstract, `i > 0`) hops,
  `AddConversion`'s PCS bridging with the empty-layer skip, and lcms2's
  `ColorSpaceIsCompatible` continuity check (violations are the typed
  `CmmError::ChainMismatch`). `IccTransform::between` **is** the two-profile chain;
  `proofing` is the four-profile chain `[src, proof, proof, dst]` at intents
  `[intent, intent, relative, proofing_intent]` with BPC on the first two hops only
  (`cmsCreateProofingTransform`, `cmsxform.c:1365-1395`); the gamut check's three
  sub-transforms run through the same engine against a hand-built Lab-identity abstract
  profile (the `cmsCreateLab4Profile` shape).
- **Single intent/BPC per chain in v1:** the chain internals take per-hop intent/BPC arrays
  (proofing needs them), but the public `IccTransform::chain` replicates one
  `TransformOptions` across every hop — exactly what lcms2's
  `cmsCreateMultiprofileTransform` does with its single intent word, so differentials
  against it are like-for-like. A public per-hop-array API (lcms2's
  `cmsCreateExtendedTransform`) is deferred until a consumer needs it.
- **Device links have no shaper fallback** (`_cmsReadDevicelinkLUT`, `cmsio1.c:705-800`):
  the A2B tag family with the perceptual fallback only — a link without a usable A2B tag is
  `MissingTag`, never a matrix/TRC read. CLUTs interpolate **trilinearly when the link's
  "PCS" field (its output space) is Lab**, the lut16 v2-Lab rule applies at each Lab end,
  and device→device links run encoded `[0, 1]` end to end; Lab/XYZ link ends are seamed to
  the crate's decoded colorimetry (the decoded equivalent of lcms2's encoded pass-through +
  `LabV4ToV2`/`LabV2ToV4` fixups). `IccTransform::device_link` accepts only link-class
  profiles; abstract profiles chain.
- **Per-hop clamping divergence (documented):** mid-chain colorimetry that leaves the v4 Lab
  encodeable range — deep CMYK blacks pushed below zero by a BPC layer, chroma past
  ±128 — clamps at each hop's tag curves (the P2 `[0, 1]` `ToneCurve` convention), where
  lcms2's unclamped *parametric* identity curves carry the overshoot to the final formatter.
  Measured on the conformance battery: ≤ 1.66e-1 device units, only at ≥ 300 % ink corners
  under relative+BPC through the Lab-identity abstract hop (≤ 4.6e-3 for the perceptual
  fixed-black layer; ≤ 3.1e-5 everywhere in-range). The conformance gate splits the two
  regimes with the mid-chain-Lab edge predicate and bounds both.
- **Gamut check emits the raw `f64` excess** (`GamutCheck`): lcms2's `GamutSampler` double
  round trip and ΔE₇₆ decision table (threshold 5.0; 1.0 for a matrix-shaper proof by tag
  presence, `cmsIsMatrixShaper`) are transcribed — the table's two zero arms fold into one
  `dE1 < T` test, provably behaviour-identical (documented on `GamutCheck::excess`; the
  fold removes the redundant comparisons no test could distinguish) — but where lcms2
  quantizes the excess into a 16-bit CLUT sampled on a coarse grid and interpolates, this
  crate evaluates the sampler exactly per pixel and returns the unquantized excess
  (`0.0` = in gamut) — classification is oracle-pinned, magnitudes deliberately are not.
- **Pixel-buffer conventions (`image`):** colour channels normalize by the format's full
  scale (255/65535), re-encode with lcms2's `_cmsQuickSaturateWord` rounding
  (`floor(x + 0.5)`, saturating; NaN → 0); alpha channels never enter the transform (copied
  when both formats carry alpha, dropped when only the source does, opaque-filled when only
  the destination does); `Bilevel`/`Indexed8` are rejected as non-continuous colour
  (`UnsupportedPixelFormat`); geometry violations are `ImageGeometry`/`BufferLength`.
  Processing is chunked (256 pixels) through one reused `f64` scratch pair — constant extra
  memory in the image size; planar buffers gather/scatter per chunk into the same
  interleaved core (identical numerics, one extra copy).
- **Pipeline optimization deferred to #372:** transforms evaluate stage by stage in `f64`;
  lcms2's stage collapsing / CLUT resampling / curve joining (`cmsopt.c`) is a performance
  concern tracked as the follow-up issue #372, not a v1 correctness concern — the
  conformance gate's LOOSE configuration measures exactly the behavioural gap those
  optimizations introduce on the oracle's side.

## Conformance gate (P7)

`tests/oracle_conformance.rs` — the epic's acceptance badge. Battery pairs (shaper↔shaper,
gray↔shaper, shaper↔LUT both directions, LUT↔LUT incl. v2/v4 serializations) × 4 intents ×
BPC {on, off} (BPC×absolute skipped — mutually exclusive), ≥ 50-pixel seeded device sweeps,
outputs compared as ΔE₀₀ through a shared destination→Lab lens. Two oracle configurations
per cell: **TIGHT** (lcms2 `TYPE_*_DBL`, `NOOPTIMIZE|NOCACHE` — evaluation semantics) and
**LOOSE** (lcms2 `TYPE_*_16`, default flags — the optimized path real callers get; only the
"scum dot" white fixup is disabled, `NOWHITEONWHITEFIXUP`, because it would dominate the
metric at the device-white corner, measured 8.5 ΔE₀₀, while gating nothing about this
crate). Measured maxima and asserted bounds (lcms2 2.19):

| class | measured max ΔE₀₀ | asserted | justification |
|-------|-------------------|----------|---------------|
| tight / shaper pairs | 7.82e-4 | `< 2e-3` | lcms2 f32 stage noise + near-black γ-inverse amplification (gray→sRGB perceptual); ~2.5× headroom keeps a decade regression visible |
| tight / LUT pairs | 6.87e-3 | `< 2e-2` | the oracle evaluates profile CLUTs through 16-bit fixed point even in double transforms (`EvaluateCLUTfloatIn16`); grid-9 tables bound the agreement |
| loose / shaper pairs | 3.34e-1 (mean 1.49e-2) | max `< 6e-1`, mean `< 5e-2` | lcms2's default path resamples the transform into a grid-33 CLUT: deep-shadow toes of γ2.2 and gamut-clip corners interpolate coarsely **on the oracle's side** (at those pixels we match the TIGHT oracle to ~1e-3) |
| loose / LUT pairs | 1.10e0 (mean 7.23e-3) | max `< 2.0`, mean `< 3e-2` | precalculated-CLUT smoothing of clip boundaries (absolute-intent white scaling clips hardest); the mean bound is the regression tripwire — a wrong tag/seam/BPC shifts whole sweeps |

The gate also asserts the classes stay *distinct* (LUT maxima dominate shaper maxima), so
the split cannot silently go stale. Also inside the gate: 3-profile chains (src→Lab4→dst,
both directions) vs `cmsCreateMultiprofileTransform` (in-range worst |Δ| 3.1e-5, clamped
regime per the P7 divergence above); device links — the v4 mAB ink-limiting link (worst
1.5e-5), the curves-only linearization link (1.5e-8), and a hand-built v2 lut16 RGB→Lab
link pinning the devicelink trilinear rule and v2-Lab seams (worst 3.4e-3 in decoded-Lab
units, a bound the tetrahedral evaluation of the same CLUT fails); proofing vs
`SOFTPROOFING` (worst 3.4e-5 across intents × BPC, plus an observability floor against the
plain pair transform); and gamut-check in/out classification agreement per pixel against
`GAMUTCHECK` alarm substitution over sentinel alarm codes (shaper and LUT proofs, two
intents), with our excess pinned to exactly `0.0` in-gamut.

## Deferred / out of scope

| Item | Notes | Status |
|------|-------|--------|
| iccMAX (`ICC.2:2019`) | A separate, parallel next-generation format (spectral PCS, v5 header); not an extension of ICC.1 and unimplementable against the lcms2 oracle. See [`references/icc`](../../references/icc/README.md). | ✗ out of scope |
| `multiProcessElementsType` (`mpet`) + `DToBx`/`BToDx` tags | The v4/iccMAX general-purpose processing pipeline; `gamut-icc` preserves it as `Raw`, and this CMM does not evaluate it. | ✗ out of scope |
| Integer/`f32` fast paths | Evaluation is `f64` throughout at Tier-1 (correctness only, not bit-reproducible — the `gamut-color` posture, see [`references/color`](../../references/color/README.md)). | ☐ unplanned |
| Pipeline optimization / stage collapsing | lcms2 collapses chains into resampled LUTs by default (`cmsopt.c`); v1 evaluates stage by stage. Follow-up issue **#372** tracks it. | ☐ deferred (#372) |
| Public per-hop intent/BPC arrays | The chain internals already take them (proofing uses them); the public `chain` API stays single-intent until a consumer needs lcms2's `cmsCreateExtendedTransform` shape. | ☐ deferred |
| Named-colour profiles | No continuous pixel transform; rejected with a typed error everywhere. | ✗ out of scope |
| K-preserving intents (`INTENT_PRESERVE_K_*`) | lcms2 extensions beyond the four ICC intents (`cmscnvrt.c` dispatch table); no ICC.1 basis. | ☐ unplanned |

## Validation

Inline unit tests (stage evaluation against hand-computed exact-dyadic values, clamp semantics
incl. NaN, object safety; tone-curve internals — reversal scan directions, flat-run edges,
out-of-range clamps, closed-form inverses — pinned against hand-derived exact values; CLUT
internals — node addressing/interleaving, per-branch tetrahedral probes and exact-tie branch
ordering against hand-transcribed formulas, multilinear vs an independent naive corner-weight
implementation up to 4-D, the measured tetrahedral-vs-trilinear divergence bound, `fclamp` and
single-node-axis edges, 15-D acceptance and geometry rejection) plus the `tests/pipeline.rs`
integration suite (construction-time rejection with exact typed variants and fields, boundary
channel counts, empty-pipeline identity, multi-pixel `Transform` buffer contract, composition,
per-channel `Stage::Curves` evaluation, curves → CLUT → matrix hand-checked pixels) and the
differential suites against lcms2: `tests/oracle_curves.rs` (forward sweeps for
identity/gamma/sampled/all five parametric types, inversion vs `cmsReverseToneCurveEx`,
analytic-vs-numeric inverse agreement, round-trip batteries over gammas, parametric curves, and
seeded random tables with and without flat runs), `tests/oracle_clut.rs` (float-pipeline
sweeps against lcms2's `TetrahedralInterpFloat`/`Eval4InputsFloat`/1-D/2-D interpolators to
f32-rounding tightness, plus end-to-end `cmsDoTransform` sweeps over synthesized devicelink
CLUT probe profiles to 16-bit-quantization tightness), and `tests/oracle_shaper.rs` (shaper
linking: device→PCS and PCS→device sweeps for sRGB/Display P3/Adobe-ish/gray v2+v4 profiles
against end-to-end lcms2 transforms over the **same serialized bytes**, to f32-rounding
tightness in XYZ plus ΔE₀₀ bounds; the three-way chad cases; analytic round trips; the
assembled sRGB matrix pinned to Lindbloom's published D50-adapted values; LUT-precedence and
error-path pins — the P4/P5 linking unit tests hand-build `gamut-icc` profiles for the
missing/mistyped-tag, singular-matrix, Lab-PCS, and dispatch boundaries, per-tag stage-order
fingerprints for all four LUT element types in both directions, the exact PCS seam constants
per encoding, intent-table/fallback selection, and the identity-matrix skip), and
`tests/oracle_lut.rs` (LUT linking: per-intent A2B/B2A differentials for the CMYK `prtr`
profile in its v4/`mAB ` and v2/`lut16` serializations and the `scnr` RGB→Lab `mAB `,
16-bit-CLUT-tight; the fallback differential over a modified-and-reserialized profile; the
absolute≡relative and per-intent-distinctness pins; the Lab-indexed-trilinear divergence
proof; and the hand-built Lab-PCS RGB shaper vs lcms2's XYZ2Lab/Lab2XYZ bridges), and
`tests/oracle_intents.rs` (P6: the absolute differential on a genuinely non-D50 `wtpt`
shaper pair — measured worst device |Δ| 1.6e-7, ΔE₀₀ 1.7e-5 — plus the absolute≠relative
observability floor and the equal-whites bitwise-equality pin; detection differentials for
every path family — matrix-shaper colorant probe, gray, fixed v4 perceptual black
(bitwise), ink round trip, relative and perceptual ramp estimators, gate refusals — worst
measured 2e-6 per component; end-to-end BPC flag-on/flag-off differentials on an all-v2
pair (worst 5e-5) with an observability floor; and the v4 default-options perceptual
differential that rides on the replicated forced-BPC rule, with a version-downgrade
control; and the **pathological-profile zoo** — hand-built gamut-icc profiles whose
256-sample return curves control the estimator's round-trip ramp sample-exactly on both
sides, steering it into every control-flow corner (the straightness band with and without
its protected window, band-edge arithmetic, top-down monotonization via a late dip, the
non-ascending rejection, the exactly-3-points fit quirk with kept chroma, the chroma
clamps via a mixing CLUT, and the XYZ-PCS bridge with a root clamped to `L* = 50`), each
asserted as a plain detection differential against lcms2 over the same serialized bytes.
P6 unit tests pin the compensation formula's defining property and hand-computed
literals, the D50-anchor guard, the fitter's roots/degeneracies/`n < 4` quirk and its
`MATRIX_DET_TOLERANCE` cutoff (solved by Cramer's rule over the same determinant, so the
transcription is observable), the detection gates and v2/v4 branch selection, the device
endpoints per space (gray/RGB/Lab/CMY/CMYK), the ramp reject/shortcut/toe-fit paths over a
hand-built round-trip vehicle, the `wtpt` quirk table, the empty-layer threshold on both
sides of 0.002 (offset in encoded scale, including the exactly-representable boundary),
every seam shape, and the forced-BPC gate matrix). P7 adds `tests/oracle_conformance.rs`
(the conformance gate above) and its unit layer: chain tests (between ≡ two-profile chain
across intents × BPC, the abstract-identity transparency pin, space-continuity and
named-colour rejections, `ColorSpaceIsCompatible` and the `AddConversion` default-arm
quirk, devicelink no-shaper-fallback/perceptual-fallback/encoded-end-to-end pins,
mid-chain device links, proofing shape + degenerate-identity case), gamut-check tests
(ΔE₇₆ pin, the shaper/LUT threshold rule incl. the tag-presence quirk, exact-zero
in-gamut vs positive excess on a gain-halved proof, buffer contract, object safety),
devicelink `LinkMode` seam/trilinear unit pins, and the `image` suite (round-half-up
saturation pins incl. NaN, interleaved == scalar path exactly at both widths across chunk
boundaries, planar == interleaved, alpha copy/drop/opaque-fill in both layouts, the
Rgb8→Cmyk8 model change, every `UnsupportedPixelFormat`/`ImageGeometry`/`BufferLength`
error, empty buffers, and a doctest). Gates: `mise run test` / `lint` /
`fmt-check` / `coverage` (≥ 80%) / `mise run mutants-crate gamut-cmm` (equivalent
float-boundary survivors are excluded in `.cargo/mutants.toml` with per-mutant proofs, the
workspace convention).
