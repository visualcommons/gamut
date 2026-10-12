//! Adapters between the shared [`gamut_codec_abi`] seam and gamut-jxl's typed codestream traits.
//!
//! [`gamut-codec-abi`](gamut_codec_abi) is the workspace-wide door a foreign (C / `-sys`) or
//! alternate codestream backend enters by; [`crate::backend`] is the JPEG-XL-shaped trait pair the
//! host actually calls. These two adapters bridge them, so a C backend reached through
//! [`gamut_codec_abi::bridge::ForeignEncoder`] and a pure-Rust one implementing
//! [`gamut_codec_abi::Encoder`] both plug into
//! [`JxlEncoder::push_backend`](crate::JxlEncoder::push_backend) unchanged:
//!
//! ```text
//! C vtable ──bridge::ForeignEncoder──▶ gamut_codec_abi::Encoder ──AbiEncodeBackend──▶ JxlCodestreamEncoder
//! ```
//!
//! # Status translation
//!
//! [`Status::OK`] is success. A late [`Status::UNSUPPORTED`] — the backend accepted the job at
//! `supports` time and then declined — becomes [`Error::Unsupported`], which the host treats as a
//! decline and falls through to the next backend. **Every other non-OK status is terminal** and
//! becomes an [`Error::InvalidInput`] that propagates to the caller, per the seam's fallback
//! contract: a backend that may have produced a partial result is never silently retried.
//!
//! # What the ABI can and cannot carry
//!
//! [`EncodeConfig`] carries a codec id and a `0..=100` quality; [`ImageDesc`] carries the pixel
//! format, dimensions, coded depth, and plane pointers. That covers the raster, the coded bit depth
//! and the lossless/lossy target, but **not** JPEG XL's [`Effort`](crate::Effort) dial,
//! [`ModularMode`] coding-tool selection, [`ColorSpec`]
//! signalling or [`Orientation`] metadata.
//! Rather than emit a stream that silently ignores a request, [`AbiEncodeBackend::supports`]
//! **declines** any job whose colour signalling, orientation or coding tool is non-default. Effort
//! is a pure speed/density free choice with no effect on decoded pixels, so it is simply not
//! conveyed and an ABI backend picks its own; a pinned [`ModularMode`] is not
//! in that class — it reshapes the codestream — so it is declined rather than dropped.

use gamut_codec_abi::{
    Decoder as AbiDecoder, EncodeConfig, Encoder as AbiEncoder, ImageDesc, MAX_PLANES, Status,
    StreamConfig,
};
use gamut_core::{Error, Result};

use crate::backend::{
    JxlCodestreamDecoder, JxlCodestreamEncoder, JxlDecoded, JxlEncodeRequest, JxlImageRef,
    JxlOwnedSamples, JxlSamples, JxlStreamInfo, layout_of,
};
use crate::config::{ColorSpec, ModularMode, Orientation};

/// The codec identifier gamut-jxl puts in every [`StreamConfig`] / [`EncodeConfig`] it builds: the
/// four-character code `"jxl "` read big-endian (`0x6A_78_6C_20`).
///
/// A backend registered for JPEG XL matches on this value; it is a permanent part of the wire
/// contract and never changes.
pub const JXL_CODEC_ID: u32 = u32::from_be_bytes(*b"jxl ");

/// The quality value [`EncodeConfig`] carries for a mathematically lossless job.
const LOSSLESS_QUALITY: u32 = 100;

/// Maps a JPEG XL request onto the ABI's `0..=100` quality scale.
///
/// The mapping is a **frozen contract** so an ABI backend can rely on it:
///
/// - lossless → [`LOSSLESS_QUALITY`] (`100`), the only value that means "bit-exact";
/// - lossy at Butteraugli distance `d` ∈ (0, 25] → `100 - round(4 · d)`, clamped to `0..=99`.
///
/// The scale is therefore monotonically decreasing in distance (a larger distance is lower
/// quality), it spans the whole lossy range (`d = 25` → `0`), and no lossy job can ever collide with
/// the lossless sentinel.
fn quality_for(req: &JxlEncodeRequest) -> u32 {
    match req.distance() {
        None => LOSSLESS_QUALITY,
        Some(d) => {
            let scaled = (4.0 * f64::from(d.get())).round();
            let quality = 100.0 - scaled;
            // `Distance` is validated finite and in (0, 25], so `scaled` is in [0, 100]; the clamp
            // pins the lossy band to 0..=99 so it can never read as the lossless sentinel.
            (quality.clamp(0.0, 99.0)) as u32
        }
    }
}

/// Whether the ABI descriptors can faithfully carry `req`.
///
/// Colour signalling, orientation and the coding-tool selection have no [`EncodeConfig`] field, so a
/// non-default request for any of them is declined rather than dropped (see the
/// [module docs](self#what-the-abi-can-and-cannot-carry)).
fn is_conveyable(req: &JxlEncodeRequest) -> bool {
    *req.color() == ColorSpec::Srgb
        && req.orientation() == Orientation::Identity
        && req.modular() == ModularMode::Auto
}

/// Builds an [`ImageDesc`] over a borrowed encode raster: one interleaved plane, tightly packed.
///
/// The plane pointer is `*mut u8` because [`ImageDesc`] is used in both directions; as an *encode
/// input* it is read-only, exactly as the descriptor documents. Deriving `*mut` from a shared slice
/// is a plain pointer cast — no dereference, so the crate stays `#![deny(unsafe_code)]`.
fn image_desc(image: &JxlImageRef<'_>, coded_depth: u32) -> ImageDesc {
    let bytes_per_sample = (image.bits_per_sample() / 8) as usize;
    let stride = (image.dimensions().width as usize)
        .saturating_mul(image.channels() as usize)
        .saturating_mul(bytes_per_sample);
    let base = match image.samples() {
        JxlSamples::U8(s) => s.as_ptr().cast_mut(),
        JxlSamples::U16(s) => s.as_ptr().cast::<u8>().cast_mut(),
    };
    let mut planes = [core::ptr::null_mut(); MAX_PLANES];
    planes[0] = base;
    let mut strides = [0usize; MAX_PLANES];
    strides[0] = stride;
    ImageDesc::new(
        image.format() as u32,
        image.dimensions().width,
        image.dimensions().height,
        coded_depth,
        1,
        planes,
        strides,
    )
}

/// Translates a terminal (non-OK) [`Status`] into a typed error.
///
/// [`Status::UNSUPPORTED`] is the fall-through code and becomes [`Error::Unsupported`] — a late
/// decline the host converts back into "try the next backend". Anything else is a backend failure
/// and becomes [`Error::InvalidInput`], which propagates.
fn status_error(status: Status, declined: &'static str, failed: &'static str) -> Error {
    let classified = if status.is_unsupported() {
        Error::unsupported(env!("CARGO_PKG_NAME"), declined)
    } else {
        Error::invalid_input(env!("CARGO_PKG_NAME"), failed)
    };
    classified.with_detail(format!("codec-abi status {}", status.0))
}

/// Adapts a [`gamut_codec_abi::Encoder`] into a [`JxlCodestreamEncoder`] that can be pushed onto a
/// [`JxlEncoder`](crate::JxlEncoder).
///
/// The wrapped backend must produce a **bare JPEG XL codestream** (signature `FF 0A`) — the seam's
/// boundary — streamed through the ABI's write callback.
pub struct AbiEncodeBackend<E> {
    /// The wrapped ABI backend.
    inner: E,
}

impl<E: AbiEncoder + Send> AbiEncodeBackend<E> {
    /// Wraps an ABI encoder as a gamut-jxl codestream backend.
    #[must_use]
    pub fn new(inner: E) -> Self {
        Self { inner }
    }

    /// Borrows the wrapped ABI backend.
    #[must_use]
    pub fn get_ref(&self) -> &E {
        &self.inner
    }

    /// Unwraps the adapter, returning the ABI backend.
    #[must_use]
    pub fn into_inner(self) -> E {
        self.inner
    }
}

impl<E: AbiEncoder + Send> JxlCodestreamEncoder for AbiEncodeBackend<E> {
    fn supports(&mut self, req: &JxlEncodeRequest) -> bool {
        is_conveyable(req)
            && self
                .inner
                .supports(&EncodeConfig::new(JXL_CODEC_ID, quality_for(req)))
    }

    fn encode(&mut self, req: &JxlEncodeRequest, image: &JxlImageRef<'_>) -> Result<Vec<u8>> {
        if !is_conveyable(req) {
            // A late decline mirroring `supports`: the host falls through to the next backend
            // rather than letting the colour/orientation request be silently dropped.
            return Err(Error::unsupported(
                env!("CARGO_PKG_NAME"),
                "JXL: codec-abi encode cannot carry this colour or orientation request",
            ));
        }
        let cfg = EncodeConfig::new(JXL_CODEC_ID, quality_for(req));
        let desc = image_desc(image, req.coded_bit_depth());
        let mut out = Vec::new();
        let mut sink = |chunk: &[u8]| {
            out.extend_from_slice(chunk);
            Status::OK
        };
        let status = self.inner.encode(&cfg, &desc, &mut sink);
        if status.is_ok() {
            Ok(out)
        } else {
            Err(status_error(
                status,
                "JXL: codec-abi encode backend declined the job",
                "JXL: codec-abi encode backend failed",
            ))
        }
    }
}

/// Adapts a [`gamut_codec_abi::Decoder`] into a [`JxlCodestreamDecoder`] that can be pushed onto a
/// [`JxlDecoder`](crate::JxlDecoder).
///
/// The ABI decodes into a **caller-allocated** buffer, so the adapter must know the output
/// dimensions before the call. It takes them from [`JxlStreamInfo::dimensions`] and **declines** when
/// the host could not determine them (no built-in header parser compiled in), rather than guessing.
pub struct AbiDecodeBackend<D> {
    /// The wrapped ABI backend.
    inner: D,
}

impl<D: AbiDecoder + Send> AbiDecodeBackend<D> {
    /// Wraps an ABI decoder as a gamut-jxl codestream backend.
    #[must_use]
    pub fn new(inner: D) -> Self {
        Self { inner }
    }

    /// Borrows the wrapped ABI backend.
    #[must_use]
    pub fn get_ref(&self) -> &D {
        &self.inner
    }

    /// Unwraps the adapter, returning the ABI backend.
    #[must_use]
    pub fn into_inner(self) -> D {
        self.inner
    }
}

impl<D: AbiDecoder + Send> JxlCodestreamDecoder for AbiDecodeBackend<D> {
    fn supports(&mut self, info: &JxlStreamInfo) -> bool {
        let (Some(dims), Some((_, _, bits))) = (info.dimensions(), layout_of(info.format())) else {
            return false;
        };
        self.inner.supports(&StreamConfig::new(
            JXL_CODEC_ID,
            dims.width,
            dims.height,
            bits,
        ))
    }

    fn decode(&mut self, info: &JxlStreamInfo, codestream: &[u8]) -> Result<JxlDecoded> {
        let Some(dims) = info.dimensions() else {
            // A late decline: without dimensions there is no buffer to decode into.
            return Err(Error::unsupported(
                env!("CARGO_PKG_NAME"),
                "JXL: codec-abi decode needs stream dimensions the host could not determine",
            ));
        };
        let (color_channels, has_alpha, bits) = layout_of(info.format()).ok_or_else(|| {
            Error::unsupported(
                env!("CARGO_PKG_NAME"),
                "JXL: pixel format is not a JPEG XL coded layout",
            )
        })?;
        let channels = color_channels + u32::from(has_alpha);
        let count = dims
            .sample_count(channels as usize)
            .ok_or_else(|| Error::invalid_input(env!("CARGO_PKG_NAME"), "JXL: image too large"))?;
        let bytes_per_sample = (bits / 8) as usize;
        let stride = (dims.width as usize)
            .saturating_mul(channels as usize)
            .saturating_mul(bytes_per_sample);

        let cfg = StreamConfig::new(JXL_CODEC_ID, dims.width, dims.height, bits);
        let mut strides = [0usize; MAX_PLANES];
        strides[0] = stride;

        // Allocate the output at the requested storage width, hand the ABI a byte view of it, and
        // wrap the filled buffer back up in the same width.
        let samples = match bits {
            8 => {
                let mut buf = vec![0u8; count];
                let mut planes = [core::ptr::null_mut(); MAX_PLANES];
                planes[0] = buf.as_mut_ptr();
                let desc = ImageDesc::new(
                    info.format() as u32,
                    dims.width,
                    dims.height,
                    bits,
                    1,
                    planes,
                    strides,
                );
                run_decode(&mut self.inner, &cfg, codestream, &desc)?;
                JxlOwnedSamples::U8(buf)
            }
            _ => {
                let mut buf = vec![0u16; count];
                let mut planes = [core::ptr::null_mut(); MAX_PLANES];
                planes[0] = buf.as_mut_ptr().cast::<u8>();
                let desc = ImageDesc::new(
                    info.format() as u32,
                    dims.width,
                    dims.height,
                    bits,
                    1,
                    planes,
                    strides,
                );
                run_decode(&mut self.inner, &cfg, codestream, &desc)?;
                JxlOwnedSamples::U16(buf)
            }
        };

        JxlDecoded::new(info.format(), dims, samples)
    }
}

/// Runs one ABI decode call and translates its status.
fn run_decode<D: AbiDecoder>(
    inner: &mut D,
    cfg: &StreamConfig,
    codestream: &[u8],
    out: &ImageDesc,
) -> Result<()> {
    let status = inner.decode(cfg, codestream, out);
    if status.is_ok() {
        Ok(())
    } else {
        Err(status_error(
            status,
            "JXL: codec-abi decode backend declined the job",
            "JXL: codec-abi decode backend failed",
        ))
    }
}

#[cfg(test)]
mod tests {
    use gamut_core::{Dimensions, PixelFormat};

    use super::*;
    use crate::backend::{JxlFraming, JxlSamples};
    use crate::config::{Distance, Effort};

    /// A request with the given distance and otherwise ABI-conveyable settings.
    fn request(distance: Option<Distance>) -> JxlEncodeRequest {
        JxlEncodeRequest::new(
            distance,
            Effort::Squirrel,
            ModularMode::Auto,
            8,
            ColorSpec::Srgb,
            Orientation::Identity,
        )
    }

    #[test]
    fn codec_id_is_the_jxl_fourcc() {
        assert_eq!(JXL_CODEC_ID, 0x6A78_6C20);
        assert_eq!(JXL_CODEC_ID.to_be_bytes(), *b"jxl ");
    }

    #[test]
    fn quality_mapping_is_the_frozen_contract() {
        // Lossless is the sentinel 100 and nothing else reaches it.
        assert_eq!(quality_for(&request(None)), 100);
        assert_eq!(quality_for(&request(Some(Distance::new(1.0).unwrap()))), 96);
        assert_eq!(quality_for(&request(Some(Distance::new(0.5).unwrap()))), 98);
        assert_eq!(quality_for(&request(Some(Distance::new(2.0).unwrap()))), 92);
        assert_eq!(quality_for(&request(Some(Distance::new(25.0).unwrap()))), 0);
        // Below 0.25 the unclamped value would be 100; the clamp keeps the lossy band at 99.
        assert_eq!(
            quality_for(&request(Some(Distance::new(f32::MIN_POSITIVE).unwrap()))),
            99
        );
        // Monotonically non-increasing across the whole validated distance range.
        let mut previous = 100;
        for step in 1..=250u32 {
            let d = Distance::new(step as f32 / 10.0).unwrap();
            let q = quality_for(&request(Some(d)));
            assert!(q <= previous, "quality rose at distance {}", d.get());
            assert!(q < 100, "lossy quality collided with the lossless sentinel");
            previous = q;
        }
    }

    #[test]
    fn conveyable_only_for_default_colour_orientation_and_coding_tool() {
        assert!(is_conveyable(&request(None)));
        assert!(!is_conveyable(&JxlEncodeRequest::new(
            None,
            Effort::Squirrel,
            ModularMode::Auto,
            8,
            ColorSpec::Pq,
            Orientation::Identity
        )));
        assert!(!is_conveyable(&JxlEncodeRequest::new(
            None,
            Effort::Squirrel,
            ModularMode::Auto,
            8,
            ColorSpec::Srgb,
            Orientation::Rotate180
        )));
        // A pinned coding tool has no EncodeConfig field either, so both non-default modes decline.
        for modular in [ModularMode::Modular, ModularMode::VarDct] {
            assert!(
                !is_conveyable(&JxlEncodeRequest::new(
                    Some(Distance::new(1.0).unwrap()),
                    Effort::Squirrel,
                    modular,
                    8,
                    ColorSpec::Srgb,
                    Orientation::Identity
                )),
                "{modular:?} should not be conveyable"
            );
        }
    }

    #[test]
    fn status_error_separates_decline_from_failure() {
        let declined = status_error(Status::UNSUPPORTED, "declined", "failed");
        assert_eq!(declined.kind(), gamut_core::ErrorKind::Unsupported);
        assert_eq!(declined.static_message(), Some("declined"));
        assert_eq!(declined.detail(), Some("codec-abi status -1"));

        for status in [Status(-7), Status(1)] {
            let failed = status_error(status, "declined", "failed");
            assert_eq!(failed.kind(), gamut_core::ErrorKind::InvalidInput);
            assert_eq!(failed.static_message(), Some("failed"));
            assert!(failed.detail().is_some());
        }
    }

    #[test]
    fn image_desc_describes_one_tight_interleaved_plane() {
        let dims = Dimensions::new(4, 2).unwrap();
        let pixels = [0u8; 4 * 2 * 4];
        let image = JxlImageRef::new(PixelFormat::Rgba8, dims, JxlSamples::U8(&pixels)).unwrap();
        let desc = image_desc(&image, 8);
        assert_eq!(desc.pixel_format, PixelFormat::Rgba8 as u32);
        assert_eq!(desc.width, 4);
        assert_eq!(desc.height, 2);
        assert_eq!(desc.depth, 8);
        assert_eq!(desc.plane_count, 1);
        assert_eq!(desc.strides[0], 16);
        assert_eq!(desc.strides[1], 0);
        assert!(!desc.planes[0].is_null());
        assert!(desc.planes[1].is_null());
        assert!(desc.is_abi_current());

        // 16-bit: the stride counts bytes, so it doubles; the coded depth is carried verbatim.
        let wide = [0u16; 4 * 2 * 3];
        let image = JxlImageRef::new(PixelFormat::Rgb16, dims, JxlSamples::U16(&wide)).unwrap();
        let desc = image_desc(&image, 10);
        assert_eq!(desc.strides[0], 24);
        assert_eq!(desc.depth, 10);
    }

    /// An ABI encoder that answers `supports` from a flag and `encode` with a fixed status,
    /// emitting a fixed payload through the sink on the OK path.
    struct FakeAbiEncoder {
        supported: bool,
        status: Status,
        payload: Vec<u8>,
        /// The config the last `encode` call saw.
        seen_quality: Option<u32>,
    }

    impl AbiEncoder for FakeAbiEncoder {
        fn supports(&mut self, cfg: &EncodeConfig) -> bool {
            assert_eq!(cfg.codec_id, JXL_CODEC_ID);
            self.supported
        }

        fn encode(
            &mut self,
            cfg: &EncodeConfig,
            image: &ImageDesc,
            sink: &mut dyn FnMut(&[u8]) -> Status,
        ) -> Status {
            self.seen_quality = Some(cfg.quality);
            assert_eq!(image.plane_count, 1);
            if self.status.is_ok() {
                // Two chunks, to prove the sink concatenates.
                let (a, b) = self.payload.split_at(self.payload.len() / 2);
                let first = sink(a);
                assert!(first.is_ok());
                sink(b)
            } else {
                self.status
            }
        }
    }

    /// A 2×2 RGB8 raster and the borrowed image ref over it.
    const RGB_2X2: [u8; 12] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];

    #[test]
    fn abi_encode_backend_round_trips_bytes_and_forwards_quality() {
        let mut backend = AbiEncodeBackend::new(FakeAbiEncoder {
            supported: true,
            status: Status::OK,
            payload: vec![0xFF, 0x0A, 0xAB, 0xCD],
            seen_quality: None,
        });
        let req = request(Some(Distance::new(2.0).unwrap()));
        assert!(backend.supports(&req));

        let dims = Dimensions::new(2, 2).unwrap();
        let image = JxlImageRef::new(PixelFormat::Rgb8, dims, JxlSamples::U8(&RGB_2X2)).unwrap();
        let bytes = backend.encode(&req, &image).expect("encode");
        assert_eq!(bytes, vec![0xFF, 0x0A, 0xAB, 0xCD]);
        assert_eq!(backend.get_ref().seen_quality, Some(92));
        assert_eq!(backend.into_inner().seen_quality, Some(92));
    }

    #[test]
    fn abi_encode_backend_declines_and_propagates_per_status() {
        let dims = Dimensions::new(2, 2).unwrap();
        let image = JxlImageRef::new(PixelFormat::Rgb8, dims, JxlSamples::U8(&RGB_2X2)).unwrap();
        let req = request(None);

        // supports=false is an early decline.
        let mut declining = AbiEncodeBackend::new(FakeAbiEncoder {
            supported: false,
            status: Status::OK,
            payload: Vec::new(),
            seen_quality: None,
        });
        assert!(!declining.supports(&req));

        // A late UNSUPPORTED is a decline expressed as Error::Unsupported.
        let mut late = AbiEncodeBackend::new(FakeAbiEncoder {
            supported: true,
            status: Status::UNSUPPORTED,
            payload: Vec::new(),
            seen_quality: None,
        });
        let error = late.encode(&req, &image).unwrap_err();
        assert_eq!(error.kind(), gamut_core::ErrorKind::Unsupported);
        assert_eq!(
            error.static_message(),
            Some("JXL: codec-abi encode backend declined the job")
        );

        // Any other status is a terminal failure.
        let mut failing = AbiEncodeBackend::new(FakeAbiEncoder {
            supported: true,
            status: Status(-42),
            payload: Vec::new(),
            seen_quality: None,
        });
        let error = failing.encode(&req, &image).unwrap_err();
        assert_eq!(error.kind(), gamut_core::ErrorKind::InvalidInput);
        assert_eq!(
            error.static_message(),
            Some("JXL: codec-abi encode backend failed")
        );
    }

    #[test]
    fn abi_encode_backend_declines_unconveyable_requests_both_ways() {
        let dims = Dimensions::new(2, 2).unwrap();
        let image = JxlImageRef::new(PixelFormat::Rgb8, dims, JxlSamples::U8(&RGB_2X2)).unwrap();
        let odd = JxlEncodeRequest::new(
            None,
            Effort::Squirrel,
            ModularMode::Auto,
            8,
            ColorSpec::Srgb,
            Orientation::Rotate90Cw,
        );
        let mut backend = AbiEncodeBackend::new(FakeAbiEncoder {
            supported: true,
            status: Status::OK,
            payload: vec![0xFF, 0x0A],
            seen_quality: None,
        });
        assert!(!backend.supports(&odd));
        let error = backend.encode(&odd, &image).unwrap_err();
        assert_eq!(error.kind(), gamut_core::ErrorKind::Unsupported);
        assert_eq!(
            error.static_message(),
            Some("JXL: codec-abi encode cannot carry this colour or orientation request")
        );
        // The inner backend was never asked to encode.
        assert_eq!(backend.get_ref().seen_quality, None);
    }

    /// An ABI decoder that fills the output plane with a fixed byte and reports a fixed status.
    struct FakeAbiDecoder {
        supported: bool,
        status: Status,
        fill: u8,
        seen_size: Option<(u32, u32, u32)>,
    }

    impl AbiDecoder for FakeAbiDecoder {
        fn supports(&mut self, cfg: &StreamConfig) -> bool {
            assert_eq!(cfg.codec_id, JXL_CODEC_ID);
            self.seen_size = Some((cfg.width, cfg.height, cfg.bit_depth));
            self.supported
        }

        fn decode(&mut self, cfg: &StreamConfig, _stream: &[u8], out: &ImageDesc) -> Status {
            self.seen_size = Some((cfg.width, cfg.height, cfg.bit_depth));
            if self.status.is_ok() {
                // Write through the plane pointer exactly as a real backend would; the host
                // allocated `height * stride` bytes.
                let len = (out.height as usize) * out.strides[0];
                for i in 0..len {
                    // SAFETY-free: the write goes through a slice built by the host, not raw
                    // pointers — this fake reconstructs it from the descriptor it was handed.
                    let byte = self.fill;
                    unsafe_write(out.planes[0], i, byte);
                }
            }
            self.status
        }
    }

    /// The one raw write the decode fake needs, isolated so the `unsafe` stays in the test module.
    #[allow(unsafe_code)]
    fn unsafe_write(base: *mut u8, offset: usize, value: u8) {
        // SAFETY: the host allocated `height * stride` bytes at `base` and the caller keeps
        // `offset` inside that region.
        unsafe { base.add(offset).write(value) };
    }

    #[test]
    fn abi_decode_backend_fills_the_requested_layout() {
        let dims = Dimensions::new(2, 2).unwrap();
        let info = JxlStreamInfo::new(
            PixelFormat::Gray8,
            JxlFraming::Codestream,
            Some(dims),
            false,
        );
        let mut backend = AbiDecodeBackend::new(FakeAbiDecoder {
            supported: true,
            status: Status::OK,
            fill: 0x5A,
            seen_size: None,
        });
        assert!(backend.supports(&info));
        assert_eq!(backend.get_ref().seen_size, Some((2, 2, 8)));

        let decoded = backend.decode(&info, &[0xFF, 0x0A]).expect("decode");
        assert_eq!(decoded.format(), PixelFormat::Gray8);
        assert_eq!(decoded.dimensions(), dims);
        assert_eq!(decoded.samples(), &JxlOwnedSamples::U8(vec![0x5A; 4]));
    }

    #[test]
    fn abi_decode_backend_allocates_sixteen_bit_output() {
        let dims = Dimensions::new(2, 2).unwrap();
        let info = JxlStreamInfo::new(
            PixelFormat::Rgba16,
            JxlFraming::Codestream,
            Some(dims),
            true,
        );
        let mut backend = AbiDecodeBackend::new(FakeAbiDecoder {
            supported: true,
            status: Status::OK,
            fill: 0x11,
            seen_size: None,
        });
        assert!(backend.supports(&info));
        assert_eq!(backend.get_ref().seen_size, Some((2, 2, 16)));
        let decoded = backend.decode(&info, &[0xFF, 0x0A]).expect("decode");
        // 2x2 RGBA at 16 bits: 16 samples, every byte 0x11.
        assert_eq!(
            decoded.into_samples(),
            JxlOwnedSamples::U16(vec![0x1111; 16])
        );
    }

    #[test]
    fn abi_decode_backend_declines_without_dimensions() {
        let info = JxlStreamInfo::new(PixelFormat::Gray8, JxlFraming::Codestream, None, false);
        let mut backend = AbiDecodeBackend::new(FakeAbiDecoder {
            supported: true,
            status: Status::OK,
            fill: 0,
            seen_size: None,
        });
        // `supports` declines outright, and the late path is an Unsupported decline too.
        assert!(!backend.supports(&info));
        let error = backend.decode(&info, &[0xFF, 0x0A]).unwrap_err();
        assert_eq!(error.kind(), gamut_core::ErrorKind::Unsupported);
        assert_eq!(
            error.static_message(),
            Some("JXL: codec-abi decode needs stream dimensions the host could not determine")
        );
    }

    #[test]
    fn abi_decode_backend_maps_statuses() {
        let dims = Dimensions::new(1, 1).unwrap();
        let info = JxlStreamInfo::new(
            PixelFormat::Gray8,
            JxlFraming::Codestream,
            Some(dims),
            false,
        );
        let mut late = AbiDecodeBackend::new(FakeAbiDecoder {
            supported: true,
            status: Status::UNSUPPORTED,
            fill: 0,
            seen_size: None,
        });
        let error = late.decode(&info, &[0xFF, 0x0A]).unwrap_err();
        assert_eq!(error.kind(), gamut_core::ErrorKind::Unsupported);
        assert_eq!(
            error.static_message(),
            Some("JXL: codec-abi decode backend declined the job")
        );

        let mut failing = AbiDecodeBackend::new(FakeAbiDecoder {
            supported: true,
            status: Status(9),
            fill: 0,
            seen_size: None,
        });
        let error = failing.decode(&info, &[0xFF, 0x0A]).unwrap_err();
        assert_eq!(error.kind(), gamut_core::ErrorKind::InvalidInput);
        assert_eq!(
            error.static_message(),
            Some("JXL: codec-abi decode backend failed")
        );

        // supports=false is the early decline.
        let mut declining = AbiDecodeBackend::new(FakeAbiDecoder {
            supported: false,
            status: Status::OK,
            fill: 0,
            seen_size: None,
        });
        assert!(!declining.supports(&info));
    }
}
