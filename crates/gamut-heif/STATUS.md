# gamut-heif — implementation status

The shared HEIF/MIAF still-image layer (issue #1078). Rows are components; the codec crates
bind onto it — `gamut-avif` with `Av1` (#1085), `gamut-heic` with `Hevc` (#1086).

**Status:** ✅ = implemented · ☐ = deferred · **OOS** = permanently out of scope.

| Component | Spec | Status |
| --- | --- | --- |
| Role-typed item model (`HeifImage`, `HeifItem`, `ItemKind`) | ISO/IEC 23008-12 §6, §9; MIAF §7 | ✅ |
| Alpha / depth auxiliaries (MIAF/CICP and HEVC URNs) | ISO/IEC 23008-12 §6.5.8; MIAF §7.3.5 | ✅ |
| `irot`/`imir` composed into an EXIF `Orientation` | ISO/IEC 23008-12:2022 §6.5 (`imir` §6.5.12) | ✅ |
| Derivation walk (`iden`, `grid`), cycle/depth guard | ISO/IEC 23008-12 §6.6.2 | ✅ |
| `iovl` compositing (RGBA surfaces only) | ISO/IEC 23008-12 §6.6.2.4 | ✅ |
| `DecodeLimits` size and work caps, fallible allocation | — | ✅ |
| YCbCr → RGBA8 / RGBA16, matrices 0/1/2/5/6/9, monochrome | ITU-T H.273 §8.3 | ✅ |
| `ChromaUpsampling::Interpolated` (libavif bilinear) / `Nearest` | — | ✅ |
| `clap` crop, `irot`, `imir` on RGBA | ISO/IEC 14496-12 §12.1.4; ISO/IEC 23008-12 §6.5 | ✅ |
| `Codestream` binding over a `gamut_codec_abi::Registry` | — | ✅ |
| C2PA manifest-store locator and summary | C2PA 2.4 §A.5 | ✅ |
| YCgCo and chromaticity-derived matrices (8, 12, 13, 14) on RGBA | ITU-T H.273 §8.3 | ☐ |
| ICC-only `colr` on the RGBA surfaces (needs a CMM) | ISO/IEC 14496-12 §12.1.5 | ☐ |
| Typed metadata (`gamut-metadata` facade) over the item model | — | ☐ — stays in the codec crates |
| Fuzz driver for `invariants::decoding_stays_within_limits` | — | ☐ — the law and its pinned-seed property ship |
| Image sequences (`moov` tracks) | — | OOS — gamut is image-first |
| Coded-picture decode (AV1, HEVC) | — | OOS — the codec crates' backends |
