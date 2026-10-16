# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.0](https://github.com/visualcommons/gamut/compare/gamut-png-v0.1.0...gamut-png-v0.2.0) - 2026-10-12

### Added

- *(cli)* report a PNG's C2PA manifest store in gamut inspect ([#784](https://github.com/visualcommons/gamut/pull/784))
- *(png)* compose the encoder's size and time knobs into a preset
- *(png)* let a caller set the IDAT optimal-parse limit
- *(png)* clean a caller-supplied palette before writing it
- *(png)* preserve metadata across a re-encode, and refuse the chunk pairs the spec forbids
- *(png)* fill a reserved C2PA store in place
- *(png)* name the inflation-ratio refusal apart from the byte budget
- *(png)* carry the C2PA manifest store in the caBX chunk
- *(png)* seal FilterStrategy, and clear only the bigram words a row dirtied
- *(png)* bound the deconstruct walk and name what it actually read
- *(png)* clean invisible colour on the 16-bit paths too
- *(png)* entropy and bigram filter heuristics
- *(png)* order the palette, and close the colour-key mutation gaps
- *(png)* reduce binary alpha to a tRNS colour key
- *(cli)* inspect PNG byte accounting
- *(png)* opt-in cleanup of invisible pixel colour
- *(png)* account every byte of a PNG with deconstruct
- *(gamut-png)* read metadata without decoding pixels
- *(png)* extend lossless auto-reduce to grey and 16-bit inputs
- *(png)* route typed presentation through gamut-core convert
- *(png)* plumb DEFLATE effort through PngEncoder::with_effort
- *(core)* add structured error diagnostics

### Fixed

- *(gamut-png)* compile analyze8/analyze16 only for tests and test-support ([#805](https://github.com/visualcommons/gamut/pull/805))
- *(png)* bound the top rung's optimal-parse span
- *(png)* keep the palette entry a colour-form background names
- *(png)* trim the trailing opaque alphas without a loop that can hang
- *(png)* race the input layout even where it is outside the profile's family
- *(png)* refuse a null only in a text annotation that is written
- *(png)* keep auto-reduce in the colour family the ICC profile names
- *(png)* write the iCCP profile name as Latin-1, the encoding the reader decodes
- *(png)* drop a text string's null instead of refusing the encode
- *(png)* check a C2PA reservation's length before allocating it
- *(png)* rescale bKGD when auto-reduce lowers the depth
- *(png)* reject a C2PA store past the 2^31 - 1 byte chunk limit
- *(png)* [**breaking**] carry an XMP packet's framing, and report what §11.3.3 only advises
- *(png)* [**breaking**] implement §11.3.3's text clauses and stop refusing the colour pair
- *(png)* count every ignored C2PA store, and say what is not counted
- *(png)* reject a C2PA span the chunk's own length contradicts
- *(png)* never read a caBX after IDAT as the manifest store
- *(png)* end the C2PA store walk with the datastream
- *(png)* count IHDR against the walk's chunk ceiling
- *(png)* charge the tally's probe counter per entry examined
- *(png)* keep the byte-exact encoding when cleanup ties on size
- *(png)* race the chunk-free reduction the raw estimate eliminated
- *(png)* resolve a background against the written palette's alpha and origin
- *(png)* count chunks, not segments, against max_chunks
- *(png)* bound the filter scan's inflation by the stream that claims it
- *(png)* emit bKGD and sBIT for the colour type actually written
- *(png)* race the cleaned encoding instead of assuming it wins
- *(png-cli)* say why the filter scan was skipped
- *(png)* budget the quantity the decoder budgets
- *(png)* emit a filtered row when every filter candidate ties
- *(png)* keep the palette only when it is actually smaller

### Other

- *(png)* bring gamut-png and gamut-deflate inside the rustdoc and README-doctest gates ([#803](https://github.com/visualcommons/gamut/pull/803))
- *(png)* adopt gamut-core's shared Segment and covers tiling primitive ([#761](https://github.com/visualcommons/gamut/pull/761))
- *(png)* pin both ends of the top rung's parse span
- *(png)* record the row the top rung's parse span costs bytes on
- *(png)* measure the parse span where it is live, and where a rung buys nothing
- *(png)* print the size table the ladder's documented command promises
- *(png)* measure what the Small to Smallest step costs time from
- *(png)* state the ordering the ladder can actually keep
- *(png)* wrap an assert the formatter reflows
- *(png)* record what each rung of the effort ladder costs and buys
- *(png)* cite tRNS and bKGD by the vendored 3rd edition's numbering
- *(png)* describe the truncate-form trim and narrow the palette cost claim
- *(png)* renumber the background unconditionally
- *(png)* say which background forms cleaning keeps, and name OPAQUE once
- *(png)* record what cleaning a caller's palette buys
- *(png)* give the trailing-opaque tRNS trim one owner
- Merge remote-tracking branch 'origin/feat/440-png-cabx' into fix/483-png-metadata-preservation
- *(png)* resolve the two intra-doc links this change broke, and name itxt_entry
- *(png)* let the notice list stay the enum's, and name both payloads a null drops
- *(png)* name the conformance clause, and stop contradicting the colour record
- *(png)* cite the sections the vendored spec actually numbers
- *(png)* say whose job the colour-chunk ranking is
- *(png)* pin both readings of an XMP chunk's iTXt framing
- *(png)* correct what preservation carries and what it only reports
- *(png)* give the C2PA store-location rule one owner
- Merge remote-tracking branch 'origin/feat/224-png-encoder-efficiency' into feat/440-png-cabx
- *(png)* cite §11.3 chunk clauses by the vendored 3rd edition's numbering
- *(png)* align C2PA read rule, fill step and counts with the code
- *(png)* format the missing-IEND accounting test
- *(png)* pin is_intact's IEND conjunct with a clean-EOF datastream
- *(png)* point the colour-key race tests at reduce::Reductions
- *(png)* state the tie order's reason as the module doc does, and match the README range to the table
- *(png)* pin what end_carry separates and what a dropped payload is called
- *(png)* record the clauses metadata preservation implements
- *(png)* pin Latin-1 text, the refused chunk pairs, and what a re-encode carries
- *(png)* read the C2PA chunk header as bytes, not offsets
- *(png)* pin the exact fit in the C2PA fill's bounds check
- *(png)* separate C2PA carriage from what a decode admits
- *(png)* name what a metadata walk returns
- Merge remote-tracking branch 'origin/feat/224-png-encoder-efficiency' into feat/440-png-cabx
- *(png)* state the tie-break rule once, without appealing to encode order
- *(png)* say what moving the chunk ceiling into push actually fixed
- Merge remote-tracking branch 'origin/feat/224-png-encoder-efficiency' into feat/440-png-cabx
- *(png)* rustfmt the chunk-ceiling closure and its boundary test
- *(png)* shrink the distinct-type fixture to the count it needs
- *(png)* qualify the bKGD/sBIT contract by axis and state what a bomb still costs
- *(png)* count the tally's lookup probes instead of trusting its shape
- *(png)* a grey sBIT needs all three channels to agree, not two
- *(png)* [**breaking**] record FilterStrategy as non-exhaustive
- *(png)* record what the races cost and how the chunks follow them
- *(png)* pin the chunk tally's constant-time lookup structurally
- *(png)* kill the five mutants the new walk code left alive
- *(png)* correct the cost model and the size claim against the encoder
- *(png)* pin the greyscale colour key the race declines
- *(png)* drop a palette sort key that cannot change the order
- *(png)* pin the walk's ceilings, its saturation and the unread verdict
- *(png)* pin the cleanup tie-break and the entropy weighting
- merge origin/master into feat/224-png-encoder-efficiency
- *(png)* drop a stray blank line in the size contract
- *(png)* say which efficiency tables are gated and which only report
- *(png)* derive every size budget from its measurement
- *(png)* delete choose_min_sum_abs
- *(png)* re-pin sub-byte indexed auto-reduce
- *(png)* pin the size race a colour key can lose
- *(png)* cover the greyscale colour key
- settle the counter rule, the png authority row and two counts
- *(png)* index the chunk tally by type
- *(png)* separate palette ordering from discovery order
- *(png)* make the colour-key arms total instead of unreachable
- *(png)* read the bigram index as one big-endian pair
- *(png)* pin the tRNS overhead term with a translucent palette
- Merge pull request #486 from visualcommons/chore/110-survey-tier1-rest
- *(png)* close the remaining survivors of the first survey
- *(png)* pin the encoding analyze8 chooses, not just that it reduces
- *(png)* refresh the efficiency tables after palette ordering
- *(png)* record the colour key in the axis scorecard
- *(png)* close the mutation gaps in the new efficiency code
- record the gamut-png efficiency baseline and the benchmarking convention
- *(png)* accelerate CRC-32 and the scanline filter loops
- *(png)* pin the output size against libpng at maximum compression
- *(png)* benchmark encode size, bpp and per-stage throughput
- *(png)* expose the encoder stages behind test-support
- deny unsafe in the hot-path crates instead of forbidding it
- *(png)* pin each auto-reduce claim separately
- adopt as_chunks for constant-size slice chunking
- merge origin/master into feat/268-pixel-conversion
- close the mutation-testing gaps in the conversion paths
- *(png)* close the mutation gaps in the reduce/decoder extension

## [0.1.0](https://github.com/justin13888/gamut/releases/tag/gamut-png-v0.1.0) - 2026-07-18

### Added

- *(png)* ancillary raw-payload exposure via a rich decode entry point
- *(png)* Adam7 de-interlacing
- *(png)* palette, tRNS, and indexed decode
- *(png)* scanline defilter and raw-sample decode for non-interlaced images
- *(png)* chunk-stream parser and IHDR validation
- *(png)* wire PNG into the umbrella crate and the CLI
- *(png)* add lossless reductions and brute-force filter selection
- *(png)* embed EXIF, ICC profiles, and XMP metadata
- *(png)* add standard ancillary chunks (colour, physical, timing, text)
- *(png)* add sub-byte bit depths (1-bit bilevel, 1/2/4-bit indexed)
- *(png)* add indexed colour with PLTE and palette transparency
- *(png)* support all non-indexed colour types at 8 and 16 bits
- *(png)* add the five scanline filters with MinSumAbs selection
- *(png)* scaffold gamut-png with RGB8 keystone + libpng oracle

### Other

- *(png)* close every diff-scoped mutation gap
- reflect the gamut-png decode surface in the workspace README and AGENTS.md
- *(png)* malformed-input rejection corpus
- *(png)* libpng differential conformance suite over generated fixtures
- apply nightly rustfmt import grouping across the workspace
- *(mise)* port justfile recipes to mise tasks
