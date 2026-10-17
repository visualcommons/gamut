# gamut-dsp — shared DSP kernels status

**v1 stabilization: GitHub issue #192.** `gamut-dsp` is the workspace's shared signal-processing
kernel library: spec-exact transform kernels, quantization rounding, and companding, implemented
once and consumed by the codec crates (`gamut-av1` for the AV1 transform family, `gamut-jpeg` for the
JPEG-1 8×8 DCT, `gamut-webp` for the shared quantize rounding, with `gamut-av2`/`gamut-vvc`
declaring the edge for future kernels). It is a leaf crate — **zero dependencies** — and
`#![deny(unsafe_code)]` (a hot-path crate: `deny`, not `forbid`, so a benchmarked win may take a
site exception; none does today).

**Keystone:** the split between *normative* and *chosen* math, under a namespace rule that keeps
every future standard additive. The `inverse_*` kernels are bit-exact transcriptions of the AV1
decoder processes — their behaviour is the spec's, frozen forever. The `forward_*` kernels are
encoder choices whose only contract is consistency with their paired inverse, so their internals
(basis tables, SIMD) may be rewritten freely under frozen signatures. And because the surface is
one module per spec family with nothing at the crate root — `av1`, `jpeg` and `math` today — a
future `jxl`/`av2` module can never collide with an existing name.

## Public surface (frozen at v1)

| Item | Shape | Openness |
| ---- | ----- | -------- |
| `av1::inverse_dct` / `av1::forward_dct` | in-place 1-D kernels over `&mut [i64]`, `n ∈ 2..=6` (lengths 4–64); the inverse takes the §7.13.3 clamp range `r` | frozen; new kernels are additive |
| `av1::inverse_adst` / `av1::forward_adst` | same shape, `n ∈ 2..=4`; DST-VII at size 4, DST-IV at 8/16 (a spec quirk, documented) | frozen |
| `av1::inverse_identity` / `av1::forward_identity` | same shape, `n ∈ 2..=5`; pure per-element √2-step scaling | frozen |
| `av1::forward_wht4x4` / `av1::inverse_wht4x4` | complete 2-D 4×4 lossless block pair over by-value `[i32; 16]`; exact algebraic round-trip | frozen |
| `jpeg::fdct8x8` / `jpeg::idct8x8` | in-place 2-D 8×8 kernels over a raster-order `&mut [i32; 64]`; separable `f64` evaluation of the T.81 §A.3.3 ideal equations, rounded once | frozen, signature *and* output (see the decision below); fast fixed-point kernels are additive new functions |
| `jpeg::idct8x8_int` / `jpeg::idct8x8_int_dc` | the fast inverse beside the pair (issue #896): in-place 13-bit fixed-point LL&M over the same `&mut [i32; 64]`, bit-exact with libjpeg-turbo's `jpeg_idct_islow`, column/row zero shortcuts; `_dc` is the DC-only sparsity hint (`i32 → i32`, the value of every sample) | additive; output frozen as libjpeg's `jidctint.c` arithmetic |
| `math::round2` `math::round2_signed` `math::clip3` `math::round_div_nearest` | scalar integer ops: AV1 §4.7 rounding/clamp plus the AV1+VP8 forward-quantize rounding | frozen; new fns are additive |

Adding modules or functions stays backward-compatible; removing or reshaping any of the above
would not.

## Settled design decisions (intentional, not gaps)

- **One module per spec family; nothing at the crate root.** Every item has exactly one canonical
  path (no root re-exports). The `av1` *module* is the §7.13.2 1-D kernel library; the `gamut-av1`
  *crate* is the codec that drives it — the 2-D row/column assembly, per-pass shifts, FLIPADST
  flips (§7.13.3), and quantizer tables all live there.
- **Total math with a panic contract, no `Result`s.** No input to any function here is
  data-dependent fallible; semantic preconditions on configuration parameters (`n`, `r`, `bits`,
  `mu`, `den`) are release `assert!`s documented under `# Panics` — the same category as
  gamut-core's documented indexing panics, not error handling. Arithmetic-headroom limits are
  documented per function and guarded by Rust's debug overflow checks; `round2`, the hottest
  primitive (twice per butterfly), has no semantic precondition and deliberately gains no branch.
- **`n: u32` size exponents, not a size enum.** The three kernel families accept different ranges
  (`2..=6` / `2..=4` / `2..=5`), so a single enum cannot make misuse unrepresentable, and
  per-family enums would push fallible conversions into `gamut-av1`'s log2-arithmetic call sites
  (the `TX_*_LOG2` tables). The spec itself parameterizes these processes by log2 size.
- **`[i32; 16]` WHT vs `&mut [i64]` 1-D kernels.** The WHT is a complete lossless block transform
  whose exact-round-trip domain provably fits `i32`, and by-value arrays match how its consumers
  hold blocks; the 1-D kernels are in-place `i64` because the 2-D passes need intermediate
  headroom under the `r` clamp. Unifying would worsen both call sites for cosmetic symmetry.
- **Forward transforms are encoder choices.** AV1 specifies only the inverses; each forward
  promises consistency with its paired inverse (absolute scale is reconciled by the 2-D shifts),
  so forward internals are freely replaceable — `forward_adst` currently derives its basis from
  the inverse's impulse responses at call time, and swapping that for const tables is a
  non-breaking follow-up.
- **Shared-vs-format-local boundary.** Kernels with a single consumer stay in their format crate
  by design: VP8 transforms in `gamut-webp` (RFC 6386 kernels differ from AV1's), PNG filters in
  `gamut-png`, the TIFF differencing predictor in `gamut-tiff`. `gamut-dsp` hosts math shared by
  ≥ 2 crates or normative families with multiple planned consumers; nobody should "helpfully"
  migrate the format-local kernels here.
- **`math` ops carry AV1 §4.7 names but serve every codec.** `Round2`/`Round2Signed`/`Clip3` are
  the operations other codec specs define equivalents of, and `round_div_nearest` is already
  shared by the AV1 and VP8 forward quantizers — that is why they live outside the `av1` module.
- **The `jpeg` pair is frozen at v1, output included.** Issue #192 froze the `av1`/`math`
  surface before the `jpeg` module landed; this entry extends the freeze to it (issue #545).
  `fdct8x8`/`idct8x8` are documented as the §A.3.3 ideal values rounded once (ties away from
  zero), and `gamut-jpeg`'s encoded coefficients and decoded samples follow from that exact
  output. A fixed-point fast kernel cannot reproduce it bit for bit, so it may not replace either
  body: fast kernels arrive as *new* functions beside the pair, which stays the reference. Only
  internals that keep every output identical (e.g. a const cosine table) may change under the
  frozen signatures.

## Deferred / tracked follow-ups (all additive — none blocks v1)

- **JPEG-in-TIFF consumer** — the `jpeg` module ships (see the surface table) and `gamut-jpeg`
  consumes it; what is deferred is `gamut-tiff`'s JPEG-in-TIFF compression, a consumer outside
  this crate. `gamut-tiff` does not declare a dsp edge today; it adds one, or reaches the kernels
  through `gamut-jpeg`, when that compression lands.
- **Fast fixed-point `jpeg` forward DCT** — the inverse landed as `idct8x8_int` (issue #896)
  beside the reference pair, which keeps its exact output (see the settled decision above); a
  fast forward kernel for the encoder is not planned yet, and would land the same way.
- **`jxl` / `av2` modules** — kernels for the JPEG XL and AV2 codecs as those stub crates grow
  real surfaces (`av2` may re-export `av1` kernels where AVM inherits them).
- **`forward_adst` const basis tables** — replace the per-call impulse-response derivation with
  const tables plus a test regenerating them from the inverse (locking the coupling
  mutation-testably); internal, justified by the divan bench when it lands.
- **SIMD variants** behind the frozen signatures, where the benches show a win.

## Validation

Backed by inline unit tests per kernel — independent naive float oracles (proportional, so the
transcription is pinned against DCT-III / DST-VII / DST-IV directly), exact golden snapshots in
*both* directions (a uniform scale or rounding regression cannot hide in the proportional
oracle), adversarial clamp-saturation probes proving the `r` range is wired through the
butterflies, ±2¹⁹/±4095 headroom probes under debug overflow checks; the `jpeg` pair is pinned
inline against a direct evaluation of the §A.3.3 double sum, plus DC, orientation and round-trip
probes; `idct8x8_int` is held to the full IEEE 1180-1990 accuracy procedure against `idct8x8`
(three input ranges × 10 000 blocks × both signs, all five bounds; T.83 is not vendored), plus
impulse, dense-block and DC-shortcut equality probes, and its bit-exactness with libjpeg-turbo
lands through `gamut-jpeg`'s oracle gate, which asserts exact equality on gray — plus `tests/surface.rs`, which drives the 12 `av1`/`math` public functions through their
`gamut_dsp::module::item` paths only (the two `jpeg` functions are not yet in it), and the
compiled crate-root doctest.
Bit-exactness of the inverse kernels against dav1d/libaom lands transitively through the 2-D
pipeline's conformance suites in `gamut-av1`/`gamut-avif`. The divan bench harness
(`benches/transforms.rs`, issue #149) tracks DCT/ADST/WHT throughput — this crate is a real
computational hot path, unlike core. The full `cargo mutants -p gamut-dsp` survey passes with
zero missed and one documented equivalence exclusion (`adst_output_permute` `|`→`^`,
`.cargo/mutants.toml`). Gates: `mise run test` / `lint` (`clippy -D warnings`, `missing_docs`
fatal) / `fmt-check` / `coverage` (≥ 80%).
