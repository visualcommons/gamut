//! Built-in profile constructors: a spec-valid [`IccProfile`] for a named colour space, for a
//! [`SourceProfile`], or for a CICP signalling triple.
//!
//! Every profile built here is a **v4 three-component matrix/TRC display profile** (ICC.1:2022
//! §8.4) — or, for [`IccProfile::gray_with_gamma`], the monochrome model of the same class.
//!
//! **Colorimetry `gamut-color` can supply is never retyped here**: the primaries and white point
//! come from [`gamut_color::cicp::ColorPrimaries::chromaticities`] for a CICP code point and from
//! [`gamut_color::RgbGamut::chromaticities`] for a named space, the RGB→XYZ construction and
//! the Bradford adaptation to the D50 PCS from [`gamut_color::matrix`], and the ST 2084 curve and
//! the BT.709-family α and β ([`BT709_ALPHA`](gamut_color::transfer::BT709_ALPHA) /
//! [`BT709_BETA`](gamut_color::transfer::BT709_BETA), H.273 §8.2) from
//! [`gamut_color::transfer`].
//!
//! Four published constants it does not supply *are* written here, because an ICC tag needs them
//! in a form no `gamut-color` function returns. Each is gated against something outside this
//! module rather than trusted:
//!
//! | Restated here | Why it cannot be borrowed | What gates it |
//! | --- | --- | --- |
//! | The IEC 61966-2-1 `(g, a, b, c, d)` set for [`Trc::Srgb`] | `gamut_color::transfer::srgb_eotf` is a *function*; §10.18 type 3 needs its five parameters | `srgb_parametric_curve_matches_gamut_color` against that function, and `oracle_every_encodable_transfer_matches_its_curve_through_lcms` against lcms2's own sRGB |
//! | [`ADOBE_RGB_GAMMA`], `563 / 256` (Adobe RGB (1998) §4.3.1.2) | `gamut_color::transfer::adobe_rgb_eotf_standard` is a *function*; §10.18 type 0 needs its exponent | `uncoded_space_curves_match_gamut_color_standard_curves` against that function, and `oracle_uncoded_space_curves_match_gamut_color_through_lcms` through lcms2 |
//! | The ROMM RGB `(g, a, b, c, d)` set for [`Trc::Romm`] (ISO 22028-2, via the ROMM RGB white paper) | `gamut_color::transfer::prophoto_rgb_eotf_standard` is a *function*; §10.18 type 3 needs its five parameters | the same two tests, against `prophoto_rgb_eotf_standard` |
//! | The PCS D50, via [`XyzNumber::D50`] | It is an **ICC** fact (§7.2.16), not a CIE one; see [`pcs_d50_chromaticity`](synthesis::pcs_d50_chromaticity) | `colorants_sum_to_the_declared_media_white_point`, and the lcms2 colorant oracle |
//!
//! Everything else this module contributes is ICC encoding, not colour science.
//!
//! # Which spaces
//!
//! A matrix/TRC profile *is* a (primaries, transfer) pair, and the two axes are resolved from
//! different places. **Primaries** come from `gamut-color`: a code point it gives no
//! chromaticities for cannot be described. Adobe RGB and ProPhoto RGB have no ITU-T H.273 code
//! point on either axis — [`gamut_color::SourceProfile`]'s `ADOBE_RGB` and `PROPHOTO_RGB` return
//! `None` from both [`colour_primaries`](gamut_color::SourceProfile::color_primaries) and
//! [`transfer_characteristics`](gamut_color::SourceProfile::transfer_characteristics) — so they
//! are reachable only as the named [`BuiltinProfile::AdobeRgb`] and
//! [`BuiltinProfile::ProPhotoRgb`], whose primaries come from the total
//! [`RgbGamut::chromaticities`](gamut_color::RgbGamut::chromaticities), whose curves are each
//! encoding's standard one, and whose profiles carry no `cicpType` tag. ProPhoto is the one space
//! whose own white is D50, so its `chad` is not a real adaptation — only the step from the CIE
//! chromaticity `(0.3457, 0.3585)` to the PCS's rounded `XYZNumber` (see
//! [`pcs_d50_chromaticity`](synthesis::pcs_d50_chromaticity)), 2.5e-4 from the identity at its largest element.
//! **Transfers** of a CICP triple are keyed on the raw ITU-T H.273 code point, because the set of
//! curves an ICC tag can *encode* is not the set `gamut-color` can *evaluate*: H.273 Table 3's
//! code points 4 and 5 are pure gammas ICC.1:2022 §10.18 encodes exactly, yet
//! `gamut_color::cicp::TransferCharacteristics` models neither. Code points 1, 6, 14 and 15 are
//! one curve with a closed form §10.18 defines exactly, so this module encodes all four, and
//! `TransferCharacteristics` models all four too; `gamut_color::transfer::standard_eotf_for` maps
//! each to the same Table 3 curve this module writes.
//!
//! ```
//! use gamut_color::cicp::TransferCharacteristics as Tc;
//! use gamut_color::transfer::standard_eotf_for;
//! use gamut_core::Cicp;
//! use gamut_icc::IccProfile;
//!
//! // Each of the four BT.709-family code points has a `TransferCharacteristics` variant …
//! assert_eq!(Tc::from_code_point(1), Some(Tc::Bt709));
//! assert_eq!(Tc::from_code_point(6), Some(Tc::Bt601));
//! assert_eq!(Tc::from_code_point(14), Some(Tc::Bt2020_10));
//! assert_eq!(Tc::from_code_point(15), Some(Tc::Bt2020_12));
//! // … and each evaluates Table 3's curve: V = 0.5 is 0.259719 in light, the value this module
//! // writes for the same code points (pinned by `bt709_curve_inverts_the_h273_transfer`).
//! for tc in [Tc::Bt709, Tc::Bt601, Tc::Bt2020_10, Tc::Bt2020_12] {
//!     let light = standard_eotf_for(tc).expect("BT.709-family code point has an EOTF")(0.5);
//!     assert!((light - 0.259_719).abs() < 1e-6);
//! }
//! // The display gammas 2.2 and 2.8 have no variant …
//! assert_eq!(Tc::from_code_point(4), None);
//! assert_eq!(Tc::from_code_point(5), None);
//!
//! // … yet are encodable here, as are all four BT.709-family code points; that the latter
//! // encode one curve is pinned by `every_bt709_family_code_point_builds_the_same_curve`.
//! let axes = Cicp { colour_primaries: 1, transfer_characteristics: 1,
//!                   matrix_coefficients: 0, full_range: true };
//! for code in [1, 4, 5, 6, 14, 15] {
//!     assert!(IccProfile::from_cicp(Cicp { transfer_characteristics: code, ..axes }).is_some());
//! }
//! ```
//!
//! # What a CICP triple contributes, and what it does not
//!
//! [`IccProfile::from_cicp`] takes all four H.273 fields but builds from two of them. Of the other
//! two, one is always rewritten and one is a precondition on the RGB samples. They do **not** rest
//! on the same authority, and are documented apart on purpose.
//!
//! `MatrixCoefficients` is **conformance**, and is rewritten. ICC.1:2022 §10.3 states that *"when
//! the data colour space in the profile header is RGB or XYZ, MatrixCoefficients shall be 0
//! (zero)"* — a `shall` — so the caller's value cannot be carried: an AVIF or HEIC `nclx` box
//! routinely signals 1, 5, 6 or 9, and writing any of those into an RGB profile would make it
//! non-conforming. Nothing is lost by that. The coefficients describe a luma–chroma *encoding* the
//! caller de-matrixes before the profile applies, and they remain in the container's own
//! signalling where a decoder reads them. The sample *range* that accompanies such an encoding is
//! the next field, and whether it is theirs depends on which coefficients they are.
//!
//! `VideoFullRangeFlag` is **not** conformance. What it scales is fixed by the coefficients beside
//! it — H.273 §8.3 applies it to **R, G and B** when `MatrixCoefficients` is 0, 8, 16 or 17
//! (equations 27 to 29, and 33 to 35), and to **Y, Cb and Cr** when it is 1, 4, 5, 6, 7 or 9 to
//! 15 (equations 30 to 32, and 36 to 38) — so the flag is read as a statement about the RGB
//! samples the profile will see, and the triple is kept only if they are full range:
//!
//! * **Full range (`1`)** under any coefficients: the RGB is full scale, and the flag is kept.
//! * **Narrow range (`0`) under luma–chroma coefficients** — `9-16-9-0`, `1-13-6-0`, what an AVIF
//!   or HEIC `nclx` box most often carries. The narrow range is on Y, Cb and Cr, and E′R, E′G and
//!   E′B come out of de-matrixing in the range 0 to 1: the RGB this profile applies to *is* full
//!   scale. So the profile is built, and its tag carries `1` — the range of the RGB it describes,
//!   exactly as `MatrixCoefficients` is written as the `0` of the RGB it describes.
//! * **Narrow range under RGB coefficients** (0, 8, 16, 17) — §10.3's own `1-1-0-0` and `9-16-0-0`
//!   examples. The narrow range is on the **RGB samples themselves**, de-matrixing does not remove
//!   it, and every colorant, `chad` and tone curve here is defined over full-scale RGB. Declined:
//!   normalising the flag would hand back a profile that renders the caller's colour *wrongly*.
//!   A narrow-range RGB `cicpType` is a legal tag — it is simply not one this module can build,
//!   the same case as primaries with no chromaticities or a transfer with no ICC tone curve. A
//!   caller holding such samples scales them to full range and passes `1`.
//! * **Narrow range under any other coefficients** — ones H.273 leaves unknown (2) or reserved —
//!   says nothing this module can rely on about the RGB, and is declined. (`gamut_core::Cicp`
//!   holds the flag as a `bool`, so no flag value outside H.273's single bit reaches here.)
//!
//! Carrying the caller's `0` through unchanged — the option that looks most conservative, since it
//! discards nothing — is not merely inconsistent. §9.2.17 makes it **non-conforming**: the colour
//! encoding a `cicpType` tag specifies *"shall be equivalent to the data colour space encoding
//! represented by this ICC profile"*, and with `MatrixCoefficients` at `0` a flag of `0` reads as
//! narrow-range RGB, which is not what full-scale colorants and tone curves represent.
//!
//! # Which reading of transfer 1, 6, 14 and 15
//!
//! H.273 Table 3 defines those four code points as an **opto-electronic** function, and an ICC
//! tone curve encodes signal → light, so by default this module writes its exact inverse:
//! `Lc = ((V + α − 1) / α)^(1/0.45)` above the knee. That is the literal reading,
//! [`CicpTransferReading::InverseOetf`], and it is what the reference implementations (libjxl,
//! libavif, Little-CMS) write, so a profile built by [`IccProfile::from_cicp`] agrees with them.
//!
//! It is not the only sanctioned reading. H.273 §8.2 NOTE 1 observes that for these code points
//! "a suggested corresponding reference electro-optical transfer characteristic function for flat
//! panel displays used in HDTV studio production has been specified in Rec. ITU-R BT.1886-0" —
//! which, at reference black zero, is a pure gamma of 2.4. Since every profile built here is a
//! *display*-class profile, that reading has a real claim, so it is offered as an opt-in:
//! [`IccProfile::from_cicp_with`] with [`CicpTransferReading::Bt1886Display`] writes
//! `gamut_color::transfer::bt1886_eotf` as a `parametricCurveType` function type 0 with
//! `g = 2.4` ([`gamut_color::transfer::BT1886_GAMMA`], borrowed rather than restated). The
//! `cicpType` tag is the same under both readings: the signalling did not change, only the curve
//! derived from it. Every other transfer code point has one curve, which both readings write.
//!
//! The two are far apart. At mid-grey `V = 0.5` the PCS `Y` is as follows, quoted as the **tag
//! evaluates**, with the parameters already rounded to `s15Fixed16` — not as the closed form
//! before rounding, which for the inverse OETF is a different number in the sixth decimal
//! (0.259719). `bt1886_reading_is_what_the_docs_quote` asserts all three rows.
//!
//! | reading | `Y` at `V = 0.5` |
//! | --- | --- |
//! | inverse OETF — [`CicpTransferReading::InverseOetf`], the default | 0.259721 |
//! | BT.1886 EOTF, `V^2.4` — [`CicpTransferReading::Bt1886Display`] | 0.189465 |
//! | the sRGB code point (13) as this module writes it, for scale | 0.214045 |
//!
//! The literal reading is 1.371× the BT.1886 one — 0.0703 in absolute `Y`. Note the third row:
//! two code points this module encodes under either reading, 1 and 13, already differ by 0.0457
//! at mid-grey under the literal one (21 % of the sRGB value), so a caller that treats "BT.709
//! primaries" and "sRGB" as interchangeable will see that much of a shift from the transfer alone.
//!
//! [`IccProfile::from_source_profile`] stays on the literal reading. No `SourceProfile` bundle
//! `gamut-color` defines projects onto transfer 1, 6, 14 or 15 today, and one that did would be
//! read the way `gamut-color`'s own `standard_eotf_for` reads those code points — as the inverse OETF.
//!
//! # HDR: the PQ profile is peak-referred
//!
//! [`BuiltinProfile::Bt2020Pq`] (and [`IccProfile::from_cicp`] for transfer 16) samples the
//! ST 2084 EOTF divided by its own 10 000 cd/m² peak, so media white *is* that peak. A
//! diffuse-white signal — BT.2408's 203 cd/m² HDR reference white, `V` ≈ 0.58 — therefore lands
//! at 203 / 10 000 = 0.0203 of media white, and a CMM converting it to sRGB writes ≈ 0.153 (code
//! 39 of 255): dark grey where SDR-referred content expects white. lcms2 measures exactly that,
//! identically under media-relative colorimetric, ICC-absolute colorimetric and perceptual
//! intents (`oracle_pq_diffuse_white_lands_at_its_share_of_the_peak` and
//! `oracle_pq_diffuse_white_is_dark_grey_in_lcms_srgb` in `tests/oracle.rs`).
//!
//! This is kept as the one behaviour, with no diffuse-white-referred sibling and no parameter
//! (issue <https://github.com/visualcommons/gamut/issues/557>), because neither normalization
//! makes a matrix/TRC profile an HDR→SDR renderer:
//!
//! * **Embedding beside HDR content for an HDR-aware pipeline**: peak-referred is right. It is
//!   the only normalization that keeps the whole PQ range inside the `[0, 1]` a `curveType`
//!   encodes, so no signal value is clipped by the profile itself.
//! * **Embedding so an SDR CMM produces something viewable**: neither is right. Referring the
//!   curve to diffuse white puts 203 cd/m² on media white but clips every highlight above it —
//!   the top ~5.6 stops of the PQ range — with no roll-off, because a matrix/TRC profile has
//!   nowhere to put a tone curve that is not also the transfer's.
//!
//! So for an SDR rendering, do not embed this profile: decode PQ to light, tone-map it with
//! `gamut-tonemap` (Reinhard, ACES, Hable, Drago — its `ToneCurve` operators), re-encode, and
//! embed an SDR profile such as [`BuiltinProfile::Srgb`] or [`BuiltinProfile::DisplayP3`].
//!
//! [`IccProfile::validate`] reports nothing about this: the profile is conforming, and the
//! normalization is a statement about which white the caller's content is graded to, which no
//! profile-level check can see.
//!
//! # Rendering intent
//!
//! Every profile built here carries `Perceptual` in the header's rendering-intent field (§7.2.15),
//! which is [`ProfileHeader::new`]'s default. That is worth stating because it is surprising on a
//! colorimetrically exact matrix/TRC profile, where `MediaRelativeColorimetric` is what most
//! callers mean. The field is a *preference* a CMM may override — the profile describes one
//! colour space either way, and the intent that actually renders is chosen at transform time — so
//! it changes nothing about the colorimetry, and a caller who wants a different default sets
//! `profile.header.rendering_intent` before serializing:
//!
//! ```
//! use gamut_icc::{BuiltinProfile, IccProfile, RenderingIntent};
//!
//! let mut p = IccProfile::builtin(BuiltinProfile::Srgb).expect("a modelled space");
//! assert_eq!(p.header.rendering_intent, RenderingIntent::Perceptual);
//! p.header.rendering_intent = RenderingIntent::MediaRelativeColorimetric;
//! assert!(p.validate().is_empty());
//! ```
//!
//! # Determinism
//!
//! A constructor is a pure function of its arguments: the creation date is
//! [`DateTime::ZERO`](crate::DateTime::ZERO), the profile ID is left unset, and every
//! open-registry header field is zero, so the same call always serializes to the same bytes. Stamp
//! an ID with [`IccWriter::recompute_profile_id`](crate::IccWriter::recompute_profile_id) if one
//! is wanted.
//!
//! # Layout
//!
//! This module names the spaces ([`BuiltinProfile`]) and builds them by name. The mapping from a
//! CICP triple to a profile — which fields are read, rewritten or declined — is
//! `cicp_mapping`; the ICC encoding every constructor shares — tone curves, D50 colorants, the
//! matrix/TRC tag set — is `synthesis`.

mod cicp_mapping;
mod synthesis;
#[cfg(test)]
mod tests;

use gamut_color::cicp::{ColorPrimaries, TransferCharacteristics};
use gamut_color::{RgbGamut, SourceProfile};
use gamut_core::Cicp;

pub use self::cicp_mapping::CicpTransferReading;
use self::cicp_mapping::cicp_of;
use self::synthesis::{COPYRIGHT, Trc, encodable_gamma, mluc, rgb_matrix_trc};
use crate::header::{ColorSpace, DeviceClass, ProfileHeader};
use crate::primitives::XyzNumber;
use crate::profile::{IccProfile, Tag};
use crate::tag_types::TagData;
use crate::tags::KnownTag;

/// Adobe RGB (1998) Color Image Encoding §4.3.1.2: the inverse colour component transfer is the
/// pure power `X^2.19921875`, "obtained from 2 51/256" (hexadecimal `02.33`) — so `563 / 256`,
/// exact in `s15Fixed16`, and with no linear segment. Pinned against
/// `gamut_color::transfer::adobe_rgb_eotf_standard` by
/// `uncoded_space_curves_match_gamut_color_standard_curves`.
const ADOBE_RGB_GAMMA: f64 = 563.0 / 256.0;

/// A colour space [`IccProfile::builtin`] can construct without any further input.
///
/// `#[non_exhaustive]` with explicit, permanently assigned discriminants: variants are appended as
/// `gamut-color` gains the colorimetry for further spaces, and an existing discriminant never
/// changes.
#[non_exhaustive]
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BuiltinProfile {
    /// sRGB: BT.709 primaries, D65 white, the IEC 61966-2-1 transfer.
    Srgb = 0,
    /// Linear sRGB: BT.709 primaries, D65 white, the identity transfer — the scene-linear
    /// working space of sRGB.
    LinearSrgb = 1,
    /// Display P3: SMPTE EG 432-1 primaries, D65 white, the sRGB transfer.
    DisplayP3 = 2,
    /// BT.2100 PQ: BT.2020 primaries, D65 white, the SMPTE ST 2084 transfer normalized to its
    /// 10 000 cd/m² peak.
    ///
    /// **Peak-referred, by decision**: media white is the 10 000 cd/m² peak, so a diffuse-white
    /// signal (BT.2408's 203 cd/m²) is 0.0203 of media white and an SDR CMM renders it dark
    /// grey. That is the profile to embed beside HDR content for an HDR-aware pipeline; for an
    /// SDR rendering, tone-map the samples with `gamut-tonemap` and embed an SDR profile such as
    /// [`BuiltinProfile::Srgb`] instead. See the module documentation's *HDR: the PQ profile is
    /// peak-referred*.
    Bt2020Pq = 3,
    /// Adobe RGB (1998): its own primaries, D65 white, the standard `X^(563/256)` transfer.
    /// ITU-T H.273 has no code point for either axis, so the profile carries no `cicpType` tag.
    AdobeRgb = 4,
    /// ProPhoto RGB (ROMM RGB, ISO 22028-2): its own primaries, D50 white, the standard
    /// `X^1.8` transfer with its linear toe. ITU-T H.273 has no code point for either axis, so
    /// the profile carries no `cicpType` tag.
    ProPhotoRgb = 5,
}

/// What a [`BuiltinProfile`] is built from.
#[derive(Debug, Clone, Copy)]
struct Space {
    /// The colour-science gamut whose chromaticities give the colorants. Total, so every space
    /// has primaries whether or not ITU-T H.273 gives them a code point.
    gamut: RgbGamut,
    /// The two CICP axes the space is defined by, when H.273 has a code point for both; this is
    /// what the `cicpType` tag records.
    axes: Option<(ColorPrimaries, TransferCharacteristics)>,
    /// The tone curve that encodes the space's transfer.
    trc: Trc,
    /// The text of the `profileDescriptionTag`.
    description: &'static str,
}

impl BuiltinProfile {
    /// The gamut, CICP axes, tone curve and description this space is built from.
    ///
    /// The curve is named here rather than derived through [`Trc::for_code_point`] so that
    /// [`IccProfile::builtin`] is total with no unreachable fallback, and so that a space with no
    /// transfer code point still has one; that the two agree wherever there is a code point is
    /// pinned by `each_coded_space_names_the_curve_for_its_own_transfer`.
    fn parts(self) -> Space {
        let (gamut, axes, trc, description) = match self {
            BuiltinProfile::Srgb => (
                RgbGamut::Srgb,
                Some((ColorPrimaries::Bt709, TransferCharacteristics::Srgb)),
                Trc::Srgb,
                "sRGB",
            ),
            BuiltinProfile::LinearSrgb => (
                RgbGamut::Srgb,
                Some((ColorPrimaries::Bt709, TransferCharacteristics::Linear)),
                Trc::Gamma(1.0),
                "Linear sRGB",
            ),
            BuiltinProfile::DisplayP3 => (
                RgbGamut::DisplayP3,
                Some((ColorPrimaries::DisplayP3, TransferCharacteristics::Srgb)),
                Trc::Srgb,
                "Display P3",
            ),
            BuiltinProfile::Bt2020Pq => (
                RgbGamut::Bt2020,
                Some((ColorPrimaries::Bt2020, TransferCharacteristics::Pq)),
                Trc::Pq,
                "BT.2100 PQ",
            ),
            BuiltinProfile::AdobeRgb => (
                RgbGamut::AdobeRgb,
                None,
                Trc::Gamma(ADOBE_RGB_GAMMA),
                "Adobe RGB (1998) compatible",
            ),
            BuiltinProfile::ProPhotoRgb => (
                RgbGamut::ProPhotoRgb,
                None,
                Trc::Romm,
                "ProPhoto RGB (ROMM) compatible",
            ),
        };
        Space {
            gamut,
            axes,
            trc,
            description,
        }
    }

    /// Every space, for the exhaustive sweeps that must not miss one.
    const ALL: [BuiltinProfile; 6] = [
        BuiltinProfile::Srgb,
        BuiltinProfile::LinearSrgb,
        BuiltinProfile::DisplayP3,
        BuiltinProfile::Bt2020Pq,
        BuiltinProfile::AdobeRgb,
        BuiltinProfile::ProPhotoRgb,
    ];

    /// The `cicpType` value this space signals: its two axes, the identity matrix coefficients
    /// (H.273 code point 0 — the profile describes RGB, not a luma–chroma encoding), and the
    /// full-range flag.
    ///
    /// `None` for a space ITU-T H.273 has no code points for — Adobe RGB and ProPhoto RGB — whose
    /// profile accordingly carries no `cicpType` tag: §9.2.17 requires the tag to be equivalent
    /// to the encoding the profile represents, and `Unspecified` on both axes is not.
    ///
    /// # Examples
    ///
    /// ```
    /// use gamut_icc::BuiltinProfile;
    /// let cicp = BuiltinProfile::Bt2020Pq.cicp().expect("BT.2100 PQ has code points");
    /// assert_eq!((cicp.colour_primaries, cicp.transfer_characteristics), (9, 16));
    /// assert_eq!(BuiltinProfile::AdobeRgb.cicp(), None);
    /// ```
    #[must_use]
    pub fn cicp(self) -> Option<Cicp> {
        let (primaries, transfer) = self.parts().axes?;
        Some(cicp_of(primaries, transfer))
    }

    /// The space whose axes are exactly `primaries` and the H.273 transfer code point
    /// `transfer`, if there is one. Lets [`IccProfile::from_cicp`] name a profile it recognizes
    /// instead of describing it by code point.
    fn for_axes(primaries: ColorPrimaries, transfer: u16) -> Option<Self> {
        Self::ALL.into_iter().find(|space| {
            space
                .parts()
                .axes
                .is_some_and(|(p, t)| p == primaries && t.code_point() == transfer)
        })
    }

    /// The space a [`SourceProfile`] bundle with no CICP axes names, if it names one: the two
    /// uncoded bundles `gamut-color` defines map onto the two uncoded spaces here. Any other
    /// combination of an uncoded gamut or transfer is a mixture this module has no space for.
    fn for_uncoded_source(source: SourceProfile) -> Option<Self> {
        match source {
            SourceProfile::ADOBE_RGB => Some(BuiltinProfile::AdobeRgb),
            SourceProfile::PROPHOTO_RGB => Some(BuiltinProfile::ProPhotoRgb),
            _ => None,
        }
    }
}

impl IccProfile {
    /// A v4 matrix/TRC display profile for a named colour space.
    ///
    /// The media white point is the D50 the PCS mandates and the colorants are adapted to it with
    /// the Bradford matrix recorded in `chad`, so the result satisfies
    /// [`validate`](IccProfile::validate) for the Display class with no further setup.
    ///
    /// Every [`BuiltinProfile`] in this release yields `Some`, and
    /// `every_builtin_space_is_buildable` pins that. The result is still an `Option` because the
    /// colorimetry is `gamut-color`'s and its chromaticity and matrix constructors are themselves
    /// fallible: a variant added here whose primaries that crate cannot supply must decline, not
    /// fall back to a profile whose colorants are silently the PCS axes.
    ///
    /// # Examples
    ///
    /// ```
    /// use gamut_icc::{BuiltinProfile, IccProfile, KnownTag, TagData};
    ///
    /// let profile = IccProfile::builtin(BuiltinProfile::DisplayP3).expect("a modelled space");
    /// assert!(profile.validate().is_empty());
    /// assert!(matches!(profile.get(KnownTag::RedColorant), Some(TagData::Xyz(_))));
    /// ```
    #[must_use]
    pub fn builtin(space: BuiltinProfile) -> Option<Self> {
        let Space {
            gamut,
            trc,
            description,
            ..
        } = space.parts();
        rgb_matrix_trc(gamut.chromaticities(), trc, description, space.cicp())
    }

    /// A v4 monochrome display profile with a pure-gamma grey tone curve (`Y = X^gamma`).
    ///
    /// The white point is D50, so no chromatic adaptation is needed and no `chad` tag is written.
    /// Nor is a `cicpType` tag: §9.2.17 permits it only when the data colour space in the header is
    /// RGB, YCbCr or XYZ, and states that it *"shall not be present for other data colour spaces"*.
    /// A monochrome profile's space is `GRAY`, so the tag's absence here is required rather than an
    /// omission — and the CICP axes have nothing to say about a grey ramp anyway.
    ///
    /// Returns `None` for a `gamma` the `kTRC` cannot carry *as the caller wrote it*. The bound
    /// is the `s15Fixed16` encoding of the curve parameter rather than the number: `gamma` is
    /// accepted from `0.5 / 65 536 = 7.629 394 531 25e-6` up to, but not including,
    /// `(2^31 − 0.5) / 65 536 = 32 767.999 992 370 605 468 75`. Below that the parameter rounds to
    /// zero and `Y = X^0` maps every input to white — the same degenerate profile a literal `0.0`
    /// would give, which [`validate`](IccProfile::validate) cannot see, since a `kTRC` holding raw
    /// zero is a well-formed tag. At or above it the parameter saturates to a different gamma —
    /// though only just: that bound is fidelity, not degeneracy, since the first rejected gamma
    /// and the last accepted one are one `f64` ulp apart and evaluate identically.
    /// Non-finite input is declined with the rest: `NaN` would otherwise reach both the
    /// description string and the tag.
    ///
    /// That range is what the format can encode, not what is colorimetrically sensible — a gamma
    /// of `1e-5` or `30 000` is encodable and useless, and both are accepted. This is policy, not
    /// an oversight: the constructor is a faithful encoder, and ICC.1:2022 §10.18 states no range
    /// for the type 0 parameter beyond its `s15Fixed16` type, so any narrower bound would be this
    /// crate judging the caller's colorimetry with no clause behind it. Real grey profiles carry
    /// roughly `1.0 ..= 3.0`; a caller that wants to catch a units mistake (a gamma passed as
    /// `2.2e-5`, which the encoding bound accepts) checks its own input — a raw `s15Fixed16`
    /// passed as a gamma needs no such guard for any gamma of `0.5` or more, since its raw value
    /// (`2.2` → `144 179`) is at least `32 768` and already above the top bound — and a checked
    /// constructor that refuses outside a sanity range would be an additive API alongside this
    /// one, not a change to it. See <https://github.com/visualcommons/gamut/issues/589>.
    ///
    /// The `profileDescriptionTag` names the gamma the `kTRC` **holds**, not the one that was
    /// asked for: an accepted `gamma` is rounded to the tag's `s15Fixed16` parameter first and
    /// every use below reads that, so `gray_with_gamma(2.2)` is described as
    /// `Grey gamma 2.1999969482421875`. A profile whose own description contradicted its own tag
    /// would be worse than a long number — at the smallest accepted gamma the two differ by a
    /// factor of two.
    ///
    /// # Examples
    ///
    /// ```
    /// use gamut_icc::{IccProfile, KnownTag, TagData};
    ///
    /// let profile = IccProfile::gray_with_gamma(2.2).expect("2.2 is encodable");
    /// assert!(profile.validate().is_empty());
    /// assert!(matches!(profile.get(KnownTag::GrayTrc), Some(TagData::ParametricCurve(_))));
    ///
    /// assert!(IccProfile::gray_with_gamma(f64::NAN).is_none());
    /// assert!(IccProfile::gray_with_gamma(0.0).is_none());
    /// ```
    #[must_use]
    pub fn gray_with_gamma(gamma: f64) -> Option<Self> {
        // Shadowing, not a second binding: from here on the value the caller asked for is out of
        // scope, so neither the tag nor the description below can be written from it. That is the
        // structural form of "the description names what the tag holds" — there is nothing else
        // left to name.
        let gamma = encodable_gamma(gamma)?.to_f64();
        Some(IccProfile {
            header: ProfileHeader::new(DeviceClass::Display, ColorSpace::Gray),
            tags: vec![
                Tag::new(
                    KnownTag::ProfileDescription,
                    mluc(&format!("Grey gamma {gamma}")),
                ),
                Tag::new(KnownTag::Copyright, mluc(COPYRIGHT)),
                Tag::new(
                    KnownTag::MediaWhitePoint,
                    TagData::Xyz(vec![XyzNumber::D50]),
                ),
                Tag::new(KnownTag::GrayTrc, Trc::Gamma(gamma).tag()),
            ],
        })
    }
}
