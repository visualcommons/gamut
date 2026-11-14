//! IFD 0's profile tags — the [`CameraProfile`] calibration and the `DNGVersion` — and the typed
//! value readers (bytes, ASCII, rationals, matrices, illuminants) the rest of the decoder shares.

use gamut_core::{Error, Result};
use gamut_ifd::Value;

use super::TrackedIfd;
use crate::profile::CameraProfile;
use crate::tags;
use crate::values::{CalibrationIlluminant, ProfileEmbedPolicy};

/// Reconstructs the [`CameraProfile`] from IFD 0's identity and calibration tags, or `None` when
/// the file carries no colour calibration at all.
///
/// `ColorMatrix1` is the tag that decides whether a colour profile exists: a monochrome camera has
/// no colour to calibrate and omits the whole calibration family (a Leica M Monochrom writes a DNG
/// 1.0.0.0 file carrying only `UniqueCameraModel`). Such a file decodes to a raw image with no
/// profile rather than failing — but a calibration that is *present and malformed* is still an
/// error, because that is a broken file rather than an absent feature.
pub(super) fn decode_profile(ifd0: &TrackedIfd) -> Result<Option<CameraProfile>> {
    // Absent colour calibration is a property of the camera, not a defect. The as-shot white
    // balance may arrive either way — `AsShotNeutral` (50728) or the spec's alternative
    // `AsShotWhiteXY` (50729) — so a file carrying only the chromaticity still yields a profile.
    let white_xy = ifd0.get(tags::AS_SHOT_WHITE_XY).is_some();
    if ifd0.get(tags::COLOR_MATRIX1).is_none()
        || ifd0.get(tags::CALIBRATION_ILLUMINANT1).is_none()
        || (ifd0.get(tags::AS_SHOT_NEUTRAL).is_none() && !white_xy)
    {
        return Ok(None);
    }
    let model = ascii_value(ifd0.get(tags::UNIQUE_CAMERA_MODEL)).ok_or_else(|| {
        Error::invalid_input(env!("CARGO_PKG_NAME"), "DNG: missing UniqueCameraModel")
    })?;
    let color_matrix1 = matrix9(ifd0, tags::COLOR_MATRIX1)?;
    let illuminant1 = illuminant(ifd0, tags::CALIBRATION_ILLUMINANT1).ok_or_else(|| {
        Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "DNG: missing CalibrationIlluminant1",
        )
    })?;
    // The spec makes the two white-balance tags mutually exclusive. A file carrying both is
    // malformed; `AsShotNeutral` wins there, matching the reference implementation, and the
    // chromaticity is dropped rather than left to contradict it.
    let stored_xy = match ifd0.get(tags::AS_SHOT_WHITE_XY) {
        Some(value) if ifd0.get(tags::AS_SHOT_NEUTRAL).is_none() => Some(
            f64_vec(Some(value))
                .filter(|v| v.len() == 2)
                .map(|v| [v[0], v[1]])
                .ok_or_else(|| {
                    Error::invalid_input(env!("CARGO_PKG_NAME"), "DNG: malformed AsShotWhiteXY")
                })?,
        ),
        _ => None,
    };
    let neutral = match ifd0.get(tags::AS_SHOT_NEUTRAL) {
        Some(value) => f64_vec(Some(value))
            .filter(|v| v.len() == 3)
            .map(|v| [v[0], v[1], v[2]])
            .ok_or_else(|| {
                Error::invalid_input(env!("CARGO_PKG_NAME"), "DNG: malformed AsShotNeutral")
            })?,
        // Replaced below by the DNG 1.7.1 §6 derivation, once the calibration it reads is in
        // place. Never observable: `stored_xy` is `Some` exactly when this arm runs.
        None => [1.0, 1.0, 1.0],
    };

    let mut profile = CameraProfile::new(model, color_matrix1, illuminant1, neutral)?;

    if let (Ok(matrix2), Some(illuminant2)) = (
        matrix9(ifd0, tags::COLOR_MATRIX2),
        illuminant(ifd0, tags::CALIBRATION_ILLUMINANT2),
    ) {
        profile = profile.with_second_illuminant(matrix2, illuminant2);
    }
    if let Ok(cc1) = matrix9(ifd0, tags::CAMERA_CALIBRATION1) {
        profile =
            profile.with_camera_calibration(cc1, matrix9(ifd0, tags::CAMERA_CALIBRATION2).ok());
    }
    if let Ok(fm1) = matrix9(ifd0, tags::FORWARD_MATRIX1) {
        profile = profile.with_forward_matrices(fm1, matrix9(ifd0, tags::FORWARD_MATRIX2).ok());
    }
    if let Some(ab) = f64_vec(ifd0.get(tags::ANALOG_BALANCE)).filter(|v| v.len() == 3) {
        profile = profile.with_analog_balance([ab[0], ab[1], ab[2]]);
    }
    if let Some(be) = f64_vec(ifd0.get(tags::BASELINE_EXPOSURE)).and_then(|v| v.first().copied()) {
        profile = profile.with_baseline_exposure(be);
    }
    if let Some(name) = ascii_value(ifd0.get(tags::PROFILE_NAME)) {
        profile = profile.with_profile_name(name);
    }
    if let Some(policy) = ifd0
        .get_u32(tags::PROFILE_EMBED_POLICY)
        .and_then(|c| ProfileEmbedPolicy::try_from(c).ok())
    {
        profile = profile.with_profile_embed_policy(policy);
    }
    // Last, so the §6 conversion sees the whole calibration it interpolates over.
    if let Some(xy) = stored_xy {
        profile = profile.with_as_shot_white_xy(xy)?;
    }
    Ok(Some(profile))
}

/// Reads `DNGVersion` as a 4-byte array (defaulting trailing bytes to zero).
pub(super) fn read_version(ifd0: &TrackedIfd) -> Result<[u8; 4]> {
    let bytes = bytes_value(ifd0.get(tags::DNG_VERSION))
        .ok_or_else(|| Error::invalid_input(env!("CARGO_PKG_NAME"), "DNG: missing DNGVersion"))?;
    let mut version = [0u8; 4];
    for (slot, b) in version.iter_mut().zip(bytes) {
        *slot = b;
    }
    Ok(version)
}

/// Extracts a `BYTE`/`UNDEFINED` value's bytes (or a `SHORT` array narrowed to bytes, used for
/// SHORT-typed `CFAPattern` variants — a DNG-specific reading `Value::as_bytes` rightly refuses).
pub(super) fn bytes_value(value: Option<&Value>) -> Option<Vec<u8>> {
    let value = value?;
    if let Some(b) = value.as_bytes() {
        return Some(b.to_vec());
    }
    match value {
        Value::Short(v) => Some(v.iter().map(|&x| x as u8).collect()),
        _ => None,
    }
}

/// Extracts an `ASCII` (or Exif 3.0 `UTF8`) string value.
pub(super) fn ascii_value(value: Option<&Value>) -> Option<String> {
    value?.as_str().map(ToOwned::to_owned)
}

/// Divides a rational's parts, mapping a zero denominator to `0.0` (the numeric policy is this
/// codec's, not the container's).
pub(super) fn ratio(n: f64, d: f64) -> f64 {
    if d == 0.0 { 0.0 } else { n / d }
}

/// Converts a `RATIONAL`/`SRATIONAL` value to `f64`s.
pub(crate) fn f64_vec(value: Option<&Value>) -> Option<Vec<f64>> {
    let value = value?;
    if let Some(r) = value.as_rationals() {
        return Some(r.iter().map(|&(n, d)| ratio(n.into(), d.into())).collect());
    }
    value
        .as_srationals()
        .map(|r| r.iter().map(|&(n, d)| ratio(n.into(), d.into())).collect())
}

/// Converts an unsigned numeric value — `SHORT`, `LONG`, or `RATIONAL`, the three types the
/// `BlackLevel` tag allows (DNG 1.7.1 p. 28) — to `f64`s.
pub(super) fn unsigned_f64s(value: &Value) -> Option<Vec<f64>> {
    match value {
        Value::Short(v) => Some(v.iter().map(|&x| f64::from(x)).collect()),
        Value::Long(v) => Some(v.iter().map(|&x| f64::from(x)).collect()),
        Value::Rational(r) => Some(r.iter().map(|&(n, d)| ratio(n.into(), d.into())).collect()),
        _ => None,
    }
}

/// Reads a 9-element `(S)RATIONAL` matrix tag as `[f64; 9]`.
fn matrix9(ifd: &TrackedIfd, tag: u16) -> Result<[f64; 9]> {
    let v = f64_vec(ifd.get(tag))
        .filter(|v| v.len() == 9)
        .ok_or_else(|| {
            Error::invalid_input(env!("CARGO_PKG_NAME"), "DNG: expected a 3x3 matrix tag")
        })?;
    let mut m = [0.0; 9];
    m.copy_from_slice(&v);
    Ok(m)
}

/// Reads a `CalibrationIlluminant` tag.
fn illuminant(ifd: &TrackedIfd, tag: u16) -> Option<CalibrationIlluminant> {
    ifd.get_u32(tag)
        .and_then(|c| u16::try_from(c).ok())
        .and_then(|c| CalibrationIlluminant::try_from(c).ok())
}

#[cfg(test)]
mod tests {
    use gamut_ifd::Ifd;

    use super::*;

    /// All three types `BlackLevel` allows, and nothing else.
    ///
    /// Only `SHORT` and `RATIONAL` were reached by fixtures, so deleting the `LONG` arm dropped it
    /// to `_ => None` and the suite stayed green (#110) -- a black level stored as `LONG`, which
    /// the spec permits, would have been refused as a bad type.
    #[test]
    fn unsigned_f64s_accepts_short_long_and_rational() {
        assert_eq!(unsigned_f64s(&Value::Short(vec![7])), Some(vec![7.0]));
        assert_eq!(
            unsigned_f64s(&Value::Long(vec![70000])),
            Some(vec![70000.0])
        );
        assert_eq!(
            unsigned_f64s(&Value::Rational(vec![(1, 2)])),
            Some(vec![0.5])
        );
        assert_eq!(unsigned_f64s(&Value::Byte(vec![7])), None);
    }

    /// A colour profile needs all three of `ColorMatrix1`, `CalibrationIlluminant1` and an as-shot
    /// white balance. Any one missing means the camera did not calibrate — a monochrome body, say —
    /// and yields `None` rather than a half-built profile or an error.
    #[test]
    fn a_profile_needs_a_matrix_an_illuminant_and_a_white_balance() {
        let matrix = || {
            Value::SRational(vec![
                (1_000_000, 1_000_000),
                (0, 1_000_000),
                (0, 1_000_000),
                (0, 1_000_000),
                (1_000_000, 1_000_000),
                (0, 1_000_000),
                (0, 1_000_000),
                (0, 1_000_000),
                (1_000_000, 1_000_000),
            ])
        };
        let neutral = || {
            Value::Rational(vec![
                (500_000, 1_000_000),
                (1_000_000, 1_000_000),
                (700_000, 1_000_000),
            ])
        };
        let model = || Value::Ascii("gamut TestCam".into());

        // All three present: a profile.
        let mut ifd = Ifd::new();
        ifd.set(tags::UNIQUE_CAMERA_MODEL, model());
        ifd.set(tags::COLOR_MATRIX1, matrix());
        ifd.set(tags::CALIBRATION_ILLUMINANT1, Value::Short(vec![21]));
        ifd.set(tags::AS_SHOT_NEUTRAL, neutral());
        assert!(
            decode_profile(&TrackedIfd::new(&ifd))
                .expect("a complete calibration decodes")
                .is_some()
        );

        // Each one removed in turn: no profile, and no error.
        for missing in [
            tags::COLOR_MATRIX1,
            tags::CALIBRATION_ILLUMINANT1,
            tags::AS_SHOT_NEUTRAL,
        ] {
            let mut partial = Ifd::new();
            partial.set(tags::UNIQUE_CAMERA_MODEL, model());
            if missing != tags::COLOR_MATRIX1 {
                partial.set(tags::COLOR_MATRIX1, matrix());
            }
            if missing != tags::CALIBRATION_ILLUMINANT1 {
                partial.set(tags::CALIBRATION_ILLUMINANT1, Value::Short(vec![21]));
            }
            if missing != tags::AS_SHOT_NEUTRAL {
                partial.set(tags::AS_SHOT_NEUTRAL, neutral());
            }
            assert!(
                decode_profile(&TrackedIfd::new(&partial))
                    .expect("an absent calibration is not an error")
                    .is_none(),
                "a calibration missing tag {missing} must yield no profile"
            );
        }

        // `AsShotWhiteXY` satisfies the white-balance requirement on its own.
        let mut by_xy = Ifd::new();
        by_xy.set(tags::UNIQUE_CAMERA_MODEL, model());
        by_xy.set(tags::COLOR_MATRIX1, matrix());
        by_xy.set(tags::CALIBRATION_ILLUMINANT1, Value::Short(vec![21]));
        by_xy.set(
            tags::AS_SHOT_WHITE_XY,
            Value::Rational(vec![(312_700, 1_000_000), (329_000, 1_000_000)]),
        );
        let profile = decode_profile(&TrackedIfd::new(&by_xy))
            .expect("a chromaticity is a white balance")
            .expect("a profile");
        assert_eq!(profile.as_shot_white_xy(), Some([0.3127, 0.329]));

        // Both tags is malformed; the neutral wins and the chromaticity is dropped, so a re-encode
        // cannot end up writing a white balance that contradicts the one that was read.
        let mut both = by_xy.clone();
        both.set(tags::AS_SHOT_NEUTRAL, neutral());
        let profile = decode_profile(&TrackedIfd::new(&both))
            .expect("both tags decode")
            .expect("a profile");
        assert_eq!(profile.as_shot_white_xy(), None);
        assert_eq!(profile.as_shot_neutral(), &[0.5, 1.0, 0.7]);
    }

    #[test]
    fn bytes_value_extracts_and_narrows() {
        assert_eq!(
            bytes_value(Some(&Value::Byte(vec![1, 2, 3]))),
            Some(vec![1, 2, 3])
        );
        // A SHORT array is narrowed to bytes (used for SHORT-typed CFAPattern variants).
        assert_eq!(
            bytes_value(Some(&Value::Short(vec![0x12, 0x34]))),
            Some(vec![0x12, 0x34])
        );
        assert_eq!(bytes_value(Some(&Value::Long(vec![1]))), None);
        assert_eq!(bytes_value(None), None);
    }
}
