//! Embedded-metadata round-trips: the `ICCP` colour profile and the `EXIF` / `XMP ` chunks
//! (RFC 9649 §2.7.2-§2.7.3), plus the `C2PA` manifest store (C2PA 2.4 §A.3.7).
//!
//! The contract under test is byte fidelity plus container conformance — a payload handed to
//! `WebpEncoder::with_*` comes back from `gamut_webp::metadata` unchanged, the `VP8X` feature flags
//! match the chunks actually present, the chunks appear in the spec's canonical order, and attaching
//! metadata changes nothing about the pixels. The differential check against libwebp's own muxer
//! lives in `oracle.rs`.

use gamut_core::{DecodeImage, Dimensions, EncodeImage, ImageBuf, ImageRef, Result, Rgb8, Rgba8};
use gamut_riff::{FourCc, RiffReader, RiffWriter, Vp8xHeader, WebpLayout};
use gamut_webp::{WebpDecoder, WebpEncoder, WebpMetadata};

/// A 200-byte "ICC profile": opaque bytes, even length.
const ICC: &[u8] = &[0x5a; 200];
/// An Exif payload of *odd* length, so the RIFF pad byte must not leak into the payload read back.
const EXIF: &[u8] = b"II\x2a\x00\x08\x00\x00\x00exif-payload";
/// An XMP packet, likewise odd-length.
const XMP: &[u8] = b"<?xpacket begin='\xef\xbb\xbf'?><x:xmpmeta/><?xpacket end='w'?>";
/// A stand-in C2PA manifest store, of odd length so the RIFF pad byte cannot leak into it.
const C2PA: &[u8] = b"opaque C2PA manifest store";

fn dims(width: u32, height: u32) -> Dimensions {
    Dimensions { width, height }
}

/// A deterministic RGB gradient.
fn rgb(w: u32, h: u32) -> Vec<u8> {
    (0..w * h)
        .flat_map(|i| [(i % w * 7) as u8, (i / w * 11) as u8, (i % 251) as u8])
        .collect()
}

/// A deterministic RGBA gradient with non-trivial alpha.
fn rgba(w: u32, h: u32) -> Vec<u8> {
    (0..w * h)
        .flat_map(|i| {
            let (x, y) = (i % w, i / w);
            [
                (x * 7) as u8,
                (y * 11) as u8,
                (x ^ y) as u8,
                ((x * 5 + y * 3) & 0xff) as u8,
            ]
        })
        .collect()
}

fn encode_rgb(encoder: &WebpEncoder, pixels: &[u8], d: Dimensions) -> Vec<u8> {
    let mut out = Vec::new();
    encoder
        .encode_image(
            ImageRef::<Rgb8>::new(pixels, d).expect("rgb fixture"),
            &mut out,
        )
        .expect("encode rgb");
    out
}

fn encode_rgba(encoder: &WebpEncoder, pixels: &[u8], d: Dimensions) -> Vec<u8> {
    let mut out = Vec::new();
    encoder
        .encode_image(
            ImageRef::<Rgba8>::new(pixels, d).expect("rgba fixture"),
            &mut out,
        )
        .expect("encode rgba");
    out
}

/// The `(exif, xmp, icc)` payloads of a file, as owned bytes.
type Payloads = (Option<Vec<u8>>, Option<Vec<u8>>, Option<Vec<u8>>);

/// Reads `file`'s metadata as a comparable triple. `WebpMetadata` is `#[non_exhaustive]`, so a test
/// outside the crate cannot build one to compare against.
fn read(file: &[u8]) -> Payloads {
    let meta = gamut_webp::metadata(file).expect("read metadata");
    (meta.exif, meta.xmp, meta.icc)
}

/// Builds the expected `(exif, xmp, icc)` triple from borrowed literals.
fn want(exif: Option<&[u8]>, xmp: Option<&[u8]>, icc: Option<&[u8]>) -> Payloads {
    (
        exif.map(<[u8]>::to_vec),
        xmp.map(<[u8]>::to_vec),
        icc.map(<[u8]>::to_vec),
    )
}

/// The chunk FourCCs of `file`, in file order.
fn chunks(file: &[u8]) -> Vec<FourCc> {
    RiffReader::new(file)
        .expect("valid RIFF/WebP")
        .map(|c| c.expect("valid chunk").fourcc)
        .collect()
}

/// The parsed `VP8X` feature header of `file`, or `None` for a simple-format file.
fn vp8x(file: &[u8]) -> Option<Vp8xHeader> {
    RiffReader::new(file)
        .expect("valid RIFF/WebP")
        .map(|c| c.expect("valid chunk"))
        .find(|c| c.fourcc == FourCc::VP8X)
        .map(|c| Vp8xHeader::from_payload(c.payload).expect("valid VP8X payload"))
}

/// Every encoder configuration that can carry metadata, as `(label, encoder)`.
fn encoders() -> Vec<(&'static str, WebpEncoder)> {
    vec![
        ("lossless", WebpEncoder::lossless()),
        ("lossy", WebpEncoder::lossy(70)),
    ]
}

#[test]
fn each_chunk_round_trips_byte_exactly() {
    // One payload at a time: only that chunk's field is populated, and the bytes are identical to
    // the input — no signature added, no padding absorbed, no reserialization.
    for (label, base) in encoders() {
        let d = dims(17, 9);
        let rgb = rgb(17, 9);

        let with_exif = encode_rgb(&base.clone().with_exif(EXIF), &rgb, d);
        assert_eq!(
            read(&with_exif),
            want(Some(EXIF), None, None),
            "{label}: EXIF only"
        );

        let with_xmp = encode_rgb(&base.clone().with_xmp(XMP), &rgb, d);
        assert_eq!(
            read(&with_xmp),
            want(None, Some(XMP), None),
            "{label}: XMP only"
        );

        let with_icc = encode_rgb(&base.clone().with_icc_profile(ICC), &rgb, d);
        assert_eq!(
            read(&with_icc),
            want(None, None, Some(ICC)),
            "{label}: ICCP only"
        );
    }
}

#[test]
fn all_three_chunks_round_trip_and_flag_themselves() {
    // All three together: payloads intact, and the VP8X feature header advertises exactly the three
    // metadata features (and no alpha, for an opaque RGB image).
    for (label, base) in encoders() {
        let d = dims(16, 16);
        let file = encode_rgb(
            &base.with_exif(EXIF).with_xmp(XMP).with_icc_profile(ICC),
            &rgb(16, 16),
            d,
        );
        assert_eq!(
            read(&file),
            want(Some(EXIF), Some(XMP), Some(ICC)),
            "{label}: all three payloads"
        );
        assert_eq!(
            vp8x(&file).unwrap_or_else(|| panic!("{label}: metadata must promote to VP8X")),
            Vp8xHeader {
                icc_profile: true,
                alpha: false,
                exif_metadata: true,
                xmp_metadata: true,
                animation: false,
                canvas_width: 16,
                canvas_height: 16,
            },
            "{label}: VP8X flags"
        );
    }
}

#[test]
fn chunks_are_emitted_in_the_canonical_spec_order() {
    // RFC 9649 §2.7: VP8X, ICCP, image data, then the metadata chunks. `ICCP` "MUST appear before
    // the image data" (§2.7.2), so a reader can colour-correct while streaming.
    let file = encode_rgb(
        &WebpEncoder::lossless()
            .with_exif(EXIF)
            .with_xmp(XMP)
            .with_icc_profile(ICC),
        &rgb(8, 8),
        dims(8, 8),
    );
    assert_eq!(
        chunks(&file),
        vec![
            FourCc::VP8X,
            FourCc::ICCP,
            FourCc::VP8L,
            FourCc::EXIF,
            FourCc::XMP,
        ]
    );
}

#[test]
fn transparent_lossy_keeps_alph_inside_the_image_data() {
    // The alpha subchunk is part of the *image data*, so it sits after `ICCP` and before both the
    // bitstream and the metadata — and the alpha flag rides alongside the metadata flags.
    let (w, h) = (32u32, 24u32);
    let file = encode_rgba(
        &WebpEncoder::lossy(70)
            .with_exif(EXIF)
            .with_xmp(XMP)
            .with_icc_profile(ICC),
        &rgba(w, h),
        dims(w, h),
    );
    assert_eq!(
        chunks(&file),
        vec![
            FourCc::VP8X,
            FourCc::ICCP,
            FourCc::ALPH,
            FourCc::VP8,
            FourCc::EXIF,
            FourCc::XMP,
        ]
    );
    let header = vp8x(&file).expect("extended file");
    assert!(header.alpha, "ALPH chunk must be advertised");
    assert!(header.icc_profile && header.exif_metadata && header.xmp_metadata);
}

#[test]
fn lossless_alpha_is_flagged_when_metadata_promotes_the_file() {
    // A VP8L bitstream carries its own alpha, so no `ALPH` chunk is emitted — but once metadata
    // forces the extended format, the `VP8X` header must still declare the transparency.
    let (w, h) = (16u32, 16u32);
    let file = encode_rgba(
        &WebpEncoder::lossless().with_exif(EXIF),
        &rgba(w, h),
        dims(w, h),
    );
    assert_eq!(
        chunks(&file),
        vec![FourCc::VP8X, FourCc::VP8L, FourCc::EXIF]
    );
    assert!(vp8x(&file).expect("extended file").alpha);

    // Fully opaque RGBA: promoted for the metadata, but with the alpha flag clear.
    let opaque = [10u8, 20, 30, 0xff].repeat((w * h) as usize);
    let file = encode_rgba(
        &WebpEncoder::lossless().with_exif(EXIF),
        &opaque,
        dims(w, h),
    );
    assert!(!vp8x(&file).expect("extended file").alpha);
}

#[test]
fn no_metadata_leaves_the_simple_format_alone() {
    // Nothing embedded, nothing to promote: an opaque image stays a single-chunk simple file, so the
    // extended-format overhead is only paid when a feature needs it.
    for (label, encoder) in encoders() {
        let file = encode_rgb(&encoder, &rgb(16, 16), dims(16, 16));
        assert_eq!(chunks(&file).len(), 1, "{label}: simple format");
        assert!(vp8x(&file).is_none(), "{label}: no VP8X chunk");
        assert_eq!(
            gamut_webp::metadata(&file).unwrap(),
            WebpMetadata::default(),
            "{label}: no metadata"
        );
    }
}

#[test]
fn metadata_does_not_disturb_the_pixels() {
    // Promotion to the extended format must be transparent to the codec: the same bitstream, so the
    // same decoded pixels (and, for RGBA, the same alpha).
    let (w, h) = (33u32, 17u32);
    let d = dims(w, h);
    for (label, base) in encoders() {
        let plain = encode_rgb(&base, &rgb(w, h), d);
        let tagged = encode_rgb(
            &base
                .clone()
                .with_exif(EXIF)
                .with_xmp(XMP)
                .with_icc_profile(ICC),
            &rgb(w, h),
            d,
        );
        let decode = |file: &[u8]| -> ImageBuf<Rgb8> {
            WebpDecoder::new().decode_image(file).expect("decode rgb")
        };
        assert_eq!(
            decode(&plain).as_samples(),
            decode(&tagged).as_samples(),
            "{label}: RGB pixels unchanged by metadata"
        );

        let source = rgba(w, h);
        let tagged = encode_rgba(&base.clone().with_icc_profile(ICC), &source, d);
        let decoded: ImageBuf<Rgba8> = WebpDecoder::new()
            .decode_image(&tagged)
            .expect("decode rgba");
        assert_eq!(decoded.dimensions(), d, "{label}: dimensions");
        let got: Vec<u8> = decoded
            .as_samples()
            .as_chunks::<4>()
            .0
            .iter()
            .map(|p| p[3])
            .collect();
        let want: Vec<u8> = source.as_chunks::<4>().0.iter().map(|p| p[3]).collect();
        assert_eq!(got, want, "{label}: alpha still lossless");
    }
}

#[test]
fn setters_keep_the_last_payload() {
    // The setters replace rather than accumulate, so exactly one chunk of each kind is ever emitted.
    let file = encode_rgb(
        &WebpEncoder::lossless()
            .with_exif(b"first")
            .with_exif(EXIF)
            .with_icc_profile(b"first")
            .with_icc_profile(ICC),
        &rgb(4, 4),
        dims(4, 4),
    );
    assert_eq!(
        chunks(&file),
        vec![FourCc::VP8X, FourCc::ICCP, FourCc::VP8L, FourCc::EXIF]
    );
    let meta = gamut_webp::metadata(&file).unwrap();
    assert_eq!(meta.exif.as_deref(), Some(EXIF));
    assert_eq!(meta.icc.as_deref(), Some(ICC));
}

#[test]
fn empty_payloads_round_trip_as_empty_chunks() {
    // A zero-length chunk is legal RIFF and the spec sets no minimum, so an empty payload is emitted
    // (flag and all) and read back as `Some(&[])` rather than silently becoming `None`.
    let file = encode_rgb(
        &WebpEncoder::lossless().with_exif(b"").with_xmp(b""),
        &rgb(2, 2),
        dims(2, 2),
    );
    let header = vp8x(&file).expect("extended file");
    assert!(header.exif_metadata && header.xmp_metadata);
    assert_eq!(read(&file), want(Some(b""), Some(b""), None));
}

#[test]
fn payloads_larger_than_a_16_bit_length_round_trip() {
    // A WebP chunk size is a `uint32`, so — unlike JPEG's 64 KiB APPn segments — a large profile
    // needs no chunking scheme and must survive whole.
    let profile: Vec<u8> = (0..100_003u32).map(|i| (i % 253) as u8).collect();
    let file = encode_rgb(
        &WebpEncoder::lossless().with_icc_profile(&profile),
        &rgb(4, 4),
        dims(4, 4),
    );
    assert_eq!(
        gamut_webp::metadata(&file).unwrap().icc.as_deref(),
        Some(profile.as_slice())
    );
}

#[test]
fn metadata_reads_a_hand_built_file_first_chunk_wins() {
    // "There SHOULD be at most one chunk of each type... readers MAY ignore all except the first
    // one" (§2.7.2-§2.7.3). Hand-built because the encoder can never emit a duplicate.
    let inner = encode_rgb(&WebpEncoder::lossless(), &rgb(2, 2), dims(2, 2));
    let bitstream = RiffReader::new(&inner)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .payload
        .to_vec();
    let header = Vp8xHeader {
        exif_metadata: true,
        icc_profile: true,
        canvas_width: 2,
        canvas_height: 2,
        ..Default::default()
    };
    let mut w = RiffWriter::new();
    w.write_chunk(FourCc::VP8X, &header.to_payload().unwrap())
        .unwrap();
    w.write_chunk(FourCc::ICCP, b"first-icc").unwrap();
    w.write_chunk(FourCc::ICCP, b"second-icc").unwrap();
    w.write_chunk(FourCc::VP8L, &bitstream).unwrap();
    w.write_chunk(FourCc::EXIF, b"first-exif").unwrap();
    w.write_chunk(FourCc::EXIF, b"second-exif").unwrap();
    let file = w.finish().unwrap();

    assert_eq!(
        read(&file),
        want(Some(b"first-exif"), None, Some(b"first-icc"))
    );
    // The duplicated chunks must not disturb the decode either.
    let decoded: ImageBuf<Rgb8> = WebpDecoder::new().decode_image(&file).expect("decode");
    assert_eq!(decoded.as_samples(), rgb(2, 2).as_slice());
}

#[test]
fn metadata_rejects_input_that_is_not_a_webp_file() {
    let err: Result<WebpMetadata> = gamut_webp::metadata(b"definitely not a WebP file");
    assert!(err.is_err());
}

/// A chunk FourCC the WebP container spec does not define — an *unknown chunk* (RFC 9649 §2.7.1.6).
const PRIVATE: [u8; 4] = *b"XYZW";

#[test]
fn unknown_chunks_survive_a_decode_re_encode_cycle() {
    // §2.7.1.6: "Readers SHOULD ignore these chunks. Writers SHOULD preserve them in their original
    // order." This is the full loop the two halves exist for — read the chunks out of one file with
    // gamut-riff's layout view, hand them back to the encoder, and find them again unchanged.
    let odd = FourCc::from(PRIVATE);
    let pixels = rgb(2, 2);
    let image = ImageRef::<Rgb8>::new(&pixels, Dimensions::new(2, 2).unwrap()).unwrap();

    let mut original = Vec::new();
    WebpEncoder::lossless()
        .with_exif(b"exif payload")
        .with_unknown_chunks(&[(odd, b"private payload")])
        .encode_image(image, &mut original)
        .expect("encode");

    // The decoder ignores it, exactly as the spec asks of readers.
    let decoded: ImageBuf<Rgb8> = WebpDecoder::new().decode_image(&original).expect("decode");
    assert_eq!(decoded.as_samples(), pixels.as_slice());

    let layout = WebpLayout::parse(&original).expect("parse");
    assert_eq!(layout.unknown.len(), 1);
    assert_eq!(layout.unknown[0].fourcc, odd);
    assert_eq!(layout.unknown[0].payload, b"private payload");

    // Re-encode, feeding the recovered chunks straight back in.
    let carried: Vec<(FourCc, &[u8])> = layout
        .unknown
        .iter()
        .map(|c| (c.fourcc, c.payload))
        .collect();
    let mut rewritten = Vec::new();
    WebpEncoder::lossless()
        .with_exif(b"exif payload")
        .with_unknown_chunks(&carried)
        .encode_image(
            ImageRef::<Rgb8>::new(decoded.as_samples(), decoded.dimensions()).unwrap(),
            &mut rewritten,
        )
        .expect("re-encode");

    let round_tripped = WebpLayout::parse(&rewritten).expect("parse");
    assert_eq!(round_tripped.unknown.len(), 1, "the chunk survived");
    assert_eq!(round_tripped.unknown[0].fourcc, odd);
    assert_eq!(round_tripped.unknown[0].payload, b"private payload");
    assert_eq!(
        round_tripped.metadata.exif,
        Some(&b"exif payload"[..]),
        "metadata survived alongside it"
    );
}

#[test]
fn an_unknown_chunk_alone_promotes_a_file_to_the_extended_format() {
    // Only the extended format has a place for an unknown chunk, so one must promote the file even
    // with no metadata and no alpha to force it.
    let pixels = rgb(2, 2);
    let image = ImageRef::<Rgb8>::new(&pixels, Dimensions::new(2, 2).unwrap()).unwrap();
    let mut file = Vec::new();
    WebpEncoder::lossless()
        .with_unknown_chunks(&[(FourCc::from(PRIVATE), b"payload")])
        .encode_image(image, &mut file)
        .expect("encode");

    let layout = WebpLayout::parse(&file).expect("parse");
    assert!(layout.vp8x.is_some(), "promoted to extended");
    assert_eq!(layout.unknown.len(), 1);
}

#[test]
fn no_unknown_chunks_leaves_a_simple_file_simple() {
    // The converse: the promotion must be driven by an actual chunk, not by the field existing.
    let pixels = rgb(2, 2);
    let image = ImageRef::<Rgb8>::new(&pixels, Dimensions::new(2, 2).unwrap()).unwrap();
    let mut file = Vec::new();
    WebpEncoder::lossless()
        .with_unknown_chunks(&[])
        .encode_image(image, &mut file)
        .expect("encode");

    let layout = WebpLayout::parse(&file).expect("parse");
    assert!(layout.vp8x.is_none(), "still the simple format");
    assert!(layout.unknown.is_empty());
}

#[test]
fn a_store_alone_promotes_a_file_to_the_extended_format_and_goes_last() {
    // Only the extended format has a place for a `C2PA` chunk, and C2PA 2.4 §A.3.7 puts it at the
    // very end of the form — behind the metadata and behind the preserved unknown chunks, which is
    // the placement nothing else in this crate's chunk order would produce.
    for (label, encoder) in encoders() {
        let file = encode_rgb(
            &encoder
                .clone()
                .with_exif(EXIF)
                .with_xmp(XMP)
                .with_icc_profile(ICC)
                .with_c2pa(C2PA)
                .with_unknown_chunks(&[(FourCc::from(*b"XYZW"), b"private")]),
            &rgb(16, 16),
            dims(16, 16),
        );
        let ids = chunks(&file);
        assert_eq!(
            ids.last().map(|f| *f.as_bytes()),
            Some(*b"C2PA"),
            "{label}: the store is the last sub-chunk"
        );
        assert_eq!(
            ids.iter().filter(|f| f.as_bytes() == b"C2PA").count(),
            1,
            "{label}: exactly one store chunk"
        );
    }
}

#[test]
fn the_store_round_trips_byte_exactly_and_sets_no_vp8x_flag() {
    // The store crosses the boundary verbatim — the pad byte its odd length forces must not be read
    // back as part of it — and RFC 9649 §2.5 defines no C2PA feature bit, so a store must leave the
    // `VP8X` flag byte exactly as a store-free file has it.
    assert_eq!(
        C2PA.len() % 2,
        0,
        "sanity: adjust the fixture if this changes"
    );
    let odd = &C2PA[..C2PA.len() - 1];
    assert_eq!(odd.len() % 2, 1);
    for (label, encoder) in encoders() {
        let file = encode_rgb(&encoder.clone().with_c2pa(odd), &rgb(16, 16), dims(16, 16));
        let meta = gamut_webp::metadata(&file).expect("read metadata");
        assert_eq!(meta.c2pa.as_deref(), Some(odd), "{label}: verbatim");
        assert_eq!(
            read(&file),
            want(None, None, None),
            "{label}: no other carrier"
        );

        let flagged = encode_rgb(
            &encoder
                .clone()
                .with_unknown_chunks(&[(FourCc::from(*b"XYZW"), b"x")]),
            &rgb(16, 16),
            dims(16, 16),
        );
        assert_eq!(
            vp8x(&file),
            vp8x(&flagged),
            "{label}: a store sets no feature flag"
        );
    }
}

#[test]
fn c2pa_span_reads_back_the_range_the_store_occupies() {
    // The read-side accessor must find the store in a finished file — including one whose earlier
    // chunks are odd-length, whose pad bytes it therefore has to count — and report the chunk's
    // whole span, not just the payload.
    let file = encode_rgb(
        &WebpEncoder::lossless().with_exif(EXIF).with_c2pa(C2PA),
        &rgb(8, 8),
        dims(8, 8),
    );
    let span = gamut_webp::c2pa_span(&file)
        .expect("parse")
        .expect("the store was embedded");
    assert_eq!(&file[span.start..span.start + 4], b"C2PA");
    assert_eq!(&file[span.start + 8..span.end], C2PA);
    assert_eq!(span.len(), 8 + C2PA.len());
}

#[test]
fn a_file_without_a_store_reports_no_span_and_no_store() {
    // The converse: an ordinary file must not be handed a range to exclude.
    let file = encode_rgb(
        &WebpEncoder::lossless().with_exif(EXIF),
        &rgb(8, 8),
        dims(8, 8),
    );
    assert_eq!(gamut_webp::c2pa_span(&file).expect("parse"), None);
    assert_eq!(gamut_webp::metadata(&file).expect("read").c2pa, None);
}

#[test]
fn a_store_survives_a_decode_re_encode_cycle_exactly_once() {
    // A `C2PA` chunk is not an unknown chunk, so a caller who carries unknown chunks forward with
    // `WebpLayout::parse` + `with_unknown_chunks` must not also carry the store and duplicate it.
    let file = encode_rgb(
        &WebpEncoder::lossless().with_c2pa(C2PA),
        &rgb(8, 8),
        dims(8, 8),
    );
    let layout = WebpLayout::parse(&file).expect("parse");
    assert!(
        layout.unknown.is_empty(),
        "the store is not an unknown chunk"
    );

    let carried: Vec<(FourCc, &[u8])> = layout
        .unknown
        .iter()
        .map(|c| (c.fourcc, c.payload))
        .collect();
    let again = encode_rgb(
        &WebpEncoder::lossless()
            .with_c2pa(layout.metadata.c2pa.expect("the store was read back"))
            .with_unknown_chunks(&carried),
        &rgb(8, 8),
        dims(8, 8),
    );
    assert_eq!(again, file);
}
