//! Differential oracle (libwebp) for the WebP codecs through the public `WebpEncoder` /
//! `WebpDecoder` surface — lossless (VP8L), lossy (VP8), alpha and metadata.
//!
//! libwebp is the third-party reference, reached through `tooling/libwebp-oracle`:
//!   - the lossless round-trips (libwebp self-round-trip; gamut↔libwebp);
//!   - lossy in RGB/RGBA, bit-exact since issue #1016: the default chroma upsampling is libwebp's
//!     fancy upsampler, and `ChromaUpsampling::Nearest` matches libwebp with fancy upsampling off;
//!   - the `ALPH` plane and the `ICCP`/`EXIF`/`XMP ` chunks in both directions.
//!
//! The YUV-plane suite, which drives the VP8 frame coder's quantizer, filter, segmentation and
//! partition knobs directly, lives inline beside it in `src/vp8/frame/oracle_tests.rs`
//! (issue #1163).

use gamut_core::{DecodeImage, Dimensions, EncodeImage, ImageBuf, ImageRef, Rgb8, Rgba8};
use gamut_webp::{ChromaUpsampling, WebpDecoder, WebpEncoder};
use libwebp_oracle::fixtures::{LARGE_DIMENSIONS, pattern_rgba, photo_like_rgba};
use libwebp_oracle::{libwebp_decode_rgba, libwebp_encode_lossless_rgba, libwebp_get_info};

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
            assert_eq!(
                (dims.width(), dims.height()),
                (w, h),
                "{label} dims at {w}x{h}"
            );
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
            ImageRef::<Rgb8>::new(rgb, Dimensions::new(w, h).unwrap()).unwrap(),
            &mut webp,
        )
        .expect("gamut encode");
    let decoded = libwebp_decode_rgba(&webp);
    assert_eq!((decoded.width, decoded.height), (w, h), "dims for {label}");
    assert_eq!(rgba_to_rgb(&decoded.rgba), rgb, "pixels for {label}");
}

#[test]
fn libwebp_decodes_a_file_carrying_a_c2pa_store_to_the_source_pixels() {
    // A `C2PA` chunk (C2PA 2.4 §A.3.7) is not an RFC 9649 chunk, so to the reference decoder it is
    // an unknown chunk it "SHOULD ignore" (§2.7.1.6). Embedding a manifest store must therefore be
    // invisible to it: the same pixels out, and the same file as an unsigned encode with the store's
    // chunk appended — nothing ahead of it moved.
    let (w, h) = (32u32, 24u32);
    let rgb = rgba_to_rgb(&pattern_rgba(w, h));
    let image = || ImageRef::<Rgb8>::new(&rgb, Dimensions::new(w, h).unwrap()).unwrap();
    let store = &b"an opaque C2PA manifest store"[..];
    // Both files carry Exif, so both are already in the extended format and the only difference
    // between them is the store's own chunk rather than the `VP8X` promotion.
    let mut plain = Vec::new();
    WebpEncoder::lossless()
        .with_exif(b"exif payload")
        .encode_image(image(), &mut plain)
        .expect("gamut encode");
    let mut signed = Vec::new();
    let report = WebpEncoder::lossless()
        .with_exif(b"exif payload")
        .with_c2pa(store)
        .encode_with_report(image(), &mut signed)
        .expect("gamut encode");

    let decoded = libwebp_decode_rgba(&signed);
    assert_eq!(
        (decoded.width, decoded.height),
        (w, h),
        "libwebp reads the canvas"
    );
    assert_eq!(
        rgba_to_rgb(&decoded.rgba),
        rgb,
        "libwebp recovers the source pixels through the store"
    );

    // The store is the only thing added, and it is where the report says it is.
    let span = report.c2pa.expect("a store was configured");
    let span = span.start as usize..span.end as usize;
    assert_eq!(&signed[span.start..span.start + 4], b"C2PA");
    assert_eq!(&signed[span.start + 8..span.end], store);
    assert_eq!(
        signed.len() - plain.len(),
        span.len() + span.len() % 2,
        "only the store's chunk and its pad byte were added"
    );
    assert_eq!(
        signed[12..span.start],
        plain[12..],
        "everything ahead of the store is byte-identical"
    );
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

#[test]
fn gamut_rgb_to_yuv_matches_libwebp_limited_range() {
    // The color-conversion gate. WebP/VP8 is *limited-range* BT.601, and gamut-color now matches
    // libwebp's per-pixel RGB→YUV (src/dsp/yuv.h `VP8RGBToY/U/V`) exactly: the luma plane is
    // bit-exact, and chroma differs by ≤2 only because gamut box-averages 2×2 before converting while
    // libwebp sums-then-converts (a rounding-order difference). A regression to full-range — the bug
    // this fix closed — would shift luma by ~17 and fail the `assert_eq` below.
    use gamut_color::{ColorRange, Planar8};
    use libwebp_oracle::libwebp_rgba_to_yuv;

    let chroma_max = |a: &[u8], b: &[u8]| {
        a.iter()
            .zip(b)
            .map(|(x, y)| (i32::from(*x) - i32::from(*y)).abs())
            .max()
            .unwrap_or(0)
    };
    for &(w, h) in DIMENSIONS.iter().chain(&[(128u32, 96u32), (300, 70)]) {
        let rgba = photo_like_rgba(w, h, 0x5eed);
        let rgb = rgba_to_rgb(&rgba);
        let src =
            ImageRef::<Rgb8>::new(&rgb, Dimensions::new(w, h).expect("dims")).expect("rgb image");
        let gamut = Planar8::from_rgb8_bt601(src, ColorRange::Limited);
        let lib = libwebp_rgba_to_yuv(&rgba, w, h);
        assert_eq!(
            gamut.planes()[0],
            lib.y.as_slice(),
            "luma must be bit-exact at {w}x{h}"
        );
        assert!(
            chroma_max(gamut.planes()[1], &lib.u) <= 2,
            "U within 2 at {w}x{h}"
        );
        assert!(
            chroma_max(gamut.planes()[2], &lib.v) <= 2,
            "V within 2 at {w}x{h}"
        );
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
                ImageRef::<Rgb8>::new(&rgb, Dimensions::new(w, h).unwrap()).unwrap(),
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

/// The largest per-sample difference between two equal-length buffers.
fn max_abs(a: &[u8], b: &[u8]) -> u8 {
    assert_eq!(a.len(), b.len(), "buffers differ in length");
    a.iter()
        .zip(b)
        .map(|(x, y)| x.abs_diff(*y))
        .max()
        .unwrap_or(0)
}

#[test]
fn gamut_decodes_libwebp_lossy_rgb_bit_exact() {
    // The decode direction in RGB: gamut and libwebp decode the *same* libwebp-encoded stream to
    // identical RGB. The planes already match bit for bit (above), the YUV→RGB inverse is
    // libwebp's own (gamut-color's `limited_range_matches_libwebp_anchors`), and the default chroma
    // upsampling is libwebp's fancy upsampler (`gamut_dsp::upsample::libwebp`, issue #1016). The
    // dimensions cover odd sizes, single rows and columns, and the 1×1 and 2×2 edge cases. Before
    // #1016 gamut replicated chroma and this was bounded at 24.
    let mut sizes = DIMENSIONS.to_vec();
    sizes.extend_from_slice(&[(64, 48), (128, 96), (49, 33), (3, 3), (4, 5)]);
    for (w, h) in sizes {
        let rgba = photo_like_rgba(w, h, 0x2ab0 ^ w ^ (h << 8));
        let webp = libwebp_oracle::libwebp_encode_lossy_rgba(&rgba, w, h, 90.0);
        let gamut: ImageBuf<Rgb8> = WebpDecoder::new()
            .decode_image(&webp)
            .expect("gamut decode");
        let lib = rgba_to_rgb(&libwebp_decode_rgba(&webp).rgba);
        let max = max_abs(gamut.as_samples(), &lib);
        assert_eq!(max, 0, "gamut vs libwebp RGB at {w}x{h}: max-diff {max}");
    }
}

#[test]
fn gamut_decodes_libwebp_lossy_alpha_rgba_bit_exact() {
    // The `ALPH` path presents RGBA through the same upsampler: colour and alpha both match
    // libwebp's default RGBA decode.
    for &(w, h) in &[(16u32, 16u32), (49, 33), (80, 17), (1, 7)] {
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
        let webp = libwebp_oracle::libwebp_encode_lossy_rgba(&rgba, w, h, 75.0);
        let got: ImageBuf<Rgba8> = WebpDecoder::new()
            .decode_image(&webp)
            .expect("gamut decode");
        let lib = libwebp_decode_rgba(&webp).rgba;
        let max = max_abs(got.as_samples(), &lib);
        assert_eq!(max, 0, "gamut vs libwebp RGBA at {w}x{h}: max-diff {max}");
    }
}

#[test]
fn nearest_decode_matches_libwebp_without_fancy_upsampling() {
    // `ChromaUpsampling::Nearest` against libwebp with `no_fancy_upsampling` (`dwebp -nofancy`).
    let nearest = WebpDecoder::new().with_chroma_upsampling(ChromaUpsampling::Nearest);
    for &(w, h) in &[(1u32, 1u32), (17, 9), (64, 48), (255, 1), (1, 255)] {
        let rgba = photo_like_rgba(w, h, 0x77 ^ w);
        let webp = libwebp_oracle::libwebp_encode_lossy_rgba(&rgba, w, h, 80.0);
        let got: ImageBuf<Rgb8> = nearest.decode_image(&webp).expect("gamut decode");
        let lib = rgba_to_rgb(&libwebp_oracle::libwebp_decode_rgba_nofancy(&webp).rgba);
        let max = max_abs(got.as_samples(), &lib);
        assert_eq!(
            max, 0,
            "nearest vs libwebp -nofancy at {w}x{h}: max-diff {max}"
        );
    }
}

#[test]
fn libwebp_decodes_gamut_lossy_alpha_exactly() {
    // gamut encodes lossy color plus a raw `ALPH` alpha plane in an extended (`VP8X`) file; libwebp
    // must recover the exact alpha (alpha is lossless). The lossy color is not compared.
    use libwebp_oracle::libwebp_decode_rgba;

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
                ImageRef::<Rgba8>::new(&rgba, Dimensions::new(w, h).unwrap()).unwrap(),
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
    use libwebp_oracle::libwebp_encode_lossy_rgba;

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
        assert_eq!(got.dimensions(), Dimensions::new(w, h).unwrap());
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
    use libwebp_oracle::libwebp_mux_read_metadata;

    let icc: Vec<u8> = (0..512u32).map(|i| (i % 251) as u8).collect();
    let exif: &[u8] = b"II\x2a\x00\x08\x00\x00\x00gamut-exif";
    let xmp: &[u8] = b"<?xpacket begin='\xef\xbb\xbf'?><x:xmpmeta/><?xpacket end='w'?>";
    let (w, h) = (48u32, 32u32);
    let dims = Dimensions::new(w, h).unwrap();
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
            libwebp_oracle::ICCP_FLAG | libwebp_oracle::EXIF_FLAG | libwebp_oracle::XMP_FLAG,
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
    use libwebp_oracle::libwebp_mux_read_metadata;

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
    let dims = Dimensions::new(w, h).unwrap();
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
        libwebp_oracle::ALPHA_FLAG | libwebp_oracle::EXIF_FLAG
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
    use libwebp_oracle::libwebp_mux_set_metadata;

    let icc: Vec<u8> = (0..1024u32).map(|i| (i % 241) as u8).collect();
    let exif: &[u8] = b"MM\x00\x2a\x00\x00\x00\x08libwebp-exif";
    let xmp: &[u8] = b"<x:xmpmeta xmlns:x='adobe:ns:meta/'></x:xmpmeta>";

    for &(w, h) in &[(1u32, 1u32), (17, 9), (64, 48)] {
        let rgba = pattern_rgba(w, h);
        let plain = libwebp_encode_lossless_rgba(&rgba, w, h);
        let tagged = libwebp_mux_set_metadata(&plain, Some(&icc), Some(exif), Some(xmp));

        let meta = WebpDecoder::new()
            .metadata(&tagged)
            .expect("read libwebp-muxed metadata");
        assert_eq!(meta.icc.as_deref(), Some(icc.as_slice()), "ICCP at {w}x{h}");
        assert_eq!(meta.exif.as_deref(), Some(exif), "EXIF at {w}x{h}");
        assert_eq!(meta.xmp.as_deref(), Some(xmp), "XMP at {w}x{h}");

        let got: ImageBuf<Rgba8> = WebpDecoder::new()
            .decode_image(&tagged)
            .expect("gamut decode libwebp-muxed file");
        assert_eq!(got.dimensions(), Dimensions::new(w, h).unwrap());
        assert_eq!(
            got.as_samples(),
            rgba.as_slice(),
            "metadata chunks must not disturb the lossless pixels at {w}x{h}"
        );

        // Only the chunks libwebp was asked to attach are reported: a subset stays a subset.
        let icc_only = libwebp_mux_set_metadata(&plain, Some(&icc), None, None);
        let meta = WebpDecoder::new()
            .metadata(&icc_only)
            .expect("read ICCP-only file");
        assert_eq!(meta.icc.as_deref(), Some(icc.as_slice()));
        assert_eq!(meta.exif, None);
        assert_eq!(meta.xmp, None);
    }
}

#[test]
fn libwebp_decodes_every_effort_levels_lossless_and_alpha() {
    // The container-level companion: lossless streams and the `ALPH` chunk at every rung, checked
    // through libwebp's own decoder rather than gamut's, so a rung that produced a stream only
    // gamut could read would fail here.
    use gamut_core::{EncodeImage, ImageRef, Rgba8};
    use gamut_webp::{Effort, WebpEncoder};
    use libwebp_oracle::libwebp_decode_rgba;

    let (w, h) = (48u32, 32u32);
    let rgba = pattern_rgba(w, h);
    let dims = gamut_core::Dimensions::new(w, h).unwrap();
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
