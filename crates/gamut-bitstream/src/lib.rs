//! Low-level bit readers/writers and entropy coders shared by the gamut codecs.
//!
//! The pieces here implement the parsing processes defined in the AV1 Bitstream & Decoding
//! Process Specification (`references/av1/av1-spec.pdf`), and their encoder-side mirrors:
//!
//! - [`BitWriter`] / [`BitReader`] — most-significant-bit-first fixed-width fields and byte
//!   alignment under mirrored names (`put_bits`/`get_bits`, `align`/`align`,
//!   `put_leb128`/`leb128`), used by the AV1 uncompressed sequence/frame headers (AV1 §4, §8.1).
//!   The reader keeps the spec's descriptor names: `f(n)` (an alias of `get_bits`), `su(n)`,
//!   `ns(n)`, `uvlc()`, `le(n)`, and `leb128()`.
//! - [`write_leb128`] / [`leb128_len`] — unsigned LEB128 used for OBU sizes (AV1 §4.10.5, Annex B),
//!   into a plain byte buffer.
//! - [`SymbolDecoder`] / [`SymbolEncoder`] — the AV1 multi-symbol arithmetic (range) coder of
//!   §8.2, the entropy back-end for coded tile data, under mirrored `decode_*` / `encode_*` verbs.
//!   The decoder is the normative direction; the encoder is derived by inverting it.
//!
//! It also carries codec-agnostic bit-packing primitives:
//!
//! - [`LsbBitReader`] — the least-significant-bit-first counterpart of [`BitReader`], for the
//!   formats that pack fields from the low bit of each byte upward: VP8L's `ReadBits(n)`
//!   (RFC 9649 §3.3).
//! - [`pack_msb_rows`] / [`unpack_msb_rows`] — MSB-first packing of fixed-width integer samples into
//!   byte-aligned rows, the sub-byte sample layout shared by the TIFF-family raw formats.
//!
//! And the table side of length-limited canonical Huffman coding, in the ITU-T T.81 form
//! (`references/jpeg/itu-t81.pdf`) that JPEG and lossless JPEG (and so DNG) share:
//!
//! - [`build_optimal_table`] — the Annex K.2 optimal 16-bit length-limited table over measured
//!   symbol frequencies, for any alphabet of at most 256 symbols, as a [`HuffmanTable`] holding
//!   the `(BITS, HUFFVAL)` lists.
//! - [`CanonicalCodes`] — the Annex C canonical [`Code`] of every symbol of a `(BITS, HUFFVAL)`
//!   pair.
//!
//! Plus a format-neutral builder in the per-symbol-lengths form of RFC 1951 §3.2.2, which VP8L
//! shares:
//!
//! - [`build_length_limited_lengths`] — Huffman code lengths over any alphabet's histogram, capped
//!   at a caller-chosen maximum.
//! - [`canonical_codes`] — the canonical code of every symbol from its code length.
//!
//! No ANS coder or JPEG XL prefix code ships here, and neither is planned: `gamut-jxl` wraps the
//! format's reference implementations rather than coding its entropy layer itself.
#![forbid(unsafe_code)]

// Compiles README.md's code blocks as doctests; exists only while rustdoc collects them.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;

mod bitreader;
mod bitwriter;
mod huffman;
mod leb128;
mod lsb_reader;
mod samplepack;
mod symbol;

pub use bitreader::BitReader;
pub use bitwriter::BitWriter;
pub use huffman::{
    CanonicalCodes, Code, HuffmanTable, build_length_limited_lengths, build_optimal_table,
    canonical_codes,
};
pub use leb128::{leb128_len, write_leb128};
pub use lsb_reader::LsbBitReader;
pub use samplepack::{pack_msb_rows, row_bytes, unpack_msb_rows};
pub use symbol::{SymbolDecoder, SymbolEncoder};
