//! VP8 key-frame decoder (RFC 6386 §9-§14): splits the control and token partitions, reads each
//! macroblock's segment, skip flag, luma/chroma modes and `B_PRED` submodes, decodes its coefficient
//! tokens, and reconstructs it through [`super::reconstruct`] before the final loop-filter pass.

// The macroblock/block math indexes several fixed-size arrays in lock-step (and over partial ranges
// like `1..16`), where explicit indices read closer to the spec than iterator adaptors.
#![allow(clippy::needless_range_loop)]

use gamut_core::{Error, Result};

use super::bool_coder::BoolDecoder;
use super::frame::{
    EntropyCtx, MB_SEGMENT_TREE, MbLevels, bmode_context, bmode_propagation, clear_mb_context,
    mb_has_coeffs,
};
use super::header::{self, UNCOMPRESSED_CHUNK_LEN};
use super::prediction::{self, B_DC_PRED, B_PRED};
use super::quant::{self, QuantFactors};
use super::reconstruct::{
    FrameBuffers, above_right_source, apply_loop_filter, predict_chroma, predict_luma,
    reconstruct_chroma, reconstruct_luma, segment_quant_factors, subblock_neighbors, write_block,
};
use super::tokens::{self, CoeffProbs};
use super::transform::idct4x4;

/// Reconstructs a skipped `B_PRED` macroblock's luma: each subblock is its prediction with no residual
/// (the encoder's all-zero-coefficient reconstruction).
fn reconstruct_bpred_zero(
    recon: &mut FrameBuffers,
    mb_x: usize,
    mb_y: usize,
    sub_modes: &[usize; 16],
    above_right: &[u8; 4],
) {
    let (px, py, rstride) = (mb_x * 16, mb_y * 16, recon.y_stride());
    for i in 0..16 {
        let (r, c) = (i / 4, i % 4);
        let (sx, sy) = (px + c * 4, py + r * 4);
        let (a, l, corner) = subblock_neighbors(recon, sx, sy, c, above_right);
        let pred = prediction::subblock_predict(sub_modes[i], &a, &l, corner);
        let pred_i16: [i16; 16] = core::array::from_fn(|k| i16::from(pred[k]));
        write_block(&mut recon.y, rstride, sx, sy, &pred_i16, &[0i16; 16]);
    }
}

/// Decodes and reconstructs the luma plane of a `B_PRED` macroblock from its submodes and the token
/// partition, interleaving token decode and reconstruction (each subblock predicts from the one
/// before it) and threading the plane-3 non-zero context. Leaves the Y2 context untouched.
#[allow(clippy::too_many_arguments)] // the reconstruction loop genuinely needs all of this state
fn decode_bpred_luma(
    recon: &mut FrameBuffers,
    dec: &mut BoolDecoder,
    above: &mut EntropyCtx,
    left: &mut EntropyCtx,
    probs: &CoeffProbs,
    mb_x: usize,
    mb_y: usize,
    qf: &QuantFactors,
    sub_modes: &[usize; 16],
    above_right: &[u8; 4],
) {
    let (px, py, rstride) = (mb_x * 16, mb_y * 16, recon.y_stride());
    for i in 0..16 {
        let (r, c) = (i / 4, i % 4);
        let (sx, sy) = (px + c * 4, py + r * 4);
        let ctx = usize::from(above.y[c]) + usize::from(left.y[r]);
        let mut lev = [0i16; 16];
        let has = tokens::decode_block(dec, &mut lev, 3, ctx, probs);
        above.y[c] = has;
        left.y[r] = has;

        let (a, l, corner) = subblock_neighbors(recon, sx, sy, c, above_right);
        let pred = prediction::subblock_predict(sub_modes[i], &a, &l, corner);
        let mut dq = [0i16; 16];
        dq[0] = quant::dequantize(lev[0], qf.y1_dc);
        for k in 1..16 {
            dq[k] = quant::dequantize(lev[k], qf.y1_ac);
        }
        let residue = idct4x4(&dq);
        let pred_i16: [i16; 16] = core::array::from_fn(|k| i16::from(pred[k]));
        write_block(&mut recon.y, rstride, sx, sy, &pred_i16, &residue);
    }
}

/// Reads the 16 `B_PRED` submodes, mirroring the encoder's `write_bmodes`.
pub(super) fn read_bmodes(
    modes: &mut BoolDecoder,
    above_col: &[usize; 4],
    left_col: &[usize; 4],
) -> [usize; 16] {
    let mut sub_modes = [B_DC_PRED; 16];
    for i in 0..16 {
        let (a, l) = bmode_context(&sub_modes, above_col, left_col, i);
        sub_modes[i] = modes.get_tree(prediction::BMODE_TREE, &prediction::KF_BMODE_PROB[a][l]);
    }
    sub_modes
}

/// Decodes a macroblock's U then V chroma blocks into `levels`, mirroring the encoder's `encode_chroma_tokens`.
fn decode_chroma_tokens(
    dec: &mut BoolDecoder,
    above: &mut EntropyCtx,
    left: &mut EntropyCtx,
    probs: &CoeffProbs,
    levels: &mut MbLevels,
) {
    for (plane_levels, above_ctx, left_ctx) in [
        (&mut levels.u, &mut above.u, &mut left.u),
        (&mut levels.v, &mut above.v, &mut left.v),
    ] {
        for i in 0..4 {
            let (r, c) = (i / 2, i % 2);
            let ctx = usize::from(above_ctx[c]) + usize::from(left_ctx[r]);
            let has = tokens::decode_block(dec, &mut plane_levels[i], 2, ctx, probs);
            above_ctx[c] = has;
            left_ctx[r] = has;
        }
    }
}

/// Decodes a whole-block (non-`B_PRED`) macroblock's coefficient blocks: Y2, 16 luma (plane 0), then
/// chroma.
fn decode_mb_tokens(
    dec: &mut BoolDecoder,
    above: &mut EntropyCtx,
    left: &mut EntropyCtx,
    probs: &CoeffProbs,
) -> MbLevels {
    let mut levels = MbLevels::default();
    let ctx = usize::from(above.y2) + usize::from(left.y2);
    let has = tokens::decode_block(dec, &mut levels.y2, 1, ctx, probs);
    above.y2 = has;
    left.y2 = has;
    for i in 0..16 {
        let (r, c) = (i / 4, i % 4);
        let ctx = usize::from(above.y[c]) + usize::from(left.y[r]);
        let has = tokens::decode_block(dec, &mut levels.y[i], 0, ctx, probs);
        above.y[c] = has;
        left.y[r] = has;
    }
    decode_chroma_tokens(dec, above, left, probs, &mut levels);
    levels
}

/// Splits the token-partition section (everything after the control partition) into `n` boolean
/// decoders (RFC 6386 §9.5): the first `n-1` partition sizes are 3-byte little-endian prefixes, the
/// last partition's size is the remainder.
fn split_token_partitions(data: &[u8], n: usize) -> Result<Vec<BoolDecoder<'_>>> {
    let sizes_len = (n - 1) * 3;
    if data.len() < sizes_len {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "VP8: token-partition sizes truncated",
        ));
    }
    let mut decoders = Vec::with_capacity(n);
    let mut offset = sizes_len;
    for i in 0..n {
        let size = if i < n - 1 {
            // A 24-bit little-endian size, read as one. Spelling it
            // `s[0] | s[1] << 8 | s[2] << 16` puts each byte in its own lane, and disjoint lanes
            // are where `|` and `^` agree -- unkillable mutants for no gain (#110).
            let s = &data[i * 3..i * 3 + 3];
            u32::from_le_bytes([s[0], s[1], s[2], 0]) as usize
        } else {
            data.len() - offset
        };
        let end = offset
            .checked_add(size)
            .filter(|&e| e <= data.len())
            .ok_or_else(|| {
                Error::invalid_input(env!("CARGO_PKG_NAME"), "VP8: token partition exceeds frame")
            })?;
        decoders.push(BoolDecoder::new(&data[offset..end]));
        offset = end;
    }
    Ok(decoders)
}

/// Decodes a VP8 key-frame bitstream (the `VP8 ` chunk payload) into reconstructed planes.
///
/// # Errors
///
/// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error for a
/// malformed stream, or an [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) error for
/// an inter frame or an undefined bitstream version (> 3).
pub fn decode_frame(data: &[u8]) -> Result<FrameBuffers> {
    let chunk = header::read_uncompressed_chunk(data)?;
    if chunk.width == 0 || chunk.height == 0 {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "VP8: zero frame dimension",
        ));
    }
    // RFC 6386 §9.1: the 3-bit version selects decoding profiles 0–3; 4–7 are undefined, and libwebp
    // rejects them, so we do too. Profiles 1–3 differ from 0 only in the inter-frame reconstruction
    // filter and a loop-filter hint; for intra key frames the explicit filter-type bit governs, so
    // 0–3 reconstruct identically here (pinned against libwebp by
    // `frame::oracle_tests::gamut_decodes_patched_vp8_profiles_like_libwebp`).
    if chunk.version > 3 {
        return Err(Error::unsupported(
            env!("CARGO_PKG_NAME"),
            "VP8: unsupported bitstream version",
        ));
    }
    let part0_end = UNCOMPRESSED_CHUNK_LEN + chunk.first_partition_size as usize;
    if part0_end > data.len() {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "VP8: first partition exceeds frame",
        ));
    }
    let mut modes = BoolDecoder::new(&data[UNCOMPRESSED_CHUNK_LEN..part0_end]);
    let (head, coeff_probs) = header::read_frame_header(&chunk, &mut modes);
    let seg_qf = segment_quant_factors(&head);
    let n = head.token_partitions as usize;
    let mut residuals = split_token_partitions(&data[part0_end..], n)?;
    let mut recon = FrameBuffers::new(u32::from(chunk.width), u32::from(chunk.height));

    let mut above = vec![EntropyCtx::default(); recon.mb_cols];
    let mut above_bmodes = vec![[B_DC_PRED; 4]; recon.mb_cols];
    let mut filter_interior = vec![false; recon.mb_cols * recon.mb_rows];
    let mut is_bpred_map = vec![false; recon.mb_cols * recon.mb_rows];
    let mut segment_map = vec![0usize; recon.mb_cols * recon.mb_rows];
    for mb_y in 0..recon.mb_rows {
        let mut left = EntropyCtx::default();
        let mut left_bmodes = [B_DC_PRED; 4];
        for mb_x in 0..recon.mb_cols {
            let segment = if head.segmentation.update_map {
                modes.get_tree(MB_SEGMENT_TREE, &head.segmentation.tree_probs)
            } else {
                0
            };
            segment_map[mb_y * recon.mb_cols + mb_x] = segment;
            let qf = seg_qf[segment];
            let skip = head.mb_no_skip_coeff && modes.get_bool(head.prob_skip_false);
            let y_mode = modes.get_tree(prediction::KF_YMODE_TREE, &prediction::KF_YMODE_PROB);
            let is_bpred = y_mode == B_PRED;
            let sub_modes = if is_bpred {
                read_bmodes(&mut modes, &above_bmodes[mb_x], &left_bmodes)
            } else {
                [B_DC_PRED; 16]
            };
            let uv_mode = modes.get_tree(prediction::KF_UV_MODE_TREE, &prediction::KF_UV_MODE_PROB);
            let u_pred = predict_chroma(&recon.u, recon.c_stride(), mb_x, mb_y, uv_mode);
            let v_pred = predict_chroma(&recon.v, recon.c_stride(), mb_x, mb_y, uv_mode);
            let cstride = recon.c_stride();

            // A skipped macroblock has no coefficients: its residual is zero (the reconstruction is the
            // prediction) and no tokens are read.
            let mut levels = MbLevels::default();
            if is_bpred {
                let above_right = above_right_source(&recon, mb_x, mb_y);
                if skip {
                    reconstruct_bpred_zero(&mut recon, mb_x, mb_y, &sub_modes, &above_right);
                } else {
                    decode_bpred_luma(
                        &mut recon,
                        &mut residuals[mb_y % n],
                        &mut above[mb_x],
                        &mut left,
                        &coeff_probs,
                        mb_x,
                        mb_y,
                        &qf,
                        &sub_modes,
                        &above_right,
                    );
                    decode_chroma_tokens(
                        &mut residuals[mb_y % n],
                        &mut above[mb_x],
                        &mut left,
                        &coeff_probs,
                        &mut levels,
                    );
                }
                reconstruct_chroma(&mut recon.u, cstride, mb_x, mb_y, &u_pred, &levels.u, &qf);
                reconstruct_chroma(&mut recon.v, cstride, mb_x, mb_y, &v_pred, &levels.v, &qf);
                filter_interior[mb_y * recon.mb_cols + mb_x] = true; // B_PRED always filters interiors
                is_bpred_map[mb_y * recon.mb_cols + mb_x] = true;
            } else {
                let y_pred = predict_luma(&recon, mb_x, mb_y, y_mode);
                if !skip {
                    levels = decode_mb_tokens(
                        &mut residuals[mb_y % n],
                        &mut above[mb_x],
                        &mut left,
                        &coeff_probs,
                    );
                }
                reconstruct_luma(&mut recon, mb_x, mb_y, &y_pred, &levels, &qf);
                reconstruct_chroma(&mut recon.u, cstride, mb_x, mb_y, &u_pred, &levels.u, &qf);
                reconstruct_chroma(&mut recon.v, cstride, mb_x, mb_y, &v_pred, &levels.v, &qf);
                filter_interior[mb_y * recon.mb_cols + mb_x] = mb_has_coeffs(&levels);
            }
            if skip {
                clear_mb_context(&mut above[mb_x], &mut left, is_bpred);
            }

            (above_bmodes[mb_x], left_bmodes) = bmode_propagation(is_bpred, y_mode, &sub_modes);
        }
    }

    apply_loop_filter(
        &mut recon,
        &head.loop_filter,
        &head.segmentation,
        &segment_map,
        &filter_interior,
        &is_bpred_map,
    );
    Ok(recon)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vp8::encoder::encode_frame;
    use crate::vp8::frame::fixtures::{chroma_dims, pattern, yuv420};

    #[test]
    fn decode_accepts_profiles_0_to_3_and_rejects_higher() {
        // RFC 6386 §9.1: the frame-tag version selects profiles 0–3 (4–7 undefined). The version sits
        // in bits 1–3 of byte 0; patching it must not change which (valid) frame decodes.
        let yuv = pattern(32, 32);
        let (bits, _) = encode_frame(&yuv, 40).expect("fixture fits the partition-size fields");
        let patch_version = |v: u8| {
            let mut p = bits.clone();
            p[0] = (p[0] & !0b1110) | (v << 1);
            p
        };
        for v in 0u8..=3 {
            assert!(
                decode_frame(&patch_version(v)).is_ok(),
                "profile {v} must decode"
            );
        }
        for v in 4u8..=7 {
            assert!(
                matches!(
                    decode_frame(&patch_version(v)),
                    Err(error) if error.kind() == gamut_core::ErrorKind::Unsupported
                ),
                "profile {v} must be rejected"
            );
        }
    }

    #[test]
    fn decode_rejects_truncated_first_partition() {
        let yuv = pattern(16, 16);
        let (mut bitstream, _) =
            encode_frame(&yuv, 40).expect("fixture fits the partition-size fields");
        bitstream.truncate(UNCOMPRESSED_CHUNK_LEN + 1);
        let _ = decode_frame(&bitstream);
    }

    #[test]
    fn decode_rejects_zero_dimension() {
        let yuv = pattern(16, 16);
        let (mut bits, _) = encode_frame(&yuv, 40).expect("fixture fits the partition-size fields");
        // Clear the 14-bit width field (low 6 bits live in byte 7) while leaving height non-zero, so
        // only the `width == 0` half of the guard fires: `||` flipped to `&&` would let it through.
        bits[6] = 0;
        bits[7] &= 0xC0;
        assert!(matches!(
            decode_frame(&bits),
            Err(error) if error.kind() == gamut_core::ErrorKind::InvalidInput
        ));
    }

    /// A partition size that needs its third byte.
    ///
    /// The size is 24-bit little-endian, and every fixture in the suite has partitions under
    /// 64 KiB -- so the high byte was always zero, and dropping it entirely changed nothing
    /// (#110). Asserted at the boundary from both sides: a buffer exactly long enough for a
    /// 65536-byte partition is accepted, and one byte shorter is refused. A reader that ignores
    /// the high byte computes a size of 0 and accepts both.
    #[test]
    fn a_partition_size_uses_its_high_byte() {
        const SIZE: usize = 1 << 16;
        // n = 2: three bytes of size table, then the first partition, then the second.
        let mut data = vec![0x00u8, 0x00, 0x01]; // little-endian 0x010000 = 65536
        data.resize(3 + SIZE, 0);

        let parts = split_token_partitions(&data, 2).expect("exactly long enough");
        assert_eq!(parts.len(), 2);

        data.pop();
        let err =
            split_token_partitions(&data, 2).expect_err("one byte short of the declared size");
        assert!(err.to_string().contains("exceeds frame"), "{err}");
    }

    #[test]
    fn split_token_partitions_size_table_boundary() {
        // Exactly the size-table length (3 bytes for n = 2) with a zero first size is valid: two
        // empty partitions. `<` widened to `<=` would wrongly reject it as truncated.
        assert!(split_token_partitions(&[0, 0, 0], 2).is_ok());
        // One byte short of the size table is truncated and must be rejected; `<` narrowed to `==`
        // would skip the bounds check.
        assert!(split_token_partitions(&[0, 0], 2).is_err());
    }

    #[test]
    fn decode_accepts_exact_fit_with_empty_token_partition() {
        // A flat image quantizes to all-skip macroblocks, so the single token partition carries no
        // tokens. Truncating the stream to exactly the end of the first partition leaves zero
        // token-partition bytes — part0_end == data.len(). The `part0_end > data.len()` guard must
        // accept this exact fit; `>=` would reject the valid (if minimal) stream.
        let (w, h) = (16u32, 16u32);
        let (cw, ch) = chroma_dims(w, h);
        let yuv = yuv420(
            w,
            h,
            vec![128; (w * h) as usize],
            vec![128; cw * ch],
            vec![128; cw * ch],
        );
        let (bits, _) = encode_frame(&yuv, 127).expect("fixture fits the partition-size fields"); // coarsest quantizer → every macroblock skips
        let chunk = header::read_uncompressed_chunk(&bits).expect("chunk");
        let end = UNCOMPRESSED_CHUNK_LEN + chunk.first_partition_size as usize;
        assert!(
            decode_frame(&bits[..end]).is_ok(),
            "a stream ending exactly at the first partition (no token bytes) must decode"
        );
    }
}
