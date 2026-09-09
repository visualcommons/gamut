//! The metadata seam end to end: what `TiffEncoder::with_metadata` puts in a file, on every
//! layout the encoder writes, and what `TiffDecoder::metadata` gets back out of it.
//!
//! Each test pins one encode path's use of the seam, so a path that stopped embedding metadata
//! fails on its own rather than hiding behind another.

use gamut_core::{Dimensions, EncodeImage, ImageRef, Rgb8};
use gamut_tiff::{
    Anomaly, Ifd, Severity, TiffDecoder, TiffEncoder, TiffMetadata, Value, deconstruct, read, tags,
};

/// Distinct payloads per carrier, so a block written under the wrong tag is visible.
const XMP: &[u8] = b"<x:xmpmeta><rdf:RDF/></x:xmpmeta>";
const IPTC: &[u8] = &[0x1c, 0x02, 0x05, 0x00, 0x04, b't', b'e', b's', b't'];
const ICC: &[u8] = &[0, 0, 0, 12, b'a', b'c', b's', b'p', 1, 2, 3, 4];

/// An Exif sub-IFD with one recognisable field (`ExposureTime`, 33434).
fn exif() -> Ifd {
    let mut ifd = Ifd::new();
    ifd.set(33434, Value::Rational(vec![(1, 250)]));
    ifd
}

fn metadata() -> TiffMetadata {
    TiffMetadata::new()
        .with_xmp(XMP.to_vec())
        .with_iptc(IPTC.to_vec())
        .with_icc(ICC.to_vec())
        .with_exif(exif())
}

fn rgb(w: u32, h: u32) -> Vec<u8> {
    (0..w * h * 3).map(|i| (i % 251) as u8).collect()
}

fn image(pixels: &[u8], w: u32, h: u32) -> ImageRef<'_, Rgb8> {
    ImageRef::<Rgb8>::new(
        pixels,
        Dimensions {
            width: w,
            height: h,
        },
    )
    .expect("image")
}

/// Asserts that `ifd` carries every block of [`metadata`] under its own tag and field type.
fn assert_carries_every_block(ifd: &Ifd) {
    assert_eq!(ifd.get(tags::XMP), Some(&Value::Byte(XMP.to_vec())));
    assert_eq!(ifd.get(tags::IPTC_NAA), Some(&Value::Byte(IPTC.to_vec())));
    assert_eq!(
        ifd.get(tags::ICC_PROFILE),
        Some(&Value::Undefined(ICC.to_vec()))
    );
    // The Exif sub-IFD is written as a pointer field, so a plain `read` sees the offset rather
    // than the directory; that it is present at all is this assertion's claim.
    assert!(ifd.get(tags::EXIF_IFD).is_some());
}

#[test]
fn the_strip_path_embeds_the_metadata_in_ifd_0() {
    let pixels = rgb(8, 4);
    let bytes = TiffEncoder::new()
        .with_metadata(metadata())
        .encode_to_vec(image(&pixels, 8, 4))
        .expect("encode");
    assert_carries_every_block(&read(&bytes).expect("read").ifds[0]);
}

#[test]
fn the_tile_path_embeds_the_metadata_in_ifd_0() {
    // Tiling builds its own directory rather than going through `build_strip_image`, so it needs
    // its own claim: a tiled encode that forgot the seam would pass the strip test above.
    let pixels = rgb(32, 32);
    let bytes = TiffEncoder::new()
        .with_tiling(16, 16)
        .with_metadata(metadata())
        .encode_to_vec(image(&pixels, 32, 32))
        .expect("encode");
    let file = read(&bytes).expect("read");
    assert!(file.ifds[0].get(tags::TILE_WIDTH).is_some(), "tiled");
    assert_carries_every_block(&file.ifds[0]);
}

#[test]
fn a_multipage_document_carries_the_metadata_on_page_0_only() {
    // The blocks describe the document, so exactly one page holds them — duplicating an ICC
    // profile onto every page would inflate the file and contradict that.
    let pixels = rgb(4, 4);
    let page = image(&pixels, 4, 4);
    let mut bytes = Vec::new();
    TiffEncoder::new()
        .with_metadata(metadata())
        .encode_pages_rgb8(&[page, page], &mut bytes)
        .expect("encode");
    let file = read(&bytes).expect("read");
    assert_eq!(file.ifds.len(), 2);
    assert_carries_every_block(&file.ifds[0]);
    for tag in [tags::XMP, tags::IPTC_NAA, tags::ICC_PROFILE, tags::EXIF_IFD] {
        assert_eq!(file.ifds[1].get(tag), None, "tag {tag} on page 1");
    }
}

#[test]
fn the_decoder_returns_every_block_verbatim() {
    let pixels = rgb(8, 4);
    let bytes = TiffEncoder::new()
        .with_metadata(metadata())
        .encode_to_vec(image(&pixels, 8, 4))
        .expect("encode");
    let read_back = TiffDecoder::new().metadata(&bytes).expect("metadata");
    assert_eq!(read_back.xmp.as_deref(), Some(XMP));
    assert_eq!(read_back.iptc.as_deref(), Some(IPTC));
    assert_eq!(read_back.icc.as_deref(), Some(ICC));
    assert_eq!(read_back.exif, Some(exif()));
}

#[test]
fn a_decoded_exif_sub_ifd_re_encodes_into_a_fully_classified_file() {
    // An `InteroperabilityIFD` (40965) *inside* the Exif directory is near-universal in camera
    // EXIF, and it is a pointer: its value is an absolute file offset. A decoder that returned it
    // as a raw `Long` rather than as a parsed child directory would hand the caller the *source*
    // file's offset, and re-encoding would write it verbatim into a file laid out differently —
    // a dangling pointer. The crate's own judge is the test: gamut-tiff's v1 guarantee is that
    // every file it writes is fully classified by `deconstruct`.
    let mut interop = Ifd::new();
    interop.set(1, Value::Ascii("R98".into())); // InteroperabilityIndex
    let mut exif = exif();
    exif.set(37500, Value::Undefined(vec![0xAB; 6])); // MakerNote, so the directory is not tiny
    exif.set_sub_ifd(tags::INTEROPERABILITY_IFD, vec![interop.clone()]);

    let pixels = rgb(8, 4);
    let first = TiffEncoder::new()
        .with_metadata(TiffMetadata::new().with_exif(exif))
        .encode_to_vec(image(&pixels, 8, 4))
        .expect("encode");

    // Decode the metadata back and feed it straight into a new encode — the round trip a caller
    // makes when rewriting a file.
    let decoded = TiffDecoder::new().metadata(&first).expect("metadata");
    let exif_back = decoded.exif.clone().expect("an Exif sub-IFD");
    assert_eq!(
        exif_back
            .sub_ifds()
            .iter()
            .find(|group| group.tag == tags::INTEROPERABILITY_IFD)
            .map(|group| group.ifds.as_slice()),
        Some(&[interop][..]),
        "the Interop directory must come back parsed, not as a raw offset"
    );
    assert_eq!(
        exif_back.get(tags::INTEROPERABILITY_IFD),
        None,
        "a parsed pointer is consumed into the sub-IFD group, not left as a stale offset"
    );

    let second = TiffEncoder::new()
        .with_metadata(decoded)
        .encode_to_vec(image(&pixels, 8, 4))
        .expect("re-encode");
    let report = deconstruct(&second).expect("deconstruct");
    assert!(
        report.segments.is_fully_classified(),
        "unclassified after a metadata round trip: {:?}",
        report.segments.unclassified
    );
    assert!(
        !report.anomalies.iter().any(
            |a| matches!(a, Anomaly::Structure { severity, .. } if *severity == Severity::Error)
        ),
        "structural errors after a metadata round trip: {:?}",
        report.anomalies
    );
}

#[test]
fn a_file_without_metadata_decodes_to_an_empty_set() {
    let pixels = rgb(8, 4);
    let bytes = TiffEncoder::new()
        .encode_to_vec(image(&pixels, 8, 4))
        .expect("encode");
    assert!(
        TiffDecoder::new()
            .metadata(&bytes)
            .expect("metadata")
            .is_empty()
    );
}
