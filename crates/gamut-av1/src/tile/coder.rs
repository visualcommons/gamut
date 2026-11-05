//! The symbol coder every syntax element goes through, and the rate units a trial prices in:
//! [`Coder`] codes into the range coder or, under [`Coding::Trial`], only tallies the rate, in
//! [`RATE_ONE`] fixed point; plus the non-symmetric and sub-exponential literal codes (§4.10).

use gamut_bitstream::SymbolEncoder;

use crate::cdf;

/// One bit of rate, in the fixed-point units every rate estimate is kept in (1/256 bit).
pub(super) const RATE_ONE: u32 = 1 << 8;

/// `log2(1 + i / 256)` in [`RATE_ONE`] units, truncated, for `i` in `0..256`: the fractional part
/// of [`log2_rate`], indexed by the eight bits below a value's leading one. Each entry takes the
/// eight fractional bits one at a time by squaring the mantissa (in Q16): a square at or above 2
/// means the next bit is set. Integer-only and built at compile time, so a rate estimate — and
/// therefore every decision it ranks — is identical on every target.
const LOG2_FRAC: [u16; 256] = {
    let mut table = [0u16; 256];
    let mut i = 0;
    while i < 256 {
        let mut m = (256 + i as u64) << 8;
        let mut bit = 8;
        while bit > 0 {
            bit -= 1;
            m = (m * m) >> 16;
            if m >= 2 << 16 {
                m >>= 1;
                table[i] |= 1 << bit;
            }
        }
        i += 1;
    }
    table
};

/// `log2(x)` in [`RATE_ONE`] units, for `x >= 1`: the integer part from the bit length, the
/// fractional part from [`LOG2_FRAC`] on the next eight bits (truncated below them).
fn log2_rate(x: u32) -> u32 {
    let int = x.ilog2();
    // The mantissa `x / 2^int` to eight fractional bits, in `[256, 512)`.
    let mantissa = (u64::from(x) << 8) >> int;
    int * RATE_ONE + u32::from(LOG2_FRAC[mantissa as usize - 256])
}

/// The rate of coding `symbol` against the cumulative `cdf` (§8.2.6 orientation: values rise to
/// 32768), in [`RATE_ONE`] units: `-log2(p)` for the symbol's probability `p / 32768`.
pub(super) fn symbol_rate(cdf: &[u16], symbol: usize) -> u32 {
    let below = if symbol > 0 { cdf[symbol - 1] } else { 0 };
    let p = u32::from(cdf[symbol] - below).max(1);
    15 * RATE_ONE - log2_rate(p)
}

/// The tile's symbol coder. Normally every symbol goes into the range coder; while a decision
/// trials a candidate ([`super::FrameEncoder::trial_cost`]), `tally` is set and symbols are priced
/// into it instead — against the CDFs as they stand, which are then not adapted, so a trial leaves
/// both the coded stream and the CDFs untouched.
#[derive(Clone, Default)]
pub(super) struct Coder {
    pub(super) range: SymbolEncoder,
    /// The rate of the symbols trialled so far, in [`RATE_ONE`] units, or `None` when coding.
    pub(super) tally: Option<u32>,
}

impl Coder {
    /// Codes `symbol` against the adapting CDF `slot` (or prices it, in a trial).
    pub(super) fn encode(&mut self, symbol: usize, slot: cdf::CdfSlot<'_>) {
        match &mut self.tally {
            Some(rate) => *rate += symbol_rate(slot.probs(), symbol),
            None => cdf::encode(&mut self.range, symbol, slot),
        }
    }

    /// Codes `symbol` against a static CDF (or prices it, in a trial).
    pub(super) fn encode_symbol(&mut self, symbol: usize, cdf: &[u16]) {
        match &mut self.tally {
            Some(rate) => *rate += symbol_rate(cdf, symbol),
            None => self.range.encode_symbol(symbol, cdf),
        }
    }

    /// Codes the low `n` bits of `value` as equiprobable bits (or prices them, in a trial).
    pub(super) fn encode_literal(&mut self, value: u32, n: u32) {
        match &mut self.tally {
            Some(rate) => *rate += n * RATE_ONE,
            None => self.range.encode_literal(value, n),
        }
    }
}

/// Whether a block is coded into the stream or only trialled: reconstructed and priced so a
/// decision can compare it against the alternatives.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Coding {
    /// Code the symbols into the tile's range coder.
    Emit,
    /// Code nothing; reconstruct and price only.
    Trial,
    /// As [`Coding::Trial`], but with every transform `DCT_DCT` rather than searched: the cheap
    /// first pass of [`super::FrameEncoder::select_luma_mode`].
    Screen,
}

/// `NS(n)` non-symmetric literal (§4.10.7): codes a value in `0..n` with `floor(log2(n))` or +1 bits.
fn encode_ns(sym: &mut SymbolEncoder, v: u32, n: u32) {
    let w = (32 - n.leading_zeros()) as i32; // FloorLog2(n) + 1
    let m = (1u32 << w) - n;
    if v < m {
        sym.encode_literal(v, (w - 1) as u32);
    } else {
        let x = v + m;
        sym.encode_literal(x >> 1, (w - 1) as u32);
        sym.encode_literal(x & 1, 1);
    }
}

/// `v - mk`, the value coded once the sub-exponential prefix has narrowed the range to `[mk, …)`.
/// Shared between the two coding tails so it has a single mutation site: in the final-literal tail
/// `mk == 2^b2` and `encode_literal` keeps only the low `b2` bits, so there a `-`→`+` would be
/// invisible — but the `encode_ns` tail feeds it a raw value, where the same mutation is caught.
fn subexp_residual(v: u32, mk: u32) -> u32 {
    v - mk
}

/// `encode_subexp(v)` — the inverse of §4.10.8 `decode_subexp`: codes `v` in `0..num_syms` with a
/// sub-exponential prefix keyed by `k`.
fn encode_subexp(sym: &mut SymbolEncoder, v: u32, num_syms: u32, k: u32) {
    let (mut i, mut mk) = (0u32, 0u32);
    loop {
        let b2 = if i != 0 { k + i - 1 } else { k };
        let a = 1u32 << b2;
        if num_syms <= mk + 3 * a {
            encode_ns(sym, subexp_residual(v, mk), num_syms - mk);
            return;
        } else if v >= mk + a {
            sym.encode_literal(1, 1); // subexp_more_bits
            i += 1;
            mk += a;
        } else {
            sym.encode_literal(0, 1);
            sym.encode_literal(subexp_residual(v, mk), b2);
            return;
        }
    }
}

/// Recenters `v` around reference `r` (the inverse of §4.10.10 `inverse_recenter`).
fn recenter(r: i32, v: i32) -> u32 {
    if v > 2 * r {
        v as u32
    } else if v < r {
        (2 * (r - v) - 1) as u32
    } else {
        (2 * (v - r)) as u32
    }
}

/// `encode_unsigned_subexp_with_ref(v, mx, k, r)` (§4.10.9 inverse): codes `v` in `0..mx` against
/// reference `r` so that coding `v == r` emits the shortest (zero-delta) code.
pub(super) fn encode_subexp_with_ref(sym: &mut SymbolEncoder, v: i32, mx: i32, k: u32, r: i32) {
    let recentered = if (r << 1) <= mx {
        recenter(r, v)
    } else {
        recenter(mx - 1 - r, mx - 1 - v)
    };
    encode_subexp(sym, recentered, mx as u32, k);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn log2_rate_is_exact_at_powers_of_two_and_truncates_between() {
        for k in 0..16 {
            assert_eq!(log2_rate(1 << k), k * RATE_ONE);
        }
        // log2(3) = 1.58496…, log2(5) = 2.32193…, log2(24576) = 14.58496…, log2(32767) =
        // 14.99996…: eight fractional bits, truncated.
        assert_eq!(log2_rate(3), 405);
        assert_eq!(log2_rate(5), 594);
        assert_eq!(log2_rate(24576), 3733);
        assert_eq!(log2_rate(32767), 3839);
    }

    #[test]
    fn symbol_rate_is_minus_log2_of_the_symbols_probability() {
        // An even split costs one bit either way.
        assert_eq!(symbol_rate(&[16384, 32768], 0), RATE_ONE);
        assert_eq!(symbol_rate(&[16384, 32768], 1), RATE_ONE);
        // p = 1/4 is two bits; p = 3/4 is 0.415 bits (15 - log2(24576), truncated).
        assert_eq!(symbol_rate(&[8192, 32768], 0), 2 * RATE_ONE);
        assert_eq!(symbol_rate(&[8192, 32768], 1), 15 * RATE_ONE - 3733);
        // A middle symbol's probability is the step up to it, not its cumulative value.
        assert_eq!(symbol_rate(&[8192, 16384, 32768], 1), 2 * RATE_ONE);
        // A certain symbol is free, and an impossible one is priced as the least probable.
        assert_eq!(symbol_rate(&[32768, 32768], 0), 0);
        assert_eq!(symbol_rate(&[0, 32768], 0), 15 * RATE_ONE);
    }

    #[test]
    fn a_tallying_coder_prices_instead_of_coding() {
        let mut cdf = cdf::CdfContext::new(0);
        let mut coder = Coder {
            tally: Some(0),
            ..Coder::default()
        };
        coder.encode(1, cdf.skip[0].slot());
        coder.encode_symbol(0, &[8192, 32768]);
        coder.encode_literal(5, 3);
        let skip = symbol_rate(cdf::CdfContext::new(0).skip[0].probs(), 1);
        assert_eq!(coder.tally, Some(skip + 2 * RATE_ONE + 3 * RATE_ONE));
        // Nothing reached the range coder, and the CDF did not adapt.
        assert_eq!(coder.range.finish(), Coder::default().range.finish());
        assert_eq!(
            format!("{:?}", cdf.skip[0]),
            format!("{:?}", cdf::CdfContext::new(0).skip[0])
        );
    }

    #[test]
    fn recenter_codes_the_expected_value() {
        // recenter maps (ref, v) to the value encode_subexp actually codes; the dav1d oracle only sees
        // v == ref (a zero Wiener delta), so the `v > 2r` and `v < r` branches are otherwise untested.
        // Pin the exact coded value per branch. (A round-trip is too weak here: `inverse_recenter`
        // recovers v even from a wrongly-coded value, so it cannot see an output change.) The `>` at
        // v == 2*ref is a continuity point — both `>` and `>=` yield 2*ref — and is excluded as equivalent.
        for (r, v, want) in [
            (5, 12, 12u32), // v > 2r        -> v
            (5, 11, 11),    // v > 2r
            (5, 2, 5),      // v < r         -> 2(r-v)-1
            (5, 0, 9),      // v < r
            (5, 5, 0),      // v == r        -> 2(v-r)
            (5, 7, 4),      // r < v <= 2r   -> 2(v-r)
            (5, 10, 10),    // v == 2r       (continuity)
            (0, 3, 3),      // ref 0
            (3, 0, 5),      // v < r
        ] {
            assert_eq!(recenter(r, v), want, "recenter({r}, {v})");
        }
    }

    #[test]
    fn subexp_coding_is_stable() {
        // encode_subexp_with_ref drives recenter -> encode_subexp -> encode_ns (§4.10.7-9). The encoder
        // only codes a zero Wiener delta (v == ref), so every non-trivial path of this sub-exponential
        // coder is unexercised by the dav1d oracle. Pin a checksum of the coded bits for a spread of
        // (v, mx, k, ref) covering both reference halves, the more-bits loop, the NS escape and the
        // final literal. Any change to the coding moves the checksum; the v == ref path's correctness
        // is proven by the loop-restoration oracle, so the pair is deterministic AND correct.
        fn fnv1a(bytes: &[u8]) -> u64 {
            bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| {
                (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
            })
        }
        let mut sym = SymbolEncoder::new();
        // encode_ns directly: cover v < m, v == m and v > m (m = (1<<FloorLog2(n)+1) - n).
        for &(v, n) in &[
            (0u32, 5u32),
            (2, 5),
            (3, 5),
            (4, 5), // n=5: m=3
            (1, 3),
            (2, 3), // n=3: m=1
            (1, 7),
            (5, 7), // n=7: m=1
            (2, 6),
            (5, 6), // n=6: m=2
            (0, 1),
            (3, 8),
            (7, 8),
        ] {
            encode_ns(&mut sym, v, n);
        }
        // encode_subexp directly: cover the NS escape, the more-bits loop, and the final literal with a
        // non-zero `mk` (v=9,k=2 iterates twice then settles in the else with mk=8).
        for &(v, ns, k) in &[
            (0u32, 16u32, 1u32),
            (9, 64, 2),
            (3, 16, 0),
            (20, 64, 1),
            (1, 4, 0),
            (50, 64, 3),
            (7, 32, 2),
        ] {
            encode_subexp(&mut sym, v, ns, k);
        }
        // encode_subexp_with_ref: both reference halves ((r<<1) <= mx and not).
        for &(v, mx, k, r) in &[
            (0i32, 16i32, 1u32, 0i32),
            (5, 16, 1, 3),
            (20, 32, 1, 25),
            (62, 64, 2, 60),
            (33, 64, 0, 1),
            (2, 5, 1, 4),
        ] {
            encode_subexp_with_ref(&mut sym, v, mx, k, r);
        }
        assert_eq!(fnv1a(&sym.finish()), 0xd0fc_c97e_4152_8bd6);
    }
}
