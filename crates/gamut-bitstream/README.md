# gamut-bitstream

`gamut-bitstream` holds the low-level bit readers, bit writers and entropy coders the gamut codecs
share — the parsing processes defined in the codec specs and their encoder-side mirrors.

## Goals

Part of the [gamut](../../README.md) workspace, this crate exists to:

- **Provide the bit-level building blocks once.** The pieces here implement the parsing processes
  of the AV1 Bitstream & Decoding Process Specification
  ([`../../references/av1/`](../../references/av1)) and their encoder-side mirrors:
  - [`BitWriter`] / [`BitReader`] — most-significant-bit-first fixed-width fields and byte
    alignment under mirrored names (`put_bits`/`get_bits`, `align`/`align`, `put_leb128`/`leb128`),
    used by the AV1 uncompressed sequence/frame headers (AV1 §4, §8.1); the reader keeps the spec's
    descriptor names `f(n)` (an alias of `get_bits`), `su(n)`, `ns(n)`, `uvlc()`, `le(n)` and
    `leb128()`.
  - [`write_leb128`] / [`leb128_len`] — unsigned LEB128 for OBU sizes (AV1 §4.10.5, Annex B).
  - [`SymbolDecoder`] / [`SymbolEncoder`] — the AV1 multi-symbol arithmetic (range) coder of
    §8.2, the entropy back-end for coded tile data, under mirrored `decode_*` / `encode_*` verbs;
    the encoder is derived by inverting the normative decoder.
  - [`pack_msb_rows`] / [`unpack_msb_rows`] / [`row_bytes`] — MSB-first packing of fixed-width
    samples into byte-aligned rows, the sub-byte layout the TIFF-family raw formats share.
- **Own the Huffman table construction the JPEG family shares.** [`build_optimal_table`] is the
  ITU-T T.81 Annex K.2 optimal 16-bit length-limited table over measured symbol frequencies, for
  any alphabet of at most 256 symbols (JPEG DC/AC symbols, the lossless-JPEG difference categories
  DNG uses), returned as a [`HuffmanTable`], and [`CanonicalCodes`] the Annex C canonical [`Code`]
  assignment over a `(BITS, HUFFVAL)` pair ([`../../references/jpeg/`](../../references/jpeg)).
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

Ships both directions of the AV1 primitives — [`BitReader`] / [`BitWriter`], LEB128
([`write_leb128`] / [`leb128_len`]), and the [`SymbolDecoder`] / [`SymbolEncoder`] range coder.
`gamut-av1` uses [`BitReader`], [`BitWriter`], [`write_leb128`] and [`SymbolEncoder`];
[`SymbolDecoder`] has no workspace consumer yet. [`BitWriter`] also backs `gamut-tiff`'s CCITT
coders. The crate further ships the MSB-first sample packing used by `gamut-dng`, and the T.81
Huffman table side ([`build_optimal_table`], [`CanonicalCodes`]) behind `gamut-jpeg`'s optimized
tables and `gamut-dng`'s lossless JPEG.

No ANS coder and no JPEG XL prefix code ship here, and neither is planned: `gamut-jxl` wraps the
format's reference implementations rather than coding its entropy layer itself. An entropy coder
for `gamut-av2` would join behind its own module when that stub crate gains an encoder.

## License

MIT or Apache-2.0, at your option — see the [workspace README](../../README.md#license).
