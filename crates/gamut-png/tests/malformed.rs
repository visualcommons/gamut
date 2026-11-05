//! Hostile-input rejection corpus: hand-crafted byte streams asserting the decoder's error
//! *policy* — what is a hard error (critical damage), what is skipped (ancillary damage), and
//! what is refused before allocation (limit and overflow guards). No oracle is involved: libpng
//! never sees malformed bytes.

mod common;

use common::{SIGNATURE, chunk, ihdr_payload, minimal_png, png_from_chunks, zlib};
use gamut_core::{DecodeImage, Error, ErrorKind, Gray8, ImageBuf, Rgb8};
use gamut_png::PngDecoder;

fn decode(png: &[u8]) -> Result<gamut_png::DecodedPng, Error> {
    PngDecoder::new().decode(png)
}

fn assert_invalid(png: &[u8], context: &str) {
    match decode(png) {
        Err(error) if error.kind() == ErrorKind::InvalidInput => {}
        other => panic!("{context}: expected InvalidInput, got {other:?}"),
    }
}

fn assert_unsupported(png: &[u8], context: &str) {
    match decode(png) {
        Err(error) if error.kind() == ErrorKind::Unsupported => {}
        other => panic!("{context}: expected Unsupported, got {other:?}"),
    }
}

/// A valid indexed 4×2 depth-1 PNG (two palette entries), assembled by hand.
fn indexed_png() -> Vec<u8> {
    // Two rows of 4 one-bit indices: 1010, 0110 -> packed 0xA0, 0x60; filter byte 0 each.
    let stream = [0u8, 0xA0, 0, 0x60];
    png_from_chunks(&[
        chunk(b"IHDR", &ihdr_payload(4, 2, 1, 3, 0)),
        chunk(b"PLTE", &[10, 20, 30, 200, 210, 220]),
        chunk(b"IDAT", &zlib(&stream)),
        chunk(b"IEND", &[]),
    ])
}

#[test]
fn baseline_fixtures_actually_decode() {
    // The corpus below mutates these; prove the unmutated forms are valid.
    assert!(decode(&minimal_png()).is_ok());
    assert!(decode(&indexed_png()).is_ok());
}

#[test]
fn every_truncated_prefix_errors() {
    let png = minimal_png();
    for cut in 0..png.len() {
        assert!(decode(&png[..cut]).is_err(), "prefix of {cut} bytes");
    }
}

#[test]
fn signature_damage_is_rejected() {
    assert_invalid(&[], "empty input");
    let png = minimal_png();
    for i in 0..8 {
        let mut bad = png.clone();
        bad[i] ^= 0x40;
        assert_invalid(&bad, &format!("signature byte {i} flipped"));
    }
    assert_invalid(&SIGNATURE, "signature only (missing IHDR)");
    // First chunk must be IHDR, not an ancillary chunk.
    let gama_first = png_from_chunks(&[
        chunk(b"gAMA", &45455u32.to_be_bytes()),
        chunk(b"IHDR", &ihdr_payload(3, 2, 8, 2, 0)),
        chunk(b"IEND", &[]),
    ]);
    assert_invalid(&gama_first, "gAMA before IHDR");
}

#[test]
fn invalid_ihdr_fields_are_rejected() {
    let stream_for = |bytes: &[u8]| chunk(b"IDAT", &zlib(bytes));
    let build = |payload: &[u8]| {
        png_from_chunks(&[
            chunk(b"IHDR", payload),
            stream_for(&[0, 0, 0, 0]),
            chunk(b"IEND", &[]),
        ])
    };
    assert_invalid(&build(&[0u8; 12]), "IHDR of 12 bytes");
    assert_invalid(&build(&[0u8; 14]), "IHDR of 14 bytes");
    assert_invalid(&build(&ihdr_payload(0, 1, 8, 2, 0)), "zero width");
    assert_invalid(&build(&ihdr_payload(1, 0, 8, 2, 0)), "zero height");
    assert_invalid(&build(&ihdr_payload(1 << 31, 1, 8, 2, 0)), "width bit 31");
    assert_invalid(&build(&ihdr_payload(1, 1 << 31, 8, 2, 0)), "height bit 31");
    for code in [1u8, 5, 7, 255] {
        assert_invalid(
            &build(&ihdr_payload(1, 1, 8, code, 0)),
            "undefined colour type",
        );
    }
    for (depth, color) in [(3u8, 0u8), (1, 2), (2, 2), (4, 2), (16, 3), (4, 4), (32, 0)] {
        assert_invalid(
            &build(&ihdr_payload(1, 1, depth, color, 0)),
            &format!("depth {depth} forbidden for colour type {color}"),
        );
    }
    let mut bad_compression = ihdr_payload(1, 1, 8, 2, 0);
    bad_compression[10] = 1;
    assert_invalid(&build(&bad_compression), "compression method 1");
    let mut bad_filter = ihdr_payload(1, 1, 8, 2, 0);
    bad_filter[11] = 1;
    assert_invalid(&build(&bad_filter), "filter method 1");
    assert_invalid(&build(&ihdr_payload(1, 1, 8, 2, 2)), "interlace method 2");
}

/// Flips the last CRC byte of the chunk whose type appears at `type_offset` hits within `png`.
fn corrupt_crc_of(png: &[u8], chunk_type: &[u8; 4]) -> Vec<u8> {
    let mut out = png.to_vec();
    let mut i = 8;
    while i + 12 <= out.len() {
        let len = u32::from_be_bytes([out[i], out[i + 1], out[i + 2], out[i + 3]]) as usize;
        if &out[i + 4..i + 8] == chunk_type {
            out[i + 11 + len] ^= 0xFF; // last CRC byte
            return out;
        }
        i += 12 + len;
    }
    panic!("chunk {chunk_type:?} not found");
}

#[test]
fn critical_crc_damage_errors_ancillary_is_skipped() {
    assert_invalid(&corrupt_crc_of(&minimal_png(), b"IHDR"), "IHDR CRC");
    assert_invalid(&corrupt_crc_of(&minimal_png(), b"IDAT"), "IDAT CRC");
    assert_invalid(&corrupt_crc_of(&indexed_png(), b"PLTE"), "PLTE CRC");

    // Ancillary CRC damage skips the chunk: a broken gAMA leaves the image decodable with no
    // gamma; a broken tRNS decodes the image fully opaque (policy pinned here).
    let with_gama = png_from_chunks(&[
        chunk(b"IHDR", &ihdr_payload(3, 2, 8, 2, 0)),
        chunk(b"gAMA", &45455u32.to_be_bytes()),
        chunk(
            b"IDAT",
            &zlib(&[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9]),
        ),
        chunk(b"IEND", &[]),
    ]);
    let decoded = decode(&corrupt_crc_of(&with_gama, b"gAMA")).expect("gAMA damage is skipped");
    assert!(
        decoded.metadata.gamma.is_none(),
        "damaged gAMA must not surface"
    );

    let with_trns = png_from_chunks(&[
        chunk(b"IHDR", &ihdr_payload(2, 1, 8, 0, 0)),
        chunk(b"tRNS", &7u16.to_be_bytes()),
        chunk(b"IDAT", &zlib(&[0, 7, 8])),
        chunk(b"IEND", &[]),
    ]);
    let decoded = decode(&corrupt_crc_of(&with_trns, b"tRNS")).expect("tRNS damage is skipped");
    assert!(decoded.transparency.is_none(), "damaged tRNS must not key");
}

#[test]
fn chunk_framing_violations_are_rejected() {
    // Declared length larger than the remaining input.
    let mut overrun = SIGNATURE.to_vec();
    overrun.extend_from_slice(&100u32.to_be_bytes());
    overrun.extend_from_slice(b"IHDR");
    overrun.extend_from_slice(&[0; 20]);
    assert_invalid(&overrun, "length past end of input");
    // Length with bit 31 set.
    let mut huge = SIGNATURE.to_vec();
    huge.extend_from_slice(&(1u32 << 31).to_be_bytes());
    huge.extend_from_slice(b"IHDR");
    assert_invalid(&huge, "length bit 31");
    // Duplicate IHDR.
    let dup = png_from_chunks(&[
        chunk(b"IHDR", &ihdr_payload(3, 2, 8, 2, 0)),
        chunk(b"IHDR", &ihdr_payload(3, 2, 8, 2, 0)),
        chunk(b"IDAT", &zlib(&[0; 20])),
        chunk(b"IEND", &[]),
    ]);
    assert_invalid(&dup, "duplicate IHDR");
    // Missing IDAT / missing IEND / non-empty IEND.
    let no_idat = png_from_chunks(&[
        chunk(b"IHDR", &ihdr_payload(3, 2, 8, 2, 0)),
        chunk(b"IEND", &[]),
    ]);
    assert_invalid(&no_idat, "missing IDAT");
    let no_iend = png_from_chunks(&[
        chunk(b"IHDR", &ihdr_payload(3, 2, 8, 2, 0)),
        chunk(
            b"IDAT",
            &zlib(&[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9]),
        ),
    ]);
    assert_invalid(&no_iend, "missing IEND");
    let mut full_iend = png_from_chunks(&[
        chunk(b"IHDR", &ihdr_payload(3, 2, 8, 2, 0)),
        chunk(
            b"IDAT",
            &zlib(&[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9]),
        ),
        chunk(b"IEND", &[42]),
    ]);
    assert_invalid(&full_iend, "IEND with payload");
    // Trailing bytes after IEND are ignored (policy pinned): still decodes.
    full_iend = minimal_png();
    full_iend.extend_from_slice(b"trailing garbage, not chunks");
    assert!(decode(&full_iend).is_ok(), "trailing bytes after IEND");
}

#[test]
fn ordering_violations_are_rejected() {
    let rgb_idat = chunk(
        b"IDAT",
        &zlib(&[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9]),
    );
    // PLTE after IDAT.
    let plte_late = png_from_chunks(&[
        chunk(b"IHDR", &ihdr_payload(3, 2, 8, 2, 0)),
        rgb_idat.clone(),
        chunk(b"PLTE", &[1, 2, 3]),
        chunk(b"IEND", &[]),
    ]);
    assert_invalid(&plte_late, "PLTE after IDAT");
    // tRNS after IDAT.
    let trns_late = png_from_chunks(&[
        chunk(b"IHDR", &ihdr_payload(3, 2, 8, 2, 0)),
        rgb_idat.clone(),
        chunk(b"tRNS", &[0, 1, 0, 2, 0, 3]),
        chunk(b"IEND", &[]),
    ]);
    assert_invalid(&trns_late, "tRNS after IDAT");
    // Non-consecutive IDAT runs.
    let split = zlib(&[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9]);
    let (first, second) = split.split_at(split.len() / 2);
    let gap = png_from_chunks(&[
        chunk(b"IHDR", &ihdr_payload(3, 2, 8, 2, 0)),
        chunk(b"IDAT", first),
        chunk(b"tEXt", b"k\0v"),
        chunk(b"IDAT", second),
        chunk(b"IEND", &[]),
    ]);
    assert_invalid(&gap, "interrupted IDAT run");
    // PLTE after tRNS (tRNS must follow the palette it modifies).
    let plte_after_trns = png_from_chunks(&[
        chunk(b"IHDR", &ihdr_payload(4, 2, 1, 3, 0)),
        chunk(b"tRNS", &[128]),
        chunk(b"PLTE", &[10, 20, 30, 200, 210, 220]),
        chunk(b"IDAT", &zlib(&[0, 0xA0, 0, 0x60])),
        chunk(b"IEND", &[]),
    ]);
    assert_invalid(&plte_after_trns, "PLTE after tRNS");
    // Misordered *informational* ancillary chunks are tolerated (policy pinned): gAMA after
    // the IDAT run still surfaces.
    let gama_late = png_from_chunks(&[
        chunk(b"IHDR", &ihdr_payload(3, 2, 8, 2, 0)),
        rgb_idat,
        chunk(b"gAMA", &45455u32.to_be_bytes()),
        chunk(b"IEND", &[]),
    ]);
    let decoded = decode(&gama_late).expect("late gAMA tolerated");
    assert_eq!(decoded.metadata.gamma, Some(45455));
}

#[test]
fn palette_violations_are_rejected() {
    let idat = chunk(b"IDAT", &zlib(&[0, 0xA0, 0, 0x60]));
    let build = |plte: Vec<u8>, trns: Option<Vec<u8>>, ihdr: [u8; 13]| {
        let mut chunks = vec![chunk(b"IHDR", &ihdr)];
        if !plte.is_empty() {
            chunks.push(chunk(b"PLTE", &plte));
        }
        if let Some(trns) = trns {
            chunks.push(chunk(b"tRNS", &trns));
        }
        chunks.push(idat.clone());
        chunks.push(chunk(b"IEND", &[]));
        png_from_chunks(&chunks)
    };
    let indexed = ihdr_payload(4, 2, 1, 3, 0);
    assert_invalid(&build(vec![], None, indexed), "indexed without PLTE");
    assert_invalid(&build(vec![1, 2, 3, 4], None, indexed), "PLTE not triples");
    assert_invalid(
        &build(vec![0; 771], None, ihdr_payload(4, 2, 8, 3, 0)),
        "257 entries",
    );
    assert_invalid(
        &build(vec![0; 9], None, indexed),
        "3 entries exceed depth 1",
    );
    assert_invalid(
        &build(vec![1, 2, 3, 4, 5, 6], Some(vec![0, 0, 0]), indexed),
        "tRNS longer than PLTE",
    );
    // Index 1 out of range for a one-entry palette (pixels use indices 0 and 1).
    assert_invalid(&build(vec![1, 2, 3], None, indexed), "index out of range");
    // PLTE is forbidden for greyscale colour types.
    let gray = ihdr_payload(3, 2, 8, 0, 0);
    let gray_idat = zlib(&[0, 1, 2, 3, 0, 4, 5, 6]);
    let plte_on_gray = png_from_chunks(&[
        chunk(b"IHDR", &gray),
        chunk(b"PLTE", &[1, 2, 3]),
        chunk(b"IDAT", &gray_idat),
        chunk(b"IEND", &[]),
    ]);
    assert_invalid(&plte_on_gray, "PLTE on greyscale");
    // tRNS is forbidden for colour types with alpha.
    let ga = png_from_chunks(&[
        chunk(b"IHDR", &ihdr_payload(2, 1, 8, 4, 0)),
        chunk(b"tRNS", &7u16.to_be_bytes()),
        chunk(b"IDAT", &zlib(&[0, 1, 2, 3, 4])),
        chunk(b"IEND", &[]),
    ]);
    assert_invalid(&ga, "tRNS on grey+alpha");
    // Malformed colour-key sizes.
    let bad_key = png_from_chunks(&[
        chunk(b"IHDR", &gray),
        chunk(b"tRNS", &[1]),
        chunk(b"IDAT", &gray_idat),
        chunk(b"IEND", &[]),
    ]);
    assert_invalid(&bad_key, "1-byte grey key");
}

#[test]
fn idat_stream_violations_are_rejected() {
    let ihdr = ihdr_payload(3, 2, 8, 2, 0);
    let build = |idat_payload: &[u8]| {
        png_from_chunks(&[
            chunk(b"IHDR", &ihdr),
            chunk(b"IDAT", idat_payload),
            chunk(b"IEND", &[]),
        ])
    };
    assert_invalid(&build(&[]), "empty IDAT payload");
    assert_invalid(&build(b"garbage"), "not a zlib stream");
    // Wrong Adler-32 trailer.
    let mut bad_adler = zlib(&[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9]);
    let last = bad_adler.len() - 1;
    bad_adler[last] ^= 0xFF;
    assert_invalid(&build(&bad_adler), "corrupt Adler-32");
    // One byte short of the image; one byte over it.
    assert_invalid(&build(&zlib(&[0u8; 19])), "stream one byte short");
    assert_invalid(&build(&zlib(&[0u8; 21])), "stream one byte over");
    // Undefined filter-type byte.
    let mut bad_filter = vec![5u8];
    bad_filter.extend_from_slice(&[0; 9]);
    bad_filter.push(0);
    bad_filter.extend_from_slice(&[0; 9]);
    assert_invalid(&build(&zlib(&bad_filter)), "filter type 5");
}

#[test]
fn bombs_and_limits_are_refused_before_allocation() {
    // A giant declared image with a tiny IDAT: the default 64 MiB budget refuses it up front.
    let bomb = png_from_chunks(&[
        chunk(b"IHDR", &ihdr_payload(100_000, 100_000, 8, 2, 0)),
        chunk(b"IDAT", &zlib(&[0; 16])),
        chunk(b"IEND", &[]),
    ]);
    assert_unsupported(&bomb, "dimension bomb hits the byte budget");
    // Arithmetic overflow of the sample count (2³¹−1 squared × RGBA16) must be InvalidInput,
    // not a wrap-around into a small allocation.
    let overflow = png_from_chunks(&[
        chunk(
            b"IHDR",
            &ihdr_payload((1 << 31) - 1, (1 << 31) - 1, 16, 6, 0),
        ),
        chunk(b"IDAT", &zlib(&[0; 16])),
        chunk(b"IEND", &[]),
    ]);
    assert!(decode(&overflow).is_err(), "usize overflow guard");
    // A zlib stream that inflates past the image's exact stream length is cut off.
    let ihdr = ihdr_payload(2, 1, 8, 0, 0); // needs 3 stream bytes
    let bomb_idat = zlib(&vec![0u8; 1 << 20]); // ~1 MiB of zeros from a few hundred bytes
    let zlib_bomb = png_from_chunks(&[
        chunk(b"IHDR", &ihdr),
        chunk(b"IDAT", &bomb_idat),
        chunk(b"IEND", &[]),
    ]);
    assert_invalid(&zlib_bomb, "zlib bomb capped at the expected stream length");
    // The metadata budget: a zTXt bomb is skipped (not an error), the image still decodes.
    let mut ztxt_payload = b"key\0\0".to_vec();
    ztxt_payload.extend_from_slice(&zlib(&vec![b'x'; 1 << 22]));
    let ztxt_bomb = png_from_chunks(&[
        chunk(b"IHDR", &ihdr_payload(3, 2, 8, 2, 0)),
        chunk(b"zTXt", &ztxt_payload),
        chunk(
            b"IDAT",
            &zlib(&[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9]),
        ),
        chunk(b"IEND", &[]),
    ]);
    let decoded = PngDecoder::new()
        .with_max_metadata_bytes(1024)
        .decode(&ztxt_bomb)
        .expect("metadata bomb skipped, image intact");
    assert!(
        decoded.metadata.texts.is_empty(),
        "busting zTXt is not surfaced"
    );
}

#[test]
fn unknown_chunks_follow_criticality() {
    // Unknown critical chunk (uppercase first letter): refuse to decode.
    let critical = png_from_chunks(&[
        chunk(b"IHDR", &ihdr_payload(3, 2, 8, 2, 0)),
        chunk(b"KRIT", &[1, 2, 3]),
        chunk(
            b"IDAT",
            &zlib(&[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9]),
        ),
        chunk(b"IEND", &[]),
    ]);
    assert_unsupported(&critical, "unknown critical chunk");
    // Unknown ancillary chunk (lowercase first letter): skipped, image decodes.
    let ancillary = png_from_chunks(&[
        chunk(b"IHDR", &ihdr_payload(3, 2, 8, 2, 0)),
        chunk(b"prVt", &[1, 2, 3]),
        chunk(
            b"IDAT",
            &zlib(&[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9]),
        ),
        chunk(b"IEND", &[]),
    ]);
    assert!(
        decode(&ancillary).is_ok(),
        "unknown ancillary chunk skipped"
    );
    // APNG control chunks are unknown ancillary chunks here: the default image decodes.
    let apng = png_from_chunks(&[
        chunk(b"IHDR", &ihdr_payload(3, 2, 8, 2, 0)),
        chunk(b"acTL", &[0, 0, 0, 2, 0, 0, 0, 0]),
        chunk(
            b"IDAT",
            &zlib(&[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9]),
        ),
        chunk(b"IEND", &[]),
    ]);
    assert!(decode(&apng).is_ok(), "APNG default image decodes");
}

#[test]
fn typed_decodes_reject_malformed_input_too() {
    // The typed path shares the pipeline: spot-check that it errors identically.
    let png = corrupt_crc_of(&minimal_png(), b"IDAT");
    let result: Result<ImageBuf<Rgb8>, Error> = PngDecoder::new().decode_image(&png);
    assert!(matches!(result, Err(error) if error.kind() == ErrorKind::InvalidInput));
    let result: Result<ImageBuf<Gray8>, Error> = PngDecoder::new().decode_image(&minimal_png());
    assert!(
        matches!(result, Err(error) if error.kind() == ErrorKind::Unsupported),
        "RGB as Gray8 is a lossy request, refused as Unsupported"
    );
}
