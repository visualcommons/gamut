# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.3.2](https://github.com/visualcommons/gamut/compare/gamut-cli-v0.3.1...gamut-cli-v0.3.2) - 2026-08-15

### Added

- *(cli)* add --jxl-modular to gamut convert

## [0.3.1](https://github.com/visualcommons/gamut/compare/gamut-cli-v0.3.0...gamut-cli-v0.3.1) - 2026-07-30

### Other

- update Cargo.lock dependencies

## [0.3.0](https://github.com/justin13888/gamut/compare/gamut-cli-v0.2.0...gamut-cli-v0.3.0) - 2026-07-18

### Added

- [**breaking**] rebuild the TIFF and DNG deconstructs on the ifd segment auditor
- *(cli)* list BitDepth::Sixteen in color list
- *(cli)* JPEG XL output format and .jxl input decoding
- *(cli)* add the gamut isobmff container entrypoint
- *(cli)* add `gamut icc` to extract and inspect embedded ICC profiles
- *(cli)* add `gamut inspect` for strict TIFF/DNG deconstruct
- *(cli)* show resolved gamut library version in gamut -V

### Fixed

- *(cli)* migrate convert to the AvifEncoder lossless/lossy API

### Other

- *(tiff)* [**breaking**] finalize the v1 surface
- Merge pull request #266 from justin13888/feat/260-color-bit-depth
- Merge branch 'master' into feat/184-isobmff-v1
- *(icc)* [**breaking**] move profile-ID computation onto ProfileId and add Display impls
- *(icc)* [**breaking**] adopt std conversion traits for signatures and header enums
- apply nightly rustfmt import grouping across the workspace
- Merge branch 'master' into feat/png

## [0.2.0](https://github.com/justin13888/gamut/compare/gamut-cli-v0.1.0...gamut-cli-v0.2.0) - 2026-06-12

### Added

- [**breaking**] migrate AVIF and WebP to typed EncodeImage/DecodeImage, drop weakly-typed methods
- *(tiff)* [**breaking**] migrate to typed EncodeImage/DecodeImage, drop weakly-typed methods
- *(cli)* add TIFF output to gamut convert
- *(cli)* decode WebP input via gamut's own decoder
- *(cli)* lossy + alpha WebP in `gamut convert`; finalize docs
- *(cli)* recognize WebP output in gamut convert
- *(cli)* add build timestamp to gamut -V
- *(cli)* report build provenance in gamut -V
- implement gamut-cli sandbox and primitives feature

### Other

- *(color)* [**breaking**] delete the unused PixelFormat enum, document BitDepth/ChromaSubsampling
- Merge pull request #44 from justin13888/feat/webp
- *(webp)* mark VP8L lossless (M0/M1) implemented across STATUS, docs, and READMEs
- Merge pull request #20 from justin13888/docs/crate-readmes
- add structurally consistent README to every crate
