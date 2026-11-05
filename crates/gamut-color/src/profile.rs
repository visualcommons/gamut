//! Source-profile bundle: ties a colour-science [`RgbGamut`] together with its
//! encoder-exact transfer and projects them back onto gamut's independent CICP
//! axes.
//!
//! gamut models primaries and transfer as *separate* CICP axes, but an upstream
//! like chromahash treats a single `Gamut` as a fixed bundle — e.g. `Bt2020`
//! means "BT.2020 primaries **and** PQ **and** Reinhard@203". A [`SourceProfile`]
//! is that bundle: it carries the `(gamut, transfer)` pair, the convenience
//! gamma-RGB → OKLab pipeline, and accessors that decompose it into
//! [`ColorPrimaries`] / [`TransferCharacteristics`].
//!
//! The HDR (BT.2020 PQ) path folds its Reinhard tone map into the encoder-exact
//! transfer (see [`crate::transfer::bt2020_pq_to_sdr`]); the general-purpose
//! tone-curve toolkit is the separate `gamut-tonemap` crate. Chromatic adaptation
//! needs no field here — it is already baked into the per-gamut `M1` matrix
//! (ProPhoto's D50→D65; see [`crate::matrix`]).

use crate::cicp::{ColorPrimaries, TransferCharacteristics};
use crate::oklab::linear_rgb_to_oklab;
use crate::primaries::RgbGamut;
use crate::transfer::{
    adobe_rgb_eotf, bt2020_pq_to_sdr, linear_eotf, prophoto_rgb_eotf, srgb_eotf,
};

/// The encoder-exact per-channel transfer a [`SourceProfile`] linearizes with.
///
/// `#[non_exhaustive]`: more transfers (HLG is the named next candidate) may be added later.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceTransfer {
    /// Linear — the identity transfer (CICP code point 8). The working space of a
    /// scene-linear pipeline (RAW demosaic / white balance / colour matrix), which
    /// operates on already-linear samples and applies its output transfer afterwards.
    Linear,
    /// sRGB EOTF (IEC 61966-2-1).
    Srgb,
    /// Adobe RGB, pure `x^2.2`.
    AdobeRgb,
    /// ProPhoto RGB, pure `x^1.8`.
    ProPhotoRgb,
    /// BT.2020 PQ EOTF → nits → Reinhard@203 (tone-mapped to SDR).
    Bt2020Pq,
}

impl SourceTransfer {
    /// Apply the gamma-encoded → linear (SDR) transfer to one channel.
    #[must_use]
    pub fn eotf(self, x: f64) -> f64 {
        match self {
            SourceTransfer::Linear => linear_eotf(x),
            SourceTransfer::Srgb => srgb_eotf(x),
            SourceTransfer::AdobeRgb => adobe_rgb_eotf(x),
            SourceTransfer::ProPhotoRgb => prophoto_rgb_eotf(x),
            SourceTransfer::Bt2020Pq => bt2020_pq_to_sdr(x),
        }
    }

    /// The CICP [`TransferCharacteristics`] code point, when one exists. Adobe
    /// RGB and ProPhoto RGB have no CICP transfer code point.
    #[must_use]
    pub fn transfer_characteristics(self) -> Option<TransferCharacteristics> {
        match self {
            SourceTransfer::Linear => Some(TransferCharacteristics::Linear),
            SourceTransfer::Srgb => Some(TransferCharacteristics::Srgb),
            SourceTransfer::Bt2020Pq => Some(TransferCharacteristics::Pq),
            SourceTransfer::AdobeRgb | SourceTransfer::ProPhotoRgb => None,
        }
    }
}

/// A source colour profile: a [`RgbGamut`] plus its encoder-exact [`SourceTransfer`].
///
/// # Examples
///
/// ```
/// use gamut_color::SourceProfile;
/// use gamut_color::cicp::{ColorPrimaries, TransferCharacteristics};
/// let p = SourceProfile::SRGB;
/// assert_eq!(p.color_primaries(), Some(ColorPrimaries::Bt709));
/// assert_eq!(p.transfer_characteristics(), Some(TransferCharacteristics::Srgb));
/// // Convenience pipeline: gamma-encoded sRGB → OKLab.
/// let lab = p.gamma_rgb_to_oklab([1.0, 1.0, 1.0]);
/// assert!((lab[0] - 1.0).abs() < 1e-6);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceProfile {
    /// The colour-science gamut (selects the `M1` matrix / primaries).
    pub gamut: RgbGamut,
    /// The encoder-exact transfer used to linearize source samples.
    pub transfer: SourceTransfer,
}

impl SourceProfile {
    /// sRGB: BT.709 primaries + sRGB transfer.
    pub const SRGB: Self = Self {
        gamut: RgbGamut::Srgb,
        transfer: SourceTransfer::Srgb,
    };
    /// Linear sRGB: BT.709 primaries + linear transfer — the scene-linear working
    /// space, same gamut as [`SourceProfile::SRGB`] with the gamma curve removed.
    pub const LINEAR_SRGB: Self = Self {
        gamut: RgbGamut::Srgb,
        transfer: SourceTransfer::Linear,
    };
    /// Display P3: DCI-P3 primaries + sRGB transfer.
    pub const DISPLAY_P3: Self = Self {
        gamut: RgbGamut::DisplayP3,
        transfer: SourceTransfer::Srgb,
    };
    /// Adobe RGB (1998): Adobe primaries + pure `x^2.2`.
    pub const ADOBE_RGB: Self = Self {
        gamut: RgbGamut::AdobeRgb,
        transfer: SourceTransfer::AdobeRgb,
    };
    /// BT.2020: BT.2020 primaries + PQ + Reinhard@203.
    pub const BT2020: Self = Self {
        gamut: RgbGamut::Bt2020,
        transfer: SourceTransfer::Bt2020Pq,
    };
    /// ProPhoto RGB: ProPhoto primaries (Bradford D50→D65) + pure `x^1.8`.
    pub const PROPHOTO_RGB: Self = Self {
        gamut: RgbGamut::ProPhotoRgb,
        transfer: SourceTransfer::ProPhotoRgb,
    };

    /// The CICP [`ColorPrimaries`] code point, when one exists. Adobe RGB and
    /// ProPhoto RGB have no CICP primaries code point.
    #[must_use]
    pub fn color_primaries(self) -> Option<ColorPrimaries> {
        match self.gamut {
            // BT.709 and sRGB share primaries (CICP code point 1).
            RgbGamut::Srgb => Some(ColorPrimaries::Bt709),
            RgbGamut::DisplayP3 => Some(ColorPrimaries::DisplayP3),
            RgbGamut::Bt2020 => Some(ColorPrimaries::Bt2020),
            RgbGamut::AdobeRgb | RgbGamut::ProPhotoRgb => None,
        }
    }

    /// The CICP [`TransferCharacteristics`] code point, when one exists.
    #[must_use]
    pub fn transfer_characteristics(self) -> Option<TransferCharacteristics> {
        self.transfer.transfer_characteristics()
    }

    /// Linearize one gamma-encoded channel with this profile's transfer.
    #[must_use]
    pub fn eotf(self, x: f64) -> f64 {
        self.transfer.eotf(x)
    }

    /// Convert a gamma-encoded source RGB triple to OKLab: apply the transfer
    /// per channel, then project through the gamut's `M1` matrix.
    #[must_use]
    pub fn gamma_rgb_to_oklab(self, rgb: [f64; 3]) -> [f64; 3] {
        let linear = [self.eotf(rgb[0]), self.eotf(rgb[1]), self.eotf(rgb[2])];
        linear_rgb_to_oklab(linear, self.gamut)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profiles_decompose_to_cicp_axes() {
        // Every named profile projects onto both CICP axes: Adobe RGB and ProPhoto have no code
        // points, and Display P3 reuses the sRGB transfer with its own primaries. (The Reinhard
        // tone map folded into the BT.2020 transfer is exercised by
        // `eotf_is_encoder_exact_per_gamut`.)
        use ColorPrimaries as Cp;
        use TransferCharacteristics as Tc;
        let cases: &[(SourceProfile, Option<Cp>, Option<Tc>)] = &[
            (SourceProfile::SRGB, Some(Cp::Bt709), Some(Tc::Srgb)),
            (
                SourceProfile::LINEAR_SRGB,
                Some(Cp::Bt709),
                Some(Tc::Linear),
            ),
            (
                SourceProfile::DISPLAY_P3,
                Some(Cp::DisplayP3),
                Some(Tc::Srgb),
            ),
            (SourceProfile::ADOBE_RGB, None, None),
            (SourceProfile::BT2020, Some(Cp::Bt2020), Some(Tc::Pq)),
            (SourceProfile::PROPHOTO_RGB, None, None),
        ];
        for &(p, primaries, transfer) in cases {
            // Labelled with the whole profile, not just the gamut: SRGB and LINEAR_SRGB
            // share `RgbGamut::Srgb` and differ only in transfer.
            assert_eq!(p.color_primaries(), primaries, "{p:?} primaries");
            assert_eq!(p.transfer_characteristics(), transfer, "{p:?} transfer");
        }
    }

    #[test]
    fn eotf_is_encoder_exact_per_gamut() {
        // Adobe is pure x^2.2, ProPhoto pure x^1.8 — distinct at the same input.
        assert_eq!(SourceProfile::ADOBE_RGB.eotf(0.5), 0.5_f64.powf(2.2));
        assert_eq!(SourceProfile::PROPHOTO_RGB.eotf(0.5), 0.5_f64.powf(1.8));
        // BT.2020 folds in the PQ + Reinhard tone map.
        assert_eq!(SourceProfile::BT2020.eotf(0.5), bt2020_pq_to_sdr(0.5));
        // Linear sRGB is the identity across the domain — in particular it is *not*
        // the sRGB curve, which bends at every interior point.
        for &x in &[0.0, 0.25, 0.5, 0.75, 1.0] {
            assert_eq!(SourceProfile::LINEAR_SRGB.eotf(x), x);
        }
        assert_ne!(
            SourceProfile::SRGB.eotf(0.5),
            SourceProfile::LINEAR_SRGB.eotf(0.5)
        );
    }

    #[test]
    fn linear_srgb_skips_the_gamma_step() {
        // Same gamut as SRGB, so feeding LINEAR_SRGB the already-linearized samples
        // must land on the same OKLab point as feeding SRGB the gamma-encoded ones.
        let gamma = [0.5, 0.25, 0.75];
        let linear = gamma.map(srgb_eotf);
        let via_srgb = SourceProfile::SRGB.gamma_rgb_to_oklab(gamma);
        let via_linear = SourceProfile::LINEAR_SRGB.gamma_rgb_to_oklab(linear);
        for (i, (&a, &b)) in via_srgb.iter().zip(via_linear.iter()).enumerate() {
            assert!((a - b).abs() < 1e-12, "oklab[{i}]: {a} vs {b}");
        }
    }

    /// Golden `gamma_rgb` vectors transcribed from chromahash
    /// `spec/test-vectors/unit-color.json` (MIT OR Apache-2.0).
    #[test]
    fn matches_chromahash_gamma_pipeline_vectors() {
        let cases: &[([f64; 3], [f64; 3])] = &[
            // gamma_mid_srgb: a genuine EOTF point (eotf(0.5) ≈ 0.214), so this exercises the
            // gamma → linear → OKLab composition. (A gamma_red [1,0,0] case is omitted: sRGB EOTF is
            // the identity at 0 and 1, so it collapses to oklab's linear [1,0,0] vector.)
            (
                [0.5, 0.5, 0.5],
                [
                    0.5981807266228486,
                    0.000000000048424320109319297,
                    0.000000022296533230825588,
                ],
            ),
        ];
        for &(gamma_rgb, want) in cases {
            let got = SourceProfile::SRGB.gamma_rgb_to_oklab(gamma_rgb);
            for (i, (&g, &w)) in got.iter().zip(want.iter()).enumerate() {
                assert!((g - w).abs() < 1e-9, "oklab[{i}] = {g}, want {w}");
            }
        }
    }
}
