//! Presenting a decoded image as a caller's pixel layout: the typed [`DecodeImage`] impls on
//! [`PngDecoder`].
//!
//! What stays here is what only PNG knows — palette lookup, folding a tRNS colour key into a real
//! alpha channel, and §13.12 sub-byte scaling ([`presentable`]) — and the layout change itself is
//! delegated to [`gamut_core::convert`], so PNG applies the same rules as every other gamut
//! decoder. The impls perform **lossless widening only** unless
//! [`PngDecoder::with_convert_policy`] permits a lossy one.
//!
//! Split from `decoder` (issue #1094): presentation changes with the workspace's pixel matrix
//! and conversion rules, never with how the chunk stream is parsed or the image data decoded.

use gamut_core::convert::{RawImage, convert_from_raw};
use gamut_core::{
    Bilevel, DecodeImage, Error, Gray8, Gray16, GrayAlpha8, GrayAlpha16, ImageBuf, Indexed8,
    Palette, Pixel, PixelFormat, Result, Rgb8, Rgb16, Rgba8, Rgba16,
};

use crate::color::ColorType;
use crate::decoder::{NativeImage, NativeSamples, PngDecoder, TransparencyKey, dims, lossy};
use crate::pack;

/// The palette's grey values (every entry has `r == g == b`), or `None` if any entry is coloured —
/// the gate for presenting an indexed file as a greyscale layout losslessly.
fn gray_palette(palette: &Palette) -> Option<Vec<u8>> {
    (0..palette.rgb_entries().len())
        .map(|i| {
            let [r, g, b] = palette.rgb(i as u8)?;
            (r == g && g == b).then_some(r)
        })
        .collect()
}

/// A decoded image reduced to one plain interleaved buffer plus the layout tag describing it.
///
/// The boundary between what is PNG's business and what is not. Everything format-specific —
/// palette lookup, folding a tRNS key into a real alpha channel, §13.12 sub-byte scaling — is
/// resolved on this side, so what crosses into [`gamut_core::convert`] is an ordinary grey,
/// grey+alpha, RGB, or RGBA buffer that any format could have produced.
enum Presentable {
    /// 8-bit samples.
    B8(PixelFormat, Vec<u8>),
    /// 16-bit samples.
    B16(PixelFormat, Vec<u16>),
}

/// Reduces a natively decoded image to a [`Presentable`] buffer.
///
/// A 1-bit greyscale image without transparency stays [`PixelFormat::Bilevel`] rather than being
/// scaled to grey, so an [`ImageBuf<Bilevel>`] request is a copy rather than a re-thresholding.
fn presentable(native: NativeImage) -> Result<Presentable> {
    let depth = native.header.bit_depth;
    let key = native.trns_key;
    Ok(match (native.header.color, native.samples) {
        (ColorType::Grayscale, NativeSamples::B8(gray)) => {
            let scale = pack::gray8_scale(depth);
            match gray_key(key) {
                // A colour key turns the image into genuine grey+alpha.
                Some(transparent) => {
                    let mut out = Vec::with_capacity(gray.len() * 2);
                    for &value in &gray {
                        out.push(value * scale);
                        out.push(alpha8(u16::from(value) != transparent));
                    }
                    Presentable::B8(PixelFormat::GrayAlpha8, out)
                }
                // Unscaled 0/1 samples: exactly what Bilevel means.
                None if depth == 1 => Presentable::B8(PixelFormat::Bilevel, gray),
                None => {
                    let mut out = gray;
                    for value in &mut out {
                        *value *= scale;
                    }
                    Presentable::B8(PixelFormat::Gray8, out)
                }
            }
        }
        (ColorType::Grayscale, NativeSamples::B16(gray)) => match gray_key(key) {
            Some(transparent) => {
                let mut out = Vec::with_capacity(gray.len() * 2);
                for &value in &gray {
                    out.push(value);
                    out.push(alpha16(value != transparent));
                }
                Presentable::B16(PixelFormat::GrayAlpha16, out)
            }
            None => Presentable::B16(PixelFormat::Gray16, gray),
        },
        (ColorType::GrayscaleAlpha, NativeSamples::B8(v)) => {
            Presentable::B8(PixelFormat::GrayAlpha8, v)
        }
        (ColorType::GrayscaleAlpha, NativeSamples::B16(v)) => {
            Presentable::B16(PixelFormat::GrayAlpha16, v)
        }
        (ColorType::Truecolor, NativeSamples::B8(rgb)) => match key {
            Some(TransparencyKey::Rgb(kr, kg, kb)) => {
                let mut out = Vec::with_capacity(rgb.len() / 3 * 4);
                for px in rgb.as_chunks::<3>().0 {
                    let opaque =
                        (u16::from(px[0]), u16::from(px[1]), u16::from(px[2])) != (kr, kg, kb);
                    out.extend_from_slice(&[px[0], px[1], px[2], alpha8(opaque)]);
                }
                Presentable::B8(PixelFormat::Rgba8, out)
            }
            _ => Presentable::B8(PixelFormat::Rgb8, rgb),
        },
        (ColorType::Truecolor, NativeSamples::B16(rgb)) => match key {
            Some(TransparencyKey::Rgb(kr, kg, kb)) => {
                let mut out = Vec::with_capacity(rgb.len() / 3 * 4);
                for px in rgb.as_chunks::<3>().0 {
                    let opaque = (px[0], px[1], px[2]) != (kr, kg, kb);
                    out.extend_from_slice(&[px[0], px[1], px[2], alpha16(opaque)]);
                }
                Presentable::B16(PixelFormat::Rgba16, out)
            }
            _ => Presentable::B16(PixelFormat::Rgb16, rgb),
        },
        (ColorType::TruecolorAlpha, NativeSamples::B8(v)) => Presentable::B8(PixelFormat::Rgba8, v),
        (ColorType::TruecolorAlpha, NativeSamples::B16(v)) => {
            Presentable::B16(PixelFormat::Rgba16, v)
        }
        (ColorType::Indexed, NativeSamples::B8(indices)) => {
            // The palette is the image's real colour; indices alone carry none of it.
            let palette = native.palette.ok_or_else(lossy)?;
            let with_alpha = palette.has_transparency();
            // An all-grey palette is genuinely a greyscale image, so it is presented as one: the
            // conversion engine widens grey into colour losslessly, but never the reverse.
            match gray_palette(&palette) {
                Some(grays) if with_alpha => {
                    let mut out = Vec::with_capacity(indices.len() * 2);
                    for &index in &indices {
                        out.push(grays.get(usize::from(index)).copied().unwrap_or_default());
                        out.push(palette.alpha(index).unwrap_or(255));
                    }
                    Presentable::B8(PixelFormat::GrayAlpha8, out)
                }
                Some(grays) => Presentable::B8(
                    PixelFormat::Gray8,
                    indices
                        .iter()
                        .map(|&index| grays.get(usize::from(index)).copied().unwrap_or_default())
                        .collect(),
                ),
                None => {
                    let format = if with_alpha {
                        PixelFormat::Rgba8
                    } else {
                        PixelFormat::Rgb8
                    };
                    Presentable::B8(format, expand_palette(&indices, &palette, with_alpha))
                }
            }
        }
        // Indexed depths are at most 8 (Table 12), so 16-bit indexed samples cannot exist.
        (ColorType::Indexed, NativeSamples::B16(_)) => {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "PNG: bit depth not allowed for the colour type",
            ));
        }
    })
}

// --- Typed presentation ------------------------------------------------------------------------
//
// Every impl funnels through `PngDecoder::present`, which reduces the file to a plain interleaved
// buffer (see `presentable`) and hands the layout change to `gamut_core::convert`. The typed path
// never inflates metadata chunks.

/// Expands validated palette indices to RGB or RGBA bytes. Indices were range-checked during
/// decode, so lookups cannot fail; a defensive default keeps the path panic-free regardless.
fn expand_palette(indices: &[u8], palette: &Palette, with_alpha: bool) -> Vec<u8> {
    let channels = if with_alpha { 4 } else { 3 };
    let mut out = Vec::with_capacity(indices.len() * channels);
    for &index in indices {
        let [red, green, blue] = palette.rgb(index).unwrap_or_default();
        out.push(red);
        out.push(green);
        out.push(blue);
        if with_alpha {
            out.push(palette.alpha(index).unwrap_or(255));
        }
    }
    out
}

impl PngDecoder {
    /// Decodes `data` and presents it as pixel layout `P`.
    ///
    /// [`Indexed8`] is served straight from the decoded indices: palette indices are not a colour
    /// layout, so there is nothing for the conversion engine to do with them (and it refuses them
    /// for exactly that reason). Every other layout goes through the shared engine.
    fn present<P: Pixel>(&self, data: &[u8]) -> Result<ImageBuf<P>> {
        let native = self.decode_native(data)?;
        let dimensions = dims(&native.header)?;
        if P::FORMAT == PixelFormat::Indexed8 {
            let NativeSamples::B8(indices) = native.samples else {
                return Err(lossy());
            };
            if native.header.color != ColorType::Indexed {
                return Err(lossy());
            }
            // `P` is Indexed8, so its sample type is u8 -- but only the value knows that, so the
            // buffer is rebuilt through the engine's identity path rather than transmuted.
            let raw = RawImage::new(&indices, PixelFormat::Indexed8, dimensions)?;
            return convert_from_raw(raw, self.policy);
        }
        match presentable(native)? {
            Presentable::B8(format, samples) => {
                convert_from_raw(RawImage::new(&samples, format, dimensions)?, self.policy)
            }
            Presentable::B16(format, samples) => {
                convert_from_raw(RawImage::new(&samples, format, dimensions)?, self.policy)
            }
        }
    }
}

impl DecodeImage<Bilevel> for PngDecoder {
    /// Accepts 1-bit greyscale without transparency; samples are presented as 0/1.
    fn decode_image(&self, data: &[u8]) -> Result<ImageBuf<Bilevel>> {
        self.present(data)
    }
}

impl DecodeImage<Gray8> for PngDecoder {
    /// Accepts 1/2/4/8-bit greyscale without transparency; sub-byte samples scale exactly to
    /// 8 bits (§13.12).
    fn decode_image(&self, data: &[u8]) -> Result<ImageBuf<Gray8>> {
        self.present(data)
    }
}

impl DecodeImage<GrayAlpha8> for PngDecoder {
    /// Accepts 8-bit grey+alpha, and 1/2/4/8-bit greyscale (opaque, or keyed by tRNS).
    fn decode_image(&self, data: &[u8]) -> Result<ImageBuf<GrayAlpha8>> {
        self.present(data)
    }
}

impl DecodeImage<Rgb8> for PngDecoder {
    /// Accepts 8-bit RGB, 1/2/4/8-bit greyscale, and an opaque palette, none with transparency.
    fn decode_image(&self, data: &[u8]) -> Result<ImageBuf<Rgb8>> {
        self.present(data)
    }
}

impl DecodeImage<Rgba8> for PngDecoder {
    /// Accepts 8-bit RGBA and, by lossless widening, 8-bit RGB / grey+alpha, 1/2/4/8-bit
    /// greyscale, and palette images, honouring a tRNS colour key.
    fn decode_image(&self, data: &[u8]) -> Result<ImageBuf<Rgba8>> {
        self.present(data)
    }
}

impl DecodeImage<Indexed8> for PngDecoder {
    /// Accepts indexed images at any bit depth, returning the bare palette indices (sub-byte
    /// indices are widened to one byte but never scaled — §13.12 rescaling does not apply to
    /// indices). The palette itself is carried by [`PngDecoder::decode`].
    fn decode_image(&self, data: &[u8]) -> Result<ImageBuf<Indexed8>> {
        self.present(data)
    }
}

impl DecodeImage<Gray16> for PngDecoder {
    /// Accepts 16-bit greyscale without transparency, and widens 1/2/4/8-bit greyscale into it.
    fn decode_image(&self, data: &[u8]) -> Result<ImageBuf<Gray16>> {
        self.present(data)
    }
}

impl DecodeImage<GrayAlpha16> for PngDecoder {
    /// Accepts 16-bit grey+alpha, and widens the narrower greyscale layouts into it (honouring a
    /// tRNS key).
    fn decode_image(&self, data: &[u8]) -> Result<ImageBuf<GrayAlpha16>> {
        self.present(data)
    }
}

impl DecodeImage<Rgb16> for PngDecoder {
    /// Accepts 16-bit RGB, and widens greyscale and 8-bit colour into it, without transparency.
    fn decode_image(&self, data: &[u8]) -> Result<ImageBuf<Rgb16>> {
        self.present(data)
    }
}

impl DecodeImage<Rgba16> for PngDecoder {
    /// The widest target: accepts every non-indexed layout, honouring a tRNS colour key.
    fn decode_image(&self, data: &[u8]) -> Result<ImageBuf<Rgba16>> {
        self.present(data)
    }
}

/// The grey colour key, if one applies.
fn gray_key(key: Option<TransparencyKey>) -> Option<u16> {
    match key {
        Some(TransparencyKey::Gray(gray)) => Some(gray),
        _ => None,
    }
}

/// 8-bit alpha: fully opaque or fully transparent.
fn alpha8(opaque: bool) -> u8 {
    if opaque { 255 } else { 0 }
}

/// 16-bit alpha: fully opaque or fully transparent.
fn alpha16(opaque: bool) -> u16 {
    if opaque { u16::MAX } else { 0 }
}

#[cfg(test)]
mod tests {
    use gamut_core::convert::ConvertPolicy;
    use gamut_core::{Dimensions, EncodeImage, ImageRef};

    use super::*;
    use crate::PngEncoder;
    use crate::decoder::tests::{build_gray_png, build_png};

    /// The policy set by `with_convert_policy` must reach the typed decode: a 16-bit file cannot fill
    /// an 8-bit layout under the default, and can under a permissive one. A decoder that dropped
    /// the setter on the floor would refuse both times.
    #[test]
    fn convert_policy_reaches_the_typed_decode() {
        let dims = Dimensions::new(2, 1).unwrap();
        let gray16 = [0u16, 0xFFFF];
        let mut png = Vec::new();
        PngEncoder::new()
            .encode_image(ImageRef::<Gray16>::new(&gray16, dims).unwrap(), &mut png)
            .expect("encode");

        let strict = PngDecoder::new();
        assert_eq!(
            DecodeImage::<Gray8>::decode_image(&strict, &png)
                .unwrap_err()
                .kind(),
            gamut_core::ErrorKind::Unsupported
        );

        let lax = PngDecoder::new().with_convert_policy(ConvertPolicy::permissive());
        let narrowed: ImageBuf<Gray8> = lax.decode_image(&png).expect("permissive decode");
        assert_eq!(narrowed.as_samples(), &[0, 255]);
    }

    #[test]
    fn widens_gray_to_rgb_and_rgba() {
        let src: Vec<u8> = (0..20u8).collect();
        let mut png = Vec::new();
        PngEncoder::new()
            .encode_image(
                ImageRef::<Gray8>::new(&src, Dimensions::new(5, 4).unwrap()).unwrap(),
                &mut png,
            )
            .unwrap();
        let rgb: ImageBuf<Rgb8> = PngDecoder::new().decode_image(&png).unwrap();
        let expected_rgb: Vec<u8> = src.iter().flat_map(|&g| [g; 3]).collect();
        assert_eq!(rgb.as_samples(), expected_rgb);
        let rgba: ImageBuf<Rgba8> = PngDecoder::new().decode_image(&png).unwrap();
        let expected_rgba: Vec<u8> = src.iter().flat_map(|&g| [g, g, g, 255]).collect();
        assert_eq!(rgba.as_samples(), expected_rgba);
    }

    #[test]
    fn refuses_lossy_requests() {
        let src = vec![0u16; 4];
        let mut png = Vec::new();
        PngEncoder::new()
            .encode_image(
                ImageRef::<Gray16>::new(&src, Dimensions::new(2, 2).unwrap()).unwrap(),
                &mut png,
            )
            .unwrap();
        // A 16-bit file cannot decode into 8-bit layouts.
        assert!(DecodeImage::<Gray8>::decode_image(&PngDecoder::new(), &png).is_err());
        assert!(DecodeImage::<Rgba8>::decode_image(&PngDecoder::new(), &png).is_err());
        // But it widens losslessly to 16-bit RGB.
        let rgb: ImageBuf<Rgb16> = PngDecoder::new().decode_image(&png).unwrap();
        assert_eq!(rgb.as_samples(), vec![0u16; 12]);
    }

    #[test]
    fn opaque_indexed_expands_to_rgb() {
        use gamut_core::Indexed8 as Idx;
        let rgb = [[1u8, 2, 3], [4, 5, 6], [7, 8, 9]];
        let palette = Palette::new(&rgb).unwrap();
        let indices = [0u8, 1, 2, 1, 0, 2];
        let mut png = Vec::new();
        PngEncoder::new()
            .encode_indexed8(
                ImageRef::<Idx>::new(&indices, Dimensions::new(3, 2).unwrap()).unwrap(),
                &palette,
                &mut png,
            )
            .unwrap();
        let decoded: ImageBuf<Rgb8> = PngDecoder::new().decode_image(&png).unwrap();
        let expected: Vec<u8> = indices
            .iter()
            .flat_map(|&idx| rgb[usize::from(idx)])
            .collect();
        assert_eq!(decoded.as_samples(), expected);
    }

    #[test]
    fn sub_byte_gray_scales_exactly_in_every_typed_widening() {
        // 4-bit grey 3×2 with samples 1,2,3 / 4,5,15 (scale ×17); packed rows 0x12 0x30 / 0x45 0xF0.
        let png = build_png(3, 2, 4, 0, &[0, 0x12, 0x30, 0, 0x45, 0xF0]);
        let values = [1u8, 2, 3, 4, 5, 15];
        let gray: ImageBuf<Gray8> = PngDecoder::new().decode_image(&png).unwrap();
        let scaled: Vec<u8> = values.iter().map(|&v| v * 17).collect();
        assert_eq!(gray.as_samples(), scaled);
        let rgb: ImageBuf<Rgb8> = PngDecoder::new().decode_image(&png).unwrap();
        let replicated: Vec<u8> = values.iter().flat_map(|&v| [v * 17; 3]).collect();
        assert_eq!(rgb.as_samples(), replicated);
        let ga: ImageBuf<GrayAlpha8> = PngDecoder::new().decode_image(&png).unwrap();
        let opaque: Vec<u8> = values.iter().flat_map(|&v| [v * 17, 255]).collect();
        assert_eq!(ga.as_samples(), opaque);
    }

    #[test]
    fn sub_16_files_widen_by_257_into_every_16bit_layout() {
        // The same 4-bit grey file, presented as each 16-bit layout: exact §13.12 scaling is
        // ×17 to 8 bits then ×257 to 16 (= ×4369 total).
        let png = build_png(3, 2, 4, 0, &[0, 0x12, 0x30, 0, 0x45, 0xF0]);
        let values = [1u16, 2, 3, 4, 5, 15];
        let gray: ImageBuf<Gray16> = PngDecoder::new().decode_image(&png).unwrap();
        assert_eq!(
            gray.as_samples(),
            values.map(|v| v * 4369),
            "Gray16 widening"
        );
        let ga: ImageBuf<GrayAlpha16> = PngDecoder::new().decode_image(&png).unwrap();
        let opaque: Vec<u16> = values.iter().flat_map(|&v| [v * 4369, u16::MAX]).collect();
        assert_eq!(ga.as_samples(), opaque, "GrayAlpha16 widening");
        let rgb: ImageBuf<Rgb16> = PngDecoder::new().decode_image(&png).unwrap();
        let replicated: Vec<u16> = values.iter().flat_map(|&v| [v * 4369; 3]).collect();
        assert_eq!(rgb.as_samples(), replicated, "Rgb16 widening");
        let rgba: ImageBuf<Rgba16> = PngDecoder::new().decode_image(&png).unwrap();
        let widened: Vec<u16> = values
            .iter()
            .flat_map(|&v| [v * 4369, v * 4369, v * 4369, u16::MAX])
            .collect();
        assert_eq!(rgba.as_samples(), widened, "Rgba16 widening");
    }

    #[test]
    fn grey_layouts_accept_only_all_grey_palettes() {
        use gamut_core::Indexed8 as Idx;
        let encode = |palette: &Palette| {
            let indices = [0u8, 1, 0, 1, 1, 0];
            let mut png = Vec::new();
            PngEncoder::new()
                .encode_indexed8(
                    ImageRef::<Idx>::new(&indices, Dimensions::new(3, 2).unwrap()).unwrap(),
                    palette,
                    &mut png,
                )
                .unwrap();
            png
        };

        // An entry with r == g but a different b is coloured: both grey layouts must refuse it.
        let colored = encode(&Palette::new(&[[7, 7, 200], [3, 3, 3]]).unwrap());
        assert!(DecodeImage::<Gray8>::decode_image(&PngDecoder::new(), &colored).is_err());
        assert!(DecodeImage::<GrayAlpha8>::decode_image(&PngDecoder::new(), &colored).is_err());

        // An all-grey opaque palette resolves to its grey values (plus opaque alpha).
        let grey = encode(&Palette::new(&[[7, 7, 7], [200, 200, 200]]).unwrap());
        let gray: ImageBuf<Gray8> = PngDecoder::new().decode_image(&grey).unwrap();
        assert_eq!(gray.as_samples(), [7, 200, 7, 200, 200, 7]);
        let ga: ImageBuf<GrayAlpha8> = PngDecoder::new().decode_image(&grey).unwrap();
        assert_eq!(
            ga.as_samples(),
            [7, 255, 200, 255, 7, 255, 200, 255, 200, 255, 7, 255]
        );

        // A grey palette with transparency: Gray8 refuses (alpha is lossy), GrayAlpha8 keeps it.
        let keyed = encode(&Palette::with_alpha(&[[7, 7, 7], [200, 200, 200]], &[128]).unwrap());
        assert!(DecodeImage::<Gray8>::decode_image(&PngDecoder::new(), &keyed).is_err());
        let ga: ImageBuf<GrayAlpha8> = PngDecoder::new().decode_image(&keyed).unwrap();
        assert_eq!(
            ga.as_samples(),
            [7, 128, 200, 255, 7, 128, 200, 255, 200, 255, 7, 128]
        );
    }

    #[test]
    fn gray8_colour_key_resolves_in_grayalpha8_and_refusals_hold() {
        // 8-bit grey 3×1 with samples 7, 8, 7 and colour key 7.
        let png = build_gray_png(3, 1, 8, 0, Some(7), &[0, 7, 8, 7]);
        let ga: ImageBuf<GrayAlpha8> = PngDecoder::new().decode_image(&png).unwrap();
        assert_eq!(ga.as_samples(), [7, 0, 8, 255, 7, 0]);
        // Alpha-less layouts refuse the keyed file (dropping transparency is lossy).
        assert!(DecodeImage::<Gray8>::decode_image(&PngDecoder::new(), &png).is_err());
        assert!(DecodeImage::<Rgb8>::decode_image(&PngDecoder::new(), &png).is_err());
        assert!(DecodeImage::<Bilevel>::decode_image(&PngDecoder::new(), &png).is_err());
    }

    #[test]
    fn bilevel_requires_exactly_1bit_keyless_gray() {
        // Depth is the only violation: 8-bit grey must NOT present as Bilevel.
        let gray8 = build_png(2, 1, 8, 0, &[0, 1, 0]);
        assert!(DecodeImage::<Bilevel>::decode_image(&PngDecoder::new(), &gray8).is_err());
        // Transparency is the only violation: keyed 1-bit grey must NOT present as Bilevel...
        let keyed = build_gray_png(2, 1, 1, 0, Some(0), &[0, 0x80]);
        assert!(DecodeImage::<Bilevel>::decode_image(&PngDecoder::new(), &keyed).is_err());
        // ...while the keyless twin decodes to 0/1 samples.
        let plain = build_png(2, 1, 1, 0, &[0, 0x80]);
        let img: ImageBuf<Bilevel> = PngDecoder::new().decode_image(&plain).unwrap();
        assert_eq!(img.as_samples(), [1, 0]);
    }

    #[test]
    fn gray16_colour_key_resolves_in_16bit_widenings() {
        // 16-bit grey 3×1 with samples 7, 8, 7 and colour key 7.
        let png = build_gray_png(3, 1, 16, 0, Some(7), &[0, 0, 7, 0, 8, 0, 7]);
        // The keyed file refuses alpha-less Gray16/Rgb16.
        assert!(DecodeImage::<Gray16>::decode_image(&PngDecoder::new(), &png).is_err());
        assert!(DecodeImage::<Rgb16>::decode_image(&PngDecoder::new(), &png).is_err());
        // GrayAlpha16 and Rgba16 resolve the key to alpha zero, exactly on matching pixels.
        let ga: ImageBuf<GrayAlpha16> = PngDecoder::new().decode_image(&png).unwrap();
        assert_eq!(ga.as_samples(), [7, 0, 8, u16::MAX, 7, 0]);
        let rgba: ImageBuf<Rgba16> = PngDecoder::new().decode_image(&png).unwrap();
        assert_eq!(
            rgba.as_samples(),
            [7, 7, 7, 0, 8, 8, 8, u16::MAX, 7, 7, 7, 0]
        );
        // The keyless twin widens opaque.
        let plain = build_png(2, 1, 16, 0, &[0, 0, 9, 1, 4]);
        let rgba: ImageBuf<Rgba16> = PngDecoder::new().decode_image(&plain).unwrap();
        assert_eq!(
            rgba.as_samples(),
            [9, 9, 9, u16::MAX, 260, 260, 260, u16::MAX]
        );
    }

    #[test]
    fn grayalpha16_widens_to_rgba16() {
        let src: Vec<u16> = vec![100, 200, 300, 400];
        let mut png = Vec::new();
        PngEncoder::new()
            .encode_image(
                ImageRef::<GrayAlpha16>::new(&src, Dimensions::new(2, 1).unwrap()).unwrap(),
                &mut png,
            )
            .unwrap();
        let rgba: ImageBuf<Rgba16> = PngDecoder::new().decode_image(&png).unwrap();
        assert_eq!(rgba.as_samples(), [100, 100, 100, 200, 300, 300, 300, 400]);
    }
}
