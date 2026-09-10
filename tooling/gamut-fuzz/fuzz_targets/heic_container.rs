//! fuzz · robustness — `gamut_heic::HeifContainer::parse`, the container half of the crate's
//! untrusted-input surface (`docs/testing.md`'s per-crate table names container `parse`).
//!
//! `gamut-heic` is decode-only and its stated product guarantee is a **full-fidelity byte
//! accounting**: every input byte maps to a box, to an appended motion-photo stream, or to an
//! explicit trailer. Real files reach that path — phones append a whole second MP4 after the HEIC,
//! and camera apps leave trailers — so it is not an exotic branch.
//!
//! ## The checks beyond the crash oracle
//!
//! - **the accessors report exactly what the segment list holds**: `appended_stream` and `trailer`
//!   are `Some` exactly when a segment of that kind exists, and `boxes()` yields one entry per
//!   `Box` segment;
//! - **`data()` is the caller's own buffer, and every borrowed slice the accessors hand out lies
//!   inside it** — checked as a pointer-range containment over `boxes()`, `appended_stream()`,
//!   `trailer()` and `unknown_meta_boxes()`. This is the promise that makes the crate zero-copy:
//!   an accessor that normalised or re-allocated on the way out would satisfy every count above
//!   and still break it.
//!
//! Injections that proved each assertion fires (re-runnable), all reported by the committed seeds
//! alone with no search, as `run.sh heic_container <seeds> -- -runs=0`. Each `is_some()` equality
//! is injected in **both** directions, because one direction is silent on a file that has no
//! segment of that kind:
//!
//! | injection in `gamut-heic` | message |
//! |---|---|
//! | `boxes()` skips the `ftyp` box | *"boxes() disagrees with the Box segments"* |
//! | `appended_stream()` returns `None` unconditionally | *"appended\_stream() disagrees with the AppendedStream segments"* |
//! | `appended_stream()` returns `Some(self.data)` unconditionally | the same |
//! | `trailer()` returns `None` unconditionally | *"trailer() disagrees with the Trailer segments"* |
//! | `trailer()` returns `Some(self.data)` unconditionally | the same |
//! | `data()` returns a leaked copy of the input rather than the input | *"data() is not the input"* |
//! | `boxes()` yields a leaked copy of each body rather than the borrowed body | *"a borrowed slice is not inside data(): 16 bytes at 0x…, data() is 272 bytes at 0x…"* |
//!
//! Two of those rows need a file the corpus did not have. `heic-single-item.heic` carries neither
//! an appended stream nor a trailer, so `appended_stream()`/`trailer()` returning `None`
//! unconditionally agreed with it and reported nothing. `appended-stream.heic` (a second top-level
//! `ftyp`, as a motion-photo phone writes) and `trailer.heic` (a truncated trailing box header,
//! retained once `ftyp` and `meta` are seen) are what make those two directions reachable.
//!
//! The accessors' **reach is one function each**, and that is a limitation rather than a flaw:
//! `boxes`, `appended_stream` and `trailer` are each a three-line `filter_map`/`find_map` over the
//! segment list. They are checked because they are the API every caller actually uses — the segment
//! list is the evidence, the accessors are the product — and because they cost one pass over a list
//! the target walks anyway.
//!
//! ## The segment tiling is deliberately not checked here
//!
//! `HeifContainer::parse` stores `gamut_isobmff::walk_segments(data)?` verbatim and `segments()`
//! returns it unchanged, so a tiling check here searches **the same function** the sibling
//! `isobmff_boxes` target already searches — and over a strictly narrower input set, since it is
//! reached only for files `gamut_isobmff::read` also accepted. Measured, not reasoned: the
//! `b.offset + 8..end` injection recorded in `isobmff_boxes` produced the *identical* message here,
//! *"segment 8..24 leaves a gap or overlaps at 0"*, because it is the identical assertion over the
//! identical values. Two ten-minute runners searching one function is a real cost at a
//! proven-zero marginal yield, so this target keeps only what is genuinely its own. The tiling
//! guarantee itself is unaffected: `isobmff_boxes` searches it, and `gamut-heic`'s own
//! `tests/accounting.rs` pins it over hand-built files.
//!
//! A crash found here is **minimised and promoted into a named deterministic case** in
//! `gamut-heic`'s own suite. The corpus is a search aid, not the regression record.

#![no_main]

use gamut_heic::{HeifContainer, SegmentKind};
use libfuzzer_sys::fuzz_target;

/// Whether `part` is a subslice of `whole`, by pointer range.
///
/// Comparing raw pointers needs no `unsafe`, and a zero-length borrow at end of input still
/// satisfies `start >= whole.start && end <= whole.end`.
fn is_inside(part: &[u8], whole: &[u8]) -> bool {
    let (part, whole) = (part.as_ptr_range(), whole.as_ptr_range());
    part.start >= whole.start && part.end <= whole.end
}

fuzz_target!(|data: &[u8]| {
    let Ok(container) = HeifContainer::parse(data) else {
        return;
    };

    // The accessors report exactly what the segment list holds.
    let boxes: Vec<_> = container.boxes().collect();
    let mut kinds = (0usize, 0usize, 0usize);
    for segment in container.segments() {
        match segment.kind {
            SegmentKind::Box { .. } => kinds.0 += 1,
            SegmentKind::AppendedStream(_) => kinds.1 += 1,
            SegmentKind::Trailer(_) => kinds.2 += 1,
            _ => {}
        }
    }
    assert_eq!(
        boxes.len(),
        kinds.0,
        "boxes() disagrees with the Box segments"
    );
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

    // `data()` is the caller's buffer, and every borrowed region the accessors hand out lies
    // inside it.
    let whole = container.data();
    assert_eq!(whole.as_ptr(), data.as_ptr(), "data() is not the input");
    assert_eq!(whole.len(), data.len(), "data() is not the whole input");
    let borrowed = boxes
        .iter()
        .map(|(_, body)| *body)
        .chain(container.appended_stream())
        .chain(container.trailer())
        .chain(container.unknown_meta_boxes().iter().map(|b| b.body));
    for part in borrowed {
        assert!(
            is_inside(part, whole),
            "a borrowed slice is not inside data(): {} bytes at {:p}, data() is {} bytes at {:p}",
            part.len(),
            part.as_ptr(),
            whole.len(),
            whole.as_ptr()
        );
    }

    // The item model and the unknown-box ledger are built on the same walk; drive them so a
    // defect there is reachable too.
    let _ = container.image().items().count();
    let _ = container.image().primary_item().id();
});
