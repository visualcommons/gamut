//! Backend-selection tests (issue #273): the ordered [`HevcDecoders`] registry and the
//! [`AbiHevcDecoder`] adapter over the `gamut-codec-abi` decoder seam.
//!
//! These live as an integration test (not an in-crate `#[cfg(test)]` module) because exercising the
//! ABI adapter means *being* a C-shaped backend — writing samples through the `ImageDesc` plane
//! pointers — which the library's `#![forbid(unsafe_code)]` rightly disallows.

use std::sync::{Arc, Mutex};

use gamut_codec_abi::{Decoder, ImageDesc, Status, StreamConfig};
use gamut_core::{Dimensions, Error, ErrorKind};
use gamut_heic::{
    AbiHevcDecoder, BACKEND_DECLINED, ChromaFormat, DecodedFrame, HEVC_CODEC_ID, HevcConfig,
    HevcDecoder, HevcDecoders, NO_BACKEND, planar_pixel_format,
};

// ============================================================================================
//   Fixtures
// ============================================================================================

/// A minimal valid `hvcC`: the 23-byte header with `lengthSizeMinusOne = 3` and no arrays.
fn hvcc_bytes() -> Vec<u8> {
    let mut v = vec![0u8; 23];
    v[0] = 1; // configurationVersion
    v[21] = 0b0000_0011; // ... | lengthSizeMinusOne = 3
    v[22] = 0; // numOfArrays
    v
}

/// A parsed minimal config: monochrome, 8-bit.
fn config() -> HevcConfig {
    HevcConfig::parse(&hvcc_bytes()).expect("minimal hvcC parses")
}

/// The same config with one VPS parameter-set array, so `annex_b` extradata is non-empty.
fn config_with_vps() -> HevcConfig {
    let mut v = hvcc_bytes();
    v[22] = 1; // numOfArrays
    v.push(0x80 | 32); // array_completeness | NAL_unit_type = 32 (VPS)
    v.extend_from_slice(&1u16.to_be_bytes()); // numNalus
    v.extend_from_slice(&3u16.to_be_bytes()); // nalUnitLength
    v.extend_from_slice(&[0x40, 0x01, 0x0C]); // the VPS NAL
    HevcConfig::parse(&v).expect("hvcC with a VPS array parses")
}

/// A 2x2 monochrome frame with a recognisable sample value.
fn frame(value: u16) -> DecodedFrame {
    DecodedFrame::new(
        2,
        2,
        8,
        ChromaFormat::Monochrome,
        vec![value; 4],
        vec![],
        vec![],
    )
    .expect("2x2 mono frame is consistent")
}

/// A shared call log, so a test can assert exactly which backends were consulted and in what order.
type Log = Arc<Mutex<Vec<String>>>;

fn log() -> Log {
    Arc::new(Mutex::new(Vec::new()))
}

fn entries(log: &Log) -> Vec<String> {
    log.lock().expect("log lock").clone()
}

/// What a [`Probe`] backend does once it has accepted a job.
#[derive(Clone, Copy)]
enum Outcome {
    /// Succeed, returning a mono frame whose samples are this value.
    Frame(u16),
    /// Fail terminally — must propagate out of the registry.
    Fail,
    /// Decline late via the [`BACKEND_DECLINED`] sentinel — must fall through.
    LateDecline,
}

/// A scriptable [`HevcDecoder`] that records every call it receives.
struct Probe {
    name: &'static str,
    supports: bool,
    outcome: Outcome,
    log: Log,
}

impl Probe {
    fn new(name: &'static str, supports: bool, outcome: Outcome, log: &Log) -> Self {
        Self {
            name,
            supports,
            outcome,
            log: Arc::clone(log),
        }
    }

    fn record(&self, event: &str) {
        self.log
            .lock()
            .expect("log lock")
            .push(format!("{}:{event}", self.name));
    }
}

impl HevcDecoder for Probe {
    fn supports(&mut self, _config: &HevcConfig) -> bool {
        self.record("supports");
        self.supports
    }

    fn decode_intra(
        &mut self,
        _config: &HevcConfig,
        _payload: &[u8],
    ) -> gamut_core::Result<DecodedFrame> {
        self.record("decode");
        match self.outcome {
            Outcome::Frame(v) => Ok(frame(v)),
            Outcome::Fail => Err(Error::InvalidInput("probe: terminal backend failure")),
            Outcome::LateDecline => Err(Error::Unsupported(BACKEND_DECLINED)),
        }
    }
}

// ============================================================================================
//   The registry
// ============================================================================================

#[test]
fn empty_registry_is_empty_and_declines() {
    let mut decoders = HevcDecoders::new();
    assert_eq!(decoders.len(), 0);
    assert!(decoders.is_empty());
    assert!(!decoders.supports(&config()));

    let err = decoders
        .decode_intra(&config(), &[])
        .expect_err("an empty registry cannot decode");
    assert_eq!(err.kind(), ErrorKind::Unsupported);
    assert_eq!(err.static_message(), Some(NO_BACKEND));
    assert_eq!(err.origin(), Some("gamut-heic"));
}

#[test]
fn default_matches_new() {
    let mut decoders = HevcDecoders::default();
    assert!(decoders.is_empty());
    assert_eq!(format!("{decoders:?}"), "HevcDecoders { backends: 0 }");

    let log = log();
    decoders.push_backend(Probe::new("a", true, Outcome::Frame(7), &log));
    assert_eq!(format!("{decoders:?}"), "HevcDecoders { backends: 1 }");
    assert_eq!(decoders.len(), 1);
    assert!(!decoders.is_empty());
}

#[test]
fn push_backend_chains_and_preserves_order() {
    let log = log();
    let mut decoders = HevcDecoders::new();
    decoders
        .push_backend(Probe::new("first", false, Outcome::Frame(1), &log))
        .push_backend(Probe::new("second", true, Outcome::Frame(2), &log))
        .push_backend(Probe::new("third", true, Outcome::Frame(3), &log));
    assert_eq!(decoders.len(), 3);

    let decoded = decoders.decode_intra(&config(), &[]).expect("second wins");
    assert_eq!(decoded.y(), &[2, 2, 2, 2]);
    // `first` declined, `second` accepted and decoded; `third` was never reached.
    assert_eq!(
        entries(&log),
        ["first:supports", "second:supports", "second:decode"]
    );
}

#[test]
fn first_supporting_backend_wins() {
    let log = log();
    let mut decoders = HevcDecoders::new();
    decoders
        .push_backend(Probe::new("a", true, Outcome::Frame(11), &log))
        .push_backend(Probe::new("b", true, Outcome::Frame(22), &log));

    let decoded = decoders.decode_intra(&config(), &[]).expect("a wins");
    assert_eq!(decoded.y(), &[11, 11, 11, 11]);
    assert_eq!(entries(&log), ["a:supports", "a:decode"]);
}

#[test]
fn accepted_then_failed_propagates_and_stops() {
    let log = log();
    let mut decoders = HevcDecoders::new();
    decoders
        .push_backend(Probe::new("failing", true, Outcome::Fail, &log))
        .push_backend(Probe::new("rescue", true, Outcome::Frame(9), &log));

    let err = decoders
        .decode_intra(&config(), &[])
        .expect_err("the accepting backend's error propagates");
    assert!(matches!(err, Error::InvalidInput(m) if m == "probe: terminal backend failure"));
    // `rescue` must NOT have been consulted at all — not even `supports`.
    assert_eq!(entries(&log), ["failing:supports", "failing:decode"]);
}

#[test]
fn late_decline_falls_through_to_the_next_backend() {
    let log = log();
    let mut decoders = HevcDecoders::new();
    decoders
        .push_backend(Probe::new("late", true, Outcome::LateDecline, &log))
        .push_backend(Probe::new("next", true, Outcome::Frame(5), &log));

    let decoded = decoders.decode_intra(&config(), &[]).expect("next decodes");
    assert_eq!(decoded.y(), &[5, 5, 5, 5]);
    assert_eq!(
        entries(&log),
        [
            "late:supports",
            "late:decode",
            "next:supports",
            "next:decode"
        ]
    );
}

#[test]
fn all_declining_backends_yield_unsupported() {
    let log = log();
    let mut decoders = HevcDecoders::new();
    decoders
        .push_backend(Probe::new("a", false, Outcome::Frame(1), &log))
        .push_backend(Probe::new("b", true, Outcome::LateDecline, &log));

    let err = decoders
        .decode_intra(&config(), &[])
        .expect_err("every backend declined");
    assert_eq!(err.kind(), ErrorKind::Unsupported);
    assert_eq!(err.static_message(), Some(NO_BACKEND));
    assert_eq!(entries(&log), ["a:supports", "b:supports", "b:decode"]);
}

/// Only the exact [`BACKEND_DECLINED`] sentinel is a late decline: an accepting backend that fails
/// with *some other* `Error::Unsupported` has still accepted the job, so its error propagates and
/// no later backend is consulted.
#[test]
fn other_unsupported_errors_are_terminal_not_a_late_decline() {
    const OTHER: &str = "probe: unsupported HEVC profile";

    struct Picky {
        log: Log,
    }
    impl HevcDecoder for Picky {
        fn decode_intra(
            &mut self,
            _config: &HevcConfig,
            _payload: &[u8],
        ) -> gamut_core::Result<DecodedFrame> {
            self.log
                .lock()
                .expect("log lock")
                .push("picky:decode".into());
            Err(Error::Unsupported(OTHER))
        }
    }

    let log = log();
    let mut decoders = HevcDecoders::new();
    decoders
        .push_backend(Picky {
            log: Arc::clone(&log),
        })
        .push_backend(Probe::new("rescue", true, Outcome::Frame(4), &log));

    let err = decoders
        .decode_intra(&config(), &[])
        .expect_err("a non-sentinel Unsupported is terminal");
    assert!(matches!(err, Error::Unsupported(m) if m == OTHER));
    assert_eq!(entries(&log), ["picky:decode"]);
}

#[test]
fn registry_supports_is_any_and_short_circuits() {
    let log = log();
    let mut decoders = HevcDecoders::new();
    decoders
        .push_backend(Probe::new("a", false, Outcome::Frame(1), &log))
        .push_backend(Probe::new("b", true, Outcome::Frame(2), &log))
        .push_backend(Probe::new("c", true, Outcome::Frame(3), &log));

    assert!(decoders.supports(&config()));
    assert_eq!(entries(&log), ["a:supports", "b:supports"]);
}

#[test]
fn registry_supports_is_false_when_all_decline() {
    let log = log();
    let mut decoders = HevcDecoders::new();
    decoders
        .push_backend(Probe::new("a", false, Outcome::Frame(1), &log))
        .push_backend(Probe::new("b", false, Outcome::Frame(2), &log));

    assert!(!decoders.supports(&config()));
    assert_eq!(entries(&log), ["a:supports", "b:supports"]);
}

/// The defaulted trait method keeps pre-registry implementations working: a `HevcDecoder` that
/// only implements `decode_intra` supports everything.
#[test]
fn default_supports_is_true() {
    struct Legacy;
    impl HevcDecoder for Legacy {
        fn decode_intra(
            &mut self,
            _config: &HevcConfig,
            _payload: &[u8],
        ) -> gamut_core::Result<DecodedFrame> {
            Ok(frame(3))
        }
    }

    assert!(Legacy.supports(&config()));
    let mut decoders = HevcDecoders::new();
    decoders.push_backend(Legacy);
    assert!(decoders.supports(&config()));
    assert_eq!(
        decoders.decode_intra(&config(), &[]).expect("decodes").y(),
        &[3, 3, 3, 3]
    );
}

// ============================================================================================
//   The codec-abi adapter
// ============================================================================================

#[test]
fn codec_id_is_the_hvc1_fourcc() {
    assert_eq!(HEVC_CODEC_ID, 0x6876_6331);
}

#[test]
fn planar_pixel_formats_are_the_classic_fourccs() {
    assert_eq!(planar_pixel_format(ChromaFormat::Monochrome), 0x5938_3030); // "Y800"
    assert_eq!(planar_pixel_format(ChromaFormat::Yuv420), 0x4934_3230); // "I420"
    assert_eq!(planar_pixel_format(ChromaFormat::Yuv422), 0x4934_3232); // "I422"
    assert_eq!(planar_pixel_format(ChromaFormat::Yuv444), 0x4934_3434); // "I444"
}

/// What an [`AbiProbe`] does when `decode` is called.
#[derive(Clone, Copy)]
enum AbiOutcome {
    /// Fill every plane with `value` and return OK.
    Fill(u16),
    /// Return `Status::UNSUPPORTED` late.
    LateUnsupported,
    /// Return an arbitrary terminal failure status.
    Fail,
}

/// A `gamut-codec-abi` decoder backend that records the descriptors it is handed.
struct AbiProbe {
    supports: bool,
    outcome: AbiOutcome,
    seen_cfg: Option<(u32, u32, u32, u32, Vec<u8>)>,
    seen_codestream: Vec<u8>,
    seen_out: Option<(u32, u32, u32, u32, u32, [usize; 4])>,
    supports_calls: usize,
    decode_calls: usize,
}

impl AbiProbe {
    fn new(supports: bool, outcome: AbiOutcome) -> Self {
        Self {
            supports,
            outcome,
            seen_cfg: None,
            seen_codestream: Vec::new(),
            seen_out: None,
            supports_calls: 0,
            decode_calls: 0,
        }
    }

    fn snapshot(&mut self, cfg: &StreamConfig) {
        assert!(
            cfg.is_abi_current(),
            "the adapter writes a current struct_size"
        );
        // SAFETY: the adapter guarantees `extradata`/`extradata_len` describe live bytes for the
        // duration of the call.
        let extradata = unsafe { cfg.extradata() }.to_vec();
        self.seen_cfg = Some((
            cfg.codec_id,
            cfg.width,
            cfg.height,
            cfg.bit_depth,
            extradata,
        ));
    }
}

impl Decoder for AbiProbe {
    fn supports(&mut self, cfg: &StreamConfig) -> bool {
        self.supports_calls += 1;
        self.snapshot(cfg);
        self.supports
    }

    fn decode(&mut self, cfg: &StreamConfig, codestream: &[u8], out: &ImageDesc) -> Status {
        self.decode_calls += 1;
        self.snapshot(cfg);
        self.seen_codestream = codestream.to_vec();
        assert!(
            out.is_abi_current(),
            "the adapter writes a current struct_size"
        );
        self.seen_out = Some((
            out.pixel_format,
            out.width,
            out.height,
            out.bit_depth,
            out.plane_count,
            out.strides,
        ));
        match self.outcome {
            AbiOutcome::LateUnsupported => Status::UNSUPPORTED,
            AbiOutcome::Fail => Status(7),
            AbiOutcome::Fill(value) => {
                fill_planes(out, value);
                Status::OK
            }
        }
    }
}

/// Writes `value` into every sample of every populated plane of `out`.
fn fill_planes(out: &ImageDesc, value: u16) {
    // Luma.
    let luma = out.width as usize * out.height as usize;
    // SAFETY: the adapter allocated `width * height` u16 samples at `planes[0]`.
    let y = unsafe { std::slice::from_raw_parts_mut(out.planes[0].cast::<u16>(), luma) };
    y.fill(value);
    if out.plane_count == 1 {
        return;
    }
    // Chroma plane dimensions are recoverable from the stride (bytes per row / 2) and the pixel
    // format's vertical subsampling.
    let chroma_w = out.strides[1] / 2;
    let chroma_h = if out.pixel_format == planar_pixel_format(ChromaFormat::Yuv420) {
        (out.height as usize).div_ceil(2)
    } else {
        out.height as usize
    };
    let len = chroma_w * chroma_h;
    for plane in 1..3 {
        // SAFETY: the adapter allocated `len` u16 samples at each chroma plane pointer.
        let p = unsafe { std::slice::from_raw_parts_mut(out.planes[plane].cast::<u16>(), len) };
        p.fill(value.wrapping_add(plane as u16));
    }
}

#[test]
fn abi_adapter_lowers_config_and_maps_planes() {
    let mut cfg = config_with_vps();
    cfg.chroma_format_idc = 1; // 4:2:0
    cfg.bit_depth_luma_minus8 = 2; // 10-bit
    cfg.bit_depth_chroma_minus8 = 2;

    let dims = Dimensions::new(5, 3).expect("dimensions");
    let mut adapter = AbiHevcDecoder::new(AbiProbe::new(true, AbiOutcome::Fill(300)), dims);
    assert_eq!(adapter.dimensions(), dims);

    let payload = [0x00, 0x00, 0x00, 0x03, 0x26, 0x01, 0xDD];
    let decoded = adapter
        .decode_intra(&cfg, &payload)
        .expect("the backend decodes");

    // Frame contract: 5x3 luma, ceil-divided 3x2 chroma, 10-bit, 4:2:0.
    assert_eq!((decoded.width(), decoded.height()), (5, 3));
    assert_eq!(decoded.bit_depth(), 10);
    assert_eq!(decoded.chroma(), ChromaFormat::Yuv420);
    assert_eq!(decoded.chroma_dimensions(), (3, 2));
    assert_eq!(decoded.y(), &[300; 15]);
    assert_eq!(decoded.cb(), &[301; 6]);
    assert_eq!(decoded.cr(), &[302; 6]);

    // Descriptors the adapter handed down.
    let probe = adapter.backend();
    assert_eq!(probe.decode_calls, 1);
    let mut expected_extradata = Vec::new();
    cfg.annex_b(&[], &mut expected_extradata)
        .expect("annex_b emits the parameter sets");
    assert_eq!(
        expected_extradata,
        [0x00, 0x00, 0x00, 0x01, 0x40, 0x01, 0x0C]
    );
    assert_eq!(
        probe.seen_cfg.clone().expect("cfg seen"),
        (HEVC_CODEC_ID, 5, 3, 10, expected_extradata)
    );
    assert_eq!(probe.seen_codestream, payload);
    assert_eq!(
        probe.seen_out.expect("out seen"),
        (
            planar_pixel_format(ChromaFormat::Yuv420),
            5,
            3,
            10,
            3,
            [10, 6, 6, 0]
        )
    );
}

#[test]
fn abi_adapter_maps_a_monochrome_frame_to_one_plane() {
    let cfg = config(); // chroma_format_idc = 0, 8-bit
    let dims = Dimensions::new(4, 2).expect("dimensions");
    let mut adapter = AbiHevcDecoder::new(AbiProbe::new(true, AbiOutcome::Fill(42)), dims);

    let decoded = adapter.decode_intra(&cfg, &[]).expect("decodes");
    assert_eq!(decoded.chroma(), ChromaFormat::Monochrome);
    assert_eq!(decoded.y(), &[42; 8]);
    assert!(decoded.cb().is_empty());
    assert!(decoded.cr().is_empty());

    let out = adapter.backend().seen_out.expect("out seen");
    assert_eq!(
        out,
        (
            planar_pixel_format(ChromaFormat::Monochrome),
            4,
            2,
            8,
            1,
            [8, 0, 0, 0]
        )
    );
}

#[test]
fn abi_adapter_supports_forwards_to_the_backend() {
    let dims = Dimensions::new(2, 2).expect("dimensions");
    let mut yes = AbiHevcDecoder::new(AbiProbe::new(true, AbiOutcome::Fill(1)), dims);
    assert!(yes.supports(&config()));
    assert_eq!(yes.backend().supports_calls, 1);
    assert_eq!(
        yes.backend().seen_cfg.clone().expect("cfg seen").0,
        HEVC_CODEC_ID
    );

    let mut no = AbiHevcDecoder::new(AbiProbe::new(false, AbiOutcome::Fill(1)), dims);
    assert!(!no.supports(&config()));
    assert_eq!(no.backend().supports_calls, 1);
}

#[test]
fn abi_adapter_late_unsupported_falls_through_the_registry() {
    let dims = Dimensions::new(2, 2).expect("dimensions");
    let mut adapter = AbiHevcDecoder::new(AbiProbe::new(true, AbiOutcome::LateUnsupported), dims);
    let err = adapter
        .decode_intra(&config(), &[])
        .expect_err("UNSUPPORTED is a decline");
    assert_eq!(err.kind(), ErrorKind::Unsupported);
    assert_eq!(err.static_message(), Some(BACKEND_DECLINED));
    assert_eq!(err.detail(), Some("codec-abi status -1"));

    // ... and the registry treats it as a fall-through, reaching the next backend.
    let log = log();
    let mut decoders = HevcDecoders::new();
    decoders
        .push_backend(AbiHevcDecoder::new(
            AbiProbe::new(true, AbiOutcome::LateUnsupported),
            dims,
        ))
        .push_backend(Probe::new("next", true, Outcome::Frame(6), &log));
    let decoded = decoders.decode_intra(&config(), &[]).expect("next decodes");
    assert_eq!(decoded.y(), &[6, 6, 6, 6]);
    assert_eq!(entries(&log), ["next:supports", "next:decode"]);
}

#[test]
fn abi_adapter_other_failure_status_propagates() {
    let dims = Dimensions::new(2, 2).expect("dimensions");
    let mut adapter = AbiHevcDecoder::new(AbiProbe::new(true, AbiOutcome::Fail), dims);
    let err = adapter
        .decode_intra(&config(), &[])
        .expect_err("a non-OK status is terminal");
    assert_eq!(err.kind(), ErrorKind::InvalidInput);
    assert_eq!(
        err.static_message(),
        Some("HEIF: HEVC backend returned a failure status")
    );
    assert_eq!(err.detail(), Some("codec-abi status 7"));

    // Through the registry: terminal, so a later backend is never consulted.
    let log = log();
    let mut decoders = HevcDecoders::new();
    decoders
        .push_backend(AbiHevcDecoder::new(
            AbiProbe::new(true, AbiOutcome::Fail),
            dims,
        ))
        .push_backend(Probe::new("rescue", true, Outcome::Frame(6), &log));
    let err = decoders
        .decode_intra(&config(), &[])
        .expect_err("propagates");
    assert_eq!(err.kind(), ErrorKind::InvalidInput);
    assert_eq!(
        err.static_message(),
        Some("HEIF: HEVC backend returned a failure status")
    );
    assert!(entries(&log).is_empty());
}

#[test]
fn abi_adapter_backend_accessors_round_trip() {
    let dims = Dimensions::new(2, 2).expect("dimensions");
    let mut adapter = AbiHevcDecoder::new(AbiProbe::new(true, AbiOutcome::Fill(1)), dims);
    adapter.backend_mut().supports_calls = 5;
    assert_eq!(adapter.backend().supports_calls, 5);
    assert_eq!(adapter.into_backend().supports_calls, 5);
}
