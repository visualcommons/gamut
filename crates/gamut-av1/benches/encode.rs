//! AV1 still-image encode throughput benchmarks (issue #149).
//!
//! Intentionally tight: lossless intra throughput plus a `qindex` sweep that exposes the
//! quantizer (speed/quality) tradeoff for the lossy path. Counters report source-pixel bytes
//! per second. AV1 intra at full 4:4:4 is the heaviest encoder in the workspace, so the test
//! image is deliberately small. Run with `cargo bench -p gamut-av1` (or `mise run bench`).

use divan::counter::BytesCount;
use divan::{Bencher, black_box};
use gamut_av1::{
    Av1Colour, Speed, encode_still_intra, encode_still_intra_with_speed,
    encode_still_lossless_identity,
};
use gamut_color::Planar8;

fn main() {
    divan::main();
}

/// Side length of the square RGB test image. Small: AV1 intra is the slowest path here.
const SIDE: u32 = 128;

/// A deterministic, non-trivial RGB gradient so the encoder sees realistic residual energy
/// (a flat image would take the all-zero fast path and misrepresent throughput).
fn gradient_planes() -> Planar8 {
    let side = SIDE;
    let mut rgb = vec![0u8; (side * side * 3) as usize];
    for y in 0..side {
        for x in 0..side {
            let i = ((y * side + x) * 3) as usize;
            rgb[i] = (x ^ y) as u8;
            rgb[i + 1] = x.wrapping_mul(3).wrapping_add(y) as u8;
            rgb[i + 2] = x.wrapping_add(y.wrapping_mul(7)) as u8;
        }
    }
    Planar8::from_rgb8_identity(&rgb, side, side).unwrap()
}

fn source_bytes() -> usize {
    (SIDE * SIDE * 3) as usize
}

#[divan::bench]
fn encode_lossless(bencher: Bencher) {
    let planes = gradient_planes();
    bencher
        .counter(BytesCount::new(source_bytes()))
        .bench_local(|| encode_still_lossless_identity(black_box(&planes)).unwrap());
}

#[divan::bench(args = [32, 96, 160, 224])]
fn encode_intra(bencher: Bencher, qindex: u8) {
    let planes = gradient_planes();
    bencher
        .counter(BytesCount::new(source_bytes()))
        .bench_local(|| encode_still_intra(black_box(&planes), qindex).unwrap());
}

/// The [`Speed`] presets at one mid quantizer, indexed into [`Speed::ALL`] (slowest first): the
/// encode time each preset's pruning buys.
#[divan::bench(args = [0, 1, 2, 3])]
fn encode_intra_speed(bencher: Bencher, preset: usize) {
    let planes = gradient_planes();
    let speed = Speed::ALL[preset];
    bencher
        .counter(BytesCount::new(source_bytes()))
        .bench_local(|| {
            encode_still_intra_with_speed(black_box(&planes), 96, Av1Colour::default(), speed)
                .unwrap()
        });
}
