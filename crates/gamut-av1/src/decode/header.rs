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
//!
//! This module holds `uncompressed_header()` itself — the frame type, size, render size and
//! superres (§5.9.2/§5.9.5/§5.9.6/§5.9.8), the delta-q/delta-lf flags, the lossless derivation and
//! `read_tx_mode()` — and each nested sub-syntax with its own parameter type is a submodule:
//! [`tile_info`] (§5.9.15), [`quantization`] (§5.9.12/§5.9.13), [`segmentation`] (§5.9.14),
//! [`loop_filter`] (§5.9.11), [`cdef`] (§5.9.19), [`restoration`] (§5.9.20) and [`film_grain`]
//! (§5.9.30).

mod cdef;
mod film_grain;
mod loop_filter;
mod quantization;
mod restoration;
mod segmentation;
mod tile_info;

pub use cdef::CdefParams;
use gamut_bitstream::BitReader;
use gamut_color::{BitDepth, ChromaSubsampling};
use gamut_core::{Error, Result};
pub use loop_filter::LoopFilterParams;
pub use quantization::QuantizationParams;
pub use restoration::{LrParams, RestorationType};
pub use segmentation::SegmentationParams;
use segmentation::{MAX_SEGMENTS, get_qindex_ignoring_delta};
pub use tile_info::TileInfo;
// The decode test fixtures size a synthetic tile grid with the parser's own `tile_log2`.
#[cfg(test)]
pub(crate) use tile_info::tile_log2;

use super::ORIGIN;
use super::obu::{SELECT_INTEGER_MV, SELECT_SCREEN_CONTENT_TOOLS, SequenceHeader};

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

/// The parsed uncompressed frame header (§5.9.2), plus the derived values §5.9.2 computes.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
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
    /// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error for a
    /// malformed or truncated header, or an
    /// [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) error for a frame this image decoder does not code — anything but an
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

        let tile_info = TileInfo::parse(r, seq, mi_cols, mi_rows)?;
        let quant = QuantizationParams::parse(r, seq)?;
        let segmentation = SegmentationParams::parse(r)?;

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

        let loop_filter = LoopFilterParams::parse(
            r,
            coded_lossless || allow_intrabc,
            seq.color.subsampling.num_planes(),
        )?;
        let cdef = CdefParams::parse(
            r,
            coded_lossless || allow_intrabc || !seq.enable_cdef,
            seq.color.subsampling.num_planes(),
        )?;
        let lr = LrParams::parse(r, seq, all_lossless || allow_intrabc)?;

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
        let apply_grain = film_grain::film_grain_params(r, seq)?;

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

    /// Refuses the tools this decoder parses but does not implement.
    ///
    /// Called once the header is complete, so the message names the tool rather than surfacing as
    /// a desync deeper in the tile data. Each refusal corresponds to a ☐ row in
    /// `gamut-avif/STATUS.md`.
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) error naming the
    /// first unimplemented tool the header signals.
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
        if seq.color.bit_depth != BitDepth::Eight {
            return Err(Error::unsupported(
                ORIGIN,
                "AV1 decode: only 8-bit streams are implemented",
            ));
        }
        // Monochrome carries no chroma at all, so a subsampling diagnostic would name the wrong
        // reason for a stream this crate's own encoder can emit.
        if seq.color.subsampling == ChromaSubsampling::Cs400 {
            return Err(Error::unsupported(
                ORIGIN,
                "AV1 decode: monochrome streams are not implemented",
            ));
        }
        if seq.color.subsampling != ChromaSubsampling::Cs444 {
            return Err(Error::unsupported(
                ORIGIN,
                "AV1 decode: only 4:4:4 chroma is implemented",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::obu::ObuIter;
    use crate::filter::{CdefStrengths, FrameFilters};
    use crate::headers::Av1Color;
    use crate::tile::BlockVariation;

    /// The in-loop filters [`parse_encoder_headers`] has the encoder signal: a non-zero value in
    /// every field, so each reads back as itself rather than as a default.
    pub(super) const ENCODER_FILTERS: FrameFilters = FrameFilters {
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
        parse_encoder_headers_with(
            width,
            height,
            base_q_idx,
            superres,
            ENCODER_FILTERS,
            true,
            BlockVariation::Frame,
        )
    }

    /// The headers the encoder writes for a frame whose in-loop filters are `filters`, with luma
    /// restoration signalled when `luma_lr` and the block variation `variation`.
    pub(super) fn parse_encoder_headers_with(
        width: u32,
        height: u32,
        base_q_idx: u8,
        superres: Option<u8>,
        filters: FrameFilters,
        luma_lr: bool,
        variation: BlockVariation,
    ) -> (SequenceHeader, FrameHeader) {
        let lossy = base_q_idx > 0;
        let cfg = crate::decode::testutil::still_config(width, height, Av1Color::default());
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
            &layout, base_q_idx, superres, false, filters, luma_lr, variation,
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
        r.align().unwrap();
        if fh.tile_info.tile_cols * fh.tile_info.tile_rows > 1 {
            assert!(!r.flag().unwrap(), "tile_start_and_end_present_flag");
            r.align().unwrap();
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
            // Every block at the frame quantizer and loop-filter level (#1011).
            assert!(!fh.segmentation.enabled, "q={q} codes no segmentation");
            assert!(!fh.delta_q_present, "q={q} codes no delta_q");
            assert!(!fh.delta_lf_present, "q={q} codes no delta_lf");
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
        let (_, fh) =
            parse_encoder_headers_with(128, 96, 100, None, filters, false, BlockVariation::Frame);
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
    fn the_block_variation_exercise_signals_segmentation_delta_q_and_delta_lf() {
        for q in [20u8, 60, 120, 200] {
            let (seq, fh) = parse_encoder_headers_with(
                128,
                96,
                q,
                None,
                ENCODER_FILTERS,
                true,
                BlockVariation::SyntaxExercise,
            );
            assert!(fh.segmentation.enabled, "q={q} enables segmentation");
            assert!(fh.delta_q_present);
            assert_eq!(fh.delta_q_res, 0);
            assert!(fh.delta_lf_present);
            assert_eq!(fh.delta_lf_res, 0);
            assert!(!fh.delta_lf_multi);
            // The fields after the deltas still read back as signalled.
            assert_eq!(fh.loop_filter.level, [ENCODER_FILTERS.deblock_level; 4]);
            fh.reject_unsupported_tools(&seq).unwrap();
        }
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
    pub(super) fn parse_built(
        still: &crate::decode::testutil::StillBuilder,
    ) -> (SequenceHeader, FrameHeader) {
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
        deep.color.bit_depth = BitDepth::Ten;
        assert_eq!(
            fh.reject_unsupported_tools(&deep)
                .unwrap_err()
                .static_message(),
            Some("AV1 decode: only 8-bit streams are implemented")
        );

        let mut sub = seq;
        sub.color.subsampling = ChromaSubsampling::Cs420;
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
        mono.color.subsampling = ChromaSubsampling::Cs400;
        assert_eq!(
            fh.reject_unsupported_tools(&mono)
                .unwrap_err()
                .static_message(),
            Some("AV1 decode: monochrome streams are not implemented")
        );
    }

    #[test]
    fn rejects_a_truncated_frame_header() {
        let cfg = crate::decode::testutil::still_config(64, 64, Av1Color::default());
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
            // The longest lossy header: segmentation and the delta params are coded too.
            crate::tile::BlockVariation::SyntaxExercise,
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
