# gamut-ifd — TIFF/IFD container core implementation status

`gamut-ifd` factors the TIFF Image File Directory structure (`references/tiff/tiff6.pdf` §2, plus
`references/tiff/bigtiff.html` for BigTIFF) out as a shared primitive consumed by both
[`gamut-tiff`](../gamut-tiff) (the TIFF codec, issue #107) and [`gamut-exif`](../gamut-exif) (EXIF
metadata, issue #34). It models *structure only* — byte order, field types, values, the IFD chain,
and the offset-driven read/write spine — never pixels, compression, or photometry.

**Keystone:** the **two-pass offset layout** in the writer ([`write`](src/writer.rs)). Out-of-line
values and following IFDs need absolute offsets that are only known after sizes are fixed, so the
writer plans the layout then back-patches the offset words; a read → write → read round-trip
reproduces the directory exactly. At v1 the symmetry extends to whole sub-IFD trees:
`read_tree(&write(&file)?, tags)? == file`.

## How it was built

The structural core was migrated from `gamut-tiff`'s self-contained IFD implementation (issue #107):
`gamut-tiff` was developed first with an inlined IFD reader/writer, and the type names here were
authored to mirror it, so the move was near-zero-diff. `gamut-tiff` now consumes this crate (with
the `bigtiff` feature) instead of its own copy; its libtiff differential oracle exercises these exact
read/write code paths byte-for-byte.

## Phases

| Phase | Spec § | Scope | Status |
| ----- | ------ | ----- | ------ |
| P1 | — | Scaffold: crate, workspace wiring, docs, region-free data-model skeleton | ✅ done |
| P2 | §2 | Header + single-IFD reader: II/MM byte order, magic, entry decode for all 12 field types | ✅ done |
| P3 | §2 | Value resolution: inline (≤ offset width) vs out-of-line offsets; multi-IFD chains (`next` links) | ✅ done |
| P4 | §2 | **Keystone** — writer with two-pass offset layout + back-patching; read→write→read round-trip | ✅ done |
| P5 | §2 | Sub-IFD pointers + nested directories (the SubIFDs/Exif/GPS offset-tag pattern) | ✅ done (#109 write, #181 read) |
| P6 | §2 | Robustness: offset-loop / overlap / truncation guards + malformed-input corpus | ✅ done (#181) |
| P7 | — | libtiff/exiv2 differential oracle gate (via the consuming codecs — see below) | ✅ via codecs |
| P8 | — | BigTIFF (8-byte offsets/counts, `Long8`/`SLong8`/`Ifd8`) — gated `bigtiff` feature, additive | ✅ done |
| P9 | §2 | RAW-grade streaming: `ReadAt` sources (slice / `Read + Seek` / rebased), lazy `IfdReader`, structural `tags` | ✅ done (#252) |
| P10 | §2 | **Byte completeness** (2.0 reshape): lossless `Value::Unknown` model, one-parser collapse, dual-ledger `Tracked`+`SegmentMap` audit, writer-declared padding + pinned spans | ✅ done (#263) |
| P11 | C2PA §A.3.6, §18.5.5 | **C2PA manifest-store carriage** (`c2pa`): the tag, the last-main-IFD placement rule, end-of-file placement by post-write relocation, the two-range exclusion set, and the read-side locator — shared by the TIFF-based codecs | ✅ done (#442) |

P5's **write** side landed with the DNG codec (issue #109): [`write`](src/writer.rs) lays out the
whole IFD *tree* — [`Ifd::set_sub_ifd`](src/entry.rs) attaches children under a pointer tag
(`SubIFDs`/`ExifIFD`/…), the writer places them and synthesises the offset-array field. The **read**
side closed with v1 (issue #181): [`read_tree`](src/reader.rs) follows caller-named pointer tags
(the generic [`read`] cannot know which `LONG` tags are offsets) with depth and cycle guards,
rebuilding the tree `write` flattens; [`read_ifd_at`](src/reader.rs) stays as the per-pointer escape
hatch for lenient decoders (gamut-dng skips unparseable children while hunting the raw IFD,
gamut-exif tolerates malformed sub-IFDs in lenient mode).

P6's corpus ([`tests/robustness.rs`](tests/robustness.rs)) drives `read`/`read_tree` over specific
malformed inputs, a full truncation sweep, an LCG byte-flip fuzz, and an exhaustive single-byte
overwrite sweep, on both variants — and immediately caught (and fixed) a hostile-BigTIFF entry-count
multiply overflow.

The read ledger's laws are stated executably in [`src/invariants.rs`](src/invariants.rs) — the
workspace's first `invariants` module (issue #240, `docs/testing.md`): plain functions checking that
`record` produces a canonical span set and that `subtract` is the set difference in canonical form,
driven here by a pinned-seed `proptest` and, once #264 lands, by the out-of-tree fuzz driver through
the `test-support` feature. Writing the law once is what keeps the two lanes from drifting. It
immediately retired a mutation exclusion that had been recorded as *provably equivalent*: the
`r.len > 0` claim filter in `ReadLedger::subtract` is not — a zero-length claim inside an unclaimed
stretch makes the walk emit two **adjacent** ranges where one is correct. The byte set is unchanged,
which is why every set-equality test missed it; the canonical form is not.

P7 is satisfied **through the consuming codecs**, deliberately: `gamut-tiff`'s libtiff oracle
round-trips real TIFF containers byte-for-byte through this crate's reader/writer (including
BigTIFF, multi-IFD, and sub-IFD structure), and `gamut-exif`'s exiv2 oracle parses/round-trips bare
TIFF streams through the same paths (the 0th IFD and the Exif sub-IFD behind the pointer). A direct
oracle dev-dependency here would drag the C-oracle toolchain into this crate's test cycle without
exercising any path the codec gates do not already cover byte-exactly.

## v1 surface (issue #181)

The API was frozen after a full-surface review; the additions and breaks:

- **Spec fix** — multi-string ASCII/UTF-8 values (TIFF 6.0 §2) round-trip: decode strips exactly
  the terminating NUL and preserves interior separators.
- **Fallible `write`** — classic-width overflow (entry count > `u16::MAX`, layout > 4 GiB) is a
  typed error instead of silent truncation; the layout contract (word alignment, structural
  determinism) is documented API.
- **`read_tree`** — write's inverse over sub-IFD trees; sub-IFD groups are tag-sorted (canonical).
- **`Ifd::remove`**, **`Value::as_str`/`as_bytes`/`as_rationals`/`as_srationals`**,
  **`Value::offset_array`**, **`align_word`** — the pieces every consumer had hand-rolled.
- **Single canonical paths** — modules are private; the surface is the crate-root re-export list
  (plus, since P9, the [`tags`](src/tags.rs) module — the one *named* module, holding the
  structural pointer-tag constants).

## P9 — RAW-grade streaming (issue #252)

Downstream RAW decoding (rawshift's ARW/CR2/NEF/DNG engines) parses multi-hundred-MB camera
files whose IFD structure is kilobytes; loading the file to hand `read` a slice is the wrong
shape. P9 adds the streaming layer **additively** — the frozen v1 slice surface is untouched:

- [`ReadAt`](src/source.rs) — positioned exact reads + length; implemented by `&[u8]`,
  `StreamSource<R: Read + Seek>`, and `Rebased<S>` (the offset-shifting adapter maker-note
  mini-IFDs and embedded TIFF blocks need). Transport failures surface as the new
  `gamut_core::Error::Io`; a stream that ends early stays `InvalidInput`, like a short slice.
- [`IfdReader`](src/stream.rs) — lazy walker: `read_ifd` fetches one directory body into raw
  entries (value/offset words verbatim, in file byte order); `value` fetches/decodes one value
  on demand, span-checked against the source length *before* allocating; `ifds()` iterates the
  chain; `read_file` / `read_tree` / the coverage methods mirror the slice APIs exactly.
- [`tags`](src/tags.rs) — the structural pointer tags (`SubIFDs`, `ExifIFD`, `GPSInfo`,
  `InteroperabilityIFD`; `MakerNote` as the named-but-not-followable blob carrier), deduplicating
  the three consumers' copies. The one sanctioned exception to "no tag semantics": these tags
  name the directory graph itself.

The guard story: the chain loop/length guard (`ChainGuard`) and the sub-IFD depth/cycle guards
(`resolve_pointers_with`) are *shared* with the slice path; since P10 the slice functions are
thin wrappers over the streaming engine (**one parser**), and `tests/robustness.rs` keeps
driving both entry points over the hostile corpus as a regression gate on the wrappers. The
streaming path is u64-native end to end (no `as usize` truncation of hostile 64-bit widths).
Layout requirement 3 of #252 (deterministic write offsets) was already the P4 keystone contract;
`tests/streaming.rs` pins the rest of the capability surface (three source shapes × orders ×
variants, audited-walk classification, the ≤64-read-bytes laziness contract, the maker-note
pattern).

## P10 — byte completeness (issue #263, the 2.0 reshape)

Issue #263 gates rawshift's TIFF/DNG migration: its parser explicitly accounts for every byte,
and this crate had to be verified — and where verification failed, fixed **structurally** — to
match. The audit found three architectural flaws, each closed by construction rather than by
convention:

- **The model was lossy.** Unknown field-type entries were skipped at read, so a read → write
  cycle silently dropped them. Now `Value::Unknown` preserves the whole entry record verbatim
  (type code, declared count, raw value/offset word in its captured byte order); the writer
  re-emits it bit-exactly and refuses the one impossible operation (transcoding the opaque word
  across a byte order/variant change). `FieldType::Ifd` (classic 13, TIFF TechNote 1) joined
  the known set. `tests/fidelity.rs` pins the whole matrix.
- **Accounting was opt-in.** Coverage marks were manual calls nothing tied to the parse. Now
  accounting is **dual-ledger**: a `Tracked` source records every byte physically read, the
  parser makes typed `SegmentMap` claims (`SpanKind` × `Parsed`/`Declared`), and `finish`
  cross-checks the two — a parse path that reads without claiming, or claims without reading,
  is caught mechanically (`unclaimed_reads`/`unread_claims`; the robustness corpus asserts the
  invariant on every successful parse). `audit`/`Auditor` drive the whole-tree walk, with
  `walk_embedded` for rebased embedded streams (DNG camera profiles, maker-note mini-IFDs).
- **Padding was tolerated, not classified.** Consumers allowed "≤ N gap bytes". Now `write_with`
  returns a map declaring every emitted byte (padding included, fully classified by
  construction), the audit reclassifies only byte-inspected plausible zero-fill, and
  `SegmentReport::is_fully_classified` is a zero-tolerance verdict. `write_with` also **pins**
  named values at exact absolute offsets — the maker-note preservation primitive — and **reserves**
  a leading vendor preamble, the positional counterpart (`WriteOptions::preamble`, #350).

### Intentional drops (the complete ledger)

1. **Unknown-type out-of-line payloads.** An unknown field type's element size is unknowable,
   so if its value/offset word was an offset, the pointed-at bytes cannot be sized, fetched, or
   relocated. The entry record round-trips verbatim; the payload bytes surface as unclassified
   in an audit (the honest signal), and after a relocating rewrite the word may dangle.
2. **Layout re-canonicalisation.** `write` produces a canonical layout (word-aligned, tag-sorted,
   tight value pool): original offsets, entry order, and padding of arbitrary *input* are not
   reproduced — `read(write(f)) == f` and the audit closed loop are the contract, not whole-file
   byte identity of foreign input. Pinned spans (`WriteOptions::pinned`) are the per-value
   escape hatch, and `WriteOptions::preamble` is its counterpart for the one *positional* run a
   rebuilt layout can reproduce — the header/first-directory gap a vendor signature sits in
   (issue #350).
3. **Cross-order/variant transcoding of unknown-type values is refused** (a typed error), since
   the opaque word's meaning cannot be re-encoded.

Dependency evaluation (the issue asked): binrw/deku (declarative sequential parsers) cannot
express the runtime-endian offset *graph* with hostile-input guards; zerocopy's endianness is a
type parameter where TIFF's is runtime data; winnow/nom model streams, not random access;
`rangemap` would replace only the ~120-line interval coalescer while the bespoke semantics
(claim provenance, sharing dedupe, conflict kinds) still need wrapping. The crate stays
zero-dependency; the guarantee comes from the dual-ledger architecture, not a parser library.

### Hardening audit (issue #262, subsumed by the one-parser collapse)

The #262 audit against rawshift's `ParseError` acceptance checklist (magic/byte-order validation,
IFD/value offset bounds with the two-error offset-vs-span distinction, circular-IFD and sub-IFD
guards, checked u64 offset arithmetic, truncated-file staging, and report-not-reject overlaps) is
now satisfied **by construction**: the slice functions are thin wrappers over the u64-native
streaming engine, so there is one code path to harden rather than two mirrored ones. Its finding —
a slice reader that cast u64 counts/offsets to `usize` before its checked arithmetic, truncating a
hostile 64-bit BigTIFF width into a silent in-bounds misparse on 32-bit targets — cannot recur:
the streaming engine stays u64 until a bound against the source length proves each conversion
lossless. [`tests/hardening_audit.rs`](tests/hardening_audit.rs) remains the acceptance artifact,
pinning the exact `Error::InvalidInput` string rawshift keys its `ParseError` mapping on for every
checklist case; per the issue ("correctness verification, not an API ask") error granularity stays
`InvalidInput(&'static str)` — no per-case error variants were added.

## P11 — C2PA manifest-store carriage (issue #442, epic #239)

The C2PA manifest store is the one cross-format payload a TIFF-based file carries *by tag*
(C2PA 2.4 §A.3.6: tag 52545 / `0xCD41`, type `UNDEFINED`), and its placement rule is unusual
enough — one store per asset, its entry in the **last IFD of the main chain**, its bytes at the
**end of the file** so a resize moves no other offset — that stating it once here, for both
`gamut-dng` (#442) and `gamut-tiff` (#446), beats two derivations. The [`c2pa`](src/c2pa.rs)
module is that one statement; `references/c2pa/README.md` is the clause map. The store stays
opaque bytes: nothing here parses the JUMBF interior or reaches a verdict.

- **Placement is a post-write relocation, not a new writer mode.** A codec that appends image
  data after `write`'s stream cannot get a value placed *after* that data from the value pool,
  and a pinned span would need the data's extent before the layout it depends on exists. So
  `reserve_entry` puts a one-byte inline placeholder in the last main IFD (the directory layout
  is final; the pool is untouched), the codec writes its whole file, and `append_store` lands
  the store at `align_word(len)`, patching only the entry's `count` and value/offset words. The
  alternative — a `WriteOptions` directive placing a tag's value at the end of the *stream* —
  would still sit before the codec's pixel data, which is not "the end of the file".
- **The exclusion set is two ranges** (§18.5.5): `C2paExclusions { store, count_field }`, the
  count field being the offset-width word at entry offset 4 (4 bytes classic, 8 BigTIFF). They
  are disjoint by construction, and reported in the crate's own `Range` (u64 start/len) — the
  same measure the segment map uses — rather than `core::ops::Range<usize>`.
- **Read side.** `locate` walks the chain to its last directory over any `ReadAt` source and
  reports the ranges for an out-of-line *or* inline store; a tag-52545 entry of another type, or
  one in a non-last directory, is "no store" (never an error), so a decoder can still surface
  it as an unmodelled field. A store declared past the end of the file is `InvalidInput`, the
  verdict `read` gives the same file.
- **Endianness.** §A.3.6 says the header's `ByteOrder` "does not govern the endianness of the
  embedded C2PA Manifest Store": the bytes cross verbatim in both directions, pinned on `MM`
  fixtures whose store is asymmetric.
- **Minimum length, and the bound that actually keeps a store out of line.** Nothing shorter
  than a JUMBF box header (8 bytes, `MIN_STORE_LEN`) can be a manifest store, and the two
  directions treat that differently, as `references/c2pa/README.md` prescribes for a reader:
  `locate` reports **absence** for a shorter value (so a foreign file stays readable and a
  decode → encode cycle over it does not trip the encoder's own minimum), while `append_store`
  **refuses** it (an encoder handed a store it cannot write must say so, not drop it).
  Out-of-line-ness is a *separate* bound and is not implied by that constant: classic TIFF's
  inline threshold is 4 bytes but **BigTIFF's is 8**, so a store of exactly `MIN_STORE_LEN`
  packs inline in a BigTIFF entry. `append_store` therefore gates on the variant's own
  `inline_threshold()` — the shortest writable BigTIFF store is nine bytes. Without that gate
  the bytes would be appended at the end of the file while the entry read back as the offset
  word pointing at them, leaving the store referenced by nothing and the exclusion ranges
  covering bytes no reader returns.
- **One store per asset.** §A.3.6 admits exactly one, so a last IFD carrying *two* tag-52545
  entries names none and `locate` reports absence. Reporting the first would be worse than
  reporting nothing: the eager `Ifd` keeps the **last** duplicate, so a caller taking bytes from
  one path and ranges from this one would get two different byte runs under one name.
- **Byte accounting.** The store is the entry's `Value` span and the alignment filler before
  it is `Padding`, so an audited read of the result is fully classified — a store at the end of
  the file is never a `Trailer`.

Deliberately not here: a `TiffFile`-level "reserve the entry in the right IFD" helper. The
codecs hold their last main IFD by hand (a DNG has exactly one), and picking it out of a chain
is a one-liner nobody would get wrong.

`C2paExclusions` is `#[non_exhaustive]`: §18.5.5 names two ranges today, and a revision naming a
third must be additive rather than a `gamut-ifd` major. It also carries a public
`C2paExclusions::new` — the attribute alone would leave a host that places a store by its own
route unable to name the ranges §18.5.5 asks it to exclude, and extensible and constructible are
both available.

**The read and write sides are deliberately asymmetric.** A BigTIFF store of exactly
`MIN_STORE_LEN` bytes packs inline; `locate` reports it (the file is lawful and its ranges are
well defined) while `append_store` refuses to *write* that shape, because an inline value is not
the run at the end of the file the placement rule is built on, and admitting it would give a
store two placements to reason about for no gain. Liberal in, conservative out — stated at both
functions so it reads as a decision rather than an oversight.

**Accepted duplication.** `gamut_heic::c2pa::JUMBF_HEADER_LEN` states the same 8-byte JUMBF box
header bound as `MIN_STORE_LEN` here. The dependency graph gives the two no shared home —
`gamut-ifd` sits *below* `gamut-heic` and neither may depend on the other — and both cite the
same clause (C2PA 2.4 §8.4.2.3's incidental description, recorded in
`references/c2pa/README.md`). Factoring it out would mean a new crate for one integer.
