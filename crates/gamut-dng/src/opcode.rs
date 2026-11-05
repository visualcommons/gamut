//! The DNG opcode-list container (DNG 1.7.1 Chapter 7): typed parsing and serialization of the
//! `OpcodeList1`/`OpcodeList2`/`OpcodeList3` tags.
//!
//! An opcode list is a processing chain a reader applies to the raw image — `OpcodeList1` to the
//! stored data, `OpcodeList2` after linearization, `OpcodeList3` after demosaicing. The container
//! is **always big-endian**, regardless of the file's byte order: a `u32` opcode count, then per
//! opcode a `u32` ID, the four DNG-spec-version octets, a `u32` flags word, a `u32` parameter
//! byte count, and the parameter bytes.
//!
//! This module models the *container*: each [`Opcode`] carries its parameters as raw bytes.
//! Decoding those parameters into per-opcode structures — and *applying* the standard opcode
//! library (WarpRectilinear, GainMap, …) — is deferred (see `STATUS.md` P18); external raw
//! pipelines can execute the typed entries themselves in the meantime.

use gamut_core::{Error, Result};

/// The standard opcode IDs (DNG 1.7.1 Chapter 7, pp. 105–121). Values outside this set are
/// vendor-private or future opcodes; readers skip them when [`Opcode::is_optional`] is set.
pub mod opcode_id {
    /// `WarpRectilinear` — rectilinear lens-distortion (+ lateral chromatic aberration) warp.
    pub const WARP_RECTILINEAR: u32 = 1;
    /// `WarpFisheye` — fisheye-to-rectilinear warp.
    pub const WARP_FISHEYE: u32 = 2;
    /// `FixVignetteRadial` — radial vignette gain correction.
    pub const FIX_VIGNETTE_RADIAL: u32 = 3;
    /// `FixBadPixelsConstant` — interpolate pixels equal to a constant.
    pub const FIX_BAD_PIXELS_CONSTANT: u32 = 4;
    /// `FixBadPixelsList` — interpolate listed bad pixels/rectangles.
    pub const FIX_BAD_PIXELS_LIST: u32 = 5;
    /// `TrimBounds` — trim the image to a rectangle.
    pub const TRIM_BOUNDS: u32 = 6;
    /// `MapTable` — map values through a lookup table.
    pub const MAP_TABLE: u32 = 7;
    /// `MapPolynomial` — map values through a polynomial.
    pub const MAP_POLYNOMIAL: u32 = 8;
    /// `GainMap` — spatially-varying gain (e.g. flat-field/shading correction).
    pub const GAIN_MAP: u32 = 9;
    /// `DeltaPerRow` — add a per-row delta.
    pub const DELTA_PER_ROW: u32 = 10;
    /// `DeltaPerColumn` — add a per-column delta.
    pub const DELTA_PER_COLUMN: u32 = 11;
    /// `ScalePerRow` — scale by a per-row factor.
    pub const SCALE_PER_ROW: u32 = 12;
    /// `ScalePerColumn` — scale by a per-column factor.
    pub const SCALE_PER_COLUMN: u32 = 13;
    /// `WarpRectilinear2` — the DNG 1.6 extended rectilinear warp.
    pub const WARP_RECTILINEAR_2: u32 = 14;
}

/// One opcode of a DNG opcode list: the container fields, with the opcode's parameters kept as
/// raw bytes (typed parameter decoding and opcode *processing* are deferred — `STATUS.md` P18).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Opcode {
    /// The opcode ID (see [`opcode_id`] for the standard set).
    pub id: u32,
    /// The DNG spec version the opcode was introduced in, as its four dotted octets in order —
    /// e.g. DNG 1.3.0.0 is `[1, 3, 0, 0]`. Writers must not declare a `DNGBackwardVersion` below
    /// the version of any non-optional opcode present (spec p. 124); [`crate::DngEncoder`]
    /// enforces this automatically.
    pub spec_version: [u8; 4],
    /// The flags word (bit 0 = optional, bit 1 = skip for preview quality).
    pub flags: u32,
    /// The opcode's parameter bytes, exactly as stored (big-endian, opcode-specific layout).
    pub parameters: Vec<u8>,
}

impl Opcode {
    /// Flag bit 0: the opcode is optional — a reader that doesn't recognise it may skip it.
    pub const FLAG_OPTIONAL: u32 = 1;
    /// Flag bit 1: the opcode may be skipped when rendering at preview quality.
    pub const FLAG_PREVIEW_SKIP: u32 = 2;

    /// Whether a reader that doesn't recognise this opcode may skip it (flag bit 0).
    #[must_use]
    pub fn is_optional(&self) -> bool {
        self.flags & Self::FLAG_OPTIONAL != 0
    }

    /// Whether the opcode may be skipped at preview quality (flag bit 1).
    #[must_use]
    pub fn skip_for_preview(&self) -> bool {
        self.flags & Self::FLAG_PREVIEW_SKIP != 0
    }
}

/// A parsed `OpcodeList1`/`OpcodeList2`/`OpcodeList3` container: an ordered chain of opcodes.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OpcodeList {
    opcodes: Vec<Opcode>,
}

impl OpcodeList {
    /// Creates an empty list (the tag default).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Parses the container bytes (strictly: truncation, an overrunning parameter count, or
    /// trailing bytes are all rejected). The container is big-endian regardless of the file's
    /// byte order (DNG 1.7.1 p. 105).
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if the
    /// bytes are not a well-formed opcode list.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let count = read_u32(bytes, 0)?;
        // Each opcode needs at least its 16-byte header; an impossible count fails fast instead
        // of looping toward the inevitable truncation error.
        if (count as usize) > bytes.len().saturating_sub(4) / 16 {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "DNG: opcode list is truncated (count exceeds the data)",
            ));
        }
        let mut opcodes = Vec::with_capacity(count as usize);
        let mut pos = 4usize;
        for _ in 0..count {
            let id = read_u32(bytes, pos)?;
            let version = bytes.get(pos + 4..pos + 8).ok_or_else(|| {
                Error::invalid_input(env!("CARGO_PKG_NAME"), "DNG: opcode list is truncated")
            })?;
            let flags = read_u32(bytes, pos + 8)?;
            let param_len = read_u32(bytes, pos + 12)? as usize;
            pos += 16;
            let parameters = bytes
                .get(
                    pos..pos.checked_add(param_len).ok_or_else(|| {
                        Error::invalid_input(
                            env!("CARGO_PKG_NAME"),
                            "DNG: opcode parameter length overflows",
                        )
                    })?,
                )
                .ok_or_else(|| {
                    Error::invalid_input(
                        env!("CARGO_PKG_NAME"),
                        "DNG: opcode parameters exceed the list data",
                    )
                })?
                .to_vec();
            pos += param_len;
            opcodes.push(Opcode {
                id,
                spec_version: [version[0], version[1], version[2], version[3]],
                flags,
                parameters,
            });
        }
        if pos != bytes.len() {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "DNG: opcode list has trailing bytes after the last opcode",
            ));
        }
        Ok(Self { opcodes })
    }

    /// Serializes the list back to the big-endian container layout (`parse`'s inverse).
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(
            4 + self
                .opcodes
                .iter()
                .map(|o| 16 + o.parameters.len())
                .sum::<usize>(),
        );
        out.extend_from_slice(&(self.opcodes.len() as u32).to_be_bytes());
        for opcode in &self.opcodes {
            out.extend_from_slice(&opcode.id.to_be_bytes());
            out.extend_from_slice(&opcode.spec_version);
            out.extend_from_slice(&opcode.flags.to_be_bytes());
            out.extend_from_slice(&(opcode.parameters.len() as u32).to_be_bytes());
            out.extend_from_slice(&opcode.parameters);
        }
        out
    }

    /// Appends an opcode to the chain.
    pub fn push(&mut self, opcode: Opcode) {
        self.opcodes.push(opcode);
    }

    /// The opcodes, in application order.
    #[must_use]
    pub fn opcodes(&self) -> &[Opcode] {
        &self.opcodes
    }

    /// The number of opcodes in the list.
    #[must_use]
    pub fn len(&self) -> usize {
        self.opcodes.len()
    }

    /// Whether the list holds no opcodes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.opcodes.is_empty()
    }
}

/// Reads a big-endian `u32` at `pos`.
fn read_u32(bytes: &[u8], pos: usize) -> Result<u32> {
    let b = pos
        .checked_add(4)
        .and_then(|end| bytes.get(pos..end))
        .ok_or_else(|| {
            Error::invalid_input(env!("CARGO_PKG_NAME"), "DNG: opcode list is truncated")
                .with_byte_offset(pos as u64)
        })?;
    Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A hand-built two-opcode container with distinct field values everywhere.
    fn sample_bytes() -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&2u32.to_be_bytes()); // count
        // Opcode 1: GainMap (9), version 1.3.0.0, optional, 3 parameter bytes.
        bytes.extend_from_slice(&9u32.to_be_bytes());
        bytes.extend_from_slice(&[1, 3, 0, 0]);
        bytes.extend_from_slice(&1u32.to_be_bytes());
        bytes.extend_from_slice(&3u32.to_be_bytes());
        bytes.extend_from_slice(&[0xAA, 0xBB, 0xCC]);
        // Opcode 2: WarpRectilinear2 (14), version 1.6.0.0, preview-skip, no parameters.
        bytes.extend_from_slice(&14u32.to_be_bytes());
        bytes.extend_from_slice(&[1, 6, 0, 0]);
        bytes.extend_from_slice(&2u32.to_be_bytes());
        bytes.extend_from_slice(&0u32.to_be_bytes());
        bytes
    }

    #[test]
    fn parse_reads_every_field() {
        let list = OpcodeList::parse(&sample_bytes()).expect("parse");
        assert_eq!(list.len(), 2);
        assert!(!list.is_empty());
        let first = &list.opcodes()[0];
        assert_eq!(first.id, opcode_id::GAIN_MAP);
        assert_eq!(first.spec_version, [1, 3, 0, 0]);
        assert_eq!(first.flags, Opcode::FLAG_OPTIONAL);
        assert!(first.is_optional());
        assert!(!first.skip_for_preview());
        assert_eq!(first.parameters, vec![0xAA, 0xBB, 0xCC]);
        let second = &list.opcodes()[1];
        assert_eq!(second.id, opcode_id::WARP_RECTILINEAR_2);
        assert_eq!(second.spec_version, [1, 6, 0, 0]);
        assert!(!second.is_optional());
        assert!(second.skip_for_preview());
        assert!(second.parameters.is_empty());
    }

    #[test]
    fn to_bytes_is_parse_inverse() {
        let bytes = sample_bytes();
        let list = OpcodeList::parse(&bytes).expect("parse");
        assert_eq!(list.to_bytes(), bytes);
        // And an empty list is the 4-byte zero count.
        assert_eq!(OpcodeList::new().to_bytes(), vec![0, 0, 0, 0]);
        assert_eq!(
            OpcodeList::parse(&[0, 0, 0, 0]).expect("empty"),
            OpcodeList::new()
        );
    }

    #[test]
    fn push_appends_in_order() {
        let mut list = OpcodeList::new();
        list.push(Opcode {
            id: opcode_id::TRIM_BOUNDS,
            spec_version: [1, 3, 0, 0],
            flags: 0,
            parameters: vec![1, 2],
        });
        assert_eq!(list.len(), 1);
        assert_eq!(list.opcodes()[0].id, opcode_id::TRIM_BOUNDS);
        let reparsed = OpcodeList::parse(&list.to_bytes()).expect("reparse");
        assert_eq!(reparsed, list);
    }

    #[test]
    fn parse_rejects_malformed_containers() {
        // Too short for the count.
        assert!(OpcodeList::parse(&[]).is_err());
        assert!(OpcodeList::parse(&[0, 0, 0]).is_err());
        // Count larger than the data can hold.
        assert!(OpcodeList::parse(&[0, 0, 0, 5, 0, 0]).is_err());
        // Truncated header.
        let mut bytes = 1u32.to_be_bytes().to_vec();
        bytes.extend_from_slice(&[0u8; 12]); // only 12 of 16 header bytes
        assert!(OpcodeList::parse(&bytes).is_err());
        // Parameter count overruns the data.
        let mut bytes = sample_bytes();
        let len = bytes.len();
        bytes[len - 4..].copy_from_slice(&100u32.to_be_bytes()); // second opcode claims 100 bytes
        assert!(OpcodeList::parse(&bytes).is_err());
        // Trailing bytes after the last opcode.
        let mut bytes = sample_bytes();
        bytes.push(0);
        assert!(OpcodeList::parse(&bytes).is_err());
    }

    #[test]
    fn count_capacity_guard_uses_complete_opcode_headers() {
        let mut one = vec![0u8; 20];
        one[3] = 1;
        assert_eq!(OpcodeList::parse(&one).expect("one opcode").len(), 1);

        let mut two_claimed = one;
        two_claimed[3] = 2;
        let error = OpcodeList::parse(&two_claimed).expect_err("truncated second opcode");
        assert!(
            error
                .static_message()
                .is_some_and(|message| message.contains("count exceeds the data"))
        );
    }
}
