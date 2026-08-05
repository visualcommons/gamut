# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [1.1.0](https://github.com/visualcommons/gamut/compare/gamut-exif-v1.0.0...gamut-exif-v1.1.0) - 2026-08-05

### Added

- *(gamut-exif)* convert gps metadata to geocoordinates

## [1.0.0](https://github.com/justin13888/gamut/compare/gamut-exif-v0.1.1...gamut-exif-v1.0.0) - 2026-07-18

### Added

- *(exif)* [**breaking**] pin the maker note at its source offset on rewrite
- *(ifd)* [**breaking**] make write fallible over classic-width overflow
- *(gamut-exif)* MakerNote passthrough and vendor detection
- *(gamut-exif)* thumbnail extract and JPEG re-embed
- *(gamut-exif)* GPS typed model
- *(gamut-exif)* writer round-trip keystone
- *(gamut-exif)* reader — marker detection and IFD traversal
- *(gamut-exif)* Exif 3.0 tag catalogue

### Fixed

- *(gamut-exif)* keep the structural thumbnail JPEG offset out of the model

### Other

- *(ifd)* mutation-harden the segment engine and auditor
- *(ifd)* byte-completeness ledgers and issue #263 status
- *(tiff/dng/exif)* source pointer-tag constants from gamut-ifd
- *(exif)* use gamut-ifd's Ifd::remove and align_word
- *(gamut-exif)* release v1.0.0
- *(gamut-exif)* exiv2 EXIF oracle and golden fixtures
- *(gamut-exif)* v1 scaffolding — error, value helpers, Exif model
- *(mise)* port justfile recipes to mise tasks

## [0.1.1](https://github.com/justin13888/gamut/compare/gamut-exif-v0.1.0...gamut-exif-v0.1.1) - 2026-06-12

### Other

- updated the following local packages: gamut-core, gamut-ifd
