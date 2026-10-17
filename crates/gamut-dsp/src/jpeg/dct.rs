//! JPEG-1 8×8 forward and inverse discrete cosine transforms (ITU-T T.81 | ISO/IEC 10918-1
//! §A.3.3).
//!
//! [`fdct8x8`] and [`idct8x8`] are direct transcriptions of the *informative* ideal FDCT / IDCT
//! equations of T.81 §A.3.3:
//!
//! ```text
//! FDCT:  S_vu = 1/4 · C_u C_v · Σ_{x=0}^{7} Σ_{y=0}^{7} s_yx · cos((2x+1)uπ/16) · cos((2y+1)vπ/16)
//! IDCT:  s_yx = 1/4 · Σ_{u=0}^{7} Σ_{v=0}^{7} C_u C_v · S_vu · cos((2x+1)uπ/16) · cos((2y+1)vπ/16)
//! with   C_u, C_v = 1/√2 for u, v = 0, and 1 otherwise.
//! ```
//!
//! §A.3.3 flags these as *ideal* functional definitions that "cannot be represented with perfect
//! accuracy by any real implementation"; the normative accuracy requirements for a conforming
//! DCT live in T.83 (ISO/IEC 10918-2). This module targets clarity and spec-exactness rather than
//! speed — it evaluates the separable transform in `f64` and rounds once at the end. Encoder
//! performance tuning (fixed-point / fast-DCT kernels) is deliberately out of scope for issue #28.
//!
//! Both transforms are **separable**: the 2-D kernel is the product of two 1-D cosine kernels, so
//! `fdct8x8` transforms each row (spatial index `x` → frequency `u`) then each column (`y` → `v`),
//! and `idct8x8` inverts column-then-row. The row/column pass order does not affect the real-valued
//! result; the single rounding is applied only when writing the integer output.
//!
//! Sample orientation follows T.81 Figure A.4: `s_yx` is the sample at row `y`, column `x`, and
//! both blocks are stored in **raster (row-major, natural) order** — element `y·8 + x` for samples
//! and `v·8 + u` for coefficients. This is the *natural* order of §A.3.6, not the zig-zag order.

use std::f64::consts::{FRAC_1_SQRT_2, PI};

/// The T.81 §A.3.3 normalization scale `C_k`: `1/√2` at the DC index `k = 0`, and `1` for every AC
/// index `k = 1..=7`.
fn normalization(k: usize) -> f64 {
    if k == 0 { FRAC_1_SQRT_2 } else { 1.0 }
}

/// The DCT cosine basis `cos((2·i + 1)·k·π / 16)` for spatial index `i` and frequency index `k`,
/// both spanning `0..8`. Recomputed per call from [`f64::cos`] (deterministic; no allocation, no
/// precomputed table to drift out of sync with the spec).
fn cosine_table() -> [[f64; 8]; 8] {
    let mut table = [[0.0f64; 8]; 8];
    for (i, row) in table.iter_mut().enumerate() {
        for (k, cell) in row.iter_mut().enumerate() {
            *cell = (((2 * i + 1) * k) as f64 * PI / 16.0).cos();
        }
    }
    table
}

/// One 1-D 8-point forward DCT-II with the T.81 §A.3.3 normalization: `F_k = ½·C_k·Σ_i f_i·
/// cos((2i+1)kπ/16)`. Two of these (over `x` then `y`) compose the separable 2-D FDCT.
fn forward_1d(input: &[f64; 8], cos: &[[f64; 8]; 8]) -> [f64; 8] {
    let mut out = [0.0f64; 8];
    for (k, out_k) in out.iter_mut().enumerate() {
        let mut sum = 0.0;
        for (i, &sample) in input.iter().enumerate() {
            sum += sample * cos[i][k];
        }
        *out_k = 0.5 * normalization(k) * sum;
    }
    out
}

/// One 1-D 8-point inverse DCT-III with the T.81 §A.3.3 normalization: `f_i = ½·Σ_k C_k·F_k·
/// cos((2i+1)kπ/16)`. Two of these (over `v` then `u`) compose the separable 2-D IDCT.
fn inverse_1d(input: &[f64; 8], cos: &[[f64; 8]; 8]) -> [f64; 8] {
    let mut out = [0.0f64; 8];
    for (i, out_i) in out.iter_mut().enumerate() {
        let mut sum = 0.0;
        for (k, &coeff) in input.iter().enumerate() {
            sum += normalization(k) * coeff * cos[i][k];
        }
        *out_i = 0.5 * sum;
    }
    out
}

/// Rounds a transform result to the nearest integer, ties **away from zero** ([`f64::round`]).
///
/// The same rule is used by [`fdct8x8`] and [`idct8x8`] so the two directions are consistent. The
/// `as i32` cast saturates on out-of-range inputs (Rust's defined float→int behaviour), but a valid
/// 8-/12-bit block never approaches `i32::MAX`.
fn round_to_i32(value: f64) -> i32 {
    value.round() as i32
}

/// In-place 2-D **forward** DCT of an 8×8 block, per ITU-T T.81 | ISO/IEC 10918-1 §A.3.3.
///
/// `block` holds the 64 source samples in raster (row-major / natural) order — element `y·8 + x`
/// is the sample `s_yx` at row `y`, column `x` (T.81 Figure A.4). On return `block` holds the
/// unquantized DCT coefficients, likewise in raster order (`v·8 + u` is `S_vu`), each rounded to
/// the nearest integer with ties away from zero.
///
/// # Input domain
/// The samples must already be **level-shifted** to a signed representation per §A.3.1 (subtract
/// `2^(P-1)`): for 8-bit precision (`P = 8`) the domain is `-128..=127`; for 12-bit (`P = 12`) it
/// is `-2048..=2047`. Level shifting is the caller's responsibility — this kernel is pure DCT.
///
/// # Output range
/// The DC term `S_00` is `8×` the block mean, so `|coeff| ≤ 8 · max|sample|`: at most `±1024` for
/// 8-bit input and `±16384` for 12-bit input; every AC term is smaller in magnitude.
///
/// # Examples
/// ```
/// use gamut_dsp::jpeg::fdct8x8;
///
/// // A flat (constant) block collapses to a pure DC coefficient: S_00 = 8·c, all AC = 0.
/// let mut block = [10i32; 64];
/// fdct8x8(&mut block);
/// assert_eq!(block[0], 80);
/// assert!(block[1..].iter().all(|&c| c == 0));
/// ```
pub fn fdct8x8(block: &mut [i32; 64]) {
    let cos = cosine_table();
    let mut work = [0.0f64; 64];

    // Row pass: 1-D forward DCT along x within each row y → intermediate T[y][u].
    for y in 0..8 {
        let mut row = [0.0f64; 8];
        for (x, cell) in row.iter_mut().enumerate() {
            *cell = f64::from(block[y * 8 + x]);
        }
        let transformed = forward_1d(&row, &cos);
        for (u, &value) in transformed.iter().enumerate() {
            work[y * 8 + u] = value;
        }
    }

    // Column pass: 1-D forward DCT along y for each frequency column u → S[v][u].
    for u in 0..8 {
        let mut col = [0.0f64; 8];
        for (y, cell) in col.iter_mut().enumerate() {
            *cell = work[y * 8 + u];
        }
        let transformed = forward_1d(&col, &cos);
        for (v, &value) in transformed.iter().enumerate() {
            block[v * 8 + u] = round_to_i32(value);
        }
    }
}

/// In-place 2-D **inverse** DCT of an 8×8 block, per ITU-T T.81 | ISO/IEC 10918-1 §A.3.3.
///
/// `block` holds the 64 dequantized DCT coefficients in raster (row-major) order — element
/// `v·8 + u` is `S_vu`. On return `block` holds the reconstructed samples in raster order
/// (`y·8 + x` is `s_yx`), each rounded to the nearest integer with ties away from zero — the same
/// rounding rule as [`fdct8x8`].
///
/// # Output
/// The result is the **level-shifted** reconstruction: it is *not* clamped and the `2^(P-1)` level
/// shift is *not* added back. Restoring the unsigned sample range (add `2^(P-1)`, clamp to
/// `0..=2^P − 1`) is the decoder's job per §A.3.1, kept out of this pure kernel.
///
/// # Examples
/// ```
/// use gamut_dsp::jpeg::idct8x8;
///
/// // A pure DC coefficient reconstructs to a flat block: s_yx = S_00 / 8 everywhere.
/// let mut block = [0i32; 64];
/// block[0] = 80;
/// idct8x8(&mut block);
/// assert!(block.iter().all(|&s| s == 10));
/// ```
pub fn idct8x8(block: &mut [i32; 64]) {
    let cos = cosine_table();
    let mut work = [0.0f64; 64];

    // Column pass: 1-D inverse DCT along v for each frequency column u → intermediate P[y][u].
    for u in 0..8 {
        let mut col = [0.0f64; 8];
        for (v, cell) in col.iter_mut().enumerate() {
            *cell = f64::from(block[v * 8 + u]);
        }
        let transformed = inverse_1d(&col, &cos);
        for (y, &value) in transformed.iter().enumerate() {
            work[y * 8 + u] = value;
        }
    }

    // Row pass: 1-D inverse DCT along u within each row y → reconstructed s[y][x].
    for y in 0..8 {
        let mut row = [0.0f64; 8];
        for (u, cell) in row.iter_mut().enumerate() {
            *cell = work[y * 8 + u];
        }
        let transformed = inverse_1d(&row, &cos);
        for (x, &value) in transformed.iter().enumerate() {
            block[y * 8 + x] = round_to_i32(value);
        }
    }
}

/// Fractional bits of the fixed-point rotation constants (libjpeg's `CONST_BITS`).
const CONST_BITS: u32 = 13;

/// Extra fractional bits the column pass keeps for the row pass (libjpeg's 8-bit `PASS1_BITS`).
const PASS1_BITS: u32 = 2;

/// The LL&M rotation constants scaled by `2^CONST_BITS` and rounded, as `jidctint.c` tabulates
/// them (`FIX_0_298631336` …): each name is the real value it approximates.
const FIX_0_298631336: i64 = 2446;
const FIX_0_390180644: i64 = 3196;
const FIX_0_541196100: i64 = 4433;
const FIX_0_765366865: i64 = 6270;
const FIX_0_899976223: i64 = 7373;
const FIX_1_175875602: i64 = 9633;
const FIX_1_501321110: i64 = 12299;
const FIX_1_847759065: i64 = 15137;
const FIX_1_961570560: i64 = 16069;
const FIX_2_053119869: i64 = 16819;
const FIX_2_562915447: i64 = 20995;
const FIX_3_072711026: i64 = 25172;

/// `DESCALE(x, n)`: divides by `2^n`, rounding half up, and narrows to `i32`.
///
/// The narrowing wraps exactly as `jidctint.c`'s `(int)` cast does on an LP64 target; it can only
/// matter for coefficients no conformant 8-bit stream produces.
fn descale(x: i64, n: u32) -> i32 {
    ((x + (1 << (n - 1))) >> n) as i32
}

/// One 8-point Loeffler–Ligtenberg–Moschytz inverse DCT as `jidctint.c` evaluates it: the even
/// part (inputs 0, 2, 4, 6) and the odd part (inputs 1, 3, 5, 7), returning the eight outputs
/// before the pass's [`descale`]. Both passes of [`idct8x8_int`] run exactly this body; they differ
/// only in the shift applied afterwards.
fn llm_1d(s: [i64; 8]) -> [i64; 8] {
    // Even part: the rotator is √2·c6.
    let z1 = (s[2] + s[6]) * FIX_0_541196100;
    let tmp2 = z1 - s[6] * FIX_1_847759065;
    let tmp3 = z1 + s[2] * FIX_0_765366865;
    let tmp0 = (s[0] + s[4]) << CONST_BITS;
    let tmp1 = (s[0] - s[4]) << CONST_BITS;
    let (tmp10, tmp13) = (tmp0 + tmp3, tmp0 - tmp3);
    let (tmp11, tmp12) = (tmp1 + tmp2, tmp1 - tmp2);

    // Odd part (LL&M figure 8): i0..i3 are the inputs 7, 5, 3, 1.
    let (i0, i1, i2, i3) = (s[7], s[5], s[3], s[1]);
    let z5 = (i0 + i2 + i1 + i3) * FIX_1_175875602;
    let z1 = -(i0 + i3) * FIX_0_899976223;
    let z2 = -(i1 + i2) * FIX_2_562915447;
    let z3 = z5 - (i0 + i2) * FIX_1_961570560;
    let z4 = z5 - (i1 + i3) * FIX_0_390180644;
    let o0 = i0 * FIX_0_298631336 + z1 + z3;
    let o1 = i1 * FIX_2_053119869 + z2 + z4;
    let o2 = i2 * FIX_3_072711026 + z2 + z3;
    let o3 = i3 * FIX_1_501321110 + z1 + z4;

    [
        tmp10 + o3,
        tmp11 + o2,
        tmp12 + o1,
        tmp13 + o0,
        tmp13 - o0,
        tmp12 - o1,
        tmp11 - o2,
        tmp10 - o3,
    ]
}

/// In-place 2-D **inverse** DCT of an 8×8 block in 13-bit fixed point — the decoder's fast
/// kernel, bit-exact with libjpeg-turbo's accurate integer IDCT (`jpeg_idct_islow`, `jidctint.c`,
/// the Loeffler–Ligtenberg–Moschytz factorisation).
///
/// `block` holds the 64 dequantized coefficients in raster (row-major) order, as for
/// [`idct8x8`]; on return it holds the level-shifted reconstruction in raster order, likewise
/// neither clamped nor shifted back by `2^(P-1)` (§A.3.1 is the decoder's job). Unlike
/// [`idct8x8`] the samples are not the ideal §A.3.3 values rounded once: they are the
/// fixed-point approximation, which meets the IEEE 1180-1990 accuracy bounds (peak error 1,
/// per-sample mean square error ≤ 0.06) that the T.83 compliance tests build on. [`idct8x8`]
/// stays the reference this kernel is measured against.
///
/// # Bit exactness
/// For every block whose samples land in libjpeg's range-limit window (any block a conformant
/// 8-bit stream decodes to), the output plus 128, clamped to `0..=255`, is exactly the sample
/// `jpeg_idct_islow` writes. Outside that window libjpeg's masked range-limit table and a plain
/// clamp diverge; and coefficients beyond the 8-bit domain wrap in the column-pass narrowing as
/// libjpeg's `(int)` casts do. No input panics: the arithmetic is `i64` throughout.
///
/// # Sparsity
/// A column whose seven AC coefficients are zero, and a row of the intermediate whose AC terms
/// are zero, take `jidctint.c`'s shortcut, which gives the same value as the full butterfly.
/// A caller that already knows a block is DC-only (a decoder that saw an end-of-block straight
/// after the DC) can skip the kernel altogether with [`idct8x8_int_dc`].
///
/// # Examples
/// ```
/// use gamut_dsp::jpeg::{idct8x8_int, idct8x8_int_dc};
///
/// // A pure DC coefficient reconstructs to a flat block, and the DC shortcut agrees.
/// let mut block = [0i32; 64];
/// block[0] = 80;
/// idct8x8_int(&mut block);
/// assert!(block.iter().all(|&s| s == 10));
/// assert_eq!(idct8x8_int_dc(80), 10);
/// ```
pub fn idct8x8_int(block: &mut [i32; 64]) {
    let mut ws = [0i32; 64];

    // Pass 1: columns, keeping PASS1_BITS extra fractional bits.
    for u in 0..8 {
        if (1..8).all(|v| block[v * 8 + u] == 0) {
            let dc = block[u].wrapping_shl(PASS1_BITS);
            for v in 0..8 {
                ws[v * 8 + u] = dc;
            }
            continue;
        }
        let out = llm_1d(core::array::from_fn(|v| i64::from(block[v * 8 + u])));
        for (v, &value) in out.iter().enumerate() {
            ws[v * 8 + u] = descale(value, CONST_BITS - PASS1_BITS);
        }
    }

    // Pass 2: rows, removing the PASS1_BITS scaling and the factor of 8 the two passes carry.
    for (row, out_row) in ws
        .as_chunks::<8>()
        .0
        .iter()
        .zip(block.as_chunks_mut::<8>().0)
    {
        if row[1..].iter().all(|&c| c == 0) {
            out_row.fill(descale(i64::from(row[0]), PASS1_BITS + 3));
            continue;
        }
        let out = llm_1d(core::array::from_fn(|x| i64::from(row[x])));
        for (sample, &value) in out_row.iter_mut().zip(out.iter()) {
            *sample = descale(value, CONST_BITS + PASS1_BITS + 3);
        }
    }
}

/// The value every sample of [`idct8x8_int`] takes for a block whose only nonzero coefficient is
/// the dequantized DC term `dc`: the sparsity hint, for a caller that knows the 63 AC terms are
/// zero and need not build the block at all.
///
/// Level-shifted and unclamped like [`idct8x8_int`]'s output, and equal to it for every `dc`.
///
/// # Examples
/// ```
/// use gamut_dsp::jpeg::idct8x8_int_dc;
///
/// assert_eq!(idct8x8_int_dc(-1024), -128);
/// assert_eq!(idct8x8_int_dc(12), 2); // 1.5 rounds half up
/// ```
#[must_use]
pub fn idct8x8_int_dc(dc: i32) -> i32 {
    descale(i64::from(dc.wrapping_shl(PASS1_BITS)), PASS1_BITS + 3)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testrng::Lcg;

    /// Largest tolerated gap between an integer transform output and the ideal real-valued §A.3.3
    /// result. A correct nearest-integer rounding is within `0.5`; the `1e-6` slack absorbs the
    /// ~`1e-9` difference between the production kernel's separable summation order and the oracle's
    /// direct double sum (which only ever matters when the true value sits within an ULP of a `x.5`
    /// tie). Any mutation to an index, loop bound, or constant moves an output by ≫ 1 and trips this.
    const ROUND_TOL: f64 = 0.5 + 1e-6;

    /// Independent, self-contained O(64²) evaluation of the ideal (unrounded) §A.3.3 FDCT equation
    /// (double sum over `x, y`). Deliberately *not* separable — the oracle the production kernel's
    /// separable evaluation must round to.
    fn direct_fdct(block: &[i32; 64]) -> [f64; 64] {
        let mut out = [0.0f64; 64];
        for v in 0..8usize {
            for u in 0..8usize {
                let mut sum = 0.0f64;
                for x in 0..8usize {
                    for y in 0..8usize {
                        sum += f64::from(block[y * 8 + x])
                            * (((2 * x + 1) * u) as f64 * PI / 16.0).cos()
                            * (((2 * y + 1) * v) as f64 * PI / 16.0).cos();
                    }
                }
                out[v * 8 + u] = 0.25 * normalization(u) * normalization(v) * sum;
            }
        }
        out
    }

    /// Independent O(64²) evaluation of the ideal (unrounded) §A.3.3 IDCT equation, oracle for
    /// [`idct8x8`].
    fn direct_idct(block: &[i32; 64]) -> [f64; 64] {
        let mut out = [0.0f64; 64];
        for y in 0..8usize {
            for x in 0..8usize {
                let mut sum = 0.0f64;
                for u in 0..8usize {
                    for v in 0..8usize {
                        sum += normalization(u)
                            * normalization(v)
                            * f64::from(block[v * 8 + u])
                            * (((2 * x + 1) * u) as f64 * PI / 16.0).cos()
                            * (((2 * y + 1) * v) as f64 * PI / 16.0).cos();
                    }
                }
                out[y * 8 + x] = 0.25 * sum;
            }
        }
        out
    }

    /// Asserts every element of `got` is the correct nearest-integer rounding of `ideal`.
    fn assert_rounds(got: &[i32; 64], ideal: &[f64; 64], ctx: &str) {
        for (idx, (&g, &t)) in got.iter().zip(ideal.iter()).enumerate() {
            assert!(
                (f64::from(g) - t).abs() <= ROUND_TOL,
                "{ctx}: index {idx} got {g}, ideal {t}",
            );
        }
    }

    /// The workhorse: over a battery of deterministic pseudo-random blocks spanning the full 8-bit
    /// and 12-bit level-shifted domains, the separable [`fdct8x8`] must round the independent direct
    /// §A.3.3 double sum to the nearest integer. This pins index orientation (x/y ↔ u/v), the
    /// `¼·C_u·C_v` scale, and every cosine argument — a swapped index, off-by-one, or wrong constant
    /// diverges by far more than half a unit.
    #[test]
    fn fdct_matches_direct_formula() {
        let mut rng = Lcg::new(0x1234_5678_9abc_def0);
        for _ in 0..600 {
            let mut block = [0i32; 64];
            for s in &mut block {
                *s = rng.level_shifted_sample(8);
            }
            let mut got = block;
            fdct8x8(&mut got);
            assert_rounds(&got, &direct_fdct(&block), "fdct 8-bit");
        }
        for _ in 0..200 {
            let mut block = [0i32; 64];
            for s in &mut block {
                *s = rng.level_shifted_sample(12);
            }
            let mut got = block;
            fdct8x8(&mut got);
            assert_rounds(&got, &direct_fdct(&block), "fdct 12-bit");
        }
    }

    /// Same battery for the inverse: [`idct8x8`] must round the independent direct double sum.
    #[test]
    fn idct_matches_direct_formula() {
        let mut rng = Lcg::new(0x0fed_cba9_8765_4321);
        for _ in 0..600 {
            let mut coeffs = [0i32; 64];
            for c in &mut coeffs {
                *c = rng.level_shifted_sample(8);
            }
            let mut got = coeffs;
            idct8x8(&mut got);
            assert_rounds(&got, &direct_idct(&coeffs), "idct 8-bit");
        }
        for _ in 0..200 {
            let mut coeffs = [0i32; 64];
            for c in &mut coeffs {
                *c = rng.level_shifted_sample(12);
            }
            let mut got = coeffs;
            idct8x8(&mut got);
            assert_rounds(&got, &direct_idct(&coeffs), "idct 12-bit");
        }
    }

    /// Pins the **sign** of the private 1-D kernels. The 2-D transforms compose each 1-D kernel
    /// twice (rows then columns), so a sign flip inside `forward_1d`/`inverse_1d` cancels itself
    /// and is invisible to every 2-D test — only a direct 1-D check can catch it. A constant input
    /// `c` must give a *positive* DC term `8·c·½·(1/√2) = 2√2·c` (and the inverse must map that DC
    /// back to the positive constant), so a negated accumulator trips the sign assertion here.
    #[test]
    fn one_dimensional_kernels_have_positive_dc_sign() {
        let cos = cosine_table();
        let constant = [3.0f64; 8];
        let spectrum = forward_1d(&constant, &cos);
        let expected_dc = 8.0 * 3.0 * 0.5 * FRAC_1_SQRT_2;
        assert!(
            (spectrum[0] - expected_dc).abs() < 1e-12,
            "forward_1d DC: got {}, expected {expected_dc}",
            spectrum[0]
        );
        for (k, &ac) in spectrum.iter().enumerate().skip(1) {
            assert!(ac.abs() < 1e-12, "forward_1d AC[{k}] = {ac}, expected 0");
        }

        let mut dc_only = [0.0f64; 8];
        dc_only[0] = expected_dc;
        let reconstructed = inverse_1d(&dc_only, &cos);
        for (i, &s) in reconstructed.iter().enumerate() {
            assert!(
                (s - 3.0).abs() < 1e-12,
                "inverse_1d sample[{i}] = {s}, expected 3"
            );
        }
    }

    /// A constant block `c` is pure DC: `S_00 = 8·c` exactly, all 63 AC coefficients exactly zero.
    /// This pins the DC normalization (`C_0 = 1/√2`, the `¼` scale) and cosine orthogonality.
    #[test]
    fn fdct_constant_block_is_dc() {
        for c in [-128i32, -1, 0, 1, 42, 127] {
            let mut block = [c; 64];
            fdct8x8(&mut block);
            assert_eq!(block[0], 8 * c, "DC for constant {c}");
            assert!(block[1..].iter().all(|&x| x == 0), "AC not zero for {c}");
        }
    }

    /// The all-zero block maps to all zeros under both transforms.
    #[test]
    fn zero_block_is_fixed_point() {
        let mut block = [0i32; 64];
        fdct8x8(&mut block);
        assert_eq!(block, [0i32; 64]);
        idct8x8(&mut block);
        assert_eq!(block, [0i32; 64]);
    }

    /// A single DC coefficient `k` reconstructs to the flat block `k/8` under the IDCT — the inverse
    /// of `fdct_constant_block_is_dc`, pinning the inverse DC normalization and `¼` scale.
    #[test]
    fn idct_dc_only_is_constant() {
        for k in [-1024i32, -8, 0, 8, 80, 1024] {
            let mut block = [0i32; 64];
            block[0] = k;
            idct8x8(&mut block);
            assert!(block.iter().all(|&s| s == k / 8), "flat value for DC {k}");
        }
    }

    /// Orientation guard: a **purely vertical** pattern `s_yx = g(y)` (constant along each row) has
    /// no horizontal frequency content, so the FDCT must leave *only* the `u = 0` coefficient column
    /// nonzero. Symmetrically, a **purely horizontal** pattern leaves only the `v = 0` row nonzero.
    /// These zeros are exact (cosine orthogonality against a constant), so they pin the `x ↔ u`
    /// (horizontal) / `y ↔ v` (vertical) mapping deterministically — an x/y index swap moves the
    /// nonzero energy to the wrong axis and trips this.
    #[test]
    fn fdct_preserves_axis_orientation() {
        let profile = [17i32, -40, 63, 5, -88, 100, -12, 34];

        // Vertical: value depends on row y only → energy confined to coefficient column u = 0.
        let mut block = [0i32; 64];
        for y in 0..8 {
            for x in 0..8 {
                block[y * 8 + x] = profile[y];
            }
        }
        fdct8x8(&mut block);
        for v in 0..8 {
            for u in 1..8 {
                assert_eq!(block[v * 8 + u], 0, "vertical: AC at (v={v}, u={u})");
            }
        }
        assert!((1..8).any(|v| block[v * 8] != 0), "vertical: no v-energy");

        // Horizontal: value depends on column x only → energy confined to coefficient row v = 0.
        let mut block = [0i32; 64];
        for y in 0..8 {
            for x in 0..8 {
                block[y * 8 + x] = profile[x];
            }
        }
        fdct8x8(&mut block);
        for v in 1..8 {
            for u in 0..8 {
                assert_eq!(block[v * 8 + u], 0, "horizontal: AC at (v={v}, u={u})");
            }
        }
        assert!((1..8).any(|u| block[u] != 0), "horizontal: no u-energy");
    }

    /// Forward then inverse recovers the original 8-bit level-shifted block within ±1 per sample.
    /// The tolerance is ±1 because the pipeline rounds twice independently — once to integer DCT
    /// coefficients, once to integer samples — and each rounding contributes at most half a unit;
    /// for these random blocks the combined per-sample error never exceeds one.
    #[test]
    fn forward_inverse_round_trip() {
        let mut rng = Lcg::new(0xdead_beef_cafe_0007);
        for _ in 0..400 {
            let mut original = [0i32; 64];
            for s in &mut original {
                *s = rng.level_shifted_sample(8);
            }
            let mut block = original;
            fdct8x8(&mut block);
            idct8x8(&mut block);
            for (idx, (&r, &o)) in block.iter().zip(original.iter()).enumerate() {
                assert!(
                    (r - o).abs() <= 1,
                    "round-trip drift {} at index {idx} (orig {o}, recon {r})",
                    r - o
                );
            }
        }
    }

    /// The pseudo-random generator IEEE Std 1180-1990 prescribes for its accuracy test
    /// (`ieeerand`, seeded with 1): uniform integers in `[-low, high]`. Only the low 31 bits of
    /// the state are ever read, so a 32-bit wrapping state reproduces the standard's sequence on
    /// any word size.
    struct Ieee1180Rand(u32);

    impl Ieee1180Rand {
        fn next(&mut self, low: i32, high: i32) -> i32 {
            self.0 = self.0.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            let unit = f64::from(self.0 & 0x7fff_fffe) / f64::from(0x7fff_ffff_u32);
            (unit * f64::from(low + high + 1)) as i32 - low
        }
    }

    /// The per-position and overall error statistics of one IEEE 1180 run, as the standard's
    /// five bounds read them.
    struct Ieee1180Stats {
        peak: i32,
        worst_position_mse: f64,
        overall_mse: f64,
        worst_position_mean: f64,
        overall_mean: f64,
    }

    /// One IEEE 1180-1990 run: `blocks` random blocks of samples in `[-low, high]` (negated when
    /// `negate`), forward-transformed by the ideal DCT and rounded, the coefficients clipped to
    /// `[-2048, 2047]`; then the reference ([`idct8x8`]) and the kernel under test
    /// ([`idct8x8_int`]) each reconstruct them, both clipped to `[-256, 255]`.
    fn ieee1180_run(low: i32, high: i32, negate: bool, blocks: usize) -> Ieee1180Stats {
        let mut rng = Ieee1180Rand(1);
        let mut sum = [0i64; 64];
        let mut sum_sq = [0i64; 64];
        let mut peak = 0;
        for _ in 0..blocks {
            let mut coeffs = [0i32; 64];
            for s in &mut coeffs {
                let v = rng.next(low, high);
                *s = if negate { -v } else { v };
            }
            fdct8x8(&mut coeffs);
            for c in &mut coeffs {
                *c = (*c).clamp(-2048, 2047);
            }
            let mut reference = coeffs;
            idct8x8(&mut reference);
            let mut test = coeffs;
            idct8x8_int(&mut test);
            for i in 0..64 {
                let err = test[i].clamp(-256, 255) - reference[i].clamp(-256, 255);
                peak = peak.max(err.abs());
                sum[i] += i64::from(err);
                sum_sq[i] += i64::from(err * err);
            }
        }
        let n = blocks as f64;
        let per = |acc: &[i64; 64]| acc.map(|a| a as f64 / n);
        let (mean, mse) = (per(&sum), per(&sum_sq));
        Ieee1180Stats {
            peak,
            worst_position_mse: mse.iter().copied().fold(0.0, f64::max),
            overall_mse: mse.iter().sum::<f64>() / 64.0,
            worst_position_mean: mean.iter().map(|m| m.abs()).fold(0.0, f64::max),
            overall_mean: (mean.iter().sum::<f64>() / 64.0).abs(),
        }
    }

    /// The IEEE 1180-1990 inverse-DCT accuracy test, in full: the three input ranges
    /// `(L, H) = (256, 255), (5, 5), (300, 300)`, each with 10 000 blocks and again sign-negated,
    /// against the standard's five bounds — peak error ≤ 1, per-position mean square error ≤ 0.06,
    /// overall mean square error ≤ 0.02, per-position mean error ≤ 0.015, overall mean error
    /// ≤ 0.0015. A wrong rotation constant, a lost rounding term or a biased descale fails one of
    /// them; the zero-in/zero-out clause is [`int_zero_block_is_zero`].
    ///
    /// Measured worst case over the six runs: peak 1, position MSE 0.0165, overall MSE 0.0139,
    /// position mean 0.0027, overall mean 0.000127 — inside every bound with margin, so the
    /// bounds are asserted as the standard states them.
    #[test]
    fn int_meets_ieee_1180_accuracy() {
        for (low, high) in [(256, 255), (5, 5), (300, 300)] {
            for negate in [false, true] {
                let s = ieee1180_run(low, high, negate, 10_000);
                let ctx = format!("L={low} H={high} negated={negate}");
                assert!(s.peak <= 1, "{ctx}: peak error {}", s.peak);
                assert!(
                    s.worst_position_mse <= 0.06,
                    "{ctx}: position mse {}",
                    s.worst_position_mse
                );
                assert!(
                    s.overall_mse <= 0.02,
                    "{ctx}: overall mse {}",
                    s.overall_mse
                );
                assert!(
                    s.worst_position_mean <= 0.015,
                    "{ctx}: position mean {}",
                    s.worst_position_mean
                );
                assert!(
                    s.overall_mean <= 0.0015,
                    "{ctx}: overall mean {}",
                    s.overall_mean
                );
            }
        }
    }

    /// IEEE 1180's last clause: an all-zero coefficient block reconstructs to all zeros.
    #[test]
    fn int_zero_block_is_zero() {
        let mut block = [0i32; 64];
        idct8x8_int(&mut block);
        assert_eq!(block, [0i32; 64]);
    }

    /// Every single-coefficient block — each of the 64 positions at both signs and three
    /// magnitudes — lands within one code of the rounded ideal value. A lone AC term is exactly
    /// the input that must *not* take a column or row shortcut, so a shortcut test that looked at
    /// the wrong coefficient (or accepted a nonzero one) flattens that block and fails here, as
    /// does a butterfly that misroutes one input.
    #[test]
    fn int_impulses_are_within_one_code_of_the_ideal() {
        for pos in 0..64 {
            for amplitude in [-1023, -100, -7, 7, 100, 1023] {
                let mut coeffs = [0i32; 64];
                coeffs[pos] = amplitude;
                let mut ideal = coeffs;
                idct8x8(&mut ideal);
                let mut got = coeffs;
                idct8x8_int(&mut got);
                for (i, (&g, &r)) in got.iter().zip(ideal.iter()).enumerate() {
                    assert!(
                        (g - r).abs() <= 1,
                        "impulse {amplitude} at {pos}: sample {i} got {g}, ideal {r}"
                    );
                }
            }
        }
    }

    /// Blocks dense in AC energy — every coefficient nonzero, so no column or row may shortcut —
    /// stay within one code of the rounded ideal value: the case where a shortcut test that
    /// fired on nonzero terms would discard the whole AC content.
    #[test]
    fn int_dense_blocks_are_within_one_code_of_the_ideal() {
        let mut rng = Lcg::new(0x5eed_1180_0000_0001);
        for _ in 0..500 {
            let mut coeffs = [0i32; 64];
            for c in &mut coeffs {
                let magnitude = 1 + rng.level_shifted_sample(6).abs();
                *c = if rng.next() >> 63 == 0 {
                    magnitude
                } else {
                    -magnitude
                };
            }
            let mut ideal = coeffs;
            idct8x8(&mut ideal);
            let mut got = coeffs;
            idct8x8_int(&mut got);
            for (i, (&g, &r)) in got.iter().zip(ideal.iter()).enumerate() {
                assert!((g - r).abs() <= 1, "sample {i} got {g}, ideal {r}");
            }
        }
    }

    /// The DC-only shortcut gives exactly what the kernel gives for the same DC-only block, over
    /// the whole 8-bit dequantized DC domain and at the `i32` extremes (where both narrow the
    /// same way).
    #[test]
    fn int_dc_shortcut_equals_the_kernel() {
        let extremes = [i32::MIN, i32::MIN + 1, i32::MAX - 1, i32::MAX];
        for dc in (-2048..=2047).chain(extremes) {
            let mut block = [0i32; 64];
            block[0] = dc;
            idct8x8_int(&mut block);
            let flat = idct8x8_int_dc(dc);
            assert!(
                block.iter().all(|&s| s == flat),
                "dc {dc}: {block:?} vs {flat}"
            );
        }
    }

    /// The DC-only value is `dc / 8` rounded half up — `jidctint.c`'s
    /// `DESCALE(dc << PASS1_BITS, PASS1_BITS + 3)` — pinned at the rounding boundaries.
    #[test]
    fn int_dc_rounds_half_up() {
        for (dc, expected) in [
            (0, 0),
            (3, 0),
            (4, 1),
            (11, 1),
            (12, 2),
            (-4, 0),
            (-5, -1),
            (-12, -1),
            (-13, -2),
        ] {
            assert_eq!(idct8x8_int_dc(dc), expected, "dc {dc}");
        }
    }
}
