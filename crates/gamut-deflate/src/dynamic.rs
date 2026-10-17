//! Dynamic-Huffman DEFLATE blocks (BTYPE = 10, RFC 1951 §3.2.7) and cost-driven block splitting.
//!
//! A dynamic block carries its own literal/length and distance codes, described by a sequence of
//! per-symbol code lengths that is itself run-length-coded (symbols 16/17/18) and Huffman-coded with
//! a small "code-length" alphabet. This is where most of DEFLATE's density comes from.
//!
//! When the token statistics shift across the input, one set of codes is suboptimal. [`multi_body`]
//! recursively splits the token stream where two independently-coded blocks cost fewer bits than
//! one (zopfli's block-splitting idea), trading encode time for size.

use crate::bitwriter::BitWriter;
use crate::huffman::CanonicalCode;
use crate::lz77::Token;
use crate::symbols;

/// Literal/length alphabet size used for the code (0..=285; 286/287 never occur).
const NUM_LITLEN: usize = 286;
/// Distance alphabet size (0..=29).
const NUM_DIST: usize = 30;
/// Code-length alphabet size (lengths 0..=15 plus repeat codes 16/17/18).
const NUM_CL: usize = 19;
/// The order in which code-length code lengths are written (RFC 1951 §3.2.7).
const CL_ORDER: [usize; NUM_CL] = [
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];

/// A run-length-coded code-length symbol plus any raw extra bits that follow it.
struct ClItem {
    sym: u16,
    extra_bits: u32,
    extra_val: u32,
}

/// Everything needed to emit (or price) one dynamic block: the three codes, the code-length RLE, and
/// the header counts.
struct Built {
    litlen: CanonicalCode,
    dist: CanonicalCode,
    cl: CanonicalCode,
    items: Vec<ClItem>,
    hlit: usize,
    hdist: usize,
    hclen: usize,
}

/// Encodes a token stream as a single complete dynamic block (the common case).
pub(crate) fn body(tokens: &[Token]) -> Vec<u8> {
    let built = build(tokens);
    let mut w = BitWriter::new();
    write_block(&mut w, &built, tokens, true);
    w.finish()
}

/// Encodes a token stream as one *or more* dynamic blocks, splitting where it saves bits.
pub(crate) fn multi_body(tokens: &[Token]) -> Vec<u8> {
    if tokens.is_empty() {
        return body(tokens);
    }
    let mut points = vec![0usize];
    recurse(tokens, 0, tokens.len(), &mut points, MAX_SPLIT_DEPTH);
    points.push(tokens.len());
    points.sort_unstable();
    points.dedup();

    let mut w = BitWriter::new();
    for k in 0..points.len() - 1 {
        let seg = &tokens[points[k]..points[k + 1]];
        let built = build(seg);
        write_block(&mut w, &built, seg, k == points.len() - 2);
    }
    w.finish()
}

/// Builds the codes and header description for a token range.
fn build(tokens: &[Token]) -> Built {
    let mut litlen_hist = [0u32; NUM_LITLEN];
    let mut dist_hist = [0u32; NUM_DIST];
    for &token in tokens {
        match token {
            Token::Literal(b) => litlen_hist[usize::from(b)] += 1,
            Token::Match { len, dist } => {
                litlen_hist[symbols::length_code(len).0 as usize] += 1;
                dist_hist[symbols::distance_code(dist).0 as usize] += 1;
            }
        }
    }
    litlen_hist[256] += 1; // end-of-block is always emitted
    // DEFLATE needs each code complete; forcing >= 2 symbols guarantees that (zlib does the same)
    // and sidesteps the incomplete single-code edge cases in §3.2.7.
    ensure_two(&mut litlen_hist);
    ensure_two(&mut dist_hist);

    let litlen = CanonicalCode::from_histogram(&litlen_hist, 15);
    let dist = CanonicalCode::from_histogram(&dist_hist, 15);

    let hlit = trimmed_len(litlen.lengths(), 257);
    let hdist = trimmed_len(dist.lengths(), 1);

    // The lit/len and distance code lengths form a single sequence (RFC 1951 §3.2.7).
    let mut combined = Vec::with_capacity(hlit + hdist);
    combined.extend_from_slice(&litlen.lengths()[..hlit]);
    combined.extend_from_slice(&dist.lengths()[..hdist]);
    let items = rle(&combined);

    let mut cl_hist = [0u32; NUM_CL];
    for item in &items {
        cl_hist[item.sym as usize] += 1;
    }
    let cl = CanonicalCode::from_histogram(&cl_hist, 7);

    // HCLEN: code-length code lengths in CL_ORDER, through the last nonzero one (minimum of 4).
    //
    // Found by search rather than by trimming down from `NUM_CL`: the trim loop's `hclen > 4`
    // bound had an unkillable `>=` twin (#493). The lit/len code always has a used symbol, the RLE
    // writes each nonzero run's length literally once, and lengths 1..=15 sit at `CL_ORDER[4..]` —
    // so the trim never reached 4. Its `-= 1` mutated to `/= 1` could only hang.
    let hclen = CL_ORDER
        .iter()
        .rposition(|&sym| cl.lengths()[sym] != 0)
        .map_or(0, |last| last + 1)
        .max(4);

    Built {
        litlen,
        dist,
        cl,
        items,
        hlit,
        hdist,
        hclen,
    }
}

/// Writes one dynamic block for `tokens` (which must be the same range `built` was built from).
fn write_block(w: &mut BitWriter, built: &Built, tokens: &[Token], is_final: bool) {
    w.write_bits(u32::from(is_final), 1); // BFINAL
    w.write_bits(0b10, 2); // BTYPE = 10 (dynamic Huffman)
    w.write_bits((built.hlit - 257) as u32, 5);
    w.write_bits((built.hdist - 1) as u32, 5);
    w.write_bits((built.hclen - 4) as u32, 4);
    for &order in CL_ORDER.iter().take(built.hclen) {
        w.write_bits(u32::from(built.cl.lengths()[order]), 3);
    }
    for item in &built.items {
        built.cl.emit(w, item.sym as usize);
        w.write_bits(item.extra_val, item.extra_bits);
    }
    for &token in tokens {
        match token {
            Token::Literal(b) => built.litlen.emit(w, usize::from(b)),
            Token::Match { len, dist } => {
                let (lsym, lbits, lextra) = symbols::length_code(len);
                built.litlen.emit(w, lsym as usize);
                w.write_bits(lextra, lbits);
                let (dsym, dbits, dextra) = symbols::distance_code(dist);
                built.dist.emit(w, dsym as usize);
                w.write_bits(dextra, dbits);
            }
        }
    }
    built.litlen.emit(w, 256); // end of block
}

/// Exact bit cost of encoding `tokens` as the single dynamic block described by `built`.
fn block_bits(built: &Built, tokens: &[Token]) -> u64 {
    // 3 header bits + HLIT/HDIST/HCLEN (5+5+4) + the code-length code lengths.
    let mut bits = 3 + 14 + built.hclen as u64 * 3;
    for item in &built.items {
        bits += u64::from(built.cl.lengths()[item.sym as usize]) + u64::from(item.extra_bits);
    }
    for &token in tokens {
        match token {
            Token::Literal(b) => bits += u64::from(built.litlen.lengths()[usize::from(b)]),
            Token::Match { len, dist } => {
                let (lsym, lbits, _) = symbols::length_code(len);
                bits += u64::from(built.litlen.lengths()[lsym as usize]) + u64::from(lbits);
                let (dsym, dbits, _) = symbols::distance_code(dist);
                bits += u64::from(built.dist.lengths()[dsym as usize]) + u64::from(dbits);
            }
        }
    }
    bits + u64::from(built.litlen.lengths()[256]) // end of block
}

/// Bit cost of `tokens` as one dynamic block.
fn cost(tokens: &[Token]) -> u64 {
    block_bits(&build(tokens), tokens)
}

/// Maximum recursion depth of the splitter (so at most `2^depth` blocks).
const MAX_SPLIT_DEPTH: u32 = 6;
/// Don't split a segment below this many tokens — tiny blocks lose to their own header overhead.
const MIN_SPLIT_TOKENS: usize = 512;
/// Number of candidate split positions probed per segment.
const SPLIT_CANDIDATES: usize = 16;

/// Recursively records split points (token indices) inside `[start, end)` where splitting reduces
/// the total bit cost.
fn recurse(tokens: &[Token], start: usize, end: usize, points: &mut Vec<usize>, depth: u32) {
    if depth == 0 || end - start < 2 * MIN_SPLIT_TOKENS {
        return;
    }
    let whole = cost(&tokens[start..end]);
    let stride = ((end - start) / SPLIT_CANDIDATES).max(MIN_SPLIT_TOKENS);
    let mut best: Option<(u64, usize)> = None;
    // A range, not a `while j < end` cursor: that bound's `<=` twin added a candidate at `j ==
    // end` whose second half is empty, so it costs the whole segment plus an empty block's header
    // and can never beat not splitting — unkillable (#493).
    for j in (start + stride..end).step_by(stride) {
        let combined = cost(&tokens[start..j]) + cost(&tokens[j..end]);
        if best.is_none_or(|(b, _)| combined < b) {
            best = Some((combined, j));
        }
    }
    if let Some((combined, j)) = best
        && combined < whole
    {
        points.push(j);
        recurse(tokens, start, j, points, depth - 1);
        recurse(tokens, j, end, points, depth - 1);
    }
}

/// Forces a histogram to have at least two used symbols so its Huffman code is complete.
fn ensure_two(hist: &mut [u32]) {
    let used = hist.iter().filter(|&&h| h > 0).count();
    if used >= 2 {
        return;
    }
    if used == 0 {
        hist[0] = 1;
        hist[1] = 1;
    } else {
        let only = hist.iter().position(|&h| h > 0).unwrap_or(0);
        hist[usize::from(only == 0)] = 1;
    }
}

/// Number of leading code lengths to emit: the index past the last nonzero length, but at least
/// `min` (RFC 1951 requires HLIT+257 and HDIST+1 entries, covering every used symbol).
///
/// A search, not a trim loop counting down, for the same reason as `build`'s HCLEN: a bounded
/// search has no `-=` for a mutant to turn into a hang (#493).
fn trimmed_len(lengths: &[u8], min: usize) -> usize {
    lengths
        .iter()
        .rposition(|&l| l != 0)
        .map_or(0, |last| last + 1)
        .max(min)
}

/// Run-length-codes a code-length sequence into code-length-alphabet symbols (RFC 1951 §3.2.7):
/// 16 = repeat the previous length 3–6 times, 17 = repeat zero 3–10 times, 18 = repeat zero 11–138
/// times. Code 16 only ever repeats within a run of one value, so "previous length" is unambiguous.
///
/// The runs come from `chunk_by`, not from a hand-written cursor: the cursor's `j = i + 1` start
/// had an unkillable `i * 1` twin (the scan re-tests `lengths[i]` and lands in the same place) and
/// its `j += 1` a `*=` twin that hung (#493). Iterating the slice's own runs leaves neither.
fn rle(lengths: &[u8]) -> Vec<ClItem> {
    let mut items = Vec::new();
    for run in lengths.chunk_by(|a, b| a == b) {
        let value = run[0];
        let mut run = run.len();
        if value == 0 {
            while run >= 11 {
                let count = run.min(138);
                items.push(ClItem {
                    sym: 18,
                    extra_bits: 7,
                    extra_val: (count - 11) as u32,
                });
                run -= count;
            }
            while run >= 3 {
                let count = run.min(10);
                items.push(ClItem {
                    sym: 17,
                    extra_bits: 3,
                    extra_val: (count - 3) as u32,
                });
                run -= count;
            }
            items.extend((0..run).map(|_| ClItem {
                sym: 0,
                extra_bits: 0,
                extra_val: 0,
            }));
        } else {
            // Emit the value literally once, then repeat it with code 16.
            items.push(ClItem {
                sym: u16::from(value),
                extra_bits: 0,
                extra_val: 0,
            });
            run -= 1;
            while run >= 3 {
                let count = run.min(6);
                items.push(ClItem {
                    sym: 16,
                    extra_bits: 2,
                    extra_val: (count - 3) as u32,
                });
                run -= count;
            }
            items.extend((0..run).map(|_| ClItem {
                sym: u16::from(value),
                extra_bits: 0,
                extra_val: 0,
            }));
        }
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Expands a code-length RLE back to the flat length sequence it encodes.
    fn expand(items: &[ClItem]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut prev = 0u8;
        for item in items {
            match item.sym {
                0..=15 => {
                    out.push(item.sym as u8);
                    prev = item.sym as u8;
                }
                16 => out.resize(out.len() + (3 + item.extra_val) as usize, prev),
                17 => out.resize(out.len() + (3 + item.extra_val) as usize, 0),
                18 => out.resize(out.len() + (11 + item.extra_val) as usize, 0),
                _ => panic!("unexpected code-length symbol {}", item.sym),
            }
        }
        out
    }

    #[test]
    fn rle_round_trips_various_runs() {
        let cases: &[Vec<u8>] = &[
            vec![],
            vec![5],
            vec![0; 200],                 // long zero run -> 18s
            vec![7; 100],                 // long nonzero run -> literal + 16s
            vec![0, 0, 0, 8, 8, 8, 8, 0], // mixed
            vec![3, 0, 0, 0, 0, 0, 6, 6], // short zero run -> 17
            (0..40u8).map(|i| i % 16).collect(),
        ];
        for case in cases {
            assert_eq!(&expand(&rle(case)), case, "RLE failed for {case:?}");
        }
    }

    #[test]
    fn trimmed_len_keeps_minimum() {
        assert_eq!(trimmed_len(&[1, 1, 0, 0], 1), 2);
        assert_eq!(trimmed_len(&[0, 0, 0, 0], 1), 1); // never below the minimum
        assert_eq!(trimmed_len(&[1, 0, 1, 0], 1), 3); // stops past the last nonzero
    }

    #[test]
    fn ensure_two_adds_symbols() {
        let mut none = [0u32; 5];
        ensure_two(&mut none);
        assert_eq!(none.iter().filter(|&&h| h > 0).count(), 2);
        let mut one = [0u32, 9, 0];
        ensure_two(&mut one);
        assert_eq!(one.iter().filter(|&&h| h > 0).count(), 2);
    }

    #[test]
    fn splitting_never_grows_a_homogeneous_stream() {
        // For uniform statistics the splitter should find no beneficial split, so multi_body ties
        // the single-block body (both one block).
        let tokens: Vec<Token> = (0..4000u32)
            .map(|i| Token::Literal((i % 7) as u8))
            .collect();
        assert!(multi_body(&tokens).len() <= body(&tokens).len());
    }

    #[test]
    fn splitting_helps_heterogeneous_stream() {
        // Two regions with very different symbol statistics: one set of codes fits both poorly, so
        // splitting into two blocks must win.
        let mut tokens: Vec<Token> = (0..3000).map(|_| Token::Literal(0)).collect();
        tokens.extend((0..3000u32).map(|i| Token::Literal((i % 200 + 30) as u8)));
        assert!(multi_body(&tokens).len() < body(&tokens).len());
    }
}
