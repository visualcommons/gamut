# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.3](https://github.com/visualcommons/gamut/compare/gamut-heic-v0.2.2...gamut-heic-v0.2.3) - 2026-08-14

### Added

- *(gamut-heic)* split annex-b export for platform decoders
- *(core)* add structured error diagnostics
- *(gamut-isobmff)* parse large and uuid boxes

### Fixed

- *(gamut-heic)* prefer nclx for rgba presentation
- *(gamut-heic)* correct imir axis semantics

## [0.2.2](https://github.com/justin13888/gamut/compare/gamut-heic-v0.2.1...gamut-heic-v0.2.2) - 2026-07-21

### Other

- updated the following local packages: gamut-codec-abi

## [0.2.1](https://github.com/justin13888/gamut/compare/gamut-heic-v0.2.0...gamut-heic-v0.2.1) - 2026-07-18

### Added

- *(heic)* pluggable HevcDecoder trait and full container decode pipeline
- *(heic)* typed hvcC record, NAL unit layer, and decoder-facing bridges
- *(heic)* full-fidelity HEIF container parse with byte-exact accounting

### Other

- *(heic)* close every diff-scoped mutation gap; refactor precondition-masked paths
- *(heic)* libheif differential conformance suite over generated fixtures

## [0.2.0](https://github.com/justin13888/gamut/compare/gamut-heic-v0.1.0...gamut-heic-v0.2.0) - 2026-06-12

### Other

- *(core)* [**breaking**] remove the legacy Encoder/Decoder traits
- clarify image-first crate boundaries
- Merge pull request #20 from justin13888/docs/crate-readmes
- add structurally consistent README to every crate
