//! fuzz · robustness — `gamut_heic`'s typed `hvcC` record and NAL layer, the second half of the
//! crate's untrusted-input surface (`docs/testing.md`'s table names container `parse` **and** NAL
//! `parse`).
//!
//! An `hvcC` record is a length-prefixed array-of-arrays whose counts and lengths all come from
//! the file, and the item payload behind it is a chain of `nal_length_size`-byte length prefixes.
//! Both are `#![forbid(unsafe_code)]` offset arithmetic, so a hostile record must end in a typed
//! error rather than a panic or a spin.
//!
//! The check beyond the crash oracle is the **append contract**: `annex_b`,
//! `annex_b_parameter_sets` and `annex_b_payload` all document that they *append* to the caller's
//! buffer — bytes already there are left in place, including on the error path — so a caller can
//! reuse one scratch buffer across items. Nothing in any of the three bodies makes that true by
//! construction: each one is free to `clear()` or to write through an index, and doing so breaks
//! every reusing caller while producing no crash at all. That is what this target searches for.
//!
//! All three emitters write into a buffer pre-filled with `SCRATCH`: the two halves, in sequence,
//! into one; `annex_b` into another. An emitter written into an empty buffer has no append
//! contract to break, which is how `annex_b`'s own body went unchecked for a round. The halves run
//! first, so a defect in a half reports as the halves' failure before `annex_b` — which delegates
//! to them — can report it a second time.
//!
//! The contract has **two halves — the success path and the error path — and one injection covers
//! only one of them**, so each has its own, all re-runnable as `run.sh heic_hvcc <seeds> --
//! -runs=0` and all reported by the committed seeds alone with no search:
//!
//! - **success path, the halves.** Begin `HevcConfig::annex_b_parameter_sets` with `out.clear()` —
//!   an emitter that replaces instead of appending, which breaks every reusing caller and crashes
//!   nothing. Reports *"an annex_b emitter overwrote what was already in the buffer"*.
//! - **success path, `annex_b` itself.** Begin `HevcConfig::annex_b`'s own body with
//!   `out.clear()`, before it delegates — the halves are untouched, so only this pass sees it.
//!   Reports *"annex_b overwrote what was already in the buffer"*.
//! - **error path.** Have `annex_b_payload` `out.clear()` before returning the error a malformed
//!   NAL length prefix produces — an emitter that unwinds the caller's buffer when it gives up.
//!   Reports *"an annex_b emitter overwrote what was already in the buffer"*, on
//!   `corpus/heic_hvcc/truncated-payload-nal.bin`.
//!
//! That seed is what makes the error half reachable: the well-formed record's payload
//! splits cleanly, so `annex_b_payload` never returns `Err` for it and the error-path injection
//! goes unreported. `truncated-payload-nal.bin` is the same record with its one NAL length prefix
//! raised by one, past the end of the payload.
//!
//! The prefix and tail comparisons take their slices with `get`, not by indexing: an emitter that
//! truncates the buffer would otherwise report a bare "range end index out of range" from inside
//! this target rather than the assertion's own message, which is a worse thing to be handed by an
//! unattended run.
//!
//! Alongside it, and explicitly **not** a differential, is a **structure pin**: `annex_b`'s body
//! *is* `annex_b_parameter_sets` followed by `annex_b_payload`, so asserting the whole equals the
//! two halves cannot fail for any input while that body stands. It is kept because the split is a
//! documented API contract with two distinct callers — an Annex-B decoder takes the whole stream,
//! an Android MediaCodec-shaped API takes `csd-0` and the samples separately — so a future
//! `annex_b` that stops delegating is a real regression. It compares what the two append passes
//! wrote after their scratch prefixes rather than costing a third emitter run: the equality of two `is_ok()` calls on the
//! same expression, which an earlier draft also asserted, is trivially true and is gone.
//!
//! `validate_still_payload` is driven for its own sake: it re-walks the payload through
//! `NalHeader::parse`, a different reach from the Annex-B emitters. The "no empty NAL unit"
//! assertion beside that walk is a **structure pin** too, and is labelled as one at the site:
//! `NalUnitIter::next` returns `Err("zero-length NAL unit")` for `len == 0` *before* it can yield
//! an empty slice, so no input reaches an `Ok` that fails it. It pins that early return staying
//! where it is, at the cost of one `is_empty` on a slice already in hand.
//!
//! ## Input framing
//!
//! `u16` big-endian record length, the `hvcC` record, then the item payload. A two-byte length
//! rather than a byte lets the engine reach records past 255 bytes (a real record with several
//! parameter sets is bigger), and taking it from the front means a mutation inside the record
//! does not also reframe the payload.
//!
//! A crash found here is **minimised and promoted into a named deterministic case** in
//! `gamut-heic`'s own suite. The corpus is a search aid, not the regression record.

#![no_main]

use gamut_heic::{HevcConfig, NalHeader, iter_nal_units};
use libfuzzer_sys::fuzz_target;

/// The bytes a reused scratch buffer is pre-filled with, so an emitter that replaces rather than
/// appends is visible as a missing prefix rather than as a length that happens to match.
const SCRATCH: [u8; 3] = [0xEE; 3];

fuzz_target!(|data: &[u8]| {
    let Some((length, rest)) = data.split_first_chunk::<2>() else {
        return;
    };
    let split = usize::from(u16::from_be_bytes(*length)).min(rest.len());
    let (record, payload) = rest.split_at(split);

    let Ok(config) = HevcConfig::parse(record) else {
        return;
    };

    // Pass one: the stream through the two halves, into a buffer that is *not* empty. The prefix
    // assertion is the live check for the halves — it fails if either replaces instead of
    // appending, on the success path or the error path.
    let mut reused = SCRATCH.to_vec();
    config.annex_b_parameter_sets(&mut reused);
    let _ = config.annex_b_payload(payload, &mut reused);
    assert_eq!(
        reused.get(..SCRATCH.len()),
        Some(&SCRATCH[..]),
        "an annex_b emitter overwrote what was already in the buffer"
    );

    // Pass two: the whole stream, as an Annex-B decoder takes it, into a second pre-filled buffer —
    // so `annex_b`'s own body is held to the append contract, not only the halves it delegates to.
    let mut whole = SCRATCH.to_vec();
    let _ = config.annex_b(payload, &mut whole);
    assert_eq!(
        whole.get(..SCRATCH.len()),
        Some(&SCRATCH[..]),
        "annex_b overwrote what was already in the buffer"
    );

    // The structure pin above: what each pass appended after its scratch prefix.
    assert_eq!(
        reused.get(SCRATCH.len()..),
        whole.get(SCRATCH.len()..),
        "annex_b is not its two documented halves concatenated"
    );

    // The NAL layer on its own reach: the still-image constraint re-walks the payload through
    // `NalHeader::parse`.
    let _ = config.validate_still_payload(payload);
    for nal in iter_nal_units(payload, config.nal_length_size()) {
        let Ok(nal) = nal else { break };
        // Structure pin, not a check: `next` errors on a zero length before it can yield an empty
        // slice, so this cannot fail by input while that early return stands (module docs).
        assert!(!nal.is_empty(), "iter_nal_units yielded an empty NAL unit");
        let _ = NalHeader::parse(nal);
    }
});
