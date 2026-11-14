//! Backend-selection tests (issues #273, #1086): [`HeifDecoder`]'s ordered backend registry and the
//! [`AbiHevcDecoder`] adapter over the `gamut-codec-abi` decoder seam.
//!
//! These live as an integration test (not an in-crate `#[cfg(test)]` module) because exercising the
//! ABI adapter means *being* a C-shaped backend — writing samples through the `ImageDesc` plane
//! pointers — which the library's `#![forbid(unsafe_code)]` rightly disallows.

mod common;

use std::sync::{Arc, Mutex};

use common::{hvcc_bytes, sps_nal};
use gamut_codec_abi::{Decoder, ImageDesc, Status, StreamConfig};
use gamut_core::{Dimensions, Error, ErrorKind};
use gamut_heic::{
    AbiHevcDecoder, ChromaSubsampling, DecodedFrame, HEVC_CODEC_ID, HeifDecoder,
    HevcCodestreamDecoder, HevcConfig,
};
use gamut_isobmff::{IsoBmffImage, Item, Property, PropertyKind};

// ============================================================================================
//   Fixtures
// ============================================================================================

/// The `hvcC` NAL unit type of a VPS, an SPS.
const VPS: u8 = 32;
const SPS: u8 = 33;

/// A one-NAL IDR_W_RADL payload (type 19: header byte 0x26) with a 4-byte length prefix.
const IDR_PAYLOAD: [u8; 7] = [0x00, 0x00, 0x00, 0x03, 0x26, 0x01, 0xDD];

/// A parsed config whose record carries an SPS for a `width` × `height` picture: monochrome, 8-bit.
fn config_sized(width: u32, height: u32) -> HevcConfig {
    HevcConfig::parse(&hvcc_bytes(&[(SPS, &sps_nal(0, (width, height), None))]))
        .expect("hvcC with an SPS parses")
}

/// The registry tests' config: a 2x2 monochrome picture.
fn config() -> HevcConfig {
    config_sized(2, 2)
}

/// A one-item `heic` file whose `hvc1` primary carries `hvcc` and `payload`.
fn heic_file(item_type: [u8; 4], hvcc: Vec<u8>, payload: Vec<u8>) -> Vec<u8> {
    let item = Item::new(1, item_type)
        .with_properties(vec![Property::new(
            true,
            PropertyKind::CodecConfiguration {
                kind: *b"hvcC",
                data: hvcc,
            },
        )])
        .with_payload(payload);
    IsoBmffImage::new(*b"heic", vec![*b"heic", *b"mif1"], 1, vec![item])
        .to_bytes()
        .expect("the model is writable")
}

/// The registry tests' file: one 2x2 monochrome `hvc1` item.
fn file() -> Vec<u8> {
    heic_file(
        *b"hvc1",
        hvcc_bytes(&[(SPS, &sps_nal(0, (2, 2), None))]),
        IDR_PAYLOAD.to_vec(),
    )
}

/// A 2x2 monochrome frame with a recognisable sample value.
fn frame(value: u16) -> DecodedFrame {
    DecodedFrame::new(
        Dimensions::new(2, 2).expect("dimensions"),
        8,
        ChromaSubsampling::Cs400,
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
    /// Fail terminally with `InvalidInput` — must propagate out of the registry.
    Fail,
    /// Fail with an `ErrorKind::Unsupported` error after accepting — terminal too.
    Unsupported,
}

/// The message of [`Outcome::Unsupported`]'s error.
const PROBE_UNSUPPORTED: &str = "probe: unsupported HEVC profile";

/// A scriptable [`HevcCodestreamDecoder`] that records every call it receives.
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

impl HevcCodestreamDecoder for Probe {
    fn supports(&mut self, _config: &HevcConfig) -> bool {
        self.record("supports");
        self.supports
    }

    fn decode_still(
        &mut self,
        _config: &HevcConfig,
        _payload: &[u8],
    ) -> gamut_core::Result<DecodedFrame> {
        self.record("decode");
        match self.outcome {
            Outcome::Frame(v) => Ok(frame(v)),
            Outcome::Fail => Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "probe: terminal backend failure",
            )),
            Outcome::Unsupported => Err(Error::unsupported(
                env!("CARGO_PKG_NAME"),
                PROBE_UNSUPPORTED,
            )),
        }
    }
}

// ============================================================================================
//   The registry
// ============================================================================================

#[test]
fn an_empty_decoder_declines_with_no_backend() {
    let err = HeifDecoder::new()
        .decode_planar(&file())
        .expect_err("a decoder with no backend cannot decode");
    assert_eq!(err.kind(), ErrorKind::Unsupported);
    assert_eq!(err.static_message(), Some(gamut_heif::NO_BACKEND));
    assert_eq!(err.origin(), Some("gamut-heic"));
}

#[test]
fn default_matches_new_and_debug_counts_backends() {
    let decoder = HeifDecoder::default();
    assert!(format!("{decoder:?}").starts_with("HeifDecoder { backends: 0,"));

    let log = log();
    let decoder = decoder.with_backend(Probe::new("a", true, Outcome::Frame(7), &log));
    assert!(format!("{decoder:?}").starts_with("HeifDecoder { backends: 1,"));
}

#[test]
fn with_backend_chains_and_preserves_order() {
    let log = log();
    let decoder = HeifDecoder::new()
        .with_backend(Probe::new("first", false, Outcome::Frame(1), &log))
        .with_backend(Probe::new("second", true, Outcome::Frame(2), &log))
        .with_backend(Probe::new("third", true, Outcome::Frame(3), &log));
    assert!(format!("{decoder:?}").starts_with("HeifDecoder { backends: 3,"));

    let decoded = decoder.decode_planar(&file()).expect("second wins");
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
    let decoder = HeifDecoder::new()
        .with_backend(Probe::new("a", true, Outcome::Frame(11), &log))
        .with_backend(Probe::new("b", true, Outcome::Frame(22), &log));

    let decoded = decoder.decode_planar(&file()).expect("a wins");
    assert_eq!(decoded.y(), &[11, 11, 11, 11]);
    assert_eq!(entries(&log), ["a:supports", "a:decode"]);
}

#[test]
fn accepted_then_failed_propagates_and_stops() {
    let log = log();
    let decoder = HeifDecoder::new()
        .with_backend(Probe::new("failing", true, Outcome::Fail, &log))
        .with_backend(Probe::new("rescue", true, Outcome::Frame(9), &log));

    let err = decoder
        .decode_planar(&file())
        .expect_err("the accepting backend's error propagates");
    assert!(
        err.kind() == ErrorKind::InvalidInput
            && err.static_message() == Some("probe: terminal backend failure")
    );
    // `rescue` must NOT have been consulted at all — not even `supports`.
    assert_eq!(entries(&log), ["failing:supports", "failing:decode"]);
}

/// `supports() == false` is the only fall-through: a backend that accepts and then reports
/// `ErrorKind::Unsupported` has still accepted the job, so its error propagates unchanged and no
/// later backend is consulted. (Before #1086 a `BACKEND_DECLINED` sentinel fell through here.)
#[test]
fn an_unsupported_error_after_accepting_is_terminal() {
    let log = log();
    let decoder = HeifDecoder::new()
        .with_backend(Probe::new("late", true, Outcome::Unsupported, &log))
        .with_backend(Probe::new("next", true, Outcome::Frame(5), &log));

    let err = decoder
        .decode_planar(&file())
        .expect_err("a late Unsupported is terminal");
    assert_eq!(err.kind(), ErrorKind::Unsupported);
    assert_eq!(err.static_message(), Some(PROBE_UNSUPPORTED));
    assert_eq!(entries(&log), ["late:supports", "late:decode"]);
}

#[test]
fn all_declining_backends_yield_no_backend() {
    let log = log();
    let decoder = HeifDecoder::new()
        .with_backend(Probe::new("a", false, Outcome::Frame(1), &log))
        .with_backend(Probe::new("b", false, Outcome::Frame(2), &log));

    let err = decoder
        .decode_planar(&file())
        .expect_err("every backend declined");
    assert_eq!(err.kind(), ErrorKind::Unsupported);
    assert_eq!(err.static_message(), Some(gamut_heif::NO_BACKEND));
    // Every backend was asked, in order, and none decoded.
    assert_eq!(entries(&log), ["a:supports", "b:supports"]);
}

#[test]
fn backend_selection_short_circuits_at_the_first_acceptance() {
    let log = log();
    let decoder = HeifDecoder::new()
        .with_backend(Probe::new("a", false, Outcome::Frame(1), &log))
        .with_backend(Probe::new("b", true, Outcome::Frame(2), &log))
        .with_backend(Probe::new("c", true, Outcome::Frame(3), &log));

    decoder.decode_planar(&file()).expect("b decodes");
    assert_eq!(entries(&log), ["a:supports", "b:supports", "b:decode"]);
}

// ============================================================================================
//   The codec-abi adapter
// ============================================================================================

#[test]
fn codec_id_is_the_hvc1_fourcc() {
    assert_eq!(HEVC_CODEC_ID, 0x6876_6331);
}

/// The `ImageDesc::pixel_format` FourCCs the adapter hands down: `Y800`, `I420`, `I422`, `I444`.
const Y800: u32 = 0x5938_3030;
const I420: u32 = 0x4934_3230;

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
    let chroma_h = if out.pixel_format == I420 {
        (out.height as usize).div_ceil(2)
    } else {
        out.height as usize
    };
    let len = chroma_w * chroma_h;
    for (plane, &ptr) in (1u16..).zip(&out.planes[1..3]) {
        // SAFETY: the adapter allocated `len` u16 samples at each chroma plane pointer.
        let p = unsafe { std::slice::from_raw_parts_mut(ptr.cast::<u16>(), len) };
        p.fill(value.wrapping_add(plane));
    }
}

#[test]
fn abi_adapter_lowers_config_and_maps_planes() {
    let vps = [0x40, 0x01, 0x0C];
    let sps = sps_nal(1, (5, 3), None);
    let mut cfg = HevcConfig::parse(&hvcc_bytes(&[(VPS, &vps), (SPS, &sps)]))
        .expect("hvcC with a VPS and an SPS parses");
    cfg.chroma_format_idc = 1; // 4:2:0
    cfg.bit_depth_luma_minus8 = 2; // 10-bit
    cfg.bit_depth_chroma_minus8 = 2;

    let mut adapter = AbiHevcDecoder::new(AbiProbe::new(true, AbiOutcome::Fill(300)));
    let decoded = adapter
        .decode_still(&cfg, &IDR_PAYLOAD)
        .expect("the backend decodes");

    // Frame contract: the SPS's 5x3 luma, ceil-divided 3x2 chroma, 10-bit, 4:2:0.
    assert_eq!((decoded.width(), decoded.height()), (5, 3));
    assert_eq!(decoded.bit_depth(), 10);
    assert_eq!(decoded.chroma(), ChromaSubsampling::Cs420);
    assert_eq!(decoded.chroma_dimensions(), (3, 2));
    assert_eq!(decoded.y(), &[300; 15]);
    assert_eq!(decoded.cb(), &[301; 6]);
    assert_eq!(decoded.cr(), &[302; 6]);

    // Descriptors the adapter handed down: the extradata is the record's parameter sets in
    // Annex-B, exactly as `annex_b_parameter_sets` emits them.
    let probe = adapter.get_ref();
    assert_eq!(probe.decode_calls, 1);
    let mut expected_extradata = Vec::new();
    cfg.annex_b_parameter_sets(&mut expected_extradata);
    let mut spelled = vec![0x00, 0x00, 0x00, 0x01];
    spelled.extend_from_slice(&vps);
    spelled.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
    spelled.extend_from_slice(&sps);
    assert_eq!(expected_extradata, spelled);
    assert_eq!(
        probe.seen_cfg.clone().expect("cfg seen"),
        (HEVC_CODEC_ID, 5, 3, 10, expected_extradata)
    );
    assert_eq!(probe.seen_codestream, IDR_PAYLOAD);
    assert_eq!(
        probe.seen_out.expect("out seen"),
        (I420, 5, 3, 10, 3, [10, 6, 6, 0])
    );
}

#[test]
fn abi_adapter_maps_a_monochrome_frame_to_one_plane() {
    let cfg = config_sized(4, 2); // chroma_format_idc = 0, 8-bit
    let mut adapter = AbiHevcDecoder::new(AbiProbe::new(true, AbiOutcome::Fill(42)));

    let decoded = adapter.decode_still(&cfg, &[]).expect("decodes");
    assert_eq!(decoded.chroma(), ChromaSubsampling::Cs400);
    assert_eq!(decoded.y(), &[42; 8]);
    assert!(decoded.cb().is_empty());
    assert!(decoded.cr().is_empty());

    let out = adapter.get_ref().seen_out.expect("out seen");
    assert_eq!(out, (Y800, 4, 2, 8, 1, [8, 0, 0, 0]));
}

#[test]
fn abi_adapter_sizes_the_planes_by_the_conformance_window() {
    // 4:2:0 crops in units of two: left 1 + right 2 columns, top 0 + bottom 1 row of chroma.
    let cfg = HevcConfig::parse(&hvcc_bytes(&[(
        SPS,
        &sps_nal(1, (16, 8), Some([1, 2, 0, 1])),
    )]))
    .expect("hvcC parses");
    let mut adapter = AbiHevcDecoder::new(AbiProbe::new(true, AbiOutcome::Fill(1)));
    let decoded = adapter.decode_still(&cfg, &[]).expect("decodes");
    // The record's chroma_format_idc is 0, so the planes are monochrome; the crop is the SPS's.
    assert_eq!((decoded.width(), decoded.height()), (16 - 6, 8 - 2));
    let seen = adapter.get_ref().seen_cfg.clone().expect("cfg seen");
    assert_eq!((seen.1, seen.2), (10, 6));
}

#[test]
fn abi_adapter_reads_an_hev1_payloads_inband_sps_when_the_record_has_none() {
    let cfg = HevcConfig::parse(&hvcc_bytes(&[])).expect("hvcC parses");
    let sps = sps_nal(0, (6, 4), None);
    let mut payload = (sps.len() as u32).to_be_bytes().to_vec();
    payload.extend_from_slice(&sps);
    payload.extend_from_slice(&IDR_PAYLOAD);

    let mut adapter = AbiHevcDecoder::new(AbiProbe::new(true, AbiOutcome::Fill(9)));
    let decoded = adapter.decode_still(&cfg, &payload).expect("decodes");
    assert_eq!((decoded.width(), decoded.height()), (6, 4));
    assert_eq!(adapter.get_ref().seen_codestream, payload);
}

#[test]
fn abi_adapter_refuses_a_picture_no_sps_sizes() {
    let cfg = HevcConfig::parse(&hvcc_bytes(&[])).expect("hvcC parses");
    let mut adapter = AbiHevcDecoder::new(AbiProbe::new(true, AbiOutcome::Fill(1)));
    let err = adapter
        .decode_still(&cfg, &IDR_PAYLOAD)
        .expect_err("no SPS, no plane size");
    assert_eq!(err.kind(), ErrorKind::InvalidInput);
    assert_eq!(
        err.static_message(),
        Some("HEIC: no SPS states the coded picture size")
    );
    // The backend was never handed planes it could not be told the size of.
    assert_eq!(adapter.get_ref().decode_calls, 0);
}

#[test]
fn abi_adapter_supports_forwards_to_the_backend() {
    let mut yes = AbiHevcDecoder::new(AbiProbe::new(true, AbiOutcome::Fill(1)));
    assert!(yes.supports(&config_sized(6, 4)));
    assert_eq!(yes.get_ref().supports_calls, 1);
    let seen = yes.get_ref().seen_cfg.clone().expect("cfg seen");
    // The record's SPS sizes the probe too.
    assert_eq!((seen.0, seen.1, seen.2), (HEVC_CODEC_ID, 6, 4));

    let mut no = AbiHevcDecoder::new(AbiProbe::new(false, AbiOutcome::Fill(1)));
    assert!(!no.supports(&config()));
    assert_eq!(no.get_ref().supports_calls, 1);
}

#[test]
fn abi_adapter_late_unsupported_is_terminal() {
    let mut adapter = AbiHevcDecoder::new(AbiProbe::new(true, AbiOutcome::LateUnsupported));
    let err = adapter
        .decode_still(&config(), &[])
        .expect_err("a late UNSUPPORTED is an error");
    assert_eq!(err.kind(), ErrorKind::Unsupported);
    assert_eq!(
        err.static_message(),
        Some("HEIC: HEVC backend declined the codestream after accepting it")
    );
    assert_eq!(err.detail(), Some("codec-abi status -1"));

    // ... and the registry does not fall through on it: the next backend is never consulted.
    let log = log();
    let decoder = HeifDecoder::new()
        .with_backend(AbiHevcDecoder::new(AbiProbe::new(
            true,
            AbiOutcome::LateUnsupported,
        )))
        .with_backend(Probe::new("next", true, Outcome::Frame(6), &log));
    let err = decoder.decode_planar(&file()).expect_err("terminal");
    assert_eq!(err.kind(), ErrorKind::Unsupported);
    assert_eq!(
        err.static_message(),
        Some("HEIC: HEVC backend declined the codestream after accepting it")
    );
    assert!(entries(&log).is_empty());
}

#[test]
fn abi_adapter_other_failure_status_propagates() {
    let mut adapter = AbiHevcDecoder::new(AbiProbe::new(true, AbiOutcome::Fail));
    let err = adapter
        .decode_still(&config(), &[])
        .expect_err("a non-OK status is terminal");
    assert_eq!(err.kind(), ErrorKind::InvalidInput);
    assert_eq!(
        err.static_message(),
        Some("HEIC: HEVC backend returned a failure status")
    );
    assert_eq!(err.detail(), Some("codec-abi status 7"));

    // Through the registry: terminal, so a later backend is never consulted.
    let log = log();
    let decoder = HeifDecoder::new()
        .with_backend(AbiHevcDecoder::new(AbiProbe::new(true, AbiOutcome::Fail)))
        .with_backend(Probe::new("rescue", true, Outcome::Frame(6), &log));
    let err = decoder.decode_planar(&file()).expect_err("propagates");
    assert_eq!(err.kind(), ErrorKind::InvalidInput);
    assert_eq!(
        err.static_message(),
        Some("HEIC: HEVC backend returned a failure status")
    );
    assert!(entries(&log).is_empty());
}

#[test]
fn abi_adapter_decodes_through_the_decoder() {
    // The adapter is a backend like any other: in a HeifDecoder it decodes an hvc1 file whose
    // record SPS states the size.
    let decoder = HeifDecoder::new().with_backend(AbiHevcDecoder::new(AbiProbe::new(
        true,
        AbiOutcome::Fill(77),
    )));
    let decoded = decoder.decode_planar(&file()).expect("decodes");
    assert_eq!((decoded.width(), decoded.height()), (2, 2));
    assert_eq!(decoded.y(), &[77; 4]);
}

#[test]
fn abi_adapter_backend_accessors_round_trip() {
    let mut adapter = AbiHevcDecoder::new(AbiProbe::new(true, AbiOutcome::Fill(1)));
    adapter.get_mut().supports_calls = 5;
    assert_eq!(adapter.get_ref().supports_calls, 5);
    assert_eq!(adapter.into_inner().supports_calls, 5);
}
