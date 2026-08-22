//! The epic's conformance gate (#323/#330): max-ΔE₀₀ differentials of every `gamut-cmm`
//! transform construction against Little-CMS over a profile battery.
//!
//! Methodology as in the other oracle suites: every profile is synthesized in memory by
//! `tooling/lcms2-oracle`, serialized once, and **both** sides read the same bytes. Each
//! battery pair runs under all four intents × BPC {on, off} (BPC × absolute skipped — the
//! two are mutually exclusive by definition), over a seeded ≥ 50-pixel device sweep, against
//! two oracle configurations:
//!
//! - **TIGHT** — lcms2 with `TYPE_*_DBL` formatters and `NOOPTIMIZE|NOCACHE`: the full
//!   stage pipeline in floats, isolating evaluation semantics. Differences here are lcms2's
//!   f32 stage arithmetic and 16-bit curve/CLUT quantization against our f64.
//! - **LOOSE** — lcms2 with `TYPE_*_16` formatters and **default flags**: the optimized
//!   16-bit path real lcms2 callers get (precalculated CLUTs, quantized I/O). This bounds
//!   the crate against lcms2-in-practice, not just lcms2-in-principle.
//!
//! Outputs are compared as **ΔE₀₀** through a shared destination→Lab lens, and the maxima
//! are asserted per scenario **class** — matrix-shaper pairs vs LUT-involved pairs — because
//! one global bound sized for grid-9 CLUT quantization (~10⁻²) would hide a whole-decade
//! regression in the analytic shaper path (~10⁻⁴). The measured maxima and the asserted
//! bounds are tabulated in `STATUS.md` ("Conformance gate (P7)") with their justifications.
//!
//! The gate also covers the P7 constructions end to end: multi-profile chains vs
//! `cmsCreateMultiprofileTransform`, device links vs the one-profile transform, soft
//! proofing vs `cmsCreateProofingTransform(SOFTPROOFING)`, and the gamut check vs
//! `cmsFLAGS_GAMUTCHECK` alarm-code substitution (classification agreement — lcms2
//! quantizes its ΔE excess into a 16-bit CLUT, so magnitudes are deliberately not compared;
//! see `src/gamut.rs`).

use gamut_cmm::{
    GamutCheck, IccTransform, ProofingOptions, Transform as _, TransformOptions,
    transform_interleaved_u8,
};
use gamut_core::PixelFormat;
use gamut_icc::{IccProfile, RenderingIntent};
use lcms2_oracle::{
    FLAGS_BLACKPOINTCOMPENSATION, FLAGS_GAMUTCHECK, FLAGS_NOCACHE, FLAGS_NOOPTIMIZE,
    FLAGS_NOWHITEONWHITEFIXUP, FLAGS_SOFTPROOFING, INTENT_ABSOLUTE_COLORIMETRIC, INTENT_PERCEPTUAL,
    INTENT_RELATIVE_COLORIMETRIC, INTENT_SATURATION, Profile, TYPE_CMYK_16, TYPE_CMYK_DBL,
    TYPE_GRAY_16, TYPE_GRAY_DBL, TYPE_Lab_DBL, TYPE_RGB_16, TYPE_RGB_DBL, Transform,
    cie2000_delta_e, cmyk_ink_limiting_devicelink, cmyk_prtr_v2, cmyk_prtr_v4, display_p3_srgb_trc,
    gray, lab4, rgb_linearization_devicelink, rgb_matrix_shaper, rgb_matrix_shaper_d65_wtpt,
    scnr_lut, set_alarm_codes, set_quiet_log_handler, srgb,
};

/// D65 chromaticity and a wide (Adobe-ish) primary set for the synthesized shapers.
const D65_XY: [f64; 2] = [0.3127, 0.3290];
const WIDE_PRIMARIES: [[f64; 2]; 3] = [[0.64, 0.33], [0.21, 0.71], [0.15, 0.06]];

/// A deterministic 64-bit LCG (Knuth's MMIX constants) for seeded sweeps.
struct Lcg(u64);

impl Lcg {
    fn next_unit(&mut self) -> f64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        f64::from((self.0 >> 33) as u32) / f64::from(u32::MAX)
    }
}

/// Serializes an oracle-synthesized profile once and hands the same bytes to both sides.
fn reopen(profile: &Profile) -> (IccProfile, Profile) {
    let bytes = profile.to_bytes();
    let parsed = IccProfile::parse(&bytes).expect("gamut-icc parses the lcms2-written profile");
    let oracle = Profile::from_bytes(&bytes).expect("lcms2 reopens its own bytes");
    (parsed, oracle)
}

/// A seeded device sweep for `channels` channels: hypercube corners, a neutral ramp, and
/// random fill to `count` pixels, all in `[0, 1]`.
fn sweep(channels: usize, count: usize, seed: u64) -> Vec<Vec<f64>> {
    let mut points: Vec<Vec<f64>> = Vec::new();
    for corner in 0..(1u32 << channels) {
        points.push(
            (0..channels)
                .map(|c| f64::from((corner >> c) & 1))
                .collect(),
        );
    }
    for i in 0..=8 {
        points.push(vec![f64::from(i) / 8.0; channels]);
    }
    let mut lcg = Lcg(seed);
    while points.len() < count {
        points.push((0..channels).map(|_| lcg.next_unit()).collect());
    }
    points
}

/// One battery member: the shared bytes on both sides plus its lcms2 format words.
struct Member {
    name: &'static str,
    parsed: IccProfile,
    oracle: Profile,
    channels: usize,
    dbl_format: u32,
    u16_format: u32,
    /// The scale of the `TYPE_*_DBL` formatter: 100.0 for ink spaces, 1.0 otherwise.
    ink_scale: f64,
}

fn member(name: &'static str, profile: &Profile, channels: usize) -> Member {
    let (parsed, oracle) = reopen(profile);
    let (dbl_format, u16_format, ink_scale) = match channels {
        1 => (TYPE_GRAY_DBL, TYPE_GRAY_16, 1.0),
        3 => (TYPE_RGB_DBL, TYPE_RGB_16, 1.0),
        4 => (TYPE_CMYK_DBL, TYPE_CMYK_16, 100.0),
        n => panic!("unexpected channel count {n}"),
    };
    Member {
        name,
        parsed,
        oracle,
        channels,
        dbl_format,
        u16_format,
        ink_scale,
    }
}

/// The battery: shaper profiles (sRGB, Display P3, a wide-gamut γ2.2 pair with default and
/// D65 `wtpt`, gray) and LUT profiles (`scnr` RGB→Lab mAB, CMYK `prtr` in v4/mAB and
/// v2/lut16 serializations).
fn battery() -> Vec<Member> {
    vec![
        member("srgb", &srgb(), 3),
        member("p3", &display_p3_srgb_trc(), 3),
        member(
            "wide",
            &rgb_matrix_shaper(D65_XY, WIDE_PRIMARIES, [2.2; 3]),
            3,
        ),
        member(
            "wide-d65-wtpt",
            &rgb_matrix_shaper_d65_wtpt(D65_XY, WIDE_PRIMARIES, [2.2; 3]),
            3,
        ),
        member("gray", &gray(D65_XY, 2.2), 1),
        member("scnr-lut", &scnr_lut(9), 3),
        member("cmyk-v4", &cmyk_prtr_v4(9), 4),
        member("cmyk-v2", &cmyk_prtr_v2(9), 4),
    ]
}

fn find<'a>(battery: &'a [Member], name: &str) -> &'a Member {
    battery
        .iter()
        .find(|member| member.name == name)
        .expect("battery member")
}

/// The scenario class an assertion bound belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    /// Both sides analytic matrix/TRC shapers.
    Shaper,
    /// At least one side goes through a (grid-9) LUT profile.
    Lut,
}

/// The representative pairs: shaper↔shaper, gray↔shaper, shaper↔LUT (both directions,
/// RGB→CMYK and CMYK→RGB), LUT↔LUT (RGB→CMYK and CMYK→CMYK across serializations).
const PAIRS: [(&str, &str, Class); 8] = [
    ("srgb", "p3", Class::Shaper),
    ("wide-d65-wtpt", "srgb", Class::Shaper),
    ("gray", "srgb", Class::Shaper),
    ("srgb", "cmyk-v2", Class::Lut),
    ("cmyk-v4", "srgb", Class::Lut),
    ("cmyk-v2", "p3", Class::Lut),
    ("scnr-lut", "cmyk-v4", Class::Lut),
    ("cmyk-v2", "cmyk-v4", Class::Lut),
];

const INTENTS: [(RenderingIntent, u32); 4] = [
    (RenderingIntent::Perceptual, INTENT_PERCEPTUAL),
    (
        RenderingIntent::MediaRelativeColorimetric,
        INTENT_RELATIVE_COLORIMETRIC,
    ),
    (RenderingIntent::Saturation, INTENT_SATURATION),
    (
        RenderingIntent::IccAbsoluteColorimetric,
        INTENT_ABSOLUTE_COLORIMETRIC,
    ),
];

/// Runs our transform over one device pixel.
fn eval_ours(ours: &IccTransform, device: &[f64]) -> Vec<f64> {
    let mut out = vec![0.0; usize::from(ours.output_channels())];
    ours.transform(device, &mut out).unwrap();
    out
}

/// Converts a device pixel to Lab through the destination's lens transform (which reads the
/// `TYPE_*_DBL` layout — ink percentages for CMYK).
fn to_lab(lens: &Transform, device: &[f64], ink_scale: f64) -> [f64; 3] {
    let scaled: Vec<f64> = device.iter().map(|&v| v * ink_scale).collect();
    let lab = lens.apply_f64(&scaled, 1, 3);
    [lab[0], lab[1], lab[2]]
}

/// ΔE₀₀ between two device pixels seen through the shared lens.
fn lens_delta_e(lens: &Transform, a: &[f64], b: &[f64], ink_scale: f64) -> f64 {
    let la = to_lab(lens, a, ink_scale);
    let lb = to_lab(lens, b, ink_scale);
    cie2000_delta_e(la, lb, 1.0, 1.0, 1.0)
}

/// Clamps lcms2's raw double outputs into `[0, 1]` device range (this crate's `ToneCurve`
/// convention — see `tests/oracle_intents.rs`).
fn clamp01(v: Vec<f64>) -> Vec<f64> {
    v.into_iter().map(|s| s.clamp(0.0, 1.0)).collect()
}

/// Per-class accumulator: the max ΔE₀₀ (with its cell label) and the running mean.
#[derive(Default)]
struct Worst {
    value: f64,
    cell: String,
    sum: f64,
    count: usize,
}

impl Worst {
    fn feed(&mut self, value: f64, cell: &str) {
        if value > self.value {
            self.value = value;
            self.cell = cell.to_owned();
        }
        self.sum += value;
        self.count += 1;
    }

    fn mean(&self) -> f64 {
        self.sum / self.count.max(1) as f64
    }
}

// The asserted per-class bounds. Measured (this battery, seeds below, lcms2 2.19):
//
//   class          max ΔE₀₀   mean ΔE₀₀   worst cell
//   tight/shaper   7.82e-4    —           gray→srgb perceptual (near-black γ-inverse noise)
//   tight/LUT      6.87e-3    —           cmyk-v2→cmyk-v4 relative (16-bit CLUT quantization)
//   loose/shaper   3.34e-1    1.49e-2     wide-d65→srgb relative (lcms2 grid-33 precalc toe)
//   loose/LUT      1.10e0     7.23e-3     cmyk-v2→p3 absolute (precalc smoothing of the clip)
//
// Max bounds carry ~2-3× headroom over the measured maxima; the LOOSE class maxima are
// dominated by the *oracle's* precalculated-CLUT approximation at gamut-clip boundaries and
// deep-shadow toes (at those very pixels our output matches the TIGHT oracle to ~1e-3), so
// the loose gate additionally asserts the MEAN — a wrong tag/seam/BPC regression shifts
// whole sweeps, blowing the mean bound long before the max one. Full table + justifications
// in STATUS.md.
const TIGHT_SHAPER_BOUND: f64 = 2e-3;
const TIGHT_LUT_BOUND: f64 = 2e-2;
const LOOSE_SHAPER_BOUND: f64 = 6e-1;
const LOOSE_LUT_BOUND: f64 = 2.0;
const LOOSE_SHAPER_MEAN_BOUND: f64 = 5e-2;
const LOOSE_LUT_MEAN_BOUND: f64 = 3e-2;

#[test]
fn conformance_pairs_battery() {
    set_quiet_log_handler();
    let battery = battery();
    let lab = lab4();
    let mut tight = [Worst::default(), Worst::default()]; // [Shaper, Lut]
    let mut loose = [Worst::default(), Worst::default()];
    for (src_name, dst_name, class) in PAIRS {
        let src = find(&battery, src_name);
        let dst = find(&battery, dst_name);
        let lens = Transform::new(
            &dst.oracle,
            dst.dbl_format,
            &lab,
            TYPE_Lab_DBL,
            INTENT_RELATIVE_COLORIMETRIC,
            FLAGS_NOCACHE,
        );
        for (our_intent, lcms_intent) in INTENTS {
            for bpc in [false, true] {
                if bpc && our_intent == RenderingIntent::IccAbsoluteColorimetric {
                    continue; // BPC and absolute are mutually exclusive: nothing to gate.
                }
                let cell = format!("{src_name}->{dst_name} {our_intent:?} bpc={bpc}");
                let bpc_flag = if bpc { FLAGS_BLACKPOINTCOMPENSATION } else { 0 };
                let ours = IccTransform::between(
                    &src.parsed,
                    &dst.parsed,
                    TransformOptions {
                        intent: our_intent,
                        black_point_compensation: bpc,
                    },
                )
                .unwrap();
                let lcms_tight = Transform::new(
                    &src.oracle,
                    src.dbl_format,
                    &dst.oracle,
                    dst.dbl_format,
                    lcms_intent,
                    FLAGS_NOOPTIMIZE | FLAGS_NOCACHE | bpc_flag,
                );
                // The 16-bit path real callers get: default flags, quantized formatters.
                // One default is disabled: the white-on-white ("scum dot") fixup, an
                // lcms2-only aesthetic that snaps the input white node onto the output
                // white in the precalculated CLUT — it would dominate the metric at the
                // device-white corner (measured 8.5 dE00 on the CMYK printer, whose paper
                // simulation is deliberately off-white) while gating nothing about this
                // crate's correctness.
                let lcms_loose = Transform::new(
                    &src.oracle,
                    src.u16_format,
                    &dst.oracle,
                    dst.u16_format,
                    lcms_intent,
                    bpc_flag | FLAGS_NOWHITEONWHITEFIXUP,
                );
                let class_slot = usize::from(class == Class::Lut);
                for device in sweep(src.channels, 60, 0xC0FF_EE00 ^ u64::from(lcms_intent)) {
                    // TIGHT: exact f64 device values on both sides.
                    let got = eval_ours(&ours, &device);
                    let ink: Vec<f64> = device.iter().map(|&v| v * src.ink_scale).collect();
                    let want = clamp01(
                        lcms_tight
                            .apply_f64(&ink, 1, dst.channels)
                            .iter()
                            .map(|&v| v / dst.ink_scale)
                            .collect(),
                    );
                    tight[class_slot].feed(lens_delta_e(&lens, &got, &want, dst.ink_scale), &cell);
                    // LOOSE: both sides fed the identical u16-quantized device values.
                    let device16: Vec<u16> = device
                        .iter()
                        .map(|&v| {
                            #[expect(
                                clippy::cast_possible_truncation,
                                clippy::cast_sign_loss,
                                reason = "v is in [0, 1]"
                            )]
                            {
                                (v * 65535.0 + 0.5) as u16
                            }
                        })
                        .collect();
                    let device_q: Vec<f64> =
                        device16.iter().map(|&v| f64::from(v) / 65535.0).collect();
                    let got = eval_ours(&ours, &device_q);
                    let want16 = lcms_loose.apply_u16(&device16, 1, dst.channels);
                    let want: Vec<f64> = want16.iter().map(|&v| f64::from(v) / 65535.0).collect();
                    loose[class_slot].feed(lens_delta_e(&lens, &got, &want, dst.ink_scale), &cell);
                }
            }
        }
    }
    eprintln!(
        "conformance maxima: tight shaper {:.3e} ({}), tight LUT {:.3e} ({}), \
         loose shaper {:.3e} ({}) mean {:.3e}, loose LUT {:.3e} ({}) mean {:.3e}",
        tight[0].value,
        tight[0].cell,
        tight[1].value,
        tight[1].cell,
        loose[0].value,
        loose[0].cell,
        loose[0].mean(),
        loose[1].value,
        loose[1].cell,
        loose[1].mean(),
    );
    assert!(
        tight[0].value < TIGHT_SHAPER_BOUND,
        "tight/shaper max ΔE00 {:.3e} at {}",
        tight[0].value,
        tight[0].cell
    );
    assert!(
        tight[1].value < TIGHT_LUT_BOUND,
        "tight/LUT max ΔE00 {:.3e} at {}",
        tight[1].value,
        tight[1].cell
    );
    assert!(
        loose[0].value < LOOSE_SHAPER_BOUND,
        "loose/shaper max ΔE00 {:.3e} at {}",
        loose[0].value,
        loose[0].cell
    );
    assert!(
        loose[1].value < LOOSE_LUT_BOUND,
        "loose/LUT max ΔE00 {:.3e} at {}",
        loose[1].value,
        loose[1].cell
    );
    assert!(
        loose[0].mean() < LOOSE_SHAPER_MEAN_BOUND,
        "loose/shaper mean ΔE00 {:.3e}",
        loose[0].mean()
    );
    assert!(
        loose[1].mean() < LOOSE_LUT_MEAN_BOUND,
        "loose/LUT mean ΔE00 {:.3e}",
        loose[1].mean()
    );
    // The classes really are distinct regimes: the LUT maxima must dominate the shaper
    // maxima, or the split (and its documented rationale) is stale.
    assert!(tight[1].value > tight[0].value);
    assert!(loose[1].value > loose[0].value);
}

#[test]
fn multiprofile_chain_matches_lcms2() {
    set_quiet_log_handler();
    // Three-profile chains through the Lab identity: RGB→Lab4→CMYK and CMYK→Lab4→RGB, at
    // two intents × BPC on/off, vs cmsCreateMultiprofileTransform (which replicates its one
    // intent and BPC flag per hop — exactly IccTransform::chain's contract).
    let (srgb_parsed, srgb_oracle) = reopen(&srgb());
    let (cmyk_parsed, cmyk_oracle) = reopen(&cmyk_prtr_v4(9));
    let (lab_parsed, lab_oracle) = reopen(&lab4());
    /// One chain case: src/dst profiles, channel counts, lcms formats, and DBL scales.
    type ChainCase<'a> = (
        &'a IccProfile,
        &'a IccProfile,
        usize,
        usize,
        u32,
        u32,
        f64,
        f64,
    );
    let cases: [ChainCase; 2] = [
        (
            &srgb_parsed,
            &cmyk_parsed,
            3,
            4,
            TYPE_RGB_DBL,
            TYPE_CMYK_DBL,
            1.0,
            100.0,
        ),
        (
            &cmyk_parsed,
            &srgb_parsed,
            4,
            3,
            TYPE_CMYK_DBL,
            TYPE_RGB_DBL,
            100.0,
            1.0,
        ),
    ];
    for (case, (src, dst, n_in, n_out, in_fmt, out_fmt, in_scale, out_scale)) in
        cases.into_iter().enumerate()
    {
        let src_oracle = if case == 0 {
            &srgb_oracle
        } else {
            &cmyk_oracle
        };
        let dst_oracle = if case == 0 {
            &cmyk_oracle
        } else {
            &srgb_oracle
        };
        for (our_intent, lcms_intent) in [
            (RenderingIntent::Perceptual, INTENT_PERCEPTUAL),
            (
                RenderingIntent::MediaRelativeColorimetric,
                INTENT_RELATIVE_COLORIMETRIC,
            ),
        ] {
            for bpc in [false, true] {
                let ours = IccTransform::chain(
                    &[src, &lab_parsed, dst],
                    TransformOptions {
                        intent: our_intent,
                        black_point_compensation: bpc,
                    },
                )
                .unwrap();
                assert_eq!(usize::from(ours.input_channels()), n_in);
                assert_eq!(usize::from(ours.output_channels()), n_out);
                let flags = FLAGS_NOOPTIMIZE
                    | FLAGS_NOCACHE
                    | if bpc { FLAGS_BLACKPOINTCOMPENSATION } else { 0 };
                let lcms = Transform::multiprofile(
                    &[src_oracle, &lab_oracle, dst_oracle],
                    in_fmt,
                    out_fmt,
                    lcms_intent,
                    flags,
                );
                // Two regimes, asserted separately. When the mid-chain colorimetry leaves
                // (or lands on the edge of) the v4 Lab encodeable range — deep CMYK blacks
                // pushed below zero by a BPC layer, or chroma past ±128 — OUR abstract hop
                // clamps it at its encoded seam (the P2 curve convention), where lcms2's
                // unclamped parametric identity curves carry the overshoot to the final
                // formatter: the documented per-hop clamping divergence (STATUS.md,
                // "Settled decisions (P7)"). Those pixels are detected via the mid-chain
                // Lab (the chain truncated after the Lab hop) and bounded loosely; every
                // in-range pixel must track lcms2 tightly.
                let mid_chain = IccTransform::chain(
                    &[src, &lab_parsed],
                    TransformOptions {
                        intent: our_intent,
                        black_point_compensation: bpc,
                    },
                )
                .unwrap();
                let mut worst = 0.0_f64;
                let mut worst_clamped = 0.0_f64;
                for device in sweep(n_in, 50, 0xABCD ^ u64::from(lcms_intent)) {
                    let got = eval_ours(&ours, &device);
                    let ink: Vec<f64> = device.iter().map(|&v| v * in_scale).collect();
                    let want = clamp01(
                        lcms.apply_f64(&ink, 1, n_out)
                            .iter()
                            .map(|&v| v / out_scale)
                            .collect(),
                    );
                    let mid = eval_ours(&mid_chain, &device);
                    let on_edge = |v: f64, lo: f64, hi: f64| v <= lo + 1e-6 || v >= hi - 1e-6;
                    let clamped = on_edge(mid[0], 0.0, 100.0)
                        || on_edge(mid[1], -128.0, 127.0)
                        || on_edge(mid[2], -128.0, 127.0);
                    for ch in 0..n_out {
                        let delta = (got[ch] - want[ch]).abs();
                        if clamped {
                            worst_clamped = worst_clamped.max(delta);
                        } else {
                            worst = worst.max(delta);
                        }
                    }
                }
                // Measured: worst 3.1e-5 (16-bit CLUT quantization of the grid-9 hops);
                // worst_clamped 1.66e-1 (relative + BPC pushing 300-400% ink blacks below
                // the encodeable floor — the clamping divergence above; 4.6e-3 for the
                // perceptual fixed-black layer, ≤ 1e-5 where no compensation overshoots).
                eprintln!(
                    "chain case {case} {our_intent:?} bpc={bpc}: worst {worst:.3e}, \
                     clamped-regime worst {worst_clamped:.3e}"
                );
                assert!(
                    worst < 2e-3,
                    "chain case {case} {our_intent:?} bpc={bpc}: worst |Δ| = {worst:e}"
                );
                assert!(
                    worst_clamped < 2.5e-1,
                    "chain case {case} {our_intent:?} bpc={bpc}: clamped-regime worst |Δ| = \
                     {worst_clamped:e}"
                );
            }
        }
    }
}

/// A hand-built v2 `lut16` RGB→Lab device-link: a 3-node CLUT warp indexed by RGB with Lab
/// output — the vehicle for the devicelink trilinear rule, the lut16 v2-Lab seams, and the
/// perceptual-tag fallback, differentially against `_cmsReadDevicelinkLUT`.
fn rgb_to_lab_lut16_link() -> IccProfile {
    use gamut_icc::{
        ColorSpace, DeviceClass, Lut16, Matrix3x3, ProfileHeader, S15Fixed16, Signature, TagData,
    };
    let identity3x3 = {
        let mut elements = [S15Fixed16(0); 9];
        for i in 0..3 {
            elements[i * 4] = S15Fixed16(0x0001_0000);
        }
        Matrix3x3 { elements }
    };
    // A smooth channel-mixing 3×3×3 CLUT (deterministic, in-range).
    let mut clut = Vec::new();
    for r in 0..3u32 {
        for g in 0..3u32 {
            for b in 0..3u32 {
                let (rf, gf, bf) = (f64::from(r) / 2.0, f64::from(g) / 2.0, f64::from(b) / 2.0);
                let l = 0.1 + 0.8 * (0.3 * rf + 0.55 * gf + 0.15 * bf);
                let a = 0.5 + 0.2 * (rf - gf);
                let bb = 0.5 + 0.2 * (gf - bf);
                for v in [l, a, bb] {
                    #[expect(
                        clippy::cast_possible_truncation,
                        clippy::cast_sign_loss,
                        reason = "v is in [0, 1]"
                    )]
                    clut.push((v * 65535.0 + 0.5) as u16);
                }
            }
        }
    }
    let mut identity_tables = Vec::new();
    for _ in 0..3 {
        identity_tables.extend([0u16, 65535]);
    }
    let lut16 = TagData::Lut16(Lut16 {
        input_channels: 3,
        output_channels: 3,
        grid_points: 3,
        matrix: identity3x3,
        input_table_entries: 2,
        output_table_entries: 2,
        input_table: identity_tables.clone(),
        clut,
        output_table: identity_tables,
    });
    let mut header = ProfileHeader::new(DeviceClass::DeviceLink, ColorSpace::Rgb);
    header.pcs = ColorSpace::Lab;
    header.version.major = 2;
    IccProfile {
        header,
        tags: vec![(Signature(*b"A2B0"), lut16)],
    }
}

#[test]
fn device_links_match_lcms2() {
    set_quiet_log_handler();
    // (a) The v4 mAB CMYK ink-limiting link (CLUT-bearing, CMYK→CMYK).
    let (parsed, oracle) = reopen(&cmyk_ink_limiting_devicelink(250.0));
    for (our_intent, lcms_intent) in [
        (RenderingIntent::Perceptual, INTENT_PERCEPTUAL),
        // Only A2B0 exists: every intent must take the perceptual fallback.
        (RenderingIntent::Saturation, INTENT_SATURATION),
    ] {
        let ours = IccTransform::device_link(&parsed, our_intent).unwrap();
        let lcms = Transform::devicelink(
            &oracle,
            TYPE_CMYK_DBL,
            TYPE_CMYK_DBL,
            lcms_intent,
            FLAGS_NOOPTIMIZE | FLAGS_NOCACHE,
        );
        let mut worst = 0.0_f64;
        for device in sweep(4, 50, 0xD1CE) {
            let got = eval_ours(&ours, &device);
            let ink: Vec<f64> = device.iter().map(|&v| v * 100.0).collect();
            let want = clamp01(
                lcms.apply_f64(&ink, 1, 4)
                    .iter()
                    .map(|&v| v / 100.0)
                    .collect(),
            );
            for ch in 0..4 {
                worst = worst.max((got[ch] - want[ch]).abs());
            }
        }
        eprintln!("ink limiting {our_intent:?}: worst {worst:.3e}");
        // Measured worst 1.5e-5 (16-bit CLUT quantization in the oracle's evaluator).
        assert!(
            worst < 5e-4,
            "ink limiting {our_intent:?}: worst |Δ| = {worst:e}"
        );
    }

    // (b) The v4 curves-only RGB linearization link (identity curves: output == input).
    let (parsed, oracle) = reopen(&rgb_linearization_devicelink());
    let ours =
        IccTransform::device_link(&parsed, RenderingIntent::MediaRelativeColorimetric).unwrap();
    let lcms = Transform::devicelink(
        &oracle,
        TYPE_RGB_DBL,
        TYPE_RGB_DBL,
        INTENT_RELATIVE_COLORIMETRIC,
        FLAGS_NOOPTIMIZE | FLAGS_NOCACHE,
    );
    let mut worst = 0.0_f64;
    for device in sweep(3, 50, 0xF00D) {
        let got = eval_ours(&ours, &device);
        let want = clamp01(lcms.apply_f64(&device, 1, 3));
        for ch in 0..3 {
            worst = worst.max((got[ch] - want[ch]).abs());
            worst = worst.max((got[ch] - device[ch]).abs()); // identity link
        }
    }
    eprintln!("linearization link: worst {worst:.3e}");
    // Measured worst 1.5e-8 (both sides analytic; f32 formatter noise only).
    assert!(worst < 5e-4, "linearization link: worst |Δ| = {worst:e}");

    // (c) The hand-built v2 lut16 RGB→Lab link: trilinear (not tetrahedral) CLUTs, v2-Lab
    // decode seam, both pinned differentially. Both sides read the same gamut-icc bytes.
    let link = rgb_to_lab_lut16_link();
    let bytes = link.to_bytes().expect("gamut-icc serializes the link");
    let reparsed = IccProfile::parse(&bytes).expect("round-trips");
    let oracle = Profile::from_bytes(&bytes).expect("lcms2 opens the link");
    let ours = IccTransform::device_link(&reparsed, RenderingIntent::Perceptual).unwrap();
    let lcms = Transform::devicelink(
        &oracle,
        TYPE_RGB_DBL,
        TYPE_Lab_DBL,
        INTENT_PERCEPTUAL,
        FLAGS_NOOPTIMIZE | FLAGS_NOCACHE,
    );
    let mut worst = 0.0_f64;
    for device in sweep(3, 60, 0x1AB) {
        let got = eval_ours(&ours, &device); // decoded Lab out
        let want = lcms.apply_f64(&device, 1, 3); // TYPE_Lab_DBL: decoded Lab
        for ch in 0..3 {
            worst = worst.max((got[ch] - want[ch]).abs());
        }
    }
    eprintln!("lut16 Lab link: worst {worst:.3e}");
    // Decoded-Lab units (L* 0..100): measured worst 3.4e-3 — small enough that the
    // tetrahedral evaluation of the same 3-node CLUT (which diverges by whole L* tenths
    // off-diagonal) fails this bound, pinning the trilinear rule differentially.
    assert!(worst < 5e-2, "lut16 Lab link: worst |Δ| = {worst:e}");
}

#[test]
fn proofing_matches_lcms2() {
    set_quiet_log_handler();
    // sRGB source previewed as the CMYK v4 printer would render, delivered to Display P3 —
    // vs cmsCreateProofingTransform with SOFTPROOFING, across intent pairs × BPC.
    let (src_parsed, src_oracle) = reopen(&srgb());
    let (dst_parsed, dst_oracle) = reopen(&display_p3_srgb_trc());
    let (proof_parsed, proof_oracle) = reopen(&cmyk_prtr_v4(9));
    for (our_intent, lcms_intent) in [
        (RenderingIntent::Perceptual, INTENT_PERCEPTUAL),
        (
            RenderingIntent::MediaRelativeColorimetric,
            INTENT_RELATIVE_COLORIMETRIC,
        ),
    ] {
        for (our_proof_intent, lcms_proof_intent) in [
            (
                RenderingIntent::MediaRelativeColorimetric,
                INTENT_RELATIVE_COLORIMETRIC,
            ),
            (
                RenderingIntent::IccAbsoluteColorimetric,
                INTENT_ABSOLUTE_COLORIMETRIC,
            ),
        ] {
            for bpc in [false, true] {
                let ours = IccTransform::proofing(
                    &src_parsed,
                    &dst_parsed,
                    &proof_parsed,
                    ProofingOptions {
                        intent: our_intent,
                        proofing_intent: our_proof_intent,
                        black_point_compensation: bpc,
                    },
                )
                .unwrap();
                let flags = FLAGS_SOFTPROOFING
                    | FLAGS_NOOPTIMIZE
                    | FLAGS_NOCACHE
                    | if bpc { FLAGS_BLACKPOINTCOMPENSATION } else { 0 };
                let lcms = Transform::proofing(
                    &src_oracle,
                    TYPE_RGB_DBL,
                    &dst_oracle,
                    TYPE_RGB_DBL,
                    &proof_oracle,
                    lcms_intent,
                    lcms_proof_intent,
                    flags,
                );
                let mut worst = 0.0_f64;
                for device in sweep(3, 50, 0x9909 ^ u64::from(lcms_intent)) {
                    let got = eval_ours(&ours, &device);
                    let want = clamp01(lcms.apply_f64(&device, 1, 3));
                    for ch in 0..3 {
                        worst = worst.max((got[ch] - want[ch]).abs());
                    }
                }
                eprintln!(
                    "proofing {our_intent:?}/{our_proof_intent:?} bpc={bpc}: worst {worst:.3e}"
                );
                // Measured worst 3.4e-5 across the eight cells (16-bit quantization of the
                // grid-9 LUT hops).
                assert!(
                    worst < 5e-3,
                    "proofing {our_intent:?}/{our_proof_intent:?} bpc={bpc}: worst |Δ| = {worst:e}"
                );
            }
        }
    }
    // SOFTPROOFING really is the semantic under test: our proofing output must diverge from
    // the plain src→dst transform somewhere (the printer simulation is visible).
    let ours = IccTransform::proofing(
        &src_parsed,
        &dst_parsed,
        &proof_parsed,
        ProofingOptions {
            intent: RenderingIntent::MediaRelativeColorimetric,
            proofing_intent: RenderingIntent::MediaRelativeColorimetric,
            black_point_compensation: false,
        },
    )
    .unwrap();
    let plain = IccTransform::between(
        &src_parsed,
        &dst_parsed,
        TransformOptions {
            intent: RenderingIntent::MediaRelativeColorimetric,
            black_point_compensation: false,
        },
    )
    .unwrap();
    let mut gap = 0.0_f64;
    for device in sweep(3, 30, 0x51AB) {
        let a = eval_ours(&ours, &device);
        let b = eval_ours(&plain, &device);
        gap = gap.max((0..3).map(|ch| (a[ch] - b[ch]).abs()).fold(0.0, f64::max));
    }
    assert!(
        gap > 0.05,
        "proofing must simulate the printer: gap = {gap:e}"
    );
}

#[test]
fn gamut_check_classification_matches_lcms2() {
    set_quiet_log_handler();
    // Wide-gamut γ2.2 source checked against two proofs: sRGB (matrix shaper → threshold
    // 1.0) and the CMYK v4 printer (LUT → threshold 5.0). lcms2 side: a proofing transform
    // with FLAGS_GAMUTCHECK and sentinel alarm codes — an out-of-gamut pixel comes back as
    // the alarm colour in every channel. Only the in/out CLASSIFICATION is compared: lcms2
    // quantizes its ΔE excess into a 16-bit CLUT sampled on a coarse grid, so magnitudes
    // (and near-boundary colours) are not comparable by construction.
    let (wide_parsed, wide_oracle) = reopen(&rgb_matrix_shaper(D65_XY, WIDE_PRIMARIES, [2.2; 3]));
    let (srgb_parsed, srgb_oracle) = reopen(&srgb());
    let (cmyk_parsed, cmyk_oracle) = reopen(&cmyk_prtr_v4(9));
    let alarm: [u16; 3] = [0xCAFE, 0x1234, 0xBEEF];
    set_alarm_codes(alarm);
    let alarm_f64: [f64; 3] = [
        f64::from(alarm[0]) / 65535.0,
        f64::from(alarm[1]) / 65535.0,
        f64::from(alarm[2]) / 65535.0,
    ];
    // Decisively in-gamut (neutrals, desaturated) and decisively out-of-gamut (saturated
    // wide-gamut colours no sRGB display or CMYK proof reproduces) device values.
    let pixels: [[f64; 3]; 8] = [
        [0.5, 0.5, 0.5],
        [0.3, 0.35, 0.4],
        [0.7, 0.6, 0.5],
        [0.25, 0.25, 0.3],
        [0.0, 1.0, 0.0],
        [0.0, 1.0, 0.2],
        [0.1, 0.9, 0.05],
        [0.0, 0.85, 0.1],
    ];
    for (proof_parsed, proof_oracle, label) in [
        (&srgb_parsed, &srgb_oracle, "srgb proof"),
        (&cmyk_parsed, &cmyk_oracle, "cmyk proof"),
    ] {
        for (our_intent, lcms_intent) in [
            (
                RenderingIntent::MediaRelativeColorimetric,
                INTENT_RELATIVE_COLORIMETRIC,
            ),
            (RenderingIntent::Perceptual, INTENT_PERCEPTUAL),
        ] {
            let check = GamutCheck::new(&wide_parsed, proof_parsed, our_intent).unwrap();
            let lcms = Transform::proofing(
                &wide_oracle,
                TYPE_RGB_DBL,
                &srgb_oracle,
                TYPE_RGB_DBL,
                proof_oracle,
                lcms_intent,
                INTENT_RELATIVE_COLORIMETRIC,
                FLAGS_GAMUTCHECK | FLAGS_NOCACHE,
            );
            let mut any_in = false;
            let mut any_out = false;
            for device in pixels {
                let mut excess = [f64::NAN];
                check.transform(&device, &mut excess).unwrap();
                let out = lcms.apply_f64(&device, 1, 3);
                let lcms_out_of_gamut = (0..3).all(|ch| (out[ch] - alarm_f64[ch]).abs() < 1e-4);
                let ours_out_of_gamut = excess[0] > 0.0;
                assert_eq!(
                    ours_out_of_gamut, lcms_out_of_gamut,
                    "{label} {our_intent:?} {device:?}: excess {} vs lcms {out:?}",
                    excess[0]
                );
                if ours_out_of_gamut {
                    any_out = true;
                } else {
                    any_in = true;
                    // In-gamut is exactly 0.0 on our side — the documented f64 contract.
                    assert_eq!(excess[0], 0.0, "{label} {device:?}");
                }
            }
            assert!(
                any_in && any_out,
                "{label} {our_intent:?}: degenerate pixel set"
            );
        }
    }
    // Restore the lcms2 default alarm codes for other tests in this process.
    set_alarm_codes([0x7F00, 0x7F00, 0x7F00]);
}

#[test]
fn pixel_buffers_ride_the_conformance_transforms() {
    set_quiet_log_handler();
    // The buffer layer over a real profile pair: Rgb8 → Cmyk8 through srgb→cmyk-v4 equals
    // the scalar path re-encoded, pixel for pixel (u8 in, u8 out, alpha-free).
    let (src_parsed, _) = reopen(&srgb());
    let (dst_parsed, _) = reopen(&cmyk_prtr_v4(9));
    let ours =
        IccTransform::between(&src_parsed, &dst_parsed, TransformOptions::default()).unwrap();
    let src: Vec<u8> = (0..60u32)
        .flat_map(|i| {
            [
                u8::try_from((i * 41) % 256).unwrap(),
                u8::try_from((i * 89) % 256).unwrap(),
                u8::try_from((i * 173) % 256).unwrap(),
            ]
        })
        .collect();
    let mut dst = vec![0u8; 240];
    transform_interleaved_u8(&ours, PixelFormat::Rgb8, &src, PixelFormat::Cmyk8, &mut dst).unwrap();
    for (px, out) in src.chunks_exact(3).zip(dst.chunks_exact(4)) {
        let device: Vec<f64> = px.iter().map(|&v| f64::from(v) / 255.0).collect();
        let mut want = [0.0; 4];
        ours.transform(&device, &mut want).unwrap();
        for ch in 0..4 {
            #[expect(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "clamped to the u8 range"
            )]
            let expected = (want[ch] * 255.0 + 0.5).floor().clamp(0.0, 255.0) as u8;
            assert_eq!(out[ch], expected, "pixel {px:?} channel {ch}");
        }
    }
}
