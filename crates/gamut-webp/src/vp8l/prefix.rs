//! VP8L canonical prefix (Huffman) codes (RFC 9649 §3.7).
//!
//! VP8L entropy-codes symbols with canonical prefix codes built from per-symbol code lengths. A
//! prefix-code group bundles five codes (green+length+cache, red, blue, alpha, distance), and meta
//! prefix codes select a group per block via an entropy image (§3.7.1-§3.7.3).
//!
//! # Bit order
//!
//! VP8L canonical codes are assigned in the usual increasing-by-(length, symbol) manner — the same
//! convention as DEFLATE — but written into an **LSB-first** stream. The encoder emits each code
//! *bit-reversed* to its length (see [`reverse_bits`]) so that the first bit on the wire is the
//! code's most-significant bit; the decoder ([`PrefixCode::read_symbol`]) reads bit by bit,
//! rebuilding the code MSB-first (the canonical "puff" decode), so no reversal is needed on read.
//!
//! # Single-symbol codes
//!
//! A code with a single used symbol is a complete tree that **consumes no bits** (RFC 9649 §3.7.2):
//! the symbol is implicit. Both [`PrefixEncoder::write_symbol`] (writes nothing) and
//! [`PrefixCode::read_symbol`] (returns the symbol without reading) honor this, so they stay in
//! lock-step. An empty alphabet is coded as a single symbol `0`.

use gamut_core::{Error, Result};

use crate::vp8l::bit_io::{BitReader, BitWriter};

/// Maximum prefix-code length in bits (RFC 9649 §3.7.2).
pub const MAX_CODE_LENGTH: usize = 15;
/// Number of literal symbols per channel (a full 8-bit byte).
pub const NUM_LITERAL_CODES: usize = 256;
/// Number of LZ77 length prefix codes packed into the green alphabet (§5.2.2).
pub const NUM_LENGTH_CODES: usize = 24;
/// Number of distance prefix codes (§5.2.2).
pub const NUM_DISTANCE_CODES: usize = 40;
/// Number of code-length code symbols (literals 0..=15 plus repeat codes 16/17/18) (§3.7.2).
pub const CODE_LENGTH_CODES: usize = 19;

/// The order in which code-length code lengths appear on the wire (RFC 9649 §3.7.2).
pub const CODE_LENGTH_CODE_ORDER: [usize; CODE_LENGTH_CODES] = [
    17, 18, 0, 1, 2, 3, 4, 5, 16, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15,
];

/// Default code length assumed by repeat-code 16 before any nonzero length is seen (§3.7.2).
const DEFAULT_CODE_LENGTH: u8 = 8;

/// Size of the green/length/cache alphabet for a given color-cache size (0 if the cache is off).
#[must_use]
pub fn green_alphabet_size(color_cache_size: usize) -> usize {
    NUM_LITERAL_CODES + NUM_LENGTH_CODES + color_cache_size
}

/// Reverses the low `num_bits` of `value` (used to emit canonical codes MSB-first into the
/// LSB-first stream).
#[must_use]
pub fn reverse_bits(value: u16, num_bits: u8) -> u16 {
    let mut v = value;
    let mut r = 0u16;
    for _ in 0..num_bits {
        r = (r << 1) | (v & 1);
        v >>= 1;
    }
    r
}

/// A canonical prefix (Huffman) decoder built from per-symbol code lengths (RFC 9649 §3.7.2).
///
/// Decoding uses the classic canonical algorithm (no large lookup table): bits are read one at a
/// time and accumulated MSB-first until they identify a symbol. A single-symbol code returns its
/// symbol without consuming any bits.
#[derive(Debug, Clone)]
pub struct PrefixCode {
    /// `counts[len]` = number of symbols coded with length `len`.
    counts: [u16; MAX_CODE_LENGTH + 1],
    /// Symbols sorted by `(length, symbol)`.
    symbols: Vec<u16>,
    /// Set for a single-symbol code (consumes 0 bits, always returns this symbol).
    single: Option<u16>,
}

impl PrefixCode {
    /// Builds a decoder from `code_lengths` (one entry per symbol; `0` = unused).
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidInput`] if a length exceeds [`MAX_CODE_LENGTH`], if no symbol is
    /// used, or if the lengths do not form a complete tree (the single-symbol leaf is the one
    /// permitted incomplete tree, per §3.7.2).
    pub fn from_code_lengths(code_lengths: &[u8]) -> Result<Self> {
        let mut counts = [0u16; MAX_CODE_LENGTH + 1];
        let mut n_used = 0usize;
        let mut last_used = 0u16;
        for (sym, &len) in code_lengths.iter().enumerate() {
            if len as usize > MAX_CODE_LENGTH {
                return Err(Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "VP8L: prefix code length too large",
                ));
            }
            if len > 0 {
                counts[len as usize] += 1;
                n_used += 1;
                last_used = sym as u16;
            }
        }
        if n_used == 0 {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "VP8L: empty prefix code",
            ));
        }
        if n_used == 1 {
            return Ok(Self {
                counts,
                symbols: Vec::new(),
                single: Some(last_used),
            });
        }
        // Completeness check (Kraft equality), over-subscription detected as a negative remainder.
        let mut left = 1i32;
        for &count in counts.iter().take(MAX_CODE_LENGTH + 1).skip(1) {
            left <<= 1;
            left -= i32::from(count);
            if left < 0 {
                return Err(Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "VP8L: over-subscribed prefix code",
                ));
            }
        }
        if left != 0 {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "VP8L: incomplete prefix code",
            ));
        }
        // Sort symbols by (length, symbol) into a flat table.
        let mut offsets = [0usize; MAX_CODE_LENGTH + 2];
        for len in 1..=MAX_CODE_LENGTH {
            offsets[len + 1] = offsets[len] + usize::from(counts[len]);
        }
        let mut symbols = vec![0u16; n_used];
        for (sym, &len) in code_lengths.iter().enumerate() {
            if len > 0 {
                let slot = &mut offsets[len as usize];
                symbols[*slot] = sym as u16;
                *slot += 1;
            }
        }
        Ok(Self {
            counts,
            symbols,
            single: None,
        })
    }

    /// Reads one symbol from `r`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidInput`] on truncation or if the bits do not match any code.
    pub fn read_symbol(&self, r: &mut BitReader<'_>) -> Result<u16> {
        if let Some(sym) = self.single {
            return Ok(sym);
        }
        let mut code: i32 = 0;
        let mut first: i32 = 0;
        let mut index: usize = 0;
        for len in 1..=MAX_CODE_LENGTH {
            code |= r.read_bit()? as i32;
            let count = i32::from(self.counts[len]);
            if code - first < count {
                let pos = index + (code - first) as usize;
                return self.symbols.get(pos).copied().ok_or_else(|| {
                    Error::invalid_input(
                        env!("CARGO_PKG_NAME"),
                        "VP8L: prefix code index out of range",
                    )
                });
            }
            index += count as usize;
            first += count;
            first <<= 1;
            code <<= 1;
        }
        Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "VP8L: invalid prefix code",
        ))
    }
}

/// Reads a single prefix code's lengths from the bitstream (simple or normal variant) and builds it
/// (RFC 9649 §3.7.2). `alphabet_size` bounds the symbols and `max_symbol`.
///
/// # Errors
///
/// Returns [`Error::InvalidInput`] on any malformed code-length coding or truncation.
pub fn read_prefix_code(r: &mut BitReader<'_>, alphabet_size: usize) -> Result<PrefixCode> {
    if r.read_bit()? == 1 {
        read_simple_prefix_code(r, alphabet_size)
    } else {
        read_normal_prefix_code(r, alphabet_size)
    }
}

/// Reads the *simple code length code* variant: 1 or 2 symbols, each with code length 1 (§3.7.2).
fn read_simple_prefix_code(r: &mut BitReader<'_>, alphabet_size: usize) -> Result<PrefixCode> {
    let num_symbols = r.read_bit()? + 1; // 1 or 2
    let is_first_8bits = r.read_bit()?;
    let mut lengths = vec![0u8; alphabet_size];
    let symbol0 = r.read_bits(1 + 7 * is_first_8bits)? as usize;
    if symbol0 >= alphabet_size {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "VP8L: simple prefix symbol out of range",
        ));
    }
    lengths[symbol0] = 1;
    if num_symbols == 2 {
        let symbol1 = r.read_bits(8)? as usize;
        if symbol1 >= alphabet_size {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "VP8L: simple prefix symbol out of range",
            ));
        }
        lengths[symbol1] = 1;
    }
    PrefixCode::from_code_lengths(&lengths)
}

/// Reads the *normal code length code* variant (§3.7.2): a meta code over `code_length_code_lengths`
/// drives literal lengths plus the repeat codes 16/17/18.
fn read_normal_prefix_code(r: &mut BitReader<'_>, alphabet_size: usize) -> Result<PrefixCode> {
    let num_code_lengths = 4 + r.read_bits(4)? as usize;
    if num_code_lengths > CODE_LENGTH_CODES {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "VP8L: too many code-length codes",
        ));
    }
    let mut cl_lengths = [0u8; CODE_LENGTH_CODES];
    for &order in CODE_LENGTH_CODE_ORDER.iter().take(num_code_lengths) {
        cl_lengths[order] = r.read_bits(3)? as u8;
    }
    let cl_code = PrefixCode::from_code_lengths(&cl_lengths)?;

    let mut max_symbol = if r.read_bit()? != 0 {
        let length_nbits = 2 + 2 * r.read_bits(3)?;
        2 + r.read_bits(length_nbits)? as usize
    } else {
        alphabet_size
    };
    if max_symbol > alphabet_size {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "VP8L: max_symbol exceeds alphabet",
        ));
    }

    let mut lengths = vec![0u8; alphabet_size];
    let mut prev_len = DEFAULT_CODE_LENGTH;
    let mut symbol = 0usize;
    while symbol < alphabet_size {
        if max_symbol == 0 {
            break;
        }
        max_symbol -= 1;
        let code = cl_code.read_symbol(r)?;
        if code < 16 {
            lengths[symbol] = code as u8;
            symbol += 1;
            if code != 0 {
                prev_len = code as u8;
            }
        } else {
            let (extra_bits, repeat_offset, value) = match code {
                16 => (2u32, 3usize, prev_len),
                17 => (3, 3, 0),
                18 => (7, 11, 0),
                _ => {
                    return Err(Error::invalid_input(
                        env!("CARGO_PKG_NAME"),
                        "VP8L: invalid code-length symbol",
                    ));
                }
            };
            let repeat = repeat_offset + r.read_bits(extra_bits)? as usize;
            if symbol + repeat > alphabet_size {
                return Err(Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "VP8L: code-length repeat overruns alphabet",
                ));
            }
            for _ in 0..repeat {
                lengths[symbol] = value;
                symbol += 1;
            }
        }
    }
    PrefixCode::from_code_lengths(&lengths)
}

/// The five canonical codes used to decode a pixel (RFC 9649 §3.7.1).
#[derive(Debug, Clone)]
pub struct PrefixCodeGroup {
    /// Green channel, LZ77 lengths, and color-cache indices.
    pub green: PrefixCode,
    /// Red channel.
    pub red: PrefixCode,
    /// Blue channel.
    pub blue: PrefixCode,
    /// Alpha channel.
    pub alpha: PrefixCode,
    /// LZ77 distance codes.
    pub distance: PrefixCode,
}

/// Reads a [`PrefixCodeGroup`] (five codes in bitstream order); the green alphabet grows by
/// `color_cache_size` (0 when the cache is off).
///
/// # Errors
///
/// Returns [`Error::InvalidInput`] on any malformed code or truncation.
pub fn read_prefix_code_group(
    r: &mut BitReader<'_>,
    color_cache_size: usize,
) -> Result<PrefixCodeGroup> {
    Ok(PrefixCodeGroup {
        green: read_prefix_code(r, green_alphabet_size(color_cache_size))?,
        red: read_prefix_code(r, NUM_LITERAL_CODES)?,
        blue: read_prefix_code(r, NUM_LITERAL_CODES)?,
        alpha: read_prefix_code(r, NUM_LITERAL_CODES)?,
        distance: read_prefix_code(r, NUM_DISTANCE_CODES)?,
    })
}

// --- Encoder side ---------------------------------------------------------------------------------

/// Derives the canonical (bit-reversed, ready-to-emit) codes for each symbol from its `lengths`.
///
/// Each returned code is reversed to its length so it can be written LSB-first with
/// [`BitWriter::write_bits`]; unused symbols (length 0) get code 0.
#[must_use]
pub fn canonical_codes(lengths: &[u8]) -> Vec<u16> {
    let mut bl_count = [0u32; MAX_CODE_LENGTH + 1];
    for &len in lengths {
        if len > 0 && (len as usize) <= MAX_CODE_LENGTH {
            bl_count[len as usize] += 1;
        }
    }
    let mut next_code = [0u32; MAX_CODE_LENGTH + 1];
    let mut code = 0u32;
    for bits in 1..=MAX_CODE_LENGTH {
        code = (code + bl_count[bits - 1]) << 1;
        next_code[bits] = code;
    }
    let mut codes = vec![0u16; lengths.len()];
    for (sym, &len) in lengths.iter().enumerate() {
        if len > 0 && (len as usize) <= MAX_CODE_LENGTH {
            let c = next_code[len as usize];
            next_code[len as usize] += 1;
            codes[sym] = reverse_bits(c as u16, len);
        }
    }
    codes
}

/// Builds length-limited (`<= max_len`) canonical Huffman code lengths from a symbol `histogram`.
///
/// Returns one length per symbol (`0` = unused). An empty histogram yields all-zero lengths (the
/// caller codes that as a single symbol `0`); a single nonzero symbol gets length 1. To bound the
/// maximum length, the histogram counts are raised toward a common floor and the tree rebuilt until
/// it fits (libwebp's approach) — this yields *a* valid code, not necessarily the optimal one
/// (density tuning is deferred to issue #31).
#[must_use]
pub fn build_length_limited_lengths(histogram: &[u32], max_len: u8) -> Vec<u8> {
    let n = histogram.len();
    let used = histogram.iter().filter(|&&h| h > 0).count();
    if used == 0 {
        return vec![0u8; n];
    }
    if used == 1 {
        let mut lengths = vec![0u8; n];
        if let Some(sym) = (0..n).find(|&i| histogram[i] > 0) {
            lengths[sym] = 1;
        }
        return lengths;
    }
    let mut count_min = 1u32;
    loop {
        let depths = huffman_pass(histogram, count_min);
        let max_depth = depths.iter().copied().max().unwrap_or(0);
        if max_depth <= u32::from(max_len) {
            return depths.iter().map(|&d| d as u8).collect();
        }
        count_min = count_min.saturating_mul(2);
    }
}

/// One Huffman construction pass; returns per-symbol depths (code lengths) with each present
/// symbol's weight floored to `count_min` (raising the floor flattens the tree, capping its depth).
fn huffman_pass(histogram: &[u32], count_min: u32) -> Vec<u32> {
    use std::cmp::Reverse;
    use std::collections::BinaryHeap;

    /// A node in the Huffman tree (`sym >= 0` marks a leaf).
    struct Node {
        left: i32,
        right: i32,
        sym: i32,
    }

    let n = histogram.len();
    let mut lengths = vec![0u32; n];
    let mut nodes: Vec<Node> = Vec::new();
    // Min-heap keyed by (weight, tie-break index) for deterministic output.
    let mut heap: BinaryHeap<Reverse<(u64, usize)>> = BinaryHeap::new();
    for (sym, &count) in histogram.iter().enumerate() {
        if count > 0 {
            let idx = nodes.len();
            nodes.push(Node {
                left: -1,
                right: -1,
                sym: sym as i32,
            });
            heap.push(Reverse((u64::from(count.max(count_min)), idx)));
        }
    }
    if nodes.len() == 1 {
        if let Some(sym) = (0..n).find(|&i| histogram[i] > 0) {
            lengths[sym] = 1;
        }
        return lengths;
    }
    while heap.len() > 1 {
        let (Some(Reverse((wa, a))), Some(Reverse((wb, b)))) = (heap.pop(), heap.pop()) else {
            break;
        };
        let idx = nodes.len();
        nodes.push(Node {
            left: a as i32,
            right: b as i32,
            sym: -1,
        });
        heap.push(Reverse((wa + wb, idx)));
    }
    let Some(Reverse((_, root))) = heap.pop() else {
        return lengths;
    };
    // Assign depths with an explicit stack (the tree can be up to `used` deep).
    let mut stack = vec![(root, 0u32)];
    while let Some((idx, depth)) = stack.pop() {
        let Some(node) = nodes.get(idx) else { continue };
        if node.sym >= 0 {
            if let Some(slot) = lengths.get_mut(node.sym as usize) {
                *slot = depth;
            }
        } else {
            stack.push((node.left as usize, depth + 1));
            stack.push((node.right as usize, depth + 1));
        }
    }
    lengths
}

/// An encoder-side prefix code: per-symbol emit patterns + lengths, with the single-symbol
/// (0-bit) special case tracked so emission stays in lock-step with [`PrefixCode::read_symbol`].
#[derive(Debug, Clone)]
pub struct PrefixEncoder {
    lengths: Vec<u8>,
    codes: Vec<u16>,
    single: bool,
}

impl PrefixEncoder {
    /// Builds an encoder from per-symbol `lengths`.
    #[must_use]
    pub fn from_lengths(lengths: &[u8]) -> Self {
        let codes = canonical_codes(lengths);
        let single = lengths.iter().filter(|&&l| l > 0).count() <= 1;
        Self {
            lengths: lengths.to_vec(),
            codes,
            single,
        }
    }

    /// Per-symbol code lengths (one entry per symbol; `0` = unused).
    #[must_use]
    pub fn lengths(&self) -> &[u8] {
        &self.lengths
    }

    /// Writes `symbol` to `w`. A single-symbol code writes nothing (0 bits).
    pub fn write_symbol(&self, w: &mut BitWriter, symbol: usize) {
        if self.single {
            return;
        }
        if let (Some(&code), Some(&len)) = (self.codes.get(symbol), self.lengths.get(symbol)) {
            w.write_bits(u32::from(code), u32::from(len));
        }
    }
}

/// One way of describing a set of code lengths on the wire. Every variant reconstructs *exactly*
/// the same lengths, so the choice between them is pure density (RFC 9649 §3.7.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Description {
    /// The *simple code length code*: 1 or 2 symbols, implicitly at code length 1.
    Simple,
    /// The *normal code length code*: the lengths are themselves prefix-coded.
    ///
    /// `run_coded` uses the repeat codes 16/17/18 rather than emitting every length literally;
    /// `trim` sets an explicit `max_symbol` so trailing zero lengths are not coded at all.
    Normal { run_coded: bool, trim: bool },
}

/// The four description variants, cheapest-looking first (order only breaks exact ties).
const DESCRIPTIONS: [Description; 5] = [
    Description::Simple,
    Description::Normal {
        run_coded: true,
        trim: true,
    },
    Description::Normal {
        run_coded: true,
        trim: false,
    },
    Description::Normal {
        run_coded: false,
        trim: true,
    },
    Description::Normal {
        run_coded: false,
        trim: false,
    },
];

/// A code-length code symbol plus its extra-bit payload, as the reader consumes it.
struct ClSymbol {
    symbol: u8,
    extra_bits: u32,
    extra_value: u32,
}

/// The symbols used by `lengths`, and whether the *simple* description can carry them.
///
/// The simple description codes the second symbol in a fixed 8-bit field and the first in 1 or 8
/// bits, so it can only express symbols `<= 255` — the green alphabet runs past that once a colour
/// cache is in play, which is exactly the case this guard exists for.
fn simple_symbols(lengths: &[u8]) -> Option<Vec<u16>> {
    let used: Vec<usize> = (0..lengths.len()).filter(|&i| lengths[i] > 0).collect();
    if used.is_empty() || used.len() > 2 {
        return None;
    }
    // Both leaves of a two-symbol code sit at depth 1, and a lone leaf is the 0-bit single-symbol
    // code; anything else is not what `Simple` reconstructs, so decline rather than corrupt.
    if used.iter().any(|&i| lengths[i] != 1) || used.iter().any(|&i| i > 0xff) {
        return None;
    }
    Some(used.iter().map(|&i| i as u16).collect())
}

/// Encodes `lengths[..count]` as a code-length symbol stream, optionally using the repeat codes.
///
/// Mirrors [`read_normal_prefix_code`]'s state machine exactly. Repeat code 16 repeats the last
/// **nonzero** length the reader saw, and the reader does not update that on a zero, so a 16 is
/// only ever emitted directly after the literal it repeats — never across an intervening zero run.
fn code_length_symbols(lengths: &[u8], count: usize, run_coded: bool) -> Vec<ClSymbol> {
    let literal = |value: u8| ClSymbol {
        symbol: value,
        extra_bits: 0,
        extra_value: 0,
    };
    let mut out = Vec::new();
    if !run_coded {
        out.extend(lengths.iter().take(count).copied().map(literal));
        return out;
    }
    let mut i = 0;
    while i < count {
        let value = lengths[i];
        let run = lengths[i..count]
            .iter()
            .take_while(|&&l| l == value)
            .count();
        i += run;
        if value == 0 {
            let mut left = run;
            while left >= 11 {
                let repeat = left.min(138);
                out.push(ClSymbol {
                    symbol: 18,
                    extra_bits: 7,
                    extra_value: (repeat - 11) as u32,
                });
                left -= repeat;
            }
            while left >= 3 {
                let repeat = left.min(10);
                out.push(ClSymbol {
                    symbol: 17,
                    extra_bits: 3,
                    extra_value: (repeat - 3) as u32,
                });
                left -= repeat;
            }
            out.extend((0..left).map(|_| literal(0)));
        } else {
            // The literal comes first so `prev_len` is this value before any 16 refers to it.
            out.push(literal(value));
            let mut left = run - 1;
            while left >= 3 {
                let repeat = left.min(6);
                out.push(ClSymbol {
                    symbol: 16,
                    extra_bits: 2,
                    extra_value: (repeat - 3) as u32,
                });
                left -= repeat;
            }
            out.extend((0..left).map(|_| literal(value)));
        }
    }
    out
}

/// The `(length_nbits_selector, length_nbits)` pair that can carry an explicit `max_symbol` of
/// `count`, or `None` if `count` is out of the field's reach.
///
/// The reader recovers `max_symbol` as `2 + read_bits(2 + 2 * selector)`, so `count` must be at
/// least 2 and `count - 2` must fit the chosen field.
fn max_symbol_field(count: usize) -> Option<(u32, u32)> {
    if count < 2 {
        return None;
    }
    (0..8u32).find_map(|selector| {
        let nbits = 2 + 2 * selector;
        ((count - 2) < (1usize << nbits)).then_some((selector, nbits))
    })
}

/// Writes `lengths` under `description`, or returns `false` without writing if that description
/// cannot express them.
fn write_description(w: &mut BitWriter, lengths: &[u8], description: Description) -> bool {
    match description {
        Description::Simple => {
            let Some(symbols) = simple_symbols(lengths) else {
                return false;
            };
            w.write_bits(1, 1); // 1 = simple code length code.
            w.write_bits((symbols.len() - 1) as u32, 1);
            let symbol0 = symbols[0];
            let is_first_8bits = u32::from(symbol0 > 1);
            w.write_bits(is_first_8bits, 1);
            w.write_bits(u32::from(symbol0), 1 + 7 * is_first_8bits);
            if let Some(&symbol1) = symbols.get(1) {
                w.write_bits(u32::from(symbol1), 8);
            }
            true
        }
        Description::Normal { run_coded, trim } => {
            // Trimming describes only up to the last used symbol; the reader leaves the rest zero.
            let count = if trim {
                match lengths.iter().rposition(|&l| l > 0) {
                    Some(last) => last + 1,
                    None => return false,
                }
            } else {
                lengths.len()
            };
            let symbols = code_length_symbols(lengths, count, run_coded);
            // `max_symbol` bounds how many code-length symbols the reader consumes, not how many
            // lengths they expand to, so it is the length of this stream.
            let field = if trim {
                match max_symbol_field(symbols.len()) {
                    Some(field) => Some(field),
                    None => return false,
                }
            } else {
                None
            };

            let mut cl_hist = [0u32; CODE_LENGTH_CODES];
            for s in &symbols {
                cl_hist[s.symbol as usize] += 1;
            }
            // The meta code's lengths are emitted in 3-bit fields, so they must fit in 7 bits.
            let cl_lengths = build_length_limited_lengths(&cl_hist, 7);
            let cl_encoder = PrefixEncoder::from_lengths(&cl_lengths);

            w.write_bits(0, 1); // 0 = normal (not simple) code length code.
            // Emit the meta code lengths in CODE_LENGTH_CODE_ORDER, trimming trailing zeros (min 4).
            let mut num_code_lengths = CODE_LENGTH_CODES;
            while num_code_lengths > 4
                && cl_lengths[CODE_LENGTH_CODE_ORDER[num_code_lengths - 1]] == 0
            {
                num_code_lengths -= 1;
            }
            w.write_bits((num_code_lengths - 4) as u32, 4);
            for &order in CODE_LENGTH_CODE_ORDER.iter().take(num_code_lengths) {
                w.write_bits(u32::from(cl_lengths[order]), 3);
            }

            match field {
                Some((selector, nbits)) => {
                    w.write_bits(1, 1);
                    w.write_bits(selector, 3);
                    w.write_bits((symbols.len() - 2) as u32, nbits);
                }
                None => w.write_bits(0, 1), // max_symbol uses the alphabet default.
            }
            for s in &symbols {
                cl_encoder.write_symbol(w, s.symbol as usize);
                w.write_bits(s.extra_value, s.extra_bits);
            }
            true
        }
    }
}

/// Writes the code description for `lengths` in whichever encoding is smallest (RFC 9649 §3.7.2).
///
/// The candidates are the *simple* code (1-2 symbols), and the *normal* code with the lengths
/// emitted literally or run-compressed with codes 16/17/18, each with and without an explicit
/// `max_symbol` trimming trailing zero lengths. All of them reconstruct identical lengths, so
/// picking the shortest is a pure density win — measured by writing each into a scratch
/// [`BitWriter`] and comparing [`BitWriter::bit_len`], the crate's keep-the-smallest idiom.
///
/// This matters far more than it looks: an alphabet with a single used symbol (an unused distance
/// code, or a constant alpha channel) costs 274 bits described literally and 4 bits as a simple
/// code, and a prefix-code group carries five descriptions.
pub fn write_prefix_code(w: &mut BitWriter, lengths: &[u8]) {
    let mut best: Option<(usize, Description)> = None;
    for &description in &DESCRIPTIONS {
        let mut scratch = BitWriter::new();
        if !write_description(&mut scratch, lengths, description) {
            continue;
        }
        let bits = scratch.bit_len();
        if best.is_none_or(|(best_bits, _)| bits < best_bits) {
            best = Some((bits, description));
        }
    }
    // `Normal { run_coded: false, trim: false }` can describe any length vector, so `best` is only
    // `None` for an empty alphabet, which no caller produces.
    if let Some((_, description)) = best {
        write_description(w, lengths, description);
    }
}

/// Writes a prefix code for 1 or 2 symbols using the *simple code length code* (RFC 9649 §3.7.2),
/// bypassing [`write_prefix_code`]'s choice. Each listed symbol is given code length 1.
///
/// Test-only: it is how the decoder suites hand-build synthetic streams. Production encoding goes
/// through [`write_prefix_code`], which reaches the same encoding when it is the smallest.
#[cfg(test)]
pub fn write_simple_prefix_code(w: &mut BitWriter, symbols: &[u16]) {
    let mut lengths = vec![0u8; 256];
    for &symbol in symbols.iter().take(2) {
        lengths[symbol as usize] = 1;
    }
    assert!(
        write_description(w, &lengths, Description::Simple),
        "test helper called with symbols the simple code cannot express"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_codes_and_decoder_agree_with_unused_symbols() {
        use crate::vp8l::bit_io::{BitReader, BitWriter};
        // A complete code (Kraft sum = 1) with gaps (length-0 unused symbols) and several lengths, so
        // the canonical-code construction's `len > 0 && len <= MAX` filter and the decoder's offset
        // prefix-sum are exercised. canonical_codes (encoder, LSB-first) and from_code_lengths
        // (decoder) are independent implementations of the same canonical algorithm, so writing each
        // symbol's code and reading it back must recover the symbol — pinning both.
        let lengths: &[u8] = &[2, 0, 1, 3, 0, 3]; // 1/2 + 1/4 + 1/8 + 1/8 = 1; symbols 1 and 4 unused
        let codes = canonical_codes(lengths);
        let decoder = PrefixCode::from_code_lengths(lengths).expect("complete code");
        for (sym, &len) in lengths.iter().enumerate() {
            if len == 0 {
                continue;
            }
            let mut w = BitWriter::new();
            w.write_bits(u32::from(codes[sym]), u32::from(len));
            let bytes = w.finish();
            assert_eq!(
                decoder.read_symbol(&mut BitReader::new(&bytes)).unwrap() as usize,
                sym,
                "symbol {sym} must round-trip through canonical_codes + from_code_lengths"
            );
        }
    }

    #[test]
    fn reverse_bits_matches_manual() {
        assert_eq!(reverse_bits(0b1, 1), 0b1);
        assert_eq!(reverse_bits(0b10, 2), 0b01);
        assert_eq!(reverse_bits(0b1011, 4), 0b1101);
        assert_eq!(reverse_bits(0b0000_0001, 8), 0b1000_0000);
        // Reversing twice is the identity for the given width.
        for v in 0u16..256 {
            assert_eq!(reverse_bits(reverse_bits(v, 8), 8), v);
        }
    }

    /// Round-trips a symbol stream through an encoder built from a histogram and a decoder built
    /// from the same lengths, exercising `build_length_limited_lengths` + `canonical_codes`.
    fn assert_code_round_trips(histogram: &[u32], stream: &[usize], max_len: u8) {
        let lengths = build_length_limited_lengths(histogram, max_len);
        assert!(lengths.iter().all(|&l| l <= max_len), "length exceeds cap");
        let encoder = PrefixEncoder::from_lengths(&lengths);
        let mut w = BitWriter::new();
        for &s in stream {
            encoder.write_symbol(&mut w, s);
        }
        let bytes = w.finish();
        let decoder = PrefixCode::from_code_lengths(&lengths).expect("valid lengths");
        let mut r = BitReader::new(&bytes);
        for &s in stream {
            assert_eq!(decoder.read_symbol(&mut r).unwrap() as usize, s);
        }
    }

    #[test]
    fn round_trips_varied_histograms() {
        // Uniform, skewed, two-symbol, and a single-symbol alphabet.
        let uniform: Vec<u32> = vec![1; 16];
        assert_code_round_trips(&uniform, &[0, 5, 15, 3, 8, 8, 0], 15);

        let mut skewed = vec![1u32; 32];
        skewed[7] = 10_000;
        skewed[19] = 2_000;
        assert_code_round_trips(&skewed, &[7, 7, 19, 0, 31, 7], 15);

        let mut two = vec![0u32; 256];
        two[10] = 5;
        two[200] = 9;
        assert_code_round_trips(&two, &[10, 200, 10, 10, 200], 15);

        let mut single = vec![0u32; 40];
        single[12] = 99;
        // Single-symbol code consumes no bits, so the stream decodes regardless of length.
        assert_code_round_trips(&single, &[12, 12, 12], 15);
    }

    #[test]
    fn forces_and_caps_15_bit_lengths() {
        // A Fibonacci-like distribution drives natural Huffman lengths well past 15; the limiter
        // must still cap them.
        let mut hist = vec![0u32; 64];
        let (mut a, mut b) = (1u32, 1u32);
        for h in hist.iter_mut() {
            *h = a;
            let next = a.saturating_add(b);
            a = b;
            b = next;
        }
        let lengths = build_length_limited_lengths(&hist, 15);
        assert!(lengths.iter().all(|&l| l <= 15));
        // Still a usable, complete code.
        let stream: Vec<usize> = (0..64).collect();
        assert_code_round_trips(&hist, &stream, 15);
    }

    #[test]
    fn normal_code_length_coding_round_trips() {
        // Build a code, serialize it with write_prefix_code, read it back, and confirm the
        // reconstructed decoder agrees on a symbol stream.
        let mut hist = vec![0u32; 256];
        for (i, h) in hist.iter_mut().enumerate() {
            *h = (i as u32 % 7) + 1;
        }
        let lengths = build_length_limited_lengths(&hist, 15);
        let encoder = PrefixEncoder::from_lengths(&lengths);

        let stream: Vec<usize> = vec![0, 1, 2, 100, 255, 17, 42, 42, 7];
        let mut w = BitWriter::new();
        write_prefix_code(&mut w, &lengths);
        for &s in &stream {
            encoder.write_symbol(&mut w, s);
        }
        let bytes = w.finish();

        let mut r = BitReader::new(&bytes);
        let decoder = read_prefix_code(&mut r, 256).expect("valid code description");
        for &s in &stream {
            assert_eq!(decoder.read_symbol(&mut r).unwrap() as usize, s);
        }
    }

    #[test]
    fn simple_code_length_coding_round_trips() {
        for symbols in [
            vec![0u16],
            vec![1u16],
            vec![5u16],
            vec![3u16, 200],
            vec![0u16, 1],
        ] {
            let mut lengths = vec![0u8; 256];
            for &s in &symbols {
                lengths[s as usize] = 1;
            }
            let encoder = PrefixEncoder::from_lengths(&lengths);
            let stream: Vec<usize> = symbols.iter().map(|&s| s as usize).collect();

            let mut w = BitWriter::new();
            write_simple_prefix_code(&mut w, &symbols);
            for &s in &stream {
                encoder.write_symbol(&mut w, s);
            }
            let bytes = w.finish();

            let mut r = BitReader::new(&bytes);
            let decoder = read_prefix_code(&mut r, 256).expect("valid simple code");
            for &s in &stream {
                assert_eq!(decoder.read_symbol(&mut r).unwrap() as usize, s);
            }
        }
    }

    /// Round-trips `lengths` through one explicit [`Description`], returning its size in bits.
    /// Panics if the description cannot express the lengths.
    fn round_trip_description(lengths: &[u8], description: Description) -> usize {
        let mut w = BitWriter::new();
        assert!(
            write_description(&mut w, lengths, description),
            "{description:?} declined lengths it should accept"
        );
        let bits = w.bit_len();
        let bytes = w.finish();
        let mut r = BitReader::new(&bytes);
        let decoder = read_prefix_code(&mut r, lengths.len()).expect("valid code description");
        // Every used symbol must decode back to itself, which is the property that matters: the
        // description is only correct if it reconstructs the very lengths the encoder coded with.
        let encoder = PrefixEncoder::from_lengths(lengths);
        for (symbol, _) in lengths.iter().enumerate().filter(|&(_, &l)| l > 0) {
            let mut w2 = BitWriter::new();
            encoder.write_symbol(&mut w2, symbol);
            let payload = w2.finish();
            let mut r2 = BitReader::new(&payload);
            assert_eq!(
                decoder.read_symbol(&mut r2).expect("symbol decodes") as usize,
                symbol,
                "{description:?} lost symbol {symbol}"
            );
        }
        bits
    }

    /// Four runs of four length-4 symbols, separated by zero runs: a *complete* code (16 leaves at
    /// depth 4) that still forces both repeat codes — 18/17 across the gaps and 16 inside each run.
    fn grouped_runs() -> Vec<u8> {
        let mut lengths = vec![0u8; 256];
        for group in 0..4usize {
            for i in 0..4usize {
                lengths[group * 20 + i] = 4;
            }
        }
        lengths
    }

    #[test]
    fn every_description_reconstructs_the_same_code() {
        // The four encodings are interchangeable by construction, so each must round-trip the same
        // lengths. Cases chosen to exercise each one's edge: a lone symbol, two symbols, long zero
        // runs (17/18), long equal-nonzero runs (16), and a run that straddles both.
        let mut lone = vec![0u8; 256];
        lone[0] = 1;
        let mut two = vec![0u8; 256];
        two[3] = 1;
        two[200] = 1;
        let uniform = vec![8u8; 256];
        let mut trailing = vec![0u8; 280];
        for l in trailing.iter_mut().take(4) {
            *l = 2;
        }
        for lengths in [lone, two, grouped_runs(), uniform, trailing] {
            for description in DESCRIPTIONS {
                if write_description(&mut BitWriter::new(), &lengths, description) {
                    round_trip_description(&lengths, description);
                }
            }
            // And the chooser's output must round-trip too, whichever it picked.
            let mut w = BitWriter::new();
            write_prefix_code(&mut w, &lengths);
            let bytes = w.finish();
            let mut r = BitReader::new(&bytes);
            read_prefix_code(&mut r, lengths.len()).expect("chosen description is readable");
        }
    }

    #[test]
    fn a_single_symbol_code_costs_four_bits() {
        // The headline density win: a lone used symbol (an unused distance code, or a constant
        // alpha channel) is 4 bits as a simple code where the literal normal code spends 274.
        // Pinned absolutely, so any change that stops reaching the simple code is caught.
        let mut lengths = vec![0u8; 256];
        lengths[0] = 1;
        let mut w = BitWriter::new();
        write_prefix_code(&mut w, &lengths);
        assert_eq!(w.bit_len(), 4);
        assert_eq!(
            round_trip_description(
                &lengths,
                Description::Normal {
                    run_coded: false,
                    trim: false
                }
            ),
            274
        );
    }

    #[test]
    fn the_simple_description_declines_symbols_it_cannot_code() {
        // The second simple symbol rides in a fixed 8-bit field, so a green alphabet extended by a
        // colour cache can hold used symbols beyond 255. Coding those as `Simple` would silently
        // truncate them, so the guard must decline and the chooser must fall back.
        let mut lengths = vec![0u8; green_alphabet_size(64)];
        lengths[300] = 1;
        assert_eq!(simple_symbols(&lengths), None);
        assert!(!write_description(
            &mut BitWriter::new(),
            &lengths,
            Description::Simple
        ));
        round_trip_description(
            &lengths,
            Description::Normal {
                run_coded: true,
                trim: true,
            },
        );

        // Two symbols where only one is out of range is equally undescribable.
        let mut mixed = vec![0u8; green_alphabet_size(64)];
        mixed[7] = 1;
        mixed[280] = 1;
        assert_eq!(simple_symbols(&mixed), None);
    }

    #[test]
    fn repeat_code_16_never_crosses_a_zero_run() {
        // The reader updates `prev_len` only on a nonzero code, so a 16 emitted after a zero run
        // would repeat the wrong value. Assert the stream always re-states the literal first.
        let lengths = grouped_runs();
        let symbols = code_length_symbols(&lengths, lengths.len(), true);
        let mut prev_nonzero = DEFAULT_CODE_LENGTH;
        for s in &symbols {
            if s.symbol == 16 {
                assert_eq!(
                    prev_nonzero, 4,
                    "a 16 repeated {prev_nonzero}, not the intended length"
                );
            }
            if s.symbol < 16 && s.symbol != 0 {
                prev_nonzero = s.symbol;
            }
        }
        assert!(symbols.iter().any(|s| s.symbol == 16), "run coding fired");
        round_trip_description(
            &lengths,
            Description::Normal {
                run_coded: true,
                trim: true,
            },
        );
    }

    #[test]
    fn max_symbol_field_picks_the_narrowest_that_fits() {
        // The reader recovers `max_symbol` as `2 + read_bits(2 + 2 * selector)`, so counts below 2
        // are unrepresentable and each selector step widens the field by two bits.
        assert_eq!(max_symbol_field(0), None);
        assert_eq!(max_symbol_field(1), None);
        assert_eq!(max_symbol_field(2), Some((0, 2)));
        assert_eq!(max_symbol_field(5), Some((0, 2)));
        assert_eq!(max_symbol_field(6), Some((1, 4)));
        assert_eq!(max_symbol_field(17), Some((1, 4)));
        assert_eq!(max_symbol_field(18), Some((2, 6)));
    }

    #[test]
    fn rejects_malformed_lengths() {
        // Over-subscribed: three length-1 codes (Kraft sum > 1).
        assert!(matches!(
            PrefixCode::from_code_lengths(&[1, 1, 1]),
            Err(error) if error.kind() == gamut_core::ErrorKind::InvalidInput
        ));
        // Incomplete: a length-1 and a length-2 code leave the tree under-filled.
        assert!(matches!(
            PrefixCode::from_code_lengths(&[1, 2]),
            Err(error) if error.kind() == gamut_core::ErrorKind::InvalidInput
        ));
        // Length beyond the 15-bit cap.
        assert!(matches!(
            PrefixCode::from_code_lengths(&[16, 0]),
            Err(error) if error.kind() == gamut_core::ErrorKind::InvalidInput
        ));
        // Empty alphabet.
        assert!(matches!(
            PrefixCode::from_code_lengths(&[0, 0, 0]),
            Err(error) if error.kind() == gamut_core::ErrorKind::InvalidInput
        ));
    }

    #[test]
    fn single_symbol_consumes_no_bits() {
        let code = PrefixCode::from_code_lengths(&[0, 0, 3, 0]).expect("single leaf");
        let mut r = BitReader::new(&[]); // no data at all
        assert_eq!(code.read_symbol(&mut r).unwrap(), 2);
        assert_eq!(code.read_symbol(&mut r).unwrap(), 2);
    }

    #[test]
    fn green_alphabet_size_includes_cache() {
        assert_eq!(green_alphabet_size(0), 280);
        assert_eq!(green_alphabet_size(1024), 280 + 1024);
    }

    #[test]
    fn reads_prefix_code_group() {
        // Emit five trivial single-symbol codes (each consumes no data) and read them as a group.
        let mut w = BitWriter::new();
        for _ in 0..5 {
            write_simple_prefix_code(&mut w, &[0]);
        }
        let bytes = w.finish();
        let mut r = BitReader::new(&bytes);
        let group = read_prefix_code_group(&mut r, 0).expect("group");
        let mut rr = BitReader::new(&[]);
        assert_eq!(group.green.read_symbol(&mut rr).unwrap(), 0);
        assert_eq!(group.distance.read_symbol(&mut rr).unwrap(), 0);
    }
}
