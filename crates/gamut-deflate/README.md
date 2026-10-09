# gamut-deflate

`gamut-deflate` is a pure-Rust **DEFLATE** (RFC 1951) and **zlib** (RFC 1950) *encoder*, tuned for
space efficiency.

## Goals

Part of the [gamut](../../README.md) workspace, this crate is the shared compression primitive
behind the codecs that embed DEFLATE streams — most directly [`gamut-png`](../gamut-png) (`IDAT`,
`zTXt`, `iCCP`), and reusable by TIFF (`Compression=8`) and any other zlib consumer. It is:

- **Space-efficient.** Encode latency is secondary to ratio. `Level::Best` runs a zopfli-style
  optimal parse (a shortest-path LZ77 search against an iteratively refined entropy model) with
  per-block dynamic Huffman codes and cost-driven block splitting, and consistently beats zlib at
  maximum effort — see [Why this crate](#why-this-crate).
- **Encoder-only.** Inflating DEFLATE is a solved problem, so — per gamut's encoder-first philosophy
  — this crate does not decode; see [Scope](#scope). Correctness is proven differentially against the
  canonical C `zlib` (see [Validation](#validation)).
- **Self-contained and safe.** 100% safe Rust (`#![deny(unsafe_code)]`), no internal dependencies.

## Why this crate

There are excellent pure-Rust DEFLATE encoders already — [`miniz_oxide`](https://docs.rs/miniz_oxide)
(the fast/general engine behind `flate2` and the `png` crate) and the
[`zopfli`](https://docs.rs/zopfli) crate (state-of-the-art ratio). This crate exists because gamut is
a *space-efficient* image library, and `Level::Best` earns its place on ratio: it produces **smaller
zlib streams than `zlib -9` and `miniz_oxide` at their maximum levels** on every input we measure,
while staying close to `zopfli` — all behind one dependency-free `Level` knob that also spans the
fast tiers, so codecs don't juggle two external crates.

Output size in bytes (zlib streams; lower is better). Every row except the pinned `lz77.rs` row is
reproduced by `cargo bench -p gamut-deflate`; that row is historical — see the note below the table:

| input                         |    raw | `Level::Best` | `zlib -9` | `miniz_oxide`-10 | `zopfli` |
| ----------------------------- | -----: | ------------: | --------: | ---------------: | -------: |
| RFC 1951 spec text (36 KB)    | 36 945 |    **10 664** |    11 112 |           11 130 |   10 544 |
| English text ×300             | 13 500 |       **103** |       110 |              107 |      103 |
| Rust source (`lz77.rs` at `4f2c2a4`) | 25 031 | **8 003** |     8 268 |            8 269 |    7 950 |
| pseudo-random (~incompressible)| 20 000 |     **2 236** |     2 290 |            2 291 |    2 122 |

The `lz77.rs` row compresses this crate's own match finder, so it is pinned to the file at commit
`4f2c2a4` (25 031 bytes): a later `cargo bench` run compresses whatever the file is by then, and
reproduces this row only against that revision of the file.

`Level::Best` lands ~2–7% below `zlib -9` and within ~1% of `zopfli` on real text and source, now
that the optimal parse prices each match length at its own nearest distance (zopfli's `sublen`)
rather than at the longest match's; what remains of the gap is `zopfli`'s 15 optimization passes +
package-merge length-limiting, vs. this crate's default 6 passes + a count-floor heuristic. The pass budget is configurable via
`DeflateEncoder::with_effort` (0 = the lazy seed parse only; 15 ≈ zopfli's budget), so size-vs-time
curves can be swept along one axis. `DeflateEncoder::with_optimal_parse_limit` is the second axis:
the optimal parse works in spans of at most 1 MiB by default, each with its own refined cost model
and each free to reference the history before it, so input size alone never disables it. If you need
inflate, streaming, or gzip framing, reach for
`flate2`/`miniz_oxide` instead — this crate is deliberately narrower (see [Scope](#scope)).

## Usage

```rust
use gamut_deflate::{DeflateEncoder, Level};

let data = b"the quick brown fox jumps over the lazy dog".repeat(8);

// Raw DEFLATE (RFC 1951):
let mut raw = Vec::new();
DeflateEncoder::new().compress(&data, &mut raw);

// zlib-wrapped (RFC 1950) — what PNG's IDAT carries:
let mut zlib = Vec::new();
DeflateEncoder::new().with_level(Level::Best).zlib_compress(&data, &mut zlib);

// Best with a zopfli-class effort budget (more optimal-parse passes, more time) and a wider
// optimal-parse span (one cost model over more data, more memory):
let mut dense = Vec::new();
DeflateEncoder::new()
    .with_level(Level::Best)
    .with_effort(15)
    .with_optimal_parse_limit(4 << 20)
    .zlib_compress(&data, &mut dense);
```

## Scope

- **Formats:** raw DEFLATE (RFC 1951) and the zlib wrapper (RFC 1950). gzip framing (RFC 1952) is out
  of scope — no gamut image format uses it — as is its CRC-32; zlib's integrity check is Adler-32,
  exposed as `adler32` (PNG's unrelated chunk CRC-32 lives in `gamut-png`).
- **Encoder-only, by design.** Inflating untrusted input is a security-sensitive, fully-solved
  problem; the workspace's decoders that need it (the DNG decoder today; a future TIFF/PNG decoder)
  depend on the safe, fuzzed `miniz_oxide` rather than a fresh implementation here.
- **One-shot.** The whole input compresses in a single call into a caller-owned `Vec<u8>`; there is
  no streaming encoder, which matches whole-image encoding.

## Status

Production-ready v1. Each compression level always produces a correct stream; higher levels only
improve the ratio, from stored blocks up through fixed/dynamic Huffman, block splitting, and a
zopfli-style optimal parse. See [STATUS.md](STATUS.md).

## Validation

- **Differential oracle** — the dev-only `tooling/zlib-oracle` (a vendored, statically-linked
  `zlib` v1.3.1) inflates the encoder's output and asserts it round-trips to the original bytes, for
  every level, across an edge-case corpus and a randomized round-trip test.
- **Ratio contract** — the oracle suite asserts `Level::Best` is no larger than `zlib -9` (and no
  larger than `Level::Default`), so regressions in the crate's reason to exist fail the build.
- **Benches** — `cargo bench -p gamut-deflate` reports ratio and throughput per level against
  `miniz_oxide` and `zopfli` baselines.

## License

Licensed under either of MIT or Apache-2.0 at your option.
