# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [2.0.1](https://github.com/visualcommons/gamut/compare/gamut-dsp-v2.0.0...gamut-dsp-v2.0.1) - 2026-09-10

### Other

- deny unsafe in the hot-path crates instead of forbidding it
- *(dsp)* share one deterministic generator across the transform sweeps

## [2.0.0](https://github.com/justin13888/gamut/compare/gamut-dsp-v1.0.0...gamut-dsp-v2.0.0) - 2026-07-18

### Added

- *(dsp)* add JPEG-1 8x8 forward/inverse DCT kernels

### Other

- *(dsp)* trim trailing whitespace in crate docs
- *(gamut-dsp)* [**breaking**] remove mulaw module

## [0.2.1](https://github.com/justin13888/gamut/compare/gamut-dsp-v0.2.0...gamut-dsp-v0.2.1) - 2026-06-12

### Added

- *(dsp)* add mu-law compand and odd-level quantization
- *(dsp)* add shared scalar math primitives
- *(dsp)* add AV1 inverse/forward ADST + identity transforms
- *(dsp)* add AV1 forward/inverse DCT (4/8/16/32/64)

### Other

- Merge pull request #20 from justin13888/docs/crate-readmes
- add structurally consistent README to every crate
