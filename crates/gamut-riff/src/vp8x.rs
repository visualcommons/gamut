//! The [`Vp8xHeader`] extended-format feature header, and the writers that assemble whole WebP
//! files around it: the simple (single-bitstream) formats and the extended format, with or without
//! metadata (RFC 9649 §2.5-§2.7).
//!
//! The extended writers place the `C2PA` manifest store last of all, as C2PA 2.4 §A.3.7 requires;
//! reading the store back is [`crate::c2pa`]'s concern.

use gamut_core::{Error, Result};

#[cfg(doc)]
use crate::c2pa::c2pa_span;
use crate::chunk::Chunk;
use crate::fourcc::FourCc;
use crate::layout::MetadataChunks;
#[cfg(doc)]
use crate::layout::{WebpChunkId, WebpLayout};
use crate::writer::RiffWriter;

/// The number of bytes in a `VP8X` chunk payload (RFC 9649 §2.7).
pub const VP8X_PAYLOAD_LEN: usize = 10;

/// The largest canvas dimension a `VP8X` header can express: the width and height are stored
/// 1-based in 24 bits, so `1..=2^24` (RFC 9649 §2.7).
pub const MAX_CANVAS_DIMENSION: u32 = 1 << 24;

/// The extended-format feature header carried by a `VP8X` chunk (RFC 9649 §2.7): which optional
/// features the file uses, plus the 1-based canvas dimensions. A simple (single-bitstream) file has no
/// `VP8X` chunk; one is required as soon as the file carries alpha, an ICC profile, metadata, or
/// animation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Vp8xHeader {
    /// The file contains an `ICCP` (ICC color profile) chunk.
    pub icc_profile: bool,
    /// The image carries transparency (an `ALPH` chunk, or alpha in a `VP8L` bitstream).
    pub alpha: bool,
    /// The file contains `EXIF` metadata.
    pub exif_metadata: bool,
    /// The file contains `XMP ` metadata.
    pub xmp_metadata: bool,
    /// The image is animated (`ANIM`/`ANMF` chunks).
    pub animation: bool,
    /// Canvas width in pixels (1-based; `1..=2^24`).
    pub canvas_width: u32,
    /// Canvas height in pixels (1-based; `1..=2^24`).
    pub canvas_height: u32,
}

impl Vp8xHeader {
    /// Encodes the 10-byte `VP8X` chunk payload (RFC 9649 §2.7, Figure 7): the feature-flag byte,
    /// three reserved bytes, and the 24-bit little-endian canvas width-minus-one and height-minus-one.
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if the
    /// canvas is one the format cannot express: either dimension outside
    /// `1..=`[`MAX_CANVAS_DIMENSION`], or a width × height product above `2^32 - 1`, both of which
    /// §2.7 forbids. Validating here rather than truncating means a header that encodes always
    /// decodes back to the same canvas.
    pub fn to_bytes(&self) -> Result<[u8; VP8X_PAYLOAD_LEN]> {
        self.validate_canvas()?;
        let flags = (u8::from(self.icc_profile) << 5)
            | (u8::from(self.alpha) << 4)
            | (u8::from(self.exif_metadata) << 3)
            | (u8::from(self.xmp_metadata) << 2)
            | (u8::from(self.animation) << 1);
        // Validated above, so neither subtraction underflows and both fit in 24 bits.
        let w = self.canvas_width - 1;
        let h = self.canvas_height - 1;
        Ok([
            flags,
            0,
            0,
            0,
            w as u8,
            (w >> 8) as u8,
            (w >> 16) as u8,
            h as u8,
            (h >> 8) as u8,
            (h >> 16) as u8,
        ])
    }

    /// Rejects a canvas the `VP8X` fields cannot carry (RFC 9649 §2.7): each dimension is 1-based in
    /// 24 bits, so `1..=2^24`, and "the product of _Canvas Width_ and _Canvas Height_ MUST be at
    /// most 2^32 - 1".
    fn validate_canvas(&self) -> Result<()> {
        for dimension in [self.canvas_width, self.canvas_height] {
            if dimension == 0 || dimension > MAX_CANVAS_DIMENSION {
                return Err(Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "VP8X: canvas dimension outside 1..=2^24",
                ));
            }
        }
        if u64::from(self.canvas_width) * u64::from(self.canvas_height) > u64::from(u32::MAX) {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "VP8X: canvas width x height exceeds 2^32 - 1",
            ));
        }
        Ok(())
    }

    /// Parses a `VP8X` chunk payload, mirroring [`to_bytes`](Self::to_bytes). The two reserved
    /// fields are ignored as the spec requires.
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if
    /// `payload` is shorter than [`VP8X_PAYLOAD_LEN`], or if the canvas it declares has a width ×
    /// height product above `2^32 - 1`, which §2.7 forbids. (The dimensions themselves need no
    /// check: 24 bits stored 1-based can only land in `1..=2^24`.)
    pub fn parse(payload: &[u8]) -> Result<Self> {
        if payload.len() < VP8X_PAYLOAD_LEN {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "VP8X: chunk payload shorter than 10 bytes",
            )
            .with_byte_offset(payload.len() as u64));
        }
        let flags = payload[0];
        let le24 = |b: &[u8]| u32::from(b[0]) | (u32::from(b[1]) << 8) | (u32::from(b[2]) << 16);
        let header = Self {
            icc_profile: flags & 0x20 != 0,
            alpha: flags & 0x10 != 0,
            exif_metadata: flags & 0x08 != 0,
            xmp_metadata: flags & 0x04 != 0,
            animation: flags & 0x02 != 0,
            canvas_width: le24(&payload[4..7]) + 1,
            canvas_height: le24(&payload[7..10]) + 1,
        };
        header
            .validate_canvas()
            .map_err(|e| e.with_byte_offset(4))?;
        Ok(header)
    }
}

/// Writes an extended WebP file: the `RIFF`/`WEBP` header, a `VP8X` feature header, then the given
/// chunks in order (RFC 9649 §2.7). Chunk ordering (e.g. `ALPH` before the `VP8 ` bitstream) is the
/// caller's responsibility.
///
/// # Errors
///
/// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if `header`
/// declares a canvas the format cannot express (see [`Vp8xHeader::to_bytes`]), or an
/// [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) error if a payload or the
/// finished file exceeds the RIFF size fields.
pub fn write_extended(header: &Vp8xHeader, chunks: &[Chunk<'_>]) -> Result<Vec<u8>> {
    let mut w = RiffWriter::new();
    w.write_chunk(FourCc::VP8X, &header.to_bytes()?)?;
    for chunk in chunks {
        w.write_chunk(chunk.fourcc, chunk.payload)?;
    }
    w.finish()
}

/// Writes an extended WebP file carrying `metadata`, placing every chunk in the canonical order the
/// spec mandates: `VP8X`, `ICCP`, the image data (an optional `ALPH` then the `VP8 `/`VP8L`
/// bitstream), then `EXIF`, `XMP ` and last of all `C2PA` (RFC 9649 §2.7 — readers "SHOULD fail"
/// when the chunks needed for reconstruction and colour correction are out of order, and metadata
/// follows the image data; C2PA 2.4 §A.3.7 for the store's place at the very end).
///
/// The three RFC 9649 metadata feature flags of `header` are **derived** from `metadata`, so a chunk
/// can never be emitted without its flag nor a flag without its chunk; `alpha`, `animation`, and the
/// canvas size are taken as given. The `C2PA` chunk has no feature flag to derive — §2.5's flag byte
/// defines none — so its presence is decided by the chunk alone. Ordering *within* `image_data` is
/// the caller's responsibility, as in [`write_extended`].
///
/// # Errors
///
/// As [`write_extended`]: an inexpressible canvas or an over-large payload or file.
pub fn write_extended_with_metadata(
    header: &Vp8xHeader,
    metadata: &MetadataChunks<'_>,
    image_data: &[Chunk<'_>],
) -> Result<Vec<u8>> {
    write_extended_preserving(header, metadata, image_data, &[])
}

/// As [`write_extended_with_metadata`], additionally re-emitting `unknown` chunks after the metadata.
///
/// A chunk whose FourCC the container spec does not define is an *unknown chunk*, and "writers
/// SHOULD preserve them in their original order" (RFC 9649 §2.7.1.6). Pass the
/// [`WebpLayout::unknown`] of a file that was read to carry an application's private chunks through
/// a read/modify/write cycle instead of dropping them. The spec places unknown chunks at the end of
/// the file and lets them "appear out of order" relative to metadata, so emitting them last is
/// conforming regardless of where they sat in the original.
///
/// A `C2PA` manifest store is the one exception to "unknown chunks last": C2PA 2.4 §A.3.7 requires
/// the `C2PA` chunk to "appear as the last sub-chunk of the first RIFF header chunk", so it is
/// emitted after the preserved chunks, not before them.
///
/// A file this function writes carries **at most one** `C2PA` chunk, and it is always in the
/// store's place at the end. A `C2PA` chunk among `unknown` is never emitted where it sits:
///
/// - with [`MetadataChunks::c2pa`] set, the field owns the chunk and the copy in `unknown` is
///   **dropped**. Emitting both would put the copy first, and every reader here takes the *first* of
///   a repeated chunk — so it would win, and [`c2pa_span`] would report a range over bytes the
///   caller never configured, which is exactly the range a signer excludes from its hash;
/// - with no store configured, the copy **is** the store of the file it came from, so it is kept and
///   written in the store's place. Dropping it would lose a foreign manifest store with no signal,
///   and leaving it among the unknown chunks could put it somewhere §A.3.7 does not allow.
///
/// Where `unknown` holds several, the first wins, as everywhere else in this crate. A caller who
/// read the file with [`WebpLayout::parse`] never sees a `C2PA` chunk in `unknown` at all — this
/// crate recognises it ([`WebpChunkId::C2pa`]) — so this handles chunks collected by a reader that
/// does not, such as a third-party one or this crate before the chunk was modelled.
///
/// [`write_extended`] does no filtering: it writes the chunks it is given, and is the escape hatch
/// for a caller who means to hand-assemble a file.
///
/// # Errors
///
/// As [`write_extended`]: an inexpressible canvas or an over-large payload or file.
pub fn write_extended_preserving(
    header: &Vp8xHeader,
    metadata: &MetadataChunks<'_>,
    image_data: &[Chunk<'_>],
    unknown: &[Chunk<'_>],
) -> Result<Vec<u8>> {
    let header = Vp8xHeader {
        icc_profile: metadata.icc.is_some(),
        exif_metadata: metadata.exif.is_some(),
        xmp_metadata: metadata.xmp.is_some(),
        ..*header
    };
    let chunk = |fourcc, payload| Chunk { fourcc, payload };
    let mut chunks: Vec<Chunk<'_>> = Vec::with_capacity(image_data.len() + 4 + unknown.len());
    if let Some(icc) = metadata.icc {
        chunks.push(chunk(FourCc::ICCP, icc));
    }
    chunks.extend_from_slice(image_data);
    if let Some(exif) = metadata.exif {
        chunks.push(chunk(FourCc::EXIF, exif));
    }
    if let Some(xmp) = metadata.xmp {
        chunks.push(chunk(FourCc::XMP, xmp));
    }
    // A `C2PA` chunk never goes out among the unknown chunks: either the configured store owns that
    // slot, or — with none configured — this one *is* the store and belongs in the store's place.
    // First wins, as everywhere else here.
    let carried = unknown.iter().find(|c| c.fourcc == FourCc::C2PA);
    chunks.extend(unknown.iter().filter(|c| c.fourcc != FourCc::C2PA));
    // C2PA 2.4 §A.3.7: the manifest store's chunk is the *last* sub-chunk of the RIFF/WEBP form —
    // after the preserved unknown chunks, not merely somewhere past the image data.
    if let Some(c2pa) = metadata.c2pa.or(carried.map(|c| c.payload)) {
        chunks.push(chunk(FourCc::C2PA, c2pa));
    }
    write_extended(&header, &chunks)
}

/// Wraps a VP8L lossless bitstream in the simple WebP (lossless) file format: a `RIFF`/`WEBP` header
/// and a single `VP8L` chunk (RFC 9649 §2.6).
///
/// # Errors
///
/// Returns an [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) error if the
/// bitstream or the finished file exceeds the RIFF size fields (§2.3, §2.4).
pub fn write_simple_lossless(vp8l_bitstream: &[u8]) -> Result<Vec<u8>> {
    let mut w = RiffWriter::new();
    w.write_chunk(FourCc::VP8L, vp8l_bitstream)?;
    w.finish()
}

/// Wraps a VP8 lossy bitstream in the simple WebP (lossy) file format: a `RIFF`/`WEBP` header and a
/// single `VP8 ` chunk (RFC 9649 §2.5).
///
/// # Errors
///
/// Returns an [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) error if the
/// bitstream or the finished file exceeds the RIFF size fields (§2.3, §2.4).
pub fn write_simple_lossy(vp8_bitstream: &[u8]) -> Result<Vec<u8>> {
    let mut w = RiffWriter::new();
    w.write_chunk(FourCc::VP8, vp8_bitstream)?;
    w.finish()
}

#[cfg(test)]
mod tests {
    use gamut_core::ErrorKind;

    use super::*;
    use crate::layout::WebpChunkId;
    use crate::reader::RiffReader;
    use crate::test_support::{chunk, chunk_ids, span_in, vp8x_header_of};

    #[test]
    fn simple_lossless_wraps_one_vp8l_chunk() {
        let bitstream = [0x2f, 0xde, 0xad, 0xbe, 0xef];
        let file = write_simple_lossless(&bitstream).unwrap();
        let chunks: Vec<_> = RiffReader::new(&file)
            .unwrap()
            .map(|c| c.unwrap())
            .collect();
        assert_eq!(chunks.len(), 1);
        assert_eq!(WebpChunkId::from(chunks[0].fourcc), WebpChunkId::Vp8l);
        assert_eq!(chunks[0].payload, &bitstream);
    }

    #[test]
    fn vp8x_header_round_trips() {
        let h = Vp8xHeader {
            icc_profile: false,
            alpha: true,
            exif_metadata: false,
            xmp_metadata: false,
            animation: false,
            canvas_width: 640,
            canvas_height: 481,
        };
        let payload = h.to_bytes().unwrap();
        assert_eq!(payload.len(), VP8X_PAYLOAD_LEN);
        assert_eq!(payload[0] & 0x10, 0x10, "alpha (L) flag is bit 4");
        assert_eq!(&payload[1..4], &[0, 0, 0], "reserved bytes are zero");
        assert_eq!(Vp8xHeader::parse(&payload).unwrap(), h);
    }

    #[test]
    fn vp8x_all_flags_and_large_canvas_round_trip() {
        // Every feature flag set, plus a dimension large enough to use all three bytes of its 24-bit
        // field — the plain round-trip only sets `alpha` and a sub-2^16 canvas, so the other flags'
        // shifts/masks and the high dimension byte (`>> 16`) would otherwise go unexercised.
        //
        // The two dimensions must be exercised *separately*: §2.7 caps width × height at 2^32 - 1,
        // and a third byte is non-zero only from 65537 up, so 65537^2 = 4_295_098_369 already
        // exceeds the cap. No legal canvas uses all three bytes of both fields at once.
        let wide = Vp8xHeader {
            icc_profile: true,
            alpha: true,
            exif_metadata: true,
            xmp_metadata: true,
            animation: true,
            canvas_width: 0x12_3456 + 1,
            canvas_height: 2,
        };
        let p = wide.to_bytes().unwrap();
        // flags = icc(0x20) | alpha(0x10) | exif(0x08) | xmp(0x04) | anim(0x02).
        assert_eq!(p[0], 0x3E);
        // 24-bit little-endian width-1 then height-1.
        assert_eq!(&p[4..7], &[0x56, 0x34, 0x12]);
        assert_eq!(&p[7..10], &[0x01, 0x00, 0x00]);
        assert_eq!(Vp8xHeader::parse(&p).unwrap(), wide);

        let tall = Vp8xHeader {
            canvas_width: 2,
            canvas_height: 0x65_4321 + 1,
            ..wide
        };
        let p = tall.to_bytes().unwrap();
        assert_eq!(&p[4..7], &[0x01, 0x00, 0x00]);
        assert_eq!(&p[7..10], &[0x21, 0x43, 0x65]);
        assert_eq!(Vp8xHeader::parse(&p).unwrap(), tall);
    }

    /// The canvas product bound is `<= 2^32 - 1`, so a product of *exactly* `2^32 - 1` is legal.
    ///
    /// RFC 9649 §2.7: "the product of Canvas Width and Canvas Height MUST be at most 2^32 - 1".
    /// The straddling pair is `65537 x 65535`, whose product is `65536^2 - 1` — exactly the bound,
    /// and both dimensions inside the 24-bit fields. Without this, nothing distinguished `>` from
    /// `>=` in the check: every other canvas test is either comfortably inside the bound or well
    /// past it, so the boundary itself was never evaluated (#110).
    #[test]
    fn vp8x_accepts_a_canvas_product_of_exactly_u32_max() {
        let exact = Vp8xHeader {
            canvas_width: 65_537,
            canvas_height: 65_535,
            ..Default::default()
        };
        assert_eq!(
            u64::from(exact.canvas_width) * u64::from(exact.canvas_height),
            u64::from(u32::MAX),
            "the fixture must sit exactly on the bound, or it tests nothing"
        );
        assert!(
            exact.to_bytes().is_ok(),
            "a product of exactly 2^32 - 1 is at most 2^32 - 1"
        );

        // And one more pixel over is refused, so the pair brackets the boundary.
        let over = Vp8xHeader {
            canvas_width: 65_538,
            canvas_height: 65_535,
            ..Default::default()
        };
        let err = over.to_bytes().expect_err("one past the bound is refused");
        assert!(
            err.to_string().contains("exceeds 2^32 - 1"),
            "wrong rejection reason: {err}"
        );
    }

    #[test]
    fn vp8x_rejects_a_canvas_the_format_cannot_express() {
        let ok = Vp8xHeader {
            canvas_width: MAX_CANVAS_DIMENSION,
            canvas_height: 1,
            ..Default::default()
        };
        assert!(ok.to_bytes().is_ok(), "2^24 x 1 is the largest width");

        // Zero is not representable: the field is 1-based, so 0 would encode as -1.
        for bad in [
            Vp8xHeader {
                canvas_width: 0,
                canvas_height: 1,
                ..Default::default()
            },
            Vp8xHeader {
                canvas_width: 1,
                canvas_height: 0,
                ..Default::default()
            },
            // One past the 24-bit field in each dimension.
            Vp8xHeader {
                canvas_width: MAX_CANVAS_DIMENSION + 1,
                canvas_height: 1,
                ..Default::default()
            },
            Vp8xHeader {
                canvas_width: 1,
                canvas_height: MAX_CANVAS_DIMENSION + 1,
                ..Default::default()
            },
        ] {
            let error = bad.to_bytes().expect_err("outside 1..=2^24");
            assert_eq!(error.origin(), Some("gamut-riff"));
            assert_eq!(error.kind(), ErrorKind::InvalidInput);
        }

        // Both dimensions legal on their own, but the product exceeds 2^32 - 1. 65536 x 65536 is
        // exactly 2^32, one past the cap; 65536 x 65535 is the largest square-ish canvas allowed.
        let over = Vp8xHeader {
            canvas_width: 65536,
            canvas_height: 65536,
            ..Default::default()
        };
        assert!(over.to_bytes().is_err(), "65536^2 is 2^32, one too many");
        let under = Vp8xHeader {
            canvas_height: 65535,
            ..over
        };
        assert!(under.to_bytes().is_ok(), "65536 x 65535 fits");
    }

    #[test]
    fn parse_rejects_a_canvas_whose_product_overflows() {
        // A hostile file can declare a canvas no encoder would produce: 2^24 x 2^24 = 2^48 pixels.
        // Rejecting it on read matches libwebp, which caps the decoded canvas area the same way.
        let mut payload = [0u8; VP8X_PAYLOAD_LEN];
        payload[4..7].copy_from_slice(&[0xFF, 0xFF, 0xFF]);
        payload[7..10].copy_from_slice(&[0xFF, 0xFF, 0xFF]);
        let error = Vp8xHeader::parse(&payload).expect_err("2^48 pixels");
        assert_eq!(error.byte_offset(), Some(4));
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
    }

    #[test]
    fn vp8x_no_flags_round_trips() {
        // All flags clear: each `flags & MASK` test (notably `alpha`, the one set above) is exercised
        // in its *false* state, so a mask mutated to `|` (always-set) is caught.
        let h = Vp8xHeader {
            canvas_width: 1,
            canvas_height: 1,
            ..Default::default()
        };
        let p = h.to_bytes().unwrap();
        assert_eq!(p[0], 0x00);
        assert_eq!(Vp8xHeader::parse(&p).unwrap(), h);
    }

    #[test]
    fn parse_rejects_short_input() {
        assert!(Vp8xHeader::parse(&[0u8; 9]).is_err());
    }

    #[test]
    fn write_extended_assembles_vp8x_then_chunks() {
        let h = Vp8xHeader {
            alpha: true,
            canvas_width: 16,
            canvas_height: 16,
            ..Default::default()
        };
        let file = write_extended(
            &h,
            &[
                chunk(FourCc::ALPH, &[1, 2, 3]),
                chunk(FourCc::VP8, &[0x9d, 0x01, 0x2a]),
            ],
        )
        .unwrap();
        let chunks: Vec<_> = RiffReader::new(&file)
            .unwrap()
            .map(|c| c.unwrap())
            .collect();
        assert_eq!(WebpChunkId::from(chunks[0].fourcc), WebpChunkId::Vp8x);
        assert_eq!(WebpChunkId::from(chunks[1].fourcc), WebpChunkId::Alpha);
        assert_eq!(WebpChunkId::from(chunks[2].fourcc), WebpChunkId::Vp8);
        assert_eq!(Vp8xHeader::parse(chunks[0].payload).unwrap(), h);
    }

    #[test]
    fn metadata_chunks_round_trip_in_canonical_order() {
        // The spec's order for a still image: VP8X, ICCP, image data, EXIF, XMP — and every payload
        // comes back byte-for-byte (odd lengths included, so the RIFF pad byte is not absorbed).
        let (icc, exif, xmp) = (&[1u8, 2, 3][..], &[0xee; 4][..], &b"<x:xmpmeta/>"[..]);
        let header = Vp8xHeader {
            alpha: true,
            canvas_width: 16,
            canvas_height: 8,
            ..Default::default()
        };
        let metadata = MetadataChunks {
            icc: Some(icc),
            exif: Some(exif),
            xmp: Some(xmp),
            c2pa: None,
        };
        let file = write_extended_with_metadata(
            &header,
            &metadata,
            &[
                chunk(FourCc::ALPH, &[9, 9]),
                chunk(FourCc::VP8, &[0x9d, 0x01, 0x2a]),
            ],
        )
        .unwrap();
        assert_eq!(
            chunk_ids(&file),
            vec![
                FourCc::VP8X,
                FourCc::ICCP,
                FourCc::ALPH,
                FourCc::VP8,
                FourCc::EXIF,
                FourCc::XMP,
            ]
        );
        assert_eq!(MetadataChunks::read(&file).unwrap(), metadata);
    }

    #[test]
    fn write_extended_with_metadata_derives_the_feature_flags() {
        // Each metadata flag follows its payload, not the caller's header, in **both** directions: a
        // stale flag with no payload is cleared, and a payload the caller forgot to flag is still
        // advertised. `alpha`, `animation`, and the canvas pass through untouched — the two headers
        // below differ only in the three metadata flags, so comparing whole headers pins that too.
        let all_flags = Vp8xHeader {
            icc_profile: true,
            exif_metadata: true,
            xmp_metadata: true,
            alpha: true,
            animation: true,
            canvas_width: 4,
            canvas_height: 5,
        };
        let no_flags = Vp8xHeader {
            icc_profile: false,
            exif_metadata: false,
            xmp_metadata: false,
            ..all_flags
        };
        let all_payloads = MetadataChunks {
            icc: Some(&[1]),
            exif: Some(&[2]),
            xmp: Some(&[3]),
            c2pa: None,
        };
        let image = &[chunk(FourCc::VP8L, &[0x2f])];

        let cleared =
            write_extended_with_metadata(&all_flags, &MetadataChunks::default(), image).unwrap();
        assert_eq!(
            vp8x_header_of(&cleared),
            no_flags,
            "a stale flag must be cleared when nothing is embedded"
        );
        assert_eq!(
            chunk_ids(&cleared),
            vec![FourCc::VP8X, FourCc::VP8L],
            "a stale flag must not conjure an empty chunk"
        );

        let advertised = write_extended_with_metadata(&no_flags, &all_payloads, image).unwrap();
        assert_eq!(
            vp8x_header_of(&advertised),
            all_flags,
            "an embedded payload must be advertised even if the caller left its flag unset"
        );
    }

    #[test]
    fn write_extended_with_metadata_flags_each_chunk_independently() {
        // One payload at a time: only that chunk's flag may be set, so the three assignments cannot be
        // swapped or share a source.
        let base = Vp8xHeader {
            canvas_width: 2,
            canvas_height: 2,
            ..Default::default()
        };
        let image = &[chunk(FourCc::VP8L, &[0x2f])];
        for (chunks, want) in [
            (
                MetadataChunks {
                    icc: Some(&[1]),
                    ..Default::default()
                },
                Vp8xHeader {
                    icc_profile: true,
                    ..base
                },
            ),
            (
                MetadataChunks {
                    exif: Some(&[2]),
                    ..Default::default()
                },
                Vp8xHeader {
                    exif_metadata: true,
                    ..base
                },
            ),
            (
                MetadataChunks {
                    xmp: Some(&[3]),
                    ..Default::default()
                },
                Vp8xHeader {
                    xmp_metadata: true,
                    ..base
                },
            ),
        ] {
            let file = write_extended_with_metadata(&base, &chunks, image).unwrap();
            assert_eq!(vp8x_header_of(&file), want, "flags for {chunks:?}");
        }
    }

    #[test]
    fn metadata_chunks_default_is_empty_and_writes_no_metadata() {
        // With nothing to embed, the extended file is exactly `write_extended`'s output — the
        // pre-metadata byte stream is preserved.
        let empty = MetadataChunks::default();
        assert!(empty.is_empty());
        let header = Vp8xHeader {
            alpha: true,
            canvas_width: 2,
            canvas_height: 2,
            ..Default::default()
        };
        let image = &[chunk(FourCc::ALPH, &[1]), chunk(FourCc::VP8, &[2])];
        assert_eq!(
            write_extended_with_metadata(&header, &empty, image).unwrap(),
            write_extended(&header, image).unwrap()
        );
    }

    #[test]
    fn simple_lossy_wraps_one_vp8_chunk() {
        let bitstream = [0x9d, 0x01, 0x2a];
        let file = write_simple_lossy(&bitstream).unwrap();
        let chunks: Vec<_> = RiffReader::new(&file)
            .unwrap()
            .map(|c| c.unwrap())
            .collect();
        assert_eq!(chunks.len(), 1);
        assert_eq!(WebpChunkId::from(chunks[0].fourcc), WebpChunkId::Vp8);
        assert_eq!(chunks[0].payload, &bitstream);
    }

    /// C2PA 2.4 §A.3.7: "this C2PA chunk shall appear as the **last sub-chunk of the first RIFF
    /// header chunk**" — for WebP, the last sub-chunk of the `RIFF`/`WEBP` form.
    ///
    /// "Last" is stricter than "after the image data": the store goes behind `EXIF`, behind `XMP `,
    /// and behind the unknown chunks §2.7.1.6 asks writers to preserve, which
    /// `write_extended_preserving` otherwise emits at the end. Nothing else pins that the store
    /// outranks the preserved chunks.
    #[test]
    fn the_c2pa_chunk_is_written_last_of_all() {
        let header = Vp8xHeader {
            canvas_width: 16,
            canvas_height: 16,
            ..Default::default()
        };
        let metadata = MetadataChunks {
            icc: Some(b"icc"),
            exif: Some(b"exif"),
            xmp: Some(b"<x/>"),
            c2pa: Some(b"store"),
        };
        let unknown = [Chunk {
            fourcc: FourCc::from(*b"XYZW"),
            payload: &[0xaa],
        }];
        let file = write_extended_preserving(
            &header,
            &metadata,
            &[chunk(FourCc::VP8L, &[0x2f])],
            &unknown,
        )
        .unwrap();
        assert_eq!(
            chunk_ids(&file),
            vec![
                FourCc::VP8X,
                FourCc::ICCP,
                FourCc::VP8L,
                FourCc::EXIF,
                FourCc::XMP,
                FourCc::from(*b"XYZW"),
                FourCc::C2PA,
            ]
        );
        assert_eq!(MetadataChunks::read(&file).unwrap(), metadata);
    }

    /// The `VP8X` feature byte has no C2PA bit (RFC 9649 §2.5), so a store must neither set one nor
    /// disturb the three flags that do exist: presence is decided by the chunk alone.
    #[test]
    fn a_c2pa_store_sets_no_vp8x_feature_flag() {
        let header = Vp8xHeader {
            canvas_width: 4,
            canvas_height: 4,
            ..Default::default()
        };
        let image = &[chunk(FourCc::VP8L, &[0x2f])];
        let with_store = write_extended_with_metadata(
            &header,
            &MetadataChunks {
                c2pa: Some(b"store"),
                ..Default::default()
            },
            image,
        )
        .unwrap();
        assert_eq!(vp8x_header_of(&with_store), header);
        // Byte for byte the file is a store-free one with the chunk appended: nothing ahead of the
        // store moved, and no flag byte changed.
        let without =
            write_extended_with_metadata(&header, &MetadataChunks::default(), image).unwrap();
        assert_eq!(&with_store[12..without.len()], &without[12..]);
    }

    /// `metadata.c2pa` owns the `C2PA` chunk: a copy handed in through `unknown` is dropped, so the
    /// file carries exactly one store and it is the configured one.
    ///
    /// Emitting both put the stale copy *first*, and every reader here takes the first — so
    /// `c2pa_span` reported a range over bytes the caller never configured, which is precisely the
    /// range a signer would exclude from its hash.
    #[test]
    fn a_c2pa_chunk_in_unknown_never_displaces_the_configured_store() {
        let header = Vp8xHeader {
            canvas_width: 4,
            canvas_height: 4,
            ..Default::default()
        };
        let file = write_extended_preserving(
            &header,
            &MetadataChunks {
                c2pa: Some(b"FRESH"),
                ..Default::default()
            },
            &[chunk(FourCc::VP8L, &[0x2f])],
            &[
                Chunk {
                    fourcc: FourCc::C2PA,
                    payload: b"STALE",
                },
                Chunk {
                    fourcc: FourCc::from(*b"XYZW"),
                    payload: b"kept",
                },
            ],
        )
        .unwrap();

        assert_eq!(
            chunk_ids(&file),
            vec![
                FourCc::VP8X,
                FourCc::VP8L,
                FourCc::from(*b"XYZW"),
                FourCc::C2PA,
            ],
            "one store chunk, last, and the genuinely unknown chunk survives"
        );
        assert_eq!(
            MetadataChunks::read(&file).unwrap().c2pa,
            Some(&b"FRESH"[..])
        );
        let span = span_in(&file).expect("the store was embedded");
        assert_eq!(&file[span.start + 8..span.end], b"FRESH");
    }

    /// With no store configured there is nothing for a carried `C2PA` chunk to displace, so it is
    /// kept rather than dropped: it is the manifest store of the file it was read out of, and losing
    /// it silently would be worse than carrying it. It still goes in the store's place (§A.3.7)
    /// rather than wherever the caller's list happened to put it.
    #[test]
    fn a_carried_c2pa_chunk_is_kept_as_the_store_when_none_is_configured() {
        let header = Vp8xHeader {
            canvas_width: 4,
            canvas_height: 4,
            ..Default::default()
        };
        let file = write_extended_preserving(
            &header,
            &MetadataChunks::default(),
            &[chunk(FourCc::VP8L, &[0x2f])],
            &[
                Chunk {
                    fourcc: FourCc::C2PA,
                    payload: b"foreign",
                },
                Chunk {
                    fourcc: FourCc::from(*b"XYZW"),
                    payload: b"kept",
                },
            ],
        )
        .unwrap();

        assert_eq!(
            chunk_ids(&file),
            vec![
                FourCc::VP8X,
                FourCc::VP8L,
                FourCc::from(*b"XYZW"),
                FourCc::C2PA,
            ],
            "the carried store moves to the end; the private chunk keeps its order"
        );
        assert_eq!(
            MetadataChunks::read(&file).unwrap().c2pa,
            Some(&b"foreign"[..]),
            "and it reads back as the store, which is what it is"
        );
    }

    /// `write_extended` is the unfiltered escape hatch: it writes what it is given, so a caller who
    /// means to hand-assemble a file still can.
    #[test]
    fn write_extended_does_not_filter_a_c2pa_chunk() {
        let header = Vp8xHeader {
            canvas_width: 4,
            canvas_height: 4,
            ..Default::default()
        };
        let file = write_extended(&header, &[chunk(FourCc::C2PA, b"store")]).unwrap();
        assert_eq!(chunk_ids(&file), vec![FourCc::VP8X, FourCc::C2PA]);
    }
}
