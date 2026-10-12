//! Executable laws for the read ledger and the directory model, shared by the property tests and
//! the fuzz tier.
//!
//! Each function takes plain data, checks one law, and returns a [`Violation`] naming what broke
//! it. No test framework is involved and nothing here panics, so the same body runs under a
//! pinned-seed `proptest` in the per-PR gate and under a corpus-guided driver in extended CI
//! (issues #240, #264). That is the point: a property is the specification a fuzzer checks, and
//! writing it once is what keeps the two lanes from drifting apart.
//!
//! Every law takes ranges already inside [`UNIVERSE`], so an arbitrary byte string from a fuzz
//! driver maps to a bounded, cheap case rather than a multi-gigabyte allocation. Normalising is
//! the *caller's* job — [`normalise`] does it — deliberately: a law that normalised its own
//! inputs would feed the same value to the model and to the code under test, and a defect in the
//! normaliser would cancel on both sides. That is the self-consistency this whole policy warns
//! about, and the pilot should not model it.
//!
//! This module is the test oracle, not the system under test, so `.cargo/mutants.toml` excludes
//! `crates/*/src/invariants.rs` from mutation. Every new `invariants` module inherits that.

use std::collections::BTreeMap;

use crate::entry::Ifd;
use crate::segment::Range;
use crate::track::ReadLedger;
use crate::value::Value;

/// The size of the offset space these laws are checked over.
///
/// Ledger arithmetic is uniform in the offset, so a bounded universe costs no generality while
/// keeping every case allocation-bounded for a fuzz driver.
pub const UNIVERSE: u64 = 4096;

/// The longest span a normalised input can describe.
const MAX_LEN: u64 = 256;

/// A law that did not hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    law: &'static str,
    detail: String,
}

impl Violation {
    /// Builds a violation of `law`.
    fn new(law: &'static str, detail: String) -> Self {
        Self { law, detail }
    }

    /// The name of the law that was broken.
    #[must_use]
    pub fn law(&self) -> &'static str {
        self.law
    }

    /// What broke it, in terms the failing case can be read from.
    #[must_use]
    pub fn detail(&self) -> &str {
        &self.detail
    }
}

impl core::fmt::Display for Violation {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}: {}", self.law, self.detail)
    }
}

/// Maps an arbitrary range wholly inside [`UNIVERSE`], preserving zero length.
///
/// Callers hand the result to the laws below, which assume it. The result never runs past the
/// universe, so the model and the code under test see exactly the same bytes — clipping one but
/// not the other would manufacture disagreements that are artefacts of the harness.
///
/// Zero-length ranges survive normalisation deliberately: they are the input class that separates
/// a claim filter that drops them from one that lets them through.
#[must_use]
pub fn normalise(range: Range) -> Range {
    let start = range.start % UNIVERSE;
    Range {
        start,
        len: (range.len % (MAX_LEN + 1)).min(UNIVERSE - start),
    }
}

/// Marks every byte offset a normalised range covers.
fn mark(set: &mut [bool], range: Range) {
    let start = usize::try_from(range.start).unwrap_or(set.len());
    let end = usize::try_from(range.end()).unwrap_or(set.len());
    for byte in set.iter_mut().take(end).skip(start) {
        *byte = true;
    }
}

/// The byte set a range list covers.
fn to_set(ranges: &[Range]) -> Vec<bool> {
    let mut set = vec![false; UNIVERSE as usize];
    for &r in ranges {
        mark(&mut set, r);
    }
    set
}

/// Non-empty, strictly ascending, and never adjacent — the shape both the ledger and the
/// subtraction promise.
///
/// Adjacency is the interesting half: two touching ranges describe the same byte set as one, so
/// a coalescing defect is invisible to a set-equality check on its own.
fn canonical_form(ranges: &[Range], law: &'static str) -> Result<(), Violation> {
    for (i, r) in ranges.iter().enumerate() {
        if r.len == 0 {
            return Err(Violation::new(law, format!("empty range at index {i}")));
        }
        if let Some(prev) = i.checked_sub(1).and_then(|p| ranges.get(p))
            && prev.end() >= r.start
        {
            return Err(Violation::new(
                law,
                format!("{prev:?} and {r:?} are adjacent or overlapping"),
            ));
        }
    }
    Ok(())
}

/// Every span a ledger reports is non-empty, sorted and separated from its neighbour, the
/// recorded bytes are exactly the bytes read, and [`ReadLedger::contains`] agrees with them.
///
/// `reads` must already lie inside [`UNIVERSE`] — pass them through [`normalise`] first.
///
/// # Errors
///
/// Returns the first [`Violation`] found.
pub fn ledger_is_canonical(reads: &[Range]) -> Result<(), Violation> {
    let mut ledger = ReadLedger::new();
    let mut model = vec![false; UNIVERSE as usize];
    for &r in reads {
        ledger.record(r.start, r.len);
        mark(&mut model, r);
    }

    canonical_form(ledger.spans(), "ledger_is_canonical")?;

    if to_set(ledger.spans()) != model {
        return Err(Violation::new(
            "ledger_is_canonical",
            format!(
                "recorded bytes differ from the bytes read: spans {:?}",
                ledger.spans()
            ),
        ));
    }

    // `contains` must agree with the model at the boundaries of every recorded span — the
    // offsets a fencepost error lives at — rather than over some fixed window the generated
    // reads would usually miss. The zero-length query is included: it is vacuously contained.
    for span in ledger.spans() {
        for probe in [
            Range {
                start: span.start,
                len: 0,
            },
            Range {
                start: span.start,
                len: 1,
            },
            *span,
            Range {
                start: span.start,
                len: span.len + 1,
            },
            Range {
                start: span.start.saturating_sub(1),
                len: 2,
            },
            Range {
                start: span.end().saturating_sub(1),
                len: 2,
            },
        ] {
            let want = (probe.start..probe.end()).all(|b| model.get(b as usize) == Some(&true));
            if ledger.contains(probe) != want {
                return Err(Violation::new(
                    "ledger_is_canonical",
                    format!("contains({probe:?}) disagrees with the recorded bytes (want {want})"),
                ));
            }
        }
    }
    Ok(())
}

/// [`ReadLedger::subtract`] returns exactly the recorded bytes that no claim covers, in
/// canonical form — however the claims are ordered, overlapped, duplicated or split, and
/// including zero-length claims, which cover nothing and must therefore split nothing.
///
/// `reads` and `claims` must already lie inside [`UNIVERSE`] — pass them through [`normalise`]
/// first.
///
/// # Errors
///
/// Returns the first [`Violation`] found.
pub fn subtract_is_set_difference(reads: &[Range], claims: &[Range]) -> Result<(), Violation> {
    let mut ledger = ReadLedger::new();
    let mut read_bytes = vec![false; UNIVERSE as usize];
    for &r in reads {
        ledger.record(r.start, r.len);
        mark(&mut read_bytes, r);
    }

    let mut claimed = vec![false; UNIVERSE as usize];
    for &c in claims {
        mark(&mut claimed, c);
    }

    let out = ledger.subtract(claims);
    canonical_form(&out, "subtract_is_set_difference")?;

    let got = to_set(&out);
    for byte in 0..UNIVERSE as usize {
        let want = read_bytes[byte] && !claimed[byte];
        if got[byte] != want {
            return Err(Violation::new(
                "subtract_is_set_difference",
                format!(
                    "byte {byte}: subtract says {}, the set difference says {want} \
                     (spans {:?}, claims {claims:?})",
                    got[byte],
                    ledger.spans()
                ),
            ));
        }
    }
    Ok(())
}

/// No directory in the tree rooted at `ifd` holds a tag both as a field and as a sub-IFD group.
///
/// This is the [`Ifd`] contract (*One tag, one store*): the writer emits one entry per field and
/// one per group, so a tag in both stores reaches disk as two entries under one tag, which TIFF
/// 6.0 §2 does not allow. It is the shape a reader misreads, so it is checked over the **fields**
/// a reader sees as well as the groups a caller built, at every level of the tree.
///
/// # Errors
///
/// Returns the first [`Violation`] found, naming the tag.
pub fn one_store_per_tag(ifd: &Ifd) -> Result<(), Violation> {
    for group in ifd.sub_ifds() {
        if ifd.get(group.tag).is_some() {
            return Err(Violation::new(
                "one_store_per_tag",
                format!("tag {} is both a field and a sub-IFD group", group.tag),
            ));
        }
        for child in &group.ifds {
            one_store_per_tag(child)?;
        }
    }
    Ok(())
}

/// One call on an [`Ifd`]'s setter surface, for [`setters_keep_one_store_per_tag`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edit {
    /// [`Ifd::set`] with a placeholder `LONG` value.
    Set(u16),
    /// [`Ifd::set_sub_ifd`] with one empty child.
    SetSubIfd(u16),
    /// [`Ifd::add_sub_ifd`] with one empty child.
    AddSubIfd(u16),
    /// [`Ifd::remove`], which removes a field and never a group.
    Remove(u16),
}

/// Which of the two stores a model tag lives in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Store {
    Field,
    Group,
}

/// After any sequence of setter calls, each tag sits in exactly the store its last setter named —
/// the later call wins — and so no tag is ever in both ([`one_store_per_tag`]).
///
/// [`Edit::Remove`] removes a field only, so a group survives it: that is `remove`'s documented
/// scope, and the model follows it rather than the setters'.
///
/// # Errors
///
/// Returns the first [`Violation`] found.
pub fn setters_keep_one_store_per_tag(edits: &[Edit]) -> Result<(), Violation> {
    let mut ifd = Ifd::new();
    let mut model: BTreeMap<u16, Store> = BTreeMap::new();
    for &edit in edits {
        match edit {
            Edit::Set(tag) => {
                ifd.set(tag, Value::Long(vec![u32::from(tag)]));
                model.insert(tag, Store::Field);
            }
            Edit::SetSubIfd(tag) => {
                ifd.set_sub_ifd(tag, vec![Ifd::new()]);
                model.insert(tag, Store::Group);
            }
            Edit::AddSubIfd(tag) => {
                ifd.add_sub_ifd(tag, Ifd::new());
                model.insert(tag, Store::Group);
            }
            Edit::Remove(tag) => {
                ifd.remove(tag);
                if model.get(&tag) == Some(&Store::Field) {
                    model.remove(&tag);
                }
            }
        }
    }

    one_store_per_tag(&ifd).map_err(|v| {
        Violation::new(
            "setters_keep_one_store_per_tag",
            format!("{} after {edits:?}", v.detail()),
        )
    })?;

    let tags_in = |store: Store| -> Vec<u16> {
        model
            .iter()
            .filter(|&(_, &s)| s == store)
            .map(|(&t, _)| t)
            .collect()
    };
    let fields: Vec<u16> = ifd.fields().iter().map(|f| f.tag).collect();
    let groups: Vec<u16> = ifd.sub_ifds().iter().map(|s| s.tag).collect();
    if fields != tags_in(Store::Field) || groups != tags_in(Store::Group) {
        return Err(Violation::new(
            "setters_keep_one_store_per_tag",
            format!(
                "fields {fields:?} and groups {groups:?} differ from the last-call-wins model \
                 {model:?} after {edits:?}"
            ),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use proptest::collection;
    use proptest::prelude::{Just, Strategy, prop_assert_eq, prop_oneof, proptest};
    use proptest::test_runner::{Config, RngSeed};

    use super::{
        Edit, MAX_LEN, Range, UNIVERSE, canonical_form, ledger_is_canonical, normalise,
        one_store_per_tag, setters_keep_one_store_per_tag, subtract_is_set_difference,
    };
    use crate::entry::Ifd;
    use crate::value::Value;

    /// The workspace property-test configuration (`docs/testing.md`).
    ///
    /// The seed is **pinned**: the `--in-diff` mutation gate is blocking, so a test whose
    /// pass/fail depends on OS entropy would make cargo-mutants report CAUGHT or MISSED for the
    /// same mutant on different runs — and the equivalence proofs in `.cargo/mutants.toml` are
    /// only meaningful against a deterministic suite. The shrink bounds are finite because
    /// proptest's defaults are `u32::MAX` iterations and *no* wall-clock cap, and shrinking runs
    /// on the failing path, which is most of what a mutation survey does.
    ///
    /// Failure persistence is **off**, and that is part of the same contract. cargo-mutants
    /// reuses one tree copy across many mutants, restoring only the file it mutated — so a
    /// `proptest-regressions` file written by one mutant's failure would survive into the next
    /// mutant's run and be replayed first, making CAUGHT/MISSED depend on mutant ordering and
    /// shard assignment. A shrunk counterexample is promoted into a named deterministic test
    /// instead, which is where its regression value belongs.
    fn config() -> Config {
        Config {
            cases: 512,
            max_shrink_iters: 2048,
            max_shrink_time: 10_000,
            rng_seed: RngSeed::Fixed(0x6761_6D75_745F_6966),
            failure_persistence: None,
            ..Config::default()
        }
    }

    /// A range inside [`UNIVERSE`], ready to hand to a law.
    ///
    /// Normalising here rather than inside the laws is deliberate: a law that normalised its own
    /// inputs would feed the same value to the model and to the code under test, so a defect in
    /// the normaliser would cancel on both sides.
    ///
    /// Zero length is drawn deliberately often (~1 in 7): a zero-length *claim* covers nothing
    /// and must therefore split nothing, and that is the input class separating a claim filter
    /// that drops empty ranges from one that keeps them. Uniform `u64` lengths would reach it
    /// about once in 257 draws.
    fn range() -> impl Strategy<Value = Range> {
        (
            0..UNIVERSE,
            prop_oneof![1 => Just(0u64), 6 => 1u64..=MAX_LEN],
        )
            .prop_map(|(start, len)| normalise(Range { start, len }))
    }

    proptest! {
        #![proptest_config(config())]

        /// `record` coalesces into a canonical span set that holds exactly the bytes read.
        #[test]
        fn recorded_spans_are_canonical(reads in collection::vec(range(), 0..24)) {
            prop_assert_eq!(ledger_is_canonical(&reads), Ok(()));
        }

        /// `subtract` is the set difference, in canonical form, for any claim list.
        #[test]
        fn subtract_is_the_set_difference(
            reads in collection::vec(range(), 0..24),
            claims in collection::vec(range(), 0..24),
        ) {
            prop_assert_eq!(subtract_is_set_difference(&reads, &claims), Ok(()));
        }

        /// The setters keep every tag in one store, the one its last setter named.
        #[test]
        fn setters_keep_one_store_per_tag_for_any_edit_sequence(
            edits in collection::vec(edit(), 0..24),
        ) {
            prop_assert_eq!(setters_keep_one_store_per_tag(&edits), Ok(()));
        }

    }

    /// One setter call over a four-tag alphabet, so the same tag is revisited — by a different
    /// setter — often enough that every store-to-store transition is drawn.
    fn edit() -> impl Strategy<Value = Edit> {
        (0u8..4, 0u16..4).prop_map(|(kind, i)| {
            let tag = 330 + i;
            match kind {
                0 => Edit::Set(tag),
                1 => Edit::SetSubIfd(tag),
                2 => Edit::AddSubIfd(tag),
                _ => Edit::Remove(tag),
            }
        })
    }

    #[test]
    fn one_store_per_tag_rejects_a_tag_in_both_stores_at_any_depth() {
        // Only `sub_ifds_mut` can build this shape: retag a group onto a field's tag.
        let mut conflicted = Ifd::new();
        conflicted.set(256, Value::Long(vec![8]));
        conflicted.set_sub_ifd(330, vec![Ifd::new()]);
        conflicted.sub_ifds_mut()[0].tag = 256;
        let err = one_store_per_tag(&conflicted).expect_err("both stores must be rejected");
        assert_eq!(err.law(), "one_store_per_tag");
        assert!(err.detail().contains("tag 256"), "{err}");

        // The same conflict one level down is found too.
        let mut root = Ifd::new();
        root.set_sub_ifd(34665, vec![conflicted]);
        assert!(one_store_per_tag(&root).is_err());
    }

    #[test]
    fn setters_keep_one_store_per_tag_follows_the_last_call() {
        // Each transition the property draws, pinned once: field -> group -> field, and a
        // `remove` that leaves a group alone.
        for edits in [
            vec![Edit::Set(330), Edit::SetSubIfd(330)],
            vec![Edit::SetSubIfd(330), Edit::Set(330)],
            vec![Edit::Set(330), Edit::AddSubIfd(330), Edit::AddSubIfd(330)],
            vec![Edit::AddSubIfd(330), Edit::Remove(330)],
            vec![Edit::Set(330), Edit::Remove(330)],
        ] {
            assert_eq!(setters_keep_one_store_per_tag(&edits), Ok(()), "{edits:?}");
        }
    }

    // The laws' own failure paths. A law that cannot report a violation is not a law, and under
    // an unmutated `track.rs` the properties above only ever exercise the `Ok` arm — so without
    // these the error arms, the accessors and `Display` are dead regions the coverage gate counts
    // and no test would notice being deleted.

    #[test]
    fn canonical_form_rejects_an_empty_range() {
        let bad = [Range { start: 4, len: 0 }];
        let err = canonical_form(&bad, "law").expect_err("empty range must be rejected");
        assert_eq!(err.law(), "law");
        assert!(err.detail().contains("empty range at index 0"), "{err}");
    }

    #[test]
    fn canonical_form_rejects_adjacent_ranges() {
        // Touching, not overlapping: the case a byte-set comparison cannot see, and the one the
        // zero-length-claim defect in `subtract` produced.
        let bad = [Range { start: 0, len: 4 }, Range { start: 4, len: 4 }];
        let err = canonical_form(&bad, "law").expect_err("adjacent ranges must be rejected");
        assert!(err.detail().contains("adjacent or overlapping"), "{err}");
        assert_eq!(format!("{err}"), format!("law: {}", err.detail()));
    }

    #[test]
    fn canonical_form_rejects_descending_ranges() {
        let bad = [Range { start: 8, len: 2 }, Range { start: 0, len: 2 }];
        assert!(canonical_form(&bad, "law").is_err());
    }

    #[test]
    fn normalise_keeps_every_range_inside_the_universe() {
        for raw in [
            Range {
                start: u64::MAX,
                len: u64::MAX,
            },
            Range {
                start: UNIVERSE - 1,
                len: MAX_LEN,
            },
            Range { start: 0, len: 0 },
        ] {
            let r = normalise(raw);
            assert!(r.start < UNIVERSE && r.end() <= UNIVERSE, "{r:?}");
        }
        // Zero length must survive: it is the input class the claim filter turns on.
        assert_eq!(normalise(Range { start: 7, len: 0 }).len, 0);
    }
}
