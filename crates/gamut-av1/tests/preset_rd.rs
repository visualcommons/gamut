//! Rate–distortion harness for the [`Speed`] presets (issue #698).
//!
//! Each preset prunes the candidates the lossy mode decisions try; it must not buy that time back
//! as compression. The full search ranks every candidate it shares with a pruned preset by the
//! same reconstruction-distortion-plus-rate cost, so on a fixed corpus it must need no more bits
//! than any pruned preset for the same PSNR. This harness encodes a fixed synthetic corpus over a
//! quantizer ladder at every preset, reports size and PSNR, and asserts that the
//! Bjøntegaard-delta rate of [`Speed::Thorough`] against each pruned preset is not positive.
//!
//! The corpus is the one the issue measured on: 4:4:4 identity stills of natural-image-like
//! "photo" content (smooth shading, off-axis stripes, light noise) and of "screen" content (flat
//! tiles ruled by thin black lines). Run with `--nocapture` to see the table.

use gamut_av1::{Av1Colour, ReconImage, Speed, encode_still_intra_with_speed};
use gamut_color::Planar8;

/// Side of the square corpus images.
const SIDE: u32 = 128;

/// The quantizer ladder each curve is sampled at, up to the top of the range.
///
/// The rungs above 160 sample the coarsest steps (issue #755), where the full search once lost
/// to `Fast`; [`the_full_search_is_not_below_any_pruned_preset_at_the_coarsest_rung`] is what
/// holds that point, because the BD-rate assertion alone cannot.
const LADDER: [u8; 6] = [40, 80, 120, 160, 200, 240];

/// Builds identity planes (Y=G, U=B, V=R) from an RGB generator.
fn planes(f: impl Fn(u32, u32) -> [u8; 3]) -> Planar8 {
    let mut rgb = vec![0u8; (SIDE * SIDE * 3) as usize];
    for y in 0..SIDE {
        for x in 0..SIDE {
            let i = ((y * SIDE + x) * 3) as usize;
            rgb[i..i + 3].copy_from_slice(&f(x, y));
        }
    }
    Planar8::from_rgb8_identity(&rgb, SIDE, SIDE).unwrap()
}

/// A deterministic hash of a position, for the photo's light noise.
fn hash(x: u32, y: u32) -> u32 {
    let mut h = x.wrapping_mul(0x9e37_79b1) ^ y.wrapping_mul(0x85eb_ca77);
    h ^= h >> 15;
    h = h.wrapping_mul(0xc2b2_ae3d);
    h ^ (h >> 13)
}

/// Smooth shading, off-axis stripes and light noise.
fn photo() -> Planar8 {
    planes(|x, y| {
        let (fx, fy) = (x as f64, y as f64);
        let shade = 70.0 + 0.6 * fx + 0.3 * fy;
        let stripes = 30.0 * ((fx * 0.9 + fy * 0.4) / 5.0).sin();
        let noise = f64::from(hash(x, y) % 9) - 4.0;
        let g = shade + stripes + noise;
        let channel = |off: f64, k: f64| (g * k + off).clamp(0.0, 255.0) as u8;
        [channel(10.0, 0.9), channel(0.0, 1.0), channel(-20.0, 0.8)]
    })
}

/// Flat tiles of a few colours, ruled by one-sample black lines.
fn screen() -> Planar8 {
    const TILES: [[u8; 3]; 4] = [
        [230, 230, 225],
        [60, 120, 200],
        [250, 200, 60],
        [40, 170, 90],
    ];
    planes(|x, y| {
        if x % 32 == 0 || y % 24 == 0 {
            [0, 0, 0]
        } else {
            TILES[((x / 32 + 3 * (y / 24)) % 4) as usize]
        }
    })
}

/// PSNR over all three planes of `recon` against `src`, in dB.
fn psnr(src: &Planar8, recon: &ReconImage) -> f64 {
    let mut sse = 0u64;
    let mut n = 0u64;
    for p in 0..3 {
        for (&s, &r) in src.plane(p).iter().zip(&recon.planes[p]) {
            let d = i64::from(s) - i64::from(r);
            sse += (d * d) as u64;
            n += 1;
        }
    }
    let mse = sse as f64 / n as f64;
    10.0 * (255.0 * 255.0 / mse.max(1e-9)).log10()
}

/// One preset's rate–distortion curve: `(PSNR, bytes)` per ladder rung.
fn curve(src: &Planar8, speed: Speed) -> Vec<(f64, f64)> {
    LADDER
        .iter()
        .map(|&q| {
            let (still, recon) =
                encode_still_intra_with_speed(src, q, Av1Colour::default(), speed).unwrap();
            (psnr(src, &recon), still.obus.len() as f64)
        })
        .collect()
}

/// `log(rate)` at `psnr` on the piecewise-linear curve through `points` (sorted by PSNR).
fn log_rate_at(points: &[(f64, f64)], psnr: f64) -> f64 {
    let seg = points
        .windows(2)
        .find(|w| psnr <= w[1].0)
        .unwrap_or(&points[points.len() - 2..]);
    let (p0, r0) = (seg[0].0, seg[0].1.ln());
    let (p1, r1) = (seg[1].0, seg[1].1.ln());
    r0 + (r1 - r0) * (psnr - p0) / (p1 - p0)
}

/// The Bjøntegaard-delta rate of `test` against `anchor`, in percent: the average bit-rate
/// difference at equal PSNR over the PSNR range both curves cover, from piecewise-linear
/// interpolation of `log(rate)`. Negative means `test` needs fewer bits.
fn bd_rate(anchor: &[(f64, f64)], test: &[(f64, f64)]) -> f64 {
    let sorted = |c: &[(f64, f64)]| {
        let mut c = c.to_vec();
        c.sort_by(|a, b| a.0.total_cmp(&b.0));
        c
    };
    let (anchor, test) = (sorted(anchor), sorted(test));
    let lo = anchor[0].0.max(test[0].0);
    let hi = anchor[anchor.len() - 1].0.min(test[test.len() - 1].0);
    assert!(hi > lo, "the two curves share no PSNR range");
    const STEPS: usize = 256;
    let mut diff = 0.0;
    for i in 0..=STEPS {
        let p = lo + (hi - lo) * i as f64 / STEPS as f64;
        diff += log_rate_at(&test, p) - log_rate_at(&anchor, p);
    }
    ((diff / (STEPS + 1) as f64).exp() - 1.0) * 100.0
}

/// PSNR at `bytes` on the curve through `points`, piecewise-linear in `log(rate)` and extended
/// past either end along its outermost segment.
fn psnr_at(points: &[(f64, f64)], bytes: f64) -> f64 {
    let mut points = points.to_vec();
    points.sort_by(|a, b| a.1.total_cmp(&b.1));
    let seg = points
        .windows(2)
        .find(|w| bytes <= w[1].1)
        .unwrap_or(&points[points.len() - 2..]);
    let (r0, p0) = (seg[0].1.ln(), seg[0].0);
    let (r1, p1) = (seg[1].1.ln(), seg[1].0);
    p0 + (p1 - p0) * (bytes.ln() - r0) / (r1 - r0)
}

#[test]
fn the_full_search_is_not_below_any_pruned_preset_at_the_coarsest_rung() {
    // Issue #755: with partitions forced by a texture test, the photo was coded in 4×4
    // transforms throughout, and at `qindex` 240 the full search took `IDTX` on 296 of them,
    // leaving 447 B / 28.94 dB against `Fast`'s 470 B / 30.55 dB. The BD-rate assertion did not
    // see it (Thorough −9.46% against Fast): it averages only over the PSNR range both curves
    // share, and Thorough's sunken point lay below Fast's lowest PSNR. So the coarsest rung is
    // held on its own, as the issue's done criterion states it: at a pruned preset's byte count,
    // the full search's curve reaches at least that preset's PSNR.
    let src = photo();
    let rung = LADDER.len() - 1;
    let thorough = curve(&src, Speed::Thorough);
    let mut below = Vec::new();
    for &speed in &Speed::ALL[1..] {
        let (psnr, bytes) = curve(&src, speed)[rung];
        let reached = psnr_at(&thorough, bytes);
        eprintln!(
            "photo q{} {speed:?} {bytes} B {psnr:.2} dB; Thorough at {bytes} B: {reached:.2} dB",
            LADDER[rung]
        );
        if reached < psnr {
            below.push(format!(
                "{speed:?}: {reached:.2} dB < {psnr:.2} dB at {bytes} B"
            ));
        }
    }
    assert!(
        below.is_empty(),
        "the full search falls below a pruned preset at q{}: {below:?}",
        LADDER[rung]
    );
}

#[test]
fn the_full_search_needs_no_more_bits_than_any_pruned_preset() {
    // Every curve is measured and reported before anything is asserted, so one run shows the
    // whole table and every preset the full search loses to.
    let mut losses = Vec::new();
    for (name, src) in [("photo", photo()), ("screen", screen())] {
        let curves: Vec<(Speed, Vec<(f64, f64)>)> =
            Speed::ALL.iter().map(|&s| (s, curve(&src, s))).collect();
        for (speed, points) in &curves {
            let row: Vec<String> = LADDER
                .iter()
                .zip(points)
                .map(|(q, (p, b))| format!("q{q} {b:>6} B {p:5.2} dB"))
                .collect();
            eprintln!("{name:6} {speed:9?} {}", row.join(" | "));
        }
        let thorough = &curves[0].1;
        for (speed, points) in &curves[1..] {
            let bd = bd_rate(points, thorough);
            eprintln!("{name:6} BD-rate Thorough vs {speed:?}: {bd:+.2}%");
            if bd > 0.0 {
                losses.push(format!("{name} vs {speed:?}: {bd:+.2}%"));
            }
        }
    }
    assert!(
        losses.is_empty(),
        "the full search needs more bits than a pruned preset at equal PSNR: {losses:?}"
    );
}

#[test]
fn screen_quality_keeps_rising_as_the_quantizer_falls() {
    // Issue #754: with in-loop filters fixed by `qindex`, the screen image's thin lines were
    // smeared to about 31.5 dB at every rung, so bytes spent below `qindex` ~120 bought nothing.
    // Filters chosen against the source switch off where they hurt, so every halving of the
    // quantizer step must buy a clear gain at every preset — well past that ceiling.
    let src = screen();
    let mut flat = Vec::new();
    for &speed in Speed::ALL {
        let points = curve(&src, speed);
        for (rung, pair) in LADDER.windows(2).zip(points.windows(2)) {
            let gain = pair[0].0 - pair[1].0;
            if gain < 3.0 {
                flat.push(format!(
                    "{speed:?} q{}→q{}: {gain:+.2} dB",
                    rung[1], rung[0]
                ));
            }
        }
        if points[0].0 < 45.0 {
            flat.push(format!("{speed:?} q{}: {:.2} dB", LADDER[0], points[0].0));
        }
    }
    assert!(flat.is_empty(), "screen quality flattens: {flat:?}");
}
