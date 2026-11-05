//! AV1 multi-symbol arithmetic (range) coder (AV1 §8.2), both directions.
//!
//! The AV1 spec defines only the *decoder* (§8.2 "Parsing process for symbol decoder").
//! [`SymbolDecoder`] is a direct transcription of it; [`SymbolEncoder`] is the matching encoder,
//! derived by inverting it, producing a byte stream the §8.2 decoder maps back to the symbols that
//! were encoded. The arithmetic mirrors the well-known `od_ec` range coder (the same one in
//! libaom / rav1e), which is purpose-built for this decoder.
//!
//! CDF convention (matches §8.2.6): a CDF for `N` symbols is a slice of `N` cumulative values in
//! `[0, 32768]`, strictly non-decreasing, with `cdf[N - 1] == 32768`. `cdf[i]` is the cumulative
//! probability (× 32768) of symbols `0..=i`. Two coding modes are provided:
//! [`SymbolEncoder::encode_symbol`] codes against a *static* CDF (`disable_cdf_update = 1`), while
//! [`SymbolEncoder::encode_symbol_adapt`] applies the §8.2.6 adaptation after each symbol
//! (`disable_cdf_update = 0`), nudging the CDF toward the just-coded symbol. The adaptation counter
//! the spec keeps as a trailing `cdf[N]` element is carried alongside as a separate `&mut u16`; a
//! decoder must apply the identical update after each symbol to stay in lockstep.
//!
//! The two are each other's hermetic oracle: this module's round-trip tests prove the encoder
//! correct without any external decoder, and prove the decoder recovers exactly what was coded —
//! including that both sides' adapting CDFs stay bit-identical.

/// Number of bits to reduce CDF precision during arithmetic coding (AV1 `EC_PROB_SHIFT`, §3).
const EC_PROB_SHIFT: u32 = 6;
/// Minimum probability assigned to each symbol during arithmetic coding (AV1 `EC_MIN_PROB`, §3).
const EC_MIN_PROB: u32 = 4;
/// CDFs are expressed on a 1 << 15 scale (AV1 §8.2.6: `cdf[N - 1] == 1 << 15`).
const CDF_PROB_TOP: u32 = 1 << 15;
/// The fixed equiprobable CDF behind `read_bool()` / `read_literal(n)` (AV1 §8.2.3, §8.2.5).
const BOOL_CDF: [u16; 2] = [1 << 14, 1 << 15];

/// The od_ec scaled sub-interval width `(r >> 8) * (f >> EC_PROB_SHIFT) >> (7 - EC_PROB_SHIFT)`
/// shared by both branches of [`SymbolEncoder::encode_q15`] (AV1 §8.2.6). `f` is an inverse-CDF
/// bracket in `[0, 1 << 15]` and `r` the current range. Factored out so this non-obvious od_ec
/// term appears once instead of three times.
///
/// The `>> (7 - EC_PROB_SHIFT)` shift is `>> 1` (`EC_PROB_SHIFT == 6`); cargo-mutants' `- → /`
/// mutation of it (`7 / 6 == 7 - 6 == 1`) is therefore equivalent and is excluded in
/// `.cargo/mutants.toml`. Every other operator here is on the encode hot path and killed by the
/// round-trip tests.
#[inline]
fn ec_partition(r: u32, f: u32) -> u32 {
    ((r >> 8) * (f >> EC_PROB_SHIFT)) >> (7 - EC_PROB_SHIFT)
}

/// Encoder for the AV1 symbol (range) coder.
///
/// Feed symbols with [`SymbolEncoder::encode_symbol`] (CDF-coded) and equiprobable bits with
/// [`SymbolEncoder::encode_literal`], then call [`SymbolEncoder::finish`] to flush and obtain the
/// coded bytes. Those bytes are exactly what a decoder consumes via `init_symbol(sz)` (AV1 §8.2.2)
/// where `sz` is the returned length.
#[derive(Debug, Clone)]
pub struct SymbolEncoder {
    /// Low end of the coding interval, kept wider than 16 bits so carries accumulate losslessly
    /// (resolved in [`SymbolEncoder::finish`]).
    low: u64,
    /// Current range, renormalised into `[1 << 15, 1 << 16)`.
    rng: u32,
    /// Bit counter; starts at `-9` so the first carry/byte crosses zero at the right moment.
    cnt: i32,
    /// Output bytes, each held as a `u16` so a pending carry lives in bit 8 until `finish`.
    precarry: Vec<u16>,
}

impl Default for SymbolEncoder {
    fn default() -> Self {
        Self::new()
    }
}

impl SymbolEncoder {
    /// Creates an encoder with the initial range state of AV1's symbol coder.
    #[must_use]
    pub fn new() -> Self {
        Self {
            low: 0,
            rng: CDF_PROB_TOP,
            cnt: -9,
            precarry: Vec::new(),
        }
    }

    /// Encodes `symbol` against a static cumulative `cdf` (`cdf.len()` symbols, `cdf[last] == 32768`).
    ///
    /// # Panics
    ///
    /// Debug builds assert `symbol < cdf.len()` and the CDF normalisation invariants.
    pub fn encode_symbol(&mut self, symbol: usize, cdf: &[u16]) {
        let nsyms = cdf.len();
        debug_assert!(symbol < nsyms);
        debug_assert_eq!(u32::from(cdf[nsyms - 1]), CDF_PROB_TOP);
        // `f(j) = (1 << 15) - cdf[j]` is the inverse-CDF term used by the §8.2.6 decoder; `fl`/`fh`
        // bracket the chosen symbol's sub-interval. For symbol 0, the upper bracket is the full top.
        let fl = if symbol > 0 {
            CDF_PROB_TOP - u32::from(cdf[symbol - 1])
        } else {
            CDF_PROB_TOP
        };
        let fh = CDF_PROB_TOP - u32::from(cdf[symbol]);
        self.encode_q15(fl, fh, symbol as u32, nsyms as u32);
    }

    /// Encodes `symbol` against an *adapting* cumulative `cdf`, then applies the §8.2.6 CDF
    /// adaptation in place (`disable_cdf_update = 0`). `count` is the spec's trailing `cdf[N]`
    /// adaptation counter (start it at 0 for a freshly initialised context); it is bumped here, up to
    /// a maximum of 32. A decoder must apply the identical §8.2.6 update (decode the symbol, then the
    /// same adaptation) with the same `count` so its CDF tracks the encoder's exactly.
    ///
    /// # Panics
    ///
    /// Debug builds assert `symbol < cdf.len()` and the CDF normalisation invariants.
    pub fn encode_symbol_adapt(&mut self, symbol: usize, cdf: &mut [u16], count: &mut u16) {
        self.encode_symbol(symbol, cdf);
        update_cdf(cdf, symbol, count);
    }

    /// Encodes the low `n` bits of `value` as equiprobable bits, most-significant bit first.
    ///
    /// This is the inverse of the decoder's `read_literal(n)` (AV1 §8.2.5), which itself calls
    /// `read_bool()` (§8.2.3) with the fixed CDF `{1 << 14, 1 << 15}`.
    pub fn encode_literal(&mut self, value: u32, n: u32) {
        for i in (0..n).rev() {
            self.encode_symbol(((value >> i) & 1) as usize, &BOOL_CDF);
        }
    }

    /// Core interval update for one symbol; `fl`/`fh` are the inverse-CDF brackets, `s` the symbol,
    /// `nsyms` the alphabet size. Mirrors `od_ec_encode_q15`, which inverts the §8.2.6 boundaries.
    fn encode_q15(&mut self, fl: u32, fh: u32, s: u32, nsyms: u32) {
        let mut low = self.low;
        let mut r = self.rng;
        debug_assert!(r >= CDF_PROB_TOP);
        let n = nsyms - 1;
        if fl < CDF_PROB_TOP {
            let u = ec_partition(r, fl) + EC_MIN_PROB * (n - (s - 1));
            let v = ec_partition(r, fh) + EC_MIN_PROB * (n - s);
            debug_assert!(u <= r && v < u);
            low += u64::from(r - u);
            r = u - v;
        } else {
            // Symbol 0: the interval reaches the top, so `low` is unchanged.
            let v = ec_partition(r, fh) + EC_MIN_PROB * (n - s);
            debug_assert!(v < r);
            r -= v;
        }
        self.normalize(low, r);
    }

    /// Renormalises `(low, rng)` back into `[1 << 15, 1 << 16)`, emitting completed bytes into
    /// `precarry`. Mirrors `od_ec_enc_normalize`.
    fn normalize(&mut self, mut low: u64, rng: u32) {
        // `d` = number of left shifts to bring `rng` to 16 bits. `rng` is in `[1, 0xFFFF]` here.
        let d = rng.leading_zeros() - 16;
        let mut c = self.cnt;
        let mut s = c + d as i32;
        if s >= 0 {
            c += 16;
            let mut m = (1u64 << c) - 1;
            if s >= 8 {
                self.precarry.push((low >> c) as u16);
                low &= m;
                c -= 8;
                m = (1u64 << c) - 1;
            }
            self.precarry.push((low >> c) as u16);
            s = c + d as i32 - 24;
            low &= m;
        }
        self.low = low << d;
        self.rng = rng << d;
        self.cnt = s;
    }

    /// Flushes the coder and returns the coded bytes. Mirrors `od_ec_enc_done`: it emits the
    /// minimum number of bits that decode correctly regardless of trailing padding, then resolves
    /// the buffered carries into a big-endian byte stream.
    #[must_use]
    pub fn finish(mut self) -> Vec<u8> {
        let l = self.low;
        let mut c = self.cnt;
        let mut s = 10 + c;
        let m: u64 = 0x3FFF;
        let mut e = ((l + m) & !m) | (m + 1);
        if s > 0 {
            let mut n = (1u64 << (c + 16)) - 1;
            loop {
                self.precarry.push((e >> (c + 16)) as u16);
                e &= n;
                s -= 8;
                c -= 8;
                n >>= 8;
                if s <= 0 {
                    break;
                }
            }
        }
        // Resolve carries from least- to most-significant byte (big-endian output).
        let mut out = vec![0u8; self.precarry.len()];
        let mut carry: u32 = 0;
        for i in (0..self.precarry.len()).rev() {
            let val = u32::from(self.precarry[i]) + carry;
            out[i] = (val & 0xff) as u8;
            carry = val >> 8;
        }
        out
    }
}

/// Adapts a cumulative `cdf` toward the just-coded `symbol` and bumps the adaptation `count`, per
/// AV1 §8.2.6 (`disable_cdf_update = 0`). `cdf` is the gamut `N`-entry cumulative form
/// (`cdf[N - 1] == 32768`, which is never touched); `count` is the spec's trailing `cdf[N]`
/// counter, capped at 32 — a higher count slows adaptation. The encoder and a conformant decoder
/// invoke this identically after coding each symbol, so their CDFs evolve in lockstep.
fn update_cdf(cdf: &mut [u16], symbol: usize, count: &mut u16) {
    // §8.2.6 with the loop's `tmp` 0/32768 split made explicit. The fixed top entry
    // (cdf[N - 1] == 32768) is never updated; entries before `symbol` move toward 0 and those from
    // `symbol` up to N - 2 move toward 32768, each by `delta >> rate`.
    //
    // An empty CDF codes nothing, so there is nothing to adapt — and no `FloorLog2(N)` either.
    let Some((_top, body)) = cdf.split_last_mut() else {
        return;
    };
    let n = body.len() + 1;
    // rate = 3 + (count > 15) + (count > 31) + Min(FloorLog2(N), 2).
    let rate = 3
        + u32::from(*count > 15)
        + u32::from(*count > 31)
        + (31 - (n as u32).leading_zeros()).min(2);
    for v in &mut body[..symbol] {
        *v -= *v >> rate;
    }
    for v in &mut body[symbol..] {
        *v += ((1u16 << 15) - *v) >> rate;
    }
    if *count < 32 {
        *count += 1;
    }
}

/// Decoder for the AV1 symbol (range) coder — the parsing process of AV1 §8.2.
///
/// This is the normative side of the pair: [`SymbolEncoder`] exists to produce bytes this maps
/// back to symbols. Construct with [`SymbolDecoder::new`] over one tile's coded bytes
/// (`init_symbol(sz)`, §8.2.2), then read with [`SymbolDecoder::decode_symbol`] (static CDFs,
/// `disable_cdf_update = 1`) or [`SymbolDecoder::decode_symbol_adapt`] (§8.2.6 adaptation,
/// `disable_cdf_update = 0`), matching whatever the encoder did.
///
/// Reads past the end of `data` yield zero bits, exactly as §8.2.2 specifies — a tile's final
/// symbols are decodable from a truncated final byte, so this is normative behaviour, not
/// leniency. Use [`SymbolDecoder::exit_symbol`] at the end of a tile to check the padding
/// invariant §8.2.4 requires.
///
/// ```
/// use gamut_bitstream::{SymbolDecoder, SymbolEncoder};
///
/// let cdf = [16384u16, 32768];
/// let mut enc = SymbolEncoder::new();
/// enc.encode_symbol(1, &cdf);
/// enc.encode_symbol(0, &cdf);
/// let bytes = enc.finish();
///
/// let mut dec = SymbolDecoder::new(&bytes);
/// assert_eq!(dec.decode_symbol(&cdf), 1);
/// assert_eq!(dec.decode_symbol(&cdf), 0);
/// ```
#[derive(Debug, Clone)]
pub struct SymbolDecoder<'a> {
    /// The coded bytes for this tile.
    data: &'a [u8],
    /// Read cursor in bits; may advance past `data.len() * 8`, where reads yield zeroes.
    bit_pos: usize,
    /// The spec's `SymbolValue`.
    value: u32,
    /// The spec's `SymbolRange`, renormalised into `[1 << 15, 1 << 16)`.
    range: u32,
    /// The spec's `SymbolMaxBits`; goes negative once the padding region is reached.
    max_bits: i64,
}

impl<'a> SymbolDecoder<'a> {
    /// Initialises the decoder over one tile's coded bytes (`init_symbol(sz)`, AV1 §8.2.2).
    #[must_use]
    pub fn new(data: &'a [u8]) -> Self {
        let sz = data.len();
        let mut d = Self {
            data,
            bit_pos: 0,
            value: 0,
            range: CDF_PROB_TOP,
            max_bits: 8 * sz as i64 - 15,
        };
        let num_bits = core::cmp::min(sz * 8, 15) as u32;
        let buf = d.read_f(num_bits);
        let padded = buf << (15 - num_bits);
        d.value = (CDF_PROB_TOP - 1) ^ padded;
        d
    }

    /// `f(n)` for the symbol decoder (AV1 §8.1): MSB-first, **zero-padded past the end of the
    /// data**, which §8.2.2 relies on for a tile's trailing symbols.
    fn read_f(&mut self, n: u32) -> u32 {
        let mut x = 0u32;
        for _ in 0..n {
            let idx = self.bit_pos >> 3;
            let bit = if idx < self.data.len() {
                (self.data[idx] >> (7 - (self.bit_pos & 7))) & 1
            } else {
                0
            };
            // `+`, not `|`: the shift vacates exactly the bit added, so the two agree on every
            // input, and `|` carried an equivalent `| -> ^` mutant (removed rather than excluded;
            // the `+` mutants are each killable).
            x = (x << 1) + u32::from(bit);
            self.bit_pos += 1;
        }
        x
    }

    /// Decodes one symbol against a static cumulative `cdf` (`read_symbol`, AV1 §8.2.6).
    ///
    /// `cdf` is the gamut `N`-entry cumulative form (`cdf[N - 1] == 32768`); the adaptation
    /// counter the spec keeps as a trailing `cdf[N]` is **not** part of it, and must not be
    /// appended. Use [`SymbolDecoder::decode_symbol_adapt`] when `disable_cdf_update` is 0.
    ///
    /// The returned symbol is always below `cdf.len()`, and an empty `cdf` decodes nothing and
    /// returns 0: a CDF that breaks the convention yields a meaningless symbol, never a panic or a
    /// read past the slice.
    pub fn decode_symbol(&mut self, cdf: &[u16]) -> usize {
        // The search is bounded by the alphabet, not by the CDF's contents: `cdf[N - 1] == 32768`
        // is what would otherwise force the last bracket to 0 and end the loop, and a caller can
        // hand over a CDF that does not honour it — the spec's §8.2.6 form, for one, carries the
        // adaptation counter as a trailing `cdf[N]`, which gamut keeps separate. Treating the last
        // symbol as forced makes the search total for any slice instead of running off its end.
        let Some(last) = cdf.len().checked_sub(1) else {
            return 0;
        };
        // `prev` is the previous symbol's bracket, `self.range` before the first; `cur` is the
        // chosen symbol's, which is 0 at the forced last symbol.
        let mut prev = self.range;
        let mut symbol = last;
        let mut cur = 0;
        for (i, &entry) in cdf.iter().enumerate().take(last) {
            // `f(i) = (1 << 15) - cdf[i]`, saturating so an out-of-convention entry above 32768
            // cannot wrap; clamped to `prev` so the brackets stay ordered whatever the CDF says.
            let f = CDF_PROB_TOP.saturating_sub(u32::from(entry));
            let bracket = (ec_partition(self.range, f) + EC_MIN_PROB * (last - i) as u32).min(prev);
            if self.value >= bracket {
                symbol = i;
                cur = bracket;
                break;
            }
            prev = bracket;
        }
        self.range = prev - cur;
        self.value -= cur;
        self.renormalize();
        symbol
    }

    /// Renormalises `(value, range)` back into `[1 << 15, 1 << 16)` (AV1 §8.2.6, ordered steps).
    fn renormalize(&mut self) {
        let bits = 15 - (31 - self.range.leading_zeros());
        self.range <<= bits;
        let num_bits = core::cmp::min(i64::from(bits), self.max_bits.max(0)) as u32;
        let new_data = self.read_f(num_bits);
        let padded = new_data << (bits - num_bits);
        self.value = padded ^ (((self.value + 1) << bits) - 1);
        self.max_bits -= i64::from(bits);
    }

    /// Decodes one symbol against an *adapting* cumulative `cdf`, then applies the §8.2.6 CDF
    /// adaptation in place — the exact mirror of [`SymbolEncoder::encode_symbol_adapt`].
    ///
    /// `count` is the spec's trailing `cdf[N]` adaptation counter; start it at 0 for a freshly
    /// initialised context and keep it beside the CDF so the decoder's contexts evolve in
    /// lockstep with the encoder's.
    pub fn decode_symbol_adapt(&mut self, cdf: &mut [u16], count: &mut u16) -> usize {
        let s = self.decode_symbol(cdf);
        update_cdf(cdf, s, count);
        s
    }

    /// Decodes one equiprobable bit (`read_bool()`, AV1 §8.2.3).
    pub fn decode_bool(&mut self) -> bool {
        self.decode_symbol(&BOOL_CDF) != 0
    }

    /// Decodes `n` equiprobable bits, most-significant first (`read_literal(n)`, AV1 §8.2.5).
    pub fn decode_literal(&mut self, n: u32) -> u32 {
        let mut x = 0;
        for _ in 0..n {
            // `+`, not `|`, as in `read_f`: the operands are disjoint, so `|` carried an
            // equivalent `| -> ^` mutant (removed rather than excluded).
            x = (x << 1) + u32::from(self.decode_bool());
        }
        x
    }

    /// Decodes an unsigned value in `0..n`: the symbol-coded form of `ns(n)` (AV1 §4.10.7).
    ///
    /// Returns 0 for `n == 0` rather than reading anything, so a caller that derives `n` from
    /// other syntax cannot spin on a degenerate range.
    pub fn decode_ns(&mut self, n: u32) -> u32 {
        if n == 0 {
            return 0;
        }
        let w = 32 - n.leading_zeros();
        // As in [`crate::BitReader::ns`]: `w` reaches 32 once `n >= 2^31`, so the spec arithmetic
        // is evaluated in u64 to keep `1 << w` off the u32 type width. Every result is `< n`.
        let m = (1u64 << w) - u64::from(n);
        let v = u64::from(self.decode_literal(w - 1));
        if v < m {
            return v as u32;
        }
        ((v << 1) - m + u64::from(self.decode_literal(1))) as u32
    }

    /// Decodes `L(n)`-style unsigned data whose width is itself coded — the `decode_unsigned`
    /// helper AV1 §5.9.13 and §5.11 use for `delta_q` and `delta_lf` magnitudes.
    ///
    /// Reads increasing unary-coded prefixes and then the remainder, capped at `max_bits` so a
    /// hostile stream cannot drive an unbounded loop.
    ///
    /// `max_bits` must be `<= 31`.
    ///
    /// # Panics
    ///
    /// Debug builds panic when `max_bits > 31`: the prefix length reaches `max_bits`, and the
    /// `1 << length` reconstructing the value is then a u32 shift at or past the type width
    /// (release builds mask the shift instead). No saturating behaviour is defined for that
    /// case — AV1 gives this helper no bound to saturate against, so the domain is left to the
    /// caller rather than invented here.
    pub fn decode_golomb(&mut self, max_bits: u32) -> u32 {
        let mut length = 0u32;
        while length < max_bits && !self.decode_bool() {
            length += 1;
        }
        if length == 0 {
            return 1;
        }
        let rest = self.decode_literal(length);
        (1u32 << length) + rest
    }

    /// The bit at `pos`, zero past the end of `data` — the same padding [`Self::read_f`] reads.
    const fn bit_at(&self, pos: usize) -> u8 {
        if pos >= self.data.len() * 8 {
            return 0;
        }
        (self.data[pos >> 3] >> (7 - (pos & 7))) & 1
    }

    /// The `exit_symbol()` check of AV1 §8.2.4, run at the end of a tile.
    ///
    /// Returns `false` for a tile that violates any of the section's three conformance
    /// requirements: `SymbolMaxBits >= -14`, a `1` bit at `trailingBitPosition`, and zeroes
    /// strictly between there and `paddingEndPosition`. The symbol decoder itself is total (it
    /// pads with zeroes past the end of the data), so this is the only place a tile's framing is
    /// validated.
    #[must_use]
    pub fn exit_symbol(&self) -> bool {
        // §8.2.4's first conformance requirement. Below -14 the decoder has invented more padding
        // than the trailing bits can account for, and `trailingBitPosition` is not even defined.
        if self.max_bits < -14 {
            return false;
        }
        // trailingBitPosition = get_position() - Min(15, SymbolMaxBits + 15). The Min is in
        // 1..=15 here, and `bit_pos` is always at least that far in: `init_symbol` reads
        // `Min(8 * sz, 15)` bits up front (which is
        // `Min(15, SymbolMaxBits + 15)` at that moment) and renormalisation only ever adds to it,
        // so the saturation never binds.
        let back = core::cmp::min(15, self.max_bits + 15) as usize;
        let trailing = self.bit_pos.saturating_sub(back);
        // The position indicator then advances by Max(0, SymbolMaxBits), which lands exactly on
        // the end of the tile's bytes: `get_position() + SymbolMaxBits` is `8 * sz` for as long as
        // SymbolMaxBits is non-negative, and once it goes negative `get_position()` has already
        // stopped at `8 * sz` because renormalisation reads `Min(bits, Max(0, SymbolMaxBits))`.
        let padding_end = self.data.len() * 8;
        // The trailing bit closes the tile's arithmetic; an all-zero tail is non-conformant, not
        // merely padding.
        if self.bit_at(trailing) != 1 {
            return false;
        }
        for pos in trailing + 1..padding_end {
            if self.bit_at(pos) != 0 {
                return false;
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Small deterministic LCG so tests are reproducible without `rand`.
    struct Lcg(u64);
    impl Lcg {
        fn next_u32(&mut self) -> u32 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (self.0 >> 32) as u32
        }
        fn below(&mut self, bound: u32) -> u32 {
            self.next_u32() % bound
        }
    }

    /// Builds a random strictly-increasing cumulative CDF for `nsyms` symbols, `cdf[last] = 32768`.
    fn random_cdf(rng: &mut Lcg, nsyms: usize) -> Vec<u16> {
        // Pick `nsyms - 1` distinct breakpoints in 1..32768, sorted, then append 32768.
        let mut points = Vec::new();
        while points.len() < nsyms - 1 {
            let p = 1 + rng.below(32767) as u16;
            if !points.contains(&p) {
                points.push(p);
            }
        }
        points.sort_unstable();
        points.push(32768);
        points
    }

    /// Encodes `value` in `0..n` with the `ns()` inverse (AV1 §4.10.7), in u64 so the
    /// `n >= 2^31` cases are expressible at all.
    fn encode_ns(enc: &mut SymbolEncoder, value: u32, n: u32) {
        let w = 32 - n.leading_zeros();
        let m = (1u64 << w) - u64::from(n);
        let value = u64::from(value);
        if value < m {
            enc.encode_literal(value as u32, w - 1);
        } else {
            enc.encode_literal((value + m) as u32, w);
        }
    }

    /// Round-trips one `(value, n)` pair through the symbol coder.
    fn check_decode_ns(value: u32, n: u32) {
        let mut enc = SymbolEncoder::new();
        encode_ns(&mut enc, value, n);
        let bytes = enc.finish();
        let mut dec = SymbolDecoder::new(&bytes);
        assert_eq!(dec.decode_ns(n), value, "decode_ns n={n} v={value}");
    }

    #[test]
    fn decode_ns_matches_the_spec_definition() {
        // Every value in 0..n for a range that exercises both branches.
        for n in 1u32..=17 {
            for value in 0..n {
                check_decode_ns(value, n);
            }
        }

        // `n == 0` reads nothing and yields 0 rather than deriving a width from an empty range.
        let bytes = SymbolEncoder::new().finish();
        let mut dec = SymbolDecoder::new(&bytes);
        assert_eq!(dec.decode_ns(0), 0);
    }

    #[test]
    fn decode_ns_spans_the_whole_u32_range() {
        // `n >= 2^31` drives `w` to 32, where `(1 << w)` is a full-width u32 shift. Same
        // boundary probes as `BitReader::ns`: the short branch, the crossing at `m`, and the
        // maximum value.
        for value in [0, 1, (1u32 << 31) - 1] {
            check_decode_ns(value, 1u32 << 31);
        }
        for value in [0, (1u32 << 31) - 2, (1u32 << 31) - 1, 1u32 << 31] {
            check_decode_ns(value, (1u32 << 31) + 1);
        }
        for value in [0, 1, 2, u32::MAX - 1] {
            check_decode_ns(value, u32::MAX);
        }
    }

    #[test]
    fn empty_stream_roundtrips() {
        let enc = SymbolEncoder::new();
        let bytes = enc.finish();
        // Nothing to decode; just ensure init does not panic.
        let _ = SymbolDecoder::new(&bytes);
    }

    #[test]
    fn single_symbol_streams_roundtrip() {
        // Exhaustively exercise small alphabets with a skewed CDF and every symbol value.
        for nsyms in 2..=12usize {
            let mut cdf: Vec<u16> = (1..nsyms).map(|i| (i * 32768 / nsyms) as u16).collect();
            cdf.push(32768);
            for s in 0..nsyms {
                let mut enc = SymbolEncoder::new();
                enc.encode_symbol(s, &cdf);
                let bytes = enc.finish();
                let mut dec = SymbolDecoder::new(&bytes);
                assert_eq!(dec.decode_symbol(&cdf), s, "nsyms={nsyms} s={s}");
            }
        }
    }

    #[test]
    fn long_random_symbol_stream_roundtrips() {
        let mut rng = Lcg(0x1234_5678_9abc_def0);
        // Pre-generate a mix of CDFs of varying sizes.
        let cdfs: Vec<Vec<u16>> = (2..=14).map(|n| random_cdf(&mut rng, n)).collect();
        let mut events = Vec::new();
        let mut enc = SymbolEncoder::new();
        for _ in 0..20_000 {
            let cdf = &cdfs[rng.below(cdfs.len() as u32) as usize];
            let s = rng.below(cdf.len() as u32) as usize;
            enc.encode_symbol(s, cdf);
            events.push((s, cdf.clone()));
        }
        let bytes = enc.finish();
        let mut dec = SymbolDecoder::new(&bytes);
        for (i, (s, cdf)) in events.iter().enumerate() {
            assert_eq!(dec.decode_symbol(cdf), *s, "event {i}");
        }
    }

    /// Writes the bit pattern `decode_golomb` consumes for `value`: a unary prefix of
    /// `length` zero bools, a terminating one bool, then the `length`-bit remainder.
    ///
    /// The prefix is written out explicitly rather than by calling a `write_golomb` helper --
    /// there is no such helper, and inventing one would make the tests below a round trip against
    /// my own encoder instead of against the layout AV1 section 5.9.13 defines.
    fn encode_golomb(enc: &mut SymbolEncoder, value: u32) {
        assert!(value >= 1, "decode_golomb never returns 0");
        let length = 31 - value.leading_zeros();
        for _ in 0..length {
            enc.encode_symbol(0, &BOOL_CDF);
        }
        enc.encode_symbol(1, &BOOL_CDF);
        if length > 0 {
            enc.encode_literal(value - (1 << length), length);
        }
    }

    #[test]
    fn read_f_pads_with_zeros_past_the_end_of_the_data() {
        // `read_f` documents the window as zero-padded past the end of the data, which AV1
        // section 8.2.2 relies on for a tile's trailing symbols.
        //
        // No public entry point reaches that branch: `new` is the only caller and it caps its
        // priming read at `min(8 * len, 15)` bits, so the byte index never reaches `data.len()`.
        // The behaviour is documented and the bounds check is real, so it is pinned here -- from
        // inside the crate, which is the only place a private method's unreached branch can be
        // reached at all.
        let mut dec = SymbolDecoder::new(&[0xFF]);

        dec.bit_pos = 0;
        assert_eq!(dec.read_f(8), 0xFF, "the one real byte reads back");

        // Now past it: further bits read as zero instead of indexing out of bounds.
        assert_eq!(dec.read_f(16), 0);
        assert_eq!(
            dec.bit_pos, 24,
            "the cursor still advances over the padding"
        );
    }

    #[test]
    fn a_decoder_over_an_empty_buffer_behaves_like_one_over_zeros() {
        // The public consequence of that padding: a tile with no bytes at all must decode as one
        // whose bytes are zero, rather than diverging or panicking.
        let mut empty = SymbolDecoder::new(&[]);
        let mut zeros = SymbolDecoder::new(&[0u8; 32]);

        for _ in 0..64 {
            assert_eq!(empty.decode_literal(4), zeros.decode_literal(4));
        }
    }

    #[test]
    fn decode_golomb_returns_one_when_the_prefix_stops_immediately() {
        // A terminating bool with no preceding zeros is length 0, which the decoder shortcuts to
        // 1 rather than reading a zero-width remainder.
        let mut enc = SymbolEncoder::new();
        enc.encode_symbol(1, &BOOL_CDF);
        let bytes = enc.finish();

        let mut dec = SymbolDecoder::new(&bytes);
        assert_eq!(dec.decode_golomb(8), 1);
    }

    #[test]
    fn decode_golomb_reconstructs_the_value_from_its_prefix_and_remainder() {
        // Every value whose prefix fits in max_bits, including both ends of each prefix length --
        // 2^k is a zero remainder, 2^(k+1) - 1 an all-ones one, which is where a `+` mutated to
        // `*` or `-` and a `<<` mutated to `>>` diverge.
        let values: Vec<u32> = (1..=64)
            .chain([100, 127, 128, 255, 256, 1000, 1023, 1024])
            .collect();

        let mut enc = SymbolEncoder::new();
        for &v in &values {
            encode_golomb(&mut enc, v);
        }
        let bytes = enc.finish();

        let mut dec = SymbolDecoder::new(&bytes);
        for &v in &values {
            assert_eq!(dec.decode_golomb(16), v, "golomb round trip failed for {v}");
        }
    }

    #[test]
    fn decode_golomb_stops_counting_the_prefix_at_max_bits() {
        // A prefix longer than max_bits must be capped rather than counted on: the cap is what
        // stops a hostile stream driving the loop. With max_bits = 4 the decoder consumes exactly
        // four zero bools, never looks for a terminator, and reads a 4-bit remainder -- so the
        // fifth bool it would otherwise have consumed is instead the top bit of that remainder.
        const MAX_BITS: u32 = 4;
        let mut enc = SymbolEncoder::new();
        for _ in 0..MAX_BITS {
            enc.encode_symbol(0, &BOOL_CDF);
        }
        enc.encode_literal(0b1011, MAX_BITS);
        let bytes = enc.finish();

        let mut dec = SymbolDecoder::new(&bytes);
        assert_eq!(dec.decode_golomb(MAX_BITS), (1 << MAX_BITS) + 0b1011);
    }

    #[test]
    fn decode_golomb_caps_the_prefix_even_when_the_stream_never_terminates_it() {
        // The bound exists for a hostile stream. An all-zero buffer decodes to an endless run of
        // zero bools, and the decoder must still return: `length` stops at max_bits.
        let mut dec = SymbolDecoder::new(&[0u8; 16]);

        let value = dec.decode_golomb(4);
        assert!(
            value >= 1 << 4,
            "prefix was not counted to the cap: {value}"
        );
        assert!(value < 1 << 5, "prefix ran past the cap: {value}");
    }

    #[test]
    fn literals_roundtrip() {
        let mut rng = Lcg(0xdead_beef_0bad_f00d);
        let mut enc = SymbolEncoder::new();
        let mut events = Vec::new();
        for _ in 0..5000 {
            let n = 1 + rng.below(16);
            let v = rng.next_u32() & ((1u32 << n) - 1);
            enc.encode_literal(v, n);
            events.push((v, n));
        }
        let bytes = enc.finish();
        let mut dec = SymbolDecoder::new(&bytes);
        for (v, n) in events {
            assert_eq!(dec.decode_literal(n), v);
        }
    }

    #[test]
    fn mixed_symbols_and_literals_roundtrip() {
        let mut rng = Lcg(0x0f0f_0f0f_1234_9999);
        let cdf = random_cdf(&mut rng, 8);
        let mut enc = SymbolEncoder::new();
        let mut events: Vec<(bool, u32)> = Vec::new(); // (is_literal, payload)
        for _ in 0..8000 {
            if rng.next_u32() & 1 == 0 {
                let s = rng.below(cdf.len() as u32);
                enc.encode_symbol(s as usize, &cdf);
                events.push((false, s));
            } else {
                let v = rng.next_u32() & 0xff;
                enc.encode_literal(v, 8);
                events.push((true, v));
            }
        }
        let bytes = enc.finish();
        let mut dec = SymbolDecoder::new(&bytes);
        for (is_lit, payload) in events {
            if is_lit {
                assert_eq!(dec.decode_literal(8), payload);
            } else {
                assert_eq!(dec.decode_symbol(&cdf) as u32, payload);
            }
        }
    }

    #[test]
    fn update_cdf_matches_spec_formula() {
        // Hand-computed from AV1 §8.2.6: rate = 3 + (count > 15) + (count > 31) + Min(FloorLog2(N), 2),
        // then each entry before `symbol` moves toward 0 and each from `symbol` on moves toward 32768
        // by `delta >> rate`. The round-trip tests below cannot pin this — the encoder and decoder
        // adapt in lockstep even with a wrong-but-symmetric formula — so the exact values are checked
        // here directly.
        fn upd(cdf: &[u16], symbol: usize, count: u16) -> (Vec<u16>, u16) {
            let mut c = cdf.to_vec();
            let mut n = count;
            update_cdf(&mut c, symbol, &mut n);
            (c, n)
        }
        // N = 2 (FloorLog2 = 1). The count thresholds 15 and 31 each step the rate.
        assert_eq!(upd(&[16384, 32768], 0, 0), (vec![17408, 32768], 1)); // rate 4: +(16384 >> 4)
        assert_eq!(upd(&[16384, 32768], 1, 0), (vec![15360, 32768], 1)); // rate 4: -(16384 >> 4)
        assert_eq!(upd(&[16384, 32768], 0, 15), (vec![17408, 32768], 16)); // 15 > 15 false ⇒ rate 4
        assert_eq!(upd(&[16384, 32768], 0, 16), (vec![16896, 32768], 17)); // 16 > 15 true  ⇒ rate 5
        assert_eq!(upd(&[16384, 32768], 0, 31), (vec![16896, 32768], 32)); // 31 > 31 false ⇒ rate 5
        assert_eq!(upd(&[16384, 32768], 0, 32), (vec![16640, 32768], 32)); // 32 > 31 true ⇒ rate 6, count saturates
        // N = 3 (FloorLog2 = 1): a mid-symbol update with count 20.
        assert_eq!(
            upd(&[10000, 20000, 32768], 1, 20),
            (vec![9688, 20399, 32768], 21)
        ); // rate 5
        // N = 8 (FloorLog2 = 3, capped to 2) pins Min(.., 2) and the full sweep.
        assert_eq!(
            upd(
                &[4096, 8192, 12288, 16384, 20480, 24576, 28672, 32768],
                3,
                0
            ),
            (
                vec![3968, 7936, 11904, 16896, 20864, 24832, 28800, 32768],
                1
            ) // rate 5
        );
    }

    #[test]
    fn adaptive_single_cdf_roundtrips() {
        // Encode a long, skewed stream against one adapting CDF. The decoder, starting from the same
        // initial CDF and applying the identical update, must recover every symbol and end with a
        // byte-identical CDF + count.
        let mut rng = Lcg(0xa1b2_c3d4_e5f6_0719);
        let init = random_cdf(&mut rng, 6);
        let mut enc = SymbolEncoder::new();
        let mut ecdf = init.clone();
        let mut ecount = 0u16;
        let mut syms = Vec::new();
        for _ in 0..10_000 {
            // Skew toward symbol 0 so the CDF moves substantially.
            let s = (rng.below(6) * rng.below(2)) as usize;
            enc.encode_symbol_adapt(s, &mut ecdf, &mut ecount);
            syms.push(s);
        }
        let bytes = enc.finish();
        let mut dec = SymbolDecoder::new(&bytes);
        let mut dcdf = init.clone();
        let mut dcount = 0u16;
        for (i, &s) in syms.iter().enumerate() {
            assert_eq!(
                dec.decode_symbol_adapt(&mut dcdf, &mut dcount),
                s,
                "event {i}"
            );
        }
        assert_eq!(ecdf, dcdf, "encoder/decoder CDFs diverged");
        assert_eq!(ecount, dcount);
        assert_ne!(
            ecdf, init,
            "CDF should have adapted away from its initial state"
        );
    }

    #[test]
    fn adaptive_multi_context_roundtrips() {
        // Several independent adapting contexts, interleaved — the realistic usage where each syntax
        // element has its own CDF + count and they must not cross-contaminate.
        let mut rng = Lcg(0x0011_2233_4455_6677);
        let inits: Vec<Vec<u16>> = (2..=10).map(|n| random_cdf(&mut rng, n)).collect();
        let mut enc = SymbolEncoder::new();
        let mut ecdfs = inits.clone();
        let mut ecounts = vec![0u16; inits.len()];
        let mut events = Vec::new();
        for _ in 0..15_000 {
            let ctx = rng.below(inits.len() as u32) as usize;
            let s = rng.below(ecdfs[ctx].len() as u32) as usize;
            enc.encode_symbol_adapt(s, &mut ecdfs[ctx], &mut ecounts[ctx]);
            events.push((ctx, s));
        }
        let bytes = enc.finish();
        let mut dec = SymbolDecoder::new(&bytes);
        let mut dcdfs = inits.clone();
        let mut dcounts = vec![0u16; inits.len()];
        for (i, &(ctx, s)) in events.iter().enumerate() {
            assert_eq!(
                dec.decode_symbol_adapt(&mut dcdfs[ctx], &mut dcounts[ctx]),
                s,
                "event {i} ctx {ctx}"
            );
        }
        assert_eq!(ecdfs, dcdfs);
        assert_eq!(ecounts, dcounts);
    }

    #[test]
    fn zero_probability_leading_symbol_roundtrips() {
        // A CDF whose first entry is 0 gives symbol 0 zero probability. Encoding a *later* symbol
        // then drives `encode_q15`'s `fl == CDF_PROB_TOP` (else) branch with `s != 0` — the only
        // path where the `EC_MIN_PROB * (n - s)` partition term is observable (an ordinary
        // strictly-increasing CDF reaches that branch only at s == 0, where `n - s == n + s`).
        // A wrong term perturbs the range by only `EC_MIN_PROB`-sized amounts per symbol, so encode
        // a long stream: the encoder/decoder divergence compounds until a symbol misdecodes. This
        // kills `n - s` → `n + s` (and `→ /`) in the else branch.
        let cdf = [0u16, 16384, 32768];
        let mut rng = Lcg(0x9e37_79b9_7f4a_7c15);
        let stream: Vec<usize> = (0..4000).map(|_| 1 + rng.below(2) as usize).collect();
        let mut enc = SymbolEncoder::new();
        for &s in &stream {
            enc.encode_symbol(s, &cdf);
        }
        let bytes = enc.finish();
        let mut dec = SymbolDecoder::new(&bytes);
        for (i, &s) in stream.iter().enumerate() {
            assert_eq!(dec.decode_symbol(&cdf), s, "event {i}");
        }
    }

    #[test]
    fn a_cdf_in_the_specs_n_plus_1_form_cannot_read_past_its_end() {
        // §8.2.6 keeps the adaptation counter as a trailing `cdf[N]`, so a caller transcribing the
        // spec literally appends a 0. The search used to terminate only on `value >= cur`, which
        // that trailing entry never produces: at `symbol == 1` the bracket is `EC_MIN_PROB`, and
        // any decoder state below it walked on to `cdf[3]` — one past the end. All-ones data
        // initialises `SymbolValue` to 0, which is exactly that state.
        let spec_form = [16384u16, 32768, 0];
        let mut dec = SymbolDecoder::new(&[0xff, 0xff, 0xff, 0xff]);
        let s = dec.decode_symbol(&spec_form);
        assert!(s < spec_form.len(), "symbol {s} is past the end of the CDF");
        // Still usable afterwards: the bounded exit leaves a renormalised, non-degenerate state.
        let _ = dec.decode_symbol(&spec_form);
        let _ = dec.decode_bool();

        // The adapting entry point funnels through the same search.
        let mut adapting = spec_form;
        let mut count = 0;
        let mut dec = SymbolDecoder::new(&[0xff, 0xff, 0xff, 0xff]);
        let s = dec.decode_symbol_adapt(&mut adapting, &mut count);
        assert!(s < spec_form.len(), "symbol {s} is past the end of the CDF");
    }

    #[test]
    fn an_empty_cdf_decodes_nothing_and_consumes_nothing() {
        // `decode_symbol(&[])` used to index `cdf[0]` unconditionally. There is no symbol to decode
        // over an empty alphabet, so it must return 0 without touching the decoder state — proven
        // by a second decoder that skips the empty read and stays in lockstep.
        let cdf = [16384u16, 32768];
        let mut enc = SymbolEncoder::new();
        for s in [1usize, 0, 1, 1, 0] {
            enc.encode_symbol(s, &cdf);
        }
        let bytes = enc.finish();

        let mut with_empty = SymbolDecoder::new(&bytes);
        let mut without = SymbolDecoder::new(&bytes);
        for s in [1usize, 0, 1, 1, 0] {
            assert_eq!(with_empty.decode_symbol(&[]), 0);
            assert_eq!(with_empty.decode_symbol(&cdf), s);
            assert_eq!(without.decode_symbol(&cdf), s);
        }

        // The adapting path must survive it too: `update_cdf` has no top entry to hold and no
        // `FloorLog2(N)` to take.
        let mut count = 3u16;
        assert_eq!(with_empty.decode_symbol_adapt(&mut [], &mut count), 0);
        assert_eq!(count, 3, "an empty CDF has nothing to adapt");
    }

    #[test]
    fn exit_symbol_accepts_a_tile_the_encoder_flushed() {
        // `finish` emits od_ec's minimal flush, whose `| (m + 1)` term *is* §8.2.4's trailing one
        // bit. Decoding the whole tile back must therefore leave `exit_symbol` satisfied, for a
        // flush of every length the coder produces.
        let mut rng = Lcg(0x243f_6a88_85a3_08d3);
        for nsyms in 2..8usize {
            for len in [0usize, 1, 2, 7, 40, 300] {
                let cdf = random_cdf(&mut rng, nsyms);
                let stream: Vec<usize> =
                    (0..len).map(|_| rng.below(nsyms as u32) as usize).collect();
                let mut enc = SymbolEncoder::new();
                for &s in &stream {
                    enc.encode_symbol(s, &cdf);
                }
                let bytes = enc.finish();

                let mut dec = SymbolDecoder::new(&bytes);
                for (i, &s) in stream.iter().enumerate() {
                    assert_eq!(
                        dec.decode_symbol(&cdf),
                        s,
                        "nsyms {nsyms}, len {len}, event {i}"
                    );
                }
                assert!(
                    dec.exit_symbol(),
                    "a flushed tile of {len} symbols over {nsyms} must exit cleanly"
                );
            }
        }
    }

    #[test]
    fn exit_symbol_rejects_a_tile_that_never_carried_its_trailing_bit() {
        // §8.2.4 needs three things, and only the last is "the padding is zero". An all-zero tile
        // satisfies that one and fails the other two, so checking padding alone silently accepts
        // it: here `SymbolMaxBits` is 9 and `trailingBitPosition` is 0, whose bit must be 1.
        assert!(!SymbolDecoder::new(&[0x00, 0x00, 0x00]).exit_symbol());

        // The same tile with the trailing bit actually present is conformant.
        assert!(SymbolDecoder::new(&[0x80, 0x00, 0x00]).exit_symbol());

        // A bit set strictly between trailingBitPosition and paddingEndPosition is not padding.
        // `paddingEndPosition` is the end of the tile's bytes, so the last byte is covered too.
        assert!(!SymbolDecoder::new(&[0x80, 0x00, 0x04]).exit_symbol());
        assert!(!SymbolDecoder::new(&[0x80, 0x40, 0x00]).exit_symbol());

        // SymbolMaxBits is 8 * sz - 15, so a zero-length tile sits at -15: below §8.2.4's floor,
        // and with no trailing bit anywhere to find.
        assert!(!SymbolDecoder::new(&[]).exit_symbol());
    }

    #[test]
    fn exit_symbol_tracks_the_trailing_bit_as_the_decoder_advances() {
        // `trailingBitPosition` is derived from the decoder's own position and SymbolMaxBits, so
        // it moves with the read cursor: the flush of a one-symbol tile is accepted only once
        // that symbol has been consumed, and a tile decoded past its flush is refused.
        let cdf = [16384u16, 32768];
        let mut enc = SymbolEncoder::new();
        for _ in 0..12 {
            enc.encode_symbol(1, &cdf);
        }
        let bytes = enc.finish();

        let mut dec = SymbolDecoder::new(&bytes);
        for _ in 0..12 {
            let _ = dec.decode_symbol(&cdf);
        }
        assert!(dec.exit_symbol(), "the flush point exits cleanly");

        // Reading on drives SymbolMaxBits down through §8.2.4's -14 floor.
        let mut over = dec;
        for _ in 0..32 {
            let _ = over.decode_symbol(&cdf);
        }
        assert!(
            !over.exit_symbol(),
            "a tile read past its padding is malformed"
        );
    }

    #[test]
    fn finish_emits_canonical_minimal_bytes() {
        // The flush is deterministic: for a fixed bit stream `finish` must emit the exact od_ec
        // minimal byte string a decoder consumes. These snapshots pin the arithmetic in `finish`
        // that the round-trip tests alone cannot — a decoder tolerates trailing slack, so a
        // wrong-but-still-decodable flush would otherwise survive. Each case is also decoded back,
        // proving the bytes are correct rather than merely fixed. Input is bit `i` of PAT.
        const PAT: u64 = 0xD2B4_F08C_3A91_67E5;
        // n = 7/15/23/31 land on s == 8, where `s -= 8` → `/=` would emit an extra flush byte; the
        // multi-byte cases pin the `(m + 1)` term of `e` against `+` → `-`/`*`.
        let cases: &[(u32, &[u8])] = &[
            (0, &[0x80]),
            (1, &[0xc0]),
            (7, &[0xa7]),
            (8, &[0xa7, 0x80]),
            (15, &[0xa7, 0xe1]),
            (16, &[0xa7, 0xe0, 0x80]),
            (23, &[0xa7, 0xe1, 0x07]),
            (24, &[0xa7, 0xe1, 0x07, 0x80]),
            (31, &[0xa7, 0xe1, 0x07, 0xc5]),
            (32, &[0xa7, 0xe1, 0x07, 0xc5, 0x80]),
            (40, &[0xa7, 0xe1, 0x07, 0xc4, 0xc7, 0x80]),
            (48, &[0xa7, 0xe1, 0x07, 0xc4, 0xc6, 0xd2, 0x80]),
        ];
        for &(n, expected) in cases {
            let mut enc = SymbolEncoder::new();
            for i in 0..n {
                enc.encode_literal(((PAT >> (i % 64)) & 1) as u32, 1);
            }
            let bytes = enc.finish();
            assert_eq!(bytes, expected, "flush bytes for {n} bits");
            let mut dec = SymbolDecoder::new(&bytes);
            for i in 0..n {
                assert_eq!(
                    dec.decode_literal(1),
                    ((PAT >> (i % 64)) & 1) as u32,
                    "decode bit {i} of {n}"
                );
            }
        }
    }
}
