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
here** — primaries and white point come from `ColourPrimaries::chromaticities` for a CICP code
point and from `Gamut::chromaticities` for a named space, the RGB→XYZ
construction and Bradford adaptation from `gamut_color::matrix`, the ST 2084 curve and H.273
§8.2's BT.709-family α and β (`BT709_ALPHA` / `BT709_BETA`) from `gamut_color::transfer` — and
those constructors are themselves fallible, so an input whose colorimetry cannot be resolved is
declined rather than given a profile whose colorants are silently the PCS axes.

Four published constants gamut-color does *not* supply are written out here, because an ICC tag
needs them in a form no gamut-color function returns, and each is gated rather than trusted:
the IEC 61966-2-1 `(g, a, b, c, d)` parameter set for §10.18 type 3, pinned by
`srgb_parametric_curve_matches_gamut_color` against `gamut_color::transfer::srgb_eotf` and by
`oracle_every_encodable_transfer_matches_its_curve_through_lcms`; Adobe RGB (1998)'s `563 / 256`
exponent and the ROMM RGB `(g, a, b, c, d)` set, pinned by
`uncoded_space_curves_match_gamut_color_standard_curves` against gamut-color's
`adobe_rgb_eotf_standard` / `prophoto_rgb_eotf_standard` and by
`oracle_uncoded_space_curves_match_gamut_color_through_lcms`; and the PCS D50 of §7.2.16, which is an ICC fact rather than
a CIE one (see "PCS white" below), pinned by `colorants_sum_to_the_declared_media_white_point` and
the lcms2 colorant oracle. `src/builtin.rs`'s module documentation tabulates the four. Colorimetry is not the only
reason to decline: `from_cicp` also refuses signalling this profile *shape* cannot carry, which is
where narrow range lands — see "CICP fields the profile does not build from" below. `builtin`
yields `Some` for every `BuiltinProfile` in this release and a test pins that.

**Dependency direction: `gamut-icc → gamut-color`.** The constructors need gamut-color's
colorimetry and gamut-icc's serializer, and only one of the two can own that edge. gamut-color is
the primitive — fan-in 10 before this edge, 11 with it, and no ICC dependency — so pointing it the
other way would invert the layering and give a widely-depended-on crate a profile serializer it
has no use for. (Contrast `gamut-cmm → gamut-icc` below: applying a profile is a layer above
parsing one; *describing* a colour space is a layer below.)

**The buildable set is what gamut-color can express**, because a matrix/TRC profile *is* a
(primaries, transfer) pair. Four built-in spaces are coded on the two CICP axes — sRGB, linear sRGB,
Display P3 and BT.2100 PQ — and carry a `cicpType` tag. Two are not: Adobe RGB (1998) and ProPhoto
RGB (`BuiltinProfile::AdobeRgb = 4`, `ProPhotoRgb = 5`, issue #537) have no ITU-T H.273 code point
on either axis, so their primaries come from gamut-color's total `Gamut::chromaticities`, their
profiles carry **no** `cicpType` tag (§9.2.17 requires it to be equivalent to the encoding, and
`Unspecified` is not), and `BuiltinProfile::cicp` returns `None` for them. Each carries its
encoding's **standard** curve rather than gamut-color's encoder-exact one, because a profile is
read by third-party CMMs: Adobe RGB (1998) §4.3.1.2's `X^(563/256)` as `parametricCurveType` type 0
(exact in `s15Fixed16`), and the ROMM `X^1.8` with its linear toe `X / 16` below `X = 1/32` as type
3. `from_source_profile` builds `SourceProfile::ADOBE_RGB` / `PROPHOTO_RGB` as those two spaces,
and declines any other bundle missing a CICP axis (a hand-assembled mixture names no space).
ProPhoto is the one space whose own white is D50, so its `chad` is not a real adaptation — only the
CIE-to-ICC D50 step below, 2.5e-4 from the identity at its largest element (`prophoto_chad_is_near_the_identity`).

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
and 15 have no `TransferCharacteristics` variant at all, while codes 1 and 14 do, and `eotf_for`
maps both to the same Table 3 curve this crate writes (`gamut_color::transfer::bt709_eotf`, whose
α and β — `BT709_ALPHA` / `BT709_BETA` — the parametric tag borrows rather than restates). All four
are exactly encodable here as the one curve Table 3 defines. Each claim about gamut-color above is
an assertion in the doctest on `src/builtin.rs`'s module documentation: which of the four code
points `TransferCharacteristics::from_code_point` models, and that `eotf_for` evaluates Table 3's
`0.259719` at `V = 0.5` for both of those.

**A `SourceProfile` bundle's own curve is not always the profile's curve.** `from_source_profile`
projects a coded bundle onto its two CICP code points and builds from those. The two uncoded
bundles linearize with gamut-color's encoder-exact `x^2.2` / `x^1.8` while their profiles carry
the standard curves above, a divergence of up to **1.3e-4** (Adobe) and **4.2e-4** (ProPhoto,
inside the toe) over the same sweep, gated by the same test. For `SourceProfile::SRGB`
that is a distinction without a difference — the bundle's transfer *is* code point 13, and profile
and bundle agree to 4.2e-6, the `s15Fixed16` rounding of the tag. For `SourceProfile::BT2020` it is
not: that bundle's transfer is ST 2084 **plus a Reinhard tone map to SDR**, while its code point is
16, which is ST 2084 alone. The profile encodes 16, peak-referred, and over a 100 001-point sweep
of the signal domain the two curves diverge by up to **0.735** absolute — 52× at `V = 0.1`. All
five figures in this paragraph are asserted at the precision written here by
`source_profile_divergence_is_what_the_docs_quote`, beside the constructor they describe. This is
the opposite call from narrow-range RGB below, and deliberately so: a sample range is a property of
the *samples*, which a full-scale profile genuinely cannot describe, whereas a tone map is gamut-color's
choice about how to *render* an HDR transfer, and §9.2.17 requires the `cicpType` tag to be
equivalent to the encoding the profile represents. A caller wanting the tone-mapped rendering
applies it to its samples and embeds an SDR profile.

**CICP fields the profile does not build from.** `from_cicp` builds from the primaries and transfer
code points only, and treats the other two fields differently on purpose — one is always
rewritten, the other is a precondition on the RGB samples, read against the first.

`MatrixCoefficients` is **rewritten to zero**. §10.3 states that "when the data colour space in the
profile header is RGB or XYZ, MatrixCoefficients shall be 0 (zero)", so the caller's value —
routinely 1, 5, 6 or 9 in an AVIF/HEIC `nclx` box — is **not** written into the `cicpType` tag;
writing it would make the profile non-conforming for the most common input there is. Nothing is
lost: the coefficients describe a luma–chroma encoding the caller de-matrixes *before* this profile
applies, and they remain in the container signalling a decoder reads them from.

`VideoFullRangeFlag` is **read against the matrix coefficients**, because ITU-T H.273 §8.3 applies
it to Y, Cb and Cr under luma–chroma coefficients (1, 4–7, 9–15; equations 30 to 32) and to R, G
and B under the others (0, 8, 16, 17; equations 27 to 29):

| Signalled | What is narrow | Result |
| --- | --- | --- |
| flag `1`, any coefficients | nothing | built; tag flag `1` |
| flag `0`, luma–chroma coefficients (`9-16-9-0`, `1-13-6-0`) | Y, Cb, Cr — de-matrixed RGB is full scale | built, the same profile as flag `1`; tag flag `1` |
| flag `0`, RGB coefficients (§10.3's `1-1-0-0`, `9-16-0-0`) | the RGB samples themselves | **declined** |
| flag `0` under coefficient 2 (unknown) or a reserved one; any flag byte but `0`/`1` | unknown | **declined** |

The tag writes `1` for the admitted narrow-range case because it describes the RGB the profile
applies to, the same reason it writes `MatrixCoefficients` `0`: once the coefficients are `0`, a
flag of `0` would read as narrow-range *RGB*, and §9.2.17 requires that "the colour encoding
specified by the CICP tag content shall be equivalent to the data colour space encoding represented
by this ICC profile". The declined RGB case is the one no de-matrixing removes: this profile's
colorants, `chad` and tone curves are all defined over full-scale RGB, so normalising the flag to
`1` would return a profile that renders the caller's colour **wrongly**, not one that merely dropped
metadata. Declining is what the crate already does for primaries it has no chromaticities for and
for a transfer with no ICC tone curve. Callers holding narrow-range RGB samples scale them to full
range and pass `1`.

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
illuminant is an ICC fact, so this crate owns it. Both tristimuli and the 2e-4 gap are pinned by
`the_two_d50_tristimuli_the_doc_names_are_what_the_constants_hold`, so this figure cannot drift
from the constants it describes.

**The BT.2100 PQ profile is peak-referred, by decision ([#557]).** Its `curveType` samples ST 2084
normalized to the transfer's own 10 000 cd/m² peak, so signal maps to media-relative luminance as a
fraction of that peak. A diffuse-white signal (the 203 cd/m² BT.2408 reference white, `V` ≈ 0.58)
therefore lands at 0.0203 of media white: lcms2 measures `Y` = 0.020294 through the built-in
profile into its own XYZ profile, identically under media-relative colorimetric, ICC-absolute
colorimetric (the `mediaWhitePointTag` is the PCS D50, so absolute scaling is the identity) and
perceptual (a matrix/TRC profile has no perceptual table) intents; into lcms2's own sRGB at
media-relative intent the same signal is 0.152965, code 39 of 255 — dark grey where SDR-referred
content expects white. That is kept as `Bt2020Pq`'s only behaviour, with no diffuse-white-referred
sibling and no parameter: peak-referred is right for embedding beside HDR content in an HDR-aware
pipeline, and *no* normalization is right for an SDR CMM — referring the curve to diffuse white
clips the top ~5.6 stops of the PQ range with no roll-off, because a matrix/TRC profile has nowhere
to put a tone curve that is not also the transfer's. For an SDR rendering, tone-map the samples
with `gamut-tonemap`'s `ToneCurve` operators and embed an SDR profile (`Srgb`, `DisplayP3`) instead.
`validate()` reports nothing about it: the profile is conforming. Both measurements are asserted by
`oracle_pq_diffuse_white_lands_at_its_share_of_the_peak` and
`oracle_pq_diffuse_white_is_dark_grey_in_lcms_srgb` (`tests/oracle.rs`).

[#557]: https://github.com/visualcommons/gamut/issues/557

**Rendering intent.** Every constructor writes `Perceptual` in the header's rendering-intent field
(§7.2.15), `ProfileHeader::new`'s default. It is a surprising default on a colorimetrically exact
matrix/TRC profile, so it is disclosed rather than left to be discovered; the field is a preference
a CMM may override, and a caller wanting another sets `header.rendering_intent` before serializing.

**Determinism.** A constructor is a pure function of its arguments: no creation timestamp, no
profile ID, no other entropy, so the same call always serializes to the same bytes.

**Acceptance.** Every check below is lcms2 (`tooling/lcms2-oracle`) reading bytes this crate
serialized, compared against something this crate did not compute:

| What lcms2 checks | Over which profiles | Test |
| --- | --- | --- |
| Colorants equal the ones lcms2 derives from the same chromaticities (four `s15Fixed16` quanta) | `from_cicp` for **every** primaries code point it builds — 1, 5, 6, 9, 12 — which covers the coded built-in spaces' three | `oracle_colorants_match_lcms_for_every_buildable_primaries` |
| The same colorant check | **every** `BuiltinProfile`, including Adobe RGB and ProPhoto RGB, which no code point reaches | `oracle_colorants_match_lcms_for_every_builtin_space` |
| All three TRCs evaluate to gamut-color's standard curve (two `uInt16` quanta) | Adobe RGB and ProPhoto RGB, through the ROMM toe | `oracle_uncoded_space_curves_match_gamut_color_through_lcms` |
| `mediaWhitePointTag` equals the D50 lcms2 writes itself | every `BuiltinProfile` and the grey constructor | `oracle_media_white_point_matches_lcms` |
| All three TRCs evaluate to the transfer's own curve (two `uInt16` quanta) | `from_cicp` for **every** encodable transfer — 1, 6, 8, 13, 14, 15, 16 — against H.273 Table 3, the identity, lcms2's own sRGB and `gamut-color`'s `pq_eotf` | `oracle_every_encodable_transfer_matches_its_curve_through_lcms` |
| The `cicpType` tag equals the one lcms2's own `cicp` synthesiser writes | every buildable (primaries, transfer) pair, built from an `nclx`-style matrix 9 | `oracle_cicp_tag_matches_the_lcms_synthesiser` |
| The grey gamma lcms2 estimates is the one asked for | `gray_with_gamma` at 1.0, 1.8, 2.2 | `oracle_gray_gamma_matches_lcms` |
| A transform through our sRGB into lcms2's own sRGB is the identity to one 8-bit code | sRGB | `oracle_transform_through_our_srgb_is_the_identity` |
| A 203 cd/m² PQ signal lands at 0.0203 of lcms2's D50 media white (two `uInt16` quanta), under media-relative, absolute and perceptual intents | BT.2100 PQ | `oracle_pq_diffuse_white_lands_at_its_share_of_the_peak` |
| The same signal into lcms2's own sRGB, media-relative, is `srgb_oetf(0.0203)` to 1e-4 | BT.2100 PQ | `oracle_pq_diffuse_white_is_dark_grey_in_lcms_srgb` |

Not checked by lcms2: the `chad` tag's contents (it is read only through the colorants it
produced), and any `from_cicp` profile's end-to-end transform other than sRGB's and BT.2100 PQ's
neutral axis at diffuse white.

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
