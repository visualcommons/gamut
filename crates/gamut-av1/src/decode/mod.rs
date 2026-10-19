//! AV1 still-image decoder (AV1 §5 parsing, §7 decoding process).
//!
//! The mirror of this crate's encoder, and the pure-Rust software tail behind
//! `gamut_avif::Av1StillDecoder`. Where the encoder chooses a subset of AV1's tools, a decoder
//! must accept whatever a conformant encoder produced — so these modules follow the spec's syntax
//! tables rather than the encoder's choices, and refuse, with a typed
//! [`Error::Unsupported`](gamut_core::Error::Unsupported), any tool that is signalled but not yet
//! implemented. A refusal is always explicit; nothing is approximated.
//!
//! Modules mirror the spec:
//!
//! - [`obu`] — OBU framing (§5.3) and the sequence header (§5.5).
//! - [`header`] — the uncompressed frame header (§5.9).
//! - [`tilegroup`] — the frame OBU and tile group framing (§5.10, §5.11.1).
//!
//! Reconstruction reuses the normative machinery the encoder already carries: `transform`'s
//! [`inverse_transform_2d`](crate::transform::inverse_transform_2d), `quant`'s
//! [`dequant`](crate::quant::dequant), the `cdf` tables, and `filter`'s in-loop filters.
//!
//! **Implemented scope.** `seq_profile = 1` (8-bit 4:4:4) intra key frames. The pixel-format
//! matrix (10/12-bit, 4:2:0/4:2:2, monochrome), intra block copy, and film grain are refused;
//! each is a ☐ row in `gamut-avif/STATUS.md`.

use gamut_bitstream::BitReader;
use gamut_core::{Error, Result};

pub(crate) mod header;
pub(crate) mod obu;
pub(crate) mod tilegroup;

/// Origin tag on every error the decoder raises.
pub(crate) const ORIGIN: &str = "gamut-av1";

pub use header::{
    CdefParams, FrameHeader, LoopFilterParams, LrParams, QuantizationParams, RestorationType,
    SegmentationParams, TileInfo, TxMode,
};
pub use obu::{ColorConfig, SequenceHeader, Subsampling};

/// Resource ceilings applied before any allocation sized by the bitstream.
///
/// A decoder is the format's attack surface: every buffer it allocates is sized by numbers an
/// attacker chose. These caps are checked against the *header* before a single sample is
/// allocated, so a hostile stream is refused rather than serviced. The defaults are generous for
/// real still images and far below what would exhaust a process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecodeLimits {
    /// Largest accepted luma width, in samples.
    pub max_width: u32,
    /// Largest accepted luma height, in samples.
    pub max_height: u32,
    /// Largest accepted luma sample count (`width * height`), bounding the total allocation
    /// independently of the aspect ratio.
    pub max_pixels: u64,
    /// Largest accepted number of tiles (`TileCols * TileRows`).
    pub max_tiles: usize,
}

impl Default for DecodeLimits {
    fn default() -> Self {
        Self {
            // AV1 level 6.0 tops out at 16384 in either dimension.
            max_width: 16384,
            max_height: 16384,
            // 256 Mpx: a 3-plane 8-bit frame held as u16 is then ~1.5 GiB at the very top end,
            // and typical stills are three orders of magnitude below it.
            max_pixels: 256 << 20,
            // MAX_TILE_ROWS * MAX_TILE_COLS (§3).
            max_tiles: 64 * 64,
        }
    }
}

impl DecodeLimits {
    /// Checks a frame's declared geometry against the caps.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidInput`] if the frame exceeds any cap.
    pub fn check(&self, width: u32, height: u32, tiles: usize) -> Result<()> {
        if width == 0 || height == 0 {
            return Err(Error::invalid_input(
                ORIGIN,
                "AV1 decode: frame has a zero dimension",
            ));
        }
        if width > self.max_width || height > self.max_height {
            return Err(Error::invalid_input(
                ORIGIN,
                "AV1 decode: frame dimensions exceed the configured limit",
            ));
        }
        if u64::from(width) * u64::from(height) > self.max_pixels {
            return Err(Error::invalid_input(
                ORIGIN,
                "AV1 decode: frame sample count exceeds the configured limit",
            ));
        }
        if tiles > self.max_tiles {
            return Err(Error::invalid_input(
                ORIGIN,
                "AV1 decode: tile count exceeds the configured limit",
            ));
        }
        Ok(())
    }
}

/// The headers of an AV1 still, without decoding any samples.
///
/// What a container needs to cross-check its own records against the codestream — the shape
/// `gamut-avif` re-derives `av1C`/`colr` from (AV1-ISOBMFF §2.3.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamInfo {
    /// The sequence header (§5.5.1).
    pub sequence: SequenceHeader,
    /// The frame header (§5.9.2).
    pub frame: FrameHeader,
    /// The number of tiles the frame's tile group carries.
    pub tile_count: usize,
}

/// The pure-Rust AV1 still-image decoder.
///
/// Reads one AV1 temporal unit — a sequence header plus an intra key frame. This is the software
/// tail behind `gamut_avif`'s decoder seam, and is usable standalone for a bare AV1 still
/// bitstream.
///
/// # Implemented scope
///
/// Today this decodes the **framing and header layer**: OBU walk (§5.3), the full sequence header
/// (§5.5), the full uncompressed frame header (§5.9), and tile-group framing (§5.11.1), surfaced
/// by [`Av1Decoder::inspect`]. Sample decoding — the tile body, reconstruction and the in-loop
/// filters — is not here yet and no entry point pretends otherwise; it arrives in the next slices
/// of issue #259.
///
/// Streams are accepted only at `seq_profile = 1` (8-bit 4:4:4) with intra key frames. Anything
/// else is refused with a typed [`Error::Unsupported`](gamut_core::Error::Unsupported) naming the
/// tool — never approximated. The remaining surface is tracked row by row in
/// `gamut-avif/STATUS.md`.
#[derive(Debug, Clone, Default)]
pub struct Av1Decoder {
    /// Resource ceilings applied before any bitstream-sized allocation.
    limits: DecodeLimits,
}

impl Av1Decoder {
    /// Creates a decoder with the default [`DecodeLimits`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a decoder with caller-chosen resource ceilings.
    #[must_use]
    pub const fn with_limits(limits: DecodeLimits) -> Self {
        Self { limits }
    }

    /// The ceilings this decoder enforces.
    #[must_use]
    pub const fn limits(&self) -> &DecodeLimits {
        &self.limits
    }

    /// Parses the sequence and frame headers of a still without decoding any samples.
    ///
    /// Cheap enough to run before committing to a decode: it walks the OBUs, reads both headers,
    /// applies this decoder's [`DecodeLimits`], refuses unimplemented tools, and validates the
    /// tile framing — but never touches a tile body. Use it to check a container's records
    /// against the codestream, or to learn a still's geometry and colour signalling up front.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidInput`](gamut_core::Error::InvalidInput) if the bitstream is
    /// malformed, truncated, or exceeds this decoder's [`DecodeLimits`], and
    /// [`Error::Unsupported`](gamut_core::Error::Unsupported) if it uses a coding tool that is
    /// not implemented.
    pub fn inspect(&self, temporal_unit: &[u8]) -> Result<StreamInfo> {
        let (sequence, frame, tiles) = self.parse_frame(temporal_unit)?;
        Ok(StreamInfo {
            sequence,
            frame,
            tile_count: tiles.len(),
        })
    }

    /// Walks the temporal unit and parses everything up to (but not including) the tile bodies.
    ///
    /// Backs [`Av1Decoder::inspect`], and is where sample decoding will attach once the tile
    /// body lands: it already produces the per-tile byte slices that step consumes.
    fn parse_frame<'a>(
        &self,
        temporal_unit: &'a [u8],
    ) -> Result<(SequenceHeader, FrameHeader, Vec<&'a [u8]>)> {
        let mut seq: Option<SequenceHeader> = None;
        let mut frame: Option<(FrameHeader, Vec<&'a [u8]>)> = None;

        for obu in obu::ObuIter::new(temporal_unit) {
            let obu = obu?;
            // §5.3.1: an OBU carrying an extension header is dropped when the chosen operating
            // point excludes its temporal or spatial layer. Sequence headers and temporal
            // delimiters are never dropped.
            if obu.has_extension
                && obu.kind != obu::OBU_SEQUENCE_HEADER
                && obu.kind != obu::OBU_TEMPORAL_DELIMITER
                && let Some(sh) = &seq
                && sh.operating_point_idc != 0
            {
                let in_temporal = (sh.operating_point_idc >> obu.temporal_id) & 1 != 0;
                let in_spatial = (sh.operating_point_idc >> (obu.spatial_id + 8)) & 1 != 0;
                if !in_temporal || !in_spatial {
                    continue;
                }
            }

            match obu.kind {
                obu::OBU_SEQUENCE_HEADER => {
                    let parsed = SequenceHeader::parse(obu.payload)?;
                    // A repeated sequence header must be identical (AV1 §7.5); a changed one would
                    // reinterpret the frame that follows.
                    if let Some(previous) = seq
                        && previous != parsed
                    {
                        return Err(Error::invalid_input(
                            ORIGIN,
                            "AV1 decode: repeated sequence header differs from the first",
                        ));
                    }
                    seq = Some(parsed);
                }
                obu::OBU_FRAME => {
                    let sh = seq.as_ref().ok_or_else(|| {
                        Error::invalid_input(
                            ORIGIN,
                            "AV1 decode: frame OBU before any sequence header",
                        )
                    })?;
                    if frame.is_some() {
                        return Err(Error::unsupported(
                            ORIGIN,
                            "AV1 decode: a still image must carry exactly one frame",
                        ));
                    }
                    // frame_obu (§5.10): the frame header, byte alignment, then the tile group.
                    let mut r = BitReader::new(obu.payload);
                    let fh = FrameHeader::parse(&mut r, sh)?;
                    r.byte_alignment()?;
                    self.validate(sh, &fh)?;
                    let tiles = tilegroup::split_tiles(r.remaining_bytes(), &fh.tile_info)?;
                    frame = Some((fh, tiles));
                }
                obu::OBU_FRAME_HEADER | obu::OBU_REDUNDANT_FRAME_HEADER => {
                    let sh = seq.as_ref().ok_or_else(|| {
                        Error::invalid_input(
                            ORIGIN,
                            "AV1 decode: frame header OBU before any sequence header",
                        )
                    })?;
                    if obu.kind == obu::OBU_REDUNDANT_FRAME_HEADER {
                        // A redundant copy repeats a header already seen; nothing new to decode.
                        continue;
                    }
                    if frame.is_some() {
                        return Err(Error::unsupported(
                            ORIGIN,
                            "AV1 decode: a still image must carry exactly one frame",
                        ));
                    }
                    let mut r = BitReader::new(obu.payload);
                    let fh = FrameHeader::parse(&mut r, sh)?;
                    // §5.3.1: the frame header OBU ends in trailing bits spanning the rest of the
                    // payload, so a header parsed at the wrong bit positions is refused here.
                    obu::obu_trailing_bits(&mut r)?;
                    self.validate(sh, &fh)?;
                    // The tiles arrive in a separate OBU_TILE_GROUP; record the header and wait.
                    frame = Some((fh, Vec::new()));
                }
                obu::OBU_TILE_GROUP => {
                    let (fh, tiles) = frame.as_mut().ok_or_else(|| {
                        Error::invalid_input(
                            ORIGIN,
                            "AV1 decode: tile group OBU before any frame header",
                        )
                    })?;
                    if !tiles.is_empty() {
                        return Err(Error::unsupported(
                            ORIGIN,
                            "AV1 decode: a still image must carry every tile in one tile group",
                        ));
                    }
                    *tiles = tilegroup::split_tiles(obu.payload, &fh.tile_info)?;
                }
                obu::OBU_TILE_LIST => {
                    return Err(Error::invalid_input(
                        ORIGIN,
                        "AV1 decode: tile list OBUs are forbidden in a still image",
                    ));
                }
                // Everything else is ignored per §5.3.1: temporal delimiters, metadata and
                // padding carry nothing this decoder needs, and reserved types "shall be ignored
                // by AV1 decoders". One arm, because a separate arm listing the named types would
                // be indistinguishable from this one.
                _ => {}
            }
        }

        let seq = seq.ok_or_else(|| {
            Error::invalid_input(
                ORIGIN,
                "AV1 decode: no sequence header in the temporal unit",
            )
        })?;
        let (frame, tiles) = frame.ok_or_else(|| {
            Error::invalid_input(ORIGIN, "AV1 decode: no frame in the temporal unit")
        })?;
        if tiles.is_empty() {
            return Err(Error::invalid_input(
                ORIGIN,
                "AV1 decode: frame header with no tile data",
            ));
        }
        Ok((seq, frame, tiles))
    }

    /// Applies the resource ceilings and the unsupported-tool refusals to a parsed header.
    fn validate(&self, seq: &SequenceHeader, frame: &FrameHeader) -> Result<()> {
        self.limits.check(
            frame.upscaled_width,
            frame.frame_height,
            frame.tile_info.tile_cols * frame.tile_info.tile_rows,
        )?;
        frame.reject_unsupported_tools(seq)
    }
}

/// Shared fixtures for the decode-side unit tests.
#[cfg(test)]
pub(crate) mod testutil {
    use gamut_bitstream::BitWriter;

    use crate::decode::header::tile_log2;
    use crate::headers::{Av1Colour, Av1StillConfig};

    /// The `color_config()` (§5.5.2) a [`StillBuilder`] codes. Every form is 8-bit; they differ in
    /// the chroma subsampling and the plane count, which several later syntax conditions read.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum BuilderColour {
        /// `seq_profile = 1` plus the §5.5.2 sRGB shortcut: 4:4:4, full range, nothing else coded.
        Srgb444,
        /// `seq_profile = 0` with an unspecified colour description: 4:2:0, so `subsampling_x`
        /// and `subsampling_y` are both 1.
        Yuv420,
        /// `seq_profile = 2` at 8 bits with an unspecified colour description: §5.5.2 infers
        /// 4:2:2 for any profile-2 depth but 12, so `subsampling_x` is 1 and `subsampling_y` 0.
        Yuv422,
        /// `seq_profile = 0` with `mono_chrome = 1`: one plane, so `NumPlanes` is 1.
        Monochrome,
    }

    impl BuilderColour {
        /// `seq_profile`.
        const fn seq_profile(self) -> u32 {
            match self {
                Self::Srgb444 => 1,
                Self::Yuv420 | Self::Monochrome => 0,
                Self::Yuv422 => 2,
            }
        }

        /// `(subsampling_x, subsampling_y)`.
        const fn subsampling(self) -> (u32, u32) {
            match self {
                Self::Srgb444 => (0, 0),
                Self::Yuv420 | Self::Monochrome => (1, 1),
                Self::Yuv422 => (1, 0),
            }
        }

        /// Whether `NumPlanes` is 3 rather than 1.
        const fn has_chroma(self) -> bool {
            !matches!(self, Self::Monochrome)
        }
    }

    /// The `lr_params()` (§5.9.20) a [`StillBuilder`] codes, for a frame that is not `AllLossless`.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) struct LrSpec {
        /// `lr_type` per plane, as coded: an index into `Remap_Lr_Type`, so 0 is `RESTORE_NONE`.
        pub types: [u32; 3],
        /// The combined `lr_unit_shift`, `0..=2`, coded as one or two bits. A sequence header with
        /// `use_128x128_superblock` codes `lr_unit_shift - 1`, so there it must be at least 1.
        pub unit_shift: u32,
        /// `lr_uv_shift`, coded only when the chroma is subsampled in both directions and a chroma
        /// plane uses restoration.
        pub uv_shift: u32,
    }

    /// The `film_grain_params()` (§5.9.30) a [`StillBuilder`] codes.
    ///
    /// Only the coded field *widths* matter: the decoder consumes the parameters and keeps nothing
    /// but `apply_grain`, so every point and coefficient is written as filler.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) struct GrainSpec {
        /// `num_y_points`.
        pub num_y_points: u32,
        /// `chroma_scaling_from_luma`.
        pub chroma_scaling_from_luma: bool,
        /// `num_cb_points`, coded only when §5.9.30's chroma-point condition holds.
        pub num_cb_points: u32,
        /// `num_cr_points`, coded alongside `num_cb_points`.
        pub num_cr_points: u32,
        /// `ar_coeff_lag`, which sets `numPosLuma = 2 * ar_coeff_lag * ( ar_coeff_lag + 1 )`.
        pub ar_coeff_lag: u32,
    }

    /// A hand-built AV1 still, for syntax paths neither `gamut-av1`'s encoder nor libaom emits.
    ///
    /// The encoder only ever writes the *reduced* still-picture header with a single 64×64-superblock
    /// tile, and libaom's all-intra usage writes the general header but with its own fixed choices.
    /// Several §5.5.1/§5.9.2 branches — `frame_size_override_flag`, an explicit render size,
    /// 128×128 superblocks, a multi-tile grid — are therefore unreachable from either. This builder
    /// is the inverse of the parser for exactly those branches, so they can be exercised directly.
    ///
    /// Everything not named here is pinned to the simplest legal value: profile 1 (8-bit 4:4:4),
    /// a shown `KEY_FRAME`, `base_q_idx = 0` (so `CodedLossless` holds and the in-loop filter
    /// blocks code no bits), and the §5.5.2 sRGB shortcut.
    #[derive(Debug, Clone, Copy)]
    pub(crate) struct StillBuilder {
        /// `max_frame_width_minus_1 + 1`.
        pub width: u32,
        /// `max_frame_height_minus_1 + 1`.
        pub height: u32,
        /// `frame_width_bits_minus_1 + 1`; may exceed the minimum the width needs.
        pub width_bits: u32,
        /// `frame_height_bits_minus_1 + 1`.
        pub height_bits: u32,
        /// `use_128x128_superblock`.
        pub use_128x128_superblock: bool,
        /// `operating_point_idc[0]`.
        pub operating_point_idc: u32,
        /// `frame_size_override_flag`; when set, the frame codes its own dimensions.
        pub frame_size_override: bool,
        /// The dimensions coded when `frame_size_override` is set.
        pub coded_size: (u32, u32),
        /// `render_and_frame_size_different`; when set, the render size is coded explicitly.
        pub render_size: Option<(u32, u32)>,
        /// How many `increment_tile_cols_log2` / `increment_tile_rows_log2` ones to emit.
        pub tile_cols_log2: u32,
        /// See [`tile_cols_log2`](Self::tile_cols_log2).
        pub tile_rows_log2: u32,
        /// `context_update_tile_id`, coded in `TileRowsLog2 + TileColsLog2` bits whenever the
        /// grid has more than one tile. It may name a tile past the grid, which the parser refuses.
        pub context_update_tile_id: u32,
        /// Explicit tile sizes in superblocks as `(per column, per row)`, coding
        /// `uniform_tile_spacing_flag = 0` and an `ns()` size for every tile. `None` codes the
        /// uniform branch from [`tile_cols_log2`](Self::tile_cols_log2) /
        /// [`tile_rows_log2`](Self::tile_rows_log2) instead. Neither encoder emits this branch.
        pub explicit_tiles: Option<(&'static [u32], &'static [u32])>,
        /// `enable_cdef` in the sequence header. A `CodedLossless` frame must still skip
        /// `cdef_params()` when this is set (§5.9.19).
        pub enable_cdef: bool,
        /// `cdef_bits`, coded when the frame is not `CodedLossless` and
        /// [`enable_cdef`](Self::enable_cdef) is set: `1 << cdef_bits` strength sets follow.
        pub cdef_bits: u32,
        /// `enable_restoration` in the sequence header. An `AllLossless` frame must still skip
        /// `lr_params()` when this is set (§5.9.20).
        pub enable_restoration: bool,
        /// Code an `INTRA_ONLY_FRAME` instead of a `KEY_FRAME`, which makes `error_resilient_mode`
        /// and `refresh_frame_flags` explicit (§5.9.2).
        pub intra_only: bool,
        /// `refresh_frame_flags`, coded only for an intra-only frame.
        pub refresh_frame_flags: u8,
        /// `frame_id_numbers_present_flag`, as
        /// `(additional_frame_id_length, delta_frame_id_length)`. Setting it makes the frame
        /// header code a `current_frame_id` of `additional + delta` bits (§5.9.2).
        pub frame_id_lengths: Option<(u32, u32)>,
        /// `seq_force_screen_content_tools`: `None` codes
        /// `seq_choose_screen_content_tools = 1` (SELECT, so the frame header codes
        /// `allow_screen_content_tools`), `Some(v)` codes the explicit value instead (§5.5.1).
        pub screen_content_tools: Option<u8>,
        /// `OrderHintBits`: `None` disables order hints, `Some(n)` codes
        /// `order_hint_bits_minus_1 = n - 1` and an `order_hint` field per frame.
        pub order_hint_bits: Option<u32>,
        /// `error_resilient_mode`, coded only for a frame that is not a shown key frame.
        pub error_resilient: bool,
        /// The `color_config()` (§5.5.2) the sequence header codes.
        pub colour: BuilderColour,
        /// `base_q_idx`. Any non-zero value clears `CodedLossless`, which is what makes
        /// `loop_filter_params()`, `cdef_params()`, `lr_params()` and `read_tx_mode()` code bits.
        pub base_q_idx: u8,
        /// `loop_filter_level[0..4]`. Levels 2 and 3 are coded only when the stream has chroma and
        /// at least one luma level is non-zero (§5.9.11).
        pub loop_filter_level: [u8; 4],
        /// `lr_params()` (§5.9.20), coded when the frame is not `AllLossless` and the sequence
        /// header set [`enable_restoration`](Self::enable_restoration).
        pub lr: LrSpec,
        /// `film_grain_params()` (§5.9.30). `Some` sets `film_grain_params_present` in the
        /// sequence header and codes `apply_grain = 1`; neither encoder ever emits film grain.
        pub film_grain: Option<GrainSpec>,
    }

    impl Default for StillBuilder {
        fn default() -> Self {
            Self {
                width: 64,
                height: 64,
                width_bits: 6,
                height_bits: 6,
                use_128x128_superblock: false,
                operating_point_idc: 0,
                frame_size_override: false,
                coded_size: (64, 64),
                render_size: None,
                tile_cols_log2: 0,
                tile_rows_log2: 0,
                context_update_tile_id: 0,
                explicit_tiles: None,
                enable_cdef: false,
                cdef_bits: 0,
                enable_restoration: false,
                intra_only: false,
                refresh_frame_flags: 0xff,
                frame_id_lengths: None,
                screen_content_tools: None,
                order_hint_bits: None,
                error_resilient: false,
                colour: BuilderColour::Srgb444,
                base_q_idx: 0,
                loop_filter_level: [0; 4],
                lr: LrSpec {
                    types: [0; 3],
                    unit_shift: 0,
                    uv_shift: 0,
                },
                film_grain: None,
            }
        }
    }

    impl StillBuilder {
        /// Emits the sequence header OBU payload (§5.5.1, general form).
        pub(crate) fn sequence_header(&self) -> Vec<u8> {
            let mut w = BitWriter::new();
            w.put_bits(self.colour.seq_profile(), 3);
            w.put_bit(0); // still_picture
            w.put_bit(0); // reduced_still_picture_header — the general form
            w.put_bit(0); // timing_info_present_flag
            w.put_bit(0); // initial_display_delay_present_flag
            w.put_bits(0, 5); // operating_points_cnt_minus_1
            w.put_bits(self.operating_point_idc, 12); // operating_point_idc[0]
            w.put_bits(0, 5); // seq_level_idx[0] (<= 7, so no seq_tier)
            w.put_bits(self.width_bits - 1, 4);
            w.put_bits(self.height_bits - 1, 4);
            w.put_bits(self.width - 1, self.width_bits);
            w.put_bits(self.height - 1, self.height_bits);
            match self.frame_id_lengths {
                Some((additional, delta)) => {
                    w.put_bit(1); // frame_id_numbers_present_flag
                    w.put_bits(delta - 2, 4); // delta_frame_id_length_minus_2
                    w.put_bits(additional - 1, 3); // additional_frame_id_length_minus_1
                }
                None => w.put_bit(0),
            }
            w.put_bit(u8::from(self.use_128x128_superblock));
            w.put_bit(0); // enable_filter_intra
            w.put_bit(0); // enable_intra_edge_filter
            w.put_bit(0); // enable_interintra_compound
            w.put_bit(0); // enable_masked_compound
            w.put_bit(0); // enable_warped_motion
            w.put_bit(0); // enable_dual_filter
            match self.order_hint_bits {
                Some(_) => {
                    w.put_bit(1); // enable_order_hint
                    w.put_bit(0); // enable_jnt_comp
                    w.put_bit(0); // enable_ref_frame_mvs
                }
                None => w.put_bit(0),
            }
            match self.screen_content_tools {
                None => {
                    w.put_bit(1); // seq_choose_screen_content_tools -> SELECT (> 0)
                    w.put_bit(1); // seq_choose_integer_mv -> SELECT
                }
                Some(force) => {
                    w.put_bit(0); // seq_choose_screen_content_tools
                    w.put_bits(u32::from(force), 1); // seq_force_screen_content_tools
                    if force > 0 {
                        w.put_bit(1); // seq_choose_integer_mv -> SELECT
                    }
                }
            }
            if let Some(bits) = self.order_hint_bits {
                w.put_bits(bits - 1, 3); // order_hint_bits_minus_1
            }
            w.put_bit(0); // enable_superres
            w.put_bit(u8::from(self.enable_cdef));
            w.put_bit(u8::from(self.enable_restoration));
            // color_config() (§5.5.2), 8-bit either way.
            w.put_bit(0); // high_bitdepth
            match self.colour {
                BuilderColour::Srgb444 => {
                    // seq_profile 1 infers mono_chrome = 0, and the sRGB triple then infers full
                    // range and 4:4:4 without coding another bit.
                    w.put_bit(1); // color_description_present_flag
                    w.put_bits(1, 8); // color_primaries = CP_BT_709
                    w.put_bits(13, 8); // transfer_characteristics = TC_SRGB
                    w.put_bits(0, 8); // matrix_coefficients = MC_IDENTITY
                }
                BuilderColour::Yuv420 => {
                    w.put_bit(0); // mono_chrome
                    w.put_bit(0); // color_description_present_flag -> all three unspecified
                    w.put_bit(0); // color_range
                    // seq_profile 0 infers subsampling_x = subsampling_y = 1, which is what makes
                    // chroma_sample_position coded.
                    w.put_bits(0, 2); // chroma_sample_position = CSP_UNKNOWN
                }
                BuilderColour::Yuv422 => {
                    w.put_bit(0); // mono_chrome
                    w.put_bit(0); // color_description_present_flag -> all three unspecified
                    w.put_bit(0); // color_range
                    // An 8-bit seq_profile 2 infers 4:2:2 and codes no subsampling bits; only
                    // 4:2:0 codes chroma_sample_position.
                }
                BuilderColour::Monochrome => {
                    w.put_bit(1); // mono_chrome
                    w.put_bit(0); // color_description_present_flag -> all three unspecified
                    w.put_bit(0); // color_range
                }
            }
            if self.colour.has_chroma() {
                // A monochrome color_config() returns straight after color_range.
                w.put_bit(0); // separate_uv_delta_q
            }
            w.put_bit(u8::from(self.film_grain.is_some())); // film_grain_params_present
            w.put_bit(1); // trailing_bits
            w.byte_align();
            w.into_bytes()
        }

        /// Emits the frame OBU payload: the uncompressed header (§5.9.2) then one byte of tile data.
        pub(crate) fn frame_obu(&self) -> Vec<u8> {
            self.frame_obu_with_header_bits().0
        }

        /// [`frame_obu`](Self::frame_obu) plus the exact bit length of the uncompressed header.
        ///
        /// A parser that reads a field at the wrong width still returns a plausible-looking header
        /// when the field's *value* is discarded — film grain is nothing but such fields. Comparing
        /// the reader's final bit position against this is what catches that.
        pub(crate) fn frame_obu_with_header_bits(&self) -> (Vec<u8>, usize) {
            let mut w = BitWriter::new();
            w.put_bit(0); // show_existing_frame
            w.put_bits(if self.intra_only { 2 } else { 0 }, 2); // frame_type
            w.put_bit(1); // show_frame
            if self.intra_only {
                // showable_frame is inferred from show_frame; error_resilient_mode is coded for
                // anything but a shown key frame.
                w.put_bit(u8::from(self.error_resilient));
            }
            w.put_bit(0); // disable_cdf_update
            // allow_screen_content_tools is coded only when the sequence header chose SELECT.
            if self.screen_content_tools.is_none() {
                w.put_bit(0); // allow_screen_content_tools
            } else if self.screen_content_tools == Some(1) {
                // seq_force_screen_content_tools = 1 infers allow_screen_content_tools = 1, and
                // seq_force_integer_mv is SELECT, so force_integer_mv is coded.
                w.put_bit(0); // force_integer_mv
            }
            if let Some((additional, delta)) = self.frame_id_lengths {
                // current_frame_id is idLen = additional_frame_id_length + delta_frame_id_length
                // bits wide (§5.9.2).
                w.put_bits(0, additional + delta);
            }
            w.put_bit(u8::from(self.frame_size_override));
            // primary_ref_frame is inferred PRIMARY_REF_NONE for an intra frame.
            if let Some(bits) = self.order_hint_bits {
                w.put_bits(0, bits); // order_hint
            }
            if self.intra_only {
                // A shown key frame infers refresh_frame_flags = allFrames; an intra-only frame
                // codes it.
                w.put_bits(u32::from(self.refresh_frame_flags), 8);
                // §5.9.2 codes ref_order_hint only for a partial refresh of an error-resilient
                // frame with order hints enabled.
                if self.refresh_frame_flags != 0xff
                    && self.error_resilient
                    && let Some(bits) = self.order_hint_bits
                {
                    for _ in 0..8 {
                        w.put_bits(0, bits); // ref_order_hint[i]
                    }
                }
            }
            if self.frame_size_override {
                w.put_bits(self.coded_size.0 - 1, self.width_bits);
                w.put_bits(self.coded_size.1 - 1, self.height_bits);
            }
            // superres_params codes nothing: enable_superres = 0.
            match self.render_size {
                Some((rw, rh)) => {
                    w.put_bit(1); // render_and_frame_size_different
                    w.put_bits(rw - 1, 16);
                    w.put_bits(rh - 1, 16);
                }
                None => w.put_bit(0),
            }
            // §5.9.2 codes allow_intrabc when screen-content tools are on and superres is not
            // scaling the frame. `allow_screen_content_tools` is only ever 1 here through an
            // explicit `seq_force_screen_content_tools = 1`.
            if self.screen_content_tools == Some(1) {
                w.put_bit(0); // allow_intrabc
            }
            w.put_bit(1); // disable_frame_end_update_cdf (coded: general header, updates enabled)
            self.tile_info(&mut w);
            // quantization_params() (§5.9.12): no DeltaQ*, no quantizer matrix.
            w.put_bits(u32::from(self.base_q_idx), 8);
            w.put_bit(0); // DeltaQYDc delta_coded
            if self.colour.has_chroma() {
                // separate_uv_delta_q is 0, so diff_uv_delta is inferred and V shares U's deltas.
                w.put_bit(0); // DeltaQUDc delta_coded
                w.put_bit(0); // DeltaQUAc delta_coded
            }
            w.put_bit(0); // using_qmatrix
            w.put_bit(0); // segmentation_enabled
            // With no segment deltas and no DeltaQ*, CodedLossless is exactly `base_q_idx == 0` —
            // and then delta_q_params(), the three in-loop filter blocks and read_tx_mode() all
            // code nothing.
            if self.base_q_idx != 0 {
                w.put_bit(0); // delta_q_present
                self.loop_filter_params(&mut w);
                if self.enable_cdef {
                    self.cdef_params(&mut w);
                }
                if self.enable_restoration {
                    self.lr_params(&mut w);
                }
                w.put_bit(0); // tx_mode_select = 0 -> TX_MODE_LARGEST
            }
            w.put_bit(1); // reduced_tx_set
            if let Some(grain) = self.film_grain {
                self.film_grain_params(&mut w, grain);
            }
            let header_bits = w.bit_len();
            w.byte_align(); // byte_alignment() before the tile group
            let mut out = w.into_bytes();
            // One tile group carrying a single byte of (never-decoded) tile data per tile.
            let tiles = match self.explicit_tiles {
                Some((cols, rows)) => cols.len() * rows.len(),
                None => (1usize << self.tile_cols_log2) * (1usize << self.tile_rows_log2),
            };
            if tiles > 1 {
                out.push(0); // tile_start_and_end_present_flag = 0, then byte alignment
                for _ in 0..tiles - 1 {
                    out.push(0); // tile_size_minus_1, one byte (TileSizeBytes = 1)
                    out.push(0xaa); // the tile's single byte
                }
            }
            out.push(0xaa); // the last tile takes the remainder
            (out, header_bits)
        }

        /// `loop_filter_params()` (§5.9.11) for a frame that is not `CodedLossless`.
        fn loop_filter_params(&self, w: &mut BitWriter) {
            w.put_bits(u32::from(self.loop_filter_level[0]), 6);
            w.put_bits(u32::from(self.loop_filter_level[1]), 6);
            if self.colour.has_chroma()
                && (self.loop_filter_level[0] != 0 || self.loop_filter_level[1] != 0)
            {
                w.put_bits(u32::from(self.loop_filter_level[2]), 6);
                w.put_bits(u32::from(self.loop_filter_level[3]), 6);
            }
            w.put_bits(0, 3); // loop_filter_sharpness
            w.put_bit(0); // loop_filter_delta_enabled
        }

        /// `cdef_params()` (§5.9.19): `1 << cdef_bits` strength sets, each strength set to its
        /// index plus one so a set read twice or skipped shows in the parsed values.
        fn cdef_params(&self, w: &mut BitWriter) {
            w.put_bits(0, 2); // cdef_damping_minus_3
            w.put_bits(self.cdef_bits, 2);
            for i in 0..(1u32 << self.cdef_bits) {
                w.put_bits(i + 1, 4); // cdef_y_pri_strength[i]
                w.put_bits(0, 2); // cdef_y_sec_strength[i]
                if self.colour.has_chroma() {
                    w.put_bits(i + 1, 4); // cdef_uv_pri_strength[i]
                    w.put_bits(0, 2); // cdef_uv_sec_strength[i]
                }
            }
        }

        /// `lr_params()` (§5.9.20) for a frame that is not `AllLossless`.
        fn lr_params(&self, w: &mut BitWriter) {
            let mut uses_lr = false;
            let mut uses_chroma_lr = false;
            let planes = if self.colour.has_chroma() { 3 } else { 1 };
            for (plane, &kind) in self.lr.types.iter().take(planes).enumerate() {
                w.put_bits(kind, 2); // lr_type
                if kind != 0 {
                    uses_lr = true;
                    if plane > 0 {
                        uses_chroma_lr = true;
                    }
                }
            }
            if !uses_lr {
                return;
            }
            if self.use_128x128_superblock {
                // The parser adds the implicit 1 back.
                w.put_bits(self.lr.unit_shift - 1, 1); // lr_unit_shift
            } else if self.lr.unit_shift == 0 {
                w.put_bit(0); // lr_unit_shift
            } else {
                w.put_bit(1); // lr_unit_shift
                w.put_bits(self.lr.unit_shift - 1, 1); // lr_unit_extra_shift
            }
            let (ssx, ssy) = self.colour.subsampling();
            if ssx == 1 && ssy == 1 && uses_chroma_lr {
                w.put_bits(self.lr.uv_shift, 1); // lr_uv_shift
            }
        }

        /// `film_grain_params()` (§5.9.30) with `apply_grain = 1`.
        fn film_grain_params(&self, w: &mut BitWriter, grain: GrainSpec) {
            w.put_bit(1); // apply_grain
            w.put_bits(0xabcd, 16); // grain_seed
            // update_grain is inferred 1: this is never an INTER_FRAME.
            w.put_bits(grain.num_y_points, 4);
            for _ in 0..grain.num_y_points {
                w.put_bits(0, 8); // point_y_value
                w.put_bits(0, 8); // point_y_scaling
            }
            let mono = !self.colour.has_chroma();
            if !mono {
                w.put_bit(u8::from(grain.chroma_scaling_from_luma));
            }
            let (ssx, ssy) = self.colour.subsampling();
            let codes_chroma_points = !mono
                && !grain.chroma_scaling_from_luma
                && !(ssx == 1 && ssy == 1 && grain.num_y_points == 0);
            let (num_cb_points, num_cr_points) = if codes_chroma_points {
                w.put_bits(grain.num_cb_points, 4);
                for _ in 0..grain.num_cb_points {
                    w.put_bits(0, 8); // point_cb_value
                    w.put_bits(0, 8); // point_cb_scaling
                }
                w.put_bits(grain.num_cr_points, 4);
                for _ in 0..grain.num_cr_points {
                    w.put_bits(0, 8); // point_cr_value
                    w.put_bits(0, 8); // point_cr_scaling
                }
                (grain.num_cb_points, grain.num_cr_points)
            } else {
                // §5.9.30 forces both counts to zero and codes neither field.
                (0, 0)
            };
            w.put_bits(0, 2); // grain_scaling_minus_8
            w.put_bits(grain.ar_coeff_lag, 2);
            let num_pos_luma = 2 * grain.ar_coeff_lag * (grain.ar_coeff_lag + 1);
            let num_pos_chroma = if grain.num_y_points > 0 {
                for _ in 0..num_pos_luma {
                    w.put_bits(0, 8); // ar_coeffs_y_plus_128[i]
                }
                num_pos_luma + 1
            } else {
                num_pos_luma
            };
            if grain.chroma_scaling_from_luma || num_cb_points > 0 {
                for _ in 0..num_pos_chroma {
                    w.put_bits(0, 8); // ar_coeffs_cb_plus_128[i]
                }
            }
            if grain.chroma_scaling_from_luma || num_cr_points > 0 {
                for _ in 0..num_pos_chroma {
                    w.put_bits(0, 8); // ar_coeffs_cr_plus_128[i]
                }
            }
            w.put_bits(0, 2); // ar_coeff_shift_minus_6
            w.put_bits(0, 2); // grain_scale_shift
            if num_cb_points > 0 {
                w.put_bits(0, 8); // cb_mult
                w.put_bits(0, 8); // cb_luma_mult
                w.put_bits(0, 9); // cb_offset
            }
            if num_cr_points > 0 {
                w.put_bits(0, 8); // cr_mult
                w.put_bits(0, 8); // cr_luma_mult
                w.put_bits(0, 9); // cr_offset
            }
            w.put_bit(0); // overlap_flag
            w.put_bit(0); // clip_to_restricted_range
        }

        /// `tile_info()` (§5.9.15), uniform spacing, emitting the requested log2 increments.
        fn tile_info(&self, w: &mut BitWriter) {
            let (sb_shift, mi_cols, mi_rows) = {
                let coded = if self.frame_size_override {
                    self.coded_size
                } else {
                    (self.width, self.height)
                };
                let shift = if self.use_128x128_superblock { 5 } else { 4 };
                (shift, 2 * ((coded.0 + 7) >> 3), 2 * ((coded.1 + 7) >> 3))
            };
            let sb_cols = if self.use_128x128_superblock {
                (mi_cols + 31) >> 5
            } else {
                (mi_cols + 15) >> 4
            };
            let sb_rows = if self.use_128x128_superblock {
                (mi_rows + 31) >> 5
            } else {
                (mi_rows + 15) >> 4
            };
            let _ = sb_shift;
            let sb_size = if self.use_128x128_superblock { 7 } else { 6 };
            let max_tile_width_sb = 4096u32 >> sb_size;
            let max_tile_area_sb = (4096u32 * 2304) >> (2 * sb_size);
            let min_log2_tile_cols = tile_log2(max_tile_width_sb, sb_cols);
            let max_log2_tile_cols = tile_log2(1, sb_cols.min(64));
            let max_log2_tile_rows = tile_log2(1, sb_rows.min(64));
            let min_log2_tiles =
                min_log2_tile_cols.max(tile_log2(max_tile_area_sb, sb_rows * sb_cols));

            if let Some((cols, rows)) = self.explicit_tiles {
                w.put_bit(0); // uniform_tile_spacing_flag
                let mut start_sb = 0;
                for &size_sb in cols {
                    let max_width = (sb_cols - start_sb).min(max_tile_width_sb);
                    put_ns(w, size_sb - 1, max_width); // width_in_sbs_minus_1
                    start_sb += size_sb;
                }
                // §5.9.15 recomputes maxTileAreaSb from the columns just coded before the rows.
                let widest_tile_sb = cols.iter().copied().max().unwrap_or(1);
                let area_sb = if min_log2_tiles > 0 {
                    (sb_rows * sb_cols) >> (min_log2_tiles + 1)
                } else {
                    sb_rows * sb_cols
                };
                let max_tile_height_sb = (area_sb / widest_tile_sb.max(1)).max(1);
                let mut start_sb = 0;
                for &size_sb in rows {
                    let max_height = (sb_rows - start_sb).min(max_tile_height_sb);
                    put_ns(w, size_sb - 1, max_height); // height_in_sbs_minus_1
                    start_sb += size_sb;
                }
                let cols_log2 = tile_log2(1, cols.len() as u32);
                let rows_log2 = tile_log2(1, rows.len() as u32);
                if cols_log2 > 0 || rows_log2 > 0 {
                    w.put_bits(self.context_update_tile_id, rows_log2 + cols_log2);
                    w.put_bits(0, 2); // tile_size_bytes_minus_1 => TileSizeBytes = 1
                }
                return;
            }

            w.put_bit(1); // uniform_tile_spacing_flag
            let mut cols_log2 = min_log2_tile_cols;
            while cols_log2 < max_log2_tile_cols {
                let inc = cols_log2 < self.tile_cols_log2;
                w.put_bit(u8::from(inc));
                if !inc {
                    break;
                }
                cols_log2 += 1;
            }
            let min_log2_tile_rows = min_log2_tiles.saturating_sub(cols_log2);
            let mut rows_log2 = min_log2_tile_rows;
            while rows_log2 < max_log2_tile_rows {
                let inc = rows_log2 < self.tile_rows_log2;
                w.put_bit(u8::from(inc));
                if !inc {
                    break;
                }
                rows_log2 += 1;
            }
            if cols_log2 > 0 || rows_log2 > 0 {
                w.put_bits(self.context_update_tile_id, rows_log2 + cols_log2);
                w.put_bits(0, 2); // tile_size_bytes_minus_1 => TileSizeBytes = 1
            }
        }

        /// The complete temporal unit: sequence header OBU + frame OBU, each size-prefixed.
        pub(crate) fn temporal_unit(&self) -> Vec<u8> {
            let mut out = Vec::new();
            write_obu(&mut out, 1, &self.sequence_header()); // OBU_SEQUENCE_HEADER
            write_obu(&mut out, 6, &self.frame_obu()); // OBU_FRAME
            out
        }
    }

    /// Writes the `ns(n)` descriptor (AV1 §4.10.7) — the inverse of `BitReader::ns`.
    fn put_ns(w: &mut BitWriter, value: u32, n: u32) {
        let width = 32 - n.leading_zeros(); // FloorLog2(n) + 1
        let m = (1u32 << width) - n;
        if value < m {
            w.put_bits(value, width - 1);
        } else {
            // The reader recovers `(v << 1) - m + extra`, so split `value + m` into its high
            // bits and its low one.
            let x = value + m;
            w.put_bits(x >> 1, width - 1);
            w.put_bits(x & 1, 1);
        }
    }

    /// Writes one OBU with `obu_has_size_field = 1` (§5.3.2).
    pub(crate) fn write_obu(out: &mut Vec<u8>, obu_type: u8, payload: &[u8]) {
        out.push((obu_type << 3) + 0b10);
        gamut_bitstream::write_leb128(out, payload.len() as u64);
        out.extend_from_slice(payload);
    }

    /// Builds the [`Av1StillConfig`] the encoder derives for `colour` at this size, so the
    /// parsers here are checked against the exact headers `gamut-av1` emits.
    pub(crate) fn still_config(width: u32, height: u32, colour: Av1Colour) -> Av1StillConfig {
        use gamut_color::cicp::ColorRange;
        Av1StillConfig {
            seq_profile: 1,
            seq_level_idx_0: crate::headers::pick_level(width, height).unwrap(),
            seq_tier_0: 0,
            high_bitdepth: false,
            twelve_bit: false,
            monochrome: false,
            chroma_subsampling_x: 0,
            chroma_subsampling_y: 0,
            chroma_sample_position: 0,
            color_primaries: colour.primaries.code_point(),
            transfer_characteristics: colour.transfer.code_point(),
            matrix_coefficients: colour.matrix.code_point(),
            full_range: matches!(colour.range, ColorRange::Full),
        }
    }

    /// Builds the sequence-header OBU payload the encoder emits, so the parser is checked against
    /// the writer it mirrors.
    pub(crate) fn encoder_seq_header(
        width: u32,
        height: u32,
        lossy: bool,
        superres: bool,
    ) -> Vec<u8> {
        let cfg = still_config(width, height, Av1Colour::default());
        crate::headers::sequence_header_payload(&cfg, width, height, lossy, superres)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_reject_degenerate_and_oversized_frames() {
        let limits = DecodeLimits::default();
        limits.check(1920, 1080, 1).unwrap();

        assert_eq!(
            limits.check(0, 16, 1).unwrap_err().static_message(),
            Some("AV1 decode: frame has a zero dimension")
        );
        assert_eq!(
            limits.check(16, 0, 1).unwrap_err().static_message(),
            Some("AV1 decode: frame has a zero dimension")
        );
        assert_eq!(
            limits.check(16385, 16, 1).unwrap_err().static_message(),
            Some("AV1 decode: frame dimensions exceed the configured limit")
        );
        assert_eq!(
            limits.check(16, 16385, 1).unwrap_err().static_message(),
            Some("AV1 decode: frame dimensions exceed the configured limit")
        );
        assert_eq!(
            limits.check(16, 16, 4097).unwrap_err().static_message(),
            Some("AV1 decode: tile count exceeds the configured limit")
        );
    }

    #[test]
    fn limits_are_inclusive_at_their_boundary() {
        // Each cap admits its exact value and refuses one more — pinning `>` against `>=`.
        let limits = DecodeLimits {
            max_width: 64,
            max_height: 32,
            max_pixels: 64 * 32,
            max_tiles: 4,
        };
        limits.check(64, 32, 4).unwrap();
        assert_eq!(
            limits.check(65, 32, 4).unwrap_err().static_message(),
            Some("AV1 decode: frame dimensions exceed the configured limit")
        );
        assert_eq!(
            limits.check(64, 33, 4).unwrap_err().static_message(),
            Some("AV1 decode: frame dimensions exceed the configured limit")
        );
        assert_eq!(
            limits.check(64, 32, 5).unwrap_err().static_message(),
            Some("AV1 decode: tile count exceeds the configured limit")
        );
        // The sample cap is exact too: 64x32 fits, 65x32 would exceed it even if the dimension
        // caps allowed it.
        let wide = DecodeLimits {
            max_width: 4096,
            max_pixels: 64 * 32,
            ..limits
        };
        wide.check(64, 32, 4).unwrap();
        assert_eq!(
            wide.check(65, 32, 4).unwrap_err().static_message(),
            Some("AV1 decode: frame sample count exceeds the configured limit")
        );
    }

    #[test]
    fn the_tile_cap_counts_columns_times_rows() {
        // A 2x2 grid is four tiles, not two: the cap must see the product.
        let still = testutil::StillBuilder {
            width: 512,
            height: 512,
            width_bits: 10,
            height_bits: 10,
            tile_cols_log2: 1,
            tile_rows_log2: 1,
            ..testutil::StillBuilder::default()
        };
        let unit = still.temporal_unit();

        let info = Av1Decoder::new().inspect(&unit).unwrap();
        assert_eq!(info.frame.tile_info.tile_cols, 2);
        assert_eq!(info.frame.tile_info.tile_rows, 2);
        assert_eq!(info.tile_count, 4);

        let capped = Av1Decoder::with_limits(DecodeLimits {
            max_tiles: 2,
            ..DecodeLimits::default()
        });
        assert_eq!(
            capped.inspect(&unit).unwrap_err().static_message(),
            Some("AV1 decode: tile count exceeds the configured limit")
        );
        // Four is exactly the grid, so it is admitted.
        let exact = Av1Decoder::with_limits(DecodeLimits {
            max_tiles: 4,
            ..DecodeLimits::default()
        });
        exact.inspect(&unit).unwrap();

        // A 4x2 grid is eight tiles; their *sum* is six. A cap of seven must refuse it, which a
        // sum would wrongly admit.
        let rect = testutil::StillBuilder {
            width: 1024,
            height: 1024,
            width_bits: 11,
            height_bits: 11,
            tile_cols_log2: 2,
            tile_rows_log2: 1,
            ..testutil::StillBuilder::default()
        };
        let rect_unit = rect.temporal_unit();
        let info = Av1Decoder::new().inspect(&rect_unit).unwrap();
        assert_eq!(
            (
                info.frame.tile_info.tile_cols,
                info.frame.tile_info.tile_rows
            ),
            (4, 2)
        );
        assert_eq!(info.tile_count, 8);
        let seven = Av1Decoder::with_limits(DecodeLimits {
            max_tiles: 7,
            ..DecodeLimits::default()
        });
        assert_eq!(
            seven.inspect(&rect_unit).unwrap_err().static_message(),
            Some("AV1 decode: tile count exceeds the configured limit")
        );
    }

    #[test]
    fn a_repeated_sequence_header_must_be_identical() {
        // AV1 §7.5: a sequence header may be repeated, but a *changed* one would reinterpret the
        // frame that follows, so it is refused rather than silently adopted.
        let still = testutil::StillBuilder::default();
        let mut repeated = Vec::new();
        testutil::write_obu(&mut repeated, 1, &still.sequence_header());
        testutil::write_obu(&mut repeated, 1, &still.sequence_header());
        testutil::write_obu(&mut repeated, 6, &still.frame_obu());
        Av1Decoder::new()
            .inspect(&repeated)
            .expect("an identical repeat is legal");

        let other = testutil::StillBuilder {
            width: 128,
            width_bits: 8,
            ..testutil::StillBuilder::default()
        };
        let mut changed = Vec::new();
        testutil::write_obu(&mut changed, 1, &still.sequence_header());
        testutil::write_obu(&mut changed, 1, &other.sequence_header());
        testutil::write_obu(&mut changed, 6, &still.frame_obu());
        assert_eq!(
            Av1Decoder::new()
                .inspect(&changed)
                .unwrap_err()
                .static_message(),
            Some("AV1 decode: repeated sequence header differs from the first")
        );
    }

    #[test]
    fn obus_outside_the_chosen_operating_point_are_dropped() {
        // OperatingPointIdc bit `temporal_id` selects the temporal layer and bit `spatial_id + 8`
        // the spatial one (§5.3.1). With idc = 0x101 only temporal 0 / spatial 0 is in the
        // operating point, so a frame tagged temporal 1 is dropped and the unit has no frame.
        let still = testutil::StillBuilder {
            operating_point_idc: 0x101,
            ..testutil::StillBuilder::default()
        };
        let seq = still.sequence_header();
        let frame = still.frame_obu();

        /// Emits a frame OBU carrying an extension header with the given ids.
        fn frame_with_extension(out: &mut Vec<u8>, temporal: u8, spatial: u8, payload: &[u8]) {
            // forbidden=0, type=OBU_FRAME(6), extension=1, has_size=1, reserved=0.
            out.push((6 << 3) | 0b100 | 0b10);
            out.push((temporal << 5) | (spatial << 3));
            gamut_bitstream::write_leb128(out, payload.len() as u64);
            out.extend_from_slice(payload);
        }

        let mut inside = Vec::new();
        testutil::write_obu(&mut inside, 1, &seq);
        frame_with_extension(&mut inside, 0, 0, &frame);
        Av1Decoder::new()
            .inspect(&inside)
            .expect("temporal 0 / spatial 0 is inside operating point 0x101");

        for (temporal, spatial) in [(1u8, 0u8), (0, 1)] {
            let mut outside = Vec::new();
            testutil::write_obu(&mut outside, 1, &seq);
            frame_with_extension(&mut outside, temporal, spatial, &frame);
            assert_eq!(
                Av1Decoder::new()
                    .inspect(&outside)
                    .unwrap_err()
                    .static_message(),
                Some("AV1 decode: no frame in the temporal unit"),
                "temporal {temporal} / spatial {spatial} must be dropped"
            );
        }

        // idc 0x202 selects temporal layer 1 and spatial layer 1, so the shift direction is
        // observable: `idc >> 1` keeps the frame, `idc << 1` would drop it.
        let shifted = testutil::StillBuilder {
            operating_point_idc: 0x202,
            ..testutil::StillBuilder::default()
        };
        let mut inside_upper = Vec::new();
        testutil::write_obu(&mut inside_upper, 1, &shifted.sequence_header());
        frame_with_extension(&mut inside_upper, 1, 1, &shifted.frame_obu());
        Av1Decoder::new()
            .inspect(&inside_upper)
            .expect("temporal 1 / spatial 1 is inside operating point 0x202");
        // Layer 0 is *outside* that operating point, which the previous case cannot show.
        let mut outside_lower = Vec::new();
        testutil::write_obu(&mut outside_lower, 1, &shifted.sequence_header());
        frame_with_extension(&mut outside_lower, 0, 0, &shifted.frame_obu());
        assert_eq!(
            Av1Decoder::new()
                .inspect(&outside_lower)
                .unwrap_err()
                .static_message(),
            Some("AV1 decode: no frame in the temporal unit")
        );

        // With idc = 0 no OBU is ever dropped, whatever its layer ids.
        let open = testutil::StillBuilder::default();
        let mut all_layers = Vec::new();
        testutil::write_obu(&mut all_layers, 1, &open.sequence_header());
        frame_with_extension(&mut all_layers, 3, 2, &open.frame_obu());
        Av1Decoder::new()
            .inspect(&all_layers)
            .expect("OperatingPointIdc 0 keeps every layer");
    }

    #[test]
    fn a_tile_list_obu_is_refused() {
        // Large-scale tile lists are forbidden in a still image (AV1-ISOBMFF §2.4); ignoring one
        // would silently decode a different picture than the stream describes.
        let still = testutil::StillBuilder::default();
        let mut unit = Vec::new();
        testutil::write_obu(&mut unit, 1, &still.sequence_header());
        testutil::write_obu(&mut unit, 8, &[0u8; 4]); // OBU_TILE_LIST
        testutil::write_obu(&mut unit, 6, &still.frame_obu());
        assert_eq!(
            Av1Decoder::new()
                .inspect(&unit)
                .unwrap_err()
                .static_message(),
            Some("AV1 decode: tile list OBUs are forbidden in a still image")
        );
    }

    #[test]
    fn a_separate_frame_header_and_tile_group_decode_together() {
        // §5.9/§5.11: the frame header may arrive in its own OBU with the tiles following in an
        // OBU_TILE_GROUP, instead of being fused into one OBU_FRAME.
        let still = testutil::StillBuilder::default();
        let frame = still.frame_obu();
        // The builder's frame OBU is the header (byte-aligned) followed by one tile byte; a
        // standalone frame header OBU ends with trailing_bits instead.
        let (header, tiles) = frame.split_at(frame.len() - 1);
        let mut header_obu = header.to_vec();
        header_obu.push(0x80); // trailing_bits: a one bit then zero padding

        let mut unit = Vec::new();
        testutil::write_obu(&mut unit, 1, &still.sequence_header());
        testutil::write_obu(&mut unit, 3, &header_obu); // OBU_FRAME_HEADER
        testutil::write_obu(&mut unit, 4, tiles); // OBU_TILE_GROUP
        let info = Av1Decoder::new()
            .inspect(&unit)
            .expect("a split frame header and tile group is a valid still");
        assert_eq!(info.tile_count, 1);
        assert_eq!(info.frame.upscaled_width, 64);

        // A second tile group would mean a partially-coded frame.
        let mut twice = unit.clone();
        testutil::write_obu(&mut twice, 4, tiles);
        assert_eq!(
            Av1Decoder::new()
                .inspect(&twice)
                .unwrap_err()
                .static_message(),
            Some("AV1 decode: a still image must carry every tile in one tile group")
        );
    }

    #[test]
    fn a_frame_header_obu_must_end_in_the_obus_trailing_bits() {
        // §5.3.4's `nbBits` spans whole bytes to the end of the OBU, while
        // `BitReader::trailing_bits` stops at the next byte boundary — so the padding *after* that
        // boundary needs checking too, or a frame header that consumed the wrong number of bits
        // still passes.
        let still = testutil::StillBuilder::default();
        let frame = still.frame_obu();
        let (header, tiles) = frame.split_at(frame.len() - 1);

        let mut header_obu = header.to_vec();
        header_obu.push(0x80); // trailing_bits: a one bit then zero padding
        let mut good = Vec::new();
        testutil::write_obu(&mut good, 1, &still.sequence_header());
        testutil::write_obu(&mut good, 3, &header_obu);
        testutil::write_obu(&mut good, 4, tiles);
        Av1Decoder::new()
            .inspect(&good)
            .expect("well-formed padding");

        // Zero padding to the end of the OBU is legal; a set bit in it is not.
        let mut zero_padded = header_obu.clone();
        zero_padded.push(0);
        let mut clean = Vec::new();
        testutil::write_obu(&mut clean, 1, &still.sequence_header());
        testutil::write_obu(&mut clean, 3, &zero_padded);
        testutil::write_obu(&mut clean, 4, tiles);
        Av1Decoder::new()
            .inspect(&clean)
            .expect("zero padding to the end of the OBU is legal");

        let mut dirty_padded = header_obu;
        dirty_padded.push(0x01);
        let mut dirty = Vec::new();
        testutil::write_obu(&mut dirty, 1, &still.sequence_header());
        testutil::write_obu(&mut dirty, 3, &dirty_padded);
        testutil::write_obu(&mut dirty, 4, tiles);
        assert_eq!(
            Av1Decoder::new()
                .inspect(&dirty)
                .unwrap_err()
                .static_message(),
            Some("AV1 OBU: non-zero padding after trailing_bits()")
        );
    }

    #[test]
    fn a_redundant_frame_header_is_ignored_rather_than_decoded_twice() {
        // OBU_REDUNDANT_FRAME_HEADER repeats a header already seen (§5.3.1); treating it as a
        // second frame would refuse a legal stream.
        let still = testutil::StillBuilder::default();
        let frame = still.frame_obu();
        let (header, _) = frame.split_at(frame.len() - 1);
        let mut header_obu = header.to_vec();
        header_obu.push(0x80);

        let mut unit = Vec::new();
        testutil::write_obu(&mut unit, 1, &still.sequence_header());
        testutil::write_obu(&mut unit, 6, &frame); // OBU_FRAME
        testutil::write_obu(&mut unit, 7, &header_obu); // OBU_REDUNDANT_FRAME_HEADER
        let info = Av1Decoder::new()
            .inspect(&unit)
            .expect("a redundant frame header must not be mistaken for a second frame");
        assert_eq!(info.tile_count, 1);
    }

    #[test]
    fn the_pixel_cap_binds_independently_of_the_dimension_caps() {
        // Both dimensions are inside their own cap, but the product is not.
        let limits = DecodeLimits {
            max_pixels: 1024,
            ..DecodeLimits::default()
        };
        limits.check(32, 32, 1).unwrap();
        assert_eq!(
            limits.check(64, 32, 1).unwrap_err().static_message(),
            Some("AV1 decode: frame sample count exceeds the configured limit")
        );
    }
}
