//! The crate's error type.

/// An error from building or evaluating a colour transform.
///
/// Every variant is a violated invariant, detected when a [`Pipeline`](crate::Pipeline) is
/// constructed or a [`Transform`](crate::Transform) is evaluated (mismatched channel counts,
/// missized sample buffers, inconsistent stage shapes), or when [`link`](crate::link) builds a
/// pipeline from a parsed profile (missing/unusable tags, non-invertible data, profile forms a
/// later phase covers). Exposing the crate's own type — rather than the shared
/// `gamut_core::Error` — keeps the failing carrier identifiable when the CMM is embedded in a
/// wider colour pipeline, and `From<CmmError> for gamut_core::Error` funnels it into that shared
/// type where one is wanted.
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
    /// [`IccCurve::new`](crate::IccCurve::new) refuses them with this typed error instead.
    #[error("cmm: parametric curve function type {0} is not supported")]
    UnsupportedParametricType(u16),

    /// A tone curve is neither non-decreasing nor non-increasing (or is constant), so
    /// [`IccCurve::inverse`](crate::IccCurve::inverse) has no functional inverse to build.
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

    /// A transform chain's profiles do not connect: a profile's entry colour space differs
    /// from the space the chain carries at that hop (lcms2's `ColorSpaceIsCompatible` check,
    /// `cmscnvrt.c:492-568`), or the chain is shorter than two profiles. The payload says
    /// which continuity rule was violated.
    #[error("cmm: profile chain mismatch ({0})")]
    ChainMismatch(&'static str),

    /// A pixel-buffer format the CMM cannot transform: [`gamut_core::PixelFormat::Bilevel`]
    /// and [`gamut_core::PixelFormat::Indexed8`] carry non-continuous colour (a threshold
    /// bit, palette indices), which no colour transform can meaningfully rescale. The payload
    /// is the offending format.
    #[error("cmm: pixel format {0:?} carries no continuous colour channels")]
    UnsupportedPixelFormat(gamut_core::PixelFormat),

    /// A pixel-buffer call's geometry is inconsistent: the format's colour channel count does
    /// not match the transform's, the source and destination disagree on the pixel count, or
    /// a planar call's plane count/lengths contradict the format. The payload says which
    /// check failed; plain length-divisibility violations use
    /// [`BufferLength`](CmmError::BufferLength) instead.
    #[error("cmm: image geometry mismatch ({0})")]
    ImageGeometry(&'static str),

    /// An ICC profile failed to parse or serialize in `gamut-icc`. Never raised by this
    /// crate's own constructors, which take profiles already parsed; it lets a caller that
    /// parses a profile and then builds a transform from it carry both failures in one type
    /// (`IccProfile::parse(bytes)?` converts through `From`). The [`IccError`] is the
    /// [`source`](std::error::Error::source).
    ///
    /// [`IccError`]: gamut_icc::IccError
    #[error("cmm: {0}")]
    Icc(#[from] gamut_icc::IccError),
}

/// A [`Result`](core::result::Result) whose error is [`CmmError`].
pub type Result<T> = core::result::Result<T, CmmError>;

impl From<CmmError> for gamut_core::Error {
    /// Funnels a [`CmmError`] into the workspace's unified [`gamut_core::Error`].
    ///
    /// [`CmmError::Icc`] is handed to `gamut-icc`'s own conversion, so its kind, origin and
    /// byte offset survive. The `Unsupported*` variants — a parametric function type, a profile
    /// form, or a pixel format outside what this crate builds or transforms — are
    /// [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported); every other variant is a
    /// violated invariant of the profile or of the caller's buffers and stages, so
    /// [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput). Each carries a static
    /// message naming its variant and the full CMM diagnostic as
    /// [`detail`](gamut_core::Error::detail).
    fn from(err: CmmError) -> Self {
        let origin = env!("CARGO_PKG_NAME");
        let core = match err {
            CmmError::Icc(icc) => return icc.into(),
            CmmError::UnsupportedParametricType(_) => gamut_core::Error::unsupported(
                origin,
                "cmm: parametric curve function type is not supported",
            ),
            CmmError::UnsupportedProfile(_) => {
                gamut_core::Error::unsupported(origin, "cmm: unsupported profile")
            }
            CmmError::UnsupportedPixelFormat(_) => gamut_core::Error::unsupported(
                origin,
                "cmm: pixel format carries no continuous colour channels",
            ),
            CmmError::StageChannelMismatch { .. } => gamut_core::Error::invalid_input(
                origin,
                "cmm: adjacent pipeline stages disagree on channel count",
            ),
            CmmError::PipelineEndsMismatch { .. } => gamut_core::Error::invalid_input(
                origin,
                "cmm: pipeline end does not match its stage chain",
            ),
            CmmError::TooManyChannels(_) => {
                gamut_core::Error::invalid_input(origin, "cmm: channel count outside 1..=16")
            }
            CmmError::BufferLength { .. } => gamut_core::Error::invalid_input(
                origin,
                "cmm: buffer length does not fit the channel count",
            ),
            CmmError::NonMonotonicCurve => gamut_core::Error::invalid_input(
                origin,
                "cmm: tone curve is not monotonic; no inverse exists",
            ),
            CmmError::ClutGeometry(_) => {
                gamut_core::Error::invalid_input(origin, "cmm: CLUT geometry inconsistent")
            }
            CmmError::MissingTag(_) => {
                gamut_core::Error::invalid_input(origin, "cmm: profile is missing a required tag")
            }
            CmmError::BadTagType(_) => {
                gamut_core::Error::invalid_input(origin, "cmm: tag holds an unusable element type")
            }
            CmmError::SingularMatrix => {
                gamut_core::Error::invalid_input(origin, "cmm: colorant matrix is singular")
            }
            CmmError::BadStage(_) => {
                gamut_core::Error::invalid_input(origin, "cmm: malformed stage")
            }
            CmmError::ChainMismatch(_) => {
                gamut_core::Error::invalid_input(origin, "cmm: profile chain mismatch")
            }
            CmmError::ImageGeometry(_) => {
                gamut_core::Error::invalid_input(origin, "cmm: image geometry mismatch")
            }
        };
        core.with_detail(err.to_string())
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error as _;

    use gamut_core::ErrorKind;
    use gamut_icc::{IccError, Signature};

    use super::*;

    /// Converts `err`, asserting the kind and static message, and that the detail is the
    /// CMM diagnostic.
    fn assert_core(err: CmmError, kind: ErrorKind, message: &str) {
        let diagnostic = err.to_string();
        let core = gamut_core::Error::from(err);
        assert_eq!(core.kind(), kind, "{diagnostic}");
        assert_eq!(core.static_message(), Some(message), "{diagnostic}");
        assert_eq!(core.origin(), Some("gamut-cmm"), "{diagnostic}");
        assert_eq!(core.detail(), Some(diagnostic.as_str()));
    }

    #[test]
    fn unsupported_variants_convert_to_unsupported() {
        use ErrorKind::Unsupported;
        assert_core(
            CmmError::UnsupportedParametricType(7),
            Unsupported,
            "cmm: parametric curve function type is not supported",
        );
        assert_core(
            CmmError::UnsupportedProfile("x"),
            Unsupported,
            "cmm: unsupported profile",
        );
        assert_core(
            CmmError::UnsupportedPixelFormat(gamut_core::PixelFormat::Bilevel),
            Unsupported,
            "cmm: pixel format carries no continuous colour channels",
        );
    }

    #[test]
    fn invariant_variants_convert_to_invalid_input() {
        use ErrorKind::InvalidInput;
        let tag = Signature(*b"rTRC");
        let cases = [
            (
                CmmError::StageChannelMismatch {
                    index: 1,
                    expected: 3,
                    found: 4,
                },
                "cmm: adjacent pipeline stages disagree on channel count",
            ),
            (
                CmmError::PipelineEndsMismatch {
                    end: "input",
                    declared: 3,
                    found: 4,
                },
                "cmm: pipeline end does not match its stage chain",
            ),
            (
                CmmError::TooManyChannels(17),
                "cmm: channel count outside 1..=16",
            ),
            (
                CmmError::BufferLength {
                    channels: 3,
                    found: 4,
                },
                "cmm: buffer length does not fit the channel count",
            ),
            (
                CmmError::NonMonotonicCurve,
                "cmm: tone curve is not monotonic; no inverse exists",
            ),
            (
                CmmError::ClutGeometry("x"),
                "cmm: CLUT geometry inconsistent",
            ),
            (
                CmmError::MissingTag(tag),
                "cmm: profile is missing a required tag",
            ),
            (
                CmmError::BadTagType(tag),
                "cmm: tag holds an unusable element type",
            ),
            (CmmError::SingularMatrix, "cmm: colorant matrix is singular"),
            (CmmError::BadStage("x"), "cmm: malformed stage"),
            (CmmError::ChainMismatch("x"), "cmm: profile chain mismatch"),
            (CmmError::ImageGeometry("x"), "cmm: image geometry mismatch"),
        ];
        for (err, message) in cases {
            assert_core(err, InvalidInput, message);
        }
    }

    #[test]
    fn icc_errors_wrap_and_convert_through_gamut_icc() {
        let icc = || IccError::Malformed("icc: duplicate tag signature");
        let err = CmmError::from(icc());
        assert_eq!(err.to_string(), "cmm: icc: duplicate tag signature");
        assert_eq!(
            err.source().map(ToString::to_string),
            Some(icc().to_string())
        );
        // Converted exactly as `gamut-icc` converts it: its origin, not this crate's.
        let core = gamut_core::Error::from(err);
        let want = gamut_core::Error::from(icc());
        assert_eq!(core.kind(), want.kind());
        assert_eq!(core.origin(), Some("gamut-icc"));
        assert_eq!(core.static_message(), want.static_message());
        assert_eq!(core.detail(), want.detail());
    }
}
