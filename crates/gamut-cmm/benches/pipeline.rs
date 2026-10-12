//! Buffer-throughput benchmarks for the pipeline-optimization knob (issues #372, #149).
//!
//! The question #372 asks is a throughput question: what does evaluating a transform stage by
//! stage cost against collapsing it first, and how does either compare to lcms2 — the oracle
//! this crate is measured against — on the same buffer? So each scenario runs five bars over
//! one 256×256 interleaved image:
//!
//! | bar | what it is |
//! |-----|------------|
//! | `off` | `PipelineOptimization::None` — v1's stage-by-stage evaluation, the default |
//! | `collapse` | identity elision + matrix folding only |
//! | `precalculate` | + curve joining and CLUT resampling (lcms2's default construction) |
//! | `lcms2_optimized` | lcms2 at `flags = 0`, its own default optimized path |
//! | `lcms2_baseline` | lcms2 at `cmsFLAGS_NOOPTIMIZE`, its stage-by-stage path |
//!
//! Two caveats when reading the lcms2 bars. They run lcms2's `TYPE_*_8` formatters (its 8-bit
//! fast path) against this crate's `f64` core, so they are a *context* line rather than a
//! like-for-like comparison; and `lcms2_oracle`'s `apply_u8` allocates a fresh output `Vec` per
//! call, which the gamut bars (writing into a reused buffer) do not.
//!
//! Transform *construction* is hoisted out of every timed closure — including the resampling,
//! which is build-time work by design — because what a pixel-buffer workload cares about is the
//! per-pixel cost it pays once the transform exists. Run with `cargo bench -p gamut-cmm`.

use divan::counter::ItemsCount;
use divan::{Bencher, black_box};
use gamut_cmm::{IccTransform, PipelineOptimization, TransformOptions, transform_interleaved_u8};
use gamut_core::PixelFormat;
use gamut_icc::{BuiltinProfile, IccProfile, RenderingIntent};
use lcms2_oracle::{
    FLAGS_NOOPTIMIZE, INTENT_RELATIVE_COLORIMETRIC, Profile, TYPE_CMYK_8, TYPE_RGB_8, Transform,
    cmyk_prtr_v4, display_p3_srgb_trc, set_quiet_log_handler, srgb,
};

fn main() {
    set_quiet_log_handler();
    divan::main();
}

/// Side length of the square test image (65 536 pixels — a thumbnail-sized buffer, big enough
/// to swamp per-call overhead and small enough to stay in cache pressure a real caller sees).
const SIDE: usize = 256;

fn pixels() -> usize {
    SIDE * SIDE
}

/// A deterministic RGB gradient with enough variety to exercise the whole device cube.
fn gradient(channels: usize) -> Vec<u8> {
    let mut buf = vec![0u8; pixels() * channels];
    for y in 0..SIDE {
        for x in 0..SIDE {
            let base = (y * SIDE + x) * channels;
            for c in 0..channels {
                let v = (x * (c + 1) + y * (channels - c)) ^ ((x * y) >> 4);
                buf[base + c] = (v % 256) as u8;
            }
        }
    }
    buf
}

/// Serializes an oracle-synthesized profile once and hands the same bytes to both sides — the
/// methodology the conformance tests use, so the two sides really are running one profile.
fn reopen(profile: &Profile) -> (IccProfile, Profile) {
    let bytes = profile.to_bytes();
    let parsed = IccProfile::parse(&bytes).expect("gamut-icc parses the lcms2-written profile");
    let oracle = Profile::from_bytes(&bytes).expect("lcms2 reopens its own bytes");
    (parsed, oracle)
}

/// The two scenarios: an all-analytic matrix/TRC shaper pair (where collapsing replaces `powf`
/// calls with a grid lookup) and a shaper→CMYK-LUT pair (where a grid-9 profile CLUT and a
/// channel-count change are already in the chain).
enum Scenario {
    ShaperPair,
    ShaperToLut,
}

impl Scenario {
    fn profiles(&self) -> (Profile, Profile) {
        match self {
            Scenario::ShaperPair => (srgb(), display_p3_srgb_trc()),
            Scenario::ShaperToLut => (srgb(), cmyk_prtr_v4(9)),
        }
    }

    /// `(src channels, dst channels, lcms2 src format, lcms2 dst format, dst pixel format)`.
    fn layout(&self) -> (usize, usize, u32, u32, PixelFormat) {
        match self {
            Scenario::ShaperPair => (3, 3, TYPE_RGB_8, TYPE_RGB_8, PixelFormat::Rgb8),
            Scenario::ShaperToLut => (3, 4, TYPE_RGB_8, TYPE_CMYK_8, PixelFormat::Cmyk8),
        }
    }
}

/// Runs one gamut-cmm bar: builds the transform at `level` outside the timed closure, then
/// times the interleaved 8-bit buffer application.
fn bench_gamut(bencher: Bencher, scenario: &Scenario, level: PipelineOptimization) {
    let (src_profile, dst_profile) = scenario.profiles();
    let (src_channels, dst_channels, _, _, dst_format) = scenario.layout();
    let (src, _) = reopen(&src_profile);
    let (dst, _) = reopen(&dst_profile);
    let transform = IccTransform::between(
        &src,
        &dst,
        TransformOptions {
            intent: RenderingIntent::MediaRelativeColorimetric,
            black_point_compensation: false,
            optimization: level,
            prefer_cicp_transfer: false,
        },
    )
    .expect("the benchmark profiles link");
    let input = gradient(src_channels);
    let mut output = vec![0u8; pixels() * dst_channels];
    bencher.counter(ItemsCount::new(pixels())).bench_local(|| {
        transform_interleaved_u8(
            &transform,
            PixelFormat::Rgb8,
            black_box(&input),
            dst_format,
            &mut output,
        )
        .expect("the buffers match the transform");
    });
}

/// Runs one lcms2 bar at `flags`.
fn bench_lcms2(bencher: Bencher, scenario: &Scenario, flags: u32) {
    let (src_profile, dst_profile) = scenario.profiles();
    let (src_channels, dst_channels, src_format, dst_format, _) = scenario.layout();
    let (_, src) = reopen(&src_profile);
    let (_, dst) = reopen(&dst_profile);
    let transform = Transform::new(
        &src,
        src_format,
        &dst,
        dst_format,
        INTENT_RELATIVE_COLORIMETRIC,
        flags,
    );
    let input = gradient(src_channels);
    bencher
        .counter(ItemsCount::new(pixels()))
        .bench_local(|| transform.apply_u8(black_box(&input), pixels(), dst_channels));
}

mod shaper_pair {
    use super::{Bencher, PipelineOptimization, Scenario, bench_gamut, bench_lcms2};

    #[divan::bench]
    fn off(bencher: Bencher) {
        bench_gamut(bencher, &Scenario::ShaperPair, PipelineOptimization::None);
    }

    #[divan::bench]
    fn collapse(bencher: Bencher) {
        bench_gamut(
            bencher,
            &Scenario::ShaperPair,
            PipelineOptimization::Collapse,
        );
    }

    #[divan::bench]
    fn precalculate(bencher: Bencher) {
        bench_gamut(
            bencher,
            &Scenario::ShaperPair,
            PipelineOptimization::Precalculate,
        );
    }

    #[divan::bench]
    fn lcms2_optimized(bencher: Bencher) {
        bench_lcms2(bencher, &Scenario::ShaperPair, 0);
    }

    #[divan::bench]
    fn lcms2_baseline(bencher: Bencher) {
        bench_lcms2(bencher, &Scenario::ShaperPair, super::FLAGS_NOOPTIMIZE);
    }
}

mod shaper_to_lut {
    use super::{Bencher, PipelineOptimization, Scenario, bench_gamut, bench_lcms2};

    #[divan::bench]
    fn off(bencher: Bencher) {
        bench_gamut(bencher, &Scenario::ShaperToLut, PipelineOptimization::None);
    }

    #[divan::bench]
    fn collapse(bencher: Bencher) {
        bench_gamut(
            bencher,
            &Scenario::ShaperToLut,
            PipelineOptimization::Collapse,
        );
    }

    #[divan::bench]
    fn precalculate(bencher: Bencher) {
        bench_gamut(
            bencher,
            &Scenario::ShaperToLut,
            PipelineOptimization::Precalculate,
        );
    }

    #[divan::bench]
    fn lcms2_optimized(bencher: Bencher) {
        bench_lcms2(bencher, &Scenario::ShaperToLut, 0);
    }

    #[divan::bench]
    fn lcms2_baseline(bencher: Bencher) {
        bench_lcms2(bencher, &Scenario::ShaperToLut, super::FLAGS_NOOPTIMIZE);
    }
}

/// The cost the [`PipelineOptimization::Precalculate`] tier moves to build time: resampling a
/// 33³ grid means 35 937 pipeline evaluations before the first pixel. Timed separately so the
/// tradeoff — a one-off build cost against a per-pixel saving — is visible rather than hidden
/// inside the other bars' setup.
#[divan::bench(args = [PipelineOptimization::None, PipelineOptimization::Precalculate])]
fn build_shaper_pair(bencher: Bencher, level: PipelineOptimization) {
    let (src, _) = reopen(&srgb());
    let (dst, _) = reopen(&display_p3_srgb_trc());
    bencher.bench_local(|| {
        IccTransform::between(
            black_box(&src),
            &dst,
            TransformOptions {
                intent: RenderingIntent::MediaRelativeColorimetric,
                black_point_compensation: false,
                optimization: level,
                prefer_cicp_transfer: false,
            },
        )
        .expect("the benchmark profiles link")
    });
}

/// `TransformOptions::prefer_cicp_transfer` (issue #555): the per-pixel cost of evaluating a
/// `cicp`-tagged profile's transfer in closed form against interpolating its TRC tags. The
/// pairs are `gamut-icc`'s built-ins, which carry the tag: sRGB (a `parametricCurveType`)
/// and BT.2100 PQ (a 1024-entry `curveType`), in both directions so each end of the link is
/// timed as a forward curve and as an inverse. `args` is the option.
fn bench_cicp(bencher: Bencher, src: BuiltinProfile, dst: BuiltinProfile, prefer_cicp: bool) {
    let src = IccProfile::builtin(src).expect("built-in");
    let dst = IccProfile::builtin(dst).expect("built-in");
    let transform = IccTransform::between(
        &src,
        &dst,
        TransformOptions {
            intent: RenderingIntent::MediaRelativeColorimetric,
            prefer_cicp_transfer: prefer_cicp,
            ..TransformOptions::default()
        },
    )
    .expect("the built-ins link");
    let input = gradient(3);
    let mut output = vec![0u8; pixels() * 3];
    bencher.counter(ItemsCount::new(pixels())).bench_local(|| {
        transform_interleaved_u8(
            &transform,
            PixelFormat::Rgb8,
            black_box(&input),
            PixelFormat::Rgb8,
            &mut output,
        )
        .expect("the buffers match the transform");
    });
}

mod cicp_transfer {
    use super::{Bencher, BuiltinProfile, bench_cicp};

    #[divan::bench(args = [false, true])]
    fn srgb_to_pq(bencher: Bencher, prefer_cicp: bool) {
        bench_cicp(
            bencher,
            BuiltinProfile::Srgb,
            BuiltinProfile::Bt2020Pq,
            prefer_cicp,
        );
    }

    #[divan::bench(args = [false, true])]
    fn pq_to_srgb(bencher: Bencher, prefer_cicp: bool) {
        bench_cicp(
            bencher,
            BuiltinProfile::Bt2020Pq,
            BuiltinProfile::Srgb,
            prefer_cicp,
        );
    }
}
