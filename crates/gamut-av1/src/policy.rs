//! The lossy encoder's tunable decisions, gathered into one plain value.
//!
//! [`EncodePolicy`] is the one place a lossy encode reads a decision the specification leaves to
//! the encoder and that tuning may want to vary: how a candidate's distortion is measured, which
//! quantizer step the rate–distortion multiplier `λ` is scaled from, whether blocks vary their
//! quantizer, the deblocking sharpness, the quantizer matrices and the screen-content tools. The
//! tile coder, the in-loop filter search and the frame header all read it from here rather than
//! fixing the decision where they use it, so a tuning experiment (#1213) adds a variant of a field
//! below and touches nothing else to try it.
//!
//! [`EncodePolicy::default`] is the policy every public entry point codes with, and it reproduces
//! the bytes the encoder emitted before the decisions were gathered here. The policy is
//! `pub(crate)` on purpose: experiments drive it from in-crate harnesses, so no public item, cargo
//! feature or `Av1EncodeConfig` knob exists for it (#1214). It is nevertheless shaped as plain
//! data — a `Copy` struct of fieldless `repr(u8)` enums with append-only discriminants — so it can
//! go public later without a redesign.
//!
//! **A decision the format makes conformant only with machinery this crate lacks offers no value
//! beyond its default.** `loop_filter_sharpness > 0` needs the §7.14.4 sharpness derivation
//! (`shift`, `limit = Clip3(1, 9 − sharpness, lvl >> shift)`), which `filter::deblock` does not
//! implement, and `using_qmatrix = 1` needs the §9.5 matrices and §7.12.3 step b in the
//! reconstruction. [`Sharpness`] and [`QuantMatrix`] therefore have one variant each, and the sites
//! that would need the machinery destructure that variant irrefutably: a variant added before its
//! machinery fails to compile exactly there, instead of signalling a header the reconstruction does
//! not follow.
//!
//! Lossless (`base_q_idx = 0`) reads none of this beyond the screen-content gate, which is off
//! there whatever the policy says.

use crate::tile::BlockVariation;

/// Every tunable decision of a lossy encode. See the [module docs](self).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub(crate) struct EncodePolicy {
    /// Whether blocks vary their quantizer and loop-filter level, and how.
    pub(crate) block_variation: BlockVariation,
    /// How a candidate's distortion against the source is measured.
    pub(crate) distortion: Distortion,
    /// Which quantizer step the rate–distortion multiplier `λ` is scaled from.
    pub(crate) lambda: Lambda,
    /// `loop_filter_sharpness` (§5.9.11).
    pub(crate) sharpness: Sharpness,
    /// `using_qmatrix` (§5.9.12).
    pub(crate) qmatrix: QuantMatrix,
    /// `allow_screen_content_tools` (§5.9.2), which gates palette.
    pub(crate) screen_content: ScreenContent,
}

impl EncodePolicy {
    /// The default policy with the position-only block variation the crate's oracle tests use to
    /// keep the segmentation, `delta_q` and `delta_lf` syntax under test.
    #[cfg(test)]
    pub(crate) const SYNTAX_EXERCISE: Self = Self {
        block_variation: BlockVariation::SyntaxExercise,
        distortion: Distortion::Sse,
        lambda: Lambda::FrameAcStep,
        sharpness: Sharpness::Zero,
        qmatrix: QuantMatrix::Off,
        screen_content: ScreenContent::Lossy,
    };
}

/// How the distortion of a reconstruction against the source is measured, for every
/// rate–distortion decision in the tiles and for the in-loop filter searches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(u8)]
pub(crate) enum Distortion {
    /// The unweighted sum of squared sample errors over the coded planes.
    #[default]
    Sse = 0,
}

impl Distortion {
    /// The distortion one sample contributes when its reconstruction is `d` away from the source.
    #[inline]
    pub(crate) fn sample(self, d: i64) -> u64 {
        match self {
            Self::Sse => (d * d) as u64,
        }
    }
}

/// Which quantizer step the rate–distortion multiplier `λ = q² / LAMBDA_DIV` is scaled from.
///
/// It is asked per block — it is given the step the block being decided is quantized with — but
/// the one variant answers frame-wide, so that a partition decision whose alternatives fall in
/// different segments weighs them on one scale.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(u8)]
pub(crate) enum Lambda {
    /// The frame's AC step (`base_q_idx`), whatever the block's own quantizer.
    #[default]
    FrameAcStep = 0,
}

impl Lambda {
    /// The step `λ` is scaled from, given the frame's AC step and the AC step of the block whose
    /// decision is being weighed.
    #[inline]
    pub(crate) fn step(self, frame_ac: u64, _block_ac: u64) -> u64 {
        match self {
            Self::FrameAcStep => frame_ac,
        }
    }
}

/// `loop_filter_sharpness` (§5.9.11). Only 0 is offered: `filter::deblock` derives its thresholds
/// without the §7.14.4 sharpness terms, so any other value would signal a filter the
/// reconstruction does not apply. Adding a variant is a compile error in `FrameEncoder::deblock`
/// until that derivation lands with it.
///
/// The discriminant is the coded value, which the frame header writes as is: a method returning
/// it would be a body of one literal, whose replacement by that literal is an equivalent mutant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(u8)]
pub(crate) enum Sharpness {
    /// `loop_filter_sharpness = 0`.
    #[default]
    Zero = 0,
}

/// `using_qmatrix` (§5.9.12). Only the flat (no-matrix) quantizer is offered: the reconstruction
/// has no §7.12.3 step b and the crate carries no §9.5 matrices. Adding a variant is a compile
/// error in the reconstruction until both land with it.
///
/// The discriminant is the coded flag, which the frame header writes as is, for the reason
/// [`Sharpness`] gives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(u8)]
pub(crate) enum QuantMatrix {
    /// `using_qmatrix = 0`.
    #[default]
    Off = 0,
}

/// When `allow_screen_content_tools` (§5.9.2) is set — and with it, when the tiles may code a
/// palette.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(u8)]
pub(crate) enum ScreenContent {
    /// On for every lossy frame, off for a lossless one; palette is then chosen by colour count.
    #[default]
    Lossy = 0,
}

impl ScreenContent {
    /// Whether a frame codes `allow_screen_content_tools = 1`, given whether it is lossy.
    pub(crate) fn allowed(self, lossy: bool) -> bool {
        match self {
            Self::Lossy => lossy,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_screen_content_tools_follow_the_lossy_path() {
        assert!(ScreenContent::Lossy.allowed(true));
        assert!(!ScreenContent::Lossy.allowed(false));
    }

    #[test]
    fn the_default_distortion_is_the_squared_error() {
        for (d, sq) in [(-300, 90_000), (-1, 1), (0, 0), (7, 49), (4095, 16_769_025)] {
            assert_eq!(Distortion::Sse.sample(d), sq, "{d}");
        }
    }

    #[test]
    fn the_default_lambda_is_frame_wide_whatever_the_block_step() {
        assert_eq!(Lambda::FrameAcStep.step(52, 104), 52);
        assert_eq!(Lambda::FrameAcStep.step(52, 26), 52);
    }
}
