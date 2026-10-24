//! LZ77 parsing: a chained-hash longest-match finder over the input bytes (RFC 1951 §4).
//!
//! The parser turns the byte stream into a sequence of [`Token`]s — literals and `(length,
//! distance)` back-references — which a block writer then entropy-codes. Match *correctness* never
//! depends on the hash; candidates are always verified by byte comparison. The hash only affects how
//! many real matches are found, i.e. the ratio.
//!
//! `parse` offers greedy (`Level::Fast`) and lazy (`Level::Default`) matching; `parse_optimal` adds
//! the zopfli-style shortest-path parse (`Level::Best`). All three share this one match finder.

use crate::huffman::CanonicalCode;
use crate::symbols::{self, MAX_DISTANCE, MAX_MATCH, MIN_MATCH};

/// One element of the LZ77 token stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Token {
    /// A single uncompressed byte.
    Literal(u8),
    /// A back-reference: copy `len` bytes (3..=258) from `dist` (1..=32768) bytes earlier.
    Match { len: u16, dist: u16 },
}

/// One step of a position's per-length nearest-distance table (zopfli's `sublen`, run-length
/// coded): every length above the previous run's `len` -- above `MIN_MATCH - 1` for a position's
/// first run -- up to and including `len` is reachable at `dist`, and at no nearer distance.
///
/// [`Matcher::walk`] emits one run per improvement of its longest match, so a position's runs
/// ascend strictly in both fields and the last one is its longest match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Run {
    len: u16,
    dist: u16,
}

/// Number of hash buckets is `1 << HASH_BITS`.
const HASH_BITS: u32 = 15;
/// Number of hash buckets.
const HASH_SIZE: usize = 1 << HASH_BITS;
/// The sliding window is the maximum back-reference distance; `prev` is indexed modulo it.
const WINDOW: usize = MAX_DISTANCE;
/// Mask for the power-of-two window.
const WMASK: usize = WINDOW - 1;

/// A chained-hash index over already-seen 3-byte sequences, used to find back-references.
///
/// `head[h]` is the most recent position whose 3-byte hash is `h`; `prev[pos & WMASK]` chains to the
/// next-older position in the same bucket. Both store absolute positions (`-1` = empty). Entries
/// older than the window are pruned by the distance check during search, so the modular `prev`
/// indexing never returns a stale match.
struct Matcher {
    head: Vec<i32>,
    prev: Vec<i32>,
}

impl Matcher {
    fn new() -> Self {
        Self {
            head: vec![-1; HASH_SIZE],
            prev: vec![-1; WINDOW],
        }
    }

    /// Hashes the 3-byte sequence `[b0, b1, b2]` that starts a candidate match.
    fn hash([b0, b1, b2]: [u8; MIN_MATCH]) -> usize {
        // The three bytes are a big-endian `u32`, so they are read as one. Spelling it
        // `b0 << 16 | b1 << 8 | b2` puts each byte in its own lane, and disjoint lanes are where
        // `|` and `^` agree -- two unkillable mutants for no gain (#110).
        let key = u32::from_be_bytes([0, b0, b1, b2]);
        // The shift already leaves exactly `HASH_BITS` bits, so the former `& (HASH_SIZE - 1)` was
        // a no-op -- and a no-op mask is unkillable by construction.
        (key.wrapping_mul(0x9E37_79B1) >> (32 - HASH_BITS)) as usize
    }

    /// Seeds the chains with every position in the window preceding `start`, so a parse beginning
    /// at `start` can still reference the bytes before it. Without this a span boundary would also
    /// be a match boundary, costing ratio at every seam.
    fn prime(&mut self, data: &[u8], start: usize) {
        for pos in start.saturating_sub(WINDOW)..start {
            self.insert(data, pos);
        }
    }

    /// Records `pos` as a future match candidate.
    ///
    /// A position with fewer than `MIN_MATCH` bytes left is not recorded: there is no key to hash.
    /// The slice pattern decides that, rather than a `pos + MIN_MATCH > data.len()` guard, because
    /// the guard had an unkillable `>=` twin (#493): the last position it would additionally drop,
    /// `data.len() - MIN_MATCH`, can only be referenced by a later position with a full key of its
    /// own, and there is none.
    fn insert(&mut self, data: &[u8], pos: usize) {
        let Some(&[b0, b1, b2]) = data.get(pos..pos + MIN_MATCH) else {
            return;
        };
        let h = Self::hash([b0, b1, b2]);
        self.prev[pos & WMASK] = self.head[h];
        self.head[h] = pos as i32;
    }

    /// Finds the back-references for the bytes at `pos`, walking at most `max_chain` candidates,
    /// and **appends** them to `runs` as one [`Run`] per length the walk improves to: every
    /// `len >= MIN_MATCH` and `1 <= dist <= 32768`, the last run being the longest match. A
    /// position with no match appends nothing.
    ///
    /// Matches never extend past `limit` (`<= data.len()`), so a caller parsing one span of a
    /// larger buffer cannot emit a token that runs off the end of its span. Candidates *behind*
    /// `pos` are unrestricted, which is what lets a span reference the history before it.
    ///
    /// Each surviving candidate is measured with [`common_prefix_len`], eight bytes per step. The
    /// prune in front of it is still a single byte: it rejects most candidates outright, and a
    /// wide read there would touch bytes past `best_len` for nothing.
    ///
    /// The runs give, for every length from `MIN_MATCH` up to the longest, the **nearest** distance
    /// at which a match of at least that length exists, which is what lets the optimal parse price
    /// a short match at its own cheapest distance instead of at the distance of the longest one
    /// (zopfli's `sublen`). The chain is walked nearest-first and a candidate only records the
    /// lengths it is the first to reach, so the first candidate reaching a length is the nearest
    /// that does. They are appended rather than written to a fresh table so that [`MatchCache`] can
    /// store every position's runs end to end.
    fn walk(&self, data: &[u8], pos: usize, max_chain: usize, limit: usize, runs: &mut Vec<Run>) {
        if pos + MIN_MATCH > limit {
            return;
        }
        let max_len = (limit - pos).min(MAX_MATCH);
        let lowest = pos.saturating_sub(WINDOW); // candidates must be >= this for dist <= window
        let mut best_len = MIN_MATCH - 1; // a real match must strictly exceed this
        let mut cand = self.head[Self::hash([data[pos], data[pos + 1], data[pos + 2]])];
        let mut chain = 0usize;
        while cand >= 0 {
            let c = cand as usize;
            if c < lowest {
                break; // out of window; chain only gets older from here
            }
            // Prune: a candidate can only win if it matches at the byte just past the current best.
            // `best_len < max_len` holds throughout the loop -- it starts at `MIN_MATCH - 1 < 3 <=
            // max_len`, and a match that reaches `max_len` breaks out below before it is stored --
            // so both reads stay inside the two `max_len` windows compared next.
            if data[c + best_len] == data[pos + best_len] {
                // `c < pos`, so both windows end at or before `limit <= data.len()`.
                let len = common_prefix_len(&data[c..c + max_len], &data[pos..pos + max_len]);
                if len > best_len {
                    let dist = pos - c;
                    // Lengths up to `best_len` were reached by a nearer candidate already; this one
                    // is the first -- hence the nearest -- to reach `best_len + 1..=len`.
                    runs.push(Run {
                        len: len as u16,
                        dist: dist as u16,
                    });
                    best_len = len;
                    if len >= max_len {
                        break; // can't do better than the maximum
                    }
                }
            }
            chain += 1;
            if chain >= max_chain {
                break;
            }
            cand = self.prev[c & WMASK];
        }
    }
}

/// Length of the common prefix of `a` and `b` (bounded by the shorter), i.e. the index of the
/// first byte at which they differ.
///
/// This is the match finder's inner loop, run once per surviving chain candidate — up to
/// `max_chain` (1024 at `Level::Best`) times per input position — so it compares **eight bytes per
/// step** instead of one: both windows are read as a `u64`, and where they differ the first
/// mismatching byte is the lowest set byte of the XOR, which `trailing_zeros / 8` locates because
/// `from_le_bytes` puts byte 0 in the least significant lane (the same on every target). The
/// remaining `< 8` bytes are compared one at a time. The result is exactly what a byte-by-byte
/// walk would return — this changes how fast a match is measured, never which match is found.
fn common_prefix_len(a: &[u8], b: &[u8]) -> usize {
    let n = a.len().min(b.len());
    let (a_words, a_tail) = a[..n].as_chunks::<8>();
    let (b_words, b_tail) = b[..n].as_chunks::<8>();
    let mut len = 0usize;
    for (x, y) in a_words.iter().zip(b_words) {
        let diff = u64::from_le_bytes(*x) ^ u64::from_le_bytes(*y);
        if diff != 0 {
            return len + (diff.trailing_zeros() / 8) as usize;
        }
        len += 8;
    }
    len + a_tail
        .iter()
        .zip(b_tail)
        .take_while(|(x, y)| x == y)
        .count()
}

/// Where a parse of one span gets each position's match [`Run`]s: searched live, or read back from
/// a [`MatchCache`] that searched them once. Both return the same runs for the same position.
///
/// A parse asks for positions in non-decreasing order, starting at or after the span's start; it
/// may ask for one position more than once.
trait Matches {
    /// The runs of the position `pos` (an absolute index into the data), shortest first.
    fn runs(&mut self, pos: usize) -> &[Run];
}

/// Live search over `data[start..end]`: a [`Matcher`] primed with the window before `start`, which
/// inserts every position below the one asked for before walking its chain.
///
/// Inserting on demand is what makes the answer independent of the order of questions: whichever
/// positions a parse skips (the bytes a match covers), the chain at `pos` always holds exactly the
/// positions in the window before it -- the same chain a [`MatchCache`] walks once.
struct Search<'a> {
    data: &'a [u8],
    end: usize,
    max_chain: usize,
    matcher: Matcher,
    /// Every position below this one is in the matcher.
    inserted: usize,
    /// The last walk's runs, reused across positions.
    runs: Vec<Run>,
}

impl<'a> Search<'a> {
    fn new(data: &'a [u8], start: usize, end: usize, max_chain: usize) -> Self {
        let mut matcher = Matcher::new();
        matcher.prime(data, start);
        Self {
            data,
            end,
            max_chain,
            matcher,
            inserted: start,
            runs: Vec::new(),
        }
    }
}

impl Matches for Search<'_> {
    fn runs(&mut self, pos: usize) -> &[Run] {
        for p in self.inserted..pos {
            self.matcher.insert(self.data, p);
        }
        self.inserted = pos;
        self.runs.clear();
        self.matcher
            .walk(self.data, pos, self.max_chain, self.end, &mut self.runs);
        &self.runs
    }
}

/// The runs of a span's leading positions, searched once and stored end to end.
///
/// The optimal parse asks the same question at every position in every pass -- only the cost model
/// changes between passes, never the match finder's answer -- so the span is searched once here
/// and every pass reads the answers back. That replaces `1 + iterations` chain walks per position
/// (the lazy seed, then one per pass) with one.
///
/// A cached position costs `2 + 4 * r` bytes for its `r` runs. A position can have up to
/// `MAX_MATCH - MIN_MATCH + 1` runs, so the cache holds only as many leading positions as a
/// caller-set run budget allows, and a parse searches the rest of the span live.
struct MatchCache<'a> {
    data: &'a [u8],
    start: usize,
    end: usize,
    max_chain: usize,
    /// `counts[i]` is the number of runs of position `start + i`, for each cached position.
    counts: Vec<u16>,
    runs: Vec<Run>,
}

impl<'a> MatchCache<'a> {
    /// Searches the positions of `data[start..end]` in order, caching each one whose runs still
    /// fit in `budget` runs in all, and stopping at the first that does not.
    fn build(data: &'a [u8], start: usize, end: usize, max_chain: usize, budget: usize) -> Self {
        let mut search = Search::new(data, start, end, max_chain);
        let mut counts = Vec::new();
        let mut runs = Vec::new();
        for pos in start..end {
            let found = search.runs(pos);
            if runs.len() + found.len() > budget {
                break;
            }
            runs.extend_from_slice(found);
            // A walk appends one run per strictly longer match, so at most 256 runs.
            counts.push(found.len() as u16);
        }
        // Growth by doubling can leave up to as much capacity again unused; give it back before
        // the passes allocate their own tables, so the budget bounds what the parse holds.
        counts.shrink_to_fit();
        runs.shrink_to_fit();
        Self {
            data,
            start,
            end,
            max_chain,
            counts,
            runs,
        }
    }

    /// A source for one parse of the span, from its start.
    fn reader(&self) -> CacheReader<'_, 'a> {
        CacheReader {
            cache: self,
            next: 0,
            offset: 0,
            search: None,
        }
    }
}

/// One parse's forward cursor over a [`MatchCache`], searching live past its cached positions.
struct CacheReader<'c, 'a> {
    cache: &'c MatchCache<'a>,
    /// The span-relative position whose runs begin at `offset`.
    next: usize,
    offset: usize,
    /// The live search for the uncached positions, started on the first one asked for.
    search: Option<Search<'a>>,
}

impl Matches for CacheReader<'_, '_> {
    fn runs(&mut self, pos: usize) -> &[Run] {
        let cache = self.cache;
        let i = pos - cache.start;
        let Some(&count) = cache.counts.get(i) else {
            return self
                .search
                .get_or_insert_with(|| {
                    Search::new(cache.data, cache.start, cache.end, cache.max_chain)
                })
                .runs(pos);
        };
        // Skip the runs of every position passed over since the last question.
        self.offset += cache.counts[self.next..i]
            .iter()
            .map(|&c| usize::from(c))
            .sum::<usize>();
        self.next = i;
        &cache.runs[self.offset..][..usize::from(count)]
    }
}

/// The longest match among `runs` as `(len, dist)`: the last run, if any.
fn longest(runs: &[Run]) -> Option<(u16, u16)> {
    runs.last().map(|run| (run.len, run.dist))
}

/// Parses `data` into an LZ77 token stream, searching up to `max_chain` candidates per position.
///
/// With `lazy` set, the parser uses lazy matching (RFC 1951 §4): after finding a match it checks
/// whether the next position starts a longer one and, if so, defers — emitting the current byte as a
/// literal. This finds better parses than pure greedy at a small time cost. A larger `max_chain`
/// finds more/longer matches, also at a time cost.
pub(crate) fn parse(data: &[u8], max_chain: usize, lazy: bool) -> Vec<Token> {
    let end = data.len();
    parse_range(
        data,
        0,
        end,
        lazy,
        &mut Search::new(data, 0, end, max_chain),
    )
}

/// Parses `data[start..end]` the same way [`parse`] does, with matches allowed to reference the
/// history before `start` but never to extend past `end`, taking each position's matches from
/// `matches`.
fn parse_range(
    data: &[u8],
    start: usize,
    end: usize,
    lazy: bool,
    matches: &mut dyn Matches,
) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut pos = start;
    while pos < end {
        let current = longest(matches.runs(pos));
        // A match covers at least `MIN_MATCH` bytes, so emitting one always advances the cursor.
        // Requiring that here rather than trusting the walk keeps termination a property of this
        // loop: a shorter run is not a codeable match anyway, and falls through to a literal.
        let Some((len, dist)) = current.filter(|&(len, _)| usize::from(len) >= MIN_MATCH) else {
            tokens.push(Token::Literal(data[pos]));
            pos += 1;
            continue;
        };
        // Lazy matching: if the next position begins a strictly longer match, defer this one.
        // No length or bounds guard is needed in front of the lookahead: a match at `pos` leaves
        // at least `MIN_MATCH - 1` bytes after `pos + 1` inside the span, the walk is bounded by
        // `end`, and a maximal match cannot be beaten, so `next_len > len` rejects that case on
        // its own.
        if lazy
            && let Some((next_len, _)) = longest(matches.runs(pos + 1))
            && next_len > len
        {
            tokens.push(Token::Literal(data[pos]));
            pos += 1;
            continue;
        }
        tokens.push(Token::Match { len, dist });
        pos += len as usize;
    }
    tokens
}

/// Parses `data` into a near-optimal LZ77 token stream using a zopfli-style iterated cost model.
///
/// Each pass runs a shortest-path dynamic program that minimises total bits under a per-symbol cost
/// model, then rebuilds the cost model from the resulting parse and repeats. The parse and its
/// entropy code co-adapt, finding cheaper parses than greedy/lazy. `iterations` bounds the passes.
///
/// The input is processed in consecutive spans of at most `span` bytes (raised to [`WINDOW`] if
/// smaller), each carrying its own cost model and each able to reference the history before it.
/// That bounds the dynamic program's working set and per-span cost without letting the *total*
/// input size decide whether the optimal parse runs at all: cost grows linearly in the number of
/// spans.
pub(crate) fn parse_optimal(
    data: &[u8],
    max_chain: usize,
    iterations: u32,
    span: usize,
) -> Vec<Token> {
    // A span shorter than the match window cannot pay for priming that window, and a zero span
    // would not advance at all, so the window is the floor.
    let span = span.max(WINDOW);
    let mut tokens = Vec::new();
    let mut start = 0;
    // Chunking (rather than an index walk) makes the advance structural: every span is non-empty
    // and the spans exactly tile the input.
    for chunk in data.chunks(span) {
        let end = start + chunk.len();
        let budget = run_budget(chunk.len());
        tokens.append(&mut optimal_span(
            data, start, end, max_chain, iterations, budget,
        ));
        start = end;
    }
    tokens
}

/// The [`MatchCache`] run budget, as runs per span byte. A cached position costs 2 bytes plus 4 per
/// run, so while the passes run the cache holds at most `2 + 4 * 2 = 10` bytes per span byte --
/// below the 12 the shortest-path table itself holds -- where an unbounded one could reach 1 KiB.
/// (Building it may briefly hold twice that, before any pass has allocated its table.)
///
/// Two covers the whole span on most material: a noisy synthetic photograph, gradients, flat
/// fills, periodic data and short-period prose measured 0.6 to 1.1 runs per byte (#959).
/// Low-entropy text-like inputs run higher (a six-letter skewed alphabet near 4, a Fibonacci word
/// near 6), and there only the span's leading positions are cached; the rest are searched live in
/// every pass, as before.
const RUNS_PER_BYTE: usize = 2;

/// The [`MatchCache`] budget for a span of `len` bytes.
fn run_budget(len: usize) -> usize {
    len * RUNS_PER_BYTE
}

/// Runs the iterated cost-model refinement over the single span `data[start..end]`.
///
/// The span's matches are searched once into a [`MatchCache`] of at most `budget` runs that the
/// lazy seed and every pass read back, searching live wherever it stops. Cached or live, a
/// position's runs are the same, so the budget moves time and memory, never the tokens.
fn optimal_span(
    data: &[u8],
    start: usize,
    end: usize,
    max_chain: usize,
    iterations: u32,
    budget: usize,
) -> Vec<Token> {
    let cache = MatchCache::build(data, start, end, max_chain, budget);
    // Seed the cost model from a lazy parse of this span.
    let mut tokens = parse_range(data, start, end, true, &mut cache.reader());
    for _ in 0..iterations {
        let (lit_cost, dist_cost) = costs(&tokens);
        let next = parse_dp(data, start, end, &mut cache.reader(), &lit_cost, &dist_cost);
        if next == tokens {
            break; // converged
        }
        tokens = next;
    }
    tokens
}

/// Per-symbol bit costs (the Huffman code lengths) for the literal/length and distance alphabets,
/// derived from a token stream's histogram. Symbols absent from the stream get the maximum cost so
/// the parse is discouraged from — but not forbidden — using them.
fn costs(tokens: &[Token]) -> (Vec<u16>, Vec<u16>) {
    let mut litlen_hist = vec![0u32; 286];
    let mut dist_hist = vec![0u32; 30];
    for &token in tokens {
        match token {
            Token::Literal(b) => litlen_hist[usize::from(b)] += 1,
            Token::Match { len, dist } => {
                litlen_hist[symbols::length_code(len).0 as usize] += 1;
                dist_hist[symbols::distance_code(dist).0 as usize] += 1;
            }
        }
    }
    litlen_hist[256] += 1; // end-of-block
    let litlen = CanonicalCode::from_histogram(&litlen_hist, 15);
    let dist = CanonicalCode::from_histogram(&dist_hist, 15);
    let to_cost = |l: u8| if l > 0 { u16::from(l) } else { 15 };
    (
        litlen.lengths().iter().map(|&l| to_cost(l)).collect(),
        dist.lengths().iter().map(|&l| to_cost(l)).collect(),
    )
}

/// One shortest-path pass over `data[start..end]`: finds the parse minimising total cost under
/// `lit_cost`/`dist_cost`. Matches, taken from `matches`, may reach back before `start` but never
/// past `end`.
fn parse_dp(
    data: &[u8],
    start: usize,
    end: usize,
    matches: &mut dyn Matches,
    lit_cost: &[u16],
    dist_cost: &[u16],
) -> Vec<Token> {
    let n = end - start;
    // `f[i]` = min cost in bits to encode `data[start..start + i]`; `blen`/`bdist` record the edge
    // taken to reach `i` (`blen == 0` means a literal). Indices are span-relative.
    let mut f = vec![u64::MAX; n + 1];
    let mut blen = vec![0u16; n + 1];
    let mut bdist = vec![0u16; n + 1];
    f[0] = 0;
    // Each length's own code cost (symbol plus extra bits), derived once per pass rather than at
    // every relaxation; the entries below `MIN_MATCH` are never read.
    let mut len_cost = [0u64; MAX_MATCH + 1];
    for (len, cost) in len_cost.iter_mut().enumerate().skip(MIN_MATCH) {
        let (lsym, lbits, _) = symbols::length_code(len as u16);
        *cost = u64::from(lit_cost[lsym as usize]) + u64::from(lbits);
    }
    for i in 0..n {
        // A literal always advances one byte.
        let lit = f[i] + u64::from(lit_cost[usize::from(data[start + i])]);
        if lit < f[i + 1] {
            f[i + 1] = lit;
            blen[i + 1] = 0;
            bdist[i + 1] = 0;
        }
        relax_matches(
            &mut f[i..],
            &mut blen[i..],
            &mut bdist[i..],
            matches.runs(start + i),
            &len_cost,
            dist_cost,
        );
    }
    // Backtrack from the end to recover the token sequence. `blen[i] == 0` marks a literal edge,
    // which covers one byte; any other value is the match length that reached `i`.
    //
    // Every edge covers at least one byte, so the walk visits at most `n` of them. Bounding the
    // loop by that count — rather than by the cursor alone — keeps termination a property of this
    // loop instead of an invariant of the table built above it.
    let mut tokens = Vec::new();
    let mut i = n;
    for _ in 0..n {
        if i == 0 {
            break;
        }
        let len = blen[i];
        tokens.push(match len {
            0 => Token::Literal(data[start + i - 1]),
            len => Token::Match {
                len,
                dist: bdist[i],
            },
        });
        i -= usize::from(len).max(1);
    }
    tokens.reverse();
    tokens
}

/// Relaxes every match edge out of one position of the shortest-path table. The slices start at
/// that position, so `f[0]` is its cost and `f[len]` the cost of the position `len` bytes on.
///
/// Every length from `MIN_MATCH` up to the longest match is reachable, each at its own nearest
/// distance: a short match need not pay the distance code of the longest one. The distance is
/// constant over each [`Run`], so its cost is derived once per run.
fn relax_matches(
    f: &mut [u64],
    blen: &mut [u16],
    bdist: &mut [u16],
    runs: &[Run],
    len_cost: &[u64; MAX_MATCH + 1],
    dist_cost: &[u16],
) {
    let mut first = MIN_MATCH;
    for run in runs {
        let (dsym, dbits, _) = symbols::distance_code(run.dist);
        let base = f[0] + u64::from(dist_cost[dsym as usize]) + u64::from(dbits);
        let last = usize::from(run.len);
        for len in first..=last {
            let cost = base + len_cost[len];
            if cost < f[len] {
                f[len] = cost;
                blen[len] = len as u16;
                bdist[len] = run.dist;
            }
        }
        first = last + 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reconstructs the original bytes from a token stream (the inverse of the LZ77 parse).
    fn reconstruct(tokens: &[Token]) -> Vec<u8> {
        let mut out = Vec::new();
        for &token in tokens {
            match token {
                Token::Literal(b) => out.push(b),
                Token::Match { len, dist } => {
                    let start = out.len() - usize::from(dist);
                    for k in 0..usize::from(len) {
                        out.push(out[start + k]);
                    }
                }
            }
        }
        out
    }

    /// `common_prefix_len` must return the index of the first differing byte wherever it falls: in
    /// the first eight-byte word, in a later word, or in the sub-word tail. The bytes are non-zero
    /// with several bits set, so an `|`/`&` in place of the XOR reports a difference where there is
    /// none, and the flipped bit is bit 0 of its byte, so `trailing_zeros % 8` (0) disagrees with
    /// `trailing_zeros / 8` (the byte index) at every offset that is not a multiple of eight.
    #[test]
    fn common_prefix_len_locates_the_first_difference_at_every_offset() {
        let a: Vec<u8> = (0..19u8).map(|i| 0x5A ^ i.wrapping_mul(0x33)).collect();
        assert!(a.iter().all(|&b| b != 0));
        for k in 0..a.len() {
            let mut b = a.clone();
            b[k] ^= 0x01;
            assert_eq!(common_prefix_len(&a, &b), k, "difference at offset {k}");
        }
    }

    /// Windows that never differ measure their whole common length: ending mid-word (the tail is
    /// walked to its end), on a word boundary (there is no tail), and when one window is longer
    /// (the result is bounded by the shorter). A boundary pin, not a mutant killer: the sweep above
    /// already kills every operator mutant, and this names the no-difference return it cannot reach.
    #[test]
    fn common_prefix_len_of_identical_windows_is_their_whole_length() {
        let a: Vec<u8> = (0..19u8).map(|i| 0xA5 ^ i.wrapping_mul(0x33)).collect();
        assert_eq!(common_prefix_len(&a, &a), 19);
        assert_eq!(common_prefix_len(&a[..16], &a[..16]), 16);
        assert_eq!(common_prefix_len(&a[..11], &a), 11);
    }

    /// Two candidates for the final `abcde`: the nearer (`abcX`, distance 7) reaches length 3, the
    /// farther (`abcde`, distance 15) reaches 5. Each length must be recorded at the nearest
    /// distance that achieves it -- 3 at 7, and 4 and 5 at 15 -- which is two runs, the second
    /// ending at the longest match. The runs are appended after whatever the buffer held.
    #[test]
    fn walk_records_each_length_at_its_nearest_distance() {
        let data = b"abcdeQQQabcXQQQabcde";
        let pos = 15;
        let mut matcher = Matcher::new();
        matcher.prime(data, pos);
        let earlier = Run { len: 9, dist: 9 };
        let mut runs = vec![earlier];
        matcher.walk(data, pos, 128, data.len(), &mut runs);
        assert_eq!(
            runs,
            [earlier, Run { len: 3, dist: 7 }, Run { len: 5, dist: 15 }]
        );
    }

    /// Low-entropy input with several runs at many positions, so a mis-skipped offset or a
    /// mis-counted budget lands on a different position's runs.
    fn skewed(len: usize) -> Vec<u8> {
        let mut seed = 0x9E37_79B9_u32;
        (0..len)
            .map(|_| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                b"aaaaaabbcd"[(seed >> 28) as usize % 10]
            })
            .collect()
    }

    /// The cache is the live search, stored: read back in the order a lazy parse asks -- skipping
    /// the positions a match covers and asking for one position twice -- every position's runs are
    /// the ones a fresh live search gives it, both inside the cached prefix and past it.
    #[test]
    fn a_cache_reader_answers_as_the_live_search_does() {
        let data = skewed(3000);
        let (start, end) = (500, 2600);
        let full = MatchCache::build(&data, start, end, 64, usize::MAX);
        assert_eq!(
            full.counts.len(),
            end - start,
            "an unlimited budget caches the span"
        );
        let partial = MatchCache::build(&data, start, end, 64, full.runs.len() / 2);
        assert!((1..end - start).contains(&partial.counts.len()));
        for cache in [&full, &partial] {
            let mut reader = cache.reader();
            let mut live = Search::new(&data, start, end, 64);
            for pos in (start..end).step_by(3) {
                for again in [pos, pos, pos + 1] {
                    assert_eq!(reader.runs(again), live.runs(again), "position {again}");
                }
            }
        }
    }

    /// The budget is a bound on the runs held, not a target: a budget of exactly the span's run
    /// total caches every position, and one run fewer stops at the last position with runs (the
    /// span's final two cannot start a match), keeping every run of the positions before it.
    #[test]
    fn a_match_cache_stops_at_the_first_position_past_its_budget() {
        let data = skewed(1200);
        let (start, end) = (100, 1100);
        let total = MatchCache::build(&data, start, end, 64, usize::MAX)
            .runs
            .len();
        let exact = MatchCache::build(&data, start, end, 64, total);
        assert_eq!((exact.counts.len(), exact.runs.len()), (end - start, total));
        let last = exact.counts.iter().rposition(|&c| c > 0).unwrap();
        assert_eq!(
            last,
            end - start - MIN_MATCH,
            "the last position that can start a match"
        );
        let short = MatchCache::build(&data, start, end, 64, total - 1);
        assert_eq!(short.counts.len(), last);
        assert_eq!(short.runs.len(), total - usize::from(exact.counts[last]));
        let summed: usize = short.counts.iter().map(|&c| usize::from(c)).sum();
        assert_eq!(summed, short.runs.len(), "counts describe the runs held");
    }

    /// The budget moves time and memory, never the tokens: a cache that stops at the first match
    /// (budget 0), one that stops partway (one run per byte, where this input has about four) and
    /// a whole one parse a span identically.
    #[test]
    fn the_cache_budget_never_changes_the_parse() {
        let data = skewed(WINDOW + 4000);
        let (start, end) = (3000, data.len());
        let whole = optimal_span(&data, start, end, 64, 3, usize::MAX);
        for budget in [0, end - start] {
            assert_eq!(
                optimal_span(&data, start, end, 64, 3, budget),
                whole,
                "budget {budget}"
            );
        }
        let mut covered = 0;
        for token in &whole {
            covered += match token {
                Token::Literal(_) => 1,
                Token::Match { len, .. } => usize::from(*len),
            };
        }
        assert_eq!(covered, end - start);
    }

    /// The per-span budget is the documented two runs per byte, the figure the cache's memory
    /// bound (`2 + 4 * 2` bytes per span byte) is stated for.
    #[test]
    fn the_run_budget_is_two_runs_per_span_byte() {
        assert_eq!(run_budget(0), 0);
        assert_eq!(run_budget(32_768), 65_536);
    }

    /// Two runs out of a position whose cost is 0: lengths 3 at distance 7 (distance code 5, one
    /// extra bit) and 4..=5 at distance 15 (code 7, two extra bits), with the far code priced
    /// cheaper than the near one. Each length must still be relaxed at its own run's distance
    /// only -- length 3 at 7 although 15 would be cheaper, since no match of length 3 at 15 is
    /// on offer -- and nothing below `MIN_MATCH` or past the longest match may be touched.
    #[test]
    fn relax_matches_prices_each_length_at_its_own_run() {
        let mut f = [0, 999, 999, 999, 999, 999, 999];
        let mut blen = [0u16; 7];
        let mut bdist = [0u16; 7];
        let mut len_cost = [0u64; MAX_MATCH + 1];
        len_cost[MIN_MATCH..=5].copy_from_slice(&[100, 200, 300]);
        let mut dist_cost = [0u16; 30];
        dist_cost[5] = 20;
        dist_cost[7] = 1;
        let runs = [Run { len: 3, dist: 7 }, Run { len: 5, dist: 15 }];
        relax_matches(&mut f, &mut blen, &mut bdist, &runs, &len_cost, &dist_cost);
        assert_eq!(f, [0, 999, 999, 100 + 21, 200 + 3, 300 + 3, 999]);
        assert_eq!(blen, [0, 0, 0, 3, 4, 5, 0]);
        assert_eq!(bdist, [0, 0, 0, 7, 15, 15, 0]);
    }

    /// With far distance codes priced high and near ones low, the shortest path through the second
    /// `abc` of `abcabcde` is a length-3 match at distance 3 plus two literals (9 + 16 = 25 bits),
    /// not the longest match: length 5 back to the opening `abcde`, 44 bytes away, costs 8 + 15 +
    /// 4 extra bits = 27 for the same five bytes. That holds only when length 3 is offered at its
    /// own distance -- relaxed at the longest match's distance it costs 27 as well and the far
    /// match wins -- so this pins that the DP sees the nearer one.
    #[test]
    fn optimal_parse_takes_a_short_match_at_its_nearer_distance() {
        let mut data = b"abcde".to_vec();
        data.extend(0x80u8..0xA4); // 36 distinct bytes: no repeated 3-gram, no match
        data.extend_from_slice(b"abcabcde");
        let far = 44;
        assert_eq!(&data[far - 3..far], b"abc");
        assert_eq!(&data[far..], b"abcde");
        // Literals and length symbols: 8 bits each. Distances 1..=4 (codes 0..=3): 1 bit; every
        // other distance code: 15 bits (plus its extra bits).
        let lit_cost = vec![8u16; 286];
        let mut dist_cost = vec![15u16; 30];
        dist_cost[..4].fill(1);
        let mut search = Search::new(&data, 0, data.len(), 128);
        let tokens = parse_dp(&data, 0, data.len(), &mut search, &lit_cost, &dist_cost);
        let leading = far; // "abcde", the filler and the first "abc" are all literals
        assert!(
            tokens[..leading]
                .iter()
                .all(|t| matches!(t, Token::Literal(_))),
            "{tokens:?}"
        );
        assert_eq!(
            &tokens[leading..],
            &[
                Token::Match { len: 3, dist: 3 },
                Token::Literal(b'd'),
                Token::Literal(b'e')
            ],
            "the short match must take the near distance"
        );
        assert_eq!(reconstruct(&tokens), data);
    }

    #[test]
    fn all_literals_when_no_repeats() {
        let data = [1u8, 2, 3, 4, 5];
        let tokens = parse(&data, 128, false);
        assert_eq!(tokens.len(), 5);
        assert!(tokens.iter().all(|t| matches!(t, Token::Literal(_))));
    }

    #[test]
    fn finds_a_repeated_block() {
        // "abcabc": positions 0-2 are literals, then a match (len 3, dist 3).
        let data = b"abcabc";
        let tokens = parse(data, 128, false);
        assert_eq!(tokens[0], Token::Literal(b'a'));
        assert_eq!(tokens[1], Token::Literal(b'b'));
        assert_eq!(tokens[2], Token::Literal(b'c'));
        assert_eq!(tokens[3], Token::Match { len: 3, dist: 3 });
    }

    #[test]
    fn long_run_uses_overlapping_match() {
        // A run of identical bytes becomes a literal then a long overlapping match at distance 1.
        let data = vec![0x5Au8; 300];
        let tokens = parse(&data, 128, false);
        assert_eq!(tokens[0], Token::Literal(0x5A));
        // The next token copies at distance 1, capped at the 258-byte maximum length.
        assert_eq!(tokens[1], Token::Match { len: 258, dist: 1 });
    }

    #[test]
    fn respects_minimum_match_length() {
        // A 2-byte repeat is too short to reference; it stays literal.
        let data = b"abxxab";
        let tokens = parse(data, 128, false);
        assert!(tokens.iter().all(|t| matches!(t, Token::Literal(_))));
    }

    #[test]
    fn optimal_parse_reconstructs_input() {
        let inputs: Vec<Vec<u8>> = vec![
            b"the quick brown fox jumps over the lazy dog. ".repeat(20),
            vec![0x42; 1000],
            (0..2000u32)
                .map(|i| (i.wrapping_mul(2_654_435_761) >> 25) as u8)
                .collect(),
            (0..2000u32).map(|i| (i % 17) as u8).collect(),
            Vec::new(),
        ];
        for data in &inputs {
            let tokens = parse_optimal(data, 256, 4, 512);
            assert_eq!(
                &reconstruct(&tokens),
                data,
                "optimal parse for {} bytes",
                data.len()
            );
        }
    }

    /// Spanning must not change what the parse *means*: every span size reconstructs the input
    /// exactly, including sizes that land mid-match and sizes larger than the input.
    #[test]
    fn spanned_optimal_parse_reconstructs_input() {
        let data = b"the quick brown fox jumps over the lazy dog. ".repeat(2400);
        assert!(
            data.len() > 3 * WINDOW,
            "the input must span several windows"
        );
        for span in [
            0usize,
            7,
            WINDOW,
            WINDOW + 1,
            WINDOW * 2 + 913,
            data.len() - 1,
            data.len(),
            data.len() * 2,
        ] {
            let tokens = parse_optimal(&data, 32, 2, span);
            assert_eq!(reconstruct(&tokens), data, "span {span}");
        }
    }

    /// A span boundary must not also be a match boundary: the second half of a doubled buffer is
    /// wholly a back-reference into the first, so parsing it as its own span still has to find
    /// those matches through the primed history rather than re-emitting literals.
    #[test]
    fn a_span_references_the_history_before_it() {
        let half = b"abracadabra alakazam presto changeo ".repeat(1000);
        let mut data = half.clone();
        data.extend_from_slice(&half);

        assert!(half.len() >= WINDOW, "each half must be a span of its own");
        let tokens = parse_optimal(&data, 32, 2, half.len());
        assert_eq!(reconstruct(&tokens), data);

        // Tokens covering the second span, i.e. everything after the first `half.len()` bytes.
        let mut covered = 0usize;
        let second: Vec<Token> = tokens
            .iter()
            .copied()
            .filter(|t| {
                let at = covered;
                covered += match t {
                    Token::Literal(_) => 1,
                    Token::Match { len, .. } => usize::from(*len),
                };
                at >= half.len()
            })
            .collect();
        // Priming lets the span be covered by maximal-length back-references; without it the
        // opening bytes would have to be re-emitted as literals.
        assert!(
            second.len() * 100 < half.len(),
            "second span should be long back-references, got {} tokens for {} bytes",
            second.len(),
            half.len()
        );
        // The span's very first token starts exactly at the boundary, so any back-reference it
        // makes necessarily reads bytes the span itself has not emitted — only the primed history
        // can supply them.
        assert!(
            matches!(second.first(), Some(Token::Match { .. })),
            "span should open with a back-reference into the primed history, got {:?}",
            second.first()
        );
    }

    /// The span bound must be applied, not merely accepted: no token may straddle a boundary.
    #[test]
    fn no_token_crosses_a_span_boundary() {
        let data = vec![0x5Au8; WINDOW * 3 + 500];
        let span = WINDOW;
        let mut at = 0usize;
        for token in parse_optimal(&data, 32, 2, span) {
            let len = match token {
                Token::Literal(_) => 1,
                Token::Match { len, .. } => usize::from(len),
            };
            assert_eq!(
                at / span,
                (at + len - 1) / span,
                "token at {at} (len {len}) crosses a {span}-byte boundary"
            );
            at += len;
        }
        assert_eq!(at, data.len());
    }

    /// A span shorter than the match window is raised to it: a sub-window span would re-prime
    /// more history than it parses, and a zero span would not advance the cursor at all.
    #[test]
    fn a_short_span_is_raised_to_the_window() {
        let data: Vec<u8> = (0..WINDOW as u32 * 2)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 25) as u8)
            .collect();
        let windowed = parse_optimal(&data, 32, 2, WINDOW);
        for span in [0usize, 1, WINDOW - 1] {
            assert_eq!(
                parse_optimal(&data, 32, 2, span),
                windowed,
                "span {span} should parse as the {WINDOW}-byte window does"
            );
        }
        assert_eq!(reconstruct(&windowed), data);
    }

    #[test]
    fn greedy_and_lazy_reconstruct_input() {
        let data = b"abracadabra abracadabra alakazam abracadabra".repeat(5);
        assert_eq!(reconstruct(&parse(&data, 128, false)), data);
        assert_eq!(reconstruct(&parse(&data, 128, true)), data);
    }
}
