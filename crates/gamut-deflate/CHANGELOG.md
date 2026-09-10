# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.1](https://github.com/visualcommons/gamut/compare/gamut-deflate-v0.1.0...gamut-deflate-v0.1.1) - 2026-09-10

### Added

- *(deflate)* span the optimal parse instead of disabling it above 1 MiB
- *(deflate)* expose optimal-parse effort via DeflateEncoder::with_effort

### Other

- *(deflate)* make the README ratio-table claim exact and name the shared walk
- *(deflate)* pin the README lz77.rs row and correct the ramp20k period
- *(deflate)* relax each match length at its own nearest distance
- *(deflate)* drop the always-true guard in front of the prune
- *(deflate)* compare eight bytes per step in the longest-match loop
- *(deflate)* remove five unkillable mutants from the bit packing
- deny unsafe in the hot-path crates instead of forbidding it
- *(deflate)* pin the encoder's output so a silent size regression fails
- *(mutants)* record the failure modes and the one way to run a survey
- *(deflate)* keep the 1 MiB boundary case fast under limit-check mutation
- *(deflate)* document the configurable optimal-parse effort budget
- *(deflate)* pin Level as repr(u8) with explicit discriminants
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
