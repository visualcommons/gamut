//! The validating-write contract: a model that cannot round-trip or does not fit the normalised
//! still-image box versions is rejected with a typed error, never silently corrupted.

mod common;

use common::{av01_item, image, item};
use gamut_isobmff::{
    IsoBmffImage, ItemReference, Property, PropertyKind, TopLevelBox, TopLevelPosition, write,
};

#[track_caller]
fn assert_rejected(img: &IsoBmffImage, expected: &str) {
    let e = write(img).unwrap_err().to_string();
    assert!(e.contains(expected), "expected {expected:?} in {e:?}");
}

#[test]
fn primary_item_id_must_name_an_item() {
    let mut img = image(vec![av01_item(1, vec![1])]);
    img.primary_item_id = 9;
    assert_rejected(&img, "primary_item_id names no item");
}

#[test]
fn duplicate_item_ids_are_rejected() {
    assert_rejected(
        &image(vec![av01_item(1, vec![1]), av01_item(1, vec![2])]),
        "duplicate item id",
    );
}

#[test]
fn item_ids_above_u16_are_rejected() {
    // The writer normalises to the 16-bit box versions; the reader still accepts 32-bit ids.
    assert_rejected(&image(vec![av01_item(0x1_0000, vec![1])]), "item id above");
    let mut it = av01_item(1, vec![1]);
    it.references = vec![ItemReference {
        reference_type: *b"auxl",
        to_item_ids: vec![0x1_0000],
    }];
    assert_rejected(&image(vec![it]), "referenced item id above");
}

#[test]
fn mime_fields_must_match_the_item_type() {
    let mut missing = item(1, *b"mime", vec![1]);
    missing.content_type = None;
    assert_rejected(&image(vec![missing]), "content_type is required");

    let mut spurious = av01_item(1, vec![1]);
    spurious.content_type = Some("text/plain".into());
    assert_rejected(&image(vec![spurious]), "content_type is required");

    let mut encoding_only = av01_item(1, vec![1]);
    encoding_only.content_encoding = Some("deflate".into());
    assert_rejected(&image(vec![encoding_only]), "requires a content_type");

    let mut empty_encoding = item(1, *b"mime", vec![1]);
    empty_encoding.content_type = Some("application/rdf+xml".into());
    empty_encoding.content_encoding = Some(String::new());
    assert_rejected(&image(vec![empty_encoding]), "empty content_encoding");
}

#[test]
fn interior_nul_does_not_silently_truncate() {
    let mut it = av01_item(1, vec![1]);
    it.name = "bad\0name".into();
    assert_rejected(&image(vec![it]), "interior NUL");

    let mut aux = av01_item(1, vec![1]);
    aux.properties.push(Property {
        essential: false,
        kind: PropertyKind::AuxiliaryType {
            aux_type: "urn\0:alpha".into(),
            aux_subtype: vec![],
        },
    });
    assert_rejected(&image(vec![aux]), "interior NUL in auxC");
}

#[test]
fn uri_items_are_unsupported() {
    assert_rejected(&image(vec![item(1, *b"uri ", vec![1])]), "uri items");
}

#[test]
fn out_of_range_transform_values_are_rejected() {
    let mut rot = av01_item(1, vec![1]);
    rot.properties.push(Property {
        essential: true,
        kind: PropertyKind::Rotation(4),
    });
    assert_rejected(&image(vec![rot]), "irot angle");

    let mut mir = av01_item(1, vec![1]);
    mir.properties.push(Property {
        essential: true,
        kind: PropertyKind::Mirror(2),
    });
    assert_rejected(&image(vec![mir]), "imir axis");
}

#[test]
fn more_than_255_properties_on_one_item_are_rejected() {
    let mut it = av01_item(1, vec![1]);
    it.properties = (0..=255u16)
        .map(|n| Property {
            essential: false,
            kind: PropertyKind::Other {
                kind: *b"unkn",
                data: n.to_be_bytes().to_vec(),
            },
        })
        .collect();
    assert_rejected(&image(vec![it]), "more than 255 properties");
}

#[test]
fn top_level_box_types_the_model_emits_itself_are_rejected() {
    // The writer emits ftyp/meta/mdat from the model; a second one in `top_level_boxes` would not
    // round-trip (a second ftyp even starts a motion-photo appendix on read).
    for ty in [b"ftyp", b"meta", b"mdat"] {
        let img = image(vec![av01_item(1, vec![1])])
            .with_top_level_boxes(vec![TopLevelBox::new(*ty, vec![])]);
        assert_rejected(&img, "owned by the model");
    }
    // Image sequences are Unsupported on read, so writing one is too.
    for ty in [b"moov", b"trak"] {
        let img = image(vec![av01_item(1, vec![1])])
            .with_top_level_boxes(vec![TopLevelBox::new(*ty, vec![])]);
        assert_rejected(&img, "image sequences");
    }
}

#[test]
fn top_level_user_type_must_pair_with_the_uuid_type() {
    // A uuid box carries its 16-byte user type; no other box does (the RawBox split).
    let img = image(vec![av01_item(1, vec![1])])
        .with_top_level_boxes(vec![TopLevelBox::new(*b"uuid", vec![1])]);
    assert_rejected(&img, "user_type is required for uuid boxes");

    let mut mistyped = TopLevelBox::new(*b"free", vec![1]);
    mistyped.user_type = Some([7; 16]);
    let img = image(vec![av01_item(1, vec![1])]).with_top_level_boxes(vec![mistyped]);
    assert_rejected(&img, "user_type is required for uuid boxes");
}

#[test]
fn interleaved_top_level_positions_are_rejected() {
    // The file holds every AfterFtyp box before every Trailing one, so a model listing a Trailing
    // box first would come back from `read` re-grouped: `write` refuses it instead of reordering.
    let img = image(vec![av01_item(1, vec![1])]).with_top_level_boxes(vec![
        TopLevelBox::new(*b"free", vec![1]).with_position(TopLevelPosition::Trailing),
        TopLevelBox::uuid([0xD8; 16], vec![2]),
    ]);
    assert_rejected(&img, "interleave positions");
}
