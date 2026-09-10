//! fuzz · robustness — `gamut_ifd`'s reader entry points, on wholly untrusted bytes.
//!
//! The sibling `ifd_read_ledger` target drives the crate's `invariants` laws over *normalised*
//! range lists. This one is the other half `docs/testing.md`'s per-crate table asks for — the
//! **driver** over the untrusted-input surface it names (`IfdReader`, `read`) — and it hands the
//! engine's bytes to the parser unchanged, because the thing under test is precisely what the
//! parser does with a byte string nobody normalised.
//!
//! Its oracle is not a law function, and deliberately so:
//!
//! - the engine's own — a panic, a hang, or an allocation past libFuzzer's RSS limit is a crash,
//!   and `gamut-ifd` is `#![forbid(unsafe_code)]` precisely so that hostile offsets end in a typed
//!   error rather than any of those;
//! - the **dual-ledger audit** (#263) is the one check here that the engine cannot make: whenever
//!   `read_audited` succeeds, every byte the parser physically read must be inside a structural
//!   claim and every `Parsed` claim must have been physically read. A parser that eats bytes it
//!   never declares — or declares bytes it never touched — produces no crash at all, and this is
//!   what sees it.
//!
//! Injection that proved the audit fires (re-runnable): in `IfdReader::read_chain`, claim the
//! header as `header_size() - 1` bytes — an off-by-one that leaves a byte the parser physically
//! reads outside every structural claim. The committed seeds alone report it, with no search:
//! `run.sh ifd_read <seeds> -- -runs=0` gives *"parser read bytes it never claimed"* carrying
//! `unclaimed_reads: [Range { start: 7, len: 1 }]`.
//!
//! ## What this target deliberately does *not* check
//!
//! An earlier draft also compared `read(data)` against `IfdReader::open(data)?.read_file()` and
//! called it a differential between two doors. It is not one: `reader.rs` defines `read` as
//! *exactly* that expression (and `read_tree` likewise), so the two sides are the same function
//! call written twice and the comparison cannot fail for any input. It is a **structure pin** on
//! the wrappers staying thin — worth having, but bounded and deterministic work, not a search.
//! It is kept where it belongs, in `crates/gamut-ifd/tests/robustness.rs`, whose `survives`
//! helper drives both doors over the exhaustive truncation and single-byte-overwrite corpus.
//! Dropping the two duplicate parses here doubles this target's execution rate.
//!
//! `tests/robustness.rs` states the audit check too, over that same bounded corpus. That corpus is
//! the reproducible per-PR gate; this is the unbounded search beside it. A crash found here is
//! **minimised and promoted into a named deterministic case in that file**; the corpus is a search
//! aid, not the regression record.

#![no_main]

use gamut_ifd::{read, read_audited, read_tree};
use libfuzzer_sys::fuzz_target;

/// Sub-IFD pointer tags a DNG/EXIF-shaped consumer would follow.
///
/// The same three `tests/robustness.rs` walks with: `SubIFDs`, `ExifIFD`, `GPSInfoIFD`. Fixed
/// rather than drawn from the input, so every execution exercises the recursive tree walk instead
/// of spending most of its draws on an empty tag list.
const POINTER_TAGS: &[u16] = &[330, 34665, 34853];

fuzz_target!(|data: &[u8]| {
    // The byte audit: the live check. Only meaningful on a parse that succeeded, because an
    // abandoned parse has no complete claim set to reconcile against.
    if let Ok((_, report)) = read_audited(data) {
        assert!(
            report.unclaimed_reads.is_empty(),
            "parser read bytes it never claimed: {report:?}"
        );
        assert!(
            report.unread_claims.is_empty(),
            "parser claimed bytes it never read: {report:?}"
        );
    }

    // The flat chain and the sub-IFD tree walk, for the engine's own oracle. A tree parse reaches
    // offsets the flat chain never visits, so it is a separate reach rather than a stronger
    // version of the flat one; neither result is compared against anything, because the only
    // available comparand is the same function under another name (see the module docs).
    let _ = read(data);
    let _ = read_tree(data, POINTER_TAGS);
});
