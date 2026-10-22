//! Measurement arm for experiment #514: does entering an equal-raw-size chunk-free candidate into
//! `write_reduced_or_native`'s race (`free_size <= input_size` in `reduce::analyze8_for`, instead of
//! the shipped strict `<`) ever buy a byte, and what does it cost?
//!
//! Not a test and not a benchmark: it asserts nothing. It prints one CSV row per
//! input x sample for whichever build of the crate it was compiled against, and
//! `experiments/514/run.sh` (`mise run experiment 514`) compiles it twice -- once at `HEAD`, once
//! with `experiments/514/variant.patch` applied -- labelling each run with `--variant`. The verdict
//! and the table live in `experiments/514/RESULTS.md`.
//!
//! Two groups of inputs:
//!
//! - **`size_contract`** -- every row of `tests/size_contract.rs`'s budget table, at the same side
//!   and settings, from the same shared corpus. The issue's acceptance measure: no row may
//!   regress, and the change is worth taking only if one improves.
//! - **`reach`** -- 8-bit grey and grey+alpha inputs on which a palette or a colour key wins the
//!   raw estimate. These are the only layouts where `free_size == input_size` can hold (see
//!   `RESULTS.md`), so they are the only rows on which the variant changes what is raced at all.
//!   No `size_contract` row is grey or grey+alpha, which is why this group exists.
//!
//! Columns: `group,input,layout,width,height,cleanup,variant,sample,bytes,crc32,encode_ns,
//! reductions,race_candidates`. `bytes` and `crc32` identify the finished file; `encode_ns` is one
//! wall-clock `encode_image` call; `reductions` is what `analyze8`/`analyze16` offered
//! (`none`, `chunk_free`, `chunked`, `chunked+free`), and `race_candidates` the number of whole
//! filter-plus-DEFLATE encodes `write_reduced_or_native` performs for it (1, 1, 2, 3). Both are
//! left empty on the `+clean` rows, whose analysis runs on cleaned samples this arm cannot see.

use std::fmt::Write as _;
use std::path::PathBuf;
use std::time::Instant;

use gamut_core::{Dimensions, EncodeImage, Gray8, GrayAlpha8, ImageRef, Pixel, Rgb8, Rgb16, Rgba8};
use gamut_png::stages::{self, Crc32, Reductions};
use gamut_png::{FilterStrategy, Level, PngEncoder};

#[path = "../tests/common/corpus.rs"]
mod corpus;

/// Interleaved sample layout of an input.
#[derive(Clone, Copy)]
enum Layout {
    Gray8,
    GrayAlpha8,
    Rgb8,
    Rgba8,
    /// 16-bit RGB, stored big-endian as the corpus generates it.
    Rgb16,
}

impl Layout {
    fn name(self) -> &'static str {
        match self {
            Self::Gray8 => "gray8",
            Self::GrayAlpha8 => "grayalpha8",
            Self::Rgb8 => "rgb8",
            Self::Rgba8 => "rgba8",
            Self::Rgb16 => "rgb16",
        }
    }

    fn channels(self) -> usize {
        match self {
            Self::Gray8 => 1,
            Self::GrayAlpha8 => 2,
            Self::Rgb8 | Self::Rgb16 => 3,
            Self::Rgba8 => 4,
        }
    }
}

/// One measured input.
struct Input {
    group: &'static str,
    name: String,
    layout: Layout,
    side: u32,
    cleanup: bool,
    samples: Vec<u8>,
}

/// The photo fixture's red channel quantised to 16 grey levels, none of them a multiple of 17, so
/// no sub-byte grey depth applies and the only reductions are a 4-bit palette (which wins the
/// estimate) and grey at depth 8 -- the input layout itself.
fn levels16_gray8(side: u32) -> Vec<u8> {
    corpus::photo_rgb(side)
        .as_chunks::<3>()
        .0
        .iter()
        .map(|px| (px[0] >> 4) * 16 + 3)
        .collect()
}

/// [`levels16_gray8`] over an 8x8 checkerboard of opaque and fully transparent cells: 32
/// grey+alpha pairs, so an 8-bit palette wins the estimate. Invisible pixels keep their grey, so
/// no colour key applies.
fn levels16_ga8(side: u32) -> Vec<u8> {
    let grey = levels16_gray8(side);
    let mut out = Vec::with_capacity(grey.len() * 2);
    for (i, g) in grey.into_iter().enumerate() {
        let (x, y) = (i as u32 % side, i as u32 / side);
        let alpha = if (x / 8 + y / 8).is_multiple_of(2) {
            255
        } else {
            0
        };
        out.extend_from_slice(&[g, alpha]);
    }
    out
}

/// A grey+alpha sprite with every invisible pixel at grey 0 and no visible pixel at grey 0, so a
/// `tRNS` colour key wins the estimate.
fn keyed_ga8(side: u32) -> Vec<u8> {
    corpus::sprite_rgba(side)
        .as_chunks::<4>()
        .0
        .iter()
        .flat_map(|px| if px[3] == 0 { [0, 0] } else { [px[0] | 1, 255] })
        .collect()
}

fn inputs() -> Vec<Input> {
    let mut v = Vec::new();
    let mut push = |group, name: &str, layout, side, cleanup, samples| {
        v.push(Input {
            group,
            name: name.to_owned(),
            layout,
            side,
            cleanup,
            samples,
        });
    };
    // `tests/size_contract.rs`'s BUDGETS, row for row.
    let sc = "size_contract";
    push(
        sc,
        "gradient_rgb8",
        Layout::Rgb8,
        128,
        false,
        corpus::gradient_rgb(128),
    );
    push(
        sc,
        "photo_rgb8",
        Layout::Rgb8,
        128,
        false,
        corpus::photo_rgb(128),
    );
    push(
        sc,
        "noise_rgb8",
        Layout::Rgb8,
        128,
        false,
        corpus::noise_rgb(128),
    );
    push(
        sc,
        "grey_as_rgb8",
        Layout::Rgb8,
        128,
        false,
        corpus::grey_as_rgb(128),
    );
    push(
        sc,
        "flat_rgba8",
        Layout::Rgba8,
        128,
        false,
        corpus::flat_rgba(128),
    );
    push(
        sc,
        "sprite_rgba8",
        Layout::Rgba8,
        128,
        false,
        corpus::sprite_rgba(128),
    );
    push(
        sc,
        "sprite_rgba8 +clean",
        Layout::Rgba8,
        128,
        true,
        corpus::sprite_rgba(128),
    );
    push(
        sc,
        "palette64_rgba8",
        Layout::Rgba8,
        128,
        false,
        corpus::palette64_rgba(128),
    );
    push(
        sc,
        "palette64_rgba8 +clean",
        Layout::Rgba8,
        128,
        true,
        corpus::palette64_rgba(128),
    );
    push(
        sc,
        "opaque256_rgba8",
        Layout::Rgba8,
        128,
        false,
        corpus::opaque256_rgba(128),
    );
    push(
        sc,
        "demotable_rgb16",
        Layout::Rgb16,
        64,
        false,
        corpus::demotable_rgb16(64),
    );
    push(
        sc,
        "tiny_rgb8",
        Layout::Rgb8,
        16,
        false,
        corpus::gradient_rgb(16),
    );
    // The layouts the variant can reach.
    for side in [128, 256] {
        push(
            "reach",
            &format!("levels16_gray8@{side}"),
            Layout::Gray8,
            side,
            false,
            levels16_gray8(side),
        );
        push(
            "reach",
            &format!("levels16_ga8@{side}"),
            Layout::GrayAlpha8,
            side,
            false,
            levels16_ga8(side),
        );
        push(
            "reach",
            &format!("keyed_ga8@{side}"),
            Layout::GrayAlpha8,
            side,
            false,
            keyed_ga8(side),
        );
    }
    v
}

/// `tests/size_contract.rs`'s `gamut_best` settings.
fn encoder(cleanup: bool) -> PngEncoder {
    PngEncoder::new()
        .with_compression(Level::Best)
        .with_filter(FilterStrategy::BruteForce)
        .with_auto_reduce(true)
        .with_transparent_cleanup(cleanup)
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

fn widen16(samples: &[u8]) -> Vec<u16> {
    samples
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&p| u16::from_be_bytes(p))
        .collect()
}

/// Encodes once, returning the file and the nanoseconds the `encode_image` call took.
fn encode(input: &Input) -> (Vec<u8>, u128) {
    let enc = encoder(input.cleanup);
    let dims = Dimensions::new(input.side, input.side).expect("valid dimensions");
    let s = &input.samples;
    let start = Instant::now();
    let out = match input.layout {
        Layout::Gray8 => encode_as::<Gray8>(&enc, s, dims),
        Layout::GrayAlpha8 => encode_as::<GrayAlpha8>(&enc, s, dims),
        Layout::Rgb8 => encode_as::<Rgb8>(&enc, s, dims),
        Layout::Rgba8 => encode_as::<Rgba8>(&enc, s, dims),
        Layout::Rgb16 => {
            let wide = widen16(s);
            let start = Instant::now();
            let out = encode_as::<Rgb16>(&enc, &wide, dims);
            return (out, start.elapsed().as_nanos());
        }
    };
    (out, start.elapsed().as_nanos())
}

/// What the analysis offered, and how many whole encodes the race therefore runs.
fn analysis(input: &Input) -> (&'static str, u8) {
    let reductions = match input.layout {
        Layout::Rgb16 => stages::analyze16(&widen16(&input.samples), 3),
        layout => stages::analyze8(&input.samples, layout.channels()),
    };
    match reductions {
        Reductions::None => ("none", 1),
        Reductions::ChunkFree(_) => ("chunk_free", 1),
        Reductions::Chunked {
            chunk_free: None, ..
        } => ("chunked", 2),
        Reductions::Chunked {
            chunk_free: Some(_),
            ..
        } => ("chunked+free", 3),
    }
}

fn main() {
    let mut variant = None;
    let mut samples = 5usize;
    let mut corpus_dir = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let value = args.next().unwrap_or_else(|| panic!("{arg} needs a value"));
        match arg.as_str() {
            "--variant" => variant = Some(value),
            "--samples" => samples = value.parse().expect("--samples takes a count"),
            "--corpus-dir" => corpus_dir = Some(PathBuf::from(value)),
            other => panic!("unknown argument {other}"),
        }
    }
    let variant = variant.expect("--variant is required");

    let mut csv = String::from(
        "group,input,layout,width,height,cleanup,variant,sample,bytes,crc32,encode_ns,reductions,race_candidates\n",
    );
    for input in inputs() {
        if let Some(dir) = &corpus_dir {
            let file = dir.join(format!("{}.raw", input.name.replace([' ', '+', '@'], "_")));
            std::fs::write(&file, &input.samples).expect("write corpus file");
        }
        let (reductions, candidates) = if input.cleanup {
            ("", String::new())
        } else {
            let (r, c) = analysis(&input);
            (r, c.to_string())
        };
        // One untimed warm-up encode, so sample 0 does not pay for cold caches.
        let _ = encode(&input);
        for sample in 0..samples {
            let (file, ns) = encode(&input);
            let mut crc = Crc32::new();
            crc.update(&file);
            writeln!(
                csv,
                "{},{},{},{},{},{},{},{},{},{:08x},{},{},{}",
                input.group,
                input.name,
                input.layout.name(),
                input.side,
                input.side,
                input.cleanup,
                variant,
                sample,
                file.len(),
                crc.finish(),
                ns,
                reductions,
                candidates,
            )
            .expect("write to String");
        }
    }
    print!("{csv}");
}
