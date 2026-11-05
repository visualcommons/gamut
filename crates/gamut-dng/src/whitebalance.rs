//! The DNG 1.7.1 §6 conversion "Translating White Balance xy Coordinates to Camera Neutral
//! Coordinates": turning an `AsShotWhiteXY` (50729) chromaticity into the camera-native neutral
//! that `AsShotNeutral` (50728) would have stored.
//!
//! A file may give its as-shot white balance either way, and the two are mutually exclusive. The xy
//! form is the more general one — it says *what colour the light was* rather than *what the sensor
//! read* — so a reader has to push it back through the camera's calibration to recover the neutral
//! every downstream step wants.
//!
//! The spec defines that push as
//!
//! ```text
//! XYZtoCamera   = AB · CC · CM
//! CameraNeutral = XYZtoCamera · XYZ(xy)      (Y = 1)
//! ```
//!
//! where `CM` and `CC` are the colour and camera-calibration matrices *interpolated for this white
//! point*. That interpolation is the reason the conversion is rendering-pipeline work rather than
//! container parsing: picking the weight means knowing the white point's correlated colour
//! temperature (see [`gamut_color::cct_from_xy`]) and those of the calibration illuminants, then
//! interpolating linearly in **reciprocal** temperature, per "One, Two, or Three Color
//! Calibrations".
//!
//! Three calibrations (`CalibrationIlluminant3`, DNG 1.6.0.0) are not modelled here; a profile
//! carrying them interpolates over its first two, which is what the pre-1.6 rule prescribes.

use gamut_color::cct_from_xy;
use gamut_color::linalg::{mat_mul3, matvec3};
use gamut_color::matrix::xy_to_xyz;
use gamut_core::{Error, Result};

use crate::values::CalibrationIlluminant;

/// The nominal correlated colour temperature of a calibration illuminant, in kelvin.
///
/// These are the values the DNG reference implementation assigns
/// (`dng_camera_profile::IlluminantToTemperature`); the fluorescent grades take the midpoint of the
/// range their EXIF light-source definition spans. `None` means the illuminant carries no
/// temperature the interpolation can weight on — `Unknown`, or `Other`, whose white point lives in
/// the `IlluminantData` spectral tags this crate does not model.
#[must_use]
pub(crate) fn illuminant_temperature(illuminant: CalibrationIlluminant) -> Option<f64> {
    Some(match illuminant {
        CalibrationIlluminant::StandardLightA | CalibrationIlluminant::Tungsten => 2850.0,
        CalibrationIlluminant::IsoStudioTungsten => 3200.0,
        CalibrationIlluminant::D50 => 5000.0,
        CalibrationIlluminant::D55
        | CalibrationIlluminant::Daylight
        | CalibrationIlluminant::FineWeather
        | CalibrationIlluminant::Flash
        | CalibrationIlluminant::StandardLightB => 5500.0,
        CalibrationIlluminant::D65
        | CalibrationIlluminant::StandardLightC
        | CalibrationIlluminant::CloudyWeather => 6500.0,
        CalibrationIlluminant::D75 | CalibrationIlluminant::Shade => 7500.0,
        CalibrationIlluminant::DaylightFluorescent => (5700.0 + 7100.0) * 0.5,
        CalibrationIlluminant::DayWhiteFluorescent => (4600.0 + 5500.0) * 0.5,
        CalibrationIlluminant::CoolWhiteFluorescent | CalibrationIlluminant::Fluorescent => {
            (3800.0 + 4500.0) * 0.5
        }
        CalibrationIlluminant::WhiteFluorescent => (3250.0 + 3800.0) * 0.5,
        CalibrationIlluminant::WarmWhiteFluorescent => (2600.0 + 3250.0) * 0.5,
        CalibrationIlluminant::Unknown | CalibrationIlluminant::Other => return None,
    })
}

/// The weight given to the *first* calibration when the white balance sits at `temperature`.
///
/// `1.0` selects calibration 1 outright, `0.0` calibration 2; in between the two are mixed by
/// linear interpolation in reciprocal temperature. A white point at or beyond either calibration's
/// own temperature takes that calibration whole, which is the spec's "otherwise, use the closest
/// calibration tag set".
#[must_use]
fn first_calibration_weight(temperature: f64, first: f64, second: f64) -> f64 {
    // The reference implementation orders the pair cool-first; a file that does not is still
    // well-defined, because each end clamps to its own calibration.
    let (low, high) = if first <= second {
        (first, second)
    } else {
        (second, first)
    };
    let weight = if temperature <= low {
        1.0
    } else if temperature >= high {
        0.0
    } else {
        (1.0 / temperature - 1.0 / high) / (1.0 / low - 1.0 / high)
    };
    if first <= second {
        weight
    } else {
        1.0 - weight
    }
}

/// Row-major `[f64; 9]` as the `3 × 3` shape the shared linear algebra takes.
fn rows(m: &[f64; 9]) -> [[f64; 3]; 3] {
    [[m[0], m[1], m[2]], [m[3], m[4], m[5]], [m[6], m[7], m[8]]]
}

/// `a·w + b·(1 - w)`, entrywise.
fn blend(a: &[f64; 9], b: &[f64; 9], weight: f64) -> [f64; 9] {
    let mut out = [0.0; 9];
    for (slot, (&x, &y)) in out.iter_mut().zip(a.iter().zip(b.iter())) {
        *slot = x * weight + y * (1.0 - weight);
    }
    out
}

/// The colour-calibration inputs the conversion reads off a profile.
///
/// Grouping them keeps the conversion a pure function of the profile's calibration, so it can be
/// exercised without building an encoder or a file.
pub(crate) struct Calibration<'a> {
    /// `ColorMatrix1` and its illuminant.
    pub color_matrix1: (&'a [f64; 9], CalibrationIlluminant),
    /// `ColorMatrix2` and its illuminant, when the profile is dual-illuminant.
    pub color_matrix2: Option<(&'a [f64; 9], CalibrationIlluminant)>,
    /// `CameraCalibration1` / `CameraCalibration2`; an absent matrix is the identity.
    pub camera_calibration: (Option<&'a [f64; 9]>, Option<&'a [f64; 9]>),
    /// `AnalogBalance`; absent is unit gain on every plane.
    pub analog_balance: Option<&'a [f64; 3]>,
}

/// The `3 × 3` identity, the value the spec gives an absent `CameraCalibration`.
const IDENTITY: [f64; 9] = [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0];

/// Converts an `AsShotWhiteXY` chromaticity into `AsShotNeutral` camera coordinates, per
/// DNG 1.7.1 §6.
///
/// The result is scaled so its largest component is `1.0` — the normalisation real files store, and
/// the one the reference implementation applies — and each component is pinned into `0.001..=1.0`,
/// because a neutral coordinate is used as a divisor and a non-positive one has no meaning.
///
/// # Errors
///
/// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if `xy` is not
/// a usable chromaticity (non-finite, `y <= 0`, or
/// outside the CIE 1960 transform's domain) or if the calibration maps it to a camera response
/// that is not finite and positive, which leaves nothing to normalise by.
pub(crate) fn camera_neutral_from_white_xy(
    calibration: &Calibration<'_>,
    xy: [f64; 2],
) -> Result<[f64; 3]> {
    let [x, y] = xy;
    if !x.is_finite() || !y.is_finite() || y <= 0.0 {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "DNG: AsShotWhiteXY must be a finite chromaticity with y > 0",
        ));
    }

    // Weight the two calibrations for this white point. Anything that leaves the weight undecidable
    // — a single calibration, an illuminant with no nominal temperature, a chromaticity with no
    // CCT — falls back to calibration 1, which is the only set every DNG is required to carry.
    let (matrix1, illuminant1) = calibration.color_matrix1;
    let weight = match calibration.color_matrix2 {
        Some((_, illuminant2)) => {
            match (
                cct_from_xy(xy),
                illuminant_temperature(illuminant1),
                illuminant_temperature(illuminant2),
            ) {
                (Some(t), Some(t1), Some(t2)) if t1 != t2 => first_calibration_weight(t, t1, t2),
                _ => 1.0,
            }
        }
        None => 1.0,
    };

    let color_matrix = match calibration.color_matrix2 {
        Some((matrix2, _)) => blend(matrix1, matrix2, weight),
        None => *matrix1,
    };
    let (cc1, cc2) = calibration.camera_calibration;
    let camera_calibration = match (cc1, cc2) {
        (Some(a), Some(b)) => blend(a, b, weight),
        (Some(a), None) => *a,
        (None, Some(b)) => *b,
        (None, None) => IDENTITY,
    };

    // XYZtoCamera = AB · CC · CM, with AB the diagonal of the analog balance.
    let mut xyz_to_camera = mat_mul3(&rows(&camera_calibration), &rows(&color_matrix));
    if let Some(balance) = calibration.analog_balance {
        for (row, &gain) in xyz_to_camera.iter_mut().zip(balance.iter()) {
            for entry in row.iter_mut() {
                *entry *= gain;
            }
        }
    }

    let neutral = matvec3(&xyz_to_camera, xy_to_xyz(x, y));
    // `f64::max` drops NaN rather than propagating it, so the peak alone cannot report a
    // non-finite component; both conditions are checked.
    let peak = neutral.iter().copied().fold(f64::MIN, f64::max);
    if !neutral.iter().all(|component| component.is_finite()) || peak <= 0.0 {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "DNG: AsShotWhiteXY maps to an unusable camera neutral",
        ));
    }
    let mut out = [0.0; 3];
    for (slot, component) in out.iter_mut().zip(neutral) {
        *slot = (component / peak).clamp(0.001, 1.0);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const IDENTITY_MATRIX: [f64; 9] = IDENTITY;
    /// D65, the chromaticity most cameras' as-shot white lands near.
    const D65: [f64; 2] = [0.3127, 0.3290];

    /// A single-illuminant calibration over `matrix` under `illuminant`.
    fn single(matrix: &[f64; 9], illuminant: CalibrationIlluminant) -> Calibration<'_> {
        Calibration {
            color_matrix1: (matrix, illuminant),
            color_matrix2: None,
            camera_calibration: (None, None),
            analog_balance: None,
        }
    }

    fn assert_close(got: [f64; 3], want: [f64; 3], what: &str) {
        for (g, w) in got.iter().zip(want.iter()) {
            assert!(
                (g - w).abs() < 1e-6,
                "{what}: got {got:?}, expected {want:?}"
            );
        }
    }

    /// With no calibration to apply, the conversion is exactly "expand xy to XYZ at Y = 1, then
    /// normalise" — so the arithmetic is checkable by hand.
    #[test]
    fn an_identity_calibration_yields_the_normalised_chromaticity() {
        let [x, y] = D65;
        let xyz = [x / y, 1.0, (1.0 - x - y) / y];
        let peak = xyz[2]; // Z is the largest component at D65.
        let neutral = camera_neutral_from_white_xy(
            &single(&IDENTITY_MATRIX, CalibrationIlluminant::D65),
            D65,
        )
        .expect("D65 through an identity calibration");
        assert_close(
            neutral,
            [xyz[0] / peak, xyz[1] / peak, xyz[2] / peak],
            "identity calibration",
        );
        assert!(
            (neutral.iter().copied().fold(f64::MIN, f64::max) - 1.0).abs() < 1e-12,
            "the largest component must be exactly 1.0, got {neutral:?}"
        );
    }

    /// `AnalogBalance` is the diagonal of `AB` in `XYZtoCamera = AB · CC · CM`, so it scales each
    /// output plane before the normalisation — doubling a plane must move the answer.
    #[test]
    fn analog_balance_scales_each_plane() {
        let balance = [1.0, 1.0, 4.0];
        let mut calibration = single(&IDENTITY_MATRIX, CalibrationIlluminant::D65);
        calibration.analog_balance = Some(&balance);
        let neutral =
            camera_neutral_from_white_xy(&calibration, D65).expect("balanced identity calibration");
        let plain = camera_neutral_from_white_xy(
            &single(&IDENTITY_MATRIX, CalibrationIlluminant::D65),
            D65,
        )
        .expect("identity calibration");
        // Z was already the peak, so quadrupling it leaves it the peak and shrinks the others 4×.
        assert_close(
            neutral,
            [plain[0] / 4.0, plain[1] / 4.0, plain[2]],
            "analog balance",
        );
    }

    /// `CameraCalibration1` is the `CC` factor, applied between `AB` and `CM`.
    #[test]
    fn camera_calibration_is_applied() {
        let calibration_matrix = [2.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0];
        let mut calibration = single(&IDENTITY_MATRIX, CalibrationIlluminant::D65);
        calibration.camera_calibration = (Some(&calibration_matrix), None);
        let neutral = camera_neutral_from_white_xy(&calibration, D65).expect("calibrated");
        let [x, y] = D65;
        let xyz = [2.0 * x / y, 1.0, (1.0 - x - y) / y];
        let peak = xyz.iter().copied().fold(f64::MIN, f64::max);
        assert_close(
            neutral,
            [xyz[0] / peak, xyz[1] / peak, xyz[2] / peak],
            "camera calibration",
        );
    }

    /// Two calibrations must be blended by the white point's temperature: a white hotter than both
    /// takes the hot one whole, a cooler one takes the cool one whole, and between them the answer
    /// is a genuine mixture rather than either end.
    #[test]
    fn two_calibrations_interpolate_by_temperature() {
        // Scaled identities, so each calibration's contribution is readable straight off the
        // normalised result's relationship to the other's.
        let daylight = IDENTITY_MATRIX;
        let tungsten = [0.5, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0];
        let pair = Calibration {
            color_matrix1: (&daylight, CalibrationIlluminant::D65),
            color_matrix2: Some((&tungsten, CalibrationIlluminant::StandardLightA)),
            camera_calibration: (None, None),
            analog_balance: None,
        };
        let neutral = |xy| camera_neutral_from_white_xy(&pair, xy).expect("dual-illuminant");

        // Hotter than D65's 6500 K: calibration 1 whole.
        let hot = neutral([0.2800, 0.2900]);
        let daylight_only = camera_neutral_from_white_xy(
            &single(&daylight, CalibrationIlluminant::D65),
            [0.2800, 0.2900],
        )
        .expect("daylight alone");
        assert_close(hot, daylight_only, "above both calibrations");

        // Cooler than Standard Light A's 2850 K: calibration 2 whole.
        let cool_xy = [0.5000, 0.4100];
        let cool = neutral(cool_xy);
        let tungsten_only = camera_neutral_from_white_xy(
            &single(&tungsten, CalibrationIlluminant::StandardLightA),
            cool_xy,
        )
        .expect("tungsten alone");
        assert_close(cool, tungsten_only, "below both calibrations");

        // In between, neither end: the mixture has to differ from both.
        let middle_xy = [0.3800, 0.3750];
        let middle = neutral(middle_xy);
        let middle_daylight =
            camera_neutral_from_white_xy(&single(&daylight, CalibrationIlluminant::D65), middle_xy)
                .expect("daylight alone");
        let middle_tungsten = camera_neutral_from_white_xy(
            &single(&tungsten, CalibrationIlluminant::StandardLightA),
            middle_xy,
        )
        .expect("tungsten alone");
        for (end, name) in [(middle_daylight, "daylight"), (middle_tungsten, "tungsten")] {
            assert!(
                middle
                    .iter()
                    .zip(end.iter())
                    .any(|(m, e)| (m - e).abs() > 1e-6),
                "a white between the calibrations must not resolve to {name} alone: {middle:?}"
            );
        }
    }

    /// An illuminant with no nominal temperature cannot weight an interpolation, so the profile's
    /// first calibration — the only one every DNG must carry — is used whole.
    #[test]
    fn an_untemperatured_illuminant_falls_back_to_the_first_calibration() {
        // Not a scalar multiple of calibration 1: the neutral is normalised to a peak of 1.0, so a
        // proportional matrix would be indistinguishable from it and the test would pass vacuously.
        let other = [0.25, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0];
        let pair = Calibration {
            color_matrix1: (&IDENTITY_MATRIX, CalibrationIlluminant::D65),
            color_matrix2: Some((&other, CalibrationIlluminant::Unknown)),
            camera_calibration: (None, None),
            analog_balance: None,
        };
        let blended = camera_neutral_from_white_xy(&pair, D65).expect("untemperatured pair");
        let first = camera_neutral_from_white_xy(
            &single(&IDENTITY_MATRIX, CalibrationIlluminant::D65),
            D65,
        )
        .expect("first alone");
        assert_close(blended, first, "fallback to calibration 1");
    }

    #[test]
    fn unusable_chromaticities_are_rejected() {
        let calibration = single(&IDENTITY_MATRIX, CalibrationIlluminant::D65);
        for xy in [
            [0.3127, 0.0],
            [0.3127, -0.1],
            [f64::NAN, 0.33],
            [0.3127, f64::INFINITY],
        ] {
            let error = camera_neutral_from_white_xy(&calibration, xy)
                .expect_err("an unusable chromaticity must be rejected");
            assert!(
                error.to_string().contains("finite chromaticity"),
                "unexpected error for {xy:?}: {error}"
            );
        }
        // A calibration that annihilates every chromaticity leaves nothing to normalise by.
        let zero = [0.0; 9];
        let error = camera_neutral_from_white_xy(&single(&zero, CalibrationIlluminant::D65), D65)
            .expect_err("a zero matrix must be rejected");
        assert!(
            error.to_string().contains("unusable camera neutral"),
            "unexpected error: {error}"
        );
    }

    /// The weight is the spec's "linear interpolation using inverse correlated color temperature",
    /// clamped to the closest calibration outside the pair's own range — in either tag order.
    #[test]
    fn the_calibration_weight_interpolates_reciprocally_and_clamps() {
        assert_eq!(first_calibration_weight(2000.0, 2850.0, 6500.0), 1.0);
        assert_eq!(first_calibration_weight(9000.0, 2850.0, 6500.0), 0.0);
        // Reversed tag order swaps which end takes which calibration.
        assert_eq!(first_calibration_weight(2000.0, 6500.0, 2850.0), 0.0);
        assert_eq!(first_calibration_weight(9000.0, 6500.0, 2850.0), 1.0);
        // The midpoint in reciprocal temperature is the midpoint of the blend, which is *not* the
        // arithmetic midpoint of the two temperatures — that is the whole point of inverting.
        let mired_midpoint = 2.0 / (1.0 / 2850.0 + 1.0 / 6500.0);
        assert!((first_calibration_weight(mired_midpoint, 2850.0, 6500.0) - 0.5).abs() < 1e-12);
        assert!(
            (first_calibration_weight((2850.0 + 6500.0) / 2.0, 2850.0, 6500.0) - 0.5).abs() > 1e-3
        );
    }

    /// Every illuminant this crate models either carries the reference implementation's nominal
    /// temperature or is explicitly temperature-less. The table is exhaustive because a wrong
    /// entry silently mis-weights the calibration interpolation rather than failing.
    #[test]
    fn illuminant_temperatures_match_the_reference() {
        use CalibrationIlluminant as I;
        for (illuminant, expected) in [
            (I::Unknown, None),
            (I::Daylight, Some(5500.0)),
            (I::Fluorescent, Some(4150.0)),
            (I::Tungsten, Some(2850.0)),
            (I::Flash, Some(5500.0)),
            (I::FineWeather, Some(5500.0)),
            (I::CloudyWeather, Some(6500.0)),
            (I::Shade, Some(7500.0)),
            (I::DaylightFluorescent, Some(6400.0)),
            (I::DayWhiteFluorescent, Some(5050.0)),
            (I::CoolWhiteFluorescent, Some(4150.0)),
            (I::WhiteFluorescent, Some(3525.0)),
            (I::WarmWhiteFluorescent, Some(2925.0)),
            (I::StandardLightA, Some(2850.0)),
            (I::StandardLightB, Some(5500.0)),
            (I::StandardLightC, Some(6500.0)),
            (I::D55, Some(5500.0)),
            (I::D65, Some(6500.0)),
            (I::D75, Some(7500.0)),
            (I::D50, Some(5000.0)),
            (I::IsoStudioTungsten, Some(3200.0)),
            (I::Other, None),
        ] {
            assert_eq!(
                illuminant_temperature(illuminant),
                expected,
                "{illuminant:?} ({})",
                illuminant.code()
            );
        }
    }

    /// Two calibrations at the *same* temperature give the interpolation nothing to weight on, so
    /// the first is used whole rather than blended at some arbitrary fraction. (Standard Light C
    /// and D65 are both nominally 6500 K, so a real profile can hit this.)
    #[test]
    fn calibrations_at_one_temperature_use_the_first() {
        // As above, deliberately not proportional to calibration 1.
        let second = [0.25, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0];
        let pair = Calibration {
            color_matrix1: (&IDENTITY_MATRIX, CalibrationIlluminant::D65),
            color_matrix2: Some((&second, CalibrationIlluminant::StandardLightC)),
            camera_calibration: (None, None),
            analog_balance: None,
        };
        assert_eq!(
            illuminant_temperature(CalibrationIlluminant::D65),
            illuminant_temperature(CalibrationIlluminant::StandardLightC),
            "the fixture must actually put both calibrations at one temperature"
        );
        // Both sides of the shared temperature. The hot one is the case that distinguishes a real
        // fallback from an interpolation over a zero-width range: weighting by `6500..=6500` would
        // clamp a hotter white onto calibration *2*, the opposite of the fallback.
        for (xy, what) in [
            ([0.3800, 0.3750], "cooler than both"),
            ([0.2830, 0.2930], "hotter than both"),
        ] {
            let blended = camera_neutral_from_white_xy(&pair, xy).expect("same-temperature pair");
            let first = camera_neutral_from_white_xy(
                &single(&IDENTITY_MATRIX, CalibrationIlluminant::D65),
                xy,
            )
            .expect("first alone");
            assert_close(blended, first, what);
        }
    }
}
