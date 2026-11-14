//! Shared fixtures for the gamut-dng integration tests.
#![allow(dead_code)] // not every test binary uses every fixture

use gamut_core::Dimensions;
use gamut_dng::{CalibrationIlluminant, CameraProfile, ProfileEmbedPolicy, RawImage, cfa_color};

/// A synthetic RGGB Bayer raw image of the given size and bit depth, with a deterministic,
/// spatially-varying mosaic and a full active area.
#[must_use]
pub fn sample_raw(width: u32, height: u32, bits: u16) -> RawImage {
    let pattern = vec![
        cfa_color::RED,
        cfa_color::GREEN,
        cfa_color::GREEN,
        cfa_color::BLUE,
    ];
    let max = u32::from(u16::try_from((1u32 << bits) - 1).unwrap_or(u16::MAX));
    let samples: Vec<u16> = (0..width * height)
        .map(|i| {
            let (x, y) = (i % width, i / width);
            (((x.wrapping_mul(53)) ^ (y.wrapping_mul(97))) % max) as u16
        })
        .collect();
    RawImage::new_cfa(
        Dimensions::new(width, height).unwrap(),
        bits,
        (2, 2),
        pattern,
        samples,
    )
    .expect("valid raw")
    .with_black_level(0.0)
    .expect("valid black level")
    .with_white_level(f64::from(max))
    .expect("valid white level")
    .with_active_area([0, 0, height, width])
    .with_default_crop([0, 0], [width, height])
}

/// A synthetic 3-plane (RGB) demosaiced linear raw image of the given size and bit depth.
#[must_use]
pub fn sample_linear_raw(width: u32, height: u32, bits: u16) -> RawImage {
    let max = u32::from(u16::try_from((1u32 << bits) - 1).unwrap_or(u16::MAX));
    let samples: Vec<u16> = (0..width * height * 3)
        .map(|i| {
            let pixel = i / 3;
            let (x, y, c) = (pixel % width, pixel / width, i % 3);
            ((x.wrapping_mul(7) ^ y.wrapping_mul(13) ^ c.wrapping_mul(101)) % max) as u16
        })
        .collect();
    RawImage::new_linear_raw(Dimensions::new(width, height).unwrap(), bits, 3, samples)
        .expect("valid linear raw")
        .with_white_level(f64::from(max))
        .expect("valid white level")
        .with_active_area([0, 0, height, width])
}

/// A plausible camera colour profile (an illustrative XYZ→camera matrix under D65).
#[must_use]
pub fn sample_profile() -> CameraProfile {
    let color_matrix1 = [
        0.6722, -0.0635, -0.0963, -0.4287, 1.2460, 0.2028, -0.0908, 0.2162, 0.5668,
    ];
    CameraProfile::new(
        "gamut TestCam",
        color_matrix1,
        CalibrationIlluminant::D65,
        [0.5128, 1.0, 0.7059],
    )
    .expect("valid profile")
}

/// A fully-populated profile: dual illuminant, per-camera calibration, forward matrix, analog
/// balance, baseline exposure, and profile identity.
#[must_use]
pub fn sample_profile_full() -> CameraProfile {
    let matrix2 = [0.90, -0.10, -0.05, -0.30, 1.20, 0.10, 0.00, -0.15, 0.80];
    let forward = [0.60, 0.20, 0.16, 0.30, 0.70, 0.00, 0.00, 0.05, 0.78];
    let identity = [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0];
    sample_profile()
        .with_second_illuminant(matrix2, CalibrationIlluminant::StandardLightA)
        .with_camera_calibration(identity, None)
        .with_forward_matrices(forward, None)
        .with_analog_balance([1.0, 1.0, 1.0])
        .with_baseline_exposure(0.5)
        .with_profile_name("gamut Standard")
        .with_profile_embed_policy(ProfileEmbedPolicy::NoRestrictions)
}
