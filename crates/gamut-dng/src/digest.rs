//! The DNG raw-image digest (`NewRawImageDigest`, tag 51111): the Adobe SDK's
//! MD5-over-raw-image algorithm (`dng_negative::FindNewRawImageDigest`), reproduced bit-exactly
//! and differentially gated against the SDK.
//!
//! The algorithm (SDK `dng_find_new_raw_image_digest_task`):
//!
//! 1. The image is covered by a grid of **digest tiles** whose unit cell is
//!    `min(256, height) × min(256, width)`; edge tiles are clipped to the image, not padded.
//! 2. Each tile's samples are serialised **planar** (all of plane 0's tile pixels row-major,
//!    then plane 1, …) as little-endian `u16` — or as single bytes when the image is 8-bit or
//!    shallower (the SDK's raw image is byte-typed then) or a `LinearizationTable` with ≤ 256
//!    entries is present (the SDK stores such data 8-bit) — and MD5-hashed.
//! 3. The final digest is the MD5 of the tile digests concatenated in row-major tile order.
//!
//! **Lossy-compressed storage** (JPEG XL 52546, lossy JPEG 34892) digests differently: the SDK
//! routes such images through `dng_lossy_compressed_image::FindDigest`, which hashes each
//! **compressed chunk** (strip/tile bytes, in offset order) and then MD5s the concatenated
//! per-chunk digests — see [`lossy_compressed_digest`]. `ValidateRawImageDigest` compares a
//! lossy-compressed file's stored tag against that value, so the encoder writes it for JPEG XL.
//!
//! The SDK folds a raw transparency mask into the digest when one exists; gamut-dng never
//! writes one, so that branch is not modelled.

use md5::{Digest, Md5};

use crate::raw::RawImage;

/// The MD5 (RFC 1321) digest of `data`. MD5 is used only because the DNG specification defines
/// its digests over it: an integrity checksum, not a security boundary.
fn md5(data: &[u8]) -> [u8; 16] {
    Md5::digest(data).into()
}

/// Computes the `NewRawImageDigest` of `raw` (see the module docs).
#[must_use]
pub(crate) fn new_raw_image_digest(raw: &RawImage) -> [u8; 16] {
    let width = raw.dimensions().width() as usize;
    let height = raw.dimensions().height() as usize;
    let spp = usize::from(raw.samples_per_pixel());
    let samples = raw.samples();

    // The SDK digests as bytes whenever its raw image is byte-typed: an image of 8 bits or
    // fewer, or 16-bit data whose <= 256-entry linearization table proves it fits in 8 bits.
    let byte_mode = raw.bits_per_sample() <= 8
        || raw
            .levels()
            .linearization_table()
            .is_some_and(|table| table.len() <= 256);

    let cell_w = width.min(256);
    let cell_h = height.min(256);
    let across = width.div_ceil(cell_w);
    let down = height.div_ceil(cell_h);

    // No capacity hints: `tile_bytes` reaches steady-state capacity after the first tile (it is
    // reused via `clear`), and hint arithmetic would only breed unkillable capacity mutants.
    let mut tile_digests = Vec::new();
    let mut tile_bytes = Vec::new();
    for tile_row in 0..down {
        for tile_col in 0..across {
            let x0 = tile_col * cell_w;
            let y0 = tile_row * cell_h;
            let tile_w = cell_w.min(width - x0);
            let tile_h = cell_h.min(height - y0);
            tile_bytes.clear();
            for plane in 0..spp {
                for row in 0..tile_h {
                    for col in 0..tile_w {
                        let sample = samples[((y0 + row) * width + x0 + col) * spp + plane];
                        if byte_mode {
                            tile_bytes.push(sample as u8);
                        } else {
                            tile_bytes.extend_from_slice(&sample.to_le_bytes());
                        }
                    }
                }
            }
            tile_digests.extend_from_slice(&md5(&tile_bytes));
        }
    }
    md5(&tile_digests)
}

/// Computes the `NewRawImageDigest` of a **lossy-compressed** image from its stored chunks
/// (the SDK's `dng_lossy_compressed_image::FindDigest`): MD5 per compressed chunk, then MD5
/// over the concatenated chunk digests in offset order.
#[must_use]
pub(crate) fn lossy_compressed_digest(chunks: &[Vec<u8>]) -> [u8; 16] {
    let mut chunk_digests = Vec::new();
    for chunk in chunks {
        chunk_digests.extend_from_slice(&md5(chunk));
    }
    md5(&chunk_digests)
}

#[cfg(test)]
mod tests {
    use gamut_core::Dimensions;

    use super::*;

    /// The lossy-compressed digest is md5 over the concatenated per-chunk md5s, in chunk order.
    #[test]
    fn lossy_compressed_digest_hashes_chunk_digests() {
        let chunks = vec![vec![1u8, 2, 3], vec![4u8, 5]];
        let mut concat = Vec::new();
        concat.extend_from_slice(&md5(&chunks[0]));
        concat.extend_from_slice(&md5(&chunks[1]));
        assert_eq!(lossy_compressed_digest(&chunks), md5(&concat));
        // Chunk order matters.
        let swapped = vec![chunks[1].clone(), chunks[0].clone()];
        assert_ne!(
            lossy_compressed_digest(&swapped),
            lossy_compressed_digest(&chunks)
        );
    }

    fn raw(width: u32, height: u32, spp: u16) -> RawImage {
        let n = width as usize * height as usize * usize::from(spp);
        let samples: Vec<u16> = (0..n).map(|i| (i * 37 % 4096) as u16).collect();
        if spp == 1 {
            RawImage::new_cfa(
                Dimensions::new(width, height).unwrap(),
                12,
                (2, 2),
                vec![0, 1, 1, 2],
                samples,
            )
            .unwrap()
        } else {
            RawImage::new_linear_raw(Dimensions::new(width, height).unwrap(), 12, spp, samples)
                .unwrap()
        }
    }

    /// Hand-computed golden for a single-tile grayscale image: the digest is
    /// `md5(md5(le_bytes(samples)))` — pinned against an independent reference computation, so
    /// the planar order, endianness, and two-level hashing cannot silently change.
    #[test]
    fn single_tile_digest_is_md5_of_tile_md5() {
        let raw = raw(4, 3, 1);
        let mut bytes = Vec::new();
        for &s in raw.samples() {
            bytes.extend_from_slice(&s.to_le_bytes());
        }
        let expected = md5(&md5(&bytes));
        assert_eq!(new_raw_image_digest(&raw), expected);
    }

    /// Multi-plane serialisation is planar, not interleaved.
    #[test]
    fn multi_plane_digest_is_planar() {
        let raw = raw(3, 2, 3);
        let mut planar = Vec::new();
        for plane in 0..3usize {
            for pixel in 0..6usize {
                planar.extend_from_slice(&raw.samples()[pixel * 3 + plane].to_le_bytes());
            }
        }
        assert_eq!(new_raw_image_digest(&raw), md5(&md5(&planar)));
        // An interleaved serialisation would differ.
        let mut interleaved = Vec::new();
        for &s in raw.samples() {
            interleaved.extend_from_slice(&s.to_le_bytes());
        }
        assert_ne!(new_raw_image_digest(&raw), md5(&md5(&interleaved)));
    }

    /// Byte-mode selection: an image of 8 bits or fewer digests bytes, and so does a deeper
    /// image once a <= 256-entry linearization table proves its values fit 8 bits — while the
    /// same deep image without the table digests u16.
    #[test]
    fn byte_mode_follows_depth_and_table() {
        let samples: Vec<u16> = (0..12).map(|i| i * 20).collect();
        let eight_bit = RawImage::new_cfa(
            Dimensions::new(4, 3).unwrap(),
            8,
            (2, 2),
            vec![0, 1, 1, 2],
            samples.clone(),
        )
        .unwrap();
        let twelve_bit = RawImage::new_cfa(
            Dimensions::new(4, 3).unwrap(),
            12,
            (2, 2),
            vec![0, 1, 1, 2],
            samples,
        )
        .unwrap();
        let table: Vec<u16> = (0..256).map(|v| v * 257).collect();
        let twelve_with_table = twelve_bit
            .clone()
            .with_levels(
                crate::levels::RawLevels::uniform(1, 0.0, 4095.0)
                    .unwrap()
                    .with_linearization_table(table),
            )
            .unwrap();

        let byte_digest = {
            let bytes: Vec<u8> = eight_bit.samples().iter().map(|&s| s as u8).collect();
            md5(&md5(&bytes))
        };
        assert_eq!(new_raw_image_digest(&eight_bit), byte_digest);
        assert_eq!(
            new_raw_image_digest(&twelve_with_table),
            byte_digest,
            "the small table selects byte mode at any depth"
        );
        assert_ne!(
            new_raw_image_digest(&twelve_bit),
            byte_digest,
            "without the table, a 12-bit image digests u16"
        );
    }
}
