//! integration · law — cross-crate interoperability checks owned by the umbrella layer.
//!
//! Each test here spans two publishable crates that must not gain a dev-dependency edge on one
//! another: `mise run check-release-deps` rejects those, because release-plz orders normal and
//! build edges but ignores dev-only ones, so a versioned dev edge between publishable crates can
//! make a bump unpublishable. That is the third linkage cause in `docs/testing.md`, and it is why
//! these cannot live in either crate they check.
//!
//! **These tests are mutation-invisible, and that cost is accepted here rather than overlooked.**
//! `.cargo/mutants.toml` sets `test_workspace = false`, so a mutant in another crate runs only
//! that crate's own tests, and nothing in this file can kill a mutant in `gamut-jpeg`,
//! `gamut-webp`, `gamut-metadata`, `gamut-color` or `gamut-tonemap` (the umbrella's own mutants,
//! in its `capability` table, are the only ones these tests see). What each test pins, and where the mutation-visible coverage of the same
//! machinery lives:
//!
//! - `metadata_facade_round_trips_through_a_jpeg_stream` and its WebP counterpart pin that the
//!   **facade's** models survive a real container. The Exif/XMP encoders and the container writers
//!   are each covered inside their own crates (`gamut-exif`'s exiv2 oracle, `gamut-jpeg`'s
//!   libjpeg-turbo oracle, `gamut-webp`'s libwebp oracle); what is only checked here is that the
//!   two ends agree across the seam.
//! - `reinhard_matches_the_color_pq_to_sdr_step` pins that `gamut-tonemap`'s Reinhard curve agrees
//!   with the value `gamut-color`'s PQ transfer function feeds it. Both sides are separately
//!   pinned in their own crates -- `gamut-tonemap`'s operator tests and `src/invariants.rs`, and
//!   `gamut-color`'s lcms2 oracle -- so a defect in either is caught there; the agreement between
//!   them is what is only checked here.
//!
//! Nothing in this file is the *only* pin on a behaviour. Anything that would be belongs in the
//! crate that owns it.

use gamut::core::{Dimensions, EncodeImage, Gray8, ImageRef};

#[test]
fn metadata_facade_round_trips_through_a_jpeg_stream() {
    use gamut::jpeg::{JpegDecoder, JpegEncoder};
    use gamut::metadata::exif::{ByteOrder, Exif, ExifTag, Value};
    use gamut::metadata::icc::{ColorSpace, DeviceClass, IccProfile, ProfileHeader};
    use gamut::metadata::xmp::{WellKnownNs, XmpMeta};
    use gamut::metadata::{Metadata, MetadataBlock};

    let mut exif = Exif::new(ByteOrder::LittleEndian);
    exif.set_tag(ExifTag::Make, Value::Ascii("gamut".to_owned()));
    let mut xmp = XmpMeta::new();
    xmp.set_text(WellKnownNs::Xmp.uri(), "CreatorTool", "gamut");
    let icc = IccProfile {
        header: ProfileHeader::new(DeviceClass::Display, ColorSpace::Rgb),
        tags: Vec::new(),
    };
    let typed = Metadata::from_carriers(Some(exif), Some(xmp), Some(icc));

    let encoded = typed.to_blocks().unwrap();
    let pixels = vec![128u8; 64];
    let image = ImageRef::<Gray8>::new(&pixels, Dimensions::new(8, 8).unwrap()).unwrap();
    let jpeg = JpegEncoder::new()
        .with_exif(encoded.exif.as_deref().unwrap())
        .with_xmp(encoded.xmp.as_deref().unwrap())
        .with_icc_profile(encoded.icc.as_deref().unwrap())
        .encode_to_vec(image)
        .unwrap();

    let read = JpegDecoder::new().metadata(&jpeg).unwrap();
    let through_jpeg = Metadata::parse_blocks(&[
        MetadataBlock::Exif(read.exif.as_deref().unwrap()),
        MetadataBlock::Xmp(read.xmp.as_deref().unwrap()),
        MetadataBlock::Icc(read.icc.as_deref().unwrap()),
    ])
    .unwrap();
    let direct = Metadata::parse_blocks(&[
        MetadataBlock::Exif(encoded.exif.as_deref().unwrap()),
        MetadataBlock::Xmp(encoded.xmp.as_deref().unwrap()),
        MetadataBlock::Icc(encoded.icc.as_deref().unwrap()),
    ])
    .unwrap();

    assert_eq!(through_jpeg, direct);
    assert_eq!(
        through_jpeg.exif.as_ref().and_then(|value| value.make()),
        Some("gamut")
    );
}

#[test]
fn metadata_facade_round_trips_through_a_webp_file() {
    // The WebP counterpart of the JPEG check above. WebP carries each block as its own RIFF chunk
    // (`ICCP` / `EXIF` / `XMP `) with no signature framing or 64 KiB segmentation, so the facade's
    // encoded bytes must reappear verbatim — this is the seam a downstream caller migrating off
    // libwebp-sys depends on.
    use gamut::core::{ImageRef, Rgb8};
    use gamut::metadata::exif::{ByteOrder, Exif, ExifTag, Value};
    use gamut::metadata::icc::{ColorSpace, DeviceClass, IccProfile, ProfileHeader};
    use gamut::metadata::xmp::{WellKnownNs, XmpMeta};
    use gamut::metadata::{Metadata, MetadataBlock};
    use gamut::webp::{WebpEncoder, metadata};

    let mut exif = Exif::new(ByteOrder::LittleEndian);
    exif.set_tag(ExifTag::Make, Value::Ascii("gamut".to_owned()));
    let mut xmp = XmpMeta::new();
    xmp.set_text(WellKnownNs::Xmp.uri(), "CreatorTool", "gamut");
    let typed = Metadata::from_carriers(
        Some(exif),
        Some(xmp),
        Some(IccProfile {
            header: ProfileHeader::new(DeviceClass::Display, ColorSpace::Rgb),
            tags: Vec::new(),
        }),
    );

    let encoded = typed.to_blocks().unwrap();
    let pixels = [64u8, 128, 192];
    let image = ImageRef::<Rgb8>::new(&pixels, Dimensions::new(1, 1).unwrap()).unwrap();
    let mut file = Vec::new();
    WebpEncoder::lossless()
        .with_exif(encoded.exif.as_deref().unwrap())
        .with_xmp(encoded.xmp.as_deref().unwrap())
        .with_icc_profile(encoded.icc.as_deref().unwrap())
        .encode_image(image, &mut file)
        .unwrap();

    let read = metadata(&file).unwrap();
    assert_eq!(read.exif, encoded.exif, "EXIF payload is unmodified");
    assert_eq!(read.xmp, encoded.xmp, "XMP payload is unmodified");
    assert_eq!(read.icc, encoded.icc, "ICC payload is unmodified");

    let through_webp = Metadata::parse_blocks(&[
        MetadataBlock::Exif(read.exif.as_deref().unwrap()),
        MetadataBlock::Xmp(read.xmp.as_deref().unwrap()),
        MetadataBlock::Icc(read.icc.as_deref().unwrap()),
    ])
    .unwrap();
    assert_eq!(
        through_webp.exif.as_ref().and_then(|value| value.make()),
        Some("gamut")
    );
    assert_eq!(
        through_webp
            .xmp
            .as_ref()
            .and_then(|value| value.get_text(WellKnownNs::Xmp.uri(), "CreatorTool")),
        Some("gamut")
    );
}

#[test]
fn reinhard_matches_the_color_pq_to_sdr_step() {
    use gamut::color::transfer::{bt2020_pq_to_sdr, pq_eotf};
    use gamut::core::luminance::HDR_REFERENCE_WHITE_NITS;
    use gamut::tonemap::{Reinhard, ToneCurve};

    for &signal in &[0.1_f64, 0.25, 0.5, 0.75, 1.0] {
        let linear = pq_eotf(signal) / HDR_REFERENCE_WHITE_NITS;
        let tone_mapped = Reinhard.map(linear as f32);
        let converted = bt2020_pq_to_sdr(signal) as f32;
        assert!(
            (tone_mapped - converted).abs() <= 1e-6,
            "signal {signal}: {tone_mapped} vs {converted}"
        );
    }
}
