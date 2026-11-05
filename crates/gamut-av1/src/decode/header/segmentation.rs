//! `segmentation_params()` (§5.9.14) and the segment-aware `get_qindex()` (§7.12.2) the header's
//! lossless derivation reads it through.

use gamut_bitstream::BitReader;
use gamut_core::Result;

use super::QuantizationParams;

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

/// `segmentation_params()` (§5.9.14).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
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
    pub(super) const fn disabled() -> Self {
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

    /// `segmentation_params()` (§5.9.14).
    ///
    /// `primary_ref_frame` is `PRIMARY_REF_NONE` for every frame this decoder accepts, so
    /// `segmentation_update_map` / `_temporal_update` / `_update_data` are inferred and the
    /// temporal-prediction branch is unreachable.
    pub(super) fn parse(r: &mut BitReader<'_>) -> Result<SegmentationParams> {
        if !r.flag()? {
            return Ok(SegmentationParams::disabled());
        }
        let mut params = SegmentationParams {
            enabled: true,
            update_map: true,
            // `segmentation_temporal_update` is inferred 0, which `disabled()` already holds.
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
}

/// `get_qindex( 1, segmentId )` (§7.12.2) — the `ignoreDeltaQ` form the header's `CodedLossless`
/// derivation uses, before any per-block `CurrentQIndex` exists.
pub(super) fn get_qindex_ignoring_delta(
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
    use crate::decode::header::tests::{ENCODER_FILTERS, parse_encoder_headers_with};
    use crate::tile::BlockVariation;

    #[test]
    fn segmentation_alt_q_survives_the_round_trip() {
        let (_, fh) = parse_encoder_headers_with(
            128,
            96,
            100,
            None,
            ENCODER_FILTERS,
            true,
            BlockVariation::SyntaxExercise,
        );
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
}
