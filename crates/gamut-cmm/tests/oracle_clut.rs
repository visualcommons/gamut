//! Differential tests of [`gamut_cmm::ClutTable`] against Little-CMS.
//!
//! Two oracle windows, per `tooling/lcms2-oracle`:
//!
//! - **Float pipeline** (`ClutPipeline`, `cmsPipelineEvalFloat` over a float CLUT stage) — the
//!   direct line to lcms2's float interpolators (`LinLerp1Dfloat`/`Eval1InputFloat`,
//!   `BilinearInterpFloat`, `TetrahedralInterpFloat`, `Eval4InputsFloat`…). lcms2 computes in
//!   `f32` and this crate in `f64` over the same 16-bit node data, so agreement is
//!   `f32`-rounding-tight (measured ≤ ~1e-6; bounds below).
//! - **Profile route** (`clut_probe_profile` + `cmsDoTransform`, `NOOPTIMIZE|NOCACHE`,
//!   `TYPE_*_DBL`) — end-to-end through a devicelink profile whose `A2B0` is identity curves →
//!   16-bit CLUT → identity curves. lcms2 evaluates profile-borne 16-bit CLUTs through its
//!   **fixed-point** interpolators even in double transforms (`EvaluateCLUTfloatIn16`
//!   quantizes to 16 bits), so this route is only 16-bit-tight (measured ≤ ~1e-4).
//!
//! There is deliberately **no** direct 3-D-multilinear-vs-lcms2 sweep: lcms2 reaches its
//! `TrilinearInterpFloat` only for Lab-indexed CLUTs (`ChangeInterpolationToTrilinear` at
//! profile-read time), and driving that path would route the comparison through Lab encodings
//! that confound the interpolator measurement. The multilinear path is instead cross-checked
//! against an independent naive 2ᴺ-corner-weight implementation in the crate's unit tests, and
//! its 1-D/2-D base cases (where lcms2's float routines *are* multilinear) are swept here.
//!
//! The two narrower precision tiers meet the same two windows **bit-exactly**, because each is
//! the oracle's own arithmetic: the profile route at `TYPE_*_16` *is* lcms2's fixed-point
//! interpolator over the table's codes, which [`NodePrecision::Fixed16`] transcribes, and the
//! float pipeline *is* lcms2's `f32` interpolator, which [`NodePrecision::F32`] runs. Both are
//! reached through the public route — a `[Clamp, Clut]` pipeline resampled at
//! `PrecalculateFixed16`/`PrecalculateF32` on exactly the grid the resampling pass picks (33
//! nodes up to three inputs, 17 for four), whose resampled nodes are therefore the parsed
//! table's own samples.

mod common;

use common::Lcg;
use gamut_cmm::{
    ClutInterpolation, ClutTable, NodePrecision, Pipeline, PipelineOptimization, Stage, Transform,
};
use gamut_icc::{Clut, ClutPrecision};
use lcms2_oracle::{
    ClutPipeline, FLAGS_NOCACHE, FLAGS_NOOPTIMIZE, INTENT_PERCEPTUAL, TYPE_CMYK_16, TYPE_CMYK_DBL,
    TYPE_GRAY_16, TYPE_GRAY_DBL, TYPE_RGB_16, TYPE_RGB_DBL, Transform as LcmsTransform,
    clut_probe_profile,
};

fn random_samples(seed: u64, n: usize) -> Vec<u16> {
    let mut lcg = Lcg::new(seed);
    (0..n).map(|_| lcg.next_u16()).collect()
}

fn u16_clut(grid: &[u8], out: u8, samples: Vec<u16>) -> Clut {
    Clut {
        grid_points: grid.to_vec(),
        output_channels: out,
        precision: ClutPrecision::U16,
        samples,
    }
}

/// Our side of a differential: a single-stage pipeline over the table (the only public
/// evaluation route), plus per-point eval.
fn clut_pipeline(table: ClutTable) -> Pipeline {
    let (n_in, n_out) = (table.input_channels(), table.output_channels());
    Pipeline::new(n_in, n_out, vec![Stage::Clut(table)]).unwrap()
}

fn eval_ours(pipeline: &Pipeline, input: &[f64]) -> Vec<f64> {
    let mut out = vec![0.0; usize::from(pipeline.output_channels())];
    pipeline.eval(input, &mut out).unwrap();
    out
}

/// The float-path oracle over the identical node data: lcms2's table holds
/// `sample as f32 / 65535.0`, ours `sample as f64 / 65535.0` (≤ 2⁻²⁵ apart per node).
fn oracle_float(grid: &[u8], samples: &[u16], out_ch: u32) -> ClutPipeline {
    let f32_samples: Vec<f32> = samples.iter().map(|&s| f32::from(s) / 65535.0).collect();
    ClutPipeline::new(grid, &f32_samples, out_ch)
}

/// Sweep inputs for a `dims`-dimensional differential: all 2ᴺ grid corners (exact 0.0/1.0 in
/// every combination — the exact-1.0 upper-index rule), per-axis node coordinates, near-tie
/// fractions around the diagonal seams (the tetrahedral branch cascade's order shows there if
/// anywhere), a NaN probe, and seeded random points slightly overshooting `[0, 1]`.
fn sweep_points(seed: u64, grid: &[u8], count: usize) -> Vec<Vec<f64>> {
    let dims = grid.len();
    let mut points: Vec<Vec<f64>> = Vec::new();
    // All 2^dims corners (exact-0/exact-1 in every combination).
    for corner in 0..(1_usize << dims) {
        points.push(
            (0..dims)
                .map(|d| if corner & (1 << d) != 0 { 1.0 } else { 0.0 })
                .collect(),
        );
    }
    // Per-axis node coordinates with the other axes held at an interior value.
    for (d, &n) in grid.iter().enumerate() {
        for i in 0..usize::from(n) {
            let mut p = vec![0.37_f64; dims];
            p[d] = i as f64 / f64::from(n - 1).max(1.0);
            points.push(p);
        }
    }
    // Near-tie fractions around the diagonal seams (rx ≈ ry ≈ rz …): the branch cascade's
    // order shows here if anywhere.
    for base in [0.3_f64, 0.5, 0.62] {
        for eps in [0.0_f64, 1e-7, 1e-4] {
            let mut p = vec![base; dims];
            if dims > 1 {
                p[1] = base + eps;
            }
            if dims > 2 {
                p[2] = base - eps;
            }
            points.push(p);
        }
    }
    // NaN clamps to 0.0 on both sides (lcms2 fclamp).
    let mut nan_point = vec![0.5_f64; dims];
    nan_point[0] = f64::NAN;
    points.push(nan_point);
    // Seeded random fill to `count`.
    let mut lcg = Lcg::new(seed);
    while points.len() < count {
        points.push((0..dims).map(|_| lcg.next_unit_with_overshoot()).collect());
    }
    points
}

/// Runs the float-path differential over a grid shape and returns the worst absolute
/// difference across all sweep points and output channels.
fn worst_vs_float_oracle(grid: &[u8], out_ch: u8, seed: u64, count: usize) -> f64 {
    let nodes: usize = grid.iter().map(|&n| usize::from(n)).product();
    let samples = random_samples(seed, nodes * usize::from(out_ch));
    let table = ClutTable::new(&u16_clut(grid, out_ch, samples.clone())).unwrap();
    let ours = clut_pipeline(table);
    let oracle = oracle_float(grid, &samples, u32::from(out_ch));
    let mut worst = 0.0_f64;
    for point in sweep_points(seed.wrapping_add(1), grid, count) {
        let got = eval_ours(&ours, &point);
        let f32_point: Vec<f32> = point.iter().map(|&v| v as f32).collect();
        let want = oracle.eval(&f32_point);
        for ch in 0..usize::from(out_ch) {
            worst = worst.max((got[ch] - f64::from(want[ch])).abs());
        }
    }
    worst
}

#[test]
fn tetrahedral_3d_matches_lcms2_float_path() {
    lcms2_oracle::set_quiet_log_handler();
    // Mixed axis sizes pin the per-axis stride ordering; ≥ 500 points per the sweep contract.
    let worst = worst_vs_float_oracle(&[5, 4, 3], 3, 42, 520);
    // lcms2 computes in f32 over ~[0,1] values: a few f32 ulps. Measured 1.5e-7.
    assert!(
        worst < 1e-6,
        "3-D tetrahedral: worst |ours − lcms2| = {worst:e}"
    );
}

#[test]
fn tetrahedral_4d_recursion_matches_lcms2_eval4inputs() {
    lcms2_oracle::set_quiet_log_handler();
    // lcms2's Eval4InputsFloat = outermost-axis slice + tetrahedral base + linear blend —
    // exactly our Tetrahedral-mode recursion, so the comparison is like-for-like and tight.
    let worst = worst_vs_float_oracle(&[3, 4, 3, 2], 3, 77, 320);
    // Measured 1.2e-7.
    assert!(
        worst < 1e-6,
        "4-D tetrahedral: worst |ours − lcms2| = {worst:e}"
    );
}

#[test]
fn one_and_two_d_multilinear_match_lcms2_float_path() {
    lcms2_oracle::set_quiet_log_handler();
    // 1-D single-output (lcms2 LinLerp1Dfloat) and multi-output (Eval1InputFloat), plus 2-D
    // (BilinearInterpFloat): lcms2's float routines below 3-D *are* the multilinear form.
    for (grid, out_ch, seed) in [
        (&[9_u8] as &[u8], 1_u8, 5_u64),
        (&[17], 2, 6),
        (&[4, 5], 2, 7),
    ] {
        let worst = worst_vs_float_oracle(grid, out_ch, seed, 200);
        // Measured ≤ 1.9e-7 across the three shapes.
        assert!(
            worst < 1e-6,
            "{}-D multilinear: worst |ours − lcms2| = {worst:e}",
            grid.len()
        );
    }
}

/// Profile-route differential: our table vs `cmsDoTransform` over a devicelink probe profile.
/// Returns the worst absolute difference. `input`/`output` pair an lcms2 `TYPE_*_DBL` format
/// code with the scale from normalized channel values to that format's range (1.0 for
/// GRAY/RGB, 100.0 for CMYK ink percentages).
fn worst_vs_probe_profile(
    grid: &[u8],
    out_ch: u8,
    input: (u32, f64),
    output: (u32, f64),
    seed: u64,
    count: usize,
) -> f64 {
    let ((in_format, in_scale), (out_format, out_scale)) = (input, output);
    let nodes: usize = grid.iter().map(|&n| usize::from(n)).product();
    let samples = random_samples(seed, nodes * usize::from(out_ch));
    let table = ClutTable::new(&u16_clut(grid, out_ch, samples.clone())).unwrap();
    let ours = clut_pipeline(table);
    let probe = clut_probe_profile(grid, &samples, u32::from(out_ch));
    let transform = LcmsTransform::devicelink(
        &probe,
        in_format,
        out_format,
        INTENT_PERCEPTUAL,
        FLAGS_NOCACHE | FLAGS_NOOPTIMIZE,
    );
    let mut worst = 0.0_f64;
    for point in sweep_points(seed.wrapping_add(1), grid, count) {
        if point.iter().any(|v| v.is_nan()) {
            // The double formatters have no NaN contract; NaN is covered on the float path.
            continue;
        }
        let got = eval_ours(&ours, &point);
        let scaled: Vec<f64> = point.iter().map(|&v| v * in_scale).collect();
        let want = transform.apply_f64(&scaled, 1, usize::from(out_ch));
        for ch in 0..usize::from(out_ch) {
            worst = worst.max((got[ch] - want[ch] / out_scale).abs());
        }
    }
    worst
}

#[test]
fn profile_borne_3d_clut_matches_within_16bit_quantization() {
    lcms2_oracle::set_quiet_log_handler();
    let worst = worst_vs_probe_profile(
        &[4, 4, 4],
        3,
        (TYPE_RGB_DBL, 1.0),
        (TYPE_RGB_DBL, 1.0),
        9,
        220,
    );
    // Fixed-point route: 16-bit input snap (0.5/65535 · per-axis slope up to Domain) plus
    // S15.16 rounding. Measured 4.7e-5.
    assert!(worst < 5e-4, "3-D probe profile: worst = {worst:e}");
}

#[test]
fn profile_borne_1d_gray_clut_matches_within_16bit_quantization() {
    lcms2_oracle::set_quiet_log_handler();
    let worst = worst_vs_probe_profile(
        &[17],
        1,
        (TYPE_GRAY_DBL, 1.0),
        (TYPE_GRAY_DBL, 1.0),
        21,
        150,
    );
    // Measured 6.3e-5 (16 cells: per-axis slope up to Domain = 16 amplifies the input snap).
    assert!(worst < 5e-4, "1-D probe profile: worst = {worst:e}");
}

#[test]
fn profile_borne_4d_cmyk_clut_matches_within_16bit_quantization() {
    lcms2_oracle::set_quiet_log_handler();
    // TYPE_CMYK_DBL carries ink percentages 0..100 on both ends (dossier §10); scale in and
    // out so the comparison stays in normalized space.
    let worst = worst_vs_probe_profile(
        &[3, 3, 3, 3],
        3,
        (TYPE_CMYK_DBL, 100.0),
        (TYPE_RGB_DBL, 1.0),
        33,
        200,
    );
    // Measured 2.7e-5.
    assert!(worst < 5e-4, "4-D probe profile: worst = {worst:e}");
}

#[test]
fn forced_multilinear_3d_still_matches_lcms2_at_nodes() {
    lcms2_oracle::set_quiet_log_handler();
    // Multilinear and tetrahedral coincide exactly on grid nodes, so node coordinates are the
    // one place the forced-multilinear table can be pinned against the (tetrahedral) float
    // oracle — the interior is covered by the naive cross-check in the unit tests.
    let grid: &[u8] = &[3, 3, 3];
    let samples = random_samples(64, 27 * 2);
    let clut = u16_clut(grid, 2, samples.clone());
    let table = ClutTable::with_interpolation(&clut, ClutInterpolation::Multilinear).unwrap();
    let ours = clut_pipeline(table);
    let oracle = oracle_float(grid, &samples, 2);
    for x in [0.0_f32, 0.5, 1.0] {
        for y in [0.0_f32, 0.5, 1.0] {
            for z in [0.0_f32, 0.5, 1.0] {
                let got = eval_ours(&ours, &[f64::from(x), f64::from(y), f64::from(z)]);
                let want = oracle.eval(&[x, y, z]);
                for ch in 0..2 {
                    assert!(
                        (got[ch] - f64::from(want[ch])).abs() < 1e-6,
                        "node ({x}, {y}, {z}) ch {ch}: {} vs {}",
                        got[ch],
                        want[ch]
                    );
                }
            }
        }
    }
}

// ---- precision tiers ----------------------------------------------------------------------

/// The resampling pass's grid for `inputs` channels (lcms2's
/// `_cmsReasonableGridpointsByColorspace` default arm): a parsed table on exactly this grid
/// resamples onto its own nodes.
fn resampled_grid(inputs: usize) -> Vec<u8> {
    vec![if inputs == 4 { 17 } else { 33 }; inputs]
}

/// `[Clamp, Clut(samples)]` resampled at `level`, asserting the pass produced the single tier
/// table this suite measures (and not, say, a declined resampling).
fn resampled_tier(
    samples: &[u16],
    inputs: usize,
    outputs: u8,
    level: PipelineOptimization,
    precision: NodePrecision,
) -> Pipeline {
    let grid = resampled_grid(inputs);
    let table = ClutTable::new(&u16_clut(&grid, outputs, samples.to_vec())).unwrap();
    let n_in = u8::try_from(inputs).unwrap();
    let pipeline = Pipeline::new(
        n_in,
        outputs,
        vec![Stage::Clamp { channels: n_in }, Stage::Clut(table)],
    )
    .unwrap()
    .optimized(level)
    .unwrap();
    match pipeline.stages() {
        [Stage::Clut(resampled)] => {
            assert_eq!(resampled.grid_points(), grid.as_slice());
            assert_eq!(resampled.node_precision(), precision);
        }
        other => panic!("resampling declined: {other:?}"),
    }
    pipeline
}

/// 16-bit sweep inputs: every corner, the half-scale code, and seeded codes — a fifth of the
/// axes drawn equal to another (fraction ties) and a fifth at full scale (the upper-node rule).
fn sweep16(seed: u64, dims: usize, count: usize) -> Vec<u16> {
    let mut codes = Vec::new();
    for corner in 0..(1_usize << dims) {
        codes.extend((0..dims).map(|d| if corner & (1 << d) != 0 { u16::MAX } else { 0 }));
    }
    codes.extend(std::iter::repeat_n(0x8000, dims));
    let mut lcg = Lcg::new(seed);
    for _ in 0..count {
        let shared = lcg.next_u16();
        codes.extend((0..dims).map(|_| match lcg.next_u32() % 5 {
            0 => shared,
            1 => u16::MAX,
            _ => lcg.next_u16(),
        }));
    }
    codes
}

/// lcms2's 16-bit format code for a probe space.
fn format16(channels: usize) -> u32 {
    match channels {
        1 => TYPE_GRAY_16,
        3 => TYPE_RGB_16,
        _ => TYPE_CMYK_16,
    }
}

#[test]
fn fixed16_tier_is_bit_exact_against_lcms2_integer_interpolators() {
    lcms2_oracle::set_quiet_log_handler();
    // (inputs, outputs): lcms2's Eval1Input (1→1 is LinLerp1D), TetrahedralInterp16 and
    // Eval4Inputs, at the probe spaces lcms2's 16-bit formatters address (gray/RGB/CMYK).
    for (inputs, outputs, seed) in [
        (1_usize, 1_u8, 0x16_01_u64),
        (1, 3, 0x16_13),
        (3, 3, 0x16_33),
        (3, 4, 0x16_34),
        (4, 3, 0x16_43),
        (4, 4, 0x16_44),
    ] {
        let grid = resampled_grid(inputs);
        let nodes: usize = grid.iter().map(|&n| usize::from(n)).product();
        // Half-range codes: lcms2 accumulates `TetrahedralInterp16`'s `Rest` and
        // `BilinearInterp16`'s `LERP` in `int`, which a cell spanning more than half the code
        // range can overflow (signed overflow, whose wrapped result is off by a code); this
        // crate accumulates in `i64` and is exact there. Smooth resampled transforms never
        // step that far within one cell, so the differential stays where both are defined.
        let samples: Vec<u16> = random_samples(seed, nodes * usize::from(outputs))
            .into_iter()
            .map(|s| s >> 1)
            .collect();
        let ours = resampled_tier(
            &samples,
            inputs,
            outputs,
            PipelineOptimization::PrecalculateFixed16,
            NodePrecision::Fixed16,
        );
        let probe = clut_probe_profile(&grid, &samples, u32::from(outputs));
        let oracle = LcmsTransform::devicelink(
            &probe,
            format16(inputs),
            format16(usize::from(outputs)),
            INTENT_PERCEPTUAL,
            FLAGS_NOCACHE | FLAGS_NOOPTIMIZE,
        );
        let src = sweep16(seed ^ 0xA5, inputs, 2000);
        let pixels = src.len() / inputs;
        let mut got = vec![0_u16; pixels * usize::from(outputs)];
        ours.transform_u16(&src, &mut got).unwrap();
        let want = oracle.apply_u16(&src, pixels, usize::from(outputs));
        let first_miss = got
            .chunks(usize::from(outputs))
            .zip(want.chunks(usize::from(outputs)))
            .position(|(g, w)| g != w);
        assert!(
            first_miss.is_none(),
            "{inputs}→{outputs}: pixel {:?} gives {:?}, lcms2 {:?}",
            first_miss.map(|p| &src[p * inputs..(p + 1) * inputs]),
            first_miss.map(|p| &got[p * usize::from(outputs)..(p + 1) * usize::from(outputs)]),
            first_miss.map(|p| &want[p * usize::from(outputs)..(p + 1) * usize::from(outputs)]),
        );
    }
}

#[test]
fn f32_tier_is_bit_exact_against_lcms2_float_interpolators() {
    lcms2_oracle::set_quiet_log_handler();
    // LinLerp1Dfloat / Eval1InputFloat, TetrahedralInterpFloat, Eval4InputsFloat.
    for (inputs, outputs, seed) in [
        (1_usize, 1_u8, 0xF32_01_u64),
        (1, 3, 0xF32_13),
        (3, 3, 0xF32_33),
        (4, 2, 0xF32_42),
    ] {
        let grid = resampled_grid(inputs);
        let nodes: usize = grid.iter().map(|&n| usize::from(n)).product();
        let samples = random_samples(seed, nodes * usize::from(outputs));
        let ours = resampled_tier(
            &samples,
            inputs,
            outputs,
            PipelineOptimization::PrecalculateF32,
            NodePrecision::F32,
        );
        // The oracle's nodes are the same values at f32: `(s / 65535) as f32`.
        let narrow: Vec<f32> = samples
            .iter()
            .map(|&s| (f64::from(s) / 65535.0) as f32)
            .collect();
        let oracle = ClutPipeline::new(&grid, &narrow, u32::from(outputs));
        for point in sweep_points(seed ^ 0x5A, &grid, 1500) {
            // Inputs the f32 tier receives exactly: it narrows `f64` to `f32` on entry.
            let narrow_point: Vec<f32> = point.iter().map(|&v| v as f32).collect();
            let wide_point: Vec<f64> = narrow_point.iter().map(|&v| f64::from(v)).collect();
            let got = eval_ours(&ours, &wide_point);
            let want: Vec<f64> = oracle
                .eval(&narrow_point)
                .into_iter()
                .map(f64::from)
                .collect();
            assert_eq!(got, want, "{inputs}→{outputs} at {narrow_point:?}");
        }
    }
}
