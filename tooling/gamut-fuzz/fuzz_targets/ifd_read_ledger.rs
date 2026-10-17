//! fuzz · law — the read ledger's laws, searched rather than sampled.
//!
//! These are the *same* functions `gamut_ifd::invariants` exposes to the pinned-seed `proptest`
//! properties in the per-PR gate. Nothing is restated here: a law written twice is a law that can
//! disagree with itself, and the whole point of the `invariants` module is that the property and
//! the fuzz target check one specification.
//!
//! What differs is the search. The property draws 512 cases from a fixed seed, which makes it
//! reproducible enough to gate a blocking mutation job. This tier is coverage-guided and
//! unbounded, so it goes looking for the case the property's 512 did not happen to contain — and
//! the pilot in #434 is the argument that such cases exist: the property there killed a mutant
//! `.cargo/mutants.toml` had recorded as *provably equivalent*, on an input (`reads = [{2747,
//! 103}], claims = [{2748, 0}]`) that no hand-written test had thought to try.
//!
//! A crash found here is **minimised and promoted into a named deterministic test** in
//! `gamut-ifd`'s own suite. The corpus is a search aid, not the regression record: a committed
//! seed is only reproducible while the generator is unchanged, and a named case is reproducible
//! forever.

#![no_main]

use gamut_ifd::Range;
use gamut_ifd::invariants::{UNIVERSE, ledger_is_canonical, normalise, subtract_is_set_difference};
use libfuzzer_sys::fuzz_target;

/// The widest span a single drawn range may describe.
///
/// Mirrors the bound the property's generator uses, so the two tiers search the same shape of
/// input rather than merely the same functions.
const MAX_LEN: u64 = 256;

/// Reads one `(start, len)` pair from `bytes`, normalised into the laws' bounded universe.
///
/// Normalising here rather than inside the laws is the same rule the property follows and for the
/// same reason: a law that normalised its own inputs would feed the identical value to the model
/// and to the code under test, so a defect in the normaliser would cancel on both sides.
fn take_range(bytes: &[u8; 4]) -> Range {
    let start = u64::from(u16::from_le_bytes([bytes[0], bytes[1]])) % UNIVERSE;
    // Zero length is reachable and deliberately so: a zero-length *claim* covers nothing and must
    // therefore split nothing, and that is the input class separating a claim filter that drops
    // empty ranges from one that keeps them. It is the class the pilot's counterexample came from.
    let len = u64::from(u16::from_le_bytes([bytes[2], bytes[3]])) % (MAX_LEN + 1);
    normalise(Range { start, len })
}

/// Splits `data` into two range lists: the reads recorded, and the claims subtracted from them.
fn parse(data: &[u8]) -> (Vec<Range>, Vec<Range>) {
    // One leading byte decides the split point, so the engine can steer the balance between the
    // two lists rather than always seeing them equal.
    let Some((&split, rest)) = data.split_first() else {
        return (Vec::new(), Vec::new());
    };
    let ranges: Vec<Range> = rest.as_chunks::<4>().0.iter().map(take_range).collect();
    if ranges.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let at = usize::from(split) % (ranges.len() + 1);
    let (reads, claims) = ranges.split_at(at);
    (reads.to_vec(), claims.to_vec())
}

fuzz_target!(|data: &[u8]| {
    let (reads, claims) = parse(data);

    // Law 1: `record` coalesces into a canonical span set holding exactly the bytes read.
    if let Err(violation) = ledger_is_canonical(&reads) {
        panic!("{violation}");
    }

    // Law 2: `subtract` is the set difference, in canonical form. This is the law that caught the
    // wrongly-excluded mutant in #434, where the byte *set* was right and the canonical *form* was
    // not — a distinction every set-equality test in the crate had missed.
    if let Err(violation) = subtract_is_set_difference(&reads, &claims) {
        panic!("{violation}");
    }
});
