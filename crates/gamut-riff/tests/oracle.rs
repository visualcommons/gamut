//! Differential tests against libwebp's demuxer, gamut-riff's container oracle.
//!
//! Both directions are covered: gamut-riff must read every file libwebp writes, and libwebp must
//! read every file gamut-riff writes. Because gamut-riff codes no bitstream, each test starts from
//! a real libwebp-encoded file and rewraps its codestream — see [`common`].

mod common;

use common::{libwebp_demux, libwebp_encode_lossless, rgb_image};
use gamut_riff::{
    Chunk, FourCc, MetadataChunks, RiffReader, Vp8xHeader, WebpChunkId, WebpLayout, c2pa_span,
    write_extended_preserving, write_simple_lossless,
};

/// The `VP8L` codestream of a freshly libwebp-encoded image, plus its dimensions.
fn libwebp_vp8l(width: u32, height: u32) -> (Vec<u8>, u32, u32) {
    let file = libwebp_encode_lossless(&rgb_image(width, height), width, height);
    let layout = WebpLayout::parse(&file).expect("gamut-riff parses libwebp's own output");
    let (id, payload) = layout.bitstream.expect("libwebp emitted a bitstream");
    assert_eq!(id, WebpChunkId::Vp8l);
    (payload.to_vec(), width, height)
}

#[test]
fn gamut_riff_parses_what_libwebp_writes() {
    // The reference encoder's simple-format output must go through the strict reader untouched —
    // including the pad-byte and ordering checks this crate added.
    for (w, h) in [(1, 1), (2, 3), (17, 5), (64, 64)] {
        let file = libwebp_encode_lossless(&rgb_image(w, h), w, h);
        let layout = WebpLayout::parse(&file).expect("libwebp's output is well-formed");
        assert!(layout.bitstream.is_some(), "{w}x{h}: bitstream found");
        assert_eq!(layout.trailing_bytes, 0, "{w}x{h}: no trailing data");
        assert!(layout.unknown.is_empty(), "{w}x{h}: no unknown chunks");

        // Every chunk the permissive reader yields must also be intact.
        let chunks: Vec<_> = RiffReader::new(&file)
            .expect("header parses")
            .collect::<Result<Vec<_>, _>>()
            .expect("every chunk parses");
        assert_eq!(chunks.len(), 1, "{w}x{h}: a simple file is one chunk");
    }
}

#[test]
fn libwebp_reads_what_gamut_riff_writes() {
    // Rewrap libwebp's own codestream with gamut-riff's simple writer; libwebp must accept the
    // result and agree on the canvas. This pins the 12-byte header and chunk framing byte for byte
    // against the reference parser.
    for (w, h) in [(1, 1), (2, 3), (17, 5), (64, 64)] {
        let (vp8l, _, _) = libwebp_vp8l(w, h);
        let rewrapped = write_simple_lossless(&vp8l).expect("write");
        let view = libwebp_demux(&rewrapped).expect("libwebp accepts gamut-riff's container");
        assert_eq!((view.canvas_width, view.canvas_height), (w, h));
        assert!(view.metadata.is_empty(), "no metadata was embedded");
    }
}

#[test]
fn libwebp_agrees_on_the_extended_container_and_its_metadata() {
    // The full extended layout gamut-riff writes — VP8X, ICCP, bitstream, EXIF, XMP — parsed by the
    // reference demuxer, which must recover each metadata payload byte for byte.
    let (w, h) = (24u32, 9u32);
    let (vp8l, _, _) = libwebp_vp8l(w, h);
    let (icc, exif, xmp) = (
        &b"an ICC profile, opaque here"[..],
        &b"exif payload"[..],
        &b"<x:xmpmeta/>"[..],
    );

    let header = Vp8xHeader {
        canvas_width: w,
        canvas_height: h,
        ..Default::default()
    };
    let file = write_extended_preserving(
        &header,
        &MetadataChunks {
            icc: Some(icc),
            exif: Some(exif),
            xmp: Some(xmp),
            c2pa: None,
        },
        &[(FourCc::VP8L, &vp8l)],
        &[],
    )
    .expect("write");

    let view = libwebp_demux(&file).expect("libwebp accepts the extended container");
    assert_eq!((view.canvas_width, view.canvas_height), (w, h));
    assert_eq!(
        view.metadata
            .iter()
            .map(|c| (c.fourcc, c.payload.as_slice()))
            .collect::<Vec<_>>(),
        vec![(*b"ICCP", icc), (*b"EXIF", exif), (*b"XMP ", xmp)],
        "libwebp recovers each metadata payload verbatim, in the spec's order"
    );
}

#[test]
fn libwebp_tolerates_the_unknown_chunks_gamut_riff_preserves() {
    // "Readers SHOULD ignore these chunks" (RFC 9649 §2.7.1.6) — so re-emitting a preserved unknown
    // chunk must not disturb the reference parser, which is what makes preservation safe to do.
    let (w, h) = (12u32, 12u32);
    let (vp8l, _, _) = libwebp_vp8l(w, h);
    let odd = FourCc::from(*b"XYZW");
    let header = Vp8xHeader {
        canvas_width: w,
        canvas_height: h,
        ..Default::default()
    };
    let file = write_extended_preserving(
        &header,
        &MetadataChunks {
            exif: Some(b"exif payload"),
            ..Default::default()
        },
        &[(FourCc::VP8L, &vp8l)],
        &[Chunk {
            fourcc: odd,
            payload: b"private payload",
        }],
    )
    .expect("write");

    let view = libwebp_demux(&file).expect("libwebp ignores the unknown chunk");
    assert_eq!((view.canvas_width, view.canvas_height), (w, h));
    assert_eq!(view.metadata.len(), 1, "only EXIF is metadata");
    assert_eq!(view.metadata[0].payload, b"exif payload");

    // And gamut-riff still finds it.
    let layout = WebpLayout::parse(&file).expect("parse");
    assert_eq!(layout.unknown.len(), 1);
    assert_eq!(layout.unknown[0].fourcc, odd);
}

#[test]
fn an_odd_sized_payload_round_trips_through_libwebp() {
    // The pad byte is the one place the two implementations could silently disagree about framing:
    // an odd metadata payload must not shift libwebp's view of the following chunk.
    let (w, h) = (8u32, 8u32);
    let (vp8l, _, _) = libwebp_vp8l(w, h);
    let odd_exif = &b"odd"[..]; // 3 bytes -> one pad byte
    assert_eq!(odd_exif.len() % 2, 1);

    let header = Vp8xHeader {
        canvas_width: w,
        canvas_height: h,
        ..Default::default()
    };
    let file = write_extended_preserving(
        &header,
        &MetadataChunks {
            exif: Some(odd_exif),
            xmp: Some(b"<x/>"),
            ..Default::default()
        },
        &[(FourCc::VP8L, &vp8l)],
        &[],
    )
    .expect("write");

    let view = libwebp_demux(&file).expect("libwebp accepts the padded chunk");
    assert_eq!(
        view.metadata
            .iter()
            .map(|c| (c.fourcc, c.payload.as_slice()))
            .collect::<Vec<_>>(),
        vec![(*b"EXIF", odd_exif), (*b"XMP ", &b"<x/>"[..])],
        "the pad byte is framing, never part of a payload"
    );
}

#[test]
fn libwebp_reads_a_file_carrying_a_c2pa_store_unchanged() {
    // C2PA 2.4 §A.3.7 puts the manifest store in a `C2PA` chunk, which RFC 9649 does not define —
    // so to the reference demuxer it is an unknown chunk it "SHOULD ignore" (§2.7.1.6). Embedding
    // one must therefore change nothing libwebp sees: same canvas, same metadata chunks, and the
    // store-free file byte for byte as the prefix, so nothing ahead of the store moved.
    let (w, h) = (20u32, 7u32);
    let (vp8l, _, _) = libwebp_vp8l(w, h);
    let header = Vp8xHeader {
        canvas_width: w,
        canvas_height: h,
        ..Default::default()
    };
    let metadata = MetadataChunks {
        icc: Some(b"an ICC profile"),
        exif: Some(b"exif payload"),
        xmp: Some(b"<x:xmpmeta/>"),
        c2pa: None,
    };
    let without = write_extended_preserving(&header, &metadata, &[(FourCc::VP8L, &vp8l)], &[])
        .expect("write");
    let store = &b"a C2PA manifest store"[..];
    let with_store = write_extended_preserving(
        &header,
        &MetadataChunks {
            c2pa: Some(store),
            ..metadata
        },
        &[(FourCc::VP8L, &vp8l)],
        &[],
    )
    .expect("write");

    assert_eq!(
        with_store[12..without.len()],
        without[12..],
        "the store is appended; nothing before it moves"
    );

    let plain = libwebp_demux(&without).expect("libwebp accepts the store-free file");
    let signed = libwebp_demux(&with_store).expect("libwebp accepts the file carrying the store");
    assert_eq!(
        (signed.canvas_width, signed.canvas_height),
        (plain.canvas_width, plain.canvas_height),
        "the canvas is untouched"
    );
    assert_eq!(
        signed.metadata, plain.metadata,
        "libwebp recovers the same metadata chunks, and never the store among them"
    );

    // And the store is exactly where gamut-riff says it is.
    let span = c2pa_span(&with_store)
        .expect("parse")
        .expect("the store was embedded");
    assert_eq!(&with_store[span.start..span.start + 4], b"C2PA");
    assert_eq!(&with_store[span.start + 8..span.end], store);
}

#[test]
fn both_implementations_reject_a_file_size_that_overruns_the_buffer() {
    // Where gamut-riff is strict, the reference parser should be too — otherwise the strictness is
    // gamut-riff inventing a rule rather than enforcing one.
    let (w, h) = (16u32, 16u32);
    let mut file = libwebp_encode_lossless(&rgb_image(w, h), w, h);
    let overrun = u32::try_from(file.len()).unwrap() + 64;
    file[4..8].copy_from_slice(&overrun.to_le_bytes());

    assert!(
        WebpLayout::parse(&file).is_err(),
        "gamut-riff rejects the overrunning file size"
    );
    assert!(
        libwebp_demux(&file).is_none(),
        "libwebp rejects it too, so the two agree"
    );
}
