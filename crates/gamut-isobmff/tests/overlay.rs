//! `ImageOverlay` (`iovl`) payload parse/serialise round-trip plus hand-authored layout fixtures
//! (ISO/IEC 23008-12 §6.6.2.4.2), pinned independently of the serialiser.

use gamut_core::ErrorKind;
use gamut_isobmff::{ImageOverlay, OverlayOffset};

/// A 16-bit-form payload: version 0, flags 0, canvas fill R,G,B,A = 1111/2222/3333/FFFF, canvas
/// 64×48, two composed inputs at (10, -1) and (-5, 32000) — negative offsets pin sign extension.
const GOLDEN_16: [u8; 22] = [
    0x00, 0x00, // version, flags (16-bit fields)
    0x11, 0x11, 0x22, 0x22, 0x33, 0x33, 0xFF, 0xFF, // canvas_fill_value R,G,B,A
    0x00, 0x40, // output_width 64
    0x00, 0x30, // output_height 48
    0x00, 0x0A, 0xFF, 0xFF, // offset[0] = (10, -1)
    0xFF, 0xFB, 0x7D, 0x00, // offset[1] = (-5, 32000)
];

/// A 32-bit-form payload: version 0, flags 1, same canvas fill, canvas 65536×1, one composed input
/// at (-70000, 100000) — a magnitude no 16-bit field can hold, negative offset pins sign extension.
const GOLDEN_32: [u8; 26] = [
    0x00, 0x01, // version, flags (32-bit fields)
    0x11, 0x11, 0x22, 0x22, 0x33, 0x33, 0xFF, 0xFF, // canvas_fill_value R,G,B,A
    0x00, 0x01, 0x00, 0x00, // output_width 65536
    0x00, 0x00, 0x00, 0x01, // output_height 1
    0xFF, 0xFE, 0xEE, 0x90, // offset[0].h = -70000
    0x00, 0x01, 0x86, 0xA0, // offset[0].v = 100000
];

fn model_16() -> ImageOverlay {
    ImageOverlay {
        canvas_fill_value: [0x1111, 0x2222, 0x3333, 0xFFFF],
        output_width: 64,
        output_height: 48,
        offsets: vec![
            OverlayOffset {
                horizontal: 10,
                vertical: -1,
            },
            OverlayOffset {
                horizontal: -5,
                vertical: 32000,
            },
        ],
    }
}

fn model_32() -> ImageOverlay {
    ImageOverlay {
        canvas_fill_value: [0x1111, 0x2222, 0x3333, 0xFFFF],
        output_width: 65536,
        output_height: 1,
        offsets: vec![OverlayOffset {
            horizontal: -70000,
            vertical: 100000,
        }],
    }
}

/// A canvas with a distinctive fill and the given dims and `(horizontal, vertical)` offsets, for
/// form-selection boundary probes.
fn ov(output_width: u32, output_height: u32, offsets: Vec<(i32, i32)>) -> ImageOverlay {
    ImageOverlay {
        canvas_fill_value: [0, 0, 0, 0xFFFF],
        output_width,
        output_height,
        offsets: offsets
            .into_iter()
            .map(|(horizontal, vertical)| OverlayOffset {
                horizontal,
                vertical,
            })
            .collect(),
    }
}

/// The serialised field-length flag: 0 = compact 16-bit form, 1 = wide 32-bit form.
fn form(o: &ImageOverlay) -> u8 {
    o.to_bytes().unwrap()[1]
}

#[test]
fn parses_hand_authored_16bit_layout() {
    assert_eq!(ImageOverlay::parse(&GOLDEN_16, 2).unwrap(), model_16());
}

#[test]
fn parses_hand_authored_32bit_layout() {
    assert_eq!(ImageOverlay::parse(&GOLDEN_32, 1).unwrap(), model_32());
}

#[test]
fn to_bytes_matches_hand_authored_layouts() {
    assert_eq!(model_16().to_bytes().unwrap(), GOLDEN_16);
    assert_eq!(model_32().to_bytes().unwrap(), GOLDEN_32);
}

#[test]
fn round_trips_both_forms() {
    for (o, count) in [(model_16(), 2), (model_32(), 1)] {
        let bytes = o.to_bytes().unwrap();
        assert_eq!(ImageOverlay::parse(&bytes, count).unwrap(), o);
    }
}

#[test]
fn form_switches_at_u16_max_dimensions() {
    // Exactly u16::MAX in each dimension (offsets fit i16) → compact form.
    assert_eq!(form(&ov(65535, 65535, vec![(0, 0)])), 0);
    // One past u16::MAX in either dimension → wide form.
    assert_eq!(form(&ov(65536, 1, vec![(0, 0)])), 1);
    assert_eq!(form(&ov(1, 65536, vec![(0, 0)])), 1);
}

#[test]
fn form_switches_at_i16_offset_bounds() {
    // i16::MAX / i16::MIN offsets (dims fit u16) → compact form.
    let edge = ov(8, 8, vec![(i32::from(i16::MAX), i32::from(i16::MIN))]);
    assert_eq!(form(&edge), 0);
    // One past i16::MAX or below i16::MIN → wide form.
    assert_eq!(form(&ov(8, 8, vec![(i32::from(i16::MAX) + 1, 0)])), 1);
    assert_eq!(form(&ov(8, 8, vec![(0, i32::from(i16::MIN) - 1)])), 1);
}

#[test]
fn boundary_values_round_trip() {
    // Compact-form extremes: u16::MAX dims and the full i16 offset range must survive a round-trip.
    let compact = ov(
        65535,
        65535,
        vec![(i32::from(i16::MAX), i32::from(i16::MIN)), (-1, 1)],
    );
    let compact_bytes = compact.to_bytes().unwrap();
    assert_eq!(compact_bytes[1], 0, "stays compact");
    assert_eq!(ImageOverlay::parse(&compact_bytes, 2).unwrap(), compact);

    // Wide-form extremes: dims past u16 and offsets outside i16, including i32 extremes.
    let wide = ov(
        70_000,
        3,
        vec![(i32::MIN, i32::MAX), (i32::from(i16::MAX) + 1, -40_000)],
    );
    let wide_bytes = wide.to_bytes().unwrap();
    assert_eq!(wide_bytes[1], 1, "goes wide");
    assert_eq!(ImageOverlay::parse(&wide_bytes, 2).unwrap(), wide);
}

#[test]
fn parses_zero_references() {
    // No composed inputs: header only, no offset pairs.
    let bytes = [
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // version/flags + fill
        0x00, 0x08, 0x00, 0x08, // 8×8 canvas
    ];
    assert_eq!(
        ImageOverlay::parse(&bytes, 0).unwrap(),
        ImageOverlay {
            canvas_fill_value: [0, 0, 0, 0],
            output_width: 8,
            output_height: 8,
            offsets: vec![],
        },
    );
}

#[test]
fn rejects_nonzero_version() {
    let mut bytes = GOLDEN_16;
    bytes[0] = 0x01;
    assert_eq!(
        ImageOverlay::parse(&bytes, 2).unwrap_err().kind(),
        ErrorKind::Unsupported
    );
}

#[test]
fn rejects_truncated_payload() {
    // One byte short of the second offset pair.
    let bytes = &GOLDEN_16[..GOLDEN_16.len() - 1];
    assert_eq!(
        ImageOverlay::parse(bytes, 2).unwrap_err().kind(),
        ErrorKind::InvalidInput
    );
}

#[test]
fn rejects_trailing_bytes() {
    let mut bytes = GOLDEN_16.to_vec();
    bytes.push(0xFF);
    assert_eq!(
        ImageOverlay::parse(&bytes, 2).unwrap_err().kind(),
        ErrorKind::InvalidInput
    );
}

#[test]
fn rejects_reference_count_mismatch() {
    // Too many claimed references: the payload runs out mid-offset (truncation).
    assert_eq!(
        ImageOverlay::parse(&GOLDEN_16, 3).unwrap_err().kind(),
        ErrorKind::InvalidInput
    );
    // Too few claimed references: the unread offset pair is surplus (trailing bytes).
    assert_eq!(
        ImageOverlay::parse(&GOLDEN_16, 1).unwrap_err().kind(),
        ErrorKind::InvalidInput
    );
}
