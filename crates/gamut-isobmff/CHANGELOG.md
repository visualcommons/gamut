# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [3.0.0](https://github.com/visualcommons/gamut/compare/gamut-isobmff-v2.0.1...gamut-isobmff-v3.0.0) - 2026-10-12

### Added

- *(gamut-isobmff, gamut-heic, gamut-avif)* [**breaking**] C2PA ranges are a C2paByteRange ([#812](https://github.com/visualcommons/gamut/pull/812))
- *(isobmff)* skip a sequence moov beside meta instead of refusing the file ([#712](https://github.com/visualcommons/gamut/pull/712))
- *(isobmff)* typed mdcv mastering-display property ([#688](https://github.com/visualcommons/gamut/pull/688))
- *(core)* shared Segment<K> and covers byte-tiling primitive ([#691](https://github.com/visualcommons/gamut/pull/691))
- *(isobmff)* [**breaking**] model and write top-level boxes
- *(isobmff)* add an IsoBmffImage constructor and builders
- *(core)* add structured error diagnostics
- *(gamut-isobmff)* parse large and uuid boxes

### Fixed

- *(isobmff)* refuse interleaved top-level positions and add push_top_level_box
- *(isobmff)* bound a top-level box's complete size, not just its payload

### Other

- *(isobmff)* share the C2PA uuid lens between gamut-heic and gamut-avif ([#781](https://github.com/visualcommons/gamut/pull/781))
- *(isobmff)* [**breaking**] own the clap, pasp, clli and mdcv value structs ([#771](https://github.com/visualcommons/gamut/pull/771))
- gate rustdoc warnings and compile README code blocks as doctests ([#713](https://github.com/visualcommons/gamut/pull/713))
- *(isobmff)* list every move the top-level box round-trip makes
- *(isobmff)* link read's top-level-box note to the public field
- *(isobmff)* reach push_top_level_box's last-AfterFtyp arm, order only
- *(isobmff)* round-trip a non-zero ftyp minor version through the builder
- *(isobmff)* make TopLevelBox and TopLevelPosition non-exhaustive
- *(isobmff)* satisfy clippy::redundant_guards in the accounting test
- *(isobmff)* share the segment walk with avif and heic
- *(isobmff)* separate the largesize body from the cursor advance

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
