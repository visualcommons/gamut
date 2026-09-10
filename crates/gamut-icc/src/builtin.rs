//! Built-in profile constructors: a spec-valid [`IccProfile`] for a named colour space, for a
//! [`SourceProfile`], or for a CICP signalling triple.
//!
//! Every profile built here is a **v4 three-component matrix/TRC display profile** (ICC.1:2022
//! §8.4) — or, for [`IccProfile::gray_with_gamma`], the monochrome model of the same class.
//!
//! **Colorimetry `gamut-color` can supply is never retyped here**: the primaries and white point
//! come from [`gamut_color::cicp::ColourPrimaries::chromaticities`], the RGB→XYZ construction and
//! the Bradford adaptation to the D50 PCS from [`gamut_color::matrix`], and the ST 2084 curve from
//! [`gamut_color::transfer`].
//!
//! Three published constants it does not supply *are* written here, because an ICC tag needs them
//! in a form no `gamut-color` function returns. Each is gated against something outside this
//! module rather than trusted:
//!
//! | Restated here | Why it cannot be borrowed | What gates it |
//! | --- | --- | --- |
//! | [`BT709_BETA`] (H.273 §8.2); [`BT709_ALPHA`] is *derived* from it, not restated | `gamut-color` has no BT.709-family curve at all — see "Which spaces" | `bt709_curve_inverts_the_h273_transfer`, against `h273_bt709_oetf`, a forward transcription of Table 3 that restates α and β on purpose so no mistyped digit is shared; and `oracle_bt709_tone_curve_matches_the_h273_transfer` through lcms2 |
//! | The IEC 61966-2-1 `(g, a, b, c, d)` set for [`Trc::Srgb`] | `gamut_color::transfer::srgb_eotf` is a *function*; §10.18 type 3 needs its five parameters | `srgb_parametric_curve_matches_gamut_color` against that function, and `oracle_srgb_tone_curve_matches_lcms` |
//! | The PCS D50, via [`XyzNumber::D50`] | It is an **ICC** fact (§7.2.16), not a CIE one; see [`pcs_d50_chromaticity`] | `colorants_sum_to_the_declared_media_white_point`, and the lcms2 colorant oracle |
//!
//! Everything else this module contributes is ICC encoding, not colour science.
//!
//! # Which spaces
//!
//! A matrix/TRC profile *is* a (primaries, transfer) pair, and the two axes are resolved from
//! different places. **Primaries** come from `gamut-color`: a code point it gives no
//! chromaticities for cannot be described, which is why [`gamut_color::SourceProfile`]'s
//! `ADOBE_RGB` and `PROPHOTO_RGB` — `None` from both
//! [`colour_primaries`](gamut_color::SourceProfile::colour_primaries) and
//! [`transfer_characteristics`](gamut_color::SourceProfile::transfer_characteristics), with
//! chromaticities private to `gamut-color` — are declined by
//! [`IccProfile::from_source_profile`] rather than having their tables retyped here.
//! **Transfers** are keyed on the raw ITU-T H.273 code point, because the set of curves an ICC
//! tag can *encode* is not the set `gamut-color` can *evaluate*: H.273 Table 3's code points 1,
//! 6, 14 and 15 are one curve with a closed form ICC.1:2022 §10.18 defines exactly, so this
//! module encodes all four, while `gamut_color::cicp::TransferCharacteristics` models only two of
//! them (1 and 14) and `gamut_color::transfer::eotf_for` returns a curve for only one — code
//! point 14, which it maps to `bt2020_pq_to_sdr`, a PQ EOTF followed by a tone map to SDR rather
//! than Table 3's curve. Keying on the code point is what keeps that tone map out of an ICC tag:
//! at `V = 0.5` it evaluates 20.3 % above the curve this module writes for the same code point.
//! Which of the two readings of code point 14 is right is `gamut-color`'s question, not this
//! crate's, and is tracked at <https://github.com/visualcommons/gamut/issues/605>.
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
//! // … and exactly one of those two has an EOTF, which is not this curve: at V = 0.5 it sits
//! // 20.3 % above Table 3's 0.259719, the light this module writes for the same code point
//! // (pinned by `bt709_curve_inverts_the_h273_transfer`).
//! assert!(eotf_for(Tc::Bt709).is_none());
//! let tone_mapped = eotf_for(Tc::Bt2020_10).expect("code point 14 has an EOTF")(0.5);
//! assert!(((tone_mapped / 0.259_719 - 1.0) * 100.0 - 20.3).abs() < 0.05);
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
//! two, one is rewritten and one is a precondition. They do **not** rest on the same authority, and
//! are documented apart on purpose.
//!
//! `MatrixCoefficients` is **conformance**, and is rewritten. ICC.1:2022 §10.3 states that *"when
//! the data colour space in the profile header is RGB or XYZ, MatrixCoefficients shall be 0
//! (zero)"* — a `shall` — so the caller's value cannot be carried: an AVIF or HEIC `nclx` box
//! routinely signals 1, 5, 6 or 9, and writing any of those into an RGB profile would make it
//! non-conforming. Nothing is lost by that. The coefficients describe a luma–chroma *encoding* the
//! caller de-matrixes before the profile applies, and they remain in the container's own
//! signalling where a decoder reads them. The sample *range* that accompanies such an encoding is
//! not theirs to describe — that is the next field, and it is guarded, not rewritten.
//!
//! `VideoFullRangeFlag` is **not** conformance, and is **not** rewritten either: a triple carrying
//! anything but full range (`1`) is **declined**. §10.3 says only that the flag "is often 1" for an
//! RGB profile, and its own RGB examples include `1-1-0-0` and `9-16-0-0` with the flag at zero, so
//! a narrow-range RGB `cicpType` is a legal tag — it is simply not one this module can build.
//! Everything the profile contains — the colorant matrix, the `chad` and the tone curves — is
//! defined over full-scale RGB, and read §10.3's `1-1-0-0` example closely: with
//! `MatrixCoefficients` already zero it is a narrow range on the **RGB samples themselves**, so
//! de-matrixing does not remove it. Normalising the flag to `1` would hand back a profile that
//! renders the caller's colour *wrongly*, not one that merely dropped a piece of metadata.
//! Declining is what this module already does for primaries it has no chromaticities for and for a
//! transfer with no ICC tone curve, and narrow range is the same case: signalling this profile
//! shape cannot describe. A caller holding narrow-range samples scales them to full range first,
//! and then the triple it passes is one this module builds.
//!
//! Nor is carrying the flag through unchanged — the option that looks most conservative, since it
//! discards nothing — merely inconsistent. §9.2.17 makes it **non-conforming**: the colour
//! encoding a `cicpType` tag specifies *"shall be equivalent to the data colour space encoding
//! represented by this ICC profile"*, and a narrow-range triple sitting beside full-scale
//! colorants and tone curves is not equivalent to what the profile represents. There is no
//! reading of the tag under which all three of "keep the flag", "keep the colorimetry" and
//! "conform" hold together.
//!
//! # Which reading of transfer 1, 6, 14 and 15
//!
//! H.273 Table 3 defines those four code points as an **opto-electronic** function, and an ICC
//! tone curve encodes signal → light, so this module writes its exact inverse:
//! `Lc = ((V + α − 1) / α)^(1/0.45)` above the knee. That is the literal reading, and it is what
//! the reference implementations write, so a profile built here agrees with them.
//!
//! It is not the only sanctioned reading. H.273 §8.2 NOTE 1 observes that for these code points
//! "a suggested corresponding reference electro-optical transfer characteristic function for flat
//! panel displays used in HDTV studio production has been specified in Rec. ITU-R BT.1886-0" —
//! which, at reference black zero, is a pure gamma of 2.4. Since every profile built here is a
//! *display*-class profile, that reading has a real claim, and the two are far apart.
//!
//! At mid-grey `V = 0.5` the PCS `Y` is as follows. The two rows this module writes are quoted as
//! the **tag evaluates**, with `(g, a, b, c, d)` already rounded to `s15Fixed16` — not as the
//! closed form before rounding, which is a different number in the sixth decimal. The BT.1886 row
//! is the closed form, because no tag here holds it.
//!
//! | reading | `Y` at `V = 0.5` |
//! | --- | --- |
//! | inverse OETF — the `parametricCurveType` this module writes | 0.259721 |
//! | BT.1886 EOTF, `V^2.4` — closed form; not written here | 0.189465 |
//! | the sRGB code point (13) as this module writes it, for scale | 0.214045 |
//!
//! The literal reading is 1.371× the BT.1886 one — 0.0703 in absolute `Y`. Note the third row:
//! two code points this module *does* encode, 1 and 13, already differ by 0.0457 at mid-grey
//! (21 % of the sRGB value), so a caller that treats "BT.709 primaries" and "sRGB" as
//! interchangeable will see that much of a shift from the transfer alone. Offering the BT.1886
//! reading as an option is tracked at
//! <https://github.com/visualcommons/gamut/issues/586>.
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
/// Measured **0.986** quanta at `V ≈ 0.9956` over a 200 001-point sweep, and 0.9854 over the
/// midpoint sweep `sampled_pq_curve_matches_gamut_color` runs — which is why that test's
/// tolerance is one quantum: a guard at twice the claimed bound is not a guard on the claim.
const SAMPLED_TRC_POINTS: usize = 1024;

/// A 3×3 matrix in row-major order — the shape `gamut_color::matrix` produces and consumes.
type Matrix3 = [[f64; 3]; 3];

/// ITU-T H.273 (07/2024) §8.2: for `TransferCharacteristics` 1, 6, 14 and 15, β is "the positive
/// constant necessary for the curve segments that meet at the value β to have continuity of both
/// value and slope", stated there as `0.018053968510807...`.
const BT709_BETA: f64 = 0.018_053_968_510_807;

/// ITU-T H.273 (07/2024) §8.2: `α = 1 + 5.5 * β = 1.099296826809442...`, derived from
/// [`BT709_BETA`] rather than restated so the two cannot disagree.
const BT709_ALPHA: f64 = 1.0 + 5.5 * BT709_BETA;

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
    /// The curve is named here rather than derived through [`Trc::for_code_point`] so that
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

    /// The space whose axes are exactly `primaries` and the H.273 transfer code point
    /// `transfer`, if there is one. Lets [`IccProfile::from_cicp`] name a profile it recognizes
    /// instead of describing it by code point.
    fn for_axes(primaries: ColourPrimaries, transfer: u8) -> Option<Self> {
        Self::ALL.into_iter().find(|space| {
            let (p, t, _, _) = space.parts();
            p == primaries && cicp_byte(t.code_point()) == transfer
        })
    }
}

/// The one `VideoFullRangeFlag` a profile built here can describe (ITU-T H.273 signals full range
/// as `1`).
///
/// Stated once and read from both ends of the module: it is what [`cicp_of`] writes and what
/// [`IccProfile::from_cicp`] requires of the caller, so the tag and the precondition cannot drift.
/// Every colorant, `chad` and tone curve written here is defined over full-scale RGB; see the
/// module docs for why narrow range is declined rather than normalised.
const FULL_RANGE: u8 = 1;

/// The `cicpType` value for a pair of CICP axes.
fn cicp_of(primaries: ColourPrimaries, transfer: TransferCharacteristics) -> Cicp {
    rgb_conforming_cicp(Cicp {
        colour_primaries: cicp_byte(primaries.code_point()),
        transfer_characteristics: cicp_byte(transfer.code_point()),
        // Set by `rgb_conforming_cicp`, which owns the §10.3 rule for every constructor.
        matrix_coefficients: 0,
        video_full_range_flag: FULL_RANGE,
    })
}

/// `cicp` with `MatrixCoefficients` replaced by the zero ICC.1:2022 §10.3 **requires** of an RGB or
/// XYZ profile.
///
/// That is the only field this module rewrites. `VideoFullRangeFlag` is not normalised here: a
/// triple that does not already carry [`FULL_RANGE`] is declined by
/// [`IccProfile::from_cicp`] instead, so nothing reaching this function can disagree with the
/// colorants and curves written alongside it. The two axes the profile is built from pass through
/// untouched.
fn rgb_conforming_cicp(cicp: Cicp) -> Cicp {
    Cicp {
        matrix_coefficients: 0,
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
    /// This is the *inverse of Table 3's opto-electronic function*, the literal reading; H.273
    /// §8.2 NOTE 1 sanctions a second one for display profiles, which the module docs quantify
    /// against this one.
    Bt709,
    /// SMPTE ST 2084 (PQ) normalized to its peak. ICC.1:2022 §10.18 defines no closed form for
    /// it, so it is a sampled `curveType` (§10.6).
    Pq,
}

impl Trc {
    /// The curve for an ITU-T H.273 `TransferCharacteristics` code point, or `None` for a code
    /// point this module cannot encode as an ICC tone curve.
    ///
    /// Keyed on the raw code point, not on [`TransferCharacteristics`]: the encodable set and the
    /// set `gamut-color` models are different sets (see the module docs). Code points 1, 6, 14
    /// and 15 are one curve — H.273 Table 3 gives them identical segments and one pair of α/β
    /// constants — so all four map to [`Trc::Bt709`].
    ///
    /// Everything else is declined, including HLG (18) and Unspecified (2), which have no
    /// `parametricCurveType` form and no `gamut-color` EOTF to sample.
    fn for_code_point(transfer: u8) -> Option<Self> {
        match transfer {
            1 | 6 | 14 | 15 => Some(Trc::Bt709),
            8 => Some(Trc::Gamma(1.0)),
            13 => Some(Trc::Srgb),
            16 => Some(Trc::Pq),
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
/// `None` when the primaries cannot be turned into colorants at all: a code point that names no
/// chromaticities ([`ColourPrimaries::Unspecified`], and any later variant `gamut-color` adds
/// without them), or chromaticities degenerate enough to have no RGB→XYZ or adaptation matrix.
/// Declining here is what stops a profile claiming the PCS axes as its primaries, so the guard is
/// structural: no caller has to know which code points are colorimetrically empty.
fn colorants_d50(primaries: ColourPrimaries) -> Option<(Matrix3, Matrix3)> {
    let (rgb, white) = primaries.chromaticities()?;
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

/// The three-component matrix/TRC display profile for a pair of CICP axes (ICC.1:2022 §8.4).
///
/// `cicp` is the `cicpType` element to record the signalling the profile was built from, already
/// normalized for §10.3. `None` when [`colorants_d50`] declines the primaries.
fn rgb_matrix_trc(
    primaries: ColourPrimaries,
    trc: Trc,
    description: &str,
    cicp: Cicp,
) -> Option<IccProfile> {
    let (colorants, chad) = colorants_d50(primaries)?;
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
    Some(IccProfile {
        header: ProfileHeader::new(DeviceClass::Display, ColorSpace::Rgb),
        tags: vec![
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
            (KnownTag::Cicp.into(), TagData::Cicp(cicp)),
        ],
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
        let (primaries, _, trc, description) = space.parts();
        rgb_matrix_trc(primaries, trc, description, space.cicp())
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
    /// of `1e-5` is encodable and useless. Whether the constructor should also refuse a
    /// meaningless gamma is deliberately left open; see
    /// <https://github.com/visualcommons/gamut/issues/589>.
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
    /// those two verbatim, replaces `MatrixCoefficients` with `0`, and passes
    /// `VideoFullRangeFlag` through — because the only value it accepts is `1`.
    ///
    /// `MatrixCoefficients` is zero because ICC.1:2022 §10.3 states that when the data colour
    /// space is RGB or XYZ it *shall* be. No information is lost: the coefficients describe a
    /// luma–chroma encoding the caller de-matrixes before this profile applies, and they stay in
    /// the container signalling (`nclx`, AV1 sequence header) a decoder actually reads them from.
    ///
    /// `VideoFullRangeFlag` is **not** rewritten. §10.3 does not require `1` — it only remarks
    /// that the flag "is often 1" for RGB, and gives RGB examples with it at zero — but this
    /// profile's colorants, `chad` and tone curves are all defined over full-scale RGB, and §10.3's
    /// own `1-1-0-0` example is a narrow range on the **RGB samples themselves** (its
    /// `MatrixCoefficients` is already zero), which no de-matrixing removes. Rewriting the flag
    /// would therefore return a profile that renders the caller's colour wrongly, so a triple that
    /// does not signal full range is declined instead. Scale narrow-range samples to full range and
    /// pass `1`.
    ///
    /// Returns `None` when the profile cannot describe the signalling: a primaries code point
    /// with no chromaticities, whether unmodelled or
    /// [`Unspecified`](ColourPrimaries::Unspecified); a transfer code point with no ICC tone
    /// curve, such as HLG (18) or Unspecified (2); or any `VideoFullRangeFlag` other than `1`.
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
    /// // Narrow range is a scaling of the RGB samples this profile shape does not perform.
    /// let narrow = Cicp { video_full_range_flag: 0, ..signalled };
    /// assert!(IccProfile::from_cicp(narrow).is_none());
    ///
    /// // "Unspecified" primaries name no chromaticities, so no profile can be built.
    /// assert!(IccProfile::from_cicp(Cicp { colour_primaries: 2, ..signalled }).is_none());
    /// ```
    #[must_use]
    pub fn from_cicp(cicp: Cicp) -> Option<Self> {
        if cicp.video_full_range_flag != FULL_RANGE {
            return None;
        }
        let primaries = ColourPrimaries::from_code_point(u16::from(cicp.colour_primaries))?;
        let trc = Trc::for_code_point(cicp.transfer_characteristics)?;
        let named = BuiltinProfile::for_axes(primaries, cicp.transfer_characteristics);
        let description = match named {
            Some(space) => space.parts().3.to_owned(),
            None => format!(
                "CICP {}/{}",
                cicp.colour_primaries, cicp.transfer_characteristics
            ),
        };
        rgb_matrix_trc(primaries, trc, &description, rgb_conforming_cicp(cicp))
    }

    /// A v4 matrix/TRC display profile for a [`SourceProfile`] — `gamut-color`'s
    /// `(gamut, transfer)` bundle.
    ///
    /// Returns `None` for a bundle with no CICP axes: `ADOBE_RGB` and `PROPHOTO_RGB` have neither
    /// a primaries nor a transfer code point, and their chromaticities are private to
    /// `gamut-color`, so this crate cannot describe them without restating tables it does not own.
    ///
    /// # The profile is the code point, not the bundle's own curve
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
    /// That is deliberate, and it is the difference between the two axes. A narrow sample range is
    /// a property of the samples, which is why [`from_cicp`](IccProfile::from_cicp) declines one it
    /// cannot describe. A tone map is not: it is `gamut-color`'s choice about how to *render* an
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
    use gamut_color::transfer::srgb_eotf;
    use lcms2_oracle::tag;

    use super::*;

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
    #[test]
    fn every_bt709_family_code_point_builds_the_same_curve() {
        let want = Trc::Bt709.tag();
        for code in [1_u8, 6, 14, 15] {
            let profile = IccProfile::from_cicp(Cicp {
                colour_primaries: 1,
                transfer_characteristics: code,
                matrix_coefficients: 0,
                video_full_range_flag: 1,
            })
            .unwrap_or_else(|| panic!("H.273 transfer {code} is encodable"));
            assert_eq!(
                profile.get(KnownTag::RedTrc),
                Some(&want),
                "transfer {code}"
            );
        }
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

    /// The four built-in spaces have four distinct sets of axes, and each round-trips through its
    /// `cicpType` value back to itself. A space whose `parts` named the wrong code point would
    /// collide with another or fail to round-trip.
    #[test]
    fn each_builtin_space_round_trips_through_its_cicp_value() {
        for space in BuiltinProfile::ALL {
            let (primaries, transfer, _, _) = space.parts();
            assert_eq!(
                BuiltinProfile::for_axes(primaries, cicp_byte(transfer.code_point())),
                Some(space),
                "{space:?} is the space for its own axes"
            );
            let built = IccProfile::builtin(space);
            assert!(built.is_some(), "{space:?} builds");
            assert_eq!(
                IccProfile::from_cicp(space.cicp()),
                built,
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
            let code = cicp_byte(transfer.code_point());
            assert_eq!(Trc::for_code_point(code), Some(trc), "{space:?}");
        }
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
        // these ITU-T H.273 Table 3 code points: the BT.709 family (1, 6, 14, 15), linear (8),
        // sRGB (13) and PQ (16). Every other byte — reserved, unspecified, the two logarithmic
        // curves, ST 240, the BT.470-6 display gammas, HLG, and everything H.273 has not
        // assigned — has no ICC tone curve here.
        const ENCODABLE: [u8; 7] = [1, 6, 8, 13, 14, 15, 16];

        for code in 0..=u8::MAX {
            if ENCODABLE.contains(&code) {
                continue;
            }
            assert_eq!(
                Trc::for_code_point(code),
                None,
                "transfer code point {code}"
            );
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

    /// Full range is a precondition of `from_cicp`, not a field it normalises: every other
    /// `VideoFullRangeFlag` byte is declined.
    ///
    /// The complement is swept rather than sampled at `0`, because the guard is a comparison and
    /// an ordering mutation of it (`<`, `>`) leaves one side of `1` still admitted. The control at
    /// `1` builds from the same axes, so the axes cannot be what any rejection is about.
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
                "video_full_range_flag {flag}"
            );
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

    /// `SourceProfile`'s bundles map onto the built-in spaces, and the two with no CICP axes are
    /// declined rather than approximated.
    ///
    /// `BT2020` maps onto [`BuiltinProfile::Bt2020Pq`] even though the bundle's own `eotf` is a
    /// tone map and the profile's `rTRC` is not: the mapping is on the **code points** the bundle
    /// projects onto, which is what `from_source_profile` is defined over. The divergence that
    /// follows is documented on the constructor and measured there (up to 0.735 absolute).
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
            let want = expected.and_then(IccProfile::builtin);
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
            let (colorants, _) = colorants_d50(primaries).expect("a modelled code point");
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
                &IccProfile::builtin(space)
                    .expect("a modelled space")
                    .to_bytes()
                    .expect("serializes"),
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
                .expect("sRGB is buildable")
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

    /// lcms2 re-opens the BT.709 profile from our serialized bytes and evaluates its `rTRC` to
    /// the light H.273 Table 3's forward function started from. An independent CMM reading an
    /// independent transcription of the spec: this is what says the `parametricCurveType` we
    /// *wrote* — function type, parameter order and `s15Fixed16` encoding — is the curve, not
    /// just that our own evaluator agrees with itself.
    #[test]
    fn oracle_bt709_tone_curve_matches_the_h273_transfer() {
        let bytes = IccProfile::from_cicp(Cicp {
            colour_primaries: 1,
            transfer_characteristics: 1,
            matrix_coefficients: 0,
            video_full_range_flag: 1,
        })
        .expect("BT.709 signalling builds")
        .to_bytes()
        .expect("serializes");
        let opened = lcms2_oracle::Profile::from_bytes(&bytes).expect("lcms2 re-opens the profile");

        for step in 0..=20 {
            let light = f64::from(step) / 20.0;
            let signal = h273_bt709_oetf(light);
            let got = opened
                .eval_tone_curve(tag::RED_TRC, signal as f32)
                .expect("rTRC present");
            assert!(
                (f64::from(got) - light).abs() < 1.0e-4,
                "BT.709 rTRC at V = {signal}: {got} vs Lc = {light}"
            );
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
