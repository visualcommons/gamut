//! Differential cross-check of the HDR static-metadata chunks (`cICP`, `mDCV`, `cLLI`) against a
//! vendored **libpng**: the encoder's chunks reach the reference reader intact and in the position
//! the PNG specification requires.

use gamut_core::{Dimensions, ImageRef, Indexed8};
use gamut_png::{Palette, PngEncoder};

/// The chunk types of `png`, in file order.
fn chunk_types(png: &[u8]) -> Vec<[u8; 4]> {
    let mut types = Vec::new();
    let mut at = 8usize;
    while at + 12 <= png.len() {
        let len = u32::from_be_bytes([png[at], png[at + 1], png[at + 2], png[at + 3]]) as usize;
        types.push([png[at + 4], png[at + 5], png[at + 6], png[at + 7]]);
        at += 12 + len;
    }
    types
}

/// The HDR static metadata reaches the reference reader in the chunks §11.3.2.7 and §11.3.2.8
/// define, with the bytes Tables 19 and 20 lay out, before `PLTE` as §5.6 Table 5 requires. The
/// vendored libpng 1.6.43 predates both chunks, so it keeps them as unknown chunks and reports
/// their payloads verbatim (`libpng_oracle::KEPT_UNKNOWN_CHUNKS`); what libpng attests is the
/// framing, the CRC and the position, and the expected bytes below are the spec's own examples.
/// A value-level cross-check in both directions waits on a vendored libpng >= 1.6.45 (#1256).
///
/// A palette image, so the position claim has a `PLTE` to be measured against. The level is set
/// from the ISOBMFF-shaped whole-cd/m² type, which converts exactly.
#[test]
fn hdr_chunks_reach_libpng_verbatim_before_plte() {
    let (w, h) = (6u32, 4u32);
    let palette = Palette::new(&[[0, 0, 0], [255, 0, 0], [0, 0, 255]]).unwrap();
    let indices: Vec<u8> = (0..(w * h) as usize).map(|i| (i % 3) as u8).collect();
    let mut png = Vec::new();
    PngEncoder::new()
        .with_cicp(gamut_png::Cicp::new(9, 16, true))
        .with_mastering_display(gamut_png::MasteringDisplay::new(
            [[35_400, 14_600], [8_500, 39_850], [6_550, 2_300]],
            [15_635, 16_450],
            40_000_000,
            5,
        ))
        .with_content_light_level(
            gamut_png::ContentLightLevel {
                max_content_light_level: 1000,
                max_pic_average_light_level: 250,
            }
            .into(),
        )
        .encode_indexed8(
            ImageRef::<Indexed8>::new(&indices, Dimensions::new(w, h).unwrap()).unwrap(),
            &palette,
            &mut png,
        )
        .expect("encode");

    let dec = libpng_oracle::decode(&png);
    assert_eq!(dec.warnings, 0, "libpng dropped nothing");
    assert_eq!(dec.pixels, indices);
    assert_eq!(
        dec.unknown_chunks,
        [
            (*b"cICP", vec![9, 16, 0, 1]),
            (
                *b"mDCV",
                vec![
                    0x8A, 0x48, 0x39, 0x08, 0x21, 0x34, 0x9B, 0xAA, 0x19, 0x96, 0x08,
                    0xFC, // RGB
                    0x3D, 0x13, 0x40, 0x42, // D65
                    0x02, 0x62, 0x5A, 0x00, 0x00, 0x00, 0x00, 0x05, // 4000 and 0.0005 cd/m²
                ]
            ),
            (
                *b"cLLI",
                vec![0x00, 0x98, 0x96, 0x80, 0x00, 0x26, 0x25, 0xA0]
            ),
        ]
    );

    let types = chunk_types(&png);
    let at = |ty: &[u8; 4]| types.iter().position(|t| t == ty).expect("chunk present");
    assert!(at(b"mDCV") < at(b"PLTE") && at(b"cLLI") < at(b"PLTE"));
}
