//! Encoder configuration: lossless vs. lossy selection, the quality knob, and the compression
//! effort ladder.

/// Compression effort: the encode-time/output-size trade-off, from fastest ([`Effort::Fastest`])
/// to densest ([`Effort::Slowest`]).
///
/// Maps one-to-one onto libwebp's `WebPConfig::method` levels `0..=6` (`cwebp -m N`): higher
/// effort spends more time searching for a smaller file at the **same** decoded quality. Effort
/// never changes what the format guarantees — a lossless encode stays bit-exact at every level,
/// and a lossy encode keeps its [`quality`](WebpConfig::quality) target — so it is a free choice
/// that only trades time for size.
///
/// The default is [`Effort::Default`] (level 4), matching libwebp's `WebPConfigInit`.
///
/// One interaction is worth knowing about on very large lossy frames. Levels 1 and up emit VP8's
/// 4x4 (`B_PRED`) modes, whose per-macroblock records fill the control partition, and that
/// partition's size field is only 19 bits (RFC 6386 §9.1). Past roughly twelve megapixels of
/// highly detailed content the records no longer fit, and the encode fails with
/// [`Error::InvalidInput`](gamut_core::Error) rather than emitting an unreadable file — the same
/// condition libwebp reports as `VP8_ENC_ERROR_PARTITION0_OVERFLOW`. [`Effort::Fastest`] never
/// emits `B_PRED`, so it covers the whole canvas range (up to 16383x16383) at any detail level.
/// Lossless encodes are unaffected: VP8L has no such field.
///
/// The discriminants **are** the libwebp method numbers and are a permanent, append-only part of
/// the contract: they are what [`level`](Self::level) and [`from_level`](Self::from_level) round
/// trip, and what a numeric CLI or FFI knob carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(u8)]
pub enum Effort {
    /// Level 0 — the fastest, least dense setting.
    Fastest = 0,
    /// Level 1.
    Faster = 1,
    /// Level 2.
    Fast = 2,
    /// Level 3 — one step quicker than the default.
    Moderate = 3,
    /// Level 4 — libwebp's default `method`, the balanced speed/density point.
    #[default]
    Default = 4,
    /// Level 5.
    Slower = 5,
    /// Level 6 — the slowest, densest setting.
    Slowest = 6,
}

impl Effort {
    /// The libwebp `method` level (`0..=6`) this variant selects.
    #[must_use]
    pub const fn level(self) -> u8 {
        self as u8
    }

    /// The [`Effort`] for a libwebp `method` level, or `None` if `level` is outside `0..=6`.
    ///
    /// The inverse of [`Effort::level`]; handy for wiring up a numeric CLI flag.
    #[must_use]
    pub const fn from_level(level: u8) -> Option<Self> {
        Some(match level {
            0 => Self::Fastest,
            1 => Self::Faster,
            2 => Self::Fast,
            3 => Self::Moderate,
            4 => Self::Default,
            5 => Self::Slower,
            6 => Self::Slowest,
            _ => return None,
        })
    }
}

/// The strength of near-lossless preprocessing.
///
/// Near-lossless is **not a bitstream mode**: the encoder still emits a conformant VP8L stream that
/// decodes bit-exactly. What it changes is the *input* — every colour channel is rounded to a
/// coarser grid first, so the spatial predictor's residuals collapse onto multiples of that grid
/// and the entropy coder has a far smaller alphabet to carry.
///
/// The scale is libwebp's `near_lossless`, where `0` is maximum loss and larger values are gentler,
/// so a caller migrating from `cwebp -near_lossless N` gets what they expect. libwebp's `100` —
/// its "off" sentinel — is deliberately **rejected**: off is a distinct state
/// ([`WebpConfig::near_lossless`] is `None`), not a point on the strength scale, so a stray value
/// can never silently disable preprocessing the caller asked for, and an unset `0` can never
/// silently mean *maximum* loss. [`from_libwebp_strength`](Self::from_libwebp_strength) maps the
/// raw libwebp value, sentinel included.
///
/// # What it guarantees
///
/// Red, green and blue move by at most [`max_deviation`](Self::max_deviation); **alpha is never
/// modified**, so transparency still round-trips bit-exactly. This diverges from libwebp, which
/// quantizes all four channels.
///
/// Setting a strength can never make the file **larger**: the encoder codes the image both with and
/// without preprocessing and keeps the smaller, so a gentle strength that would not have paid
/// simply falls back to the exact encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NearLossless(u8);

impl NearLossless {
    /// The highest strength value, one below libwebp's "off" sentinel.
    const MAX_STRENGTH: u8 = 99;

    /// Creates a strength on libwebp's scale, rejecting `100` (the "off" sentinel — use `None`)
    /// and anything above it.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidInput`](gamut_core::Error) if `strength` exceeds `99`.
    pub fn new(strength: u8) -> gamut_core::Result<Self> {
        if strength <= Self::MAX_STRENGTH {
            Ok(Self(strength))
        } else {
            Err(gamut_core::Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "WebP: near-lossless strength must be 0..=99 (100 means off — use None)",
            ))
        }
    }

    /// The wrapped strength on libwebp's scale.
    #[must_use]
    pub const fn get(self) -> u8 {
        self.0
    }

    /// Maps a raw libwebp `near_lossless` value: `100` and above is the "off" sentinel and yields
    /// `None`; `0..=99` yields the matching strength.
    #[must_use]
    pub const fn from_libwebp_strength(strength: u8) -> Option<Self> {
        if strength <= Self::MAX_STRENGTH {
            Some(Self(strength))
        } else {
            None
        }
    }

    /// The number of low bits this strength may discard per colour channel — libwebp's
    /// `5 - strength / 20`, so `0..=19` gives 5 bits and `80..=99` gives 1.
    #[must_use]
    pub const fn bits(self) -> u8 {
        5 - self.0 / 20
    }

    /// The maximum absolute deviation this strength may introduce in **red, green or blue**:
    /// half the quantization step, i.e. `1`, `2`, `4`, `8` or `16`. Alpha's deviation is always
    /// zero.
    #[must_use]
    pub const fn max_deviation(self) -> u16 {
        1u16 << (self.bits() - 1)
    }
}

/// Which WebP bitstream the encoder produces.
///
/// `#[non_exhaustive]`: modes are an open set — variants for deferred coding strategies are added
/// as they ship, so match with a wildcard arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum WebpMode {
    /// VP8L lossless coding — the input is reproduced bit-exactly (the default; gamut's M0 path).
    #[default]
    Lossless,
    /// VP8 lossy coding — smaller output at a quality/size tradeoff set by [`WebpConfig::quality`].
    Lossy,
}

/// Configuration for a [`WebpEncoder`](crate::WebpEncoder).
///
/// `quality` ranges `0..=100` and applies only to [`WebpMode::Lossy`], where it is the usual quality
/// factor (higher = larger output, closer to the source). [`WebpMode::Lossless`] reproduces the
/// input exactly and ignores `quality`. Build one with the [`WebpEncoder`](crate::WebpEncoder)
/// constructors and builders rather than by hand — they keep the fields consistent.
///
/// `#[non_exhaustive]`: the configuration is an open set — fields for deferred encoder knobs are
/// added as they ship. Read it as the snapshot returned by
/// [`WebpEncoder::config`](crate::WebpEncoder::config).
///
/// `Copy` is **retained** deliberately, unlike [`AvifConfig`](https://docs.rs/gamut-avif): the
/// owned payloads that forced AVIF to drop it (ICC profiles, Exif/XMP) already live on
/// [`WebpEncoder`](crate::WebpEncoder) rather than in the config, and every knob here is plain
/// scalar data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct WebpConfig {
    /// The bitstream mode to encode.
    pub mode: WebpMode,
    /// Lossy quality factor, `0..=100` (higher = larger, closer to the source). Ignored for
    /// lossless. Values above `100` behave as `100` (the encoder clamps silently) — a frozen
    /// contract.
    pub quality: u8,
    /// Compression effort (libwebp's `method`, `0..=6`). Applies to **both** modes; it never
    /// changes the decoded pixels of a lossless encode nor the quality target of a lossy one,
    /// only how hard the encoder searches and therefore how long it takes and how small the
    /// result is.
    pub effort: Effort,
    /// Near-lossless preprocessing strength, or `None` (the default) for off — bit-exact.
    ///
    /// Applies to [`WebpMode::Lossless`] only, and is ignored for [`WebpMode::Lossy`], exactly as
    /// [`quality`](Self::quality) is ignored for lossless.
    pub near_lossless: Option<NearLossless>,
}

impl Default for WebpConfig {
    fn default() -> Self {
        Self {
            mode: WebpMode::Lossless,
            quality: 75,
            effort: Effort::Default,
            near_lossless: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_lossless_quality_75() {
        let c = WebpConfig::default();
        assert_eq!(c.mode, WebpMode::Lossless);
        assert_eq!(c.quality, 75);
        assert_eq!(c.effort, Effort::Default);
        assert_eq!(c.near_lossless, None);
        assert_eq!(WebpMode::default(), WebpMode::Lossless);
    }

    #[test]
    fn near_lossless_rejects_the_off_sentinel_and_maps_libwebp_values() {
        // `100` is libwebp's "off"; representing it as a strength would let a stray value silently
        // disable preprocessing, and an unset `0` silently mean *maximum* loss.
        assert!(NearLossless::new(0).is_ok());
        assert!(NearLossless::new(99).is_ok());
        assert!(NearLossless::new(100).is_err());
        assert!(NearLossless::new(u8::MAX).is_err());
        assert_eq!(NearLossless::from_libwebp_strength(100), None);
        assert_eq!(NearLossless::from_libwebp_strength(u8::MAX), None);
        assert_eq!(
            NearLossless::from_libwebp_strength(60).map(NearLossless::get),
            Some(60)
        );
    }

    #[test]
    fn near_lossless_bits_follow_libwebps_table() {
        // The bits are the frozen half of the contract: the deviation bound derives from them, so
        // pin every boundary of `5 - strength / 20`.
        for (strength, bits) in [
            (0u8, 5u8),
            (19, 5),
            (20, 4),
            (39, 4),
            (40, 3),
            (59, 3),
            (60, 2),
            (79, 2),
            (80, 1),
            (99, 1),
        ] {
            let nl = NearLossless::new(strength).expect("in range");
            assert_eq!(nl.bits(), bits, "strength {strength}");
            assert_eq!(
                nl.max_deviation(),
                1u16 << (bits - 1),
                "strength {strength}"
            );
        }
    }

    #[test]
    fn effort_level_round_trips_over_the_full_range() {
        // The discriminants are the libwebp `method` numbers and are a permanent part of the
        // contract, so pin both directions across the whole range plus the rejected values.
        for level in 0..=6u8 {
            let effort = Effort::from_level(level).expect("0..=6 is in range");
            assert_eq!(effort.level(), level);
        }
        assert_eq!(Effort::Fastest.level(), 0);
        assert_eq!(Effort::Default.level(), 4);
        assert_eq!(Effort::Slowest.level(), 6);
        assert_eq!(Effort::from_level(7), None);
        assert_eq!(Effort::from_level(u8::MAX), None);
        assert_eq!(Effort::default(), Effort::Default);
    }
}
