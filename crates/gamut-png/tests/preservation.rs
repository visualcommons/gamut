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
use gamut_png::{MetadataNotice, PngDecoder, PngEncoder, PngMetadata, SrgbIntent};

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
        chunk(b"iTXt", &compressed_xmp()),
        chunk(b"caBX", b"\0\0\0\x10jumbc2pa"),
    ];
    chunks.extend_from_slice(extra);
    chunks.push(chunk(b"IDAT", &zlib(&[0u8; 20])));
    chunks.push(chunk(b"IEND", &[]));
    png_from_chunks(&chunks)
}

/// The `iTXt` payload for a **compressed** XMP packet carrying both §11.3.3.4 fields.
///
/// §11.3.3.1 Table 21 recommends the null framing for XMP compliance — flag 0, both strings
/// empty — but recommends is all it does, and a provenance packet is exactly the payload a
/// writer compresses. The uncompressed fixture that stood here could not see the flag being
/// dropped, which is how a 57× inflation went unnoticed.
fn compressed_xmp() -> Vec<u8> {
    let mut itxt = b"XML:com.adobe.xmp\0\x01\0en\0Metadata\0".to_vec();
    itxt.extend_from_slice(&zlib(&xmp_packet()));
    itxt
}

/// A realistic XMP packet: repetitive RDF followed by the whitespace padding XMP Part 3
/// recommends so an in-place update can grow without rewriting the file. That padding is exactly
/// why a real packet is stored compressed, and exactly what a writer that loses the compression
/// flag puts back in full.
fn xmp_packet() -> Vec<u8> {
    let mut packet = XMP_RDF.as_bytes().to_vec();
    packet.resize(packet.len() + 3_072, b' ');
    packet.extend_from_slice(b"<?xpacket end='w'?>");
    packet
}

/// The RDF body of [`xmp_packet`].
const XMP_RDF: &str = concat!(
    "<?xpacket begin='' id='W5M0MpCehiHzreSzNTczkc9d'?>",
    "<x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:RDF ",
    "xmlns:rdf='http://www.w3.org/1999/02/22-rdf-syntax-ns#'>",
    "<rdf:Description rdf:about='' xmlns:dc='http://purl.org/dc/elements/1.1/'>",
    "<dc:title><rdf:Alt><rdf:li xml:lang='x-default'>a title</rdf:li></rdf:Alt></dc:title>",
    "<dc:creator><rdf:Seq><rdf:li>a creator</rdf:li></rdf:Seq></dc:creator>",
    "<dc:rights><rdf:Alt><rdf:li xml:lang='x-default'>a notice</rdf:li></rdf:Alt></dc:rights>",
    "<dc:description><rdf:Alt><rdf:li xml:lang='x-default'>a description</rdf:li></rdf:Alt>",
    "</dc:description></rdf:Description></rdf:RDF></x:xmpmeta>",
);

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
    assert_eq!(re.xmp_framing, meta.xmp_framing);
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
            .metadata_notices()
            .contains(&MetadataNotice::NonRgbCicp),
        "{:?}",
        encoder.metadata_notices()
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
        encoder.metadata_notices(),
        [MetadataNotice::C2paManifestStore]
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

/// The same claim for the XMP packet, which is where it was untrue: the packet leaves the read
/// side through its own field, so the `iTXt` framing that field does *not* hold — §11.3.3.4's
/// compression flag above all — has to travel beside it or be invented at the writer.
///
/// A provenance packet is exactly the payload a writer compresses, and rewriting one
/// uncompressed inflates it by a factor a user notices. Kills a mutant that ignores
/// [`XmpFraming::compressed`](gamut_png::XmpFraming::compressed).
#[test]
fn a_compressed_xmp_packet_goes_back_into_a_compressed_itxt() {
    let meta = gamut_png::metadata(&minimal_source(&[chunk(b"iTXt", &compressed_xmp())])).unwrap();
    assert_eq!(meta.xmp, Some(xmp_packet()));

    let out = re_encoded_bytes(|e| e.with_metadata(&meta));
    let carried = chunk_payload(&out, b"iTXt").expect("the packet");
    // keyword, NUL, then §11.3.3.4's compression flag.
    assert_eq!(carried[18], 1, "the flag is set: {carried:?}");

    // Measured against the same carry with the flag cleared, so the claim is the inflation the
    // flag prevents rather than a threshold that happens to hold for this packet.
    let mut flat = meta.clone();
    flat.xmp_framing.as_mut().expect("framed").compressed = false;
    let flat_out = re_encoded_bytes(|e| e.with_metadata(&flat));
    let inflated = chunk_payload(&flat_out, b"iTXt").expect("the packet");
    assert!(
        carried.len() * 2 < inflated.len(),
        "{} bytes compressed against {} uncompressed",
        carried.len(),
        inflated.len()
    );
}

/// §11.3.3.4's language tag and translated keyword are as much a part of the XMP chunk as of any
/// other `iTXt`, and §11.3.3.1 Table 21 only *recommends* leaving them empty. A file that fills
/// them is a file whose bytes have to come back.
///
/// Separate from the compression claim above because a writer can keep the flag and still drop
/// the two strings — the defect this pins was exactly that pair going missing together.
#[test]
fn an_xmp_packet_keeps_its_language_and_translated_keyword() {
    let meta = gamut_png::metadata(&source(&[])).unwrap();
    let framing = meta.xmp_framing.clone().expect("the source frames it");
    assert_eq!(framing.language.as_deref(), Some("en"));
    assert_eq!(framing.translated_keyword.as_deref(), Some("Metadata"));
    assert!(framing.compressed);

    let re = re_encoded(|e| e.with_metadata(&meta));
    assert_eq!(re.xmp_framing, Some(framing));
}

/// A PNG carries one XMP packet, so the encoder's packet is a single-value payload like `eXIf` or
/// `iCCP`: setting it again replaces it. Appending instead wrote two `iTXt` chunks under the one
/// keyword §11.3.3.1 Table 21 reserves, and this crate's reader keeps the *first* — so the packet
/// a caller carried in was the one silently discarded, inside the feature built to end silent
/// discarding.
#[test]
fn carrying_an_xmp_packet_replaces_one_already_set() {
    let meta = gamut_png::metadata(&source(&[])).unwrap();
    let out = re_encoded_bytes(|e| {
        e.with_xmp("<x:xmpmeta id='set directly'/>")
            .with_metadata(&meta)
    });

    let mut packets = 0;
    let mut i = 8;
    while i + 12 <= out.len() {
        let len = u32::from_be_bytes([out[i], out[i + 1], out[i + 2], out[i + 3]]) as usize;
        if &out[i + 4..i + 8] == b"iTXt" && out[i + 8..].starts_with(b"XML:com.adobe.xmp\0") {
            packets += 1;
        }
        i += 12 + len;
    }
    assert_eq!(packets, 1, "one keyword, one chunk");
    assert_eq!(
        gamut_png::metadata(&out).unwrap().xmp,
        meta.xmp,
        "and it is the carried packet, not the one it replaced"
    );
}

/// §11.3.3.1's keyword *shape* rules are lowercase throughout — "Keywords shall contain only
/// printable Latin-1", "leading spaces, trailing spaces, and consecutive spaces are not
/// permitted" — and §15 gives the BCP 14 keywords force "when, and only when, they appear in all
/// capitals". This crate's reader accepts every one of these keywords and returns them
/// unchanged, so refusing to write them back would fail a conversion over a file whose pixels
/// are fine, leaving no escape but to discard the file's metadata entirely.
///
/// So they are written verbatim and reported. Kills a mutant that turns any of these back into a
/// refusal, or that drops the annotation instead of writing it.
#[test]
fn a_keyword_the_reader_accepts_survives_the_re_encode_with_a_notice() {
    for (keyword, notice) in [
        (" Author", MetadataNotice::TextKeywordSpacing),
        ("Author ", MetadataNotice::TextKeywordSpacing),
        ("Two  Words", MetadataNotice::TextKeywordSpacing),
        ("Auth\u{7F}or", MetadataNotice::TextKeywordRepertoire),
        ("Auth\u{A0}or", MetadataNotice::TextKeywordRepertoire),
    ] {
        let mut text = keyword.as_bytes().to_vec();
        text.extend_from_slice(b"\0body");
        let png = minimal_source(&[chunk(b"tEXt", &text)]);
        let meta = gamut_png::metadata(&png).unwrap();
        assert_eq!(meta.texts.len(), 1, "the reader accepts {keyword:?}");

        let encoder = PngEncoder::new().with_metadata(&meta);
        assert_eq!(encoder.metadata_notices(), [notice], "keyword {keyword:?}");
        let re = re_encoded(|_| encoder.clone());
        assert_eq!(re.texts, meta.texts, "keyword {keyword:?} came back whole");
    }
}

/// The other half of the same line: a keyword no chunk can hold is left behind rather than
/// written, because all three text chunks fix that field at 1–79 Latin-1 bytes and a reader —
/// this crate's own included — drops a chunk whose keyword busts it. Writing it would be the
/// silent loss, so the annotation goes and the notice stays.
///
/// Driven through the setters, because the reader will not produce such a keyword from a file.
#[test]
fn a_keyword_no_chunk_can_hold_is_left_behind_with_a_notice() {
    for (keyword, notice) in [
        ("", MetadataNotice::TextKeywordLength),
        (&"k".repeat(80), MetadataNotice::TextKeywordLength),
        ("题", MetadataNotice::TextKeywordNotLatin1),
    ] {
        let encoder = PngEncoder::new().with_text(keyword, "body");
        assert_eq!(
            encoder.metadata_notices(),
            [notice],
            "keyword of {} chars",
            keyword.chars().count()
        );
        assert!(
            re_encoded(|_| encoder.clone()).texts.is_empty(),
            "keyword of {} chars was not written",
            keyword.chars().count()
        );
    }
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
/// reports what the file held — so the write side is where it has to be said out loud. It is
/// **reported, not refused**: the pixels of such a file are fine, and failing the whole encode
/// would leave a caller no way to convert it but to discard its ICC profile too.
#[test]
fn a_non_utf8_xmp_packet_is_reported_and_the_re_encode_proceeds() {
    let mut itxt = b"XML:com.adobe.xmp\0\0\0\0\0".to_vec();
    itxt.extend_from_slice(b"<x:xmpmeta \xFF\xFE/>");
    let meta = gamut_png::metadata(&minimal_source(&[chunk(b"iTXt", &itxt)])).unwrap();
    assert!(meta.xmp.is_some(), "the read side surfaces the raw packet");

    let encoder = PngEncoder::new().with_metadata(&meta);
    assert_eq!(encoder.metadata_notices(), [MetadataNotice::XmpNotUtf8]);
    assert!(
        !MetadataNotice::XmpNotUtf8.carried(),
        "the packet is left behind, not written"
    );
    assert!(re_encoded(|_| encoder.clone()).xmp.is_none());
}

/// A null in a text *string* is a shape this crate's own reader hands back: §11.3.3.2 makes the
/// text last and length-delimited ("The text string is not null-terminated (the length of the
/// chunk defines the ending)"), so the reader stops at the keyword's null and everything after
/// it — later nulls included — is the text. Refusing to write it back would fail a re-encode on
/// a file this crate decoded without complaint, which is the failure the notice channel exists to
/// end. It is not written either: libpng truncates such a text at the null, so the chunk would
/// hold different annotations for different readers. Dropped, and named.
#[test]
fn a_null_in_a_carried_text_string_is_dropped_with_a_notice() {
    let png = minimal_source(&[chunk(b"tEXt", b"Comment\0val\0ue")]);
    let meta = gamut_png::metadata(&png).unwrap();
    assert_eq!(
        meta.texts.first().map(|t| t.text.as_str()),
        Some("val\0ue"),
        "the reader hands the null back"
    );

    let encoder = PngEncoder::new().with_metadata(&meta);
    assert_eq!(
        encoder.metadata_notices(),
        [MetadataNotice::TextStringNull],
        "named, not refused"
    );
    assert!(
        re_encoded(|_| encoder.clone()).texts.is_empty(),
        "and not written"
    );
}

/// The null that still refuses is the one in a *keyword*: all three chunks are framed "Keyword …
/// Null separator …", so `Auth\0or` re-parses as the annotation `Auth` with `or` for its text and
/// the file means something the caller never supplied.
///
/// Built through the setter, which is the only way in — the reader splits a chunk at its first
/// null and never returns a keyword holding one. Kills a mutant that drops the accumulated
/// annotations' validation from the encode path.
#[test]
fn a_null_in_a_carried_keyword_refuses_the_re_encode() {
    let pixels = vec![0u8; 3 * 4];
    let image = ImageRef::<Rgb8>::new(&pixels, Dimensions::new(2, 2).unwrap()).unwrap();
    let error = PngEncoder::new()
        .with_text("Auth\0or", "body")
        .encode_to_vec(image)
        .expect_err("refused");
    assert_eq!(error.kind(), ErrorKind::InvalidInput);
    assert!(
        error
            .to_string()
            .contains("may not contain a null character"),
        "{error}"
    );
}

/// Naming a payload is only useful if the name says something. `gamut convert` prints these
/// lines and they are the whole of what a user learns about metadata that did not survive
/// intact, so each has to identify the payload and give the reason.
///
/// Pinned here rather than in `gamut-cli`, whose tests the mutation gate cannot see: a mutant
/// that empties [`MetadataNotice::reason`] or its `Display` would otherwise leave the command
/// printing nothing at all.
#[test]
fn a_notice_names_its_payload_in_words() {
    let store = MetadataNotice::C2paManifestStore.to_string();
    assert!(store.contains("C2PA manifest store"), "{store}");
    assert!(store.contains("re-sign"), "{store}");

    let cicp = MetadataNotice::NonRgbCicp.to_string();
    assert!(cicp.contains("cICP"), "{cicp}");
    assert!(cicp.contains("matrix coefficients"), "{cicp}");
    assert_eq!(
        cicp,
        MetadataNotice::NonRgbCicp.reason(),
        "Display is the reason"
    );
}

/// The whole point of the channel is that "it did not come along" and "it came along bent" are
/// different news for a user, so [`MetadataNotice::carried`] has to separate them — and it is
/// the only thing that does.
///
/// Kills a mutant that makes `carried` constant either way, which would have `gamut convert`
/// telling a user their ICC profile was dropped when it was not.
#[test]
fn a_notice_says_whether_the_payload_reached_the_output() {
    for carried in [
        MetadataNotice::TextKeywordRepertoire,
        MetadataNotice::TextKeywordSpacing,
        MetadataNotice::ItxtLanguageTag,
    ] {
        assert!(carried.carried(), "{carried:?}");
    }
    for lost in [
        MetadataNotice::NonRgbCicp,
        MetadataNotice::C2paManifestStore,
        MetadataNotice::TextKeywordNotLatin1,
        MetadataNotice::TextKeywordLength,
        MetadataNotice::XmpNotUtf8,
        MetadataNotice::TextStringNull,
    ] {
        assert!(!lost.carried(), "{lost:?}");
    }
}
