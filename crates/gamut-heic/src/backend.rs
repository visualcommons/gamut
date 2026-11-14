//! The HEVC codestream backend seam: the typed [`HevcCodestreamDecoder`] trait a caller plugs a
//! platform or reference HEVC decoder into, and the [`AbiHevcDecoder`] adapter that bridges a
//! [`gamut_codec_abi::Decoder`] (and, through [`gamut_codec_abi::bridge::ForeignDecoder`], a
//! C/`-sys` backend) onto it.
//!
//! `gamut-heic` owns everything around the coded picture through `gamut-heif`, but not the HEVC
//! codestream itself (issue #18). [`HeifDecoder`](crate::HeifDecoder) keeps its backends in a
//! [`gamut_codec_abi::Registry`], whose fallback contract is the workspace-wide one:
//!
//! - Backends are tried in **the order they were added**
//!   ([`HeifDecoder::with_backend`](crate::HeifDecoder::with_backend)).
//! - [`supports`](HevcCodestreamDecoder::supports) returning `false` — or, across the C seam,
//!   [`Decoder::supports`] returning `false` — is the **only** fall-through signal.
//! - A backend that *accepts* a job and then fails returns its error unchanged; nothing after it is
//!   tried, so a partial result is never silently masked by a retry.
//! - There is **no implicit software tail**: gamut ships no HEVC codestream decoder, so a coded
//!   item no backend accepts is refused with [`gamut_heif::NO_BACKEND`].

use gamut_codec_abi::{Decoder, ImageDesc, MAX_PLANES, StreamConfig, codec_id, pixel_format};
use gamut_color::ChromaSubsampling;
use gamut_core::{Error, Result};
use gamut_heif::{DecodeLimits, DecodedFrame};

use crate::hvcc::HevcConfig;
use crate::sps;

/// The codec identifier [`AbiHevcDecoder`] writes into [`StreamConfig::codec_id`]: the big-endian
/// FourCC `hvc1` (`0x68766331`).
///
/// A backend keyed on FourCCs can compare against this constant directly; the value is a permanent
/// part of the seam's wire contract and never changes.
pub const HEVC_CODEC_ID: u32 = codec_id(*b"hvc1");

/// The message of the terminal error [`AbiHevcDecoder`] returns when its backend reports
/// [`Status::UNSUPPORTED`](gamut_codec_abi::Status::UNSUPPORTED) from
/// [`decode`](gamut_codec_abi::Decoder::decode), after its `supports` accepted the job.
pub(crate) const LATE_DECLINE: &str =
    "HEIC: HEVC backend declined the codestream after accepting it";

/// The message of the error [`AbiHevcDecoder`] returns when the system refuses a plane.
pub(crate) const PLANE_ALLOCATION: &str = "HEIC: out of memory allocating a decode plane";

/// The message of the terminal error [`AbiHevcDecoder`] returns for any other non-OK status.
pub(crate) const BACKEND_FAILED: &str = "HEIC: HEVC backend returned a failure status";

/// A pluggable HEVC **codestream decoder** backend: decodes one coded `hvc1`/`hev1` item to planar
/// samples.
///
/// gamut-heic decodes the container — and everything around the coded picture — itself; the HEVC
/// codestream is handed to a backend the caller adds with
/// [`HeifDecoder::with_backend`](crate::HeifDecoder::with_backend) (libde265, VideoToolbox,
/// MediaCodec, a hardware block, …). The shape is the workspace's codestream-hook one: object-safe,
/// borrowed bytes in, owned plain data out. `Send` is not a supertrait; the decoder requires it
/// where a backend is added. A C or `-sys` backend reaches this trait through [`AbiHevcDecoder`]
/// rather than implementing it directly.
///
/// `config` is the item's parsed `hvcC` record and `payload` the item's length-prefixed NAL unit
/// stream, already checked against the still-image constraint
/// ([`HevcConfig::validate_still_payload`]: every VCL NAL unit is an IRAP picture). A backend that
/// wants one Annex-B stream bridges the two with [`HevcConfig::annex_b`]; the crate docs tabulate
/// what each platform decoder API wants.
pub trait HevcCodestreamDecoder {
    /// Reports whether this backend will decode a picture coded under `config` — its profile,
    /// tier, level, chroma format and bit depth. Returning `false` is the **only** fall-through to
    /// the next backend; a backend that returns `true` owns the item, and its error is the
    /// caller's.
    fn supports(&mut self, config: &HevcConfig) -> bool;

    /// Decodes the coded item to its planar frame — native bit depth and chroma, before any colour
    /// conversion, `clap`, `irot` or `imir` — whose dimensions are the **post-conformance-window
    /// crop** luma dimensions.
    ///
    /// # Errors
    ///
    /// The backend's own error, returned to the caller unchanged: a backend that accepted the job
    /// owns it.
    fn decode_still(&mut self, config: &HevcConfig, payload: &[u8]) -> Result<DecodedFrame>;
}

/// Adapts a [`gamut_codec_abi::Decoder`] — the workspace's `repr(C)`-shaped codestream seam,
/// whether a pure-Rust backend or a foreign vtable bridged through
/// [`gamut_codec_abi::bridge::ForeignDecoder`] — into an [`HevcCodestreamDecoder`].
///
/// # What it translates
///
/// - **Down**: [`HevcConfig`] becomes a [`StreamConfig`] with [`codec_id`](StreamConfig::codec_id)
///   [`HEVC_CODEC_ID`], the picture size, the record's luma bit depth, and the `hvcC` parameter
///   sets as Annex-B `extradata` (exactly what [`HevcConfig::annex_b_parameter_sets`] emits). The
///   item payload is passed through as the codestream, still length-prefixed.
/// - **Up**: the adapter allocates the `u16` planes itself, hands their pointers to the backend in
///   an [`ImageDesc`] (one plane for monochrome, otherwise Y/Cb/Cr; strides are `plane_width * 2`
///   bytes; the [`pixel_format`](ImageDesc::pixel_format) is the classic FourCC of the chroma
///   layout — `Y800`, `I420`, `I422` or `I444` — with samples always `u16` in native byte order and
///   [`bit_depth`](ImageDesc::bit_depth) carrying the real depth), then rebuilds a
///   [`DecodedFrame`] through the validating [`DecodedFrame::new`].
///
/// # Where the picture size comes from
///
/// An `hvcC` record carries no picture size, and the planes must be sized before the call, so each
/// call reads it from the codestream itself: `pic_width_in_luma_samples` and
/// `pic_height_in_luma_samples` of the first SPS — the record's, or an `hev1` payload's inband one —
/// less its conformance window (ITU-T H.265 §7.4.3.2.1), the cropped picture a decoder outputs. One
/// adapter therefore serves every item of a file, whatever its size, and sits in a
/// [`HeifDecoder`](crate::HeifDecoder) like any other backend. [`supports`](Decoder::supports) is
/// asked with the same size.
///
/// # Resource limits
///
/// The SPS is as untrusted as the rest of the file — `pic_width_in_luma_samples` is a `ue(v)`
/// reaching `2^32 - 1` — and the adapter allocates its planes before the backend runs, so it
/// checks the SPS size against its own [`DecodeLimits`] ([`with_limits`](Self::with_limits);
/// [`DecodeLimits::default`] unless set) first, and reserves the planes fallibly. A picture past
/// the limits is refused with [`DecodeLimits::check`]'s
/// [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) error, and an allocation the
/// system refuses is an [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) error too,
/// never an abort; the backend is not called in either case. A [`HeifDecoder`](crate::HeifDecoder)
/// cannot reach into its backends, so a caller who raises its limits raises the adapter's to
/// match.
///
/// # Status mapping
///
/// [`Status::OK`](gamut_codec_abi::Status::OK) ⇒ a validated [`DecodedFrame`]. Any other status
/// [`decode`](Decoder::decode) returns is terminal — the backend accepted the job — and propagates
/// unchanged out of the registry: [`Status::UNSUPPORTED`](gamut_codec_abi::Status::UNSUPPORTED) as
/// an [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) error, every other status as
/// an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error, each carrying the
/// status code in its detail. Only `supports` falls through.
#[derive(Debug, Clone, Copy)]
pub struct AbiHevcDecoder<D> {
    backend: D,
    /// The caps the SPS picture size is checked against before the planes are allocated.
    limits: DecodeLimits,
}

/// The [`ImageDesc::pixel_format`] tag for planar samples of `chroma`: the classic FourCC of the
/// chroma layout. `chroma` is always one an `hvcC` record can declare.
fn planar_pixel_format(chroma: ChromaSubsampling) -> u32 {
    match chroma {
        ChromaSubsampling::Cs400 => pixel_format::Y800,
        ChromaSubsampling::Cs420 => pixel_format::I420,
        ChromaSubsampling::Cs422 => pixel_format::I422,
        _ => pixel_format::I444,
    }
}

impl<D: Decoder> AbiHevcDecoder<D> {
    /// Wraps `backend`, under [`DecodeLimits::default`].
    #[must_use]
    pub fn new(backend: D) -> Self {
        Self {
            backend,
            limits: DecodeLimits::default(),
        }
    }

    /// Replaces the caps the codestream's picture size is checked against before any plane is
    /// allocated (see [Resource limits](Self#resource-limits)).
    #[must_use]
    pub fn with_limits(mut self, limits: DecodeLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Borrows the wrapped backend.
    #[must_use]
    pub fn get_ref(&self) -> &D {
        &self.backend
    }

    /// Mutably borrows the wrapped backend.
    #[must_use]
    pub fn get_mut(&mut self) -> &mut D {
        &mut self.backend
    }

    /// Consumes the adapter, returning the wrapped backend.
    #[must_use]
    pub fn into_inner(self) -> D {
        self.backend
    }

    /// The [`StreamConfig`] for a `width` × `height` picture coded under `config`, borrowing
    /// `extradata` for the call.
    fn stream_config(
        config: &HevcConfig,
        (width, height): (u32, u32),
        extradata: &[u8],
    ) -> StreamConfig {
        let mut cfg = StreamConfig::new(
            HEVC_CODEC_ID,
            width,
            height,
            u32::from(config.bit_depth_luma()),
        );
        cfg.extradata = extradata.as_ptr();
        cfg.extradata_len = extradata.len();
        cfg
    }
}

/// A zeroed plane of `len` samples, reserved fallibly: a reservation the system refuses is an
/// [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) error rather than an abort.
fn zeroed_plane(len: usize) -> Result<Vec<u16>> {
    let mut plane = Vec::new();
    plane
        .try_reserve_exact(len)
        .map_err(|_| Error::unsupported(env!("CARGO_PKG_NAME"), PLANE_ALLOCATION))?;
    plane.resize(len, 0);
    Ok(plane)
}

/// The `hvcC` parameter sets as an Annex-B `extradata` blob.
fn extradata(config: &HevcConfig) -> Vec<u8> {
    let mut out = Vec::new();
    config.annex_b_parameter_sets(&mut out);
    out
}

impl<D: Decoder> HevcCodestreamDecoder for AbiHevcDecoder<D> {
    /// Forwards to [`Decoder::supports`] with the lowered [`StreamConfig`]. The picture size comes
    /// from the record's SPS; a record without one is lowered with a `0 × 0` size, since a hev1
    /// item's inband SPS is not in reach here — [`decode_still`](Self::decode_still) reads it.
    fn supports(&mut self, config: &HevcConfig) -> bool {
        let size = sps::output_size(config, &[]).map_or((0, 0), |d| (d.width(), d.height()));
        let extradata = extradata(config);
        self.backend
            .supports(&Self::stream_config(config, size, &extradata))
    }

    /// Runs the wrapped backend into planes sized by the codestream's SPS, and maps them back
    /// through [`DecodedFrame::new`].
    ///
    /// # Errors
    ///
    /// An [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error when no SPS
    /// states the picture size or the SPS is malformed, when the picture's size overflows `usize`,
    /// or from [`DecodedFrame::new`]'s validation; an
    /// [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) error when the picture is
    /// past the adapter's [`DecodeLimits`] or a plane cannot be allocated, both before the backend
    /// runs; and the backend's non-OK status, as the [type docs](Self#status-mapping) map it.
    fn decode_still(&mut self, config: &HevcConfig, payload: &[u8]) -> Result<DecodedFrame> {
        let size = sps::output_size(config, payload)?;
        let (width, height) = (size.width(), size.height());
        // The SPS is the file's claim, sized up to `2^32 - 1` a side: hold it to the limits before
        // a single plane exists.
        self.limits.check(width, height, env!("CARGO_PKG_NAME"))?;
        let chroma = config.chroma_subsampling();
        let bit_depth = config.bit_depth_luma();
        let luma_len = size.num_pixels().ok_or_else(|| {
            Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "HEIC: frame dimensions overflow usize",
            )
        })?;
        let (chroma_w, chroma_h) = chroma.chroma_dimensions(width, height);
        // Cannot overflow: each chroma dimension is at most its luma dimension, and `luma_len` fit.
        let chroma_len = chroma_w as usize * chroma_h as usize;
        let monochrome = chroma == ChromaSubsampling::Cs400;

        let mut y = zeroed_plane(luma_len)?;
        let (mut cb, mut cr) = if monochrome {
            (Vec::new(), Vec::new())
        } else {
            (zeroed_plane(chroma_len)?, zeroed_plane(chroma_len)?)
        };

        let mut planes: [*mut u8; MAX_PLANES] = [core::ptr::null_mut(); MAX_PLANES];
        let mut strides = [0usize; MAX_PLANES];
        planes[0] = y.as_mut_ptr().cast::<u8>();
        strides[0] = width as usize * core::mem::size_of::<u16>();
        let plane_count = if monochrome {
            1
        } else {
            planes[1] = cb.as_mut_ptr().cast::<u8>();
            planes[2] = cr.as_mut_ptr().cast::<u8>();
            strides[1] = chroma_w as usize * core::mem::size_of::<u16>();
            strides[2] = strides[1];
            3
        };
        let out = ImageDesc::new(
            planar_pixel_format(chroma),
            width,
            height,
            u32::from(bit_depth),
            plane_count,
            planes,
            strides,
        );

        let extradata = extradata(config);
        let cfg = Self::stream_config(config, (width, height), &extradata);
        let status = self.backend.decode(&cfg, payload, &out);
        if !status.is_ok() {
            let error = if status.is_unsupported() {
                Error::unsupported(env!("CARGO_PKG_NAME"), LATE_DECLINE)
            } else {
                Error::invalid_input(env!("CARGO_PKG_NAME"), BACKEND_FAILED)
            };
            return Err(error.with_detail(format!("codec-abi status {}", status.0)));
        }
        DecodedFrame::new(size, bit_depth, chroma, y, cb, cr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zeroed_plane_refuses_an_impossible_allocation_instead_of_aborting() {
        assert_eq!(zeroed_plane(3).unwrap(), vec![0, 0, 0]);
        // usize::MAX u16s is past isize::MAX bytes, so the reservation fails without allocating.
        let error = zeroed_plane(usize::MAX).unwrap_err();
        assert_eq!(error.kind(), gamut_core::ErrorKind::Unsupported);
        assert_eq!(error.static_message(), Some(PLANE_ALLOCATION));
    }

    #[test]
    fn planar_pixel_formats_are_the_classic_fourccs() {
        assert_eq!(planar_pixel_format(ChromaSubsampling::Cs400), 0x5938_3030); // "Y800"
        assert_eq!(planar_pixel_format(ChromaSubsampling::Cs420), 0x4934_3230); // "I420"
        assert_eq!(planar_pixel_format(ChromaSubsampling::Cs422), 0x4934_3232); // "I422"
        assert_eq!(planar_pixel_format(ChromaSubsampling::Cs444), 0x4934_3434); // "I444"
    }
}
