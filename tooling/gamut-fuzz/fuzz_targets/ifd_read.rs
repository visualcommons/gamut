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
//! - a **differential between two public entry points**: the slice functions are thin wrappers
//!   over the streaming engine, so `read` and `IfdReader::read_file` must either both fail or
//!   parse to equal files. One parser, two doors.
//! - the **dual-ledger audit** (#263): whenever `read_audited` succeeds, every byte the parser
//!   physically read is inside a structural claim and every `Parsed` claim was physically read.
//!
//! `tests/robustness.rs` states the same three checks over a bounded, exhaustive corpus —
//! truncations, single-byte overwrites, a named malformed list. That corpus is the reproducible
//! per-PR gate; this is the unbounded search beside it. A crash found here is **minimised and
//! promoted into a named deterministic case in that file**; the corpus is a search aid, not the
//! regression record.

#![no_main]

use gamut_ifd::{IfdReader, TiffFile, read, read_audited, read_tree};
use libfuzzer_sys::fuzz_target;

/// Renders a parsed file to its structural `Debug` form, for comparing two parses.
///
/// `TiffFile` derives `PartialEq`, not `Eq`, because a `FLOAT`/`DOUBLE` field holds `f32`/`f64` —
/// and an arbitrary byte string decodes to `NaN` often enough that the engine finds one within a
/// minute. `NaN != NaN` makes `PartialEq` non-reflexive there, so `a == b` reports a
/// disagreement between two *identical* parses. `Debug` renders `NaN` as a value like any other,
/// which is what makes it the total comparison this differential needs; the field order and the
/// enum variants are all in the rendering, so nothing structural is lost by going through it.
fn structure(file: &TiffFile) -> String {
    format!("{file:?}")
}

/// Sub-IFD pointer tags a DNG/EXIF-shaped consumer would follow.
///
/// The same three `tests/robustness.rs` walks with: `SubIFDs`, `ExifIFD`, `GPSInfoIFD`. Fixed
/// rather than drawn from the input, so every execution exercises the recursive tree walk instead
/// of spending most of its draws on an empty tag list.
const POINTER_TAGS: &[u16] = &[330, 34665, 34853];

fuzz_target!(|data: &[u8]| {
    // The flat chain, through both doors.
    let slice = read(data);
    let stream = IfdReader::open(data).and_then(|mut r| r.read_file());
    match (&slice, &stream) {
        (Ok(a), Ok(b)) => assert_eq!(structure(a), structure(b), "flat parse disagreement"),
        (Err(_), Err(_)) => {}
        _ => panic!("flat readers disagree: slice {slice:?} vs stream {stream:?}"),
    }

    // The sub-IFD tree walk, through both doors. A tree parse reaches offsets the flat chain
    // never visits, so it is a separate reach rather than a stronger version of the above.
    let slice_tree = read_tree(data, POINTER_TAGS);
    let stream_tree = IfdReader::open(data).and_then(|mut r| r.read_tree(POINTER_TAGS));
    match (&slice_tree, &stream_tree) {
        (Ok(a), Ok(b)) => assert_eq!(structure(a), structure(b), "tree parse disagreement"),
        (Err(_), Err(_)) => {}
        _ => panic!("tree readers disagree: slice {slice_tree:?} vs stream {stream_tree:?}"),
    }

    // The byte audit. A parser that eats bytes it never declares — or declares bytes it never
    // touched — is what this catches, and it is only meaningful on a parse that succeeded.
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
});
