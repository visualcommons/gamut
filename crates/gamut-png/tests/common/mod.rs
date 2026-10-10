//! Shared helpers for the decoder test suites: deterministic pixel generators, a chunk-level
//! PNG builder for hand-crafted (and deliberately malformed) streams, and a self-contained
//! CRC-32 so the builders do not depend on the crate under test.
#![allow(dead_code)] // each integration-test binary uses its own subset

/// The efficiency corpus, shared with `benches/encode.rs` (issue #224).
pub mod corpus;

/// The 8-byte PNG signature.
pub const SIGNATURE: [u8; 8] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];

/// CRC-32 (ISO 3309, polynomial 0xEDB88320) over `bytes` — the PNG chunk CRC.
pub fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

/// Frames one chunk: `length ‖ type ‖ payload ‖ CRC`.
pub fn chunk(chunk_type: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(12 + payload.len());
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(chunk_type);
    out.extend_from_slice(payload);
    let mut covered = chunk_type.to_vec();
    covered.extend_from_slice(payload);
    out.extend_from_slice(&crc32(&covered).to_be_bytes());
    out
}

/// A 13-byte IHDR payload.
pub fn ihdr_payload(
    width: u32,
    height: u32,
    bit_depth: u8,
    color_type: u8,
    interlace: u8,
) -> [u8; 13] {
    let mut data = [0u8; 13];
    data[0..4].copy_from_slice(&width.to_be_bytes());
    data[4..8].copy_from_slice(&height.to_be_bytes());
    data[8] = bit_depth;
    data[9] = color_type;
    data[12] = interlace;
    data
}

/// Concatenates the signature and the given chunks into a PNG byte stream.
pub fn png_from_chunks(chunks: &[Vec<u8>]) -> Vec<u8> {
    let mut png = SIGNATURE.to_vec();
    for chunk in chunks {
        png.extend_from_slice(chunk);
    }
    png
}

/// zlib-compresses `payload` with the reference-independent gamut deflater.
pub fn zlib(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    gamut_deflate::DeflateEncoder::new().zlib_compress(payload, &mut out);
    out
}

/// A minimal valid PNG: 3×2 RGB8, filter None rows, values 1..=18.
pub fn minimal_png() -> Vec<u8> {
    let stream: Vec<u8> = (0..2)
        .flat_map(|row| std::iter::once(0u8).chain((0..9).map(move |i| (row * 9 + i + 1) as u8)))
        .collect();
    png_from_chunks(&[
        chunk(b"IHDR", &ihdr_payload(3, 2, 8, 2, 0)),
        chunk(b"IDAT", &zlib(&stream)),
        chunk(b"IEND", &[]),
    ])
}

/// Deterministic pseudo-random bytes with enough structure to vary between rows and channels.
pub fn noise(len: usize, seed: u32) -> Vec<u8> {
    (0..len)
        .map(|i| {
            let x = (i as u32)
                .wrapping_mul(2_654_435_761)
                .wrapping_add(seed.wrapping_mul(97))
                ^ (i as u32 >> 3);
            (x ^ (x >> 13)) as u8
        })
        .collect()
}

/// Channels per pixel for a PNG colour-type code.
pub fn channels(color_type: u8) -> usize {
    match color_type {
        0 | 3 => 1,
        4 => 2,
        2 => 3,
        6 => 4,
        other => panic!("invalid colour type {other}"),
    }
}

/// Deterministic raw samples in the layout `libpng_oracle::encode` expects: one byte per sample
/// (masked to the bit depth) below 16, big-endian byte pairs at 16.
pub fn sample_bytes(width: u32, height: u32, color_type: u8, bit_depth: u8, seed: u32) -> Vec<u8> {
    let samples = (width * height) as usize * channels(color_type);
    if bit_depth == 16 {
        return noise(samples * 2, seed);
    }
    let mask = (1u16 << bit_depth) - 1;
    noise(samples, seed)
        .into_iter()
        .map(|value| value & mask as u8)
        .collect()
}

/// The §13.12 scale factor from a sub-byte grey sample to its 8-bit presentation.
pub fn gray8_scale(bit_depth: u8) -> u8 {
    match bit_depth {
        1 => 255,
        2 => 85,
        4 => 17,
        _ => 1,
    }
}

/// The minimal-but-valid 132-byte ICC profile header libpng's *write-side* iCCP validation
/// accepts (stricter than the read side: it also demands the D50 PCS illuminant).
pub fn tiny_icc_profile() -> Vec<u8> {
    let mut icc = vec![0u8; 132];
    icc[0..4].copy_from_slice(&132u32.to_be_bytes()); // profile size
    icc[8..12].copy_from_slice(&0x0210_0000u32.to_be_bytes()); // version 2.1
    icc[12..16].copy_from_slice(b"mntr");
    icc[16..20].copy_from_slice(b"RGB ");
    icc[20..24].copy_from_slice(b"XYZ ");
    icc[36..40].copy_from_slice(b"acsp"); // ICC signature
    icc[68..72].copy_from_slice(&0x0000_F6D6u32.to_be_bytes()); // D50 illuminant X
    icc[72..76].copy_from_slice(&0x0001_0000u32.to_be_bytes()); // D50 illuminant Y
    icc[76..80].copy_from_slice(&0x0000_D32Du32.to_be_bytes()); // D50 illuminant Z
    icc
}

/// A minimal-but-plausible EXIF payload: TIFF header plus an empty IFD.
pub fn tiny_exif() -> Vec<u8> {
    vec![
        0x49, 0x49, 0x2A, 0x00, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    ]
}

/// Every valid Table-12 colour-type/bit-depth pair, flattened (libpng's `COLOR_*` codes).
pub const TABLE_12: &[(u8, u8)] = &[
    (libpng_oracle::COLOR_GRAY, 1),
    (libpng_oracle::COLOR_GRAY, 2),
    (libpng_oracle::COLOR_GRAY, 4),
    (libpng_oracle::COLOR_GRAY, 8),
    (libpng_oracle::COLOR_GRAY, 16),
    (libpng_oracle::COLOR_PALETTE, 1),
    (libpng_oracle::COLOR_PALETTE, 2),
    (libpng_oracle::COLOR_PALETTE, 4),
    (libpng_oracle::COLOR_PALETTE, 8),
    (libpng_oracle::COLOR_RGB, 8),
    (libpng_oracle::COLOR_RGB, 16),
    (libpng_oracle::COLOR_GRAY_ALPHA, 8),
    (libpng_oracle::COLOR_GRAY_ALPHA, 16),
    (libpng_oracle::COLOR_RGBA, 8),
    (libpng_oracle::COLOR_RGBA, 16),
];

/// A full-size palette for an indexed fixture at `depth`.
fn full_palette(depth: u8) -> Vec<[u8; 3]> {
    (0..(1usize << depth))
        .map(|i| [i as u8, (i * 7 + 3) as u8, 255 - i as u8])
        .collect()
}

/// Encodes a deterministic fixture with libpng (full-size palette for indexed depths).
pub fn libpng_fixture(
    width: u32,
    height: u32,
    color_type: u8,
    depth: u8,
    interlace: bool,
) -> Vec<u8> {
    let pixels = sample_bytes(width, height, color_type, depth, 11);
    let palette = full_palette(depth);
    let opts = libpng_oracle::EncodeOpts {
        interlace,
        palette: (color_type == libpng_oracle::COLOR_PALETTE).then_some(&palette),
        ..libpng_oracle::EncodeOpts::default()
    };
    libpng_oracle::encode(&pixels, width, height, color_type, depth, &opts)
}

/// An 8-bit RGB fixture libpng wrote with exactly one filter on every scanline. `mask` is one of
/// libpng's `FILTER_*` bits, so the *oracle* chooses the filter, not gamut.
pub fn libpng_forced_filter(width: u32, height: u32, mask: u8) -> Vec<u8> {
    let pixels = sample_bytes(width, height, libpng_oracle::COLOR_RGB, 8, 11);
    let opts = libpng_oracle::EncodeOpts {
        filters: Some(mask),
        ..libpng_oracle::EncodeOpts::default()
    };
    libpng_oracle::encode(&pixels, width, height, libpng_oracle::COLOR_RGB, 8, &opts)
}

/// An 8-bit RGB fixture carrying extra raw chunks written verbatim after IHDR — used for chunk
/// types this crate does not recognise, ancillary and critical alike.
pub fn libpng_with_extra_chunks(width: u32, height: u32, extra: &[([u8; 4], &[u8])]) -> Vec<u8> {
    let pixels = sample_bytes(width, height, libpng_oracle::COLOR_RGB, 8, 11);
    let opts = libpng_oracle::EncodeOpts {
        extra_chunks: extra,
        ..libpng_oracle::EncodeOpts::default()
    };
    libpng_oracle::encode(&pixels, width, height, libpng_oracle::COLOR_RGB, 8, &opts)
}

/// A structurally perfect PNG whose IDAT payload is not a zlib stream. Every CRC is valid, so
/// only the *compressed data* is damaged — the one input that isolates a decompression failure
/// from a framing failure.
pub fn png_with_garbage_idat(width: u32, height: u32) -> Vec<u8> {
    png_from_chunks(&[
        chunk(b"IHDR", &ihdr_payload(width, height, 8, 2, 0)),
        chunk(b"IDAT", b"this is not a zlib stream"),
        chunk(b"IEND", &[]),
    ])
}

/// A PNG whose IHDR claims 2^30 x 2^30 with a tiny IDAT: the filtered stream it implies is far
/// past any sane inflation budget, so a reader must decline rather than attempt it.
pub fn png_with_huge_ihdr() -> Vec<u8> {
    png_from_chunks(&[
        chunk(b"IHDR", &ihdr_payload(1 << 30, 1 << 30, 8, 2, 0)),
        chunk(b"IDAT", &zlib(&[0u8; 16])),
        chunk(b"IEND", &[]),
    ])
}
