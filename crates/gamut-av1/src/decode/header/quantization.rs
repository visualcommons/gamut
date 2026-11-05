//! `quantization_params()` (§5.9.12) and the `read_delta_q()` (§5.9.13) it is built from.

use gamut_bitstream::BitReader;
use gamut_core::Result;

use crate::decode::obu::SequenceHeader;

/// `quantization_params()` (§5.9.12).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
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

impl QuantizationParams {
    /// `quantization_params()` (§5.9.12).
    pub(super) fn parse(r: &mut BitReader<'_>, seq: &SequenceHeader) -> Result<QuantizationParams> {
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
}

/// `read_delta_q()` (§5.9.13).
fn read_delta_q(r: &mut BitReader<'_>) -> Result<i32> {
    if r.flag()? { r.su(7) } else { Ok(0) }
}

#[cfg(test)]
mod tests {
    use gamut_color::ChromaSubsampling;

    use crate::decode::header::tests::parse_built;

    #[test]
    fn a_monochrome_frame_codes_no_chroma_quantizer_deltas() {
        // §5.9.12 codes the U (and V) `delta_coded` flags only when NumPlanes > 1. Zero luma
        // filter levels keep `loop_filter_params()` from coding any chroma level either way.
        let still = crate::decode::testutil::StillBuilder {
            colour: crate::decode::testutil::BuilderColour::Monochrome,
            base_q_idx: 128,
            ..crate::decode::testutil::StillBuilder::default()
        };
        let (seq, fh) = parse_built(&still);
        assert_eq!(seq.color.subsampling, ChromaSubsampling::Cs400);
        assert_eq!(fh.quant.base_q_idx, 128);
    }
}
