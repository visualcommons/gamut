# gamut-cmm — ICC colour management module status

**Epic: GitHub issue #323.** The colour management module (CMM) over the profiles
[`gamut-icc`](../gamut-icc) parses: builds runnable colour transforms (device→PCS→device) and
evaluates them over interleaved `f64` pixels (and, through the precision tiers, `u8`/`u16`
codes in fixed point). Data layouts follow **ICC.1:2022**
([`references/icc`](../../references/icc)); scope is the ICC v2/v4 still-image profile set.
Runtime dependencies: `gamut-icc`, `gamut-color`, `gamut-core`; `#![deny(unsafe_code)]` — a
hot-path crate, so a benchmarked exception may be taken (AGENTS.md, `## Conventions`), and none
is: the crate is 100% safe Rust.

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
| P8 | #372 | Pipeline optimization: the opt-in `PipelineOptimization` knob (`optimize`) — identity elision, matrix folding, curve joining, CLUT resampling — threaded through every transform constructor, default off, with divan throughput benches | ✅ |
| — | #555 | `TransformOptions::prefer_cicp_transfer`: closed-form evaluation of a shaper profile's `cicp`-signalled transfer, checked against its TRC; default off on the measured trade (below) | ✅ |
| — | #423 | Precision tiers: `PipelineOptimization::PrecalculateF32`/`PrecalculateFixed16` (`NodePrecision`) — the resampled CLUT at `f32` or 16-bit fixed point, bit-exact against lcms2's interpolators, with integer `Transform::transform_u8`/`transform_u16` entry points under the image functions; each tier budgeted in the conformance gate. SIMD is separate (deferred table); the 8-bit matrix-shaper path is #992 (next row) | ✅ |
| — | #992 | 8-bit matrix-shaper: lcms2's `OptimizeMatrixShaper` (256-entry Q1.14 input tables, Q1.14 matrix, 16 385-entry output tables) run by `transform_u8` of an `IccTransform` at `PrecalculateFixed16` whose link collapses to curves → matrix → curves; bit-exact against lcms2's `TYPE_RGB_8` transform over the whole 8-bit cube, budgeted in codes against the `f64` reference | ✅ |

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
- **Pipeline optimization is opt-in (P8, #372):** transforms still evaluate stage by stage
  **by default** — `PipelineOptimization::None`, the value every `TransformOptions`/
  `ProofingOptions` default carries, so v1's documented numerics are what an unconfigured
  transform produces. The collapsed forms are a knob, not a policy, precisely because the
  conformance gate's LOOSE configuration already measures what they cost.

## Settled decisions (P8, pipeline optimization)

- **One knob, on every constructor.** `PipelineOptimization` is a fieldless `#[repr(u32)]`
  enum with permanent append-only discriminants (the workspace C-portability convention, as
  on `ClutInterpolation`), carried as a field of `TransformOptions` and `ProofingOptions`.
  `IccTransform::device_link` and `GamutCheck::new` were widened from a bare
  `RenderingIntent` to the same `TransformOptions` so no constructor can silently miss the
  knob (a breaking signature change, taken while the crate is still unpublished; both
  document that they consume `intent` + `optimization` and ignore `black_point_compensation`,
  which neither construction has ever applied). Every `IccTransform` constructor funnels
  through `IccTransform::from_pipeline`, which is where the level is applied.
- **Three levels, cumulative.** `None` (default) → `Collapse` (identity elision + adjacent
  affine folding, including `MatrixN` shape changes) → `Precalculate` (+ curve joining and
  whole-pipeline CLUT resampling). Elision and folding run to a fixpoint; termination is
  structural, since each rewrite strictly shortens the stage list.
- **Resampling is gated on the domain, not guessed.** The whole chain collapses into one
  CLUT only when its first stage confines its input to `[0, 1]` (`Curves`/`Clut`/`Clamp`) and
  its last stage confines its output likewise — so a PCS-entering pipeline (which the link
  module always starts with the PCS *encode* matrix) is never resampled onto a unit-cube grid
  it does not live on, and a PCS-leaving one is never squeezed into a `ClutTable`'s
  normalized nodes. Also declined: a chain of fewer than two stages (already one lookup), and
  a grid above 2²⁰ nodes (`f64` nodes make the ceiling worth pinning; lcms2 has none).
- **lcms2's constants, not invented ones.** Grid resolution follows
  `_cmsReasonableGridpointsByColorspace`'s default arm (`cmspcs.c`): 33 nodes/axis up to 3
  inputs, 17 for CMYK, 7 above four. Joined curves tabulate at 4096 points
  (`PRELINEARIZATION_POINTS`, `cmsopt.c:418`). One deliberate divergence: no white-point
  "scum dot" fixup (lcms2's `PatchLUT`) is applied — the same fixup the conformance gate
  disables on the oracle's side with `NOWHITEONWHITEFIXUP`.
- **Exactness where it is free.** Identity elision is exact-equality only (a near-identity
  matrix is a real, if small, colour operation and stays); joining a structurally-identity
  curve returns the other curve unchanged rather than tabulating it.

### Precision budget (P8)

Measured over the conformance battery (`tests/oracle_conformance.rs`,
`optimized_transforms_stay_inside_the_precision_budget`, lcms2 2.19):

| level | metric | measured max | asserted | justification |
|-------|--------|--------------|----------|---------------|
| `Collapse` | device units vs our own `None` | 7.06e-16 | `< 1e-12` | folding only re-associates the same products; 1e-15 is ~3 ulps of a sample. Measured in device units because the gate's ΔE₀₀ lens is an lcms2 transform whose internal floats are f32 — through the lens this reads a flat 0 |
| `Precalculate` | ΔE₀₀ vs our own `None` | 1.08e-1 shaper / 6.35e-2 LUT | `< 1.0` / `< 5.0`, **and** `< 3e-1` / `< 2e-1` | **the LOOSE row of the conformance table above, verbatim** — `Precalculate` builds the very construction (grid-33/17 resampled CLUT, 4096-point joined curves) that row already measures on lcms2's side, so the budget is "no worse than lcms2's own default path", not a fresh number. Since #453 re-measured that row at 5.0, the contract sits two decades above what the pass does, so a **drift guard** at ~3× the measured maxima is asserted alongside it — the contract is the promise, the guard is what would catch a regression |
| `Precalculate` | ΔE₀₀ vs lcms2 at default flags | 2.98e-3 shaper / 7.12e-3 LUT | `< 1.0` / `< 5.0`, **and** `< 3e-1` / `< 2e-1` | the evidence for that reading: matching lcms2's construction lands the optimized output **two decades closer** to lcms2's optimized output than the unoptimized path's 3.92e-1 |

### Throughput (P8)

`cargo bench -p gamut-cmm` (`benches/pipeline.rs`, divan; 256×256 interleaved `u8` buffer,
median of 100 samples; one machine, so read the ratios, not the absolute times):

| scenario | `None` | `Collapse` | `Precalculate` | lcms2 default | lcms2 `NOOPTIMIZE` |
|----------|--------|------------|----------------|---------------|--------------------|
| sRGB → Display P3 | 8.53 ms (7.7 Mpx/s) | 8.19 ms (8.0) | **3.03 ms (21.6)** | 0.46 ms (141) | 8.15 ms (8.0) |
| sRGB → CMYK `prtr` (grid-9) | 15.04 ms (4.4) | 14.93 ms (4.4) | **3.26 ms (20.1)** | 1.22 ms (53.7) | 15.06 ms (4.4) |

So: `Precalculate` is **2.8×/4.6×** the default path's throughput, `Collapse` alone is worth
a few percent (it removes stages, not `powf` calls), and this crate's stage-by-stage
evaluation sits right on lcms2's own stage-by-stage baseline. The remaining gap to lcms2's
default path is representational, not structural — lcms2 runs a `u16` fixed-point CLUT under
8-bit formatters where this crate interpolates `f64` (the precision tiers #423 added — see
"Throughput (#423)" below). Building the resampled transform costs 4.8 ms against 0.16 ms
unoptimized (35 937 pipeline evaluations for a 33³ grid), so the tier pays off from roughly
one 256×256 buffer onward; its `f64` node table is 35 937 × 3 × 8 B ≈ 0.86 MB at 3×3 channels
(this line said ~6.9 MB until #423 re-derived it; the `f32` and 16-bit tables are 0.43 MB
and 0.22 MB).

## Settled decisions (#423, precision tiers)

- **Tiers are optimization levels, not a separate knob.** `PipelineOptimization` gains
  `PrecalculateF32 = 3` and `PrecalculateFixed16 = 4` (append-only discriminants), so every
  constructor that already takes the knob — `between`, `chain`, `device_link`, `proofing`,
  `GamutCheck::new` — reaches the tiers with no new parameter. Each is the `Precalculate`
  construction (lcms2's grid, joined curves, interpolation mode) with the resampled CLUT stored
  at a narrower `NodePrecision` (`F64 = 0`, `F32 = 1`, `Fixed16 = 2`, read back through
  `ClutTable::node_precision`). Tables built from profiles stay `F64`; `f64` is the Tier-1
  reference every tier is measured against.
- **Each tier is lcms2's own arithmetic, bit for bit.** `F32` runs the reference interpolators
  generically at `f32` — lcms2's float interpolators are `cmsFloat32Number` — and matches
  `ClutPipeline` (`cmsPipelineEvalFloat`) exactly in 1-D, 3-D and 4-D. `Fixed16` transcribes
  `Eval1Input`/`LinLerp1D`, `BilinearInterp16`, `TetrahedralInterp16` and the
  `Eval4Inputs`…`Eval15Inputs` recursion, with `_cmsToFixedDomain`, `LinearInterp` and the
  `0x8001` single-shift rounding, and matches lcms2's 16-bit `cmsDoTransform` over a probe
  devicelink exactly for 1→1, 1→3, 3→3, 3→4, 4→3 and 4→4 (`tests/oracle_clut.rs`). The
  tetrahedron is selected by ordering the fractions rather than by lcms2's branch cascade:
  ties sit on a face two tetrahedra share and `Rest` is an exact integer sum, so the two
  agree to the bit (pinned against a transcription of the cascade, ties included).
- **One guarded divergence: `i64` accumulation.** lcms2 accumulates `TetrahedralInterp16`'s
  `Rest` and `BilinearInterp16`'s `LERP` in `int`, which signed-overflows when one cell steps
  more than half the code range; the wrapped result is off by a code. This crate accumulates
  in `i64` and is exact there. `LinearInterp`'s 32-bit *unsigned* wrap is not a divergence —
  it reproduces the arithmetic shift for every in-range result, which the unit tests check
  against a wrap-around transcription. No smooth resampled transform steps that far in one
  cell; the differential stays on half-range tables where both are defined.
- **Fixed-point construction rules.** A `Fixed16` table needs ≥ 2 nodes per axis (lcms2's
  integer routines have no single-node guard, so there is no fixed-point original for the
  float path's constant-axis semantics) and tetrahedral interpolation from 3 inputs (lcms2's
  `TrilinearInterp16` serves only Lab-indexed profile tables, which are never resampled).
  Every grid the resampling builds satisfies both; `ClutTable` rejects the rest with
  `ClutGeometry`.
- **Integer entry points on `Transform`.** `transform_u16`/`transform_u8` are *provided*
  methods (object safety kept): normalize by 65535/255, call `transform`, re-encode with
  `_cmsQuickSaturateWord` rounding — exactly the `image` module's old per-sample conversion,
  so every existing transform keeps its numerics bit for bit. `transform_interleaved_*`/
  `transform_planar_*` now route colour channels through them. A `Pipeline` that is one
  `Fixed16` CLUT overrides both and never touches a float: 8-bit codes widen with
  `FROM_8_TO_16` (`v·257`) and narrow with `FROM_16_TO_8` (`(c·65281 + 2²³) >> 24`), lcms2's
  8-bit formatter conversions — which agree with the `f64` route's round-half-up for every
  code, so the fast path is a speed choice only (its one selection mutant is excluded in
  `.cargo/mutants.toml` on that argument). The `f64` entry point over a `Fixed16` table
  quantizes its input with the same encoding, so all three entry points agree.
- **SIMD: no `unsafe`, and the `unsafe`/portable-SIMD trade is a maintainer decision.** #423
  asks for that decision to be recorded *before* any SIMD lands. This change takes none of it:
  the tiers are scalar safe Rust (the fixed-point inner loop is shaped for autovectorization —
  corner offsets resolved once per pixel, a branch-free per-channel loop — but nothing relies
  on it), and `#![deny(unsafe_code)]` is untouched. Whether `std::simd` or explicit
  `#[target_feature]` intrinsics are worth an `#[expect(unsafe_code)]` exception is handed to
  the maintainers as its own issue (deferred table).
- **Not hand-written: the changelog.** release-plz generates `CHANGELOG.md` from the
  conventional commits; the `feat(gamut-cmm)!` commit is the entry.

### Precision budget (#423)

Measured over the conformance battery (`tests/oracle_conformance.rs`,
`precision_tiers_stay_inside_their_budget`, lcms2 2.19; 8 pairs × 2 intents × 40 pixels, each
fed the same 16-bit-quantized device values):

| tier | metric | measured max (shaper / LUT) | asserted | justification |
|------|--------|-----------------------------|----------|---------------|
| `PrecalculateF32` | device units vs our `Precalculate` | 1.04e-7 (both) | `< 3e-7`, and `> 0` | measured in device units for `Collapse`'s reason — the ΔE₀₀ lens is an f32 lcms2 transform and cannot see an f32 last place. The bound *is* the tier's definition: a few f32 ulps (2⁻²⁴ ≈ 6e-8) through one interpolation; `> 0` proves the tier did not silently run in `f64` |
| `PrecalculateFixed16` | ΔE₀₀ vs our `Precalculate`, through `transform_u16` | 2.88e-3 / 7.12e-3 | `< 1.0` / `< 5.0`, **and** `< 1e-2` / `< 2e-2`, and `> 1e-5` | **the LOOSE row verbatim** as contract — this is the representation lcms2's default 16-bit path runs — with a ~3× drift guard beside it, as for `Precalculate`. The error is 16-bit node quantization (≤ ½ code) plus 16.16 rounding (≤ 1 code), worst in srgb→p3's deep shadows and at the grid-9 printer's ends |
| `PrecalculateFixed16` | ΔE₀₀ vs lcms2 default path, through `transform_u16` | 3.29e-3 / 6.87e-3 | same | where `Precalculate` already lands (2.98e-3 / 7.12e-3): sharing the representation adds nothing measurable; what remains is how each side computes its nodes (lcms2 samples its f32 stages at 16-bit-quantized node coordinates) |

### Throughput (#423)

`cargo bench -p gamut-cmm --bench pipeline -- shaper_ --sample-count 60` (256×256 interleaved
`u8`, **fastest** of 60 samples, every bar in one run). The host was heavily loaded (load
average ≈ 37 on 16 cores), so absolute times sit above the P8 table's and only the ratios
within this table are meaningful:

| scenario | `None` | `Precalculate` | `PrecalculateF32` | `PrecalculateFixed16` | lcms2 default | lcms2 `NOOPTIMIZE` |
|----------|--------|----------------|-------------------|-----------------------|---------------|--------------------|
| sRGB → Display P3 | 12.76 ms (5.1 Mpx/s) | 4.59 ms (14.3) | 5.01 ms (13.1) | **2.26 ms (29.1)** | 0.67 ms (97.5) | 8.22 ms (8.0) |
| sRGB → CMYK `prtr` (grid-9) | 15.61 ms (4.2) | 3.14 ms (20.9) | 3.82 ms (17.1) | **1.79 ms (36.6)** | 1.59 ms (41.1) | 16.36 ms (4.0) |

So the fixed-point tier is **2.0×/1.75×** `Precalculate`'s throughput, and on the CLUT
scenario it is within **1.13×** of lcms2's default path — the same representation lcms2 runs
there (`OptimizeByResampling` → `TetrahedralInterp16` between `FROM_8_TO_16` and
`FROM_16_TO_8`), so what is left is per-pixel overhead, not arithmetic. The shaper pair stays
**3.4×** behind because lcms2 does not use a CLUT there at all: its 8-bit `OptimizeMatrixShaper`
path is a different construction, which #992 added ("Throughput (#992)" below). The `f32` tier is *slower* than `f64`
(0.92×/0.82×): scalar `f32` arithmetic costs the same as `f64` on this hardware and the tier
adds an input narrowing and an output widening per pixel. What it buys is lcms2's float
arithmetic bit for bit and half the node memory — not speed — which is why the fixed-point
tier, not the `f32` one, is the throughput tier.

## Settled decisions (#992, 8-bit matrix-shaper)

- **Selected under `PrecalculateFixed16`, at the 8-bit entry point, on `IccTransform`.** No new
  `PipelineOptimization` level (adding a variant to the exhaustive enum would be a breaking
  change for no new choice a caller makes): the fixed-point tier is "lcms2's integer
  representation under its 8- and 16-bit formatters", and for an 8-bit RGB → RGB matrix/TRC
  link that representation *is* `OptimizeMatrixShaper`. lcms2 picks the pass from the input
  format at transform creation; this crate's transforms learn the format per call, so the
  transform builds the matrix-shaper beside its resampled CLUT (≈ 52 KB more) and
  `transform_u8` runs it, while `transform_u16` and the `f64` entry point keep the CLUT — the
  input tables are indexed by the 8-bit code itself. `Pipeline::optimized` returns a
  `Pipeline`, which has no 8-bit-only representation, so a bare pipeline never carries one.
- **Selection is the `Collapse` passes' shape, not lcms2's `PreOptimize`.** The pass runs when
  the collapsed link is exactly curves → 3×3 matrix → curves, declines lcms2's
  near-identity-without-offset case (`_cmsMAT3isIdentity`, `1/65535`, where lcms2 joins the
  curves instead), and declines a matrix whose Q1.14 accumulation could overflow `i32` (row
  magnitudes summing past ≈ 8 — C leaves lcms2's overflow undefined; no realistic colorant pair
  comes near). It therefore also runs on links with a folded seam adjustment (absolute intent,
  BPC), where lcms2's `_MultiplyMatrix` stops at the offset-carrying matrix and resamples, and
  does not run on Lab-PCS shaper pairs, whose `XYZ2Lab`/`Lab2XYZ` round trip lcms2 removes and
  `Collapse` keeps.
- **lcms2's widths, transcribed.** Input entries are the curve at the `f32` code `i / 255`,
  rounded to `f32` (`cmsEvalToneCurveFloat`), then `DOUBLE_TO_1FIXED14`; output entries are the
  curve at `i / 16384`, rounded to `f32`, encoded with `_cmsQuickSaturateWord` — including its
  fast floor, which rounds to the nearest `2⁻¹⁶` first and so differs from `floor` within `2⁻¹⁷`
  of an integer — and narrowed with `FROM_16_TO_8`. The output tables keep that byte rather
  than lcms2's `FROM_8_TO_16` word (its formatter narrows it straight back): same result, half
  the memory. A `ToneCurve` clamps its output to `[0, 1]`, so lcms2's `≥ 8.0` input-table arm and
  its output-side clamp have nothing to act on and are not transcribed.
- **One deliberate divergence: the offset's scale.** `MatShaperEval16` adds the Q1.14 offset to
  the Q2.28 matrix product before its `>> 14`, so lcms2 applies a matrix-shaper offset `2¹⁴` too
  small. lcms2 never reaches the pass with a non-zero offset on a two-profile link (above); this
  crate does, so it keeps the offset in Q2.28. The two agree to the bit whenever the offset is
  zero; the unit test `mat_shaper_tracks_the_f64_chain_offset_included` holds a 0.0625 offset to
  the `f64` chain, where lcms2's scale would miss by ~15 codes.
- **Sampled curves are not bit-exact.** lcms2 evaluates a `curveType` table through its 16-bit
  table interpolation; `ToneCurve` interpolates it in `f64`. A profile with sampled TRCs runs the
  same pass and lands within the budget below, but can differ from lcms2 by a code where the two
  round differently. The battery's RGB shapers are parametric and pure-gamma.

### Precision budget (#992)

Measured over `tests/oracle_conformance.rs` (lcms2 2.19) on the RGB matrix/TRC pairs srgb→p3,
p3→srgb, wide→srgb, srgb→wide and wide-d65-wtpt→srgb:

| metric | measured | asserted | justification |
|--------|----------|----------|---------------|
| codes vs lcms2 `TYPE_RGB_8`, default flags, whole 8-bit cube (`mat_shaper_8_matches_lcms2_rgb8_bit_for_bit`) | 0 mismatches, every pair at perceptual and relative | `== 0` | the pass *is* lcms2's; the two intents are the ones with no seam adjustment, where lcms2 selects it too. A CLUT at any precision does not reach this, so the equality is also the evidence the pass ran |
| codes vs our `None` through `transform_u8` (`mat_shaper_8_stays_inside_its_budget`, 4 intents × 4096 pixels) | max 2 (srgb→wide), mean 8.84e-3 | `≤ 2` | stated in codes because the 16-bit LOOSE rows do not transfer: one 8-bit code in a γ2.2 toe is itself ~1 ΔE₀₀. 2 is also the whole-cube maximum (1 for srgb→p3 and wide→srgb) and is inherent: Q1.14's first step above black, 1/16384 linear, is 3.1 codes of a γ2.2 encoding. An integer bound has no headroom to give — a third code is a regression |
| mean ΔE₀₀ of those codes, destination lens | 5.83e-3 (max 1.38e0 at srgb→wide saturation — that one-to-two-code toe) | `< 2e-2` | the sensitive drift guard, ~3.4× the measured mean, as the LOOSE means are: a structural mistake (a wrong curve, a lost offset) moves it by orders of magnitude |

The conformance gate does not loosen: `f64` stays the Tier-1 reference and every existing row is
unchanged.

### Throughput (#992)

`cargo bench -p gamut-cmm --bench pipeline -- shaper_ --sample-count 60` (256×256 interleaved
`u8`, **fastest** of 60 samples, one run; load average ≈ 23 on 16 cores, so read the ratios):

| scenario | `None` | `PrecalculateFixed16` CLUT (`precalculate_fixed16`) | 8-bit matrix-shaper (`mat_shaper_8`) | lcms2 default | lcms2 `NOOPTIMIZE` |
|----------|--------|------------------------------------------------------|--------------------------------------|---------------|--------------------|
| sRGB → Display P3 | 13.01 ms (5.0 Mpx/s) | 2.23 ms (29.3) | **0.52 ms (126.3)** | 0.63 ms (103.5) | 12.87 ms (5.1) |

So the matrix-shaper is **4.3×** the fixed-point CLUT on the shaper pair and closes the 3.4× gap
#423 left: it now runs **1.22×** lcms2's default path. Part of that margin is the harness, not the
arithmetic — `lcms2_oracle`'s `apply_u8` allocates its output per call, which the gamut bars do
not (`benches/pipeline.rs`) — so read it as parity. Building it costs 256 + 16 385 curve
evaluations per channel on top of the resampling. The `precalculate_fixed16` bar of the shaper
pair now times a bare `Pipeline` (which carries no matrix-shaper), so it keeps measuring the
CLUT the #423 table measured.

## Settled decisions (#555, CICP-signalled transfer characteristics)

`TransformOptions::prefer_cicp_transfer` (and its `ProofingOptions` twin; `GamutCheck::new`
honours it on all three legs) evaluates a matrix/TRC profile's tone curves in closed form from
the ITU-T H.273 transfer its `cicp` tag (ICC.1:2022 §10.3) names. It is **shipped, default off**,
on the measurements below; moxcms's equivalent (`allow_use_cicp_transfer`) defaults on, and that
analogy was deliberately not followed.

- **Closed forms come from `gamut-color`.** `standard_eotf_for`/`standard_oetf_for` — the
  standards-pure dispatch whose PQ arm is ST 2084 normalized to its 10 000 cd/m² peak, not
  `eotf_for`'s encoder-exact SDR tone map — reach code points 1, 6, 8, 13, 14, 15, 16 and 18
  (HLG, scene-referred, added with this work; 6 and 15, the same curve as 1 and 14 in H.273
  Table 3, since #675). Code point 8 builds the structural identity so the optimizer still
  elides it.
- **Equivalence is checked, not assumed.** §10.3 says the tag's encoding *shall* be equivalent
  to the profile's; a profile in the wild may violate it. The closed form replaces a curve
  only if the two agree within `2⁻⁸` at every point of the 4096-point grid `is_monotonic`
  probes. That bound sits between what an *encoding* of a named curve misses it by (1.5e-5 for
  the PQ built-in's 1024-entry table, 4.2e-6 / 1.3e-6 for the sRGB / BT.709 parametric
  encodings, 1.7e-3 for a 16-entry sRGB table) and what a *different* curve does (gamma 2.2
  or Adobe RGB's `x^(563/256)` labelled sRGB: 8.5e-3 / 8.6e-3). So the option never changes
  how a mislabelled profile renders, and the TRC tags are still read and validated first.
- **Shaper profiles only.** LUT-based profiles' curves are not the profile's transfer
  function; device links read only their A2B tag.
- **Default off, because the speed claim failed and the precision claim is one-directional.**
  Precision: the PQ built-in's reversed table misses ST 2084's inverse by up to 5.5e-2 of
  signal at black (its leading run quantizes to zero, so linear 0 encodes to ≈ 0.055),
  3.6e-2 around 1 cd/m² and 3.7e-4 above 10 cd/m²; the closed form is exact. Device → PCS
  gains only the table's 1.5e-5, and the parametric sRGB/BT.709 curves are within 1e-5 either
  way. Speed, `cargo bench -p gamut-cmm -- cicp_transfer` (256×256 `u8`, fastest of 100
  samples on a loaded machine — read the ratios):

  | scenario | TRC tags | closed form |
  |----------|----------|-------------|
  | sRGB → BT.2100 PQ | 6.35–8.95 ms | 10.96–10.98 ms |
  | BT.2100 PQ → sRGB | 7.33–10.06 ms | 9.89–9.98 ms |

  ST 2084 costs two `powf` per sample where a sampled curve costs one interpolation, so the
  option is never faster. Off, every transform is built exactly as lcms2 builds it.

| class | path | TIGHT max ΔE₀₀ | LOOSE max / mean | asserted |
|-------|------|----------------|------------------|----------|
| sRGB ↔ BT.709 (parametric) | option on | 8.59e-4 | 2.18e-3 / 4.80e-4 | the published shaper budgets, `< 2e-3` / `< 1.0` / `< 3e-2` |
| sRGB/BT.709 ↔ BT.2100 PQ | TRC tags | 2.74e-2 | 1.36e0 / 3.18e-2 | own class, `< 1e-1` / `< 3.0` / `< 1e-1` |
| sRGB/BT.709 ↔ BT.2100 PQ | option on | 3.97e-2 | 1.36e0 / 3.17e-2 | same class |

The PQ pairs are outside the shaper budgets on the **default** path already (14× over TIGHT):
lcms2 evaluates the 1024-entry table through 16-bit interpolation and PQ's steep inverse near
black amplifies it, through a lens that is that same table. With the option on, what is left is
lcms2's error, not this crate's — at black lcms2 encodes 0.0547 where ST 2084 gives 7.3e-7. Both
paths are gated (`tests/oracle_conformance.rs`, `cicp_preferring_transforms_match_lcms2`,
`sampled_pq_transforms_stay_inside_their_class_budget`).

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
| tight / shaper pairs | 8.72e-4 | `< 2e-3` | lcms2 f32 stage noise + near-black γ-inverse amplification (gray→sRGB perceptual); ~2.5× headroom keeps a decade regression visible |
| tight / LUT pairs | 8.77e-3 | `< 2e-2` | the oracle evaluates profile CLUTs through 16-bit fixed point even in double transforms (`EvaluateCLUTfloatIn16`); grid-9 tables bound the agreement |
| loose / shaper pairs | 3.92e-1 (mean 1.00e-2) | max `< 1.0`, mean `< 3e-2` | lcms2's default path resamples the transform into a grid-33 CLUT: deep-shadow toes of γ2.2 and gamut-clip corners interpolate coarsely **on the oracle's side** (at those pixels we match the TIGHT oracle to ~1e-3) |
| loose / LUT pairs | 2.06e0 (mean 1.39e-2) | max `< 5.0`, mean `< 3e-2` | precalculated-CLUT smoothing of clip boundaries (absolute-intent white scaling clips hardest); the mean bound is the regression tripwire — a wrong tag/seam/BPC shifts whole sweeps |

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

**All four rows were re-measured over the full input domain for #453.** Every sweep feeding them
had previously covered only `[0, 0.5)`: the shared generator divided a 31-bit draw by `u32::MAX`,
so `next_unit` capped at 0.4999999999 and `oracle_clut.rs`'s overshoot helper never crossed 1.0.
The pre-#453 figures were 7.82e-4 / 6.87e-3 / 3.34e-1 (mean 1.49e-2) / 1.10e0 (mean 7.23e-3), and
are not comparable with the ones above.

Only the LOOSE class moved materially, and that is the expected direction: **TIGHT bounds this
crate, LOOSE bounds the oracle's approximation.** TIGHT compares against lcms2's full-precision
path and grew 25% (8.77e-3 against an unchanged `< 2e-2`); LOOSE compares against lcms2's default
16-bit precalculated path and doubled. At the worst LOOSE cell — cmyk-v4→sRGB
MediaRelativeColorimetric with BPC — TIGHT is 8.77e-3, a factor of 235 lower, so our output tracks
lcms2's reference answer there and what it diverges from is lcms2's grid. The shaper mean
**tightened**, 5e-2 → 3e-2.

## Deferred / out of scope

| Item | Notes | Status |
|------|-------|--------|
| iccMAX (`ICC.2:2019`) | A separate, parallel next-generation format (spectral PCS, v5 header); not an extension of ICC.1 and unimplementable against the lcms2 oracle. See [`references/icc`](../../references/icc/README.md). | ✗ out of scope |
| `multiProcessElementsType` (`mpet`) + `DToBx`/`BToDx` tags | The v4/iccMAX general-purpose processing pipeline; `gamut-icc` preserves it as `Raw`, and this CMM does not evaluate it. | ✗ out of scope |
| Integer/`f32` CLUT tiers | Landed with #423: `PrecalculateF32`/`PrecalculateFixed16` (above). `f64` stays the Tier-1 default (correctness only, not bit-reproducible — the `gamut-color` posture, see [`references/color`](../../references/color/README.md)). | ✅ |
| 8-bit integer matrix-shaper path | Landed with #992: lcms2's `OptimizeMatrixShaper` under `PrecalculateFixed16`'s 8-bit entry point (above). | ✅ |
| SIMD evaluation | #423 requires a maintainer decision on `unsafe`/portable SIMD before any lands; the tiers are scalar safe Rust (settled decisions above). Handed to the maintainers as #993. | ☐ awaiting decision |
| White-point ("scum dot") fixup on a resampled CLUT | lcms2's `PatchLUT` snaps the input white node onto the output white in a precalculated table (`cmsFLAGS_NOWHITEONWHITEFIXUP` disables it, as the conformance gate does). An lcms2 aesthetic with no ICC.1 basis; the resampling pass does not replicate it. | ✗ out of scope |
| Public per-hop intent/BPC arrays | The chain internals already take them (proofing uses them); the public `chain` API stays single-intent until a consumer needs lcms2's `cmsCreateExtendedTransform` shape. | ☐ deferred |
| Named-colour profiles | No continuous pixel transform; rejected with a typed error everywhere. | ✗ out of scope |
| Public `prefer_cicp` on `link::device_to_pcs`/`pcs_to_device` | The per-profile builders stay TRC-only (and so does black-point *detection*, which runs through them); the option lives on the transform constructors. | ☐ deferred |
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
error, empty buffers, and a doctest). P8 adds `tests/optimize.rs` (the knob is off by
default; optimization off reproduces `between` **bit-exactly** against the same link
composed independently out of `device_to_pcs`/`pcs_to_device`; each pass observed through
the public `Pipeline::optimized` API — folding to a single matrix, joining to a single curve
set, resampling a shaper pair into one grid-33 tetrahedral CLUT; every constructor accepting
the knob; and the shape invariance the knob must preserve) plus the optimization unit layer
in `src/optimize.rs` (per-pass rewrites with hand-computed folded coefficients, the
rectangular `MatrixN` fold across a shape change, the fixpoint that only an elision uncovers,
exact-only identity elision against a one-ulp near-identity, the exact identity-curve join,
node-exact resampling, lcms2's grid-point rule, and each of the four reasons resampling
declines — PCS entry, PCS exit, a single-stage chain, and the node ceiling, with the
one-channel-narrower grid that still fits proving the ceiling is a boundary and not a
blanket refusal), the conformance gate's optimized-budget battery
(`optimized_transforms_stay_inside_the_precision_budget` — the table above), and
`benches/pipeline.rs` (divan throughput, off vs the
two levels vs lcms2 at default and `NOOPTIMIZE` flags). #423 adds the precision-tier layer:
`src/clut.rs` pins the fixed-point helpers (`_cmsQuickSaturateWord`, `_cmsToFixedDomain`,
`LinearInterp` against a 32-bit wrap-around transcription) and each fixed-point interpolator
against independent transcriptions of lcms2's 1-D, 2-D, tetrahedral (its own branch cascade,
ties included), 4-D and 5-D routines, the `f32` tier's width, and the fixed-point
construction rules; `src/transform.rs` the provided integer entry points over a transform that
overrides nothing (chunking across unequal in/out widths, the buffer contract) and
`FROM_8_TO_16`/`FROM_16_TO_8` exhaustively; `src/pipeline.rs` the fixed-point pipeline's
integer path and the three entry points' agreement on every tier; `src/optimize.rs` that each
level resamples onto lcms2's grid at its own precision. `tests/oracle_clut.rs` holds both tiers
**bit-exact** against lcms2 (16-bit `cmsDoTransform` over a probe devicelink; the float
`ClutPipeline`), `tests/oracle_conformance.rs` the budget table above, and the benches a bar
per tier. Gates: `mise run test` / `lint` /
`fmt-check` / `coverage` (≥ 80%) / `mise run mutants-crate gamut-cmm` (equivalent
float-boundary survivors are excluded in `.cargo/mutants.toml` with per-mutant proofs, the
workspace convention).
