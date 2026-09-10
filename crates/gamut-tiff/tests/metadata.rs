//! The metadata seam end to end: what `TiffEncoder::with_metadata` puts in a file, on every
//! layout the encoder writes, and what `TiffDecoder::metadata` gets back out of it.
//!
//! Each test pins one encode path's use of the seam, so a path that stopped embedding metadata
//! fails on its own rather than hiding behind another.

use gamut_core::{DecodeImage, Dimensions, EncodeImage, ImageBuf, ImageRef, Rgb8};
use gamut_tiff::{
    Anomaly, Ifd, Severity, TiffDecoder, TiffEncoder, TiffMetadata, Value, deconstruct, read, tags,
};

/// Distinct payloads per carrier, so a block written under the wrong tag is visible.
const XMP: &[u8] = b"<x:xmpmeta><rdf:RDF/></x:xmpmeta>";
const IPTC: &[u8] = &[0x1c, 0x02, 0x05, 0x00, 0x04, b't', b'e', b's', b't'];
const ICC: &[u8] = &[0, 0, 0, 12, b'a', b'c', b's', b'p', 1, 2, 3, 4];
/// A JUMBF-shaped C2PA manifest store: long enough for `gamut_ifd::c2pa::locate` to accept it
/// (`LBox` + `TBox`, 8 bytes) and out of line in a classic TIFF entry.
const STORE: &[u8] = &[0, 0, 0, 0x16, b'j', b'u', b'm', b'b', 1, 2, 3, 4];

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
fn the_encoder_refuses_an_exif_tree_its_own_decoder_could_not_read_back() {
    // The encoder used to write any nesting a caller built and `metadata()` refused a third level
    // of it, so this crate emitted a well-formed file it could not itself read — the one shape a
    // seam whose contract is "what the file holds is what the caller gets" must not have. The
    // refusal is the encoder's, before any pixel work; the reader's side of the same bound is
    // `a_directory_below_the_exif_interop_pair_is_too_deep` (src/metadata.rs), and the depth this
    // pair *does* reach round-trips in
    // `a_decoded_exif_sub_ifd_re_encodes_into_a_fully_classified_file` above.
    let mut interop = Ifd::new();
    interop.set(1, Value::Ascii("R98".into())); // InteroperabilityIndex
    let mut inner = exif();
    inner.set_sub_ifd(tags::INTEROPERABILITY_IFD, vec![interop]);
    let mut deeper = exif();
    deeper.set_sub_ifd(tags::INTEROPERABILITY_IFD, vec![inner]);

    let pixels = rgb(8, 4);
    let err = TiffEncoder::new()
        .with_metadata(TiffMetadata::new().with_exif(deeper))
        .encode_to_vec(image(&pixels, 8, 4))
        .expect_err("a tree the decoder refuses must not be written");
    assert!(err.to_string().contains("nests deeper"), "{err}");
}

/// The uncompressed 2×2 RGB directory every hand-built page below starts from: enough fields for
/// the pixels to decode, and nothing that feeds [`TiffMetadata`].
fn page_ifd() -> Ifd {
    let mut ifd = Ifd::new();
    ifd.set(tags::IMAGE_WIDTH, Value::Short(vec![2]));
    ifd.set(tags::IMAGE_LENGTH, Value::Short(vec![2]));
    ifd.set(tags::BITS_PER_SAMPLE, Value::Short(vec![8, 8, 8]));
    ifd.set(tags::COMPRESSION, Value::Short(vec![1]));
    ifd.set(tags::PHOTOMETRIC_INTERPRETATION, Value::Short(vec![2]));
    ifd.set(tags::SAMPLES_PER_PIXEL, Value::Short(vec![3]));
    ifd.set(tags::ROWS_PER_STRIP, Value::Short(vec![2]));
    ifd
}

/// The one strip [`page_ifd`]'s directory describes.
fn page_strips() -> Vec<Vec<u8>> {
    vec![vec![0u8; 2 * 2 * 3]]
}

/// A well-formed single-strip RGB file carrying XMP, plus one extra IFD-0 field.
///
/// Used to hand `metadata()` a file whose *pixels* are perfectly readable but whose IFD 0 carries
/// a pointer tag feeding no field of [`TiffMetadata`].
fn file_with_extra_ifd0_field(tag: u16, value: Value) -> Vec<u8> {
    let mut ifd = page_ifd();
    ifd.set(tags::XMP, Value::Byte(XMP.to_vec()));
    ifd.set(tag, value);
    gamut_tiff::write_image(
        gamut_tiff::ByteOrder::LittleEndian,
        gamut_tiff::Variant::Classic,
        &ifd,
        &page_strips(),
    )
    .expect("write")
}

/// Serialises `pages` as a multi-page classic TIFF, every page carrying [`page_strips`].
fn multipage(pages: &[Ifd]) -> Vec<u8> {
    let pages: Vec<(Ifd, Vec<Vec<u8>>)> = pages
        .iter()
        .map(|ifd| (ifd.clone(), page_strips()))
        .collect();
    gamut_tiff::write_multipage(
        gamut_tiff::ByteOrder::LittleEndian,
        gamut_tiff::Variant::Classic,
        &pages,
    )
    .expect("write")
}

#[test]
fn a_broken_pointer_the_metadata_does_not_use_does_not_hide_the_blocks() {
    // `TiffMetadata` has five fields, and an IFD-0 `SubIFDs` (330) or `GPSInfo` (34853) group
    // feeds none of them — a thumbnail directory and a GPS directory are not XMP, IPTC, ICC, C2PA
    // or the Exif sub-IFD. So following them can only add failure modes: a dangling one would make
    // every block unreachable because of a pointer nobody asked for. The pixels of these files
    // decode fine, which is exactly what makes losing the metadata indefensible.
    for tag in [tags::SUB_IFDS, tags::GPS_INFO] {
        let dangling = Value::Long(vec![0xFFFF_FF00]);
        let bytes = file_with_extra_ifd0_field(tag, dangling);
        let meta = TiffDecoder::new()
            .metadata(&bytes)
            .unwrap_or_else(|e| panic!("tag {tag}: metadata must survive a dangling pointer: {e}"));
        assert_eq!(meta.xmp.as_deref(), Some(XMP), "tag {tag}");
        // The same property keeps a multi-page file whose pages share one thumbnail directory
        // readable: an offset that is never followed cannot trip the cross-chain loop guard.
    }
}

#[test]
fn a_broken_exif_pointer_is_still_an_error() {
    // The other half of the scoping rule. The Exif sub-IFD's content *is* returned, so reporting
    // `exif: None` for a directory the file declares would be silent loss — this is the one
    // pointer whose failure the caller must hear about. The *message* is the claim, not merely
    // `is_err`: the file also carries a dangling offset the reader must not have followed for any
    // other reason, so a refusal naming something else would mean the wrong pointer failed.
    let bytes = file_with_extra_ifd0_field(tags::EXIF_IFD, Value::Long(vec![0xFFFF_FF00]));
    let err = TiffDecoder::new()
        .metadata(&bytes)
        .expect_err("a dangling ExifIFD is the caller's business");
    assert!(err.to_string().contains("read out of bounds"), "{err}");
}

#[test]
fn a_broken_pointer_on_a_page_the_metadata_discards_does_not_fail_the_read() {
    // The same rule that keeps `SubIFDs` and `GPSInfo` out of `POINTER_TAGS`, applied to whole
    // *pages*: the blocks come from IFD 0 and the C2PA store from the last IFD, so a pointer
    // anywhere else feeds nothing this returns and following it can only add failure modes. A
    // dangling `ExifIFD` on page 1 of a two-page document used to fail the whole call.
    //
    // The store on that same last page is the other half of the claim: its entry carries the
    // store's bytes rather than an offset, so the directory whose pointers are never resolved
    // still delivers it.
    let mut page0 = page_ifd();
    page0.set(tags::XMP, Value::Byte(XMP.to_vec()));
    let mut page1 = page_ifd();
    page1.set(tags::EXIF_IFD, Value::Long(vec![0xFFFF_FF00]));
    page1.set(tags::C2PA_MANIFEST_STORE, Value::Undefined(STORE.to_vec()));
    let bytes = multipage(&[page0, page1]);

    let meta = TiffDecoder::new()
        .metadata(&bytes)
        .expect("a pointer on a discarded page must not fail the read");
    assert_eq!(meta.xmp.as_deref(), Some(XMP), "IFD 0's blocks");
    assert_eq!(meta.c2pa.as_deref(), Some(STORE), "the last IFD's store");
    // And the file is sound, which is what makes losing its metadata indefensible.
    let decoded: ImageBuf<Rgb8> = TiffDecoder::new().decode_image(&bytes).expect("decode");
    assert_eq!(decoded.dimensions().width, 2);
}

/// A two-page file whose pages point their `ExifIFD` at **one** directory.
///
/// Built in two passes: the offset the writer gives page 0's Exif directory is not known until it
/// has laid the file out, so pass 1 carries a same-sized `LONG` placeholder on page 1 and pass 2
/// substitutes the real offset into a byte-identical layout.
fn two_pages_sharing_one_exif_directory() -> Vec<u8> {
    let build = |offset: u32| {
        let mut page0 = page_ifd();
        page0.set_sub_ifd(tags::EXIF_IFD, vec![exif()]);
        let mut page1 = page_ifd();
        page1.set(tags::EXIF_IFD, Value::Long(vec![offset]));
        multipage(&[page0, page1])
    };
    let laid_out = read(&build(0)).expect("read").ifds[0]
        .get_u32(tags::EXIF_IFD)
        .expect("page 0's Exif pointer");
    let bytes = build(laid_out);
    let pages = read(&bytes).expect("read").ifds;
    assert_eq!(
        (
            pages[0].get_u32(tags::EXIF_IFD),
            pages[1].get_u32(tags::EXIF_IFD)
        ),
        (Some(laid_out), Some(laid_out)),
        "the two pages must end up sharing one directory for this to be the file under test"
    );
    bytes
}

#[test]
fn two_pages_naming_one_exif_directory_do_not_trip_the_loop_guard() {
    // A cross-page pointer graph the reader walked with a single `visited` set: page 1's
    // `ExifIFD` looked like a second pointer claiming page 0's directory and failed the call —
    // for a page whose Exif is discarded. Page 0's Exif is the one that is returned, so it is
    // what the assertion reads.
    let bytes = two_pages_sharing_one_exif_directory();
    let meta = TiffDecoder::new()
        .metadata(&bytes)
        .expect("a shared directory on a discarded page must not fail the read");
    assert_eq!(meta.exif, Some(exif()));
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
