# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.4.0](https://github.com/visualcommons/gamut/compare/gamut-webp-v0.3.1...gamut-webp-v0.4.0) - 2026-08-22

### Added

- *(webp)* [**breaking**] require an explicit policy for lossy presentation
- *(core)* add structured error diagnostics

### Other

- adopt as_chunks for constant-size slice chunking
- close the mutation-testing gaps in the conversion paths
- merge origin/master into feat/268-pixel-conversion

## [0.3.1](https://github.com/visualcommons/gamut/compare/gamut-webp-v0.3.0...gamut-webp-v0.3.1) - 2026-07-30

### Added

- *(gamut-webp)* read and write the EXIF, XMP, and ICCP metadata chunks

### Other

- record WebP metadata support in STATUS, READMEs, and AGENTS.md
- *(gamut-webp)* pin the metadata chunks against libwebp's muxer

## [0.3.0](https://github.com/justin13888/gamut/compare/gamut-webp-v0.2.0...gamut-webp-v0.3.0) - 2026-07-18

### Added

- *(webp)* decode VP8 mb_lf_adjustments and validate the bitstream version

### Other

- *(dsp)* [**breaking**] namespace the surface into av1, math, and mulaw spec-family modules
- *(tooling)* finish just/lefthook references left from the migration
- apply nightly rustfmt import grouping across the workspace
- *(webp)* retire the equivalent sharpness mutant via an explicit literal
- *(webp)* pin VP8L predictor/prefix correctness; justify remaining equivalents
- *(webp)* pin VP8L transforms and LZ77 codecs; justify encoder free choices
- *(webp)* pin VP8 quantization/header correctness; justify encoder free choices
- *(webp)* close the loop-filter and VP8L decoder mutation survivors
- *(webp)* [**breaking**] narrow the public API surface
- *(webp)* drop inert reserve hints in the ARGB converters
- *(webp)* close surviving mutants in the bool coder and loop filter
- *(webp)* pin B_PRED subblock prediction; drop its dead DC duplicate
- *(webp)* close surviving mutants in transforms, decoder, encoder, alpha
- *(webp)* consolidate redundant lossless oracle tests
- *(webp)* record mb_lf and version conformance in STATUS and README
- *(webp)* document deferred and unsupported features
- *(webp)* correct stale config and decoder documentation
- *(webp)* drop the unused gamut-bitstream dependency
- *(color)* [**breaking**] unify the YCbCr range selector on cicp::ColorRange
- add Divan benchmark harnesses for codec and primitive crates

## [0.2.0](https://github.com/justin13888/gamut/compare/gamut-webp-v0.1.0...gamut-webp-v0.2.0) - 2026-06-12

### Added

- [**breaking**] migrate AVIF and WebP to typed EncodeImage/DecodeImage, drop weakly-typed methods
- *(cli)* lossy + alpha WebP in `gamut convert`; finalize docs
- *(webp)* lossless-compressed alpha (ALPH C=1)
- *(webp)* lossy alpha — raw ALPH chunk, filters, RGBA API
- *(webp)* VP8X extended container header
- *(webp)* per-segment filter levels, libwebp interop, robustness corpus
- *(webp)* per-macroblock skip coding (mb_skip_coeff)
- *(webp)* 1/2/4/8 DCT token partitions
- *(webp)* quantizer segmentation
- *(webp)* normal in-loop deblocking filter
- *(webp)* simple in-loop deblocking filter
- *(webp)* B_PRED per-4x4 luma intra prediction
- *(webp)* whole-block V/H/TM intra prediction with mode selection
- *(webp)* minimal DC-only VP8 lossy pipeline, bit-exact vs libwebp
- *(webp)* implement VP8 key-frame header read/write
- *(webp)* implement VP8 coefficient token coding
- *(webp)* implement VP8 dequantization tables and factors
- *(webp)* implement VP8 4×4 DCT and WHT transforms
- *(webp)* implement VP8 boolean entropy coder and tree coding
- *(webp)* emit multi-group entropy image (meta prefix codes)
- *(webp)* emit LZ77 backward references and color cache
- *(webp)* emit color-indexing (palette) transform
- *(webp)* emit subtract-green, predictor, and color transforms
- *(webp)* implement VP8L encoder (literals + single prefix-code group)
- *(webp)* implement full VP8L lossless decoder
- *(webp)* implement VP8L LZ77 distance mapping and color cache
- *(webp)* implement VP8L inverse transforms (subtract-green, predictor, color, color-indexing)
- *(webp)* implement canonical prefix codes (decode + length-limited encode)
- *(webp)* implement VP8L header read/write
- *(webp)* implement LSB-first VP8L bit reader and writer
- *(webp)* scaffold codec module tree and wire encoder/decoder API

### Fixed

- *(color)* encode WebP lossy as limited-range BT.601 to match libwebp

### Other

- *(webp)* use shared dsp/color scalar primitives
- *(webp)* add VP8L lossless decoder robustness corpus
- *(webp)* pin VP8 decoder against libwebp's forced feature surface
- *(webp)* add realistic + large-image corpus for VP8 lossy oracle
- *(webp)* add realistic + large-image corpus for VP8L lossless oracle
- *(webp)* mark VP8L lossless (M0/M1) implemented across STATUS, docs, and READMEs
- *(webp)* expand libwebp oracle matrix and add decoder robustness corpus
- *(webp)* document scope decisions for non-core features
- *(webp)* add libwebp differential oracle harness
- *(webp)* add two-part implementation STATUS.md and refresh READMEs
- clarify image-first crate boundaries
- Merge pull request #20 from justin13888/docs/crate-readmes
- add structurally consistent README to every crate
