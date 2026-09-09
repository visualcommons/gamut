//! WebP-specific helpers over the generic RIFF layer: classifying WebP chunks, the [`Vp8xHeader`]
//! extended-format feature header, the [`MetadataChunks`] passthrough for
//! `ICCP`/`EXIF`/`XMP `/`C2PA`, and writing the simple (single-bitstream) and extended file formats
//! (RFC 9649 §2.5-§2.7).
//!
//! The remaining extended-format chunks (`ANIM`/`ANMF`) are tracked in `gamut-webp/STATUS.md`
//! section A and are out of scope under the image-first charter.
//!
//! The `C2PA` chunk is the one carrier here that RFC 9649 does not define: it is C2PA 2.4 §A.3.7's,
//! and to the WebP container it is an unknown chunk readers ignore (§2.7.1.6). It is modelled
//! anyway — [`C2PA_FOURCC`], [`WebpChunkId::C2pa`], [`MetadataChunks::c2pa`] and [`c2pa_span`] —
//! because §A.3.7 constrains *where* it goes, which the generic unknown-chunk passthrough cannot
//! express.

use core::ops::Range;

use gamut_core::{Error, Result};

use crate::chunk::{CHUNK_HEADER_LEN, Chunk, pad_len};
use crate::fourcc::FourCc;
use crate::reader::RiffReader;
use crate::writer::RiffWriter;

/// The number of bytes in a `VP8X` chunk payload (RFC 9649 §2.7).
pub const VP8X_PAYLOAD_LEN: usize = 10;

/// The largest canvas dimension a `VP8X` header can express: the width and height are stored
/// 1-based in 24 bits, so `1..=2^24` (RFC 9649 §2.7).
pub const MAX_CANVAS_DIMENSION: u32 = 1 << 24;

/// The FourCC of the chunk that carries a C2PA manifest store: `C2PA` (C2PA 2.4 §A.3.7).
///
/// Deliberately not one of [`FourCc`]'s associated constants, which are RFC 9649's chunk
/// vocabulary. This identifier belongs to the C2PA specification instead, so it is named beside the
/// code that places it — and the WebP container itself only ever sees an unknown chunk (§2.7.1.6).
pub const C2PA_FOURCC: FourCc = FourCc(*b"C2PA");

/// Byte length of a RIFF/WebP file header: the `RIFF` magic, the `uint32` file size, and the `WEBP`
/// form type (RFC 9649 §2.4). Chunks begin here.
const RIFF_HEADER_LEN: usize = 12;

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
    /// Returns [`Error::InvalidInput`] if the canvas is one the format cannot express: either
    /// dimension outside `1..=`[`MAX_CANVAS_DIMENSION`], or a width × height product above
    /// `2^32 - 1`, both of which §2.7 forbids. Validating here rather than truncating means a
    /// header that encodes always decodes back to the same canvas.
    pub fn to_payload(&self) -> Result<[u8; VP8X_PAYLOAD_LEN]> {
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

    /// Parses a `VP8X` chunk payload, mirroring [`to_payload`](Self::to_payload). The two reserved
    /// fields are ignored as the spec requires.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidInput`] if `payload` is shorter than [`VP8X_PAYLOAD_LEN`], or if the
    /// canvas it declares has a width × height product above `2^32 - 1`, which §2.7 forbids. (The
    /// dimensions themselves need no check: 24 bits stored 1-based can only land in `1..=2^24`.)
    pub fn from_payload(payload: &[u8]) -> Result<Self> {
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
/// Returns [`Error::InvalidInput`] if `header` declares a canvas the format cannot express (see
/// [`Vp8xHeader::to_payload`]), or [`Error::Unsupported`] if a payload or the finished file exceeds
/// the RIFF size fields.
pub fn write_extended(header: &Vp8xHeader, chunks: &[(FourCc, &[u8])]) -> Result<Vec<u8>> {
    let mut w = RiffWriter::new();
    w.write_chunk(FourCc::VP8X, &header.to_payload()?)?;
    for (fourcc, payload) in chunks {
        w.write_chunk(*fourcc, payload)?;
    }
    w.finish()
}

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
    /// Returns [`Error::InvalidInput`] if `data` is not a valid RIFF/WebP file, or if a chunk's
    /// declared size runs past the end of the data.
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

/// Locates the `C2PA` chunk of the WebP file in `data`, returning the byte range its **whole**
/// on-disk span occupies: the four identifier bytes, the four-byte size field, and the payload.
///
/// That whole span is the range a `c2pa.hash.data` assertion excludes (C2PA 2.4 §18.5), not just
/// the payload: an update manifest may resize the manifest store, and resizing it changes the value
/// of the size field as surely as it changes the bytes after it, so a hash that covered the size
/// field could not survive the update the exclusion exists to permit.
///
/// The pad byte RIFF appends after an odd-length payload (RFC 9649 §2.3) is **outside** the range.
/// It is framing the container adds around the chunk, not part of the chunk's data — the same
/// reason [`Chunk::payload`] excludes it. A caller who needs the padded span can add
/// `range.len() % 2`: the 8-byte chunk header is even, so the span's parity is the store's.
///
/// The **first** `C2PA` chunk wins, as it does in [`MetadataChunks::read`]; a conformant file has at
/// most one, placed last (§A.3.7), which is where [`write_extended_preserving`] puts it.
///
/// # Errors
///
/// Returns [`Error::InvalidInput`] if `data` is not a valid RIFF/WebP file, or if a chunk's declared
/// size runs past the end of the data.
///
/// # Example
///
/// ```
/// use gamut_riff::{MetadataChunks, Vp8xHeader, c2pa_span, write_extended_with_metadata};
///
/// let store = b"a C2PA manifest store";
/// let header = Vp8xHeader { canvas_width: 1, canvas_height: 1, ..Default::default() };
/// let metadata = MetadataChunks { c2pa: Some(store), ..Default::default() };
/// let file = write_extended_with_metadata(&header, &metadata, &[])?;
///
/// let span = c2pa_span(&file)?.expect("the store was embedded");
/// assert_eq!(&file[span.start..span.start + 4], b"C2PA");
/// assert_eq!(span.len(), 8 + store.len());
/// # Ok::<(), gamut_core::Error>(())
/// ```
pub fn c2pa_span(data: &[u8]) -> Result<Option<Range<usize>>> {
    let mut offset = RIFF_HEADER_LEN;
    for chunk in RiffReader::new(data)? {
        let chunk = chunk?;
        let span = CHUNK_HEADER_LEN + chunk.payload.len();
        if WebpChunkId::from(chunk.fourcc) == WebpChunkId::C2pa {
            return Ok(Some(offset..offset + span));
        }
        // Skip the pad byte too. The reader framed this payload from a `uint32` size field, so the
        // cast is exact.
        offset += span + pad_len(chunk.payload.len() as u32);
    }
    Ok(None)
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
    image_data: &[(FourCc, &[u8])],
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
    image_data: &[(FourCc, &[u8])],
    unknown: &[Chunk<'_>],
) -> Result<Vec<u8>> {
    let header = Vp8xHeader {
        icc_profile: metadata.icc.is_some(),
        exif_metadata: metadata.exif.is_some(),
        xmp_metadata: metadata.xmp.is_some(),
        ..*header
    };
    let mut chunks: Vec<(FourCc, &[u8])> = Vec::with_capacity(image_data.len() + 4 + unknown.len());
    if let Some(icc) = metadata.icc {
        chunks.push((FourCc::ICCP, icc));
    }
    chunks.extend_from_slice(image_data);
    if let Some(exif) = metadata.exif {
        chunks.push((FourCc::EXIF, exif));
    }
    if let Some(xmp) = metadata.xmp {
        chunks.push((FourCc::XMP, xmp));
    }
    // A `C2PA` chunk never goes out among the unknown chunks: either the configured store owns that
    // slot, or — with none configured — this one *is* the store and belongs in the store's place.
    // First wins, as everywhere else here.
    let carried = unknown.iter().find(|c| c.fourcc == C2PA_FOURCC);
    chunks.extend(
        unknown
            .iter()
            .filter(|c| c.fourcc != C2PA_FOURCC)
            .map(|c| (c.fourcc, c.payload)),
    );
    // C2PA 2.4 §A.3.7: the manifest store's chunk is the *last* sub-chunk of the RIFF/WEBP form —
    // after the preserved unknown chunks, not merely somewhere past the image data.
    if let Some(c2pa) = metadata.c2pa.or(carried.map(|c| c.payload)) {
        chunks.push((C2PA_FOURCC, c2pa));
    }
    write_extended(&header, &chunks)
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
    /// Returns [`Error::InvalidInput`] if `data` is not a valid RIFF/WebP file, if a chunk runs
    /// past the end of the data, or if a reconstruction chunk appears out of order (the error
    /// carries the offending chunk's byte offset). Returns [`Error::Unsupported`] for an animated
    /// file — an `ANIM` or `ANMF` chunk — which is outside this crate's still-image scope.
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
                        layout.vp8x = Some(Vp8xHeader::from_payload(chunk.payload)?);
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

/// Wraps a VP8L lossless bitstream in the simple WebP (lossless) file format: a `RIFF`/`WEBP` header
/// and a single `VP8L` chunk (RFC 9649 §2.6).
///
/// # Errors
///
/// Returns [`Error::Unsupported`] if the bitstream or the finished file exceeds the RIFF size
/// fields (§2.3, §2.4).
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
/// Returns [`Error::Unsupported`] if the bitstream or the finished file exceeds the RIFF size
/// fields (§2.3, §2.4).
pub fn write_simple_lossy(vp8_bitstream: &[u8]) -> Result<Vec<u8>> {
    let mut w = RiffWriter::new();
    w.write_chunk(FourCc::VP8, vp8_bitstream)?;
    w.finish()
}

#[cfg(test)]
mod tests {
    use gamut_core::ErrorKind;

    use super::*;

    #[test]
    fn classifies_known_and_unknown_chunks() {
        assert_eq!(WebpChunkId::from(FourCc::VP8), WebpChunkId::Vp8);
        assert_eq!(WebpChunkId::from(FourCc::VP8L), WebpChunkId::Vp8l);
        assert_eq!(WebpChunkId::from(FourCc::VP8X), WebpChunkId::Vp8x);
        assert_eq!(WebpChunkId::from(FourCc::ALPH), WebpChunkId::Alpha);
        assert_eq!(WebpChunkId::from(FourCc::ICCP), WebpChunkId::Iccp);
        assert_eq!(WebpChunkId::from(FourCc::EXIF), WebpChunkId::Exif);
        assert_eq!(WebpChunkId::from(FourCc::XMP), WebpChunkId::Xmp);
        assert_eq!(WebpChunkId::from(C2PA_FOURCC), WebpChunkId::C2pa);
        assert_eq!(WebpChunkId::from(FourCc::ANIM), WebpChunkId::Anim);
        assert_eq!(WebpChunkId::from(FourCc::ANMF), WebpChunkId::Anmf);
        let weird = FourCc::from(*b"XYZW");
        assert_eq!(WebpChunkId::from(weird), WebpChunkId::Unknown(weird));
    }

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
        let payload = h.to_payload().unwrap();
        assert_eq!(payload.len(), VP8X_PAYLOAD_LEN);
        assert_eq!(payload[0] & 0x10, 0x10, "alpha (L) flag is bit 4");
        assert_eq!(&payload[1..4], &[0, 0, 0], "reserved bytes are zero");
        assert_eq!(Vp8xHeader::from_payload(&payload).unwrap(), h);
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
        let p = wide.to_payload().unwrap();
        // flags = icc(0x20) | alpha(0x10) | exif(0x08) | xmp(0x04) | anim(0x02).
        assert_eq!(p[0], 0x3E);
        // 24-bit little-endian width-1 then height-1.
        assert_eq!(&p[4..7], &[0x56, 0x34, 0x12]);
        assert_eq!(&p[7..10], &[0x01, 0x00, 0x00]);
        assert_eq!(Vp8xHeader::from_payload(&p).unwrap(), wide);

        let tall = Vp8xHeader {
            canvas_width: 2,
            canvas_height: 0x65_4321 + 1,
            ..wide
        };
        let p = tall.to_payload().unwrap();
        assert_eq!(&p[4..7], &[0x01, 0x00, 0x00]);
        assert_eq!(&p[7..10], &[0x21, 0x43, 0x65]);
        assert_eq!(Vp8xHeader::from_payload(&p).unwrap(), tall);
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
            exact.to_payload().is_ok(),
            "a product of exactly 2^32 - 1 is at most 2^32 - 1"
        );

        // And one more pixel over is refused, so the pair brackets the boundary.
        let over = Vp8xHeader {
            canvas_width: 65_538,
            canvas_height: 65_535,
            ..Default::default()
        };
        let err = over
            .to_payload()
            .expect_err("one past the bound is refused");
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
        assert!(ok.to_payload().is_ok(), "2^24 x 1 is the largest width");

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
            let error = bad.to_payload().expect_err("outside 1..=2^24");
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
        assert!(over.to_payload().is_err(), "65536^2 is 2^32, one too many");
        let under = Vp8xHeader {
            canvas_height: 65535,
            ..over
        };
        assert!(under.to_payload().is_ok(), "65536 x 65535 fits");
    }

    #[test]
    fn from_payload_rejects_a_canvas_whose_product_overflows() {
        // A hostile file can declare a canvas no encoder would produce: 2^24 x 2^24 = 2^48 pixels.
        // Rejecting it on read matches libwebp, which caps the decoded canvas area the same way.
        let mut payload = [0u8; VP8X_PAYLOAD_LEN];
        payload[4..7].copy_from_slice(&[0xFF, 0xFF, 0xFF]);
        payload[7..10].copy_from_slice(&[0xFF, 0xFF, 0xFF]);
        let error = Vp8xHeader::from_payload(&payload).expect_err("2^48 pixels");
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
        let p = h.to_payload().unwrap();
        assert_eq!(p[0], 0x00);
        assert_eq!(Vp8xHeader::from_payload(&p).unwrap(), h);
    }

    #[test]
    fn from_payload_rejects_short_input() {
        assert!(Vp8xHeader::from_payload(&[0u8; 9]).is_err());
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
                (FourCc::ALPH, &[1, 2, 3]),
                (FourCc::VP8, &[0x9d, 0x01, 0x2a]),
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
        assert_eq!(Vp8xHeader::from_payload(chunks[0].payload).unwrap(), h);
    }

    /// The chunk FourCCs of `file`, in file order.
    fn chunk_ids(file: &[u8]) -> Vec<FourCc> {
        RiffReader::new(file)
            .unwrap()
            .map(|c| c.unwrap().fourcc)
            .collect()
    }

    /// The parsed `VP8X` feature header of an extended `file` (its first chunk).
    fn vp8x_header_of(file: &[u8]) -> Vp8xHeader {
        let first = RiffReader::new(file).unwrap().next().unwrap().unwrap();
        assert_eq!(
            first.fourcc,
            FourCc::VP8X,
            "an extended file opens with VP8X"
        );
        Vp8xHeader::from_payload(first.payload).unwrap()
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
            &[(FourCc::ALPH, &[9, 9]), (FourCc::VP8, &[0x9d, 0x01, 0x2a])],
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
        let image: &[(FourCc, &[u8])] = &[(FourCc::VP8L, &[0x2f])];

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
        let image: &[(FourCc, &[u8])] = &[(FourCc::VP8L, &[0x2f])];
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
        let image: &[(FourCc, &[u8])] = &[(FourCc::ALPH, &[1]), (FourCc::VP8, &[2])];
        assert_eq!(
            write_extended_with_metadata(&header, &empty, image).unwrap(),
            write_extended(&header, image).unwrap()
        );
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
        w.write_chunk(FourCc::VP8X, &lying.to_payload().unwrap())
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

    /// Assembles a file from raw chunks, bypassing the ordering the writers impose — the only way
    /// to build the malformed inputs `WebpLayout::parse` must reject.
    fn raw_file(chunks: &[(FourCc, &[u8])]) -> Vec<u8> {
        let mut w = RiffWriter::new();
        for (fourcc, payload) in chunks {
            w.write_chunk(*fourcc, payload).unwrap();
        }
        w.finish().unwrap()
    }

    /// A `VP8X` payload for a small canvas with the given feature flags left at their defaults.
    fn vp8x(alpha: bool) -> [u8; VP8X_PAYLOAD_LEN] {
        Vp8xHeader {
            alpha,
            canvas_width: 16,
            canvas_height: 16,
            ..Default::default()
        }
        .to_payload()
        .unwrap()
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
            &[(FourCc::VP8L, layout.bitstream.unwrap().1)],
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
            &[(FourCc::VP8L, layout.bitstream.unwrap().1)],
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
        let file =
            write_extended_preserving(&header, &metadata, &[(FourCc::VP8L, &[0x2f])], &unknown)
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
                C2PA_FOURCC,
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
        let image: &[(FourCc, &[u8])] = &[(FourCc::VP8L, &[0x2f])];
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

    /// `c2pa_span` reports the chunk's whole span — identifier, size field and payload — because
    /// that is what a `c2pa.hash.data` exclusion covers (C2PA 2.4 §18.5). The RIFF pad byte after an
    /// odd-length store (§2.3) is framing, so it stays outside both the span and the payload.
    #[test]
    fn c2pa_span_covers_the_whole_chunk_but_not_the_pad_byte() {
        let store = b"odd length store!"; // 17 bytes -> one pad byte
        assert_eq!(store.len() % 2, 1, "the fixture must exercise the pad byte");
        let file = write_extended_with_metadata(
            &Vp8xHeader {
                canvas_width: 4,
                canvas_height: 4,
                ..Default::default()
            },
            &MetadataChunks {
                c2pa: Some(store),
                ..Default::default()
            },
            &[(FourCc::VP8L, &[0x2f])],
        )
        .unwrap();

        let span = c2pa_span(&file).unwrap().expect("the store was embedded");
        assert_eq!(&file[span.start..span.start + 4], b"C2PA", "identifier");
        assert_eq!(
            &file[span.start + 4..span.start + 8],
            &(store.len() as u32).to_le_bytes(),
            "size field"
        );
        assert_eq!(&file[span.start + 8..span.end], store, "payload");
        assert_eq!(span.end, file.len() - 1, "the pad byte is outside the span");
        assert_eq!(file[file.len() - 1], 0, "§2.3: the pad byte is zero");
    }

    /// The span is an offset into the whole file, so the pad byte of every *preceding* odd-length
    /// chunk has to be counted as well.
    #[test]
    fn c2pa_span_counts_the_padding_of_the_chunks_before_it() {
        let mut w = RiffWriter::new();
        w.write_chunk(FourCc::VP8L, &[0x2f, 0x00, 0x00]).unwrap(); // odd -> padded
        w.write_chunk(FourCc::EXIF, b"odd").unwrap(); // odd -> padded
        w.write_chunk(C2PA_FOURCC, b"store").unwrap();
        let file = w.finish().unwrap();
        let span = c2pa_span(&file).unwrap().expect("the store was embedded");
        assert_eq!(&file[span.start..span.start + 4], b"C2PA");
        // 12-byte file header, then two chunks of 8 header + 3 payload + 1 pad bytes each; the
        // store's own chunk is its 8-byte header plus its five payload bytes.
        let start = 12 + (8 + 3 + 1) + (8 + 3 + 1);
        assert_eq!(span, start..start + 8 + 5);
    }

    /// A file with no store has no span to report, rather than an empty range at some offset a
    /// caller might hash around.
    #[test]
    fn c2pa_span_is_none_without_a_store() {
        let file = write_simple_lossless(&[0x2f]).unwrap();
        assert_eq!(c2pa_span(&file).unwrap(), None);
    }

    /// The store is *not* an unknown chunk to this crate: `WebpLayout::parse` routes it to
    /// `MetadataChunks::c2pa`, so `write_extended_preserving` cannot emit it twice — once out of
    /// `unknown` and once out of `metadata` — when a file is read, modified and written back.
    #[test]
    fn a_store_read_back_is_metadata_and_survives_a_rewrite_exactly_once() {
        let original = raw_file(&[
            (FourCc::VP8X, &vp8x(false)),
            (FourCc::VP8L, &[0x2f]),
            (C2PA_FOURCC, b"store"),
        ]);
        let layout = WebpLayout::parse(&original).unwrap();
        assert_eq!(layout.metadata.c2pa, Some(&b"store"[..]));
        assert!(layout.unknown.is_empty(), "a store is not an unknown chunk");

        let rewritten = write_extended_preserving(
            &layout.vp8x.unwrap(),
            &layout.metadata,
            &[(FourCc::VP8L, layout.bitstream.unwrap().1)],
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
            (C2PA_FOURCC, b"store"),
            (FourCc::VP8L, &[0x2f]),
        ]);
        let layout = WebpLayout::parse(&file).unwrap();
        assert_eq!(layout.metadata.c2pa, Some(&b"store"[..]));
        // 12-byte header + the 18-byte VP8X chunk, then 8 + 5 bytes of store chunk.
        assert_eq!(c2pa_span(&file).unwrap(), Some(30..43));
    }

    /// The store joins the "first of each kind wins" policy the crate applies everywhere, and the
    /// span reported is the same chunk the payload came from.
    #[test]
    fn metadata_chunks_read_keeps_the_first_store() {
        let mut w = RiffWriter::new();
        w.write_chunk(FourCc::VP8L, &[0x2f]).unwrap();
        w.write_chunk(C2PA_FOURCC, b"first").unwrap();
        w.write_chunk(C2PA_FOURCC, b"second").unwrap();
        let file = w.finish().unwrap();
        assert_eq!(
            MetadataChunks::read(&file).unwrap().c2pa,
            Some(&b"first"[..])
        );
        let span = c2pa_span(&file).unwrap().expect("the store was embedded");
        assert_eq!(&file[span.start + 8..span.end], b"first");
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
            &[(FourCc::VP8L, &[0x2f])],
            &[
                Chunk {
                    fourcc: C2PA_FOURCC,
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
                C2PA_FOURCC,
            ],
            "one store chunk, last, and the genuinely unknown chunk survives"
        );
        assert_eq!(
            MetadataChunks::read(&file).unwrap().c2pa,
            Some(&b"FRESH"[..])
        );
        let span = c2pa_span(&file).unwrap().expect("the store was embedded");
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
            &[(FourCc::VP8L, &[0x2f])],
            &[
                Chunk {
                    fourcc: C2PA_FOURCC,
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
                C2PA_FOURCC,
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
        let file = write_extended(&header, &[(C2PA_FOURCC, b"store")]).unwrap();
        assert_eq!(chunk_ids(&file), vec![FourCc::VP8X, C2PA_FOURCC]);
    }

    /// `c2pa_span` walks the same framing every reader here does, so it rejects a file whose framing
    /// is bad rather than reporting a range into it — the `# Errors` contract its docs state.
    #[test]
    fn c2pa_span_rejects_a_malformed_file() {
        assert_eq!(
            c2pa_span(b"not a RIFF file at all").unwrap_err().kind(),
            ErrorKind::InvalidInput
        );

        // Well-formed header, then a chunk whose declared size runs past the end of the data.
        let mut file = write_simple_lossless(&[0x2f, 0x00]).unwrap();
        let len = file.len();
        file[16..20].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(file.len(), len, "only the size field was edited");
        assert_eq!(
            c2pa_span(&file).unwrap_err().kind(),
            ErrorKind::InvalidInput
        );
    }
}
