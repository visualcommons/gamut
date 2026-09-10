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

    /// Hashes the 3 bytes at `pos` (caller guarantees `pos + 3 <= data.len()`).
    fn hash(data: &[u8], pos: usize) -> usize {
        // The three bytes are a big-endian `u32`, so they are read as one. Spelling it
        // `b0 << 16 | b1 << 8 | b2` puts each byte in its own lane, and disjoint lanes are where
        // `|` and `^` agree -- two unkillable mutants for no gain (#110).
        let key = u32::from_be_bytes([0, data[pos], data[pos + 1], data[pos + 2]]);
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
    fn insert(&mut self, data: &[u8], pos: usize) {
        if pos + MIN_MATCH > data.len() {
            return;
        }
        let h = Self::hash(data, pos);
        self.prev[pos & WMASK] = self.head[h];
        self.head[h] = pos as i32;
    }

    /// Finds the longest back-reference for the bytes at `pos`, walking at most `max_chain`
    /// candidates. Returns `(len, dist)` with `len >= MIN_MATCH` and `1 <= dist <= 32768`.
    ///
    /// Matches never extend past `limit` (`<= data.len()`), so a caller parsing one span of a
    /// larger buffer cannot emit a token that runs off the end of its span. Candidates *behind*
    /// `pos` are unrestricted, which is what lets a span reference the history before it.
    ///
    /// Each surviving candidate is measured with [`common_prefix_len`], eight bytes per step. The
    /// prune in front of it is still a single byte: it rejects most candidates outright, and a
    /// wide read there would touch bytes past `best_len` for nothing.
    fn find(&self, data: &[u8], pos: usize, max_chain: usize, limit: usize) -> Option<(u16, u16)> {
        if pos + MIN_MATCH > limit {
            return None;
        }
        let max_len = (limit - pos).min(MAX_MATCH);
        let lowest = pos.saturating_sub(WINDOW); // candidates must be >= this for dist <= window
        let mut best_len = MIN_MATCH - 1; // a real match must strictly exceed this
        let mut best_dist = 0usize;
        let mut cand = self.head[Self::hash(data, pos)];
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
                    best_len = len;
                    best_dist = pos - c;
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
        if best_len >= MIN_MATCH {
            Some((best_len as u16, best_dist as u16))
        } else {
            None
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

/// Parses `data` into an LZ77 token stream, searching up to `max_chain` candidates per position.
///
/// With `lazy` set, the parser uses lazy matching (RFC 1951 §4): after finding a match it checks
/// whether the next position starts a longer one and, if so, defers — emitting the current byte as a
/// literal. This finds better parses than pure greedy at a small time cost. A larger `max_chain`
/// finds more/longer matches, also at a time cost.
pub(crate) fn parse(data: &[u8], max_chain: usize, lazy: bool) -> Vec<Token> {
    parse_range(data, 0, data.len(), max_chain, lazy)
}

/// Parses `data[start..end]` the same way [`parse`] does, with matches allowed to reference the
/// history before `start` but never to extend past `end`.
fn parse_range(data: &[u8], start: usize, end: usize, max_chain: usize, lazy: bool) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut matcher = Matcher::new();
    matcher.prime(data, start);
    let mut pos = start;
    while pos < end {
        let current = matcher.find(data, pos, max_chain, end);
        matcher.insert(data, pos); // pos becomes a candidate for subsequent positions
        // A match covers at least `MIN_MATCH` bytes, so emitting one always advances the cursor.
        // Requiring that here rather than trusting `find` keeps termination a property of this
        // loop: a shorter run is not a codeable match anyway, and falls through to a literal.
        let Some((len, dist)) = current.filter(|&(len, _)| usize::from(len) >= MIN_MATCH) else {
            tokens.push(Token::Literal(data[pos]));
            pos += 1;
            continue;
        };
        // Lazy matching: if the next position begins a strictly longer match, defer this one.
        // No length or bounds guard is needed in front of the lookahead: `find` is already bounded
        // by `end` (and yields nothing once fewer than `MIN_MATCH` bytes remain), and a maximal
        // match cannot be beaten, so `next_len > len` rejects that case on its own.
        if lazy
            && let Some((next_len, _)) = matcher.find(data, pos + 1, max_chain, end)
            && next_len > len
        {
            tokens.push(Token::Literal(data[pos]));
            pos += 1;
            continue;
        }
        tokens.push(Token::Match { len, dist });
        // `pos` is already inserted; insert the rest of the covered span so future matches can
        // reference inside it.
        let covered = pos + len as usize;
        for p in (pos + 1)..covered {
            matcher.insert(data, p);
        }
        pos = covered;
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
        tokens.append(&mut optimal_span(data, start, end, max_chain, iterations));
        start = end;
    }
    tokens
}

/// Runs the iterated cost-model refinement over the single span `data[start..end]`.
fn optimal_span(
    data: &[u8],
    start: usize,
    end: usize,
    max_chain: usize,
    iterations: u32,
) -> Vec<Token> {
    // Seed the cost model from a lazy parse of this span.
    let mut tokens = parse_range(data, start, end, max_chain, true);
    for _ in 0..iterations {
        let (lit_cost, dist_cost) = costs(&tokens);
        let next = parse_dp(data, start, end, max_chain, &lit_cost, &dist_cost);
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
/// `lit_cost`/`dist_cost`. Matches may reach back before `start` but never past `end`.
fn parse_dp(
    data: &[u8],
    start: usize,
    end: usize,
    max_chain: usize,
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
    let mut matcher = Matcher::new();
    matcher.prime(data, start);
    for i in 0..n {
        let fi = f[i];
        // A literal always advances one byte.
        let lit = fi + u64::from(lit_cost[usize::from(data[start + i])]);
        if lit < f[i + 1] {
            f[i + 1] = lit;
            blen[i + 1] = 0;
            bdist[i + 1] = 0;
        }
        let found = matcher.find(data, start + i, max_chain, end);
        matcher.insert(data, start + i);
        if let Some((max_len, dist)) = found {
            let (dsym, dbits, _) = symbols::distance_code(dist);
            let dcost = u64::from(dist_cost[dsym as usize]) + u64::from(dbits);
            // Every length from MIN_MATCH up to the longest match is reachable at this distance.
            for len in MIN_MATCH..=max_len as usize {
                let (lsym, lbits, _) = symbols::length_code(len as u16);
                let cost = fi + u64::from(lit_cost[lsym as usize]) + u64::from(lbits) + dcost;
                if cost < f[i + len] {
                    f[i + len] = cost;
                    blen[i + len] = len as u16;
                    bdist[i + len] = dist;
                }
            }
        }
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
