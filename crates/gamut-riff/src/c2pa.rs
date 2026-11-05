//! Locating the `C2PA` manifest store of a WebP file: [`c2pa_span`] for the first store's byte
//! range and [`c2pa_summary`] for a non-validating report of every `C2PA` chunk.
//!
//! The `C2PA` chunk is the one carrier here that RFC 9649 does not define: it is C2PA 2.4 §A.3.7's,
//! and to the WebP container it is an unknown chunk readers ignore (§2.7.1.6). It is modelled
//! anyway — [`FourCc::C2PA`], [`WebpChunkId::C2pa`], [`MetadataChunks::c2pa`] and this module —
//! because §A.3.7 constrains *where* it goes, which the generic unknown-chunk passthrough cannot
//! express.

use core::ops::Range;

use gamut_core::Result;
// The length below which a `C2PA` chunk's payload cannot be a manifest store. The bound `gamut-ifd`
// applies to a tag-52545 value and `gamut-heic` to a store's `LBox`, applied here to the chunk
// payload §A.3.7 makes the store.
use gamut_core::c2pa::MIN_STORE_LEN;

use crate::chunk::{CHUNK_HEADER_LEN, Chunk, RIFF_HEADER_LEN, pad_len};
#[cfg(doc)]
use crate::fourcc::FourCc;
#[cfg(doc)]
use crate::layout::MetadataChunks;
use crate::layout::WebpChunkId;
use crate::reader::RiffReader;
#[cfg(doc)]
use crate::vp8x::write_extended_preserving;

/// Locates the `C2PA` chunk of the WebP file in `data`, returning the byte range its **whole**
/// on-disk span occupies: the four identifier bytes, the four-byte size field, and the payload.
///
/// The range is in `u64` file offsets — the unit of a file position, as `gamut-ifd` reports its
/// C2PA exclusion ranges and [`gamut_core::Error::byte_offset`] its offsets — rather than in the
/// `usize` of the slice it was read from.
///
/// The whole span is reported, rather than the payload alone, because that is the exclusion range
/// issue #445 specified for a `c2pa.hash.data` assertion. Two limits of that choice are stated here
/// rather than implied:
///
/// - **It does not let a hash survive a resize.** Resizing the store also changes the RIFF
///   *File Size* field at bytes `4..8` (RFC 9649 §2.4), which lies outside this range and is
///   therefore hashed. Only a replacement of the **same length** leaves every hashed byte unchanged.
/// - **It overlaps the chunk's header, which C2PA 2.4 §18.5.1 forbids as written.** §18.5.1 says a
///   data-hash exclusion range "shall not overlap with any header or length field associated with
///   that unit, except for freebox or pad data", and makes no exception for RIFF. A signer that
///   needs the §18.5.1 reading excludes the payload alone, `range.start + 8..range.end`. Which of
///   the two ranges this function should report is open, pending a maintainer decision.
///
/// The pad byte RIFF appends after an odd-length payload (RFC 9649 §2.3) is **outside** the range.
/// It is framing the container adds around the chunk, not part of the chunk's data — the same
/// reason [`Chunk::payload`] excludes it. A caller who needs the padded span can add
/// `(range.end - range.start) % 2`: the 8-byte chunk header is even, so the span's parity is the
/// store's.
///
/// The **first** `C2PA` chunk wins, as it does in [`MetadataChunks::read`]; a conformant file has at
/// most one, placed last (§A.3.7), which is where [`write_extended_preserving`] puts it.
///
/// # Errors
///
/// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if `data` is
/// not a valid RIFF/WebP file, or if a chunk's declared size runs past the end of the data.
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
/// let start = usize::try_from(span.start).expect("an offset into a slice fits in usize");
/// assert_eq!(&file[start..start + 4], b"C2PA");
/// assert_eq!(span.end - span.start, 8 + store.len() as u64);
/// # Ok::<(), gamut_core::Error>(())
/// ```
pub fn c2pa_span(data: &[u8]) -> Result<Option<Range<u64>>> {
    for located in located_chunks(data)? {
        let (span, chunk) = located?;
        if WebpChunkId::from(chunk.fourcc) == WebpChunkId::C2pa {
            // Offsets into an in-memory slice; `usize` never exceeds 64 bits on a supported target.
            return Ok(Some(span.start as u64..span.end as u64));
        }
    }
    Ok(None)
}

/// The chunks of the WebP file in `data`, each with the byte range its whole on-disk span occupies
/// — identifier, size field and payload, without the pad byte — in file order. The one offset walk
/// behind [`c2pa_span`] and [`c2pa_summary`], so the two cannot disagree about where a chunk sits.
///
/// # Errors
///
/// As [`RiffReader::new`] up front, and per item as its iterator.
fn located_chunks(data: &[u8]) -> Result<impl Iterator<Item = Result<(Range<usize>, Chunk<'_>)>>> {
    let mut offset = RIFF_HEADER_LEN;
    Ok(RiffReader::new(data)?.map(move |chunk| {
        let chunk = chunk?;
        let span = offset..offset + CHUNK_HEADER_LEN + chunk.payload.len();
        // Skip the pad byte too. The reader framed this payload from a `uint32` size field, so the
        // cast is exact.
        offset = span.end + pad_len(chunk.payload.len() as u32);
        Ok((span, chunk))
    }))
}

// `C2paSummary::summary_lines` carries this text in the line that reports the stores, never as a
// footnote.
pub use gamut_core::c2pa::NOT_VALIDATED as C2PA_NOT_VALIDATED;

/// Where a `C2PA` chunk sits among the file's chunks — a **positional fact about bytes**, never a
/// verdict.
///
/// C2PA 2.4 §A.3.7 requires the chunk to "appear as the last sub-chunk of the first RIFF header
/// chunk". A chunk anywhere else is still reported, with its true range; this says where it is so a
/// reader deciding what to hand a validator can see it.
///
/// Non-exhaustive and with permanent discriminants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
#[repr(u8)]
pub enum C2paChunkPosition {
    /// The last chunk of the `RIFF`/`WEBP` form, where §A.3.7 puts it.
    Last = 0,
    /// Another chunk follows it.
    BeforeLaterChunks = 1,
}

impl C2paChunkPosition {
    /// The clause a report adds to this chunk's line, or `None` when it sits where §A.3.7 puts it.
    #[must_use]
    pub const fn note(self) -> Option<&'static str> {
        match self {
            Self::BeforeLaterChunks => {
                Some("another chunk follows it, where C2PA 2.4 §A.3.7 places it last")
            }
            Self::Last => None,
        }
    }
}

/// One `C2PA` chunk's manifest store, reported **without its bytes**: where the store sits, how big
/// it is, and where its chunk sits among the file's chunks.
///
/// The bytes are absent by construction: they are opaque to this crate, and a byte range is what a
/// caller hands to `c2pa-rs` or to `dd`. Use [`MetadataChunks::c2pa`] when the bytes are wanted.
///
/// Non-exhaustive: a later revision may report more without a breaking change.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct C2paStoreSummary {
    /// The half-open byte range of the store — the chunk's payload, without its 8-byte header or
    /// the RIFF pad byte. [`c2pa_span`] reports the first chunk's whole span instead, header
    /// included; the store is `span.start + 8..span.end`.
    pub range: Range<usize>,
    /// Where the chunk sits among the file's chunks.
    pub position: C2paChunkPosition,
}

impl C2paStoreSummary {
    /// The store's size in bytes, in the `u64` of a file length as [`c2pa_span`] reports offsets.
    #[must_use]
    pub fn size(&self) -> u64 {
        // A length within an in-memory slice; `usize` never exceeds 64 bits on a supported target.
        self.range.len() as u64
    }
}

/// A `C2PA` chunk from which no manifest store could be read, reported **without its bytes**: its
/// payload is shorter than the 8-byte JUMBF box header every store opens with, so it cannot be a
/// store — but the chunk is present, and a report that dropped it would read as absence of
/// provenance. The `gamut-ifd` and `gamut-heic` summaries list such a value as unread for the same
/// reason.
///
/// Non-exhaustive: a later revision may report more without a breaking change.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct C2paUnreadChunk {
    /// The half-open byte range of the chunk's payload, without its 8-byte header or the RIFF pad
    /// byte — the bytes a store would occupy.
    pub range: Range<usize>,
    /// Where the chunk sits among the file's chunks.
    pub position: C2paChunkPosition,
}

/// A non-validating report of the C2PA manifest stores a WebP file carries: every `C2PA` chunk in
/// file order, with its store's range, size and position.
///
/// Built by [`c2pa_summary`]. The summary carries the facts and renders the lines, so every host
/// prints the same words and none can drop [`C2PA_NOT_VALIDATED`] while rewording them — the shape
/// and wording of `gamut-heic`'s summary, repeated for this container (issue #530).
///
/// A `C2PA` chunk has no framing of its own beyond the RIFF header — its payload *is* the store
/// (§A.3.7) — so the one way this reader can find a chunk and read no store from it is a payload
/// shorter than a JUMBF box header, listed in [`unread`](Self::unread). A chunk that runs past the
/// data fails the whole RIFF walk instead.
///
/// Non-exhaustive: a later revision may report more without a breaking change.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct C2paSummary {
    /// Every `C2PA` chunk's store, in file order. §A.3.7 admits one per file; a second is listed
    /// rather than hidden, because [`MetadataChunks::read`] and [`c2pa_span`] take the first and a
    /// report that showed one would hide the bytes they pass over.
    pub stores: Vec<C2paStoreSummary>,
    /// Every `C2PA` chunk whose payload is too short to be a store, in file order.
    pub unread: Vec<C2paUnreadChunk>,
}

/// One detail line's source, so the two lists render in the file order they were read in.
enum DetailEntry<'a> {
    Store(&'a C2paStoreSummary),
    Unread(&'a C2paUnreadChunk),
}

impl DetailEntry<'_> {
    fn start(&self) -> usize {
        match self {
            Self::Store(store) => store.range.start,
            Self::Unread(unread) => unread.range.start,
        }
    }

    fn line(&self) -> String {
        let (range, position, what, verdict) = match self {
            Self::Store(store) => (
                &store.range,
                store.position,
                "store",
                "located, not validated",
            ),
            Self::Unread(unread) => (
                &unread.range,
                unread.position,
                "payload",
                "unread: shorter than the 8-byte JUMBF box header a store opens with",
            ),
        };
        format!(
            "  C2PA chunk: {size}-byte {what} at [{start}, {end}) — {verdict}{note}",
            size = range.len(),
            start = range.start,
            end = range.end,
            note = position
                .note()
                .map_or_else(String::new, |note| format!("; {note}")),
        )
    }
}

impl C2paSummary {
    /// The whole human-readable report, uncapped: [`summary_lines`](Self::summary_lines) followed
    /// by every [`detail_lines`](Self::detail_lines) entry. A terminal host should cap the detail
    /// lines instead, reporting [`detail_line_count`](Self::detail_line_count) beside them.
    #[must_use]
    pub fn report_lines(&self) -> Vec<String> {
        let mut lines = self.summary_lines();
        lines.extend(self.detail_lines());
        lines
    }

    /// The report's head — at most two lines, whatever the file holds: the headline, carrying
    /// [`C2PA_NOT_VALIDATED`] inline, and — when the file carries more than one `C2PA` chunk — a
    /// line saying §A.3.7 admits one and which of them this crate's readers take. Not indented.
    ///
    /// The headline has four outcomes, because a file whose `C2PA` chunks yielded no store is
    /// neither "located" nor "none found".
    #[must_use]
    pub fn summary_lines(&self) -> Vec<String> {
        let (located, unread) = (self.stores.len(), self.unread.len());
        let present = format!(
            "{unread} C2PA chunk{noun} present",
            noun = if unread == 1 { " is" } else { "s are" }
        );
        let stores = format!(
            "{located} manifest store{plural} located, NOT VALIDATED",
            plural = if located == 1 { "" } else { "s" },
        );
        let headline = match (located, unread) {
            (0, 0) => format!("C2PA: no C2PA chunk found — {C2PA_NOT_VALIDATED}"),
            (0, _) => format!(
                "C2PA: no manifest store could be read, but {present} — that is NOT absence of \
                 provenance — {C2PA_NOT_VALIDATED}"
            ),
            (_, 0) => format!("C2PA: {stores} — {C2PA_NOT_VALIDATED}"),
            _ => format!(
                "C2PA: {stores}, and {present} from which no store could be read — \
                 {C2PA_NOT_VALIDATED}"
            ),
        };
        let mut lines = vec![headline];
        let count = self.detail_line_count();
        if count > 1 {
            lines.push(format!(
                "C2PA chunks: {count}, where C2PA 2.4 §A.3.7 admits one per file; gamut's readers \
                 take the first"
            ));
        }
        lines
    }

    /// One line per `C2PA` chunk — store or unread — **in file order**, so a host that caps the
    /// list hides the file's last chunks rather than one kind of chunk. Each line is indented two
    /// spaces relative to the headline and built on demand: the payload's size and half-open
    /// range, whether a store was located or why none was read, and the chunk's
    /// [`C2paChunkPosition::note`] when it has one.
    pub fn detail_lines(&self) -> impl Iterator<Item = String> + '_ {
        let mut entries: Vec<DetailEntry<'_>> = self
            .stores
            .iter()
            .map(DetailEntry::Store)
            .chain(self.unread.iter().map(DetailEntry::Unread))
            .collect();
        // Different chunks occupy disjoint ranges, so the lower start is the earlier chunk.
        entries.sort_by_key(DetailEntry::start);
        entries.into_iter().map(|entry| entry.line())
    }

    /// How many lines [`detail_lines`](Self::detail_lines) yields, without building any — the
    /// number of `C2PA` chunks in the file.
    #[must_use]
    pub fn detail_line_count(&self) -> usize {
        self.stores.len() + self.unread.len()
    }
}

/// A non-validating summary of the `C2PA` chunks of the WebP file in `data`: every store's range,
/// size and position, in file order. See [`C2paSummary`] for what it is for and
/// [`C2PA_NOT_VALIDATED`] for what has to be said beside it.
///
/// Unlike [`c2pa_span`], which stops at the first `C2PA` chunk, this walks every chunk, so it also
/// fails on a malformed chunk *after* the first store.
///
/// # Errors
///
/// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if `data` is
/// not a valid RIFF/WebP file, or if a chunk's declared size runs past the end of the data.
pub fn c2pa_summary(data: &[u8]) -> Result<C2paSummary> {
    let mut summary = C2paSummary::default();
    let mut last_end = None;
    for located in located_chunks(data)? {
        let (span, chunk) = located?;
        if WebpChunkId::from(chunk.fourcc) == WebpChunkId::C2pa {
            let range = span.start + CHUNK_HEADER_LEN..span.end;
            let position = C2paChunkPosition::BeforeLaterChunks;
            if range.len() < MIN_STORE_LEN {
                summary.unread.push(C2paUnreadChunk { range, position });
            } else {
                summary.stores.push(C2paStoreSummary { range, position });
            }
        }
        last_end = Some(span.end);
    }
    // Only the file's final chunk is last, and a `C2PA` chunk is last exactly when it ends where
    // that one does.
    let positions = summary
        .stores
        .iter_mut()
        .map(|store| (store.range.end, &mut store.position))
        .chain(
            summary
                .unread
                .iter_mut()
                .map(|unread| (unread.range.end, &mut unread.position)),
        );
    for (end, position) in positions {
        if Some(end) == last_end {
            *position = C2paChunkPosition::Last;
        }
    }
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use gamut_core::ErrorKind;

    use super::*;
    use crate::fourcc::FourCc;
    use crate::layout::MetadataChunks;
    use crate::test_support::{chunk, raw_file, span_in};
    use crate::vp8x::{Vp8xHeader, write_extended_with_metadata, write_simple_lossless};
    use crate::writer::RiffWriter;

    /// `c2pa_span` reports the chunk's whole span — identifier, size field and payload — the range
    /// issue #445 specifies (see `c2pa_span` for its conflict with C2PA 2.4 §18.5.1). The RIFF pad
    /// byte after an odd-length store (§2.3) is framing, so it stays outside both the span and the
    /// payload.
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
            &[chunk(FourCc::VP8L, &[0x2f])],
        )
        .unwrap();

        let span = span_in(&file).expect("the store was embedded");
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
        w.write_chunk(FourCc::C2PA, b"store").unwrap();
        let file = w.finish().unwrap();
        let span = c2pa_span(&file).unwrap().expect("the store was embedded");
        // 12-byte file header, then two chunks of 8 header + 3 payload + 1 pad bytes each; the
        // store's own chunk is its 8-byte header plus its five payload bytes.
        let start: u64 = 12 + (8 + 3 + 1) + (8 + 3 + 1);
        assert_eq!(span, start..start + 8 + 5);
        let span = span_in(&file).expect("the store was embedded");
        assert_eq!(&file[span.start..span.start + 4], b"C2PA");
    }

    /// A file with no store has no span to report, rather than an empty range at some offset a
    /// caller might hash around.
    #[test]
    fn c2pa_span_is_none_without_a_store() {
        let file = write_simple_lossless(&[0x2f]).unwrap();
        assert_eq!(c2pa_span(&file).unwrap(), None);
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

    /// The summary lists every `C2PA` chunk's payload in file order, past the pad bytes of the
    /// chunks before it, and marks the one the file ends with as last.
    #[test]
    fn c2pa_summary_lists_every_store_and_marks_only_the_final_chunk_last() {
        let file = raw_file(&[
            (FourCc::VP8L, &[0x2f, 0x00, 0x00]), // odd -> padded
            (FourCc::C2PA, b"the first"),        // odd -> padded
            (FourCc::EXIF, b"exif"),
            (FourCc::C2PA, b"the second"),
        ]);
        let summary = c2pa_summary(&file).unwrap();
        let first = 12 + (8 + 3 + 1) + 8;
        let second = first + 9 + 1 + (8 + 4) + 8;
        assert_eq!(
            summary.stores,
            vec![
                C2paStoreSummary {
                    range: first..first + 9,
                    position: C2paChunkPosition::BeforeLaterChunks,
                },
                C2paStoreSummary {
                    range: second..second + 10,
                    position: C2paChunkPosition::Last,
                },
            ]
        );
        assert!(summary.unread.is_empty());
        assert_eq!(&file[first..first + 9], b"the first");
        assert_eq!(&file[second..second + 10], b"the second");
        // The store is the span `c2pa_span` reports, less the chunk header.
        assert_eq!(span_in(&file), Some(first - 8..first + 9));

        assert_eq!(summary.detail_line_count(), 2);
        assert_eq!(
            summary.report_lines(),
            vec![
                format!("C2PA: 2 manifest stores located, NOT VALIDATED — {C2PA_NOT_VALIDATED}"),
                "C2PA chunks: 2, where C2PA 2.4 §A.3.7 admits one per file; gamut's readers take \
                 the first"
                    .to_owned(),
                format!(
                    "  C2PA chunk: 9-byte store at [{first}, {}) — located, not validated; another \
                     chunk follows it, where C2PA 2.4 §A.3.7 places it last",
                    first + 9
                ),
                format!(
                    "  C2PA chunk: 10-byte store at [{second}, {}) — located, not validated",
                    second + 10
                ),
            ]
        );
    }

    /// A `C2PA` payload shorter than the 8-byte JUMBF box header is listed as unread, never as a
    /// store and never dropped; one of exactly 8 bytes is a store. Unread and located chunks share
    /// one file-ordered detail list, and an unread chunk the file ends with is marked last.
    #[test]
    fn c2pa_summary_lists_a_payload_shorter_than_a_jumbf_header_as_unread() {
        let file = raw_file(&[
            (FourCc::VP8L, &[0x2f]),
            (FourCc::C2PA, b"8 bytes!"),
            (FourCc::C2PA, b"7 bytes"),
        ]);
        let summary = c2pa_summary(&file).unwrap();
        let store = 12 + (8 + 1 + 1) + 8;
        let unread = store + 8 + 8;
        assert_eq!(
            summary.stores,
            vec![C2paStoreSummary {
                range: store..store + 8,
                position: C2paChunkPosition::BeforeLaterChunks,
            }]
        );
        assert_eq!(
            summary.unread,
            vec![C2paUnreadChunk {
                range: unread..unread + 7,
                position: C2paChunkPosition::Last,
            }]
        );
        assert_eq!(summary.detail_line_count(), 2);
        assert_eq!(
            summary.report_lines(),
            vec![
                format!(
                    "C2PA: 1 manifest store located, NOT VALIDATED, and 1 C2PA chunk is present \
                     from which no store could be read — {C2PA_NOT_VALIDATED}"
                ),
                "C2PA chunks: 2, where C2PA 2.4 §A.3.7 admits one per file; gamut's readers take \
                 the first"
                    .to_owned(),
                format!(
                    "  C2PA chunk: 8-byte store at [{store}, {}) — located, not validated; another \
                     chunk follows it, where C2PA 2.4 §A.3.7 places it last",
                    store + 8
                ),
                format!(
                    "  C2PA chunk: 7-byte payload at [{unread}, {}) — unread: shorter than the \
                     8-byte JUMBF box header a store opens with",
                    unread + 7
                ),
            ]
        );
    }

    /// Chunks that yielded no store are not reported as absence: the headline says they are
    /// present, in the plural when there are several, and the unread line precedes a later store
    /// in the detail list.
    #[test]
    fn c2pa_summary_keeps_unread_chunks_apart_from_absence() {
        let file = raw_file(&[
            (FourCc::C2PA, b""),
            (FourCc::C2PA, b"short"),
            (FourCc::VP8L, &[0x2f]),
        ]);
        let summary = c2pa_summary(&file).unwrap();
        assert!(summary.stores.is_empty());
        assert_eq!(summary.unread.len(), 2);
        assert!(
            summary
                .unread
                .iter()
                .all(|unread| unread.position == C2paChunkPosition::BeforeLaterChunks)
        );
        assert_eq!(
            summary.summary_lines()[0],
            format!(
                "C2PA: no manifest store could be read, but 2 C2PA chunks are present — that is \
                 NOT absence of provenance — {C2PA_NOT_VALIDATED}"
            )
        );

        let mixed = raw_file(&[(FourCc::C2PA, b"short"), (FourCc::C2PA, b"a whole store")]);
        let summary = c2pa_summary(&mixed).unwrap();
        let lines: Vec<String> = summary.detail_lines().collect();
        assert!(lines[0].contains("5-byte payload"), "{lines:?}");
        assert!(lines[1].contains("13-byte store"), "{lines:?}");
    }

    /// One store placed last reports one line and no multiplicity line; none reports absence with
    /// the disclaimer still inline.
    #[test]
    fn c2pa_summary_headlines_one_store_and_none() {
        let one = raw_file(&[(FourCc::VP8L, &[0x2f]), (FourCc::C2PA, b"a store!")]);
        let summary = c2pa_summary(&one).unwrap();
        assert_eq!(summary.stores.len(), 1);
        assert_eq!(summary.stores[0].size(), 8);
        assert_eq!(summary.stores[0].position, C2paChunkPosition::Last);
        assert_eq!(
            summary.summary_lines(),
            vec![format!(
                "C2PA: 1 manifest store located, NOT VALIDATED — {C2PA_NOT_VALIDATED}"
            )]
        );

        let none = write_simple_lossless(&[0x2f]).unwrap();
        let summary = c2pa_summary(&none).unwrap();
        assert_eq!(summary.detail_line_count(), 0);
        assert_eq!(
            summary.report_lines(),
            vec![format!("C2PA: no C2PA chunk found — {C2PA_NOT_VALIDATED}")]
        );
    }

    /// A chunk running past the data fails the walk, even after a store was seen — the summary
    /// lists every chunk, so it cannot stop at the first store as `c2pa_span` does.
    #[test]
    fn c2pa_summary_refuses_a_chunk_that_runs_past_the_data() {
        let mut file = raw_file(&[(FourCc::C2PA, b"store!"), (FourCc::EXIF, b"exif")]);
        let size_field = file.len() - 4 - 4;
        file[size_field..size_field + 4].copy_from_slice(&64u32.to_le_bytes());
        assert_eq!(
            c2pa_summary(&file).unwrap_err().kind(),
            ErrorKind::InvalidInput
        );
        assert!(c2pa_span(&file).unwrap().is_some());
    }
}
