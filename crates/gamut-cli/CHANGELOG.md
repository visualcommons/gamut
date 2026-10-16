# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.4.0](https://github.com/visualcommons/gamut/compare/gamut-cli-v0.3.1...gamut-cli-v0.4.0) - 2026-10-12

### Added

- *(gamut-isobmff, gamut-heic, gamut-avif)* [**breaking**] C2PA ranges are a C2paByteRange ([#812](https://github.com/visualcommons/gamut/pull/812))
- *(gamut-avif, gamut-cli)* [**breaking**] follow gamut-av1's Balanced default speed ([#810](https://github.com/visualcommons/gamut/pull/810))
- *(cli)* report a PNG's C2PA manifest store in gamut inspect ([#784](https://github.com/visualcommons/gamut/pull/784))
- *(color)* model H.273 transfer code points 6 and 15 ([#779](https://github.com/visualcommons/gamut/pull/779))
- *(cli)* add --output json to gamut inspect ([#768](https://github.com/visualcommons/gamut/pull/768))
- *(icc)* report the §10.3 cicp MatrixCoefficients clause from validate ([#740](https://github.com/visualcommons/gamut/pull/740))
- *(cli)* [**breaking**] report the C2PA manifest store in gamut inspect for AVIF, WebP, TIFF and DNG ([#718](https://github.com/visualcommons/gamut/pull/718))
- *(gamut-cli)* AVIF encoder flags for speed, threads, alpha quality, clean alpha, matrix, depth and chroma ([#744](https://github.com/visualcommons/gamut/pull/744))
- *(isobmff)* skip a sequence moov beside meta instead of refusing the file ([#712](https://github.com/visualcommons/gamut/pull/712))
- *(gamut-cli)* [**breaking**] batch convert — many inputs in parallel, stdin/stdout, overwrite, quiet, AVIF size breakdown ([#705](https://github.com/visualcommons/gamut/pull/705))
- *(isobmff)* typed mdcv mastering-display property ([#688](https://github.com/visualcommons/gamut/pull/688))
- *(gamut-cli)* keep alpha, depth, ICC and Exif when converting to AVIF ([#682](https://github.com/visualcommons/gamut/pull/682))
- *(cli)* put the PNG effort ladder on gamut convert
- *(cli)* cap the C2PA list and say when a format was asserted
- *(cli)* say what metadata a conversion could not carry
- *(cli)* report a HEIC's C2PA manifest store in gamut inspect
- *(png)* name the inflation-ratio refusal apart from the byte budget
- *(cli)* inspect PNG byte accounting
- *(cli)* add --webp-effort and --webp-near-lossless
- *(cli)* decode PNG and JPEG through gamut's own decoders
- *(cli)* add --png-effort to convert
- *(cli)* add --jxl-modular to gamut convert

### Fixed

- *(cli)* list a missing IEND as a PNG inspect finding
- *(png)* [**breaking**] carry an XMP packet's framing, and report what §11.3.3 only advises
- *(cli)* confirm a sniffed container is a HEVC still image
- *(cli)* restore the truncated-list notice for TIFF and DNG
- *(cli)* gate inspect on what it read, and bound the lists it prints
- *(png-cli)* say why the filter scan was skipped
- *(webp)* quantize near-lossless uniformly and never inflate

### Other

- *(isobmff)* share the C2PA uuid lens between gamut-heic and gamut-avif ([#781](https://github.com/visualcommons/gamut/pull/781))
- *(isobmff)* [**breaking**] own the clap, pasp, clli and mdcv value structs ([#771](https://github.com/visualcommons/gamut/pull/771))
- align gamut-color and gamut-cli crate doc summaries with their code ([#739](https://github.com/visualcommons/gamut/pull/739))
- gate rustdoc warnings and compile README code blocks as doctests ([#713](https://github.com/visualcommons/gamut/pull/713))
- Merge remote-tracking branch 'origin/master' into feat/484-png-effort-dial
- Merge remote-tracking branch 'origin/master' into fix/483-png-metadata-preservation
- *(cli)* build the inspect fixtures through IsoBmffImage::new
- Merge remote-tracking branch 'origin/feat/224-png-encoder-efficiency' into feat/448-cli-inspect-c2pa
- *(cli)* resolve --png-preset through gamut-png's own ladder
- Merge remote-tracking branch 'origin/feat/440-png-cabx' into fix/483-png-metadata-preservation
- *(cli)* pin the carried-with-a-caveat report and the profile-family reduction
- Merge remote-tracking branch 'origin/master' into feat/224-png-encoder-efficiency
- *(cli)* format the ISOBMFF-movie route test
- *(cli)* pin that an ISOBMFF movie is refused by the HEIC parse, not by brand
- *(cli)* drive all three PNG inspect verdicts through the binary
- *(cli)* drive the shape the C2PA cap can hide through the binary
- *(png)* pin Latin-1 text, the refused chunk pairs, and what a re-encode carries
- *(cli)* drive gamut inspect's HEIC arm through the built binary
- *(cli)* record the gamut inspect exit-code contract in docs/
- *(cli)* state why inspect's verification gate is PNG-only
- *(cli)* build the isobmff demo image through the constructor
- *(cli)* state what inspect's exit code means per format
- merge origin/master into feat/261-webp-encoder-knobs
- adopt as_chunks for constant-size slice chunking
- merge origin/master into feat/261-webp-encoder-knobs
- merge origin/master into feat/268-pixel-conversion
- merge origin/master into feat/268-pixel-conversion

## [0.3.1](https://github.com/visualcommons/gamut/compare/gamut-cli-v0.3.0...gamut-cli-v0.3.1) - 2026-07-30

### Other

- update Cargo.lock dependencies

## [0.3.0](https://github.com/justin13888/gamut/compare/gamut-cli-v0.2.0...gamut-cli-v0.3.0) - 2026-07-18

### Added

- [**breaking**] rebuild the TIFF and DNG deconstructs on the ifd segment auditor
- *(cli)* list BitDepth::Sixteen in color list
- *(cli)* JPEG XL output format and .jxl input decoding
- *(cli)* add the gamut isobmff container entrypoint
- *(cli)* add `gamut icc` to extract and inspect embedded ICC profiles
- *(cli)* add `gamut inspect` for strict TIFF/DNG deconstruct
- *(cli)* show resolved gamut library version in gamut -V

### Fixed

- *(cli)* migrate convert to the AvifEncoder lossless/lossy API

### Other

- *(tiff)* [**breaking**] finalize the v1 surface
- Merge pull request #266 from justin13888/feat/260-color-bit-depth
- Merge branch 'master' into feat/184-isobmff-v1
- *(icc)* [**breaking**] move profile-ID computation onto ProfileId and add Display impls
- *(icc)* [**breaking**] adopt std conversion traits for signatures and header enums
- apply nightly rustfmt import grouping across the workspace
- Merge branch 'master' into feat/png

## [0.2.0](https://github.com/justin13888/gamut/compare/gamut-cli-v0.1.0...gamut-cli-v0.2.0) - 2026-06-12

### Added

- [**breaking**] migrate AVIF and WebP to typed EncodeImage/DecodeImage, drop weakly-typed methods
- *(tiff)* [**breaking**] migrate to typed EncodeImage/DecodeImage, drop weakly-typed methods
- *(cli)* add TIFF output to gamut convert
- *(cli)* decode WebP input via gamut's own decoder
- *(cli)* lossy + alpha WebP in `gamut convert`; finalize docs
- *(cli)* recognize WebP output in gamut convert
- *(cli)* add build timestamp to gamut -V
- *(cli)* report build provenance in gamut -V
- implement gamut-cli sandbox and primitives feature

### Other

- *(color)* [**breaking**] delete the unused PixelFormat enum, document BitDepth/ChromaSubsampling
- Merge pull request #44 from justin13888/feat/webp
- *(webp)* mark VP8L lossless (M0/M1) implemented across STATUS, docs, and READMEs
- Merge pull request #20 from justin13888/docs/crate-readmes
- add structurally consistent README to every crate
