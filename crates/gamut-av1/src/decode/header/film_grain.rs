//! `film_grain_params()` (§5.9.30), read for its width: grain synthesis is refused, not applied.

use gamut_bitstream::BitReader;
use gamut_core::Result;

use crate::decode::obu::{SequenceHeader, subsampling_x, subsampling_y};

/// `film_grain_params()` (§5.9.30). Returns `apply_grain`.
///
/// The parameters are consumed for their bit width even though this decoder does not
/// synthesise grain: they precede nothing else in the header, but reading them keeps the
/// trailing-bits check meaningful, and `apply_grain` is what
/// [`super::FrameHeader::reject_unsupported_tools`] refuses on.
pub(super) fn film_grain_params(r: &mut BitReader<'_>, seq: &SequenceHeader) -> Result<bool> {
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
        && !(subsampling_x(seq.color.subsampling) == 1
            && subsampling_y(seq.color.subsampling) == 1
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

#[cfg(test)]
mod tests {
    use gamut_color::ChromaSubsampling;

    use crate::decode::header::tests::parse_built;

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
        assert_eq!(seq.color.subsampling, ChromaSubsampling::Cs420);
        assert!(seq.film_grain_params_present);
        assert!(fh.apply_grain);
    }

    #[test]
    fn film_grain_codes_the_chroma_points_for_4_2_2_grain_without_luma_points() {
        // The §5.9.30 chroma-point exemption needs `subsampling_x`, `subsampling_y` *and*
        // `num_y_points == 0`. 4:2:2 with no luma points fails only the `subsampling_y` term, so
        // the counts and their multiplier triples are still coded.
        let still = crate::decode::testutil::StillBuilder {
            colour: crate::decode::testutil::BuilderColour::Yuv422,
            film_grain: Some(crate::decode::testutil::GrainSpec {
                num_y_points: 0,
                chroma_scaling_from_luma: false,
                num_cb_points: 1,
                num_cr_points: 2,
                ar_coeff_lag: 1,
            }),
            ..crate::decode::testutil::StillBuilder::default()
        };
        let (seq, fh) = parse_built(&still);
        assert_eq!(seq.color.subsampling, ChromaSubsampling::Cs422);
        assert!(fh.apply_grain);
    }
}
