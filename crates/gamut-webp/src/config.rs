//! Encoder configuration: lossless vs. lossy selection, the quality knob, the compression effort
//! ladder, and the `cwebp -z` lossless presets.

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
/// highly detailed content the records no longer fit, and the encode fails with an
/// [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error rather than emitting an
/// unreadable file — the same
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

/// The default [`WebpConfig::quality`], `75` — libwebp's `WebPConfigInit` default. In lossless mode
/// it is the search budget the VP8L encoder ran at before `quality` reached it (issue #1240), and
/// the budget a lossy encode's `ALPH` plane is still searched at.
pub(crate) const DEFAULT_QUALITY: u8 = 75;

/// A lossless compression preset — libwebp's `WebPConfigLosslessPreset` levels, `cwebp -z 0..=9`.
///
/// Each level sets the two dials libwebp ties to it, [`effort`](Self::effort) (its `method`) and
/// [`quality`](Self::quality), from fastest ([`Z0`](Self::Z0)) to densest ([`Z9`](Self::Z9)); apply
/// one with [`WebpConfig::with_lossless_preset`]. The pairs are libwebp's own table:
///
/// | Level | Effort | Quality |
/// | --- | --- | --- |
/// | `Z0` | 0 | 0 |
/// | `Z1` | 1 | 20 |
/// | `Z2` | 2 | 25 |
/// | `Z3` | 3 | 30 |
/// | `Z4` | 3 | 50 |
/// | `Z5` | 4 | 50 |
/// | `Z6` | 4 | 75 |
/// | `Z7` | 4 | 90 |
/// | `Z8` | 5 | 90 |
/// | `Z9` | 6 | 100 |
///
/// Neither dial decreases from one level to the next, and gamut's lossless search is monotone in
/// both (see [`WebpConfig::quality`]), so a higher level never codes an image larger than a lower
/// one. [`Z6`](Self::Z6) is the default configuration's search.
///
/// The discriminants **are** the `-z` levels and are a permanent, append-only part of the
/// contract: they are what [`level`](Self::level) and [`from_level`](Self::from_level) round trip,
/// and what a numeric CLI or FFI knob carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum LosslessPreset {
    /// `-z 0` — effort 0, quality 0: the fastest.
    Z0 = 0,
    /// `-z 1` — effort 1, quality 20.
    Z1 = 1,
    /// `-z 2` — effort 2, quality 25.
    Z2 = 2,
    /// `-z 3` — effort 3, quality 30.
    Z3 = 3,
    /// `-z 4` — effort 3, quality 50.
    Z4 = 4,
    /// `-z 5` — effort 4, quality 50.
    Z5 = 5,
    /// `-z 6` — effort 4, quality 75: the default configuration's search (`cwebp`'s default `-z`).
    Z6 = 6,
    /// `-z 7` — effort 4, quality 90.
    Z7 = 7,
    /// `-z 8` — effort 5, quality 90.
    Z8 = 8,
    /// `-z 9` — effort 6, quality 100: the densest.
    Z9 = 9,
}

impl LosslessPreset {
    /// The `(effort, quality)` pair each level selects, indexed by level — libwebp's
    /// `kLosslessPresets` (`src/enc/config_enc.c`).
    const TABLE: [(Effort, u8); 10] = [
        (Effort::Fastest, 0),
        (Effort::Faster, 20),
        (Effort::Fast, 25),
        (Effort::Moderate, 30),
        (Effort::Moderate, 50),
        (Effort::Default, 50),
        (Effort::Default, 75),
        (Effort::Default, 90),
        (Effort::Slower, 90),
        (Effort::Slowest, 100),
    ];

    /// The `-z` level (`0..=9`) this variant selects.
    #[must_use]
    pub const fn level(self) -> u8 {
        self as u8
    }

    /// The [`LosslessPreset`] for a `-z` level, or `None` if `level` is outside `0..=9`.
    ///
    /// The inverse of [`LosslessPreset::level`].
    #[must_use]
    pub const fn from_level(level: u8) -> Option<Self> {
        Some(match level {
            0 => Self::Z0,
            1 => Self::Z1,
            2 => Self::Z2,
            3 => Self::Z3,
            4 => Self::Z4,
            5 => Self::Z5,
            6 => Self::Z6,
            7 => Self::Z7,
            8 => Self::Z8,
            9 => Self::Z9,
            _ => return None,
        })
    }

    /// The compression [`Effort`] (libwebp's `method`) this level sets.
    #[must_use]
    pub const fn effort(self) -> Effort {
        Self::TABLE[self as usize].0
    }

    /// The lossless [`quality`](WebpConfig::quality) this level sets.
    #[must_use]
    pub const fn quality(self) -> u8 {
        Self::TABLE[self as usize].1
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
    /// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if
    /// `strength` exceeds `99`.
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
/// as they ship, so match with a wildcard arm. The discriminants are permanent and append-only, so
/// the mode crosses a C or FFI boundary as its `u8`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
#[repr(u8)]
pub enum WebpMode {
    /// VP8L lossless coding — the input is reproduced bit-exactly (the default; gamut's M0 path).
    #[default]
    Lossless = 0,
    /// VP8 lossy coding — smaller output at a quality/size tradeoff set by [`WebpConfig::quality`].
    Lossy = 1,
}

/// What the encoder does with the colour under fully transparent (`alpha = 0`) pixels — libwebp's
/// `WebPConfig::exact` (cwebp's `-exact`, libvips' `exact`), named for what it preserves.
///
/// An `alpha = 0` pixel is invisible once composited, but its RGB still costs bits. **Cleaning**
/// rewrites it so it codes cheaply, as libwebp does when `exact` is off: a lossy encode runs a port
/// of libwebp's `WebPCleanupTransparentArea` on the YUV planes (each fully transparent 8×8 block is
/// flattened, and the transparent samples of a partly visible block take the mean luma of its
/// visible ones); a lossless encode sets every `alpha = 0` pixel to transparent black
/// (`WebPReplaceTransparentPixels(pic, 0x000000)`). No pixel with `alpha > 0` is ever rewritten.
/// **Preserving** codes the colour as given, so a lossless decode returns it bit-exactly.
///
/// The default, [`Exact::ByMode`], resolves per [`WebpMode`] (see [`preserves`](Self::preserves)):
/// **clean for [`WebpMode::Lossy`]**, where nothing exact is promised — libwebp's own default — and
/// **preserve for [`WebpMode::Lossless`]**, so a lossless encode still reproduces every input byte,
/// including the colour an `Rgba8` decode exposes under `alpha = 0`. That lossless default departs
/// from libwebp, whose `exact = 0` default cleans lossless input too.
///
/// Two consequences worth knowing. Cleaning changes only invisible samples, but VP8's transform and
/// loop filter mix neighbours, so a cleaned lossy file's visible pixels may decode marginally
/// differently from an uncleaned one, exactly as libwebp's do. And with `exact` off libwebp's
/// lossless encoder additionally zeroes the predictor residual of transparent pixels inside VP8L,
/// which gamut does not yet do (issue #1282), so a cleaned lossless file can still be larger
/// than libwebp's.
///
/// The discriminants are permanent and append-only, so the setting crosses a C or FFI boundary as
/// its `u8`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(u8)]
pub enum Exact {
    /// Clean for [`WebpMode::Lossy`], preserve for [`WebpMode::Lossless`] (the default).
    #[default]
    ByMode = 0,
    /// Preserve the colour under transparent pixels in either mode — libwebp's `exact = 1`.
    Preserve = 1,
    /// Clean the colour under transparent pixels in either mode — libwebp's `exact = 0`.
    Clean = 2,
}

impl Exact {
    /// Whether an encode in `mode` codes the colour under `alpha = 0` pixels as given (`true`) or
    /// cleans it first (`false`).
    #[must_use]
    pub const fn preserves(self, mode: WebpMode) -> bool {
        match self {
            Self::ByMode => matches!(mode, WebpMode::Lossless),
            Self::Preserve => true,
            Self::Clean => false,
        }
    }
}

/// Configuration for a [`WebpEncoder`](crate::WebpEncoder).
///
/// `quality` ranges `0..=100` and drives both modes: for [`WebpMode::Lossy`] it is the usual quality
/// factor (higher = larger output, closer to the source); for [`WebpMode::Lossless`], which
/// reproduces the input exactly either way, it is libwebp's lossless search budget alongside
/// [`effort`](Self::effort) (higher = smaller output, slower).
/// [`with_lossless_preset`](Self::with_lossless_preset) sets both the way `cwebp -z` does.
///
/// `#[non_exhaustive]`: the configuration is an open set — fields for deferred encoder knobs are
/// added as they ship. Start from [`WebpConfig::default`] (or the snapshot
/// [`WebpEncoder::config`](crate::WebpEncoder::config) returns), set the fields you want, and build
/// an encoder from it with [`WebpEncoder::from_config`](crate::WebpEncoder::from_config):
///
/// ```
/// use gamut_webp::{Effort, WebpConfig, WebpEncoder, WebpMode};
///
/// let mut config = WebpConfig::default();
/// config.mode = WebpMode::Lossy;
/// config.quality = 60;
/// config.effort = Effort::Slowest;
/// assert_eq!(WebpEncoder::from_config(config).config(), config);
/// ```
///
/// `Copy`: every knob here is plain scalar data; the owned payloads (ICC profiles, Exif/XMP) live
/// on [`WebpEncoder`](crate::WebpEncoder) rather than in the config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct WebpConfig {
    /// The bitstream mode to encode.
    pub mode: WebpMode,
    /// Quality, `0..=100`. Values above `100` behave as `100` (the encoder clamps silently) — a
    /// frozen contract.
    ///
    /// **Lossless.** The search budget, as libwebp's `quality` is for `lossless = 1` (`cwebp
    /// -lossless -q N`): within each [`effort`](Self::effort) level, a higher quality races more
    /// candidate encodings — deeper LZ77 hash chains and more colour-cache sizes — and keeps the
    /// smallest. Every candidate offered at a quality is offered at every higher one, so output
    /// size is **non-increasing in quality** as it is in effort, by construction; the decoded
    /// pixels never change. The default `75` is the search the encoder ran before quality reached
    /// it, byte for byte; below it candidates drop out (at `0` an effort level keeps only its
    /// transform, palette and grouping candidates), above it more join (at `90` and at `100`).
    /// gamut's candidates are its own, so a quality buys the same *kind* of work it buys libwebp,
    /// not the same bytes.
    ///
    /// **Lossy.** The quality factor (higher = larger, closer to the source). It selects the VP8 base quantizer index along libwebp's `QualityToCompression` curve, so a
    /// given `quality` here means what the same `quality` means to libwebp, cwebp's `-q` and
    /// libvips' `Q`: with `c = quality / 100`, `linear_c = c·2/3` below `c = 0.75` and `2c − 1` from
    /// there up, and the index is `⌊127·(1 − linear_c^(1/3))⌋` — `127` at `0`, `26` at the default
    /// `75`, `0` at `100`. That is libwebp's base quantizer with spatial noise shaping off; gamut
    /// codes the whole frame at it and does not yet modulate it per segment the way libwebp's SNS
    /// does (deferred, issue #1017).
    pub quality: u8,
    /// Compression effort (libwebp's `method`, `0..=6`). Applies to **both** modes; it never
    /// changes the decoded pixels of a lossless encode nor the quality target of a lossy one,
    /// only how hard the encoder searches and therefore how long it takes and how small the
    /// result is.
    pub effort: Effort,
    /// Near-lossless preprocessing strength, or `None` (the default) for off — bit-exact.
    ///
    /// Applies to [`WebpMode::Lossless`] only, and is ignored for [`WebpMode::Lossy`].
    pub near_lossless: Option<NearLossless>,
    /// Alpha quality, `0..=100` (libwebp's `alpha_quality`, cwebp's `-alpha_q`, libvips' `alpha_q`);
    /// the default `100` stores alpha losslessly. Values above `100` behave as `100`, as for
    /// [`quality`](Self::quality).
    ///
    /// Applies to [`WebpMode::Lossy`] only, where a transparent image carries its alpha in an
    /// `ALPH` chunk; a [`WebpMode::Lossless`] encode carries alpha inside the VP8L bitstream,
    /// bit-exactly, and ignores it. Below `100` the alpha plane is reduced to at most
    /// `2 + quality / 5` distinct values up to `70` (`2` at `0`, `16` at `70`) and
    /// `16 + (quality − 70) · 8` above it (`248` at `99`) — libwebp's own mapping and its own
    /// k-means level quantizer, which keeps the plane's minimum and maximum — and the `ALPH`
    /// header records the level reduction (RFC 9649 §2.7.1, `P = 1`). The reduced plane is then
    /// coded losslessly, so the decoded alpha is exactly the quantized plane — byte for byte the
    /// one libwebp produces at the same `alpha_quality`.
    ///
    /// Fewer levels usually, not always, mean fewer bytes. Grainy soft-edged masks shrink steadily
    /// as the quality falls; but at high settings (more levels than the mask has structure) the size
    /// can wander slightly either side of lossless, and a perfectly smooth ramp, which the alpha
    /// predictors already code almost for free, can grow when quantized into steps. libwebp shares
    /// both behaviours.
    pub alpha_quality: u8,
    /// Whether the colour under fully transparent pixels is preserved or cleaned before coding
    /// (libwebp's `exact`). The default [`Exact::ByMode`] cleans a [`WebpMode::Lossy`] encode and
    /// preserves a [`WebpMode::Lossless`] one; see [`Exact`].
    pub exact: Exact,
}

impl Default for WebpConfig {
    fn default() -> Self {
        Self {
            mode: WebpMode::Lossless,
            quality: DEFAULT_QUALITY,
            effort: Effort::Default,
            near_lossless: None,
            alpha_quality: 100,
            exact: Exact::ByMode,
        }
    }
}

impl WebpConfig {
    /// Applies a lossless preset the way libwebp's `WebPConfigLosslessPreset` does (`cwebp -z N`):
    /// selects [`WebpMode::Lossless`] and sets [`effort`](Self::effort) and
    /// [`quality`](Self::quality) to the preset's pair, leaving every other field as it was.
    ///
    /// ```
    /// use gamut_webp::{Effort, LosslessPreset, WebpConfig, WebpMode};
    ///
    /// let config = WebpConfig::default().with_lossless_preset(LosslessPreset::Z9);
    /// assert_eq!(config.mode, WebpMode::Lossless);
    /// assert_eq!((config.effort, config.quality), (Effort::Slowest, 100));
    /// ```
    #[must_use]
    pub const fn with_lossless_preset(mut self, preset: LosslessPreset) -> Self {
        self.mode = WebpMode::Lossless;
        self.effort = preset.effort();
        self.quality = preset.quality();
        self
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
        assert_eq!(c.alpha_quality, 100, "alpha is lossless by default");
        assert_eq!(c.exact, Exact::ByMode);
        assert_eq!(Exact::default(), Exact::ByMode);
        assert_eq!(WebpMode::default(), WebpMode::Lossless);
    }

    /// The per-mode default is libwebp's for lossy (clean) and bit-exact for lossless (preserve);
    /// the explicit settings apply in both modes alike.
    #[test]
    fn exact_resolves_per_mode() {
        for (exact, lossless, lossy) in [
            (Exact::ByMode, true, false),
            (Exact::Preserve, true, true),
            (Exact::Clean, false, false),
        ] {
            assert_eq!(exact.preserves(WebpMode::Lossless), lossless, "{exact:?}");
            assert_eq!(exact.preserves(WebpMode::Lossy), lossy, "{exact:?}");
        }
    }

    #[test]
    fn exact_discriminants_are_permanent() {
        assert_eq!(Exact::ByMode as u8, 0);
        assert_eq!(Exact::Preserve as u8, 1);
        assert_eq!(Exact::Clean as u8, 2);
    }

    #[test]
    fn mode_discriminants_are_permanent() {
        assert_eq!(WebpMode::Lossless as u8, 0);
        assert_eq!(WebpMode::Lossy as u8, 1);
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
    fn lossless_preset_level_round_trips_over_the_full_range() {
        for level in 0..=9u8 {
            let preset = LosslessPreset::from_level(level).expect("0..=9 is in range");
            assert_eq!(preset.level(), level);
        }
        assert_eq!(LosslessPreset::Z0.level(), 0);
        assert_eq!(LosslessPreset::Z9.level(), 9);
        assert_eq!(LosslessPreset::from_level(10), None);
        assert_eq!(LosslessPreset::from_level(u8::MAX), None);
    }

    #[test]
    fn lossless_presets_follow_libwebps_table() {
        // libwebp's `kLosslessPresets`; `tests/oracle.rs` checks the same pairs against the linked
        // library's `WebPConfigLosslessPreset`.
        let table: Vec<(u8, u8)> = (0..=9u8)
            .map(|l| {
                let p = LosslessPreset::from_level(l).expect("0..=9");
                (p.effort().level(), p.quality())
            })
            .collect();
        assert_eq!(
            table,
            [
                (0, 0),
                (1, 20),
                (2, 25),
                (3, 30),
                (3, 50),
                (4, 50),
                (4, 75),
                (4, 90),
                (5, 90),
                (6, 100)
            ]
        );
    }

    #[test]
    fn with_lossless_preset_sets_mode_effort_and_quality_only() {
        let base = WebpConfig {
            mode: WebpMode::Lossy,
            near_lossless: NearLossless::from_libwebp_strength(40),
            alpha_quality: 41,
            exact: Exact::Clean,
            ..WebpConfig::default()
        };
        let got = base.with_lossless_preset(LosslessPreset::Z3);
        assert_eq!(
            got,
            WebpConfig {
                mode: WebpMode::Lossless,
                effort: Effort::Moderate,
                quality: 30,
                ..base
            }
        );
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
