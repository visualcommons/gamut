//! Dev-only perceptual-metric oracle: **SSIMULACRA 2** scores for 8-bit sRGB images.
//!
//! PSNR and SSIM are mean-squared-error-family metrics, so they credit a flat quantization table by
//! construction and cannot see frequency-dependent visibility (experiment #29's `RESULTS.md`). This
//! crate gives gamut's quality experiments a perceptual metric to rank encoder variants by (issue
//! #972): [`ssimulacra2_srgb8`] scores a distorted image against its source on SSIMULACRA 2's scale,
//! where 100 is identical, about 90 is visually lossless at a normal viewing distance, and lower is
//! worse (it goes negative for very strong distortion).
//!
//! The metric is the pure-Rust `ssimulacra2` crate (rust-av), a port of the reference
//! `tools/ssimulacra2.cc` in libjxl. Its own test holds it within 0.25 of the reference's score on its
//! fixture pair; this crate does not re-derive that agreement. The libjxl reference itself is not
//! linked: the tool and its `tools/gauss_blur.cc` are not part of the installed library, they
//! include libjxl's internal C++ headers (`lib/jxl/enc_xyb.h`, `lib/jxl/image_bundle.h`, ...), and
//! the workspace's one libjxl build (`gamut-jxl-sys`, `links = "jxl"`, so no second build may
//! declare it) publishes only the installed public headers (`DEP_JXL_INCLUDE`).
//!
//! What this crate fixes is the input contract: gamut's interleaved `Rgb8` samples are always
//! labelled sRGB transfer with BT.709 primaries, the colour space every gamut encoder's `Rgb8` input
//! is defined in, so a caller cannot hand the metric mislabelled samples.

use ssimulacra2::{ColorPrimaries, Rgb, TransferCharacteristic, compute_frame_ssimulacra2};

/// Why a pair of images could not be scored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// A buffer's length is not `width * height * 3`.
    Geometry,
    /// SSIMULACRA 2 needs at least 8x8 pixels: its first scale computes 8x8-window statistics.
    TooSmall,
    /// The metric refused the pair for a reason this crate does not distinguish.
    Metric,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Geometry => "buffer length is not width * height * 3",
            Self::TooSmall => "SSIMULACRA 2 needs at least 8x8 pixels",
            Self::Metric => "SSIMULACRA 2 refused the image pair",
        })
    }
}

impl std::error::Error for Error {}

/// Interleaved 8-bit sRGB as the metric's normalised `f32` triples.
fn to_rgb(samples: &[u8], width: usize, height: usize) -> Result<Rgb, Error> {
    if Some(samples.len()) != width.checked_mul(height).and_then(|n| n.checked_mul(3)) {
        return Err(Error::Geometry);
    }
    let data = samples
        .as_chunks::<3>()
        .0
        .iter()
        .map(|px| px.map(|s| f32::from(s) / 255.0))
        .collect();
    Rgb::new(
        data,
        width,
        height,
        TransferCharacteristic::SRGB,
        ColorPrimaries::BT709,
    )
    .map_err(|_| Error::Geometry)
}

/// SSIMULACRA 2 score of `distorted` against `reference`, both interleaved 8-bit sRGB of
/// `width` x `height` pixels. 100 means identical; higher is better.
///
/// # Errors
///
/// [`Error::Geometry`] where a buffer is not `width * height * 3` bytes, [`Error::TooSmall`] below
/// 8x8 pixels, and [`Error::Metric`] where the metric refuses the pair otherwise.
pub fn ssimulacra2_srgb8(
    reference: &[u8],
    distorted: &[u8],
    width: u32,
    height: u32,
) -> Result<f64, Error> {
    let (w, h) = (width as usize, height as usize);
    let (a, b) = (to_rgb(reference, w, h)?, to_rgb(distorted, w, h)?);
    if w < 8 || h < 8 {
        return Err(Error::TooSmall);
    }
    compute_frame_ssimulacra2(a, b).map_err(|_| Error::Metric)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A deterministic 64x48 gradient with some texture, interleaved RGB.
    fn image() -> Vec<u8> {
        (0..64 * 48)
            .flat_map(|i| {
                let (x, y) = (i % 64, i / 64);
                [(x * 4) as u8, (y * 5) as u8, ((x * y) % 251) as u8]
            })
            .collect()
    }

    #[test]
    fn identical_images_score_one_hundred() {
        let a = image();
        let score = ssimulacra2_srgb8(&a, &a, 64, 48).expect("score");
        assert!((score - 100.0).abs() < 1e-6, "score {score}");
    }

    #[test]
    fn stronger_distortion_scores_lower() {
        let a = image();
        let shift = |d: u8| a.iter().map(|&s| s.saturating_add(d)).collect::<Vec<_>>();
        let mild = ssimulacra2_srgb8(&a, &shift(2), 64, 48).expect("mild");
        let strong = ssimulacra2_srgb8(&a, &shift(24), 64, 48).expect("strong");
        assert!(
            100.0 > mild && mild > strong,
            "mild {mild}, strong {strong}"
        );
    }

    #[test]
    fn rejects_a_buffer_that_does_not_match_its_geometry() {
        let a = image();
        assert_eq!(
            ssimulacra2_srgb8(&a, &a[..a.len() - 3], 64, 48),
            Err(Error::Geometry)
        );
    }

    #[test]
    fn rejects_an_image_below_eight_by_eight() {
        let a = vec![128u8; 7 * 8 * 3];
        assert_eq!(ssimulacra2_srgb8(&a, &a, 7, 8), Err(Error::TooSmall));
    }
}
