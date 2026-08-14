# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [1.1.0](https://github.com/visualcommons/gamut/compare/gamut-dng-v1.0.0...gamut-dng-v1.1.0) - 2026-08-14

### Added

- *(core)* add structured error diagnostics

### Fixed

- *(tiff)* reject non-progressing CCITT runs

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
