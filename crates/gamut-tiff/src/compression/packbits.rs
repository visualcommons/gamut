//! PackBits compression (TIFF 6.0 §9, `Compression = 32773`).
//!
//! PackBits is a simple byte-oriented run-length scheme. A control byte `n` (read as a signed
//! `i8`) means: `0..=127` → copy the next `n + 1` bytes literally; `-127..=-1` → copy the next
//! single byte `1 - n` times; `-128` → no-op. Each image row is packed independently (runs never
//! cross a row boundary), so the encoder works one row at a time.

use gamut_core::{Error, Result};

/// Returns the length (capped at 128) of the run of bytes equal to `data[i]` starting at `i`.
///
/// Counted over the slice rather than by a hand-advanced cursor, so the walk is bounded by the
/// data it reads: the former `while … { len += 1 }` turned into a non-terminating loop under the
/// `+= -> *=` mutation (#585). The rewrite also retires that loop's operator mutants.
fn run_length(data: &[u8], i: usize) -> usize {
    let b = data[i];
    data[i..].iter().take(128).take_while(|&&x| x == b).count()
}

/// PackBits-encodes one row, appending to `out`. Runs never cross the row boundary (§9).
pub fn encode_row(row: &[u8], out: &mut Vec<u8>) {
    let mut i = 0;
    while i < row.len() {
        let run = run_length(row, i);
        if run >= 2 {
            // Replicate run: control = -(run - 1), stored as a `u8`.
            out.push((1i32 - run as i32) as i8 as u8);
            out.push(row[i]);
            i += run;
        } else {
            // Literal run: bytes up to the next run of ≥2, capped at 128. Counted over the
            // positions it may cover rather than by stepping `i` in a loop, which hung under the
            // `+= -> *=` mutation (#585).
            let start = i;
            let limit = row.len().min(start + 128);
            i += (start..limit)
                .take_while(|&j| run_length(row, j) < 2)
                .count();
            out.push((i - start - 1) as u8);
            out.extend_from_slice(&row[start..i]);
        }
    }
}

/// PackBits-decodes `src` until exactly `expected` bytes are produced.
///
/// # Errors
///
/// Returns [`Error::InvalidInput`] if `src` is truncated or decodes to the wrong length.
pub fn decode(src: &[u8], expected: usize) -> Result<Vec<u8>> {
    // Cap the pre-allocation so a malformed `expected` can't reserve a huge buffer up front.
    let mut out = Vec::with_capacity(expected.min(1 << 16));
    let mut i = 0;
    while out.len() < expected {
        let n = *src.get(i).ok_or_else(|| {
            Error::invalid_input(env!("CARGO_PKG_NAME"), "PackBits: truncated control byte")
        })? as i8;
        i += 1;
        if n >= 0 {
            let count = n as usize + 1;
            let chunk = src.get(i..i + count).ok_or_else(|| {
                Error::invalid_input(env!("CARGO_PKG_NAME"), "PackBits: truncated literal run")
            })?;
            out.extend_from_slice(chunk);
            i += count;
        } else if n != -128 {
            let count = (1 - i32::from(n)) as usize;
            let b = *src.get(i).ok_or_else(|| {
                Error::invalid_input(env!("CARGO_PKG_NAME"), "PackBits: truncated replicate run")
            })?;
            i += 1;
            out.resize(out.len() + count, b);
        }
    }
    if out.len() != expected {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "PackBits: decoded length mismatch",
        ));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A run of exactly two ends the literal that precedes it.
    ///
    /// The literal scanner runs `while run_length(row, i) < 2`, and two is the boundary it turns
    /// on. Relaxing it to `<= 2` absorbs the pair into the literal instead of emitting a
    /// replicate -- still valid PackBits that decodes to the same row, so every round-trip stays
    /// green, and the existing byte-exact tests use runs of 100 and 200, which never place a
    /// two-run beside a literal (#110).
    ///
    /// Neither rule dominates: here ours costs a byte, because breaking the literal adds a second
    /// control byte; on a row of nothing but pairs it saves one per pair. Ours is the simple
    /// always-replicate rule, and this test is what makes that a decision rather than an
    /// accident.
    #[test]
    fn a_run_of_exactly_two_is_emitted_as_a_replicate() {
        let mut out = Vec::new();
        encode_row(&[1, 2, 3, 4, 4, 5, 6], &mut out);
        assert_eq!(
            out,
            vec![
                2, 1, 2, 3, // literal: three bytes, stopped by the pair
                0xFF, 4, // replicate: control 1 - 2 = -1, then the byte
                1, 5, 6, // literal: the remaining two
            ]
        );
    }

    fn roundtrip(row: &[u8]) {
        let mut enc = Vec::new();
        encode_row(row, &mut enc);
        let dec = decode(&enc, row.len()).expect("decode");
        assert_eq!(dec, row);
    }

    /// A run of identical bytes is encoded as a *replicate*, not as literals.
    ///
    /// Every encoder test went through the round-trip helper, which decodes and compares to the
    /// input -- and that cannot see an encoder which stops compressing. Replacing `run_length`
    /// with a constant 0 or 1 makes `encode_row` take the literal branch for every byte: the
    /// output is still valid PackBits, still decodes to exactly the input, and is *larger than
    /// the row it encodes*. Both constants survived the whole suite (#110).
    ///
    /// So this asserts the bytes, not the round trip. A 100-byte run is two bytes: the control
    /// `1 - 100 = -99`, then the value.
    #[test]
    fn a_run_is_encoded_as_a_replicate_not_as_literals() {
        let row = vec![0xABu8; 100];
        let mut out = Vec::new();
        encode_row(&row, &mut out);

        assert_eq!(
            out,
            vec![(1i32 - 100) as i8 as u8, 0xAB],
            "a 100-byte run is one replicate pair"
        );
        assert!(
            out.len() < row.len(),
            "an encoder that does not compress is not an encoder"
        );
    }

    /// Runs longer than 128 are split, because `run_length` caps there (§9's control-byte range).
    ///
    /// 200 identical bytes are two replicates -- 128 then 72 -- not one oversized control byte.
    #[test]
    fn a_run_longer_than_128_is_split_at_the_cap() {
        let row = vec![0x5Au8; 200];
        let mut out = Vec::new();
        encode_row(&row, &mut out);

        assert_eq!(
            out,
            vec![
                (1i32 - 128) as i8 as u8,
                0x5A,
                (1i32 - 72) as i8 as u8,
                0x5A,
            ],
            "128 then 72, each as its own replicate pair"
        );
    }

    /// A row that alternates never repeats, so it is all literals -- the other side of the branch.
    ///
    /// Without this, capping the literal path would look like an improvement rather than a bug.
    #[test]
    fn a_row_without_runs_is_all_literals() {
        let row: Vec<u8> = (0..100u8).collect();
        let mut out = Vec::new();
        encode_row(&row, &mut out);

        assert_eq!(out[0], 99, "a single literal block of 100 bytes");
        assert_eq!(&out[1..], &row[..]);
        assert_eq!(
            out.len(),
            row.len() + 1,
            "literals cost exactly one control byte"
        );
    }

    #[test]
    fn roundtrips_runs_and_literals() {
        roundtrip(&[]);
        roundtrip(&[5]);
        roundtrip(&[7, 7, 7, 7, 7]);
        roundtrip(&[1, 2, 3, 4, 5]);
        roundtrip(&[9, 9, 1, 2, 9, 9, 9, 3]);
        roundtrip(&[0xAA; 300]); // run longer than 128
        let mixed: Vec<u8> = (0..200).map(|i| (i % 7) as u8).collect();
        roundtrip(&mixed);
        roundtrip(&[42; 128]);
        roundtrip(&[42; 129]);
    }

    #[test]
    fn decode_rejects_truncation_and_overrun() {
        assert!(decode(&[], 4).is_err());
        assert!(decode(&[0x00], 4).is_err()); // literal of 1 but no data byte
        // A literal claiming 5 bytes when only 4 are expected overruns.
        assert!(decode(&[0x04, 1, 2, 3, 4, 5], 4).is_err());
    }
}
