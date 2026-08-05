# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [1.0.1](https://github.com/visualcommons/gamut/compare/gamut-metadata-v1.0.0...gamut-metadata-v1.0.1) - 2026-08-05

### Other

- updated the following local packages: gamut-exif

## [1.0.0](https://github.com/justin13888/gamut/compare/gamut-metadata-v0.1.1...gamut-metadata-v1.0.0) - 2026-07-18

### Added

- *(metadata)* implement MetadataEmbedder + EncodedMetadata (P3)
- *(metadata)* implement MetadataExtractor (P2) with IPTC->XMP reconciliation (P4 read)
- *(metadata)* add MetadataError and Result

### Fixed

- *(metadata)* transpose the now-fallible EXIF serialization in embed

### Other

- *(metadata)* [**breaking**] forward carrier-native ICC/IPTC errors
- *(gamut-metadata)* release v1.0.0
- *(metadata)* apply workspace nightly rustfmt
- *(metadata)* document the v1 surface; remove STATUS.md
- *(metadata)* round-trip equality keystone + unit coverage
- *(metadata)* [**breaking**] make xmp the sole IPTC home; drop duplicate iptc field
- apply nightly rustfmt import grouping across the workspace
- *(mise)* port justfile recipes to mise tasks

## [0.1.1](https://github.com/justin13888/gamut/compare/gamut-metadata-v0.1.0...gamut-metadata-v0.1.1) - 2026-06-12

### Other

- updated the following local packages: gamut-core, gamut-exif, gamut-icc, gamut-xmp, gamut-iptc
