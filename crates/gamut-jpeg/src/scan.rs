//! Entropy-scan decoding (T.81 Annex F §F.2 and Annex G §G.2): the byte-destuffing bit reader
//! (§F.2.2.5), the canonical `DECODE`/`RECEIVE`/`EXTEND` procedures (Figures F.16, F.17, F.12), the
//! sequential MCU walk that reconstructs each component's sample plane (§A.2), and the progressive
//! DCT decoder (§G.2) that accumulates DCT coefficients across scans. Restart processing follows
//! §E.2.5.
//!
//! Decoding is deliberately generous: every read is bounds-checked and every table lookup is
//! fallible, so malformed input yields a typed [`Error`] and never a panic. The decoder can also
//! supply an MCU-row ceiling for a DNL-deferred sequential frame, preventing an unbounded sample
//! plane from growing before the height marker arrives.
//!
//! # Progressive coefficient model (§G.1.1)
//!
//! A progressive (SOF2) frame is coded as many scans, each carrying one *band* of the zig-zag
//! sequence at one *successive-approximation* precision. The decoder therefore cannot reconstruct a
//! block from a single scan; instead each frame component owns a full `i32` DCT-coefficient buffer
//! ([`ProgComp`]) at block granularity that the scans fill incrementally. Only after **all** scans
//! (at EOI) is each component dequantized (§A.3.4), inverse-transformed (§A.3.3) and level-shifted
//! (§A.3.1) — once — through the same [`write_block`] tail the sequential path uses.

use std::num::NonZeroU16;

use gamut_core::{Error, Result};
use gamut_dsp::jpeg::idct8x8;

use crate::huffman::{DecTable, LOOKAHEAD};
use crate::syntax::{Frame, ScanHeader, Tables};
use crate::zigzag::ZIGZAG;

/// One decoded component's reconstructed sample plane, stored at block-padded resolution: `stride`
/// is `blocks_per_line · 8` and rows grow in units of a block as the scan proceeds. The valid image
/// region (`comp_w × comp_h`) is cropped from this during upsampling.
pub struct Plane {
    /// Reconstructed clamped samples, row-major, `stride` wide.
    pub data: Vec<u8>,
    /// Row length in samples (`blocks_per_line · 8`).
    pub stride: usize,
    /// Horizontal sampling factor of the component (for later upsampling).
    pub h: u8,
    /// Vertical sampling factor of the component (for later upsampling).
    pub v: u8,
}

impl Plane {
    /// Grows the backing store so at least `rows` pixel rows exist (zero-filled). Written as a
    /// resize to `max(len, want)` — never a shrink — so there is no "already big enough" comparison
    /// whose boundary would be unobservable (a same-length resize is a no-op).
    fn ensure_rows(&mut self, rows: usize) {
        let want = rows * self.stride;
        self.data.resize(self.data.len().max(want), 0);
    }
}

/// The result of decoding one scan: each coded component's plane (paired with its frame index) and
/// the position of the marker that terminated the entropy data, where the segment parser resumes.
pub struct ScanResult {
    /// `(frame_component_index, plane)` for every component coded by this scan.
    pub planes: Vec<(usize, Plane)>,
    /// Byte offset of the terminating marker's `0xFF` prefix, where the segment parser resumes.
    pub marker_offset: usize,
}

/// Allocation ceiling for a sequential scan whose frame height is deferred to DNL.
#[derive(Clone, Copy)]
pub struct McuRowLimit {
    /// Maximum number of MCU rows the configured limits can permit.
    pub rows: usize,
    /// Static [`Error::Unsupported`] message identifying the limit that supplied this ceiling.
    pub message: &'static str,
}

/// The outcome of fetching one destuffed entropy byte.
enum Fetch {
    /// A data byte (any stuffed `0x00` after `0xFF` already consumed).
    Data(u8),
    /// A marker was reached; the reader's `marker`/`marker_off` are set and `pos` is frozen.
    Marker,
    /// The input ended without a terminating marker (truncated stream).
    Eof,
}

/// A most-significant-bit-first reader over the entropy-coded segment, applying §F.2.2.5 byte
/// destuffing (`0xFF 0x00` → literal `0xFF`) and stopping cleanly at any real marker (fill `0xFF`
/// bytes before a marker per §B.1.1.2 are skipped). Once a marker is reached, further bit requests
/// are satisfied with `1`-padding so the final valid symbols still decode; the byte cursor stays
/// parked on the marker.
///
/// The buffer is 64 bits wide and [`Self::fill`] tops it up a whole byte at a time to more than 56
/// bits, so a read is a shift and a mask rather than a byte fetch per request, and a Huffman code
/// can be resolved from a single 16-bit [`Self::peek`] (issue #305). Prefetching runs ahead of what
/// the decoder has consumed, and is invisible to every caller: whatever it appends past the
/// entropy data -- `1`-padding at a marker, zero bytes at the end of a truncated stream -- is
/// counted in [`Self::pad_bits`], and [`Self::consume`] is where reaching into it is judged, so a
/// truncation is reported at the read that needs the missing bits, exactly as a byte-at-a-time
/// reader would.
struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
    bit_buf: u64,
    bit_count: u32,
    /// How many of the low `bit_count` buffered bits were appended past the entropy data rather
    /// than read from it: `1`-padding past a marker, or zero bytes past the end of a stream that
    /// has no marker. Padding only ever follows the last data byte, so it is always the low end of
    /// the buffer, and a stream reaches at most one of the two kinds.
    pad_bits: u32,
    /// Whether a read has consumed marker padding: the decoder needed more bits than the entropy
    /// data held before its marker.
    overread: bool,
    marker: Option<u8>,
    marker_off: usize,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8], start: usize) -> Self {
        Self {
            data,
            pos: start,
            bit_buf: 0,
            bit_count: 0,
            pad_bits: 0,
            overread: false,
            marker: None,
            marker_off: start,
        }
    }

    /// Fetches the next destuffed data byte, or reports a marker / EOF (§F.2.2.5).
    fn fetch(&mut self) -> Fetch {
        if self.marker.is_some() {
            return Fetch::Marker;
        }
        loop {
            let b = match self.data.get(self.pos) {
                Some(&b) => b,
                None => return Fetch::Eof,
            };
            if b != 0xFF {
                self.pos += 1;
                return Fetch::Data(b);
            }
            match self.data.get(self.pos + 1) {
                None => return Fetch::Eof,
                Some(0x00) => {
                    self.pos += 2;
                    return Fetch::Data(0xFF);
                }
                Some(0xFF) => {
                    // Fill byte before a marker (§B.1.1.2): skip it and re-examine.
                    self.pos += 1;
                }
                Some(&m) => {
                    self.marker = Some(m);
                    self.marker_off = self.pos;
                    return Fetch::Marker;
                }
            }
        }
    }

    /// Tops the buffer up with as many whole bytes as its 64 bits have room for: entropy data while
    /// there is some, then `1`-padding past a marker or zero bytes past the end of a truncated
    /// stream, both counted in [`Self::pad_bits`]. Afterwards at least 57 bits are buffered, so any
    /// read of up to 16 bits can be [`Self::peek`]ed.
    ///
    /// The byte count is computed before the loop rather than re-tested against the buffered count
    /// on each pass, so the loop is bounded by its own arithmetic whatever the body does.
    ///
    /// Each byte is composed with `+` rather than `|`: the shift just vacated 8 low zero bits and
    /// the addend is `< 256`, so the addition fills exactly those vacant bits (the same convention
    /// as `bitwriter`/`pack_nibbles`, where `|` and `^` would be equivalent on disjoint operands).
    fn fill(&mut self) {
        let room = (64 - self.bit_count) / 8;
        for _ in 0..room {
            let byte = match self.fetch() {
                Fetch::Data(b) => b,
                Fetch::Marker => {
                    self.pad_bits += 8;
                    0xFF
                }
                Fetch::Eof => {
                    self.pad_bits += 8;
                    0x00
                }
            };
            self.bit_buf = (self.bit_buf << 8) + u64::from(byte);
        }
        self.bit_count += room * 8;
    }

    /// The next `n` (`1..=16`) buffered bits, MSB-first, without consuming them. Valid after
    /// [`Self::fill`], which leaves more than 16 bits buffered.
    fn peek(&self, n: u32) -> u32 {
        ((self.bit_buf >> (self.bit_count - n)) & ((1u64 << n) - 1)) as u32
    }

    /// Consumes the next `n` buffered bits. Reaching past the entropy data is allowed at a marker
    /// (the `1`-padding the final symbols decode from, recorded as an overread) and is a truncated
    /// stream otherwise -- the error a byte-at-a-time reader raises when its fetch finds no byte.
    fn consume(&mut self, n: u32) -> Result<()> {
        let data_bits = self.bit_count - self.pad_bits;
        if n > data_bits {
            if self.marker.is_none() {
                return Err(Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "JPEG: truncated entropy-coded data",
                )
                .with_byte_offset(self.pos as u64));
            }
            // Past the data, padding is all there is: what remains buffered is no longer told
            // apart, because `at_data_end` answers from `overread` alone from here on.
            self.overread = true;
            self.pad_bits = 0;
        }
        self.bit_count -= n;
        Ok(())
    }

    /// Reads `n` (`0..=16`) bits MSB-first (`RECEIVE`, Figure F.17).
    fn read_bits(&mut self, n: u32) -> Result<u32> {
        if n == 0 {
            return Ok(0);
        }
        self.fill();
        let bits = self.peek(n);
        self.consume(n)?;
        Ok(bits)
    }

    /// Reads one bit (the `NEXTBIT` of Figure F.16).
    fn read_bit(&mut self) -> Result<i32> {
        self.fill();
        let bit = self.peek(1);
        self.consume(1)?;
        Ok(bit as i32)
    }

    /// Whether, at an MCU-row boundary, only `1`-padding remains before a marker — the end of the
    /// entropy data for a `Y = 0` (DNL) frame whose MCU-row count is not known in advance. In the
    /// middle of a scan the pending bits are real data (or the next byte is not a marker), so this is
    /// `false`; a false positive would require the pending bits to be all-`1` *and* the next byte to
    /// begin a marker, which coincide only at the true end.
    ///
    /// "Pending" means bits the decoder has not consumed and that came from entropy data: the
    /// prefetched padding past a marker is not pending, and a decoder that has already consumed
    /// padding is past the end by definition.
    fn at_data_end(&self) -> bool {
        if self.overread {
            return true;
        }
        let pending = self.bit_count - self.pad_bits;
        if pending >= 8 {
            return false; // a whole unconsumed data byte is buffered
        }
        let mask = (1u64 << pending) - 1;
        if ((self.bit_buf >> self.pad_bits) & mask) != mask {
            return false; // pending bits are not all-1 padding
        }
        // Peek forward for a marker without consuming (mirrors `fetch`): skip any run of fill 0xFF
        // bytes (§B.1.1.2) and classify the byte that follows.
        let fill = self
            .data
            .get(self.pos..)
            .unwrap_or_default()
            .iter()
            .take_while(|&&b| b == 0xFF)
            .count();
        match self.data.get(self.pos + fill) {
            // No 0xFF prefix at all: still inside the destuffed data byte stream.
            _ if fill == 0 => false,
            // Truncated, or a stuffed `0xFF 0x00` (literal data), or a restart marker: not the end.
            // A restart (RSTm) is a mid-scan byte-aligned boundary — a complete scan never emits a
            // trailing RST, so an RSTm always precedes more MCUs (§E.2.5). Treating it as the end
            // would truncate a Y=0 frame whose restart interval lands on an MCU-row start.
            None | Some(0x00) => false,
            Some(&m) if (0xD0..=0xD7).contains(&m) => false,
            // Any other marker (EOI, DNL, …) terminates the entropy data.
            Some(_) => true,
        }
    }

    /// Byte-aligns by discarding everything buffered: the partial byte the decoder was in, and any
    /// whole bytes prefetched past it, which the marker search below would skip as data anyway.
    fn discard_buffer(&mut self) {
        self.bit_count = 0;
        self.bit_buf = 0;
        self.pad_bits = 0;
        self.overread = false;
    }

    /// Consumes a restart marker at an interval boundary (§E.2.5): byte-aligns, verifies the marker
    /// is `RSTm` with `m == expected`, and advances past it. The caller resets the DC predictors.
    fn take_restart(&mut self, expected: u8) -> Result<()> {
        self.discard_buffer();
        let code = loop {
            match self.fetch() {
                Fetch::Data(_) => continue,
                Fetch::Marker => break self.marker.unwrap_or(0),
                Fetch::Eof => {
                    return Err(Error::invalid_input(
                        env!("CARGO_PKG_NAME"),
                        "JPEG: truncated before restart marker",
                    ));
                }
            }
        };
        if !(0xD0..=0xD7).contains(&code) {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "JPEG: expected restart marker",
            ));
        }
        if code - 0xD0 != expected {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "JPEG: restart marker out of sequence",
            ));
        }
        self.pos = self.marker_off + 2;
        self.marker = None;
        Ok(())
    }

    /// Byte-aligns and advances to the marker that ends the scan, returning `(code, offset)` without
    /// consuming it (the segment parser resumes at `offset`).
    fn end_marker(&mut self) -> Result<(u8, usize)> {
        self.discard_buffer();
        loop {
            match self.fetch() {
                Fetch::Data(_) => continue,
                Fetch::Marker => return Ok((self.marker.unwrap_or(0), self.marker_off)),
                Fetch::Eof => {
                    return Err(Error::invalid_input(
                        env!("CARGO_PKG_NAME"),
                        "JPEG: missing marker after scan",
                    ));
                }
            }
        }
    }
}

/// `EXTEND` (Figure F.12): sign-extends the `t`-bit magnitude `v` to a signed value. `t ≥ 1`.
fn extend(v: u32, t: u8) -> i32 {
    let t = u32::from(t);
    let v = v as i32;
    if v < (1 << (t - 1)) {
        v - (1 << t) + 1
    } else {
        v
    }
}

/// `DECODE` (Figure F.16): resolves the canonical code at the head of the bit stream, returning its
/// symbol.
///
/// The procedure runs over a 16-bit [`BitReader::peek`] -- one probe of the table's precomputed
/// [`DecTable::lookup`] for codes of up to [`LOOKAHEAD`] bits, [`DecTable::walk`] otherwise -- and
/// then consumes exactly the bits it read. A stream that ends inside the code therefore fails in
/// [`BitReader::consume`], as reading those bits one at a time would.
fn decode_symbol(table: &DecTable, r: &mut BitReader) -> Result<u8> {
    r.fill();
    let bits = r.peek(16);
    // A miss is told apart by `NonZeroU16`, not by comparing the entry with zero: inverting that
    // comparison would send misses to a zero-length "hit" that consumes nothing, which a Y=0 frame
    // can only report as a hang, never as a failure.
    let (length, symbol) = match NonZeroU16::new(table.lookup(bits >> (16 - LOOKAHEAD))) {
        Some(entry) => (u32::from(entry.get() >> 8), Ok((entry.get() & 0xFF) as u8)),
        None => table.walk(bits),
    };
    r.consume(length)?;
    symbol
}

/// Per-scan-component decode context: the tables it uses, its sampling, and its running DC predictor
/// and output plane.
struct Ctx<'a> {
    frame_index: usize,
    h: u8,
    v: u8,
    dc: &'a DecTable,
    ac: &'a DecTable,
    quant: &'a [u16; 64],
    pred: i32,
    plane: Plane,
}

impl Ctx<'_> {
    /// Decodes one 8×8 block (§F.2.2): the differential DC coefficient then the run/size AC
    /// coefficients, dequantizes into natural order, inverse-transforms, level-shifts (+128) and
    /// clamps to `0..=255`, and writes the samples into the plane at block position `(bx, by)`
    /// (§A.3.1).
    fn decode_block(&mut self, r: &mut BitReader, bx: usize, by: usize) -> Result<()> {
        let mut zz = [0i32; 64];
        // DC: differential against the running predictor (§F.2.2.1).
        let t = decode_symbol(self.dc, r)?;
        if t > 11 {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "JPEG: DC magnitude category > 11",
            ));
        }
        let diff = if t == 0 {
            0
        } else {
            extend(r.read_bits(u32::from(t))?, t)
        };
        self.pred = self.pred.wrapping_add(diff);
        zz[0] = self.pred.wrapping_mul(i32::from(self.quant[0]));
        // AC: run/size symbols in zig-zag order (§F.2.2.2).
        let mut k = 1usize;
        while k < 64 {
            let rs = decode_symbol(self.ac, r)?;
            let run = usize::from(rs >> 4);
            let size = rs & 0x0F;
            if size == 0 {
                if run == 15 {
                    k += 16; // ZRL: sixteen zero coefficients
                    continue;
                }
                break; // EOB: the rest of the block is zero
            }
            k += run;
            if k > 63 {
                return Err(Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "JPEG: AC coefficient index past 63",
                ));
            }
            let coeff = extend(r.read_bits(u32::from(size))?, size);
            let natural = ZIGZAG[k];
            zz[natural] = coeff.wrapping_mul(i32::from(self.quant[natural]));
            k += 1;
        }
        // Inverse DCT, level shift and clamp (§A.3.1).
        write_block(&mut zz, &mut self.plane, bx, by);
        Ok(())
    }
}

/// Inverse-transforms a dequantized natural-order block `zz`, level-shifts (+128) and clamps each
/// sample to `0..=255`, and writes the 8×8 result into `plane` at block position `(bx, by)`
/// (§A.3.1). The single reconstruction tail shared by the sequential ([`Ctx::decode_block`]) and
/// progressive ([`ProgComp::into_plane`]) paths, so both produce byte-identical samples.
fn write_block(zz: &mut [i32; 64], plane: &mut Plane, bx: usize, by: usize) {
    idct8x8(zz);
    plane.ensure_rows((by + 1) * 8);
    let stride = plane.stride;
    for (row, samples) in zz.as_chunks::<8>().0.iter().enumerate() {
        let dst = (by * 8 + row) * stride + bx * 8;
        for (out, &s) in plane.data[dst..dst + 8].iter_mut().zip(samples) {
            *out = (s + 128).clamp(0, 255) as u8;
        }
    }
}

/// Decodes the entropy data of one scan starting at byte `start`, per §A.2 and Annex F.
///
/// `frame` gives the component geometry (Hmax/Vmax, X/Y); `scan` selects the coded components and
/// their tables; `tables` holds the current dequantization/Huffman tables and restart interval. When
/// `frame.y == 0` the MCU-row count is unknown up front and rows are decoded until the entropy data
/// ends at a marker (a `Y = 0` frame's height arrives in the following DNL segment).
///
/// # Errors
///
/// [`Error::InvalidInput`] for an undefined referenced table, a corrupt Huffman code, an
/// out-of-range coefficient, a bad restart sequence, or a truncated stream.
pub fn decode_scan(
    data: &[u8],
    start: usize,
    frame: &Frame,
    scan: &ScanHeader,
    tables: &Tables,
    row_limit: Option<McuRowLimit>,
) -> Result<ScanResult> {
    let hmax = usize::from(frame.hmax());
    let vmax = usize::from(frame.vmax());
    let x = usize::from(frame.x);
    let interleaved = scan.interleaved();

    // Horizontal MCU count is always known (X ≠ 0). The vertical count is known only when Y ≠ 0.
    let mcus_x = if interleaved {
        x.div_ceil(8 * hmax)
    } else {
        // Non-interleaved: MCU = one block; blocks span the component's own width (§A.2.2).
        let fc = &frame.components[scan.components[0].frame_index];
        let comp_w = (x * usize::from(fc.h)).div_ceil(hmax);
        comp_w.div_ceil(8)
    };
    let mcus_y: Option<usize> = if frame.y == 0 {
        None
    } else {
        let y = usize::from(frame.y);
        Some(if interleaved {
            y.div_ceil(8 * vmax)
        } else {
            let fc = &frame.components[scan.components[0].frame_index];
            let comp_h = (y * usize::from(fc.v)).div_ceil(vmax);
            comp_h.div_ceil(8)
        })
    };

    // Build a decode context per scan component, sized to its block layout in this scan.
    let mut ctxs: Vec<Ctx> = Vec::with_capacity(scan.components.len());
    for sc in &scan.components {
        let fc = &frame.components[sc.frame_index];
        let dc = tables.dc[usize::from(sc.td)].as_ref().ok_or_else(|| {
            Error::invalid_input(env!("CARGO_PKG_NAME"), "JPEG: scan uses undefined DC table")
        })?;
        let ac = tables.ac[usize::from(sc.ta)].as_ref().ok_or_else(|| {
            Error::invalid_input(env!("CARGO_PKG_NAME"), "JPEG: scan uses undefined AC table")
        })?;
        let quant = tables.quant[usize::from(fc.tq)].as_ref().ok_or_else(|| {
            Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "JPEG: scan uses undefined quant table",
            )
        })?;
        let blk_cols = if interleaved { usize::from(fc.h) } else { 1 };
        let blocks_per_line = mcus_x * blk_cols;
        ctxs.push(Ctx {
            frame_index: sc.frame_index,
            h: fc.h,
            v: fc.v,
            dc,
            ac,
            quant,
            pred: 0,
            plane: Plane {
                data: Vec::new(),
                stride: blocks_per_line * 8,
                h: fc.h,
                v: fc.v,
            },
        });
    }

    let mut reader = BitReader::new(data, start);
    let restart = usize::from(tables.restart_interval);
    let mut mcus_done = 0usize;
    let mut expected_rst = 0u8;
    let mut mcu_y = 0usize;

    loop {
        match mcus_y {
            Some(rows) if mcu_y >= rows => break,
            None if reader.at_data_end() => break,
            None if row_limit.is_some_and(|limit| mcu_y >= limit.rows) => {
                return Err(Error::unsupported(
                    env!("CARGO_PKG_NAME"),
                    row_limit.map_or("JPEG: image exceeds a decoder limit", |limit| limit.message),
                ));
            }
            _ => {}
        }
        for mcu_x in 0..mcus_x {
            if restart != 0 && mcus_done != 0 && mcus_done.is_multiple_of(restart) {
                reader.take_restart(expected_rst)?;
                expected_rst = (expected_rst + 1) & 7;
                for ctx in &mut ctxs {
                    ctx.pred = 0;
                }
            }
            for ctx in &mut ctxs {
                if interleaved {
                    let (h, v) = (usize::from(ctx.h), usize::from(ctx.v));
                    for by in 0..v {
                        for bx in 0..h {
                            ctx.decode_block(&mut reader, mcu_x * h + bx, mcu_y * v + by)?;
                        }
                    }
                } else {
                    ctx.decode_block(&mut reader, mcu_x, mcu_y)?;
                }
            }
            mcus_done += 1;
        }
        mcu_y += 1;
    }

    let (_marker, marker_offset) = reader.end_marker()?;
    let planes = ctxs.into_iter().map(|c| (c.frame_index, c.plane)).collect();
    Ok(ScanResult {
        planes,
        marker_offset,
    })
}

// ================================================================================================
// Progressive DCT decoding (T.81 Annex G).
// ================================================================================================

/// One frame component's progressive DCT-coefficient accumulator (§G.1.1). It owns the component's
/// entire block grid of natural-order coefficients, filled incrementally by the progressive scans
/// and dequantized + inverse-transformed only once, at frame completion, by [`Self::into_plane`].
pub struct ProgComp {
    /// Horizontal sampling factor `Hi` (carried through to the reconstructed [`Plane`]).
    h: u8,
    /// Vertical sampling factor `Vi`.
    v: u8,
    /// Width of the component's block grid in blocks, `ceil(comp_w / 8)` (§A.2.2).
    bw: usize,
    /// Height of the component's block grid in blocks, `ceil(comp_h / 8)`.
    bh: usize,
    /// `bw · bh` contiguous 64-coefficient blocks in natural (raster) order, zero-initialised.
    coeffs: Vec<i32>,
    /// The dequantization table, bound at the component's **first** scan (§B.2.4.1) and thereafter
    /// immune to DQT redefinition, mirroring the reference decoder's latch.
    quant: [u16; 64],
    /// Whether [`Self::quant`] has been bound yet.
    quant_bound: bool,
    /// Per zig-zag position, the successive-approximation bit position `Al` of the most recent scan
    /// that coded it, or `None` if never coded — the band bookkeeping (§G.1.1.1) that rejects
    /// out-of-order and overlapping scans.
    coded_al: [Option<u8>; 64],
}

impl ProgComp {
    /// Allocates an all-zero coefficient buffer for a component with the given sampling factors and
    /// block-grid dimensions.
    #[must_use]
    pub fn new(h: u8, v: u8, bw: usize, bh: usize) -> Self {
        Self {
            h,
            v,
            bw,
            bh,
            coeffs: vec![0i32; bw * bh * 64],
            quant: [0u16; 64],
            quant_bound: false,
            coded_al: [None; 64],
        }
    }

    /// Whether the component's DC first pass has been delivered (§G.1.1.1.1 requires it before any
    /// AC scan, and the partial-render policy requires it before reconstruction).
    #[must_use]
    pub fn has_dc(&self) -> bool {
        self.coded_al[0].is_some()
    }

    /// The natural-order coefficients of block `(bx, by)`.
    fn block_mut(&mut self, bx: usize, by: usize) -> &mut [i32] {
        let off = (by * self.bw + bx) * 64;
        &mut self.coeffs[off..off + 64]
    }

    /// Dequantizes (§A.3.4), inverse-transforms (§A.3.3) and level-shifts (§A.3.1) every block into
    /// a reconstructed sample [`Plane`] — the once-only progressive reconstruction at EOI.
    #[must_use]
    pub fn into_plane(self) -> Plane {
        let stride = self.bw * 8;
        let mut plane = Plane {
            data: vec![0u8; stride * self.bh * 8],
            stride,
            h: self.h,
            v: self.v,
        };
        for by in 0..self.bh {
            for bx in 0..self.bw {
                let off = (by * self.bw + bx) * 64;
                let mut zz = [0i32; 64];
                for (i, cell) in zz.iter_mut().enumerate() {
                    *cell = self.coeffs[off + i].wrapping_mul(i32::from(self.quant[i]));
                }
                write_block(&mut zz, &mut plane, bx, by);
            }
        }
        plane
    }
}

/// Verifies the scan's band ordering against each component's history and updates it (§G.1.1.1).
/// A first pass (`Ah = 0`) requires every band position to be previously uncoded; a refinement
/// (`Ah ≠ 0`) requires every position to have been coded at exactly `Al = Ah` in a prior scan; an AC
/// scan additionally requires the component's DC first pass to have already happened (§G.1.1.1.1).
fn validate_progression(scan: &ScanHeader, comps: &mut [ProgComp]) -> Result<()> {
    let (ss, se) = (usize::from(scan.ss), usize::from(scan.se));
    for sc in &scan.components {
        let c = &mut comps[sc.frame_index];
        let (lo, hi) = if ss == 0 { (0, 0) } else { (ss, se) };
        if ss != 0 && !c.has_dc() {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "JPEG: progressive AC scan before the component's DC scan",
            ));
        }
        for k in lo..=hi {
            match (scan.ah, c.coded_al[k]) {
                // First pass of a band: the position must not have been coded yet (no overlap).
                (0, None) => {}
                (0, Some(_)) => {
                    return Err(Error::invalid_input(
                        env!("CARGO_PKG_NAME"),
                        "JPEG: progressive band coded twice at the first pass (overlap)",
                    ));
                }
                // Refinement: the position's history must be exactly Ah (its previous Al).
                (ah, Some(prev)) if prev == ah => {}
                (_, _) => {
                    return Err(Error::invalid_input(
                        env!("CARGO_PKG_NAME"),
                        "JPEG: progressive refinement without a matching first pass",
                    ));
                }
            }
        }
        for slot in c.coded_al[lo..=hi].iter_mut() {
            *slot = Some(scan.al);
        }
    }
    Ok(())
}

/// A resolved scan component for the progressive entropy loop: its frame index, sampling factors,
/// and whichever entropy table the scan's band requires (DC for `Ss = 0`, AC otherwise).
struct ProgSc<'a> {
    frame_index: usize,
    h: usize,
    v: usize,
    dc: Option<&'a DecTable>,
    ac: Option<&'a DecTable>,
}

/// Decodes one progressive scan's entropy data starting at byte `start`, accumulating coefficients
/// into `comps` (§G.2). Returns the byte offset of the marker that terminates the entropy data.
///
/// The scan carries one band `[Ss..=Se]` at successive-approximation precision `(Ah, Al)` (validated
/// by [`crate::syntax::parse_sos`] for progressive). DC scans (`Ss = 0`) may interleave up to four
/// components; AC scans (`Ss ≥ 1`) are single-component (§B.2.3). First passes (`Ah = 0`) Huffman-code
/// the band scaled by the point transform `2^Al` (§A.4); refinements (`Ah ≠ 0`) append correction /
/// newly-nonzero bits (§G.1.2.3). At each restart boundary the DC predictors and the EOB run counter
/// are reset (§E.2.5, §G.1.2.2).
///
/// # Errors
///
/// [`Error::InvalidInput`] for an undefined referenced table, an out-of-order or overlapping scan,
/// a corrupt Huffman code, an out-of-band coefficient index, a bad restart sequence, or a truncated
/// stream.
pub fn decode_progressive_scan(
    data: &[u8],
    start: usize,
    frame: &Frame,
    scan: &ScanHeader,
    tables: &Tables,
    comps: &mut [ProgComp],
) -> Result<usize> {
    let (ss, se) = (usize::from(scan.ss), usize::from(scan.se));
    let (ah, al) = (scan.ah, scan.al);
    let interleaved = scan.interleaved();

    // Bind each scan component's quantization table at its first reference (§B.2.4.1), then check
    // and update the band-progression bookkeeping.
    for sc in &scan.components {
        let c = &mut comps[sc.frame_index];
        if !c.quant_bound {
            let fc = &frame.components[sc.frame_index];
            c.quant = tables.quant[usize::from(fc.tq)].ok_or_else(|| {
                Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "JPEG: scan uses undefined quant table",
                )
            })?;
            c.quant_bound = true;
        }
    }
    validate_progression(scan, comps)?;

    // Resolve the entropy table each scan component needs for this band.
    let mut scs: Vec<ProgSc> = Vec::with_capacity(scan.components.len());
    for sc in &scan.components {
        let fc = &frame.components[sc.frame_index];
        let dc = if ss == 0 {
            Some(tables.dc[usize::from(sc.td)].as_ref().ok_or_else(|| {
                Error::invalid_input(env!("CARGO_PKG_NAME"), "JPEG: scan uses undefined DC table")
            })?)
        } else {
            None
        };
        let ac = if ss == 0 {
            None
        } else {
            Some(tables.ac[usize::from(sc.ta)].as_ref().ok_or_else(|| {
                Error::invalid_input(env!("CARGO_PKG_NAME"), "JPEG: scan uses undefined AC table")
            })?)
        };
        scs.push(ProgSc {
            frame_index: sc.frame_index,
            h: usize::from(fc.h),
            v: usize::from(fc.v),
            dc,
            ac,
        });
    }

    // MCU grid: interleaved (DC) scans use the padded whole-image MCU grid; a non-interleaved scan
    // (AC, or a single-component DC scan) walks the component's own block grid, one block per MCU
    // (§A.2.2).
    let (mcus_x, mcus_y) = if interleaved {
        let hmax = usize::from(frame.hmax());
        let vmax = usize::from(frame.vmax());
        (
            usize::from(frame.x).div_ceil(8 * hmax),
            usize::from(frame.y).div_ceil(8 * vmax),
        )
    } else {
        (comps[scs[0].frame_index].bw, comps[scs[0].frame_index].bh)
    };

    let mut reader = BitReader::new(data, start);
    let restart = usize::from(tables.restart_interval);
    let mut preds = vec![0i32; scs.len()];
    let mut eobrun = 0u32;
    let mut mcus_done = 0usize;
    let mut expected_rst = 0u8;

    for my in 0..mcus_y {
        for mx in 0..mcus_x {
            if restart != 0 && mcus_done != 0 && mcus_done.is_multiple_of(restart) {
                reader.take_restart(expected_rst)?;
                expected_rst = (expected_rst + 1) & 7;
                preds.fill(0);
                eobrun = 0; // the EOB run does not cross a restart boundary (§G.1.2.2)
            }
            for (si, sc) in scs.iter().enumerate() {
                if ss == 0 {
                    // DC band: one block per (Hi, Vi) sub-position in an interleaved MCU, else one.
                    let (nh, nv) = if interleaved { (sc.h, sc.v) } else { (1, 1) };
                    for by in 0..nv {
                        for bx in 0..nh {
                            let (bcol, brow) = if interleaved {
                                (mx * sc.h + bx, my * sc.v + by)
                            } else {
                                (mx, my)
                            };
                            let c = &mut comps[sc.frame_index];
                            // Padding blocks in a partial rightmost/bottom MCU are decoded (to keep
                            // the entropy stream and DC predictor in sync) but not stored (§A.2.2).
                            let real = bcol < c.bw && brow < c.bh;
                            if ah == 0 {
                                let t = decode_symbol(sc.dc.unwrap(), &mut reader)?;
                                if t > 11 {
                                    return Err(Error::invalid_input(
                                        env!("CARGO_PKG_NAME"),
                                        "JPEG: DC magnitude category > 11",
                                    ));
                                }
                                let diff = if t == 0 {
                                    0
                                } else {
                                    extend(reader.read_bits(u32::from(t))?, t)
                                };
                                preds[si] = preds[si].wrapping_add(diff);
                                if real {
                                    // First pass: scale by the point transform 2^Al (§A.4, §G.1.2.1).
                                    c.block_mut(bcol, brow)[0] = preds[si] << al;
                                }
                            } else {
                                // Refinement: append the next lower bit of the DC coefficient (§G.1.2.3).
                                let bit = reader.read_bit()?;
                                if real && bit != 0 {
                                    c.block_mut(bcol, brow)[0] |= 1i32 << al;
                                }
                            }
                        }
                    }
                } else {
                    // AC band: single component, one block per MCU (non-interleaved).
                    let c = &mut comps[sc.frame_index];
                    let block = c.block_mut(mx, my);
                    if ah == 0 {
                        decode_ac_first(
                            &mut reader,
                            block,
                            ss,
                            se,
                            al,
                            sc.ac.unwrap(),
                            &mut eobrun,
                        )?;
                    } else {
                        decode_ac_refine(
                            &mut reader,
                            block,
                            ss,
                            se,
                            al,
                            sc.ac.unwrap(),
                            &mut eobrun,
                        )?;
                    }
                }
            }
            mcus_done += 1;
        }
    }

    let (_marker, marker_offset) = reader.end_marker()?;
    Ok(marker_offset)
}

/// Decodes one block's AC band in a first-pass progressive scan (`Ah = 0`, §G.1.2.2, Figure G.3
/// mirrored for decode). Coefficients are Huffman run/size coded within `[Ss..=Se]` and scaled by
/// the point transform `2^Al` (§A.4); `EOBRUN` counts whole all-zero-band blocks and persists across
/// blocks within the restart interval.
fn decode_ac_first(
    r: &mut BitReader,
    block: &mut [i32],
    ss: usize,
    se: usize,
    al: u8,
    ac: &DecTable,
    eobrun: &mut u32,
) -> Result<()> {
    // This block is inside a still-running EOB run: it is entirely zero in this band.
    if *eobrun > 0 {
        *eobrun -= 1;
        return Ok(());
    }
    let mut k = ss;
    while k <= se {
        let rs = decode_symbol(ac, r)?;
        let run = usize::from(rs >> 4);
        let size = rs & 0x0F;
        if size == 0 {
            if run != 15 {
                // EOBn: run length is 2^run plus `run` appended bits; this block is the first of
                // the run, so decrement once (§G.1.2.2, Table G.1).
                *eobrun = 1u32 << run;
                if run != 0 {
                    *eobrun += r.read_bits(run as u32)?;
                }
                *eobrun -= 1;
                return Ok(());
            }
            k += 16; // ZRL: sixteen zero coefficients
        } else {
            k += run;
            if k > se {
                return Err(Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "JPEG: progressive AC run past band end",
                ));
            }
            let coeff = extend(r.read_bits(u32::from(size))?, size);
            block[ZIGZAG[k]] = coeff << al;
            k += 1;
        }
    }
    Ok(())
}

/// Decodes one block's AC band in a successive-approximation refinement scan (`Ah ≠ 0`, §G.1.2.3 /
/// §G.2.2). For every coefficient with a non-zero history a correction bit is read (a 1-bit adds one
/// to the scaled magnitude); newly non-zero coefficients (`SSSS = 1`) are inserted per the run/size
/// symbols; EOB-run blocks still consume correction bits for their non-zero-history coefficients.
fn decode_ac_refine(
    r: &mut BitReader,
    block: &mut [i32],
    ss: usize,
    se: usize,
    al: u8,
    ac: &DecTable,
    eobrun: &mut u32,
) -> Result<()> {
    let p1: i32 = 1i32 << al; // +1 in the bit position being refined
    let m1: i32 = -1i32 << al; // −1 in the bit position being refined
    let mut k = ss;
    if *eobrun == 0 {
        while k <= se {
            let rs = decode_symbol(ac, r)?;
            let mut run = i32::from(rs >> 4);
            let size = rs & 0x0F;
            let mut new_val = 0i32;
            if size != 0 {
                // In a refinement scan a newly non-zero coefficient always has magnitude 1; the one
                // appended bit is its sign (1 → positive, 0 → negative) (§G.1.2.3 rule a).
                if size != 1 {
                    return Err(Error::invalid_input(
                        env!("CARGO_PKG_NAME"),
                        "JPEG: progressive AC refinement coefficient size ≠ 1",
                    ));
                }
                new_val = if r.read_bit()? != 0 { p1 } else { m1 };
            } else if run != 15 {
                // EOBn: the rest of this block (and following blocks) is handled by the EOB tail.
                *eobrun = 1u32 << run;
                if run != 0 {
                    *eobrun += r.read_bits(run as u32)?;
                }
                break;
            }
            // else run == 15 (ZRL): skip sixteen zero-history coefficients, new_val stays 0.

            // Advance over the band, appending a correction bit to each non-zero-history coefficient
            // and counting down `run` zero-history coefficients to the target insertion position.
            loop {
                let nat = ZIGZAG[k];
                if block[nat] != 0 {
                    if r.read_bit()? != 0 {
                        block[nat] = refine(block[nat], p1, m1);
                    }
                } else {
                    run -= 1;
                    if run < 0 {
                        break; // reached the target zero-history coefficient
                    }
                }
                k += 1;
                if k > se {
                    break;
                }
            }
            if new_val != 0 {
                if k > se {
                    return Err(Error::invalid_input(
                        env!("CARGO_PKG_NAME"),
                        "JPEG: progressive AC refinement past band end",
                    ));
                }
                block[ZIGZAG[k]] = new_val;
            }
            k += 1;
        }
    }
    if *eobrun > 0 {
        // A block within an EOB run: no new coefficients, but non-zero-history coefficients in the
        // remaining band still receive their correction bits (§G.1.2.3).
        while k <= se {
            let nat = ZIGZAG[k];
            if block[nat] != 0 && r.read_bit()? != 0 {
                block[nat] = refine(block[nat], p1, m1);
            }
            k += 1;
        }
        *eobrun -= 1;
    }
    Ok(())
}

/// Applies one successive-approximation correction bit to a non-zero coefficient (§G.1.2.3): the
/// scaled magnitude increases by one bit — `+p1` for a positive coefficient, `+m1` (= `−p1`) for a
/// negative one. The refined bit position is always previously zero (the prior scan coded bit
/// `Ah = Al + 1` and above), so no guard against a double-set is needed.
fn refine(coeff: i32, p1: i32, m1: i32) -> i32 {
    if coeff > 0 {
        coeff.wrapping_add(p1)
    } else {
        coeff.wrapping_add(m1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extend_matches_figure_f12() {
        // t=1: v=0 → −1, v=1 → 1.
        assert_eq!(extend(0, 1), -1);
        assert_eq!(extend(1, 1), 1);
        // t=3: v in 0..3 map to −7..−4, v in 4..7 map to 4..7.
        assert_eq!(extend(0, 3), -7);
        assert_eq!(extend(3, 3), -4);
        assert_eq!(extend(4, 3), 4);
        assert_eq!(extend(7, 3), 7);
    }

    /// The bits of `bytes`, MSB-first: the reference the buffered reader must reproduce.
    fn bit(bytes: &[u8], i: usize) -> u32 {
        u32::from(bytes[i / 8] >> (7 - i % 8) & 1)
    }

    /// `BitReader::fill`/`peek`/`read_bits`: reads of every width from 1 to 16 bits, in a sequence
    /// that crosses each refill at a different offset, return exactly the stream's bits. A refill
    /// that took one byte too many would shift the oldest buffered bit out of the 64-bit word; one
    /// that took too few, or miscounted, would misalign every later read.
    #[test]
    fn bit_reader_reads_match_the_stream_across_refills() {
        let bytes: Vec<u8> = (0..64u32).map(|i| (i * 37 + 11) as u8 & 0xFE).collect();
        let mut r = BitReader::new(&bytes, 0);
        let mut at = 0usize;
        for n in (1..=16u32).cycle().take(40) {
            let want = (0..n as usize).fold(0, |v, k| v * 2 + bit(&bytes, at + k));
            assert_eq!(r.read_bits(n).unwrap(), want, "{n} bits at bit {at}");
            at += n as usize;
        }
        // `read_bit` takes the next single bit, and zero-width reads take nothing.
        assert_eq!(r.read_bits(0).unwrap(), 0);
        assert_eq!(r.read_bit().unwrap() as u32, bit(&bytes, at));
    }

    /// `BitReader::consume`: a stream that ends without a marker supplies exactly its own bits. The
    /// read that ends on the last one succeeds; the next fails as truncated, at the stream's end.
    #[test]
    fn bit_reader_truncation_fails_at_the_first_missing_bit() {
        let mut r = BitReader::new(&[0xAB, 0xCD], 0);
        assert_eq!(r.read_bits(12).unwrap(), 0xABC);
        assert_eq!(r.read_bits(4).unwrap(), 0xD);
        let err = r.read_bit().unwrap_err();
        assert_eq!(
            err.static_message(),
            Some("JPEG: truncated entropy-coded data")
        );
        assert_eq!(err.byte_offset(), Some(2));
    }

    /// `BitReader::consume` at a marker: reads past the data return `1`-padding instead of failing,
    /// and from then on `at_data_end` reports the end whatever follows.
    #[test]
    fn bit_reader_pads_with_ones_past_a_marker() {
        let mut r = BitReader::new(&[0x12, 0xFF, 0xD0], 0);
        assert_eq!(r.read_bits(8).unwrap(), 0x12);
        // Exactly at the data's end, before a restart marker: not the end of the scan.
        assert!(!r.at_data_end());
        assert_eq!(r.read_bits(5).unwrap(), 0x1F);
        assert!(r.at_data_end(), "an overread is the end, even before RSTm");
    }

    /// `BitReader::at_data_end` reads only the pending *data* bits: prefetched marker padding is
    /// not pending, a pending partial byte must be all ones, a whole pending byte means data
    /// remains, and the marker that follows must end the scan (not be a restart).
    #[test]
    fn bit_reader_at_data_end_reads_only_pending_data_bits() {
        let end_after = |bytes: &[u8], n: u32| {
            let mut r = BitReader::new(bytes, 0);
            r.read_bits(n).unwrap();
            r.at_data_end()
        };
        // Four pending bits, all ones, then EOI.
        assert!(end_after(&[0xAF, 0xFF, 0xD9], 4));
        // Four pending bits, not all ones.
        assert!(!end_after(&[0xA7, 0xFF, 0xD9], 4));
        // A whole pending byte of ones (a stuffed 0xFF) is data, not padding.
        assert!(!end_after(&[0x00, 0xFF, 0x00, 0xFF, 0xD9], 8));
        // All-ones partial byte, but the marker is a restart: more MCUs follow.
        assert!(!end_after(&[0xAF, 0xFF, 0xD3], 4));
        // Nothing read yet: a data byte is next.
        assert!(!BitReader::new(&[0x00, 0xFF, 0xD9], 0).at_data_end());
    }

    /// `decode_symbol`: codes resolved by the lookahead table and codes longer than it decode alike,
    /// and each consumes exactly its own length. The table has one code of every length 1..=16
    /// except the all-ones code of length 16, so the stream walks both paths in turn.
    #[test]
    fn decode_symbol_consumes_exactly_each_codes_length() {
        let bits = [1u8; 16];
        let values: Vec<u8> = (1..=16).collect();
        let table = DecTable::from_bits(&bits, &values).unwrap();
        // Code of length l is l−1 ones then a zero; length 16 is fifteen ones then a zero.
        let mut stream = Vec::new();
        for l in [1usize, 9, 10, 3, 16, 12, 2] {
            stream.extend(std::iter::repeat_n(1u8, l - 1));
            stream.push(0);
        }
        stream.extend([0, 1, 0, 1]); // trailing raw bits, read after the last code
        let mut packed = vec![0u8; stream.len().div_ceil(8)];
        for (i, &b) in stream.iter().enumerate() {
            packed[i / 8] += b << (7 - i % 8);
        }
        // Byte-stuff (§F.1.2.3): the long runs of ones form whole 0xFF bytes.
        let mut bytes = Vec::new();
        for b in packed {
            bytes.push(b);
            if b == 0xFF {
                bytes.push(0x00);
            }
        }
        bytes.extend([0xFF, 0xD9]);
        let mut r = BitReader::new(&bytes, 0);
        for l in [1u8, 9, 10, 3, 16, 12, 2] {
            assert_eq!(decode_symbol(&table, &mut r).unwrap(), l);
        }
        assert_eq!(r.read_bits(4).unwrap(), 0b0101);
    }

    /// `decode_symbol` on a code the table cannot resolve: sixteen ones match no code of this
    /// table (its all-ones length-16 slot is unassigned), the Figure F.16 undecodable case.
    #[test]
    fn decode_symbol_rejects_an_unassigned_code() {
        let bits = [1u8; 16];
        let values: Vec<u8> = (1..=16).collect();
        let table = DecTable::from_bits(&bits, &values).unwrap();
        let mut r = BitReader::new(&[0xFF, 0x00, 0xFF, 0x00, 0xFF, 0x00], 0);
        assert_eq!(
            decode_symbol(&table, &mut r).unwrap_err().static_message(),
            Some("JPEG: undecodable Huffman code")
        );
    }
}
