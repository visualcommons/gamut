//! PNG encode size and throughput benchmarks (issues #224, #149).
//!
//! For a space-efficient encoder two things matter, and they trade against each other: the size it
//! achieves and the time it costs. So `cargo bench -p gamut-png` first prints two tables -- output
//! size and bits-per-pixel against libpng at maximum compression, then where the bytes went stage
//! by stage -- and only then runs the divan throughput benchmarks.
//!
//! Both tables are computed through [`gamut_png::deconstruct`], which reads any PNG whoever wrote
//! it. That is what makes the libpng column a like-for-like comparison rather than two encoders'
//! self-reports, and it is why the stage table can attribute a size difference to filtering, to
//! the colour-type choice, or to DEFLATE.
//!
//! Counters report bytes of *source* pixels per second, so figures are comparable with the other
//! codec suites. Run with `cargo bench -p gamut-png` (or `mise run bench`). The per-stage rows
//! need this crate's `test-support` feature, which its own dev-dependency on itself already
//! enables for every test and bench build -- there is no flag to pass.
//!
//! Intentionally tight: this measures **encoding**, on a generated 8-bit corpus, and nothing else.
//! There is no decode axis -- `PngDecoder`'s cost is a separate question against a separate
//! oracle, and folding the two into one aggregate would let a decode win mask an encode
//! regression. There is no ancillary-chunk axis: a metadata chunk costs its own payload plus
//! twelve bytes of framing, and the one piece of real work in the compressed ones (`iCCP`,
//! `zTXt`) is a `gamut-deflate` call that `gamut-deflate`'s own suite already measures -- none of
//! it is decided by the encoder's pixel path. There is no interlace axis because there is nothing
//! to measure: `ihdr::write` always emits interlace method 0, and Adam7 is a decode-side feature
//! here. What is left -- compression level, filter strategy, auto-reduce, and the corpus itself --
//! are the four axes the encoder actually chooses between.

use divan::counter::BytesCount;
use divan::{Bencher, black_box};
use gamut_core::{Dimensions, EncodeImage, ImageRef, Rgb8, Rgba8};
use gamut_png::{FilterStrategy, FilterType, Level, PngEncoder, deconstruct};

// The corpus lives with the size contract that asserts against it, so the budgets in
// `tests/size_contract.rs` and the table printed here can never describe different pixels.
#[path = "../tests/common/corpus.rs"]
mod corpus;

use corpus::{
    flat_rgba, gradient_rgb, grey_as_rgb, noise_rgb, palette64_rgba, photo_rgb, sprite_rgba,
};

fn main() {
    print_size_table();
    print_stage_table();
    print_heuristic_table();
    divan::main();
}

/// Side length of the square test images.
///
/// 256 is the floor that means anything here: RGB at 256x256 is 192 KiB, roughly six times the
/// 32 KiB DEFLATE window, so LZ77 match behaviour is real. A 64x64 image fits *inside* the window
/// and would flatter both encoders equally, hiding the thing being measured.
const SIDE: u32 = 256;

/// What a corpus entry is: named pixels in one of the two layouts the tables exercise.
enum Pixels {
    /// 8-bit RGB, `SIDE x SIDE`.
    Rgb(Vec<u8>),
    /// 8-bit RGBA, `SIDE x SIDE`.
    Rgba(Vec<u8>),
}

/// One named corpus entry.
struct Case {
    /// Short name, used as the table's row label and the divan argument.
    name: &'static str,
    /// Image width in pixels.
    width: u32,
    /// Image height in pixels.
    height: u32,
    /// The samples.
    pixels: Pixels,
}

impl Case {
    /// Raw sample bytes -- the denominator every ratio in the tables is read against.
    fn raw_len(&self) -> usize {
        match &self.pixels {
            Pixels::Rgb(v) | Pixels::Rgba(v) => v.len(),
        }
    }

    /// libpng's colour-type code for this entry's layout.
    fn libpng_color_type(&self) -> u8 {
        match self.pixels {
            Pixels::Rgb(_) => libpng_oracle::COLOR_RGB,
            Pixels::Rgba(_) => libpng_oracle::COLOR_RGBA,
        }
    }

    /// Encodes with gamut at the given knobs.
    fn gamut(&self, level: Level, filter: FilterStrategy, auto_reduce: bool) -> Vec<u8> {
        self.gamut_with(level, filter, auto_reduce, false)
    }

    /// As [`Self::gamut`], with the opt-in transparent-colour cleanup as well.
    fn gamut_with(
        &self,
        level: Level,
        filter: FilterStrategy,
        auto_reduce: bool,
        cleanup: bool,
    ) -> Vec<u8> {
        let encoder = PngEncoder::new()
            .with_compression(level)
            .with_filter(filter)
            .with_auto_reduce(auto_reduce)
            .with_transparent_cleanup(cleanup);
        let dims = Dimensions::new(self.width, self.height).expect("corpus dimensions are valid");
        let mut out = Vec::new();
        match &self.pixels {
            Pixels::Rgb(v) => {
                let image = ImageRef::<Rgb8>::new(v, dims).expect("buffer matches dimensions");
                encoder.encode_image(image, &mut out).expect("encode");
            }
            Pixels::Rgba(v) => {
                let image = ImageRef::<Rgba8>::new(v, dims).expect("buffer matches dimensions");
                encoder.encode_image(image, &mut out).expect("encode");
            }
        }
        out
    }

    /// Encodes the *same source layout* with libpng at zlib level 9.
    ///
    /// Deliberately no `palette` option even for palettisable entries: handing libpng a palette
    /// would hand it gamut's own reduction, and the comparison would stop measuring anything.
    /// libpng's default adaptive filtering is left alone -- that is the honest baseline.
    fn libpng9(&self) -> Vec<u8> {
        let samples = match &self.pixels {
            Pixels::Rgb(v) | Pixels::Rgba(v) => v.as_slice(),
        };
        libpng_oracle::encode(
            samples,
            self.width,
            self.height,
            self.libpng_color_type(),
            8,
            &libpng_oracle::EncodeOpts {
                compression_level: Some(9),
                ..libpng_oracle::EncodeOpts::default()
            },
        )
    }
}

/// The size-table corpus: one entry per axis that actually changes encoder behaviour.
fn corpus() -> Vec<Case> {
    let rgb = |name, pixels| Case {
        name,
        width: SIDE,
        height: SIDE,
        pixels: Pixels::Rgb(pixels),
    };
    let rgba = |name, pixels| Case {
        name,
        width: SIDE,
        height: SIDE,
        pixels: Pixels::Rgba(pixels),
    };
    vec![
        rgb("gradient_rgb8", gradient_rgb(SIDE)),
        rgb("photo_rgb8", photo_rgb(SIDE)),
        rgb("noise_rgb8", noise_rgb(SIDE)),
        rgb("grey_as_rgb8", grey_as_rgb(SIDE)),
        rgba("palette64_rgba8", palette64_rgba(SIDE)),
        rgba("sprite_rgba8", sprite_rgba(SIDE)),
        rgba("flat_rgba8", flat_rgba(SIDE)),
        // The regime where the signature and five chunks of framing dominate bits-per-pixel, and
        // the only row where `overhead_bytes` is legible.
        Case {
            name: "tiny_rgb8",
            width: 16,
            height: 16,
            pixels: Pixels::Rgb(gradient_rgb(16)),
        },
    ]
}

/// The knobs the size table reports gamut under: its default, and its smallest-output setting.
const BEST: (Level, FilterStrategy, bool) = (Level::Best, FilterStrategy::BruteForce, true);

/// Prints output size and bits-per-pixel against libpng at zlib level 9.
fn print_size_table() {
    println!(
        "\ngamut-png output size, bytes (lower is better); bpp is the whole file over the pixel count:\n\n\
         {:<17} {:>9} {:>9} {:>9} {:>9} {:>9}  {:>9} {:>7}",
        "input", "raw", "default", "best", "+clean", "libpng-9", "best/lp9", "bpp"
    );
    for case in corpus() {
        let default = case.gamut(Level::Default, FilterStrategy::MinSumAbs, false);
        let best = case.gamut(BEST.0, BEST.1, BEST.2);
        // The opt-in cleanup is off in every other column; this one shows what it is worth.
        let cleaned = case.gamut_with(BEST.0, BEST.1, BEST.2, true);
        let libpng = case.libpng9();
        let delta = (best.len() as f64 / libpng.len().max(1) as f64 - 1.0) * 100.0;
        let bpp = |bytes: &[u8]| bytes.len() as f64 * 8.0 / f64::from(case.width * case.height);
        println!(
            "{:<17} {:>9} {:>9} {:>9} {:>9} {:>9}  {:>8.1}% {:>7.3}",
            case.name,
            case.raw_len(),
            default.len(),
            best.len(),
            cleaned.len(),
            libpng.len(),
            delta,
            bpp(&best),
        );
    }
}

/// Prints where gamut's bytes went, stage by stage -- every column read back out of the encoded
/// file through [`deconstruct`], so the table describes the artefact rather than the encoder's
/// own bookkeeping.
fn print_stage_table() {
    println!(
        "\nwhere the bytes went (gamut at Level::Best + BruteForce + auto-reduce):\n\n\
         {:<17} {:>14} {:>5} {:>10} {:>10} {:>7} {:>9}  filters N/S/U/A/P",
        "input", "type", "depth", "filtered", "idat", "deflate", "overhead"
    );
    for case in corpus() {
        let png = case.gamut(BEST.0, BEST.1, BEST.2);
        let report = deconstruct(&png).expect("gamut's own output deconstructs");
        let filters = report.filters.histogram().map_or_else(
            || "-".to_string(),
            |h| {
                let n = |f| h.count(f);
                format!(
                    "{}/{}/{}/{}/{}",
                    n(FilterType::None),
                    n(FilterType::Sub),
                    n(FilterType::Up),
                    n(FilterType::Average),
                    n(FilterType::Paeth)
                )
            },
        );
        println!(
            "{:<17} {:>14} {:>5} {:>10} {:>10} {:>6.1}% {:>9}  {}",
            case.name,
            format!("{:?}", report.header.color_type),
            report.header.bit_depth,
            report.filtered_len,
            report.idat_compressed,
            report.idat_ratio() * 100.0,
            report.overhead_bytes(),
            filters,
        );
    }
}

/// Prints what each per-scanline filter heuristic is worth on its own.
///
/// `BruteForce` tries them all and keeps the smallest, so the aggregate table above cannot say
/// *which* one earned the win — and that is exactly the question issue #480 asks. oxipng's
/// evidence for demoting libpng's MinSum out of its default preset is a preset table, not
/// published byte counts, so gamut has to measure it on its own corpus.
fn print_heuristic_table() {
    println!(
        "\nper-scanline filter heuristic, IDAT bytes at Level::Best (lower is better):\n\n\
         {:<17} {:>10} {:>10} {:>10}  winner",
        "input", "MinSumAbs", "Entropy", "Bigrams"
    );
    for case in corpus() {
        let of = |filter| {
            let png = case.gamut(Level::Best, filter, false);
            deconstruct(&png)
                .expect("gamut's own output deconstructs")
                .idat_compressed
        };
        let (msa, ent, big) = (
            of(FilterStrategy::MinSumAbs),
            of(FilterStrategy::MinEntropy),
            of(FilterStrategy::MinBigrams),
        );
        let best = msa.min(ent).min(big);
        // A three-way tie is a real outcome on incompressible input, and naming the first
        // heuristic the winner there would record a preference the measurement did not find.
        let winner = if msa == ent && ent == big {
            "tie"
        } else if best == msa {
            "MinSumAbs"
        } else if best == ent {
            "Entropy"
        } else {
            "Bigrams"
        };
        println!(
            "{:<17} {:>10} {:>10} {:>10}  {winner}",
            case.name, msa, ent, big
        );
    }
}

fn case_named(name: &str) -> Case {
    corpus()
        .into_iter()
        .find(|c| c.name == name)
        .unwrap_or_else(|| panic!("unknown corpus entry {name}"))
}

#[divan::bench(args = [Level::Fast, Level::Default, Level::Best])]
fn encode_level(bencher: Bencher, level: Level) {
    let case = case_named("gradient_rgb8");
    bencher
        .counter(BytesCount::new(case.raw_len()))
        .bench_local(|| case.gamut(black_box(level), FilterStrategy::MinSumAbs, false));
}

#[divan::bench(args = [
    FilterStrategy::None,
    FilterStrategy::Fixed(FilterType::Paeth),
    FilterStrategy::MinSumAbs,
    FilterStrategy::BruteForce,
])]
fn encode_filter_strategy(bencher: Bencher, filter: FilterStrategy) {
    let case = case_named("gradient_rgb8");
    bencher
        .counter(BytesCount::new(case.raw_len()))
        .bench_local(|| case.gamut(Level::Default, black_box(filter), false));
}

/// Attributes the whole reduce stage without needing any seam into it: the same image encoded
/// with the analysis on and off.
#[divan::bench(args = [false, true])]
fn encode_auto_reduce(bencher: Bencher, auto_reduce: bool) {
    let case = case_named("palette64_rgba8");
    bencher
        .counter(BytesCount::new(case.raw_len()))
        .bench_local(|| {
            case.gamut(
                Level::Default,
                FilterStrategy::MinSumAbs,
                black_box(auto_reduce),
            )
        });
}

#[divan::bench(args = ["gradient_rgb8", "photo_rgb8", "noise_rgb8", "palette64_rgba8"])]
fn encode_corpus(bencher: Bencher, name: &str) {
    let case = case_named(name);
    bencher
        .counter(BytesCount::new(case.raw_len()))
        .bench_local(|| case.gamut(Level::Default, FilterStrategy::MinSumAbs, true));
}

/// Reading the accounting back out of a finished file -- the cost every table row pays.
#[divan::bench]
fn deconstruct_a_finished_png(bencher: Bencher) {
    let case = case_named("gradient_rgb8");
    let png = case.gamut(Level::Default, FilterStrategy::MinSumAbs, false);
    bencher
        .counter(BytesCount::new(png.len()))
        .bench_local(|| deconstruct(black_box(&png)).expect("deconstruct"));
}

/// Per-stage rows. Behind `test-support` because a `benches/` target is a separate crate and the
/// encoder's stages are crate-private; see `gamut_png::stages`.
#[cfg(feature = "test-support")]
mod stages {
    use gamut_png::stages;

    use super::{Bencher, BytesCount, Case, Pixels, SIDE, black_box, case_named, noise_rgb};

    /// The filtered stride and row length an RGB8 image of `SIDE` presents.
    const BPP: usize = 3;
    const ROW_BYTES: usize = SIDE as usize * BPP;

    fn rgb_samples(case: &Case) -> &[u8] {
        match &case.pixels {
            Pixels::Rgb(v) | Pixels::Rgba(v) => v,
        }
    }

    #[divan::bench(args = [
        gamut_png::FilterStrategy::None,
        gamut_png::FilterStrategy::Fixed(gamut_png::FilterType::Paeth),
        gamut_png::FilterStrategy::MinSumAbs,
    ])]
    fn filter_image(bencher: Bencher, strategy: gamut_png::FilterStrategy) {
        let case = case_named("gradient_rgb8");
        let samples = rgb_samples(&case).to_vec();
        bencher
            .counter(BytesCount::new(samples.len()))
            .bench_local(|| stages::filter_image(black_box(strategy), &samples, ROW_BYTES, BPP));
    }

    #[divan::bench(args = [1u8, 2, 4])]
    fn pack_scanlines(bencher: Bencher, depth: u8) {
        let samples = vec![1u8; (SIDE * SIDE) as usize];
        bencher
            .counter(BytesCount::new(samples.len()))
            .bench_local(|| {
                stages::pack_scanlines(&samples, SIDE as usize, SIDE as usize, black_box(depth))
            });
    }

    /// Both sides of the auto-reduce early exit: a palettisable image, and one with far more than
    /// 256 colours where the scan bails.
    #[divan::bench(args = ["palettisable", "too_many_colors"])]
    fn analyze8(bencher: Bencher, kind: &str) {
        let case = case_named(if kind == "palettisable" {
            "palette64_rgba8"
        } else {
            "photo_rgb8"
        });
        let channels = match case.pixels {
            Pixels::Rgb(_) => 3,
            Pixels::Rgba(_) => 4,
        };
        let samples = rgb_samples(&case).to_vec();
        bencher
            .counter(BytesCount::new(samples.len()))
            .bench_local(|| stages::analyze8(&samples, black_box(channels)));
    }

    /// 16-bit analysis, with and without a lawful demotion available: every sample `k * 257`
    /// demotes, an arbitrary one does not, and the two take different paths.
    #[divan::bench(args = [true, false])]
    fn analyze16(bencher: Bencher, demotable: bool) {
        let n = (SIDE * SIDE) as usize;
        let samples: Vec<u16> = (0..n)
            .map(|i| {
                let v = (i % 256) as u16;
                if demotable { v * 257 } else { v * 257 + 1 }
            })
            .collect();
        bencher
            .counter(BytesCount::new(samples.len() * 2))
            .bench_local(|| stages::analyze16(&samples, black_box(1)));
    }

    /// Runs over every IDAT byte, so it is on the critical path of every encode.
    #[divan::bench]
    fn crc32(bencher: Bencher) {
        let data = noise_rgb(SIDE);
        bencher
            .counter(BytesCount::new(data.len()))
            .bench_local(|| {
                let mut crc = stages::Crc32::new();
                crc.update(black_box(&data));
                crc.finish()
            });
    }
}
