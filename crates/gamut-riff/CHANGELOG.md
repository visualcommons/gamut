# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [1.0.0](https://github.com/visualcommons/gamut/compare/gamut-riff-v0.1.3...gamut-riff-v1.0.0) - 2026-08-22

### Added

- *(riff)* [**breaking**] enforce reconstruction-chunk order and carry unknown chunks
- *(riff)* [**breaking**] validate the spec's size and canvas bounds
- *(core)* add structured error diagnostics

### Other

- *(riff)* release v1.0.0
- *(riff)* ledger the v1 surface and correct the RFC citations
- *(riff)* spec fixtures, robustness sweep, and a libwebp demux oracle
- *(riff)* [**breaking**] narrow the frozen public surface
- *(riff)* drop the unused gamut-bitstream dependency

## [0.1.3](https://github.com/visualcommons/gamut/compare/gamut-riff-v0.1.2...gamut-riff-v0.1.3) - 2026-07-30

### Added

- *(gamut-riff)* carry ICCP, EXIF, and XMP chunks through the WebP container

## [0.1.2](https://github.com/justin13888/gamut/compare/gamut-riff-v0.1.1...gamut-riff-v0.1.2) - 2026-07-18

### Other

- Merge pull request #151 from justin13888/feat/benchmarks
- *(riff)* close mutation-testing gaps

## [0.1.1](https://github.com/justin13888/gamut/compare/gamut-riff-v0.1.0...gamut-riff-v0.1.1) - 2026-06-12

### Added

- *(webp)* VP8X extended container header
- *(riff)* implement RIFF container reader/writer for WebP

### Other

- *(webp)* document scope decisions for non-core features
- *(webp)* add two-part implementation STATUS.md and refresh READMEs
- Merge pull request #20 from justin13888/docs/crate-readmes
- add structurally consistent README to every crate
