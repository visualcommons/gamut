//! The encoder's effort ladder: [`Effort`], the named rungs, and the knob values each composes.
//!
//! Split from `encoder` (issue #1094) because the ladder changes for its own reason — a rung
//! re-tuned against the corpus — and never with the encode pipeline it configures.
//! [`PngEncoder::with_effort`] applies a rung; it destructures [`EffortKnobs`] exhaustively, so
//! a knob added here fails to compile until the setter assigns it.

use gamut_deflate::{DeflateEncoder, Level};

#[cfg(doc)]
use crate::PngEncoder;
use crate::filter::{FilterStrategy, FilterType};

/// One rung of the encoder's size/time ladder: a named setting for all five of the knobs that
/// trade encoding time for output size (issue #484).
///
/// The knobs — [`with_compression`](PngEncoder::with_compression),
/// [`with_deflate_passes`](PngEncoder::with_deflate_passes), [`with_filter`](PngEncoder::with_filter),
/// [`with_optimal_parse_limit`](PngEncoder::with_optimal_parse_limit) and
/// [`with_auto_reduce`](PngEncoder::with_auto_reduce) — are independent, and nothing mapped one
/// choice onto a sensible combination of them: a caller wanting the smallest file had to know that
/// it means [`Level::Best`] *and* [`FilterStrategy::BruteForce`] *and* auto-reduce. A rung is that
/// knowledge, named. [`with_effort`](PngEncoder::with_effort) applies one.
///
/// # What a rung does not touch
///
/// [`with_transparent_cleanup`](PngEncoder::with_transparent_cleanup) is deliberately absent from
/// every rung. It is this crate's one *lossy* knob — it rewrites stored samples no decoder renders
/// — and a dial named for effort must never be the thing that silently changes what a file stores.
/// Enable it beside a rung when you want it. Ancillary chunks, metadata and pushed backends are
/// untouched for the same reason: they are what the file *says*, not how hard the encoder worked.
///
/// # Choosing a rung, not a setting
///
/// The rungs are the contract; which knob values each one selects is not, and may be re-tuned as
/// this crate's corpus grows. Two things are fixed. [`Effort::Balanced`] encodes byte-identically
/// to a default [`PngEncoder`]; and every rung is strictly smaller than the rung above it **summed
/// over this crate's corpus** — which is deliberately not a per-row promise, because per row it is
/// false. A rung that fixes one filter can beat a rung that searches per row on a picture that
/// suits it, and one corpus row measures exactly that. See `crates/gamut-png/tests/effort.rs` for
/// what is gated and `STATUS.md` for the measured tables.
///
/// The discriminants are a permanent, append-only part of the contract: they are what
/// [`level`](Self::level) and [`from_level`](Self::from_level) round trip, and what a numeric CLI
/// or FFI knob carries. Non-exhaustive, so a later rung is not a breaking change — match with a
/// wildcard arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(u8)]
#[non_exhaustive]
pub enum Effort {
    /// Level 0 — encode quickly and accept a larger file: greedy matching, and one fixed filter
    /// rather than a per-row filter search.
    Fast = 0,
    /// Level 1 — what a default [`PngEncoder`] already does, and the balanced speed/size point.
    #[default]
    Balanced = 1,
    /// Level 2 — the optimal parse and lossless reduction, still on one filter heuristic.
    Small = 2,
    /// Level 3 — the whole-image filter search on top of [`Effort::Small`], plus zopfli's own
    /// refinement budget and an eight-times-wider optimal-parse span.
    ///
    /// Slowest by a wide margin, and the margin is the filter search: on this crate's corpus the
    /// step from `Small` costs **6.26x** and buys 3.6%, and on four of the nine rows it buys three
    /// bytes or fewer. Intended for write-once assets where size dominates, and worth measuring on
    /// your own material rather than assuming — see `STATUS.md`.
    ///
    /// Also the most memory-hungry rung: the filter search's eight candidates run concurrently,
    /// each with its own whole-image filtered copy and 8 MiB-span parse, so peak memory grows with
    /// image size times the candidate count. A 2048x2048 RGB photograph peaked near 1115 MiB
    /// resident where the same search run serially peaked near 229 MiB (both at refinement budget
    /// 1; `STATUS.md` has the table).
    Smallest = 3,
}

/// The knob values one [`Effort`] rung selects.
///
/// Crate-private, and exhaustively destructured by [`PngEncoder::with_effort`], so adding a knob
/// to the ladder fails to compile until every rung says what it does with it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct EffortKnobs {
    pub(crate) level: Level,
    pub(crate) deflate_passes: u8,
    pub(crate) filter: FilterStrategy,
    pub(crate) optimal_parse_limit: usize,
    pub(crate) auto_reduce: bool,
}

/// The optimal-parse span [`Effort::Smallest`] selects: **8 MiB**, eight times
/// [`DeflateEncoder::DEFAULT_OPTIMAL_PARSE_LIMIT`].
///
/// Finite, and that is the point. The span is the only thing bounding the shortest-path parse's
/// working set: `gamut-deflate` allocates three span-length vectors per refinement pass — the cost
/// row, the chosen length and the chosen distance — for 12 bytes per byte of span, and since #959
/// it also holds the span's match cache, capped at 10 bytes per byte of span: **up to 22 bytes
/// per byte of span** in all (20.3 to 20.7 measured resident; the two `u16` rows are
/// zero-initialised, so pages they never write are never faulted in). An unbounded span therefore
/// makes one encode's peak memory grow without bound in the image, which is not something to ship
/// behind a rung named for size. Measured peak resident set for one encode of a 4096x4096 RGB
/// photograph at [`Level::Best`]: **1130.8 MiB** unbounded against **290.9 MiB** at this span and
/// **178.6 MiB** at the 1 MiB default.
///
/// Those are **one** stream's figures, and the rung does not parse one stream at a time. Its
/// [`FilterStrategy::BruteForce`] runs eight candidates concurrently (#624), each with its own
/// filtered image and its own span-length buffers, so the rung's own peak is several times a
/// single encode's. Measured for a whole `BruteForce` encode at this span, refinement budget 1,
/// concurrent against the serial search it replaced: **424.0 against 84.2 MiB** at 1024x1024,
/// **1114.5 against 228.8 MiB** at 2048x2048, and **1431.9 against 533.0 MiB** at 4096x4096. The
/// concurrent figures are typical rather than a bound, since they depend on scheduling. The span
/// bounds only each candidate's parse buffers: each candidate also holds a whole-image filtered
/// copy (`filter::filter_image` in `compress_scanlines`), so the rung's peak memory is **not**
/// bounded by the span — it grows with image size times the number of concurrent candidates.
///
/// Eight and not more, measured rather than picked: a wider span only helps an image whose
/// *filtered* stream exceeds it, and the help saturates fast. At 8 MiB the span is the whole
/// stream — so byte-identical to unbounded — for any image up to about 1670x1670 RGB or
/// 1448x1448 RGBA, which is where every size win this crate measured for the knob lives. Past
/// that it keeps most of what is left: on a 2048x2048 gradient it takes −0.100% of the −0.106%
/// unbounded reaches, and on a 4096x4096 photograph −0.028% of −0.044%. Doubling to 16 MiB buys
/// a further 0.005% there and costs another 164.9 MiB, which is the trade this constant declines.
/// `STATUS.md` carries the whole sweep, including the rows where a wider span costs bytes.
///
/// Both ends of that — wider than the default, and finite — are what
/// `the_smallest_rungs_parse_span_sits_between_the_default_and_no_bound_at_all` asserts. Neither is
/// observable through an encode at corpus size, where every candidate span already exceeds the
/// filtered stream.
const SMALLEST_OPTIMAL_PARSE_LIMIT: usize = 8 << 20;

impl Effort {
    /// The ladder level (`0..=3`) this rung selects, lower being faster.
    #[must_use]
    pub const fn level(self) -> u8 {
        self as u8
    }

    /// The [`Effort`] for a ladder level, or `None` if `level` is outside `0..=3`.
    ///
    /// The inverse of [`Effort::level`]; handy for wiring up a numeric CLI flag.
    #[must_use]
    pub const fn from_level(level: u8) -> Option<Self> {
        Some(match level {
            0 => Self::Fast,
            1 => Self::Balanced,
            2 => Self::Small,
            3 => Self::Smallest,
            _ => return None,
        })
    }

    /// The knob values this rung composes — the ladder's one definition of itself.
    pub(crate) const fn knobs(self) -> EffortKnobs {
        match self {
            // Greedy matching, and one fixed filter rather than a per-row search.
            //
            // `Paeth` and not `FilterStrategy::None`, which is the obvious guess and is measurably
            // wrong: skipping the filter hands DEFLATE a stream so much larger that the compressor
            // loses more time than the filter pass saves. Over the corpus in `tests/effort.rs`,
            // `None` was both the **largest** result (46 267 bytes against 17 530) and *slower*
            // than the fixed Paeth predictor — a dominated candidate, not a trade. `Fixed(Paeth)`
            // in turn dominates `MinSumAbs` here (smaller *and* faster), and beats a fixed `Up`
            // on size; see `STATUS.md`.
            Self::Fast => EffortKnobs {
                level: Level::Fast,
                deflate_passes: 0,
                filter: FilterStrategy::Fixed(FilterType::Paeth),
                optimal_parse_limit: DeflateEncoder::DEFAULT_OPTIMAL_PARSE_LIMIT,
                auto_reduce: false,
            },
            // Deliberately spelled out rather than read back from `PngEncoder::new`: that the two
            // agree is a claim `the_balanced_rung_is_a_default_encoder` tests, not a tautology
            // this function arranges.
            Self::Balanced => EffortKnobs {
                level: Level::Default,
                deflate_passes: DeflateEncoder::DEFAULT_DEFLATE_PASSES,
                filter: FilterStrategy::MinSumAbs,
                optimal_parse_limit: DeflateEncoder::DEFAULT_OPTIMAL_PARSE_LIMIT,
                auto_reduce: false,
            },
            // The two structural wins — the optimal parse and lossless reduction — without the
            // whole-image filter search, which costs one full DEFLATE per candidate.
            Self::Small => EffortKnobs {
                level: Level::Best,
                deflate_passes: DeflateEncoder::DEFAULT_DEFLATE_PASSES,
                filter: FilterStrategy::MinSumAbs,
                optimal_parse_limit: DeflateEncoder::DEFAULT_OPTIMAL_PARSE_LIMIT,
                auto_reduce: true,
            },
            // `zopfli`'s own refinement budget of 15, the full brute-force filter search, and the
            // widest optimal-parse span this crate will spend memory on — see
            // `SMALLEST_OPTIMAL_PARSE_LIMIT` for why that span is finite and why it is this wide.
            Self::Smallest => EffortKnobs {
                level: Level::Best,
                deflate_passes: 15,
                filter: FilterStrategy::BruteForce,
                optimal_parse_limit: SMALLEST_OPTIMAL_PARSE_LIMIT,
                auto_reduce: true,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_effort_level_round_trips_and_the_ladder_is_contiguous() {
        // `from_level` is the mechanical enumeration of the ladder — `tests/effort.rs` walks it
        // from 0 until it yields `None`, so the rungs must be contiguous from 0 and the first
        // gap must be the end. Extending the ladder without extending this range fails here.
        for level in 0..=3u8 {
            let preset = Effort::from_level(level).expect("0..=3 is in range");
            assert_eq!(preset.level(), level, "level {level} does not round trip");
        }
        assert_eq!(Effort::from_level(4), None, "the ladder ends at 3");
        assert_eq!(Effort::from_level(u8::MAX), None);
    }

    #[test]
    fn the_smallest_rungs_parse_span_sits_between_the_default_and_no_bound_at_all() {
        // Both ends are the point of the knob. Wider than the default, or the rung is not asking
        // for anything; finite, or one encode's parse state grows without bound in the image, at
        // up to 22 bytes per byte of span. Asserted on the value rather than through an encode because
        // it is not observable through one: this crate's corpus filters to less than the 32 KiB
        // span floor, so every candidate span already covers the whole stream and every rung
        // emits the same bytes whatever the limit says.
        let span = Effort::Smallest.knobs().optimal_parse_limit;
        assert!(
            span > DeflateEncoder::DEFAULT_OPTIMAL_PARSE_LIMIT && span < usize::MAX,
            "the top rung's parse span is {span}, not between the {} default and no bound at all",
            DeflateEncoder::DEFAULT_OPTIMAL_PARSE_LIMIT
        );
    }

    #[test]
    fn the_default_effort_is_the_balanced_rung() {
        // `Effort::default()` is what a `#[derive(Default)]` config struct or an unset FFI field
        // lands on, and it must be the rung that changes nothing.
        assert_eq!(Effort::default(), Effort::Balanced);
    }
}
