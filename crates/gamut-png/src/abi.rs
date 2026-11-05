//! Adapters that plug a [`gamut_codec_abi`] backend into the PNG IDAT seam, in both directions.
//!
//! [`ZlibInflater`] / [`ZlibDeflater`] are PNG-local traits (see the `backend` module for why).
//! [`AbiZlibInflater`] and [`AbiZlibDeflater`] wrap a generic [`gamut_codec_abi::Decoder`] /
//! [`gamut_codec_abi::Encoder`] — which is what a C or `-sys` backend reaches through
//! [`gamut_codec_abi::bridge`] — so a foreign zlib/DEFLATE implementation plugs in without either
//! side knowing about the other.
//!
//! # Mapping
//!
//! | PNG seam | codec-abi |
//! |---|---|
//! | [`IdatInfo`] | [`StreamConfig`] / [`EncodeConfig`] with [`ZLIB_CODEC_ID`] |
//! | the filtered scanline stream | a single-plane [`ImageDesc`] (see below) |
//! | `supports() == false` | `supports()` → `false` |
//! | a failure after accepting | any non-[`Status::OK`] status from the call |
//!
//! `supports` is the only fall-through. A non-OK status from the call itself is a terminal error
//! the host **propagates** rather than falling through to the next backend: an
//! [`ErrorKind::Unsupported`] one for [`Status::UNSUPPORTED`], an [`ErrorKind::InvalidInput`] one
//! for every other status.
//!
//! [`ErrorKind::Unsupported`]: gamut_core::ErrorKind::Unsupported
//! [`ErrorKind::InvalidInput`]: gamut_core::ErrorKind::InvalidInput
//!
//! # The [`ImageDesc`] convention for this seam
//!
//! The datum crossing the PNG seam is a flat byte stream, not a pixel raster: filtering and
//! sub-byte packing stay crate-side. So the descriptor carries the *whole* filtered stream as a
//! **single plane of one row** — `plane_count = 1`, `strides[0] = raw_len` — because Adam7 makes
//! per-row lengths non-uniform and a backend has no use for them anyway. `width` / `height` /
//! `bit_depth` are still filled from IHDR so a backend can size its work, and `pixel_format` is
//! [`PIXEL_FORMAT_FILTERED_BYTES`].
//!
//! A decoder backend must fill exactly [`IdatInfo::raw_len`] bytes — PNG always knows the inflated
//! size ahead of time — and the host still re-checks that against its cap.

use gamut_codec_abi::{
    Decoder, EncodeConfig, Encoder, ImageDesc, MAX_PLANES, Status, StreamConfig, codec_id,
    pixel_format,
};
use gamut_core::{Error, Result};

use crate::backend::{IdatInfo, ZlibDeflater, ZlibInflater};

/// The codec id this seam agrees on: the FourCC `"zlib"`, big-endian.
///
/// A PNG codestream is a zlib stream (RFC 1950 over DEFLATE), so both the decode and encode
/// descriptors carry this id; a backend matches on it to recognise the job.
pub const ZLIB_CODEC_ID: u32 = codec_id(*b"zlib");

/// The [`ImageDesc::pixel_format`] tag used across this seam: the namespace's
/// [`OPAQUE_BYTES`](pixel_format::OPAQUE_BYTES) (`u32::MAX`).
///
/// Deliberately **not** a pixel layout: the bytes are the filtered scanline stream (filter byte +
/// packed samples per row), not pixels in any layout, and their meaning is [`ZLIB_CODEC_ID`]'s.
pub const PIXEL_FORMAT_FILTERED_BYTES: u32 = pixel_format::OPAQUE_BYTES;

/// The error a non-OK, non-`UNSUPPORTED` status becomes. Terminal: the host propagates it.
fn backend_failed() -> Error {
    Error::invalid_input(
        env!("CARGO_PKG_NAME"),
        "PNG: IDAT codec-abi backend reported an error",
    )
}

/// Builds the single-plane descriptor for a `len`-byte filtered stream at `ptr`.
fn desc(info: &IdatInfo, ptr: *mut u8, len: usize) -> ImageDesc {
    let mut planes = [std::ptr::null_mut(); MAX_PLANES];
    let mut strides = [0usize; MAX_PLANES];
    planes[0] = ptr;
    strides[0] = len;
    ImageDesc::new(
        PIXEL_FORMAT_FILTERED_BYTES,
        info.width(),
        info.height(),
        u32::from(info.bit_depth()),
        1,
        planes,
        strides,
    )
}

/// Turns a non-OK status from an accepted job into its terminal typed error.
fn from_status(status: Status) -> Error {
    if status.is_unsupported() {
        Error::unsupported(
            env!("CARGO_PKG_NAME"),
            "PNG: IDAT codec-abi backend declined the stream",
        )
        .with_detail(format!("codec-abi status {}", status.0))
    } else {
        backend_failed().with_detail(format!("codec-abi status {}", status.0))
    }
}

/// Adapts a [`gamut_codec_abi::Decoder`] into a [`ZlibInflater`].
///
/// To be pushed onto a [`PngDecoder`](crate::PngDecoder), `D` must also be [`Send`]: the decoder
/// holds its registry behind `Arc<Mutex<…>>`. For a foreign C backend that means constructing
/// [`gamut_codec_abi::bridge::ForeignDecoder`] through its `unsafe` constructor, by which the
/// caller asserts thread-safety.
pub struct AbiZlibInflater<D> {
    decoder: D,
}

impl<D> AbiZlibInflater<D> {
    /// The wrapped codec-abi decoder.
    #[must_use]
    pub fn get_ref(&self) -> &D {
        &self.decoder
    }

    /// The wrapped codec-abi decoder, mutably.
    #[must_use]
    pub fn get_mut(&mut self) -> &mut D {
        &mut self.decoder
    }

    /// Consumes the adapter, returning the wrapped codec-abi decoder.
    #[must_use]
    pub fn into_inner(self) -> D {
        self.decoder
    }
}

impl<D: Decoder> AbiZlibInflater<D> {
    /// Wraps a codec-abi decoder as a PNG IDAT inflater.
    #[must_use]
    pub fn new(decoder: D) -> Self {
        Self { decoder }
    }

    /// The [`StreamConfig`] this adapter presents for `info` — the descriptor a backend's
    /// `supports` sees. Exposed so a caller can predicate its own backend on the same values.
    #[must_use]
    pub fn config(info: &IdatInfo) -> StreamConfig {
        StreamConfig::new(
            ZLIB_CODEC_ID,
            info.width(),
            info.height(),
            u32::from(info.bit_depth()),
        )
    }
}

impl<D: Decoder> ZlibInflater for AbiZlibInflater<D> {
    fn supports(&mut self, info: &IdatInfo) -> bool {
        self.decoder.supports(&Self::config(info))
    }

    fn inflate(&mut self, info: &IdatInfo, zlib: &[u8], max_out: usize) -> Result<Vec<u8>> {
        // The inflated size is known from IHDR. Refuse to *allocate* past the host's cap here as
        // well — the host's post-return re-check is the guarantee, this is just not wasting memory
        // on a job that is already doomed.
        let len = info.raw_len();
        if len > max_out {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "PNG: IDAT is larger than the decoder's output budget",
            ));
        }
        let mut out = vec![0u8; len];
        let image = desc(info, out.as_mut_ptr(), len);
        let status = self.decoder.decode(&Self::config(info), zlib, &image);
        if status.is_ok() {
            Ok(out)
        } else {
            Err(from_status(status))
        }
    }
}

/// Adapts a [`gamut_codec_abi::Encoder`] into a [`ZlibDeflater`].
///
/// The encode job carries a `quality` knob that has no PNG meaning (DEFLATE is lossless), so it
/// defaults to `100` and is settable via [`with_quality`](Self::with_quality) for backends that
/// read it as an effort level.
///
/// To be pushed onto a [`PngEncoder`](crate::PngEncoder), `E` must also be [`Send`], as for
/// [`AbiZlibInflater`].
pub struct AbiZlibDeflater<E> {
    encoder: E,
    quality: u32,
}

impl<E> AbiZlibDeflater<E> {
    /// The wrapped codec-abi encoder.
    #[must_use]
    pub fn get_ref(&self) -> &E {
        &self.encoder
    }

    /// The wrapped codec-abi encoder, mutably.
    #[must_use]
    pub fn get_mut(&mut self) -> &mut E {
        &mut self.encoder
    }

    /// Consumes the adapter, returning the wrapped codec-abi encoder.
    #[must_use]
    pub fn into_inner(self) -> E {
        self.encoder
    }
}

impl<E: Encoder> AbiZlibDeflater<E> {
    /// Wraps a codec-abi encoder as a PNG IDAT deflater, at the default quality/effort of `100`.
    #[must_use]
    pub fn new(encoder: E) -> Self {
        Self {
            encoder,
            quality: 100,
        }
    }

    /// Sets the [`EncodeConfig::quality`] handed to the backend — an *effort* knob here, since
    /// DEFLATE is lossless and the decoded pixels are identical either way.
    #[must_use]
    pub fn with_quality(mut self, quality: u32) -> Self {
        self.quality = quality;
        self
    }

    /// The [`EncodeConfig`] this adapter presents.
    #[must_use]
    pub fn config(&self) -> EncodeConfig {
        EncodeConfig::new(ZLIB_CODEC_ID, self.quality)
    }
}

impl<E: Encoder> ZlibDeflater for AbiZlibDeflater<E> {
    fn supports(&mut self, _info: &IdatInfo) -> bool {
        let cfg = self.config();
        self.encoder.supports(&cfg)
    }

    fn deflate(&mut self, info: &IdatInfo, raw: &[u8]) -> Result<Vec<u8>> {
        let cfg = self.config();
        // A cast to `*mut` is safe; the backend only reads through it (encode input).
        let image = desc(info, raw.as_ptr().cast_mut(), raw.len());
        let mut zlib = Vec::new();
        let status = self.encoder.encode(&cfg, &image, &mut |chunk: &[u8]| {
            zlib.extend_from_slice(chunk);
            Status::OK
        });
        if status.is_ok() {
            Ok(zlib)
        } else {
            Err(from_status(status))
        }
    }
}
