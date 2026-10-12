//! End-to-end correctness: encode → decode with libavif → compare planes to the source.
//!
//! This is the authoritative container check: a real AVIF reader must parse gamut's container and
//! reproduce the encoder's pixels. libavif (dav1d backend) is linked in from the
//! `third_party/libavif` + `third_party/dav1d` submodules via the `libavif-oracle` dev-dependency,
//! so the check is hermetic and always runs — it never depends on an `avifdec` binary being
//! installed. Building these tests therefore needs cmake/meson/ninja/nasm and the checked-out
//! submodules (`git submodule update --init --recursive`).

use gamut_avif::{AvifEncoder, Mirror, Rotation};
use gamut_core::{Dimensions, EncodeImage, ImageRef, Rgb8};

/// Source RGB pattern (structure + variation to exercise nonzero coefficients).
fn rgb_at(x: u32, y: u32) -> (u8, u8, u8) {
    (
        ((x * 7 + y * 3) & 0xff) as u8,
        ((x * x + y) & 0xff) as u8,
        ((x ^ (y * 5)) & 0xff) as u8,
    )
}

/// Builds the interleaved RGB source buffer for a `w`×`h` frame.
fn source_rgb(w: u32, h: u32) -> Vec<u8> {
    let mut rgb = vec![0u8; (w * h * 3) as usize];
    for y in 0..h {
        for x in 0..w {
            let i = ((y * w + x) * 3) as usize;
            let (r, g, b) = rgb_at(x, y);
            rgb[i] = r;
            rgb[i + 1] = g;
            rgb[i + 2] = b;
        }
    }
    rgb
}

fn roundtrip(w: u32, h: u32) {
    let rgb = source_rgb(w, h);

    let mut avif = Vec::new();
    AvifEncoder::new()
        .encode_image(
            ImageRef::<Rgb8>::new(
                &rgb,
                Dimensions {
                    width: w,
                    height: h,
                },
            )
            .unwrap(),
            &mut avif,
        )
        .unwrap();

    let decoded = libavif_oracle::decode_avif(&avif)
        .unwrap_or_else(|e| panic!("libavif decode failed for {w}x{h}: {e}"));
    assert_eq!((decoded.width, decoded.height), (w, h));
    // Lossless must stay 4:4:4 — the identity matrix requires it (AV1 §6.4.2) — so the luma index
    // below is valid for all three planes. 1 is `AVIF_PIXEL_FORMAT_YUV444`.
    assert_eq!(
        decoded.yuv_format, 1,
        "lossless must stay 4:4:4 for {w}x{h}"
    );
    let [yp, up, vp] = &decoded.planes;

    // Identity matrix mapping: Y=G, U=B, V=R.
    for y in 0..h {
        for x in 0..w {
            let i = (y * w + x) as usize;
            let (r, g, b) = rgb_at(x, y);
            assert_eq!(yp[i], u16::from(g), "Y!=G at ({x},{y}) in {w}x{h}");
            assert_eq!(up[i], u16::from(b), "U!=B at ({x},{y}) in {w}x{h}");
            assert_eq!(vp[i], u16::from(r), "V!=R at ({x},{y}) in {w}x{h}");
        }
    }
}

#[test]
fn lossless_roundtrip_via_libavif() {
    // Tiny, non-aligned (edge padding + forced partition splits), single-SB, and multi-SB frames.
    for (w, h) in [
        (1, 1),
        (8, 8),
        (17, 13),
        (31, 31),
        (64, 64),
        (100, 80),
        (200, 150),
    ] {
        roundtrip(w, h);
    }
}

/// Mirrors gamut-avif's documented quality→`base_q_idx` mapping (see `references/avif/README.md`):
/// the test needs the exact `base_q_idx` that [`AvifEncoder::lossy`] selects, so it can compute the
/// matching AV1 reconstruction to compare the decoded pixels against. The production mapping clamps
/// out-of-range quality; the tests only feed `0..=100`, so this asserts that precondition instead.
fn quality_to_quant(quality: u8) -> u8 {
    debug_assert!(quality <= 100, "quality must be 0..=100, got {quality}");
    (((100 - u32::from(quality)) * 255 / 100) as u8).max(1)
}

#[test]
fn lossy_roundtrip_via_libavif() {
    // For lossy coding the decoded image is not the source, but it must equal the AV1 encoder's
    // own reconstruction byte-for-byte: libavif runs a conformant decoder (dav1d) over the OBUs the
    // container carries, so this validates the whole container + lossy AV1 path end-to-end.
    for &quality in &[95u8, 80, 50, 20] {
        let qidx = quality_to_quant(quality);
        for &(w, h) in &[(8, 8), (17, 13), (40, 24), (100, 80)] {
            let rgb = source_rgb(w, h);

            let mut avif = Vec::new();
            AvifEncoder::lossy(quality)
                .encode_image(
                    ImageRef::<Rgb8>::new(
                        &rgb,
                        Dimensions {
                            width: w,
                            height: h,
                        },
                    )
                    .unwrap(),
                    &mut avif,
                )
                .unwrap();

            // The AV1 layer's reconstruction (the exact decoder output) for the same input, coded
            // through the same BT.709 matrix *and* the same 4:2:0 chroma the lossy encoder
            // defaults to.
            let img = ImageRef::<Rgb8>::new(
                &rgb,
                Dimensions {
                    width: w,
                    height: h,
                },
            )
            .unwrap();
            let planes = gamut_color::Planar8::from_rgb8_matrix_subsampled(
                img,
                lossy_matrix(),
                gamut_color::ChromaSubsampling::Cs420,
            )
            .unwrap();
            let (_, recon) = gamut_av1::encode_still_intra_with_speed(
                &planes,
                qidx,
                lossy_colour(),
                AvifEncoder::lossy(quality).speed(),
            )
            .unwrap();

            let decoded = libavif_oracle::decode_avif(&avif)
                .unwrap_or_else(|e| panic!("libavif decode failed for {w}x{h} q{quality}: {e}"));
            // libavif's own view of the format, so the container really carries 4:2:0 rather than
            // the planes merely happening to line up. 3 is `AVIF_PIXEL_FORMAT_YUV420`.
            assert_eq!(
                decoded.yuv_format, 3,
                "lossy output must be 4:2:0 for {w}x{h} q{quality}"
            );
            // Decoded Y/Cb/Cr planes are the AV1 recon planes 0/1/2.
            for (p, (d, r)) in decoded.planes.iter().zip(&recon.planes).enumerate() {
                assert_eq!(
                    d, r,
                    "plane {p} mismatch (libavif vs AV1 recon) for {w}x{h} q{quality}"
                );
            }
        }
    }
}

/// A photographic-ish source: smooth gradients with mild texture, and **correlated** channels (a
/// shared luminance ramp with small per-channel offsets), which is what a colour matrix is for.
///
/// [`source_rgb`] is deliberately the opposite — three near-independent high-frequency patterns —
/// so it stresses the coding tools, but a luma–chroma matrix cannot decorrelate what is already
/// decorrelated. The colour-facing assertions below therefore use this generator instead.
fn photo_rgb(w: u32, h: u32) -> Vec<u8> {
    let mut rgb = vec![0u8; (w * h * 3) as usize];
    for y in 0..h {
        for x in 0..w {
            let i = ((y * w + x) * 3) as usize;
            // A diagonal luminance ramp plus a slow ripple, shared by all three channels.
            let base = 40 + (x * 120 / w.max(1)) + (y * 80 / h.max(1)) + ((x / 8 + y / 8) % 5) * 4;
            rgb[i] = base.min(255) as u8;
            rgb[i + 1] = (base * 9 / 10).min(255) as u8;
            rgb[i + 2] = (base * 7 / 10 + 30).min(255) as u8;
        }
    }
    rgb
}

/// The colour the lossy encoder signals by default: BT.709 primaries/matrix, sRGB transfer, full
/// range.
fn lossy_colour() -> gamut_av1::Av1Colour {
    gamut_av1::Av1Colour {
        matrix: gamut_color::MatrixCoefficients::Bt709,
        ..gamut_av1::Av1Colour::default()
    }
}

/// The matching prepared transform.
fn lossy_matrix() -> gamut_color::RgbToYcbcr {
    gamut_color::RgbToYcbcr::new(
        gamut_color::MatrixCoefficients::Bt709,
        gamut_color::ColorRange::Full,
        gamut_color::BitDepth::Eight,
    )
    .unwrap()
}

#[test]
fn lossy_ycbcr_returns_to_rgb_through_libavif() {
    // The colour half of the round trip, which the plane comparison above cannot see: libavif's
    // `avifImageYUVToRGB` reads the `colr`/sequence-header matrix and converts back to RGB. If the
    // encoder wrote YCbCr samples but signalled the wrong matrix — or signalled BT.709 while
    // coding BT.601 planes — the returned RGB would be visibly wrong even though the planes still
    // matched the reconstruction. Bounded rather than exact: 8-bit YCbCr quantization plus lossy
    // coding both contribute. Measured worst channel error: 7 at q95, 12 at q80.
    for &(quality, tolerance) in &[(95u8, 12u8), (80, 20)] {
        for &(w, h) in &[(32u32, 32u32), (64, 48)] {
            let rgb = photo_rgb(w, h);
            let mut avif = Vec::new();
            AvifEncoder::lossy(quality)
                .encode_image(
                    ImageRef::<Rgb8>::new(
                        &rgb,
                        Dimensions {
                            width: w,
                            height: h,
                        },
                    )
                    .unwrap(),
                    &mut avif,
                )
                .unwrap();

            let (dw, dh, rgba) = libavif_oracle::decode_rgba(&avif)
                .unwrap_or_else(|e| panic!("libavif RGBA decode failed {w}x{h} q{quality}: {e}"));
            assert_eq!((dw, dh), (w, h));
            let mut worst = 0u8;
            for i in 0..(w * h) as usize {
                for c in 0..3 {
                    worst = worst.max(rgba[i * 4 + c].abs_diff(rgb[i * 3 + c]));
                }
                assert_eq!(rgba[i * 4 + 3], 255, "opaque alpha");
            }
            assert!(
                worst <= tolerance,
                "q{quality} {w}x{h}: worst channel error {worst} > {tolerance}"
            );
        }
    }
}

#[test]
fn lossy_ycbcr_is_smaller_than_the_identity_encoding() {
    // The point of the change: coding the lossy path through a luma–chroma matrix decorrelates the
    // channels, so the same image at the same quality costs fewer bytes than identity GBR.
    // Measured on this content: 31–38% smaller across the cases below.
    for &quality in &[80u8, 50] {
        for &(w, h) in &[(64u32, 48u32), (100, 80)] {
            let rgb = photo_rgb(w, h);
            let img = ImageRef::<Rgb8>::new(
                &rgb,
                Dimensions {
                    width: w,
                    height: h,
                },
            )
            .unwrap();
            let ycbcr = AvifEncoder::lossy(quality).encode_to_vec(img).unwrap();
            let identity = AvifEncoder::lossy(quality)
                .with_matrix(gamut_color::MatrixCoefficients::Identity)
                .encode_to_vec(img)
                .unwrap();
            assert!(
                ycbcr.len() < identity.len(),
                "q{quality} {w}x{h}: BT.709 {} bytes vs identity {} bytes",
                ycbcr.len(),
                identity.len()
            );
        }
    }
}

#[test]
fn orientation_transforms_roundtrip_via_libavif() {
    // `irot`/`imir` are display-time transforms marked *essential*: a conformant reader must parse
    // and honour them or reject the file, so libavif decoding successfully proves they are
    // well-formed MIAF properties. libavif (default settings) does not bake the transform into the
    // returned samples, so the stored planes are unchanged — the lossless pixels still round-trip.
    let (w, h) = (24u32, 16u32);
    let rgb = source_rgb(w, h);
    for (rot, mir) in [
        (Rotation::Ccw90, None),
        (Rotation::None, Some(Mirror::TopBottom)),
        (Rotation::Ccw270, Some(Mirror::LeftRight)),
        (Rotation::Ccw180, Some(Mirror::TopBottom)),
    ] {
        let mut enc = AvifEncoder::new().with_rotation(rot);
        if let Some(mirror) = mir {
            enc = enc.with_mirror(mirror);
        }
        let mut avif = Vec::new();
        enc.encode_image(
            ImageRef::<Rgb8>::new(
                &rgb,
                Dimensions {
                    width: w,
                    height: h,
                },
            )
            .unwrap(),
            &mut avif,
        )
        .unwrap();

        let decoded = libavif_oracle::decode_avif(&avif)
            .unwrap_or_else(|e| panic!("libavif rejected irot={rot:?} imir={mir:?}: {e}"));
        assert_eq!(
            (decoded.width, decoded.height),
            (w, h),
            "coded dims unchanged"
        );
        let [yp, up, vp] = &decoded.planes;
        for y in 0..h {
            for x in 0..w {
                let i = (y * w + x) as usize;
                let (r, g, b) = rgb_at(x, y);
                assert_eq!(
                    yp[i],
                    u16::from(g),
                    "Y at ({x},{y}) irot={rot:?} imir={mir:?}"
                );
                assert_eq!(up[i], u16::from(b));
                assert_eq!(vp[i], u16::from(r));
            }
        }
    }
}

#[test]
fn every_chroma_format_roundtrips_via_libavif() {
    // The container-level gate for the chroma surface: what `with_chroma` selects must survive into
    // the file, be what libavif reports, and decode to exactly the AV1 encoder's reconstruction.
    // Odd dimensions make the ceiling division on each subsampled axis observable.
    for (chroma, want_format) in [
        (gamut_color::ChromaSubsampling::Cs444, 1u32),
        (gamut_color::ChromaSubsampling::Cs422, 2),
        (gamut_color::ChromaSubsampling::Cs420, 3),
    ] {
        for &(w, h) in &[(8u32, 8u32), (17, 13), (40, 24), (100, 80)] {
            let rgb = source_rgb(w, h);
            let mut avif = Vec::new();
            AvifEncoder::lossy(60)
                .with_chroma(chroma)
                .encode_image(
                    ImageRef::<Rgb8>::new(
                        &rgb,
                        Dimensions {
                            width: w,
                            height: h,
                        },
                    )
                    .unwrap(),
                    &mut avif,
                )
                .unwrap();

            let img = ImageRef::<Rgb8>::new(
                &rgb,
                Dimensions {
                    width: w,
                    height: h,
                },
            )
            .unwrap();
            let planes =
                gamut_color::Planar8::from_rgb8_matrix_subsampled(img, lossy_matrix(), chroma)
                    .unwrap();
            let (_, recon) = gamut_av1::encode_still_intra_with_speed(
                &planes,
                quality_to_quant(60),
                lossy_colour(),
                AvifEncoder::lossy(60).speed(),
            )
            .unwrap();

            let decoded = libavif_oracle::decode_avif(&avif)
                .unwrap_or_else(|e| panic!("libavif decode failed for {chroma:?} {w}x{h}: {e}"));
            assert_eq!(
                decoded.yuv_format, want_format,
                "{chroma:?} {w}x{h}: container carries the wrong pixel format"
            );
            for (p, (d, r)) in decoded.planes.iter().zip(&recon.planes).enumerate() {
                assert_eq!(d, r, "{chroma:?} {w}x{h}: plane {p} mismatch");
            }
        }
    }
}
