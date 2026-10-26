//! The codestream backend registries (issue #275): push order, the decline/fall-through contract,
//! the built-in `vp8`/`vp8l` tails, codestream-discriminant routing, `Clone` sharing, and the
//! `gamut-codec-abi` adapters in both directions.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use gamut_codec_abi as abi;
use gamut_color::{ChromaUpsampling, ColorRange, Yuv420};
use gamut_core::{
    DecodeImage, Dimensions, EncodeImage, Error, ErrorKind, ImageBuf, ImageRef, Result, Rgb8, Rgba8,
};
use gamut_riff::{RiffReader, WebpChunkId};
use gamut_webp::{
    AbiDecoderBackend, AbiEncoderBackend, CodestreamInfo, DecodedRaster, PIXEL_FORMAT_ARGB,
    PIXEL_FORMAT_YUV420, RasterRef, WebpCodestream, WebpCodestreamDecoder, WebpCodestreamEncoder,
    WebpDecoder, WebpEncodeRequest, WebpEncoder,
};

#[track_caller]
fn assert_error<T>(result: Result<T>, kind: ErrorKind, message: &'static str) {
    match result {
        Err(error) => {
            assert_eq!(error.kind(), kind);
            assert_eq!(error.static_message(), Some(message));
        }
        Ok(_) => panic!("expected {kind:?}: {message}"),
    }
}

// ================================================================================================
// Fixtures
// ================================================================================================

fn dims(width: u32, height: u32) -> Dimensions {
    Dimensions { width, height }
}

/// A deterministic RGB gradient.
fn rgb(w: u32, h: u32) -> Vec<u8> {
    (0..w * h)
        .flat_map(|i| {
            let (x, y) = (i % w, i / w);
            [(x * 7) as u8, (y * 11) as u8, (x ^ y) as u8]
        })
        .collect()
}

fn encode_rgb(enc: &WebpEncoder, px: &[u8], d: Dimensions) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    enc.encode_image(ImageRef::<Rgb8>::new(px, d).expect("fixture"), &mut out)?;
    Ok(out)
}

/// The payload of the first chunk with `id` in `file`.
fn chunk_payload(file: &[u8], id: WebpChunkId) -> Vec<u8> {
    RiffReader::new(file)
        .expect("riff")
        .filter_map(std::result::Result::ok)
        .find(|c| WebpChunkId::from(c.fourcc) == id)
        .expect("chunk present")
        .payload
        .to_vec()
}

/// A valid default-encoded lossless file and its `VP8L` chunk payload.
fn lossless_fixture() -> (Vec<u8>, Vec<u8>) {
    let file = encode_rgb(&WebpEncoder::lossless(), &rgb(8, 8), dims(8, 8)).expect("encode");
    let payload = chunk_payload(&file, WebpChunkId::Vp8l);
    (file, payload)
}

/// A valid default-encoded lossy file and its `VP8 ` chunk payload.
fn lossy_fixture() -> (Vec<u8>, Vec<u8>) {
    let file = encode_rgb(&WebpEncoder::lossy(60), &rgb(16, 16), dims(16, 16)).expect("encode");
    let payload = chunk_payload(&file, WebpChunkId::Vp8);
    (file, payload)
}

// ================================================================================================
// Scripted Rust backends
// ================================================================================================

/// What a scripted backend does once it accepts a job.
#[derive(Clone)]
enum Outcome {
    /// Return this VP8L raster (or, for the encoder, these payload bytes).
    Argb(Vec<u32>),
    /// Return a YUV raster of the given constant luma — used to prove discriminant mismatches are
    /// rejected as well as to serve genuine `VP8 ` jobs.
    Yuv(u8),
    /// Return these bytes as the encoded chunk payload.
    Bytes(Vec<u8>),
    /// Fail with this error.
    Fail(&'static str),
}

/// A decode backend that accepts only `accepts` (a codestream, or nothing) and records every call.
#[derive(Clone)]
struct ScriptedDecoder {
    accepts: Option<WebpCodestream>,
    outcome: Outcome,
    log: Arc<Log>,
    name: &'static str,
}

/// Shared call log so a test can assert exactly which backends were consulted, in order.
#[derive(Default)]
struct Log {
    supports: std::sync::Mutex<Vec<&'static str>>,
    ran: std::sync::Mutex<Vec<&'static str>>,
    clones: AtomicUsize,
}

impl Log {
    fn supports(&self) -> Vec<&'static str> {
        self.supports.lock().expect("log").clone()
    }
    fn ran(&self) -> Vec<&'static str> {
        self.ran.lock().expect("log").clone()
    }
}

impl ScriptedDecoder {
    fn new(
        name: &'static str,
        accepts: Option<WebpCodestream>,
        outcome: Outcome,
        log: &Arc<Log>,
    ) -> Self {
        Self {
            accepts,
            outcome,
            log: Arc::clone(log),
            name,
        }
    }
}

impl WebpCodestreamDecoder for ScriptedDecoder {
    fn supports(&mut self, info: &CodestreamInfo) -> bool {
        self.log.supports.lock().expect("log").push(self.name);
        self.accepts == Some(info.codestream())
    }

    fn decode(&mut self, info: &CodestreamInfo, _payload: &[u8]) -> Result<DecodedRaster> {
        self.log.ran.lock().expect("log").push(self.name);
        self.log.clones.fetch_add(1, Ordering::SeqCst);
        let d = info.dimensions();
        match &self.outcome {
            Outcome::Argb(pixels) => Ok(DecodedRaster::Argb {
                dimensions: d,
                pixels: pixels.clone(),
            }),
            Outcome::Yuv(luma) => {
                let (cw, ch) = (
                    Yuv420::chroma_width(d.width) as usize,
                    Yuv420::chroma_height(d.height) as usize,
                );
                Ok(DecodedRaster::Yuv420(Yuv420::new(
                    d.width,
                    d.height,
                    vec![*luma; (d.width * d.height) as usize],
                    vec![128; cw * ch],
                    vec![128; cw * ch],
                )?))
            }
            Outcome::Bytes(_) => panic!("byte outcome is encode-only"),
            Outcome::Fail(msg) => Err(Error::InvalidInput(msg)),
        }
    }
}

/// An encode backend mirroring [`ScriptedDecoder`].
#[derive(Clone)]
struct ScriptedEncoder {
    accepts: Option<WebpCodestream>,
    outcome: Outcome,
    log: Arc<Log>,
    name: &'static str,
}

impl ScriptedEncoder {
    fn new(
        name: &'static str,
        accepts: Option<WebpCodestream>,
        outcome: Outcome,
        log: &Arc<Log>,
    ) -> Self {
        Self {
            accepts,
            outcome,
            log: Arc::clone(log),
            name,
        }
    }
}

impl WebpCodestreamEncoder for ScriptedEncoder {
    fn supports(&mut self, req: &WebpEncodeRequest) -> bool {
        self.log.supports.lock().expect("log").push(self.name);
        self.accepts == Some(req.codestream())
    }

    fn encode(&mut self, _req: &WebpEncodeRequest, _raster: &RasterRef<'_>) -> Result<Vec<u8>> {
        self.log.ran.lock().expect("log").push(self.name);
        self.log.clones.fetch_add(1, Ordering::SeqCst);
        match &self.outcome {
            Outcome::Bytes(bytes) => Ok(bytes.clone()),
            Outcome::Fail(msg) => Err(Error::InvalidInput(msg)),
            _ => panic!("raster outcomes are decode-only"),
        }
    }
}

// ================================================================================================
// Decode registry
// ================================================================================================

#[test]
fn decode_uses_the_first_accepting_backend_in_push_order() {
    let (file, _) = lossless_fixture();
    let log = Arc::new(Log::default());
    let mut dec = WebpDecoder::new();
    dec.push_backend(ScriptedDecoder::new(
        "first",
        Some(WebpCodestream::Vp8l),
        Outcome::Argb(vec![0xff00_0000 | 0x0011_2233; 64]),
        &log,
    ))
    .push_backend(ScriptedDecoder::new(
        "second",
        Some(WebpCodestream::Vp8l),
        Outcome::Argb(vec![0xffff_ffff; 64]),
        &log,
    ));

    let got: ImageBuf<Rgb8> = dec.decode_image(&file).expect("decode");
    assert_eq!(got.dimensions(), dims(8, 8));
    assert_eq!(got.as_samples(), [0x11u8, 0x22, 0x33].repeat(64).as_slice());
    // The second backend is never even asked once the first accepts.
    assert_eq!(log.supports(), vec!["first"]);
    assert_eq!(log.ran(), vec!["first"]);
}

#[test]
fn decode_skips_decliners_then_uses_the_acceptor() {
    let (file, _) = lossless_fixture();
    let log = Arc::new(Log::default());
    let mut dec = WebpDecoder::new();
    dec.push_backend(ScriptedDecoder::new(
        "declines",
        None,
        Outcome::Fail("must not run"),
        &log,
    ))
    .push_backend(ScriptedDecoder::new(
        "accepts",
        Some(WebpCodestream::Vp8l),
        Outcome::Argb(vec![0xff01_0203; 64]),
        &log,
    ));

    let got: ImageBuf<Rgb8> = dec.decode_image(&file).expect("decode");
    assert_eq!(got.as_samples(), [1u8, 2, 3].repeat(64).as_slice());
    assert_eq!(log.supports(), vec!["declines", "accepts"]);
    assert_eq!(log.ran(), vec!["accepts"]);
}

#[test]
fn decode_falls_back_to_the_builtin_tail_when_all_decline() {
    let (file, _) = lossless_fixture();
    let baseline: ImageBuf<Rgb8> = WebpDecoder::new().decode_image(&file).expect("baseline");

    let log = Arc::new(Log::default());
    let mut dec = WebpDecoder::new();
    dec.push_backend(ScriptedDecoder::new(
        "a",
        None,
        Outcome::Fail("must not run"),
        &log,
    ))
    .push_backend(ScriptedDecoder::new(
        "b",
        None,
        Outcome::Fail("must not run"),
        &log,
    ));

    let got: ImageBuf<Rgb8> = dec.decode_image(&file).expect("decode");
    assert_eq!(got.dimensions(), baseline.dimensions());
    assert_eq!(got.as_samples(), baseline.as_samples());
    assert_eq!(log.supports(), vec!["a", "b"]);
    assert!(log.ran().is_empty(), "no backend may run after declining");
}

#[test]
fn decode_propagates_an_accepted_backends_error_without_running_the_tail() {
    // The payload is a perfectly good VP8L stream: if the built-in tail were consulted the decode
    // would succeed. It must not be.
    let (file, _) = lossless_fixture();
    let via_tail: Result<ImageBuf<Rgb8>> = WebpDecoder::new().decode_image(&file);
    assert!(via_tail.is_ok(), "fixture must decode with the tail");

    let log = Arc::new(Log::default());
    let mut dec = WebpDecoder::new();
    dec.push_backend(ScriptedDecoder::new(
        "boom",
        Some(WebpCodestream::Vp8l),
        Outcome::Fail("backend exploded"),
        &log,
    ));
    let err: Result<ImageBuf<Rgb8>> = dec.decode_image(&file);
    assert!(matches!(err, Err(Error::InvalidInput("backend exploded"))));
    assert_eq!(log.ran(), vec!["boom"]);
}

#[test]
fn decode_routing_is_per_codestream() {
    let log = Arc::new(Log::default());
    let (lossless, _) = lossless_fixture();
    let (lossy, _) = lossy_fixture();

    // A VP8-only backend must not be *chosen* for a VP8L stream (it is asked, and declines).
    let mut dec = WebpDecoder::new();
    dec.push_backend(ScriptedDecoder::new(
        "vp8only",
        Some(WebpCodestream::Vp8),
        Outcome::Yuv(200),
        &log,
    ));
    let _: ImageBuf<Rgb8> = dec.decode_image(&lossless).expect("lossless via tail");
    assert!(log.ran().is_empty(), "VP8 backend ran on a VP8L stream");

    // On a VP8 stream the same backend accepts, and its constant-luma raster reaches the output.
    let got: ImageBuf<Rgb8> = dec.decode_image(&lossy).expect("lossy via backend");
    assert_eq!(log.ran(), vec!["vp8only"]);
    let expected = Yuv420::new(16, 16, vec![200u8; 256], vec![128u8; 64], vec![128u8; 64])
        .expect("yuv")
        .to_rgb8(ColorRange::Limited, ChromaUpsampling::Nearest);
    assert_eq!(got.as_samples(), expected.as_slice());

    // And a VP8L-only backend is never chosen for a VP8 stream.
    let log2 = Arc::new(Log::default());
    let mut dec2 = WebpDecoder::new();
    dec2.push_backend(ScriptedDecoder::new(
        "vp8lonly",
        Some(WebpCodestream::Vp8l),
        Outcome::Argb(vec![0xffff_ffff; 256]),
        &log2,
    ));
    let _: ImageBuf<Rgb8> = dec2.decode_image(&lossy).expect("lossy via tail");
    assert_eq!(log2.supports(), vec!["vp8lonly"]);
    assert!(log2.ran().is_empty(), "VP8L backend ran on a VP8 stream");
}

#[test]
fn decode_rejects_a_raster_that_does_not_match_the_codestream() {
    let (file, _) = lossless_fixture();
    let log = Arc::new(Log::default());
    let mut dec = WebpDecoder::new();
    dec.push_backend(ScriptedDecoder::new(
        "wrong-raster",
        Some(WebpCodestream::Vp8l),
        Outcome::Yuv(64),
        &log,
    ));
    let err: Result<ImageBuf<Rgb8>> = dec.decode_image(&file);
    assert_error(
        err,
        ErrorKind::InvalidInput,
        "WebP: VP8L decode produced a YUV raster",
    );
}

#[test]
fn decode_rejects_an_argb_raster_from_a_vp8_backend() {
    let (file, _) = lossy_fixture();
    let log = Arc::new(Log::default());
    let mut dec = WebpDecoder::new();
    dec.push_backend(ScriptedDecoder::new(
        "wrong-raster",
        Some(WebpCodestream::Vp8),
        Outcome::Argb(vec![0xffff_ffff; 256]),
        &log,
    ));
    let err: Result<ImageBuf<Rgb8>> = dec.decode_image(&file);
    assert_error(
        err,
        ErrorKind::InvalidInput,
        "WebP: VP8 decode produced an ARGB raster",
    );
    // Same rejection on the RGBA path.
    let err: Result<ImageBuf<Rgba8>> = dec.decode_image(&file);
    assert_error(
        err,
        ErrorKind::InvalidInput,
        "WebP: VP8 decode produced an ARGB raster",
    );
}

/// A backend that always panics, used to poison the registry lock.
struct PanickingDecoder;

impl WebpCodestreamDecoder for PanickingDecoder {
    fn supports(&mut self, _info: &CodestreamInfo) -> bool {
        panic!("backend panic");
    }

    fn decode(&mut self, _info: &CodestreamInfo, _payload: &[u8]) -> Result<DecodedRaster> {
        unreachable!("supports panics first")
    }
}

/// An encode backend that always panics.
struct PanickingEncoder;

impl WebpCodestreamEncoder for PanickingEncoder {
    fn supports(&mut self, _req: &WebpEncodeRequest) -> bool {
        panic!("backend panic");
    }

    fn encode(&mut self, _req: &WebpEncodeRequest, _raster: &RasterRef<'_>) -> Result<Vec<u8>> {
        unreachable!("supports panics first")
    }
}

#[test]
fn a_panicking_decode_backend_poisons_the_registry() {
    let (file, _) = lossless_fixture();
    let mut dec = WebpDecoder::new();
    dec.push_backend(PanickingDecoder);

    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let first = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _: Result<ImageBuf<Rgb8>> = dec.decode_image(&file);
    }));
    std::panic::set_hook(hook);
    assert!(first.is_err(), "the backend panic must surface");

    // The lock is now poisoned: the next decode reports it instead of using stale backend state,
    // which is the half that matters -- a silent retry over a half-updated registry is the
    // failure this guard exists to prevent.
    let err: Result<ImageBuf<Rgb8>> = dec.decode_image(&file);
    assert_error(
        err,
        ErrorKind::InvalidInput,
        "WebP: a codestream backend panicked (registry lock poisoned)",
    );
}

#[test]
fn a_panicking_encode_backend_poisons_the_registry() {
    // The encoder keeps its own registry, so this can be wrong while the decoder path above is
    // right. Same contract, separately reachable.
    let mut enc = WebpEncoder::lossless();
    enc.push_backend(PanickingEncoder);

    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let first = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = encode_rgb(&enc, &rgb(8, 8), dims(8, 8));
    }));
    std::panic::set_hook(hook);
    assert!(first.is_err(), "the backend panic must surface");

    assert_error(
        encode_rgb(&enc, &rgb(8, 8), dims(8, 8)),
        ErrorKind::InvalidInput,
        "WebP: a codestream backend panicked (registry lock poisoned)",
    );
}

#[test]
fn abi_adapters_return_their_wrapped_backend() {
    let decoder = AbiDecoderBackend::new(AbiFakeDecoder {
        accepts: 7,
        status: abi::Status::OK,
        fill: 3,
        seen_strides: [0; abi::MAX_PLANES],
    });
    assert_eq!(decoder.into_inner().accepts, 7);
}

#[test]
fn decode_skips_the_registry_when_the_codestream_header_is_unparseable() {
    // A malformed VP8 payload has no peekable dimensions, so no backend is consulted at all and the
    // built-in decoder reports the parse error.
    let file = gamut_riff::write_simple_lossy(&[0x9d, 0x01, 0x2a]).unwrap();
    let log = Arc::new(Log::default());
    let mut dec = WebpDecoder::new();
    dec.push_backend(ScriptedDecoder::new(
        "never",
        Some(WebpCodestream::Vp8),
        Outcome::Yuv(0),
        &log,
    ));
    let err: Result<ImageBuf<Rgb8>> = dec.decode_image(&file);
    assert!(err.is_err());
    assert!(log.supports().is_empty());
    assert!(log.ran().is_empty());
}

#[test]
fn decode_backends_also_serve_the_rgba_path() {
    let (file, _) = lossless_fixture();
    let log = Arc::new(Log::default());
    let mut dec = WebpDecoder::new();
    dec.push_backend(ScriptedDecoder::new(
        "argb",
        Some(WebpCodestream::Vp8l),
        Outcome::Argb(vec![0x8011_2233; 64]),
        &log,
    ));
    let got: ImageBuf<Rgba8> = dec.decode_image(&file).expect("decode");
    assert_eq!(
        got.as_samples(),
        [0x11u8, 0x22, 0x33, 0x80].repeat(64).as_slice()
    );
    assert_eq!(log.ran(), vec!["argb"]);
}

// ================================================================================================
// Encode registry
// ================================================================================================

#[test]
fn encode_uses_the_first_accepting_backend_in_push_order() {
    let log = Arc::new(Log::default());
    let mut enc = WebpEncoder::lossless();
    enc.push_backend(ScriptedEncoder::new(
        "first",
        Some(WebpCodestream::Vp8l),
        Outcome::Bytes(vec![0xaa, 0xbb, 0xcc]),
        &log,
    ))
    .push_backend(ScriptedEncoder::new(
        "second",
        Some(WebpCodestream::Vp8l),
        Outcome::Bytes(vec![0xff]),
        &log,
    ));

    let file = encode_rgb(&enc, &rgb(8, 8), dims(8, 8)).expect("encode");
    assert_eq!(
        chunk_payload(&file, WebpChunkId::Vp8l),
        vec![0xaa, 0xbb, 0xcc]
    );
    assert_eq!(log.supports(), vec!["first"]);
    assert_eq!(log.ran(), vec!["first"]);
}

#[test]
fn encode_skips_decliners_and_falls_back_to_the_builtin_tail() {
    let baseline = encode_rgb(&WebpEncoder::lossless(), &rgb(8, 8), dims(8, 8)).expect("baseline");

    let log = Arc::new(Log::default());
    let mut enc = WebpEncoder::lossless();
    enc.push_backend(ScriptedEncoder::new(
        "a",
        None,
        Outcome::Fail("must not run"),
        &log,
    ))
    .push_backend(ScriptedEncoder::new(
        "b",
        None,
        Outcome::Fail("must not run"),
        &log,
    ));
    let file = encode_rgb(&enc, &rgb(8, 8), dims(8, 8)).expect("encode");
    assert_eq!(
        file, baseline,
        "all-decline must be byte-identical to default"
    );
    assert_eq!(log.supports(), vec!["a", "b"]);
    assert!(log.ran().is_empty());
}

#[test]
fn encode_propagates_an_accepted_backends_error_without_running_the_tail() {
    let log = Arc::new(Log::default());
    let mut enc = WebpEncoder::lossy(60);
    enc.push_backend(ScriptedEncoder::new(
        "boom",
        Some(WebpCodestream::Vp8),
        Outcome::Fail("encoder exploded"),
        &log,
    ));
    let err = encode_rgb(&enc, &rgb(16, 16), dims(16, 16));
    assert!(matches!(err, Err(Error::InvalidInput("encoder exploded"))));
    assert_eq!(log.ran(), vec!["boom"]);
}

#[test]
fn encode_routing_is_per_codestream() {
    let log = Arc::new(Log::default());
    // A VP8L-only backend must not be chosen for a lossy (VP8) encode.
    let mut lossy = WebpEncoder::lossy(60);
    lossy.push_backend(ScriptedEncoder::new(
        "vp8lonly",
        Some(WebpCodestream::Vp8l),
        Outcome::Bytes(vec![0xde, 0xad]),
        &log,
    ));
    let baseline = encode_rgb(&WebpEncoder::lossy(60), &rgb(16, 16), dims(16, 16)).expect("base");
    let file = encode_rgb(&lossy, &rgb(16, 16), dims(16, 16)).expect("encode");
    assert_eq!(file, baseline);
    assert_eq!(log.supports(), vec!["vp8lonly"]);
    assert!(log.ran().is_empty());

    // A VP8-only backend is chosen for that same lossy encode.
    let log2 = Arc::new(Log::default());
    let mut enc = WebpEncoder::lossy(60);
    enc.push_backend(ScriptedEncoder::new(
        "vp8only",
        Some(WebpCodestream::Vp8),
        Outcome::Bytes(vec![0x01, 0x02, 0x03, 0x04]),
        &log2,
    ));
    let file = encode_rgb(&enc, &rgb(16, 16), dims(16, 16)).expect("encode");
    assert_eq!(
        chunk_payload(&file, WebpChunkId::Vp8),
        vec![0x01, 0x02, 0x03, 0x04]
    );
    assert_eq!(log2.ran(), vec!["vp8only"]);
}

#[test]
fn encode_backend_serves_the_transparent_extended_path_while_alph_stays_container_side() {
    // A transparent RGBA lossy encode: the backend supplies the `VP8 ` payload, but the `ALPH`
    // chunk is still produced container-side and matches the default encoder's bytes exactly.
    let (w, h) = (16u32, 16u32);
    let rgba: Vec<u8> = (0..w * h)
        .flat_map(|i| [10u8, 20, 30, (i & 0x7f) as u8])
        .collect();
    let mut baseline_file = Vec::new();
    WebpEncoder::lossy(60)
        .encode_image(
            ImageRef::<Rgba8>::new(&rgba, dims(w, h)).expect("fixture"),
            &mut baseline_file,
        )
        .expect("baseline");

    let log = Arc::new(Log::default());
    let mut enc = WebpEncoder::lossy(60);
    enc.push_backend(ScriptedEncoder::new(
        "vp8",
        Some(WebpCodestream::Vp8),
        Outcome::Bytes(vec![0x5a; 5]),
        &log,
    ));
    let mut file = Vec::new();
    enc.encode_image(
        ImageRef::<Rgba8>::new(&rgba, dims(w, h)).expect("fixture"),
        &mut file,
    )
    .expect("encode");

    assert_eq!(chunk_payload(&file, WebpChunkId::Vp8), vec![0x5a; 5]);
    assert_eq!(
        chunk_payload(&file, WebpChunkId::Alpha),
        chunk_payload(&baseline_file, WebpChunkId::Alpha),
        "ALPH must stay container-side and unchanged"
    );
    assert_eq!(log.ran(), vec!["vp8"]);
}

// ================================================================================================
// Clone sharing
// ================================================================================================

#[test]
fn clone_shares_encoder_backends() {
    let log = Arc::new(Log::default());
    let mut enc = WebpEncoder::lossless();
    enc.push_backend(ScriptedEncoder::new(
        "shared",
        Some(WebpCodestream::Vp8l),
        Outcome::Bytes(vec![0x77]),
        &log,
    ));
    let clone = enc.clone();
    let original_file = encode_rgb(&enc, &rgb(8, 8), dims(8, 8)).expect("encode");
    let clone_file = encode_rgb(&clone, &rgb(8, 8), dims(8, 8)).expect("encode");

    assert_eq!(chunk_payload(&clone_file, WebpChunkId::Vp8l), vec![0x77]);
    assert_eq!(original_file, clone_file);
    // The same backend object served both encoders — two runs, one instance.
    assert_eq!(log.ran(), vec!["shared", "shared"]);
    assert_eq!(log.clones.load(Ordering::SeqCst), 2);
}

#[test]
fn clone_shares_decoder_backends() {
    let (file, _) = lossless_fixture();
    let log = Arc::new(Log::default());
    let mut dec = WebpDecoder::new();
    dec.push_backend(ScriptedDecoder::new(
        "shared",
        Some(WebpCodestream::Vp8l),
        Outcome::Argb(vec![0xff04_0506; 64]),
        &log,
    ));
    let clone = dec.clone();
    let a: ImageBuf<Rgb8> = dec.decode_image(&file).expect("decode");
    let b: ImageBuf<Rgb8> = clone.decode_image(&file).expect("decode");
    assert_eq!(a.as_samples(), b.as_samples());
    assert_eq!(b.as_samples(), [4u8, 5, 6].repeat(64).as_slice());
    assert_eq!(log.ran(), vec!["shared", "shared"]);
}

#[test]
fn debug_reports_the_backend_count() {
    let mut enc = WebpEncoder::lossless();
    assert!(format!("{enc:?}").contains("backends: 0"));
    let log = Arc::new(Log::default());
    enc.push_backend(ScriptedEncoder::new(
        "x",
        None,
        Outcome::Bytes(Vec::new()),
        &log,
    ));
    assert!(format!("{enc:?}").contains("backends: 1"));

    let mut dec = WebpDecoder::new();
    assert!(format!("{dec:?}").contains("backends: 0"));
    dec.push_backend(ScriptedDecoder::new("y", None, Outcome::Yuv(0), &log));
    assert!(format!("{dec:?}").contains("backends: 1"));
}

// ================================================================================================
// gamut-codec-abi adapters
// ================================================================================================

/// A scripted `gamut-codec-abi` decoder: accepts one `codec_id`, then fills the output planes with
/// `fill` (or returns `status` when it is not OK).
struct AbiFakeDecoder {
    accepts: u32,
    status: abi::Status,
    fill: u8,
    /// The plane strides the adapter described, recorded so the test can pin them exactly.
    seen_strides: [usize; abi::MAX_PLANES],
}

impl abi::Decoder for AbiFakeDecoder {
    fn supports(&mut self, cfg: &abi::StreamConfig) -> bool {
        cfg.codec_id == self.accepts
    }

    fn decode(
        &mut self,
        _cfg: &abi::StreamConfig,
        _codestream: &[u8],
        out: &abi::ImageDesc,
    ) -> abi::Status {
        self.seen_strides = out.strides;
        if !self.status.is_ok() {
            return self.status;
        }
        let (w, h) = (out.width as usize, out.height as usize);
        let plane_len = |i: usize| match out.pixel_format {
            PIXEL_FORMAT_ARGB => w * h * 4,
            _ if i == 0 => w * h,
            _ => w.div_ceil(2) * h.div_ceil(2),
        };
        for i in 0..out.plane_count as usize {
            // SAFETY: the adapter guarantees each of the first `plane_count` pointers is valid for
            // the plane length implied by the descriptor's format and dimensions.
            let plane = unsafe { std::slice::from_raw_parts_mut(out.planes[i], plane_len(i)) };
            plane.fill(self.fill.wrapping_add(i as u8));
        }
        abi::Status::OK
    }
}

/// A scripted `gamut-codec-abi` encoder: accepts one `codec_id`, then streams `chunks` (or returns
/// `status` when it is not OK). Records the descriptor it was handed.
struct AbiFakeEncoder {
    accepts: u32,
    status: abi::Status,
    chunks: Vec<Vec<u8>>,
    seen: Option<(u32, u32, u32, u32)>,
    /// The plane strides the adapter described, recorded so the test can pin them exactly.
    seen_strides: [usize; abi::MAX_PLANES],
}

impl abi::Encoder for AbiFakeEncoder {
    fn supports(&mut self, cfg: &abi::EncodeConfig) -> bool {
        cfg.codec_id == self.accepts
    }

    fn encode(
        &mut self,
        cfg: &abi::EncodeConfig,
        image: &abi::ImageDesc,
        sink: &mut dyn FnMut(&[u8]) -> abi::Status,
    ) -> abi::Status {
        self.seen = Some((
            cfg.quality,
            image.pixel_format,
            image.plane_count,
            image.width,
        ));
        self.seen_strides = image.strides;
        if !self.status.is_ok() {
            return self.status;
        }
        for chunk in &self.chunks {
            let status = sink(chunk);
            if !status.is_ok() {
                return status;
            }
        }
        abi::Status::OK
    }
}

#[test]
fn abi_decode_adapter_declines_a_codec_id_it_does_not_accept() {
    let mut backend = AbiDecoderBackend::new(AbiFakeDecoder {
        accepts: WebpCodestream::Vp8.codec_id(),
        status: abi::Status::OK,
        fill: 0,
        seen_strides: [0; abi::MAX_PLANES],
    });
    assert!(!backend.supports(&CodestreamInfo::new(WebpCodestream::Vp8l, dims(4, 4))));
    assert!(backend.supports(&CodestreamInfo::new(WebpCodestream::Vp8, dims(4, 4))));
}

#[test]
fn abi_decode_adapter_round_trips_both_rasters() {
    // VP8L: one BGRA plane -> 0xAARRGGBB pixels.
    let mut lossless = AbiDecoderBackend::new(AbiFakeDecoder {
        accepts: WebpCodestream::Vp8l.codec_id(),
        status: abi::Status::OK,
        fill: 0x11,
        seen_strides: [0; abi::MAX_PLANES],
    });
    let info = CodestreamInfo::new(WebpCodestream::Vp8l, dims(2, 2));
    match lossless.decode(&info, &[]).expect("decode") {
        DecodedRaster::Argb { dimensions, pixels } => {
            assert_eq!(dimensions, dims(2, 2));
            assert_eq!(pixels, vec![0x1111_1111u32; 4]);
        }
        other => panic!("expected ARGB, got {other:?}"),
    }
    // One packed plane: stride is exactly `width * 4` bytes, the rest unused.
    assert_eq!(lossless.into_inner().seen_strides, [8, 0, 0, 0]);

    // VP8: three planes, filled 0x20 / 0x21 / 0x22 by plane index.
    let mut lossy = AbiDecoderBackend::new(AbiFakeDecoder {
        accepts: WebpCodestream::Vp8.codec_id(),
        status: abi::Status::OK,
        fill: 0x20,
        seen_strides: [0; abi::MAX_PLANES],
    });
    let info = CodestreamInfo::new(WebpCodestream::Vp8, dims(4, 2));
    match lossy.decode(&info, &[]).expect("decode") {
        DecodedRaster::Yuv420(yuv) => {
            assert_eq!((yuv.width(), yuv.height()), (4, 2));
            assert_eq!(yuv.y(), vec![0x20u8; 8].as_slice());
            assert_eq!(yuv.u(), vec![0x21u8; 2].as_slice());
            assert_eq!(yuv.v(), vec![0x22u8; 2].as_slice());
        }
        other => panic!("expected YUV, got {other:?}"),
    }
    // Tightly packed planes: luma stride `width`, chroma stride `ceil(width / 2)`.
    assert_eq!(lossy.into_inner().seen_strides, [4, 2, 2, 0]);
}

#[test]
fn abi_decode_adapter_maps_statuses_to_typed_errors() {
    for codestream in [WebpCodestream::Vp8, WebpCodestream::Vp8l] {
        let info = CodestreamInfo::new(codestream, dims(2, 2));
        // A late UNSUPPORTED cannot re-open the fallback, so it is a typed Unsupported error.
        let mut late = AbiDecoderBackend::new(AbiFakeDecoder {
            accepts: codestream.codec_id(),
            status: abi::Status::UNSUPPORTED,
            fill: 0,
            seen_strides: [0; abi::MAX_PLANES],
        });
        assert_error(
            late.decode(&info, &[]),
            ErrorKind::Unsupported,
            "WebP: codec-abi decode backend declined after accepting the job",
        );
        // Any other non-OK status is a backend failure.
        let mut failed = AbiDecoderBackend::new(AbiFakeDecoder {
            accepts: codestream.codec_id(),
            status: abi::Status(-42),
            fill: 0,
            seen_strides: [0; abi::MAX_PLANES],
        });
        assert_error(
            failed.decode(&info, &[]),
            ErrorKind::InvalidInput,
            "WebP: codec-abi decode backend failed",
        );
    }
}

#[test]
fn abi_encode_adapter_streams_chunks_and_describes_the_image() {
    // VP8L: the ARGB pixels are staged as one BGRA plane and the concatenated sink output is the
    // returned chunk payload.
    let mut lossless = AbiEncoderBackend::new(AbiFakeEncoder {
        accepts: WebpCodestream::Vp8l.codec_id(),
        status: abi::Status::OK,
        chunks: vec![vec![1, 2], vec![3], vec![4, 5, 6]],
        seen: None,
        seen_strides: [0; abi::MAX_PLANES],
    });
    let req = WebpEncodeRequest::new(WebpCodestream::Vp8l, dims(2, 1), 90);
    assert!(lossless.supports(&req));
    let pixels = [0xff00_0000u32, 0x0000_00ff];
    let payload = lossless
        .encode(
            &req,
            &RasterRef::Argb {
                dimensions: dims(2, 1),
                pixels: &pixels,
            },
        )
        .expect("encode");
    assert_eq!(payload, vec![1, 2, 3, 4, 5, 6]);
    let seen = lossless.into_inner();
    assert_eq!(seen.seen, Some((90, PIXEL_FORMAT_ARGB, 1, 2)));
    assert_eq!(seen.seen_strides, [8, 0, 0, 0], "stride is width * 4 bytes");

    // VP8: the YUV planes are described in place.
    let mut lossy = AbiEncoderBackend::new(AbiFakeEncoder {
        accepts: WebpCodestream::Vp8.codec_id(),
        status: abi::Status::OK,
        chunks: vec![vec![0xab]],
        seen: None,
        seen_strides: [0; abi::MAX_PLANES],
    });
    let req = WebpEncodeRequest::new(WebpCodestream::Vp8, dims(4, 2), 35);
    let yuv = Yuv420::new(4, 2, vec![7; 8], vec![8; 2], vec![9; 2]).expect("yuv");
    assert_eq!(
        lossy
            .encode(&req, &RasterRef::Yuv420(&yuv))
            .expect("encode"),
        vec![0xab]
    );
    let seen = lossy.into_inner();
    assert_eq!(seen.seen, Some((35, PIXEL_FORMAT_YUV420, 3, 4)));
    assert_eq!(
        seen.seen_strides,
        [4, 2, 2, 0],
        "luma stride is width, chroma ceil(width / 2)"
    );
}

#[test]
fn abi_encode_adapter_declines_and_maps_statuses() {
    let mut backend = AbiEncoderBackend::new(AbiFakeEncoder {
        accepts: WebpCodestream::Vp8.codec_id(),
        status: abi::Status::OK,
        chunks: Vec::new(),
        seen: None,
        seen_strides: [0; abi::MAX_PLANES],
    });
    assert!(!backend.supports(&WebpEncodeRequest::new(
        WebpCodestream::Vp8l,
        dims(1, 1),
        50
    )));

    let yuv = Yuv420::new(2, 2, vec![0; 4], vec![0; 1], vec![0; 1]).expect("yuv");
    let req = WebpEncodeRequest::new(WebpCodestream::Vp8, dims(2, 2), 50);
    let mut late = AbiEncoderBackend::new(AbiFakeEncoder {
        accepts: WebpCodestream::Vp8.codec_id(),
        status: abi::Status::UNSUPPORTED,
        chunks: Vec::new(),
        seen: None,
        seen_strides: [0; abi::MAX_PLANES],
    });
    assert_error(
        late.encode(&req, &RasterRef::Yuv420(&yuv)),
        ErrorKind::Unsupported,
        "WebP: codec-abi encode backend declined after accepting the job",
    );
    let mut failed = AbiEncoderBackend::new(AbiFakeEncoder {
        accepts: WebpCodestream::Vp8.codec_id(),
        status: abi::Status(9),
        chunks: Vec::new(),
        seen: None,
        seen_strides: [0; abi::MAX_PLANES],
    });
    assert_error(
        failed.encode(&req, &RasterRef::Yuv420(&yuv)),
        ErrorKind::InvalidInput,
        "WebP: codec-abi encode backend failed",
    );
}

#[test]
fn abi_adapters_plug_into_the_registries_end_to_end() {
    // Decode: an ABI backend accepted by the registry supplies the picture.
    let (file, _) = lossless_fixture();
    let mut dec = WebpDecoder::new();
    dec.push_backend(AbiDecoderBackend::new(AbiFakeDecoder {
        accepts: WebpCodestream::Vp8l.codec_id(),
        status: abi::Status::OK,
        fill: 0x33,
        seen_strides: [0; abi::MAX_PLANES],
    }));
    // The fake fills all 32 ARGB bits with 0x33, so the raster is semi-transparent (alpha 0x33) and
    // Rgba8 is the layout that holds it losslessly. Requesting Rgb8 would discard that alpha, which
    // now needs an explicit AlphaPolicy rather than happening silently.
    let got: ImageBuf<Rgba8> = dec.decode_image(&file).expect("decode");
    assert_eq!(got.as_samples(), [0x33u8; 4].repeat(64).as_slice());

    // Encode: an ABI backend's streamed bytes become the VP8L chunk payload.
    let mut enc = WebpEncoder::lossless();
    enc.push_backend(AbiEncoderBackend::new(AbiFakeEncoder {
        accepts: WebpCodestream::Vp8l.codec_id(),
        status: abi::Status::OK,
        chunks: vec![vec![0xc0, 0xde]],
        seen: None,
        seen_strides: [0; abi::MAX_PLANES],
    }));
    let out = encode_rgb(&enc, &rgb(8, 8), dims(8, 8)).expect("encode");
    assert_eq!(chunk_payload(&out, WebpChunkId::Vp8l), vec![0xc0, 0xde]);

    // A non-matching ABI backend declines, and the built-in tail produces the default bytes.
    let mut enc2 = WebpEncoder::lossless();
    enc2.push_backend(AbiEncoderBackend::new(AbiFakeEncoder {
        accepts: WebpCodestream::Vp8.codec_id(),
        status: abi::Status::OK,
        chunks: vec![vec![0xff]],
        seen: None,
        seen_strides: [0; abi::MAX_PLANES],
    }));
    assert_eq!(
        encode_rgb(&enc2, &rgb(8, 8), dims(8, 8)).expect("encode"),
        encode_rgb(&WebpEncoder::lossless(), &rgb(8, 8), dims(8, 8)).expect("baseline")
    );
}
