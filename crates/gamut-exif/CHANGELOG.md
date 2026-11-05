# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [3.0.0](https://github.com/visualcommons/gamut/compare/gamut-exif-v2.0.0...gamut-exif-v3.0.0) - 2026-11-05

### Added

- *(exif)* decode Fujifilm MakerNote tags into a read-only view ([#1039](https://github.com/visualcommons/gamut/pull/1039))
- *(exif)* decode Sony MakerNote tags into a read-only view ([#1034](https://github.com/visualcommons/gamut/pull/1034))
- *(exif)* add the Leica MakerNote dialects and decode their tags ([#1008](https://github.com/visualcommons/gamut/pull/1008))
- *(exif)* decode Canon MakerNote tags into a read-only view ([#1000](https://github.com/visualcommons/gamut/pull/1000))
- *(exif)* resolve the MakerNote dispatch layout per vendor ([#990](https://github.com/visualcommons/gamut/pull/990))
- *(gamut-ifd)* shared tag registry for TIFF 6.0 and DNG tags ([#892](https://github.com/visualcommons/gamut/pull/892))

### Other

- *(gamut-ifd)* [**breaking**] named-struct returns, one audited reader, core Segment, flat C2PA exports ([#1140](https://github.com/visualcommons/gamut/pull/1140))
- *(gamut-exif)* [**breaking**] with_* setters, one error type, one path per item ([#1139](https://github.com/visualcommons/gamut/pull/1139))
- *(gamut-ifd)* [**breaking**] own the shared TIFF/DNG audit types, extents and writer layout ([#1130](https://github.com/visualcommons/gamut/pull/1130))
- correct per-crate prose that contradicts shipped code ([#1127](https://github.com/visualcommons/gamut/pull/1127))
- *(gamut-core)* [**breaking**] opaque Error, private Dimensions fields, shared vocabulary ([#1126](https://github.com/visualcommons/gamut/pull/1126))
- bring repository-wide docs in line with the shipped code ([#1114](https://github.com/visualcommons/gamut/pull/1114))

## [2.0.0](https://github.com/visualcommons/gamut/compare/gamut-exif-v1.0.0...gamut-exif-v2.0.0) - 2026-10-12

### Migration notes

- `parse` keeps its signature, its accept/reject verdict and its re-serialised bytes, with the
  exceptions below, measured over a 3 144-case truncation-and-corruption sweep against 1.0.0.
- An error message's offset is now a position in the buffer the caller handed in, so for a marked
  blob it is six bytes larger than before — the `Exif\0\0` marker — while a `Dropped::offset`
  stays relative to the TIFF stream.
- A 1st IFD carrying `JPEGInterchangeFormat` with no `JPEGInterchangeFormatLength` used to parse as
  a thumbnail that simply had no bytes; it is now a loss named in the report
  (`DropReason::ThumbnailLengthMissing`) and rejected by `strict` with `ExifError::BadThumbnail`. A
  caller running `strict` over blobs 1.0.0 accepted should know the verdict moved.
- A 1st IFD whose integer `StripOffsets` names strips that cannot be read used to parse as a
  thumbnail with no strips, its `StripOffsets` kept and re-emitted by the writer addressing
  nothing; it is now a `DroppedRegion::ThumbnailStrips` loss with `StripOffsets` removed from the
  model, rejected by `strict` with `ExifError::BadThumbnail`.
- Neither verdict move refuses a conformant input (see the README's "Unreadable thumbnails"), so
  they are fixes, not redefinitions.

### Added

- *(exif)* name shadowed duplicate-tag entries in the read report ([#791](https://github.com/visualcommons/gamut/pull/791))
- *(exif)* deepen the read report with per-entry recovery, a byte audit and a real-DNG laziness case ([#770](https://github.com/visualcommons/gamut/pull/770))
- *(exif)* type TagConstraintError::FieldType's expected and actual fields
- *(exif)* describe the enumerated tag values behind a describe feature
- *(exif)* reject a write that contradicts the tag's DC-008 value shape
- *(exif)* catalogue the seven Exif 3.0 authorship and software tags
- *(exif)* expose each tag's DC-008 field type and component count
- *(exif)* report what a lenient parse discarded
- *(exif)* read EXIF from any ReadAt byte source
- *(gamut-exif)* convert gps metadata to geocoordinates

### Fixed

- *(exif)* pin the MakerNote entry the model keeps, not the first duplicate ([#795](https://github.com/visualcommons/gamut/pull/795))
- *(gamut-exif)* never re-emit a thumbnail pointer whose bytes were dropped ([#708](https://github.com/visualcommons/gamut/pull/708))
- *(exiv2-oracle)* serialise the EXIF entry points behind the exiv2 lock ([#707](https://github.com/visualcommons/gamut/pull/707))
- *(exif)* name the unreadable range instead of a mandatory sibling
- *(exif)* name a thumbnail offset that carries no length
- *(exif)* describe the nine GPS reference tags CIPA DC-008 enumerates
- *(exif)* propagate a failing source through the maker-note pin
- *(exif)* serialise this file's exiv2 calls
- *(exif)* read GainControl as SHORT, per its own section
- *(exif)* name trailing directories and propagate a failing source

### Other

- *(exif)* lead the describe differential's file name with oracle ([#817](https://github.com/visualcommons/gamut/pull/817))
- *(exif)* keep the thumbnail pair rule independent of Compression ([#737](https://github.com/visualcommons/gamut/pull/737))
- gate rustdoc warnings and compile README code blocks as doctests ([#713](https://github.com/visualcommons/gamut/pull/713))
- *(gamut-exif)* differential of describe tables against exiv2's print() ([#685](https://github.com/visualcommons/gamut/pull/685))
- *(exif)* scope the per-tag type claim to DC-008's five tag tables
- Merge remote-tracking branch 'origin/master' into feat/417-exif-tag-breadth
- *(exif)* name Table 21's axis at the last two sites
- *(exif)* name Table 21's axis at every site and drop a release tense
- *(exif)* correct the offset-frame note and name Table 21's axis
- *(exif)* ground the strict thumbnail rule and ship the changed verdict
- *(exif)* scope the Table 21 grounding and the streaming offset frame
- *(exif)* name the thumbnail drop reason for its one site
- *(exif)* count the excluded tables as the five they are
- *(exif)* state the rule that selects the described tags
- Merge remote-tracking branch 'origin/feat/419-exif-streaming-reader' into feat/417-exif-tag-breadth
- *(exif)* correct the strict report, the offset frames and two doc links
- *(exif)* sweep the thumbnail and trailing-chain reads for a failing source
- *(exif)* state what the catalogue covers and which doors are lenient
- Merge remote-tracking branch 'origin/feat/419-exif-streaming-reader' into feat/417-exif-tag-breadth
- *(exif)* name the vendored source of each non-DC-008 tag
- *(exif)* drop an unreachable arm from the maker-note pin
- *(exif)* name the exact exiv2 version the divergence list came from
- Merge remote-tracking branch 'origin/feat/419-exif-streaming-reader' into feat/417-exif-tag-breadth
- *(exif)* point the crate docs at the checked setter and the describe feature
- *(exif)* pin which tags CIPA DC-008 enumerates
- *(exif)* record the completed DC-008 catalogue and its two features
- *(exif)* check every catalogued tag name against exiv2
- *(exif)* close the survivors of this crate's first mutation survey
- *(exif)* separate what equality covers from what it ignores

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
