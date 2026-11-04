//! `WebpDecoder`'s caller-set resource limits (issue #1025): `with_max_dimensions` and
//! `with_max_image_bytes`, checked against the bitstream header before any frame-sized allocation
//! and before any backend is offered the job.
//!
//! The over-budget fixtures are header-only: a frame the limits admit would be decoded in full, so a
//! large one is admitted only behind a backend that accepts the job and fails at once — reaching that
//! backend is the proof the limits let it through, and nothing frame-sized is ever allocated.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use gamut_core::{
    DecodeImage, Dimensions, EncodeImage, Error, ErrorKind, ImageBuf, ImageRef, Result, Rgb8, Rgba8,
};
use gamut_webp::{CodestreamInfo, DecodedRaster, WebpCodestreamDecoder, WebpDecoder, WebpEncoder};

const SIZE_LIMIT: &str = "WebP: image exceeds the size limit";
const DIMENSION_LIMIT: &str = "WebP: image exceeds the dimension limit";
const BACKEND_RAN: &str = "test backend ran";

/// The fuzz-found lossy WebP from issue #1025, verbatim (its RIFF size field reads 54): a `VP8 `
/// key frame declaring 4176×4176, which decodes to 70 MB of RGBA.
const REPRODUCER_HEX: &str = "52494646360000005745425056503820\
                              2a000000d001009d012a505050505050\
                              50505050505050505050505050505050\
                              5050505050505005000300a48ec0";

fn reproducer() -> Vec<u8> {
    (0..REPRODUCER_HEX.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&REPRODUCER_HEX[i..i + 2], 16).expect("hex"))
        .collect()
}

#[track_caller]
fn assert_refused<T>(result: Result<T>, message: &'static str) {
    match result {
        Err(error) => {
            assert_eq!(error.kind(), ErrorKind::Unsupported);
            assert_eq!(error.static_message(), Some(message));
        }
        Ok(_) => panic!("expected the decoder to refuse with {message:?}"),
    }
}

/// A backend that accepts every job and fails it at once, counting how often it was reached.
struct Failing(Arc<AtomicUsize>);

impl WebpCodestreamDecoder for Failing {
    fn supports(&mut self, _: &CodestreamInfo) -> bool {
        true
    }

    fn decode(&mut self, _: &CodestreamInfo, _: &[u8]) -> Result<DecodedRaster> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(Error::invalid_input("limits-test", BACKEND_RAN))
    }
}

/// Decodes `file` through `decoder` with a [`Failing`] backend installed, returning whether the
/// backend was reached (the limits let the frame through) or the limit error it was refused with.
fn reaches_backend(decoder: WebpDecoder, file: &[u8]) -> Result<bool> {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut decoder = decoder;
    decoder.push_backend(Failing(Arc::clone(&calls)));
    let result: Result<ImageBuf<Rgba8>> = decoder.decode_image(file);
    match result {
        Err(error) if error.static_message() == Some(BACKEND_RAN) => {
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            Ok(true)
        }
        Err(error) => {
            assert_eq!(
                calls.load(Ordering::SeqCst),
                0,
                "refused after the backend ran"
            );
            Err(error)
        }
        Ok(_) => panic!("the failing backend cannot produce an image"),
    }
}

/// The reproducer with its frame dimensions rewritten (scale bits cleared).
fn lossy_header(width: u16, height: u16) -> Vec<u8> {
    let mut file = reproducer();
    file[26..28].copy_from_slice(&width.to_le_bytes());
    file[28..30].copy_from_slice(&height.to_le_bytes());
    file
}

/// A simple-format lossless file holding only a VP8L header declaring `width`×`height` (RFC 9649
/// §3.2: signature `0x2f`, then 14-bit `width - 1`, 14-bit `height - 1`, alpha hint, 3-bit
/// version 0, LSB first).
fn lossless_header(width: u32, height: u32) -> Vec<u8> {
    let bits = (width - 1) | ((height - 1) << 14);
    let mut header = vec![0x2f];
    header.extend_from_slice(&bits.to_le_bytes());
    gamut_riff::write_simple_lossless(&header).expect("riff")
}

/// A real `width`×`height` RGB image encoded by `encoder`.
fn encoded(encoder: &WebpEncoder, width: u32, height: u32) -> Vec<u8> {
    let px: Vec<u8> = (0..width * height * 3).map(|i| (i * 37) as u8).collect();
    let mut out = Vec::new();
    encoder
        .encode_image(
            ImageRef::<Rgb8>::new(&px, Dimensions { width, height }).expect("fixture"),
            &mut out,
        )
        .expect("encode");
    out
}

#[test]
fn default_budget_refuses_the_54_byte_reproducer() {
    let result: Result<ImageBuf<Rgba8>> = WebpDecoder::new().decode_image(&reproducer());
    assert_refused(result, SIZE_LIMIT);
}

#[test]
fn default_budget_is_exactly_a_4096_square_rgba_frame() {
    // 4096 × 4096 × 4 = 64 MiB is admitted; one more row is not, and is refused before the backend.
    assert!(reaches_backend(WebpDecoder::new(), &lossy_header(4096, 4096)).expect("admitted"));
    assert_refused(
        reaches_backend(WebpDecoder::new(), &lossy_header(4096, 4097)),
        SIZE_LIMIT,
    );
}

#[test]
fn default_dimensions_admit_the_format_maximum() {
    // Without a byte budget in the way, the default dimension caps admit VP8L's largest frame.
    let decoder = WebpDecoder::new().with_max_image_bytes(usize::MAX);
    assert!(reaches_backend(decoder, &lossless_header(16384, 16384)).expect("admitted"));
}

#[test]
fn max_image_bytes_charges_four_bytes_per_pixel() {
    // An opaque lossy 3×5 image is charged 3 × 5 × 4 = 60 bytes, though it decodes to 45 of RGB.
    let file = encoded(&WebpEncoder::lossy(90), 3, 5);
    let at: Result<ImageBuf<Rgb8>> = WebpDecoder::new()
        .with_max_image_bytes(60)
        .decode_image(&file);
    assert!(at.is_ok());
    let under: Result<ImageBuf<Rgb8>> = WebpDecoder::new()
        .with_max_image_bytes(59)
        .decode_image(&file);
    assert_refused(under, SIZE_LIMIT);
}

#[test]
fn max_dimensions_bound_each_side_inclusively() {
    let file = encoded(&WebpEncoder::lossless(), 3, 5);
    let at: Result<ImageBuf<Rgb8>> = WebpDecoder::new()
        .with_max_dimensions(3, 5)
        .decode_image(&file);
    assert!(at.is_ok());
    let narrow: Result<ImageBuf<Rgb8>> = WebpDecoder::new()
        .with_max_dimensions(2, 5)
        .decode_image(&file);
    assert_refused(narrow, DIMENSION_LIMIT);
    let short: Result<ImageBuf<Rgb8>> = WebpDecoder::new()
        .with_max_dimensions(3, 4)
        .decode_image(&file);
    assert_refused(short, DIMENSION_LIMIT);
}

#[test]
fn limits_are_checked_before_any_backend_is_offered_the_job() {
    let decoder = WebpDecoder::new().with_max_dimensions(4175, 4176);
    assert_refused(reaches_backend(decoder, &reproducer()), DIMENSION_LIMIT);
}
