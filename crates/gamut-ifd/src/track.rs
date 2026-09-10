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

use core::iter::Peekable;
use core::slice::Iter;

use gamut_core::Result;

use crate::segment::Range;
use crate::source::ReadAt;

/// A sorted, coalesced set of byte ranges that were physically read.
///
/// Re-reads merge silently — the ledger is a set of bytes touched, not a log of operations —
/// so re-fetching a value is never mistaken for a double-claim.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReadLedger {
    /// Disjoint, non-adjacent, sorted by start.
    spans: Vec<Range>,
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
        let i = self.spans.partition_point(|s| s.end() < start);
        if i == self.spans.len() || self.spans[i].start > end {
            self.spans.insert(
                i,
                Range {
                    start,
                    len: end - start,
                },
            );
            return;
        }
        let new_start = self.spans[i].start.min(start);
        // The spans that merge are the run from `i` whose starts still reach `end`; taking that
        // run as a sub-slice is what bounds the walk. A hand-advanced index is bounded instead
        // by its own arithmetic being right -- `j *= 1` never advances one -- and the loop it
        // drove could then be reported only as a mutation-testing timeout, never as a wrong
        // answer (issue #110).
        let merge_len = self.spans[i..]
            .iter()
            .take_while(|s| s.start <= end)
            .count();
        let j = i + merge_len;
        let new_end = self.spans[i..j].iter().fold(end, |e, s| e.max(s.end()));
        self.spans.splice(
            i..j,
            [Range {
                start: new_start,
                len: new_end - new_start,
            }],
        );
    }

    /// The recorded spans: disjoint, non-adjacent, sorted by start.
    #[must_use]
    pub fn spans(&self) -> &[Range] {
        &self.spans
    }

    /// Whether every byte of `range` was read. (Spans are coalesced, so containment can only
    /// hold within a single span.)
    #[must_use]
    pub fn contains(&self, range: Range) -> bool {
        if range.len == 0 {
            return true;
        }
        let i = self.spans.partition_point(|s| s.start <= range.start);
        i > 0 && self.spans[i - 1].end() >= range.end()
    }

    /// The bytes read but **not** covered by `claims` — the parser-defect signal
    /// ([`SegmentReport::unclaimed_reads`](crate::SegmentReport::unclaimed_reads)). `claims`
    /// need not be sorted or disjoint; it is normalised internally.
    #[must_use]
    pub fn subtract(&self, claims: &[Range]) -> Vec<Range> {
        // Normalise: sort and merge (overlap and adjacency both coalesce).
        let mut merged: Vec<Range> = Vec::with_capacity(claims.len());
        let mut sorted: Vec<Range> = claims.iter().copied().filter(|r| r.len > 0).collect();
        sorted.sort_by_key(|r| (r.start, r.len));
        for r in sorted {
            match merged.last_mut() {
                Some(last) if r.start <= last.end() => {
                    let new_end = last.end().max(r.end());
                    last.len = new_end - last.start;
                }
                _ => merged.push(r),
            }
        }
        // Two-pointer subtract of `merged` from the ledger spans.
        let mut out = Vec::new();
        let mut c = merged.iter().peekable();
        for span in &self.spans {
            let mut pos = span.start;
            let end = span.end();
            while pos < end {
                match next_live_claim(&mut c, pos) {
                    Some(r) if r.start <= pos => {
                        // Covered up to the claim's end.
                        pos = r.end().min(end);
                    }
                    // Uncovered up to whichever comes first: the next claim's start, or the end
                    // of this span.
                    //
                    // The two cases were separate arms, split on `r.start < end`. They are the
                    // same arm: at `r.start == end` the old second arm pushed `end - pos` and set
                    // `pos = end`, which is exactly what the fallback did, so `<` and `<=` there
                    // produced identical output and no test could tell them apart. Written as a
                    // `min` the operator is gone rather than excluded (#110) -- and `min` is not
                    // equivalent to `max` here, so what replaces it is killable.
                    next => {
                        let stop = next.map_or(end, |r| r.start.min(end));
                        out.push(Range {
                            start: pos,
                            len: stop - pos,
                        });
                        pos = stop;
                    }
                }
            }
        }
        out
    }
}

/// The first claim at the head of `claims` that reaches past `pos`, dropping the ones that do
/// not.
///
/// This exists as its own function because its comparison is the one thing in
/// [`ReadLedger::subtract`] no test can pin. Dropping a settled claim is what leaves the walk's
/// covered arm a claim that ends *after* `pos`, and so what makes `pos` advance; relax the
/// comparison and the walk stops making progress instead of producing a wrong answer, which
/// cargo-mutants can report only as a timeout (issue #110). Confining it here keeps the
/// exclusion that documents it anchored to a name rather than to a line, and leaves every other
/// comparison in `subtract` — all of them killable — outside its reach. Returning the surviving
/// claim rather than nothing is what keeps the *body* mutant killable: `None` says "no claim
/// covers anything", and the walk then reports every read as unclaimed.
fn next_live_claim<'a>(claims: &mut Peekable<Iter<'a, Range>>, pos: u64) -> Option<&'a Range> {
    while claims.next_if(|r| r.end() <= pos).is_some() {}
    claims.peek().copied()
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
    pub fn into_parts(self) -> (S, ReadLedger) {
        (self.inner, self.ledger)
    }
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

    fn spans(pairs: &[(u64, u64)]) -> Vec<Range> {
        pairs
            .iter()
            .map(|&(start, len)| Range { start, len })
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
        assert!(ledger.contains(Range { start: 4, len: 6 }));
        assert!(ledger.contains(Range { start: 5, len: 2 }));
        assert!(!ledger.contains(Range { start: 3, len: 2 }));
        assert!(!ledger.contains(Range { start: 8, len: 4 }));
        assert!(!ledger.contains(Range { start: 20, len: 1 }));
        assert!(ledger.contains(Range { start: 0, len: 0 }), "empty range");
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
        let (inner, ledger) = tracked.into_parts();
        assert_eq!(inner, data);
        assert_eq!(ledger.spans(), spans(&[(0, 2)]));
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
