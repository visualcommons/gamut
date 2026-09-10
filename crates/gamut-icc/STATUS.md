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
colorimetry they resolve is **gamut-color's, and whatever gamut-color can supply is never retyped
here** — primaries and white point come from `ColourPrimaries::chromaticities`, the RGB→XYZ
construction and Bradford adaptation from `gamut_color::matrix`, the ST 2084 curve from
`gamut_color::transfer` — and those constructors are themselves fallible, so an input whose
colorimetry cannot be resolved is declined rather than given a profile whose colorants are silently
the PCS axes.

Three published constants gamut-color does *not* supply are written out here, because an ICC tag
needs them in a form no gamut-color function returns, and each is gated rather than trusted:
H.273 §8.2's β (`BT709_BETA`, from which α is derived rather than restated), pinned by
`bt709_curve_inverts_the_h273_transfer` against an independent forward transcription of Table 3 and
by the lcms2 oracle; the IEC 61966-2-1 `(g, a, b, c, d)` parameter set for §10.18 type 3, pinned by
`srgb_parametric_curve_matches_gamut_color` against `gamut_color::transfer::srgb_eotf` and by
`oracle_srgb_tone_curve_matches_lcms`; and the PCS D50 of §7.2.16, which is an ICC fact rather than
a CIE one (see "PCS white" below), pinned by `colorants_sum_to_the_declared_media_white_point` and
the lcms2 colorant oracle. `src/builtin.rs`'s module documentation tabulates the three. Colorimetry is not the only
reason to decline: `from_cicp` also refuses signalling this profile *shape* cannot carry, which is
where narrow range lands — see "CICP fields the profile does not build from" below. `builtin`
yields `Some` for every `BuiltinProfile` in this release and a test pins that.

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
| PQ / ST 2084 (code 16) | `curveType`, 1024 `uInt16` samples | §10.18 defines no closed form for PQ; 1024 points keep interpolation error under one `uInt16` quantum — measured 0.986, guarded at 1 |

Declined: HLG (code 18) and Unspecified (code 2) have neither a §10.18 closed form nor a
gamut-color EOTF to sample from, and every other H.273 code point is unmodelled here. The transfer
axis is keyed on the **raw code point**, not on `gamut_color::cicp::TransferCharacteristics`,
because the set of curves an ICC tag can encode is not the set gamut-color can evaluate: codes 6
and 15 have no `TransferCharacteristics` variant at all; code 1 has one but no `eotf_for` curve;
and code 14 has an `eotf_for` curve that is **not this one** — `bt2020_pq_to_sdr`, a PQ EOTF plus
a tone map to SDR, which at `V = 0.5` is 20.3 % above what Table 3 gives code 14. All four are
exactly encodable here as the one curve Table 3 defines. That the two crates read code point 14
differently is a `gamut-color` question, filed as
[#605](https://github.com/visualcommons/gamut/issues/605) rather than resolved here; the doctest
on `src/builtin.rs`'s module documentation pins every clause of this paragraph that is a claim
about gamut-color, so it cannot drift from the crate it describes.

**A `SourceProfile` bundle's own curve is not always the profile's curve.** `from_source_profile`
projects the bundle onto its two CICP code points and builds from those. For `SourceProfile::SRGB`
that is a distinction without a difference — the bundle's transfer *is* code point 13, and profile
and bundle agree to 4.2e-6, the `s15Fixed16` rounding of the tag. For `SourceProfile::BT2020` it is
not: that bundle's transfer is ST 2084 **plus a Reinhard tone map to SDR**, while its code point is
16, which is ST 2084 alone. The profile encodes 16, peak-referred, and over a 100 001-point sweep
of the signal domain the two curves diverge by up to **0.735** absolute — 52× at `V = 0.1`. This is
the opposite call from narrow range above, and deliberately so: a sample range is a property of the
*samples*, which a full-scale profile genuinely cannot describe, whereas a tone map is gamut-color's
choice about how to *render* an HDR transfer, and §9.2.17 requires the `cicpType` tag to be
equivalent to the encoding the profile represents. A caller wanting the tone-mapped rendering
applies it to its samples and embeds an SDR profile.

**CICP fields the profile does not build from.** `from_cicp` builds from the primaries and transfer
code points only, and treats the other two fields differently on purpose — one is rewritten, one is
a precondition.

`MatrixCoefficients` is **rewritten to zero**. §10.3 states that "when the data colour space in the
profile header is RGB or XYZ, MatrixCoefficients shall be 0 (zero)", so the caller's value —
routinely 1, 5, 6 or 9 in an AVIF/HEIC `nclx` box — is **not** written into the `cicpType` tag;
writing it would make the profile non-conforming for the most common input there is. Nothing is
lost: the coefficients describe a luma–chroma encoding the caller de-matrixes *before* this profile
applies, and they remain in the container signalling a decoder reads them from.

`VideoFullRangeFlag` is **not** rewritten. A triple carrying anything but full range (`1`) is
**declined**, and carrying it through unchanged instead would be non-conforming rather than merely
inconsistent: §9.2.17 requires that "the colour encoding specified by the CICP tag content shall be
equivalent to the data colour space encoding represented by this ICC profile", which a narrow-range
triple beside full-scale colorants is not. §10.3's own RGB examples put the flag at zero (`1-1-0-0`, `9-16-0-0`), and read
`1-1-0-0` closely: with `MatrixCoefficients` already zero it is a narrow range on the *RGB samples
themselves*, which de-matrixing does not remove. This profile's colorants, `chad` and tone curves
are all defined over full-scale RGB, so normalising the flag to `1` would return a profile that
renders the caller's colour **wrongly**, not one that merely dropped metadata. Declining is what the
crate already does for primaries it has no chromaticities for and for a transfer with no ICC tone
curve. Callers holding narrow-range samples scale them to full range and pass `1`.

`gray_with_gamma` writes no `cicpType` tag at all: §9.2.17 permits the tag only for an RGB, YCbCr
or XYZ data colour space and says it shall not be present otherwise, and a monochrome profile's
space is `GRAY`.

**Grey gamma domain.** `gray_with_gamma` takes open `f64` input and declines anything the `kTRC`
cannot carry *non-degenerately*. The bound is on the encoding of the §10.18 type 0 parameter, not
on the number: `S15Fixed16::from_f64` (§4.6) rounds `gamma × 65 536` and then clamps, so it never
fails and it degenerates at both ends. Accepted is `0.5 / 65 536 = 7.629 394 531 25e-6` up to but
not including `(2^31 − 0.5) / 65 536 = 32 767.999 992 370 605 468 75`; below that the parameter
rounds to raw zero, which is a `Y = X^0` curve mapping every input to white and a tag `validate`
reports clean, and at or above it the parameter clamps to a gamma the encoder chose rather than the
caller. Non-finite input is declined with the rest. The two bounds are not the same kind of thing:
the bottom one refuses a **degenerate** profile, while the top one is **fidelity only** — the first
rejected gamma and the last accepted one are one `f64` ulp apart (`2^-38`, under four parts in a
trillion) and evaluate identically, so nothing is saved from misrendering there. It is kept because
a domain stated at both ends is easier to reason about than one open at the top, and the crate says
which kind each end is rather than implying both refuse harm.

The `profileDescriptionTag` names the gamma the tag **holds**, not the one requested:
`gray_with_gamma(2.2)` is described as `Grey gamma 2.1999969482421875`. At the smallest accepted
gamma the two differ by a factor of two, and a profile whose description contradicts its own tag is
worse than a long number.

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
