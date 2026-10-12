//! The encode-speed preset (issue #654): `AvifEncoder::with_speed` codes every AV1 still of the
//! file — the colour item and the alpha auxiliary — at `gamut-av1`'s preset, and a conformant
//! reader (libavif over dav1d) reconstructs each preset's output to exactly the AV1 encoder's own
//! reconstruction.
//!
//! libavif is linked in from the `third_party/libavif` + `third_party/dav1d` submodules via the
//! `libavif-oracle` dev-dependency (`mise run fetch-av1-oracles` fetches dav1d).

use gamut_av1::{Av1Colour, EncodedStill, ReconImage};
use gamut_avif::{AvifContainer, AvifEncoder, Speed};
use gamut_color::{BitDepth, ColorRange, MatrixCoefficients, Planar8, Planar16, RgbToYcbcr};
use gamut_core::{Dimensions, EncodeImage, Gray8, ImageRef, Rgba8, Rgba16};

const W: u32 = 40;
const H: u32 = 24;

const DIMS: Dimensions = Dimensions {
    width: W,
    height: H,
};

/// `AvifEncoder::lossy(50)`'s `base_q_idx`: `(100 - 50) * 255 / 100`.
const Q50: u8 = 127;

/// A textured RGBA source with structured, non-opaque alpha: enough detail that the presets'
/// pruned searches settle on different modes, so a preset that never reached a still is visible.
fn source_rgba8() -> Vec<u8> {
    let mut px = Vec::with_capacity((W * H * 4) as usize);
    for y in 0..H {
        for x in 0..W {
            px.extend_from_slice(&[
                ((x * 7 + y * 3) & 0xff) as u8,
                ((x * x + y) & 0xff) as u8,
                ((x ^ (y * 5)) & 0xff) as u8,
                ((x * 11 + y * 29) % 256) as u8,
            ]);
        }
    }
    px
}

/// The 8-bit source widened to 16 bits by bit replication.
fn source_rgba16() -> Vec<u16> {
    source_rgba8().iter().map(|&s| u16::from(s) * 257).collect()
}

/// The 8-bit source's green channel as a greyscale image.
fn source_gray8() -> Vec<u8> {
    source_rgba8()
        .as_chunks::<4>()
        .0
        .iter()
        .map(|px| px[1])
        .collect()
}

/// The lossy default colour signalling: BT.709 matrix, full range.
fn lossy_colour() -> Av1Colour {
    Av1Colour {
        matrix: MatrixCoefficients::Bt709,
        ..Av1Colour::default()
    }
}

fn bt709(depth: BitDepth) -> RgbToYcbcr {
    RgbToYcbcr::new(MatrixCoefficients::Bt709, ColorRange::Full, depth).unwrap()
}

/// The coded AV1 payloads of `avif`: the primary item's, then its alpha auxiliary's if it has one.
fn payloads(avif: &[u8]) -> (Vec<u8>, Option<Vec<u8>>) {
    let container = AvifContainer::parse(avif).expect("parses");
    let img = container.image();
    let colour = img.primary_item();
    let alpha = img
        .alpha_auxiliary_of(colour.id())
        .map(|item| item.as_isobmff_item().payload.clone());
    (colour.as_isobmff_item().payload.clone(), alpha)
}

fn encode_rgba8(encoder: &AvifEncoder) -> Vec<u8> {
    let px = source_rgba8();
    encoder
        .encode_to_vec(ImageRef::<Rgba8>::new(&px, DIMS).unwrap())
        .unwrap()
}

/// The built-in colour and alpha stills (with their reconstructions) for the 8-bit RGBA source at
/// `speed` — what `lossy(50).with_speed(speed)` must carry.
fn rgba8_stills(speed: Speed) -> [(EncodedStill, ReconImage); 2] {
    let px = source_rgba8();
    let image = ImageRef::<Rgba8>::new(&px, DIMS).unwrap();
    let colour = Planar8::from_rgba8_matrix_view(image, bt709(BitDepth::Eight));
    let alpha = Planar8::from_rgba8_alpha_view(image);
    [
        gamut_av1::encode_still_intra_with_speed(&colour, Q50, lossy_colour(), speed).unwrap(),
        gamut_av1::encode_still_intra_with_speed(&alpha, Q50, Av1Colour::monochrome(), speed)
            .unwrap(),
    ]
}

#[test]
fn speed_defaults_to_the_full_search_and_reports_the_chosen_preset() {
    assert_eq!(AvifEncoder::lossy(50).speed(), Speed::Thorough);
    for &speed in Speed::ALL {
        assert_eq!(AvifEncoder::lossy(50).with_speed(speed).speed(), speed);
    }
}

#[test]
fn the_full_search_preset_codes_the_bytes_of_an_encoder_without_one() {
    assert_eq!(
        encode_rgba8(&AvifEncoder::lossy(50).with_speed(Speed::Thorough)),
        encode_rgba8(&AvifEncoder::lossy(50))
    );
}

#[test]
fn lossless_codes_the_same_bytes_at_every_speed() {
    let reference = encode_rgba8(&AvifEncoder::lossless());
    for &speed in Speed::ALL {
        assert_eq!(
            encode_rgba8(&AvifEncoder::lossless().with_speed(speed)),
            reference,
            "{speed:?}"
        );
    }
}

#[test]
fn rgba8_codes_colour_and_alpha_at_the_chosen_preset() {
    let mut seen = Vec::new();
    for &speed in Speed::ALL {
        let (colour, alpha) = payloads(&encode_rgba8(&AvifEncoder::lossy(50).with_speed(speed)));
        let [want_colour, want_alpha] = rgba8_stills(speed);
        assert_eq!(colour, want_colour.0.obus, "colour at {speed:?}");
        assert_eq!(alpha, Some(want_alpha.0.obus), "alpha at {speed:?}");
        seen.push((colour, alpha));
    }
    // The fixture separates the presets in both stills, so the equalities above could not hold
    // with a preset that never reached one of them.
    let (thorough, fastest) = (&seen[0], &seen[Speed::ALL.len() - 1]);
    assert_ne!(thorough.0, fastest.0, "colour differs across presets");
    assert_ne!(thorough.1, fastest.1, "alpha differs across presets");
}

#[test]
fn rgba16_codes_colour_and_alpha_at_the_chosen_preset() {
    let px = source_rgba16();
    let image = ImageRef::<Rgba16>::new(&px, DIMS).unwrap();
    let colour_planes = Planar16::from_rgba16_matrix_view(image, bt709(BitDepth::Ten));
    let alpha_planes = Planar16::from_rgba16_alpha_view(image, BitDepth::Ten);
    let at = |speed| {
        AvifEncoder::lossy(50)
            .with_bit_depth(BitDepth::Ten)
            .with_speed(speed)
            .encode_to_vec(image)
            .unwrap()
    };
    let (thorough, _) = payloads(&at(Speed::Thorough));
    let (colour, alpha) = payloads(&at(Speed::Fastest));
    let want = |planes, colour| {
        gamut_av1::encode_still_intra16_with_speed(planes, Q50, colour, Speed::Fastest)
            .unwrap()
            .0
            .obus
    };
    assert_eq!(colour, want(&colour_planes, lossy_colour()));
    assert_eq!(alpha, Some(want(&alpha_planes, Av1Colour::monochrome())));
    assert_ne!(colour, thorough, "the fixture separates the presets");
}

#[test]
fn gray8_codes_its_monochrome_still_at_the_chosen_preset() {
    let px = source_gray8();
    let image = ImageRef::<Gray8>::new(&px, DIMS).unwrap();
    let at = |speed| {
        AvifEncoder::lossy(50)
            .with_speed(speed)
            .encode_to_vec(image)
            .unwrap()
    };
    let want = gamut_av1::encode_still_intra_with_speed(
        &Planar8::from_gray8_view(image),
        Q50,
        Av1Colour::monochrome(),
        Speed::Fastest,
    )
    .unwrap()
    .0
    .obus;
    let (fastest, _) = payloads(&at(Speed::Fastest));
    assert_eq!(fastest, want);
    assert_ne!(
        fastest,
        payloads(&at(Speed::Thorough)).0,
        "the fixture separates the presets"
    );
}

#[test]
fn libavif_reconstructs_every_preset_to_the_av1_reconstruction() {
    for &speed in Speed::ALL {
        let avif = encode_rgba8(&AvifEncoder::lossy(50).with_speed(speed));
        let [(_, colour), (_, alpha)] = rgba8_stills(speed);

        let decoded = libavif_oracle::decode_avif(&avif)
            .unwrap_or_else(|e| panic!("libavif decode failed at {speed:?}: {e}"));
        for (p, (got, want)) in decoded.planes.iter().zip(&colour.planes).enumerate() {
            assert_eq!(got, want, "colour plane {p} at {speed:?}");
        }

        let (_, _, rgba) = libavif_oracle::decode_rgba(&avif)
            .unwrap_or_else(|e| panic!("libavif RGBA decode failed at {speed:?}: {e}"));
        let merged: Vec<u16> = rgba
            .as_chunks::<4>()
            .0
            .iter()
            .map(|px| u16::from(px[3]))
            .collect();
        assert_eq!(merged, alpha.planes[0], "alpha at {speed:?}");
    }
}
