//! The AV1 OBU layer: unit-type classification ([`ObuType`]), the OBU header ([`ObuHeader`]), and
//! the low-overhead item-payload split ([`iter_obus`]). Crate-private: `gamut-av1` exports the
//! public OBU walk ([`gamut_av1::ObuIter`]); this copy stays only until that walk reports what
//! [`Av1Config`](crate::Av1Config) needs from it — each OBU's size-field flag and raw bytes.
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
pub(crate) enum ObuType {
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
    pub(crate) fn from_raw(value: u8) -> Self {
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

    /// Whether this OBU carries (part of) a coded frame — `OBU_FRAME_HEADER`, `OBU_TILE_GROUP`, or
    /// `OBU_FRAME`. The AVIF sync-sample constraint is stated relative to the first such OBU
    /// (AV1-ISOBMFF §2.4: the sequence header must precede the first frame header).
    #[must_use]
    pub(crate) fn is_frame_bearing(self) -> bool {
        matches!(self, Self::FrameHeader | Self::TileGroup | Self::Frame)
    }
}

/// The one- or two-byte AV1 OBU header (AV1 §5.3.2/§5.3.3).
///
/// `obu_forbidden_bit` (which must be 0) is validated by [`parse_header`] and not retained; `obu_reserved_1bit` and the extension header's reserved bits are ignored on read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct ObuHeader {
    /// The `obu_type` (four bits), classified.
    pub(crate) obu_type: ObuType,
    /// `obu_has_size_field`: whether a `leb128()` payload size follows the header. In an item
    /// payload every OBU has it set except (optionally) the last, which then fills the remainder
    /// (AV1-ISOBMFF §2.4).
    pub(crate) has_size_field: bool,
    /// `temporal_id` from the extension header (three bits) — 0 when no extension header is
    /// present (a still image is a single temporal layer).
    pub(crate) temporal_id: u8,
    /// `spatial_id` from the extension header (two bits) — 0 when no extension header is present.
    pub(crate) spatial_id: u8,
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
pub(crate) struct Obu<'a> {
    /// The parsed OBU header.
    pub(crate) header: ObuHeader,
    /// The full OBU bytes — header, size field (if any), and payload.
    pub(crate) raw: &'a [u8],
    /// The OBU payload (the bytes after the header and size field).
    pub(crate) payload: &'a [u8],
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
pub(crate) struct ObuIter<'a> {
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
pub(crate) fn iter_obus(payload: &[u8]) -> ObuIter<'_> {
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

#[cfg(test)]
mod tests {
    use gamut_core::{Error, ErrorKind};

    use super::*;

    /// Appends the minimal `leb128()` encoding of `value`.
    fn leb128(mut value: usize, out: &mut Vec<u8>) {
        loop {
            let byte = (value & 0x7f) as u8;
            value >>= 7;
            if value == 0 {
                out.push(byte);
                break;
            }
            out.push(byte | 0x80);
        }
    }

    /// A size-fielded OBU of the given type (no extension header).
    fn obu(ty: u8, payload: &[u8]) -> Vec<u8> {
        let mut v = vec![(ty << 3) | 0x02];
        leb128(payload.len(), &mut v);
        v.extend_from_slice(payload);
        v
    }

    /// A sequence-header OBU whose first payload byte carries
    /// `seq_profile(3) | still_picture(1) | reduced_still_picture_header(1)`.
    fn seq_obu(reduced: bool) -> Vec<u8> {
        let b0 = if reduced { 0x18 } else { 0x00 };
        obu(1, &[b0, 0xAA])
    }

    /// A frame OBU whose first payload byte carries
    /// `show_existing_frame(1) | frame_type(2) | show_frame(1)`.
    fn frame_obu(first_byte: u8) -> Vec<u8> {
        obu(6, &[first_byte, 0xBB, 0xCC])
    }

    fn concat(parts: &[Vec<u8>]) -> Vec<u8> {
        parts.iter().flatten().copied().collect()
    }

    #[track_caller]
    fn assert_invalid<T: core::fmt::Debug>(result: Result<T>, message: &str) -> Error {
        match result {
            Err(error) if error.kind() == ErrorKind::InvalidInput => {
                assert_eq!(error.static_message(), Some(message));
                error
            }
            other => panic!("expected InvalidInput({message:?}), got {other:?}"),
        }
    }

    // ---- OBU split -------------------------------------------------------------------------------

    #[test]
    fn obu_split_is_byte_exact() {
        let td = obu(2, &[]);
        let seq = seq_obu(true);
        let frame = frame_obu(0x10);
        let stream = concat(&[td.clone(), seq.clone(), frame.clone()]);
        let obus: Vec<Obu<'_>> = iter_obus(&stream).collect::<Result<_>>().unwrap();
        assert_eq!(obus.len(), 3);
        assert_eq!(obus[0].header.obu_type, ObuType::TemporalDelimiter);
        assert_eq!(obus[0].raw, &td[..]);
        assert!(obus[0].payload.is_empty());
        assert_eq!(obus[1].header.obu_type, ObuType::SequenceHeader);
        assert_eq!(obus[1].raw, &seq[..]);
        assert_eq!(obus[1].payload, &seq[2..]);
        assert_eq!(obus[2].header.obu_type, ObuType::Frame);
        assert_eq!(obus[2].raw, &frame[..]);
        assert_eq!(obus[2].payload, &frame[2..]);
        // The split consumes the stream exactly; an empty payload yields zero OBUs without error.
        assert_eq!(iter_obus(&[]).count(), 0);
    }

    #[test]
    fn obu_type_classifies_every_raw_value() {
        let named = [
            (1, ObuType::SequenceHeader),
            (2, ObuType::TemporalDelimiter),
            (3, ObuType::FrameHeader),
            (4, ObuType::TileGroup),
            (5, ObuType::Metadata),
            (6, ObuType::Frame),
            (7, ObuType::RedundantFrameHeader),
            (8, ObuType::TileList),
            (15, ObuType::Padding),
        ];
        for raw in 0..=15u8 {
            let expected = named
                .iter()
                .find(|(value, _)| *value == raw)
                .map_or(ObuType::Other(raw), |&(_, ty)| ty);
            assert_eq!(ObuType::from_raw(raw), expected, "obu_type {raw}");
        }
        assert!(ObuType::Frame.is_frame_bearing());
        assert!(ObuType::FrameHeader.is_frame_bearing());
        assert!(ObuType::TileGroup.is_frame_bearing());
        assert!(!ObuType::SequenceHeader.is_frame_bearing());
        assert!(!ObuType::Metadata.is_frame_bearing());
    }

    #[test]
    fn obu_header_reads_the_extension_fields() {
        // obu_type 6, extension flag, size field; extension byte: temporal_id 5, spatial_id 2.
        let bytes = [(6 << 3) | 0x04 | 0x02, (5 << 5) | (2 << 3), 0x00];
        let header = parse_header(&bytes).unwrap().0;
        assert_eq!(header.obu_type, ObuType::Frame);
        assert!(header.has_size_field);
        assert_eq!(header.temporal_id, 5);
        assert_eq!(header.spatial_id, 2);
        // Without the extension flag both ids read 0.
        let plain = parse_header(&[(6 << 3) | 0x02, 0x00]).unwrap().0;
        assert_eq!((plain.temporal_id, plain.spatial_id), (0, 0));
        // The whole OBU (header + extension + size + payload) splits, too.
        let obus: Vec<Obu<'_>> = iter_obus(&bytes).collect::<Result<_>>().unwrap();
        assert_eq!(obus.len(), 1);
        assert!(obus[0].payload.is_empty());
    }

    #[test]
    fn obu_header_rejects_the_forbidden_bit_and_truncation() {
        assert_invalid(parse_header(&[0x80]), "AVIF: OBU forbidden bit set");
        assert_invalid(parse_header(&[]), "AVIF: truncated OBU header");
        // Extension flag set but no extension byte.
        assert_invalid(
            parse_header(&[(6 << 3) | 0x04]),
            "AVIF: truncated OBU extension header",
        );
        // The split reports the same refusals, at the offset they occur.
        assert_invalid(
            iter_obus(&[0x80]).next().unwrap(),
            "AVIF: OBU forbidden bit set",
        );
    }

    #[test]
    fn sizeless_last_obu_fills_the_remainder() {
        // A frame OBU without a size field consumes the rest of the payload.
        let mut stream = seq_obu(true);
        stream.push(6 << 3); // header: type 6, no extension, no size field
        stream.extend_from_slice(&[0x10, 0xBB, 0xCC]);
        let obus: Vec<Obu<'_>> = iter_obus(&stream).collect::<Result<_>>().unwrap();
        assert_eq!(obus.len(), 2);
        assert!(!obus[1].header.has_size_field);
        assert_eq!(obus[1].payload, &[0x10, 0xBB, 0xCC]);
    }

    #[test]
    fn leb128_accepts_padding_and_enforces_the_conformance_bounds() {
        // A padded (non-minimal) two-byte encoding of 3: reference encoders emit these.
        let stream = [(5 << 3) | 0x02, 0x83, 0x00, 0xAA, 0xBB, 0xCC];
        let obus: Vec<Obu<'_>> = iter_obus(&stream).collect::<Result<_>>().unwrap();
        assert_eq!(obus[0].payload, &[0xAA, 0xBB, 0xCC]);
        // Nine continuation bytes: longer than the 8-byte bound.
        let long = [
            (5 << 3) | 0x02,
            0x80,
            0x80,
            0x80,
            0x80,
            0x80,
            0x80,
            0x80,
            0x80,
        ];
        assert_invalid(
            iter_obus(&long).next().unwrap(),
            "AVIF: OBU size field longer than 8 bytes",
        );
        // 1 << 32 does not fit the 32-bit obu_size bound.
        let huge = [(5 << 3) | 0x02, 0x80, 0x80, 0x80, 0x80, 0x10];
        assert_invalid(
            iter_obus(&huge).next().unwrap(),
            "AVIF: OBU size exceeds 32 bits",
        );
        // u32::MAX itself is legal. With no corresponding payload it must reach the payload bounds
        // check, rather than being rejected by the size-field bound.
        let max = [(5 << 3) | 0x02, 0xff, 0xff, 0xff, 0xff, 0x0f];
        assert_invalid(
            iter_obus(&max).next().unwrap(),
            "AVIF: truncated OBU payload",
        );
        // A truncated size field (continuation bit on the final byte).
        let mut cut = obu(2, &[0]);
        cut.extend_from_slice(&[(5 << 3) | 0x02, 0x80]);
        let error = assert_invalid(
            iter_obus(&cut).nth(1).unwrap(),
            "AVIF: truncated OBU size field",
        );
        assert_eq!(error.byte_offset(), Some(4));
    }

    #[test]
    fn obu_split_rejects_a_truncated_body() {
        let mut stream = obu(2, &[0]);
        // Size field claims 4 bytes; only 2 follow.
        stream.extend_from_slice(&[(5 << 3) | 0x02, 0x04, 0xAA, 0xBB]);
        let error = assert_invalid(
            iter_obus(&stream).nth(1).unwrap(),
            "AVIF: truncated OBU payload",
        );
        assert_eq!(error.byte_offset(), Some(5));
    }
}
