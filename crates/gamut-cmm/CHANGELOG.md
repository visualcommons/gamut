# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0](https://github.com/visualcommons/gamut/releases/tag/gamut-cmm-v0.1.0) - 2026-09-10

### Added

- *(cmm)* [**breaking**] take pipeline optimization on every transform constructor
- *(cmm)* collapse pipeline stages behind an opt-in optimization level
- *(gamut-cmm)* LUT pipelines and per-intent tag selection
- *(gamut-cmm)* matrix/TRC shaper pipelines and the chad convention
- *(gamut-cmm)* N-D multilinear and tetrahedral CLUT interpolation
- *(gamut-cmm)* tone-curve tables and monotonic inversion
- *(gamut-cmm)* new crate with the pipeline/stage model

### Fixed

- *(cmm)* sweep the full input domain and re-pin the conformance bounds

### Other

- deny unsafe in the hot-path crates instead of forbidding it
- *(cmm)* share one generator across the six oracle suites
- *(cmm)* drop the unreachable axis guard on CLUT resampling
- *(cmm)* record pipeline optimization in the README and status ledger
- *(cmm)* benchmark buffer throughput off, on, and against lcms2
- *(cmm)* gate the optimized paths against the lcms2 precision budget
- merge origin/master into feat/330-cmm-chaining-conformance
- merge origin/master into feat/329-cmm-intents-bpc
- merge origin/master into feat/328-cmm-lut-pipelines
- *(gamut-cmm)* record the LUT-phase settled decisions
- *(gamut-cmm)* pin LUT profiles and intent selection against Little-CMS
- *(gamut-cmm)* record the shaper and chad settled decisions
- *(gamut-cmm)* pin shaper profiles against Little-CMS
- *(gamut-cmm)* transcribe the tetrahedral decomposition
- *(gamut-cmm)* pin CLUT interpolation against Little-CMS
- *(gamut-cmm)* record the tone-curve phase decisions
- *(gamut-cmm)* pin tone curves against Little-CMS
