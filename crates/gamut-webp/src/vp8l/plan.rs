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

use crate::config::Effort;

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

/// Hard ceiling on the number of candidates each rung may enumerate, so encode cost cannot grow
/// silently as the ladder fills in.
pub(crate) const MAX_PLANS: [usize; 7] = [1, 4, 8, 13, 19, 27, 36];

/// The full spatial chain at the default block size — the shape `Structure::Auto` falls back to.
const FULL_SPATIAL: Structure = Structure::Spatial {
    subtract_green: true,
    predictor: Some(DEFAULT_TRANSFORM_BITS),
    color: Some(DEFAULT_TRANSFORM_BITS),
};

/// The candidate plans for `effort`, in evaluation order.
///
/// Append-only by construction: each rung extends the previous rung's list, which is what makes
/// output size non-increasing in effort (see the module docs).
#[must_use]
pub(crate) fn enumerate(effort: Effort) -> Vec<Vp8lPlan> {
    let mut plans = vec![SPINE];
    for level in 1..=effort.level() {
        plans.extend(added_at(level));
    }
    debug_assert!(
        plans.len() <= MAX_PLANS[effort.level() as usize],
        "effort {} enumerated {} plans, over its ceiling",
        effort.level(),
        plans.len()
    );
    plans
}

/// A plan that differs from the spine only in its structure.
const fn with_structure(structure: Structure) -> Vp8lPlan {
    Vp8lPlan { structure, ..SPINE }
}

/// The plans rung `level` adds on top of rung `level - 1`.
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

    #[test]
    fn each_rung_extends_the_one_below_it() {
        // The monotonicity guarantee rests entirely on this: a rung's candidate list must be a
        // prefix-extension of the rung below, never a replacement. If someone converts `added_at`
        // into a "choose a different set per level" table, this fails.
        for level in 1..=6u8 {
            let lower = enumerate(Effort::from_level(level - 1).expect("in range"));
            let upper = enumerate(Effort::from_level(level).expect("in range"));
            assert!(
                upper.len() > lower.len(),
                "rung {level} added no candidates, so it cannot differ from rung {}",
                level - 1
            );
            assert_eq!(
                &upper[..lower.len()],
                &lower[..],
                "rung {level} is not an extension of rung {}",
                level - 1
            );
        }
    }

    #[test]
    fn every_rung_stays_within_its_candidate_ceiling() {
        // The ceiling is the encode-cost contract; enumerating past it would make a rung
        // arbitrarily slow without anyone noticing.
        for level in 0..=6u8 {
            let plans = enumerate(Effort::from_level(level).expect("in range"));
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
            let plans = enumerate(Effort::from_level(level).expect("in range"));
            assert_eq!(plans[0], SPINE, "rung {level} does not lead with the spine");
        }
    }

    #[test]
    fn no_rung_enumerates_the_same_plan_twice() {
        // A duplicate is pure wasted encode time — it can never win, because ties resolve to the
        // earlier copy.
        let plans = enumerate(Effort::Slowest);
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
        for plan in enumerate(Effort::Slowest) {
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
        for plan in enumerate(Effort::Slowest) {
            if let Grouping::Signature { prefix_bits } = plan.grouping {
                assert!(
                    (2..=9).contains(&prefix_bits),
                    "prefix-bits {prefix_bits} is outside the 3-bit field"
                );
            }
        }
    }
}
