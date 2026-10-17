//! Measurement arm for experiment #612: does modified-Zeng ordering of the encoder-derived palette
//! (`reduce::ordered_palette`, today transparent entries first and then Rec. 601 luma) buy bytes,
//! and on which inputs?
//!
//! Not a test and not a benchmark: it asserts nothing. It prints one CSV row per input x sample
//! for whichever build of the crate it was compiled against. `experiments/612/run.sh`
//! (`mise run experiment 612`) compiles it three times -- once at `HEAD` (`baseline`), and once
//! with each of `experiments/612/mzeng.patch` and `experiments/612/mzeng-trns.patch` applied --
//! labelling each run with `--variant`. The verdict and the table live in
//! `experiments/612/RESULTS.md`.
//!
//! Two groups of inputs:
//!
//! - **`size_contract`** -- every row of `tests/size_contract.rs`'s budget table, at the same side
//!   and settings, from the same shared corpus: the gated rows, so a regression there is visible.
//!   Rows that never reach the indexed path are controls and must not move.
//! - **`reach`** -- palette-reaching inputs the corpus lacks: posterised smooth content (where
//!   index adjacency follows colour adjacency, Zeng's best case), grey ramps, and a sprite whose
//!   visible part is posterised photo content over a translucent ring. Each at two sides.
//!
//! Columns: `group,input,layout,width,height,cleanup,variant,sample,bytes,crc32,encode_ns,
//! colour_type,indexed_bytes`. `bytes` and `crc32` identify the finished file; `encode_ns` is one
//! wall-clock `encode_image` call; `colour_type` is the file's IHDR colour type (3 = indexed).
//!
//! `indexed_bytes` is the ordering's own signal, which the finished file hides whenever the indexed
//! candidate loses `write_reduced_or_native`'s race: the size of the indexed candidate alone, at
//! the same settings, whatever wins. It takes the palette and indices `analyze8`/`analyze16` built
//! -- in whichever order this build's `ordered_palette` gave them -- and encodes them through
//! `encode_indexed8`, the caller-palette path, whose cleaning drops and merges nothing here (every
//! entry is named and distinct) and keeps the order. It is empty where the analysis offers no
//! palette, and on the `+clean` rows, whose analysis runs on cleaned samples this arm cannot see.
//!
//! `first_seen_bytes` is the same candidate with its palette renumbered by first appearance in
//! raster order, which is what a caller-supplied palette built by scanning the image looks like and
//! what `encode_indexed8` keeps today. It is identical across builds, and it is the reference for
//! the caller-palette half of the issue: what reordering a caller's palette could buy over leaving
//! it alone.

use std::fmt::Write as _;
use std::path::PathBuf;
use std::time::Instant;

use gamut_core::{Dimensions, EncodeImage, Gray8, ImageRef, Indexed8, Pixel, Rgb8, Rgb16, Rgba8};
use gamut_png::stages::{self, Crc32, Reduced, Reductions};
use gamut_png::{FilterStrategy, Level, PngEncoder, PngPalette};

#[path = "../tests/common/corpus.rs"]
mod corpus;

/// Interleaved sample layout of an input.
#[derive(Clone, Copy)]
enum Layout {
    Gray8,
    Rgb8,
    Rgba8,
    /// 16-bit RGB, stored big-endian as the corpus generates it.
    Rgb16,
}

impl Layout {
    fn name(self) -> &'static str {
        match self {
            Self::Gray8 => "gray8",
            Self::Rgb8 => "rgb8",
            Self::Rgba8 => "rgba8",
            Self::Rgb16 => "rgb16",
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

/// `photo_rgb` posterised to 3-3-2 bits: at most 256 colours, so the palette applies, and
/// neighbouring pixels carry neighbouring colours -- the co-occurrence structure Zeng exploits.
fn photo332_rgb(side: u32) -> Vec<u8> {
    corpus::photo_rgb(side)
        .as_chunks::<3>()
        .0
        .iter()
        .flat_map(|px| [px[0] & 0xE0, px[1] & 0xE0, px[2] & 0xC0])
        .collect()
}

/// `photo_rgb` posterised to 2 bits per channel: at most 64 colours, a 6-bit (so 8-bit) index.
fn photo222_rgb(side: u32) -> Vec<u8> {
    corpus::photo_rgb(side)
        .as_chunks::<3>()
        .0
        .iter()
        .flat_map(|px| [px[0] & 0xC0, px[1] & 0xC0, px[2] & 0xC0])
        .collect()
}

/// `gradient_rgb` posterised to 3-3-2 bits: XOR-and-shear structure rather than smooth shading,
/// so luma order and adjacency order disagree.
fn gradient332_rgb(side: u32) -> Vec<u8> {
    corpus::gradient_rgb(side)
        .as_chunks::<3>()
        .0
        .iter()
        .flat_map(|px| [px[0] & 0xE0, px[1] & 0xE0, px[2] & 0xC0])
        .collect()
}

/// The photo fixture's red channel quantised to 16 grey levels, none a multiple of 17, so no
/// sub-byte grey depth applies and a 4-bit palette wins the estimate. Luma order is already the
/// ramp order here, so this is luma's best case.
fn levels16_gray8(side: u32) -> Vec<u8> {
    corpus::photo_rgb(side)
        .as_chunks::<3>()
        .0
        .iter()
        .map(|px| (px[0] >> 4) * 16 + 3)
        .collect()
}

/// [`photo332_rgb`] inside a disc, a translucent (alpha 128) ring around it, and fully
/// transparent black outside: the indexed path with a `tRNS` table, where pure Zeng order and the
/// transparent-first rule pull in different directions.
fn photo_sprite_rgba(side: u32) -> Vec<u8> {
    let photo = photo332_rgb(side);
    let r2 = (i64::from(side) * i64::from(side)) / 9;
    let ring2 = (i64::from(side) * i64::from(side)) / 6;
    let mut out = Vec::with_capacity((side * side * 4) as usize);
    for (i, px) in photo.as_chunks::<3>().0.iter().enumerate() {
        let (x, y) = (i as u32 % side, i as u32 / side);
        let cx = i64::from(x) - i64::from(side) / 2;
        let cy = i64::from(y) - i64::from(side) / 2;
        let d2 = cx * cx + cy * cy;
        if d2 < r2 {
            out.extend_from_slice(&[px[0], px[1], px[2], 255]);
        } else if d2 < ring2 {
            out.extend_from_slice(&[px[0] & 0x80, px[1] & 0x80, px[2] & 0x80, 128]);
        } else {
            out.extend_from_slice(&[0, 0, 0, 0]);
        }
    }
    out
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
    let rows: [(&str, Layout, u32, bool, Vec<u8>); 12] = [
        (
            "gradient_rgb8",
            Layout::Rgb8,
            128,
            false,
            corpus::gradient_rgb(128),
        ),
        (
            "photo_rgb8",
            Layout::Rgb8,
            128,
            false,
            corpus::photo_rgb(128),
        ),
        (
            "noise_rgb8",
            Layout::Rgb8,
            128,
            false,
            corpus::noise_rgb(128),
        ),
        (
            "grey_as_rgb8",
            Layout::Rgb8,
            128,
            false,
            corpus::grey_as_rgb(128),
        ),
        (
            "flat_rgba8",
            Layout::Rgba8,
            128,
            false,
            corpus::flat_rgba(128),
        ),
        (
            "sprite_rgba8",
            Layout::Rgba8,
            128,
            false,
            corpus::sprite_rgba(128),
        ),
        (
            "sprite_rgba8 +clean",
            Layout::Rgba8,
            128,
            true,
            corpus::sprite_rgba(128),
        ),
        (
            "palette64_rgba8",
            Layout::Rgba8,
            128,
            false,
            corpus::palette64_rgba(128),
        ),
        (
            "palette64_rgba8 +clean",
            Layout::Rgba8,
            128,
            true,
            corpus::palette64_rgba(128),
        ),
        (
            "opaque256_rgba8",
            Layout::Rgba8,
            128,
            false,
            corpus::opaque256_rgba(128),
        ),
        (
            "demotable_rgb16",
            Layout::Rgb16,
            64,
            false,
            corpus::demotable_rgb16(64),
        ),
        (
            "tiny_rgb8",
            Layout::Rgb8,
            16,
            false,
            corpus::gradient_rgb(16),
        ),
    ];
    for (name, layout, side, cleanup, samples) in rows {
        push(sc, name, layout, side, cleanup, samples);
    }
    for side in [128, 256] {
        push(
            "reach",
            &format!("photo332_rgb8@{side}"),
            Layout::Rgb8,
            side,
            false,
            photo332_rgb(side),
        );
        push(
            "reach",
            &format!("photo222_rgb8@{side}"),
            Layout::Rgb8,
            side,
            false,
            photo222_rgb(side),
        );
        push(
            "reach",
            &format!("gradient332_rgb8@{side}"),
            Layout::Rgb8,
            side,
            false,
            gradient332_rgb(side),
        );
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
            &format!("photo_sprite_rgba8@{side}"),
            Layout::Rgba8,
            side,
            false,
            photo_sprite_rgba(side),
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

/// The corpus's big-endian 16-bit bytes as samples.
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
    // The 16-bit widening happens before the clock starts, so every row times the encode alone.
    let wide = match input.layout {
        Layout::Rgb16 => widen16(s),
        _ => Vec::new(),
    };
    let start = Instant::now();
    let out = match input.layout {
        Layout::Gray8 => encode_as::<Gray8>(&enc, s, dims),
        Layout::Rgb8 => encode_as::<Rgb8>(&enc, s, dims),
        Layout::Rgba8 => encode_as::<Rgba8>(&enc, s, dims),
        Layout::Rgb16 => encode_as::<Rgb16>(&enc, &wide, dims),
    };
    (out, start.elapsed().as_nanos())
}

/// The size of the indexed candidate alone in this build's order and in first-seen order, or
/// `None` where the analysis offers no palette (see the module doc's `indexed_bytes`).
fn indexed_candidate(input: &Input) -> Option<(usize, usize)> {
    let reductions = match input.layout {
        Layout::Rgb16 => stages::analyze16(&widen16(&input.samples), 3),
        Layout::Gray8 => stages::analyze8(&input.samples, 1),
        Layout::Rgb8 => stages::analyze8(&input.samples, 3),
        Layout::Rgba8 => stages::analyze8(&input.samples, 4),
    };
    let Reductions::Chunked {
        chunked:
            Reduced::Indexed {
                indices,
                plte,
                trns,
                ..
            },
        ..
    } = reductions
    else {
        return None;
    };
    let rgb: Vec<[u8; 3]> = plte.as_chunks::<3>().0.to_vec();
    let alpha = trns.unwrap_or_default();
    let ordered = indexed_size(input.side, &indices, &rgb, &alpha);

    // The same palette renumbered by first appearance in raster order: the order a caller that
    // builds its palette while scanning would hand `encode_indexed8`, and the order the
    // caller-palette path keeps today.
    let mut renumber = [None::<u8>; 256];
    let mut first_rgb = Vec::with_capacity(rgb.len());
    let mut first_alpha = Vec::with_capacity(rgb.len());
    let first_indices: Vec<u8> = indices
        .iter()
        .map(|&i| {
            *renumber[usize::from(i)].get_or_insert_with(|| {
                first_rgb.push(rgb[usize::from(i)]);
                first_alpha.push(alpha.get(usize::from(i)).copied().unwrap_or(255));
                (first_rgb.len() - 1) as u8
            })
        })
        .collect();
    let first_seen = indexed_size(input.side, &first_indices, &first_rgb, &first_alpha);
    Some((ordered, first_seen))
}

/// One `encode_indexed8` at the experiment's settings, returning the file's length.
fn indexed_size(side: u32, indices: &[u8], rgb: &[[u8; 3]], alpha: &[u8]) -> usize {
    let palette = PngPalette::with_transparency(rgb, alpha).expect("analysis palette is valid");
    let dims = Dimensions::new(side, side).expect("valid dimensions");
    let image = ImageRef::<Indexed8>::new(indices, dims).expect("one index per pixel");
    let mut out = Vec::new();
    encoder(false)
        .encode_indexed8(image, &palette, &mut out)
        .expect("indexed encode");
    out.len()
}

fn main() {
    let mut variant = None;
    let mut samples = 3usize;
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
        "group,input,layout,width,height,cleanup,variant,sample,bytes,crc32,encode_ns,colour_type,indexed_bytes,first_seen_bytes\n",
    );
    for input in inputs() {
        if let Some(dir) = &corpus_dir {
            let file = dir.join(format!("{}.raw", input.name.replace([' ', '+', '@'], "_")));
            std::fs::write(&file, &input.samples).expect("write corpus file");
        }
        let (indexed, first_seen) = if input.cleanup {
            (String::new(), String::new())
        } else {
            indexed_candidate(&input).map_or_else(
                || (String::new(), String::new()),
                |(o, f)| (o.to_string(), f.to_string()),
            )
        };
        // One untimed warm-up encode, so sample 0 does not pay for cold caches.
        let _ = encode(&input);
        for sample in 0..samples {
            let (file, ns) = encode(&input);
            let mut crc = Crc32::new();
            crc.update(&file);
            // Signature (8) + IHDR length and type (8) + width, height, bit depth: byte 25.
            let colour_type = file[25];
            writeln!(
                csv,
                "{},{},{},{},{},{},{},{},{},{:08x},{},{},{},{}",
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
                colour_type,
                indexed,
                first_seen,
            )
            .expect("write to String");
        }
    }
    print!("{csv}");
}
