//! Role/relationship lenses: on a rich fixture (primary + alpha/depth auxiliaries + thumbnail +
//! Exif/XMP + a grid), every lens must return exactly the right items, and the derived-image
//! payload parsers must validate against the `dimg` reference count.

mod common;

use common::{auxc, av01_item, av1c, clean_file, iref, ispe, item};
use gamut_avif::{AvifContainer, AvifItem, ItemKind};
use gamut_isobmff::{
    EntityGroup, ImageGrid, ImageOverlay, IsoBmffImage, Item, OverlayOffset, Property, PropertyKind,
};

const ALPHA_URN: &str = "urn:mpeg:mpegB:cicp:systems:auxiliary:alpha";
const DEPTH_URN: &str = "urn:mpeg:mpegB:cicp:systems:auxiliary:depth";

/// The ids of a lens result, for order-exact assertions.
fn ids(items: Vec<AvifItem<'_>>) -> Vec<u32> {
    items.iter().map(AvifItem::id).collect()
}

/// An `av01` auxiliary (hidden) with the given aux-type URN and an `auxl` reference to `master`.
fn aux_item(id: u32, master: u32, aux_urn: &str) -> Item {
    item(id, *b"av01", vec![0xAA])
        .with_hidden(true)
        .with_properties(vec![
            av1c(common::AV1C_444_8BIT.to_vec()),
            ispe(64, 48),
            auxc(aux_urn),
        ])
        .with_references(vec![iref(b"auxl", &[master])])
}

/// The full fixture: primary(1) + alpha(2) + depth(3) + thumbnail(4) + Exif(5) + XMP(6) + grid(7)
/// over tiles 8..=11.
fn rich_file() -> Vec<u8> {
    // The premultiplied colour image is the source of the `prem` reference to its alpha (2).
    let primary = av01_item(1, vec![1, 2, 3, 4]).with_references(vec![iref(b"prem", &[2])]);
    let thumbnail = av01_item(4, vec![7, 7]).with_references(vec![iref(b"thmb", &[1])]);
    let exif = item(5, *b"Exif", b"\x00\x00\x00\x00MM\x00*".to_vec())
        .with_references(vec![iref(b"cdsc", &[1])]);
    let xmp = item(6, *b"mime", b"<x:xmpmeta/>".to_vec())
        .with_content_type("application/rdf+xml")
        .with_references(vec![iref(b"cdsc", &[1])]);
    let grid = item(
        7,
        *b"grid",
        ImageGrid {
            rows: 2,
            columns: 2,
            output_width: 128,
            output_height: 96,
        }
        .to_bytes()
        .unwrap(),
    )
    .with_references(vec![iref(b"dimg", &[8, 9, 10, 11])]);
    let tile = |id: u32| av01_item(id, vec![0xEE]).with_hidden(true);

    clean_file(
        1,
        vec![
            primary,
            aux_item(2, 1, ALPHA_URN),
            aux_item(3, 1, DEPTH_URN),
            thumbnail,
            exif,
            xmp,
            grid,
            tile(8),
            tile(9),
            tile(10),
            tile(11),
        ],
    )
}

#[test]
fn relationship_lenses_resolve_exact_items() {
    let data = rich_file();
    let c = AvifContainer::parse(&data).unwrap();
    let img = c.image();

    assert_eq!(ids(img.thumbnails_of(1)), vec![4]);
    assert_eq!(ids(img.auxiliaries_of(1)), vec![2, 3]);
    assert_eq!(img.alpha_auxiliary_of(1).map(|i| i.id()), Some(2));
    assert_eq!(img.depth_auxiliary_of(1).map(|i| i.id()), Some(3));
    assert!(img.is_premultiplied(1));
    assert!(!img.is_premultiplied(2));
    assert_eq!(ids(img.metadata_of(1)), vec![5, 6]);
    assert_eq!(img.exif().map(|i| i.id()), Some(5));
    assert_eq!(img.xmp().map(|i| i.id()), Some(6));
    assert_eq!(ids(img.derivation_sources(7)), vec![8, 9, 10, 11]);

    // The Exif payload is exposed raw, 4-byte tiff-header-offset prefix included — the caller
    // (e.g. rawshift) strips it.
    let exif = img.exif().unwrap();
    assert_eq!(
        exif.as_isobmff_item().payload,
        b"\x00\x00\x00\x00MM\x00*".to_vec()
    );
    // The Exif metadata item is not a coded image (pins `ItemKind::is_coded_image`).
    assert!(matches!(exif.kind(), ItemKind::Exif));
    assert!(!exif.kind().is_coded_image());
}

#[test]
fn brands_groups_and_alternatives_pin_exact_values() {
    // A file with an explicit major/compatible brand set and two entity groups — one `altr`, one
    // non-`altr` (`ster`) — pins `major_brand`/`compatible_brands`/`groups`/`alternatives`
    // against their body-replacement mutants, and the `== b"altr"` filter against its `!=`
    // inversion (which would return the `ster` group instead of the `altr` one).
    let model = IsoBmffImage::new(
        *b"avif",
        vec![*b"mif1", *b"avif"],
        1,
        vec![
            av01_item(1, vec![1, 2, 3, 4]),
            av01_item(2, vec![5, 6]).with_hidden(true),
        ],
    )
    .with_groups(vec![
        EntityGroup::new(*b"ster", 7, vec![1, 2]),
        EntityGroup::new(*b"altr", 8, vec![2, 1]),
    ]);
    let data = model.to_bytes().unwrap();
    let c = AvifContainer::parse(&data).unwrap();
    let img = c.image();

    assert_eq!(img.major_brand(), *b"avif");
    assert_eq!(img.compatible_brands(), &[*b"mif1", *b"avif"][..]);
    assert_eq!(img.groups(), model.groups.as_slice());

    let alts = img.alternatives();
    assert_eq!(alts.len(), 1);
    assert_eq!(alts[0].group_type, *b"altr");
    assert_eq!(alts[0].group_id, 8);
    assert_eq!(alts[0].entity_ids, vec![2, 1]);
}

#[test]
fn alpha_auxiliary_requires_the_alpha_urn() {
    // A master whose *only* auxiliary is a depth map must have no alpha auxiliary. This pins the
    // URN comparison against a `-> true` replacement, which would misreport the depth aux as
    // alpha.
    let master = av01_item(1, vec![1, 2, 3, 4]);
    let depth = aux_item(2, 1, DEPTH_URN);
    let data = clean_file(1, vec![master, depth]);
    let c = AvifContainer::parse(&data).unwrap();
    let img = c.image();

    assert_eq!(img.depth_auxiliary_of(1).map(|i| i.id()), Some(2));
    assert!(img.alpha_auxiliary_of(1).is_none());
}

#[test]
fn grid_lens_parses_payload_and_validates_tile_count() {
    let data = rich_file();
    let c = AvifContainer::parse(&data).unwrap();

    let grid = c.image().grid(7).unwrap();
    assert_eq!(
        grid,
        ImageGrid {
            rows: 2,
            columns: 2,
            output_width: 128,
            output_height: 96,
        }
    );
    assert!(matches!(c.image().item(7).unwrap().kind(), ItemKind::Grid));
}

#[test]
fn grid_lens_rejects_mismatched_tile_count() {
    // A 2x2 grid (4 tiles) whose `dimg` lists only 3 sources must fail — a decoder cannot
    // assemble it. The grid item is the primary (a grid is a displayable image).
    let grid = item(
        1,
        *b"grid",
        ImageGrid {
            rows: 2,
            columns: 2,
            output_width: 32,
            output_height: 32,
        }
        .to_bytes()
        .unwrap(),
    )
    .with_references(vec![iref(b"dimg", &[2, 3, 4])]); // 3 sources, not 4
    let tile = |id: u32| av01_item(id, vec![0xEE]).with_hidden(true);
    let data = clean_file(1, vec![grid, tile(2), tile(3), tile(4)]);
    let c = AvifContainer::parse(&data).unwrap();

    assert!(c.image().grid(1).is_err());
}

#[test]
fn overlay_lens_parses_payload_with_dimg_count() {
    // An `iovl` item composing the primary image twice; its payload holds one offset pair per
    // dimg.
    let overlay = ImageOverlay {
        canvas_fill_value: [0, 0, 0, 0xFFFF],
        output_width: 100,
        output_height: 80,
        offsets: vec![
            OverlayOffset {
                horizontal: 0,
                vertical: 0,
            },
            OverlayOffset {
                horizontal: 10,
                vertical: 20,
            },
        ],
    };
    let iovl = item(2, *b"iovl", overlay.to_bytes().unwrap())
        .with_references(vec![iref(b"dimg", &[1, 1])])
        .with_properties(vec![]);
    let data = clean_file(1, vec![av01_item(1, vec![1, 2, 3, 4]), iovl]);
    let c = AvifContainer::parse(&data).unwrap();

    let parsed = c.image().overlay(2).unwrap();
    assert_eq!(parsed, overlay);
    assert!(matches!(
        c.image().item(2).unwrap().kind(),
        ItemKind::Overlay
    ));
}

#[test]
fn brand_predicate_distinguishes_av1_and_non_av1() {
    // `avif` major brand → AV1 still.
    let data = rich_file();
    assert!(AvifContainer::parse(&data).unwrap().image().is_av1_still());

    // A generic `mif1` file whose primary carries `hvc1`/`hvcC` (a HEIC-shaped file) is not an
    // AV1 still, even though `read` accepts the container.
    let hevc_item = item(1, *b"hvc1", vec![1, 2, 3, 4]).with_properties(vec![Property::new(
        true,
        PropertyKind::CodecConfiguration {
            kind: *b"hvcC",
            data: vec![1, 2, 3],
        },
    )]);
    let mut model = IsoBmffImage::new(*b"mif1", vec![*b"mif1"], 1, vec![hevc_item]);
    let data = model.to_bytes().unwrap();
    assert!(!AvifContainer::parse(&data).unwrap().image().is_av1_still());

    // The same generic `mif1` container, but the primary is an `av01` item carrying `av1C` → AV1
    // still by the mif1 + av1C fallback.
    model.items = vec![av01_item(1, vec![1, 2, 3, 4])];
    let data = model.to_bytes().unwrap();
    assert!(AvifContainer::parse(&data).unwrap().image().is_av1_still());

    // The intra-only brand `avio` also marks an AV1 still.
    model.major_brand = *b"avio";
    model.items = vec![av01_item(1, vec![1, 2, 3, 4])];
    let data = model.to_bytes().unwrap();
    let c = AvifContainer::parse(&data).unwrap();
    assert_eq!(c.image().major_brand(), *b"avio");
    assert!(c.image().is_av1_still());
}
