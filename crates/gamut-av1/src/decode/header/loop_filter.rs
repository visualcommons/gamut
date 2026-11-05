//! `loop_filter_params()` (§5.9.11): the deblocking levels, sharpness and deltas.

use gamut_bitstream::BitReader;
use gamut_core::Result;

/// `TOTAL_REFS_PER_FRAME` (§3).
const TOTAL_REFS_PER_FRAME: usize = 8;

/// `loop_filter_params()` (§5.9.11).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
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
    pub(super) const fn defaults() -> Self {
        Self {
            level: [0; 4],
            sharpness: 0,
            delta_enabled: false,
            // INTRA_FRAME = 1, GOLDEN/ALTREF/ALTREF2 = -1, the rest 0.
            ref_deltas: [1, 0, 0, 0, -1, 0, -1, -1],
            mode_deltas: [0; 2],
        }
    }

    /// `loop_filter_params()` (§5.9.11).
    pub(super) fn parse(
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
}

#[cfg(test)]
mod tests {
    use crate::decode::header::TxMode;
    use crate::decode::header::tests::parse_built;

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
    fn a_monochrome_frame_codes_no_chroma_filter_levels() {
        // §5.9.11 codes `loop_filter_level[2]` and `[3]` only when NumPlanes > 1, whatever the
        // luma levels are.
        let still = crate::decode::testutil::StillBuilder {
            colour: crate::decode::testutil::BuilderColour::Monochrome,
            base_q_idx: 128,
            loop_filter_level: [5, 9, 17, 34],
            ..crate::decode::testutil::StillBuilder::default()
        };
        let (_, fh) = parse_built(&still);
        assert_eq!(fh.loop_filter.level, [5, 9, 0, 0]);
    }
}
