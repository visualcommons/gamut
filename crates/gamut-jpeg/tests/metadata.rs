//! Integration tests for the APP1/APP2 metadata surface: `metadata()` extraction of EXIF, XMP, and
//! multi-segment ICC payloads, the ICC chunk-consistency error corpus, the skip rules for foreign
//! or continuation segments, and the encoder's `with_exif`/`with_xmp`/`with_icc_profile`
//! embedding (round-trips, chunk framing at the 65519-byte boundaries, size caps, segment order).

use gamut_core::{Dimensions, EncodeImage, ErrorKind, Gray8, ImageRef, Rgb8};
use gamut_jpeg::{
    JpegDecoder, JpegEncoder, Process, ProgressiveScript, RawMetadata as JpegMetadata,
};

/// The APP-segment payloads [`JpegDecoder::metadata`] reads from `data`.
fn metadata(data: &[u8]) -> gamut_core::Result<JpegMetadata> {
    JpegDecoder::new().metadata(data)
}

/// Encodes a minimal valid grayscale JPEG to splice APP segments into.
fn base_jpeg() -> Vec<u8> {
    let pixels = vec![128u8; 64];
    let image = ImageRef::<Gray8>::new(&pixels, Dimensions::new(8, 8).unwrap()).unwrap();
    JpegEncoder::new().encode_to_vec(image).unwrap()
}

/// Builds an APPn marker segment: `0xFF, code`, big-endian length (counting itself), payload.
fn app_segment(code: u8, payload: &[u8]) -> Vec<u8> {
    let mut seg = vec![0xFF, code];
    let len = u16::try_from(payload.len() + 2).unwrap();
    seg.extend_from_slice(&len.to_be_bytes());
    seg.extend_from_slice(payload);
    seg
}

/// Inserts segments immediately after the SOI marker of `jpeg`.
fn splice(jpeg: &[u8], segments: &[Vec<u8>]) -> Vec<u8> {
    let mut out = jpeg[..2].to_vec();
    for seg in segments {
        out.extend_from_slice(seg);
    }
    out.extend_from_slice(&jpeg[2..]);
    out
}

/// Builds one APP2 `ICC_PROFILE` payload (signature, 1-based index, total count, chunk data).
fn icc_payload(index: u8, count: u8, data: &[u8]) -> Vec<u8> {
    let mut payload = b"ICC_PROFILE\0".to_vec();
    payload.push(index);
    payload.push(count);
    payload.extend_from_slice(data);
    payload
}

#[test]
fn plain_stream_has_no_metadata() {
    assert_eq!(metadata(&base_jpeg()).unwrap(), JpegMetadata::default());
}

#[test]
fn exif_app1_is_extracted_with_the_signature_stripped() {
    // A tiny but shaped TIFF stream: little-endian header + zero-entry IFD.
    let tiff = b"II\x2A\x00\x08\x00\x00\x00\x00\x00".to_vec();
    let mut payload = b"Exif\0\0".to_vec();
    payload.extend_from_slice(&tiff);
    let jpeg = splice(&base_jpeg(), &[app_segment(0xE1, &payload)]);
    let meta = metadata(&jpeg).unwrap();
    assert_eq!(meta.exif.as_deref(), Some(tiff.as_slice()));
    assert_eq!(meta.xmp, None);
    assert_eq!(meta.icc, None);
}

#[test]
fn xmp_app1_is_extracted_with_the_uri_stripped() {
    let packet = b"<?xpacket begin=\"\"?><x:xmpmeta/><?xpacket end=\"w\"?>".to_vec();
    let mut payload = b"http://ns.adobe.com/xap/1.0/\0".to_vec();
    payload.extend_from_slice(&packet);
    let jpeg = splice(&base_jpeg(), &[app_segment(0xE1, &payload)]);
    assert_eq!(
        metadata(&jpeg).unwrap().xmp.as_deref(),
        Some(packet.as_slice())
    );
}

#[test]
fn single_chunk_icc_is_extracted() {
    let profile: Vec<u8> = (0..200u32).map(|i| (i % 251) as u8).collect();
    let jpeg = splice(
        &base_jpeg(),
        &[app_segment(0xE2, &icc_payload(1, 1, &profile))],
    );
    assert_eq!(
        metadata(&jpeg).unwrap().icc.as_deref(),
        Some(profile.as_slice())
    );
}

#[test]
fn out_of_order_icc_chunks_reassemble_by_index() {
    // Three distinct chunks delivered 2, 3, 1: reassembly must follow the index bytes, not the
    // segment order.
    let (a, b, c) = (vec![1u8; 5], vec![2u8; 7], vec![3u8; 3]);
    let jpeg = splice(
        &base_jpeg(),
        &[
            app_segment(0xE2, &icc_payload(2, 3, &b)),
            app_segment(0xE2, &icc_payload(3, 3, &c)),
            app_segment(0xE2, &icc_payload(1, 3, &a)),
        ],
    );
    let expected: Vec<u8> = [a, b, c].concat();
    assert_eq!(metadata(&jpeg).unwrap().icc, Some(expected));
}

#[test]
fn icc_chunk_inconsistencies_are_rejected() {
    let base = base_jpeg();
    let cases: &[&[Vec<u8>]] = &[
        // Count disagrees between chunks.
        &[
            app_segment(0xE2, &icc_payload(1, 2, b"aa")),
            app_segment(0xE2, &icc_payload(2, 3, b"bb")),
        ],
        // Index 0 (indices are 1-based).
        &[app_segment(0xE2, &icc_payload(0, 1, b"aa"))],
        // Zero count.
        &[app_segment(0xE2, &icc_payload(1, 0, b"aa"))],
        // Index beyond the count.
        &[app_segment(0xE2, &icc_payload(3, 2, b"aa"))],
        // Duplicated index.
        &[
            app_segment(0xE2, &icc_payload(1, 2, b"aa")),
            app_segment(0xE2, &icc_payload(1, 2, b"bb")),
        ],
        // Declared 2 chunks, delivered 1.
        &[app_segment(0xE2, &icc_payload(1, 2, b"aa"))],
        // Signature present but the index/count bytes are missing.
        &[app_segment(0xE2, b"ICC_PROFILE\0")],
    ];
    for segments in cases {
        let jpeg = splice(&base, segments);
        assert!(
            matches!(metadata(&jpeg), Err(error) if error.kind() == ErrorKind::InvalidInput),
            "case {segments:?} was not rejected"
        );
    }
}

#[test]
fn foreign_app_segments_are_skipped() {
    // A non-ICC APP2 (Exif Flashpix "FPXR"), an ExtendedXMP APP1 continuation, and an unrelated
    // APP1 must all be ignored without error.
    let jpeg = splice(
        &base_jpeg(),
        &[
            app_segment(0xE2, b"FPXR\0not an icc chunk"),
            app_segment(
                0xE1,
                b"http://ns.adobe.com/xmp/extension/\0guid-and-chunk-data",
            ),
            app_segment(0xE1, b"not a known app1 payload"),
        ],
    );
    assert_eq!(metadata(&jpeg).unwrap(), JpegMetadata::default());
}

#[test]
fn duplicate_exif_and_xmp_app1_first_wins() {
    let first = b"Exif\0\0II\x2A\x00first".to_vec();
    let second = b"Exif\0\0II\x2A\x00second".to_vec();
    let xmp_first = b"http://ns.adobe.com/xap/1.0/\0<first/>".to_vec();
    let xmp_second = b"http://ns.adobe.com/xap/1.0/\0<second/>".to_vec();
    let jpeg = splice(
        &base_jpeg(),
        &[
            app_segment(0xE1, &first),
            app_segment(0xE1, &xmp_first),
            app_segment(0xE1, &second),
            app_segment(0xE1, &xmp_second),
        ],
    );
    let meta = metadata(&jpeg).unwrap();
    assert_eq!(meta.exif.as_deref(), Some(&b"II\x2A\x00first"[..]));
    assert_eq!(meta.xmp.as_deref(), Some(&b"<first/>"[..]));
}

#[test]
fn metadata_stops_at_the_scan() {
    // An APP1 spliced after the SOS marker is entropy data territory; metadata() must have stopped.
    let base = base_jpeg();
    let sos = base
        .windows(2)
        .position(|w| w == [0xFF, 0xDA])
        .expect("stream has an SOS");
    let mut payload = b"Exif\0\0".to_vec();
    payload.extend_from_slice(b"II\x2A\x00");
    let seg = app_segment(0xE1, &payload);
    let mut jpeg = base[..sos].to_vec();
    jpeg.extend_from_slice(&seg);
    jpeg.extend_from_slice(&base[sos..]);
    // The segment sits *before* SOS here, so it IS found …
    assert!(metadata(&jpeg).unwrap().exif.is_some());
    // … but appended after the EOI-terminated scan it is not reachable: truncate the walk at SOS.
    let mut after = base.clone();
    after.extend_from_slice(&seg);
    assert_eq!(metadata(&after).unwrap(), JpegMetadata::default());
}

/// Collects `(marker, payload)` for every marker segment up to (excluding) the first SOS.
fn segments(jpeg: &[u8]) -> Vec<(u8, Vec<u8>)> {
    assert_eq!(&jpeg[..2], [0xFF, 0xD8], "missing SOI");
    let mut pos = 2;
    let mut out = Vec::new();
    loop {
        assert_eq!(jpeg[pos], 0xFF, "expected a marker at {pos}");
        let marker = jpeg[pos + 1];
        if marker == 0xDA {
            return out;
        }
        let len = usize::from(u16::from_be_bytes([jpeg[pos + 2], jpeg[pos + 3]]));
        out.push((marker, jpeg[pos + 4..pos + 2 + len].to_vec()));
        pos += 2 + len;
    }
}

/// A distinct-content pseudo-profile so chunk reordering or boundary slips change the bytes.
fn fake_profile(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

#[test]
fn encoded_metadata_round_trips() {
    // All three payloads on one colour stream, read back byte-exact through metadata().
    let tiff = b"II\x2A\x00\x08\x00\x00\x00\x00\x00".to_vec();
    let xmp = b"<?xpacket begin=\"\"?><x:xmpmeta/><?xpacket end=\"w\"?>".to_vec();
    let icc = fake_profile(300);
    let pixels = vec![200u8; 8 * 8 * 3];
    let image = ImageRef::<Rgb8>::new(&pixels, Dimensions::new(8, 8).unwrap()).unwrap();
    let jpeg = JpegEncoder::new()
        .with_exif(&tiff)
        .with_xmp(&xmp)
        .with_icc_profile(&icc)
        .encode_to_vec(image)
        .unwrap();
    let meta = metadata(&jpeg).unwrap();
    assert_eq!(meta.exif, Some(tiff));
    assert_eq!(meta.xmp, Some(xmp));
    assert_eq!(meta.icc, Some(icc));
}

#[test]
fn progressive_streams_carry_the_same_metadata() {
    let tiff = b"MM\x00\x2A\x00\x00\x00\x08".to_vec();
    let icc = fake_profile(64);
    let pixels = vec![10u8; 64];
    let image = ImageRef::<Gray8>::new(&pixels, Dimensions::new(8, 8).unwrap()).unwrap();
    let jpeg = JpegEncoder::new()
        .with_process(Process::Progressive(ProgressiveScript::Libjpeg))
        .with_exif(&tiff)
        .with_icc_profile(&icc)
        .encode_to_vec(image)
        .unwrap();
    let meta = metadata(&jpeg).unwrap();
    assert_eq!(meta.exif, Some(tiff));
    assert_eq!(meta.icc, Some(icc));
}

#[test]
fn prefixed_exif_input_is_not_double_prefixed() {
    // with_exif accepts a blob already carrying the "Exif\0\0" signature; the reader must get the
    // bare TIFF stream back either way.
    let tiff = b"II\x2A\x00\x08\x00\x00\x00".to_vec();
    let mut prefixed = b"Exif\0\0".to_vec();
    prefixed.extend_from_slice(&tiff);
    let pixels = vec![0u8; 64];
    let image = ImageRef::<Gray8>::new(&pixels, Dimensions::new(8, 8).unwrap()).unwrap();
    let jpeg = JpegEncoder::new()
        .with_exif(&prefixed)
        .encode_to_vec(image)
        .unwrap();
    assert_eq!(metadata(&jpeg).unwrap().exif, Some(tiff));
}

#[test]
fn icc_chunking_at_the_segment_boundaries() {
    // 65519 profile bytes fill exactly one APP2 chunk; one more byte forces a second; ~200 KB
    // takes four. Indices are 1-based and every chunk repeats the shared count.
    let pixels = vec![128u8; 64];
    let image = ImageRef::<Gray8>::new(&pixels, Dimensions::new(8, 8).unwrap()).unwrap();
    for (len, want_chunks) in [(65519usize, 1usize), (65520, 2), (200_000, 4)] {
        let profile = fake_profile(len);
        let jpeg = JpegEncoder::new()
            .with_icc_profile(&profile)
            .encode_to_vec(ImageRef::<Gray8>::new(&pixels, image.dimensions()).unwrap())
            .unwrap();
        let app2: Vec<_> = segments(&jpeg)
            .into_iter()
            .filter(|(m, _)| *m == 0xE2)
            .collect();
        assert_eq!(app2.len(), want_chunks, "profile len {len}");
        for (i, (_, payload)) in app2.iter().enumerate() {
            assert!(payload.starts_with(b"ICC_PROFILE\0"));
            assert_eq!(payload[12], i as u8 + 1, "1-based chunk index");
            assert_eq!(payload[13], want_chunks as u8, "shared chunk count");
            // Every chunk but the last is full.
            let data_len = payload.len() - 14;
            if i + 1 < want_chunks {
                assert_eq!(data_len, 65519);
            }
        }
        assert_eq!(metadata(&jpeg).unwrap().icc, Some(profile));
    }
}

#[test]
fn metadata_segments_are_ordered_app0_exif_xmp_icc_before_tables() {
    let pixels = vec![1u8; 64];
    let image = ImageRef::<Gray8>::new(&pixels, Dimensions::new(8, 8).unwrap()).unwrap();
    let jpeg = JpegEncoder::new()
        .with_exif(b"II\x2A\x00")
        .with_xmp(b"<x/>")
        .with_icc_profile(&fake_profile(16))
        .encode_to_vec(image)
        .unwrap();
    let markers: Vec<u8> = segments(&jpeg).iter().map(|(m, _)| *m).collect();
    // JFIF APP0 first (T.871), then EXIF APP1, XMP APP1, ICC APP2, then the DQT tables.
    assert_eq!(&markers[..4], &[0xE0, 0xE1, 0xE1, 0xE2]);
    assert_eq!(markers[4], 0xDB);
}

#[test]
fn oversized_and_empty_metadata_is_rejected_before_writing() {
    let pixels = vec![0u8; 64];
    let image = || ImageRef::<Gray8>::new(&pixels, Dimensions::new(8, 8).unwrap()).unwrap();
    // EXIF is a single APP1: 65527 TIFF bytes fit, 65528 do not.
    let mut out = Vec::new();
    assert!(
        JpegEncoder::new()
            .with_exif(&vec![0u8; 65_527])
            .encode_image(image(), &mut out)
            .is_ok()
    );
    let mut rejected = Vec::new();
    assert!(matches!(
        JpegEncoder::new()
            .with_exif(&vec![0u8; 65_528])
            .encode_image(image(), &mut rejected),
        Err(error) if error.kind() == ErrorKind::InvalidInput
    ));
    // XMP: exactly the 65502-byte StandardXMP cap fits; one byte more needs ExtendedXMP, which
    // is unsupported.
    let mut out = Vec::new();
    assert!(
        JpegEncoder::new()
            .with_xmp(&vec![b' '; 65_502])
            .encode_image(image(), &mut out)
            .is_ok()
    );
    assert!(matches!(
        JpegEncoder::new()
            .with_xmp(&vec![b' '; 65_503])
            .encode_image(image(), &mut rejected),
        Err(error) if error.kind() == ErrorKind::Unsupported
    ));
    // ICC: exactly 255 full chunks fit; one byte more cannot be indexed by the one-byte count.
    let mut out = Vec::new();
    assert!(
        JpegEncoder::new()
            .with_icc_profile(&vec![0u8; 255 * 65_519])
            .encode_image(image(), &mut out)
            .is_ok()
    );
    assert!(matches!(
        JpegEncoder::new()
            .with_icc_profile(&vec![0u8; 255 * 65_519 + 1])
            .encode_image(image(), &mut rejected),
        Err(error) if error.kind() == ErrorKind::InvalidInput
    ));
    // Empty payloads are meaningless and rejected.
    for enc in [
        JpegEncoder::new().with_exif(b""),
        JpegEncoder::new().with_xmp(b""),
        JpegEncoder::new().with_icc_profile(b""),
    ] {
        assert!(matches!(
            enc.encode_image(image(), &mut rejected),
            Err(error) if error.kind() == ErrorKind::InvalidInput
        ));
    }
    // A failed encode writes nothing.
    assert!(rejected.is_empty());
}

#[test]
fn oracle_reads_back_gamut_written_icc_byte_exact() {
    // libjpeg-turbo's jpeg_read_icc_profile is the reference reassembly of the APP2 chunk
    // sequence: a multi-chunk profile written by gamut must come back byte-identical.
    let pixels = vec![90u8; 64];
    let image = ImageRef::<Gray8>::new(&pixels, Dimensions::new(8, 8).unwrap()).unwrap();
    for len in [300usize, 65519, 200_000] {
        let profile = fake_profile(len);
        let jpeg = JpegEncoder::new()
            .with_icc_profile(&profile)
            .encode_to_vec(ImageRef::<Gray8>::new(&pixels, image.dimensions()).unwrap())
            .unwrap();
        let read = libjpeg_oracle::read_icc_profile(&jpeg)
            .expect("oracle accepts the stream")
            .expect("oracle finds the profile");
        assert_eq!(read, profile, "profile len {len}");
    }
}

#[test]
fn gamut_reads_back_oracle_written_icc_byte_exact() {
    // The reverse direction: jpeg_write_icc_profile is the reference producer of the chunk
    // framing, and metadata() must reassemble it identically — including a multi-chunk profile.
    let pixels: Vec<u8> = (0..64u32 * 64 * 3).map(|i| (i % 251) as u8).collect();
    for len in [128usize, 100_000] {
        let profile = fake_profile(len);
        let jpeg = libjpeg_oracle::encode_with_metadata(
            &pixels,
            64,
            64,
            &libjpeg_oracle::EncodeParams::default(),
            None,
            Some(&profile),
        )
        .expect("oracle encodes");
        assert_eq!(
            metadata(&jpeg).unwrap().icc,
            Some(profile),
            "profile len {len}"
        );
    }
}

#[test]
fn exif_interop_matches_the_oracle_marker_capture() {
    // gamut → oracle: the APP1 payload libjpeg-turbo captures is the signature + the TIFF stream.
    let tiff = b"II\x2A\x00\x08\x00\x00\x00\x00\x00".to_vec();
    let pixels = vec![50u8; 64];
    let image = ImageRef::<Gray8>::new(&pixels, Dimensions::new(8, 8).unwrap()).unwrap();
    let jpeg = JpegEncoder::new()
        .with_exif(&tiff)
        .encode_to_vec(image)
        .unwrap();
    let mut expected = b"Exif\0\0".to_vec();
    expected.extend_from_slice(&tiff);
    assert_eq!(
        libjpeg_oracle::read_first_app1(&jpeg).unwrap(),
        Some(expected.clone())
    );

    // oracle → gamut: an APP1 written verbatim by jpeg_write_marker reads back stripped.
    let pixels3: Vec<u8> = vec![70u8; 16 * 16 * 3];
    let jpeg = libjpeg_oracle::encode_with_metadata(
        &pixels3,
        16,
        16,
        &libjpeg_oracle::EncodeParams::default(),
        Some(&expected),
        None,
    )
    .expect("oracle encodes");
    assert_eq!(metadata(&jpeg).unwrap().exif, Some(tiff));
}

#[test]
fn malformed_streams_are_rejected() {
    // Missing SOI.
    assert!(matches!(
        metadata(b"\xFF\xE1\x00\x04ab"),
        Err(error) if error.kind() == ErrorKind::InvalidInput
    ));
    // A standalone marker (RST0) where a segment is expected.
    assert!(matches!(
        metadata(&[0xFF, 0xD8, 0xFF, 0xD0]),
        Err(error) if error.kind() == ErrorKind::InvalidInput
    ));
    // A standalone TEM marker followed by bytes that *would* scan as a plausible empty segment
    // and a valid stream: the standalone-marker rejection itself must fire — without it the walk
    // would misread the marker as segment-bearing and succeed.
    let base = base_jpeg();
    let mut tem = vec![0xFF, 0xD8, 0xFF, 0x01, 0x00, 0x02];
    tem.extend_from_slice(&base[2..]);
    assert!(matches!(metadata(&tem), Err(error) if error.kind() == ErrorKind::InvalidInput));
    // A declared segment length running past the end of the data.
    let truncated = [0xFF, 0xD8, 0xFF, 0xE1, 0xFF, 0xFF, 0x00];
    assert!(matches!(metadata(&truncated), Err(error) if error.kind() == ErrorKind::InvalidInput));
}
