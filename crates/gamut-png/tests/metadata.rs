//! The pixel-free metadata entry points: `gamut_png::metadata` and `PngDecoder::metadata`
//! (issue #379), mirroring `gamut-jpeg`'s and `gamut-webp`'s suites for the same shape.
//!
//! No oracle is involved: every stream is built in-process, either by the crate's own encoder
//! (for the round-trip assertions) or chunk by chunk from `common` (for the cases the encoder
//! cannot produce — cICP, a metadata-only stream, a deliberately corrupt CRC).

mod common;

use common::{
    chunk, ihdr_payload, minimal_png, png_from_chunks, tiny_exif, tiny_icc_profile, zlib,
};
use gamut_core::{Dimensions, EncodeImage, ErrorKind, ImageRef, Rgb8};
use gamut_png::{PngDecoder, PngEncoder, PngMetadata};

/// A 2×2 RGB8 source for the encoder-driven tests.
fn source() -> Vec<u8> {
    (0..12).map(|i| i as u8 * 20).collect()
}

fn encode(build: impl FnOnce(PngEncoder) -> PngEncoder) -> Vec<u8> {
    let pixels = source();
    let image = ImageRef::<Rgb8>::new(&pixels, Dimensions::new(2, 2).unwrap()).unwrap();
    build(PngEncoder::new()).encode_to_vec(image).unwrap()
}

#[test]
fn plain_png_has_no_metadata() {
    assert_eq!(
        gamut_png::metadata(&minimal_png()).unwrap(),
        PngMetadata::default()
    );
}

/// Every carrier round-trips through the encoder byte-for-byte. The point of the entry point is
/// that a caller can hand these straight to `gamut_metadata::MetadataBlock`, so an approximate
/// match would not do.
#[test]
fn every_carrier_round_trips_byte_exact() {
    let exif = tiny_exif();
    let icc = tiny_icc_profile();
    let xmp = "<?xpacket begin='' id='W5M0MpCehiHzreSzNTczkc9d'?><x:xmpmeta/>";
    let c2pa = b"\0\0\0\x14jumbopaque store";
    let png = encode(|e| {
        e.with_exif(&exif)
            .with_icc_profile("Tiny", &icc)
            .with_xmp(xmp)
            .with_c2pa(c2pa)
            .with_text("Author", "nobody")
            .with_compressed_text("Comment", "compressed comment")
            .with_international_text("Title", "international title")
            .with_gamma(1.0 / 2.2)
            // cICP rather than sRGB, which §5.6 Table 5 and §11.3.2.5 forbid beside the iCCP
            // this file also carries; sRGB's own carriage is pinned by `roundtrip.rs`.
            .with_cicp(9, 16, true)
            .with_chromaticities(
                (0.3127, 0.3290),
                (0.6400, 0.3300),
                (0.3000, 0.6000),
                (0.1500, 0.0600),
            )
    });

    let meta = gamut_png::metadata(&png).unwrap();
    assert_eq!(meta.exif.as_deref(), Some(exif.as_slice()));
    let profile = meta.icc_profile.as_ref().expect("iCCP present");
    assert_eq!(profile.name, "Tiny");
    assert_eq!(profile.profile, icc);
    assert_eq!(meta.xmp.as_deref(), Some(xmp.as_bytes()));
    assert_eq!(meta.c2pa.as_deref(), Some(&c2pa[..]));
    assert_eq!(meta.gamma, Some(45_455));
    let cicp = meta.cicp.expect("cICP present");
    assert_eq!(
        (
            cicp.color_primaries,
            cicp.transfer_function,
            cicp.matrix_coefficients,
            cicp.full_range
        ),
        (9, 16, 0, true)
    );
    let chrm = meta.chromaticities.expect("cHRM present");
    assert_eq!(chrm.white, (31_270, 32_900));
    assert_eq!(chrm.red, (64_000, 33_000));

    // All three text flavours arrive, and the XMP packet is not repeated among them.
    let keywords: Vec<&str> = meta.texts.iter().map(|t| t.keyword.as_str()).collect();
    assert_eq!(keywords, ["Author", "Comment", "Title"]);
    assert_eq!(meta.texts[1].text, "compressed comment");
}

/// The anti-drift assertion: both walks hand their CRC-valid ancillary chunks to the same
/// `decoded::collect`, which is the only place that decides what carries metadata — so the two
/// entry points must report the same thing for the same file. A chunk type wired into one walk
/// and not the other fails here.
#[test]
fn metadata_agrees_with_decode_field_for_field() {
    // Built chunk by chunk rather than by the encoder, so that *every* field is populated: the
    // encoder refuses sRGB beside iCCP (§5.6 Table 5, §11.3.2.5), and a comparison of two `None`s
    // would not see a chunk wired into one walk and not the other. A reader still meets such a
    // file, and §13.1 says an ancillary chunk it cannot use is skipped, not fatal.
    let exif = tiny_exif();
    let icc = tiny_icc_profile();
    let mut iccp = b"Tiny\0\0".to_vec();
    iccp.extend_from_slice(&zlib(&icc));
    let mut chrm = Vec::new();
    for coord in [
        31_270u32, 32_900, 64_000, 33_000, 30_000, 60_000, 15_000, 6_000,
    ] {
        chrm.extend_from_slice(&coord.to_be_bytes());
    }
    let png = png_from_chunks(&[
        chunk(b"IHDR", &ihdr_payload(3, 2, 8, 2, 0)),
        chunk(b"eXIf", &exif),
        chunk(b"iCCP", &iccp),
        chunk(b"sRGB", &[1]),
        chunk(b"cICP", &[1, 13, 0, 1]),
        chunk(b"gAMA", &45_455u32.to_be_bytes()),
        chunk(b"cHRM", &chrm),
        chunk(b"tEXt", b"Author\0nobody"),
        chunk(b"iTXt", b"XML:com.adobe.xmp\0\0\0\0\0<x:xmpmeta/>"),
        chunk(b"caBX", b"\0\0\0\x10jumbc2pa"),
        chunk(b"IDAT", &zlib(&[0u8; 20])),
        chunk(b"IEND", &[]),
    ]);

    let meta = gamut_png::metadata(&png).unwrap();
    let decoded = PngDecoder::new().decode(&png).unwrap();

    assert_eq!(meta.exif, decoded.exif);
    assert_eq!(meta.icc_profile, decoded.icc_profile);
    assert_eq!(meta.xmp, decoded.xmp);
    assert_eq!(meta.c2pa, decoded.c2pa);
    assert_eq!(meta.c2pa_ignored, decoded.c2pa_ignored);
    assert_eq!(meta.texts, decoded.texts);
    assert_eq!(meta.gamma, decoded.gamma);
    assert_eq!(meta.chromaticities, decoded.chromaticities);
    assert_eq!(meta.srgb, decoded.srgb);
    assert_eq!(meta.cicp, decoded.cicp);
    // A `None` on both sides would pass every comparison above, so pin that the file really did
    // carry each field.
    assert!(meta.exif.is_some() && meta.icc_profile.is_some() && meta.xmp.is_some());
    assert!(meta.c2pa.is_some() && !meta.texts.is_empty());
    assert!(meta.gamma.is_some() && meta.chromaticities.is_some());
    assert!(meta.srgb.is_some() && meta.cicp.is_some());
}

/// The probe case from #379: cICP is uncompressed, so a colour-space probe costs a chunk walk and
/// nothing more. Built by hand so the assertion reads the walk, not the encoder's own chunk.
#[test]
fn cicp_is_read_without_inflating_anything() {
    // BT.2020 primaries (9), PQ transfer (16), RGB matrix (0), full range.
    let png = png_from_chunks(&[
        chunk(b"IHDR", &ihdr_payload(3, 2, 8, 2, 0)),
        chunk(b"cICP", &[9, 16, 0, 1]),
        chunk(b"IDAT", &zlib(&[0u8; 20])),
        chunk(b"IEND", &[]),
    ]);

    let cicp = gamut_png::metadata(&png)
        .unwrap()
        .cicp
        .expect("cICP present");
    assert_eq!(cicp.color_primaries, 9);
    assert_eq!(cicp.transfer_function, 16);
    assert_eq!(cicp.matrix_coefficients, 0);
    assert!(cicp.full_range);
}

/// `metadata` does not require IDAT — pixels are not its business. `decode` still does, which is
/// the difference that lets this entry point serve a header-only probe.
#[test]
fn a_stream_without_idat_still_yields_metadata() {
    let exif = tiny_exif();
    let png = png_from_chunks(&[
        chunk(b"IHDR", &ihdr_payload(3, 2, 8, 2, 0)),
        chunk(b"eXIf", &exif),
        chunk(b"IEND", &[]),
    ]);

    assert_eq!(
        gamut_png::metadata(&png).unwrap().exif.as_deref(),
        Some(exif.as_slice())
    );
    assert!(
        PngDecoder::new().decode(&png).is_err(),
        "decode must still demand IDAT"
    );
}

/// The load-bearing claim of the whole change: pixels are never touched. An IDAT holding bytes
/// that are not a zlib stream at all cannot be inflated — yet the metadata reads fine, while
/// `decode` fails on exactly the work `metadata` skipped.
#[test]
fn corrupt_idat_does_not_affect_metadata() {
    let exif = tiny_exif();
    let png = png_from_chunks(&[
        chunk(b"IHDR", &ihdr_payload(3, 2, 8, 2, 0)),
        chunk(b"eXIf", &exif),
        chunk(b"IDAT", b"this is not a zlib stream"),
        chunk(b"IEND", &[]),
    ]);

    assert_eq!(
        gamut_png::metadata(&png).unwrap().exif.as_deref(),
        Some(exif.as_slice())
    );
    assert!(
        PngDecoder::new().decode(&png).is_err(),
        "decode must fail on an uninflatable IDAT"
    );
}

/// §13.1: an ancillary chunk whose CRC does not match is skipped, not an error — the same rule
/// `decode` follows.
#[test]
fn ancillary_crc_mismatch_skips_the_chunk() {
    let mut bad = chunk(b"eXIf", &tiny_exif());
    let last = bad.len() - 1;
    bad[last] ^= 0xFF; // corrupt the CRC, leaving framing intact

    let png = png_from_chunks(&[
        chunk(b"IHDR", &ihdr_payload(3, 2, 8, 2, 0)),
        bad,
        chunk(b"gAMA", &45_455u32.to_be_bytes()),
        chunk(b"IDAT", &zlib(&[0u8; 20])),
        chunk(b"IEND", &[]),
    ]);

    let meta = gamut_png::metadata(&png).unwrap();
    assert_eq!(meta.exif, None, "corrupt eXIf must be skipped");
    // The walk continues past it: the following chunk is still collected.
    assert_eq!(meta.gamma, Some(45_455));
}

/// `PngDecoder::metadata` exists so the inflation budget is settable; the free function cannot
/// carry one. A budget below the profile size skips the chunk rather than erroring, matching the
/// documented `with_max_metadata_bytes` semantics.
#[test]
fn the_decoder_entry_point_honours_the_metadata_budget() {
    let icc = tiny_icc_profile();
    let png = encode(|e| e.with_icc_profile("Tiny", &icc).with_gamma(1.0 / 2.2));

    let generous = PngDecoder::new().with_max_metadata_bytes(1 << 20);
    assert!(generous.metadata(&png).unwrap().icc_profile.is_some());

    let stingy = PngDecoder::new().with_max_metadata_bytes(8);
    let meta = stingy.metadata(&png).unwrap();
    assert!(
        meta.icc_profile.is_none(),
        "a profile past the budget is skipped"
    );
    // Skipping is not failing: uncompressed chunks are unaffected by the budget.
    assert_eq!(meta.gamma, Some(45_455));

    // The free function uses the 16 MiB default, so it still reads the profile.
    assert!(gamut_png::metadata(&png).unwrap().icc_profile.is_some());
}

#[test]
fn malformed_streams_are_rejected() {
    // Not a PNG at all.
    assert!(gamut_png::metadata(b"definitely not a PNG file").is_err());
    // Signature only.
    assert!(gamut_png::metadata(&common::SIGNATURE).is_err());
    // First chunk is not IHDR.
    let no_ihdr = png_from_chunks(&[
        chunk(b"gAMA", &45_455u32.to_be_bytes()),
        chunk(b"IEND", &[]),
    ]);
    assert!(gamut_png::metadata(&no_ihdr).is_err());
    // Truncated: no IEND.
    let no_iend = png_from_chunks(&[
        chunk(b"IHDR", &ihdr_payload(3, 2, 8, 2, 0)),
        chunk(b"eXIf", &tiny_exif()),
    ]);
    assert!(gamut_png::metadata(&no_iend).is_err());
    // A duplicate IHDR is malformed *input* — not merely an unrecognised critical chunk. The
    // kinds are asserted apart because a duplicate IHDR would otherwise fall through to the
    // unknown-critical arm and still "fail", masking the loss of its own rule.
    let dup_ihdr = png_from_chunks(&[
        chunk(b"IHDR", &ihdr_payload(3, 2, 8, 2, 0)),
        chunk(b"IHDR", &ihdr_payload(3, 2, 8, 2, 0)),
        chunk(b"IEND", &[]),
    ]);
    assert_eq!(
        gamut_png::metadata(&dup_ihdr).unwrap_err().kind(),
        ErrorKind::InvalidInput
    );
    // An unknown *critical* chunk (uppercase first byte) is unsupported, as in `decode`.
    let unknown_critical = png_from_chunks(&[
        chunk(b"IHDR", &ihdr_payload(3, 2, 8, 2, 0)),
        chunk(b"XxXx", &[0]),
        chunk(b"IEND", &[]),
    ]);
    assert_eq!(
        gamut_png::metadata(&unknown_critical).unwrap_err().kind(),
        ErrorKind::Unsupported
    );
    // A corrupt IHDR is rejected even though no pixels are read.
    let bad_ihdr = png_from_chunks(&[
        chunk(b"IHDR", &ihdr_payload(0, 0, 8, 2, 0)),
        chunk(b"IEND", &[]),
    ]);
    assert!(gamut_png::metadata(&bad_ihdr).is_err());
}

/// An unknown *ancillary* chunk is skipped rather than rejected, so an APNG (acTL/fcTL/fdAT) or a
/// vendor chunk reads its metadata normally — matching `decode`, which renders the default image.
#[test]
fn unknown_ancillary_chunks_are_ignored() {
    let png = png_from_chunks(&[
        chunk(b"IHDR", &ihdr_payload(3, 2, 8, 2, 0)),
        chunk(b"acTL", &[0, 0, 0, 2, 0, 0, 0, 0]),
        chunk(b"gAMA", &45_455u32.to_be_bytes()),
        chunk(b"vpAg", &[1, 2, 3]),
        chunk(b"IDAT", &zlib(&[0u8; 20])),
        chunk(b"IEND", &[]),
    ]);
    assert_eq!(gamut_png::metadata(&png).unwrap().gamma, Some(45_455));
}
