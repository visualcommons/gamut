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
//! Injection that proved the append check fires (re-runnable): begin
//! `HevcConfig::annex_b_parameter_sets` with `out.clear()` — an emitter that replaces instead of
//! appending, which breaks every reusing caller and crashes nothing. The committed seed alone
//! reports it, with no search: `run.sh heic_hvcc <seeds> -- -runs=0` gives *"an annex_b emitter
//! overwrote what was already in the buffer"*.
//!
//! Alongside it, and explicitly **not** a differential, is a **structure pin**: `annex_b`'s body
//! *is* `annex_b_parameter_sets` followed by `annex_b_payload`, so asserting the whole equals the
//! two halves cannot fail for any input while that body stands. It is kept because the split is a
//! documented API contract with two distinct callers — an Annex-B decoder takes the whole stream,
//! an Android MediaCodec-shaped API takes `csd-0` and the samples separately — so a future
//! `annex_b` that stops delegating is a real regression. It is folded into the append check's
//! second pass rather than costing a third emitter run: the equality of two `is_ok()` calls on the
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

    // Pass one: the whole stream into an empty buffer, as an Annex-B decoder takes it.
    let mut whole = Vec::new();
    let _ = config.annex_b(payload, &mut whole);

    // Pass two: the same stream through the two halves, into a buffer that is *not* empty. The
    // prefix assertion is the live check — it fails if either half replaces instead of appending,
    // on the success path or the error path. The tail assertion is the structure pin above.
    let mut reused = SCRATCH.to_vec();
    config.annex_b_parameter_sets(&mut reused);
    let _ = config.annex_b_payload(payload, &mut reused);
    assert_eq!(
        &reused[..SCRATCH.len()],
        &SCRATCH[..],
        "an annex_b emitter overwrote what was already in the buffer"
    );
    assert_eq!(
        &reused[SCRATCH.len()..],
        &whole[..],
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
