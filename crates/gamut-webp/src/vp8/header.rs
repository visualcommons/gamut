//! VP8 key-frame frame header (RFC 6386 §9, §19.1–§19.2): the uncompressed 10-byte chunk (frame tag,
//! start code, dimensions) plus the boolean-coded header fields (color space, loop filter, partition
//! count, quantizer indices, coefficient-probability updates).
//!
//! gamut codes key frames only (`key_frame` bit = 0). The header carries the `update_segmentation()`
//! record (per-segment quantizer/filter adjustments + the segment-id tree probs), the loop-filter
//! parameters, the partition count, the quantizer indices, and the coefficient-probability-update
//! record — all parsed by the decoder so it tracks the working [`CoeffProbs`]. Per-macroblock
//! loop-filter adjustments (`mb_lf_adjustments`) are parsed into [`LoopFilterDeltas`] and applied to
//! each macroblock's filter level during reconstruction. Tracked in `../STATUS.md` section H.

use gamut_core::{Error, Result};

use super::bool_coder::{BoolDecoder, BoolEncoder};
use super::tokens::{self, CoeffProbs, DEFAULT_COEFF_PROBS};

/// The 3-byte start code that follows the frame tag in a VP8 key frame (RFC 6386 §9.1).
pub const VP8_KEYFRAME_START_CODE: [u8; 3] = [0x9d, 0x01, 0x2a];

/// Length in bytes of a key-frame's uncompressed data chunk (RFC 6386 §9.1).
pub const UNCOMPRESSED_CHUNK_LEN: usize = 10;

/// Per-segment adjustment state (RFC 6386 §9.3, §10). Still images usually leave this disabled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Segmentation {
    /// Whether segmentation is enabled for the frame.
    pub enabled: bool,
    /// Whether the per-macroblock segment map is (re)transmitted this frame.
    pub update_map: bool,
    /// Feature-data mode: `true` = absolute values, `false` = deltas from the frame base
    /// (`segment_feature_mode`).
    pub abs_delta: bool,
    /// Per-segment quantizer adjustment (absolute or delta, per [`abs_delta`](Self::abs_delta)).
    pub quantizer: [i8; 4],
    /// Per-segment loop-filter-level adjustment.
    pub filter_strength: [i8; 4],
    /// Branch probabilities for the segment-id tree (default 255 each).
    pub tree_probs: [u8; 3],
}

/// Per-macroblock loop-filter deltas (RFC 6386 §9.4 `mb_lf_adjustments`). For key frames every
/// macroblock is intra, so only the intra reference-frame class (`ref_frame[0]`) applies to all
/// macroblocks and the `B_PRED` mode class (`mode[0]`) applies additionally to 4×4-predicted ones;
/// the other entries are inter-frame classes gamut never codes but round-trips for completeness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LoopFilterDeltas {
    /// Reference-frame deltas, indexed intra / last / golden / altref. Only index 0 (intra) applies
    /// to key frames.
    pub ref_frame: [i8; 4],
    /// Prediction-mode-class deltas. Only index 0 (the `B_PRED` class) applies to key frames.
    pub mode: [i8; 4],
}

/// Loop-filter header parameters (RFC 6386 §9.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LoopFilterParams {
    /// `true` selects the simple filter; `false` selects the normal filter.
    pub simple: bool,
    /// Base filter level (`0..=63`); 0 disables the loop filter.
    pub level: u8,
    /// Sharpness level (`0..=7`).
    pub sharpness: u8,
    /// Per-macroblock reference/mode loop-filter deltas (`mb_lf_adjustments`); all-zero when the
    /// feature is disabled.
    pub deltas: LoopFilterDeltas,
}

/// Dequantization indices (RFC 6386 §9.6): a base AC index plus a signed delta per plane/coefficient.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct QuantIndices {
    /// Base quantizer index (the Y1 AC index, `0..=127`).
    pub y_ac: u8,
    /// Y1 DC index delta.
    pub y_dc_delta: i8,
    /// Y2 (WHT) DC index delta.
    pub y2_dc_delta: i8,
    /// Y2 (WHT) AC index delta.
    pub y2_ac_delta: i8,
    /// Chroma DC index delta.
    pub uv_dc_delta: i8,
    /// Chroma AC index delta.
    pub uv_ac_delta: i8,
}

/// A VP8 key-frame header (RFC 6386 §9). Intra/key-frame fields only — gamut codes no inter-frame
/// state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Vp8FrameHeader {
    /// Frame width in pixels (the 14-bit field of the uncompressed chunk).
    pub width: u16,
    /// Frame height in pixels (the 14-bit field of the uncompressed chunk).
    pub height: u16,
    /// Horizontal upscaling hint (2 bits; 0 = none).
    pub horizontal_scale: u8,
    /// Vertical upscaling hint (2 bits; 0 = none).
    pub vertical_scale: u8,
    /// Bitstream version (3 bits; selects loop-filter / reconstruction variants).
    pub version: u8,
    /// Color space (0 = YUV per BT.601; 1 is reserved).
    pub color_space: u8,
    /// Whether pixel clamping is required (the `clamping_type` flag).
    pub clamp_required: bool,
    /// Segmentation state (§9.3).
    pub segmentation: Segmentation,
    /// Loop-filter header (§9.4).
    pub loop_filter: LoopFilterParams,
    /// Number of DCT-coefficient token partitions (1, 2, 4, or 8) (§9.5).
    pub token_partitions: u8,
    /// Dequantization indices (§9.6).
    pub quant: QuantIndices,
    /// Whether token-probability updates persist past this frame (§9.11).
    pub refresh_entropy_probs: bool,
    /// Whether macroblocks may signal that they carry no non-zero coefficients (§9.10).
    pub mb_no_skip_coeff: bool,
    /// Probability that a macroblock is *not* skipped (only meaningful if `mb_no_skip_coeff`) (§9.10).
    pub prob_skip_false: u8,
}

/// The parsed uncompressed data chunk (RFC 6386 §9.1, §19.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UncompressedChunk {
    /// Whether this is a key frame (the frame-tag bit is `0` for key frames).
    pub is_key_frame: bool,
    /// Bitstream version (3 bits).
    pub version: u8,
    /// Whether the frame is meant to be displayed.
    pub show_frame: bool,
    /// Size in bytes of the first (control) partition, excluding this chunk.
    pub first_partition_size: u32,
    /// Frame width in pixels (14 bits).
    pub width: u16,
    /// Frame height in pixels (14 bits).
    pub height: u16,
    /// Horizontal upscaling hint (2 bits).
    pub horizontal_scale: u8,
    /// Vertical upscaling hint (2 bits).
    pub vertical_scale: u8,
}

/// `log2` of a token-partition count `{1, 2, 4, 8}`.
fn log2_partitions(count: u8) -> u32 {
    debug_assert!(
        matches!(count, 1 | 2 | 4 | 8),
        "token partition count must be 1, 2, 4, or 8"
    );
    u32::from(count).trailing_zeros()
}

/// The largest control-partition size the frame tag can describe: the field is 19 bits wide
/// (RFC 6386 §9.1, bits 5-23 of the 3-byte tag), so 512 KiB - 1 is a hard format ceiling, not an
/// implementation limit. libwebp reports the same condition as `VP8_ENC_ERROR_PARTITION0_OVERFLOW`.
pub const MAX_FIRST_PARTITION_SIZE: u32 = (1 << 19) - 1;

/// Writes the 10-byte uncompressed data chunk for a key frame (RFC 6386 §19.1) to `out`:
/// the frame tag (with `first_partition_size` and `show_frame = 1`), the start code, and the
/// little-endian width/height + scale codes.
///
/// # Errors
///
/// Returns [`Error::InvalidInput`] if `first_partition_size` exceeds
/// [`MAX_FIRST_PARTITION_SIZE`]. The size shares a 3-byte tag with the version and show-frame
/// bits, so an oversized value would silently lose its high bits and describe a partition
/// boundary that is not there — a stream no decoder can read. Reporting it is the only honest
/// option: the ceiling is the format's.
pub fn write_uncompressed_chunk(
    header: &Vp8FrameHeader,
    first_partition_size: u32,
    out: &mut Vec<u8>,
) -> Result<()> {
    if first_partition_size > MAX_FIRST_PARTITION_SIZE {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "VP8: control partition exceeds the frame tag's 19-bit size field",
        ));
    }
    // key_frame bit (bit 0) = 0; version in bits 1-3; show_frame = 1 in bit 4; size in bits 5-23.
    let tag = (u32::from(header.version) << 1) | (1 << 4) | (first_partition_size << 5);
    out.push((tag & 0xff) as u8);
    out.push(((tag >> 8) & 0xff) as u8);
    out.push(((tag >> 16) & 0xff) as u8);
    out.extend_from_slice(&VP8_KEYFRAME_START_CODE);
    let h = u32::from(header.width) | (u32::from(header.horizontal_scale) << 14);
    out.push((h & 0xff) as u8);
    out.push(((h >> 8) & 0xff) as u8);
    let v = u32::from(header.height) | (u32::from(header.vertical_scale) << 14);
    out.push((v & 0xff) as u8);
    out.push(((v >> 8) & 0xff) as u8);
    Ok(())
}

/// Parses the uncompressed data chunk (RFC 6386 §19.1).
///
/// # Errors
///
/// Returns [`Error::InvalidInput`] if the data is too short or the key-frame start code is wrong, or
/// [`Error::Unsupported`] for an inter frame (gamut codes key frames only).
pub fn read_uncompressed_chunk(data: &[u8]) -> Result<UncompressedChunk> {
    if data.len() < 3 {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "VP8: truncated frame tag",
        ));
    }
    let tag = u32::from(data[0]) | (u32::from(data[1]) << 8) | (u32::from(data[2]) << 16);
    let is_key_frame = (tag & 1) == 0;
    if !is_key_frame {
        return Err(Error::unsupported(
            env!("CARGO_PKG_NAME"),
            "VP8: only intra key frames are supported",
        ));
    }
    if data.len() < UNCOMPRESSED_CHUNK_LEN {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "VP8: truncated key-frame header",
        ));
    }
    if data[3..6] != VP8_KEYFRAME_START_CODE {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "VP8: bad key-frame start code",
        ));
    }
    let hsc = u32::from(data[6]) | (u32::from(data[7]) << 8);
    let vsc = u32::from(data[8]) | (u32::from(data[9]) << 8);
    Ok(UncompressedChunk {
        is_key_frame,
        version: ((tag >> 1) & 0x7) as u8,
        show_frame: (tag >> 4) & 1 != 0,
        first_partition_size: (tag >> 5) & 0x7_FFFF,
        width: (hsc & 0x3FFF) as u16,
        horizontal_scale: (hsc >> 14) as u8,
        height: (vsc & 0x3FFF) as u16,
        vertical_scale: (vsc >> 14) as u8,
    })
}

/// Writes a signed quantizer-index delta as `present` flag + magnitude `L(4)` + sign (RFC 6386 §19.2).
fn write_delta(enc: &mut BoolEncoder, delta: i8) {
    if delta == 0 {
        enc.put_flag(false);
    } else {
        enc.put_flag(true);
        enc.put_literal(u32::from(delta.unsigned_abs()), 4);
        enc.put_flag(delta < 0);
    }
}

/// Reads a signed quantizer-index delta (RFC 6386 §19.2).
fn read_delta(dec: &mut BoolDecoder) -> i8 {
    if dec.get_flag() {
        let magnitude = dec.get_literal(4) as i8;
        if dec.get_flag() {
            -magnitude
        } else {
            magnitude
        }
    } else {
        0
    }
}

/// Writes `quant_indices()` (RFC 6386 §19.2): the base AC index then the five per-plane deltas.
fn write_quant_indices(enc: &mut BoolEncoder, quant: &QuantIndices) {
    enc.put_literal(u32::from(quant.y_ac), 7);
    write_delta(enc, quant.y_dc_delta);
    write_delta(enc, quant.y2_dc_delta);
    write_delta(enc, quant.y2_ac_delta);
    write_delta(enc, quant.uv_dc_delta);
    write_delta(enc, quant.uv_ac_delta);
}

/// Reads `quant_indices()` (RFC 6386 §19.2).
fn read_quant_indices(dec: &mut BoolDecoder) -> QuantIndices {
    QuantIndices {
        y_ac: dec.get_literal(7) as u8,
        y_dc_delta: read_delta(dec),
        y2_dc_delta: read_delta(dec),
        y2_ac_delta: read_delta(dec),
        uv_dc_delta: read_delta(dec),
        uv_ac_delta: read_delta(dec),
    }
}

/// Writes the `update_segmentation()` record (RFC 6386 §19.2): the map-update flag, optional feature
/// data (per-segment quantizer and loop-filter adjustments), and optional segment-id tree probs.
fn write_update_segmentation(enc: &mut BoolEncoder, seg: &Segmentation) {
    enc.put_flag(seg.update_map);
    let update_data = seg.abs_delta || seg.quantizer != [0; 4] || seg.filter_strength != [0; 4];
    enc.put_flag(update_data);
    if update_data {
        enc.put_flag(seg.abs_delta); // segment_feature_mode
        for q in seg.quantizer {
            write_segment_feature(enc, q, 7);
        }
        for f in seg.filter_strength {
            write_segment_feature(enc, f, 6);
        }
    }
    if seg.update_map {
        for p in seg.tree_probs {
            if p == 255 {
                enc.put_flag(false); // segment_prob_update: keep the default 255
            } else {
                enc.put_flag(true);
                enc.put_literal(u32::from(p), 8);
            }
        }
    }
}

/// Writes one signed segment-feature value as `present` flag + magnitude + sign (RFC 6386 §19.2).
fn write_segment_feature(enc: &mut BoolEncoder, value: i8, bits: u32) {
    if value == 0 {
        enc.put_flag(false);
    } else {
        enc.put_flag(true);
        enc.put_literal(u32::from(value.unsigned_abs()), bits);
        enc.put_flag(value < 0);
    }
}

/// Reads the `update_segmentation()` record, mirroring [`write_update_segmentation`].
fn read_update_segmentation(dec: &mut BoolDecoder) -> Segmentation {
    let update_map = dec.get_flag();
    let mut seg = Segmentation {
        enabled: true,
        update_map,
        tree_probs: [255; 3],
        ..Segmentation::default()
    };
    if dec.get_flag() {
        seg.abs_delta = dec.get_flag();
        for q in &mut seg.quantizer {
            *q = read_segment_feature(dec, 7);
        }
        for f in &mut seg.filter_strength {
            *f = read_segment_feature(dec, 6);
        }
    }
    if update_map {
        for p in &mut seg.tree_probs {
            if dec.get_flag() {
                *p = dec.get_literal(8) as u8;
            }
        }
    }
    seg
}

/// Reads one signed segment-feature value, mirroring [`write_segment_feature`].
fn read_segment_feature(dec: &mut BoolDecoder, bits: u32) -> i8 {
    if dec.get_flag() {
        let mag = dec.get_literal(bits) as i8;
        if dec.get_flag() { -mag } else { mag }
    } else {
        0
    }
}

/// Writes `mb_lf_adjustments()` (RFC 6386 §9.4, §19.2): the enable flag, then — when any delta is
/// non-zero — the update flag and the four reference-frame and four mode deltas, each coded as an
/// update flag + `L(6)` magnitude + sign (the same shape as a segment feature).
fn write_mb_lf_adjustments(enc: &mut BoolEncoder, deltas: &LoopFilterDeltas) {
    let enabled = deltas.ref_frame != [0; 4] || deltas.mode != [0; 4];
    enc.put_flag(enabled); // loop_filter_adj_enable
    if enabled {
        enc.put_flag(true); // mode_ref_lf_delta_update: transmit the values this frame
        for delta in deltas.ref_frame {
            write_segment_feature(enc, delta, 6);
        }
        for delta in deltas.mode {
            write_segment_feature(enc, delta, 6);
        }
    }
}

/// Reads `mb_lf_adjustments()` (RFC 6386 §9.4, §19.2), mirroring [`write_mb_lf_adjustments`]. A delta
/// not transmitted this frame keeps its default of zero — gamut codes key frames only, so there is no
/// prior frame whose deltas would persist.
fn read_mb_lf_adjustments(dec: &mut BoolDecoder) -> LoopFilterDeltas {
    let mut deltas = LoopFilterDeltas::default();
    if dec.get_flag() {
        // loop_filter_adj_enable
        if dec.get_flag() {
            // mode_ref_lf_delta_update
            for delta in &mut deltas.ref_frame {
                *delta = read_segment_feature(dec, 6);
            }
            for delta in &mut deltas.mode {
                *delta = read_segment_feature(dec, 6);
            }
        }
    }
    deltas
}

/// Writes the boolean-coded key-frame header (RFC 6386 §19.2) into the first (control) partition's
/// encoder `enc`, leaving it open for the per-macroblock records that follow. Segmentation,
/// per-macroblock loop-filter adjustments, and coefficient-probability updates are emitted as
/// configured.
pub fn write_frame_header(
    enc: &mut BoolEncoder,
    header: &Vp8FrameHeader,
    coeff_probs: &tokens::CoeffProbs,
) {
    enc.put_literal(u32::from(header.color_space), 1);
    enc.put_flag(!header.clamp_required); // clamping_type: 1 = no clamp needed
    enc.put_flag(header.segmentation.enabled);
    if header.segmentation.enabled {
        write_update_segmentation(enc, &header.segmentation);
    }
    enc.put_flag(header.loop_filter.simple); // filter_type
    enc.put_literal(u32::from(header.loop_filter.level), 6);
    enc.put_literal(u32::from(header.loop_filter.sharpness), 3);
    write_mb_lf_adjustments(enc, &header.loop_filter.deltas);
    enc.put_literal(log2_partitions(header.token_partitions), 2);
    write_quant_indices(enc, &header.quant);
    enc.put_flag(header.refresh_entropy_probs);
    // The record is a delta against the key-frame defaults, so passing the defaults themselves
    // emits the all-"no update" record a single-pass encoder needs.
    tokens::write_coeff_prob_updates(enc, coeff_probs, &DEFAULT_COEFF_PROBS);
    enc.put_flag(header.mb_no_skip_coeff);
    if header.mb_no_skip_coeff {
        enc.put_literal(u32::from(header.prob_skip_false), 8);
    }
}

/// Reads the boolean-coded key-frame header (RFC 6386 §19.2) from the control-partition decoder `dec`,
/// returning the header and the working coefficient-probability table after any updates. Reading
/// cannot fail: the boolean coder yields zero past the end of input rather than erroring, so every
/// field has a defined value even on a truncated control partition.
pub fn read_frame_header(
    chunk: &UncompressedChunk,
    dec: &mut BoolDecoder,
) -> (Vp8FrameHeader, CoeffProbs) {
    let color_space = dec.get_literal(1) as u8;
    let clamp_required = !dec.get_flag();
    let segmentation = if dec.get_flag() {
        read_update_segmentation(dec)
    } else {
        Segmentation::default()
    };
    let simple = dec.get_flag();
    let level = dec.get_literal(6) as u8;
    let sharpness = dec.get_literal(3) as u8;
    let loop_filter = LoopFilterParams {
        simple,
        level,
        sharpness,
        deltas: read_mb_lf_adjustments(dec),
    };
    let token_partitions = 1u8 << dec.get_literal(2);
    let quant = read_quant_indices(dec);
    let refresh_entropy_probs = dec.get_flag();
    let mut coeff_probs = DEFAULT_COEFF_PROBS;
    tokens::read_coeff_prob_updates(dec, &mut coeff_probs);
    let mb_no_skip_coeff = dec.get_flag();
    let prob_skip_false = if mb_no_skip_coeff {
        dec.get_literal(8) as u8
    } else {
        0
    };
    let header = Vp8FrameHeader {
        width: chunk.width,
        height: chunk.height,
        horizontal_scale: chunk.horizontal_scale,
        vertical_scale: chunk.vertical_scale,
        version: chunk.version,
        color_space,
        clamp_required,
        segmentation,
        loop_filter,
        token_partitions,
        quant,
        refresh_entropy_probs,
        mb_no_skip_coeff,
        prob_skip_false,
    };
    (header, coeff_probs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_header() -> Vp8FrameHeader {
        Vp8FrameHeader {
            width: 176,
            height: 144,
            horizontal_scale: 0,
            vertical_scale: 0,
            version: 0,
            color_space: 0,
            clamp_required: true,
            segmentation: Segmentation::default(),
            loop_filter: LoopFilterParams {
                simple: false,
                level: 0,
                sharpness: 0,
                ..Default::default()
            },
            token_partitions: 1,
            quant: QuantIndices::default(),
            refresh_entropy_probs: true,
            mb_no_skip_coeff: false,
            prob_skip_false: 0,
        }
    }

    /// Encodes a header to a complete (header-only) VP8 bitstream and decodes it back.
    fn roundtrip(header: &Vp8FrameHeader) {
        let mut enc = BoolEncoder::new();
        write_frame_header(&mut enc, header, &DEFAULT_COEFF_PROBS);
        let part0 = enc.finish();
        let mut stream = Vec::new();
        write_uncompressed_chunk(header, part0.len() as u32, &mut stream)
            .expect("a header-only partition is far inside the 19-bit size field");
        stream.extend_from_slice(&part0);

        let chunk = read_uncompressed_chunk(&stream).expect("chunk");
        assert!(chunk.is_key_frame);
        assert_eq!((chunk.width, chunk.height), (header.width, header.height));
        assert_eq!(chunk.first_partition_size as usize, part0.len());

        let end = UNCOMPRESSED_CHUNK_LEN + chunk.first_partition_size as usize;
        let mut dec = BoolDecoder::new(&stream[UNCOMPRESSED_CHUNK_LEN..end]);
        let (decoded, probs) = read_frame_header(&chunk, &mut dec);
        assert_eq!(&decoded, header);
        assert_eq!(
            probs, DEFAULT_COEFF_PROBS,
            "minimal header carries no prob updates"
        );
    }

    /// The frame tag's size field is 19 bits, and it shares its three bytes with the version and
    /// show-frame bits: a control partition of 512 KiB or more used to lose its high bit there and
    /// describe a partition boundary that is not in the stream, which every decoder rejects — a
    /// silently unreadable file from a successful encode. A frame can genuinely reach this: the
    /// per-macroblock mode records of a `B_PRED`-heavy 16-megapixel frame fill partition 0 well
    /// past the ceiling. The boundary is pinned from both sides, and the rejection is asserted on
    /// its message so removing the guard cannot pass by tripping some later check.
    #[test]
    fn oversized_control_partition_is_reported_not_truncated() {
        let header = sample_header();
        // Exactly at the ceiling: describable, and it round-trips through the reader unchanged.
        let mut stream = Vec::new();
        write_uncompressed_chunk(&header, MAX_FIRST_PARTITION_SIZE, &mut stream)
            .expect("the ceiling itself is encodable");
        let chunk = read_uncompressed_chunk(&stream).expect("chunk");
        assert_eq!(chunk.first_partition_size, MAX_FIRST_PARTITION_SIZE);
        // One byte over: rejected, and nothing is appended to the caller's buffer.
        let mut stream = Vec::new();
        let err = write_uncompressed_chunk(&header, MAX_FIRST_PARTITION_SIZE + 1, &mut stream)
            .expect_err("one byte past the field must not be encodable");
        assert!(
            err.to_string().contains("control partition"),
            "unexpected error: {err}"
        );
        assert!(stream.is_empty(), "a rejected write must emit no bytes");
    }

    #[test]
    fn minimal_header_round_trips() {
        roundtrip(&sample_header());
    }

    #[test]
    fn dimensions_and_scale_round_trip() {
        for (w, h, hs, vs) in [
            (1u16, 1u16, 0u8, 0u8),
            (16, 16, 0, 0),
            (16383, 1, 3, 0),
            (17, 9, 1, 2),
        ] {
            let mut header = sample_header();
            header.width = w;
            header.height = h;
            header.horizontal_scale = hs;
            header.vertical_scale = vs;
            roundtrip(&header);
        }
    }

    #[test]
    fn quant_filter_and_flags_round_trip() {
        let mut header = sample_header();
        header.quant = QuantIndices {
            y_ac: 100,
            y_dc_delta: 7,
            y2_dc_delta: -8,
            y2_ac_delta: 15,
            uv_dc_delta: -1,
            uv_ac_delta: 0,
        };
        header.loop_filter = LoopFilterParams {
            simple: true,
            level: 47,
            sharpness: 5,
            ..Default::default()
        };
        header.color_space = 1;
        header.clamp_required = false;
        header.refresh_entropy_probs = false;
        header.version = 3;
        roundtrip(&header);
    }

    #[test]
    fn skip_probability_round_trips() {
        let mut header = sample_header();
        header.mb_no_skip_coeff = true;
        header.prob_skip_false = 210;
        roundtrip(&header);
    }

    #[test]
    fn partition_counts_round_trip() {
        for count in [1u8, 2, 4, 8] {
            let mut header = sample_header();
            header.token_partitions = count;
            roundtrip(&header);
        }
    }

    #[test]
    fn rejects_inter_frame_and_bad_start_code() {
        // Inter frame: frame-tag bit 0 set.
        assert!(matches!(
            read_uncompressed_chunk(&[0x01, 0, 0, 0x9d, 0x01, 0x2a, 0, 0, 0, 0]),
            Err(error) if error.kind() == gamut_core::ErrorKind::Unsupported
        ));
        // Key frame with a corrupted start code.
        assert!(matches!(
            read_uncompressed_chunk(&[0x00, 0, 0, 0x9d, 0x01, 0x2b, 16, 0, 16, 0]),
            Err(error) if error.kind() == gamut_core::ErrorKind::InvalidInput
        ));
        // Truncated.
        assert!(read_uncompressed_chunk(&[0x00, 0, 0]).is_err());
    }

    #[test]
    fn mb_lf_adjustments_round_trip_and_disable() {
        // The deltas survive a write→read of the `mb_lf_adjustments()` record across the all-zero
        // (disabled), ref-only, mode-only, and fully-populated cases.
        for deltas in [
            LoopFilterDeltas::default(),
            LoopFilterDeltas {
                ref_frame: [5, 0, -3, 0],
                mode: [0; 4],
            },
            LoopFilterDeltas {
                ref_frame: [0; 4],
                mode: [-8, 0, 0, 7],
            },
            LoopFilterDeltas {
                ref_frame: [1, -2, 3, -4],
                mode: [10, -20, 30, -31],
            },
        ] {
            let mut enc = BoolEncoder::new();
            write_mb_lf_adjustments(&mut enc, &deltas);
            let bytes = enc.finish();
            let got = read_mb_lf_adjustments(&mut BoolDecoder::new(&bytes));
            assert_eq!(got, deltas, "deltas {deltas:?} must round-trip");
        }
        // A disabled (all-zero) record is a single `0` bit, so it must encode strictly shorter than a
        // populated one — pinning that the encoder takes the disabled path when there is nothing to send.
        let enc_len = |d: &LoopFilterDeltas| {
            let mut e = BoolEncoder::new();
            write_mb_lf_adjustments(&mut e, d);
            e.finish().len()
        };
        assert!(
            enc_len(&LoopFilterDeltas::default())
                < enc_len(&LoopFilterDeltas {
                    ref_frame: [9; 4],
                    mode: [9; 4],
                }),
            "an all-zero adjustment record must encode shorter than a populated one"
        );
    }

    #[test]
    fn loop_filter_deltas_round_trip_through_frame_header() {
        let mut header = sample_header();
        header.loop_filter = LoopFilterParams {
            simple: false,
            level: 30,
            sharpness: 0,
            deltas: LoopFilterDeltas {
                ref_frame: [5, 0, -3, 0],
                mode: [-8, 0, 0, 7],
            },
        };
        roundtrip(&header);
    }

    #[test]
    fn segmentation_round_trips() {
        let mut header = sample_header();
        header.segmentation = Segmentation {
            enabled: true,
            update_map: true,
            abs_delta: false,
            quantizer: [-8, -2, 5, 12],
            filter_strength: [0; 4],
            tree_probs: [120, 200, 64],
        };
        let chunk = UncompressedChunk {
            is_key_frame: true,
            version: 0,
            show_frame: true,
            first_partition_size: 0,
            width: header.width,
            height: header.height,
            horizontal_scale: 0,
            vertical_scale: 0,
        };
        let mut enc = BoolEncoder::new();
        write_frame_header(&mut enc, &header, &DEFAULT_COEFF_PROBS);
        let bytes = enc.finish();
        let (decoded, _) = read_frame_header(&chunk, &mut BoolDecoder::new(&bytes));
        assert_eq!(decoded.segmentation, header.segmentation);
    }

    #[test]
    fn read_uncompressed_chunk_length_boundaries() {
        // Three bytes are enough to read the frame tag, so the key-frame check fires and an inter
        // frame is rejected as `Unsupported`; `data.len() < 3` widened to `<= 3` would instead
        // reject it as a truncated tag (`InvalidInput`) before the tag is ever examined.
        assert!(matches!(
            read_uncompressed_chunk(&[0x01, 0x00, 0x00]),
            Err(error) if error.kind() == gamut_core::ErrorKind::Unsupported
        ));
        // Exactly `UNCOMPRESSED_CHUNK_LEN` (10) bytes is a complete minimal key-frame chunk; the
        // `data.len() < 10` guard widened to `<= 10` would reject this valid input.
        let minimal = [0x00, 0, 0, 0x9d, 0x01, 0x2a, 16, 0, 16, 0];
        let chunk = read_uncompressed_chunk(&minimal).expect("a 10-byte chunk is complete");
        assert_eq!((chunk.width, chunk.height), (16, 16));
    }

    #[test]
    fn show_frame_flag_is_decoded() {
        // `write_uncompressed_chunk` always sets show_frame (tag bit 4); a round-trip must read it
        // back true — pinning the `>> 4` shift and `!= 0` test (`<< 4` / `== 0` would clear it).
        let mut stream = Vec::new();
        write_uncompressed_chunk(&sample_header(), 0, &mut stream).expect("zero fits");
        assert!(read_uncompressed_chunk(&stream).expect("chunk").show_frame);
        // A frame tag with bit 4 clear must decode to show_frame = false — pinning the `& 1` mask
        // against `| 1` / `^ 1`, which would force the bit set.
        let no_show = [0x00, 0, 0, 0x9d, 0x01, 0x2a, 16, 0, 16, 0];
        assert!(!read_uncompressed_chunk(&no_show).expect("chunk").show_frame);
    }

    #[test]
    fn segmentation_filter_strength_only_round_trips() {
        // The quantizer deltas are zero but the filter-strength deltas are not, so whether the
        // feature-data block is written hinges on the `filter_strength != [0; 4]` term; `!=` flipped
        // to `==` would skip the block and silently drop the filter deltas.
        let mut header = sample_header();
        header.segmentation = Segmentation {
            enabled: true,
            update_map: false,
            abs_delta: false,
            quantizer: [0; 4],
            filter_strength: [3, -6, 9, -12],
            tree_probs: [255; 3],
        };
        let chunk = UncompressedChunk {
            is_key_frame: true,
            version: 0,
            show_frame: true,
            first_partition_size: 0,
            width: header.width,
            height: header.height,
            horizontal_scale: 0,
            vertical_scale: 0,
        };
        let mut enc = BoolEncoder::new();
        write_frame_header(&mut enc, &header, &DEFAULT_COEFF_PROBS);
        let bytes = enc.finish();
        let (decoded, _) = read_frame_header(&chunk, &mut BoolDecoder::new(&bytes));
        assert_eq!(decoded.segmentation, header.segmentation);
    }
}
