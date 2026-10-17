//! The AV1 OBU layer: unit-type classification ([`ObuType`]), the OBU header ([`ObuHeader`]), and
//! the low-overhead item-payload split ([`iter_obus`]).
//!
//! This is *container scope* (issue #250): the reader classifies each OBU (sequence header vs
//! frame vs metadata) and splits an `av01` item payload into OBUs, but never interprets an OBU
//! payload beyond the fixed leading bits
//! [`Av1Config::validate_still_payload`](crate::Av1Config::validate_still_payload) peeks — AV1
//! bitstream decoding is codec scope. All
//! layouts are from the AV1 specification §5.3 (the open_bitstream_unit low-overhead syntax) and
//! §4.10.5 (`leb128()`); the item-payload framing rules are AV1-ISOBMFF v1.3.0 §2.4 (see
//! `references/av1`).

use gamut_core::{Error, Result};

/// An AV1 OBU type (AV1 §5.3.2), as the four-bit `obu_type` field.
///
/// The variants name every type the AVIF container layer must classify; any other value (0 and the
/// reserved 9..=14) round-trips through [`Other`](Self::Other), so [`from_raw`](Self::from_raw)
/// followed by [`raw`](Self::raw) is the identity for all `0..=15`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ObuType {
    /// `OBU_SEQUENCE_HEADER` (1) — the sequence header an AV1 image item carries exactly once
    /// (AVIF v1.2.0 §2.1).
    SequenceHeader,
    /// `OBU_TEMPORAL_DELIMITER` (2) — a temporal-unit boundary (empty payload).
    TemporalDelimiter,
    /// `OBU_FRAME_HEADER` (3) — a frame header without tile data.
    FrameHeader,
    /// `OBU_TILE_GROUP` (4) — coded tile data for the preceding frame header.
    TileGroup,
    /// `OBU_METADATA` (5) — metadata (HDR, timecode, …).
    Metadata,
    /// `OBU_FRAME` (6) — a frame header and its tile data in one OBU.
    Frame,
    /// `OBU_REDUNDANT_FRAME_HEADER` (7) — a repeated frame header.
    RedundantFrameHeader,
    /// `OBU_TILE_LIST` (8) — large-scale-tile data; forbidden in an item payload
    /// (AV1-ISOBMFF §2.4).
    TileList,
    /// `OBU_PADDING` (15) — padding bytes.
    Padding,
    /// Any other `obu_type` value (0 or the reserved 9..=14), preserved verbatim (the raw four-bit
    /// value).
    Other(u8),
}

impl ObuType {
    /// Classifies a raw four-bit `obu_type` value (`0..=15`).
    ///
    /// Values outside the named set map to [`Other`](Self::Other), so this never fails.
    #[must_use]
    pub fn from_raw(value: u8) -> Self {
        match value {
            1 => Self::SequenceHeader,
            2 => Self::TemporalDelimiter,
            3 => Self::FrameHeader,
            4 => Self::TileGroup,
            5 => Self::Metadata,
            6 => Self::Frame,
            7 => Self::RedundantFrameHeader,
            8 => Self::TileList,
            15 => Self::Padding,
            other => Self::Other(other),
        }
    }

    /// The raw four-bit `obu_type` value.
    #[must_use]
    pub fn raw(self) -> u8 {
        match self {
            Self::SequenceHeader => 1,
            Self::TemporalDelimiter => 2,
            Self::FrameHeader => 3,
            Self::TileGroup => 4,
            Self::Metadata => 5,
            Self::Frame => 6,
            Self::RedundantFrameHeader => 7,
            Self::TileList => 8,
            Self::Padding => 15,
            Self::Other(other) => other,
        }
    }

    /// Whether this OBU carries (part of) a coded frame — `OBU_FRAME_HEADER`, `OBU_TILE_GROUP`, or
    /// `OBU_FRAME`. The AVIF sync-sample constraint is stated relative to the first such OBU
    /// (AV1-ISOBMFF §2.4: the sequence header must precede the first frame header).
    #[must_use]
    pub fn is_frame_bearing(self) -> bool {
        matches!(self, Self::FrameHeader | Self::TileGroup | Self::Frame)
    }
}

/// The one- or two-byte AV1 OBU header (AV1 §5.3.2/§5.3.3).
///
/// `obu_forbidden_bit` (which must be 0) is validated at [`parse`](Self::parse) time and not
/// retained; `obu_reserved_1bit` and the extension header's reserved bits are ignored on read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ObuHeader {
    /// The `obu_type` (four bits), classified.
    pub obu_type: ObuType,
    /// `obu_has_size_field`: whether a `leb128()` payload size follows the header. In an item
    /// payload every OBU has it set except (optionally) the last, which then fills the remainder
    /// (AV1-ISOBMFF §2.4).
    pub has_size_field: bool,
    /// `temporal_id` from the extension header (three bits) — 0 when no extension header is
    /// present (a still image is a single temporal layer).
    pub temporal_id: u8,
    /// `spatial_id` from the extension header (two bits) — 0 when no extension header is present.
    pub spatial_id: u8,
}

impl ObuHeader {
    /// Parses the OBU header at the start of `obu`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidInput`] if `obu` is too short (one byte, or two with
    /// `obu_extension_flag` set) or if `obu_forbidden_bit` is set (a conforming AV1 OBU header
    /// always has it 0).
    pub fn parse(obu: &[u8]) -> Result<Self> {
        parse_header(obu).map(|(header, _)| header)
    }
}

/// Parses the OBU header at the start of `data`, also returning its length in bytes (1, or 2 with
/// an extension header).
fn parse_header(data: &[u8]) -> Result<(ObuHeader, usize)> {
    let &[b0, ..] = data else {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "AVIF: truncated OBU header",
        ));
    };
    // obu_forbidden_bit(1) | obu_type(4) | obu_extension_flag(1) | obu_has_size_field(1) |
    // obu_reserved_1bit(1)
    if b0 & 0x80 != 0 {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "AVIF: OBU forbidden bit set",
        ));
    }
    let obu_type = ObuType::from_raw((b0 >> 3) & 0x0f);
    let has_size_field = b0 & 0x02 != 0;
    let (temporal_id, spatial_id, len) = if b0 & 0x04 != 0 {
        // temporal_id(3) | spatial_id(2) | extension_header_reserved_3bits(3)
        let Some(&b1) = data.get(1) else {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "AVIF: truncated OBU extension header",
            ));
        };
        (b1 >> 5, (b1 >> 3) & 0x03, 2)
    } else {
        (0, 0, 1)
    };
    Ok((
        ObuHeader {
            obu_type,
            has_size_field,
            temporal_id,
            spatial_id,
        },
        len,
    ))
}

/// One OBU of a low-overhead (Section 5) stream, as split by [`iter_obus`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Obu<'a> {
    /// The parsed OBU header.
    pub header: ObuHeader,
    /// The full OBU bytes — header, size field (if any), and payload.
    pub raw: &'a [u8],
    /// The OBU payload (the bytes after the header and size field).
    pub payload: &'a [u8],
}

/// A fallible iterator over the OBUs of a low-overhead `av01` item payload (AV1 §5.3.1;
/// AV1-ISOBMFF §2.4). Created by [`iter_obus`].
///
/// Each item is a borrowed [`Obu`] (`Ok`) or the first error encountered (`Err`), after which the
/// iterator is exhausted. It yields OBUs until the payload is consumed **exactly**: a clean end
/// stops iteration with no error; a truncated header, size field, or body is reported as an error
/// (the every-byte principle). An OBU with `obu_has_size_field = 0` fills the remainder of the
/// payload and is therefore structurally last (AV1-ISOBMFF §2.4 permits that only for the final
/// OBU).
#[derive(Debug)]
pub struct ObuIter<'a> {
    /// The payload not yet split. Each OBU is cut off its front, so iteration ends because this
    /// runs out, never on the arithmetic of `pos` (#890).
    rest: &'a [u8],
    /// `rest`'s offset into the payload; it only places errors.
    pos: usize,
    done: bool,
}

impl<'a> Iterator for ObuIter<'a> {
    type Item = Result<Obu<'a>>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done || self.rest.is_empty() {
            return None;
        }
        // Any error below fuses the iterator: it is reported once, then `done` stops iteration.
        self.done = true;
        let rest = self.rest;
        let (header, header_len) = match parse_header(rest) {
            Ok(parsed) => parsed,
            Err(e) => return Some(Err(e.with_byte_offset(self.pos as u64))),
        };
        let (payload_start, payload_len) = if header.has_size_field {
            let (size, size_len) = match read_leb128(&rest[header_len..]) {
                Ok(read) => read,
                Err(e) => {
                    return Some(Err(e.with_byte_offset((self.pos + header_len) as u64)));
                }
            };
            (header_len + size_len, size as usize)
        } else {
            // No size field: the payload fills the remainder, making this OBU the last.
            (header_len, rest.len() - header_len)
        };
        let Some(end) = payload_start
            .checked_add(payload_len)
            .filter(|&e| e <= rest.len())
        else {
            return Some(Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "AVIF: truncated OBU payload",
            )
            .with_byte_offset((self.pos + payload_start) as u64)));
        };
        // The read succeeded: clear the fuse and advance past this OBU.
        self.done = false;
        let (raw, tail) = rest.split_at(end);
        self.rest = tail;
        self.pos += end;
        Some(Ok(Obu {
            header,
            raw,
            payload: &raw[payload_start..],
        }))
    }
}

/// Splits a low-overhead (Section 5) OBU stream — an `av01` item payload, or an `av1C`
/// `configOBUs` field — into its OBUs (AV1 §5.3.1; AV1-ISOBMFF §2.4), returning a fallible lazy
/// iterator.
///
/// Each OBU is `header [+ extension] [+ leb128 size] + payload`; an OBU without a size field fills
/// the remainder (legal only for the last OBU of an item payload — the split makes that structural,
/// since such an OBU always consumes the rest).
///
/// A lazy fallible iterator is the primary API (rather than a collecting `-> Result<Vec<Obu>>`):
/// it borrows without allocating and lets a caller stop early, and the collecting form is just
/// `iter_obus(..).collect::<Result<Vec<_>>>()`. An empty payload yields zero OBUs (a valid,
/// error-free empty iteration).
///
/// The iterator is bounds-checked and consumes the payload exactly: see [`ObuIter`] for the
/// truncation error behaviour. The `leb128()` size read follows AV1 §4.10.5: at most 8 bytes and a
/// value below 2³², with padded (non-minimal) encodings accepted — reference encoders emit them.
#[must_use]
pub fn iter_obus(payload: &[u8]) -> ObuIter<'_> {
    ObuIter {
        rest: payload,
        pos: 0,
        done: false,
    }
}

/// Reads an AV1 `leb128()` value (AV1 §4.10.5), returning `(value, bytes_read)`.
///
/// Enforces the conformance bounds — at most 8 bytes, value representable in 32 bits — but accepts
/// padded (non-minimal) encodings, which the spec permits and reference encoders emit for
/// rewritable size fields.
fn read_leb128(data: &[u8]) -> Result<(u64, usize)> {
    let mut value = 0u64;
    for i in 0..8 {
        let Some(&byte) = data.get(i) else {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "AVIF: truncated OBU size field",
            ));
        };
        value |= u64::from(byte & 0x7f) << (i * 7);
        if byte & 0x80 == 0 {
            if value > u64::from(u32::MAX) {
                return Err(Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "AVIF: OBU size exceeds 32 bits",
                ));
            }
            return Ok((value, i + 1));
        }
    }
    Err(Error::invalid_input(
        env!("CARGO_PKG_NAME"),
        "AVIF: OBU size field longer than 8 bytes",
    ))
}

/// Appends the minimal AV1 `leb128()` encoding of `value` to `out` (AV1 §4.10.5).
pub(crate) fn write_leb128(mut value: u64, out: &mut Vec<u8>) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            break;
        }
        // `byte + 0x80`, not `byte | 0x80`. `byte` is `value & 0x7f`, so bit 7 is known clear and
        // `|` and `^` cannot be told apart there -- an unkillable mutant (#110). The `&` mutation
        // of the same operator is a different matter and is caught by the multi-byte test below.
        out.push(byte + 0x80);
    }
}
