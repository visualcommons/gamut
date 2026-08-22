//! The crate's error type.

/// An error from building or evaluating a colour transform.
///
/// Every variant is a violated invariant, detected when a [`Pipeline`](crate::Pipeline) is
/// constructed or a [`Transform`](crate::Transform) is evaluated (mismatched channel counts,
/// missized sample buffers, inconsistent stage shapes), or when [`link`](crate::link) builds a
/// pipeline from a parsed profile (missing/unusable tags, non-invertible data, profile forms a
/// later phase covers). Exposing the crate's own type — rather than the shared
/// `gamut_core::Error` — keeps the failing carrier identifiable when the CMM is embedded in a
/// wider colour pipeline.
///
/// Marked `#[non_exhaustive]`: the CMM phases (curves, CLUTs, profile linking, intents) add
/// failure categories additively without a breaking change.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CmmError {
    /// Two adjacent pipeline stages disagree on the channel count flowing between them: the
    /// stage at `index` expects `expected` input channels but the stage before it produces
    /// `found`. Also raised by [`Pipeline::compose`](crate::Pipeline::compose) when the second
    /// pipeline's input does not match the first pipeline's output (there `index` is the
    /// position the second pipeline's first stage would take).
    #[error(
        "cmm: stage {index} expects {expected} input channels, previous stage produces {found}"
    )]
    StageChannelMismatch {
        /// Zero-based index of the stage whose input is mismatched.
        index: usize,
        /// The input channel count that stage expects.
        expected: u8,
        /// The output channel count actually produced upstream.
        found: u8,
    },

    /// A pipeline's declared end does not match the stage chain: the `end` ("input" or
    /// "output") was declared as `declared` channels, but the terminal stage on that end
    /// carries `found`. For an empty (identity) pipeline this reports the two declared ends
    /// disagreeing with each other.
    #[error("cmm: pipeline {end} declares {declared} channels, found {found}")]
    PipelineEndsMismatch {
        /// Which pipeline end is mismatched: `"input"` or `"output"`.
        end: &'static str,
        /// The channel count the pipeline declared for that end.
        declared: u8,
        /// The channel count the stage chain actually carries there.
        found: u8,
    },

    /// A declared channel count is outside `1..=`[`MAX_CHANNELS`](crate::MAX_CHANNELS) (ICC
    /// caps multi-dimensional
    /// transform inputs at 16 channels; zero channels is structurally meaningless).
    #[error("cmm: channel count {0} outside 1..=16")]
    TooManyChannels(u8),

    /// A sample buffer's length does not fit the transform's channel count: `found` samples is
    /// not a whole number of `channels`-sample pixels, or disagrees with the pixel count
    /// implied by the paired buffer.
    #[error("cmm: buffer length {found} is not a multiple of {channels} channels")]
    BufferLength {
        /// The per-pixel channel count of the offending buffer's end of the transform.
        channels: u8,
        /// The offending buffer's length in samples.
        found: usize,
    },

    /// A hand-built parametric curve carries a function type outside the five ICC.1:2022 §10.18
    /// defines (0–4). Unreachable from parsed profiles — `gamut-icc`'s parser rejects such
    /// types — but `gamut_icc::ParametricCurve::eval` silently treats them as the identity, so
    /// [`ToneCurve::new`](crate::ToneCurve::new) refuses them with this typed error instead.
    #[error("cmm: parametric curve function type {0} is not supported")]
    UnsupportedParametricType(u16),

    /// A tone curve is neither non-decreasing nor non-increasing (or is constant), so
    /// [`ToneCurve::inverse`](crate::ToneCurve::inverse) has no functional inverse to build.
    #[error("cmm: tone curve is not monotonic; no inverse exists")]
    NonMonotonicCurve,

    /// A CLUT's declared geometry is inconsistent, so [`ClutTable`](crate::ClutTable) cannot
    /// index it safely: no input dimensions, an axis with zero grid nodes, a sample count that
    /// disagrees with `∏ grid_points × output_channels`, or a tetrahedral interpolation
    /// request for fewer than 3 input channels. Unreachable from `gamut-icc`-parsed CLUTs
    /// (the parser upholds the invariants), reachable from hand-built values.
    #[error("cmm: CLUT geometry inconsistent ({0})")]
    ClutGeometry(&'static str),

    /// A profile lacks a tag the requested link requires: a matrix/TRC shaper build needs all
    /// three colorants (`rXYZ`/`gXYZ`/`bXYZ`) and TRCs (`rTRC`/`gTRC`/`bTRC`) for RGB, or
    /// `kTRC` for gray; a profile whose device space has no shaper form (CMYK and friends)
    /// needs the intent's LUT tag or the perceptual fallback (`A2B0`/`B2A0`) — the payload is
    /// then the requested intent's primary LUT tag. The payload is the missing tag's
    /// signature.
    #[error("cmm: profile is missing required tag {0}")]
    MissingTag(gamut_icc::Signature),

    /// A required tag is present but holds an element type the link cannot use: a colorant tag
    /// without an `XYZType` value (or with an empty one), a TRC tag holding something other
    /// than a `curveType`/`parametricCurveType`, or an `A2Bx`/`B2Ax` tag holding anything but
    /// the four LUT element types (`lut8`/`lut16`/`mAB `/`mBA ` — an `mpet` payload, which
    /// `gamut-icc` preserves as raw bytes, lands here) or carrying a zero-entry `lut16`
    /// table. The payload is the offending tag's signature.
    #[error("cmm: tag {0} holds an unusable element type")]
    BadTagType(gamut_icc::Signature),

    /// The profile's colorant matrix has no finite inverse, so no PCS→device shaper transform
    /// exists. Raised when the determinant is zero or non-finite
    /// (`gamut_color::linalg::mat_inv_3x3` returning `None` — the crate's conditioning
    /// threshold: exact singularity, no epsilon), or when an inverse entry overflows to
    /// non-finite.
    #[error("cmm: colorant matrix is singular; no PCS-to-device transform exists")]
    SingularMatrix,

    /// The profile is outside what [`link`](crate::link) currently builds; the payload says
    /// which boundary was hit (currently: a LUT-less profile whose header PCS is neither XYZ
    /// nor Lab cannot take the matrix/TRC shaper fallback).
    #[error("cmm: unsupported profile ({0})")]
    UnsupportedProfile(&'static str),

    /// A stage's internal shape is inconsistent — a
    /// [`Stage::MatrixN`](crate::Stage::MatrixN) whose coefficient or offset vector length
    /// contradicts its declared `rows`/`cols`. Detected by
    /// [`Pipeline::new`](crate::Pipeline::new); unreachable from pipelines this crate builds,
    /// reachable from hand-built stages.
    #[error("cmm: malformed stage ({0})")]
    BadStage(&'static str),
}

/// A [`Result`](core::result::Result) whose error is [`CmmError`].
pub type Result<T> = core::result::Result<T, CmmError>;
