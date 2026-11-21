# Decoder security audit

Every gamut decoder accepts bytes an attacker controls. This document is the bar each one is held
to, the method that checks a crate against it, and where each crate stands. It is the checklist
[#311](https://github.com/visualcommons/gamut/issues/311) asks for. The per-crate issues it filed
carry the work, one issue per crate. #311 stays open as their tracker until all of them close.

The audit is **posture**, not proof. The fuzz tier (`tooling/gamut-fuzz`) searches for
counterexamples; this checklist states what a crate must be able to *argue* about every
untrusted-input entry point, with the evidence that settles each claim. A crate passes when every
item below reads **Pass** in its row, each with that evidence.

## The checklist

An **untrusted-input entry point** is any public function that takes file bytes, or a model parsed
from file bytes. A parsed ICC profile or a parsed metadata block counts too: every value in it is
still the writer's choice. `docs/testing.md`'s per-crate table names the entry points in its
"Fuzz entry point" column.

1. **Limits on every untrusted-input entry point.** A decoder that sizes an output from a header
   takes a caller-set `DecodeLimits` (`gamut_core::DecodeLimits`; gamut-heif's own type until
   #1226 folds it in). Its default is bounded, and it is checked **against the header before any
   buffer is sized from it**. A parser whose output only borrows from the input, or is bounded
   by the input's length, needs no limit, and its row says why. Structural recursion (IFD trees,
   HEIF derivations, XML nesting) has a fixed depth cap.
2. **No panics on hostile input.** Library code has no `unwrap`, `expect`, `panic!` or
   `unreachable!` (`AGENTS.md` already forbids the first two). Every slice index and every
   `assert!` that input can reach is proven in range by a prior check, cited at the site, or
   rewritten as `get` with a typed error. A stack overflow counts as a panic here, and it is worse
   than one: it aborts the process.
3. **No unbounded allocation.** Every `with_capacity`, `vec![_; n]`, `reserve` and `resize` whose
   size an input influences is bounded by item 1's limit, by the bytes left in the input, or by a
   constant. A reservation sized from a declared count is made only after the count has been
   checked against what the data can actually deliver.
4. **Checked arithmetic on attacker sizes.** Products and sums of declared dimensions, counts,
   offsets and lengths use `checked_*` (or are proven bounded by a prior check), and narrowing
   casts of them use `try_from`. In a release build an overflow does not panic: it **wraps** to a
   small value that passes the very check it should have failed.
5. **`unsafe` posture.** `#![forbid(unsafe_code)]` everywhere, except the six hot-path crates
   that `AGENTS.md` lets `deny` it, with no `#[expect(unsafe_code)]` exception. A crate with an
   `extern "C"` surface also qualifies if that surface stays in its one named module.
6. **A fuzz target and a clean 60-second smoke.** Every entry point is driven by a
   `tooling/gamut-fuzz` target, and each target runs clean for 60 seconds from its committed seeds.

## Method

Items 2–4 start from a mechanical survey. Its counts say **where to look**, not what is wrong.
The lints are Clippy *restriction* lints, and most sites they flag are sound. A per-crate issue
exists to turn each site into a verdict.

```bash
# Items 2 and 4: library code only (`--lib` drops `#[cfg(test)]`).
cargo clippy --lib --all-features -p gamut-<crate> -- -A clippy::all \
  -W clippy::unwrap_used -W clippy::expect_used -W clippy::panic -W clippy::unreachable \
  -W clippy::indexing_slicing -W clippy::arithmetic_side_effects -W clippy::cast_possible_truncation

# Item 3: allocation sites in library code; the survey stops at each file's `#[cfg(test)]`.
rg -n 'with_capacity\(|vec!\[[^\]]*;|\.reserve(_exact)?\(|\.resize\(' crates/gamut-<crate>/src

# Item 5.
rg -n '^#!\[(forbid|deny)\(unsafe_code\)\]' crates/gamut-<crate>/src/lib.rs

# Item 6, one target at a time and never in parallel; see tooling/gamut-fuzz/README.md.
mise run fuzz <target> tooling/gamut-fuzz/state/<target> tooling/gamut-fuzz/corpus/<target> -- -max_total_time=60
```

The allocation scan sees only explicit reservations. It misses a `Vec` that grows through
`push`, `extend` or `collect` in a loop whose bound is a declared count, and such loops are found
only by reading for item 3.

A clean smoke is weak evidence. Sixty seconds from cold explores shallowly, and libFuzzer's
`-max_len` defaults to the largest seed, so a defect that needs a large input is out of its reach.
The defect #1260 needs a 180 KB input to reach a depth of 5 000, and that is how the smoke missed it. Size- and depth-driven
defects are found by reading for items 1–4, and the fuzz tier is not a substitute for that
reading.

## Where each crate stands

The survey ran at `master@a0727765`. The full survey for each crate, with its allocation-site
list, is in its issue. "Index", "arith" and "cast" are the `indexing_slicing`,
`arithmetic_side_effects` and `cast_possible_truncation` counts. `unwrap`, `expect`, `panic!` and
`unreachable!` counted **zero** in every crate below, and every crate holds item 5.

| Crate | Issue | Limits (item 1) | Index | Arith | Cast | Alloc sites | Fuzz targets (60 s smoke) |
|---|---|---|---|---|---|---|---|
| `gamut-png` | #1262 | Pass | 125 | 132 | 11 | 29 | `png_decode` clean |
| `gamut-jpeg` | #1263 | Pass | 204 | 289 | 52 | 32 | `jpeg_decode` clean |
| `gamut-webp` | #1264 | Pass at the source level | 443 | 599 | 137 | 43 | `webp_decode` clean, `riff_chunks` clean |
| `gamut-riff` | #1265 | Not applicable | 23 | 21 | 10 | 2 | `riff_chunks` clean |
| `gamut-jxl` | #1266 | Pass | 6 | 16 | 1 | 7 | `jxl_decode` clean |
| `gamut-av1` | #1267 | Pass for the header layer | 611 | 780 | 109 | 51 | `av1_inspect` clean |
| `gamut-avif` | #1268 | Pass | 23 | 39 | 9 | 4 | `avif_parse_decode` clean, `avif_clean_alpha` clean |
| `gamut-heif` | #1269 | Pass, with its own type | 32 | 84 | 17 | 3 | `heif_decode_limits` clean |
| `gamut-heic` | #1270 | Pass | 8 | 11 | 0 | 2 | `heic_container` clean, `heic_hvcc` clean |
| `gamut-isobmff` | #1271 | Not applicable | 29 | 15 | 25 | 13 | `isobmff_boxes` clean |
| `gamut-tiff` | #1272 | Partial | 45 | 105 | 32 | 16 | `tiff_decode` clean, `ifd_read` clean |
| `gamut-dng` | #1273 | Fail | 85 | 88 | 42 | 32 | `dng_decode` clean, `ifd_read` clean |
| `gamut-ifd` | #1274 | Pass | 92 | 73 | 21 | 18 | `ifd_read` clean, `ifd_read_ledger` clean, `ifd_one_store` clean |
| `gamut-deflate` | #1275 | Pass | 79 | 99 | 20 | 10 | `deflate_inflate_zlib` clean |
| `gamut-icc` | #1276 | Not applicable as a decode budget | 29 | 28 | 36 | 36 | `icc_parse` clean, `cmm_transform` clean |
| `gamut-exif` | #1277 | Not applicable | 6 | 5 | 5 | 4 | `exif_parse` clean |
| `gamut-xmp` | #1278 | Fail | 6 | 18 | 0 | 0 | `xmp_parse` clean, `metadata_extract` clean |
| `gamut-iptc` | #1279 | Not applicable | 14 | 19 | 3 | 1 | `iptc_read_irb` clean, `metadata_extract` clean |
| `gamut-metadata` | #1280 | Inherited from the carrier parsers | 0 | 7 | 4 | 0 | `metadata_extract` clean |
| `gamut-cmm` | #1281 | Pass by construction | 201 | 161 | 4 | 17 | `cmm_transform` clean |

**Defects confirmed** by the audit:

- **#1260** — `gamut-xmp`: element nesting is unbounded. A 180 KB packet of 5 000 nested
  `rdf:parseType="Resource"` levels overflows the stack. Past 65 535 levels, quick-xml's `u16`
  nesting counter overflows.
- `gamut-tiff`: `decode_strips` and `decode_tiles` reserve the whole page from header tags before
  any strip is read. #1226 is repairing it, and #1272 tracks that it lands with a test.
- `gamut-dng`: no caller-set limits, tracked by #1107.
- A stale document: `tooling/gamut-fuzz/README.md` still says the WebP decoder has no caller-set
  limit, although `WebpDecoder::with_limits` landed for #1025. #1226 is editing that row, so
  #1264 records the correction and this branch leaves the row alone.

**Out of the audit's scope.** `gamut-core`, `gamut-color`, `gamut-dsp`, `gamut-bitstream` and
`gamut-tonemap` are shared primitives. They take caller-built values, and hostile bytes reach them
only *through* a decoder above, whose targets exercise them. A panic one of those targets finds
in a primitive is fixed in the primitive, as `AGENTS.md`'s "Work moves up the dependency graph"
requires. `gamut-codec-abi` reads no file bytes, only the plain data a registered backend returns.
`gamut-av2` and `gamut-vvc` are stubs. The binary and binding crates (`gamut`, `gamut-cli`,
`gamut-wasm`, `gamut-ffi`, `gamut-jxl-sys`) add no parser of their own.
