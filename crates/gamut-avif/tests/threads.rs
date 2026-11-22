//! The thread cap (issue #655) and the tile layout it parallelises over (#1233):
//! `AvifEncoder::with_threads` codes an RGBA input's colour and alpha stills concurrently, and
//! each still's tiles on `gamut-av1`'s tile-parallel path, and the file is byte-identical at every
//! thread count — at the default single tile, and at a multi-tile layout from
//! `AvifEncoder::with_tiling`, where each still's tiles do run concurrently. A conformant reader
//! (libavif over dav1d) reconstructs the multi-threaded, multi-tile file to exactly the AV1
//! encoder's own reconstruction at that layout.
//!
//! libavif is linked in from the `third_party/libavif` + `third_party/dav1d` submodules via the
//! `libavif-oracle` dev-dependency (`mise run fetch-av1-oracles` fetches dav1d).

use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};

use gamut_av1::{Av1Color, Av1EncodeConfig, Tiling};
use gamut_avif::{Av1CodestreamEncoder, Av1EncodeRequest, AvifEncoder};
use gamut_color::{BitDepth, ColorRange, MatrixCoefficients, Planar8, Planar16, RgbToYcbcr};
use gamut_core::{Dimensions, EncodeImage, Gray8, ImageRef, Pixel, Result, Rgb8, Rgba8, Rgba16};

/// The fixture's width, two and a half superblocks: wide enough that [`two_columns`] codes two
/// tile columns.
const W: u32 = 160;
const H: u32 = 40;

fn dims() -> Dimensions {
    Dimensions::new(W, H).unwrap()
}

/// A request for two tile columns: three superblock columns split into tiles two wide, so two
/// tiles for every still of the fixture.
fn two_columns() -> Tiling {
    Tiling::new(1, 0)
}

/// `AvifEncoder::lossy(50)`'s `base_q_idx`: `(100 - 50) * 255 / 100`.
const Q50: u8 = 127;

/// The thread counts every determinism check compares against one thread: an even and an odd
/// split, more threads than the two stills have tiles, and the machine's own parallelism.
fn caps() -> [NonZeroUsize; 5] {
    [threads(2), threads(3), threads(4), threads(16), available()]
}

fn threads(n: usize) -> NonZeroUsize {
    NonZeroUsize::new(n).unwrap()
}

/// The machine's parallelism, the value a caller passes for "every core".
fn available() -> NonZeroUsize {
    std::thread::available_parallelism().unwrap_or(NonZeroUsize::MIN)
}

/// A textured RGBA source with structured, non-opaque alpha.
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

fn source_rgb8() -> Vec<u8> {
    source_rgba8()
        .as_chunks::<4>()
        .0
        .iter()
        .flat_map(|px| [px[0], px[1], px[2]])
        .collect()
}

fn source_rgba16() -> Vec<u16> {
    source_rgba8().iter().map(|&s| u16::from(s) * 257).collect()
}

fn source_gray8() -> Vec<u8> {
    source_rgba8()
        .as_chunks::<4>()
        .0
        .iter()
        .map(|px| px[1])
        .collect()
}

fn encode<P: Pixel>(encoder: &AvifEncoder, px: &[P::Sample]) -> Vec<u8>
where
    AvifEncoder: EncodeImage<P>,
{
    encoder
        .encode_to_vec(ImageRef::<P>::new(px, dims()).unwrap())
        .unwrap()
}

/// Asserts that every cap in [`caps`] codes the bytes one thread codes, for `base` and `px`.
fn assert_thread_count_invariant<P: Pixel>(base: &AvifEncoder, px: &[P::Sample])
where
    AvifEncoder: EncodeImage<P>,
{
    let reference = encode::<P>(&base.clone().with_threads(threads(1)), px);
    for cap in caps() {
        assert_eq!(
            encode::<P>(&base.clone().with_threads(cap), px),
            reference,
            "{cap:?}"
        );
    }
}

#[test]
fn threads_default_to_one_and_report_the_chosen_cap() {
    assert_eq!(AvifEncoder::new().config().threads, threads(1));
    assert_eq!(AvifEncoder::lossy(50).config().threads, threads(1));
    assert_eq!(
        AvifEncoder::new().with_threads(threads(6)).config().threads,
        threads(6)
    );
}

#[test]
fn rgba8_codes_the_same_bytes_at_every_thread_count() {
    let px = source_rgba8();
    assert_thread_count_invariant::<Rgba8>(&AvifEncoder::lossy(50), &px);
    assert_thread_count_invariant::<Rgba8>(&AvifEncoder::lossless(), &px);
}

/// At two tile columns each still's tiles are coded concurrently on its share, and the file is
/// still the one-thread file — which is not the single-tile file, so the layout reached the
/// stream.
#[test]
fn a_multi_tile_rgba8_encode_codes_the_same_bytes_at_every_thread_count() {
    let px = source_rgba8();
    for base in [AvifEncoder::lossy(50), AvifEncoder::lossless()] {
        let tiled = base.clone().with_tiling(two_columns());
        assert_thread_count_invariant::<Rgba8>(&tiled, &px);
        assert_ne!(encode::<Rgba8>(&tiled, &px), encode::<Rgba8>(&base, &px));
    }
}

#[test]
fn rgba16_codes_the_same_bytes_at_every_thread_count() {
    let px = source_rgba16();
    assert_thread_count_invariant::<Rgba16>(
        &AvifEncoder::lossy(50).with_bit_depth(BitDepth::Ten),
        &px,
    );
}

#[test]
fn rgb8_codes_the_same_bytes_at_every_thread_count() {
    assert_thread_count_invariant::<Rgb8>(&AvifEncoder::lossy(50), &source_rgb8());
}

#[test]
fn gray8_codes_the_same_bytes_at_every_thread_count() {
    let px = source_gray8();
    assert_thread_count_invariant::<Gray8>(&AvifEncoder::lossy(50), &px);
    assert_thread_count_invariant::<Gray8>(
        &AvifEncoder::lossy(50).with_bit_depth(BitDepth::Twelve),
        &px,
    );
}

#[test]
fn libavif_reconstructs_a_multithreaded_multi_tile_rgba_encode_to_the_av1_reconstruction() {
    let px = source_rgba8();
    let avif = encode::<Rgba8>(
        &AvifEncoder::lossy(50)
            .with_tiling(two_columns())
            .with_threads(threads(4)),
        &px,
    );

    let image = ImageRef::<Rgba8>::new(&px, dims()).unwrap();
    let bt709 =
        RgbToYcbcr::new(MatrixCoefficients::Bt709, ColorRange::Full, BitDepth::Eight).unwrap();
    let colour_planes = Planar8::from_rgba8_matrix_view(image, bt709);
    let colour = Av1Color::DEFAULT.with_matrix(MatrixCoefficients::Bt709);
    let config = |colour| {
        Av1EncodeConfig::new(Q50)
            .with_color(colour)
            .with_effort(AvifEncoder::lossy(50).config().effort)
            .with_tiling(two_columns())
    };
    let colour_recon = gamut_av1::encode_still_with_recon(&colour_planes, &config(colour))
        .unwrap()
        .recon;
    let alpha_recon = gamut_av1::encode_still_with_recon(
        &Planar8::from_rgba8_alpha_view(image),
        &config(Av1Color::monochrome()),
    )
    .unwrap()
    .recon;

    let decoded = libavif_oracle::decode_avif(&avif).expect("libavif decodes the file");
    for (p, (got, want)) in decoded.planes.iter().zip(&colour_recon.planes).enumerate() {
        assert_eq!(got, want, "colour plane {p}");
    }
    let (_, _, rgba) = libavif_oracle::decode_rgba(&avif).expect("libavif decodes the RGBA");
    let alpha: Vec<u16> = rgba
        .as_chunks::<4>()
        .0
        .iter()
        .map(|px| u16::from(px[3]))
        .collect();
    assert_eq!(alpha, alpha_recon.planes[0]);
}

/// A backend that records the thread share it was offered, then declines, so the built-in
/// encoder still codes the file.
struct Recorder(Arc<Mutex<Vec<usize>>>);

impl Av1CodestreamEncoder for Recorder {
    fn supports(&mut self, req: &Av1EncodeRequest) -> bool {
        self.0.lock().unwrap().push(req.threads().get());
        false
    }

    fn encode_still(&mut self, _req: &Av1EncodeRequest, _planes: &Planar8) -> Result<Vec<u8>> {
        unreachable!("the recorder declines every job")
    }

    fn encode_still16(&mut self, _req: &Av1EncodeRequest, _planes: &Planar16) -> Result<Vec<u8>> {
        unreachable!("the recorder declines every job")
    }
}

/// The colour share a backend is offered for `encoder`'s encode of `px`.
fn offered<P: Pixel>(encoder: AvifEncoder, px: &[P::Sample]) -> Vec<usize>
where
    AvifEncoder: EncodeImage<P>,
{
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut encoder = encoder;
    encoder = encoder.with_backend(Recorder(Arc::clone(&seen)));
    encode::<P>(&encoder, px);
    seen.lock().unwrap().clone()
}

#[test]
fn a_backend_is_offered_the_whole_budget_for_an_image_without_alpha() {
    let px = source_rgb8();
    assert_eq!(offered::<Rgb8>(AvifEncoder::lossy(50), &px), [1]);
    assert_eq!(
        offered::<Rgb8>(AvifEncoder::lossy(50).with_threads(threads(5)), &px),
        [5]
    );
}

/// Auto tiling over a frame just past one 512 × 512 tile area: libavif's rule asks for two tile
/// columns, so the file is exactly the explicit two-column file at every thread count — and not
/// the single-tile one — and libavif decodes it bit-exactly.
#[test]
fn auto_tiling_codes_libavifs_layout_and_libavif_decodes_it() {
    let (w, h) = (520u32, 512u32);
    let px: Vec<u8> = (0..w * h)
        .map(|i| ((i % w) * 3 + (i / w) * 5) as u8)
        .collect();
    let image = ImageRef::<Gray8>::new(&px, Dimensions::new(w, h).unwrap()).unwrap();
    let encode = |encoder: AvifEncoder| encoder.encode_to_vec(image).unwrap();

    let auto = encode(AvifEncoder::lossless().with_auto_tiling(true));
    assert_eq!(
        auto,
        encode(AvifEncoder::lossless().with_tiling(Tiling::new(1, 0)))
    );
    assert_ne!(auto, encode(AvifEncoder::lossless()));
    assert_eq!(
        encode(
            AvifEncoder::lossless()
                .with_auto_tiling(true)
                .with_threads(threads(8))
        ),
        auto
    );

    let decoded = libavif_oracle::decode_avif(&auto).expect("libavif decodes the file");
    let source: Vec<u16> = px.iter().map(|&s| u16::from(s)).collect();
    assert_eq!(decoded.planes[0], source);
}

#[test]
fn a_backend_is_offered_the_colour_share_beside_an_alpha_auxiliary() {
    let px8 = source_rgba8();
    let px16 = source_rgba16();
    for (cap, share) in [(1, 1), (2, 1), (5, 3), (8, 4)] {
        let encoder = AvifEncoder::lossy(50).with_threads(threads(cap));
        assert_eq!(
            offered::<Rgba8>(encoder.clone(), &px8),
            [share],
            "8-bit {cap}"
        );
        assert_eq!(
            offered::<Rgba16>(encoder.with_bit_depth(BitDepth::Ten), &px16),
            [share],
            "10-bit {cap}"
        );
    }
}
