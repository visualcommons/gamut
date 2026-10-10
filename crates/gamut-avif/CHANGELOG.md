# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [2.0.0](https://github.com/visualcommons/gamut/compare/gamut-avif-v1.1.0...gamut-avif-v2.0.0) - 2026-10-10

### Added

- *(avif)* reserve, write and locate a C2PA manifest store
- *(avif)* lower high-bit-depth jobs across the codec-abi seam
- *(avif)* [**breaking**] honour with_chroma on the 16-bit input path
- *(avif)* encode Rgb16 and Rgba16 as 10- or 12-bit AVIF
- *(avif)* carry the coded bit depth across the AV1 encode seam
- *(avif)* encode Rgba8 with an alpha auxiliary item and Gray8 as monochrome
- *(avif)* complete the 4:2:2 coding path
- *(avif)* [**breaking**] default lossy output to 4:2:0 and expose the chroma format
- *(av1)* parse OBUs and the AV1 sequence and frame headers
- *(avif)* select CICP colour and write ICC, Exif and XMP
- *(av1)* adapt CDFs while coding tiles
- *(gamut-avif)* add the high-bit-depth RGBA16 presentation surface
- *(avif)* code the lossy path in YCbCr
- *(core)* add structured error diagnostics
- *(gamut-isobmff)* parse large and uuid boxes

### Fixed

- *(avif)* refuse a 4 GiB C2PA reservation before zero-filling it
- *(avif)* refuse a C2PA reservation that cannot be framed
- *(avif)* probe both update-store prefixes, as gamut-heic does
- *(avif)* send the backend the chroma the planes actually carry
- *(avif)* refuse a subsampled backend stream instead of stamping it 4:4:4

### Other

- *(avif)* refuse loosened 4 GiB bounds before the one allocating call
- *(avif)* reflow the 64-bit ceiling qualification
- *(avif)* qualify the 4 GiB C2PA ceiling claims as 64-bit only
- *(avif)* pin the reserved-slot minimum with literal lengths
- *(avif)* say with_c2pa applies no minimum to a supplied store
- *(avif)* state the container writer's 4 GiB C2PA refusal
- *(avif)* ask for the store's room instead of adding it in
- *(avif)* say the encoder writes only box_purpose manifest
- *(avif)* share the dav1d bridge through tests/common
- *(avif)* write the reserved C2PA slot's zeros once
- *(avif)* name the C2PA read type for the slot it reports
- *(avif)* key the C2PA locator fixture on the box type, not the body
- *(avif)* record the C2PA carriage in STATUS and README
- *(avif)* append the C2PA uuid box with push_top_level_box
- *(avif)* keep the C2PA remux oracle test to one failure reason
- *(avif)* libavif decodes an AVIF carrying a C2PA uuid box unchanged
- *(avif)* build IsoBmffImage through its constructor
- *(avif)* close the crate's first mutation survey
- *(isobmff)* drop imports the moved tests no longer use
- *(isobmff)* share the segment walk with avif and heic
- *(avif)* separate grid cropping from tile placement
- *(avif)* pin the sequence-header chroma derivation directly
- merge feat/398-av1-high-bitdepth into feat/399-avif-16bit-inputs
- merge feat/397-avif-alpha-aux into feat/398-av1-high-bitdepth
- merge origin/master into feat/397-avif-alpha-aux
- Merge pull request #403 from visualcommons/feat/391-avif-422-profile2
- Merge pull request #402 from visualcommons/feat/390-avif-420-profile0
- *(avif)* pin each branch of the backend subsampling derivation
- Merge branch 'feat/390-avif-420-profile0' into feat/391-avif-422-profile2
- *(avif)* say precisely when with_chroma is ignored
- Merge branch 'feat/390-avif-420-profile0' into feat/391-avif-422-profile2
- Merge remote-tracking branch 'origin/master' into feat/390-avif-420-profile0
- *(color)* take the coded depth from the matrix, not beside it
- Merge remote-tracking branch 'origin/master' into feat/259-avif-decoder
- *(avif)* record the 10/12-bit input surface
- *(avif)* check the 10/12-bit inputs against libavif and dav1d
- *(av1)* record the high-bit-depth sample path
- *(avif)* kill the mutants the alpha slice pulled into the diff
- *(avif)* record the alpha and grayscale inputs in the status ledger
- *(avif)* check the alpha and grayscale surface against libavif
- merge feat/390-avif-420-profile0 into feat/391-avif-422-profile2
- close the remaining chroma mutation gaps
- *(av1)* record monochrome encoding in the README and status ledger
- merge feat/390-avif-420-profile0 into feat/391-avif-422-profile2
- merge origin/master into feat/390-avif-420-profile0
- cover the chroma derivations directly, not through an encode
- *(avif)* use as_chunks in the metadata round-trip test
- *(avif)* record the colour and metadata rows as implemented
- Merge pull request #394 from visualcommons/feat/389-av1-per-plane-geometry
- derive chroma plane sizes from one shared rule
- mark the aom/dav1d submodules `update = none`
- adopt as_chunks for constant-size slice chunking
- Merge branch 'master' into feat/335-avif-ycbcr-matrix
- Merge pull request #360 from visualcommons/feat/303-high-bit-depth-presentation
- pin the blend rounding and the high-bit-depth paths mutation testing missed
- *(avif)* close the mutation gaps in the colour seam
- record the high-bit-depth presentation surface in STATUS and READMEs
- *(av1)* record CDF adaptation in STATUS and README
- *(avif)* record the YCbCr matrix surface

## [1.1.0](https://github.com/justin13888/gamut/compare/gamut-avif-v1.0.0...gamut-avif-v1.1.0) - 2026-07-20

### Added

- *(avif)* add pluggable Av1StillEncoder backends + push_backend (additive)

### Other

- *(avif)* ledger the encode backend seam and reserve the decode registry
- *(avif)* assert byte-identical default output and the backend fallback contract

## [1.0.0](https://github.com/justin13888/gamut/compare/gamut-avif-v0.3.0...gamut-avif-v1.0.0) - 2026-07-18

### Added

- *(avif)* treat unspecified matrix coefficients as bt601 on rgba
- *(avif)* add rgba presentation path with colour alpha and transforms
- *(avif)* add Av1StillDecoder seam and planar decode pipeline
- *(avif)* add byte-accounting container and role-typed image view
- *(avif)* add typed av1C parse and OBU enumeration layer
- *(isobmff)* [**breaking**] finalise the v1 still-image container surface

### Fixed

- *(avif)* follow the 2022 imir axis semantics on both surfaces

### Other

- *(avif)* record the decode surface in STATUS and references
- *(avif)* add libavif and dav1d differential conformance suite
- *(avif)* release v1.0.0
- *(avif)* finalize the v1 scope ledger and crate docs
- *(avif)* [**breaking**] future-proof the v1 public API
- *(avif)* hermetic remux round-trip via the libavif oracle
- *(avif)* note the container rows are ready in gamut-isobmff v1
- *(av1)* vendor libaom as the definitive AV1 reference oracle
- *(avif)* assert the quality precondition in the roundtrip helper
- *(avif)* define AvifEncoder constructors before its default
- *(avif)* finalize v1 documentation
- *(avif)* assert container field contents and trim redundant tests
- *(avif)* [**breaking**] type the orientation API
- *(avif)* [**breaking**] replace raw qindex with a 0..=100 quality knob
- *(avif)* build the container via gamut_isobmff::write
- add Divan benchmark harnesses for codec and primitive crates

## [0.3.0](https://github.com/justin13888/gamut/compare/gamut-avif-v0.2.0...gamut-avif-v0.3.0) - 2026-06-12

### Added

- [**breaking**] migrate AVIF and WebP to typed EncodeImage/DecodeImage, drop weakly-typed methods
- *(avif)* irot/imir display-orientation transforms
- *(av1)* superres — horizontal upscaling (§7.16) with loop restoration after upscale
- *(av1)* loop restoration — luma Wiener filter (§7.17)
- *(av1)* multi-tile (two uniform tile columns) + tile-group framing
- *(av1)* rectangular partitions (PARTITION_HORZ/VERT) + rect transforms
- *(av1)* TX_64X64 transforms + 64×64 PARTITION_NONE blocks
- *(av1)* segmentation with per-segment alternate quantizers (SEG_LVL_ALT_Q)
- *(av1)* luma palette mode (selection + colors + wavefront index map)
- *(av1)* enable allow_screen_content_tools + palette_mode_info signaling
- *(av1)* per-superblock delta-LF (loop-filter-level delta) [lossy-intra delta-lf]
- *(av1)* block-level skip (skip = 1) for lossy intra [lossy-intra skip]
- *(av1)* per-superblock delta-Q (TX_MODE_SELECT frame) [lossy-intra P10]
- *(av1)* variable transform size (TX_MODE_SELECT) for lossy intra [lossy-intra P9]
- *(av1)* 32×32 transform blocks (TX_32X32) for lossy intra [lossy-intra P7e]
- *(av1)* 16×16 transform blocks (TX_16X16) for lossy intra [lossy-intra P7d]
- *(av1)* complete the 8×8 luma intra mode surface [lossy-intra P7c]
- *(av1)* 8×8 transform blocks (TX_8X8) for lossy intra [lossy-intra P7b]
- *(av1)* chroma-from-luma (CfL) intra prediction [lossy-intra P14]
- *(av1)* recursive filter-intra prediction [lossy-intra P13]
- *(avif)* expose lossy encoding through the AVIF container

### Other

- *(av1)* [**breaking**] widen reconstruction to u16 for high-bit-depth support
- *(av1)* reconcile STATUS.md with shipped lossy-intra surface
- Merge pull request #49 from justin13888/feat/av1-lossy-p19-cdef
- Merge branch 'master' into feat/av1-lossy-p18-deblock
- vendor dav1d/libavif as submodule FFI oracles for decoder cross-checks
- clarify av1 codec vs avif format distinction
- Merge pull request #20 from justin13888/docs/crate-readmes
- add structurally consistent README to every crate
