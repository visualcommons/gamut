//! Stage-collapsing pipeline optimization: the opt-in throughput path (#372).
//!
//! v1 evaluates a transform stage by stage, which is the clearest and most testable model but
//! pays a per-pixel cost for every stage a profile link happens to produce — a two-shaper pair
//! runs six `powf` calls and two 3×3 products per pixel where one grid lookup would do. lcms2
//! collapses such a chain into a single precalculated table by default (`cmsopt.c`); this
//! module is that idea, transcribed into the crate's `Pipeline`/`Stage` model and put **behind
//! an opt-in knob** ([`PipelineOptimization`], default [`None`](PipelineOptimization::None)) so
//! v1's exact stage-by-stage numerics stay the default.
//!
//! # The passes
//!
//! | pass | level | what it does |
//! |------|-------|--------------|
//! | identity elision | [`Collapse`](PipelineOptimization::Collapse) | drops [`Stage::Identity`] and any exactly-identity affine stage |
//! | matrix folding | [`Collapse`](PipelineOptimization::Collapse) | replaces an adjacent affine pair with their composed affine |
//! | curve joining | [`Precalculate`](PipelineOptimization::Precalculate) | tabulates an adjacent [`Stage::Curves`] pair as one curve set |
//! | CLUT resampling | [`Precalculate`](PipelineOptimization::Precalculate) | replaces the whole chain with one resampled [`Stage::Clut`] |
//!
//! Elision and folding both shrink the stage list by one, so the two run together to a
//! fixpoint (termination is structural: every rewriting pass strictly reduces the stage
//! count).
//!
//! # Precision
//!
//! No pass is bit-preserving, and none is meant to be — the knob's contract is that
//! [`None`](PipelineOptimization::None) is, and that each level stays inside a stated budget:
//!
//! - **[`Collapse`]** is exact up to `f64` re-association: folding `B·(A·x + a) + b` into
//!   `(B·A)·x + (B·a + b)` rounds differently in the last places, and eliding an identity
//!   affine drops multiplications by `0.0` that today turn a non-finite sample into `NaN`
//!   across every output channel. Measured against unoptimized output over the conformance
//!   battery the worst deviation is **1.0e-15 device units** — three ulps of a sample, and
//!   below what the gate's ΔE₀₀ lens can even resolve (STATUS.md).
//! - **[`Precalculate`]** is the lossy tier, and deliberately the *same* construction lcms2's
//!   default path applies: a 4096-point joined curve table and a grid-33 (RGB) / grid-17
//!   (CMYK) / grid-7 (hifi) resampled CLUT. Its budget is therefore the conformance gate's
//!   **LOOSE** row — max ΔE₀₀ `< 6e-1` for shaper pairs and `< 2.0` for LUT pairs (measured
//!   2.6e-1 / 5.1e-2) — because that row already measures exactly this approximation on the
//!   oracle's side (STATUS.md, "Conformance gate (P7)"). Matching lcms2's construction rather
//!   than inventing one also brings the *outputs* together: optimized-vs-lcms2-optimized
//!   measures 3.5e-3 ΔE₀₀ where unoptimized-vs-lcms2-optimized measures 3.3e-1.
//!
//! One deliberate divergence from lcms2: no white-point "scum dot" fixup is applied to the
//! resampled table (lcms2's `PatchLUT`, which its callers disable with
//! `cmsFLAGS_NOWHITEONWHITEFIXUP` — the conformance gate disables it too, because it would
//! dominate the metric at the device-white corner).
//!
//! [`Collapse`]: PipelineOptimization::Collapse
//! [`Precalculate`]: PipelineOptimization::Precalculate

use crate::clut::{ClutInterpolation, ClutTable};
use crate::curve::ToneCurve;
use crate::error::Result;
use crate::pipeline::{MAX_CHANNELS, Pipeline, Stage};

/// How far [`Pipeline::optimized`] collapses a stage chain before it is evaluated.
///
/// The levels are cumulative: [`Precalculate`](Self::Precalculate) runs everything
/// [`Collapse`](Self::Collapse) runs. The default is [`None`](Self::None) — v1's exact
/// stage-by-stage semantics — so opting into a level is always a deliberate act.
///
/// The discriminants are permanent and append-only, and the type is a fieldless `#[repr(u32)]`
/// enum (the workspace C-portability convention, as on
/// [`ClutInterpolation`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u32)]
pub enum PipelineOptimization {
    /// No optimization: the pipeline is evaluated exactly as it was built, stage by stage.
    /// The default, and the only level whose numeric output is pinned by the crate's v1
    /// conformance gate.
    #[default]
    None = 0,
    /// Structural collapsing only: identity elision and adjacent-matrix folding. Changes
    /// results solely by `f64` re-association (module docs) — worth taking wherever a
    /// transform is applied to more than a handful of pixels.
    Collapse = 1,
    /// [`Collapse`](Self::Collapse) plus the sampled-table passes: curve joining and
    /// whole-pipeline CLUT resampling. This is lcms2's default path, and carries its
    /// precision budget (module docs) — the throughput tier, for pixel-buffer workloads that
    /// can spend the resampling's build time and memory.
    Precalculate = 2,
}

/// Grid nodes per axis for a resampled CLUT, by input channel count — lcms2's
/// `_cmsReasonableGridpointsByColorspace` default arm (`cmspcs.c:695-703`): 7 above four
/// channels, 17 for CMYK, 33 for RGB and everything narrower.
fn reasonable_grid_points(input_channels: u8) -> u8 {
    match input_channels {
        0..=3 => 33,
        4 => 17,
        _ => 7,
    }
}

/// The node-count ceiling on a resampled CLUT: past it the pass declines rather than
/// allocating.
///
/// Set to **exactly** the widest grid the pass will ever build — `7^7` = 823 543 nodes, seven
/// hifi inputs at [`reasonable_grid_points`]'s 7-node resolution (~52 MB of `f64` samples at 8
/// output channels). The next step up, `7^8`, is 5.8M nodes and ~370 MB, so it declines. lcms2
/// needs no such guard: its tables are 16-bit and its own hifi grids stay small.
///
/// Sitting the constant *on* a reachable grid rather than near one is deliberate — it makes the
/// ceiling a boundary a test can straddle exactly, which a round number like `1 << 20` could not,
/// since no product of the 33/17/7 grid rule lands on it.
///
/// It also **subsumes** [`ClutTable`]'s own 15-axis limit: `reasonable_grid_points` returns 7 for
/// any input count above four, and `7^8` already exceeds this ceiling, so no accepted grid ever
/// reaches eight axes — let alone sixteen. A separate axis guard would be unreachable code.
const MAX_RESAMPLED_NODES: usize = 7_usize.pow(7);

/// Applies `level`'s passes to `pipeline` (the funnel behind [`Pipeline::optimized`]).
///
/// # Errors
///
/// Whatever [`Pipeline::new`] reports for the rebuilt chain — unreachable for a pipeline that
/// was valid on the way in, since every pass preserves each seam's channel count.
pub(crate) fn optimize(pipeline: Pipeline, level: PipelineOptimization) -> Result<Pipeline> {
    match level {
        PipelineOptimization::None => Ok(pipeline),
        PipelineOptimization::Collapse => collapse(pipeline),
        PipelineOptimization::Precalculate => {
            let joined = join_curves(collapse(pipeline)?)?;
            resample(joined)
        }
    }
}

/// Rebuilds `pipeline` with `stages`, keeping its declared ends.
fn rebuild(pipeline: &Pipeline, stages: Vec<Stage>) -> Result<Pipeline> {
    Pipeline::new(
        pipeline.input_channels(),
        pipeline.output_channels(),
        stages,
    )
}

/// The affine view of a stage — `(rows, cols, row-major coefficients, offset)` — or `None`
/// for a stage that is not an affine map. [`Stage::Matrix`] is the 3×3 case of
/// [`Stage::MatrixN`], so both answer here and fold against each other.
//
// Deliberately exhaustive (no wildcard), like `Stage::eval`: a new variant must decide
// whether it is affine before this compiles.
fn affine(stage: &Stage) -> Option<(usize, usize, Vec<f64>, Vec<f64>)> {
    match stage {
        Stage::Matrix { m, offset } => Some((3, 3, m.as_flattened().to_vec(), offset.to_vec())),
        Stage::MatrixN {
            rows,
            cols,
            m,
            offset,
        } => Some((
            usize::from(*rows),
            usize::from(*cols),
            m.clone(),
            offset.clone(),
        )),
        Stage::Identity { .. }
        | Stage::Clamp { .. }
        | Stage::Curves(_)
        | Stage::Clut(_)
        | Stage::XyzToLab
        | Stage::LabToXyz => None,
    }
}

/// The stage an affine `(rows, cols, m, offset)` evaluates as: the 3×3 case narrows back to
/// [`Stage::Matrix`], so folding a `Matrix` pair yields a `Matrix` again (and the pipelines
/// this crate builds keep the shape their pattern matches expect).
fn affine_stage(rows: usize, cols: usize, m: Vec<f64>, offset: Vec<f64>) -> Stage {
    if rows == 3 && cols == 3 {
        let mut square = [[0.0; 3]; 3];
        for (r, row) in square.iter_mut().enumerate() {
            row.copy_from_slice(&m[r * 3..r * 3 + 3]);
        }
        let mut off = [0.0; 3];
        off.copy_from_slice(&offset);
        return Stage::Matrix {
            m: square,
            offset: off,
        };
    }
    Stage::MatrixN {
        // Both counts came from a validated stage, so they are in `1..=MAX_CHANNELS`.
        rows: rows as u8,
        cols: cols as u8,
        m,
        offset,
    }
}

/// Whether an affine stage is *exactly* the identity map: square, a unit diagonal, zero
/// off-diagonals, and a zero offset, all by exact `f64` equality (no tolerance — an
/// approximate identity is a real, if small, colour operation and stays).
fn is_identity_affine(rows: usize, cols: usize, m: &[f64], offset: &[f64]) -> bool {
    rows == cols
        && offset.iter().all(|&o| o == 0.0)
        && m.iter().enumerate().all(|(i, &v)| {
            let expected = if i / cols == i % cols { 1.0 } else { 0.0 };
            v == expected
        })
}

/// Composes affine `a` then affine `b` into one: `b(a(x)) = (B·A)·x + (B·a_off + b_off)`.
/// `b`'s column count equals `a`'s row count (the pipeline's validated seam), so the product
/// is `b_rows × a_cols`.
fn fold_affine(
    (a_rows, a_cols, a_m, a_off): &(usize, usize, Vec<f64>, Vec<f64>),
    (b_rows, b_cols, b_m, b_off): &(usize, usize, Vec<f64>, Vec<f64>),
) -> Option<Stage> {
    if b_cols != a_rows {
        return None;
    }
    let mut m = vec![0.0; b_rows * a_cols];
    for r in 0..*b_rows {
        for c in 0..*a_cols {
            let mut acc = 0.0;
            for k in 0..*a_rows {
                acc += b_m[r * b_cols + k] * a_m[k * a_cols + c];
            }
            m[r * a_cols + c] = acc;
        }
    }
    let offset = (0..*b_rows)
        .map(|r| {
            let mut acc = b_off[r];
            for k in 0..*a_rows {
                acc += b_m[r * b_cols + k] * a_off[k];
            }
            acc
        })
        .collect();
    Some(affine_stage(*b_rows, *a_cols, m, offset))
}

/// Identity elision and adjacent-matrix folding, run to a fixpoint.
///
/// # Errors
///
/// As [`optimize`].
fn collapse(pipeline: Pipeline) -> Result<Pipeline> {
    let (input_channels, output_channels, mut stages) = pipeline.into_parts();
    // Every rewriting pass removes at least one stage, so at most `len` passes rewrite anything
    // and the one after them confirms the fixpoint. The loop is bounded by that count rather than
    // only by `changed`, so a sweep that misreports a change — `collapse_once` stubbed to
    // `(vec![], true)` hung the suite (#585) — runs out of passes instead.
    for _ in 0..=stages.len() {
        let (next, changed) = collapse_once(stages);
        stages = next;
        if !changed {
            break;
        }
    }
    Pipeline::new(input_channels, output_channels, stages)
}

/// One elision/folding sweep; reports whether it rewrote anything.
fn collapse_once(stages: Vec<Stage>) -> (Vec<Stage>, bool) {
    let mut out: Vec<Stage> = Vec::with_capacity(stages.len());
    let mut changed = false;
    for stage in stages {
        // Identity elision: a pass-through stage, or an affine that is exactly the identity.
        if matches!(stage, Stage::Identity { .. }) {
            changed = true;
            continue;
        }
        if let Some((rows, cols, m, offset)) = affine(&stage)
            && is_identity_affine(rows, cols, &m, &offset)
        {
            changed = true;
            continue;
        }
        // Matrix folding: this stage against the one already emitted before it.
        if let (Some(previous), Some(current)) = (out.last().and_then(affine), affine(&stage))
            && let Some(folded) = fold_affine(&previous, &current)
        {
            out.pop();
            out.push(folded);
            changed = true;
            continue;
        }
        out.push(stage);
    }
    (out, changed)
}

/// Tabulates every adjacent [`Stage::Curves`] pair as one curve set ([`ToneCurve::joined`]).
/// Adjacent curve sets always carry the same channel count (the pipeline's validated seam),
/// so the join is always shape-legal.
///
/// # Errors
///
/// As [`optimize`].
fn join_curves(pipeline: Pipeline) -> Result<Pipeline> {
    let (input_channels, output_channels, stages) = pipeline.into_parts();
    let mut out: Vec<Stage> = Vec::with_capacity(stages.len());
    for stage in stages {
        if let (Some(Stage::Curves(first)), Stage::Curves(second)) = (out.last(), &stage) {
            let joined = first
                .iter()
                .zip(second)
                .map(|(a, b)| ToneCurve::joined(a, b))
                .collect();
            out.pop();
            out.push(Stage::Curves(joined));
            continue;
        }
        out.push(stage);
    }
    Pipeline::new(input_channels, output_channels, out)
}

/// Whether a stage confines its input to `[0, 1]` — the property that makes the unit
/// hypercube the pipeline's whole reachable input domain, and so makes a grid over that cube
/// a faithful resampling.
///
/// [`Stage::Curves`] clamps per channel ([`ToneCurve::eval`]), [`Stage::Clut`] clamps per axis
/// (lcms2's `fclamp`), and [`Stage::Clamp`] is the clamp itself. Every other stage passes its
/// input through unbounded — which is exactly how a PCS-entering pipeline starts (the link
/// module prepends the PCS *encode* matrix that maps decoded colorimetry into `[0, 1]`), so
/// this test is what keeps resampling off pipelines whose input is not the device cube.
fn clamps_domain(stage: &Stage) -> bool {
    matches!(
        stage,
        Stage::Curves(_) | Stage::Clut(_) | Stage::Clamp { .. }
    )
}

/// Replaces the whole chain with one CLUT resampled over the unit input hypercube, when that
/// is both sound and worth doing. Declines (returning the pipeline unchanged) when:
///
/// - the chain has fewer than two stages — one stage is already a single lookup, and
///   resampling it could only add error and memory;
/// - it is already a single resampled-shaped [`Stage::Clut`];
/// - the first stage does not confine its input to `[0, 1]` ([`clamps_domain`]): the grid
///   would then cover only part of the reachable domain, silently clamping everything else;
/// - the last stage does not confine its *output* to `[0, 1]`: [`ClutTable`] holds normalized
///   node samples, so a pipeline ending in decoded colorimetry (a PCS end) is not
///   representable as one;
/// - the grid would exceed [`MAX_RESAMPLED_NODES`], which is tighter than the 15 axes a
///   [`ClutTable`] can index and is therefore the only size guard needed.
///
/// # Errors
///
/// As [`optimize`], plus [`ClutTable::from_samples`]'s geometry errors — unreachable here,
/// since the grid is constructed to satisfy them.
fn resample(pipeline: Pipeline) -> Result<Pipeline> {
    let stages = pipeline.stages();
    if stages.len() < 2 {
        return Ok(pipeline);
    }
    let (Some(first), Some(last)) = (stages.first(), stages.last()) else {
        return Ok(pipeline);
    };
    if !clamps_domain(first) || !clamps_domain(last) {
        return Ok(pipeline);
    }
    let inputs = pipeline.input_channels();
    let outputs = pipeline.output_channels();
    let grid = reasonable_grid_points(inputs);
    let axes = usize::from(inputs);
    // The whole grid must fit the node ceiling, which is tighter than the 15 axes a CLUT can
    // index (see MAX_RESAMPLED_NODES) and so is the only size guard the pass needs.
    let Some(nodes) = usize::from(grid)
        .checked_pow(u32::from(inputs))
        .filter(|&n| n <= MAX_RESAMPLED_NODES)
    else {
        return Ok(pipeline);
    };
    let divisor = f64::from(grid - 1);
    let mut samples = Vec::with_capacity(nodes * usize::from(outputs));
    let mut coordinates = vec![0.0_f64; axes];
    let mut node = [0.0_f64; MAX_CHANNELS as usize];
    let node = &mut node[..usize::from(outputs)];
    for index in 0..nodes {
        // Decode the flat node index into per-axis grid positions, last axis fastest — the
        // §10.12.3 sample order `ClutTable` expects.
        let mut rest = index;
        for coordinate in coordinates.iter_mut().rev() {
            *coordinate = f64::from(u32::try_from(rest % usize::from(grid)).unwrap_or(0)) / divisor;
            rest /= usize::from(grid);
        }
        pipeline.eval(&coordinates, node)?;
        samples.extend_from_slice(node);
    }
    // The resampled table is indexed by the device cube, so it takes lcms2's device default:
    // tetrahedral from 3 axes up, multilinear below (where the two agree anyway).
    let interpolation = if axes >= 3 {
        ClutInterpolation::Tetrahedral
    } else {
        ClutInterpolation::Multilinear
    };
    let table = ClutTable::from_samples(vec![grid; axes], outputs, samples, interpolation)?;
    rebuild(&pipeline, vec![Stage::Clut(table)])
}

#[cfg(test)]
mod tests {
    use gamut_icc::{Curve, CurveOrParametric, U8Fixed8};

    use super::*;

    /// A non-trivial 3×3 affine over exact dyadic rationals, so a fold's coefficients assert
    /// with `==`.
    fn dyadic() -> Stage {
        Stage::Matrix {
            m: [[0.5, -0.25, 0.125], [1.0, 2.0, -0.5], [-2.0, 0.25, 1.0]],
            offset: [0.5, -0.25, 2.0],
        }
    }

    fn identity_matrix() -> Stage {
        Stage::Matrix {
            m: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
            offset: [0.0; 3],
        }
    }

    fn gamma(g: u16) -> ToneCurve {
        ToneCurve::new(&CurveOrParametric::Curve(Curve::Gamma(U8Fixed8(g))))
            .expect("a gamma curve is always constructible")
    }

    fn identity_curve() -> ToneCurve {
        ToneCurve::new(&CurveOrParametric::Curve(Curve::Identity))
            .expect("the identity curve is always constructible")
    }

    fn kinds(pipeline: &Pipeline) -> Vec<&'static str> {
        pipeline
            .stages()
            .iter()
            .map(|stage| match stage {
                Stage::Identity { .. } => "identity",
                Stage::Clamp { .. } => "clamp",
                Stage::Curves(_) => "curves",
                Stage::Clut(_) => "clut",
                Stage::Matrix { .. } => "matrix",
                Stage::MatrixN { .. } => "matrixn",
                Stage::XyzToLab => "xyz2lab",
                Stage::LabToXyz => "lab2xyz",
            })
            .collect()
    }

    fn eval(pipeline: &Pipeline, input: &[f64]) -> Vec<f64> {
        let mut out = vec![0.0; usize::from(pipeline.output_channels())];
        pipeline.eval(input, &mut out).expect("shapes agree");
        out
    }

    #[test]
    fn none_is_the_identity_pass() {
        let pipeline = Pipeline::new(
            3,
            3,
            vec![dyadic(), Stage::Identity { channels: 3 }, identity_matrix()],
        )
        .unwrap();
        let before = kinds(&pipeline);
        let after = optimize(pipeline, PipelineOptimization::None).unwrap();
        // Nothing is elided, folded, or reordered: the chain is exactly as it was built.
        assert_eq!(kinds(&after), before);
        assert_eq!(after.stages().len(), 3);
    }

    #[test]
    fn identity_stages_and_identity_matrices_are_elided() {
        let pipeline = Pipeline::new(
            3,
            3,
            vec![
                Stage::Identity { channels: 3 },
                Stage::Clamp { channels: 3 },
                identity_matrix(),
                Stage::Clamp { channels: 3 },
            ],
        )
        .unwrap();
        let optimized = optimize(pipeline, PipelineOptimization::Collapse).unwrap();
        assert_eq!(kinds(&optimized), ["clamp", "clamp"]);
    }

    #[test]
    fn a_near_identity_matrix_is_not_elided() {
        // One ulp off the identity is still a colour operation; elision is exact-only.
        let mut m = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
        m[1][1] = 1.0 + f64::EPSILON;
        let pipeline = Pipeline::new(
            3,
            3,
            vec![Stage::Matrix {
                m,
                offset: [0.0; 3],
            }],
        )
        .unwrap();
        let optimized = optimize(pipeline, PipelineOptimization::Collapse).unwrap();
        assert_eq!(kinds(&optimized), ["matrix"]);
    }

    #[test]
    fn adjacent_matrices_fold_into_their_composition() {
        let pipeline = Pipeline::new(3, 3, vec![dyadic(), dyadic()]).unwrap();
        let optimized = optimize(pipeline, PipelineOptimization::Collapse).unwrap();
        assert_eq!(kinds(&optimized), ["matrix"]);
        let Stage::Matrix { m, offset } = &optimized.stages()[0] else {
            panic!("the fold of two 3x3 matrices narrows back to Stage::Matrix");
        };
        // Row 0 of A·A, by hand: [0.5,-0.25,0.125]·A columns.
        //   0.5·0.5   + (−0.25)·1.0 + 0.125·(−2.0) = −0.25
        //   0.5·(−0.25) + (−0.25)·2.0 + 0.125·0.25 = −0.59375
        //   0.5·0.125 + (−0.25)·(−0.5) + 0.125·1.0 = 0.3125
        assert_eq!(m[0], [-0.25, -0.59375, 0.3125]);
        // offset row 0: 0.5·0.5 + (−0.25)·(−0.25) + 0.125·2.0 + 0.5 = 1.0625
        assert_eq!(offset[0], 1.0625);
    }

    #[test]
    fn folding_matches_stage_by_stage_evaluation() {
        let stages = vec![dyadic(), dyadic(), dyadic()];
        let unoptimized = Pipeline::new(3, 3, stages).unwrap();
        let optimized = optimize(unoptimized.clone(), PipelineOptimization::Collapse).unwrap();
        assert_eq!(optimized.stages().len(), 1);
        for probe in [[0.0, 0.0, 0.0], [0.25, 0.5, -1.0], [1.0, 1.0, 1.0]] {
            let a = eval(&unoptimized, &probe);
            let b = eval(&optimized, &probe);
            for (x, y) in a.iter().zip(&b) {
                assert!((x - y).abs() < 1e-12, "{a:?} vs {b:?} at {probe:?}");
            }
        }
    }

    #[test]
    fn rectangular_matrices_fold_across_a_shape_change() {
        // 1→3 white scaling then 3→1 channel picking: the fold is a 1×1 MatrixN.
        let up = Stage::MatrixN {
            rows: 3,
            cols: 1,
            m: vec![0.5, 0.25, 0.125],
            offset: vec![0.0; 3],
        };
        let down = Stage::MatrixN {
            rows: 1,
            cols: 3,
            m: vec![0.0, 1.0, 0.0],
            offset: vec![0.25],
        };
        let pipeline = Pipeline::new(1, 1, vec![up, down]).unwrap();
        let optimized = optimize(pipeline, PipelineOptimization::Collapse).unwrap();
        let [
            Stage::MatrixN {
                rows,
                cols,
                m,
                offset,
            },
        ] = optimized.stages()
        else {
            panic!("a 1x3 by 3x1 fold is a 1x1 MatrixN");
        };
        assert_eq!((*rows, *cols), (1, 1));
        assert_eq!(m.as_slice(), [0.25]);
        assert_eq!(offset.as_slice(), [0.25]);
    }

    #[test]
    fn elision_uncovers_a_further_fold() {
        // The identity between the two matrices blocks folding until it is elided — the
        // fixpoint loop is what makes the second sweep happen.
        let pipeline = Pipeline::new(
            3,
            3,
            vec![dyadic(), Stage::Identity { channels: 3 }, dyadic()],
        )
        .unwrap();
        let optimized = optimize(pipeline, PipelineOptimization::Collapse).unwrap();
        assert_eq!(kinds(&optimized), ["matrix"]);
    }

    /// Kills `delete !` in `collapse`. `elision_uncovers_a_further_fold` finishes in one sweep
    /// (an elided stage is never pushed, so the fold sees its neighbour at once); here the fold
    /// itself *produces* the identity, which only a second sweep elides.
    #[test]
    fn a_fold_that_yields_the_identity_is_elided_by_the_next_sweep() {
        let scale = |d: [f64; 3]| Stage::Matrix {
            m: [[d[0], 0.0, 0.0], [0.0, d[1], 0.0], [0.0, 0.0, d[2]]],
            offset: [0.0; 3],
        };
        let pipeline =
            Pipeline::new(3, 3, vec![scale([2.0, 4.0, 0.5]), scale([0.5, 0.25, 2.0])]).unwrap();
        let optimized = optimize(pipeline, PipelineOptimization::Collapse).unwrap();
        assert!(kinds(&optimized).is_empty(), "{:?}", kinds(&optimized));
    }

    #[test]
    fn collapse_leaves_curves_and_cluts_alone() {
        let pipeline = Pipeline::new(
            3,
            3,
            vec![
                Stage::Curves(vec![gamma(0x0233); 3]),
                dyadic(),
                Stage::Curves(vec![gamma(0x0100); 3]),
            ],
        )
        .unwrap();
        let optimized = optimize(pipeline, PipelineOptimization::Collapse).unwrap();
        assert_eq!(kinds(&optimized), ["curves", "matrix", "curves"]);
    }

    #[test]
    fn adjacent_curve_sets_join_into_one() {
        let pipeline = Pipeline::new(
            3,
            3,
            vec![
                Stage::Curves(vec![gamma(0x0200); 3]),
                Stage::Curves(vec![gamma(0x0200); 3]),
            ],
        )
        .unwrap();
        // Two stages, both clamping: resampling would take over, so exercise the join alone.
        let joined = join_curves(pipeline).unwrap();
        assert_eq!(kinds(&joined), ["curves"]);
        // γ2 ∘ γ2 = γ4, up to the 4096-point table's chord error (worst here 2.2e-8, at
        // x = 0.5 where γ4's curvature peaks — the smooth, easy case; the toe of an inverse
        // gamma is the hard one the precision budget is sized for).
        for probe in [0.0, 0.25, 0.5, 1.0] {
            let out = eval(&joined, &[probe, probe, probe]);
            assert!(
                (out[0] - probe.powi(4)).abs() < 1e-7,
                "joined({probe}) = {out:?}"
            );
        }
    }

    #[test]
    fn joining_an_inverse_gamma_toe_stays_inside_the_stated_chord_error() {
        // The worst shape a joined curve meets: γ1/2.2's near-black toe, where the slope is
        // unbounded and the tabulation's chord error peaks in the very first interval. This
        // pins the number the crate's precision budget quotes (measured 8.2e-3 at x ≈ 1.2e-4)
        // so a wider table — or a lost identity shortcut — shows up as a change here.
        let toe = gamma(0x0074); // u8Fixed8 0x0074 = 0.453125 ≈ 1/2.2
        let joined = ToneCurve::joined(&toe, &identity_curve());
        // The identity leg keeps it exact...
        assert_eq!(joined.eval(0.5), toe.eval(0.5));
        // ...but tabulating against a real second curve does not.
        let tabulated = ToneCurve::joined(&toe, &gamma(0x0100));
        let mut worst = 0.0_f64;
        for i in 0..=40_000 {
            let x = f64::from(i) / 40_000.0;
            worst = worst.max((tabulated.eval(x) - toe.eval(x)).abs());
        }
        assert!((5e-3..2e-2).contains(&worst), "worst chord error {worst}");
    }

    #[test]
    fn joining_with_an_identity_curve_is_exact() {
        let g = gamma(0x0233);
        for joined in [
            ToneCurve::joined(&identity_curve(), &g),
            ToneCurve::joined(&g, &identity_curve()),
        ] {
            for i in 0..=256 {
                let x = f64::from(i) / 256.0;
                // Bit-exact, not merely close: the identity leg is dropped, not tabulated.
                assert_eq!(joined.eval(x), g.eval(x), "at {x}");
            }
        }
    }

    #[test]
    fn grid_points_follow_the_lcms2_default_rule() {
        assert_eq!(reasonable_grid_points(1), 33);
        assert_eq!(reasonable_grid_points(3), 33);
        assert_eq!(reasonable_grid_points(4), 17);
        assert_eq!(reasonable_grid_points(5), 7);
    }

    #[test]
    fn resampling_reproduces_the_pipeline_at_its_grid_nodes() {
        let pipeline = Pipeline::new(
            3,
            3,
            vec![
                Stage::Curves(vec![gamma(0x0233); 3]),
                dyadic(),
                Stage::Clamp { channels: 3 },
            ],
        )
        .unwrap();
        let optimized = optimize(pipeline.clone(), PipelineOptimization::Precalculate).unwrap();
        let [Stage::Clut(table)] = optimized.stages() else {
            panic!("a device-cube chain resamples into one CLUT");
        };
        assert_eq!(table.grid_points(), [33, 33, 33]);
        // At a node the interpolation weights are 0/1, so the resampled value is the
        // pipeline's own — the sampling is exact there and only interpolates between.
        for node in [[0, 0, 0], [8, 16, 32], [32, 32, 32]] {
            let probe: Vec<f64> = node.iter().map(|&i| f64::from(i) / 32.0).collect();
            let expected = eval(&pipeline, &probe);
            let got = eval(&optimized, &probe);
            for (x, y) in expected.iter().zip(&got) {
                assert!(
                    (x - y).abs() < 1e-12,
                    "{expected:?} vs {got:?} at {probe:?}"
                );
            }
        }
    }

    #[test]
    fn resampling_declines_a_pipeline_entered_from_a_pcs() {
        // A decoded-Lab entry: the leading matrix does not confine the input to [0, 1], so a
        // grid over the unit cube would cover the wrong domain.
        let pipeline = Pipeline::new(
            3,
            3,
            vec![
                Stage::Matrix {
                    m: [
                        [0.01, 0.0, 0.0],
                        [0.0, 1.0 / 255.0, 0.0],
                        [0.0, 0.0, 1.0 / 255.0],
                    ],
                    offset: [0.0, 0.5, 0.5],
                },
                Stage::Curves(vec![gamma(0x0233); 3]),
            ],
        )
        .unwrap();
        let optimized = optimize(pipeline, PipelineOptimization::Precalculate).unwrap();
        assert_eq!(kinds(&optimized), ["matrix", "curves"]);
    }

    #[test]
    fn resampling_declines_a_pipeline_leaving_for_a_pcs() {
        let pipeline = Pipeline::new(
            3,
            3,
            vec![Stage::Curves(vec![gamma(0x0233); 3]), Stage::XyzToLab],
        )
        .unwrap();
        let optimized = optimize(pipeline, PipelineOptimization::Precalculate).unwrap();
        assert_eq!(kinds(&optimized), ["curves", "xyz2lab"]);
    }

    #[test]
    fn resampling_declines_a_single_stage_chain() {
        let pipeline = Pipeline::new(3, 3, vec![Stage::Curves(vec![gamma(0x0233); 3])]).unwrap();
        let optimized = optimize(pipeline, PipelineOptimization::Precalculate).unwrap();
        assert_eq!(kinds(&optimized), ["curves"]);
    }

    #[test]
    fn resampling_declines_a_grid_over_the_node_ceiling() {
        // 8 inputs at 7 nodes per axis is 5.8M nodes — past MAX_RESAMPLED_NODES.
        let pipeline = Pipeline::new(
            8,
            8,
            vec![
                Stage::Clamp { channels: 8 },
                Stage::Curves(vec![gamma(0x0233); 8]),
            ],
        )
        .unwrap();
        let optimized = optimize(pipeline, PipelineOptimization::Precalculate).unwrap();
        assert_eq!(kinds(&optimized), ["clamp", "curves"]);
        // Seven inputs (823543 nodes) still fits, so the ceiling is a real boundary and not
        // a blanket refusal above four channels.
        let smaller = Pipeline::new(
            7,
            7,
            vec![
                Stage::Clamp { channels: 7 },
                Stage::Curves(vec![gamma(0x0233); 7]),
            ],
        )
        .unwrap();
        let optimized = optimize(smaller, PipelineOptimization::Precalculate).unwrap();
        assert_eq!(kinds(&optimized), ["clut"]);
    }

    #[test]
    fn precalculate_includes_the_collapse_passes() {
        // The trailing XyzToLab blocks resampling, so what is left must still show folding.
        let pipeline = Pipeline::new(
            3,
            3,
            vec![
                Stage::Curves(vec![gamma(0x0233); 3]),
                dyadic(),
                Stage::Identity { channels: 3 },
                dyadic(),
                Stage::XyzToLab,
            ],
        )
        .unwrap();
        let optimized = optimize(pipeline, PipelineOptimization::Precalculate).unwrap();
        assert_eq!(kinds(&optimized), ["curves", "matrix", "xyz2lab"]);
    }
}
