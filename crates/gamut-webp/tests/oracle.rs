//! Differential oracle (libwebp) for the WebP codecs — lossless (VP8L) and lossy (VP8).
//!
//! libwebp is the third-party reference. Lossy is checked at the YUV-plane level (RGB↔YCbCr is
//! implementation-defined and off the bit-exact gate), in both directions:
//!   - **gamut encode → libwebp decode == gamut decode**, across every encoder feature (prediction,
//!     loop filters, segmentation, token partitions, skip) — pinning gamut's streams as conformant;
//!   - **libwebp encode → gamut decode == libwebp decode**, bit-exact — pinning gamut's decoder
//!     against the full feature surface a production encoder emits (per-segment filter levels,
//!     probability updates, …);
//!   - the lossless round-trips (libwebp self-round-trip; gamut↔libwebp) that shipped with VP8L.

mod common;

use common::{
    libwebp_decode_rgba, libwebp_encode_lossless_rgba, libwebp_get_info, pattern_rgba,
    photo_like_rgba,
};
use gamut_core::{DecodeImage, Dimensions, EncodeImage, ImageBuf, ImageRef, Rgb8, Rgba8};
use gamut_webp::{WebpDecoder, WebpEncoder};

/// The standard dimension matrix exercised by the differential tests, including the awkward
/// single-row / single-column / non-power-of-two cases.
const DIMENSIONS: &[(u32, u32)] = &[
    (1, 1),
    (2, 2),
    (16, 16),
    (17, 9),
    (64, 48),
    (255, 1),
    (1, 255),
];

/// Larger canvases that push past the small-block regime the `DIMENSIONS` matrix (≤255px) stays in:
/// VP8 macroblock grids spanning many rows, and VP8L entropy-image regions plus LZ77 back-references
/// whose distances only exceed 256 above this size. `(300, 70)` is deliberately not a multiple of
/// libwebp's histogram block size, so it straddles entropy-image tile boundaries.
const LARGE_DIMENSIONS: &[(u32, u32)] =
    &[(256, 256), (384, 288), (640, 480), (1024, 768), (300, 70)];

/// Drops the alpha byte of an interleaved RGBA buffer, yielding interleaved RGB.
fn rgba_to_rgb(rgba: &[u8]) -> Vec<u8> {
    rgba.as_chunks::<4>()
        .0
        .iter()
        .flat_map(|p| [p[0], p[1], p[2]])
        .collect()
}

#[test]
fn libwebp_lossless_self_roundtrip() {
    // Encode → get_info → decode entirely within libwebp: the wrappers and the linked libwebp build
    // are correct iff a fully-opaque image survives a lossless round-trip bit-exactly.
    for (w, h) in [(1u32, 1u32), (16, 16), (17, 9), (64, 48)] {
        let rgba = pattern_rgba(w, h);
        let webp = libwebp_encode_lossless_rgba(&rgba, w, h);
        assert!(!webp.is_empty(), "encode produced no bytes at {w}x{h}");
        assert_eq!(
            libwebp_get_info(&webp),
            Some((w, h)),
            "get_info mismatch at {w}x{h}"
        );
        let decoded = libwebp_decode_rgba(&webp);
        assert_eq!((decoded.width, decoded.height), (w, h));
        assert_eq!(
            decoded.rgba, rgba,
            "lossless must round-trip bit-exactly at {w}x{h}"
        );
    }
}

#[test]
fn gamut_decodes_libwebp_lossless_to_source() {
    // libwebp encodes (choosing its own transforms / LZ77 / color cache); gamut must decode back to
    // the exact source pixels — the end-to-end lossless guarantee. Both an algebraic pattern and
    // photographic content are exercised over the small matrix *and* the large canvases (>256px) that
    // reach the multi-tile entropy-image and long-back-reference decode paths the small inputs never do.
    for &(w, h) in DIMENSIONS.iter().chain(LARGE_DIMENSIONS) {
        for (label, rgba) in [
            ("pattern", pattern_rgba(w, h)),
            ("photo", photo_like_rgba(w, h, 0x51ed)),
        ] {
            let webp = libwebp_encode_lossless_rgba(&rgba, w, h);
            let got: ImageBuf<Rgb8> = WebpDecoder::new()
                .decode_image(&webp)
                .expect("gamut decode");
            let dims = got.dimensions();
            assert_eq!((dims.width, dims.height), (w, h), "{label} dims at {w}x{h}");
            assert_eq!(
                got.as_samples(),
                rgba_to_rgb(&rgba).as_slice(),
                "{label} pixel mismatch at {w}x{h}"
            );
        }
    }
}

#[test]
fn libwebp_decodes_gamut_lossless_to_source() {
    // The reverse direction: gamut encodes, libwebp (the reference) decodes and must recover the
    // source — proving gamut emits a conformant lossless stream. Both content types over the small
    // matrix and the large canvases that exercise gamut's entropy-image / long-back-reference encoder.
    for &(w, h) in DIMENSIONS.iter().chain(LARGE_DIMENSIONS) {
        for (label, rgba) in [
            ("pattern", pattern_rgba(w, h)),
            ("photo", photo_like_rgba(w, h, 0x9a1c)),
        ] {
            assert_gamut_encode_libwebp_decode(
                &rgba_to_rgb(&rgba),
                w,
                h,
                &format!("{label} {w}x{h}"),
            );
        }
    }
}

/// Encodes interleaved RGB with gamut and decodes it with libwebp, asserting the pixels survive.
fn assert_gamut_encode_libwebp_decode(rgb: &[u8], w: u32, h: u32, label: &str) {
    let mut webp = Vec::new();
    WebpEncoder::lossless()
        .encode_image(
            ImageRef::<Rgb8>::new(
                rgb,
                Dimensions {
                    width: w,
                    height: h,
                },
            )
            .unwrap(),
            &mut webp,
        )
        .expect("gamut encode");
    let decoded = libwebp_decode_rgba(&webp);
    assert_eq!((decoded.width, decoded.height), (w, h), "dims for {label}");
    assert_eq!(rgba_to_rgb(&decoded.rgba), rgb, "pixels for {label}");
}

#[test]
fn libwebp_decodes_every_gamut_encoder_path() {
    // Each image steers gamut's encoder down a different path; libwebp must decode them all.
    let (w, h) = (40u32, 40u32);
    let n = (w * h) as usize;

    // Solid color → palette transform with 8-pixel bundling.
    let solid: Vec<u8> = [30u8, 60, 90].repeat(n);
    assert_gamut_encode_libwebp_decode(&solid, w, h, "solid");

    // Few colors, repetitive → palette + color cache + LZ77.
    let palette = [[10u8, 20, 30], [40, 50, 60], [70, 80, 90]];
    let few: Vec<u8> = (0..n).flat_map(|i| palette[i % 3]).collect();
    assert_gamut_encode_libwebp_decode(&few, w, h, "few-color");

    // 32 colors split top/bottom → palette + multi-group entropy image.
    let regioned: Vec<u8> = (0..n)
        .flat_map(|i| {
            let (x, y) = (i as u32 % w, i as u32 / w);
            let scatter = ((x * 7 + y * 11) % 16) as u8;
            let base = if y < h / 2 { 0 } else { 16 };
            let idx = base + scatter;
            [idx, idx.wrapping_mul(7), idx.wrapping_mul(13)]
        })
        .collect();
    assert_gamut_encode_libwebp_decode(&regioned, w, h, "multi-region");

    // Many colors → spatial transforms (subtract-green/predictor/color) + LZ77 + cache.
    let many: Vec<u8> = (0..n)
        .flat_map(|i| {
            let (x, y) = (i as u32 % w, i as u32 / w);
            [
                (x * 9 + y * 5) as u8,
                (x * 13 + y * 7) as u8,
                (x * 17 + y * 3) as u8,
            ]
        })
        .collect();
    assert_gamut_encode_libwebp_decode(&many, w, h, "many-color");
}

/// Builds a structured YUV 4:2:0 image (real residuals to exercise the transforms/tokens).
fn synthetic_yuv(w: u32, h: u32) -> gamut_color::Yuv420 {
    let (wu, hu) = (w as usize, h as usize);
    let (cw, ch) = (
        gamut_color::Yuv420::chroma_width(w) as usize,
        gamut_color::Yuv420::chroma_height(h) as usize,
    );
    let y = (0..wu * hu)
        .map(|i| ((i * 9 + (i / wu) * 5) & 0xff) as u8)
        .collect();
    let u = (0..cw * ch).map(|i| ((i * 3 + 80) & 0xff) as u8).collect();
    let v = (0..cw * ch).map(|i| ((i * 7 + 150) & 0xff) as u8).collect();
    gamut_color::Yuv420::new(w, h, y, u, v).unwrap()
}

/// B_PRED-favorable content: each 4×4 region carries a different gradient direction, so a single
/// whole-block mode predicts the macroblock poorly and gamut's encoder picks per-subblock `B_PRED`.
fn detailed_yuv(w: u32, h: u32) -> gamut_color::Yuv420 {
    let (wu, hu) = (w as usize, h as usize);
    let (cw, ch) = (
        gamut_color::Yuv420::chroma_width(w) as usize,
        gamut_color::Yuv420::chroma_height(h) as usize,
    );
    let y = (0..wu * hu)
        .map(|i| {
            let (x, yy) = (i % wu, i / wu);
            let v = match (x / 4 + yy / 4) % 4 {
                0 => x * 18,
                1 => yy * 18,
                2 => (x + yy) * 18,
                _ => x.wrapping_sub(yy).wrapping_mul(18),
            };
            (v & 0xff) as u8
        })
        .collect();
    let u = (0..cw * ch).map(|i| ((i * 3) & 0xff) as u8).collect();
    let v = (0..cw * ch).map(|i| ((i * 9 + 70) & 0xff) as u8).collect();
    gamut_color::Yuv420::new(w, h, y, u, v).unwrap()
}

/// Builds a YUV 4:2:0 image with **photographic-like statistics** directly in the YUV domain (so the
/// bit-exact YUV conformance gate stays independent of the RGB↔YCbCr layer, which PR4 pins
/// separately): a smooth luma gradient + low-amplitude detail + hard region edges, with gently
/// varying chroma. RNG-free and `seed`-parameterised, like [`common::photo_like_rgba`].
fn photo_like_yuv(w: u32, h: u32, seed: u32) -> gamut_color::Yuv420 {
    let (wu, hu) = (w as usize, h as usize);
    let (cw, ch) = (
        gamut_color::Yuv420::chroma_width(w) as usize,
        gamut_color::Yuv420::chroma_height(h) as usize,
    );
    let hash = |x: i64, y: i64, k: i64| -> i64 {
        let mut v = x.wrapping_mul(374_761_393)
            ^ y.wrapping_mul(668_265_263)
            ^ k.wrapping_mul(2_654_435_761)
            ^ i64::from(seed).wrapping_mul(2_246_822_519);
        v = (v ^ (v >> 13)).wrapping_mul(1_274_126_177);
        (v ^ (v >> 16)) & 0xff
    };
    let clamp = |v: i64| v.clamp(0, 255) as u8;
    let (wi, hi) = (wu.max(1) as i64, hu.max(1) as i64);
    let y = (0..wu * hu)
        .map(|i| {
            let (x, yy) = ((i % wu) as i64, (i / wu) as i64);
            // Smooth diagonal base + small detail + sharp 8-region steps (B_PRED / loop-filter bait).
            let base = x * 170 / wi + yy * 60 / hi;
            let detail = (hash(x, yy, 0) - 128) / 10;
            let edge = if (x * 4 / wi + yy * 4 / hi) % 2 == 0 {
                28
            } else {
                0
            };
            clamp(base + detail + edge)
        })
        .collect();
    let (cwi, chi) = (cw.max(1) as i64, ch.max(1) as i64);
    let u = (0..cw * ch)
        .map(|i| {
            let (x, yy) = ((i % cw) as i64, (i / cw) as i64);
            clamp(100 + x * 70 / cwi + (hash(x, yy, 1) - 128) / 16)
        })
        .collect();
    let v = (0..cw * ch)
        .map(|i| {
            let (x, yy) = ((i % cw) as i64, (i / cw) as i64);
            clamp(140 + yy * 60 / chi + (hash(x, yy, 2) - 128) / 16)
        })
        .collect();
    gamut_color::Yuv420::new(w, h, y, u, v).unwrap()
}

#[test]
fn gamut_lossy_bpred_matches_libwebp_bit_exact() {
    // Detailed content drives gamut's encoder into per-4×4 B_PRED macroblocks; libwebp must decode the
    // same gamut bitstream to identical YUV — the tier-3 conformance gate for the B_PRED path.
    use common::libwebp_decode_yuv;
    use gamut_riff::write_simple_lossy;
    use gamut_webp::vp8::frame::{decode_frame, encode_frame};

    for &(w, h) in &[(16u32, 16u32), (32, 32), (48, 48), (49, 33), (64, 16)] {
        for &quant_index in &[0u8, 8, 40] {
            let (payload, _) = encode_frame(&detailed_yuv(w, h), quant_index)
                .expect("fixture fits the partition-size fields");
            let webp = write_simple_lossy(&payload).unwrap();
            let lib = libwebp_decode_yuv(&webp);
            let gamut = decode_frame(&payload).expect("gamut decode").to_yuv420();
            assert_eq!((lib.width, lib.height), (w, h), "dims at {w}x{h}");
            assert_eq!(
                gamut.y(),
                lib.y.as_slice(),
                "B_PRED Y mismatch at {w}x{h} q{quant_index}"
            );
            assert_eq!(
                gamut.u(),
                lib.u.as_slice(),
                "B_PRED U mismatch at {w}x{h} q{quant_index}"
            );
            assert_eq!(
                gamut.v(),
                lib.v.as_slice(),
                "B_PRED V mismatch at {w}x{h} q{quant_index}"
            );
        }
    }
}

#[test]
fn gamut_lossy_options_match_libwebp_bit_exact() {
    // The alternative encoder paths — the simple loop filter and quantizer segmentation — must each
    // produce a stream libwebp decodes identically to gamut's decoder (the tier-3 conformance gate).
    use common::libwebp_decode_yuv;
    use gamut_riff::write_simple_lossy;
    use gamut_webp::vp8::frame::{EncodeOptions, decode_frame, encode_frame_filtered};

    let base = EncodeOptions::default();
    let cases = [
        (
            "simple-filter",
            EncodeOptions {
                simple_filter: true,
                ..base
            },
        ),
        (
            "segmented",
            EncodeOptions {
                segmented: true,
                ..base
            },
        ),
        (
            "segmented+simple",
            EncodeOptions {
                simple_filter: true,
                segmented: true,
                ..base
            },
        ),
        (
            "partitions-2",
            EncodeOptions {
                partitions: 2,
                ..base
            },
        ),
        (
            "partitions-4",
            EncodeOptions {
                partitions: 4,
                ..base
            },
        ),
        (
            "partitions-8",
            EncodeOptions {
                partitions: 8,
                ..base
            },
        ),
        (
            "everything",
            EncodeOptions {
                simple_filter: true,
                segmented: true,
                partitions: 4,
                ..base
            },
        ),
    ];
    for (label, opts) in cases {
        // (33, 145) spans ten macroblock rows, so the eight-partition cases route across every one.
        for &(w, h) in &[(32u32, 32u32), (48, 48), (49, 33), (33, 145)] {
            for &q in &[12u8, 48] {
                let (payload, _) = encode_frame_filtered(&detailed_yuv(w, h), q, opts)
                    .expect("fixture fits the partition-size fields");
                let webp = write_simple_lossy(&payload).unwrap();
                let lib = libwebp_decode_yuv(&webp);
                let gamut = decode_frame(&payload).expect("gamut decode").to_yuv420();
                assert_eq!(gamut.y(), lib.y.as_slice(), "{label} Y at {w}x{h} q{q}");
                assert_eq!(gamut.u(), lib.u.as_slice(), "{label} U at {w}x{h} q{q}");
                assert_eq!(gamut.v(), lib.v.as_slice(), "{label} V at {w}x{h} q{q}");
            }
        }
    }
}

#[test]
fn gamut_lossy_yuv_matches_libwebp_bit_exact() {
    // A VP8 bitstream decodes to a deterministic integer YUV, so gamut's own decoder and libwebp must
    // agree bit-for-bit on the same gamut-produced bitstream — the tier-3 conformance gate that pins
    // the encoder to a spec-valid stream. (gamut additionally checks encoder-recon == its own decoder
    // in frame.rs; together these need no pixel tolerance.)
    use common::libwebp_decode_yuv;
    use gamut_riff::write_simple_lossy;
    use gamut_webp::vp8::frame::{decode_frame, encode_frame};

    for &(w, h) in &[
        (16u32, 16u32),
        (32, 32),
        (17, 9),
        (64, 48),
        (80, 16),
        (33, 49),
    ] {
        for &quant_index in &[0u8, 20, 60, 110] {
            let (payload, _) = encode_frame(&synthetic_yuv(w, h), quant_index)
                .expect("fixture fits the partition-size fields");
            let webp = write_simple_lossy(&payload).unwrap();
            let lib = libwebp_decode_yuv(&webp);
            let gamut = decode_frame(&payload).expect("gamut decode").to_yuv420();
            assert_eq!((lib.width, lib.height), (w, h), "dims at {w}x{h}");
            assert_eq!(
                gamut.y(),
                lib.y.as_slice(),
                "Y mismatch at {w}x{h} q{quant_index}"
            );
            assert_eq!(
                gamut.u(),
                lib.u.as_slice(),
                "U mismatch at {w}x{h} q{quant_index}"
            );
            assert_eq!(
                gamut.v(),
                lib.v.as_slice(),
                "V mismatch at {w}x{h} q{quant_index}"
            );
        }
    }
}

#[test]
fn gamut_lossy_yuv_realistic_and_large_matches_libwebp() {
    // The same tier-3 conformance gate as above, but on photographic-like YUV and on canvases far
    // larger than the small synthetic frames: macroblock grids spanning dozens of rows (8-partition
    // routing across all of them) and realistic residual/token distributions. gamut's own decoder and
    // libwebp must still agree bit-for-bit on the gamut-produced stream.
    use common::libwebp_decode_yuv;
    use gamut_riff::write_simple_lossy;
    use gamut_webp::vp8::frame::{decode_frame, encode_frame};

    // Large sizes span many MB rows (768/16 = 48, so 8-partition routing touches every row) and an
    // awkward width (513×97). gamut's encoder is fast enough here that the full range stays cheap.
    let dims = [
        (32u32, 32u32),
        (64, 48),
        (256, 256),
        (384, 288),
        (640, 480),
        (1024, 768),
        (513, 97),
    ];
    for &(w, h) in &dims {
        for &quant_index in &[12u8, 56] {
            let (payload, _) = encode_frame(&photo_like_yuv(w, h, 0x7e57), quant_index)
                .expect("fixture fits the partition-size fields");
            let webp = write_simple_lossy(&payload).unwrap();
            let lib = libwebp_decode_yuv(&webp);
            let gamut = decode_frame(&payload).expect("gamut decode").to_yuv420();
            assert_eq!((lib.width, lib.height), (w, h), "dims at {w}x{h}");
            assert_eq!(gamut.y(), lib.y.as_slice(), "Y at {w}x{h} q{quant_index}");
            assert_eq!(gamut.u(), lib.u.as_slice(), "U at {w}x{h} q{quant_index}");
            assert_eq!(gamut.v(), lib.v.as_slice(), "V at {w}x{h} q{quant_index}");
        }
    }
}

#[test]
fn gamut_lossy_loop_filter_deltas_match_libwebp_bit_exact() {
    // mb_lf_adjustments (RFC 6386 §9.4) are a decode path libwebp supports but cwebp never emits, so
    // the only way to pin gamut's *application* of them against the reference is to emit them from
    // gamut's encoder and require libwebp to decode the result identically to gamut's own decoder.
    // (An internal encode→decode round-trip can't: the encoder and decoder share `apply_loop_filter`,
    // so a bug there would cancel.) detailed_yuv forces B_PRED macroblocks, exercising the mode[0]
    // (B_PRED) delta alongside ref_frame[0].
    use common::libwebp_decode_yuv;
    use gamut_riff::write_simple_lossy;
    use gamut_webp::vp8::frame::{
        EncodeOptions, LoopFilterDeltas, decode_frame, encode_frame, encode_frame_filtered,
    };

    let cases = [
        LoopFilterDeltas {
            ref_frame: [12, 0, 0, 0],
            mode: [0; 4],
        }, // intra ref-frame delta only
        LoopFilterDeltas {
            ref_frame: [0; 4],
            mode: [10, 0, 0, 0],
        }, // B_PRED mode delta only
        LoopFilterDeltas {
            ref_frame: [-20, 0, 0, 0],
            mode: [0; 4],
        }, // negative: clamps levels toward 0
        LoopFilterDeltas {
            ref_frame: [8, 0, 0, 0],
            mode: [-12, 0, 0, 0],
        },
    ];

    // Guard: the deltas must actually change libwebp's decoded output, else a silently-dropped delta
    // would make the conformance assertions below vacuous.
    {
        let yuv = detailed_yuv(48, 48);
        let base = write_simple_lossy(
            &encode_frame(&yuv, 16)
                .expect("fixture fits the partition-size fields")
                .0,
        )
        .unwrap();
        let with = write_simple_lossy(
            &encode_frame_filtered(
                &yuv,
                16,
                EncodeOptions {
                    loop_filter_deltas: cases[0],
                    ..Default::default()
                },
            )
            .expect("fixture fits the partition-size fields")
            .0,
        )
        .unwrap();
        assert_ne!(
            libwebp_decode_yuv(&base).y,
            libwebp_decode_yuv(&with).y,
            "loop-filter deltas must change libwebp's decoded luma"
        );
    }

    for deltas in cases {
        for &(w, h) in &[(32u32, 32u32), (48, 48), (49, 33)] {
            for &q in &[16u8, 48] {
                let opts = EncodeOptions {
                    loop_filter_deltas: deltas,
                    ..Default::default()
                };
                let (payload, _) = encode_frame_filtered(&detailed_yuv(w, h), q, opts)
                    .expect("fixture fits the partition-size fields");
                let webp = write_simple_lossy(&payload).unwrap();
                let lib = libwebp_decode_yuv(&webp);
                let gamut = decode_frame(&payload).expect("gamut decode").to_yuv420();
                assert_eq!((lib.width, lib.height), (w, h), "dims at {w}x{h}");
                assert_eq!(gamut.y(), lib.y.as_slice(), "Y {deltas:?} at {w}x{h} q{q}");
                assert_eq!(gamut.u(), lib.u.as_slice(), "U {deltas:?} at {w}x{h} q{q}");
                assert_eq!(gamut.v(), lib.v.as_slice(), "V {deltas:?} at {w}x{h} q{q}");
            }
        }
    }
}

#[test]
fn gamut_decodes_patched_vp8_profiles_like_libwebp() {
    // VP8 profiles 1–3 (the 3-bit frame-tag version) are a decode path cwebp never emits. Patch the
    // version field (bits 1–3 of byte 0) of a gamut key frame and require gamut and libwebp to decode
    // it identically — pinning that the profile field does not alter intra key-frame reconstruction
    // (the explicit filter-type bit governs), matching the reference rather than the RFC's prose.
    use common::libwebp_decode_yuv;
    use gamut_riff::write_simple_lossy;
    use gamut_webp::vp8::frame::{decode_frame, encode_frame};

    for &(w, h) in &[(32u32, 32u32), (49, 33)] {
        let (payload, _) =
            encode_frame(&detailed_yuv(w, h), 24).expect("fixture fits the partition-size fields");
        for version in 1u8..=3 {
            let mut patched = payload.clone();
            patched[0] = (patched[0] & !0b1110) | (version << 1);
            let webp = write_simple_lossy(&patched).unwrap();
            let lib = libwebp_decode_yuv(&webp);
            let gamut = decode_frame(&patched).expect("gamut decode").to_yuv420();
            assert_eq!((lib.width, lib.height), (w, h), "dims v{version} {w}x{h}");
            assert_eq!(gamut.y(), lib.y.as_slice(), "Y v{version} at {w}x{h}");
            assert_eq!(gamut.u(), lib.u.as_slice(), "U v{version} at {w}x{h}");
            assert_eq!(gamut.v(), lib.v.as_slice(), "V v{version} at {w}x{h}");
        }
    }
}

/// Extracts the `VP8 ` (lossy) chunk payload from a RIFF/WebP file.
fn vp8_payload(webp: &[u8]) -> Vec<u8> {
    use gamut_riff::{RiffReader, WebpChunkId};
    RiffReader::new(webp)
        .expect("riff")
        .filter_map(Result::ok)
        .find(|c| matches!(WebpChunkId::from(c.fourcc), WebpChunkId::Vp8))
        .expect("VP8 chunk")
        .payload
        .to_vec()
}

#[test]
fn gamut_decodes_libwebp_lossy_bit_exact() {
    // The reverse-direction conformance gate: a real libwebp lossy encoder emits VP8 streams using its
    // own loop filter, segmentation, token-probability updates, and skip choices. gamut's native
    // decoder must reproduce libwebp's own YUV output bit-for-bit — proving it handles the full
    // feature surface a production encoder actually emits, not just gamut's own streams.
    use common::{libwebp_decode_yuv, libwebp_encode_lossy_rgba};

    for &(w, h) in &[
        (16u32, 16u32),
        (32, 32),
        (64, 48),
        (49, 33),
        (80, 17),
        (255, 3),
    ] {
        for q in [6.0f32, 35.0, 70.0, 100.0] {
            let rgba = pattern_rgba(w, h);
            let webp = libwebp_encode_lossy_rgba(&rgba, w, h, q);
            let gamut = gamut_webp::vp8::frame::decode_frame(&vp8_payload(&webp))
                .expect("gamut decode")
                .to_yuv420();
            let lib = libwebp_decode_yuv(&webp);
            assert_eq!((lib.width, lib.height), (w, h), "dims at {w}x{h}");
            assert_eq!(gamut.y(), lib.y.as_slice(), "Y at {w}x{h} q{q}");
            assert_eq!(gamut.u(), lib.u.as_slice(), "U at {w}x{h} q{q}");
            assert_eq!(gamut.v(), lib.v.as_slice(), "V at {w}x{h} q{q}");
        }
    }
}

#[test]
fn gamut_decodes_libwebp_lossy_realistic_and_large() {
    // The reverse direction at scale: libwebp lossy-encodes photographic RGBA over a lossy-appropriate
    // small matrix plus the full LARGE_DIMENSIONS (up to 1024×768), and gamut's decoder must reproduce
    // libwebp's own YUV bit-for-bit — pinning the decode paths (per-segment filter levels, probability
    // updates, partitioning) on frames far larger and more varied than the tiny synthetic inputs.
    use common::{libwebp_decode_yuv, libwebp_encode_lossy_rgba};

    let small = [
        (16u32, 16u32),
        (32, 32),
        (64, 48),
        (49, 33),
        (80, 17),
        (255, 3),
    ];
    for &(w, h) in small.iter().chain(LARGE_DIMENSIONS) {
        for q in [20.0f32, 80.0] {
            let rgba = photo_like_rgba(w, h, 0x1d0f);
            let webp = libwebp_encode_lossy_rgba(&rgba, w, h, q);
            let gamut = gamut_webp::vp8::frame::decode_frame(&vp8_payload(&webp))
                .expect("gamut decode")
                .to_yuv420();
            let lib = libwebp_decode_yuv(&webp);
            assert_eq!((lib.width, lib.height), (w, h), "dims at {w}x{h}");
            assert_eq!(gamut.y(), lib.y.as_slice(), "Y at {w}x{h} q{q}");
            assert_eq!(gamut.u(), lib.u.as_slice(), "U at {w}x{h} q{q}");
            assert_eq!(gamut.v(), lib.v.as_slice(), "V at {w}x{h} q{q}");
        }
    }
}

#[test]
fn gamut_decodes_libwebp_lossy_forced_features_bit_exact() {
    // The one-shot `WebPEncodeRGBA` the other reverse-direction tests use can't independently reach
    // the VP8 feature surface — it runs at a fixed method with the complex loop filter and libwebp's
    // default segmentation. Drive libwebp's *advanced* encoder to force each knob in turn (simple vs
    // complex filter, 1..=4 segments, low/high effort, filter strength, then several at once) and
    // require gamut's decoder to reproduce libwebp's own YUV bit-for-bit on every variant — pinning
    // the decoder against streams a production encoder can emit but cwebp's defaults rarely do.
    use common::{LibwebpLossyConfig, libwebp_decode_yuv, libwebp_encode_lossy_rgba_config};

    let base = LibwebpLossyConfig {
        quality: 75.0,
        filter_type: 1,
        segments: 1,
        method: 4,
        filter_strength: 60,
    };

    // Guard: prove the advanced config actually reaches the encoder. This is a *conformance* gate
    // (gamut must match whatever libwebp emits), so it would still pass if the forcing were silently
    // ignored — but then it would add nothing over the default-config reverse oracle. Require distinct
    // feature settings to produce distinct streams for the same input. (Token-partition count is *not*
    // guarded here: libwebp's encoder ignores `config.partitions` and always writes one partition —
    // see `LibwebpLossyConfig` — so that path is covered forward in `gamut_lossy_options_*` instead.)
    {
        let (w, h) = (64u32, 48);
        let rgba = photo_like_rgba(w, h, 0x00c0_ffee);
        let one = libwebp_encode_lossy_rgba_config(&rgba, w, h, &base);
        let simple = libwebp_encode_lossy_rgba_config(
            &rgba,
            w,
            h,
            &LibwebpLossyConfig {
                filter_type: 0,
                ..base
            },
        );
        assert_ne!(
            one, simple,
            "switching to the simple loop filter must change the stream"
        );
        let multi_seg = libwebp_encode_lossy_rgba_config(
            &rgba,
            w,
            h,
            &LibwebpLossyConfig {
                segments: 4,
                ..base
            },
        );
        assert_ne!(
            one, multi_seg,
            "forcing four segments must change the stream"
        );
    }

    let mut cases: Vec<(String, LibwebpLossyConfig)> = Vec::new();
    for filter_type in [0, 1] {
        cases.push((
            format!("filter_type={filter_type}"),
            LibwebpLossyConfig {
                filter_type,
                ..base
            },
        ));
    }
    for segments in [1, 2, 3, 4] {
        cases.push((
            format!("segments={segments}"),
            LibwebpLossyConfig { segments, ..base },
        ));
    }
    for method in [0, 3, 6] {
        cases.push((
            format!("method={method}"),
            LibwebpLossyConfig { method, ..base },
        ));
    }
    for filter_strength in [0, 30] {
        cases.push((
            format!("filter_strength={filter_strength}"),
            LibwebpLossyConfig {
                filter_strength,
                ..base
            },
        ));
    }
    cases.push((
        "combined".into(),
        LibwebpLossyConfig {
            filter_type: 0,
            segments: 4,
            method: 6,
            filter_strength: 20,
            ..base
        },
    ));

    // (128, 96) spans six MB rows, exercising per-segment filter levels across many rows.
    for &(w, h) in &[(32u32, 32u32), (64, 48), (49, 33), (128, 96)] {
        let rgba = photo_like_rgba(w, h, 0x00c0_ffee);
        for (label, cfg) in &cases {
            let webp = libwebp_encode_lossy_rgba_config(&rgba, w, h, cfg);
            let gamut = gamut_webp::vp8::frame::decode_frame(&vp8_payload(&webp))
                .expect("gamut decode")
                .to_yuv420();
            let lib = libwebp_decode_yuv(&webp);
            assert_eq!((lib.width, lib.height), (w, h), "dims {label} at {w}x{h}");
            assert_eq!(gamut.y(), lib.y.as_slice(), "Y {label} at {w}x{h}");
            assert_eq!(gamut.u(), lib.u.as_slice(), "U {label} at {w}x{h}");
            assert_eq!(gamut.v(), lib.v.as_slice(), "V {label} at {w}x{h}");
        }
    }
}

#[test]
fn gamut_rgb_to_yuv_matches_libwebp_limited_range() {
    // The color-conversion gate. WebP/VP8 is *limited-range* BT.601, and gamut-color now matches
    // libwebp's per-pixel RGB→YUV (src/dsp/yuv.h `VP8RGBToY/U/V`) exactly: the luma plane is
    // bit-exact, and chroma differs by ≤2 only because gamut box-averages 2×2 before converting while
    // libwebp sums-then-converts (a rounding-order difference). A regression to full-range — the bug
    // this fix closed — would shift luma by ~17 and fail the `assert_eq` below.
    use common::libwebp_rgba_to_yuv;
    use gamut_color::{ColorRange, Yuv420};

    let chroma_max = |a: &[u8], b: &[u8]| {
        a.iter()
            .zip(b)
            .map(|(x, y)| (i32::from(*x) - i32::from(*y)).abs())
            .max()
            .unwrap_or(0)
    };
    for &(w, h) in DIMENSIONS.iter().chain(&[(128u32, 96u32), (300, 70)]) {
        let rgba = photo_like_rgba(w, h, 0x5eed);
        let gamut =
            Yuv420::from_rgb8(&rgba_to_rgb(&rgba), w, h, ColorRange::Limited).expect("from_rgb8");
        let lib = libwebp_rgba_to_yuv(&rgba, w, h);
        assert_eq!(
            gamut.y(),
            lib.y.as_slice(),
            "luma must be bit-exact at {w}x{h}"
        );
        assert!(chroma_max(gamut.u(), &lib.u) <= 2, "U within 2 at {w}x{h}");
        assert!(chroma_max(gamut.v(), &lib.v) <= 2, "V within 2 at {w}x{h}");
    }
}

#[test]
fn gamut_lossy_webp_decodes_correctly_in_libwebp() {
    // End-to-end interop regression guard: gamut encodes lossy WebP, libwebp (the browser-equivalent
    // decoder) decodes it, and the result must match the source within the lossy budget — *not* the
    // systematic per-sample colour shift the old full-range encoding produced in every standard
    // decoder. Mean absolute error is the robust metric (lossy edges spike the max).
    for &(w, h) in &[(64u32, 48u32), (128, 96), (49, 33)] {
        let rgb = rgba_to_rgb(&photo_like_rgba(w, h, 0x1cef));
        let mut webp = Vec::new();
        WebpEncoder::lossy(90)
            .encode_image(
                ImageRef::<Rgb8>::new(
                    &rgb,
                    Dimensions {
                        width: w,
                        height: h,
                    },
                )
                .unwrap(),
                &mut webp,
            )
            .expect("gamut encode");
        let lib = rgba_to_rgb(&libwebp_decode_rgba(&webp).rgba);
        let mae: f64 = lib
            .iter()
            .zip(&rgb)
            .map(|(a, b)| f64::from((i32::from(*a) - i32::from(*b)).unsigned_abs()))
            .sum::<f64>()
            / rgb.len() as f64;
        assert!(
            mae <= 8.0,
            "gamut→libwebp interop MAE {mae:.2} too high at {w}x{h} (colour shift regression?)"
        );
    }
}

#[test]
fn gamut_decodes_libwebp_lossy_close_to_libwebp() {
    // The decode direction: gamut and libwebp must decode the *same* libwebp-encoded stream to close
    // RGB. The per-pixel YUV→RGB inverse is bit-exact with libwebp (pinned in gamut-color's
    // `limited_range_matches_libwebp_anchors` unit test); the residual here is chroma *upsampling*
    // (gamut nearest-replicates, libwebp uses fancy bilinear) — a quality choice (issue #32), not a
    // colour error. A regression of the inverse back to full-range would blow well past this bound.
    use common::libwebp_encode_lossy_rgba;

    let max_abs = |a: &[u8], b: &[u8]| {
        a.iter()
            .zip(b)
            .map(|(x, y)| (i32::from(*x) - i32::from(*y)).abs())
            .max()
            .unwrap_or(0)
    };
    for &(w, h) in &[(64u32, 48u32), (128, 96), (49, 33)] {
        let rgba = photo_like_rgba(w, h, 0x2ab0);
        let webp = libwebp_encode_lossy_rgba(&rgba, w, h, 90.0);
        let gamut: ImageBuf<Rgb8> = WebpDecoder::new()
            .decode_image(&webp)
            .expect("gamut decode");
        let lib = rgba_to_rgb(&libwebp_decode_rgba(&webp).rgba);
        assert!(
            max_abs(gamut.as_samples(), &lib) <= 24,
            "gamut vs libwebp decode differs by >24 at {w}x{h} (more than chroma upsampling)"
        );
    }
}

#[test]
fn libwebp_decodes_gamut_lossy_alpha_exactly() {
    // gamut encodes lossy color plus a raw `ALPH` alpha plane in an extended (`VP8X`) file; libwebp
    // must recover the exact alpha (alpha is lossless). The lossy color is not compared.
    use common::libwebp_decode_rgba;

    for &(w, h) in &[(16u32, 16u32), (32, 24), (17, 9), (49, 33)] {
        let rgba: Vec<u8> = (0..(w * h) as usize)
            .flat_map(|i| {
                let (x, y) = (i as u32 % w, i as u32 / w);
                [
                    (x * 7) as u8,
                    (y * 9) as u8,
                    (x ^ y) as u8,
                    ((x * 11 + y * 5) & 0xff) as u8,
                ]
            })
            .collect();
        let mut file = Vec::new();
        WebpEncoder::lossy(70)
            .encode_image(
                ImageRef::<Rgba8>::new(
                    &rgba,
                    Dimensions {
                        width: w,
                        height: h,
                    },
                )
                .unwrap(),
                &mut file,
            )
            .expect("gamut rgba encode");
        let decoded = libwebp_decode_rgba(&file);
        assert_eq!((decoded.width, decoded.height), (w, h), "dims at {w}x{h}");
        let lib_alpha: Vec<u8> = decoded
            .rgba
            .as_chunks::<4>()
            .0
            .iter()
            .map(|p| p[3])
            .collect();
        let src_alpha: Vec<u8> = rgba.as_chunks::<4>().0.iter().map(|p| p[3]).collect();
        assert_eq!(
            lib_alpha, src_alpha,
            "libwebp must recover gamut's exact alpha at {w}x{h}"
        );
    }
}

#[test]
fn gamut_decodes_libwebp_lossy_alpha_exactly() {
    // libwebp encodes lossy color plus a *compressed* (C=1) `ALPH` plane (what cwebp emits); gamut's
    // decoder must recover the exact alpha — the reverse-direction gate for the lossless-alpha path.
    use common::libwebp_encode_lossy_rgba;

    for &(w, h) in &[(16u32, 16u32), (32, 24), (49, 33), (80, 17)] {
        let rgba: Vec<u8> = (0..(w * h) as usize)
            .flat_map(|i| {
                let (x, y) = (i as u32 % w, i as u32 / w);
                [
                    (x * 7) as u8,
                    (y * 9) as u8,
                    (x ^ y) as u8,
                    ((x * 11 + y * 5) & 0xff) as u8,
                ]
            })
            .collect();
        let webp = libwebp_encode_lossy_rgba(&rgba, w, h, 75.0);
        let got: ImageBuf<Rgba8> = WebpDecoder::new()
            .decode_image(&webp)
            .expect("gamut decode libwebp lossy+alpha");
        assert_eq!(
            got.dimensions(),
            Dimensions {
                width: w,
                height: h
            }
        );
        let dec_alpha: Vec<u8> = got
            .as_samples()
            .as_chunks::<4>()
            .0
            .iter()
            .map(|p| p[3])
            .collect();
        let src_alpha: Vec<u8> = rgba.as_chunks::<4>().0.iter().map(|p| p[3]).collect();
        assert_eq!(
            dec_alpha, src_alpha,
            "gamut must recover libwebp's exact alpha at {w}x{h}"
        );
    }
}

#[test]
fn libwebp_reads_gamut_metadata_chunks_and_flags() {
    // Forward direction for the metadata surface: gamut embeds `ICCP`/`EXIF`/`XMP `, and libwebp's own
    // muxer must recover every payload byte-for-byte and decode the same `VP8X` feature flags.
    //
    // `WebPMuxCreate` also *validates* the container (muxinternal.c `MuxValidate`): it rejects a file
    // whose feature flags disagree with the chunks actually present, or whose reconstruction chunks
    // are out of order. So this pins gamut's flag bookkeeping and chunk ordering against the
    // reference implementation, not just the payload bytes.
    use common::libwebp_mux_read_metadata;

    let icc: Vec<u8> = (0..512u32).map(|i| (i % 251) as u8).collect();
    let exif: &[u8] = b"II\x2a\x00\x08\x00\x00\x00gamut-exif";
    let xmp: &[u8] = b"<?xpacket begin='\xef\xbb\xbf'?><x:xmpmeta/><?xpacket end='w'?>";
    let (w, h) = (48u32, 32u32);
    let dims = Dimensions {
        width: w,
        height: h,
    };
    let rgba = photo_like_rgba(w, h, 7);

    for (label, encoder) in [
        ("lossless", WebpEncoder::lossless()),
        ("lossy", WebpEncoder::lossy(80)),
    ] {
        // Opaque RGB input: only the three metadata flags should be set.
        let rgb = rgba_to_rgb(&rgba);
        let mut file = Vec::new();
        encoder
            .clone()
            .with_icc_profile(&icc)
            .with_exif(exif)
            .with_xmp(xmp)
            .encode_image(ImageRef::<Rgb8>::new(&rgb, dims).unwrap(), &mut file)
            .expect("encode with metadata");

        let read = libwebp_mux_read_metadata(&file);
        assert_eq!(read.icc.as_deref(), Some(icc.as_slice()), "{label}: ICCP");
        assert_eq!(read.exif.as_deref(), Some(exif), "{label}: EXIF");
        assert_eq!(read.xmp.as_deref(), Some(xmp), "{label}: XMP");
        assert_eq!(
            read.feature_flags,
            libwebp_sys::ICCP_FLAG | libwebp_sys::EXIF_FLAG | libwebp_sys::XMP_FLAG,
            "{label}: libwebp must see exactly the three metadata features"
        );

        // libwebp must still decode the promoted (extended) container, to exactly the pixels it gets
        // from the same encode without metadata — the promotion touches the container, not the
        // codestream.
        let mut plain = Vec::new();
        encoder
            .encode_image(ImageRef::<Rgb8>::new(&rgb, dims).unwrap(), &mut plain)
            .expect("encode without metadata");
        let decoded = libwebp_decode_rgba(&file);
        assert_eq!((decoded.width, decoded.height), (w, h), "{label}: canvas");
        assert_eq!(
            decoded.rgba,
            libwebp_decode_rgba(&plain).rgba,
            "{label}: metadata must not disturb the pixels libwebp decodes"
        );
    }
}

#[test]
fn libwebp_reads_gamut_metadata_alongside_alpha() {
    // A transparent lossy image adds `ALPH` *inside* the image data, between `ICCP` and the
    // bitstream. libwebp's muxer is the arbiter of that layout: it rejects a mis-ordered file, and its
    // feature flags must show alpha next to the metadata.
    use common::libwebp_mux_read_metadata;

    let (w, h) = (32u32, 24u32);
    let rgba: Vec<u8> = (0..(w * h) as usize)
        .flat_map(|i| {
            let (x, y) = (i as u32 % w, i as u32 / w);
            [
                (x * 7) as u8,
                (y * 9) as u8,
                (x ^ y) as u8,
                ((x * 11 + y * 5) & 0xff) as u8,
            ]
        })
        .collect();
    let dims = Dimensions {
        width: w,
        height: h,
    };
    let exif: &[u8] = b"II\x2a\x00\x08\x00\x00\x00alpha-and-exif";
    let mut file = Vec::new();
    WebpEncoder::lossy(75)
        .with_exif(exif)
        .encode_image(ImageRef::<Rgba8>::new(&rgba, dims).unwrap(), &mut file)
        .expect("encode transparent lossy with metadata");

    let read = libwebp_mux_read_metadata(&file);
    assert_eq!(read.exif.as_deref(), Some(exif));
    assert_eq!(
        read.feature_flags,
        libwebp_sys::ALPHA_FLAG | libwebp_sys::EXIF_FLAG
    );
    // And the alpha still survives the round-trip through gamut's own decoder.
    let got: ImageBuf<Rgba8> = WebpDecoder::new().decode_image(&file).expect("decode");
    let dec_alpha: Vec<u8> = got
        .as_samples()
        .as_chunks::<4>()
        .0
        .iter()
        .map(|p| p[3])
        .collect();
    let src_alpha: Vec<u8> = rgba.as_chunks::<4>().0.iter().map(|p| p[3]).collect();
    assert_eq!(dec_alpha, src_alpha);
}

#[test]
fn gamut_reads_metadata_libwebp_embedded() {
    // Reverse direction: libwebp encodes the image and its muxer attaches the metadata — the exact
    // path `cwebp -metadata all` takes — so this is a reference-produced fixture. gamut must recover
    // every payload byte-for-byte and still decode the pixels bit-exactly.
    use common::{libwebp_mux_set_metadata, pattern_rgba};

    let icc: Vec<u8> = (0..1024u32).map(|i| (i % 241) as u8).collect();
    let exif: &[u8] = b"MM\x00\x2a\x00\x00\x00\x08libwebp-exif";
    let xmp: &[u8] = b"<x:xmpmeta xmlns:x='adobe:ns:meta/'></x:xmpmeta>";

    for &(w, h) in &[(1u32, 1u32), (17, 9), (64, 48)] {
        let rgba = pattern_rgba(w, h);
        let plain = libwebp_encode_lossless_rgba(&rgba, w, h);
        let tagged = libwebp_mux_set_metadata(&plain, Some(&icc), Some(exif), Some(xmp));

        let meta = gamut_webp::metadata(&tagged).expect("read libwebp-muxed metadata");
        assert_eq!(meta.icc.as_deref(), Some(icc.as_slice()), "ICCP at {w}x{h}");
        assert_eq!(meta.exif.as_deref(), Some(exif), "EXIF at {w}x{h}");
        assert_eq!(meta.xmp.as_deref(), Some(xmp), "XMP at {w}x{h}");

        let got: ImageBuf<Rgba8> = WebpDecoder::new()
            .decode_image(&tagged)
            .expect("gamut decode libwebp-muxed file");
        assert_eq!(
            got.dimensions(),
            Dimensions {
                width: w,
                height: h
            }
        );
        assert_eq!(
            got.as_samples(),
            rgba.as_slice(),
            "metadata chunks must not disturb the lossless pixels at {w}x{h}"
        );

        // Only the chunks libwebp was asked to attach are reported: a subset stays a subset.
        let icc_only = libwebp_mux_set_metadata(&plain, Some(&icc), None, None);
        let meta = gamut_webp::metadata(&icc_only).expect("read ICCP-only file");
        assert_eq!(meta.icc.as_deref(), Some(icc.as_slice()));
        assert_eq!(meta.exif, None);
        assert_eq!(meta.xmp, None);
    }
}

#[test]
fn libwebp_decodes_every_effort_level_bit_exactly() {
    // The ladder's conformance gate. Each rung changes what the encoder emits — 4x4 search on or
    // off, derived coefficient probabilities, a measured skip probability, a dead-zone quantizer —
    // and every one of those must still produce a stream libwebp reconstructs identically to
    // gamut's own decoder. Without this, a rung could quietly ship a stream only gamut can read.
    //
    // Swept over efforts x sizes x quantizers rather than crossed with the full dimension matrices
    // the other tests use: each rung needs conformance coverage, not conformance coverage of every
    // shape, and the combinatorial version would dominate the suite's runtime.
    use common::libwebp_decode_yuv;
    use gamut_riff::write_simple_lossy;
    use gamut_webp::Effort;
    use gamut_webp::vp8::frame::{EncodeOptions, decode_frame, encode_frame_filtered};

    for level in 0..=6u8 {
        let effort = Effort::from_level(level).expect("0..=6");
        let opts = EncodeOptions {
            effort,
            ..EncodeOptions::default()
        };
        for &(w, h) in &[(32u32, 32u32), (49, 33)] {
            for &q in &[12u8, 48] {
                for (kind, yuv) in [
                    ("detailed", detailed_yuv(w, h)),
                    ("photo", photo_like_yuv(w, h, 7)),
                ] {
                    let (payload, _) = encode_frame_filtered(&yuv, q, opts)
                        .expect("fixture fits the partition-size fields");
                    let webp = write_simple_lossy(&payload).unwrap();
                    let lib = libwebp_decode_yuv(&webp);
                    let gamut = decode_frame(&payload).expect("gamut decode").to_yuv420();
                    assert_eq!(
                        gamut.y(),
                        lib.y.as_slice(),
                        "effort {level} {kind} Y {w}x{h} q{q}"
                    );
                    assert_eq!(
                        gamut.u(),
                        lib.u.as_slice(),
                        "effort {level} {kind} U {w}x{h} q{q}"
                    );
                    assert_eq!(
                        gamut.v(),
                        lib.v.as_slice(),
                        "effort {level} {kind} V {w}x{h} q{q}"
                    );
                }
            }
        }
    }
}

#[test]
fn libwebp_decodes_every_effort_levels_lossless_and_alpha() {
    // The container-level companion: lossless streams and the `ALPH` chunk at every rung, checked
    // through libwebp's own decoder rather than gamut's, so a rung that produced a stream only
    // gamut could read would fail here.
    use common::{libwebp_decode_rgba, pattern_rgba};
    use gamut_core::{EncodeImage, ImageRef, Rgba8};
    use gamut_webp::{Effort, WebpEncoder};

    let (w, h) = (48u32, 32u32);
    let rgba = pattern_rgba(w, h);
    let dims = gamut_core::Dimensions {
        width: w,
        height: h,
    };
    for level in 0..=6u8 {
        let effort = Effort::from_level(level).expect("0..=6");
        let mut lossless = Vec::new();
        WebpEncoder::lossless()
            .with_effort(effort)
            .encode_image(
                ImageRef::<Rgba8>::new(&rgba, dims).expect("fixture"),
                &mut lossless,
            )
            .expect("lossless encode");
        let decoded = libwebp_decode_rgba(&lossless);
        assert_eq!(
            decoded.rgba, rgba,
            "libwebp did not recover the source losslessly at effort {level}"
        );

        let mut lossy = Vec::new();
        WebpEncoder::lossy(70)
            .with_effort(effort)
            .encode_image(
                ImageRef::<Rgba8>::new(&rgba, dims).expect("fixture"),
                &mut lossy,
            )
            .expect("lossy encode");
        let decoded = libwebp_decode_rgba(&lossy);
        let got: Vec<u8> = decoded
            .rgba
            .as_chunks::<4>()
            .0
            .iter()
            .map(|p| p[3])
            .collect();
        let want: Vec<u8> = rgba.as_chunks::<4>().0.iter().map(|p| p[3]).collect();
        assert_eq!(
            got, want,
            "libwebp did not recover the alpha plane exactly at effort {level}"
        );
    }
}
