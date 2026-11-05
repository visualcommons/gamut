//! The built-in constructors' suite: the CICP mapping and the profile synthesis are pinned
//! together, because every observable result of either is a whole profile.

use gamut_color::cicp::ColorPrimaries;
use gamut_color::transfer::{
    BT1886_GAMMA, adobe_rgb_eotf_standard, bt1886_eotf, pq_eotf, prophoto_rgb_eotf_standard,
    srgb_eotf,
};
use gamut_color::{Chromaticities, SourceTransfer};
use gamut_core::Cicp;
use lcms2_oracle::tag;

use super::synthesis::{SAMPLED_TRC_POINTS, Trc, colorants_d50};
use super::*;
use crate::primitives::XyzNumber;
use crate::profile::IccProfile;
use crate::tag_types::TagData;
use crate::tags::KnownTag;

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
        for code in [1_u16, 6, 14, 15] {
            let profile = IccProfile::from_cicp_with(
                Cicp {
                    colour_primaries: 1,
                    transfer_characteristics: code,
                    matrix_coefficients: 0,
                    full_range: true,
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
    const BT709_FAMILY: [u16; 4] = [1, 6, 14, 15];
    let rgb = |transfer| Cicp {
        colour_primaries: 1,
        transfer_characteristics: transfer,
        matrix_coefficients: 0,
        full_range: true,
    };
    for transfer in 0..=255_u16 {
        let literal = IccProfile::from_cicp(rgb(transfer));
        let display = IccProfile::from_cicp_with(rgb(transfer), CicpTransferReading::Bt1886Display);
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
                full_range: true,
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
            BuiltinProfile::for_axes(primaries, transfer.code_point()),
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
            let code = transfer.code_point();
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
    const ENCODABLE: [u16; 9] = [1, 4, 5, 6, 8, 13, 14, 15, 16];

    for code in 0..=255_u16 {
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
        full_range: true,
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
        full_range: true,
    };
    let reference = IccProfile::from_cicp(conforming).expect("the control builds");
    for matrix in [1_u16, 5, 6, 9] {
        let signalled = Cicp {
            matrix_coefficients: matrix,
            ..conforming
        };
        let profile = IccProfile::from_cicp(signalled).expect("signalling with a matrix builds");
        assert_eq!(profile, reference, "matrix {matrix}");
        assert_eq!(
            profile.get(KnownTag::Cicp),
            Some(&TagData::Cicp(conforming)),
            "matrix {matrix}: the coefficients are not taken from the caller"
        );
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
    const EQUATIONS_30_TO_32: [u16; 12] = [1, 4, 5, 6, 7, 9, 10, 11, 12, 13, 14, 15];
    let full = Cicp {
        colour_primaries: 9,
        transfer_characteristics: 16,
        matrix_coefficients: 0,
        full_range: true,
    };
    let want = IccProfile::from_cicp(full).expect("the full-range control builds");
    for matrix in 0..=255_u16 {
        let got = IccProfile::from_cicp(Cicp {
            matrix_coefficients: matrix,
            full_range: false,
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
fn rgb_cicp(primaries: u16, transfer: u16) -> Option<IccProfile> {
    IccProfile::from_cicp(Cicp {
        colour_primaries: primaries,
        transfer_characteristics: transfer,
        matrix_coefficients: 0,
        full_range: true,
    })
}

/// Every primaries code point `from_cicp` builds, found by asking it rather than restated.
fn buildable_primaries() -> Vec<u16> {
    (0..=255_u16)
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
        let primaries = ColorPrimaries::from_code_point(code).expect("a modelled code point");
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
    Chromaticities {
        primaries: rgb,
        white,
    }: Chromaticities,
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
    for code in [1_u16, 4, 5, 6, 8, 13, 14, 15, 16] {
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
    for code in [1_u16, 6, 14, 15] {
        let profile = IccProfile::from_cicp_with(
            Cicp {
                colour_primaries: 1,
                transfer_characteristics: code,
                matrix_coefficients: 0,
                full_range: true,
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
    let byte = |code: u16| u8::try_from(code).expect("an H.273 code point is a byte");
    for primaries in buildable_primaries() {
        for transfer in [1_u16, 4, 5, 6, 8, 13, 14, 15, 16] {
            let ours = IccProfile::from_cicp(Cicp {
                colour_primaries: primaries,
                transfer_characteristics: transfer,
                matrix_coefficients: 9,
                full_range: true,
            })
            .expect("buildable signalling");
            let lcms = IccProfile::parse(
                &lcms2_oracle::cicp(byte(primaries), byte(transfer), 0, 1).to_bytes(),
            )
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
