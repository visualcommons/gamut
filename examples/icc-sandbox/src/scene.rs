//! The pixels every scenario feeds in.
//!
//! One chart — the 24 patches of a colour-checker layout, as sRGB code values — rendered as an
//! interleaved RGB8 image, and the same chart as the **linear raw values a camera records**: the
//! scene's colorimetry pushed back through camera A's characterization matrix, which is what the
//! sensor would have produced if camera A's profile is the true one.

use gamut_color::lab::{D50_XYZ, xyz_to_lab};
use gamut_color::linalg::mat_inv_3x3;
use gamut_color::transfer::srgb_eotf;

use crate::profiles::{CAMERA_A_PRIMARIES, camera_matrix};

/// The chart's patches, row-major (6 × 4), as sRGB 8-bit code values: the commonly published
/// sRGB renderings of the classic 24-patch colour checker.
pub const PATCHES: [[u8; 3]; 24] = [
    [115, 82, 68],
    [194, 150, 130],
    [98, 122, 157],
    [87, 108, 67],
    [133, 128, 177],
    [103, 189, 170],
    [214, 126, 44],
    [80, 91, 166],
    [193, 90, 99],
    [94, 60, 108],
    [157, 188, 64],
    [224, 163, 46],
    [56, 61, 150],
    [70, 148, 73],
    [175, 54, 60],
    [231, 199, 31],
    [187, 86, 149],
    [8, 133, 161],
    [243, 243, 242],
    [200, 200, 200],
    [160, 160, 160],
    [122, 122, 121],
    [85, 85, 85],
    [52, 52, 52],
];

/// Patches per chart row.
pub const COLUMNS: u32 = 6;
/// Chart rows.
pub const ROWS: u32 = 4;
/// Pixels per patch side.
pub const PATCH: u32 = 24;

/// The chart's width in pixels.
pub const WIDTH: u32 = COLUMNS * PATCH;
/// The chart's height in pixels.
pub const HEIGHT: u32 = ROWS * PATCH;

/// The patch index under pixel `(x, y)`.
fn patch_at(x: u32, y: u32) -> usize {
    ((y / PATCH) * COLUMNS + x / PATCH) as usize
}

/// Renders one sample triple per patch into a `WIDTH × HEIGHT` interleaved image.
fn render<T: Copy>(values: &[[T; 3]; 24]) -> Vec<T> {
    let mut out = Vec::with_capacity((WIDTH * HEIGHT * 3) as usize);
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            out.extend_from_slice(&values[patch_at(x, y)]);
        }
    }
    out
}

/// The chart as interleaved RGB8 code values.
#[must_use]
pub fn chart_rgb8() -> Vec<u8> {
    render(&PATCHES)
}

/// One patch's scene colorimetry: the sRGB code values decoded and taken to PCS XYZ (D50).
#[must_use]
pub fn patch_xyz(patch: [u8; 3]) -> [f64; 3] {
    let linear = patch.map(|c| srgb_eotf(f64::from(c) / 255.0));
    crate::panels::linear_srgb_to_xyz_d50(linear)
}

/// One patch's scene colorimetry as PCS Lab (D50).
#[must_use]
pub fn patch_lab(patch: [u8; 3]) -> [f64; 3] {
    xyz_to_lab(patch_xyz(patch), D50_XYZ)
}

/// The exposure the raw capture is taken at: a little under clipping, as a camera meters.
pub const RAW_EXPOSURE: f64 = 0.9;

/// The linear raw values (`[0, 1]`) camera A's sensor records for each patch: the scene XYZ
/// times the inverse of camera A's characterization matrix, at [`RAW_EXPOSURE`], clipped to the
/// sensor's range.
#[must_use]
pub fn raw_patches() -> [[f64; 3]; 24] {
    let to_camera = mat_inv_3x3(&camera_matrix(&CAMERA_A_PRIMARIES)).unwrap_or_default();
    PATCHES.map(|p| {
        let xyz = patch_xyz(p);
        [0, 1, 2].map(|r| {
            let v = to_camera[r][0] * xyz[0] + to_camera[r][1] * xyz[1] + to_camera[r][2] * xyz[2];
            (v * RAW_EXPOSURE).clamp(0.0, 1.0)
        })
    })
}

/// The raw capture as an interleaved 16-bit image, the form a raw decoder hands over.
#[must_use]
pub fn raw_rgb16() -> Vec<u16> {
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "clamped to [0, 1] above"
    )]
    let codes = raw_patches().map(|p| p.map(|v| (v * 65535.0).round() as u16));
    render(&codes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chart_has_one_triple_per_pixel() {
        assert_eq!(chart_rgb8().len(), (WIDTH * HEIGHT * 3) as usize);
        assert_eq!(raw_rgb16().len(), (WIDTH * HEIGHT * 3) as usize);
    }

    #[test]
    fn patch_layout_is_row_major() {
        assert_eq!(patch_at(0, 0), 0);
        assert_eq!(patch_at(PATCH, 0), 1);
        assert_eq!(patch_at(0, PATCH), COLUMNS as usize);
        assert_eq!(patch_at(WIDTH - 1, HEIGHT - 1), 23);
    }

    #[test]
    fn neutral_patches_are_neutral_in_lab() {
        for p in &PATCHES[19..] {
            let lab = patch_lab(*p);
            assert!(lab[1].abs() < 1.0 && lab[2].abs() < 1.0, "{p:?} → {lab:?}");
        }
    }
}
