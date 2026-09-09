//! The public WebP encoder: orchestrates color handling, the VP8/VP8L bitstream, and the RIFF
//! container, mirroring the shape of [`gamut_avif::AvifEncoder`](https://docs.rs/gamut-avif).
//!
//! Both the lossless **VP8L** path (see [`crate::vp8l::encoder`]) and the lossy **VP8** path are
//! implemented, via the [`EncodeImage<Rgb8>`](gamut_core::EncodeImage) and `EncodeImage<Rgba8>`
//! impls; transparent lossy images use the extended (`VP8X`) format with a raw `ALPH` alpha chunk,
//! as does any image carrying embedded metadata.

use core::ops::Range;
use std::fmt;
use std::sync::{Arc, Mutex};

use gamut_color::{ColorRange, Yuv420};
use gamut_core::{Dimensions, EncodeImage, Error, ImageRef, Pixel, Result, Rgb8, Rgba8};
use gamut_riff::{
    Chunk, FourCc, MetadataChunks, Vp8xHeader, WebpChunkId, c2pa_span, write_extended_preserving,
    write_simple_lossless, write_simple_lossy,
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

/// Whether `fourcc` names a chunk that is genuinely *unknown* — the only kind
/// [`WebpEncoder::with_unknown_chunks`] carries through.
///
/// The classification is `gamut-riff`'s and is **asked for, not restated**: [`WebpChunkId::from`]
/// knows every chunk the WebP container defines (RFC 9649 §2.5-§2.7) plus the `C2PA` chunk of
/// C2PA 2.4 §A.3.7, and each of those is one this crate writes itself — from a dedicated setter, or
/// from the image. A hand-written list of them is a copy of that table that drifts the moment
/// `gamut-riff` recognises one more, which is exactly what happened: an eight-name list omitted
/// `ANIM` and `ANMF`, so an animation chunk was accepted and produced a file
/// [`gamut_riff::WebpLayout::parse`] then rejected as out of order.
fn is_unknown_chunk(fourcc: FourCc) -> bool {
    matches!(WebpChunkId::from(fourcc), WebpChunkId::Unknown(_))
}

/// Narrows a C2PA reservation to the `uint32` a RIFF chunk's size field holds (RFC 9649 §2.3).
///
/// Split out of [`WebpEncoder::with_c2pa_reserved`] so the limit can be tested without allocating
/// the 4 GiB it would take to reach it — the same reason `gamut-riff`'s writer splits out its own
/// `chunk_size_field`.
fn reservation_len(len: usize) -> Result<u32> {
    u32::try_from(len).map_err(|_| {
        Error::unsupported(
            env!("CARGO_PKG_NAME"),
            "WebP: C2PA reservation exceeds the uint32 chunk size field",
        )
    })
}

/// Encodes 8-bit RGB images to WebP.
///
/// Construct with [`WebpEncoder::new`] (lossless), [`WebpEncoder::lossless`], or
/// [`WebpEncoder::lossy`], then encode via the [`EncodeImage`](gamut_core::EncodeImage) trait.
///
/// Embedded metadata is attached with [`with_exif`](Self::with_exif) / [`with_xmp`](Self::with_xmp)
/// / [`with_icc_profile`](Self::with_icc_profile), which promote the output to the extended (`VP8X`)
/// format automatically. A C2PA manifest store is attached with [`with_c2pa`](Self::with_c2pa) or
/// reserved with [`with_c2pa_reserved`](Self::with_c2pa_reserved), and
/// [`encode_with_report`](Self::encode_with_report) reports where in the finished file it landed.
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
    /// The `C2PA` chunk payload — a C2PA manifest store, or a reservation of zero bytes to be
    /// filled in once it has been computed over the finished file.
    c2pa: Option<Vec<u8>>,
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
            .field("c2pa", &self.c2pa.as_ref().map(Vec::len))
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

    /// Embeds a C2PA manifest store as a `C2PA` chunk (C2PA 2.4 §A.3.7), promoting the output to
    /// the extended (`VP8X`) format.
    ///
    /// The store is written **verbatim** and placed as the last sub-chunk of the `RIFF`/`WEBP` form,
    /// which is where §A.3.7 requires it — behind `EXIF`, `XMP ` and any chunk passed to
    /// [`with_unknown_chunks`](Self::with_unknown_chunks). No `VP8X` feature flag advertises it:
    /// RFC 9649 §2.5 defines no C2PA bit, so presence is decided by the chunk alone.
    ///
    /// gamut carries the store; it does not build, hash, sign or validate one — that is a C2PA
    /// implementation's job (`c2pa-rs`). Use [`encode_with_report`](Self::encode_with_report) to
    /// learn the byte range the store's chunk occupies, which is what a `c2pa.hash.data` assertion
    /// excludes (§18.5).
    ///
    /// The last of [`with_c2pa`](Self::with_c2pa) / [`with_c2pa_reserved`](Self::with_c2pa_reserved)
    /// wins; a file carries exactly one store.
    #[must_use]
    pub fn with_c2pa(mut self, store: &[u8]) -> Self {
        self.c2pa = Some(store.to_vec());
        self
    }

    /// Reserves `len` zero bytes for a C2PA manifest store not yet computed.
    ///
    /// A store cannot be handed to the encoder complete, because its hard binding digests the
    /// finished file (C2PA 2.4 §15.12.1.1) — which does not exist until the encoder has run. The
    /// reserve-then-fill flow §18.5 asks for is three steps:
    ///
    /// 1. encode with the reservation, through
    ///    [`encode_with_report`](Self::encode_with_report), and keep the reported range;
    /// 2. hash the returned file with that **whole** range excluded, and build the store;
    /// 3. encode again with [`with_c2pa`](Self::with_c2pa) and a store of the **same length**, which
    ///    reproduces the same file with the reserved bytes replaced.
    ///
    /// The reservation is `len` bytes exactly — no slack is added — so ask for what the signer says
    /// it needs. A store shorter than the reservation would move every byte after it and invalidate
    /// the hash, which is why step 3 must match the length rather than merely fit inside it.
    ///
    /// No upper bound is imposed beyond what the container can express: a signer's `reserve_size` is
    /// its own business, and neither `gamut-avif` nor `gamut-png` caps one either.
    ///
    /// The last of [`with_c2pa`](Self::with_c2pa) / [`with_c2pa_reserved`](Self::with_c2pa_reserved)
    /// wins; a file carries exactly one store.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Unsupported`](gamut_core::Error) if `len` exceeds the `uint32` a RIFF chunk's
    /// size field holds (RFC 9649 §2.3) — the same limit
    /// [`gamut_riff::RiffWriter::write_chunk`] enforces, checked here so the reservation is refused
    /// rather than allocated: `vec![0; len]` panics on a `len` no allocator could serve, and a
    /// library path must return a typed error instead.
    pub fn with_c2pa_reserved(mut self, len: usize) -> Result<Self> {
        reservation_len(len)?;
        self.c2pa = Some(vec![0; len]);
        Ok(self)
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
    ///
    /// This is the only setter that takes a FourCC from the caller rather than just a payload, so it
    /// is the only one with an invalid input to reject — which is why it is fallible where the rest
    /// of the builder is not.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidInput`](gamut_core::Error) if `chunks` names any chunk the WebP
    /// container defines (RFC 9649 §2.5-§2.7) or the `C2PA` chunk of C2PA 2.4 §A.3.7 — that is,
    /// anything [`gamut_riff::WebpChunkId`] classifies as something other than
    /// [`Unknown`](gamut_riff::WebpChunkId::Unknown). Each is written by this crate itself, from a
    /// dedicated setter or from the image, so passing one through would emit it twice and the
    /// pass-through copy would win: `gamut-riff`'s readers take the *first* of a repeated chunk. The
    /// error names the offending FourCC in its detail, escaping any non-printable byte.
    /// Use [`with_icc_profile`](Self::with_icc_profile), [`with_exif`](Self::with_exif),
    /// [`with_xmp`](Self::with_xmp) or [`with_c2pa`](Self::with_c2pa) instead.
    pub fn with_unknown_chunks(mut self, chunks: &[(FourCc, &[u8])]) -> Result<Self> {
        if let Some((fourcc, _)) = chunks.iter().find(|(fourcc, _)| !is_unknown_chunk(*fourcc)) {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "WebP: this chunk is written by the encoder itself and cannot be passed through as \
                 an unknown chunk",
            )
            .with_detail(format!("{fourcc}")));
        }
        self.unknown = chunks
            .iter()
            .map(|(fourcc, payload)| (*fourcc, payload.to_vec()))
            .collect();
        Ok(self)
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
            c2pa: self.c2pa.as_deref(),
        }
    }

    /// Wraps a coded `bitstream` in a WebP file (RFC 9649 §2.5-§2.7).
    ///
    /// With nothing that needs the extended format — no metadata and no separate `ALPH` chunk — this
    /// is the simple format: the `RIFF`/`WEBP` header plus the lone `VP8 `/`VP8L` chunk. Otherwise
    /// the file is promoted to extended, and the chunks go out in the spec's canonical order:
    /// `VP8X`, `ICCP`, `ALPH`, the bitstream, `EXIF`, `XMP `, the preserved unknown chunks, and last
    /// of all `C2PA` (C2PA 2.4 §A.3.7).
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

/// Where the things an encode *placed* ended up in the file it produced.
///
/// Returned by [`WebpEncoder::encode_with_report`]. Construct nothing here — the encoder fills it
/// in. Marked `#[non_exhaustive]` so a later revision can report a further region without a
/// breaking change.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct WebpEncodeReport {
    /// The byte range the `C2PA` chunk occupies in the encoded file, or `None` when no manifest
    /// store was configured.
    ///
    /// The range covers the chunk's **whole** span — the four identifier bytes, the four-byte size
    /// field and the payload — because that is what a `c2pa.hash.data` assertion excludes (C2PA 2.4
    /// §18.5): an update manifest may resize the store, which changes the size field's value as
    /// well as the bytes after it. The RIFF pad byte that follows an odd-length store (RFC 9649
    /// §2.3) is outside the range; it is framing the container adds, not store.
    pub c2pa: Option<Range<usize>>,
}

impl WebpEncoder {
    /// Encodes `image` and reports where the encoder placed what it was asked to place.
    ///
    /// The bytes are exactly the bytes [`EncodeImage::encode_image`] produces for the same encoder
    /// and image — this is the same code path, not a second one — so the report can be taken as a
    /// description of any file this encoder writes. It is a separate entry point because the
    /// object-safe `EncodeImage` seam carries no channel for one.
    ///
    /// # Errors
    ///
    /// As [`EncodeImage::encode_image`], plus [`Error::InvalidInput`](gamut_core::Error) if the
    /// `C2PA` chunk read back out of the finished file is not the store that was configured. That
    /// check is what makes the reported range trustworthy: a range is only returned once the bytes
    /// inside it have been confirmed to be the caller's own store, so a signer can never be handed a
    /// span over somebody else's.
    ///
    /// That last error is **defence in depth** and no caller should expect to observe it: no input
    /// this API accepts can make the encoder write a `C2PA` chunk that is not the configured store —
    /// [`with_unknown_chunks`](Self::with_unknown_chunks) refuses one and
    /// [`gamut_riff::write_extended_preserving`] gives the configured store the slot. It is kept as
    /// a live check rather than a `debug_assert!` because this is a signing path, where a release
    /// build is exactly where the protection is worth its one walk of the chunk list.
    ///
    /// # Example
    ///
    /// ```
    /// use gamut_core::{Dimensions, ImageRef, Rgb8};
    /// use gamut_webp::WebpEncoder;
    ///
    /// let pixels = [10u8, 20, 30];
    /// let image = ImageRef::<Rgb8>::new(&pixels, Dimensions::new(1, 1)?)?;
    /// let (file, report) = WebpEncoder::lossless()
    ///     .with_c2pa_reserved(64)?
    ///     .encode_with_report(image)?;
    ///
    /// let span = report.c2pa.expect("a store was reserved");
    /// assert_eq!(&file[span.start..span.start + 4], b"C2PA");
    /// assert_eq!(span.len(), 8 + 64);
    /// # Ok::<(), gamut_core::Error>(())
    /// ```
    pub fn encode_with_report<P: Pixel>(
        &self,
        image: ImageRef<'_, P>,
    ) -> Result<(Vec<u8>, WebpEncodeReport)>
    where
        Self: EncodeImage<P>,
    {
        let mut file = Vec::new();
        self.encode_image(image, &mut file)?;
        // The range is read back out of the finished bytes with the very locator the read side uses
        // (`gamut_riff::c2pa_span`), so a writer and a reader can never disagree about it. Reading
        // the payload back through the *other* reader as well turns that into a checked claim: the
        // range is returned only once the bytes inside it are known to be the configured store, so
        // a stray `C2PA` chunk could never make the report name somebody else's bytes.
        let c2pa = c2pa_span(&file)?;
        if MetadataChunks::read(&file)?.c2pa != self.c2pa.as_deref() {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "WebP: the C2PA chunk in the encoder's own output is not the configured store",
            ));
        }
        Ok((file, WebpEncodeReport { c2pa }))
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
    use gamut_core::{DecodeImage, ErrorKind, ImageBuf};
    use gamut_riff::C2PA_FOURCC;

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

    /// The reservation is `len` zero bytes exactly — no slack, no framing. A signer sizes its store
    /// against this number, so a reservation that were merely "at least `len`" would be useless.
    #[test]
    fn with_c2pa_reserved_is_exactly_len_zero_bytes() {
        assert_eq!(
            WebpEncoder::lossless().with_c2pa_reserved(0).unwrap().c2pa,
            Some(vec![])
        );
        assert_eq!(
            WebpEncoder::lossless().with_c2pa_reserved(5).unwrap().c2pa,
            Some(vec![0, 0, 0, 0, 0])
        );
        assert_eq!(
            WebpEncoder::lossless().c2pa,
            None,
            "unconfigured by default"
        );
    }

    /// The reservation limit is the RIFF chunk size field itself, admitted right up to its ceiling
    /// and refused one past it. Testing the narrowing function rather than the builder is what makes
    /// the boundary reachable at all: pinning it through `with_c2pa_reserved` would mean allocating
    /// 4 GiB to watch the accepted side succeed.
    #[test]
    fn a_reservation_is_narrowed_to_the_uint32_size_field() {
        assert_eq!(reservation_len(0).expect("empty"), 0);
        assert_eq!(
            reservation_len(u32::MAX as usize).expect("the ceiling is admitted"),
            u32::MAX
        );
        // One past the ceiling is only expressible where `usize` is wider than `u32`; on a 32-bit
        // target (wasm32) no `usize` can exceed it, so there is nothing to refuse.
        if let Ok(past) = usize::try_from(u64::from(u32::MAX) + 1) {
            let err = reservation_len(past).expect_err("one past the ceiling is refused");
            assert_eq!(err.kind(), ErrorKind::Unsupported);
            assert!(err.to_string().contains("uint32 chunk size field"), "{err}");
        }
    }

    /// And the builder refuses such a length instead of reaching `vec![0; len]`, which panics with
    /// "capacity overflow" on a length no allocator can serve — CLAUDE.md forbids a panic on a
    /// library path.
    #[test]
    fn with_c2pa_reserved_refuses_a_length_it_cannot_represent() {
        let Ok(too_big) = usize::try_from(u64::from(u32::MAX) + 1) else {
            return; // 32-bit target: unreachable, as above.
        };
        let err = WebpEncoder::lossless()
            .with_c2pa_reserved(too_big)
            .expect_err("a reservation past the uint32 size field is refused");
        assert_eq!(err.kind(), ErrorKind::Unsupported);
    }

    /// Every FourCC `gamut-riff` classifies is refused, so the mistake is caught at the call that
    /// made it rather than resolved silently in favour of the pass-through copy.
    ///
    /// `ANIM` and `ANMF` are in the list deliberately. A hand-written table of "chunks with their
    /// own setter" left them out, and the encoder then accepted an `ANIM` chunk and wrote a file
    /// `WebpLayout::parse` refuses as having its reconstruction chunks out of order — an encoder
    /// steered into producing a file it cannot read back.
    #[test]
    fn with_unknown_chunks_refuses_every_chunk_the_container_defines() {
        let classified = [
            FourCc::VP8X,
            FourCc::VP8,
            FourCc::VP8L,
            FourCc::ALPH,
            FourCc::ICCP,
            FourCc::EXIF,
            FourCc::XMP,
            FourCc::ANIM,
            FourCc::ANMF,
            C2PA_FOURCC,
        ];
        for reserved in classified {
            let err = WebpEncoder::lossless()
                .with_unknown_chunks(&[(reserved, b"payload")])
                .err()
                .unwrap_or_else(|| panic!("{reserved} must be refused"));
            assert_eq!(err.kind(), ErrorKind::InvalidInput, "{reserved}");
            assert!(
                err.to_string().contains(&reserved.to_string()),
                "{reserved}: the error names the offending FourCC, got {err}"
            );
        }
        // A genuinely unknown FourCC is still accepted, and the check does not depend on position.
        let private = FourCc::from(*b"XYZW");
        let ok = WebpEncoder::lossless()
            .with_unknown_chunks(&[(private, b"payload")])
            .expect("a private chunk is accepted");
        assert_eq!(ok.unknown, vec![(private, b"payload".to_vec())]);
        assert!(
            WebpEncoder::lossless()
                .with_unknown_chunks(&[(private, b"a"), (C2PA_FOURCC, b"b")])
                .is_err(),
            "a reserved FourCC is refused wherever it sits in the list"
        );
    }

    /// A file carries exactly one store, so the two setters share one slot and the last call wins —
    /// including when the two kinds are mixed, which is the case a per-setter "last wins" would miss.
    #[test]
    fn the_last_c2pa_call_wins_whichever_kind_it_is() {
        assert_eq!(
            WebpEncoder::lossless()
                .with_c2pa(b"first")
                .with_c2pa(b"second")
                .c2pa,
            Some(b"second".to_vec())
        );
        assert_eq!(
            WebpEncoder::lossless()
                .with_c2pa_reserved(4)
                .unwrap()
                .with_c2pa(b"store")
                .c2pa,
            Some(b"store".to_vec())
        );
        assert_eq!(
            WebpEncoder::lossless()
                .with_c2pa(b"store")
                .with_c2pa_reserved(2)
                .unwrap()
                .c2pa,
            Some(vec![0, 0])
        );
    }

    /// The debug rendering names the store by length only: a manifest store is large and is not
    /// something a log should spill.
    #[test]
    fn debug_reports_the_store_length_not_its_bytes() {
        let rendered = format!("{:?}", WebpEncoder::lossless().with_c2pa(b"a store"));
        assert!(rendered.contains("c2pa: Some(7)"), "{rendered}");
        assert!(!rendered.contains("a store"), "{rendered}");
    }

    /// `encode_with_report` is `encode_image` plus a report — not a second encoding path — and it
    /// reports nothing when nothing was placed.
    #[test]
    fn encode_with_report_is_encode_image_plus_a_report() {
        let rgb = [0x10, 0x20, 0x30].repeat(4);
        let encoder = WebpEncoder::lossless();
        let image = || ImageRef::<Rgb8>::new(&rgb, dims(2, 2)).unwrap();

        let mut expected = Vec::new();
        encoder
            .encode_image(image(), &mut expected)
            .expect("encode");
        let (file, report) = encoder.encode_with_report(image()).expect("encode");
        assert_eq!(file, expected);
        assert_eq!(report, WebpEncodeReport::default());
        assert_eq!(report.c2pa, None, "no store was configured");
    }

    /// The reported range is the chunk's whole span (C2PA 2.4 §18.5): identifier, size field and
    /// payload, with the RIFF pad byte of an odd-length store left outside it.
    #[test]
    fn encode_with_report_names_the_whole_chunk_and_not_its_pad_byte() {
        let store = b"an odd-length manifest store!!"; // sized to an odd length below
        let store = &store[..29];
        assert_eq!(store.len() % 2, 1, "the fixture must exercise the pad byte");
        let rgb = [9u8, 8, 7].repeat(4);
        let (file, report) = WebpEncoder::lossless()
            .with_c2pa(store)
            .encode_with_report(ImageRef::<Rgb8>::new(&rgb, dims(2, 2)).unwrap())
            .expect("encode");

        let span = report.c2pa.expect("a store was configured");
        assert_eq!(&file[span.start..span.start + 4], b"C2PA", "identifier");
        assert_eq!(
            &file[span.start + 4..span.start + 8],
            &(store.len() as u32).to_le_bytes(),
            "size field"
        );
        assert_eq!(&file[span.start + 8..span.end], store, "payload");
        assert_eq!(span.end, file.len() - 1, "the pad byte is outside the span");
        assert_eq!(
            file[file.len() - 1],
            0,
            "RFC 9649 §2.3: the pad byte is zero"
        );
    }

    /// The reported range is only handed back once the bytes inside it have been confirmed to be the
    /// configured store, so a signer can never be given a span over somebody else's bytes. The
    /// encoder cannot be made to write a second `C2PA` chunk — `with_unknown_chunks` refuses one and
    /// `write_extended_preserving` filters one — so this pins the checked claim from the inside: for
    /// every store the encoder accepts, the span it reports contains exactly that store.
    #[test]
    fn the_reported_span_always_contains_the_configured_store() {
        let rgb = [1u8, 2, 3].repeat(4);
        for store in [&b""[..], &b"x"[..], &b"even"[..], &[0xff; 64][..]] {
            let (file, report) = WebpEncoder::lossless()
                .with_c2pa(store)
                .encode_with_report(ImageRef::<Rgb8>::new(&rgb, dims(2, 2)).unwrap())
                .expect("encode");
            let span = report.c2pa.expect("a store was configured");
            assert_eq!(span.len(), 8 + store.len(), "{store:?}: whole-chunk span");
            assert_eq!(&file[span.start + 8..span.end], store, "{store:?}: payload");
            assert_eq!(
                crate::metadata(&file).unwrap().c2pa.as_deref(),
                Some(store),
                "{store:?}: and the reader agrees"
            );
        }
    }

    /// The reserve-then-fill flow only works if filling a reservation disturbs nothing else: two
    /// equal-length stores must give two files that differ in exactly the reported span, so a hash
    /// taken with that span excluded survives the substitution.
    #[test]
    fn filling_a_reservation_changes_only_the_reported_span() {
        let rgb = [3u8, 5, 7].repeat(9);
        let image = || ImageRef::<Rgb8>::new(&rgb, dims(3, 3)).unwrap();
        let encode = |store: &[u8]| {
            WebpEncoder::lossless()
                .with_c2pa(store)
                .encode_with_report(image())
                .expect("encode")
        };

        let (reserved, report) = WebpEncoder::lossless()
            .with_c2pa_reserved(8)
            .unwrap()
            .encode_with_report(image())
            .expect("encode");
        let span = report.c2pa.expect("a store was reserved");
        let (first, first_report) = encode(b"11111111");
        let (second, second_report) = encode(b"22222222");

        assert_eq!(first_report.c2pa, Some(span.clone()));
        assert_eq!(second_report.c2pa, Some(span.clone()));
        assert_eq!(first.len(), reserved.len());
        assert_eq!(second.len(), reserved.len());
        for (label, filled) in [
            ("reserved", &reserved),
            ("first", &first),
            ("second", &second),
        ] {
            assert_eq!(
                filled[..span.start],
                reserved[..span.start],
                "{label}: before"
            );
            assert_eq!(filled[span.end..], reserved[span.end..], "{label}: after");
        }
        assert_eq!(
            &reserved[span.start + 8..span.end],
            &[0; 8],
            "the reservation is zeros"
        );
        assert_ne!(first[span.clone()], second[span], "the stores differ");
    }
}
