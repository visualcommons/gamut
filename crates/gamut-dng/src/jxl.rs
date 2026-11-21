//! JPEG XL (Compression = 52546, DNG 1.7) chunk codec: the bridge between DNG's tile/strip
//! layout and [`gamut_jxl`].
//!
//! Each DNG chunk holds one complete JPEG XL bitstream — the spec allows both the bare
//! codestream (recommended for multi-tile images; what Apple ProRAW uses) and the ISO BMFF
//! container, and [`gamut_jxl::JxlDecoder`] accepts either.
//!
//! **Range semantics** (matching the reference Adobe DNG SDK, the crate's oracle): JPEG XL image
//! data decodes to **full-range 16-bit** code values, whatever precision the codestream declares
//! — the SDK requests pixel-format depth from libjxl, and real writers depend on it (an Apple
//! ProRAW declares `BitsPerSample = 10` yet sets `WhiteLevel = 65535`). A JXL IFD's
//! `BitsPerSample` therefore records the codestream's stored precision, not the decoded
//! representation; encode likewise treats input as full-range 16-bit.
//!
//! Floating-point JPEG XL raws (fp16, `SampleFormat = 3`) are detected here and rejected with a
//! typed error — float sample support is deferred crate-wide (see `STATUS.md`).

use gamut_core::{DecodeImage, Error, Gray16, Result, Rgb16};
use gamut_jxl::{DecodeLimits, Distance, Effort, JxlDecoder};

/// The JPEG XL encode parameters of a DNG raw image: the payload of
/// [`DngCompression::Jxl`](crate::DngCompression::Jxl), so they can only be set where they take
/// effect.
///
/// Plain `Copy` data over gamut-jxl's own validated [`Distance`] and
/// [`JxlEffort`](crate::JxlEffort): [`lossless`](Self::lossless) (the [`Default`]) or
/// [`lossy`](Self::lossy) at a Butteraugli distance, at libjxl's default effort
/// ([`JxlEffort::Squirrel`](crate::JxlEffort::Squirrel), level 7) unless
/// [`with_effort`](Self::with_effort) picks another. The written raw IFD records both in the
/// `JXLDistance` (`0.0` for lossless) and `JXLEffort` tags (DNG 1.7.1 pp. 97-98).
///
/// ```
/// use gamut_dng::{Distance, DngCompression, DngEncoder, JxlEffort, JxlSettings};
///
/// const LOSSLESS: DngCompression = DngCompression::Jxl(JxlSettings::lossless());
/// let lossy = JxlSettings::lossy(Distance::new(1.0)?).with_effort(JxlEffort::Hare);
/// let encoder = DngEncoder::new().with_compression(DngCompression::Jxl(lossy));
/// # let _ = (LOSSLESS, encoder);
/// # Ok::<(), gamut_core::Error>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct JxlSettings {
    /// `None` is lossless (the default); `Some` is lossy at that Butteraugli distance.
    distance: Option<Distance>,
    effort: Effort,
}

impl Default for JxlSettings {
    /// [`JxlSettings::lossless`].
    fn default() -> Self {
        Self::lossless()
    }
}

impl JxlSettings {
    /// Lossless JPEG XL at libjxl's default effort
    /// ([`JxlEffort::Squirrel`](crate::JxlEffort::Squirrel)).
    #[must_use]
    pub const fn lossless() -> Self {
        Self {
            distance: None,
            effort: Effort::Squirrel,
        }
    }

    /// Lossy JPEG XL at the Butteraugli `distance` (1.0 ≈ visually lossless), at the default
    /// effort. `distance` is already validated by [`Distance::new`], so this cannot fail.
    #[must_use]
    pub const fn lossy(distance: Distance) -> Self {
        Self {
            distance: Some(distance),
            ..Self::lossless()
        }
    }

    /// Returns these settings at the given libjxl effort.
    #[must_use]
    pub const fn with_effort(self, effort: Effort) -> Self {
        Self { effort, ..self }
    }

    /// The Butteraugli distance: `None` for lossless.
    #[must_use]
    pub fn distance(&self) -> Option<Distance> {
        self.distance
    }

    /// The libjxl effort.
    #[must_use]
    pub fn effort(&self) -> Effort {
        self.effort
    }

    /// The `JXLDistance` tag value: the distance, or `0.0` — libjxl's own lossless sentinel —
    /// when lossless.
    pub(crate) fn distance_tag(&self) -> f32 {
        self.distance.map_or(0.0, Distance::get)
    }
}

/// Decodes one JPEG XL chunk of `cols × rows` pixels at `spp` samples each, returning exactly
/// `cols * rows * spp` full-range 16-bit code values (see the module docs).
///
/// The stream's declared geometry and channel count must agree with the IFD's — a disagreement
/// means the file is lying about its own contents.
pub(crate) fn decode_chunk(bytes: &[u8], cols: usize, rows: usize, spp: usize) -> Result<Vec<u16>> {
    // The chunk's size is the IFD's tile/strip geometry, which the header is checked against
    // below, so the decoder is bounded to exactly that raster rather than the workspace's general
    // default (32768 per side, 16384 × 16384 pixels): a single-strip raw can legitimately exceed
    // it, and nothing beyond it is ever asked.
    let side = |n: usize| u32::try_from(n).unwrap_or(u32::MAX);
    let pixels = cols.saturating_mul(rows);
    let budget = pixels.saturating_mul(spp).saturating_mul(2);
    let decoder = JxlDecoder::new().with_limits(
        DecodeLimits::new()
            .with_max_width(side(cols))
            .with_max_height(side(rows))
            .with_max_pixels(u64::try_from(pixels).unwrap_or(u64::MAX))
            .with_max_image_bytes(budget),
    );
    let info = decoder.info(bytes)?;
    if info.is_float {
        return Err(Error::unsupported(
            env!("CARGO_PKG_NAME"),
            "DNG: floating-point JPEG XL raw data is not supported",
        ));
    }
    if (
        info.dimensions.width() as usize,
        info.dimensions.height() as usize,
    ) != (cols, rows)
    {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "DNG: JPEG XL chunk geometry disagrees with the tile/strip layout",
        ));
    }
    if usize::from(info.color_channels) != spp || info.has_alpha {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "DNG: JPEG XL channel count disagrees with SamplesPerPixel",
        ));
    }
    match spp {
        1 => Ok(<JxlDecoder as DecodeImage<Gray16>>::decode_image(&decoder, bytes)?.into_samples()),
        3 => Ok(<JxlDecoder as DecodeImage<Rgb16>>::decode_image(&decoder, bytes)?.into_samples()),
        // The spec allows exactly 1 or 3 planes for JPEG XL image data.
        _ => Err(Error::unsupported(
            env!("CARGO_PKG_NAME"),
            "DNG: JPEG XL image data must have 1 or 3 sample planes",
        )),
    }
}

/// Encodes one chunk of `cols × rows` pixels at `spp` full-range 16-bit samples each as a bare
/// JPEG XL codestream (the spec's recommendation for tiled image data), lossless or lossy and at
/// the effort `settings` carries.
#[cfg(all(
    feature = "jxl-encode",
    any(not(target_arch = "wasm32"), target_os = "emscripten")
))]
pub(crate) fn encode_chunk(
    samples: &[u16],
    cols: usize,
    rows: usize,
    spp: usize,
    settings: JxlSettings,
) -> Result<Vec<u8>> {
    use gamut_core::{Dimensions, EncodeImage, ImageRef};
    use gamut_jxl::JxlEncoder;

    let encoder = settings
        .distance()
        .map_or_else(JxlEncoder::lossless, JxlEncoder::lossy)
        .with_effort(settings.effort());
    let dims = Dimensions::new(cols as u32, rows as u32)?;
    let mut out = Vec::new();
    match spp {
        1 => {
            encoder.encode_image(ImageRef::<Gray16>::new(samples, dims)?, &mut out)?;
        }
        3 => {
            encoder.encode_image(ImageRef::<Rgb16>::new(samples, dims)?, &mut out)?;
        }
        _ => {
            return Err(Error::unsupported(
                env!("CARGO_PKG_NAME"),
                "DNG: JPEG XL image data must have 1 or 3 sample planes",
            ));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod settings_tests {
    use super::*;

    /// Lossless — the default — is at libjxl's default effort, recorded as `JXLDistance` 0.0.
    #[test]
    fn default_settings_are_lossless_at_squirrel() {
        let settings = JxlSettings::lossless();
        assert_eq!(JxlSettings::default(), settings);
        assert_eq!(settings.distance(), None);
        assert_eq!(settings.effort(), Effort::Squirrel);
        assert_eq!(settings.distance_tag().to_bits(), 0.0f32.to_bits());
    }

    /// `lossy` keeps its distance, and the tag records exactly that distance.
    #[test]
    fn lossy_settings_carry_their_distance() {
        let distance = Distance::new(2.5).expect("a valid distance");
        let settings = JxlSettings::lossy(distance);
        assert_eq!(settings.distance(), Some(distance));
        assert_eq!(settings.effort(), Effort::Squirrel);
        assert_eq!(settings.distance_tag().to_bits(), 2.5f32.to_bits());
    }

    /// `with_effort` replaces the effort and keeps the distance.
    #[test]
    fn with_effort_keeps_the_distance() {
        let distance = Distance::new(1.5).expect("a valid distance");
        let settings = JxlSettings::lossy(distance).with_effort(Effort::Hare);
        assert_eq!(settings.effort(), Effort::Hare);
        assert_eq!(settings.distance(), Some(distance));
    }
}

#[cfg(all(
    test,
    feature = "jxl-encode",
    any(not(target_arch = "wasm32"), target_os = "emscripten")
))]
mod tests {
    use super::*;

    /// A stray alpha channel invalidates a raw chunk on its own, even when the colour channel
    /// count matches `SamplesPerPixel` (the spec allows exactly 1 or 3 planes, no alpha).
    #[test]
    fn decode_chunk_rejects_alpha_streams() {
        use gamut_core::{Dimensions, EncodeImage, ImageRef, Rgba16};
        use gamut_jxl::JxlEncoder;
        let dims = Dimensions::new(2, 2).unwrap();
        let px: Vec<u16> = (0..16u16).map(|i| i * 4096).collect();
        let mut bytes = Vec::new();
        JxlEncoder::lossless()
            .encode_image(ImageRef::<Rgba16>::new(&px, dims).unwrap(), &mut bytes)
            .unwrap();
        let err = decode_chunk(&bytes, 2, 2, 3).unwrap_err();
        assert!(
            matches!(err, ref error if error.kind() == gamut_core::ErrorKind::InvalidInput && error.static_message().is_some_and(|message| message.contains("channel"))),
            "{err:?}"
        );
    }
}
