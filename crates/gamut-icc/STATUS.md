# gamut-icc — ICC profile implementation status

Part of the **image metadata primitives** campaign (GitHub issue #34). Implements the ICC profile
format (`references/icc`, ICC.1:2022 = ISO 15076-1) as a parser + serializer.

**Keystone:** the multi-dimensional transform tag types — `lutAToB`/`lutBToA` (`mAB `/`mBA `) and the
legacy `lut8`/`lut16` — which carry the matrix → curve → CLUT → curve pipeline that defines
device↔PCS conversion.

**Oracle:** differential vs **Little-CMS (lcms2)** (dev-only FFI, `tooling/lcms2-oracle`) — gamut-icc
decodes lcms-synthesized profiles to the same values lcms reports, and lcms re-opens gamut-icc's
serialization as an equivalent profile.

## Phases

| Phase | Spec § | Scope | Status |
| ----- | ------ | ----- | ------ |
| P1 | — | Scaffold: crate, workspace wiring, docs, data-model skeleton | ✅ |
| P2 | §7.2–7.3 | Header parse (all fields) + tag table | ✅ |
| P3 | §10 | Simple element types: `XYZType`, `curveType`, `parametricCurveType`, `textType`, `multiLocalizedUnicodeType` | ✅ |
| P4 | §9 | Matrix/TRC (shaper) profiles: `rXYZ`/`gXYZ`/`bXYZ` + `rTRC`/`gTRC`/`bTRC` + `wtpt`/`chad`/`desc`/`cprt` | ✅ |
| P5 | §10 | **Keystone** — LUT transform types: `lut8`/`lut16`/`lutAToB`/`lutBToA` | ✅ |
| P6 | §7 | Writer/serialize + round-trip; `size` and profile-ID (MD5) recomputation | ✅ |
| P7 | — | v2 legacy quirks (`textDescriptionType`) | ✅ |
| P8 | — | lcms2 differential oracle gate | ✅ |
| P9 | §10 | **Full §10 coverage** — every remaining element type decoded (see below) | ✅ |
| P10 | §8 | Profile-class conformance validation (`IccProfile::validate`) | ✅ |
| P11 | — | **v1 stabilization** (issue #180): API-surface hardening (private modules, `Eq` model, std conversion traits, validated fallible writes, `ProfileHeader::new`) + spec-citation audit | ✅ |
| P12 | §8.4, §9.2 | **Built-in profiles** (issue #424): `IccProfile::builtin` / `gray_with_gamma` / `from_cicp` / `from_source_profile` | ✅ |

## Modelled element types

**Every ICC.1:2022 §10 element type is decoded semantically:** `XYZType`, `curveType`,
`parametricCurveType` (function types 0–4), `textType`, `multiLocalizedUnicodeType`,
`textDescriptionType` (v2), `dateTimeType`, `signatureType`, `s15Fixed16ArrayType`, `lut8Type`,
`lut16Type`, `lutAToBType`, `lutBToAType`, `namedColor2Type`, `chromaticityType`, `cicpType`,
`measurementType`, `viewingConditionsType`, `dataType`, `colorantOrderType`, `colorantTableType`,
`u16Fixed16ArrayType`, `uInt8/16/32/64ArrayType`, `profileSequenceDescType`,
`profileSequenceIdentifierType`, `responseCurveSet16Type`, and `dictType`.

Any element type *not* defined in ICC.1:2022 §10 (e.g. iccMAX's `multiProcessElementsType`, or
private/unregistered types) is preserved verbatim as `TagData::Raw` and round-trips byte-for-byte,
so no profile is rejected for carrying an unmodelled tag.

## Built-in profiles

`IccProfile::builtin`, `gray_with_gamma`, `from_cicp` and `from_source_profile` construct
spec-valid v4 three-component matrix/TRC display profiles (§8.4). All four return `Option`: the
colorimetry they resolve is **gamut-color's and is never restated here** — primaries and white
point come from `ColourPrimaries::chromaticities`, the RGB→XYZ construction and Bradford
adaptation from `gamut_color::matrix`, the ST 2084 curve from `gamut_color::transfer` — and those
constructors are themselves fallible, so an input whose colorimetry cannot be resolved is declined
rather than given a profile whose colorants are silently the PCS axes. `builtin` yields `Some` for
every `BuiltinProfile` in this release and a test pins that.

**Dependency direction: `gamut-icc → gamut-color`.** The constructors need gamut-color's
colorimetry and gamut-icc's serializer, and only one of the two can own that edge. gamut-color is
the primitive — fan-in 10 before this edge, 11 with it, and no ICC dependency — so pointing it the
other way would invert the layering and give a widely-depended-on crate a profile serializer it
has no use for. (Contrast `gamut-cmm → gamut-icc` below: applying a profile is a layer above
parsing one; *describing* a colour space is a layer below.)

**The buildable set is exactly what gamut-color can express on the two CICP axes**, because a
matrix/TRC profile *is* a (primaries, transfer) pair: sRGB, linear sRGB, Display P3 and BT.2100 PQ.
`SourceProfile::ADOBE_RGB` and `PROPHOTO_RGB` return `None` from both `colour_primaries()` and
`transfer_characteristics()`, and their chromaticities are private to gamut-color, so
`from_source_profile` declines them rather than restating tables this crate does not own. Adding
them needs a public chromaticity accessor in gamut-color first — tracked separately.

**Tone-curve shape per transfer.** A `parametricCurveType` (§10.18) is used wherever ITU-T H.273
gives the transfer a closed form ICC also defines, and a sampled `curveType` (§10.6) otherwise:

| Transfer (H.273) | ICC encoding | Deciding clause |
| ---------------- | ------------ | --------------- |
| Linear (code 8) | `parametricCurveType` type 0, `g = 1` | §10.18 type 0 is `Y = X^g` exactly |
| BT.709 family (codes 1, 6, 14, 15) | `parametricCurveType` type 3, `(g, a, b, c, d)` | Table 3 gives all four one curve — "functionally the same as the values 1, 6 and 15" — with α = 1 + 5.5β and β = 0.018053968510807…; its inverse is §10.18 type 3 exactly |
| sRGB / IEC 61966-2-1 (code 13) | `parametricCurveType` type 3, `(g, a, b, c, d)` | §10.18 type 3 is the spec's own piecewise form |
| Grey gamma (`gray_with_gamma`) | `parametricCurveType` type 0 | §10.18 type 0; `s15Fixed16` beats `curveType`'s single `u8Fixed8` entry |
| PQ / ST 2084 (code 16) | `curveType`, 1024 `uInt16` samples | §10.18 defines no closed form for PQ; 1024 points keep interpolation error under one `uInt16` quantum |

Declined: HLG (code 18) and Unspecified (code 2) have neither a §10.18 closed form nor a
gamut-color EOTF to sample from, and every other H.273 code point is unmodelled here. The transfer
axis is keyed on the **raw code point**, not on `gamut_color::cicp::TransferCharacteristics`,
because the set of curves an ICC tag can encode is not the set gamut-color can evaluate: codes 6
and 15 have no `TransferCharacteristics` variant, and none of the four BT.709-family codes has a
gamut-color EOTF, yet all four are exactly encodable.

**CICP fields the profile does not carry.** `from_cicp` builds from the primaries and transfer
code points only. §10.3 states that "when the data colour space in the profile header is RGB or
XYZ, MatrixCoefficients shall be 0 (zero)", so the caller's `MatrixCoefficients` — routinely 1, 5,
6 or 9 in an AVIF/HEIC `nclx` box — is **not** written into the `cicpType` tag; writing it would
make the profile non-conforming for the most common input there is. `VideoFullRangeFlag` is
normalized to `1` alongside it, because the profile's matrix and tone curves are defined over
full-scale RGB. Neither is a loss of information: both describe a luma–chroma encoding the caller
de-matrixes *before* this profile applies, and both remain in the container signalling a decoder
reads them from.

**Grey gamma domain.** `gray_with_gamma` takes open `f64` input and declines anything a `kTRC`
cannot carry: non-finite, non-positive, or ≥ 32 768, the first magnitude `s15Fixed16` (§4.6) cannot
hold. `S15Fixed16::from_f64` saturates rather than failing, so accepting those would silently write
a gamma nobody asked for.

**PCS white.** Colorants are Bradford-adapted to the D50 that `XYZNumber::D50` encodes (§7.2.16),
not to `gamut_color::matrix::D50`. The two differ by 2e-4 in Z — the CIE chromaticity against ICC's
rounded tristimulus — and adapting to the CIE one while writing the ICC one as the
`mediaWhitePointTag` leaves the colorants disagreeing with the white point they sum to. The PCS
illuminant is an ICC fact, so this crate owns it.

**Known limit: the BT.2100 PQ profile is peak-referred.** Its `curveType` samples ST 2084
normalized to the transfer's own 10 000 cd/m² peak, so signal maps to media-relative luminance as a
fraction of that peak. A diffuse-white signal (the ~203 cd/m² BT.2408 reference white) therefore
evaluates to roughly 0.02 media-relative, and a CMM rendering such content through this profile
puts it near black. That is mathematically correct for a peak-referred profile and it is not what a
caller embedding the profile alongside SDR-referred content necessarily expects; choosing a
diffuse-white-referred normalization instead is a colour-appearance decision with its own
consequences, so it is tracked separately rather than changed silently here.

**Determinism.** A constructor is a pure function of its arguments: no creation timestamp, no
profile ID, no other entropy, so the same call always serializes to the same bytes.

**Acceptance.** lcms2 re-opens every constructed profile and reports the same colorants as it
derives from the same primaries itself (within four `s15Fixed16` quanta), evaluates our sRGB TRC to
the same values as the sRGB profile it synthesizes itself, estimates the grey gamma we asked for,
and transforms through our sRGB into its own sRGB as the identity to within one 8-bit code.

## Deferred / intentional leniencies

- **Applying transforms** (a CMM): gamut-icc parses and serializes profiles; evaluating a profile's
  device↔PCS conversion is out of scope. Curve/`XYZ`/matrix values expose `to_f64`/`eval` accessors
  as the seam.
- **`dictType` "present but not for display" marker** (§10.9): a display name/value with a nonzero
  offset and zero size decodes as absent, so that marker does not round-trip distinctly (the
  empty-vs-absent distinction for *value strings* **is** preserved). Documented in `src/dict.rs`.
- **`mAB `/`mBA ` stage combinations** (§10.12.1/§10.13.1): the spec permits only certain stage
  combinations (B alone; M+matrix+B; A+CLUT+B; all five). gamut-icc accepts and re-emits *any*
  combination a profile signals (only the B-curves are required), so non-conformant real-world
  profiles still round-trip losslessly.
- **CMM integration**: building runnable transforms (matrix/TRC → pipeline, `chad` application)
  belongs in `gamut-cmm` (epic #323; dependency direction `gamut-cmm → gamut-icc`), not here.
- **`multiProcessElementsType`** (`mpet`, and the `D2Bx`/`B2Dx` transform tags that use it): the
  v4/iccMAX general-purpose processing pipeline is preserved as `Raw` rather than decoded. Every
  other §10 element type is now modelled (see above).
- **iccMAX (`ICC.2:2019`, profile version 5)**: out of scope. iccMAX is a separate, parallel
  next-generation format (spectral PCS, `multiProcessElementsType`, ~20 new tag types), *not* an
  extension of the ICC.1 format this crate targets; the real-world profiles embedded in images are
  all ICC.1 v2/v4, and the lcms2 oracle does not implement iccMAX. See
  [`references/icc/README.md`](../../references/icc/README.md).
