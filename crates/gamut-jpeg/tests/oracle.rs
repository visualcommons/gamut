//! Differential conformance cross-checks against a vendored **libjpeg-turbo** (v3.x).
//!
//! Two directions prove the codec against the canonical reference implementation:
//!
//! - **Encode** — gamut encodes, libjpeg-turbo decodes; the recovered pixels must match the source
//!   within the format's lossy tolerance. This is the real point of the gate: it proves gamut emits
//!   spec-valid streams the reference decoder reads back correctly.
//! - **Decode** — libjpeg-turbo encodes, gamut decodes; gamut's output must match libjpeg-turbo's
//!   own decode of the *same* stream. Decode-vs-decode parity isolates entropy/dequant/IDCT
//!   correctness from the lossy encode error (both decoders see identical coefficients).
//!
//! Tolerances are **measured**, then asserted with a margin; each bound cites the measured worst
//! case in a comment. The exception is decode-vs-decode parity on gray and 4:4:4: gamut's IDCT is
//! libjpeg-turbo's accurate integer IDCT (`gamut_dsp::jpeg::idct8x8_int`, issue #896), so with
//! no upsampling filter between them the two decodes are asserted **byte-identical**. Content is deterministic (a smooth gradient for fidelity-vs-source, a fixed
//! LCG for the geometry-sensitive parity checks) so the numbers are stable across runs.

use gamut_core::{DecodeImage, Dimensions, EncodeImage, Gray8, ImageBuf, ImageRef, Rgb8};
use gamut_jpeg::{ChromaSubsampling, JpegDecoder, JpegEncoder, JpegProcess};
use libjpeg_oracle::{EncodeParams, Subsampling};

/// The dimension battery: block-aligned, odd, and larger-than-MCU cases so component-dimension
/// arithmetic (§A.1.1) and edge padding are all exercised.
const DIMS: &[(u32, u32)] = &[(8, 8), (16, 16), (17, 9), (33, 31), (64, 48)];

/// One colour configuration, shared by both directions.
#[derive(Clone, Copy, Debug)]
enum Mode {
    Gray,
    C444,
    C422,
    C420,
    C440,
}

impl Mode {
    const ALL: [Mode; 5] = [Mode::Gray, Mode::C444, Mode::C422, Mode::C420, Mode::C440];

    fn channels(self) -> u32 {
        match self {
            Mode::Gray => 1,
            _ => 3,
        }
    }

    fn gamut_subsampling(self) -> ChromaSubsampling {
        match self {
            Mode::Gray | Mode::C444 => ChromaSubsampling::Ycbcr444,
            Mode::C422 => ChromaSubsampling::Ycbcr422,
            Mode::C420 => ChromaSubsampling::Ycbcr420,
            Mode::C440 => ChromaSubsampling::Ycbcr440,
        }
    }

    fn oracle_subsampling(self) -> Subsampling {
        match self {
            Mode::C422 => Subsampling::S422,
            Mode::C420 => Subsampling::S420,
            Mode::C440 => Subsampling::S440,
            _ => Subsampling::S444,
        }
    }

    /// The frame's `(Hi, Vi)` per component, as T.81 §B.2.2 declares them: luma carries the
    /// subsampling ratio and both chroma components stay `1×1`.
    fn sampling_factors(self) -> Vec<(u8, u8)> {
        let luma = match self {
            Mode::Gray => return vec![(1, 1)],
            Mode::C444 => (1, 1),
            Mode::C422 => (2, 1),
            Mode::C420 => (2, 2),
            Mode::C440 => (1, 2),
        };
        vec![luma, (1, 1), (1, 1)]
    }

    /// Chroma is upsampled by different filters (gamut = sample replication, libjpeg = fancy
    /// triangle), so subsampled modes diverge legitimately; only gray and 4:4:4 avoid that.
    fn upsampling_matches(self) -> bool {
        matches!(self, Mode::Gray | Mode::C444)
    }
}

// --- content generators -------------------------------------------------------------------------

/// A smooth RGB gradient — JPEG-friendly (low high-frequency energy), so fidelity-vs-source
/// tolerances reflect quantization rather than ringing.
fn rgb_gradient(w: u32, h: u32) -> Vec<u8> {
    let mut px = Vec::with_capacity((w * h * 3) as usize);
    for y in 0..h {
        for x in 0..w {
            px.push((x * 255 / w.max(1)) as u8);
            px.push((y * 255 / h.max(1)) as u8);
            px.push((((x + y) * 255) / (w + h).max(1)) as u8);
        }
    }
    px
}

/// The single-channel luma companion of [`rgb_gradient`].
fn gray_gradient(w: u32, h: u32) -> Vec<u8> {
    let mut px = Vec::with_capacity((w * h) as usize);
    for y in 0..h {
        for x in 0..w {
            px.push((((x * 255 / w.max(1)) as u16 + (y * 255 / h.max(1)) as u16) / 2) as u8);
        }
    }
    px
}

/// A fixed LCG (Numerical Recipes constants) producing deterministic, high-frequency content — the
/// worst case for IDCT-rounding parity and for the upsampling-filter divergence.
struct Lcg(u32);

impl Lcg {
    fn new(seed: u32) -> Self {
        Self(seed)
    }
    fn byte(&mut self) -> u8 {
        self.0 = self.0.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (self.0 >> 24) as u8
    }
}

/// `w * h * channels` deterministic LCG bytes.
fn lcg_pixels(w: u32, h: u32, channels: u32, seed: u32) -> Vec<u8> {
    let mut lcg = Lcg::new(seed);
    (0..(w * h * channels) as usize)
        .map(|_| lcg.byte())
        .collect()
}

// --- metrics ------------------------------------------------------------------------------------

/// Max absolute per-sample difference and the mean-squared error between equal-length buffers.
fn diff_stats(a: &[u8], b: &[u8]) -> (u8, f64) {
    assert_eq!(a.len(), b.len(), "compared buffers differ in length");
    let mut max = 0u8;
    let mut sse = 0f64;
    for (&x, &y) in a.iter().zip(b) {
        let d = x.abs_diff(y);
        max = max.max(d);
        sse += f64::from(d) * f64::from(d);
    }
    (max, sse / a.len().max(1) as f64)
}

/// Peak signal-to-noise ratio (dB); `∞` for an exact match.
fn psnr(mse: f64) -> f64 {
    if mse == 0.0 {
        f64::INFINITY
    } else {
        10.0 * (255.0 * 255.0 / mse).log10()
    }
}

// --- gamut helpers ------------------------------------------------------------------------------

/// Encodes `pixels` with gamut in the given mode, returning the JPEG stream.
fn gamut_encode(mode: Mode, pixels: &[u8], w: u32, h: u32, quality: u8, restart: u16) -> Vec<u8> {
    gamut_encode_cfg(mode, pixels, w, h, quality, restart, false)
}

/// Encodes `pixels` with gamut, selecting baseline or progressive.
fn gamut_encode_cfg(
    mode: Mode,
    pixels: &[u8],
    w: u32,
    h: u32,
    quality: u8,
    restart: u16,
    progressive: bool,
) -> Vec<u8> {
    let enc = JpegEncoder::new()
        .with_quality(quality)
        .with_restart_interval(restart)
        .with_progressive(progressive);
    gamut_encode_with(enc, mode, pixels, w, h)
}

/// Encodes `pixels` with gamut's baseline path using Annex K.2 optimized Huffman tables.
fn gamut_encode_optimized(
    mode: Mode,
    pixels: &[u8],
    w: u32,
    h: u32,
    quality: u8,
    restart: u16,
) -> Vec<u8> {
    let enc = JpegEncoder::new()
        .with_quality(quality)
        .with_restart_interval(restart)
        .with_optimized_tables(true);
    gamut_encode_with(enc, mode, pixels, w, h)
}

/// Drives an already-configured encoder over the mode's pixel type, applying its subsampling.
fn gamut_encode_with(enc: JpegEncoder, mode: Mode, pixels: &[u8], w: u32, h: u32) -> Vec<u8> {
    let dims = Dimensions::new(w, h).unwrap();
    match mode {
        Mode::Gray => enc
            .encode_to_vec(ImageRef::<Gray8>::new(pixels, dims).unwrap())
            .unwrap(),
        _ => enc
            .with_subsampling(mode.gamut_subsampling())
            .encode_to_vec(ImageRef::<Rgb8>::new(pixels, dims).unwrap())
            .unwrap(),
    }
}

/// Decodes `jpeg` with gamut into interleaved samples (1 channel for gray, 3 for colour).
fn gamut_decode(mode: Mode, jpeg: &[u8]) -> (u32, u32, Vec<u8>) {
    let dec = JpegDecoder::new();
    match mode {
        Mode::Gray => {
            let img: ImageBuf<Gray8> = dec.decode_image(jpeg).unwrap();
            (img.width(), img.height(), img.as_samples().to_vec())
        }
        _ => {
            let img: ImageBuf<Rgb8> = dec.decode_image(jpeg).unwrap();
            (img.width(), img.height(), img.as_samples().to_vec())
        }
    }
}

/// Encodes `pixels` with the libjpeg-turbo oracle in the given mode (sequential or progressive).
#[allow(clippy::too_many_arguments)]
fn oracle_encode(
    mode: Mode,
    pixels: &[u8],
    w: u32,
    h: u32,
    quality: i32,
    restart: u16,
    optimize: bool,
    progressive: bool,
) -> Vec<u8> {
    let params = EncodeParams {
        quality,
        gray: matches!(mode, Mode::Gray),
        subsampling: mode.oracle_subsampling(),
        progressive,
        restart_interval: restart,
        optimize_coding: optimize,
    };
    libjpeg_oracle::encode(pixels, w, h, &params).expect("oracle encode")
}

// ================================================================================================
// Encode direction: gamut encodes → libjpeg-turbo decodes.
// ================================================================================================

#[test]
fn encode_battery_decoded_by_libjpeg_turbo() {
    // dims × mode × quality × restart. libjpeg-turbo must decode every stream with the right
    // geometry (stream validity — the real point), and the pixels must be faithful to the SOURCE.
    let mut worst_gray444 = 0u8; // max per-pixel diff, gray + 4:4:4
    let mut worst_sub_mse = 0f64; // worst MSE, 4:2:2 + 4:2:0 + 4:4:0
    for &(w, h) in DIMS {
        for mode in Mode::ALL {
            for &q in &[50u8, 75, 90] {
                for &restart in &[0u16, 3] {
                    let src = if matches!(mode, Mode::Gray) {
                        gray_gradient(w, h)
                    } else {
                        rgb_gradient(w, h)
                    };
                    let jpeg = gamut_encode(mode, &src, w, h, q, restart);
                    let img = libjpeg_oracle::decode(&jpeg).expect("libjpeg-turbo decode");
                    assert_eq!(
                        (img.width, img.height, img.channels),
                        (w, h, mode.channels()),
                        "geometry {mode:?} {w}x{h} q{q} r{restart}"
                    );
                    let (max, mse) = diff_stats(&img.pixels, &src);
                    if mode.upsampling_matches() {
                        worst_gray444 = worst_gray444.max(max);
                    } else {
                        worst_sub_mse = worst_sub_mse.max(mse);
                    }
                }
            }
        }
    }
    // Measured worst over the battery (q ∈ {50,75,90}): gray/444 max-diff = 11 (the q50 cell
    // dominates); subsampled PSNR = 30.11 dB, unchanged when 4:4:0 joined (#1014). Asserted with
    // margin.
    assert!(worst_gray444 <= 16, "gray/444 max-diff {worst_gray444}");
    assert!(
        psnr(worst_sub_mse) > 28.0,
        "subsampled PSNR {:.2} dB",
        psnr(worst_sub_mse)
    );
}

#[test]
fn encode_sampling_factors_are_what_libjpeg_turbo_reads() {
    // The frame header gamut writes for each mode, read back by libjpeg-turbo's own header parser
    // rather than gamut's: the factors the reference decoder upsamples by are the ones the mode
    // names. Baseline and progressive write their SOF through different paths, so both are read.
    let (w, h) = (17u32, 9u32);
    for mode in Mode::ALL {
        let src = gray_or_rgb_gradient(mode, w, h);
        for progressive in [false, true] {
            let jpeg = gamut_encode_cfg(mode, &src, w, h, 75, 0, progressive);
            assert_eq!(
                libjpeg_oracle::sampling_factors(&jpeg).expect("libjpeg-turbo header"),
                mode.sampling_factors(),
                "{mode:?} progressive={progressive}"
            );
        }
    }
}

#[test]
fn encode_fidelity_tracks_libjpeg_turbo_own_encode() {
    // Against the reference doing the same job: for every colour mode, gamut's stream and
    // libjpeg-turbo's own stream of the same source at the same quality, both decoded by
    // libjpeg-turbo, must reconstruct the source to within one PSNR margin of each other. The same
    // margin binds every mode, so a newer grid meets exactly the bar the older ones do.
    for mode in [Mode::C444, Mode::C422, Mode::C420, Mode::C440] {
        let mut worst_gap = f64::NEG_INFINITY; // libjpeg-turbo's PSNR minus gamut's, in dB
        for &(w, h) in DIMS {
            for &q in &[50u8, 75, 90] {
                let src = rgb_gradient(w, h);
                let ours = gamut_encode(mode, &src, w, h, q, 0);
                let theirs = oracle_encode(mode, &src, w, h, i32::from(q), 0, false, false);
                let ours = libjpeg_oracle::decode(&ours).expect("decode gamut stream");
                let theirs = libjpeg_oracle::decode(&theirs).expect("decode libjpeg stream");
                let gap = psnr(diff_stats(&theirs.pixels, &src).1)
                    - psnr(diff_stats(&ours.pixels, &src).1);
                worst_gap = worst_gap.max(gap);
            }
        }
        // Measured worst gap per mode over DIMS × q{50,75,90}: 4:4:4 0.62 dB, 4:2:2 1.33 dB
        // (8×8 q75, the battery's worst), 4:2:0 0.61 dB, 4:4:0 0.41 dB. One margin for all.
        assert!(
            worst_gap <= 2.0,
            "{mode:?}: gamut trails libjpeg-turbo's own encode by {worst_gap:.2} dB"
        );
    }
}

#[test]
fn encode_q90_is_tight_against_source() {
    // A focused high-quality cell: at q90 the gradient should reconstruct nearly exactly through
    // the reference decoder. Gray/444 assert a small per-pixel cap; subsampled a high PSNR floor.
    let mut worst_gray444 = 0u8;
    let mut worst_sub_mse = 0f64;
    for &(w, h) in DIMS {
        for mode in Mode::ALL {
            let src = if matches!(mode, Mode::Gray) {
                gray_gradient(w, h)
            } else {
                rgb_gradient(w, h)
            };
            let jpeg = gamut_encode(mode, &src, w, h, 90, 0);
            let img = libjpeg_oracle::decode(&jpeg).expect("decode");
            let (max, mse) = diff_stats(&img.pixels, &src);
            if mode.upsampling_matches() {
                worst_gray444 = worst_gray444.max(max);
            } else {
                worst_sub_mse = worst_sub_mse.max(mse);
            }
        }
    }
    // Measured at q90: gray/444 max-diff = 5; subsampled PSNR = 34.18 dB. Asserted with margin.
    assert!(worst_gray444 <= 8, "q90 gray/444 max-diff {worst_gray444}");
    assert!(
        psnr(worst_sub_mse) > 31.0,
        "q90 sub PSNR {:.2} dB",
        psnr(worst_sub_mse)
    );
}

#[test]
fn encode_cross_parity_gamut_vs_libjpeg() {
    // Same gamut stream, decoded by gamut and by libjpeg-turbo. For gray/4:4:4 (no upsampling-filter
    // divergence) both decoders run the same integer IDCT, so the decodes are identical. For 4:2:0
    // and 4:4:0 the upsampling filters legitimately differ (gamut = replication, libjpeg = fancy/triangle), so
    // only a PSNR floor is asserted.
    let mut worst_tight = 0u8;
    let mut worst_sub_mse = 0f64;
    for &(w, h) in &[(16u32, 16u32), (33, 31), (64, 48)] {
        for mode in [Mode::Gray, Mode::C444, Mode::C420, Mode::C440] {
            let src = if matches!(mode, Mode::Gray) {
                gray_gradient(w, h)
            } else {
                rgb_gradient(w, h)
            };
            let jpeg = gamut_encode(mode, &src, w, h, 85, 0);
            let oracle = libjpeg_oracle::decode(&jpeg).expect("oracle decode");
            let (_, _, gamut) = gamut_decode(mode, &jpeg);
            let (max, mse) = diff_stats(&gamut, &oracle.pixels);
            if mode.upsampling_matches() {
                worst_tight = worst_tight.max(max);
            } else {
                worst_sub_mse = worst_sub_mse.max(mse);
            }
        }
    }
    // gray/444 decode-vs-decode: exact (max-diff 0; it was 2 under the f64 reference IDCT).
    // Subsampled PSNR measured 35.22 dB (4:2:0 16×16; 4:4:0's worst is 38.23 dB), bounded by the
    // replication-vs-fancy upsampling divergence, asserted with margin.
    assert_eq!(worst_tight, 0, "gray/444 parity max-diff {worst_tight}");
    assert!(
        psnr(worst_sub_mse) > 32.0,
        "subsampled parity PSNR {:.2} dB",
        psnr(worst_sub_mse)
    );
}

#[test]
fn progressive_encode_libjpeg_decodes_identically_to_baseline() {
    // The sharp progressive-encode gate: gamut's SOF2 stream and its baseline stream of the same
    // image carry identical quantized coefficients, so libjpeg-turbo — decoding both through the same
    // IDCT and upsampling — must produce EXACTLY the same pixels. This proves the progressive entropy
    // coder (DC/AC first pass, successive-approximation refinement, EOB runs, optimized Huffman
    // tables) delivers the baseline coefficients bit-for-bit. Asserted as exact equality across a
    // dims × mode × quality × restart battery; any deviation is a coder bug, not a tolerance.
    for &(w, h) in DIMS {
        for mode in Mode::ALL {
            for &q in &[40u8, 75, 95] {
                for &restart in &[0u16, 2] {
                    let src = lcg_pixels(w, h, mode.channels(), 0x1234_5678);
                    let baseline = gamut_encode_cfg(mode, &src, w, h, q, restart, false);
                    let progressive = gamut_encode_cfg(mode, &src, w, h, q, restart, true);
                    // gamut really emitted SOF2.
                    assert_eq!(
                        gamut_jpeg::info(&progressive).unwrap().process,
                        JpegProcess::Progressive,
                        "gamut must emit SOF2 for {mode:?} {w}x{h}"
                    );
                    let base_dec =
                        libjpeg_oracle::decode(&baseline).expect("oracle decode baseline");
                    let prog_dec =
                        libjpeg_oracle::decode(&progressive).expect("oracle decode progressive");
                    assert_eq!(
                        (prog_dec.width, prog_dec.height, prog_dec.channels),
                        (w, h, mode.channels()),
                        "geometry {mode:?} {w}x{h} q{q} r{restart}"
                    );
                    assert_eq!(
                        prog_dec.pixels, base_dec.pixels,
                        "progressive != baseline through libjpeg-turbo: {mode:?} {w}x{h} q{q} r{restart}"
                    );
                }
            }
        }
    }
}

#[test]
fn optimized_baseline_libjpeg_decodes_identically_to_the_fixed_tables() {
    // The sharp optimized-table gate, the baseline twin of the progressive one above: Annex K.2
    // tables change only the Huffman code words, never the quantized coefficients, so libjpeg-turbo
    // must decode the optimized stream to EXACTLY the same pixels as the fixed-table stream of the
    // same image. Any deviation is an entropy-coder bug — the gather pass and the emit pass having
    // disagreed — not a tolerance. Asserted over dims × mode × quality × restart, which also covers
    // the DC-predictor resets at restart boundaries and the sparse-histogram edge cases.
    let mut fixed_total = 0usize;
    let mut optimized_total = 0usize;
    for &(w, h) in DIMS {
        for mode in Mode::ALL {
            for &q in &[40u8, 75, 95] {
                for &restart in &[0u16, 2] {
                    let src = lcg_pixels(w, h, mode.channels(), 0x1234_5678);
                    let fixed = gamut_encode(mode, &src, w, h, q, restart);
                    let optimized = gamut_encode_optimized(mode, &src, w, h, q, restart);
                    // Still a baseline (SOF0) stream — the flag must not change the process.
                    assert_eq!(
                        gamut_jpeg::info(&optimized).unwrap().process,
                        JpegProcess::Baseline,
                        "optimized tables must stay baseline for {mode:?} {w}x{h}"
                    );
                    let fixed_dec = libjpeg_oracle::decode(&fixed).expect("oracle decode fixed");
                    let opt_dec =
                        libjpeg_oracle::decode(&optimized).expect("oracle decode optimized");
                    assert_eq!(
                        (opt_dec.width, opt_dec.height, opt_dec.channels),
                        (w, h, mode.channels()),
                        "geometry {mode:?} {w}x{h} q{q} r{restart}"
                    );
                    assert_eq!(
                        opt_dec.pixels, fixed_dec.pixels,
                        "optimized != fixed through libjpeg-turbo: {mode:?} {w}x{h} q{q} r{restart}"
                    );
                    // A Huffman code fitted to this image's own statistics is never longer than the
                    // generic Annex K one, and its table is never larger than the 162-value maximum
                    // the standard tables already spend.
                    assert!(
                        optimized.len() <= fixed.len(),
                        "optimized {} > fixed {} bytes: {mode:?} {w}x{h} q{q} r{restart}",
                        optimized.len(),
                        fixed.len()
                    );
                    fixed_total += fixed.len();
                    optimized_total += optimized.len();
                }
            }
        }
    }
    assert!(
        optimized_total < fixed_total,
        "optimized {optimized_total} bytes vs fixed {fixed_total} across the battery"
    );
}

#[test]
fn optimized_baseline_is_tight_against_libjpeg_turbo_optimize_coding() {
    // Against the reference doing the same job: libjpeg-turbo with `optimize_coding`. gamut builds
    // its tables from the same §K.2 procedure over the same symbol alphabet, so the stream sizes
    // must be in the same league — this catches a table that is technically valid but badly built
    // (e.g. a mis-ordered HUFFVAL) which the decode-equality check above would not notice.
    for &(w, h) in &[(33u32, 31u32), (64, 48)] {
        for mode in Mode::ALL {
            let src = gray_or_rgb_gradient(mode, w, h);
            let gamut = gamut_encode_optimized(mode, &src, w, h, 75, 0);
            let oracle = oracle_encode(mode, &src, w, h, 75, 0, true, false);
            // Measured worst ratio over this battery: 1.083 (gray 64×48). Asserted with margin.
            let ratio = gamut.len() as f64 / oracle.len() as f64;
            assert!(
                ratio < 1.15,
                "{mode:?} {w}x{h}: gamut {} vs libjpeg -optimize {} bytes (ratio {ratio:.3})",
                gamut.len(),
                oracle.len()
            );
        }
    }
}

/// The gradient content of the right channel count for `mode`.
fn gray_or_rgb_gradient(mode: Mode, w: u32, h: u32) -> Vec<u8> {
    if matches!(mode, Mode::Gray) {
        gray_gradient(w, h)
    } else {
        rgb_gradient(w, h)
    }
}

// ================================================================================================
// Decode direction: libjpeg-turbo encodes → gamut decodes.
// ================================================================================================

#[test]
fn decode_matches_libjpeg_turbo_own_decode() {
    // libjpeg-turbo encodes; gamut decodes and must match libjpeg-turbo's OWN decode of the same
    // stream (decode-vs-decode parity — no lossy encode error in the comparison). The `optimize`
    // cell exercises arbitrary (non-standard) DHT tables; the `restart` cell exercises RSTn resync.
    let mut worst_tight = 0u8;
    let mut worst_sub_mse = 0f64;
    for &(w, h) in &[(16u32, 16u32), (17, 9), (33, 31), (64, 48)] {
        for mode in Mode::ALL {
            for &q in &[40i32, 75, 95] {
                for &(restart, optimize) in &[(0u16, false), (2u16, true)] {
                    let src = lcg_pixels(w, h, mode.channels(), 0x1234_5678);
                    let jpeg = oracle_encode(mode, &src, w, h, q, restart, optimize, false);
                    let oracle = libjpeg_oracle::decode(&jpeg).expect("oracle decode");
                    assert_eq!(
                        (oracle.width, oracle.height, oracle.channels),
                        (w, h, mode.channels())
                    );
                    let (dw, dh, gamut) = gamut_decode(mode, &jpeg);
                    assert_eq!(
                        (dw, dh, gamut.len()),
                        (w, h, oracle.pixels.len()),
                        "geometry {mode:?} {w}x{h} q{q} r{restart} opt{optimize}"
                    );
                    let (max, mse) = diff_stats(&gamut, &oracle.pixels);
                    if mode.upsampling_matches() {
                        worst_tight = worst_tight.max(max);
                    } else {
                        worst_sub_mse = worst_sub_mse.max(mse);
                    }
                }
            }
        }
    }
    // Over q ∈ {40,75,95} on high-frequency LCG content: gray/444 decode parity is exact (max-diff
    // 0; 3 under the f64 reference IDCT); subsampled PSNR measured 22.91 dB (upsampling-filter
    // divergence, worst on random content), asserted with margin.
    assert_eq!(
        worst_tight, 0,
        "gray/444 decode-parity max-diff {worst_tight}"
    );
    assert!(
        psnr(worst_sub_mse) > 20.0,
        "subsampled decode-parity PSNR {:.2} dB",
        psnr(worst_sub_mse)
    );
}

#[test]
fn decode_progressive_matches_libjpeg_turbo_own_decode() {
    // libjpeg-turbo emits a PROGRESSIVE (SOF2) stream via its standard `jpeg_simple_progression`
    // 10-scan script (DC first + refinement, luma/chroma AC bands with successive approximation);
    // gamut's progressive decoder must reconstruct it and match libjpeg-turbo's OWN decode of the
    // same stream. Progressive and sequential coefficients are identical and both decoders run the
    // same integer IDCT, so gray/4:4:4 must match exactly and the only divergence left is the
    // upsampling-filter difference (subsampled) — asserted as in the sequential decode-parity test.
    let mut worst_tight = 0u8; // gray + 4:4:4: must stay 0
    let mut worst_sub_mse = 0f64; // subsampled: upsampling-filter divergence
    for &(w, h) in &[(16u32, 16u32), (17, 9), (33, 31), (64, 48)] {
        for mode in Mode::ALL {
            for &q in &[40i32, 75, 95] {
                for &(restart, optimize) in &[(0u16, false), (2u16, true)] {
                    let src = lcg_pixels(w, h, mode.channels(), 0x1234_5678);
                    let jpeg = oracle_encode(mode, &src, w, h, q, restart, optimize, true);
                    // Confirm the oracle really produced a progressive frame (SOF2 present).
                    assert_eq!(
                        gamut_jpeg::info(&jpeg).unwrap().process,
                        JpegProcess::Progressive,
                        "oracle must emit SOF2 for {mode:?} {w}x{h}"
                    );
                    let oracle = libjpeg_oracle::decode(&jpeg).expect("oracle decode");
                    assert_eq!(
                        (oracle.width, oracle.height, oracle.channels),
                        (w, h, mode.channels())
                    );
                    let (dw, dh, gamut) = gamut_decode(mode, &jpeg);
                    assert_eq!(
                        (dw, dh, gamut.len()),
                        (w, h, oracle.pixels.len()),
                        "geometry {mode:?} {w}x{h} q{q} r{restart} opt{optimize}"
                    );
                    let (max, mse) = diff_stats(&gamut, &oracle.pixels);
                    if mode.upsampling_matches() {
                        worst_tight = worst_tight.max(max);
                    } else {
                        worst_sub_mse = worst_sub_mse.max(mse);
                    }
                }
            }
        }
    }
    // Over q ∈ {40,75,95} on high-frequency LCG content, progressive streams: gray/444 decode
    // parity is exact (max-diff 0, as in the sequential battery — the coefficients match);
    // subsampled PSNR measured 22.9 dB (upsampling-filter divergence), asserted with the same
    // margin as the sequential decode-parity test.
    assert_eq!(
        worst_tight, 0,
        "progressive gray/444 decode-parity max-diff {worst_tight}"
    );
    assert!(
        psnr(worst_sub_mse) > 20.0,
        "progressive subsampled decode-parity PSNR {:.2} dB",
        psnr(worst_sub_mse)
    );
}

#[test]
fn progressive_byte_flip_sweep_never_panics() {
    // The no-panic-on-untrusted-input gate for the progressive decoder: flip a bit in every byte of
    // a real libjpeg-turbo progressive fixture (10-scan script, restart interval, subsampled) and
    // assert decode returns Ok or Err — never a panic — across the DC/AC first-pass, refinement,
    // EOBRUN and restart code paths. Also truncate at every length.
    let (w, h) = (24u32, 20u32);
    let src = lcg_pixels(w, h, 3, 0x0BAD_F00D);
    let base = oracle_encode(Mode::C420, &src, w, h, 60, 3, true, true);
    assert_eq!(
        gamut_jpeg::info(&base).unwrap().process,
        JpegProcess::Progressive
    );
    let dec = JpegDecoder::new();
    for i in (0..base.len()).step_by(3) {
        let mut m = base.clone();
        m[i] ^= 0xFF;
        let _ = <JpegDecoder as DecodeImage<Rgb8>>::decode_image(&dec, &m);
    }
    for cut in (0..base.len()).step_by(2) {
        let _ = <JpegDecoder as DecodeImage<Rgb8>>::decode_image(&dec, &base[..cut]);
    }
}

#[test]
fn oracle_is_libjpeg_turbo_3() {
    // Guards against accidental submodule drift to a different major version.
    let v = libjpeg_oracle::version();
    assert!(v.starts_with("3."), "expected libjpeg-turbo 3.x, got {v:?}");
}

// ================================================================================================
// Custom quantization tables (`with_quant_tables`) through the reference decoder.
// ================================================================================================

#[test]
fn encode_all_one_custom_tables_are_near_lossless() {
    // All-1 tables quantize with step 1 everywhere — the residual error is FDCT/IDCT rounding
    // only, so libjpeg-turbo's decode must sit within a few codes of the source. This pins that
    // the caller's tables really drive quantization (any scaled Annex K pair would be far coarser
    // at high frequencies and blow the bound on LCG content).
    use gamut_jpeg::QuantTables;
    let ones = QuantTables::new([1u8; 64], [1u8; 64]).unwrap();
    let mut worst = 0u8;
    for &(w, h) in DIMS {
        for mode in [Mode::Gray, Mode::C444] {
            let src = lcg_pixels(w, h, mode.channels(), 0xC0FF_EE00);
            let enc = JpegEncoder::new().with_quant_tables(ones);
            let jpeg = gamut_encode_with(enc, mode, &src, w, h);
            let img = libjpeg_oracle::decode(&jpeg).expect("libjpeg-turbo decode");
            assert_eq!(
                (img.width, img.height, img.channels),
                (w, h, mode.channels()),
                "geometry {mode:?} {w}x{h}"
            );
            let (max, _) = diff_stats(&img.pixels, &src);
            worst = worst.max(max);
        }
    }
    // Measured worst over the battery: 3 (gray = pure IDCT rounding; 4:4:4 adds the YCbCr
    // round-trip). Asserted with margin.
    assert!(worst <= 4, "all-1 tables max-diff {worst}");
}

#[test]
fn encode_coarse_custom_tables_decode_parity_gamut_vs_libjpeg() {
    // A deliberately coarse custom pair (Annex K scaled to quality 20): both decoders see the
    // same stream, so gamut's decode must match libjpeg-turbo's exactly, as the standard-table
    // battery does on gray/4:4:4.
    use gamut_jpeg::QuantTables;
    let coarse = QuantTables::annex_k().scaled(20);
    let mut worst = 0u8;
    for &(w, h) in DIMS {
        for mode in [Mode::Gray, Mode::C444] {
            let src = lcg_pixels(w, h, mode.channels(), 0xBAD_5EED);
            let enc = JpegEncoder::new().with_quant_tables(coarse);
            let jpeg = gamut_encode_with(enc, mode, &src, w, h);
            let reference = libjpeg_oracle::decode(&jpeg).expect("libjpeg-turbo decode");
            let (gw, gh, gamut_px) = gamut_decode(mode, &jpeg);
            assert_eq!((gw, gh), (w, h), "geometry {mode:?} {w}x{h}");
            let (max, _) = diff_stats(&gamut_px, &reference.pixels);
            worst = worst.max(max);
        }
    }
    // Exact: max-diff 0 over the battery (2 under the f64 reference IDCT).
    assert_eq!(worst, 0, "coarse custom tables parity max-diff {worst}");
}

// ================================================================================================
// Rate–distortion optimization (`with_rd_optimization`) through the reference decoder.
// ================================================================================================

/// Textured content: a smooth two-axis gradient plus low-amplitude LCG noise — many small,
/// near-threshold AC coefficients, the population trellis quantization exists to prune.
fn textured_pixels(w: u32, h: u32, channels: u32) -> Vec<u8> {
    let mut lcg = Lcg::new(0x7E97_0A1D);
    let mut px = Vec::with_capacity((w * h * channels) as usize);
    for y in 0..h {
        for x in 0..w {
            for c in 0..channels {
                let base = (x * 200 / w.max(1) + y * 55 / h.max(1) + c * 17) as i32;
                let noise = i32::from(lcg.byte() % 33) - 16;
                px.push((base + noise).clamp(0, 255) as u8);
            }
        }
    }
    px
}

/// Composite content: a flat left half and an LCG-textured right half — the flat/busy contrast
/// that per-block adaptive λ modulation exists for.
fn composite_pixels(w: u32, h: u32, channels: u32) -> Vec<u8> {
    let mut lcg = Lcg::new(0x5EED_CAFE);
    let mut px = Vec::with_capacity((w * h * channels) as usize);
    for _y in 0..h {
        for x in 0..w {
            for _c in 0..channels {
                px.push(if x < w / 2 { 128 } else { lcg.byte() });
            }
        }
    }
    px
}

/// Encodes with the given RD mode, no other configuration changes.
fn gamut_encode_rd(
    mode: Mode,
    pixels: &[u8],
    w: u32,
    h: u32,
    quality: u8,
    restart: u16,
    rd: gamut_jpeg::RdOptimization,
) -> Vec<u8> {
    let enc = JpegEncoder::new()
        .with_quality(quality)
        .with_restart_interval(restart)
        .with_rd_optimization(rd);
    gamut_encode_with(enc, mode, pixels, w, h)
}

#[test]
fn trellis_shrinks_streams_at_bounded_psnr_cost() {
    use gamut_jpeg::RdOptimization;
    // The RD gate: across dims × mode × quality × restart on gradient and composite content,
    // every trellis stream must (a) decode by libjpeg-turbo with the right geometry, (b) be no
    // larger than the plain stream, and (c) lose no more than 0.5 dB PSNR against the source.
    // Aggregated over the battery the size saving must clear a measured-then-margined floor.
    // Larger-than-DIMS canvases: the RD claims are about entropy data, which the marker
    // prologue would otherwise dominate; stream validity on tiny dims is the other batteries'
    // job. Content: smooth gradients (little to prune — the PSNR guard matters) and textured
    // (the near-threshold population trellis prunes — the size win matters).
    let mut plain_total = 0usize;
    let mut trellis_total = 0usize;
    for &(w, h) in &[(64u32, 48u32), (96, 80)] {
        for mode in [Mode::Gray, Mode::C444, Mode::C420] {
            for &q in &[50u8, 75, 90] {
                for &restart in &[0u16, 2] {
                    for content in 0..2 {
                        let src = if content == 0 {
                            if matches!(mode, Mode::Gray) {
                                gray_gradient(w, h)
                            } else {
                                rgb_gradient(w, h)
                            }
                        } else {
                            textured_pixels(w, h, mode.channels())
                        };
                        let plain = gamut_encode(mode, &src, w, h, q, restart);
                        let trellis =
                            gamut_encode_rd(mode, &src, w, h, q, restart, RdOptimization::Trellis);
                        // Modeled AC bits never exceed plain's (nearest rounding is in the
                        // search space), but byte padding and 0xFF stuffing can still wobble a
                        // couple of bytes on tiny streams.
                        assert!(
                            trellis.len() <= plain.len() + 2,
                            "{mode:?} {w}x{h} q{q} r{restart} c{content}: trellis grew the stream"
                        );
                        let p_img = libjpeg_oracle::decode(&plain).expect("decode plain");
                        let t_img = libjpeg_oracle::decode(&trellis).expect("decode trellis");
                        assert_eq!(
                            (t_img.width, t_img.height, t_img.channels),
                            (w, h, mode.channels()),
                            "geometry {mode:?} {w}x{h} q{q} r{restart}"
                        );
                        let (_, p_mse) = diff_stats(&p_img.pixels, &src);
                        let (_, t_mse) = diff_stats(&t_img.pixels, &src);
                        assert!(
                            psnr(t_mse) >= psnr(p_mse) - 0.5,
                            "{mode:?} {w}x{h} q{q} r{restart} c{content}: PSNR {:.2} -> {:.2}",
                            psnr(p_mse),
                            psnr(t_mse)
                        );
                        plain_total += plain.len();
                        trellis_total += trellis.len();
                    }
                }
            }
        }
    }
    // Measured at λ = 144: 8.70% of the battery's total bytes saved; worst cell −0.35 dB.
    // Asserted with margin.
    let saved = plain_total - trellis_total;
    assert!(
        saved * 100 >= plain_total * 5,
        "trellis saved only {saved} of {plain_total} bytes"
    );
}

#[test]
fn adaptive_trellis_redistributes_bits_by_block_activity() {
    use gamut_jpeg::RdOptimization;
    // The modulation direction, both ways. Activity is measured relative to the quantization
    // resolution (√(energy / Σ step²)), so "busy" means busy *at this quality*: at q90+ the steps
    // are fine and LCG noise sits far above them — λ scales UP and the adaptive stream must be
    // smaller in aggregate at a bounded PSNR cost. On smooth gradients λ scales DOWN, so the
    // adaptive stream keeps at least trellis fidelity (and may spend a few more bytes doing it).
    let mut busy_trellis = 0usize;
    let mut busy_adaptive = 0usize;
    for &(w, h) in &[(33u32, 31u32), (64, 48)] {
        for mode in [Mode::Gray, Mode::C444] {
            for &q in &[90u8, 95] {
                let src = lcg_pixels(w, h, mode.channels(), 0xADA9_71FE);
                let trellis = gamut_encode_rd(mode, &src, w, h, q, 0, RdOptimization::Trellis);
                let adaptive =
                    gamut_encode_rd(mode, &src, w, h, q, 0, RdOptimization::TrellisAdaptive);
                let t_img = libjpeg_oracle::decode(&trellis).expect("decode trellis");
                let a_img = libjpeg_oracle::decode(&adaptive).expect("decode adaptive");
                let (_, t_mse) = diff_stats(&t_img.pixels, &src);
                let (_, a_mse) = diff_stats(&a_img.pixels, &src);
                assert!(
                    psnr(a_mse) >= psnr(t_mse) - 1.5,
                    "{mode:?} {w}x{h} q{q}: busy adaptive PSNR {:.2} vs trellis {:.2}",
                    psnr(a_mse),
                    psnr(t_mse)
                );
                busy_trellis += trellis.len();
                busy_adaptive += adaptive.len();
            }
        }
    }
    // Uniform noise is adaptive Q's thinnest showcase — most coefficients sit far above the
    // marginal band even at 4λ, so the aggregate effect is small; the point of this gate is the
    // *direction*. Measured: 0.67% saved (28836 vs 29031). Floor margined to 0.3%.
    assert!(
        busy_adaptive * 1000 <= busy_trellis * 997,
        "busy content: adaptive {busy_adaptive} vs trellis {busy_trellis}: no saving"
    );

    // Smooth content: λ is modulated down, so adaptive fidelity must not fall below trellis's.
    for &(w, h) in &[(33u32, 31u32), (64, 48)] {
        let src = gray_gradient(w, h);
        let trellis = gamut_encode_rd(Mode::Gray, &src, w, h, 75, 0, RdOptimization::Trellis);
        let adaptive = gamut_encode_rd(
            Mode::Gray,
            &src,
            w,
            h,
            75,
            0,
            RdOptimization::TrellisAdaptive,
        );
        let (_, t_mse) = diff_stats(&libjpeg_oracle::decode(&trellis).unwrap().pixels, &src);
        let (_, a_mse) = diff_stats(&libjpeg_oracle::decode(&adaptive).unwrap().pixels, &src);
        // Redistribution is fidelity-neutral on smooth content (measured worst: −0.06 dB at
        // 64×48; the per-block factor sits near 1 either side of the activity reference). The
        // strict flat-block protection direction is unit-pinned in `rd::tests`.
        assert!(
            psnr(a_mse) >= psnr(t_mse) - 0.15,
            "{w}x{h}: smooth adaptive PSNR {:.2} fell below trellis {:.2}",
            psnr(a_mse),
            psnr(t_mse)
        );
    }
}

#[test]
fn trellis_composes_with_custom_tables_and_progressive() {
    use gamut_jpeg::{QuantTables, RdOptimization};
    // Composition cells: (a) trellis over caller-supplied tables — the stream must stay
    // libjpeg-decodable with sane fidelity; (b) trellis progressive vs trellis baseline — the
    // same coefficients, so libjpeg's decodes must be byte-identical (the exactness invariant).
    let (w, h) = (33u32, 31u32);
    let src = composite_pixels(w, h, 3);
    let tables = QuantTables::annex_k().scaled(60);
    let enc = JpegEncoder::new()
        .with_quant_tables(tables)
        .with_rd_optimization(RdOptimization::Trellis);
    let custom = gamut_encode_with(enc, Mode::C444, &src, w, h);
    let img = libjpeg_oracle::decode(&custom).expect("decode custom+trellis");
    let (_, mse) = diff_stats(&img.pixels, &src);
    // Measured: 20.96 dB on this half-noise composite at ~q60. A loose sanity floor proving the
    // pipeline composes, not a tuning pin.
    assert!(psnr(mse) > 19.0, "custom+trellis PSNR {:.2}", psnr(mse));

    for rd in [RdOptimization::Trellis, RdOptimization::TrellisAdaptive] {
        let base_enc = JpegEncoder::new().with_quality(75).with_rd_optimization(rd);
        let prog_enc = JpegEncoder::new()
            .with_quality(75)
            .with_rd_optimization(rd)
            .with_progressive(true);
        let base = gamut_encode_with(base_enc, Mode::C444, &src, w, h);
        let prog = gamut_encode_with(prog_enc, Mode::C444, &src, w, h);
        let base_px = libjpeg_oracle::decode(&base)
            .expect("decode baseline")
            .pixels;
        let prog_px = libjpeg_oracle::decode(&prog)
            .expect("decode progressive")
            .pixels;
        assert_eq!(
            base_px, prog_px,
            "{rd:?}: progressive coefficients diverged from baseline"
        );
    }
}
