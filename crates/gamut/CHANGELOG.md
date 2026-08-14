# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.3.3](https://github.com/visualcommons/gamut/compare/gamut-v0.3.2...gamut-v0.3.3) - 2026-08-14

### Other

- updated the following local packages: gamut-core, gamut-color, gamut-av1, gamut-isobmff, gamut-avif, gamut-ifd, gamut-jxl, gamut-dng, gamut-exif, gamut-heic, gamut-xmp, gamut-jpeg, gamut-png, gamut-tiff, gamut-tonemap, gamut-webp, gamut-bitstream, gamut-av2, gamut-icc, gamut-iptc, gamut-metadata, gamut-vvc

## [0.3.2](https://github.com/visualcommons/gamut/compare/gamut-v0.3.1...gamut-v0.3.2) - 2026-07-30

### Other

- *(gamut)* round-trip the metadata facade through a WebP file

## [0.3.1](https://github.com/justin13888/gamut/compare/gamut-v0.3.0...gamut-v0.3.1) - 2026-07-18

### Added

- *(dng)* JPEG XL raw decode and encode (Compression 52546)
- *(jpeg)* add gamut-jpeg with baseline sequential DCT encoder
- *(gamut)* expose gamut-isobmff via the isobmff feature
- *(cli)* show resolved gamut library version in gamut -V

### Other

- apply nightly rustfmt import grouping across the workspace
- Merge branch 'master' into feat/dng
- Merge branch 'master' into feat/png

## [0.3.0](https://github.com/justin13888/gamut/compare/gamut-v0.2.0...gamut-v0.3.0) - 2026-06-12

### Added

- *(tonemap)* add gamut-tonemap crate with tone-curve primitives

### Other

- *(core)* [**breaking**] remove the legacy Encoder/Decoder traits
- Merge branch 'feat/tiff' into master
- Merge branch 'master' into feat/tonemapping
- Merge pull request #20 from justin13888/docs/crate-readmes
- add structurally consistent README to every crate
