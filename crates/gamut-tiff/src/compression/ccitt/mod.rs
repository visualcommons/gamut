//! CCITT bilevel coding.
//!
//! This phase implements **Modified Huffman** (TIFF 6.0 §10, `Compression = 2`): each row is
//! coded as alternating white/black runs using the T.4 run-length tables ([`tables`]). A row
//! always begins with a (possibly zero-length) white run; a run is zero or more make-up codes
//! followed by exactly one terminating code; and each row starts on a byte boundary (no EOL, no
//! fill bits, no RTC). T.4/T.6 (Group 3/4) reuse these tables in a later phase.

mod g4;
mod tables;

use std::collections::HashMap;
use std::sync::OnceLock;

pub use g4::{g4_decode_strip, g4_encode_strip};
use gamut_bitstream::BitWriter;
use gamut_core::{Error, Result};

/// Parsed code tables: run → (codeword value, bit length) for encoding, and (bit length,
/// codeword value) → run for decoding, separately for white and black runs.
struct Codes {
    white_enc: HashMap<u16, (u32, u8)>,
    black_enc: HashMap<u16, (u32, u8)>,
    white_dec: HashMap<(u8, u32), u16>,
    black_dec: HashMap<(u8, u32), u16>,
}

/// Parses a MSB-first binary string into `(value, bit length)`.
///
/// Accumulated as `acc * 2 + bit` rather than `(acc << 1) | bit`. The two compute the same thing,
/// but the bitwise form had an equivalent mutant: `acc << 1` always leaves a zero in the low bit,
/// where `|` and `^` agree, so no test could tell them apart (#110). The arithmetic form has no
/// such twin -- every operator mutation of `*` or `+` changes the parsed code.
fn parse(code: &str) -> (u32, u8) {
    let value = code
        .bytes()
        .fold(0u32, |acc, b| acc * 2 + u32::from(b == b'1'));
    (value, code.len() as u8)
}

fn codes() -> &'static Codes {
    static CODES: OnceLock<Codes> = OnceLock::new();
    CODES.get_or_init(|| {
        let mut c = Codes {
            white_enc: HashMap::new(),
            black_enc: HashMap::new(),
            white_dec: HashMap::new(),
            black_dec: HashMap::new(),
        };
        for &(run, code) in tables::WHITE.iter().chain(tables::SHARED_MAKEUP) {
            let (v, l) = parse(code);
            c.white_enc.insert(run, (v, l));
            c.white_dec.insert((l, v), run);
        }
        for &(run, code) in tables::BLACK.iter().chain(tables::SHARED_MAKEUP) {
            let (v, l) = parse(code);
            c.black_enc.insert(run, (v, l));
            c.black_dec.insert((l, v), run);
        }
        c
    })
}

/// Reads `1` bit at `index` (MSB-first) from a packed row, or `0` past the end.
fn bit_at(row: &[u8], index: usize) -> u8 {
    (row[index / 8] >> (7 - (index % 8))) & 1
}

/// Emits one run of `length` pixels (zero or more make-up codes + one terminating code).
fn encode_run(out: &mut BitWriter, length: usize, white: bool) -> Result<()> {
    let codes = codes();
    let enc = if white {
        &codes.white_enc
    } else {
        &codes.black_enc
    };
    let put = |out: &mut BitWriter, run: u16| -> Result<()> {
        let &(v, l) = enc
            .get(&run)
            .ok_or_else(|| Error::unsupported(env!("CARGO_PKG_NAME"), "CCITT: missing run code"))?;
        out.put_bits(v, u32::from(l));
        Ok(())
    };
    // Runs longer than the largest make-up code repeat it, then one make-up code
    // takes the remaining multiple of 64 and a terminating code the rest. Computed rather than
    // looped on `r`: the former `while r >= 64 { r -= m }` never terminated when a mutation made
    // `m` zero (#585).
    for _ in 0..length / 2560 {
        put(out, 2560)?;
    }
    let mut r = length % 2560;
    if r >= 64 {
        put(out, (r / 64 * 64) as u16)?;
        r %= 64;
    }
    put(out, r as u16)
}

/// Modified-Huffman-encodes one packed bilevel row of `width` pixels into `out`, then byte-aligns.
///
/// Following the CCITT convention (and libtiff), a `0` bit is a white pixel; the image's
/// photometric interpretation is applied separately when the bits become samples.
///
/// One pass over the `width` pixels, closing the current run at each colour change: the walk is
/// bounded by the pixels it reads, where the former nested `while x < width { … x += 1 }` hung
/// under its `<`/`+=` mutations (#585). `width` is never zero (an image edge is non-zero), so the
/// final run is always a real one.
fn encode_row(out: &mut BitWriter, row: &[u8], width: usize) -> Result<()> {
    let white_bit = 0u8;
    // A row starts with a white run; a row that starts black opens with a zero-length one.
    let mut white = true;
    let mut run = 0;
    for x in 0..width {
        if (bit_at(row, x) == white_bit) != white {
            encode_run(out, run, white)?;
            white = !white;
            run = 0;
        }
        run += 1;
    }
    encode_run(out, run, white)?;
    out.byte_align();
    Ok(())
}

/// Modified-Huffman-encodes a strip of `packed` bilevel rows (`stored_row_bytes` each).
///
/// # Errors
///
/// Returns [`Error::Unsupported`] only on an internal table inconsistency (never for valid input).
pub fn mh_encode_strip(packed: &[u8], stored_row_bytes: usize, width: usize) -> Result<Vec<u8>> {
    let mut out = BitWriter::new();
    for row in packed.chunks(stored_row_bytes) {
        encode_row(&mut out, row, width)?;
    }
    Ok(out.into_bytes())
}

/// A MSB-first bit reader over a strip's coded bytes.
struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn read_bit(&mut self) -> Option<u8> {
        let byte = self.data.get(self.pos / 8)?;
        let bit = (byte >> (7 - (self.pos % 8))) & 1;
        self.pos += 1;
        Some(bit)
    }

    fn align_to_byte(&mut self) {
        self.pos = self.pos.div_ceil(8) * 8;
    }
}

/// Decodes one run-length code (white or black), returning its run length.
fn decode_code(r: &mut BitReader, white: bool) -> Result<u16> {
    let dec = if white {
        &codes().white_dec
    } else {
        &codes().black_dec
    };
    let mut value = 0u32;
    let mut len = 0u8;
    loop {
        let bit = r.read_bit().ok_or_else(|| {
            Error::invalid_input(env!("CARGO_PKG_NAME"), "CCITT: truncated code")
                .with_byte_offset((r.pos / 8) as u64)
        })?;
        // `value * 2 + bit`, not `(value << 1) | bit`: the shift leaves a zero in the low bit
        // where `|` and `^` agree, which made the bitwise form an equivalent mutant (#110).
        value = value * 2 + u32::from(bit);
        len += 1;
        if let Some(&run) = dec.get(&(len, value)) {
            return Ok(run);
        }
        if len >= 14 {
            return Err(
                Error::invalid_input(env!("CARGO_PKG_NAME"), "CCITT: invalid code")
                    .with_byte_offset((r.pos / 8) as u64),
            );
        }
    }
}

/// Decodes a full run (make-up codes + one terminating code) of the given colour.
fn decode_run(r: &mut BitReader, white: bool) -> Result<usize> {
    let mut total = 0usize;
    loop {
        let run = decode_code(r, white)? as usize;
        total += run;
        if run < 64 {
            return Ok(total);
        }
    }
}

/// Modified-Huffman-decodes a strip into `rows` packed bilevel rows of `width` pixels each.
///
/// # Errors
///
/// Returns [`Error::InvalidInput`] if the stream is truncated, a code is invalid, or a row's runs
/// do not sum to `width`.
pub fn mh_decode_strip(data: &[u8], rows: usize, width: usize) -> Result<Vec<u8>> {
    let stored_row_bytes = width.div_ceil(8);
    let white_bit = 0u8;
    let mut reader = BitReader { data, pos: 0 };
    let mut out = vec![0u8; stored_row_bytes * rows];
    for row in 0..rows {
        let dst = &mut out[row * stored_row_bytes..(row + 1) * stored_row_bytes];
        let mut pos = 0;
        let mut white = true;
        let mut previous_run_was_zero = false;
        while pos < width {
            let run = decode_run(&mut reader, white)?;
            if run == 0 {
                if previous_run_was_zero {
                    return Err(Error::invalid_input(
                        env!("CARGO_PKG_NAME"),
                        "CCITT: consecutive zero-length runs make no row progress",
                    )
                    .with_byte_offset((reader.pos / 8) as u64));
                }
                previous_run_was_zero = true;
            } else {
                previous_run_was_zero = false;
            }
            if pos + run > width {
                return Err(Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "CCITT: run overruns the row",
                )
                .with_byte_offset((reader.pos / 8) as u64));
            }
            // Asked as a bool, not as a colour code compared against 1. With `white_bit` in
            // {0, 1}, `1 - white_bit` yields {1, 0} and the mutation `1 + white_bit` yields
            // {1, 2}; since the test below was `== 1`, and neither 0 nor 2 is 1, the two behaved
            // identically and the mutant was unkillable (#110). Same shape as `g4::fill`.
            let set = if white {
                white_bit == 1
            } else {
                white_bit != 1
            };
            if set {
                for p in pos..pos + run {
                    dst[p / 8] |= 0x80 >> (p % 8);
                }
            }
            pos += run;
            white = !white;
        }
        reader.align_to_byte();
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(rows: &[Vec<u8>], width: usize) {
        let stored = width.div_ceil(8);
        let packed: Vec<u8> = rows.iter().flatten().copied().collect();
        let enc = mh_encode_strip(&packed, stored, width).expect("encode");
        let dec = mh_decode_strip(&enc, rows.len(), width).expect("decode");
        assert_eq!(dec, packed);
    }

    /// Packs a row of 0/1 pixel values into MSB-first bytes.
    fn pack(bits: &[u8]) -> Vec<u8> {
        let mut row = vec![0u8; bits.len().div_ceil(8)];
        for (i, &b) in bits.iter().enumerate() {
            if b != 0 {
                row[i / 8] |= 0x80 >> (i % 8);
            }
        }
        row
    }

    #[test]
    fn roundtrips_simple_rows() {
        // all white, all black, alternating, starts-black.
        let w = 16;
        roundtrip(&[pack(&[0; 16])], w);
        roundtrip(&[pack(&[1; 16])], w);
        let alt: Vec<u8> = (0..16).map(|i| (i % 2) as u8).collect();
        roundtrip(&[pack(&alt)], w);
        let starts_black: Vec<u8> = (0..16).map(|i| u8::from(i < 5)).collect();
        roundtrip(&[pack(&starts_black)], w);
    }

    /// A run whose length needs the make-up code for exactly 64.
    ///
    /// `decode_run` accumulates make-up codes and stops at the first *terminating* code, which is
    /// the test `run < 64`: terminating codes are 0..=63 and make-ups are multiples of 64. At
    /// exactly 64 the two forms disagree -- `<=` treats the make-up as terminating, returns
    /// early, and leaves the terminating code that follows it unread, desynchronising everything
    /// after.
    ///
    /// The existing long-run fixture uses 2700, which decomposes as 2560 + 128 + 12 and never
    /// produces a 64, so `<` and `<=` behaved identically there (#110). Runs of 64..=127 are the
    /// ones that use the 64 make-up, so this covers both ends of that band and the boundary just
    /// below it.
    #[test]
    fn roundtrips_runs_that_use_the_64_make_up_code() {
        let w = 256;
        for run in [63usize, 64, 65, 127, 128] {
            let mut bits = vec![0u8; w];
            for b in bits.iter_mut().take(run) {
                *b = 1;
            }
            let stored = w.div_ceil(8);
            let packed = pack(&bits);
            let enc = mh_encode_strip(&packed, stored, w).expect("encode");
            let dec = mh_decode_strip(&enc, 1, w).expect("decode");
            assert_eq!(dec, packed, "a black run of exactly {run} pixels");
        }
    }

    #[test]
    fn roundtrips_long_runs() {
        // A run longer than 64 forces a make-up code; > 2623 forces repeated 2560 make-ups.
        let w = 3000;
        let mut bits = vec![0u8; w];
        for b in bits.iter_mut().take(2700) {
            *b = 1;
        }
        roundtrip(&[pack(&bits)], w);
    }

    #[test]
    fn decode_errors_report_exact_byte_offsets() {
        let mut truncated = BitReader { data: &[], pos: 8 };
        let error = decode_code(&mut truncated, true).expect_err("truncated code");
        assert_eq!(error.byte_offset(), Some(1));
        assert!(
            error
                .static_message()
                .is_some_and(|message| message.contains("truncated code"))
        );

        let mut invalid = BitReader::new(&[0, 0]);
        let error = decode_code(&mut invalid, true).expect_err("invalid code");
        assert_eq!(error.byte_offset(), Some(1));
        assert!(
            error
                .static_message()
                .is_some_and(|message| message.contains("invalid code"))
        );

        let encoded = mh_encode_strip(&[0], 1, 2).expect("two-white-pixel row");
        let error = mh_decode_strip(&encoded, 1, 1).expect_err("run overrun");
        assert_eq!(error.byte_offset(), Some(0));
        assert!(
            error
                .static_message()
                .is_some_and(|message| message.contains("run overruns"))
        );

        let mut zeros = BitWriter::new();
        encode_run(&mut zeros, 1, true).expect("one white pixel");
        encode_run(&mut zeros, 0, false).expect("zero black run");
        encode_run(&mut zeros, 0, true).expect("zero white run");
        zeros.byte_align();
        let error = mh_decode_strip(&zeros.into_bytes(), 1, 2)
            .expect_err("consecutive zero runs must not hang");
        assert_eq!(error.byte_offset(), Some(3));
        assert!(
            error
                .static_message()
                .is_some_and(|message| message.contains("no row progress"))
        );
    }
}
