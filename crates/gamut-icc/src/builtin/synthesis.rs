//! Profile synthesis: the ICC encoding every built-in constructor shares — the tone curves a
//! transfer becomes, the D50-adapted colorants and `chad`, and the matrix/TRC tag set (ICC.1:2022
//! §8.4) they are assembled into. Nothing here decides *which* space to build; that is the parent
//! module's and `cicp_mapping`'s.

use gamut_color::Chromaticities;
use gamut_color::linalg::mat_mul3;
use gamut_color::matrix::{bradford_adapt, rgb_to_xyz_matrix};
use gamut_color::transfer::{BT709_ALPHA, BT709_BETA, BT1886_GAMMA, pq_eotf};
use gamut_core::Cicp;

use super::cicp_mapping::CicpTransferReading;
use crate::curve::{Curve, ParametricCurve};
use crate::header::{ColorSpace, DeviceClass, ProfileHeader};
use crate::mluc::{Mluc, MlucRecord};
use crate::primitives::{S15Fixed16, XyzNumber};
use crate::profile::{IccProfile, Tag};
use crate::tag_types::TagData;
use crate::tags::KnownTag;

/// The `copyrightTag` text every built-in profile carries. The profiles encode published
/// colorimetry and carry no rights of their own.
pub(super) const COPYRIGHT: &str = "Public domain — no rights reserved.";

/// Samples in a `curveType` TRC for a transfer with no `parametricCurveType` closed form.
///
/// 1024 points keep the linear-interpolation error of the steepest such curve (ST 2084 near its
/// peak) below one `uInt16` quantum, so the table is as exact as the encoding it is written in.
/// Measured **0.986** quanta at `V ≈ 0.9956` over a 200 001-point sweep, and 0.9854 over the
/// midpoint sweep `sampled_pq_curve_matches_gamut_color` runs — which is why that test's
/// tolerance is one quantum: a guard at twice the claimed bound is not a guard on the claim.
pub(super) const SAMPLED_TRC_POINTS: usize = 1024;

/// A 3×3 matrix in row-major order — the shape `gamut_color::matrix` produces and consumes.
type Matrix3 = [[f64; 3]; 3];

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
pub(super) fn encodable_gamma(gamma: f64) -> Option<S15Fixed16> {
    let raw = (gamma * 65_536.0).round();
    (1.0..=f64::from(i32::MAX))
        .contains(&raw)
        .then(|| S15Fixed16::from_f64(gamma))
}

/// A tone-response curve this module can encode.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum Trc {
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
    /// Keyed on the raw code point, not on [`TransferCharacteristics`](gamut_color::cicp::TransferCharacteristics): the encodable set and the
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
    pub(super) fn for_code_point(transfer: u16, reading: CicpTransferReading) -> Option<Self> {
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
    pub(super) fn tag(self) -> TagData {
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
pub(super) fn pq_samples() -> Vec<u16> {
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
pub(super) fn pcs_d50_chromaticity() -> [f64; 2] {
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
/// A code point that names no chromaticities at all ([`ColorPrimaries::Unspecified`](gamut_color::cicp::ColorPrimaries::Unspecified), and any
/// later variant `gamut-color` adds without them) never reaches here: [`IccProfile::from_cicp`]
/// declines it on the `Option` [`ColorPrimaries::chromaticities`](gamut_color::cicp::ColorPrimaries::chromaticities) returns, which is what stops a
/// profile claiming the PCS axes as its primaries.
pub(super) fn colorants_d50(
    Chromaticities {
        primaries: rgb,
        white,
    }: Chromaticities,
) -> Option<(Matrix3, Matrix3)> {
    let rgb_to_xyz = rgb_to_xyz_matrix(&rgb, white)?;
    let chad = bradford_adapt(white, pcs_d50_chromaticity())?;
    Some((mat_mul3(&chad, &rgb_to_xyz), chad))
}

/// A `multiLocalizedUnicodeType` element carrying one `en-US` record — the v4 form of the
/// description and copyright tags.
pub(super) fn mluc(text: &str) -> TagData {
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
pub(super) fn rgb_matrix_trc(
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
        Tag::new(KnownTag::ProfileDescription, mluc(description)),
        Tag::new(KnownTag::Copyright, mluc(COPYRIGHT)),
        Tag::new(
            KnownTag::MediaWhitePoint,
            TagData::Xyz(vec![XyzNumber::D50]),
        ),
        Tag::new(KnownTag::ChromaticAdaptation, chad_tag),
        Tag::new(KnownTag::RedColorant, column(0)),
        Tag::new(KnownTag::GreenColorant, column(1)),
        Tag::new(KnownTag::BlueColorant, column(2)),
        Tag::new(KnownTag::RedTrc, curve.clone()),
        Tag::new(KnownTag::GreenTrc, curve.clone()),
        Tag::new(KnownTag::BlueTrc, curve),
    ];
    tags.extend(cicp.map(|cicp| Tag::new(KnownTag::Cicp, TagData::Cicp(cicp))));
    Some(IccProfile {
        header: ProfileHeader::new(DeviceClass::Display, ColorSpace::Rgb),
        tags,
    })
}
