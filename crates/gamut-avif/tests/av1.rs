//! The av1C / OBU layer: golden field-exact `Av1Config` parses, the low-overhead OBU split, the
//! `full_stream` bridge, and the `validate_still_payload` still-image constraint matrix
//! (AV1-ISOBMFF v1.3.0 §2.3/§2.4; AVIF v1.2.0 §2.1; AV1 §4.10.5/§5.3).

use gamut_avif::Av1Config;
use gamut_color::ChromaSubsampling;
use gamut_core::{Error, ErrorKind};

/// Appends the minimal `leb128()` encoding of `value`.
fn leb128(mut value: usize, out: &mut Vec<u8>) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            break;
        }
        out.push(byte | 0x80);
    }
}

/// A size-fielded OBU of the given type (no extension header).
fn obu(ty: u8, payload: &[u8]) -> Vec<u8> {
    let mut v = vec![(ty << 3) | 0x02];
    leb128(payload.len(), &mut v);
    v.extend_from_slice(payload);
    v
}

/// A sequence-header OBU whose first payload byte carries
/// `seq_profile(3) | still_picture(1) | reduced_still_picture_header(1)`.
fn seq_obu(reduced: bool) -> Vec<u8> {
    let b0 = if reduced { 0x18 } else { 0x00 }; // still_picture follows reduced in practice
    obu(1, &[b0, 0xAA])
}

/// A frame OBU whose first payload byte carries
/// `show_existing_frame(1) | frame_type(2) | show_frame(1)`.
fn frame_obu(first_byte: u8) -> Vec<u8> {
    obu(6, &[first_byte, 0xBB, 0xCC])
}

fn concat(parts: &[Vec<u8>]) -> Vec<u8> {
    parts.iter().flatten().copied().collect()
}

#[track_caller]
fn assert_invalid<T: core::fmt::Debug>(result: Result<T, Error>, message: &str) -> Error {
    match result {
        Err(error) if error.kind() == ErrorKind::InvalidInput => {
            assert_eq!(error.static_message(), Some(message));
            error
        }
        other => panic!("expected InvalidInput({message:?}), got {other:?}"),
    }
}

// ---- av1C parse ------------------------------------------------------------------------------

#[test]
fn av1c_golden_parse_is_field_exact() {
    // marker+version 0x81; profile 1, level 0x0C; tier 0, high_bitdepth, 4:4:4;
    // presentation delay present, minus-one = 7.
    let record = [0x81, (1 << 5) | 0x0C, 0x40, 0x17];
    let c = Av1Config::parse(&record).unwrap();
    assert_eq!(c.seq_profile, 1);
    assert_eq!(c.seq_level_idx_0, 0x0C);
    assert_eq!(c.seq_tier_0, 0);
    assert!(c.high_bitdepth);
    assert!(!c.twelve_bit);
    assert!(!c.monochrome);
    assert_eq!((c.chroma_subsampling_x, c.chroma_subsampling_y), (0, 0));
    assert_eq!(c.chroma_sample_position, 0);
    assert_eq!(c.initial_presentation_delay_minus_one, Some(7));
    assert!(c.config_obus.is_empty());
    assert_eq!(c.bit_depth(), 10);
    assert_eq!(c.chroma_subsampling(), ChromaSubsampling::Cs444);
}

#[test]
fn av1c_reserved_bits_are_ignored() {
    // The three reserved bits before the delay flag, and the four reserved delay bits when the
    // flag is clear, must not affect the parse.
    let zeroed = Av1Config::parse(&[0x81, 0x00, 0x0C, 0x00]).unwrap();
    let dirty = Av1Config::parse(&[0x81, 0x00, 0x0C, 0xE7]).unwrap();
    assert_eq!(zeroed, dirty);
    assert_eq!(zeroed.initial_presentation_delay_minus_one, None);
}

#[test]
fn av1c_bit_depth_covers_every_profile_arm() {
    let parse = |b1: u8, b2: u8| Av1Config::parse(&[0x81, b1, b2, 0x00]).unwrap();
    assert_eq!(parse(0 << 5, 0x0C).bit_depth(), 8); // profile 0, no high_bitdepth: 4:2:0
    assert_eq!(parse(0 << 5, 0x4C).bit_depth(), 10); // high_bitdepth
    assert_eq!(parse(2 << 5, 0x68).bit_depth(), 12); // profile 2, high_bitdepth + twelve_bit, 4:2:2
    assert_eq!(parse(2 << 5, 0x28).bit_depth(), 8); // twelve_bit without high_bitdepth is inert
    // Chroma mapping: mono / 4:2:0 / 4:2:2 / 4:4:4.
    assert_eq!(
        parse(0, 0x1C).chroma_subsampling(),
        ChromaSubsampling::Cs400
    );
    assert_eq!(
        parse(0, 0x0C).chroma_subsampling(),
        ChromaSubsampling::Cs420
    );
    assert_eq!(
        parse(2 << 5, 0x08).chroma_subsampling(),
        ChromaSubsampling::Cs422
    );
    assert_eq!(
        parse(1 << 5, 0x00).chroma_subsampling(),
        ChromaSubsampling::Cs444
    );
    // Ceiling-division chroma plane dimensions on odd luma sizes.
    assert_eq!(ChromaSubsampling::Cs420.chroma_dimensions(5, 3), (3, 2));
    assert_eq!(ChromaSubsampling::Cs422.chroma_dimensions(5, 3), (3, 3));
    assert_eq!(ChromaSubsampling::Cs444.chroma_dimensions(5, 3), (5, 3));
    assert_eq!(ChromaSubsampling::Cs400.chroma_dimensions(5, 3), (0, 0));
}

#[test]
fn av1c_keeps_config_obus_verbatim_and_requires_their_size_fields() {
    let seq = seq_obu(true);
    let mut record = vec![0x81, 1 << 5, 0x00, 0x00];
    record.extend_from_slice(&seq);
    let c = Av1Config::parse(&record).unwrap();
    assert_eq!(c.config_obus, seq);
    // A sizeless configOBU violates the §2.3.4 SHALL.
    let mut sizeless = vec![0x81, 1 << 5, 0x00, 0x00, 1 << 3];
    sizeless.extend_from_slice(&[0x18, 0xAA]);
    assert_invalid(
        Av1Config::parse(&sizeless),
        "AVIF: av1C configOBUs must carry size fields",
    );
}

#[test]
fn av1c_rejects_malformed_records() {
    assert_invalid(
        Av1Config::parse(&[0x81, 0x00, 0x0C]),
        "AVIF: av1C truncated",
    );
    assert_invalid(
        Av1Config::parse(&[0x01, 0x00, 0x0C, 0x00]),
        "AVIF: av1C marker must be 1",
    );
    match Av1Config::parse(&[0x82, 0x00, 0x0C, 0x00]) {
        Err(error) if error.kind() == ErrorKind::Unsupported => {
            assert_eq!(error.static_message(), Some("AVIF: av1C version must be 1"))
        }
        other => panic!("expected Unsupported, got {other:?}"),
    }
    // Subsampling (0, 1) is not expressible by an AV1 sequence header.
    assert_invalid(
        Av1Config::parse(&[0x81, 0x00, 0x04, 0x00]),
        "AVIF: av1C chroma subsampling (0, 1) is not expressible",
    );
    // Monochrome requires (1, 1).
    assert_invalid(
        Av1Config::parse(&[0x81, 0x00, 0x18, 0x00]),
        "AVIF: av1C monochrome requires chroma subsampling (1, 1)",
    );
}

// ---- full_stream -----------------------------------------------------------------------------

#[test]
fn full_stream_prepends_delimiter_and_config_obus() {
    let seq = seq_obu(true);
    let mut record = vec![0x81, 1 << 5, 0x00, 0x00];
    record.extend_from_slice(&seq);
    let c = Av1Config::parse(&record).unwrap();
    let frame = frame_obu(0x10);
    let mut out = vec![0xEE]; // pre-existing bytes must be preserved (append semantics)
    c.full_stream(&frame, &mut out).unwrap();
    let expected = concat(&[vec![0xEE, 0x12, 0x00], seq, frame]);
    assert_eq!(out, expected);
}

#[test]
fn full_stream_normalizes_a_sizeless_last_obu() {
    let c = Av1Config::parse(&[0x81, 1 << 5, 0x00, 0x00]).unwrap();
    // seq (sized) + frame without a size field.
    let mut payload = seq_obu(true);
    payload.push(6 << 3);
    payload.extend_from_slice(&[0x10, 0xBB, 0xCC]);
    let mut out = Vec::new();
    c.full_stream(&payload, &mut out).unwrap();
    // The frame is re-emitted with obu_has_size_field set and a minimal leb128 size.
    let expected = concat(&[vec![0x12, 0x00], seq_obu(true), frame_obu(0x10)]);
    assert_eq!(out, expected);
}

/// A sizeless OBU carrying exactly one payload byte, which is what separates the header length
/// from a plausible-looking wrong answer.
///
/// `full_stream` computes `header_len = raw.len() - payload.len()`. The neighbouring test uses a
/// three-byte payload behind a one-byte header, where `4 - 3` and `4 / 3` are both 1 -- so the
/// division mutant produced identical output and survived (#110). With a one-byte payload the two
/// part company: `2 - 1` is 1 and `2 / 1` is 2, and the mutant copies the payload byte into the
/// header and then emits it again.
#[test]
fn full_stream_normalizes_a_sizeless_obu_with_a_single_payload_byte() {
    let c = Av1Config::parse(&[0x81, 1 << 5, 0x00, 0x00]).unwrap();
    let mut payload = seq_obu(true);
    payload.push(6 << 3); // frame OBU header, no size field, no extension
    payload.push(0x10); // exactly one payload byte
    let mut out = Vec::new();
    c.full_stream(&payload, &mut out).unwrap();
    let expected = concat(&[vec![0x12, 0x00], seq_obu(true), obu(6, &[0x10])]);
    assert_eq!(out, expected);
}

/// A payload long enough to need a two-byte leb128 size, which is the only way to observe the
/// continuation bit.
///
/// `write_leb128` sets it with `byte | 0x80`. Every OBU in the suite was short enough to encode
/// its size in one byte, so the loop never took a second pass and the continuation bit was never
/// written -- mutating that `|` to `&`, which would clear the byte entirely, changed nothing
/// observable (#110).
#[test]
fn full_stream_writes_a_multi_byte_leb128_size() {
    let c = Av1Config::parse(&[0x81, 1 << 5, 0x00, 0x00]).unwrap();
    // 200 bytes: over the 127-byte single-byte leb128 limit, so the size encodes as 0xC8 0x01.
    let body: Vec<u8> = (0..200u32).map(|i| (i % 251) as u8).collect();
    let mut payload = seq_obu(true);
    payload.push(6 << 3); // sizeless frame OBU, so full_stream must write the size itself
    payload.extend_from_slice(&body);
    let mut out = Vec::new();
    c.full_stream(&payload, &mut out).unwrap();

    let expected = concat(&[vec![0x12, 0x00], seq_obu(true), obu(6, &body)]);
    assert_eq!(out, expected);
    // Stated directly as well, so the intent survives a change to the `obu` helper.
    assert!(
        out.windows(2).any(|w| w == [0xC8, 0x01]),
        "the 200-byte size must be encoded as a two-byte leb128"
    );
}

// ---- validate_still_payload ------------------------------------------------------------------

fn config() -> Av1Config {
    Av1Config::parse(&[0x81, 1 << 5, 0x00, 0x00]).unwrap()
}

#[test]
fn still_payload_accepts_the_canonical_shapes() {
    // Reduced still picture: the frame is a key frame by construction.
    config()
        .validate_still_payload(&concat(&[seq_obu(true), frame_obu(0xFF)]))
        .unwrap();
    // Non-reduced: the frame's fixed bits must read show_existing 0, KEY, show_frame 1.
    config()
        .validate_still_payload(&concat(&[seq_obu(false), frame_obu(0x10)]))
        .unwrap();
    // SHOULD-level shapes are tolerated: temporal delimiter, metadata before the sequence
    // header, padding after the frame, and a frame-header/tile-group pair.
    config()
        .validate_still_payload(&concat(&[
            obu(2, &[]),
            obu(5, &[0x01]),
            seq_obu(true),
            obu(3, &[0xFF]),
            obu(4, &[0xDD]),
            obu(15, &[0x00]),
        ]))
        .unwrap();
}

#[test]
fn still_payload_requires_exactly_one_sequence_header() {
    assert_invalid(
        config().validate_still_payload(&concat(&[seq_obu(true), seq_obu(true), frame_obu(0xFF)])),
        "AVIF: item payload must have exactly one sequence header OBU",
    );
    // No sequence header at all: the frame arrives first.
    assert_invalid(
        config().validate_still_payload(&frame_obu(0x10)),
        "AVIF: sequence header OBU must precede the first frame",
    );
    // A sequence header only after the first frame is the same ordering violation.
    assert_invalid(
        config().validate_still_payload(&concat(&[frame_obu(0x10), seq_obu(true)])),
        "AVIF: sequence header OBU must precede the first frame",
    );
}

#[test]
fn still_payload_requires_a_frame() {
    assert_invalid(
        config().validate_still_payload(&seq_obu(true)),
        "AVIF: item payload has no frame-bearing OBU",
    );
    assert_invalid(
        config().validate_still_payload(&[]),
        "AVIF: item payload must have exactly one sequence header OBU",
    );
}

#[test]
fn still_payload_rejects_a_tile_list() {
    assert_invalid(
        config().validate_still_payload(&concat(&[
            seq_obu(true),
            obu(8, &[0x00]),
            frame_obu(0xFF),
        ])),
        "AVIF: item payload must not contain a tile list OBU",
    );
}

#[test]
fn still_payload_rejects_a_leading_tile_group() {
    // A tile group before any frame header is malformed even under reduced_still_picture_header.
    assert_invalid(
        config().validate_still_payload(&concat(&[seq_obu(true), obu(4, &[0xDD])])),
        "AVIF: tile group OBU precedes the first frame header",
    );
}

#[test]
fn still_payload_checks_the_first_frame_bits() {
    let with_frame =
        |b0: u8| config().validate_still_payload(&concat(&[seq_obu(false), frame_obu(b0)]));
    assert_invalid(
        with_frame(0x90),
        "AVIF: first frame must not be show_existing_frame",
    );
    assert_invalid(with_frame(0x30), "AVIF: first frame must be a key frame");
    assert_invalid(
        with_frame(0x00),
        "AVIF: first frame must have show_frame set",
    );
    // Empty sequence-header / frame OBUs cannot carry the bits to check.
    assert_invalid(
        config().validate_still_payload(&concat(&[obu(1, &[]), frame_obu(0x10)])),
        "AVIF: empty sequence header OBU",
    );
    assert_invalid(
        config().validate_still_payload(&concat(&[seq_obu(false), obu(6, &[])])),
        "AVIF: empty frame OBU",
    );
}

// ---- coherence with the crate's own encoder --------------------------------------------------

#[test]
fn encoder_output_carries_a_parseable_av1c_and_a_valid_still_payload() {
    use gamut_core::{Dimensions, EncodeImage, ImageRef, Rgb8};
    use gamut_isobmff::PropertyKind;

    let rgb: Vec<u8> = (0..8 * 8 * 3).map(|i| (i * 31) as u8).collect();
    let image = ImageRef::<Rgb8>::new(&rgb, Dimensions::new(8, 8).unwrap()).unwrap();
    let mut file = Vec::new();
    gamut_avif::AvifEncoder::new()
        .encode_image(image, &mut file)
        .unwrap();

    let parsed = gamut_isobmff::IsoBmffImage::parse(&file).unwrap();
    let item = &parsed.items[0];
    let av1c = item
        .properties
        .iter()
        .find_map(|p| match &p.kind {
            PropertyKind::CodecConfiguration { kind, data } => (kind == b"av1C").then_some(data),
            _ => None,
        })
        .expect("av1C present");
    let config = Av1Config::parse(av1c).unwrap();
    // The identity 8-bit 4:4:4 path: High profile, no high_bitdepth, no subsampling.
    assert_eq!(config.seq_profile, 1);
    assert_eq!(config.bit_depth(), 8);
    assert_eq!(config.chroma_subsampling(), ChromaSubsampling::Cs444);
    assert!(config.config_obus.is_empty());
    // The emitted temporal unit satisfies the still-image constraints this crate enforces.
    config.validate_still_payload(&item.payload).unwrap();
    // And its OBUs split cleanly with exactly one sequence header.
    let types: Vec<u8> = gamut_av1::ObuIter::new(&item.payload)
        .map(|o| o.map(|o| o.obu_type))
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        types
            .iter()
            .filter(|t| **t == gamut_av1::Obu::SEQUENCE_HEADER)
            .count(),
        1
    );
}
