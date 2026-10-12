//! Built-in profile constructors: a spec-valid [`IccProfile`] for a named colour space, for a
//! [`SourceProfile`], or for a CICP signalling triple.
//!
//! Every profile built here is a **v4 three-component matrix/TRC display profile** (ICC.1:2022
//! §8.4) — or, for [`IccProfile::gray_with_gamma`], the monochrome model of the same class.
//!
//! **Colorimetry `gamut-color` can supply is never retyped here**: the primaries and white point
//! come from [`gamut_color::cicp::ColourPrimaries::chromaticities`] for a CICP code point and from
//! [`gamut_color::Gamut::chromaticities`] for a named space, the RGB→XYZ construction and
//! the Bradford adaptation to the D50 PCS from [`gamut_color::matrix`], and the ST 2084 curve and
//! the BT.709-family α and β ([`BT709_ALPHA`] / [`BT709_BETA`], H.273 §8.2) from
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
//! | The PCS D50, via [`XyzNumber::D50`] | It is an **ICC** fact (§7.2.16), not a CIE one; see [`pcs_d50_chromaticity`] | `colorants_sum_to_the_declared_media_white_point`, and the lcms2 colorant oracle |
//!
//! Everything else this module contributes is ICC encoding, not colour science.
//!
//! # Which spaces
//!
//! A matrix/TRC profile *is* a (primaries, transfer) pair, and the two axes are resolved from
//! different places. **Primaries** come from `gamut-color`: a code point it gives no
//! chromaticities for cannot be described. Adobe RGB and ProPhoto RGB have no ITU-T H.273 code
//! point on either axis — [`gamut_color::SourceProfile`]'s `ADOBE_RGB` and `PROPHOTO_RGB` return
//! `None` from both [`colour_primaries`](gamut_color::SourceProfile::colour_primaries) and
//! [`transfer_characteristics`](gamut_color::SourceProfile::transfer_characteristics) — so they
//! are reachable only as the named [`BuiltinProfile::AdobeRgb`] and
//! [`BuiltinProfile::ProPhotoRgb`], whose primaries come from the total
//! [`Gamut::chromaticities`](gamut_color::Gamut::chromaticities), whose curves are each
//! encoding's standard one, and whose profiles carry no `cicpType` tag. ProPhoto is the one space
//! whose own white is D50, so its `chad` is not a real adaptation — only the step from the CIE
//! chromaticity `(0.3457, 0.3585)` to the PCS's rounded `XYZNumber` (see
//! [`pcs_d50_chromaticity`]), 2.5e-4 from the identity at its largest element.
//! **Transfers** of a CICP triple are keyed on the raw ITU-T H.273 code point, because the set of
//! curves an ICC tag can *encode* is not the set `gamut-color` can *evaluate*: H.273 Table 3's
//! code points 1, 6, 14 and 15 are one curve with a closed form ICC.1:2022 §10.18 defines exactly, so this
//! module encodes all four, while `gamut_color::cicp::TransferCharacteristics` models only two of
//! them (1 and 14). `gamut_color::transfer::eotf_for` maps both of those to the same Table 3
//! curve this module writes.
//!
//! ```
//! use gamut_color::cicp::TransferCharacteristics as Tc;
//! use gamut_color::transfer::eotf_for;
//! use gamut_icc::{Cicp, IccProfile};
//!
//! // Two of the four BT.709-family code points have a `TransferCharacteristics` variant …
//! assert_eq!(Tc::from_code_point(1), Some(Tc::Bt709));
//! assert_eq!(Tc::from_code_point(14), Some(Tc::Bt2020_10));
//! assert_eq!(Tc::from_code_point(6), None);
//! assert_eq!(Tc::from_code_point(15), None);
//! // … and both evaluate Table 3's curve: V = 0.5 is 0.259719 in light, the value this module
//! // writes for the same code points (pinned by `bt709_curve_inverts_the_h273_transfer`).
//! for tc in [Tc::Bt709, Tc::Bt2020_10] {
//!     let light = eotf_for(tc).expect("BT.709-family code point has an EOTF")(0.5);
//!     assert!((light - 0.259_719).abs() < 1e-6);
//! }
//!
//! // All four are encodable here regardless; that they encode one curve is pinned by
//! // `every_bt709_family_code_point_builds_the_same_curve`.
//! let axes = Cicp { colour_primaries: 1, transfer_characteristics: 1,
//!                   matrix_coefficients: 0, video_full_range_flag: 1 };
//! for code in [1, 6, 14, 15] {
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
//! * **Anything else** — any other flag byte, or narrow range under coefficients H.273 leaves
//!   unknown (2) or reserved — says nothing this module can rely on about the RGB, and is
//!   declined.
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
//! read the way `gamut-color`'s own `eotf_for` reads those code points — as the inverse OETF.
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

use gamut_color::cicp::{ColourPrimaries, TransferCharacteristics};
use gamut_color::linalg::mat_mul3;
use gamut_color::matrix::{bradford_adapt, rgb_to_xyz_matrix};
use gamut_color::transfer::{BT709_ALPHA, BT709_BETA, BT1886_GAMMA, pq_eotf};
use gamut_color::{Gamut, SourceProfile};

use crate::cicp::Cicp;
use crate::curve::{Curve, ParametricCurve};
use crate::header::{ColorSpace, DeviceClass, ProfileHeader};
use crate::mluc::{Mluc, MlucRecord};
use crate::primitives::{S15Fixed16, XyzNumber};
use crate::profile::IccProfile;
use crate::tag_types::TagData;
use crate::tags::KnownTag;

/// The `copyrightTag` text every built-in profile carries. The profiles encode published
/// colorimetry and carry no rights of their own.
const COPYRIGHT: &str = "Public domain — no rights reserved.";

/// Samples in a `curveType` TRC for a transfer with no `parametricCurveType` closed form.
///
/// 1024 points keep the linear-interpolation error of the steepest such curve (ST 2084 near its
/// peak) below one `uInt16` quantum, so the table is as exact as the encoding it is written in.
/// Measured **0.986** quanta at `V ≈ 0.9956` over a 200 001-point sweep, and 0.9854 over the
/// midpoint sweep `sampled_pq_curve_matches_gamut_color` runs — which is why that test's
/// tolerance is one quantum: a guard at twice the claimed bound is not a guard on the claim.
const SAMPLED_TRC_POINTS: usize = 1024;

/// A 3×3 matrix in row-major order — the shape `gamut_color::matrix` produces and consumes.
type Matrix3 = [[f64; 3]; 3];

/// CIE 1931 `([R, G, B], white)` chromaticities — the shape both
/// [`ColourPrimaries::chromaticities`] and [`Gamut::chromaticities`] return.
type Chromaticities = ([[f64; 2]; 3], [f64; 2]);

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
    gamut: Gamut,
    /// The two CICP axes the space is defined by, when H.273 has a code point for both; this is
    /// what the `cicpType` tag records.
    axes: Option<(ColourPrimaries, TransferCharacteristics)>,
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
                Gamut::Srgb,
                Some((ColourPrimaries::Bt709, TransferCharacteristics::Srgb)),
                Trc::Srgb,
                "sRGB",
            ),
            BuiltinProfile::LinearSrgb => (
                Gamut::Srgb,
                Some((ColourPrimaries::Bt709, TransferCharacteristics::Linear)),
                Trc::Gamma(1.0),
                "Linear sRGB",
            ),
            BuiltinProfile::DisplayP3 => (
                Gamut::DisplayP3,
                Some((ColourPrimaries::DisplayP3, TransferCharacteristics::Srgb)),
                Trc::Srgb,
                "Display P3",
            ),
            BuiltinProfile::Bt2020Pq => (
                Gamut::Bt2020,
                Some((ColourPrimaries::Bt2020, TransferCharacteristics::Pq)),
                Trc::Pq,
                "BT.2100 PQ",
            ),
            BuiltinProfile::AdobeRgb => (
                Gamut::AdobeRgb,
                None,
                Trc::Gamma(ADOBE_RGB_GAMMA),
                "Adobe RGB (1998) compatible",
            ),
            BuiltinProfile::ProPhotoRgb => (
                Gamut::ProPhotoRgb,
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
    fn for_axes(primaries: ColourPrimaries, transfer: u8) -> Option<Self> {
        Self::ALL.into_iter().find(|space| {
            space
                .parts()
                .axes
                .is_some_and(|(p, t)| p == primaries && cicp_byte(t.code_point()) == transfer)
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

/// Which of ITU-T H.273's two sanctioned readings [`IccProfile::from_cicp_with`] gives the
/// BT.709-family transfer code points 1, 6, 14 and 15.
///
/// H.273 Table 3 defines those code points by an **opto-electronic** function, and §8.2 NOTE 1
/// adds that Rec. ITU-R BT.1886 specifies "a suggested corresponding reference electro-optical
/// transfer characteristic function for flat panel displays". The two readings write different
/// tone curves — at mid-grey `V = 0.5` the PCS `Y` is 0.259719 under the first and 0.189465 under
/// the second — and the same `cicpType` tag, because the signalling is the same either way. Every
/// other transfer code point has one curve and is built identically under both readings.
///
/// `#[non_exhaustive]` with explicit, permanently assigned discriminants, so the value crosses a
/// C boundary as a plain byte and a variant is appended rather than renumbered.
///
/// # Examples
///
/// ```
/// use gamut_icc::{Cicp, CicpTransferReading, IccProfile};
///
/// let bt709 = Cicp { colour_primaries: 1, transfer_characteristics: 1,
///                    matrix_coefficients: 0, video_full_range_flag: 1 };
/// let display = IccProfile::from_cicp_with(bt709, CicpTransferReading::Bt1886Display);
/// assert!(display.is_some());
/// assert_ne!(display, IccProfile::from_cicp(bt709));
/// assert_eq!(CicpTransferReading::default(), CicpTransferReading::InverseOetf);
/// ```
#[non_exhaustive]
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum CicpTransferReading {
    /// The literal reading, and the default: the exact inverse of H.273 Table 3's
    /// opto-electronic function, `Lc = ((V + α − 1) / α)^(1/0.45)` above the knee and `V / 4.5`
    /// below it — what [`IccProfile::from_cicp`] writes, and what libjxl, libavif and Little-CMS
    /// write for the same code points.
    #[default]
    InverseOetf = 0,
    /// The display reading H.273 §8.2 NOTE 1 names: the Rec. ITU-R BT.1886 reference EOTF at
    /// reference black zero, a pure gamma of 2.4 (`gamut_color::transfer::bt1886_eotf`), written
    /// as a `parametricCurveType` function type 0.
    Bt1886Display = 1,
}

/// The one `VideoFullRangeFlag` a profile built here can describe (ITU-T H.273 signals full range
/// as `1`).
///
/// Stated once and read from both ends of the module: it is what [`rgb_conforming_cicp`] writes
/// and what [`rgb_is_full_range`] accepts, so the tag and the precondition cannot drift. Every
/// colorant, `chad` and tone curve written here is defined over full-scale RGB; see the module
/// docs for which narrow-range signalling that still admits.
const FULL_RANGE: u8 = 1;

/// ITU-T H.273 (07/2024) §8.3: the `MatrixCoefficients` code points for which
/// `VideoFullRangeFlag` scales Y, Cb and Cr (equations 30 to 32) rather than R, G and B — so a
/// narrow range under them leaves de-matrixed RGB full scale. Transcribed from the clause's own
/// list; every other code point is an RGB-range one (0, 8, 16, 17), unknown (2) or reserved.
const LUMA_CHROMA_MATRICES: [u8; 12] = [1, 4, 5, 6, 7, 9, 10, 11, 12, 13, 14, 15];

/// Whether the RGB samples a profile built from `cicp` applies to are full range — full range
/// signalled outright, or narrow range on luma–chroma signals the caller de-matrixes first.
fn rgb_is_full_range(cicp: Cicp) -> bool {
    match cicp.video_full_range_flag {
        FULL_RANGE => true,
        0 => LUMA_CHROMA_MATRICES.contains(&cicp.matrix_coefficients),
        _ => false,
    }
}

/// The `cicpType` value for a pair of CICP axes.
fn cicp_of(primaries: ColourPrimaries, transfer: TransferCharacteristics) -> Cicp {
    rgb_conforming_cicp(Cicp {
        colour_primaries: cicp_byte(primaries.code_point()),
        transfer_characteristics: cicp_byte(transfer.code_point()),
        // Both set by `rgb_conforming_cicp`, which owns the §10.3 and range rules for every
        // constructor.
        matrix_coefficients: 0,
        video_full_range_flag: FULL_RANGE,
    })
}

/// `cicp` as it describes the RGB the profile applies to: `MatrixCoefficients` replaced by the
/// zero ICC.1:2022 §10.3 **requires** of an RGB or XYZ profile, and `VideoFullRangeFlag` by
/// [`FULL_RANGE`].
///
/// The flag is written, not passed through, because once the coefficients are `0` a flag of `0`
/// would read as narrow-range *RGB* — which is not what a caller's narrow-range luma–chroma
/// signalling decodes to. Only triples [`rgb_is_full_range`] admits reach here, so the value
/// written is always the range of the samples the colorants and curves are defined over. The two
/// axes the profile is built from pass through untouched.
fn rgb_conforming_cicp(cicp: Cicp) -> Cicp {
    Cicp {
        matrix_coefficients: 0,
        video_full_range_flag: FULL_RANGE,
        ..cicp
    }
}

/// A CICP code point as the byte `cicpType` stores it (ICC.1:2022 §10.3 — four `uInt8`s).
///
/// ITU-T H.273 defines every code point in `0..=255`, so this is total for any modelled value; a
/// value that does not fit a byte cannot be signalled at all and becomes `2` (Unspecified).
fn cicp_byte(code_point: u16) -> u8 {
    u8::try_from(code_point).unwrap_or(2)
}

/// The `s15Fixed16` a `parametricCurveType` function type 0 parameter (ICC.1:2022 §10.18) carries
/// for `gamma`, or `None` when the encoding cannot hold it non-degenerately.
///
/// The guard is on the **encoding**, not on the number, because [`S15Fixed16::from_f64`] rounds
/// `gamma × 65 536` and then *clamps*: it never fails, and it degenerates at both ends. The
/// comparison is against the product *before* the clamp — afterwards a saturated parameter is
/// indistinguishable from one the caller asked for.
///
/// * Below `0.5 / 65 536 = 7.629 394 531 25e-6` the product rounds to raw `0`, and `Y = X^0` maps
///   every input — black included — to white. That is the same profile a literal `gamma` of `0.0`
///   would give, and [`IccProfile::validate`] cannot see it: a `kTRC` holding raw zero is a
///   structurally well-formed tag.
/// * From `(2^31 − 0.5) / 65 536 = 32 767.999 992 370 605 468 75` upward the product exceeds
///   `i32::MAX` and the clamp writes `32 767.999 984 741 210 937 5` instead — a gamma chosen by
///   the encoder rather than by the caller. (The clamp, not the fixed-point width, is where this
///   starts: the *largest representable* value, `32 767.999 984 741 210 937 5`, is half a quantum
///   lower still, and every gamma between the two merely rounds to it.)
///
/// Between the two the encoding is a rounding of at most half a quantum, which is what a
/// fixed-point tag is for; inside that range the clamp in [`S15Fixed16::from_f64`] is inert, so
/// the value returned is the same product this guard tested. Non-finite input satisfies neither
/// comparison and is declined with the rest.
///
/// The two ends are not the same kind of refusal. The bottom one refuses a **degenerate** profile:
/// an all-white curve nothing downstream can detect. The top one is **fidelity only** — the first
/// rejected gamma and the last accepted one are one `f64` ulp apart (`2^-38`, under four parts in
/// a trillion) and evaluate identically, so no caller is saved from a misrendering by it. It is
/// kept for the symmetry of a domain closed at both ends, and named here so the symmetry is not
/// mistaken for a second correctness claim.
fn encodable_gamma(gamma: f64) -> Option<S15Fixed16> {
    let raw = (gamma * 65_536.0).round();
    (1.0..=f64::from(i32::MAX))
        .contains(&raw)
        .then(|| S15Fixed16::from_f64(gamma))
}

/// A tone-response curve this module can encode.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Trc {
    /// `Y = X^g`, a `parametricCurveType` function type 0 (ICC.1:2022 §10.18).
    Gamma(f64),
    /// IEC 61966-2-1 (sRGB), a `parametricCurveType` function type 3 — the spec's own piecewise
    /// closed form, so the encoding is exact rather than sampled.
    Srgb,
    /// The ITU-T H.273 Table 3 curve shared by transfer code points 1 (BT.709), 6 (BT.601),
    /// 14 (BT.2020 10-bit) and 15 (BT.2020 12-bit) — one function, as Table 3's own informative
    /// remark says ("functionally the same as the values 1, 6 and 15"). Also a
    /// `parametricCurveType` function type 3.
    ///
    /// This is the *inverse of Table 3's opto-electronic function*, the literal reading
    /// ([`CicpTransferReading::InverseOetf`]); H.273 §8.2 NOTE 1 sanctions a second one for
    /// display profiles, [`CicpTransferReading::Bt1886Display`], which is `Gamma(2.4)` and which
    /// the module docs quantify against this one.
    Bt709,
    /// ROMM RGB (ISO 22028-2), the ProPhoto transfer: `Y = X^1.8` above `X = 1/32`, and the
    /// linear toe `Y = X / 16` below it. A `parametricCurveType` function type 3, exactly.
    Romm,
    /// SMPTE ST 2084 (PQ) normalized to its peak. ICC.1:2022 §10.18 defines no closed form for
    /// it, so it is a sampled `curveType` (§10.6).
    Pq,
}

impl Trc {
    /// The curve for an ITU-T H.273 `TransferCharacteristics` code point under `reading`, or
    /// `None` for a code point this module cannot encode as an ICC tone curve.
    ///
    /// Keyed on the raw code point, not on [`TransferCharacteristics`]: the encodable set and the
    /// set `gamut-color` models are different sets (see the module docs). Code points 1, 6, 14
    /// and 15 are one curve — H.273 Table 3 gives them identical segments and one pair of α/β
    /// constants — so all four map to one curve per reading: [`Trc::Bt709`] under
    /// [`CicpTransferReading::InverseOetf`], and the BT.1886 pure gamma under
    /// [`CicpTransferReading::Bt1886Display`]. `reading` changes nothing else.
    ///
    /// Code points 4 and 5 are Table 3's "assumed display gamma" 2.2 (Rec. ITU-R BT.470-6
    /// System M) and 2.8 (System B, G): pure power functions, so each is §10.18 function type 0
    /// exactly, the same shape linear (8) takes with `g = 1`. They are display gammas already, so
    /// the BT.1886 reading does not reach them.
    ///
    /// Everything else is declined, including HLG (18) and Unspecified (2), which have no
    /// `parametricCurveType` form and no `gamut-color` EOTF to sample. SMPTE ST 240 (7) is also
    /// declined: its inverse is a §10.18 type 3 closed form, but it needs its own α/β and toe
    /// constants, which nothing here or in `gamut-color` yet supplies or gates.
    fn for_code_point(transfer: u8, reading: CicpTransferReading) -> Option<Self> {
        match (transfer, reading) {
            (1 | 6 | 14 | 15, CicpTransferReading::InverseOetf) => Some(Trc::Bt709),
            (1 | 6 | 14 | 15, CicpTransferReading::Bt1886Display) => Some(Trc::Gamma(BT1886_GAMMA)),
            (4, _) => Some(Trc::Gamma(2.2)),
            (5, _) => Some(Trc::Gamma(2.8)),
            (8, _) => Some(Trc::Gamma(1.0)),
            (13, _) => Some(Trc::Srgb),
            (16, _) => Some(Trc::Pq),
            _ => None,
        }
    }

    /// The tag element encoding this curve.
    fn tag(self) -> TagData {
        match self {
            Trc::Gamma(g) => TagData::ParametricCurve(ParametricCurve {
                function_type: 0,
                params: vec![S15Fixed16::from_f64(g)],
            }),
            // IEC 61966-2-1: `Y = ((X + 0.055) / 1.055)^2.4` for `X >= 0.04045`, else `X / 12.92`
            // — ICC.1:2022 §10.18 function type 3 in its `(g, a, b, c, d)` order. Pinned against
            // `gamut_color::transfer::srgb_eotf` by `srgb_parametric_curve_matches_gamut_color`.
            Trc::Srgb => TagData::ParametricCurve(ParametricCurve {
                function_type: 3,
                params: [2.4, 1.0 / 1.055, 0.055 / 1.055, 1.0 / 12.92, 0.04045]
                    .into_iter()
                    .map(S15Fixed16::from_f64)
                    .collect(),
            }),
            // The inverse of H.273 Table 3's opto-electronic function, which is what an ICC tone
            // curve encodes: `Lc = ((V + α − 1) / α)^(1/0.45)` for `V >= 4.5 β`, else `V / 4.5`.
            // In §10.18 type 3's `(g, a, b, c, d)` order. Pinned against an independent
            // transcription of Table 3 by `bt709_curve_inverts_the_h273_transfer`.
            Trc::Bt709 => TagData::ParametricCurve(ParametricCurve {
                function_type: 3,
                params: [
                    1.0 / 0.45,
                    1.0 / BT709_ALPHA,
                    (BT709_ALPHA - 1.0) / BT709_ALPHA,
                    1.0 / 4.5,
                    4.5 * BT709_BETA,
                ]
                .into_iter()
                .map(S15Fixed16::from_f64)
                .collect(),
            }),
            // ROMM RGB: the white paper's encoding is `16 Rr` below `Et = 1/512` and
            // `Rr^(1/1.8)` above, so its inverse is `X / 16` below `16 Et = 1/32` and `X^1.8` above
            // — §10.18 type 3 with `a = 1` and `b = 0`. Every parameter but `g` is a power of two
            // and exact in `s15Fixed16`. Pinned against
            // `gamut_color::transfer::prophoto_rgb_eotf_standard` by
            // `uncoded_space_curves_match_gamut_color_standard_curves`.
            Trc::Romm => TagData::ParametricCurve(ParametricCurve {
                function_type: 3,
                params: [1.8, 1.0, 0.0, 1.0 / 16.0, 1.0 / 32.0]
                    .into_iter()
                    .map(S15Fixed16::from_f64)
                    .collect(),
            }),
            Trc::Pq => TagData::Curve(Curve::Sampled(pq_samples())),
        }
    }
}

/// [`SAMPLED_TRC_POINTS`] uniform samples of the ST 2084 EOTF normalized to its own peak.
///
/// [`pq_eotf`] returns absolute luminance in cd/m², so it is divided by `pq_eotf(1.0)` — the
/// 10 000 cd/m² peak, read back from the same function rather than restated here — to land in the
/// `[0, 1]` range a `curveType` encodes.
fn pq_samples() -> Vec<u16> {
    let peak = pq_eotf(1.0);
    let last = (SAMPLED_TRC_POINTS - 1) as f64;
    (0..SAMPLED_TRC_POINTS)
        .map(|i| {
            let signal = i as f64 / last;
            let light = (pq_eotf(signal) / peak).clamp(0.0, 1.0);
            (light * 65535.0).round() as u16
        })
        .collect()
}

/// The PCS D50 white as a CIE 1931 chromaticity, derived from the exact `XYZNumber` ICC.1:2022
/// §7.2.16 mandates for the PCS illuminant.
///
/// Deliberately *not* [`gamut_color::matrix::D50`]: that is the CIE-published chromaticity
/// `(0.3457, 0.3585)`, whose tristimulus `Z` at `Y = 1` is 0.825105, while ICC's rounded
/// `XYZNumber` encoding carries 0.824905. Adapting to the CIE one while writing the ICC one as
/// the `mediaWhitePointTag` would leave the colorants disagreeing with the white point they are
/// supposed to sum to, by 2.0e-4 in Z. The PCS illuminant is an ICC fact, so this crate owns it.
///
/// Both tristimuli and the gap between them are pinned by
/// `the_two_d50_tristimuli_the_doc_names_are_what_the_constants_hold`, so this paragraph cannot
/// drift from the two constants it quotes.
fn pcs_d50_chromaticity() -> [f64; 2] {
    let [x, y, z] = XyzNumber::D50.to_f64();
    let sum = x + y + z;
    [x / sum, y / sum]
}

/// The D50-adapted colorant columns for `primaries`, and the chromatic-adaptation matrix that took
/// them there — the `rXYZ`/`gXYZ`/`bXYZ` (ICC.1:2022 §9.2.46, §9.2.31, §9.2.4) and `chad`
/// (§9.2.15) tag contents.
///
/// Those are the numbers the standard's clause **headings** carry. ICC.1:2022 numbers two of the
/// same clauses differently where §8.4.3 cross-references them (9.2.44 for `redMatrixColumnTag`,
/// 9.2.30 for `greenMatrixColumnTag`) — an erratum in the published document, not two editions.
/// Every §9.2.x citation in this crate uses the heading numbering. No gate checks that yet;
/// <https://github.com/visualcommons/gamut/issues/606> is the one that would.
///
/// `None` when the chromaticities are degenerate enough to have no RGB→XYZ or adaptation matrix.
/// A code point that names no chromaticities at all ([`ColourPrimaries::Unspecified`], and any
/// later variant `gamut-color` adds without them) never reaches here: [`IccProfile::from_cicp`]
/// declines it on the `Option` [`ColourPrimaries::chromaticities`] returns, which is what stops a
/// profile claiming the PCS axes as its primaries.
fn colorants_d50((rgb, white): Chromaticities) -> Option<(Matrix3, Matrix3)> {
    let rgb_to_xyz = rgb_to_xyz_matrix(&rgb, white)?;
    let chad = bradford_adapt(white, pcs_d50_chromaticity())?;
    Some((mat_mul3(&chad, &rgb_to_xyz), chad))
}

/// A `multiLocalizedUnicodeType` element carrying one `en-US` record — the v4 form of the
/// description and copyright tags.
fn mluc(text: &str) -> TagData {
    TagData::MultiLocalizedUnicode(Mluc {
        records: vec![MlucRecord {
            language: *b"en",
            country: *b"US",
            text: text.to_owned(),
        }],
    })
}

/// The three-component matrix/TRC display profile for a set of chromaticities and a tone curve
/// (ICC.1:2022 §8.4).
///
/// `cicp` is the `cicpType` element to record the signalling the profile was built from, already
/// normalized for §10.3, or `None` for a space with no CICP code points, whose profile then
/// carries no `cicpType` tag. `None` when [`colorants_d50`] declines the chromaticities.
fn rgb_matrix_trc(
    chromaticities: Chromaticities,
    trc: Trc,
    description: &str,
    cicp: Option<Cicp>,
) -> Option<IccProfile> {
    let (colorants, chad) = colorants_d50(chromaticities)?;
    // A colorant tag is a *column* of the RGB→XYZ matrix: the XYZ of that primary at full scale.
    let column = |j: usize| {
        TagData::Xyz(vec![XyzNumber::from_f64([
            colorants[0][j],
            colorants[1][j],
            colorants[2][j],
        ])])
    };
    let chad_tag = TagData::S15Fixed16Array(
        chad.iter()
            .flatten()
            .map(|&v| S15Fixed16::from_f64(v))
            .collect(),
    );
    let curve = trc.tag();
    let mut tags = vec![
        (KnownTag::ProfileDescription.into(), mluc(description)),
        (KnownTag::Copyright.into(), mluc(COPYRIGHT)),
        (
            KnownTag::MediaWhitePoint.into(),
            TagData::Xyz(vec![XyzNumber::D50]),
        ),
        (KnownTag::ChromaticAdaptation.into(), chad_tag),
        (KnownTag::RedColorant.into(), column(0)),
        (KnownTag::GreenColorant.into(), column(1)),
        (KnownTag::BlueColorant.into(), column(2)),
        (KnownTag::RedTrc.into(), curve.clone()),
        (KnownTag::GreenTrc.into(), curve.clone()),
        (KnownTag::BlueTrc.into(), curve),
    ];
    tags.extend(cicp.map(|cicp| (KnownTag::Cicp.into(), TagData::Cicp(cicp))));
    Some(IccProfile {
        header: ProfileHeader::new(DeviceClass::Display, ColorSpace::Rgb),
        tags,
    })
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
    /// `2.2e-5`, or a raw `s15Fixed16`) checks its own input, and a checked constructor that
    /// refuses outside a sanity range would be an additive API alongside this one, not a change
    /// to it. See <https://github.com/visualcommons/gamut/issues/589>.
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
                (
                    KnownTag::ProfileDescription.into(),
                    mluc(&format!("Grey gamma {gamma}")),
                ),
                (KnownTag::Copyright.into(), mluc(COPYRIGHT)),
                (
                    KnownTag::MediaWhitePoint.into(),
                    TagData::Xyz(vec![XyzNumber::D50]),
                ),
                (KnownTag::GrayTrc.into(), Trc::Gamma(gamma).tag()),
            ],
        })
    }

    /// A v4 matrix/TRC display profile for a CICP signalling triple — the path from what AVIF,
    /// HEIC and JXL usually carry (a `colr`/`nclx` code-point trio) to an embeddable profile.
    ///
    /// Only the primaries and transfer code points shape the profile. The `cicpType` tag records
    /// those two verbatim and describes the RGB the profile applies to: `MatrixCoefficients` `0`
    /// and `VideoFullRangeFlag` `1`.
    ///
    /// `MatrixCoefficients` is zero because ICC.1:2022 §10.3 states that when the data colour
    /// space is RGB or XYZ it *shall* be. No information is lost: the coefficients describe a
    /// luma–chroma encoding the caller de-matrixes before this profile applies, and they stay in
    /// the container signalling (`nclx`, AV1 sequence header) a decoder actually reads them from.
    ///
    /// `VideoFullRangeFlag` is read against those coefficients, because ITU-T H.273 §8.3 applies
    /// it to Y, Cb and Cr under luma–chroma coefficients (1, 4–7, 9–15) and to R, G and B under
    /// the others. Narrow range on luma–chroma signals — `9-16-9-0`, `1-13-6-0` — de-matrixes to
    /// full-scale RGB, so it is built, and the tag says `1`. Narrow range under RGB coefficients
    /// (0, 8, 16, 17) — §10.3's own `1-1-0-0` example — is a narrow range on the **RGB samples
    /// themselves**, which no de-matrixing removes and which this profile's full-scale colorants,
    /// `chad` and tone curves cannot describe, so it is declined. Scale such samples to full range
    /// and pass `1`.
    ///
    /// Returns `None` when the profile cannot describe the signalling: a primaries code point
    /// with no chromaticities, whether unmodelled or
    /// [`Unspecified`](ColourPrimaries::Unspecified); a transfer code point with no ICC tone
    /// curve, such as HLG (18) or Unspecified (2); narrow range under coefficients that are not
    /// luma–chroma ones; or a `VideoFullRangeFlag` other than `0` or `1`.
    ///
    /// # Examples
    ///
    /// ```
    /// use gamut_icc::{BuiltinProfile, Cicp, IccProfile};
    ///
    /// // BT.709 primaries + sRGB transfer is sRGB, and is built as such.
    /// let signalled = Cicp { colour_primaries: 1, transfer_characteristics: 13,
    ///                        matrix_coefficients: 0, video_full_range_flag: 1 };
    /// assert_eq!(IccProfile::from_cicp(signalled), IccProfile::builtin(BuiltinProfile::Srgb));
    ///
    /// // The BT.601 matrix an AVIF `nclx` box may carry does not change the profile, and is not
    /// // written into it.
    /// let matrixed = Cicp { matrix_coefficients: 6, ..signalled };
    /// assert_eq!(IccProfile::from_cicp(matrixed), IccProfile::from_cicp(signalled));
    ///
    /// // Narrow range on Y/Cb/Cr de-matrixes to full-scale RGB: the same profile …
    /// let narrow_ycbcr = Cicp { video_full_range_flag: 0, ..matrixed };
    /// assert_eq!(IccProfile::from_cicp(narrow_ycbcr), IccProfile::from_cicp(signalled));
    ///
    /// // … but narrow range on the RGB samples themselves is a scaling this profile does not
    /// // perform.
    /// let narrow_rgb = Cicp { video_full_range_flag: 0, ..signalled };
    /// assert!(IccProfile::from_cicp(narrow_rgb).is_none());
    ///
    /// // "Unspecified" primaries name no chromaticities, so no profile can be built.
    /// assert!(IccProfile::from_cicp(Cicp { colour_primaries: 2, ..signalled }).is_none());
    /// ```
    ///
    /// The BT.709-family transfers 1, 6, 14 and 15 are read literally, as the inverse of H.273
    /// Table 3's opto-electronic function; [`from_cicp_with`](IccProfile::from_cicp_with) offers
    /// the BT.1886 display reading instead. This is that function with
    /// [`CicpTransferReading::InverseOetf`].
    #[must_use]
    pub fn from_cicp(cicp: Cicp) -> Option<Self> {
        Self::from_cicp_with(cicp, CicpTransferReading::InverseOetf)
    }

    /// [`from_cicp`](IccProfile::from_cicp), with the reading of the BT.709-family transfer code
    /// points 1, 6, 14 and 15 chosen by the caller.
    ///
    /// Under [`CicpTransferReading::Bt1886Display`] those four code points get the Rec. ITU-R
    /// BT.1886 reference EOTF at reference black zero — `Y = X^2.4`, a `parametricCurveType`
    /// function type 0 — which H.273 §8.2 NOTE 1 names for flat-panel displays, rather than the
    /// inverse OETF. Everything else is as `from_cicp` documents it, including every refusal:
    ///
    /// * The `cicpType` tag records the **same** code point under both readings — the signalling
    ///   is unchanged; only the tone curve derived from it is.
    /// * The `profileDescriptionTag` names the reading when it changed the curve, as
    ///   `CICP 1/1 (BT.1886 display EOTF)`, so the two profiles for one triple are told apart by
    ///   a reader that only shows the description.
    /// * Every other transfer code point has one curve and builds the same profile under either
    ///   reading.
    ///
    /// # Examples
    ///
    /// ```
    /// use gamut_icc::{Cicp, CicpTransferReading, IccProfile, KnownTag, TagData};
    ///
    /// let bt709 = Cicp { colour_primaries: 1, transfer_characteristics: 1,
    ///                    matrix_coefficients: 0, video_full_range_flag: 1 };
    /// let display = IccProfile::from_cicp_with(bt709, CicpTransferReading::Bt1886Display)
    ///     .expect("BT.709 is buildable");
    /// let Some(TagData::ParametricCurve(trc)) = display.get(KnownTag::RedTrc) else {
    ///     panic!("a parametric tone curve");
    /// };
    /// assert!((trc.eval(0.5) - 0.189_465).abs() < 1e-6);
    ///
    /// // The signalling recorded is the caller's, whichever reading built the curve.
    /// let literal = IccProfile::from_cicp(bt709).expect("BT.709 is buildable");
    /// assert_eq!(display.get(KnownTag::Cicp), literal.get(KnownTag::Cicp));
    ///
    /// // sRGB has one curve, so the reading does not reach it.
    /// let srgb = Cicp { transfer_characteristics: 13, ..bt709 };
    /// assert_eq!(
    ///     IccProfile::from_cicp_with(srgb, CicpTransferReading::Bt1886Display),
    ///     IccProfile::from_cicp(srgb),
    /// );
    /// ```
    #[must_use]
    pub fn from_cicp_with(cicp: Cicp, reading: CicpTransferReading) -> Option<Self> {
        if !rgb_is_full_range(cicp) {
            return None;
        }
        let primaries = ColourPrimaries::from_code_point(u16::from(cicp.colour_primaries))?;
        let chromaticities = primaries.chromaticities()?;
        let transfer = cicp.transfer_characteristics;
        let trc = Trc::for_code_point(transfer, reading)?;
        let named = BuiltinProfile::for_axes(primaries, transfer);
        let mut description = match named {
            Some(space) => space.parts().description.to_owned(),
            None => format!("CICP {}/{transfer}", cicp.colour_primaries),
        };
        // Compared against the literal reading's curve rather than keyed on `reading`, so the
        // qualifier is written exactly where the reading changed the profile and nowhere else.
        if Some(trc) != Trc::for_code_point(transfer, CicpTransferReading::InverseOetf) {
            description.push_str(" (BT.1886 display EOTF)");
        }
        rgb_matrix_trc(
            chromaticities,
            trc,
            &description,
            Some(rgb_conforming_cicp(cicp)),
        )
    }

    /// A v4 matrix/TRC display profile for a [`SourceProfile`] — `gamut-color`'s
    /// `(gamut, transfer)` bundle.
    ///
    /// A bundle with both CICP axes is built through [`from_cicp`](IccProfile::from_cicp). The two
    /// bundles with neither — `ADOBE_RGB` and `PROPHOTO_RGB` — are built as
    /// [`BuiltinProfile::AdobeRgb`] and [`BuiltinProfile::ProPhotoRgb`], with no `cicpType` tag.
    /// Returns `None` for any other bundle missing an axis: a hand-assembled mixture such as Adobe
    /// RGB primaries under the sRGB transfer names no space this module builds.
    ///
    /// # The profile is the standard space, not the bundle's own curve
    ///
    /// A [`SourceProfile`] is a *rendering* bundle, and for one of them the profile built here and
    /// the bundle's own [`eotf`](SourceProfile::eotf) are not the same curve.
    /// `SourceProfile::BT2020` pairs BT.2020 primaries with `SourceTransfer::Bt2020Pq`, which is
    /// the ST 2084 EOTF **followed by a Reinhard tone map to SDR**. Its CICP transfer code point
    /// is 16, and 16 is ST 2084 — so this profile encodes ST 2084, peak-referred, without the tone
    /// map. Swept over the signal domain at 100 001 points the two diverge by up to **0.735**
    /// absolute, 52× at `V = 0.1`; `SourceProfile::SRGB`, whose transfer *is* its code point,
    /// agrees with its profile to 4.2e-6 — the `s15Fixed16` rounding of the tag, and nothing else.
    ///
    /// The two uncoded bundles diverge for a smaller reason. `gamut-color` linearizes them with
    /// its *encoder-exact* curves — Adobe as a pure `x^2.2`, ProPhoto as a pure `x^1.8` — while
    /// the profile carries each encoding's **standard** curve: `X^(563/256)` for Adobe RGB, and
    /// for ProPhoto the ROMM `X^1.8` with its linear toe below `X = 1/32`. A profile is handed to
    /// a third-party CMM, which should read the space its name promises. Over the same sweep the
    /// Adobe bundle diverges from its profile by up to **1.3e-4**, and the ProPhoto bundle by up
    /// to **4.2e-4**, inside the toe.
    ///
    /// All five figures are asserted, at the precision written here, by
    /// `source_profile_divergence_is_what_the_docs_quote`.
    ///
    /// The tone map's omission is deliberate, and it is the difference between the two axes. A
    /// narrow sample range is a property of the samples, which is why
    /// [`from_cicp`](IccProfile::from_cicp) declines one it cannot describe. A tone map is not: it is `gamut-color`'s choice about how to *render* an
    /// HDR transfer into SDR, and an ICC transfer tag is the encoding the code point names.
    /// Writing the tone map into the tag would hand a CMM a curve no other reader of the same CICP
    /// triple would produce, and would contradict the `cicpType` tag written beside it (ICC.1:2022
    /// §9.2.17: the CICP tag content "shall be equivalent to the data colour space encoding
    /// represented by this ICC profile"). A caller that wants the tone-mapped rendering applies it
    /// to its samples and embeds an SDR profile.
    ///
    /// # Examples
    ///
    /// ```
    /// use gamut_color::SourceProfile;
    /// use gamut_icc::{BuiltinProfile, IccProfile};
    ///
    /// assert_eq!(
    ///     IccProfile::from_source_profile(SourceProfile::SRGB),
    ///     IccProfile::builtin(BuiltinProfile::Srgb),
    /// );
    /// assert_eq!(
    ///     IccProfile::from_source_profile(SourceProfile::ADOBE_RGB),
    ///     IccProfile::builtin(BuiltinProfile::AdobeRgb),
    /// );
    /// ```
    #[must_use]
    pub fn from_source_profile(source: SourceProfile) -> Option<Self> {
        match (source.colour_primaries(), source.transfer_characteristics()) {
            (Some(primaries), Some(transfer)) => Self::from_cicp(cicp_of(primaries, transfer)),
            _ => BuiltinProfile::for_uncoded_source(source).and_then(Self::builtin),
        }
    }
}

#[cfg(test)]
mod tests {
    use gamut_color::SourceTransfer;
    use gamut_color::transfer::{
        adobe_rgb_eotf_standard, bt1886_eotf, prophoto_rgb_eotf_standard, srgb_eotf,
    };
    use lcms2_oracle::tag;

    use super::*;

    /// Every [`BuiltinProfile`] and the grey constructor satisfy ICC.1:2022 §8's required-tag set
    /// for the Display class, and serialize. `validate` is gamut-icc's own §8 checker, so this
    /// pins the *tag set* each constructor emits, not its colorimetry.
    #[test]
    fn every_constructor_satisfies_the_section_8_display_model() {
        let profiles = BuiltinProfile::ALL
            .into_iter()
            .map(|space| (format!("{space:?}"), IccProfile::builtin(space)))
            .chain([("gray".to_owned(), IccProfile::gray_with_gamma(2.2))]);
        for (label, profile) in profiles {
            let profile = profile.unwrap_or_else(|| panic!("{label}: constructed"));
            assert_eq!(profile.validate(), vec![], "{label}: §8 conformance");
            assert!(profile.to_bytes().is_ok(), "{label}: serializes");
        }
    }

    /// No [`BuiltinProfile`] declines. `builtin` returns an `Option` only because the colorimetry
    /// it resolves is `gamut-color`'s and that crate's constructors are fallible; this is what
    /// makes a variant added without chromaticities fail here instead of shipping.
    #[test]
    fn every_builtin_space_is_buildable() {
        for space in BuiltinProfile::ALL {
            assert!(IccProfile::builtin(space).is_some(), "{space:?}");
        }
    }

    /// A code point too large for `cicpType`'s `uInt8` becomes Unspecified. Unreachable from any
    /// modelled CICP value — H.273 code points are bytes — so it is pinned at that input directly.
    #[test]
    fn an_oversized_code_point_signals_unspecified() {
        assert_eq!(cicp_byte(255), 255);
        assert_eq!(cicp_byte(256), 2);
    }

    /// The `parametricCurveType` type 3 parameters this module writes for sRGB reproduce
    /// `gamut-color`'s own `srgb_eotf`. Without this the coefficients would be a second,
    /// unguarded copy of the IEC 61966-2-1 curve that could drift from the crate that owns it.
    #[test]
    fn srgb_parametric_curve_matches_gamut_color() {
        let TagData::ParametricCurve(curve) = Trc::Srgb.tag() else {
            panic!("sRGB is encoded as a parametricCurveType");
        };
        // The `s15Fixed16` quantization of (g, a, b, c, d) is the whole of the error budget:
        // one quantum is 1/65536, and the largest derivative of the curve with respect to a
        // parameter is `g` = 2.4.
        for step in 0..=100 {
            let x = f64::from(step) / 100.0;
            let (got, want) = (curve.eval(x), srgb_eotf(x));
            assert!(
                (got - want).abs() < 1.0e-4,
                "sRGB TRC at {x}: {got} vs {want}"
            );
        }
    }

    /// ITU-T H.273 (07/2024) Table 3, transfer code point 1, transcribed **forward** exactly as
    /// the spec states it: `V = α Lc^0.45 − (α − 1)` for `1 >= Lc >= β`, and `V = 4.500 Lc` for
    /// `β > Lc >= 0`.
    ///
    /// Its α and β are written out again here on purpose. A test that reused [`BT709_ALPHA`] and
    /// [`BT709_BETA`] would share any mistyped digit with the code it is checking.
    fn h273_bt709_oetf(light: f64) -> f64 {
        let (alpha, beta) = (1.099_296_826_809_442, 0.018_053_968_510_807);
        if light >= beta {
            alpha * light.powf(0.45) - (alpha - 1.0)
        } else {
            4.500 * light
        }
    }

    /// The `parametricCurveType` written for the BT.709 family inverts H.273 Table 3's
    /// opto-electronic function, on both segments and across the β knee. An ICC tone curve
    /// encodes signal → light, so this is the law that fixes the direction as well as the
    /// constants: writing the forward function, or `α` where `α − 1` belongs, moves the curve by
    /// far more than the tolerance.
    ///
    /// The tolerance is the `s15Fixed16` quantization of `(g, a, b, c, d)` — measured at most
    /// 1.4e-6 over the whole domain — with an order of magnitude of headroom.
    #[test]
    fn bt709_curve_inverts_the_h273_transfer() {
        let TagData::ParametricCurve(curve) = Trc::Bt709.tag() else {
            panic!("the BT.709 family is encoded as a parametricCurveType");
        };
        for step in 0..=1000 {
            let light = f64::from(step) / 1000.0;
            let got = curve.eval(h273_bt709_oetf(light));
            assert!(
                (got - light).abs() < 1.0e-5,
                "BT.709 TRC round trip at Lc = {light}: {got}"
            );
        }
    }

    /// H.273 Table 3 gives code points 1, 6, 14 and 15 the same two segments and the same α/β —
    /// its own remark calls 14 "functionally the same as the values 1, 6 and 15" — so all four
    /// build one curve. Each is asserted separately: the code maps them in a single or-pattern,
    /// whose alternatives no mutation of the match itself would exercise.
    ///
    /// The same holds under each reading, so it is asserted under each: the BT.1886 reading's
    /// or-pattern is a second one, with alternatives of its own.
    #[test]
    fn every_bt709_family_code_point_builds_the_same_curve() {
        for (reading, want) in [
            (CicpTransferReading::InverseOetf, Trc::Bt709.tag()),
            (
                CicpTransferReading::Bt1886Display,
                Trc::Gamma(BT1886_GAMMA).tag(),
            ),
        ] {
            for code in [1_u8, 6, 14, 15] {
                let profile = IccProfile::from_cicp_with(
                    Cicp {
                        colour_primaries: 1,
                        transfer_characteristics: code,
                        matrix_coefficients: 0,
                        video_full_range_flag: 1,
                    },
                    reading,
                )
                .unwrap_or_else(|| panic!("H.273 transfer {code} is encodable"));
                assert_eq!(
                    profile.get(KnownTag::RedTrc),
                    Some(&want),
                    "transfer {code}, {reading:?}"
                );
            }
        }
    }

    /// The tone curve written for the BT.1886 reading reproduces `gamut-color`'s own
    /// `bt1886_eotf`, so the exponent it borrows ([`BT1886_GAMMA`]) is checked against the
    /// function that owns the curve rather than trusted. The tolerance is the `s15Fixed16`
    /// rounding of `g = 2.4` — at most `6.1e-6 × max |X^g ln X|`, under 1e-6 — with headroom.
    #[test]
    fn bt1886_curve_matches_gamut_color() {
        let TagData::ParametricCurve(curve) = Trc::Gamma(BT1886_GAMMA).tag() else {
            panic!("the BT.1886 reading is encoded as a parametricCurveType");
        };
        assert_eq!(curve.function_type, 0, "a pure gamma is §10.18 type 0");
        for step in 0..=1000 {
            let signal = f64::from(step) / 1000.0;
            let (got, want) = (curve.eval(signal), bt1886_eotf(signal));
            assert!(
                (got - want).abs() < 2.0e-6,
                "BT.1886 TRC at V = {signal}: {got} vs {want}"
            );
        }
    }

    /// The reading reaches exactly the BT.709 family. For every transfer byte, either both
    /// readings build the same profile, or the code point is one of 1, 6, 14 and 15 and the
    /// profiles differ in the tone curves and the description — and *not* in the `cicpType` tag,
    /// which records the caller's signalling under both. The family is restated here rather than
    /// read back from [`Trc::for_code_point`], so a code point wrongly brought under the reading,
    /// or wrongly left out of it, fails.
    #[test]
    fn the_reading_reaches_only_the_bt709_family() {
        const BT709_FAMILY: [u8; 4] = [1, 6, 14, 15];
        let rgb = |transfer| Cicp {
            colour_primaries: 1,
            transfer_characteristics: transfer,
            matrix_coefficients: 0,
            video_full_range_flag: 1,
        };
        for transfer in 0..=u8::MAX {
            let literal = IccProfile::from_cicp(rgb(transfer));
            let display =
                IccProfile::from_cicp_with(rgb(transfer), CicpTransferReading::Bt1886Display);
            assert_eq!(
                IccProfile::from_cicp_with(rgb(transfer), CicpTransferReading::InverseOetf),
                literal,
                "transfer {transfer}: from_cicp is the literal reading"
            );
            if !BT709_FAMILY.contains(&transfer) {
                assert_eq!(display, literal, "transfer {transfer}");
                continue;
            }
            let (literal, display) = (literal.expect("buildable"), display.expect("buildable"));
            assert_eq!(
                display.get(KnownTag::Cicp),
                Some(&TagData::Cicp(rgb(transfer))),
                "transfer {transfer}: the tag records the signalling"
            );
            assert_eq!(display.get(KnownTag::Cicp), literal.get(KnownTag::Cicp));
            assert_ne!(display.get(KnownTag::RedTrc), literal.get(KnownTag::RedTrc));
            assert_eq!(
                display.get(KnownTag::ProfileDescription),
                Some(&mluc(&format!("CICP 1/{transfer} (BT.1886 display EOTF)"))),
                "transfer {transfer}"
            );
            assert_eq!(
                literal.get(KnownTag::ProfileDescription),
                Some(&mluc(&format!("CICP 1/{transfer}"))),
                "transfer {transfer}"
            );
            assert_eq!(display.validate(), vec![], "transfer {transfer}: §8");
        }
    }

    /// The mid-grey figures the module docs' *Which reading* table quotes, measured through the
    /// `rTRC` each profile carries: 0.259721 for the literal reading of transfer 1, 0.189465 for
    /// the BT.1886 reading of it, 0.214045 for sRGB (13), and the 1.371× / 0.0703 / 0.0457 the
    /// prose derives from them. Each bound is half the last digit written.
    #[test]
    fn bt1886_reading_is_what_the_docs_quote() {
        let mid_grey = |transfer, reading| {
            let profile = IccProfile::from_cicp_with(
                Cicp {
                    colour_primaries: 1,
                    transfer_characteristics: transfer,
                    matrix_coefficients: 0,
                    video_full_range_flag: 1,
                },
                reading,
            )
            .expect("buildable");
            red_trc(&profile)(0.5)
        };
        let literal = mid_grey(1, CicpTransferReading::InverseOetf);
        let display = mid_grey(1, CicpTransferReading::Bt1886Display);
        let srgb = mid_grey(13, CicpTransferReading::InverseOetf);
        assert!((literal - 0.259_721).abs() < 5.0e-7, "literal: {literal}");
        assert!((display - 0.189_465).abs() < 5.0e-7, "BT.1886: {display}");
        assert!((srgb - 0.214_045).abs() < 5.0e-7, "sRGB: {srgb}");
        assert!((literal / display - 1.371).abs() < 5.0e-4);
        assert!((literal - display - 0.0703).abs() < 5.0e-5);
        assert!((literal - srgb - 0.0457).abs() < 5.0e-5);
    }

    /// The sampled PQ curve reproduces `gamut-color`'s `pq_eotf` normalized to its peak, to within
    /// the `uInt16` quantum the table is written in. This is what justifies
    /// [`SAMPLED_TRC_POINTS`]: it pins that the sampling density is fine enough to make the
    /// encoding, not the table, the limit on accuracy.
    ///
    /// The sweep visits every one of the table's intervals at both ends *and at its midpoint*,
    /// which is where linear interpolation is furthest from the curve it interpolates. A uniform
    /// 101-point sweep misses the peak: it measures 0.962 quanta where the true worst case is
    /// 0.986, at `V ≈ 0.9956`. The tolerance is therefore **one** quantum — the bound
    /// [`SAMPLED_TRC_POINTS`] claims — not two; the arithmetic is deterministic, so a guard that
    /// tight cannot flake.
    #[test]
    fn sampled_pq_curve_matches_gamut_color() {
        let TagData::Curve(curve) = Trc::Pq.tag() else {
            panic!("PQ is encoded as a sampled curveType");
        };
        let peak = pq_eotf(1.0);
        // Twice the interval count: an even `step` lands on a sample, an odd one on a midpoint.
        let steps = 2 * (SAMPLED_TRC_POINTS - 1);
        for step in 0..=steps {
            let x = step as f64 / steps as f64;
            let (got, want) = (curve.eval(x), pq_eotf(x) / peak);
            assert!(
                (got - want).abs() < 1.0 / 65535.0,
                "PQ TRC at {x}: {got} vs {want}"
            );
        }
    }

    /// The four coded built-in spaces have four distinct sets of axes, and each round-trips
    /// through its `cicpType` value back to itself. A space whose `parts` named the wrong code
    /// point would collide with another or fail to round-trip; one whose `gamut` disagreed with
    /// its primaries code point would build a different profile from each path.
    #[test]
    fn each_builtin_space_round_trips_through_its_cicp_value() {
        for space in BuiltinProfile::ALL {
            let Some((primaries, transfer)) = space.parts().axes else {
                continue;
            };
            assert_eq!(
                BuiltinProfile::for_axes(primaries, cicp_byte(transfer.code_point())),
                Some(space),
                "{space:?} is the space for its own axes"
            );
            let built = IccProfile::builtin(space);
            assert!(built.is_some(), "{space:?} builds");
            assert_eq!(
                IccProfile::from_cicp(space.cicp().expect("a coded space")),
                built,
                "{space:?} builds the same profile from its own CICP value"
            );
        }
    }

    /// The curve each space names is the one its own transfer code point maps to. `parts` states
    /// the curve directly so that `builtin` needs no fallback, and this is what stops the two
    /// halves of that statement drifting apart. No built-in space's transfer is in the BT.709
    /// family, so the curve is the same under either reading.
    #[test]
    fn each_coded_space_names_the_curve_for_its_own_transfer() {
        for space in BuiltinProfile::ALL {
            let Space { axes, trc, .. } = space.parts();
            if let Some((_, transfer)) = axes {
                let code = cicp_byte(transfer.code_point());
                for reading in READINGS {
                    assert_eq!(
                        Trc::for_code_point(code, reading),
                        Some(trc),
                        "{space:?}, {reading:?}"
                    );
                }
            }
        }
    }

    /// Every [`CicpTransferReading`], for the sweeps that must hold under each.
    const READINGS: [CicpTransferReading; 2] = [
        CicpTransferReading::InverseOetf,
        CicpTransferReading::Bt1886Display,
    ];

    /// Exactly Adobe RGB and ProPhoto RGB have no CICP axes, and their profiles accordingly carry
    /// no `cicpType` tag — §9.2.17 requires the tag to be equivalent to the encoding, which
    /// `Unspecified` is not. Every coded space writes one. Asserted per space, so a coded space
    /// that lost its axes fails here rather than slipping into the uncoded path.
    #[test]
    fn only_the_uncoded_spaces_omit_the_cicp_tag() {
        for space in BuiltinProfile::ALL {
            let uncoded = matches!(
                space,
                BuiltinProfile::AdobeRgb | BuiltinProfile::ProPhotoRgb
            );
            assert_eq!(space.cicp().is_none(), uncoded, "{space:?} axes");
            let profile = IccProfile::builtin(space).expect("buildable");
            assert_eq!(
                profile.get(KnownTag::Cicp).is_none(),
                uncoded,
                "{space:?} cicp tag"
            );
        }
    }

    /// A built-in space and the `gamut-color` function its tone curve must reproduce.
    type StandardCurve = (BuiltinProfile, fn(f64) -> f64);

    /// The two spaces with no transfer code point, each against `gamut-color`'s **standard**
    /// curve for its encoding — the curve they are built to carry, not the encoder-exact one.
    const UNCODED_STANDARD_CURVES: [StandardCurve; 2] = [
        (BuiltinProfile::AdobeRgb, adobe_rgb_eotf_standard),
        (BuiltinProfile::ProPhotoRgb, prophoto_rgb_eotf_standard),
    ];

    /// The two uncoded spaces carry their encodings' **standard** curves, which reproduce
    /// `gamut-color`'s own `adobe_rgb_eotf_standard` and `prophoto_rgb_eotf_standard` — not the
    /// encoder-exact `x^2.2` and `x^1.8` the matching `SourceProfile` bundles linearize with.
    /// Sampled densely below `1/32` as well, so the ROMM toe and its knee are both reached.
    ///
    /// The tolerance is the `s15Fixed16` rounding of ROMM's `g = 1.8` — the only inexact
    /// parameter of either tag; Adobe's `563 / 256` is exact.
    #[test]
    fn uncoded_space_curves_match_gamut_color_standard_curves() {
        for (space, standard) in UNCODED_STANDARD_CURVES {
            let TagData::ParametricCurve(curve) = space.parts().trc.tag() else {
                panic!("{space:?} is encoded as a parametricCurveType");
            };
            let toe = (0..64).map(|step| f64::from(step) / 2048.0);
            let body = (0..=100).map(|step| f64::from(step) / 100.0);
            for x in toe.chain(body) {
                let (got, want) = (curve.eval(x), standard(x));
                assert!(
                    (got - want).abs() < 1.0e-6,
                    "{space:?} TRC at {x}: {got} vs {want}"
                );
            }
        }
    }

    /// ProPhoto RGB is the one built-in space whose own white is D50, so its `chad` is not a real
    /// chromatic adaptation: only the step from the CIE D50 chromaticity to the PCS's rounded
    /// `XYZNumber`, 2.5e-4 from the identity at its largest element as the module docs and
    /// `STATUS.md` quote — gated at half that figure's last digit. Adobe RGB — D65, like every
    /// other built-in space — is the control: a real Bradford adaptation, more than 1e-2 from the
    /// identity on some element.
    #[test]
    fn prophoto_chad_is_near_the_identity() {
        let off_identity = |space: BuiltinProfile| {
            let (_, chad) = colorants_d50(space.parts().gamut.chromaticities()).expect("modelled");
            let mut worst = 0.0_f64;
            for (i, row) in chad.iter().enumerate() {
                for (j, &v) in row.iter().enumerate() {
                    worst = worst.max((v - f64::from(u8::from(i == j))).abs());
                }
            }
            worst
        };
        let prophoto = off_identity(BuiltinProfile::ProPhotoRgb);
        assert!(
            (prophoto - 2.5e-4).abs() < 5.0e-6,
            "ProPhoto chad off identity by {prophoto}"
        );
        let adobe = off_identity(BuiltinProfile::AdobeRgb);
        assert!(adobe > 1.0e-2, "Adobe chad off identity by {adobe}");
    }

    /// No transfer code point outside the encodable set builds a tone curve.
    ///
    /// The set is restated here rather than read back from [`Trc::for_code_point`], which is what
    /// makes this the only test that fails when a code point is wrongly *admitted*. Every other
    /// test of the mapping starts from a code point it already believes in, so it can see a
    /// member sent to the wrong curve but not a non-member let in — and no mutation of a match
    /// expression produces an extra arm, so the mutation gate cannot see it either.
    #[test]
    fn no_unlisted_transfer_code_point_is_encodable() {
        // ICC.1:2022 §10.18 gives a closed form, or gamut-color an EOTF to sample, for exactly
        // these ITU-T H.273 Table 3 code points: the BT.709 family (1, 6, 14, 15), the BT.470-6
        // display gammas 2.2 (4) and 2.8 (5), linear (8), sRGB (13) and PQ (16). Every other
        // byte — reserved, unspecified, the two logarithmic curves, ST 240, HLG, and everything
        // H.273 has not assigned — has no ICC tone curve here.
        const ENCODABLE: [u8; 9] = [1, 4, 5, 6, 8, 13, 14, 15, 16];

        for code in 0..=u8::MAX {
            if ENCODABLE.contains(&code) {
                continue;
            }
            for reading in READINGS {
                assert_eq!(
                    Trc::for_code_point(code, reading),
                    None,
                    "transfer code point {code}, {reading:?}"
                );
            }
        }
    }

    /// A transfer code point this module has no ICC tone curve for cannot be built — whether or
    /// not `gamut-color` evaluates it, since transfers are keyed on the raw code point — and
    /// neither can primaries with no chromaticities, unmodelled or unspecified. Each rejection is asserted at an input that isolates
    /// it: the primaries are valid when the transfer is the reason, and vice versa.
    #[test]
    fn unrepresentable_signalling_is_rejected() {
        let srgb = Cicp {
            colour_primaries: 1,
            transfer_characteristics: 13,
            matrix_coefficients: 0,
            video_full_range_flag: 1,
        };
        assert!(IccProfile::from_cicp(srgb).is_some(), "the control builds");
        for (label, cicp) in [
            (
                "unmodelled primaries",
                Cicp {
                    colour_primaries: 11,
                    ..srgb
                },
            ),
            (
                "unspecified primaries",
                Cicp {
                    colour_primaries: 2,
                    ..srgb
                },
            ),
            (
                "unmodelled transfer",
                Cicp {
                    transfer_characteristics: 17,
                    ..srgb
                },
            ),
            (
                "unspecified transfer",
                Cicp {
                    transfer_characteristics: 2,
                    ..srgb
                },
            ),
            (
                "HLG transfer, no ICC tone curve",
                Cicp {
                    transfer_characteristics: 18,
                    ..srgb
                },
            ),
        ] {
            assert_eq!(IccProfile::from_cicp(cicp), None, "{label}");
        }
    }

    /// The one field `from_cicp` rewrites does not reach the `cicpType` tag, so two triples
    /// differing only in it are one profile. The coefficients an AVIF or HEIC `nclx` box carries
    /// (1, 5, 6, 9 …) are dropped because ICC.1:2022 §10.3 says they *shall* be zero in an RGB
    /// profile, and what a caller can observe is exactly that: the value it passed is not carried.
    #[test]
    fn from_cicp_zeroes_the_matrix_coefficients() {
        let conforming = Cicp {
            colour_primaries: 9,
            transfer_characteristics: 16,
            matrix_coefficients: 0,
            video_full_range_flag: 1,
        };
        let reference = IccProfile::from_cicp(conforming).expect("the control builds");
        for matrix in [1_u8, 5, 6, 9] {
            let signalled = Cicp {
                matrix_coefficients: matrix,
                ..conforming
            };
            let profile =
                IccProfile::from_cicp(signalled).expect("signalling with a matrix builds");
            assert_eq!(profile, reference, "matrix {matrix}");
            assert_eq!(
                profile.get(KnownTag::Cicp),
                Some(&TagData::Cicp(conforming)),
                "matrix {matrix}: the coefficients are not taken from the caller"
            );
        }
    }

    /// Under RGB coefficients, full range is a precondition of `from_cicp`, not a field it
    /// normalises: every other `VideoFullRangeFlag` byte is declined. Under luma–chroma
    /// coefficients every byte but `0` and `1` is declined too.
    ///
    /// The complement is swept rather than sampled at `0`, so a guard that admitted any byte
    /// beyond the two H.273 defines is seen. The control at `1` builds from the same axes, so the
    /// axes cannot be what any rejection is about.
    #[test]
    fn a_cicp_triple_that_is_not_full_range_is_declined() {
        let full = Cicp {
            colour_primaries: 1,
            transfer_characteristics: 13,
            matrix_coefficients: 0,
            video_full_range_flag: FULL_RANGE,
        };
        assert!(
            IccProfile::from_cicp(full).is_some(),
            "the full-range control builds"
        );
        for flag in 0..=u8::MAX {
            if flag == FULL_RANGE {
                continue;
            }
            assert_eq!(
                IccProfile::from_cicp(Cicp {
                    video_full_range_flag: flag,
                    ..full
                }),
                None,
                "RGB coefficients, video_full_range_flag {flag}"
            );
            if flag != 0 {
                assert_eq!(
                    IccProfile::from_cicp(Cicp {
                        matrix_coefficients: 9,
                        video_full_range_flag: flag,
                        ..full
                    }),
                    None,
                    "luma–chroma coefficients, video_full_range_flag {flag}"
                );
            }
        }
    }

    /// Narrow range is built exactly when H.273 §8.3 puts it on Y, Cb and Cr — equations 30 to
    /// 32's list, 1, 4–7 and 9–15 — and then yields the full-range profile, whose `cicpType`
    /// says `1`: de-matrixing a narrow-range luma–chroma signal gives full-scale RGB, and that is
    /// what the profile describes. Every other coefficient is declined: 0, 8, 16 and 17 put the
    /// narrow range on the RGB samples, 2 leaves it unknown, and the rest are reserved.
    ///
    /// The list is restated here rather than read back from [`LUMA_CHROMA_MATRICES`], and the
    /// whole byte range is swept, so a coefficient wrongly admitted or wrongly left out fails
    /// here — the or-pattern-like membership test has no mutant for either.
    #[test]
    fn narrow_range_builds_only_under_luma_chroma_coefficients() {
        const EQUATIONS_30_TO_32: [u8; 12] = [1, 4, 5, 6, 7, 9, 10, 11, 12, 13, 14, 15];
        let full = Cicp {
            colour_primaries: 9,
            transfer_characteristics: 16,
            matrix_coefficients: 0,
            video_full_range_flag: FULL_RANGE,
        };
        let want = IccProfile::from_cicp(full).expect("the full-range control builds");
        for matrix in 0..=u8::MAX {
            let got = IccProfile::from_cicp(Cicp {
                matrix_coefficients: matrix,
                video_full_range_flag: 0,
                ..full
            });
            if EQUATIONS_30_TO_32.contains(&matrix) {
                assert_eq!(got.as_ref(), Some(&want), "matrix {matrix}, narrow range");
                assert_eq!(
                    got.as_ref().and_then(|p| p.get(KnownTag::Cicp)),
                    Some(&TagData::Cicp(full)),
                    "matrix {matrix}: the tag describes the full-range RGB"
                );
            } else {
                assert_eq!(got, None, "matrix {matrix}, narrow range");
            }
        }
    }

    /// A grey gamma is open `f64` input, and the constructor accepts exactly those the `kTRC`
    /// parameter carries back. Each accepted gamma is read out of the tag it was written into, so
    /// the test fails if the value survives the guard but not the encoding; each rejected one is
    /// the neighbour of an accepted one, so both bounds are pinned from both sides.
    ///
    /// The `s15Fixed16` conversion degenerates at both ends and never fails, which is why the
    /// bound is on the encoding: above the top it clamps to `i32::MAX`, and below the bottom it
    /// rounds to raw zero — a `Y = X^0` curve mapping every input to white, which serializes and
    /// which `validate` reports clean.
    ///
    /// The two bounds are restated as literal decimals instead of being reused from
    /// [`encodable_gamma`], so a wrong scale factor there cannot agree with the test.
    #[test]
    fn a_grey_gamma_the_ktrc_cannot_carry_is_declined() {
        /// `0.5 / 65 536` — one half of a `s15Fixed16` quantum, which is both the smallest gamma
        /// whose parameter rounds to raw 1 rather than raw 0 and the largest rounding error the
        /// encoding may introduce.
        const HALF_QUANTUM: f64 = 7.629_394_531_25e-6;
        /// `(2 ^ 31 − 0.5) / 65 536` — the smallest gamma whose parameter clamps to `i32::MAX`.
        /// Exactly 32 767.999 992 370 605 468 75, written at the shortest decimal that names
        /// that `f64` and no other.
        const SATURATING: f64 = 32_767.999_992_370_605;

        for gamma in [HALF_QUANTUM, 1.0, 2.2, SATURATING.next_down()] {
            let profile = IccProfile::gray_with_gamma(gamma)
                .unwrap_or_else(|| panic!("gamma {gamma} is encodable"));
            let Some(TagData::ParametricCurve(curve)) = profile.get(KnownTag::GrayTrc) else {
                panic!("gamma {gamma}: the grey TRC is a parametricCurveType");
            };
            let written = curve.params[0].to_f64();
            assert!(
                written > 0.0 && (written - gamma).abs() <= HALF_QUANTUM,
                "gamma {gamma} reached the kTRC as {written}"
            );
        }

        for gamma in [
            0.0,
            -1.0,
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            HALF_QUANTUM.next_down(),
            1.0e-6,
            SATURATING,
            40_000.0,
        ] {
            assert_eq!(IccProfile::gray_with_gamma(gamma), None, "gamma {gamma}");
        }
    }

    /// `SourceProfile`'s bundles map onto the built-in spaces — the two with no CICP axes onto
    /// the two uncoded spaces — and a mixture with an uncoded axis is declined rather than
    /// approximated.
    ///
    /// `BT2020` maps onto [`BuiltinProfile::Bt2020Pq`] even though the bundle's own `eotf` is a
    /// tone map and the profile's `rTRC` is not: the mapping is on the **code points** the bundle
    /// projects onto, which is what `from_source_profile` is defined over. The divergence that
    /// follows is documented on the constructor (up to 0.735 absolute), and that figure is gated
    /// by `source_profile_divergence_is_what_the_docs_quote`.
    #[test]
    fn source_profiles_map_onto_the_builtin_spaces() {
        let cases = [
            (SourceProfile::SRGB, Some(BuiltinProfile::Srgb)),
            (SourceProfile::LINEAR_SRGB, Some(BuiltinProfile::LinearSrgb)),
            (SourceProfile::DISPLAY_P3, Some(BuiltinProfile::DisplayP3)),
            (SourceProfile::BT2020, Some(BuiltinProfile::Bt2020Pq)),
            (SourceProfile::ADOBE_RGB, Some(BuiltinProfile::AdobeRgb)),
            (
                SourceProfile::PROPHOTO_RGB,
                Some(BuiltinProfile::ProPhotoRgb),
            ),
            // A hand-assembled mixture with an uncoded axis names no space: declined.
            (
                SourceProfile {
                    transfer: SourceTransfer::Srgb,
                    ..SourceProfile::ADOBE_RGB
                },
                None,
            ),
        ];
        for (source, expected) in cases {
            let want = expected.and_then(IccProfile::builtin);
            assert_eq!(
                IccProfile::from_source_profile(source),
                want,
                "{source:?} → {expected:?}"
            );
        }
    }

    /// The `rTRC` of `profile` evaluated by this crate's own curve evaluator — the tag as written,
    /// `s15Fixed16` rounding and sampling included.
    fn red_trc(profile: &IccProfile) -> impl Fn(f64) -> f64 + '_ {
        move |x| match profile.get(KnownTag::RedTrc) {
            Some(TagData::ParametricCurve(curve)) => curve.eval(x),
            Some(TagData::Curve(curve)) => curve.eval(x),
            other => panic!("an rTRC tone curve, found {other:?}"),
        }
    }

    /// The figures `from_source_profile`'s doc and `STATUS.md` quote for how far a
    /// [`SourceProfile`] bundle's own `eotf` sits from the profile built for it: up to **0.735**
    /// absolute and **52×** at `V = 0.1` for `BT2020`, against **4.2e-6** for the `SRGB` control,
    /// and **1.3e-4** / **4.2e-4** for the encoder-exact `ADOBE_RGB` / `PROPHOTO_RGB` bundles
    /// against the standard curves their profiles carry.
    /// Both sides are measured as they ship — the bundle through `gamut-color`'s `eotf`, the
    /// profile through the `rTRC` tag this module writes — over the 100 001-point sweep the prose
    /// names. Each bound is half the last digit the prose states, so the gate is exactly as tight
    /// as the claim; the figures otherwise restate another crate's curve with nothing checking it.
    #[test]
    fn source_profile_divergence_is_what_the_docs_quote() {
        let largest_divergence = |source: SourceProfile| {
            let profile = IccProfile::from_source_profile(source).expect("a buildable bundle");
            let trc = red_trc(&profile);
            (0..=100_000)
                .map(|step| {
                    let x = f64::from(step) / 100_000.0;
                    (source.eotf(x) - trc(x)).abs()
                })
                .fold(0.0, f64::max)
        };

        let bt2020 = largest_divergence(SourceProfile::BT2020);
        assert!((bt2020 - 0.735).abs() < 5.0e-4, "BT2020: {bt2020}");
        let srgb = largest_divergence(SourceProfile::SRGB);
        assert!((srgb - 4.2e-6).abs() < 5.0e-8, "SRGB: {srgb}");
        let adobe = largest_divergence(SourceProfile::ADOBE_RGB);
        assert!((adobe - 1.3e-4).abs() < 5.0e-6, "ADOBE_RGB: {adobe}");
        let prophoto = largest_divergence(SourceProfile::PROPHOTO_RGB);
        assert!(
            (prophoto - 4.2e-4).abs() < 5.0e-6,
            "PROPHOTO_RGB: {prophoto}"
        );

        let profile = IccProfile::from_source_profile(SourceProfile::BT2020).expect("buildable");
        let ratio = SourceProfile::BT2020.eotf(0.1) / red_trc(&profile)(0.1);
        assert!((ratio - 52.0).abs() < 0.5, "BT2020 at V = 0.1: {ratio}×");
    }

    /// Each space's colorants sum to the media white point the same profile declares — the law a
    /// matrix/TRC profile has to satisfy for full-scale RGB to land on the PCS white. It is what
    /// forces the Bradford adaptation to target [`pcs_d50_chromaticity`] rather than the CIE D50.
    #[test]
    fn colorants_sum_to_the_declared_media_white_point() {
        let want = XyzNumber::D50.to_f64();
        for space in BuiltinProfile::ALL {
            let Space {
                gamut, description, ..
            } = space.parts();
            let label = description;
            let (colorants, _) = colorants_d50(gamut.chromaticities()).expect("a modelled gamut");
            for axis in 0..3 {
                let sum: f64 = colorants[axis].iter().sum();
                assert!(
                    (sum - want[axis]).abs() < 1.0e-6,
                    "{label} white [{axis}]: {sum} vs {}",
                    want[axis]
                );
            }
        }
    }

    /// The two D50s [`pcs_d50_chromaticity`] distinguishes hold the tristimuli its doc quotes:
    /// `gamut_color::matrix::D50` gives `Z = 0.825105` at `Y = 1`, ICC's encoded
    /// [`XyzNumber::D50`] carries `0.824905`, and the gap is the documented 2.0e-4. A drift guard
    /// on two constants this crate does not own — the doc sentence naming them is otherwise
    /// unchecked, and its arithmetic is what decides the adaptation target.
    #[test]
    fn the_two_d50_tristimuli_the_doc_names_are_what_the_constants_hold() {
        let [x, y] = gamut_color::matrix::D50;
        let cie_z = (1.0 - x - y) / y;
        let icc_z = XyzNumber::D50.to_f64()[2];
        assert!((cie_z - 0.825_105).abs() < 5.0e-7, "CIE D50 Z: {cie_z}");
        assert!((icc_z - 0.824_905).abs() < 5.0e-7, "ICC D50 Z: {icc_z}");
        // Each bound is half the last digit the doc states: six decimals on the two tristimuli,
        // two significant figures on the gap they differ by.
        assert!(
            (cie_z - icc_z - 2.0e-4).abs() < 5.0e-6,
            "gap: {}",
            cie_z - icc_z
        );
    }

    /// A constructor is a pure function of its arguments: the same call serializes to the same
    /// bytes. Guards the "no timestamp, no ID, no entropy" property the module doc promises, which
    /// a later `DateTime::now()` would silently break.
    #[test]
    fn constructors_are_byte_deterministic() {
        let serialize = || {
            IccProfile::builtin(BuiltinProfile::Srgb)
                .expect("sRGB is buildable")
                .to_bytes()
                .expect("sRGB serializes")
        };
        assert_eq!(serialize(), serialize());
    }

    // --- differential: Little-CMS re-opens what we built ------------------------------------

    /// A transform from our sRGB profile to lcms2's own sRGB profile is the identity, to within a
    /// code. This is the end-to-end acceptance: colorants, white point, adaptation and TRC all
    /// have to be right together for a full round trip through the PCS to come back unchanged.
    #[test]
    fn oracle_transform_through_our_srgb_is_the_identity() {
        let ours = lcms2_oracle::Profile::from_bytes(
            &IccProfile::builtin(BuiltinProfile::Srgb)
                .expect("sRGB is buildable")
                .to_bytes()
                .expect("serializes"),
        )
        .expect("lcms2 re-opens the profile");
        let reference = lcms2_oracle::srgb();

        let pixels: Vec<u8> = (0..=255u8)
            .flat_map(|v| [v, 255 - v, v.wrapping_mul(3)])
            .collect();
        // Intent 1 is media-relative colorimetric: the intent under which two profiles for the
        // same space must agree exactly.
        let out = lcms2_oracle::transform_rgb8(&ours, &reference, 1, &pixels);
        assert_eq!(out.len(), pixels.len());
        for (i, (&got, &want)) in out.iter().zip(pixels.iter()).enumerate() {
            assert!(
                got.abs_diff(want) <= 1,
                "channel {i}: {got} vs {want} through our sRGB → lcms2 sRGB"
            );
        }
    }

    /// The serialized bytes of `profile`, re-opened by lcms2.
    fn opened_by_lcms(profile: &IccProfile) -> lcms2_oracle::Profile {
        lcms2_oracle::Profile::from_bytes(&profile.to_bytes().expect("serializes"))
            .expect("lcms2 re-opens the profile")
    }

    /// The profile `from_cicp` builds for full-range RGB signalling of `primaries` and `transfer`.
    fn rgb_cicp(primaries: u8, transfer: u8) -> Option<IccProfile> {
        IccProfile::from_cicp(Cicp {
            colour_primaries: primaries,
            transfer_characteristics: transfer,
            matrix_coefficients: 0,
            video_full_range_flag: 1,
        })
    }

    /// Every primaries code point `from_cicp` builds, found by asking it rather than restated.
    fn buildable_primaries() -> Vec<u8> {
        (0..=u8::MAX)
            .filter(|&code| rgb_cicp(code, 13).is_some())
            .collect()
    }

    /// lcms2 re-opens a profile for **every** primaries code point `from_cicp` builds — not only
    /// the three the built-in spaces use — and reports the colorants it derives from the same
    /// chromaticities itself. This is the colorimetric acceptance gate: the D50 adaptation, the
    /// column-vs-row orientation of the colorant tags and the `s15Fixed16` encoding are all only
    /// checked against an independent implementation. The code points BT.470 BG (5) and SMPTE
    /// 170M (6) are reachable only through `from_cicp`; the built-in spaces' three are reached
    /// here through the same path, which `each_builtin_space_round_trips_through_its_cicp_value`
    /// pins to be the profile `builtin` returns.
    ///
    /// The tolerance is four `s15Fixed16` quanta, so it is the tag encoding — not the derivation —
    /// that sets it: the largest observed disagreement is 1.2e-5 (BT.470 BG), under one quantum;
    /// over the three built-in primaries alone it is 9.1e-6 (BT.2020). A transposed
    /// matrix, a missing Bradford adaptation or a wrong white point all move a colorant by more
    /// than 1e-2. lcms2 builds its reference from the same chromaticities; its gamma is
    /// irrelevant to the colorants.
    #[test]
    fn oracle_colorants_match_lcms_for_every_buildable_primaries() {
        let codes = buildable_primaries();
        for code in [1, 5, 6, 9, 12] {
            assert!(codes.contains(&code), "primaries {code} build");
        }
        for code in codes {
            let primaries =
                ColourPrimaries::from_code_point(u16::from(code)).expect("a modelled code point");
            let chromaticities = primaries.chromaticities().expect("a modelled code point");
            let profile = rgb_cicp(code, 13).expect("buildable");
            assert_colorants_match_lcms(&format!("primaries {code}"), &profile, chromaticities);
        }
    }

    /// The colorants lcms2 reads back from `profile` against the ones it derives itself from
    /// `chromaticities`, to four `s15Fixed16` quanta — the gate both colorant oracles share.
    fn assert_colorants_match_lcms(
        label: &str,
        profile: &IccProfile,
        (rgb, white): Chromaticities,
    ) {
        let reference = lcms2_oracle::rgb_matrix_shaper(white, rgb, [2.2, 2.2, 2.2]);
        let ours = opened_by_lcms(profile);
        for (name, sig) in [
            ("red", tag::RED_COLORANT),
            ("green", tag::GREEN_COLORANT),
            ("blue", tag::BLUE_COLORANT),
        ] {
            let got = ours.read_xyz(sig).expect("colorant present");
            let want = reference.read_xyz(sig).expect("colorant present");
            for axis in 0..3 {
                assert!(
                    (got[axis] - want[axis]).abs() < 4.0 / 65536.0,
                    "{label} {name} colorant [{axis}]: {got:?} vs {want:?}"
                );
            }
        }
    }

    /// lcms2 re-opens **every** built-in space and reports the colorants it derives from the same
    /// chromaticities itself — the acceptance gate for the two spaces no CICP code point reaches,
    /// which `oracle_colorants_match_lcms_for_every_buildable_primaries` therefore cannot see:
    /// Adobe RGB, and ProPhoto RGB, whose D50 white makes it the one space where the adaptation
    /// lcms2 performs from the same white is (nearly) the identity. Same tolerance as that test.
    #[test]
    fn oracle_colorants_match_lcms_for_every_builtin_space() {
        for space in BuiltinProfile::ALL {
            let profile = IccProfile::builtin(space).expect("buildable");
            let chromaticities = space.parts().gamut.chromaticities();
            assert_colorants_match_lcms(&format!("{space:?}"), &profile, chromaticities);
        }
    }

    /// lcms2 evaluates the three TRCs of the two uncoded spaces to `gamut-color`'s standard
    /// curves — `adobe_rgb_eotf_standard` and `prophoto_rgb_eotf_standard`, the latter sampled
    /// through its toe as well. `oracle_every_encodable_transfer_matches_its_curve_through_lcms`
    /// is keyed on transfer code points, which neither curve has. Same two-quanta tolerance.
    #[test]
    fn oracle_uncoded_space_curves_match_gamut_color_through_lcms() {
        for (space, standard) in UNCODED_STANDARD_CURVES {
            let ours = opened_by_lcms(&IccProfile::builtin(space).expect("buildable"));
            let toe = (0..64).map(|step| f64::from(step) / 2048.0);
            let body = (0..=64).map(|step| f64::from(step) / 64.0);
            for signal in toe.chain(body) {
                for (channel, sig) in [
                    ("r", tag::RED_TRC),
                    ("g", tag::GREEN_TRC),
                    ("b", tag::BLUE_TRC),
                ] {
                    let got = f64::from(
                        ours.eval_tone_curve(sig, signal as f32)
                            .expect("TRC present"),
                    );
                    let want = standard(signal);
                    assert!(
                        (got - want).abs() < 2.0 / 65535.0,
                        "{space:?} {channel}TRC at V = {signal}: {got} vs {want}"
                    );
                }
            }
        }
    }

    /// lcms2 reads the `mediaWhitePointTag` of every profile built here — each built-in space and
    /// the grey constructor — as the D50 it writes into a matrix/TRC profile it builds itself.
    /// The colorant oracle only sees the white point through the colorants; this reads it.
    #[test]
    fn oracle_media_white_point_matches_lcms() {
        let reference = lcms2_oracle::rgb_matrix_shaper(
            [0.3127, 0.3290],
            [[0.64, 0.33], [0.30, 0.60], [0.15, 0.06]],
            [2.2, 2.2, 2.2],
        )
        .read_xyz(tag::MEDIA_WHITE_POINT)
        .expect("lcms2 writes a white point");
        let profiles = BuiltinProfile::ALL
            .into_iter()
            .map(|space| (format!("{space:?}"), IccProfile::builtin(space)))
            .chain([("grey".to_owned(), IccProfile::gray_with_gamma(2.2))]);
        for (label, profile) in profiles {
            let got = opened_by_lcms(&profile.expect("buildable"))
                .read_xyz(tag::MEDIA_WHITE_POINT)
                .expect("wtpt present");
            for axis in 0..3 {
                assert!(
                    (got[axis] - reference[axis]).abs() < 1.0 / 65536.0,
                    "{label} white point [{axis}]: {got:?} vs {reference:?}"
                );
            }
        }
    }

    /// lcms2 evaluates the three TRCs of the profile built for **every** encodable transfer code
    /// point to that transfer's own curve, taken from a source independent of the tag: H.273
    /// Table 3's forward function for the BT.709 family, Table 3's assumed display gammas written
    /// out as `V^2.2` and `V^2.8` for code points 4 and 5, the identity for linear, lcms2's own
    /// sRGB for sRGB, and `gamut-color`'s `pq_eotf` for PQ. The display-gamma, linear and PQ tags
    /// are otherwise only checked by this crate's own evaluator. An independent CMM reading an independent
    /// transcription of each curve is what says the tag this module *wrote* — function type,
    /// parameter order, `s15Fixed16` encoding, sampling — is the curve, not just that this
    /// crate's evaluator agrees with itself.
    ///
    /// The tolerance is two `uInt16` quanta: the sampled PQ table's own interpolation error is
    /// under one (see [`SAMPLED_TRC_POINTS`]), and lcms2 adds its 16-bit evaluation of the table
    /// on top. Measured worst cases on this grid: 1.08e-5 for PQ, 4.2e-6 for sRGB, 1.3e-6 for the
    /// BT.709 family, zero for linear. A wrong curve, direction or normalization moves a sample
    /// by orders of magnitude more.
    #[test]
    fn oracle_every_encodable_transfer_matches_its_curve_through_lcms() {
        let srgb = lcms2_oracle::srgb();
        let peak = pq_eotf(1.0);
        for code in [1_u8, 4, 5, 6, 8, 13, 14, 15, 16] {
            let ours = opened_by_lcms(&rgb_cicp(1, code).expect("an encodable transfer"));
            for step in 0..=64 {
                let light_or_signal = f64::from(step) / 64.0;
                // (signal, the light that signal must decode to)
                let (signal, want) = match code {
                    1 | 6 | 14 | 15 => (h273_bt709_oetf(light_or_signal), light_or_signal),
                    4 => (light_or_signal, light_or_signal.powf(2.2)),
                    5 => (light_or_signal, light_or_signal.powf(2.8)),
                    8 => (light_or_signal, light_or_signal),
                    13 => (
                        light_or_signal,
                        f64::from(
                            srgb.eval_tone_curve(tag::RED_TRC, light_or_signal as f32)
                                .expect("lcms2 sRGB rTRC"),
                        ),
                    ),
                    _ => (light_or_signal, pq_eotf(light_or_signal) / peak),
                };
                for (channel, sig) in [
                    ("r", tag::RED_TRC),
                    ("g", tag::GREEN_TRC),
                    ("b", tag::BLUE_TRC),
                ] {
                    let got = f64::from(
                        ours.eval_tone_curve(sig, signal as f32)
                            .expect("TRC present"),
                    );
                    assert!(
                        (got - want).abs() < 2.0 / 65535.0,
                        "transfer {code} {channel}TRC at V = {signal}: {got} vs {want}"
                    );
                }
            }
        }
    }

    /// lcms2 evaluates the three TRCs of the profile built under the BT.1886 reading, for each of
    /// the four BT.709-family code points, to Rec. ITU-R BT.1886 Annex 1's EOTF at reference
    /// black zero — `V^2.4`, written out here as the spec states it rather than read back from
    /// `gamut-color` or from the tag. It is what says the tag this reading *writes* (function
    /// type 0, its one `s15Fixed16` parameter) is the display curve for an independent CMM.
    ///
    /// Same two-`uInt16`-quanta tolerance as the literal reading's oracle; the `s15Fixed16`
    /// rounding of `g` is under 1e-6 of it.
    #[test]
    fn oracle_bt1886_reading_matches_its_curve_through_lcms() {
        for code in [1_u8, 6, 14, 15] {
            let profile = IccProfile::from_cicp_with(
                Cicp {
                    colour_primaries: 1,
                    transfer_characteristics: code,
                    matrix_coefficients: 0,
                    video_full_range_flag: 1,
                },
                CicpTransferReading::Bt1886Display,
            )
            .expect("an encodable transfer");
            let ours = opened_by_lcms(&profile);
            for step in 0..=64 {
                let signal = f64::from(step) / 64.0;
                let want = signal.powf(2.40);
                for (channel, sig) in [
                    ("r", tag::RED_TRC),
                    ("g", tag::GREEN_TRC),
                    ("b", tag::BLUE_TRC),
                ] {
                    let got = f64::from(
                        ours.eval_tone_curve(sig, signal as f32)
                            .expect("TRC present"),
                    );
                    assert!(
                        (got - want).abs() < 2.0 / 65535.0,
                        "transfer {code} {channel}TRC at V = {signal}: {got} vs {want}"
                    );
                }
            }
        }
    }

    /// The `cicpType` tag every buildable signalling writes is the one lcms2's own `cicp`
    /// synthesiser writes for the same four fields, read back through this crate's parser.
    /// The two inputs coincide once §10.3 has zeroed `MatrixCoefficients`, so each triple is
    /// built from a caller's `nclx`-style matrix 9 and compared with lcms2's tag for matrix 0:
    /// a field written at the wrong offset, or the matrix passed through, shows up as a mismatch
    /// against bytes this crate did not produce.
    #[test]
    fn oracle_cicp_tag_matches_the_lcms_synthesiser() {
        for primaries in buildable_primaries() {
            for transfer in [1_u8, 4, 5, 6, 8, 13, 14, 15, 16] {
                let ours = IccProfile::from_cicp(Cicp {
                    colour_primaries: primaries,
                    transfer_characteristics: transfer,
                    matrix_coefficients: 9,
                    video_full_range_flag: 1,
                })
                .expect("buildable signalling");
                let lcms =
                    IccProfile::parse(&lcms2_oracle::cicp(primaries, transfer, 0, 1).to_bytes())
                        .expect("lcms2's profile parses");
                assert_eq!(
                    ours.get(KnownTag::Cicp),
                    lcms.get(KnownTag::Cicp),
                    "cicp {primaries}/{transfer}"
                );
                assert!(ours.get(KnownTag::Cicp).is_some(), "cicp tag written");
            }
        }
    }

    /// lcms2 reports the grey profile's gamma as the one asked for. Pins the monochrome
    /// constructor's `kTRC` against an independent reader rather than against our own encoder.
    #[test]
    fn oracle_gray_gamma_matches_lcms() {
        for gamma in [1.0, 1.8, 2.2] {
            let bytes = IccProfile::gray_with_gamma(gamma)
                .expect("an encodable gamma")
                .to_bytes()
                .expect("serializes");
            let profile =
                lcms2_oracle::Profile::from_bytes(&bytes).expect("lcms2 re-opens the profile");
            let estimated = profile
                .estimate_gamma(tag::GRAY_TRC, 0.01)
                .expect("kTRC present");
            assert!(
                (estimated - gamma).abs() < 0.01,
                "grey gamma {gamma}: lcms2 estimates {estimated}"
            );
        }
    }
}
