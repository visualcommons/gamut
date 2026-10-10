# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [2.0.0](https://github.com/visualcommons/gamut/compare/gamut-tiff-v1.0.0...gamut-tiff-v2.0.0) - 2026-10-10

### Added

- *(tiff)* carry the C2PA manifest store over the shared placement rules
- *(tiff)* carry ICC, XMP, IPTC-IIM and an Exif sub-IFD
- *(tiff)* add the TiffInfo pre-decode probe
- *(tiff)* encode 16-bit grayscale, RGB and RGBA
- *(tiff)* decode 16-bit grayscale, RGB, RGBA and CMYK samples
- *(tiff)* add the SampleFormat tag type and reject non-integer samples
- *(tiff)* [**breaking**] require an explicit policy for lossy presentation
- *(core)* add structured error diagnostics
- *(gamut-tiff)* support deflate compression

### Fixed

- *(tiff)* read an IPTC/NAA block that Adobe software types LONG
- *(tiff)* give the duplicate-tag anomaly the severity its sibling carries
- *(tiff)* grade a directory that repeats a tag
- *(tiff)* refuse a field and a group under one Exif tag
- *(tiff)* decide an Exif pointer field by its on-disk type code
- *(tiff)* refuse an Exif pointer tag carried as a plain field
- *(tiff)* keep an Exif directory whose only content is a group
- *(tiff)* resolve every standard pointer inside the Exif subtree
- *(tiff)* keep the classic-count guard out of a mutant's reach
- *(tiff)* refuse a store classic TIFF's count word cannot describe
- *(tiff)* refuse an Exif tree deeper than the reader walks back
- *(tiff)* kill the diff mutants the round-4 repairs left alive
- *(tiff)* refuse a C2PA reservation no buffer could hold
- *(tiff)* stop a pointer on a discarded page from failing the read
- *(tiff)* refuse a bad C2PA configuration before every pixel path
- *(tiff)* stop a pointer the metadata never returns from failing the read
- *(tiff)* make the metadata seam agree with its own contracts
- *(tiff)* stop LZW decoding once the strip is satisfied
- *(tiff)* reject non-progressing CCITT runs

### Other

- *(tiff)* name the overlapping-store refusal c2pa_exclusions inherits
- *(tiff)* read the C2PA exclusion ranges through their accessors
- *(tiff)* state that a multi-directory ExifIFD array reads back as its first directory
- *(tiff)* list every configuration refusal under the encoders' error sections
- *(tiff)* list Anomaly::DuplicateTag and its grading change among the #446 additions
- *(tiff)* return map_audit_findings its doc line from check_duplicate_tags
- *(tiff)* derive the second strip's offset from the strip length
- *(tiff)* name the constant the IFD-0 sweep is actually derived from
- *(tiff)* pin the one direction in which the writer is stricter than the reader
- *(tiff)* pin what the duplicate-tag pass's silent skip promises
- *(tiff)* repeat the tag the oracle's readout actually depends on
- *(tiff)* state the repeated-tag refusal and the type-code rule where each is claimed
- *(tiff)* ask libtiff what a directory repeating a tag means
- *(tiff)* pin the IFD-0 pointer set against the constant it names
- *(tiff)* fail the IFD-0 scoping test under the regression it names
- *(tiff)* restate the Exif subtree's refusals where each is claimed
- *(tiff)* assert the group a written Exif directory carries, not its count
- *(tiff)* correct what the seam claims about pointers and offsets
- *(tiff)* make the classic count bound assertable at its boundary
- *(tiff)* pin the configuration refusal on every public encode surface
- *(tiff)* say why the restated pointer match carries no feature guard
- *(tiff)* narrow the discarded-page test to the rule it names
- *(tiff)* find a visited sub-IFD offset in log time, not linear
- *(tiff)* pin the store on the pixel paths nothing else encoded
- *(tiff)* make the C2PA check load-bearing instead of discarded
- *(tiff)* stop claiming more than the seam delivers
- *(tiff)* record the seam's normalisations and its IFD-0 placement
- *(tiff)* pin the palette path's C2PA manifest store
- *(tiff)* fuzz the metadata entry points and pin a BigTIFF store
- *(tiff)* read the multi-page store reservation as a let-chain
- *(tiff)* record the metadata seam and the C2PA manifest store
- *(tiff)* re-attach a #[test] this branch had split off
- *(tiff)* diagnose the audit's three sub-IFD findings
- *(tiff)* pin the LZW decoder's 12-bit code-width cap
- *(tiff)* assert the archival verdict through its public wrapper
- *(tiff)* the fifth and last shift-or equivalent
- *(tiff)* pin T.6's vertical code table and PackBits' two-run rule
- *(tiff)* pin the SHORT/LONG boundary, and a fourth shift-or equivalent
- *(tiff)* two more CCITT equivalents, removed rather than excluded
- *(tiff)* pin the 64 make-up boundary and the half-present tile pair
- *(tiff)* assert LZW compresses, the third encoder to hide behind a round trip
- *(tiff)* assert LZW's width bound at compile time instead of guarding it
- *(tiff)* pin that G4 chooses pass mode, which no oracle could see
- *(tiff)* make two G4 colour decisions falsifiable
- *(tiff)* assert PackBits actually compresses, not just that it round-trips
- *(tiff)* share the two patterns the libtiff cross-checks both use
- adopt as_chunks for constant-size slice chunking
- *(tiff)* pin the 16-bit CMYK narrowing to its depth policy
- merge origin/master into feat/268-pixel-conversion
- *(tiff)* pin which guard rejects a mismatched sample count
- *(tiff)* record 16-bit sample support in the scope ledger
- *(tiff)* extract page-header parsing from the decode funnel
- *(tiff)* scope the decode-size guard to a named helper

## [1.0.0](https://github.com/justin13888/gamut/compare/gamut-tiff-v0.2.0...gamut-tiff-v1.0.0) - 2026-07-18

### Added

- [**breaking**] rebuild the TIFF and DNG deconstructs on the ifd segment auditor
- *(ifd)* [**breaking**] preserve unknown field types losslessly as Value::Unknown
- *(tiff)* [**breaking**] replace the code accessors with TryFrom and From conversions
- *(tiff)* add strict deconstruct mode with full-file accounting

### Other

- *(ifd)* byte-completeness ledgers and issue #263 status
- *(tiff)* release v1.0.0
- *(tiff)* document the v1 surface and scope ledger
- *(tiff)* [**breaking**] finalize the v1 surface
- *(tiff)* drop the dormant gamut-color and gamut-dsp dependencies
- *(tiff/dng/exif)* source pointer-tag constants from gamut-ifd
- Merge pull request #235 from justin13888/feat/181-ifd-v1
- *(tiff)* correct the gamut-dsp attribution in the crate docs
- apply nightly rustfmt import grouping across the workspace
- *(mise)* port justfile recipes to mise tasks
- Merge pull request #151 from justin13888/feat/benchmarks
- *(tiff)* close mutation-testing gaps

## [0.2.0](https://github.com/justin13888/gamut/compare/gamut-tiff-v0.1.0...gamut-tiff-v0.2.0) - 2026-06-12

### Added

- *(tiff)* [**breaking**] migrate to typed EncodeImage/DecodeImage, drop weakly-typed methods

### Other

- *(core)* [**breaking**] remove the legacy Encoder/Decoder traits
- *(tiff)* type the palette colour table as Palette8
