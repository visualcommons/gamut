//! The VP8 encoder's effort ladder (issue #32).
//!
//! The coding tools each [`Effort`](crate::Effort) level may spend time on, as **one table** rather
//! than `if effort >= n` tests scattered through the encoder. Keeping it in one place means each
//! tool can be switched on in isolation by a test — which is what makes the probability optimizer's
//! "same pixels, fewer bits" property directly assertable — and gives mutation testing a single
//! high-value target instead of a dozen comparisons.
//!
//! Nothing here affects conformance: every rung emits a valid key frame that libwebp decodes
//! identically to gamut's own decoder. The rungs differ only in how hard the encoder looks.

/// How much work the encoder spends choosing `B_PRED` (per-4×4) luma prediction.
///
/// It is the single most expensive thing the encoder does — ten submodes searched for each of
/// sixteen subblocks, on **every** macroblock, whether or not it ends up being used — so it is the
/// natural thing for the fast rungs to give up first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Bpred {
    /// Never considered; every macroblock uses a whole-block mode.
    Off,
    /// Considered only when the macroblock's whole-block prediction is poor enough to suggest it
    /// might pay, avoiding the search on flat content.
    Gated,
    /// Always considered.
    Always,
}

/// How the forward quantizer rounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QuantBias {
    /// Round to nearest.
    Nearest,
    /// Round with a dead zone: coefficients near the threshold collapse to zero, which costs little
    /// distortion and saves the token entirely.
    DeadZone,
}

/// How much work the `ALPH` alpha chunk's encoder spends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AlphaEffort {
    /// Filter chosen from the residual magnitudes, then whichever of raw/compressed is smaller.
    Balanced,
    /// Also search the pre-filter on the lossless-compressed path, which is otherwise forced to
    /// "none" — several VP8L encodes, which is why it sits at the top of the ladder.
    Exhaustive,
}

/// The coding tools enabled at one effort level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EffortTools {
    /// How hard to look for a `B_PRED` macroblock.
    pub bpred: Bpred,
    /// How the forward quantizer rounds.
    pub quant_bias: QuantBias,
    /// Whether to derive the frame's coefficient probabilities from what it actually coded.
    pub two_pass_probs: bool,
    /// Whether to measure the skip probability rather than guess it from the quantizer.
    pub measured_skip_prob: bool,
    /// How much work the alpha chunk's encoder spends.
    pub alpha: AlphaEffort,
}

/// The ladder, indexed by effort level `0..=6`.
///
/// Level 2 is deliberately pinned to the historical toolset, which is what let every step of the
/// restructure that introduced this table be checked against unchanged output bytes.
pub(crate) const EFFORT_TABLE: [EffortTools; 7] = [
    // 0 — the fastest rung: no 4x4 search at all.
    EffortTools {
        bpred: Bpred::Off,
        quant_bias: QuantBias::Nearest,
        two_pass_probs: false,
        measured_skip_prob: false,
        alpha: AlphaEffort::Balanced,
    },
    // 1 — 4x4 search only where the whole-block prediction is poor.
    EffortTools {
        bpred: Bpred::Gated,
        quant_bias: QuantBias::Nearest,
        two_pass_probs: false,
        measured_skip_prob: false,
        alpha: AlphaEffort::Balanced,
    },
    // 2 — the historical toolset.
    EffortTools {
        bpred: Bpred::Always,
        quant_bias: QuantBias::Nearest,
        two_pass_probs: false,
        measured_skip_prob: false,
        alpha: AlphaEffort::Balanced,
    },
    // 3 — entropy coding starts describing the frame instead of guessing at it. Free in
    // distortion: the decoded pixels are identical, only the bit cost changes.
    EffortTools {
        bpred: Bpred::Always,
        quant_bias: QuantBias::Nearest,
        two_pass_probs: true,
        measured_skip_prob: true,
        alpha: AlphaEffort::Balanced,
    },
    // 4 — the default: adds the dead-zone quantizer.
    EffortTools {
        bpred: Bpred::Always,
        quant_bias: QuantBias::DeadZone,
        two_pass_probs: true,
        measured_skip_prob: true,
        alpha: AlphaEffort::Balanced,
    },
    // 5 — spend real time on the alpha plane.
    EffortTools {
        bpred: Bpred::Always,
        quant_bias: QuantBias::DeadZone,
        two_pass_probs: true,
        measured_skip_prob: true,
        alpha: AlphaEffort::Exhaustive,
    },
    // 6 — the slowest rung.
    EffortTools {
        bpred: Bpred::Always,
        quant_bias: QuantBias::DeadZone,
        two_pass_probs: true,
        measured_skip_prob: true,
        alpha: AlphaEffort::Exhaustive,
    },
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Effort;

    #[test]
    fn the_table_covers_every_effort_level() {
        // Indexing the table by `Effort::level()` is only sound if it has a row per rung.
        for level in 0..=6u8 {
            let effort = Effort::from_level(level).expect("in range");
            let _ = EFFORT_TABLE[effort.level() as usize];
        }
        assert_eq!(EFFORT_TABLE.len(), 7);
    }

    #[test]
    fn tools_are_only_ever_added_as_effort_rises() {
        // The ladder's promise is that a higher rung never does *less* work, so no tool may switch
        // back off. Encoded as an ordering on each field rather than as prose.
        let rank_bpred = |b| match b {
            Bpred::Off => 0,
            Bpred::Gated => 1,
            Bpred::Always => 2,
        };
        let rank_alpha = |a| match a {
            AlphaEffort::Balanced => 0,
            AlphaEffort::Exhaustive => 1,
        };
        for level in 1..EFFORT_TABLE.len() {
            let (lower, upper) = (EFFORT_TABLE[level - 1], EFFORT_TABLE[level]);
            assert!(
                rank_bpred(upper.bpred) >= rank_bpred(lower.bpred),
                "rung {level} searches less than rung {}",
                level - 1
            );
            assert!(
                rank_alpha(upper.alpha) >= rank_alpha(lower.alpha),
                "rung {level} spends less on alpha than rung {}",
                level - 1
            );
            assert!(upper.two_pass_probs >= lower.two_pass_probs);
            assert!(upper.measured_skip_prob >= lower.measured_skip_prob);
            assert!(
                (upper.quant_bias == QuantBias::DeadZone)
                    >= (lower.quant_bias == QuantBias::DeadZone)
            );
        }
    }

    #[test]
    fn level_two_is_the_historical_toolset() {
        // The anchor the byte-identity checks during the two-pass restructure relied on. If this
        // ever changes, `tests/default_bytes.rs`'s lossy digests must move with it.
        assert_eq!(
            EFFORT_TABLE[2],
            EffortTools {
                bpred: Bpred::Always,
                quant_bias: QuantBias::Nearest,
                two_pass_probs: false,
                measured_skip_prob: false,
                alpha: AlphaEffort::Balanced,
            }
        );
    }
}
