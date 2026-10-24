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
//!   battery the worst deviation is **7.06e-16 device units** — a few ulps of a sample, and
//!   below what the gate's ΔE₀₀ lens can even resolve (STATUS.md).
//! - **[`Precalculate`]** is the lossy tier, and deliberately the *same* construction lcms2's
//!   default path applies: a 4096-point joined curve table and a grid-33 (RGB) / grid-17
//!   (CMYK) / grid-7 (hifi) resampled CLUT. Its budget is therefore the conformance gate's
//!   **LOOSE** row — max ΔE₀₀ `< 1.0` for shaper pairs and `< 5.0` for LUT pairs (measured
//!   1.08e-1 / 6.35e-2, with drift guards at `< 3e-1` / `< 2e-1`) — because that row already
//!   measures exactly this approximation on the oracle's side (STATUS.md, "Conformance gate
//!   (P7)"). Matching lcms2's construction rather than inventing one also brings the
//!   *outputs* together: optimized-vs-lcms2-optimized measures 2.98e-3 ΔE₀₀ where
//!   unoptimized-vs-lcms2-optimized measures 3.92e-1.
//!
//! # Precision tiers
//!
//! [`PrecalculateF32`](PipelineOptimization::PrecalculateF32) and
//! [`PrecalculateFixed16`](PipelineOptimization::PrecalculateFixed16) run exactly the
//! [`Precalculate`] construction — same grid, same joined curves, same interpolation mode — and
//! store the resampled table at a narrower [`NodePrecision`]. Each is gated against
//! `Precalculate` itself, so the budget is the error the tier *adds*:
//!
//! - **`f32`** — nodes rounded to `f32` and interpolated at that width: lcms2's own float
//!   interpolators, bit for bit. Measured ≤ 1.04e-7 device units over the conformance battery
//!   (under two `f32` ulps of a sample), half the node memory.
//! - **16-bit fixed point** — nodes encoded to 16-bit codes and interpolated by transcriptions
//!   of lcms2's integer interpolators, bit for bit. Under the 8- and 16-bit buffer entry points
//!   ([`crate::Transform::transform_u8`]/[`crate::Transform::transform_u16`]) the whole
//!   evaluation is integer arithmetic; an 8-bit code widens with lcms2's `FROM_8_TO_16` and the
//!   result narrows with `FROM_16_TO_8`. Measured ≤ 2.88e-3 / 7.12e-3 ΔE₀₀ (shaper / LUT
//!   pairs) against `Precalculate`, and 3.29e-3 / 6.87e-3 against lcms2's default 16-bit path
//!   — where `Precalculate` itself lands. A quarter of the node memory.
//!
//! When the resampling declines (a PCS end, a single stage, the node ceiling), every tier
//! leaves the pipeline exactly as `Precalculate` does — there is no table to narrow — and the
//! integer entry points convert through `f64`.
//!
//! One deliberate divergence from lcms2: no white-point "scum dot" fixup is applied to the
//! resampled table (lcms2's `PatchLUT`, which its callers disable with
//! `cmsFLAGS_NOWHITEONWHITEFIXUP` — the conformance gate disables it too, because it would
//! dominate the metric at the device-white corner).
//!
//! [`Collapse`]: PipelineOptimization::Collapse
//! [`Precalculate`]: PipelineOptimization::Precalculate

use crate::clut::{ClutInterpolation, ClutTable, NodePrecision};
use crate::curve::ToneCurve;
use crate::error::Result;
use crate::pipeline::{MAX_CHANNELS, Pipeline, Stage};
use crate::transform::{check_buffers, narrow_u16};

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
    /// [`Precalculate`](Self::Precalculate) with the resampled CLUT stored and interpolated
    /// in **`f32`** ([`NodePrecision::F32`]) — lcms2's own float-interpolator width, at half
    /// the node memory. Its added error over `Precalculate` is `f32` rounding of the nodes and
    /// the interpolation (module docs, "Precision tiers"). It buys lcms2's float arithmetic and
    /// the memory, **not** speed: scalar `f32` costs what `f64` does, plus a conversion at each
    /// end, so it measures slightly *slower* than `Precalculate` (`STATUS.md`, "Throughput
    /// (#423)") — [`PrecalculateFixed16`](Self::PrecalculateFixed16) is the throughput tier.
    PrecalculateF32 = 3,
    /// [`Precalculate`](Self::Precalculate) with the resampled CLUT stored as **16-bit
    /// codes** and interpolated in 16.16 fixed point ([`NodePrecision::Fixed16`]) — the
    /// representation lcms2 runs under its 8- and 16-bit formatters, at a quarter of the node
    /// memory. On a pipeline that resamples, the 8- and 16-bit buffer entry points
    /// ([`crate::Transform::transform_u8`]/[`crate::Transform::transform_u16`], and so
    /// [`crate::transform_interleaved_u8`] and its siblings) run integer arithmetic end to
    /// end; the `f64` entry point quantizes to 16 bits on the way in (module docs, "Precision
    /// tiers").
    PrecalculateFixed16 = 4,
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
        PipelineOptimization::Precalculate => precalculate(pipeline, NodePrecision::F64),
        PipelineOptimization::PrecalculateF32 => precalculate(pipeline, NodePrecision::F32),
        PipelineOptimization::PrecalculateFixed16 => precalculate(pipeline, NodePrecision::Fixed16),
    }
}

/// The sampled-table levels: collapse, join curves, then resample at `precision`.
///
/// # Errors
///
/// As [`optimize`].
fn precalculate(pipeline: Pipeline, precision: NodePrecision) -> Result<Pipeline> {
    let joined = join_curves(collapse(pipeline)?)?;
    resample(joined, precision)
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
fn resample(pipeline: Pipeline, precision: NodePrecision) -> Result<Pipeline> {
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
    let table =
        ClutTable::from_samples(vec![grid; axes], outputs, samples, interpolation, precision)?;
    rebuild(&pipeline, vec![Stage::Clut(table)])
}

/// Entries of a matrix-shaper input table: one per 8-bit code (`MatShaper8Data::Shaper1R`).
const SHAPER_IN_ENTRIES: usize = 256;

/// Entries of a matrix-shaper output table: every Q1.14 value of `[0, 1]`, `0..=0x4000`
/// (`MatShaper8Data::Shaper2R`).
const SHAPER_OUT_ENTRIES: usize = 16_385;

/// One in Q1.14, `0x4000` — the scale of every matrix-shaper table entry and coefficient.
const ONE_Q14: i32 = 1 << 14;

/// The precomputed 8-bit matrix-shaper (lcms2's `MatShaper8Data`, `cmsopt.c`): a
/// shaper → 3×3 matrix → shaper chain as three 256-entry Q1.14 input tables, a Q1.14 matrix
/// and offset, and three 16 385-entry output tables already narrowed to 8 bits.
///
/// Built by [`mat_shaper_8`] and run only by an 8-bit entry point
/// ([`crate::IccTransform`]'s `transform_u8`): the input tables are indexed by the 8-bit code
/// itself, so there is no 16-bit or `f64` form of it. About 52 KB (3 KB of input tables, 48 KB
/// of output tables); lcms2 stores the output tables as 16-bit `FROM_8_TO_16(b)` words its
/// optimized 8-bit formatter narrows again, which this keeps as the byte `b` — the same result
/// at half the memory.
#[derive(Debug, Clone)]
pub(crate) struct MatShaper8 {
    /// Per channel, code `i` through the first shaper, in Q1.14 (`FillFirstShaper`).
    input: [[i32; SHAPER_IN_ENTRIES]; 3],
    /// The matrix in Q1.14, row-major (`DOUBLE_TO_1FIXED14` of each coefficient).
    matrix: [[i32; 3]; 3],
    /// The offset in Q1.14.
    offset: [i32; 3],
    /// Per channel, Q1.14 value `i` through the second shaper, as the 8-bit code lcms2's
    /// 8-bit-output `FillSecondShaper` stores.
    output: [Box<[u8; SHAPER_OUT_ENTRIES]>; 3],
}

/// lcms2's `DOUBLE_TO_1FIXED14`: `floor(x · 16384 + 0.5)`. `as` saturates an out-of-range
/// coefficient, which [`fits_q14_accumulator`] then refuses.
fn to_q14(x: f64) -> i32 {
    #[expect(
        clippy::cast_possible_truncation,
        reason = "saturating by design; the accumulator guard refuses a saturated coefficient"
    )]
    {
        (x * f64::from(ONE_Q14) + 0.5).floor() as i32
    }
}

/// lcms2's `_cmsQuickFloor` (`lcms2_internal.h`, default build — no
/// `CMS_DONT_USE_FAST_FLOOR`): adds `1.5 · 2³⁶`, which rounds `val` to the nearest `2⁻¹⁶`,
/// then takes the integer part of that 16.16 value. So it is `floor` except within `2⁻¹⁷` below
/// an integer, where it rounds up — the difference that makes a plain `floor` disagree with
/// lcms2 on a few output-table entries.
fn quick_floor(val: f64) -> i32 {
    let magic = 68_719_476_736.0 * 1.5;
    // The low 32 bits of the sum's mantissa are `val` in 16.16 fixed point.
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap,
        reason = "the low word reinterpreted as a signed 16.16 value, as lcms2's union does"
    )]
    let fixed = (val + magic).to_bits() as u32 as i32;
    fixed >> 16
}

/// lcms2's `_cmsQuickSaturateWord(v · 65535)` for the `v ∈ [0, 1]` a second shaper produces:
/// `_cmsQuickFloorWord(v · 65535 + 0.5)`.
///
/// lcms2's two saturation guards are dropped rather than transcribed, because on this domain
/// neither can change a result: the operand is at least `0.5`, so `<= 0` never holds, and at
/// the top (`≥ 65535`, reached only by `v` within `2⁻¹⁷` of 1) `_cmsQuickFloorWord`'s own
/// 16-bit wrap already yields `0xFFFF` — the 16.16 value `32768` reads back as `-32768`, and
/// `-32768 + 32767` truncates to `0xFFFF`.
fn quick_saturate_word_unit(v: f64) -> u16 {
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "lcms2's `(cmsUInt16Number)` truncation, which the top end relies on"
    )]
    {
        (quick_floor(v * 65535.0 + 0.5 - 32767.0) + 32767) as u16
    }
}

/// lcms2's `FillFirstShaper`: code `i` through `curve` into Q1.14, evaluated at lcms2's
/// widths — the code as the `f32` `i / 255`, the curve's result rounded to `f32`
/// (`cmsEvalToneCurveFloat`). lcms2's `0x7fffffff` arm for a result `≥ 8.0` is absent: a
/// [`ToneCurve`] clamps its output to `[0, 1]`, so every entry is in `0..=0x4000`.
fn first_shaper(curve: &ToneCurve) -> [i32; SHAPER_IN_ENTRIES] {
    let mut table = [0; SHAPER_IN_ENTRIES];
    for (code, entry) in (0_u8..=u8::MAX).zip(table.iter_mut()) {
        #[expect(
            clippy::cast_possible_truncation,
            reason = "lcms2 evaluates the curve at the f32 code"
        )]
        let x = (f64::from(code) / 255.0) as f32;
        #[expect(
            clippy::cast_possible_truncation,
            reason = "cmsEvalToneCurveFloat returns f32"
        )]
        let y = curve.eval(f64::from(x)) as f32;
        *entry = to_q14(f64::from(y));
    }
    table
}

/// lcms2's 8-bit-output `FillSecondShaper`: Q1.14 value `i` (exact in `f32`) through `curve`,
/// rounded to `f32`, to a 16-bit word, and narrowed with `FROM_16_TO_8`. lcms2's clamp of the
/// curve's result to `[0, 1]` is absent because [`ToneCurve::eval`] already guarantees it.
fn second_shaper(curve: &ToneCurve) -> Box<[u8; SHAPER_OUT_ENTRIES]> {
    let mut table = Box::new([0; SHAPER_OUT_ENTRIES]);
    for (step, entry) in (0_u16..).zip(table.iter_mut()) {
        #[expect(
            clippy::cast_possible_truncation,
            reason = "cmsEvalToneCurveFloat returns f32"
        )]
        let y = curve.eval(f64::from(step) / f64::from(ONE_Q14)) as f32;
        *entry = narrow_u16(quick_saturate_word_unit(f64::from(y)));
    }
    table
}

/// Whether every row's worst-case Q1.14 accumulation — each coefficient's magnitude times the
/// largest input-table entry (`0x4000`), plus the offset and the `0x2000` rounding term —
/// stays inside `i32`.
///
/// lcms2 evaluates the sum in `int` with no such check, so a matrix whose row magnitudes sum
/// past about 8 (no realistic colorant pair; sRGB ↔ Display P3 sums under 1.3) overflows there,
/// which C leaves undefined. The pass declines such a matrix, so wherever it runs the `i32`
/// arithmetic is exactly lcms2's.
fn fits_q14_accumulator(matrix: &[[i32; 3]; 3], offset: &[i32; 3]) -> bool {
    matrix.iter().zip(offset).all(|(row, &off)| {
        let reach: i64 = row.iter().map(|&q| i64::from(q).abs()).sum();
        reach * i64::from(ONE_Q14) + i64::from(off).abs() + 0x2000 <= i64::from(i32::MAX)
    })
}

/// lcms2's `_cmsMAT3isIdentity`: every coefficient within `1/65535` of the identity's.
fn is_near_identity(m: &[[f64; 3]; 3]) -> bool {
    m.iter().enumerate().all(|(r, row)| {
        row.iter().enumerate().all(|(c, &v)| {
            let identity = if r == c { 1.0 } else { 0.0 };
            (v - identity).abs() < 1.0 / 65535.0
        })
    })
}

/// lcms2's `OptimizeMatrixShaper` (`cmsopt.c`) for an 8-bit RGB → RGB transform: when the
/// collapsed chain ([`collapse`]) is exactly curves → 3×3 matrix → curves, precompute it as a
/// [`MatShaper8`]. `None` — the transform keeps its other paths — when:
///
/// - the collapsed chain has any other shape (a CLUT, a PCS conversion, a Lab bridge, a
///   channel-count change, curves that are not separated by exactly one matrix);
/// - the matrix is within `1/65535` of the identity and the offset is zero: lcms2 drops such a
///   matrix and joins the curves instead (`_cmsMAT3isIdentity && Offset == NULL`);
/// - the Q1.14 accumulation could overflow `i32` ([`fits_q14_accumulator`]).
///
/// Where lcms2 also selects this pass, the result is lcms2's to the bit for parametric and
/// gamma curves; lcms2 evaluates a *sampled* curve through its 16-bit table interpolation,
/// which [`ToneCurve`] does in `f64`, so a profile with `curveType` tables can land a code away
/// at the entries where the two round differently. Two places select the pass where lcms2 does
/// not, both within its accuracy: a seam adjustment (absolute intent, black-point
/// compensation), which lcms2 keeps as a separate offset-carrying matrix its `_MultiplyMatrix`
/// refuses to fold, and a matrix [`collapse`] folds that lcms2's `PreOptimize` would too but
/// that sits beside a stage lcms2 removes first.
///
/// # Errors
///
/// As [`optimize`].
pub(crate) fn mat_shaper_8(pipeline: &Pipeline) -> Result<Option<MatShaper8>> {
    let collapsed = collapse(pipeline.clone())?;
    let [
        Stage::Curves(first),
        Stage::Matrix { m, offset },
        Stage::Curves(second),
    ] = collapsed.stages()
    else {
        return Ok(None);
    };
    if is_near_identity(m) && offset.iter().all(|&o| o == 0.0) {
        return Ok(None);
    }
    let matrix = m.map(|row| row.map(to_q14));
    let offset = offset.map(to_q14);
    if !fits_q14_accumulator(&matrix, &offset) {
        return Ok(None);
    }
    // A 3×3 matrix fixes both curve sets at three curves (the pipeline's validated seams).
    let mut input = [[0; SHAPER_IN_ENTRIES]; 3];
    for (table, curve) in input.iter_mut().zip(first) {
        *table = first_shaper(curve);
    }
    let [r, g, b] = [0, 1, 2].map(|channel| second_shaper(&second[channel]));
    Ok(Some(MatShaper8 {
        input,
        matrix,
        offset,
        output: [r, g, b],
    }))
}

impl MatShaper8 {
    /// lcms2's `MatShaperEval16` over one 8-bit pixel: the input tables, the matrix in Q1.14
    /// with `0x2000` rounding, a clip to `0..=0x4000`, and the output tables.
    fn eval(&self, pixel: [u8; 3]) -> [u8; 3] {
        let linear: [i32; 3] = [0, 1, 2].map(|c| self.input[c][usize::from(pixel[c])]);
        [0, 1, 2].map(|c| {
            let row = self.matrix[c];
            let sum = row[0] * linear[0] + row[1] * linear[1] + row[2] * linear[2];
            let value = (sum + self.offset[c] + 0x2000) >> 14;
            // Clipped to 0..=0x4000, so the cast is exact and the index in bounds.
            #[expect(clippy::cast_sign_loss, reason = "clamped non-negative")]
            let index = value.clamp(0, ONE_Q14) as usize;
            self.output[c][index]
        })
    }

    /// Runs the matrix-shaper over interleaved RGB bytes.
    ///
    /// # Errors
    ///
    /// [`crate::CmmError::BufferLength`] under the [`crate::Transform`] buffer contract at three
    /// channels in and out.
    pub(crate) fn transform(&self, src: &[u8], dst: &mut [u8]) -> Result<()> {
        check_buffers(3, 3, src.len(), dst.len())?;
        for (pixel, out) in src
            .as_chunks::<3>()
            .0
            .iter()
            .zip(dst.as_chunks_mut::<3>().0)
        {
            *out = self.eval(*pixel);
        }
        Ok(())
    }
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
    fn precision_levels_resample_at_their_tier_with_the_same_construction() {
        let pipeline = Pipeline::new(
            4,
            3,
            vec![
                Stage::Curves(vec![gamma(0x0233); 4]),
                Stage::MatrixN {
                    rows: 3,
                    cols: 4,
                    m: vec![
                        0.5, 0.25, 0.0, 0.125, 0.0, 0.5, 0.25, 0.125, 0.25, 0.0, 0.5, 0.125,
                    ],
                    offset: vec![0.0; 3],
                },
                Stage::Clamp { channels: 3 },
            ],
        )
        .unwrap();
        for (level, precision) in [
            (PipelineOptimization::Precalculate, NodePrecision::F64),
            (PipelineOptimization::PrecalculateF32, NodePrecision::F32),
            (
                PipelineOptimization::PrecalculateFixed16,
                NodePrecision::Fixed16,
            ),
        ] {
            let optimized = optimize(pipeline.clone(), level).unwrap();
            let [Stage::Clut(table)] = optimized.stages() else {
                panic!("{level:?}: a device-cube chain resamples into one CLUT");
            };
            // The tiers differ in representation only: lcms2's grid and interpolation mode.
            assert_eq!(table.grid_points(), [17, 17, 17, 17], "{level:?}");
            assert_eq!(table.interpolation(), ClutInterpolation::Tetrahedral);
            assert_eq!(table.node_precision(), precision, "{level:?}");
            // At a node every tier returns the pipeline's value to its own precision.
            let probe = [0.25, 0.5, 0.75, 1.0];
            let (expected, got) = (eval(&pipeline, &probe), eval(&optimized, &probe));
            let tolerance = match precision {
                NodePrecision::F64 => 1e-12,
                NodePrecision::F32 => 1e-7,
                NodePrecision::Fixed16 => 0.5 / 65535.0,
            };
            for (x, y) in expected.iter().zip(&got) {
                assert!(
                    (x - y).abs() <= tolerance,
                    "{level:?}: {expected:?} vs {got:?}"
                );
            }
        }
        // A declined resampling is declined at every tier: nothing is quantized.
        let pcs_exit = Pipeline::new(
            3,
            3,
            vec![Stage::Curves(vec![gamma(0x0233); 3]), Stage::XyzToLab],
        )
        .unwrap();
        for level in [
            PipelineOptimization::PrecalculateF32,
            PipelineOptimization::PrecalculateFixed16,
        ] {
            let optimized = optimize(pcs_exit.clone(), level).unwrap();
            assert_eq!(kinds(&optimized), ["curves", "xyz2lab"], "{level:?}");
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
