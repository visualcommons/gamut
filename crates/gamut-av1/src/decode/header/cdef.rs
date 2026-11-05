//! `cdef_params()` (§5.9.19): the CDEF damping and strength sets.

use gamut_bitstream::BitReader;
use gamut_core::Result;

/// `cdef_params()` (§5.9.19). Strengths are stored post-mapping (a coded secondary of 3 becomes 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
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
    pub(super) const fn disabled() -> Self {
        Self {
            damping: 3,
            bits: 0,
            y_pri: [0; 8],
            y_sec: [0; 8],
            uv_pri: [0; 8],
            uv_sec: [0; 8],
        }
    }

    /// `cdef_params()` (§5.9.19).
    pub(super) fn parse(
        r: &mut BitReader<'_>,
        skipped: bool,
        num_planes: usize,
    ) -> Result<CdefParams> {
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
}

/// The §5.9.19 secondary-strength remap: a coded 3 means 4.
const fn remap_sec_strength(coded: u8) -> u8 {
    if coded == 3 { 4 } else { coded }
}

#[cfg(test)]
mod tests {
    use gamut_color::ChromaSubsampling;

    use super::*;
    use crate::decode::header::TxMode;
    use crate::decode::header::tests::parse_built;

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
    fn secondary_strength_remaps_only_three() {
        assert_eq!(remap_sec_strength(0), 0);
        assert_eq!(remap_sec_strength(1), 1);
        assert_eq!(remap_sec_strength(2), 2);
        assert_eq!(remap_sec_strength(3), 4);
    }

    #[test]
    fn cdef_params_code_one_strength_set_per_cdef_bits_value() {
        // §5.9.19 reads `1 << cdef_bits` strength sets. Every other fixture has `cdef_bits = 0`,
        // where one set is read whatever the shift does.
        let still = crate::decode::testutil::StillBuilder {
            base_q_idx: 128,
            enable_cdef: true,
            cdef_bits: 1,
            ..crate::decode::testutil::StillBuilder::default()
        };
        let (_, fh) = parse_built(&still);
        assert_eq!(fh.cdef.bits, 1);
        assert_eq!(fh.cdef.y_pri[..2], [1, 2]);
        assert_eq!(fh.cdef.uv_pri[..2], [1, 2]);
    }

    #[test]
    fn a_monochrome_frame_codes_no_chroma_cdef_strengths() {
        // §5.9.19 codes `cdef_uv_pri_strength` / `cdef_uv_sec_strength` only when NumPlanes > 1.
        // Two strength sets put a luma strength after the first set, so reading chroma strengths
        // for the one plane would misplace it as well as overrun the header.
        let still = crate::decode::testutil::StillBuilder {
            colour: crate::decode::testutil::BuilderColour::Monochrome,
            base_q_idx: 128,
            enable_cdef: true,
            cdef_bits: 1,
            ..crate::decode::testutil::StillBuilder::default()
        };
        let (seq, fh) = parse_built(&still);
        assert_eq!(seq.color.subsampling, ChromaSubsampling::Cs400);
        assert_eq!(fh.cdef.y_pri[..2], [1, 2]);
        assert_eq!(fh.cdef.uv_pri, [0; 8]);
    }
}
