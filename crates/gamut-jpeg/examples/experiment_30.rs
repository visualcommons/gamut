//! Measurement arm for experiment #30's progressive scan scripts: how many bytes each script spends
//! on the same quantized coefficients.
//!
//! Not a test and not a benchmark. Sandboxed experiment code under the #29 policy -- it must keep
//! compiling as the library moves, and it is validated only by the numbers it prints.
//! `experiments/30/run.sh` (`mise run experiment 30`) drives it; the verdict and the tables live in
//! `experiments/30/RESULTS.md`.
//!
//! The scan script is private to `src/progressive.rs`, so each alternative script is a patch of it
//! (`experiments/30/scans-*.patch`), and this arm is rebuilt once per patch. A progressive stream's
//! quantized coefficients equal the baseline stream's at the same configuration (the
//! `with_progressive` contract), so a script changes the rate and nothing else: the comparison is a
//! plain size ratio at each point, with no rate-distortion curve to fit. The arm checks that claim
//! at every point instead of trusting it: libjpeg-turbo (the crate's oracle) decodes both streams,
//! and the arm panics unless the two decodes are identical. That is the one assertion it makes.
//!
//! One subcommand:
//!
//! - `scans --image NAME=PATH ... [--synthetic WxH]` prints one CSV row per image x configuration x
//!   quality: `image,width,height,config,quality,baseline_bytes,progressive_bytes,scans`.
//!   `baseline_bytes` is the sequential stream with optimized Huffman tables
//!   (`with_optimized_tables(true)`), `progressive_bytes` the progressive stream
//!   (`with_progressive(true)`, whose tables are always optimized), and `scans` the progressive
//!   stream's SOS count. Configurations: `ycbcr420` and `ycbcr444` code the RGB input;
//!   `gray` codes its BT.601 luma, `(299 R + 587 G + 114 B + 500) / 1000`, as a `Gray8` image.
//!   Inputs are read as #29's arm reads them (`experiment_29.rs`: binary PPM or 24-bit BMP);
//!   `--synthetic` adds the decode bench's deterministic generator (`benches/common/corpus.rs`).

use std::path::Path;

use gamut_core::{Dimensions, EncodeImage, Gray8, ImageRef, Rgb8};
use gamut_jpeg::{ChromaSubsampling, JpegEncoder};

#[allow(
    dead_code,
    reason = "the arm reuses #29's input loader, quality sweep and corpus, not its measurement"
)]
#[path = "experiment_29.rs"]
mod exp29;

/// The decode bench's generator, through #29's arm: a second `#[path]` include of the same file
/// would compile it twice (`clippy::duplicate_mod`).
use exp29::corpus;

/// One coding configuration: the RGB input at a chroma subsampling, or its luma as grayscale.
#[derive(Clone, Copy)]
enum Config {
    Ycbcr(ChromaSubsampling),
    Gray,
}

const CONFIGS: [(&str, Config); 3] = [
    ("ycbcr420", Config::Ycbcr(ChromaSubsampling::Ycbcr420)),
    ("ycbcr444", Config::Ycbcr(ChromaSubsampling::Ycbcr444)),
    ("gray", Config::Gray),
];

/// BT.601 luma of interleaved RGB, rounded to 8 bits.
fn grey(rgb: &[u8]) -> Vec<u8> {
    rgb.as_chunks::<3>()
        .0
        .iter()
        .map(|p| {
            let y = 299 * u32::from(p[0]) + 587 * u32::from(p[1]) + 114 * u32::from(p[2]);
            ((y + 500) / 1000) as u8
        })
        .collect()
}

/// Encodes `rgb` under `config` at `quality`, progressive or sequential with optimized tables.
fn encode(config: Config, quality: u8, rgb: &[u8], w: u32, h: u32, progressive: bool) -> Vec<u8> {
    let dims = Dimensions::new(w, h).expect("dimensions");
    let enc = JpegEncoder::new()
        .with_quality(quality)
        .with_optimized_tables(true)
        .with_progressive(progressive);
    let mut out = Vec::new();
    match config {
        Config::Ycbcr(subsampling) => {
            let image = ImageRef::<Rgb8>::new(rgb, dims).expect("image");
            enc.with_subsampling(subsampling)
                .encode_image(image, &mut out)
                .expect("encode");
        }
        Config::Gray => {
            let samples = grey(rgb);
            let image = ImageRef::<Gray8>::new(&samples, dims).expect("image");
            enc.encode_image(image, &mut out).expect("encode");
        }
    }
    out
}

/// The number of SOS markers in `jpeg`. Entropy-coded data stuffs every `0xFF` with `0x00`
/// (§B.1.1.5), so `FF DA` occurs only as a marker.
fn scans(jpeg: &[u8]) -> usize {
    jpeg.windows(2).filter(|p| *p == [0xFF, 0xDA]).count()
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    assert!(
        args.first().is_some_and(|c| c == "scans"),
        "usage: experiment_30 scans --image NAME=PATH ... [--synthetic WxH]"
    );
    let mut images: Vec<(String, Vec<u8>, u32, u32)> = Vec::new();
    let mut it = args[1..].iter();
    while let Some(flag) = it.next() {
        let value = it.next().unwrap_or_else(|| panic!("{flag} needs a value"));
        match flag.as_str() {
            "--image" => {
                let (name, path) = value.split_once('=').expect("--image NAME=PATH");
                let (rgb, w, h) = exp29::load(Path::new(path));
                images.push((name.to_owned(), rgb, w, h));
            }
            "--synthetic" => {
                let (w, h) = value.split_once('x').expect("--synthetic WxH");
                let (w, h) = (w.parse().expect("width"), h.parse().expect("height"));
                images.push((format!("synthetic-{value}"), corpus::photo_rgb(w, h), w, h));
            }
            _ => panic!("unknown flag {flag}"),
        }
    }
    println!("image,width,height,config,quality,baseline_bytes,progressive_bytes,scans");
    for (name, rgb, w, h) in &images {
        for (config_name, config) in CONFIGS {
            for &q in &exp29::QUALITIES {
                let baseline = encode(config, q, rgb, *w, *h, false);
                let progressive = encode(config, q, rgb, *w, *h, true);
                let decoded = |jpeg: &[u8]| {
                    libjpeg_oracle::decode(jpeg)
                        .expect("libjpeg-turbo decode")
                        .pixels
                };
                assert!(
                    decoded(&baseline) == decoded(&progressive),
                    "{name} {config_name} q{q}: the progressive stream decodes differently"
                );
                println!(
                    "{name},{w},{h},{config_name},{q},{},{},{}",
                    baseline.len(),
                    progressive.len(),
                    scans(&progressive)
                );
            }
        }
    }
}
