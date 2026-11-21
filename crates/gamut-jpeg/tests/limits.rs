//! Public decoder resource-limit contract: exact boundaries, native byte accounting, and hostile
//! frame headers rejected before a frame-sized allocation is possible.

use gamut_core::{DecodeImage, Dimensions, EncodeImage, Gray8, ImageBuf, ImageRef, Rgb8};
use gamut_jpeg::{DecodeLimits, JpegDecoder, JpegEncoder};

fn gray_jpeg(width: u32, height: u32) -> Vec<u8> {
    let pixels = vec![127; (width * height) as usize];
    JpegEncoder::new()
        .encode_to_vec(
            ImageRef::<Gray8>::new(&pixels, Dimensions::new(width, height).unwrap()).unwrap(),
        )
        .unwrap()
}

fn rgb_jpeg(width: u32, height: u32) -> Vec<u8> {
    let pixels = vec![127; (width * height * 3) as usize];
    JpegEncoder::new()
        .encode_to_vec(
            ImageRef::<Rgb8>::new(&pixels, Dimensions::new(width, height).unwrap()).unwrap(),
        )
        .unwrap()
}

fn gray_error(decoder: &JpegDecoder, jpeg: &[u8]) -> String {
    DecodeImage::<Gray8>::decode_image(decoder, jpeg)
        .unwrap_err()
        .static_message()
        .unwrap()
        .to_owned()
}

#[test]
fn dimension_limits_are_exact_and_independent() {
    let jpeg = gray_jpeg(17, 13);
    let exact: ImageBuf<Gray8> = JpegDecoder::new()
        .with_limits(DecodeLimits::new().with_max_width(17).with_max_height(13))
        .decode_image(&jpeg)
        .unwrap();
    assert_eq!(exact.dimensions(), Dimensions::new(17, 13).unwrap());

    assert_eq!(
        gray_error(
            &JpegDecoder::new()
                .with_limits(DecodeLimits::new().with_max_width(16).with_max_height(13)),
            &jpeg
        ),
        "JPEG: image exceeds the dimension limit"
    );
    assert_eq!(
        gray_error(
            &JpegDecoder::new()
                .with_limits(DecodeLimits::new().with_max_width(17).with_max_height(12)),
            &jpeg
        ),
        "JPEG: image exceeds the dimension limit"
    );
}

#[test]
fn byte_limit_uses_the_frame_component_count_at_an_exact_boundary() {
    let gray = gray_jpeg(8, 8);
    assert!(
        DecodeImage::<Gray8>::decode_image(
            &JpegDecoder::new().with_limits(DecodeLimits::new().with_max_image_bytes(8 * 8)),
            &gray
        )
        .is_ok()
    );
    assert_eq!(
        gray_error(
            &JpegDecoder::new().with_limits(DecodeLimits::new().with_max_image_bytes(8 * 8 - 1)),
            &gray
        ),
        "JPEG: image exceeds the size limit"
    );

    // The encoder's default 4:2:0 subsampling does not discount the native three-channel raster.
    let rgb = rgb_jpeg(8, 8);
    assert!(
        DecodeImage::<Rgb8>::decode_image(
            &JpegDecoder::new().with_limits(DecodeLimits::new().with_max_image_bytes(8 * 8 * 3)),
            &rgb
        )
        .is_ok()
    );
    assert_eq!(
        DecodeImage::<Rgb8>::decode_image(
            &JpegDecoder::new()
                .with_limits(DecodeLimits::new().with_max_image_bytes(8 * 8 * 3 - 1)),
            &rgb
        )
        .unwrap_err()
        .static_message()
        .unwrap(),
        "JPEG: image exceeds the size limit"
    );
}

#[test]
fn synthetic_pathological_sof_is_rejected_by_the_cap_first() {
    // SOI + one-component SOF0 declaring 65535×65535 + EOI. There are deliberately no tables or
    // scan: the limit error proves geometry is refused at the SOF, before later decode state matters.
    let jpeg = [
        0xFF, 0xD8, // SOI
        0xFF, 0xC0, 0, 11, // SOF0, length 11
        8, 0xFF, 0xFF, 0xFF, 0xFF, // P=8, Y=65535, X=65535
        1, 1, 0x11, 0, // one component
        0xFF, 0xD9, // EOI
    ];
    assert_eq!(
        gray_error(
            &JpegDecoder::new().with_limits(
                DecodeLimits::new()
                    .with_max_width(4096)
                    .with_max_height(4096)
            ),
            &jpeg
        ),
        "JPEG: image exceeds the dimension limit"
    );
    assert_eq!(
        gray_error(
            &JpegDecoder::new().with_limits(
                DecodeLimits::new()
                    .with_max_width(u32::from(u16::MAX))
                    .with_max_height(u32::from(u16::MAX))
                    .with_max_pixels(u64::MAX)
                    .with_max_image_bytes(64 << 20)
            ),
            &jpeg
        ),
        "JPEG: image exceeds the size limit"
    );
}

/// SOI + a one-component SOF0 declaring `x` × `y` + EOI: no tables and no scan, so a limit error
/// proves the geometry was refused at the SOF, and any other error that it got past the limits.
fn gray_sof(x: u16, y: u16) -> [u8; 17] {
    let [xh, xl] = x.to_be_bytes();
    let [yh, yl] = y.to_be_bytes();
    [
        0xFF, 0xD8, // SOI
        0xFF, 0xC0, 0, 11, // SOF0, length 11
        8, yh, yl, xh, xl, // P=8, Y, X
        1, 1, 0x11, 0, // one component
        0xFF, 0xD9, // EOI
    ]
}

#[test]
fn default_refuses_a_side_over_32768_that_t81_permits_and_widened_limits_lift_it() {
    // T.81 Table B.2 lets X reach 65535; the workspace default stops at 32768 per side.
    const DIMENSION: &str = "JPEG: image exceeds the dimension limit";
    assert_ne!(
        gray_error(&JpegDecoder::new(), &gray_sof(32768, 1)),
        DIMENSION
    );
    assert_eq!(
        gray_error(&JpegDecoder::new(), &gray_sof(32769, 1)),
        DIMENSION
    );
    assert_eq!(
        gray_error(&JpegDecoder::default(), &gray_sof(1, 32769)),
        DIMENSION
    );
    let widened = JpegDecoder::new().with_limits(
        DecodeLimits::new()
            .with_max_width(u32::from(u16::MAX))
            .with_max_height(u32::from(u16::MAX))
            .with_max_pixels(u64::MAX),
    );
    assert_ne!(
        gray_error(&widened, &gray_sof(u16::MAX, u16::MAX)),
        DIMENSION
    );
}

#[test]
fn default_pixel_cap_is_exactly_16384_squared() {
    const DIMENSION: &str = "JPEG: image exceeds the dimension limit";
    assert_ne!(
        gray_error(&JpegDecoder::new(), &gray_sof(16384, 16384)),
        DIMENSION
    );
    assert_eq!(
        gray_error(&JpegDecoder::new(), &gray_sof(16384, 16385)),
        DIMENSION
    );
}

#[test]
fn default_has_no_byte_budget_so_a_6000_by_4000_photograph_decodes() {
    // 72 MB of RGB8, past the 64 MiB the default used to budget; libjpeg-turbo has no cap at all.
    let decoded: ImageBuf<Rgb8> = JpegDecoder::new()
        .decode_image(&rgb_jpeg(6000, 4000))
        .unwrap();
    assert_eq!(decoded.dimensions(), Dimensions::new(6000, 4000).unwrap());
}

#[test]
fn limits_are_cloneable_and_defaults_admit_ordinary_images() {
    let jpeg = gray_jpeg(8, 8);
    let default: ImageBuf<Gray8> = JpegDecoder::default().decode_image(&jpeg).unwrap();
    assert_eq!(default.dimensions(), Dimensions::new(8, 8).unwrap());

    let limited = JpegDecoder::new()
        .with_limits(DecodeLimits::new().with_max_width(7).with_max_height(8))
        .clone();
    assert_eq!(
        gray_error(&limited, &jpeg),
        "JPEG: image exceeds the dimension limit"
    );
}
