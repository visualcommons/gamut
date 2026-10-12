//! Byte-accounting totality for [`walk_segments`]: the segments tile `0..len` exactly, and their
//! kinds match the expected decomposition (boxes, appended stream, trailer).
//!
//! These pin the walk itself, not any container built on it. `gamut-avif` and `gamut-heic` each
//! carried a copy of this file when each carried a copy of the walk (#436); with one
//! implementation there is one place to pin it, and asserting against `walk_segments` directly
//! rather than through a container's `parse` keeps the reach to the walk — a defect in item
//! validation or in `read` cannot fail these.
//!
//! `walk_meta_children` is pinned in `unknown_meta.rs` beside this.

mod common;

use common::{bx, cat, ftyp, hdlr, iinf_v0, infe_v2, meta, pitm_v0};
use gamut_core::segment::covers;
use gamut_isobmff::{Segment, SegmentKind, walk_segments};

/// A minimal well-formed file: `ftyp` + `meta` + `mdat`.
///
/// Deliberately hand-built rather than reused from a container crate's fixtures: the walk cares
/// only that a top-level box is typed `ftyp` or `meta`, so a fixture carrying AVIF's `av1C` or
/// HEIC's `hvcC` would imply a dependence this walk does not have.
fn clean_file(payload: &[u8]) -> Vec<u8> {
    let m = meta(&[hdlr(), pitm_v0(1), iinf_v0(&[infe_v2(1, b"av01")])]);
    cat(&[ftyp(), m, bx(b"mdat", payload)])
}

/// Asserts the every-byte invariant through the shared predicate, [`covers`]: non-empty segments,
/// contiguous and non-overlapping, from 0 to `len`. The predicate's own negative cases (gap,
/// overlap, empty segment, short/long tail) are pinned once, beside it in `gamut-core` (#500).
fn assert_covers(segments: &[Segment<'_>], len: usize) {
    assert!(
        covers(segments, len),
        "segments must tile 0..{len}: {segments:?}"
    );
}

/// The top-level box types, in order (the `SegmentKind::Box` segments).
fn box_types(segments: &[Segment<'_>]) -> Vec<[u8; 4]> {
    segments
        .iter()
        .filter_map(|s| match &s.kind {
            SegmentKind::Box { ty, .. } => Some(*ty),
            _ => None,
        })
        .collect()
}

#[test]
fn clean_file_is_all_boxes() {
    let data = clean_file(&[0xAA, 0xBB, 0xCC, 0xDD]);
    let (segments, meta_body) = walk_segments(&data).unwrap();

    assert_covers(&segments, data.len());
    assert_eq!(box_types(&segments), vec![*b"ftyp", *b"meta", *b"mdat"]);
    assert!(
        segments
            .iter()
            .all(|s| matches!(s.kind, SegmentKind::Box { .. })),
        "every segment is a Box"
    );
    assert!(meta_body.is_some(), "the meta body is handed back");
}

#[test]
fn unknown_top_level_box_surfaces_verbatim() {
    // Google Motion Photo appends a top-level `mpvd` box (absent from MP4RA) after the image
    // boxes; Pixel phones emit motion-photo files with exactly this shape.
    let clean = clean_file(&[1, 2, 3, 4]);
    let mpvd_body = b"MP4-motion-photo-video-data".to_vec();
    let data = cat(&[clean, bx(b"mpvd", &mpvd_body)]);
    let (segments, _) = walk_segments(&data).unwrap();

    assert_covers(&segments, data.len());
    assert_eq!(
        box_types(&segments),
        vec![*b"ftyp", *b"meta", *b"mdat", *b"mpvd"]
    );
    // The unknown box is surfaced with its exact body — nothing dropped.
    let mpvd = segments
        .iter()
        .find_map(|s| match &s.kind {
            SegmentKind::Box { ty, body } if *ty == *b"mpvd" => Some(*body),
            _ => None,
        })
        .expect("mpvd surfaced");
    assert_eq!(mpvd, mpvd_body.as_slice());
    // It is a primary-stream box, not an appended stream or trailer.
    assert!(!segments.iter().any(|s| matches!(
        s.kind,
        SegmentKind::AppendedStream(_) | SegmentKind::Trailer(_)
    )));
}

#[test]
fn alternate_size_headers_account_for_every_header_byte() {
    // size == 1 means a 64-bit `largesize` follows the type; size == 0 means "to end of file".
    // Both put the body at a different offset from the ordinary 8-byte header, so the segment
    // ranges are what prove no header byte fell outside the accounting.
    let clean = clean_file(&[1, 2, 3, 4]);
    let mut large = vec![0, 0, 0, 1, b'f', b'r', b'e', b'e'];
    large.extend_from_slice(&19_u64.to_be_bytes());
    large.extend_from_slice(&[0xAA, 0xBB, 0xCC]);
    let open = vec![0, 0, 0, 0, b's', b'k', b'i', b'p', 0xDD, 0xEE];
    let data = cat(&[clean.clone(), large.clone(), open]);
    let (segments, _) = walk_segments(&data).unwrap();

    assert_covers(&segments, data.len());
    assert_eq!(
        box_types(&segments),
        vec![*b"ftyp", *b"meta", *b"mdat", *b"free", *b"skip"]
    );
    let body_of = |want: &[u8; 4]| {
        segments
            .iter()
            .find_map(|s| match &s.kind {
                SegmentKind::Box { ty, body } if ty == want => Some(*body),
                _ => None,
            })
            .unwrap()
    };
    assert_eq!(body_of(b"free"), &[0xAA, 0xBB, 0xCC]);
    assert_eq!(body_of(b"skip"), &[0xDD, 0xEE]);
    assert_eq!(segments[3].range, clean.len()..clean.len() + large.len());
}

#[test]
fn second_ftyp_starts_appended_stream_to_eof() {
    // Samsung motion photo: a second whole file (second top-level `ftyp` + its own stream), then a
    // proprietary trailer — all opaque, all one appended segment starting at the second ftyp.
    let clean = clean_file(&[9, 9, 9, 9]);
    let appendix = cat(&[ftyp(), b"\xDE\xAD\xBE\xEFtrailing-garbage".to_vec()]);
    let data = cat(&[clean.clone(), appendix.clone()]);
    let (segments, _) = walk_segments(&data).unwrap();

    assert_covers(&segments, data.len());
    // Primary stream is unchanged; the appendix is a single AppendedStream.
    assert_eq!(box_types(&segments), vec![*b"ftyp", *b"meta", *b"mdat"]);
    let appended = segments
        .iter()
        .find(|s| matches!(s.kind, SegmentKind::AppendedStream(_)))
        .expect("appended stream");
    assert_eq!(appended.range, clean.len()..data.len());
    match appended.kind {
        SegmentKind::AppendedStream(bytes) => assert_eq!(bytes, appendix.as_slice()),
        _ => unreachable!(),
    }
    assert!(
        !segments
            .iter()
            .any(|s| matches!(s.kind, SegmentKind::Trailer(_)))
    );
}

#[test]
fn trailing_garbage_without_ftyp_is_a_trailer() {
    // Non-box trailing bytes with no second ftyp: retained as an explicit Trailer.
    let clean = clean_file(&[5, 5, 5, 5]);
    let garbage = b"SamsungSEF\x00trailer-blob".to_vec();
    let data = cat(&[clean.clone(), garbage.clone()]);
    let (segments, _) = walk_segments(&data).unwrap();

    assert_covers(&segments, data.len());
    assert_eq!(box_types(&segments), vec![*b"ftyp", *b"meta", *b"mdat"]);
    let trailer = segments
        .iter()
        .find(|s| matches!(s.kind, SegmentKind::Trailer(_)))
        .expect("trailer");
    assert_eq!(trailer.range, clean.len()..data.len());
    match trailer.kind {
        SegmentKind::Trailer(bytes) => assert_eq!(bytes, garbage.as_slice()),
        _ => unreachable!(),
    }
    assert!(
        !segments
            .iter()
            .any(|s| matches!(s.kind, SegmentKind::AppendedStream(_)))
    );
}

#[test]
fn malformed_box_before_meta_is_an_error() {
    // The tolerance is conditional on having seen BOTH ftyp and meta. Before meta, the same
    // trailing bytes that would become a Trailer above are a hard error instead — which is the
    // rule that keeps this walk from disagreeing with `read` about where the stream ends.
    let data = cat(&[ftyp(), b"\x00\x00\x00\x40junk-oversized-box".to_vec()]);
    assert!(walk_segments(&data).is_err());
}

#[test]
fn malformed_box_before_ftyp_is_an_error() {
    // The other half of the same condition: meta alone does not license the tolerance either.
    let m = meta(&[hdlr(), pitm_v0(1), iinf_v0(&[infe_v2(1, b"av01")])]);
    let data = cat(&[m, b"\x00\x00\x00\x40junk-oversized-box".to_vec()]);
    assert!(walk_segments(&data).is_err());
}
