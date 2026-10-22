//! Low-level bit readers/writers and entropy coders shared by the gamut codecs.
//!
//! The pieces here implement the parsing processes defined in the AV1 Bitstream & Decoding
//! Process Specification (`references/av1/av1-spec.pdf`), and their encoder-side mirrors:
//!
//! - [`BitWriter`] / [`BitReader`] — most-significant-bit-first fixed-width fields (`f(n)`) and
//!   byte alignment, used by the AV1 uncompressed sequence/frame headers (AV1 §4, §8.1). The
//!   reader also covers `su(n)`, `ns(n)`, `uvlc()`, `le(n)`, and `leb128()`.
//! - [`write_leb128`] / [`leb128_len`] — unsigned LEB128 used for OBU sizes (AV1 §4.10.5, Annex B).
//! - [`SymbolDecoder`] / [`SymbolEncoder`] — the AV1 multi-symbol arithmetic (range) coder of
//!   §8.2, the entropy back-end for coded tile data. The decoder is the normative direction; the
//!   encoder is derived by inverting it.
//!
//! It also carries codec-agnostic bit-packing primitives:
//!
//! - [`pack_msb_rows`] / [`unpack_msb_rows`] — MSB-first packing of fixed-width integer samples into
//!   byte-aligned rows, the sub-byte sample layout shared by the TIFF-family raw formats.
//!
//! And the table side of length-limited canonical Huffman coding, in the ITU-T T.81 form
//! (`references/jpeg/itu-t81.pdf`) that JPEG and lossless JPEG (and so DNG) share:
//!
//! - [`build_optimal_table`] — the Annex K.2 optimal 16-bit length-limited table over measured
//!   symbol frequencies, for any alphabet of at most 256 symbols, as a `(BITS, HUFFVAL)` pair.
//! - [`CanonicalCodes`] — the Annex C canonical `(code, length)` of every symbol of a
//!   `(BITS, HUFFVAL)` pair.
//!
//! The forward-looking ANS coders and the JPEG XL prefix codes named in the workspace plan (for
//! AV2 / JPEG XL) are not implemented yet; they will join this crate behind their own modules.
#![forbid(unsafe_code)]

// Compiles README.md's code blocks as doctests; exists only while rustdoc collects them.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;

mod bitreader;
mod bitwriter;
mod huffman;
mod leb128;
mod samplepack;
mod symbol;

pub use bitreader::BitReader;
pub use bitwriter::BitWriter;
pub use huffman::{CanonicalCodes, build_optimal_table};
pub use leb128::{leb128_len, write_leb128};
pub use samplepack::{pack_msb_rows, row_bytes, unpack_msb_rows};
pub use symbol::{SymbolDecoder, SymbolEncoder};
