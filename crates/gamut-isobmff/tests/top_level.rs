//! Top-level boxes the model does not otherwise own (`IsoBmffImage::top_level_boxes`, #443): the
//! writer's placement of each `TopLevelPosition` pinned exact-byte, the reader's position
//! assignment pinned against hand-authored spec bytes independent of the writer, the byte-identical
//! read→write round-trip of a file carrying such boxes, and the byte-accounting walk still tiling
//! the written file.
//!
//! The box under test throughout is C2PA 2.4's `ContentProvenanceBox` — a `uuid` box with the
//! §A.5.1 user type — because §A.5.3's placement rule (after `ftyp`, before the first `mdat`) is
//! what `TopLevelPosition::AfterFtyp` exists to satisfy; its payload is opaque to this crate.

mod common;

use common::{av01_item, bx, cat, ftyp, hdlr, iinf_v0, image, infe_v2, meta, pitm_v0};
use gamut_isobmff::{
    IsoBmffImage, SegmentKind, TopLevelBox, TopLevelPosition, read, walk_segments, write,
};

/// C2PA 2.4 §A.5.1: the `ContentProvenanceBox` user type `D8FEC3D6-1B0E-483C-9297-5828877EC481`.
const C2PA_UUID: [u8; 16] = [
    0xD8, 0xFE, 0xC3, 0xD6, 0x1B, 0x0E, 0x48, 0x3C, 0x92, 0x97, 0x58, 0x28, 0x87, 0x7E, 0xC4, 0x81,
];

/// Splits a file into its top-level `(type, body)` pairs by reading the 32-bit size fields — a
/// minimal walk deliberately independent of the crate's own `BoxReader`.
fn top_level(buf: &[u8]) -> Vec<([u8; 4], &[u8])> {
    let mut out = Vec::new();
    let mut p = 0;
    while p < buf.len() {
        let size = u32::from_be_bytes([buf[p], buf[p + 1], buf[p + 2], buf[p + 3]]) as usize;
        let ty = [buf[p + 4], buf[p + 5], buf[p + 6], buf[p + 7]];
        out.push((ty, &buf[p + 8..p + size]));
        p += size;
    }
    out
}

fn types(buf: &[u8]) -> Vec<[u8; 4]> {
    top_level(buf).into_iter().map(|(ty, _)| ty).collect()
}

/// A minimal still image carrying a C2PA `uuid` box after `ftyp` and a `free` box after `mdat`.
fn image_with_both_positions() -> IsoBmffImage {
    image(vec![av01_item(1, vec![1, 2, 3, 4])]).with_top_level_boxes(vec![
        TopLevelBox::uuid(C2PA_UUID, b"c2pa-manifest-store".to_vec()),
        TopLevelBox::new(*b"free", vec![0xAA, 0xBB]).with_position(TopLevelPosition::Trailing),
    ])
}

#[test]
fn c2pa_uuid_box_is_written_after_ftyp_and_before_meta() {
    let store = b"c2pa-manifest-store".to_vec();
    let img = image(vec![av01_item(1, vec![1, 2, 3, 4])])
        .with_top_level_boxes(vec![TopLevelBox::uuid(C2PA_UUID, store.clone())]);
    let f = write(&img).unwrap();

    // §A.5.3: after ftyp, before the first mdat (and here before meta as well).
    assert_eq!(types(&f), [*b"ftyp", *b"uuid", *b"meta", *b"mdat"]);
    // Exact bytes at the position right after ftyp: 32-bit size covering header + user type +
    // payload, the `uuid` type, the 16-byte user type, then the payload verbatim.
    let ftyp_len = 8 + top_level(&f)[0].1.len();
    let expected = cat(&[
        &(8 + 16 + store.len() as u32).to_be_bytes()[..],
        b"uuid",
        &C2PA_UUID,
        &store,
    ]);
    assert_eq!(&f[ftyp_len..ftyp_len + expected.len()], expected.as_slice());
}

#[test]
fn trailing_box_is_written_after_mdat() {
    let img = image(vec![av01_item(1, vec![1, 2, 3, 4])]).with_top_level_boxes(vec![
        TopLevelBox::new(*b"free", vec![0xAA, 0xBB]).with_position(TopLevelPosition::Trailing),
    ]);
    let f = write(&img).unwrap();

    assert_eq!(types(&f), [*b"ftyp", *b"meta", *b"mdat", *b"free"]);
    // The file ends with the exact box: size 10, `free`, the two payload bytes, no user type.
    assert_eq!(
        &f[f.len() - 10..],
        &[0, 0, 0, 10, b'f', b'r', b'e', b'e', 0xAA, 0xBB]
    );
}

#[test]
fn boxes_keep_model_order_within_each_position() {
    // Two boxes at each position, grouped as `write` requires (AfterFtyp then Trailing): the
    // writer keeps model order inside each group, and the groups land on either side of the
    // ftyp/meta/mdat spine.
    let img = image(vec![av01_item(1, vec![1, 2, 3, 4])]).with_top_level_boxes(vec![
        TopLevelBox::uuid(C2PA_UUID, vec![2]),
        TopLevelBox::new(*b"free", vec![4]),
        TopLevelBox::new(*b"skip", vec![1]).with_position(TopLevelPosition::Trailing),
        TopLevelBox::new(*b"free", vec![3]).with_position(TopLevelPosition::Trailing),
    ]);
    let f = write(&img).unwrap();

    let boxes = top_level(&f);
    let summary: Vec<([u8; 4], u8)> = boxes
        .iter()
        .map(|(ty, body)| (*ty, *body.last().unwrap()))
        .collect();
    assert_eq!(
        summary,
        [
            (*b"ftyp", b'A'),
            (*b"uuid", 2),
            (*b"free", 4),
            (*b"meta", 4),
            (*b"mdat", 4),
            (*b"skip", 1),
            (*b"free", 3),
        ]
    );
}

#[test]
fn read_then_write_is_byte_identical_for_a_file_carrying_top_level_boxes() {
    // What makes retaining the boxes safe rather than lossy: a file this crate wrote with boxes at
    // both positions re-serialises to the identical bytes after a read.
    let f = write(&image_with_both_positions()).unwrap();
    let parsed = read(&f).unwrap();
    assert_eq!(write(&parsed).unwrap(), f);
}

#[test]
fn reader_positions_boxes_by_whether_mdat_precedes_them() {
    // Hand-authored spec bytes, independent of the writer: ftyp, a C2PA uuid box, meta (itself
    // carrying a uuid child, which is *not* a top-level box), a skip box between meta and mdat,
    // mdat, then a trailing free box. Positions: before mdat → AfterFtyp (whether before or after
    // meta), after mdat → Trailing; mdat itself is never retained; the uuid user type is split
    // off the payload; the meta child is never promoted.
    let uuid_in_meta = bx(b"uuid", &cat(&[&[0u8; 16][..], b"inside-meta"]));
    let m = meta(&[
        hdlr(),
        pitm_v0(1),
        iinf_v0(&[infe_v2(1, b"av01")]),
        uuid_in_meta,
    ]);
    let data = cat(&[
        ftyp(),
        bx(b"uuid", &cat(&[&C2PA_UUID[..], b"store"])),
        m,
        bx(b"skip", b"between"),
        bx(b"mdat", &[9, 9, 9, 9]),
        bx(b"free", b"after"),
    ]);
    let img = read(&data).unwrap();

    assert_eq!(
        img.top_level_boxes,
        vec![
            TopLevelBox::uuid(C2PA_UUID, b"store".to_vec()),
            TopLevelBox::new(*b"skip", b"between".to_vec()),
            TopLevelBox::new(*b"free", b"after".to_vec()).with_position(TopLevelPosition::Trailing),
        ]
    );
}

#[test]
fn written_top_level_boxes_are_fully_accounted() {
    // Every byte of a file carrying top-level boxes still maps to a Box segment — the boxes are
    // well-formed spans, not a trailer, and nothing is left unclassified.
    let f = write(&image_with_both_positions()).unwrap();
    let (segments, _) = walk_segments(&f).unwrap();

    assert_eq!(segments[0].range.start, 0);
    for pair in segments.windows(2) {
        assert_eq!(pair[0].range.end, pair[1].range.start, "contiguous");
    }
    assert_eq!(segments.last().unwrap().range.end, f.len());
    let kinds: Vec<[u8; 4]> = segments
        .iter()
        .map(|s| match &s.kind {
            SegmentKind::Box { ty, .. } => *ty,
            other => panic!("non-box segment {other:?}"),
        })
        .collect();
    assert_eq!(kinds, [*b"ftyp", *b"uuid", *b"meta", *b"mdat", *b"free"]);
}

#[test]
fn push_top_level_box_appends_within_the_position_group() {
    // The #444 path: a parsed model already carries boxes at both positions; every push must land
    // at the END of its own position group — after the last AfterFtyp box (not at index 0), or
    // after the last Trailing box — so the list never interleaves. Boxes are told apart by their
    // one-byte payload. (That such a grouped list round-trips is pinned by
    // `boxes_keep_model_order_within_each_position` and `tests/roundtrip.rs`, not here.)
    let start = image(vec![av01_item(1, vec![1, 2, 3, 4])]).with_top_level_boxes(vec![
        TopLevelBox::uuid(C2PA_UUID, vec![1]),
        TopLevelBox::new(*b"free", vec![2]).with_position(TopLevelPosition::Trailing),
    ]);
    let mut model = read(&write(&start).unwrap()).unwrap();
    let order = |m: &IsoBmffImage| -> Vec<(u8, TopLevelPosition)> {
        m.top_level_boxes
            .iter()
            .map(|b| (b.payload[0], b.position))
            .collect()
    };
    let (a, t) = (TopLevelPosition::AfterFtyp, TopLevelPosition::Trailing);

    // [a, t] + a → [a, new, t]: after the last AfterFtyp box, not at index 0.
    model.push_top_level_box(TopLevelBox::new(*b"skip", vec![3]));
    assert_eq!(order(&model), [(1, a), (3, a), (2, t)]);
    // [a, a, t] + t → [a, a, t, new]: at the very end.
    model.push_top_level_box(TopLevelBox::new(*b"skip", vec![4]).with_position(t));
    assert_eq!(order(&model), [(1, a), (3, a), (2, t), (4, t)]);
    // [a, a, t, t] + a → [a, a, new, t, t].
    model.push_top_level_box(TopLevelBox::new(*b"free", vec![5]));
    assert_eq!(order(&model), [(1, a), (3, a), (5, a), (2, t), (4, t)]);
}
