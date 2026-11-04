//! Encoder configuration: the [`Effort`] speed/density dial, the [`ModularMode`] coding-tool
//! selector, the [`FrameSettings`] progression and decoding-speed overrides (over [`Toggle`],
//! [`ProgressiveDc`] and [`DecodingSpeed`]), the validated lossy [`Distance`] newtype, the output
//! [`Container`] selector, the [`ColorSpec`] colour signalling, and the internal [`Mode`] that makes
//! a lossless-with-a-distance state unrepresentable.

use gamut_core::{Error, Result};

/// Encoder effort: the speed/density trade-off, from fastest ([`Effort::Lightning`]) to densest
/// ([`Effort::Glacier`]).
///
/// Maps directly onto libjxl's `JXL_ENC_FRAME_SETTING_EFFORT` levels `1..=10`; higher effort spends
/// more time searching for a smaller file at the same quality. The named variants are libjxl's own
/// codenames. The default is [`Effort::Squirrel`] (level 7), matching libjxl's default.
///
/// libjxl's level 11 ("tectonic plate") is expert-gated behind `JxlEncoderAllowExpertOptions` and is
/// deliberately out of scope, so this enum caps at level 10.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Effort {
    /// Level 1 — the fastest, lowest-density setting.
    Lightning,
    /// Level 2.
    Thunder,
    /// Level 3.
    Falcon,
    /// Level 4.
    Cheetah,
    /// Level 5.
    Hare,
    /// Level 6.
    Wombat,
    /// Level 7 — libjxl's default, a balanced speed/density point.
    #[default]
    Squirrel,
    /// Level 8.
    Kitten,
    /// Level 9.
    Tortoise,
    /// Level 10 — the slowest, highest-density setting.
    Glacier,
}

impl Effort {
    /// The libjxl effort level (`1..=10`) this variant selects.
    #[must_use]
    pub fn level(self) -> u8 {
        match self {
            Self::Lightning => 1,
            Self::Thunder => 2,
            Self::Falcon => 3,
            Self::Cheetah => 4,
            Self::Hare => 5,
            Self::Wombat => 6,
            Self::Squirrel => 7,
            Self::Kitten => 8,
            Self::Tortoise => 9,
            Self::Glacier => 10,
        }
    }

    /// The [`Effort`] for a libjxl effort level, or `None` if `level` is outside `1..=10`.
    ///
    /// The inverse of [`Effort::level`]; handy for wiring up a numeric CLI flag.
    #[must_use]
    pub fn from_level(level: u8) -> Option<Self> {
        Some(match level {
            1 => Self::Lightning,
            2 => Self::Thunder,
            3 => Self::Falcon,
            4 => Self::Cheetah,
            5 => Self::Hare,
            6 => Self::Wombat,
            7 => Self::Squirrel,
            8 => Self::Kitten,
            9 => Self::Tortoise,
            10 => Self::Glacier,
            _ => return None,
        })
    }
}

/// Which of JPEG XL's two coding tools the encoder is told to use.
///
/// JPEG XL codes an image either with **VarDCT** — the variable-block-size DCT path, XYB colour and
/// perceptual quantisation, aimed at photographic material — or with **Modular**, the
/// predictor/transform path that also underpins lossless coding. Maps directly onto libjxl's
/// `JXL_ENC_FRAME_SETTING_MODULAR`, whose three states are `-1` (encoder chooses), `0` (VarDCT) and
/// `1` (Modular).
///
/// The default is [`ModularMode::Auto`], which leaves the choice to libjxl and emits exactly the
/// bytes gamut produced before this knob existed — the option is not sent at all.
///
/// Two things worth knowing before reaching for a forced mode:
///
/// - **Lossless is already Modular.** [`JxlEncoder::lossless`](crate::JxlEncoder::lossless) makes
///   libjxl select Modular regardless of this setting, so [`ModularMode::Modular`] is redundant
///   there and [`ModularMode::VarDct`] is a contradiction the encoder **rejects** rather than
///   silently ignore.
/// - **Forcing Modular usually costs rate on lossy photos.** Above roughly
///   [`Distance`] `0.5` the VarDCT path is the denser choice for photographic input. This knob
///   exists so a stream can be produced under a *deliberately chosen* coding tool — for
///   comparison, or for synthetic/screenshot-like material Modular suits — not as a quality dial.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ModularMode {
    /// Let libjxl choose the coding tool (libjxl option value `-1`); the default.
    #[default]
    Auto,
    /// Force the VarDCT path (libjxl option value `0`).
    VarDct,
    /// Force the Modular path (libjxl option value `1`).
    Modular,
}

impl ModularMode {
    /// The libjxl `JXL_ENC_FRAME_SETTING_MODULAR` value (`-1`, `0` or `1`) this variant selects.
    #[must_use]
    pub fn option_value(self) -> i32 {
        match self {
            Self::Auto => -1,
            Self::VarDct => 0,
            Self::Modular => 1,
        }
    }

    /// The [`ModularMode`] for a libjxl option value, or `None` if `value` is outside `-1..=1`.
    ///
    /// The inverse of [`ModularMode::option_value`]; handy for wiring up a numeric CLI flag.
    #[must_use]
    pub fn from_option_value(value: i32) -> Option<Self> {
        Some(match value {
            -1 => Self::Auto,
            0 => Self::VarDct,
            1 => Self::Modular,
            _ => return None,
        })
    }
}

/// A libjxl on/off frame setting that can also be left to the encoder.
///
/// Several libjxl frame settings are tri-state: `-1` lets the encoder decide, `0` turns the feature
/// off and `1` turns it on. [`Toggle::EncoderDefault`] — the default — leaves the setting
/// **unsent**, so libjxl's own choice applies and the stream is byte-identical to one encoded
/// without naming the knob at all. Used by [`FrameSettings`] for the AC progressions and
/// responsive mode.
///
/// The discriminants are permanent and append-only (a C caller may name them as constants); they
/// are not libjxl's option values — [`Toggle::option_value`] is the mapping.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Toggle {
    /// Leave the setting unsent so libjxl chooses; the default.
    #[default]
    EncoderDefault = 0,
    /// Turn the feature off (libjxl option value `0`).
    Off = 1,
    /// Turn the feature on (libjxl option value `1`).
    On = 2,
}

impl Toggle {
    /// The libjxl option value this variant sends, or `None` for [`Toggle::EncoderDefault`], which
    /// sends nothing.
    #[must_use]
    pub fn option_value(self) -> Option<i32> {
        match self {
            Self::EncoderDefault => None,
            Self::Off => Some(0),
            Self::On => Some(1),
        }
    }
}

/// libjxl's progressive-DC frame setting (`JXL_ENC_FRAME_SETTING_PROGRESSIVE_DC`): how many
/// lower-resolution DC passes a VarDCT frame carries ahead of the full-resolution data.
///
/// [`ProgressiveDc::EncoderDefault`] — the default — leaves the setting **unsent**, so libjxl's own
/// choice applies. The discriminants are permanent and append-only; [`ProgressiveDc::option_value`]
/// is the mapping onto libjxl's values.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ProgressiveDc {
    /// Leave the setting unsent so libjxl chooses; the default.
    #[default]
    EncoderDefault = 0,
    /// No lower-resolution DC pass (libjxl option value `0`).
    Off = 1,
    /// One extra 64×64-downscaled pass (libjxl option value `1`).
    OneLevel = 2,
    /// A 512×512- and a 64×64-downscaled pass (libjxl option value `2`).
    TwoLevels = 3,
}

impl ProgressiveDc {
    /// The libjxl option value this variant sends, or `None` for
    /// [`ProgressiveDc::EncoderDefault`], which sends nothing.
    #[must_use]
    pub fn option_value(self) -> Option<i32> {
        match self {
            Self::EncoderDefault => None,
            Self::Off => Some(0),
            Self::OneLevel => Some(1),
            Self::TwoLevels => Some(2),
        }
    }
}

/// libjxl's decoding-speed tier (`JXL_ENC_FRAME_SETTING_DECODING_SPEED`): trades density for a
/// faster decode, from [`DecodingSpeed::Tier0`] (slowest to decode, densest) to
/// [`DecodingSpeed::Tier4`] (fastest to decode).
///
/// [`DecodingSpeed::EncoderDefault`] — the default — leaves the setting **unsent**. libjxl's own
/// default is tier `0`, but that is libjxl's choice to make, so gamut does not restate it. The
/// discriminants are permanent and append-only; [`DecodingSpeed::option_value`] is the mapping
/// onto libjxl's tiers.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum DecodingSpeed {
    /// Leave the setting unsent so libjxl chooses; the default.
    #[default]
    EncoderDefault = 0,
    /// Tier `0`: slowest to decode, best quality for the size.
    Tier0 = 1,
    /// Tier `1`.
    Tier1 = 2,
    /// Tier `2`.
    Tier2 = 3,
    /// Tier `3`.
    Tier3 = 4,
    /// Tier `4`: fastest to decode, at some cost in quality for the size.
    Tier4 = 5,
}

impl DecodingSpeed {
    /// The libjxl tier (`0..=4`) this variant sends, or `None` for
    /// [`DecodingSpeed::EncoderDefault`], which sends nothing.
    #[must_use]
    pub fn option_value(self) -> Option<i32> {
        match self {
            Self::EncoderDefault => None,
            Self::Tier0 => Some(0),
            Self::Tier1 => Some(1),
            Self::Tier2 => Some(2),
            Self::Tier3 => Some(3),
            Self::Tier4 => Some(4),
        }
    }
}

/// The progression and decoding-speed frame settings, each left to libjxl unless set.
///
/// Plain `Copy` data, built from [`FrameSettings::default`] with the chainable `with_*` methods and
/// handed to [`JxlEncoder::with_frame_settings`](crate::JxlEncoder::with_frame_settings):
///
/// | Setting | libjxl frame setting | Mainly shapes |
/// | --- | --- | --- |
/// | [`progressive_ac`](Self::progressive_ac) | `PROGRESSIVE_AC` (spectral AC progression) | VarDCT |
/// | [`qprogressive_ac`](Self::qprogressive_ac) | `QPROGRESSIVE_AC` (quantized AC progression) | VarDCT |
/// | [`progressive_dc`](Self::progressive_dc) | `PROGRESSIVE_DC` (lower-resolution DC passes) | VarDCT |
/// | [`responsive`](Self::responsive) | `RESPONSIVE` (progressive Modular) | Modular parts |
/// | [`decoding_speed`](Self::decoding_speed) | `DECODING_SPEED` (tier `0..=4`) | both |
///
/// Every field defaults to its encoder-default variant, which leaves that setting **unsent**, so
/// the default value encodes exactly as an encoder that never named it. A set field is forwarded to
/// libjxl as-is, and how the settings combine is libjxl's behaviour, reproduced rather than
/// second-guessed: an AC progression also turns responsive mode on, and on a lossless (Modular)
/// frame additionally forces the quantized progression; responsive mode also governs the
/// Modular-coded parts of a lossy VarDCT frame.
///
/// [`Effort`] and [`ModularMode`] are libjxl frame settings too, but predate this struct and stay
/// separate builders on the encoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct FrameSettings {
    /// Spectral progression of the VarDCT AC coefficients.
    progressive_ac: Toggle,
    /// Quantization (least-significant-bit) progression of the VarDCT AC coefficients.
    qprogressive_ac: Toggle,
    /// Lower-resolution DC passes.
    progressive_dc: ProgressiveDc,
    /// Progressive (responsive) Modular encoding.
    responsive: Toggle,
    /// The decoding-speed tier.
    decoding_speed: DecodingSpeed,
}

// Not `const fn`: cargo-mutants' `Default::default()` body replacement is unviable in a const fn,
// which would leave every builder and getter here outside the mutation gate.
impl FrameSettings {
    /// Sets the spectral AC progression (`PROGRESSIVE_AC`). Returns the updated settings.
    #[must_use]
    pub fn with_progressive_ac(mut self, progressive_ac: Toggle) -> Self {
        self.progressive_ac = progressive_ac;
        self
    }

    /// Sets the quantization AC progression (`QPROGRESSIVE_AC`). Returns the updated settings.
    #[must_use]
    pub fn with_qprogressive_ac(mut self, qprogressive_ac: Toggle) -> Self {
        self.qprogressive_ac = qprogressive_ac;
        self
    }

    /// Sets the lower-resolution DC passes (`PROGRESSIVE_DC`). Returns the updated settings.
    #[must_use]
    pub fn with_progressive_dc(mut self, progressive_dc: ProgressiveDc) -> Self {
        self.progressive_dc = progressive_dc;
        self
    }

    /// Sets progressive (responsive) Modular encoding (`RESPONSIVE`). Returns the updated settings.
    #[must_use]
    pub fn with_responsive(mut self, responsive: Toggle) -> Self {
        self.responsive = responsive;
        self
    }

    /// Sets the decoding-speed tier (`DECODING_SPEED`). Returns the updated settings.
    #[must_use]
    pub fn with_decoding_speed(mut self, decoding_speed: DecodingSpeed) -> Self {
        self.decoding_speed = decoding_speed;
        self
    }

    /// The spectral AC progression setting.
    #[must_use]
    pub fn progressive_ac(&self) -> Toggle {
        self.progressive_ac
    }

    /// The quantization AC progression setting.
    #[must_use]
    pub fn qprogressive_ac(&self) -> Toggle {
        self.qprogressive_ac
    }

    /// The lower-resolution DC pass setting.
    #[must_use]
    pub fn progressive_dc(&self) -> ProgressiveDc {
        self.progressive_dc
    }

    /// The responsive (progressive Modular) setting.
    #[must_use]
    pub fn responsive(&self) -> Toggle {
        self.responsive
    }

    /// The decoding-speed tier setting.
    #[must_use]
    pub fn decoding_speed(&self) -> DecodingSpeed {
        self.decoding_speed
    }
}

/// A validated Butteraugli target distance for lossy encoding.
///
/// The distance is libjxl's perceptual quality dial: `1.0` is "visually lossless" (the default) and
/// larger values trade visual quality for a smaller file, up to the maximum of `25.0`. A [`Distance`]
/// is guaranteed finite and in the half-open range `(0.0, 25.0]`.
///
/// `0.0` — libjxl's "mathematically lossless" sentinel — is deliberately **rejected**: lossless is a
/// distinct mode ([`crate::JxlEncoder::lossless`]), not a point on the lossy quality scale, and
/// keeping the two apart means a lossy configuration can never silently become lossless (or the
/// reverse) through a stray value.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct Distance(f32);

impl Distance {
    /// Creates a [`Distance`], rejecting non-finite values and anything outside `(0.0, 25.0]`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidInput`] if `distance` is NaN or infinite, is `0.0` or negative, or
    /// exceeds `25.0`.
    pub fn new(distance: f32) -> Result<Self> {
        if distance.is_finite() && distance > 0.0 && distance <= 25.0 {
            Ok(Self(distance))
        } else {
            Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "JXL: distance must be finite and in (0, 25]",
            ))
        }
    }

    /// The wrapped distance value.
    #[must_use]
    pub fn get(self) -> f32 {
        self.0
    }
}

impl Default for Distance {
    /// The "visually lossless" default distance of `1.0`.
    fn default() -> Self {
        Self(1.0)
    }
}

/// Which JPEG XL byte stream the encoder emits.
///
/// The two share the same coded image but differ in framing:
///
/// - [`Container::Codestream`] — a bare JPEG XL codestream, starting with the 2-byte signature
///   `0xFF 0x0A`. The smallest option; carries no room for metadata boxes.
/// - [`Container::IsoBmff`] — the ISO BMFF container (`.jxl` file format), starting with the 12-byte
///   box signature `00 00 00 0C 4A 58 4C 20 0D 0A 87 0A`. The framing needed to attach Exif/XMP/JUMBF
///   metadata boxes (metadata embedding itself is future work).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Container {
    /// A bare codestream (signature `0xFF 0x0A`); the default.
    #[default]
    Codestream,
    /// The ISO BMFF `.jxl` container (12-byte box signature).
    IsoBmff,
}

/// The display orientation the encoder signals for the coded samples, matching the eight EXIF
/// orientation values.
///
/// Orientation is metadata: the encoder stores the caller's samples exactly as given (in coded
/// order) and decoders apply the transform on output. The four transposing variants
/// ([`Orientation::Transpose`], [`Orientation::Rotate90Cw`], [`Orientation::AntiTranspose`],
/// [`Orientation::Rotate90Ccw`]) swap the displayed width and height relative to the coded
/// dimensions. The default is [`Orientation::Identity`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Orientation {
    /// No transform (EXIF 1); the default.
    #[default]
    Identity,
    /// Mirror horizontally (EXIF 2).
    FlipHorizontal,
    /// Rotate 180 degrees (EXIF 3).
    Rotate180,
    /// Mirror vertically (EXIF 4).
    FlipVertical,
    /// Transpose: mirror across the main diagonal (EXIF 5). Swaps displayed width/height.
    Transpose,
    /// Rotate 90 degrees clockwise (EXIF 6). Swaps displayed width/height.
    Rotate90Cw,
    /// Anti-transpose: mirror across the anti-diagonal (EXIF 7). Swaps displayed width/height.
    AntiTranspose,
    /// Rotate 90 degrees counter-clockwise (EXIF 8). Swaps displayed width/height.
    Rotate90Ccw,
}

impl Orientation {
    /// The EXIF orientation value (`1..=8`) this variant signals.
    #[must_use]
    pub fn exif_value(self) -> u8 {
        match self {
            Self::Identity => 1,
            Self::FlipHorizontal => 2,
            Self::Rotate180 => 3,
            Self::FlipVertical => 4,
            Self::Transpose => 5,
            Self::Rotate90Cw => 6,
            Self::AntiTranspose => 7,
            Self::Rotate90Ccw => 8,
        }
    }

    /// The [`Orientation`] for an EXIF orientation value, or `None` if `value` is outside `1..=8`.
    ///
    /// The inverse of [`Orientation::exif_value`]; handy for forwarding EXIF metadata.
    #[must_use]
    pub fn from_exif_value(value: u8) -> Option<Self> {
        Some(match value {
            1 => Self::Identity,
            2 => Self::FlipHorizontal,
            3 => Self::Rotate180,
            4 => Self::FlipVertical,
            5 => Self::Transpose,
            6 => Self::Rotate90Cw,
            7 => Self::AntiTranspose,
            8 => Self::Rotate90Ccw,
            _ => return None,
        })
    }

    /// Whether this orientation swaps the displayed width and height relative to the coded
    /// dimensions (the four diagonal/rotating transforms, EXIF `5..=8`).
    #[must_use]
    pub fn transposes(self) -> bool {
        matches!(
            self,
            Self::Transpose | Self::Rotate90Cw | Self::AntiTranspose | Self::Rotate90Ccw
        )
    }
}

/// The colour interpretation the encoder signals for the pixel samples.
///
/// JPEG XL separates the coded samples from their colour meaning: the encoder never converts the
/// caller's pixels between colour spaces, it *signals* how they are to be interpreted. The default
/// is [`ColorSpec::Srgb`], matching how 8/16-bit integer buffers are conventionally produced.
///
/// - [`ColorSpec::Srgb`] — standard sRGB (IEC 61966-2-1): sRGB primaries, D65, the sRGB transfer
///   curve. Gray images signal the same transfer curve with luminance-only samples.
/// - [`ColorSpec::LinearSrgb`] — sRGB primaries and D65 with a **linear** transfer function, for
///   samples that are already linear light.
/// - [`ColorSpec::Pq`] — Rec. ITU-R BT.2100 with the SMPTE ST 2084 (PQ) transfer function, the
///   HDR10-style encoding for absolute-luminance HDR samples (typically 16-bit).
/// - [`ColorSpec::Hlg`] — Rec. ITU-R BT.2100 with the Hybrid Log-Gamma transfer function, the
///   scene-referred broadcast HDR encoding.
/// - [`ColorSpec::Icc`] — an embedded ICC profile carried verbatim in the codestream. The profile's
///   data colour space must match the image (`RGB ` for 3-channel, `GRAY` for 1-channel layouts);
///   this is validated when encoding.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ColorSpec {
    /// Standard (gamma-encoded) sRGB; the default.
    #[default]
    Srgb,
    /// sRGB primaries and white point with a linear transfer function.
    LinearSrgb,
    /// Rec. BT.2100 primaries with the SMPTE ST 2084 (PQ) transfer function.
    Pq,
    /// Rec. BT.2100 primaries with the Hybrid Log-Gamma (HLG) transfer function.
    Hlg,
    /// An embedded ICC profile, stored verbatim in the codestream.
    Icc(Vec<u8>),
}

/// Validates that an ICC profile is plausibly attachable to an image with the given colour family:
/// long enough to carry a header, and with a data colour space signature (header bytes 16..20)
/// matching grayscale (`GRAY`) vs colour (`RGB `).
///
/// This is a cheap structural pre-check so an obvious mismatch is a clear typed error before the
/// bytes reach libjxl; full profile validation remains libjxl's job.
#[cfg(all(
    feature = "encode",
    any(not(target_arch = "wasm32"), target_os = "emscripten")
))]
pub(crate) fn validate_icc(icc: &[u8], is_gray: bool) -> Result<()> {
    // The fixed ICC header is 128 bytes; anything shorter cannot be a profile at all.
    if icc.len() < 128 {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "JXL: ICC profile is too short",
        ));
    }
    let space = &icc[16..20];
    let expected: &[u8; 4] = if is_gray { b"GRAY" } else { b"RGB " };
    if space != expected {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "JXL: ICC profile color space does not match the image layout",
        ));
    }
    Ok(())
}

/// The encoder's lossless-vs-lossy state.
///
/// Kept private so that the invalid combination — lossless *and* a distance — is unrepresentable:
/// [`Mode::Lossless`] carries no distance, and [`Mode::Lossy`] always carries a validated one.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Mode {
    /// Mathematically lossless: the decoded image is bit-exact to the input.
    Lossless,
    /// Lossy at the given Butteraugli [`Distance`].
    Lossy(Distance),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effort_level_round_trips_over_full_range() {
        // Every variant maps to a distinct 1..=10 level and back.
        let all = [
            Effort::Lightning,
            Effort::Thunder,
            Effort::Falcon,
            Effort::Cheetah,
            Effort::Hare,
            Effort::Wombat,
            Effort::Squirrel,
            Effort::Kitten,
            Effort::Tortoise,
            Effort::Glacier,
        ];
        for (i, e) in all.into_iter().enumerate() {
            let level = (i + 1) as u8;
            assert_eq!(e.level(), level, "{e:?} level");
            assert_eq!(Effort::from_level(level), Some(e), "from_level({level})");
        }
        // Exhaustive 1..=10 coverage: no gaps, no aliasing.
        for level in 1..=10u8 {
            assert_eq!(Effort::from_level(level).unwrap().level(), level);
        }
    }

    #[test]
    fn effort_from_level_rejects_out_of_range() {
        assert_eq!(Effort::from_level(0), None);
        assert_eq!(Effort::from_level(11), None);
        assert_eq!(Effort::from_level(255), None);
    }

    #[test]
    fn effort_default_is_squirrel() {
        assert_eq!(Effort::default(), Effort::Squirrel);
        assert_eq!(Effort::default().level(), 7);
    }

    #[test]
    fn modular_mode_option_value_round_trips_over_full_range() {
        // Every variant maps to a distinct -1..=1 libjxl value and back.
        let all = [ModularMode::Auto, ModularMode::VarDct, ModularMode::Modular];
        for (i, m) in all.into_iter().enumerate() {
            let value = i as i32 - 1;
            assert_eq!(m.option_value(), value, "{m:?} option value");
            assert_eq!(
                ModularMode::from_option_value(value),
                Some(m),
                "from_option_value({value})"
            );
        }
        // Exhaustive -1..=1 coverage: no gaps, no aliasing.
        for value in -1..=1i32 {
            assert_eq!(
                ModularMode::from_option_value(value)
                    .unwrap()
                    .option_value(),
                value
            );
        }
    }

    #[test]
    fn modular_mode_from_option_value_rejects_out_of_range() {
        assert_eq!(ModularMode::from_option_value(-2), None);
        assert_eq!(ModularMode::from_option_value(2), None);
        assert_eq!(ModularMode::from_option_value(i32::MIN), None);
        assert_eq!(ModularMode::from_option_value(i32::MAX), None);
    }

    #[test]
    fn modular_mode_default_is_auto() {
        assert_eq!(ModularMode::default(), ModularMode::Auto);
        assert_eq!(ModularMode::default().option_value(), -1);
    }

    #[test]
    fn toggle_option_values_are_libjxl_s_tri_state() {
        assert_eq!(Toggle::EncoderDefault.option_value(), None);
        assert_eq!(Toggle::Off.option_value(), Some(0));
        assert_eq!(Toggle::On.option_value(), Some(1));
        assert_eq!(Toggle::default(), Toggle::EncoderDefault);
    }

    #[test]
    fn toggle_discriminants_are_permanent() {
        assert_eq!(Toggle::EncoderDefault as u8, 0);
        assert_eq!(Toggle::Off as u8, 1);
        assert_eq!(Toggle::On as u8, 2);
    }

    #[test]
    fn progressive_dc_option_values_cover_libjxl_s_range() {
        assert_eq!(ProgressiveDc::EncoderDefault.option_value(), None);
        assert_eq!(ProgressiveDc::Off.option_value(), Some(0));
        assert_eq!(ProgressiveDc::OneLevel.option_value(), Some(1));
        assert_eq!(ProgressiveDc::TwoLevels.option_value(), Some(2));
        assert_eq!(ProgressiveDc::default(), ProgressiveDc::EncoderDefault);
    }

    #[test]
    fn progressive_dc_discriminants_are_permanent() {
        assert_eq!(ProgressiveDc::EncoderDefault as u8, 0);
        assert_eq!(ProgressiveDc::Off as u8, 1);
        assert_eq!(ProgressiveDc::OneLevel as u8, 2);
        assert_eq!(ProgressiveDc::TwoLevels as u8, 3);
    }

    #[test]
    fn decoding_speed_option_values_are_tiers_zero_to_four() {
        assert_eq!(DecodingSpeed::EncoderDefault.option_value(), None);
        let tiers = [
            DecodingSpeed::Tier0,
            DecodingSpeed::Tier1,
            DecodingSpeed::Tier2,
            DecodingSpeed::Tier3,
            DecodingSpeed::Tier4,
        ];
        for (tier, speed) in tiers.into_iter().enumerate() {
            assert_eq!(speed.option_value(), Some(tier as i32), "{speed:?}");
            // Discriminants are permanent: the encoder default is 0, tier N is N + 1.
            assert_eq!(speed as u8, tier as u8 + 1, "{speed:?} discriminant");
        }
        assert_eq!(DecodingSpeed::EncoderDefault as u8, 0);
        assert_eq!(DecodingSpeed::default(), DecodingSpeed::EncoderDefault);
    }

    #[test]
    fn frame_settings_default_leaves_every_setting_to_libjxl() {
        let fs = FrameSettings::default();
        assert_eq!(fs.progressive_ac(), Toggle::EncoderDefault);
        assert_eq!(fs.qprogressive_ac(), Toggle::EncoderDefault);
        assert_eq!(fs.progressive_dc(), ProgressiveDc::EncoderDefault);
        assert_eq!(fs.responsive(), Toggle::EncoderDefault);
        assert_eq!(fs.decoding_speed(), DecodingSpeed::EncoderDefault);
    }

    #[test]
    fn frame_settings_builders_each_touch_only_their_own_field() {
        let base = FrameSettings::default();
        let fs = base.with_progressive_ac(Toggle::On);
        assert_eq!(fs.progressive_ac(), Toggle::On);
        assert_eq!(fs.with_progressive_ac(Toggle::EncoderDefault), base);

        let fs = base.with_qprogressive_ac(Toggle::Off);
        assert_eq!(fs.qprogressive_ac(), Toggle::Off);
        assert_eq!(fs.with_qprogressive_ac(Toggle::EncoderDefault), base);

        let fs = base.with_progressive_dc(ProgressiveDc::TwoLevels);
        assert_eq!(fs.progressive_dc(), ProgressiveDc::TwoLevels);
        assert_eq!(fs.with_progressive_dc(ProgressiveDc::EncoderDefault), base);

        let fs = base.with_responsive(Toggle::On);
        assert_eq!(fs.responsive(), Toggle::On);
        assert_eq!(fs.with_responsive(Toggle::EncoderDefault), base);

        let fs = base.with_decoding_speed(DecodingSpeed::Tier3);
        assert_eq!(fs.decoding_speed(), DecodingSpeed::Tier3);
        assert_eq!(fs.with_decoding_speed(DecodingSpeed::EncoderDefault), base);
    }

    #[test]
    fn distance_accepts_the_valid_interval() {
        // Just above zero, the default, and the upper bound are all accepted.
        assert_eq!(
            Distance::new(f32::MIN_POSITIVE).unwrap().get(),
            f32::MIN_POSITIVE
        );
        assert_eq!(Distance::new(1.0).unwrap().get(), 1.0);
        assert_eq!(Distance::new(25.0).unwrap().get(), 25.0);
        assert_eq!(Distance::new(0.5).unwrap().get(), 0.5);
    }

    #[test]
    fn distance_rejects_out_of_range_and_non_finite() {
        // 0.0 is rejected on purpose: lossless is a mode, not a distance.
        assert!(Distance::new(0.0).is_err());
        assert!(Distance::new(-1.0).is_err());
        assert!(Distance::new(f32::NAN).is_err());
        assert!(Distance::new(f32::INFINITY).is_err());
        assert!(Distance::new(f32::NEG_INFINITY).is_err());
        // Just over the upper bound is rejected; the bound itself (tested above) is not.
        assert!(Distance::new(25.0001).is_err());
        assert!(Distance::new(f32::MAX).is_err());
    }

    #[test]
    fn distance_default_is_one() {
        assert_eq!(Distance::default().get(), 1.0);
    }

    #[test]
    fn container_default_is_codestream() {
        assert_eq!(Container::default(), Container::Codestream);
    }

    #[test]
    fn color_spec_default_is_srgb() {
        assert_eq!(ColorSpec::default(), ColorSpec::Srgb);
    }

    #[test]
    fn orientation_exif_value_round_trips_over_full_range() {
        let all = [
            Orientation::Identity,
            Orientation::FlipHorizontal,
            Orientation::Rotate180,
            Orientation::FlipVertical,
            Orientation::Transpose,
            Orientation::Rotate90Cw,
            Orientation::AntiTranspose,
            Orientation::Rotate90Ccw,
        ];
        for (i, o) in all.into_iter().enumerate() {
            let value = (i + 1) as u8;
            assert_eq!(o.exif_value(), value, "{o:?} exif value");
            assert_eq!(Orientation::from_exif_value(value), Some(o));
        }
        for value in 1..=8u8 {
            assert_eq!(
                Orientation::from_exif_value(value).unwrap().exif_value(),
                value
            );
        }
    }

    #[test]
    fn orientation_from_exif_rejects_out_of_range() {
        assert_eq!(Orientation::from_exif_value(0), None);
        assert_eq!(Orientation::from_exif_value(9), None);
        assert_eq!(Orientation::from_exif_value(255), None);
    }

    #[test]
    fn orientation_default_is_identity_and_transposing_set_is_exact() {
        assert_eq!(Orientation::default(), Orientation::Identity);
        // Exactly EXIF 5..=8 transpose; 1..=4 do not.
        for value in 1..=8u8 {
            let o = Orientation::from_exif_value(value).unwrap();
            assert_eq!(o.transposes(), value >= 5, "{o:?}");
        }
    }

    /// A minimal 128-byte "profile": all zeros except the data colour space signature.
    #[cfg(all(
        feature = "encode",
        any(not(target_arch = "wasm32"), target_os = "emscripten")
    ))]
    fn fake_icc(space: &[u8; 4]) -> Vec<u8> {
        let mut icc = vec![0u8; 128];
        icc[16..20].copy_from_slice(space);
        icc
    }

    #[test]
    #[cfg(all(
        feature = "encode",
        any(not(target_arch = "wasm32"), target_os = "emscripten")
    ))]
    fn validate_icc_accepts_matching_color_spaces() {
        assert!(validate_icc(&fake_icc(b"RGB "), false).is_ok());
        assert!(validate_icc(&fake_icc(b"GRAY"), true).is_ok());
    }

    #[test]
    #[cfg(all(
        feature = "encode",
        any(not(target_arch = "wasm32"), target_os = "emscripten")
    ))]
    fn validate_icc_rejects_mismatched_color_spaces() {
        // An RGB profile on a grayscale image and vice versa are both structural mismatches.
        assert!(validate_icc(&fake_icc(b"RGB "), true).is_err());
        assert!(validate_icc(&fake_icc(b"GRAY"), false).is_err());
        // CMYK profiles are never attachable to gamut-jxl's layouts.
        assert!(validate_icc(&fake_icc(b"CMYK"), false).is_err());
    }

    #[test]
    #[cfg(all(
        feature = "encode",
        any(not(target_arch = "wasm32"), target_os = "emscripten")
    ))]
    fn validate_icc_rejects_short_profiles() {
        assert!(validate_icc(&[], false).is_err());
        assert!(validate_icc(&[0u8; 127], false).is_err());
        assert!(validate_icc(&[0u8; 20], true).is_err());
    }
}
