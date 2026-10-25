//! The conformance badge: the battery's transforms cross-checked against Little-CMS.
//!
//! Every [`PAIRS`] entry is linked by `gamut-cmm` ([`IccTransform::between`], `f64`, stage by
//! stage) and by lcms2 at its full-precision path (`TYPE_*_DBL`, `NOOPTIMIZE | NOCACHE`) — the
//! comparison `gamut-cmm`'s own conformance gate calls TIGHT — at all four intents, with and
//! without black-point compensation (none for ICC-absolute, which excludes it), over a device
//! grid. Both outputs are measured through the same lcms2 "lens" (destination device → Lab,
//! media-relative), so the reported figure is a ΔE₀₀ between two renderings of one colour, not
//! a distance in device units. lcms2's unbounded float output is clamped to the device range
//! first, as `gamut-cmm`'s gate does (`clamp01` there).
//!
//! Native only: lcms2 is C, statically linked by the dev-only `tooling/lcms2-oracle`.

use gamut_cmm::{IccTransform, Transform, TransformOptions};
use gamut_color::lab::delta_e_2000;
use gamut_icc::{IccProfile, RenderingIntent};
use lcms2_oracle::{
    FLAGS_BLACKPOINTCOMPENSATION, FLAGS_NOCACHE, FLAGS_NOOPTIMIZE, INTENT_ABSOLUTE_COLORIMETRIC,
    INTENT_PERCEPTUAL, INTENT_RELATIVE_COLORIMETRIC, INTENT_SATURATION, Profile, TYPE_CMYK_DBL,
    TYPE_Lab_DBL, TYPE_RGB_DBL, Transform as LcmsTransform, lab4, set_quiet_log_handler,
};

use crate::profiles::{Battery, ProfileKind};

/// The badge's pass threshold, max ΔE₀₀: `gamut-cmm`'s own TIGHT LUT bound
/// (`TIGHT_LUT_BOUND` in `crates/gamut-cmm/tests/oracle_conformance.rs`), the bound that crate
/// holds its colour error to against lcms2's full-precision path.
pub const BOUND: f64 = 2e-2;

/// The source → destination pairs checked: every path the panels render — each source into the
/// v4 display sRGB, each RGB source into the printer — plus Display P3 as a destination, so a
/// parametric-TRC inverse other than the display's is covered too.
///
/// One pair is deliberately absent: the printer into the **v2** sRGB (its 1024-entry sampled
/// TRC inverted numerically), which no panel renders and which measures 3.51e-2 at ICC-absolute
/// — over [`BOUND`]. #1003 traced the excess to lcms2, which evaluates the reversed sampled TRC
/// in 16 bits even in a double-precision transform, not to `gamut-cmm`; that crate gates the
/// pair under its own class bound (`TIGHT_SAMPLED_TRC_LUT_BOUND`, 8e-2), and this badge keeps
/// the plain LUT bound rather than loosening it for one pair no panel renders.
pub const PAIRS: [(ProfileKind, ProfileKind); 8] = [
    (ProfileKind::SrgbV2, ProfileKind::SrgbV4),
    (ProfileKind::SrgbV2, ProfileKind::DisplayP3),
    (ProfileKind::DisplayP3, ProfileKind::SrgbV4),
    (ProfileKind::CameraA, ProfileKind::SrgbV4),
    (ProfileKind::CameraB, ProfileKind::SrgbV4),
    (ProfileKind::SrgbV2, ProfileKind::Printer),
    (ProfileKind::DisplayP3, ProfileKind::Printer),
    (ProfileKind::Printer, ProfileKind::SrgbV4),
];

/// Grid steps per device axis (`STEPS³` RGB or `STEPS⁴` CMYK samples per cell).
pub const STEPS: usize = 6;

/// One (pair, intent, BPC) cell's result.
#[derive(Debug, Clone)]
pub struct Cell {
    /// The source profile.
    pub src: ProfileKind,
    /// The destination profile.
    pub dst: ProfileKind,
    /// The rendering intent.
    pub intent: RenderingIntent,
    /// Whether BPC was requested.
    pub bpc: bool,
    /// The worst ΔE₀₀ between the two engines over the grid.
    pub max_de00: f64,
}

/// The whole cross-check.
#[derive(Debug, Clone)]
pub struct Report {
    /// Every cell, in run order.
    pub cells: Vec<Cell>,
}

impl Report {
    /// The worst cell, if any ran.
    #[must_use]
    pub fn worst(&self) -> Option<&Cell> {
        self.cells
            .iter()
            .max_by(|a, b| a.max_de00.total_cmp(&b.max_de00))
    }

    /// The worst ΔE₀₀ over every cell (0 when none ran).
    #[must_use]
    pub fn max_de00(&self) -> f64 {
        self.worst().map_or(0.0, |c| c.max_de00)
    }

    /// Whether every cell is under [`BOUND`].
    #[must_use]
    pub fn passes(&self) -> bool {
        !self.cells.is_empty() && self.max_de00() < BOUND
    }
}

/// The intents, paired with lcms2's codes.
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

/// lcms2's double formatter for a channel count, and its scale: CMYK `_DBL` carries ink
/// percentages (`0..=100`), RGB unit values.
fn format(channels: usize) -> (u32, f64) {
    if channels == 4 {
        (TYPE_CMYK_DBL, 100.0)
    } else {
        (TYPE_RGB_DBL, 1.0)
    }
}

/// Every grid point of a `channels`-dimensional unit cube at [`STEPS`] steps, interleaved.
fn grid(channels: usize) -> Vec<f64> {
    let total = STEPS.pow(u32::try_from(channels).unwrap_or(0));
    let last = (STEPS - 1) as f64;
    let mut out = Vec::with_capacity(total * channels);
    for i in 0..total {
        let mut rest = i;
        for _ in 0..channels {
            out.push((rest % STEPS) as f64 / last);
            rest /= STEPS;
        }
    }
    out
}

/// Runs the cross-check over `battery`.
///
/// # Errors
///
/// A message naming the pair when a battery profile did not parse, lcms2 cannot read its bytes,
/// or `gamut-cmm` refuses to link a pair lcms2 links — every one a failure of the badge.
pub fn run(battery: &Battery) -> Result<Report, String> {
    set_quiet_log_handler();
    let lab = lab4();
    let mut cells = Vec::new();
    for (src_kind, dst_kind) in PAIRS {
        let load = |kind: ProfileKind| -> Result<(&IccProfile, Profile), String> {
            let entry = battery.get(kind);
            let ours = entry
                .profile()
                .ok_or_else(|| format!("{}: did not parse", kind.name()))?;
            let theirs = Profile::from_bytes(&entry.bytes)
                .ok_or_else(|| format!("{}: lcms2 cannot read it", kind.name()))?;
            Ok((ours, theirs))
        };
        let (src, src_lcms) = load(src_kind)?;
        let (dst, dst_lcms) = load(dst_kind)?;
        let src_channels = usize::from(src.header.data_color_space.channel_count());
        let dst_channels = usize::from(dst.header.data_color_space.channel_count());
        let (src_format, src_scale) = format(src_channels);
        let (dst_format, dst_scale) = format(dst_channels);
        let lens = LcmsTransform::new(
            &dst_lcms,
            dst_format,
            &lab,
            TYPE_Lab_DBL,
            INTENT_RELATIVE_COLORIMETRIC,
            FLAGS_NOCACHE,
        );
        let input = grid(src_channels);
        let pixels = input.len() / src_channels;
        let scaled_input: Vec<f64> = input.iter().map(|v| v * src_scale).collect();
        for (intent, lcms_intent) in INTENTS {
            for bpc in [false, true] {
                if bpc && intent == RenderingIntent::IccAbsoluteColorimetric {
                    continue;
                }
                let options = TransformOptions {
                    intent,
                    black_point_compensation: bpc,
                    ..TransformOptions::default()
                };
                let pair = format!("{} → {}", src_kind.name(), dst_kind.name());
                let ours = IccTransform::between(src, dst, options)
                    .map_err(|e| format!("{pair}: gamut-cmm: {e}"))?;
                let mut ours_out = vec![0.0; pixels * dst_channels];
                ours.transform(&input, &mut ours_out)
                    .map_err(|e| format!("{pair}: gamut-cmm: {e}"))?;
                let ours_scaled: Vec<f64> = ours_out.iter().map(|v| v * dst_scale).collect();
                let bpc_flag = if bpc { FLAGS_BLACKPOINTCOMPENSATION } else { 0 };
                let theirs = LcmsTransform::new(
                    &src_lcms,
                    src_format,
                    &dst_lcms,
                    dst_format,
                    lcms_intent,
                    FLAGS_NOOPTIMIZE | FLAGS_NOCACHE | bpc_flag,
                );
                // lcms2's float path is unbounded (an out-of-gamut colour leaves `[0, 1]`);
                // `gamut-cmm` clamps device values, as its own gate clamps the oracle's.
                let theirs_out: Vec<f64> = theirs
                    .apply_f64(&scaled_input, pixels, dst_channels)
                    .into_iter()
                    .map(|v| v.clamp(0.0, dst_scale))
                    .collect();
                let ours_lab = lens.apply_f64(&ours_scaled, pixels, 3);
                let theirs_lab = lens.apply_f64(&theirs_out, pixels, 3);
                let max_de00 = ours_lab
                    .as_chunks::<3>()
                    .0
                    .iter()
                    .zip(theirs_lab.as_chunks::<3>().0)
                    .map(|(a, b)| delta_e_2000(*a, *b))
                    .fold(0.0, f64::max);
                cells.push(Cell {
                    src: src_kind,
                    dst: dst_kind,
                    intent,
                    bpc,
                    max_de00,
                });
            }
        }
    }
    Ok(Report { cells })
}
