//! `read(&write(&img)?) == img` across the shapes this crate is meant to model.
//!
//! Roundtrip is the crate's keystone correctness net (the analogue of `gamut-ifd`'s
//! read→write→read check): every supported box, property, reference, and group — plus the
//! property-dedup pooling and verbatim preservation of unrecognised boxes — must survive a
//! write→read cycle unchanged. Each test is one modelled scenario a future AVIF/HEIC encoder
//! actually produces.

mod common;

use common::{av01_item, image, item};
use gamut_isobmff::{
    ColourInformation, EntityGroup, IsoBmffImage, Item, ItemReference, NclxColr, Property,
    PropertyKind, TopLevelBox, TopLevelPosition, read, write,
};

#[track_caller]
fn assert_roundtrips(img: &IsoBmffImage) {
    assert_eq!(&read(&write(img).unwrap()).unwrap(), img);
}

#[test]
fn minimal_still() {
    assert_roundtrips(&image(vec![av01_item(
        1,
        vec![0xde, 0xad, 0xbe, 0xef, 1, 2, 3],
    )]));
}

#[test]
fn transform_properties() {
    // Every legal irot/imir value, incl. the boundary angles and a full_range=false colr.
    for (angle, axis) in [(1, 0), (3, 1), (0, 1)] {
        let mut item = av01_item(1, vec![9; 16]);
        item.properties[3] = Property {
            essential: false,
            kind: PropertyKind::Colour(ColourInformation::Nclx(NclxColr {
                colour_primaries: 9,
                transfer_characteristics: 16,
                matrix_coefficients: 9,
                full_range: false,
            })),
        };
        item.properties.push(Property {
            essential: true,
            kind: PropertyKind::Rotation(angle),
        });
        item.properties.push(Property {
            essential: true,
            kind: PropertyKind::Mirror(axis),
        });
        assert_roundtrips(&image(vec![item]));
    }
}

#[test]
fn alpha_auxiliary_item() {
    // The M3 shape: a hidden alpha aux item carrying auxC, referencing its master via auxl, with
    // premultiplication signalled via prem.
    let mut alpha = av01_item(2, vec![7; 24]);
    alpha.hidden = true;
    alpha.properties.push(Property {
        essential: false,
        kind: PropertyKind::AuxiliaryType {
            aux_type: "urn:mpeg:mpegB:cicp:systems:auxiliary:alpha".into(),
            aux_subtype: vec![],
        },
    });
    alpha.references = vec![
        ItemReference {
            reference_type: *b"auxl",
            to_item_ids: vec![1],
        },
        ItemReference {
            reference_type: *b"prem",
            to_item_ids: vec![1],
        },
    ];
    assert_roundtrips(&image(vec![av01_item(1, vec![1; 32]), alpha]));
}

#[test]
fn exif_and_xmp_metadata_items() {
    // The M4 shape: an Exif item (coded item type) and an XMP mime item (content type +
    // encoding), each describing the primary image via cdsc.
    let mut exif = item(2, *b"Exif", vec![0, 0, 0, 0, b'M', b'M']);
    exif.name = "exif".into();
    exif.references = vec![ItemReference {
        reference_type: *b"cdsc",
        to_item_ids: vec![1],
    }];
    let mut xmp = item(3, *b"mime", b"<x:xmpmeta/>".to_vec());
    xmp.content_type = Some("application/rdf+xml".into());
    xmp.content_encoding = Some("deflate".into());
    xmp.references = vec![ItemReference {
        reference_type: *b"cdsc",
        to_item_ids: vec![1],
    }];
    assert_roundtrips(&image(vec![av01_item(1, vec![5; 16]), exif, xmp]));
}

#[test]
fn icc_colour_profiles() {
    for icc in [
        ColourInformation::RestrictedIcc(vec![0x61, 0x63, 0x73, 0x70]),
        ColourInformation::UnrestrictedIcc(vec![1, 2, 3, 4, 5]),
    ] {
        let mut item = av01_item(1, vec![3; 9]);
        item.properties[3] = Property {
            essential: false,
            kind: PropertyKind::Colour(icc),
        };
        assert_roundtrips(&image(vec![item]));
    }
}

#[test]
fn hdr_and_aspect_properties() {
    let mut item = av01_item(1, vec![8; 12]);
    item.properties.push(Property {
        essential: false,
        kind: PropertyKind::ContentLightLevel {
            max_content_light_level: 1000,
            max_pic_average_light_level: 400,
        },
    });
    item.properties.push(Property {
        essential: true,
        kind: PropertyKind::CleanAperture {
            width_n: 40,
            width_d: 1,
            height_n: 30,
            height_d: 1,
            horiz_off_n: (-2i32) as u32, // raw two's-complement signed offset
            horiz_off_d: 2,
            vert_off_n: 1,
            vert_off_d: 2,
        },
    });
    item.properties.push(Property {
        essential: false,
        kind: PropertyKind::PixelAspectRatio {
            h_spacing: 4,
            v_spacing: 3,
        },
    });
    assert_roundtrips(&image(vec![item]));
}

#[test]
fn grid_derivation_with_hidden_tiles() {
    // The M5 shape: a derived grid item (payload = the opaque ImageGrid struct) referencing its
    // hidden tiles via dimg, with the grid as the (non-1) primary item.
    let tiles: Vec<Item> = (1..=4)
        .map(|id| {
            let mut tile = av01_item(id, vec![id as u8; 8]);
            tile.hidden = true;
            tile
        })
        .collect();
    let mut grid = item(10, *b"grid", vec![0, 0, 1, 1, 0, 64, 0, 64]);
    grid.references = vec![ItemReference {
        reference_type: *b"dimg",
        to_item_ids: vec![1, 2, 3, 4],
    }];
    grid.properties = vec![Property {
        essential: false,
        kind: PropertyKind::ImageSpatialExtents {
            width: 64,
            height: 64,
        },
    }];
    let mut items = vec![grid];
    items.extend(tiles);
    assert_roundtrips(&image(items));
}

#[test]
fn entity_groups() {
    let mut img = image(vec![av01_item(1, vec![1; 8]), av01_item(2, vec![2; 8])]);
    img.groups = vec![EntityGroup {
        group_type: *b"altr",
        group_id: 100,
        entity_ids: vec![1, 2],
    }];
    assert_roundtrips(&img);
}

#[test]
fn item_without_data_or_properties() {
    // A derived item may carry neither payload nor properties (e.g. `iden`); its iloc extent is
    // empty and its ipma row is absent.
    assert_roundtrips(&image(vec![
        av01_item(1, vec![4; 4]),
        item(2, *b"iden", vec![]),
    ]));
}

#[test]
fn wide_ipma_indices() {
    // More than 127 distinct pooled properties forces the 16-bit ipma association form.
    let mut it = av01_item(1, vec![6; 10]);
    for n in 0..130u8 {
        it.properties.push(Property {
            essential: n % 2 == 0,
            kind: PropertyKind::Other {
                kind: *b"unkn",
                data: vec![n],
            },
        });
    }
    assert_roundtrips(&image(vec![it]));
}

#[test]
fn two_items_sharing_a_property() {
    // Two items with identical properties exercise the ipco dedup → ipma re-expand path.
    assert_roundtrips(&image(vec![
        av01_item(1, vec![1, 2, 3]),
        av01_item(2, vec![4, 5, 6, 7]),
    ]));
}

#[test]
fn compatible_brand_order_is_preserved() {
    let mut img = image(vec![av01_item(1, vec![1, 2, 3, 4])]);
    img.compatible_brands = vec![*b"mif1", *b"avif", *b"MA1A", *b"miaf"];
    assert_roundtrips(&img);
}

#[test]
fn unknown_property_preserved_verbatim() {
    // An unrecognised property box (and an av1C with a non-empty configOBUs tail) must survive as
    // opaque bytes.
    let mut it = av01_item(1, vec![1; 10]);
    it.properties[0] = Property {
        essential: true,
        kind: PropertyKind::CodecConfiguration {
            kind: *b"av1C",
            data: vec![0x81, 0x05, 0x0c, 0x00, 0xaa, 0xbb, 0xcc],
        },
    };
    it.properties.push(Property {
        essential: false,
        kind: PropertyKind::Other {
            kind: *b"a1op",
            data: vec![0x01],
        },
    });
    assert_roundtrips(&image(vec![it]));
}

#[test]
fn top_level_boxes() {
    // A C2PA-shaped uuid box after ftyp and a trailing free box: type, user type, payload and
    // position all survive the cycle.
    let img = image(vec![av01_item(1, vec![1; 8])]).with_top_level_boxes(vec![
        TopLevelBox::uuid([0xD8; 16], b"manifest-store".to_vec()),
        TopLevelBox::new(*b"free", vec![0xAA, 0xBB]).with_position(TopLevelPosition::Trailing),
    ]);
    assert_roundtrips(&img);
}

#[test]
fn ftyp_minor_version() {
    // A non-zero minor version, set through the builder, survives the cycle.
    assert_roundtrips(&image(vec![av01_item(1, vec![2; 4])]).with_minor_version(0x0102_0304));
}
