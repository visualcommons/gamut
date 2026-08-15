# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.4.2](https://github.com/visualcommons/gamut/compare/gamut-av1-v0.4.1...gamut-av1-v0.4.2) - 2026-08-15

### Added

- *(core)* add structured error diagnostics

## [0.4.1](https://github.com/justin13888/gamut/compare/gamut-av1-v0.4.0...gamut-av1-v0.4.1) - 2026-07-20

### Other

- updated the following local packages: gamut-core, gamut-color, gamut-bitstream

## [0.4.0](https://github.com/justin13888/gamut/compare/gamut-av1-v0.3.0...gamut-av1-v0.4.0) - 2026-07-18

### Other

- *(dsp)* [**breaking**] namespace the surface into av1, math, and mulaw spec-family modules
- *(tooling)* finish just/lefthook references left from the migration
- apply nightly rustfmt import grouping across the workspace
- *(av1)* vendor libaom as the definitive AV1 reference oracle
- Merge branch 'master' into test/mutants-av1
- *(av1)* close mutation gaps in tile.rs (block coding + entropy)
- *(av1)* close mutation gaps in filter.rs (in-loop filters)
- *(av1)* close mutation gaps in quant, transform, headers, encoder, cdf

## [0.3.0](https://github.com/justin13888/gamut/compare/gamut-av1-v0.2.0...gamut-av1-v0.3.0) - 2026-06-12

### Added

- *(av1)* [**breaking**] type ReconImage.bit_depth as BitDepth; add Planar8 view ctor
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
- *(av1)* complete directional intra modes (D45/D67/D203) [lossy-intra P12b]
- *(av1)* directional intra modes V/H/D135/D113/D157 [lossy-intra P12a]
- *(av1)* non-directional luma intra modes (PAETH/SMOOTH) [lossy-intra P11]
- *(av1)* per-block transform-type selection from TX_SET_INTRA_2
- *(av1)* extend lossy quantizer range to all four CDF contexts
- *(av1)* lossy intra reconstruction pivot (DCT + quant), bit-exact
- *(av1)* add 2-D transform assembly (inverse + forward)
- *(av1)* add quantizer tables, dequant, and encoder quantizer

### Fixed

- *(av1)* force-split partition blocks that extend past the frame edge

### Other

- *(av1)* [**breaking**] widen reconstruction to u16 for high-bit-depth support
- *(av1)* reconcile STATUS.md with shipped lossy-intra surface
- *(av1)* use shared dsp/color scalar primitives
- Merge pull request #49 from justin13888/feat/av1-lossy-p19-cdef
- Merge branch 'master' into feat/av1-lossy-p18-deblock
- vendor dav1d/libavif as submodule FFI oracles for decoder cross-checks
- clarify av1 codec vs avif format distinction
- Merge pull request #20 from justin13888/docs/crate-readmes
- add structurally consistent README to every crate
