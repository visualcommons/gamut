# gamut-tonemap — tone-mapping primitives status

**v1 stabilization: GitHub issue #188.** Tone-mapping math primitives: the `ToneCurve` trait plus
eight built-in operators, implemented clean-slate from the primary sources transcribed in
[`references/tonemap`](../../references/tonemap/README.md), and the RGB-aware `ToneMapper` trait
with `CurveToneMapper`, which applies any curve to a whole pixel (#1236). Operators take
non-negative **linear-light** input and are pure `f32 -> f32` maps; the absolute reference luminances they
normalize against live in `gamut_core::luminance`. Runtime dependency: `gamut-core` only;
`#![forbid(unsafe_code)]`.

**Keystone:** the **construction-time validity boundary**. Parameterized operators validate their
parameters — including the derived divisors/normalizers the hot loop actually uses (`white²`,
`partial(white)`, Drago's prefactor) — exactly once, at construction, so `map` stays branch-lean
and the numeric contract (never NaN, non-negative, monotone up to f32 rounding) holds for every
constructible operator across the full f32 input range.

## Public surface (frozen at v1)

| Item | Shape | Openness |
| ---- | ----- | -------- |
| `ToneCurve` | `map(&self, f32) -> f32` + provided `map_slice(&mut [f32])`; dyn-compatible | open trait; blanket impl covers every `Fn(f32) -> f32` |
| `Linear`, `Reinhard`, `Aces` | parameterless operators (zero-sized) | literal-constructible by design — never `#[non_exhaustive]` |
| `Clamp`, `Exposure`, `ReinhardExtended`, `Hable`, `Drago` | validated constructors returning `gamut_core::Result`; by-value accessors | private fields — adding fields is non-breaking |
| `SDR_WHITE_NORMALIZED`, `DEFAULT_REINHARD_WHITE`, `DEFAULT_HABLE_WHITE`, `DEFAULT_DRAGO_BIAS` | `f32` constants | documented literals backing the `Default` impls |
| `ToneMapper` (#1236) | `map_rgb(&self, [f32; 3]) -> [f32; 3]` + provided `map_rgb_slice(&mut [[f32; 3]])`; dyn-compatible | open trait; no blanket impl, so forwarding impls stay possible |
| `CurveToneMapper<C: ToneCurve>` (#1236) | `new(curve, source_peak)` + `with_target_peak` / `with_desat` / `with_luma_weights` (validated, `gamut_core::Result`) and `with_mode`; accessors (`curve()` by reference, `mode` / `desat` / `source_peak` / `target_peak` / `luma_weights` by value) | private fields; `Debug + Clone + Copy + PartialEq` where `C` is |
| `ApplyMode` (#1236) | `PerChannel = 0`, `Luminance = 1`, `MaxRgb = 2` (default) | `#[non_exhaustive]`, `#[repr(u32)]`; discriminants permanent (C ABI, #1103), new variants append |
| `DEFAULT_DESAT` (#1236) | `f32` constant `2.0` | ffmpeg `tonemap`'s `desat` default |

Every item is public only at the crate root (the modules are private), so each has exactly one
path. All operators are `Debug + Clone + Copy + PartialEq`. Adding
operators, trait impls, or provided trait methods stays backward-compatible; removing or reshaping
any of the above would not.

## Operators

| Operator | Source | Status |
|----------|--------|--------|
| `ToneCurve` trait + blanket `Fn(f32) -> f32` impl | — | ✅ |
| `Linear` — identity passthrough | — | ✅ |
| `Clamp` — hard clamp to `[0, max]` | — | ✅ |
| `Exposure` — linear pre-scale (gain or photographic stops) | photographic convention | ✅ |
| `Reinhard` — `L / (1 + L)` | Reinhard et al. 2002, Eq. 3 | ✅ |
| `ReinhardExtended` — white-point variant | Reinhard et al. 2002, Eq. 4 | ✅ |
| `Aces` — filmic approximation | Narkowicz 2016 (fit to ACES RRT+ODT) | ✅ |
| `Hable` — Uncharted 2 filmic | Hable 2010 | ✅ |
| `Drago` — adaptive logarithmic | Drago et al. 2003, Eq. 4 | ✅ |
| `ToneMapper` trait + `CurveToneMapper` — per-channel / luminance / max-RGB application, peaks, desaturation | ffmpeg `vf_tonemap.c` `tonemap()` (max-RGB + desat); libplacebo (luminance) | ✅ |

## Settled design decisions (intentional, not gaps)

- **The blanket `Fn(f32) -> f32` impl trades away all forwarding impls.** Because `&F`, `&mut F`,
  `Box<F>`, and `Rc<F>` are themselves `Fn` whenever `F` is, coherence forecloses
  `impl ToneCurve for &T` (and `Box<T>`, `Rc<T>`, …) forever — in particular `Box<dyn ToneCurve>`
  will never implement `ToneCurve` itself. The blanket impl is its own escape hatch
  (`|x| boxed.map(x)`). Chosen deliberately: closures-as-curves is the ergonomic core of the API,
  every built-in is `Copy`, and `&dyn ToneCurve` works for dynamic dispatch.
- **f32 scalar math, Tier-1 correctness-only** (determinism note in
  `references/tonemap/README.md`). `gamut-color` keeps its own f64 encoder-exact
  `bt2020_pq_to_sdr`; the two implementations are proven to agree by a dev-dependency cross-check
  test rather than shared code, so tonemap never becomes a runtime dependency of the colour
  pipeline.
- **One precision: `f32`, with no f64 twin of `ToneCurve` (#1102, settled).** The documented
  HDR-to-SDR pipeline runs through `gamut-color`'s `f64` transfer functions, and the caller narrows
  to `f32` at the `ToneCurve` boundary — documented on the trait and in the crate docs. Narrowing
  costs at most half an f32 ULP, below any display or 16-bit code step, while an `f64` signature
  would double the bandwidth of every `map_slice` and break the frozen v1 trait (and every closure
  curve) for precision no consumer can observe; the `ToneMapper` follow-up (#1236) keeps `f32`
  too. What the decision answers to is an oracle, not a round-trip: each built-in `map` is
  compared with an independent `f64` evaluation of its published formula, written without the f32
  overflow guards, within `8 · f32::EPSILON` relative error plus an absolute floor of four
  subnormal steps (`4 · 2⁻¹⁴⁹`), raised to `4 · f32::EPSILON` for Hable and Drago alone (their
  near-zero outputs have no relative digits left in f32: Hable's `partial` cancels and Drago's
  `ln(x + 1)` rounds `x + 1` to 1) — from 0 through subnormals, every decade, and a dense
  `[0, 16]` grid to `f32::MAX`, with `+∞` required exactly where the `f64` value exceeds f32. The
  bound is claimed at representative parameters: degenerate ones the constructors still admit
  (a Hable white of `1e-30`, a `ReinhardExtended` white below about `1e-8`, where `x²` underflows
  ahead of the `/white²` that would restore it) keep only the never-NaN contract. A caller who needs `f64` end to end evaluates the published formula directly, as
  `gamut-color` does.
- **Numeric contract over literal transcription at f32 extremes.** Every operator is NaN-free
  across the entire f32 input range: `Aces` evaluates at `min(x, 8)` inside its exact saturation
  region, `Hable` clamps its argument where the curve sits within one ULP of its asymptote (both
  branchless), `ReinhardExtended` evaluates Eq (4) as `t + (x·t)/white²` with `t = x/(1 + x)`,
  which is the association that saturates only when the exact value genuinely exceeds f32 rather
  than when an intermediate does (#471). Each deviation is derived and justified in
  `references/tonemap/README.md`; all published golden values are unchanged.
- **`Aces` names the Narkowicz fit.** The official colour-coupled ACES RRT+ODT — and the Hill
  fit — need 3×3 matrices and could never be a scalar `ToneCurve`, so there is no ambiguity to
  reserve the name against.
- **`Default` impls are the API; the root `DEFAULT_*`/`SDR_WHITE_NORMALIZED` constants hold the
  documented literals** (mirroring the `gamut_core::luminance` precedent). No `Default` for
  `Exposure` or `Drago` — there is no canonical exposure, and a scene maximum cannot be guessed.
- **An RGB-aware trait beside the frozen scalar one (#1236, settled).** `ToneCurve` keeps its
  scalar definition. Pixel-level operation is a second trait, `ToneMapper`, because desaturation,
  luminance- or max-RGB-driven application and peak-relative curves all need the pixel. The
  configured operator is a struct, `CurveToneMapper`, over any `ToneCurve`. Operators that are not
  a scalar curve in a mode (BT.2390's EETF, BT.2446 Method A) implement the trait directly.
  - **Peaks.** They enter as one rule, `T(x) = target_peak · f(x) / f(source_peak)`. ffmpeg's
    `hable`, `reinhard`, `linear` and `clip` curves all have this form, so the source peak maps
    exactly onto the target peak for any curve. The constructor rejects a curve whose
    normalization is not a normal, positive `f32`.
  - **Defaults.** They are ffmpeg's: `MaxRgb`, `desat = 2`, target peak `1`. Desaturation runs
    before the mode in every mode, and its `1e-6` floors are kept as written.
  - **Luma weights.** They are `gamut_core`'s fixed-point ones: BT.709 by default, any triple
    summing to `LUMA_ONE` via `with_luma_weights`. This adds no dependency edge. The BT.709
    defaults are each within `2⁻¹⁷` of ffmpeg's decimals, which is the one intended difference;
    `gamut_core`'s other tables are within one unit, `2⁻¹⁶` (BT.2020's green carries the spare
    unit that makes the sum exact, `0.59` units off).
  - **Precision.** It stays `f32`. Luma and the gain are capped at `f32::MAX`, so a saturating
    curve cannot produce `∞/∞` or `0·∞`.
- **Composition stays a closure.** `|x| b.map(a.map(x))` composes curves today; a `then`
  combinator would be additive if ergonomics ever demand it.

## Deferred (all additive — none blocks v1)

| Item | Notes | Status |
|------|-------|--------|
| Turnkey HDR→SDR helper | Pair a curve with `gamut-color` transfer functions (linearize → map → re-encode) behind an optional feature; needs a `gamut-color` runtime dependency. | ☐ |
| Curve-composition combinators | e.g. `curve.then(other)` as a provided trait method; today compose with a closure: `\|x\| b.map(a.map(x))`. | ☐ |
| Other filmic fits | The Narkowicz ACES fit ships; the Stephen Hill fit needs RRT/ODT matrices (colour-coupled), so it would be a new, non-scalar API. | ☐ |
| BT.2390 EETF | The reference HDR-to-display mapping (libplacebo/mpv default), in the PQ domain. A `ToneMapper`, or a `ToneCurve` on PQ-normalized luminance. #1254. | ☐ |
| BT.2446 Method A | HDR-to-SDR in a perceptually linearized luma domain with chroma correction, so a direct `ToneMapper`. #1255. | ☐ |

## Out of scope

`gamut-tonemap` is deliberately a tone-mapping *math* library — scalar curves and their application to a linear RGB pixel. The following are **not** provided here
— they belong to the surrounding pipeline (`gamut-color`, the codec crates):

- Colour-space conversion, gamut mapping, white-point adaptation.
- Transfer-function linearization / re-encoding (EOTF/OETF) — see `gamut-color`.
- Pixel I/O and alpha handling: apply a curve or a `ToneMapper` to the RGB channels while
  preserving alpha in the caller.
- The full ACES RRT+ODT transform (colour-space-coupled).

## Validation

Inline unit tests: per-operator fixed points plus independent golden values transcribed from the
primary sources, monotonicity grids, constructor-rejection cases (including parameters whose
*derived* math degenerates), one adversarial f32-extremes sweep (never-NaN / non-negative at
`{0, 1e20, f32::MAX}` for all eight operators), the Reinhard ↔ `gamut-color` cross-check, and
one `*_tracks_f64_oracle` test per operator holding its `f32` `map` to an independent `f64`
evaluation of the published formula across the whole `f32` input range (bound above, under the
f32 decision). `CurveToneMapper` has inline tests for every mode, builder and rejection. Its
max-RGB mode with desaturation is held by `tests/ffmpeg_desat_golden.rs` to an `f64`
transcription of ffmpeg's `tonemap()`. That sweep covers the hable, reinhard, linear and clip
curves, four peaks, four desat strengths and 11³ pixels, within 32 f32 ULPs of the output scale.
Five literal goldens were computed independently from the same formula.

There is no external oracle for this crate's *shape* — the `f64` formula pins values, not the
laws a custom curve must keep — so the `ToneCurve` contract itself is the authority and
`src/invariants.rs` states it as executable laws — non-negative and never NaN, monotonic
non-decreasing, and `map_slice` as elementwise order-independent `map`. For `ToneMapper` the laws
are never-NaN/non-negative channels, `map_rgb_slice` as pixelwise `map_rgb`, one common gain (hue
kept) in the luminance and max-RGB modes, and a grey source peak mapping to the target peak. Those laws are driven by
pinned-seed `proptest` properties here and are the attachment point for the out-of-tree fuzz tier
(`test-support`); see `docs/testing.md`. The per-operator tests above pin *where* each curve goes,
the laws pin *what shape* it has everywhere.

**Drago's monotonicity is conditional (#439, resolved).** `Drago::map` transcribes Eq (4)
faithfully, and Eq (4) is not monotonic non-decreasing on `[0, world_max]` for every parameter
pair the constructors accept: the denominator `ln(2 + 8·(x/world_max)^k)` outgrows the numerator
`ln(x + 1)` over a band of `x`, so monotonicity needs `bias` to rise with `world_max`. The
transcription was checked first and cleared — the paper prints `log(0.5)` as the bias base in both
Eq (3) and Eq (4), and base `0.8` is strictly worse — so what was wrong was the blanket promise in
the docs. `Drago::is_monotonic` now states the condition as a predicate
(`world_max·10·ln 10 ≥ 8k(world_max+1)ln(world_max+1)`, `k = ln bias / ln 0.5`), and the
`invariants` properties drive it in both directions over the full accepted parameter range: that
the curve really is monotonic wherever the predicate says so, and that a decrease really is there
wherever it says not. At the default bias the ceiling is `world_max ≈ 2.1e5`, against the paper's
own worked example of 230 cd/m². Derivation and the measured table: `references/tonemap/README.md`.
Divan benches cover `map_slice` throughput for every operator and `map_rgb_slice` for
`CurveToneMapper` in each mode (`cargo bench -p gamut-tonemap`).
Gates: `mise run test` / `lint` (`clippy -D warnings`, `missing_docs` fatal) / `fmt-check` /
`coverage` (≥ 80%) / `mise run mutants-crate gamut-tonemap`.
