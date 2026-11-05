//! `lr_params()` (§5.9.20): the per-plane loop-restoration type and unit size.

use gamut_bitstream::BitReader;
use gamut_core::Result;

use crate::decode::obu::{SequenceHeader, subsampling_x, subsampling_y};

/// `RESTORATION_TILESIZE_MAX` (§3).
const RESTORATION_TILESIZE_MAX: u32 = 256;

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

/// `lr_params()` (§5.9.20).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
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
    pub(super) const fn disabled() -> Self {
        Self {
            frame_restoration_type: [RestorationType::None; 3],
            loop_restoration_size: [RESTORATION_TILESIZE_MAX; 3],
            uses_lr: false,
        }
    }

    /// `lr_params()` (§5.9.20).
    pub(super) fn parse(
        r: &mut BitReader<'_>,
        seq: &SequenceHeader,
        skipped: bool,
    ) -> Result<LrParams> {
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
            let uv_shift = if subsampling_x(seq.color.subsampling) == 1
                && subsampling_y(seq.color.subsampling) == 1
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
}

#[cfg(test)]
mod tests {
    use gamut_color::ChromaSubsampling;

    use super::*;
    use crate::decode::header::tests::parse_built;

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
        assert_eq!(seq.color.subsampling, ChromaSubsampling::Cs420);
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
    fn lr_unit_size_under_128x128_superblocks_adds_the_implicit_shift() {
        // §5.9.20: with `use_128x128_superblock` the coded `lr_unit_shift` is incremented, so a
        // coded 0 is a shift of 1 and `LoopRestorationSize` is 256 >> (2 - 1) = 128. Every other
        // fixture codes a shift of 2, where `>> 0` and `<< 0` agree.
        let still = crate::decode::testutil::StillBuilder {
            use_128x128_superblock: true,
            base_q_idx: 128,
            enable_restoration: true,
            lr: crate::decode::testutil::LrSpec {
                types: [2, 0, 0],
                unit_shift: 1,
                uv_shift: 0,
            },
            ..crate::decode::testutil::StillBuilder::default()
        };
        let (_, fh) = parse_built(&still);
        assert_eq!(fh.lr.loop_restoration_size, [128; 3]);
    }
}
