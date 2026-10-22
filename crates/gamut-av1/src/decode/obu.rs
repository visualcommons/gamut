//! OBU framing (AV1 §5.3) and the sequence header (§5.5), parse side.
//!
//! The mirror of [`crate::headers`]'s writers. Two things live here:
//!
//! - [`ObuIter`], which walks a temporal unit's open bitstream units. It handles both the
//!   low-overhead form (`obu_has_size_field = 1`, what AVIF's `av01` payload and gamut's own
//!   encoder use) and the length-delimited form where the size comes from the enclosing
//!   container.
//! - [`SequenceHeader`], the full §5.5.1 parse — reduced *and* non-reduced, all operating points,
//!   and the complete §5.5.2 `color_config()`. Fields this decoder cannot act on are still parsed
//!   (so the bit positions of everything after them are right) and refused later, at the point
//!   where acting on them would matter.

use gamut_bitstream::BitReader;
use gamut_core::{Error, Result};

use super::ORIGIN;

/// `OBU_SEQUENCE_HEADER` (§6.2.2).
pub(crate) const OBU_SEQUENCE_HEADER: u8 = 1;
/// `OBU_TEMPORAL_DELIMITER`.
pub(crate) const OBU_TEMPORAL_DELIMITER: u8 = 2;
/// `OBU_FRAME_HEADER`.
pub(crate) const OBU_FRAME_HEADER: u8 = 3;
/// `OBU_TILE_GROUP`.
pub(crate) const OBU_TILE_GROUP: u8 = 4;
/// `OBU_FRAME` (frame header ∥ tile group).
pub(crate) const OBU_FRAME: u8 = 6;
/// `OBU_REDUNDANT_FRAME_HEADER`.
pub(crate) const OBU_REDUNDANT_FRAME_HEADER: u8 = 7;
/// `OBU_TILE_LIST` (large-scale tiles; forbidden inside an AVIF item).
pub(crate) const OBU_TILE_LIST: u8 = 8;

/// `SELECT_SCREEN_CONTENT_TOOLS` (§3).
pub(crate) const SELECT_SCREEN_CONTENT_TOOLS: u8 = 2;
/// `SELECT_INTEGER_MV` (§3).
pub(crate) const SELECT_INTEGER_MV: u8 = 2;

/// `trailing_bits( obu_size * 8 - payloadBits )` (AV1 §5.3.1, §5.3.4) for an OBU whose payload
/// `r` reads: a `1` marker bit, then zero padding to the **end of the payload**.
///
/// [`BitReader::trailing_bits`] alone stops at the next byte boundary, but §5.3.4's `nbBits` spans
/// whole bytes to the end of the OBU, so the remainder is padding and must be zero too. Applied to
/// every OBU except `OBU_TILE_GROUP`, `OBU_TILE_LIST` and `OBU_FRAME`, this is what makes a
/// payload whose syntax was parsed at the wrong bit positions fail loudly instead of yielding a
/// plausible-looking header.
///
/// # Errors
///
/// Returns [`Error::InvalidInput`] if the marker bit is absent, the payload is truncated, or any
/// padding bit is set.
pub(crate) fn obu_trailing_bits(r: &mut BitReader<'_>) -> Result<()> {
    r.trailing_bits()?;
    // Byte-aligned after `trailing_bits`, so the rest of the payload is whole padding bytes.
    while r.bits_remaining() > 0 {
        if r.f(8)? != 0 {
            return Err(Error::invalid_input(
                ORIGIN,
                "AV1 OBU: non-zero padding after trailing_bits()",
            ));
        }
    }
    Ok(())
}

/// `CP_BT_709`, `TC_SRGB`, `MC_IDENTITY` — the §5.5.2 sRGB shortcut triple.
const CP_BT_709: u8 = 1;
/// See [`CP_BT_709`].
const TC_SRGB: u8 = 13;
/// See [`CP_BT_709`].
const MC_IDENTITY: u8 = 0;
/// `CP_UNSPECIFIED` / `TC_UNSPECIFIED` / `MC_UNSPECIFIED` (§6.4.2).
const UNSPECIFIED: u8 = 2;

/// One open bitstream unit located within a temporal unit.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Obu<'a> {
    /// `obu_type` (§5.3.2).
    pub(crate) kind: u8,
    /// `temporal_id` from the extension header, or 0 when absent.
    pub(crate) temporal_id: u8,
    /// `spatial_id` from the extension header, or 0 when absent.
    pub(crate) spatial_id: u8,
    /// Whether an extension header was present.
    pub(crate) has_extension: bool,
    /// The OBU payload, excluding the header and any size field.
    pub(crate) payload: &'a [u8],
}

/// Walks the open bitstream units of a temporal unit (AV1 §5.3.1).
///
/// Every OBU in the stream must carry `obu_has_size_field = 1`, which is what both the AVIF
/// `av01` item payload (AV1-ISOBMFF §2.4) and gamut's own encoder emit. A final OBU without a
/// size field is accepted and takes the rest of the input, matching the low-overhead rule that
/// only the last unit may omit its size.
pub(crate) struct ObuIter<'a> {
    /// The unconsumed remainder of the temporal unit.
    rest: &'a [u8],
}

impl<'a> ObuIter<'a> {
    /// Starts a walk over `data`.
    pub(crate) const fn new(data: &'a [u8]) -> Self {
        Self { rest: data }
    }
}

impl<'a> Iterator for ObuIter<'a> {
    type Item = Result<Obu<'a>>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.rest.is_empty() {
            return None;
        }
        Some(self.parse_next())
    }
}

impl<'a> ObuIter<'a> {
    /// Parses one OBU from the front of `rest`, advancing past it.
    fn parse_next(&mut self) -> Result<Obu<'a>> {
        let mut r = BitReader::new(self.rest);
        if r.f(1)? != 0 {
            return Err(Error::invalid_input(ORIGIN, "AV1 OBU: forbidden bit set"));
        }
        let kind = r.f(4)? as u8;
        let has_extension = r.flag()?;
        let has_size_field = r.flag()?;
        // obu_reserved_1bit is ignored by a decoder (§6.2.2).
        let _ = r.f(1)?;
        let (temporal_id, spatial_id) = if has_extension {
            let t = r.f(3)? as u8;
            let s = r.f(2)? as u8;
            let _ = r.f(3)?; // extension_header_reserved_3bits
            (t, s)
        } else {
            (0, 0)
        };

        let header_len = 1 + usize::from(has_extension);
        let (size, size_len) = if has_size_field {
            let before = r.bit_position();
            let size = r.leb128()?;
            (size as usize, (r.bit_position() - before) / 8)
        } else {
            // §5.3.1: obu_size = sz - 1 - obu_extension_flag. Only valid for the final OBU, where
            // `sz` is everything that remains.
            (self.rest.len() - header_len, 0)
        };

        let start = header_len + size_len;
        let end = start.checked_add(size).ok_or_else(|| {
            Error::invalid_input(ORIGIN, "AV1 OBU: size field overflows the address space")
        })?;
        if end > self.rest.len() {
            return Err(Error::invalid_input(
                ORIGIN,
                "AV1 OBU: size field runs past the end of the temporal unit",
            ));
        }
        let payload = &self.rest[start..end];
        self.rest = &self.rest[end..];
        Ok(Obu {
            kind,
            temporal_id,
            spatial_id,
            has_extension,
            payload,
        })
    }
}

/// Chroma subsampling as signalled by `color_config()` (§5.5.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Subsampling {
    /// `subsampling_x = 0`, `subsampling_y = 0`.
    Yuv444,
    /// `subsampling_x = 1`, `subsampling_y = 0`.
    Yuv422,
    /// `subsampling_x = 1`, `subsampling_y = 1`.
    Yuv420,
    /// `mono_chrome = 1`: a luma plane only.
    Monochrome,
}

impl Subsampling {
    /// `subsampling_x`.
    #[must_use]
    pub const fn x(self) -> u32 {
        match self {
            Self::Yuv444 => 0,
            Self::Yuv422 | Self::Yuv420 | Self::Monochrome => 1,
        }
    }

    /// `subsampling_y`.
    #[must_use]
    pub const fn y(self) -> u32 {
        match self {
            Self::Yuv444 | Self::Yuv422 => 0,
            Self::Yuv420 | Self::Monochrome => 1,
        }
    }

    /// `NumPlanes` (§5.5.2): 1 for monochrome, 3 otherwise.
    #[must_use]
    pub const fn num_planes(self) -> usize {
        match self {
            Self::Monochrome => 1,
            _ => 3,
        }
    }
}

/// The colour signalling of `color_config()` (§5.5.2), as CICP code points.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColorConfig {
    /// `BitDepth`: 8, 10, or 12.
    pub bit_depth: u32,
    /// Chroma format, with `mono_chrome` folded in.
    pub subsampling: Subsampling,
    /// `color_primaries` (CICP H.273).
    pub color_primaries: u8,
    /// `transfer_characteristics` (CICP H.273).
    pub transfer_characteristics: u8,
    /// `matrix_coefficients` (CICP H.273).
    pub matrix_coefficients: u8,
    /// `color_range`: `true` for full range.
    pub full_range: bool,
    /// `chroma_sample_position` (only meaningful for 4:2:0).
    pub chroma_sample_position: u8,
    /// `separate_uv_delta_q`.
    pub separate_uv_delta_q: bool,
}

/// The AV1 sequence header (§5.5.1), fully parsed.
///
/// Both the reduced still-picture form and the general form are read. The fields a still-image
/// decoder never acts on (timing info, decoder model, extra operating points) are consumed for
/// their bit width and otherwise discarded — keeping them would be dead surface on an image
/// codec, but skipping their *bits* would corrupt everything that follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SequenceHeader {
    /// `seq_profile`: 0 = Main, 1 = High, 2 = Professional.
    pub seq_profile: u8,
    /// `still_picture`.
    pub still_picture: bool,
    /// `reduced_still_picture_header`.
    pub reduced_still_picture_header: bool,
    /// `seq_level_idx[0]` of the chosen operating point.
    pub seq_level_idx: u8,
    /// `seq_tier[0]` of the chosen operating point.
    pub seq_tier: u8,
    /// `OperatingPointIdc` of the chosen operating point.
    pub operating_point_idc: u32,
    /// `frame_width_bits_minus_1 + 1` — the width, in bits, of every coded frame width. An
    /// encoder may choose more bits than the minimum, so `frame_size()` must use this rather than
    /// re-derive it from [`max_frame_width`](Self::max_frame_width).
    pub frame_width_bits: u32,
    /// `frame_height_bits_minus_1 + 1`. See [`frame_width_bits`](Self::frame_width_bits).
    pub frame_height_bits: u32,
    /// `max_frame_width_minus_1 + 1`.
    pub max_frame_width: u32,
    /// `max_frame_height_minus_1 + 1`.
    pub max_frame_height: u32,
    /// `frame_id_numbers_present_flag`.
    pub frame_id_numbers_present: bool,
    /// `delta_frame_id_length_minus_2 + 2`, when frame ids are present.
    pub delta_frame_id_length: u32,
    /// `additional_frame_id_length_minus_1 + 1`, when frame ids are present.
    pub additional_frame_id_length: u32,
    /// `use_128x128_superblock`.
    pub use_128x128_superblock: bool,
    /// `enable_filter_intra`.
    pub enable_filter_intra: bool,
    /// `enable_intra_edge_filter`.
    pub enable_intra_edge_filter: bool,
    /// `enable_superres`.
    pub enable_superres: bool,
    /// `enable_cdef`.
    pub enable_cdef: bool,
    /// `enable_restoration`.
    pub enable_restoration: bool,
    /// `enable_order_hint` (0 for a reduced still-picture header).
    pub enable_order_hint: bool,
    /// `OrderHintBits`.
    pub order_hint_bits: u32,
    /// `seq_force_screen_content_tools`.
    pub seq_force_screen_content_tools: u8,
    /// `seq_force_integer_mv`.
    pub seq_force_integer_mv: u8,
    /// `color_config()`.
    pub color: ColorConfig,
    /// `film_grain_params_present`.
    pub film_grain_params_present: bool,
    /// `decoder_model_info_present_flag`. A still image never carries a decoder model; the flag
    /// is recorded because it makes every frame header code `buffer_removal_time` (§5.9.2), so a
    /// decoder must refuse the stream rather than parse on and desync.
    pub decoder_model_info_present: bool,
}

impl SequenceHeader {
    /// Parses a sequence header OBU payload (§5.5.1), including the OBU's trailing bits (§5.3.1).
    ///
    /// `payload` is the OBU's payload alone — the bytes after the OBU header and its `obu_size`
    /// field — so a container that has already split its OBUs (an AVIF item, an `av1C`
    /// `configOBUs`) reads the header through this one parser rather than a second copy of §5.5.
    /// It reads syntax only: no [`DecodeLimits`](crate::DecodeLimits) are applied and no tool is
    /// refused, so a header this crate cannot *decode* still parses.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidInput`] if the payload is truncated, internally inconsistent, or
    /// does not end in the `trailing_bits()` the OBU's declared size requires.
    pub fn parse(payload: &[u8]) -> Result<Self> {
        let mut r = BitReader::new(payload);
        let header = Self::parse_syntax(&mut r)?;
        // §5.3.1 ends every OBU but a tile group, tile list or frame with
        // `trailing_bits(obu_size * 8 - payloadBits)`. It is the only thing that ties the bits the
        // parse consumed to the size the OBU declared: without it a header whose branch flags were
        // corrupted parses a different field sequence, consumes a different number of bits, and
        // still "succeeds" — yielding plausible but fabricated dimensions and CICP code points.
        obu_trailing_bits(&mut r)?;
        Ok(header)
    }

    /// The §5.5.1 syntax proper, leaving the OBU's trailing bits to [`SequenceHeader::parse`].
    fn parse_syntax(r: &mut BitReader<'_>) -> Result<Self> {
        let seq_profile = r.f(3)? as u8;
        if seq_profile > 2 {
            return Err(Error::invalid_input(
                ORIGIN,
                "AV1 sequence header: seq_profile above 2 is reserved",
            ));
        }
        let still_picture = r.flag()?;
        let reduced_still_picture_header = r.flag()?;

        let seq_level_idx;
        let seq_tier;
        let operating_point_idc;
        let mut decoder_model_info_present = false;
        if reduced_still_picture_header {
            seq_level_idx = r.f(5)? as u8;
            seq_tier = 0;
            operating_point_idc = 0;
        } else {
            let timing_info_present = r.flag()?;
            let mut buffer_delay_length = 0u32;
            if timing_info_present {
                Self::skip_timing_info(r)?;
                decoder_model_info_present = r.flag()?;
                if decoder_model_info_present {
                    buffer_delay_length = r.f(5)? + 1;
                    let _ = r.f(32)?; // num_units_in_decoding_tick
                    let _ = r.f(5)?; // buffer_removal_time_length_minus_1
                    let _ = r.f(5)?; // frame_presentation_time_length_minus_1
                }
            }
            let initial_display_delay_present = r.flag()?;
            let operating_points_cnt = r.f(5)? + 1;
            // §5.5.1 `choose_operating_point()` is decoder policy; a still-image decoder takes
            // operating point 0, the one every conformant stream must be decodable at.
            let mut chosen = (0u32, 0u8, 0u8);
            for i in 0..operating_points_cnt {
                let idc = r.f(12)?;
                let level = r.f(5)? as u8;
                let tier = if level > 7 { r.f(1)? as u8 } else { 0 };
                if decoder_model_info_present && r.flag()? {
                    // operating_parameters_info(i) (§5.5.5).
                    let _ = r.f64(buffer_delay_length)?; // decoder_buffer_delay
                    let _ = r.f64(buffer_delay_length)?; // encoder_buffer_delay
                    let _ = r.f(1)?; // low_delay_mode_flag
                }
                if initial_display_delay_present && r.flag()? {
                    let _ = r.f(4)?; // initial_display_delay_minus_1
                }
                if i == 0 {
                    chosen = (idc, level, tier);
                }
            }
            operating_point_idc = chosen.0;
            seq_level_idx = chosen.1;
            seq_tier = chosen.2;
        }

        let frame_width_bits = r.f(4)? + 1;
        let frame_height_bits = r.f(4)? + 1;
        let max_frame_width = r.f(frame_width_bits)? + 1;
        let max_frame_height = r.f(frame_height_bits)? + 1;

        let frame_id_numbers_present = if reduced_still_picture_header {
            false
        } else {
            r.flag()?
        };
        let mut delta_frame_id_length = 0;
        let mut additional_frame_id_length = 0;
        if frame_id_numbers_present {
            delta_frame_id_length = r.f(4)? + 2;
            additional_frame_id_length = r.f(3)? + 1;
        }

        let use_128x128_superblock = r.flag()?;
        let enable_filter_intra = r.flag()?;
        let enable_intra_edge_filter = r.flag()?;

        let enable_order_hint;
        let order_hint_bits;
        let seq_force_screen_content_tools;
        let seq_force_integer_mv;
        if reduced_still_picture_header {
            enable_order_hint = false;
            order_hint_bits = 0;
            seq_force_screen_content_tools = SELECT_SCREEN_CONTENT_TOOLS;
            seq_force_integer_mv = SELECT_INTEGER_MV;
        } else {
            let _ = r.f(1)?; // enable_interintra_compound
            let _ = r.f(1)?; // enable_masked_compound
            let _ = r.f(1)?; // enable_warped_motion
            let _ = r.f(1)?; // enable_dual_filter
            enable_order_hint = r.flag()?;
            if enable_order_hint {
                let _ = r.f(1)?; // enable_jnt_comp
                let _ = r.f(1)?; // enable_ref_frame_mvs
            }
            seq_force_screen_content_tools = if r.flag()? {
                SELECT_SCREEN_CONTENT_TOOLS
            } else {
                r.f(1)? as u8
            };
            seq_force_integer_mv = if seq_force_screen_content_tools > 0 {
                if r.flag()? {
                    SELECT_INTEGER_MV
                } else {
                    r.f(1)? as u8
                }
            } else {
                SELECT_INTEGER_MV
            };
            order_hint_bits = if enable_order_hint { r.f(3)? + 1 } else { 0 };
        }

        let enable_superres = r.flag()?;
        let enable_cdef = r.flag()?;
        let enable_restoration = r.flag()?;
        let color = Self::parse_color_config(r, seq_profile)?;
        let film_grain_params_present = r.flag()?;

        Ok(Self {
            seq_profile,
            still_picture,
            reduced_still_picture_header,
            seq_level_idx,
            seq_tier,
            operating_point_idc,
            frame_width_bits,
            frame_height_bits,
            max_frame_width,
            max_frame_height,
            frame_id_numbers_present,
            delta_frame_id_length,
            additional_frame_id_length,
            use_128x128_superblock,
            enable_filter_intra,
            enable_intra_edge_filter,
            enable_superres,
            enable_cdef,
            enable_restoration,
            enable_order_hint,
            order_hint_bits,
            seq_force_screen_content_tools,
            seq_force_integer_mv,
            color,
            film_grain_params_present,
            decoder_model_info_present,
        })
    }

    /// `timing_info()` (§5.5.3) — consumed for its width; a still image has no timing.
    fn skip_timing_info(r: &mut BitReader<'_>) -> Result<()> {
        let _ = r.f(32)?; // num_units_in_display_tick
        let _ = r.f(32)?; // time_scale
        if r.flag()? {
            let _ = r.uvlc()?; // num_ticks_per_picture_minus_1
        }
        Ok(())
    }

    /// `color_config()` (§5.5.2).
    fn parse_color_config(r: &mut BitReader<'_>, seq_profile: u8) -> Result<ColorConfig> {
        let high_bitdepth = r.flag()?;
        let bit_depth = if seq_profile == 2 && high_bitdepth {
            if r.flag()? { 12 } else { 10 }
        } else if high_bitdepth {
            10
        } else {
            8
        };

        let mono_chrome = if seq_profile == 1 { false } else { r.flag()? };
        let color_description_present = r.flag()?;
        let (color_primaries, transfer_characteristics, matrix_coefficients) =
            if color_description_present {
                (r.f(8)? as u8, r.f(8)? as u8, r.f(8)? as u8)
            } else {
                (UNSPECIFIED, UNSPECIFIED, UNSPECIFIED)
            };

        if mono_chrome {
            let full_range = r.flag()?;
            return Ok(ColorConfig {
                bit_depth,
                subsampling: Subsampling::Monochrome,
                color_primaries,
                transfer_characteristics,
                matrix_coefficients,
                full_range,
                chroma_sample_position: 0,
                separate_uv_delta_q: false,
            });
        }

        let full_range;
        let (ssx, ssy);
        if color_primaries == CP_BT_709
            && transfer_characteristics == TC_SRGB
            && matrix_coefficients == MC_IDENTITY
        {
            // The sRGB shortcut infers full range and 4:4:4 and codes no bits.
            full_range = true;
            (ssx, ssy) = (0u32, 0u32);
        } else {
            full_range = r.flag()?;
            (ssx, ssy) = match seq_profile {
                0 => (1, 1),
                1 => (0, 0),
                _ => {
                    if bit_depth == 12 {
                        let x = r.f(1)?;
                        let y = if x == 1 { r.f(1)? } else { 0 };
                        (x, y)
                    } else {
                        (1, 0)
                    }
                }
            };
        }
        let chroma_sample_position = if ssx == 1 && ssy == 1 {
            r.f(2)? as u8
        } else {
            0
        };
        let separate_uv_delta_q = r.flag()?;

        let subsampling = match (ssx, ssy) {
            (0, 0) => Subsampling::Yuv444,
            (1, 0) => Subsampling::Yuv422,
            _ => Subsampling::Yuv420,
        };
        Ok(ColorConfig {
            bit_depth,
            subsampling,
            color_primaries,
            transfer_characteristics,
            matrix_coefficients,
            full_range,
            chroma_sample_position,
            separate_uv_delta_q,
        })
    }
}

#[cfg(test)]
mod tests {
    use gamut_bitstream::BitWriter;

    use super::*;
    use crate::decode::testutil::{encoder_seq_header, still_config};
    use crate::headers::Av1Colour;

    #[test]
    fn parses_the_encoders_reduced_sequence_header() {
        for (w, h, lossy, superres) in [
            (16u32, 16u32, false, false),
            (64, 64, true, false),
            (257, 129, true, true),
        ] {
            let payload = encoder_seq_header(w, h, lossy, superres);
            let sh = SequenceHeader::parse(&payload).unwrap();
            assert_eq!(sh.seq_profile, 1);
            assert!(sh.still_picture);
            assert!(sh.reduced_still_picture_header);
            assert_eq!(sh.max_frame_width, w);
            assert_eq!(sh.max_frame_height, h);
            assert!(!sh.use_128x128_superblock);
            assert_eq!(sh.enable_filter_intra, lossy);
            assert!(!sh.enable_intra_edge_filter);
            assert_eq!(sh.enable_superres, superres);
            assert_eq!(sh.enable_cdef, lossy);
            assert_eq!(sh.enable_restoration, lossy);
            assert_eq!(sh.color.bit_depth, 8);
            assert_eq!(sh.color.subsampling, Subsampling::Yuv444);
            assert!(sh.color.full_range);
            assert!(!sh.film_grain_params_present);
            assert_eq!(
                sh.seq_force_screen_content_tools,
                SELECT_SCREEN_CONTENT_TOOLS
            );
            assert_eq!(sh.seq_force_integer_mv, SELECT_INTEGER_MV);
            assert_eq!(sh.order_hint_bits, 0);
        }
    }

    #[test]
    fn parses_a_non_srgb_colour_triple() {
        // BT.709 primaries / BT.709 transfer / BT.709 matrix is outside the sRGB shortcut, so
        // `color_range` is coded explicitly.
        use gamut_color::cicp::{
            ColorRange, ColourPrimaries, MatrixCoefficients, TransferCharacteristics,
        };
        let colour = Av1Colour {
            primaries: ColourPrimaries::Bt709,
            transfer: TransferCharacteristics::Bt709,
            matrix: MatrixCoefficients::Bt709,
            range: ColorRange::Limited,
        };
        let cfg = still_config(32, 32, colour);
        let payload = crate::headers::sequence_header_payload(&cfg, 32, 32, true, false);
        let sh = SequenceHeader::parse(&payload).unwrap();
        assert_eq!(sh.color.color_primaries, 1);
        assert_eq!(sh.color.transfer_characteristics, 1);
        assert_eq!(sh.color.matrix_coefficients, 1);
        assert!(!sh.color.full_range, "limited range must survive the parse");
        assert_eq!(sh.color.subsampling, Subsampling::Yuv444);
    }

    /// A general (non-reduced) §5.5.1 sequence header for a 64×64 frame. `operating_points` codes
    /// everything from `timing_info_present_flag` through the operating-point loop, and `colour`
    /// codes the whole `color_config()` (§5.5.2); every other field is zero.
    ///
    /// Neither encoder this crate is tested against emits timing info, a decoder model, a level
    /// above 7, profile 2 or a high bit depth, so those §5.5 branches are only reachable from here.
    fn general_header(
        seq_profile: u32,
        operating_points: impl FnOnce(&mut BitWriter),
        colour: impl FnOnce(&mut BitWriter),
    ) -> Vec<u8> {
        let mut w = BitWriter::new();
        w.put_bits(seq_profile, 3);
        w.put_bit(1); // still_picture
        w.put_bit(0); // reduced_still_picture_header
        operating_points(&mut w);
        w.put_bits(5, 4); // frame_width_bits_minus_1
        w.put_bits(5, 4); // frame_height_bits_minus_1
        w.put_bits(63, 6); // max_frame_width_minus_1
        w.put_bits(63, 6); // max_frame_height_minus_1
        w.put_bit(0); // frame_id_numbers_present_flag
        w.put_bits(0, 3); // use_128x128_superblock, enable_filter_intra, enable_intra_edge_filter
        w.put_bits(0, 4); // the four inter-coding tool flags
        w.put_bit(0); // enable_order_hint
        w.put_bit(1); // seq_choose_screen_content_tools
        w.put_bit(1); // seq_choose_integer_mv
        w.put_bits(0, 3); // enable_superres, enable_cdef, enable_restoration
        colour(&mut w);
        w.put_bit(0); // film_grain_params_present
        w.put_bit(1); // trailing_bits
        w.byte_align();
        w.into_bytes()
    }

    /// One operating point at level 0, with no timing, decoder model or display delay.
    fn one_level_0_operating_point(w: &mut BitWriter) {
        w.put_bit(0); // timing_info_present_flag
        w.put_bit(0); // initial_display_delay_present_flag
        w.put_bits(0, 5); // operating_points_cnt_minus_1
        w.put_bits(0, 12); // operating_point_idc[0]
        w.put_bits(0, 5); // seq_level_idx[0]
    }

    #[test]
    fn levels_above_7_and_only_those_code_a_tier() {
        // §5.5.1: `if (seq_level_idx[i] > 7) seq_tier[i]`. Operating point 0 is at level 8 with
        // the tier bit set; operating point 1 at level 7 codes none, so reading one there shifts
        // every later field by a bit.
        let payload = general_header(
            0,
            |w| {
                w.put_bit(0); // timing_info_present_flag
                w.put_bit(0); // initial_display_delay_present_flag
                w.put_bits(1, 5); // operating_points_cnt_minus_1
                w.put_bits(0x103, 12); // operating_point_idc[0]
                w.put_bits(8, 5); // seq_level_idx[0]
                w.put_bit(1); // seq_tier[0]
                w.put_bits(0x101, 12); // operating_point_idc[1]
                w.put_bits(7, 5); // seq_level_idx[1], no tier
            },
            |w| {
                w.put_bits(0, 3); // high_bitdepth, mono_chrome, color_description_present_flag
                w.put_bit(0); // color_range
                w.put_bits(0, 2); // chroma_sample_position
                w.put_bit(0); // separate_uv_delta_q
            },
        );
        let sh = SequenceHeader::parse(&payload).unwrap();
        assert_eq!(
            (sh.operating_point_idc, sh.seq_level_idx, sh.seq_tier),
            (0x103, 8, 1)
        );
        assert_eq!((sh.max_frame_width, sh.max_frame_height), (64, 64));
    }

    #[test]
    fn timing_info_and_the_decoder_model_are_consumed_at_their_coded_widths() {
        // §5.5.3 `timing_info()` is 64 bits plus a `uvlc()` when `equal_picture_interval` is set,
        // and §5.5.4 makes `buffer_delay_length_minus_1 + 1` the width of both §5.5.5 delays. A
        // still decoder keeps none of it, so only the fields after it can show it was skipped
        // at the right width.
        let payload = general_header(
            0,
            |w| {
                w.put_bit(1); // timing_info_present_flag
                w.put_bits(1001, 32); // num_units_in_display_tick
                w.put_bits(60_000, 32); // time_scale
                w.put_bit(1); // equal_picture_interval
                w.put_bit(1); // num_ticks_per_picture_minus_1 = uvlc() 0
                w.put_bit(1); // decoder_model_info_present_flag
                w.put_bits(9, 5); // buffer_delay_length_minus_1: delays are 10 bits
                w.put_bits(0xdead_beef, 32); // num_units_in_decoding_tick
                w.put_bits(0, 5); // buffer_removal_time_length_minus_1
                w.put_bits(0, 5); // frame_presentation_time_length_minus_1
                w.put_bit(0); // initial_display_delay_present_flag
                w.put_bits(0, 5); // operating_points_cnt_minus_1
                w.put_bits(0x7ff, 12); // operating_point_idc[0]
                w.put_bits(5, 5); // seq_level_idx[0]
                w.put_bit(1); // decoder_model_present_for_this_op[0]
                w.put_bits(0x2aa, 10); // decoder_buffer_delay
                w.put_bits(0x155, 10); // encoder_buffer_delay
                w.put_bit(1); // low_delay_mode_flag
            },
            |w| {
                w.put_bits(0, 3); // high_bitdepth, mono_chrome, color_description_present_flag
                w.put_bit(1); // color_range
                w.put_bits(0, 2); // chroma_sample_position
                w.put_bit(0); // separate_uv_delta_q
            },
        );
        let sh = SequenceHeader::parse(&payload).unwrap();
        assert!(sh.decoder_model_info_present);
        assert_eq!((sh.operating_point_idc, sh.seq_level_idx), (0x7ff, 5));
        assert_eq!((sh.max_frame_width, sh.max_frame_height), (64, 64));
        assert!(sh.color.full_range);
    }

    #[test]
    fn profile_2_codes_twelve_bit_and_each_of_its_chroma_layouts() {
        // §5.5.2 for `seq_profile = 2` outside the sRGB shortcut: 10-bit is always 4:2:2, and
        // only 12-bit codes `subsampling_x`, then `subsampling_y` only when `subsampling_x` is 1.
        // `chroma_sample_position` is coded for 4:2:0 alone.
        for (twelve_bit, ss, depth, subsampling, csp) in [
            (false, None, 10, Subsampling::Yuv422, 0),
            (true, Some((0, None)), 12, Subsampling::Yuv444, 0),
            (true, Some((1, Some(0))), 12, Subsampling::Yuv422, 0),
            (true, Some((1, Some(1))), 12, Subsampling::Yuv420, 2),
        ] {
            let payload = general_header(2, one_level_0_operating_point, |w| {
                w.put_bit(1); // high_bitdepth
                w.put_bit(u8::from(twelve_bit)); // twelve_bit
                w.put_bit(0); // mono_chrome
                w.put_bit(0); // color_description_present_flag
                w.put_bit(0); // color_range
                if let Some((x, y)) = ss {
                    w.put_bits(x, 1); // subsampling_x
                    if let Some(y) = y {
                        w.put_bits(y, 1); // subsampling_y
                    }
                }
                if subsampling == Subsampling::Yuv420 {
                    w.put_bits(csp, 2); // chroma_sample_position
                }
                w.put_bit(0); // separate_uv_delta_q
            });
            let sh = SequenceHeader::parse(&payload)
                .unwrap_or_else(|e| panic!("{depth}-bit {subsampling:?}: {e}"));
            assert_eq!(sh.seq_profile, 2);
            assert_eq!(sh.color.bit_depth, depth, "{subsampling:?}");
            assert_eq!(sh.color.subsampling, subsampling, "{depth}-bit");
            assert_eq!(u32::from(sh.color.chroma_sample_position), csp);
        }
    }

    #[test]
    fn a_high_bitdepth_profile_0_header_is_ten_bit_without_a_twelve_bit_flag() {
        // `twelve_bit` is coded only for `seq_profile == 2` (§5.5.2); profile 0 at high bit depth
        // is 10-bit and goes straight on to `mono_chrome`.
        let payload = general_header(0, one_level_0_operating_point, |w| {
            w.put_bit(1); // high_bitdepth
            w.put_bit(0); // mono_chrome
            w.put_bit(0); // color_description_present_flag
            w.put_bit(1); // color_range
            w.put_bits(1, 2); // chroma_sample_position
            w.put_bit(0); // separate_uv_delta_q
        });
        let sh = SequenceHeader::parse(&payload).unwrap();
        assert_eq!(sh.color.bit_depth, 10);
        assert_eq!(sh.color.subsampling, Subsampling::Yuv420);
        assert_eq!(sh.color.chroma_sample_position, 1);
    }

    #[test]
    fn an_identity_matrix_alone_is_not_the_srgb_shortcut() {
        // The §5.5.2 shortcut needs all three of BT.709 primaries, sRGB transfer and the identity
        // matrix. Identity with unspecified primaries and transfer still codes `color_range`.
        let payload = general_header(1, one_level_0_operating_point, |w| {
            w.put_bit(0); // high_bitdepth; profile 1 codes no mono_chrome
            w.put_bit(1); // color_description_present_flag
            w.put_bits(2, 8); // color_primaries = unspecified
            w.put_bits(2, 8); // transfer_characteristics = unspecified
            w.put_bits(0, 8); // matrix_coefficients = MC_IDENTITY
            w.put_bit(0); // color_range: limited
            w.put_bit(0); // separate_uv_delta_q
        });
        let sh = SequenceHeader::parse(&payload).unwrap();
        assert!(!sh.color.full_range, "the shortcut would infer full range");
        assert_eq!(sh.color.subsampling, Subsampling::Yuv444);
    }

    #[test]
    fn rejects_a_reserved_profile() {
        // seq_profile = 3 in the top 3 bits.
        let payload = [0b1110_0000u8, 0, 0, 0];
        assert_eq!(
            SequenceHeader::parse(&payload)
                .unwrap_err()
                .static_message(),
            Some("AV1 sequence header: seq_profile above 2 is reserved")
        );
    }

    #[test]
    fn a_sequence_header_must_end_in_the_obus_trailing_bits() {
        // §5.3.1's `trailing_bits(obu_size * 8 - payloadBits)` is the only tie between the bits the
        // parse consumed and the size the OBU declared. Without it a corrupted branch flag makes
        // the parse read a different field sequence and still succeed, so the caller cross-checks
        // its container against fabricated dimensions and CICP code points.
        let payload = encoder_seq_header(16, 16, false, false);
        SequenceHeader::parse(&payload).expect("the encoder's own header is well formed");

        // Clearing the marker bit leaves only zero padding.
        let mut no_marker = payload.clone();
        let last = no_marker.len() - 1;
        assert_ne!(no_marker[last], 0, "the marker bit lives in the last byte");
        no_marker[last] = 0;
        assert_eq!(
            SequenceHeader::parse(&no_marker)
                .unwrap_err()
                .static_message(),
            Some("AV1 trailing_bits(): missing marker bit")
        );

        // Padding past the byte boundary is still padding: a zero byte is legal, a non-zero one is
        // not. `BitReader::trailing_bits` alone stops at the byte boundary and would accept both.
        let mut zero_padded = payload.clone();
        zero_padded.push(0);
        SequenceHeader::parse(&zero_padded).expect("zero padding to the end of the OBU is legal");
        let mut dirty_padded = payload.clone();
        dirty_padded.push(0x01);
        assert_eq!(
            SequenceHeader::parse(&dirty_padded)
                .unwrap_err()
                .static_message(),
            Some("AV1 OBU: non-zero padding after trailing_bits()")
        );

        // Dropping the last byte truncates the trailing bits away.
        assert!(SequenceHeader::parse(&payload[..payload.len() - 1]).is_err());
    }

    #[test]
    fn a_flipped_branch_flag_is_refused_rather_than_silently_reinterpreted() {
        // `color_description_present_flag` gates 24 bits of CICP. Clearing it makes the parse land
        // 24 bits early, where the trailing-bits check is what turns "wrong colour" into a
        // refusal. `StillBuilder`'s general header puts the flag at bit 64 — asserted here so the
        // test fails loudly rather than silently flipping some other bit if the builder changes.
        let still = crate::decode::testutil::StillBuilder::default();
        let payload = still.sequence_header();
        let sane = SequenceHeader::parse(&payload).expect("the builder's header is well formed");
        assert_eq!(sane.color.transfer_characteristics, 13);
        assert_eq!(
            payload[8] & 0x80,
            0x80,
            "bit 64 is color_description_present_flag"
        );

        let mut flipped = payload;
        flipped[8] &= 0x7f;
        assert!(
            SequenceHeader::parse(&flipped).is_err(),
            "a header parsed at the wrong bit positions must be refused, not reinterpreted"
        );
    }

    #[test]
    fn rejects_a_truncated_sequence_header() {
        let payload = encoder_seq_header(16, 16, false, false);
        for cut in 0..payload.len() {
            assert!(
                SequenceHeader::parse(&payload[..cut]).is_err(),
                "truncation to {cut} bytes must be refused"
            );
        }
    }

    #[test]
    fn subsampling_geometry_matches_the_signalled_flags() {
        for (s, x, y, planes) in [
            (Subsampling::Yuv444, 0, 0, 3),
            (Subsampling::Yuv422, 1, 0, 3),
            (Subsampling::Yuv420, 1, 1, 3),
            (Subsampling::Monochrome, 1, 1, 1),
        ] {
            assert_eq!(s.x(), x);
            assert_eq!(s.y(), y);
            assert_eq!(s.num_planes(), planes);
        }
    }

    #[test]
    fn walks_the_encoders_temporal_unit() {
        let seq = encoder_seq_header(16, 16, false, false);
        let frame = vec![0xaa, 0xbb, 0xcc];
        let unit = crate::headers::assemble_temporal_unit(&seq, &frame);
        let obus: Vec<_> = ObuIter::new(&unit).collect::<Result<Vec<_>>>().unwrap();
        assert_eq!(obus.len(), 2);
        assert_eq!(obus[0].kind, OBU_SEQUENCE_HEADER);
        assert_eq!(obus[0].payload, &seq[..]);
        assert!(!obus[0].has_extension);
        assert_eq!(obus[1].kind, OBU_FRAME);
        assert_eq!(obus[1].payload, &frame[..]);
    }

    #[test]
    fn parses_an_extension_header() {
        // header: forbidden=0, type=OBU_FRAME(6), ext=1, has_size=1, reserved=0 -> 0b0_0110_1_1_0
        // ext byte: temporal_id=3, spatial_id=2, reserved=0 -> 0b011_10_000
        let unit = [0b0011_0110u8, 0b0111_0000, 0x02, 0xde, 0xad];
        let obus: Vec<_> = ObuIter::new(&unit).collect::<Result<Vec<_>>>().unwrap();
        assert_eq!(obus.len(), 1);
        assert_eq!(obus[0].kind, OBU_FRAME);
        assert!(obus[0].has_extension);
        assert_eq!(obus[0].temporal_id, 3);
        assert_eq!(obus[0].spatial_id, 2);
        assert_eq!(obus[0].payload, &[0xde, 0xad]);
    }

    #[test]
    fn final_obu_may_omit_its_size_field() {
        // has_size_field = 0: the payload runs to the end of the input.
        let unit = [0b0011_0000u8, 0x01, 0x02, 0x03];
        let obus: Vec<_> = ObuIter::new(&unit).collect::<Result<Vec<_>>>().unwrap();
        assert_eq!(obus.len(), 1);
        assert_eq!(obus[0].payload, &[0x01, 0x02, 0x03]);
    }

    #[test]
    fn rejects_the_forbidden_bit_and_an_overlong_size() {
        let unit = [0b1011_0110u8, 0x00];
        let err = ObuIter::new(&unit).next().unwrap().unwrap_err();
        assert_eq!(err.static_message(), Some("AV1 OBU: forbidden bit set"));

        // forbidden=0, type=OBU_FRAME(6), ext=0, has_size=1: the size field claims 8 bytes but
        // only one follows.
        let unit = [0b0011_0010u8, 0x08, 0x00];
        let err = ObuIter::new(&unit).next().unwrap().unwrap_err();
        assert_eq!(
            err.static_message(),
            Some("AV1 OBU: size field runs past the end of the temporal unit")
        );
    }
}
