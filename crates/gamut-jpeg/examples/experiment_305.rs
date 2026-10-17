//! Measurement arm for experiment #305: how far is `gamut-jpeg`'s decoder from zune-jpeg on large
//! camera-shaped streams, and what did this build's changes buy?
//!
//! Not a test and not a benchmark: it asserts nothing. It prints one CSV row per input x decoder x
//! sample for whichever build of the crate it was compiled against. `experiments/305/run.sh`
//! (`mise run experiment 305`) compiles it twice -- once with `crates/gamut-jpeg/src` as it stood
//! before the optimisation (`before`), once at `HEAD` (`head`) -- and runs the two binaries in
//! alternating rounds (`--first-sample`, `--header`), labelling each with `--variant`. The verdict
//! and the table live in `experiments/305/RESULTS.md`.
//!
//! Three decoders read every stream, timed round-robin within each sample so that drift in the
//! machine's state lands on all three alike:
//!
//! - `gamut` -- `JpegDecoder::decode_image` to `Rgb8`, the build under test.
//! - `zune-jpeg` -- the pure-Rust reference the issue measures the gap against, to RGB.
//! - `libjpeg-turbo` -- the crate's oracle, to RGB: the output reference, and a second yardstick.
//!   Its time includes one copy the binding makes out of the C allocation (`tooling/libjpeg-oracle`
//!   `take_owned`). gamut's includes the copy its own presentation makes through
//!   `gamut_core::convert`, which a caller pays too; the arm itself copies nothing.
//!
//! Columns: `input,width,height,jpeg_bytes,variant,decoder,sample,decode_ns,output_fnv64,
//! max_abs_diff_vs_libjpeg,mean_abs_diff_vs_libjpeg`. `decode_ns` is one wall-clock decode;
//! `output_fnv64` fingerprints the decoded samples, so a change that moved a single output byte
//! shows up between variants; the two diff columns compare that output with libjpeg-turbo's decode
//! of the same stream (constant per input x decoder, repeated per row so every row stands alone).
//! Sample `-1` is the untimed warm-up and is not written.
//!
//! The corpus (`benches/common/corpus.rs`) is written to `--corpus-dir` as `<input>.jpg`, so the
//! driver can record each stream's SHA-256 beside the measurements.

use std::path::PathBuf;
use std::time::Instant;

use gamut_core::{DecodeImage, Rgb8};
use gamut_jpeg::JpegDecoder;
use zune_jpeg::zune_core::bytestream::ZCursor;

#[path = "../benches/common/corpus.rs"]
mod corpus;

/// FNV-1a 64-bit over the decoded samples: a fingerprint, not a checksum anyone verifies against.
fn fnv64(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// The three decoders, each returning interleaved RGB8.
const DECODERS: [&str; 3] = ["gamut", "zune-jpeg", "libjpeg-turbo"];

fn decode(decoder: &str, jpeg: &[u8]) -> Vec<u8> {
    match decoder {
        "gamut" => DecodeImage::<Rgb8>::decode_image(&JpegDecoder::new(), jpeg)
            .expect("gamut decode")
            .into_samples(),
        "zune-jpeg" => zune_jpeg::JpegDecoder::new(ZCursor::new(jpeg))
            .decode()
            .expect("zune-jpeg decode"),
        _ => {
            libjpeg_oracle::decode_forced_rgb(jpeg)
                .expect("libjpeg-turbo decode")
                .pixels
        }
    }
}

/// Times one decode. The output is returned so the optimiser cannot discard the work, and dropped by
/// the caller after the clock stops.
fn timed(decoder: &str, jpeg: &[u8]) -> (u128, Vec<u8>) {
    let start = Instant::now();
    let out = decode(decoder, jpeg);
    (start.elapsed().as_nanos(), out)
}

struct Args {
    variant: String,
    samples: usize,
    /// Index of this run's first sample, so a driver that splits the samples into rounds keeps one
    /// numbering per variant.
    first_sample: usize,
    /// Whether to print the CSV header (the first round only).
    header: bool,
    width: u32,
    height: u32,
    corpus_dir: Option<PathBuf>,
}

fn args() -> Args {
    let mut a = Args {
        variant: "head".to_owned(),
        samples: 100,
        first_sample: 0,
        header: true,
        width: 6000,
        height: 4000,
        corpus_dir: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let value = it.next().unwrap_or_else(|| panic!("{flag} needs a value"));
        match flag.as_str() {
            "--variant" => a.variant = value,
            "--samples" => a.samples = value.parse().expect("--samples"),
            "--first-sample" => a.first_sample = value.parse().expect("--first-sample"),
            "--header" => a.header = value.parse().expect("--header true|false"),
            "--width" => a.width = value.parse().expect("--width"),
            "--height" => a.height = value.parse().expect("--height"),
            "--corpus-dir" => a.corpus_dir = Some(PathBuf::from(value)),
            _ => panic!("unknown flag {flag}"),
        }
    }
    a
}

fn main() {
    let a = args();
    let streams = corpus::streams(a.width, a.height);
    if a.header {
        println!(
            "input,width,height,jpeg_bytes,variant,decoder,sample,decode_ns,output_fnv64,\
             max_abs_diff_vs_libjpeg,mean_abs_diff_vs_libjpeg"
        );
    }
    for s in &streams {
        if let Some(dir) = &a.corpus_dir {
            std::fs::write(dir.join(format!("{}.jpg", s.name)), &s.jpeg).expect("write corpus");
        }
        let reference = decode("libjpeg-turbo", &s.jpeg);
        // Warm-up, and the per-decoder output facts every row of this input repeats.
        let facts: Vec<(u64, u8, f64)> = DECODERS
            .iter()
            .map(|d| {
                let out = decode(d, &s.jpeg);
                let (max, mean) = corpus::diff(&out, &reference);
                (fnv64(&out), max, mean)
            })
            .collect();
        for sample in a.first_sample..a.first_sample + a.samples {
            for (d, &(fnv, max, mean)) in DECODERS.iter().zip(&facts) {
                let (ns, out) = timed(d, &s.jpeg);
                drop(out);
                let variant = if *d == "gamut" {
                    a.variant.as_str()
                } else {
                    "-"
                };
                println!(
                    "{},{},{},{},{variant},{d},{sample},{ns},{fnv:016x},{max},{mean:.4}",
                    s.name,
                    a.width,
                    a.height,
                    s.jpeg.len()
                );
            }
        }
    }
}
