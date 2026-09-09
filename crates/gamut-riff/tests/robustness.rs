//! The container parsers under hostile input: exhaustive truncation and bit-flip sweeps.
//!
//! `RiffReader` and `WebpLayout` chew on attacker-controlled `uint32` sizes straight off the wire,
//! which is exactly the surface behind libwebp's CVE record. The crate's answer is
//! `#![forbid(unsafe_code)]` plus typed errors, and the contract these tests pin is: **never panic,
//! never hang, never allocate on a count the input chose** — any input is either parsed or refused
//! with an `Err`.
//!
//! This follows the workspace's established robustness idiom (see `gamut-webp/tests/robustness.rs`
//! and `gamut-ifd/tests/robustness.rs`): deterministic exhaustive sweeps over a valid seed, no
//! fuzzing dependency.

use gamut_riff::{
    FourCc, MetadataChunks, RiffReader, Vp8xHeader, WebpLayout, write_extended_preserving,
};

/// A valid extended file exercising every chunk role the parsers distinguish: the `VP8X` header,
/// a colour profile, an alpha chunk, the bitstream, both metadata chunks, and an unknown chunk —
/// with an odd payload in the mix so the pad-byte path is covered too.
fn seed() -> Vec<u8> {
    let header = Vp8xHeader {
        alpha: true,
        canvas_width: 8,
        canvas_height: 4,
        ..Default::default()
    };
    write_extended_preserving(
        &header,
        &MetadataChunks {
            icc: Some(b"icc"), // odd -> padded
            exif: Some(b"exif"),
            xmp: Some(b"<x/>"),
            c2pa: None,
        },
        &[
            (FourCc::ALPH, &[0x00, 0x11]),
            (FourCc::VP8L, &[0x2f, 0x00, 0x00]), // odd -> padded
        ],
        &[],
    )
    .expect("the seed is well-formed")
}

/// Runs both parsers over `data`. Neither may panic; either verdict is acceptable.
fn parse_both(data: &[u8]) {
    if let Ok(reader) = RiffReader::new(data) {
        // Drain the iterator: a chunk that errors must end iteration, not loop forever.
        let mut seen = 0;
        for chunk in reader {
            seen += 1;
            assert!(seen <= 64, "iteration must terminate, not spin");
            if chunk.is_err() {
                break;
            }
        }
    }
    let _ = WebpLayout::parse(data);
}

#[test]
fn the_seed_parses() {
    let file = seed();
    let layout = WebpLayout::parse(&file).expect("baseline");
    assert!(layout.vp8x.is_some());
    assert!(layout.bitstream.is_some());
    assert_eq!(layout.metadata.icc, Some(&b"icc"[..]));
}

#[test]
fn survives_truncation_at_every_length() {
    let file = seed();
    for len in 0..=file.len() {
        parse_both(&file[..len]);
    }
}

#[test]
fn survives_every_single_bit_flip() {
    let file = seed();
    for i in 0..file.len() {
        for bit in 0..8u8 {
            let mut bad = file.clone();
            bad[i] ^= 1 << bit;
            parse_both(&bad);
        }
    }
}

#[test]
fn survives_a_hostile_chunk_size_at_every_chunk_offset() {
    // The chunk size field is the parser's single most dangerous input: it drives a slice range. Try
    // the extremes at every 4-byte-aligned position, so each real size field is hit.
    let file = seed();
    for offset in (12..file.len().saturating_sub(4)).step_by(4) {
        for hostile in [
            u32::MAX,
            u32::MAX - 1,
            0x8000_0000,
            0x7fff_ffff,
            file.len() as u32,
            0,
            1,
        ] {
            let mut bad = file.clone();
            bad[offset..offset + 4].copy_from_slice(&hostile.to_le_bytes());
            parse_both(&bad);
        }
    }
}

#[test]
fn survives_a_hostile_file_size_field() {
    let file = seed();
    for hostile in [
        u32::MAX,
        0x8000_0000,
        0,
        1,
        3,
        4,
        5,
        file.len() as u32,
        file.len() as u32 + 1,
    ] {
        let mut bad = file.clone();
        bad[4..8].copy_from_slice(&hostile.to_le_bytes());
        parse_both(&bad);
    }
}

#[test]
fn rejects_short_and_non_riff_inputs_without_panicking() {
    let inputs: &[&[u8]] = &[
        b"",
        b"R",
        b"RIFF",
        b"RIFF\x00\x00\x00\x00",
        b"RIFF\x04\x00\x00\x00WEBP",
        b"RIFF\xff\xff\xff\xffWEBP",
        b"WEBPRIFF\x00\x00\x00\x00",
        b"definitely not a WebP file at all",
        &[0xff; 64],
        &[0x00; 64],
    ];
    for input in inputs {
        parse_both(input);
    }
    // The empty-chunk-region case is legal and must parse to nothing, not error.
    let empty = b"RIFF\x04\x00\x00\x00WEBP";
    assert_eq!(RiffReader::new(empty).expect("valid header").count(), 0);
    assert_eq!(
        WebpLayout::parse(empty).expect("valid header").bitstream,
        None
    );
}

#[test]
fn a_chunk_count_never_drives_an_allocation() {
    // `WebpLayout` collects unknown chunks into a `Vec`. A file claiming thousands of them must
    // only ever allocate for chunks that are really present in the input — the bytes bound the
    // work, so a small file cannot ask for a large allocation.
    let mut file = Vec::from(*b"RIFF\x00\x00\x00\x00WEBP");
    for _ in 0..64 {
        file.extend_from_slice(b"XYZW\x00\x00\x00\x00"); // 64 empty unknown chunks
    }
    let size = u32::try_from(file.len() - 8).unwrap();
    file[4..8].copy_from_slice(&size.to_le_bytes());

    let layout = WebpLayout::parse(&file).expect("empty unknown chunks are well-formed");
    assert_eq!(layout.unknown.len(), 64);
    assert!(layout.unknown.iter().all(|c| c.payload.is_empty()));
}
