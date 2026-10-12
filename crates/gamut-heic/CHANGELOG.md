# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.3.0](https://github.com/visualcommons/gamut/compare/gamut-heic-v0.2.2...gamut-heic-v0.3.0) - 2026-10-12

### Added

- *(gamut-isobmff, gamut-heic, gamut-avif)* [**breaking**] C2PA ranges are a C2paByteRange ([#812](https://github.com/visualcommons/gamut/pull/812))
- *(isobmff)* skip a sequence moov beside meta instead of refusing the file ([#712](https://github.com/visualcommons/gamut/pull/712))
- *(isobmff)* typed mdcv mastering-display property ([#688](https://github.com/visualcommons/gamut/pull/688))
- *(core)* shared Segment<K> and covers byte-tiling primitive ([#691](https://github.com/visualcommons/gamut/pull/691))
- *(heic)* wire the gamut-metadata facade behind a `metadata` feature
- *(heic)* report every top-level uuid box a C2PA scan sees
- *(heic)* report a C2PA box that yielded no manifest store
- *(heic)* summarize a located C2PA manifest store without its bytes
- *(heic)* locate the C2PA manifest store and report its byte range
- *(gamut-heic)* add the high-bit-depth RGBA16 presentation surface
- *(gamut-heic)* split annex-b export for platform decoders
- *(core)* add structured error diagnostics
- *(gamut-isobmff)* parse large and uuid boxes

### Fixed

- *(heic)* report a merkle box as auxiliary C2PA framing, not as unread
- *(heic)* refuse a content-encoded XMP item as Unsupported in blocks()
- *(heic)* state every non-empty class in the C2PA headline, in file order
- *(heic)* locate an update manifest store past the merkle offset
- *(gamut-heic)* prefer nclx for rgba presentation
- *(gamut-heic)* correct imir axis semantics

### Other

- *(isobmff)* share the C2PA uuid lens between gamut-heic and gamut-avif ([#781](https://github.com/visualcommons/gamut/pull/781))
- *(isobmff)* [**breaking**] own the clap, pasp, clli and mdcv value structs ([#771](https://github.com/visualcommons/gamut/pull/771))
- *(heic)* [**breaking**] replace C2paSummary::is_present with a C2paPresence answer ([#742](https://github.com/visualcommons/gamut/pull/742))
- gate rustdoc warnings and compile README code blocks as doctests ([#713](https://github.com/visualcommons/gamut/pull/713))
- Merge remote-tracking branch 'origin/master' into feat/448-cli-inspect-c2pa
- *(heic)* build the test image through IsoBmffImage::new
- Merge remote-tracking branch 'origin/feat/224-png-encoder-efficiency' into feat/448-cli-inspect-c2pa
- Merge remote-tracking branch 'origin/feat/449-xmp-dcterms-provenance' into feat/420-metadata-facade-wiring
- *(heic)* bound the C2PA detail list by the summary, not by its merge
- *(heic)* bind the no-byte-leak guard to a buffer that is not static
- *(heic)* record what the C2PA report states beside its stores
- *(heic)* record the unread-C2PA-box outcome in STATUS
- *(heic)* record the C2PA reporting shape in STATUS
- *(heic)* rustfmt the byte-leak assertion in the c2pa summary test
- *(heic)* build IsoBmffImage through its constructor
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
