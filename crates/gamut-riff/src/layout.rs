//! Reading a WebP file's chunks by role: [`WebpChunkId`] classifies a chunk, [`MetadataChunks`]
//! collects the `ICCP`/`EXIF`/`XMP `/`C2PA` payloads, and [`WebpLayout`] sorts every chunk into its
//! role while enforcing the spec's ordering rule (RFC 9649 §2.5-§2.7).
//!
//! The remaining extended-format chunks (`ANIM`/`ANMF`) are tracked in `gamut-webp/STATUS.md`
//! section A and are out of scope under the image-first charter: they are classified, so an
//! animated file is refused rather than mis-read.

use gamut_core::{Error, Result};

use crate::chunk::{CHUNK_HEADER_LEN, Chunk, RIFF_HEADER_LEN, pad_len};
use crate::fourcc::FourCc;
use crate::reader::RiffReader;
use crate::vp8x::Vp8xHeader;
#[cfg(doc)]
use crate::{c2pa::c2pa_span, vp8x::write_extended_preserving, vp8x::write_extended_with_metadata};

/// The metadata chunks an extended WebP file may carry, **borrowed** rather than copied: the `ICCP`
/// colour profile, the `EXIF` / `XMP ` metadata payloads (RFC 9649 §2.7.1.4-§2.7.1.5), and the
/// `C2PA` manifest store (C2PA 2.4 §A.3.7).
///
/// The container assigns these payloads no meaning — each is carried verbatim, so metadata survives
/// a read/write cycle byte for byte with no reserialization. Use [`MetadataChunks::read`] to collect
/// them from a file and [`write_extended_with_metadata`] to emit them in the spec's chunk order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MetadataChunks<'a> {
    /// The `ICCP` chunk payload: an ICC colour profile. `None` means sRGB is assumed (§2.7.1.4).
    pub icc: Option<&'a [u8]>,
    /// The `EXIF` chunk payload: Exif metadata, carried bare (no `"Exif\0\0"` signature).
    pub exif: Option<&'a [u8]>,
    /// The `XMP ` chunk payload: an XMP packet.
    pub xmp: Option<&'a [u8]>,
    /// The `C2PA` chunk payload: a C2PA manifest store, opaque here — nothing in this crate parses,
    /// validates or signs it. A writer places it last, as §A.3.7 requires; [`c2pa_span`] reports
    /// where it landed. The pad byte RIFF adds after an odd-length store is framing, not store.
    pub c2pa: Option<&'a [u8]>,
}

impl<'a> MetadataChunks<'a> {
    /// Collects the metadata chunks of the WebP file in `data`, borrowing each payload in place.
    ///
    /// The spec allows at most one chunk of each kind and lets readers "ignore all except the first
    /// one" (RFC 9649 §2.7.1.4-§2.7.1.5), so the **first** `ICCP` / `EXIF` / `XMP ` chunk wins. The
    /// `C2PA` chunk gets the same policy: C2PA 2.4 §A.3.7 admits one manifest store per file, so a
    /// second chunk is malformed and picking the first is as good a recovery as any. The
    /// `VP8X` feature flags are advisory here: a payload is reported because its chunk is present,
    /// never because a flag claims it is — so a flag set over a missing chunk yields `None`, and a
    /// chunk a non-conformant writer left unflagged is still recovered.
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if `data`
    /// is not a valid RIFF/WebP file, or if a chunk's declared size runs past the end of the data.
    pub fn read(data: &'a [u8]) -> Result<Self> {
        let mut found = Self::default();
        for chunk in RiffReader::new(data)? {
            let chunk = chunk?;
            let slot = match WebpChunkId::from(chunk.fourcc) {
                WebpChunkId::Iccp => &mut found.icc,
                WebpChunkId::Exif => &mut found.exif,
                WebpChunkId::Xmp => &mut found.xmp,
                WebpChunkId::C2pa => &mut found.c2pa,
                _ => continue,
            };
            slot.get_or_insert(chunk.payload);
        }
        Ok(found)
    }

    /// Whether no metadata chunk is present — i.e. nothing here forces a simple (single-bitstream)
    /// file into the extended format.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.icc.is_none() && self.exif.is_none() && self.xmp.is_none() && self.c2pa.is_none()
    }
}

/// The position a chunk occupies in the extended format's reconstruction sequence: `VP8X`, `ICCP`,
/// `ANIM`, then the image data (`ALPH` before the bitstream) — RFC 9649 §2.7.
///
/// `None` marks a chunk the ordering rule does not constrain: metadata (`EXIF`/`XMP `) and unknown
/// chunks, which the spec says "MAY appear out of order". `C2PA` joins them: §2.7 knows nothing of
/// it, and C2PA 2.4 §A.3.7's "last sub-chunk" is a rule this crate's *writer* honours rather than
/// one a reader may impose on a file it did not write.
const fn reconstruction_rank(id: WebpChunkId) -> Option<u8> {
    match id {
        WebpChunkId::Vp8x => Some(0),
        WebpChunkId::Iccp => Some(1),
        WebpChunkId::Anim => Some(2),
        WebpChunkId::Anmf | WebpChunkId::Alpha => Some(3),
        WebpChunkId::Vp8 | WebpChunkId::Vp8l => Some(4),
        WebpChunkId::Exif | WebpChunkId::Xmp | WebpChunkId::C2pa | WebpChunkId::Unknown(_) => None,
    }
}

/// A still-image WebP file's chunks, sorted into their roles and checked against the spec's
/// ordering rule (RFC 9649 §2.7).
///
/// Where [`RiffReader`] is the permissive low-level iterator and [`MetadataChunks::read`] collects
/// only the metadata, this is the **strict** reader: it rejects a file whose reconstruction chunks
/// are out of order, and it keeps the unknown chunks so a caller can write them back out with
/// [`write_extended_preserving`]. Every payload is borrowed from the input, never copied.
///
/// # Example
///
/// ```
/// use gamut_riff::{WebpLayout, WebpChunkId, write_simple_lossless};
///
/// let file = write_simple_lossless(&[0x2f, 0x01, 0x02])?;
/// let layout = WebpLayout::parse(&file)?;
/// assert_eq!(layout.bitstream, Some((WebpChunkId::Vp8l, &[0x2f, 0x01, 0x02][..])));
/// assert!(layout.unknown.is_empty());
/// # Ok::<(), gamut_core::Error>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct WebpLayout<'a> {
    /// The parsed `VP8X` feature header, or `None` for a simple (single-bitstream) file.
    pub vp8x: Option<Vp8xHeader>,
    /// The `ICCP`, `EXIF`, `XMP ` and `C2PA` payloads, first of each kind winning as the spec
    /// permits.
    pub metadata: MetadataChunks<'a>,
    /// The `ALPH` chunk payload, when the file carries lossy alpha.
    pub alph: Option<&'a [u8]>,
    /// The image bitstream and which codestream it is: `VP8 ` (lossy) or `VP8L` (lossless).
    pub bitstream: Option<(WebpChunkId, &'a [u8])>,
    /// Unknown chunks, in the order they appeared — what §2.7.1.6 asks writers to preserve.
    pub unknown: Vec<Chunk<'a>>,
    /// Bytes past the region the RIFF file-size field declares; see
    /// [`RiffReader::trailing_bytes`].
    pub trailing_bytes: usize,
}

impl<'a> WebpLayout<'a> {
    /// Parses the still-image WebP file in `data`.
    ///
    /// Enforces the ordering rule the spec states for the chunks "necessary for reconstruction and
    /// color correction" — `VP8X`, `ICCP`, `ANIM`, `ANMF`, `ALPH`, `VP8 `, `VP8L` — which "MUST
    /// appear in the order described" and over which "readers SHOULD fail" when they do not.
    /// Metadata and unknown chunks are exempt by the same paragraph and may appear anywhere.
    ///
    /// Where a chunk may legally repeat, the **first** wins, matching [`MetadataChunks::read`].
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if `data`
    /// is not a valid RIFF/WebP file, if a chunk runs past the end of the data, or if a
    /// reconstruction chunk appears out of order (the error carries the offending chunk's byte
    /// offset). Returns an [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) error for
    /// an animated file — an `ANIM` or `ANMF` chunk — which is outside this crate's still-image
    /// scope.
    pub fn parse(data: &'a [u8]) -> Result<Self> {
        let reader = RiffReader::new(data)?;
        let mut layout = Self {
            trailing_bytes: reader.trailing_bytes(),
            ..Self::default()
        };
        // Rank of the last reconstruction chunk seen; the sequence must never regress.
        let mut last_rank = 0;
        let mut offset = RIFF_HEADER_LEN;
        for chunk in reader {
            let chunk = chunk?;
            let id = WebpChunkId::from(chunk.fourcc);
            if let Some(rank) = reconstruction_rank(id) {
                if rank < last_rank {
                    return Err(Error::invalid_input(
                        env!("CARGO_PKG_NAME"),
                        "WebP: reconstruction chunks are out of order",
                    )
                    .with_byte_offset(offset as u64));
                }
                last_rank = rank;
            }
            match id {
                WebpChunkId::Vp8x => {
                    if layout.vp8x.is_none() {
                        layout.vp8x = Some(Vp8xHeader::parse(chunk.payload)?);
                    }
                }
                WebpChunkId::Iccp => {
                    layout.metadata.icc.get_or_insert(chunk.payload);
                }
                WebpChunkId::Exif => {
                    layout.metadata.exif.get_or_insert(chunk.payload);
                }
                WebpChunkId::Xmp => {
                    layout.metadata.xmp.get_or_insert(chunk.payload);
                }
                WebpChunkId::C2pa => {
                    layout.metadata.c2pa.get_or_insert(chunk.payload);
                }
                WebpChunkId::Alpha => {
                    layout.alph.get_or_insert(chunk.payload);
                }
                WebpChunkId::Vp8 | WebpChunkId::Vp8l => {
                    if layout.bitstream.is_none() {
                        layout.bitstream = Some((id, chunk.payload));
                    }
                }
                WebpChunkId::Anim | WebpChunkId::Anmf => {
                    return Err(Error::unsupported(
                        env!("CARGO_PKG_NAME"),
                        "WebP: animated files (ANIM/ANMF) are out of scope",
                    )
                    .with_byte_offset(offset as u64));
                }
                WebpChunkId::Unknown(_) => layout.unknown.push(chunk),
            }
            offset += CHUNK_HEADER_LEN + chunk.payload.len() + pad_len(chunk.payload.len() as u32);
        }
        Ok(layout)
    }
}

/// Identifies a WebP chunk by its FourCC, distinguishing the chunks defined by the WebP container
/// spec from any unrecognized ("unknown") chunk that readers must ignore (RFC 9649 §2.5-§2.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum WebpChunkId {
    /// Lossy VP8 bitstream (`VP8 `).
    Vp8,
    /// Lossless VP8L bitstream (`VP8L`).
    Vp8l,
    /// Extended-format feature header (`VP8X`).
    Vp8x,
    /// Alpha bitstream (`ALPH`).
    Alpha,
    /// ICC color profile (`ICCP`).
    Iccp,
    /// Exif metadata (`EXIF`).
    Exif,
    /// XMP metadata (`XMP `).
    Xmp,
    /// C2PA manifest store (`C2PA`) — C2PA 2.4 §A.3.7, not an RFC 9649 chunk.
    C2pa,
    /// Global animation parameters (`ANIM`).
    Anim,
    /// Animation frame (`ANMF`).
    Anmf,
    /// A chunk whose FourCC is not one defined by the WebP container spec.
    Unknown(FourCc),
}

impl From<FourCc> for WebpChunkId {
    fn from(fourcc: FourCc) -> Self {
        match &fourcc.0 {
            b"VP8 " => Self::Vp8,
            b"VP8L" => Self::Vp8l,
            b"VP8X" => Self::Vp8x,
            b"ALPH" => Self::Alpha,
            b"ICCP" => Self::Iccp,
            b"EXIF" => Self::Exif,
            b"XMP " => Self::Xmp,
            b"C2PA" => Self::C2pa,
            b"ANIM" => Self::Anim,
            b"ANMF" => Self::Anmf,
            _ => Self::Unknown(fourcc),
        }
    }
}

#[cfg(test)]
mod tests {
    use gamut_core::ErrorKind;

    use super::*;
    use crate::test_support::{chunk, raw_file, span_in, vp8x};
    use crate::vp8x::{
        write_extended_preserving, write_extended_with_metadata, write_simple_lossless,
    };
    use crate::writer::RiffWriter;

    #[test]
    fn classifies_known_and_unknown_chunks() {
        assert_eq!(WebpChunkId::from(FourCc::VP8), WebpChunkId::Vp8);
        assert_eq!(WebpChunkId::from(FourCc::VP8L), WebpChunkId::Vp8l);
        assert_eq!(WebpChunkId::from(FourCc::VP8X), WebpChunkId::Vp8x);
        assert_eq!(WebpChunkId::from(FourCc::ALPH), WebpChunkId::Alpha);
        assert_eq!(WebpChunkId::from(FourCc::ICCP), WebpChunkId::Iccp);
        assert_eq!(WebpChunkId::from(FourCc::EXIF), WebpChunkId::Exif);
        assert_eq!(WebpChunkId::from(FourCc::XMP), WebpChunkId::Xmp);
        assert_eq!(WebpChunkId::from(FourCc::C2PA), WebpChunkId::C2pa);
        assert_eq!(WebpChunkId::from(FourCc::ANIM), WebpChunkId::Anim);
        assert_eq!(WebpChunkId::from(FourCc::ANMF), WebpChunkId::Anmf);
        let weird = FourCc::from(*b"XYZW");
        assert_eq!(WebpChunkId::from(weird), WebpChunkId::Unknown(weird));
    }

    #[test]
    fn is_empty_is_false_for_each_chunk_kind() {
        // Each field must count towards emptiness on its own, so a promotion decision keyed on
        // `is_empty` cannot miss a single-chunk file.
        for chunks in [
            MetadataChunks {
                icc: Some(&[0]),
                ..Default::default()
            },
            MetadataChunks {
                exif: Some(&[0]),
                ..Default::default()
            },
            MetadataChunks {
                xmp: Some(&[0]),
                ..Default::default()
            },
            MetadataChunks {
                c2pa: Some(&[0]),
                ..Default::default()
            },
        ] {
            assert!(!chunks.is_empty(), "{chunks:?} carries a payload");
        }
    }

    #[test]
    fn metadata_chunks_read_keeps_the_first_of_each_kind() {
        // "Readers MAY ignore all except the first one" (§2.7.1.4-§2.7.1.5).
        let mut w = RiffWriter::new();
        w.write_chunk(FourCc::ICCP, b"first-icc").unwrap();
        w.write_chunk(FourCc::VP8L, &[0x2f]).unwrap();
        w.write_chunk(FourCc::EXIF, b"first-exif").unwrap();
        w.write_chunk(FourCc::EXIF, b"second-exif").unwrap();
        w.write_chunk(FourCc::ICCP, b"second-icc").unwrap();
        let file = w.finish().unwrap();
        let got = MetadataChunks::read(&file).unwrap();
        assert_eq!(got.icc, Some(&b"first-icc"[..]));
        assert_eq!(got.exif, Some(&b"first-exif"[..]));
        assert_eq!(got.xmp, None);
    }

    #[test]
    fn metadata_chunks_read_reports_presence_not_vp8x_flags() {
        // A flag set over a missing chunk yields `None`; a chunk an unconformant writer left
        // unflagged is still recovered. The flags never fabricate or suppress a payload.
        let lying = Vp8xHeader {
            icc_profile: true,
            exif_metadata: true,
            xmp_metadata: true,
            canvas_width: 1,
            canvas_height: 1,
            ..Default::default()
        };
        let mut w = RiffWriter::new();
        w.write_chunk(FourCc::VP8X, &lying.to_bytes().unwrap())
            .unwrap();
        w.write_chunk(FourCc::VP8L, &[0x2f]).unwrap();
        w.write_chunk(FourCc::XMP, b"<x/>").unwrap();
        let file = w.finish().unwrap();
        assert_eq!(
            MetadataChunks::read(&file).unwrap(),
            MetadataChunks {
                icc: None,
                exif: None,
                xmp: Some(&b"<x/>"[..]),
                c2pa: None,
            }
        );
    }

    #[test]
    fn metadata_chunks_read_is_empty_for_a_simple_file() {
        let file = write_simple_lossless(&[0x2f, 1, 2]).unwrap();
        assert!(MetadataChunks::read(&file).unwrap().is_empty());
    }

    #[test]
    fn metadata_chunks_read_rejects_malformed_input() {
        assert!(MetadataChunks::read(b"not a webp file").is_err());
        // A chunk whose declared size runs past the data must surface the reader's error rather than
        // being silently treated as "no metadata".
        let mut file = write_simple_lossless(&[0; 4]).unwrap();
        file[16..20].copy_from_slice(&5u32.to_le_bytes());
        assert!(MetadataChunks::read(&file).is_err());
    }

    #[test]
    fn layout_parses_a_simple_file() {
        let file = write_simple_lossless(&[0x2f, 1, 2]).unwrap();
        let layout = WebpLayout::parse(&file).unwrap();
        assert_eq!(layout.vp8x, None, "a simple file has no VP8X");
        assert_eq!(
            layout.bitstream,
            Some((WebpChunkId::Vp8l, &[0x2f, 1, 2][..]))
        );
        assert_eq!(layout.alph, None);
        assert!(layout.metadata.is_empty());
        assert!(layout.unknown.is_empty());
        assert_eq!(layout.trailing_bytes, 0);
    }

    #[test]
    fn layout_parses_the_canonical_extended_order() {
        // RFC 9649 §2.7.3, Figure 17: VP8X, ICCP, VP8L, XMP.
        let file = raw_file(&[
            (FourCc::VP8X, &vp8x(false)),
            (FourCc::ICCP, b"icc"),
            (FourCc::VP8L, &[0x2f]),
            (FourCc::XMP, b"<x/>"),
        ]);
        let layout = WebpLayout::parse(&file).unwrap();
        assert_eq!(layout.vp8x.unwrap().canvas_width, 16);
        assert_eq!(layout.metadata.icc, Some(&b"icc"[..]));
        assert_eq!(layout.metadata.xmp, Some(&b"<x/>"[..]));
        assert_eq!(layout.bitstream, Some((WebpChunkId::Vp8l, &[0x2f][..])));
    }

    #[test]
    fn layout_rejects_each_out_of_order_reconstruction_pair() {
        // "Readers SHOULD fail when chunks necessary for reconstruction and color correction are
        // out of order" (§2.7). Each pair below inverts one adjacent step of the sequence
        // VP8X -> ICCP -> ALPH -> bitstream, so no single rank comparison can be dropped.
        let alph: &[u8] = &[1, 2];
        let vp8: &[u8] = &[0x9d, 0x01, 0x2a];
        let inverted: &[&[(FourCc, &[u8])]] = &[
            // ICCP before VP8X
            &[
                (FourCc::ICCP, b"icc"),
                (FourCc::VP8X, &vp8x(true)),
                (FourCc::VP8, vp8),
            ],
            // ALPH before ICCP
            &[
                (FourCc::VP8X, &vp8x(true)),
                (FourCc::ALPH, alph),
                (FourCc::ICCP, b"icc"),
                (FourCc::VP8, vp8),
            ],
            // bitstream before ALPH
            &[
                (FourCc::VP8X, &vp8x(true)),
                (FourCc::VP8, vp8),
                (FourCc::ALPH, alph),
            ],
        ];
        for chunks in inverted {
            let file = raw_file(chunks);
            let error = WebpLayout::parse(&file).expect_err("out of order");
            assert_eq!(error.origin(), Some("gamut-riff"));
            assert_eq!(error.kind(), ErrorKind::InvalidInput);
            // The offset is asserted by VALUE, not merely `is_some()`. The walk advances by
            // `CHUNK_HEADER_LEN + payload + pad_len(payload)` per chunk, and `is_some()` holds for
            // every arithmetic mutation of that sum -- `+` to `-` or `*` alike -- because the
            // offending chunk is still reached and still reported, just at a nonsense position
            // (#110). Recomputing the expected offset from the fixture is what makes the sum
            // itself observable.
            let mut boundaries = vec![12u64];
            for (_, payload) in chunks.iter() {
                let last = *boundaries
                    .last()
                    .expect("seeded with the RIFF header length");
                boundaries.push(
                    last + (CHUNK_HEADER_LEN + payload.len() + pad_len(payload.len() as u32))
                        as u64,
                );
            }
            let reported = error.byte_offset().expect("an offset is reported");
            assert!(
                boundaries.contains(&reported),
                "reported offset {reported} is not a chunk boundary; boundaries are {boundaries:?}"
            );
            assert!(
                reported > 12,
                "the first chunk is never the out-of-order one, so 12 would mean the walk \
                 never advanced"
            );
        }
    }

    #[test]
    fn layout_lets_metadata_and_unknown_chunks_appear_out_of_order() {
        // The same paragraph exempts them: "Metadata and unknown chunks MAY appear out of order."
        // Here EXIF and an unknown chunk sit *before* the bitstream, which must still parse.
        let odd = FourCc::from(*b"XYZW");
        let file = raw_file(&[
            (FourCc::VP8X, &vp8x(false)),
            (FourCc::EXIF, b"exif"),
            (odd, b"private"),
            (FourCc::VP8L, &[0x2f]),
            (FourCc::XMP, b"<x/>"),
        ]);
        let layout = WebpLayout::parse(&file).expect("metadata may float");
        assert_eq!(
            layout.metadata.exif,
            Some(&b"exif"[..]),
            "EXIF before the bitstream is unusual but explicitly allowed"
        );
        assert_eq!(layout.metadata.xmp, Some(&b"<x/>"[..]));
        assert_eq!(layout.unknown.len(), 1);
        assert_eq!(layout.unknown[0].fourcc, odd);

        // `ICCP`, by contrast, is *not* exempt: §2.7 lists it among the ordered chunks and §2.7.1.4
        // adds "this chunk MUST appear before the image data".
        let late_icc = raw_file(&[
            (FourCc::VP8X, &vp8x(false)),
            (FourCc::VP8L, &[0x2f]),
            (FourCc::ICCP, b"icc"),
        ]);
        assert!(
            WebpLayout::parse(&late_icc).is_err(),
            "a colour profile after the image data is an ordering violation"
        );
    }

    #[test]
    fn layout_keeps_unknown_chunks_in_their_original_order() {
        // §2.7.1.6: "Writers SHOULD preserve them in their original order." Preserving order on
        // read is the half that makes that possible.
        let (a, b, c) = (
            FourCc::from(*b"AAAA"),
            FourCc::from(*b"BBBB"),
            FourCc::from(*b"CCCC"),
        );
        let file = raw_file(&[
            (FourCc::VP8X, &vp8x(false)),
            (FourCc::VP8L, &[0x2f]),
            (c, b"third"),
            (a, b"first"),
            (b, b"second"),
        ]);
        let layout = WebpLayout::parse(&file).unwrap();
        assert_eq!(
            layout
                .unknown
                .iter()
                .map(|k| (k.fourcc, k.payload))
                .collect::<Vec<_>>(),
            vec![(c, &b"third"[..]), (a, &b"first"[..]), (b, &b"second"[..])],
        );
    }

    #[test]
    fn unknown_chunks_survive_a_read_write_cycle() {
        let odd = FourCc::from(*b"XYZW");
        let original = raw_file(&[
            (FourCc::VP8X, &vp8x(false)),
            (FourCc::ICCP, b"icc"),
            (FourCc::VP8L, &[0x2f]),
            (odd, b"private payload"),
        ]);
        let layout = WebpLayout::parse(&original).unwrap();

        let header = Vp8xHeader {
            canvas_width: 16,
            canvas_height: 16,
            ..Default::default()
        };
        let rewritten = write_extended_preserving(
            &header,
            &layout.metadata,
            &[chunk(FourCc::VP8L, layout.bitstream.unwrap().1)],
            &layout.unknown,
        )
        .unwrap();

        let round_tripped = WebpLayout::parse(&rewritten).unwrap();
        assert_eq!(round_tripped.unknown.len(), 1);
        assert_eq!(round_tripped.unknown[0].fourcc, odd);
        assert_eq!(round_tripped.unknown[0].payload, b"private payload");
        assert_eq!(round_tripped.metadata.icc, Some(&b"icc"[..]));

        // Without the preserving writer the chunk is dropped — the behaviour this closes.
        let dropped = write_extended_with_metadata(
            &header,
            &layout.metadata,
            &[chunk(FourCc::VP8L, layout.bitstream.unwrap().1)],
        )
        .unwrap();
        assert!(WebpLayout::parse(&dropped).unwrap().unknown.is_empty());
    }

    #[test]
    fn layout_keeps_the_first_of_each_repeatable_chunk() {
        let file = raw_file(&[
            (FourCc::VP8X, &vp8x(true)),
            (FourCc::ICCP, b"first-icc"),
            (FourCc::ICCP, b"second-icc"),
            (FourCc::ALPH, b"first-alph"),
            (FourCc::ALPH, b"second-alph"),
            (FourCc::VP8, b"first-vp8"),
            (FourCc::VP8, b"second-vp8"),
        ]);
        let layout = WebpLayout::parse(&file).unwrap();
        assert_eq!(layout.metadata.icc, Some(&b"first-icc"[..]));
        assert_eq!(layout.alph, Some(&b"first-alph"[..]));
        assert_eq!(
            layout.bitstream,
            Some((WebpChunkId::Vp8, &b"first-vp8"[..]))
        );
    }

    #[test]
    fn layout_reports_animation_as_out_of_scope() {
        for fourcc in [FourCc::ANIM, FourCc::ANMF] {
            let file = raw_file(&[(FourCc::VP8X, &vp8x(false)), (fourcc, &[0; 6])]);
            let error = WebpLayout::parse(&file).expect_err("animation is out of scope");
            assert_eq!(
                error.kind(),
                ErrorKind::Unsupported,
                "an animated file is unsupported, not malformed"
            );
        }
    }

    #[test]
    fn layout_surfaces_trailing_bytes() {
        let mut file = write_simple_lossless(&[0x2f, 1, 2]).unwrap();
        file.extend_from_slice(b"motion photo stream");
        assert_eq!(WebpLayout::parse(&file).unwrap().trailing_bytes, 19);
    }

    /// The store is *not* an unknown chunk to this crate: `WebpLayout::parse` routes it to
    /// `MetadataChunks::c2pa`, so `write_extended_preserving` cannot emit it twice — once out of
    /// `unknown` and once out of `metadata` — when a file is read, modified and written back.
    #[test]
    fn a_store_read_back_is_metadata_and_survives_a_rewrite_exactly_once() {
        let original = raw_file(&[
            (FourCc::VP8X, &vp8x(false)),
            (FourCc::VP8L, &[0x2f]),
            (FourCc::C2PA, b"store"),
        ]);
        let layout = WebpLayout::parse(&original).unwrap();
        assert_eq!(layout.metadata.c2pa, Some(&b"store"[..]));
        assert!(layout.unknown.is_empty(), "a store is not an unknown chunk");

        let rewritten = write_extended_preserving(
            &layout.vp8x.unwrap(),
            &layout.metadata,
            &[chunk(FourCc::VP8L, layout.bitstream.unwrap().1)],
            &layout.unknown,
        )
        .unwrap();
        assert_eq!(rewritten, original);
    }

    /// §A.3.7 constrains a *writer*. A reader must not turn §2.7's ordering rule against a store it
    /// finds early, since §2.7 does not list `C2PA` among the reconstruction chunks.
    #[test]
    fn a_store_ahead_of_the_bitstream_still_parses() {
        let file = raw_file(&[
            (FourCc::VP8X, &vp8x(false)),
            (FourCc::C2PA, b"store"),
            (FourCc::VP8L, &[0x2f]),
        ]);
        let layout = WebpLayout::parse(&file).unwrap();
        assert_eq!(layout.metadata.c2pa, Some(&b"store"[..]));
        // 12-byte header + the 18-byte VP8X chunk, then 8 + 5 bytes of store chunk.
        assert_eq!(span_in(&file), Some(30..43));
    }

    /// The store joins the "first of each kind wins" policy the crate applies everywhere, and the
    /// span reported is the same chunk the payload came from.
    #[test]
    fn metadata_chunks_read_keeps_the_first_store() {
        let mut w = RiffWriter::new();
        w.write_chunk(FourCc::VP8L, &[0x2f]).unwrap();
        w.write_chunk(FourCc::C2PA, b"first").unwrap();
        w.write_chunk(FourCc::C2PA, b"second").unwrap();
        let file = w.finish().unwrap();
        assert_eq!(
            MetadataChunks::read(&file).unwrap().c2pa,
            Some(&b"first"[..])
        );
        let span = span_in(&file).expect("the store was embedded");
        assert_eq!(&file[span.start + 8..span.end], b"first");
    }
}
