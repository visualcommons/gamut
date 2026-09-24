//! PNG scanline filtering (PNG spec §9).
//!
//! Before compression, each scanline is transformed by one of five filters that predict each byte
//! from its neighbours (left, above, above-left) and store the residual. Good filter choices make
//! the data far more compressible. Filters operate on raw bytes with a stride of `bpp` (bytes per
//! pixel, ≥1); they reference the *unfiltered* bytes of the current and previous rows.

/// A PNG scanline filter type (the leading byte of each filtered scanline).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FilterType {
    /// No filtering; the bytes are stored as-is.
    None = 0,
    /// Residual from the byte `bpp` to the left.
    Sub = 1,
    /// Residual from the byte directly above.
    Up = 2,
    /// Residual from the floor-average of the left and above bytes.
    Average = 3,
    /// Residual from the Paeth predictor of left, above, and above-left.
    Paeth = 4,
}

/// How the encoder chooses a filter for each scanline (a space/time trade-off).
///
/// Non-exhaustive: a heuristic is a measurement result, and this crate's own `STATUS.md` records
/// the corpus that decides which ones are worth shipping — so the set grows as that corpus does.
/// Match with a wildcard arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum FilterStrategy {
    /// Filter every scanline with [`FilterType::None`] (fastest; good for already-random data).
    None,
    /// Use one fixed filter for every scanline.
    Fixed(FilterType),
    /// Per scanline, pick the filter minimising the sum of absolute residuals — the standard
    /// libpng heuristic. A good size/speed balance and the default.
    MinSumAbs,
    /// Per scanline, pick the filter whose residuals have the lowest Shannon entropy.
    ///
    /// Sum-of-absolutes asks "are these bytes small?"; entropy asks "are these bytes *repetitive*?"
    /// — which is the question DEFLATE actually answers. A row of alternating 0 and 200 scores
    /// badly under `MinSumAbs` and beautifully under this.
    ///
    /// The only strategy that scores in floating point. `f64::log2` is not required to be
    /// correctly rounded, so this strategy's output is reproducible on a machine but not
    /// guaranteed bit-identical across libm implementations — which is why it is absent from
    /// [`BRUTE_FORCE_STRATEGIES`](crate::PngEncoder), keeping the default and `BruteForce` paths
    /// integer-only and their output byte-exact everywhere. It never uniquely won a corpus row
    /// (`STATUS.md`), so it is offered rather than chosen.
    MinEntropy,
    /// Per scanline, pick the filter producing the fewest distinct byte bigrams.
    ///
    /// A cheaper proxy for the same idea one order up: LZ77 matches runs, not single bytes, so
    /// counting distinct adjacent pairs approximates how much of the row it can back-reference.
    MinBigrams,
    /// Encode the whole image under several filter strategies, DEFLATE each, and keep the smallest.
    /// Pairs with [`Level::Best`](gamut_deflate::Level::Best) for maximum compression; slowest.
    BruteForce,
}

impl FilterType {
    /// The filter type for a scanline's leading filter byte, or `None` for an undefined code
    /// (PNG §9.1 defines exactly 0–4 for filter method 0).
    pub(crate) fn from_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(FilterType::None),
            1 => Some(FilterType::Sub),
            2 => Some(FilterType::Up),
            3 => Some(FilterType::Average),
            4 => Some(FilterType::Paeth),
            _ => None,
        }
    }
}

/// The Paeth predictor (PNG §9.4): chooses whichever of `a` (left), `b` (above), `c` (above-left)
/// is closest to `a + b - c`, with the spec's exact tie-break order.
fn paeth(a: u8, b: u8, c: u8) -> u8 {
    let (ai, bi, ci) = (i32::from(a), i32::from(b), i32::from(c));
    let p = ai + bi - ci;
    let pa = (p - ai).abs();
    let pb = (p - bi).abs();
    let pc = (p - ci).abs();
    if pa <= pb && pa <= pc {
        a
    } else if pb <= pc {
        b
    } else {
        c
    }
}

/// Forward-filters one scanline `cur` (with previous raw row `prev`, all zero for the first row)
/// into `out` (which is overwritten to `cur.len()` bytes).
///
/// Structured for the vectoriser rather than for brevity, because this is the encoder's hottest
/// loop -- `MinSumAbs` runs it five times per scanline and `BruteForce` up to ten. Three things
/// matter, and the straightforward version does none of them:
///
/// * The `i >= bpp` test that picks between a real left-neighbour and an implicit zero is loop
///   invariant, so the row splits into a `bpp`-long prologue where `a` and `c` are zero and a
///   body where they are not. Testing it per byte defeats vectorisation outright.
/// * The body then reads five *equal-length* subslices, which lets the bounds checks fold away
///   instead of being re-proved for every index.
/// * The filter is matched once, outside the loop, so each arm is a straight-line kernel rather
///   than a branch per byte. And `out` is sized once, so there is no capacity check per `push`.
fn filter_row(filter: FilterType, cur: &[u8], prev: &[u8], bpp: usize, out: &mut Vec<u8>) {
    let n = cur.len();
    out.clear();
    out.resize(n, 0);
    let head = bpp.min(n);

    // Prologue: the first `bpp` bytes have no left neighbour, so `a == c == 0`. That collapses
    // Sub to a copy and -- less obviously -- Paeth to Up, because `paeth(0, b, 0) == b` for every
    // `b` (at `b == 0` all three distances tie and the spec's order picks `a`, which is also 0).
    match filter {
        FilterType::None | FilterType::Sub => out[..head].copy_from_slice(&cur[..head]),
        FilterType::Up | FilterType::Paeth => {
            for (d, (&x, &b)) in out[..head]
                .iter_mut()
                .zip(cur[..head].iter().zip(&prev[..head]))
            {
                *d = x.wrapping_sub(b);
            }
        }
        FilterType::Average => {
            for (d, (&x, &b)) in out[..head]
                .iter_mut()
                .zip(cur[..head].iter().zip(&prev[..head]))
            {
                *d = x.wrapping_sub(b / 2);
            }
        }
    }

    // Body: `x` is the current byte, `a` the byte `bpp` to its left, `b` the byte above, `c` the
    // byte above-left. All five slices are the same length by construction.
    let m = n - head;
    let dst = &mut out[head..];
    let x = &cur[head..];
    let a = &cur[..m];
    let b = &prev[head..];
    let c = &prev[..m];
    match filter {
        FilterType::None => dst.copy_from_slice(x),
        FilterType::Sub => {
            for (d, (&x, &a)) in dst.iter_mut().zip(x.iter().zip(a)) {
                *d = x.wrapping_sub(a);
            }
        }
        FilterType::Up => {
            for (d, (&x, &b)) in dst.iter_mut().zip(x.iter().zip(b)) {
                *d = x.wrapping_sub(b);
            }
        }
        FilterType::Average => {
            for (d, ((&x, &a), &b)) in dst.iter_mut().zip(x.iter().zip(a).zip(b)) {
                *d = x.wrapping_sub(((u16::from(a) + u16::from(b)) / 2) as u8);
            }
        }
        FilterType::Paeth => {
            for (d, (((&x, &a), &b), &c)) in dst.iter_mut().zip(x.iter().zip(a).zip(b).zip(c)) {
                *d = x.wrapping_sub(paeth(a, b, c));
            }
        }
    }
}

/// Reconstructs one scanline in place from its filtered bytes (PNG §9.2–§9.4, the decoder's
/// inverse of [`filter_row`]): `row` holds the filtered bytes on entry and the raw bytes on exit.
/// `prev` is the *raw* previous scanline of the same (reduced) image — all zero for the first row —
/// and `bpp` is the filter stride in bytes (≥ 1). In-place works because reconstruction consumes
/// left-to-right: `row[i - bpp]` is already raw when `row[i]` needs it.
pub(crate) fn unfilter_row(filter: FilterType, row: &mut [u8], prev: &[u8], bpp: usize) {
    for i in 0..row.len() {
        let a = if i >= bpp { row[i - bpp] } else { 0 };
        let b = prev[i];
        let c = if i >= bpp { prev[i - bpp] } else { 0 };
        row[i] = match filter {
            FilterType::None => row[i],
            FilterType::Sub => row[i].wrapping_add(a),
            FilterType::Up => row[i].wrapping_add(b),
            FilterType::Average => row[i].wrapping_add(((u16::from(a) + u16::from(b)) / 2) as u8),
            FilterType::Paeth => row[i].wrapping_add(paeth(a, b, c)),
        };
    }
}

/// The minimum-sum-of-absolute-residuals score for a filtered row (each byte counted as a signed
/// magnitude). Lower is more compressible.
fn sum_abs(filtered: &[u8]) -> u64 {
    filtered
        .iter()
        .map(|&x| u64::from(x.min(x.wrapping_neg())))
        .sum()
}

/// How a candidate row is judged. Lower is better for all three, so they are interchangeable in
/// [`choose_by`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Score {
    /// Sum of absolute residuals, bytes read as signed magnitudes (libpng's heuristic).
    SumAbs,
    /// Shannon entropy of the byte histogram.
    Entropy,
    /// Count of distinct adjacent byte pairs.
    Bigrams,
}

/// Scratch a scorer needs, allocated once per image rather than per scanline.
///
/// Hoisting saves the *allocation*; it does not on its own save the clearing, and the clearing is
/// the larger cost. The bigram set is 8 KiB, so wiping it wholesale would cost 8 KiB per candidate
/// and five candidates per scanline — 40 KiB of memset per row, independent of how long the row
/// is, which for any ordinary row is more work than the scoring. So the set records which words it
/// dirtied and clears only those: a row of `n` bytes touches at most `n - 1` of them.
struct Scratch {
    /// Byte histogram for [`Score::Entropy`].
    histogram: [u32; 256],
    /// One bit per (previous, current) byte pair for [`Score::Bigrams`].
    bigrams: Vec<u64>,
    /// The indices of the `bigrams` words this scorer set, so the reset touches only them. Holds
    /// each dirtied word exactly once — a word is pushed when it goes from all-zero to non-zero.
    dirty: Vec<usize>,
}

impl Scratch {
    fn new() -> Self {
        Self {
            histogram: [0; 256],
            bigrams: vec![0; 1 << 10],
            dirty: Vec::new(),
        }
    }
}

/// Scores a filtered row; lower is better in every variant, so candidates compare directly.
fn score(kind: Score, filtered: &[u8], scratch: &mut Scratch) -> u64 {
    match kind {
        Score::SumAbs => sum_abs(filtered),
        Score::Entropy => {
            scratch.histogram.fill(0);
            for &b in filtered {
                scratch.histogram[b as usize] += 1;
            }
            // Shannon entropy times the row length, `Σ c·log2(n/c)`, in 1/256ths of a bit so the
            // comparison is integer-exact and the choice reproducible run to run.
            //
            // Stated this way every term is non-negative (`c ≤ n`) and the whole score is bounded
            // by `8n·256` — a byte alphabet carries at most 8 bits — so lower is better directly,
            // rather than by complementing against `u64::MAX`. That matters beyond tidiness: a
            // score that can *reach* `u64::MAX` is a score that can collide with a sentinel, and
            // this one did.
            //
            // Equivalent to the `n·log2(n) − Σ c·log2(c)` form: `n` is constant across a row's
            // five candidates, so it cannot change the ranking either way. The `c == 1` terms
            // contribute `0` there and `1·log2(n)` here, which is why the filter below is `c > 0`
            // — a zero count is excluded because `0·log2(n/0)` is not a number, not because it
            // contributes nothing.
            let n = filtered.len() as f64;
            let bits: f64 = scratch
                .histogram
                .iter()
                .filter(|&&c| c > 0)
                .map(|&c| f64::from(c) * (n / f64::from(c)).log2())
                .sum();
            (bits * 256.0) as u64
        }
        Score::Bigrams => {
            let mut distinct = 0u64;
            for pair in filtered.windows(2) {
                // The pair *is* a big-endian `u16`, so read it as one. Spelling it `a << 8 | b`
                // costs two operators that carry no meaning of their own -- one of which has no
                // behavioural variant at all, since the low byte of `a << 8` is zero and `|` is
                // therefore indistinguishable from `^`.
                let index = usize::from(u16::from_be_bytes([pair[0], pair[1]]));
                let (word, bit) = (index >> 6, index & 63);
                if scratch.bigrams[word] & (1 << bit) == 0 {
                    // Record the word the first time it leaves zero, so `dirty` lists each
                    // dirtied word once and the reset below is exact.
                    if scratch.bigrams[word] == 0 {
                        scratch.dirty.push(word);
                    }
                    scratch.bigrams[word] |= 1 << bit;
                    distinct += 1;
                }
            }
            // Leave the set all-zero for the next candidate, touching only what was dirtied.
            for &word in &scratch.dirty {
                scratch.bigrams[word] = 0;
            }
            scratch.dirty.clear();
            distinct
        }
    }
}

/// Filters every scanline of `samples` (row-major, `row_bytes` per row) per `strategy`, producing
/// the filter-prefixed byte stream that gets compressed: a filter-type byte then the filtered row,
/// for each scanline. `bpp` is the filter stride (bytes per pixel, ≥1).
pub fn filter_image(
    strategy: FilterStrategy,
    samples: &[u8],
    row_bytes: usize,
    bpp: usize,
) -> Vec<u8> {
    let height = samples.len().checked_div(row_bytes).unwrap_or(0);
    let mut out = Vec::with_capacity((row_bytes + 1) * height);
    let zero_row = vec![0u8; row_bytes];
    let mut prev = zero_row.as_slice();
    let mut scratch = Vec::with_capacity(row_bytes);
    let mut chosen = Vec::with_capacity(row_bytes);
    let mut aux = Scratch::new();
    // The per-scanline heuristics differ only in how they score a candidate. BruteForce is
    // resolved to concrete strategies by the encoder; if it reaches here, fall back to MinSumAbs.
    let adaptive = match strategy {
        FilterStrategy::MinSumAbs | FilterStrategy::BruteForce => Some(Score::SumAbs),
        FilterStrategy::MinEntropy => Some(Score::Entropy),
        FilterStrategy::MinBigrams => Some(Score::Bigrams),
        FilterStrategy::None | FilterStrategy::Fixed(_) => None,
    };
    for y in 0..height {
        let cur = &samples[y * row_bytes..(y + 1) * row_bytes];
        match adaptive {
            Some(kind) => {
                let filter = choose_by(kind, cur, prev, bpp, &mut scratch, &mut chosen, &mut aux);
                out.push(filter as u8);
                out.extend_from_slice(&chosen);
            }
            None => {
                let filter = match strategy {
                    FilterStrategy::Fixed(f) => f,
                    _ => FilterType::None,
                };
                out.push(filter as u8);
                filter_row(filter, cur, prev, bpp, &mut scratch);
                out.extend_from_slice(&scratch);
            }
        }
        prev = cur;
    }
    out
}

/// Tries all five filters and keeps the one `kind` ranks lowest, leaving its bytes in
/// `best_bytes`.
///
/// The first minimum wins, so a tie resolves to the earlier filter in None/Sub/Up/Average/Paeth
/// order — deterministic, which the byte-reproducibility contract depends on.
///
/// Returning the winning bytes in `best_bytes` rather than just the winning filter is what makes
/// this five passes over the row instead of six: the caller would otherwise re-run [`filter_row`]
/// for the filter just chosen, having already computed exactly those bytes and thrown them away.
/// Keeping them costs one `memcpy` per improvement, against a full filter pass per scanline.
fn choose_by(
    kind: Score,
    cur: &[u8],
    prev: &[u8],
    bpp: usize,
    scratch: &mut Vec<u8>,
    best_bytes: &mut Vec<u8>,
    aux: &mut Scratch,
) -> FilterType {
    let mut best = FilterType::None;
    // `None`, not a sentinel score. Seeding with `u64::MAX` and improving on a strict `<` leaves
    // `best_bytes` unwritten when every candidate scores `u64::MAX` — and `filter_image` reuses
    // that buffer across scanlines, so the row would be emitted with its predecessor's residuals
    // under a filter byte of 0. `Option` makes "nothing chosen yet" unrepresentable as a score, so
    // the first candidate is always taken whatever any scorer returns.
    let mut best_score: Option<u64> = None;
    for filter in [
        FilterType::None,
        FilterType::Sub,
        FilterType::Up,
        FilterType::Average,
        FilterType::Paeth,
    ] {
        filter_row(filter, cur, prev, bpp, scratch);
        let candidate = score(kind, scratch, aux);
        if best_score.is_none_or(|best| candidate < best) {
            best_score = Some(candidate);
            best = filter;
            best_bytes.clear();
            best_bytes.extend_from_slice(scratch);
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reconstructs a scanline from its filtered form via the production [`unfilter_row`].
    fn reconstruct(filter: FilterType, filt: &[u8], prev: &[u8], bpp: usize) -> Vec<u8> {
        let mut cur = filt.to_vec();
        unfilter_row(filter, &mut cur, prev, bpp);
        cur
    }

    #[test]
    fn every_filter_is_invertible() {
        let filters = [
            FilterType::None,
            FilterType::Sub,
            FilterType::Up,
            FilterType::Average,
            FilterType::Paeth,
        ];
        for bpp in [1usize, 2, 3, 4] {
            for seed in 0..8u32 {
                let n = 4 * bpp + (seed as usize % 5);
                let cur: Vec<u8> = (0..n)
                    .map(|i| {
                        (i as u32)
                            .wrapping_mul(seed.wrapping_add(7))
                            .wrapping_mul(31) as u8
                    })
                    .collect();
                let prev: Vec<u8> = (0..n)
                    .map(|i| (i as u32 ^ seed.wrapping_mul(13)).wrapping_mul(17) as u8)
                    .collect();
                for &f in &filters {
                    let mut filt = Vec::new();
                    filter_row(f, &cur, &prev, bpp, &mut filt);
                    assert_eq!(reconstruct(f, &filt, &prev, bpp), cur, "{f:?} bpp={bpp}");
                }
            }
        }
    }

    #[test]
    fn paeth_matches_spec_examples() {
        // a + b - c closest wins; ties favour a, then b.
        assert_eq!(paeth(10, 20, 10), 20); // p=20 -> b
        assert_eq!(paeth(100, 90, 80), 100); // p=110 -> a (|10| vs |20| vs |30|)
        assert_eq!(paeth(0, 0, 0), 0);
        assert_eq!(paeth(255, 0, 0), 255); // p=255 -> a
    }

    #[test]
    fn paeth_tie_breaks_exactly_per_spec() {
        // The observable tie is pb == pc < pa, where the spec's order picks b over c: with
        // c = (2a + b) / 3, pb = pc = |a-b|/3 while pa = 2|a-b|/3.
        assert_eq!(paeth(0, 9, 3), 9); // pa=6, pb=pc=3 -> b (a "<" mutant would pick c)
        assert_eq!(paeth(90, 0, 60), 0); // pa=60, pb=pc=30 -> b
        // pa == pb with distinct a and b forces pc = 0, so c wins outright.
        assert_eq!(paeth(10, 30, 20), 20); // p=20: pa=pb=10, pc=0 -> c
        assert_eq!(paeth(100, 60, 80), 80); // p=80: pa=pb=20, pc=0 -> c
    }

    #[test]
    fn unfilter_golden_vectors() {
        // Hand-computed reconstructions with non-trivial predecessors (bpp = 2).
        let prev = [10u8, 20, 30, 40, 50, 60];
        let mut sub = [1u8, 2, 3, 4, 5, 6];
        unfilter_row(FilterType::Sub, &mut sub, &prev, 2);
        assert_eq!(sub, [1, 2, 4, 6, 9, 12]);
        let mut up = [1u8, 2, 3, 4, 5, 6];
        unfilter_row(FilterType::Up, &mut up, &prev, 2);
        assert_eq!(up, [11, 22, 33, 44, 55, 66]);
        // Average: floor((a + b) / 2) with u16 widening; first pixel has a = 0.
        let mut avg = [1u8, 2, 3, 4, 5, 6];
        unfilter_row(FilterType::Average, &mut avg, &prev, 2);
        // x0: 1+10/2=6, 2+20/2=12; x1: 3+(6+30)/2=21, 4+(12+40)/2=30; x2: 5+(21+50)/2=40, 6+(30+60)/2=51
        assert_eq!(avg, [6, 12, 21, 30, 40, 51]);
        // Average overflow: a + b = 255 + 255 must widen, not wrap, before halving.
        let mut wide = [0u8, 0];
        unfilter_row(FilterType::Average, &mut wide, &[255, 255], 2);
        let mut wide2 = [1u8, 255];
        unfilter_row(FilterType::Average, &mut wide2, &[255, 255], 1);
        assert_eq!(wide, [127, 127]); // floor(255/2) twice (a = 0 for the first pixel)
        assert_eq!(wide2, [128, 190]); // 1+floor(255/2)=128, then 255+floor((128+255)/2)=255+191 wraps to 190
    }

    /// A row that is *large* but *repetitive*: alternating 0 and 200 under `Sub`.
    ///
    /// This is the case the two new heuristics exist for. Sum-of-absolutes asks "are these bytes
    /// small?" and rates it terribly; entropy and bigrams ask "are these bytes repetitive?", which
    /// is the question DEFLATE actually answers.
    #[test]
    fn entropy_and_bigrams_prefer_repetition_where_sum_abs_prefers_smallness() {
        let repetitive: Vec<u8> = (0..64).map(|i| if i % 2 == 0 { 0 } else { 200 }).collect();
        let varied: Vec<u8> = (0..64u8).map(|i| i / 8).collect();
        let mut aux = Scratch::new();

        // Sum-of-absolutes: the varied row is far "smaller" and wins.
        assert!(
            score(Score::SumAbs, &varied, &mut aux) < score(Score::SumAbs, &repetitive, &mut aux)
        );
        // Entropy and bigrams: the repetitive row has two symbols and one alternating pair, and
        // wins by a mile.
        assert!(
            score(Score::Entropy, &repetitive, &mut aux) < score(Score::Entropy, &varied, &mut aux)
        );
        assert!(
            score(Score::Bigrams, &repetitive, &mut aux) < score(Score::Bigrams, &varied, &mut aux)
        );
    }

    #[test]
    fn the_bigram_score_counts_distinct_adjacent_pairs() {
        let mut aux = Scratch::new();
        // (1,2), (2,1), (1,2), (2,1) -> two distinct pairs, however long the run.
        assert_eq!(score(Score::Bigrams, &[1, 2, 1, 2, 1], &mut aux), 2);
        // A constant row has exactly one.
        assert_eq!(score(Score::Bigrams, &[7, 7, 7, 7], &mut aux), 1);
        // Every pair distinct.
        assert_eq!(score(Score::Bigrams, &[1, 2, 3, 4], &mut aux), 3);
        // Fewer than two bytes has no pairs at all.
        assert_eq!(score(Score::Bigrams, &[9], &mut aux), 0);
        assert_eq!(score(Score::Bigrams, &[], &mut aux), 0);
        // Distinct *pairs*, not distinct second bytes: (1,3), (3,2), (2,3) is three pairs over two
        // distinct second bytes, so an index that dropped the high byte would report two. Every
        // vector above happens to have as many pairs as second bytes, so none of them can tell.
        assert_eq!(score(Score::Bigrams, &[1, 3, 2, 3], &mut aux), 3);
    }

    #[test]
    fn the_scratch_is_reusable_across_rows() {
        // The histogram and bigram set are allocated once per image, so a stale one would silently
        // score the wrong thing on every row after the first.
        let mut aux = Scratch::new();
        let first = score(Score::Bigrams, &[1, 2, 3, 4], &mut aux);
        let second = score(Score::Bigrams, &[7, 7, 7, 7], &mut aux);
        assert_eq!(first, 3);
        assert_eq!(second, 1, "the previous row's pairs must not carry over");

        let a = score(Score::Entropy, &[0, 0, 0, 0], &mut aux);
        let b = score(Score::Entropy, &[0, 1, 2, 3], &mut aux);
        assert!(a < b, "a constant row must stay the lower-entropy one");
    }

    #[test]
    fn a_universal_score_tie_keeps_the_first_filters_bytes() {
        // Every candidate for this row has all-distinct bytes, so under a scorer that ranks by
        // repetition they all score identically. `choose_by` must still emit the first candidate's
        // bytes: before the `Option` seed it emitted none at all, and `filter_image` produced a
        // one-byte stream for a two-byte row -- a PNG whose IDAT is shorter than its image.
        assert_eq!(
            filter_image(FilterStrategy::MinEntropy, &[1, 3], 2, 1),
            [FilterType::None as u8, 1, 3]
        );
    }

    #[test]
    fn a_tied_row_does_not_reuse_the_previous_rows_residuals() {
        // The companion failure to the one above, and the dangerous one: `filter_image` hoists the
        // chosen-bytes buffer out of the row loop, so a row that chose nothing re-emitted its
        // predecessor's residuals under a filter byte of 0 -- a structurally valid PNG carrying
        // the wrong pixels, with no error anywhere.
        assert_eq!(
            filter_image(FilterStrategy::MinEntropy, &[0, 0, 0, 1], 2, 1),
            [FilterType::None as u8, 0, 0, FilterType::None as u8, 0, 1]
        );
    }

    #[test]
    fn the_entropy_score_weights_each_symbol_by_how_often_it_occurs() {
        // Entropy is `sum c*log2(n/c)`, not `sum log2(n/c)`: each symbol's surprise is weighted by
        // how much of the row it accounts for. Drop the weighting and the score degenerates into
        // something that mostly counts distinct symbols, which ranks these two rows the other way
        // round.
        //
        // `balanced` is two symbols split evenly -- the worst case for a two-symbol row, a full
        // bit per byte. `concentrated` has *more* distinct symbols but spends 14 of its 16 bytes
        // on one of them, so it carries less information and must score lower. Unweighted it
        // scores higher, because it has three log terms against two.
        let mut aux = Scratch::new();
        let mut balanced = vec![0u8; 8];
        balanced.extend(std::iter::repeat_n(1u8, 8));
        let mut concentrated = vec![0u8; 14];
        concentrated.extend_from_slice(&[1, 2]);

        let balanced = score(Score::Entropy, &balanced, &mut aux);
        let concentrated = score(Score::Entropy, &concentrated, &mut aux);
        assert!(
            concentrated < balanced,
            "concentrated {concentrated} should score below balanced {balanced}"
        );
    }

    #[test]
    fn the_entropy_scale_separates_rows_closer_than_one_bit() {
        // The scale is what makes the score integer-exact: these two rows carry 8.000 and 8.490
        // bits, which both floor to 8. Only multiplying by 256 before the cast keeps them apart,
        // so this is the assertion that a `+ 256.0` or `/ 256.0` scale cannot satisfy.
        let mut aux = Scratch::new();
        let even = score(Score::Entropy, &[0, 0, 0, 0, 1, 1, 1, 1], &mut aux);
        let skewed = score(Score::Entropy, &[0, 0, 0, 0, 0, 0, 1, 2], &mut aux);
        assert!(even < skewed, "{even} < {skewed}");
    }

    #[test]
    fn min_sum_abs_prefers_flat_residuals() {
        // A horizontal gradient (each pixel = previous + k) filters to a constant under Sub, which
        // scores far below None.
        let row: Vec<u8> = (0..30u8).map(|i| i.wrapping_mul(3)).collect();
        let prev = vec![0u8; row.len()];
        let chosen = choose_by(
            Score::SumAbs,
            &row,
            &prev,
            1,
            &mut Vec::new(),
            &mut Vec::new(),
            &mut Scratch::new(),
        );
        assert_eq!(chosen, FilterType::Sub);
    }
}
