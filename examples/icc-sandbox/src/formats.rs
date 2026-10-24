//! The *Formats* scenario: one set of pixels, two embedded profiles, two containers.
//!
//! The ICC profile travels in the container, not in a metadata block: PNG carries it in the
//! `iCCP` chunk (zlib-compressed, PNG 3rd edition §11.3.2.3), JPEG in APP2 `ICC_PROFILE`
//! segments (ICC.1 Annex B.4). [`encode`] writes the chart through `gamut-png`'s or
//! `gamut-jpeg`'s encoder with a profile attached; [`decode`] reads the pixels back with the same
//! crate's decoder and pulls the profile out with its metadata reader. The profile bytes come out
//! exactly as they went in — the containers never touch them.

use gamut_core::{DecodeImage, Dimensions, EncodeImage, ImageRef, Rgb8};
use gamut_jpeg::{JpegDecoder, JpegEncoder};
use gamut_png::{PngDecoder, PngEncoder, Preset};

/// The two containers the scenario compares.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Container {
    /// PNG, profile in `iCCP`.
    Png,
    /// JPEG, profile in APP2 `ICC_PROFILE`.
    Jpeg,
}

impl Container {
    /// Both containers, in display order.
    pub const ALL: [Container; 2] = [Container::Png, Container::Jpeg];

    /// The container's name and where it keeps the profile.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Container::Png => "PNG (iCCP chunk)",
            Container::Jpeg => "JPEG (APP2 ICC_PROFILE)",
        }
    }
}

/// The JPEG quality the scenario encodes at — high, so the colour difference between the two
/// profiles is what the eye sees, not the codec's.
pub const JPEG_QUALITY: u8 = 95;

/// The `iCCP` profile name (PNG requires one; 1–79 Latin-1 characters).
pub const PNG_PROFILE_NAME: &str = "embedded profile";

/// A container file decoded back: its pixels and the profile it carried.
#[derive(Debug, Clone)]
pub struct Decoded {
    /// Interleaved RGB8 samples.
    pub rgb8: Vec<u8>,
    /// The image size.
    pub width: u32,
    /// The image size.
    pub height: u32,
    /// The embedded ICC profile, verbatim, if the file carries one.
    pub icc: Option<Vec<u8>>,
}

/// Encodes interleaved RGB8 `pixels` of `width × height` into `container` with `icc` embedded.
///
/// # Errors
///
/// Whatever the encoder reports (`gamut_core::Error`), e.g. for a buffer that does not match the
/// dimensions.
pub fn encode(
    container: Container,
    pixels: &[u8],
    width: u32,
    height: u32,
    icc: &[u8],
) -> gamut_core::Result<Vec<u8>> {
    let image = ImageRef::<Rgb8>::new(pixels, Dimensions::new(width, height)?)?;
    match container {
        Container::Png => PngEncoder::new()
            .with_preset(Preset::Fast)
            .with_icc_profile(PNG_PROFILE_NAME, icc)
            .encode_to_vec(image),
        Container::Jpeg => JpegEncoder::new()
            .with_quality(JPEG_QUALITY)
            .with_icc_profile(icc)
            .encode_to_vec(image),
    }
}

/// Decodes a `container` file to RGB8 and extracts its embedded profile.
///
/// # Errors
///
/// Whatever the decoder or metadata reader reports for a malformed file.
pub fn decode(container: Container, bytes: &[u8]) -> gamut_core::Result<Decoded> {
    let (image, icc) = match container {
        Container::Png => {
            let image: gamut_core::ImageBuf<Rgb8> = PngDecoder::new().decode_image(bytes)?;
            let icc = gamut_png::metadata(bytes)?.icc_profile.map(|p| p.profile);
            (image, icc)
        }
        Container::Jpeg => {
            let image: gamut_core::ImageBuf<Rgb8> = JpegDecoder::new().decode_image(bytes)?;
            (image, gamut_jpeg::metadata(bytes)?.icc)
        }
    };
    Ok(Decoded {
        width: image.width(),
        height: image.height(),
        rgb8: image.into_samples(),
        icc,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profiles::{Battery, ProfileKind};
    use crate::scene;

    #[test]
    fn both_containers_carry_the_profile_verbatim() {
        let battery = Battery::build().expect("battery builds");
        let icc = &battery.get(ProfileKind::DisplayP3).bytes;
        let pixels = scene::chart_rgb8();
        for container in Container::ALL {
            let file =
                encode(container, &pixels, scene::WIDTH, scene::HEIGHT, icc).expect("encodes");
            let decoded = decode(container, &file).expect("decodes");
            assert_eq!(
                decoded.icc.as_deref(),
                Some(icc.as_slice()),
                "{container:?}"
            );
            assert_eq!(
                (decoded.width, decoded.height),
                (scene::WIDTH, scene::HEIGHT)
            );
        }
    }

    #[test]
    fn png_pixels_survive_exactly() {
        let battery = Battery::build().expect("battery builds");
        let icc = &battery.get(ProfileKind::SrgbV2).bytes;
        let pixels = scene::chart_rgb8();
        let file =
            encode(Container::Png, &pixels, scene::WIDTH, scene::HEIGHT, icc).expect("encodes");
        assert_eq!(decode(Container::Png, &file).expect("decodes").rgb8, pixels);
    }
}
