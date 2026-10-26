//! Hostile-input corpus for the pure-Rust decoder: on empty, truncated, garbage, bit-flipped and
//! over-large input the decoder must return an `Err` and **never panic** or allocate unboundedly.
//!
//! The invariant everywhere is *no panic*. A `#[test]` that panics fails, so simply driving the
//! decoder over each input (and, where meaningful, asserting the `Err`) is the whole test — we do not
//! need `catch_unwind`.
//!
//! The file itself needs only the decoder, so it is gated on `decode`; the parts that first *produce*
//! a valid stream to mangle need the libjxl-backed encoder and so are gated additionally on
//! `all(feature = "encode", any(not(target_arch = "wasm32"), target_os = "emscripten"))`, exactly like the other differential tests.
#![cfg(feature = "decode")]

use gamut_core::{DecodeImage, Rgba8};
use gamut_jxl::{DecodePartialImage, JxlDecoder};

// Only the stream-producing corpus needs the shared helpers (and the libjxl-backed encoder they use),
// so the module is pulled in under the same gate. `mod common;` here resolves to `tests/common/mod.rs`.
#[cfg(all(
    feature = "encode",
    any(not(target_arch = "wasm32"), target_os = "emscripten")
))]
mod common;

/// Decodes `data` as `Rgba8` — the widest natural request, so the internal conversion paths (alpha
/// padding, grayscale expansion) are exercised on whatever the (possibly corrupt) header claims.
fn decode_rgba8(data: &[u8]) -> gamut_core::Result<gamut_core::ImageBuf<Rgba8>> {
    <JxlDecoder as DecodeImage<Rgba8>>::decode_image(&JxlDecoder::new(), data)
}

/// The same request through the best-effort path, whose outcome is deliberately discarded.
///
/// Every corpus case below is driven through this as well as [`decode_rgba8`]. The relaxed path
/// reaches jxl-rs's flush machinery, which the rejecting path never touches — and that machinery
/// has open upstream bugs (libjxl/jxl-rs#730, #771, #783), so it is exactly the code that most needs
/// the no-panic invariant asserted over hostile input. `Ok` is a legitimate outcome here: a partial
/// decode of a corrupt stream may well produce a (possibly blank) image, and that is the point.
fn decode_rgba8_partial(data: &[u8]) {
    let _ =
        <JxlDecoder as DecodePartialImage<Rgba8>>::decode_partial_image(&JxlDecoder::new(), data);
}

/// A short deterministic "random-ish" junk generator: a linear congruential sweep, so the garbage is
/// reproducible across runs (no `rand` dependency, no flakiness).
fn junk(len: usize, seed: u32) -> Vec<u8> {
    let mut state = seed | 1;
    (0..len)
        .map(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (state >> 24) as u8
        })
        .collect()
}

#[test]
fn empty_input_errors_without_panicking() {
    assert!(decode_rgba8(&[]).is_err(), "empty input must error");
    // No dimensions to size a buffer with, so the best-effort path refuses this one too.
    decode_rgba8_partial(&[]);
}

#[test]
fn bare_signature_alone_errors() {
    // The 2-byte codestream signature with nothing after it is not a decodable image.
    assert!(decode_rgba8(&[0xFF, 0x0A]).is_err());
    decode_rgba8_partial(&[0xFF, 0x0A]);
}

#[test]
fn signatures_followed_by_garbage_error() {
    // A valid *signature* followed by a garbage body must be rejected, not decoded and not panic.
    for seed in [1u32, 0xDEAD_BEEF, 0x1234_5678] {
        // Bare codestream signature 0xFF 0x0A + junk.
        let mut cs = vec![0xFF, 0x0A];
        cs.extend_from_slice(&junk(512, seed));
        assert!(decode_rgba8(&cs).is_err(), "FF0A+junk seed {seed:#x}");
        decode_rgba8_partial(&cs);

        // The 12-byte ISO BMFF JXL signature box + junk.
        const JXL_BOX: [u8; 12] = [
            0x00, 0x00, 0x00, 0x0C, 0x4A, 0x58, 0x4C, 0x20, 0x0D, 0x0A, 0x87, 0x0A,
        ];
        let mut ct = JXL_BOX.to_vec();
        ct.extend_from_slice(&junk(512, seed ^ 0xFFFF));
        assert!(decode_rgba8(&ct).is_err(), "box+junk seed {seed:#x}");
        decode_rgba8_partial(&ct);
    }
}

/// Five bytes — a codestream signature and a size header declaring 9717 × 8098 — that the
/// best-effort path otherwise answers with a zero-filled ~314 MB `HeaderOnly` raster (#1026).
const HEADER_ONLY_9717X8098: [u8; 5] = [0xFF, 0x0A, 0x0A, 0xFD, 0x1A];

/// Decodes `data` best-effort as `Rgba8` through `decoder`, returning only the error.
fn partial_error(decoder: &JxlDecoder, data: &[u8]) -> gamut_core::Error {
    <JxlDecoder as DecodePartialImage<Rgba8>>::decode_partial_image(decoder, data)
        .map(|_| ())
        .expect_err("the caller's limit must refuse the stream")
}

#[test]
fn caller_limits_refuse_a_stream_truncated_after_its_headers() {
    // The stream really declares those dimensions: the header parser reads them back.
    let info = JxlDecoder::new()
        .info(&HEADER_ONLY_9717X8098)
        .expect("the size header parses");
    assert_eq!(
        (info.dimensions.width, info.dimensions.height),
        (9717, 8098)
    );

    let size = |message: &str, error: gamut_core::Error| {
        assert_eq!(
            error.kind(),
            gamut_core::ErrorKind::Unsupported,
            "{message}"
        );
        error.static_message()
    };
    // Either side over its cap is refused on its own.
    let narrow = JxlDecoder::new().with_max_dimensions(9716, 8098);
    assert_eq!(
        size("width", partial_error(&narrow, &HEADER_ONLY_9717X8098)),
        Some("JXL: image exceeds the dimension limit")
    );
    let short = JxlDecoder::new().with_max_dimensions(9717, 8097);
    assert_eq!(
        size("height", partial_error(&short, &HEADER_ONLY_9717X8098)),
        Some("JXL: image exceeds the dimension limit")
    );
    // A byte budget well under the declared raster is refused too.
    let budget = JxlDecoder::new().with_max_image_bytes(1 << 20);
    assert_eq!(
        size("bytes", partial_error(&budget, &HEADER_ONLY_9717X8098)),
        Some("JXL: image exceeds the size limit")
    );
    // The rejecting path refuses on the limit too, not on the truncation it would otherwise
    // report.
    let error = <JxlDecoder as DecodeImage<Rgba8>>::decode_image(&budget, &HEADER_ONLY_9717X8098)
        .expect_err("the limit refuses the full decode as well");
    assert_eq!(
        error.static_message(),
        Some("JXL: image exceeds the size limit")
    );
}

/// The stream-producing corpus: everything that first encodes a real JPEG XL stream (via libjxl) and
/// then feeds a mangled version of it to the decoder.
#[cfg(all(
    feature = "encode",
    any(not(target_arch = "wasm32"), target_os = "emscripten")
))]
mod with_streams {
    use gamut_core::convert::ConvertPolicy;
    use gamut_core::{
        DecodeImage, Dimensions, EncodeImage, ErrorKind, Gray8, ImageRef, Pixel, Rgb8, Rgba8,
    };
    use gamut_jxl::{Container, DecodePartialImage, Effort, JxlDecoder, JxlEncoder};

    use super::{decode_rgba8, decode_rgba8_partial};
    use crate::common::gen_u8;

    /// A small, valid, lossless bare codestream (textured 16×16 RGB).
    fn valid_codestream() -> Vec<u8> {
        let (w, h) = (16u32, 16u32);
        let px = gen_u8(w, h, Rgb8::CHANNELS);
        let dims = Dimensions::new(w, h).unwrap();
        let img = ImageRef::<Rgb8>::new(&px, dims).unwrap();
        JxlEncoder::lossless().encode_to_vec(img).unwrap()
    }

    /// The same image, in the ISO BMFF container framing.
    fn valid_container() -> Vec<u8> {
        let (w, h) = (16u32, 16u32);
        let px = gen_u8(w, h, Rgb8::CHANNELS);
        let dims = Dimensions::new(w, h).unwrap();
        let img = ImageRef::<Rgb8>::new(&px, dims).unwrap();
        JxlEncoder::lossless()
            .with_container(Container::IsoBmff)
            .encode_to_vec(img)
            .unwrap()
    }

    #[test]
    fn short_prefixes_of_both_framings_error() {
        // A prefix of 1..=16 bytes of either framing is far too short to be a complete image, so
        // every one must return an error (and none may panic).
        let cs = valid_codestream();
        let ct = valid_container();
        for stream in [&cs, &ct] {
            for len in 1..=16usize.min(stream.len()) {
                assert!(
                    decode_rgba8(&stream[..len]).is_err(),
                    "prefix len {len} decoded unexpectedly"
                );
                decode_rgba8_partial(&stream[..len]);
            }
        }
    }

    /// Every truncation length of a small valid stream, driven through the best-effort path.
    ///
    /// Dense (step 1, not 7) and on a *lossless* stream by design: Modular is the one encoding for
    /// which jxl-rs hands a deliberately incomplete section to its bit reader, so this is the shape
    /// that reaches the least-travelled upstream code. The invariant is only "no panic" — these
    /// 16×16 fixtures are a single coded group, so they have nothing to render partially, and it is
    /// `partial.rs` that asserts what a decodable truncation actually produces.
    #[test]
    fn dense_truncations_of_both_framings_never_panic_on_the_partial_path() {
        for stream in [valid_codestream(), valid_container()] {
            for len in 0..=stream.len() {
                decode_rgba8_partial(&stream[..len]);
            }
        }
    }

    #[test]
    fn systematic_truncations_error_until_full() {
        // Every 7th truncation length from 0 up to (but not including) the full stream must error:
        // jxl-rs signals the missing tail as "needs more input", which the decoder maps to a
        // truncation error. The full stream itself decodes cleanly. This pins the *default* path's
        // contract, which the opt-in `DecodePartialImage` surface deliberately does not move; the
        // best-effort behaviour is asserted separately, above and in `partial.rs`.
        for stream in [valid_codestream(), valid_container()] {
            let full = stream.len();
            let mut len = 0;
            while len < full {
                assert!(
                    decode_rgba8(&stream[..len]).is_err(),
                    "truncation to {len}/{full} bytes decoded unexpectedly"
                );
                len += 7;
            }
            // Sanity: the untruncated stream is genuinely decodable, so the errors above are about
            // truncation, not an always-broken stream.
            assert!(decode_rgba8(&stream).is_ok(), "full stream must decode");
        }
    }

    #[test]
    fn single_bit_flips_never_panic() {
        // Flip each bit in the first 256 bytes (or the whole stream if shorter) of a small valid
        // codestream and decode the result. Bits in the header usually corrupt the stream (Err), but
        // bits in the entropy-coded sample data may decode to a *different* image — both outcomes are
        // acceptable. The invariant is only: no panic, and no unbounded allocation (the decoder's
        // pixel limit bounds any size claimed by a flipped dimension field).
        let base = valid_codestream();
        let scan = base.len().min(256);
        for byte in 0..scan {
            for bit in 0..8u8 {
                let mut m = base.clone();
                m[byte] ^= 1 << bit;
                // Drive the decoder; discard the outcome. A panic here fails the test.
                let _ = decode_rgba8(&m);
                // Same corpus through the relaxed path, which additionally reaches the flush code.
                decode_rgba8_partial(&m);
            }
        }
    }

    /// A small lossless bare codestream of layout `P` (16×16, textured).
    fn codestream_of<P: Pixel<Sample = u8> + std::fmt::Debug>() -> Vec<u8>
    where
        JxlEncoder: EncodeImage<P>,
    {
        let (w, h) = (16u32, 16u32);
        let px = gen_u8(w, h, P::CHANNELS);
        let img = ImageRef::<P>::new(&px, Dimensions::new(w, h).unwrap()).unwrap();
        JxlEncoder::lossless().encode_to_vec(img).unwrap()
    }

    #[test]
    fn dimension_caps_admit_their_exact_bound() {
        let stream = valid_codestream();
        let at = JxlDecoder::new().with_max_dimensions(16, 16);
        let image = <JxlDecoder as DecodeImage<Rgb8>>::decode_image(&at, &stream)
            .expect("an image exactly at the caps decodes");
        assert_eq!(image.dimensions(), Dimensions::new(16, 16).unwrap());
        for (w, h) in [(15, 16), (16, 15)] {
            let under = JxlDecoder::new().with_max_dimensions(w, h);
            let err = <JxlDecoder as DecodeImage<Rgb8>>::decode_image(&under, &stream)
                .expect_err("one pixel over a cap is refused");
            assert_eq!(err.kind(), ErrorKind::Unsupported);
            assert_eq!(
                err.static_message(),
                Some("JXL: image exceeds the dimension limit")
            );
        }
    }

    /// Asserts `budget` bytes decode `stream` as `P` under `policy` and `budget - 1` are refused.
    fn assert_byte_budget_is_exact<P: Pixel>(stream: &[u8], policy: ConvertPolicy, budget: usize)
    where
        JxlDecoder: DecodeImage<P>,
    {
        let decoder = JxlDecoder::new().with_convert_policy(policy);
        let at = decoder.clone().with_max_image_bytes(budget);
        assert!(
            <JxlDecoder as DecodeImage<P>>::decode_image(&at, stream).is_ok(),
            "{budget} bytes admit the raster"
        );
        let under = decoder.with_max_image_bytes(budget - 1);
        let err = <JxlDecoder as DecodeImage<P>>::decode_image(&under, stream)
            .map(|_| ())
            .expect_err("one byte under the raster is refused");
        assert_eq!(err.kind(), ErrorKind::Unsupported);
        assert_eq!(
            err.static_message(),
            Some("JXL: image exceeds the size limit")
        );
    }

    #[test]
    fn the_byte_budget_weighs_the_requested_layout_when_it_is_wider() {
        // A grayscale stream widened to RGBA: the requested 4 bytes per pixel are what is budgeted.
        assert_byte_budget_is_exact::<Rgba8>(
            &codestream_of::<Gray8>(),
            ConvertPolicy::lossless(),
            16 * 16 * 4,
        );
    }

    #[test]
    fn the_byte_budget_weighs_the_stream_layout_when_it_is_wider() {
        // An RGBA stream whose alpha a permissive policy drops: the stream's 4 bytes per pixel are
        // decoded before the 3-byte result exists, so they are what is budgeted.
        assert_byte_budget_is_exact::<Rgb8>(
            &codestream_of::<Rgba8>(),
            ConvertPolicy::permissive(),
            16 * 16 * 4,
        );
    }

    #[test]
    fn oversized_dimensions_trigger_the_pixel_limit() {
        // Honestly provoke the decoder's pixel-limit guard: encode a real 10000×10000 grayscale image
        // (flat, so it compresses to a few KiB and encodes in well under a second), then decode it.
        // jxl-rs checks `xsize.max(16) * ysize * (3 + extra_channels)` against the decoder's
        // `1 << 28` pixel limit while parsing the file header — 10000·10000·3 = 3.0e8 ≥ 2.68e8 — so it
        // rejects the stream *before* allocating any pixel buffer. This exercises the
        // `ImageSizeTooLarge → pixel-limit` mapping on a genuine oversized stream, not a forged header.
        let (w, h) = (10_000u32, 10_000u32);
        let px = vec![0u8; w as usize * h as usize];
        let dims = Dimensions::new(w, h).unwrap();
        let img = ImageRef::<Gray8>::new(&px, dims).unwrap();
        let bytes = JxlEncoder::lossless()
            .with_effort(Effort::Lightning)
            .encode_to_vec(img)
            .unwrap();

        // The caller's caps only tighten: the most permissive settings leave the fixed ceiling.
        let wide_open = JxlDecoder::new()
            .with_max_dimensions(u32::MAX, u32::MAX)
            .with_max_image_bytes(usize::MAX);
        let err = <JxlDecoder as DecodeImage<Rgba8>>::decode_image(&wide_open, &bytes)
            .expect_err("the pixel limit is not raised by the caller's caps");
        assert_eq!(
            err.static_message(),
            Some("JXL: image exceeds the decoder pixel limit")
        );

        let err = decode_rgba8(&bytes).expect_err("oversized image must be rejected");
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
        assert_eq!(
            err.static_message(),
            Some("JXL: image exceeds the decoder pixel limit")
        );
        assert!(err.detail().is_some());

        // The best-effort path relaxes truncation, not the memory bound: the same typed refusal.
        let err = <JxlDecoder as DecodePartialImage<Rgba8>>::decode_partial_image(
            &JxlDecoder::new(),
            &bytes,
        )
        .expect_err("the pixel limit is not relaxed by the best-effort policy");
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
        assert_eq!(
            err.static_message(),
            Some("JXL: image exceeds the decoder pixel limit")
        );
    }
}
