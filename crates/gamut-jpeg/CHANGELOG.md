# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.1](https://github.com/visualcommons/gamut/compare/gamut-jpeg-v0.1.0...gamut-jpeg-v0.1.1) - 2026-08-05

### Other

- updated the following local packages: gamut-core, gamut-color

## [0.1.0](https://github.com/justin13888/gamut/releases/tag/gamut-jpeg-v0.1.0) - 2026-07-18

### Added

- *(jpeg)* reuse the destination buffer in decode_image_into
- *(jpeg)* embed EXIF/XMP/ICC metadata via encoder builder methods
- *(jpeg)* read APP1/APP2 metadata (EXIF, XMP, multi-segment ICC) via metadata()
- *(jpeg)* close out issue #28 hardening (P6)
- *(jpeg)* add progressive DCT encoding (SOF2)
- *(jpeg)* add progressive DCT decoding (SOF2)
- *(jpeg)* add sequential DCT Huffman decoder (SOF0/SOF1)
- *(jpeg)* add gamut-jpeg with baseline sequential DCT encoder

### Other

- *(jpeg)* kill the surviving P7 metadata boundary mutants
- *(jpeg)* record the APPn metadata phase (P7) in STATUS.md and README
- *(jpeg)* prove APP1/APP2 metadata interop against libjpeg-turbo and the facade
- *(jpeg)* add libjpeg-turbo differential oracle gate
