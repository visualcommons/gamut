# Tone-mapping operators — source of truth (issue #188)

This directory records the authoritative definitions for every tone-mapping operator implemented by
`gamut-tonemap`, together with their primary-source citations. Each coefficient and formula baked
into the code is transcribed here so the literals can be audited against the source rather than
trusted by copy-paste. The built-in operators are implemented **clean-slate** from these published
definitions, not wrapped from a C library.

The reference white-luminance levels these operators normalize against (HDR 203 cd/m², SDR
100 cd/m², PQ peak 10 000 cd/m²) are defined once in `gamut_core::luminance` and documented under
`references/color/README.md` (ITU-R BT.2408, SMPTE ST 2084 / ITU-R BT.2100).

> Determinism note: `gamut-tonemap` evaluates these in **`f32`** scalar math (Tier-1, correctness
> only) using `std` `powf`/`ln`/`log10`. Golden values below are computed in higher precision; the
> crate's tests compare to them within a small `f32` tolerance.

---

## Vendored primary sources

Freely-published / openly-licensed primary sources are vendored alongside this README so the
literals can be audited offline.

| file | source |
|------|--------|
| `reinhard-2002-photographic-tone-reproduction.pdf` | Reinhard, Stark, Shirley, Ferwerda, "Photographic Tone Reproduction for Digital Images", SIGGRAPH 2002 — Univ. of Utah tech report UUCS-02-001 (freely published); DOI <https://doi.org/10.1145/566654.566575> |
| `drago-2003-adaptive-logarithmic-mapping.pdf` | Drago, Myszkowski, Annen, Chiba, "Adaptive Logarithmic Mapping For Displaying High Contrast Scenes", Eurographics 2003 / CGF 22(3) — © Eurographics; author copy via MPI Informatik (`resources.mpi-inf.mpg.de/tmo/logmap/`) |
| `aces-filmic-narkowicz.html` | Krzysztof Narkowicz, "ACES Filmic Tone Mapping Curve", 2016-01-06 — <https://knarkowicz.wordpress.com/2016/01/06/aces-filmic-tone-mapping-curve/> (author releases the code as **CC0 / MIT**) |
| `uncharted2-hable-filmicworlds.html` | John Hable, "Filmic Tonemapping Operators", filmicworlds.com, 2010-05-05 (GDC 2010 "Uncharted 2: HDR Lighting") — <http://filmicworlds.com/blog/filmic-tonemapping-operators/> |

Cross-referenced (not duplicated here): `references/color/itu-r-bt2408-8.pdf` (HDR Reference White
203 cd/m²) and the SMPTE ST 2084 PQ constants transcribed in `references/color/README.md`.

The official **ACES** system (the curve below only *approximates*) is the Academy Color Encoding
System, <https://github.com/ampas/aces-dev> — not vendored; the Narkowicz fit is the immediate
provenance of the coefficients.

---

## Reinhard — global photographic operators (2002)

Source: Reinhard et al. 2002, §3.1 "Initial luminance mapping" (verified 2026-06). Code:
`gamut_tonemap::{Reinhard, ReinhardExtended}`.

**Simple operator, Eq (3):**

```
L_d = L / (1 + L)
```

Maps `[0, ∞) → [0, 1)`. High luminances are scaled by ≈`1/L`, low luminances by ≈`1`; the
denominator is a graceful blend, guaranteeing all luminances land in a displayable range.

**Extended operator (allows high luminances to burn out), Eq (4):**

```
L_d = L · (1 + L / L_white²) / (1 + L)
```

`L_white` is "the smallest luminance that will be mapped to pure white" (`map(L_white) = 1`). Setting
`L_white` to the scene maximum `L_max` or higher avoids burn-out; as `L_white → ∞` the operator
reverts to Eq (3).

`gamut-tonemap` default `DEFAULT_REINHARD_WHITE = 2.03` = BT.2408 HDR Reference White (203 cd/m²) /
SDR diffuse white (100 cd/m²). Golden: `Reinhard.map(1)=0.5`, `map(3)=0.75`;
`ReinhardExtended{4}.map(4)=1.0`, `map(2)=0.75`.

**Evaluation note — factored form.** With `t = L / (1 + L)`, Eq (4) is evaluated as
`t + (L·t) / L_white²`, algebraically identical to the printed form. Two overflows are avoided,
and they are different:

- The direct numerator `L · (1 + L / L_white²)` overflows for huge `L` even when the exact result
  fits (e.g. `L = f32::MAX`, `L_white = 2.03`: exact `≈ 8×10³⁷`). `t` is bounded by 1, which fixes
  that.
- The quotient `L / L_white²` overflows *on its own* for `L_white` near the bottom of the
  construction domain below, even where the whole expression fits comfortably. Dividing
  `L·t = L²/(1 + L) < L` instead of `L` fixes that: it is the smaller numerator over the same
  divisor. The largest admitted `L_white` that used to overflow was `1.7141993×10⁻¹⁹`, at
  `L = 10`, where the exact value is `≈ 3.09×10³⁸` — inside f32 with room to spare (#471).

What remains saturates to `+∞` only when the exact value genuinely exceeds f32 range, which the
`ToneCurve` contract permits. Every golden above is bit-identical under either form; the current
order is also the one that lands `map(L_white)` on exactly `1.0` at extreme white points, where
the earlier order returned `0.99999994`.

**Construction domain.** `L_white²` (the divisor) must be a normal f32, i.e. `L_white` roughly
within `[1.1×10⁻¹⁹, 1.8×10¹⁹]`: a square that underflows to zero or subnormal makes the division
blow up at moderate `L`, and one that overflows to `∞` silently breaks the `map(L_white) = 1`
fixed point.

> Note: Reinhard's key-value pre-scale (Eq 1–2, `L = (a/L̄_w)·L_w`, an exposure/auto-exposure step)
> is **not** part of these operators — `gamut-tonemap` models it with the composable `Exposure`
> operator (below) so each curve stays a pure `f32 → f32` map.

---

## Exposure — pre-scaling

Source: standard photographic exposure convention (powers of two / "stops"; APEX system). Code:
`gamut_tonemap::Exposure`.

```
map(x) = x · gain          gain = 2^stops      (one stop doubles exposure)
```

A composable linear gain applied before a curve. Models Reinhard's key scaling, the ACES "×0.6"
input pre-exposure, and Hable's "×2.0" exposure bias as an explicit operator rather than a hidden
constant. Golden: `Exposure::new(2).map(3)=6`; `Exposure::from_stops(1).scale()=2`.

---

## ACES filmic — Narkowicz approximation (2016)

Source: Narkowicz 2016 (`aces-filmic-narkowicz.html`, verified 2026-06), license CC0/MIT. Code:
`gamut_tonemap::Aces`.

```
ACESFilm(x) = saturate( (x·(a·x + b)) / (x·(c·x + d) + e) )
a = 2.51   b = 0.03   c = 2.43   d = 0.59   e = 0.14
```

`saturate` clamps to `[0, 1]`. This is a **luminance-only rational fit** to `ODT(RRT(x))` sampled in
REC.709 / D65 (display gamma 2.4 removed); it is **not** the official ACES RRT+ODT transform (which
is colour-space-coupled — AP0/AP1 matrices, glow/red modifiers — and out of scope for a scalar
curve). The input is assumed pre-exposed (so `1 → ≈0.8`); apply `Exposure::new(0.6)` first to match
the original ACES curve. Golden: `map(0)=0`, `map(0.5)≈0.616307`, `map(1)≈0.803797`, `map(1e6)→1`
(saturates; the unclamped limit is `a/c ≈ 1.033`).

**Evaluation note — saturation clamp.** The unclamped rational is `≥ 1` exactly when
`numerator − denominator = (a−c)·x² + (b−d)·x − e = 0.08x² − 0.56x − 0.14 ≥ 0`, i.e. for
`x ≥ (0.56 + √(0.56² + 4·0.08·0.14)) / (2·0.08) ≈ 7.2417`. Under `saturate`, every such input maps
to exactly `1.0`, so the implementation evaluates the rational at `min(x, 8)` — branchless, keeping
the per-sample loop vectorizable — with ample margin: the unclamped value at `x = 8` is `≈ 1.0031`,
thousands of f32 ULPs above 1, so the saturate still lands on exactly `1.0`. Without the argument
clamp both quadratics overflow f32 for `x ≳ 2×10¹⁹`, yielding `inf/inf = NaN`.

---

## Hable / Uncharted 2 filmic (2010)

Source: Hable 2010 (`uncharted2-hable-filmicworlds.html`, verified 2026-06). Code:
`gamut_tonemap::Hable`.

```
partial(x) = ((x·(A·x + C·B) + D·E) / (x·(A·x + B) + D·F)) − E/F
A = 0.15 (shoulder strength)   B = 0.50 (linear strength)   C = 0.10 (linear angle)
D = 0.20 (toe strength)        E = 0.02 (toe numerator)     F = 0.30 (toe denominator)
W = 11.2 (linear white point)

map(x) = partial(x) / partial(W)
```

White-point normalized so `map(W) = 1` and `map(0) = 0`. The original presentation additionally
applied an exposure bias of 2.0 (`curr = partial(2·x)`) — modelled here by composing
`Exposure::new(2.0)` — and a final `pow(·, 1/2.2)` display re-encode, which belongs to the target
transfer function (`gamut-color`), not the tone curve. Default `W = DEFAULT_HABLE_WHITE = 11.2`.
Golden (`W = 11.2`): `map(0)=0`, `map(1)≈0.304300`, `map(4)≈0.713240`, `map(11.2)=1`.

**Evaluation note — argument clamp.** `partial` clamps its argument to `10¹⁸` before evaluating.
Expanding the rational, `partial(x) = (1 − E/F) − ((B − C·B)/A)/x + O(1/x²) ≈ 0.93̅ − 3/x`, so at
`x = 10¹⁸` the curve sits `3×10⁻¹⁸` below its horizontal asymptote — some ten orders of magnitude
inside one f32 ULP (`≈ 6×10⁻⁸` at `0.93`); `partial(10¹⁸)` and `partial(9×10¹⁸)` are bit-identical
in f32. Without the clamp the quadratics overflow to `inf/inf = NaN` for `x ≳ 1.5×10¹⁹`. Clamping
inside `partial` also covers the `partial(W)` normalizer for absurdly large white points.

> Hable later (2017, "Filmic Tonemapping with Piecewise Power Curves") proposed a different,
> piecewise operator; the curve above is the original 2010 Uncharted 2 operator.

---

## Drago — adaptive logarithmic mapping (2003)

Source: Drago et al. 2003, §3 (verified 2026-06). Code: `gamut_tonemap::Drago`.

Bias power function, Eq (3) (Perlin & Hoffert):

```
bias_b(t) = t^(log(b) / log(0.5))
```

Main tone-mapping function, Eq (4):

```
L_d = (L_dmax · 0.01) / log10(L_wmax + 1)  ·  log(L_w + 1) / log( 2 + ((L_w / L_wmax)^(log(b)/log(0.5))) · 8 )
```

- `L_w` — world (scene) luminance; `L_wmax` — maximum scene luminance (`world_max`).
- `L_dmax` — maximum display luminance; the paper uses **100 cd/m²** (CRT reference), so
  `L_dmax · 0.01 = 1` and `L_d` is display-relative in `[0, 1]`.
- `b` — bias, useful range **[0.5, 1.0]**, default **0.85** (`DEFAULT_DRAGO_BIAS`). For `b < 0.7` the
  output can exceed 1.0 (the display clamps to `L_dmax`); `map` is a faithful, clamp-free
  transcription of Eq (4).
- The numerator/denominator log base cancels (Eq 2, change of base), so `ln` is used; only the
  `log10(L_wmax + 1)` normalization is base-specific.

Fixed points: `map(0) = 0` and `map(L_wmax) = 1`. Golden (`L_wmax = 100`, `b = 0.85`):
`map(10) ≈ 0.630858`. Golden through `with_bias` (`L_wmax = 100`, `b = 0.5`, making the bias
exponent exactly 1): `map(10) = ln(11) / ln(2 + (10/100)·8) / log10(101) = ln(11) / ln(2.8) /
log10(101) ≈ 1.161946` (> 1 as expected for `b < 0.7`).

**Construction domain.** `world_max` must be large enough that `world_max + 1 > 1` in f32
(`world_max ≳ 6×10⁻⁸`); below that the `log10(L_wmax + 1)` normalizer collapses to zero and Eq (4)
degenerates (`map(0)` would evaluate `inf · 0 = NaN`). The bias exponent and the normalizing
prefactor are computed once at construction (identical expressions, identical f32 values — pure
hoisting). Inputs are meaningful on the paper's domain `[0, L_wmax]`; outside it the curve stays
finite and non-NaN but is no longer a faithful reading of the model.

**Monotonicity is conditional (#439).** Eq (4) is not monotonic non-decreasing on `[0, L_wmax]`
for every parameter pair, and this is a property of the published formula, not of the crate's
transcription — the base of the bias exponent is `log(0.5)` in both Eq (3) and Eq (4) as printed,
with no `log(0.8)` variant anywhere in §3, and substituting `0.8` makes matters strictly worse
(it breaks monotonicity even at `L_wmax = 100`, where base `0.5` is clean).

Differentiating Eq (4): with `w = 8·(L_w/L_wmax)^k` and `k = log(b)/log(0.5)`, the derivative is
non-negative at `L_w` exactly when

    L_w · (2 + w) · ln(2 + w)  ≥  (L_w + 1) · ln(L_w + 1) · k · w

The tightest point of that on the domain is the top, `L_w = L_wmax`, where `w = 8`, giving the
closed form `gamut_tonemap::Drago::is_monotonic` evaluates:

    L_wmax · 10 · ln(10)  ≥  8 · k · (L_wmax + 1) · ln(L_wmax + 1)

The endpoint being the binding point is a numerical result rather than an analytic one: the ratio
of the two sides is *not* monotone in `L_w` (its `(2+w)ln(2+w)/w` factor dips near `w ≈ 3.2` and
rises again by `w = 8`), and interior minima up to 7% below the endpoint do occur. What was
checked is the thing that matters — that no interior minimum crosses 1 before the endpoint does.
Binary-searching the flip point of the closed form against a 40 000-point sweep of the exact
condition puts the two within six significant figures for every bias from 0.5 to 0.99, and the
closed form never permits a `L_wmax` the sweep rejects.

The resulting ceilings, which the `Drago` rustdoc reproduces:

| `b` | monotonic for `L_wmax` up to |
|---|---|
| 0.50 | 13.6 |
| 0.70 | 262 |
| 0.80 | 7.63 × 10³ |
| 0.85 (default) | 2.14 × 10⁵ |
| 0.90 | 1.67 × 10⁸ |
| 0.95 | 7.79 × 10¹⁶ |

The paper's own worked example is `L_wmax = 230 cd/m²` (Figure 5), so the default configuration
has three orders of magnitude of headroom. Note also that the paper divides `L_w` and `L_wmax` by
the world adaptation luminance `L_wa` before Eq (4) (§3.1), which `gamut-tonemap` deliberately
defers to `Exposure` — an unnormalized `L_wmax` of 10⁶ is not a case the paper contemplates.

---

## Cross-reference

- Luminance reference levels: `gamut_core::luminance`; provenance in `references/color/README.md`
  (ITU-R BT.2408 HDR Reference White; SMPTE ST 2084 PQ peak).
- The `Reinhard` operator is cross-checked at test time against `gamut-color`'s `bt2020_pq_to_sdr`
  (which applies the same `L/(1+L)` step) so the two crates' implementations are proven to agree.
