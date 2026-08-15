# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [1.1.0](https://github.com/visualcommons/gamut/compare/gamut-tiff-v1.0.0...gamut-tiff-v1.1.0) - 2026-08-15

### Added

- *(tiff)* add the TiffInfo pre-decode probe
- *(tiff)* encode 16-bit grayscale, RGB and RGBA
- *(tiff)* decode 16-bit grayscale, RGB, RGBA and CMYK samples
- *(tiff)* add the SampleFormat tag type and reject non-integer samples
- *(core)* add structured error diagnostics
- *(gamut-tiff)* support deflate compression

### Fixed

- *(tiff)* reject non-progressing CCITT runs

### Other

- *(tiff)* pin the 16-bit CMYK narrowing to its depth policy
- merge origin/master into feat/268-pixel-conversion
- *(tiff)* pin which guard rejects a mismatched sample count
- *(tiff)* record 16-bit sample support in the scope ledger
- *(tiff)* extract page-header parsing from the decode funnel
- *(tiff)* scope the decode-size guard to a named helper

## [1.0.0](https://github.com/justin13888/gamut/compare/gamut-tiff-v0.2.0...gamut-tiff-v1.0.0) - 2026-07-18

### Added

- [**breaking**] rebuild the TIFF and DNG deconstructs on the ifd segment auditor
- *(ifd)* [**breaking**] preserve unknown field types losslessly as Value::Unknown
- *(tiff)* [**breaking**] replace the code accessors with TryFrom and From conversions
- *(tiff)* add strict deconstruct mode with full-file accounting

### Other

- *(ifd)* byte-completeness ledgers and issue #263 status
- *(tiff)* release v1.0.0
- *(tiff)* document the v1 surface and scope ledger
- *(tiff)* [**breaking**] finalize the v1 surface
- *(tiff)* drop the dormant gamut-color and gamut-dsp dependencies
- *(tiff/dng/exif)* source pointer-tag constants from gamut-ifd
- Merge pull request #235 from justin13888/feat/181-ifd-v1
- *(tiff)* correct the gamut-dsp attribution in the crate docs
- apply nightly rustfmt import grouping across the workspace
- *(mise)* port justfile recipes to mise tasks
- Merge pull request #151 from justin13888/feat/benchmarks
- *(tiff)* close mutation-testing gaps

## [0.2.0](https://github.com/justin13888/gamut/compare/gamut-tiff-v0.1.0...gamut-tiff-v0.2.0) - 2026-06-12

### Added

- *(tiff)* [**breaking**] migrate to typed EncodeImage/DecodeImage, drop weakly-typed methods

### Other

- *(core)* [**breaking**] remove the legacy Encoder/Decoder traits
- *(tiff)* type the palette colour table as Palette8
