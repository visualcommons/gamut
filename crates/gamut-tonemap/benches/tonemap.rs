//! Tone-curve throughput benchmarks (issue #149).
//!
//! Intentionally tight: `ToneCurve::map_slice` — the per-sample inner loop of HDR→SDR
//! conversion — over a ~1M-sample buffer for the built-in operators, and
//! `ToneMapper::map_rgb_slice` over a ~1M-pixel buffer for `CurveToneMapper` in each
//! `ApplyMode` (#1236). Counters report samples or pixels mapped per second. Run with
//! `cargo bench -p gamut-tonemap`.

use divan::Bencher;
use divan::counter::ItemsCount;
use gamut_tonemap::{
    Aces, ApplyMode, Clamp, CurveToneMapper, Drago, Exposure, Hable, Reinhard, ReinhardExtended,
    ToneCurve, ToneMapper,
};

fn main() {
    divan::main();
}

/// Samples mapped per measured iteration (~1M).
const N: usize = 1 << 20;

/// A ramp of linear-light values spanning the SDR range and into HDR highlights (0..≈20).
fn hdr_samples() -> Vec<f32> {
    (0..N).map(|i| (i % 1000) as f32 * 0.02).collect()
}

#[divan::bench]
fn reinhard(bencher: Bencher) {
    let curve = Reinhard;
    bencher
        .counter(ItemsCount::new(N))
        .with_inputs(hdr_samples)
        .bench_local_refs(|buf| curve.map_slice(buf));
}

#[divan::bench]
fn reinhard_extended(bencher: Bencher) {
    let curve = ReinhardExtended::new(4.0).unwrap();
    bencher
        .counter(ItemsCount::new(N))
        .with_inputs(hdr_samples)
        .bench_local_refs(|buf| curve.map_slice(buf));
}

#[divan::bench]
fn clamp(bencher: Bencher) {
    let curve = Clamp::new(1.0).unwrap();
    bencher
        .counter(ItemsCount::new(N))
        .with_inputs(hdr_samples)
        .bench_local_refs(|buf| curve.map_slice(buf));
}

#[divan::bench]
fn exposure(bencher: Bencher) {
    let curve = Exposure::new(0.6).unwrap();
    bencher
        .counter(ItemsCount::new(N))
        .with_inputs(hdr_samples)
        .bench_local_refs(|buf| curve.map_slice(buf));
}

#[divan::bench]
fn aces(bencher: Bencher) {
    let curve = Aces;
    bencher
        .counter(ItemsCount::new(N))
        .with_inputs(hdr_samples)
        .bench_local_refs(|buf| curve.map_slice(buf));
}

#[divan::bench]
fn hable(bencher: Bencher) {
    let curve = Hable::default();
    bencher
        .counter(ItemsCount::new(N))
        .with_inputs(hdr_samples)
        .bench_local_refs(|buf| curve.map_slice(buf));
}

#[divan::bench]
fn drago(bencher: Bencher) {
    let curve = Drago::new(20.0).unwrap();
    bencher
        .counter(ItemsCount::new(N))
        .with_inputs(hdr_samples)
        .bench_local_refs(|buf| curve.map_slice(buf));
}

/// A ramp of linear-light pixels with distinct channels, spanning SDR into HDR highlights, so
/// desaturation and every driving signal see varied input.
fn hdr_pixels() -> Vec<[f32; 3]> {
    (0..N)
        .map(|i| {
            let v = (i % 1000) as f32 * 0.02;
            [v, v * 0.5, v * 0.25]
        })
        .collect()
}

/// `CurveToneMapper` over the default `Hable` at a 1000-nit source peak, with ffmpeg's default
/// desaturation, in `mode`.
fn bench_mapper(bencher: Bencher, mode: ApplyMode) {
    let mapper = CurveToneMapper::new(Hable::default(), 10.0)
        .unwrap()
        .with_mode(mode);
    bencher
        .counter(ItemsCount::new(N))
        .with_inputs(hdr_pixels)
        .bench_local_refs(|buf| mapper.map_rgb_slice(buf));
}

#[divan::bench]
fn mapper_per_channel(bencher: Bencher) {
    bench_mapper(bencher, ApplyMode::PerChannel);
}

#[divan::bench]
fn mapper_luminance(bencher: Bencher) {
    bench_mapper(bencher, ApplyMode::Luminance);
}

#[divan::bench]
fn mapper_max_rgb(bencher: Bencher) {
    bench_mapper(bencher, ApplyMode::MaxRgb);
}
