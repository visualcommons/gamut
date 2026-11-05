//! The HEVC NAL-unit layer: unit-type classification ([`NalUnitType`]), the two-byte NAL header
//! ([`NalHeader`]), and the length-prefixed item-payload split ([`iter_nal_units`]).
//!
//! This is *container scope* (issue #238): the reader classifies each NAL unit (parameter set vs SEI
//! vs IRAP slice) and splits the `hvc1`/`hev1` item payload into NAL units, but never interprets an
//! RBSP payload — slice/CTU decoding is codec scope (issue #18). All layouts are from `references/heif`
//! §§2–3 (ITU-T H.265 §7.3.1.2 / Table 7-1 and ISO/IEC 14496-15 §8.3.2).

use gamut_core::{Error, Result};

/// An HEVC NAL unit type (ITU-T H.265 Table 7-1), as the six-bit `nal_unit_type` field.
///
/// The variants name every type the HEIF container layer must classify (`references/heif` §3);
/// any other value round-trips through [`Other`](Self::Other), so [`from_raw`](Self::from_raw)
/// followed by [`raw`](Self::raw) is the identity for all `0..=63`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NalUnitType {
    /// `BLA_W_LP` (16) — IRAP (VCL): broken-link access with leading pictures.
    BlaWLp,
    /// `BLA_W_RADL` (17) — IRAP (VCL).
    BlaWRadl,
    /// `BLA_N_LP` (18) — IRAP (VCL).
    BlaNLp,
    /// `IDR_W_RADL` (19) — IRAP (VCL): instantaneous decoding refresh.
    IdrWRadl,
    /// `IDR_N_LP` (20) — IRAP (VCL).
    IdrNLp,
    /// `CRA_NUT` (21) — IRAP (VCL): clean random access.
    CraNut,
    /// `RSV_IRAP_VCL22` (22) — reserved IRAP (VCL).
    RsvIrapVcl22,
    /// `RSV_IRAP_VCL23` (23) — reserved IRAP (VCL).
    RsvIrapVcl23,
    /// `VPS_NUT` (32) — video parameter set.
    Vps,
    /// `SPS_NUT` (33) — sequence parameter set.
    Sps,
    /// `PPS_NUT` (34) — picture parameter set.
    Pps,
    /// `PREFIX_SEI_NUT` (39) — supplemental enhancement information (prefix).
    PrefixSei,
    /// `SUFFIX_SEI_NUT` (40) — supplemental enhancement information (suffix).
    SuffixSei,
    /// Any other `nal_unit_type` value, preserved verbatim (the raw six-bit value, `0..=63`).
    Other(u8),
}

impl NalUnitType {
    /// Classifies a raw six-bit `nal_unit_type` value (`0..=63`).
    ///
    /// Values outside the named set map to [`Other`](Self::Other), so this never fails.
    #[must_use]
    pub fn from_raw(value: u8) -> Self {
        match value {
            16 => Self::BlaWLp,
            17 => Self::BlaWRadl,
            18 => Self::BlaNLp,
            19 => Self::IdrWRadl,
            20 => Self::IdrNLp,
            21 => Self::CraNut,
            22 => Self::RsvIrapVcl22,
            23 => Self::RsvIrapVcl23,
            32 => Self::Vps,
            33 => Self::Sps,
            34 => Self::Pps,
            39 => Self::PrefixSei,
            40 => Self::SuffixSei,
            other => Self::Other(other),
        }
    }

    /// The raw six-bit `nal_unit_type` value.
    #[must_use]
    pub fn raw(self) -> u8 {
        match self {
            Self::BlaWLp => 16,
            Self::BlaWRadl => 17,
            Self::BlaNLp => 18,
            Self::IdrWRadl => 19,
            Self::IdrNLp => 20,
            Self::CraNut => 21,
            Self::RsvIrapVcl22 => 22,
            Self::RsvIrapVcl23 => 23,
            Self::Vps => 32,
            Self::Sps => 33,
            Self::Pps => 34,
            Self::PrefixSei => 39,
            Self::SuffixSei => 40,
            Self::Other(other) => other,
        }
    }

    /// Whether this is an IRAP (intra random-access point) VCL type — `nal_unit_type` `16..=23`
    /// (BLA/IDR/CRA and the two reserved IRAP types). A HEIF still-image coded picture is one of
    /// these (`references/heif` §3).
    #[must_use]
    pub fn is_irap(self) -> bool {
        matches!(self.raw(), 16..=23)
    }

    /// Whether this is a parameter set — VPS (32), SPS (33), or PPS (34).
    #[must_use]
    pub fn is_parameter_set(self) -> bool {
        matches!(self, Self::Vps | Self::Sps | Self::Pps)
    }

    /// Whether this is a VCL (video coding layer) NAL type — `nal_unit_type < 32`. VCL units carry
    /// coded slice segments; non-VCL units carry parameter sets, SEI, and delimiters.
    #[must_use]
    pub fn is_vcl(self) -> bool {
        self.raw() < 32
    }
}

/// The two-byte HEVC NAL unit header (ITU-T H.265 §7.3.1.2; `references/heif` §3).
///
/// `forbidden_zero_bit` (which must be 0) is validated at [`parse`](Self::parse) time and not
/// retained.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NalHeader {
    /// The `nal_unit_type` (six bits), classified.
    pub unit_type: NalUnitType,
    /// `nuh_layer_id` (six bits) — 0 for the base layer (still-image items are single-layer).
    pub layer_id: u16,
    /// `nuh_temporal_id_plus1` (three bits) — `TemporalId + 1`; 1 for a still image.
    pub temporal_id_plus1: u8,
}

impl NalHeader {
    /// Parses the two-byte NAL header at the start of `nal`.
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if `nal` is
    /// shorter than two bytes or if `forbidden_zero_bit` is set (a conforming HEVC NAL header always has it 0).
    pub fn parse(nal: &[u8]) -> Result<Self> {
        let &[b0, b1, ..] = nal else {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "HEIC: truncated NAL header",
            ));
        };
        // forbidden_zero_bit(1) | nal_unit_type(6) | nuh_layer_id(6) | nuh_temporal_id_plus1(3)
        if b0 & 0x80 != 0 {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "HEIC: forbidden_zero_bit set",
            ));
        }
        let unit_type = NalUnitType::from_raw((b0 >> 1) & 0x3f);
        let layer_id = (u16::from(b0 & 0x01) << 5) | u16::from(b1 >> 3);
        let temporal_id_plus1 = b1 & 0x07;
        Ok(Self {
            unit_type,
            layer_id,
            temporal_id_plus1,
        })
    }
}

/// A fallible iterator over the NAL units of a length-prefixed `hvc1`/`hev1` item payload
/// (`references/heif` §2). Created by [`iter_nal_units`].
///
/// Each item is a borrowed NAL unit slice (`Ok`) or the first error encountered (`Err`), after which
/// the iterator is exhausted. It yields items until the payload is consumed **exactly**: a clean end
/// (cursor at end of payload) stops iteration with no error, but a partial trailing length prefix,
/// a truncated NAL body, or a zero-length NAL is reported as an error (the every-byte principle).
#[derive(Debug)]
pub struct NalUnitIter<'a> {
    data: &'a [u8],
    pos: usize,
    len_size: usize,
    done: bool,
}

impl<'a> Iterator for NalUnitIter<'a> {
    type Item = Result<&'a [u8]>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done || self.pos == self.data.len() {
            return None;
        }
        // Any error below fuses the iterator: it is reported once, then `done` stops iteration.
        self.done = true;
        let body_start = self.pos + self.len_size;
        let Some(prefix) = self.data.get(self.pos..body_start) else {
            return Some(Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "HEIC: truncated NAL length prefix",
            )
            .with_byte_offset(self.pos as u64)));
        };
        let len = prefix
            .iter()
            .fold(0usize, |acc, &b| (acc << 8) | usize::from(b));
        if len == 0 {
            return Some(Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "HEIC: zero-length NAL unit",
            )
            .with_byte_offset(self.pos as u64)));
        }
        let Some(nal) = body_start
            .checked_add(len)
            .and_then(|end| self.data.get(body_start..end))
        else {
            return Some(Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "HEIC: truncated NAL unit body",
            )
            .with_byte_offset(body_start as u64)));
        };
        // The read succeeded: clear the fuse and advance past this NAL unit.
        self.done = false;
        self.pos = body_start + len;
        Some(Ok(nal))
    }
}

/// Splits a length-prefixed `hvc1`/`hev1` item payload into its NAL units (ISO/IEC 14496-15 §8.3.2;
/// `references/heif` §2), returning a fallible lazy iterator.
///
/// `nal_length_size` is the length-prefix width in bytes — `1`, `2`, or `4`, obtained from
/// [`HevcConfig::nal_length_size`](crate::HevcConfig::nal_length_size) (= `lengthSizeMinusOne + 1`).
/// Each NAL unit is preceded by a big-endian length field of that width; the units are concatenated
/// with no Annex-B start codes.
///
/// A lazy fallible iterator is the primary API (rather than a collecting `-> Result<Vec<&[u8]>>`):
/// it borrows without allocating and lets a caller stop early, and the collecting form is just
/// `iter_nal_units(..).collect::<Result<Vec<_>>>()`. An empty payload yields zero NAL units (a valid,
/// error-free empty iteration).
///
/// The iterator is bounds-checked and consumes the payload exactly: see [`NalUnitIter`] for the
/// truncation / zero-length / trailing-byte error behaviour.
#[must_use]
pub fn iter_nal_units(payload: &[u8], nal_length_size: usize) -> NalUnitIter<'_> {
    NalUnitIter {
        data: payload,
        pos: 0,
        len_size: nal_length_size,
        done: false,
    }
}
