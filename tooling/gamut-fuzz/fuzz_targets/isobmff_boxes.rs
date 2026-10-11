//! fuzz · robustness — `gamut_isobmff`'s box walk and model reader, on untrusted bytes.
//!
//! `docs/testing.md`'s per-crate table names `read` as this crate's untrusted-input surface. An
//! ISOBMFF file is a tree of length-prefixed boxes whose every length the file chose, so the
//! `#![forbid(unsafe_code)]` reader must end in a typed error rather than a panic, a hang or an
//! allocation sized from a declared count.
//!
//! ## The check beyond the crash oracle
//!
//! The crate's own **byte-accounting totality**: when `walk_segments` succeeds, its segments tile
//! `0..len` exactly — starting at 0, contiguous, non-overlapping, the last ending at end of file.
//! That is the guarantee `tests/accounting.rs` pins over hand-built files; here it is asked of
//! whatever the engine produces, which is where a wrapping box length would show up as a hole or an
//! overlap rather than as a crash.
//!
//! Both halves of the tiling are checked separately, and each has its own injection. Both are
//! re-runnable as `run.sh isobmff_boxes <seeds> -- -runs=0` and both are reported by the committed
//! seed alone, with no search:
//!
//! - **contiguity.** In `walk_segments`, record a box's segment as `b.offset + 8..end` — byte
//!   accounting that counts box bodies and forgets their headers. Gives *"segment 8..24 leaves a
//!   gap or overlaps at 0"*.
//! - **coverage to end of file.** In `walk_segments`, drop the last segment (`segments.pop()`
//!   before the return) — accounting that stops one box short. Gives *"coverage does not run to
//!   end of file"*.
//!
//! ## Two assertions here are structure pins, not checks
//!
//! Both are labelled at the site and neither is listed in the README's "check beyond the crash
//! oracle" column, because **no file can fail them**:
//!
//! - **the box cursor strictly advances.** The defect this names is a box whose declared size does
//!   not move the cursor — and `next_box` cannot express it: it reads the 4-byte size and the
//!   4-byte type through `take` *before* any success return, so `position()` has already grown by
//!   8 by the time a `RawBox` exists. Measured, not reasoned: with the `size < header_size` guard
//!   removed and the body length taken as `size.saturating_sub(header_size)` — the exact defect
//!   the assertion advertises — the committed seed reports nothing, and neither does a bounded
//!   search. The control fires immediately: rewinding `self.pos` to the box offset before the
//!   `Ok(Some(..))` return reports *"BoxReader did not advance past 0"*. So what the assertion
//!   really pins is that unconditional 8-byte header read staying where it is.
//! - **no segment is empty.** `walk_segments` pushes exactly three shapes, and each is non-empty
//!   for the same reason: a `Box` segment is `b.offset..reader.position()`, which the 8-byte header
//!   read above has already widened; an `AppendedStream` is `b.offset..data.len()` for a box that
//!   was read, so `data.len() > b.offset`; and a `Trailer` is `box_start..data.len()` on an `Err`,
//!   which `next_box` only returns when at least one byte remained.
//!
//! Both cost one comparison inside a loop the target walks anyway.
//!
//! A crash found here is **minimised and promoted into a named deterministic case** in
//! `gamut-isobmff`'s own suite. The corpus is a search aid, not the regression record.

#![no_main]

use gamut_isobmff::{BoxReader, read, walk_meta_children, walk_segments};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // The raw box layer is driven separately from `walk_segments` because it is the lower layer
    // and a caller may use it directly. The two assertions in this loop are structure pins on
    // `next_box`'s unconditional 8-byte header read, not checks (see the module docs).
    let mut reader = BoxReader::new(data);
    let mut position = reader.position();
    while let Ok(Some(_)) = reader.next_box() {
        let next = reader.position();
        assert!(
            next > position,
            "BoxReader did not advance past {position} (len {})",
            data.len()
        );
        assert!(next <= data.len(), "BoxReader ran past the end: {next}");
        position = next;
    }

    // The segment walk: byte-accounting totality. Walking a cursor states the whole claim once —
    // start at 0, contiguous, non-overlapping, ending at end of file — and it is the form that
    // stays correct for a zero-length input, where an empty segment list already tiles `0..0`.
    if let Ok((segments, meta_body)) = walk_segments(data) {
        let mut cursor = 0usize;
        for segment in &segments {
            assert_eq!(
                segment.range.start, cursor,
                "segment {:?} leaves a gap or overlaps at {cursor}",
                segment.range
            );
            // Structure pin, not a check: every segment `walk_segments` can push is non-empty by
            // construction (see the module docs).
            assert!(
                segment.range.end > segment.range.start,
                "empty segment {:?}",
                segment.range
            );
            cursor = segment.range.end;
        }
        assert_eq!(cursor, data.len(), "coverage does not run to end of file");
        if let Some(body) = meta_body {
            let _ = walk_meta_children(body);
        }
    }

    // The model reader, the entry point the table names. It validates items and properties on top
    // of the walk, so it reaches offsets the walk alone never resolves.
    let _ = read(data);
});
