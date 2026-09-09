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
spec-valid v4 three-component matrix/TRC display profiles (§8.4). The colorimetry is **gamut-color's
and is never restated here**: primaries and white point come from
`ColourPrimaries::chromaticities`, the RGB→XYZ construction and Bradford adaptation from
`gamut_color::matrix`, the ST 2084 curve from `gamut_color::transfer`.

**Dependency direction: `gamut-icc → gamut-color`.** The constructors need gamut-color's
colorimetry and gamut-icc's serializer, and only one of the two can own that edge. gamut-color is
the primitive — fan-in 8, no ICC dependency — so pointing it the other way would invert the
layering and give a widely-depended-on crate a profile serializer it has no use for. (Contrast
`gamut-cmm → gamut-icc` below: applying a profile is a layer above parsing one; *describing* a
colour space is a layer below.)

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
| sRGB / IEC 61966-2-1 (code 13) | `parametricCurveType` type 3, `(g, a, b, c, d)` | §10.18 type 3 is the spec's own piecewise form |
| Grey gamma (`gray_with_gamma`) | `parametricCurveType` type 0 | §10.18 type 0; `s15Fixed16` beats `curveType`'s single `u8Fixed8` entry |
| PQ / ST 2084 (codes 16, 14) | `curveType`, 1024 `uInt16` samples | §10.18 defines no closed form for PQ; 1024 points keep interpolation error under one `uInt16` quantum |

**PCS white.** Colorants are Bradford-adapted to the D50 that `XYZNumber::D50` encodes (§7.2.16),
not to `gamut_color::matrix::D50`. The two differ by 2e-4 in Z — the CIE chromaticity against ICC's
rounded tristimulus — and adapting to the CIE one while writing the ICC one as the
`mediaWhitePointTag` leaves the colorants disagreeing with the white point they sum to. The PCS
illuminant is an ICC fact, so this crate owns it.

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
