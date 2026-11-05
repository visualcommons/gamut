//! AV1 transform sizes/types and the 2-D inverse transform process (AV1 §7.13.3), plus the matched
//! encoder forward 2-D transform.
//!
//! [`inverse_transform_2d`] is the normative decoder operation: per-row 1-D inverse (with the
//! rectangular `√2` scaling and `rowShift`), an intermediate clamp, then per-column 1-D inverse
//! (with `colShift`). It must be bit-exact and is the only transform-side operation the
//! reconstruction depends on. [`forward_transform_2d`] is the encoder operation; AV1 does not
//! specify it, and reconstruction correctness is independent of it (the encoder reconstructs with
//! the same inverse the decoder runs), so its only job is to produce good coefficients. The forward
//! pairs with the inverse up to a per-size power-of-two gain that is divided out.
//!
//! FLIPADST is not distinguished here — its 1-D transform is the inverse ADST; the up/down and
//! left/right flips are applied at reconstruction (AV1 §7.12.3), exposed via [`TxType::flip_ud`] /
//! [`TxType::flip_lr`].

use gamut_dsp::av1::{
    forward_adst, forward_dct, forward_identity, inverse_adst, inverse_dct, inverse_identity,
};
use gamut_dsp::math::{clip3, round2};

/// The 19 AV1 transform sizes (`TX_SIZES_ALL`), in spec order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(missing_docs)]
pub enum TxSize {
    Tx4x4,
    Tx8x8,
    Tx16x16,
    Tx32x32,
    Tx64x64,
    Tx4x8,
    Tx8x4,
    Tx8x16,
    Tx16x8,
    Tx16x32,
    Tx32x16,
    Tx32x64,
    Tx64x32,
    Tx4x16,
    Tx16x4,
    Tx8x32,
    Tx32x8,
    Tx16x64,
    Tx64x16,
}

// Tx_Width_Log2 / Tx_Height_Log2 (AV1 §9.3), in TX_SIZES_ALL order.
const TX_WIDTH_LOG2: [u32; 19] = [2, 3, 4, 5, 6, 2, 3, 3, 4, 4, 5, 5, 6, 2, 4, 3, 5, 4, 6];
const TX_HEIGHT_LOG2: [u32; 19] = [2, 3, 4, 5, 6, 3, 2, 4, 3, 5, 4, 6, 5, 4, 2, 5, 3, 6, 4];
// Transform_Row_Shift (AV1 §7.13.3).
const TRANSFORM_ROW_SHIFT: [u32; 19] = [0, 1, 2, 2, 2, 0, 0, 1, 1, 1, 1, 1, 1, 1, 1, 2, 2, 2, 2];

impl TxSize {
    /// Base-2 logarithm of the transform width.
    #[must_use]
    pub fn log2_width(self) -> u32 {
        TX_WIDTH_LOG2[self as usize]
    }

    /// Base-2 logarithm of the transform height.
    #[must_use]
    pub fn log2_height(self) -> u32 {
        TX_HEIGHT_LOG2[self as usize]
    }

    /// Transform width in samples.
    #[must_use]
    pub fn width(self) -> usize {
        1 << self.log2_width()
    }

    /// Transform height in samples.
    #[must_use]
    pub fn height(self) -> usize {
        1 << self.log2_height()
    }

    /// `dqDenom` (AV1 §7.12.3): the dequantization divisor for this size (1, 2, or 4).
    #[must_use]
    pub fn dq_denom(self) -> i32 {
        match self {
            TxSize::Tx32x32
            | TxSize::Tx16x32
            | TxSize::Tx32x16
            | TxSize::Tx16x64
            | TxSize::Tx64x16 => 2,
            TxSize::Tx64x64 | TxSize::Tx32x64 | TxSize::Tx64x32 => 4,
            _ => 1,
        }
    }

    fn row_shift(self) -> u32 {
        TRANSFORM_ROW_SHIFT[self as usize]
    }
}

/// The 1-D transform applied along one axis.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tx1d {
    Dct,
    Adst,
    Identity,
}

/// AV1 transform types (`TX_TYPES`). The name is `<vertical>_<horizontal>`, i.e.
/// `<column transform>_<row transform>`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(missing_docs)]
#[expect(
    dead_code,
    reason = "the FLIPADST and 1-D-identity types the encoder never chooses are spec vocabulary the \
              sample decoder (#259) reconstructs"
)]
pub enum TxType {
    DctDct,
    AdstDct,
    DctAdst,
    AdstAdst,
    FlipadstDct,
    DctFlipadst,
    FlipadstFlipadst,
    AdstFlipadst,
    FlipadstAdst,
    Idtx,
    VDct,
    HDct,
    VAdst,
    HAdst,
    VFlipadst,
    HFlipadst,
}

impl TxType {
    /// The 1-D transform applied along rows (the horizontal / width direction), AV1 §7.13.3.
    fn row_1d(self) -> Tx1d {
        use TxType::*;
        match self {
            DctDct | AdstDct | FlipadstDct | HDct => Tx1d::Dct,
            DctAdst | AdstAdst | DctFlipadst | FlipadstFlipadst | AdstFlipadst | FlipadstAdst
            | HAdst | HFlipadst => Tx1d::Adst,
            Idtx | VDct | VAdst | VFlipadst => Tx1d::Identity,
        }
    }

    /// The 1-D transform applied along columns (the vertical / height direction), AV1 §7.13.3.
    fn col_1d(self) -> Tx1d {
        use TxType::*;
        match self {
            DctDct | DctAdst | DctFlipadst | VDct => Tx1d::Dct,
            AdstDct | AdstAdst | FlipadstDct | FlipadstFlipadst | AdstFlipadst | FlipadstAdst
            | VAdst | VFlipadst => Tx1d::Adst,
            Idtx | HDct | HAdst | HFlipadst => Tx1d::Identity,
        }
    }

    /// Whether reconstruction flips this block vertically (AV1 §7.12.3 `flipUD`).
    #[must_use]
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "read by the sample decoder (#259)")
    )]
    pub fn flip_ud(self) -> bool {
        use TxType::*;
        matches!(
            self,
            FlipadstDct | FlipadstAdst | VFlipadst | FlipadstFlipadst
        )
    }

    /// Whether reconstruction flips this block horizontally (AV1 §7.12.3 `flipLR`).
    #[must_use]
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "read by the sample decoder (#259)")
    )]
    pub fn flip_lr(self) -> bool {
        use TxType::*;
        matches!(
            self,
            DctFlipadst | AdstFlipadst | HFlipadst | FlipadstFlipadst
        )
    }
}

/// Apply the 1-D inverse transform of `kind` over `t[..1<<n]` (clamp range `r`).
fn inverse_1d(kind: Tx1d, t: &mut [i64], n: u32, r: u32) {
    match kind {
        Tx1d::Dct => inverse_dct(t, n, r),
        Tx1d::Adst => inverse_adst(t, n, r),
        Tx1d::Identity => inverse_identity(t, n),
    }
}

/// Apply the 1-D forward transform of `kind` over `t[..1<<n]`.
fn forward_1d(kind: Tx1d, t: &mut [i64], n: u32) {
    match kind {
        Tx1d::Dct => forward_dct(t, n),
        Tx1d::Adst => forward_adst(t, n),
        Tx1d::Identity => forward_identity(t, n),
    }
}

/// 2-D inverse transform (AV1 §7.13.3). `dequant` is the row-major `h × w` array of dequantized
/// coefficients (only the top-left `min(32,w) × min(32,h)` are nonzero for large sizes); the result
/// is the row-major `h × w` residual.
///
/// This is the normative decoder transform and is bit-exact.
///
/// # Panics
/// Panics if `dequant.len() < width * height`.
#[must_use]
pub fn inverse_transform_2d(dequant: &[i32], tx: TxSize, ty: TxType, bit_depth: u32) -> Vec<i32> {
    let (log2w, log2h) = (tx.log2_width(), tx.log2_height());
    let (w, h) = (tx.width(), tx.height());
    assert!(dequant.len() >= w * h, "inverse_transform_2d: short input");
    let row_shift = tx.row_shift();
    let col_shift = 4u32;
    let row_clamp = bit_depth + 8;
    let col_clamp = (bit_depth + 6).max(16);
    let rect = (log2w as i32 - log2h as i32).abs() == 1;

    let mut resid = vec![0i64; w * h];
    let mut t = [0i64; 64];

    // Row transforms.
    for i in 0..h {
        for j in 0..w {
            t[j] = if i < 32 && j < 32 {
                i64::from(dequant[i * w + j])
            } else {
                0
            };
        }
        if rect {
            for v in &mut t[..w] {
                *v = round2(*v * 2896, 12);
            }
        }
        inverse_1d(ty.row_1d(), &mut t[..w], log2w, row_clamp);
        for j in 0..w {
            resid[i * w + j] = round2(t[j], row_shift);
        }
    }

    // Intermediate clamp.
    let lim = 1i64 << (col_clamp - 1);
    for v in &mut resid {
        *v = clip3(-lim, lim - 1, *v);
    }

    // Column transforms.
    for j in 0..w {
        for i in 0..h {
            t[i] = resid[i * w + j];
        }
        inverse_1d(ty.col_1d(), &mut t[..h], log2h, col_clamp);
        for i in 0..h {
            resid[i * w + j] = round2(t[i], col_shift);
        }
    }

    resid.iter().map(|&v| v as i32).collect()
}

/// 2-D forward transform (encoder). Applies the forward 1-D transforms along columns then rows and
/// divides out the inverse's fixed shifts, so `inverse_transform_2d(forward_transform_2d(x))`
/// recovers `x` up to quantization. Reconstruction correctness does not depend on this function.
///
/// `residual` is the row-major `h × w` residual; the result is the row-major `h × w` coefficient
/// array. FLIPADST flips are not applied here (see [`TxType::flip_ud`]/[`TxType::flip_lr`]).
///
/// # Panics
/// Panics if `residual.len() < width * height`.
#[must_use]
pub fn forward_transform_2d(residual: &[i32], tx: TxSize, ty: TxType) -> Vec<i32> {
    let (log2w, log2h) = (tx.log2_width(), tx.log2_height());
    let (w, h) = (tx.width(), tx.height());
    assert!(residual.len() >= w * h, "forward_transform_2d: short input");
    let rect = (log2w as i32 - log2h as i32).abs() == 1;

    let mut coeff = vec![0i64; w * h];
    let mut t = [0i64; 64];

    // Column forward transforms.
    for j in 0..w {
        for i in 0..h {
            t[i] = i64::from(residual[i * w + j]);
        }
        forward_1d(ty.col_1d(), &mut t[..h], log2h);
        for i in 0..h {
            coeff[i * w + j] = t[i];
        }
    }

    // Row forward transforms.
    for i in 0..h {
        for j in 0..w {
            t[j] = coeff[i * w + j];
        }
        forward_1d(ty.row_1d(), &mut t[..w], log2w);
        for j in 0..w {
            coeff[i * w + j] = t[j];
        }
    }

    // Normalize so the no-quant round trip is unit gain. `FWD_SHIFT` is calibrated for DCT/ADST on
    // both axes; an *identity* axis carries `2^(log2dim - 1)` less gain than a DCT/ADST axis, so the
    // forward amplifies by `log2dim - 1` extra per identity axis. Rectangular `|Δlog2|=1` sizes
    // additionally divide by `√2` (the inverse's rect scaling). Pinned by the `*_round_trip_*` tests.
    let mut shift = FWD_SHIFT[tx as usize];
    if ty.row_1d() == Tx1d::Identity {
        shift += log2w as i32 - 1;
    }
    if ty.col_1d() == Tx1d::Identity {
        shift += log2h as i32 - 1;
    }
    coeff
        .iter()
        .map(|&c| {
            let mut v = c;
            if rect {
                v = round2(v * 2896, 12);
            }
            v = if shift >= 0 {
                v << shift
            } else {
                round2(v, (-shift) as u32)
            };
            v as i32
        })
        .collect()
}

/// Per-size forward normalization shift (positive = left/amplify, negative = right/attenuate), in
/// `TX_SIZES_ALL` order. Rectangular `|Δlog2|=1` sizes additionally divide by `√2`. Pinned by the
/// `*_round_trip_*` tests.
const FWD_SHIFT: [i32; 19] = [
    2, 1, 0, -2, -4, 2, 2, 1, 1, -1, -1, -3, -3, 1, 1, 0, 0, -2, -2,
];

#[cfg(test)]
mod tests {
    use super::*;

    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0
        }
        fn resid(&mut self) -> i32 {
            (self.next() >> 40) as i32 % 256 - 128
        }
    }

    const ALL_SIZES: [TxSize; 19] = [
        TxSize::Tx4x4,
        TxSize::Tx8x8,
        TxSize::Tx16x16,
        TxSize::Tx32x32,
        TxSize::Tx64x64,
        TxSize::Tx4x8,
        TxSize::Tx8x4,
        TxSize::Tx8x16,
        TxSize::Tx16x8,
        TxSize::Tx16x32,
        TxSize::Tx32x16,
        TxSize::Tx32x64,
        TxSize::Tx64x32,
        TxSize::Tx4x16,
        TxSize::Tx16x4,
        TxSize::Tx8x32,
        TxSize::Tx32x8,
        TxSize::Tx16x64,
        TxSize::Tx64x16,
    ];

    /// A smooth (low-frequency) residual block: bilinear ramp plus a small dither. Low frequency so
    /// that 64-wide/tall transforms — which keep only the top-left 32×32 coefficients — can
    /// represent and round-trip it without losing energy to the truncation.
    fn smooth_residual(w: usize, h: usize, rng: &mut Lcg) -> Vec<i32> {
        // Zero-mean low-frequency ramp, like a real prediction residual: a small DC keeps the
        // big-block DC coefficient inside the inverse's intermediate clamp, and the low frequency
        // keeps it representable under the 64-size 32×32 coefficient truncation.
        let a = rng.resid() / 2;
        let b = rng.resid() / 2;
        let (cw, ch) = (w as i32, h as i32);
        (0..w * h)
            .map(|p| {
                let (x, y) = ((p % w) as i32, (p / w) as i32);
                (a * (2 * x - cw + 1)) / cw + (b * (2 * y - ch + 1)) / ch
            })
            .collect()
    }

    #[test]
    fn sizes_have_consistent_dimensions() {
        for tx in ALL_SIZES {
            assert_eq!(tx.width(), 1 << tx.log2_width());
            assert_eq!(tx.height(), 1 << tx.log2_height());
            assert!(tx.width() >= 4 && tx.width() <= 64);
            assert!(tx.height() >= 4 && tx.height() <= 64);
        }
    }

    #[test]
    fn dct_dct_round_trip_is_near_identity() {
        // smooth residual -> forward 2D -> inverse 2D recovers it (no quant) within rounding
        // tolerance, for every transform size. DCT_DCT is valid for all sizes.
        let mut rng = Lcg(0x00c0_ffee_1234_5678);
        for tx in ALL_SIZES {
            let (w, h) = (tx.width(), tx.height());
            let mut max_err = 0i32;
            for _ in 0..20 {
                let residual = smooth_residual(w, h, &mut rng);
                let coeff = forward_transform_2d(&residual, tx, TxType::DctDct);
                let recon = inverse_transform_2d(&coeff, tx, TxType::DctDct, 8);
                for (r, o) in residual.iter().zip(&recon) {
                    max_err = max_err.max((r - o).abs());
                }
            }
            // Fixed-point rounding accumulates with the number of passes/stages; bound generously.
            let bound = 4 + 2 * (tx.log2_width() as i32 + tx.log2_height() as i32);
            assert!(
                max_err <= bound,
                "{tx:?}: round-trip max error {max_err} exceeds {bound}",
            );
        }
    }

    #[test]
    fn adst_and_identity_round_trip() {
        // Exercise ADST (sizes with both dims <= 16) and identity (dims <= 32) tx types.
        let mut rng = Lcg(0xdead_1010_2020_3030);
        let cases = [
            (TxSize::Tx4x4, TxType::AdstAdst),
            (TxSize::Tx8x8, TxType::AdstDct),
            (TxSize::Tx16x16, TxType::DctAdst),
            (TxSize::Tx8x8, TxType::Idtx),
            (TxSize::Tx16x16, TxType::VDct),
            (TxSize::Tx32x32, TxType::Idtx),
            (TxSize::Tx8x16, TxType::HAdst),
            (TxSize::Tx16x8, TxType::VFlipadst),
        ];
        for (tx, ty) in cases {
            let (w, h) = (tx.width(), tx.height());
            let mut max_err = 0i32;
            for _ in 0..40 {
                let residual = smooth_residual(w, h, &mut rng);
                let coeff = forward_transform_2d(&residual, tx, ty);
                let recon = inverse_transform_2d(&coeff, tx, ty, 8);
                for (r, o) in residual.iter().zip(&recon) {
                    max_err = max_err.max((r - o).abs());
                }
            }
            let bound = 6 + 2 * (tx.log2_width() as i32 + tx.log2_height() as i32);
            assert!(
                max_err <= bound,
                "{tx:?}/{ty:?}: round-trip error {max_err} > {bound}"
            );
        }
    }

    #[test]
    fn flip_flags_match_spec() {
        assert!(TxType::FlipadstDct.flip_ud() && !TxType::FlipadstDct.flip_lr());
        assert!(TxType::DctFlipadst.flip_lr() && !TxType::DctFlipadst.flip_ud());
        assert!(TxType::FlipadstFlipadst.flip_ud() && TxType::FlipadstFlipadst.flip_lr());
        assert!(!TxType::DctDct.flip_ud() && !TxType::DctDct.flip_lr());
    }

    #[test]
    fn dq_denom_matches_spec() {
        assert_eq!(TxSize::Tx4x4.dq_denom(), 1);
        assert_eq!(TxSize::Tx32x32.dq_denom(), 2);
        assert_eq!(TxSize::Tx64x64.dq_denom(), 4);
        assert_eq!(TxSize::Tx16x32.dq_denom(), 2);
        assert_eq!(TxSize::Tx32x64.dq_denom(), 4);
        assert_eq!(TxSize::Tx8x8.dq_denom(), 1);
    }

    #[test]
    fn inverse_transform_zeros_coeffs_outside_kept_32x32() {
        // A 64-point transform codes only its top-left 32×32 coefficients (§7.13.3); any coefficient
        // at row or column index ≥ 32 must be dropped. Placing energy at exactly (row 32, col 0) and
        // (row 0, col 32) — one step past the boundary on each axis — must reconstruct to all-zero.
        // This pins the `i < 32 && j < 32` guard: an off-by-one (`<` → `<=`) or `&&` → `||` would
        // read that coefficient and leak it into the output.
        let tx = TxSize::Tx64x64;
        let (w, h) = (tx.width(), tx.height());
        for &(r, c) in &[(32usize, 0usize), (0, 32)] {
            let mut coeff = vec![0i32; w * h];
            coeff[r * w + c] = 1 << 18; // large, so any leak is unmistakable
            let out = inverse_transform_2d(&coeff, tx, TxType::DctDct, 8);
            assert!(
                out.iter().all(|&v| v == 0),
                "coeff at (row {r}, col {c}) past the 32×32 boundary leaked into the reconstruction",
            );
        }
    }

    #[test]
    fn inverse_transform_intermediate_clamp_is_pinned() {
        // Between the row and column passes the inverse clamps to the spec's intermediate range
        // (`colClampRange = max(bitDepth + 6, 16)`, magnitude `1 << (range - 1)`, §7.13.3). Driving
        // the row pass with maximum-magnitude dequantized coefficients pushes its output past that
        // range so the clamp engages. The golden is taken at 12-bit — the highest AV1 depth, the one
        // where `bitDepth + 6` clears the 16 floor so the `max(.., 16)` and the `range - 1` shift are
        // both live — pinning `colClampRange`, the `1 << (range - 1)` limit, and its `lim - 1` upper
        // bound. `sum`/`wsum` are full-reconstruction checksums (`wsum` position-weighted, so a change
        // at any single coefficient moves it). Values verified to shift under every clamp mutation.
        let bit_depth = 12u32;
        let amp = 1i32 << (7 + bit_depth); // dequant clamp magnitude at this depth (quant.rs)
        let cases = [
            (TxSize::Tx64x64, 0u32, 391_287i64, 763_543_869i64),
            (TxSize::Tx64x64, 1, 391_327, 839_599_092),
            (TxSize::Tx32x32, 0, 53_781, 28_040_091),
            (TxSize::Tx32x32, 1, 53_751, 27_072_964),
        ];
        for (tx, pat, want_sum, want_wsum) in cases {
            let (w, h) = (tx.width(), tx.height());
            // Coefficients at the dequant clamp magnitude, restricted to the kept 32×32; `scale`
            // halves them for the linearity probe below.
            let coeff = |scale: i32| -> Vec<i32> {
                (0..w * h)
                    .map(|p| {
                        let (x, y) = (p % w, p / w);
                        if x >= 32 || y >= 32 {
                            0
                        } else if pat == 0 || (x + y) % 2 == 0 {
                            (amp - 1) / scale
                        } else {
                            -amp / scale
                        }
                    })
                    .collect()
            };
            let out = inverse_transform_2d(&coeff(1), tx, TxType::DctDct, bit_depth);
            // The clamp must actually engage, or the golden would not exercise it: full-scale is
            // non-linear vs half-scale (saturation breaks `f(2x) = 2·f(x)`).
            let half = inverse_transform_2d(&coeff(2), tx, TxType::DctDct, bit_depth);
            assert!(
                out.iter().zip(&half).any(|(&a, &b)| a != 2 * b),
                "{tx:?} pat{pat}: intermediate clamp never engaged",
            );
            let sum: i64 = out.iter().map(|&v| i64::from(v)).sum();
            let wsum = out.iter().enumerate().fold(0i64, |acc, (i, &v)| {
                acc.wrapping_add(i64::from(v).wrapping_mul(i as i64 + 1))
            });
            assert_eq!((sum, wsum), (want_sum, want_wsum), "{tx:?} pat{pat}");
        }
    }
}
