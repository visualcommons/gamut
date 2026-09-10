//! DNG encode + decode throughput, against the Adobe DNG SDK's decode (issue #163).
//!
//! `cargo bench -p gamut-dng --bench codec` first prints a fixture table — the byte volumes each
//! measured region moves — then runs divan throughput benchmarks over the codec matrix the crate
//! ships: **uncompressed**, **Deflate** and **lossless JPEG**, each for **CFA** and **LinearRaw**
//! photometry. The counter is always the *raw sample volume* (`samples × 2` bytes), so encode,
//! decode and the reference implementation are all quoted against the same denominator and are
//! directly comparable.
//!
//! # What is inside the timed region, and what is not
//!
//! **Inside**, for every benchmark: the codec call itself, the allocation and growth of the buffer
//! it produces, and that buffer's teardown. Every closure below returns `()` and drops its result
//! explicitly, because divan otherwise defers a returned value's drop until after timing — which
//! would charge gamut nothing for freeing a decoded image while the SDK, whose `dng_negative`
//! destructor runs inside its own call, pays in full.
//!
//! **Outside**, for every benchmark: synthesising the sensor samples, building the [`RawImage`]
//! and [`CameraProfile`], and encoding the DNG (or the bare lossless-JPEG stream) that the decode
//! benchmarks read. Those are fixtures; timing them would measure this file rather than the codec.
//! No benchmark here touches the filesystem.
//!
//! # Is the gamut-versus-SDK comparison fair?
//!
//! Two comparisons are published, and their biases point in *opposite* directions, so together
//! they bracket the truth rather than flattering one side.
//!
//! `decode_dng_*` — **the SDK is favoured, by a stated and computable margin.** Both sides parse
//! the same in-memory bytes: [`gamut_dng_oracle::decode_dng_in_memory`] exists precisely so the
//! reference implementation is not charged for a temporary file or for the export `memcpy` that
//! crossing the FFI boundary would otherwise need (see its docs). What remains is that
//! `DngDecoder::decode` is a *whole-file* decode and `ReadStage1Image` is not: gamut additionally
//! decodes IFD 0's uncompressed RGB preview and reconstructs the metadata, work the SDK's stage-1
//! read skips entirely. The preview's size is exact and not a guess — `⌊w/2⌋ × ⌊h/2⌋ × 3` bytes
//! against the raw's `w × h × planes × 2` — so the fixture table prints it per case and the
//! handicap can be read off directly. It is not normalised away because gamut exposes no
//! raw-image-only decode entry point, and inventing one to make a benchmark look better would be
//! the wrong direction of causation.
//!
//! `decode_lossless_jpeg_*` — **gamut is favoured, by a smaller margin.** Here the subjects match
//! exactly: the same bare SOF3 stream in, the same interleaved `Vec<u16>` out, no container work
//! on either side. The residual bias is the FFI export path
//! ([`gamut_dng_oracle::decode_lossless_jpeg`] spools into a `std::vector`, copies that into a
//! `malloc`d buffer, and copies *that* into a `Vec`), which charges the SDK two extra passes over
//! the sample volume that gamut's single `Vec` does not pay. Those are memory-bandwidth passes,
//! not entropy decoding, so this is the tighter of the two comparisons — but it is a bias, and it
//! runs the other way.
//!
//! There is no `encode_adobe_sdk`: the oracle shim wraps the SDK's reader, not its writer, so no
//! reference encode number exists to compare against and none is fabricated. Encode throughput is
//! reported for gamut alone, across the same matrix.

use divan::counter::BytesCount;
use divan::{Bencher, black_box};
use gamut_core::Dimensions;
use gamut_dng::raw::cfa_color;
use gamut_dng::{
    CalibrationIlluminant, CameraProfile, Compression, DngDecoder, DngEncoder, RawImage,
    lossless_jpeg,
};

fn main() {
    print_fixture_table();
    divan::main();
}

/// Fixture frame size, in pixels. Large enough that the codecs dominate per-call overhead, small
/// enough that `cargo bench --workspace` stays affordable: a `LinearRaw` frame at this size is
/// 1.1 MiB of samples and a CFA frame 384 KiB.
const WIDTH: u32 = 512;
const HEIGHT: u32 = 384;

/// Fixture sample depth. 16-bit for every case, so the axis the matrix varies is the compression
/// scheme and the photometry — not the packing. (DNG's Deflate path is restricted to whole-byte
/// depths anyway; see `DngEncoder::encode`.)
const BITS: u16 = 16;

/// Which photometry a fixture carries — the two the encoder writes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Photometry {
    /// A single-plane RGGB Bayer mosaic.
    Cfa,
    /// A demosaiced three-plane linear image.
    LinearRaw,
}

impl Photometry {
    /// Colour planes per pixel.
    fn planes(self) -> u32 {
        match self {
            Photometry::Cfa => 1,
            Photometry::LinearRaw => 3,
        }
    }

    /// The fixture raw image for this photometry.
    fn raw(self) -> RawImage {
        let dims = Dimensions::new(WIDTH, HEIGHT).expect("non-empty fixture dimensions");
        let samples = sensor_samples(self.planes());
        let max = f64::from((1u32 << BITS) - 1);
        match self {
            Photometry::Cfa => {
                let pattern = vec![
                    cfa_color::RED,
                    cfa_color::GREEN,
                    cfa_color::GREEN,
                    cfa_color::BLUE,
                ];
                RawImage::new_cfa(dims, BITS, (2, 2), pattern, samples).expect("valid CFA fixture")
            }
            Photometry::LinearRaw => {
                RawImage::new_linear_raw(dims, BITS, 3, samples).expect("valid LinearRaw fixture")
            }
        }
        .with_black_level(0.0)
        .expect("valid black level")
        .with_white_level(max)
        .expect("valid white level")
        .with_active_area([0, 0, HEIGHT, WIDTH])
        .with_default_crop([0, 0], [WIDTH, HEIGHT])
    }
}

impl std::fmt::Display for Photometry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Photometry::Cfa => "cfa",
            Photometry::LinearRaw => "linear-raw",
        })
    }
}

/// One cell of the codec matrix: a photometry crossed with a compression scheme.
#[derive(Clone, Copy)]
struct Case {
    photometry: Photometry,
    compression: Compression,
}

impl Case {
    /// The fixture raw image.
    fn raw(self) -> RawImage {
        self.photometry.raw()
    }

    /// The fixture DNG the decode benchmarks read: this case's raw image, encoded.
    fn encoded(self) -> Vec<u8> {
        let mut out = Vec::new();
        encoder(self.compression)
            .encode(&self.raw(), &profile(), &mut out)
            .expect("fixture DNG encodes");
        out
    }

    /// Raw sample volume in bytes — the throughput denominator for every benchmark.
    fn raw_bytes(self) -> usize {
        raw_bytes(self.photometry)
    }
}

impl std::fmt::Display for Case {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let scheme = match self.compression {
            Compression::Uncompressed => "uncompressed",
            Compression::Deflate => "deflate",
            Compression::LosslessJpeg => "lossless-jpeg",
            other => return write!(f, "{}/{other:?}", self.photometry),
        };
        write!(f, "{}/{scheme}", self.photometry)
    }
}

/// The two photometries, as a benchmark argument list.
const PHOTOMETRIES: [Photometry; 2] = [Photometry::Cfa, Photometry::LinearRaw];

/// The codec matrix: every compression this crate encodes without an optional feature, crossed
/// with both photometries. JPEG XL is absent deliberately — encoding it needs the `jxl-encode`
/// feature (and a C++ toolchain), so a default `cargo bench` could not produce its fixture.
const CASES: [Case; 6] = [
    Case {
        photometry: Photometry::Cfa,
        compression: Compression::Uncompressed,
    },
    Case {
        photometry: Photometry::Cfa,
        compression: Compression::Deflate,
    },
    Case {
        photometry: Photometry::Cfa,
        compression: Compression::LosslessJpeg,
    },
    Case {
        photometry: Photometry::LinearRaw,
        compression: Compression::Uncompressed,
    },
    Case {
        photometry: Photometry::LinearRaw,
        compression: Compression::Deflate,
    },
    Case {
        photometry: Photometry::LinearRaw,
        compression: Compression::LosslessJpeg,
    },
];

/// A sensor-like frame: a smooth illumination falloff, a per-CFA-channel gain, and deterministic
/// per-photosite noise, interleaved across `planes`.
///
/// The noise is the point. Raw sensor data is hard to compress *because* of it, so a clean
/// synthetic gradient would flatter every compressor equally and measure nothing a real file
/// would see. This follows the same model `benches/compression.rs` uses for its packed payloads;
/// the two cannot share one generator because they produce different things — that bench needs
/// packed bytes for `gamut-deflate`, this one needs `u16` samples for a [`RawImage`].
fn sensor_samples(planes: u32) -> Vec<u16> {
    let max = f64::from((1u32 << BITS) - 1);
    let mut samples = Vec::with_capacity((WIDTH * HEIGHT * planes) as usize);
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            for plane in 0..planes {
                // Cosine-fourth-law-ish falloff from the frame centre.
                let dx = f64::from(x) / f64::from(WIDTH) - 0.5;
                let dy = f64::from(y) / f64::from(HEIGHT) - 0.5;
                let falloff = 1.0 - 1.4 * (dx * dx + dy * dy);
                // A green photosite collects roughly twice what red and blue do; for a linear
                // image the same gains index the interleaved planes.
                let channel = if planes == 1 {
                    (x % 2, y % 2)
                } else {
                    (plane % 2, plane / 2)
                };
                let gain = match channel {
                    (0, 0) => 0.42, // R
                    (1, 1) => 0.31, // B
                    _ => 0.70,      // G
                };
                // Deterministic shot-noise stand-in, a few percent of full scale.
                let hash = ((y * WIDTH + x) * planes + plane).wrapping_mul(2_654_435_761) >> 11;
                let noise = f64::from(hash % 2048) / 2048.0 - 0.5;
                let value = (falloff * gain + noise * 0.05).clamp(0.0, 1.0) * max;
                samples.push(value as u16);
            }
        }
    }
    samples
}

/// A plausible camera colour profile (an illustrative XYZ→camera matrix under D65). The encoder
/// needs one; nothing here measures it.
fn profile() -> CameraProfile {
    CameraProfile::new(
        "gamut BenchCam",
        [
            0.6722, -0.0635, -0.0963, -0.4287, 1.2460, 0.2028, -0.0908, 0.2162, 0.5668,
        ],
        CalibrationIlluminant::D65,
        [0.5128, 1.0, 0.7059],
    )
    .expect("valid profile")
}

/// The encoder under test, at one compression scheme. Everything else is the encoder's default.
fn encoder(compression: Compression) -> DngEncoder {
    DngEncoder::new().with_compression(compression)
}

/// Raw sample volume in bytes for a photometry: `w × h × planes × 2`.
fn raw_bytes(photometry: Photometry) -> usize {
    (WIDTH * HEIGHT * photometry.planes()) as usize * size_of::<u16>()
}

/// Bytes of IFD-0 preview a decode of one of these fixtures additionally unpacks:
/// `⌊w/2⌋ × ⌊h/2⌋ × 3`, uncompressed RGB8 (the encoder always writes the preview uncompressed).
///
/// This is the whole of the `decode_dng_gamut` / `decode_dng_adobe_sdk` asymmetry that is
/// attributable to pixels; the rest is IFD and metadata reconstruction, which does not scale with
/// the frame.
fn preview_bytes() -> usize {
    (WIDTH / 2 * (HEIGHT / 2) * 3) as usize
}

/// A bare lossless-JPEG (SOF3) stream over one photometry's fixture samples — one component for
/// CFA, three interleaved for `LinearRaw`.
fn lossless_jpeg_stream(photometry: Photometry) -> Vec<u8> {
    let planes = photometry.planes() as usize;
    lossless_jpeg::encode(
        &sensor_samples(photometry.planes()),
        WIDTH as usize,
        HEIGHT as usize,
        planes,
        BITS,
    )
    .expect("fixture lossless-JPEG stream encodes")
}

/// Prints the byte volumes each measured region moves, so a throughput number can be read against
/// what it is a throughput *of* — including the preview volume that separates gamut's whole-file
/// decode from the SDK's stage-1 read.
fn print_fixture_table() {
    println!(
        "\nDNG codec fixtures, {WIDTH}x{HEIGHT} at {BITS}-bit (bytes):\n\n\
         {:<26} {:>12} {:>12} {:>8} {:>12} {:>10}",
        "case", "raw samples", "encoded DNG", "of raw", "IFD0 preview", "of raw"
    );
    for case in CASES {
        let raw = case.raw_bytes();
        let encoded = case.encoded().len();
        let preview = preview_bytes();
        println!(
            "{case:<26} {raw:>12} {encoded:>12} {:>7.1}% {preview:>12} {:>9.1}%",
            encoded as f64 / raw as f64 * 100.0,
            preview as f64 / raw as f64 * 100.0,
        );
    }
    println!(
        "\n`decode_dng_gamut` decodes the whole file — raw image, that IFD-0 preview and the\n\
         metadata; `decode_dng_adobe_sdk` reads the raw image only. The \"IFD0 preview / of raw\"\n\
         column is the pixel volume of that difference. `decode_lossless_jpeg_*` has no such gap:\n\
         same stream in, same samples out.\n"
    );
}

/// Encode: `DngEncoder::encode` over a prepared raw image and profile.
///
/// Timed: preview derivation, sample packing and compression, IFD-tree layout, and the growth and
/// teardown of the output buffer. Not timed: building the raw image and the profile.
#[divan::bench(args = CASES)]
fn encode_gamut(bencher: Bencher, case: Case) {
    let raw = case.raw();
    let profile = profile();
    let encoder = encoder(case.compression);
    bencher
        .counter(BytesCount::new(case.raw_bytes()))
        .bench_local(|| {
            let mut out = Vec::new();
            encoder
                .encode(black_box(&raw), black_box(&profile), &mut out)
                .expect("encode");
            drop(black_box(out));
        });
}

/// Decode, gamut: `DngDecoder::decode` over a prepared DNG.
///
/// Timed: container parse, raw-image decode, IFD-0 preview decode, metadata reconstruction, and
/// the teardown of everything decoded. Not timed: producing the DNG bytes.
#[divan::bench(args = CASES)]
fn decode_dng_gamut(bencher: Bencher, case: Case) {
    let bytes = case.encoded();
    let decoder = DngDecoder::new();
    bencher
        .counter(BytesCount::new(case.raw_bytes()))
        .bench_local(|| {
            drop(black_box(
                decoder.decode(black_box(&bytes)).expect("decode"),
            ));
        });
}

/// Decode, Adobe DNG SDK: parse → build negative → `ReadStage1Image`, over the *same* bytes, from
/// memory.
///
/// Timed: everything the reference implementation does to materialise the raw image, including
/// the negative's teardown. Not timed: producing the DNG bytes — and, by construction of
/// [`gamut_dng_oracle::decode_dng_in_memory`], no temporary file and no FFI export copy. See this
/// file's header for the residual asymmetry against `decode_dng_gamut`.
#[divan::bench(args = CASES)]
fn decode_dng_adobe_sdk(bencher: Bencher, case: Case) {
    let bytes = case.encoded();
    bencher
        .counter(BytesCount::new(case.raw_bytes()))
        .bench_local(|| {
            // No `drop` to place: `DecodedExtent` is plain `Copy` data, because the SDK's own
            // teardown already ran — inside the call, and so inside this timed region.
            black_box(
                gamut_dng_oracle::decode_dng_in_memory(black_box(&bytes)).expect("SDK decode"),
            );
        });
}

/// Lossless-JPEG codestream decode, gamut: `lossless_jpeg::decode` over a bare SOF3 stream.
///
/// Timed: marker parse, Huffman + predictor decode, and the teardown of the sample buffer. Not
/// timed: encoding the stream.
#[divan::bench(args = PHOTOMETRIES)]
fn decode_lossless_jpeg_gamut(bencher: Bencher, photometry: Photometry) {
    let stream = lossless_jpeg_stream(photometry);
    bencher
        .counter(BytesCount::new(raw_bytes(photometry)))
        .bench_local(|| {
            drop(black_box(
                lossless_jpeg::decode(black_box(&stream)).expect("decode"),
            ));
        });
}

/// Lossless-JPEG codestream decode, Adobe DNG SDK: `DecodeLosslessJPEG` over the *same* stream.
///
/// Timed: the SDK's decode plus the FFI export path (spool vector → `malloc`d buffer → `Vec`),
/// which is two passes over the sample volume more than gamut pays. That bias favours gamut and
/// is the reason this file publishes two comparisons rather than one.
#[divan::bench(args = PHOTOMETRIES)]
fn decode_lossless_jpeg_adobe_sdk(bencher: Bencher, photometry: Photometry) {
    let stream = lossless_jpeg_stream(photometry);
    let expected = (WIDTH * HEIGHT * photometry.planes()) as usize;
    bencher
        .counter(BytesCount::new(raw_bytes(photometry)))
        .bench_local(|| {
            drop(black_box(
                gamut_dng_oracle::decode_lossless_jpeg(black_box(&stream), expected)
                    .expect("SDK decode"),
            ));
        });
}
