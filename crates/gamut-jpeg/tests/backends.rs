//! The whole-interchange-stream backend seam (issue #277): registry fallback order, the
//! byte-identical default path, crate-owned APPn metadata in both directions, and the
//! `gamut-codec-abi` adapters.
//!
//! The backends here are deliberately trivial (they echo canned streams or canned rasters) — what is
//! under test is the *crate's* contract around them, not any real codec.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use gamut_codec_abi::{EncodeConfig, ImageDesc, Status, StreamConfig};
use gamut_core::{
    Cmyk8, DecodeImage, Dimensions, EncodeImage, Error, Gray8, ImageBuf, ImageRef, PixelFormat,
    Rgb8,
};
use gamut_jpeg::backend::{
    AbiStreamDecoder, AbiStreamEncoder, DecodedJpeg, JPEG_CODEC_ID, JpegEncodeRequest,
    JpegStreamDecoder, JpegStreamEncoder, JpegStreamInfo, RasterRef, backend_declined,
    is_backend_declined,
};
use gamut_jpeg::{ChromaSubsampling, JpegDecoder, JpegEncoder, JpegProcess};

// ---------------------------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------------------------

/// The dimensions every fixture uses: deliberately not a multiple of 8 or 16, so edge padding and
/// the 4:2:0 MCU grid are exercised.
const W: u32 = 24;
const H: u32 = 17;

/// The grayscale fixture the goldens were generated from.
fn gray_pixels() -> Vec<u8> {
    (0..W * H)
        .map(|i| ((i * 7 + (i / W) * 11) % 256) as u8)
        .collect()
}

/// The colour fixture the goldens were generated from.
fn rgb_pixels() -> Vec<u8> {
    (0..W * H * 3)
        .map(|i| ((i * 13 + (i / 71) * 29) % 256) as u8)
        .collect()
}

/// The encoder configuration the goldens were generated with.
fn golden_encoder(progressive: bool) -> JpegEncoder {
    JpegEncoder::new()
        .with_quality(78)
        .with_subsampling(ChromaSubsampling::Ycbcr420)
        .with_progressive(progressive)
}

/// Loads a byte-exact golden captured from the encoder *before* the backend seam existed.
fn golden(name: &str) -> Vec<u8> {
    let path = format!("{}/tests/goldens/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(&path).unwrap_or_else(|e| panic!("golden {path}: {e}"))
}

/// A small valid JPEG a backend can hand back as its "produced" stream.
fn backend_stream() -> Vec<u8> {
    JpegEncoder::new()
        .with_quality(40)
        .encode_to_vec(ImageRef::<Gray8>::new(&gray_pixels(), dims()).unwrap())
        .unwrap()
}

fn dims() -> Dimensions {
    Dimensions::new(W, H).unwrap()
}

// ---------------------------------------------------------------------------------------------
// Test backends
// ---------------------------------------------------------------------------------------------

/// Records the push-order call sequence shared by the backends of one test.
type Log = Arc<Mutex<Vec<&'static str>>>;

/// What a scripted backend does once it has been asked.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Act {
    /// Decline in `supports`.
    Decline,
    /// Accept, then succeed.
    Accept,
    /// Accept, then fail terminally.
    Fail,
    /// Accept, then return the late-decline sentinel.
    LateDecline,
}

/// A scripted decode backend: logs when it is consulted, then acts.
struct ScriptedDecoder {
    name: &'static str,
    act: Act,
    log: Log,
    /// The raster a successful decode returns.
    out: DecodedJpeg,
}

impl JpegStreamDecoder for ScriptedDecoder {
    fn supports(&mut self, _info: &JpegStreamInfo) -> bool {
        self.log.lock().unwrap().push(self.name);
        self.act != Act::Decline
    }

    fn decode(&mut self, _info: &JpegStreamInfo, _jpeg: &[u8]) -> gamut_core::Result<DecodedJpeg> {
        match self.act {
            Act::Accept => Ok(self.out.clone()),
            Act::LateDecline => Err(backend_declined()),
            _ => Err(Error::InvalidInput("scripted decode failure")),
        }
    }
}

/// A scripted encode backend: logs when it is consulted, then acts.
struct ScriptedEncoder {
    name: &'static str,
    act: Act,
    log: Log,
    /// The stream a successful encode returns.
    out: Vec<u8>,
}

impl JpegStreamEncoder for ScriptedEncoder {
    fn supports(&mut self, _req: &JpegEncodeRequest) -> bool {
        self.log.lock().unwrap().push(self.name);
        self.act != Act::Decline
    }

    fn encode(
        &mut self,
        _req: &JpegEncodeRequest,
        _image: &RasterRef<'_>,
    ) -> gamut_core::Result<Vec<u8>> {
        match self.act {
            Act::Accept => Ok(self.out.clone()),
            Act::LateDecline => Err(backend_declined()),
            _ => Err(Error::InvalidInput("scripted encode failure")),
        }
    }
}

/// A flat mid-grey raster of the fixture size, so an accepted decode is distinguishable from a real
/// one by inspection.
fn flat_gray() -> DecodedJpeg {
    DecodedJpeg::new(W, H, PixelFormat::Gray8, vec![99u8; (W * H) as usize]).unwrap()
}

// ---------------------------------------------------------------------------------------------
// Byte-identical default output
// ---------------------------------------------------------------------------------------------

#[test]
fn default_encoder_output_is_byte_identical_to_the_pre_seam_goldens() {
    let g = gray_pixels();
    let c = rgb_pixels();
    let gi = ImageRef::<Gray8>::new(&g, dims()).unwrap();
    let ci = ImageRef::<Rgb8>::new(&c, dims()).unwrap();
    for (name, progressive) in [("baseline", false), ("progressive", true)] {
        let enc = golden_encoder(progressive);
        assert_eq!(
            enc.encode_to_vec(gi).unwrap(),
            golden(&format!("gray_{name}.jpg")),
            "grayscale {name} output drifted"
        );
        assert_eq!(
            enc.encode_to_vec(ci).unwrap(),
            golden(&format!("color_{name}.jpg")),
            "colour {name} output drifted"
        );
    }
}

#[test]
fn default_metadata_encoder_output_is_byte_identical_to_the_pre_seam_golden() {
    let g = gray_pixels();
    let gi = ImageRef::<Gray8>::new(&g, dims()).unwrap();
    let bytes = JpegEncoder::new()
        .with_quality(78)
        .with_exif(&exif_fixture())
        .with_xmp(&xmp_fixture())
        .with_icc_profile(&icc_fixture())
        .encode_to_vec(gi)
        .unwrap();
    assert_eq!(bytes, golden("gray_meta.jpg"));
}

#[test]
fn an_empty_registry_leaves_both_directions_on_the_built_in_path() {
    let g = gray_pixels();
    let gi = ImageRef::<Gray8>::new(&g, dims()).unwrap();
    let plain = JpegEncoder::new().encode_to_vec(gi).unwrap();
    // A decoder/encoder that has had no backend pushed behaves exactly as before.
    let mut enc = JpegEncoder::new();
    assert!(format!("{enc:?}").contains("backends: 0"));
    let dec = JpegDecoder::new();
    assert_eq!(
        format!("{dec:?}"),
        "JpegDecoder { max_width: None, max_height: None, max_image_bytes: None, backends: 0 }"
    );
    let a: ImageBuf<Gray8> = dec.decode_image(&plain).unwrap();

    // Pushing a backend that always declines must not change either result.
    let log: Log = Arc::default();
    enc.push_backend(ScriptedEncoder {
        name: "e",
        act: Act::Decline,
        log: log.clone(),
        out: Vec::new(),
    });
    assert_eq!(enc.encode_to_vec(gi).unwrap(), plain);
    let mut dec2 = JpegDecoder::new();
    dec2.push_backend(ScriptedDecoder {
        name: "d",
        act: Act::Decline,
        log: log.clone(),
        out: flat_gray(),
    });
    let b: ImageBuf<Gray8> = dec2.decode_image(&plain).unwrap();
    assert_eq!(a.as_samples(), b.as_samples());
    assert_eq!(*log.lock().unwrap(), vec!["e", "d"]);
}

// ---------------------------------------------------------------------------------------------
// Registry order and fallback contract
// ---------------------------------------------------------------------------------------------

#[test]
fn decode_tries_backends_in_push_order_and_stops_at_the_first_acceptance() {
    let stream = backend_stream();
    let log: Log = Arc::default();
    let mut dec = JpegDecoder::new();
    for (name, act) in [
        ("first", Act::Decline),
        ("second", Act::Accept),
        ("third", Act::Accept),
    ] {
        dec.push_backend(ScriptedDecoder {
            name,
            act,
            log: log.clone(),
            out: flat_gray(),
        });
    }
    let img: ImageBuf<Gray8> = dec.decode_image(&stream).unwrap();
    assert_eq!(*log.lock().unwrap(), vec!["first", "second"]);
    assert_eq!(img.as_samples(), vec![99u8; (W * H) as usize]);
}

#[test]
fn decode_falls_through_to_the_built_in_tail_when_every_backend_declines() {
    let stream = backend_stream();
    let expected: ImageBuf<Gray8> = JpegDecoder::new().decode_image(&stream).unwrap();
    let log: Log = Arc::default();
    let mut dec = JpegDecoder::new();
    for name in ["a", "b", "c"] {
        dec.push_backend(ScriptedDecoder {
            name,
            act: Act::Decline,
            log: log.clone(),
            out: flat_gray(),
        });
    }
    let img: ImageBuf<Gray8> = dec.decode_image(&stream).unwrap();
    assert_eq!(*log.lock().unwrap(), vec!["a", "b", "c"]);
    assert_eq!(img.as_samples(), expected.as_samples());
}

#[test]
fn decode_accepted_then_failed_propagates_and_never_reaches_the_built_in_tail() {
    let stream = backend_stream();
    let builtin: ImageBuf<Gray8> = JpegDecoder::new().decode_image(&stream).unwrap();
    let log: Log = Arc::default();
    let mut dec = JpegDecoder::new();
    dec.push_backend(ScriptedDecoder {
        name: "failing",
        act: Act::Fail,
        log: log.clone(),
        out: flat_gray(),
    });
    dec.push_backend(ScriptedDecoder {
        name: "never",
        act: Act::Accept,
        log: log.clone(),
        out: flat_gray(),
    });
    let err = DecodeImage::<Gray8>::decode_image(&dec, &stream).unwrap_err();
    assert_eq!(err.static_message().unwrap(), "scripted decode failure");
    // Only the failing backend ran: neither a later backend nor the built-in tail was consulted.
    assert_eq!(*log.lock().unwrap(), vec!["failing"]);
    assert!(!builtin.as_samples().is_empty());
}

#[test]
fn decode_late_decline_resumes_the_fall_through() {
    let stream = backend_stream();
    let log: Log = Arc::default();
    let mut dec = JpegDecoder::new();
    dec.push_backend(ScriptedDecoder {
        name: "late",
        act: Act::LateDecline,
        log: log.clone(),
        out: flat_gray(),
    });
    dec.push_backend(ScriptedDecoder {
        name: "next",
        act: Act::Accept,
        log: log.clone(),
        out: flat_gray(),
    });
    let img: ImageBuf<Gray8> = dec.decode_image(&stream).unwrap();
    assert_eq!(*log.lock().unwrap(), vec!["late", "next"]);
    assert_eq!(img.as_samples(), vec![99u8; (W * H) as usize]);
}

#[test]
fn encode_tries_backends_in_push_order_and_stops_at_the_first_acceptance() {
    let g = gray_pixels();
    let gi = ImageRef::<Gray8>::new(&g, dims()).unwrap();
    let produced = backend_stream();
    let log: Log = Arc::default();
    let mut enc = JpegEncoder::new();
    for (name, act) in [
        ("first", Act::Decline),
        ("second", Act::Accept),
        ("third", Act::Accept),
    ] {
        enc.push_backend(ScriptedEncoder {
            name,
            act,
            log: log.clone(),
            out: produced.clone(),
        });
    }
    let out = enc.encode_to_vec(gi).unwrap();
    assert_eq!(*log.lock().unwrap(), vec!["first", "second"]);
    // No metadata is configured and the backend emitted none, so the stream survives verbatim.
    assert_eq!(out, produced);
}

#[test]
fn encode_falls_through_to_the_built_in_tail_when_every_backend_declines() {
    let g = gray_pixels();
    let gi = ImageRef::<Gray8>::new(&g, dims()).unwrap();
    let expected = JpegEncoder::new().encode_to_vec(gi).unwrap();
    let log: Log = Arc::default();
    let mut enc = JpegEncoder::new();
    for name in ["a", "b", "c"] {
        enc.push_backend(ScriptedEncoder {
            name,
            act: Act::Decline,
            log: log.clone(),
            out: Vec::new(),
        });
    }
    assert_eq!(enc.encode_to_vec(gi).unwrap(), expected);
    assert_eq!(*log.lock().unwrap(), vec!["a", "b", "c"]);
}

#[test]
fn encode_accepted_then_failed_propagates_and_never_reaches_the_built_in_tail() {
    let g = gray_pixels();
    let gi = ImageRef::<Gray8>::new(&g, dims()).unwrap();
    let log: Log = Arc::default();
    let mut enc = JpegEncoder::new();
    enc.push_backend(ScriptedEncoder {
        name: "failing",
        act: Act::Fail,
        log: log.clone(),
        out: Vec::new(),
    });
    enc.push_backend(ScriptedEncoder {
        name: "never",
        act: Act::Accept,
        log: log.clone(),
        out: backend_stream(),
    });
    let mut out = Vec::new();
    let err = enc.encode_image(gi, &mut out).unwrap_err();
    assert_eq!(err.static_message().unwrap(), "scripted encode failure");
    assert_eq!(*log.lock().unwrap(), vec!["failing"]);
    // The built-in tail never ran, so not one byte was written.
    assert!(out.is_empty());
}

#[test]
fn encode_late_decline_resumes_the_fall_through() {
    let g = gray_pixels();
    let gi = ImageRef::<Gray8>::new(&g, dims()).unwrap();
    let produced = backend_stream();
    let log: Log = Arc::default();
    let mut enc = JpegEncoder::new();
    enc.push_backend(ScriptedEncoder {
        name: "late",
        act: Act::LateDecline,
        log: log.clone(),
        out: Vec::new(),
    });
    enc.push_backend(ScriptedEncoder {
        name: "next",
        act: Act::Accept,
        log: log.clone(),
        out: produced.clone(),
    });
    assert_eq!(enc.encode_to_vec(gi).unwrap(), produced);
    assert_eq!(*log.lock().unwrap(), vec!["late", "next"]);
}

#[test]
fn cloning_shares_backends_rather_than_duplicating_them() {
    /// Counts how many encodes the *shared* backend instance saw.
    struct Counting(Arc<AtomicUsize>, Vec<u8>);
    impl JpegStreamEncoder for Counting {
        fn supports(&mut self, _req: &JpegEncodeRequest) -> bool {
            true
        }
        fn encode(
            &mut self,
            _req: &JpegEncodeRequest,
            _image: &RasterRef<'_>,
        ) -> gamut_core::Result<Vec<u8>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(self.1.clone())
        }
    }
    let calls = Arc::new(AtomicUsize::new(0));
    let mut enc = JpegEncoder::new();
    enc.push_backend(Counting(calls.clone(), backend_stream()));
    let clone = enc.clone();
    let g = gray_pixels();
    let gi = ImageRef::<Gray8>::new(&g, dims()).unwrap();
    enc.encode_to_vec(gi).unwrap();
    clone.encode_to_vec(gi).unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(format!("{clone:?}").contains("backends: 1"));
}

// ---------------------------------------------------------------------------------------------
// JpegStreamInfo: what the crate parses before consulting a backend
// ---------------------------------------------------------------------------------------------

/// A decode backend that captures the [`JpegStreamInfo`] it was offered, then declines.
struct Capturing(Arc<Mutex<Option<JpegStreamInfo>>>);

impl JpegStreamDecoder for Capturing {
    fn supports(&mut self, info: &JpegStreamInfo) -> bool {
        *self.0.lock().unwrap() = Some(info.clone());
        false
    }
    fn decode(&mut self, _info: &JpegStreamInfo, _jpeg: &[u8]) -> gamut_core::Result<DecodedJpeg> {
        unreachable!("declined in supports")
    }
}

/// Decodes `stream` through a capturing backend and returns the info the crate handed it.
fn captured(stream: &[u8]) -> JpegStreamInfo {
    let seen = Arc::new(Mutex::new(None));
    let mut dec = JpegDecoder::new();
    dec.push_backend(Capturing(seen.clone()));
    let _: ImageBuf<Rgb8> = dec.decode_image(stream).unwrap();
    let info = seen.lock().unwrap().clone();
    info.expect("backend was consulted")
}

#[test]
fn stream_info_is_populated_before_the_backend_is_consulted() {
    let c = rgb_pixels();
    let ci = ImageRef::<Rgb8>::new(&c, dims()).unwrap();

    let baseline_420 = JpegEncoder::new()
        .with_subsampling(ChromaSubsampling::Ycbcr420)
        .encode_to_vec(ci)
        .unwrap();
    let info = captured(&baseline_420);
    assert_eq!(info.width(), W);
    assert_eq!(info.height(), H);
    assert_eq!(info.components(), 3);
    assert_eq!(info.precision(), 8);
    assert_eq!(info.process(), JpegProcess::Baseline);
    assert_eq!(info.sampling_factors(), &[(2, 2), (1, 1), (1, 1)]);
    assert_eq!(info.subsampling(), Some(ChromaSubsampling::Ycbcr420));

    let prog_422 = JpegEncoder::new()
        .with_subsampling(ChromaSubsampling::Ycbcr422)
        .with_progressive(true)
        .encode_to_vec(ci)
        .unwrap();
    let info = captured(&prog_422);
    assert_eq!(info.process(), JpegProcess::Progressive);
    assert_eq!(info.sampling_factors(), &[(2, 1), (1, 1), (1, 1)]);
    assert_eq!(info.subsampling(), Some(ChromaSubsampling::Ycbcr422));

    let g = gray_pixels();
    let gray = JpegEncoder::new()
        .encode_to_vec(ImageRef::<Gray8>::new(&g, dims()).unwrap())
        .unwrap();
    let info = captured(&gray);
    assert_eq!(info.components(), 1);
    assert_eq!(info.sampling_factors(), &[(1, 1)]);
    assert_eq!(info.subsampling(), None);

    // `JpegStreamInfo::parse` is the same parse the crate runs internally.
    assert_eq!(JpegStreamInfo::parse(&gray).unwrap(), info);
    assert_eq!(
        JpegStreamInfo::parse(&baseline_420).unwrap(),
        captured(&baseline_420)
    );
}

#[test]
fn stream_info_parse_rejects_malformed_streams_before_any_backend_runs() {
    assert_eq!(
        JpegStreamInfo::parse(&[0xFF, 0xD9])
            .unwrap_err()
            .static_message()
            .unwrap(),
        "JPEG: missing SOI marker"
    );
    // A frame header declaring Nf=3 but whose segment length only covers one component entry.
    let short = [
        0xFF, 0xD8, 0xFF, 0xC0, 0x00, 0x0A, 8, 0, 8, 0, 8, 3, 1, 0x22,
    ];
    assert_eq!(
        JpegStreamInfo::parse(&short)
            .unwrap_err()
            .static_message()
            .unwrap(),
        "JPEG: truncated frame header"
    );
    // A backend must never be consulted for a stream the crate cannot parse.
    let seen = Arc::new(Mutex::new(None));
    let mut dec = JpegDecoder::new();
    dec.push_backend(Capturing(seen.clone()));
    assert!(DecodeImage::<Rgb8>::decode_image(&dec, &[0xFF, 0xD9]).is_err());
    assert!(seen.lock().unwrap().is_none());
}

/// What a [`CapturingEncoder`] recorded: the request, then the raster's width, height, format,
/// sample count, and first byte.
type SeenRequest = Arc<Mutex<Option<(JpegEncodeRequest, u32, u32, PixelFormat, usize, u8)>>>;

/// An encode backend that captures the request and raster it was offered, then declines late so the
/// built-in encoder still produces the output.
struct CapturingEncoder(SeenRequest);

impl JpegStreamEncoder for CapturingEncoder {
    fn supports(&mut self, _req: &JpegEncodeRequest) -> bool {
        true
    }
    fn encode(
        &mut self,
        req: &JpegEncodeRequest,
        image: &RasterRef<'_>,
    ) -> gamut_core::Result<Vec<u8>> {
        *self.0.lock().unwrap() = Some((
            *req,
            image.width(),
            image.height(),
            image.format(),
            image.samples().len(),
            image.samples()[0],
        ));
        Err(backend_declined())
    }
}

#[test]
fn the_encode_request_mirrors_every_encoder_setting_and_the_raster() {
    let g = gray_pixels();
    let c = rgb_pixels();
    let gi = ImageRef::<Gray8>::new(&g, dims()).unwrap();
    let ci = ImageRef::<Rgb8>::new(&c, dims()).unwrap();

    // Defaults: quality 75, 4:2:0, sequential, no restarts.
    let seen = Arc::new(Mutex::new(None));
    let mut enc = JpegEncoder::new();
    enc.push_backend(CapturingEncoder(seen.clone()));
    enc.encode_to_vec(gi).unwrap();
    let (req, w, h, fmt, len, first) = seen.lock().unwrap().unwrap();
    assert_eq!((req.width(), req.height()), (W, H));
    assert_eq!(req.format(), PixelFormat::Gray8);
    assert_eq!(req.quality(), 75);
    assert_eq!(req.subsampling(), ChromaSubsampling::Ycbcr420);
    assert!(!req.progressive(), "the default process is sequential");
    assert_eq!(req.restart_interval(), 0);
    assert_eq!((w, h, fmt), (W, H, PixelFormat::Gray8));
    assert_eq!(len, (W * H) as usize);
    assert_eq!(first, g[0]);

    // Every knob moved: the request must report exactly what was configured.
    let seen = Arc::new(Mutex::new(None));
    let mut enc = JpegEncoder::new()
        .with_quality(31)
        .with_subsampling(ChromaSubsampling::Ycbcr444)
        .with_progressive(true)
        .with_restart_interval(5);
    enc.push_backend(CapturingEncoder(seen.clone()));
    enc.encode_to_vec(ci).unwrap();
    let (req, w, h, fmt, len, first) = seen.lock().unwrap().unwrap();
    assert_eq!(req.quality(), 31);
    assert_eq!(req.subsampling(), ChromaSubsampling::Ycbcr444);
    assert!(req.progressive());
    assert_eq!(req.restart_interval(), 5);
    assert_eq!((w, h, fmt), (W, H, PixelFormat::Rgb8));
    assert_eq!(len, (W * H * 3) as usize);
    assert_eq!(first, c[0]);
}

// ---------------------------------------------------------------------------------------------
// Metadata ownership
// ---------------------------------------------------------------------------------------------

fn exif_fixture() -> Vec<u8> {
    b"II*\0\x08\0\0\0\0\0".to_vec()
}

fn xmp_fixture() -> Vec<u8> {
    b"<?xpacket begin=''?><x:xmpmeta/><?xpacket end='w'?>".to_vec()
}

/// An ICC profile large enough to need three APP2 chunks (the chunk payload is 65 519 bytes).
fn icc_fixture() -> Vec<u8> {
    (0..140_000u32).map(|i| (i % 251) as u8).collect()
}

/// A backend that returns a stream with **no** APPn metadata of its own.
struct BareStream(Vec<u8>);

impl JpegStreamEncoder for BareStream {
    fn supports(&mut self, _req: &JpegEncodeRequest) -> bool {
        true
    }
    fn encode(
        &mut self,
        _req: &JpegEncodeRequest,
        _image: &RasterRef<'_>,
    ) -> gamut_core::Result<Vec<u8>> {
        Ok(self.0.clone())
    }
}

#[test]
fn a_backend_stream_without_appn_still_gets_the_crates_metadata() {
    let bare = backend_stream();
    assert_eq!(
        gamut_jpeg::metadata(&bare).unwrap(),
        gamut_jpeg::JpegMetadata::default(),
        "the fixture backend stream must start out metadata-free"
    );
    let mut enc = JpegEncoder::new()
        .with_exif(&exif_fixture())
        .with_xmp(&xmp_fixture())
        .with_icc_profile(&icc_fixture());
    enc.push_backend(BareStream(bare.clone()));
    let g = gray_pixels();
    let out = enc
        .encode_to_vec(ImageRef::<Gray8>::new(&g, dims()).unwrap())
        .unwrap();

    let meta = gamut_jpeg::metadata(&out).unwrap();
    assert_eq!(meta.exif.as_deref(), Some(exif_fixture().as_slice()));
    assert_eq!(meta.xmp.as_deref(), Some(xmp_fixture().as_slice()));
    // Multi-segment ICC reassembles on the decode side.
    assert_eq!(meta.icc.as_deref(), Some(icc_fixture().as_slice()));
    assert_eq!(count_segments(&out, 0xE2), 3, "ICC must span three APP2s");
    // The coded image itself is untouched: pixels still match the backend's stream.
    let a: ImageBuf<Gray8> = JpegDecoder::new().decode_image(&out).unwrap();
    let b: ImageBuf<Gray8> = JpegDecoder::new().decode_image(&bare).unwrap();
    assert_eq!(a.as_samples(), b.as_samples());
}

#[test]
fn patched_metadata_lands_exactly_where_the_built_in_prologue_puts_it() {
    // The backend hands back precisely what the built-in encoder produces *without* metadata, so a
    // correctly positioned patch must reproduce the built-in encoder's metadata-bearing stream byte
    // for byte — pinning that EXIF/XMP/ICC go after the JFIF APP0 and before the DQT.
    let g = gray_pixels();
    let gi = ImageRef::<Gray8>::new(&g, dims()).unwrap();
    let bare = JpegEncoder::new()
        .with_quality(40)
        .encode_to_vec(gi)
        .unwrap();
    let expected = JpegEncoder::new()
        .with_quality(40)
        .with_exif(&exif_fixture())
        .with_xmp(&xmp_fixture())
        .with_icc_profile(&icc_fixture())
        .encode_to_vec(gi)
        .unwrap();

    let mut enc = JpegEncoder::new()
        .with_quality(40)
        .with_exif(&exif_fixture())
        .with_xmp(&xmp_fixture())
        .with_icc_profile(&icc_fixture());
    enc.push_backend(BareStream(bare));
    assert_eq!(enc.encode_to_vec(gi).unwrap(), expected);
}

#[test]
fn a_backend_stream_with_its_own_appn_is_patched_not_double_written() {
    // The backend emits a stream that already carries *different* EXIF/XMP/ICC.
    let g = gray_pixels();
    let gi = ImageRef::<Gray8>::new(&g, dims()).unwrap();
    let theirs = JpegEncoder::new()
        .with_exif(b"MM\0*\0\0\0\x08\0\0")
        .with_xmp(b"<?xpacket begin=''?>THEIRS<?xpacket end='w'?>")
        .with_icc_profile(&vec![7u8; 80_000])
        .encode_to_vec(gi)
        .unwrap();
    assert_eq!(count_segments(&theirs, 0xE1), 2);
    assert_eq!(count_segments(&theirs, 0xE2), 2);

    let mut enc = JpegEncoder::new()
        .with_exif(&exif_fixture())
        .with_xmp(&xmp_fixture())
        .with_icc_profile(&icc_fixture());
    enc.push_backend(BareStream(theirs));
    let out = enc.encode_to_vec(gi).unwrap();

    // Exactly one EXIF APP1 + one XMP APP1, and exactly the three chunks of *our* profile.
    assert_eq!(count_segments(&out, 0xE1), 2);
    assert_eq!(count_segments(&out, 0xE2), 3);
    let meta = gamut_jpeg::metadata(&out).unwrap();
    assert_eq!(meta.exif.as_deref(), Some(exif_fixture().as_slice()));
    assert_eq!(meta.xmp.as_deref(), Some(xmp_fixture().as_slice()));
    assert_eq!(meta.icc.as_deref(), Some(icc_fixture().as_slice()));
}

#[test]
fn a_backend_stream_loses_appn_metadata_the_encoder_did_not_configure() {
    let g = gray_pixels();
    let gi = ImageRef::<Gray8>::new(&g, dims()).unwrap();
    let theirs = JpegEncoder::new()
        .with_exif(&exif_fixture())
        .with_icc_profile(&vec![7u8; 4_000])
        .encode_to_vec(gi)
        .unwrap();
    let mut enc = JpegEncoder::new();
    enc.push_backend(BareStream(theirs));
    let out = enc.encode_to_vec(gi).unwrap();
    // The crate owns metadata: with none configured, the produced stream carries none.
    assert_eq!(
        gamut_jpeg::metadata(&out).unwrap(),
        gamut_jpeg::JpegMetadata::default()
    );
    assert_eq!(count_segments(&out, 0xE1), 0);
    assert_eq!(count_segments(&out, 0xE2), 0);
    // Non-crate-owned APPn survives: the JFIF APP0 is still there, exactly once.
    assert_eq!(count_segments(&out, 0xE0), 1);
}

#[test]
fn metadata_caps_are_validated_before_any_backend_runs() {
    let g = gray_pixels();
    let gi = ImageRef::<Gray8>::new(&g, dims()).unwrap();
    let log: Log = Arc::default();
    let mut enc = JpegEncoder::new().with_xmp(&vec![b'x'; 65_503]);
    enc.push_backend(ScriptedEncoder {
        name: "never",
        act: Act::Accept,
        log: log.clone(),
        out: backend_stream(),
    });
    let err = enc.encode_to_vec(gi).unwrap_err();
    assert_eq!(
        err.static_message().unwrap(),
        "JPEG: XMP exceeds one APP1 segment (ExtendedXMP not supported)"
    );
    assert!(log.lock().unwrap().is_empty());
}

#[test]
fn a_backend_stream_that_is_not_a_jpeg_is_rejected() {
    let g = gray_pixels();
    let gi = ImageRef::<Gray8>::new(&g, dims()).unwrap();
    for (bytes, msg) in [
        (vec![0u8; 8], "JPEG: missing SOI marker"),
        (
            vec![0xFF, 0xD8, 0xFF, 0xD8],
            "JPEG: backend stream does not end with EOI",
        ),
        (
            vec![0xFF, 0xD8, 0xFF, 0xD0, 0xFF, 0xD9],
            "JPEG: backend stream has a standalone marker before the first scan",
        ),
        (
            vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x40, 0xFF, 0xD9],
            "JPEG: truncated segment",
        ),
        // Framed correctly, but carrying no frame header at all.
        (
            vec![0xFF, 0xD8, 0xFF, 0xD9],
            "JPEG: no frame header before scan/end",
        ),
        // A frame of the wrong size: the crate validates what the backend produced.
        (
            wrong_size_stream(),
            "JPEG: backend stream declares different dimensions than the encoded image",
        ),
    ] {
        let mut enc = JpegEncoder::new();
        enc.push_backend(BareStream(bytes));
        assert_eq!(
            enc.encode_to_vec(gi).unwrap_err().static_message().unwrap(),
            msg
        );
    }
}

/// A well-formed stream whose frame is a different size than the fixture the encoder was given.
fn wrong_size_stream() -> Vec<u8> {
    let px = vec![0u8; 64];
    JpegEncoder::new()
        .encode_to_vec(ImageRef::<Gray8>::new(&px, Dimensions::new(8, 8).unwrap()).unwrap())
        .unwrap()
}

#[test]
fn decode_metadata_is_never_delegated_to_a_backend() {
    let g = gray_pixels();
    let stream = JpegEncoder::new()
        .with_exif(&exif_fixture())
        .with_icc_profile(&icc_fixture())
        .encode_to_vec(ImageRef::<Gray8>::new(&g, dims()).unwrap())
        .unwrap();
    // A backend that hands back an unrelated raster cannot influence what `metadata` reports.
    let log: Log = Arc::default();
    let mut dec = JpegDecoder::new();
    dec.push_backend(ScriptedDecoder {
        name: "x",
        act: Act::Accept,
        log: log.clone(),
        out: flat_gray(),
    });
    let img: ImageBuf<Gray8> = dec.decode_image(&stream).unwrap();
    assert_eq!(img.as_samples(), vec![99u8; (W * H) as usize]);
    let meta = gamut_jpeg::metadata(&stream).unwrap();
    assert_eq!(meta.exif.as_deref(), Some(exif_fixture().as_slice()));
    assert_eq!(meta.icc.as_deref(), Some(icc_fixture().as_slice()));
}

/// Counts the marker segments with code `marker` in the header region of `stream` (up to the first
/// SOS), by walking the segment lengths.
fn count_segments(stream: &[u8], marker: u8) -> usize {
    let mut pos = 2;
    let mut n = 0;
    while pos + 4 <= stream.len() {
        assert_eq!(stream[pos], 0xFF, "expected a marker at {pos}");
        let code = stream[pos + 1];
        if code == 0xDA || code == 0xD9 {
            break;
        }
        if code == marker {
            n += 1;
        }
        let len = usize::from(u16::from_be_bytes([stream[pos + 2], stream[pos + 3]]));
        pos += 2 + len;
    }
    n
}

// ---------------------------------------------------------------------------------------------
// Presentation of backend rasters
// ---------------------------------------------------------------------------------------------

/// A decode backend that returns a fixed raster for every stream.
struct Fixed(DecodedJpeg);

impl JpegStreamDecoder for Fixed {
    fn supports(&mut self, _info: &JpegStreamInfo) -> bool {
        true
    }
    fn decode(&mut self, _info: &JpegStreamInfo, _jpeg: &[u8]) -> gamut_core::Result<DecodedJpeg> {
        Ok(self.0.clone())
    }
}

fn decoder_returning(raster: DecodedJpeg) -> JpegDecoder {
    let mut dec = JpegDecoder::new();
    dec.push_backend(Fixed(raster));
    dec
}

#[test]
fn decoder_limits_run_before_backend_selection_and_after_backend_output() {
    let stream = backend_stream();
    let log: Log = Arc::default();
    let mut preflight = JpegDecoder::new().with_max_dimensions(W - 1, H);
    preflight.push_backend(ScriptedDecoder {
        name: "must-not-run",
        act: Act::Accept,
        log: log.clone(),
        out: flat_gray(),
    });
    assert_eq!(
        DecodeImage::<Gray8>::decode_image(&preflight, &stream)
            .unwrap_err()
            .static_message()
            .unwrap(),
        "JPEG: image exceeds the dimension limit"
    );
    assert!(log.lock().unwrap().is_empty());

    // The input header itself is exactly within both caps, but a backend may return a different
    // shape or layout. Re-check the owned result before presentation or destination replacement.
    let too_wide = decoder_returning(
        DecodedJpeg::new(
            W + 1,
            H,
            PixelFormat::Gray8,
            vec![0; ((W + 1) * H) as usize],
        )
        .unwrap(),
    )
    .with_max_dimensions(W, H);
    assert_eq!(
        DecodeImage::<Gray8>::decode_image(&too_wide, &stream)
            .unwrap_err()
            .static_message()
            .unwrap(),
        "JPEG: image exceeds the dimension limit"
    );

    let too_many_bytes = decoder_returning(
        DecodedJpeg::new(W, H, PixelFormat::Rgb8, vec![0; (W * H * 3) as usize]).unwrap(),
    )
    .with_max_image_bytes((W * H) as usize);
    assert_eq!(
        DecodeImage::<Rgb8>::decode_image(&too_many_bytes, &stream)
            .unwrap_err()
            .static_message()
            .unwrap(),
        "JPEG: image exceeds the size limit"
    );
}

#[test]
fn backend_limit_error_leaves_decode_into_destination_unchanged() {
    let stream = backend_stream();
    let decoder = decoder_returning(
        DecodedJpeg::new(
            W + 1,
            H,
            PixelFormat::Gray8,
            vec![0; ((W + 1) * H) as usize],
        )
        .unwrap(),
    )
    .with_max_dimensions(W, H);
    let mut dst = ImageBuf::<Gray8>::new(vec![77; (W * H) as usize], dims()).unwrap();
    let ptr = dst.as_samples().as_ptr();

    assert!(decoder.decode_image_into(&stream, &mut dst).is_err());
    assert_eq!(dst.as_samples().as_ptr(), ptr);
    assert!(dst.as_samples().iter().all(|&sample| sample == 77));
}

#[test]
fn backend_rasters_are_presented_by_the_same_rules_as_the_built_in_decoder() {
    let stream = backend_stream();
    let gray = DecodedJpeg::new(2, 1, PixelFormat::Gray8, vec![10, 20]).unwrap();
    let rgb = DecodedJpeg::new(2, 1, PixelFormat::Rgb8, vec![1, 2, 3, 4, 5, 6]).unwrap();
    let cmyk = DecodedJpeg::new(2, 1, PixelFormat::Cmyk8, vec![1, 2, 3, 4, 5, 6, 7, 8]).unwrap();

    // The refusal messages now come from the shared conversion engine rather than from JPEG: the
    // presentation rules are gamut-core's, so the wording is the same for every format crate.
    // Gray → Rgb8 replicates; Gray → Gray8 passes through; Gray → Cmyk8 is rejected.
    let d = decoder_returning(gray.clone());
    let r: ImageBuf<Rgb8> = d.decode_image(&stream).unwrap();
    assert_eq!(r.as_samples(), &[10, 10, 10, 20, 20, 20]);
    let g: ImageBuf<Gray8> = d.decode_image(&stream).unwrap();
    assert_eq!(g.as_samples(), &[10, 20]);
    assert_eq!(
        DecodeImage::<Cmyk8>::decode_image(&d, &stream)
            .unwrap_err()
            .static_message()
            .unwrap(),
        "convert: CMYK conversion needs a colour-management transform, not a layout change"
    );

    // Rgb passes through to Rgb8 and is rejected for the other two.
    let d = decoder_returning(rgb);
    let r: ImageBuf<Rgb8> = d.decode_image(&stream).unwrap();
    assert_eq!(r.as_samples(), &[1, 2, 3, 4, 5, 6]);
    assert_eq!(
        DecodeImage::<Gray8>::decode_image(&d, &stream)
            .unwrap_err()
            .static_message()
            .unwrap(),
        "convert: target layout cannot hold colour; set a LumaPolicy"
    );

    // Cmyk passes through to Cmyk8 and is rejected for Rgb8.
    let d = decoder_returning(cmyk);
    let c: ImageBuf<Cmyk8> = d.decode_image(&stream).unwrap();
    assert_eq!(c.as_samples(), &[1, 2, 3, 4, 5, 6, 7, 8]);
    assert_eq!(
        DecodeImage::<Rgb8>::decode_image(&d, &stream)
            .unwrap_err()
            .static_message()
            .unwrap(),
        "convert: CMYK conversion needs a colour-management transform, not a layout change"
    );
}

#[test]
fn decode_image_into_reuses_a_matching_buffer_and_replaces_a_mismatched_one() {
    let stream = backend_stream();
    let d = decoder_returning(DecodedJpeg::new(2, 1, PixelFormat::Gray8, vec![10, 20]).unwrap());

    let mut dst: ImageBuf<Gray8> =
        ImageBuf::new(vec![0, 0], Dimensions::new(2, 1).unwrap()).unwrap();
    let ptr = dst.as_samples().as_ptr();
    d.decode_image_into(&stream, &mut dst).unwrap();
    assert_eq!(dst.as_samples(), &[10, 20]);
    assert_eq!(dst.as_samples().as_ptr(), ptr, "allocation must be reused");

    let mut other: ImageBuf<Gray8> =
        ImageBuf::new(vec![0; 9], Dimensions::new(3, 3).unwrap()).unwrap();
    d.decode_image_into(&stream, &mut other).unwrap();
    assert_eq!(other.dimensions(), Dimensions::new(2, 1).unwrap());
    assert_eq!(other.as_samples(), &[10, 20]);

    // The same, for the RGB and CMYK presentations.
    let d = decoder_returning(
        DecodedJpeg::new(2, 1, PixelFormat::Rgb8, vec![1, 2, 3, 4, 5, 6]).unwrap(),
    );
    let mut rgb: ImageBuf<Rgb8> =
        ImageBuf::new(vec![0; 6], Dimensions::new(2, 1).unwrap()).unwrap();
    d.decode_image_into(&stream, &mut rgb).unwrap();
    assert_eq!(rgb.as_samples(), &[1, 2, 3, 4, 5, 6]);
    let mut rgb2: ImageBuf<Rgb8> =
        ImageBuf::new(vec![0; 3], Dimensions::new(1, 1).unwrap()).unwrap();
    d.decode_image_into(&stream, &mut rgb2).unwrap();
    assert_eq!(rgb2.dimensions(), Dimensions::new(2, 1).unwrap());

    let d = decoder_returning(
        DecodedJpeg::new(2, 1, PixelFormat::Cmyk8, vec![1, 2, 3, 4, 5, 6, 7, 8]).unwrap(),
    );
    let mut cmyk: ImageBuf<Cmyk8> =
        ImageBuf::new(vec![0; 8], Dimensions::new(2, 1).unwrap()).unwrap();
    d.decode_image_into(&stream, &mut cmyk).unwrap();
    assert_eq!(cmyk.as_samples(), &[1, 2, 3, 4, 5, 6, 7, 8]);
    let mut cmyk2: ImageBuf<Cmyk8> =
        ImageBuf::new(vec![0; 4], Dimensions::new(1, 1).unwrap()).unwrap();
    d.decode_image_into(&stream, &mut cmyk2).unwrap();
    assert_eq!(cmyk2.dimensions(), Dimensions::new(2, 1).unwrap());
}

// ---------------------------------------------------------------------------------------------
// codec-abi adapters
// ---------------------------------------------------------------------------------------------

/// What the decode adapter's `StreamConfig` carried: codec id, width, height, bit depth, and
/// extradata length.
type SeenStream = Arc<Mutex<Option<(u32, u32, u32, u32, usize)>>>;

/// What the encode adapter carried: quality, pixel format, width, height, plane count, stride, and
/// the first raster byte.
type SeenEncode = Arc<Mutex<Option<(u32, u32, u32, u32, u32, usize, u8)>>>;

/// A `gamut-codec-abi` decoder twin scripted with the status it returns.
struct AbiDec {
    supports: bool,
    status: Status,
    /// The byte written into every output sample on success.
    fill: u8,
    seen: SeenStream,
}

impl gamut_codec_abi::Decoder for AbiDec {
    fn supports(&mut self, cfg: &StreamConfig) -> bool {
        *self.seen.lock().unwrap() = Some((
            cfg.codec_id,
            cfg.width,
            cfg.height,
            cfg.bit_depth,
            cfg.extradata_len,
        ));
        self.supports
    }

    fn decode(&mut self, cfg: &StreamConfig, codestream: &[u8], out: &ImageDesc) -> Status {
        if !self.status.is_ok() {
            return self.status;
        }
        // The seam hands over the *whole* interchange stream.
        assert_eq!(&codestream[..2], &[0xFF, 0xD8]);
        assert_eq!(&codestream[codestream.len() - 2..], &[0xFF, 0xD9]);
        assert_eq!(cfg.codec_id, JPEG_CODEC_ID);
        let len = out.strides[0] * out.height as usize;
        // SAFETY-equivalent: the host allocated `len` bytes behind planes[0] for this call.
        unsafe { std::ptr::write_bytes(out.planes[0], self.fill, len) };
        Status::OK
    }
}

#[test]
fn abi_decode_adapter_round_trips_bytes_and_carries_the_frame_shape() {
    let stream = backend_stream();
    let seen = Arc::new(Mutex::new(None));
    let mut dec = JpegDecoder::new();
    dec.push_backend(AbiStreamDecoder::new(AbiDec {
        supports: true,
        status: Status::OK,
        fill: 42,
        seen: seen.clone(),
    }));
    let img: ImageBuf<Gray8> = dec.decode_image(&stream).unwrap();
    assert_eq!(img.dimensions(), dims());
    assert_eq!(img.as_samples(), vec![42u8; (W * H) as usize]);
    assert_eq!(
        *seen.lock().unwrap(),
        Some((JPEG_CODEC_ID, W, H, 8, 0)),
        "the adapter must fill StreamConfig from JpegStreamInfo"
    );
}

#[test]
fn abi_decode_adapter_declines_on_unsupported_and_propagates_other_statuses() {
    let stream = backend_stream();
    let seen = Arc::new(Mutex::new(None));

    // `supports() == false` → normal fall-through to the built-in tail.
    let mut dec = JpegDecoder::new();
    dec.push_backend(AbiStreamDecoder::new(AbiDec {
        supports: false,
        status: Status::OK,
        fill: 1,
        seen: seen.clone(),
    }));
    let via_backend: ImageBuf<Gray8> = dec.decode_image(&stream).unwrap();
    let builtin: ImageBuf<Gray8> = JpegDecoder::new().decode_image(&stream).unwrap();
    assert_eq!(via_backend.as_samples(), builtin.as_samples());

    // A *late* UNSUPPORTED is a decline: the built-in tail still runs.
    let mut dec = JpegDecoder::new();
    dec.push_backend(AbiStreamDecoder::new(AbiDec {
        supports: true,
        status: Status::UNSUPPORTED,
        fill: 1,
        seen: seen.clone(),
    }));
    let late: ImageBuf<Gray8> = dec.decode_image(&stream).unwrap();
    assert_eq!(late.as_samples(), builtin.as_samples());

    // Any other non-OK status is terminal.
    let mut dec = JpegDecoder::new();
    dec.push_backend(AbiStreamDecoder::new(AbiDec {
        supports: true,
        status: Status(-7),
        fill: 1,
        seen,
    }));
    assert_eq!(
        DecodeImage::<Gray8>::decode_image(&dec, &stream)
            .unwrap_err()
            .static_message()
            .unwrap(),
        "JPEG: codec-abi decode backend returned a failure status"
    );
}

/// A `gamut-codec-abi` encoder twin scripted with the status it returns.
struct AbiEnc {
    supports: bool,
    status: Status,
    /// The stream emitted, in two chunks, on success.
    out: Vec<u8>,
    seen: SeenEncode,
}

impl gamut_codec_abi::Encoder for AbiEnc {
    fn supports(&mut self, cfg: &EncodeConfig) -> bool {
        assert_eq!(cfg.codec_id, JPEG_CODEC_ID);
        self.supports
    }

    fn encode(
        &mut self,
        cfg: &EncodeConfig,
        image: &ImageDesc,
        sink: &mut dyn FnMut(&[u8]) -> Status,
    ) -> Status {
        // SAFETY-equivalent: the host's raster is valid for the duration of this call.
        let first = unsafe { *image.planes[0] };
        *self.seen.lock().unwrap() = Some((
            cfg.quality,
            image.pixel_format,
            image.width,
            image.height,
            image.plane_count,
            image.strides[0],
            first,
        ));
        if !self.status.is_ok() {
            return self.status;
        }
        // Two chunks, to prove the adapter concatenates the sink stream.
        let mid = self.out.len() / 2;
        let s = sink(&self.out[..mid]);
        if !s.is_ok() {
            return s;
        }
        sink(&self.out[mid..])
    }
}

#[test]
fn abi_encode_adapter_concatenates_chunks_and_describes_the_raster() {
    let g = gray_pixels();
    let gi = ImageRef::<Gray8>::new(&g, dims()).unwrap();
    let produced = backend_stream();
    let seen = Arc::new(Mutex::new(None));
    let mut enc = JpegEncoder::new().with_quality(63);
    enc.push_backend(AbiStreamEncoder::new(AbiEnc {
        supports: true,
        status: Status::OK,
        out: produced.clone(),
        seen: seen.clone(),
    }));
    assert_eq!(enc.encode_to_vec(gi).unwrap(), produced);
    assert_eq!(
        *seen.lock().unwrap(),
        Some((63, PixelFormat::Gray8 as u32, W, H, 1, W as usize, g[0]))
    );
}

#[test]
fn abi_encode_adapter_declines_on_unsupported_and_propagates_other_statuses() {
    let g = gray_pixels();
    let gi = ImageRef::<Gray8>::new(&g, dims()).unwrap();
    let builtin = JpegEncoder::new().encode_to_vec(gi).unwrap();
    let seen = Arc::new(Mutex::new(None));

    // `supports() == false` declines *before* the backend runs: it would have succeeded and its
    // stream differs from the built-in's, so the built-in output proves the adapter asked at all.
    let produced = backend_stream();
    assert_ne!(produced, builtin);
    let mut enc = JpegEncoder::new();
    enc.push_backend(AbiStreamEncoder::new(AbiEnc {
        supports: false,
        status: Status::OK,
        out: produced,
        seen: seen.clone(),
    }));
    assert_eq!(enc.encode_to_vec(gi).unwrap(), builtin);
    assert!(
        seen.lock().unwrap().is_none(),
        "a declined backend must never be asked to encode"
    );

    // A *late* UNSUPPORTED (after `supports` accepted) also reaches the built-in tail.
    let mut enc = JpegEncoder::new();
    enc.push_backend(AbiStreamEncoder::new(AbiEnc {
        supports: true,
        status: Status::UNSUPPORTED,
        out: Vec::new(),
        seen: seen.clone(),
    }));
    assert_eq!(enc.encode_to_vec(gi).unwrap(), builtin);
    assert!(seen.lock().unwrap().is_some());

    let mut enc = JpegEncoder::new();
    enc.push_backend(AbiStreamEncoder::new(AbiEnc {
        supports: true,
        status: Status(3),
        out: Vec::new(),
        seen,
    }));
    assert_eq!(
        enc.encode_to_vec(gi).unwrap_err().static_message().unwrap(),
        "JPEG: codec-abi encode backend returned a failure status"
    );
}

#[test]
fn abi_decode_adapter_rejects_component_counts_it_cannot_lay_out() {
    // A two-component frame: valid enough to parse, but not presentable.
    let mut stream = vec![0xFF, 0xD8, 0xFF, 0xC0, 0x00, 0x0E, 8, 0, 8, 0, 8, 2];
    stream.extend_from_slice(&[1, 0x11, 0, 2, 0x11, 0]);
    stream.extend_from_slice(&[0xFF, 0xD9]);
    let info = JpegStreamInfo::parse(&stream).unwrap();
    assert_eq!(info.components(), 2);

    let mut dec = JpegDecoder::new();
    dec.push_backend(AbiStreamDecoder::new(AbiDec {
        supports: true,
        status: Status::OK,
        fill: 0,
        seen: Arc::new(Mutex::new(None)),
    }));
    assert_eq!(
        DecodeImage::<Rgb8>::decode_image(&dec, &stream)
            .unwrap_err()
            .static_message()
            .unwrap(),
        "JPEG: only 1, 3, or 4 component streams are supported"
    );
}

#[test]
fn the_declined_sentinel_is_public_and_recognised() {
    let err = backend_declined();
    assert!(is_backend_declined(&err));
    assert!(!is_backend_declined(&Error::InvalidInput("nope")));
    assert!(!is_backend_declined(&Error::Unsupported("nope")));
}

#[test]
fn custom_quant_tables_pin_the_encode_to_the_built_in_path() {
    // A `JpegEncodeRequest` cannot carry caller-supplied tables, so an encoder configured with
    // `with_quant_tables` must not consult its registry at all — an always-accepting backend
    // would otherwise encode with the wrong tables. The output must equal a registry-free encode
    // of the same configuration.
    use gamut_jpeg::QuantTables;
    let tables = QuantTables::new([9u8; 64], [13u8; 64]).unwrap();
    let log: Log = Arc::default();
    let mut enc = JpegEncoder::new().with_quant_tables(tables);
    enc.push_backend(ScriptedEncoder {
        name: "eager",
        act: Act::Accept,
        log: Arc::clone(&log),
        out: backend_stream(),
    });
    let pixels = gray_pixels();
    let img = ImageRef::<Gray8>::new(&pixels, dims()).unwrap();
    let with_registry = enc.encode_to_vec(img).unwrap();
    assert!(
        log.lock().unwrap().is_empty(),
        "no backend may be consulted while custom tables are set"
    );
    let img = ImageRef::<Gray8>::new(&pixels, dims()).unwrap();
    let built_in = JpegEncoder::new()
        .with_quant_tables(tables)
        .encode_to_vec(img)
        .unwrap();
    assert_eq!(with_registry, built_in);
}

#[test]
fn rd_optimization_pins_the_encode_to_the_built_in_path() {
    // The RD configuration cannot ride a `JpegEncodeRequest`, so a non-`None` mode must skip the
    // registry entirely, exactly like custom quantization tables.
    use gamut_jpeg::RdOptimization;
    let log: Log = Arc::default();
    let mut enc = JpegEncoder::new().with_rd_optimization(RdOptimization::Trellis);
    enc.push_backend(ScriptedEncoder {
        name: "eager",
        act: Act::Accept,
        log: Arc::clone(&log),
        out: backend_stream(),
    });
    let pixels = gray_pixels();
    let img = ImageRef::<Gray8>::new(&pixels, dims()).unwrap();
    let with_registry = enc.encode_to_vec(img).unwrap();
    assert!(
        log.lock().unwrap().is_empty(),
        "no backend may be consulted while RD optimization is on"
    );
    let img = ImageRef::<Gray8>::new(&pixels, dims()).unwrap();
    let built_in = JpegEncoder::new()
        .with_rd_optimization(RdOptimization::Trellis)
        .encode_to_vec(img)
        .unwrap();
    assert_eq!(with_registry, built_in);
}

#[test]
fn progressive_script_pins_the_encode_to_the_built_in_path() {
    // A `JpegEncodeRequest` carries `progressive` but not the scan script, so a non-default script
    // must skip the registry rather than let a backend encode with its own.
    use gamut_jpeg::ProgressiveScript;
    let log: Log = Arc::default();
    let configured = || {
        JpegEncoder::new()
            .with_progressive(true)
            .with_progressive_script(ProgressiveScript::Mozjpeg)
    };
    let mut enc = configured();
    enc.push_backend(ScriptedEncoder {
        name: "eager",
        act: Act::Accept,
        log: Arc::clone(&log),
        out: backend_stream(),
    });
    let pixels = gray_pixels();
    let img = ImageRef::<Gray8>::new(&pixels, dims()).unwrap();
    let with_registry = enc.encode_to_vec(img).unwrap();
    assert!(
        log.lock().unwrap().is_empty(),
        "no backend may be consulted while a non-default scan script is set"
    );
    let img = ImageRef::<Gray8>::new(&pixels, dims()).unwrap();
    assert_eq!(with_registry, configured().encode_to_vec(img).unwrap());
}
