//! Item-level views: kind classification, typed property accessors, the typed `av1C` lens,
//! transformative-property order and the MIAF-order helper, the unsupported-essential-property
//! flag, and primary-item validation.

mod common;

use common::{
    AV1C_444_8BIT, av01_item, av1c, cat, clean_file, ftyp, hdlr, iinf_v0, infe_v2, ispe, item,
    meta, pitm_v0,
};
use gamut_avif::{
    AvifContainer, CleanAperture, ContentLightLevel, ItemKind, MasteringDisplay, PixelAspectRatio,
    TransformativeProperty,
};
use gamut_core::ErrorKind;
use gamut_isobmff::{ColourInformation, Item, NclxColr, Property, PropertyKind};

fn prop(essential: bool, kind: PropertyKind) -> Property {
    Property { essential, kind }
}

#[test]
fn primary_item_must_exist() {
    // Hand-authored: `pitm` names item 99, but only item 1 exists. `read` accepts it (it does not
    // resolve the primary); the AVIF layer rejects it at parse.
    let m = meta(&[hdlr(), pitm_v0(99), iinf_v0(&[infe_v2(1, b"av01", false)])]);
    let data = cat(&[ftyp(b"avif"), m]);
    assert!(AvifContainer::parse(&data).is_err());
}

#[test]
fn primary_item_must_not_be_hidden() {
    // 23008-12: the primary item shall not be hidden.
    let hidden_primary = Item {
        hidden: true,
        ..av01_item(1, vec![1, 2, 3, 4])
    };
    let data = clean_file(1, vec![hidden_primary]);
    assert!(AvifContainer::parse(&data).is_err());
}

#[test]
fn item_kind_classification_and_av1_predicate() {
    let data = clean_file(
        1,
        vec![
            av01_item(1, vec![1, 2, 3, 4]),
            Item {
                hidden: true,
                properties: vec![prop(
                    true,
                    PropertyKind::CodecConfiguration {
                        kind: *b"hvcC",
                        data: vec![1, 2, 3],
                    },
                )],
                ..item(2, *b"hvc1", vec![5, 6])
            },
        ],
    );
    let c = AvifContainer::parse(&data).unwrap();
    let img = c.image();

    // av01: the AV1 coded image.
    let av01 = img.item(1).unwrap();
    assert_eq!(av01.kind(), ItemKind::CodedImage { codec: *b"av01" });
    assert!(av01.kind().is_coded_image());
    assert!(av01.kind().is_av1());

    // hvc1: a coded image a HEIF-family file may carry, but not AV1.
    let hvc1 = img.item(2).unwrap();
    assert_eq!(hvc1.kind(), ItemKind::CodedImage { codec: *b"hvc1" });
    assert!(hvc1.kind().is_coded_image());
    assert!(!hvc1.kind().is_av1());
}

#[test]
fn av1_config_lens_distinguishes_absent_foreign_and_malformed() {
    let data = clean_file(
        1,
        vec![
            av01_item(1, vec![1, 2, 3, 4]),
            // No codec configuration at all.
            Item {
                hidden: true,
                properties: vec![ispe(8, 8)],
                ..item(2, *b"av01", vec![5])
            },
            // A foreign (hvcC) configuration is not an av1C.
            Item {
                hidden: true,
                properties: vec![prop(
                    true,
                    PropertyKind::CodecConfiguration {
                        kind: *b"hvcC",
                        data: vec![1],
                    },
                )],
                ..item(3, *b"hvc1", vec![6])
            },
            // A malformed av1C (marker bit clear).
            Item {
                hidden: true,
                properties: vec![av1c(vec![0x01, 0x20, 0x00, 0x00])],
                ..item(4, *b"av01", vec![7])
            },
        ],
    );
    let c = AvifContainer::parse(&data).unwrap();
    let img = c.image();

    let valid = img.item(1).unwrap().av1_config().expect("present").unwrap();
    assert_eq!(valid.seq_profile, 1);
    assert_eq!(valid.bit_depth(), 8);
    assert!(img.item(2).unwrap().av1_config().is_none());
    assert!(img.item(3).unwrap().av1_config().is_none());
    assert!(matches!(
        img.item(4).unwrap().av1_config(),
        Some(Err(error)) if error.kind() == ErrorKind::InvalidInput
    ));

    // The raw record stays reachable opaquely alongside the typed lens.
    let (kind, body) = img.item(1).unwrap().codec_configuration().unwrap();
    assert_eq!(kind, b"av1C");
    assert_eq!(body, &AV1C_444_8BIT[..]);
}

#[test]
fn typed_property_accessors() {
    let full_item = Item {
        properties: vec![
            av1c(AV1C_444_8BIT.to_vec()),
            ispe(100, 200),
            prop(
                false,
                PropertyKind::PixelInformation {
                    bits_per_channel: vec![10, 10, 10],
                },
            ),
            prop(
                false,
                PropertyKind::Colour(ColourInformation::Nclx(NclxColr {
                    colour_primaries: 9,
                    transfer_characteristics: 16,
                    matrix_coefficients: 9,
                    full_range: true,
                })),
            ),
            prop(
                false,
                PropertyKind::ContentLightLevel {
                    max_content_light_level: 1000,
                    max_pic_average_light_level: 400,
                },
            ),
            prop(
                false,
                PropertyKind::PixelAspectRatio {
                    h_spacing: 1,
                    v_spacing: 1,
                },
            ),
            prop(
                false,
                PropertyKind::CleanAperture {
                    width_n: 90,
                    width_d: 1,
                    height_n: 180,
                    height_d: 1,
                    horiz_off_n: 5,
                    horiz_off_d: 1,
                    vert_off_n: 6,
                    vert_off_d: 1,
                },
            ),
        ],
        ..item(1, *b"av01", vec![1, 2, 3, 4])
    };
    let data = clean_file(1, vec![full_item]);
    let c = AvifContainer::parse(&data).unwrap();
    let it = c.image().primary_item();

    assert_eq!(it.dimensions().unwrap().width, 100);
    assert_eq!(it.dimensions().unwrap().height, 200);
    assert_eq!(it.bits_per_channel(), Some(&[10u8, 10, 10][..]));
    assert!(matches!(
        it.colour(),
        Some(ColourInformation::Nclx(NclxColr {
            transfer_characteristics: 16,
            ..
        }))
    ));
    assert_eq!(
        it.content_light_level(),
        Some(ContentLightLevel {
            max_content_light_level: 1000,
            max_pic_average_light_level: 400,
        })
    );
    assert_eq!(
        it.pixel_aspect_ratio(),
        Some(PixelAspectRatio {
            h_spacing: 1,
            v_spacing: 1,
        })
    );
    assert_eq!(
        it.clean_aperture(),
        Some(CleanAperture {
            width_n: 90,
            width_d: 1,
            height_n: 180,
            height_d: 1,
            horiz_off_n: 5,
            horiz_off_d: 1,
            vert_off_n: 6,
            vert_off_d: 1,
        })
    );
}

#[test]
fn mastering_display_types_the_mdcv_property() {
    // Every field distinct, so a transposed primary pair or swapped luminance bound cannot
    // reproduce the expected value. The `clli` ahead of it checks the lens skips other HDR
    // properties rather than taking the first one.
    let mdcv = MasteringDisplay {
        display_primaries: [[13250, 34500], [7500, 3000], [34000, 16000]],
        white_point: [15635, 16450],
        max_display_mastering_luminance: 10_000_000,
        min_display_mastering_luminance: 50,
    };
    let with_mdcv = Item {
        properties: vec![
            av1c(AV1C_444_8BIT.to_vec()),
            prop(
                false,
                PropertyKind::ContentLightLevel {
                    max_content_light_level: 1000,
                    max_pic_average_light_level: 400,
                },
            ),
            prop(
                false,
                PropertyKind::MasteringDisplayColourVolume {
                    display_primaries: mdcv.display_primaries,
                    white_point: mdcv.white_point,
                    max_display_mastering_luminance: mdcv.max_display_mastering_luminance,
                    min_display_mastering_luminance: mdcv.min_display_mastering_luminance,
                },
            ),
        ],
        ..item(1, *b"av01", vec![1, 2, 3, 4])
    };
    let without = Item {
        properties: vec![av1c(AV1C_444_8BIT.to_vec())],
        ..item(2, *b"av01", vec![5, 6, 7, 8])
    };
    let data = clean_file(1, vec![with_mdcv, without]);
    let c = AvifContainer::parse(&data).unwrap();
    let img = c.image();

    assert_eq!(img.item(1).unwrap().mastering_display(), Some(mdcv));
    assert_eq!(img.item(2).unwrap().mastering_display(), None);
}

#[test]
fn transformative_properties_preserve_association_order() {
    // Item 1: irot then clap (out of MIAF order). Item 2: clap, irot, imir (MIAF order).
    let clap = PropertyKind::CleanAperture {
        width_n: 1,
        width_d: 1,
        height_n: 1,
        height_d: 1,
        horiz_off_n: 0,
        horiz_off_d: 1,
        vert_off_n: 0,
        vert_off_d: 1,
    };
    let out_of_order = Item {
        properties: vec![
            prop(true, PropertyKind::Rotation(1)),
            prop(true, clap.clone()),
        ],
        ..item(1, *b"av01", vec![1])
    };
    let miaf_ordered = Item {
        hidden: true,
        properties: vec![
            prop(true, clap),
            prop(true, PropertyKind::Rotation(2)),
            prop(true, PropertyKind::Mirror(1)),
        ],
        ..item(2, *b"av01", vec![2])
    };
    let data = clean_file(1, vec![out_of_order, miaf_ordered]);
    let c = AvifContainer::parse(&data).unwrap();
    let img = c.image();

    let a = img.item(1).unwrap();
    assert!(matches!(
        a.transformative_properties().as_slice(),
        [
            TransformativeProperty::Rotation(1),
            TransformativeProperty::CleanAperture(_)
        ]
    ));
    assert!(!a.is_miaf_transform_ordered());

    let b = img.item(2).unwrap();
    assert!(matches!(
        b.transformative_properties().as_slice(),
        [
            TransformativeProperty::CleanAperture(_),
            TransformativeProperty::Rotation(2),
            TransformativeProperty::Mirror(1)
        ]
    ));
    assert!(b.is_miaf_transform_ordered());
}

#[test]
fn unsupported_essential_property_flag() {
    let unsupported = Item {
        properties: vec![
            av1c(AV1C_444_8BIT.to_vec()),
            prop(
                true, // essential + unrecognised → a conforming reader must not render it
                PropertyKind::Other {
                    kind: *b"a1op", // the layered-image operating-point property (not yet typed)
                    data: vec![9],
                },
            ),
        ],
        ..item(1, *b"av01", vec![1])
    };
    let supported = Item {
        hidden: true,
        properties: vec![
            // essential codec config — decode layer's concern, not counted
            av1c(AV1C_444_8BIT.to_vec()),
            prop(
                true, // essential but recognised
                PropertyKind::ImageSpatialExtents {
                    width: 64,
                    height: 48,
                },
            ),
            prop(
                false, // unrecognised but non-essential → renderable
                PropertyKind::Other {
                    kind: *b"zzzz",
                    data: vec![9, 9],
                },
            ),
        ],
        ..item(2, *b"av01", vec![2])
    };
    let data = clean_file(1, vec![unsupported, supported]);
    let c = AvifContainer::parse(&data).unwrap();
    let img = c.image();

    assert!(img.item(1).unwrap().has_unsupported_essential_property());
    assert!(!img.item(2).unwrap().has_unsupported_essential_property());
}

#[test]
fn rotation_and_mirror_accessors_pin_exact_values() {
    // Item 1 carries irot=3 and imir=1; item 2 carries imir=0; item 3 carries neither. The exact
    // values (not `is_some`) pin the accessors against the `None`/`Some(0)`/`Some(1)` and
    // match-arm-deletion mutants: irot(3) differs from every replacement, and the two mirror axes
    // together rule out both `Some(0)` and `Some(1)`.
    let with_both = Item {
        properties: vec![
            av1c(AV1C_444_8BIT.to_vec()),
            prop(true, PropertyKind::Rotation(3)),
            prop(true, PropertyKind::Mirror(1)),
        ],
        ..item(1, *b"av01", vec![1])
    };
    let mirror_axis0 = Item {
        hidden: true,
        properties: vec![
            av1c(AV1C_444_8BIT.to_vec()),
            prop(true, PropertyKind::Mirror(0)),
        ],
        ..item(2, *b"av01", vec![2])
    };
    let neither = Item {
        hidden: true,
        properties: vec![av1c(AV1C_444_8BIT.to_vec())],
        ..item(3, *b"av01", vec![3])
    };
    let data = clean_file(1, vec![with_both, mirror_axis0, neither]);
    let c = AvifContainer::parse(&data).unwrap();
    let img = c.image();

    assert_eq!(img.item(1).unwrap().rotation(), Some(3));
    assert_eq!(img.item(1).unwrap().mirror(), Some(1));
    assert_eq!(img.item(2).unwrap().mirror(), Some(0));
    assert_eq!(img.item(2).unwrap().rotation(), None);
    assert_eq!(img.item(3).unwrap().rotation(), None);
    assert_eq!(img.item(3).unwrap().mirror(), None);
}

#[test]
fn unknown_item_type_is_not_classified_as_coded() {
    // An unrecognised four-cc must classify as `Unknown`, not `CodedImage`. This pins the `kind`
    // match guard (`is_coded_image_type(ty)`) and the helper against their `-> true`
    // replacements, which would misclassify it as a coded image.
    let unknown = Item {
        hidden: true,
        properties: vec![av1c(AV1C_444_8BIT.to_vec())],
        ..item(2, *b"zzzz", vec![9])
    };
    let data = clean_file(1, vec![av01_item(1, vec![1, 2, 3, 4]), unknown]);
    let c = AvifContainer::parse(&data).unwrap();
    let it = c.image().item(2).unwrap();
    assert_eq!(it.kind(), ItemKind::Unknown(*b"zzzz"));
    assert!(!it.kind().is_coded_image());
    // The coded classifier remains true for a real coded item (the positive direction).
    assert!(c.image().item(1).unwrap().kind().is_coded_image());
}
