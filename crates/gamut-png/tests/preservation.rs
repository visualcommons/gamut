//! `PngEncoder::with_png_metadata` / `with_metadata_from` (issue #483): what a re-encode carries
//! forward from the file it rewrites, and what it deliberately does not.
//!
//! Example and drift-guard level, over gamut's own read→write seam. The source files are built
//! chunk by chunk from `common` so a fixture can carry exactly the combination each claim is
//! about, without the encoder's own choices standing in the way.
//!
//! gamut reading its own output back is a round trip, which cannot see a defect symmetric across
//! this crate's reader and writer — so the claims that matter most are also asserted through the
//! *reference* reader: `libpng_oracle::decode` reports which chunk carried each payload and what
//! bytes libpng read out of it (issue #572).

mod common;

use common::{chunk, ihdr_payload, png_from_chunks, tiny_exif, tiny_icc_profile, zlib};
use gamut_core::{Dimensions, EncodeImage, ErrorKind, ImageRef, Rgb8};
use gamut_png::{
    Background, MetadataNotice, PngDecoder, PngEncoder, PngImage, PngMetadata, SrgbIntent,
    TextChunkKind,
};
use libpng_oracle::{OracleText, OracleTextKind};

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
    iccp.extend_from_slice(&zlib(&icc_profile()));
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

/// [`tiny_icc_profile`] grown by 128 bytes that do not deflate away, so the reference reader sees
/// it: libpng 1.6.43 drops an `iCCP` chunk shorter than 92 bytes as "too short"
/// (`libpng_oracle::DecodedImage::iccp`), which the tiny profile's chunk is.
fn icc_profile() -> Vec<u8> {
    let mut icc = tiny_icc_profile();
    icc.extend((0u32..128).map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8));
    let size = icc.len() as u32;
    icc[0..4].copy_from_slice(&size.to_be_bytes());
    icc
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
    let re = re_encoded(|e| e.with_png_metadata(&meta));

    assert_eq!(re.exif, meta.exif);
    assert_eq!(re.icc_profile, meta.icc_profile);
    assert_eq!(re.gamma, Some(45_455));
    let chrm = re.chromaticities.expect("cHRM carried");
    assert_eq!(
        (chrm.white, chrm.blue),
        ((CHRM[0], CHRM[1]), (CHRM[6], CHRM[7]))
    );
    // Every text annotation, in file order, with the Latin-1 `é` intact — the XMP packet last
    // among them, where the source put it, with its compression flag and both §11.3.3.4 tags.
    let texts: Vec<(&str, &str)> = re
        .texts
        .iter()
        .map(|t| (t.keyword.as_str(), t.text.as_str()))
        .collect();
    let packet = String::from_utf8(xmp_packet()).unwrap();
    assert_eq!(
        texts,
        [
            ("Author", "café"),
            ("Note", "gämut"),
            ("XML:com.adobe.xmp", packet.as_str())
        ]
    );
    assert_eq!(re.texts, meta.texts);
}

/// The headline claim again, read by **libpng** rather than by this crate: every payload the source
/// carried is in the re-encode, in the chunk the source carried it in, with the bytes it held —
/// the Latin-1 `é` in a `tEXt`, the `iTXt` language tag, translated keyword and compression flag,
/// the `cICP` libpng keeps as an unknown chunk — and the C2PA store is not.
///
/// Kills a defect symmetric across this crate's reader and writer, which the round trip above
/// agrees with by construction.
#[test]
fn the_reference_reader_sees_every_carried_chunk() {
    let meta = gamut_png::metadata(&source(&[chunk(b"cICP", &[9, 16, 0, 1])])).unwrap();
    let dec = libpng_oracle::decode(&re_encoded_bytes(|e| e.with_png_metadata(&meta)));

    assert_eq!(dec.warnings, 0, "libpng dropped nothing");
    assert_eq!(dec.exif, Some(tiny_exif()));
    assert_eq!(dec.iccp, Some(("Tiny".to_string(), icc_profile())));
    assert_eq!(dec.gamma, Some(45_455));
    assert_eq!(dec.chromaticities, Some(CHRM));
    assert_eq!(dec.srgb, None);
    assert_eq!(
        dec.unknown_chunks,
        [(*b"cICP", vec![9, 16, 0, 1])],
        "cICP carried, caBX not"
    );

    let mut texts = dec.texts;
    texts.sort_by(|a, b| a.keyword.cmp(&b.keyword));
    let text = |keyword: &str, text: &str, kind| OracleText {
        keyword: keyword.into(),
        text: text.into(),
        kind,
    };
    let itxt = |language: &str, translated: &str, compressed| OracleTextKind::ITxt {
        language: language.into(),
        translated: translated.into(),
        compressed,
    };
    let packet = String::from_utf8(xmp_packet()).unwrap();
    assert_eq!(
        texts,
        [
            text("Author", "caf\u{e9}", OracleTextKind::Text),
            text("Note", "g\u{e4}mut", itxt("de", "Notiz", false)),
            text("XML:com.adobe.xmp", &packet, itxt("en", "Metadata", true)),
        ]
    );
}

/// A `zTXt` reaches the reference reader as a `zTXt`, holding the text it held.
///
/// Kills the `CompressedText` routing in `with_metadata_view` together with any defect in the
/// encoder's `zTXt` framing that this crate's reader would mirror.
#[test]
fn the_reference_reader_sees_a_carried_ztxt_as_ztxt() {
    let mut ztxt = b"Comment\0\0".to_vec();
    ztxt.extend_from_slice(&zlib(b"na\xEFve"));
    let meta = gamut_png::metadata(&minimal_source(&[chunk(b"zTXt", &ztxt)])).unwrap();
    let dec = libpng_oracle::decode(&re_encoded_bytes(|e| e.with_png_metadata(&meta)));

    assert_eq!(
        dec.texts,
        [OracleText {
            keyword: "Comment".into(),
            text: "na\u{ef}ve".into(),
            kind: OracleTextKind::ZTxt,
        }]
    );
}

/// A carried `sRGB` reaches the reference reader with its rendering intent. Alone, because libpng
/// keeps only the first of `sRGB` and `iCCP` (`png_handle_sRGB`/`png_handle_iCCP`, "too many
/// profiles"), so the pair [`a_profile_and_a_rendering_intent_are_both_carried`] pins cannot be
/// read back through it.
#[test]
fn the_reference_reader_sees_a_carried_rendering_intent() {
    let meta = gamut_png::metadata(&minimal_source(&[chunk(b"sRGB", &[1])])).unwrap();
    let dec = libpng_oracle::decode(&re_encoded_bytes(|e| e.with_png_metadata(&meta)));
    assert_eq!((dec.srgb, dec.warnings), (Some(1), 0));
}

/// The four chunks #569 put on the read surface come back out byte for byte when the written
/// header is the source's: `pHYs` (which a viewer scales by, so dropping it changes the printed
/// size), `tIME`, and the colour-type-shaped `sBIT` and `bKGD`.
///
/// Kills the `sBIT`, `Rgb`-background, `pHYs` and `tIME` carries in `with_metadata_view`.
#[test]
fn significant_bits_background_dimensions_and_time_survive_a_re_encode() {
    let carried: [(&[u8; 4], &[u8]); 4] = [
        (b"sBIT", &[5, 6, 5]),
        (b"bKGD", &[0, 10, 0, 20, 0, 30]),
        (b"pHYs", &[0, 0, 0x0E, 0xC4, 0, 0, 0x0B, 0x13, 1]),
        (b"tIME", &[0x07, 0xEA, 10, 16, 12, 34, 56]),
    ];
    let extra: Vec<Vec<u8>> = carried.iter().map(|(ty, data)| chunk(ty, data)).collect();
    let meta = gamut_png::metadata(&minimal_source(&extra)).unwrap();
    let png = re_encoded_bytes(|e| e.with_png_metadata(&meta));

    for (ty, data) in carried {
        assert_eq!(
            chunk_payload(&png, ty).as_deref(),
            Some(data),
            "{}",
            String::from_utf8_lossy(ty)
        );
    }
}

/// A sub-byte greyscale background is surfaced in the units of the decoded pixels — §13.12 scales
/// a two-bit sample to eight bits by 85 — so carrying it over the decoded pixels names the same
/// grey, and auto-reduce, packing the pixels back to two bits, maps the background back with
/// them.
///
/// Kills the `Gray`-background carry in `with_metadata_view`, and pins the read side's units
/// against the encoder's: a background surfaced unscaled would be written as the darker grey 2/255.
#[test]
fn a_sub_byte_grey_background_names_the_same_grey_after_a_re_encode() {
    // 2×2 two-bit grey, every pixel level 2, background level 2.
    let png = png_from_chunks(&[
        chunk(b"IHDR", &ihdr_payload(2, 2, 2, 0, 0)),
        chunk(b"bKGD", &[0, 2]),
        chunk(b"IDAT", &zlib(&[0, 0b1010_0000, 0, 0b1010_0000])),
        chunk(b"IEND", &[]),
    ]);
    let decoded = PngDecoder::new().decode(&png).unwrap();
    let PngImage::Gray8(pixels) = &decoded.image else {
        panic!("two-bit grey decodes to Gray8");
    };
    assert_eq!(pixels.as_samples(), [170; 4]);
    assert_eq!(decoded.background, Some(Background::Gray(170)));

    let encode = |reduce: bool| {
        PngEncoder::new()
            .with_auto_reduce(reduce)
            .with_metadata_from(&decoded)
            .encode_to_vec(pixels.as_ref())
            .expect("re-encode")
    };
    let eight_bit = encode(false);
    assert_eq!(chunk_payload(&eight_bit, b"bKGD"), Some(vec![0, 170]));
    let reduced = encode(true);
    assert_eq!(
        chunk_payload(&reduced, b"IHDR").map(|ihdr| ihdr[8]),
        Some(2),
        "auto-reduce packs the pixels back to two bits"
    );
    assert_eq!(chunk_payload(&reduced, b"bKGD"), Some(vec![0, 2]));
}

/// A palette-index background names an entry of the source's palette, so it is carried onto an
/// `encode_indexed8` of the decoded indices and that palette — and still names the same colour
/// there, whatever index the cleaned palette gives it.
///
/// Kills the `Index`-background carry in `with_metadata_view`.
#[test]
fn a_palette_index_background_is_carried_onto_the_same_palette() {
    let png = png_from_chunks(&[
        chunk(b"IHDR", &ihdr_payload(2, 1, 8, 3, 0)),
        chunk(b"PLTE", &[10, 20, 30, 40, 50, 60]),
        chunk(b"bKGD", &[1]),
        chunk(b"IDAT", &zlib(&[0, 0, 1])),
        chunk(b"IEND", &[]),
    ]);
    let decoded = PngDecoder::new().decode(&png).unwrap();
    let PngImage::Indexed8(indices) = &decoded.image else {
        panic!("a palette image decodes to Indexed8");
    };
    let palette = decoded.palette.as_ref().expect("palette");
    let mut out = Vec::new();
    PngEncoder::new()
        .with_metadata_from(&decoded)
        .encode_indexed8(indices.as_ref(), palette, &mut out)
        .expect("re-encode");

    let re = PngDecoder::new().decode(&out).unwrap();
    let Some(Background::Index(index)) = re.background else {
        panic!("background carried as an index, got {:?}", re.background);
    };
    let re_palette = re.palette.as_ref().expect("palette");
    assert_eq!(re_palette.rgb(index), Some([40, 50, 60]));
}

/// §11.3.3.4's language tag and translated keyword are what make an `iTXt` international; a
/// re-encode that reduced every annotation to a bare keyword and text would silently strip them.
#[test]
fn an_itxt_keeps_its_language_and_translated_keyword() {
    let meta = gamut_png::metadata(&source(&[])).unwrap();
    let re = re_encoded(|e| e.with_png_metadata(&meta));

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

    let re = re_encoded(|e| e.with_png_metadata(&meta));
    assert_eq!(re.icc_profile, meta.icc_profile);
    assert_eq!(re.srgb, meta.srgb);
}

/// A profile name is Latin-1 on both sides of the carry (§11.3.2.3), so it goes back as the bytes
/// it came out as. Written as UTF-8, the 0xE9 below became two bytes, which garbled the name and
/// pushed this 79-byte one past the 79 bytes the reader accepts — so the profile itself was lost
/// on read-back, with no notice.
#[test]
fn a_latin1_profile_name_of_the_full_length_is_carried_byte_for_byte() {
    let name = [b"caf\xE9".as_slice(), &[b'n'; 75]].concat();
    assert_eq!(name.len(), 79);
    let mut iccp = name.clone();
    iccp.extend_from_slice(b"\0\0");
    iccp.extend_from_slice(&zlib(&tiny_icc_profile()));
    let meta = gamut_png::metadata(&minimal_source(&[chunk(b"iCCP", &iccp)])).unwrap();
    assert!(meta.icc_profile.is_some(), "the source carries the profile");

    let out = re_encoded_bytes(|e| e.with_png_metadata(&meta));
    let payload = chunk_payload(&out, b"iCCP").expect("the profile is written");
    assert_eq!(&payload[..80], [name.as_slice(), b"\0"].concat());
    let re = gamut_png::metadata(&out).expect("read back");
    assert_eq!(re.icc_profile, meta.icc_profile);
}

/// §11.3.2.3 says of the profile name what §11.3.3.1 says of a keyword: "Leading, trailing, and
/// consecutive spaces are not permitted". This crate's reader returns such a name unchanged, so
/// a carry writes it back verbatim — the profile is the payload whose loss changes what a viewer
/// paints — and names the deviation, as it does a keyword's.
///
/// Kills the `iCCP` notice's path into [`PngEncoder::metadata_notices`], and a carry that drops
/// the profile instead of writing it.
#[test]
fn a_profile_name_the_reader_accepts_survives_the_re_encode_with_a_notice() {
    let mut iccp = b" Tiny\0\0".to_vec();
    iccp.extend_from_slice(&zlib(&tiny_icc_profile()));
    let meta = gamut_png::metadata(&minimal_source(&[chunk(b"iCCP", &iccp)])).unwrap();
    assert!(meta.icc_profile.is_some(), "the reader accepts the name");

    let encoder = PngEncoder::new().with_png_metadata(&meta);
    assert_eq!(
        encoder.metadata_notices(),
        [MetadataNotice::IccpNameSpacing]
    );
    assert!(MetadataNotice::IccpNameSpacing.carried());
    let re = re_encoded(|_| encoder.clone());
    assert_eq!(re.icc_profile, meta.icc_profile, "came back whole");
}

/// The other half: a profile name this crate's reader refuses — the issue's three silent losses
/// (#619), an empty name, an 80-byte one and one holding a null — plus one outside Latin-1 that
/// read back as mojibake. Each now leaves the profile behind *with* a notice, and the encode
/// proceeds. Driven through the setter, because the reader will not produce such a name.
#[test]
fn a_profile_name_no_reader_accepts_is_left_behind_with_a_notice() {
    for (name, notice) in [
        ("", MetadataNotice::IccpNameLength),
        (&"p".repeat(80), MetadataNotice::IccpNameLength),
        ("pro\0file", MetadataNotice::IccpNameNull),
        ("\u{153}rofile", MetadataNotice::IccpNameNotLatin1),
    ] {
        let encoder = PngEncoder::new().with_icc_profile(name, &tiny_icc_profile());
        assert_eq!(encoder.metadata_notices(), [notice], "name {name:?}");
        assert!(!notice.carried(), "name {name:?}");
        assert_eq!(
            re_encoded(|_| encoder.clone()).icc_profile,
            None,
            "name {name:?} was not written"
        );
    }
}

/// The writer's half of §11.3.5.1's Table 26: a `with_time` the reader would skip as malformed is
/// not written, and `metadata_notices` names it, so the encoder never emits a `tIME` its own
/// reader drops.
///
/// Kills the time notice's path into [`PngEncoder::metadata_notices`].
#[test]
fn an_out_of_range_time_is_left_behind_with_a_notice() {
    let encoder = PngEncoder::new().with_time(2026, 6, 15, 24, 0, 0);
    assert_eq!(encoder.metadata_notices(), [MetadataNotice::TimeOutOfRange]);
    assert_eq!(
        chunk_payload(&re_encoded_bytes(|_| encoder.clone()), b"tIME"),
        None,
        "no tIME chunk is written"
    );
}

/// §11.3.2.6: "RGB is currently the only supported color model in PNG, and as such Matrix
/// Coefficients shall be set to 0." A source chunk that says otherwise is not a conforming cICP,
/// so it is dropped rather than reproduced — while a conforming one is carried, which matters
/// because §4.3 Table 1 makes cICP the *highest*-precedence colour chunk.
#[test]
fn a_cicp_is_carried_only_when_its_matrix_coefficients_are_zero() {
    let conforming = gamut_png::metadata(&source(&[chunk(b"cICP", &[9, 16, 0, 1])])).unwrap();
    let carried = re_encoded(|e| e.with_png_metadata(&conforming))
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
    let encoder = PngEncoder::new().with_png_metadata(&non_rgb);
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

    let encoder = PngEncoder::new().with_png_metadata(&meta);
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
fn with_metadata_from_agrees_with_with_png_metadata() {
    let png = source(&[
        chunk(b"cICP", &[9, 16, 0, 1]),
        chunk(b"sBIT", &[5, 6, 5]),
        chunk(b"bKGD", &[0, 10, 0, 20, 0, 30]),
        chunk(b"pHYs", &[0, 0, 0x0E, 0xC4, 0, 0, 0x0E, 0xC4, 1]),
        chunk(b"tIME", &[0x07, 0xEA, 10, 16, 12, 34, 56]),
    ]);
    let decoded = PngDecoder::new().decode(&png).unwrap();
    let meta = gamut_png::metadata(&png).unwrap();

    let from_decoded = re_encoded(|e| e.with_metadata_from(&decoded));
    let from_metadata = re_encoded(|e| e.with_png_metadata(&meta));
    assert_eq!(from_decoded, from_metadata);
    // A pair of empty results would satisfy the comparison above.
    assert!(from_decoded.icc_profile.is_some() && from_decoded.cicp.is_some());
    assert!(!from_decoded.texts.is_empty() && from_decoded.exif.is_some());
    assert!(from_decoded.significant_bits.is_some() && from_decoded.background.is_some());
    assert!(from_decoded.physical_dimensions.is_some() && from_decoded.modification_time.is_some());
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

    let out = re_encoded_bytes(|e| e.with_png_metadata(&meta));
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

    let out = re_encoded_bytes(|e| e.with_png_metadata(&meta));
    let note = chunk_payload(&out, b"iTXt").expect("the Note annotation");
    // keyword, NUL, compression flag 1, method 0, language, NUL, translated keyword, NUL.
    assert!(note.starts_with(b"Note\0\x01\0de\0Notiz\0"), "{note:?}");
    assert!(
        note.len() < body.len() / 4,
        "still compressed: {} bytes",
        note.len()
    );
}

/// The same claim for the XMP packet, which is where it was once untrue: the packet used to leave
/// the read side through a field of its own, and the `iTXt` framing that field did not hold —
/// §11.3.3.4's compression flag above all — had to travel beside it. It is now an ordinary entry
/// of `texts` and keeps its flag the way every annotation does; this pins that it still does.
///
/// A provenance packet is exactly the payload a writer compresses, and rewriting one
/// uncompressed inflates it by a factor a user notices.
#[test]
fn a_compressed_xmp_packet_goes_back_into_a_compressed_itxt() {
    let meta = gamut_png::metadata(&minimal_source(&[chunk(b"iTXt", &compressed_xmp())])).unwrap();
    assert_eq!(meta.xmp(), Some(&xmp_packet()[..]));

    let out = re_encoded_bytes(|e| e.with_png_metadata(&meta));
    let carried = chunk_payload(&out, b"iTXt").expect("the packet");
    // keyword, NUL, then §11.3.3.4's compression flag.
    assert_eq!(carried[18], 1, "the flag is set: {carried:?}");

    // Measured against the same carry with the flag cleared, so the claim is the inflation the
    // flag prevents rather than a threshold that happens to hold for this packet.
    let mut flat = meta.clone();
    flat.texts[0].kind = TextChunkKind::International;
    let flat_out = re_encoded_bytes(|e| e.with_png_metadata(&flat));
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
    let packet = |meta: &PngMetadata| {
        meta.texts
            .iter()
            .find(|text| text.keyword == "XML:com.adobe.xmp")
            .cloned()
            .expect("the source carries a packet")
    };
    let framed = packet(&meta);
    assert_eq!(framed.language.as_deref(), Some("en"));
    assert_eq!(framed.translated_keyword.as_deref(), Some("Metadata"));
    assert_eq!(framed.kind, TextChunkKind::CompressedInternational);

    let re = re_encoded(|e| e.with_png_metadata(&meta));
    assert_eq!(packet(&re), framed);
}

/// The XMP `iTXt` chunks in `png`, counted by walking the chunk stream.
fn xmp_chunks(png: &[u8]) -> usize {
    let mut packets = 0;
    let mut i = 8;
    while i + 12 <= png.len() {
        let len = u32::from_be_bytes([png[i], png[i + 1], png[i + 2], png[i + 3]]) as usize;
        if &png[i + 4..i + 8] == b"iTXt" && png[i + 8..].starts_with(b"XML:com.adobe.xmp\0") {
            packets += 1;
        }
        i += 12 + len;
    }
    packets
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
            .with_png_metadata(&meta)
    });

    assert_eq!(xmp_chunks(&out), 1, "one keyword, one chunk");
    assert_eq!(
        gamut_png::metadata(&out).unwrap().xmp(),
        meta.xmp(),
        "and it is the carried packet, not the one it replaced"
    );
}

/// The other half: a carry whose source holds no packet has none to replace a directly-set one
/// with, so the packet the caller set survives it.
///
/// Kills the guard on `Ancillary::end_carry`'s displacement, without which every carry would
/// discard a packet set beforehand.
#[test]
fn carrying_a_source_without_a_packet_keeps_one_already_set() {
    let meta = gamut_png::metadata(&minimal_source(&[chunk(b"tEXt", b"Author\0me")])).unwrap();
    let re = re_encoded(|e| {
        e.with_xmp("<x:xmpmeta id='set directly'/>")
            .with_png_metadata(&meta)
    });
    assert_eq!(re.xmp(), Some(&b"<x:xmpmeta id='set directly'/>"[..]));
}

/// A source that holds two packets is written back with two: the carry copies the text list as
/// it stands and does not decide, on the source's behalf, which packet was meant. This crate's
/// reader takes the first, so the packet [`PngMetadata::xmp`] reports before and after the carry
/// is the same one.
#[test]
fn a_source_with_two_packets_is_carried_with_both() {
    let meta = gamut_png::metadata(&minimal_source(&[
        chunk(
            b"iTXt",
            b"XML:com.adobe.xmp\0\0\0\0\0<x:xmpmeta id='first'/>",
        ),
        chunk(
            b"iTXt",
            b"XML:com.adobe.xmp\0\0\0\0\0<x:xmpmeta id='second'/>",
        ),
    ]))
    .unwrap();
    let out = re_encoded_bytes(|e| e.with_png_metadata(&meta));
    assert_eq!(xmp_chunks(&out), 2);
    assert_eq!(
        gamut_png::metadata(&out).unwrap().xmp(),
        Some(&b"<x:xmpmeta id='first'/>"[..])
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

        let encoder = PngEncoder::new().with_png_metadata(&meta);
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

    let once = re_encoded(|e| e.with_png_metadata(&meta));
    let twice = re_encoded(|e| e.with_png_metadata(&meta).with_png_metadata(&meta));
    assert_eq!(once, twice);
    assert_eq!(
        once.texts.len(),
        3,
        "the fixture carries two annotations and the XMP packet"
    );
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

    let encoder = PngEncoder::new().with_png_metadata(&meta);
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
        MetadataNotice::TextStringNull,
    ] {
        assert!(!lost.carried(), "{lost:?}");
    }
}
