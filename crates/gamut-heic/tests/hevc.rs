//! S2 (issue #238): the `hvcC` HEVCDecoderConfigurationRecord parse and the HEVC NAL layer.
//!
//! Golden fixtures are hand-authored per `references/heif` §§1–3 (the normative field tables) and
//! every parsed field is asserted exactly, so a mutated bit-shift/mask cannot survive.

mod common;

use common::{clean_file, item};
use gamut_core::ErrorKind;
use gamut_heic::{ChromaFormat, HeifContainer, HevcConfig, NalHeader, NalUnitType, iter_nal_units};
use gamut_isobmff::{Property, PropertyKind};

// ---- byte-builders ---------------------------------------------------------------------------

/// A 23-byte `hvcC` header with the given `lengthSizeMinusOne` and `numOfArrays`; other fields fixed
/// to a Main Still Picture config (profile_space 0, tier 0, profile_idc 3, level 90, 4:2:0, 8-bit).
/// Reserved bits are written all-ones as the spec (§1) recommends.
fn hvcc_header(length_size_minus_one: u8, num_of_arrays: u8) -> Vec<u8> {
    // constantFrameRate 0 | numTemporalLayers 1 | temporalIdNested 1 | lengthSizeMinusOne.
    let packed = (1 << 3) | (1 << 2) | (length_size_minus_one & 0x03);
    vec![
        0x01, // configurationVersion
        0x03, // profile_space 0 | tier 0 | profile_idc 3
        0x60,
        0x00,
        0x00,
        0x00, // profile_compatibility_flags = 0x60000000
        0x90,
        0x00,
        0x00,
        0x00,
        0x00,
        0x00, // constraint_indicator_flags = 0x900000000000
        0x5A, // level_idc = 90
        0xF0,
        0x00, // reserved 1111 | min_spatial_segmentation_idc 0
        0xFC, // reserved | parallelismType 0
        0xFD, // reserved | chroma_format_idc 1 (4:2:0)
        0xF8, // reserved | bit_depth_luma_minus8 0
        0xF8, // reserved | bit_depth_chroma_minus8 0
        0x00,
        0x00, // avgFrameRate 0
        packed,
        num_of_arrays,
    ]
}

/// One `hvcC` parameter-set array: `array_completeness` | reserved 0 | `NAL_unit_type`, then the
/// length-prefixed NAL units.
fn nal_array(completeness: bool, ty: u8, nals: &[&[u8]]) -> Vec<u8> {
    let mut out = vec![(u8::from(completeness) << 7) | (ty & 0x3f)];
    out.extend_from_slice(&(nals.len() as u16).to_be_bytes());
    for n in nals {
        out.extend_from_slice(&(n.len() as u16).to_be_bytes());
        out.extend_from_slice(n);
    }
    out
}

/// The Golden-1 Main Still Picture `hvcC`: VPS + SPS + PPS, one NAL each, `lengthSizeMinusOne` 3.
fn main_still_hvcc() -> Vec<u8> {
    let mut out = hvcc_header(3, 3);
    out.extend(nal_array(true, 32, &[&[0x40, 0x01, 0xAA]])); // VPS
    out.extend(nal_array(true, 33, &[&[0x42, 0x01, 0xBB]])); // SPS
    out.extend(nal_array(true, 34, &[&[0x44, 0x01, 0xCC]])); // PPS
    out
}

// ---- Golden 1: field-exact ------------------------------------------------------------------

#[test]
fn golden_main_still_every_field() {
    let cfg = HevcConfig::parse(&main_still_hvcc()).expect("valid hvcC");
    assert_eq!(cfg.general_profile_space, 0);
    assert!(!cfg.general_tier_flag);
    assert_eq!(cfg.general_profile_idc, 3);
    assert_eq!(cfg.general_profile_compatibility_flags, 0x6000_0000);
    assert_eq!(cfg.general_constraint_indicator_flags, 0x9000_0000_0000);
    assert_eq!(cfg.general_level_idc, 90);
    assert_eq!(cfg.min_spatial_segmentation_idc, 0);
    assert_eq!(cfg.parallelism_type, 0);
    assert_eq!(cfg.chroma_format_idc, 1);
    assert_eq!(cfg.chroma_format(), ChromaFormat::Yuv420);
    assert_eq!(cfg.bit_depth_luma_minus8, 0);
    assert_eq!(cfg.bit_depth_chroma_minus8, 0);
    assert_eq!(cfg.bit_depth_luma(), 8);
    assert_eq!(cfg.bit_depth_chroma(), 8);
    assert_eq!(cfg.avg_frame_rate, 0);
    assert_eq!(cfg.constant_frame_rate, 0);
    assert_eq!(cfg.num_temporal_layers, 1);
    assert!(cfg.temporal_id_nested);
    assert_eq!(cfg.length_size_minus_one, 3);
    assert_eq!(cfg.nal_length_size(), 4);

    assert_eq!(cfg.arrays.len(), 3);
    assert!(cfg.arrays[0].completeness);
    assert_eq!(cfg.arrays[0].nal_unit_type, NalUnitType::Vps);
    assert_eq!(cfg.arrays[0].nal_units, vec![vec![0x40, 0x01, 0xAA]]);
    assert_eq!(cfg.arrays[1].nal_unit_type, NalUnitType::Sps);
    assert_eq!(cfg.arrays[2].nal_unit_type, NalUnitType::Pps);

    // Convenience accessors flatten across arrays, in file order.
    let vps: Vec<&[u8]> = cfg.vps().collect();
    assert_eq!(vps, vec![&[0x40, 0x01, 0xAA][..]]);
    let sps: Vec<&[u8]> = cfg.sps().collect();
    assert_eq!(sps, vec![&[0x42, 0x01, 0xBB][..]]);
    let pps: Vec<&[u8]> = cfg.pps().collect();
    assert_eq!(pps, vec![&[0x44, 0x01, 0xCC][..]]);
}

// ---- Golden 2: 4:2:2 / 10-bit / multiple SPS / SEI array / completeness=false ----------------

fn golden2_hvcc() -> Vec<u8> {
    let mut out = vec![
        0x01, // version
        0x64, // profile_space 1 | tier 1 | profile_idc 4 (Rext)
        0x00, 0x00, 0x00, 0x01, // compatibility_flags = 1
        0x00, 0x00, 0x00, 0x00, 0x00, 0x01, // constraint_indicator_flags = 1
        0x78, // level_idc = 120
        0xF0, 0x0F, // reserved | min_spatial_segmentation_idc 15
        0xFE, // reserved | parallelismType 2 (tile)
        0xFE, // reserved | chroma_format_idc 2 (4:2:2)
        0xFA, // reserved | bit_depth_luma_minus8 2 (10-bit)
        0xFA, // reserved | bit_depth_chroma_minus8 2 (10-bit)
        0x00, 0x00, // avgFrameRate 0
        0x50, // cfr 1 | numTemporalLayers 2 | temporalIdNested 0 | lengthSizeMinusOne 0
        0x03, // numOfArrays = 3
    ];
    // SPS array, completeness=false, two SPS NALs (multiple SPS).
    out.extend(nal_array(
        false,
        33,
        &[&[0x42, 0x01, 0x11], &[0x42, 0x01, 0x22, 0x33]],
    ));
    // PPS array, completeness=true.
    out.extend(nal_array(true, 34, &[&[0x44, 0x01, 0x55]]));
    // PREFIX_SEI array, completeness=false.
    out.extend(nal_array(false, 39, &[&[0x4E, 0x01, 0x99]]));
    out
}

#[test]
fn golden2_extended_every_field() {
    let cfg = HevcConfig::parse(&golden2_hvcc()).expect("valid hvcC");
    assert_eq!(cfg.general_profile_space, 1);
    assert!(cfg.general_tier_flag);
    assert_eq!(cfg.general_profile_idc, 4);
    assert_eq!(cfg.general_profile_compatibility_flags, 1);
    assert_eq!(cfg.general_constraint_indicator_flags, 1);
    assert_eq!(cfg.general_level_idc, 120);
    assert_eq!(cfg.min_spatial_segmentation_idc, 15);
    assert_eq!(cfg.parallelism_type, 2);
    assert_eq!(cfg.chroma_format_idc, 2);
    assert_eq!(cfg.chroma_format(), ChromaFormat::Yuv422);
    assert_eq!(cfg.bit_depth_luma(), 10);
    assert_eq!(cfg.bit_depth_chroma(), 10);
    assert_eq!(cfg.constant_frame_rate, 1);
    assert_eq!(cfg.num_temporal_layers, 2);
    assert!(!cfg.temporal_id_nested);
    assert_eq!(cfg.length_size_minus_one, 0);
    assert_eq!(cfg.nal_length_size(), 1);

    assert_eq!(cfg.arrays.len(), 3);
    assert!(!cfg.arrays[0].completeness);
    assert_eq!(cfg.arrays[0].nal_unit_type, NalUnitType::Sps);
    assert!(cfg.arrays[1].completeness);
    assert!(!cfg.arrays[2].completeness);
    assert_eq!(cfg.arrays[2].nal_unit_type, NalUnitType::PrefixSei);

    // Multiple SPS across the single SPS array, in order.
    let sps: Vec<&[u8]> = cfg.sps().collect();
    assert_eq!(
        sps,
        vec![&[0x42, 0x01, 0x11][..], &[0x42, 0x01, 0x22, 0x33][..]]
    );
    assert_eq!(cfg.vps().count(), 0);
}

#[test]
fn full_chroma_and_444_mapping() {
    // Exercise the chroma-format arm the goldens do not: monochrome (0) and 4:4:4 (3).
    for (idc, expected) in [(0u8, ChromaFormat::Monochrome), (3u8, ChromaFormat::Yuv444)] {
        let mut bytes = hvcc_header(3, 0);
        bytes[16] = 0xFC | idc; // reserved | chroma_format_idc
        let cfg = HevcConfig::parse(&bytes).expect("valid hvcC");
        assert_eq!(cfg.chroma_format_idc, idc);
        assert_eq!(cfg.chroma_format(), expected);
    }
}

// ---- Reserved bits are ignored on read (§1) --------------------------------------------------

#[test]
fn reserved_bits_ignored() {
    // Golden 1 with every reserved bit zeroed (the spec writes them all-ones) must parse identically.
    let mut zeroed = main_still_hvcc();
    zeroed[13] = 0x00; // reserved(4)=0 | min_spatial hi
    zeroed[14] = 0x00; // min_spatial lo (was 0)
    zeroed[15] = 0x00; // reserved(6)=0 | parallelismType 0
    zeroed[16] = 0x01; // reserved(6)=0 | chroma_format_idc 1
    zeroed[17] = 0x00; // reserved(5)=0 | bit_depth_luma_minus8 0
    zeroed[18] = 0x00; // reserved(5)=0 | bit_depth_chroma_minus8 0
    assert_eq!(
        HevcConfig::parse(&zeroed).expect("valid"),
        HevcConfig::parse(&main_still_hvcc()).expect("valid"),
    );
}

// ---- hvcC error paths ------------------------------------------------------------------------

#[test]
fn version_not_one_is_unsupported() {
    let mut bytes = main_still_hvcc();
    bytes[0] = 2;
    assert!(matches!(
        HevcConfig::parse(&bytes),
        Err(error) if error.kind() == ErrorKind::Unsupported
    ));
}

#[test]
fn length_size_minus_one_two_is_invalid() {
    let mut bytes = hvcc_header(0, 0);
    bytes[21] = (bytes[21] & !0x03) | 0x02; // set lengthSizeMinusOne = 2 (illegal)
    assert!(matches!(
        HevcConfig::parse(&bytes),
        Err(error) if error.kind() == ErrorKind::InvalidInput
    ));
}

#[test]
fn truncation_is_invalid() {
    let full = main_still_hvcc();
    // Truncated header (only 10 of 23 bytes).
    assert!(matches!(
        HevcConfig::parse(&full[..10]),
        Err(error) if error.kind() == ErrorKind::InvalidInput
    ));
    // Header present but numOfArrays says 3 and no array bytes follow (array-header boundary).
    assert!(matches!(
        HevcConfig::parse(&full[..23]),
        Err(error) if error.kind() == ErrorKind::InvalidInput
    ));
    // Truncated mid-NAL-body: drop the final PPS RBSP byte.
    assert!(matches!(
        HevcConfig::parse(&full[..full.len() - 1]),
        Err(error) if error.kind() == ErrorKind::InvalidInput
    ));
}

#[test]
fn trailing_byte_after_arrays_is_invalid() {
    let mut bytes = main_still_hvcc();
    bytes.push(0x00);
    assert!(matches!(
        HevcConfig::parse(&bytes),
        Err(error) if error.kind() == ErrorKind::InvalidInput
    ));
}

// ---- NAL iteration: 1/2/4-byte prefixes and boundaries ---------------------------------------

fn collect(payload: &[u8], len_size: usize) -> gamut_core::Result<Vec<Vec<u8>>> {
    iter_nal_units(payload, len_size)
        .map(|r| r.map(<[u8]>::to_vec))
        .collect()
}

#[test]
fn nal_split_one_two_four_byte_prefixes() {
    // 1-byte prefixes.
    let p1 = [0x02, 0xAA, 0xBB, 0x01, 0xCC];
    assert_eq!(collect(&p1, 1).unwrap(), vec![vec![0xAA, 0xBB], vec![0xCC]]);
    // 2-byte prefixes.
    let p2 = [0x00, 0x02, 0xAA, 0xBB, 0x00, 0x01, 0xCC];
    assert_eq!(collect(&p2, 2).unwrap(), vec![vec![0xAA, 0xBB], vec![0xCC]]);
    // 4-byte prefixes.
    let p4 = [
        0x00, 0x00, 0x00, 0x02, 0xAA, 0xBB, 0x00, 0x00, 0x00, 0x01, 0xCC,
    ];
    assert_eq!(collect(&p4, 4).unwrap(), vec![vec![0xAA, 0xBB], vec![0xCC]]);
}

#[test]
fn nal_split_empty_payload_is_zero_units() {
    assert_eq!(iter_nal_units(&[], 4).count(), 0);
    assert_eq!(collect(&[], 4).unwrap(), Vec::<Vec<u8>>::new());
}

#[test]
fn nal_split_zero_length_is_invalid() {
    assert!(collect(&[0x00, 0x00], 2).is_err());
}

#[test]
fn nal_split_truncated_body_is_invalid() {
    // Length says 5 bytes; only one follows.
    assert!(collect(&[0x00, 0x05, 0xAA], 2).is_err());
}

#[test]
fn nal_split_trailing_bytes_are_invalid() {
    // One valid NAL, then a lone byte that cannot begin a 2-byte length prefix.
    assert!(collect(&[0x00, 0x01, 0xAA, 0x99], 2).is_err());
}

// ---- NAL header: classification table + bit extraction ---------------------------------------

#[test]
fn nal_unit_type_classification_table() {
    let named = [
        (16u8, NalUnitType::BlaWLp),
        (17, NalUnitType::BlaWRadl),
        (18, NalUnitType::BlaNLp),
        (19, NalUnitType::IdrWRadl),
        (20, NalUnitType::IdrNLp),
        (21, NalUnitType::CraNut),
        (22, NalUnitType::RsvIrapVcl22),
        (23, NalUnitType::RsvIrapVcl23),
        (32, NalUnitType::Vps),
        (33, NalUnitType::Sps),
        (34, NalUnitType::Pps),
        (39, NalUnitType::PrefixSei),
        (40, NalUnitType::SuffixSei),
    ];
    for (raw, ty) in named {
        assert_eq!(NalUnitType::from_raw(raw), ty);
        assert_eq!(ty.raw(), raw);
    }
    // Boundary values around the named ranges map to Other and round-trip.
    for raw in [15u8, 24, 31, 35, 38, 41, 63] {
        assert_eq!(NalUnitType::from_raw(raw), NalUnitType::Other(raw));
        assert_eq!(NalUnitType::Other(raw).raw(), raw);
    }
    // Classification helpers.
    assert!(NalUnitType::IdrWRadl.is_irap() && NalUnitType::IdrWRadl.is_vcl());
    assert!(NalUnitType::RsvIrapVcl23.is_irap());
    assert!(!NalUnitType::Vps.is_irap() && !NalUnitType::Vps.is_vcl());
    assert!(NalUnitType::Vps.is_parameter_set() && NalUnitType::Pps.is_parameter_set());
    assert!(!NalUnitType::PrefixSei.is_parameter_set() && !NalUnitType::PrefixSei.is_vcl());
    assert!(NalUnitType::Other(1).is_vcl() && !NalUnitType::Other(1).is_irap());
}

#[test]
fn nal_header_bit_extraction() {
    // nal_unit_type 33 (Sps), nuh_layer_id 53 (high bit set), temporal_id_plus1 5 — all non-trivial.
    let header = NalHeader::parse(&[0x43, 0xAD, 0xFF]).expect("valid header");
    assert_eq!(header.unit_type, NalUnitType::Sps);
    assert_eq!(header.layer_id, 53);
    assert_eq!(header.temporal_id_plus1, 5);
}

#[test]
fn nal_header_forbidden_bit_and_truncation() {
    assert!(matches!(
        NalHeader::parse(&[0x80, 0x00]),
        Err(error) if error.kind() == ErrorKind::InvalidInput
    ));
    assert!(matches!(
        NalHeader::parse(&[0x40]),
        Err(error) if error.kind() == ErrorKind::InvalidInput
    ));
}

// ---- annex_b conversion ----------------------------------------------------------------------

#[test]
fn annex_b_golden_output() {
    let cfg = HevcConfig::parse(&main_still_hvcc()).expect("valid");
    // Two payload NALs (4-byte prefixes), both IDR_W_RADL slices.
    let payload = [
        0x00, 0x00, 0x00, 0x03, 0x26, 0x01, 0xDD, // NAL 1
        0x00, 0x00, 0x00, 0x03, 0x26, 0x01, 0xEE, // NAL 2
    ];
    let mut out = vec![0x77]; // pre-existing content: annex_b appends.
    cfg.annex_b(&payload, &mut out).expect("annex_b");
    assert_eq!(
        out,
        vec![
            0x77, //
            0x00, 0x00, 0x00, 0x01, 0x40, 0x01, 0xAA, // VPS
            0x00, 0x00, 0x00, 0x01, 0x42, 0x01, 0xBB, // SPS
            0x00, 0x00, 0x00, 0x01, 0x44, 0x01, 0xCC, // PPS
            0x00, 0x00, 0x00, 0x01, 0x26, 0x01, 0xDD, // payload NAL 1
            0x00, 0x00, 0x00, 0x01, 0x26, 0x01, 0xEE, // payload NAL 2
        ]
    );
}

// ---- split emitters: parameter sets (MediaCodec csd-0) / payload (sample data) ------------------

#[test]
fn annex_b_parameter_sets_emits_only_the_record_in_decoder_order() {
    // Arrays deliberately out of order (PPS, SPS, VPS, SEI); the emit order must be VPS→SPS→PPS→SEI.
    let mut bytes = hvcc_header(3, 4);
    bytes.extend(nal_array(true, 34, &[&[0x44, 0x01, 0xC1]])); // PPS
    bytes.extend(nal_array(true, 33, &[&[0x42, 0x01, 0xB1]])); // SPS
    bytes.extend(nal_array(true, 32, &[&[0x40, 0x01, 0xA1]])); // VPS
    bytes.extend(nal_array(false, 39, &[&[0x4E, 0x01, 0x91]])); // PREFIX_SEI
    let cfg = HevcConfig::parse(&bytes).expect("valid");
    let mut out = vec![0x77]; // pre-existing content: the emitter appends.
    cfg.annex_b_parameter_sets(&mut out);
    assert_eq!(
        out,
        vec![
            0x77, //
            0x00, 0x00, 0x00, 0x01, 0x40, 0x01, 0xA1, // VPS
            0x00, 0x00, 0x00, 0x01, 0x42, 0x01, 0xB1, // SPS
            0x00, 0x00, 0x00, 0x01, 0x44, 0x01, 0xC1, // PPS
            0x00, 0x00, 0x00, 0x01, 0x4E, 0x01, 0x91, // SEI (after PPS)
        ]
    );
}

#[test]
fn annex_b_parameter_sets_of_arrayless_record_emits_nothing() {
    // An `hev1` record may carry no arrays at all (parameter sets inband in the payload).
    let cfg = HevcConfig::parse(&hvcc_header(3, 0)).expect("valid");
    let mut out = Vec::new();
    cfg.annex_b_parameter_sets(&mut out);
    assert!(out.is_empty());
}

#[test]
fn annex_b_payload_emits_only_the_payload() {
    let cfg = HevcConfig::parse(&main_still_hvcc()).expect("valid"); // VPS+SPS+PPS present
    let payload = [
        0x00, 0x00, 0x00, 0x03, 0x26, 0x01, 0xDD, // NAL 1
        0x00, 0x00, 0x00, 0x03, 0x26, 0x01, 0xEE, // NAL 2
    ];
    let mut out = vec![0x77]; // pre-existing content: the emitter appends.
    cfg.annex_b_payload(&payload, &mut out).expect("payload");
    assert_eq!(
        out,
        vec![
            0x77, //
            // No parameter set is emitted — only the two payload NAL units.
            0x00, 0x00, 0x00, 0x01, 0x26, 0x01, 0xDD, // payload NAL 1
            0x00, 0x00, 0x00, 0x01, 0x26, 0x01, 0xEE, // payload NAL 2
        ]
    );
}

#[test]
fn annex_b_payload_honours_nal_length_size() {
    // lengthSizeMinusOne 0 ⇒ one-byte length prefixes.
    let cfg = HevcConfig::parse(&hvcc_header(0, 0)).expect("valid");
    assert_eq!(cfg.nal_length_size(), 1);
    let mut out = Vec::new();
    cfg.annex_b_payload(&[0x03, 0x26, 0x01, 0xDD, 0x02, 0x4E, 0x01], &mut out)
        .expect("payload");
    assert_eq!(
        out,
        vec![
            0x00, 0x00, 0x00, 0x01, 0x26, 0x01, 0xDD, //
            0x00, 0x00, 0x00, 0x01, 0x4E, 0x01,
        ]
    );
}

#[test]
fn annex_b_payload_propagates_split_error() {
    let cfg = HevcConfig::parse(&main_still_hvcc()).expect("valid");
    let mut out = Vec::new();
    // Payload length prefix (4-byte) claims 9 bytes but only 1 follows.
    assert!(matches!(
        cfg.annex_b_payload(&[0x00, 0x00, 0x00, 0x09, 0xAA], &mut out),
        Err(error) if error.kind() == ErrorKind::InvalidInput
    ));
}

#[test]
fn annex_b_is_parameter_sets_then_payload() {
    let cfg = HevcConfig::parse(&main_still_hvcc()).expect("valid");
    let payload = [0x00, 0x00, 0x00, 0x03, 0x26, 0x01, 0xDD];

    let mut whole = Vec::new();
    cfg.annex_b(&payload, &mut whole).expect("annex_b");

    let mut split = Vec::new();
    cfg.annex_b_parameter_sets(&mut split);
    cfg.annex_b_payload(&payload, &mut split).expect("payload");

    assert_eq!(whole, split);
}

// ---- still-image IRAP constraint -------------------------------------------------------------

#[test]
fn validate_still_payload_accepts_irap_and_sei() {
    let cfg = HevcConfig::parse(&main_still_hvcc()).expect("valid"); // nal_length_size 4
    // IDR (19) VCL + PREFIX_SEI (39) non-VCL both permitted.
    let payload = [
        0x00, 0x00, 0x00, 0x03, 0x26, 0x01, 0xDD, // IDR_W_RADL
        0x00, 0x00, 0x00, 0x03, 0x4E, 0x01, 0x99, // PREFIX_SEI
    ];
    assert!(cfg.validate_still_payload(&payload).is_ok());
}

#[test]
fn validate_still_payload_rejects_trailing_picture() {
    let cfg = HevcConfig::parse(&main_still_hvcc()).expect("valid");
    // nal_unit_type 1 (TRAIL_R): a VCL slice that is not IRAP.
    let payload = [0x00, 0x00, 0x00, 0x03, 0x02, 0x01, 0xDD];
    assert!(matches!(
        cfg.validate_still_payload(&payload),
        Err(error) if error.kind() == ErrorKind::InvalidInput
    ));
}

// ---- container wiring: HeifItem::hevc_config -------------------------------------------------

/// A single-`hvc1`-item HEIF file whose primary carries `hvcc_data` as its `hvcC` property.
fn file_with_hvcc(hvcc_data: Vec<u8>) -> Vec<u8> {
    let it = item(1, *b"hvc1", vec![0xAA]).with_properties(vec![Property::new(
        true,
        PropertyKind::CodecConfiguration {
            kind: *b"hvcC",
            data: hvcc_data,
        },
    )]);
    clean_file(1, vec![it])
}

#[test]
fn hevc_config_present_parses() {
    let bytes = file_with_hvcc(main_still_hvcc());
    let container = HeifContainer::parse(&bytes).expect("parse");
    let cfg = container
        .image()
        .primary_item()
        .hevc_config()
        .expect("hvcC present")
        .expect("hvcC valid");
    assert_eq!(cfg.nal_length_size(), 4);
}

#[test]
fn hevc_config_absent_for_av1c_item() {
    // An av01 item with an av1C configuration (no hvcC) → None (absent, not malformed).
    let it = item(1, *b"av01", vec![0xAA]).with_properties(vec![Property::new(
        true,
        PropertyKind::CodecConfiguration {
            kind: *b"av1C",
            data: vec![0x81, 0x00, 0x00, 0x00],
        },
    )]);
    let bytes = clean_file(1, vec![it]);
    let container = HeifContainer::parse(&bytes).expect("parse");
    assert!(container.image().primary_item().hevc_config().is_none());
}

#[test]
fn hevc_config_malformed_is_some_err() {
    // hvcC present but configurationVersion = 2 → Some(Err), distinct from absent.
    let bytes = file_with_hvcc(vec![0x02]);
    let container = HeifContainer::parse(&bytes).expect("parse");
    assert!(matches!(
        container.image().primary_item().hevc_config(),
        Some(Err(_))
    ));
}
