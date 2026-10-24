//! Measurement arm for experiment #29: which quantization choices the encoder does not make yet buy
//! rate at equal fidelity, measured through the crate's public API only.
//!
//! Not a test and not a benchmark: it asserts nothing. Sandboxed experiment code under the #29
//! policy -- it must keep compiling as the library moves, and it is validated only by the quality
//! numbers it prints. `experiments/29/run.sh` (`mise run experiment 29`) drives it; the verdict and
//! the tables live in `experiments/29/RESULTS.md`.
//!
//! Every variant is a configuration of the shipped `JpegEncoder`
//! (`with_quality` / `with_quant_tables` / `with_subsampling` / `with_rd_optimization` /
//! `with_color_mode`), always with `with_optimized_tables(true)` so that the Huffman tables fit each
//! variant's coefficients and the comparison is between quantization choices, not entropy tables.
//! Nothing in `crates/gamut-jpeg/src` is patched. The alternate tables are built here, from the
//! formulas documented on [`csf_luma`], [`flat`] and [`scaled_table`], and handed in through
//! `QuantTables::new(..).scaled(quality)`, so the frozen IJG quality mapping applies to them exactly
//! as it does to Annex K.
//!
//! Every stream is decoded by libjpeg-turbo (the crate's oracle), not by gamut's own decoder, so a
//! defect shared by gamut's encoder and decoder cannot flatter a variant. XYB streams are decoded
//! to their passthrough scaled-XYB samples and inverted here to sRGB the way an ICC-aware consumer
//! would (`gamut_color::xyb`), as `tests/xyb.rs` does.
//!
//! Two subcommands:
//!
//! - `measure --image NAME=PATH ... [--synthetic WxH]` prints one CSV row per image x variant x
//!   quality: `image,width,height,variant,quality,bytes,bpp,psnr_y,psnr_rgb,ssim_y`. `psnr_y` is
//!   PSNR of BT.601 luma (`0.299 R + 0.587 G + 0.114 B`, unrounded) against the source; `psnr_rgb`
//!   is PSNR over all three sRGB channels; `ssim_y` is Wang et al. (2004) SSIM on that luma with
//!   the paper's 11x11 Gaussian window (sigma 1.5), K1 = 0.01, K2 = 0.03, averaged over every
//!   window that fits (no padding). Inputs are binary PPM (`P6`, maxval 255) or uncompressed
//!   24-bit BMP. `--synthetic` adds the deterministic photo-like generator the decode bench uses
//!   (`benches/common/corpus.rs`).
//! - `bdrate --baseline VARIANT FILE` reads that CSV and prints, per image x variant x metric, the
//!   Bjontegaard-style rate difference against the baseline: the mean of
//!   `ln(bpp_variant) - ln(bpp_baseline)` over the metrics' overlapping range, with each curve
//!   interpolated piecewise-linearly in `ln(bpp)` (200 evenly spaced metric values), reported as
//!   `exp(mean) - 1` in percent. Negative is smaller files at equal fidelity. SSIM is taken as
//!   `-10 log10(1 - SSIM)` so its top end is not compressed. Columns:
//!   `image,variant,baseline,metric,bd_rate_pct,overlap_lo,overlap_hi`; an `image` of `mean` is the
//!   arithmetic mean over the images whose curves overlap.
//!
//! `tables` prints the alternate base tables, natural order, one CSV row per table.

use std::collections::BTreeMap;
use std::path::Path;

use gamut_color::transfer::srgb_oetf;
use gamut_color::xyb::{unscale_xyb, xyb_to_linear_srgb};
use gamut_core::{Dimensions, EncodeImage, ImageRef, Rgb8};
use gamut_jpeg::{
    CHROMINANCE, ChromaSubsampling, JpegColorMode, JpegEncoder, LUMINANCE, QuantTables,
    RdOptimization,
};

#[allow(
    dead_code,
    reason = "the arm uses only the generator, not the bench's streams"
)]
#[path = "../benches/common/corpus.rs"]
mod corpus;

/// The quality sweep every variant is encoded at.
const QUALITIES: [u8; 14] = [30, 35, 40, 45, 50, 55, 60, 65, 70, 75, 80, 85, 90, 95];

/// Viewing geometries for the CSF tables, in pixels per degree of visual angle: 32 is a 96 dpi
/// display at about 48 cm, 64 the same display at twice the distance (or a 192 dpi one at 48 cm).
/// An experiment free choice; the CSF table's shape depends on it, steeper as it grows.
const PIXELS_PER_DEGREE: [f64; 2] = [32.0, 64.0];

/// Mannos and Sakrison (1974) contrast sensitivity at `f` cycles per degree:
/// `A(f) = 2.6 (0.0192 + 0.114 f) exp(-(0.114 f)^1.1)`.
fn mannos_sakrison(f: f64) -> f64 {
    2.6 * (0.0192 + 0.114 * f) * (-(0.114 * f).powf(1.1)).exp()
}

/// Geometric mean of a table's entries.
fn geomean(t: &[f64]) -> f64 {
    (t.iter().map(|v| v.ln()).sum::<f64>() / t.len() as f64).exp()
}

/// Rounds and clamps real-valued steps into a baseline table.
fn to_table(steps: &[f64; 64]) -> [u8; 64] {
    steps.map(|s| s.round().clamp(1.0, 255.0) as u8)
}

/// The Annex K luminance table as reals, natural order.
fn annex_k_luma() -> [f64; 64] {
    LUMINANCE.map(f64::from)
}

/// A luminance table inversely proportional to the Mannos-Sakrison CSF at each coefficient's
/// radial frequency, with the CSF held at its peak below the peak frequency (the usual low-pass
/// plateau: the raw CSF falls towards DC, which would quantize DC hardest). Coefficient `(u, v)`'s
/// basis completes `u / 2` cycles per 8 samples, so its frequency is `sqrt(u^2 + v^2) / 16` cycles
/// per pixel times `ppd` (one of [`PIXELS_PER_DEGREE`]). Scaled so its geometric mean equals
/// Annex K.1's, which keeps the quality sweep's strength comparable; the BD-rate does not depend
/// on that scale.
fn csf_luma(ppd: f64) -> [u8; 64] {
    let (peak, _) = (1..=800)
        .map(|i| f64::from(i) * 0.05)
        .map(|f| (f, mannos_sakrison(f)))
        .fold((0.0, 0.0), |best, p| if p.1 > best.1 { p } else { best });
    let mut steps = [0f64; 64];
    for (i, s) in steps.iter_mut().enumerate() {
        let (u, v) = ((i % 8) as f64, (i / 8) as f64);
        let f = (u * u + v * v).sqrt() / 16.0 * ppd;
        *s = 1.0 / mannos_sakrison(f.max(peak));
    }
    let k = geomean(&annex_k_luma()) / geomean(&steps);
    to_table(&steps.map(|s| s * k))
}

/// A flat table at Annex K.1's geometric mean: every frequency quantized alike, the MSE-optimal
/// shape for an orthonormal transform.
fn flat() -> [u8; 64] {
    to_table(&[geomean(&annex_k_luma()); 64])
}

/// `base` with every step multiplied by `k`.
fn scaled_table(base: &[u8; 64], k: f64) -> [u8; 64] {
    to_table(&base.map(|b| f64::from(b) * k))
}

/// Which base tables a variant scales by quality.
#[derive(Clone, Copy)]
enum Tables {
    /// `with_quality(q)`: the frozen Annex K path.
    AnnexK,
    /// `with_quant_tables(QuantTables::new(luma, chroma).scaled(q))`.
    Custom([u8; 64], [u8; 64]),
}

/// One encoder configuration under test.
struct Variant {
    name: &'static str,
    xyb: bool,
    subsampling: ChromaSubsampling,
    rd: RdOptimization,
    tables: Tables,
}

fn variants() -> Vec<Variant> {
    let v = |name, xyb, subsampling, rd, tables| Variant {
        name,
        xyb,
        subsampling,
        rd,
        tables,
    };
    let (s420, s444) = (ChromaSubsampling::Ycbcr420, ChromaSubsampling::Ycbcr444);
    let (none, trellis) = (RdOptimization::None, RdOptimization::Trellis);
    let custom = |l, c| Tables::Custom(l, c);
    let [csf32, csf64] = PIXELS_PER_DEGREE.map(csf_luma);
    vec![
        // YCbCr: the shipped baseline and the shipped RD modes, for scale.
        v("annexk", false, s420, none, Tables::AnnexK),
        v("annexk-444", false, s444, none, Tables::AnnexK),
        v("trellis", false, s420, trellis, Tables::AnnexK),
        v(
            "trellis-adaptive",
            false,
            s420,
            RdOptimization::TrellisAdaptive,
            Tables::AnnexK,
        ),
        // YCbCr: alternate luminance tables, Annex K chroma.
        v("luma-flat", false, s420, none, custom(flat(), CHROMINANCE)),
        v("luma-csf32", false, s420, none, custom(csf32, CHROMINANCE)),
        v("luma-csf64", false, s420, none, custom(csf64, CHROMINANCE)),
        v(
            "luma-csf32-trellis",
            false,
            s420,
            trellis,
            custom(csf32, CHROMINANCE),
        ),
        // YCbCr: the luma/chroma balance, Annex K luma.
        v(
            "chroma-x0.5",
            false,
            s420,
            none,
            custom(LUMINANCE, scaled_table(&CHROMINANCE, 0.5)),
        ),
        v(
            "chroma-x2",
            false,
            s420,
            none,
            custom(LUMINANCE, scaled_table(&CHROMINANCE, 2.0)),
        ),
        // XYB (always 4:4:4): the shipped placeholder pairing, then XYB-directed tables. X and Y
        // share destination 0 and the stored B - Y uses destination 1; the public API has no third.
        v("xyb-annexk", true, s444, none, Tables::AnnexK),
        v("xyb-flat", true, s444, none, custom(flat(), flat())),
        v("xyb-csf32", true, s444, none, custom(csf32, CHROMINANCE)),
        v(
            "xyb-b-x0.5",
            true,
            s444,
            none,
            custom(LUMINANCE, scaled_table(&CHROMINANCE, 0.5)),
        ),
        v(
            "xyb-b-x2",
            true,
            s444,
            none,
            custom(LUMINANCE, scaled_table(&CHROMINANCE, 2.0)),
        ),
        v(
            "xyb-b-x4",
            true,
            s444,
            none,
            custom(LUMINANCE, scaled_table(&CHROMINANCE, 4.0)),
        ),
        v("xyb-trellis", true, s444, trellis, Tables::AnnexK),
    ]
}

/// Encodes `rgb` with `variant` at `quality`.
fn encode(variant: &Variant, quality: u8, rgb: &[u8], w: u32, h: u32) -> Vec<u8> {
    let dims = Dimensions::new(w, h).expect("dimensions");
    let image = ImageRef::<Rgb8>::new(rgb, dims).expect("image");
    let mut enc = JpegEncoder::new()
        .with_optimized_tables(true)
        .with_subsampling(variant.subsampling)
        .with_rd_optimization(variant.rd);
    if variant.xyb {
        enc = enc.with_color_mode(JpegColorMode::Xyb);
    }
    enc = match variant.tables {
        Tables::AnnexK => enc.with_quality(quality),
        Tables::Custom(l, c) => {
            enc.with_quant_tables(QuantTables::new(l, c).expect("tables").scaled(quality))
        }
    };
    let mut out = Vec::new();
    enc.encode_image(image, &mut out).expect("encode");
    out
}

/// Decodes with libjpeg-turbo to sRGB; XYB passthrough samples are inverted to sRGB.
fn decode(jpeg: &[u8], xyb: bool) -> Vec<u8> {
    let samples = libjpeg_oracle::decode_forced_rgb(jpeg)
        .expect("libjpeg-turbo decode")
        .pixels;
    if !xyb {
        return samples;
    }
    samples
        .as_chunks::<3>()
        .0
        .iter()
        .flat_map(|px| {
            let scaled = px.map(|s| f64::from(s) / 255.0);
            let linear = xyb_to_linear_srgb(unscale_xyb(scaled));
            linear.map(|v| (srgb_oetf(v.clamp(0.0, 1.0)) * 255.0).round() as u8)
        })
        .collect()
}

/// BT.601 luma of interleaved RGB, unrounded.
fn luma(rgb: &[u8]) -> Vec<f64> {
    rgb.as_chunks::<3>()
        .0
        .iter()
        .map(|p| 0.299 * f64::from(p[0]) + 0.587 * f64::from(p[1]) + 0.114 * f64::from(p[2]))
        .collect()
}

fn psnr(sse: f64, n: usize) -> f64 {
    let mse = sse / n as f64;
    if mse == 0.0 {
        f64::INFINITY
    } else {
        10.0 * (255.0 * 255.0 / mse).log10()
    }
}

/// Separable 11x11 Gaussian (sigma 1.5) over the windows that fit: a `(w - 10) x (h - 10)` plane.
fn blur(p: &[f64], w: usize, h: usize, kernel: &[f64; 11]) -> Vec<f64> {
    let (ow, oh) = (w - 10, h - 10);
    let mut rows = vec![0f64; ow * h];
    for y in 0..h {
        for x in 0..ow {
            rows[y * ow + x] = (0..11).map(|k| kernel[k] * p[y * w + x + k]).sum();
        }
    }
    let mut out = vec![0f64; ow * oh];
    for y in 0..oh {
        for x in 0..ow {
            out[y * ow + x] = (0..11).map(|k| kernel[k] * rows[(y + k) * ow + x]).sum();
        }
    }
    out
}

/// Mean SSIM (Wang, Bovik, Sheikh and Simoncelli 2004) of two luma planes.
fn ssim(a: &[f64], b: &[f64], w: usize, h: usize) -> f64 {
    let mut kernel = [0f64; 11];
    for (i, k) in kernel.iter_mut().enumerate() {
        let d = i as f64 - 5.0;
        *k = (-d * d / (2.0 * 1.5 * 1.5)).exp();
    }
    let sum: f64 = kernel.iter().sum();
    kernel.iter_mut().for_each(|k| *k /= sum);
    let prod =
        |f: &dyn Fn(f64, f64) -> f64| a.iter().zip(b).map(|(&x, &y)| f(x, y)).collect::<Vec<_>>();
    let mu_a = blur(a, w, h, &kernel);
    let mu_b = blur(b, w, h, &kernel);
    let aa = blur(&prod(&|x, _| x * x), w, h, &kernel);
    let bb = blur(&prod(&|_, y| y * y), w, h, &kernel);
    let ab = blur(&prod(&|x, y| x * y), w, h, &kernel);
    let (c1, c2) = ((0.01f64 * 255.0).powi(2), (0.03f64 * 255.0).powi(2));
    let n = mu_a.len();
    (0..n)
        .map(|i| {
            let (ma, mb) = (mu_a[i], mu_b[i]);
            let (va, vb, cov) = (aa[i] - ma * ma, bb[i] - mb * mb, ab[i] - ma * mb);
            ((2.0 * ma * mb + c1) * (2.0 * cov + c2)) / ((ma * ma + mb * mb + c1) * (va + vb + c2))
        })
        .sum::<f64>()
        / n as f64
}

/// Reads a binary PPM (`P6`, maxval 255) or an uncompressed 24-bit BMP to interleaved RGB.
fn load(path: &Path) -> (Vec<u8>, u32, u32) {
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    if bytes.starts_with(b"P6") {
        load_ppm(&bytes)
    } else if bytes.starts_with(b"BM") {
        load_bmp(&bytes)
    } else {
        panic!("{}: neither P6 PPM nor BMP", path.display())
    }
}

fn load_ppm(bytes: &[u8]) -> (Vec<u8>, u32, u32) {
    let mut fields = Vec::new();
    let mut i = 2;
    while fields.len() < 3 {
        while bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if bytes[i] == b'#' {
            while bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        let start = i;
        while bytes[i].is_ascii_digit() {
            i += 1;
        }
        let field = std::str::from_utf8(&bytes[start..i]).expect("ascii");
        fields.push(field.parse::<u32>().expect("PPM header field"));
    }
    assert_eq!(fields[2], 255, "PPM maxval must be 255");
    let (w, h) = (fields[0], fields[1]);
    let data = &bytes[i + 1..];
    (data[..(w * h * 3) as usize].to_vec(), w, h)
}

fn load_bmp(bytes: &[u8]) -> (Vec<u8>, u32, u32) {
    let le32 = |o: usize| u32::from_le_bytes(bytes[o..o + 4].try_into().expect("4 bytes"));
    let offset = le32(10) as usize;
    let w = le32(18);
    let raw_h = le32(22) as i32;
    let bpp = u16::from_le_bytes([bytes[28], bytes[29]]);
    assert_eq!(bpp, 24, "only 24-bit BMP");
    assert_eq!(le32(30), 0, "only uncompressed (BI_RGB) BMP");
    let h = raw_h.unsigned_abs();
    let stride = (w as usize * 3).div_ceil(4) * 4;
    let mut rgb = Vec::with_capacity(w as usize * h as usize * 3);
    for y in 0..h as usize {
        // Positive height is bottom-up.
        let row = if raw_h > 0 { h as usize - 1 - y } else { y };
        let start = offset + row * stride;
        for px in bytes[start..start + w as usize * 3].as_chunks::<3>().0 {
            rgb.extend_from_slice(&[px[2], px[1], px[0]]);
        }
    }
    (rgb, w, h)
}

fn measure(args: &[String]) {
    let mut images: Vec<(String, Vec<u8>, u32, u32)> = Vec::new();
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        let value = it.next().unwrap_or_else(|| panic!("{flag} needs a value"));
        match flag.as_str() {
            "--image" => {
                let (name, path) = value.split_once('=').expect("--image NAME=PATH");
                let (rgb, w, h) = load(Path::new(path));
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
    println!("image,width,height,variant,quality,bytes,bpp,psnr_y,psnr_rgb,ssim_y");
    for (name, rgb, w, h) in &images {
        let (wu, hu) = (*w as usize, *h as usize);
        let src_y = luma(rgb);
        for variant in variants() {
            for &q in &QUALITIES {
                let jpeg = encode(&variant, q, rgb, *w, *h);
                let out = decode(&jpeg, variant.xyb);
                assert_eq!(out.len(), rgb.len(), "decoded geometry");
                let out_y = luma(&out);
                let sse_y: f64 = src_y.iter().zip(&out_y).map(|(a, b)| (a - b).powi(2)).sum();
                let sse_rgb: f64 = rgb
                    .iter()
                    .zip(&out)
                    .map(|(&a, &b)| f64::from(a.abs_diff(b)).powi(2))
                    .sum();
                let bpp = jpeg.len() as f64 * 8.0 / (wu * hu) as f64;
                println!(
                    "{name},{w},{h},{},{q},{},{bpp:.5},{:.4},{:.4},{:.6}",
                    variant.name,
                    jpeg.len(),
                    psnr(sse_y, wu * hu),
                    psnr(sse_rgb, wu * hu * 3),
                    ssim(&src_y, &out_y, wu, hu)
                );
            }
        }
    }
}

/// One rate-distortion point: `(metric, ln bpp)`.
type Curve = Vec<(f64, f64)>;

/// Piecewise-linear `ln bpp` at metric value `m` on a curve sorted by metric.
fn interp(curve: &Curve, m: f64) -> f64 {
    let i = curve.partition_point(|p| p.0 < m).clamp(1, curve.len() - 1);
    let ((m0, r0), (m1, r1)) = (curve[i - 1], curve[i]);
    if m1 == m0 {
        r0
    } else {
        r0 + (r1 - r0) * (m - m0) / (m1 - m0)
    }
}

/// Mean `ln` rate difference of `b` against `a` over their overlapping metric range, or `None`
/// where the ranges do not overlap. Returns `(percent, lo, hi)`.
fn bd_rate(a: &Curve, b: &Curve) -> Option<(f64, f64, f64)> {
    let lo = a[0].0.max(b[0].0);
    let hi = a[a.len() - 1].0.min(b[b.len() - 1].0);
    // Metrics are finite (non-finite points are dropped when the curves are built).
    if hi <= lo {
        return None;
    }
    let n = 200;
    let mean = (0..n)
        .map(|i| {
            let m = lo + (hi - lo) * (f64::from(i) + 0.5) / f64::from(n);
            interp(b, m) - interp(a, m)
        })
        .sum::<f64>()
        / f64::from(n);
    Some(((mean.exp() - 1.0) * 100.0, lo, hi))
}

fn bdrate(args: &[String]) {
    let [flag, baseline, file] = args else {
        panic!("bdrate --baseline VARIANT FILE");
    };
    assert_eq!(flag, "--baseline");
    let text = std::fs::read_to_string(file).expect("read CSV");
    const METRICS: [&str; 3] = ["psnr_y", "psnr_rgb", "ssim_y_db"];
    // (image, variant) -> per metric curve.
    let mut curves: BTreeMap<(String, String), [Curve; 3]> = BTreeMap::new();
    let mut variant_order: Vec<String> = Vec::new();
    for line in text.lines().skip(1) {
        let c: Vec<&str> = line.split(',').collect();
        let num = |i: usize| c[i].parse::<f64>().expect("number");
        let ln_bpp = num(6).ln();
        let ssim_db = -10.0 * (1.0 - num(9)).log10();
        let e = curves
            .entry((c[0].to_owned(), c[3].to_owned()))
            .or_default();
        for (curve, m) in e.iter_mut().zip([num(7), num(8), ssim_db]) {
            if m.is_finite() {
                curve.push((m, ln_bpp));
            }
        }
        if !variant_order.iter().any(|v| v == c[3]) {
            variant_order.push(c[3].to_owned());
        }
    }
    for curve in curves.values_mut().flat_map(|c| c.iter_mut()) {
        curve.sort_by(|p, q| p.0.total_cmp(&q.0));
    }
    let images: Vec<String> = {
        let mut v: Vec<String> = curves.keys().map(|k| k.0.clone()).collect();
        v.dedup();
        v
    };
    println!("image,variant,baseline,metric,bd_rate_pct,overlap_lo,overlap_hi");
    for variant in variant_order.iter().filter(|v| *v != baseline) {
        for (mi, metric) in METRICS.iter().enumerate() {
            let mut sum = 0.0;
            let mut count = 0u32;
            for image in &images {
                let base = &curves[&(image.clone(), baseline.clone())][mi];
                let this = &curves[&(image.clone(), variant.clone())][mi];
                match bd_rate(base, this) {
                    Some((pct, lo, hi)) => {
                        sum += pct;
                        count += 1;
                        println!("{image},{variant},{baseline},{metric},{pct:.2},{lo:.3},{hi:.3}");
                    }
                    None => println!("{image},{variant},{baseline},{metric},,,"),
                }
            }
            if count > 0 {
                println!(
                    "mean,{variant},{baseline},{metric},{:.2},,",
                    sum / f64::from(count)
                );
            } else {
                println!("mean,{variant},{baseline},{metric},,,");
            }
        }
    }
}

fn tables() {
    let row = |name: &str, t: &[u8; 64]| {
        let cells: Vec<String> = t.iter().map(u8::to_string).collect();
        println!("{name},{}", cells.join(","));
    };
    row("annexk-luma", &LUMINANCE);
    row("annexk-chroma", &CHROMINANCE);
    row("flat", &flat());
    for ppd in PIXELS_PER_DEGREE {
        row(&format!("csf{ppd}-luma"), &csf_luma(ppd));
    }
    for k in [0.5, 2.0, 4.0] {
        row(
            &format!("annexk-chroma-x{k}"),
            &scaled_table(&CHROMINANCE, k),
        );
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("measure") => measure(&args[1..]),
        Some("bdrate") => bdrate(&args[1..]),
        Some("tables") => tables(),
        _ => panic!("usage: experiment_29 measure|bdrate|tables ..."),
    }
}
