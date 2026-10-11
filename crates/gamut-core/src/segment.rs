//! Byte-tiling: the format-agnostic shape of a "every input byte maps to exactly one typed
//! segment" report, and the predicate that checks it.
//!
//! A format crate's byte-accounting walk (`gamut_isobmff::walk_segments`, which `gamut-heic` and
//! `gamut-avif` share; a deconstruct report) decomposes its input into [`Segment`]s tagged with
//! the format's own `kind` enum. What makes such a list *total* is the same in every format: the
//! segments are non-empty, contiguous, non-overlapping, start at `0` and end at the input length.
//! [`covers`] states that once, so the formats keep their kinds and walks and share only the
//! law. [`tiles`] is the same predicate over bare ranges, for a report whose segments carry
//! their extents in another form (e.g. `u64` offsets for files past `usize`).
//!
//! The walks themselves stay in the format crates: each stops on its own format's rules, and
//! those rules are what a format crate's tests pin.

use core::ops::Range;

/// One contiguous run of an input, tagged by what it holds.
///
/// `K` is the format's own classification (e.g. `gamut_isobmff::SegmentKind`); this type adds
/// nothing to it but the half-open byte range it occupies. A list of these is total over an
/// input of `len` bytes exactly when [`covers`] says so.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Segment<K> {
    /// The half-open byte range this segment occupies within the input (`start..end`).
    pub range: Range<usize>,
    /// What the bytes in [`range`](Self::range) are.
    pub kind: K,
}

/// Whether `segments` tile `0..len` exactly: every segment is non-empty, the first starts at
/// `0`, each one starts where the previous one ended (no gap, no overlap), and the last ends at
/// `len`.
///
/// An empty list tiles only an empty input (`len == 0`). This is the every-byte invariant of a
/// byte-accounting report; see [`tiles`] for the same predicate over bare ranges.
#[must_use]
pub fn covers<K>(segments: &[Segment<K>], len: usize) -> bool {
    tiles(segments.iter().map(|s| s.range.clone()), len)
}

/// Whether `ranges`, in order, tile `0..len` exactly — the predicate behind [`covers`], over
/// bare half-open ranges of any unsigned offset type (`usize` for an in-memory input, `u64` for
/// a file addressed past `usize`).
///
/// Every range must be non-empty (`start < end`) and start where the previous one ended, the
/// first at `0`; the last must end at `len`. An empty sequence tiles only `len == 0`. The ranges
/// are consumed in the order given and are not sorted: an out-of-order list is not a tiling.
#[must_use]
pub fn tiles<T>(ranges: impl IntoIterator<Item = Range<T>>, len: T) -> bool
where
    T: Copy + PartialOrd + From<u8>,
{
    let mut at = T::from(0);
    for range in ranges {
        if range.start != at || range.is_empty() {
            return false;
        }
        at = range.end;
    }
    at == len
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds segments with unit kinds from bare ranges — the kind plays no part in tiling.
    fn segs(ranges: &[Range<usize>]) -> Vec<Segment<()>> {
        ranges
            .iter()
            .map(|r| Segment {
                range: r.clone(),
                kind: (),
            })
            .collect()
    }

    #[test]
    fn contiguous_segments_ending_at_len_cover() {
        assert!(covers(&segs(&[0..3, 3..4, 4..10]), 10));
        assert!(covers(&segs(&[0..1]), 1));
    }

    #[test]
    fn an_empty_list_covers_only_an_empty_input() {
        assert!(covers::<()>(&[], 0));
        assert!(!covers::<()>(&[], 1));
    }

    #[test]
    fn a_first_segment_not_at_zero_does_not_cover() {
        assert!(!covers(&segs(&[1..10]), 10));
    }

    #[test]
    fn a_gap_does_not_cover() {
        assert!(!covers(&segs(&[0..3, 4..10]), 10));
    }

    #[test]
    fn an_overlap_does_not_cover() {
        assert!(!covers(&segs(&[0..4, 3..10]), 10));
    }

    #[test]
    fn an_empty_segment_does_not_cover() {
        // Chained correctly, but a zero-length segment classifies no byte.
        assert!(!covers(&segs(&[0..3, 3..3, 3..10]), 10));
        assert!(!covers(&segs(&[0..0]), 0));
    }

    #[test]
    fn a_reversed_segment_does_not_cover() {
        // `start > end` chains back to the start of a following segment but claims no byte.
        assert!(!covers(&segs(&[0..5, 5..2, 2..10]), 10));
    }

    #[test]
    fn a_short_tail_does_not_cover() {
        assert!(!covers(&segs(&[0..3, 3..9]), 10));
    }

    #[test]
    fn a_long_tail_does_not_cover() {
        assert!(!covers(&segs(&[0..3, 3..11]), 10));
    }

    #[test]
    fn out_of_order_segments_do_not_cover() {
        assert!(!covers(&segs(&[3..10, 0..3]), 10));
    }

    #[test]
    fn tiles_accepts_u64_offsets() {
        let big = u64::from(u32::MAX) + 7;
        assert!(tiles([0..8u64, 8..big], big));
        assert!(!tiles([0..8u64, 9..big], big));
    }
}
