# gamut-png — PNG codec status

Tracking GitHub issues #24 (encoder) and #249 (decoder): a research-grade, space-efficient PNG
**encoder** on par with the best PNG encoders, and a spec-compliant **decoder** for the rawshift
migration off `zune-png`. Delivered as small, individually green phases (each
`mise run test`/`lint`/`fmt-check`/`coverage` ≥80%).

**Keystone:** the signature → IHDR → IDAT → IEND pipeline with filter-None 8-bit RGB (P2) — once
libpng decodes that pixel-exact, each later phase swaps in another colour type, a filter, or a
space optimisation behind the same chunk spine and CRC.

**Oracle:** differential vs **libpng** (`tooling/libpng-oracle` + `third_party/libpng`, dev-only
FFI), in both directions: libpng decodes the encoder's output → pixel-exact with the source, and a
libpng reference-encode entry point generates the decoder's conformance fixtures (interlaced,
sub-byte, forced-filter, metadata-laden) that gamut-png and libpng must decode identically. Output
size is measured against libpng at zlib level 9 by `cargo bench -p gamut-png` and enforced by
`tests/size_contract.rs` (see [Efficiency](#efficiency-issue-224)).

**Out of scope:** Adam7 *encoding*, animation/APNG (gamut is image-first; the decoder reads an
APNG's default image). Format-agnostic pixel conversion (grey↔RGB, alpha, 16↔8-bit) is
[`gamut_core::convert`]'s (issue #268), not this crate's: the decoder resolves what only PNG knows —
palette lookup, folding a tRNS key into a real alpha channel, §13.12 sub-byte scaling — and hands the
layout change to the shared engine. A typed decode is lossless by default; `PngDecoder::convert_policy`
opts into narrowing. That is distinct from the encoder's *lossless* auto-reduce (#338), which demotes
16→8 only when every sample is exactly `k·257` and drops alpha only when fully opaque.

## Phases

| Phase | Spec | Scope | Status |
| ----- | ---- | ----- | ------ |
| P1 | §5, §11.2.1 | Scaffold + workspace wiring + libpng-oracle/submodule; CRC-32; chunk writer + signature; `ColorType` + bit-depth matrix; IHDR | ✅ done |
| P2 | §6, §9, §11.2.4 | **Keystone:** `EncodeImage<Rgb8>`, filter None, DEFLATE → signature/IHDR/IDAT/IEND | ✅ done |
| P3 | §9 | All 5 scanline filters (None/Sub/Up/Average/Paeth) + `MinSumAbs` selection | ✅ done |
| P4 | §6.1 | Colour types: Gray8/Gray16/Rgb16/Rgba8/Rgba16/GrayAlpha8/16 (16-bit big-endian) | ✅ done |
| P5 | §11.2.2/§11.3.2 | Indexed (`encode_indexed8` + PLTE + tRNS), 8-bit | ✅ done |
| P6 | §7.2 | Sub-byte depths: 1-bit bilevel grey + auto-minimal-depth indexed (1/2/4) | ✅ done |
| P7 | §11.3 | Standard ancillary chunks: gAMA/cHRM/sRGB/sBIT/bKGD/pHYs/tIME/tEXt/zTXt/iTXt | ✅ done |
| P8 | §11.3 | Metadata: eXIf, iCCP (deflate-compressed), iTXt-XMP (raw-bytes setters) | ✅ done |
| P9 | §4.5 | **Space opt:** lossless palette/gray/alpha-drop reduction (size-estimate chosen) + brute-force filter strategy; extended to grey/grey-alpha/16-bit inputs with lossless 16→8 demotion and sub-byte grey packing (#338) | ✅ done |
| P10 | — | CLI `gamut convert → .png`; umbrella `png` feature; final API review | ✅ done |
| E1 | #224 | **Efficiency:** `deconstruct` byte accounting; divan size/bpp + per-stage bench; libpng-9 size contract; opt-in transparent cleanup; palette-vs-native race; `crc32fast` and restructured filter kernels (see [Efficiency](#efficiency-issue-224)) | ✅ done |
| C1 | C2PA 2.4 §A.3.2, §18.5.4 | **C2PA carriage** (#440): the `caBX` manifest store — raw decode surface (`c2pa`; first CRC-valid chunk before `IDAT` wins, ignored ones counted, under the metadata budget); `with_c2pa` / `with_c2pa_reserved` as the last chunk before `IDAT`; the whole-chunk exclusion span from `encode_with_report` and `PngReport::c2pa`, filled in place by `fill_c2pa` (see [C2PA](#c2pa-manifest-store-issue-440)) | ✅ done |
| M1 | §4.3, §11.3.2.6, §11.3.3 | **Metadata preservation** (#483): `with_metadata` / `with_metadata_from` carry a read file's eXIf/iCCP/sRGB/cICP/gAMA/cHRM/XMP/text chunks into a re-encode, each annotation back into the chunk it came from and the XMP packet back into the framing its `iTXt` gave it (`gamut convert` uses it; `--strip-metadata` opts out; what could not be carried faithfully is named by `metadata_notices`); `with_cicp`; §11.3.3.2/§11.3.3.4's null prohibition refuses the encode and §11.3.3.1's advisory keyword rules report through the notice channel, with promotion to `iTXt` for text outside Latin-1 (see [Metadata preservation](#metadata-preservation-issue-483)) | ✅ done |

## Decoder phases (issue #249)

| Phase | Spec | Scope | Status |
| ----- | ---- | ----- | ------ |
| D1 | §5, §11.2.1 | libpng-oracle reference *encode* entry point (fixture generator); chunk-stream parser + CRC policy; IHDR validation | ✅ done |
| D2 | §9, §10 | `PngDecoder` + decode limits (dimensions, byte budget); bounded zlib inflation (`miniz_oxide`); scanline defilter; non-interlaced typed `DecodeImage` matrix (lossless widening via `gamut_core::convert`) | ✅ done |
| D3 | §11.2.2/§11.3.1 | Palette + tRNS: `PngPalette::from_chunks`, index range checks, `DecodeImage<Indexed8>`, RGB(A) expansion, colour keys | ✅ done |
| D4 | §8.1, §13.10 | Adam7 de-interlacing (per-pass defilter/unpack, empty passes, checked stream-length sum) | ✅ done |
| D5 | §11.3 | Rich `decode()` → `DecodedPng`: raw eXIf/iCCP/XMP/text payloads (MetadataBlock-ready), parsed gAMA/cHRM/sRGB/cICP, metadata inflation budget | ✅ done |
| D6 | — | libpng differential conformance suite over generated fixtures; malformed-input rejection corpus; mutation-gap closure | ✅ done |
| D7 | §5, §11.3 | Pixel-free metadata entry point (issue #379): `metadata()` / `PngDecoder::metadata()` → `PngMetadata`, sharing one chunk-classification predicate with `decode()`; IDAT skipped by length, never read or inflated. Mirrors `gamut_jpeg::metadata` / `gamut_webp::metadata` | ✅ done |

## C2PA manifest store (issue #440)

Part of epic #239. gamut **locates, bounds, carries and reserves** the C2PA manifest store; it
never parses or judges it. The store is opaque bytes plus byte ranges here, and validation is
`c2pa-rs`'s (`references/c2pa/README.md` draws the boundary).

**Carriage.** The store is the data of a `caBX` chunk, uncompressed (C2PA 2.4 §A.3.2). The chunk
type is spelled once, in `chunk::CABX`, and its *property bits* are asserted rather than only its
letters: ancillary and private (bit 5 set on bytes 0 and 1), reserved bit clear, and — the point —
**unsafe to copy** (bit 5 *clear* on byte 3; PNG §5.4 Table 6 gives that polarity, and the issue's
prose had it backwards). A PNG editor that rewrites the image must drop an unrecognised
unsafe-to-copy chunk, which is the container enforcing the same no-copy-forward law
`gamut-metadata`'s `C2paPolicy` states for the facade: a store is bound to the bytes it was signed
over, so one copied forward into a rewritten file is invalid by construction.

**What counts as the store.** One rule, in every reader: the **first CRC-valid `caBX` before the
first `IDAT`**. First, because a file carries exactly one store (§A.3.2) — PNG has no multi-chunk
store, unlike JPEG's APP11 run. CRC-valid, because §13.1 makes a mismatch skippable and the decode
skips it. Before `IDAT`, because §A.3.2 puts it there and calls data after it bad-form: a `caBX`
appended to a finished file is not that file's provenance, and accepting one would let an appender
give a store to a file that carries none.

**Decode.** `DecodedPng::c2pa` / `PngMetadata::c2pa` carry that chunk verbatim, ready for
`MetadataBlock::C2pa`. The store is attacker-sized like every ancillary payload, so its bytes are
charged to the one cumulative `with_max_metadata_bytes` budget; a store past the remainder is
skipped, not an error, and — skipped — is still the file's first store, so a smaller one after it
is ignored rather than substituted.

`c2pa_ignored` counts every CRC-valid `caBX` **in the datastream** that was not surfaced as the
store (a `usize`: the file's real number, not a saturated ceiling). That is three cases — a chunk
later than the first, one positioned after `IDAT`, and the store-position chunk itself when it
busted the budget — and the count deliberately does not say which: `c2pa == None` with a non-zero
count is any of them, not evidence of an appended store. What it does **not** cover is a `caBX`
after `IEND`, which is a trailer rather than part of the datastream (§13.2) and which neither
metadata walk reaches; that shape is visible in `deconstruct`'s report, as a trailer segment.

**Encode.** `with_c2pa(store)` embeds a store computed for this file; `with_c2pa_reserved(len)`
writes `len` zero bytes in its place. Either is emitted as the **last** chunk before the first
`IDAT` — after `PLTE`/`tRNS` and every other ancillary chunk — so the chunk's offset depends only
on what precedes it and every later byte is `IDAT`/`IEND`. §A.3.2 asks only that it precede
`IDAT`; last-before-`IDAT` is what makes the reserve-then-fill flow a no-move: encode with the
reservation, hash with the chunk's span excluded, then encode again with the finished store of the
same length — the output is byte-reproducible, so the second file differs from the first only in
the payload and the chunk CRC. `tests/c2pa.rs` pins that as an exact-byte diff.

**Exclusion span, and filling it.** `encode_with_report` (for the file just written) and
`PngReport::c2pa` (for any file, including an indexed encode) name the chunk's **whole** span —
length, type, payload and CRC — as `C2paSpan`, with the payload bracketed inside it. §18.5.4 says
the length and type go inside the exclusion; the CRC must too, since it changes with the payload,
and a `c2pa.hash.data` computed over any of them breaks on the store's first write. The span is
derived from the same chunk walk the byte accounting uses, so it is always one of the report's
claimed segments.

`fill_c2pa(&mut png, &span, store)` then writes the finished store into that span in place,
rewriting the payload and the chunk CRC and nothing else — O(store) rather than the O(encode) of a
second `with_c2pa` pass, and without tying the signature to the encoder reproducing its output.
Its arguments are validated first (span inside the image, framing a chunk, naming a `caBX`, store
exactly the reserved length), so a rejected call leaves the file untouched rather than half
filled.

A span is **carriage**, not a decode result. The report has no byte budget, so a store past
`with_max_metadata_bytes` is still spanned here while `decode().c2pa` is `None`; likewise
`chunk(b"caBX").count` counts CRC-invalid chunks and chunks in the trailer, which `c2pa_ignored`
does not. Each number answers its own question, and the docs say so rather than promising they
agree.

**Placement is ours, not the format's.** The store is written last before `IDAT` so its offset
depends only on what precedes it — the property the reserve-then-fill flow rests on. PNG §14.3.2
warns that ordering relative to *other ancillary chunks* is never guaranteed and an editor may
insert one after ours, so "last" describes files as this encoder wrote them; readers assume only
"before `IDAT`".

**Oracle.** libpng has no C2PA support and carries `caBX` as an unknown chunk — which is exactly
the proof needed for framing: for the same payload it must produce the same length, type and CRC
bytes as gamut, it must decode gamut's file pixel-exact with the chunk in place, and gamut must read
the store from a libpng-written file. The behavioural oracle (`c2pa-rs`, against which a store's
hash assertion can be checked over the excluded span) is issue #447.

**Not done, by design.** No JUMBF parsing, not even of the outer box length. No validation verdict
of any kind. `gamut convert` does not carry a store across a re-encode (that is the facade's
`C2paPolicy` law, and the CLI's own path is #448/#483).

## Metadata preservation (issue #483)

The read side has surfaced every metadata payload since D5, and the write side has accepted every
one since P8, but nothing joined them: a re-encode dropped all of it, so `gamut convert`'s PNG
path round-tripped 0% of a file's metadata.

`PngEncoder::with_metadata(&PngMetadata)` and `with_metadata_from(&DecodedPng)` are that join —
one private borrowed view behind two entry points, so the pixel-free `metadata()` walk and a full
`decode()` reach it without copying a large ICC profile twice. `gamut convert` uses it on the PNG
output path; `--strip-metadata` is the opt-out. **Preserve is the default**: a stripped file is
smaller, but dropping an ICC profile silently changes what a viewer paints, so the loss is the
thing that has to be asked for. Carrying the same metadata twice carries it once — the text list
is replaced, not appended to, so the single-value colour slots and the annotations are idempotent
alike.

**Identity, not just content.** `TextChunk::kind` records which of §11.3.3's three chunks carried
an annotation and whether its text was compressed, and a carry puts it back in the same one.
Without it a `zTXt` is indistinguishable from a `tEXt` once decoded, and a compressed 40-byte
payload comes back out as 1 600 uncompressed bytes — no words lost, but not preservation either.

The **XMP packet leaves the read side through its own field**, not through `texts`, so the framing
that field does not hold travels beside it in `XmpFraming`: §11.3.3.4's compression flag, language
tag and translated keyword. §11.3.3.1 Table 21 recommends the null framing for XMP compliance
("with Compression Flag set to 0, and both Language Tag and Translated Keyword set to the null
string") — recommends, not requires, and a provenance packet is exactly the payload a writer
compresses. The measured cost of getting this wrong, on the fixture in `tests/preservation.rs`: a
354-byte `iTXt` rewritten as 3 734 bytes, a factor of 10.6, with the language tag and translated
keyword gone as well. `with_xmp` — which has no source file to take framing from — takes Table 21's
recommended framing. The packet is a **single-value payload** like `iCCP` or `eXIf`: setting it
again replaces it, because a second `iTXt` under the reserved keyword is one this crate's own
reader discards.

Consolidating the packet into `texts` would retire `XmpFraming` and put the packet back in its
file position rather than first among the annotations; it reshapes a public type, so it is
[#600](https://github.com/visualcommons/gamut/issues/600), not this work.

**Two payloads cannot be carried, and neither is dropped in silence.** `metadata_notices()` names
them and `gamut convert` prints them:

- a `cICP` whose matrix coefficients are not 0 — §11.3.2.6 requires 0 for PNG, so the source chunk
  is not conforming and carrying it forward would reproduce the defect;
- the **C2PA manifest store**, signed over the exact bytes of the file it was made for, which is
  why `caBX` is unsafe to copy (C2PA 2.4 §A.3.2). Re-sign the output and set it with `with_c2pa`.

**The colour chunks are carried together, not resolved.** §5.6 Table 5 and §11.3.2.5 say only that
`sRGB` and `iCCP` "should not" appear together — lowercase, and §15 gives the BCP 14 keywords
force "when, and only when, they appear in all capitals" — while §4.3 Table 1 *presupposes* the
co-occurrence and defines the outcome by ranking the chunks (cICP 1, iCCP 2, sRGB 3, cHRM+gAMA 4).
libpng reads a file carrying both and returns the same pixels (`tests/oracle.rs`). So both are
written: dropping either would throw away colour information the source carried, and a reader
takes the one it can use.

That last clause is a claim about **other** readers, not about this crate. Table 1 ranks the chunks
for a reader, and which one to honour depends on whether the reader has a CMM at all — which an
encoder cannot know. gamut-png's own reader surfaces `cICP`, `iCCP`, `sRGB`, `cHRM` and `gAMA` side
by side and ranks none of them; resolving a profile against an intent is `gamut-cmm`'s work
(epic #323), and this encoder deliberately does not pre-empt it.

**Only the null byte refuses the encode. Everything else §11.3.3 asks for is a notice.**
§15 gives the BCP 14 keywords force "when, and only when, they appear in all capitals", and every
statement §11.3.3.1 makes about a keyword's shape is lowercase — "Keywords shall contain only
printable Latin-1", "leading spaces, trailing spaces, and consecutive spaces are not permitted",
"Keywords are restricted to 1 to 79 bytes in length". The same argument that lets `sRGB` and
`iCCP` be carried together applies here, so what separates the outcomes is the *consequence*, not
the wording:

| Field | Clause | Outcome |
| --- | --- | --- |
| A null in a keyword or text string | §11.3.3.2, §11.3.3.4 | **refuses the encode** — the null is the field separator, so the chunk re-parses as a *different* annotation |
| Keyword outside Latin-1, or outside 1–79 bytes | §11.3.3.1 | annotation **dropped**, `TextKeywordNotLatin1` / `TextKeywordLength` — no chunk can hold it, and this crate's own reader drops one that tries |
| Keyword outside `0x20`–`0x7E` / `0xA1`–`0xFF`, or with a leading, trailing or consecutive space | §11.3.3.1 | **written verbatim**, `TextKeywordRepertoire` / `TextKeywordSpacing` |
| `iTXt` language tag outside ASCII letters, digits and `-` | §11.3.3.4 | tag **dropped**, annotation written, `ItxtLanguageTag` |
| XMP packet that is not UTF-8 | §11.3.3.4 | packet **dropped**, `XmpNotUtf8` |

The written-verbatim row is the important one, and it is where an earlier draft of this work got
it wrong. Five keyword shapes — a leading space, a trailing space, consecutive spaces, a C0/C1
control, U+00A0 — are ones this crate's *reader* accepts and returns unchanged. Refusing to write
them back made a re-encode fail on a file whose pixels are fine, and the only escape was
`--strip-metadata`, which discards the ICC profile too. A writer must not be stricter than its own
reader about a clause that is advisory in the first place; `MetadataNotice::carried()` tells a
caller which of these reached the output.

**The specification contradicts itself about a `tEXt` text string, and the more specific clause
wins.** §11.3.3.1's closing paragraph: "There are also tEXt and zTXt chunks, whose content is
restricted to the printable Latin-1 character set plus U+000A LINE FEED (LF)." §11.3.3.2, which
defines `tEXt`: "Text is interpreted according to the Latin-1 character set [ISO_8859-1]. The text
string may contain any Latin-1 character." Both are in `references/png/png-3.html`. §11.3.3.2 is
the more specific and the more permissive, so it is taken: every Latin-1 character is written into
the chunk that already interprets its bytes as Latin-1, and only a character Latin-1 cannot encode
**promotes** to `iTXt` — which is what §11.3.3.2 itself directs ("Text containing characters
outside the repertoire of ISO/IEC 8859-1 should be encoded using the iTXt chunk"), keeping the
caller's compression via §11.3.3.4's own flag. Taking the tighter reading silently changed a
conforming annotation's chunk *type*, which contradicts the identity claim above. The keyword rule
stays as §11.3.3.1 writes it, because that clause is specific to keywords and all three chunks
share it.

**Two spec defects** the same issue found, both in the writer, both fixed:

- *`tEXt`/`zTXt` carried UTF-8.* §11.3.3.2 interprets a `tEXt` text string as Latin-1 and
  §11.3.3.3 makes an inflated `zTXt` identical to it, but the writer pushed the Rust `String`'s
  bytes, storing `C3 A9` where `é` belongs. Text and keyword are now converted once at the setter
  and the entry holds the bytes its chunk carries, so the wrong encoding is unrepresentable rather
  than merely avoided.
- *`iTXt` lost its language tag and translated keyword*, the two fields that make it
  international, and its compression flag.

`with_cicp` (§11.3.2.6) was added with this work — without it, preservation would silently drop the
highest-precedence colour chunk of any file that carries one. It takes no matrix argument: PNG
fixes that byte at 0.

**Not done.** `pHYs`, `tIME`, `sBIT` and `bKGD` are not part of `PngMetadata`/`DecodedPng`, so they
cannot be carried (set them with their own builder methods). The `iTXt` language tag is checked for
its character set, not for full BCP 47 well-formedness (subtag order, registry membership). The XMP
packet rides in its own field beside `XmpFraming` rather than in `texts`, so a carry emits it
**first** among the annotations regardless of where it sat in the source, and the two fields can be
set inconsistently by a caller building a `PngMetadata` by hand — #600. `sPLT` and `hIST` are
surfaced by neither read walk, so they are not carried either. `gamut convert` carries metadata
only PNG→PNG; mapping a JPEG/WebP/JXL input's metadata into PNG chunks is a cross-format job of its
own. The libpng oracle reads no chunk back and drops warnings, so preservation is pinned against
gamut's own reader plus a decode the oracle accepts — #502, #571 and #572 are what would make it
differential.

## Efficiency (issue #224)

Correctness was settled long before efficiency was measured. This section is the measured state:
what the encoder achieves, what it costs, and — per axis — what it does not do yet.

Everything here is produced by `cargo bench -p gamut-png`. What is *gated* is narrower, and
worth being precise about: `tests/size_contract.rs` asserts the size table -- every row including
`tiny_rgb8` and both `+clean` columns -- as a ratio against libpng-9 at 128×128, and pins
`with_transparent_cleanup` never costing bytes on any row. The throughput and per-heuristic tables
below are **reported, not gated**: timings cannot fail a build without making it flaky, which is
why CI runs the benches for compile-rot only ([#437]). One machine, so **read the ratios, not the
absolute times**.

### Output size vs libpng at zlib level 9

256×256 unless noted, gamut at `Level::Best` + `FilterStrategy::BruteForce` + auto-reduce.
`+clean` additionally enables `with_transparent_cleanup`. Lower is better.

| input | raw | default | best | +clean | libpng-9 | best/lp9 | bpp |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `gradient_rgb8` | 196 608 | 2 831 | 1 562 | 1 562 | 2 393 | **−34.7%** | 0.191 |
| `photo_rgb8` | 196 608 | 29 885 | 19 570 | 19 570 | 27 467 | **−28.8%** | 2.389 |
| `noise_rgb8` | 196 608 | 196 983 | 196 983 | 196 983 | 197 280 | −0.2% | 24.046 |
| `grey_as_rgb8` | 196 608 | 721 | 368 | 368 | 566 | **−35.0%** | 0.045 |
| `palette64_rgba8` | 262 144 | 1 274 | 726 | 688 | 1 102 | **−34.1%** | 0.089 |
| `sprite_rgba8` | 262 144 | 4 181 | 3 729 | **2 235** | 3 889 | −4.1% | 0.455 |
| `flat_rgba8` | 262 144 | 821 | 103 | 103 | 664 | **−84.5%** | 0.013 |
| `tiny_rgb8` (16×16) | 768 | 136 | 119 | 119 | 138 | **−13.8%** | 3.719 |

gamut is smaller than libpng-9 on every row, though `noise_rgb8` is a 0.2% near-tie rather than a
win: incompressible input leaves both encoders emitting stored blocks, so that row's budget is the
one deliberately set above parity (1.02) and it is excluded from the win assertion. The margin is
thin where no reduction applies
(`gradient`, `tiny`) or nothing is compressible (`noise`), and large where a lawful
representation change is available that libpng does not attempt.

### Filter heuristics (issue #480)

`BruteForce` tries every whole-image strategy and keeps the smallest, so the size table above
cannot say *which* heuristic earned the win. IDAT bytes at `Level::Best`, each heuristic alone:

| input | MinSumAbs | Entropy | Bigrams | winner |
| --- | --- | --- | --- | --- |
| `gradient_rgb8` | 2 215 | 2 215 | **1 505** | Bigrams |
| `photo_rgb8` | 25 364 | 22 427 | **19 513** | Bigrams |
| `noise_rgb8` | 196 890 | 196 890 | 196 890 | tie |
| `grey_as_rgb8` | **475** | 506 | 506 | MinSumAbs |
| `palette64_rgba8` | 990 | 899 | **770** | Bigrams |
| `sprite_rgba8` | **3 672** | 3 857 | 4 062 | MinSumAbs |
| `flat_rgba8` | **573** | 573 | 605 | MinSumAbs |
| `tiny_rgb8` | 79 | 79 | **62** | Bigrams |

**Bigrams wins four rows by 22–32%; MinSumAbs wins three by 5–6%.** Both stay in the brute-force
set: neither dominates, and the margins run the wrong way to drop either. That matches oxipng
keeping MinSum at `-o 0`/`-o 6` while its default preset leads with Bigrams.

**Entropy is never the unique winner**, and that is a recorded negative result. It beats MinSumAbs
on the photographic and palette rows but loses to Bigrams on both, and ties MinSumAbs elsewhere.
Since the brute-force set is resolved by taking the smallest, a candidate dominated everywhere
costs a full filter pass and a full DEFLATE for nothing — so it is not in that set. It stays
selectable: eight images is a corpus, not a proof.

### Throughput

| stage | before | after | |
| --- | --- | --- | --- |
| `crc32` | 420.8 MB/s | 8.996 GB/s | 21× |
| `filter_image` / None | 497.9 MB/s | 16.26 GB/s | 33× |
| `filter_image` / `Fixed(Paeth)` | 277.1 MB/s | 1.202 GB/s | 4.3× |
| `filter_image` / `MinSumAbs` | 46.7 MB/s | 265.8 MB/s | 5.7× |

All safe Rust: `crc32fast` keeps its `unsafe` to itself, and the filter gains are structural
(hoisting a loop-invariant branch, equal-length subslices, one `match` per row instead of per
byte) plus removing a sixth redundant filter pass per scanline.

### Per-axis state

| # | Axis | State |
| --- | --- | --- |
| 1 | Filter selection | **partial** — MinSumAbs, Entropy and Bigrams per line, plus seven whole-image candidates each fully DEFLATEd. Bigrams is worth 22–32% where it wins (see above). Still missing: per-line trial deflate, `AtomicMin` pruning, and a two-tier cheap-trial codec. [#480]. `FilterStrategy` became `#[non_exhaustive]` with this phase — a heuristic is a measurement result and the set grows with the corpus — which is a **breaking change** for any downstream exhaustive `match`: add a wildcard arm. |
| 2 | DEFLATE quality | **good, ~2% behind zopfli**, and honestly documented in `gamut-deflate`. Two contained wins remain: an 8-byte-at-a-time match compare, and `parse_dp`'s single-distance relaxation. [#478], [#479] |
| 3 | Smallest lawful representation | **partial** — every reduction is implemented (grey, alpha-drop, ≤256 palette, 16→8, sub-byte, and a `tRNS` colour key for grey/truecolour) and the key is worth ~7–9% on a contiguous transparent region, *not* the 25% the raw-byte arithmetic suggests: the alpha plane it removes is usually the most compressible plane in the image. What is not done is the **selection**. `reduce::analyze8` still resolves *some* candidates on the raw estimate alone, and a raw estimate cannot see DEFLATE (below). Until the three-candidate race below it resolved all of them, and the eliminated runner-up was often the one that won the finished file: an opaque RGBA image with ≤256 colours kept an alpha channel that was 255 everywhere (349 bytes against 317), and a 16-bit image whose samples are all `k·257` kept all sixteen bits (220 against 172). The estimate now hands the best **chunk-free** candidate over beside the chunk-carrying one and `write_reduced_or_native` measures both, which closes that whole family — the chunk-free gates are mutually exclusive, so at most one such candidate ever exists. The remainder is the *pair* that both carry a chunk: where a palette and a `tRNS` colour key are both lawful, only the raw-smaller one is ever encoded. |
| 4 | Palette optimization | **partial** — trailing-opaque `tRNS` trim, plus ordering: transparent entries first (so that trim cuts as far as §11.3.2.1 allows) then by luma. Worth −14.7% on the sprite row against +1.5% on `palette64`. A **caller-supplied** palette is now cleaned as well (see [Cleaning a caller's palette](#cleaning-a-callers-palette)), so the two paths cost the same for the same picture. Modified-Zeng ordering — and any ordering of a caller's palette — remain, as a measured heuristic rather than a rule the spec states. [#612] |
| 5 | Cleaning invisible data | **done** — `with_transparent_cleanup`, opt-in, on every alpha-carrying layout at 8 and 16 bits. It is the crate's **one lossy knob**: it rewrites stored samples no decoder renders, where every other reduction here is byte-exact, which is why it is off by default and separate from `with_auto_reduce`. Worth **40.1%** on the sprite row, and it is what makes a colour key reachable at all on a source whose invisible pixels carry different unseen colours. It is a *transform*, not a reduction, so it is **raced** rather than assumed: on `palette64_rgba8` cleaning measured −2.3% at 32×32, **+10.7% at 128×128** and −5.2% at 256×256, because zeroing invisible pixels that carry structure destroys bytes DEFLATE was compressing. `cleaned_or_plain` encodes both and keeps the smaller, so the knob can never cost bytes. A tie keeps the **plain** encoding: cleaning buys its rewritten samples with a size win, and where there is no win there is nothing to buy them with. |
| 6 | Metadata hygiene | **preserve, never strip** — the encoder emits exactly what the caller set, and `gamut convert` carries a PNG input's metadata into a PNG output unless `--strip-metadata` asks otherwise (see [Metadata preservation](#metadata-preservation-issue-483)). Preserving costs bytes, and that is the trade this axis takes: a smaller file that silently lost a colour profile is not a better one. The one exception is shape, not policy: `bKGD` and `sBIT` are resolved against the header actually written (see [Chunks that follow the race](#the-cost-model-and-why-it-is-a-race)). [#483] |
| 7 | Interlacing | **correctly none.** Adam7 costs 5–20%; out of scope by declaration. |
| 8 | Effort / speed / determinism | **partial** — output is byte-reproducible (no time, no randomness, and the one `HashMap` is never iterated). The five size/time knobs now compose into a four-rung `Preset` dial (see [The effort ladder](#the-effort-ladder-issue-484)), and `gamut-deflate`'s `with_optimal_parse_limit` is reachable from `PngEncoder` at last, so PNG callers are no longer stuck at 1 MiB spans. What remains is parallelism: `BruteForce`'s seven candidates are embarrassingly parallel and still run one after another, which is a workspace-level dependency decision rather than a local change. [#484], [#624] |
| 9 | Correctness / robustness | **covered** — 16-bit, odd dimensions, 1×1, CRC policy, malformed input. |

### The cost model, and why it is a race

`reduce::analyze8` chooses by comparing **raw** sizes, which does not predict compressed size when
one candidate's bytes are incompressible and the other's are not. A palette carries a `PLTE` (and
often `tRNS`) that DEFLATE cannot touch, while the pixels it replaces may compress by two orders of
magnitude. Measured on `palette64_rgba8`, whose palette candidate carries a flat 224 bytes of
`PLTE` + `tRNS` (192 + 8 payload, 24 framing) at every size — the fixture's colour count does not
depend on its side:

| side | emitted | IDAT | PLTE+tRNS emitted | libpng-9 |
| --- | --- | --- | --- | --- |
| 128 | 364 | 307 | — palette declined | 405 |
| 160 | 465 | 408 | — palette declined | 572 |
| 192 | 563 | 506 | — palette declined | 707 |
| 256 | 726 | 445 | 224 | 1 102 |

The raw-size estimate sees 16 664 against 65 536 and picks the palette by 4× **at every one of
these sizes**. The finished files disagree: the palette's 224 fixed bytes are incompressible while
the pixels they replace compress by two orders of magnitude, so indexing only pays once the image
is large enough to amortise them — the crossover sits between 192 and 256. So
`write_reduced_or_native` encodes the candidates and keeps the smallest, the same way
`FilterStrategy::BruteForce` already resolves filters — no tuned constant, and never worse than
any candidate it encoded. The three declined rows are the evidence: had the estimate been trusted,
each would have carried a palette and been larger.

**Three candidates, not two.** "Never worse than any candidate it encoded" is only worth
having if the candidates that could win are among them, and for a while they were not. The
estimate collapsed five reductions to one winner and only that winner was raced, so on an image
where the palette won the estimate the reductions it beat — the alpha drop, the greyscale
collapse, the 16→8 demotion — were never encoded, and losing the race dropped the file all the way
back to *no* reduction. `reduce::Reductions` therefore carries the best chunk-free candidate beside
the chunk-carrying one, and the race is over three encodings: chunk-carrying, chunk-free,
unreduced. Ties resolve toward the earlier of `chunked ≻ chunk-free ≻ native` — the more reduced
encoding, and among equal-length files the one already emitted, so a tie changes no output.

A chunk-free *winner* still pays for nothing: it adds nothing DEFLATE cannot compress, so the raw
comparison that chose it is sound and it is written straight out. It is a chunk-free *runner-up*
that has to be measured, because the candidate that beat it does carry a chunk.

**What the races cost.** Each race is a full extra encode, and they nest: `FilterStrategy::BruteForce`
tries seven whole-image strategies, `write_reduced_or_native` encodes up to three candidates when
the reduction carries a chunk (a palette's `PLTE`/`tRNS`, a colour key's `tRNS`), and
`cleaned_or_plain` encodes both the cleaned and the untouched samples when cleanup changed
anything. The worst case — `Level::Best` + `BruteForce` + auto-reduce + cleanup on an alpha image
that is cleanable, palettisable or keyable, *and* has a chunk-free reduction available — is
therefore 7 × 3 × 2 = **42** filter-plus-DEFLATE passes for one file, against 7 for `BruteForce`
alone. That is the price of choosing by measured size rather than by a
cost model; a model good enough to skip the losing candidate is [#480]'s remainder.

**Chunks that follow the race.** `bKGD` and `sBIT` have a payload whose shape is the colour type, and
the race decides the colour type after they were set. Both are resolved against the header actually
written — RGBA `sBIT` loses its alpha entry under RGB or a palette, an RGB or grey background under a
palette becomes the index of its entry (an opaque entry where a transparent twin exists), a grey RGB
triple collapses to one grey sample — and omitted, without error, where no lossless conversion
exists, since a payload shaped for the wrong colour type is a chunk libpng rejects and drops. A
caller's palette *index* survives only on the `encode_indexed8` path, whose palette is the caller's;
under an encoder-derived palette it names nothing and is omitted. On that path the index is
renumbered with the entry it names when cleaning renumbers the palette, and the entry it names is
kept even when no pixel names it — the chunk is carried verbatim, so the alternative is a
background silently repainted. This holds across colour **types**; on the depth axis a `bKGD` sample
is range-checked but not rescaled with a 16→8 demotion or a sub-byte packing — that is [#501].

### The effort ladder (issue #484)

`with_compression`, `with_effort`, `with_filter`, `with_optimal_parse_limit` and
`with_auto_reduce` are five independent knobs. Nothing mapped one choice onto a sensible
combination of them, so a caller wanting the smallest file had to know that it means `Level::Best`
*and* `FilterStrategy::BruteForce` *and* auto-reduce. `Preset` is that knowledge, named.

Measured over the nine-row efficiency corpus at 64x64 (32x32 for the 16-bit row). **The byte
columns are exact, and `cargo test -p gamut-png --test effort -- --nocapture` prints them** — the
size-ordering test encodes every row at every rung to make its assertion, so it publishes the
matrix it already computed and this table can be regenerated by paste rather than by hand.

**The millisecond columns are not produced by that command, and nothing in this repository
reproduces them in one step.** They were taken by an ad-hoc harness in the test profile rather than
by `cargo bench`, on one machine, with every arm warmed up and the minimum kept over three
interleaved passes. They are not in `benches/encode.rs` because that suite runs at 256x256, where
one `Preset::Smallest` row is seven whole-image DEFLATE passes over sixteen times these pixels —
minutes per row, which is not a benchmark anyone would run. Read the
ratios, not the absolute milliseconds — and read them as approximate: a first pass measured
without warm-up put the same two ratios at 150x and 968x rather than 186x and 1147x, so the order
of magnitude is the finding and the third digit is not. Every arm is the same pure-Rust binary and
no reference codec is linked into this measurement, so nothing here depends on which native
library the loader resolved.

| rung | ms per corpus pass | relative | bytes | vs `Balanced` |
| --- | ---: | ---: | ---: | ---: |
| `Fast` | 0.951 | 0.32x | 17 530 | +3.4% |
| `Balanced` | 2.939 | 1x | 16 952 | — |
| `Small` | 546.7 | ~190x | 16 497 | **−2.7%** |
| `Smallest` | 3 370.7 | ~1150x | 15 903 | **−6.2%** |

The shape is the finding: **the ladder is steep in time and shallow in size.** `Small` costs
roughly 190x `Balanced` to save 2.7%, and `Smallest` roughly 1150x to save 6.2%, because both
cross into the zopfli-style optimal parse and `Smallest` additionally runs a full DEFLATE per
brute-force candidate. That is the trade `Level::Best` has always carried; the dial does not
change it, it makes it selectable and says what it costs.

**What the `Small` to `Smallest` step actually buys time from** — measured by adding one knob at a
time to `Small`, same method:

| arm | ms per corpus pass | relative |
| --- | ---: | ---: |
| `Small` | 575.3 | 1x |
| `Small` + `BruteForce` | 3 604.0 | **6.26x** |
| `Small` + effort 15 | 574.0 | 1.00x |
| `Small` + the top rung's parse span | 581.6 | 1.01x |
| `Smallest` (all three) | 3 563.3 | 6.19x |

**The whole step is the filter search.** Raising the refinement budget from 6 to 15 costs nothing
measurable, because the passes stop early at a fixed point and this corpus reaches it well before
six; and the parse span costs nothing *at this size* by construction, since the whole corpus
filters to less than the 32 KiB window the span floor already covers, so every arm above parses
each row as one span whatever the limit says — the parse-span row above was taken when the rung
set an unbounded span and stands unchanged for the 8 MiB it sets now, because at this size the two
are the same parse. So the 6.26x is
the seven whole-image candidates running one after another, which makes this table the direct
case for [#624]: it is the one factor here that parallelism could reclaim.

That inertness is also a warning about this whole section: **the parse span is the one rung knob
that no number on this page observes.** Everything above is 64x64. What the knob does where it is
live is measured separately, below.

Per row:

| input | `Fast` | `Balanced` | `Small` | `Smallest` |
| --- | ---: | ---: | ---: | ---: |
| `gradient_rgb8` | 301 | 292 | 259 | **224** |
| `photo_rgb8` | 2 758 | 2 339 | 2 193 | **1 736** |
| `noise_rgb8` | 12 420 | 12 420 | 12 420 | 12 420 |
| `grey_as_rgb8` | 154 | 146 | **98** | **98** |
| `palette64_rgba8` | 236 | 216 | 209 | **193** |
| `sprite_rgba8` | 1 070 | 987 | 902 | **859** |
| `flat_rgba8` | 197 | 167 | **86** | **86** |
| `opaque256_rgba8` | 239 | 224 | 212 | **172** |
| `demotable_rgb16` | **155** | 161 | 118 | **115** |

**The ladder is ordered over the corpus, not per row**, and `demotable_rgb16` is the recorded
counterexample: `Fast` emits 155 bytes there against `Balanced`'s 161. `Fast` fixes the Paeth
predictor where `Balanced` runs the per-row `MinSumAbs` search, and a fixed predictor beats a
heuristic on a picture that suits it. A cheaper rung coming out smaller costs a caller nothing, so
this is not a defect — but it means "no rung is larger than the rung above it" is not a promise
this crate can keep. `tests/effort.rs` gates the aggregate ordering (strictly, so a rung that buys
nothing fails) rather than pinning an accident of the corpus. `noise_rgb8` ties across all four
rungs for the reason it ties everywhere: incompressible input leaves every setting emitting stored
blocks.

**And the top rung buys nothing at all on a third of the corpus.** The aggregate ordering is a
promise about the total, and the total hides this: `Smallest` is byte-identical to `Small` on
`grey_as_rgb8` (98) and `flat_rgba8` (86), ties with every other rung on `noise_rgb8`, and saves
three bytes on `demotable_rgb16` — four of nine rows where 6.26x the time buys three bytes or
fewer, and three where it buys none. `Smallest`'s 3.6% over `Small` is earned on the five rows
that are left, and mostly on `photo_rgb8`. A rung is a bet on the material, not a guarantee about
it: if your images look like the rows that tie, the rung above costs six times the time for
nothing, and the only way to know is to measure your own corpus.

**`Fast` filters with a fixed Paeth rather than not filtering at all**, which is the opposite of
the obvious guess and was settled by measuring, not by argument. Over the same corpus, warmed up,
minimum of five interleaved passes:

| `Fast` candidate | relative time | bytes |
| --- | ---: | ---: |
| `FilterStrategy::None` | 1.21x | 46 267 |
| `Fixed(Up)` | 0.94x | 18 064 |
| `Fixed(Paeth)` | 1x | **17 530** |
| `MinSumAbs` | 1.25x | 17 600 |

Skipping the filter hands DEFLATE a stream so much larger that the compressor loses more time than
the filter pass saves: `None` is both 2.6x the largest result *and* 21% slower than fixed Paeth —
a dominated candidate, not a trade. `Fixed(Paeth)` likewise dominates `MinSumAbs` here, smaller
*and* faster. Only `Fixed(Up)` is a genuine alternative, 6% quicker for 3% more bytes; Paeth takes
the rung because the ladder already has three slower entries above it and the bottom rung's job is
to cost almost nothing extra in size.

### The optimal-parse span, measured where it is live

Every number in the section above is 64x64, and at 64x64 this knob does nothing: the whole corpus
filters to less than the 32 KiB window that is the span floor, so each row is one span whatever the
limit says. A rung that set the span could therefore assert a direction nobody had observed. These
are the measurements that observe it — `Level::Best`, one filter (`MinSumAbs`), auto-reduce on,
the span the only thing varied, on the same generated corpus at sizes whose filtered stream
exceeds the 1 MiB default.

**Whether a wider span saves bytes is a property of the picture, not a direction.** At 1024x1024,
refinement budget 6, no bound against the 1 MiB default:

| input | filtered stream | 1 MiB | no bound | change |
| --- | ---: | ---: | ---: | ---: |
| `grey_as_rgb8` | 1 049 600 | 2 272 | 2 255 | **−0.75%** |
| `gradient_rgb8` | 3 146 752 | 21 905 | 21 866 | **−0.18%** |
| `sprite_rgba8` | 4 195 328 | 25 775 | 25 743 | **−0.12%** |
| `palette64_rgba8` | 1 049 600 | 3 729 | 3 726 | −0.08% |
| `opaque256_rgba8` | 1 049 600 | 5 738 | 5 734 | −0.07% |
| `flat_rgba8` | 132 096 | 222 | 222 | — one span already |
| `noise_rgb8` | 3 146 752 | 3 147 636 | 3 147 636 | — stored blocks |
| `photo_rgb8` | 3 146 752 | 314 121 | 314 436 | **+0.10%** |

A wider span is a *different* cost model, not a better one: it refines one histogram over material
the narrower spans were free to model separately, and on a photograph — the least homogeneous row
here — that loses. Five rows win by under a percent, one loses by a tenth of one, two cannot move
either way, and the spread is small in both directions.

The photograph still loses at the rung's own settings, where the filter search gets a say in the
result: one encode of the same 1024x1024 `photo_rgb8` at the whole `Preset::Smallest` rung emits
284 991 bytes at the 1 MiB default and **285 022** at 8 MiB, 16 MiB and no bound alike — the three
agree because that image's filtered stream is 3.1 MB, inside any of them. So the top rung's span
costs this picture 31 bytes, and the rung takes it anyway on the strength of the five rows above
that it saves more on. That is the honest shape of the knob: it is a bet, and this document is
where the losing rows are written down.

**And the win saturates.** At 2048x2048 the gradient's stream is 12 MiB, so the span choice is a
real one:

| span | spans over the stream | bytes | vs 1 MiB |
| --- | ---: | ---: | ---: |
| 1 MiB (default) | 13 | 73 663 | — |
| 2 MiB | 7 | 73 619 | −0.060% |
| 4 MiB | 4 | 73 603 | −0.081% |
| **8 MiB** | 2 | 73 589 | **−0.100%** |
| 16 MiB | 1 | 73 585 | −0.106% |
| no bound | 1 | 73 585 | −0.106% |

**What an unbounded span costs is memory, and it has no bound of its own.** `gamut-deflate`'s
shortest-path parse allocates three span-length vectors per refinement pass — the cost row
(`u64`), the chosen length and the chosen distance (`u16` each) — for 12 bytes per byte of span,
so the span is the only thing bounding this encoder's working set. Peak resident set for one
encode per process (`/usr/bin/time -v`, square RGB photograph, refinement budget 1):

| image | filtered stream | 1 MiB | 8 MiB | 16 MiB | no bound |
| --- | ---: | ---: | ---: | ---: | ---: |
| 1024x1024 | 3 146 752 | 21.9 MiB | 46.7 MiB | 46.6 MiB | 46.5 MiB |
| 2048x2048 | 12 584 960 | 56.0 MiB | 127.2 MiB | 177.4 MiB | 177.2 MiB |
| 4096x4096 | 50 335 744 | 177.9 MiB | 219.0 MiB | 312.6 MiB | **701.8 MiB** |

That is the formula, measured. Against the 1 MiB column the extra resident set per extra byte of
span is 12.3, 11.0 and 11.1 bytes across the three rows — 12 allocated, slightly less resident
because the two `u16` rows are zero-initialised and pages they never write are never faulted in.
Time is not what scales: the same twelve runs moved by under 2% and not monotonically
(8.41 / 8.37 / 8.37 / 8.47 s, 34.65 / 34.08 / 33.92 / 33.95 s, 133.9 / 135.4 / 134.3 / 132.7 s),
because the parse's work is linear in the input whatever the span. A wider span *can* still take
more refinement passes to converge, so time is data-dependent rather than flat; memory is neither.
`PngEncoder::with_optimal_parse_limit` says all of this at the setter;
`DeflateEncoder::with_optimal_parse_limit`, where the allocation actually happens, still names only
time, which is [#631].

**So `Preset::Smallest` takes a finite 8 MiB span and not an unbounded one.** At 8 MiB the span is
the whole stream — byte-identical to no bound — for any image up to about 1670x1670 RGB or
1448x1448 RGBA, which is where every win in the first table lives. Past that it keeps most of what
is left (−0.100% of the −0.106% available on the 2048 gradient; −0.028% of −0.044% on a 4096
photograph) for a third of the memory. Doubling to 16 MiB buys a further 0.005% at 4096x4096 and
costs another 94 MiB, which is the trade the constant declines. An encoder knob whose working set
grows without bound in the image is not something to ship behind a rung named for size — and a
caller who has measured that their material wants more can still say
`with_optimal_parse_limit(usize::MAX)` after the rung, having read what it costs.

**What the dial deliberately does not touch.** `with_transparent_cleanup` is in no rung. It is this
crate's one lossy knob (axis 5), and a dial named for effort must not be what silently changes
which samples a file stores; enable it beside a rung. Ancillary chunks, metadata and pushed
backends are untouched for the same reason — they are what the file *says*, not how hard the
encoder worked.

**What the gates cannot see.** The times above are reported, not gated, for the reason [#437]
gives for the rest of this document: a timing cannot fail a build without making it flaky. What
*is* gated is the aggregate size ordering, the byte-identity of `Preset::Balanced` with a default
`PngEncoder`, and — through libpng — that no rung changes the pixels a file resolves to. The last
covers the 8-bit rows only, and that bound is libpng's rather than this crate's: `decode_rgba8`
drives libpng's *simplified* API, which treats a 16-bit file as linear and converts it to sRGB on
the way to 8-bit output. A stored sample of 40 comes back as 40 from an 8-bit file and as 110 from
a 16-bit one, so it is not a depth-neutral resolver and cannot compare a rung that stores 16 bits
against one that losslessly demotes to 8. 16-bit fidelity is pinned by `tests/oracle.rs` at its own
stored depth.

### Cleaning a caller's palette

`encode_indexed8` takes the palette the caller hands it. That palette is not built from the pixels,
so — unlike the encoder-derived one, which cannot contain either by construction — it may hold
entries nothing names and entries that name a colour another entry already names. Both are written
into an incompressible `PLTE`, and the count of them decides the index bit depth.

So the palette is cleaned before it is written (`PngPalette::cleaned`): an entry no pixel and no
`bKGD` index names is dropped, a later entry with the same RGB **and** the same alpha as an earlier
one is merged into it, the trailing opaque `tRNS` bytes §11.3.2.1 lets a chunk omit are omitted, the
index bit depth is derived from what survives, and the image's indices — and a
`with_background_index` background — are renumbered onto the result. Surviving entries keep the
caller's relative order; **ordering** a caller's palette is a separate, heuristic question ([#612]).

It is silent and lossless, which is why it goes through no notice channel: a merged entry did not
fail to come along, it arrived under another index. libpng resolving the file to the caller's exact
RGBA is the test of that (`tests/oracle.rs`).

Measured on a 64×64 four-colour picture handed a full 256-entry palette (4 colours repeated 64
times, one of them transparent):

| palette handed in | before | after |
| --- | ---: | ---: |
| 256 entries, 4 colours | 1 194 | **162** |
| 4 entries, tight | 164 | **162** |

The redundant palette now costs exactly what the tight one costs — the files are byte-identical,
which `a_redundant_palette_costs_what_the_tight_one_costs` pins — because after cleaning they *are*
the same palette: −86.4% on the first row. The tight palette's own 2 bytes are the `tRNS` trim,
which this path did not previously apply. What is bought is rarely the `PLTE` bytes alone: 252
dropped entries also take the index stream from 8 bits per pixel to 2.

[#437]: https://github.com/visualcommons/gamut/issues/437
[#478]: https://github.com/visualcommons/gamut/issues/478
[#479]: https://github.com/visualcommons/gamut/issues/479
[#480]: https://github.com/visualcommons/gamut/issues/480
[#481]: https://github.com/visualcommons/gamut/issues/481
[#482]: https://github.com/visualcommons/gamut/issues/482
[#483]: https://github.com/visualcommons/gamut/issues/483
[#484]: https://github.com/visualcommons/gamut/issues/484
[#501]: https://github.com/visualcommons/gamut/issues/501
[#612]: https://github.com/visualcommons/gamut/issues/612
[#624]: https://github.com/visualcommons/gamut/issues/624
[#631]: https://github.com/visualcommons/gamut/issues/631
