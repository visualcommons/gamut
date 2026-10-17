//! JPEG decode throughput against zune-jpeg (issue #305).
//!
//! `cargo bench -p gamut-jpeg --bench decode` first prints a parity table -- each decoder's output
//! against libjpeg-turbo's decode of the same stream, the crate's output oracle -- and then runs
//! the divan arms: `gamut` and `zune-jpeg` decoding the same libjpeg-turbo-written stream, per
//! stream shape. The ratio between the two arms is the number the issue tracks; the parity table is
//! what makes it a like-for-like ratio, since a decoder that skipped work would show up there
//! before it showed up as a speed-up.
//!
//! The streams come from `common/corpus.rs`, shared with the experiment arm
//! (`examples/experiment_305.rs`), which measures the same decoders on 24-megapixel streams with a
//! hundred samples each and writes the raw per-sample data `experiments/305/` keeps. This bench is
//! the tight-loop view: a 3-megapixel image, so `mise run bench` stays affordable, still far larger
//! than any cache, so the per-pixel paths are what is measured.
//!
//! Counters report bytes of decoded RGB8 per second, comparable with the other codec suites.
//! Decoding to `Rgb8` includes gamut's upsampling and colour conversion, as zune-jpeg's RGB output
//! includes its own: the bench measures what a caller asking for pixels pays.

use divan::counter::BytesCount;
use divan::{Bencher, black_box};
use gamut_core::{DecodeImage, Rgb8};
use gamut_jpeg::JpegDecoder;
use zune_jpeg::zune_core::bytestream::ZCursor;

#[path = "common/corpus.rs"]
mod corpus;

/// Image geometry for the divan arms: 2048x1536, three megapixels.
const WIDTH: u32 = 2048;
/// See [`WIDTH`].
const HEIGHT: u32 = 1536;

fn main() {
    print_parity_table();
    divan::main();
}

fn gamut(jpeg: &[u8]) -> Vec<u8> {
    DecodeImage::<Rgb8>::decode_image(&JpegDecoder::new(), jpeg)
        .expect("gamut decode")
        .as_samples()
        .to_vec()
}

fn zune(jpeg: &[u8]) -> Vec<u8> {
    zune_jpeg::JpegDecoder::new(ZCursor::new(jpeg))
        .decode()
        .expect("zune-jpeg decode")
}

/// Each decoder's output against libjpeg-turbo's decode of the same stream.
fn print_parity_table() {
    println!(
        "\nDecode parity vs libjpeg-turbo ({WIDTH}x{HEIGHT}, q{}):",
        corpus::QUALITY
    );
    println!(
        "{:<16} {:>9} {:>9} {:>11} {:>9} {:>11}",
        "stream", "bytes", "gamut max", "gamut mean", "zune max", "zune mean"
    );
    for s in corpus::streams(WIDTH, HEIGHT) {
        let reference = libjpeg_oracle::decode_forced_rgb(&s.jpeg)
            .expect("libjpeg-turbo decode")
            .pixels;
        let (gmax, gmean) = corpus::diff(&gamut(&s.jpeg), &reference);
        let (zmax, zmean) = corpus::diff(&zune(&s.jpeg), &reference);
        println!(
            "{:<16} {:>9} {gmax:>9} {gmean:>11.4} {zmax:>9} {zmean:>11.4}",
            s.name,
            s.jpeg.len()
        );
    }
    println!();
}

/// The stream-shape names of [`corpus::SHAPES`]: the divan argument, so each arm runs per shape.
fn shapes() -> impl Iterator<Item = &'static str> {
    corpus::SHAPES.iter().map(|&(name, _, _)| name)
}

/// The stream for one shape, encoded once per arm outside the timed region.
fn stream(shape: &str) -> Vec<u8> {
    corpus::streams(WIDTH, HEIGHT)
        .into_iter()
        .find(|s| s.name == shape)
        .expect("shape is one of corpus::SHAPES")
        .jpeg
}

/// Decoded RGB8 bytes per decode: the throughput counter.
fn rgb_bytes() -> BytesCount {
    BytesCount::new(WIDTH as usize * HEIGHT as usize * 3)
}

#[divan::bench(args = shapes(), sample_count = 100, sample_size = 1)]
fn gamut_decode(bencher: Bencher, shape: &str) {
    let jpeg = stream(shape);
    bencher
        .counter(rgb_bytes())
        .bench(|| gamut(black_box(&jpeg)));
}

#[divan::bench(args = shapes(), sample_count = 100, sample_size = 1)]
fn zune_decode(bencher: Bencher, shape: &str) {
    let jpeg = stream(shape);
    bencher
        .counter(rgb_bytes())
        .bench(|| zune(black_box(&jpeg)));
}
