//! `PngEncoder::with_metadata` / `with_metadata_from` (issue #483): what a re-encode carries
//! forward from the file it rewrites, and what it deliberately does not.
//!
//! Example and drift-guard level, over gamut's own read→write seam. The source files are built
//! chunk by chunk from `common` so a fixture can carry exactly the combination each claim is
//! about, without the encoder's own choices standing in the way. That a re-encode's output is a
//! file the *reference* reader accepts is `tests/oracle.rs`'s job, not this file's.

mod common;

use common::{chunk, ihdr_payload, png_from_chunks, tiny_exif, tiny_icc_profile, zlib};
use gamut_core::{Dimensions, EncodeImage, ErrorKind, ImageRef, Rgb8};
use gamut_png::{DroppedMetadata, PngDecoder, PngEncoder, PngMetadata, SrgbIntent};

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

/// A source carrying only `extra` between the header and the image data — for a claim about one
/// annotation, which the full [`source`] pile would confuse with its own.
fn minimal_source(extra: &[Vec<u8>]) -> Vec<u8> {
    let mut chunks = vec![chunk(b"IHDR", &ihdr_payload(3, 2, 8, 2, 0))];
    chunks.extend_from_slice(extra);
    chunks.push(chunk(b"IDAT", &zlib(&[0u8; 20])));
    chunks.push(chunk(b"IEND", &[]));
    png_from_chunks(&chunks)
}

/// Re-encodes a 2×2 image under `build`, returning the output bytes.
fn re_encoded_bytes(build: impl FnOnce(PngEncoder) -> PngEncoder) -> Vec<u8> {
    let pixels = vec![0u8; 3 * 4];
    let image = ImageRef::<Rgb8>::new(&pixels, Dimensions::new(2, 2).unwrap()).unwrap();
    build(PngEncoder::new())
        .encode_to_vec(image)
        .expect("re-encode")
}

/// Re-encodes a 2×2 image under `build`, and reads back what the output carries.
fn re_encoded(build: impl FnOnce(PngEncoder) -> PngEncoder) -> PngMetadata {
    gamut_png::metadata(&re_encoded_bytes(build)).expect("read back")
}

/// The payload of the first chunk of type `ty`, for a claim about which *chunk* carries an
/// annotation rather than what text it holds — the distinction a decode erases.
fn chunk_payload(png: &[u8], ty: &[u8; 4]) -> Option<Vec<u8>> {
    let mut i = 8; // past the signature
    while i + 12 <= png.len() {
        let len = u32::from_be_bytes([png[i], png[i + 1], png[i + 2], png[i + 3]]) as usize;
        if &png[i + 4..i + 8] == ty {
            return Some(png[i + 8..i + 8 + len].to_vec());
        }
        i += 12 + len;
    }
    None
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

/// A source may legally carry both, and both are kept. §5.6 Table 5 and §11.3.2.5 say only that
/// `sRGB` and `iCCP` "should not" appear together — lowercase, and §15 gives the BCP 14 keywords
/// force "when, and only when, they appear in all capitals" — while §4.3 Table 1 presupposes the
/// pair and ranks it, `iCCP` (2) over `sRGB` (3). Dropping either would lose colour information
/// the source carried, which is exactly what this preservation path exists to stop.
///
/// That the result is a file the reference reader accepts is pinned against libpng in
/// `tests/oracle.rs`.
#[test]
fn a_profile_and_a_rendering_intent_are_both_carried() {
    let meta = gamut_png::metadata(&source(&[chunk(b"sRGB", &[1])])).unwrap();
    assert_eq!(meta.srgb, Some(SrgbIntent::RelativeColorimetric));
    assert!(meta.icc_profile.is_some(), "the source carries both");

    let re = re_encoded(|e| e.with_metadata(&meta));
    assert_eq!(re.icc_profile, meta.icc_profile);
    assert_eq!(re.srgb, meta.srgb);
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
    let encoder = PngEncoder::new().with_metadata(&non_rgb);
    assert!(re_encoded(|_| encoder.clone()).cicp.is_none());
    // Dropped, but not in silence: the caller can say so.
    assert!(
        encoder
            .dropped_metadata()
            .contains(&DroppedMetadata::NonRgbCicp),
        "{:?}",
        encoder.dropped_metadata()
    );
}

/// Drift guard. A C2PA manifest store is signed over the exact bytes of the file it was made for,
/// which is why C2PA 2.4 §A.3.2 marks `caBX` unsafe to copy: carried into a re-encode it is
/// invalid by construction, and a validator would report a tampered file rather than an unsigned
/// one. This asserts the omission is deliberate, because adding one line would undo it silently.
#[test]
fn the_c2pa_manifest_store_is_never_carried_forward() {
    let meta = gamut_png::metadata(&source(&[])).unwrap();
    assert!(meta.c2pa.is_some(), "the source carries a store");

    let encoder = PngEncoder::new().with_metadata(&meta);
    assert!(re_encoded(|_| encoder.clone()).c2pa.is_none());
    assert_eq!(
        encoder.dropped_metadata(),
        [DroppedMetadata::C2paManifestStore]
    );
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

/// §11.3.3.3 makes a `zTXt` "semantically equivalent" to a `tEXt`, so a decode that keeps only the
/// text loses no *words* — but rewriting a compressed annotation uncompressed is still not
/// preservation: the fixture's 1 600-byte body is a 40-byte chunk in the source, and a re-encode
/// that forgets which chunk it came from writes it back forty times larger.
///
/// Kills the `CompressedText` arm of `with_metadata_view`'s routing, and any mutant that collapses
/// [`TextChunkKind`](gamut_png::TextChunkKind) to one value.
#[test]
fn a_compressed_annotation_goes_back_into_a_compressed_chunk() {
    let body = "the quick brown fox ".repeat(80);
    let mut ztxt = b"Comment\0\0".to_vec();
    ztxt.extend_from_slice(&zlib(body.as_bytes()));
    let meta = gamut_png::metadata(&minimal_source(&[chunk(b"zTXt", &ztxt)])).unwrap();

    let out = re_encoded_bytes(|e| e.with_metadata(&meta));
    let carried = chunk_payload(&out, b"zTXt").expect("carried as zTXt");
    assert!(
        chunk_payload(&out, b"tEXt").is_none(),
        "not inflated to tEXt"
    );
    assert!(
        carried.len() < body.len() / 4,
        "still compressed: {} bytes for a {}-byte body",
        carried.len(),
        body.len()
    );
}

/// The same claim for the compression flag §11.3.3.4 gives `iTXt`: a compressed international
/// annotation stays compressed, and keeps the language tag and translated keyword that a plain
/// `iTXt` rewrite would have kept but a `tEXt` rewrite would have dropped.
///
/// Kills the `CompressedInternational` arm of `with_metadata_view`'s routing.
#[test]
fn a_compressed_itxt_goes_back_into_a_compressed_itxt() {
    let body = "gämut ".repeat(200);
    let mut itxt = b"Note\0\x01\0de\0Notiz\0".to_vec();
    itxt.extend_from_slice(&zlib(body.as_bytes()));
    let meta = gamut_png::metadata(&minimal_source(&[chunk(b"iTXt", &itxt)])).unwrap();

    let out = re_encoded_bytes(|e| e.with_metadata(&meta));
    let note = chunk_payload(&out, b"iTXt").expect("the Note annotation");
    // keyword, NUL, compression flag 1, method 0, language, NUL, translated keyword, NUL.
    assert!(note.starts_with(b"Note\0\x01\0de\0Notiz\0"), "{note:?}");
    assert!(
        note.len() < body.len() / 4,
        "still compressed: {} bytes",
        note.len()
    );
}

/// Carrying the same metadata twice is carrying it once. The single-value slots are idempotent
/// because a second write overwrites the first; the text list is the one place where a second
/// call would otherwise append a duplicate of every annotation — which is what a caller that
/// builds an encoder in a loop, or reuses one across files, would get.
#[test]
fn carrying_the_same_metadata_twice_carries_it_once() {
    let meta = gamut_png::metadata(&source(&[])).unwrap();

    let once = re_encoded(|e| e.with_metadata(&meta));
    let twice = re_encoded(|e| e.with_metadata(&meta).with_metadata(&meta));
    assert_eq!(once, twice);
    assert_eq!(once.texts.len(), 2, "the fixture carries two annotations");
}

/// §11.3.3.4 gives the `iTXt` text field UTF-8 and no alternative, so a packet that is not UTF-8
/// has no chunk this encoder can frame. The read side hands it over as raw bytes regardless — it
/// reports what the file held — so the write side is where it has to be said out loud. Refusing
/// is the point: the alternative is a caller who asked for preservation and got a file with the
/// packet missing and nothing to read about it.
#[test]
fn a_non_utf8_xmp_packet_refuses_the_re_encode() {
    let mut itxt = b"XML:com.adobe.xmp\0\0\0\0\0".to_vec();
    itxt.extend_from_slice(b"<x:xmpmeta \xFF\xFE/>");
    let meta = gamut_png::metadata(&minimal_source(&[chunk(b"iTXt", &itxt)])).unwrap();
    assert!(meta.xmp.is_some(), "the read side surfaces the raw packet");

    let pixels = vec![0u8; 3 * 4];
    let image = ImageRef::<Rgb8>::new(&pixels, Dimensions::new(2, 2).unwrap()).unwrap();
    let error = PngEncoder::new()
        .with_metadata(&meta)
        .encode_to_vec(image)
        .expect_err("refused");
    assert_eq!(error.kind(), ErrorKind::InvalidInput);
    assert!(
        error.to_string().contains("XMP packet is not UTF-8"),
        "{error}"
    );
}

/// Naming a dropped payload is only useful if the name says something. `gamut convert` prints
/// these lines and they are the whole of what a user learns about metadata that did not survive,
/// so each has to identify the payload and give the reason it could not come along.
///
/// Pinned here rather than in `gamut-cli`, whose tests the mutation gate cannot see: a mutant
/// that empties [`DroppedMetadata::reason`] or its `Display` would otherwise leave the command
/// printing nothing at all.
#[test]
fn a_dropped_payload_is_named_in_words() {
    let store = DroppedMetadata::C2paManifestStore.to_string();
    assert!(store.contains("C2PA manifest store"), "{store}");
    assert!(store.contains("re-sign"), "{store}");

    let cicp = DroppedMetadata::NonRgbCicp.to_string();
    assert!(cicp.contains("cICP"), "{cicp}");
    assert!(cicp.contains("matrix coefficients"), "{cicp}");
    assert_eq!(
        cicp,
        DroppedMetadata::NonRgbCicp.reason(),
        "Display is the reason"
    );
}
