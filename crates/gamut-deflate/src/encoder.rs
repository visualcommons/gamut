//! The public DEFLATE encoder: a [`DeflateEncoder`] builder with a [`Level`] knob and the
//! `compress` (raw RFC 1951) / `zlib_compress` (RFC 1950) entry points.

use crate::adler32::adler32;
use crate::{block, dynamic, lz77, zlib};

/// Compression effort, trading encode time for output size. Every level produces a correct stream;
/// they differ only in ratio.
///
/// The discriminants are permanent and append-only, so the enum maps directly onto a C ABI;
/// `#[non_exhaustive]` holds callers to that, so a level appended later is not a breaking change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
#[non_exhaustive]
pub enum Level {
    /// Stored (uncompressed) blocks only — the always-correct floor and an upper bound on size.
    Store = 0,
    /// Fast: greedy matching with fixed Huffman codes.
    Fast = 1,
    /// Balanced default: lazy matching with per-block dynamic Huffman codes.
    #[default]
    Default = 2,
    /// Smallest output: a zopfli-style optimal parse with per-block dynamic Huffman codes and
    /// cost-driven block splitting. Slowest; intended for write-once assets where size dominates.
    Best = 3,
}

/// A reusable DEFLATE / zlib encoder configured by a [`Level`]. Plain `Copy` configuration: it
/// owns no buffers, so passing it by value costs nothing.
#[derive(Debug, Clone, Copy)]
pub struct DeflateEncoder {
    level: Level,
    deflate_passes: u8,
    optimal_parse_limit: usize,
}

impl Default for DeflateEncoder {
    fn default() -> Self {
        Self::new()
    }
}

impl DeflateEncoder {
    /// The default [`Level::Best`] pass budget: the number of cost-model refinement passes the
    /// optimal parse runs unless overridden by [`DeflateEncoder::with_deflate_passes`].
    pub const DEFAULT_DEFLATE_PASSES: u8 = 6;

    /// The default [`Level::Best`] optimal-parse limit: the largest span the shortest-path parse
    /// handles in one piece, unless overridden by [`DeflateEncoder::with_optimal_parse_limit`].
    ///
    /// 1 MiB, chosen so a single span's dynamic program stays cheap in working set — 22 MiB of
    /// parse state at most (see [`DeflateEncoder::with_optimal_parse_limit`] for the arithmetic).
    /// Inputs larger than this are parsed as consecutive spans of this size rather than falling
    /// back to lazy matching, so total input size never decides whether the optimal parse runs.
    pub const DEFAULT_OPTIMAL_PARSE_LIMIT: usize = 1 << 20;

    /// Creates an encoder at [`Level::Default`].
    #[must_use]
    pub fn new() -> Self {
        Self {
            level: Level::Default,
            deflate_passes: Self::DEFAULT_DEFLATE_PASSES,
            optimal_parse_limit: Self::DEFAULT_OPTIMAL_PARSE_LIMIT,
        }
    }

    /// Sets the compression [`Level`].
    #[must_use]
    pub fn with_level(mut self, level: Level) -> Self {
        self.level = level;
        self
    }

    /// Sets the [`Level::Best`] pass budget: the maximum number of cost-model refinement passes the
    /// zopfli-style optimal parse runs (default [`DeflateEncoder::DEFAULT_DEFLATE_PASSES`];
    /// `zopfli`'s own default budget is 15).
    ///
    /// `0` skips refinement entirely and emits the lazy seed parse — still with `Best`'s full
    /// match-finder depth and block splitting, and always a correct stream. Higher values only cost
    /// time: passes stop early once the parse reaches a fixed point. The knob is ignored at every
    /// other level.
    #[must_use]
    pub fn with_deflate_passes(mut self, passes: u8) -> Self {
        self.deflate_passes = passes;
        self
    }

    /// Sets the [`Level::Best`] optimal-parse limit: the largest span the shortest-path parse
    /// handles in one piece (default [`DeflateEncoder::DEFAULT_OPTIMAL_PARSE_LIMIT`]).
    ///
    /// Input longer than the limit is parsed as consecutive spans of this size, each with its own
    /// refined cost model and each free to reference the history before it. Raising the limit
    /// lets one cost model span more data — sometimes a small ratio win on homogeneous input, and
    /// sometimes a small loss: the sign is a property of the data. A limit below the 32 KiB LZ77
    /// window is raised to it: a shorter span would re-prime more history than it parses.
    ///
    /// **What the limit costs is memory, not time.** The shortest-path parse holds three
    /// span-length vectors for each refinement pass — the cost row, the chosen length and the
    /// chosen distance — which is 12 bytes for every byte of span. Beside them it keeps the span's
    /// match-finder results, searched once and read back by every pass: 2 bytes per position plus
    /// 4 per match run there, capped at two runs per span byte on average (positions past the cap
    /// are searched again in each pass instead, with the same result). So the working set is
    /// **at most 22 bytes for every byte of span** — about 16 to 19 on the synthetic photograph,
    /// flat and periodic material #959 measured at 0.6 to 1.1 runs per byte, and about 20.5
    /// resident on `gamut-png`'s filtered `photo_rgb` stream, whose runs come nearer the cap
    /// (`gamut-png`'s `STATUS.md` has the figures) — and the widest span is
    /// `min(max(limit, 32 KiB), input length)`. The limit is the only bound on it: set it past
    /// the input's length (`usize::MAX`) and one encode's peak memory grows linearly and without
    /// bound in the input. The parse's work is linear in the input whatever the span, so wall
    /// time at a fixed [`with_deflate_passes`](Self::with_deflate_passes) budget stays roughly
    /// flat as the limit rises — `gamut-png` measured peak resident set 177.9 → 701.8 MiB
    /// against 133.9 → 132.7 s on a 4096x4096 RGB photograph, default limit against none, before
    /// the match results were kept (#959) — though a wider span can take more refinement passes to converge on some
    /// material, so time is data-dependent rather than strictly flat.
    ///
    /// The knob is ignored at every other level.
    #[must_use]
    pub fn with_optimal_parse_limit(mut self, limit: usize) -> Self {
        self.optimal_parse_limit = limit;
        self
    }

    /// Encodes `data` as a raw DEFLATE stream (RFC 1951), appending to `out` and returning the
    /// number of bytes written. Any input — including empty — produces a valid stream.
    pub fn compress(&self, data: &[u8], out: &mut Vec<u8>) -> usize {
        let start = out.len();
        self.write_body(data, out);
        out.len() - start
    }

    /// Encodes `data` as a zlib stream (RFC 1950): a 2-byte header, the DEFLATE body, and a
    /// big-endian Adler-32 trailer. Appends to `out` and returns the number of bytes written. This
    /// is the stream PNG's `IDAT` carries.
    pub fn zlib_compress(&self, data: &[u8], out: &mut Vec<u8>) -> usize {
        let start = out.len();
        out.extend_from_slice(&zlib::header(self.level));
        self.write_body(data, out);
        out.extend_from_slice(&adler32(1, data).to_be_bytes());
        out.len() - start
    }

    /// Appends the DEFLATE body for `data` to `out`, choosing the smallest block encoding the
    /// level offers. The winner is chosen among complete candidates, so it is built in its own
    /// buffer and appended once; no further intermediate copy is made.
    fn write_body(&self, data: &[u8], out: &mut Vec<u8>) {
        let body = match self.level {
            // The uncompressed floor.
            Level::Store => block::stored(data),
            // LZ77 parse, then keep the smallest of stored / fixed-Huffman / dynamic-Huffman.
            // `Best` additionally runs the optimal parse and splits into multiple dynamic blocks.
            Level::Fast | Level::Default | Level::Best => {
                let chain = self.max_chain();
                let tokens = if matches!(self.level, Level::Best) {
                    lz77::parse_optimal(
                        data,
                        chain,
                        u32::from(self.deflate_passes),
                        self.optimal_parse_limit,
                    )
                } else {
                    // Fast = greedy, Default = lazy.
                    lz77::parse(data, chain, matches!(self.level, Level::Default))
                };
                let mut best = block::stored(data);
                let fixed = block::fixed(&tokens);
                if fixed.len() < best.len() {
                    best = fixed;
                }
                // Best splits into multiple dynamic blocks where it saves bits; the others use a
                // single block (already at/below zlib-9).
                let dynamic = if matches!(self.level, Level::Best) {
                    dynamic::multi_body(&tokens)
                } else {
                    dynamic::body(&tokens)
                };
                if dynamic.len() < best.len() {
                    best = dynamic;
                }
                best
            }
        };
        out.extend_from_slice(&body);
    }

    /// LZ77 match-finder search depth for this level — the time/ratio knob.
    fn max_chain(&self) -> usize {
        match self.level {
            Level::Store => 0,
            Level::Fast => 16,
            Level::Default => 128,
            Level::Best => 1024,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The body `write_body` appends to an empty buffer.
    fn body_of(encoder: DeflateEncoder, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        encoder.write_body(data, &mut out);
        out
    }

    #[test]
    fn compress_appends_and_reports_length() {
        let mut out = vec![0xEE];
        let n = DeflateEncoder::new().compress(b"abc", &mut out);
        assert_eq!(n, out.len() - 1);
        assert_eq!(out[0], 0xEE, "existing bytes are preserved");
    }

    #[test]
    fn zlib_compress_has_header_and_trailer() {
        let data = b"abcabcabc";
        let mut out = Vec::new();
        let n = DeflateEncoder::new().zlib_compress(data, &mut out);
        assert_eq!(n, out.len());
        assert_eq!(out[0], 0x78, "zlib CMF byte");
        // Trailer is the big-endian Adler-32 of the *uncompressed* data.
        let trailer = &out[out.len() - 4..];
        assert_eq!(trailer, adler32(1, data).to_be_bytes());
    }

    /// The default optimal-parse limit is a documented number — README, STATUS.md and the
    /// `with_optimal_parse_limit` doc all name 1 MiB — so it is pinned rather than merely derived.
    #[test]
    fn default_optimal_parse_limit_is_one_mebibyte() {
        assert_eq!(DeflateEncoder::DEFAULT_OPTIMAL_PARSE_LIMIT, 1_048_576);
    }

    #[test]
    fn fixed_huffman_beats_stored_on_ascii_text() {
        // All-ASCII bytes (< 144) get 8-bit fixed codes, so a fixed block undercuts stored's
        // per-block byte overhead. The encoder must pick it.
        let data = b"the quick brown fox jumps over the lazy dog. ".repeat(40);
        let mut fixed = Vec::new();
        DeflateEncoder::new()
            .with_level(Level::Fast)
            .zlib_compress(&data, &mut fixed);
        let mut store = Vec::new();
        DeflateEncoder::new()
            .with_level(Level::Store)
            .zlib_compress(&data, &mut store);
        assert!(
            fixed.len() < store.len(),
            "fixed {} should beat stored {}",
            fixed.len(),
            store.len()
        );
    }

    /// `zlib_compress` reports what it appended, not the buffer's new length: with bytes already
    /// in `out` the two differ, so `out.len() - start` cannot be read as `out.len() + start`.
    #[test]
    fn zlib_compress_reports_only_the_bytes_it_appended() {
        let mut out = vec![0xEE; 3];
        let n = DeflateEncoder::new().zlib_compress(b"abc", &mut out);
        assert_eq!(n, out.len() - 3);
    }

    /// When the fixed-Huffman body is exactly as long as the stored one, `write_body` keeps the
    /// stored body: a candidate replaces the incumbent only when strictly shorter. Every fourth
    /// byte value is 64 distinct bytes, so there is no match: 36 below 144 at 8 fixed bits and 28
    /// above at 9, a fixed body of `3 + 36 * 8 + 28 * 9 + 7 = 550` bits, 69 bytes — the stored
    /// body's `5 + 64`. Spread that thinly, the alphabet makes a dynamic header far too dear.
    #[test]
    fn write_body_keeps_stored_over_an_equally_long_fixed_body() {
        let data: Vec<u8> = (0..=255).step_by(4).collect();
        let encoder = DeflateEncoder::new().with_level(Level::Fast);
        let tokens = lz77::parse(&data, encoder.max_chain(), false);
        let stored = block::stored(&data);
        assert_eq!(
            block::fixed(&tokens).len(),
            stored.len(),
            "fixture must tie"
        );
        assert!(
            dynamic::body(&tokens).len() > stored.len(),
            "dynamic must not win"
        );
        assert_eq!(body_of(encoder, &data), stored);
    }

    /// When the dynamic body is exactly as long as the best so far, `write_body` keeps the
    /// incumbent — here the fixed body, both 20 bytes against the stored body's 23.
    #[test]
    fn write_body_keeps_fixed_over_an_equally_long_dynamic_body() {
        let data = b"ecbbcbeabebbdabadb";
        let encoder = DeflateEncoder::new().with_level(Level::Default);
        let tokens = lz77::parse(data, encoder.max_chain(), true);
        let fixed = block::fixed(&tokens);
        assert_eq!(
            dynamic::body(&tokens).len(),
            fixed.len(),
            "fixture must tie"
        );
        assert!(
            fixed.len() < block::stored(data).len(),
            "fixed must beat stored"
        );
        assert_eq!(body_of(encoder, data), fixed);
    }
}
