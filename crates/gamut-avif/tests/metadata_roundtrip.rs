//! The colour and metadata surface, checked against a real AVIF reader: encode with the
//! `with_primaries` / `with_transfer` / `with_icc_profile` / `with_exif` / `with_xmp` knobs, then
//! ask libavif what it found.
//!
//! Reading our own container back proves the bytes round-trip through `gamut-isobmff`; it cannot
//! prove another implementation agrees about where a payload lives or how it is framed. libavif
//! (dav1d backend) is linked in from the `third_party/libavif` + `third_party/dav1d` submodules via
//! the `libavif-oracle` dev-dependency, so this check is hermetic — it never depends on an
//! `avifdec` binary being installed. Building it needs cmake/meson/ninja/nasm and the checked-out
//! submodules (`git submodule update --init --recursive` plus `mise run fetch-av1-oracles`).
//!
//! Spec references: `colr` and the CICP code points — AVIF v1.2.0 §2.2 and ITU-T H.273; the ICC
//! `colour_type` `prof` — ISO/IEC 23008-12; Exif and XMP items and their `cdsc` reference — AVIF
//! v1.2.0 §9.1.2 and ISO/IEC 23008-12.

use gamut_avif::{AvifContainer, AvifEncoder, ContentLightLevel, MasteringDisplay, Rotation};
use gamut_color::{ColourPrimaries, MatrixCoefficients, TransferCharacteristics};
use gamut_core::{Dimensions, EncodeImage, ImageRef, Rgb8};
use gamut_isobmff::PropertyKind;

const W: u32 = 34;
const H: u32 = 18;

/// A structured source: enough variation that the codestream is not degenerate, and identical
/// across every case so a difference in the decoded planes can only come from the knobs.
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

fn encode(encoder: AvifEncoder) -> Vec<u8> {
    let rgb = source_rgb();
    let dims = Dimensions {
        width: W,
        height: H,
    };
    encoder
        .encode_to_vec(ImageRef::<Rgb8>::new(&rgb, dims).expect("buffer matches dimensions"))
        .expect("encode")
}

/// A deterministic payload standing in for a real ICC profile — the encoder carries it verbatim and
/// libavif does not parse it, so its *content* is irrelevant and its *bytes* are the assertion.
fn payload(seed: u8, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| seed.wrapping_add((i * 31) as u8))
        .collect()
}

/// A minimal but **valid** Exif TIFF stream: a little-endian header pointing at an IFD holding one
/// `Orientation` (0x0112) SHORT = 1, then a null next-IFD offset.
///
/// It has to be real. libavif validates the Exif payload at parse time and rejects a file whose
/// item is not a TIFF stream ("Invalid Exif payload"), so a caller of `with_exif` owes the encoder
/// a well-formed stream — the encoder carries bytes verbatim and does not check them.
fn exif_tiff() -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(b"II\x2a\x00"); // little-endian byte order, magic 42
    v.extend_from_slice(&8u32.to_le_bytes()); // IFD0 begins right after the header
    v.extend_from_slice(&1u16.to_le_bytes()); // one entry
    v.extend_from_slice(&0x0112u16.to_le_bytes()); // Orientation
    v.extend_from_slice(&3u16.to_le_bytes()); // SHORT
    v.extend_from_slice(&1u32.to_le_bytes()); // count
    v.extend_from_slice(&1u32.to_le_bytes()); // value 1, stored inline
    v.extend_from_slice(&0u32.to_le_bytes()); // no next IFD
    v
}

/// A plausible XMP packet. libavif carries XMP opaquely, but a realistic fixture keeps the test
/// honest about what callers actually pass.
fn xmp_packet() -> Vec<u8> {
    br#"<?xpacket begin="" id="W5M0MpCehiHzreSzNTczkc9d"?><x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about=""/></rdf:RDF></x:xmpmeta><?xpacket end="w"?>"#.to_vec()
}

#[test]
fn libavif_reads_back_the_selected_cicp_code_points() {
    // Each case pairs what was configured with what CICP says. Primaries and transfer are tags, so
    // they survive on the lossless path too — where matrix and range are deliberately pinned.
    let cases = [
        (
            AvifEncoder::lossless()
                .with_primaries(ColourPrimaries::Bt2020)
                .with_transfer(TransferCharacteristics::Pq),
            9u16,
            16u16,
            0u16,
            true,
        ),
        (
            AvifEncoder::lossy(50)
                .with_primaries(ColourPrimaries::DisplayP3)
                .with_transfer(TransferCharacteristics::Hlg)
                .with_matrix(MatrixCoefficients::Bt2020Ncl),
            12,
            18,
            9,
            true,
        ),
        // BT.709 primaries with a non-sRGB transfer leaves AV1 §5.5.2's sRGB shortcut, so
        // `color_range` stops being inferred and is coded explicitly. libavif must still report
        // full range — the bit now says so rather than the shortcut implying it.
        (
            AvifEncoder::lossless().with_transfer(TransferCharacteristics::Linear),
            1,
            8,
            0,
            true,
        ),
    ];
    for (encoder, primaries, transfer, matrix, full_range) in cases {
        let avif = encode(encoder);
        let s = libavif_oracle::introspect(&avif).expect("libavif parses the container");
        assert_eq!(s.color_primaries, primaries);
        assert_eq!(s.transfer_characteristics, transfer);
        assert_eq!(s.matrix_coefficients, matrix);
        assert_eq!(s.full_range, full_range);
        assert_eq!((s.width, s.height), (W, H));
    }
}

#[test]
fn libavif_reads_back_the_embedded_icc_profile() {
    let icc = payload(0x11, 512);
    let avif = encode(AvifEncoder::new().with_icc_profile(&icc));
    let s = libavif_oracle::introspect(&avif).expect("libavif parses the container");
    assert_eq!(s.icc, icc, "the profile is carried verbatim");
    // The CICP box is not displaced by the profile: libavif reports both, which is what lets a
    // non-colour-managed reader still interpret the samples.
    assert_eq!(s.color_primaries, 1);
    assert_eq!(s.transfer_characteristics, 13);
}

#[test]
fn libavif_reads_back_the_exif_and_xmp_items() {
    let exif = exif_tiff();
    let xmp = xmp_packet();
    let avif = encode(AvifEncoder::new().with_exif(&exif).with_xmp(&xmp));
    let s = libavif_oracle::introspect(&avif).expect("libavif parses the container");
    // libavif strips the 4-byte `exif_tiff_header_offset` the encoder prepends, so what comes back
    // is exactly the bare TIFF stream the caller handed over. That equality is the whole point of
    // the framing: get the prefix wrong and this is off by four bytes.
    assert_eq!(s.exif, exif, "Exif item framing agrees with libavif");
    assert_eq!(s.xmp, xmp, "XMP packet is carried verbatim");
}

/// `MaxCLL` and `MaxPALL` differ, so a swapped pair cannot pass for the right one.
const CLLI: ContentLightLevel = ContentLightLevel {
    max_content_light_level: 1000,
    max_pic_average_light_level: 400,
};

/// Every field differs from every other, so a transposed primary pair or swapped luminance bound
/// cannot reproduce the expected value.
const MDCV: MasteringDisplay = MasteringDisplay {
    display_primaries: [[13250, 34500], [7500, 3000], [34000, 16000]],
    white_point: [15635, 16450],
    max_display_mastering_luminance: 10_000_000,
    min_display_mastering_luminance: 50,
};

/// A PQ-tagged BT.2020 encoder carrying both HDR properties — the configuration they describe.
fn hdr_encoder() -> AvifEncoder {
    AvifEncoder::lossless()
        .with_primaries(ColourPrimaries::Bt2020)
        .with_transfer(TransferCharacteristics::Pq)
        .with_content_light_level(CLLI)
        .with_mastering_display(MDCV)
}

#[test]
fn libavif_reads_back_the_content_light_level() {
    let s = libavif_oracle::introspect(&encode(hdr_encoder())).expect("libavif parses the container");
    assert_eq!(s.clli, [1000, 400], "libavif reads MaxCLL/MaxPALL as written");
}

#[test]
fn libavif_accepts_the_mastering_display_property() {
    // The pinned libavif skips `mdcv` rather than reading it, so value agreement is not checkable
    // here (gamut-heic's `oracle_isobmff_mdcv` checks the box layout against libheif). What it can
    // check is placement: libavif ignores an item citing an *essential* property it does not
    // support, so a wrongly-essential `mdcv` would leave it with no primary image to parse.
    let avif = encode(AvifEncoder::new().with_mastering_display(MDCV));
    let s = libavif_oracle::introspect(&avif).expect("libavif parses the container");
    assert_eq!((s.width, s.height), (W, H));
}

#[test]
fn the_crate_reads_back_the_hdr_properties_as_descriptive() {
    // A rotation as well, so the order of descriptive before transformative properties is visible.
    let avif = encode(hdr_encoder().with_rotation(Rotation::Ccw90));
    let container = AvifContainer::parse(&avif).expect("gamut-avif parses its own output");
    let primary = container.image().primary_item();

    assert_eq!(primary.content_light_level(), Some(CLLI));
    assert_eq!(primary.mastering_display(), Some(MDCV));

    let properties = &primary.as_isobmff_item().properties;
    let position = |want: fn(&PropertyKind) -> bool| {
        properties
            .iter()
            .position(|p| want(&p.kind))
            .expect("property is associated")
    };
    let clli = position(|k| matches!(k, PropertyKind::ContentLightLevel { .. }));
    let mdcv = position(|k| matches!(k, PropertyKind::MasteringDisplayColourVolume { .. }));
    let irot = position(|k| matches!(k, PropertyKind::Rotation(_)));
    assert!(!properties[clli].essential, "clli is descriptive");
    assert!(!properties[mdcv].essential, "mdcv is descriptive");
    assert!(clli < irot && mdcv < irot, "descriptive properties precede transformative ones");
}

#[test]
fn metadata_does_not_disturb_the_coded_pixels() {
    // Every knob here is container-side. The codestream must be untouched, so a fully-annotated
    // file decodes to the same planes as a bare one — and, being lossless, to the source itself.
    let bare = encode(AvifEncoder::new());
    let annotated = encode(
        AvifEncoder::new()
            .with_icc_profile(&payload(0x11, 512))
            .with_exif(&exif_tiff())
            .with_xmp(&xmp_packet()),
    );
    assert!(
        annotated.len() > bare.len(),
        "the payloads are actually stored"
    );

    let a = libavif_oracle::decode_avif(&bare).expect("libavif decodes the bare file");
    let b = libavif_oracle::decode_avif(&annotated).expect("libavif decodes the annotated file");
    assert_eq!(a.planes, b.planes, "the codestream is unchanged");

    // …and both are still bit-exact to the source (identity matrix: Y=G, U=B, V=R).
    let rgb = source_rgb();
    for (i, px) in rgb.as_chunks::<3>().0.iter().enumerate() {
        assert_eq!(b.planes[0][i], u16::from(px[1]), "Y = G at {i}");
        assert_eq!(b.planes[1][i], u16::from(px[2]), "U = B at {i}");
        assert_eq!(b.planes[2][i], u16::from(px[0]), "V = R at {i}");
    }
}

#[test]
fn the_crate_reads_back_what_it_wrote() {
    // The write side and the crate's own role lenses must describe the same file — otherwise
    // gamut-avif can emit metadata it cannot itself find.
    let icc = payload(0x11, 512);
    let exif = exif_tiff();
    let xmp = xmp_packet();
    let avif = encode(
        AvifEncoder::new()
            .with_icc_profile(&icc)
            .with_exif(&exif)
            .with_xmp(&xmp),
    );
    let container = AvifContainer::parse(&avif).expect("gamut-avif parses its own output");
    let image = container.image();
    let primary = image.primary_item();

    assert_eq!(primary.icc_profile(), Some(icc.as_slice()));
    // `colour()` returns the *first* `colr`, which stays the CICP one.
    assert!(matches!(
        primary.colour(),
        Some(gamut_isobmff::ColourInformation::Nclx(_))
    ));

    // The metadata items are found by their `cdsc` reference to the primary, not by position.
    let described: Vec<u32> = image
        .metadata_of(primary.id())
        .iter()
        .map(gamut_avif::AvifItem::id)
        .collect();
    assert_eq!(described.len(), 2, "Exif and XMP both describe the primary");

    let mut want_exif = 0u32.to_be_bytes().to_vec();
    want_exif.extend_from_slice(&exif);
    assert_eq!(
        image.exif().map(|i| i.as_isobmff_item().payload.as_slice()),
        Some(want_exif.as_slice()),
        "the raw payload keeps the HEIF offset prefix libavif strips"
    );
    assert_eq!(
        image.xmp().map(|i| i.as_isobmff_item().payload.as_slice()),
        Some(xmp.as_slice())
    );
}
