//! Reading a PNG's metadata without its pixels: the chunk walk behind [`metadata`] and
//! [`PngDecoder::metadata`], which skips `IDAT` by length, and [`collected`], the one place a
//! walk's chunks become a [`PngMetadata`] — shared with the full decode so the entry points
//! cannot disagree.
//!
//! Split from `decoder` (issue #1094): the walk changes with what counts as metadata and where a
//! C2PA store may sit, never with how image data is inflated, defiltered or presented.

use gamut_core::{Error, Result};

use crate::chunk::{CABX, ChunkReader};
use crate::decoded::{self, PngMetadata};
use crate::decoder::DEFAULT_MAX_METADATA_BYTES;
#[cfg(doc)]
use crate::decoder::PngDecoder;
use crate::ihdr::{self, Ihdr};

/// What one metadata walk found: the chunks in a position where their type may appear, how
/// many CRC-valid `caBX` chunks sat after `IDAT` — never the store (C2PA §A.3.2), so counted
/// rather than returned among the chunks — and the IHDR, which `sBIT` and `bKGD` are read against.
pub(crate) type MetadataWalk<'a> = (Vec<([u8; 4], &'a [u8])>, usize, Ihdr);

/// Walks the chunk stream collecting the CRC-valid ancillary chunks, for [`decoded::collect`] to
/// classify — the same handoff [`PngDecoder::parse_stream`] makes, so the two entry points cannot
/// disagree about which chunks carry metadata.
///
/// Deliberately not `parse_stream` itself: that accumulates every IDAT payload into an owned
/// `Vec` and then requires at least one, neither of which a metadata read should do. Here IDAT
/// (and PLTE) is skipped by length, so the pixel data is never touched or copied.
///
/// Returns a [`MetadataWalk`]: the chunks together with the number of CRC-valid `caBX` chunks
/// seen *after* `IDAT`,
/// which are never the store (C2PA §A.3.2) and so are counted rather than returned — the same split
/// [`PngDecoder::parse_stream`] makes, so the two entry points agree on what the store is —
/// and the parsed IHDR.
pub(crate) fn walk_metadata_chunks(data: &[u8]) -> Result<MetadataWalk<'_>> {
    let mut reader = ChunkReader::new(data)?;
    let first = reader
        .next_chunk()?
        .ok_or_else(|| Error::invalid_input(env!("CARGO_PKG_NAME"), "PNG: missing IHDR"))?;
    if first.chunk_type != *b"IHDR" {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "PNG: first chunk must be IHDR",
        ));
    }
    if !first.crc_ok {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "PNG: critical chunk CRC mismatch",
        ));
    }
    // Validated the same way `decode` does, so a stream with a corrupt IHDR is rejected here too
    // rather than yielding metadata from a broken file; kept, because `sBIT` and `bKGD` are
    // shaped by its colour type and depth.
    let header = ihdr::parse(first.data)?;

    let mut chunks = Vec::new();
    let mut c2pa_after_idat = 0usize;
    let mut seen_idat = false;
    let mut seen_iend = false;
    while let Some(chunk) = reader.next_chunk()? {
        match &chunk.chunk_type {
            b"IHDR" => {
                return Err(Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "PNG: duplicate IHDR",
                ));
            }
            b"IEND" => {
                if !chunk.crc_ok {
                    return Err(Error::invalid_input(
                        env!("CARGO_PKG_NAME"),
                        "PNG: critical chunk CRC mismatch",
                    ));
                }
                if !chunk.data.is_empty() {
                    return Err(Error::invalid_input(
                        env!("CARGO_PKG_NAME"),
                        "PNG: IEND payload must be empty",
                    ));
                }
                seen_iend = true;
                break;
            }
            // The pixel-bearing critical chunks. Skipped by length — never read, never copied.
            b"IDAT" | b"PLTE" => seen_idat |= &chunk.chunk_type == b"IDAT",
            _ if chunk.is_ancillary() => {
                // §13.1: a CRC mismatch skips the chunk rather than failing the image.
                if chunk.crc_ok {
                    // A store precedes IDAT (C2PA §A.3.2); one after it is counted, not offered.
                    if chunk.chunk_type == CABX && seen_idat {
                        c2pa_after_idat += 1;
                    } else {
                        chunks.push((chunk.chunk_type, chunk.data));
                    }
                }
            }
            _ => {
                // §5.4/§13.2: an unknown critical chunk means this is not a stream this decoder
                // understands. Rejected here as in `decode`, so `metadata` never reports on a
                // file the rest of the crate would refuse.
                return Err(Error::unsupported(
                    env!("CARGO_PKG_NAME"),
                    "PNG: unknown critical chunk",
                ));
            }
        }
    }
    if !seen_iend {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "PNG: missing IEND",
        ));
    }
    Ok((chunks, c2pa_after_idat, header))
}

/// Assembles the metadata from a walk's results: the chunks in a store-bearing position, the
/// count of `caBX` chunks the walk found after `IDAT`, and the IHDR the chunks are read against.
///
/// The one place that addition happens, so `decode` and both `metadata` entry points cannot come
/// to report different counts for the same file.
pub(crate) fn collected(
    chunks: &[([u8; 4], &[u8])],
    budget: usize,
    c2pa_after_idat: usize,
    header: &Ihdr,
) -> PngMetadata {
    let mut meta = decoded::collect(chunks, budget, header);
    meta.c2pa_ignored += c2pa_after_idat;
    meta
}

/// Reads a PNG's ancillary metadata without decoding any pixels.
///
/// Walks the chunk stream, collecting the metadata-bearing ancillary chunks and inflating only
/// the compressed ones (iCCP, zTXt, and compressed iTXt) under one cumulative 16 MiB budget,
/// which the raw C2PA manifest store (`caBX`) is charged to as well. IDAT is skipped by length,
/// so no pixel data is read, copied, or inflated — which makes this cheap enough for a probe on
/// a large file. `cICP`, `sRGB`, `gAMA` and `cHRM` are uncompressed and cost nothing beyond the
/// walk.
///
/// The result matches [`PngDecoder::decode`] field for field on the same file. Use
/// [`PngDecoder::metadata`] instead if you need a metadata budget other than the default.
///
/// # Errors
///
/// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if the
/// signature, IHDR, or chunk framing is malformed, IHDR is duplicated, or IEND is missing; an
/// [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) error for an unknown critical
/// chunk. Unlike
/// [`PngDecoder::decode`] it does **not** require IDAT — a stream carrying only metadata is read
/// successfully. Malformed *metadata* payloads are never errors: the affected chunk is skipped
/// (§13.1) and its field stays empty, as are chunks whose CRC does not match.
///
/// # Example
///
/// The extracted payload is byte-for-byte identical to the one given to the encoder:
///
/// ```
/// use gamut_core::{Dimensions, EncodeImage, ImageRef, Rgb8};
/// use gamut_png::PngEncoder;
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let exif = b"II\x2a\x00opaque exif bytes";
/// let pixels = vec![0u8; 3 * 4];
/// let image = ImageRef::<Rgb8>::new(&pixels, Dimensions::new(2, 2)?)?;
/// let png = PngEncoder::new().with_exif(exif).encode_to_vec(image)?;
///
/// let meta = gamut_png::metadata(&png)?;
/// assert_eq!(meta.exif.as_deref(), Some(exif.as_slice()));
/// # Ok(())
/// # }
/// ```
///
/// A PNG with no ancillary chunks reads back as the default:
///
/// ```
/// use gamut_core::{Dimensions, EncodeImage, ImageRef, Rgb8};
/// use gamut_png::{PngEncoder, PngMetadata};
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let pixels = vec![0u8; 3 * 4];
/// let image = ImageRef::<Rgb8>::new(&pixels, Dimensions::new(2, 2)?)?;
/// let png = PngEncoder::new().encode_to_vec(image)?;
/// assert_eq!(gamut_png::metadata(&png)?, PngMetadata::default());
/// # Ok(())
/// # }
/// ```
pub fn metadata(data: &[u8]) -> Result<PngMetadata> {
    let (chunks, c2pa_after_idat, header) = walk_metadata_chunks(data)?;
    Ok(collected(
        &chunks,
        DEFAULT_MAX_METADATA_BYTES,
        c2pa_after_idat,
        &header,
    ))
}
