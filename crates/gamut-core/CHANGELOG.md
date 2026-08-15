# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [2.1.0](https://github.com/visualcommons/gamut/compare/gamut-core-v2.0.1...gamut-core-v2.1.0) - 2026-08-15

### Added

- *(core)* add format-agnostic pixel conversion
- *(core)* add full-range sample widening and narrowing
- *(core)* add fixed-point luma weights to luminance
- *(core)* add structured error diagnostics
- add optional serde support to public enums

### Other

- close the mutation-testing gaps in the conversion paths
- *(core)* drive convert through the public API

## [2.0.1](https://github.com/justin13888/gamut/compare/gamut-core-v2.0.0...gamut-core-v2.0.1) - 2026-07-20

### Other

- *(gamut-core)* lock PixelFormat and ColorModel discriminants at compile time

## [2.0.0](https://github.com/justin13888/gamut/compare/gamut-core-v1.0.0...gamut-core-v2.0.0) - 2026-07-18

### Added

- *(core)* pin C-compatible layouts for Dimensions and ColorModel
- *(core)* add PixelFormat runtime tag mirroring the sealed Pixel matrix
- *(core)* add Error::Io variant for stream-backed sources

## [0.2.0](https://github.com/justin13888/gamut/compare/gamut-core-v0.1.0...gamut-core-v0.2.0) - 2026-06-12

### Added

- *(core)* add EncodeImage/DecodeImage traits alongside Encoder/Decoder
- *(core)* add ImageRef/ImageBuf branded image buffers
- *(core)* add Pixel/Sample/ColorModel pixel vocabulary
- *(core)* add validated Dimensions constructor and area helpers

### Other

- *(core)* [**breaking**] remove the legacy Encoder/Decoder traits
- Merge pull request #20 from justin13888/docs/crate-readmes
- add structurally consistent README to every crate
