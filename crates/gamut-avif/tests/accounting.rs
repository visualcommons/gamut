//! AVIF container wiring: `parse` runs `gamut_isobmff::walk_segments` and surfaces its
//! results faithfully.
//!
//! The walk's rules are pinned in `gamut-isobmff`'s own `tests/accounting.rs` -- one
//! implementation, one place that pins it (#436). What stays here is what is specific to this
//! crate: that its container type is wired to the shared walk, plus the format-specific cases
//! below.

mod common;

use common::{av01_item, bx, clean_file};
use gamut_avif::{AvifContainer, SegmentKind};

/// Asserts the every-byte invariant through the shared predicate,
/// [`gamut_core::segment::covers`], whose own negative cases are pinned once in `gamut-core`
/// (#500).
fn assert_covers(container: &AvifContainer, len: usize) {
    let segs = container.segments();
    assert!(
        gamut_core::segment::covers(segs, len),
        "segments must tile 0..{len}: {segs:?}"
    );
}

/// The top-level box types, in order (the `SegmentKind::Box` segments).
fn box_types(container: &AvifContainer) -> Vec<[u8; 4]> {
    container.boxes().map(|(ty, _)| ty).collect()
}

/// `parse` runs the shared walk and exposes its results through every accessor.
///
/// The walk's own behaviour -- unknown boxes, alternate size headers, appended streams, trailers,
/// and the ftyp+meta tolerance rule -- is pinned once in `gamut-isobmff`'s `tests/accounting.rs`
/// since #436 made it one implementation. What is left for this crate to pin is that
/// `AvifContainer::parse` calls it and hands the results out unmodified, which is what a
/// mis-wiring would break.
#[test]
fn parse_exposes_the_shared_segment_walk() {
    let data = clean_file(1, vec![av01_item(1, vec![0xAA, 0xBB, 0xCC, 0xDD])]);
    let c = AvifContainer::parse(&data).unwrap();

    assert_covers(&c, data.len());
    assert_eq!(box_types(&c), vec![*b"ftyp", *b"meta", *b"mdat"]);
    assert!(c.appended_stream().is_none());
    assert!(c.trailer().is_none());
    // Every segment is a Box.
    assert!(
        c.segments()
            .iter()
            .all(|s| matches!(s.kind, SegmentKind::Box { .. }))
    );

    // `data()` returns the exact input buffer it was parsed from. Compare by contents *and*
    // pointer identity — it borrows `data`.
    assert_eq!(c.data(), data.as_slice());
    assert_eq!(c.data().as_ptr(), data.as_ptr());
    assert_eq!(c.data().len(), data.len());
}

/// The trailer accessor surfaces trailing bytes, not just the absence of them.
///
/// `parse_exposes_the_shared_segment_walk` asserts `trailer().is_none()` on a clean file, and that
/// was the whole of what the suite said about it -- so an accessor hard-wired to `None` satisfied
/// it, and both the `-> None` replacement and the deletion of its `Trailer` arm survived (#110).
/// Pinning the absent case cannot distinguish "there is none here" from "there is never any".
#[test]
fn trailer_surfaces_trailing_bytes() {
    // Three bytes cannot be a box header, which needs eight, so the walk ends the file with a
    // trailer segment rather than another box.
    let mut data = clean_file(1, vec![av01_item(1, vec![0xAA, 0xBB, 0xCC, 0xDD])]);
    let clean_len = data.len();
    data.extend_from_slice(&[0xDE, 0xAD, 0xBE]);

    let c = AvifContainer::parse(&data).unwrap();
    assert_eq!(c.trailer(), Some(&[0xDE, 0xAD, 0xBE][..]));
    assert_covers(&c, data.len());
    // It borrows the input rather than copying, and starts exactly where the boxes stop.
    assert_eq!(c.trailer().unwrap().as_ptr(), data[clean_len..].as_ptr());
}

/// The unknown-meta-box accessor surfaces what the semantic parse did not consume.
///
/// The same gap as `trailer`: the only assertion was `is_empty()` on the encoder's own output,
/// which an accessor returning a permanently empty slice satisfies.
#[test]
fn unknown_meta_boxes_surfaces_unconsumed_children() {
    let clean = clean_file(1, vec![av01_item(1, vec![0xAA, 0xBB, 0xCC, 0xDD])]);
    // `free` is a real ISOBMFF box that the AVIF semantic parse has no use for, so it comes back
    // out of `meta` verbatim.
    let data = splice_into_meta(&clean, &bx(b"free", &[0x01, 0x02, 0x03, 0x04]));

    let c = AvifContainer::parse(&data).unwrap();
    let unknown = c.unknown_meta_boxes();
    assert_eq!(unknown.len(), 1, "the free box is surfaced");
    assert_eq!(unknown[0].ty, *b"free");
    assert_eq!(unknown[0].body, &[0x01, 0x02, 0x03, 0x04]);
    assert_covers(&c, data.len());
}

/// Rebuilds `data` with `child` appended inside its top-level `meta` box, fixing the box size.
fn splice_into_meta(data: &[u8], child: &[u8]) -> Vec<u8> {
    let mut pos = 0usize;
    while pos + 8 <= data.len() {
        let size = u32::from_be_bytes(data[pos..pos + 4].try_into().unwrap()) as usize;
        assert!(size >= 8, "well-formed box");
        if &data[pos + 4..pos + 8] == b"meta" {
            let end = pos + size;
            let mut out = Vec::with_capacity(data.len() + child.len());
            out.extend_from_slice(&data[..pos]);
            out.extend_from_slice(&((size + child.len()) as u32).to_be_bytes());
            out.extend_from_slice(b"meta");
            out.extend_from_slice(&data[pos + 8..end]);
            out.extend_from_slice(child);
            out.extend_from_slice(&data[end..]);
            return out;
        }
        pos += size;
    }
    panic!("no meta box in fixture");
}

#[test]
fn encoder_output_accounts_totally() {
    // The crate's own encoder output decomposes into exactly ftyp + meta + mdat with no appended
    // stream or trailer — the two halves of the crate agree on the container shape.
    use gamut_core::{Dimensions, EncodeImage, ImageRef, Rgb8};
    let rgb = vec![200u8; 4 * 4 * 3];
    let image = ImageRef::<Rgb8>::new(
        &rgb,
        Dimensions {
            width: 4,
            height: 4,
        },
    )
    .unwrap();
    let mut data = Vec::new();
    gamut_avif::AvifEncoder::new()
        .encode_image(image, &mut data)
        .unwrap();
    let c = AvifContainer::parse(&data).unwrap();
    assert_covers(&c, data.len());
    assert_eq!(box_types(&c), vec![*b"ftyp", *b"meta", *b"mdat"]);
    assert!(c.unknown_meta_boxes().is_empty());
    assert!(c.image().is_av1_still());
}

/// The `iloc`-resolved payload length of `item_id` in a parsed file.
fn payload_len(c: &AvifContainer, item_id: u32) -> usize {
    c.image()
        .item(item_id)
        .expect("item present")
        .as_isobmff_item()
        .payload
        .len()
}

/// The body length of the file's single `mdat` — read off the segment walk, not `iloc`.
fn mdat_len(c: &AvifContainer) -> usize {
    let mut mdats = c.boxes().filter(|(ty, _)| ty == b"mdat");
    let (_, body) = mdats.next().expect("an mdat");
    assert!(mdats.next().is_none(), "exactly one mdat");
    body.len()
}

/// `encode_with_report` reports the colour and alpha codestream sizes a reader of the file finds:
/// the `iloc` extent lengths of the primary item and of its alpha auxiliary (issue #662).
///
/// The source pairs a textured colour image with a constant alpha plane, so the two codestreams
/// differ in length and a report that swapped them would fail. With no metadata configured the
/// `mdat` holds exactly those two payloads, which pins the sum against the segment walk as well as
/// against `iloc`.
#[test]
fn encode_with_report_sizes_are_the_colour_and_alpha_iloc_extents() {
    use gamut_core::{Dimensions, EncodeImage, ImageRef, Rgba8};
    const W: u32 = 24;
    const H: u32 = 16;
    let mut rgba = Vec::with_capacity((W * H * 4) as usize);
    for y in 0..H {
        for x in 0..W {
            rgba.extend_from_slice(&[(x * 11) as u8, (y * 13) as u8, ((x ^ y) * 7) as u8, 255]);
        }
    }
    let image = ImageRef::<Rgba8>::new(
        &rgba,
        Dimensions {
            width: W,
            height: H,
        },
    )
    .unwrap();
    // The fixture's alpha is opaque, which by default codes no alpha auxiliary at all; keep it, so
    // there is an alpha extent to report.
    let encoder = gamut_avif::AvifEncoder::new().with_keep_opaque_alpha(true);
    let (bytes, report) = encoder.encode_with_report(image).unwrap();
    assert_eq!(bytes, encoder.encode_to_vec(image).unwrap());

    let c = AvifContainer::parse(&bytes).unwrap();
    let primary = c.image().primary_item().id();
    let alpha = c.image().alpha_auxiliary_of(primary).expect("alpha").id();
    assert_eq!(report.color_bytes, payload_len(&c, primary));
    assert_eq!(report.alpha_bytes, payload_len(&c, alpha));
    assert_ne!(
        report.color_bytes, report.alpha_bytes,
        "fixture tells them apart"
    );
    assert_eq!(report.color_bytes + report.alpha_bytes, mdat_len(&c));
}

/// Without an alpha channel there is no alpha auxiliary, and `alpha_bytes` is `0`; the colour
/// item alone fills the `mdat`.
#[test]
fn encode_with_report_alpha_bytes_is_zero_without_alpha() {
    use gamut_core::{Dimensions, ImageRef, Rgb8};
    let rgb: Vec<u8> = (0..8 * 8 * 3).map(|i| (i * 5) as u8).collect();
    let image = ImageRef::<Rgb8>::new(
        &rgb,
        Dimensions {
            width: 8,
            height: 8,
        },
    )
    .unwrap();
    let (bytes, report) = gamut_avif::AvifEncoder::new()
        .encode_with_report(image)
        .unwrap();

    let c = AvifContainer::parse(&bytes).unwrap();
    let primary = c.image().primary_item().id();
    assert!(c.image().alpha_auxiliary_of(primary).is_none());
    assert_eq!(report.alpha_bytes, 0);
    assert_eq!(report.color_bytes, payload_len(&c, primary));
    assert_eq!(report.color_bytes, mdat_len(&c));
}
