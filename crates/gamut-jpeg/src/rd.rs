//! Rate–distortion optimized quantization: per-block trellis coefficient search and adaptive
//! per-block lambda modulation ([`JpegEncoder::with_rd_optimization`](crate::JpegEncoder::with_rd_optimization)).
//!
//! Plain §A.3.4 quantization rounds each DCT coefficient to its nearest multiple of the
//! quantization step, minimizing distortion alone. The trellis instead minimizes the Lagrangian
//! `J = D + λ·R` over the *joint* choice of every AC coefficient in a block, where `R` is the
//! exact §F.1.2.2 run/size entropy cost — dropping (or shrinking by one) a coefficient whose bits
//! buy little fidelity. The idea goes back to Crouse & Ramchandran's joint
//! thresholding/quantization and mozjpeg's trellis; this implementation is written against T.81's
//! coding model directly.
//!
//! Three deliberate free choices, documented here and in STATUS.md:
//!
//! - **AC only.** The DC coefficient keeps plain rounding: DC trellis is a cross-block dynamic
//!   program through the §F.1.2.1 predictor (and its restart resets) for marginal gain — deferred,
//!   as mozjpeg also ships it separately.
//! - **Fixed rate proxy.** Rates are costed against the typical Annex K.5/K.6 AC tables for the
//!   component's class regardless of what the stream later codes with
//!   ([`with_optimized_tables`](crate::JpegEncoder::with_optimized_tables) or the progressive
//!   per-scan tables — both built *after* the coefficients exist). A configuration-only rate model
//!   keeps the chosen coefficients identical between the baseline and progressive processes, which
//!   preserves the crate's progressive-equals-baseline exactness invariant.
//! - **Step-normalized distortion.** `D` is measured in units of each coefficient's own
//!   quantization step: `d_k = (e_k / s_k)²` (fixed point). The quantization table *is* the
//!   format's perceptual error-weighting model, so normalizing by it makes one λ meaningful for
//!   every frequency, table, and quality — a low-frequency (small-step) error costs
//!   proportionally more than the same absolute error at a high frequency, and λ itself is a
//!   dimensionless tuned constant ([`DEFAULT_LAMBDA`], an encoder free choice pinned by an exact
//!   unit test). mozjpeg's trellis normalizes its cost the same way.
//!
//! All arithmetic is integer (`i64`, fixed point), so the search is deterministic across
//! platforms. The unweighted DCT-domain squared error equals pixel-domain squared error because
//! `gamut_dsp`'s FDCT is orthonormal (Parseval); the step-normalization then reweights it
//! per-frequency exactly as the quantization table does.

use gamut_dsp::math::round_div_nearest;

use crate::encoder::magnitude_category;
use crate::huffman::EncTable;
use crate::marker;
use crate::zigzag::ZIGZAG;

/// Fixed-point scale for distortions, λ, and the modulation factor (2^12).
const FIX_SHIFT: u32 = 12;

/// The tuned λ, in `FIX_SHIFT` fixed point (so `144/4096 ≈ 0.035` step²-units of distortion per
/// bit). Dimensionless thanks to the step-normalized distortion: for a lone coefficient reached
/// by a `rate`-bit symbol the drop threshold is `|c|/s < (1 + λ·rate/2^12) / 2`, so 144 zeroes
/// the marginal `|c|/s ∈ [0.5, ~0.62)` band on a 7-bit symbol. Tuned on the oracle RD battery
/// (gradient + textured content, 64×48/96×80): the measured sweep gave 4.7% saved at λ = 64,
/// 8.0% at 128, 8.7% at 144, 9.4% at 160 with worst-cell PSNR losses of 0.28/0.31/0.35/0.43 dB —
/// 144 keeps clear margin under the battery's 0.5 dB gate. An encoder free choice, pinned by
/// `default_lambda_pins_the_tuned_constant`.
const DEFAULT_LAMBDA: i64 = 144;

/// Adaptive modulation clamp: λ is scaled by `m ∈ [1/4, 4]` (fixed-point) per block.
const MOD_MIN: i64 = 1 << (FIX_SHIFT - 2);
/// Upper modulation clamp; see [`MOD_MIN`].
const MOD_MAX: i64 = 1 << (FIX_SHIFT + 2);

/// The per-component rate–distortion context: the AC rate proxy table, the fixed-point λ, and
/// whether per-block adaptive modulation is enabled.
pub(crate) struct RdCtx {
    /// The AC-class rate proxy (typical Annex K.5 or K.6 for the component's class).
    ac: EncTable,
    /// λ in `FIX_SHIFT` fixed point ([`DEFAULT_LAMBDA`], or an explicit value in tests).
    lambda: i64,
    /// Modulate λ per block from the block's own AC energy ([`RdOptimization::TrellisAdaptive`](crate::RdOptimization::TrellisAdaptive)).
    adaptive: bool,
}

impl RdCtx {
    /// Builds the context for one component class: `ac` is the rate proxy, `adaptive` the
    /// per-block modulation switch. λ is the tuned [`DEFAULT_LAMBDA`] — the step-normalized
    /// distortion makes it valid for every table and quality.
    pub(crate) fn new(ac: EncTable, adaptive: bool) -> Self {
        Self {
            lambda: DEFAULT_LAMBDA,
            ac,
            adaptive,
        }
    }

    /// The bit cost of one `(run, size)` AC symbol plus its `size` magnitude bits, or `None` when
    /// the proxy table has no code for it (the transition is pruned, never invented).
    fn rs_bits(&self, run: u8, size: u8) -> Option<i64> {
        let (_, len) = self.ac.lookup(marker::pack_nibbles(run, size))?;
        Some(i64::from(len) + i64::from(size))
    }

    /// The bit cost of a ZRL (16-zero-run) symbol, if the proxy table codes one.
    fn zrl_bits(&self) -> Option<i64> {
        self.ac.lookup(0xF0).map(|(_, len)| i64::from(len))
    }

    /// The bit cost of an EOB symbol, if the proxy table codes one.
    fn eob_bits(&self) -> Option<i64> {
        self.ac.lookup(0x00).map(|(_, len)| i64::from(len))
    }
}

/// Integer square root: the largest `r` with `r² ≤ n`.
fn isqrt(n: i64) -> i64 {
    debug_assert!(n >= 0);
    // Seed one BELOW the float estimate: the correctly-rounded f64 sqrt of an i64 is within one
    // of the true root (above 2⁵² it can land exactly on ⌈√n⌉ — e.g. n = (2²⁶+1)² − 1 rounds to
    // 2²⁶ + 1), so `estimate − 1` never overshoots and the single ascending settle is exact and
    // platform-independent. A negative seed (n = 0) self-heals: the first probe is 0² ≤ 0.
    let mut r = (n as f64).sqrt() as i64 - 1;
    while (r + 1) * (r + 1) <= n {
        r += 1;
    }
    r
}

/// The per-block λ modulation factor in `FIX_SHIFT` fixed point, clamped to `[1/4, 4]`:
/// `m = √(ac_energy / Σ step²)`. A busy block (AC energy well above one step per coefficient)
/// masks error and tolerates a larger λ; a flat block gets a smaller one. The factor depends only
/// on the block's own coefficients, so it is deterministic and restart-independent.
fn activity_factor(ac_energy: i64, sum_step_sq: i64) -> i64 {
    if sum_step_sq == 0 {
        return 1 << FIX_SHIFT;
    }
    let m = isqrt((ac_energy << (2 * FIX_SHIFT)) / sum_step_sq);
    m.clamp(MOD_MIN, MOD_MAX)
}

/// Trellis-quantizes one block: `dct` is the natural-order **unquantized** FDCT output, `quant`
/// the natural-order steps. Returns natural-order quantized coefficients whose entropy coding is
/// `J = D + λ·R`-optimal over the §F.1.2.2 run/size model (per-coefficient candidates: the
/// nearest multiple and its shrink-by-one neighbour, or zero via the run structure).
///
/// DC (`dct[0]`) is always plain nearest rounding. If the rate proxy cannot price a path at all
/// (a symbol missing from the table — impossible with the standard tables), the block falls back
/// to plain rounding rather than guessing.
pub(crate) fn trellis_quantize(dct: &[i32; 64], quant: &[u16; 64], ctx: &RdCtx) -> [i32; 64] {
    // Per zig-zag position 1..=63: the unquantized coefficient, its step, its nearest-rounding
    // candidate, and the cumulative zeroing distortion prefix P[k] = Σ_{i≤k} (c_i/s_i)² (fixed
    // point) — distortions are normalized by each coefficient's own step (module docs). The raw
    // (unnormalized) AC energy feeds the adaptive activity factor.
    let mut coeff = [0i64; 64]; // signed unquantized coefficient, zig-zag order
    let mut step = [1i64; 64];
    let mut v_hi = [0i64; 64];
    let mut prefix = [0i64; 64]; // prefix[k] = P[k]; prefix[0] = 0 (DC never zeroed here)
    let mut ac_energy = 0i64;
    let mut sum_step_sq = 0i64;
    for k in 1..64 {
        let c = i64::from(dct[ZIGZAG[k]]);
        let s = i64::from(quant[ZIGZAG[k]]);
        coeff[k] = c;
        step[k] = s;
        v_hi[k] = i64::from(round_div_nearest(c.unsigned_abs() as i32, s as i32));
        prefix[k] = prefix[k - 1] + ((c * c) << FIX_SHIFT) / (s * s);
        ac_energy += c * c;
        sum_step_sq += s * s;
    }

    let lambda = if ctx.adaptive {
        (ctx.lambda * activity_factor(ac_energy, sum_step_sq)) >> FIX_SHIFT
    } else {
        ctx.lambda
    };

    // Step-normalized squared error of coding position k as magnitude v (v ≥ 1), fixed point.
    let dist = |k: usize, v: i64| -> i64 {
        let e = coeff[k].abs() - v * step[k];
        ((e * e) << FIX_SHIFT) / (step[k] * step[k])
    };
    // Rate of a transition arriving at a coefficient of magnitude v after `run` zeros: any whole
    // 16-zero prefixes as ZRLs, then the (run % 16, size) symbol plus its magnitude bits.
    let rate = |run: i64, v: i64| -> Option<i64> {
        let zrl = if run >= 16 {
            ctx.zrl_bits()? * (run / 16)
        } else {
            0
        };
        let size = magnitude_category(v as i32);
        Some(zrl + ctx.rs_bits((run % 16) as u8, size)?)
    };

    // DP over nodes (k, cand) with cand ∈ {v_hi, v_hi − 1} \ {0}; node j = 0 is the virtual
    // start (the DC slot). best[k][c] is the minimal J of coding 1..=k with the last nonzero at k
    // as candidate c; parents record the backtrack chain. Costs are D·2^FIX + λ_fixed·bits.
    const UNREACHABLE: i64 = i64::MAX;
    let cand = |k: usize, c: usize| -> i64 { if c == 0 { v_hi[k] } else { v_hi[k] - 1 } };
    let mut best = [[UNREACHABLE; 2]; 64];
    let mut parent = [[(0usize, 0usize); 2]; 64];
    for k in 1..64 {
        for c in 0..2usize {
            let v = cand(k, c);
            if v < 1 {
                continue;
            }
            let node_cost = dist(k, v);
            // Predecessors, nearest first: on exact ties the denser coding wins, which keeps the
            // λ = 0 search identical to plain nearest rounding (including half-step ties).
            for j in (0..k).rev() {
                let base = if j == 0 {
                    0
                } else {
                    let b = best[j].iter().copied().min().unwrap_or(UNREACHABLE);
                    if b == UNREACHABLE {
                        continue;
                    }
                    // The two candidates of j differ in value, not reachability; take the best,
                    // but remember which for the backtrack.
                    b
                };
                let Some(r) = rate((k - j - 1) as i64, v) else {
                    continue;
                };
                let zero_gap = prefix[k - 1] - prefix[j];
                let total = base + lambda * r + zero_gap + node_cost;
                if total < best[k][c] {
                    // Which of j's candidates carried `base` (j = 0 has none, so 0 is fine).
                    let jc = usize::from(j != 0 && best[j][1] < best[j][0]);
                    best[k][c] = total;
                    parent[k][c] = (j, jc);
                }
            }
        }
    }

    // Termination: end the block after position j (EOB unless j == 63), farthest last-nonzero
    // first so exact ties again prefer the plain-rounding-shaped (denser) coding.
    let mut final_best = UNREACHABLE;
    let mut final_node: Option<(usize, usize)> = None;
    for j in (1..64).rev() {
        for (c, &cost) in best[j].iter().enumerate() {
            if cost == UNREACHABLE {
                continue;
            }
            let tail = prefix[63] - prefix[j];
            let eob = if j < 63 {
                match ctx.eob_bits() {
                    Some(b) => lambda * b,
                    None => continue,
                }
            } else {
                0
            };
            let total = cost + tail + eob;
            if total < final_best {
                final_best = total;
                final_node = Some((j, c));
            }
        }
    }
    // The all-zero coding: §F.1.2.2 emits EOB whenever the block ends in zeros, so an empty AC
    // block always pays exactly one EOB.
    if let Some(eob) = ctx.eob_bits() {
        let all_zero = prefix[63] + lambda * eob;
        if all_zero < final_best {
            final_node = None;
        }
    } else if final_best == UNREACHABLE {
        // No EOB and no reachable node: the proxy cannot price this block at all.
        let mut q = [0i32; 64];
        for (i, dst) in q.iter_mut().enumerate() {
            *dst = round_div_nearest(dct[i], i32::from(quant[i]));
        }
        return q;
    }

    // Backtrack into a natural-order block; DC is plain nearest rounding.
    let mut q = [0i32; 64];
    q[0] = round_div_nearest(dct[0], i32::from(quant[0]));
    let mut node = final_node;
    while let Some((k, c)) = node {
        let v = cand(k, c);
        q[ZIGZAG[k]] = if coeff[k] < 0 { -v as i32 } else { v as i32 };
        let (j, jc) = parent[k][c];
        node = (j > 0).then_some((j, jc));
    }
    q
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::huffman::{self, EncTable};
    use crate::quant;

    /// A context over the typical Annex K.5 luma AC table with an explicit λ.
    fn ctx_with_lambda(lambda: i64) -> RdCtx {
        RdCtx {
            ac: EncTable::from_spec(&huffman::STD_LUMA_AC),
            lambda,
            adaptive: false,
        }
    }

    /// Plain §A.3.4 nearest rounding of a raw DCT block — the trellis's λ = 0 reference.
    fn plain(dct: &[i32; 64], quant: &[u16; 64]) -> [i32; 64] {
        let mut q = [0i32; 64];
        for (dst, (&c, &s)) in q.iter_mut().zip(dct.iter().zip(quant.iter())) {
            *dst = round_div_nearest(c, i32::from(s));
        }
        q
    }

    /// A block with `value` at zig-zag position `k` (everything else zero, DC included).
    fn block_at(k: usize, value: i32) -> [i32; 64] {
        let mut b = [0i32; 64];
        b[ZIGZAG[k]] = value;
        b
    }

    #[test]
    fn rate_model_matches_the_annex_k5_lengths() {
        // Annex K.5 (typical luma AC): EOB = 4 bits, ZRL = 11 bits, (0,1) = 2-bit code, (4,1) =
        // 6-bit code, (15,10) = 16-bit code. rs_bits adds the `size` magnitude bits on top; a
        // symbol outside the table (size 11 does not exist in baseline) is None, never a guess.
        let ctx = ctx_with_lambda(0);
        assert_eq!(ctx.eob_bits(), Some(4));
        assert_eq!(ctx.zrl_bits(), Some(11));
        assert_eq!(ctx.rs_bits(0, 1), Some(2 + 1));
        assert_eq!(ctx.rs_bits(4, 1), Some(6 + 1));
        assert_eq!(ctx.rs_bits(15, 10), Some(16 + 10));
        assert_eq!(ctx.rs_bits(3, 11), None);
    }

    #[test]
    fn zero_lambda_reproduces_plain_rounding() {
        // With λ = 0 the search minimizes pure distortion, whose per-coefficient optimum is
        // exactly nearest rounding — including DC, zero-rounded positions, and the EOB tail. A
        // deterministic LCG covers many magnitudes and signs; any candidate-generation or
        // distortion-accounting error diverges somewhere in the battery.
        let quant = quant::scale(&quant::LUMINANCE, 50);
        let ctx = ctx_with_lambda(0);
        let mut state = 0x1234_5678u32;
        for _ in 0..64 {
            let mut dct = [0i32; 64];
            for c in dct.iter_mut() {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                // Signed coefficients in roughly the FDCT's real output range.
                *c = ((state >> 20) as i32) - 2048;
            }
            assert_eq!(trellis_quantize(&dct, &quant, &ctx), plain(&dct, &quant));
        }
    }

    #[test]
    fn zero_lambda_keeps_the_exact_half_step_tie() {
        // |c| = s/2 is the rounding tie: `round_div_nearest(5, 10)` gives 1 and zeroing gives the
        // identical distortion 25. The trellis must resolve the tie the same way plain rounding
        // does (keep the 1), which the nearest-predecessor-first / strict-`<` ordering guarantees.
        let mut quant = [1u16; 64];
        quant[ZIGZAG[5]] = 10;
        let ctx = ctx_with_lambda(0);
        let out = trellis_quantize(&block_at(5, 5), &quant, &ctx);
        assert_eq!(out[ZIGZAG[5]], 1);
    }

    #[test]
    fn huge_lambda_zeroes_every_ac_but_never_dc() {
        // When rate dominates completely the cheapest coding is the immediate EOB: all AC zero.
        // DC is exempt from the trellis by design (plain rounding).
        let quant = quant::scale(&quant::LUMINANCE, 50);
        let ctx = ctx_with_lambda(i64::MAX / (1 << 20));
        let mut dct = [0i32; 64];
        dct[0] = 400; // DC
        dct[ZIGZAG[1]] = 300;
        dct[ZIGZAG[8]] = -250;
        dct[ZIGZAG[63]] = 90;
        let out = trellis_quantize(&dct, &quant, &ctx);
        assert_eq!(out[0], round_div_nearest(400, i32::from(quant[0])));
        assert!(
            out.iter().enumerate().all(|(i, &v)| i == 0 || v == 0),
            "all AC must be zeroed at huge λ"
        );
    }

    #[test]
    fn drop_versus_keep_threshold_is_exact() {
        // One coefficient c = 6 at zig-zag 5 with step 10: keeping v = 1 costs normalized
        // distortion (16·2^12)/100 = 655 and rate rs(4,1) = 7 bits; dropping costs (36·2^12)/100
        // = 1474 (EOB is paid either way). The decision flips at λ·7 = 1474 − 655 → λ = 117.
        let mut quant = [1u16; 64];
        quant[ZIGZAG[5]] = 10;
        let dct = block_at(5, 6);
        let keep = trellis_quantize(&dct, &quant, &ctx_with_lambda(110));
        assert_eq!(keep[ZIGZAG[5]], 1, "below threshold the coefficient stays");
        let drop = trellis_quantize(&dct, &quant, &ctx_with_lambda(125));
        assert_eq!(drop, [0i32; 64], "above threshold it is zeroed");
    }

    #[test]
    fn shrink_by_one_candidate_wins_when_a_bit_is_worth_more_than_its_error() {
        // c = 17, s = 10 → v_hi = 2 (normalized d = (9·2^12)/100 = 368, size 2) vs v = 1
        // (d = (49·2^12)/100 = 2007, size 1). Keeping the same run, the size-2 symbol costs
        // rs(4,2) − rs(4,1) = (10+2) − (6+1) = 5 more bits, so above λ·5 = 2007 − 368 → λ = 328
        // the smaller candidate wins (zero only far above: its own threshold is λ = 1404). This
        // pins the v_hi − 1 candidate generation.
        let mut quant = [1u16; 64];
        quant[ZIGZAG[5]] = 10;
        let dct = block_at(5, 17);
        let hi = trellis_quantize(&dct, &quant, &ctx_with_lambda(300));
        assert_eq!(hi[ZIGZAG[5]], 2);
        let lo = trellis_quantize(&dct, &quant, &ctx_with_lambda(350));
        assert_eq!(lo[ZIGZAG[5]], 1);
    }

    #[test]
    fn run_of_16_costs_zrl_plus_short_symbol_and_beats_the_run_of_15() {
        // K.5 quirk the model must reproduce: a 15-zero run codes as the rare 16-bit (15,1)
        // symbol (17 bits with the magnitude bit), while a 16-zero run codes ZRL + (0,1)
        // (11 + 3 = 14 bits). With z − d = 1474 − 655 = 819 the keep threshold is λ < 48 for the
        // run of 15 but λ < 58 for the run of 16 — at λ = 53 the coefficient behind MORE zeros is
        // the one that survives.
        let mut quant16 = [1u16; 64];
        quant16[ZIGZAG[16]] = 10;
        let behind_15 = trellis_quantize(&block_at(16, 6), &quant16, &ctx_with_lambda(53));
        assert_eq!(behind_15, [0i32; 64], "run of 15: dropped at λ = 53");

        let mut quant17 = [1u16; 64];
        quant17[ZIGZAG[17]] = 10;
        let behind_16 = trellis_quantize(&block_at(17, 6), &quant17, &ctx_with_lambda(53));
        assert_eq!(behind_16[ZIGZAG[17]], 1, "run of 16: kept at λ = 53");
    }

    #[test]
    fn last_position_pays_no_eob() {
        // A lone coefficient at zig-zag 63 codes as 3 ZRLs + (14,1) + 1 magnitude bit = 50 bits
        // and, ending the block, pays no EOB — while dropping it pays the 4-bit EOB. With
        // z − d = 819 the keep condition is λ·(50 − 4) < 819 → λ < 17.8; were EOB wrongly charged
        // at position 63 the bound would be λ < 16.4. λ = 17 sits between the two and must keep.
        let mut quant = [1u16; 64];
        quant[ZIGZAG[63]] = 10;
        let out = trellis_quantize(&block_at(63, 6), &quant, &ctx_with_lambda(17));
        assert_eq!(out[ZIGZAG[63]], 1);
    }

    #[test]
    fn default_lambda_pins_the_tuned_constant() {
        // The λ value is an encoder free choice; this exact pin is the regression guard that
        // stands in for a mutants exclusion. 144/2^12 ≈ 0.035 step²-units per bit zeroes the
        // marginal |c|/s ∈ [0.5, ~0.62) band on a 7-bit symbol (see the constant's docs).
        assert_eq!(DEFAULT_LAMBDA, 144);
        assert_eq!(
            RdCtx::new(EncTable::from_spec(&huffman::STD_LUMA_AC), false).lambda,
            144
        );
    }

    #[test]
    fn activity_factor_is_monotone_and_clamped() {
        let sum_step_sq = 63 * 100;
        // Flat block: no AC energy → the lower clamp. Busy: far above reference → upper clamp.
        assert_eq!(activity_factor(0, sum_step_sq), MOD_MIN);
        assert_eq!(activity_factor(i64::from(u32::MAX), sum_step_sq), MOD_MAX);
        // At exactly the reference energy the factor is 1.0 (2^12).
        assert_eq!(activity_factor(sum_step_sq, sum_step_sq), 1 << FIX_SHIFT);
        // Monotone in between.
        let quarter = activity_factor(sum_step_sq / 2, sum_step_sq);
        let mid = activity_factor(sum_step_sq * 2, sum_step_sq);
        assert!(MOD_MIN < quarter && quarter < (1 << FIX_SHIFT));
        assert!((1 << FIX_SHIFT) < mid && mid < MOD_MAX);
    }

    #[test]
    fn adaptive_context_protects_a_flat_block() {
        // A low-energy block (every |c|/s marginal) modulates λ DOWN, so the adaptive coding
        // keeps at least as many coefficients as the base trellis — the flat-block protection
        // direction, the mirror of the busy-block test below.
        let quant = quant::scale(&quant::LUMINANCE, 50);
        let mut dct = [0i32; 64];
        // Low-frequency coefficients just above half a step: prime droppable territory.
        for k in 1..6 {
            dct[ZIGZAG[k]] = (i64::from(quant[ZIGZAG[k]]) * 6 / 10) as i32;
        }
        let base = ctx_with_lambda(DEFAULT_LAMBDA);
        let adaptive = RdCtx {
            ac: EncTable::from_spec(&huffman::STD_LUMA_AC),
            lambda: DEFAULT_LAMBDA,
            adaptive: true,
        };
        let base_kept = trellis_quantize(&dct, &quant, &base)
            .iter()
            .filter(|&&v| v != 0)
            .count();
        let adaptive_kept = trellis_quantize(&dct, &quant, &adaptive)
            .iter()
            .filter(|&&v| v != 0)
            .count();
        assert!(
            adaptive_kept >= base_kept,
            "flat block: adaptive kept {adaptive_kept} < trellis {base_kept}"
        );
        assert!(adaptive_kept > 0, "flat-block protection must keep detail");
    }

    #[test]
    fn modulation_clamps_and_the_zero_reference_guard_are_pinned() {
        // The clamp bounds are load-bearing free choices: pin their numeric values (a mutated
        // shift or offset in the constant would silently widen or narrow the adaptive range).
        assert_eq!(MOD_MIN, 1024); // ¼ in 2^12 fixed point
        assert_eq!(MOD_MAX, 16384); // 4 in 2^12 fixed point
        // The zero-reference guard (defensive: in-crate quantization steps are always ≥ 1) must
        // return exactly 1.0 fixed-point, not a shifted-down zero.
        assert_eq!(activity_factor(123, 0), 4096);
    }

    #[test]
    fn zero_lambda_keeps_adjacent_half_step_ties() {
        // TWO adjacent half-step ties: every keep/drop combination of the pair costs the same
        // distortion at λ = 0, so the search's tie-breaking discipline (strict `<`, nearest
        // predecessor first, farthest termination first) is what makes the output equal plain
        // rounding — a tie-preference flip anywhere in the DP or termination loop drops one.
        let mut quant = [1u16; 64];
        quant[ZIGZAG[5]] = 10;
        quant[ZIGZAG[6]] = 10;
        let mut dct = [0i32; 64];
        dct[ZIGZAG[5]] = 5;
        dct[ZIGZAG[6]] = 5;
        let out = trellis_quantize(&dct, &quant, &ctx_with_lambda(0));
        assert_eq!((out[ZIGZAG[5]], out[ZIGZAG[6]]), (1, 1));
    }

    #[test]
    fn zrl_runs_are_charged_and_multiplied() {
        // The ZRL arm of the rate model, both branch selection and multiplication.
        //
        // Branch: a run of 16 codes as ZRL + (0,1) = 11 + 3 = 14 bits, so with z − d = 819 the
        // keep threshold is λ < 58; were the ZRL branch skipped the rate would be 3 bits and the
        // threshold λ < 273. λ = 100 separates them: the coefficient must be DROPPED.
        let mut quant17 = [1u16; 64];
        quant17[ZIGZAG[17]] = 10;
        let dropped = trellis_quantize(&block_at(17, 6), &quant17, &ctx_with_lambda(100));
        assert_eq!(dropped, [0i32; 64], "run of 16 must pay its ZRL at λ = 100");

        // Multiplication: a run of 33 needs TWO ZRLs — rate = 2·11 + (1,1)'s 4 + 1 = 27 bits,
        // keep threshold λ < 30. A miscounted single ZRL (rate 18 → λ < 45) or a divided one
        // (rate 10 → λ < 81) would keep at λ = 38; the correct cost drops.
        let mut quant34 = [1u16; 64];
        quant34[ZIGZAG[34]] = 10;
        let dropped = trellis_quantize(&block_at(34, 6), &quant34, &ctx_with_lambda(38));
        assert_eq!(
            dropped, [0i32; 64],
            "run of 33 must pay both ZRLs at λ = 38"
        );
        // And below the true threshold the same coefficient survives (the branch is priced, not
        // pruned).
        let kept = trellis_quantize(&block_at(34, 6), &quant34, &ctx_with_lambda(25));
        assert_eq!(kept[ZIGZAG[34]], 1);
    }

    #[test]
    fn adaptive_lambda_is_exactly_base_times_activity() {
        // The adaptive path must scale λ by exactly activity_factor(block AC energy, Σ step²):
        // recompute both inputs here with the true formulas and demand array equality with an
        // explicit-λ run. The block is built so the resulting λ (453) and the λ of a perturbed
        // activity reference (the clamp at 576) sit on opposite sides of a marginal
        // coefficient's drop threshold (468) — so a miscomputed energy or step sum changes the
        // output, not just an internal number.
        let quant = [10u16; 64];
        let mut dct = [0i32; 64];
        dct[ZIGZAG[1]] = 250; // big: kept at every λ in play; provides the block's energy
        dct[ZIGZAG[5]] = 9; // marginal: kept at λ = 453, dropped at λ = 576
        let energy: i64 = 250 * 250 + 9 * 9;
        let sum_step_sq: i64 = 63 * 100;
        let factor = activity_factor(energy, sum_step_sq);
        let explicit = (DEFAULT_LAMBDA * factor) >> FIX_SHIFT;
        assert_eq!(explicit, 453, "the crafted block's effective λ");

        let adaptive_ctx = RdCtx {
            ac: EncTable::from_spec(&huffman::STD_LUMA_AC),
            lambda: DEFAULT_LAMBDA,
            adaptive: true,
        };
        let adaptive = trellis_quantize(&dct, &quant, &adaptive_ctx);
        let reference = trellis_quantize(&dct, &quant, &ctx_with_lambda(explicit));
        assert_eq!(adaptive, reference);
        assert_eq!(
            adaptive[ZIGZAG[5]], 1,
            "marginal coefficient kept at λ = 453"
        );
        // Sanity for the kill-radius claim: the clamped-up λ really flips the marginal
        // coefficient, so a perturbed activity computation cannot masquerade as correct.
        let flipped = trellis_quantize(&dct, &quant, &ctx_with_lambda(576));
        assert_eq!(flipped[ZIGZAG[5]], 0);
    }

    #[test]
    fn a_table_with_no_eob_falls_back_to_plain_rounding() {
        // A proxy table carrying ONLY the ZRL symbol prices no transition and no EOB, so the
        // search cannot cost any coding of the block at all — the documented fallback is plain
        // nearest rounding, never a silently zeroed block.
        let mut bits = [0u8; 16];
        bits[10] = 1; // one symbol of code length 11
        let zrl_only = RdCtx {
            ac: EncTable::from_bits_values(&bits, &[0xF0]),
            lambda: DEFAULT_LAMBDA,
            adaptive: false,
        };
        let quant = [10u16; 64];
        let mut dct = [0i32; 64];
        dct[0] = 77;
        dct[ZIGZAG[1]] = 25;
        let out = trellis_quantize(&dct, &quant, &zrl_only);
        let mut plain_expected = [0i32; 64];
        plain_expected[0] = round_div_nearest(77, 10);
        plain_expected[ZIGZAG[1]] = round_div_nearest(25, 10);
        assert_eq!(out, plain_expected);
    }

    #[test]
    fn isqrt_is_exact_at_boundaries() {
        for (n, r) in [
            (0, 0),
            (1, 1),
            (3, 1),
            (4, 2),
            (15, 3),
            (16, 4),
            (1 << 40, 1 << 20),
        ] {
            assert_eq!(isqrt(n), r, "isqrt({n})");
        }
        // n = (2²⁶+1)² − 1: the correctly-rounded f64 sqrt is EXACTLY 2²⁶ + 1 (the true root
        // plus ~1/2²⁷, closer to the next float than to itself), so a naive truncate-the-float
        // seed overshoots — this pins the seed-minus-one discipline that keeps the ascending
        // settle sufficient. (Below 2⁵² the float estimate never overshoots; this is the
        // smallest-magnitude family where it does.)
        let k = (1i64 << 26) + 1;
        assert_eq!(isqrt(k * k - 1), k - 1);
        assert_eq!(isqrt((1 << 40) - 1), (1 << 20) - 1);
    }

    #[test]
    fn adaptive_context_spends_fewer_bits_on_a_busy_block_than_trellis_alone() {
        // The same busy block under the same base λ: the adaptive context scales λ up (busy
        // blocks mask error), so its coding can only drop MORE coefficients, never fewer.
        let quant = quant::scale(&quant::LUMINANCE, 50);
        let mut state = 0x00C0_FFEEu32;
        let mut dct = [0i32; 64];
        for c in dct.iter_mut() {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            *c = ((state >> 21) as i32) - 1024;
        }
        let base = ctx_with_lambda(DEFAULT_LAMBDA);
        let adaptive = RdCtx {
            ac: EncTable::from_spec(&huffman::STD_LUMA_AC),
            lambda: DEFAULT_LAMBDA,
            adaptive: true,
        };
        let plain_kept = trellis_quantize(&dct, &quant, &base)
            .iter()
            .skip(1)
            .filter(|&&v| v != 0)
            .count();
        let adaptive_kept = trellis_quantize(&dct, &quant, &adaptive)
            .iter()
            .skip(1)
            .filter(|&&v| v != 0)
            .count();
        assert!(
            adaptive_kept <= plain_kept,
            "busy block: adaptive kept {adaptive_kept} > trellis {plain_kept}"
        );
    }
}
