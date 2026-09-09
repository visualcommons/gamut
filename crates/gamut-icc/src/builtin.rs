//! Built-in profile constructors: a spec-valid [`IccProfile`] for a named colour space, for a
//! [`SourceProfile`], or for a CICP signalling triple.
//!
//! Every profile built here is a **v4 three-component matrix/TRC display profile** (ICC.1:2022
//! §8.4) — or, for [`IccProfile::gray_with_gamma`], the monochrome model of the same class. The
//! colorimetry is never written down twice: the primaries and white point come from
//! [`gamut_color::cicp::ColourPrimaries::chromaticities`], the RGB→XYZ construction and the
//! Bradford adaptation to the D50 PCS from [`gamut_color::matrix`], and the ST 2084 curve from
//! [`gamut_color::transfer`]. This crate contributes the ICC encoding, not the numbers.
//!
//! # Which spaces
//!
//! The buildable set is exactly what `gamut-color` can express on the two CICP axes — because a
//! matrix/TRC profile *is* a (primaries, transfer) pair. [`gamut_color::SourceProfile`]'s
//! `ADOBE_RGB` and `PROPHOTO_RGB` return `None` from both
//! [`colour_primaries`](gamut_color::SourceProfile::colour_primaries) and
//! [`transfer_characteristics`](gamut_color::SourceProfile::transfer_characteristics), and their
//! chromaticities are private to `gamut-color`, so [`IccProfile::from_source_profile`] returns
//! `None` for them rather than this module retyping the tables.
//!
//! # Determinism
//!
//! A constructor is a pure function of its arguments: the creation date is
//! [`DateTime::ZERO`](crate::DateTime::ZERO), the profile ID is left unset, and every
//! open-registry header field is zero, so the same call always serializes to the same bytes. Stamp
//! an ID with [`IccWriter::recompute_profile_id`](crate::IccWriter::recompute_profile_id) if one
//! is wanted.

use gamut_color::SourceProfile;
use gamut_color::cicp::{ColourPrimaries, TransferCharacteristics};
use gamut_color::linalg::mat_mul3;
use gamut_color::matrix::{bradford_adapt, rgb_to_xyz_matrix};
use gamut_color::transfer::pq_eotf;

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
const SAMPLED_TRC_POINTS: usize = 1024;

/// The 3×3 identity, used as the total fallback in [`colorants_d50`].
const IDENTITY_3X3: [[f64; 3]; 3] = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];

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
    Bt2020Pq = 3,
}

impl BuiltinProfile {
    /// The two CICP axes this space is defined by, the tone curve that encodes its transfer, and
    /// the text of its `profileDescriptionTag`.
    ///
    /// The curve is named here rather than derived through [`Trc::from_cicp`] so that
    /// [`IccProfile::builtin`] is total with no unreachable fallback; that the two agree is
    /// pinned by `each_builtin_space_names_the_curve_for_its_own_transfer`.
    fn parts(self) -> (ColourPrimaries, TransferCharacteristics, Trc, &'static str) {
        match self {
            BuiltinProfile::Srgb => (
                ColourPrimaries::Bt709,
                TransferCharacteristics::Srgb,
                Trc::Srgb,
                "sRGB",
            ),
            BuiltinProfile::LinearSrgb => (
                ColourPrimaries::Bt709,
                TransferCharacteristics::Linear,
                Trc::Gamma(1.0),
                "Linear sRGB",
            ),
            BuiltinProfile::DisplayP3 => (
                ColourPrimaries::DisplayP3,
                TransferCharacteristics::Srgb,
                Trc::Srgb,
                "Display P3",
            ),
            BuiltinProfile::Bt2020Pq => (
                ColourPrimaries::Bt2020,
                TransferCharacteristics::Pq,
                Trc::Pq,
                "BT.2100 PQ",
            ),
        }
    }

    /// Every space, for the exhaustive sweeps that must not miss one.
    const ALL: [BuiltinProfile; 4] = [
        BuiltinProfile::Srgb,
        BuiltinProfile::LinearSrgb,
        BuiltinProfile::DisplayP3,
        BuiltinProfile::Bt2020Pq,
    ];

    /// The `cicpType` value this space signals: its two axes, the identity matrix coefficients
    /// (H.273 code point 0 — the profile describes RGB, not a luma–chroma encoding), and the
    /// full-range flag.
    ///
    /// # Examples
    ///
    /// ```
    /// use gamut_icc::BuiltinProfile;
    /// let cicp = BuiltinProfile::Bt2020Pq.cicp();
    /// assert_eq!((cicp.colour_primaries, cicp.transfer_characteristics), (9, 16));
    /// ```
    #[must_use]
    pub fn cicp(self) -> Cicp {
        let (primaries, transfer, _, _) = self.parts();
        cicp_of(primaries, transfer)
    }

    /// The space whose axes are exactly `primaries` and `transfer`, if there is one. Lets
    /// [`IccProfile::from_cicp`] name a profile it recognizes instead of describing it by code
    /// point.
    fn for_axes(primaries: ColourPrimaries, transfer: TransferCharacteristics) -> Option<Self> {
        Self::ALL.into_iter().find(|space| {
            let (p, t, _, _) = space.parts();
            (p, t) == (primaries, transfer)
        })
    }
}

/// The `cicpType` value for a pair of CICP axes, with identity matrix coefficients and the
/// full-range flag set.
fn cicp_of(primaries: ColourPrimaries, transfer: TransferCharacteristics) -> Cicp {
    Cicp {
        colour_primaries: cicp_byte(primaries.code_point()),
        transfer_characteristics: cicp_byte(transfer.code_point()),
        matrix_coefficients: 0,
        video_full_range_flag: 1,
    }
}

/// A CICP code point as the byte `cicpType` stores it (ICC.1:2022 §10.7 — four `uInt8`s).
///
/// ITU-T H.273 defines every code point in `0..=255`, so this is total for any modelled value; a
/// value that does not fit a byte cannot be signalled at all and becomes `2` (Unspecified).
fn cicp_byte(code_point: u16) -> u8 {
    u8::try_from(code_point).unwrap_or(2)
}

/// A tone-response curve this module can encode.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Trc {
    /// `Y = X^g`, a `parametricCurveType` function type 0 (ICC.1:2022 §10.18).
    Gamma(f64),
    /// IEC 61966-2-1 (sRGB), a `parametricCurveType` function type 3 — the spec's own piecewise
    /// closed form, so the encoding is exact rather than sampled.
    Srgb,
    /// SMPTE ST 2084 (PQ) normalized to its peak. ICC.1:2022 §10.18 defines no closed form for
    /// it, so it is a sampled `curveType` (§10.6).
    Pq,
}

impl Trc {
    /// The curve for a CICP transfer code point, or `None` where `gamut-color` implements no
    /// curve for it ([`TransferCharacteristics::Bt709`], [`TransferCharacteristics::Hlg`] and
    /// [`TransferCharacteristics::Unspecified`] — see
    /// [`gamut_color::transfer::eotf_for`]).
    fn from_cicp(transfer: TransferCharacteristics) -> Option<Self> {
        match transfer {
            TransferCharacteristics::Linear => Some(Trc::Gamma(1.0)),
            TransferCharacteristics::Srgb => Some(Trc::Srgb),
            TransferCharacteristics::Pq | TransferCharacteristics::Bt2020_10 => Some(Trc::Pq),
            TransferCharacteristics::Bt709
            | TransferCharacteristics::Hlg
            | TransferCharacteristics::Unspecified => None,
            // `TransferCharacteristics` is `#[non_exhaustive]`: a code point gamut-color models
            // later has no curve here until this match names it.
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
/// Deliberately *not* [`gamut_color::matrix::D50`]: that is the CIE-published chromaticity, whose
/// tristimulus Z is 0.82521, while ICC's rounded encoding is 0.8249. Adapting to the CIE one while
/// writing the ICC one as the `mediaWhitePointTag` would leave the colorants disagreeing with the
/// white point they are supposed to sum to, by 2e-4 in Z. The PCS illuminant is an ICC fact, so
/// this crate owns it.
fn pcs_d50_chromaticity() -> [f64; 2] {
    let [x, y, z] = XyzNumber::D50.to_f64();
    let sum = x + y + z;
    [x / sum, y / sum]
}

/// The D50-adapted colorant columns for `primaries`, and the chromatic-adaptation matrix that took
/// them there — the `rXYZ`/`gXYZ`/`bXYZ` (§9.2.10) and `chad` (§9.2.35) tag contents.
///
/// Total by construction. A code point that names no chromaticities
/// ([`ColourPrimaries::Unspecified`]) — or, were one ever added, chromaticities with no RGB→XYZ
/// matrix — yields the identity for both, i.e. colorants equal to the PCS axes. Every public entry
/// point rejects `Unspecified` before reaching here, so that result is not observable through them.
fn colorants_d50(primaries: ColourPrimaries) -> ([[f64; 3]; 3], [[f64; 3]; 3]) {
    let (rgb_to_xyz, chad) = primaries
        .chromaticities()
        .and_then(|(rgb, white)| {
            rgb_to_xyz_matrix(&rgb, white).zip(bradford_adapt(white, pcs_d50_chromaticity()))
        })
        .unwrap_or((IDENTITY_3X3, IDENTITY_3X3));
    (mat_mul3(&chad, &rgb_to_xyz), chad)
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

/// The three-component matrix/TRC display profile for a pair of CICP axes (ICC.1:2022 §8.4).
///
/// `cicp` is the `cicpType` element to record the signalling the profile was built from.
fn rgb_matrix_trc(
    primaries: ColourPrimaries,
    trc: Trc,
    description: &str,
    cicp: Cicp,
) -> IccProfile {
    let (colorants, chad) = colorants_d50(primaries);
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
    IccProfile {
        header: ProfileHeader::new(DeviceClass::Display, ColorSpace::Rgb),
        tags: vec![
            (KnownTag::ProfileDescription.into(), mluc(description)),
            (KnownTag::Copyright.into(), mluc(COPYRIGHT)),
            (KnownTag::MediaWhitePoint.into(), TagData::Xyz(vec![XyzNumber::D50])),
            (KnownTag::ChromaticAdaptation.into(), chad_tag),
            (KnownTag::RedColorant.into(), column(0)),
            (KnownTag::GreenColorant.into(), column(1)),
            (KnownTag::BlueColorant.into(), column(2)),
            (KnownTag::RedTrc.into(), curve.clone()),
            (KnownTag::GreenTrc.into(), curve.clone()),
            (KnownTag::BlueTrc.into(), curve),
            (KnownTag::Cicp.into(), TagData::Cicp(cicp)),
        ],
    }
}

impl IccProfile {
    /// A v4 matrix/TRC display profile for a named colour space.
    ///
    /// The media white point is the D50 the PCS mandates and the colorants are adapted to it with
    /// the Bradford matrix recorded in `chad`, so the result satisfies
    /// [`validate`](IccProfile::validate) for the Display class with no further setup.
    ///
    /// # Examples
    ///
    /// ```
    /// use gamut_icc::{BuiltinProfile, IccProfile, KnownTag, TagData};
    ///
    /// let profile = IccProfile::builtin(BuiltinProfile::DisplayP3);
    /// assert!(profile.validate().is_empty());
    /// assert!(matches!(profile.get(KnownTag::RedColorant), Some(TagData::Xyz(_))));
    /// ```
    #[must_use]
    pub fn builtin(space: BuiltinProfile) -> Self {
        let (primaries, _, trc, description) = space.parts();
        rgb_matrix_trc(primaries, trc, description, space.cicp())
    }

    /// A v4 monochrome display profile with a pure-gamma grey tone curve (`Y = X^gamma`).
    ///
    /// The white point is D50, so no chromatic adaptation is needed and no `chad` tag is written.
    ///
    /// # Examples
    ///
    /// ```
    /// use gamut_icc::{IccProfile, KnownTag, TagData};
    ///
    /// let profile = IccProfile::gray_with_gamma(2.2);
    /// assert!(profile.validate().is_empty());
    /// assert!(matches!(profile.get(KnownTag::GrayTrc), Some(TagData::ParametricCurve(_))));
    /// ```
    #[must_use]
    pub fn gray_with_gamma(gamma: f64) -> Self {
        IccProfile {
            header: ProfileHeader::new(DeviceClass::Display, ColorSpace::Gray),
            tags: vec![
                (
                    KnownTag::ProfileDescription.into(),
                    mluc(&format!("Grey gamma {gamma}")),
                ),
                (KnownTag::Copyright.into(), mluc(COPYRIGHT)),
                (KnownTag::MediaWhitePoint.into(), TagData::Xyz(vec![XyzNumber::D50])),
                (KnownTag::GrayTrc.into(), Trc::Gamma(gamma).tag()),
            ],
        }
    }

    /// A v4 matrix/TRC display profile for a CICP signalling triple — the path from what AVIF,
    /// HEIC and JXL usually carry (a `colr`/`nclx` code-point trio) to an embeddable profile.
    ///
    /// The `cicpType` tag records `cicp` verbatim, including its matrix coefficients and range
    /// flag: those describe a luma–chroma *encoding*, which a matrix/TRC profile does not model,
    /// so the profile's own pipeline always describes the RGB signal after any de-matrixing.
    ///
    /// Returns `None` when the profile cannot describe the signalling: an unmodelled or
    /// [`Unspecified`](ColourPrimaries::Unspecified) primaries code point, or a transfer
    /// characteristic for which `gamut-color` implements no curve (BT.709 and HLG today).
    ///
    /// # Examples
    ///
    /// ```
    /// use gamut_icc::{BuiltinProfile, Cicp, IccProfile};
    ///
    /// // BT.709 primaries + sRGB transfer is sRGB, and is built as such.
    /// let signalled = Cicp { colour_primaries: 1, transfer_characteristics: 13,
    ///                        matrix_coefficients: 0, video_full_range_flag: 1 };
    /// assert_eq!(IccProfile::from_cicp(signalled), Some(IccProfile::builtin(BuiltinProfile::Srgb)));
    ///
    /// // "Unspecified" primaries name no chromaticities, so no profile can be built.
    /// assert!(IccProfile::from_cicp(Cicp { colour_primaries: 2, ..signalled }).is_none());
    /// ```
    #[must_use]
    pub fn from_cicp(cicp: Cicp) -> Option<Self> {
        let primaries = ColourPrimaries::from_code_point(u16::from(cicp.colour_primaries))?;
        if primaries == ColourPrimaries::Unspecified {
            return None;
        }
        let transfer =
            TransferCharacteristics::from_code_point(u16::from(cicp.transfer_characteristics))?;
        let trc = Trc::from_cicp(transfer)?;
        let named = BuiltinProfile::for_axes(primaries, transfer);
        let description = match named {
            Some(space) => space.parts().3.to_owned(),
            None => format!(
                "CICP {}/{}",
                cicp.colour_primaries, cicp.transfer_characteristics
            ),
        };
        Some(rgb_matrix_trc(primaries, trc, &description, cicp))
    }

    /// A v4 matrix/TRC display profile for a [`SourceProfile`] — `gamut-color`'s
    /// `(gamut, transfer)` bundle.
    ///
    /// Returns `None` for a bundle with no CICP axes: `ADOBE_RGB` and `PROPHOTO_RGB` have neither
    /// a primaries nor a transfer code point, and their chromaticities are private to
    /// `gamut-color`, so this crate cannot describe them without restating tables it does not own.
    ///
    /// # Examples
    ///
    /// ```
    /// use gamut_color::SourceProfile;
    /// use gamut_icc::{BuiltinProfile, IccProfile};
    ///
    /// assert_eq!(
    ///     IccProfile::from_source_profile(SourceProfile::SRGB),
    ///     Some(IccProfile::builtin(BuiltinProfile::Srgb)),
    /// );
    /// assert!(IccProfile::from_source_profile(SourceProfile::ADOBE_RGB).is_none());
    /// ```
    #[must_use]
    pub fn from_source_profile(source: SourceProfile) -> Option<Self> {
        let primaries = source.colour_primaries()?;
        let transfer = source.transfer_characteristics()?;
        Self::from_cicp(cicp_of(primaries, transfer))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gamut_color::transfer::srgb_eotf;
    use lcms2_oracle::tag;

    /// Every [`BuiltinProfile`] and the grey constructor satisfy ICC.1:2022 §8's required-tag set
    /// for the Display class, and serialize. `validate` is gamut-icc's own §8 checker, so this
    /// pins the *tag set* each constructor emits, not its colorimetry.
    #[test]
    fn every_constructor_satisfies_the_section_8_display_model() {
        let profiles = [
            ("srgb", IccProfile::builtin(BuiltinProfile::Srgb)),
            ("linear", IccProfile::builtin(BuiltinProfile::LinearSrgb)),
            ("p3", IccProfile::builtin(BuiltinProfile::DisplayP3)),
            ("pq", IccProfile::builtin(BuiltinProfile::Bt2020Pq)),
            ("gray", IccProfile::gray_with_gamma(2.2)),
        ];
        for (label, profile) in profiles {
            assert_eq!(profile.validate(), vec![], "{label}: §8 conformance");
            assert!(profile.to_bytes().is_ok(), "{label}: serializes");
        }
    }

    /// `colorants_d50` is total: a code point naming no chromaticities yields the identity for
    /// both the colorants and the adaptation matrix. This is the arm the public constructors can
    /// never reach, so it is pinned here at the only input that reaches it.
    #[test]
    fn colorants_of_unspecified_primaries_are_the_identity() {
        let (colorants, chad) = colorants_d50(ColourPrimaries::Unspecified);
        assert_eq!(colorants, IDENTITY_3X3);
        assert_eq!(chad, IDENTITY_3X3);
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
            assert!((got - want).abs() < 1.0e-4, "sRGB TRC at {x}: {got} vs {want}");
        }
    }

    /// The sampled PQ curve reproduces `gamut-color`'s `pq_eotf` normalized to its peak, to within
    /// the `uInt16` quantum the table is written in. This is what justifies
    /// [`SAMPLED_TRC_POINTS`]: it pins that the sampling density is fine enough to make the
    /// encoding, not the table, the limit on accuracy.
    #[test]
    fn sampled_pq_curve_matches_gamut_color() {
        let TagData::Curve(curve) = Trc::Pq.tag() else {
            panic!("PQ is encoded as a sampled curveType");
        };
        let peak = pq_eotf(1.0);
        for step in 0..=100 {
            let x = f64::from(step) / 100.0;
            let (got, want) = (curve.eval(x), pq_eotf(x) / peak);
            assert!(
                (got - want).abs() < 2.0 / 65535.0,
                "PQ TRC at {x}: {got} vs {want}"
            );
        }
    }

    /// The four built-in spaces have four distinct sets of axes, and each round-trips through its
    /// `cicpType` value back to itself. A space whose `parts` named the wrong code point would
    /// collide with another or fail to round-trip.
    #[test]
    fn each_builtin_space_round_trips_through_its_cicp_value() {
        for space in BuiltinProfile::ALL {
            let (primaries, transfer, _, _) = space.parts();
            assert_eq!(
                BuiltinProfile::for_axes(primaries, transfer),
                Some(space),
                "{space:?} is the space for its own axes"
            );
            assert_eq!(
                IccProfile::from_cicp(space.cicp()),
                Some(IccProfile::builtin(space)),
                "{space:?} builds the same profile from its own CICP value"
            );
        }
    }

    /// The curve each space names is the one its own transfer code point maps to. `parts` states
    /// the curve directly so that `builtin` needs no fallback, and this is what stops the two
    /// halves of that statement drifting apart.
    #[test]
    fn each_builtin_space_names_the_curve_for_its_own_transfer() {
        for space in BuiltinProfile::ALL {
            let (_, transfer, trc, _) = space.parts();
            assert_eq!(Trc::from_cicp(transfer), Some(trc), "{space:?}");
        }
    }

    /// A transfer `gamut-color` implements no curve for cannot be built, and neither can
    /// unmodelled or unspecified primaries. Each rejection is asserted at an input that isolates
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
                "BT.709 transfer, no curve in gamut-color",
                Cicp {
                    transfer_characteristics: 1,
                    ..srgb
                },
            ),
            (
                "HLG transfer, no curve in gamut-color",
                Cicp {
                    transfer_characteristics: 18,
                    ..srgb
                },
            ),
        ] {
            assert_eq!(IccProfile::from_cicp(cicp), None, "{label}");
        }
    }

    /// `SourceProfile`'s bundles map onto the built-in spaces, and the two with no CICP axes are
    /// declined rather than approximated.
    #[test]
    fn source_profiles_map_onto_the_builtin_spaces() {
        let cases = [
            (SourceProfile::SRGB, Some(BuiltinProfile::Srgb)),
            (SourceProfile::LINEAR_SRGB, Some(BuiltinProfile::LinearSrgb)),
            (SourceProfile::DISPLAY_P3, Some(BuiltinProfile::DisplayP3)),
            (SourceProfile::BT2020, Some(BuiltinProfile::Bt2020Pq)),
            (SourceProfile::ADOBE_RGB, None),
            (SourceProfile::PROPHOTO_RGB, None),
        ];
        for (source, expected) in cases {
            let want = expected.map(IccProfile::builtin);
            assert_eq!(
                IccProfile::from_source_profile(source),
                want,
                "{source:?} → {expected:?}"
            );
        }
    }

    /// Each space's colorants sum to the media white point the same profile declares — the law a
    /// matrix/TRC profile has to satisfy for full-scale RGB to land on the PCS white. It is what
    /// forces the Bradford adaptation to target [`pcs_d50_chromaticity`] rather than the CIE D50.
    #[test]
    fn colorants_sum_to_the_declared_media_white_point() {
        let want = XyzNumber::D50.to_f64();
        for space in BuiltinProfile::ALL {
            let (primaries, _, _, label) = space.parts();
            let (colorants, _) = colorants_d50(primaries);
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

    /// A constructor is a pure function of its arguments: the same call serializes to the same
    /// bytes. Guards the "no timestamp, no ID, no entropy" property the module doc promises, which
    /// a later `DateTime::now()` would silently break.
    #[test]
    fn constructors_are_byte_deterministic() {
        let first = IccProfile::builtin(BuiltinProfile::Srgb).to_bytes();
        let second = IccProfile::builtin(BuiltinProfile::Srgb).to_bytes();
        assert_eq!(first.ok(), second.ok());
    }

    // --- differential: Little-CMS re-opens what we built ------------------------------------

    /// lcms2 re-opens each built-in profile and reports the same colorants as it computes for the
    /// same primaries itself. This is the colorimetric acceptance gate: the D50 adaptation, the
    /// column-vs-row orientation of the colorant tags and the `s15Fixed16` encoding are all only
    /// checked against an independent implementation.
    ///
    /// The tolerance is four `s15Fixed16` quanta, so it is the tag encoding — not the derivation —
    /// that sets it: the largest observed disagreement is 2.4e-5, under two quanta. A transposed
    /// matrix, a missing Bradford adaptation or a wrong white point all move a colorant by more
    /// than 1e-2.
    #[test]
    fn oracle_colorants_match_lcms_for_the_same_primaries() {
        for space in [
            BuiltinProfile::Srgb,
            BuiltinProfile::DisplayP3,
            BuiltinProfile::Bt2020Pq,
        ] {
            let (primaries, _, _, label) = space.parts();
            let (rgb, white) = primaries.chromaticities().expect("a modelled code point");
            // lcms2 builds its own matrix/TRC profile from the same chromaticities; the gamma is
            // irrelevant to the colorants.
            let reference = lcms2_oracle::rgb_matrix_shaper(white, rgb, [2.2, 2.2, 2.2]);
            let ours = lcms2_oracle::Profile::from_bytes(
                &IccProfile::builtin(space).to_bytes().expect("serializes"),
            )
            .expect("lcms2 re-opens the profile");

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
    }

    /// lcms2 evaluates our sRGB tone curve to the same values as it evaluates the sRGB curve in
    /// the profile it synthesizes itself. Two independent constructions of IEC 61966-2-1, read by
    /// the same reference CMM.
    #[test]
    fn oracle_srgb_tone_curve_matches_lcms() {
        let ours = lcms2_oracle::Profile::from_bytes(
            &IccProfile::builtin(BuiltinProfile::Srgb)
                .to_bytes()
                .expect("serializes"),
        )
        .expect("lcms2 re-opens the profile");
        let reference = lcms2_oracle::srgb();

        for step in 0..=20 {
            let x = step as f32 / 20.0;
            let got = ours.eval_tone_curve(tag::RED_TRC, x).expect("rTRC present");
            let want = reference
                .eval_tone_curve(tag::RED_TRC, x)
                .expect("rTRC present");
            assert!(
                (got - want).abs() < 1.0e-4,
                "sRGB rTRC at {x}: {got} vs {want}"
            );
        }
    }

    /// A transform from our sRGB profile to lcms2's own sRGB profile is the identity, to within a
    /// code. This is the end-to-end acceptance: colorants, white point, adaptation and TRC all
    /// have to be right together for a full round trip through the PCS to come back unchanged.
    #[test]
    fn oracle_transform_through_our_srgb_is_the_identity() {
        let ours = lcms2_oracle::Profile::from_bytes(
            &IccProfile::builtin(BuiltinProfile::Srgb)
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

    /// lcms2 reports the grey profile's gamma as the one asked for. Pins the monochrome
    /// constructor's `kTRC` against an independent reader rather than against our own encoder.
    #[test]
    fn oracle_gray_gamma_matches_lcms() {
        for gamma in [1.0, 1.8, 2.2] {
            let bytes = IccProfile::gray_with_gamma(gamma)
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
