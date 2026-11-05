//! Foreign-file conformance: spec-conformant fixtures hand-authored from ISO/IEC 14496-12 /
//! 23008-12 field layouts, *independent* of this crate's writer. They pin the reader's coverage
//! of what foreign encoders emit and this crate's writer never does: `iloc` v1/v2, `idat`
//! placement, multi-extent payloads, base offsets, 8-byte fields, 32-bit item ids (`pitm` v1,
//! `infe` v3, `iref` v1, `ipma` v1), and 16-bit `ipma` indices.

mod common;

use common::{bx, cat, ftyp, full, hdlr, iinf_v0, infe_v2, meta, pitm_v0};
use gamut_isobmff::{IsoBmffImage, PropertyKind};

/// An `ispe` property box.
fn ispe(width: u32, height: u32) -> Vec<u8> {
    full(
        b"ispe",
        0,
        0,
        &cat(&[width.to_be_bytes(), height.to_be_bytes()]),
    )
}

/// An `iprp` holding one `ispe` associated (non-essential) with `item_id`.
fn iprp_one_ispe_v0(item_id: u16) -> Vec<u8> {
    let ipco = bx(b"ipco", &ispe(64, 48));
    let ipma = full(
        b"ipma",
        0,
        0,
        &cat(&[
            &1u32.to_be_bytes()[..], // entry_count
            &item_id.to_be_bytes(),  // item_ID (16-bit in v0)
            &[1u8],                  // association_count
            &[0x01],                 // essential 0 | index 1
        ]),
    );
    bx(b"iprp", &cat(&[ipco, ipma]))
}

#[test]
fn iloc_v1_idat_multi_extent_with_base_offset() {
    // iloc v1: construction_method 1 (idat), base_offset_size 4, index_size 4 (the per-extent
    // extent_index precedes each offset and must be skipped), two extents concatenated.
    // idat body: ..XYZ..ABCD — base_offset 2, extents (0,3) and (5,4) → payload "XYZABCD".
    let iloc = full(
        b"iloc",
        1,
        0,
        &cat(&[
            &[0x44u8, 0x44][..], // offset_size 4 | length_size 4, base_offset_size 4 | index 4
            &1u16.to_be_bytes(), // item_count
            &1u16.to_be_bytes(), // item_ID
            &1u16.to_be_bytes(), // reserved(12) | construction_method(4) = 1 (idat)
            &0u16.to_be_bytes(), // data_reference_index
            &2u32.to_be_bytes(), // base_offset
            &2u16.to_be_bytes(), // extent_count
            &1u32.to_be_bytes(), // extent 0 extent_index
            &0u32.to_be_bytes(), // extent 0 offset
            &3u32.to_be_bytes(), // extent 0 length
            &2u32.to_be_bytes(), // extent 1 extent_index
            &5u32.to_be_bytes(), // extent 1 offset
            &4u32.to_be_bytes(), // extent 1 length
        ]),
    );
    let idat = bx(b"idat", b"..XYZ..ABCD");
    let m = meta(&[
        hdlr(),
        pitm_v0(1),
        iloc,
        iinf_v0(&[infe_v2(1, b"av01")]),
        idat,
        iprp_one_ispe_v0(1),
    ]);
    let img = IsoBmffImage::parse(&cat(&[ftyp(), m])).unwrap();

    assert_eq!(img.primary_item_id, 1);
    assert_eq!(img.items.len(), 1);
    assert_eq!(img.items[0].payload, b"XYZABCD");
    assert_eq!(
        img.items[0].properties[0].kind,
        PropertyKind::ImageSpatialExtents {
            width: 64,
            height: 48
        }
    );
}

#[test]
fn v2_boxes_with_32bit_ids_and_wide_ipma() {
    // The full 32-bit repertoire: pitm v1, infe v3, iloc v2 (u32 ids, 8-byte fields, idat),
    // iref v1, ipma v1 with 16-bit indices. Ids exceed u16 so the (normalising) writer must
    // reject the parsed model, while the reader carries it in full.
    let primary: u32 = 0x0001_0001;
    let alpha: u32 = 0x0001_0002;

    let infe_v3 = |id: u32, flags: u32| {
        full(
            b"infe",
            3,
            flags,
            &cat(&[&id.to_be_bytes()[..], &[0, 0], b"av01", &[0]]),
        )
    };
    let iinf_v1 = full(
        b"iinf",
        1,
        0,
        &cat(&[
            &2u32.to_be_bytes()[..],
            &infe_v3(primary, 0),
            &infe_v3(alpha, 1), // flags & 1: hidden
        ]),
    );
    let iloc_item = |id: u32, offset: u64, length: u64| {
        cat(&[
            &id.to_be_bytes()[..],
            &1u16.to_be_bytes(), // construction_method 1 (idat)
            &0u16.to_be_bytes(), // data_reference_index
            &1u16.to_be_bytes(), // extent_count
            &offset.to_be_bytes(),
            &length.to_be_bytes(),
        ])
    };
    let iloc_v2 = full(
        b"iloc",
        2,
        0,
        &cat(&[
            &[0x88u8, 0x00][..], // offset_size 8 | length_size 8, base_offset_size 0
            &2u32.to_be_bytes(), // item_count (32-bit in v2)
            &iloc_item(primary, 0, 4),
            &iloc_item(alpha, 4, 2),
        ]),
    );
    let idat = bx(b"idat", b"MAINab");
    let iref_v1 = full(
        b"iref",
        1,
        0,
        &bx(
            b"auxl",
            &cat(&[
                &alpha.to_be_bytes()[..], // from_item_ID (32-bit in v1)
                &1u16.to_be_bytes(),      // reference_count
                &primary.to_be_bytes(),
            ]),
        ),
    );
    let ipco = bx(b"ipco", &ispe(8, 8));
    let ipma_v1_wide = full(
        b"ipma",
        1,
        1, // flags & 1: 16-bit essential|index words
        &cat(&[
            &1u32.to_be_bytes()[..],
            &primary.to_be_bytes(),   // item_ID (32-bit in v1)
            &[1u8],                   // association_count
            &0x8001u16.to_be_bytes(), // essential 1 | index 1
        ]),
    );
    let m = meta(&[
        hdlr(),
        full(b"pitm", 1, 0, &primary.to_be_bytes()),
        iloc_v2,
        iinf_v1,
        idat,
        iref_v1,
        bx(b"iprp", &cat(&[ipco, ipma_v1_wide])),
    ]);
    let img = IsoBmffImage::parse(&cat(&[ftyp(), m])).unwrap();

    assert_eq!(img.primary_item_id, primary);
    assert_eq!(img.items[0].id, primary);
    assert_eq!(img.items[0].payload, b"MAIN");
    assert!(img.items[0].properties[0].essential);
    assert_eq!(
        img.items[0].properties[0].kind,
        PropertyKind::ImageSpatialExtents {
            width: 8,
            height: 8
        }
    );
    assert_eq!(img.items[1].id, alpha);
    assert_eq!(img.items[1].payload, b"ab");
    assert!(img.items[1].hidden);
    assert_eq!(img.items[1].references.len(), 1);
    assert_eq!(img.items[1].references[0].reference_type, *b"auxl");
    assert_eq!(img.items[1].references[0].to_item_ids, vec![primary]);

    // The parsed model exceeds the writer's normalised 16-bit id range.
    assert!(img.to_bytes().is_err());
}
