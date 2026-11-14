//! Encoder configuration: lossless vs. lossy selection, the quality knob, the output colour
//! encoding, alpha handling, orientation, HDR descriptors, effort and threads — every plain-data
//! knob of an [`AvifEncoder`](crate::AvifEncoder).

use core::num::NonZeroUsize;

use gamut_av1::Effort;
use gamut_color::{
    BitDepth, ChromaSubsampling, ColorPrimaries, ColorRange, MatrixCoefficients,
    TransferCharacteristics,
};
use gamut_core::Orientation;
use gamut_isobmff::{ContentLightLevel, MasteringDisplay};

/// How the colour of an RGBA input relates to its alpha, and so what an RGBA encode codes and
/// declares. Select it with [`AvifEncoder::with_alpha_mode`](crate::AvifEncoder::with_alpha_mode).
///
/// A file whose colour item carries a `prem` item reference to its alpha auxiliary declares the
/// colour **premultiplied** — associated with alpha (ISO/IEC 23008-12 §6); without
/// one the colour is unassociated. A reader un-premultiplies the first before compositing, so
/// declaring the wrong one darkens or halos the displayed edges.
///
/// | Mode | Input colour | Coded colour | `prem` written |
/// | --- | --- | --- | --- |
/// | [`Unassociated`](Self::Unassociated) | unassociated | as given | no |
/// | [`Premultiplied`](Self::Premultiplied) | already premultiplied | as given | yes |
/// | [`PremultiplyOnEncode`](Self::PremultiplyOnEncode) | unassociated | premultiplied by the encoder | yes |
/// | [`CleanOnEncode`](Self::CleanOnEncode) | unassociated | hidden colour cleaned by the encoder (lossy only) | no |
///
/// Every mode is ignored by an input without an alpha channel: with no alpha auxiliary there is
/// no alpha to associate with and nothing for `prem` to target.
///
/// `#[non_exhaustive]`, `#[repr(u8)]` with permanent, append-only discriminants: plain
/// configuration data that must stay mechanically portable to C (see the workspace charter).
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum AlphaMode {
    /// The colour is **unassociated** (straight) and is coded as given, with no `prem` — the
    /// default, and the interpretation [`Rgba8`](gamut_core::Rgba8)'s
    /// [`ColorModel::Rgba`](gamut_core::ColorModel::Rgba) already documents.
    #[default]
    Unassociated = 0,
    /// The caller's colour is **already premultiplied** by its alpha. It is coded as given and the
    /// file declares it with `prem`: a declaration, not a conversion — the encoder multiplies
    /// nothing and divides nothing. Getting the declaration wrong darkens or halos the edges of
    /// the displayed image.
    Premultiplied = 1,
    /// The caller's colour is **unassociated**; the encoder premultiplies it — with
    /// [`gamut_core::convert::premultiply_alpha`], `round(colour * alpha / max)` at the input's
    /// own sample scale — before coding, and declares the result with `prem`.
    ///
    /// Opaque pixels code exactly as given and alpha is never touched. Every other pixel keeps
    /// only `alpha + 1` colour levels, so a reader that un-premultiplies recovers the source only
    /// to within rounding — `⌊(a + max) / 2a⌋` code values at alpha `a`, and no colour at all
    /// under a fully transparent pixel. That makes this mode **lossy in the colour channels even
    /// under [`AvifMode::Lossless`]**, which then codes the *premultiplied* colour bit-exactly.
    PremultiplyOnEncode = 2,
    /// The caller's colour is **unassociated**, and before a **lossy** encode the encoder
    /// replaces the colour hidden under transparent and semi-transparent pixels with a smooth
    /// bleed of the visible edge, so the coder spends no bits on colour nobody sees and the
    /// visible edge does not ring against it. No `prem` is written.
    ///
    /// What a reader displays does not change: alpha and opaque pixels are untouched, a fully
    /// transparent pixel's colour is invisible, and a semi-transparent pixel's colour moves only
    /// within the range that premultiplies to the same value. Only the decoded *unassociated*
    /// colour under transparency differs from the source, which is why it is opt-in.
    ///
    /// [`AvifMode::Lossless`] stays bit-exact and codes this mode exactly as
    /// [`Unassociated`](Self::Unassociated), and so does
    /// [`AvifEncoder::encode_raw_planes`](crate::AvifEncoder::encode_raw_planes), which rewrites
    /// no colour.
    CleanOnEncode = 3,
}

impl AlphaMode {
    /// Whether the file declares its colour premultiplied with a `prem` item reference.
    pub(crate) fn declares_premultiplied(self) -> bool {
        matches!(self, Self::Premultiplied | Self::PremultiplyOnEncode)
    }
}

/// Whether an RGBA input whose every alpha sample is fully opaque keeps its alpha auxiliary item.
/// Select it with [`AvifEncoder::with_opaque_alpha`](crate::AvifEncoder::with_opaque_alpha).
///
/// `#[repr(u8)]` with permanent, append-only discriminants, like [`AlphaMode`].
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum OpaqueAlpha {
    /// Code a fully opaque RGBA input as the RGB image it is indistinguishable from: no alpha
    /// auxiliary, byte-identical to the RGB encode of its colour — the default.
    #[default]
    Drop = 0,
    /// Keep the alpha auxiliary item even when every alpha sample is fully opaque.
    Keep = 1,
}

/// Which AVIF bitstream the encoder produces.
///
/// `#[non_exhaustive]`: modes are an open set — variants for deferred coding strategies (e.g.
/// metric- or size-targeted rate control, tracked in `STATUS.md`) are added as they ship, so
/// match with a wildcard arm.
///
/// `#[repr(u8)]` with permanent, append-only discriminants: the mode is plain configuration data
/// that must stay mechanically portable to C (see the workspace charter).
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum AvifMode {
    /// Lossless AV1 intra coding — the decoded output is bit-exact to the input (the default;
    /// gamut's M0 path). The RGB samples map to identity-matrix 4:4:4 planes, so no colour
    /// conversion is applied.
    #[default]
    Lossless = 0,
    /// Lossy AV1 intra coding — smaller output at a quality/size tradeoff set by
    /// [`AvifConfig::quality`].
    Lossy = 1,
}

/// Configuration for an [`AvifEncoder`](crate::AvifEncoder).
///
/// `quality` ranges `0..=100` (higher = larger output, closer to the source) and applies to
/// [`AvifMode::Lossy`]; it is ignored for [`AvifMode::Lossless`].
///
/// Every plain-data knob of the encoder is a field here — the byte payloads (ICC, Exif, XMP,
/// C2PA) and the codestream backends are not — so a configuration read back with
/// [`AvifEncoder::config`](crate::AvifEncoder::config) feeds straight back in through
/// [`AvifEncoder::from_config`](crate::AvifEncoder::from_config):
///
/// ```
/// use gamut_avif::{AvifConfig, AvifEncoder, AvifMode};
///
/// let mut config = AvifEncoder::lossy(60).config();
/// config.quality = 40;
/// let encoder = AvifEncoder::from_config(config);
/// assert_eq!(encoder.config().mode, AvifMode::Lossy);
/// assert_eq!(encoder.config().quality, 40);
/// assert_eq!(AvifEncoder::from_config(AvifConfig::default()).config(), AvifEncoder::new().config());
/// ```
///
/// `#[non_exhaustive]`: the configuration is an open set — fields for deferred encoder knobs
/// (see `STATUS.md`) are added as they ship — so start from [`AvifConfig::default`] or from
/// [`AvifEncoder::config`](crate::AvifEncoder::config) rather than a struct literal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct AvifConfig {
    /// The bitstream mode to encode.
    pub mode: AvifMode,
    /// Lossy quality factor, `0..=100`. Values above `100` behave as `100` (the encoder clamps
    /// silently) — a frozen v1 contract.
    pub quality: u8,
    /// Lossy quality factor for the **alpha auxiliary item**, `0..=100` on the same scale and
    /// through the same `base_q_idx` mapping as `quality`, or `None` to code alpha at `quality`.
    ///
    /// `None` (the default) keeps alpha on the colour item's quantizer, so an encoder that never
    /// sets this writes exactly the bytes it wrote before the knob existed. Values above `100`
    /// behave as `100`, as for `quality`. Applies only to inputs that carry alpha
    /// ([`Rgba8`](gamut_core::Rgba8), [`Rgba16`](gamut_core::Rgba16)) and only to
    /// [`AvifMode::Lossy`]: [`AvifMode::Lossless`] **ignores** it, exactly as it ignores
    /// `quality`, and codes alpha bit-exactly. Set it with
    /// [`AvifEncoder::with_alpha_quality`](crate::AvifEncoder::with_alpha_quality).
    pub alpha_quality: Option<u8>,
    /// The CICP matrix the samples are coded through.
    ///
    /// [`MatrixCoefficients::Identity`] carries R'G'B' directly as GBR planes — the only way to be
    /// bit-exact, so [`AvifMode::Lossless`] always uses it and **ignores** this field, exactly as
    /// it ignores `quality`. [`AvifMode::Lossy`] defaults to [`MatrixCoefficients::Bt709`], whose
    /// luma–chroma decorrelation is worth a substantial fraction of the bitrate.
    ///
    pub matrix: MatrixCoefficients,
    /// The signal range the coded samples occupy. Full range is the AVIF ecosystem's default (it
    /// is what `avifImageCreate` sets) and spends all 256 codes on the signal.
    pub range: ColorRange,
    /// How the chroma planes are sampled relative to luma.
    ///
    /// [`AvifMode::Lossy`] defaults to [`ChromaSubsampling::Cs420`]: it is smaller at equal quality
    /// on photographic content, and it is the only format a Main-profile decoder can read — 4:4:4
    /// is AV1 **Profile 1**, which several hardware still-image paths reject outright.
    ///
    /// [`AvifMode::Lossless`] always uses [`ChromaSubsampling::Cs444`] and **ignores** this field,
    /// exactly as it ignores `matrix` and `quality`: discarding three quarters of the chroma
    /// samples is not lossless, and AV1 §6.4.2 forbids the identity matrix below 4:4:4 anyway.
    ///
    /// [`ChromaSubsampling::Cs400`] (monochrome) is rejected at encode time.
    pub chroma: ChromaSubsampling,
    /// The CICP colour primaries the samples are tagged with — the gamut their R'G'B' values are
    /// interpreted in.
    ///
    /// Unlike `matrix` and `range`, this is a **pure tag**: no sample is touched by it, so
    /// [`AvifMode::Lossless`] honours it rather than ignoring it. Defaults to
    /// [`ColorPrimaries::Bt709`] — sRGB's gamut.
    pub primaries: ColorPrimaries,
    /// The CICP transfer characteristics the samples are tagged with — the transfer function
    /// already applied to them.
    ///
    /// A pure tag like `primaries`: the encoder applies **no** transfer function, so selecting e.g.
    /// [`TransferCharacteristics::Pq`] *labels* samples the caller has already encoded that way. It
    /// does not convert them, and the HDR metadata (`clli`/`mdcv`) is set separately, in
    /// [`content_light_level`](Self::content_light_level) and
    /// [`mastering_display`](Self::mastering_display). Defaults to
    /// [`TransferCharacteristics::Srgb`].
    pub transfer: TransferCharacteristics,
    /// The depth the samples are coded at, or `None` for the input's own default: **8** for an
    /// 8-bit input ([`Rgb8`](gamut_core::Rgb8), [`Rgba8`](gamut_core::Rgba8),
    /// [`Gray8`](gamut_core::Gray8)) and **12** for a 16-bit one
    /// ([`Rgb16`](gamut_core::Rgb16), [`Rgba16`](gamut_core::Rgba16)).
    ///
    /// `None` and an explicit choice are different requests, which is why this is an `Option`: the
    /// default codes each input at a depth that suits it, while an explicit [`BitDepth::Ten`] or
    /// [`BitDepth::Twelve`] applies to *every* input — an 8-bit input is widened to it by bit
    /// replication, a 16-bit one narrowed to it by truncation. [`BitDepth::Eight`] is accepted for
    /// 8-bit inputs only. See
    /// [`AvifEncoder::with_bit_depth`](crate::AvifEncoder::with_bit_depth) for both conversions.
    pub bit_depth: Option<BitDepth>,
    /// How an RGBA input's colour relates to its alpha — see [`AlphaMode`]. Meaningless without an
    /// alpha channel, so it reaches the file only for an RGBA encode.
    pub alpha_mode: AlphaMode,
    /// Whether a fully opaque RGBA input keeps its alpha auxiliary — see [`OpaqueAlpha`].
    pub opaque_alpha: OpaqueAlpha,
    /// The display orientation a reader applies, written as `irot`/`imir` item properties. The
    /// stored pixels are unchanged. [`Orientation::Identity`] (the default) writes neither.
    pub orientation: Orientation,
    /// HDR content light level, written as a non-essential `clli` property of the primary colour
    /// item, or `None` for none.
    pub content_light_level: Option<ContentLightLevel>,
    /// HDR mastering display colour volume, written as a non-essential `mdcv` property of the
    /// primary colour item, or `None` for none.
    pub mastering_display: Option<MasteringDisplay>,
    /// The `gamut-av1` mode-decision search preset every AV1 still of the file is coded at —
    /// colour and alpha alike. Defaults to [`Effort::default`].
    pub effort: Effort,
    /// The most threads an encode may use; `None` is the machine's available parallelism.
    /// Defaults to `Some(1)`. The output does not depend on it.
    pub threads: Option<NonZeroUsize>,
}

impl Default for AvifConfig {
    fn default() -> Self {
        Self {
            mode: AvifMode::Lossless,
            quality: 75,
            alpha_quality: None,
            matrix: MatrixCoefficients::Identity,
            range: ColorRange::Full,
            chroma: ChromaSubsampling::Cs444,
            primaries: ColorPrimaries::Bt709,
            transfer: TransferCharacteristics::Srgb,
            bit_depth: None,
            alpha_mode: AlphaMode::Unassociated,
            opaque_alpha: OpaqueAlpha::Drop,
            orientation: Orientation::Identity,
            content_light_level: None,
            mastering_display: None,
            effort: Effort::default(),
            threads: Some(NonZeroUsize::MIN),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_lossless_identity_quality_75() {
        let c = AvifConfig::default();
        assert_eq!(c.mode, AvifMode::Lossless);
        assert_eq!(c.quality, 75);
        // Unset: alpha follows `quality`, so the default encoder's bytes predate the knob.
        assert_eq!(c.alpha_quality, None);
        assert_eq!(c.matrix, MatrixCoefficients::Identity);
        assert_eq!(c.range, ColorRange::Full);
        assert_eq!(c.primaries, ColorPrimaries::Bt709);
        assert_eq!(c.transfer, TransferCharacteristics::Srgb);
        // Unset: each input codes at its own default depth (8 for 8-bit, 12 for 16-bit), so the
        // default encoder's 8-bit bytes predate the knob reaching 8-bit inputs.
        assert_eq!(c.bit_depth, None);
        // Opt-in: hidden colour is coded as given unless the caller asks for it to be cleaned.
        assert_eq!(c.alpha_mode, AlphaMode::Unassociated);
        assert_eq!(c.opaque_alpha, OpaqueAlpha::Drop);
        assert_eq!(c.orientation, Orientation::Identity);
        assert_eq!(c.content_light_level, None);
        assert_eq!(c.mastering_display, None);
        assert_eq!(c.effort, Effort::default());
        // One thread unless asked: an encoder without the knob spawns nothing.
        assert_eq!(c.threads, Some(NonZeroUsize::MIN));
        assert_eq!(AvifMode::default(), AvifMode::Lossless);
        assert_eq!(AlphaMode::default(), AlphaMode::Unassociated);
        assert_eq!(OpaqueAlpha::default(), OpaqueAlpha::Drop);
    }

    #[test]
    fn mode_discriminants_are_the_frozen_c_abi_values() {
        // `AvifMode` is plain configuration data crossing the C boundary, so its discriminants are
        // append-only: a reordering would silently change what an existing C caller selects.
        assert_eq!(AvifMode::Lossless as u8, 0);
        assert_eq!(AvifMode::Lossy as u8, 1);
        assert_eq!(AlphaMode::Unassociated as u8, 0);
        assert_eq!(AlphaMode::Premultiplied as u8, 1);
        assert_eq!(AlphaMode::PremultiplyOnEncode as u8, 2);
        assert_eq!(AlphaMode::CleanOnEncode as u8, 3);
        assert_eq!(OpaqueAlpha::Drop as u8, 0);
        assert_eq!(OpaqueAlpha::Keep as u8, 1);
    }

    #[test]
    fn only_the_premultiplied_modes_declare_prem() {
        assert!(!AlphaMode::Unassociated.declares_premultiplied());
        assert!(AlphaMode::Premultiplied.declares_premultiplied());
        assert!(AlphaMode::PremultiplyOnEncode.declares_premultiplied());
        // Cleaning rewrites hidden colour but keeps it unassociated.
        assert!(!AlphaMode::CleanOnEncode.declares_premultiplied());
    }
}
