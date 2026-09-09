//! The C2PA manifest-store surface (C2PA 2.4 §A.5): the reserve → report → patch contract of
//! `AvifEncoder::with_c2pa_reserved` / `with_c2pa` / `encode_with_report` pinned exact-byte, the
//! `AvifContainer::c2pa` locator reading back what the encoder wrote and a mid-update pair, and —
//! against the crate's real readers — that a file carrying the box decodes unchanged.
//!
//! The exact-byte test is the load-bearing one for the epic's encoder criterion: the slot's
//! offset is reported *before* a signer runs, so nothing after placement may move a byte. Two
//! equal-length payloads written into the slot must yield two files that differ in exactly that
//! span and nowhere else, and patching a reserved file at the reported range must reproduce the
//! file the encoder writes for that store outright.
//!
//! The oracle half is hermetic: libavif (dav1d backend) is linked from the `third_party/libavif` +
//! `third_party/dav1d` submodules via the `libavif-oracle` / `dav1d-oracle` dev-dependencies, so
//! building it needs cmake/meson/ninja/nasm and the checked-out submodules (`git submodule update
//! --init --recursive` plus `mise run fetch-av1-oracles`). Neither reader knows C2PA; both must
//! read a file carrying the box as if it were not there.

mod common;

use common::av01_item;
use gamut_avif::{
    Av1Config, Av1StillDecoder, AvifContainer, AvifEncoder, C2PA_UUID, C2paBoxPurpose, DecodedFrame,
};
use gamut_core::{Dimensions, EncodeImage, Error, ImageRef, Result, Rgb8};
use gamut_isobmff::{IsoBmffImage, TopLevelBox, TopLevelPosition, write};

const W: u32 = 34;
const H: u32 = 18;

/// A structured source, identical across every case so a difference in the output can only come
/// from the C2PA knobs.
fn source_rgb() -> Vec<u8> {
    let mut rgb = vec![0u8; (W * H * 3) as usize];
    for y in 0..H {
        for x in 0..W {
            let i = ((y * W + x) * 3) as usize;
            rgb[i] = ((x * 7 + y * 3) & 0xff) as u8;
            rgb[i + 1] = ((x * x + y) & 0xff) as u8;
            rgb[i + 2] = ((x ^ (y * 5)) & 0xff) as u8;
        }
    }
    rgb
}

fn image(rgb: &[u8]) -> ImageRef<'_, Rgb8> {
    ImageRef::<Rgb8>::new(
        rgb,
        Dimensions {
            width: W,
            height: H,
        },
    )
    .expect("buffer matches dimensions")
}

/// A deterministic payload standing in for a manifest store; every byte differs from
/// `payload(seed ^ 0xff, len)`, so "differs exactly here" is checkable byte by byte.
fn payload(seed: u8, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| seed.wrapping_add((i * 31) as u8))
        .collect()
}

/// [`Av1StillDecoder`] over the real dav1d decoder, bridged the way a platform decoder would be.
struct Dav1dDecoder;

impl Av1StillDecoder for Dav1dDecoder {
    fn decode_still(&mut self, config: &Av1Config, payload: &[u8]) -> Result<DecodedFrame> {
        let mut stream = Vec::new();
        config.full_stream(payload, &mut stream)?;
        let pic = dav1d_oracle::decode_obu(&stream)
            .map_err(|_| Error::InvalidInput("c2pa: dav1d rejected the stream"))?;
        let [y, u, v] = pic.planes;
        DecodedFrame::new(
            pic.width,
            pic.height,
            pic.bit_depth,
            config.chroma_format(),
            y,
            u,
            v,
        )
    }
}

#[test]
fn two_equal_length_stores_give_files_that_differ_exactly_in_the_reported_slot() {
    const LEN: usize = 256;
    let rgb = source_rgb();
    let a = payload(0x11, LEN);
    let b = payload(0x11 ^ 0xff, LEN);
    assert!(a.iter().zip(&b).all(|(x, y)| x != y), "every byte differs");

    let (file_a, report_a) = AvifEncoder::new()
        .with_c2pa(&a)
        .encode_with_report(image(&rgb))
        .expect("encode a");
    let (file_b, report_b) = AvifEncoder::new()
        .with_c2pa(&b)
        .encode_with_report(image(&rgb))
        .expect("encode b");

    let range = report_a.c2pa.clone().expect("a slot was written");
    assert_eq!(report_b.c2pa, Some(range.clone()), "same offset for both");
    assert_eq!(range.len(), LEN);
    assert_eq!(file_a.len(), file_b.len());
    assert_eq!(&file_a[range.clone()], &a[..], "a's slot holds a");
    assert_eq!(&file_b[range.clone()], &b[..], "b's slot holds b");
    // Outside the slot the files are byte-identical: nothing moved, nothing else changed.
    assert_eq!(file_a[..range.start], file_b[..range.start]);
    assert_eq!(file_a[range.end..], file_b[range.end..]);
}

#[test]
fn patching_a_reserved_slot_at_the_reported_range_reproduces_the_written_store_file() {
    const LEN: usize = 256;
    let rgb = source_rgb();
    let store = payload(0x22, LEN);

    let (mut reserved, report) = AvifEncoder::new()
        .with_c2pa_reserved(LEN)
        .encode_with_report(image(&rgb))
        .expect("encode reserved");
    let range = report.c2pa.expect("a slot was reserved");
    assert_eq!(range.len(), LEN);
    assert!(
        reserved[range.clone()].iter().all(|&b| b == 0),
        "the reserved slot is zero bytes"
    );

    // The signer's move: overwrite the slot in place. The result must be the file the encoder
    // would have written for that store — the two paths agree on every byte.
    reserved[range].copy_from_slice(&store);
    let direct = AvifEncoder::new()
        .with_c2pa(&store)
        .encode_to_vec(image(&rgb))
        .expect("encode with the store");
    assert_eq!(reserved, direct);
}

#[test]
fn encode_with_report_yields_encode_to_vec_bytes_and_no_range_when_unconfigured() {
    let rgb = source_rgb();
    let plain = AvifEncoder::new()
        .encode_to_vec(image(&rgb))
        .expect("encode");
    let (bytes, report) = AvifEncoder::new()
        .encode_with_report(image(&rgb))
        .expect("encode with report");
    assert_eq!(bytes, plain, "the same encode, with a report alongside");
    assert_eq!(report.c2pa, None);

    // With a slot, the bytes are still the trait's bytes.
    let reserved = AvifEncoder::new().with_c2pa_reserved(64);
    let plain = reserved.encode_to_vec(image(&rgb)).expect("encode");
    let (bytes, report) = reserved
        .encode_with_report(image(&rgb))
        .expect("encode with report");
    assert_eq!(bytes, plain);
    assert_eq!(report.c2pa.map(|r| r.len()), Some(64));
}

#[test]
fn the_crate_locates_the_slot_it_reserved_at_the_reported_range() {
    let rgb = source_rgb();
    let (bytes, report) = AvifEncoder::new()
        .with_c2pa_reserved(128)
        .encode_with_report(image(&rgb))
        .expect("encode");
    let container = AvifContainer::parse(&bytes).expect("gamut-avif parses its own output");
    let store = container.c2pa().expect("the slot is located");
    assert_eq!(
        Some(store.range.clone()),
        report.c2pa,
        "reader and writer agree"
    );
    assert_eq!(
        store.slot_bytes,
        &[0u8; 128][..],
        "an unfilled slot is zeros"
    );
    assert_eq!(store.purpose, C2paBoxPurpose::Manifest);
    assert_eq!(&bytes[store.range], store.slot_bytes);
    assert_eq!(container.c2pa_manifest_stores().count(), 1);

    // A file with no box locates nothing.
    let plain = AvifEncoder::new()
        .encode_to_vec(image(&rgb))
        .expect("encode");
    assert!(
        AvifContainer::parse(&plain)
            .expect("parses")
            .c2pa()
            .is_none()
    );
}

#[test]
fn a_file_mid_update_reports_its_original_and_update_stores_in_file_order() {
    // The §A.5.3 mid-update shape: the previous store re-labelled `original` where it was (after
    // `ftyp`), and an `update` store as the last box of the file. The payloads are transcribed
    // from §A.5.1.2/§A.5.3 by hand here — version/flags, `box_purpose` NUL, merkle offset, store
    // — independently of the encoder's builder, so the locator is checked against the clause and
    // not against its own writer.
    fn c2pa_box(purpose: &[u8], store: &[u8]) -> Vec<u8> {
        let mut p = vec![0u8; 4];
        p.extend_from_slice(purpose);
        p.push(0);
        p.extend_from_slice(&[0u8; 8]);
        p.extend_from_slice(store);
        p
    }
    let original = payload(0x33, 40);
    let update = payload(0x44, 24);
    let model = IsoBmffImage::new(
        *b"avif",
        vec![*b"avif", *b"mif1", *b"miaf"],
        1,
        vec![av01_item(
            1,
            vec![0x0A, 0x01, 0x18, 0x32, 0x03, 0xAA, 0xBB, 0xCC],
        )],
    )
    .with_top_level_boxes(vec![
        TopLevelBox::uuid(C2PA_UUID, c2pa_box(b"original", &original)),
        TopLevelBox::uuid(C2PA_UUID, c2pa_box(b"update", &update))
            .with_position(TopLevelPosition::Trailing),
    ]);
    let bytes = write(&model).expect("writes");
    let container = AvifContainer::parse(&bytes).expect("parses");

    let stores: Vec<_> = container.c2pa_manifest_stores().collect();
    assert_eq!(stores.len(), 2);
    assert_eq!(stores[0].purpose, C2paBoxPurpose::Original);
    assert_eq!(stores[0].slot_bytes, &original[..]);
    assert_eq!(&bytes[stores[0].range.clone()], &original[..]);
    assert_eq!(stores[1].purpose, C2paBoxPurpose::Update);
    assert_eq!(stores[1].slot_bytes, &update[..]);
    assert_eq!(&bytes[stores[1].range.clone()], &update[..]);
    // The update box is the file's last box, so its slot runs to end of file.
    assert_eq!(stores[1].range.end, bytes.len());
    // `c2pa()` is "the first one", never a judgement about which is active.
    assert_eq!(
        container.c2pa().map(|s| s.purpose),
        Some(C2paBoxPurpose::Original)
    );
}

#[test]
fn libavif_decodes_a_file_carrying_a_reserved_slot_unchanged() {
    // libavif knows nothing of C2PA. A top-level `uuid` box between `ftyp` and `meta` must be
    // invisible to it: the same structure and the same pixels as the encode without the box.
    let rgb = source_rgb();
    let bare = AvifEncoder::new()
        .encode_to_vec(image(&rgb))
        .expect("encode");
    let reserved = AvifEncoder::new()
        .with_c2pa_reserved(4096)
        .encode_to_vec(image(&rgb))
        .expect("encode");
    assert!(reserved.len() > bare.len(), "the slot is actually stored");

    let a = libavif_oracle::decode_avif(&bare).expect("libavif decodes the bare file");
    let b = libavif_oracle::decode_avif(&reserved).expect("libavif decodes the file with the box");
    assert_eq!(
        (a.width, a.height, a.bit_depth),
        (b.width, b.height, b.bit_depth)
    );
    assert_eq!(a.planes, b.planes, "pixels differ once the box is carried");

    let sa = libavif_oracle::introspect(&bare).expect("libavif parses the bare file");
    let sb = libavif_oracle::introspect(&reserved).expect("libavif parses the file with the box");
    assert_eq!(
        (
            sa.width,
            sa.height,
            sa.depth,
            sa.yuv_format,
            sa.alpha_present
        ),
        (
            sb.width,
            sb.height,
            sb.depth,
            sb.yuv_format,
            sb.alpha_present
        )
    );
}

#[test]
fn dav1d_through_the_crate_decodes_a_file_carrying_a_reserved_slot_unchanged() {
    // The crate's own container parse over a file with the box, with the codestream decoded by the
    // real dav1d: the presentation-ready output equals the bare file's — and, lossless, the
    // source.
    let rgb = source_rgb();
    let bare = AvifEncoder::new()
        .encode_to_vec(image(&rgb))
        .expect("encode");
    let reserved = AvifEncoder::new()
        .with_c2pa_reserved(4096)
        .encode_to_vec(image(&rgb))
        .expect("encode");

    let a = AvifContainer::parse(&bare)
        .expect("parses")
        .decode_primary_rgba8(&mut Dav1dDecoder)
        .expect("decodes");
    let b = AvifContainer::parse(&reserved)
        .expect("parses")
        .decode_primary_rgba8(&mut Dav1dDecoder)
        .expect("decodes");
    assert_eq!(a.as_samples(), b.as_samples());
    for (i, px) in rgb.as_chunks::<3>().0.iter().enumerate() {
        assert_eq!(&b.as_samples()[i * 4..i * 4 + 3], px, "pixel {i}");
        assert_eq!(b.as_samples()[i * 4 + 3], 255, "opaque at {i}");
    }
}
