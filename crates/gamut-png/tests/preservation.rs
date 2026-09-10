//! `PngEncoder::with_metadata` / `with_metadata_from` (issue #483): what a re-encode carries
//! forward from the file it rewrites, and what it deliberately does not.
//!
//! Example and drift-guard level. No oracle: the claim is about gamut's own read→write seam, and
//! the source files are built chunk by chunk from `common` so a fixture can carry combinations
//! this encoder refuses to write — notably `sRGB` beside `iCCP`, which §5.6 Table 5 and §11.3.2.5
//! tell encoders not to produce but which a reader still meets.

mod common;

use common::{chunk, ihdr_payload, png_from_chunks, tiny_exif, tiny_icc_profile, zlib};
use gamut_core::{Dimensions, EncodeImage, ImageRef, Rgb8};
use gamut_png::{PngDecoder, PngEncoder, PngMetadata, SrgbIntent};

/// The `cHRM` payload for the sRGB primaries, in the ×100 000 units §11.3.2.1 stores.
const CHRM: [u32; 8] = [
    31_270, 32_900, 64_000, 33_000, 30_000, 60_000, 15_000, 6_000,
];

/// A source file carrying every metadata chunk the read side surfaces, built by hand.
///
/// `extra` is appended before `IDAT`, so a case can add or replace a colour chunk without
/// rebuilding the pile.
fn source(extra: &[Vec<u8>]) -> Vec<u8> {
    let mut iccp = b"Tiny\0\0".to_vec();
    iccp.extend_from_slice(&zlib(&tiny_icc_profile()));
    let mut chrm = Vec::new();
    for coord in CHRM {
        chrm.extend_from_slice(&coord.to_be_bytes());
    }
    let mut chunks = vec![
        chunk(b"IHDR", &ihdr_payload(3, 2, 8, 2, 0)),
        chunk(b"eXIf", &tiny_exif()),
        chunk(b"iCCP", &iccp),
        chunk(b"gAMA", &45_455u32.to_be_bytes()),
        chunk(b"cHRM", &chrm),
        chunk(b"tEXt", b"Author\0caf\xE9"),
        chunk(b"iTXt", b"Note\0\0\0de\0Notiz\0g\xC3\xA4mut"),
        chunk(b"iTXt", b"XML:com.adobe.xmp\0\0\0\0\0<x:xmpmeta/>"),
        chunk(b"caBX", b"\0\0\0\x10jumbc2pa"),
    ];
    chunks.extend_from_slice(extra);
    chunks.push(chunk(b"IDAT", &zlib(&[0u8; 20])));
    chunks.push(chunk(b"IEND", &[]));
    png_from_chunks(&chunks)
}

/// Re-encodes a 2×2 image under `build`, and reads back what the output carries.
fn re_encoded(build: impl FnOnce(PngEncoder) -> PngEncoder) -> PngMetadata {
    let pixels = vec![0u8; 3 * 4];
    let image = ImageRef::<Rgb8>::new(&pixels, Dimensions::new(2, 2).unwrap()).unwrap();
    let png = build(PngEncoder::new())
        .encode_to_vec(image)
        .expect("re-encode");
    gamut_png::metadata(&png).expect("read back")
}

/// The headline claim of #483: nothing the read side surfaced is dropped on the way back out.
/// Before this, `gamut convert`'s PNG path round-tripped 0% of it.
#[test]
fn every_carried_chunk_survives_a_re_encode() {
    let meta = gamut_png::metadata(&source(&[])).unwrap();
    let re = re_encoded(|e| e.with_metadata(&meta));

    assert_eq!(re.exif, meta.exif);
    assert_eq!(re.icc_profile, meta.icc_profile);
    assert_eq!(re.xmp, meta.xmp);
    assert_eq!(re.gamma, Some(45_455));
    let chrm = re.chromaticities.expect("cHRM carried");
    assert_eq!(
        (chrm.white, chrm.blue),
        ((CHRM[0], CHRM[1]), (CHRM[6], CHRM[7]))
    );
    // Both text annotations, in file order, with the Latin-1 `é` intact.
    let texts: Vec<(&str, &str)> = re
        .texts
        .iter()
        .map(|t| (t.keyword.as_str(), t.text.as_str()))
        .collect();
    assert_eq!(texts, [("Author", "café"), ("Note", "gämut")]);
}

/// §11.3.3.4's language tag and translated keyword are what make an `iTXt` international; a
/// re-encode that reduced every annotation to a bare keyword and text would silently strip them.
#[test]
fn an_itxt_keeps_its_language_and_translated_keyword() {
    let meta = gamut_png::metadata(&source(&[])).unwrap();
    let re = re_encoded(|e| e.with_metadata(&meta));

    let note = re.texts.iter().find(|t| t.keyword == "Note").expect("Note");
    assert_eq!(note.language.as_deref(), Some("de"));
    assert_eq!(note.translated_keyword.as_deref(), Some("Notiz"));
}

/// §4.3 Table 1 ranks the colour chunks and a reader honours the lowest priority number, so of a
/// source carrying both the `iCCP` (2) is the chunk that was being used and the `sRGB` (3) the
/// chunk that was being ignored. Carrying both would be the pair §5.6 Table 5 and §11.3.2.5
/// refuse, and would make the file unencodable.
#[test]
fn srgb_gives_way_to_an_icc_profile_from_the_same_file() {
    let meta = gamut_png::metadata(&source(&[chunk(b"sRGB", &[1])])).unwrap();
    assert_eq!(meta.srgb, Some(SrgbIntent::RelativeColorimetric));
    assert!(meta.icc_profile.is_some(), "the source carries both");

    let re = re_encoded(|e| e.with_metadata(&meta));
    assert!(re.icc_profile.is_some(), "the ICC profile is kept");
    assert!(re.srgb.is_none(), "the lower-priority sRGB is dropped");
}

/// The converse: with no ICC profile to outrank it, the rendering intent is the colour
/// information the file has, and dropping it would lose it.
#[test]
fn srgb_is_carried_when_no_icc_profile_outranks_it() {
    let png = png_from_chunks(&[
        chunk(b"IHDR", &ihdr_payload(3, 2, 8, 2, 0)),
        chunk(b"sRGB", &[2]),
        chunk(b"IDAT", &zlib(&[0u8; 20])),
        chunk(b"IEND", &[]),
    ]);
    let meta = gamut_png::metadata(&png).unwrap();

    let re = re_encoded(|e| e.with_metadata(&meta));
    assert_eq!(re.srgb, Some(SrgbIntent::Saturation));
}

/// §11.3.2.6: "RGB is currently the only supported color model in PNG, and as such Matrix
/// Coefficients shall be set to 0." A source chunk that says otherwise is not a conforming cICP,
/// so it is dropped rather than reproduced — while a conforming one is carried, which matters
/// because §4.3 Table 1 makes cICP the *highest*-precedence colour chunk.
#[test]
fn a_cicp_is_carried_only_when_its_matrix_coefficients_are_zero() {
    let conforming = gamut_png::metadata(&source(&[chunk(b"cICP", &[9, 16, 0, 1])])).unwrap();
    let carried = re_encoded(|e| e.with_metadata(&conforming))
        .cicp
        .expect("cICP carried");
    assert_eq!(
        (
            carried.color_primaries,
            carried.transfer_function,
            carried.matrix_coefficients,
            carried.full_range
        ),
        (9, 16, 0, true)
    );

    let non_rgb = gamut_png::metadata(&source(&[chunk(b"cICP", &[9, 16, 1, 1])])).unwrap();
    assert!(non_rgb.cicp.is_some(), "the source carries it");
    assert!(re_encoded(|e| e.with_metadata(&non_rgb)).cicp.is_none());
}

/// Drift guard. A C2PA manifest store is signed over the exact bytes of the file it was made for,
/// which is why C2PA 2.4 §A.3.2 marks `caBX` unsafe to copy: carried into a re-encode it is
/// invalid by construction, and a validator would report a tampered file rather than an unsigned
/// one. This asserts the omission is deliberate, because adding one line would undo it silently.
#[test]
fn the_c2pa_manifest_store_is_never_carried_forward() {
    let meta = gamut_png::metadata(&source(&[])).unwrap();
    assert!(meta.c2pa.is_some(), "the source carries a store");

    assert!(re_encoded(|e| e.with_metadata(&meta)).c2pa.is_none());
}

/// The two entry points differ only in which read surface they take, so a field wired into one
/// and not the other is a bug this catches — the same anti-drift shape `tests/metadata.rs` uses
/// for the two *read* walks.
#[test]
fn with_metadata_from_agrees_with_with_metadata() {
    let png = source(&[chunk(b"cICP", &[9, 16, 0, 1])]);
    let decoded = PngDecoder::new().decode(&png).unwrap();
    let meta = gamut_png::metadata(&png).unwrap();

    let from_decoded = re_encoded(|e| e.with_metadata_from(&decoded));
    let from_metadata = re_encoded(|e| e.with_metadata(&meta));
    assert_eq!(from_decoded, from_metadata);
    // A pair of empty results would satisfy the comparison above.
    assert!(from_decoded.icc_profile.is_some() && from_decoded.cicp.is_some());
    assert!(!from_decoded.texts.is_empty() && from_decoded.exif.is_some());
}
