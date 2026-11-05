# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.0](https://github.com/visualcommons/gamut/compare/gamut-cmm-v0.1.0...gamut-cmm-v0.2.0) - 2026-11-05

### Added

- *(gamut-jpeg)* [**breaking**] XYB parity with jpegli — B-only subsampling and an sRGB decode through gamut-cmm ([#1043](https://github.com/visualcommons/gamut/pull/1043))
- *(gamut-cmm)* [**breaking**] add f32 and 16-bit fixed-point CLUT precision tiers ([#994](https://github.com/visualcommons/gamut/pull/994))

### Other

- *(gamut-cmm)* [**breaking**] growable options, image-typed buffers, no name collisions ([#1153](https://github.com/visualcommons/gamut/pull/1153))
- *(gamut-icc)* [**breaking**] named tag table, repr'd header enums, core Cicp/Error, split builtin ([#1146](https://github.com/visualcommons/gamut/pull/1146))
- *(gamut-color)* [**breaking**] one chroma/CICP vocabulary, kernels to gamut-dsp, Color spelling ([#1135](https://github.com/visualcommons/gamut/pull/1135))
- correct per-crate prose that contradicts shipped code ([#1127](https://github.com/visualcommons/gamut/pull/1127))
- bring repository-wide docs in line with the shipped code ([#1114](https://github.com/visualcommons/gamut/pull/1114))
- *(gamut-cmm)* gate LUT sources into a sampled-TRC destination against lcms2 ([#1023](https://github.com/visualcommons/gamut/pull/1023))
- *(gamut-cmm)* add lcms2's 8-bit matrix-shaper pass ([#1001](https://github.com/visualcommons/gamut/pull/1001))

## [0.1.0](https://github.com/visualcommons/gamut/releases/tag/gamut-cmm-v0.1.0) - 2026-10-12

### Added

- *(color)* model H.273 transfer code points 6 and 15 ([#779](https://github.com/visualcommons/gamut/pull/779))
- *(gamut-cmm)* [**breaking**] option to evaluate cicp-signalled transfer functions in closed form ([#716](https://github.com/visualcommons/gamut/pull/716))
- *(cmm)* [**breaking**] take pipeline optimization on every transform constructor
- *(cmm)* collapse pipeline stages behind an opt-in optimization level
- *(gamut-cmm)* pixel-buffer application over gamut-core formats
- *(gamut-cmm)* transform chaining, device links, proofing, and gamut check
- *(gamut-cmm)* absolute colorimetric intent and black point compensation
- *(gamut-cmm)* LUT pipelines and per-intent tag selection
- *(gamut-cmm)* matrix/TRC shaper pipelines and the chad convention
- *(gamut-cmm)* N-D multilinear and tetrahedral CLUT interpolation
- *(gamut-cmm)* tone-curve tables and monotonic inversion
- *(gamut-cmm)* new crate with the pipeline/stage model

### Fixed

- *(cmm)* sweep the full input domain and re-pin the conformance bounds
- *(gamut-cmm)* guard the empty chain before indexing it
- *(gamut-cmm)* drive buffer chunking by iterator, not a mutable cursor

### Other

- gate rustdoc warnings and compile README code blocks as doctests ([#713](https://github.com/visualcommons/gamut/pull/713))
- deny unsafe in the hot-path crates instead of forbidding it
- *(cmm)* share one generator across the six oracle suites
- *(cmm)* drop the unreachable axis guard on CLUT resampling
- *(cmm)* record pipeline optimization in the README and status ledger
- *(cmm)* benchmark buffer throughput off, on, and against lcms2
- *(cmm)* gate the optimized paths against the lcms2 precision budget
- merge origin/master into feat/330-cmm-chaining-conformance
- merge origin/master into feat/329-cmm-intents-bpc
- merge origin/master into feat/328-cmm-lut-pipelines
- *(gamut-cmm)* conformance thresholds and the v1 closing ledger
- *(gamut-cmm)* ΔE₀₀ conformance gate against Little-CMS
- vendor the black-point-compensation and rendering-intent references
- *(gamut-cmm)* pin intents and BPC against Little-CMS
- *(gamut-cmm)* record the LUT-phase settled decisions
- *(gamut-cmm)* pin LUT profiles and intent selection against Little-CMS
- *(gamut-cmm)* record the shaper and chad settled decisions
- *(gamut-cmm)* pin shaper profiles against Little-CMS
- *(gamut-cmm)* transcribe the tetrahedral decomposition
- *(gamut-cmm)* pin CLUT interpolation against Little-CMS
- *(gamut-cmm)* record the tone-curve phase decisions
- *(gamut-cmm)* pin tone curves against Little-CMS
