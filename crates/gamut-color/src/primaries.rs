//! RGB colour spaces by their colorimetry: the CIE 1931 [`Chromaticities`] of a set of primaries
//! and its white point, and the [`RgbGamut`]s the colour science computes in.
//!
//! [`RgbGamut`] is a colour-science gamut, distinct from the CICP
//! [`ColorPrimaries`](crate::cicp::ColorPrimaries) code point: Adobe RGB and ProPhoto RGB have no
//! H.273 code point, so the two enums cannot be one. Both answer the same question —
//! [`RgbGamut::chromaticities`] and
//! [`ColorPrimaries::chromaticities`](crate::cicp::ColorPrimaries::chromaticities) return one
//! [`Chromaticities`] type — and where a gamut has a code point they return the same numbers.

use crate::matrix::{
    ADOBE_RGB_PRIMARIES, BT2020_PRIMARIES, D50, D65, DISPLAY_P3_PRIMARIES, PROPHOTO_PRIMARIES,
    SRGB_PRIMARIES,
};

/// The CIE 1931 chromaticities of an RGB colour space: its three primaries and its white point,
/// each an `(x, y)` pair.
///
/// Plain `Copy` data, laid out exactly as [`rgb_to_xyz_matrix`](crate::matrix::rgb_to_xyz_matrix)
/// takes its arguments: `rgb_to_xyz_matrix(&c.primaries, c.white)`.
///
/// # Examples
///
/// ```
/// use gamut_color::RgbGamut;
/// use gamut_color::matrix::{D50, rgb_to_xyz_matrix};
///
/// let c = RgbGamut::ProPhotoRgb.chromaticities();
/// assert_eq!(c.white, D50);
/// let m = rgb_to_xyz_matrix(&c.primaries, c.white).unwrap();
/// // Row 1 of an RGB→XYZ matrix is the luminance (Y) weighting, so it sums to 1.
/// assert!((m[1].iter().sum::<f64>() - 1.0).abs() < 1e-12);
/// ```
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Chromaticities {
    /// The `[R, G, B]` primaries, each `[x, y]`.
    pub primaries: [[f64; 2]; 3],
    /// The white point, `[x, y]`.
    pub white: [f64; 2],
}

/// Source RGB gamut of the colour science, selecting its [`Chromaticities`] and the OKLab
/// linear-RGB → LMS ([`m1_matrix`](Self::m1_matrix)) projection.
///
/// See the [module docs](self) for how it relates to the CICP
/// [`ColorPrimaries`](crate::cicp::ColorPrimaries), and [`crate::profile`] for the mapping onto
/// CICP axes.
///
/// `#[repr(u8)]` with explicit, permanent, append-only discriminants: this is plain configuration
/// data that must stay mechanically portable to C (see the workspace charter); a new gamut takes
/// the next free discriminant and never renumbers an existing one. `#[non_exhaustive]`: more gamuts
/// (with their derived `M1` matrices) may be added later.
#[repr(u8)]
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RgbGamut {
    /// sRGB / BT.709 primaries, D65.
    Srgb = 0,
    /// Display P3 (DCI-P3 primaries), D65.
    DisplayP3 = 1,
    /// Adobe RGB (1998), D65.
    AdobeRgb = 2,
    /// ITU-R BT.2020 primaries, D65.
    Bt2020 = 3,
    /// ProPhoto RGB (ROMM), D50 (Bradford-adapted to D65 in [`m1_matrix`](Self::m1_matrix)).
    ProPhotoRgb = 4,
}

impl RgbGamut {
    /// The CIE 1931 chromaticities of this gamut.
    ///
    /// Total, unlike [`ColorPrimaries::chromaticities`](crate::cicp::ColorPrimaries::chromaticities):
    /// every `RgbGamut` has primaries, including the two with no ITU-T H.273 code point (Adobe RGB
    /// and ProPhoto RGB). Where a gamut *does* have a code point the two accessors return the same
    /// numbers. ProPhoto RGB is the one gamut whose white is D50 rather than D65.
    ///
    /// # Examples
    ///
    /// ```
    /// use gamut_color::RgbGamut;
    /// use gamut_color::matrix::D50;
    ///
    /// let c = RgbGamut::ProPhotoRgb.chromaticities();
    /// assert_eq!(c.white, D50);
    /// assert_eq!(c.primaries[2], [0.036598, 0.000105]);
    /// ```
    #[must_use]
    pub fn chromaticities(self) -> Chromaticities {
        let (primaries, white) = match self {
            RgbGamut::Srgb => (SRGB_PRIMARIES, D65),
            RgbGamut::DisplayP3 => (DISPLAY_P3_PRIMARIES, D65),
            RgbGamut::AdobeRgb => (ADOBE_RGB_PRIMARIES, D65),
            RgbGamut::Bt2020 => (BT2020_PRIMARIES, D65),
            RgbGamut::ProPhotoRgb => (PROPHOTO_PRIMARIES, D50),
        };
        Chromaticities { primaries, white }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two gamuts with no H.273 code point carry the chromaticities their own encoding
    /// documents publish, transcribed here independently of `matrix.rs`: Adobe RGB (1998) Color
    /// Image Encoding §4.3.1.1 (`references/color/adobe-rgb-1998.pdf`) to the four places it
    /// states, and the ROMM RGB white paper (`references/color/romm-rgb.pdf`, the free stand-in
    /// for ISO 22028-2) to *its* four places — the crate carries ProPhoto to six, so the ROMM
    /// gate is half the paper's last digit. These are the only two `RgbGamut`s
    /// `ColorPrimaries::chromaticities` cannot vouch for.
    #[test]
    fn uncoded_gamuts_carry_their_published_chromaticities() {
        assert_eq!(
            RgbGamut::AdobeRgb.chromaticities(),
            Chromaticities {
                primaries: [[0.6400, 0.3300], [0.2100, 0.7100], [0.1500, 0.0600]],
                white: [0.3127, 0.3290],
            }
        );
        let c = RgbGamut::ProPhotoRgb.chromaticities();
        let romm = [[0.7347, 0.2653], [0.1596, 0.8404], [0.0366, 0.0001]];
        for (got, want) in c.primaries.iter().flatten().zip(romm.iter().flatten()) {
            assert!((got - want).abs() <= 0.5e-4, "ROMM primary {got} vs {want}");
        }
        assert_eq!(c.white, [0.3457, 0.3585], "ROMM white is D50");
    }

    /// Where a gamut has an H.273 primaries code point, the total accessor agrees with the
    /// code-point one: the two must never become two copies of the same colorimetry.
    #[test]
    fn coded_gamuts_agree_with_their_color_primaries() {
        use crate::cicp::ColorPrimaries;
        for (gamut, primaries) in [
            (RgbGamut::Srgb, ColorPrimaries::Bt709),
            (RgbGamut::DisplayP3, ColorPrimaries::DisplayP3),
            (RgbGamut::Bt2020, ColorPrimaries::Bt2020),
        ] {
            assert_eq!(
                Some(gamut.chromaticities()),
                primaries.chromaticities(),
                "{gamut:?}"
            );
        }
    }

    #[test]
    fn rgb_gamut_discriminants_are_the_frozen_c_abi_values() {
        // Permanent and append-only: a C caller may pass the raw discriminant.
        assert_eq!(RgbGamut::Srgb as u8, 0);
        assert_eq!(RgbGamut::DisplayP3 as u8, 1);
        assert_eq!(RgbGamut::AdobeRgb as u8, 2);
        assert_eq!(RgbGamut::Bt2020 as u8, 3);
        assert_eq!(RgbGamut::ProPhotoRgb as u8, 4);
    }
}
