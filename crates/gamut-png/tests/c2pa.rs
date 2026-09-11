//! The C2PA manifest store's carriage in the `caBX` chunk (issue #440; C2PA 2.4 §A.3.2,
//! §18.5.4). Exact-byte: where the encoder puts the chunk, that a reservation is filled without
//! moving a byte outside it, and that both reports name the same whole-chunk span at known
//! offsets. Differential: libpng frames the same payload into the same bytes, decodes gamut's
//! file pixel-exact with the chunk in place, and gamut reads the store back from a libpng-written
//! file. It also pins the two rules that decide what *is* the store: exactly one per file, and
//! never one positioned after `IDAT`. The store is opaque bytes throughout — its behavioural
//! oracle, `c2pa-rs`, is issue #447's.

mod common;

use common::{
    chunk, ihdr_payload, libpng_with_extra_chunks, png_from_chunks, sample_bytes, tiny_exif,
    tiny_icc_profile, zlib,
};
use gamut_core::{DecodeImage, Dimensions, EncodeImage, ImageBuf, ImageRef, Indexed8, Rgb8, Rgba8};
use gamut_png::{
    PhysicalUnit, PngDecoder, PngEncoder, PngPalette, SegmentKind, SrgbIntent, deconstruct,
    fill_c2pa,
};

/// A stand-in manifest store of `len` bytes: not all zero, no two runs alike, so a fill is
/// visible byte for byte. Opaque here, as every store is.
fn store(len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| (i.wrapping_mul(37) ^ 0x5A) as u8)
        .collect()
}

/// Chunk types in file order (after the signature).
fn chunk_types(png: &[u8]) -> Vec<[u8; 4]> {
    let mut types = Vec::new();
    let mut i = 8;
    while i + 12 <= png.len() {
        let len = u32::from_be_bytes([png[i], png[i + 1], png[i + 2], png[i + 3]]) as usize;
        types.push([png[i + 4], png[i + 5], png[i + 6], png[i + 7]]);
        i += 12 + len;
    }
    types
}

/// The first chunk of type `ty`, framing included: length, type, payload, CRC.
fn framed_chunk(png: &[u8], ty: &[u8; 4]) -> Option<Vec<u8>> {
    let mut i = 8;
    while i + 12 <= png.len() {
        let len = u32::from_be_bytes([png[i], png[i + 1], png[i + 2], png[i + 3]]) as usize;
        if &png[i + 4..i + 8] == ty {
            return Some(png[i..i + 12 + len].to_vec());
        }
        i += 12 + len;
    }
    None
}

/// A 12×9 RGB8 source with enough structure to filter and compress.
fn rgb_source() -> (Vec<u8>, Dimensions) {
    (
        sample_bytes(12, 9, libpng_oracle::COLOR_RGB, 8, 11),
        Dimensions::new(12, 9).expect("valid"),
    )
}

/// The encoder configured with every other ancillary chunk this crate writes, so the store's
/// placement is asserted against all of them at once.
fn everything_else() -> PngEncoder {
    PngEncoder::new()
        .with_gamma(1.0 / 2.2)
        .with_srgb(SrgbIntent::Perceptual)
        .with_chromaticities((0.3127, 0.3290), (0.64, 0.33), (0.30, 0.60), (0.15, 0.06))
        .with_icc_profile("Tiny", &tiny_icc_profile())
        .with_significant_bits(&[8, 8, 8, 8])
        .with_background_rgb(0, 0, 0)
        .with_physical_dimensions(2835, 2835, PhysicalUnit::Meter)
        .with_time(2026, 9, 6, 1, 2, 3)
        .with_text("Title", "placement")
        .with_compressed_text("Comment", "zlib body")
        .with_international_text("Note", "utf-8")
        .with_exif(&tiny_exif())
        .with_xmp("<x:xmpmeta/>")
}

/// §A.3.2 asks that `caBX` precede `IDAT`; the encoder puts it *immediately* before the first
/// `IDAT`, after every other ancillary chunk — colour, metadata, text, `PLTE` and `tRNS` — so
/// that nothing whose size could shift the store follows it. Exactly one store, whatever else
/// is set and whichever candidate the auto-reduce race keeps.
#[test]
fn cabx_is_the_chunk_immediately_before_idat_after_every_other_chunk() {
    // Few colours with transparency: the palette candidate is in play.
    let rgba: Vec<u8> = (0..64u8)
        .flat_map(|i| {
            [
                i % 4 * 60,
                200,
                i % 3 * 90,
                if i % 5 == 0 { 0 } else { 255 },
            ]
        })
        .collect();
    let image =
        ImageRef::<Rgba8>::new(&rgba, Dimensions::new(8, 8).expect("valid")).expect("image");
    let png = everything_else()
        .with_auto_reduce(true)
        .with_c2pa(&store(48))
        .encode_to_vec(image)
        .expect("encode");
    let types = chunk_types(&png);
    let idat = types.iter().position(|t| t == b"IDAT").expect("IDAT");
    assert_eq!(types[idat - 1], *b"caBX", "{types:?}");
    assert_eq!(types.iter().filter(|t| *t == b"caBX").count(), 1);
    assert_eq!(types.first(), Some(b"IHDR"));
    assert_eq!(types.last(), Some(b"IEND"));

    // The indexed path, whose PLTE and tRNS the caller supplies, orders the same way.
    let palette = PngPalette::with_transparency(&[[1, 2, 3], [4, 5, 6]], &[9]).expect("palette");
    let indices = [0u8, 1, 1, 0, 1, 0];
    let image =
        ImageRef::<Indexed8>::new(&indices, Dimensions::new(3, 2).expect("valid")).expect("image");
    let mut png = Vec::new();
    everything_else()
        .with_c2pa(&store(16))
        .encode_indexed8(image, &palette, &mut png)
        .expect("encode");
    let types = chunk_types(&png);
    let idat = types.iter().position(|t| t == b"IDAT").expect("IDAT");
    assert_eq!(types[idat - 1], *b"caBX", "{types:?}");
    let plte = types.iter().position(|t| t == b"PLTE").expect("PLTE");
    let trns = types.iter().position(|t| t == b"tRNS").expect("tRNS");
    assert!(plte < trns && trns < idat - 1, "{types:?}");
    // No `encode_with_report` on this path: the deconstruct report names the same chunk.
    let span = deconstruct(&png)
        .expect("deconstruct")
        .c2pa()
        .expect("span");
    assert_eq!(&png[span.payload], &store(16)[..]);
}

/// A reservation is a `caBX` whose payload is exactly `len` zero bytes — no slack — and it is
/// byte-identical to embedding `len` explicit zeros. Zero is a length too.
#[test]
fn a_reservation_is_a_zero_payload_of_exactly_the_requested_length() {
    let (pixels, dims) = rgb_source();
    let image = ImageRef::<Rgb8>::new(&pixels, dims).expect("image");
    let (png, report) = PngEncoder::new()
        .with_c2pa_reserved(40)
        .encode_with_report(image)
        .expect("encode");
    let span = report.c2pa.expect("a reservation is reported");
    assert_eq!(span.payload.len(), 40);
    assert_eq!(span.chunk.len(), 40 + 12);
    assert!(png[span.payload.clone()].iter().all(|&b| b == 0));
    assert_eq!(
        &png[span.chunk.start..span.chunk.start + 4],
        &40u32.to_be_bytes()
    );
    assert_eq!(&png[span.chunk.start + 4..span.chunk.start + 8], b"caBX");

    let explicit = PngEncoder::new()
        .with_c2pa(&[0; 40])
        .encode_to_vec(image)
        .expect("encode");
    assert_eq!(png, explicit, "a reservation is an explicit all-zero store");

    let (empty, report) = PngEncoder::new()
        .with_c2pa_reserved(0)
        .encode_with_report(image)
        .expect("encode");
    let span = report.c2pa.expect("an empty reservation is still a chunk");
    assert_eq!(span.payload.len(), 0);
    assert_eq!(&empty[span.chunk.clone()][..8], b"\0\0\0\0caBX");
}

/// The reserve-then-fill contract: encoding again with a store of the reserved length changes
/// **only** bytes inside the chunk's span — the payload and its CRC — and not the length, the
/// type, or any byte before or after the chunk. Two different equal-length stores likewise
/// differ only there. This is what makes a hash computed over the reserved file, with the span
/// excluded (§18.5.4), still hold over the filled one.
#[test]
fn filling_a_reservation_changes_only_the_chunk_span() {
    let (pixels, dims) = rgb_source();
    let image = ImageRef::<Rgb8>::new(&pixels, dims).expect("image");
    let (reserved, report) = everything_else()
        .with_c2pa_reserved(64)
        .encode_with_report(image)
        .expect("encode");
    let span = report.c2pa.expect("span");
    let first = store(64);
    let second: Vec<u8> = first.iter().map(|b| !b).collect();
    let filled = everything_else()
        .with_c2pa(&first)
        .encode_to_vec(image)
        .expect("encode");
    let refilled = everything_else()
        .with_c2pa(&second)
        .encode_to_vec(image)
        .expect("encode");

    for (label, a, b) in [
        ("reserved vs filled", &reserved, &filled),
        ("filled vs refilled", &filled, &refilled),
    ] {
        assert_eq!(a.len(), b.len(), "{label}: equal lengths");
        let differing: Vec<usize> = (0..a.len()).filter(|&i| a[i] != b[i]).collect();
        assert!(!differing.is_empty(), "{label}: the stores differ");
        assert!(
            differing.iter().all(|i| span.chunk.contains(i)),
            "{label}: bytes outside the caBX span changed at {:?}",
            differing
                .iter()
                .filter(|i| !span.chunk.contains(i))
                .collect::<Vec<_>>()
        );
        assert_eq!(
            a[span.chunk.start..span.chunk.start + 8],
            b[span.chunk.start..span.chunk.start + 8],
            "{label}: length and type are unchanged"
        );
        assert_ne!(
            a[span.chunk.end - 4..span.chunk.end],
            b[span.chunk.end - 4..span.chunk.end],
            "{label}: the CRC follows the payload"
        );
    }
    assert_eq!(&filled[span.payload.clone()], &first[..]);
    assert_eq!(&refilled[span.payload.clone()], &second[..]);
    // The filled file's own report names the very same span.
    assert_eq!(
        deconstruct(&filled).expect("deconstruct").c2pa(),
        Some(span)
    );
}

/// The exclusion span is the **whole** chunk — length, type, payload and CRC — at offsets a
/// reader can compute by hand: after the signature (8) and the framed IHDR (25), a 7-byte store
/// occupies `33..52` with its payload at `41..48`. The span is one of the report's claimed
/// segments, and the payload it brackets is what the decoder surfaces.
#[test]
fn the_exclusion_span_is_the_whole_chunk_at_known_offsets() {
    let png = png_from_chunks(&[
        chunk(b"IHDR", &ihdr_payload(3, 2, 8, 2, 0)),
        chunk(b"caBX", b"jumbf!!"),
        chunk(b"IDAT", &zlib(&[0u8; 20])),
        chunk(b"IEND", &[]),
    ]);
    let report = deconstruct(&png).expect("deconstruct");
    let span = report.c2pa().expect("caBX");
    assert_eq!(span.chunk, 33..52);
    assert_eq!(span.payload, 41..48);
    assert_eq!(
        &png[span.chunk.start..span.chunk.start + 4],
        &7u32.to_be_bytes()
    );
    assert_eq!(&png[span.chunk.start + 4..span.chunk.start + 8], b"caBX");
    assert_eq!(&png[span.payload.clone()], b"jumbf!!");
    assert!(
        report.segments.iter().any(|s| s.range == span.chunk
            && matches!(
                s.kind,
                SegmentKind::Chunk {
                    chunk_type: [b'c', b'a', b'B', b'X'],
                    payload_len: 7,
                    crc_ok: true,
                }
            )),
        "the span is a claimed segment: {:?}",
        report.segments
    );
    assert_eq!(
        gamut_png::metadata(&png).expect("metadata").c2pa.as_deref(),
        Some(&b"jumbf!!"[..])
    );
}

/// Neither setter set: no chunk, no span from either report, nothing surfaced on decode.
#[test]
fn without_a_store_there_is_no_chunk_and_no_span() {
    let (pixels, dims) = rgb_source();
    let image = ImageRef::<Rgb8>::new(&pixels, dims).expect("image");
    let (png, report) = everything_else().encode_with_report(image).expect("encode");
    assert_eq!(report.c2pa, None);
    assert!(!chunk_types(&png).contains(b"caBX"));
    assert_eq!(deconstruct(&png).expect("deconstruct").c2pa(), None);
    let decoded = PngDecoder::new().decode(&png).expect("decode");
    assert_eq!(decoded.c2pa, None);
    assert_eq!(decoded.c2pa_ignored, 0);
}

/// Both read entry points surface the store byte for byte, and the last of the two setters
/// wins — a file carries exactly one store.
#[test]
fn decode_and_metadata_surface_the_store_verbatim_and_the_last_setter_wins() {
    let (pixels, dims) = rgb_source();
    let image = ImageRef::<Rgb8>::new(&pixels, dims).expect("image");
    let store = store(300);
    let png = PngEncoder::new()
        .with_c2pa(&store)
        .encode_to_vec(image)
        .expect("encode");
    let meta = gamut_png::metadata(&png).expect("metadata");
    assert_eq!(meta.c2pa.as_deref(), Some(&store[..]));
    assert_eq!(meta.c2pa_ignored, 0);
    let decoded = PngDecoder::new().decode(&png).expect("decode");
    assert_eq!(decoded.c2pa, meta.c2pa);
    assert_eq!(decoded.c2pa_ignored, 0);

    let reserved_last = PngEncoder::new()
        .with_c2pa(&store)
        .with_c2pa_reserved(5)
        .encode_to_vec(image)
        .expect("encode");
    assert_eq!(
        gamut_png::metadata(&reserved_last).expect("metadata").c2pa,
        Some(vec![0; 5])
    );
    let store_last = PngEncoder::new()
        .with_c2pa_reserved(5)
        .with_c2pa(&store)
        .encode_to_vec(image)
        .expect("encode");
    assert_eq!(
        gamut_png::metadata(&store_last).expect("metadata").c2pa,
        Some(store)
    );
}

/// Exactly one store per file: the first `caBX` is the store, a second is counted, never
/// concatenated onto the first and never merged into its span.
#[test]
fn the_first_store_wins_and_a_second_is_counted_not_merged() {
    let png = png_from_chunks(&[
        chunk(b"IHDR", &ihdr_payload(3, 2, 8, 2, 0)),
        chunk(b"caBX", b"first"),
        chunk(b"caBX", b"second"),
        chunk(b"IDAT", &zlib(&[0u8; 20])),
        chunk(b"IEND", &[]),
    ]);
    let meta = gamut_png::metadata(&png).expect("metadata");
    assert_eq!(meta.c2pa.as_deref(), Some(&b"first"[..]));
    assert_eq!(meta.c2pa_ignored, 1);
    let decoded = PngDecoder::new().decode(&png).expect("decode");
    assert_eq!(decoded.c2pa.as_deref(), Some(&b"first"[..]));
    assert_eq!(decoded.c2pa_ignored, 1);

    let report = deconstruct(&png).expect("deconstruct");
    let span = report.c2pa().expect("span");
    assert_eq!(span.chunk, 33..50, "the first chunk, 5 + 12 bytes");
    assert_eq!(&png[span.payload], b"first");
    assert_eq!(report.chunk(b"caBX").expect("stats").count, 2);
}

/// A `caBX` positioned **after** `IDAT` is never the store: §A.3.2 puts the store before `IDAT`
/// and calls data after it bad-form, so accepting one would let anybody append a store to a
/// finished file and have it read back as that file's provenance. It is ignored as the store but
/// not hidden — the decode counts it, and the byte accounting still shows the chunk.
#[test]
fn a_cabx_after_idat_is_never_the_store_but_is_still_visible() {
    let appended = png_from_chunks(&[
        chunk(b"IHDR", &ihdr_payload(3, 2, 8, 2, 0)),
        chunk(b"IDAT", &zlib(&[0u8; 20])),
        chunk(b"caBX", b"appended store"),
        chunk(b"IEND", &[]),
    ]);
    let meta = gamut_png::metadata(&appended).expect("metadata");
    assert_eq!(meta.c2pa, None, "an appended chunk is not the file's store");
    assert_eq!(meta.c2pa_ignored, 1, "but it is counted, not hidden");
    let decoded = PngDecoder::new().decode(&appended).expect("decode");
    assert_eq!(decoded.c2pa, None);
    assert_eq!(decoded.c2pa_ignored, 1);

    // The byte accounting still sees the chunk; it just does not call it the store.
    let report = deconstruct(&appended).expect("deconstruct");
    assert_eq!(report.c2pa(), None, "no store span for an appended chunk");
    assert_eq!(report.chunk(b"caBX").expect("stats").count, 1);

    // A real store before IDAT is unaffected by a second one appended after it: the first wins
    // and the appended one is counted, so the two rules compose.
    let both = png_from_chunks(&[
        chunk(b"IHDR", &ihdr_payload(3, 2, 8, 2, 0)),
        chunk(b"caBX", b"real store"),
        chunk(b"IDAT", &zlib(&[0u8; 20])),
        chunk(b"caBX", b"appended"),
        chunk(b"IEND", &[]),
    ]);
    let meta = gamut_png::metadata(&both).expect("metadata");
    assert_eq!(meta.c2pa.as_deref(), Some(&b"real store"[..]));
    assert_eq!(meta.c2pa_ignored, 1);
    let span = deconstruct(&both)
        .expect("deconstruct")
        .c2pa()
        .expect("span");
    assert_eq!(&both[span.payload], b"real store");
}

/// The two ignore-reasons add up rather than replacing one another: a duplicate before `IDAT`
/// and two chunks after it are three ignored chunks, counted the same way by both entry points.
#[test]
fn ignored_stores_before_and_after_idat_are_counted_together() {
    let png = png_from_chunks(&[
        chunk(b"IHDR", &ihdr_payload(3, 2, 8, 2, 0)),
        chunk(b"caBX", b"the store"),
        chunk(b"caBX", b"duplicate"),
        chunk(b"IDAT", &zlib(&[0u8; 20])),
        chunk(b"caBX", b"appended one"),
        chunk(b"caBX", b"appended two"),
        chunk(b"IEND", &[]),
    ]);
    let meta = gamut_png::metadata(&png).expect("metadata");
    assert_eq!(meta.c2pa.as_deref(), Some(&b"the store"[..]));
    assert_eq!(meta.c2pa_ignored, 3, "one duplicate plus two appended");
    let decoded = PngDecoder::new().decode(&png).expect("decode");
    assert_eq!(decoded.c2pa_ignored, 3, "decode agrees with metadata");
}

/// The reserve-then-fill flow end to end, the way a signer runs it: reserve, take the span,
/// fill in place, read the store back. The filled file is byte-identical to one encoded with
/// the store set from the start, which is what makes the two routes interchangeable — and the
/// signer's hash, taken over the reserved file with the span excluded, still describes it.
#[test]
fn a_reserved_store_is_filled_in_place_to_the_same_bytes_as_a_second_encode() {
    let (pixels, dims) = rgb_source();
    let image = ImageRef::<Rgb8>::new(&pixels, dims).expect("image");
    let finished = store(64);

    let (mut reserved, report) = everything_else()
        .with_c2pa_reserved(64)
        .encode_with_report(image)
        .expect("encode");
    let span = report.c2pa.expect("span");
    let outside_before: Vec<u8> = (0..reserved.len())
        .filter(|i| !span.chunk.contains(i))
        .map(|i| reserved[i])
        .collect();

    fill_c2pa(&mut reserved, &span, &finished).expect("fill");

    let reencoded = everything_else()
        .with_c2pa(&finished)
        .encode_to_vec(image)
        .expect("encode");
    assert_eq!(
        reserved, reencoded,
        "filling in place lands on the encoder's own bytes"
    );

    let outside_after: Vec<u8> = (0..reserved.len())
        .filter(|i| !span.chunk.contains(i))
        .map(|i| reserved[i])
        .collect();
    assert_eq!(
        outside_before, outside_after,
        "no byte outside the span moved"
    );

    // The filled store reads back through the ordinary decode path, so its CRC is right.
    assert_eq!(
        gamut_png::metadata(&reserved).expect("metadata").c2pa,
        Some(finished)
    );
    let typed: ImageBuf<Rgb8> = PngDecoder::new().decode_image(&reserved).expect("decode");
    assert_eq!(typed.as_samples(), pixels, "the pixels are untouched");
}

/// The indexed route (`encode_indexed8` has no report of its own): take the span from the byte
/// accounting, fill in place, read it back.
#[test]
fn an_indexed_encode_reserves_and_fills_through_the_report() {
    let palette = PngPalette::new(&[[1, 2, 3], [4, 5, 6]]).expect("palette");
    let indices = [0u8, 1, 1, 0, 1, 0];
    let image =
        ImageRef::<Indexed8>::new(&indices, Dimensions::new(3, 2).expect("valid")).expect("image");
    let mut png = Vec::new();
    PngEncoder::new()
        .with_c2pa_reserved(24)
        .encode_indexed8(image, &palette, &mut png)
        .expect("encode");

    let span = deconstruct(&png)
        .expect("deconstruct")
        .c2pa()
        .expect("span");
    let finished = store(24);
    fill_c2pa(&mut png, &span, &finished).expect("fill");
    assert_eq!(
        gamut_png::metadata(&png).expect("metadata").c2pa,
        Some(finished)
    );
}

/// The budget-skipped store is counted like every other chunk the walk declines to surface, so
/// the count means "CRC-valid `caBX` chunks in the datastream that are not the store" and not
/// "chunks somebody appended". Both entry points agree.
#[test]
fn a_store_past_the_budget_is_counted_among_the_ignored() {
    let (pixels, dims) = rgb_source();
    let image = ImageRef::<Rgb8>::new(&pixels, dims).expect("image");
    let png = PngEncoder::new()
        .with_c2pa(&store(1000))
        .encode_to_vec(image)
        .expect("encode");

    let generous = PngDecoder::new().with_max_metadata_bytes(1000);
    let meta = generous.metadata(&png).expect("metadata");
    assert!(meta.c2pa.is_some());
    assert_eq!(meta.c2pa_ignored, 0, "an admitted store is not ignored");

    let tight = PngDecoder::new().with_max_metadata_bytes(999);
    let meta = tight.metadata(&png).expect("metadata");
    assert_eq!(meta.c2pa, None);
    assert_eq!(
        meta.c2pa_ignored, 1,
        "the store the budget skipped is counted"
    );
    let decoded = tight.decode(&png).expect("decode");
    assert_eq!(decoded.c2pa_ignored, 1, "decode agrees with metadata");
}

/// What the count deliberately does **not** see: a `caBX` after `IEND` is a trailer, not part of
/// the datastream (§13.2), so neither metadata walk reaches it and it is counted zero. The byte
/// accounting is where that shape shows up, as a trailer segment — which is what the field's
/// docs point a caller at.
#[test]
fn a_cabx_after_iend_is_a_trailer_the_report_sees_and_the_count_does_not() {
    let mut png = png_from_chunks(&[
        chunk(b"IHDR", &ihdr_payload(3, 2, 8, 2, 0)),
        chunk(b"IDAT", &zlib(&[0u8; 20])),
        chunk(b"IEND", &[]),
    ]);
    let datastream_len = png.len();
    png.extend_from_slice(&chunk(b"caBX", b"appended after IEND"));

    let meta = gamut_png::metadata(&png).expect("metadata");
    assert_eq!(meta.c2pa, None);
    assert_eq!(meta.c2pa_ignored, 0, "a trailer is not in the datastream");

    let report = deconstruct(&png).expect("deconstruct");
    assert_eq!(report.c2pa(), None, "a trailer is not the store either");
    let trailer = report
        .segments
        .iter()
        .find(|segment| segment.kind == SegmentKind::Trailer)
        .expect("the appended bytes are accounted as a trailer");
    assert_eq!(trailer.range, datastream_len..png.len());
    assert!(report.is_fully_classified());
}

/// A `caBX` whose CRC does not match is skipped on decode (§13.1) — it is not the store and
/// it is not a duplicate either, since it never reaches the metadata pass — and the exclusion
/// span names the CRC-valid store the decoder actually surfaces, not the damaged bytes before
/// it. The damage is still visible: the report accounts both chunks and is not intact.
#[test]
fn a_cabx_with_a_bad_crc_is_neither_the_store_nor_the_exclusion_span() {
    let mut damaged = chunk(b"caBX", b"corrupt");
    let last = damaged.len() - 1;
    damaged[last] ^= 0xFF;
    let png = png_from_chunks(&[
        chunk(b"IHDR", &ihdr_payload(3, 2, 8, 2, 0)),
        damaged,
        chunk(b"caBX", b"valid"),
        chunk(b"IDAT", &zlib(&[0u8; 20])),
        chunk(b"IEND", &[]),
    ]);
    let meta = gamut_png::metadata(&png).expect("metadata");
    assert_eq!(meta.c2pa.as_deref(), Some(&b"valid"[..]));
    assert_eq!(meta.c2pa_ignored, 0);

    let report = deconstruct(&png).expect("deconstruct");
    let span = report.c2pa().expect("the valid store");
    assert_eq!(&png[span.payload], b"valid");
    assert_eq!(span.chunk.start, 33 + 12 + 7, "after the damaged chunk");
    assert_eq!(report.chunk(b"caBX").expect("stats").count, 2);
    assert!(!report.is_intact());
}

/// The store is attacker-sized like every ancillary payload, so it is charged to the decoder's
/// cumulative metadata budget: a budget of exactly its length admits it, one byte less skips
/// it — without error, and without touching the pixels.
#[test]
fn a_store_past_the_metadata_budget_is_skipped_not_an_error() {
    let (pixels, dims) = rgb_source();
    let image = ImageRef::<Rgb8>::new(&pixels, dims).expect("image");
    let store = store(1000);
    let png = PngEncoder::new()
        .with_c2pa(&store)
        .encode_to_vec(image)
        .expect("encode");

    let exact = PngDecoder::new().with_max_metadata_bytes(1000);
    assert_eq!(
        exact.metadata(&png).expect("metadata").c2pa.as_deref(),
        Some(&store[..])
    );
    let tight = PngDecoder::new().with_max_metadata_bytes(999);
    assert_eq!(tight.metadata(&png).expect("metadata").c2pa, None);
    let decoded = tight.decode(&png).expect("decode still succeeds");
    assert_eq!(decoded.c2pa, None);
    let typed: ImageBuf<Rgb8> = tight.decode_image(&png).expect("typed decode");
    assert_eq!(typed.as_samples(), pixels);
}

/// The libpng oracle. libpng has no C2PA support and carries `caBX` as an unknown chunk, which
/// is exactly what proves the framing: for the same payload it must produce the same twelve
/// framing bytes — length, type and CRC — around the same store, or one of the two is wrong
/// about §5.3. It then decodes gamut's file pixel-exact with the chunk in place, and gamut reads
/// the store back from libpng's file, where libpng frames unknown chunks right after IHDR.
#[test]
fn gamut_frames_cabx_byte_for_byte_as_libpng_does() {
    let (pixels, dims) = rgb_source();
    let image = ImageRef::<Rgb8>::new(&pixels, dims).expect("image");
    let store = store(77);
    let gamut = PngEncoder::new()
        .with_c2pa(&store)
        .encode_to_vec(image)
        .expect("encode");
    let reference = libpng_with_extra_chunks(12, 9, &[(*b"caBX", &store)]);

    let ours = framed_chunk(&gamut, b"caBX").expect("gamut wrote the chunk");
    let theirs = framed_chunk(&reference, b"caBX").expect("libpng wrote the chunk");
    assert_eq!(
        ours, theirs,
        "length, type, payload and CRC agree with libpng"
    );
    assert_eq!(ours.len(), 12 + 77);

    let decoded = libpng_oracle::decode(&gamut);
    assert_eq!((decoded.width, decoded.height), (12, 9));
    assert_eq!(decoded.pixels, pixels, "libpng decodes past the chunk");

    let meta = gamut_png::metadata(&reference).expect("metadata");
    assert_eq!(meta.c2pa.as_deref(), Some(&store[..]));
    let span = deconstruct(&reference)
        .expect("deconstruct")
        .c2pa()
        .expect("span");
    assert_eq!(
        span.chunk,
        33..33 + 12 + 77,
        "libpng put it right after IHDR"
    );
    let typed: ImageBuf<Rgb8> = PngDecoder::new()
        .decode_image(&reference)
        .expect("gamut decodes libpng's file");
    assert_eq!(typed.as_samples(), pixels);
}
