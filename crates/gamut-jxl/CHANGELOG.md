# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.4.1](https://github.com/visualcommons/gamut/compare/gamut-jxl-v0.4.0...gamut-jxl-v0.4.1) - 2026-08-14

### Added

- *(core)* add structured error diagnostics

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
