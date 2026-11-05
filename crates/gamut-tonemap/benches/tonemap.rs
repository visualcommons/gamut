//! Tone-curve throughput benchmarks (issue #149).
//!
//! Intentionally tight: `ToneCurve::map_slice` — the per-sample inner loop of HDR→SDR
//! conversion — over a ~1M-sample buffer for the built-in operators. Counters report samples
//! mapped per second. Run with `cargo bench -p gamut-tonemap`.

use divan::Bencher;
use divan::counter::ItemsCount;
use gamut_tonemap::{Aces, Clamp, Drago, Exposure, Hable, Reinhard, ReinhardExtended, ToneCurve};

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
