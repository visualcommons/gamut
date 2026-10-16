# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.0](https://github.com/visualcommons/gamut/compare/gamut-ffi-v0.1.3...gamut-ffi-v0.2.0) - 2026-10-12

### Added

- *(gamut-isobmff, gamut-heic, gamut-avif)* [**breaking**] C2PA ranges are a C2paByteRange ([#812](https://github.com/visualcommons/gamut/pull/812))
- *(ffi)* expose AvifEncoder::with_keep_opaque_alpha as gamut_avif_encoder_set_keep_opaque_alpha ([#728](https://github.com/visualcommons/gamut/pull/728))
- *(avif)* drop an all-opaque alpha plane on RGBA encode ([#720](https://github.com/visualcommons/gamut/pull/720))
- *(ffi)* expose AVIF alpha quality as gamut_avif_encoder_set_alpha_quality ([#704](https://github.com/visualcommons/gamut/pull/704))
- *(ffi)* AVIF C2PA slot setters and the encode report's slot range ([#700](https://github.com/visualcommons/gamut/pull/700))
- *(ffi)* add AVIF encode and container-parse C entry points ([#681](https://github.com/visualcommons/gamut/pull/681))
- *(gamut-cmm)* new crate with the pipeline/stage model
- *(core)* add structured error diagnostics

### Fixed

- *(gamut-core)* [**breaking**] make PixelFormat::ALL a slice ([#669](https://github.com/visualcommons/gamut/pull/669))

### Other

- gate rustdoc warnings and compile README code blocks as doctests ([#713](https://github.com/visualcommons/gamut/pull/713))

## [0.1.3](https://github.com/visualcommons/gamut/compare/gamut-ffi-v0.1.2...gamut-ffi-v0.1.3) - 2026-07-30

### Other

- updated the following local packages: gamut

## [0.1.2](https://github.com/justin13888/gamut/compare/gamut-ffi-v0.1.1...gamut-ffi-v0.1.2) - 2026-07-18

### Added

- *(ffi)* mirror the gamut feature table with strict forwarding

### Other

- *(ffi)* specify the C API design contract for issue 242

## [0.1.1](https://github.com/justin13888/gamut/compare/gamut-ffi-v0.1.0...gamut-ffi-v0.1.1) - 2026-06-12

### Other

- Merge pull request #20 from justin13888/docs/crate-readmes
- add structurally consistent README to every crate
