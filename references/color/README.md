# Color-science constants — source of truth (issue #37)

This directory records the authoritative values for the color/tone-mapping math added
to `gamut-color` and `gamut-dsp` under issue #37, together with their primary-source
citations. Every constant baked into the code is listed here so the literals can be
audited against the standard rather than trusted by copy-paste.

The same-author upstream **chromahash** (`MIT OR Apache-2.0`) is the immediate provenance
for the *encoder-exact simplifications* (e.g. Adobe = pure `x^2.2`, ProPhoto = pure
`x^1.8`, BT.2020 = PQ→Reinhard@203). chromahash's `spec/validate.py` already derives the
OKLab `M1` matrices from first principles and checks them to `1e-7`; gamut reproduces that
derivation as a unit test (`gamut-color::matrix`) so the literal `M1_*` tables are
*computed-equivalent*, not merely transcribed.

> Determinism note: gamut implements these at **Tier-1 (correctness only)** using `std`
> f64 (`powf`/`cbrt`/`ln`). chromahash's bit-reproducible substrate (`cbrt_halley`,
> `portable_pow/exp/ln/cos`) is intentionally **not** ported — see issue #37. Therefore
> gamut's outputs agree with chromahash's golden vectors only within a small tolerance,
> not bit-for-bit.

---

## Vendored primary sources

The freely-published primary sources for the values below are vendored alongside this README so the
literals can be audited offline. The paywalled SMPTE/IEC/ISO editions cannot be redistributed — every
constant they define is instead transcribed with its exact value in the tables below (see
*Not vendored*).

| file | source |
|------|--------|
| `oklab-ottosson.html`    | Björn Ottosson, "A perceptual color space for image processing" — <https://bottosson.github.io/posts/oklab/> |
| `bradford-lindbloom.html`| Bruce Lindbloom, chromatic adaptation (Bradford CAT) — <http://www.brucelindbloom.com/Eqn_ChromAdapt.html> |
| `itu-r-bt709-6.pdf`      | ITU-R BT.709-6 — sRGB/BT.709 primaries & white point |
| `itu-r-bt2020-2.pdf`     | ITU-R BT.2020-2 — BT.2020 primaries & white point |
| `itu-r-bt2100-2.pdf`     | ITU-R BT.2100-2 — PQ/HLG systems (BT.2100-3 is now in force; `-2` is the edition cited below) |
| `itu-r-bt2408-8.pdf`     | Report ITU-R BT.2408-8 (2024) — HDR Reference White (203 cd/m²) for HDR production |
| `adobe-rgb-1998.pdf`     | Adobe RGB (1998) Color Image Encoding — primaries & γ (authentic Adobe file via the Internet Archive; Adobe's host no longer serves it) |
| `romm-rgb.pdf`           | ROMM RGB / ProPhoto white paper — the free primary reference for ISO 22028-2's reference encoding |
| `ciede2000-sharma-2005.pdf` | Sharma, Wu & Dalal, "The CIEDE2000 Color-Difference Formula: Implementation Notes, Supplementary Test Data, and Mathematical Observations", *Color Res. Appl.* 30(1):21–30 (2005) — <https://hajim.rochester.edu/ece/sites/gsharma/papers/CIEDE2000CRNAFeb05.pdf> |
| `ciede2000-testdata.txt` | The canonical 34-pair CIEDE2000 golden set accompanying the Sharma paper — <https://hajim.rochester.edu/ece/sites/gsharma/ciede2000/dataNprograms/ciede2000testdata.txt> |
| `lab-lindbloom.html`     | Bruce Lindbloom, XYZ ↔ Lab equations (the exact-rational ε/κ transcription) — <http://www.brucelindbloom.com/Eqn_XYZ_to_Lab.html> |

## Not vendored (paywalled — values transcribed inline below)

- **SMPTE ST 2084:2014** (PQ EOTF) and **SMPTE EG 432-1** (DCI-P3 with D65) — SMPTE, paywalled.
- **IEC 61966-2-1** (sRGB transfer functions) — IEC, paywalled.
- **ISO 22028-2** (ROMM/ProPhoto reference colour encoding) — ISO, paywalled; the ROMM RGB white paper
  above is the freely-published primary reference for the same primaries and encoding.
- **CIE 15:2004** (colorimetry — CIELab) — CIE, paywalled; the ε/κ junction rationals are
  transcribed in the *CIELab* section below, and the freely-published Lindbloom page above is
  the vendored transcription of the same equations. Its daylight locus — `x` as a cubic in
  `1/T` (`−4.6070e9/T³ + 2.9678e6/T² + 0.09911e3/T + 0.244063` for 4000–7000 K,
  `−2.0064e9/T³ + 1.9018e6/T² + 0.24748e3/T + 0.237040` for 7000–25 000 K) and
  `y = −3.000x² + 2.870x − 0.275` — is transcribed in `cct::daylight_xy`, coefficient for
  coefficient what Little-CMS's `cmsWhitePointFromTemp` (`third_party/lcms2/src/cmswtpnt.c`)
  carries.
- **Wyszecki & Stiles, _Color Science: Concepts and Methods, Quantitative Data and Formulae_,
  2nd ed. (Wiley, 1982), p. 228** (Robertson's isotemperature lines) — a book, not vendorable;
  the 31-row table is transcribed in the *Correlated colour temperature* section below.

---

## OKLab matrices — Björn Ottosson

Source: "A perceptual color space for image processing", Björn Ottosson,
<https://bottosson.github.io/posts/oklab/> (verified 2026-06). Public-domain / MIT.
Nonlinearity between `M1` and `M2` is the **cube root** (γ = 1/3).

`M1` — linear sRGB → LMS cone response:
```
0.4122214708  0.5363325363  0.0514459929
0.2119034982  0.6806995451  0.1073969566
0.0883024619  0.2817188376  0.6299787005
```

`M2` — cube-root LMS (l'm's') → OKLab [L, a, b]:
```
0.2104542553  0.7936177850  -0.0040720468
1.9779984951 -2.4285922050   0.4505937099
0.0259040371  0.7827717662  -0.8086757660
```

`M2⁻¹` — OKLab → cube-root LMS (first column is all 1.0):
```
1.0  0.3963377774  0.2158037573
1.0 -0.1055613458 -0.0638541728
1.0 -0.0894841775 -1.2914855480
```

`M1⁻¹` — LMS → linear sRGB:
```
 4.0767416621 -3.3077115913  0.2309699292
-1.2684380046  2.6097574011 -0.3413193965
-0.0041960863 -0.7034186147  1.7076147010
```

Per-gamut `M1[gamut] = M_LMS · M_XYZ[gamut]` (with Bradford D50→D65 baked into ProPhoto)
are listed in `gamut-color/src/oklab.rs`; they are verified against the derivation below.

---

## PQ — SMPTE ST 2084 / ITU-R BT.2100 EOTF

Source: SMPTE ST 2084:2014; ITU-R BT.2100-2. Constants are exact dyadic rationals:

| const | value            | rational            |
|-------|------------------|---------------------|
| m1    | `0.1593017578125`| `(2610/4096) / 4`   |
| m2    | `78.84375`       | `(2523/4096) · 128` |
| c1    | `0.8359375`      | `3424/4096`         |
| c2    | `18.8515625`     | `(2413/4096) · 32`  |
| c3    | `18.6875`        | `(2392/4096) · 32`  |

Peak luminance `10000` cd/m². EOTF (E' → linear, normalized to [0,1]·10000 nits — signal → light
is the EOTF per ST 2084; the encode direction is the *inverse* EOTF):
`Y = ((max(E'^(1/m2) − c1, 0)) / (c2 − c3·E'^(1/m2)))^(1/m1)`.
Note `c1 = c3 − c2 + 1`.

---

## HDR / SDR reference white — ITU-R BT.2408

Source: Report ITU-R BT.2408-8 (2024) (`itu-r-bt2408-8.pdf`, verified 2026-06). **HDR Reference
White = 203 cd/m²** — the nominal signal level of graphics / diffuse white in PQ and HLG
production. (Earlier code mislabeled this "SDR reference white"; 203 is the *HDR* reference level.)
The classic **SDR reference white is 100 cd/m²**; BT.2408 is the framework that maps SDR 100 %
diffuse white onto HDR Reference White. PQ peak luminance is **10 000 cd/m²** (ST 2084, above).

These three levels are defined once in `gamut_core::luminance` (`HDR_REFERENCE_WHITE_NITS = 203`,
`SDR_REFERENCE_WHITE_NITS = 100`, `PQ_PEAK_NITS = 10000`) and shared by `gamut-color` (the BT.2020
PQ→SDR path tone-maps relative to 203) and `gamut-tonemap` (`DEFAULT_REINHARD_WHITE = 203/100 =
2.03`). The tone-curve operators are documented in `references/tonemap/README.md`.

---

## sRGB transfer — IEC 61966-2-1

EOTF (gamma→linear): `x ≤ 0.04045 ? x/12.92 : ((x+0.055)/1.055)^2.4`.
OETF (linear→gamma): `x ≤ 0.0031308 ? 12.92·x : 1.055·x^(1/2.4) − 0.055`.

---

## Linear transfer — ITU-T H.273 / ISO/IEC 23091-2 code point 8

`TransferCharacteristics = 8` is **Linear**, defined as the identity `V = Lc` over the
reference domain (H.273 Table 3, row 8). There is no constant to transcribe: the curve is
`f(x) = x`, implemented as `gamut_color::transfer::linear_eotf` and paired with BT.709
primaries (code point 1) as `SourceProfile::LINEAR_SRGB` — the scene-linear working space a
RAW pipeline demosaics, white-balances, and colour-matrixes in before applying its output
transfer.

H.273 is freely published by the ITU: <https://www.itu.int/rec/T-REC-H.273>.

Vendored in this directory as: `T-REC-H.273-202407-I!!PDF-E.pdf`.s

---

## Encoder-exact transfer simplifications (chromahash)

These deliberately differ from the textbook curves; gamut exposes both so the bitstream a
metrics tool predicts matches what the encoder did:

| gamut       | encoder-exact (chromahash) | textbook                                   |
|-------------|----------------------------|--------------------------------------------|
| Adobe RGB   | `x^2.2`                    | `x^(563/256)` = `x^2.19921875`             |
| ProPhoto    | `x^1.8` (no toe)           | linear toe `slope 16` below `Eₜ = 1/512`   |
| BT.2020     | PQ→nits→Reinhard@203 nits  | pure ST 2084 → nits (no tone map)          |

---

## RGB primaries + white points (CIE 1931 xy)

Sources: ITU-R BT.709-6 (sRGB), ITU-R BT.2020-2 (BT.2020), SMPTE EG 432-1 (DCI-P3 with
D65), Adobe RGB (1998) Color Image Encoding, ISO 22028-2 / ROMM RGB (ProPhoto),
ITU-T H.273 Table 2 (the CICP `ColourPrimaries` code points).

| gamut        | R              | G                | B                  | white | CICP |
|--------------|----------------|------------------|--------------------|-------|------|
| sRGB/BT.709  | (0.6400,0.3300)| (0.3000,0.6000)  | (0.1500,0.0600)    | D65   | 1    |
| BT.601 625-line | (0.6400,0.3300)| (0.2900,0.6000) | (0.1500,0.0600)   | D65   | 5    |
| BT.601 525-line | (0.6300,0.3400)| (0.3100,0.5950) | (0.1550,0.0700)   | D65   | 6    |
| BT.2020      | (0.7080,0.2920)| (0.1700,0.7970)  | (0.1310,0.0460)    | D65   | 9    |
| Display P3   | (0.6800,0.3200)| (0.2650,0.6900)  | (0.1500,0.0600)    | D65   | 12   |
| Adobe RGB    | (0.6400,0.3300)| (0.2100,0.7100)  | (0.1500,0.0600)    | D65   | —    |
| ProPhoto RGB | (0.734699,0.265301)|(0.159597,0.840403)|(0.036598,0.000105)| D50  | —   |

White points: **D65** = (0.3127, 0.3290); **D50** = (0.3457, 0.3585).

The two BT.601 rows are **different** primary sets — 625-line is EBU Tech. 3213-E (PAL/SECAM),
525-line is SMPTE 170M (NTSC). This is the opposite of the *matrix* coefficients for the same
two code points, where 5 and 6 name identical `Kr`/`Kb` values. Adobe RGB and ProPhoto have no
CICP code point and are reachable only through `gamut_color::profile`.

---

## Bradford chromatic adaptation (cone response matrix)

Source: Lindbloom, <http://www.brucelindbloom.com/Eqn_ChromAdapt.html>; CIECAM/ICC Bradford
CAT (verified 2026-06).
```
 0.8951  0.2664 -0.1614
-0.7502  1.7135  0.0367
 0.0389 -0.0685  1.0296
```
Adaptation: `M_adapt = M_B⁻¹ · diag(cone_dst / cone_src) · M_B`, applied for non-D65
gamuts (ProPhoto's D50→D65) before the LMS projection.

---

## CIELab — CIE 15:2004 (exact junction rationals)

Source: CIE 15:2004 *Colorimetry* (paywalled, not vendored — verified 2026-08); freely-published
transcription: Bruce Lindbloom, <http://www.brucelindbloom.com/Eqn_XYZ_to_Lab.html>, vendored as
`lab-lindbloom.html`. The companding constants are **exact rationals**, never the rounded
`0.008856` / `903.3` forms (the two branches of `f` are tangent only at the exact ε):

| const | value                 | rational      |
|-------|-----------------------|---------------|
| ε     | `0.008856451679…`     | `216/24389`   |
| κ     | `903.2962962…`        | `24389/27`    |

`f(t) = t^(1/3)` for `t > ε`, else `(κ·t + 16)/116`; note `κ·ε = 8` exactly (the L\* value at
the junction). Implemented as `gamut_color::lab` with the whites `D50 = (63190/65536, 1,
54061/65536)` (the ICC PCS illuminant at its exact u1Fixed15 header encoding, ICC.1:2022
§7.2.16 — lcms2 instead uses the rounded `0.9642`/`0.8249` literals) and `D65` derived from the
chromaticity `(0.3127, 0.3290)` in the table above.

CIEDE2000 (ΔE₀₀) follows Sharma, Wu & Dalal (2005), vendored as `ciede2000-sharma-2005.pdf`,
Eqs. (2)–(22) — including the ±360° hue adjustments and the `C1′·C2′ = 0` sum convention their
"mathematical observations" flag — and is pinned to the vendored 34-pair golden set
`ciede2000-testdata.txt` at 1e-4 in both argument orders. ΔE94 and ΔE-CMC are deliberately out
of scope (documented in `gamut_color::lab`).

---

## ICC PCS Lab/XYZ encodings

Source: ICC.1:2022 Annex A and the legacy ICC.1:2001-04 §6.3.4, both vendored under
[`references/icc/`](../icc/) (`icc.1-2022-05.pdf`, `icc.1-2001-04.pdf`). The fixed-point
codecs — PCSXYZ u1Fixed15 (`v·32768`, range `0 … 1 + 32767/32768`), 16-bit PCSLAB v4
(`L·65535/100`, `(a|b + 128)·257`), the legacy 16-bit v2 with its `0xFF00` top-end scaling
(`L·652.8`, `(a|b + 128)·256`), and 8-bit Lab — live in `crates/gamut-color/src/lab.rs`, with
rounding and clamping replicating lcms2's `cmspcs.c` (floor of `v + 0.5`, saturating) so the
planned lcms2 differential tests (issue #322) can demand exact agreement.

---

## µ-law companding (chromahash v0.6)

`compress(v) = sign(v)·ln(1+µ|v|)/ln(1+µ)`, `expand(c) = sign(c)·((1+µ)^|c| − 1)/µ`.
Quantization uses an **odd** level count: `max_idx = 2^bits − 2` (the top code is never
written), so the center index dequantizes to exactly 0. Round-half-away-from-zero.
Defaults: `µ_L = 5.0`, `µ_C = 8.0`, `µ_alpha = 5.0`.

---

## XYB (JPEG XL opsin colour space) — ISO/IEC 18181-1 / libjxl

Sources: the pre-ISO Committee Draft `references/jxl/1908.03565.pdf` (defines the XYB space and
the normative inverse), and the **frozen** reference-implementation constants of **libjxl
0.12.0** — the exact version this workspace pins as its JXL oracle (`references/jxl/README.md`) —
vendored verbatim (BSD-3-Clause, header retained) as `references/jxl/opsin_params.h`.
Implemented by `gamut-color`'s `xyb` module; consumed by `gamut-jpeg`'s XYB colour mode and its
embedded ICC profile (regenerated and byte-pinned by `crates/gamut/tests/xyb_icc.rs`).

Forward: linear sRGB → opsin absorbance matrix → per-channel `∛(x + b) − ∛b` →
`X = (L′−M′)/2, Y = (L′+M′)/2, B = S′`.

Opsin absorbance matrix (`kOpsinAbsorbanceMatrix`; middle/last entries of each row are defined
as `1 −` the others, so each row sums to 1):
```
0.30                  1 − 0.078 − 0.30      0.078
0.23                  1 − 0.078 − 0.23      0.078
0.24342268924547819   0.20476744424496821   1 − kM20 − kM21
```
Bias (`kOpsinAbsorbanceBias`, all channels): `b = 0.0037930732552754493`.

Frozen inverse (`kDefaultInverseOpsinAbsorbanceMatrix` — transcribed, not re-derived: the decode
direction is normative and carries its own f32-rounded literals):
```
 11.031566901960783  −9.866943921568629   −0.16462299647058826
 −3.254147380392157   4.418770392156863   −0.16462299647058826
 −3.6588512862745097  2.7129230470588235   1.9459282392156863
```

Scaled-XYB byte encoding (`kScaledXYBOffset` / `kScaledXYBScale`; the **third stored channel is
`B − Y`**): `sᵢ = clamp((storedᵢ + offsetᵢ)·scaleᵢ, 0, 1)` with
```
offset = (0.015386134, 0.0, 0.27770459)
scale  = (22.995788804, 1.183000077, 1.502141333)
```

XYB ICC `A2B0` matrix (libjxl `jxl_cms_internal.h`, `CreateICCLutAtoBTagForXYB`): the literal
`0.5 · XYZ(D50)←linear-sRGB · inverse-opsin` (the 0.5 bakes in the mAB PCS-XYZ encoding
ceiling), verified against a fresh derivation in `crates/gamut/tests/xyb_icc.rs` before use:
```
 1.5170095  −1.1065225   0.071623
−0.050022    0.5683655  −0.018344
−1.387676    1.1145555   0.6857255
```

---

## Correlated colour temperature — Robertson's method

`cct::cct_from_xy` implements Robertson's method: convert the chromaticity to CIE 1960 UCS
`(u, v)`, walk a table of *isotemperature lines* — points on the Planckian locus, each with the
slope of the constant-CCT line crossing it — find the pair the sample falls between, and
interpolate in **reciprocal** temperature (mired), where the locus is near-linear.

The table is Wyszecki & Stiles, 2nd ed., p. 228: 31 rows of `(mired, u, v, slope)` from 0 mired
(∞ K) to 600 mired (1667 K), transcribed verbatim into `crates/gamut-color/src/cct.rs`. That table
is also what the **Adobe DNG SDK** carries in `dng_temperature.cpp`, which is why the two agree:
DNG 1.7.1's "One, Two, or Three Color Calibrations" requires "linear interpolation using inverse
correlated color temperature", so a DNG reader must resolve a white point's CCT exactly this way to
weight a camera's colour calibrations. `gamut-dng` uses it for the §6 `AsShotWhiteXY` conversion,
gated against the SDK's own derivation.

Accuracy is the method's, not the transcription's: the standard illuminants recover their nominal
temperatures to within ~10–20 K, which is what interpolating a 31-row table buys. Distance from the
locus (the "tint" axis the SDK also reports) is deliberately not modelled — calibration
interpolation weights on CCT alone.

---

## YCbCr matrix coefficients — ITU-T H.273 §8.3 / ISO/IEC 23091-2

Source: ITU-T H.273 (2024-07) Table 4 (`MatrixCoefficients`) and §8.3, the non-constant-luminance
YCbCr ↔ RGB relations. Backs `gamut_color::YcbcrMatrix`.

The published luma weights are exact four-decimal values, so the derivation is exact integer
arithmetic — unlike the rest of this directory, that path is bit-exact and deterministic rather
than Tier-1 `f64`.

| Code point | Name | `Kr` | `Kb` |
| --- | --- | --- | --- |
| 1 | BT.709 / BT.1361 / sRGB-matrix | 0.2126 | 0.0722 |
| 5 | BT.470 System B,G / BT.601 625 | 0.2990 | 0.1140 |
| 6 | BT.601 525 / SMPTE 170M | 0.2990 | 0.1140 |
| 9 | BT.2020 non-constant luminance | 0.2627 | 0.0593 |

Code points 5 and 6 are distinct points naming identical coefficients; both are modeled so a `colr`
box read as 5 is written back as 5.

With `Kg = 1 − Kr − Kb`, the inverse (de-matrixing) is

```
R' = Y'                              + 2(1 − Kr)·Cr
G' = Y' − (2·Kb(1 − Kb)/Kg)·Cb − (2·Kr(1 − Kr)/Kg)·Cr
B' = Y' + 2(1 − Kb)·Cb
```

Range normalization at bit depth `bd`, with `max = 2^bd − 1` (H.273 §8.3):

| | luma offset | luma scale | chroma offset | chroma scale |
| --- | --- | --- | --- | --- |
| Limited ("studio swing") | `16 << (bd − 8)` | `219 << (bd − 8)` | `128 << (bd − 8)` | `224 << (bd − 8)` |
| Full | `0` | `max` | `2^(bd − 1)` | `max` |

`Y' = (Y − luma_offset)/luma_scale`, `Cb = (cb − chroma_offset)/chroma_scale`, and the output
sample is `round(max · R')` saturated to `0..=max` (the AV1 `Clip1` of `clip_pixel`).

Two consequences worth recording, both asserted by tests: the full-range chroma coefficients are
bit-depth independent (`max / chroma_scale = 1`), and the limited-range luma gain depends only on
`(range, bit_depth)`, never on the matrix.

Note that `gamut_color::ycbcr_to_rgb` does **not** use these relations: it is a bit-exact port of
libwebp's `VP8YUVToR/G/B`, kept so WebP decode matches libwebp per pixel. The two are both correct
BT.601 and differ by at most 1 LSB.
