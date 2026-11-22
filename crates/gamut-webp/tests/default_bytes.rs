//! Byte-identical-default regression: with no backend pushed, every encode path must produce
//! exactly the bytes the built-in `vp8`/`vp8l` tails produced before the backend registries landed
//! (issue #275), and every decode path must produce exactly the same pixels.
//!
//! The fixtures are pinned by length + FNV-1a-64 digest (and, for the smallest case, the full byte
//! vector), so any change to the default output — including one that merely re-orders chunks — fails
//! here rather than silently shipping. The digests were captured from the pre-change encoder.
//!
//! Re-pinning is deliberate, and every re-pin is a commit that says why. The issue-#31 density work
//! moved these twice: first when the encoder began choosing the smallest of the spec's prefix-code
//! description encodings, then when the effort ladder began racing candidate encodings against each
//! other. Cumulatively, for the same pixels:
//!
//! - lossless RGB 696 → 182 bytes, lossless RGBA 690 → 178 (−74%);
//! - the transparent lossy file 932 → 186, because its `ALPH` chunk — a green-only image — now
//!   reaches a transform chain suited to it instead of one that decorrelates channels it does not
//!   have.
//!
//! The lossy digests moved once too, when the VP8 effort ladder landed (issue #32): the default
//! rung now derives the frame's coefficient probabilities from what it actually coded, measures the
//! skip probability instead of guessing it from the quantizer, and quantizes with a dead zone.
//! `effort_two_still_reproduces_the_historical_lossy_bytes` keeps the pre-ladder bytes pinned at the
//! rung that still produces them, which is what made the two-pass restructure checkable as a pure
//! refactor.
//!
//! They moved again with issue #1204, when `quality` began following libwebp's
//! `QualityToCompression` curve instead of a straight line: quality 60 now codes at base quantizer
//! 33 rather than 50, so each lossy file is larger and closer to its source (lossy RGB 134 → 152
//! bytes, transparent lossy 184 → 202, the 16×16 opaque file 58 → 62). The historical anchor asks
//! for quality 33, which lands on the old quantizer 50, and its bytes did not move.
//!
//! Issue #1017 moved them once more by turning on libwebp's spatial noise shaping at its default
//! strength 50: up to four quantizer segments and the chroma quantizer deltas (lossy RGB 152 → 158
//! bytes, transparent lossy 192 → 204, the opaque file 62 → 64). Every pre-#1017 pin is kept
//! beside its successor with `with_sns_strength(0)`, which reproduces the old bytes exactly.

use gamut_core::convert::{AlphaPolicy, ConvertPolicy};
use gamut_core::{DecodeImage, Dimensions, EncodeImage, ImageBuf, ImageRef, Rgb8, Rgba8};
use gamut_webp::{
    ChromaUpsampling, Effort, Exact, LosslessPreset, WebpConfig, WebpDecoder, WebpEncoder,
};

/// FNV-1a (64-bit) over `bytes` — a dependency-free digest for pinning fixture bytes.
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

fn dims(width: u32, height: u32) -> Dimensions {
    Dimensions::new(width, height).unwrap()
}

/// A deterministic RGB gradient.
fn rgb_fixture(w: u32, h: u32) -> Vec<u8> {
    (0..w * h)
        .flat_map(|i| {
            let (x, y) = (i % w, i / w);
            [(x * 7) as u8, (y * 11) as u8, (x ^ y) as u8]
        })
        .collect()
}

/// A deterministic RGBA gradient with non-trivial alpha.
fn rgba_fixture(w: u32, h: u32) -> Vec<u8> {
    (0..w * h)
        .flat_map(|i| {
            let (x, y) = (i % w, i / w);
            [
                (x * 7) as u8,
                (y * 11) as u8,
                (x ^ y) as u8,
                ((x * 5 + y * 3) & 0xff) as u8,
            ]
        })
        .collect()
}

fn encode_rgb(enc: &WebpEncoder, px: &[u8], d: Dimensions) -> Vec<u8> {
    let mut out = Vec::new();
    enc.encode_image(ImageRef::<Rgb8>::new(px, d).expect("rgb fixture"), &mut out)
        .expect("encode");
    out
}

fn encode_rgba(enc: &WebpEncoder, px: &[u8], d: Dimensions) -> Vec<u8> {
    let mut out = Vec::new();
    enc.encode_image(
        ImageRef::<Rgba8>::new(px, d).expect("rgba fixture"),
        &mut out,
    )
    .expect("encode");
    out
}

/// Asserts the exact length and digest of a fixture's bytes.
fn assert_bytes(what: &str, bytes: &[u8], len: usize, digest: u64) {
    assert_eq!(bytes.len(), len, "{what}: length changed");
    assert_eq!(fnv1a64(bytes), digest, "{what}: bytes changed");
}

#[test]
fn lossless_rgb_default_bytes_are_unchanged() {
    let file = encode_rgb(&WebpEncoder::lossless(), &rgb_fixture(24, 16), dims(24, 16));
    assert_bytes("lossless rgb", &file, 182, 0x8e85_b274_9b43_e196);
}

#[test]
fn lossless_rgba_default_bytes_are_unchanged() {
    let file = encode_rgba(
        &WebpEncoder::lossless(),
        &rgba_fixture(24, 16),
        dims(24, 16),
    );
    assert_bytes("lossless rgba", &file, 178, 0xbc81_86a9_fd2e_b67b);
}

/// Lossless `quality` drives the VP8L search since issue #1240, and its default `75` at effort 4 is
/// the search the encoder ran before, so the same pins hold when both are spelled out — directly
/// or as `cwebp -z 6`, the preset with that pair.
#[test]
fn lossless_quality_75_at_effort_4_keeps_the_default_bytes() {
    let mut config = WebpConfig::default();
    config.quality = 75;
    config.effort = Effort::Default;
    for (what, config) in [
        ("explicit quality 75", config),
        (
            "preset z6",
            WebpConfig::default().with_lossless_preset(LosslessPreset::Z6),
        ),
    ] {
        let enc = WebpEncoder::from_config(config);
        let rgb = encode_rgb(&enc, &rgb_fixture(24, 16), dims(24, 16));
        assert_bytes(what, &rgb, 182, 0x8e85_b274_9b43_e196);
        let rgba = encode_rgba(&enc, &rgba_fixture(24, 16), dims(24, 16));
        assert_bytes(what, &rgba, 178, 0xbc81_86a9_fd2e_b67b);
    }
}

#[test]
fn lossy_rgb_default_bytes_are_unchanged() {
    // Issue #1017 moved the default: a lossy encode now runs libwebp's spatial noise shaping at
    // strength 50 (152 → 158 bytes here, the segment map and the finer chroma DC quantizer). SNS
    // off still reproduces the pre-#1017 bytes, which is what shows SNS is the only thing that
    // moved.
    let file = encode_rgb(&WebpEncoder::lossy(60), &rgb_fixture(32, 24), dims(32, 24));
    assert_bytes("lossy rgb", &file, 158, 0x5753_e084_9c32_49d2);
    let plain = WebpEncoder::lossy(60).with_sns_strength(0);
    let file = encode_rgb(&plain, &rgb_fixture(32, 24), dims(32, 24));
    assert_bytes("lossy rgb without sns", &file, 152, 0x9408_de4e_61f6_9872);
}

#[test]
fn lossy_rgba_extended_default_bytes_are_unchanged() {
    // The transparent path: VP8X + ALPH + VP8 . `ALPH` stays container-side, so its bytes are
    // pinned here too.
    //
    // Issue #1207 moved the default: a lossy encode now cleans the colour under `alpha = 0` first
    // (libwebp's `exact = 0`), which flattens the fixture's transparent luma (202 → 192 bytes).
    // `Exact::Preserve` still reproduces the pre-#1207 bytes, which is what shows the cleanup is the
    // only thing that moved.
    //
    // Issue #1017's spatial noise shaping moved it again (192 → 204); the `Exact::Preserve` row
    // runs with SNS off, so it still pins the pre-#1207 file.
    let file = encode_rgba(&WebpEncoder::lossy(60), &rgba_fixture(32, 24), dims(32, 24));
    assert_bytes("lossy rgba extended", &file, 204, 0x9b58_223e_004c_b63e);
    let exact = WebpEncoder::lossy(60)
        .with_exact(Exact::Preserve)
        .with_sns_strength(0);
    let file = encode_rgba(&exact, &rgba_fixture(32, 24), dims(32, 24));
    assert_bytes(
        "lossy rgba extended exact",
        &file,
        202,
        0x519a_6ee2_7660_1fb4,
    );
}

#[test]
fn lossy_rgba_opaque_default_bytes_are_unchanged() {
    // Small enough to pin every byte of the file, container header included. Issue #1017's
    // spatial noise shaping grew it 62 → 64 bytes: one flat macroblock is one segment, so what
    // moved is the frame's chroma quantizer deltas.
    const OPAQUE: [u8; 64] = [
        0x52, 0x49, 0x46, 0x46, 0x38, 0x00, 0x00, 0x00, 0x57, 0x45, 0x42, 0x50, 0x56, 0x50, 0x38,
        0x20, 0x2b, 0x00, 0x00, 0x00, 0xd0, 0x01, 0x00, 0x9d, 0x01, 0x2a, 0x10, 0x00, 0x10, 0x00,
        0x04, 0x00, 0x42, 0x25, 0xa2, 0x02, 0x74, 0xf4, 0x6e, 0xf9, 0x70, 0xd0, 0x35, 0x00, 0xfe,
        0xca, 0xc3, 0x6f, 0xff, 0x2c, 0xff, 0xfd, 0x96, 0xff, 0xb2, 0xde, 0x33, 0xf0, 0x24, 0xad,
        0x59, 0x84, 0x00, 0x00,
    ];
    let px = [120u8, 60, 200, 0xff].repeat(16 * 16);
    let file = encode_rgba(&WebpEncoder::lossy(60), &px, dims(16, 16));
    assert_eq!(file.as_slice(), OPAQUE.as_slice());
}

#[test]
fn default_decode_output_is_unchanged() {
    // Re-pinned once, deliberately, by issue #1016: the default lossy decode now upsamples chroma
    // with libwebp's fancy upsampler (byte-identical to libwebp, gated in `tests/oracle.rs`). The
    // digests from before that change held under `ChromaUpsampling::Nearest`, which is what showed
    // the switch moved only the upsampler. Both rows were re-pinned by issue #1204, which re-coded
    // the files they decode at a finer quantizer. The transparent file is encoded with
    // `Exact::Preserve` since issue #1207, so it is still the very file these digests decode: this
    // pins the decoder, and the encoder's cleanup default is pinned above.
    //
    // Both files are encoded with SNS off since issue #1017 turned it on by default, for the same
    // reason: they are still the files these digests decode.
    let plain = WebpEncoder::lossy(60).with_sns_strength(0);
    let lossy = encode_rgb(&plain, &rgb_fixture(32, 24), dims(32, 24));
    let exact = plain.with_exact(Exact::Preserve);
    let ext = encode_rgba(&exact, &rgba_fixture(32, 24), dims(32, 24));
    let nearest = WebpDecoder::new().with_chroma_upsampling(ChromaUpsampling::Nearest);
    for (decoder, rgb_digest, rgba_digest) in [
        (
            WebpDecoder::new(),
            0x9831_b6e7_fd42_bef8,
            0x34f1_7011_1a16_f0f2,
        ),
        (nearest, 0x2882_b3e0_5228_16ef, 0x2a7b_fe1d_d054_bd01),
    ] {
        let rgb: ImageBuf<Rgb8> = decoder.decode_image(&lossy).expect("decode");
        assert_eq!(rgb.dimensions(), dims(32, 24));
        assert_bytes("lossy decode rgb", rgb.as_samples(), 2304, rgb_digest);

        let rgba: ImageBuf<Rgba8> = decoder.decode_image(&ext).expect("decode");
        assert_eq!(rgba.dimensions(), dims(32, 24));
        assert_bytes("extended decode rgba", rgba.as_samples(), 3072, rgba_digest);
    }
}

#[test]
fn effort_two_still_reproduces_the_historical_lossy_bytes() {
    // `Effort::Fast` (level 2) is pinned to the pre-ladder VP8 toolset, which is what made the
    // two-pass restructure checkable: splitting the encoder into a decision pass and a writing
    // pass must not change a single bit, and the digests below are the ones this file carried
    // before either the restructure or the ladder existed.
    //
    // Keeping the anchor as a test rather than as a comment means a future change to the low rungs
    // has to be deliberate.
    //
    // Those digests were captured at quality 60, which the old linear mapping sent to base
    // quantizer 50. Since issue #1204 quality follows libwebp's curve, on which 33 is the quality
    // that lands on 50, so the anchor now asks for 33 and the bytes are untouched.
    //
    // Since issue #1017 the lossy default runs spatial noise shaping; the pre-SNS coder is
    // strength 0, so the anchor asks for that too.
    let e2 = WebpEncoder::lossy(33)
        .with_effort(Effort::Fast)
        .with_sns_strength(0);
    let rgb = encode_rgb(&e2, &rgb_fixture(32, 24), dims(32, 24));
    assert_bytes("lossy rgb @ effort 2", &rgb, 136, 0x2eb3_c30d_36b6_067e);

    const HISTORICAL_OPAQUE: [u8; 58] = [
        0x52, 0x49, 0x46, 0x46, 0x32, 0x00, 0x00, 0x00, 0x57, 0x45, 0x42, 0x50, 0x56, 0x50, 0x38,
        0x20, 0x26, 0x00, 0x00, 0x00, 0x70, 0x01, 0x00, 0x9d, 0x01, 0x2a, 0x10, 0x00, 0x10, 0x00,
        0x06, 0x40, 0x64, 0x08, 0x00, 0x2b, 0x82, 0x64, 0x34, 0xc7, 0xdc, 0xfe, 0x6d, 0x4c, 0x7f,
        0xff, 0x57, 0x7e, 0x76, 0xfd, 0x4f, 0x7b, 0x1f, 0xc1, 0x96, 0xdb, 0x04, 0x00,
    ];
    let px = [120u8, 60, 200, 0xff].repeat(16 * 16);
    let opaque = encode_rgba(&e2, &px, dims(16, 16));
    assert_eq!(opaque.as_slice(), HISTORICAL_OPAQUE.as_slice());
}

/// The policy set by `convert_policy` must reach the typed decode.
///
/// A file that genuinely carries transparency cannot be presented as RGB without discarding it, so
/// the default decoder refuses; naming an `AlphaPolicy` permits it. A decoder that dropped the
/// setter would refuse both times.
#[test]
fn convert_policy_reaches_the_typed_decode() {
    let dims = Dimensions::new(2, 2).unwrap();
    // Alpha varies, so the file is genuinely transparent rather than opaque-and-therefore-lossless.
    #[rustfmt::skip]
    let rgba = [
        0x10u8, 0x20, 0x30, 0x00,
        0x40,   0x50, 0x60, 0x80,
        0x70,   0x80, 0x90, 0xc0,
        0xa0,   0xb0, 0xc0, 0xff,
    ];
    let webp = WebpEncoder::lossless()
        .encode_to_vec(ImageRef::<Rgba8>::new(&rgba, dims).unwrap())
        .expect("encode");

    let refused = DecodeImage::<Rgb8>::decode_image(&WebpDecoder::new(), &webp)
        .expect_err("alpha must not be discarded silently");
    assert_eq!(refused.kind(), gamut_core::ErrorKind::Unsupported);

    let dropped: ImageBuf<Rgb8> = WebpDecoder::new()
        .with_convert_policy(ConvertPolicy::lossless().with_alpha(AlphaPolicy::Drop))
        .decode_image(&webp)
        .expect("drop decode");
    let expected: Vec<u8> = rgba
        .as_chunks::<4>()
        .0
        .iter()
        .flat_map(|p| [p[0], p[1], p[2]])
        .collect();
    assert_eq!(dropped.as_samples(), expected.as_slice());

    // Compositing is a different answer to the same question, so it must not agree with dropping
    // on the pixels that are actually transparent.
    let composited: ImageBuf<Rgb8> = WebpDecoder::new()
        .with_convert_policy(
            ConvertPolicy::lossless()
                .with_alpha(AlphaPolicy::CompositeOver)
                .with_background([u16::MAX; 3]),
        )
        .decode_image(&webp)
        .expect("composite decode");
    assert_eq!(&composited.as_samples()[0..3], &[255, 255, 255]);
    assert_ne!(composited.as_samples(), dropped.as_samples());
}
