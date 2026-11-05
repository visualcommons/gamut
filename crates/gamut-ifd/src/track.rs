//! The physical read ledger: proof of what a parse actually touched.
//!
//! [`Tracked`] wraps any [`ReadAt`] source and records every successful read into a
//! [`ReadLedger`] — a coalesced set of the bytes physically fetched. Cross-checking that ledger
//! against the parser's structural claims ([`SegmentMap::finish`](crate::SegmentMap::finish))
//! turns byte accounting from a promise into a machine-checked invariant: a parse path that
//! reads bytes it never claims, or claims bytes it never read, is caught mechanically.
//!
//! Wrap the **outermost physical** source (`Tracked<StreamSource<File>>`, `Tracked<&[u8]>`);
//! [`Rebased`](crate::Rebased) views layered on `&mut Tracked<…>` delegate down, so reads made
//! through a rebased view (a maker-note mini-IFD) land in the ledger at **physical** offsets.

use core::ops::Range;

use gamut_core::Result;

use crate::source::ReadAt;

/// A sorted, coalesced set of byte ranges that were physically read.
///
/// Re-reads merge silently — the ledger is a set of bytes touched, not a log of operations —
/// so re-fetching a value is never mistaken for a double-claim.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReadLedger {
    /// Disjoint, non-adjacent, sorted by start.
    spans: Vec<Range<u64>>,
}

impl ReadLedger {
    /// Creates an empty ledger.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records that `[start, start + len)` was read, merging with overlapping or adjacent
    /// spans. A zero-length read records nothing.
    pub fn record(&mut self, start: u64, len: u64) {
        if len == 0 {
            return;
        }
        let end = start.saturating_add(len);
        // First span that could merge: the earliest whose end reaches `start` (adjacency
        // included).
        let i = self.spans.partition_point(|s| s.end < start);
        if i == self.spans.len() || self.spans[i].start > end {
            self.spans.insert(i, start..end);
            return;
        }
        let new_start = self.spans[i].start.min(start);
        // The spans that merge are the run from `i` whose starts still reach `end`; taking that
        // run as a sub-slice is what bounds the walk. A hand-advanced index is bounded by its own
        // arithmetic instead, and the mutant that stalls that arithmetic -- `j *= 1` never
        // advances an index of zero -- makes the loop spin instead of finishing wrong, so the
        // escape is reportable only as a mutation-testing timeout and no test can assert on it
        // (issue #110).
        let merge_len = self.spans[i..]
            .iter()
            .take_while(|s| s.start <= end)
            .count();
        let j = i + merge_len;
        let new_end = self.spans[i..j].iter().fold(end, |e, s| e.max(s.end));
        self.spans
            .splice(i..j, core::iter::once(new_start..new_end));
    }

    /// The recorded spans: disjoint, non-adjacent, sorted by start.
    #[must_use]
    pub fn spans(&self) -> &[Range<u64>] {
        &self.spans
    }

    /// Whether every byte of `range` was read. (Spans are coalesced, so containment can only
    /// hold within a single span.) An empty range is contained.
    #[must_use]
    pub fn contains(&self, range: &Range<u64>) -> bool {
        if range.is_empty() {
            return true;
        }
        let i = self.spans.partition_point(|s| s.start <= range.start);
        i > 0 && self.spans[i - 1].end >= range.end
    }

    /// The bytes read but **not** covered by `claims` — the parser-defect signal
    /// ([`SegmentReport::unclaimed_reads`](crate::SegmentReport::unclaimed_reads)). `claims`
    /// need not be sorted or disjoint; it is normalised internally.
    #[must_use]
    pub fn subtract(&self, claims: &[Range<u64>]) -> Vec<Range<u64>> {
        // Normalise: sort and merge (overlap and adjacency both coalesce).
        let mut merged: Vec<Range<u64>> = Vec::with_capacity(claims.len());
        let mut sorted: Vec<Range<u64>> =
            claims.iter().filter(|r| r.end > r.start).cloned().collect();
        sorted.sort_by_key(|r| (r.start, r.end));
        for r in sorted {
            match merged.last_mut() {
                Some(last) if r.start <= last.end => last.end = last.end.max(r.end),
                _ => merged.push(r),
            }
        }
        // Subtract `merged` from each ledger span. Both lists are sorted and disjoint, so the
        // claims that can touch one span are a contiguous run of `merged`: the run that reaches
        // past the span's start, up to the last that begins before its end. Taking that run as a
        // sub-slice is what bounds the walk. A cursor advanced by hand is bounded by its own
        // arithmetic instead, and the mutants of that arithmetic split two ways: one that stalls
        // the cursor (`pos *= n` never moves one that starts at zero) spins instead of finishing,
        // and is reportable only as a mutation-testing timeout, while one that merely mis-steps
        // it returns a wrong answer a test can pin. Only the first kind is unkillable, and
        // bounding the walk by the data removes it (issue #110).
        let mut out = Vec::new();
        for span in &self.spans {
            let end = span.end;
            let live = &merged[merged.partition_point(|r| r.end <= span.start)..];
            let touching = live.iter().take_while(|r| r.start < end).count();
            // Each claim closes the stretch before it and moves the cursor to its own end,
            // clamped to the span; whatever is left over after the run is the trailing stretch.
            let mut pos = span.start;
            for r in &live[..touching] {
                push_gap(&mut out, pos, r.start);
                pos = r.end.min(end);
            }
            push_gap(&mut out, pos, end);
        }
        out
    }
}

/// Records `[start, stop)` as unclaimed, unless it is empty.
///
/// One guard serves both the stretch before a claim and the one after the last claim, so the
/// emptiness test is written once. `stop` may be *below* `start` -- a claim reaching back before
/// the span it is subtracted from starts there -- which is why the test is `>` and not `!=`.
fn push_gap(out: &mut Vec<Range<u64>>, start: u64, stop: u64) {
    if stop > start {
        out.push(start..stop);
    }
}

/// A [`ReadAt`] adaptor that records every successful read into a [`ReadLedger`].
///
/// ```
/// use gamut_ifd::{ReadAt, Tracked};
///
/// let data: &[u8] = &[1, 2, 3, 4, 5, 6];
/// let mut tracked = Tracked::new(data);
/// let mut buf = [0u8; 2];
/// tracked.read_exact_at(1, &mut buf).unwrap();
/// tracked.read_exact_at(3, &mut buf).unwrap(); // adjacent: coalesces
/// assert_eq!(tracked.ledger().spans().len(), 1);
/// ```
#[derive(Debug)]
pub struct Tracked<S> {
    inner: S,
    ledger: ReadLedger,
}

impl<S> Tracked<S> {
    /// Wraps `inner`, starting with an empty ledger.
    #[must_use]
    pub fn new(inner: S) -> Self {
        Self {
            inner,
            ledger: ReadLedger::new(),
        }
    }

    /// The ledger of bytes read so far.
    #[must_use]
    pub fn ledger(&self) -> &ReadLedger {
        &self.ledger
    }

    /// Unwraps into the inner source and the final ledger.
    #[must_use]
    pub fn into_parts(self) -> TrackedParts<S> {
        TrackedParts {
            source: self.inner,
            ledger: self.ledger,
        }
    }
}

/// What [`Tracked::into_parts`] returns: the unwrapped source and every byte read through it.
#[derive(Debug)]
pub struct TrackedParts<S> {
    /// The source [`Tracked::new`] wrapped.
    pub source: S,
    /// The final ledger of bytes read.
    pub ledger: ReadLedger,
}

impl<S: ReadAt> ReadAt for Tracked<S> {
    fn read_exact_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        // Record only successful reads: a failed bounds check touched nothing meaningful.
        self.inner.read_exact_at(offset, buf)?;
        self.ledger.record(offset, buf.len() as u64);
        Ok(())
    }

    fn len(&mut self) -> Result<u64> {
        self.inner.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Half-open ranges from `(start, len)` pairs.
    fn spans(pairs: &[(u64, u64)]) -> Vec<Range<u64>> {
        pairs
            .iter()
            .map(|&(start, len)| start..start + len)
            .collect()
    }

    #[test]
    fn record_merges_overlap_adjacency_and_order() {
        let mut ledger = ReadLedger::new();
        ledger.record(10, 5); // [10, 15)
        ledger.record(0, 4); // [0, 4) — before, disjoint
        ledger.record(4, 2); // adjacent: [0, 6)
        ledger.record(12, 6); // overlap: [10, 18)
        ledger.record(6, 4); // bridges [0,6) and [10,18)? no — [6,10) is adjacent to both
        assert_eq!(ledger.spans(), spans(&[(0, 18)]));
        ledger.record(30, 0); // zero-length: ignored
        assert_eq!(ledger.spans().len(), 1);
    }

    /// A read ending exactly where an existing span begins is adjacency and must coalesce —
    /// pinning the disjoint-insert boundary from the *before* side.
    #[test]
    fn record_adjacent_before_an_existing_span_coalesces() {
        let mut ledger = ReadLedger::new();
        ledger.record(10, 5); // [10, 15)
        ledger.record(6, 4); // [6, 10) — ends exactly at the span's start
        assert_eq!(ledger.spans(), spans(&[(6, 9)]));
    }

    #[test]
    fn record_bridging_multiple_spans_coalesces_them() {
        let mut ledger = ReadLedger::new();
        ledger.record(0, 2);
        ledger.record(4, 2);
        ledger.record(8, 2);
        assert_eq!(ledger.spans().len(), 3);
        ledger.record(1, 8); // covers the gaps: one span [0, 10)
        assert_eq!(ledger.spans(), spans(&[(0, 10)]));
    }

    #[test]
    fn contains_requires_full_coverage() {
        let mut ledger = ReadLedger::new();
        ledger.record(4, 6); // [4, 10)
        assert!(ledger.contains(&(4..10)));
        assert!(ledger.contains(&(5..7)));
        assert!(!ledger.contains(&(3..5)));
        assert!(!ledger.contains(&(8..12)));
        assert!(!ledger.contains(&(20..21)));
        assert!(ledger.contains(&(0..0)), "empty range");
    }

    #[test]
    fn subtract_reports_exactly_the_unclaimed_reads() {
        let mut ledger = ReadLedger::new();
        ledger.record(0, 10); // [0, 10)
        ledger.record(20, 5); // [20, 25)
        // Claims cover [0,4) and [6,22) — unclaimed: [4,6) and [22,25).
        let unclaimed = ledger.subtract(&spans(&[(0, 4), (6, 16)]));
        assert_eq!(unclaimed, spans(&[(4, 2), (22, 3)]));
        // Full coverage: nothing unclaimed. Unsorted, overlapping claims are normalised.
        let unclaimed = ledger.subtract(&spans(&[(18, 10), (0, 12), (5, 10)]));
        assert!(unclaimed.is_empty());
        // No claims at all: everything read is unclaimed.
        assert_eq!(ledger.subtract(&[]), spans(&[(0, 10), (20, 5)]));
    }

    /// Claim normalisation merges away from offset 0 with exact extents (`new_end - last.start`
    /// degenerates when `last.start == 0`), and the merged length bounds the subtraction.
    #[test]
    fn subtract_normalises_overlapping_claims_away_from_origin() {
        let mut ledger = ReadLedger::new();
        ledger.record(0, 40);
        // Overlapping claims [10, 20) + [15, 25) normalise to [10, 25).
        let unclaimed = ledger.subtract(&spans(&[(10, 10), (15, 10)]));
        assert_eq!(unclaimed, spans(&[(0, 10), (25, 15)]));
    }

    #[test]
    fn tracked_records_only_successful_reads() {
        let data: &[u8] = &[1, 2, 3, 4];
        let mut tracked = Tracked::new(data);
        let mut buf = [0u8; 2];
        tracked.read_exact_at(0, &mut buf).expect("in bounds");
        assert!(tracked.read_exact_at(3, &mut buf).is_err(), "out of bounds");
        assert_eq!(tracked.ledger().spans(), spans(&[(0, 2)]));
        assert_eq!(ReadAt::len(&mut tracked).expect("len"), 4);
        let parts = tracked.into_parts();
        assert_eq!(parts.source, data);
        assert_eq!(parts.ledger.spans(), spans(&[(0, 2)]));
    }

    /// Reads through a `Rebased` view of a tracked source land in the ledger at **physical**
    /// offsets — the maker-note pattern.
    #[test]
    fn rebased_reads_record_physical_offsets() {
        let data: &[u8] = &[0, 1, 2, 3, 4, 5, 6, 7];
        let mut tracked = Tracked::new(data);
        {
            let mut view = (&mut tracked).rebased(4);
            let mut buf = [0u8; 2];
            view.read_exact_at(1, &mut buf).expect("read");
            assert_eq!(buf, [5, 6]);
        }
        assert_eq!(tracked.ledger().spans(), spans(&[(5, 2)]));
    }
}
