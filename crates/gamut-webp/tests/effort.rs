//! The compression-effort ladder's contract (issue #261), across every rung `0..=6`.
//!
//! Three properties, in decreasing order of importance:
//!
//! 1. **Correctness is effort-independent.** A lossless encode reproduces its input bit-exactly at
//!    every rung; effort only chooses how hard the encoder searches.
//! 2. **Size is non-increasing in effort.** The VP8L ladder guarantees this *by construction* — a
//!    rung's candidate plans are a superset of the rung below's, and ties resolve to the earlier
//!    plan — so it is asserted exactly, with no tolerance.
//! 3. **Encoding is deterministic.** Two encodes at the same rung are byte-identical, which is what
//!    catches floating point or hash-iteration order leaking into an encoder decision.
//!
//! Fixtures stay small on purpose: the top rungs are the slowest code in the crate and this suite
//! runs inside the coverage and mutation-testing lanes.

use gamut_core::{DecodeImage, Dimensions, EncodeImage, ImageBuf, ImageRef, Rgb8, Rgba8};
use gamut_webp::{Effort, WebpDecoder, WebpEncoder};

/// Every rung of the ladder, lowest first.
fn all_efforts() -> Vec<Effort> {
    (0..=6)
        .map(|l| Effort::from_level(l).expect("0..=6"))
        .collect()
}

fn dims(width: u32, height: u32) -> Dimensions {
    Dimensions::new(width, height).unwrap()
}

/// A smooth ramp — compressible, and the case the spatial transforms are for.
fn ramp_rgba(w: u32, h: u32) -> Vec<u8> {
    (0..w * h)
        .flat_map(|i| {
            let (x, y) = (i % w, i / w);
            [
                (x * 3) as u8,
                (y * 5) as u8,
                (x + y) as u8,
                0xff_u8.saturating_sub((x / 4) as u8),
            ]
        })
        .collect()
}

/// A handful of distinct colours — the palette path.
fn palette_rgba(w: u32, h: u32) -> Vec<u8> {
    const COLOURS: [[u8; 4]; 5] = [
        [0, 0, 0, 255],
        [255, 0, 0, 255],
        [0, 255, 0, 255],
        [0, 0, 255, 128],
        [255, 255, 255, 255],
    ];
    (0..w * h)
        .flat_map(|i| COLOURS[(i as usize * 7 / 3) % COLOURS.len()])
        .collect()
}

/// High-entropy content: nothing should compress it much, so it exercises the paths that give up.
fn noisy_rgba(w: u32, h: u32) -> Vec<u8> {
    let mut state = 0x1234_5678u32;
    (0..w * h * 4)
        .map(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (state >> 24) as u8
        })
        .collect()
}

/// A single flat colour — the degenerate case where every alphabet is single-symbol.
fn flat_rgba(w: u32, h: u32) -> Vec<u8> {
    [0x20u8, 0x40, 0x60, 0xff].repeat((w * h) as usize)
}

/// The fixture corpus as `(label, pixels, dimensions)`.
fn corpus() -> Vec<(&'static str, Vec<u8>, Dimensions)> {
    vec![
        ("ramp", ramp_rgba(64, 48), dims(64, 48)),
        ("palette", palette_rgba(48, 32), dims(48, 32)),
        ("noisy", noisy_rgba(32, 24), dims(32, 24)),
        ("flat", flat_rgba(40, 24), dims(40, 24)),
        ("single-pixel", flat_rgba(1, 1), dims(1, 1)),
        ("one-row", ramp_rgba(37, 1), dims(37, 1)),
        ("one-column", ramp_rgba(1, 29), dims(1, 29)),
    ]
}

fn encode_lossless(effort: Effort, px: &[u8], d: Dimensions) -> Vec<u8> {
    let mut out = Vec::new();
    WebpEncoder::lossless()
        .with_effort(effort)
        .encode_image(ImageRef::<Rgba8>::new(px, d).expect("fixture"), &mut out)
        .expect("lossless encode");
    out
}

fn encode_lossy(effort: Effort, quality: u8, px: &[u8], d: Dimensions) -> Vec<u8> {
    let mut out = Vec::new();
    WebpEncoder::lossy(quality)
        .with_effort(effort)
        .encode_image(ImageRef::<Rgba8>::new(px, d).expect("fixture"), &mut out)
        .expect("lossy encode");
    out
}

#[test]
fn every_effort_level_round_trips_losslessly() {
    // The guarantee that must never depend on effort: lossless is bit-exact at every rung, for
    // every shape, including the degenerate 1xN / Nx1 / single-pixel cases.
    for (label, px, d) in corpus() {
        for effort in all_efforts() {
            let file = encode_lossless(effort, &px, d);
            let decoded: ImageBuf<Rgba8> =
                WebpDecoder::new().decode_image(&file).unwrap_or_else(|e| {
                    panic!("{label} at effort {}: decode failed: {e}", effort.level())
                });
            assert_eq!(
                decoded.dimensions(),
                d,
                "{label} at effort {}",
                effort.level()
            );
            assert_eq!(
                decoded.as_samples(),
                px.as_slice(),
                "{label} at effort {} is not bit-exact",
                effort.level()
            );
        }
    }
}

#[test]
fn lossless_size_is_non_increasing_in_effort() {
    // The VP8L ladder's central invariant, guaranteed by construction: each rung's candidate plans
    // extend the rung below's, and ties resolve to the earlier plan. Asserted exactly — if anyone
    // turns the ladder into a "different set per rung" table, this fails.
    for (label, px, d) in corpus() {
        let sizes: Vec<usize> = all_efforts()
            .into_iter()
            .map(|e| encode_lossless(e, &px, d).len())
            .collect();
        for level in 1..sizes.len() {
            assert!(
                sizes[level] <= sizes[level - 1],
                "{label}: effort {level} grew to {} from {} at effort {}",
                sizes[level],
                sizes[level - 1],
                level - 1
            );
        }
    }
}

#[test]
fn encoding_is_deterministic_at_every_effort() {
    // Any floating point or hash-iteration order reaching an encoder decision would show up here
    // as a byte difference between two runs of the same configuration.
    for (label, px, d) in corpus() {
        for effort in all_efforts() {
            assert_eq!(
                encode_lossless(effort, &px, d),
                encode_lossless(effort, &px, d),
                "{label}: lossless effort {} is not deterministic",
                effort.level()
            );
            assert_eq!(
                encode_lossy(effort, 70, &px, d),
                encode_lossy(effort, 70, &px, d),
                "{label}: lossy effort {} is not deterministic",
                effort.level()
            );
        }
    }
}

#[test]
fn every_effort_level_produces_a_decodable_lossy_file() {
    // Lossy output is not bit-exact, so the contract is weaker: every rung must still produce a
    // file the decoder reads back at the right shape, with the alpha plane exact (alpha is stored
    // losslessly at every rung — effort never makes it lossy).
    for (label, px, d) in corpus() {
        for effort in all_efforts() {
            let file = encode_lossy(effort, 70, &px, d);
            let decoded: ImageBuf<Rgba8> =
                WebpDecoder::new().decode_image(&file).unwrap_or_else(|e| {
                    panic!("{label} at effort {}: decode failed: {e}", effort.level())
                });
            assert_eq!(
                decoded.dimensions(),
                d,
                "{label} at effort {}",
                effort.level()
            );
            let got: Vec<u8> = decoded
                .as_samples()
                .as_chunks::<4>()
                .0
                .iter()
                .map(|p| p[3])
                .collect();
            let want: Vec<u8> = px.as_chunks::<4>().0.iter().map(|p| p[3]).collect();
            assert_eq!(
                got,
                want,
                "{label} at effort {}: alpha must stay lossless",
                effort.level()
            );
        }
    }
}

#[test]
fn effort_does_not_disturb_the_rgb_surface() {
    // The `Rgb8` path shares the codestream encoders with `Rgba8` but not the container decisions,
    // so pin that it too round-trips at both extremes of the ladder.
    let rgb: Vec<u8> = (0..32u32 * 24)
        .flat_map(|i| [(i % 251) as u8, (i % 37) as u8, (i % 199) as u8])
        .collect();
    let d = dims(32, 24);
    for effort in [Effort::Fastest, Effort::Slowest] {
        let mut out = Vec::new();
        WebpEncoder::lossless()
            .with_effort(effort)
            .encode_image(ImageRef::<Rgb8>::new(&rgb, d).expect("fixture"), &mut out)
            .expect("encode");
        let decoded: ImageBuf<Rgb8> = WebpDecoder::new().decode_image(&out).expect("decode");
        assert_eq!(decoded.as_samples(), rgb.as_slice());
    }
}

/// Encodes losslessly with near-lossless preprocessing at `strength`.
fn encode_near_lossless(strength: u8, px: &[u8], d: Dimensions) -> Vec<u8> {
    let mut out = Vec::new();
    WebpEncoder::lossless()
        .with_near_lossless(Some(
            gamut_webp::NearLossless::new(strength).expect("0..=99"),
        ))
        .encode_image(ImageRef::<Rgba8>::new(px, d).expect("fixture"), &mut out)
        .expect("near-lossless encode");
    out
}

#[test]
fn near_lossless_off_is_byte_identical_to_plain_lossless() {
    // `None` must be a true no-op, not merely "close enough" — otherwise every existing caller
    // silently changes output the day the knob lands.
    for (label, px, d) in corpus() {
        let mut with_none = Vec::new();
        WebpEncoder::lossless()
            .with_near_lossless(None)
            .encode_image(
                ImageRef::<Rgba8>::new(&px, d).expect("fixture"),
                &mut with_none,
            )
            .expect("encode");
        assert_eq!(
            with_none,
            encode_lossless(Effort::default(), &px, d),
            "{label}: near-lossless None must be byte-identical"
        );
    }
}

#[test]
fn near_lossless_keeps_rgb_within_the_bound_and_alpha_exact() {
    // The contract callers rely on. The stream itself is still bit-exact lossless — it just codes
    // a quantized image — so what is checked is the distance from the *original*.
    for (label, px, d) in corpus() {
        for strength in [0u8, 40, 60, 99] {
            let bound = gamut_webp::NearLossless::new(strength)
                .expect("0..=99")
                .max_deviation();
            let file = encode_near_lossless(strength, &px, d);
            let decoded: ImageBuf<Rgba8> = WebpDecoder::new().decode_image(&file).expect("decode");
            assert_eq!(decoded.dimensions(), d, "{label} at strength {strength}");
            for (i, (before, after)) in px
                .as_chunks::<4>()
                .0
                .iter()
                .zip(decoded.as_samples().as_chunks::<4>().0)
                .enumerate()
            {
                assert_eq!(
                    before[3], after[3],
                    "{label} at strength {strength}: alpha moved at pixel {i}"
                );
                for c in 0..3 {
                    assert!(
                        u16::from(before[c].abs_diff(after[c])) <= bound,
                        "{label} at strength {strength}: channel {c} moved {} at pixel {i}, bound {bound}",
                        before[c].abs_diff(after[c])
                    );
                }
            }
        }
    }
}

#[test]
fn near_lossless_never_inflates_and_pays_at_strength() {
    // Two things a caller needs to be able to rely on.
    //
    // First, turning the knob on can never make a file *bigger*: quantization is not
    // unconditionally a win, so the encoder codes the image both ways and keeps the smaller. That
    // guard is what makes the knob safe to set without measuring.
    //
    // Second, it has to actually do something at strength, or it is plumbing rather than a feature.
    // The fixture is a gradient carrying low-amplitude noise — photographic content, and the case
    // the technique is for. A pure ramp would not do: the spatial predictor already drives it to
    // all-zero residuals, so there are no low bits left to discard.
    let (w, h) = (96u32, 72u32);
    let mut state = 0x9e37_79b9u32;
    let px: Vec<u8> = (0..w * h)
        .flat_map(|i| {
            let (x, y) = (i % w, i / w);
            let base = [(x * 2) as u8, (y * 2) as u8, (x + y) as u8];
            let mut out = [0u8; 4];
            for (c, slot) in base.iter().zip(out.iter_mut()) {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                *slot = c.wrapping_add(((state >> 28) as u8) & 0x0f);
            }
            out[3] = 0xff;
            out
        })
        .collect();
    let d = dims(w, h);
    let exact = encode_lossless(Effort::default(), &px, d).len();
    for strength in [0u8, 20, 40, 60, 80, 99] {
        let size = encode_near_lossless(strength, &px, d).len();
        assert!(
            size <= exact,
            "strength {strength} inflated the file: {size} vs {exact} exact"
        );
    }
    let aggressive = encode_near_lossless(0, &px, d).len();
    assert!(
        aggressive < exact * 4 / 5,
        "the strongest setting saved only {} of {exact} bytes",
        exact - aggressive
    );
}

#[test]
fn near_lossless_is_ignored_by_the_lossy_path() {
    // Near-lossless is a VP8L preprocessing step. The lossy path documents that it ignores the
    // knob, so pin that rather than leaving it to the reader's assumption.
    let (w, h) = (32u32, 24u32);
    let px = ramp_rgba(w, h);
    let d = dims(w, h);
    let mut with_nl = Vec::new();
    WebpEncoder::lossy(70)
        .with_near_lossless(Some(gamut_webp::NearLossless::new(0).expect("0..=99")))
        .encode_image(
            ImageRef::<Rgba8>::new(&px, d).expect("fixture"),
            &mut with_nl,
        )
        .expect("encode");
    assert_eq!(with_nl, encode_lossy(Effort::default(), 70, &px, d));
}

/// Peak signal-to-noise ratio, in dB, between two RGBA buffers over the colour channels.
fn psnr_rgb(a: &[u8], b: &[u8]) -> f64 {
    let mut sse = 0f64;
    let mut n = 0f64;
    for (x, y) in a.as_chunks::<4>().0.iter().zip(b.as_chunks::<4>().0) {
        for c in 0..3 {
            let d = f64::from(x[c]) - f64::from(y[c]);
            sse += d * d;
            n += 1.0;
        }
    }
    if sse == 0.0 {
        return f64::INFINITY;
    }
    10.0 * (255.0 * 255.0 * n / sse).log10()
}

fn decode_rgba(file: &[u8]) -> ImageBuf<Rgba8> {
    WebpDecoder::new().decode_image(file).expect("decode")
}

#[test]
fn lossy_rungs_above_the_anchor_never_cost_size_or_quality() {
    // Lossy size is not monotone across the *whole* ladder, and asserting that it were would be
    // wrong: the fastest rungs drop `B_PRED` entirely, which removes mode bits and can make output
    // *smaller* at lower quality. What must hold is the useful half — every rung above the
    // historical anchor (level 2) is at least as good on both axes, because those rungs only add
    // entropy-coding and quantizer work.
    for (label, px, d) in corpus() {
        for quality in [40u8, 75] {
            let anchor = encode_lossy(Effort::Fast, quality, &px, d);
            let anchor_psnr = psnr_rgb(&px, decode_rgba(&anchor).as_samples());
            for level in 3..=6u8 {
                let effort = Effort::from_level(level).expect("in range");
                let file = encode_lossy(effort, quality, &px, d);
                assert!(
                    file.len() <= anchor.len(),
                    "{label} q{quality}: effort {level} grew to {} from {} at effort 2",
                    file.len(),
                    anchor.len()
                );
                let psnr = psnr_rgb(&px, decode_rgba(&file).as_samples());
                assert!(
                    psnr >= anchor_psnr - 1.0,
                    "{label} q{quality}: effort {level} lost {:.2} dB against effort 2",
                    anchor_psnr - psnr
                );
            }
        }
    }
}

#[test]
fn optimizing_coefficient_probabilities_costs_no_quality() {
    // The property that makes the two-pass entropy work provably safe: probabilities change how
    // many bits the tokens take, never what those tokens decode to. Effort 2 and 3 differ *only*
    // in the probability and skip-probability derivation, so their reconstructions must be
    // identical pixel for pixel while effort 3's file is no larger.
    for (label, px, d) in corpus() {
        for quality in [40u8, 75] {
            let default_probs = encode_lossy(Effort::Fast, quality, &px, d);
            let optimized = encode_lossy(Effort::Moderate, quality, &px, d);
            assert_eq!(
                decode_rgba(&optimized).as_samples(),
                decode_rgba(&default_probs).as_samples(),
                "{label} q{quality}: optimizing probabilities changed the decoded pixels"
            );
            assert!(
                optimized.len() <= default_probs.len(),
                "{label} q{quality}: optimizing probabilities grew the file, {} vs {}",
                optimized.len(),
                default_probs.len()
            );
        }
    }
}

/// FNV-1a (64-bit), the digest `tests/default_bytes.rs` pins its fixtures with.
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// The gradient fixtures `tests/default_bytes.rs` pins the default rung with, so the numbers here
/// and there describe the same pixels.
fn gradient_rgb(w: u32, h: u32) -> Vec<u8> {
    (0..w * h)
        .flat_map(|i| {
            let (x, y) = (i % w, i / w);
            [(x * 7) as u8, (y * 11) as u8, (x ^ y) as u8]
        })
        .collect()
}

fn gradient_rgba(w: u32, h: u32) -> Vec<u8> {
    (0..w * h)
        .flat_map(|i| {
            let (x, y) = (i % w, i / w);
            [
                (x * 7) as u8,
                (y * 11) as u8,
                (x ^ y) as u8,
                ((x * 5 + y * 3) & 0xff) as u8,
            ]
        })
        .collect()
}

/// A gradient carrying deterministic high-frequency noise, at a size where the two-pass VP8
/// path has something to measure: the 32x24 gradients above produce too few coefficient tokens
/// for any context to pay for a probability-update record, so the whole derivation collapses to
/// the defaults and its inputs stop being observable. At 96x80 the updates are adopted, and the
/// rung-2 to rung-3 step drops the file by an eighth instead of two bytes.
fn busy(w: u32, h: u32, channels: usize) -> Vec<u8> {
    let mut state: u32 = 0x9E37_79B9;
    (0..w * h)
        .flat_map(|i| {
            let (x, y) = (i % w, i / w);
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let n = (state >> 27) as u8;
            let px = [
                ((x * 3) as u8).wrapping_add(n),
                ((y * 5) as u8).wrapping_add(n >> 1),
                ((x ^ y) as u8).wrapping_add(n << 1),
                ((x + y * 2) & 0xff) as u8,
            ];
            px[..channels].to_vec()
        })
        .collect()
}

/// Alternating 16-pixel bands of flat colour and noise. Uniform detail hides two decisions: a
/// frame that is busy everywhere makes every macroblock take `B_PRED`, so the whole-block `Y2`
/// path is never coded and the effort-1 `B_PRED` gate never sees a macroblock its threshold
/// could exclude. Half the blocks here predict perfectly and half not at all.
fn patchwork(w: u32, h: u32, channels: usize) -> Vec<u8> {
    let mut state: u32 = 0x1234_5678;
    (0..w * h)
        .flat_map(|i| {
            let (x, y) = (i % w, i / w);
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let flat = ((x / 16) + (y / 16)) % 2 == 0;
            let n = if flat { 0 } else { (state >> 26) as u8 };
            let px = [
                0x40_u8.wrapping_add(n),
                0x80_u8.wrapping_add(n << 1),
                0xC0_u8.wrapping_add(n >> 1),
                if flat { 0xff } else { 0x80_u8.wrapping_add(n) },
            ];
            px[..channels].to_vec()
        })
        .collect()
}

/// A smooth ramp, coded at high quality. The busy and patchwork fixtures are both extremes: their
/// macroblocks either take `B_PRED` or quantize away to a skip, and both of those bypass the
/// whole-block `Y2` residual entirely. A gentle gradient at a fine quantizer is the case in
/// between — whole-block prediction wins, and the residual still survives quantization — which is
/// also where the effort-1 `B_PRED` gate meets prediction errors near its threshold rather than
/// far on either side of it.
fn smooth_rgb(w: u32, h: u32) -> Vec<u8> {
    (0..w * h)
        .flat_map(|i| {
            let (x, y) = (i % w, i / w);
            [(40 + x) as u8, (60 + y) as u8, (80 + (x + y) / 2) as u8]
        })
        .collect()
}

/// A tiled motif with slow drift, in a few hundred colours. The other fixtures are all either
/// unique-per-pixel or a handful of flat colours, and neither shape lets the VP8L ladder's
/// *parse* plans win: long backward matches only exist when content repeats, lazy matching only
/// pays when the better match starts one pixel later than the greedy one would take, and the
/// colour cache's size only matters in the range between "everything fits" and "nothing repeats".
fn tiled_rgba(w: u32, h: u32) -> Vec<u8> {
    (0..w * h)
        .flat_map(|i| {
            let (x, y) = (i % w, i / w);
            let motif = (x % 13) * 7 + (y % 11) * 3;
            let drift = (x / 13 + y / 11) % 5;
            let v = (motif + drift * 17) as u8;
            [v, v.wrapping_mul(3), v.wrapping_add(90), 0xff]
        })
        .collect()
}

/// Every rung's output, pinned by length + digest — the density contract made concrete.
///
/// `tests/default_bytes.rs` pins the *default* rung. That leaves the other six describing
/// themselves only by inequalities (round-trips losslessly, is no larger than the rung below), and
/// inequalities are satisfied by a great deal of code that is nonetheless wrong: a mis-derived
/// coefficient probability, a token counter threading the wrong neighbour context, a dead zone
/// applied to the wrong coefficient, a VP8L plan racer keeping the wrong candidate, an `ALPH`
/// pre-filter written into the wrong header bits — each of those still round-trips and still
/// shrinks, and each moves a number in this table.
///
/// Re-pinning is expected whenever a rung's search genuinely changes, and the commit that does it
/// should say what moved and why (the same rule `default_bytes.rs` states). The lossy column is
/// deliberately **not** monotone at rung 0→1: rung 0 emits no `B_PRED` modes at all, so it spends
/// fewer header bits on a worse prediction. Only the VP8L ladder claims monotonicity.
#[test]
fn every_effort_rung_pins_its_output_bytes() {
    let d = dims(32, 24);
    let rgb = gradient_rgb(32, 24);
    let rgba = gradient_rgba(32, 24);

    // Indexed by effort level 0..=6.
    const LOSSLESS_RGB: [(usize, u64); 7] = [
        (354, 0x73f6_5a70_5f83_090f),
        (314, 0x78f8_05cc_0245_d9ff),
        (244, 0xffe4_2036_c125_47dc),
        (240, 0x84c5_1f6b_3460_7770),
        (240, 0x84c5_1f6b_3460_7770),
        (240, 0x84c5_1f6b_3460_7770),
        (224, 0xcc01_b0ff_f924_fc02),
    ];
    const LOSSLESS_RGBA: [(usize, u64); 7] = [
        (348, 0x8c73_bdb3_57b6_6710),
        (324, 0x318e_a962_4829_8dab),
        (252, 0xaebd_b52e_45a9_f819),
        (246, 0x6f7c_d692_613c_775e),
        (246, 0x6f7c_d692_613c_775e),
        (246, 0x6f7c_d692_613c_775e),
        (216, 0xb770_b826_a8e4_1754),
    ];
    const LOSSY60_RGB: [(usize, u64); 7] = [
        (122, 0x9ef8_1c74_ea19_d16c),
        (136, 0x2eb3_c30d_36b6_067e),
        (136, 0x2eb3_c30d_36b6_067e),
        (134, 0xe796_fd02_7a9b_65e5),
        (134, 0x414b_d885_3a94_d316),
        (134, 0x414b_d885_3a94_d316),
        (134, 0x414b_d885_3a94_d316),
    ];
    const LOSSY60_RGBA: [(usize, u64); 7] = [
        (172, 0x3b9c_e9fc_45c8_e0ba),
        (186, 0x4e86_861b_b8de_dbb4),
        (186, 0x4e86_861b_b8de_dbb4),
        (184, 0xd5b5_d2a9_f740_8bc7),
        (184, 0xe2df_69e2_2c62_ce64),
        (184, 0xe2df_69e2_2c62_ce64),
        (184, 0xe2df_69e2_2c62_ce64),
    ];

    // The busy fixture is the one the two-pass VP8 derivation is actually visible in (see `busy`).
    let bd = dims(96, 80);
    let brgb = busy(96, 80, 3);
    let brgba = busy(96, 80, 4);
    const BUSY_LOSSLESS_RGB: [(usize, u64); 7] = [
        (14372, 0x0699_b1a7_3d00_db45),
        (14372, 0x0699_b1a7_3d00_db45),
        (14372, 0x0699_b1a7_3d00_db45),
        (14242, 0x56f2_1de9_92fd_9c49),
        (13434, 0xc295_20d5_69cc_56e6),
        (13434, 0xc295_20d5_69cc_56e6),
        (13280, 0xff8c_c0b5_1326_359e),
    ];
    const BUSY_LOSSLESS_RGBA: [(usize, u64); 7] = [
        (14690, 0x0c41_f16b_2ac5_700b),
        (14536, 0x87cc_4cc4_4171_3c60),
        (14536, 0x87cc_4cc4_4171_3c60),
        (14452, 0xb2b7_d34b_b003_56c8),
        (13992, 0x49a2_7f5d_23b1_5e29),
        (13202, 0x9f43_8822_8cf0_0d09),
        (13202, 0x9f43_8822_8cf0_0d09),
    ];
    const BUSY_LOSSY60_RGB: [(usize, u64); 7] = [
        (1898, 0x3b9b_365a_f0da_ec79),
        (1794, 0x662a_15bc_cf1e_5f8b),
        (1794, 0x662a_15bc_cf1e_5f8b),
        (1570, 0x9744_c62e_e66f_299e),
        (1452, 0x1af0_7556_ce3c_4e47),
        (1452, 0x1af0_7556_ce3c_4e47),
        (1452, 0x1af0_7556_ce3c_4e47),
    ];
    const BUSY_LOSSY60_RGBA: [(usize, u64); 7] = [
        (2006, 0x1b05_1ab2_78fa_182d),
        (1902, 0x7723_400f_6c97_eb68),
        (1902, 0x7723_400f_6c97_eb68),
        (1678, 0x2854_d6a4_7516_7592),
        (1560, 0xee54_68aa_8ddc_632e),
        (1560, 0xee54_68aa_8ddc_632e),
        (1560, 0xee54_68aa_8ddc_632e),
    ];

    let prgb = patchwork(96, 80, 3);
    let prgba = patchwork(96, 80, 4);
    const PATCH_LOSSLESS_RGB: [(usize, u64); 7] = [
        (4860, 0x7bdb_f2c2_5cdb_1bfe),
        (4860, 0x7bdb_f2c2_5cdb_1bfe),
        (4548, 0x76aa_bae6_f922_51a6),
        (4454, 0x3d81_9a50_8c47_ff83),
        (3742, 0x35f0_63db_581e_8103),
        (3564, 0x5d56_8fa9_b31f_a5b0),
        (3564, 0x5d56_8fa9_b31f_a5b0),
    ];
    const PATCH_LOSSLESS_RGBA: [(usize, u64); 7] = [
        (4916, 0xafed_9682_0caa_2947),
        (4916, 0xafed_9682_0caa_2947),
        (4564, 0x743b_e192_421e_5da8),
        (4510, 0x9ff1_623d_9845_7fb3),
        (3806, 0x0e2e_4db8_4d3f_c7fd),
        (3586, 0x73ff_3012_b8f4_14ae),
        (3586, 0x73ff_3012_b8f4_14ae),
    ];
    const PATCH_LOSSY60_RGB: [(usize, u64); 7] = [
        (2328, 0x694d_3467_b970_2251),
        (1832, 0xafb1_b617_367a_ce08),
        (1832, 0xafb1_b617_367a_ce08),
        (1530, 0x9f44_352a_1325_6914),
        (1500, 0x7eca_6871_1963_eae7),
        (1500, 0x7eca_6871_1963_eae7),
        (1500, 0x7eca_6871_1963_eae7),
    ];
    const PATCH_LOSSY60_RGBA: [(usize, u64); 7] = [
        (5930, 0x0a06_79c4_cf67_fa1b),
        (5434, 0x9d9d_2ab5_bf81_63a6),
        (5434, 0x9d9d_2ab5_bf81_63a6),
        (5132, 0x617f_8f42_96ac_c10f),
        (5102, 0x01e0_f3cc_764b_692d),
        (5102, 0x01e0_f3cc_764b_692d),
        (5102, 0x01e0_f3cc_764b_692d),
    ];

    let srgb = smooth_rgb(96, 80);
    const SMOOTH_LOSSY95_RGB: [(usize, u64); 7] = [
        (492, 0xb6f5_db75_6942_5bdc),
        (472, 0xf13f_9296_fc56_0ab0),
        (484, 0x293f_cb65_b36d_4eb2),
        (446, 0x314b_0c7b_7da6_2863),
        (316, 0xc5dc_3cc3_1d04_a684),
        (316, 0xc5dc_3cc3_1d04_a684),
        (316, 0xc5dc_3cc3_1d04_a684),
    ];

    let trgba = tiled_rgba(96, 80);
    const TILED_LOSSLESS_RGBA: [(usize, u64); 7] = [
        (8074, 0x9e59_e894_c56f_1018),
        (1230, 0x20f1_a6a8_6fa1_96b1),
        (1230, 0x20f1_a6a8_6fa1_96b1),
        (1202, 0xa513_d6d8_bd3f_44f3),
        (1202, 0xa513_d6d8_bd3f_44f3),
        (1202, 0xa513_d6d8_bd3f_44f3),
        (1100, 0xaaec_4438_b319_d2a5),
    ];

    for (level, effort) in all_efforts().into_iter().enumerate() {
        for (label, want, got) in [
            (
                "lossless rgb",
                LOSSLESS_RGB[level],
                encode_lossless_rgb(effort, &rgb, d),
            ),
            (
                "lossless rgba",
                LOSSLESS_RGBA[level],
                encode_lossless(effort, &rgba, d),
            ),
            (
                "lossy60 rgb",
                LOSSY60_RGB[level],
                encode_lossy_rgb(effort, 60, &rgb, d),
            ),
            (
                "lossy60 rgba",
                LOSSY60_RGBA[level],
                encode_lossy(effort, 60, &rgba, d),
            ),
            (
                "busy lossless rgb",
                BUSY_LOSSLESS_RGB[level],
                encode_lossless_rgb(effort, &brgb, bd),
            ),
            (
                "busy lossless rgba",
                BUSY_LOSSLESS_RGBA[level],
                encode_lossless(effort, &brgba, bd),
            ),
            (
                "busy lossy60 rgb",
                BUSY_LOSSY60_RGB[level],
                encode_lossy_rgb(effort, 60, &brgb, bd),
            ),
            (
                "busy lossy60 rgba",
                BUSY_LOSSY60_RGBA[level],
                encode_lossy(effort, 60, &brgba, bd),
            ),
            (
                "patchwork lossless rgb",
                PATCH_LOSSLESS_RGB[level],
                encode_lossless_rgb(effort, &prgb, bd),
            ),
            (
                "patchwork lossless rgba",
                PATCH_LOSSLESS_RGBA[level],
                encode_lossless(effort, &prgba, bd),
            ),
            (
                "patchwork lossy60 rgb",
                PATCH_LOSSY60_RGB[level],
                encode_lossy_rgb(effort, 60, &prgb, bd),
            ),
            (
                "patchwork lossy60 rgba",
                PATCH_LOSSY60_RGBA[level],
                encode_lossy(effort, 60, &prgba, bd),
            ),
            (
                "smooth lossy95 rgb",
                SMOOTH_LOSSY95_RGB[level],
                encode_lossy_rgb(effort, 95, &srgb, bd),
            ),
            (
                "tiled lossless rgba",
                TILED_LOSSLESS_RGBA[level],
                encode_lossless(effort, &trgba, bd),
            ),
        ] {
            assert_eq!(
                (got.len(), fnv1a64(&got)),
                want,
                "{label} at effort {level}: got ({}, {:#018x})",
                got.len(),
                fnv1a64(&got)
            );
        }
    }
}

/// [`encode_lossless`] for an RGB (no-alpha) surface.
fn encode_lossless_rgb(effort: Effort, px: &[u8], d: Dimensions) -> Vec<u8> {
    let mut out = Vec::new();
    WebpEncoder::lossless()
        .with_effort(effort)
        .encode_image(ImageRef::<Rgb8>::new(px, d).expect("rgb fixture"), &mut out)
        .expect("encode");
    out
}

/// [`encode_lossy`] for an RGB (no-alpha) surface.
fn encode_lossy_rgb(effort: Effort, quality: u8, px: &[u8], d: Dimensions) -> Vec<u8> {
    let mut out = Vec::new();
    WebpEncoder::lossy(quality)
        .with_effort(effort)
        .encode_image(ImageRef::<Rgb8>::new(px, d).expect("rgb fixture"), &mut out)
        .expect("encode");
    out
}

/// Near-lossless keeps the **smaller** of the quantized and exact encodings, and a tie is not
/// smaller — so a strength that buys nothing must leave the file bit-exact, not merely no larger.
///
/// The distinction is the whole reason the knob is safe to turn on: "no larger" would still let a
/// gentle setting silently discard low bits for nothing. Tiny fixtures are where ties actually
/// happen (the two candidates differ in content but not in length), which is what makes this
/// reachable at all.
#[test]
fn a_near_lossless_setting_that_buys_nothing_stays_bit_exact() {
    let mut ties = 0usize;
    for (w, h) in [(1u32, 1u32), (1, 3), (3, 1), (2, 2), (3, 3), (4, 4)] {
        let d = dims(w, h);
        let px = noisy_rgba(w, h);
        let exact = encode_lossless(Effort::default(), &px, d);
        for strength in [0u8, 20, 40, 60, 80, 99] {
            let got = encode_near_lossless(strength, &px, d);
            assert!(
                got.len() <= exact.len(),
                "{w}x{h} strength {strength}: near-lossless must never inflate"
            );
            if got.len() == exact.len() {
                ties += 1;
                assert_eq!(
                    got, exact,
                    "{w}x{h} strength {strength}: a tie must keep the exact encoding"
                );
            }
        }
    }
    assert!(ties > 0, "the fixtures must actually produce ties to pin");
}
