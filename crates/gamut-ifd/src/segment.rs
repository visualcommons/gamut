//! Typed byte-range accounting: a segment map of the file, for strict "deconstruct" decoding.
//!
//! Ordinary decoding reads the structures it needs and ignores the rest. For archival / critical
//! use a stricter guarantee is wanted: that **every byte of the file maps to a typed structure**,
//! with anything left over surfaced rather than silently dropped. [`SegmentMap`] collects the
//! parser's claims — each byte range tagged with *what* it is ([`SpanKind`]) and *how* it was
//! claimed ([`Claim`]) — and [`finish`](SegmentMap::finish)es into a [`SegmentReport`].
//!
//! The report is **dual-ledger** checked when a read ledger is supplied (see
//! [`Tracked`](crate::Tracked)): every byte the parser physically read must fall inside some
//! claim ([`unclaimed_reads`](SegmentReport::unclaimed_reads) — a parser accounting defect, not
//! a file defect), and every [`Claim::Parsed`] claim must be covered by actual reads
//! ([`unread_claims`](SegmentReport::unread_claims)). Coverage thereby becomes a *proof* of what
//! the parse touched, not a promise.
//!
//! Two claims with **identical** extents dedupe as legal TIFF value sharing
//! ([`shared`](SegmentReport::shared)); partially overlapping claims are structural
//! [`conflicts`](SegmentReport::conflicts).

use gamut_core::Result;

use crate::source::ReadAt;
use crate::track::ReadLedger;

/// A half-open byte range `[start, start + len)` within a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Range {
    /// The offset of the first byte.
    pub start: u64,
    /// The number of bytes.
    pub len: u64,
}

impl Range {
    /// The offset one past the last byte (`start + len`, saturating).
    #[must_use]
    pub fn end(self) -> u64 {
        self.start.saturating_add(self.len)
    }
}

/// What a claimed byte range structurally *is*.
///
/// `#[non_exhaustive]`: codecs layered on this crate may need further kinds without a breaking
/// change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SpanKind {
    /// The 8-byte (classic) or 16-byte (BigTIFF) image file header.
    Header,
    /// A directory body — count field, entry records, and next-IFD pointer — at offset `ifd`.
    IfdBody {
        /// The file offset of the directory.
        ifd: u64,
    },
    /// The out-of-line value of entry `tag` in the directory at offset `ifd`.
    Value {
        /// The file offset of the directory holding the entry.
        ifd: u64,
        /// The entry's tag.
        tag: u16,
    },
    /// File data located by tag values (strips, tiles, an embedded JPEG, …) — see [`DataLabel`].
    Data(DataLabel),
    /// Word-alignment padding: all-zero filler between structures, either declared by the
    /// writer or classified by [`SegmentReport::classify_padding`].
    Padding,
    /// Bytes between the file header and the first structure it points at — a vendor preamble.
    /// Real writers put signatures here (Apple's ProRAW files carry the ASCII `APPLEDNG`
    /// immediately after the 8-byte TIFF header). Classified by
    /// [`SegmentReport::classify_unclaimed`].
    Preamble,
    /// Non-zero filler between two claimed structures that no tag accounts for — leftover bytes
    /// real writers leave behind when they rewrite a file in place. Distinct from
    /// [`Padding`](Self::Padding), which is all-zero and word-aligned. Classified by
    /// [`SegmentReport::classify_unclaimed`].
    Interstitial,
    /// A run of bytes appended after the last structure the file accounts for, reaching the end
    /// of the file. Classified by [`SegmentReport::classify_unclaimed`].
    Trailer,
}

/// What located a [`SpanKind::Data`] range.
///
/// `#[non_exhaustive]`: further structural data carriers can be named without a breaking change;
/// [`Other`](DataLabel::Other) covers codec-specific ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum DataLabel {
    /// `StripOffsets`/`StripByteCounts` pixel data.
    Strip,
    /// `TileOffsets`/`TileByteCounts` pixel data.
    Tile,
    /// `JPEGInterchangeFormat`/`JPEGInterchangeFormatLength` embedded JPEG data.
    JpegInterchange,
    /// The `MakerNote` vendor blob's out-of-line payload.
    MakerNote,
    /// `FreeOffsets`/`FreeByteCounts` declared dead space.
    Free,
    /// A codec-specific carrier, named by the codec.
    Other(&'static str),
}

/// How a byte range was claimed: whether the claimant actually fetched the bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Claim {
    /// The parser fetched and consumed these bytes — checked against the read ledger.
    Parsed,
    /// The extent was asserted from offset/length fields without fetching the bytes (strips,
    /// tiles, …) — a structural audit must not read gigabytes of pixel data to prove
    /// completeness, so these are exempt from the read-ledger check (but still bounds-checked).
    Declared,
}

/// One typed, claimed byte range of the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Segment {
    /// The claimed byte range.
    pub range: Range,
    /// What the range is.
    pub kind: SpanKind,
}

/// Two claims that partially overlap — a structure claiming bytes another already claimed,
/// which an archival validator treats as out-of-spec. (Claims with *identical* extents are
/// legal TIFF value sharing and land in [`SegmentReport::shared`] instead.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Conflict {
    /// The earlier-placed segment.
    pub a: Segment,
    /// The overlapping newcomer.
    pub b: Segment,
}

/// One byte range claimed by more than one structure with **identical** extents — TIFF permits
/// entries to share an out-of-line value, so this is informational, not a defect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedSpan {
    /// The shared byte range.
    pub range: Range,
    /// Every claimant's kind, in claim order.
    pub kinds: Vec<SpanKind>,
}

/// An accumulator of typed byte-range claims made while walking a file.
///
/// Claims are appended in parse order via [`claim`](Self::claim) and resolved once, in
/// [`finish`](Self::finish); there is no online query during parsing.
#[derive(Debug, Clone)]
pub struct SegmentMap {
    file_len: u64,
    claims: Vec<(Range, SpanKind, Claim)>,
}

impl SegmentMap {
    /// Creates an accumulator for a file of `file_len` bytes.
    #[must_use]
    pub fn new(file_len: u64) -> Self {
        Self {
            file_len,
            claims: Vec::new(),
        }
    }

    /// The file length this map accounts.
    #[must_use]
    pub fn file_len(&self) -> u64 {
        self.file_len
    }

    /// Claims `[start, start + len)` as `kind`. A zero-length claim is ignored. A range that
    /// reaches past the end of the file is reported in
    /// [`out_of_bounds`](SegmentReport::out_of_bounds) (as supplied); its in-bounds portion, if
    /// any, still counts toward classification.
    pub fn claim(&mut self, start: u64, len: u64, kind: SpanKind, provenance: Claim) {
        if len == 0 {
            return;
        }
        self.claims.push((Range { start, len }, kind, provenance));
    }

    /// Absorbs the claims of a map built over a [`Rebased`](crate::Rebased) view, shifting every
    /// offset (ranges and the offsets inside [`SpanKind`]) by `base` so they land at physical
    /// positions — how an embedded IFD stream's walk (a maker note, a DNG camera profile) joins
    /// the whole-file map.
    pub(crate) fn merge_shifted(&mut self, other: SegmentMap, base: u64) {
        for (range, kind, provenance) in other.claims {
            let kind = match kind {
                SpanKind::IfdBody { ifd } => SpanKind::IfdBody { ifd: ifd + base },
                SpanKind::Value { ifd, tag } => SpanKind::Value {
                    ifd: ifd + base,
                    tag,
                },
                other => other,
            };
            self.claims.push((
                Range {
                    start: range.start.saturating_add(base),
                    len: range.len,
                },
                kind,
                provenance,
            ));
        }
    }

    /// Resolves the claims into a [`SegmentReport`]: the typed segments, everything
    /// unclassified, identical-extent sharing, partial-overlap conflicts, and out-of-bounds
    /// claims — cross-checked against `reads` (the physical read ledger) when supplied.
    #[must_use]
    pub fn finish(self, reads: Option<&ReadLedger>) -> SegmentReport {
        let mut in_bounds: Vec<(Range, SpanKind, Claim)> = Vec::new();
        let mut out_of_bounds: Vec<Segment> = Vec::new();
        for (range, kind, provenance) in self.claims {
            if range.end() > self.file_len {
                // Record the offending claim as supplied for diagnostics, and clamp the
                // in-bounds part so the bytes that *were* valid still count.
                out_of_bounds.push(Segment { range, kind });
                if range.start < self.file_len {
                    in_bounds.push((
                        Range {
                            start: range.start,
                            len: self.file_len - range.start,
                        },
                        kind,
                        provenance,
                    ));
                }
            } else {
                in_bounds.push((range, kind, provenance));
            }
        }
        in_bounds.sort_by(|a, b| a.0.start.cmp(&b.0.start).then(a.0.end().cmp(&b.0.end())));

        let mut segments: Vec<Segment> = Vec::new();
        let mut conflicts: Vec<Conflict> = Vec::new();
        let mut shared: Vec<SharedSpan> = Vec::new();
        // The merged union of claimed bytes, for gap computation and the read-ledger subtract.
        let mut covered: Vec<Range> = Vec::new();
        for (i, (range, kind, _)) in in_bounds.iter().enumerate() {
            // Sorting makes identical extents consecutive: dedupe them as legal sharing.
            if i > 0 && in_bounds[i - 1].0 == *range {
                match shared.last_mut() {
                    Some(s) if s.range == *range => s.kinds.push(*kind),
                    _ => shared.push(SharedSpan {
                        range: *range,
                        kinds: vec![in_bounds[i - 1].1, *kind],
                    }),
                }
                continue;
            }
            if let Some(last) = covered.last_mut() {
                if range.start < last.end() {
                    // Partial overlap: out-of-spec double-claim. Sorted iteration guarantees
                    // every earlier segment starts at or before `range.start` (so it always
                    // begins before `range` ends); overlap is purely whether it reaches
                    // strictly past `range.start`.
                    let a = segments
                        .iter()
                        .rev()
                        .find(|s| s.range.end() > range.start)
                        .copied()
                        .unwrap_or(Segment {
                            range: *last,
                            kind: *kind,
                        });
                    conflicts.push(Conflict {
                        a,
                        b: Segment {
                            range: *range,
                            kind: *kind,
                        },
                    });
                    let new_end = last.end().max(range.end());
                    last.len = new_end - last.start;
                } else if range.start == last.end() {
                    // Adjacent structures: extend the union without flagging anything.
                    last.len = range.end() - last.start;
                } else {
                    covered.push(*range);
                }
            } else {
                covered.push(*range);
            }
            segments.push(Segment {
                range: *range,
                kind: *kind,
            });
        }

        // Everything not claimed — interior gaps and any trailing run — is unclassified.
        let mut unclassified = Vec::new();
        let mut cursor = 0u64;
        for r in &covered {
            if r.start > cursor {
                unclassified.push(Range {
                    start: cursor,
                    len: r.start - cursor,
                });
            }
            cursor = r.end();
        }
        if cursor < self.file_len {
            unclassified.push(Range {
                start: cursor,
                len: self.file_len - cursor,
            });
        }

        // The dual-ledger cross-check: reads ⊆ claims, and Parsed claims ⊆ reads.
        let (unclaimed_reads, unread_claims) = match reads {
            Some(ledger) => {
                let unclaimed = ledger.subtract(&covered);
                let unread = in_bounds
                    .iter()
                    .filter(|(range, _, provenance)| {
                        *provenance == Claim::Parsed && !ledger.contains(*range)
                    })
                    .map(|(range, kind, _)| Segment {
                        range: *range,
                        kind: *kind,
                    })
                    .collect();
                (unclaimed, unread)
            }
            None => (Vec::new(), Vec::new()),
        };

        SegmentReport {
            file_len: self.file_len,
            segments,
            unclassified,
            conflicts,
            shared,
            out_of_bounds,
            unclaimed_reads,
            unread_claims,
        }
    }
}

/// The result of resolving a [`SegmentMap`]: every byte of the file classified, or the precise
/// account of what was not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentReport {
    /// The total size of the file, in bytes.
    pub file_len: u64,
    /// Every claimed byte range, typed, sorted by start. When [`conflicts`](Self::conflicts) is
    /// non-empty, segments may overlap.
    pub segments: Vec<Segment>,
    /// Bytes in no claim: interior gaps and any trailing run, sorted.
    pub unclassified: Vec<Range>,
    /// Partially overlapping claims — out-of-spec double-claims.
    pub conflicts: Vec<Conflict>,
    /// Identical-extent multi-claims (legal TIFF value sharing) — informational.
    pub shared: Vec<SharedSpan>,
    /// Claims reaching past the end of the file.
    pub out_of_bounds: Vec<Segment>,
    /// Bytes physically read but never claimed — a **parser accounting defect**, not a file
    /// defect. Empty when no read ledger was supplied to [`SegmentMap::finish`].
    pub unclaimed_reads: Vec<Range>,
    /// [`Claim::Parsed`] claims not fully covered by the read ledger — likewise a parser
    /// defect. Empty when no read ledger was supplied.
    pub unread_claims: Vec<Segment>,
}

impl SegmentReport {
    /// Whether every byte of the file maps to exactly one typed segment and — when a read
    /// ledger was supplied — both dual-ledger invariants hold. **The archival verdict**, with
    /// no tolerance thresholds. ([`shared`](Self::shared) spans are legal and do not fail it.)
    #[must_use]
    pub fn is_fully_classified(&self) -> bool {
        self.unclassified.is_empty()
            && self.conflicts.is_empty()
            && self.out_of_bounds.is_empty()
            && self.unclaimed_reads.is_empty()
            && self.unread_claims.is_empty()
    }

    /// The number of unclassified bytes.
    #[must_use]
    pub fn unclassified_bytes(&self) -> u64 {
        self.unclassified.iter().map(|r| r.len).sum()
    }

    /// The offset one past the file header, when one was claimed — where a vendor preamble
    /// starts. Both classification passes anchor on it, so they agree on that position.
    fn header_end(&self) -> Option<u64> {
        self.segments
            .iter()
            .find(|s| s.kind == SpanKind::Header)
            .map(|s| s.range.end())
    }

    /// Reclassifies word-alignment padding out of [`unclassified`](Self::unclassified) by
    /// inspecting the actual bytes: an **all-zero** unclassified range that either ends on an
    /// even (word) boundary immediately before a following segment, or reaches the end of the
    /// file with at most one byte, becomes a [`SpanKind::Padding`] segment. Anything else —
    /// non-zero filler, or zeros in a structurally implausible place — stays unclassified,
    /// which is the correct archival signal.
    ///
    /// The one structural exception is the **preamble region**: the run from the end of the
    /// header to a directory body — the gap the header's first-IFD pointer skips over. Nothing
    /// on disk separates a zero-filled vendor preamble there from alignment filler (nor either
    /// from the filler byte an odd-length preamble carries *inside* it), so the whole run is left
    /// to [`classify_unclaimed`](Self::classify_unclaimed) to name a single [`SpanKind::Preamble`]
    /// — matching what a writer emitting a preamble declares.
    ///
    /// # Errors
    ///
    /// Returns [`gamut_core::Error`] if `src` fails while the range's bytes are inspected.
    pub fn classify_padding<S: ReadAt>(&mut self, src: &mut S) -> Result<()> {
        let mut starts: Vec<u64> = self.segments.iter().map(|s| s.range.start).collect();
        starts.sort_unstable();
        let mut directories: Vec<u64> = self
            .segments
            .iter()
            .filter(|s| matches!(s.kind, SpanKind::IfdBody { .. }))
            .map(|s| s.range.start)
            .collect();
        directories.sort_unstable();
        let header_end = self.header_end();
        let mut remaining = Vec::new();
        for range in std::mem::take(&mut self.unclassified) {
            let preamble_region =
                Some(range.start) == header_end && directories.binary_search(&range.end()).is_ok();
            let interior = range.end() % 2 == 0 && starts.binary_search(&range.end()).is_ok();
            let at_eof = range.end() == self.file_len && range.len <= 1;
            if !preamble_region && (interior || at_eof) && all_zero(src, range)? {
                self.segments.push(Segment {
                    range,
                    kind: SpanKind::Padding,
                });
            } else {
                remaining.push(range);
            }
        }
        self.unclassified = remaining;
        self.segments.sort_by_key(|s| (s.range.start, s.range.len));
        Ok(())
    }

    /// Names every remaining unclassified range by *where it sits*, rather than leaving real
    /// files permanently unaccounted: a range immediately following the header becomes
    /// [`SpanKind::Preamble`], one reaching the end of the file becomes [`SpanKind::Trailer`],
    /// and any other interior gap becomes [`SpanKind::Interstitial`].
    ///
    /// This is deliberately a separate, explicit pass — like [`classify_padding`](Self::
    /// classify_padding) — so that "every byte is accounted for" is never reached by accident.
    /// Run it *after* `classify_padding`, so all-zero word padding keeps its more specific kind.
    ///
    /// The bytes are not discarded: each becomes a typed [`Segment`] whose range still addresses
    /// the original file, and [`unclaimed_spans`](Self::unclaimed_spans) enumerates exactly the
    /// spans this pass named. A caller that wants the stricter verdict can assert on that list —
    /// which is what pins these ranges down per file, so a genuine parser gap cannot hide among
    /// them. The dual-ledger invariants ([`unclaimed_reads`](Self::unclaimed_reads) and
    /// [`unread_claims`](Self::unread_claims)) are untouched and still catch parser defects.
    pub fn classify_unclaimed(&mut self) {
        let header_end = self.header_end();
        for range in std::mem::take(&mut self.unclassified) {
            let kind = if Some(range.start) == header_end {
                SpanKind::Preamble
            } else if range.end() == self.file_len {
                SpanKind::Trailer
            } else {
                SpanKind::Interstitial
            };
            self.segments.push(Segment { range, kind });
        }
        self.segments.sort_by_key(|s| (s.range.start, s.range.len));
    }

    /// The spans [`classify_unclaimed`](Self::classify_unclaimed) named — every byte the file's
    /// own structures did not account for, typed by position and in file order.
    ///
    /// Empty before that pass runs. Assert on this to pin down exactly which unaccounted bytes a
    /// file is expected to carry.
    #[must_use]
    pub fn unclaimed_spans(&self) -> Vec<Segment> {
        self.segments
            .iter()
            .filter(|s| {
                matches!(
                    s.kind,
                    SpanKind::Preamble | SpanKind::Interstitial | SpanKind::Trailer
                )
            })
            .copied()
            .collect()
    }

    /// The total number of bytes [`unclaimed_spans`](Self::unclaimed_spans) covers.
    #[must_use]
    pub fn unclaimed_span_bytes(&self) -> u64 {
        self.unclaimed_spans().iter().map(|s| s.range.len).sum()
    }
}

/// Whether every byte of `range` in `src` is zero, read in bounded chunks.
fn all_zero<S: ReadAt>(src: &mut S, range: Range) -> Result<bool> {
    /// Bytes per read. The buffer is a stack array, so this also bounds the frame.
    const CHUNK: usize = 4096;
    let mut buf = [0u8; CHUNK];
    let end = range.end();
    // Stepping the range yields the chunk starts, so the range itself bounds the walk. A
    // hand-advanced cursor is bounded instead by its own arithmetic being right: `pos *= n`
    // never moves one that starts at zero, and the loop it drove could then be reported only
    // as a mutation-testing timeout, never as a wrong answer (issue #110).
    for pos in (range.start..end).step_by(CHUNK) {
        let n = usize::try_from((end - pos).min(CHUNK as u64)).unwrap_or(CHUNK);
        src.read_exact_at(pos, &mut buf[..n])?;
        if buf[..n].iter().any(|&b| b != 0) {
            return Ok(false);
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(start: u64, len: u64) -> SpanKind {
        let _ = (start, len);
        SpanKind::Header
    }

    fn report(file_len: u64, claims: &[(u64, u64)]) -> SegmentReport {
        let mut map = SegmentMap::new(file_len);
        for &(start, len) in claims {
            map.claim(start, len, seg(start, len), Claim::Parsed);
        }
        map.finish(None)
    }

    #[test]
    fn full_contiguous_claims_classify_cleanly() {
        let map = SegmentMap::new(10);
        assert_eq!(map.file_len(), 10);
        let r = report(10, &[(0, 4), (4, 6)]);
        assert!(r.is_fully_classified());
        assert_eq!(r.segments.len(), 2);
        assert_eq!(r.unclassified_bytes(), 0);
    }

    #[test]
    fn out_of_order_claims_are_sorted() {
        let r = report(10, &[(4, 6), (0, 4)]);
        assert!(r.is_fully_classified());
        assert_eq!(r.segments[0].range, Range { start: 0, len: 4 });
    }

    #[test]
    fn interior_gap_and_trailing_are_unclassified() {
        let r = report(20, &[(0, 5), (10, 6)]);
        assert_eq!(
            r.unclassified,
            vec![Range { start: 5, len: 5 }, Range { start: 16, len: 4 }]
        );
        assert!(!r.is_fully_classified());
        assert_eq!(r.unclassified_bytes(), 9);
    }

    #[test]
    fn identical_extents_are_legal_sharing_not_conflict() {
        let mut map = SegmentMap::new(10);
        map.claim(2, 8, SpanKind::Value { ifd: 0, tag: 1 }, Claim::Parsed);
        map.claim(2, 8, SpanKind::Value { ifd: 0, tag: 2 }, Claim::Parsed);
        map.claim(0, 2, SpanKind::Header, Claim::Parsed);
        let r = map.finish(None);
        assert!(r.conflicts.is_empty());
        assert_eq!(r.shared.len(), 1);
        assert_eq!(r.shared[0].range, Range { start: 2, len: 8 });
        assert_eq!(r.shared[0].kinds.len(), 2);
        // Shared bytes count once; the report is still fully classified.
        assert!(r.is_fully_classified());
        assert_eq!(r.segments.len(), 2);
    }

    #[test]
    fn three_identical_extents_accumulate_kinds() {
        let mut map = SegmentMap::new(4);
        for tag in 1..=3 {
            map.claim(0, 4, SpanKind::Value { ifd: 0, tag }, Claim::Parsed);
        }
        let r = map.finish(None);
        assert_eq!(r.shared.len(), 1);
        assert_eq!(r.shared[0].kinds.len(), 3);
        assert!(r.is_fully_classified());
    }

    /// Overlap and adjacency union-extension away from offset 0, with exact unclassified
    /// extents — `new_end - last.start` and `new_end + last.start` coincide when
    /// `last.start == 0`, so a non-zero start is what actually pins the merge arithmetic.
    #[test]
    fn merges_away_from_origin_have_exact_extents() {
        let r = report(20, &[(5, 6), (8, 6)]); // overlap on [8, 11)
        assert_eq!(r.conflicts.len(), 1);
        assert_eq!(
            r.unclassified,
            vec![Range { start: 0, len: 5 }, Range { start: 14, len: 6 }]
        );

        let r = report(20, &[(5, 5), (10, 5)]); // adjacent at 10
        assert!(r.conflicts.is_empty());
        assert_eq!(
            r.unclassified,
            vec![Range { start: 0, len: 5 }, Range { start: 15, len: 5 }]
        );
    }

    /// The conflict names the actual overlapped segment, not merely the union so far.
    #[test]
    fn conflict_names_the_overlapped_segment() {
        // (6, 2) overlaps the *second* segment (4, 4); the union-so-far is [0, 8), so a wrong
        // pick is distinguishable.
        let r = report(10, &[(0, 4), (4, 4), (6, 2)]);
        assert_eq!(r.conflicts.len(), 1);
        assert_eq!(r.conflicts[0].a.range, Range { start: 4, len: 4 });
        assert_eq!(r.conflicts[0].b.range, Range { start: 6, len: 2 });
    }

    /// The conflict skips an adjacent (non-overlapping) later segment and names the earlier
    /// segment that actually reaches past the newcomer's start.
    #[test]
    fn conflict_skips_an_adjacent_red_herring() {
        // (6, 2) nests in (0, 10); then (8, 4) overlaps (0, 10) — but (6, 2), the most recent
        // segment, merely *touches* offset 8 and must not be named.
        let r = report(20, &[(0, 10), (6, 2), (8, 4)]);
        assert_eq!(r.conflicts.len(), 2);
        assert_eq!(r.conflicts[1].b.range, Range { start: 8, len: 4 });
        assert_eq!(r.conflicts[1].a.range, Range { start: 0, len: 10 });
    }

    /// Two *distinct* shared extents produce two separate `SharedSpan`s, each accumulating only
    /// its own claimants.
    #[test]
    fn distinct_shared_spans_stay_separate() {
        let mut map = SegmentMap::new(8);
        for tag in [1u16, 2] {
            map.claim(0, 4, SpanKind::Value { ifd: 0, tag }, Claim::Parsed);
        }
        for tag in [3u16, 4] {
            map.claim(4, 4, SpanKind::Value { ifd: 0, tag }, Claim::Parsed);
        }
        let r = map.finish(None);
        assert_eq!(r.shared.len(), 2, "{:?}", r.shared);
        assert!(r.shared.iter().all(|s| s.kinds.len() == 2));
        assert!(r.is_fully_classified());
    }

    #[test]
    fn partial_overlap_is_a_conflict() {
        let r = report(10, &[(0, 6), (4, 6)]);
        assert_eq!(r.conflicts.len(), 1);
        assert_eq!(r.conflicts[0].a.range, Range { start: 0, len: 6 });
        assert_eq!(r.conflicts[0].b.range, Range { start: 4, len: 6 });
        assert!(!r.is_fully_classified());
        // The union still covers the file: nothing unclassified, but the conflict fails it.
        assert!(r.unclassified.is_empty());
    }

    #[test]
    fn nested_range_is_a_conflict() {
        let r = report(10, &[(0, 10), (2, 3)]);
        assert_eq!(r.conflicts.len(), 1);
        assert!(r.unclassified.is_empty());
    }

    #[test]
    fn out_of_bounds_claim_is_recorded_and_clamped() {
        let r = report(10, &[(0, 8), (8, 6)]);
        assert_eq!(r.out_of_bounds.len(), 1);
        assert_eq!(r.out_of_bounds[0].range, Range { start: 8, len: 6 });
        // The in-bounds portion [8, 10) still counts toward classification — clamped exactly.
        assert!(r.unclassified.is_empty());
        assert!(
            r.segments.contains(&Segment {
                range: Range { start: 8, len: 2 },
                kind: seg(8, 6),
            }),
            "{:?}",
            r.segments
        );
        assert!(!r.is_fully_classified());
    }

    /// A claim starting exactly at the file end has no in-bounds part: no phantom zero-length
    /// segment appears, and the whole file stays unclassified.
    #[test]
    fn claim_at_exactly_file_end_is_out_of_bounds_only() {
        let r = report(10, &[(10, 4)]);
        assert_eq!(r.out_of_bounds.len(), 1);
        assert!(r.segments.is_empty(), "{:?}", r.segments);
        assert_eq!(r.unclassified, vec![Range { start: 0, len: 10 }]);
    }

    #[test]
    fn fully_out_of_bounds_claim_covers_nothing() {
        let r = report(10, &[(0, 10), (20, 4)]);
        assert_eq!(r.out_of_bounds.len(), 1);
        assert!(!r.is_fully_classified());
    }

    #[test]
    fn zero_length_claims_are_ignored() {
        let r = report(10, &[(0, 10), (5, 0)]);
        assert!(r.is_fully_classified());
        assert_eq!(r.segments.len(), 1);
    }

    #[test]
    fn empty_map_is_all_unclassified() {
        let r = report(10, &[]);
        assert_eq!(r.unclassified, vec![Range { start: 0, len: 10 }]);
        assert_eq!(r.unclassified_bytes(), 10);
    }

    #[test]
    fn ledger_flags_unclaimed_reads() {
        use crate::track::ReadLedger;
        let mut map = SegmentMap::new(10);
        map.claim(0, 4, SpanKind::Header, Claim::Parsed);
        let mut ledger = ReadLedger::default();
        ledger.record(0, 4); // the claimed read
        ledger.record(6, 2); // a read nothing claimed — a parser defect
        let r = map.finish(Some(&ledger));
        assert_eq!(r.unclaimed_reads, vec![Range { start: 6, len: 2 }]);
        assert!(r.unread_claims.is_empty());
        assert!(!r.is_fully_classified());
    }

    #[test]
    fn ledger_flags_unread_parsed_claims_but_not_declared() {
        use crate::track::ReadLedger;
        let mut map = SegmentMap::new(20);
        map.claim(0, 4, SpanKind::Header, Claim::Parsed);
        // A Parsed claim whose bytes were never read: a parser defect.
        map.claim(4, 4, SpanKind::IfdBody { ifd: 4 }, Claim::Parsed);
        // A Declared claim is exempt — strips are asserted, not fetched.
        map.claim(8, 12, SpanKind::Data(DataLabel::Strip), Claim::Declared);
        let mut ledger = ReadLedger::default();
        ledger.record(0, 4);
        let r = map.finish(Some(&ledger));
        assert_eq!(r.unread_claims.len(), 1);
        assert_eq!(r.unread_claims[0].range, Range { start: 4, len: 4 });
        assert!(r.unclaimed_reads.is_empty());
        assert!(!r.is_fully_classified());
    }

    #[test]
    fn clean_dual_ledger_is_fully_classified() {
        use crate::track::ReadLedger;
        let mut map = SegmentMap::new(12);
        map.claim(0, 8, SpanKind::Header, Claim::Parsed);
        map.claim(8, 4, SpanKind::Data(DataLabel::Strip), Claim::Declared);
        let mut ledger = ReadLedger::default();
        ledger.record(0, 8);
        let r = map.finish(Some(&ledger));
        assert!(r.is_fully_classified(), "report: {r:?}");
    }

    #[test]
    fn classify_padding_takes_only_plausible_zero_runs() {
        // Layout: [0,7) header | 7 one zero pad byte | [8,12) value | 12.. trailing junk.
        let data: &[u8] = &[1, 1, 1, 1, 1, 1, 1, 0, 2, 2, 2, 2, 9, 9];
        let mut map = SegmentMap::new(data.len() as u64);
        map.claim(0, 7, SpanKind::Header, Claim::Parsed);
        map.claim(8, 4, SpanKind::Value { ifd: 0, tag: 1 }, Claim::Parsed);
        let mut r = map.finish(None);
        assert_eq!(r.unclassified.len(), 2);
        let mut src = data;
        r.classify_padding(&mut src).expect("classify");
        // The zero byte at 7 (even end, before the value segment) became padding.
        assert!(
            r.segments.contains(&Segment {
                range: Range { start: 7, len: 1 },
                kind: SpanKind::Padding
            }),
            "segments: {:?}",
            r.segments
        );
        // The non-zero trailing junk stays unclassified.
        assert_eq!(r.unclassified, vec![Range { start: 12, len: 2 }]);
        assert!(!r.is_fully_classified());
    }

    #[test]
    fn classify_padding_rejects_nonzero_and_odd_ended_gaps() {
        // A non-zero gap byte at an otherwise plausible position stays unclassified.
        let nonzero: &[u8] = &[1, 1, 1, 1, 1, 1, 1, 5, 2, 2, 2, 2];
        let mut map = SegmentMap::new(nonzero.len() as u64);
        map.claim(0, 7, SpanKind::Header, Claim::Parsed);
        map.claim(8, 4, SpanKind::Value { ifd: 0, tag: 1 }, Claim::Parsed);
        let mut r = map.finish(None);
        let mut src = nonzero;
        r.classify_padding(&mut src).expect("classify");
        assert_eq!(r.unclassified, vec![Range { start: 7, len: 1 }]);

        // An all-zero gap ending on an odd boundary is not word alignment: stays unclassified.
        let odd_end: &[u8] = &[1, 1, 1, 1, 1, 1, 0, 0, 0, 2, 2, 2];
        let mut map = SegmentMap::new(odd_end.len() as u64);
        map.claim(0, 6, SpanKind::Header, Claim::Parsed);
        map.claim(9, 3, SpanKind::Value { ifd: 0, tag: 1 }, Claim::Parsed);
        let mut r = map.finish(None);
        let mut src = odd_end;
        r.classify_padding(&mut src).expect("classify");
        assert_eq!(r.unclassified, vec![Range { start: 6, len: 3 }]);
    }

    /// The header/first-directory gap is the preamble region: an all-zero run there is a
    /// zero-filled vendor preamble (or a preamble plus its alignment filler), indistinguishable
    /// on disk from either, so the padding pass must leave it whole for `classify_unclaimed`.
    /// Both halves of the rule matter: a zero run that starts after the header but ends at
    /// something other than a directory, or ends at a directory without starting at the header,
    /// is ordinary alignment padding.
    #[test]
    fn classify_padding_leaves_the_preamble_region_to_classify_unclaimed() {
        // [0,4) header | [4,8) all-zero preamble | [8,12) directory body.
        let data: &[u8] = &[1, 1, 1, 1, 0, 0, 0, 0, 2, 2, 2, 2];
        let mut map = SegmentMap::new(data.len() as u64);
        map.claim(0, 4, SpanKind::Header, Claim::Parsed);
        map.claim(8, 4, SpanKind::IfdBody { ifd: 8 }, Claim::Parsed);
        let mut r = map.finish(None);
        let mut src = data;
        r.classify_padding(&mut src).expect("classify");
        assert_eq!(
            r.unclassified,
            vec![Range { start: 4, len: 4 }],
            "the preamble region is not padding: {r:?}"
        );
        r.classify_unclaimed();
        assert_eq!(
            r.unclaimed_spans(),
            vec![Segment {
                range: Range { start: 4, len: 4 },
                kind: SpanKind::Preamble,
            }]
        );

        // Same zero run, but the following structure is a value rather than a directory body:
        // ordinary alignment padding.
        let mut map = SegmentMap::new(data.len() as u64);
        map.claim(0, 4, SpanKind::Header, Claim::Parsed);
        map.claim(8, 4, SpanKind::Value { ifd: 0, tag: 1 }, Claim::Parsed);
        let mut r = map.finish(None);
        let mut src = data;
        r.classify_padding(&mut src).expect("classify");
        assert!(r.is_fully_classified(), "report: {r:?}");
        assert!(r.segments.contains(&Segment {
            range: Range { start: 4, len: 4 },
            kind: SpanKind::Padding,
        }));

        // A zero run before a directory body that does *not* start at the header's end is
        // alignment padding too.
        let interior: &[u8] = &[1, 1, 1, 1, 3, 3, 0, 0, 2, 2, 2, 2];
        let mut map = SegmentMap::new(interior.len() as u64);
        map.claim(0, 4, SpanKind::Header, Claim::Parsed);
        map.claim(4, 2, SpanKind::Value { ifd: 0, tag: 1 }, Claim::Parsed);
        map.claim(8, 4, SpanKind::IfdBody { ifd: 8 }, Claim::Parsed);
        let mut r = map.finish(None);
        let mut src = interior;
        r.classify_padding(&mut src).expect("classify");
        assert!(r.is_fully_classified(), "report: {r:?}");
        assert!(r.segments.contains(&Segment {
            range: Range { start: 6, len: 2 },
            kind: SpanKind::Padding,
        }));
    }

    /// The three positions a real camera file leaves bytes in — right after the header, between
    /// structures, and appended at the end — each get their own kind, and the archival verdict
    /// only then holds.
    #[test]
    fn classify_unclaimed_names_gaps_by_position() {
        // [0,4) header | [4,6) preamble | [6,10) value | [10,12) interstitial | [12,16) value |
        // [16,20) trailer.
        let mut map = SegmentMap::new(20);
        map.claim(0, 4, SpanKind::Header, Claim::Parsed);
        map.claim(6, 4, SpanKind::Value { ifd: 0, tag: 1 }, Claim::Parsed);
        map.claim(12, 4, SpanKind::Value { ifd: 0, tag: 2 }, Claim::Parsed);
        let mut r = map.finish(None);
        assert_eq!(r.unclassified.len(), 3);
        assert!(r.unclaimed_spans().is_empty(), "not named before the pass");

        r.classify_unclaimed();

        assert!(r.unclassified.is_empty());
        assert!(r.is_fully_classified(), "report: {r:?}");
        assert_eq!(
            r.unclaimed_spans(),
            vec![
                Segment {
                    range: Range { start: 4, len: 2 },
                    kind: SpanKind::Preamble
                },
                Segment {
                    range: Range { start: 10, len: 2 },
                    kind: SpanKind::Interstitial
                },
                Segment {
                    range: Range { start: 16, len: 4 },
                    kind: SpanKind::Trailer
                },
            ]
        );
        assert_eq!(r.unclaimed_span_bytes(), 8);
    }

    /// Without a header segment there is no preamble position, so a leading gap is interstitial —
    /// the pass must not guess that offset 0 means "after the header".
    #[test]
    fn classify_unclaimed_needs_a_header_to_call_a_gap_a_preamble() {
        let mut map = SegmentMap::new(12);
        map.claim(4, 4, SpanKind::Value { ifd: 0, tag: 1 }, Claim::Parsed);
        let mut r = map.finish(None);
        r.classify_unclaimed();
        assert_eq!(
            r.unclaimed_spans()
                .iter()
                .map(|s| s.kind)
                .collect::<Vec<_>>(),
            vec![SpanKind::Interstitial, SpanKind::Trailer],
        );
    }

    /// The pass names positions; it must never paper over a parser defect, which the dual-ledger
    /// invariants report separately and which still fails the verdict.
    #[test]
    fn classify_unclaimed_does_not_mask_parser_defects() {
        let mut map = SegmentMap::new(12);
        map.claim(0, 4, SpanKind::Header, Claim::Parsed);
        // The parser read [8,12) but claimed nothing there.
        let mut ledger = ReadLedger::new();
        ledger.record(8, 4);
        let mut r = map.finish(Some(&ledger));
        r.classify_unclaimed();
        assert!(r.unclassified.is_empty());
        assert!(
            !r.unclaimed_reads.is_empty(),
            "the read ledger still reports the defect"
        );
        assert!(!r.is_fully_classified(), "report: {r:?}");
    }

    #[test]
    fn classify_padding_accepts_single_zero_at_eof() {
        let data: &[u8] = &[1, 1, 1, 1, 0];
        let mut map = SegmentMap::new(data.len() as u64);
        map.claim(0, 4, SpanKind::Header, Claim::Parsed);
        let mut r = map.finish(None);
        let mut src = data;
        r.classify_padding(&mut src).expect("classify");
        assert!(r.is_fully_classified(), "report: {r:?}");
        // But a multi-byte zero tail is not writer padding.
        let long_tail: &[u8] = &[1, 1, 1, 1, 0, 0];
        let mut map = SegmentMap::new(long_tail.len() as u64);
        map.claim(0, 4, SpanKind::Header, Claim::Parsed);
        let mut r = map.finish(None);
        let mut src = long_tail;
        r.classify_padding(&mut src).expect("classify");
        assert_eq!(r.unclassified, vec![Range { start: 4, len: 2 }]);
    }

    /// The degenerate whole-file case: a 1-byte all-zero file with no claims classifies as a
    /// single at-EOF padding byte — and the chunked zero-scan must start from the range's own
    /// offset (a scan anchored anywhere else divides by zero or misreads here).
    #[test]
    fn classify_padding_handles_a_zero_claim_file() {
        let data: &[u8] = &[0];
        let mut r = SegmentMap::new(1).finish(None);
        assert_eq!(r.unclassified, vec![Range { start: 0, len: 1 }]);
        let mut src = data;
        r.classify_padding(&mut src).expect("classify");
        assert!(r.is_fully_classified(), "report: {r:?}");
        assert_eq!(
            r.segments,
            vec![Segment {
                range: Range { start: 0, len: 1 },
                kind: SpanKind::Padding,
            }]
        );
    }

    /// A multi-byte all-zero interior gap ending on a word boundary before a segment *is*
    /// classifiable (pinning that padding length is not restricted to one byte).
    #[test]
    fn classify_padding_accepts_multibyte_interior_zeros() {
        let data: &[u8] = &[1, 1, 1, 0, 0, 0, 2, 2, 2, 2];
        let mut map = SegmentMap::new(data.len() as u64);
        map.claim(0, 3, SpanKind::Header, Claim::Parsed);
        map.claim(6, 4, SpanKind::Value { ifd: 0, tag: 1 }, Claim::Parsed);
        let mut r = map.finish(None);
        let mut src = data;
        r.classify_padding(&mut src).expect("classify");
        assert!(r.is_fully_classified(), "report: {r:?}");
    }
}
