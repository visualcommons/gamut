# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.3.0](https://github.com/visualcommons/gamut/compare/gamut-heic-v0.2.2...gamut-heic-v0.3.0) - 2026-09-10

### Added

- *(heic)* locate the C2PA manifest store and report its byte range
- *(gamut-heic)* add the high-bit-depth RGBA16 presentation surface
- *(core)* add structured error diagnostics
- *(gamut-isobmff)* parse large and uuid boxes

### Fixed

- *(heic)* locate an update manifest store past the merkle offset
- *(gamut-heic)* prefer nclx for rgba presentation
- *(gamut-heic)* correct imir axis semantics

### Other

- *(isobmff)* drop imports the moved tests no longer use
- *(isobmff)* share the segment walk with avif and heic
- *(heic)* separate per-item adaptation from backend ownership
- *(heic)* correct the JUMBF traceability claim and the probe's reach
- *(heic)* state the update probe's real strength, not a fail-safe one
- *(heic)* correct the JUMBF citation and narrow the scan-scope claim
- *(heic)* pin that only a `uuid` box carries a manifest store
- *(heic)* keep the umbrella-feature note in the usage section
- *(heic)* record the C2PA manifest-store locator in the ledger
- *(heic)* pin the C2PA locator's offsets and rejection branches
- derive chroma plane sizes from one shared rule
- adopt as_chunks for constant-size slice chunking
- Merge pull request #360 from visualcommons/feat/303-high-bit-depth-presentation
- pin the blend rounding and the high-bit-depth paths mutation testing missed
- record the high-bit-depth presentation surface in STATUS and READMEs
- *(gamut-heic)* make the RGBA pipeline generic over sample width

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
