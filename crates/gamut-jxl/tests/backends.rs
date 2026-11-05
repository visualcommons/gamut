//! Integration tests for the pluggable codestream backends (issue #276), exercised through the
//! crate's **public** surface only.
//!
//! The unit tests in `src/encoder.rs` / `src/decoder.rs` pin the registry's ordering and fallback
//! logic in isolation; these prove the same contract across the crate boundary against *real*
//! streams, and — most importantly — that a default encoder/decoder still produces **byte-identical**
//! output now that the built-in implementations are merely the last backend in a list.
//!
//! Both codec halves are needed to compare an encode against a decode, so the module is compiled
//! only where they are both available.
#![cfg(all(
    feature = "encode",
    feature = "decode",
    any(not(target_arch = "wasm32"), target_os = "emscripten")
))]

use gamut_codec_abi::{
    Decoder as AbiDecoderTrait, EncodeConfig, Encoder as AbiEncoderTrait, ImageDesc, Status,
    StreamConfig,
};
use gamut_core::{
    DecodeImage, Dimensions, EncodeImage, ErrorKind, Gray8, ImageBuf, ImageRef, Rgb8, Rgba8,
};
use gamut_jxl::{
    AbiJxlDecoder, AbiJxlEncoder, BitDepth, Container, Distance, Effort, JXL_CODEC_ID,
    JxlCodestreamDecoder, JxlCodestreamEncoder, JxlDecoded, JxlDecoder, JxlEncodeRequest,
    JxlEncoder, JxlImageRef, JxlOwnedSamples, JxlStreamInfo, ModularMode,
};

mod common;

/// A 16×16 textured RGB image: the fixture every encode test uses.
fn fixture() -> (Vec<u8>, Dimensions) {
    let (w, h) = (16u32, 16u32);
    let pixels = common::gen_u8(w, h, 3);
    (pixels, Dimensions::new(w, h).unwrap())
}

/// Encodes the fixture with `encoder`, returning the stream.
fn encode_fixture(encoder: &JxlEncoder) -> Vec<u8> {
    let (pixels, dims) = fixture();
    let image = ImageRef::<Rgb8>::new(&pixels, dims).expect("fixture");
    let mut out = Vec::new();
    encoder.encode_image(image, &mut out).expect("encode");
    out
}

/// A backend that declines everything and counts how often it was asked.
struct Counting {
    supports_calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl Counting {
    fn new() -> (Self, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        let counter = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        (
            Self {
                supports_calls: std::sync::Arc::clone(&counter),
            },
            counter,
        )
    }

    fn calls(counter: &std::sync::atomic::AtomicUsize) -> usize {
        counter.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl JxlCodestreamEncoder for Counting {
    fn supports(&mut self, _req: &JxlEncodeRequest<'_>) -> bool {
        self.supports_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        false
    }

    fn encode(
        &mut self,
        _req: &JxlEncodeRequest<'_>,
        _image: &JxlImageRef<'_>,
    ) -> gamut_core::Result<Vec<u8>> {
        unreachable!("a declining backend is never asked to encode")
    }
}

impl JxlCodestreamDecoder for Counting {
    fn supports(&mut self, _info: &JxlStreamInfo) -> bool {
        self.supports_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        false
    }

    fn decode(
        &mut self,
        _info: &JxlStreamInfo,
        _codestream: &[u8],
    ) -> gamut_core::Result<JxlDecoded> {
        unreachable!("a declining backend is never asked to decode")
    }
}

#[test]
fn default_encode_output_is_byte_identical_with_a_declining_backend() {
    // The invariant the whole redesign rests on: the built-in libjxl path is unchanged, so pushing
    // a backend that declines cannot perturb a single output byte.
    for configured in [
        JxlEncoder::lossless(),
        JxlEncoder::lossy(Distance::new(2.0).unwrap()).with_effort(Effort::Falcon),
        JxlEncoder::lossless().with_bit_depth(BitDepth::Eight),
    ] {
        let baseline = encode_fixture(&configured);

        let (backend, counter) = Counting::new();
        let with_backend = configured.clone().with_backend(backend);
        let observed = encode_fixture(&with_backend);

        assert_eq!(
            observed, baseline,
            "a declining backend must not change the built-in output"
        );
        // …and it really was consulted, so the equality is not vacuous.
        assert_eq!(Counting::calls(&counter), 1);
    }
}

#[test]
fn default_decode_output_is_byte_identical_with_a_declining_backend() {
    let stream = encode_fixture(&JxlEncoder::lossless());
    let (pixels, dims) = fixture();

    let baseline: ImageBuf<Rgb8> = JxlDecoder::new().decode_image(&stream).expect("decode");
    assert_eq!(baseline.as_samples(), pixels.as_slice());
    assert_eq!(baseline.dimensions(), dims);

    let (backend, counter) = Counting::new();
    let with_backend = JxlDecoder::new().with_backend(backend);
    let observed: ImageBuf<Rgb8> = with_backend.decode_image(&stream).expect("decode");

    assert_eq!(observed.as_samples(), baseline.as_samples());
    assert_eq!(observed.dimensions(), baseline.dimensions());
    assert_eq!(Counting::calls(&counter), 1);
}

/// The fields of the [`JxlEncodeRequest`] a [`Canned`] backend records: lossless flag, effort,
/// coding tool, coded bit depth.
type SeenRequest = std::sync::Arc<std::sync::Mutex<Option<(bool, Effort, ModularMode, u32)>>>;

/// A backend that returns a canned codestream, recording the request it saw.
struct Canned {
    stream: Vec<u8>,
    seen: SeenRequest,
}

impl JxlCodestreamEncoder for Canned {
    fn supports(&mut self, _req: &JxlEncodeRequest<'_>) -> bool {
        true
    }

    fn encode(
        &mut self,
        req: &JxlEncodeRequest<'_>,
        image: &JxlImageRef<'_>,
    ) -> gamut_core::Result<Vec<u8>> {
        assert_eq!(image.dimensions(), Dimensions::new(16, 16).unwrap());
        assert_eq!(image.color_channels(), 3);
        assert!(!image.has_alpha());
        *self.seen.lock().expect("test lock") = Some((
            req.is_lossless(),
            req.effort(),
            req.modular(),
            req.coded_bit_depth(),
        ));
        Ok(self.stream.clone())
    }
}

#[test]
fn a_pushed_encode_backend_supplies_the_codestream_and_sees_the_request() {
    // The stream the backend hands back is a genuine one, so the result must decode normally.
    let real = encode_fixture(&JxlEncoder::lossless());
    let seen = std::sync::Arc::new(std::sync::Mutex::new(None));

    let encoder = JxlEncoder::lossy(Distance::new(3.0).unwrap())
        .with_effort(Effort::Tortoise)
        .with_modular(ModularMode::Modular)
        .with_backend(Canned {
            stream: real.clone(),
            seen: std::sync::Arc::clone(&seen),
        });

    let produced = encode_fixture(&encoder);
    assert_eq!(
        produced, real,
        "the backend's bytes are the output verbatim"
    );

    // The request carried the configuration, not the built-in's defaults.
    let (lossless, effort, modular, depth) = seen.lock().expect("test lock").expect("backend ran");
    assert!(!lossless);
    assert_eq!(effort, Effort::Tortoise);
    assert_eq!(modular, ModularMode::Modular);
    assert_eq!(depth, 8);

    let (pixels, _) = fixture();
    let decoded: ImageBuf<Rgb8> = JxlDecoder::new().decode_image(&produced).expect("decode");
    assert_eq!(decoded.as_samples(), pixels.as_slice());
}

#[test]
fn container_requests_never_reach_a_backend() {
    // Every container-level feature is vetoed host-side, so the backend's `supports` is not called
    // even once and the built-in container path produces the stream.
    let cases: [(JxlEncoder, &[u8]); 3] = [
        (
            JxlEncoder::lossless().with_container(Container::IsoBmff),
            &[0x00, 0x00, 0x00, 0x0C],
        ),
        (
            JxlEncoder::lossless()
                .with_container(Container::IsoBmff)
                .with_exif(&[0x49, 0x49, 0x2A, 0x00]),
            &[0x00, 0x00, 0x00, 0x0C],
        ),
        (
            JxlEncoder::lossless()
                .with_container(Container::IsoBmff)
                .with_xmp(b"<x:xmpmeta xmlns:x='adobe:ns:meta/'/>"),
            &[0x00, 0x00, 0x00, 0x0C],
        ),
    ];

    for (encoder, signature) in cases {
        let (backend, counter) = Counting::new();
        let encoder = encoder.with_backend(backend);
        let stream = encode_fixture(&encoder);
        assert_eq!(
            Counting::calls(&counter),
            0,
            "a container request must not consult the registry"
        );
        assert_eq!(&stream[..4], signature, "the built-in container path ran");
    }
}

#[test]
fn jpeg_recompression_never_reaches_a_backend() {
    let (backend, counter) = Counting::new();
    let encoder = JxlEncoder::lossless().with_backend(backend);
    let mut out = Vec::new();
    // An empty JPEG is rejected by the built-in path; what matters is that the registry was skipped.
    let error = encoder.recompress_jpeg(&[], &mut out).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::InvalidInput);
    assert_eq!(error.static_message(), Some("JXL: empty JPEG input"));
    assert_eq!(Counting::calls(&counter), 0);
}

/// A decode backend that returns a canned raster, recording the info it saw.
struct CannedDecode {
    raster: JxlDecoded,
    seen: std::sync::Arc<std::sync::Mutex<Option<JxlStreamInfo>>>,
}

impl JxlCodestreamDecoder for CannedDecode {
    fn supports(&mut self, info: &JxlStreamInfo) -> bool {
        *self.seen.lock().expect("test lock") = Some(*info);
        true
    }

    fn decode(
        &mut self,
        _info: &JxlStreamInfo,
        _codestream: &[u8],
    ) -> gamut_core::Result<JxlDecoded> {
        Ok(self.raster.clone())
    }
}

#[test]
fn a_pushed_decode_backend_supplies_the_raster_and_sees_the_stream_dimensions() {
    let stream = encode_fixture(&JxlEncoder::lossless());
    let seen = std::sync::Arc::new(std::sync::Mutex::new(None));

    let raster = JxlDecoded::new(
        gamut_core::PixelFormat::Rgb8,
        Dimensions::new(2, 2).unwrap(),
        JxlOwnedSamples::U8(vec![0x42; 12]),
    )
    .expect("raster");

    let decoder = JxlDecoder::new().with_backend(CannedDecode {
        raster,
        seen: std::sync::Arc::clone(&seen),
    });

    let decoded: ImageBuf<Rgb8> = decoder.decode_image(&stream).expect("decode");
    assert_eq!(decoded.as_samples(), &[0x42; 12]);
    assert_eq!(decoded.dimensions(), Dimensions::new(2, 2).unwrap());

    // The host probed the real stream's headers for the backend's benefit.
    let info = seen.lock().expect("test lock").expect("backend ran");
    assert_eq!(info.dimensions(), Some(Dimensions::new(16, 16).unwrap()));
    assert_eq!(info.format(), gamut_core::PixelFormat::Rgb8);
}

/// A `gamut-codec-abi` encoder that streams a canned codestream through the ABI's sink.
struct AbiFixture {
    stream: Vec<u8>,
    seen_quality: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl AbiEncoderTrait for AbiFixture {
    fn supports(&mut self, cfg: &EncodeConfig) -> bool {
        cfg.codec_id == JXL_CODEC_ID
    }

    fn encode(
        &mut self,
        cfg: &EncodeConfig,
        image: &ImageDesc,
        sink: &mut dyn FnMut(&[u8]) -> Status,
    ) -> Status {
        self.seen_quality
            .store(cfg.quality as usize, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(image.width, 16);
        assert_eq!(image.height, 16);
        assert_eq!(image.bit_depth, 8);
        sink(&self.stream)
    }
}

#[test]
fn a_codec_abi_encode_backend_plugs_in_through_the_adapter() {
    let real = encode_fixture(&JxlEncoder::lossless());
    let quality = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

    let encoder = JxlEncoder::lossy(Distance::new(1.0).unwrap()).with_backend(AbiJxlEncoder::new(
        AbiFixture {
            stream: real.clone(),
            seen_quality: std::sync::Arc::clone(&quality),
        },
    ));

    let produced = encode_fixture(&encoder);
    assert_eq!(produced, real);
    // Distance 1.0 maps to quality 96 under the frozen contract.
    assert_eq!(quality.load(std::sync::atomic::Ordering::SeqCst), 96);
}

#[test]
fn a_codec_abi_encode_backend_declines_a_pinned_coding_tool() {
    // `EncodeConfig` carries a codec id and a quality — there is no field for the VarDCT/Modular
    // selection — so the adapter declines rather than let the backend answer with the other tool.
    // The job falls through to the built-in libjxl tail, which honours it.
    let stream = vec![0xFF, 0x0A, 0xDE, 0xAD];
    let quality = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

    let configured =
        JxlEncoder::lossy(Distance::new(1.0).unwrap()).with_modular(ModularMode::Modular);
    let baseline = encode_fixture(&configured);

    let encoder = configured
        .clone()
        .with_backend(AbiJxlEncoder::new(AbiFixture {
            stream: stream.clone(),
            seen_quality: std::sync::Arc::clone(&quality),
        }));

    let produced = encode_fixture(&encoder);
    assert_ne!(produced, stream, "the ABI backend must not have run");
    assert_eq!(
        produced, baseline,
        "the declined job falls through to the built-in tail unchanged"
    );
    assert_eq!(
        quality.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "encode was never reached"
    );

    // The same backend does accept the otherwise-identical job with the coding tool left to libjxl.
    let auto = JxlEncoder::lossy(Distance::new(1.0).unwrap()).with_backend(AbiJxlEncoder::new(
        AbiFixture {
            stream: stream.clone(),
            seen_quality: std::sync::Arc::clone(&quality),
        },
    ));
    assert_eq!(encode_fixture(&auto), stream);
}

/// A `gamut-codec-abi` decoder that reports a fixed status and never writes.
struct AbiDecodeFixture(Status);

impl AbiDecoderTrait for AbiDecodeFixture {
    fn supports(&mut self, cfg: &StreamConfig) -> bool {
        cfg.codec_id == JXL_CODEC_ID
    }

    fn decode(&mut self, _cfg: &StreamConfig, _stream: &[u8], _out: &ImageDesc) -> Status {
        self.0
    }
}

#[test]
fn a_codec_abi_decode_backend_s_failure_status_is_terminal() {
    // Declining is `supports`' job alone: once the backend accepted, every non-OK status — a late
    // UNSUPPORTED included — propagates instead of falling through to the built-in tail.
    let stream = encode_fixture(&JxlEncoder::lossless());
    for (status, detail) in [
        (Status::UNSUPPORTED, "codec-abi status -1"),
        (Status(-99), "codec-abi status -99"),
    ] {
        let failing = JxlDecoder::new().with_backend(AbiJxlDecoder::new(AbiDecodeFixture(status)));
        let result: gamut_core::Result<ImageBuf<Rgb8>> = failing.decode_image(&stream);
        let error = result.unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert_eq!(
            error.static_message(),
            Some("JXL: codec-abi decode backend failed")
        );
        assert_eq!(error.detail(), Some(detail));
    }
}

#[test]
fn a_codec_abi_decode_backend_round_trips_a_zeroed_raster() {
    // An OK status with an untouched (host-zeroed) buffer still round-trips the layout contract:
    // the host returns exactly what the backend was asked to fill.
    let stream = encode_fixture(&JxlEncoder::lossless());
    let decoder = JxlDecoder::new().with_backend(AbiJxlDecoder::new(AbiDecodeFixture(Status::OK)));
    let decoded: ImageBuf<Gray8> = decoder.decode_image(&stream).expect("decode");
    assert_eq!(decoded.dimensions(), Dimensions::new(16, 16).unwrap());
    assert_eq!(decoded.as_samples(), vec![0u8; 16 * 16].as_slice());
}

#[test]
fn the_public_surface_composes_for_alpha_layouts_too() {
    // A pushed backend serves every layout brand, not just the one it was written against.
    let (w, h) = (4u32, 4u32);
    let pixels = common::gen_u8(w, h, 4);
    let dims = Dimensions::new(w, h).unwrap();
    let image = ImageRef::<Rgba8>::new(&pixels, dims).expect("fixture");

    let mut baseline = Vec::new();
    JxlEncoder::lossless()
        .encode_image(image, &mut baseline)
        .expect("encode");

    struct Echo(Vec<u8>);
    impl JxlCodestreamEncoder for Echo {
        fn supports(&mut self, _req: &JxlEncodeRequest<'_>) -> bool {
            true
        }

        fn encode(
            &mut self,
            _req: &JxlEncodeRequest<'_>,
            image: &JxlImageRef<'_>,
        ) -> gamut_core::Result<Vec<u8>> {
            assert_eq!(image.channels(), 4);
            assert!(image.has_alpha());
            Ok(self.0.clone())
        }
    }

    let encoder = JxlEncoder::lossless().with_backend(Echo(baseline.clone()));
    let mut produced = Vec::new();
    encoder
        .encode_image(image, &mut produced)
        .expect("backend encode");
    assert_eq!(produced, baseline);

    let decoded: ImageBuf<Rgba8> = JxlDecoder::new().decode_image(&produced).expect("decode");
    assert_eq!(decoded.as_samples(), pixels.as_slice());
}
