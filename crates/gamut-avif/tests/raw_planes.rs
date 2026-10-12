//! Caller-supplied planes (`AvifEncoder::encode_raw_planes`, issue #660): what a real reader gets
//! back, and what the container declares.
//!
//! The entry point skips colour conversion, so its whole contract is that the planes handed over
//! are the planes coded — bit-exact on the lossless path, and on the lossy path exactly the
//! reconstruction `gamut-av1` computed for them (a conformant decoder is obliged to reproduce it)
//! — and that `colr nclx` states the caller's declaration rather than the encoder's own knobs.
//! libavif (dav1d backend) answers for the colour item and the container; dav1d directly, through
//! this crate's own `Av1StillDecoder` seam, answers for the alpha auxiliary, which libavif's
//! planar output does not carry.
//!
//! Building this needs the `third_party/libavif` + `third_party/dav1d` submodules and a C
//! toolchain, like the crate's other oracle tests.

use gamut_av1::{Av1Colour, encode_still_intra_with, encode_still_intra16_with};
use gamut_avif::{
    AlphaMode, Av1Config, Av1StillDecoder, AvifContainer, AvifEncoder, DecodedFrame, RawPlanes,
};
use gamut_color::{
    BitDepth, ChromaSubsampling, ColorRange, ColourPrimaries, MatrixCoefficients, Planar8,
    Planar16, TransferCharacteristics,
};
use gamut_core::{Error, ErrorKind, Result};

const W: u32 = 23;
const H: u32 = 13;

/// `libavif`'s `avifPixelFormat` for each sampling the entry point takes.
fn yuv_format(subsampling: ChromaSubsampling) -> u32 {
    match subsampling {
        ChromaSubsampling::Cs444 => 1,
        ChromaSubsampling::Cs422 => 2,
        ChromaSubsampling::Cs420 => 3,
        other => panic!("no colour format for {other:?}"),
    }
}

/// A plane of `w * h` samples below `max`, structured on both axes and distinct per `seed`, so a
/// swapped plane, a transposed axis or a sample shifted by a row cannot coincide with the source.
fn plane(w: u32, h: u32, seed: u32, max: u32) -> Vec<u16> {
    (0..h)
        .flat_map(|y| (0..w).map(move |x| ((x * 37 + y * 11 + seed * 97) ^ (x * y)) % (max + 1)))
        .map(|s| s as u16)
        .collect()
}

/// The three source planes at `subsampling`, below `max`.
fn source(subsampling: ChromaSubsampling, max: u32) -> [Vec<u16>; 3] {
    let (cw, ch) = subsampling.chroma_dimensions(W, H);
    [
        plane(W, H, 0, max),
        plane(cw, ch, 1, max),
        plane(cw, ch, 2, max),
    ]
}

fn planar8(subsampling: ChromaSubsampling) -> Planar8 {
    let narrow = |p: Vec<u16>| p.into_iter().map(|s| s as u8).collect();
    let [y, u, v] = source(subsampling, 255);
    Planar8::from_planes_subsampled(W, H, subsampling, [narrow(y), narrow(u), narrow(v)])
        .expect("8-bit planes")
}

fn planar16(subsampling: ChromaSubsampling, depth: BitDepth) -> Planar16 {
    let [y, u, v] = source(subsampling, u32::from(depth.max_value()));
    Planar16::from_planes_subsampled(W, H, subsampling, depth, [y, u, v]).expect("planes")
}

fn alpha8() -> Planar8 {
    let a = plane(W, H, 3, 255).into_iter().map(|s| s as u8).collect();
    Planar8::from_planes_subsampled(W, H, ChromaSubsampling::Cs400, [a, vec![], vec![]])
        .expect("alpha plane")
}

fn alpha16(depth: BitDepth) -> Planar16 {
    let a = plane(W, H, 3, u32::from(depth.max_value()));
    Planar16::from_planes_subsampled(W, H, ChromaSubsampling::Cs400, depth, [a, vec![], vec![]])
        .expect("alpha plane")
}

fn widened(planes: &Planar8) -> [Vec<u16>; 3] {
    [0, 1, 2].map(|i| planes.plane(i).iter().map(|&s| u16::from(s)).collect())
}

fn owned(planes: &Planar16) -> [Vec<u16>; 3] {
    [0, 1, 2].map(|i| planes.plane(i).to_vec())
}

/// [`Av1StillDecoder`] over the real dav1d decoder, as `conformance.rs` bridges it.
struct Dav1dDecoder;

impl Av1StillDecoder for Dav1dDecoder {
    fn decode_still(&mut self, config: &Av1Config, payload: &[u8]) -> Result<DecodedFrame> {
        let mut stream = Vec::new();
        config.full_stream(payload, &mut stream)?;
        let pic = dav1d_oracle::decode_obu(&stream)
            .map_err(|_| Error::InvalidInput("raw planes: dav1d rejected the stream"))?;
        let [y, u, v] = pic.planes;
        DecodedFrame::new(
            pic.width,
            pic.height,
            pic.bit_depth,
            config.chroma_format(),
            y,
            u,
            v,
        )
    }
}

/// The alpha auxiliary's luma plane, decoded by dav1d.
fn decoded_alpha(avif: &[u8]) -> Vec<u16> {
    let container = AvifContainer::parse(avif).expect("parses");
    let image = container.image();
    let alpha = image
        .alpha_auxiliary_of(image.primary_item().id())
        .expect("an alpha auxiliary");
    let frame = container
        .decode_item_planar(alpha.id(), &mut Dav1dDecoder)
        .expect("dav1d decodes the alpha auxiliary");
    frame.y().to_vec()
}

/// The `base_q_idx` `AvifEncoder::lossy(quality)` codes at (`encoder.rs`'s mapping).
fn quality_to_quant(quality: u8) -> u8 {
    (((100 - u32::from(quality)) * 255 / 100) as u8).max(1)
}

#[test]
fn lossless_planes_decode_bit_exact_through_libavif_at_every_depth() {
    let declared = |matrix| (matrix, ColorRange::Limited);
    // A luma–chroma matrix at limited range, and the identity matrix (GBR) — at full range, since
    // AV1 §5.5.2 can signal nothing else for BT.709 / sRGB / identity.
    for (matrix, range) in [
        declared(MatrixCoefficients::Bt709),
        (MatrixCoefficients::Identity, ColorRange::Full),
    ] {
        let p8 = planar8(ChromaSubsampling::Cs444);
        let raw = RawPlanes::eight(&p8, None, matrix, range).unwrap();
        let avif = AvifEncoder::lossless()
            .encode_raw_planes_to_vec(raw)
            .unwrap();
        let decoded = libavif_oracle::decode_avif(&avif).expect("libavif decodes");
        assert_eq!((decoded.bit_depth, decoded.yuv_format), (8, 1));
        assert_eq!(decoded.planes, widened(&p8), "8-bit {matrix:?}");

        for (depth, bits) in [(BitDepth::Ten, 10), (BitDepth::Twelve, 12)] {
            let p16 = planar16(ChromaSubsampling::Cs444, depth);
            let raw = RawPlanes::high(&p16, None, matrix, range).unwrap();
            let avif = AvifEncoder::lossless()
                .encode_raw_planes_to_vec(raw)
                .unwrap();
            let decoded = libavif_oracle::decode_avif(&avif).expect("libavif decodes");
            assert_eq!((decoded.bit_depth, decoded.yuv_format), (bits, 1));
            assert_eq!(decoded.planes, owned(&p16), "{bits}-bit {matrix:?}");
        }
    }
}

#[test]
fn lossy_subsampled_planes_decode_to_gamut_av1s_own_reconstruction() {
    // A lossy encode of raw planes is the AV1 encode of exactly those planes: libavif must decode
    // the reconstruction `gamut-av1` computed for them at the same quantizer and colour, and that
    // reconstruction must sit within the quantizer's error of the source. The source is
    // deliberately busy (see `plane`), so the bound is on the mean error, not the worst sample:
    // quality 90 lands near 8 code values on it, while two unrelated uniform planes differ by
    // about a third of the range (~85) on average — the bound separates "these planes" from "some
    // other planes", and the reconstruction equality above is the exact claim.
    const QUALITY: u8 = 90;
    const MEAN_ERROR_8: f64 = 12.0;
    let colour = |matrix| Av1Colour {
        matrix,
        range: ColorRange::Limited,
        ..Av1Colour::default()
    };
    let mean_error = |a: &[Vec<u16>; 3], b: &[Vec<u16>; 3]| -> f64 {
        let diffs: Vec<u16> = a
            .iter()
            .zip(b)
            .flat_map(|(a, b)| a.iter().zip(b).map(|(&x, &y)| x.abs_diff(y)))
            .collect();
        diffs.iter().map(|&d| f64::from(d)).sum::<f64>() / diffs.len() as f64
    };
    let encoder = AvifEncoder::lossy(QUALITY);

    let p8 = planar8(ChromaSubsampling::Cs420);
    let raw = RawPlanes::eight(&p8, None, MatrixCoefficients::Bt601, ColorRange::Limited).unwrap();
    let decoded = libavif_oracle::decode_avif(&encoder.encode_raw_planes_to_vec(raw).unwrap())
        .expect("libavif decodes");
    let (_, recon) = encode_still_intra_with(
        &p8,
        quality_to_quant(QUALITY),
        colour(MatrixCoefficients::Bt601),
    )
    .unwrap();
    assert_eq!(decoded.yuv_format, yuv_format(ChromaSubsampling::Cs420));
    assert_eq!(decoded.planes, recon.planes);
    let err = mean_error(&decoded.planes, &widened(&p8));
    assert!(err <= MEAN_ERROR_8, "8-bit 4:2:0 mean error {err}");

    let p16 = planar16(ChromaSubsampling::Cs422, BitDepth::Ten);
    let raw = RawPlanes::high(
        &p16,
        None,
        MatrixCoefficients::Bt2020Ncl,
        ColorRange::Limited,
    )
    .unwrap();
    let decoded = libavif_oracle::decode_avif(&encoder.encode_raw_planes_to_vec(raw).unwrap())
        .expect("libavif decodes");
    let (_, recon) = encode_still_intra16_with(
        &p16,
        quality_to_quant(QUALITY),
        colour(MatrixCoefficients::Bt2020Ncl),
    )
    .unwrap();
    assert_eq!(decoded.yuv_format, yuv_format(ChromaSubsampling::Cs422));
    assert_eq!(decoded.planes, recon.planes);
    // Four times the 8-bit bound: the same quantizer step is two bits finer relative to a 10-bit
    // sample's scale.
    let err = mean_error(&decoded.planes, &owned(&p16));
    assert!(err <= MEAN_ERROR_8 * 4.0, "10-bit 4:2:2 mean error {err}");
}

#[test]
fn colr_nclx_states_the_planes_declaration_not_the_encoders_knobs() {
    // The encoder's own matrix/range/chroma knobs are set to values the planes contradict; the
    // file must describe the planes. The primaries/transfer tags are the encoder's, as on every
    // other input.
    let encoder = AvifEncoder::lossy(80)
        .with_matrix(MatrixCoefficients::Bt709)
        .with_color_range(ColorRange::Full)
        .with_chroma(ChromaSubsampling::Cs444)
        .with_primaries(ColourPrimaries::Bt2020)
        .with_transfer(TransferCharacteristics::Pq);
    for (matrix, range, full_range) in [
        (MatrixCoefficients::Bt601, ColorRange::Limited, false),
        (MatrixCoefficients::Bt2020Ncl, ColorRange::Full, true),
    ] {
        let p16 = planar16(ChromaSubsampling::Cs420, BitDepth::Twelve);
        let raw = RawPlanes::high(&p16, None, matrix, range).unwrap();
        let s = libavif_oracle::introspect(&encoder.encode_raw_planes_to_vec(raw).unwrap())
            .expect("libavif parses");
        assert_eq!(s.matrix_coefficients, matrix as u16);
        assert_eq!(s.full_range, full_range, "{matrix:?}");
        assert_eq!((s.depth, s.yuv_format), (12, 3));
        assert_eq!(
            (s.color_primaries, s.transfer_characteristics),
            (
                ColourPrimaries::Bt2020 as u16,
                TransferCharacteristics::Pq as u16
            )
        );
    }
}

#[test]
fn an_alpha_plane_decodes_bit_exact_as_the_auxiliary_at_the_colour_depth() {
    let p8 = planar8(ChromaSubsampling::Cs444);
    let a8 = alpha8();
    let raw =
        RawPlanes::eight(&p8, Some(&a8), MatrixCoefficients::Bt709, ColorRange::Full).unwrap();
    let avif = AvifEncoder::lossless()
        .encode_raw_planes_to_vec(raw)
        .unwrap();
    assert!(libavif_oracle::introspect(&avif).unwrap().alpha_present);
    assert_eq!(decoded_alpha(&avif), widened(&a8)[0]);

    let p16 = planar16(ChromaSubsampling::Cs444, BitDepth::Ten);
    let a16 = alpha16(BitDepth::Ten);
    let raw = RawPlanes::high(
        &p16,
        Some(&a16),
        MatrixCoefficients::Bt709,
        ColorRange::Full,
    )
    .unwrap();
    let avif = AvifEncoder::lossless()
        .encode_raw_planes_to_vec(raw)
        .unwrap();
    assert_eq!(decoded_alpha(&avif), a16.plane(0));
}

#[test]
fn a_premultiplied_declaration_writes_prem_for_raw_planes() {
    let p8 = planar8(ChromaSubsampling::Cs420);
    let a8 = alpha8();
    let raw =
        RawPlanes::eight(&p8, Some(&a8), MatrixCoefficients::Bt709, ColorRange::Full).unwrap();
    let premultiplied = |mode| {
        let avif = AvifEncoder::lossy(70)
            .with_alpha_mode(mode)
            .encode_raw_planes_to_vec(raw)
            .unwrap();
        libavif_oracle::introspect(&avif)
            .unwrap()
            .premultiplied_alpha
    };
    assert!(premultiplied(AlphaMode::Premultiplied));
    assert!(!premultiplied(AlphaMode::Unassociated));
}

#[test]
fn premultiply_on_encode_is_refused_only_with_an_alpha_plane() {
    let p8 = planar8(ChromaSubsampling::Cs444);
    let a8 = alpha8();
    let encoder = AvifEncoder::lossless().with_alpha_mode(AlphaMode::PremultiplyOnEncode);
    let with_alpha =
        RawPlanes::eight(&p8, Some(&a8), MatrixCoefficients::Bt709, ColorRange::Full).unwrap();
    let err = encoder.encode_raw_planes_to_vec(with_alpha).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Unsupported);
    // Without alpha the mode is meaningless and ignored, as it is for an `Rgb8` input.
    let without = RawPlanes::eight(&p8, None, MatrixCoefficients::Bt709, ColorRange::Full).unwrap();
    assert!(encoder.encode_raw_planes_to_vec(without).is_ok());
}

#[test]
fn planes_the_av1_encode_cannot_code_are_refused_with_typed_errors() {
    let p420 = planar8(ChromaSubsampling::Cs420);
    let refused = |encoder: AvifEncoder, matrix, range| {
        let raw = RawPlanes::eight(&p420, None, matrix, range).unwrap();
        encoder.encode_raw_planes_to_vec(raw).unwrap_err().kind()
    };
    // `gamut-av1` codes lossless only at 4:4:4.
    assert_eq!(
        refused(
            AvifEncoder::lossless(),
            MatrixCoefficients::Bt709,
            ColorRange::Full
        ),
        ErrorKind::Unsupported
    );
    // AV1 §6.4.2: the identity matrix needs 4:4:4.
    assert_eq!(
        refused(
            AvifEncoder::lossy(50),
            MatrixCoefficients::Identity,
            ColorRange::Full
        ),
        ErrorKind::InvalidInput
    );
    // AV1 codes no 16-bit samples (§6.4.1).
    let p16 = planar16(ChromaSubsampling::Cs444, BitDepth::Sixteen);
    let raw = RawPlanes::high(&p16, None, MatrixCoefficients::Bt709, ColorRange::Full).unwrap();
    assert_eq!(
        AvifEncoder::lossless()
            .encode_raw_planes_to_vec(raw)
            .unwrap_err()
            .kind(),
        ErrorKind::Unsupported
    );
}
