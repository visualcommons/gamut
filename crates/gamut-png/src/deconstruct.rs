//! Where a PNG's bytes went (issue #224): every byte of the file classified into a typed
//! [`Segment`], plus the per-stage figures an encoder-efficiency comparison is built from.
//!
//! This is the measurement counterpart to [`crate::PngEncoder`]. It works on **any** PNG, not
//! just this crate's output, so the same numbers can be read off libpng's, oxipng's or
//! zopflipng's files and compared directly: bits per pixel, what the DEFLATE stage achieved in
//! isolation, how many bytes went to chunk framing, and which scanline filters the encoder
//! actually chose.
//!
//! # The every-byte invariant
//!
//! [`PngReport::segments`] is contiguous, non-overlapping, and covers `0..file_len` exactly.
//! It holds by construction, and [`PngReport::is_fully_classified`] re-derives it from the list
//! rather than storing a flag, so a walk bug makes the predicate false instead of silently
//! agreeing with itself. This mirrors [`gamut_isobmff::segments`]'s guarantee for ISOBMFF; PNG's
//! chunk stream needs its own walk (there are no boxes and no `meta` level), but the shape and
//! the names are deliberately the same.
//!
//! # What is an error and what is a finding
//!
//! Deliberately more tolerant than [`crate::PngDecoder::metadata`], and for the reason
//! [`gamut_dng::deconstruct`] gives: a measurement tool that refuses to measure is useless.
//! Unknown ancillary **and critical** chunks, CRC mismatches, a missing IEND, trailing bytes and
//! a truncated tail are all *reported*, never errors. Only a file with no header to report on —
//! bad signature, no first chunk, a first chunk that is not IHDR, or an unparsable IHDR — fails.

use core::ops::Range;
use std::collections::HashMap;

use gamut_core::{Error, Result};

use crate::chunk::{ChunkReader, RawChunk, SIGNATURE};
use crate::decoded::PngHeader;
use crate::decoder::DEFAULT_MAX_IMAGE_BYTES;
use crate::filter::FilterType;
use crate::{adam7, ihdr, inflate};

/// Chunk framing overhead: 4 length bytes + 4 type bytes + 4 CRC bytes (§5.3).
const FRAMING: usize = 12;

/// One contiguous run of the input file, tagged by what it holds ([`SegmentKind`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    /// The half-open byte range this segment occupies within the input (`start..end`).
    pub range: Range<usize>,
    /// What the bytes in [`range`](Self::range) are.
    pub kind: SegmentKind,
}

/// What a [`Segment`] holds.
///
/// Non-exhaustive: a future revision may name a further region (an APNG frame span, say) without
/// a breaking change — match with a wildcard arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SegmentKind {
    /// The 8-byte PNG file signature (§5.2). Always the first segment.
    Signature,
    /// One complete chunk: 4 length bytes, 4 type bytes, the payload, 4 CRC bytes (§5.3), so the
    /// segment is always `payload_len + 12` bytes long.
    Chunk {
        /// The chunk's four-character type, e.g. `*b"IDAT"`. Recognised and unrecognised types
        /// alike appear here — critical ones included; the walk never drops a chunk.
        chunk_type: [u8; 4],
        /// The declared payload length, framing excluded.
        payload_len: usize,
        /// Whether the stored CRC-32 over type + payload matched (§5.5). A mismatch is reported,
        /// never an error: §13.1 makes it recoverable in an ancillary chunk, and the framing is
        /// intact either way, so the walk can keep going and account the rest of the file.
        crc_ok: bool,
    },
    /// Bytes after IEND. Not part of the datastream — §13.2 asks decoders to ignore them, so they
    /// are surfaced here rather than silently dropped.
    Trailer,
    /// From the first chunk header that does not frame — truncated, or declaring a length that
    /// overruns the input — to end of file. A file carrying one is not a complete PNG.
    Truncated,
}

/// Per-chunk-type totals, in first-appearance order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct ChunkStats {
    /// The chunk's four-character type.
    pub chunk_type: [u8; 4],
    /// How many chunks of this type the file carries.
    pub count: usize,
    /// Total payload bytes across those chunks — framing excluded.
    pub payload_bytes: usize,
}

impl ChunkStats {
    /// Framing bytes these chunks cost: 12 per chunk (4 length + 4 type + 4 CRC, §5.3).
    #[must_use]
    pub fn framing_bytes(&self) -> usize {
        self.count * FRAMING
    }

    /// Payload plus framing — what this chunk type costs the file in total.
    #[must_use]
    pub fn total_bytes(&self) -> usize {
        self.payload_bytes + self.framing_bytes()
    }

    /// Whether the type is ancillary — bit 5 of the first byte set, i.e. lowercase (§5.4).
    #[must_use]
    pub fn is_ancillary(&self) -> bool {
        self.chunk_type[0] & 0x20 != 0
    }
}

/// One reduced image making up the filtered stream: an Adam7 pass (§8.1), or the whole image when
/// the file is not interlaced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct PassStats {
    /// Pass index in transmission order (`0..7`); always `0` when the file is not interlaced.
    pub index: u8,
    /// The reduced image's width in pixels. Never zero: an empty pass carries no bytes at all,
    /// not even filter-type bytes (§7.3), so it is omitted entirely.
    pub width: u32,
    /// The reduced image's height in pixels. Never zero, for the same reason.
    pub height: u32,
    /// Bytes per scanline excluding the filter-type byte: `ceil(width × bits_per_pixel / 8)`, so
    /// a sub-byte depth includes its row padding (§7.2).
    pub row_bytes: usize,
    /// This pass's contribution to the filtered stream: `height × (1 + row_bytes)`.
    pub filtered_len: usize,
}

/// How many scanlines chose each of the five filters (§9.1), summed over every pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FilterHistogram {
    counts: [u32; 5],
}

impl FilterHistogram {
    /// Scanlines that chose `filter`.
    #[must_use]
    pub fn count(self, filter: FilterType) -> u32 {
        self.counts[filter as usize]
    }

    /// Total scanlines — the sum over all five filters, and the image's scanline count.
    #[must_use]
    pub fn total(self) -> u32 {
        self.counts.iter().sum()
    }
}

/// The outcome of the walk's optional filter scan: the counts, or why there are none.
///
/// The scan is the one part of a report that has to inflate the IDAT stream, so it is the one
/// part that can be absent. Which is why the absence is *typed*: "no histogram" conflates a file
/// this reader declined to inflate with a file whose compressed data is broken, and only the
/// second is damage. [`is_damage`](Self::is_damage) answers that question once, for both
/// [`PngReport::is_intact`] and any caller that has to grade a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterScan {
    /// The IDAT stream inflated to the expected length and every scanline's filter code was read.
    Counted(FilterHistogram),
    /// No counts, for the stated reason.
    Skipped(SkippedFilterScan),
}

impl FilterScan {
    /// The per-filter counts, if the scan ran.
    #[must_use]
    pub fn histogram(self) -> Option<FilterHistogram> {
        match self {
            Self::Counted(histogram) => Some(histogram),
            Self::Skipped(_) => None,
        }
    }

    /// Why there are no counts, if there are none.
    #[must_use]
    pub fn skipped(self) -> Option<SkippedFilterScan> {
        match self {
            Self::Counted(_) => None,
            Self::Skipped(reason) => Some(reason),
        }
    }

    /// Whether the scan actually ran, so the counts describe bytes this reader read.
    ///
    /// The complement of [`is_damage`](Self::is_damage) only for a scan that ran: a skip is
    /// either damage or a refusal to read, and **neither is a verification**. A caller grading a
    /// file — [`PngReport::is_verified`], an archival gate — asks this; a caller asking whether
    /// anything is known to be *wrong* asks `is_damage`.
    #[must_use]
    pub fn is_counted(self) -> bool {
        matches!(self, Self::Counted(_))
    }

    /// Whether the missing counts mean the *file* is damaged — see
    /// [`SkippedFilterScan::is_damage`]. A scan that ran is never damage.
    #[must_use]
    pub fn is_damage(self) -> bool {
        match self {
            Self::Counted(_) => false,
            Self::Skipped(reason) => reason.is_damage(),
        }
    }
}

/// Why a [`FilterScan`] carries no counts.
///
/// `#[repr(u8)]` with explicit discriminants, which are **permanent and append-only**: the value
/// is plain data a C caller reads by number, so a variant is never renumbered or removed.
/// Non-exhaustive — match with a wildcard arm, and prefer [`is_damage`](Self::is_damage) to
/// enumerating the reasons yourself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
#[non_exhaustive]
pub enum SkippedFilterScan {
    /// The image the header describes is larger than this reader's byte budget, so the walk
    /// declined to inflate a stream a decode would refuse to allocate. **Nothing is known to be
    /// wrong with the file** — it may be a perfectly sound very large PNG, read by raising
    /// [`DeconstructLimits::max_image_bytes`].
    OverBudget = 0,
    /// The IDAT stream is not a valid zlib stream, is truncated, or inflates past the length the
    /// header implies.
    CorruptStream = 1,
    /// The stream inflated, but to a different length than the header implies, so the scanline
    /// boundaries it describes are not where the filter bytes are.
    LengthMismatch = 2,
    /// A scanline's leading byte is not one of the five filter codes §9.1 defines.
    UndefinedFilterCode = 3,
    /// The image fits this reader's byte budget but is past the decoder's default one, and the
    /// IDAT stream is too short to plausibly inflate to it — more than sixty-four times its own
    /// length — which is the shape of a zlib bomb under a permissive budget.
    ///
    /// Distinct from [`OverBudget`](Self::OverBudget), which the image *exceeding* the budget
    /// raises: a file refused here is one whose declared image the budget admits, so saying it is
    /// too large would name a limit it does not cross. **Nothing is known to be wrong with the
    /// file** either — a flat 16384×16384 image really does compress this far — only that this
    /// reader will not spend the budget to find out.
    ImplausibleInflation = 4,
}

impl SkippedFilterScan {
    /// Whether this reason means the **file** is damaged, rather than merely unread.
    ///
    /// The single source of truth for that question, so no caller has to re-derive it from the
    /// variant list. The two reasons that are not damage are the two refusals to read —
    /// [`OverBudget`](Self::OverBudget) and
    /// [`ImplausibleInflation`](Self::ImplausibleInflation) — which describe what this reader was
    /// willing to spend, not what the file contains. Every other reason is a statement about the
    /// bytes, and a future reason is damage until it says otherwise.
    #[must_use]
    pub fn is_damage(self) -> bool {
        !matches!(self, Self::OverBudget | Self::ImplausibleInflation)
    }
}

/// Where a PNG's bytes went: a total byte accounting plus the figures an encoder-efficiency
/// comparison is built from. Produced by [`deconstruct`].
///
/// Non-exhaustive: report categories may be added without a breaking change.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PngReport {
    /// The input's total length in bytes — what [`segments`](Self::segments) together covers.
    pub file_len: usize,
    /// IHDR: dimensions, bit depth, colour type, interlace method.
    pub header: PngHeader,
    /// Every byte of the input in file order — contiguous, non-overlapping, covering
    /// `0..file_len` exactly. See the [every-byte invariant](self#the-every-byte-invariant).
    pub segments: Vec<Segment>,
    /// Per-chunk-type totals, in first-appearance order.
    pub chunks: Vec<ChunkStats>,
    /// The concatenated IDAT payload length: the zlib codestream, framing excluded. This is what
    /// the encoder's compression stage produced, and the numerator of
    /// [`idat_ratio`](Self::idat_ratio).
    pub idat_compressed: usize,
    /// The length that codestream inflates to — the filter-prefixed scanline stream. Derived from
    /// IHDR alone (the sum over [`passes`](Self::passes) when interlaced), so it is known even
    /// when [`filters`](Self::filters) was skipped.
    pub filtered_len: usize,
    /// The reduced images making up the filtered stream: one entry per non-empty Adam7 pass, or
    /// exactly one entry for a non-interlaced image.
    pub passes: Vec<PassStats>,
    /// Scanlines per filter type, or the reason the IDAT stream was not scanned. Everything else
    /// in this report is derived from framing and IHDR, so it survives whatever the reason is.
    pub filters: FilterScan,
}

impl PngReport {
    /// **The headline law.** Whether the segments tile the input exactly: the first starts at 0,
    /// each ends where the next starts, none is empty, and the last ends at `file_len`.
    /// Re-derived from [`segments`](Self::segments) rather than stored.
    #[must_use]
    pub fn is_fully_classified(&self) -> bool {
        let mut expected = 0usize;
        for segment in &self.segments {
            if segment.range.start != expected || segment.range.end <= segment.range.start {
                return false;
            }
            expected = segment.range.end;
        }
        expected == self.file_len
    }

    /// Whether every byte of this file belongs to a complete, undamaged PNG datastream: fully
    /// classified, no [`SegmentKind::Truncated`] and no [`SegmentKind::Trailer`], every CRC
    /// valid, IEND present, and nothing damaging found by the filter scan.
    ///
    /// A trailer counts against it even though §13.2 lets a *decoder* ignore trailing bytes,
    /// because [`bits_per_pixel`](Self::bits_per_pixel) divides the whole file by the pixel
    /// count: bytes outside the datastream still inflate the headline figure, so a size
    /// comparison has to know they are there.
    ///
    /// Independent of whether every chunk type was *recognised* — an unknown critical chunk is
    /// still accounted for. The filter conjunct is
    /// [`!filters.is_damage()`](FilterScan::is_damage), not "the scan ran": a stream this reader
    /// declined to inflate says nothing against the file, while a corrupt zlib payload under a
    /// valid CRC is damage **only** the scan can see, so dropping the conjunct would stop
    /// detecting it.
    #[must_use]
    pub fn is_intact(&self) -> bool {
        self.is_fully_classified()
            && !self.filters.is_damage()
            && self.segments.iter().all(|segment| match segment.kind {
                SegmentKind::Truncated | SegmentKind::Trailer => false,
                SegmentKind::Chunk { crc_ok, .. } => crc_ok,
                SegmentKind::Signature => true,
            })
            && self.chunk(b"IEND").is_some()
    }

    /// Whether this file is intact **and every byte of it was actually read**: `is_intact()` plus
    /// [`FilterScan::is_counted`].
    ///
    /// The distinction [`is_intact`](Self::is_intact) deliberately does not make. `is_intact` is
    /// "nothing is known to be wrong", which a file whose IDAT was never inflated satisfies
    /// vacuously — and a corrupt zlib payload under a valid CRC is damage *only* the scan can
    /// see, so for an over-budget file `is_intact` is a statement about this reader's budget
    /// rather than about the bytes. A gate that must not pass an unread file asks this instead;
    /// a caller reporting what is known against a file keeps asking `is_intact`.
    #[must_use]
    pub fn is_verified(&self) -> bool {
        self.is_intact() && self.filters.is_counted()
    }

    /// **Stored bits per image pixel** — the space-efficiency figure of merit: the whole file,
    /// framing and metadata included, over `width × height`. Distinct from the *uncompressed*
    /// rate, which is `header.color_type.channels() × header.bit_depth`.
    #[must_use]
    pub fn bits_per_pixel(&self) -> f64 {
        let pixels = f64::from(self.header.width) * f64::from(self.header.height);
        // IHDR rejects a zero dimension, so `pixels >= 1.0` for any report that exists.
        self.file_len as f64 * 8.0 / pixels
    }

    /// The DEFLATE stage's compression ratio in isolation: `idat_compressed / filtered_len`.
    /// Below 1.0 means the codestream compressed. Filtering and colour-type choice are *upstream*
    /// of this number, which is what makes it the right lens for attributing a size difference to
    /// the compressor rather than to the rest of the encoder.
    ///
    /// `0.0` when the filtered stream has no length. That is not a dead branch: IHDR admits
    /// dimensions whose filtered stream overflows `usize` — 2³¹−1 square at RGBA16 is 2⁶⁵ bytes —
    /// and [`deconstruct`] reports such a file rather than refusing it, leaving
    /// [`filtered_len`](Self::filtered_len) zero. Thirteen header bytes reach it, so the guard is
    /// what keeps `gamut inspect` from dividing by zero on a hostile file.
    #[must_use]
    pub fn idat_ratio(&self) -> f64 {
        if self.filtered_len == 0 {
            return 0.0;
        }
        self.idat_compressed as f64 / self.filtered_len as f64
    }

    /// Every byte that is not IDAT payload: the signature, all chunk framing, and every non-IDAT
    /// payload.
    #[must_use]
    pub fn overhead_bytes(&self) -> usize {
        self.file_len - self.idat_compressed
    }

    /// Total chunk framing: 12 bytes per chunk in the file.
    #[must_use]
    pub fn framing_bytes(&self) -> usize {
        self.chunks.iter().map(ChunkStats::framing_bytes).sum()
    }

    /// The decoded image's byte cost — `width × height × channels`, doubled at depth 16 — or
    /// `None` when that overflows `usize`.
    ///
    /// The quantity a decoder budgets, and the one this walk gates its filter scan on, so a
    /// [`SkippedFilterScan::OverBudget`] report is exactly one whose `native_bytes` exceeds the
    /// reader's budget. Distinct from [`filtered_len`](Self::filtered_len), which adds one filter
    /// byte per scanline and counts sub-byte samples packed.
    #[must_use]
    pub fn native_bytes(&self) -> Option<usize> {
        ihdr::native_bytes(
            self.header.width,
            self.header.height,
            self.header.color_type.channels(),
            self.header.bit_depth,
        )
    }

    /// The stats for one chunk type, if the file carries it.
    ///
    /// A linear scan of [`chunks`](Self::chunks), so it costs O(distinct chunk types) per call —
    /// bounded by the *types* the file carries, not by its chunk count. Looking up a handful of
    /// types is what this is for; to summarise every type, iterate [`chunks`](Self::chunks) once
    /// rather than calling this per type.
    #[must_use]
    pub fn chunk(&self, chunk_type: &[u8; 4]) -> Option<ChunkStats> {
        self.chunks
            .iter()
            .find(|stats| &stats.chunk_type == chunk_type)
            .copied()
    }
}

/// Accumulates the per-chunk-type totals of one walk, in time linear in the chunk count.
///
/// A chunk type is four **unvalidated** bytes — [`crate::chunk`] reads them straight out of the
/// file and the walk never drops a chunk — so a hostile 12-byte-per-chunk file carries one
/// *distinct* type per chunk. Accumulating with a linear `find` over the types seen so far is
/// then quadratic in the file length: 4.8 MB of empty chunks took 40.9 s. The index makes each
/// chunk O(1), and `stats` keeps the first-appearance order [`PngReport::chunks`] documents.
///
/// The keys are attacker-chosen, which is safe **because** [`HashMap`]'s default hasher is
/// SipHash-1-3 seeded per process: collisions cannot be precomputed against it. Do not swap in a
/// faster unseeded hasher (`FxHash`, `AHash` without a random seed) — that would reopen the
/// quadratic blow-up this type exists to close, by a different route.
struct ChunkTally {
    /// One entry per distinct type, in first-appearance order.
    stats: Vec<ChunkStats>,
    /// Type → its index in `stats`. Dropped at the end of the walk; never surfaced.
    index: HashMap<[u8; 4], usize>,
    /// Lookup work done so far, in entries examined — the probe that makes this type's
    /// complexity assertable by count rather than by clock. Charged by
    /// [`lookup`](Self::lookup), which is where the examining happens.
    #[cfg(test)]
    probes: usize,
}

impl ChunkTally {
    /// An empty tally.
    fn new() -> Self {
        Self {
            stats: Vec::new(),
            index: HashMap::new(),
            #[cfg(test)]
            probes: 0,
        }
    }

    /// Adds one chunk of `chunk_type` carrying `payload_len` payload bytes.
    ///
    /// All of its lookup work goes through [`lookup`](Self::lookup), which is where that work is
    /// accounted.
    fn record(&mut self, chunk_type: [u8; 4], payload_len: usize) {
        match self.lookup(chunk_type) {
            Some(at) => {
                self.stats[at].count += 1;
                self.stats[at].payload_bytes += payload_len;
            }
            None => {
                self.index.insert(chunk_type, self.stats.len());
                self.stats.push(ChunkStats {
                    chunk_type,
                    count: 1,
                    payload_bytes: payload_len,
                });
            }
        }
    }

    /// Where `chunk_type`'s entry sits in `stats`, if it has one — the tally's **only** lookup,
    /// and the only place the probe counter is charged.
    ///
    /// The charge is one per entry the strategy *examines*, made where the examining happens: a
    /// hash lookup examines the single entry its bucket holds, whatever `stats` already contains,
    /// so it charges one and N chunks cost N probes. The linear scan this replaced compares
    /// entries in a loop, so the same rule charges one per comparison from inside that loop, and
    /// N chunks of N distinct types cost about N²/2 — which is what makes
    /// `the_tally_probes_once_per_chunk_whatever_the_number_of_distinct_types` fail if the
    /// quadratic walk ever comes back, instead of bounding the walk by the clock. A replacement
    /// strategy must keep that rule; charging once per call regardless of the work done would
    /// leave the test asserting nothing.
    fn lookup(&mut self, chunk_type: [u8; 4]) -> Option<usize> {
        let at = self.index.get(&chunk_type).copied();
        #[cfg(test)]
        {
            // One bucket entry examined, whatever `stats` holds.
            self.probes += 1;
        }
        at
    }

    /// The accumulated totals, in first-appearance order.
    fn into_stats(self) -> Vec<ChunkStats> {
        self.stats
    }
}

/// Classifies every byte of `png` and, where the IDAT stream is sound and within budget, counts
/// the scanline filter each row chose.
///
/// Pixels are never reconstructed: no defiltering, no unpacking, no de-interlacing, no palette
/// resolution. The walk reads chunk framing and the IHDR, and inflates IDAT only to read one
/// filter byte per scanline.
///
/// Works on any PNG, whichever encoder produced it, which is what makes the figures comparable
/// across encoders (issue #224).
///
/// Walks under [`DeconstructLimits::default()`]. Use
/// [`deconstruct_with_limits`] to match a decoder you configured yourself.
///
/// # Errors
///
/// Returns [`Error::InvalidInput`] when there is no header to report on — a bad signature, no
/// first chunk, a first chunk that is not IHDR, or an IHDR whose payload is invalid — or when the
/// input carries more chunks than [`DeconstructLimits::max_chunks`] allows. Everything else is
/// **reported, not errored** — unknown ancillary *and critical* chunks, CRC mismatches, a missing
/// IEND, trailing bytes after IEND, a truncated tail, and a corrupt IDAT stream.
pub fn deconstruct(png: &[u8]) -> Result<PngReport> {
    deconstruct_with_limits(png, DeconstructLimits::default())
}

/// The ceilings a [`deconstruct`] walk observes on attacker-chosen quantities.
///
/// Every field is a quantity the *input* chooses, which is why each has a ceiling: a report is
/// routinely run over files from anywhere (`gamut inspect` is pointed at whatever is on disk), and
/// the crate's decoder already caps the same quantities for the same reason.
///
/// Non-exhaustive: ceilings may be added without a breaking change. Build from
/// [`default()`](Self::default) and adjust the fields you care about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct DeconstructLimits {
    /// The largest decoded image, in bytes, whose IDAT stream is worth inflating to count
    /// filters. Above it the scan is skipped as [`SkippedFilterScan::OverBudget`] and every other
    /// figure is still reported, because everything else is derived from framing and IHDR.
    ///
    /// This is the quantity [`crate::PngDecoder::with_max_image_bytes`] budgets, and matching the
    /// two is the point: a report is only "what a decode would have allocated" against a decoder
    /// configured the same way. The default matches the decoder's default.
    ///
    /// Raising it past the decoder's default admits larger *images*, not larger *inflations from
    /// small files*: above that default the walk also refuses, before inflating, a stream that
    /// would grow to more than sixty-four times its own length, so a permissive budget cannot be
    /// spent by a zlib bomb. That refusal reports
    /// [`SkippedFilterScan::ImplausibleInflation`] — the image fits this budget, so it is not
    /// [`OverBudget`](SkippedFilterScan::OverBudget).
    pub max_image_bytes: usize,
    /// The largest number of chunks the walk will materialize into segments and per-type stats.
    ///
    /// A chunk costs 12 bytes of input and buys a `Segment` plus, for a type not seen before, a
    /// `ChunkStats` and an index entry — so an input of unbounded chunk count is an input of
    /// unbounded heap, at roughly an order of magnitude over the file size. The chunk *type* is
    /// four unvalidated bytes, so the distinct-type count is attacker-chosen too.
    ///
    /// Counted over chunks, **IHDR included**: `with_max_chunks(N)` admits a file of exactly N
    /// chunks and refuses one of N + 1, and `with_max_chunks(0)` admits no file at all, since
    /// every datastream this walk reports on opens with IHDR.
    ///
    /// The default admits any plausible real file — a PNG at the ceiling is at least 12 MiB of
    /// pure chunk framing — while bounding a crafted one.
    pub max_chunks: usize,
}

/// The chunk-count ceiling a default [`deconstruct`] walk observes.
///
/// A PNG reaching it carries at least 12 MiB of chunk framing alone, which no real file does and a
/// crafted one reaches cheaply.
pub const DEFAULT_MAX_CHUNKS: usize = 1 << 20;

impl Default for DeconstructLimits {
    fn default() -> Self {
        Self {
            max_image_bytes: DEFAULT_MAX_IMAGE_BYTES,
            max_chunks: DEFAULT_MAX_CHUNKS,
        }
    }
}

impl DeconstructLimits {
    /// Sets [`max_image_bytes`](Self::max_image_bytes).
    ///
    /// Builder methods rather than a struct literal, matching
    /// [`PngDecoder::with_max_image_bytes`](crate::PngDecoder::with_max_image_bytes) — and
    /// necessary as well as symmetrical, since a non-exhaustive struct cannot be built by literal
    /// outside this crate at all.
    #[must_use]
    pub fn with_max_image_bytes(mut self, bytes: usize) -> Self {
        self.max_image_bytes = bytes;
        self
    }

    /// Sets [`max_chunks`](Self::max_chunks).
    #[must_use]
    pub fn with_max_chunks(mut self, chunks: usize) -> Self {
        self.max_chunks = chunks;
        self
    }
}

/// [`deconstruct`], under caller-chosen [`DeconstructLimits`].
///
/// # Errors
///
/// As [`deconstruct`], against `limits` rather than the defaults.
pub fn deconstruct_with_limits(png: &[u8], limits: DeconstructLimits) -> Result<PngReport> {
    let mut reader = ChunkReader::new(png)?;
    let mut segments = vec![Segment {
        range: 0..SIGNATURE.len(),
        kind: SegmentKind::Signature,
    }];

    let first = reader.next_chunk()?.ok_or_else(|| {
        Error::invalid_input(env!("CARGO_PKG_NAME"), "PNG: no chunk after the signature")
    })?;
    if &first.chunk_type != b"IHDR" {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "PNG: first chunk is not IHDR",
        ));
    }
    let native = ihdr::parse(first.data)?;
    let header = PngHeader {
        width: native.width,
        height: native.height,
        bit_depth: native.bit_depth,
        color_type: native.color,
        interlaced: native.interlaced,
    };

    let mut tally = ChunkTally::new();
    let mut idat = Vec::new();
    let mut saw_iend = false;
    // Every chunk enters the report here, IHDR included, so the ceiling is checked here too.
    // A check placed only inside the loop below counts IHDR (it counts every chunk pushed so
    // far), so it is exact for any datastream that reaches the loop body — but it never runs for
    // one that does not: a walk that pushes no chunk after IHDR, because the datastream is
    // truncated at it or ends there, escaped the ceiling entirely. Visible at `max_chunks(0)`,
    // which admitted such a file.
    let push =
        |segments: &mut Vec<Segment>, tally: &mut ChunkTally, chunk: &RawChunk| -> Result<()> {
            segments.push(Segment {
                range: chunk.range.clone(),
                kind: SegmentKind::Chunk {
                    chunk_type: chunk.chunk_type,
                    payload_len: chunk.data.len(),
                    crc_ok: chunk.crc_ok,
                },
            });
            tally.record(chunk.chunk_type, chunk.data.len());
            // The signature segment is not a chunk, so the ceiling is over one fewer than the
            // segments materialized so far. Nothing but a chunk has been pushed at this point:
            // `Truncated` and `Trailer` end the walk.
            if segments.len() - 1 > limits.max_chunks {
                return Err(Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "PNG: more chunks than the walk's ceiling admits",
                ));
            }
            Ok(())
        };
    push(&mut segments, &mut tally, &first)?;

    loop {
        match reader.next_chunk() {
            Ok(None) => break,
            Ok(Some(chunk)) => {
                if &chunk.chunk_type == b"IDAT" {
                    idat.extend_from_slice(chunk.data);
                }
                let is_iend = &chunk.chunk_type == b"IEND";
                push(&mut segments, &mut tally, &chunk)?;
                if is_iend {
                    saw_iend = true;
                    break;
                }
            }
            // A header that does not frame ends the datastream; the rest of the file is
            // accounted as one opaque run rather than dropped (§13.2's tolerance, extended to
            // damage the spec does not describe).
            Err(_) => {
                // `next_chunk` returns `Ok(None)` when nothing is left, so reaching an error means
                // bytes remain and this range is never empty. No guard: a `start < png.len()`
                // check here can never be false, which makes it dead code and an equivalent
                // mutant rather than a safety net.
                let start = reader.offset();
                debug_assert!(
                    start < png.len(),
                    "a framing error leaves bytes unaccounted"
                );
                segments.push(Segment {
                    range: start..png.len(),
                    kind: SegmentKind::Truncated,
                });
                break;
            }
        }
    }
    if saw_iend && reader.offset() < png.len() {
        segments.push(Segment {
            range: reader.offset()..png.len(),
            kind: SegmentKind::Trailer,
        });
    }

    let passes = pass_stats(&native);
    let filtered_len = adam7::expected_stream_len(&native).unwrap_or(0);
    let filters = scan_filters(
        &native,
        &idat,
        filtered_len,
        &passes,
        limits.max_image_bytes,
    );

    Ok(PngReport {
        file_len: png.len(),
        header,
        segments,
        chunks: tally.into_stats(),
        idat_compressed: idat.len(),
        filtered_len,
        passes,
        filters,
    })
}

/// The reduced images making up the filtered stream, skipping empty passes exactly as
/// [`adam7::expected_stream_len`] does — so `filtered_len` is the sum of these and can be checked
/// against it rather than merely asserted.
fn pass_stats(header: &ihdr::Ihdr) -> Vec<PassStats> {
    let mut out = Vec::new();
    let mut total = 0usize;
    for (index, pass) in adam7::passes_for(header.interlaced).iter().enumerate() {
        let (width, height) = adam7::pass_dimensions(pass, header.width, header.height);
        if width == 0 || height == 0 {
            continue;
        }
        let Some(row_bytes) = (width as usize)
            .checked_mul(header.bits_per_pixel())
            .map(|bits| bits.div_ceil(8))
        else {
            return Vec::new();
        };
        let Some(filtered_len) = row_bytes
            .checked_add(1)
            .and_then(|stride| (height as usize).checked_mul(stride))
        else {
            return Vec::new();
        };
        // `adam7::expected_stream_len` fails on the seven-pass *sum* as well as on each pass, so
        // this has to fail with it. Without the running check, a header whose passes each fit but
        // whose total overflows leaves `filtered_len` saturated to 0 while `passes` still
        // describes all seven -- a self-inconsistent report, and a `0.0%` ratio that reads as a
        // measurement rather than as an overflow.
        let Some(running) = total.checked_add(filtered_len) else {
            return Vec::new();
        };
        total = running;
        out.push(PassStats {
            index: index as u8,
            width,
            height,
            row_bytes,
            filtered_len,
        });
    }
    out
}

/// Whether this file's IDAT stream is worth inflating to count filters: whether the image its
/// header describes fits `max_image_bytes`.
///
/// The budgeted quantity is [`ihdr::native_bytes`] — the decoded buffer — because that is exactly
/// what [`crate::PngDecoder`] budgets, so "a report never allocates more than a decode would"
/// holds by construction. Budgeting the *filtered* stream instead states the same intent over a
/// different number: the two differ by one filter byte per scanline, so a 4096×4096 RGBA8 image
/// is 67 108 864 native bytes (decodes on the default budget) and 67 112 960 filtered — and the
/// report declined to scan a file the decoder decodes, reporting it as damaged.
///
/// Inflation is still bounded: the filtered stream is at most the native bytes plus one byte per
/// scanline, so a file that passes here inflates to under twice the budget.
///
/// The budget is a parameter rather than a constant so the boundary is reachable from a unit test
/// without a 64 MiB fixture.
fn fits_decode_budget(header: &ihdr::Ihdr, max_image_bytes: usize) -> bool {
    ihdr::native_bytes(
        header.width,
        header.height,
        header.color.channels(),
        header.bit_depth,
    )
    .is_some_and(|native| native <= max_image_bytes)
}

/// How many times its own length an IDAT stream may inflate, once the image it describes is past
/// the decoder's default budget.
///
/// DEFLATE's ceiling is about 1032:1, so a stream at this ratio is either a large flat image or a
/// bomb — and above [`DEFAULT_MAX_IMAGE_BYTES`] the walk stops assuming the former. A flat 16k×16k
/// image is the one real file this declines, and it is declined as this reader's unwillingness to
/// spend ([`SkippedFilterScan::ImplausibleInflation`]), not as damage — and not as
/// [`OverBudget`](SkippedFilterScan::OverBudget) either, since its image fits the budget that
/// admitted it.
///
/// What a small hostile file can still cost, numerically: inside the default budget the ratio
/// does not apply, so a few-kilobyte stream declaring an image that just fits 64 MiB is inflated
/// to that image's filtered length — 64 MiB plus one byte per scanline, 64 MiB + 4 KiB for
/// 4096×4096 RGBA8 and up to 128 MiB for a degenerate one-pixel-wide greyscale column. That is
/// exactly the decoder's own default exposure to the same header (`PngDecoder` allocates it),
/// so the walk is never a cheaper bomb target than a decode; the ratio only stops a *raised*
/// image budget from becoming one.
const INFLATION_RATIO: usize = 64;

/// Whether a stream of `idat_len` compressed bytes may be inflated to `filtered_len`: it must
/// carry at least a sixty-fourth of what it claims to inflate to. Inclusive, as
/// [`fits_decode_budget`] is, and saturating — a stream too large to multiply is allowed anything,
/// not wrapped to a small allowance that would refuse every huge file.
///
/// [`DeconstructLimits::max_image_bytes`] bounds the *image* a caller is willing to scan; this
/// bounds the *file* against it, and [`scan_filters`] applies it only past the decoder's default
/// budget, so every file the decoder inflates by default is scanned whatever its ratio. `gamut
/// inspect` raises the image budget to a gigabyte so that a 16k×16k photograph is read, and that
/// is right for a photograph — its IDAT is hundreds of megabytes. It is wrong for a megabyte
/// declaring the same header over a zlib stream of zeros, which the header budget alone would
/// inflate to that gigabyte before reading one filter byte.
fn fits_inflation_ratio(filtered_len: usize, idat_len: usize) -> bool {
    filtered_len <= idat_len.saturating_mul(INFLATION_RATIO)
}

/// Inflates the IDAT stream and counts the filter byte leading each scanline.
///
/// Every early return names its own reason, so a caller can tell a file this reader declined to
/// inflate from one whose compressed data is broken. Every other figure in the report is derived
/// from framing and IHDR, so it survives all of these.
fn scan_filters(
    header: &ihdr::Ihdr,
    idat: &[u8],
    filtered_len: usize,
    passes: &[PassStats],
    max_image_bytes: usize,
) -> FilterScan {
    if !fits_decode_budget(header, max_image_bytes) {
        return FilterScan::Skipped(SkippedFilterScan::OverBudget);
    }
    // The image fits the caller's budget; past the decoder's *default* budget the file still has
    // to be one that can plausibly inflate to it. Checked before `inflate_zlib` runs, because
    // `filtered_len` is the cap it would otherwise fill from a stream of any size. The floor is
    // stated over the header, like the budget, not over the filtered length: the two differ by
    // one filter byte per scanline, and an image exactly at the default budget must scan.
    if !fits_decode_budget(header, DEFAULT_MAX_IMAGE_BYTES)
        && !fits_inflation_ratio(filtered_len, idat.len())
    {
        return FilterScan::Skipped(SkippedFilterScan::ImplausibleInflation);
    }
    let Ok(stream) = inflate::inflate_zlib(idat, filtered_len) else {
        return FilterScan::Skipped(SkippedFilterScan::CorruptStream);
    };
    if stream.len() != filtered_len {
        return FilterScan::Skipped(SkippedFilterScan::LengthMismatch);
    }
    let mut counts = [0u32; 5];
    let mut at = 0usize;
    for pass in passes {
        for _ in 0..pass.height {
            // The pass geometry sums to `filtered_len`, which the stream just matched, so this
            // index is in range; a mismatch between the two is the same defect as a short stream.
            let Some(&code) = stream.get(at) else {
                return FilterScan::Skipped(SkippedFilterScan::LengthMismatch);
            };
            let Some(filter) = FilterType::from_code(code) else {
                return FilterScan::Skipped(SkippedFilterScan::UndefinedFilterCode);
            };
            counts[filter as usize] += 1;
            at += 1 + pass.row_bytes;
        }
    }
    FilterScan::Counted(FilterHistogram { counts })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ColorType;

    /// A report with the given segment ranges and file length. Built by hand because
    /// [`deconstruct`] cannot produce a malformed tiling: it is correct by construction, so every
    /// negative case for [`PngReport::is_fully_classified`] has to be assembled here. That is also
    /// why these live inline — the predicate is only falsifiable from inside the crate.
    fn report_with(ranges: &[(usize, usize)], file_len: usize) -> PngReport {
        PngReport {
            file_len,
            header: PngHeader {
                width: 1,
                height: 1,
                bit_depth: 8,
                color_type: ColorType::Truecolor,
                interlaced: false,
            },
            segments: ranges
                .iter()
                .map(|&(start, end)| Segment {
                    range: start..end,
                    kind: SegmentKind::Trailer,
                })
                .collect(),
            chunks: Vec::new(),
            idat_compressed: 0,
            filtered_len: 0,
            passes: Vec::new(),
            filters: FilterScan::Skipped(SkippedFilterScan::CorruptStream),
        }
    }

    #[test]
    fn contiguous_segments_covering_the_file_are_fully_classified() {
        assert!(report_with(&[(0, 8), (8, 20), (20, 33)], 33).is_fully_classified());
    }

    #[test]
    fn a_gap_between_segments_is_not_fully_classified() {
        // Every segment is non-empty and the last still reaches `file_len`, so only the
        // start-chaining half of the predicate can reject this.
        assert!(!report_with(&[(0, 8), (9, 33)], 33).is_fully_classified());
    }

    #[test]
    fn an_empty_segment_is_not_fully_classified() {
        // The mirror case: the chain is unbroken, so only the non-empty half can reject it.
        assert!(!report_with(&[(0, 8), (8, 8), (8, 33)], 33).is_fully_classified());
    }

    #[test]
    fn segments_must_start_at_zero_and_reach_the_end() {
        assert!(!report_with(&[(4, 33)], 33).is_fully_classified());
        assert!(!report_with(&[(0, 20)], 33).is_fully_classified());
        assert!(!report_with(&[], 33).is_fully_classified());
        // ...and a zero-length file with no segments is vacuously covered.
        assert!(report_with(&[], 0).is_fully_classified());
    }

    /// A header for the budget boundary, built directly: `ihdr::parse` would only add a byte
    /// layout between the test and the quantity under test.
    fn header(width: u32, height: u32, bit_depth: u8, color: ColorType) -> ihdr::Ihdr {
        ihdr::Ihdr {
            width,
            height,
            bit_depth,
            color,
            interlaced: false,
        }
    }

    #[test]
    fn the_decode_budget_is_inclusive_and_measures_the_decoded_image() {
        // 4096x4096 RGBA8 is exactly the decoder's default budget, so the walk must scan it. Its
        // *filtered* stream is 67 112 960 bytes — 4096 more, one filter byte per scanline — which
        // is how a cap stated over the filtered length came to decline an image that decodes.
        let at_budget = header(4096, 4096, 8, ColorType::TruecolorAlpha);
        assert!(fits_decode_budget(&at_budget, DEFAULT_MAX_IMAGE_BYTES));
        assert!(!fits_decode_budget(&at_budget, DEFAULT_MAX_IMAGE_BYTES - 1));
        assert!(fits_decode_budget(&at_budget, DEFAULT_MAX_IMAGE_BYTES + 1));
        // One pixel past the budget, at the same dimensions: the depth is the difference.
        assert!(!fits_decode_budget(
            &header(4096, 4096, 16, ColorType::TruecolorAlpha),
            DEFAULT_MAX_IMAGE_BYTES
        ));
        // A header whose decoded size overflows `usize` is declined, not wrapped.
        assert!(!fits_decode_budget(
            &header(0x7FFF_FFFF, 0x7FFF_FFFF, 16, ColorType::TruecolorAlpha),
            usize::MAX
        ));
    }

    #[test]
    fn only_a_refusal_to_read_is_not_damage() {
        // The single source of truth for `is_intact`'s filter conjunct: declining to inflate a
        // stream is a statement about what this reader will spend, everything else about the
        // file. Both refusals decline, for different reasons, and neither is damage.
        for reason in [
            SkippedFilterScan::OverBudget,
            SkippedFilterScan::ImplausibleInflation,
        ] {
            assert!(!reason.is_damage(), "{reason:?}");
            assert!(!FilterScan::Skipped(reason).is_damage(), "{reason:?}");
        }
        for reason in [
            SkippedFilterScan::CorruptStream,
            SkippedFilterScan::LengthMismatch,
            SkippedFilterScan::UndefinedFilterCode,
        ] {
            assert!(reason.is_damage(), "{reason:?}");
            assert!(FilterScan::Skipped(reason).is_damage(), "{reason:?}");
        }
        let counted = FilterScan::Counted(FilterHistogram {
            counts: [1, 0, 0, 0, 0],
        });
        assert!(!counted.is_damage(), "a scan that ran is never damage");
    }

    #[test]
    fn a_filter_scan_exposes_exactly_one_of_its_two_sides() {
        // Built here because `FilterHistogram`'s counts are private, so the `Counted` side is
        // only constructible from inside the crate.
        let histogram = FilterHistogram {
            counts: [1, 2, 0, 0, 0],
        };
        let counted = FilterScan::Counted(histogram);
        assert_eq!(counted.histogram(), Some(histogram));
        assert_eq!(counted.skipped(), None);

        let skipped = FilterScan::Skipped(SkippedFilterScan::OverBudget);
        assert_eq!(skipped.histogram(), None);
        assert_eq!(skipped.skipped(), Some(SkippedFilterScan::OverBudget));
    }

    #[test]
    fn the_skip_reasons_keep_their_published_discriminants() {
        // `#[repr(u8)]` plain data crossing the C ABI: these numbers are permanent and
        // append-only, so a variant is never renumbered or removed, only added after the last.
        assert_eq!(SkippedFilterScan::OverBudget as u8, 0);
        assert_eq!(SkippedFilterScan::CorruptStream as u8, 1);
        assert_eq!(SkippedFilterScan::LengthMismatch as u8, 2);
        assert_eq!(SkippedFilterScan::UndefinedFilterCode as u8, 3);
        assert_eq!(SkippedFilterScan::ImplausibleInflation as u8, 4);
    }

    #[test]
    fn an_overlap_is_not_fully_classified() {
        assert!(!report_with(&[(0, 20), (10, 33)], 33).is_fully_classified());
    }

    /// A PNG whose IHDR declares `width`×`height` RGBA8 over a zlib stream of `stream_len` zero
    /// bytes — a stream far too short for the header, which is the point: whether the walk
    /// inflates it at all is what the reason it reports tells apart.
    fn png_declaring(width: u32, height: u32, stream_len: usize) -> Vec<u8> {
        let mut idat = Vec::new();
        gamut_deflate::DeflateEncoder::new().zlib_compress(&vec![0u8; stream_len], &mut idat);
        let mut png = SIGNATURE.to_vec();
        ihdr::write(&mut png, width, height, 8, ColorType::TruecolorAlpha);
        crate::chunk::write_chunk(&mut png, *b"IDAT", &idat);
        crate::chunk::write_chunk(&mut png, *b"IEND", &[]);
        png
    }

    #[test]
    fn a_declared_gigabyte_over_a_small_stream_is_refused_before_inflation() {
        // 16384x16384 RGBA8 is exactly one gigabyte decoded, which `gamut inspect` budgets for
        // (its ceiling is 1 << 30). Under the header budget alone the walk hands that gigabyte
        // to `inflate_zlib` as the cap and a zlib bomb of zeros fills it from about a megabyte
        // of input. The stream here is tiny, so without the ratio bound the walk inflates it
        // completely and reports the *file's* `LengthMismatch`; with it, the walk reports its own
        // `ImplausibleInflation` and never inflates — the reason is the discriminator.
        let bomb = png_declaring(16384, 16384, 4096);
        let generous = DeconstructLimits::default().with_max_image_bytes(1 << 30);
        let report = deconstruct_with_limits(&bomb, generous).expect("deconstruct");
        assert_eq!(
            report.native_bytes(),
            Some(1 << 30),
            "precondition: the declared image is exactly the budget, so it is not over it"
        );
        assert_eq!(
            report.filters,
            FilterScan::Skipped(SkippedFilterScan::ImplausibleInflation),
            "a stream that would inflate to a gigabyte from four kilobytes is refused for its \
             ratio, not for exceeding a budget it exactly meets"
        );
        assert_eq!(
            report.filtered_len,
            16384 * (16384 * 4 + 1),
            "the header-derived figure is still reported"
        );
    }

    /// The index is what `record` answers "have I seen this type?" from, so it has to be
    /// complete and right: after any sequence of records it holds exactly one entry per distinct
    /// type, each mapping to the position in `stats` whose entry carries that type, and the
    /// counts show both arms of `record` ran. This pins the index's *content*; it does not by
    /// itself rule out a `record` that scans `stats` and also maintains the index — the probe
    /// count in `the_tally_probes_once_per_chunk_whatever_the_number_of_distinct_types` does.
    #[test]
    fn the_tally_index_names_every_recorded_type_at_its_position() {
        let mut tally = ChunkTally::new();
        let types: Vec<[u8; 4]> = (0..300u32).map(|i| i.to_be_bytes()).collect();
        for (i, ty) in types.iter().enumerate() {
            // Every type once, every third one a second time: both arms of `record`.
            tally.record(*ty, i);
            if i % 3 == 0 {
                tally.record(*ty, 1);
            }
        }
        assert_eq!(
            tally.index.len(),
            tally.stats.len(),
            "one index entry per distinct type"
        );
        assert_eq!(tally.stats.len(), types.len());
        for (at, stats) in tally.stats.iter().enumerate() {
            assert_eq!(
                tally.index.get(&stats.chunk_type),
                Some(&at),
                "type {:?} is indexed at its own position",
                stats.chunk_type
            );
            assert_eq!(stats.chunk_type, types[at], "first-appearance order");
            let repeated = at % 3 == 0;
            assert_eq!(stats.count, if repeated { 2 } else { 1 });
            assert_eq!(stats.payload_bytes, at + usize::from(repeated));
        }
    }

    /// The complexity claim itself, by count rather than by clock: N chunks cost N lookup
    /// probes however many distinct types they use. [`ChunkTally::lookup`] charges one probe per
    /// entry it examines, so a linear scan over `stats` — the defect the index replaced,
    /// quadratic in the number of distinct types — charges one per comparison and costs
    /// 2 096 128 probes here (measured, N²/2 to the entry), while the hash lookup charges exactly
    /// one per record. Two files of the same chunk count, one with every type distinct and one
    /// with a single type, must cost the same. Wall-clock timing of the same claim belongs to
    /// `benches/`.
    #[test]
    fn the_tally_probes_once_per_chunk_whatever_the_number_of_distinct_types() {
        const CHUNKS: usize = 2048;
        let mut distinct = ChunkTally::new();
        for i in 0..CHUNKS as u32 {
            distinct.record(i.to_be_bytes(), 0);
        }
        let mut repeated = ChunkTally::new();
        for _ in 0..CHUNKS {
            repeated.record(*b"crUD", 0);
        }
        assert_eq!(
            distinct.stats.len(),
            CHUNKS,
            "precondition: every type distinct"
        );
        assert_eq!(repeated.stats.len(), 1, "precondition: one type throughout");
        assert_eq!(
            distinct.probes, CHUNKS,
            "one probe per record with every type distinct"
        );
        assert_eq!(
            repeated.probes, CHUNKS,
            "and the same with one type: the count is O(N)"
        );
    }

    #[test]
    fn the_inflation_ratio_is_inclusive_and_saturates() {
        // A stream may inflate to exactly sixty-four times its length and not one byte more.
        assert!(fits_inflation_ratio(64 * 1000, 1000));
        assert!(!fits_inflation_ratio(64 * 1000 + 1, 1000));
        // An empty stream inflates to nothing.
        assert!(fits_inflation_ratio(0, 0));
        assert!(!fits_inflation_ratio(1, 0));
        // Overflow saturates rather than wrapping to a small allowance that would refuse every
        // huge stream.
        assert!(fits_inflation_ratio(usize::MAX, usize::MAX / 2));
    }

    #[test]
    fn an_image_inside_the_default_budget_is_scanned_whatever_its_ratio() {
        // A flat image compresses thousands-fold and is a real PNG: 1024x1024 RGBA8 from a
        // few dozen bytes of zlib. Its ratio is far past sixty-four, and it is inside the
        // decoder's default budget, so it is inflated — and this one is sound, so it is
        // counted. The floor is the header, not the filtered length: a scan refused here would
        // be the walk declining a file the decoder decodes.
        let side = 1024u32;
        let stream_len = side as usize * (side as usize * 4 + 1);
        let flat = png_declaring(side, side, stream_len);
        let report = deconstruct(&flat).expect("deconstruct");
        assert!(
            report.idat_compressed * INFLATION_RATIO < stream_len,
            "precondition: the fixture inflates by more than the ratio allows"
        );
        assert!(
            report.filters.is_counted(),
            "inside the default budget the ratio does not apply, got {:?}",
            report.filters
        );
    }
}
