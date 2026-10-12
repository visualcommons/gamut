//! Exact-byte assertions pinning the serialization (mutation killers), plus the property-dedup
//! guarantee. These inspect raw bytes where `read` would normalise away the detail under test
//! (reserved bits, `ipma` flags and essential bits, `ipco` sharing, `infe` flag bytes).

mod common;

use common::{av01_item, image, item};
use gamut_isobmff::{
    CleanAperture, ColourInformation, ContentLightLevel, EntityGroup, ItemReference, NclxColr,
    PixelAspectRatio, Property, PropertyKind, read, write,
};

fn be32(buf: &[u8], p: usize) -> u32 {
    u32::from_be_bytes([buf[p], buf[p + 1], buf[p + 2], buf[p + 3]])
}

/// Position of the first occurrence of `fourcc`.
fn find(buf: &[u8], fourcc: &[u8; 4]) -> usize {
    buf.windows(4)
        .position(|w| w == fourcc)
        .unwrap_or_else(|| panic!("box {fourcc:?} not found"))
}

/// The body bytes (after the 8-byte size+type header) of the first box of type `fourcc`.
fn box_body<'a>(buf: &'a [u8], fourcc: &[u8; 4]) -> &'a [u8] {
    let p = find(buf, fourcc);
    let size = be32(buf, p - 4) as usize;
    &buf[p + 4..p - 4 + size]
}

/// Count of occurrences of `fourcc` (windowed; payloads avoid these codes).
fn count(buf: &[u8], fourcc: &[u8; 4]) -> usize {
    buf.windows(4).filter(|w| *w == fourcc).count()
}

#[test]
fn meta_contains_the_required_child_boxes() {
    let f = write(&image(vec![av01_item(1, vec![1, 2, 3, 4])])).unwrap();
    for fourcc in [
        b"hdlr", b"pitm", b"iloc", b"iinf", b"infe", b"iprp", b"ipco", b"ipma",
    ] {
        assert!(count(&f, fourcc) >= 1, "missing box {fourcc:?}");
    }
    // The handler must be a picture handler; iref/grpl/idat are only written when populated.
    assert_eq!(&box_body(&f, b"hdlr")[8..12], b"pict");
    for absent in [b"iref", b"grpl", b"idat"] {
        assert_eq!(count(&f, absent), 0, "unexpected box {absent:?}");
    }
}

#[test]
fn top_level_layout_is_ftyp_then_meta_then_mdat() {
    let f = write(&image(vec![av01_item(1, vec![1, 2, 3, 4])])).unwrap();
    assert_eq!(&f[4..8], b"ftyp");
    assert!(find(&f, b"ftyp") < find(&f, b"meta"));
    assert!(find(&f, b"meta") < find(&f, b"mdat"));
}

#[test]
fn ftyp_body_is_exact() {
    let f = write(&image(vec![av01_item(1, vec![0xA5; 4])])).unwrap();
    // major `avif`, minor 0, compatible avif/mif1/miaf/MA1A.
    assert_eq!(
        box_body(&f, b"ftyp"),
        b"avif\x00\x00\x00\x00avifmif1miafMA1A"
    );
}

#[test]
fn colr_nclx_body_encodes_every_field() {
    // Distinct non-zero CICP code points and full_range = true so every byte (incl. the bit-7
    // full_range flag) is observable.
    let mut it = item(1, *b"av01", vec![0; 8]);
    it.properties = vec![Property {
        essential: false,
        kind: PropertyKind::Colour(ColourInformation::Nclx(NclxColr {
            colour_primaries: 2,
            transfer_characteristics: 3,
            matrix_coefficients: 5,
            full_range: true,
        })),
    }];
    let f = write(&image(vec![it])).unwrap();
    assert_eq!(
        box_body(&f, b"colr"),
        &[b'n', b'c', b'l', b'x', 0, 2, 0, 3, 0, 5, 0x80]
    );
}

#[test]
fn property_bodies_are_exact() {
    // One item carrying every newly-typed property; each body is pinned byte-for-byte against
    // the ISO/IEC 14496-12 / 23008-12 field layouts.
    let mut it = av01_item(1, vec![0; 4]);
    it.properties = vec![
        Property {
            essential: false,
            kind: PropertyKind::AuxiliaryType {
                aux_type: "urn:x".into(),
                aux_subtype: vec![0xAA],
            },
        },
        Property {
            essential: true,
            kind: PropertyKind::CleanAperture(CleanAperture {
                width_n: 1,
                width_d: 2,
                height_n: 3,
                height_d: 4,
                horiz_off_n: 5,
                horiz_off_d: 6,
                vert_off_n: 7,
                vert_off_d: 8,
            }),
        },
        Property {
            essential: false,
            kind: PropertyKind::PixelAspectRatio(PixelAspectRatio {
                h_spacing: 9,
                v_spacing: 10,
            }),
        },
        Property {
            essential: false,
            kind: PropertyKind::ContentLightLevel(ContentLightLevel {
                max_content_light_level: 0x1234,
                max_pic_average_light_level: 0x0056,
            }),
        },
        Property {
            essential: false,
            kind: PropertyKind::Colour(ColourInformation::RestrictedIcc(vec![0xDE, 0xAD])),
        },
    ];
    let f = write(&image(vec![it])).unwrap();
    // auxC: FullBox v0 + NUL-terminated URN + subtype bytes.
    assert_eq!(
        box_body(&f, b"auxC"),
        &[0, 0, 0, 0, b'u', b'r', b'n', b':', b'x', 0, 0xAA]
    );
    // clap: eight raw u32 fields, no FullBox header.
    let clap: Vec<u8> = (1u32..=8).flat_map(|v| v.to_be_bytes()).collect();
    assert_eq!(box_body(&f, b"clap"), clap);
    // pasp: two raw u32 fields.
    assert_eq!(box_body(&f, b"pasp"), &[0, 0, 0, 9, 0, 0, 0, 10]);
    // clli: two raw u16 fields.
    assert_eq!(box_body(&f, b"clli"), &[0x12, 0x34, 0x00, 0x56]);
    // colr with an ICC payload: colour_type then the profile verbatim.
    assert_eq!(box_body(&f, b"colr"), &[b'r', b'I', b'C', b'C', 0xDE, 0xAD]);
}

#[test]
fn infe_body_encodes_mime_fields_and_hidden_flag() {
    let mut xmp = item(2, *b"mime", vec![1]);
    xmp.name = "x".into();
    xmp.content_type = Some("a/b".into());
    xmp.content_encoding = Some("gz".into());
    xmp.hidden = true;
    let f = write(&image(vec![av01_item(1, vec![0; 4]), xmp])).unwrap();
    // The second infe is the mime item's.
    let p = find(&f, b"infe");
    let rest = &f[p + 4..];
    let body = box_body(rest, b"infe");
    assert_eq!(
        body,
        &[
            2, 0, 0, 1, // version 2, flags & 1 = hidden
            0, 2, // item_ID
            0, 0, // item_protection_index
            b'm', b'i', b'm', b'e', // item_type
            b'x', 0, // item_name
            b'a', b'/', b'b', 0, // content_type
            b'g', b'z', 0, // content_encoding
        ]
    );
}

#[test]
fn iref_body_is_exact() {
    let mut alpha = item(2, *b"av01", vec![2]);
    alpha.references = vec![ItemReference {
        reference_type: *b"auxl",
        to_item_ids: vec![1],
    }];
    let f = write(&image(vec![av01_item(1, vec![0; 4]), alpha])).unwrap();
    // iref: FullBox v0 wrapping one SingleItemTypeReferenceBox `auxl` (from 2, count 1, to 1).
    assert_eq!(
        box_body(&f, b"iref"),
        &[
            0, 0, 0, 0, 0, 0, 0, 14, b'a', b'u', b'x', b'l', 0, 2, 0, 1, 0, 1
        ]
    );
}

#[test]
fn grpl_body_is_exact() {
    let mut img = image(vec![av01_item(1, vec![0; 4]), av01_item(2, vec![1; 4])]);
    img.groups = vec![EntityGroup {
        group_type: *b"altr",
        group_id: 100,
        entity_ids: vec![1, 2],
    }];
    let f = write(&img).unwrap();
    // grpl wraps one EntityToGroupBox `altr` (FullBox v0): group_id 100, 2 entities, ids 1 and 2.
    assert_eq!(
        box_body(&f, b"grpl"),
        &[
            0, 0, 0, 28, b'a', b'l', b't', b'r', 0, 0, 0, 0, // child box header + FullBox v0
            0, 0, 0, 100, // group_id
            0, 0, 0, 2, // num_entities_in_group
            0, 0, 0, 1, 0, 0, 0, 2, // entity_ids
        ]
    );
}

#[test]
fn ipma_associates_base_properties_with_essential_av1c() {
    let f = write(&image(vec![av01_item(1, vec![0; 8])])).unwrap();
    let ipma = box_body(&f, b"ipma");
    assert_eq!(ipma[3], 0, "flags: single-byte association form");
    assert_eq!(ipma[10], 4, "association_count");
    // av1C essential (0x80 | 1), then ispe/pixi/colr non-essential.
    assert_eq!(&ipma[11..15], &[0x81, 2, 3, 4]);
}

#[test]
fn ipma_marks_transforms_essential() {
    let mut it = av01_item(1, vec![0; 8]);
    it.properties.push(Property {
        essential: true,
        kind: PropertyKind::Rotation(1),
    });
    it.properties.push(Property {
        essential: true,
        kind: PropertyKind::Mirror(1),
    });
    let f = write(&image(vec![it])).unwrap();
    let ipma = box_body(&f, b"ipma");
    assert_eq!(ipma[10], 6, "association_count");
    assert_eq!(&ipma[11..17], &[0x81, 2, 3, 4, 0x85, 0x86]);
}

#[test]
fn ipma_switches_to_16bit_entries_above_127_pool_slots() {
    // 128 distinct properties: index 128 no longer fits 7 bits, so flags&1 must select the
    // two-byte essential|index form for the whole box — while at exactly 127 the single-byte
    // form must still be used (the boundary is the normalisation contract, not a free choice).
    let props = |n: u8| -> Vec<Property> {
        (0..n)
            .map(|n| Property {
                essential: n == 0, // only the first association carries the essential bit
                kind: PropertyKind::Other {
                    kind: *b"unkn",
                    data: vec![n],
                },
            })
            .collect()
    };

    let mut narrow = item(1, *b"av01", vec![0; 4]);
    narrow.properties = props(127);
    let f = write(&image(vec![narrow])).unwrap();
    let ipma = box_body(&f, b"ipma");
    assert_eq!(
        ipma[3], 0,
        "flags: 127 slots still fit the single-byte form"
    );
    assert_eq!(&ipma[11..13], &[0x81, 2], "essential|index bytes");

    let mut wide = item(1, *b"av01", vec![0; 4]);
    wide.properties = props(128);
    let f = write(&image(vec![wide])).unwrap();
    let ipma = box_body(&f, b"ipma");
    assert_eq!(ipma[3], 1, "flags & 1: 16-bit association form");
    assert_eq!(ipma[10], 128, "association_count");
    assert_eq!(&ipma[11..15], &[0x80, 1, 0x00, 2], "essential|index words");
}

#[test]
fn iloc_extent_resolves_to_the_item_payload() {
    let payload = vec![1u8, 2, 3, 4, 5, 6, 7, 8, 9, 10];
    let f = write(&image(vec![av01_item(1, payload.clone())])).unwrap();
    // The reader resolves the iloc extent against the file; the recovered payload must be exact.
    assert_eq!(read(&f).unwrap().items[0].payload, payload);
}

#[test]
fn identical_properties_are_pooled_into_one_ipco_entry() {
    // Two items with identical ispe/pixi/colr/av1C must share one ipco entry each (dedup), not
    // duplicate them. A "never dedup" writer would emit two of each.
    let f = write(&image(vec![
        av01_item(1, vec![1, 1, 1]),
        av01_item(2, vec![2, 2, 2, 2]),
    ]))
    .unwrap();
    assert_eq!(count(&f, b"ispe"), 1, "ispe should be pooled");
    assert_eq!(count(&f, b"av1C"), 1, "av1C should be pooled");
    assert_eq!(count(&f, b"colr"), 1, "colr should be pooled");
    // ...but both items still round-trip with their full property lists.
    let parsed = read(&f).unwrap();
    assert_eq!(parsed.items.len(), 2);
    assert_eq!(parsed.items[0].properties.len(), 4);
    assert_eq!(parsed.items[1].properties.len(), 4);
}
