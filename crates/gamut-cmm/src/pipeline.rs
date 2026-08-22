//! The keystone pipeline/stage model: a colour transform as a validated chain of stages.
//!
//! [`Pipeline::new`] is the crate's validity boundary — every channel count (the pipeline's
//! declared ends, each stage's input/output, and every adjacent seam) is checked exactly once at
//! construction, so evaluation carries no per-sample validation beyond buffer lengths and a
//! constructed [`Pipeline`] can always run.

use gamut_color::lab::{D50_XYZ, lab_to_xyz, xyz_to_lab};

use crate::clut::ClutTable;
use crate::curve::ToneCurve;
use crate::error::{CmmError, Result};
use crate::transform::Transform;

/// The largest channel count a stage or pipeline end may declare.
///
/// ICC.1:2022 caps multi-dimensional transform inputs at 16 channels (the CLUT encodings of
/// §10.12/§10.13; device spaces themselves stop at 15 colorants, `FCLR`), so 16 bounds every
/// channel count this CMM can meet. Fixing the bound lets [`Pipeline::eval`] ping-pong two
/// fixed-size stack buffers instead of allocating per pixel.
pub const MAX_CHANNELS: u8 = 16;

/// One evaluation step of a colour transform: maps [`input_channels`](Stage::input_channels)
/// samples to [`output_channels`](Stage::output_channels) samples of one pixel.
///
/// # Growth plan
///
/// The enum is `#[non_exhaustive]` and grows additively with the CMM phases: [`Curves`]
/// (#325, landed), [`Clut`](Stage::Clut) (#326, landed), [`MatrixN`](Stage::MatrixN) (#327,
/// landed), and [`XyzToLab`](Stage::XyzToLab)/[`LabToXyz`](Stage::LabToXyz) (#328, landed)
/// each arrive **together with their `eval` arm** — the crate-internal `eval` match is
/// deliberately exhaustive (no wildcard), so the compiler forces every future variant to
/// bring its evaluation in the same change.
///
/// [`Curves`]: Stage::Curves
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Stage {
    /// Passes `channels` samples through unchanged.
    Identity {
        /// The channel count carried through, in `1..=`[`MAX_CHANNELS`].
        channels: u8,
    },
    /// Clamps every one of `channels` samples to `[0.0, 1.0]`.
    ///
    /// Out-of-range semantics match the oracle (lcms2's `fclamp`): **negative values and NaN
    /// clamp to `0.0`**, values above `1.0` clamp to `1.0`. ICC.1 does not define CMM clamping
    /// behaviour, so the observable choice — including NaN → `0.0` rather than propagation —
    /// follows Little-CMS.
    Clamp {
        /// The channel count clamped, in `1..=`[`MAX_CHANNELS`].
        channels: u8,
    },
    /// Per-channel 1-D tone curves: applies `curves[i]` to sample `i` — n-in/n-out, with `n`
    /// the number of curves.
    ///
    /// The stage form of the "curve set" every ICC LUT transform carries (the input/output
    /// tables of `lut8`/`lut16`, the A/M/B curves of `lutAToB`/`lutBToA`, and the
    /// matrix-shaper TRCs). Each [`ToneCurve`] clamps its channel to `[0, 1]` on both sides
    /// (see [`ToneCurve::eval`]).
    Curves(Vec<ToneCurve>),
    /// A multi-dimensional colour lookup table: interpolates
    /// [`input_channels`](ClutTable::input_channels) samples through a validated grid to
    /// [`output_channels`](ClutTable::output_channels) samples.
    ///
    /// The stage form of the CLUT every ICC LUT transform carries (`lut8`/`lut16`,
    /// `lutAToB`/`lutBToA`, ICC.1:2022 §10.10–§10.13). Interpolation semantics — lcms2's
    /// tetrahedral/multilinear split, input clamping, and edge rules — are documented on
    /// [`ClutTable`].
    Clut(ClutTable),
    /// A 3-in/3-out affine matrix: `out = m · in + offset`.
    ///
    /// `m` is row-major (`out[r] = m[r][0]·in[0] + m[r][1]·in[1] + m[r][2]·in[2] + offset[r]`)
    /// — the `e1..e12` layout of the lutAToB/lutBToA matrix element (ICC.1:2022 §10.12.5).
    Matrix {
        /// The 3×3 matrix, row-major.
        m: [[f64; 3]; 3],
        /// The per-row offset added after the multiply (`e10..e12`).
        offset: [f64; 3],
    },
    /// A general `cols`-in/`rows`-out affine matrix: `out = m · in + offset`.
    ///
    /// The rectangular sibling of [`Matrix`](Stage::Matrix), for the channel-count-changing
    /// seams profile linking needs — a gray shaper's 1→3 white-scaling and 3→1
    /// channel-picking matrices (lcms2's `cmsStageAllocMatrix(3, 1, …)` /
    /// `(1, 3, …)` in `cmsio1.c`). `m` is row-major with `rows × cols` coefficients
    /// (`out[r] = Σ_c m[r·cols + c]·in[c] + offset[r]`), `offset` holds `rows` entries.
    /// [`Pipeline::new`] validates both lengths ([`CmmError::BadStage`] on mismatch) and the
    /// `1..=`[`MAX_CHANNELS`] bounds on the channel counts.
    MatrixN {
        /// The output channel count (matrix rows), in `1..=`[`MAX_CHANNELS`].
        rows: u8,
        /// The input channel count (matrix columns), in `1..=`[`MAX_CHANNELS`].
        cols: u8,
        /// The `rows × cols` coefficients, row-major.
        m: Vec<f64>,
        /// The per-row offset added after the multiply, `rows` entries.
        offset: Vec<f64>,
    },
    /// Converts one pixel of decoded PCSXYZ (D50-relative, `Y = 1.0`) to decoded CIELAB
    /// (D50 white, `L*` in `0..=100`) — 3-in/3-out, via
    /// [`gamut_color::lab::xyz_to_lab`] with [`gamut_color::lab::D50_XYZ`].
    ///
    /// The decoded-domain form of lcms2's `_cmsStageAllocXYZ2Lab` stage (`EvaluateXYZ2Lab`,
    /// `cmslut.c`, which un-encodes by `MAX_ENCODEABLE_XYZ`, runs `cmsXYZ2Lab` against D50,
    /// and re-encodes — this crate's PCS seams are already decoded, so only the colorimetric
    /// core remains). Used where a pipeline built over XYZ colorimetry must end at a Lab PCS:
    /// the Lab-PCS RGB matrix/TRC shaper (lcms2's `BuildRGBInputMatrixShaper` appends exactly
    /// this stage when `cmsGetPCS == cmsSigLabData`).
    XyzToLab,
    /// The inverse of [`XyzToLab`](Stage::XyzToLab): decoded CIELAB → decoded PCSXYZ,
    /// 3-in/3-out (lcms2's `_cmsStageAllocLab2XYZ`; prepended by
    /// `BuildRGBOutputMatrixShaper` for a Lab PCS).
    LabToXyz,
}

impl Stage {
    /// The number of samples this stage consumes per pixel.
    #[must_use]
    pub fn input_channels(&self) -> u8 {
        match self {
            Self::Identity { channels } | Self::Clamp { channels } => *channels,
            // Counts above 255 saturate for reporting; anything above MAX_CHANNELS is
            // rejected by `Pipeline::new` either way.
            Self::Curves(curves) => u8::try_from(curves.len()).unwrap_or(u8::MAX),
            Self::Clut(table) => table.input_channels(),
            Self::Matrix { .. } | Self::XyzToLab | Self::LabToXyz => 3,
            Self::MatrixN { cols, .. } => *cols,
        }
    }

    /// The number of samples this stage produces per pixel.
    #[must_use]
    pub fn output_channels(&self) -> u8 {
        match self {
            Self::Identity { channels } | Self::Clamp { channels } => *channels,
            Self::Curves(curves) => u8::try_from(curves.len()).unwrap_or(u8::MAX),
            Self::Clut(table) => table.output_channels(),
            Self::Matrix { .. } | Self::XyzToLab | Self::LabToXyz => 3,
            Self::MatrixN { rows, .. } => *rows,
        }
    }

    /// Checks the stage's *internal* consistency — the invariants a hand-built stage can
    /// violate that the channel accessors cannot express. Called once per stage by
    /// [`Pipeline::new`].
    ///
    /// # Errors
    ///
    /// [`CmmError::BadStage`] for a [`MatrixN`](Stage::MatrixN) whose coefficient or offset
    /// vector length contradicts its declared `rows`/`cols`.
    //
    // Exhaustive like `eval` (no wildcard): a new variant must decide its validation here.
    fn validate(&self) -> Result<()> {
        match self {
            Self::Identity { .. }
            | Self::Clamp { .. }
            | Self::Curves(_)
            | Self::Clut(_)
            | Self::Matrix { .. }
            | Self::XyzToLab
            | Self::LabToXyz => Ok(()),
            Self::MatrixN {
                rows,
                cols,
                m,
                offset,
            } => {
                if m.len() != usize::from(*rows) * usize::from(*cols) {
                    return Err(CmmError::BadStage(
                        "MatrixN coefficient count differs from rows x cols",
                    ));
                }
                if offset.len() != usize::from(*rows) {
                    return Err(CmmError::BadStage(
                        "MatrixN offset length differs from rows",
                    ));
                }
                Ok(())
            }
        }
    }

    /// Evaluates the stage over one pixel: reads `input_channels()` samples from `input` and
    /// writes `output_channels()` samples to `output`. The caller ([`Pipeline::eval`])
    /// guarantees both slice lengths.
    //
    // Deliberately an exhaustive match with NO wildcard arm: adding a `Stage` variant must fail
    // compilation here until its eval arm lands in the same change (see the enum's growth plan).
    pub(crate) fn eval(&self, input: &[f64], output: &mut [f64]) {
        match self {
            Self::Identity { .. } => output.copy_from_slice(input),
            Self::Clamp { .. } => {
                for (out, &v) in output.iter_mut().zip(input) {
                    // lcms2 fclamp semantics: NaN and negatives → 0.0, above 1.0 → 1.0.
                    *out = if v.is_nan() { 0.0 } else { v.clamp(0.0, 1.0) };
                }
            }
            Self::Curves(curves) => {
                for ((out, curve), &v) in output.iter_mut().zip(curves).zip(input) {
                    *out = curve.eval(v);
                }
            }
            Self::Clut(table) => table.eval(input, output),
            Self::Matrix { m, offset } => {
                for ((out, row), off) in output.iter_mut().zip(m).zip(offset) {
                    *out = row[0] * input[0] + row[1] * input[1] + row[2] * input[2] + off;
                }
            }
            Self::XyzToLab => {
                output.copy_from_slice(&xyz_to_lab([input[0], input[1], input[2]], D50_XYZ));
            }
            Self::LabToXyz => {
                output.copy_from_slice(&lab_to_xyz([input[0], input[1], input[2]], D50_XYZ));
            }
            Self::MatrixN {
                rows: _,
                cols,
                m,
                offset,
            } => {
                let cols = usize::from(*cols);
                for (row, (out, off)) in output.iter_mut().zip(offset).enumerate() {
                    let coefficients = &m[row * cols..(row + 1) * cols];
                    let mut acc = *off;
                    for (c, v) in coefficients.iter().zip(input) {
                        acc += c * v;
                    }
                    *out = acc;
                }
            }
        }
    }
}

/// Rejects a declared channel count outside `1..=`[`MAX_CHANNELS`].
fn check_channel_count(channels: u8) -> Result<()> {
    if channels == 0 || channels > MAX_CHANNELS {
        return Err(CmmError::TooManyChannels(channels));
    }
    Ok(())
}

/// A colour transform as a validated chain of [`Stage`]s.
///
/// Construction ([`Pipeline::new`]) is the validity boundary: every channel count is checked
/// once, so a constructed pipeline always evaluates. An empty stage list is a valid identity
/// pipeline (its declared ends must agree).
///
/// # Space/time tradeoff
///
/// Evaluation is allocation-free: [`Pipeline::eval`] ping-pongs two fixed
/// `[f64; MAX_CHANNELS]` stack buffers (256 bytes total, regardless of stage count) instead of
/// allocating intermediates, at the cost of capping channel counts at [`MAX_CHANNELS`] — the
/// bound ICC itself imposes on transforms.
#[derive(Debug, Clone)]
pub struct Pipeline {
    stages: Vec<Stage>,
    input_channels: u8,
    output_channels: u8,
}

impl Pipeline {
    /// Builds a pipeline from a stage chain, validating every channel count.
    ///
    /// Checks, in order: the declared ends and every stage's input/output are in
    /// `1..=`[`MAX_CHANNELS`]; every stage is internally consistent (a
    /// [`Stage::MatrixN`]'s vector lengths match its declared shape); each stage's input
    /// matches the previous stage's output; the declared `input_channels` equals the first
    /// stage's input and `output_channels` the last stage's output. An empty `stages` list
    /// builds a valid identity pipeline iff `input_channels == output_channels`.
    ///
    /// # Errors
    ///
    /// [`CmmError::TooManyChannels`] for a count outside `1..=`[`MAX_CHANNELS`],
    /// [`CmmError::BadStage`] for an internally inconsistent stage,
    /// [`CmmError::StageChannelMismatch`] for a disagreeing adjacent pair, and
    /// [`CmmError::PipelineEndsMismatch`] for a declared end the stage chain contradicts.
    pub fn new(input_channels: u8, output_channels: u8, stages: Vec<Stage>) -> Result<Self> {
        check_channel_count(input_channels)?;
        check_channel_count(output_channels)?;
        for stage in &stages {
            check_channel_count(stage.input_channels())?;
            check_channel_count(stage.output_channels())?;
            stage.validate()?;
        }
        for (index, pair) in stages.windows(2).enumerate() {
            if pair[1].input_channels() != pair[0].output_channels() {
                return Err(CmmError::StageChannelMismatch {
                    index: index + 1,
                    expected: pair[1].input_channels(),
                    found: pair[0].output_channels(),
                });
            }
        }
        if let (Some(first), Some(last)) = (stages.first(), stages.last()) {
            if first.input_channels() != input_channels {
                return Err(CmmError::PipelineEndsMismatch {
                    end: "input",
                    declared: input_channels,
                    found: first.input_channels(),
                });
            }
            if last.output_channels() != output_channels {
                return Err(CmmError::PipelineEndsMismatch {
                    end: "output",
                    declared: output_channels,
                    found: last.output_channels(),
                });
            }
        } else if input_channels != output_channels {
            // Empty stage list: identity, so the declared output must repeat the input.
            return Err(CmmError::PipelineEndsMismatch {
                end: "output",
                declared: output_channels,
                found: input_channels,
            });
        }
        Ok(Self {
            stages,
            input_channels,
            output_channels,
        })
    }

    /// The number of samples this pipeline consumes per pixel.
    #[must_use]
    pub fn input_channels(&self) -> u8 {
        self.input_channels
    }

    /// The number of samples this pipeline produces per pixel.
    #[must_use]
    pub fn output_channels(&self) -> u8 {
        self.output_channels
    }

    /// The validated stage chain, first to last.
    #[must_use]
    pub fn stages(&self) -> &[Stage] {
        &self.stages
    }

    /// Concatenates `next` after `self`: the result runs `self`'s stages, then `next`'s, with
    /// `self`'s input and `next`'s output as its ends.
    ///
    /// # Errors
    ///
    /// [`CmmError::StageChannelMismatch`] if `next`'s input channel count does not equal
    /// `self`'s output channel count (with `index` set to `self.stages().len()`, the position
    /// `next`'s first stage would take).
    pub fn compose(self, next: Pipeline) -> Result<Pipeline> {
        if next.input_channels != self.output_channels {
            return Err(CmmError::StageChannelMismatch {
                index: self.stages.len(),
                expected: next.input_channels,
                found: self.output_channels,
            });
        }
        let mut stages = self.stages;
        stages.extend(next.stages);
        Ok(Pipeline {
            stages,
            input_channels: self.input_channels,
            output_channels: next.output_channels,
        })
    }

    /// Evaluates the pipeline over one pixel: `input` holds exactly
    /// [`input_channels`](Self::input_channels) samples, `output` exactly
    /// [`output_channels`](Self::output_channels).
    ///
    /// Allocation-free: intermediates ping-pong between two `[f64; MAX_CHANNELS]` stack
    /// buffers (see the type-level space/time note). For whole interleaved buffers, use the
    /// [`Transform`] impl instead.
    ///
    /// # Errors
    ///
    /// [`CmmError::BufferLength`] if either slice's length differs from the declared per-pixel
    /// channel count.
    pub fn eval(&self, input: &[f64], output: &mut [f64]) -> Result<()> {
        if input.len() != usize::from(self.input_channels) {
            return Err(CmmError::BufferLength {
                channels: self.input_channels,
                found: input.len(),
            });
        }
        if output.len() != usize::from(self.output_channels) {
            return Err(CmmError::BufferLength {
                channels: self.output_channels,
                found: output.len(),
            });
        }
        let mut a = [0.0_f64; MAX_CHANNELS as usize];
        let mut b = [0.0_f64; MAX_CHANNELS as usize];
        a[..input.len()].copy_from_slice(input);
        let (mut cur, mut next) = (&mut a, &mut b);
        for stage in &self.stages {
            let n_in = usize::from(stage.input_channels());
            let n_out = usize::from(stage.output_channels());
            stage.eval(&cur[..n_in], &mut next[..n_out]);
            core::mem::swap(&mut cur, &mut next);
        }
        output.copy_from_slice(&cur[..output.len()]);
        Ok(())
    }
}

impl Transform for Pipeline {
    fn transform(&self, src: &[f64], dst: &mut [f64]) -> Result<()> {
        let n_in = usize::from(self.input_channels);
        let n_out = usize::from(self.output_channels);
        if !src.len().is_multiple_of(n_in) {
            return Err(CmmError::BufferLength {
                channels: self.input_channels,
                found: src.len(),
            });
        }
        if dst.len() != (src.len() / n_in) * n_out {
            return Err(CmmError::BufferLength {
                channels: self.output_channels,
                found: dst.len(),
            });
        }
        for (pixel_in, pixel_out) in src.chunks_exact(n_in).zip(dst.chunks_exact_mut(n_out)) {
            self.eval(pixel_in, pixel_out)?;
        }
        Ok(())
    }

    fn input_channels(&self) -> u8 {
        self.input_channels
    }

    fn output_channels(&self) -> u8 {
        self.output_channels
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A non-trivial matrix stage whose products and sums are exact dyadic rationals, so every
    /// coefficient position can be asserted with exact `f64` equality.
    fn dyadic_matrix() -> Stage {
        Stage::Matrix {
            m: [[0.5, -0.25, 0.125], [1.0, 2.0, -0.5], [-2.0, 0.25, 1.0]],
            offset: [0.5, -0.25, 2.0],
        }
    }

    #[test]
    fn matrix_eval_matches_hand_computation() {
        let mut out = [0.0; 3];
        dyadic_matrix().eval(&[0.25, 0.5, -1.0], &mut out);
        // Row by row: 0.5·0.25 − 0.25·0.5 + 0.125·(−1) + 0.5      = 0.375
        //             1.0·0.25 + 2.0·0.5  − 0.5·(−1)   − 0.25     = 1.5
        //            −2.0·0.25 + 0.25·0.5 + 1.0·(−1)   + 2.0      = 0.625
        assert_eq!(out, [0.375, 1.5, 0.625]);
    }

    #[test]
    fn clamp_eval_follows_lcms2_fclamp() {
        let stage = Stage::Clamp { channels: 6 };
        let mut out = [9.0; 6];
        stage.eval(&[-0.5, f64::NAN, 1.5, 0.5, 0.0, 1.0], &mut out);
        // Negatives and NaN → 0.0; above 1.0 → 1.0; in-range (boundaries included) unchanged.
        assert_eq!(out, [0.0, 0.0, 1.0, 0.5, 0.0, 1.0]);
    }

    #[test]
    fn identity_eval_copies_input() {
        let stage = Stage::Identity { channels: 4 };
        let mut out = [0.0; 4];
        stage.eval(&[0.1, -2.0, 7.5, 1.0], &mut out);
        assert_eq!(out, [0.1, -2.0, 7.5, 1.0]);
    }

    #[test]
    fn stage_channel_accessors_per_variant() {
        assert_eq!(Stage::Identity { channels: 5 }.input_channels(), 5);
        assert_eq!(Stage::Identity { channels: 5 }.output_channels(), 5);
        assert_eq!(Stage::Clamp { channels: 7 }.input_channels(), 7);
        assert_eq!(Stage::Clamp { channels: 7 }.output_channels(), 7);
        assert_eq!(dyadic_matrix().input_channels(), 3);
        assert_eq!(dyadic_matrix().output_channels(), 3);
        for stage in [Stage::XyzToLab, Stage::LabToXyz] {
            assert_eq!(stage.input_channels(), 3);
            assert_eq!(stage.output_channels(), 3);
        }
    }

    #[test]
    fn xyz_to_lab_eval_delegates_to_gamut_color_with_d50() {
        use gamut_color::lab::{D50_XYZ, xyz_to_lab};
        // The D50 white maps to L* = 100, a* = b* = 0 — and only under the D50 white; a wrong
        // white constant (or a swapped conversion direction) breaks this exact anchor.
        let mut out = [0.0; 3];
        Stage::XyzToLab.eval(&D50_XYZ, &mut out);
        assert!((out[0] - 100.0).abs() < 1e-12, "L* = {}", out[0]);
        assert!(out[1].abs() < 1e-12 && out[2].abs() < 1e-12, "{out:?}");
        // An off-white chromatic probe matches the gamut-color reference exactly (bitwise:
        // the stage is a delegation, not a re-derivation).
        let xyz = [0.25, 0.5, 0.125];
        Stage::XyzToLab.eval(&xyz, &mut out);
        assert_eq!(out, xyz_to_lab(xyz, D50_XYZ));
    }

    #[test]
    fn lab_to_xyz_eval_inverts_xyz_to_lab() {
        use gamut_color::lab::{D50_XYZ, lab_to_xyz};
        let lab = [62.5, -20.25, 33.75];
        let mut xyz = [0.0; 3];
        Stage::LabToXyz.eval(&lab, &mut xyz);
        assert_eq!(xyz, lab_to_xyz(lab, D50_XYZ));
        // Round trip through both stages is f64-tight.
        let mut back = [0.0; 3];
        Stage::XyzToLab.eval(&xyz, &mut back);
        for ch in 0..3 {
            assert!((back[ch] - lab[ch]).abs() < 1e-12, "{back:?} vs {lab:?}");
        }
    }

    /// A rectangular 2×3 affine stage with exact-dyadic coefficients, offsets included.
    fn dyadic_matrix_n() -> Stage {
        Stage::MatrixN {
            rows: 2,
            cols: 3,
            m: vec![0.5, -0.25, 0.125, 1.0, 2.0, -0.5],
            offset: vec![0.5, -0.25],
        }
    }

    #[test]
    fn matrix_n_eval_matches_hand_computation() {
        let mut out = [0.0; 2];
        dyadic_matrix_n().eval(&[0.25, 0.5, -1.0], &mut out);
        // Row 0: 0.5·0.25 − 0.25·0.5 + 0.125·(−1) + 0.5 = 0.375
        // Row 1: 1.0·0.25 + 2.0·0.5  − 0.5·(−1)   − 0.25 = 1.5
        assert_eq!(out, [0.375, 1.5]);
    }

    #[test]
    fn matrix_n_eval_covers_column_and_row_shapes() {
        // 3×1 (the gray device→PCS shape): out = column · scalar, exactly.
        let widen = Stage::MatrixN {
            rows: 3,
            cols: 1,
            m: vec![0.5, 1.0, -0.25],
            offset: vec![0.0, 0.125, 0.0],
        };
        let mut out = [0.0; 3];
        widen.eval(&[0.5], &mut out);
        assert_eq!(out, [0.25, 0.625, -0.125]);
        // 1×3 (the gray PCS→device shape): picks/combines a row, exactly.
        let pick = Stage::MatrixN {
            rows: 1,
            cols: 3,
            m: vec![0.0, 1.0, 0.0],
            offset: vec![0.0],
        };
        let mut out = [0.0; 1];
        pick.eval(&[0.25, 0.75, 0.5], &mut out);
        assert_eq!(out, [0.75]);
    }

    #[test]
    fn matrix_n_channel_accessors_are_cols_in_rows_out() {
        assert_eq!(dyadic_matrix_n().input_channels(), 3);
        assert_eq!(dyadic_matrix_n().output_channels(), 2);
    }

    #[test]
    fn pipeline_rejects_malformed_matrix_n() {
        // Coefficient count disagrees with rows × cols.
        let err = Pipeline::new(
            3,
            2,
            vec![Stage::MatrixN {
                rows: 2,
                cols: 3,
                m: vec![0.0; 5],
                offset: vec![0.0; 2],
            }],
        )
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "cmm: malformed stage (MatrixN coefficient count differs from rows x cols)"
        );
        // Offset length disagrees with rows.
        let err = Pipeline::new(
            3,
            2,
            vec![Stage::MatrixN {
                rows: 2,
                cols: 3,
                m: vec![0.0; 6],
                offset: vec![0.0; 3],
            }],
        )
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "cmm: malformed stage (MatrixN offset length differs from rows)"
        );
        // Zero rows is a channel-count violation, caught before the length checks.
        let err = Pipeline::new(
            3,
            1,
            vec![Stage::MatrixN {
                rows: 0,
                cols: 3,
                m: vec![],
                offset: vec![],
            }],
        )
        .unwrap_err();
        assert!(matches!(err, CmmError::TooManyChannels(0)));
    }

    #[test]
    fn well_formed_matrix_n_builds_and_runs_in_a_pipeline() {
        let pipeline = Pipeline::new(3, 2, vec![dyadic_matrix_n()]).unwrap();
        let mut out = [0.0; 2];
        pipeline.eval(&[0.25, 0.5, -1.0], &mut out).unwrap();
        assert_eq!(out, [0.375, 1.5]);
    }

    #[test]
    fn eval_ping_pongs_through_more_than_two_stages() {
        // Matrix → Identity → Clamp exercises both swap directions of the two stack buffers.
        let pipeline = Pipeline::new(
            3,
            3,
            vec![
                dyadic_matrix(),
                Stage::Identity { channels: 3 },
                Stage::Clamp { channels: 3 },
            ],
        )
        .unwrap();
        let mut out = [0.0; 3];
        pipeline.eval(&[0.25, 0.5, -1.0], &mut out).unwrap();
        // Matrix result [0.375, 1.5, 0.625] survives Identity, then Clamp caps 1.5 to 1.0.
        assert_eq!(out, [0.375, 1.0, 0.625]);
    }
}
