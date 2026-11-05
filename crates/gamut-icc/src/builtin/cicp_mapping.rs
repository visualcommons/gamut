//! The CICP mapping: from an ITU-T H.273 signalling triple (or a `gamut-color`
//! [`SourceProfile`]) to the matrix/TRC profile that describes it — which of the four fields
//! shape the profile, which are rewritten for ICC.1:2022 §10.3, and which signalling is declined.
//! See the parent module's documentation for the reasoning behind each rule.

use gamut_color::SourceProfile;
use gamut_color::cicp::{ColorPrimaries, TransferCharacteristics};
use gamut_core::Cicp;

use super::BuiltinProfile;
use super::synthesis::{Trc, rgb_matrix_trc};
use crate::profile::IccProfile;

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
/// use gamut_core::Cicp;
/// use gamut_icc::{CicpTransferReading, IccProfile};
///
/// let bt709 = Cicp { colour_primaries: 1, transfer_characteristics: 1,
///                    matrix_coefficients: 0, full_range: true };
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

/// ITU-T H.273 (07/2024) §8.3: the `MatrixCoefficients` code points for which
/// `VideoFullRangeFlag` scales Y, Cb and Cr (equations 30 to 32) rather than R, G and B — so a
/// narrow range under them leaves de-matrixed RGB full scale. Transcribed from the clause's own
/// list; every other code point is an RGB-range one (0, 8, 16, 17), unknown (2) or reserved.
pub(super) const LUMA_CHROMA_MATRICES: [u16; 12] = [1, 4, 5, 6, 7, 9, 10, 11, 12, 13, 14, 15];

/// Whether the RGB samples a profile built from `cicp` applies to are full range — full range
/// signalled outright, or narrow range on luma–chroma signals the caller de-matrixes first.
pub(super) fn rgb_is_full_range(cicp: Cicp) -> bool {
    cicp.full_range || LUMA_CHROMA_MATRICES.contains(&cicp.matrix_coefficients)
}

/// The `cicpType` value for a pair of CICP axes.
pub(super) fn cicp_of(primaries: ColorPrimaries, transfer: TransferCharacteristics) -> Cicp {
    rgb_conforming_cicp(Cicp {
        colour_primaries: primaries.code_point(),
        transfer_characteristics: transfer.code_point(),
        // Both set by `rgb_conforming_cicp`, which owns the §10.3 and range rules for every
        // constructor.
        matrix_coefficients: 0,
        full_range: true,
    })
}

/// `cicp` as it describes the RGB the profile applies to: `MatrixCoefficients` replaced by the
/// zero ICC.1:2022 §10.3 **requires** of an RGB or XYZ profile, and `VideoFullRangeFlag` set,
/// because every colorant, `chad` and tone curve written here is defined over full-scale RGB.
///
/// The flag is written, not passed through, because once the coefficients are `0` a narrow-range
/// flag would read as narrow-range *RGB* — which is not what a caller's narrow-range luma–chroma
/// signalling decodes to. Only triples [`rgb_is_full_range`] admits reach here, so the value
/// written is always the range of the samples the colorants and curves are defined over. The two
/// axes the profile is built from pass through untouched.
pub(super) fn rgb_conforming_cicp(cicp: Cicp) -> Cicp {
    Cicp {
        matrix_coefficients: 0,
        full_range: true,
        ..cicp
    }
}

impl IccProfile {
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
    /// [`Unspecified`](ColorPrimaries::Unspecified); a transfer code point with no ICC tone
    /// curve, such as HLG (18) or Unspecified (2); or narrow range under coefficients that are
    /// not luma–chroma ones.
    ///
    /// # Examples
    ///
    /// ```
    /// use gamut_core::Cicp;
    /// use gamut_icc::{BuiltinProfile, IccProfile};
    ///
    /// // BT.709 primaries + sRGB transfer is sRGB, and is built as such.
    /// let signalled = Cicp { colour_primaries: 1, transfer_characteristics: 13,
    ///                        matrix_coefficients: 0, full_range: true };
    /// assert_eq!(IccProfile::from_cicp(signalled), IccProfile::builtin(BuiltinProfile::Srgb));
    ///
    /// // The BT.601 matrix an AVIF `nclx` box may carry does not change the profile, and is not
    /// // written into it.
    /// let matrixed = Cicp { matrix_coefficients: 6, ..signalled };
    /// assert_eq!(IccProfile::from_cicp(matrixed), IccProfile::from_cicp(signalled));
    ///
    /// // Narrow range on Y/Cb/Cr de-matrixes to full-scale RGB: the same profile …
    /// let narrow_ycbcr = Cicp { full_range: false, ..matrixed };
    /// assert_eq!(IccProfile::from_cicp(narrow_ycbcr), IccProfile::from_cicp(signalled));
    ///
    /// // … but narrow range on the RGB samples themselves is a scaling this profile does not
    /// // perform.
    /// let narrow_rgb = Cicp { full_range: false, ..signalled };
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
    /// use gamut_core::Cicp;
    /// use gamut_icc::{CicpTransferReading, IccProfile, KnownTag, TagData};
    ///
    /// let bt709 = Cicp { colour_primaries: 1, transfer_characteristics: 1,
    ///                    matrix_coefficients: 0, full_range: true };
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
        let primaries = ColorPrimaries::from_code_point(cicp.colour_primaries)?;
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
        match (source.color_primaries(), source.transfer_characteristics()) {
            (Some(primaries), Some(transfer)) => Self::from_cicp(cicp_of(primaries, transfer)),
            _ => BuiltinProfile::for_uncoded_source(source).and_then(Self::builtin),
        }
    }
}
