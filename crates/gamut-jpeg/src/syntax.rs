//! Marker-segment parsing for the decoder (T.81 Annex B): the frame/scan headers, the quantization
//! and Huffman table segments, the restart interval, and the advisory JFIF/Adobe application
//! segments. Every reader is bounds-checked and returns a typed error rather than panicking on
//! malformed input, per the "generous decode" contract.
//!
//! This module owns the parsed representations ([`Frame`], [`ScanHeader`], [`Tables`], [`ColorInfo`])
//! and the pure functions that fill them from segment payloads; the marker-loop driver lives in
//! [`crate::decoder`]. Payloads passed here are the segment bytes **after** the two length bytes
//! (§B.1.1.4), so a reader validating against `Table B.2/B.3` sizes checks the payload length
//! directly.

use gamut_core::{Error, Result};

use crate::huffman::DecTable;

/// The number of quantization / Huffman table destinations (`Tq`, `Th` are two-bit fields, `0..=3`).
const MAX_TABLES: usize = 4;

/// A cursor over a segment payload with bounds-checked big-endian reads.
struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    /// Bytes not yet consumed.
    fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }

    /// Reads one byte, erroring (`what`) if the payload is exhausted.
    fn u8(&mut self, what: &'static str) -> Result<u8> {
        let b = *self.data.get(self.pos).ok_or_else(|| {
            Error::invalid_input(env!("CARGO_PKG_NAME"), what).with_byte_offset(self.pos as u64)
        })?;
        self.pos += 1;
        Ok(b)
    }

    /// Reads a big-endian `u16`, erroring (`what`) if fewer than two bytes remain. Composed with
    /// `+` rather than `|`: the shift vacated the 8 low bits and `lo < 256` fills exactly them (the
    /// `bitwriter`/`pack_nibbles` convention).
    fn u16(&mut self, what: &'static str) -> Result<u16> {
        let hi = self.u8(what)?;
        let lo = self.u8(what)?;
        Ok((u16::from(hi) << 8) + u16::from(lo))
    }

    /// Borrows the next `n` bytes, erroring (`what`) if fewer remain.
    fn take(&mut self, n: usize, what: &'static str) -> Result<&'a [u8]> {
        let offset = self.pos as u64;
        let end = self.pos.checked_add(n).ok_or_else(|| {
            Error::invalid_input(env!("CARGO_PKG_NAME"), what).with_byte_offset(offset)
        })?;
        let slice = self.data.get(self.pos..end).ok_or_else(|| {
            Error::invalid_input(env!("CARGO_PKG_NAME"), what).with_byte_offset(offset)
        })?;
        self.pos = end;
        Ok(slice)
    }
}

/// One frame component (SOF, Table B.2): its id `Ci`, sampling factors `Hi`/`Vi`, and quantization
/// table destination `Tqi`.
#[derive(Debug, Clone, Copy)]
pub struct FrameComponent {
    /// Component identifier `Ci` (matched by the scan's `Csj`).
    pub id: u8,
    /// Horizontal sampling factor `Hi` (`1..=4`).
    pub h: u8,
    /// Vertical sampling factor `Vi` (`1..=4`).
    pub v: u8,
    /// Quantization-table destination selector `Tqi` (`0..=3`).
    pub tq: u8,
}

/// A parsed frame header (SOF0/SOF1, §B.2.2). Height [`Frame::y`] may be `0` until a later DNL
/// segment supplies it (§B.2.5). Precision is validated to be 8 at parse time and the SOF0/SOF1
/// distinction is handled by the marker-loop driver, so neither is retained here (the two processes
/// decode identically at 8-bit).
#[derive(Debug, Clone)]
pub struct Frame {
    /// Number of lines `Y`; `0` means "defined later by DNL".
    pub y: u16,
    /// Samples per line `X`.
    pub x: u16,
    /// The frame's components in declaration order.
    pub components: Vec<FrameComponent>,
}

impl Frame {
    /// The maximum horizontal sampling factor `Hmax` over all components (§A.1.1).
    #[must_use]
    pub fn hmax(&self) -> u8 {
        self.components.iter().map(|c| c.h).max().unwrap_or(1)
    }

    /// The maximum vertical sampling factor `Vmax` over all components (§A.1.1).
    #[must_use]
    pub fn vmax(&self) -> u8 {
        self.components.iter().map(|c| c.v).max().unwrap_or(1)
    }
}

/// One scan component (SOS, Table B.3): the index of the referenced [`Frame`] component and its DC
/// and AC entropy-table destinations `Tdj`/`Taj`.
#[derive(Debug, Clone, Copy)]
pub struct ScanComponent {
    /// Index into [`Frame::components`] of the referenced component.
    pub frame_index: usize,
    /// DC entropy-coding table destination `Tdj` (`0..=3`).
    pub td: u8,
    /// AC entropy-coding table destination `Taj` (`0..=3`).
    pub ta: u8,
}

/// A parsed scan header (SOS, §B.2.3). Baseline/sequential scans fix `Ss=0`, `Se=63`, `Ah=Al=0`;
/// progressive scans carry the spectral-selection band `[Ss..=Se]` and the successive-approximation
/// bit positions `Ah`/`Al` (§G.1.1).
#[derive(Debug, Clone)]
pub struct ScanHeader {
    /// The scan's components in interleave order.
    pub components: Vec<ScanComponent>,
    /// Start of spectral selection `Ss` (the first coded zig-zag index; `0` for a DC scan).
    pub ss: u8,
    /// End of spectral selection `Se` (the last coded zig-zag index; `0` when `Ss = 0`).
    pub se: u8,
    /// Successive-approximation bit position high `Ah` (`0` for a band's first scan; else the `Al`
    /// of the preceding scan of that band).
    pub ah: u8,
    /// Successive-approximation bit position low `Al` (the point transform applied before coding).
    pub al: u8,
}

impl ScanHeader {
    /// Whether the scan is interleaved (`Ns > 1`), per §A.2.3; a single-component scan is
    /// non-interleaved (§A.2.2).
    #[must_use]
    pub fn interleaved(&self) -> bool {
        self.components.len() > 1
    }
}

/// The decoder's mutable table state, redefinable between scans (§B.2.4): the dequantization tables
/// (natural order) and the DC/AC Huffman decode tables, each indexed by destination `0..=3`, plus
/// the current restart interval.
#[derive(Debug, Default)]
pub struct Tables {
    /// Dequantization tables in natural (raster) order; `None` until defined by a DQT segment.
    pub quant: [Option<[u16; 64]>; MAX_TABLES],
    /// DC Huffman decode tables; `None` until defined by a DHT segment.
    pub dc: [Option<DecTable>; MAX_TABLES],
    /// AC Huffman decode tables; `None` until defined by a DHT segment.
    pub ac: [Option<DecTable>; MAX_TABLES],
    /// Restart interval `Ri` in MCUs (`0` = no restarts), from the most recent DRI segment.
    pub restart_interval: u16,
}

/// Colour-interpretation hints gathered from the advisory application segments: the JFIF APP0
/// presence flag and the Adobe APP14 transform byte (TN #5116).
#[derive(Debug, Default, Clone, Copy)]
pub struct ColorInfo {
    /// Whether a JFIF APP0 segment was seen (implies YCbCr for 3-component streams).
    pub jfif: bool,
    /// The Adobe APP14 `transform` byte, if an Adobe segment was seen: `0` = unknown (RGB/CMYK),
    /// `1` = YCbCr, `2` = YCCK.
    pub adobe_transform: Option<u8>,
}

/// Parses a DQT segment (§B.2.4.1) into `tables`, supporting multiple tables per segment and both
/// 8-bit (`Pq=0`) and 16-bit (`Pq=1`) precision. Values are read in zig-zag order and stored in
/// natural order.
///
/// # Errors
///
/// An [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error on a bad precision
/// (`Pq > 1`), destination (`Tq > 3`), a zero quantization value (forbidden by Table B.5), or a length that does not match the tables.
pub fn parse_dqt(payload: &[u8], tables: &mut Tables) -> Result<()> {
    use crate::zigzag::ZIGZAG;
    let mut r = Reader::new(payload);
    while r.remaining() > 0 {
        let pq_tq = r.u8("JPEG: truncated DQT")?;
        let pq = pq_tq >> 4;
        let tq = usize::from(pq_tq & 0x0F);
        if tq >= MAX_TABLES {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "JPEG: DQT table destination > 3",
            ));
        }
        let mut table = [0u16; 64];
        match pq {
            0 => {
                let vals = r.take(64, "JPEG: truncated DQT (8-bit)")?;
                for (k, &v) in ZIGZAG.iter().zip(vals.iter()) {
                    if v == 0 {
                        return Err(Error::invalid_input(
                            env!("CARGO_PKG_NAME"),
                            "JPEG: zero quantization value",
                        ));
                    }
                    table[*k] = u16::from(v);
                }
            }
            1 => {
                for &k in ZIGZAG.iter() {
                    let v = r.u16("JPEG: truncated DQT (16-bit)")?;
                    if v == 0 {
                        return Err(Error::invalid_input(
                            env!("CARGO_PKG_NAME"),
                            "JPEG: zero quantization value",
                        ));
                    }
                    table[k] = v;
                }
            }
            _ => {
                return Err(Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "JPEG: DQT precision Pq > 1",
                ));
            }
        }
        tables.quant[tq] = Some(table);
    }
    Ok(())
}

/// Parses a DHT segment (§B.2.4.2) into `tables`, supporting multiple tables per segment. Each
/// table's BITS/HUFFVAL are validated by [`DecTable::from_bits`] (Annex C code space).
///
/// # Errors
///
/// An [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error on a bad class
/// (`Tc > 1`), destination (`Th > 3`), a length that does not match `sum(BITS)`, or an overfull code
/// space.
pub fn parse_dht(payload: &[u8], tables: &mut Tables) -> Result<()> {
    let mut r = Reader::new(payload);
    while r.remaining() > 0 {
        let tc_th = r.u8("JPEG: truncated DHT")?;
        let tc = tc_th >> 4;
        let th = usize::from(tc_th & 0x0F);
        if th >= MAX_TABLES {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "JPEG: DHT table destination > 3",
            ));
        }
        let mut bits = [0u8; 16];
        bits.copy_from_slice(r.take(16, "JPEG: truncated DHT counts")?);
        let total: usize = bits.iter().map(|&b| usize::from(b)).sum();
        let values = r.take(total, "JPEG: truncated DHT values")?;
        let table = DecTable::from_bits(&bits, values)?;
        match tc {
            0 => tables.dc[th] = Some(table),
            1 => tables.ac[th] = Some(table),
            _ => {
                return Err(Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "JPEG: DHT class Tc > 1",
                ));
            }
        }
    }
    Ok(())
}

/// Parses a DRI segment (§B.2.4.4) into a restart interval `Ri`.
///
/// # Errors
///
/// An [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if the payload is not
/// exactly two bytes.
pub fn parse_dri(payload: &[u8]) -> Result<u16> {
    let mut r = Reader::new(payload);
    let ri = r.u16("JPEG: truncated DRI")?;
    if r.remaining() != 0 {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "JPEG: DRI length must be 4",
        ));
    }
    Ok(ri)
}

/// Parses a DNL segment (§B.2.5) into a number-of-lines `NL`.
///
/// # Errors
///
/// An [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if the payload is not
/// exactly two bytes or `NL == 0`.
pub fn parse_dnl(payload: &[u8]) -> Result<u16> {
    let mut r = Reader::new(payload);
    let nl = r.u16("JPEG: truncated DNL")?;
    if r.remaining() != 0 {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "JPEG: DNL length must be 4",
        ));
    }
    if nl == 0 {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "JPEG: DNL number-of-lines is 0",
        ));
    }
    Ok(nl)
}

/// Parses a SOF0/SOF1 frame header (§B.2.2, Table B.2). SOF0 and SOF1 are decoded identically at
/// 8-bit, so the caller need not distinguish them here.
///
/// # Errors
///
/// An [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) error for 12-bit precision or
/// more than four components; an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput)
/// error for a bad precision, a zero-width or zero-component frame, an out-of-range sampling factor or
/// table destination, a duplicate component id, or a length mismatch.
pub fn parse_sof(payload: &[u8]) -> Result<Frame> {
    let mut r = Reader::new(payload);
    match r.u8("JPEG: truncated SOF")? {
        8 => {}
        12 => {
            return Err(Error::unsupported(
                env!("CARGO_PKG_NAME"),
                "JPEG: 12-bit precision not supported",
            ));
        }
        _ => {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "JPEG: invalid SOF precision",
            ));
        }
    }
    let y = r.u16("JPEG: truncated SOF")?;
    let x = r.u16("JPEG: truncated SOF")?;
    if x == 0 {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "JPEG: SOF samples-per-line X is 0",
        ));
    }
    let nf = r.u8("JPEG: truncated SOF")?;
    if nf == 0 {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "JPEG: SOF has zero components",
        ));
    }
    if nf > 4 {
        return Err(Error::unsupported(
            env!("CARGO_PKG_NAME"),
            "JPEG: more than 4 components not supported",
        ));
    }
    let mut components = Vec::with_capacity(usize::from(nf));
    for _ in 0..nf {
        let id = r.u8("JPEG: truncated SOF component")?;
        let hv = r.u8("JPEG: truncated SOF component")?;
        let tq = r.u8("JPEG: truncated SOF component")?;
        let h = hv >> 4;
        let v = hv & 0x0F;
        if !(1..=4).contains(&h) || !(1..=4).contains(&v) {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "JPEG: SOF sampling factor out of 1..=4",
            ));
        }
        if usize::from(tq) >= MAX_TABLES {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "JPEG: SOF Tq > 3",
            ));
        }
        if components.iter().any(|c: &FrameComponent| c.id == id) {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "JPEG: duplicate SOF component id",
            ));
        }
        components.push(FrameComponent { id, h, v, tq });
    }
    if r.remaining() != 0 {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "JPEG: SOF length mismatch",
        ));
    }
    Ok(Frame { y, x, components })
}

/// Parses a SOS scan header (§B.2.3, Table B.3) against `frame`, resolving each `Csj` to a frame
/// component index and validating the spectral-selection fields for the frame's process. When
/// `progressive` is false (sequential SOF0/SOF1) the fields are pinned to `Ss=0`, `Se=63`,
/// `Ah=Al=0`; when true (SOF2) the progressive band/precision constraints of §B.2.3 and §G.1.1 are
/// enforced instead.
///
/// # Errors
///
/// An [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error for a bad component
/// count, an unknown or duplicated component selector,
/// an out-of-range table destination, an interleaved sampling sum `> 10`, a length mismatch, or a
/// spectral-selection field outside the range Table B.3 allows for the process (including a
/// multi-component AC scan or a mismatched `Ah`/`Al` in the progressive case).
pub fn parse_sos(payload: &[u8], frame: &Frame, progressive: bool) -> Result<ScanHeader> {
    let mut r = Reader::new(payload);
    let ns = r.u8("JPEG: truncated SOS")?;
    if ns == 0 || ns > 4 {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "JPEG: SOS component count out of 1..=4",
        ));
    }
    let mut components = Vec::with_capacity(usize::from(ns));
    for _ in 0..ns {
        let cs = r.u8("JPEG: truncated SOS component")?;
        let td_ta = r.u8("JPEG: truncated SOS component")?;
        let td = td_ta >> 4;
        let ta = td_ta & 0x0F;
        if usize::from(td) >= MAX_TABLES || usize::from(ta) >= MAX_TABLES {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "JPEG: SOS table destination > 3",
            ));
        }
        let frame_index = frame
            .components
            .iter()
            .position(|c| c.id == cs)
            .ok_or_else(|| {
                Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "JPEG: SOS references unknown component",
                )
            })?;
        if components
            .iter()
            .any(|c: &ScanComponent| c.frame_index == frame_index)
        {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "JPEG: duplicate SOS component",
            ));
        }
        components.push(ScanComponent {
            frame_index,
            td,
            ta,
        });
    }
    let ss = r.u8("JPEG: truncated SOS")?;
    let se = r.u8("JPEG: truncated SOS")?;
    let ah_al = r.u8("JPEG: truncated SOS")?;
    let ah = ah_al >> 4;
    let al = ah_al & 0x0F;
    if r.remaining() != 0 {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "JPEG: SOS length mismatch",
        ));
    }
    if progressive {
        validate_progressive_spectral(ss, se, ah, al, components.len())?;
    } else if ss != 0 || se != 63 || ah_al != 0 {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "JPEG: non-baseline spectral selection (progressive scan)",
        ));
    }
    // Interleaved sum(Hi·Vi) ≤ 10 (§A.2.2). A single-component scan is exempt.
    if components.len() > 1 {
        let sum: u32 = components
            .iter()
            .map(|c| {
                let fc = &frame.components[c.frame_index];
                u32::from(fc.h) * u32::from(fc.v)
            })
            .sum();
        if sum > 10 {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "JPEG: interleaved sampling sum(Hi·Vi) > 10",
            ));
        }
    }
    Ok(ScanHeader {
        components,
        ss,
        se,
        ah,
        al,
    })
}

/// Validates the spectral-selection / successive-approximation fields of a progressive scan header
/// (§B.2.3, Table B.3, and §G.1.1.1). Mirrors the reference decoder's checks:
///
/// - a DC scan (`Ss = 0`) must set `Se = 0`;
/// - an AC scan (`Ss ≥ 1`) must have `Ss ≤ Se ≤ 63` and be single-component (`Ns = 1`);
/// - a refinement scan (`Ah ≠ 0`) must have `Al = Ah − 1`;
/// - `Al ≤ 13`.
fn validate_progressive_spectral(ss: u8, se: u8, ah: u8, al: u8, ns: usize) -> Result<()> {
    if ss == 0 {
        if se != 0 {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "JPEG: progressive DC scan must set Se = 0",
            ));
        }
    } else {
        if ss > se || se > 63 {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "JPEG: progressive AC band out of Ss ≤ Se ≤ 63",
            ));
        }
        if ns != 1 {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "JPEG: progressive AC scan must be single-component",
            ));
        }
    }
    if ah != 0 && al != ah - 1 {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "JPEG: progressive refinement requires Al = Ah − 1",
        ));
    }
    if al > 13 {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "JPEG: progressive Al > 13",
        ));
    }
    Ok(())
}

/// Reads the JFIF marker from an APP0 payload, setting [`ColorInfo::jfif`] when the `"JFIF\0"`
/// identifier is present (T.871 §10.1). Malformed/short APP0 payloads are advisory and silently
/// ignored (never an error).
pub fn parse_app0(payload: &[u8], color: &mut ColorInfo) {
    if payload.len() >= 5 && &payload[..5] == b"JFIF\0" {
        color.jfif = true;
    }
}

/// Reads the Adobe transform byte from an APP14 payload, setting [`ColorInfo::adobe_transform`] when
/// the `"Adobe"` identifier and a transform byte are present (TN #5116: identifier + version(2) +
/// flags0(2) + flags1(2) + transform(1) = 12 bytes). Malformed/short payloads are advisory and
/// silently ignored.
pub fn parse_app14(payload: &[u8], color: &mut ColorInfo) {
    // "Adobe" (5) + version(2) + flags0(2) + flags1(2) + transform(1) = 12 bytes.
    if payload.len() >= 12 && &payload[..5] == b"Adobe" {
        color.adobe_transform = Some(payload[11]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame_3comp() -> Frame {
        Frame {
            y: 16,
            x: 16,
            components: vec![
                FrameComponent {
                    id: 1,
                    h: 2,
                    v: 2,
                    tq: 0,
                },
                FrameComponent {
                    id: 2,
                    h: 1,
                    v: 1,
                    tq: 1,
                },
                FrameComponent {
                    id: 3,
                    h: 1,
                    v: 1,
                    tq: 1,
                },
            ],
        }
    }

    #[test]
    fn dqt_8bit_reads_zigzag_into_natural_order() {
        use crate::zigzag::ZIGZAG;
        // Pq=0, Tq=0, then 64 zig-zag values 1..=64 (all non-zero). Stored natural[ZIGZAG[k]] = k+1.
        let mut payload = vec![0x00];
        payload.extend(1..=64u8);
        let mut tables = Tables::default();
        parse_dqt(&payload, &mut tables).unwrap();
        let t = tables.quant[0].unwrap();
        for (k, &nat) in ZIGZAG.iter().enumerate() {
            assert_eq!(t[nat], (k + 1) as u16);
        }
    }

    #[test]
    fn dqt_rejects_zero_value_and_bad_precision() {
        let mut zero = vec![0x00];
        zero.extend(std::iter::repeat_n(0u8, 64)); // all-zero table
        assert!(parse_dqt(&zero, &mut Tables::default()).is_err());
        // Pq=2 (>1) is invalid.
        let bad_pq = vec![0x20];
        assert!(parse_dqt(&bad_pq, &mut Tables::default()).is_err());
        // Tq=4 (>3) is invalid.
        let mut bad_tq = vec![0x04];
        bad_tq.extend(std::iter::repeat_n(1u8, 64));
        assert!(parse_dqt(&bad_tq, &mut Tables::default()).is_err());
    }

    #[test]
    fn dqt_16bit_two_bytes_per_value() {
        use crate::zigzag::ZIGZAG;
        // Pq=1, Tq=1; value k written big-endian as 0x0100+? use 256+k so both bytes exercised.
        let mut payload = vec![0x11];
        for k in 0..64u16 {
            payload.extend_from_slice(&(256 + k).to_be_bytes());
        }
        let mut tables = Tables::default();
        parse_dqt(&payload, &mut tables).unwrap();
        let t = tables.quant[1].unwrap();
        assert_eq!(t[ZIGZAG[0]], 256);
        assert_eq!(t[ZIGZAG[63]], 256 + 63);
    }

    #[test]
    fn sos_resolves_components_and_rejects_progressive_fields() {
        let frame = frame_3comp();
        // Ns=3, (1,0x00)(2,0x11)(3,0x11), Ss=0 Se=63 Ah|Al=0.
        let payload = vec![3, 1, 0x00, 2, 0x11, 3, 0x11, 0, 63, 0];
        let scan = parse_sos(&payload, &frame, false).unwrap();
        assert_eq!(scan.components.len(), 3);
        assert_eq!(scan.components[0].frame_index, 0);
        assert_eq!(scan.components[1].td, 1);
        assert!(scan.interleaved());
        assert_eq!((scan.ss, scan.se, scan.ah, scan.al), (0, 63, 0, 0));
        // Each of Ss, Se, Ah/Al out of its baseline value is independently rejected (pinning all
        // three `||` terms of the spectral-selection check): Ss=1, Se=62, Ah|Al=0x10.
        assert!(parse_sos(&[1, 1, 0x00, 1, 63, 0], &frame, false).is_err());
        assert!(parse_sos(&[1, 1, 0x00, 0, 62, 0], &frame, false).is_err());
        assert!(parse_sos(&[1, 1, 0x00, 0, 63, 0x10], &frame, false).is_err());
        // Unknown component selector 9.
        let unknown = vec![1, 9, 0x00, 0, 63, 0];
        assert!(parse_sos(&unknown, &frame, false).is_err());
        // A DC table selector out of range (Td=4) is rejected even when Ta is valid (pinning both
        // sides of the `Td>=4 || Ta>=4` check).
        assert!(parse_sos(&[1, 1, 0x40, 0, 63, 0], &frame, false).is_err());
        assert!(parse_sos(&[1, 1, 0x04, 0, 63, 0], &frame, false).is_err());
    }

    #[test]
    fn sos_progressive_spectral_validation() {
        let frame = frame_3comp();
        // A progressive DC first scan: interleaved, Ss=0 Se=0 Ah=0 Al=1 (point transform).
        let dc = parse_sos(&[3, 1, 0x00, 2, 0x11, 3, 0x11, 0, 0, 0x01], &frame, true).unwrap();
        assert_eq!((dc.ss, dc.se, dc.ah, dc.al), (0, 0, 0, 1));
        // A single-component AC band Ss=1 Se=5, first pass Al=2.
        let ac = parse_sos(&[1, 1, 0x00, 1, 5, 0x02], &frame, true).unwrap();
        assert_eq!((ac.ss, ac.se, ac.ah, ac.al), (1, 5, 0, 2));
        // An AC refinement Ah=2 Al=1 (Al = Ah − 1) is accepted.
        assert!(parse_sos(&[1, 1, 0x00, 1, 5, 0x21], &frame, true).is_ok());
        // A single-coefficient AC band Ss = Se is legal — pins `ss > se` at its boundary (a `>=` or
        // `==` mutant would reject the equal case).
        assert!(parse_sos(&[1, 1, 0x00, 3, 3, 0x00], &frame, true).is_ok());
        // Se > 63 is rejected.
        assert!(parse_sos(&[1, 1, 0x00, 1, 64, 0x00], &frame, true).is_err());
        // A DC scan with Se ≠ 0 is rejected (footnote c: Se = 0 when Ss = 0).
        assert!(parse_sos(&[1, 1, 0x00, 0, 63, 0], &frame, true).is_err());
        // An AC scan with Se < Ss is rejected.
        assert!(parse_sos(&[1, 1, 0x00, 5, 1, 0], &frame, true).is_err());
        // An AC scan with more than one component is rejected (§B.2.3/G.1.1.1.1).
        assert!(parse_sos(&[2, 1, 0x00, 2, 0x00, 1, 5, 0], &frame, true).is_err());
        // A refinement whose Ah ≠ Al + 1 is rejected: Ah=2, Al=0.
        assert!(parse_sos(&[1, 1, 0x00, 1, 5, 0x20], &frame, true).is_err());
        // Al = 14 (> 13) is rejected.
        assert!(parse_sos(&[1, 1, 0x00, 1, 5, 0x0E], &frame, true).is_err());
        // Al = 13 is the accepted boundary.
        assert!(parse_sos(&[1, 1, 0x00, 1, 5, 0x0D], &frame, true).is_ok());
    }

    #[test]
    fn sos_rejects_interleaved_sampling_sum_over_10() {
        // Two components at Hi=3, Vi=2 → sum(Hi·Vi) = 6 + 6 = 12 > 10. Chosen so the products (6+6)
        // exceed 10 while the *sums* (5+5) do not — pinning that the check multiplies Hi·Vi rather
        // than adding them.
        let frame = Frame {
            components: vec![
                FrameComponent {
                    id: 1,
                    h: 3,
                    v: 2,
                    tq: 0,
                },
                FrameComponent {
                    id: 2,
                    h: 3,
                    v: 2,
                    tq: 0,
                },
            ],
            ..frame_3comp()
        };
        let payload = vec![2, 1, 0x00, 2, 0x00, 0, 63, 0];
        assert!(parse_sos(&payload, &frame, false).is_err());
    }

    #[test]
    fn sos_interleaved_sampling_sum_of_exactly_10_is_legal() {
        // (2,2) + (2,2) + (1,1) + (1,1) → 4+4+1+1 = 10, the §B.2.3 limit itself: must parse (the
        // constraint is `> 10`, not `≥ 10`).
        let frame = Frame {
            y: 16,
            x: 16,
            components: vec![
                FrameComponent {
                    id: 1,
                    h: 2,
                    v: 2,
                    tq: 0,
                },
                FrameComponent {
                    id: 2,
                    h: 2,
                    v: 2,
                    tq: 0,
                },
                FrameComponent {
                    id: 3,
                    h: 1,
                    v: 1,
                    tq: 0,
                },
                FrameComponent {
                    id: 4,
                    h: 1,
                    v: 1,
                    tq: 0,
                },
            ],
        };
        let payload = vec![4, 1, 0x00, 2, 0x00, 3, 0x00, 4, 0x00, 0, 63, 0];
        let scan = parse_sos(&payload, &frame, false).unwrap();
        assert_eq!(scan.components.len(), 4);
    }

    #[test]
    fn sos_single_component_scan_is_exempt_from_the_sampling_sum() {
        // One component at 4×4 (Hi·Vi = 16 > 10) is legal in a NON-interleaved scan: the §B.2.3
        // sum constraint applies only to interleaved (Ns > 1) scans (§A.2.2).
        let frame = Frame {
            y: 32,
            x: 32,
            components: vec![FrameComponent {
                id: 1,
                h: 4,
                v: 4,
                tq: 0,
            }],
        };
        let scan = parse_sos(&[1, 1, 0x00, 0, 63, 0], &frame, false).unwrap();
        assert_eq!(scan.components.len(), 1);
        assert!(!scan.interleaved());
    }

    #[test]
    fn sof_rejects_12bit_and_duplicate_ids() {
        // P=12 → Unsupported.
        let p12 = vec![12, 0, 8, 0, 8, 1, 1, 0x11, 0];
        assert!(matches!(
            parse_sof(&p12),
            Err(error) if error.kind() == gamut_core::ErrorKind::Unsupported
        ));
        // Two components sharing id 1.
        let dup = vec![8, 0, 8, 0, 8, 2, 1, 0x11, 0, 1, 0x11, 0];
        assert!(parse_sof(&dup).is_err());
        // Nf=0.
        let zero = vec![8, 0, 8, 0, 8, 0];
        assert!(parse_sof(&zero).is_err());
        // Hi=0 (sampling factor out of range).
        let h0 = vec![8, 0, 8, 0, 8, 1, 1, 0x01, 0];
        assert!(parse_sof(&h0).is_err());
    }

    #[test]
    fn sof_parses_dimensions_and_sampling() {
        let payload = vec![8, 0x01, 0x00, 0x00, 0x40, 1, 1, 0x22, 0];
        let frame = parse_sof(&payload).unwrap();
        assert_eq!((frame.x, frame.y), (64, 256));
        assert_eq!(frame.components[0].h, 2);
        assert_eq!(frame.components[0].v, 2);
        assert_eq!((frame.hmax(), frame.vmax()), (2, 2));
    }

    #[test]
    fn app0_and_app14_are_advisory() {
        let mut c = ColorInfo::default();
        parse_app0(b"JFIF\0\x01\x02", &mut c);
        assert!(c.jfif);
        // Short/garbage APP0 is ignored, not an error.
        let mut c2 = ColorInfo::default();
        parse_app0(b"JF", &mut c2);
        assert!(!c2.jfif);
        // Adobe APP14 transform byte at offset 11 ("Adobe" + version(2) + flags0(2) + flags1(2) + t).
        let mut c3 = ColorInfo::default();
        parse_app14(b"Adobe\x00\x64\x00\x00\x00\x00\x02", &mut c3);
        assert_eq!(c3.adobe_transform, Some(2));
        // A short Adobe payload (no transform byte) is ignored, not an error.
        let mut c4 = ColorInfo::default();
        parse_app14(b"Adobe\x00\x64", &mut c4);
        assert_eq!(c4.adobe_transform, None);
    }
}
