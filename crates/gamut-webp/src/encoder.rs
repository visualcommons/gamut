//! The public WebP encoder: orchestrates color handling, the VP8/VP8L bitstream, and the RIFF
//! container, mirroring the shape of [`gamut_avif::AvifEncoder`](https://docs.rs/gamut-avif).
//!
//! Both the lossless **VP8L** path (see [`crate::vp8l::encoder`]) and the lossy **VP8** path are
//! implemented, via the [`EncodeImage<Rgb8>`](gamut_core::EncodeImage) and `EncodeImage<Rgba8>`
//! impls; transparent lossy images use the extended (`VP8X`) format with a raw `ALPH` alpha chunk,
//! as does any image carrying embedded metadata.

use std::fmt;
use std::sync::{Arc, Mutex};

use gamut_color::{ColorRange, Yuv420};
use gamut_core::{Dimensions, EncodeImage, ImageRef, Result, Rgb8, Rgba8};
use gamut_riff::{
    Chunk, FourCc, MetadataChunks, Vp8xHeader, write_extended_preserving, write_simple_lossless,
    write_simple_lossy,
};

use crate::alpha;
use crate::backend::{
    RasterRef, SharedEncoder, WebpCodestream, WebpCodestreamEncoder, WebpEncodeRequest,
    dispatch_encode,
};
use crate::config::{Effort, NearLossless, WebpConfig, WebpMode};
use crate::vp8::frame::{EncodeOptions, encode_frame_filtered};
use crate::vp8l::encoder::encode as encode_vp8l;
use crate::vp8l::near_lossless;
use crate::vp8l::transform::make_argb;

/// Maps a `0..=100` quality to a VP8 base quantizer index (`0..=127`); higher quality → lower index
/// (less quantization). This is the keystone's simple mapping; finer rate control is issue #32.
fn quality_to_quant(quality: u8) -> u8 {
    let q = u32::from(quality.min(100));
    ((100 - q) * 127 / 100) as u8
}

/// Encodes 8-bit RGB images to WebP.
///
/// Construct with [`WebpEncoder::new`] (lossless), [`WebpEncoder::lossless`], or
/// [`WebpEncoder::lossy`], then encode via the [`EncodeImage`](gamut_core::EncodeImage) trait.
///
/// Embedded metadata is attached with [`with_exif`](Self::with_exif) / [`with_xmp`](Self::with_xmp)
/// / [`with_icc_profile`](Self::with_icc_profile), which promote the output to the extended (`VP8X`)
/// format automatically.
///
/// The codestream itself may be produced by a pluggable backend installed with
/// [`push_backend`](Self::push_backend); with none installed (the default) the crate's own
/// `vp8`/`vp8l` encoders produce byte-identical output to before the seam existed. See
/// [`crate::backend`] for the fallback contract.
#[derive(Clone, Default)]
pub struct WebpEncoder {
    /// Encoder configuration (mode + quality).
    config: WebpConfig,
    /// The `EXIF` chunk payload to embed, verbatim.
    exif: Option<Vec<u8>>,
    /// The `XMP ` chunk payload to embed, verbatim.
    xmp: Option<Vec<u8>>,
    /// The `ICCP` chunk payload (ICC colour profile) to embed, verbatim.
    icc: Option<Vec<u8>>,
    /// Unknown chunks to re-emit after the metadata, in the order given (RFC 9649 §2.7.1.6).
    unknown: Vec<(FourCc, Vec<u8>)>,
    /// Pluggable codestream encoders, tried in push order ahead of the built-in tails.
    backends: Vec<SharedEncoder>,
}

impl fmt::Debug for WebpEncoder {
    /// Renders the config plus the metadata payloads' byte lengths and the number of installed
    /// backends (a backend need not be `Debug`).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WebpEncoder")
            .field("config", &self.config)
            .field("exif", &self.exif.as_ref().map(Vec::len))
            .field("xmp", &self.xmp.as_ref().map(Vec::len))
            .field("icc", &self.icc.as_ref().map(Vec::len))
            .field("backends", &self.backends.len())
            .finish()
    }
}

impl WebpEncoder {
    /// Creates an encoder with the default configuration (lossless VP8L).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates an encoder that produces a lossless VP8L bitstream (the default mode).
    #[must_use]
    pub fn lossless() -> Self {
        Self::default()
    }

    /// Creates an encoder that produces a lossy VP8 bitstream at the given `quality` (`0..=100`).
    #[must_use]
    pub fn lossy(quality: u8) -> Self {
        Self {
            config: WebpConfig {
                mode: WebpMode::Lossy,
                quality,
                ..WebpConfig::default()
            },
            ..Self::default()
        }
    }

    /// Embeds Exif metadata as an `EXIF` chunk (RFC 9649 §2.7.3), promoting the output to the
    /// extended (`VP8X`) format and setting the Exif feature flag.
    ///
    /// `exif` is stored **verbatim**: a WebP `EXIF` chunk carries the bare payload — unlike a JPEG
    /// APP1 segment, there is no `"Exif\0\0"` signature to add or strip — so [`crate::metadata`]
    /// reads back exactly these bytes. Calling this twice keeps the last payload.
    #[must_use]
    pub fn with_exif(mut self, exif: &[u8]) -> Self {
        self.exif = Some(exif.to_vec());
        self
    }

    /// Embeds an XMP packet as an `XMP ` chunk (RFC 9649 §2.7.3), promoting the output to the
    /// extended (`VP8X`) format and setting the XMP feature flag.
    ///
    /// Takes bytes rather than `&str` because a packet may open with a BOM. The payload is stored
    /// verbatim, so [`crate::metadata`] reads back exactly these bytes. Calling this twice keeps the
    /// last payload.
    #[must_use]
    pub fn with_xmp(mut self, xmp: &[u8]) -> Self {
        self.xmp = Some(xmp.to_vec());
        self
    }

    /// Embeds an ICC colour profile as an `ICCP` chunk (RFC 9649 §2.7.2), promoting the output to
    /// the extended (`VP8X`) format and setting the ICC feature flag.
    ///
    /// The profile is stored verbatim and placed before the image data, as the spec requires, so
    /// [`crate::metadata`] reads back exactly these bytes. With no profile embedded, readers assume
    /// sRGB. Calling this twice keeps the last payload.
    #[must_use]
    pub fn with_icc_profile(mut self, profile: &[u8]) -> Self {
        self.icc = Some(profile.to_vec());
        self
    }

    /// Sets the compression [`Effort`] — libwebp's `method` dial, `0..=6`.
    ///
    /// Applies to both modes. Higher effort spends more time searching for a smaller file; it
    /// never changes what a lossless encode reproduces (still bit-exact) nor a lossy encode's
    /// [`quality`](WebpConfig::quality) target. Calling this twice keeps the last value.
    #[must_use]
    pub fn with_effort(mut self, effort: Effort) -> Self {
        self.config.effort = effort;
        self
    }

    /// Sets (or, with `None`, clears) near-lossless preprocessing.
    ///
    /// Applies to [`WebpMode::Lossless`] only; a lossy encoder ignores it, exactly as a lossless
    /// encoder ignores [`quality`](WebpConfig::quality). The coded stream stays a conformant,
    /// bit-exact VP8L stream — what changes is its *input*, which is quantized in smooth regions
    /// first. Red, green and blue move by at most
    /// [`NearLossless::max_deviation`]; **alpha is never touched**. Calling this twice keeps the
    /// last value.
    #[must_use]
    pub fn with_near_lossless(mut self, near_lossless: Option<NearLossless>) -> Self {
        self.config.near_lossless = near_lossless;
        self
    }

    /// Encodes the lossless codestream, applying near-lossless preprocessing when configured.
    ///
    /// With a strength set, the image is coded **both ways** and the smaller result kept. That
    /// guard exists because quantization is not unconditionally a win: a gentle setting can shift
    /// every value without meaningfully shrinking the residual alphabet, costing a few bytes rather
    /// than saving them. Keeping the smaller makes the knob monotone from the caller's point of
    /// view — turning it on can never inflate a file — at the cost of one extra encode on a path
    /// that is opt-in anyway.
    ///
    /// Preprocessing is host-side and runs **before** the backend dispatch, so a pluggable
    /// codestream backend simply receives already-quantized pixels and needs no knob of its own.
    /// It also lands before the palette is built, since quantization is precisely what can drop an
    /// image under the 256-colour threshold and make the palette path available.
    fn encode_lossless(&self, argb: &[u32], dims: Dimensions) -> Result<Vec<u8>> {
        let exact = self.encode_vp8l_codestream(argb, dims)?;
        let Some(strength) = self.config.near_lossless else {
            return Ok(exact);
        };
        let quantized = near_lossless::apply(argb, strength.bits());
        let candidate = self.encode_vp8l_codestream(&quantized, dims)?;
        Ok(if candidate.len() < exact.len() {
            candidate
        } else {
            exact
        })
    }

    /// Re-emits `chunks` whose FourCC the container spec does not define, after the metadata and in
    /// the order given — what RFC 9649 §2.7.1.6 asks of writers: "writers SHOULD preserve them in
    /// their original order".
    ///
    /// Pair with [`gamut_riff::WebpLayout::parse`], whose `unknown` field yields exactly this list
    /// from a file that was read, to carry an application's private chunks through a
    /// decode/re-encode cycle instead of dropping them. Any unknown chunk promotes the output to
    /// the extended (`VP8X`) format, since only that format has a place to put one. Calling this
    /// twice keeps the last list.
    #[must_use]
    pub fn with_unknown_chunks(mut self, chunks: &[(FourCc, &[u8])]) -> Self {
        self.unknown = chunks
            .iter()
            .map(|(fourcc, payload)| (*fourcc, payload.to_vec()))
            .collect();
        self
    }

    /// Installs a codestream encoder backend, returning `&mut self` so pushes chain.
    ///
    /// Backends are tried in **push order**, ahead of the built-in `vp8`/`vp8l` encoders, which
    /// remain the implicit tails and cannot be removed. A backend declines a job by returning
    /// `false` from [`supports`](WebpCodestreamEncoder::supports); once it accepts, its error
    /// propagates and no other encoder is tried.
    ///
    /// **Cloning a `WebpEncoder` shares its backends**: the registry holds each backend behind an
    /// [`Arc`], so a clone dispatches to the very same backend objects (and the same interior
    /// state), it does not copy them.
    pub fn push_backend(&mut self, backend: impl WebpCodestreamEncoder + 'static) -> &mut Self {
        self.backends.push(Arc::new(Mutex::new(backend)));
        self
    }

    /// Encodes the lossless codestream for `argb`, via a backend when one accepts, else the
    /// built-in VP8L encoder.
    fn encode_vp8l_codestream(&self, argb: &[u32], dims: Dimensions) -> Result<Vec<u8>> {
        let req = WebpEncodeRequest::new(WebpCodestream::Vp8l, dims, self.config.quality)
            .with_effort(self.config.effort);
        let raster = RasterRef::Argb {
            dimensions: dims,
            pixels: argb,
        };
        match dispatch_encode(&self.backends, &req, &raster) {
            Some(result) => result,
            None => encode_vp8l(argb, dims, self.config.effort),
        }
    }

    /// Encodes the lossy codestream for `yuv`, via a backend when one accepts, else the built-in
    /// VP8 encoder.
    fn encode_vp8_codestream(&self, yuv: &Yuv420, dims: Dimensions) -> Result<Vec<u8>> {
        let req = WebpEncodeRequest::new(WebpCodestream::Vp8, dims, self.config.quality)
            .with_effort(self.config.effort);
        let raster = RasterRef::Yuv420(yuv);
        match dispatch_encode(&self.backends, &req, &raster) {
            Some(result) => result,
            None => {
                let opts = EncodeOptions {
                    effort: self.config.effort,
                    ..EncodeOptions::default()
                };
                Ok(encode_frame_filtered(yuv, quality_to_quant(self.config.quality), opts)?.0)
            }
        }
    }

    /// Returns the encoder's configuration.
    #[must_use]
    pub fn config(&self) -> WebpConfig {
        self.config
    }

    /// Borrows the configured metadata payloads for the container writer.
    fn metadata_chunks(&self) -> MetadataChunks<'_> {
        MetadataChunks {
            icc: self.icc.as_deref(),
            exif: self.exif.as_deref(),
            xmp: self.xmp.as_deref(),
        }
    }

    /// Wraps a coded `bitstream` in a WebP file (RFC 9649 §2.5-§2.7).
    ///
    /// With nothing that needs the extended format — no metadata and no separate `ALPH` chunk — this
    /// is the simple format: the `RIFF`/`WEBP` header plus the lone `VP8 `/`VP8L` chunk. Otherwise
    /// the file is promoted to extended, and the chunks go out in the spec's canonical order:
    /// `VP8X`, `ICCP`, `ALPH`, the bitstream, `EXIF`, `XMP `.
    ///
    /// `has_alpha` records transparency for the `VP8X` feature flag independently of `alph`, because
    /// a `VP8L` bitstream carries its own alpha and so needs no `ALPH` chunk.
    ///
    /// # Errors
    ///
    /// Propagates the container writer's rejection of a canvas or a payload the RIFF/WebP fields
    /// cannot express (RFC 9649 §2.3, §2.4, §2.7).
    fn wrap(
        &self,
        dims: Dimensions,
        codestream: WebpCodestream,
        bitstream: &[u8],
        alph: Option<&[u8]>,
        has_alpha: bool,
    ) -> Result<Vec<u8>> {
        let metadata = self.metadata_chunks();
        if metadata.is_empty() && alph.is_none() && self.unknown.is_empty() {
            return match codestream {
                WebpCodestream::Vp8 => write_simple_lossy(bitstream),
                WebpCodestream::Vp8l => write_simple_lossless(bitstream),
            };
        }
        let mut image_data: Vec<(FourCc, &[u8])> = Vec::with_capacity(2);
        if let Some(alph) = alph {
            image_data.push((FourCc::ALPH, alph));
        }
        image_data.push((
            match codestream {
                WebpCodestream::Vp8 => FourCc::VP8,
                WebpCodestream::Vp8l => FourCc::VP8L,
            },
            bitstream,
        ));
        let header = Vp8xHeader {
            alpha: has_alpha,
            canvas_width: dims.width,
            canvas_height: dims.height,
            ..Default::default()
        };
        let unknown: Vec<Chunk<'_>> = self
            .unknown
            .iter()
            .map(|(fourcc, payload)| Chunk {
                fourcc: *fourcc,
                payload,
            })
            .collect();
        write_extended_preserving(&header, &metadata, &image_data, &unknown)
    }

    /// Encodes interleaved 8-bit RGB `pixels` (row-major) of `dims`, appending the WebP file to
    /// `out`. Backs the [`EncodeImage<Rgb8>`] impl; the buffer is already validated by [`ImageRef`].
    fn encode_rgb8_inner(
        &self,
        pixels: &[u8],
        dims: Dimensions,
        out: &mut Vec<u8>,
    ) -> Result<usize> {
        let file = match self.config.mode {
            WebpMode::Lossless => {
                let argb: Vec<u32> = pixels
                    .as_chunks::<3>()
                    .0
                    .iter()
                    .map(|p| make_argb(0xff, p[0], p[1], p[2]))
                    .collect();
                let bitstream = self.encode_lossless(&argb, dims)?;
                self.wrap(dims, WebpCodestream::Vp8l, &bitstream, None, false)
            }
            WebpMode::Lossy => {
                // WebP/VP8 is limited-range BT.601 (what libwebp + browsers decode); see ColorRange.
                let yuv = Yuv420::from_rgb8(pixels, dims.width, dims.height, ColorRange::Limited)?;
                let payload = self.encode_vp8_codestream(&yuv, dims)?;
                self.wrap(dims, WebpCodestream::Vp8, &payload, None, false)
            }
        }?;
        let written = file.len();
        out.extend_from_slice(&file);
        Ok(written)
    }

    /// Encodes interleaved 8-bit RGBA `pixels` (row-major) of `dims`, appending the WebP file to
    /// `out`. A fully opaque image with no metadata produces a simple file; a transparent one uses
    /// the extended (`VP8X`) format with a raw `ALPH` alpha chunk (lossy color) or in-bitstream alpha
    /// (lossless). Backs the [`EncodeImage<Rgba8>`] impl; the buffer is already validated by
    /// [`ImageRef`].
    fn encode_rgba8_inner(
        &self,
        pixels: &[u8],
        dims: Dimensions,
        out: &mut Vec<u8>,
    ) -> Result<usize> {
        let transparent = pixels.as_chunks::<4>().0.iter().any(|p| p[3] != 0xff);
        let file = match self.config.mode {
            WebpMode::Lossless => {
                let argb: Vec<u32> = pixels
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|p| make_argb(p[3], p[0], p[1], p[2]))
                    .collect();
                let bitstream = self.encode_lossless(&argb, dims)?;
                // A VP8L bitstream carries its own alpha, so there is no `ALPH` chunk — but an
                // extended file must still advertise the transparency in its `VP8X` header.
                self.wrap(dims, WebpCodestream::Vp8l, &bitstream, None, transparent)
            }
            WebpMode::Lossy => {
                let rgb: Vec<u8> = pixels
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .flat_map(|p| [p[0], p[1], p[2]])
                    .collect();
                let yuv = Yuv420::from_rgb8(&rgb, dims.width, dims.height, ColorRange::Limited)?;
                let vp8 = self.encode_vp8_codestream(&yuv, dims)?;
                if transparent {
                    let alpha: Vec<u8> = pixels.as_chunks::<4>().0.iter().map(|p| p[3]).collect();
                    let alph =
                        alpha::write_alph(&alpha, dims.width as usize, dims.height as usize)?;
                    self.wrap(dims, WebpCodestream::Vp8, &vp8, Some(&alph), true)
                } else {
                    self.wrap(dims, WebpCodestream::Vp8, &vp8, None, false)
                }
            }
        }?;
        let written = file.len();
        out.extend_from_slice(&file);
        Ok(written)
    }
}

impl EncodeImage<Rgb8> for WebpEncoder {
    fn encode_image(&self, image: ImageRef<'_, Rgb8>, out: &mut Vec<u8>) -> Result<usize> {
        self.encode_rgb8_inner(image.as_samples(), image.dimensions(), out)
    }
}

impl EncodeImage<Rgba8> for WebpEncoder {
    fn encode_image(&self, image: ImageRef<'_, Rgba8>, out: &mut Vec<u8>) -> Result<usize> {
        self.encode_rgba8_inner(image.as_samples(), image.dimensions(), out)
    }
}

#[cfg(test)]
mod tests {
    use gamut_core::{DecodeImage, ImageBuf};

    use super::*;

    fn dims(w: u32, h: u32) -> Dimensions {
        Dimensions {
            width: w,
            height: h,
        }
    }

    #[test]
    fn constructors_select_mode() {
        assert_eq!(WebpEncoder::new().config().mode, WebpMode::Lossless);
        assert_eq!(WebpEncoder::lossless().config().mode, WebpMode::Lossless);
        let lossy = WebpEncoder::lossy(40);
        assert_eq!(lossy.config().mode, WebpMode::Lossy);
        assert_eq!(lossy.config().quality, 40);
    }

    #[test]
    fn with_effort_sets_the_knob_without_disturbing_the_mode() {
        // Effort is orthogonal to mode and quality: setting it must not perturb either, and the
        // last call wins.
        assert_eq!(WebpEncoder::new().config().effort, Effort::Default);
        let enc = WebpEncoder::lossy(40)
            .with_effort(Effort::Slowest)
            .with_effort(Effort::Fastest);
        assert_eq!(enc.config().effort, Effort::Fastest);
        assert_eq!(enc.config().mode, WebpMode::Lossy);
        assert_eq!(enc.config().quality, 40);
        assert_eq!(
            WebpEncoder::lossless()
                .with_effort(Effort::Slower)
                .config()
                .effort,
            Effort::Slower
        );
    }

    #[test]
    fn rejects_mismatched_buffer_length() {
        // Validation now lives at the ImageRef boundary, before the encoder is even called.
        assert!(ImageRef::<Rgb8>::new(&[0u8; 10], dims(2, 2)).is_err());
    }

    #[test]
    fn lossless_encodes_a_valid_webp_file() {
        // A solid 2x2 RGB image encodes to a RIFF/WebP file that the gamut decoder reads back
        // bit-exactly (the round-trip is the lossless guarantee).
        let mut out = Vec::new();
        let rgb = [0x10, 0x20, 0x30].repeat(4);
        let written = WebpEncoder::lossless()
            .encode_image(ImageRef::<Rgb8>::new(&rgb, dims(2, 2)).unwrap(), &mut out)
            .expect("encode");
        assert_eq!(written, out.len());
        assert_eq!(&out[0..4], b"RIFF");

        let decoded: ImageBuf<Rgb8> = crate::WebpDecoder::new()
            .decode_image(&out)
            .expect("decode");
        assert_eq!(decoded.dimensions(), dims(2, 2));
        assert_eq!(decoded.as_samples(), rgb.as_slice());
    }

    #[test]
    fn lossy_encodes_a_decodable_webp_file() {
        // Lossy now produces a RIFF/WebP the native decoder reads back to RGB of the right shape (the
        // pixels are lossy, so only structure is checked here; bit-exactness is the libwebp oracle).
        let mut out = Vec::new();
        let rgb = [40u8, 80, 120].repeat(16 * 16);
        let written = WebpEncoder::lossy(60)
            .encode_image(ImageRef::<Rgb8>::new(&rgb, dims(16, 16)).unwrap(), &mut out)
            .expect("lossy encode");
        assert_eq!(written, out.len());
        assert_eq!(&out[0..4], b"RIFF");
        let decoded: ImageBuf<Rgb8> = crate::WebpDecoder::new()
            .decode_image(&out)
            .expect("decode");
        assert_eq!(decoded.dimensions(), dims(16, 16));
        assert_eq!(decoded.as_samples().len(), 16 * 16 * 3);
    }

    #[test]
    fn lossy_rgba_round_trips_alpha_exactly() {
        // Transparent content: the alpha is stored losslessly (raw `ALPH`), so it round-trips
        // bit-exactly through the extended container; only the color is lossy.
        let (w, h) = (32u32, 24u32);
        let rgba: Vec<u8> = (0..(w * h) as usize)
            .flat_map(|i| {
                let (x, y) = (i as u32 % w, i as u32 / w);
                [
                    (x * 7) as u8,
                    (y * 9) as u8,
                    (x ^ y) as u8,
                    ((x * 5 + y * 3) & 0xff) as u8,
                ]
            })
            .collect();
        let mut file = Vec::new();
        WebpEncoder::lossy(75)
            .encode_image(
                ImageRef::<Rgba8>::new(&rgba, dims(w, h)).unwrap(),
                &mut file,
            )
            .expect("rgba encode");
        assert_eq!(&file[0..4], b"RIFF");

        let decoded: ImageBuf<Rgba8> = crate::WebpDecoder::new()
            .decode_image(&file)
            .expect("rgba decode");
        assert_eq!(decoded.dimensions(), dims(w, h));
        let dec_alpha: Vec<u8> = decoded
            .as_samples()
            .as_chunks::<4>()
            .0
            .iter()
            .map(|p| p[3])
            .collect();
        let src_alpha: Vec<u8> = rgba.as_chunks::<4>().0.iter().map(|p| p[3]).collect();
        assert_eq!(dec_alpha, src_alpha, "alpha must round-trip losslessly");
    }

    #[test]
    fn opaque_rgba_uses_the_simple_lossy_format() {
        use gamut_riff::{RiffReader, WebpChunkId};
        let rgba = [120u8, 60, 200, 0xff].repeat(16 * 16);
        let mut file = Vec::new();
        WebpEncoder::lossy(60)
            .encode_image(
                ImageRef::<Rgba8>::new(&rgba, dims(16, 16)).unwrap(),
                &mut file,
            )
            .expect("rgba encode");
        // A fully-opaque image carries no alpha overhead — just a single `VP8 ` chunk.
        let ids: Vec<_> = RiffReader::new(&file)
            .unwrap()
            .map(|c| WebpChunkId::from(c.unwrap().fourcc))
            .collect();
        assert_eq!(ids, vec![WebpChunkId::Vp8]);
    }

    #[test]
    fn quality_to_quant_maps_endpoints_and_is_monotonic() {
        // Higher quality → lower base quantizer index; pins the exact mapping the lossy path relies
        // on (otherwise the function can be replaced by a constant with no test noticing).
        assert_eq!(quality_to_quant(0), 127);
        assert_eq!(quality_to_quant(100), 0);
        assert_eq!(quality_to_quant(50), 63);
        assert_eq!(quality_to_quant(75), 31);
        assert_eq!(quality_to_quant(255), 0, "quality saturates at 100");
        for q in 1u8..=100 {
            assert!(
                quality_to_quant(q) <= quality_to_quant(q - 1),
                "must be non-increasing at q={q}"
            );
        }
    }

    #[test]
    fn transparent_lossy_sets_the_vp8x_alpha_flag() {
        use gamut_riff::{RiffReader, Vp8xHeader, WebpChunkId};
        // A transparent lossy image is wrapped in an extended (VP8X) file whose feature header must
        // advertise alpha, so conformant decoders know to read the ALPH chunk.
        let rgba: Vec<u8> = (0..16 * 16u32)
            .flat_map(|i| [10u8, 20, 30, (i & 0x7f) as u8])
            .collect();
        let mut file = Vec::new();
        WebpEncoder::lossy(60)
            .encode_image(
                ImageRef::<Rgba8>::new(&rgba, dims(16, 16)).unwrap(),
                &mut file,
            )
            .expect("encode");
        let vp8x = RiffReader::new(&file)
            .unwrap()
            .filter_map(Result::ok)
            .find(|c| matches!(WebpChunkId::from(c.fourcc), WebpChunkId::Vp8x))
            .expect("transparent lossy must emit a VP8X chunk");
        assert!(
            Vp8xHeader::from_payload(vp8x.payload).unwrap().alpha,
            "VP8X must advertise alpha for a transparent image"
        );
    }

    #[test]
    fn encode_image_is_object_safe() {
        let mut out = Vec::new();
        let rgb = [7u8, 8, 9];
        let enc: &dyn EncodeImage<Rgb8> = &WebpEncoder::new();
        let written = enc
            .encode_image(ImageRef::<Rgb8>::new(&rgb, dims(1, 1)).unwrap(), &mut out)
            .expect("encode via trait");
        assert_eq!(written, out.len());
        assert_eq!(&out[0..4], b"RIFF");
    }
}
