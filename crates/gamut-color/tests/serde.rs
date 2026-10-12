#![cfg(feature = "serde")]

//! Serde contract tests for the feature-gated colour enums.

use gamut_color::{
    BitDepth, ChromaSubsampling, ColorRange, ColourPrimaries, MatrixCoefficients,
    TransferCharacteristics,
};

fn round_trip<T>(values: &[T])
where
    T: Copy + core::fmt::Debug + PartialEq + serde::Serialize + serde::de::DeserializeOwned,
{
    for &value in values {
        let json = serde_json::to_string(&value).unwrap();
        assert_eq!(serde_json::from_str::<T>(&json).unwrap(), value);
    }
}

#[test]
fn public_enums_round_trip_by_variant_name() {
    round_trip(&[
        MatrixCoefficients::Identity,
        MatrixCoefficients::Bt709,
        MatrixCoefficients::Unspecified,
        MatrixCoefficients::Bt470Bg,
        MatrixCoefficients::Bt601,
        MatrixCoefficients::YCgCo,
        MatrixCoefficients::Bt2020Ncl,
    ]);
    round_trip(&[
        ColourPrimaries::Bt709,
        ColourPrimaries::Unspecified,
        ColourPrimaries::Bt601Pal,
        ColourPrimaries::Smpte170m,
        ColourPrimaries::Bt2020,
        ColourPrimaries::DisplayP3,
    ]);
    round_trip(&[
        TransferCharacteristics::Bt709,
        TransferCharacteristics::Unspecified,
        TransferCharacteristics::Bt601,
        TransferCharacteristics::Linear,
        TransferCharacteristics::Srgb,
        TransferCharacteristics::Bt2020_10,
        TransferCharacteristics::Bt2020_12,
        TransferCharacteristics::Pq,
        TransferCharacteristics::Hlg,
    ]);
    round_trip(&[ColorRange::Limited, ColorRange::Full]);
    round_trip(&[
        BitDepth::Eight,
        BitDepth::Ten,
        BitDepth::Twelve,
        BitDepth::Sixteen,
    ]);
    round_trip(&[
        ChromaSubsampling::Cs444,
        ChromaSubsampling::Cs422,
        ChromaSubsampling::Cs420,
        ChromaSubsampling::Cs400,
    ]);

    assert_eq!(
        serde_json::to_string(&MatrixCoefficients::Bt2020Ncl).unwrap(),
        "\"Bt2020Ncl\""
    );
    assert_eq!(
        serde_json::to_string(&TransferCharacteristics::Bt2020_10).unwrap(),
        "\"Bt2020_10\""
    );
    assert_eq!(
        serde_json::to_string(&BitDepth::Sixteen).unwrap(),
        "\"Sixteen\""
    );
    assert_eq!(
        serde_json::to_string(&ChromaSubsampling::Cs420).unwrap(),
        "\"Cs420\""
    );
}

#[test]
fn unknown_variant_names_are_rejected() {
    assert!(serde_json::from_str::<MatrixCoefficients>("\"FutureMatrix\"").is_err());
    assert!(serde_json::from_str::<ColourPrimaries>("\"FuturePrimaries\"").is_err());
    assert!(serde_json::from_str::<TransferCharacteristics>("\"FutureTransfer\"").is_err());
    assert!(serde_json::from_str::<ColorRange>("\"Broadcast\"").is_err());
    assert!(serde_json::from_str::<BitDepth>("\"Fourteen\"").is_err());
    assert!(serde_json::from_str::<ChromaSubsampling>("\"Cs411\"").is_err());
}
