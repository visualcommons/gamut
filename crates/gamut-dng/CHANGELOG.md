# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [2.0.0](https://github.com/visualcommons/gamut/compare/gamut-dng-v1.0.0...gamut-dng-v2.0.0) - 2026-10-10

### Added

- *(dng)* flag a libz the loader took from the build graph
- *(dng)* [**breaking**] type the C2PA manifest store and report both exclusion ranges
- *(dng)* [**breaking**] consume the metadata facade instead of restating it
- *(dng)* decode the untyped camera-profile colour tags
- *(dng)* keep a vendor preamble at its original offset through a rewrite
- *(dng)* type AsShotWhiteXY and derive its camera neutral per DNG 1.7.1 §6
- *(deflate)* span the optimal parse instead of disabling it above 1 MiB
- *(dng)* [**breaking**] decode what real cameras actually write
- *(core)* add structured error diagnostics

### Fixed

- *(dng)* bound a C2PA reservation before allocating it
- *(ifd)* validate the exclusion set locate builds from a file
- *(dng)* print which zlib the benchmark's Deflate arm measured
- *(dng)* print only measured numbers from the codec benchmark
- *(dng)* give the benchmark's blue plane blue's gain
- *(dng)* agree on absence for a duplicated store, and stop dropping a page
- *(dng)* recognise the C2PA store tag and refuse one that packs inline
- *(dng)* bound Deflate inflation to the expected chunk size
- *(tiff)* reject non-progressing CCITT runs

### Other

- Merge remote-tracking branch 'origin/master' into perf/163-dng-benchmark-harness
- *(dng-ifd)* align C2PA docs with the BigTIFF refusal, the validating constructor and the trailing channel
- *(dng)* name the in-memory oracle pin for the extent it checks
- *(dng)* say what makes the two candidate zlibs separable, not that they are
- *(dng)* say which two zlibs the version string cannot separate
- *(dng)* point the benchmark section at the two issues it opened
- *(dng)* name the inflate each Deflate arm actually runs
- *(dng)* call the export-path arm a bound at its own definition
- *(dng)* republish the benchmark's numbers with what they depend on
- *(dng)* pin the oracle's in-memory decode where a gate runs it
- *(dng)* describe the benchmark's counter rule where it is stated
- *(dng)* publish every decode row the harness measures
- *(dng)* interleave each codec pair into one benchmark
- *(dng)* price the oracle's lossless-JPEG export path
- *(dng)* record the two defects the codec benchmark found
- *(dng)* pad the benchmark's case column through the formatter
- *(dng)* apply rustfmt to the codec benchmark
- *(dng)* record the codec benchmark harness and its fairness
- *(dng)* benchmark encode and decode against the Adobe DNG SDK
- *(dng)* state the preservation contract's two residues where it is made
- *(dng)* pin the trailing-extra guard against a surfaced directory
- *(dng)* find the maximum black per plane across the repeat pattern
- *(dng)* pin the exclusive upper bound of the RATIONAL black-level grid
- *(dng)* assert the archival verdict can also be false
- *(dng)* reject a malformed BlackLevelDeltaV
- *(dng)* find a NoiseProfile that a writer left in IFD 0
- *(dng)* cover is_lossy, the LONG level type, and the repeat-dim guards
- *(dng)* pin both halves of the WhiteLevel guard and the repeat-dim count
- *(dng)* write MD5's F and G in their XOR forms
- *(dng)* reject a FLOAT tag that is present but empty
- *(dng)* reject a WhiteLevel count that is neither one nor per-plane
- *(dng)* name the two differential files for their oracles
- Merge pull request #411 from visualcommons/feat/353-dng-metadata-facade
- *(dng)* merge master into the colour-projection branch
- *(dng)* record the colour projection in the README and status ledger
- Merge remote-tracking branch 'origin/master' into feat/350-dng-preserve-offsets
- Merge pull request #392 from visualcommons/feat/349-dng-asshotwhitexy
- adopt as_chunks for constant-size slice chunking
- Merge pull request #348 from visualcommons/feat/174-dng-real-corpus
- *(dng)* record the real-camera tier and the v2 breaks
- *(dng)* record the Deflate codec split and its measurements
- *(dng)* benchmark DNG Deflate against miniz_oxide
- *(dng)* encode Deflate strips with gamut-deflate

## [1.0.0](https://github.com/justin13888/gamut/releases/tag/gamut-dng-v1.0.0) - 2026-07-18

### Added

- *(dng)* preserving rewrite carrying everything, with maker-note pinning
- *(dng)* audit embedded camera-profile streams over the Adobe sample corpus
- [**breaking**] rebuild the TIFF and DNG deconstructs on the ifd segment auditor
- *(ifd)* [**breaking**] preserve unknown field types losslessly as Value::Unknown
- *(dng)* publish the lossless_jpeg module for external raw pipelines
- *(dng)* lossless-JPEG decode hardening to the full T.81 process-14 envelope
- *(dng)* typed OpcodeList container with parse, expose, and pass-through write
- *(dng)* RawImage::to_linear — the chapter-5 raw-to-linear mapping
- *(dng)* read and write the LinearizationTable tag
- *(dng)* [**breaking**] typed RawLevels model with the full BlackLevel family
- *(ifd)* [**breaking**] make write fallible over classic-width overflow
- *(dng)* add strict deconstruct mode with full-file accounting
- *(gamut-dng)* embed + decode EXIF/XMP/IPTC/ICC metadata
- *(gamut-dng)* lossless JPEG (SOF3) encode + decode
- *(gamut-dng)* Deflate/ZIP compression (encode + decode)
- *(gamut-dng)* BigTIFF (64-bit) DNG support
- *(gamut-dng)* full DNG decoder
- *(gamut-dng)* bit-depth packing (8/10/12/14/16) + default crop
- *(gamut-dng)* full colour-calibration profile
- *(gamut-dng)* encode LinearRaw (demosaiced) images
- *(gamut-dng)* encode uncompressed CFA DNG (keystone)
- *(gamut-dng)* add DNG tag and value tables
- *(gamut-dng)* scaffold DNG codec crate

### Other

- Merge remote-tracking branch 'origin/master' into chore/263-byte-completeness
- *(ifd)* mutation-harden the segment engine and auditor
- *(dng)* cover the deconstruct anomaly paths
- *(ifd)* byte-completeness ledgers and issue #263 status
- Merge pull request #271 from justin13888/feat/253-dng-api-refinement
- *(dng)* record the #253 bridge surface in STATUS, README, and crate docs
- *(dng)* differential lossless-JPEG suite against the SDK codec
- *(dng)* differential to_linear gate against the Adobe SDK stage-2 image
- *(dng)* use gamut-ifd's typed accessors and layout helpers
- apply nightly rustfmt import grouping across the workspace
- *(mise)* port justfile recipes to mise tasks
- *(gamut-dng)* use an odd width in the linear round-trip
- *(gamut-dng)* close remaining DNG codec mutation gaps
- *(gamut-dng)* close lossless-JPEG codec mutation gaps
- *(gamut-dng)* cover the 8-bit bitpack fast path
- *(gamut-dng)* clarify DNGVersion octets and Deflate codec choice
- *(gamut-dng)* reuse gamut-bitstream sample packing
- *(gamut-dng)* finalize STATUS, README, and workspace layout
- *(gamut-dng)* gate CFA DNG output on the Adobe SDK + libtiff
