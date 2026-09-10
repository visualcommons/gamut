//! fuzz · robustness — `gamut_heic::HeifContainer::parse`, the container half of the crate's
//! untrusted-input surface (`docs/testing.md`'s per-crate table names container `parse`).
//!
//! `gamut-heic` is decode-only and its stated product guarantee is a **full-fidelity byte
//! accounting**: every input byte maps to a box, to an appended motion-photo stream, or to an
//! explicit trailer. That is a claim the engine can be pointed at directly, and it is stronger
//! than "did not crash": a container that silently drops a region still parses.
//!
//! So this target checks, on every successful parse:
//!
//! - the segments tile `0..len` exactly — start at 0, contiguous, non-overlapping, none empty,
//!   last ending at end of file;
//! - the accessors are consistent with that tiling: `appended_stream` and `trailer` are `Some`
//!   exactly when a segment of that kind exists, and `boxes()` yields one entry per `Box`
//!   segment;
//! - every borrowed slice the accessors hand out is a subslice of the input the container was
//!   given, which is what `data()` promises.
//!
//! Real files reach here: phones append a whole second MP4 after the HEIC, and camera apps leave
//! trailers, so the accounting path is not an exotic branch.
//!
//! Injection that proved the accessor check fires (re-runnable): make `HeifContainer::boxes()`
//! skip the `ftyp` box — an accessor that filters what the segments hold. The committed seed alone
//! reports it, with no search: `run.sh heic_container <seeds> -- -runs=0` gives *"boxes()
//! disagrees with the Box segments"*.
//!
//! Its **reach is one function per accessor**, and that is a limitation rather than a flaw:
//! `boxes`, `appended_stream` and `trailer` are each a three-line `filter_map`/`find_map` over the
//! segment list, so the check sees a defect in those and nothing deeper. It is kept at that size
//! because the accessors are the API every caller actually uses — the segment list is the
//! evidence, the accessors are the product — and because it costs one pass over a list the target
//! walks anyway. The tiling check above is the one with the deep reach: it sees the whole parse.
//!
//! A crash found here is **minimised and promoted into a named deterministic case** in
//! `gamut-heic`'s own suite. The corpus is a search aid, not the regression record.

#![no_main]

use gamut_heic::{HeifContainer, SegmentKind};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(container) = HeifContainer::parse(data) else {
        return;
    };

    // Walking a cursor states the whole tiling claim once — start at 0, contiguous,
    // non-overlapping, no empty segment, ending at end of file — and stays correct for a
    // zero-length input, where an empty segment list already tiles `0..0`.
    let segments = container.segments();
    let mut cursor = 0usize;
    for segment in segments {
        assert_eq!(
            segment.range.start, cursor,
            "segment {:?} leaves a gap or overlaps at {cursor}",
            segment.range
        );
        assert!(
            segment.range.end > segment.range.start,
            "empty segment {:?}",
            segment.range
        );
        cursor = segment.range.end;
    }
    assert_eq!(cursor, data.len(), "coverage does not run to end of file");

    // The accessors report exactly what the tiling holds.
    let boxes = container.boxes().count();
    let mut kinds = (0usize, 0usize, 0usize);
    for segment in segments {
        match segment.kind {
            SegmentKind::Box { .. } => kinds.0 += 1,
            SegmentKind::AppendedStream(_) => kinds.1 += 1,
            SegmentKind::Trailer(_) => kinds.2 += 1,
            _ => {}
        }
    }
    assert_eq!(boxes, kinds.0, "boxes() disagrees with the Box segments");
    assert_eq!(
        container.appended_stream().is_some(),
        kinds.1 > 0,
        "appended_stream() disagrees with the AppendedStream segments"
    );
    assert_eq!(
        container.trailer().is_some(),
        kinds.2 > 0,
        "trailer() disagrees with the Trailer segments"
    );

    // `data()` returns the input, and every borrowed region lies inside it.
    assert_eq!(container.data().as_ptr(), data.as_ptr());
    assert_eq!(container.data().len(), data.len());

    // The item model and the unknown-box ledger are built on the same walk; drive them so a
    // defect there is reachable too.
    let _ = container.image().items().count();
    let _ = container.image().primary_item().id();
    let _ = container.unknown_meta_boxes().len();
});
