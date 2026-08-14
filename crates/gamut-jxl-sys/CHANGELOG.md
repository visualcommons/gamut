# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.1](https://github.com/visualcommons/gamut/compare/gamut-jxl-sys-v0.1.0...gamut-jxl-sys-v0.1.1) - 2026-08-14

### Added

- *(jxl-sys)* declare the MODULAR frame-setting id

## [0.1.0](https://github.com/justin13888/gamut/releases/tag/gamut-jxl-sys-v0.1.0) - 2026-07-18

### Added

- *(dng-oracle)* link real libjxl via gamut-jxl-sys; digest and sample-file entry points
- *(jxl)* coded-bit-depth decode and encode, and a header info peek
- *(jxl-sys)* build libjxl for wasm32-unknown-emscripten
- *(jxl-sys)* declare the jbrd, box, and color-oracle FFI surface
- *(jxl-sys)* add gamut-jxl-sys building libjxl 0.12.0 via jpegxl-src

### Other

- fix the wasm JXL lane — recent emsdk pin and growable heap
