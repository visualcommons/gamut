//! The public WebP decoder: parses the RIFF container and routes to the VP8/VP8L bitstream decoder.
//!
//! Container parsing and format routing are implemented (via [`gamut_riff`]). The lossless **VP8L**
//! and lossy **VP8** bitstreams are decoded natively; an extended **VP8X** file is parsed and its
//! inner bitstream decoded. Decoding to [`Rgba8`](gamut_core::Rgba8) applies a lossy file's `ALPH`
//! alpha chunk and preserves a VP8L stream's own alpha.
//!
//! Every typed decode runs one container traversal producing the file's *native* layout, then hands
//! it to [`gamut_core::convert`] — so WebP applies the same widening and narrowing rules as every
//! other gamut decoder. Decoding a genuinely transparent file to [`Rgb8`](gamut_core::Rgb8) would
//! discard the alpha channel, so it is refused unless
//! [`WebpDecoder::convert_policy`] permits it.
//!
//! Every decode is bounded by caller-set resource limits
//! ([`with_max_dimensions`](WebpDecoder::with_max_dimensions),
//! [`with_max_image_bytes`](WebpDecoder::with_max_image_bytes)), checked against the bitstream's
//! own header before any frame-sized allocation and before any backend is offered the job.

use std::fmt;
use std::sync::{Arc, Mutex};

use gamut_color::{ChromaUpsampling, ColorRange, Yuv420};
use gamut_core::convert::{ConvertPolicy, RawImage, convert_from_raw};
use gamut_core::{
    DecodeImage, Dimensions, Error, ImageBuf, Pixel, PixelFormat, Result, Rgb8, Rgba8,
};
use gamut_riff::{WebpChunkId, WebpLayout};

use crate::alpha;
use crate::backend::{
    CodestreamInfo, DecodedRaster, SharedDecoder, WebpCodestream, WebpCodestreamDecoder,
    dispatch_decode, peek_dimensions,
};
use crate::vp8l::decoder::{argb_to_rgb8, argb_to_rgba8, decode as decode_vp8l};

/// Default cap on the decoded frame: 64 MiB, a 4096×4096 frame at four bytes per pixel — the same
/// budget, and the same image, as `gamut-png`'s `PngDecoder` default.
const DEFAULT_MAX_IMAGE_BYTES: usize = 64 << 20;

/// The format's own dimension bound: VP8L codes `width - 1` and `height - 1` in 14 bits each
/// (RFC 9649 §3.2), so no WebP bitstream declares a side above 16384; a VP8 key frame's 14-bit
/// fields stop one short of it (RFC 6386 §9.1).
const FORMAT_MAX_DIMENSION: u32 = 1 << 14;

/// Bytes the budget charges per pixel: the 8-bit RGBA frame, the widest layout a WebP decodes to and
/// the size of the VP8L decoder's own ARGB working buffer.
const BUDGET_BYTES_PER_PIXEL: u64 = 4;

/// Decodes a WebP file to interleaved 8-bit RGB.
///
/// gamut ships its own decoder because every WebP decoder in the Rust ecosystem ultimately wraps
/// libwebp; a `#![forbid(unsafe_code)]` decoder removes that crate's memory-unsafety exposure.
/// The codestream itself may be decoded by a pluggable backend installed with
/// [`push_backend`](Self::push_backend) — a hardware VP8 decoder, say; with none installed (the
/// default) the crate's own `vp8`/`vp8l` decoders run. See [`crate::backend`] for the fallback
/// contract.
///
/// # Resource limits
///
/// A WebP's few header bytes can declare a 16384×16384 frame — 1 GiB of RGBA — so the decoder
/// bounds what it will decode. The defaults accept every dimension the format allows but cap the
/// decoded frame at 64 MiB ([`with_max_image_bytes`](Self::with_max_image_bytes)); tighten the
/// dimension caps with [`with_max_dimensions`](Self::with_max_dimensions) when the application
/// knows its domain. Both are checked against the `VP8 ` key-frame or `VP8L` header before any
/// frame-sized allocation, and before any [backend](Self::push_backend) is offered the job; a file
/// over either limit is refused with [`Error::Unsupported`].
#[derive(Clone)]
pub struct WebpDecoder {
    /// Pluggable codestream decoders, tried in push order ahead of the built-in tails.
    backends: Vec<SharedDecoder>,
    /// Which lossy layout conversions a typed decode may perform.
    policy: ConvertPolicy,
    /// The widest frame accepted, in pixels.
    max_width: u32,
    /// The tallest frame accepted, in pixels.
    max_height: u32,
    /// The largest decoded frame accepted, in bytes at four bytes per pixel.
    max_image_bytes: usize,
}

impl Default for WebpDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for WebpDecoder {
    /// Renders the number of installed backends (a backend need not be `Debug`) and the limits.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WebpDecoder")
            .field("backends", &self.backends.len())
            .field("max_width", &self.max_width)
            .field("max_height", &self.max_height)
            .field("max_image_bytes", &self.max_image_bytes)
            .finish_non_exhaustive()
    }
}

impl WebpDecoder {
    /// Creates a decoder with the default configuration: no backends, the lossless
    /// [`ConvertPolicy`], format-maximum dimensions and a 64 MiB decoded-frame budget.
    #[must_use]
    pub fn new() -> Self {
        Self {
            backends: Vec::new(),
            policy: ConvertPolicy::default(),
            max_width: FORMAT_MAX_DIMENSION,
            max_height: FORMAT_MAX_DIMENSION,
            max_image_bytes: DEFAULT_MAX_IMAGE_BYTES,
        }
    }

    /// Caps the accepted frame dimensions; a wider or taller frame is refused with
    /// [`Error::Unsupported`] before any allocation. The default is the format's own bound, 16384
    /// on each side.
    #[must_use]
    pub fn with_max_dimensions(mut self, width: u32, height: u32) -> Self {
        self.max_width = width;
        self.max_height = height;
        self
    }

    /// Caps the decoded frame in bytes (default 64 MiB), refusing a larger one with
    /// [`Error::Unsupported`] before any allocation.
    ///
    /// The frame is charged at **four bytes per pixel** (`width × height × 4`, the 8-bit RGBA
    /// frame) whatever the file turns out to hold: whether a lossless image is opaque is known
    /// only after it is decoded, and the limit has to be decided from the header alone. So the
    /// budget a caller sets admits the same frames whether they are later presented as
    /// [`Rgb8`] or [`Rgba8`].
    #[must_use]
    pub fn with_max_image_bytes(mut self, bytes: usize) -> Self {
        self.max_image_bytes = bytes;
        self
    }

    /// Enforces the dimension and byte-budget limits against the frame a codestream header declares.
    fn check_limits(&self, dims: Dimensions) -> Result<()> {
        if dims.width > self.max_width || dims.height > self.max_height {
            return Err(Error::unsupported(
                env!("CARGO_PKG_NAME"),
                "WebP: image exceeds the dimension limit",
            ));
        }
        // Two `u32` sides times four cannot overflow `u64`, and `usize` widens into `u64` losslessly
        // on every target Rust supports, so the comparison is exact.
        let bytes = u64::from(dims.width) * u64::from(dims.height) * BUDGET_BYTES_PER_PIXEL;
        if bytes > self.max_image_bytes as u64 {
            return Err(Error::unsupported(
                env!("CARGO_PKG_NAME"),
                "WebP: image exceeds the size limit",
            ));
        }
        Ok(())
    }

    /// Installs a codestream decoder backend, returning `&mut self` so pushes chain.
    ///
    /// Backends are tried in **push order**, ahead of the built-in `vp8`/`vp8l` decoders, which
    /// remain the implicit tails and cannot be removed. A backend declines a job by returning
    /// `false` from [`supports`](WebpCodestreamDecoder::supports); once it accepts, its error
    /// propagates and no other decoder is tried.
    ///
    /// **Cloning a `WebpDecoder` shares its backends**: the registry holds each backend behind an
    /// [`Arc`], so a clone dispatches to the very same backend objects (and the same interior
    /// state), it does not copy them.
    pub fn push_backend(&mut self, backend: impl WebpCodestreamDecoder + 'static) -> &mut Self {
        self.backends.push(Arc::new(Mutex::new(backend)));
        self
    }

    /// Decodes one codestream `payload` through the registry, falling back to the built-in decoder
    /// when every backend declines (or when the codestream header cannot even be peeked, in which
    /// case the built-in decoder reports the parse error).
    ///
    /// The resource limits are enforced here, on the peeked header, ahead of every decoder. A
    /// header that cannot be peeked is one both built-in decoders reject while reading it —
    /// [`peek_dimensions`] runs their own header readers — so it never reaches a frame-sized
    /// allocation unchecked.
    fn decode_codestream(
        &self,
        codestream: WebpCodestream,
        payload: &[u8],
    ) -> Result<DecodedRaster> {
        let peeked = peek_dimensions(codestream, payload);
        if let Some(dims) = peeked {
            self.check_limits(dims)?;
        }
        if !self.backends.is_empty()
            && let Some(dims) = peeked
            && let Some(result) = dispatch_decode(
                &self.backends,
                &CodestreamInfo::new(codestream, dims),
                payload,
            )
        {
            return result;
        }
        match codestream {
            WebpCodestream::Vp8 => Ok(DecodedRaster::Yuv420(
                crate::vp8::frame::decode_frame(payload)?.to_yuv420(),
            )),
            WebpCodestream::Vp8l => {
                let (dimensions, pixels) = decode_vp8l(payload)?;
                Ok(DecodedRaster::Argb { dimensions, pixels })
            }
        }
    }

    /// Unwraps a lossless decode result, which is ARGB by construction (the registry rejects a
    /// backend that returns the other variant).
    fn expect_argb(raster: DecodedRaster) -> Result<(Dimensions, Vec<u32>)> {
        match raster {
            DecodedRaster::Argb { dimensions, pixels } => Ok((dimensions, pixels)),
            DecodedRaster::Yuv420(_) => Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "WebP: VP8L decode produced a YUV raster",
            )),
        }
    }

    /// Unwraps a lossy decode result, which is YUV 4:2:0 by construction.
    fn expect_yuv(raster: DecodedRaster) -> Result<Yuv420> {
        match raster {
            DecodedRaster::Yuv420(yuv) => Ok(yuv),
            DecodedRaster::Argb { .. } => Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "WebP: VP8 decode produced an ARGB raster",
            )),
        }
    }

    /// Selects which lossy conversions a typed decode may perform.
    ///
    /// Defaults to [`ConvertPolicy::lossless`], under which a file that genuinely carries
    /// transparency cannot be decoded as [`Rgb8`]. A file whose alpha is entirely opaque still
    /// decodes as [`Rgb8`], because discarding an all-opaque alpha channel loses nothing.
    #[must_use]
    pub fn convert_policy(mut self, policy: ConvertPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Sorts the container's chunks and locates the image bitstream.
    ///
    /// [`WebpLayout::parse`] is the single container walk behind the pixel path: it validates the
    /// `VP8X` header, enforces the spec's reconstruction-chunk order, and hands back the `ALPH` and
    /// bitstream payloads (RFC 9649 §2.7).
    fn layout(data: &[u8]) -> Result<(WebpLayout<'_>, WebpCodestream, &[u8])> {
        let layout = WebpLayout::parse(data)?;
        let (id, payload) = layout.bitstream.ok_or_else(|| {
            Error::invalid_input(env!("CARGO_PKG_NAME"), "WebP: no VP8/VP8L bitstream chunk")
        })?;
        let codestream = match id {
            WebpChunkId::Vp8l => WebpCodestream::Vp8l,
            _ => WebpCodestream::Vp8,
        };
        Ok((layout, codestream, payload))
    }

    /// Decodes the WebP file in `data` to the interleaved 8-bit layout it natively carries,
    /// returning the samples with the [`PixelFormat`] describing them.
    ///
    /// One decode serves every typed request: a lossy `VP8` bitstream becomes RGB, gaining an alpha
    /// channel only when an `ALPH` chunk supplies one, and a lossless `VP8L` bitstream becomes RGBA
    /// unless every pixel is opaque. Reporting `Rgb8` for a fully opaque image is what keeps the
    /// common "decode this WebP as RGB" request lossless rather than making it depend on a policy.
    fn decode_native(&self, data: &[u8]) -> Result<(Dimensions, PixelFormat, Vec<u8>)> {
        let (layout, codestream, payload) = Self::layout(data)?;
        match codestream {
            WebpCodestream::Vp8l => {
                let (dims, argb) =
                    Self::expect_argb(self.decode_codestream(WebpCodestream::Vp8l, payload)?)?;
                let mut out = Vec::new();
                // A VP8L stream always codes an alpha channel, but most images are fully opaque;
                // reporting those as RGB keeps an `Rgb8` request lossless.
                let format = if argb.iter().all(|&p| (p >> 24) as u8 == 0xff) {
                    argb_to_rgb8(&argb, &mut out);
                    PixelFormat::Rgb8
                } else {
                    argb_to_rgba8(&argb, &mut out);
                    PixelFormat::Rgba8
                };
                Ok((dims, format, out))
            }
            WebpCodestream::Vp8 => {
                let yuv = Self::expect_yuv(self.decode_codestream(WebpCodestream::Vp8, payload)?)?;
                let dims = Dimensions {
                    width: yuv.width(),
                    height: yuv.height(),
                };
                // WebP/VP8 is limited-range BT.601; decode with the matching inverse.
                let rgb = yuv.to_rgb8(ColorRange::Limited, ChromaUpsampling::Nearest);
                // A lossy bitstream carries no alpha of its own: it exists only when the extended
                // container supplies an `ALPH` chunk.
                let Some(payload) = layout.alph else {
                    return Ok((dims, PixelFormat::Rgb8, rgb));
                };
                let (w, h) = (dims.width as usize, dims.height as usize);
                let alpha = alpha::read_alph(payload, w, h)?;
                let mut out = Vec::with_capacity(w * h * 4);
                for (px, &a) in rgb.as_chunks::<3>().0.iter().zip(alpha.iter()) {
                    out.extend_from_slice(&[px[0], px[1], px[2], a]);
                }
                Ok((dims, PixelFormat::Rgba8, out))
            }
        }
    }

    /// Decodes `data` and presents it as pixel layout `P`.
    fn present<P: Pixel<Sample = u8>>(&self, data: &[u8]) -> Result<ImageBuf<P>> {
        let (dims, format, pixels) = self.decode_native(data)?;
        convert_from_raw(RawImage::new(&pixels, format, dims)?, self.policy)
    }
}

impl DecodeImage<Rgb8> for WebpDecoder {
    /// An opaque file decodes straight to RGB. One that genuinely carries transparency needs an
    /// [`AlphaPolicy`](gamut_core::convert::AlphaPolicy) -- see [`WebpDecoder::convert_policy`].
    fn decode_image(&self, data: &[u8]) -> Result<ImageBuf<Rgb8>> {
        self.present(data)
    }
}

impl DecodeImage<Rgba8> for WebpDecoder {
    /// A file without transparency gains an opaque alpha channel; one with an `ALPH` chunk or a
    /// VP8L alpha channel keeps it.
    fn decode_image(&self, data: &[u8]) -> Result<ImageBuf<Rgba8>> {
        self.present(data)
    }
}

#[cfg(test)]
mod tests {
    use gamut_riff::{FourCc, RiffReader, RiffWriter, write_simple_lossless, write_simple_lossy};

    use super::*;
    use crate::vp8l::bit_io::BitWriter;
    use crate::vp8l::header::Vp8lHeader;
    use crate::vp8l::prefix::write_simple_prefix_code;

    /// Builds a simple-lossless WebP file holding a solid-color `width`×`height` VP8L image.
    fn solid_lossless_webp(width: u32, height: u32, r: u8, g: u8, b: u8) -> Vec<u8> {
        let mut w = BitWriter::new();
        Vp8lHeader::from_dimensions(Dimensions { width, height }, false)
            .unwrap()
            .write(&mut w);
        w.write_bits(0, 1); // no transforms
        w.write_bits(0, 1); // no color cache
        w.write_bits(0, 1); // single meta prefix code
        write_simple_prefix_code(&mut w, &[u16::from(g)]);
        write_simple_prefix_code(&mut w, &[u16::from(r)]);
        write_simple_prefix_code(&mut w, &[u16::from(b)]);
        write_simple_prefix_code(&mut w, &[0xff]); // alpha (opaque)
        write_simple_prefix_code(&mut w, &[0]); // distance (unused)
        write_simple_lossless(&w.finish()).unwrap()
    }

    #[test]
    fn debug_renders_the_backend_count_and_the_limits() {
        let decoder = WebpDecoder::new()
            .with_max_dimensions(3, 5)
            .with_max_image_bytes(7);
        assert_eq!(
            format!("{decoder:?}"),
            "WebpDecoder { backends: 0, max_width: 3, max_height: 5, max_image_bytes: 7, .. }"
        );
    }

    #[test]
    fn decodes_lossless_container_to_rgb8() {
        let file = solid_lossless_webp(2, 2, 0x12, 0x34, 0x56);
        let got: ImageBuf<Rgb8> = WebpDecoder::new().decode_image(&file).unwrap();
        assert_eq!(
            got.dimensions(),
            Dimensions {
                width: 2,
                height: 2
            }
        );
        assert_eq!(got.as_samples(), [0x12, 0x34, 0x56].repeat(4).as_slice());
    }

    #[test]
    fn routes_lossy_container_to_vp8() {
        // A `VP8 ` chunk reaches the VP8 decoder, which rejects this malformed (non-key-frame, 3-byte)
        // payload rather than panicking.
        let file = write_simple_lossy(&[0x9d, 0x01, 0x2a]).unwrap();
        let got: Result<ImageBuf<Rgb8>> = WebpDecoder::new().decode_image(&file);
        assert!(got.is_err());
    }

    #[test]
    fn decodes_extended_container_with_inner_bitstream() {
        use gamut_riff::{Vp8xHeader, write_extended};
        // A VP8X feature header followed by a VP8L bitstream decodes to the inner image (the alpha
        // flag's `ALPH` chunk is handled in a later milestone).
        let inner = solid_lossless_webp(2, 2, 0x11, 0x22, 0x33);
        let vp8l = RiffReader::new(&inner)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .payload
            .to_vec();
        let header = Vp8xHeader {
            canvas_width: 2,
            canvas_height: 2,
            ..Default::default()
        };
        let file = write_extended(&header, &[(FourCc::VP8L, &vp8l)]).unwrap();
        let got: ImageBuf<Rgb8> = WebpDecoder::new()
            .decode_image(&file)
            .expect("decode VP8X file");
        assert_eq!(
            got.dimensions(),
            Dimensions {
                width: 2,
                height: 2
            }
        );
        assert_eq!(got.as_samples(), [0x11, 0x22, 0x33].repeat(4).as_slice());
    }

    #[test]
    fn rejects_extended_container_without_bitstream() {
        // A VP8X header with no following bitstream chunk has nothing to decode.
        let header = gamut_riff::Vp8xHeader {
            canvas_width: 4,
            canvas_height: 4,
            ..Default::default()
        };
        let file = gamut_riff::write_extended(&header, &[]).unwrap();
        let got: Result<ImageBuf<Rgb8>> = WebpDecoder::new().decode_image(&file);
        assert!(matches!(
            got,
            Err(error) if error.kind() == gamut_core::ErrorKind::InvalidInput
        ));
    }

    #[test]
    fn skips_leading_metadata_then_decodes_bitstream() {
        // A leading metadata chunk must be skipped; the VP8L chunk that follows is decoded.
        let vp8l = {
            let full = solid_lossless_webp(1, 1, 9, 8, 7);
            // Extract just the VP8L chunk payload from the simple-lossless file.
            RiffReader::new(&full)
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .payload
                .to_vec()
        };
        let mut w = RiffWriter::new();
        w.write_chunk(FourCc::ICCP, &[1, 2, 3, 4]).unwrap();
        w.write_chunk(FourCc::VP8L, &vp8l).unwrap();
        let file = w.finish().unwrap();
        let got: ImageBuf<Rgb8> = WebpDecoder::new().decode_image(&file).unwrap();
        assert_eq!(
            got.dimensions(),
            Dimensions {
                width: 1,
                height: 1
            }
        );
        assert_eq!(got.as_samples(), [9, 8, 7].as_slice());
    }

    #[test]
    fn errors_when_no_bitstream_chunk() {
        let mut w = RiffWriter::new();
        w.write_chunk(FourCc::EXIF, &[0xee; 6]).unwrap();
        let file = w.finish().unwrap();
        let err: Result<ImageBuf<Rgb8>> = WebpDecoder::new().decode_image(&file);
        assert!(matches!(
            err,
            Err(error) if error.kind() == gamut_core::ErrorKind::InvalidInput
        ));
    }

    #[test]
    fn rejects_non_riff_data() {
        let err: Result<ImageBuf<Rgb8>> = WebpDecoder::new().decode_image(b"not a webp");
        assert!(matches!(
            err,
            Err(error) if error.kind() == gamut_core::ErrorKind::InvalidInput
        ));
    }

    #[test]
    fn decodes_lossless_container_to_rgba8() {
        // A VP8L file decoded to RGBA carries the stream's own alpha (opaque here). Pins the VP8L arm
        // of the RGBA decoder, which deleting would route to "no bitstream".
        let file = solid_lossless_webp(2, 2, 0x12, 0x34, 0x56);
        let got: ImageBuf<Rgba8> = WebpDecoder::new().decode_image(&file).unwrap();
        assert_eq!(
            got.dimensions(),
            Dimensions {
                width: 2,
                height: 2
            }
        );
        assert_eq!(
            got.as_samples(),
            [0x12, 0x34, 0x56, 0xff].repeat(4).as_slice()
        );
    }

    #[test]
    fn rejects_malformed_vp8x_header() {
        // A VP8X chunk with a too-short payload (a valid one is 10 bytes) is malformed: the decoder
        // must parse-and-reject it, not silently skip to the inner bitstream. Pins the VP8X arm of
        // both the RGB and RGBA paths.
        let inner = solid_lossless_webp(2, 2, 1, 2, 3);
        let vp8l = RiffReader::new(&inner)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .payload
            .to_vec();
        let mut w = RiffWriter::new();
        w.write_chunk(FourCc::VP8X, &[0u8; 4]).unwrap();
        w.write_chunk(FourCc::VP8L, &vp8l).unwrap();
        let file = w.finish().unwrap();
        let rgb: Result<ImageBuf<Rgb8>> = WebpDecoder::new().decode_image(&file);
        assert!(
            rgb.is_err(),
            "RGB decode must reject a malformed VP8X header"
        );
        let rgba: Result<ImageBuf<Rgba8>> = WebpDecoder::new().decode_image(&file);
        assert!(
            rgba.is_err(),
            "RGBA decode must reject a malformed VP8X header"
        );
    }
}
