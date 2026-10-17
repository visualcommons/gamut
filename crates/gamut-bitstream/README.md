# gamut-bitstream

`gamut-bitstream` holds the low-level bit writers and entropy coders the gamut codecs share — the
encoder-side mirror of the parsing processes defined in the codec specs.

## Goals

Part of the [gamut](../../README.md) workspace, this crate exists to:

- **Provide the bit-level building blocks once.** The pieces here are the encoder-side counterpart of
  the AV1 Bitstream & Decoding Process Specification
  ([`../../references/av1/`](../../references/av1)):
  - [`BitWriter`] — most-significant-bit-first fixed-width fields (`f(n)`) and byte alignment, used
    by the AV1 uncompressed sequence/frame headers (AV1 §4, §8.1).
  - [`write_leb128`] / [`leb128_len`] — unsigned LEB128 for OBU sizes (AV1 §4.10.5, Annex B).
  - [`SymbolEncoder`] — the AV1 multi-symbol arithmetic (range) coder, derived by inverting the
    symbol *decoder* of AV1 §8.2; the entropy back-end for coded tile data.
- **Own the Huffman table construction the JPEG family shares.** [`build_optimal_table`] is the
  ITU-T T.81 Annex K.2 optimal 16-bit length-limited table over measured symbol frequencies, for
  any alphabet of at most 256 symbols (JPEG DC/AC symbols, the lossless-JPEG difference categories
  DNG uses), and [`CanonicalCodes`] the Annex C canonical `(code, length)` assignment over a
  `(BITS, HUFFVAL)` pair ([`../../references/jpeg/`](../../references/jpeg)).
- **Stay byte-exact with decoders.** Each writer is validated to produce streams real decoders
  (`dav1d`, `avifdec`) accept, since it is derived directly from the decode process.
- **Stay allocation-conscious.** Writers append into caller buffers and avoid per-symbol heap churn.
- **Stay memory-safe.** `#![forbid(unsafe_code)]`.

## Usage

```rust
use gamut_bitstream::{leb128_len, write_leb128};

// Unsigned LEB128, as used for AV1 OBU sizes: 300 -> 0xac 0x02 (2 bytes).
let mut out = Vec::new();
write_leb128(&mut out, 300);
assert_eq!(out, [0xac, 0x02]);
assert_eq!(leb128_len(300), 2);
```

## Status

M0 provides the AV1 encoder-side primitives: [`BitWriter`], LEB128 ([`write_leb128`] /
[`leb128_len`]), and the [`SymbolEncoder`] range coder. The T.81 Huffman table side
([`build_optimal_table`], [`CanonicalCodes`]) serves `gamut-jpeg`'s optimized tables. The
forward-looking ANS coders and JPEG XL prefix codes named in the workspace plan (for AV2 / JPEG XL)
are not implemented yet.

## Roadmap

- ANS and JPEG XL prefix entropy coders (for `gamut-av2` / `gamut-jxl`), each behind its own
  module.

## License

Licensed under either of MIT or Apache-2.0 at your option.
