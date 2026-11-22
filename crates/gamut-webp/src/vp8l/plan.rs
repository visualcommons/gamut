//! The VP8L encoder's candidate-plan ladder (issue #31).
//!
//! Every knob the encoder can turn lives in a [`Vp8lPlan`], so encoding under a plan is a pure,
//! deterministic function of `(pixels, dimensions, plan)`. [`enumerate`] then maps an [`Effort`]
//! onto the list of plans to try, and the driver in [`super::encoder`] encodes each one and keeps
//! the shortest.
//!
//! # Why the ladder is monotone
//!
//! [`enumerate`] is **append-only**: `enumerate(e - 1)` is a prefix of `enumerate(e)`, built by
//! extending the previous rung's list rather than replacing it. The driver keeps the shortest
//! encoding and breaks ties toward the earlier plan, so a level-`e` result is the minimum over a
//! superset of the level-`e-1` candidates. Output size is therefore non-increasing in effort **for
//! every image, by construction** rather than by measurement — and byte-identical whenever nothing
//! new helps, which keeps the upper rungs stable on content they cannot improve.
//!
//! This is why the search is one flat list of *complete* plans rather than two stages ("pick a
//! transform chain, then refine the parse"). A staged search can have its stage-one winner lose
//! under the refined parse, which would break the nesting the guarantee rests on. It is also why
//! candidates are only ever **added**: a deeper LZ77 chain finds longer matches, and a longer match
//! at a farther distance can cost more bits than a shorter near one, so the shallow-chain plan has
//! to stay in the list forever rather than being replaced.
//!
//! # Quality: the second axis (issue #1240)
//!
//! libwebp drives its lossless search with two dials, `method` and `quality`; `cwebp -z N` sets
//! both. Here effort picks *which kinds* of candidate a rung races (transform chains, palette
//! orders, groupings), and quality caps *how far each candidate searches*: every plan has a
//! [`min_quality`], derived from its two search-depth knobs — the LZ77 hash-chain depth and how
//! far its colour-cache size strays from the heuristic — and [`enumerate`] keeps only the plans
//! whose floor the requested quality reaches.
//!
//! Filtering never reorders, so the guarantee carries over to both axes at once. At a fixed
//! quality, rung `e - 1`'s list is still a prefix of rung `e`'s; at a fixed effort, the list at
//! quality `q - 1` is a subsequence of the list at `q`. Output size is therefore non-increasing in
//! effort *and* in quality. Every plan the ladder held before quality reached it has a floor of at
//! most [`DEFAULT_QUALITY`], and the plans quality adds above it sit at the end of their rung with
//! floors above it, so at the default quality every rung enumerates exactly its historical list.

use crate::config::{DEFAULT_QUALITY, Effort};

/// Block-size exponent used for the predictor and colour sub-images by default (16×16 blocks).
pub(crate) const DEFAULT_TRANSFORM_BITS: u8 = 4;

/// Block-size exponent used for the meta-prefix (entropy) image by default (16×16 meta-blocks).
pub(crate) const DEFAULT_PREFIX_BITS: u32 = 4;

/// How a plan orders the colour-indexing (palette) transform's entries.
///
/// The palette is stored subtraction-coded onto the previous entry, and the index image is then
/// spatially predicted, so the ordering changes both the palette's own cost and how well the index
/// image compresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PaletteOrder {
    /// Distinct colours in the order they first appear — the cheapest to build.
    FirstSeen,
    /// Sorted by the packed `0xAARRGGBB` value, which shrinks the subtraction-coded palette and
    /// puts similar colours on adjacent indices.
    Ascending,
}

/// The transform chain a plan emits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Structure {
    /// Take the palette path when the image has few enough distinct colours, else the spatial path
    /// with the full transform chain. The rung-0 spine, and the encoder's historical behaviour.
    Auto,
    /// The colour-indexing (palette) path. Only applicable when a palette exists.
    Palette {
        /// How to order the palette entries.
        order: PaletteOrder,
    },
    /// The spatial path, with each transform independently present or absent.
    ///
    /// Emitting a transform is not free — the predictor and colour transforms each carry a
    /// sub-resolution image — so a transform that does not pay for itself is better left out
    /// entirely. That is especially true of the green-only images the `ALPH` chunk codes, where
    /// subtract-green actively destroys two constant channels.
    Spatial {
        /// Whether to apply the subtract-green transform.
        subtract_green: bool,
        /// Block-size exponent for the predictor transform, or `None` to omit it.
        predictor: Option<u8>,
        /// Block-size exponent for the colour transform, or `None` to omit it.
        color: Option<u8>,
    },
}

/// How many bits of colour cache a plan uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CacheBits {
    /// The size heuristic applied to the image actually being coded.
    Auto,
    /// The heuristic shifted by a signed delta, clamped into the spec's `1..=11` (or off at 0).
    AutoDelta(i8),
    /// No colour cache at all.
    Off,
}

/// How a plan splits the image into prefix-code groups.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Grouping {
    /// One prefix-code group for the whole image — no entropy image, no per-group overhead.
    Single,
    /// Group meta-blocks by their most frequent green symbol.
    Signature {
        /// Block-size exponent for the entropy image.
        prefix_bits: u32,
    },
}

/// The LZ77 match-finder's search budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Lz77Params {
    /// Maximum hash-chain length walked per position.
    pub max_chain: usize,
    /// Whether to defer a match when the next position starts a longer one (lazy matching).
    pub lazy: bool,
}

/// One complete VP8L encoding configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Vp8lPlan {
    /// The transform chain to emit.
    pub structure: Structure,
    /// The colour-cache size to use.
    pub cache: CacheBits,
    /// How to split the image into prefix-code groups.
    pub grouping: Grouping,
    /// The LZ77 search budget.
    pub lz77: Lz77Params,
}

/// The rung-0 spine: the single plan every higher rung's candidate list starts from.
const SPINE: Vp8lPlan = Vp8lPlan {
    structure: Structure::Auto,
    cache: CacheBits::Auto,
    grouping: Grouping::Signature {
        prefix_bits: DEFAULT_PREFIX_BITS,
    },
    lz77: Lz77Params {
        max_chain: 32,
        lazy: false,
    },
};

/// Hard ceiling on the number of candidates each rung may enumerate at quality `100`, so encode
/// cost cannot grow silently as the ladder fills in.
pub(crate) const MAX_PLANS: [usize; 7] = [3, 8, 14, 21, 29, 39, 50];

/// The full spatial chain at the default block size — the shape `Structure::Auto` falls back to.
const FULL_SPATIAL: Structure = Structure::Spatial {
    subtract_green: true,
    predictor: Some(DEFAULT_TRANSFORM_BITS),
    color: Some(DEFAULT_TRANSFORM_BITS),
};

/// The candidate plans for `effort` at lossless `quality` (`0..=100`; above `100` behaves as
/// `100`), in evaluation order.
///
/// Append-only by construction in effort, and filtered without reordering by quality, which is
/// what makes output size non-increasing in both (see the module docs).
#[must_use]
pub(crate) fn enumerate(effort: Effort, quality: u8) -> Vec<Vp8lPlan> {
    let quality = quality.min(100);
    let mut plans = vec![SPINE];
    for level in 0..=effort.level() {
        plans.extend(
            added_at(level)
                .into_iter()
                .chain(refined_at(level))
                .filter(|plan| min_quality(plan) <= quality),
        );
    }
    debug_assert!(
        plans.len() <= MAX_PLANS[effort.level() as usize],
        "effort {} enumerated {} plans, over its ceiling",
        effort.level(),
        plans.len()
    );
    plans
}

/// The lowest lossless quality at which `plan` is offered: the higher of its LZ77 floor and its
/// colour-cache floor.
///
/// The floors rise with how far each knob departs from the spine's cheap default. An LZ77 parse
/// that is lazy, or walks deeper chains, costs encode time on every candidate it touches; a cache
/// size away from the heuristic is a guess whose odds fall with its distance. The thresholds are
/// set so that every plan the ladder held before quality reached it (issue #1240) sits at or below
/// [`DEFAULT_QUALITY`] — the default's bytes do not move — and the plans quality alone adds
/// (chains past 128, cache deltas past 2) sit above it.
#[must_use]
pub(crate) fn min_quality(plan: &Vp8lPlan) -> u8 {
    let lz77 = match (plan.lz77.lazy, plan.lz77.max_chain) {
        (false, ..=32) => 0,
        (_, ..=16) => 25,
        (_, ..=128) => DEFAULT_QUALITY,
        (_, ..=192) => 90,
        _ => 100,
    };
    let cache = match plan.cache {
        CacheBits::Auto => 0,
        CacheBits::Off => 30,
        CacheBits::AutoDelta(delta) if delta.unsigned_abs() <= 2 => 50,
        CacheBits::AutoDelta(_) => 90,
    };
    lz77.max(cache)
}

/// `plan` with its LZ77 search made lazy at `max_chain`.
const fn with_lazy_chain(plan: Vp8lPlan, max_chain: usize) -> Vp8lPlan {
    Vp8lPlan {
        lz77: Lz77Params {
            max_chain,
            lazy: true,
        },
        ..plan
    }
}

/// A plan that differs from the spine only in its structure.
const fn with_structure(structure: Structure) -> Vp8lPlan {
    Vp8lPlan { structure, ..SPINE }
}

/// The plans only a quality above [`DEFAULT_QUALITY`] reaches, appended to rung `level` after
/// [`added_at`]'s: each takes one of the rung's shapes deeper than the default search goes — an
/// LZ77 chain past 128, or a colour cache three or more bits from the heuristic.
fn refined_at(level: u8) -> Vec<Vp8lPlan> {
    let no_green = Structure::Spatial {
        subtract_green: false,
        predictor: Some(DEFAULT_TRANSFORM_BITS),
        color: None,
    };
    let full = with_structure(FULL_SPATIAL);
    match level {
        0 => vec![with_lazy_chain(SPINE, 192), with_lazy_chain(SPINE, 256)],
        1 => vec![with_lazy_chain(full, 192), with_lazy_chain(full, 256)],
        2 => vec![
            with_lazy_chain(with_structure(no_green), 192),
            with_lazy_chain(
                with_structure(Structure::Palette {
                    order: PaletteOrder::Ascending,
                }),
                256,
            ),
        ],
        3 => vec![
            Vp8lPlan {
                cache: CacheBits::AutoDelta(-3),
                ..SPINE
            },
            Vp8lPlan {
                cache: CacheBits::AutoDelta(3),
                ..full
            },
        ],
        4 => vec![
            with_lazy_chain(
                Vp8lPlan {
                    grouping: Grouping::Single,
                    ..SPINE
                },
                192,
            ),
            with_lazy_chain(
                Vp8lPlan {
                    grouping: Grouping::Single,
                    ..full
                },
                256,
            ),
        ],
        5 => vec![
            Vp8lPlan {
                cache: CacheBits::AutoDelta(-3),
                ..full
            },
            with_lazy_chain(
                Vp8lPlan {
                    cache: CacheBits::AutoDelta(3),
                    ..SPINE
                },
                256,
            ),
        ],
        6 => vec![
            with_lazy_chain(
                Vp8lPlan {
                    cache: CacheBits::AutoDelta(-1),
                    ..full
                },
                256,
            ),
            with_lazy_chain(
                Vp8lPlan {
                    structure: no_green,
                    grouping: Grouping::Single,
                    cache: CacheBits::Off,
                    ..SPINE
                },
                256,
            ),
        ],
        _ => Vec::new(),
    }
}

/// The plans rung `level` adds on top of rung `level - 1`, at every quality their
/// [`min_quality`] reaches.
fn added_at(level: u8) -> Vec<Vp8lPlan> {
    match level {
        // Race the two paths against each other. `Auto` always takes the palette when one exists,
        // which is not always the denser choice; and the colour transform is pure overhead on
        // content whose channels are already decorrelated.
        1 => vec![
            with_structure(FULL_SPATIAL),
            with_structure(Structure::Spatial {
                subtract_green: true,
                predictor: Some(DEFAULT_TRANSFORM_BITS),
                color: None,
            }),
            with_structure(Structure::Palette {
                order: PaletteOrder::FirstSeen,
            }),
        ],
        // The green-only shape (`ALPH` payloads, masks): subtract-green turns two constant channels
        // into two copies of the negated green, so leaving it off is a large win there. Plus a
        // deeper, lazy parse.
        2 => vec![
            with_structure(Structure::Spatial {
                subtract_green: false,
                predictor: Some(DEFAULT_TRANSFORM_BITS),
                color: None,
            }),
            with_structure(Structure::Palette {
                order: PaletteOrder::Ascending,
            }),
            Vp8lPlan {
                lz77: Lz77Params {
                    max_chain: 16,
                    lazy: true,
                },
                ..SPINE
            },
            Vp8lPlan {
                structure: FULL_SPATIAL,
                lz77: Lz77Params {
                    max_chain: 16,
                    lazy: true,
                },
                ..SPINE
            },
        ],
        // Cache sizing: the heuristic is a guess, and one bit either way is often worth a percent.
        3 => vec![
            Vp8lPlan {
                cache: CacheBits::AutoDelta(-1),
                ..SPINE
            },
            Vp8lPlan {
                cache: CacheBits::AutoDelta(1),
                ..SPINE
            },
            Vp8lPlan {
                structure: FULL_SPATIAL,
                cache: CacheBits::AutoDelta(-1),
                ..SPINE
            },
            Vp8lPlan {
                structure: FULL_SPATIAL,
                cache: CacheBits::AutoDelta(1),
                ..SPINE
            },
            Vp8lPlan {
                structure: Structure::Spatial {
                    subtract_green: false,
                    predictor: Some(DEFAULT_TRANSFORM_BITS),
                    color: None,
                },
                cache: CacheBits::Off,
                ..SPINE
            },
        ],
        // Grouping and finer predictor blocks. A single group avoids the entropy image entirely,
        // which wins on small or statistically uniform images.
        4 => vec![
            Vp8lPlan {
                grouping: Grouping::Single,
                ..SPINE
            },
            Vp8lPlan {
                structure: FULL_SPATIAL,
                grouping: Grouping::Single,
                ..SPINE
            },
            with_structure(Structure::Spatial {
                subtract_green: true,
                predictor: Some(3),
                color: Some(DEFAULT_TRANSFORM_BITS),
            }),
            with_structure(Structure::Spatial {
                subtract_green: true,
                predictor: Some(3),
                color: None,
            }),
            Vp8lPlan {
                structure: FULL_SPATIAL,
                lz77: Lz77Params {
                    max_chain: 64,
                    lazy: true,
                },
                ..SPINE
            },
            Vp8lPlan {
                lz77: Lz77Params {
                    max_chain: 64,
                    lazy: true,
                },
                ..SPINE
            },
        ],
        // Wider sweeps: the spec allows a cache up to 11 bits (the heuristic self-caps at 10), and
        // coarser predictor blocks pay off on smooth content.
        5 => vec![
            Vp8lPlan {
                cache: CacheBits::Off,
                ..SPINE
            },
            Vp8lPlan {
                structure: FULL_SPATIAL,
                cache: CacheBits::Off,
                ..SPINE
            },
            Vp8lPlan {
                cache: CacheBits::AutoDelta(2),
                ..SPINE
            },
            with_structure(Structure::Spatial {
                subtract_green: true,
                predictor: Some(5),
                color: Some(DEFAULT_TRANSFORM_BITS),
            }),
            with_structure(Structure::Spatial {
                subtract_green: true,
                predictor: Some(5),
                color: None,
            }),
            Vp8lPlan {
                grouping: Grouping::Signature { prefix_bits: 3 },
                ..SPINE
            },
            Vp8lPlan {
                structure: FULL_SPATIAL,
                grouping: Grouping::Signature { prefix_bits: 5 },
                ..SPINE
            },
            Vp8lPlan {
                structure: Structure::Palette {
                    order: PaletteOrder::Ascending,
                },
                grouping: Grouping::Single,
                ..SPINE
            },
        ],
        // The exhaustive rung: extreme block sizes, the deepest parse, and the remaining
        // structure/grouping combinations.
        6 => vec![
            with_structure(Structure::Spatial {
                subtract_green: true,
                predictor: Some(2),
                color: Some(DEFAULT_TRANSFORM_BITS),
            }),
            with_structure(Structure::Spatial {
                subtract_green: true,
                predictor: Some(6),
                color: None,
            }),
            with_structure(Structure::Spatial {
                subtract_green: false,
                predictor: Some(3),
                color: None,
            }),
            with_structure(Structure::Spatial {
                subtract_green: true,
                predictor: None,
                color: None,
            }),
            Vp8lPlan {
                grouping: Grouping::Signature { prefix_bits: 2 },
                ..SPINE
            },
            Vp8lPlan {
                structure: FULL_SPATIAL,
                lz77: Lz77Params {
                    max_chain: 128,
                    lazy: true,
                },
                ..SPINE
            },
            Vp8lPlan {
                lz77: Lz77Params {
                    max_chain: 128,
                    lazy: true,
                },
                cache: CacheBits::AutoDelta(-1),
                ..SPINE
            },
            Vp8lPlan {
                structure: Structure::Spatial {
                    subtract_green: false,
                    predictor: Some(DEFAULT_TRANSFORM_BITS),
                    color: None,
                },
                grouping: Grouping::Single,
                cache: CacheBits::Off,
                ..SPINE
            },
            Vp8lPlan {
                structure: Structure::Palette {
                    order: PaletteOrder::Ascending,
                },
                lz77: Lz77Params {
                    max_chain: 128,
                    lazy: true,
                },
                ..SPINE
            },
        ],
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn effort(level: u8) -> Effort {
        Effort::from_level(level).expect("in range")
    }

    /// The qualities `cwebp -z` uses, plus the endpoints.
    const QUALITIES: [u8; 8] = [0, 20, 25, 30, 50, 75, 90, 100];

    #[test]
    fn each_rung_extends_the_one_below_it() {
        // The monotonicity guarantee rests entirely on this: at every quality, a rung's candidate
        // list must be a prefix-extension of the rung below, never a replacement. If someone
        // converts `added_at` into a "choose a different set per level" table, this fails.
        for quality in QUALITIES {
            for level in 1..=6u8 {
                let lower = enumerate(effort(level - 1), quality);
                let upper = enumerate(effort(level), quality);
                // Rung 3 races cache sizes only, which a quality under 30 does not reach, so below
                // that it adds nothing; from the default up every rung must add something.
                assert!(
                    upper.len() > lower.len() || quality < DEFAULT_QUALITY,
                    "rung {level} added no candidates at quality {quality}"
                );
                assert_eq!(
                    &upper[..lower.len()],
                    &lower[..],
                    "rung {level} is not an extension of rung {} at quality {quality}",
                    level - 1
                );
            }
        }
    }

    /// Whether `sub` is `sup` with zero or more plans removed, order kept.
    fn is_subsequence(sub: &[Vp8lPlan], sup: &[Vp8lPlan]) -> bool {
        let mut rest = sup.iter();
        sub.iter().all(|plan| rest.any(|p| p == plan))
    }

    #[test]
    fn each_quality_extends_the_one_below_it() {
        // The second axis of the same guarantee: raising quality only adds plans, never removes
        // or reorders one, so the shortest encoding can only get shorter.
        for level in 0..=6u8 {
            for quality in 1..=100u8 {
                let lower = enumerate(effort(level), quality - 1);
                let upper = enumerate(effort(level), quality);
                assert!(
                    is_subsequence(&lower, &upper),
                    "rung {level}: quality {quality} is not an extension of {}",
                    quality - 1
                );
            }
        }
    }

    #[test]
    fn quality_above_100_behaves_as_100() {
        for level in 0..=6u8 {
            assert_eq!(enumerate(effort(level), 101), enumerate(effort(level), 100));
            assert_eq!(
                enumerate(effort(level), u8::MAX),
                enumerate(effort(level), 100)
            );
        }
    }

    #[test]
    fn the_default_quality_enumerates_each_rungs_historical_list() {
        // At quality 75 every rung offers exactly the candidates it raced before quality reached
        // the search (issue #1240), so the default bytes cannot move: none of the plans quality
        // adds is reachable, and none of the historical plans is filtered out.
        for level in 0..=6u8 {
            let mut historical = vec![SPINE];
            for rung in 1..=level {
                historical.extend(added_at(rung));
            }
            assert_eq!(
                enumerate(effort(level), DEFAULT_QUALITY),
                historical,
                "rung {level}"
            );
            assert_eq!(
                enumerate(effort(level), 100).len(),
                historical.len() + (0..=level).map(|r| refined_at(r).len()).sum::<usize>(),
                "rung {level}: quality 100 must reach every refinement"
            );
        }
    }

    #[test]
    fn every_lossless_preset_searches_more_than_the_one_below_it() {
        // `cwebp -z 0..=9` as (effort, quality) pairs — the pins the oracle test checks against
        // libwebp. Each level must race strictly more candidates than the last, or it would be a
        // level in name only; the counts also pin every `min_quality` threshold a preset crosses.
        let counts: Vec<usize> = (0..=9u8)
            .map(|level| {
                let p = crate::LosslessPreset::from_level(level).expect("0..=9");
                enumerate(p.effort(), p.quality()).len()
            })
            .collect();
        assert_eq!(counts, [1, 4, 8, 9, 13, 17, 19, 25, 34, 50]);
    }

    #[test]
    fn min_quality_rises_with_chain_depth() {
        let at = |lazy, max_chain| {
            min_quality(&Vp8lPlan {
                lz77: Lz77Params { max_chain, lazy },
                ..SPINE
            })
        };
        assert_eq!(
            min_quality(&SPINE),
            0,
            "the spine is offered at every quality"
        );
        assert_eq!(at(false, 32), 0);
        assert_eq!(at(false, 33), DEFAULT_QUALITY);
        assert_eq!(at(true, 1), 25, "a lazy parse is a refinement at any depth");
        assert_eq!(at(true, 16), 25);
        assert_eq!(at(true, 17), DEFAULT_QUALITY);
        assert_eq!(at(true, 128), DEFAULT_QUALITY);
        assert_eq!(at(true, 129), 90);
        assert_eq!(at(true, 192), 90);
        assert_eq!(at(true, 193), 100);
    }

    #[test]
    fn min_quality_rises_with_cache_distance() {
        let at = |cache| min_quality(&Vp8lPlan { cache, ..SPINE });
        assert_eq!(at(CacheBits::Auto), 0);
        assert_eq!(at(CacheBits::Off), 30);
        for delta in [-2, -1, 1, 2] {
            assert_eq!(at(CacheBits::AutoDelta(delta)), 50, "delta {delta}");
        }
        for delta in [-3, 3, i8::MIN] {
            assert_eq!(at(CacheBits::AutoDelta(delta)), 90, "delta {delta}");
        }
    }

    #[test]
    fn min_quality_is_the_higher_of_the_two_floors() {
        let plan = |cache| with_lazy_chain(Vp8lPlan { cache, ..SPINE }, 256);
        assert_eq!(min_quality(&plan(CacheBits::Auto)), 100);
        assert_eq!(min_quality(&plan(CacheBits::Off)), 100);
        assert_eq!(
            min_quality(&Vp8lPlan {
                cache: CacheBits::AutoDelta(3),
                ..with_lazy_chain(SPINE, 16)
            }),
            90
        );
    }

    #[test]
    fn every_rung_stays_within_its_candidate_ceiling() {
        // The ceiling is the encode-cost contract; enumerating past it would make a rung
        // arbitrarily slow without anyone noticing.
        for level in 0..=6u8 {
            let plans = enumerate(effort(level), 100);
            assert!(
                !plans.is_empty(),
                "rung {level} must offer at least one plan"
            );
            assert!(
                plans.len() <= MAX_PLANS[level as usize],
                "rung {level} enumerated {} plans, ceiling {}",
                plans.len(),
                MAX_PLANS[level as usize]
            );
        }
    }

    #[test]
    fn the_spine_is_every_rungs_first_candidate() {
        // Ties resolve to the earliest plan, so the spine being first is what makes an unhelpful
        // higher rung reproduce the lower rung's bytes exactly rather than merely its size.
        for level in 0..=6u8 {
            let plans = enumerate(effort(level), 100);
            assert_eq!(plans[0], SPINE, "rung {level} does not lead with the spine");
        }
    }

    #[test]
    fn no_rung_enumerates_the_same_plan_twice() {
        // A duplicate is pure wasted encode time — it can never win, because ties resolve to the
        // earlier copy.
        let plans = enumerate(Effort::Slowest, 100);
        for (i, plan) in plans.iter().enumerate() {
            assert!(
                !plans[..i].contains(plan),
                "plan {plan:?} is enumerated more than once"
            );
        }
    }

    #[test]
    fn transform_block_sizes_stay_within_the_spec_field() {
        // The predictor/colour block-size exponent is written as `bits - 2` in a 3-bit field, so
        // only 2..=9 is representable; anything else would corrupt the stream.
        for plan in enumerate(Effort::Slowest, 100) {
            if let Structure::Spatial {
                predictor, color, ..
            } = plan.structure
            {
                for bits in [predictor, color].into_iter().flatten() {
                    assert!(
                        (2..=9).contains(&bits),
                        "block-size exponent {bits} is outside the 3-bit field"
                    );
                }
            }
        }
    }

    #[test]
    fn entropy_image_block_sizes_stay_within_the_spec_field() {
        // Same 3-bit `bits - 2` field for the meta-prefix image.
        for plan in enumerate(Effort::Slowest, 100) {
            if let Grouping::Signature { prefix_bits } = plan.grouping {
                assert!(
                    (2..=9).contains(&prefix_bits),
                    "prefix-bits {prefix_bits} is outside the 3-bit field"
                );
            }
        }
    }
}
