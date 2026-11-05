//! Non-raw sub-images of a DNG: previews, transparency masks, depth maps, enhanced images, and
//! semantic masks (DNG 1.6+ — what Apple ProRAW uses for subject/sky/skin masks).
//!
//! Every image IFD in the file other than the main raw is surfaced as a [`SubImage`]: its role
//! ([`SubImageKind`], from `NewSubFileType`), geometry, photometry, and pixel data. Pixels
//! decode through the same strip/tile pipeline as the raw image where the compression is in
//! decode scope; otherwise the compressed chunks are carried verbatim as
//! [`SubImageData::Undecoded`] — explicitly represented either way, never dropped (issue #109's
//! decode contract). Semantic-mask IFDs additionally carry their [`SemanticMaskInfo`]
//! (`SemanticName`/`SemanticInstanceID`/`MaskSubArea`).

use gamut_core::Dimensions;

/// The role of a non-raw sub-image, from its IFD's `NewSubFileType` (DNG 1.7.1 pp. 22-23).
///
/// Unrecognised bit patterns are preserved as [`Other`](Self::Other), never dropped.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubImageKind {
    /// `1` — a reduced-resolution preview of the main image.
    Preview,
    /// `0x10001` — an alternative (non-primary) preview.
    AltPreview,
    /// `4` — a transparency mask.
    TransparencyMask,
    /// `5` — a reduced-resolution transparency mask.
    TransparencyMaskReduced,
    /// `8` — a depth map.
    DepthMap,
    /// `9` — a reduced-resolution depth map.
    DepthMapReduced,
    /// `16` — an enhanced (processed) version of the main image (DNG 1.5).
    EnhancedImage,
    /// `32` — a gain map image (DNG 1.7.1).
    GainMap,
    /// `0x10004` — a semantic mask (DNG 1.6); see [`SemanticMaskInfo`].
    SemanticMask,
    /// A full-resolution main image (`NewSubFileType` 0) that was not selected as the raw —
    /// e.g. a second full-resolution IFD.
    MainImage,
    /// Any other `NewSubFileType` bit pattern, preserved verbatim.
    Other(u32),
}

impl SubImageKind {
    /// Classifies an on-disk `NewSubFileType` value.
    #[must_use]
    pub fn from_code(code: u32) -> Self {
        match code {
            0 => SubImageKind::MainImage,
            1 => SubImageKind::Preview,
            4 => SubImageKind::TransparencyMask,
            5 => SubImageKind::TransparencyMaskReduced,
            8 => SubImageKind::DepthMap,
            9 => SubImageKind::DepthMapReduced,
            16 => SubImageKind::EnhancedImage,
            32 => SubImageKind::GainMap,
            0x0001_0001 => SubImageKind::AltPreview,
            0x0001_0004 => SubImageKind::SemanticMask,
            other => SubImageKind::Other(other),
        }
    }

    /// The on-disk `NewSubFileType` value.
    #[must_use]
    pub fn code(self) -> u32 {
        match self {
            SubImageKind::MainImage => 0,
            SubImageKind::Preview => 1,
            SubImageKind::TransparencyMask => 4,
            SubImageKind::TransparencyMaskReduced => 5,
            SubImageKind::DepthMap => 8,
            SubImageKind::DepthMapReduced => 9,
            SubImageKind::EnhancedImage => 16,
            SubImageKind::GainMap => 32,
            SubImageKind::AltPreview => 0x0001_0001,
            SubImageKind::SemanticMask => 0x0001_0004,
            SubImageKind::Other(code) => code,
        }
    }
}

/// A sub-image's pixel payload: decoded samples, or the verbatim compressed chunks when the
/// scheme is outside decode scope (e.g. a baseline-DCT JPEG preview, or lossy JPEG 34892).
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubImageData {
    /// Decoded samples, `width * height * samples_per_pixel` long. JPEG XL data is full-range
    /// 16-bit (see [`crate::decoder`]); other schemes carry code values at the IFD's bit depth.
    Decoded(Vec<u16>),
    /// The stored chunks (strips or tiles, in offset order), verbatim.
    Undecoded {
        /// The IFD's `Compression` code.
        compression: u16,
        /// The compressed chunk bytes, in offset-array order.
        chunks: Vec<Vec<u8>>,
    },
}

/// `MaskSubArea` (52536): how a cropped semantic mask places into the full mask area (which
/// corresponds to the main image's `ActiveArea`); DNG 1.7.1 pp. 73-74.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MaskSubArea {
    /// `T_crop` — top coordinate of the cropped mask within the full mask.
    pub top: u32,
    /// `L_crop` — left coordinate of the cropped mask within the full mask.
    pub left: u32,
    /// `W_full` — width of the full (uncropped) mask.
    pub full_width: u32,
    /// `H_full` — height of the full (uncropped) mask.
    pub full_height: u32,
}

/// The semantic-mask tags of a [`SubImageKind::SemanticMask`] IFD (DNG 1.7.1 pp. 73-74).
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SemanticMaskInfo {
    /// `SemanticName` (52526) — the mask's purpose, e.g. Apple's `"Person"`/`"Sky"` (required
    /// by the spec, but surfaced as written).
    pub name: Option<String>,
    /// `SemanticInstanceID` (52528) — distinguishes instances that share a name.
    pub instance_id: Option<String>,
    /// `MaskSubArea` (52536) — the crop placement, when present and valid (the spec says an
    /// invalid tag "should be ignored"; validation pairs top with the mask height and left with
    /// the mask width, as the SDK does — the spec's own inequality text transposes the axes).
    pub sub_area: Option<MaskSubArea>,
}

/// IFD 0's depth-map description tags (DNG 1.5; DNG 1.7.1 pp. 74-76), describing how a
/// [`SubImageKind::DepthMap`] sub-image's values map to distances.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DepthInfo {
    /// `DepthFormat` (51177): 0 unknown, 1 linear, 2 inverse.
    pub format: Option<u16>,
    /// `DepthNear` (51178) as its stored rational — the distance of value 0 (`0/0` = unknown).
    pub near: Option<(u32, u32)>,
    /// `DepthFar` (51179) as its stored rational — the distance of the maximum value (`0/0` =
    /// unknown, `n/0` = infinity).
    pub far: Option<(u32, u32)>,
    /// `DepthUnits` (51180): 0 unknown, 1 metres.
    pub units: Option<u16>,
    /// `DepthMeasureType` (51181): 0 unknown, 1 optical axis, 2 optical ray.
    pub measure_type: Option<u16>,
}

/// One decoded non-raw image IFD (see the module docs).
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub struct SubImage {
    /// The image's role, from `NewSubFileType`.
    pub kind: SubImageKind,
    /// The `PhotometricInterpretation` code, kept verbatim (use
    /// [`interpretation`](Self::interpretation) for the typed reading).
    pub photometric: u16,
    /// The image dimensions.
    pub dimensions: Dimensions,
    /// Bits per sample as declared by the IFD (for JPEG XL, the codestream precision; decoded
    /// JXL samples are full-range 16-bit).
    pub bits_per_sample: u16,
    /// Samples per pixel.
    pub samples_per_pixel: u16,
    /// The pixel payload — decoded, or the verbatim chunks.
    pub data: SubImageData,
    /// Semantic-mask tags, when this is a semantic mask (or carries the tags).
    pub semantic: Option<SemanticMaskInfo>,
    /// Every field of this image's IFD the pipeline does not model, verbatim, in tag order.
    /// Empty for an image stored in IFD 0 itself (those extras are
    /// [`DecodedDng::ifd0_extra`](crate::DecodedDng::ifd0_extra)).
    pub extra_tags: Vec<crate::decoder::RawTag>,
}

impl SubImage {
    /// The typed photometric interpretation, when the code is one the crate models.
    #[must_use]
    pub fn interpretation(&self) -> Option<crate::values::PhotometricInterpretation> {
        crate::values::dng_photometric(self.photometric)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_codes_roundtrip_exhaustively() {
        for (kind, code) in [
            (SubImageKind::MainImage, 0u32),
            (SubImageKind::Preview, 1),
            (SubImageKind::TransparencyMask, 4),
            (SubImageKind::TransparencyMaskReduced, 5),
            (SubImageKind::DepthMap, 8),
            (SubImageKind::DepthMapReduced, 9),
            (SubImageKind::EnhancedImage, 16),
            (SubImageKind::GainMap, 32),
            (SubImageKind::AltPreview, 0x0001_0001),
            (SubImageKind::SemanticMask, 0x0001_0004),
            (SubImageKind::Other(0xDEAD), 0xDEAD),
        ] {
            assert_eq!(SubImageKind::from_code(code), kind, "{code:#x}");
            assert_eq!(kind.code(), code, "{kind:?}");
        }
    }
}
