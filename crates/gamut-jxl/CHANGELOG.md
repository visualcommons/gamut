# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.5.0](https://github.com/visualcommons/gamut/compare/gamut-jxl-v0.4.0...gamut-jxl-v0.5.0) - 2026-10-10

### Added

- *(jxl)* read metadata boxes back and wire the gamut-metadata facade
- *(jxl)* [**breaking**] require an explicit policy for lossy presentation
- *(jxl)* decode truncated codestreams best-effort behind DecodePartialImage
- *(jxl)* expose modular-mode control on the encoder
- *(core)* add structured error diagnostics

### Fixed

- *(jxl)* restore the crate-wide deny(unsafe_code) and its safety section

### Other

- Merge remote-tracking branch 'origin/master' into feat/420-metadata-facade-wiring
- *(jxl)* drop the chunks_exact_to_as_chunks expectation clippy no longer needs
- *(jxl)* pin with_metadata's serialize-failure path
- *(jxl)* document that a model's ICC replaces the encoder's colour spec
- *(jxl)* pin that an empty container box is walked over, not rejected
- *(jxl)* bound the container box walk's step inside the walk
- *(jxl)* reformat the metadata wiring with the workspace rustfmt
- *(jxl)* separate registry sharing from equality semantics
- adopt as_chunks for constant-size slice chunking
- merge origin/master into feat/268-pixel-conversion
- *(jxl)* pin that colour-as-grayscale is refused before the decode
- close the mutation-testing gaps in the conversion paths
- merge origin/master into feat/268-pixel-conversion
- *(jxl)* record partial decode and answer the encoder-knob question
- *(jxl)* sweep 16-bit-plus-alpha across the encoder-configuration grid
- *(jxl)* sweep truncated streams through the partial decode path
- *(jxl)* fold the raw-frame to ImageBuf tail into one helper
- *(jxl)* rejoin the orphaned decode-side clause in the README
- *(jxl)* record modular-mode control in STATUS and README

## [0.4.0](https://github.com/justin13888/gamut/compare/gamut-jxl-v0.3.0...gamut-jxl-v0.4.0) - 2026-07-20

### Added

- *(jxl)* [**breaking**] make libjxl/jxl-rs pushable backend tails behind push_backend

### Other

- *(jxl)* pin the no-backend refusal messages on every build
- *(jxl)* restate encode/decode feature semantics + record deferred container ownership

## [0.3.0](https://github.com/justin13888/gamut/compare/gamut-jxl-v0.2.0...gamut-jxl-v0.3.0) - 2026-07-18

### Added

- *(jxl)* coded-bit-depth decode and encode, and a header info peek
- *(jxl)* enable the encoder on wasm32-unknown-emscripten
- *(jxl)* Exif and XMP container boxes
- *(jxl)* encoder orientation signalling
- *(jxl)* surface the embedded ICC profile from the decoder
- *(jxl)* [**breaking**] typed color encoding (ICC, linear sRGB, PQ, HLG)
- *(jxl)* implement JPEG bitstream recompression (jbrd)
- *(jxl)* pure-Rust decoder wrapping jxl-rs with DecodeImage impls
- *(jxl)* libjxl-backed encoder with typed lossless/lossy, effort, and container options

### Other

- *(jxl)* retire the timeout-caught mutants
- *(jxl)* move shipped features off the deferred ledger; state the wasm boundary
- *(jxl)* rewrite README for the wrap architecture; add STATUS.md ledger and oracle pin
- *(jxl)* decoder robustness corpus and feature-grid differential matrix

## [0.2.0](https://github.com/justin13888/gamut/compare/gamut-jxl-v0.1.0...gamut-jxl-v0.2.0) - 2026-06-12

### Other

- *(core)* [**breaking**] remove the legacy Encoder/Decoder traits
- Merge pull request #20 from justin13888/docs/crate-readmes
- add structurally consistent README to every crate
