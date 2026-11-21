#![cfg(feature = "serde")]

//! Serde contract tests for the feature-gated plain-data surface: enums by variant name, structs by
//! field name, and `Dimensions` validated on the way in.

use core::fmt::Debug;

use gamut_core::convert::{AlphaPolicy, ConvertPolicy, DepthPolicy, LumaPolicy};
use gamut_core::{Cicp, ColorModel, DecodeLimits, Dimensions, ErrorKind, Orientation, PixelFormat};
use serde::Serialize;
use serde::de::DeserializeOwned;

/// Asserts `value` serializes to `json` and that `json` deserializes back to `value`.
fn assert_json<T>(value: T, json: &str)
where
    T: Serialize + DeserializeOwned + PartialEq + Debug,
{
    assert_eq!(serde_json::to_string(&value).unwrap(), json);
    assert_eq!(serde_json::from_str::<T>(json).unwrap(), value);
}

/// Asserts every value in `all` survives a JSON round trip unchanged.
fn assert_round_trips<T>(all: &[T])
where
    T: Serialize + DeserializeOwned + PartialEq + Debug,
{
    for value in all {
        let json = serde_json::to_string(value).unwrap();
        assert_eq!(&serde_json::from_str::<T>(&json).unwrap(), value);
    }
}

#[test]
fn color_models_round_trip_by_variant_name() {
    let all = [
        ColorModel::Gray,
        ColorModel::GrayAlpha,
        ColorModel::Rgb,
        ColorModel::Rgba,
        ColorModel::Cmyk,
        ColorModel::Bilevel,
        ColorModel::Indexed,
    ];

    assert_json(ColorModel::Rgba, "\"Rgba\"");
    assert_round_trips(&all);
    assert!(serde_json::from_str::<ColorModel>("\"FutureModel\"").is_err());
}

#[test]
fn pixel_formats_round_trip_by_variant_name() {
    assert_json(PixelFormat::GrayAlpha16, "\"GrayAlpha16\"");
    assert_round_trips(PixelFormat::ALL);
    assert!(serde_json::from_str::<PixelFormat>("\"Rgb32F\"").is_err());
}

#[test]
fn error_kinds_round_trip_by_variant_name() {
    assert_json(ErrorKind::InvalidInput, "\"InvalidInput\"");
    assert_round_trips(&[
        ErrorKind::InvalidInput,
        ErrorKind::Unsupported,
        ErrorKind::Io,
    ]);
    assert!(serde_json::from_str::<ErrorKind>("\"Timeout\"").is_err());
}

#[test]
fn orientations_round_trip_by_variant_name() {
    // The variant name, not the EXIF value the discriminant carries.
    assert_json(Orientation::Rotate90Cw, "\"Rotate90Cw\"");
    let all: Vec<Orientation> = (1..=8).filter_map(Orientation::from_exif_value).collect();
    assert_eq!(all.len(), 8);
    assert_round_trips(&all);
    assert!(serde_json::from_str::<Orientation>("\"Rotate45\"").is_err());
}

#[test]
fn convert_policy_enums_round_trip_by_variant_name() {
    assert_json(AlphaPolicy::CompositeOver, "\"CompositeOver\"");
    assert_round_trips(&[
        AlphaPolicy::Reject,
        AlphaPolicy::Drop,
        AlphaPolicy::CompositeOver,
    ]);
    assert!(serde_json::from_str::<AlphaPolicy>("\"Premultiply\"").is_err());

    assert_json(DepthPolicy::Rescale, "\"Rescale\"");
    assert_round_trips(&[DepthPolicy::Reject, DepthPolicy::Rescale]);
    assert!(serde_json::from_str::<DepthPolicy>("\"Truncate\"").is_err());

    assert_json(LumaPolicy::Bt2020, "\"Bt2020\"");
    assert_round_trips(&[
        LumaPolicy::Reject,
        LumaPolicy::Bt601,
        LumaPolicy::Bt709,
        LumaPolicy::Bt2020,
    ]);
    assert!(serde_json::from_str::<LumaPolicy>("\"Bt2100\"").is_err());
}

#[test]
fn convert_policy_round_trips_by_field_name() {
    let policy = ConvertPolicy::lossless()
        .with_alpha(AlphaPolicy::CompositeOver)
        .with_depth(DepthPolicy::Rescale)
        .with_luma(LumaPolicy::Bt601)
        .with_background([1, 2, 3])
        .with_threshold(4);
    assert_json(
        policy,
        r#"{"alpha":"CompositeOver","depth":"Rescale","luma":"Bt601","background":[1,2,3],"threshold":4}"#,
    );
}

#[test]
fn cicp_round_trips_by_field_name() {
    let cicp = Cicp {
        colour_primaries: 9,
        transfer_characteristics: 16,
        matrix_coefficients: 9,
        full_range: false,
    };
    assert_json(
        cicp,
        r#"{"colour_primaries":9,"transfer_characteristics":16,"matrix_coefficients":9,"full_range":false}"#,
    );
}

#[test]
fn decode_limits_round_trip_by_field_name() {
    let limits = DecodeLimits::new()
        .with_max_width(640)
        .with_max_height(480)
        .with_max_image_bytes(1 << 20);
    assert_json(
        limits,
        r#"{"max_width":640,"max_height":480,"max_image_bytes":1048576}"#,
    );
}

#[test]
fn dimensions_round_trip_by_field_name() {
    assert_json(Dimensions::new(3, 2).unwrap(), r#"{"width":3,"height":2}"#);
}

#[test]
fn dimensions_deserialize_rejects_a_zero_side() {
    // Deserializing goes through `Dimensions::new`, so the non-zero invariant holds for a value
    // read from untrusted data exactly as for one built in code.
    for json in [r#"{"width":0,"height":2}"#, r#"{"width":3,"height":0}"#] {
        let err = serde_json::from_str::<Dimensions>(json).unwrap_err();
        assert!(err.to_string().contains("zero-sized image"), "{err}");
    }
}
