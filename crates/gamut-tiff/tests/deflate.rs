//! Adobe Deflate: complete zlib streams per strip/tile, Predictor 2 segment boundaries, legacy tag
//! compatibility, and bidirectional libtiff parity.

use gamut_core::{DecodeImage, Dimensions, EncodeImage, ImageBuf, ImageRef, Rgb8};
use gamut_tiff::{Compression, Predictor, TiffDecoder, TiffEncoder, read, tags};

fn pattern(width: u32, height: u32) -> Vec<u8> {
    let mut pixels = Vec::with_capacity((width * height * 3) as usize);
    for y in 0..height {
        for x in 0..width {
            pixels.extend_from_slice(&[
                (x.wrapping_mul(3).wrapping_add(y)) as u8,
                (y.wrapping_mul(5) ^ x) as u8,
                (x.wrapping_mul(7).wrapping_add(y.wrapping_mul(11))) as u8,
            ]);
        }
    }
    pixels
}

fn gamut_encode(pixels: &[u8], width: u32, height: u32, tiled: bool, predicted: bool) -> Vec<u8> {
    let mut encoder = TiffEncoder::new().with_compression(Compression::Deflate);
    if tiled {
        encoder = encoder.with_tiling(16, 16);
    }
    if predicted {
        encoder = encoder.with_predictor(Predictor::HorizontalDifferencing);
    }
    encoder
        .encode_to_vec(
            ImageRef::<Rgb8>::new(pixels, Dimensions::new(width, height).unwrap()).unwrap(),
        )
        .unwrap()
}

fn assert_gamut_decode(tiff: &[u8], expected: &[u8]) {
    let decoded: ImageBuf<Rgb8> = TiffDecoder::new().decode_image(tiff).unwrap();
    assert_eq!(decoded.as_samples(), expected);
}

#[test]
fn strip_and_tile_round_trips_match_libtiff_with_predictor_on_and_off() {
    // Both dimensions cross tile boundaries and leave padded right/bottom edges. A textured first
    // pixel in each tile row catches any predictor state leaking across a tile boundary.
    let (width, height) = (35, 19);
    let pixels = pattern(width, height);
    for tiled in [false, true] {
        for predicted in [false, true] {
            let tiff = gamut_encode(&pixels, width, height, tiled, predicted);
            assert_eq!(
                read(&tiff).unwrap().ifds[0].get_u32(tags::COMPRESSION),
                Some(8),
                "the encoder must use the standard Adobe tag"
            );
            assert_gamut_decode(&tiff, &pixels);
            let oracle_pixels = if tiled {
                libtiff_oracle::decode_rgba(&tiff)
                    .unwrap()
                    .2
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .flat_map(|pixel| [pixel[0], pixel[1], pixel[2]])
                    .collect()
            } else {
                libtiff_oracle::decode_tiff(&tiff).unwrap().pixels
            };
            assert_eq!(
                oracle_pixels, pixels,
                "libtiff decode tiled={tiled} predicted={predicted}"
            );

            let oracle = match (tiled, predicted) {
                (false, false) => libtiff_oracle::encode_rgb8(
                    &pixels,
                    width,
                    height,
                    libtiff_oracle::Compression::Deflate,
                ),
                (false, true) => libtiff_oracle::encode_rgb8_predictor(
                    &pixels,
                    width,
                    height,
                    libtiff_oracle::Compression::Deflate,
                ),
                (true, false) => libtiff_oracle::encode_rgb8_tiled(
                    &pixels,
                    width,
                    height,
                    16,
                    16,
                    libtiff_oracle::Compression::Deflate,
                ),
                (true, true) => libtiff_oracle::encode_rgb8_tiled_predictor(
                    &pixels,
                    width,
                    height,
                    16,
                    16,
                    libtiff_oracle::Compression::Deflate,
                ),
            }
            .unwrap();
            assert_gamut_decode(&oracle, &pixels);
        }
    }
}

#[test]
fn legacy_deflate_code_is_accepted_but_never_emitted() {
    let (width, height) = (9, 7);
    let pixels = pattern(width, height);
    let mut tiff = gamut_encode(&pixels, width, height, false, false);
    assert_eq!(&tiff[..2], b"II");
    let ifd = u32::from_le_bytes(tiff[4..8].try_into().unwrap()) as usize;
    let entries = u16::from_le_bytes(tiff[ifd..ifd + 2].try_into().unwrap()) as usize;
    let compression = (0..entries)
        .map(|index| ifd + 2 + index * 12)
        .find(|&entry| u16::from_le_bytes(tiff[entry..entry + 2].try_into().unwrap()) == 259)
        .unwrap();
    assert_eq!(
        u16::from_le_bytes(tiff[compression + 8..compression + 10].try_into().unwrap()),
        8
    );
    tiff[compression + 8..compression + 10].copy_from_slice(&32946u16.to_le_bytes());
    assert_gamut_decode(&tiff, &pixels);
}

#[test]
fn corrupt_segment_is_a_typed_error() {
    let pixels = pattern(17, 13);
    let mut tiff = gamut_encode(&pixels, 17, 13, false, false);
    let file = read(&tiff).unwrap();
    let offset = file.ifds[0].get_u32_vec(tags::STRIP_OFFSETS).unwrap()[0] as usize;
    let count = file.ifds[0].get_u32_vec(tags::STRIP_BYTE_COUNTS).unwrap()[0] as usize;
    tiff[offset + count - 1] ^= 0xFF; // corrupt the zlib Adler-32 trailer
    let result: gamut_core::Result<ImageBuf<Rgb8>> = TiffDecoder::new().decode_image(&tiff);
    let error = result.unwrap_err();
    assert_eq!(error.static_message(), Some("corrupt zlib stream"));
    assert!(error.detail().is_some());
}
