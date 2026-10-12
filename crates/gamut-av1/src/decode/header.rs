//! The uncompressed frame header (AV1 §5.9), parse side.
//!
//! Mirrors [`crate::headers::frame_header_payload`], but reads the *whole* §5.9.2 syntax rather
//! than the subset gamut's encoder emits: a decoder has to accept what any conformant encoder
//! produced. Everything that changes a downstream bit position is parsed even when this decoder
//! cannot act on it, and the tools it cannot honour are refused by
//! [`FrameHeader::reject_unsupported_tools`] once the whole header is read — so a refusal names
//! the actual tool instead of surfacing as a bit-position desync further in.
//!
//! Inter-frame syntax is the one exception. `gamut` is image-first (no inter coding, per the
//! workspace charter), so a non-intra `frame_type` is refused where it is read; the reference,
//! motion-vector, and global-motion syntax that follows is never reachable.

use gamut_bitstream::BitReader;
use gamut_core::{Error, Result};

use super::ORIGIN;
use super::obu::{SELECT_INTEGER_MV, SELECT_SCREEN_CONTENT_TOOLS, SequenceHeader};

/// `MAX_SEGMENTS` (§3).
pub(crate) const MAX_SEGMENTS: usize = 8;
/// `SEG_LVL_MAX` (§3).
pub(crate) const SEG_LVL_MAX: usize = 8;
/// `SEG_LVL_ALT_Q` (§3).
pub(crate) const SEG_LVL_ALT_Q: usize = 0;
/// `SEG_LVL_REF_FRAME` (§3).
const SEG_LVL_REF_FRAME: usize = 5;
/// `MAX_LOOP_FILTER` (§3).
const MAX_LOOP_FILTER: i32 = 63;
/// `TOTAL_REFS_PER_FRAME` (§3).
const TOTAL_REFS_PER_FRAME: usize = 8;
/// `MAX_TILE_WIDTH` (§3), in luma samples.
const MAX_TILE_WIDTH: u32 = 4096;
/// `MAX_TILE_AREA` (§3), in luma samples.
const MAX_TILE_AREA: u32 = 4096 * 2304;
/// `MAX_TILE_ROWS` / `MAX_TILE_COLS` (§3).
const MAX_TILE_ROWS: u32 = 64;
/// See [`MAX_TILE_ROWS`].
const MAX_TILE_COLS: u32 = 64;
/// `RESTORATION_TILESIZE_MAX` (§3).
const RESTORATION_TILESIZE_MAX: u32 = 256;
/// `SUPERRES_NUM` (§3), the fixed numerator of the upscaling ratio.
const SUPERRES_NUM: u32 = 8;
/// `SUPERRES_DENOM_MIN` (§3).
const SUPERRES_DENOM_MIN: u32 = 9;
/// `SUPERRES_DENOM_BITS` (§3).
const SUPERRES_DENOM_BITS: u32 = 3;
/// `KEY_FRAME` (§6.8.2).
const KEY_FRAME: u8 = 0;
/// `INTRA_ONLY_FRAME` (§6.8.2).
const INTRA_ONLY_FRAME: u8 = 2;

/// `Segmentation_Feature_Bits` (§5.9.14).
const SEGMENTATION_FEATURE_BITS: [u32; SEG_LVL_MAX] = [8, 6, 6, 6, 6, 3, 0, 0];
/// `Segmentation_Feature_Signed` (§5.9.14).
const SEGMENTATION_FEATURE_SIGNED: [bool; SEG_LVL_MAX] =
    [true, true, true, true, true, false, false, false];
/// `Segmentation_Feature_Max` (§5.9.14).
const SEGMENTATION_FEATURE_MAX: [i32; SEG_LVL_MAX] = [
    255,
    MAX_LOOP_FILTER,
    MAX_LOOP_FILTER,
    MAX_LOOP_FILTER,
    MAX_LOOP_FILTER,
    7,
    0,
    0,
];

/// `TxMode` (§6.8.21).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxMode {
    /// `ONLY_4X4`: every transform block is 4×4 (implied by `CodedLossless`).
    Only4x4,
    /// `TX_MODE_LARGEST`: the largest transform the block size allows.
    Largest,
    /// `TX_MODE_SELECT`: the transform size is coded per block.
    Select,
}

/// `FrameRestorationType` (§6.10.15), after `Remap_Lr_Type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestorationType {
    /// `RESTORE_NONE`.
    None,
    /// `RESTORE_WIENER`.
    Wiener,
    /// `RESTORE_SGRPROJ`: self-guided restoration.
    SgrProj,
    /// `RESTORE_SWITCHABLE`: chosen per restoration unit.
    Switchable,
}

/// `Remap_Lr_Type` (§5.9.20).
const REMAP_LR_TYPE: [RestorationType; 4] = [
    RestorationType::None,
    RestorationType::Switchable,
    RestorationType::Wiener,
    RestorationType::SgrProj,
];

/// `loop_filter_params()` (§5.9.11).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoopFilterParams {
    /// `loop_filter_level[0..4]`: two luma passes then U and V.
    pub level: [u8; 4],
    /// `loop_filter_sharpness`.
    pub sharpness: u8,
    /// `loop_filter_delta_enabled`.
    pub delta_enabled: bool,
    /// `loop_filter_ref_deltas[TOTAL_REFS_PER_FRAME]`.
    pub ref_deltas: [i8; TOTAL_REFS_PER_FRAME],
    /// `loop_filter_mode_deltas[2]`.
    pub mode_deltas: [i8; 2],
}

impl LoopFilterParams {
    /// The `setup_past_independence()` defaults (§7.20), which are also what §5.9.11 restores on
    /// the `CodedLossless || allow_intrabc` early return.
    const fn defaults() -> Self {
        Self {
            level: [0; 4],
            sharpness: 0,
            delta_enabled: false,
            // INTRA_FRAME = 1, GOLDEN/ALTREF/ALTREF2 = -1, the rest 0.
            ref_deltas: [1, 0, 0, 0, -1, 0, -1, -1],
            mode_deltas: [0; 2],
        }
    }
}

/// `cdef_params()` (§5.9.19). Strengths are stored post-mapping (a coded secondary of 3 becomes 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CdefParams {
    /// `CdefDamping`.
    pub damping: u32,
    /// `cdef_bits`: `1 << cdef_bits` strength sets are signalled.
    pub bits: u32,
    /// `cdef_y_pri_strength[i]`.
    pub y_pri: [u8; 8],
    /// `cdef_y_sec_strength[i]`, after the 3 → 4 remap.
    pub y_sec: [u8; 8],
    /// `cdef_uv_pri_strength[i]`.
    pub uv_pri: [u8; 8],
    /// `cdef_uv_sec_strength[i]`, after the 3 → 4 remap.
    pub uv_sec: [u8; 8],
}

impl CdefParams {
    /// The §5.9.19 early-return values when CDEF is off.
    const fn disabled() -> Self {
        Self {
            damping: 3,
            bits: 0,
            y_pri: [0; 8],
            y_sec: [0; 8],
            uv_pri: [0; 8],
            uv_sec: [0; 8],
        }
    }
}

/// `lr_params()` (§5.9.20).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LrParams {
    /// `FrameRestorationType[plane]`.
    pub frame_restoration_type: [RestorationType; 3],
    /// `LoopRestorationSize[plane]`.
    pub loop_restoration_size: [u32; 3],
    /// `UsesLr`.
    pub uses_lr: bool,
}

impl LrParams {
    /// The §5.9.20 early-return values when restoration is off.
    const fn disabled() -> Self {
        Self {
            frame_restoration_type: [RestorationType::None; 3],
            loop_restoration_size: [RESTORATION_TILESIZE_MAX; 3],
            uses_lr: false,
        }
    }
}

/// `segmentation_params()` (§5.9.14).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SegmentationParams {
    /// `segmentation_enabled`.
    pub enabled: bool,
    /// `segmentation_update_map`.
    pub update_map: bool,
    /// `segmentation_temporal_update`.
    pub temporal_update: bool,
    /// `FeatureEnabled[segment][feature]`.
    pub feature_enabled: [[bool; SEG_LVL_MAX]; MAX_SEGMENTS],
    /// `FeatureData[segment][feature]`, already clipped to the feature's range.
    pub feature_data: [[i32; SEG_LVL_MAX]; MAX_SEGMENTS],
    /// `SegIdPreSkip`.
    pub seg_id_pre_skip: bool,
    /// `LastActiveSegId`.
    pub last_active_seg_id: u8,
}

impl SegmentationParams {
    /// All features off — the §5.9.14 `segmentation_enabled == 0` branch.
    const fn disabled() -> Self {
        Self {
            enabled: false,
            update_map: false,
            temporal_update: false,
            feature_enabled: [[false; SEG_LVL_MAX]; MAX_SEGMENTS],
            feature_data: [[0; SEG_LVL_MAX]; MAX_SEGMENTS],
            seg_id_pre_skip: false,
            last_active_seg_id: 0,
        }
    }

    /// `seg_feature_active_idx( segmentId, feature )` (§6.10.8).
    #[must_use]
    pub const fn feature_active(&self, segment: usize, feature: usize) -> bool {
        self.enabled && self.feature_enabled[segment][feature]
    }
}

/// `quantization_params()` (§5.9.12).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuantizationParams {
    /// `base_q_idx`.
    pub base_q_idx: u8,
    /// `DeltaQYDc`.
    pub delta_q_y_dc: i32,
    /// `DeltaQUDc`.
    pub delta_q_u_dc: i32,
    /// `DeltaQUAc`.
    pub delta_q_u_ac: i32,
    /// `DeltaQVDc`.
    pub delta_q_v_dc: i32,
    /// `DeltaQVAc`.
    pub delta_q_v_ac: i32,
    /// `using_qmatrix`.
    pub using_qmatrix: bool,
    /// `qm_y` / `qm_u` / `qm_v`, valid only when `using_qmatrix`.
    pub qm: [u8; 3],
}

/// The tile grid of `tile_info()` (§5.9.15).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TileInfo {
    /// `TileCols`.
    pub tile_cols: usize,
    /// `TileRows`.
    pub tile_rows: usize,
    /// `TileColsLog2`.
    pub tile_cols_log2: u32,
    /// `TileRowsLog2`.
    pub tile_rows_log2: u32,
    /// `MiColStarts[0..=TileCols]`.
    pub mi_col_starts: Vec<u32>,
    /// `MiRowStarts[0..=TileRows]`.
    pub mi_row_starts: Vec<u32>,
    /// `context_update_tile_id`.
    pub context_update_tile_id: u32,
    /// `TileSizeBytes`.
    pub tile_size_bytes: usize,
}

/// The parsed uncompressed frame header (§5.9.2), plus the derived values §5.9.2 computes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameHeader {
    /// `FrameWidth`, the coded (post-superres-downscale) luma width.
    pub frame_width: u32,
    /// `FrameHeight`.
    pub frame_height: u32,
    /// `UpscaledWidth`, the width after superres upscaling — what the caller sees.
    pub upscaled_width: u32,
    /// `RenderWidth`.
    pub render_width: u32,
    /// `RenderHeight`.
    pub render_height: u32,
    /// `SuperresDenom`.
    pub superres_denom: u32,
    /// `use_superres`.
    pub use_superres: bool,
    /// `MiCols`.
    pub mi_cols: u32,
    /// `MiRows`.
    pub mi_rows: u32,
    /// `disable_cdf_update`.
    pub disable_cdf_update: bool,
    /// `allow_screen_content_tools`.
    pub allow_screen_content_tools: bool,
    /// `allow_intrabc`.
    pub allow_intrabc: bool,
    /// `tile_info()`.
    pub tile_info: TileInfo,
    /// `quantization_params()`.
    pub quant: QuantizationParams,
    /// `segmentation_params()`.
    pub segmentation: SegmentationParams,
    /// `delta_q_present`.
    pub delta_q_present: bool,
    /// `delta_q_res`.
    pub delta_q_res: u32,
    /// `delta_lf_present`.
    pub delta_lf_present: bool,
    /// `delta_lf_res`.
    pub delta_lf_res: u32,
    /// `delta_lf_multi`.
    pub delta_lf_multi: bool,
    /// `CodedLossless`.
    pub coded_lossless: bool,
    /// `AllLossless`.
    pub all_lossless: bool,
    /// `LosslessArray[segmentId]`.
    pub lossless_array: [bool; MAX_SEGMENTS],
    /// `loop_filter_params()`.
    pub loop_filter: LoopFilterParams,
    /// `cdef_params()`.
    pub cdef: CdefParams,
    /// `lr_params()`.
    pub lr: LrParams,
    /// `TxMode`.
    pub tx_mode: TxMode,
    /// `reduced_tx_set`.
    pub reduced_tx_set: bool,
    /// `apply_grain` from `film_grain_params()`.
    pub apply_grain: bool,
}

impl FrameHeader {
    /// Parses `uncompressed_header()` (§5.9.2) from `r`, given the active sequence header.
    ///
    /// The reader is left positioned immediately after the header, before `byte_alignment()`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidInput`] for a malformed or truncated header, or
    /// [`Error::Unsupported`] for a frame this image decoder does not code — anything but an
    /// intra key frame.
    pub(crate) fn parse(r: &mut BitReader<'_>, seq: &SequenceHeader) -> Result<Self> {
        if seq.decoder_model_info_present {
            // §5.9.2 codes `buffer_removal_time` per operating point in every frame header. The
            // workspace charter puts the decoder model out of scope (it is sequence-only), and
            // parsing on without honouring it would desync everything after it.
            return Err(Error::unsupported(
                ORIGIN,
                "AV1 frame header: decoder model info is out of scope for a still image",
            ));
        }
        let id_len = seq.additional_frame_id_length + seq.delta_frame_id_length;
        let mut frame_type = KEY_FRAME;
        let mut error_resilient_mode = true;

        if !seq.reduced_still_picture_header {
            if r.flag()? {
                // show_existing_frame: a still image has no reference buffer to show from.
                return Err(Error::unsupported(
                    ORIGIN,
                    "AV1 frame header: show_existing_frame needs a reference frame buffer",
                ));
            }
            frame_type = r.f(2)? as u8;
            if frame_type != KEY_FRAME && frame_type != INTRA_ONLY_FRAME {
                return Err(Error::unsupported(
                    ORIGIN,
                    "AV1 frame header: inter frames are out of scope for a still-image decoder",
                ));
            }
            let show_frame = r.flag()?;
            if !show_frame {
                // showable_frame; a still image codes exactly one shown frame.
                let _ = r.f(1)?;
                return Err(Error::unsupported(
                    ORIGIN,
                    "AV1 frame header: a still image must code a shown frame",
                ));
            }
            // §5.9.2: a shown key frame infers `error_resilient_mode = 1`; an intra-only frame
            // codes it.
            if frame_type != KEY_FRAME {
                error_resilient_mode = r.flag()?;
            }
        }

        let disable_cdf_update = r.flag()?;
        let allow_screen_content_tools =
            if seq.seq_force_screen_content_tools == SELECT_SCREEN_CONTENT_TOOLS {
                r.flag()?
            } else {
                seq.seq_force_screen_content_tools != 0
            };
        if allow_screen_content_tools && seq.seq_force_integer_mv == SELECT_INTEGER_MV {
            let _ = r.f(1)?; // force_integer_mv; an intra frame forces it to 1 regardless.
        }
        if seq.frame_id_numbers_present {
            let _ = r.f(id_len)?; // current_frame_id
        }

        // `frame_size_override_flag` is inferred 0 under a reduced still-picture header and
        // coded otherwise (a SWITCH_FRAME would infer 1, but those are refused above).
        let frame_size_override = if seq.reduced_still_picture_header {
            false
        } else {
            r.flag()?
        };
        let _ = r.f(seq.order_hint_bits)?; // order_hint
        // primary_ref_frame is PRIMARY_REF_NONE for an intra frame — no bits.
        // `buffer_removal_time` is not coded: the decoder model is refused above.

        // refresh_frame_flags (§5.9.2): a shown key frame refreshes every slot with no bits; an
        // intra-only frame codes the mask. `allFrames` is `(1 << NUM_REF_FRAMES) - 1`.
        const ALL_FRAMES: u32 = (1 << 8) - 1;
        let refresh_frame_flags = if frame_type == KEY_FRAME {
            ALL_FRAMES
        } else {
            r.f(8)?
        };
        // FrameIsIntra holds, so this reduces to the partial-refresh case.
        if refresh_frame_flags != ALL_FRAMES && error_resilient_mode && seq.enable_order_hint {
            for _ in 0..8 {
                let _ = r.f(seq.order_hint_bits)?; // ref_order_hint[i]
            }
        }

        let (frame_width, frame_height, upscaled_width, superres_denom, use_superres) =
            Self::frame_size(r, seq, frame_size_override)?;
        let (render_width, render_height) = Self::render_size(r, upscaled_width, frame_height)?;
        let allow_intrabc = if allow_screen_content_tools && upscaled_width == frame_width {
            r.flag()?
        } else {
            false
        };

        // disable_frame_end_update_cdf is inferred 1 under a reduced still-picture header or when
        // CDF updates are disabled; otherwise it is coded.
        if !seq.reduced_still_picture_header && !disable_cdf_update {
            let _ = r.f(1)?;
        }

        // compute_image_size() (§5.9.9).
        let mi_cols = 2 * ((frame_width + 7) >> 3);
        let mi_rows = 2 * ((frame_height + 7) >> 3);

        let tile_info = Self::tile_info(r, seq, mi_cols, mi_rows)?;
        let quant = Self::quantization_params(r, seq)?;
        let segmentation = Self::segmentation_params(r)?;

        // delta_q_params() (§5.9.17) then delta_lf_params() (§5.9.18).
        let delta_q_present = quant.base_q_idx > 0 && r.flag()?;
        let delta_q_res = if delta_q_present { r.f(2)? } else { 0 };
        let mut delta_lf_present = false;
        let mut delta_lf_res = 0;
        let mut delta_lf_multi = false;
        if delta_q_present {
            if !allow_intrabc {
                delta_lf_present = r.flag()?;
            }
            if delta_lf_present {
                delta_lf_res = r.f(2)?;
                delta_lf_multi = r.flag()?;
            }
        }

        // CodedLossless / LosslessArray (§5.9.2).
        let mut lossless_array = [false; MAX_SEGMENTS];
        let mut coded_lossless = true;
        for (segment, lossless) in lossless_array.iter_mut().enumerate() {
            let qindex = get_qindex_ignoring_delta(&segmentation, &quant, segment);
            *lossless = qindex == 0
                && quant.delta_q_y_dc == 0
                && quant.delta_q_u_ac == 0
                && quant.delta_q_u_dc == 0
                && quant.delta_q_v_ac == 0
                && quant.delta_q_v_dc == 0;
            if !*lossless {
                coded_lossless = false;
            }
        }
        let all_lossless = coded_lossless && frame_width == upscaled_width;

        let loop_filter = Self::loop_filter_params(
            r,
            coded_lossless || allow_intrabc,
            seq.color.subsampling.num_planes(),
        )?;
        let cdef = Self::cdef_params(
            r,
            coded_lossless || allow_intrabc || !seq.enable_cdef,
            seq.color.subsampling.num_planes(),
        )?;
        let lr = Self::lr_params(r, seq, all_lossless || allow_intrabc)?;

        // read_tx_mode() (§5.9.21).
        let tx_mode = if coded_lossless {
            TxMode::Only4x4
        } else if r.flag()? {
            TxMode::Select
        } else {
            TxMode::Largest
        };
        // frame_reference_mode() and skip_mode_params() code nothing for an intra frame, and
        // allow_warped_motion is inferred 0.
        let reduced_tx_set = r.flag()?;
        // global_motion_params() codes nothing for an intra frame.
        let apply_grain = Self::film_grain_params(r, seq)?;

        Ok(Self {
            frame_width,
            frame_height,
            upscaled_width,
            render_width,
            render_height,
            superres_denom,
            use_superres,
            mi_cols,
            mi_rows,
            disable_cdf_update,
            allow_screen_content_tools,
            allow_intrabc,
            tile_info,
            quant,
            segmentation,
            delta_q_present,
            delta_q_res,
            delta_lf_present,
            delta_lf_res,
            delta_lf_multi,
            coded_lossless,
            all_lossless,
            lossless_array,
            loop_filter,
            cdef,
            lr,
            tx_mode,
            reduced_tx_set,
            apply_grain,
        })
    }

    /// `frame_size()` (§5.9.5) with the embedded `superres_params()` (§5.9.8).
    ///
    /// Returns `(FrameWidth, FrameHeight, UpscaledWidth, SuperresDenom, use_superres)`.
    fn frame_size(
        r: &mut BitReader<'_>,
        seq: &SequenceHeader,
        frame_size_override: bool,
    ) -> Result<(u32, u32, u32, u32, bool)> {
        // §5.9.5 re-reads the dimensions with the *coded* widths from the sequence header, which
        // an encoder may set wider than the minimum needed for `max_frame_*`.
        let (mut width, height) = if frame_size_override {
            (
                r.f(seq.frame_width_bits)? + 1,
                r.f(seq.frame_height_bits)? + 1,
            )
        } else {
            (seq.max_frame_width, seq.max_frame_height)
        };

        let use_superres = seq.enable_superres && r.flag()?;
        let superres_denom = if use_superres {
            r.f(SUPERRES_DENOM_BITS)? + SUPERRES_DENOM_MIN
        } else {
            SUPERRES_NUM
        };
        let upscaled_width = width;
        width = (upscaled_width * SUPERRES_NUM + superres_denom / 2) / superres_denom;
        Ok((width, height, upscaled_width, superres_denom, use_superres))
    }

    /// `render_size()` (§5.9.6).
    fn render_size(
        r: &mut BitReader<'_>,
        upscaled_width: u32,
        frame_height: u32,
    ) -> Result<(u32, u32)> {
        if r.flag()? {
            Ok((r.f(16)? + 1, r.f(16)? + 1))
        } else {
            Ok((upscaled_width, frame_height))
        }
    }

    /// `tile_info()` (§5.9.15), both the uniform and the explicit spacing branch.
    fn tile_info(
        r: &mut BitReader<'_>,
        seq: &SequenceHeader,
        mi_cols: u32,
        mi_rows: u32,
    ) -> Result<TileInfo> {
        let (sb_shift, sb_cols, sb_rows) = if seq.use_128x128_superblock {
            (5u32, (mi_cols + 31) >> 5, (mi_rows + 31) >> 5)
        } else {
            (4u32, (mi_cols + 15) >> 4, (mi_rows + 15) >> 4)
        };
        let sb_size = sb_shift + 2;
        let max_tile_width_sb = MAX_TILE_WIDTH >> sb_size;
        let mut max_tile_area_sb = MAX_TILE_AREA >> (2 * sb_size);
        let min_log2_tile_cols = tile_log2(max_tile_width_sb, sb_cols);
        let max_log2_tile_cols = tile_log2(1, sb_cols.min(MAX_TILE_COLS));
        let max_log2_tile_rows = tile_log2(1, sb_rows.min(MAX_TILE_ROWS));
        let min_log2_tiles = min_log2_tile_cols.max(tile_log2(max_tile_area_sb, sb_rows * sb_cols));

        let mut mi_col_starts = Vec::new();
        let mut mi_row_starts = Vec::new();
        let tile_cols_log2;
        let tile_rows_log2;

        if r.flag()? {
            // uniform_tile_spacing_flag == 1.
            let mut cols_log2 = min_log2_tile_cols;
            while cols_log2 < max_log2_tile_cols {
                if r.flag()? {
                    cols_log2 += 1;
                } else {
                    break;
                }
            }
            let tile_width_sb = (sb_cols + (1 << cols_log2) - 1) >> cols_log2;
            let mut start_sb = 0;
            while start_sb < sb_cols {
                mi_col_starts.push(start_sb << sb_shift);
                start_sb += tile_width_sb;
            }
            mi_col_starts.push(mi_cols);

            let min_log2_tile_rows = min_log2_tiles.saturating_sub(cols_log2);
            let mut rows_log2 = min_log2_tile_rows;
            while rows_log2 < max_log2_tile_rows {
                if r.flag()? {
                    rows_log2 += 1;
                } else {
                    break;
                }
            }
            let tile_height_sb = (sb_rows + (1 << rows_log2) - 1) >> rows_log2;
            let mut start_sb = 0;
            while start_sb < sb_rows {
                mi_row_starts.push(start_sb << sb_shift);
                start_sb += tile_height_sb;
            }
            mi_row_starts.push(mi_rows);

            tile_cols_log2 = cols_log2;
            tile_rows_log2 = rows_log2;
        } else {
            // Explicit tile sizes.
            let mut widest_tile_sb = 0;
            let mut start_sb = 0;
            while start_sb < sb_cols {
                mi_col_starts.push(start_sb << sb_shift);
                let max_width = (sb_cols - start_sb).min(max_tile_width_sb);
                let size_sb = r.ns(max_width)? + 1;
                widest_tile_sb = widest_tile_sb.max(size_sb);
                start_sb += size_sb;
            }
            mi_col_starts.push(mi_cols);
            let cols = mi_col_starts.len() - 1;
            tile_cols_log2 = tile_log2(1, cols as u32);

            max_tile_area_sb = if min_log2_tiles > 0 {
                (sb_rows * sb_cols) >> (min_log2_tiles + 1)
            } else {
                sb_rows * sb_cols
            };
            let max_tile_height_sb = (max_tile_area_sb / widest_tile_sb.max(1)).max(1);
            let mut start_sb = 0;
            while start_sb < sb_rows {
                mi_row_starts.push(start_sb << sb_shift);
                let max_height = (sb_rows - start_sb).min(max_tile_height_sb);
                let size_sb = r.ns(max_height)? + 1;
                start_sb += size_sb;
            }
            mi_row_starts.push(mi_rows);
            tile_rows_log2 = tile_log2(1, (mi_row_starts.len() - 1) as u32);
        }

        let tile_cols = mi_col_starts.len() - 1;
        let tile_rows = mi_row_starts.len() - 1;
        if tile_cols == 0 || tile_rows == 0 {
            return Err(Error::invalid_input(
                ORIGIN,
                "AV1 tile_info: frame has no tiles",
            ));
        }

        let (context_update_tile_id, tile_size_bytes) = if tile_cols_log2 > 0 || tile_rows_log2 > 0
        {
            let id = r.f(tile_rows_log2 + tile_cols_log2)?;
            let bytes = r.f(2)? as usize + 1;
            (id, bytes)
        } else {
            (0, 1)
        };
        if context_update_tile_id as usize >= tile_cols * tile_rows {
            return Err(Error::invalid_input(
                ORIGIN,
                "AV1 tile_info: context_update_tile_id is out of range",
            ));
        }

        Ok(TileInfo {
            tile_cols,
            tile_rows,
            tile_cols_log2,
            tile_rows_log2,
            mi_col_starts,
            mi_row_starts,
            context_update_tile_id,
            tile_size_bytes,
        })
    }

    /// `quantization_params()` (§5.9.12).
    fn quantization_params(
        r: &mut BitReader<'_>,
        seq: &SequenceHeader,
    ) -> Result<QuantizationParams> {
        let base_q_idx = r.f(8)? as u8;
        let delta_q_y_dc = read_delta_q(r)?;
        let (delta_q_u_dc, delta_q_u_ac, delta_q_v_dc, delta_q_v_ac) =
            if seq.color.subsampling.num_planes() > 1 {
                let diff_uv_delta = seq.color.separate_uv_delta_q && r.flag()?;
                let u_dc = read_delta_q(r)?;
                let u_ac = read_delta_q(r)?;
                if diff_uv_delta {
                    (u_dc, u_ac, read_delta_q(r)?, read_delta_q(r)?)
                } else {
                    (u_dc, u_ac, u_dc, u_ac)
                }
            } else {
                (0, 0, 0, 0)
            };
        let using_qmatrix = r.flag()?;
        let qm = if using_qmatrix {
            let y = r.f(4)? as u8;
            let u = r.f(4)? as u8;
            let v = if seq.color.separate_uv_delta_q {
                r.f(4)? as u8
            } else {
                u
            };
            [y, u, v]
        } else {
            [0; 3]
        };
        Ok(QuantizationParams {
            base_q_idx,
            delta_q_y_dc,
            delta_q_u_dc,
            delta_q_u_ac,
            delta_q_v_dc,
            delta_q_v_ac,
            using_qmatrix,
            qm,
        })
    }

    /// `segmentation_params()` (§5.9.14).
    ///
    /// `primary_ref_frame` is `PRIMARY_REF_NONE` for every frame this decoder accepts, so
    /// `segmentation_update_map` / `_temporal_update` / `_update_data` are inferred and the
    /// temporal-prediction branch is unreachable.
    fn segmentation_params(r: &mut BitReader<'_>) -> Result<SegmentationParams> {
        if !r.flag()? {
            return Ok(SegmentationParams::disabled());
        }
        let mut params = SegmentationParams {
            enabled: true,
            update_map: true,
            temporal_update: false,
            ..SegmentationParams::disabled()
        };
        for segment in 0..MAX_SEGMENTS {
            for feature in 0..SEG_LVL_MAX {
                if !r.flag()? {
                    continue;
                }
                params.feature_enabled[segment][feature] = true;
                let bits = SEGMENTATION_FEATURE_BITS[feature];
                let limit = SEGMENTATION_FEATURE_MAX[feature];
                // A zero-width feature (6 and 7) reads no bits and clamps to 0, which `f(0)`
                // and `Clip3(0, 0, ..)` already give — no special case needed.
                params.feature_data[segment][feature] = if SEGMENTATION_FEATURE_SIGNED[feature] {
                    r.su(bits + 1)?.clamp(-limit, limit)
                } else {
                    (r.f(bits)? as i32).clamp(0, limit)
                };
            }
        }
        for segment in 0..MAX_SEGMENTS {
            for feature in 0..SEG_LVL_MAX {
                if params.feature_enabled[segment][feature] {
                    params.last_active_seg_id = segment as u8;
                    if feature >= SEG_LVL_REF_FRAME {
                        params.seg_id_pre_skip = true;
                    }
                }
            }
        }
        Ok(params)
    }

    /// `loop_filter_params()` (§5.9.11).
    fn loop_filter_params(
        r: &mut BitReader<'_>,
        skipped: bool,
        num_planes: usize,
    ) -> Result<LoopFilterParams> {
        let mut lf = LoopFilterParams::defaults();
        if skipped {
            return Ok(lf);
        }
        lf.level[0] = r.f(6)? as u8;
        lf.level[1] = r.f(6)? as u8;
        if num_planes > 1 && (lf.level[0] != 0 || lf.level[1] != 0) {
            lf.level[2] = r.f(6)? as u8;
            lf.level[3] = r.f(6)? as u8;
        }
        lf.sharpness = r.f(3)? as u8;
        lf.delta_enabled = r.flag()?;
        if lf.delta_enabled && r.flag()? {
            // loop_filter_delta_update.
            for delta in &mut lf.ref_deltas {
                if r.flag()? {
                    *delta = r.su(7)? as i8;
                }
            }
            for delta in &mut lf.mode_deltas {
                if r.flag()? {
                    *delta = r.su(7)? as i8;
                }
            }
        }
        Ok(lf)
    }

    /// `cdef_params()` (§5.9.19).
    fn cdef_params(r: &mut BitReader<'_>, skipped: bool, num_planes: usize) -> Result<CdefParams> {
        if skipped {
            return Ok(CdefParams::disabled());
        }
        let mut cdef = CdefParams::disabled();
        cdef.damping = r.f(2)? + 3;
        cdef.bits = r.f(2)?;
        for i in 0..(1usize << cdef.bits) {
            cdef.y_pri[i] = r.f(4)? as u8;
            cdef.y_sec[i] = remap_sec_strength(r.f(2)? as u8);
            if num_planes > 1 {
                cdef.uv_pri[i] = r.f(4)? as u8;
                cdef.uv_sec[i] = remap_sec_strength(r.f(2)? as u8);
            }
        }
        Ok(cdef)
    }

    /// `lr_params()` (§5.9.20).
    fn lr_params(r: &mut BitReader<'_>, seq: &SequenceHeader, skipped: bool) -> Result<LrParams> {
        if skipped || !seq.enable_restoration {
            return Ok(LrParams::disabled());
        }
        let mut lr = LrParams::disabled();
        let mut uses_chroma_lr = false;
        for plane in 0..seq.color.subsampling.num_planes() {
            let kind = REMAP_LR_TYPE[r.f(2)? as usize];
            lr.frame_restoration_type[plane] = kind;
            if kind != RestorationType::None {
                lr.uses_lr = true;
                if plane > 0 {
                    uses_chroma_lr = true;
                }
            }
        }
        if lr.uses_lr {
            let mut shift = r.f(1)?;
            if seq.use_128x128_superblock {
                shift += 1;
            } else if shift != 0 {
                shift += r.f(1)?;
            }
            lr.loop_restoration_size[0] = RESTORATION_TILESIZE_MAX >> (2 - shift);
            let uv_shift = if seq.color.subsampling.x() == 1
                && seq.color.subsampling.y() == 1
                && uses_chroma_lr
            {
                r.f(1)?
            } else {
                0
            };
            lr.loop_restoration_size[1] = lr.loop_restoration_size[0] >> uv_shift;
            lr.loop_restoration_size[2] = lr.loop_restoration_size[1];
        }
        Ok(lr)
    }

    /// `film_grain_params()` (§5.9.30). Returns `apply_grain`.
    ///
    /// The parameters are consumed for their bit width even though this decoder does not
    /// synthesise grain: they precede nothing else in the header, but reading them keeps the
    /// trailing-bits check meaningful, and `apply_grain` is what
    /// [`FrameHeader::reject_unsupported_tools`] refuses on.
    fn film_grain_params(r: &mut BitReader<'_>, seq: &SequenceHeader) -> Result<bool> {
        if !seq.film_grain_params_present {
            return Ok(false);
        }
        if !r.flag()? {
            return Ok(false);
        }
        let _ = r.f(16)?; // grain_seed
        // frame_type is always intra here, so update_grain is inferred 1.
        let num_y_points = r.f(4)?;
        for _ in 0..num_y_points {
            let _ = r.f(8)?; // point_y_value
            let _ = r.f(8)?; // point_y_scaling
        }
        let mono = seq.color.subsampling.num_planes() == 1;
        let chroma_scaling_from_luma = if mono { false } else { r.flag()? };
        let (mut num_cb_points, mut num_cr_points) = (0, 0);
        if !mono
            && !chroma_scaling_from_luma
            && !(seq.color.subsampling.x() == 1
                && seq.color.subsampling.y() == 1
                && num_y_points == 0)
        {
            num_cb_points = r.f(4)?;
            for _ in 0..num_cb_points {
                let _ = r.f(8)?;
                let _ = r.f(8)?;
            }
            num_cr_points = r.f(4)?;
            for _ in 0..num_cr_points {
                let _ = r.f(8)?;
                let _ = r.f(8)?;
            }
        }
        let _ = r.f(2)?; // grain_scaling_minus_8
        let ar_coeff_lag = r.f(2)?;
        let num_pos_luma = 2 * ar_coeff_lag * (ar_coeff_lag + 1);
        let num_pos_chroma = if num_y_points > 0 {
            for _ in 0..num_pos_luma {
                let _ = r.f(8)?;
            }
            num_pos_luma + 1
        } else {
            num_pos_luma
        };
        if chroma_scaling_from_luma || num_cb_points > 0 {
            for _ in 0..num_pos_chroma {
                let _ = r.f(8)?;
            }
        }
        if chroma_scaling_from_luma || num_cr_points > 0 {
            for _ in 0..num_pos_chroma {
                let _ = r.f(8)?;
            }
        }
        let _ = r.f(2)?; // ar_coeff_shift_minus_6
        let _ = r.f(2)?; // grain_scale_shift
        if num_cb_points > 0 {
            let _ = r.f(8)?; // cb_mult
            let _ = r.f(8)?; // cb_luma_mult
            let _ = r.f(9)?; // cb_offset
        }
        if num_cr_points > 0 {
            let _ = r.f(8)?;
            let _ = r.f(8)?;
            let _ = r.f(9)?;
        }
        let _ = r.f(1)?; // overlap_flag
        let _ = r.f(1)?; // clip_to_restricted_range
        Ok(true)
    }

    /// Refuses the tools this decoder parses but does not implement.
    ///
    /// Called once the header is complete, so the message names the tool rather than surfacing as
    /// a desync deeper in the tile data. Each refusal corresponds to a ☐ row in
    /// `gamut-avif/STATUS.md`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Unsupported`] naming the first unimplemented tool the header signals.
    pub(crate) fn reject_unsupported_tools(&self, seq: &SequenceHeader) -> Result<()> {
        if self.allow_intrabc {
            return Err(Error::unsupported(
                ORIGIN,
                "AV1 decode: intra block copy (allow_intrabc) is not implemented",
            ));
        }
        if self.apply_grain {
            return Err(Error::unsupported(
                ORIGIN,
                "AV1 decode: film grain synthesis is not implemented",
            ));
        }
        if seq.color.bit_depth != 8 {
            return Err(Error::unsupported(
                ORIGIN,
                "AV1 decode: only 8-bit streams are implemented",
            ));
        }
        // Monochrome carries no chroma at all, so a subsampling diagnostic would name the wrong
        // reason for a stream this crate's own encoder can emit.
        if seq.color.subsampling == super::obu::Subsampling::Monochrome {
            return Err(Error::unsupported(
                ORIGIN,
                "AV1 decode: monochrome streams are not implemented",
            ));
        }
        if seq.color.subsampling != super::obu::Subsampling::Yuv444 {
            return Err(Error::unsupported(
                ORIGIN,
                "AV1 decode: only 4:4:4 chroma is implemented",
            ));
        }
        Ok(())
    }
}

/// `read_delta_q()` (§5.9.13).
fn read_delta_q(r: &mut BitReader<'_>) -> Result<i32> {
    if r.flag()? { r.su(7) } else { Ok(0) }
}

/// The §5.9.19 secondary-strength remap: a coded 3 means 4.
const fn remap_sec_strength(coded: u8) -> u8 {
    if coded == 3 { 4 } else { coded }
}

/// `tile_log2( blkSize, target )` (§5.9.16): the smallest `k` with `blkSize << k >= target`.
pub(crate) fn tile_log2(blk_size: u32, target: u32) -> u32 {
    let mut k = 0;
    while (blk_size << k) < target {
        k += 1;
    }
    k
}

/// `get_qindex( 1, segmentId )` (§7.12.2) — the `ignoreDeltaQ` form the header's `CodedLossless`
/// derivation uses, before any per-block `CurrentQIndex` exists.
fn get_qindex_ignoring_delta(
    seg: &SegmentationParams,
    quant: &QuantizationParams,
    segment: usize,
) -> i32 {
    let base = i32::from(quant.base_q_idx);
    if seg.feature_active(segment, SEG_LVL_ALT_Q) {
        (base + seg.feature_data[segment][SEG_LVL_ALT_Q]).clamp(0, 255)
    } else {
        base
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::obu::{ObuIter, Subsampling};
    use crate::filter::{CdefStrengths, FrameFilters};
    use crate::headers::Av1Colour;

    /// The in-loop filters [`parse_encoder_headers`] has the encoder signal: a non-zero value in
    /// every field, so each reads back as itself rather than as a default.
    const ENCODER_FILTERS: FrameFilters = FrameFilters {
        deblock_level: 37,
        cdef: CdefStrengths {
            y_pri: 9,
            y_sec: 2,
            uv_pri: 14,
            uv_sec: 1,
        },
    };

    /// Parses the sequence + frame header out of a temporal unit the encoder produced.
    fn parse_encoder_headers(
        width: u32,
        height: u32,
        base_q_idx: u8,
        superres: Option<u8>,
    ) -> (SequenceHeader, FrameHeader) {
        parse_encoder_headers_with(width, height, base_q_idx, superres, ENCODER_FILTERS, true)
    }

    /// The headers the encoder writes for a frame whose in-loop filters are `filters`, with luma
    /// restoration signalled when `luma_lr`.
    fn parse_encoder_headers_with(
        width: u32,
        height: u32,
        base_q_idx: u8,
        superres: Option<u8>,
        filters: FrameFilters,
        luma_lr: bool,
    ) -> (SequenceHeader, FrameHeader) {
        let lossy = base_q_idx > 0;
        let cfg = crate::decode::testutil::still_config(width, height, Av1Colour::default());
        let seq_payload =
            crate::headers::sequence_header_payload(&cfg, width, height, lossy, superres.is_some());
        // The tiles span the coded width, which superres narrows (as `encode_with` resolves them).
        let coded_width = superres.map_or(width as usize, |cd| {
            crate::filter::superres_downscaled_width(width as usize, usize::from(cd) + 9)
        });
        let layout = crate::headers::TileLayout::resolve(
            crate::headers::Tiling::default(),
            coded_width,
            height as usize,
            cfg.seq_level_idx_0,
            superres,
        );
        let frame_payload = crate::headers::frame_header_payload(
            &layout, base_q_idx, superres, false, filters, luma_lr,
        );
        let unit = crate::headers::assemble_temporal_unit(&seq_payload, &frame_payload);
        let obus: Vec<_> = ObuIter::new(&unit).collect::<Result<Vec<_>>>().unwrap();
        let seq = SequenceHeader::parse(obus[0].payload).unwrap();
        let mut r = BitReader::new(obus[1].payload);
        let fh = FrameHeader::parse(&mut r, &seq).unwrap();
        // The header is all the payload holds (no tile data is appended), bar the tile group's
        // `tile_start_and_end_present_flag` (0) when there is more than one tile: the parser must
        // end on its zero alignment padding, so a field coded that the syntax does not read —
        // which would leave its bits for the fields after it — cannot pass unnoticed.
        r.byte_alignment().unwrap();
        if fh.tile_info.tile_cols * fh.tile_info.tile_rows > 1 {
            assert!(!r.flag().unwrap(), "tile_start_and_end_present_flag");
            r.byte_alignment().unwrap();
        }
        assert!(r.remaining_bytes().is_empty(), "unread header bytes");
        (seq, fh)
    }

    #[test]
    fn parses_the_encoders_lossless_frame_header() {
        let (seq, fh) = parse_encoder_headers(64, 64, 0, None);
        assert!(fh.coded_lossless, "base_q_idx 0 must be CodedLossless");
        assert!(fh.all_lossless);
        assert_eq!(fh.tx_mode, TxMode::Only4x4);
        assert_eq!(fh.quant.base_q_idx, 0);
        assert!(!fh.segmentation.enabled);
        assert!(!fh.delta_q_present);
        assert!(!fh.delta_lf_present);
        assert_eq!(fh.loop_filter.level, [0; 4]);
        assert_eq!(fh.cdef, CdefParams::disabled());
        assert_eq!(fh.lr, LrParams::disabled());
        assert!(fh.reduced_tx_set);
        assert!(!fh.apply_grain);
        assert_eq!(fh.frame_width, 64);
        assert_eq!(fh.upscaled_width, 64);
        assert_eq!(fh.mi_cols, 16);
        assert_eq!(fh.mi_rows, 16);
        assert!(!fh.allow_intrabc);
        fh.reject_unsupported_tools(&seq).unwrap();
    }

    #[test]
    fn parses_the_encoders_lossy_frame_header() {
        for q in [20u8, 60, 120, 200] {
            let (seq, fh) = parse_encoder_headers(128, 96, q, None);
            assert!(!fh.coded_lossless, "q={q} must not be CodedLossless");
            assert_eq!(fh.tx_mode, TxMode::Select);
            assert_eq!(fh.quant.base_q_idx, q);
            assert!(fh.segmentation.enabled, "q={q} enables segmentation");
            assert!(fh.delta_q_present);
            assert_eq!(fh.delta_q_res, 0);
            assert!(fh.delta_lf_present);
            assert!(!fh.delta_lf_multi);
            // One deblock level, repeated for both luma passes and both chroma planes.
            assert_eq!(fh.loop_filter.level, [ENCODER_FILTERS.deblock_level; 4]);
            // CDEF: one strength set, secondary strengths already remapped.
            let cdef = ENCODER_FILTERS.cdef;
            assert_eq!(fh.cdef.bits, 0);
            assert_eq!(fh.cdef.damping, 3);
            assert_eq!(fh.cdef.y_pri[0], cdef.y_pri);
            assert_eq!(fh.cdef.y_sec[0], cdef.y_sec);
            assert_eq!(fh.cdef.uv_pri[0], cdef.uv_pri);
            assert_eq!(fh.cdef.uv_sec[0], cdef.uv_sec);
            // Loop restoration: Wiener on luma only, unit size 256.
            assert_eq!(
                fh.lr.frame_restoration_type,
                [
                    RestorationType::Wiener,
                    RestorationType::None,
                    RestorationType::None
                ]
            );
            assert!(fh.lr.uses_lr);
            assert_eq!(fh.lr.loop_restoration_size[0], 256);
            fh.reject_unsupported_tools(&seq).unwrap();
        }
    }

    #[test]
    fn the_encoder_signals_its_chosen_filters_and_no_restoration_when_none_helps() {
        // The opposite corner of every choice `parses_the_encoders_lossy_frame_header` pins: the
        // loop filter off (so the chroma levels are not coded and read back as 0), the weakest
        // non-zero CDEF strengths with secondary 4 (coded as 3), and no restored unit — which must
        // signal RESTORE_NONE and code no `lr_unit_shift`, since the tiles then code no unit.
        let filters = FrameFilters {
            deblock_level: 0,
            cdef: CdefStrengths {
                y_pri: 1,
                y_sec: 4,
                uv_pri: 0,
                uv_sec: 1,
            },
        };
        let (_, fh) = parse_encoder_headers_with(128, 96, 100, None, filters, false);
        assert_eq!(fh.loop_filter.level, [0; 4]);
        assert_eq!(
            (fh.cdef.y_pri[0], fh.cdef.y_sec[0]),
            (1, 4),
            "a stored 4 is coded as 3 and remapped back"
        );
        assert_eq!((fh.cdef.uv_pri[0], fh.cdef.uv_sec[0]), (0, 1));
        assert_eq!(fh.lr, LrParams::disabled());
    }

    #[test]
    fn segmentation_alt_q_survives_the_round_trip() {
        let (_, fh) = parse_encoder_headers(128, 96, 100, None);
        for (segment, alt) in crate::tile::SEG_ALT_Q.iter().enumerate() {
            match alt {
                Some(delta) => {
                    assert!(fh.segmentation.feature_active(segment, SEG_LVL_ALT_Q));
                    assert_eq!(fh.segmentation.feature_data[segment][SEG_LVL_ALT_Q], *delta);
                }
                None => assert!(!fh.segmentation.feature_active(segment, SEG_LVL_ALT_Q)),
            }
        }
        assert!(!fh.segmentation.seg_id_pre_skip, "ALT_Q is below REF_FRAME");
        // §5.9.14: every frame this decoder accepts has primary_ref_frame == PRIMARY_REF_NONE,
        // which infers segmentation_update_map = 1 and segmentation_temporal_update = 0 rather
        // than coding them — so an enabled segmentation always reports a fresh map.
        assert!(fh.segmentation.update_map);
        assert!(!fh.segmentation.temporal_update);
    }

    #[test]
    fn superres_downscales_the_coded_width() {
        // coded_denom 7 ⇒ SuperresDenom 16, so FrameWidth = round(256 * 8 / 16) = 128.
        let (_, fh) = parse_encoder_headers(256, 64, 100, Some(7));
        assert!(fh.use_superres);
        assert_eq!(fh.superres_denom, 16);
        assert_eq!(fh.upscaled_width, 256);
        assert_eq!(fh.frame_width, 128);
        assert_eq!(fh.render_width, 256, "render size follows UpscaledWidth");
        assert!(!fh.all_lossless, "superres frames are never AllLossless");
        assert_eq!(fh.mi_cols, 2 * ((128 + 7) >> 3));
    }

    #[test]
    fn tile_grid_matches_the_encoders_split() {
        // 128 px wide is 2 superblocks, so the encoder emits two tile columns.
        let (_, fh) = parse_encoder_headers(128, 64, 100, None);
        assert_eq!(fh.tile_info.tile_cols, 2);
        assert_eq!(fh.tile_info.tile_rows, 1);
        assert_eq!(fh.tile_info.tile_cols_log2, 1);
        assert_eq!(fh.tile_info.mi_col_starts, vec![0, 16, fh.mi_cols]);
        assert_eq!(fh.tile_info.mi_row_starts, vec![0, fh.mi_rows]);
        assert_eq!(fh.tile_info.tile_size_bytes, 4);
        assert_eq!(fh.tile_info.context_update_tile_id, 0);

        // 64 px wide is one superblock: a single tile, and no tile-size fields.
        let (_, fh) = parse_encoder_headers(64, 64, 100, None);
        assert_eq!(fh.tile_info.tile_cols, 1);
        assert_eq!(fh.tile_info.tile_cols_log2, 0);
        assert_eq!(fh.tile_info.tile_size_bytes, 1);
    }

    /// Parses a synthetic still built by [`StillBuilder`], checking that the parser stops exactly
    /// where the builder's uncompressed header ended.
    ///
    /// The position check is what makes fields the decoder *discards* testable: reading one at the
    /// wrong width leaves every later field misplaced, which a `FrameHeader` comparison alone can
    /// miss entirely when the misplaced fields are all thrown away (film grain is nothing else).
    fn parse_built(still: &crate::decode::testutil::StillBuilder) -> (SequenceHeader, FrameHeader) {
        let seq = SequenceHeader::parse(&still.sequence_header()).unwrap();
        let (payload, header_bits) = still.frame_obu_with_header_bits();
        let mut r = BitReader::new(&payload);
        let fh = FrameHeader::parse(&mut r, &seq).unwrap();
        assert_eq!(
            r.bit_position(),
            header_bits,
            "the parser must consume the coded header bit for bit"
        );
        (seq, fh)
    }

    #[test]
    fn loop_filter_defaults_match_setup_past_independence() {
        // §7.20: INTRA_FRAME is +1, GOLDEN/ALTREF/ALTREF2 are -1, everything else 0. A
        // CodedLossless frame returns these without coding any bits (§5.9.11).
        let (_, fh) = parse_built(&crate::decode::testutil::StillBuilder::default());
        assert!(fh.coded_lossless);
        assert_eq!(fh.loop_filter.ref_deltas, [1, 0, 0, 0, -1, 0, -1, -1]);
        assert_eq!(fh.loop_filter.mode_deltas, [0, 0]);
        assert_eq!(fh.loop_filter.level, [0; 4]);
        assert!(!fh.loop_filter.delta_enabled);
        assert_eq!(fh.loop_filter.sharpness, 0);
    }

    #[test]
    fn cdef_params_are_skipped_when_coded_lossless_even_with_cdef_enabled() {
        // §5.9.19 returns early on `CodedLossless || allow_intrabc || !enable_cdef`. With CDEF
        // enabled in the sequence header, only the CodedLossless term can skip the block — and it
        // must, or the parser would consume strength bits that were never written.
        let still = crate::decode::testutil::StillBuilder {
            enable_cdef: true,
            ..crate::decode::testutil::StillBuilder::default()
        };
        let (seq, fh) = parse_built(&still);
        assert!(seq.enable_cdef, "the sequence header enables CDEF");
        assert!(fh.coded_lossless);
        assert_eq!(fh.cdef, CdefParams::disabled());
        // Everything after cdef_params still lands correctly, which is what proves no bits were
        // consumed by mistake.
        assert!(fh.reduced_tx_set);
        assert_eq!(fh.tx_mode, TxMode::Only4x4);
    }

    #[test]
    fn frame_size_override_codes_the_frames_own_dimensions() {
        // §5.9.5: with the override flag the frame codes its dimensions with the *sequence
        // header's* bit widths, independent of `max_frame_*`.
        let still = crate::decode::testutil::StillBuilder {
            width: 256,
            height: 128,
            width_bits: 12,
            height_bits: 11,
            frame_size_override: true,
            coded_size: (200, 100),
            ..crate::decode::testutil::StillBuilder::default()
        };
        let (_, fh) = parse_built(&still);
        assert_eq!(fh.frame_width, 200, "the coded width, not max_frame_width");
        assert_eq!(fh.frame_height, 100);
        assert_eq!(fh.upscaled_width, 200);
        // MiCols/MiRows follow the coded size (§5.9.9).
        assert_eq!(fh.mi_cols, 2 * ((200 + 7) >> 3));
        assert_eq!(fh.mi_rows, 2 * ((100 + 7) >> 3));
        // Without the override the sequence header's maximum is used instead.
        let (_, plain) = parse_built(&crate::decode::testutil::StillBuilder {
            frame_size_override: false,
            ..still
        });
        assert_eq!(plain.frame_width, 256);
        assert_eq!(plain.frame_height, 128);
    }

    #[test]
    fn an_explicit_render_size_is_read_as_coded() {
        // §5.9.6: `render_and_frame_size_different` codes a 16-bit minus-one pair; otherwise the
        // render size follows UpscaledWidth/FrameHeight.
        let still = crate::decode::testutil::StillBuilder {
            render_size: Some((1920, 1080)),
            ..crate::decode::testutil::StillBuilder::default()
        };
        let (_, fh) = parse_built(&still);
        assert_eq!((fh.render_width, fh.render_height), (1920, 1080));
        assert_eq!(
            fh.frame_width, 64,
            "the render size does not change the coded size"
        );

        let (_, plain) = parse_built(&crate::decode::testutil::StillBuilder::default());
        assert_eq!((plain.render_width, plain.render_height), (64, 64));
    }

    #[test]
    fn an_intra_only_frame_codes_its_refresh_mask() {
        // §5.9.2: a shown KEY_FRAME infers `refresh_frame_flags = allFrames` and codes no bits;
        // an INTRA_ONLY_FRAME codes the 8-bit mask (and `error_resilient_mode` before it). Both
        // must leave the rest of the header at the same bit position.
        for mask in [0xffu8, 0x01, 0x00] {
            let still = crate::decode::testutil::StillBuilder {
                intra_only: true,
                refresh_frame_flags: mask,
                ..crate::decode::testutil::StillBuilder::default()
            };
            let (_, fh) = parse_built(&still);
            assert_eq!(
                fh.frame_width, 64,
                "mask {mask:#x} must not desync the header"
            );
            assert!(fh.coded_lossless);
            assert!(fh.reduced_tx_set);
        }
    }

    #[test]
    fn an_explicit_screen_content_tools_force_is_honoured() {
        // §5.9.2 codes `allow_screen_content_tools` only when the sequence header chose SELECT;
        // otherwise the sequence header's forced value stands. Getting that test backwards would
        // consume — or fail to consume — the frame header's flag and desync the rest.
        for (force, expected) in [(0u8, false), (1, true)] {
            let still = crate::decode::testutil::StillBuilder {
                screen_content_tools: Some(force),
                ..crate::decode::testutil::StillBuilder::default()
            };
            let (seq, fh) = parse_built(&still);
            assert_eq!(seq.seq_force_screen_content_tools, force);
            assert_eq!(
                fh.allow_screen_content_tools, expected,
                "seq_force_screen_content_tools = {force} forces allow = {expected}"
            );
            assert_eq!(
                fh.frame_width, 64,
                "force {force} must not desync the header"
            );
            assert!(fh.reduced_tx_set);
        }

        // The SELECT case still reads the frame header's own flag.
        let (seq, fh) = parse_built(&crate::decode::testutil::StillBuilder::default());
        assert_eq!(
            seq.seq_force_screen_content_tools,
            SELECT_SCREEN_CONTENT_TOOLS
        );
        assert!(!fh.allow_screen_content_tools);
    }

    #[test]
    fn a_full_refresh_mask_suppresses_the_reference_order_hints() {
        // §5.9.2 reads `ref_order_hint[i]` only when `refresh_frame_flags != allFrames` (and the
        // frame is error-resilient with order hints on). `allFrames` is `(1 << 8) - 1 = 255`, so
        // a mask of 0xff must read no hints while 0x01 reads eight — a wrong constant would
        // consume the wrong number of bits and desync everything after.
        let base = crate::decode::testutil::StillBuilder {
            intra_only: true,
            error_resilient: true,
            order_hint_bits: Some(7),
            ..crate::decode::testutil::StillBuilder::default()
        };

        let (seq, fh) = parse_built(&crate::decode::testutil::StillBuilder {
            refresh_frame_flags: 0xff,
            ..base
        });
        assert!(seq.enable_order_hint);
        assert_eq!(seq.order_hint_bits, 7);
        assert_eq!(fh.frame_width, 64, "a full mask reads no ref_order_hint");
        assert!(fh.reduced_tx_set);

        let (_, partial) = parse_built(&crate::decode::testutil::StillBuilder {
            refresh_frame_flags: 0x01,
            ..base
        });
        assert_eq!(
            partial.frame_width, 64,
            "a partial mask reads eight ref_order_hint fields"
        );
        assert!(partial.reduced_tx_set);
    }

    #[test]
    fn frame_id_numbers_widen_the_frame_header_by_their_sum() {
        // §5.9.2: idLen = additional_frame_id_length + delta_frame_id_length, and
        // `current_frame_id` occupies exactly that many bits. Reading the wrong width desyncs
        // everything after it, so the rest of the header is asserted rather than the id itself
        // (which this decoder discards). additional 1 + delta 2 = 3 bits, where a product gives 2.
        for (additional, delta) in [(1u32, 2u32), (3, 5), (8, 2)] {
            let still = crate::decode::testutil::StillBuilder {
                frame_id_lengths: Some((additional, delta)),
                ..crate::decode::testutil::StillBuilder::default()
            };
            let (seq, fh) = parse_built(&still);
            assert!(seq.frame_id_numbers_present);
            assert_eq!(seq.additional_frame_id_length, additional);
            assert_eq!(seq.delta_frame_id_length, delta);
            assert_eq!(
                fh.frame_width, 64,
                "id length {additional}+{delta} must not desync the header"
            );
            assert!(fh.coded_lossless);
            assert!(fh.reduced_tx_set);
        }
    }

    #[test]
    fn tile_info_uses_the_128x128_superblock_geometry() {
        // §5.9.15 derives sbCols/sbRows and sbSize from `use_128x128_superblock`; a 128-wide
        // frame is two 64-superblocks but only one 128-superblock, so the two grids differ.
        let base = crate::decode::testutil::StillBuilder {
            width: 128,
            height: 128,
            width_bits: 8,
            height_bits: 8,
            tile_cols_log2: 1,
            ..crate::decode::testutil::StillBuilder::default()
        };
        let (seq64, small) = parse_built(&base);
        assert!(!seq64.use_128x128_superblock);
        assert_eq!(small.tile_info.tile_cols, 2, "two 64x64 superblock columns");
        assert_eq!(small.tile_info.mi_col_starts, vec![0, 16, small.mi_cols]);

        let (seq128, large) = parse_built(&crate::decode::testutil::StillBuilder {
            use_128x128_superblock: true,
            tile_cols_log2: 0,
            ..base
        });
        assert!(seq128.use_128x128_superblock);
        assert_eq!(
            large.tile_info.tile_cols, 1,
            "the same frame is a single 128x128 superblock column"
        );
        assert_eq!(large.tile_info.mi_col_starts, vec![0, large.mi_cols]);
    }

    #[test]
    fn tile_info_derives_the_tile_limits_from_the_superblock_size() {
        // `maxTileWidthSb = MAX_TILE_WIDTH >> sbSize` and
        // `maxTileAreaSb = MAX_TILE_AREA >> (2 * sbSize)` set `minLog2TileCols` and
        // `minLog2Tiles`, which force a minimum number of tiles once a frame is wide or large
        // enough. A frame wider than MAX_TILE_WIDTH cannot be one tile column.
        let wide = crate::decode::testutil::StillBuilder {
            width: 8192,
            height: 128,
            width_bits: 13,
            height_bits: 8,
            ..crate::decode::testutil::StillBuilder::default()
        };
        let (_, fh) = parse_built(&wide);
        assert!(
            fh.tile_info.tile_cols >= 2,
            "8192 px exceeds MAX_TILE_WIDTH, so one tile column is illegal"
        );
        assert_eq!(fh.tile_info.tile_cols_log2, 1);

        // A frame whose area exceeds MAX_TILE_AREA forces extra tiles through minLog2Tiles even
        // when each column is narrow enough.
        let big = crate::decode::testutil::StillBuilder {
            width: 4096,
            height: 4096,
            width_bits: 12,
            height_bits: 12,
            ..crate::decode::testutil::StillBuilder::default()
        };
        let (_, fh) = parse_built(&big);
        assert!(
            fh.tile_info.tile_cols * fh.tile_info.tile_rows >= 2,
            "a 4096x4096 frame exceeds MAX_TILE_AREA for a single tile"
        );
    }

    #[test]
    fn tile_info_spaces_multiple_tile_rows_uniformly() {
        // 256x256 is 4x4 64x64-superblocks. `tile_rows_log2 = 1` splits it into two tile rows of
        // `ceil(4 / 2) = 2` superblocks, so the second row starts at superblock 2 — MI unit
        // `2 << sbShift` = 32. Every other case in this suite has a single tile row, where both
        // the ceiling term and the `<< sbShift` are unobservable: the loop pushes one start of 0
        // whatever they compute.
        let still = crate::decode::testutil::StillBuilder {
            width: 256,
            height: 256,
            width_bits: 8,
            height_bits: 8,
            tile_rows_log2: 1,
            ..crate::decode::testutil::StillBuilder::default()
        };
        let (_, fh) = parse_built(&still);
        assert_eq!(fh.tile_info.tile_rows, 2);
        assert_eq!(fh.tile_info.mi_row_starts, [0, 32, 64]);
        assert_eq!(fh.tile_info.tile_rows_log2, 1);
        // The columns stay a single tile, so the row spacing is the only thing under test.
        assert_eq!(fh.tile_info.tile_cols, 1);
        assert_eq!(fh.tile_info.mi_col_starts, [0, 64]);
    }

    #[test]
    fn tile_info_reads_explicit_tile_spacing() {
        // `uniform_tile_spacing_flag = 0` codes every tile's width and height as an `ns()` size
        // instead of deriving them from a log2 count (§5.9.15). Neither `gamut-av1`'s encoder nor
        // libaom ever writes this branch — both emit uniform spacing — so `StillBuilder` is the
        // only way to reach it. A 2x2 grid over 4x4 superblocks puts each boundary at superblock
        // 2, MI unit 32.
        let still = crate::decode::testutil::StillBuilder {
            width: 256,
            height: 256,
            width_bits: 8,
            height_bits: 8,
            explicit_tiles: Some((&[2, 2], &[2, 2])),
            ..crate::decode::testutil::StillBuilder::default()
        };
        let (_, fh) = parse_built(&still);
        assert_eq!(fh.tile_info.tile_cols, 2);
        assert_eq!(fh.tile_info.tile_rows, 2);
        assert_eq!(fh.tile_info.mi_col_starts, [0, 32, 64]);
        assert_eq!(fh.tile_info.mi_row_starts, [0, 32, 64]);
        assert_eq!(fh.tile_info.tile_cols_log2, 1);
        assert_eq!(fh.tile_info.tile_rows_log2, 1);
    }

    #[test]
    fn lr_params_are_skipped_when_all_lossless_even_with_restoration_enabled() {
        // §5.9.20 returns the disabled defaults without coding a bit once AllLossless holds, even
        // though the sequence header enabled restoration. Reading `lr_type` here would consume
        // the bits that follow it and desync the rest of the header.
        let still = crate::decode::testutil::StillBuilder {
            enable_restoration: true,
            ..crate::decode::testutil::StillBuilder::default()
        };
        let (seq, fh) = parse_built(&still);
        assert!(seq.enable_restoration);
        assert!(fh.all_lossless);
        assert_eq!(fh.lr.frame_restoration_type, [RestorationType::None; 3]);
        assert!(!fh.lr.uses_lr);
        assert_eq!(fh.lr.loop_restoration_size, [RESTORATION_TILESIZE_MAX; 3]);
        // The bit `lr_params()` must not have eaten.
        assert!(fh.reduced_tx_set);
    }

    #[test]
    fn superres_rounds_with_half_the_denominator() {
        // §5.9.8: FrameWidth = (UpscaledWidth * 8 + SuperresDenom / 2) / SuperresDenom. The
        // `/ 2` rounding term changes the result for an odd denominator, so coded_denom 2
        // (SuperresDenom 11) pins it: (64*8 + 5) / 11 = 47, where dropping or mis-taking the
        // term gives 46.
        let (_, fh) = parse_encoder_headers(64, 64, 100, Some(2));
        assert_eq!(fh.superres_denom, 11);
        assert_eq!(fh.upscaled_width, 64);
        assert_eq!(fh.frame_width, (64 * 8 + 11 / 2) / 11);
        assert_eq!(fh.frame_width, 47);
    }

    #[test]
    fn tile_log2_matches_the_spec_definition() {
        assert_eq!(tile_log2(1, 1), 0);
        assert_eq!(tile_log2(1, 2), 1);
        assert_eq!(tile_log2(1, 3), 2);
        assert_eq!(tile_log2(1, 4), 2);
        assert_eq!(tile_log2(1, 5), 3);
        assert_eq!(tile_log2(64, 64), 0);
        assert_eq!(tile_log2(64, 65), 1);
    }

    #[test]
    fn secondary_strength_remaps_only_three() {
        assert_eq!(remap_sec_strength(0), 0);
        assert_eq!(remap_sec_strength(1), 1);
        assert_eq!(remap_sec_strength(2), 2);
        assert_eq!(remap_sec_strength(3), 4);
    }

    #[test]
    fn get_qindex_applies_the_segment_delta_and_clips() {
        let quant = QuantizationParams {
            base_q_idx: 100,
            delta_q_y_dc: 0,
            delta_q_u_dc: 0,
            delta_q_u_ac: 0,
            delta_q_v_dc: 0,
            delta_q_v_ac: 0,
            using_qmatrix: false,
            qm: [0; 3],
        };
        let mut seg = SegmentationParams::disabled();
        assert_eq!(get_qindex_ignoring_delta(&seg, &quant, 0), 100);

        seg.enabled = true;
        seg.feature_enabled[0][SEG_LVL_ALT_Q] = true;
        seg.feature_data[0][SEG_LVL_ALT_Q] = -30;
        assert_eq!(get_qindex_ignoring_delta(&seg, &quant, 0), 70);

        // Clipping at both ends.
        seg.feature_data[0][SEG_LVL_ALT_Q] = -255;
        assert_eq!(get_qindex_ignoring_delta(&seg, &quant, 0), 0);
        seg.feature_data[0][SEG_LVL_ALT_Q] = 255;
        assert_eq!(get_qindex_ignoring_delta(&seg, &quant, 0), 255);

        // An inactive segment ignores the data even when the array holds a value.
        assert_eq!(get_qindex_ignoring_delta(&seg, &quant, 1), 100);
    }

    #[test]
    fn refuses_the_tools_that_are_not_implemented() {
        let (seq, fh) = parse_encoder_headers(64, 64, 100, None);

        let mut grain = fh.clone();
        grain.apply_grain = true;
        assert_eq!(
            grain
                .reject_unsupported_tools(&seq)
                .unwrap_err()
                .static_message(),
            Some("AV1 decode: film grain synthesis is not implemented")
        );

        let mut ibc = fh.clone();
        ibc.allow_intrabc = true;
        assert_eq!(
            ibc.reject_unsupported_tools(&seq)
                .unwrap_err()
                .static_message(),
            Some("AV1 decode: intra block copy (allow_intrabc) is not implemented")
        );

        let mut deep = seq;
        deep.color.bit_depth = 10;
        assert_eq!(
            fh.reject_unsupported_tools(&deep)
                .unwrap_err()
                .static_message(),
            Some("AV1 decode: only 8-bit streams are implemented")
        );

        let mut sub = seq;
        sub.color.subsampling = Subsampling::Yuv420;
        assert_eq!(
            fh.reject_unsupported_tools(&sub)
                .unwrap_err()
                .static_message(),
            Some("AV1 decode: only 4:4:4 chroma is implemented")
        );

        // Monochrome is its own deferred row, and its own diagnostic: this crate encodes
        // monochrome stills, so refusing one with a message about chroma subsampling would name a
        // reason that does not exist for a stream carrying no chroma.
        let mut mono = seq;
        mono.color.subsampling = Subsampling::Monochrome;
        assert_eq!(
            fh.reject_unsupported_tools(&mono)
                .unwrap_err()
                .static_message(),
            Some("AV1 decode: monochrome streams are not implemented")
        );
    }

    #[test]
    fn tile_info_recomputes_the_row_area_limit_from_the_coded_columns() {
        // §5.9.15's explicit-spacing branch recomputes `maxTileAreaSb` *after* the columns are
        // read: `(sbRows * sbCols) >> (minLog2Tiles + 1)` when `minLog2Tiles > 0`, the whole frame
        // area otherwise. That sets `maxTileHeightSb`, the `ns()` bound every row size is coded
        // against, so a wrong area reads the wrong number of bits per row.
        //
        // 3136 px is 49 64x64 superblocks each way. 49 * 49 = 2401 exceeds `maxTileAreaSb` (2304),
        // so `minLog2Tiles` is 1 and the recomputed area is 2401 >> 2 = 600. A widest column of 25
        // superblocks then caps a tile row at 600 / 25 = 24, which is why the rows below are
        // 17 + 16 + 16 and not a single 49. Taking the `else` branch would allow 96, and dividing
        // instead of multiplying would collapse the cap to 1 and split the frame into 49 rows.
        let still = crate::decode::testutil::StillBuilder {
            width: 3136,
            height: 3136,
            width_bits: 12,
            height_bits: 12,
            explicit_tiles: Some((&[25, 24], &[17, 16, 16])),
            ..crate::decode::testutil::StillBuilder::default()
        };
        let (_, fh) = parse_built(&still);
        assert_eq!(fh.tile_info.tile_cols, 2);
        assert_eq!(fh.tile_info.tile_rows, 3);
        assert_eq!(fh.tile_info.mi_col_starts, [0, 400, 784]);
        assert_eq!(fh.tile_info.mi_row_starts, [0, 272, 528, 784]);
        assert_eq!(fh.tile_info.tile_cols_log2, 1);
        assert_eq!(fh.tile_info.tile_rows_log2, 2);
    }

    #[test]
    fn loop_filter_codes_the_chroma_levels_when_either_luma_level_is_set() {
        // §5.9.11 codes `loop_filter_level[2]` and `[3]` when NumPlanes > 1 and *either* luma
        // level is non-zero. gamut's encoder derives both luma levels from `base_q_idx` and so
        // always writes them equal; only a hand-built header separates the spec's `||` from `&&`.
        let still = crate::decode::testutil::StillBuilder {
            base_q_idx: 128,
            loop_filter_level: [0, 5, 17, 34],
            ..crate::decode::testutil::StillBuilder::default()
        };
        let (_, fh) = parse_built(&still);
        assert!(!fh.coded_lossless, "base_q_idx 128 codes the filter blocks");
        assert_eq!(fh.loop_filter.level, [0, 5, 17, 34]);
        assert_eq!(fh.tx_mode, TxMode::Largest);

        // The mirror case: with both luma levels zero no chroma level is coded at all, and the
        // §7.20 ref-delta defaults still stand because `loop_filter_delta_enabled` is 0.
        let (_, quiet) = parse_built(&crate::decode::testutil::StillBuilder {
            loop_filter_level: [0, 0, 17, 34],
            ..still
        });
        assert_eq!(quiet.loop_filter.level, [0; 4]);
        assert_eq!(quiet.loop_filter.ref_deltas, [1, 0, 0, 0, -1, 0, -1, -1]);
    }

    #[test]
    fn lr_params_code_the_uv_shift_only_for_subsampled_chroma_restoration() {
        // §5.9.20 codes `lr_uv_shift` only when `subsampling_x`, `subsampling_y` and `usesChromaLr`
        // all hold, and `usesChromaLr` is set by a plane *after* the first choosing a restoration
        // type. gamut's encoder emits 4:4:4 with Wiener on luma alone, so it exercises neither.
        let still = crate::decode::testutil::StillBuilder {
            colour: crate::decode::testutil::BuilderColour::Yuv420,
            base_q_idx: 128,
            enable_restoration: true,
            lr: crate::decode::testutil::LrSpec {
                // Wiener on Y and U, none on V: the chroma term comes from plane 1.
                types: [2, 2, 0],
                unit_shift: 2,
                uv_shift: 1,
            },
            ..crate::decode::testutil::StillBuilder::default()
        };
        let (seq, fh) = parse_built(&still);
        assert_eq!(seq.color.subsampling, Subsampling::Yuv420);
        assert!(fh.lr.uses_lr);
        assert_eq!(
            fh.lr.frame_restoration_type,
            [
                RestorationType::Wiener,
                RestorationType::Wiener,
                RestorationType::None
            ]
        );
        // LoopRestorationSize[0] = 256 >> (2 - 2); the chroma planes take a further lr_uv_shift.
        assert_eq!(fh.lr.loop_restoration_size, [256, 128, 128]);

        // Luma-only restoration leaves `usesChromaLr` clear, so no `lr_uv_shift` is coded and the
        // chroma sizes follow the luma one.
        let (_, luma_only) = parse_built(&crate::decode::testutil::StillBuilder {
            lr: crate::decode::testutil::LrSpec {
                types: [2, 0, 0],
                unit_shift: 2,
                uv_shift: 0,
            },
            ..still
        });
        assert!(luma_only.lr.uses_lr);
        assert_eq!(luma_only.lr.loop_restoration_size, [256; 3]);
    }

    #[test]
    fn film_grain_params_are_consumed_at_their_full_width() {
        // §5.9.30 is parsed purely for its width — the decoder keeps only `apply_grain` — so the
        // bit-position check in `parse_built` is what pins every field. `film_grain_params_present`
        // is 0 in everything either encoder emits, so this syntax is otherwise never entered.
        //
        // 4:4:4 with luma *and* chroma points takes the widest path: `numPosLuma` is
        // 2 * 2 * 3 = 12 AR coefficients, `numPosChroma` is 13 because num_y_points is non-zero,
        // and both the cb and the cr multiplier triples are coded.
        let still = crate::decode::testutil::StillBuilder {
            film_grain: Some(crate::decode::testutil::GrainSpec {
                num_y_points: 3,
                chroma_scaling_from_luma: false,
                num_cb_points: 2,
                num_cr_points: 1,
                ar_coeff_lag: 2,
            }),
            ..crate::decode::testutil::StillBuilder::default()
        };
        let (seq, fh) = parse_built(&still);
        assert!(seq.film_grain_params_present);
        assert!(fh.apply_grain);
        assert_eq!(
            fh.reject_unsupported_tools(&seq)
                .unwrap_err()
                .static_message(),
            Some("AV1 decode: film grain synthesis is not implemented")
        );

        // `apply_grain = 0` still costs the one flag and nothing else.
        let (_, none) = parse_built(&crate::decode::testutil::StillBuilder {
            film_grain: None,
            ..still
        });
        assert!(!none.apply_grain);
    }

    #[test]
    fn film_grain_omits_the_chroma_points_for_subsampled_grain_without_luma_points() {
        // §5.9.30 forces `num_cb_points` and `num_cr_points` to 0 — coding neither field, neither
        // set of AR coefficients, and neither multiplier triple — when the stream is 4:2:0 and
        // `num_y_points` is 0. It is the one input where all three terms of that condition are
        // load-bearing, and where `chroma_scaling_from_luma || num_cb_points` must stay a genuine
        // test rather than an always-true one.
        let still = crate::decode::testutil::StillBuilder {
            colour: crate::decode::testutil::BuilderColour::Yuv420,
            film_grain: Some(crate::decode::testutil::GrainSpec {
                num_y_points: 0,
                chroma_scaling_from_luma: false,
                num_cb_points: 0,
                num_cr_points: 0,
                // numPosLuma = 12, and with no luma points numPosChroma is 12 too — coefficients
                // that must not be read at all here.
                ar_coeff_lag: 2,
            }),
            ..crate::decode::testutil::StillBuilder::default()
        };
        let (seq, fh) = parse_built(&still);
        assert_eq!(seq.color.subsampling, Subsampling::Yuv420);
        assert!(seq.film_grain_params_present);
        assert!(fh.apply_grain);
    }

    #[test]
    fn rejects_a_truncated_frame_header() {
        let cfg = crate::decode::testutil::still_config(64, 64, Av1Colour::default());
        let seq_payload = crate::headers::sequence_header_payload(&cfg, 64, 64, true, false);
        let seq = SequenceHeader::parse(&seq_payload).unwrap();
        let layout = crate::headers::TileLayout::resolve(
            crate::headers::Tiling::default(),
            64,
            64,
            cfg.seq_level_idx_0,
            None,
        );
        let frame = crate::headers::frame_header_payload(
            &layout,
            100,
            None,
            false,
            crate::filter::FrameFilters::default(),
            true,
        );
        for cut in 0..frame.len() {
            let mut r = BitReader::new(&frame[..cut]);
            assert!(
                FrameHeader::parse(&mut r, &seq).is_err(),
                "truncation to {cut} bytes must be refused"
            );
        }
    }
}
