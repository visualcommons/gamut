//! Color-conversion throughput benchmarks (issue #149).
//!
//! Intentionally tight: the per-pixel hot paths codecs actually lean on — RGB↔YCbCr 4:2:0
//! (chroma subsampling and reconstruction), the sRGB transfer function, and the OKLab forward
//! transform. Counters report pixels (or samples) processed per second. Run with
//! `cargo bench -p gamut-color`.

use divan::counter::ItemsCount;
use divan::{Bencher, black_box};
use gamut_color::oklab::{Gamut, linear_rgb_to_oklab};
use gamut_color::transfer::srgb_eotf;
use gamut_color::{ChromaUpsampling, ColorRange, Yuv420};

fn main() {
    divan::main();
}

/// Side length of the square test image (≈65k pixels).
const SIDE: u32 = 256;

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

fn pixels() -> usize {
    (SIDE * SIDE) as usize
}

#[divan::bench]
fn rgb_to_yuv420(bencher: Bencher) {
    let rgb = gradient_rgb();
    bencher.counter(ItemsCount::new(pixels())).bench_local(|| {
        Yuv420::from_rgb8(black_box(&rgb), SIDE, SIDE, ColorRange::Limited).unwrap()
    });
}

#[divan::bench]
fn yuv420_to_rgb(bencher: Bencher) {
    let yuv = Yuv420::from_rgb8(&gradient_rgb(), SIDE, SIDE, ColorRange::Limited).unwrap();
    bencher.counter(ItemsCount::new(pixels())).bench_local(|| {
        black_box(&yuv).to_rgb8(ColorRange::Limited, ChromaUpsampling::Interpolated)
    });
}

#[divan::bench]
fn srgb_eotf_slice(bencher: Bencher) {
    let samples: Vec<f64> = (0..pixels()).map(|i| (i % 256) as f64 / 255.0).collect();
    bencher
        .counter(ItemsCount::new(samples.len()))
        .bench_local(|| {
            let mut acc = 0.0f64;
            for &x in &samples {
                acc += srgb_eotf(black_box(x));
            }
            acc
        });
}

#[divan::bench]
fn linear_rgb_to_oklab_slice(bencher: Bencher) {
    let rgb: Vec<[f64; 3]> = (0..pixels())
        .map(|i| {
            let v = (i % 256) as f64 / 255.0;
            [v, 1.0 - v, (v * 0.5).fract()]
        })
        .collect();
    bencher.counter(ItemsCount::new(rgb.len())).bench_local(|| {
        let mut acc = [0.0f64; 3];
        for &px in &rgb {
            let lab = linear_rgb_to_oklab(black_box(px), Gamut::Srgb);
            acc[0] += lab[0];
            acc[1] += lab[1];
            acc[2] += lab[2];
        }
        acc
    });
}
