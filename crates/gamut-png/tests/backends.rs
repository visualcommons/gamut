//! End-to-end tests for the pluggable IDAT zlib seam (issue #278): the fallback contract in both
//! directions, the host-enforced zlib-bomb cap, byte-identical defaults, and the codec-abi
//! adapters.
//!
//! These live outside the crate because the codec-abi test backends need `unsafe` to touch the
//! `ImageDesc` plane pointers, and the library is `#![forbid(unsafe_code)]`.

use std::sync::{Arc, Mutex};

use gamut_codec_abi::{Decoder, EncodeConfig, Encoder, ImageDesc, Status, StreamConfig};
use gamut_core::{
    Bilevel, DecodeImage, Dimensions, EncodeImage, Error, Gray8, Gray16, GrayAlpha8, ImageBuf,
    ImageRef, Indexed8, Result, Rgb8, Rgb16, Rgba8,
};
use gamut_deflate::DeflateEncoder;
use gamut_png::{
    AbiDeflater, AbiInflater, CODEC_ID_ZLIB, FilterStrategy, IdatDeflater, IdatInflater, IdatInfo,
    Level, PIXEL_FORMAT_FILTERED_BYTES, PngDecoder, PngEncoder, PngPalette,
};

// ---------------------------------------------------------------------------------------------
// Byte-identical defaults
// ---------------------------------------------------------------------------------------------

/// Bytes captured from the encoder **before** the seam existed, except where a row's re-capture is
/// recorded below. Pushing no backend must reproduce them exactly: the registry is inert by
/// construction, not merely "close enough".
///
/// This pins the *seam*, not the encoder — so a deliberate encoding improvement re-captures the
/// affected row, and the change is recorded here rather than being absorbed silently:
///
/// * `rgb8_best_bruteforce`, issue #224: `FilterStrategy::MinBigrams` joined
///   `BRUTE_FORCE_STRATEGIES` and wins on this fixture, taking the IDAT from 36 bytes to 21. The
///   gate catching that is the point of it — an encoder change that made output *larger* would
///   look identical here, and would be a regression.
const GOLDEN: [(&str, &str); 11] = [
    (
        "gray8",
        "89504e470d0a1a0a0000000d4948445200000008000000080800000000e164e1570000001349444154789c636460860046092883852c06003207016b6db19e170000000049454e44ae426082",
    ),
    (
        "grayalpha8",
        "89504e470d0a1a0a0000000d49484452000000080000000808040000006e0676000000002449444154789c636460e542012c01010404183f7c4513707025a4e5c15334010353540100d57c0aebe9f5dbfd0000000049454e44ae426082",
    ),
    (
        "rgb8",
        "89504e470d0a1a0a0000000d49484452000000080000000808020000004b6d29dc0000003549444154789c636460e713c5061857acdf865d22203c0ebbc48fff6cd825162c5f875dc2c33f0cbbc487efffb04bcc98bf0cab04000a8a1a99a5b90d510000000049454e44ae426082",
    ),
    (
        "rgba8",
        "89504e470d0a1a0a0000000d4948445200000008000000080806000000c40fbe8b0000003e49444154789c6364e01653d4c1031813b2cb1af12a3870fada43bc0a14b4cd1cf12a68e89eb610af8207afbf31e255e0e01d968857c182d5db0ee253000033a93599467ae6660000000049454e44ae426082",
    ),
    (
        "gray16",
        "89504e470d0a1a0a0000000d4948445200000008000000081000000000b1f43d140000002949444154789c63646060feca820419e557b0a008b028a00930c6fd20a0024d80f92be3de0f04b40000e1d93d75bf3a2b240000000049454e44ae426082",
    ),
    (
        "rgb16",
        "89504e470d0a1a0a0000000d49484452000000080000000810020000001bfdf59f0000005449444154789c636460607fcd7f4dfca004061407437451967d1c2028ce218101c5c1105d148b0688081e0dfbc11a809663b7e320cc00341b8875145403765fc0ecc0d0b01f4d02e1084c3b909c84db51c8be0000ef1b418e4c9e234f0000000049454e44ae426082",
    ),
    (
        "bilevel",
        "89504e470d0a1a0a0000000d4948445200000008000000080100000000ec7483260000000f49444154789c6398c4ccc0cc8020010ff401c634b098550000000049454e44ae426082",
    ),
    (
        "indexed8",
        "89504e470d0a1a0a0000000d49484452000000080000000804030000003621a3b80000000f504c5445000000ff000000ff000000ff0909092696ae9d0000002749444154789c636654d6ffc46c728d598df987f6356606072113466666b51bdacc1f8024420200d7450b5d4ef4e87e0000000049454e44ae426082",
    ),
    (
        "rgb8_best_bruteforce",
        "89504e470d0a1a0a0000000d49484452000000080000000808020000004b6d29dc000000154944415478da636460e713c5069856e0008353020008cb701e6f73d8bc0000000049454e44ae426082",
    ),
    (
        "rgb8_fast",
        "89504e470d0a1a0a0000000d49484452000000080000000808020000004b6d29dc00000045494441547801636460e713c5061857acdf86290a048c01e17198a240c0f8e33f1ba62810302e58be0e531408183dfcc330458180f1c3f77f98a240c03863fe324c512000000a8a1a991b68ffb80000000049454e44ae426082",
    ),
    (
        "rgba8_autoreduce",
        "89504e470d0a1a0a0000000d4948445200000008000000080806000000c40fbe8b0000003e49444154789c6364e01653d4c1031813b2cb1af12a3870fada43bc0a14b4cd1cf12a68e89eb610af8207afbf31e255e0e01d968857c182d5db0ee253000033a93599467ae6660000000049454e44ae426082",
    ),
];

const W: u32 = 8;
const H: u32 = 8;
const N: usize = (W * H) as usize;

fn dims() -> Dimensions {
    Dimensions::new(W, H).expect("8x8")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn golden(name: &str) -> &'static str {
    GOLDEN
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, h)| *h)
        .expect("named golden")
}

fn rgb8_samples() -> Vec<u8> {
    (0..N * 3).map(|i| (i * 7) as u8).collect()
}

#[test]
fn default_output_is_byte_identical_across_colour_types_and_depths() {
    let mut out = Vec::new();
    let check = |name: &str, bytes: &[u8]| {
        assert_eq!(hex(bytes), golden(name), "{name} output changed");
    };

    let g8: Vec<u8> = (0..N).map(|i| (i * 3) as u8).collect();
    PngEncoder::new()
        .encode_image(
            ImageRef::<Gray8>::new(&g8, dims()).expect("gray8"),
            &mut out,
        )
        .expect("encode");
    check("gray8", &out);
    out.clear();

    let ga8: Vec<u8> = (0..N * 2).map(|i| (i * 5) as u8).collect();
    PngEncoder::new()
        .encode_image(
            ImageRef::<GrayAlpha8>::new(&ga8, dims()).expect("ga8"),
            &mut out,
        )
        .expect("encode");
    check("grayalpha8", &out);
    out.clear();

    let rgb8 = rgb8_samples();
    PngEncoder::new()
        .encode_image(
            ImageRef::<Rgb8>::new(&rgb8, dims()).expect("rgb8"),
            &mut out,
        )
        .expect("encode");
    check("rgb8", &out);
    out.clear();

    let rgba8: Vec<u8> = (0..N * 4).map(|i| (i * 11) as u8).collect();
    PngEncoder::new()
        .encode_image(
            ImageRef::<Rgba8>::new(&rgba8, dims()).expect("rgba8"),
            &mut out,
        )
        .expect("encode");
    check("rgba8", &out);
    out.clear();

    let g16: Vec<u16> = (0..N).map(|i| (i * 1013) as u16).collect();
    PngEncoder::new()
        .encode_image(
            ImageRef::<Gray16>::new(&g16, dims()).expect("gray16"),
            &mut out,
        )
        .expect("encode");
    check("gray16", &out);
    out.clear();

    let rgb16: Vec<u16> = (0..N * 3).map(|i| (i * 2027) as u16).collect();
    PngEncoder::new()
        .encode_image(
            ImageRef::<Rgb16>::new(&rgb16, dims()).expect("rgb16"),
            &mut out,
        )
        .expect("encode");
    check("rgb16", &out);
    out.clear();

    let bil: Vec<u8> = (0..N).map(|i| u8::from(i % 3 == 0)).collect();
    PngEncoder::new()
        .encode_image(
            ImageRef::<Bilevel>::new(&bil, dims()).expect("bilevel"),
            &mut out,
        )
        .expect("encode");
    check("bilevel", &out);
    out.clear();

    let idx: Vec<u8> = (0..N).map(|i| (i % 5) as u8).collect();
    let plte = PngPalette::new(&[[0, 0, 0], [255, 0, 0], [0, 255, 0], [0, 0, 255], [9, 9, 9]])
        .expect("palette");
    PngEncoder::new()
        .encode_indexed8(
            ImageRef::<Indexed8>::new(&idx, dims()).expect("indexed"),
            &plte,
            &mut out,
        )
        .expect("encode");
    check("indexed8", &out);
    out.clear();

    PngEncoder::new()
        .with_compression(Level::Best)
        .with_filter(FilterStrategy::BruteForce)
        .encode_image(
            ImageRef::<Rgb8>::new(&rgb8, dims()).expect("rgb8"),
            &mut out,
        )
        .expect("encode");
    check("rgb8_best_bruteforce", &out);
    out.clear();

    PngEncoder::new()
        .with_compression(Level::Fast)
        .encode_image(
            ImageRef::<Rgb8>::new(&rgb8, dims()).expect("rgb8"),
            &mut out,
        )
        .expect("encode");
    check("rgb8_fast", &out);
    out.clear();

    PngEncoder::new()
        .with_auto_reduce(true)
        .encode_image(
            ImageRef::<Rgba8>::new(&rgba8, dims()).expect("rgba8"),
            &mut out,
        )
        .expect("encode");
    check("rgba8_autoreduce", &out);
}

#[test]
fn default_decode_is_unchanged_with_no_backend() {
    let rgb8 = rgb8_samples();
    let mut png = Vec::new();
    PngEncoder::new()
        .encode_image(
            ImageRef::<Rgb8>::new(&rgb8, dims()).expect("rgb8"),
            &mut png,
        )
        .expect("encode");
    let back: ImageBuf<Rgb8> = PngDecoder::new().decode_image(&png).expect("decode");
    assert_eq!(back.as_samples(), rgb8);
}

// ---------------------------------------------------------------------------------------------
// Test doubles
// ---------------------------------------------------------------------------------------------

/// What a scripted backend should do once it has accepted a job.
#[derive(Clone)]
enum Act {
    /// Do the real work (built-in zlib), recording the call.
    Work,
    /// Return exactly `n` bytes of output — used to exercise the host's cap re-check.
    Produce(usize),
    /// Decline late, the `Status::UNSUPPORTED` equivalent.
    DeclineLate,
    /// Fail terminally.
    Fail,
}

/// A scripted inflater that records what it saw.
struct ScriptedInflater {
    accepts: bool,
    act: Act,
    log: Arc<Mutex<Vec<String>>>,
    name: &'static str,
    seen_max_out: Arc<Mutex<Vec<usize>>>,
}

impl ScriptedInflater {
    fn new(name: &'static str, accepts: bool, act: Act, log: &Arc<Mutex<Vec<String>>>) -> Self {
        Self {
            accepts,
            act,
            log: Arc::clone(log),
            name,
            seen_max_out: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl IdatInflater for ScriptedInflater {
    fn supports(&mut self, _info: &IdatInfo) -> bool {
        self.log
            .lock()
            .expect("log")
            .push(format!("{}:supports", self.name));
        self.accepts
    }

    fn inflate(&mut self, info: &IdatInfo, zlib: &[u8], max_out: usize) -> Result<Vec<u8>> {
        self.log
            .lock()
            .expect("log")
            .push(format!("{}:inflate", self.name));
        self.seen_max_out.lock().expect("seen").push(max_out);
        assert_eq!(
            max_out,
            info.raw_len(),
            "the host passes the exact expected size as the cap"
        );
        match self.act {
            Act::Work => miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(zlib, max_out)
                .map_err(|_| Error::InvalidInput("test: inflate failed")),
            Act::Produce(n) => Ok(vec![0u8; n]),
            Act::DeclineLate => Err(Error::Unsupported("test: late decline")),
            Act::Fail => Err(Error::InvalidInput("test: backend exploded")),
        }
    }
}

/// A scripted deflater that records what it saw.
struct ScriptedDeflater {
    accepts: bool,
    act: Act,
    log: Arc<Mutex<Vec<String>>>,
    name: &'static str,
}

impl ScriptedDeflater {
    fn new(name: &'static str, accepts: bool, act: Act, log: &Arc<Mutex<Vec<String>>>) -> Self {
        Self {
            accepts,
            act,
            log: Arc::clone(log),
            name,
        }
    }
}

impl IdatDeflater for ScriptedDeflater {
    fn supports(&mut self, _info: &IdatInfo) -> bool {
        self.log
            .lock()
            .expect("log")
            .push(format!("{}:supports", self.name));
        self.accepts
    }

    fn deflate(&mut self, info: &IdatInfo, raw: &[u8]) -> Result<Vec<u8>> {
        self.log
            .lock()
            .expect("log")
            .push(format!("{}:deflate", self.name));
        assert_eq!(info.raw_len(), raw.len(), "raw_len describes the input");
        match self.act {
            Act::Work | Act::Produce(_) => {
                let mut zlib = Vec::new();
                // Deliberately *not* the encoder's configured level: this is what proves a pushed
                // backend bypasses `with_compression`.
                DeflateEncoder::new()
                    .with_level(Level::Fast)
                    .zlib_compress(raw, &mut zlib);
                Ok(zlib)
            }
            Act::DeclineLate => Err(Error::Unsupported("test: late decline")),
            Act::Fail => Err(Error::InvalidInput("test: backend exploded")),
        }
    }
}

fn log() -> Arc<Mutex<Vec<String>>> {
    Arc::new(Mutex::new(Vec::new()))
}

fn entries(log: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
    log.lock().expect("log").clone()
}

/// A small PNG plus the samples it encodes.
fn sample_png() -> (Vec<u8>, Vec<u8>) {
    let rgb8 = rgb8_samples();
    let mut png = Vec::new();
    PngEncoder::new()
        .encode_image(
            ImageRef::<Rgb8>::new(&rgb8, dims()).expect("rgb8"),
            &mut png,
        )
        .expect("encode");
    (png, rgb8)
}

// ---------------------------------------------------------------------------------------------
// The security invariant: the host owns the cap
// ---------------------------------------------------------------------------------------------

#[test]
fn a_backend_returning_more_than_max_out_is_rejected_by_the_host() {
    let (png, _) = sample_png();
    // The expected filtered stream is 8 rows × (1 + 24) = 200 bytes; return one byte more.
    let expected = H as usize * (1 + W as usize * 3);
    let log = log();
    let mut decoder = PngDecoder::new();
    decoder.push_backend(ScriptedInflater::new(
        "greedy",
        true,
        Act::Produce(expected + 1),
        &log,
    ));
    let err = DecodeImage::<Rgb8>::decode_image(&decoder, &png)
        .expect_err("an over-cap backend must be rejected");
    assert_eq!(
        err.static_message().unwrap(),
        "PNG: IDAT backend produced more than the allowed output size",
        "the host's re-check — not the backend — must catch this"
    );
    assert_eq!(entries(&log), vec!["greedy:supports", "greedy:inflate"]);
}

#[test]
fn max_out_is_the_exact_expected_size_and_a_backend_at_the_cap_is_accepted() {
    let (png, _) = sample_png();
    let expected = H as usize * (1 + W as usize * 3);
    let log = log();
    let backend = ScriptedInflater::new("exact", true, Act::Produce(expected), &log);
    let seen = Arc::clone(&backend.seen_max_out);
    let mut decoder = PngDecoder::new();
    decoder.push_backend(backend);
    // All-zero filtered bytes are a valid image (filter type 0, black pixels).
    let decoded: ImageBuf<Rgb8> =
        DecodeImage::<Rgb8>::decode_image(&decoder, &png).expect("cap-respecting backend is used");
    assert_eq!(decoded.as_samples(), vec![0u8; N * 3]);
    assert_eq!(*seen.lock().expect("seen"), vec![expected]);
}

#[test]
fn a_short_backend_result_is_still_length_checked_by_the_decoder() {
    let (png, _) = sample_png();
    let log = log();
    let mut decoder = PngDecoder::new();
    decoder.push_backend(ScriptedInflater::new("short", true, Act::Produce(5), &log));
    let err = DecodeImage::<Rgb8>::decode_image(&decoder, &png)
        .expect_err("a short stream is not a valid image");
    assert_eq!(
        err.static_message().unwrap(),
        "PNG: IDAT is shorter than the image"
    );
}

// ---------------------------------------------------------------------------------------------
// The fallback contract — decode side
// ---------------------------------------------------------------------------------------------

#[test]
fn inflaters_are_tried_in_push_order_and_declines_skip() {
    let (png, rgb8) = sample_png();
    let log = log();
    let mut decoder = PngDecoder::new();
    decoder
        .push_backend(ScriptedInflater::new("first", false, Act::Work, &log))
        .push_backend(ScriptedInflater::new("second", true, Act::Work, &log))
        .push_backend(ScriptedInflater::new("third", true, Act::Work, &log));
    let decoded: ImageBuf<Rgb8> =
        DecodeImage::<Rgb8>::decode_image(&decoder, &png).expect("decode");
    assert_eq!(decoded.as_samples(), rgb8);
    assert_eq!(
        entries(&log),
        vec![
            "first:supports",
            "second:supports",
            "second:inflate",
            // "third" is never consulted: the second backend accepted and succeeded.
        ]
    );
}

#[test]
fn all_inflaters_declining_falls_through_to_the_builtin_tail() {
    let (png, rgb8) = sample_png();
    let log = log();
    let mut decoder = PngDecoder::new();
    decoder
        .push_backend(ScriptedInflater::new("early", false, Act::Work, &log))
        .push_backend(ScriptedInflater::new("late", true, Act::DeclineLate, &log));
    let decoded: ImageBuf<Rgb8> =
        DecodeImage::<Rgb8>::decode_image(&decoder, &png).expect("decode");
    assert_eq!(
        decoded.as_samples(),
        rgb8,
        "the miniz_oxide tail must have produced this"
    );
    assert_eq!(
        entries(&log),
        vec!["early:supports", "late:supports", "late:inflate"]
    );
}

#[test]
fn an_accepted_then_failed_inflater_propagates_and_the_tail_is_not_used() {
    let (png, _) = sample_png();
    let log = log();
    let mut decoder = PngDecoder::new();
    decoder
        .push_backend(ScriptedInflater::new("boom", true, Act::Fail, &log))
        .push_backend(ScriptedInflater::new("never", true, Act::Work, &log));
    let err =
        DecodeImage::<Rgb8>::decode_image(&decoder, &png).expect_err("the failure must propagate");
    assert_eq!(err.static_message().unwrap(), "test: backend exploded");
    assert_eq!(
        entries(&log),
        vec!["boom:supports", "boom:inflate"],
        "no later backend and no built-in tail may run"
    );
}

// ---------------------------------------------------------------------------------------------
// The fallback contract — encode side
// ---------------------------------------------------------------------------------------------

#[test]
fn deflaters_are_tried_in_push_order_and_declines_skip() {
    let rgb8 = rgb8_samples();
    let log = log();
    let mut encoder = PngEncoder::new();
    encoder
        .push_backend(ScriptedDeflater::new("first", false, Act::Work, &log))
        .push_backend(ScriptedDeflater::new("second", true, Act::Work, &log))
        .push_backend(ScriptedDeflater::new("third", true, Act::Work, &log));
    let mut png = Vec::new();
    encoder
        .encode_image(
            ImageRef::<Rgb8>::new(&rgb8, dims()).expect("rgb8"),
            &mut png,
        )
        .expect("encode");
    assert_eq!(
        entries(&log),
        vec!["first:supports", "second:supports", "second:deflate"]
    );
    let decoded: ImageBuf<Rgb8> = PngDecoder::new().decode_image(&png).expect("decode");
    assert_eq!(decoded.as_samples(), rgb8);
}

#[test]
fn all_deflaters_declining_falls_through_to_gamut_deflate() {
    let rgb8 = rgb8_samples();
    let log = log();
    let mut encoder = PngEncoder::new();
    encoder
        .push_backend(ScriptedDeflater::new("early", false, Act::Work, &log))
        .push_backend(ScriptedDeflater::new("late", true, Act::DeclineLate, &log));
    let mut png = Vec::new();
    encoder
        .encode_image(
            ImageRef::<Rgb8>::new(&rgb8, dims()).expect("rgb8"),
            &mut png,
        )
        .expect("encode");
    assert_eq!(
        entries(&log),
        vec!["early:supports", "late:supports", "late:deflate"]
    );
    assert_eq!(
        hex(&png),
        golden("rgb8"),
        "the built-in tail must produce byte-identical default output"
    );
}

#[test]
fn an_accepted_then_failed_deflater_propagates_and_the_tail_is_not_used() {
    let rgb8 = rgb8_samples();
    let log = log();
    let mut encoder = PngEncoder::new();
    encoder
        .push_backend(ScriptedDeflater::new("boom", true, Act::Fail, &log))
        .push_backend(ScriptedDeflater::new("never", true, Act::Work, &log));
    let mut png = Vec::new();
    let err = encoder
        .encode_image(
            ImageRef::<Rgb8>::new(&rgb8, dims()).expect("rgb8"),
            &mut png,
        )
        .expect_err("the failure must propagate");
    assert_eq!(err.static_message().unwrap(), "test: backend exploded");
    assert_eq!(entries(&log), vec!["boom:supports", "boom:deflate"]);
}

#[test]
fn a_pushed_deflater_bypasses_with_compression_but_the_tail_still_honours_it() {
    let rgb8 = rgb8_samples();
    let image = ImageRef::<Rgb8>::new(&rgb8, dims()).expect("rgb8");

    // The backend always compresses at Level::Fast, whatever the encoder is configured with.
    let mut accepted = Vec::new();
    let mut encoder = PngEncoder::new().with_compression(Level::Best);
    encoder.push_backend(ScriptedDeflater::new("fast", true, Act::Work, &log()));
    encoder.encode_image(image, &mut accepted).expect("encode");
    assert_eq!(
        hex(&accepted),
        golden("rgb8_fast"),
        "an accepted stream ignores Level::Best — it is a gamut-deflate concept"
    );
    assert_ne!(hex(&accepted), golden("rgb8_best_bruteforce"));

    // The same encoder, with the backend declining, honours Level::Best again.
    let mut declined = Vec::new();
    let mut encoder = PngEncoder::new()
        .with_compression(Level::Best)
        .with_filter(FilterStrategy::BruteForce);
    encoder.push_backend(ScriptedDeflater::new("nope", false, Act::Work, &log()));
    encoder.encode_image(image, &mut declined).expect("encode");
    assert_eq!(
        hex(&declined),
        golden("rgb8_best_bruteforce"),
        "the built-in tail must still honour the configured level"
    );
}

// ---------------------------------------------------------------------------------------------
// Round trip through a pushed pair, and shared-registry semantics
// ---------------------------------------------------------------------------------------------

#[test]
fn a_pushed_deflater_and_inflater_pair_round_trips() {
    let rgb8 = rgb8_samples();
    let log = log();
    let mut encoder = PngEncoder::new();
    encoder.push_backend(ScriptedDeflater::new("enc", true, Act::Work, &log));
    let mut png = Vec::new();
    encoder
        .encode_image(
            ImageRef::<Rgb8>::new(&rgb8, dims()).expect("rgb8"),
            &mut png,
        )
        .expect("encode");

    let mut decoder = PngDecoder::new();
    decoder.push_backend(ScriptedInflater::new("dec", true, Act::Work, &log));
    let decoded: ImageBuf<Rgb8> =
        DecodeImage::<Rgb8>::decode_image(&decoder, &png).expect("decode");
    assert_eq!(decoded.as_samples(), rgb8);
    // Both custom backends did the work; neither built-in tail ran.
    assert_eq!(
        entries(&log),
        vec!["enc:supports", "enc:deflate", "dec:supports", "dec:inflate"]
    );
    // The file is a real PNG the default decoder also reads.
    let plain: ImageBuf<Rgb8> = PngDecoder::new().decode_image(&png).expect("plain decode");
    assert_eq!(plain.as_samples(), rgb8);
}

#[test]
fn cloning_shares_the_pushed_backends() {
    let log = log();
    let mut encoder = PngEncoder::new();
    encoder.push_backend(ScriptedDeflater::new("shared", true, Act::Work, &log));
    let clone = encoder.clone();
    let rgb8 = rgb8_samples();
    let mut png = Vec::new();
    clone
        .encode_image(
            ImageRef::<Rgb8>::new(&rgb8, dims()).expect("rgb8"),
            &mut png,
        )
        .expect("encode");
    assert_eq!(entries(&log), vec!["shared:supports", "shared:deflate"]);

    let mut decoder = PngDecoder::new();
    decoder.push_backend(ScriptedInflater::new("shared-dec", true, Act::Work, &log));
    let clone = decoder.clone();
    let decoded: ImageBuf<Rgb8> = DecodeImage::<Rgb8>::decode_image(&clone, &png).expect("decode");
    assert_eq!(decoded.as_samples(), rgb8);
    assert_eq!(
        entries(&log),
        vec![
            "shared:supports",
            "shared:deflate",
            "shared-dec:supports",
            "shared-dec:inflate"
        ]
    );
}

#[test]
fn debug_and_default_still_work_with_a_registry() {
    let mut encoder = PngEncoder::default();
    assert!(format!("{encoder:?}").contains("Registry(0 backend(s))"));
    encoder.push_backend(ScriptedDeflater::new("d", true, Act::Work, &log()));
    assert!(format!("{encoder:?}").contains("Registry(1 backend(s))"));

    let mut decoder = PngDecoder::default();
    assert!(format!("{decoder:?}").contains("Registry(0 backend(s))"));
    decoder.push_backend(ScriptedInflater::new("i", true, Act::Work, &log()));
    assert!(format!("{decoder:?}").contains("Registry(1 backend(s))"));
}

#[test]
fn idat_info_describes_the_stream_the_backend_is_offered() {
    /// Captures the descriptor and declines.
    struct Capture(Arc<Mutex<Option<IdatInfo>>>);
    impl IdatInflater for Capture {
        fn supports(&mut self, info: &IdatInfo) -> bool {
            *self.0.lock().expect("slot") = Some(*info);
            false
        }
        fn inflate(&mut self, _: &IdatInfo, _: &[u8], _: usize) -> Result<Vec<u8>> {
            unreachable!("declined")
        }
    }
    let (png, _) = sample_png();
    let slot = Arc::new(Mutex::new(None));
    let mut decoder = PngDecoder::new();
    decoder.push_backend(Capture(Arc::clone(&slot)));
    let _: ImageBuf<Rgb8> = DecodeImage::<Rgb8>::decode_image(&decoder, &png).expect("decode");
    let info = slot.lock().expect("slot").expect("captured");
    assert_eq!(info.width(), W);
    assert_eq!(info.height(), H);
    assert_eq!(info.bit_depth(), 8);
    assert_eq!(info.color_type(), gamut_png::ColorType::Truecolor);
    assert_eq!(info.raw_len(), H as usize * (1 + W as usize * 3));
}

// ---------------------------------------------------------------------------------------------
// codec-abi adapters
// ---------------------------------------------------------------------------------------------

/// One recorded `decode` call: codec id, pixel format, width, height, and the plane stride.
type DecodeCall = (u32, u32, u32, u32, usize);

/// A codec-abi decoder that either inflates for real or returns a scripted status.
struct AbiDec {
    status: Option<Status>,
    seen: Arc<Mutex<Vec<DecodeCall>>>,
}

impl Decoder for AbiDec {
    fn supports(&mut self, cfg: &StreamConfig) -> bool {
        assert!(cfg.is_abi_current());
        cfg.codec_id == CODEC_ID_ZLIB
    }

    fn decode(&mut self, cfg: &StreamConfig, codestream: &[u8], out: &ImageDesc) -> Status {
        self.seen.lock().expect("seen").push((
            cfg.codec_id,
            out.pixel_format,
            out.width,
            out.height,
            out.strides[0],
        ));
        if let Some(status) = self.status {
            return status;
        }
        let Ok(bytes) =
            miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(codestream, out.strides[0])
        else {
            return Status(-9);
        };
        if bytes.len() != out.strides[0] {
            return Status(-9);
        }
        // SAFETY: the host allocated `strides[0]` bytes at `planes[0]` for this call.
        let dst = unsafe { std::slice::from_raw_parts_mut(out.planes[0], out.strides[0]) };
        dst.copy_from_slice(&bytes);
        Status::OK
    }
}

/// A codec-abi encoder that either zlib-compresses for real or returns a scripted status.
struct AbiEnc {
    status: Option<Status>,
    seen_quality: Arc<Mutex<Vec<u32>>>,
}

impl Encoder for AbiEnc {
    fn supports(&mut self, cfg: &EncodeConfig) -> bool {
        assert!(cfg.is_abi_current());
        self.seen_quality.lock().expect("seen").push(cfg.quality);
        cfg.codec_id == CODEC_ID_ZLIB
    }

    fn encode(
        &mut self,
        _cfg: &EncodeConfig,
        image: &ImageDesc,
        sink: &mut dyn FnMut(&[u8]) -> Status,
    ) -> Status {
        if let Some(status) = self.status {
            return status;
        }
        assert_eq!(image.pixel_format, PIXEL_FORMAT_FILTERED_BYTES);
        assert_eq!(image.plane_count, 1);
        // SAFETY: the host lent `strides[0]` readable bytes at `planes[0]` for this call.
        let raw = unsafe { std::slice::from_raw_parts(image.planes[0], image.strides[0]) };
        let mut zlib = Vec::new();
        DeflateEncoder::new().zlib_compress(raw, &mut zlib);
        // Deliver in two pieces, proving the sink concatenates.
        let (head, tail) = zlib.split_at(zlib.len() / 2);
        let first = sink(head);
        if !first.is_ok() {
            return first;
        }
        sink(tail)
    }
}

#[test]
fn abi_adapters_round_trip_a_png() {
    let rgb8 = rgb8_samples();
    let seen_quality = Arc::new(Mutex::new(Vec::new()));
    let mut encoder = PngEncoder::new();
    encoder.push_backend(AbiDeflater::new(AbiEnc {
        status: None,
        seen_quality: Arc::clone(&seen_quality),
    }));
    let mut png = Vec::new();
    encoder
        .encode_image(
            ImageRef::<Rgb8>::new(&rgb8, dims()).expect("rgb8"),
            &mut png,
        )
        .expect("encode");
    assert_eq!(*seen_quality.lock().expect("seen"), vec![100]);

    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut decoder = PngDecoder::new();
    decoder.push_backend(AbiInflater::new(AbiDec {
        status: None,
        seen: Arc::clone(&seen),
    }));
    let decoded: ImageBuf<Rgb8> =
        DecodeImage::<Rgb8>::decode_image(&decoder, &png).expect("decode");
    assert_eq!(decoded.as_samples(), rgb8);
    assert_eq!(
        *seen.lock().expect("seen"),
        vec![(
            CODEC_ID_ZLIB,
            PIXEL_FORMAT_FILTERED_BYTES,
            W,
            H,
            H as usize * (1 + W as usize * 3)
        )],
        "the descriptor carries the zlib id and the stream as one plane"
    );
    // The bytes are a normal PNG.
    let plain: ImageBuf<Rgb8> = PngDecoder::new().decode_image(&png).expect("plain decode");
    assert_eq!(plain.as_samples(), rgb8);
}

#[test]
fn abi_late_unsupported_declines_to_the_builtin_tail() {
    let (png, rgb8) = sample_png();
    let mut decoder = PngDecoder::new();
    decoder.push_backend(AbiInflater::new(AbiDec {
        status: Some(Status::UNSUPPORTED),
        seen: Arc::new(Mutex::new(Vec::new())),
    }));
    let decoded: ImageBuf<Rgb8> = DecodeImage::<Rgb8>::decode_image(&decoder, &png)
        .expect("a late UNSUPPORTED must fall through, not fail");
    assert_eq!(decoded.as_samples(), rgb8);

    let mut encoder = PngEncoder::new();
    encoder.push_backend(AbiDeflater::new(AbiEnc {
        status: Some(Status::UNSUPPORTED),
        seen_quality: Arc::new(Mutex::new(Vec::new())),
    }));
    let mut out = Vec::new();
    encoder
        .encode_image(
            ImageRef::<Rgb8>::new(&rgb8, dims()).expect("rgb8"),
            &mut out,
        )
        .expect("encode");
    assert_eq!(
        hex(&out),
        golden("rgb8"),
        "the gamut-deflate tail produced it"
    );
}

#[test]
fn abi_other_statuses_propagate_as_typed_errors() {
    let (png, rgb8) = sample_png();
    let mut decoder = PngDecoder::new();
    decoder.push_backend(AbiInflater::new(AbiDec {
        status: Some(Status(-42)),
        seen: Arc::new(Mutex::new(Vec::new())),
    }));
    let err = DecodeImage::<Rgb8>::decode_image(&decoder, &png)
        .expect_err("a terminal status must propagate");
    assert_eq!(
        err.static_message().unwrap(),
        "PNG: IDAT codec-abi backend reported an error"
    );

    let mut encoder = PngEncoder::new();
    encoder.push_backend(AbiDeflater::new(AbiEnc {
        status: Some(Status(7)),
        seen_quality: Arc::new(Mutex::new(Vec::new())),
    }));
    let mut out = Vec::new();
    let err = encoder
        .encode_image(
            ImageRef::<Rgb8>::new(&rgb8, dims()).expect("rgb8"),
            &mut out,
        )
        .expect_err("a terminal status must propagate");
    assert_eq!(
        err.static_message().unwrap(),
        "PNG: IDAT codec-abi backend reported an error"
    );
}

#[test]
fn abi_supports_false_skips_the_backend_entirely() {
    let (png, rgb8) = sample_png();
    /// Rejects every codec id, so `supports` is false.
    struct WrongCodec;
    impl Decoder for WrongCodec {
        fn supports(&mut self, _cfg: &StreamConfig) -> bool {
            false
        }
        fn decode(&mut self, _: &StreamConfig, _: &[u8], _: &ImageDesc) -> Status {
            unreachable!("never accepted")
        }
    }
    let mut decoder = PngDecoder::new();
    decoder.push_backend(AbiInflater::new(WrongCodec));
    let decoded: ImageBuf<Rgb8> =
        DecodeImage::<Rgb8>::decode_image(&decoder, &png).expect("decode");
    assert_eq!(decoded.as_samples(), rgb8);
}

#[test]
fn abi_deflater_quality_is_configurable() {
    let seen_quality = Arc::new(Mutex::new(Vec::new()));
    let mut encoder = PngEncoder::new();
    encoder.push_backend(
        AbiDeflater::new(AbiEnc {
            status: None,
            seen_quality: Arc::clone(&seen_quality),
        })
        .with_quality(11),
    );
    let rgb8 = rgb8_samples();
    let mut png = Vec::new();
    encoder
        .encode_image(
            ImageRef::<Rgb8>::new(&rgb8, dims()).expect("rgb8"),
            &mut png,
        )
        .expect("encode");
    assert_eq!(*seen_quality.lock().expect("seen"), vec![11]);
}

#[test]
fn abi_inflater_refuses_to_allocate_past_max_out() {
    // Driven directly as an `IdatInflater`: through `PngDecoder` the cap and the expected size are
    // always equal, so this defensive guard needs the trait exercised on its own.
    let mut inflater = AbiInflater::new(AbiDec {
        status: None,
        seen: Arc::new(Mutex::new(Vec::new())),
    });
    let info = IdatInfo::new(4, 2, 8, gamut_png::ColorType::Grayscale, 10);
    let err = inflater
        .inflate(&info, &[], 9)
        .expect_err("a 10-byte job under a 9-byte cap must be refused");
    assert_eq!(
        err.static_message().unwrap(),
        "PNG: IDAT is larger than the decoder's output budget"
    );

    // Exactly at the cap the job runs.
    let raw = vec![0u8; 10];
    let mut zlib = Vec::new();
    DeflateEncoder::new().zlib_compress(&raw, &mut zlib);
    assert_eq!(inflater.inflate(&info, &zlib, 10).expect("at the cap"), raw);
}

#[test]
fn abi_configs_describe_the_job() {
    let info = IdatInfo::new(4, 2, 16, gamut_png::ColorType::Truecolor, 10);
    let cfg = AbiInflater::<AbiDec>::config(&info);
    assert!(cfg.is_abi_current());
    assert_eq!(cfg.codec_id, CODEC_ID_ZLIB);
    assert_eq!(cfg.width, 4);
    assert_eq!(cfg.height, 2);
    assert_eq!(cfg.bit_depth, 16);
    assert_eq!(cfg.extradata_len, 0);

    let deflater = AbiDeflater::new(AbiEnc {
        status: None,
        seen_quality: Arc::new(Mutex::new(Vec::new())),
    });
    assert_eq!(deflater.config().codec_id, CODEC_ID_ZLIB);
    assert_eq!(deflater.config().quality, 100);
    assert_eq!(deflater.with_quality(3).config().quality, 3);
}
