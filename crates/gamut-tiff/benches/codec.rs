//! TIFF encode/decode throughput benchmarks (issue #149).
//!
//! Intentionally tight: TIFF is lossless, so the relevant axis is the compression scheme
//! (a speed/size tradeoff rather than speed/quality). The encode sweep covers uncompressed,
//! PackBits, LZW, and LZW with the horizontal predictor; decode round-trips an LZW file.
//! Counters report source-pixel bytes per second. Run with `cargo bench -p gamut-tiff`.

use divan::counter::BytesCount;
use divan::{Bencher, black_box};
use gamut_core::{DecodeImage, Dimensions, EncodeImage, ImageBuf, ImageRef, Rgb8};
use gamut_tiff::{Compression, Predictor, TiffDecoder, TiffEncoder};

fn main() {
    divan::main();
}

/// Side length of the square RGB test image.
const SIDE: u32 = 256;

/// A deterministic, non-trivial RGB gradient — enough local structure that the predictor and
/// run-length schemes have something to work with, without being trivially compressible.
fn gradient_rgb() -> Vec<u8> {
    let side = SIDE;
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
    Dimensions::new(SIDE, SIDE).unwrap()
}

fn source_bytes() -> usize {
    (SIDE * SIDE * 3) as usize
}

#[divan::bench(args = ["none", "packbits", "lzw", "lzw+predictor"])]
fn encode(bencher: Bencher, scheme: &str) {
    let (compression, predictor) = match scheme {
        "none" => (Compression::None, Predictor::None),
        "packbits" => (Compression::PackBits, Predictor::None),
        "lzw" => (Compression::Lzw, Predictor::None),
        "lzw+predictor" => (Compression::Lzw, Predictor::HorizontalDifferencing),
        other => panic!("unknown scheme {other}"),
    };
    let rgb = gradient_rgb();
    let dims = dims();
    bencher
        .counter(BytesCount::new(source_bytes()))
        .bench_local(|| {
            let mut out = Vec::new();
            let image = ImageRef::<Rgb8>::new(black_box(&rgb), dims).unwrap();
            TiffEncoder::new()
                .with_compression(compression)
                .with_predictor(predictor)
                .encode_image(image, &mut out)
                .unwrap();
            out
        });
}

#[divan::bench]
fn decode_lzw(bencher: Bencher) {
    let rgb = gradient_rgb();
    let mut encoded = Vec::new();
    TiffEncoder::new()
        .with_compression(Compression::Lzw)
        .encode_image(ImageRef::<Rgb8>::new(&rgb, dims()).unwrap(), &mut encoded)
        .unwrap();
    bencher
        .counter(BytesCount::new(source_bytes()))
        .bench_local(|| -> ImageBuf<Rgb8> {
            TiffDecoder::new()
                .decode_image(black_box(&encoded))
                .unwrap()
        });
}
