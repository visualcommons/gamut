//! The typed `hvcC` HEVCDecoderConfigurationRecord ([`HevcConfig`]) and the config-driven bridges to
//! a downstream HEVC decoder: the Annex-B emitters [`HevcConfig::annex_b`],
//! [`HevcConfig::annex_b_parameter_sets`], and [`HevcConfig::annex_b_payload`] (length-prefixed →
//! Annex-B start codes), and [`HevcConfig::validate_still_payload`] (the still-image IRAP
//! constraint). Which emitter each platform decoder API wants is tabulated in the [crate] docs.
//!
//! Layout is `references/heif` §1 (ISO/IEC 14496-15 §8.3.3.1) exactly. All `reserved` fields are
//! **ignored** on read (a reader must not reject non-conforming reserved bits — §1); they are masked
//! away and never validated.

use gamut_color::ChromaSubsampling;
use gamut_core::{Error, Result};

use crate::nal::{NalHeader, NalUnitType, iter_nal_units};

/// One parameter-set array of a [`HevcConfig`] (`references/heif` §1): all NAL units sharing a
/// `NAL_unit_type`, in file order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NalArray {
    /// `array_completeness`: when `true`, this array holds *all* NAL units of
    /// [`nal_unit_type`](Self::nal_unit_type) and none appear inband in the item payload.
    pub completeness: bool,
    /// The NAL unit type shared by every unit in [`nal_units`](Self::nal_units).
    pub nal_unit_type: NalUnitType,
    /// The raw NAL units (each is `header + RBSP`, no start code), in file order.
    pub nal_units: Vec<Vec<u8>>,
}

/// A parsed `hvcC` HEVCDecoderConfigurationRecord (ISO/IEC 14496-15 §8.3.3.1; `references/heif` §1).
///
/// The fixed 23-byte header fields are exposed as typed values (bit-width fields as the smallest
/// integer that holds them), followed by the parameter-set [`arrays`](Self::arrays). Reserved bits
/// are ignored on read per §1, so two records differing only in their reserved bits parse equal.
///
/// Construct via [`parse`](Self::parse). Non-exhaustive so later fields can be surfaced additively.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct HevcConfig {
    /// `general_profile_space` (2 bits) — from the HEVC `profile_tier_level()`.
    pub general_profile_space: u8,
    /// `general_tier_flag` (1 bit) — `false` = Main tier, `true` = High tier.
    pub general_tier_flag: bool,
    /// `general_profile_idc` (5 bits) — 1 = Main, 2 = Main 10, 3 = Main Still Picture, 4 = Rext.
    pub general_profile_idc: u8,
    /// `general_profile_compatibility_flags` (32 bits).
    pub general_profile_compatibility_flags: u32,
    /// `general_constraint_indicator_flags` (48 bits, held in the low 48 bits of the `u64`).
    pub general_constraint_indicator_flags: u64,
    /// `general_level_idc` (8 bits).
    pub general_level_idc: u8,
    /// `min_spatial_segmentation_idc` (12 bits).
    pub min_spatial_segmentation_idc: u16,
    /// `parallelismType` (2 bits) — 0 = unknown/mixed, 1 = slice, 2 = tile, 3 = WPP.
    pub parallelism_type: u8,
    /// `chroma_format_idc` (2 bits) — see [`chroma_subsampling`](Self::chroma_subsampling).
    pub chroma_format_idc: u8,
    /// `bit_depth_luma_minus8` (3 bits) — see [`bit_depth_luma`](Self::bit_depth_luma).
    pub bit_depth_luma_minus8: u8,
    /// `bit_depth_chroma_minus8` (3 bits) — see [`bit_depth_chroma`](Self::bit_depth_chroma).
    pub bit_depth_chroma_minus8: u8,
    /// `avgFrameRate` (16 bits) — 0 = unspecified (a still image).
    pub avg_frame_rate: u16,
    /// `constantFrameRate` (2 bits).
    pub constant_frame_rate: u8,
    /// `numTemporalLayers` (3 bits).
    pub num_temporal_layers: u8,
    /// `temporalIdNested` (1 bit).
    pub temporal_id_nested: bool,
    /// `lengthSizeMinusOne` (2 bits) — the NAL length-prefix width minus one; see
    /// [`nal_length_size`](Self::nal_length_size).
    pub length_size_minus_one: u8,
    /// The parameter-set arrays (VPS/SPS/PPS/SEI…), in file order.
    pub arrays: Vec<NalArray>,
}

impl HevcConfig {
    /// Parses an `hvcC` HEVCDecoderConfigurationRecord from its raw body bytes (the `hvcC` item
    /// property, `references/heif` §1).
    ///
    /// Reserved bits are masked away and never validated (§1). The parse is exact: every byte must
    /// belong to the header or an array — trailing bytes after the last array are an error.
    ///
    /// # Errors
    ///
    /// - An [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) error if
    ///   `configurationVersion != 1`.
    /// - An [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if
    ///   `lengthSizeMinusOne` is 2 (14496-15 permits only 0, 1, 3 ⇒
    ///   1/2/4-byte prefixes), if the record is truncated anywhere (header, an array header, or a
    ///   NAL unit body), or if any bytes remain after the final array.
    pub fn parse(data: &[u8]) -> Result<Self> {
        let mut r = Reader::new(data);
        let configuration_version = r.u8()?;
        if configuration_version != 1 {
            return Err(Error::unsupported(
                env!("CARGO_PKG_NAME"),
                "HEIC: hvcC configurationVersion must be 1",
            ));
        }
        let b1 = r.u8()?;
        let general_profile_space = (b1 >> 6) & 0x03;
        let general_tier_flag = (b1 >> 5) & 0x01 != 0;
        let general_profile_idc = b1 & 0x1f;
        let general_profile_compatibility_flags = r.u32()?;
        let general_constraint_indicator_flags = r.u48()?;
        let general_level_idc = r.u8()?;
        // reserved(4) | min_spatial_segmentation_idc(12): keep only the low 12 bits.
        let min_spatial_segmentation_idc = r.u16()? & 0x0fff;
        // reserved(6) | parallelismType(2).
        let parallelism_type = r.u8()? & 0x03;
        // reserved(6) | chroma_format_idc(2).
        let chroma_format_idc = r.u8()? & 0x03;
        // reserved(5) | bit_depth_luma_minus8(3).
        let bit_depth_luma_minus8 = r.u8()? & 0x07;
        // reserved(5) | bit_depth_chroma_minus8(3).
        let bit_depth_chroma_minus8 = r.u8()? & 0x07;
        let avg_frame_rate = r.u16()?;
        // constantFrameRate(2) | numTemporalLayers(3) | temporalIdNested(1) | lengthSizeMinusOne(2).
        let packed = r.u8()?;
        let constant_frame_rate = (packed >> 6) & 0x03;
        let num_temporal_layers = (packed >> 3) & 0x07;
        let temporal_id_nested = (packed >> 2) & 0x01 != 0;
        let length_size_minus_one = packed & 0x03;
        if length_size_minus_one == 2 {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "HEIC: hvcC lengthSizeMinusOne 2 (only 0/1/3 are legal)",
            ));
        }
        let num_of_arrays = r.u8()?;

        // `numOfArrays`/`numNalus`/`nalUnitLength` are untrusted; do not pre-allocate from them —
        // the bounded reads below fail on truncation, so a malformed count errors after a bounded
        // number of iterations (mirrors the gamut-isobmff reader).
        let mut arrays = Vec::new();
        for _ in 0..num_of_arrays {
            // array_completeness(1) | reserved(1) | NAL_unit_type(6).
            let head = r.u8()?;
            let completeness = head & 0x80 != 0;
            let nal_unit_type = NalUnitType::from_raw(head & 0x3f);
            let num_nalus = r.u16()?;
            let mut nal_units = Vec::new();
            for _ in 0..num_nalus {
                let nal_unit_length = usize::from(r.u16()?);
                nal_units.push(r.take(nal_unit_length)?.to_vec());
            }
            arrays.push(NalArray {
                completeness,
                nal_unit_type,
                nal_units,
            });
        }
        if !r.at_end() {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "HEIC: hvcC trailing bytes after arrays",
            ));
        }

        Ok(Self {
            general_profile_space,
            general_tier_flag,
            general_profile_idc,
            general_profile_compatibility_flags,
            general_constraint_indicator_flags,
            general_level_idc,
            min_spatial_segmentation_idc,
            parallelism_type,
            chroma_format_idc,
            bit_depth_luma_minus8,
            bit_depth_chroma_minus8,
            avg_frame_rate,
            constant_frame_rate,
            num_temporal_layers,
            temporal_id_nested,
            length_size_minus_one,
            arrays,
        })
    }

    /// The luma bit depth (`bit_depth_luma_minus8 + 8`).
    #[must_use]
    pub fn bit_depth_luma(&self) -> u8 {
        self.bit_depth_luma_minus8 + 8
    }

    /// The chroma bit depth (`bit_depth_chroma_minus8 + 8`).
    #[must_use]
    pub fn bit_depth_chroma(&self) -> u8 {
        self.bit_depth_chroma_minus8 + 8
    }

    /// The chroma sampling `chroma_format_idc` declares (`references/heif` §1): 0 is monochrome
    /// ([`Cs400`](ChromaSubsampling::Cs400)), 1 is 4:2:0, 2 is 4:2:2 and 3 is 4:4:4.
    #[must_use]
    pub fn chroma_subsampling(&self) -> ChromaSubsampling {
        match self.chroma_format_idc {
            0 => ChromaSubsampling::Cs400,
            1 => ChromaSubsampling::Cs420,
            2 => ChromaSubsampling::Cs422,
            // `chroma_format_idc` is a 2-bit field, so the only remaining value is 3.
            _ => ChromaSubsampling::Cs444,
        }
    }

    /// The NAL length-prefix width in bytes (`lengthSizeMinusOne + 1` ⇒ 1, 2, or 4), governing the
    /// item-payload split ([`crate::iter_nal_units`], `references/heif` §2).
    #[must_use]
    pub fn nal_length_size(&self) -> usize {
        usize::from(self.length_size_minus_one) + 1
    }

    /// The NAL units of the given parameter-set type across all arrays, in file order.
    ///
    /// A single record may carry more than one array of the same type, and more than one NAL unit
    /// per array (e.g. several SPS), so this flattens every match in order.
    pub fn parameter_sets(&self, ty: NalUnitType) -> impl Iterator<Item = &[u8]> + '_ {
        self.arrays
            .iter()
            .filter(move |a| a.nal_unit_type == ty)
            .flat_map(|a| a.nal_units.iter().map(Vec::as_slice))
    }

    /// The Video Parameter Set (VPS) NAL units, in file order (a record normally carries one).
    pub fn vps(&self) -> impl Iterator<Item = &[u8]> + '_ {
        self.parameter_sets(NalUnitType::Vps)
    }

    /// The Sequence Parameter Set (SPS) NAL units, in file order (a record may carry several).
    pub fn sps(&self) -> impl Iterator<Item = &[u8]> + '_ {
        self.parameter_sets(NalUnitType::Sps)
    }

    /// The Picture Parameter Set (PPS) NAL units, in file order (a record may carry several).
    pub fn pps(&self) -> impl Iterator<Item = &[u8]> + '_ {
        self.parameter_sets(NalUnitType::Pps)
    }

    /// Converts an `hvc1`/`hev1` item `payload` to a complete Annex-B (ITU-T H.265 Annex B) NAL
    /// stream, appending to `out` (ISO/IEC 14496-15 §8.3.2 / §8.4; `references/heif` §2).
    ///
    /// This is exactly [`annex_b_parameter_sets`](Self::annex_b_parameter_sets) followed by
    /// [`annex_b_payload`](Self::annex_b_payload): the record's parameter sets first, then every NAL
    /// unit of `payload`, each prefixed with a four-byte start code (`00 00 00 01`). It is the form
    /// a raw Annex-B decoder wants — VAAPI/FFmpeg, libde265 — where an API that takes the parameter
    /// sets separately (Android MediaCodec `csd-0`) wants the two halves instead. See the [crate]
    /// docs for the per-API mapping.
    ///
    /// Bytes are appended, so callers can reuse a scratch buffer (allocation-conscious).
    ///
    /// # Errors
    ///
    /// Propagates the payload-split errors of [`crate::iter_nal_units`] (truncated length prefix,
    /// truncated NAL body, or a zero-length NAL). On error, bytes already appended to `out` are left
    /// in place.
    pub fn annex_b(&self, payload: &[u8], out: &mut Vec<u8>) -> Result<()> {
        self.annex_b_parameter_sets(out);
        self.annex_b_payload(payload, out)
    }

    /// Emits the record's parameter-set [`arrays`](Self::arrays) as an Annex-B NAL stream, appending
    /// to `out` (ISO/IEC 14496-15 §8.4; `references/heif` §2).
    ///
    /// Each NAL unit is prefixed with a four-byte start code (`00 00 00 01`), ordered all VPS, then
    /// all SPS, then all PPS, then any remaining arrays (e.g. SEI) in file order — the decoder-init
    /// order H.265 expects, regardless of the order the arrays appear in the record. This is the
    /// Android MediaCodec `csd-0` blob and the VAAPI parameter-set feed; the coded picture follows
    /// from [`annex_b_payload`](Self::annex_b_payload).
    ///
    /// Emitting nothing is valid and not an error: an `hev1` record may carry its parameter sets
    /// inband in the item payload instead (see
    /// [`HeifContainer::hevc_inband_parameter_sets_allowed`](crate::HeifContainer::hevc_inband_parameter_sets_allowed)).
    ///
    /// # Example
    ///
    /// ```
    /// use gamut_heic::HevcConfig;
    ///
    /// // A minimal `hvcC`: the 23-byte header (4-byte NAL length prefixes) plus one VPS array.
    /// let mut record = vec![0u8; 23];
    /// record[0] = 1; // configurationVersion
    /// record[21] = 0b0000_0011; // ... | lengthSizeMinusOne = 3
    /// record[22] = 1; // numOfArrays
    /// // array_completeness | VPS (32); numNalus = 1; nalUnitLength = 3; the NAL unit.
    /// record.extend_from_slice(&[0xA0, 0x00, 0x01, 0x00, 0x03, 0x40, 0x01, 0xAA]);
    /// let config = HevcConfig::parse(&record).unwrap();
    ///
    /// let mut csd0 = Vec::new();
    /// config.annex_b_parameter_sets(&mut csd0);
    /// assert_eq!(csd0, [0x00, 0x00, 0x00, 0x01, 0x40, 0x01, 0xAA]);
    /// ```
    pub fn annex_b_parameter_sets(&self, out: &mut Vec<u8>) {
        for nal in self.vps() {
            emit_annex_b(nal, out);
        }
        for nal in self.sps() {
            emit_annex_b(nal, out);
        }
        for nal in self.pps() {
            emit_annex_b(nal, out);
        }
        for array in &self.arrays {
            if array.nal_unit_type.is_parameter_set() {
                continue;
            }
            for nal in &array.nal_units {
                emit_annex_b(nal, out);
            }
        }
    }

    /// Emits an `hvc1`/`hev1` item `payload` — and nothing else — as an Annex-B NAL stream,
    /// appending to `out` (ISO/IEC 14496-15 §8.3.2; `references/heif` §2).
    ///
    /// Every NAL unit of `payload` is emitted in order, its length prefix
    /// ([`nal_length_size`](Self::nal_length_size) bytes) replaced by a four-byte start code
    /// (`00 00 00 01`). No parameter set from the record is emitted — this is the sample data an API
    /// that was configured separately expects (Android MediaCodec, after `csd-0` from
    /// [`annex_b_parameter_sets`](Self::annex_b_parameter_sets)).
    ///
    /// Payload NAL units are passed through as they appear, with no de-duplication against the
    /// record: an `hev1` payload may carry inband parameter sets, which are emitted here as well.
    /// That is intentional — an H.265 decoder accepts repeated parameter sets.
    ///
    /// # Errors
    ///
    /// Propagates the payload-split errors of [`crate::iter_nal_units`] (truncated length prefix,
    /// truncated NAL body, or a zero-length NAL). On error, bytes already appended to `out` are left
    /// in place.
    ///
    /// # Example
    ///
    /// ```
    /// use gamut_heic::HevcConfig;
    ///
    /// // A minimal `hvcC`: the 23-byte header, 4-byte NAL length prefixes, no arrays.
    /// let mut record = vec![0u8; 23];
    /// record[0] = 1; // configurationVersion
    /// record[21] = 0b0000_0011; // ... | lengthSizeMinusOne = 3
    /// let config = HevcConfig::parse(&record).unwrap();
    ///
    /// // One length-prefixed IDR_W_RADL NAL unit.
    /// let mut sample = Vec::new();
    /// config
    ///     .annex_b_payload(&[0x00, 0x00, 0x00, 0x03, 0x26, 0x01, 0xDD], &mut sample)
    ///     .unwrap();
    /// assert_eq!(sample, [0x00, 0x00, 0x00, 0x01, 0x26, 0x01, 0xDD]);
    /// ```
    pub fn annex_b_payload(&self, payload: &[u8], out: &mut Vec<u8>) -> Result<()> {
        for nal in iter_nal_units(payload, self.nal_length_size()) {
            emit_annex_b(nal?, out);
        }
        Ok(())
    }

    /// Validates that an `hvc1`/`hev1` item `payload` satisfies the HEIF still-image constraint: every
    /// VCL NAL unit is an IRAP picture (`nal_unit_type` `16..=23`), so the item is independently
    /// decodable with no inter-picture prediction (`references/heif` §3).
    ///
    /// Non-VCL NAL units (parameter sets, SEI) are permitted and pass through unchecked.
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if a VCL
    /// NAL unit is not an IRAP type, and propagates the
    /// payload-split ([`crate::iter_nal_units`]) and NAL-header ([`NalHeader::parse`]) errors.
    pub fn validate_still_payload(&self, payload: &[u8]) -> Result<()> {
        for nal in iter_nal_units(payload, self.nal_length_size()) {
            let header = NalHeader::parse(nal?)?;
            if header.unit_type.is_vcl() && !header.unit_type.is_irap() {
                return Err(Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "HEIC: still-image payload has a non-IRAP VCL NAL unit",
                ));
            }
        }
        Ok(())
    }
}

/// Appends one NAL unit to an Annex-B stream: a four-byte start code (`00 00 00 01`) followed by the
/// unit's bytes (ITU-T H.265 Annex B; ISO/IEC 14496-15 §8.4).
fn emit_annex_b(nal: &[u8], out: &mut Vec<u8>) {
    out.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
    out.extend_from_slice(nal);
}

/// A minimal big-endian, bounds-checked byte cursor for the `hvcC` body. Every read is fallible and
/// returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error on
/// truncation, so a malformed length never reads out of bounds.
struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    /// Reads `n` bytes, advancing the cursor.
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let offset = self.pos as u64;
        let end = self
            .pos
            .checked_add(n)
            .filter(|&end| end <= self.data.len())
            .ok_or_else(|| {
                Error::invalid_input(env!("CARGO_PKG_NAME"), "HEIC: hvcC truncated")
                    .with_byte_offset(offset)
            })?;
        let out = &self.data[self.pos..end];
        self.pos = end;
        Ok(out)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16> {
        let b = self.take(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }

    fn u32(&mut self) -> Result<u32> {
        let b = self.take(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    /// Reads a 48-bit big-endian value into the low 48 bits of a `u64`.
    fn u48(&mut self) -> Result<u64> {
        let b = self.take(6)?;
        Ok(b.iter()
            .fold(0u64, |acc, &byte| (acc << 8) | u64::from(byte)))
    }

    fn at_end(&self) -> bool {
        self.pos == self.data.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chroma_format_idc_maps_onto_the_shared_subsampling_vocabulary() {
        // Total mapping, asserted per value so a mutant that collapses two arms dies. The
        // numberings deliberately differ: `chroma_format_idc` is HEVC's, `ChromaSubsampling`'s
        // discriminants are gamut's own.
        let mut raw = vec![0u8; 23];
        raw[0] = 1;
        raw[21] = 0b11;
        for (idc, chroma) in [
            (0, ChromaSubsampling::Cs400),
            (1, ChromaSubsampling::Cs420),
            (2, ChromaSubsampling::Cs422),
            (3, ChromaSubsampling::Cs444),
        ] {
            // reserved(6) | chroma_format_idc(2), reserved bits set as a writer sets them.
            raw[16] = 0xFC | idc;
            assert_eq!(
                HevcConfig::parse(&raw).unwrap().chroma_subsampling(),
                chroma
            );
        }
    }
}
