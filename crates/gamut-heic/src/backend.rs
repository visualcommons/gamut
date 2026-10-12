//! Backend selection for the [`HevcDecoder`] seam (issue #273): the ordered [`HevcDecoders`]
//! registry and the [`AbiHevcDecoder`] adapter over a [`gamut_codec_abi::Decoder`].
//!
//! `gamut-heic` owns the container but not the HEVC codestream (issue #18), so every decode is
//! driven through a caller-supplied [`HevcDecoder`]. This module retrofits the #241 *registry*
//! shape onto that single hook, **additively**: [`HevcDecoders`] is itself a [`HevcDecoder`], so it
//! drops into every existing `&mut dyn HevcDecoder` call site
//! ([`HeifImage::decode_item_planar`](crate::HeifImage::decode_item_planar),
//! [`decode_item_rgba8`](crate::HeifImage::decode_item_rgba8),
//! [`decode_primary_rgba8`](crate::HeifImage::decode_primary_rgba8)) with no signature change.
//!
//! # Fallback contract
//!
//! Mirrors [`gamut_codec_abi`]'s contract exactly:
//!
//! - Backends are tried in **push order**.
//! - [`HevcDecoder::supports`] returning `false` — or a late [`Status::UNSUPPORTED`] surfaced as the
//!   [`BACKEND_DECLINED`] sentinel — is the **only** signal that falls through to the next backend.
//! - A backend that accepts and then fails **propagates**: its error is returned unchanged and no
//!   later backend is consulted, so a partial result is never silently masked.
//! - There is **no implicit software tail**: gamut ships no in-tree HEVC codestream decoder (issue
//!   #18). An empty registry, or one where every backend declines, returns [`Error::Unsupported`].

use gamut_codec_abi::{Decoder, ImageDesc, MAX_PLANES, StreamConfig};
use gamut_core::{Dimensions, Error, Result};

use crate::{ChromaFormat, DecodedFrame, HevcConfig, HevcDecoder};

/// The codec identifier [`AbiHevcDecoder`] writes into [`StreamConfig::codec_id`]: the big-endian
/// FourCC `hvc1` (`0x68766331`).
///
/// A backend keyed on FourCCs can compare against this constant directly; the value is a permanent
/// part of the seam's wire contract and never changes.
pub const HEVC_CODEC_ID: u32 = u32::from_be_bytes(*b"hvc1");

/// The [`Error::Unsupported`] message a backend returns to decline a codestream **after** its
/// [`HevcDecoder::supports`] already said `true` — the "late decline".
///
/// [`HevcDecoders`] treats exactly this message as a fall-through (it tries the next backend);
/// every other error propagates. [`AbiHevcDecoder`] produces it when its wrapped backend returns
/// [`Status::UNSUPPORTED`](gamut_codec_abi::Status::UNSUPPORTED) from
/// [`decode`](gamut_codec_abi::Decoder::decode), which is the C-side spelling of the same signal.
pub const BACKEND_DECLINED: &str = "HEIC: HEVC backend declined the codestream after accepting it";

/// The [`Error::Unsupported`] message [`HevcDecoders`] returns when no backend accepted the job —
/// an empty registry, or every backend declining.
///
/// gamut ships no in-tree software HEVC decoder (issue #18), so there is no implicit fallback tail
/// after the pushed backends.
pub const NO_BACKEND: &str =
    "HEIC: no HEVC backend accepted the codestream (gamut ships no software HEVC decoder)";

/// An ordered registry of HEVC codestream backends that is **itself** a [`HevcDecoder`].
///
/// Push backends in preference order with [`push_backend`](Self::push_backend); hand the registry
/// to any decode entry point exactly where a single backend would go:
///
/// ```
/// use gamut_heic::{ChromaFormat, DecodedFrame, HevcConfig, HevcDecoder, HevcDecoders};
/// use gamut_core::Result;
///
/// struct Gray;
/// impl HevcDecoder for Gray {
///     fn decode_intra(&mut self, _c: &HevcConfig, _p: &[u8]) -> Result<DecodedFrame> {
///         DecodedFrame::new(2, 2, 8, ChromaFormat::Monochrome, vec![128; 4], vec![], vec![])
///     }
/// }
///
/// # // Minimal valid hvcC: 23-byte header, lengthSizeMinusOne = 3, numOfArrays = 0.
/// # let mut raw = vec![0u8; 23];
/// # raw[0] = 1;
/// # raw[21] = 0b0000_0011;
/// # raw[22] = 0;
/// let config = HevcConfig::parse(&raw).unwrap();
///
/// let mut decoders = HevcDecoders::new();
/// decoders.push_backend(Gray);
/// assert_eq!(decoders.len(), 1);
/// // `decoders` is a `HevcDecoder`, so it goes wherever `&mut Gray` would:
/// let frame = decoders.decode_intra(&config, &[]).unwrap();
/// assert_eq!(frame.width(), 2);
/// ```
///
/// Backends are stored as `Box<dyn HevcDecoder + Send>`: [`HevcDecoder`] deliberately has no `Send`
/// supertrait (a single-threaded backend stays usable), so `Send` is bound here, at insertion. The
/// registry is threaded through the decode calls as an argument rather than stored behind a shared
/// handle, so no interior mutability (`Arc`/`Mutex`) is involved.
///
/// See the [`HevcDecoder`] docs for the fallback contract.
#[derive(Default)]
pub struct HevcDecoders {
    backends: Vec<Box<dyn HevcDecoder + Send>>,
}

impl core::fmt::Debug for HevcDecoders {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("HevcDecoders")
            .field("backends", &self.backends.len())
            .finish()
    }
}

impl HevcDecoders {
    /// Creates an empty registry. Decoding through it before any
    /// [`push_backend`](Self::push_backend) yields [`Error::Unsupported`] ([`NO_BACKEND`]).
    #[must_use]
    pub fn new() -> Self {
        Self {
            backends: Vec::new(),
        }
    }

    /// Appends `backend` to the end of the try order and returns `&mut self` for chaining.
    ///
    /// Order is preference order: the first backend whose [`HevcDecoder::supports`] returns `true`
    /// (and that does not late-decline) handles the codestream.
    pub fn push_backend(&mut self, backend: impl HevcDecoder + Send + 'static) -> &mut Self {
        self.backends.push(Box::new(backend));
        self
    }

    /// The number of registered backends.
    #[must_use]
    pub fn len(&self) -> usize {
        self.backends.len()
    }

    /// Returns `true` iff no backend has been pushed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.backends.is_empty()
    }
}

impl HevcDecoder for HevcDecoders {
    /// `true` iff **any** registered backend supports `config`. Backends are queried in push order
    /// and the query short-circuits at the first acceptance.
    fn supports(&mut self, config: &HevcConfig) -> bool {
        self.backends.iter_mut().any(|b| b.supports(config))
    }

    /// Decodes through the first backend that accepts the job.
    ///
    /// # Errors
    ///
    /// Propagates the accepting backend's error unchanged (no later backend is tried), or returns
    /// [`Error::Unsupported`] with the [`NO_BACKEND`] message when every backend declines.
    fn decode_intra(&mut self, config: &HevcConfig, payload: &[u8]) -> Result<DecodedFrame> {
        for backend in &mut self.backends {
            if !backend.supports(config) {
                continue;
            }
            match backend.decode_intra(config, payload) {
                Err(error)
                    if error.kind() == gamut_core::ErrorKind::Unsupported
                        && error.static_message() == Some(BACKEND_DECLINED) =>
                {
                    continue;
                }
                outcome => return outcome,
            }
        }
        Err(Error::unsupported(env!("CARGO_PKG_NAME"), NO_BACKEND))
    }
}

/// Adapts a [`gamut_codec_abi::Decoder`] — the workspace's `repr(C)`-shaped codestream seam, whether
/// a pure-Rust backend or a foreign vtable bridged through
/// [`gamut_codec_abi::bridge::ForeignDecoder`] — into a [`HevcDecoder`].
///
/// # What it translates
///
/// - **Down**: [`HevcConfig`] becomes a [`StreamConfig`] with [`codec_id`](StreamConfig::codec_id)
///   [`HEVC_CODEC_ID`], the adapter's [`dimensions`](Self::dimensions), the record's luma bit depth,
///   and the `hvcC` parameter sets as Annex-B `extradata` (exactly what
///   [`HevcConfig::annex_b_parameter_sets`] emits). The item payload is passed through as the
///   codestream, still length-prefixed.
/// - **Up**: the adapter allocates the `u16` planes itself, hands their pointers to the backend in
///   an [`ImageDesc`] (one plane for [`ChromaFormat::Monochrome`], otherwise Y/Cb/Cr; strides are
///   `plane_width * 2` bytes; [`pixel_format`](ImageDesc::pixel_format) is
///   [`planar_pixel_format`]), then rebuilds a [`DecodedFrame`] through the validating
///   [`DecodedFrame::new`].
///
/// # Why dimensions are supplied
///
/// An `hvcC` record carries no picture size, so the adapter is constructed with the item's `ispe`
/// dimensions (`HeifItem::dimensions`) — the buffer the backend writes into must be sized before
/// the call.
///
/// # Status mapping
///
/// [`Status::OK`](gamut_codec_abi::Status::OK) ⇒ a validated [`DecodedFrame`].
/// [`Status::UNSUPPORTED`](gamut_codec_abi::Status::UNSUPPORTED) returned late from
/// [`decode`](Decoder::decode) ⇒ [`Error::Unsupported`] carrying [`BACKEND_DECLINED`], the sentinel
/// [`HevcDecoders`] falls through on. Any other non-OK status ⇒ [`Error::InvalidInput`] — terminal,
/// and it propagates out of the registry.
#[derive(Debug, Clone, Copy)]
pub struct AbiHevcDecoder<D> {
    backend: D,
    dimensions: Dimensions,
}

/// The [`ImageDesc::pixel_format`] tag for the planar samples the seam exchanges: the classic
/// FourCC of the chroma layout — `Y800`, `I420`, `I422`, or `I444`.
///
/// Samples are **always** `u16` in native byte order regardless of the coded bit depth (matching
/// [`DecodedFrame`]'s uniform plane type); [`ImageDesc::depth`] carries the real bit depth.
#[must_use]
pub const fn planar_pixel_format(chroma: ChromaFormat) -> u32 {
    match chroma {
        ChromaFormat::Monochrome => u32::from_be_bytes(*b"Y800"),
        ChromaFormat::Yuv420 => u32::from_be_bytes(*b"I420"),
        ChromaFormat::Yuv422 => u32::from_be_bytes(*b"I422"),
        ChromaFormat::Yuv444 => u32::from_be_bytes(*b"I444"),
    }
}

impl<D: Decoder> AbiHevcDecoder<D> {
    /// Wraps `backend`, decoding pictures of `dimensions` (the item's `ispe` size).
    #[must_use]
    pub fn new(backend: D, dimensions: Dimensions) -> Self {
        Self {
            backend,
            dimensions,
        }
    }

    /// The picture dimensions this adapter allocates output planes for.
    #[must_use]
    pub fn dimensions(&self) -> Dimensions {
        self.dimensions
    }

    /// Borrows the wrapped backend.
    #[must_use]
    pub fn backend(&self) -> &D {
        &self.backend
    }

    /// Mutably borrows the wrapped backend.
    #[must_use]
    pub fn backend_mut(&mut self) -> &mut D {
        &mut self.backend
    }

    /// Consumes the adapter, returning the wrapped backend.
    #[must_use]
    pub fn into_backend(self) -> D {
        self.backend
    }

    /// Builds the `hvcC` parameter sets as an Annex-B `extradata` blob.
    fn extradata(config: &HevcConfig) -> Vec<u8> {
        let mut out = Vec::new();
        config.annex_b_parameter_sets(&mut out);
        out
    }

    /// Builds the [`StreamConfig`] for `config`, borrowing `extradata` for the call.
    fn stream_config(&self, config: &HevcConfig, extradata: &[u8]) -> StreamConfig {
        let mut cfg = StreamConfig::new(
            HEVC_CODEC_ID,
            self.dimensions.width,
            self.dimensions.height,
            u32::from(config.bit_depth_luma()),
        );
        cfg.extradata = extradata.as_ptr();
        cfg.extradata_len = extradata.len();
        cfg
    }
}

impl<D: Decoder> HevcDecoder for AbiHevcDecoder<D> {
    /// Forwards to [`Decoder::supports`] with the lowered [`StreamConfig`]. Lowering a parsed
    /// [`HevcConfig`] is infallible (the parameter sets come from the record, not the payload), so
    /// this probe reflects only the backend's own answer.
    fn supports(&mut self, config: &HevcConfig) -> bool {
        let extradata = Self::extradata(config);
        let cfg = self.stream_config(config, &extradata);
        self.backend.supports(&cfg)
    }

    /// Runs the wrapped backend and maps its planes back through [`DecodedFrame::new`].
    ///
    /// # Errors
    ///
    /// [`Error::Unsupported`] with the [`BACKEND_DECLINED`] message when the backend returns
    /// [`Status::UNSUPPORTED`](gamut_codec_abi::Status::UNSUPPORTED); [`Error::InvalidInput`] for
    /// any other non-OK status, for a picture whose size overflows `usize`, or from
    /// [`DecodedFrame::new`]'s validation.
    fn decode_intra(&mut self, config: &HevcConfig, payload: &[u8]) -> Result<DecodedFrame> {
        let width = self.dimensions.width;
        let height = self.dimensions.height;
        let chroma = config.chroma_format();
        let bit_depth = config.bit_depth_luma();
        let luma_len = self.dimensions.num_pixels().ok_or_else(|| {
            Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "HEIF: frame dimensions overflow usize",
            )
        })?;
        let (chroma_w, chroma_h) = chroma.chroma_dimensions(width, height);
        // Cannot overflow: each chroma dimension is at most its luma dimension, and `luma_len` fit.
        let chroma_len = chroma_w as usize * chroma_h as usize;

        let mut y = vec![0u16; luma_len];
        let (mut cb, mut cr) = if chroma == ChromaFormat::Monochrome {
            (Vec::new(), Vec::new())
        } else {
            (vec![0u16; chroma_len], vec![0u16; chroma_len])
        };

        let mut planes: [*mut u8; MAX_PLANES] = [core::ptr::null_mut(); MAX_PLANES];
        let mut strides = [0usize; MAX_PLANES];
        planes[0] = y.as_mut_ptr().cast::<u8>();
        strides[0] = width as usize * core::mem::size_of::<u16>();
        let plane_count = if chroma == ChromaFormat::Monochrome {
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

        let extradata = Self::extradata(config);
        let cfg = self.stream_config(config, &extradata);
        let status = self.backend.decode(&cfg, payload, &out);
        if status.is_unsupported() {
            return Err(Error::unsupported(env!("CARGO_PKG_NAME"), BACKEND_DECLINED)
                .with_detail(format!("codec-abi status {}", status.0)));
        }
        if !status.is_ok() {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "HEIF: HEVC backend returned a failure status",
            )
            .with_detail(format!("codec-abi status {}", status.0)));
        }
        DecodedFrame::new(width, height, bit_depth, chroma, y, cb, cr)
    }
}
