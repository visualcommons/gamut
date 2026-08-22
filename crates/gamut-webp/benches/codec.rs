//! WebP encode/decode throughput benchmarks (issue #149).
//!
//! Intentionally tight: VP8L (lossless) and VP8 (lossy) encode throughput, a lossy
//! quality sweep that exposes the speed/quality tradeoff, and the matching decode paths.
//! Counters report bytes of *source* pixels per second so figures are comparable across
//! codecs. Run with `cargo bench -p gamut-webp` (or `mise run bench`).

use divan::counter::BytesCount;
use divan::{Bencher, black_box};
use gamut_core::{DecodeImage, Dimensions, EncodeImage, ImageBuf, ImageRef, Rgb8};
use gamut_webp::{Effort, NearLossless, WebpDecoder, WebpEncoder};

fn main() {
    divan::main();
}

/// Side length of the square RGB test image. Modest on purpose so the suite stays quick.
const SIDE: u32 = 256;

/// A deterministic, non-trivial RGB gradient — avoids the all-constant fast paths so the
/// measured work reflects realistic entropy.
fn gradient_rgb(side: u32) -> Vec<u8> {
    let mut buf = vec![0u8; (side * side * 3) as usize];
    for y in 0..side {
        for x in 0..side {
            let i = ((y * side + x) * 3) as usize;
            buf[i] = (x ^ y) as u8;
            buf[i + 1] = x.wrapping_mul(3).wrapping_add(y) as u8;
            buf[i + 2] = x.wrapping_add(y.wrapping_mul(7)) as u8;
        }
    }
    buf
}

fn dims() -> Dimensions {
    Dimensions {
        width: SIDE,
        height: SIDE,
    }
}

fn source_bytes() -> usize {
    (SIDE * SIDE * 3) as usize
}

#[divan::bench]
fn encode_lossless(bencher: Bencher) {
    let rgb = gradient_rgb(SIDE);
    let dims = dims();
    bencher
        .counter(BytesCount::new(source_bytes()))
        .bench_local(|| {
            let mut out = Vec::new();
            let image = ImageRef::<Rgb8>::new(black_box(&rgb), dims).unwrap();
            WebpEncoder::lossless()
                .encode_image(image, &mut out)
                .unwrap();
            out
        });
}

#[divan::bench(args = [25, 50, 75, 90])]
fn encode_lossy(bencher: Bencher, quality: u8) {
    let rgb = gradient_rgb(SIDE);
    let dims = dims();
    bencher
        .counter(BytesCount::new(source_bytes()))
        .bench_local(|| {
            let mut out = Vec::new();
            let image = ImageRef::<Rgb8>::new(black_box(&rgb), dims).unwrap();
            WebpEncoder::lossy(quality)
                .encode_image(image, &mut out)
                .unwrap();
            out
        });
}

/// The effort ladder's cost, which is the other half of what the knob trades. Lossless is where
/// the spread is widest: the low rungs try a single encoding, the high ones race a few dozen.
#[divan::bench(args = [0, 2, 4, 6])]
fn encode_lossless_effort(bencher: Bencher, level: u8) {
    let rgb = gradient_rgb(SIDE);
    let dims = dims();
    let effort = Effort::from_level(level).expect("0..=6");
    bencher
        .counter(BytesCount::new(source_bytes()))
        .bench_local(|| {
            let mut out = Vec::new();
            let image = ImageRef::<Rgb8>::new(black_box(&rgb), dims).unwrap();
            WebpEncoder::lossless()
                .with_effort(effort)
                .encode_image(image, &mut out)
                .unwrap();
            out
        });
}

/// The lossy ladder: rung 0 skips the 4x4 search entirely, the upper rungs add a second entropy
/// pass over the recorded macroblocks.
#[divan::bench(args = [0, 2, 4, 6])]
fn encode_lossy_effort(bencher: Bencher, level: u8) {
    let rgb = gradient_rgb(SIDE);
    let dims = dims();
    let effort = Effort::from_level(level).expect("0..=6");
    bencher
        .counter(BytesCount::new(source_bytes()))
        .bench_local(|| {
            let mut out = Vec::new();
            let image = ImageRef::<Rgb8>::new(black_box(&rgb), dims).unwrap();
            WebpEncoder::lossy(75)
                .with_effort(effort)
                .encode_image(image, &mut out)
                .unwrap();
            out
        });
}

/// Near-lossless costs a second lossless encode, because the encoder keeps whichever of the exact
/// and preprocessed codings is smaller.
#[divan::bench]
fn encode_near_lossless(bencher: Bencher) {
    let rgb = gradient_rgb(SIDE);
    let dims = dims();
    let strength = NearLossless::new(60).expect("0..=99");
    bencher
        .counter(BytesCount::new(source_bytes()))
        .bench_local(|| {
            let mut out = Vec::new();
            let image = ImageRef::<Rgb8>::new(black_box(&rgb), dims).unwrap();
            WebpEncoder::lossless()
                .with_near_lossless(Some(strength))
                .encode_image(image, &mut out)
                .unwrap();
            out
        });
}

#[divan::bench]
fn decode_lossless(bencher: Bencher) {
    let rgb = gradient_rgb(SIDE);
    let mut encoded = Vec::new();
    WebpEncoder::lossless()
        .encode_image(ImageRef::<Rgb8>::new(&rgb, dims()).unwrap(), &mut encoded)
        .unwrap();
    bencher
        .counter(BytesCount::new(source_bytes()))
        .bench_local(|| -> ImageBuf<Rgb8> {
            WebpDecoder::new()
                .decode_image(black_box(&encoded))
                .unwrap()
        });
}

#[divan::bench]
fn decode_lossy(bencher: Bencher) {
    let rgb = gradient_rgb(SIDE);
    let mut encoded = Vec::new();
    WebpEncoder::lossy(75)
        .encode_image(ImageRef::<Rgb8>::new(&rgb, dims()).unwrap(), &mut encoded)
        .unwrap();
    bencher
        .counter(BytesCount::new(source_bytes()))
        .bench_local(|| -> ImageBuf<Rgb8> {
            WebpDecoder::new()
                .decode_image(black_box(&encoded))
                .unwrap()
        });
}
