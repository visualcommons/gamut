//! Measurement arm for experiment #480: the filter-selection techniques oxipng ships that
//! `gamut-png` does not -- BigEnt, two-tier trials, `AtomicMin` pruning and per-line trial
//! deflate -- measured against the shipped heuristics on the crate's own corpus.
//!
//! Not a test and not a benchmark. It prints one CSV row per input x variant x sample, and panics
//! only where its own reconstruction of the shipped encoder disagrees with the shipped encoder (see
//! "Cross-checks" below), because a measurement taken on a reconstruction that is wrong would be
//! about the wrong thing. `experiments/480/run.sh` (`mise run experiment 480`) runs it and records
//! provenance; the verdict and the tables live in `experiments/480/RESULTS.md`.
//!
//! Everything is measured at the **IDAT** (zlib stream) level on the **unreduced** samples, the
//! same basis as `STATUS.md`'s per-heuristic table and `benches/encode.rs`'s
//! `print_heuristic_table`: `Level::Best` at the default effort and optimal-parse limit, which is
//! what `PngEncoder` uses. Nothing here patches the crate; every variant is built from the public
//! `stages::filter_image` and `gamut_deflate::DeflateEncoder`.
//!
//! ## Variants
//!
//! Per-scanline candidates, each one whole image filtered one way and DEFLATEd at `Level::Best`
//! (`time_ns` = filter selection + DEFLATE):
//!
//! - `none`, `sub`, `up`, `average`, `paeth` -- `FilterStrategy::None` / `Fixed(_)`.
//! - `minsumabs`, `bigrams`, `entropy` -- the shipped per-scanline heuristics, via
//!   `stages::filter_image`.
//! - `bigent` -- oxipng's BigEnt: per scanline, the filter whose residual *bigrams* have the lowest
//!   Shannon entropy, `sum c*log2(n/c)` over the histogram of adjacent byte pairs.
//! - `trial4` -- oxipng's `Brute`: per scanline, the filter whose row, appended to the previous
//!   four chosen rows, DEFLATEs smallest at `Level::Default` (lazy matching, smallest of
//!   stored/fixed/dynamic block). `Level::Fast` is fixed-Huffman only and blind to the symbol
//!   statistics the trial exists to measure, so it is not the trial codec.
//!
//! Whole-image strategies:
//!
//! - `brute7` -- the shipped `FilterStrategy::BruteForce`, end to end through `PngEncoder`
//!   (`auto_reduce` off), `idat_bytes` read back with `deconstruct`.
//! - `twotier-{fast,default}-top{1,2}` -- the same seven candidates DEFLATEd at the cheap tier;
//!   the `k` smallest there (ties to the earlier candidate) are re-encoded at `Level::Best` and
//!   the smallest of those kept. `time_ns` covers all of it.
//! - `atomicmin-model` -- **modelled, not measured** (`timing` = `model`). The bytes are `brute7`'s
//!   by construction: pruning only abandons a trial that would lose. The time is this sample's
//!   seven candidate times, less what a trial that ends above the best-so-far `B` could have saved
//!   by bailing the moment its output passed `B`, assuming output grows linearly with input:
//!   `deflate_ns * (1 - B/S)`. `gamut-deflate` builds a whole DEFLATE body before it returns any
//!   of it, so nothing can bail today; this is the ceiling an upstream budget hook would buy.
//!
//! ## Cross-checks
//!
//! `bigent` and `trial4` choose per row among the five `Fixed` images: a filter reads only the
//! *unfiltered* current and previous rows, so row `y` of `Fixed(f)` is exactly the bytes filter `f`
//! gives row `y` inside any per-scanline strategy. The arm proves that splice by choosing with its
//! own sum-of-absolutes scorer and requiring the result to equal `stages::filter_image(MinSumAbs)`
//! byte for byte. It proves the brute-force reconstruction by requiring the smallest of the seven
//! candidate streams to equal `brute7`'s IDAT length.
//!
//! Columns: `input,layout,width,height,variant,sample,idat_bytes,crc32,time_ns,timing`. `crc32`
//! identifies the zlib stream (empty where the arm never holds it: `brute7`, the model row).

use std::fmt::Write as _;
use std::path::PathBuf;
use std::time::Instant;

use gamut_core::{Dimensions, EncodeImage, ImageRef, Pixel, Rgb8, Rgb16, Rgba8};
use gamut_deflate::DeflateEncoder;
use gamut_png::stages::{self, Crc32};
use gamut_png::{FilterStrategy, FilterType, Level, PngEncoder, deconstruct};

#[path = "../tests/common/corpus.rs"]
mod corpus;

/// Interleaved sample layout of an input.
#[derive(Clone, Copy)]
enum Layout {
    Rgb8,
    Rgba8,
    /// 16-bit RGB, stored big-endian as the corpus generates it.
    Rgb16,
}

impl Layout {
    fn name(self) -> &'static str {
        match self {
            Self::Rgb8 => "rgb8",
            Self::Rgba8 => "rgba8",
            Self::Rgb16 => "rgb16",
        }
    }

    /// Bytes per pixel: the filter stride.
    fn bpp(self) -> usize {
        match self {
            Self::Rgb8 => 3,
            Self::Rgba8 => 4,
            Self::Rgb16 => 6,
        }
    }
}

/// One measured input.
struct Input {
    name: &'static str,
    layout: Layout,
    side: u32,
    samples: Vec<u8>,
}

impl Input {
    fn row_bytes(&self) -> usize {
        self.side as usize * self.layout.bpp()
    }
}

/// The bench corpus (`benches/encode.rs`, the rows `STATUS.md`'s heuristic table reports), plus
/// the two size-contract generators it lacks, all at 256 except the window-sized `tiny_rgb8`.
fn inputs() -> Vec<Input> {
    let at = |name, layout, side, samples| Input {
        name,
        layout,
        side,
        samples,
    };
    vec![
        at(
            "gradient_rgb8",
            Layout::Rgb8,
            256,
            corpus::gradient_rgb(256),
        ),
        at("photo_rgb8", Layout::Rgb8, 256, corpus::photo_rgb(256)),
        at("noise_rgb8", Layout::Rgb8, 256, corpus::noise_rgb(256)),
        at("grey_as_rgb8", Layout::Rgb8, 256, corpus::grey_as_rgb(256)),
        at(
            "palette64_rgba8",
            Layout::Rgba8,
            256,
            corpus::palette64_rgba(256),
        ),
        at("sprite_rgba8", Layout::Rgba8, 256, corpus::sprite_rgba(256)),
        at(
            "opaque256_rgba8",
            Layout::Rgba8,
            256,
            corpus::opaque256_rgba(256),
        ),
        at(
            "demotable_rgb16",
            Layout::Rgb16,
            256,
            corpus::demotable_rgb16(256),
        ),
        at("flat_rgba8", Layout::Rgba8, 256, corpus::flat_rgba(256)),
        at("tiny_rgb8", Layout::Rgb8, 16, corpus::gradient_rgb(16)),
    ]
}

/// The five filters, in the order the shipped chooser tries them (ties go to the earlier one).
const FILTERS: [(FilterType, &str); 5] = [
    (FilterType::None, "none"),
    (FilterType::Sub, "sub"),
    (FilterType::Up, "up"),
    (FilterType::Average, "average"),
    (FilterType::Paeth, "paeth"),
];

/// `PngEncoder`'s DEFLATE settings at `level`: its defaults are `DeflateEncoder`'s.
fn deflater(level: Level) -> DeflateEncoder {
    DeflateEncoder::new().with_level(level)
}

fn zlib(level: Level, filtered: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    deflater(level).zlib_compress(filtered, &mut out);
    out
}

fn crc(bytes: &[u8]) -> String {
    let mut c = Crc32::new();
    c.update(bytes);
    format!("{:08x}", c.finish())
}

/// The shipped sum-of-absolute-residuals score (bytes read as signed magnitudes).
fn sum_abs(row: &[u8]) -> u64 {
    row.iter()
        .map(|&x| u64::from(x.min(x.wrapping_neg())))
        .sum()
}

/// oxipng's BigEnt: Shannon entropy of the adjacent-byte-pair histogram, as `sum c*log2(n/c)` in
/// 1/256ths of a bit (the shipped `Entropy` score's scale, one order up). `counts` is all-zero on
/// entry and on return; only the touched bins are reset.
fn bigent(row: &[u8], counts: &mut [u32], touched: &mut Vec<usize>) -> u64 {
    for pair in row.windows(2) {
        let i = usize::from(u16::from_be_bytes([pair[0], pair[1]]));
        if counts[i] == 0 {
            touched.push(i);
        }
        counts[i] += 1;
    }
    let n = row.len().saturating_sub(1) as f64;
    let mut bits = 0.0;
    for &i in touched.iter() {
        let c = f64::from(counts[i]);
        bits += c * (n / c).log2();
        counts[i] = 0;
    }
    touched.clear();
    (bits * 256.0) as u64
}

/// The five `Fixed` images of `input`, each `height` rows of filter byte + residuals.
fn fixed_images(input: &Input) -> Vec<Vec<u8>> {
    FILTERS
        .iter()
        .map(|&(f, _)| {
            stages::filter_image(
                FilterStrategy::Fixed(f),
                &input.samples,
                input.row_bytes(),
                input.layout.bpp(),
            )
        })
        .collect()
}

/// Splices a per-row choice out of the five `Fixed` images: `pick(y, candidates, chosen_so_far)`
/// returns the index into [`FILTERS`] for row `y`, where each candidate is a whole filtered row
/// (filter byte first).
fn splice(
    fixed: &[Vec<u8>],
    row_bytes: usize,
    mut pick: impl FnMut(&[&[u8]; 5], &[u8]) -> usize,
) -> Vec<u8> {
    let stride = row_bytes + 1;
    let height = fixed[0].len() / stride;
    let mut out = Vec::with_capacity(fixed[0].len());
    for y in 0..height {
        let span = y * stride..(y + 1) * stride;
        let rows: [&[u8]; 5] = std::array::from_fn(|f| &fixed[f][span.clone()]);
        let chosen = pick(&rows, &out);
        out.extend_from_slice(rows[chosen]);
    }
    out
}

/// The first index of the minimum of `scores` -- the shipped chooser's tie-break.
fn first_min(scores: [u64; 5]) -> usize {
    let mut best = 0;
    for (i, &s) in scores.iter().enumerate() {
        if s < scores[best] {
            best = i;
        }
    }
    best
}

fn choose_sum_abs(fixed: &[Vec<u8>], row_bytes: usize) -> Vec<u8> {
    splice(fixed, row_bytes, |rows, _| {
        first_min(std::array::from_fn(|f| sum_abs(&rows[f][1..])))
    })
}

fn choose_bigent(fixed: &[Vec<u8>], row_bytes: usize) -> Vec<u8> {
    let mut counts = vec![0u32; 1 << 16];
    let mut touched = Vec::new();
    splice(fixed, row_bytes, |rows, _| {
        first_min(std::array::from_fn(|f| {
            bigent(&rows[f][1..], &mut counts, &mut touched)
        }))
    })
}

/// Rows of prior context a [`choose_trial`] candidate is compressed after.
const TRIAL_CONTEXT_ROWS: usize = 4;

fn choose_trial(fixed: &[Vec<u8>], row_bytes: usize) -> Vec<u8> {
    let stride = row_bytes + 1;
    let codec = deflater(Level::Default);
    let mut buf = Vec::new();
    let mut sink = Vec::new();
    splice(fixed, row_bytes, |rows, so_far| {
        let context = &so_far[so_far.len().saturating_sub(TRIAL_CONTEXT_ROWS * stride)..];
        first_min(std::array::from_fn(|f| {
            buf.clear();
            buf.extend_from_slice(context);
            buf.extend_from_slice(rows[f]);
            sink.clear();
            codec.compress(&buf, &mut sink) as u64
        }))
    })
}

/// One CSV row's measured payload.
struct Measured {
    variant: String,
    idat_bytes: usize,
    crc32: String,
    time_ns: u128,
    timing: &'static str,
}

fn measured(variant: &str, stream: &[u8], time_ns: u128) -> Measured {
    Measured {
        variant: variant.to_owned(),
        idat_bytes: stream.len(),
        crc32: crc(stream),
        time_ns,
        timing: "measured",
    }
}

/// Filters with `select` and DEFLATEs at `Level::Best`, timing both.
fn candidate(variant: &str, select: impl FnOnce() -> Vec<u8>) -> (Measured, Vec<u8>, u128) {
    let start = Instant::now();
    let filtered = select();
    let filter_ns = start.elapsed().as_nanos();
    let stream = zlib(Level::Best, &filtered);
    let total = start.elapsed().as_nanos();
    (
        measured(variant, &stream, total),
        filtered,
        total - filter_ns,
    )
}

fn encode_as<P: Pixel>(enc: &PngEncoder, data: &[P::Sample], dims: Dimensions) -> Vec<u8>
where
    PngEncoder: EncodeImage<P>,
{
    let image = ImageRef::<P>::new(data, dims).expect("buffer matches dimensions");
    let mut out = Vec::new();
    enc.encode_image(image, &mut out).expect("encode");
    out
}

/// The shipped `BruteForce` end to end, returning the IDAT length and the encode's nanoseconds.
fn brute7(input: &Input) -> (usize, u128) {
    let enc = PngEncoder::new()
        .with_compression(Level::Best)
        .with_filter(FilterStrategy::BruteForce)
        .with_auto_reduce(false);
    let dims = Dimensions::new(input.side, input.side).expect("valid dimensions");
    let s = &input.samples;
    let wide: Vec<u16> = match input.layout {
        Layout::Rgb16 => s
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&p| u16::from_be_bytes(p))
            .collect(),
        _ => Vec::new(),
    };
    let start = Instant::now();
    let png = match input.layout {
        Layout::Rgb8 => encode_as::<Rgb8>(&enc, s, dims),
        Layout::Rgba8 => encode_as::<Rgba8>(&enc, s, dims),
        Layout::Rgb16 => encode_as::<Rgb16>(&enc, &wide, dims),
    };
    let ns = start.elapsed().as_nanos();
    let idat = deconstruct(&png)
        .expect("gamut's own output deconstructs")
        .idat_compressed;
    (idat, ns)
}

/// The seven `BRUTE_FORCE_STRATEGIES` in the encoder's preference order.
fn brute_candidates(input: &Input) -> [Vec<u8>; 7] {
    let f = |s| stages::filter_image(s, &input.samples, input.row_bytes(), input.layout.bpp());
    [
        f(FilterStrategy::None),
        f(FilterStrategy::Fixed(FilterType::Sub)),
        f(FilterStrategy::Fixed(FilterType::Up)),
        f(FilterStrategy::Fixed(FilterType::Average)),
        f(FilterStrategy::Fixed(FilterType::Paeth)),
        f(FilterStrategy::MinSumAbs),
        f(FilterStrategy::MinBigrams),
    ]
}

/// Two-tier brute force: every candidate at `tier`, the `keep` smallest re-encoded at `Best`.
fn two_tier(input: &Input, tier: Level, keep: usize) -> (Vec<u8>, u128) {
    let start = Instant::now();
    let candidates = brute_candidates(input);
    let mut ranked: Vec<(usize, usize)> = candidates
        .iter()
        .enumerate()
        .map(|(i, c)| (zlib(tier, c).len(), i))
        .collect();
    ranked.sort_unstable();
    let mut best: Option<Vec<u8>> = None;
    // In preference order among the survivors, so an equal `Best` size keeps the earlier one.
    let mut survivors: Vec<usize> = ranked.iter().take(keep).map(|&(_, i)| i).collect();
    survivors.sort_unstable();
    for i in survivors {
        let stream = zlib(Level::Best, &candidates[i]);
        if best.as_ref().is_none_or(|b| stream.len() < b.len()) {
            best = Some(stream);
        }
    }
    (best.unwrap_or_default(), start.elapsed().as_nanos())
}

fn main() {
    let mut samples = 3usize;
    let mut corpus_dir = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let value = args.next().unwrap_or_else(|| panic!("{arg} needs a value"));
        match arg.as_str() {
            "--samples" => samples = value.parse().expect("--samples takes a count"),
            "--corpus-dir" => corpus_dir = Some(PathBuf::from(value)),
            other => panic!("unknown argument {other}"),
        }
    }

    let mut csv =
        String::from("input,layout,width,height,variant,sample,idat_bytes,crc32,time_ns,timing\n");
    for input in inputs() {
        if let Some(dir) = &corpus_dir {
            std::fs::write(dir.join(format!("{}.raw", input.name)), &input.samples)
                .expect("write corpus file");
        }
        let (rb, bpp) = (input.row_bytes(), input.layout.bpp());
        let fixed = fixed_images(&input);
        assert_eq!(
            choose_sum_abs(&fixed, rb),
            stages::filter_image(FilterStrategy::MinSumAbs, &input.samples, rb, bpp),
            "{}: the row splice does not reproduce the shipped MinSumAbs",
            input.name
        );
        let shipped = |s| stages::filter_image(s, &input.samples, rb, bpp);

        // One untimed warm-up pass, so sample 0 does not pay for cold caches.
        let _ = brute7(&input);
        for sample in 0..samples {
            let mut rows = Vec::new();
            // The seven brute-force candidates, in preference order, with their DEFLATE time.
            let mut brute = Vec::new();
            for (i, &(f, name)) in FILTERS.iter().enumerate() {
                let strategy = if i == 0 {
                    FilterStrategy::None
                } else {
                    FilterStrategy::Fixed(f)
                };
                let (m, _, deflate_ns) = candidate(name, || shipped(strategy));
                brute.push((m.idat_bytes, m.time_ns, deflate_ns));
                rows.push(m);
            }
            for (name, strategy) in [
                ("minsumabs", FilterStrategy::MinSumAbs),
                ("bigrams", FilterStrategy::MinBigrams),
            ] {
                let (m, _, deflate_ns) = candidate(name, || shipped(strategy));
                brute.push((m.idat_bytes, m.time_ns, deflate_ns));
                rows.push(m);
            }
            rows.push(candidate("entropy", || shipped(FilterStrategy::MinEntropy)).0);
            rows.push(candidate("bigent", || choose_bigent(&fixed_images(&input), rb)).0);
            rows.push(candidate("trial4", || choose_trial(&fixed_images(&input), rb)).0);

            let (brute_bytes, brute_ns) = brute7(&input);
            let smallest = brute.iter().map(|b| b.0).min().unwrap_or_default();
            assert_eq!(
                smallest, brute_bytes,
                "{}: the seven reconstructed candidates do not reproduce BruteForce",
                input.name
            );
            rows.push(Measured {
                variant: "brute7".to_owned(),
                idat_bytes: brute_bytes,
                crc32: String::new(),
                time_ns: brute_ns,
                timing: "measured",
            });

            for (tier, tier_name) in [(Level::Fast, "fast"), (Level::Default, "default")] {
                for keep in [1, 2] {
                    let (stream, ns) = two_tier(&input, tier, keep);
                    rows.push(measured(
                        &format!("twotier-{tier_name}-top{keep}"),
                        &stream,
                        ns,
                    ));
                }
            }

            // AtomicMin in preference order: a trial that ends at `S >= B` could have bailed once
            // its output passed `B` (an equal size loses the tie to the earlier candidate anyway).
            let mut best_so_far = usize::MAX;
            let mut total = 0u128;
            for &(bytes, ns, deflate_ns) in &brute {
                total += ns;
                if bytes >= best_so_far {
                    let saved = deflate_ns as f64 * (1.0 - best_so_far as f64 / bytes as f64);
                    total -= saved as u128;
                }
                best_so_far = best_so_far.min(bytes);
            }
            rows.push(Measured {
                variant: "atomicmin-model".to_owned(),
                idat_bytes: brute_bytes,
                crc32: String::new(),
                time_ns: total,
                timing: "model",
            });

            for m in rows {
                writeln!(
                    csv,
                    "{},{},{},{},{},{},{},{},{},{}",
                    input.name,
                    input.layout.name(),
                    input.side,
                    input.side,
                    m.variant,
                    sample,
                    m.idat_bytes,
                    m.crc32,
                    m.time_ns,
                    m.timing,
                )
                .expect("write to String");
            }
        }
    }
    print!("{csv}");
}
