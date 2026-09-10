# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [2.0.0](https://github.com/visualcommons/gamut/compare/gamut-dng-v1.0.0...gamut-dng-v2.0.0) - 2026-09-10

### Added

- *(dng)* decode the untyped camera-profile colour tags
- *(dng)* type AsShotWhiteXY and derive its camera neutral per DNG 1.7.1 §6
- *(dng)* [**breaking**] decode what real cameras actually write
- *(core)* add structured error diagnostics

### Fixed

- *(tiff)* reject non-progressing CCITT runs

### Other

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
- Merge pull request #392 from visualcommons/feat/349-dng-asshotwhitexy
- adopt as_chunks for constant-size slice chunking
- Merge pull request #348 from visualcommons/feat/174-dng-real-corpus
- *(dng)* record the real-camera tier and the v2 breaks

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
