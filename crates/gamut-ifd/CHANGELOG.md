# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [3.1.0](https://github.com/visualcommons/gamut/compare/gamut-ifd-v3.0.0...gamut-ifd-v3.1.0) - 2026-10-24

### Added

- *(gamut-ifd)* shared tag registry for TIFF 6.0 and DNG tags ([#892](https://github.com/visualcommons/gamut/pull/892))

## [3.0.0](https://github.com/visualcommons/gamut/compare/gamut-ifd-v2.0.1...gamut-ifd-v3.0.0) - 2026-10-12

### Added

- *(exif)* deepen the read report with per-entry recovery, a byte audit and a real-DNG laziness case ([#770](https://github.com/visualcommons/gamut/pull/770))
- *(ifd)* report shadowed duplicate-tag entries ([#724](https://github.com/visualcommons/gamut/pull/724))
- *(cli)* [**breaking**] report the C2PA manifest store in gamut inspect for AVIF, WebP, TIFF and DNG ([#718](https://github.com/visualcommons/gamut/pull/718))
- *(ifd)* state the C2PA manifest-store placement and exclusion rules
- *(ifd)* reserve a leading vendor preamble in the writer's layout
- *(ifd)* [**breaking**] name the bytes a file does not account for
- *(core)* add structured error diagnostics
- *(gamut-ifd)* add f64 value coercion

### Fixed

- *(gamut-ifd)* refuse a recognised type code in UnknownValue::new ([#809](https://github.com/visualcommons/gamut/pull/809))
- *(ifd)* bound every sub-IFD walk by one whole-walk directory budget ([#792](https://github.com/visualcommons/gamut/pull/792))
- *(gamut-ifd)* track the audit's visited sub-IFDs in a set, not a scanned list ([#757](https://github.com/visualcommons/gamut/pull/757))
- *(gamut-ifd)* keep a tag in one store across set and set_sub_ifd ([#678](https://github.com/visualcommons/gamut/pull/678))
- *(ifd)* validate the exclusion set locate builds from a file
- *(ifd)* validate a hand-built C2PA exclusion set
- *(ifd)* name a duplicated C2PA entry and let a host build an exclusion set
- *(ifd)* keep a C2PA store out of line and reject an ambiguous entry
- *(ifd)* let the test suite compile under default features
- *(mutants)* retire an exclusion that was a coverage gap, not an equivalence
- *(ifd)* declare the preamble the way an audit reads it
- *(tiff)* reject non-progressing CCITT runs

### Other

- give every fuzz-tier law its own failure-path test ([#745](https://github.com/visualcommons/gamut/pull/745))
- *(ifd)* compare reader-agreement parses by Debug rendering, not NaN-blind == ([#733](https://github.com/visualcommons/gamut/pull/733))
- gate rustdoc warnings and compile README code blocks as doctests ([#713](https://github.com/visualcommons/gamut/pull/713))
- *(tiff)* hoist the scoped sub-IFD resolver into gamut-ifd ([#683](https://github.com/visualcommons/gamut/pull/683))
- Merge remote-tracking branch 'origin/master' into feat/446-tiff-metadata-seam
- Merge remote-tracking branch 'origin/master' into feat/442-dng-c2pa-manifest-store
- Merge remote-tracking branch 'origin/feat/442-dng-c2pa-manifest-store' into feat/446-tiff-metadata-seam
- *(ifd)* format the exclusion constructor test
- Merge remote-tracking branch 'origin/master' into test/264-parser-fuzz-targets
- Merge remote-tracking branch 'origin/master' into feat/442-dng-c2pa-manifest-store
- *(ifd)* record that gamut-tiff now calls the shared C2PA placer and locator
- *(dng-ifd)* align C2PA docs with the BigTIFF refusal, the validating constructor and the trailing channel
- *(ifd)* say what a hand-advanced cursor's mutants actually do
- *(ifd)* bound the claim subtraction by the claims it walks
- *(ifd)* name the malformed-input case for the assertion it makes
- *(ifd)* confine the claim skip whose mutant cannot terminate
- *(ifd)* bound the audit's cursor walks by the data they walk
- *(ifd)* reject a 65 535-entry directory against the source length
- *(ifd)* close the mutation survivors the corpus pass left
- *(ifd)* pin where the pool resumes after jumping a pin
- *(ifd)* cover the pinned-span placement rules
- *(ifd)* state the read ledger's laws as executable invariants
- adopt as_chunks for constant-size slice chunking

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
