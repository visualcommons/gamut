# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [3.0.0](https://github.com/visualcommons/gamut/compare/gamut-color-v2.0.0...gamut-color-v3.0.0) - 2026-09-10

### Added

- *(avif)* [**breaking**] honour with_chroma on the 16-bit input path
- *(color)* report the coded plane count of a chroma layout
- *(dng)* type AsShotWhiteXY and derive its camera neutral per DNG 1.7.1 §6
- *(color)* correlated colour temperature by Robertson's method
- *(gamut-color)* add CIELab, LCh, xyY, ΔE₀₀, and the ICC PCS encodings
- *(core)* add structured error diagnostics
- add optional serde support to public enums

### Other

- deny unsafe in the hot-path crates instead of forbidding it
- *(color)* pin each Lab encoding claim separately
- *(dsp)* share one deterministic generator across the transform sweeps
- *(color)* pin the 16-bit box average to its exact value
- merge feat/398-av1-high-bitdepth into feat/399-avif-16bit-inputs
- merge feat/397-avif-alpha-aux into feat/398-av1-high-bitdepth
- merge origin/master into feat/397-avif-alpha-aux
- Merge branch 'feat/390-avif-420-profile0' into feat/391-avif-422-profile2
- Merge remote-tracking branch 'origin/master' into feat/390-avif-420-profile0
- Merge pull request #394 from visualcommons/feat/389-av1-per-plane-geometry
- Merge pull request #392 from visualcommons/feat/349-dng-asshotwhitexy
- merge origin/master into feat/322-lcms2-oracle-transforms
- merge origin/master into feat/321-color-cielab-de2000
- *(gamut-color)* state the fast-floor gap instead of denying it
- *(gamut-color)* kill the surviving lab mutants
- *(color)* vendor the Sharma CIEDE2000 paper and Lab references

## [2.0.0](https://github.com/justin13888/gamut/compare/gamut-color-v1.1.0...gamut-color-v2.0.0) - 2026-07-20

### Added

- *(gamut-color)* add SourceProfile::LINEAR_SRGB
- *(gamut-color)* add TransferCharacteristics::Linear (CICP code point 8)

## [1.1.0](https://github.com/justin13888/gamut/compare/gamut-color-v1.0.0...gamut-color-v1.1.0) - 2026-07-18

### Added

- *(color)* accept 16-bit samples in clip_pixel
- *(color)* add BitDepth::Sixteen and max_value()

### Other

- *(color)* record Sixteen as a non-AV1 modeled depth

## [0.3.0](https://github.com/justin13888/gamut/compare/gamut-color-v0.2.0...gamut-color-v0.3.0) - 2026-06-12

### Added

- *(av1)* [**breaking**] type ReconImage.bit_depth as BitDepth; add Planar8 view ctor
- *(color)* add clip_pixel8 pixel-saturation helper
- *(av1)* superres — horizontal upscaling (§7.16) with loop restoration after upscale
- *(color)* BT.601 YCbCr 4:2:0 conversion for VP8

### Other

- *(color)* [**breaking**] delete the unused PixelFormat enum, document BitDepth/ChromaSubsampling
- Merge pull request #142 from justin13888/feat/avif-still-image-compliance
- *(av1)* [**breaking**] widen reconstruction to u16 for high-bit-depth support
- *(color)* use Ord::clamp in clip_pixel8
- Merge pull request #101 from justin13888/feat/av1-lossy-superres
- Merge pull request #20 from justin13888/docs/crate-readmes
- add structurally consistent README to every crate
