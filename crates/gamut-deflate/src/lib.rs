//! `gamut-deflate` — a pure-Rust **DEFLATE** (RFC 1951) and **zlib** (RFC 1950) *encoder*.
//!
//! DEFLATE is the compression engine behind PNG's `IDAT`/`zTXt`/`iCCP`, TIFF's `Compression=8`,
//! gzip, and zlib streams generally. This crate provides it as a shared primitive, sitting below the
//! codec crates alongside [`gamut-bitstream`](https://docs.rs/gamut-bitstream) in the workspace's
//! dependency graph, with no internal dependencies of its own.
//!
//! # Space efficiency
//!
//! This is a *space-optimizing* encoder, not a general-purpose one: encode time is traded for ratio
//! (see [`Level`]). [`Level::Best`] runs a zopfli-style optimal parse — a shortest-path LZ77 search
//! against an iteratively refined entropy model — with per-block dynamic Huffman codes and
//! cost-driven block splitting, and reliably produces **smaller** output than zlib at maximum effort:
//! measured at roughly 1–7% below `zlib -9` (and below `miniz_oxide` at its top level) across text,
//! source, and mixed inputs. That size win — not speed — is the reason to reach for this crate rather
//! than delegate to [`miniz_oxide`](https://docs.rs/miniz_oxide) or the
//! [`zopfli`](https://docs.rs/zopfli) crate; `README.md` carries the head-to-head numbers. Lower
//! levels trade ratio back for speed.
//!
//! # Encoder only
//!
//! Following gamut's encoder-first philosophy, this crate **does not decode**. Inflating DEFLATE is a
//! thoroughly solved problem — `miniz_oxide` is a safe, fuzzed, pure-Rust decoder already used across
//! the ecosystem — and decompressing untrusted input is a security-sensitive surface best left to a
//! hardened implementation. Decoders in this workspace that need inflate (the DNG and PNG decoders
//! today; a TIFF `Compression=8` decoder in future) depend on `miniz_oxide` directly. Encoder
//! correctness is instead proven differentially: the dev-only `zlib-oracle` inflates this crate's
//! output with the canonical C `zlib` and asserts it round-trips to the original bytes.
//!
//! # Scope
//!
//! - **Formats:** raw DEFLATE (RFC 1951) via [`DeflateEncoder::compress`] and the zlib wrapper
//!   (RFC 1950) via [`DeflateEncoder::zlib_compress`]. gzip framing (RFC 1952) is out of scope — no
//!   gamut image format uses it — as is its CRC-32; zlib's integrity check is the Adler-32 exposed
//!   here as [`adler32`] (PNG's unrelated chunk CRC-32 lives in `gamut-png`).
//! - **One-shot:** the whole input is compressed in a single call into a caller-owned `Vec<u8>`;
//!   there is no streaming/incremental encoder, which suits whole-image encoding.
//!
//! # Example
//!
//! ```
//! use gamut_deflate::{DeflateEncoder, Level};
//!
//! let data = b"the quick brown fox jumps over the lazy dog".repeat(8);
//! let mut zlib_stream = Vec::new();
//! let written = DeflateEncoder::new()
//!     .with_level(Level::Best)
//!     .zlib_compress(&data, &mut zlib_stream);
//! assert_eq!(written, zlib_stream.len());
//! // A zlib stream starts with the 0x78 CMF byte.
//! assert_eq!(zlib_stream[0], 0x78);
//! ```
// `deny`, not `forbid`, because this crate is on an encode hot path: a measured win may take
// the exception (AGENTS.md, `## Conventions`). None does today — this is 100% safe Rust.
#![deny(unsafe_code)]

// Compiles README.md's code blocks as doctests; exists only while rustdoc collects them.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;

mod adler32;
mod bitwriter;
mod block;
mod dynamic;
mod encoder;
mod huffman;
mod lz77;
mod symbols;
mod zlib;

pub use adler32::adler32;
pub use encoder::{DeflateEncoder, Level};
