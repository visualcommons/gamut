# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [2.1.0](https://github.com/visualcommons/gamut/compare/gamut-ifd-v2.0.1...gamut-ifd-v2.1.0) - 2026-08-14

### Added

- *(core)* add structured error diagnostics
- *(gamut-ifd)* add f64 value coercion

### Fixed

- *(tiff)* reject non-progressing CCITT runs

## [2.0.1](https://github.com/justin13888/gamut/compare/gamut-ifd-v2.0.0...gamut-ifd-v2.0.1) - 2026-07-20

### Other

- updated the following local packages: gamut-core

## [2.0.0](https://github.com/justin13888/gamut/compare/gamut-ifd-v1.0.0...gamut-ifd-v2.0.0) - 2026-07-18

### Added

- *(dng)* preserving rewrite carrying everything, with maker-note pinning
- *(ifd)* composable Auditor with embedded rebased-stream walks
- [**breaking**] rebuild the TIFF and DNG deconstructs on the ifd segment auditor
- *(ifd)* shared structural byte-completeness auditor
- *(ifd)* writer-declared segment map with zero-fill padding and pinned spans
- *(ifd)* tracked read ledger and typed segment map for dual-ledger accounting
- *(ifd)* [**breaking**] preserve unknown field types losslessly as Value::Unknown
- *(ifd)* add unclamped u64 value and Ifd accessors
- *(ifd)* well-known structural pointer-tag constants
- *(ifd)* lazy IfdReader with raw directory entries over any ReadAt source
- *(ifd)* ReadAt source trait with slice, stream, and rebased adapters

### Other

- Merge remote-tracking branch 'origin/master' into chore/263-byte-completeness
- *(ifd)* close out the mutation survivors
- *(ifd)* mutation-harden the segment engine and auditor
- *(ifd)* byte-completeness ledgers and issue #263 status
- *(ifd)* byte-completeness fidelity matrix
- *(ifd)* [**breaking**] remove the superseded Coverage engine
- *(ifd)* [**breaking**] collapse the slice reader onto the streaming engine
- *(ifd)* kill the two diff-scoped mutation survivors
- *(ifd)* STATUS/README for the streaming source layer
- *(ifd)* streaming-vs-slice differential over the robustness corpus and fixtures
- *(ifd)* share pointer-resolution and chain guards for a second reader

## [0.1.1](https://github.com/justin13888/gamut/compare/gamut-ifd-v0.1.0...gamut-ifd-v0.1.1) - 2026-06-12

### Other

- updated the following local packages: gamut-core
