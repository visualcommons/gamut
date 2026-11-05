//! The camera colour profile an encoder writes into the DNG: the colour-calibration matrices, the
//! calibration illuminant(s), and the as-shot white balance.
//!
//! A minimal profile needs only `ColorMatrix1` + an illuminant + `AsShotNeutral`; the `with_*`
//! setters add the optional dual-illuminant matrices, per-camera calibration, forward matrices, and
//! the profile-identity tags. (The third illuminant and the profile look/tone tables remain for a
//! later phase — see `STATUS.md`.)

use gamut_core::{Error, Result};

use crate::values::{CalibrationIlluminant, ProfileEmbedPolicy};
use crate::whitebalance::{Calibration, camera_neutral_from_white_xy};

/// The denominator used when storing a coordinate as a TIFF `RATIONAL`/`SRATIONAL`.
///
/// Six significant digits comfortably exceed the precision DNG colour math needs, and the matrix
/// entries (`|x| < 4`) and white-balance coordinates (`0..1`) stay well inside `i32`/`u32` range.
const RATIONAL_DEN: i32 = 1_000_000;

/// Converts a finite `f64` to a signed `RATIONAL` `(numerator, denominator)` pair.
#[must_use]
pub(crate) fn srational(x: f64) -> (i32, i32) {
    ((x * f64::from(RATIONAL_DEN)).round() as i32, RATIONAL_DEN)
}

/// Converts a non-negative `f64` to an unsigned `RATIONAL` `(numerator, denominator)` pair
/// (negatives clamp to zero).
#[must_use]
pub(crate) fn urational(x: f64) -> (u32, u32) {
    let den = RATIONAL_DEN as u32;
    ((x.max(0.0) * f64::from(den)).round() as u32, den)
}

/// A camera colour profile: how the sensor's native colours relate to CIE XYZ, and the white
/// balance the shot was taken under.
///
/// `color_matrix1` is the row-major `3 × 3` matrix mapping CIE XYZ (under `calibration_illuminant1`)
/// to the camera's native colour space (`ColorMatrix1`). `as_shot_neutral` is the camera-native
/// value of a neutral subject (`AsShotNeutral`). The optional second illuminant, per-camera
/// calibration, forward matrices, analog balance, and identity fields are set via the `with_*`
/// methods.
///
/// A profile may instead record its white balance as a chromaticity — `AsShotWhiteXY` (50729), the
/// spec's mutually-exclusive alternative to `AsShotNeutral` — via
/// [`with_as_shot_white_xy`](Self::with_as_shot_white_xy). The camera-native neutral stays
/// available either way: it is what [`as_shot_neutral`](Self::as_shot_neutral) returns, derived per
/// DNG 1.7.1 §6 when the chromaticity is the stored form.
#[derive(Debug, Clone)]
pub struct CameraProfile {
    unique_camera_model: String,
    color_matrix1: [f64; 9],
    calibration_illuminant1: CalibrationIlluminant,
    as_shot_neutral: [f64; 3],
    color_matrix2: Option<([f64; 9], CalibrationIlluminant)>,
    camera_calibration1: Option<[f64; 9]>,
    camera_calibration2: Option<[f64; 9]>,
    forward_matrix1: Option<[f64; 9]>,
    forward_matrix2: Option<[f64; 9]>,
    analog_balance: Option<[f64; 3]>,
    as_shot_white_xy: Option<[f64; 2]>,
    baseline_exposure: Option<f64>,
    profile_name: Option<String>,
    profile_embed_policy: Option<ProfileEmbedPolicy>,
}

impl CameraProfile {
    /// Creates a profile for a 3-colour (RGB) camera with a single calibration illuminant.
    ///
    /// `color_matrix1` is the row-major `3 × 3` XYZ → camera-native matrix; `as_shot_neutral` is the
    /// 3-component as-shot neutral. `unique_camera_model` must be a non-empty, non-localized model
    /// name (the `UniqueCameraModel` tag raw processors key their calibration on).
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if `unique_camera_model` is empty or any `as_shot_neutral`
    /// component is not strictly positive (a neutral coordinate must be a usable divisor).
    pub fn new(
        unique_camera_model: impl Into<String>,
        color_matrix1: [f64; 9],
        calibration_illuminant1: CalibrationIlluminant,
        as_shot_neutral: [f64; 3],
    ) -> Result<Self> {
        let unique_camera_model = unique_camera_model.into();
        if unique_camera_model.is_empty() {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "DNG: UniqueCameraModel must not be empty",
            ));
        }
        if !as_shot_neutral.iter().all(|&n| n.is_finite() && n > 0.0) {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "DNG: AsShotNeutral components must be positive",
            ));
        }
        Ok(Self {
            unique_camera_model,
            color_matrix1,
            calibration_illuminant1,
            as_shot_neutral,
            color_matrix2: None,
            camera_calibration1: None,
            camera_calibration2: None,
            forward_matrix1: None,
            forward_matrix2: None,
            analog_balance: None,
            as_shot_white_xy: None,
            baseline_exposure: None,
            profile_name: None,
            profile_embed_policy: None,
        })
    }

    /// Adds a second calibration illuminant and its `ColorMatrix2` (dual-illuminant calibration).
    #[must_use]
    pub fn with_second_illuminant(
        mut self,
        color_matrix2: [f64; 9],
        calibration_illuminant2: CalibrationIlluminant,
    ) -> Self {
        self.color_matrix2 = Some((color_matrix2, calibration_illuminant2));
        self
    }

    /// Sets the per-camera `CameraCalibration1` (and optionally `CameraCalibration2`) matrices.
    #[must_use]
    pub fn with_camera_calibration(mut self, cc1: [f64; 9], cc2: Option<[f64; 9]>) -> Self {
        self.camera_calibration1 = Some(cc1);
        self.camera_calibration2 = cc2;
        self
    }

    /// Sets the `ForwardMatrix1` (and optionally `ForwardMatrix2`) white-balanced camera → XYZ(D50)
    /// matrices.
    #[must_use]
    pub fn with_forward_matrices(mut self, fm1: [f64; 9], fm2: Option<[f64; 9]>) -> Self {
        self.forward_matrix1 = Some(fm1);
        self.forward_matrix2 = fm2;
        self
    }

    /// Sets the `AnalogBalance` (per-plane gain applied before the colour matrix).
    #[must_use]
    pub fn with_analog_balance(mut self, analog_balance: [f64; 3]) -> Self {
        self.analog_balance = Some(analog_balance);
        self
    }

    /// Records the as-shot white balance as `AsShotWhiteXY` (50729) — a CIE xy chromaticity —
    /// instead of `AsShotNeutral` (50728), and recomputes the camera-native neutral from it.
    ///
    /// The two tags are mutually exclusive, so a profile carrying a chromaticity writes 50729 and
    /// omits 50728. [`as_shot_neutral`](Self::as_shot_neutral) keeps working: it returns the
    /// coordinates DNG 1.7.1 §6 derives from `xy` through this profile's calibration, replacing the
    /// value passed to [`new`](Self::new).
    ///
    /// That derivation reads the colour matrices, the camera calibration and the analog balance, so
    /// call this **after** the `with_*` setters that supply them; calling it earlier derives the
    /// neutral from an incomplete calibration.
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if `xy` is not a usable chromaticity, or if this profile's
    /// calibration maps it to an all-zero camera response.
    pub fn with_as_shot_white_xy(mut self, xy: [f64; 2]) -> Result<Self> {
        self.as_shot_neutral = camera_neutral_from_white_xy(&self.calibration(), xy)?;
        self.as_shot_white_xy = Some(xy);
        Ok(self)
    }

    /// The colour-calibration inputs the DNG 1.7.1 §6 white-balance conversion reads.
    fn calibration(&self) -> Calibration<'_> {
        Calibration {
            color_matrix1: (&self.color_matrix1, self.calibration_illuminant1),
            color_matrix2: self
                .color_matrix2
                .as_ref()
                .map(|(matrix, illuminant)| (matrix, *illuminant)),
            camera_calibration: (
                self.camera_calibration1.as_ref(),
                self.camera_calibration2.as_ref(),
            ),
            analog_balance: self.analog_balance.as_ref(),
        }
    }

    /// Sets the `BaselineExposure` (default exposure compensation, in stops).
    #[must_use]
    pub fn with_baseline_exposure(mut self, stops: f64) -> Self {
        self.baseline_exposure = Some(stops);
        self
    }

    /// Sets the `ProfileName`.
    #[must_use]
    pub fn with_profile_name(mut self, name: impl Into<String>) -> Self {
        self.profile_name = Some(name.into());
        self
    }

    /// Sets the `ProfileEmbedPolicy`.
    #[must_use]
    pub fn with_profile_embed_policy(mut self, policy: ProfileEmbedPolicy) -> Self {
        self.profile_embed_policy = Some(policy);
        self
    }

    /// The non-localized unique camera model name.
    #[must_use]
    pub fn unique_camera_model(&self) -> &str {
        &self.unique_camera_model
    }

    /// The row-major `3 × 3` XYZ → camera-native colour matrix (`ColorMatrix1`).
    #[must_use]
    pub fn color_matrix1(&self) -> &[f64; 9] {
        &self.color_matrix1
    }

    /// The calibration illuminant for `color_matrix1`.
    #[must_use]
    pub fn calibration_illuminant1(&self) -> CalibrationIlluminant {
        self.calibration_illuminant1
    }

    /// The as-shot neutral (white balance) in camera-native coordinates.
    #[must_use]
    pub fn as_shot_neutral(&self) -> &[f64; 3] {
        &self.as_shot_neutral
    }

    /// The second-illuminant `(ColorMatrix2, CalibrationIlluminant2)`, if set.
    #[must_use]
    pub fn second_illuminant(&self) -> Option<&([f64; 9], CalibrationIlluminant)> {
        self.color_matrix2.as_ref()
    }

    /// The `(CameraCalibration1, CameraCalibration2)` matrices, if set.
    #[must_use]
    pub fn camera_calibration(&self) -> (Option<&[f64; 9]>, Option<&[f64; 9]>) {
        (
            self.camera_calibration1.as_ref(),
            self.camera_calibration2.as_ref(),
        )
    }

    /// The `(ForwardMatrix1, ForwardMatrix2)` matrices, if set.
    #[must_use]
    pub fn forward_matrices(&self) -> (Option<&[f64; 9]>, Option<&[f64; 9]>) {
        (self.forward_matrix1.as_ref(), self.forward_matrix2.as_ref())
    }

    /// The as-shot white balance as a CIE xy chromaticity (`AsShotWhiteXY`), if that is the form
    /// this profile stores. `None` means the white balance is stored as `AsShotNeutral`.
    #[must_use]
    pub fn as_shot_white_xy(&self) -> Option<[f64; 2]> {
        self.as_shot_white_xy
    }

    /// The `AnalogBalance`, if set.
    #[must_use]
    pub fn analog_balance(&self) -> Option<&[f64; 3]> {
        self.analog_balance.as_ref()
    }

    /// The `BaselineExposure` in stops, if set.
    #[must_use]
    pub fn baseline_exposure(&self) -> Option<f64> {
        self.baseline_exposure
    }

    /// The `ProfileName`, if set.
    #[must_use]
    pub fn profile_name(&self) -> Option<&str> {
        self.profile_name.as_deref()
    }

    /// The `ProfileEmbedPolicy`, if set.
    #[must_use]
    pub fn profile_embed_policy(&self) -> Option<ProfileEmbedPolicy> {
        self.profile_embed_policy
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rational_helpers_round_trip_to_double() {
        let (n, d) = srational(-0.5);
        assert!((f64::from(n) / f64::from(d) + 0.5).abs() < 1e-9);
        let (n, d) = urational(0.8);
        assert!((f64::from(n) / f64::from(d) - 0.8).abs() < 1e-9);
        assert_eq!(urational(-1.0), (0, RATIONAL_DEN as u32));
    }

    #[test]
    fn new_validates() {
        let m = [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0];
        assert!(CameraProfile::new("Cam", m, CalibrationIlluminant::D65, [0.5, 1.0, 0.6]).is_ok());
        assert!(CameraProfile::new("", m, CalibrationIlluminant::D65, [0.5, 1.0, 0.6]).is_err());
        assert!(CameraProfile::new("Cam", m, CalibrationIlluminant::D65, [0.0, 1.0, 0.6]).is_err());
    }

    /// Recording the white balance as a chromaticity replaces the neutral `new` was given with
    /// the one DNG 1.7.1 §6 derives, and reports the chromaticity as the stored form.
    #[test]
    fn as_shot_white_xy_replaces_the_neutral() {
        let m = [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0];
        let neutral = [0.5, 1.0, 0.6];
        let plain = CameraProfile::new("Cam", m, CalibrationIlluminant::D65, neutral)
            .expect("valid profile");
        assert_eq!(plain.as_shot_white_xy(), None);
        assert_eq!(plain.as_shot_neutral(), &neutral);

        let xy = [0.3127, 0.3290];
        let by_xy = plain.with_as_shot_white_xy(xy).expect("D65 is convertible");
        assert_eq!(by_xy.as_shot_white_xy(), Some(xy));
        assert_ne!(
            by_xy.as_shot_neutral(),
            &neutral,
            "the derived neutral must replace the one `new` was given"
        );
        // Through an identity calibration the derivation is the normalised chromaticity.
        let [x, y] = xy;
        let z = (1.0 - x - y) / y;
        assert!(
            (by_xy.as_shot_neutral()[2] - 1.0).abs() < 1e-12,
            "Z is the peak at D65"
        );
        assert!((by_xy.as_shot_neutral()[1] - 1.0 / z).abs() < 1e-9);
    }

    #[test]
    fn as_shot_white_xy_rejects_an_unusable_chromaticity() {
        let m = [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0];
        let profile = CameraProfile::new("Cam", m, CalibrationIlluminant::D65, [0.5, 1.0, 0.6])
            .expect("valid profile");
        assert!(profile.with_as_shot_white_xy([0.3127, 0.0]).is_err());
    }

    #[test]
    fn optional_fields_set() {
        let m = [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0];
        let p = CameraProfile::new(
            "Cam",
            m,
            CalibrationIlluminant::StandardLightA,
            [0.5, 1.0, 0.6],
        )
        .unwrap()
        .with_second_illuminant(m, CalibrationIlluminant::D65)
        .with_camera_calibration(m, Some(m))
        .with_forward_matrices(m, None)
        .with_analog_balance([1.0, 1.0, 1.0])
        .with_baseline_exposure(0.25)
        .with_profile_name("gamut Standard")
        .with_profile_embed_policy(ProfileEmbedPolicy::NoRestrictions);
        assert_eq!(p.second_illuminant().unwrap().1, CalibrationIlluminant::D65);
        assert!(p.camera_calibration().1.is_some());
        assert!(p.forward_matrices().1.is_none());
        assert_eq!(p.baseline_exposure(), Some(0.25));
        assert_eq!(p.profile_name(), Some("gamut Standard"));
        assert_eq!(
            p.profile_embed_policy(),
            Some(ProfileEmbedPolicy::NoRestrictions)
        );
    }

    #[test]
    fn getters_return_the_stored_values() {
        let m = [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0];
        // Distinctive, non-`[1, 1, 1]` values so a getter that returned a constant is caught.
        let base =
            CameraProfile::new("Cam", m, CalibrationIlluminant::D65, [0.5, 1.0, 0.6]).unwrap();
        assert_eq!(base.as_shot_neutral(), &[0.5, 1.0, 0.6]);
        assert_eq!(base.analog_balance(), None);

        let balanced = base.with_analog_balance([0.4, 0.5, 0.6]);
        assert_eq!(balanced.analog_balance(), Some(&[0.4, 0.5, 0.6]));
    }
}
