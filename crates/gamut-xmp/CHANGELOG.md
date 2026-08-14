# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [1.1.0](https://github.com/visualcommons/gamut/compare/gamut-xmp-v1.0.0...gamut-xmp-v1.1.0) - 2026-08-14

### Added

- *(core)* add structured error diagnostics

## [1.0.0](https://github.com/justin13888/gamut/compare/gamut-xmp-v0.1.1...gamut-xmp-v1.0.0) - 2026-07-18

### Added

- *(xmp)* round out the model with array and removal conveniences
- *(xmp)* [**breaking**] register writer namespace prefixes and finish the namespace surface
- *(xmp)* parse an XmpPacket into the property graph
- *(xmp)* canonical RDF/XML serializer and packet emission
- *(xmp)* cover the remaining standard schemas
- *(xmp)* [**breaking**] parse XMP packets into the property graph
- *(xmp)* [**breaking**] model the URI value and per-item qualifiers, add error type

### Fixed

- *(xmp)* escape control characters and match the trailer end attribute

### Other

- *(xmp)* release v1.0.0
- *(xmp)* exit the reader loop with an explicit EOF check
- *(xmp)* reserve ns<digits> prefixes so synthesis cannot collide
- *(xmp)* document the v1 surface, scope, and intentional skips
- *(xmp)* pin Part 1-3 conformance edge cases against spec and oracle
- *(xmp)* cross-check the serializer against exiv2's Adobe XMPCore
- *(xmp)* add the exiv2 XMP conformance oracle
- *(xmp)* eliminate surviving mutants in the parser and serializer
- *(xmp)* cover reader edge cases and prohibited forms
- *(xmp)* document the v1 public API and settled decisions
- *(xmp)* wrap long lines to satisfy rustfmt
- *(deps)* add quick-xml and thiserror to gamut-xmp

## [0.1.1](https://github.com/justin13888/gamut/compare/gamut-xmp-v0.1.0...gamut-xmp-v0.1.1) - 2026-06-12

### Other

- updated the following local packages: gamut-core
