# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.1](https://github.com/visualcommons/gamut/compare/gamut-deflate-v0.1.0...gamut-deflate-v0.1.1) - 2026-08-15

### Other

- *(dng)* record the Deflate codec split and its measurements

## [0.1.0](https://github.com/justin13888/gamut/releases/tag/gamut-deflate-v0.1.0) - 2026-07-18

### Added

- *(gamut-deflate)* stabilize v1 with ratio contract and benches
- *(deflate)* add zopfli-style optimal parse (Level::Best)
- *(deflate)* add cost-driven block splitting (Level::Best)
- *(deflate)* add dynamic-Huffman blocks and lazy matching (Level::Default)
- *(deflate)* add LZ77 matching and length/distance symbol coding
- *(deflate)* add fixed-Huffman blocks with stored-vs-fixed selection
- *(deflate)* scaffold gamut-deflate with stored blocks + zlib oracle

### Other

- reflect the gamut-png decode surface in the workspace README and AGENTS.md
- *(mise)* port justfile recipes to mise tasks
