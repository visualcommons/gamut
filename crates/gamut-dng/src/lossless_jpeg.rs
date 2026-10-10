//! Lossless JPEG (ITU-T T.81 process 14, SOF3) — the standard DNG raw compression (`Compression =
//! 7`).
//!
//! This is the Huffman-coded, prediction-based *lossless* JPEG (not the lossy DCT codec): each
//! sample's prediction error is Huffman-coded by magnitude category plus mantissa bits, exactly
//! as a JPEG DC coefficient. The mosaic / linear planes map to JPEG components (one per
//! `SamplesPerPixel`), interleaved in a single scan. Differences are reduced modulo 2^16 so they
//! always fit a category `0..=16`, and reconstruction wraps to match the reference decoder.
//!
//! **Decode** covers the full process-14 envelope a conformant DNG reader needs — any predictor
//! `Ss = 1..=7` (T.81 Table H.1), the point transform (`Al`), per-component Huffman tables
//! (multi-table `DHT` segments, up to four destinations), and restart markers (`DRI`/`RSTn`) —
//! and is differentially tested against the Adobe DNG SDK's codec. Non-interleaved multi-scan
//! files, subsampled components, and DNL-deferred heights are rejected as unsupported.
//!
//! **Encode** deliberately stays a valid subset — predictor 1, one Huffman table, no point
//! transform, no restarts — which every reader must accept; the wider decode surface exists for
//! files other writers produce.

use gamut_core::{Error, Result};

/// A decoded lossless-JPEG frame.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LosslessJpeg {
    /// Samples per line.
    pub width: usize,
    /// Number of lines.
    pub height: usize,
    /// Interleaved components per sample.
    pub components: usize,
    /// Sample precision in bits (SOF3 `P`, `2..=16`).
    pub precision: u16,
    /// Interleaved samples, row-major, `width * height * components` long. The point transform,
    /// if any, is already applied (values are shifted back up).
    pub samples: Vec<u16>,
}

// JPEG markers.
const MARKER: u8 = 0xFF;
const SOI: u8 = 0xD8;
const EOI: u8 = 0xD9;
const SOF3: u8 = 0xC3;
const DHT: u8 = 0xC4;
const SOS: u8 = 0xDA;
const DRI: u8 = 0xDD;
const RST0: u8 = 0xD0;

/// A fixed, valid Huffman table over the 17 magnitude categories (`SSSS = 0..=16`).
///
/// `BITS[i]` (1-indexed) is the number of codes of length `i`; here 15 codes of length 4 and 2 of
/// length 5 (Kraft sum `15/16 + 2/32 = 1`). The table is written into the DHT, so the decoder uses
/// whatever we emit — correctness does not depend on it being optimal.
const BITS: [u8; 16] = [0, 0, 0, 15, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
const HUFFVAL: [u8; 17] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];

/// Canonical Huffman codes per symbol: `(code, length)` keyed by symbol value.
fn canonical_codes(bits: &[u8; 16], huffval: &[u8]) -> Vec<(u16, u8)> {
    // Code lengths in HUFFVAL order (JPEG Annex C: generate_size_table).
    let mut sizes = Vec::new();
    for (len_minus_1, &count) in bits.iter().enumerate() {
        for _ in 0..count {
            sizes.push((len_minus_1 + 1) as u8);
        }
    }
    // Codes (generate_code_table): ascending within each length.
    let mut table = vec![(0u16, 0u8); 256];
    let mut code: u16 = 0;
    let mut k = 0;
    let mut length = sizes.first().copied().unwrap_or(0);
    while k < sizes.len() {
        while k < sizes.len() && sizes[k] == length {
            table[huffval[k] as usize] = (code, length);
            code += 1;
            k += 1;
        }
        code <<= 1;
        length += 1;
    }
    table
}

/// MSB-first bit writer with JPEG `FF` → `FF 00` byte stuffing.
struct BitWriter {
    out: Vec<u8>,
    acc: u32,
    nbits: u32,
}

impl BitWriter {
    fn new() -> Self {
        Self {
            out: Vec::new(),
            acc: 0,
            nbits: 0,
        }
    }

    fn put(&mut self, value: u32, count: u8) {
        if count == 0 {
            return;
        }
        self.acc |= (value & ((1u32 << count) - 1)) << (32 - self.nbits - u32::from(count));
        self.nbits += u32::from(count);
        while self.nbits >= 8 {
            let byte = (self.acc >> 24) as u8;
            self.out.push(byte);
            if byte == MARKER {
                self.out.push(0x00); // stuff
            }
            self.acc <<= 8;
            self.nbits -= 8;
        }
    }

    fn finish(mut self) -> Vec<u8> {
        // `put` drains whole bytes, so `nbits` is always in `0..8` here.
        if self.nbits > 0 {
            // Pad the final partial byte with 1-bits (JPEG convention).
            self.put(0xFF, 8 - self.nbits as u8);
        }
        self.out
    }
}

/// The magnitude category (`SSSS`) of a difference and its mantissa bits.
fn magnitude(diff: i32) -> (u8, u32) {
    if diff == 0 {
        return (0, 0);
    }
    if diff == -32768 {
        return (16, 0); // T.81 special case: category 16 carries no mantissa.
    }
    let magnitude = diff.unsigned_abs();
    let ssss = (32 - magnitude.leading_zeros()) as u8;
    // Mantissa: diff for non-negative, diff - 1 (i.e. one's-complement low bits) for negative.
    let mantissa = if diff >= 0 { diff } else { diff - 1 } as u32;
    (ssss, mantissa & ((1u32 << ssss) - 1))
}

/// Reduces a raw prediction error to the canonical `[-32768, 32767]` range (mod 2^16).
fn reduce(diff: i32) -> i32 {
    if diff < -32768 {
        diff + 65536
    } else if diff > 32767 {
        diff - 65536
    } else {
        diff
    }
}

/// The T.81 prediction for the sample at `(x, y)` of component `c` (Table H.1 + the boundary
/// rules of §H.1.2), over already-reconstructed `samples` in the point-transformed domain.
///
/// `predictor` is the scan's `Ss` (`1..=7`); `precision`/`pt` give the default prediction
/// `2^(P - Pt - 1)`. `(origin_y, origin_x)` is where the current restart interval began (the
/// start of the scan, or the sample after the latest `RSTn`): the interval's first sample
/// predicts the default, the rest of that line predicts from the left neighbour, the first
/// sample of each later line predicts from above, and everywhere else the selected predictor
/// applies.
#[allow(clippy::too_many_arguments)] // mirrors the T.81 prediction context verbatim
fn predict(
    samples: &[u16],
    width: usize,
    comp: usize,
    x: usize,
    y: usize,
    c: usize,
    precision: u16,
    pt: u16,
    predictor: u8,
    origin: (usize, usize),
) -> i32 {
    let at = |xx: usize, yy: usize| i32::from(samples[(yy * width + xx) * comp + c]);
    let (origin_y, origin_x) = origin;
    if y == origin_y {
        if x == origin_x {
            return 1i32 << (precision - pt - 1);
        }
        return at(x - 1, y); // rest of the interval's first line: Ra
    }
    if x == 0 {
        return at(x, y - 1); // first sample of other lines: Rb
    }
    let ra = at(x - 1, y);
    let rb = at(x, y - 1);
    let rc = at(x - 1, y - 1);
    match predictor {
        1 => ra,
        2 => rb,
        3 => rc,
        4 => ra + rb - rc,
        5 => ra + ((rb - rc) >> 1),
        6 => rb + ((ra - rc) >> 1),
        // Validated to `1..=7` when the scan header is parsed.
        _ => (ra + rb) >> 1,
    }
}

/// Encodes interleaved `samples` (`width * height * components`) as a lossless JPEG at
/// `precision` bits per sample, using predictor 1, a single Huffman table, and no point
/// transform or restarts (a valid encoder free choice — see the module docs).
///
/// # Errors
///
/// Returns [`Error::InvalidInput`] if `precision` is not `2..=16`, `components` is not `1..=4`,
/// a dimension is zero or exceeds 65535, or `samples.len()` is not
/// `width * height * components`.
pub fn encode(
    samples: &[u16],
    width: usize,
    height: usize,
    components: usize,
    precision: u16,
) -> Result<Vec<u8>> {
    if !(2..=16).contains(&precision) {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "DNG: lossless-JPEG precision must be 2..=16",
        ));
    }
    if !(1..=4).contains(&components) {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "DNG: lossless JPEG carries 1..=4 interleaved components",
        ));
    }
    if width == 0 || height == 0 || width > 65535 || height > 65535 {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "DNG: lossless-JPEG dimensions must be 1..=65535",
        ));
    }
    if samples.len() != width * height * components {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "DNG: lossless-JPEG sample count must be width * height * components",
        ));
    }
    let codes = canonical_codes(&BITS, &HUFFVAL);
    let mut out = Vec::new();
    out.extend_from_slice(&[MARKER, SOI]);

    // SOF3: lossless frame.
    out.extend_from_slice(&[MARKER, SOF3]);
    let sof_len = 8 + 3 * components;
    out.extend_from_slice(&(sof_len as u16).to_be_bytes());
    out.push(precision as u8);
    out.extend_from_slice(&(height as u16).to_be_bytes());
    out.extend_from_slice(&(width as u16).to_be_bytes());
    out.push(components as u8);
    for c in 0..components {
        out.push((c + 1) as u8); // component id
        out.push(0x11); // H=1, V=1
        out.push(0x00); // quantization table (unused in lossless)
    }

    // DHT: one DC table (class 0, id 0).
    out.extend_from_slice(&[MARKER, DHT]);
    let dht_len = 2 + 1 + 16 + HUFFVAL.len();
    out.extend_from_slice(&(dht_len as u16).to_be_bytes());
    out.push(0x00); // Tc=0 (DC/lossless), Th=0
    out.extend_from_slice(&BITS);
    out.extend_from_slice(&HUFFVAL);

    // SOS: all components, predictor selector 1.
    out.extend_from_slice(&[MARKER, SOS]);
    let sos_len = 6 + 2 * components;
    out.extend_from_slice(&(sos_len as u16).to_be_bytes());
    out.push(components as u8);
    for c in 0..components {
        out.push((c + 1) as u8); // component selector
        out.push(0x00); // Td=0, Ta=0
    }
    out.push(1); // Ss = predictor 1
    out.push(0); // Se
    out.push(0); // Ah=0, Al (point transform) = 0

    let mut writer = BitWriter::new();
    for y in 0..height {
        for x in 0..width {
            for c in 0..components {
                let actual = i32::from(samples[(y * width + x) * components + c]);
                let prediction =
                    predict(samples, width, components, x, y, c, precision, 0, 1, (0, 0));
                let diff = reduce(actual - prediction);
                let (ssss, mantissa) = magnitude(diff);
                let (code, len) = codes[ssss as usize];
                writer.put(u32::from(code), len);
                // Categories 0 and 16 carry no mantissa; only `1..16` has magnitude bits.
                if ssss != 0 && ssss < 16 {
                    writer.put(mantissa, ssss);
                }
            }
        }
    }
    out.extend_from_slice(&writer.finish());
    out.extend_from_slice(&[MARKER, EOI]);
    Ok(out)
}

/// MSB-first bit reader that unstuffs `FF 00` and treats other markers as end-of-stream.
struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
    acc: u32,
    nbits: u32,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            pos: 0,
            acc: 0,
            nbits: 0,
        }
    }

    fn next_bit(&mut self) -> u32 {
        if self.nbits == 0 {
            let byte = self.data.get(self.pos).copied().unwrap_or(0);
            self.pos += 1;
            if byte == MARKER {
                // FF 00 is a stuffed FF; any other follower is a marker (end of scan) — feed zeros.
                if self.data.get(self.pos) == Some(&0x00) {
                    self.pos += 1;
                } else {
                    self.pos -= 1; // leave the marker in place
                }
            }
            self.acc = u32::from(byte);
            self.nbits = 8;
        }
        self.nbits -= 1;
        (self.acc >> self.nbits) & 1
    }

    fn receive(&mut self, count: u8) -> u32 {
        let mut v = 0u32;
        for _ in 0..count {
            // + (not |): v<<1 has a clear low bit, so | would be an equivalent (unkillable) mutant.
            v = (v << 1) + self.next_bit();
        }
        v
    }

    /// Consumes an `RSTn` marker at a restart boundary: discards any pending padding bits (the
    /// entropy stream is byte-aligned before a restart marker) and requires the next two bytes to
    /// be `FF` + `expected` (T.81 §E.1.4/§H.2.2 — the modulo-8 sequence number must match).
    fn sync_restart(&mut self, expected: u8) -> Result<()> {
        self.nbits = 0; // byte-align: drop the pad bits of the current byte
        if self.data.get(self.pos) == Some(&MARKER)
            && self.data.get(self.pos + 1) == Some(&expected)
        {
            self.pos += 2;
            Ok(())
        } else {
            Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "DNG: lossless JPEG missing or out-of-sequence restart marker",
            )
            .with_byte_offset(self.pos as u64))
        }
    }
}

/// Decodes one Huffman symbol (the magnitude category) using a canonical table.
fn decode_symbol(reader: &mut BitReader, codes: &[(u16, u8)]) -> Result<u8> {
    let mut code: u16 = 0;
    for len in 1..=16u8 {
        // + (not |): code<<1 has a clear low bit, so | would be an equivalent (unkillable) mutant.
        code = (code << 1) + reader.next_bit() as u16;
        for (sym, &(c, l)) in codes.iter().enumerate() {
            if l == len && c == code {
                return Ok(sym as u8);
            }
        }
    }
    Err(Error::invalid_input(
        env!("CARGO_PKG_NAME"),
        "DNG: invalid lossless-JPEG Huffman code",
    )
    .with_byte_offset(reader.pos as u64))
}

/// Reconstructs a difference from its category `ssss` and the mantissa bits read from `reader`.
fn extend(reader: &mut BitReader, ssss: u8) -> i32 {
    if ssss == 0 {
        return 0;
    }
    if ssss == 16 {
        return -32768;
    }
    let t = reader.receive(ssss) as i32;
    if t < (1 << (ssss - 1)) {
        t - (1 << ssss) + 1
    } else {
        t
    }
}

/// Reads a big-endian `u16` at `pos`.
fn be16(data: &[u8], pos: usize) -> Result<usize> {
    let b = pos
        .checked_add(2)
        .and_then(|end| data.get(pos..end))
        .ok_or_else(|| {
            Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "DNG: truncated lossless-JPEG marker",
            )
            .with_byte_offset(pos as u64)
        })?;
    Ok(usize::from(u16::from_be_bytes([b[0], b[1]])))
}

/// The SOF3 frame header.
struct Frame {
    precision: u16,
    width: usize,
    height: usize,
    /// Component identifiers, in frame order.
    component_ids: Vec<u8>,
}

/// The SOS scan header, resolved against the frame.
struct Scan {
    /// The predictor selector `Ss` (`1..=7`).
    predictor: u8,
    /// The point transform `Al`/`Pt`.
    point_transform: u16,
    /// Per component (in frame order): the Huffman table destination `Td`.
    table_ids: Vec<usize>,
}

/// Parses a SOF3 segment starting at its length field.
fn parse_sof3(data: &[u8], pos: usize) -> Result<Frame> {
    let precision = u16::from(
        *data
            .get(pos + 2)
            .ok_or_else(|| Error::invalid_input(env!("CARGO_PKG_NAME"), "DNG: truncated SOF3"))?,
    );
    if !(2..=16).contains(&precision) {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "DNG: lossless-JPEG precision must be 2..=16",
        ));
    }
    let height = be16(data, pos + 3)?;
    let width = be16(data, pos + 5)?;
    if height == 0 {
        // A zero height defers the line count to a DNL marker after the scan (§B.2.5).
        return Err(Error::unsupported(
            env!("CARGO_PKG_NAME"),
            "DNG: lossless JPEG with DNL-deferred height is not supported",
        ));
    }
    let count = usize::from(
        *data
            .get(pos + 7)
            .ok_or_else(|| Error::invalid_input(env!("CARGO_PKG_NAME"), "DNG: truncated SOF3"))?,
    );
    if !(1..=4).contains(&count) {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "DNG: lossless JPEG carries 1..=4 components",
        ));
    }
    let mut component_ids = Vec::with_capacity(count);
    for c in 0..count {
        let spec = data.get(pos + 8 + 3 * c..pos + 11 + 3 * c).ok_or_else(|| {
            Error::invalid_input(env!("CARGO_PKG_NAME"), "DNG: truncated SOF3 components")
        })?;
        if spec[1] != 0x11 {
            return Err(Error::unsupported(
                env!("CARGO_PKG_NAME"),
                "DNG: subsampled lossless-JPEG components are not supported",
            ));
        }
        component_ids.push(spec[0]);
    }
    Ok(Frame {
        precision,
        width,
        height,
        component_ids,
    })
}

/// Parses a DHT segment (possibly holding several tables) into the four destinations.
fn parse_dht(
    data: &[u8],
    pos: usize,
    len: usize,
    tables: &mut [Option<Vec<(u16, u8)>>; 4],
) -> Result<()> {
    let end = pos
        .checked_add(len)
        .filter(|e| *e <= data.len())
        .ok_or_else(|| Error::invalid_input(env!("CARGO_PKG_NAME"), "DNG: truncated DHT"))?;
    let mut at = pos + 2;
    while at < end {
        let tc_th = data[at];
        let (class, dest) = (tc_th >> 4, usize::from(tc_th & 0x0F));
        if class != 0 {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "DNG: lossless JPEG uses DC-class (Tc=0) Huffman tables",
            ));
        }
        if dest > 3 {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "DNG: Huffman table destination must be 0..=3",
            ));
        }
        let bits: [u8; 16] = data
            .get(at + 1..at + 17)
            .ok_or_else(|| Error::invalid_input(env!("CARGO_PKG_NAME"), "DNG: truncated DHT"))?
            .try_into()
            .map_err(|_| Error::invalid_input(env!("CARGO_PKG_NAME"), "DNG: truncated DHT"))?;
        let nvals: usize = bits.iter().map(|&b| usize::from(b)).sum();
        let huffval = data.get(at + 17..at + 17 + nvals).ok_or_else(|| {
            Error::invalid_input(env!("CARGO_PKG_NAME"), "DNG: truncated DHT values")
        })?;
        if huffval.iter().any(|&v| v > 16) {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "DNG: lossless-JPEG Huffman symbols are magnitude categories 0..=16",
            ));
        }
        tables[dest] = Some(canonical_codes(&bits, huffval));
        at += 17 + nvals;
    }
    if at != end {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "DNG: DHT length mismatch",
        ));
    }
    Ok(())
}

/// Parses the SOS header, resolving each scan component against the frame.
fn parse_sos(data: &[u8], pos: usize, frame: &Frame) -> Result<Scan> {
    let ns = usize::from(
        *data
            .get(pos + 2)
            .ok_or_else(|| Error::invalid_input(env!("CARGO_PKG_NAME"), "DNG: truncated SOS"))?,
    );
    if ns != frame.component_ids.len() {
        return Err(Error::unsupported(
            env!("CARGO_PKG_NAME"),
            "DNG: only single-scan (fully interleaved) lossless JPEG is supported",
        ));
    }
    let mut table_ids = Vec::with_capacity(ns);
    for c in 0..ns {
        let spec = data.get(pos + 3 + 2 * c..pos + 5 + 2 * c).ok_or_else(|| {
            Error::invalid_input(env!("CARGO_PKG_NAME"), "DNG: truncated SOS components")
        })?;
        if spec[0] != frame.component_ids[c] {
            return Err(Error::unsupported(
                env!("CARGO_PKG_NAME"),
                "DNG: lossless-JPEG scan components must follow the frame order",
            ));
        }
        table_ids.push(usize::from(spec[1] >> 4)); // Td; Ta is unused in lossless
    }
    let tail = data
        .get(pos + 3 + 2 * ns..pos + 6 + 2 * ns)
        .ok_or_else(|| Error::invalid_input(env!("CARGO_PKG_NAME"), "DNG: truncated SOS"))?;
    let (ss, se, ah, al) = (
        tail[0],
        tail[1],
        u16::from(tail[2] >> 4),
        u16::from(tail[2] & 0x0F),
    );
    if !(1..=7).contains(&ss) {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "DNG: lossless-JPEG predictor (Ss) must be 1..=7",
        ));
    }
    if se != 0 || ah != 0 {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "DNG: lossless-JPEG scan header must have Se = 0 and Ah = 0",
        ));
    }
    if al >= frame.precision {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "DNG: lossless-JPEG point transform must be below the precision",
        ));
    }
    Ok(Scan {
        predictor: ss,
        point_transform: al,
        table_ids,
    })
}

/// Decodes a lossless JPEG, returning its geometry and interleaved samples.
///
/// Accepts the full T.81 process-14 reader envelope: predictors 1–7, the point transform,
/// per-component Huffman tables, and restart intervals. See the module docs for what is
/// rejected as [`Error::Unsupported`].
///
/// # Errors
///
/// Returns [`Error::InvalidInput`] if the markers or entropy stream are malformed, or
/// [`Error::Unsupported`] for conformant-but-unsupported shapes (multi-scan, subsampling,
/// DNL-deferred height).
pub fn decode(data: &[u8]) -> Result<LosslessJpeg> {
    if data.len() < 2 || data[0] != MARKER || data[1] != SOI {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "DNG: not a JPEG (missing SOI)",
        ));
    }
    let mut pos = 2;
    let mut frame: Option<Frame> = None;
    let mut tables: [Option<Vec<(u16, u8)>>; 4] = [None, None, None, None];
    let mut restart_interval = 0usize;
    let scan;

    loop {
        // Find the next marker: skip to the first `FF` at or after `pos`. `position` returns an
        // offset strictly within `data[pos..]`, so there is no off-by-one bound to mutate.
        let off = data[pos..]
            .iter()
            .position(|&b| b == MARKER)
            .ok_or_else(|| {
                Error::invalid_input(env!("CARGO_PKG_NAME"), "DNG: lossless JPEG missing SOS")
            })?;
        pos += off;
        while data.get(pos) == Some(&MARKER) {
            pos += 1;
        }
        let marker = *data.get(pos).ok_or_else(|| {
            Error::invalid_input(env!("CARGO_PKG_NAME"), "DNG: truncated lossless JPEG")
        })?;
        pos += 1;
        match marker {
            SOF3 => {
                if frame.is_some() {
                    return Err(Error::invalid_input(
                        env!("CARGO_PKG_NAME"),
                        "DNG: duplicate SOF3 frame header",
                    ));
                }
                let len = be16(data, pos)?;
                frame = Some(parse_sof3(data, pos)?);
                pos += len;
            }
            // Any other frame type (baseline/extended/progressive DCT, other lossless variants,
            // arithmetic coding) is not the DNG lossless process.
            0xC0..=0xC2 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF | 0xCC => {
                return Err(Error::unsupported(
                    env!("CARGO_PKG_NAME"),
                    "DNG: only the SOF3 (lossless Huffman) JPEG process is supported",
                ));
            }
            DHT => {
                let len = be16(data, pos)?;
                parse_dht(data, pos, len, &mut tables)?;
                pos += len;
            }
            DRI => {
                let len = be16(data, pos)?;
                if len != 4 {
                    return Err(Error::invalid_input(
                        env!("CARGO_PKG_NAME"),
                        "DNG: DRI segment must be 4 bytes",
                    ));
                }
                restart_interval = be16(data, pos + 2)?;
                pos += len;
            }
            SOS => {
                let f = frame.as_ref().ok_or_else(|| {
                    Error::invalid_input(env!("CARGO_PKG_NAME"), "DNG: SOS before SOF3")
                })?;
                let len = be16(data, pos)?;
                scan = parse_sos(data, pos, f)?;
                pos += len; // entropy data follows
                break;
            }
            EOI => {
                return Err(Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "DNG: lossless JPEG ended before SOS",
                ));
            }
            _ => {
                // Skip any other marker segment by its length.
                let len = be16(data, pos)?;
                pos += len;
            }
        }
    }

    let frame = frame.ok_or_else(|| {
        Error::invalid_input(env!("CARGO_PKG_NAME"), "DNG: lossless JPEG missing SOF3")
    })?;
    let (width, height, components) = (frame.width, frame.height, frame.component_ids.len());
    if width == 0 {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "DNG: lossless JPEG has zero dimensions",
        ));
    }
    // One resolved table reference per component (missing destinations are an error up front).
    let mut codes: Vec<&[(u16, u8)]> = Vec::with_capacity(components);
    for &id in &scan.table_ids {
        codes.push(tables[id].as_deref().ok_or_else(|| {
            Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "DNG: scan references an undefined Huffman table",
            )
        })?);
    }

    let count = width
        .checked_mul(height)
        .and_then(|n| n.checked_mul(components))
        .ok_or_else(|| {
            Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "DNG: lossless JPEG dimensions overflow",
            )
        })?;
    let mut samples = vec![0u16; count];
    let mut reader = BitReader::new(&data[pos..]);
    let pt = scan.point_transform;
    // Restart-interval state: `origin` is where the current interval began (start of scan, or
    // the MCU after the latest RSTn); `mcus` counts MCUs within the interval; `rst` cycles 0..8.
    let mut origin = (0usize, 0usize);
    let mut mcus = 0usize;
    let mut rst = 0u8;
    for y in 0..height {
        for x in 0..width {
            if restart_interval > 0 && mcus == restart_interval {
                reader.sync_restart(RST0 + rst)?;
                rst = (rst + 1) % 8;
                mcus = 0;
                origin = (y, x);
            }
            for (c, code) in codes.iter().enumerate() {
                let ssss = decode_symbol(&mut reader, code)?;
                if ssss > 16 {
                    return Err(Error::invalid_input(
                        env!("CARGO_PKG_NAME"),
                        "DNG: lossless-JPEG category out of range",
                    ));
                }
                let diff = extend(&mut reader, ssss);
                let px = predict(
                    &samples,
                    width,
                    components,
                    x,
                    y,
                    c,
                    frame.precision,
                    pt,
                    scan.predictor,
                    origin,
                );
                samples[(y * width + x) * components + c] = (px + diff) as u16;
            }
            mcus += 1;
        }
    }
    // Undo the point transform: reconstruction ran in the downshifted domain (§H.2.3).
    if pt > 0 {
        for sample in &mut samples {
            *sample <<= pt;
        }
    }
    Ok(LosslessJpeg {
        width,
        height,
        components,
        precision: frame.precision,
        samples,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips_varied_shapes_and_depths() {
        for &precision in &[8u16, 12, 14, 16] {
            roundtrip(17, 9, 1, precision); // CFA-like single component, odd dims
            roundtrip(8, 8, 3, precision); // linear RGB
            roundtrip(1, 1, 1, precision); // smallest
        }
    }

    fn roundtrip(width: usize, height: usize, components: usize, precision: u16) {
        let max = (1u32 << precision) - 1;
        let samples: Vec<u16> = (0..width * height * components)
            .map(|i| ((i as u32).wrapping_mul(2654435761) % (max + 1)) as u16)
            .collect();
        let encoded = encode(&samples, width, height, components, precision).expect("encode");
        let decoded = decode(&encoded).expect("decode");
        assert_eq!(
            (
                decoded.width,
                decoded.height,
                decoded.components,
                decoded.precision
            ),
            (width, height, components, precision)
        );
        assert_eq!(
            decoded.samples, samples,
            "{width}x{height}x{components} @ {precision}-bit"
        );
    }

    /// Round-trips an explicit sample buffer (so we can hand-pick values that exercise the
    /// Huffman category math: zero, small/large positive and negative DC differences, and the
    /// full 16-bit extremes that drive `reduce`/`magnitude`/`extend`).
    fn roundtrip_samples(samples: &[u16], width: usize, height: usize, components: usize) {
        let encoded = encode(samples, width, height, components, 16).expect("encode");
        let decoded = decode(&encoded).expect("decode");
        assert_eq!(decoded.width, width);
        assert_eq!(decoded.height, height);
        assert_eq!(decoded.components, components);
        assert_eq!(decoded.samples, samples);
    }

    #[test]
    fn roundtrips_extreme_dc_differences() {
        // A single row whose neighbour deltas hit the category boundaries (incl. wrap-around so
        // `reduce` produces -32768 / 32767) and both signs of every magnitude.
        let row: Vec<u16> = vec![
            0, 65535, 0, 1, 0, 32768, 32767, 0, 2, 4, 8, 16, 256, 65280, 65535, 0,
        ];
        let len = row.len();
        roundtrip_samples(&row, len, 1, 1);
        // Same values down a single column (exercises the first-column "predict from above" path).
        roundtrip_samples(&row, 1, len, 1);
        // Two interleaved components with offset values (exercises per-component prediction).
        let mut two = Vec::new();
        for &v in &row {
            two.push(v);
            two.push(v ^ 0x8000);
        }
        roundtrip_samples(&two, len, 1, 2);
    }

    #[test]
    fn encode_validates_inputs() {
        assert!(encode(&[0], 1, 1, 1, 1).is_err()); // precision below 2
        assert!(encode(&[0], 1, 1, 1, 17).is_err()); // precision above 16
        assert!(encode(&[0; 5], 1, 1, 5, 16).is_err()); // five components
        assert!(encode(&[], 0, 1, 1, 16).is_err()); // zero width
        assert!(encode(&[0; 3], 2, 1, 1, 16).is_err()); // count mismatch
        assert!(encode(&[0; 4], 2, 2, 1, 16).is_ok());
    }

    #[test]
    fn reduce_wraps_at_exact_boundaries() {
        // Inside the canonical range: unchanged (kills `< -32768` -> `<= -32768`,
        // `> 32767` -> `>= 32767`).
        assert_eq!(reduce(-32768), -32768);
        assert_eq!(reduce(32767), 32767);
        // Just outside: wraps by exactly 2^16 (kills `> 32767` -> `==`, and the subtraction
        // operator mutants `-` -> `+` / `/`).
        assert_eq!(reduce(32768), -32768);
        assert_eq!(reduce(-32769), 32767);
        assert_eq!(reduce(40000), 40000 - 65536);
        assert_eq!(reduce(-40000), -40000 + 65536);
    }

    #[test]
    fn magnitude_categories_are_exact() {
        assert_eq!(magnitude(0), (0, 0));
        // The T.81 special case: -32768 is category 16 with no mantissa. Deleting the unary `-`
        // would make this `diff == 32768` (never reached) and fall through to (16, 0x7fff).
        assert_eq!(magnitude(-32768), (16, 0));
        // Positive: mantissa == diff; negative: one's-complement low bits (diff - 1).
        assert_eq!(magnitude(1), (1, 1));
        assert_eq!(magnitude(-1), (1, 0));
        assert_eq!(magnitude(2), (2, 0b10));
        assert_eq!(magnitude(-2), (2, 0b01));
        assert_eq!(magnitude(32767), (15, 0x7fff));
    }

    #[test]
    fn extend_reconstructs_category_16_as_min() {
        // ssss == 16 reads no mantissa and must yield -32768 (kills the deleted unary `-`, which
        // would return 32768; observable only here since the decoder folds both mod 2^16).
        let mut reader = BitReader::new(&[]);
        assert_eq!(extend(&mut reader, 16), -32768);
        // ssss == 0 yields 0 with no bits consumed.
        let mut reader = BitReader::new(&[]);
        assert_eq!(extend(&mut reader, 0), 0);
    }

    #[test]
    fn bit_writer_pads_only_a_partial_final_byte() {
        // Exactly one full byte: finish must NOT append a padding byte (kills `nbits > 0`
        // -> `nbits >= 0`, which would `put(0xff, 8)` and grow the output).
        let mut w = BitWriter::new();
        w.put(0b1010_1010, 8);
        assert_eq!(w.finish(), vec![0b1010_1010]);
        // A partial byte (3 bits) is padded with 1-bits up to one byte.
        let mut w = BitWriter::new();
        w.put(0b101, 3);
        assert_eq!(w.finish(), vec![0b1011_1111]);
        // Empty writer produces no bytes.
        assert_eq!(BitWriter::new().finish(), Vec::<u8>::new());
    }

    #[test]
    fn bit_reader_holds_position_at_a_real_marker() {
        // `FF` followed by a non-`00` byte is a real marker (end of scan): the reader must leave
        // it in place and feed `1`-bits forever (kills `pos -= 1` -> `+= 1` / `/= 1`, which would
        // advance past the marker and start reading the following bytes).
        let mut reader = BitReader::new(&[0xFF, 0xD9]);
        for _ in 0..24 {
            assert_eq!(reader.next_bit(), 1);
        }
        // A stuffed `FF 00` decodes to the data bits of `FF` then continues to the next byte.
        let mut reader = BitReader::new(&[0xFF, 0x00, 0x00]);
        for _ in 0..8 {
            assert_eq!(reader.next_bit(), 1); // the FF
        }
        for _ in 0..8 {
            assert_eq!(reader.next_bit(), 0); // the following 0x00
        }
    }

    /// Builds a valid 1x1x1 stream, then lets the caller mutate it.
    fn valid_stream() -> Vec<u8> {
        encode(&[12345u16], 1, 1, 1, 16).expect("encode")
    }

    #[test]
    fn encode_dimension_boundaries_are_exact() {
        for (width, height) in [(0, 1), (1, 0), (65536, 1), (1, 65536)] {
            let error = encode(&[], width, height, 1, 16).expect_err("invalid dimensions");
            assert!(
                error
                    .static_message()
                    .is_some_and(|message| message.contains("dimensions must be 1..=65535")),
                "unexpected error for {width}x{height}: {error:?}"
            );
        }

        let samples = vec![0u16; 65535];
        assert!(encode(&samples, 65535, 1, 1, 16).is_ok());
        assert!(encode(&samples, 1, 65535, 1, 16).is_ok());
    }

    #[test]
    fn four_component_header_ranges_are_exact() {
        let mut sof3 = vec![0, 20, 16, 0, 1, 0, 1, 4];
        for id in 1..=4 {
            sof3.extend_from_slice(&[id, 0x11, 0]);
        }
        let frame = parse_sof3(&sof3, 0).expect("four-component SOF3");
        assert_eq!(frame.component_ids, vec![1, 2, 3, 4]);

        let sos = [0, 14, 4, 1, 0, 2, 0, 3, 0, 4, 0, 1, 0, 0];
        let scan = parse_sos(&sos, 0, &frame).expect("four-component SOS");
        assert_eq!(scan.table_ids, vec![0, 0, 0, 0]);
    }

    #[test]
    fn dht_destination_boundary_is_exact() {
        let mut segment = vec![0, 19, 3];
        segment.extend_from_slice(&[0; 16]);
        let mut tables: [Option<Vec<(u16, u8)>>; 4] = std::array::from_fn(|_| None);
        parse_dht(&segment, 0, segment.len(), &mut tables).expect("destination 3");
        assert!(tables[3].is_some());

        segment[2] = 4;
        let error = parse_dht(&segment, 0, segment.len(), &mut tables)
            .expect_err("destination 4 must be rejected");
        assert!(
            error
                .static_message()
                .is_some_and(|message| message.contains("destination must be 0..=3"))
        );
    }

    #[test]
    fn decode_rejects_bad_header() {
        // Too short / no SOI.
        assert!(decode(&[]).is_err());
        assert!(decode(&[MARKER, 0x00]).is_err());
        // `< 2` -> `==`: a 1-byte `FF` must error, not index `data[1]` out of bounds.
        assert!(decode(&[MARKER]).is_err());
        // `< 2` -> `<=`: a bare SOI (len 2) must reach the marker loop and fail with "missing SOS",
        // not the header's "not a JPEG" message.
        match decode(&[MARKER, SOI]) {
            Err(error) if error.kind() == gamut_core::ErrorKind::InvalidInput => {
                let m = error.static_message().unwrap_or_default();
                assert!(m.contains("missing SOS"), "got {m:?}")
            }
            Err(other) => panic!("expected missing-SOS error, got {other:?}"),
            Ok(_) => panic!("expected missing-SOS error, got Ok"),
        }
        // Corrupting only the SOI's `FF` (byte 0) must fail: the `||` chain has to OR the
        // marker checks, not AND them (kills both `||` -> `&&` in the header guard).
        let mut s = valid_stream();
        s[0] = 0x00;
        assert!(decode(&s).is_err());
    }

    #[test]
    fn decode_skips_stray_bytes_and_unknown_segments() {
        // A stray non-marker byte right after SOI must be scanned over to find SOF3 (kills
        // `!= MARKER` -> `==`, and the scan filter `< data.len()` -> `==`/`>`).
        let s = valid_stream();
        let mut spliced = s[..2].to_vec();
        spliced.push(0xAA);
        spliced.extend_from_slice(&s[2..]);
        let decoded = decode(&spliced).expect("decode past stray byte");
        assert_eq!(decoded.samples, vec![12345u16]);

        // An unknown marker segment (APP0-like) must be skipped by its length (kills the `_`-arm
        // `pos += len` -> `-=` / `*=`).
        let mut spliced = s[..2].to_vec();
        spliced.extend_from_slice(&[MARKER, 0xE0, 0x00, 0x04, 0x00, 0x00]); // FF E0, len=4, 2 pad
        spliced.extend_from_slice(&s[2..]);
        let decoded = decode(&spliced).expect("decode past unknown segment");
        assert_eq!(decoded.samples, vec![12345u16]);
    }

    #[test]
    fn decode_rejects_eoi_before_sos() {
        // An EOI marker encountered before SOS must error (kills `delete match arm EOI`, which
        // would treat EOI as a skippable segment and then decode the spliced-in real markers).
        let s = valid_stream();
        let mut spliced = s[..2].to_vec();
        spliced.extend_from_slice(&[MARKER, EOI, 0x00, 0x04, 0x00, 0x00]); // FF D9, len=4, 2 pad
        spliced.extend_from_slice(&s[2..]);
        assert!(decode(&spliced).is_err());
    }

    #[test]
    fn decode_rejects_zero_dimensions_and_dnl() {
        // Patch the SOF3 width field (bytes 9..11 of a 1x1 stream) to zero. The zero width is
        // rejected after header parsing.
        let mut s = valid_stream();
        s[9] = 0;
        s[10] = 0;
        assert!(decode(&s).is_err());
        // A zero height is a DNL-deferred line count — Unsupported, not silently mis-decoded.
        let mut s = valid_stream();
        s[7] = 0;
        s[8] = 0;
        assert!(matches!(
            decode(&s),
            Err(error) if error.kind() == gamut_core::ErrorKind::Unsupported
        ));
    }

    // ------------------------------------------------------------------------------------------
    // A configurable stream builder covering the decode envelope the fixed `encode` cannot emit:
    // arbitrary predictors, a point transform, restart intervals, and per-component tables.
    // Prediction reuses the module's `predict`, so builder/decoder agreement alone cannot prove
    // the boundary rules — the Adobe-SDK differential suite (tests/adobe_oracle.rs) and the
    // hand-computed fixture below break that symmetry.
    // ------------------------------------------------------------------------------------------

    /// A second, distinct Huffman assignment (the categories in reverse), so selecting the wrong
    /// table destination misdecodes instead of accidentally agreeing.
    const HUFFVAL_REV: [u8; 17] = [16, 15, 14, 13, 12, 11, 10, 9, 8, 7, 6, 5, 4, 3, 2, 1, 0];

    /// Test-only bit writer with byte-stuffing plus restart-marker support.
    struct TestBits {
        out: Vec<u8>,
        acc: u32,
        nbits: u32,
    }

    impl TestBits {
        fn new() -> Self {
            Self {
                out: Vec::new(),
                acc: 0,
                nbits: 0,
            }
        }

        fn put(&mut self, value: u32, count: u8) {
            for i in (0..count).rev() {
                self.acc = (self.acc << 1) | ((value >> i) & 1);
                self.nbits += 1;
                if self.nbits == 8 {
                    let byte = self.acc as u8;
                    self.out.push(byte);
                    if byte == MARKER {
                        self.out.push(0x00);
                    }
                    self.acc = 0;
                    self.nbits = 0;
                }
            }
        }

        /// Pads to a byte boundary with 1-bits and emits a raw (unstuffed) restart marker.
        fn restart(&mut self, m: u8) {
            self.align();
            self.out.extend_from_slice(&[MARKER, RST0 + m]);
        }

        fn align(&mut self) {
            while self.nbits != 0 {
                self.put(1, 1);
            }
        }

        fn finish(mut self) -> Vec<u8> {
            self.align();
            self.out
        }
    }

    /// Builds a complete SOF3 stream with the given scan shape. `tables` lists the DHT
    /// destinations to define (all in one segment), each as `(dest, huffval)` over the shared
    /// `BITS` lengths; `table_ids` gives each component's `Td`.
    struct StreamBuilder {
        precision: u16,
        predictor: u8,
        pt: u16,
        restart_interval: usize,
        tables: Vec<(u8, Vec<u8>)>,
        table_ids: Vec<u8>,
    }

    impl StreamBuilder {
        fn simple(precision: u16, predictor: u8, components: usize) -> Self {
            Self {
                precision,
                predictor,
                pt: 0,
                restart_interval: 0,
                tables: vec![(0, HUFFVAL.to_vec())],
                table_ids: vec![0; components],
            }
        }

        fn build(&self, samples: &[u16], width: usize, height: usize) -> Vec<u8> {
            let components = self.table_ids.len();
            assert_eq!(samples.len(), width * height * components);
            let mut out = vec![MARKER, SOI];

            out.extend_from_slice(&[MARKER, SOF3]);
            out.extend_from_slice(&((8 + 3 * components) as u16).to_be_bytes());
            out.push(self.precision as u8);
            out.extend_from_slice(&(height as u16).to_be_bytes());
            out.extend_from_slice(&(width as u16).to_be_bytes());
            out.push(components as u8);
            for c in 0..components {
                out.extend_from_slice(&[(c + 1) as u8, 0x11, 0x00]);
            }

            // One DHT segment holding every destination back-to-back.
            out.extend_from_slice(&[MARKER, DHT]);
            let dht_len = 2 + self
                .tables
                .iter()
                .map(|(_, hv)| 17 + hv.len())
                .sum::<usize>();
            out.extend_from_slice(&(dht_len as u16).to_be_bytes());
            for (dest, huffval) in &self.tables {
                out.push(*dest); // Tc=0 | Th
                out.extend_from_slice(&BITS);
                out.extend_from_slice(huffval);
            }

            if self.restart_interval > 0 {
                out.extend_from_slice(&[MARKER, DRI, 0x00, 0x04]);
                out.extend_from_slice(&(self.restart_interval as u16).to_be_bytes());
            }

            out.extend_from_slice(&[MARKER, SOS]);
            out.extend_from_slice(&((6 + 2 * components) as u16).to_be_bytes());
            out.push(components as u8);
            for (c, td) in self.table_ids.iter().enumerate() {
                out.extend_from_slice(&[(c + 1) as u8, td << 4]);
            }
            out.push(self.predictor);
            out.push(0); // Se
            out.push(self.pt as u8); // Ah=0 | Al

            // Entropy-code the point-transformed samples with the module's own prediction.
            let shifted: Vec<u16> = samples.iter().map(|&s| s >> self.pt).collect();
            let codes: Vec<Vec<(u16, u8)>> = self
                .table_ids
                .iter()
                .map(|td| {
                    let huffval = &self.tables.iter().find(|(d, _)| d == td).expect("table").1;
                    canonical_codes(&BITS, huffval)
                })
                .collect();
            let mut bits = TestBits::new();
            let mut origin = (0usize, 0usize);
            let mut mcus = 0usize;
            let mut rst = 0u8;
            for y in 0..height {
                for x in 0..width {
                    if self.restart_interval > 0 && mcus == self.restart_interval {
                        bits.restart(rst);
                        rst = (rst + 1) % 8;
                        mcus = 0;
                        origin = (y, x);
                    }
                    for c in 0..components {
                        let actual = i32::from(shifted[(y * width + x) * components + c]);
                        let pred = predict(
                            &shifted,
                            width,
                            components,
                            x,
                            y,
                            c,
                            self.precision,
                            self.pt,
                            self.predictor,
                            origin,
                        );
                        let diff = reduce(actual - pred);
                        let (ssss, mantissa) = magnitude(diff);
                        let (code, len) = codes[c][ssss as usize];
                        bits.put(u32::from(code), len);
                        if ssss != 0 && ssss < 16 {
                            bits.put(mantissa, ssss);
                        }
                    }
                    mcus += 1;
                }
            }
            out.extend_from_slice(&bits.finish());
            out.extend_from_slice(&[MARKER, EOI]);
            out
        }
    }

    fn test_samples(width: usize, height: usize, components: usize, precision: u16) -> Vec<u16> {
        let max = (1u32 << precision) - 1;
        (0..width * height * components)
            .map(|i| ((i as u32).wrapping_mul(40503).wrapping_add(17) % (max + 1)) as u16)
            .collect()
    }

    #[test]
    fn decodes_every_predictor_at_varied_precisions() {
        for predictor in 1..=7u8 {
            for &precision in &[2u16, 8, 12, 16] {
                let samples = test_samples(9, 7, 1, precision);
                let stream = StreamBuilder::simple(precision, predictor, 1).build(&samples, 9, 7);
                let decoded = decode(&stream)
                    .unwrap_or_else(|e| panic!("predictor {predictor} @ {precision}-bit: {e:?}"));
                assert_eq!(
                    decoded.samples, samples,
                    "predictor {predictor} @ {precision}-bit"
                );
            }
        }
    }

    #[test]
    fn decodes_the_point_transform() {
        for &pt in &[1u16, 4] {
            let samples = test_samples(8, 5, 1, 12);
            let mut builder = StreamBuilder::simple(12, 4, 1);
            builder.pt = pt;
            let decoded = decode(&builder.build(&samples, 8, 5)).expect("decode");
            // The point transform is lossy: low bits are gone, values return upshifted.
            let expected: Vec<u16> = samples.iter().map(|&s| (s >> pt) << pt).collect();
            assert_eq!(decoded.samples, expected, "Pt = {pt}");
        }
    }

    #[test]
    fn decodes_per_component_tables_from_one_dht_segment() {
        // Component 0 uses destination 0 (identity order), component 1 uses destination 1
        // (reversed order): a decoder that ignores Td and reuses one table misdecodes.
        let samples = test_samples(6, 4, 2, 12);
        let mut builder = StreamBuilder::simple(12, 1, 2);
        builder.tables = vec![(0, HUFFVAL.to_vec()), (1, HUFFVAL_REV.to_vec())];
        builder.table_ids = vec![0, 1];
        let decoded = decode(&builder.build(&samples, 6, 4)).expect("decode");
        assert_eq!(decoded.samples, samples);
    }

    #[test]
    fn decodes_restart_intervals() {
        // Row-aligned (interval == width) and non-aligned (interval straddles rows).
        for interval in [6usize, 4] {
            let samples = test_samples(6, 5, 1, 12);
            let mut builder = StreamBuilder::simple(12, 4, 1);
            builder.restart_interval = interval;
            let decoded = decode(&builder.build(&samples, 6, 5)).expect("decode");
            assert_eq!(decoded.samples, samples, "interval {interval}");
        }
        // More than 8 intervals: the RSTn sequence number must wrap modulo 8.
        let samples = test_samples(4, 12, 1, 8);
        let mut builder = StreamBuilder::simple(8, 2, 1);
        builder.restart_interval = 4;
        let decoded = decode(&builder.build(&samples, 4, 12)).expect("decode");
        assert_eq!(decoded.samples, samples);
    }

    /// A fully hand-computed stream (no shared helpers): 3x1, 8-bit, predictor 1, restart
    /// interval 2. Verifies the decoder against T.81 by hand — the first sample predicts
    /// `2^(P-1) = 128`, the second predicts from the left, and the sample after the restart
    /// marker predicts the default again (the reset), not its left neighbour.
    #[test]
    fn hand_computed_restart_stream_decodes_exactly() {
        #[rustfmt::skip]
        let stream = [
            0xFF, 0xD8, // SOI
            0xFF, 0xC3, 0x00, 0x0B, 8, 0x00, 0x01, 0x00, 0x03, 1, 0x01, 0x11, 0x00, // SOF3 3x1
            0xFF, 0xC4, 0x00, 0x24, 0x00, // DHT, Tc/Th = 0
            0, 0, 0, 15, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, // BITS
            0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, // HUFFVAL
            0xFF, 0xDD, 0x00, 0x04, 0x00, 0x02, // DRI, Ri = 2
            0xFF, 0xDA, 0x00, 0x08, 1, 0x01, 0x00, 1, 0, 0x00, // SOS, Ss=1, Al=0
            // MCU 0: 100 - 128 = -28 -> ssss 5 (code 0101), mantissa (-29)&31 = 00011.
            // MCU 1: 101 - 100 = 1 -> ssss 1 (code 0001), mantissa 1. 14 bits + '11' padding:
            0b0101_0001, 0b1000_1111,
            0xFF, 0xD0, // RST0
            // MCU 2 (after reset): 50 - 128 = -78 -> ssss 7 (code 0111),
            // mantissa (-79)&127 = 0110001. 11 bits + '11111' padding:
            0b0111_0110, 0b0011_1111,
            0xFF, 0xD9, // EOI
        ];
        let decoded = decode(&stream).expect("decode");
        assert_eq!(
            (decoded.width, decoded.height, decoded.components),
            (3, 1, 1)
        );
        assert_eq!(decoded.samples, vec![100, 101, 50]);
    }

    /// The restart marker's modulo-8 sequence number is verified: RST1 where RST0 is due must
    /// fail rather than resynchronize silently.
    #[test]
    fn out_of_sequence_restart_marker_is_rejected() {
        let samples = test_samples(6, 2, 1, 8);
        let mut builder = StreamBuilder::simple(8, 1, 1);
        builder.restart_interval = 6;
        let mut stream = builder.build(&samples, 6, 2);
        // The only restart marker in the stream is FF D0; corrupt it to FF D1.
        let at = stream
            .windows(2)
            .position(|w| w == [0xFF, 0xD0])
            .expect("restart marker present");
        stream[at + 1] = 0xD1;
        assert!(decode(&stream).is_err());
    }

    #[test]
    fn scan_header_fields_are_validated() {
        let samples = test_samples(4, 3, 1, 12);
        let base = StreamBuilder::simple(12, 1, 1).build(&samples, 4, 3);
        // The scan tail is [Ss, Se, AhAl] just before the entropy data. SOS layout for one
        // component: FF DA | len(2) | Ns | Cs Td | Ss Se AhAl -> Ss sits at sos + 7.
        let sos = base
            .windows(2)
            .position(|w| w == [0xFF, 0xDA])
            .expect("SOS");
        let (ss_at, se_at, ahal_at) = (sos + 7, sos + 8, sos + 9);
        for bad_ss in [0u8, 8] {
            let mut s = base.clone();
            s[ss_at] = bad_ss;
            assert!(decode(&s).is_err(), "Ss = {bad_ss} must be rejected");
        }
        let mut s = base.clone();
        s[se_at] = 1; // Se must be 0
        assert!(decode(&s).is_err());
        let mut s = base.clone();
        s[ahal_at] = 0x10; // Ah must be 0
        assert!(decode(&s).is_err());
        let mut s = base.clone();
        s[ahal_at] = 12; // Al >= precision
        assert!(decode(&s).is_err());
        // The unmodified stream still decodes (the probe offsets are right).
        assert!(decode(&base).is_ok());
    }

    #[test]
    fn unsupported_shapes_are_rejected_as_unsupported() {
        let samples = test_samples(4, 3, 2, 12);
        let base = StreamBuilder::simple(12, 1, 2).build(&samples, 4, 3);

        // Subsampled component: patch the first component's H/V byte (SOF3 header).
        let mut s = base.clone();
        let sof = s.windows(2).position(|w| w == [0xFF, 0xC3]).expect("SOF3");
        s[sof + 11] = 0x21;
        assert!(matches!(
            decode(&s),
            Err(error) if error.kind() == gamut_core::ErrorKind::Unsupported
        ));

        // Multi-scan: the scan lists fewer components than the frame (Ns=1 of 2). Rebuild the
        // SOS by hand: one component spec, fixed length.
        let mut s = base[..base
            .windows(2)
            .position(|w| w == [0xFF, 0xDA])
            .expect("SOS")]
            .to_vec();
        s.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x08, 1, 0x01, 0x00, 1, 0, 0x00]);
        assert!(matches!(
            decode(&s),
            Err(error) if error.kind() == gamut_core::ErrorKind::Unsupported
        ));

        // A non-SOF3 frame type.
        let mut s = base.clone();
        s[sof + 1] = 0xC0;
        assert!(matches!(
            decode(&s),
            Err(error) if error.kind() == gamut_core::ErrorKind::Unsupported
        ));

        // A duplicate SOF3 is malformed, not unsupported.
        let mut s = base.clone();
        let sof_seg = base[sof..sof + 2 + 8 + 3 * 2].to_vec();
        s.splice(sof..sof, sof_seg);
        assert!(matches!(
            decode(&s),
            Err(error) if error.kind() == gamut_core::ErrorKind::InvalidInput
        ));
    }

    #[test]
    fn missing_referenced_table_is_rejected() {
        // The scan selects destination 1. Build a valid stream defining destinations 0 and 1,
        // then re-home the second table to destination 2, leaving Td = 1 dangling (the stream
        // layout is otherwise untouched).
        let samples = test_samples(4, 3, 1, 12);
        let mut builder = StreamBuilder::simple(12, 1, 1);
        builder.table_ids = vec![1];
        builder.tables = vec![(0, HUFFVAL.to_vec()), (1, HUFFVAL.to_vec())];
        let with_table = builder.build(&samples, 4, 3);
        assert!(decode(&with_table).is_ok(), "control stream must decode");
        let mut without = with_table;
        let dht = without
            .windows(2)
            .position(|w| w == [0xFF, 0xC4])
            .expect("DHT");
        // Second table's Tc/Th byte: after the 4-byte segment head and the first table's
        // 1 + 16 + 17 bytes.
        let second_dest = dht + 4 + 1 + 16 + HUFFVAL.len();
        assert_eq!(without[second_dest], 0x01, "probe must hit the Tc/Th byte");
        without[second_dest] = 0x02;
        assert!(decode(&without).is_err());
    }
    /// Differential conformance: the Adobe DNG SDK's own lossless-JPEG codec
    /// (`DecodeLosslessJPEG<Scalar>`, via `gamut_dng_oracle::decode_lossless_jpeg`) must decode
    /// the same streams to the same samples. This breaks the builder/decoder symmetry of the
    /// unit tests above: the SDK derives its predictions independently, so a boundary-rule or
    /// predictor bug on our side cannot cancel out.
    mod sdk_differential {
        use super::super::*;
        use super::{HUFFVAL_REV, StreamBuilder, test_samples};

        fn assert_sdk_agrees(stream: &[u8], expected: &[u16], what: &str) {
            let ours = decode(stream).unwrap_or_else(|e| panic!("{what}: gamut decode: {e:?}"));
            assert_eq!(ours.samples, expected, "{what}: gamut decode");
            let theirs = gamut_dng_oracle::decode_lossless_jpeg(stream, expected.len())
                .unwrap_or_else(|e| panic!("{what}: SDK decode: {e}"));
            assert_eq!(ours.samples, theirs, "{what}: gamut vs SDK");
        }

        #[test]
        fn sdk_matches_our_encoder_output() {
            for &(w, h, c, precision) in &[
                (17usize, 9usize, 1usize, 12u16),
                (8, 8, 3, 16),
                (5, 4, 2, 8),
            ] {
                let samples = test_samples(w, h, c, precision);
                let stream = encode(&samples, w, h, c, precision).expect("encode");
                assert_sdk_agrees(&stream, &samples, "encoder output");
            }
        }

        /// The oracle's non-exporting entry point (`decode_lossless_jpeg_extent`) reaches the
        /// same decode as the exporting one: same stream in, same sample count out. The codec
        /// benchmark times the pair against each other to price the FFI export path, and that
        /// subtraction is only a price if the two decodes are otherwise identical.
        #[test]
        fn sdk_extent_entry_counts_the_same_samples_as_the_exporting_entry() {
            let samples = test_samples(8, 8, 3, 16);
            let stream = encode(&samples, 8, 8, 3, 16).expect("encode");
            let exported =
                gamut_dng_oracle::decode_lossless_jpeg(&stream, samples.len()).expect("SDK decode");
            let counted = gamut_dng_oracle::decode_lossless_jpeg_extent(&stream, samples.len())
                .expect("SDK extent decode");
            assert_eq!(counted, exported.len());
        }

        #[test]
        fn sdk_matches_every_predictor() {
            for predictor in 1..=7u8 {
                let samples = test_samples(9, 7, 1, 12);
                let stream = StreamBuilder::simple(12, predictor, 1).build(&samples, 9, 7);
                assert_sdk_agrees(&stream, &samples, &format!("predictor {predictor}"));
            }
        }

        #[test]
        fn sdk_matches_the_point_transform() {
            // The SDK's bare codec spools the *downshifted* values (its Pt upshift happens in
            // dng_read_image, above the codec); our decode returns them upshifted per §H.2.3.
            // So the comparison is on the downshifted domain: ours >> Pt == SDK.
            for &pt in &[1u16, 4] {
                let samples = test_samples(8, 5, 1, 12);
                let mut builder = StreamBuilder::simple(12, 4, 1);
                builder.pt = pt;
                let stream = builder.build(&samples, 8, 5);
                let ours = decode(&stream).expect("gamut decode");
                let expected: Vec<u16> = samples.iter().map(|&s| (s >> pt) << pt).collect();
                assert_eq!(ours.samples, expected, "Pt {pt}: gamut decode");
                let theirs = gamut_dng_oracle::decode_lossless_jpeg(&stream, expected.len())
                    .expect("SDK decode");
                let ours_down: Vec<u16> = ours.samples.iter().map(|&s| s >> pt).collect();
                assert_eq!(ours_down, theirs, "Pt {pt}: gamut (downshifted) vs SDK");
            }
        }

        #[test]
        fn sdk_matches_row_aligned_restarts() {
            // The SDK supports whole-row restart intervals only (restartInRows = Ri / width), so the
            // differential fixtures stay row-aligned; general intervals are covered by the internal
            // round-trip tests.
            for rows_per_interval in [1usize, 2] {
                let samples = test_samples(6, 6, 1, 12);
                let mut builder = StreamBuilder::simple(12, 5, 1);
                builder.restart_interval = 6 * rows_per_interval;
                let stream = builder.build(&samples, 6, 6);
                assert_sdk_agrees(
                    &stream,
                    &samples,
                    &format!("restart every {rows_per_interval} row(s)"),
                );
            }
        }

        #[test]
        fn sdk_matches_per_component_tables() {
            let samples = test_samples(6, 4, 2, 12);
            let mut builder = StreamBuilder::simple(12, 1, 2);
            builder.tables = vec![(0, HUFFVAL.to_vec()), (1, HUFFVAL_REV.to_vec())];
            builder.table_ids = vec![0, 1];
            let stream = builder.build(&samples, 6, 4);
            assert_sdk_agrees(&stream, &samples, "per-component tables");
        }

        /// End to end: a DNG whose raw strip is a *predictor-4* SOF3 stream — a shape gamut's
        /// own encoder never writes — decodes to identical samples through gamut's DNG decoder
        /// and the Adobe SDK's negative reader.
        #[test]
        fn predictor4_strip_dng_decodes_identically_to_adobe() {
            use gamut_ifd::{ByteOrder, Ifd, TiffFile, Value, Variant, write};

            use crate::tags;

            let (w, h) = (8usize, 6usize);
            let samples = test_samples(w, h, 1, 12);
            let strip = StreamBuilder::simple(12, 4, 1).build(&samples, w, h);

            let mut ifd = Ifd::new();
            ifd.set(tags::NEW_SUBFILE_TYPE, Value::Long(vec![0]));
            ifd.set(tags::IMAGE_WIDTH, Value::Short(vec![w as u16]));
            ifd.set(tags::IMAGE_LENGTH, Value::Short(vec![h as u16]));
            ifd.set(tags::BITS_PER_SAMPLE, Value::Short(vec![12]));
            ifd.set(tags::COMPRESSION, Value::Short(vec![7])); // lossless JPEG
            ifd.set(tags::PHOTOMETRIC_INTERPRETATION, Value::Short(vec![32803]));
            ifd.set(tags::SAMPLES_PER_PIXEL, Value::Short(vec![1]));
            ifd.set(tags::ROWS_PER_STRIP, Value::Short(vec![h as u16]));
            ifd.set(tags::CFA_REPEAT_PATTERN_DIM, Value::Short(vec![2, 2]));
            ifd.set(tags::CFA_PATTERN, Value::Byte(vec![0, 1, 1, 2]));
            ifd.set(tags::CFA_PLANE_COLOR, Value::Byte(vec![0, 1, 2]));
            ifd.set(tags::DNG_VERSION, Value::Byte(vec![1, 4, 0, 0]));
            ifd.set(
                tags::UNIQUE_CAMERA_MODEL,
                Value::Ascii("gamut TestCam".to_owned()),
            );
            ifd.set(
                tags::COLOR_MATRIX1,
                Value::SRational(vec![
                    (1, 1),
                    (0, 1),
                    (0, 1),
                    (0, 1),
                    (1, 1),
                    (0, 1),
                    (0, 1),
                    (0, 1),
                    (1, 1),
                ]),
            );
            ifd.set(tags::CALIBRATION_ILLUMINANT1, Value::Short(vec![21])); // D65
            ifd.set(
                tags::AS_SHOT_NEUTRAL,
                Value::Rational(vec![(1, 1), (1, 1), (1, 1)]),
            );
            ifd.set(tags::STRIP_OFFSETS, Value::Long(vec![0]));
            ifd.set(
                tags::STRIP_BYTE_COUNTS,
                Value::Long(vec![strip.len() as u32]),
            );

            // Two-pass layout: size the container, then point the strip just past it.
            let single = |ifd: Ifd| TiffFile {
                order: ByteOrder::LittleEndian,
                variant: Variant::Classic,
                ifds: vec![ifd],
            };
            let mut dng = write(&single(ifd.clone())).expect("write");
            let strip_at = dng.len() as u32;
            let mut placed = ifd;
            placed.set(tags::STRIP_OFFSETS, Value::Long(vec![strip_at]));
            dng = write(&single(placed)).expect("rewrite");
            dng.extend_from_slice(&strip);

            let ours = crate::decoder::DngDecoder::new()
                .decode(&dng)
                .expect("gamut decode");
            assert_eq!(ours.raw.samples(), samples.as_slice());
            let adobe = gamut_dng_oracle::read_raw_dng(&dng).expect("Adobe decode");
            assert_eq!(adobe.samples, samples);
        }
    }
}
