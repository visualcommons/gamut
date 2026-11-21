//! HDR static metadata: [`ContentLightLevel`] and [`MasteringDisplay`], the two SMPTE/CTA
//! descriptions every HDR-capable format carries beside its colour code points ([`crate::Cicp`]).
//!
//! One representation for every carrier that shares their units — the ISOBMFF `clli`/`mdcv`
//! properties of an AVIF or HEIC file, the HEVC SEI messages they come from, and (for the mastering
//! display) the PNG `mDCV` chunk. Every field is the stored integer code point, not a converted
//! float, so a value read from one file and written to another is bit-exact.

/// Content light level `MaxCLL`/`MaxPALL`, in whole cd/m² — the ISOBMFF `clli` property
/// (ISO/IEC 14496-12 `ContentLightLevelBox`), with the semantics of the HEVC content light level
/// SEI (ISO/IEC 23008-2 §D.3.35) and CTA-861.3.
///
/// Not the PNG `cLLI` chunk's unit: PNG §11.3.2.8 stores both levels as four-byte integers in
/// units of 0.0001 cd/m², a finer and wider range than this type's `u16` whole cd/m², so
/// `gamut-png` carries its own type and converts from this one exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ContentLightLevel {
    /// Maximum content light level (`MaxCLL`).
    pub max_content_light_level: u16,
    /// Maximum picture average light level (`MaxPALL`).
    pub max_pic_average_light_level: u16,
}

/// Mastering display colour volume — SMPTE ST 2086 metadata describing the display the content
/// was mastered on: the ISOBMFF `mdcv` property (ISO/IEC 14496-12
/// `MasteringDisplayColourVolumeBox`, a plain `Box` with the `SmDm` field layout per AV1-ISOBMFF
/// §2.3.4; semantics from the HEVC mastering display colour volume SEI, ISO/IEC 23008-2 Annex D)
/// and the PNG `mDCV` chunk (PNG 3rd edition §11.3.2.7, Table 19), which share these units.
///
/// Every field is the stored integer code point, not a converted float, so a value read from one
/// file and written to another is bit-exact. Nothing range-checks them: neither carrier does, and
/// a reader rejecting out-of-range values would refuse files the world holds.
///
/// Non-exhaustive: construct it with [`MasteringDisplay::new`], not a struct literal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub struct MasteringDisplay {
    /// The three display primaries as `[x, y]` CIE 1931 chromaticity pairs in units of 0.00002,
    /// in stored order. The carriers disagree on that order: the HEVC convention the ISOBMFF box
    /// follows is green, blue, red, while PNG §11.3.2.7 orders them by chromaticity (largest `x`,
    /// then largest `y`, then the remaining one — red, green, blue for an RGB space). Neither
    /// carrier enforces its order, so they are kept exactly as stored, and moving a value from
    /// one carrier to the other is the caller's reordering.
    pub display_primaries: [[u16; 2]; 3],
    /// The white point as an `[x, y]` CIE 1931 chromaticity pair in units of 0.00002.
    pub white_point: [u16; 2],
    /// Nominal maximum display luminance, in units of 0.0001 cd/m².
    pub max_display_mastering_luminance: u32,
    /// Nominal minimum display luminance, in units of 0.0001 cd/m².
    pub min_display_mastering_luminance: u32,
}

impl MasteringDisplay {
    /// The stored code points: the three `[x, y]` display primaries in stored order, the
    /// `[x, y]` white point (both in units of 0.00002), and the maximum and minimum mastering
    /// luminance (in units of 0.0001 cd/m²).
    #[must_use]
    pub fn new(
        display_primaries: [[u16; 2]; 3],
        white_point: [u16; 2],
        max_display_mastering_luminance: u32,
        min_display_mastering_luminance: u32,
    ) -> Self {
        Self {
            display_primaries,
            white_point,
            max_display_mastering_luminance,
            min_display_mastering_luminance,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Kills a [`MasteringDisplay::new`] that swaps or drops an argument: every field differs.
    #[test]
    fn mastering_display_new_places_each_argument_in_its_field() {
        let mdcv = MasteringDisplay::new([[1, 2], [3, 4], [5, 6]], [7, 8], 9, 10);
        assert_eq!(mdcv.display_primaries, [[1, 2], [3, 4], [5, 6]]);
        assert_eq!(mdcv.white_point, [7, 8]);
        assert_eq!(mdcv.max_display_mastering_luminance, 9);
        assert_eq!(mdcv.min_display_mastering_luminance, 10);
    }
}
