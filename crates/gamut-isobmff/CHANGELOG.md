# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- *(isobmff)* own the C2PA `ContentProvenanceBox` framing shared by gamut-heic and gamut-avif
  (#505): `C2PA_UUID`, `C2paBoxPurpose`, `C2paUnreadReason`, the `C2paUuidBox::classify` lens over a
  top-level `Segment` reporting a box-bounded `C2paSlot` (with `merkle_offset` and the
  `LBox`-trimmed `store()` and its file range `store_range()`), and the
  `content_provenance_payload` / `content_provenance_reserved` builders.
- *(isobmff)* typed `mdcv` mastering-display property (`PropertyKind::MasteringDisplayColourVolume`):
  strict 24-byte read, byte-exact write. A file whose `mdcv` body is not exactly 24 bytes, which
  was previously carried verbatim as `PropertyKind::Other`, is now rejected as invalid input.

## [2.0.1](https://github.com/justin13888/gamut/compare/gamut-isobmff-v2.0.0...gamut-isobmff-v2.0.1) - 2026-07-20

### Other

- updated the following local packages: gamut-core

## [2.0.0](https://github.com/justin13888/gamut/compare/gamut-isobmff-v1.0.0...gamut-isobmff-v2.0.0) - 2026-07-18

### Added

- *(isobmff)* iovl overlay payload, exported box-walk primitives, motion-photo-tolerant read
- *(isobmff)* typed ImageGrid derived-image payload helper
- *(isobmff)* [**breaking**] finalise the v1 still-image container surface

### Fixed

- *(avif)* follow the 2022 imir axis semantics on both surfaces

### Other

- *(heic)* close every diff-scoped mutation gap; refactor precondition-masked paths
- *(isobmff)* document the CLI container entrypoint
- *(isobmff)* drop the equivalent-mutant index_size branch
- *(isobmff)* disambiguate the write() intra-doc link
- *(isobmff)* structural count guards and indexed id joins
- *(isobmff)* ledger the v1 surface — deferred vs likely-out-of-scope
- *(isobmff)* pin the reader against hand-authored spec fixtures

### Added

- A typed box-tree model (`IsoBmffImage`/`Item`/`Property`/`PropertyKind`/`ColourInformation`/
  `NclxColr`) and a `read` parser, making this a codec-agnostic ISOBMFF still-image container with
  symmetric `read`/`write` (`read(&write(&img)) == img`) that AVIF and HEIC can share. Codec
  configuration (`av1C`/`hvcC`) is carried opaquely; unrecognised property boxes round-trip verbatim.

### Changed

- **BREAKING:** the AVIF-specific `write_avif_still`/`Av1cConfig`/`AvifStillImage`/`ImageTransform`
  API is replaced by `write` over `IsoBmffImage`. Construct the model and call `write`.

### Removed

- The unused `gamut-bitstream` dependency (ISOBMFF boxes are byte-aligned big-endian).

## [0.3.0](https://github.com/justin13888/gamut/compare/gamut-isobmff-v0.2.0...gamut-isobmff-v0.3.0) - 2026-06-12

### Added

- *(avif)* irot/imir display-orientation transforms

### Other

- Merge pull request #20 from justin13888/docs/crate-readmes
- add structurally consistent README to every crate
