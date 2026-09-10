//! fuzz · robustness — `gamut_isobmff`'s box walk and model reader, on untrusted bytes.
//!
//! `docs/testing.md`'s per-crate table names `read` as this crate's untrusted-input surface. An
//! ISOBMFF file is a tree of length-prefixed boxes whose every length the file chose, so the
//! `#![forbid(unsafe_code)]` reader must end in a typed error rather than a panic, a hang (a box
//! whose declared size does not advance the cursor) or an allocation sized from a declared count.
//!
//! Beyond the crash oracle it checks the crate's own **byte-accounting totality**: when
//! `walk_segments` succeeds, its segments tile `0..len` exactly — starting at 0, contiguous,
//! non-overlapping, none empty, the last ending at end of file. That is the guarantee
//! `tests/accounting.rs` pins over hand-built files; here it is asked of whatever the engine
//! produces, which is where a size-0 or a wrapping box length would show up as a hole or an
//! overlap rather than as a crash.
//!
//! Injection that proved the tiling check fires (re-runnable): in `walk_segments`, record a box's
//! segment as `b.offset + 8..end` — byte accounting that counts box bodies and forgets their
//! headers. The committed seed alone reports it, with no search:
//! `run.sh isobmff_boxes <seeds> -- -runs=0` gives *"segment 8..24 leaves a gap or overlaps at
//! 0"*.
//!
//! `BoxReader` is driven separately from `walk_segments` because it is the lower layer and a
//! caller may use it directly: the check there is that the cursor advances strictly, so a walk of
//! a hostile file cannot spin.
//!
//! A crash found here is **minimised and promoted into a named deterministic case** in
//! `gamut-isobmff`'s own suite. The corpus is a search aid, not the regression record.

#![no_main]

use gamut_isobmff::{BoxReader, read, walk_meta_children, walk_segments};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // The raw box layer: every successful step must consume at least one byte, or a walk of a
    // hostile file never terminates.
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

    // The segment walk: byte-accounting totality. Walking a cursor rather than asserting the
    // four properties separately states the whole claim once — start at 0, contiguous,
    // non-overlapping, no empty segment, ending at end of file — and it is the form that stays
    // correct for a zero-length input, where an empty segment list already tiles `0..0`.
    if let Ok((segments, meta_body)) = walk_segments(data) {
        let mut cursor = 0usize;
        for segment in &segments {
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
        if let Some(body) = meta_body {
            let _ = walk_meta_children(body);
        }
    }

    // The model reader, the entry point the table names. It validates items and properties on top
    // of the walk, so it reaches offsets the walk alone never resolves.
    let _ = read(data);
});
