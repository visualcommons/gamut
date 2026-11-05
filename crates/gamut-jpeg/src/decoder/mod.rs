//! The DCT Huffman decoder: [`JpegDecoder`] and the marker-loop driver.
//!
//! Decoding is where the codec is generous (T.81 Annex F §F.2, Annex G §G.2, Annex A §A.2): it
//! accepts any spec-valid baseline (SOF0), extended-sequential (SOF1) **or progressive (SOF2)** 8-bit
//! stream, resolves the colour space from the JFIF/Adobe application segments, and rejects malformed
//! input with a typed [`Error`] rather than panicking. Its [`DecodeLimits`] reject hostile frame
//! geometry before the built-in decoder allocates from it. The pipeline is:
//! walk the marker segments
//! (SOI → tables → frame → scans → EOI, [`crate::syntax`]); decode each scan's entropy data into
//! per-component sample planes ([`crate::scan`]); upsample the chroma planes to full resolution
//! (libjpeg-turbo's fancy upsampling by default, see [Upsampling](#upsampling)); and
//! colour-convert to the requested pixel layout.
//!
//! # Progressive frames (SOF2)
//!
//! A progressive frame is coded as many scans, each carrying one band of the zig-zag sequence at one
//! successive-approximation precision. The driver detects SOF2, allocates one coefficient
//! accumulator per component ([`crate::scan::ProgComp`]), routes every scan through the progressive
//! entropy decoder, and reconstructs all components once at EOI. Two policy choices (both documented
//! in `STATUS.md`):
//!
//! - **Deferred height (Y = 0 / DNL) is rejected** as an
//!   [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) error: the coefficient
//!   buffers must be sized to the full block grid before the first scan, and no real encoder (nor
//!   the libjpeg-turbo oracle) emits a `Y = 0` progressive frame.
//! - **Partial streams render generously** (matching libjpeg): a frame that ends before every band
//!   is complete still reconstructs from the coefficients delivered so far, provided every component
//!   received its DC first pass — otherwise the frame has no baseline and is rejected as
//!   an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error.
//!
//! # Colour interpretation
//!
//! Following the de-facto rules that libjpeg-family decoders use, the component count and the
//! advisory APP0/APP14 segments select the transform:
//!
//! - **1 component** → grayscale.
//! - **3 components** → Adobe APP14 `transform=1` ⇒ YCbCr, `transform=0` ⇒ RGB; with no APP14, a
//!   JFIF APP0 ⇒ YCbCr; with neither, component ids `R,G,B` (`0x52,0x47,0x42`) ⇒ RGB, otherwise
//!   YCbCr (the de-facto default).
//! - **4 components** → Adobe APP14 `transform=2` ⇒ YCCK (the inverse YCbCr transform on the first
//!   three channels, `K` passed through); otherwise CMYK stored verbatim (no Adobe inversion, the
//!   libjpeg convention).
//! - **2 or > 4 components** → [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported).
//!
//! # Upsampling
//!
//! T.81 leaves the reconstruction filter for a component coded at reduced resolution open (§A.2
//! NOTE). By default ([`ChromaUpsampling::Interpolated`]) the decoder does what libjpeg-turbo's
//! `djpeg` does by default, the "fancy" triangle filter of `jdsample.c`, through
//! [`gamut_dsp::upsample::libjpeg`]. That covers a component at half the horizontal resolution
//! (4:2:2), half the vertical (4:4:0) or half both (4:2:0), whatever the absolute sampling
//! factors. A 4:2:2 or 4:2:0 component only one or two samples wide, and every other integer
//! ratio, is replicated, as libjpeg-turbo does too. The output is therefore byte-identical to
//! libjpeg-turbo's default decode, which the crate's oracle tests pin. A non-integer ratio (the
//! `3`s §A.1.1 permits, e.g. a component sampled 2 of 3) is replicated too; libjpeg-turbo refuses
//! such a stream (`JERR_FRACT_SAMPLE_NOTIMPL`), so there is no reference output to match.
//! [`JpegDecoder::with_chroma_upsampling`] selects [`ChromaUpsampling::Nearest`], which replicates
//! every component (`djpeg -nosmooth`). The choice applies to every subsampled
//! component, CMYK and RGB included, and only to the built-in decoder: a pushed backend presents
//! its own raster.
//!
//! # Module layout
//!
//! One submodule per pipeline stage: `segments` reads the marker layer (SOI, marker codes, segment
//! lengths, and the frame header [`JpegDecoder::info`] reports), `driver` runs the marker loop
//! and assembles the decoded planes into a `DecodedImage`, `upsample` maps each component onto the
//! output grid, and `present` applies the colour transform and writes the caller's pixel layout.
//! This module holds the decoder's configuration and limits, the header-only readers
//! ([`JpegDecoder::info`], [`JpegDecoder::metadata`]), and the `DecodedImage` the stages hand on.

mod driver;
mod present;
mod segments;
#[cfg(test)]
mod tests;
mod upsample;

use std::fmt;
use std::sync::{Arc, Mutex};

use gamut_color::ChromaUpsampling;
use gamut_core::convert::ConvertPolicy;
// Named only by the docs: the presentation layouts live in `present`.
#[cfg(doc)]
use gamut_core::{Cmyk8, DecodeImage, Gray8, Rgb8};
use gamut_core::{DecodeLimits, Error, RawMetadata, Result};

pub(crate) use self::segments::{expect_soi, frame_header, read_marker, read_segment, trunc_sof};
use crate::appmeta;
use crate::backend::{self, DecodedJpeg, DecoderRegistry, JpegCodestreamDecoder, JpegStreamInfo};
use crate::marker::code;
use crate::scan::McuRowLimit;
use crate::syntax::{ColorInfo, Frame};

/// The default [`DecodeLimits`]: each side capped at the format's own 16-bit ceiling (T.81 Table
/// B.2, `X` and `Y` ≤ 65535), so the default admits every dimension a frame header can declare and
/// leaves the workspace's 64 MiB byte budget
/// ([`DecodeLimits::DEFAULT_MAX_IMAGE_BYTES`]) as the guard.
const DEFAULT_LIMITS: DecodeLimits = DecodeLimits::new()
    .with_max_width(u16::MAX as u32)
    .with_max_height(u16::MAX as u32);

/// A decoder for sequential (baseline SOF0 / extended SOF1) 8-bit Huffman JPEG streams.
///
/// Cheap to construct; drive it through the [`DecodeImage`] trait. It presents the decoded image as
/// [`Rgb8`] (grayscale replicated, YCbCr/RGB three-component), [`Gray8`] (single-component streams
/// only), or [`Cmyk8`] (four-component CMYK/YCCK streams only).
///
/// The defaults bound hostile input the way `gamut-png`'s decoder does: the dimension caps sit at
/// the format's own 16-bit ceiling (T.81 Table B.2: `X`, `Y` ≤ 65535) and the native interleaved
/// raster is capped at 64 MiB — the byte budget, not the dimension caps, is the real guard, since a
/// few-byte SOF declaring 65535 × 65535 × 3 would otherwise ask for ~12.9 GB before any scan is
/// read. Tighten either with [`with_limits`](Self::with_limits); a caller that trusts its input and
/// needs a larger raster loosens the budget with
/// `DecodeLimits::new().with_max_image_bytes(usize::MAX)`.
///
/// # Example
///
/// ```
/// use gamut_core::{DecodeImage, EncodeImage, Dimensions, Gray8, ImageRef, Rgb8};
/// use gamut_jpeg::{JpegDecoder, JpegEncoder};
///
/// // Round-trip a small grayscale image through the encoder and back.
/// let pixels: Vec<u8> = (0..64).map(|i| (i * 4) as u8).collect();
/// let image = ImageRef::<Gray8>::new(&pixels, Dimensions::new(8, 8)?)?;
/// let jpeg = JpegEncoder::new().with_quality(90).encode_to_vec(image)?;
///
/// let decoded = JpegDecoder::new().decode_image(&jpeg)?;
/// let _rgb: gamut_core::ImageBuf<Rgb8> = decoded; // grayscale replicated across channels
/// # Ok::<(), gamut_core::Error>(())
/// ```
#[derive(Clone)]
pub struct JpegDecoder {
    /// The dimension and native-raster byte bounds.
    limits: DecodeLimits,
    /// Pluggable whole-stream backends, tried in push order ahead of the built-in decoder.
    backends: DecoderRegistry,
    /// Which lossy layout conversions a typed decode may perform.
    policy: ConvertPolicy,
    /// How the built-in decoder reconstructs subsampled components.
    upsampling: ChromaUpsampling,
}

impl fmt::Debug for JpegDecoder {
    /// The backends are opaque `dyn` objects, so only their count is shown.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JpegDecoder")
            .field("limits", &self.limits)
            .field("backends", &self.backends.len())
            .field("upsampling", &self.upsampling)
            .finish()
    }
}

impl Default for JpegDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl JpegDecoder {
    /// Creates a decoder with the default limits (65535 × 65535 dimensions, 64 MiB of native
    /// interleaved raster), libjpeg-turbo's fancy chroma upsampling
    /// ([`ChromaUpsampling::Interpolated`]) and no pluggable backends: every stream is decoded by
    /// the built-in decoder.
    #[must_use]
    pub fn new() -> Self {
        Self {
            limits: DEFAULT_LIMITS,
            backends: DecoderRegistry::new(),
            policy: ConvertPolicy::lossless(),
            upsampling: ChromaUpsampling::Interpolated,
        }
    }

    /// Sets the resource bounds ([`DecodeLimits`]) the decoder enforces.
    ///
    /// The width and height bounds refuse a wider SOF, or a known taller one, before any
    /// frame-sized allocation; a DNL-deferred height is bounded while its first scan is decoded and
    /// checked exactly when the DNL arrives. The byte bound caps the image's **native** interleaved
    /// 8-bit raster, `width × height × frame components`, independently of the requested
    /// presentation type: a one-component grayscale JPEG costs one byte per pixel even when decoded
    /// as [`Rgb8`]. For a DNL-deferred height the decoder derives a safe MCU-row ceiling from that
    /// budget before growing its sample planes. A breach is an
    /// [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) error.
    ///
    /// The default caps each side at 65535, the most a frame header can declare, and the raster at
    /// [`DecodeLimits::DEFAULT_MAX_IMAGE_BYTES`] (64 MiB).
    #[must_use]
    pub fn with_limits(mut self, limits: DecodeLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Selects which lossy conversions a typed decode may perform.
    ///
    /// Defaults to [`ConvertPolicy::lossless`]: a colour stream cannot be decoded as [`Gray8`],
    /// since inventing a luminance from colour is a choice only the caller can make. Grayscale
    /// still replicates into RGB, which loses nothing.
    #[must_use]
    pub fn with_convert_policy(mut self, policy: ConvertPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Selects how the built-in decoder reconstructs components coded at reduced resolution.
    ///
    /// Defaults to [`ChromaUpsampling::Interpolated`]: libjpeg-turbo's fancy upsampling, so the
    /// output is byte-identical to `djpeg`'s default decode.
    /// [`ChromaUpsampling::Nearest`] replicates each sample instead, which is cheaper. A pushed
    /// backend presents its own raster and ignores this setting.
    #[must_use]
    pub fn with_chroma_upsampling(mut self, upsampling: ChromaUpsampling) -> Self {
        self.upsampling = upsampling;
        self
    }

    /// Appends a [`JpegCodestreamDecoder`] backend to this decoder's registry and returns the
    /// decoder.
    ///
    /// Backends are consulted in **push order** for every decode, each being offered the
    /// crate-parsed [`JpegStreamInfo`] through [`JpegCodestreamDecoder::supports`]; the first to
    /// accept receives the **whole** SOI..EOI stream. The built-in decoder is the implicit tail,
    /// used only when every backend declines; a backend that accepts and then fails returns its
    /// error unchanged. APPn metadata ([`Self::metadata`]) is always read crate-side.
    ///
    /// `Send` is bound here, where the backend is inserted, rather than on the trait, so the
    /// decoder stays `Send + Sync`.
    ///
    /// # Cloning shares backends
    ///
    /// Backends are stored behind `Arc<Mutex<..>>`, so **cloning a `JpegDecoder` shares them with
    /// the clone** rather than duplicating them: a stateful backend sees the decodes of every clone,
    /// and its lock serializes concurrent ones. A backend added to one decoder after the clone is
    /// not seen by the other.
    #[must_use]
    pub fn with_backend(mut self, backend: impl JpegCodestreamDecoder + Send + 'static) -> Self {
        self.backends.push(Arc::new(Mutex::new(backend)));
        self
    }

    /// Offers `data` to the registry, returning the first accepted raster, or `None` when there are
    /// no backends or every one declined — in which case the caller runs the built-in decoder.
    ///
    /// The marker layer is parsed **here**, crate-side, before any backend is consulted.
    fn decode_via_backend(&self, data: &[u8]) -> Result<Option<DecodedJpeg>> {
        if self.backends.is_empty() {
            return Ok(None);
        }
        let info = JpegStreamInfo::parse(data)?;
        self.check_limits(
            info.width(),
            info.height().unwrap_or(0),
            usize::from(info.components()),
        )?;
        let decoded = backend::decode_with_backends(&self.backends, &info, data)?;
        if let Some(img) = &decoded {
            self.check_limits(img.width(), img.height(), img.format().channels())?;
        }
        Ok(decoded)
    }

    /// Checks dimensions and native raster bytes. A zero `height` is a DNL-deferred SOF: width and
    /// the minimum possible one-row raster are still checked immediately.
    fn check_limits(&self, width: u32, height: u32, components: usize) -> Result<()> {
        if width > self.limits.max_width() || (height != 0 && height > self.limits.max_height()) {
            return Err(Error::unsupported(
                env!("CARGO_PKG_NAME"),
                "JPEG: image exceeds the dimension limit",
            ));
        }
        let row_bytes = usize::try_from(width)
            .ok()
            .and_then(|width| width.checked_mul(components))
            .ok_or_else(|| {
                Error::invalid_input(env!("CARGO_PKG_NAME"), "JPEG: image dimensions overflow")
            })?;
        let native_bytes = if height == 0 {
            row_bytes
        } else {
            row_bytes
                .checked_mul(usize::try_from(height).map_err(|_| {
                    Error::invalid_input(env!("CARGO_PKG_NAME"), "JPEG: image dimensions overflow")
                })?)
                .ok_or_else(|| {
                    Error::invalid_input(env!("CARGO_PKG_NAME"), "JPEG: image dimensions overflow")
                })?
        };
        if native_bytes > self.limits.max_image_bytes() {
            return Err(Error::unsupported(
                env!("CARGO_PKG_NAME"),
                "JPEG: image exceeds the size limit",
            ));
        }
        Ok(())
    }

    /// Caps the number of MCU rows a sequential `Y=0` scan may allocate before its DNL arrives.
    /// The cap is the tighter of the explicit height and the height implied by the byte budget.
    fn deferred_row_limit(
        &self,
        frame: &Frame,
        scan: &crate::syntax::ScanHeader,
    ) -> Result<McuRowLimit> {
        let dimension_limit = McuRowLimit {
            rows: scan_mcu_rows(
                frame,
                scan,
                usize::try_from(self.limits.max_height()).unwrap_or(usize::MAX),
            ),
            message: "JPEG: image exceeds the dimension limit",
        };
        let row_bytes = usize::from(frame.x)
            .checked_mul(frame.components.len())
            .ok_or_else(|| {
                Error::invalid_input(env!("CARGO_PKG_NAME"), "JPEG: image dimensions overflow")
            })?;
        let size_limit = McuRowLimit {
            rows: scan_mcu_rows(frame, scan, self.limits.max_image_bytes() / row_bytes),
            message: "JPEG: image exceeds the size limit",
        };
        Ok(if size_limit.rows < dimension_limit.rows {
            size_limit
        } else {
            dimension_limit
        })
    }
}

/// Number of MCU rows needed to cover `height` output lines in `scan`.
fn scan_mcu_rows(frame: &Frame, scan: &crate::syntax::ScanHeader, height: usize) -> usize {
    // SOF/DNL heights are u16, so a looser application cap cannot require a larger grid. Clamping
    // also keeps the sampling-factor multiplication below `usize` overflow on narrow targets.
    let height = height.min(usize::from(u16::MAX));
    let vmax = usize::from(frame.vmax());
    if scan.interleaved() {
        height.div_ceil(8 * vmax)
    } else {
        let fc = &frame.components[scan.components[0].frame_index];
        let comp_h = (height * usize::from(fc.v)).div_ceil(vmax);
        comp_h.div_ceil(8)
    }
}

/// The DCT process a JPEG stream uses, as reported by [`JpegDecoder::info`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum JpegProcess {
    /// Baseline sequential DCT (SOF0).
    Baseline,
    /// Extended sequential DCT (SOF1).
    ExtendedSequential,
    /// Progressive DCT (SOF2).
    Progressive,
}

/// A lightweight summary of a JPEG stream's frame header, from [`JpegDecoder::info`].
///
/// Reads only the marker segments up to and including the frame header — no entropy decoding — so it
/// is cheap to call before committing to a full decode. Marked `#[non_exhaustive]` so fields can be
/// added without a breaking change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct JpegInfo {
    /// Samples per line `X` (§B.2.2).
    pub width: u32,
    /// Number of lines `Y`; `None` when the frame defers its height to a DNL segment (§B.2.5) —
    /// the frame header's `Y = 0` is that deferral, never an empty image.
    pub height: Option<u32>,
    /// Number of components `Nf`.
    pub components: u8,
    /// Sample precision `P` in bits.
    pub precision: u8,
    /// The DCT process (from the SOFn marker).
    pub process: JpegProcess,
}

impl JpegDecoder {
    /// Reads a JPEG stream's frame header without decoding it, returning its [`JpegInfo`].
    ///
    /// No configuration of the decoder affects the answer: it reads only the marker segments up to
    /// and including the frame header, and enforces no [`DecodeLimits`].
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if the
    /// stream is malformed or has no frame header, or an
    /// [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) error if the frame uses a
    /// process other than baseline / extended-sequential / progressive DCT.
    pub fn info(&self, data: &[u8]) -> Result<JpegInfo> {
        let (process, payload) = frame_header(data)?;
        // Frame header layout (§B.2.2): P, Y(2), X(2), Nf, then components.
        let precision = *payload.first().ok_or_else(trunc_sof)?;
        let y = u16::from_be_bytes([
            *payload.get(1).ok_or_else(trunc_sof)?,
            *payload.get(2).ok_or_else(trunc_sof)?,
        ]);
        let x = u16::from_be_bytes([
            *payload.get(3).ok_or_else(trunc_sof)?,
            *payload.get(4).ok_or_else(trunc_sof)?,
        ]);
        let nf = *payload.get(5).ok_or_else(trunc_sof)?;
        Ok(JpegInfo {
            width: u32::from(x),
            height: (y != 0).then_some(u32::from(y)),
            components: nf,
            precision,
            process,
        })
    }

    /// Reads a JPEG stream's embedded APP1/APP2 metadata without decoding any pixels.
    ///
    /// Walks the marker segments up to the first SOS (or EOI) — metadata segments precede the scan
    /// data, so no entropy decoding happens — and collects the three de-facto payloads into a
    /// [`RawMetadata`]: APP1 EXIF (Exif 3.0 §4.7.2) as the TIFF stream with its `"Exif\0\0"`
    /// signature stripped, APP1 XMP (XMP Part 3 §1.1.3) as the `xpacket` with its namespace-URI
    /// signature stripped, and the APP2 `ICC_PROFILE` chunk sequence (ICC.1:2001-04 Annex B.4),
    /// reassembled by chunk index regardless of segment order. For a duplicated EXIF or XMP APP1
    /// the first segment wins (the libjpeg-family convention); ExtendedXMP continuation segments
    /// and unrecognized APP1/APP2 payloads are skipped. Metadata is always read crate-side: no
    /// pushed backend is consulted.
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if the
    /// stream is malformed before the first scan, or if the `ICC_PROFILE` chunk sequence is
    /// inconsistent (index/count out of range, duplicated, mismatched, or missing chunks).
    ///
    /// # Example
    ///
    /// The extracted payload is byte-for-byte identical to the one given to the encoder:
    ///
    /// ```
    /// use gamut_core::{Dimensions, EncodeImage, Gray8, ImageRef, RawMetadata};
    /// use gamut_jpeg::{JpegDecoder, JpegEncoder};
    ///
    /// let icc = b"opaque ICC profile bytes";
    /// let pixels = vec![0u8; 64];
    /// let image = ImageRef::<Gray8>::new(&pixels, Dimensions::new(8, 8)?)?;
    /// let jpeg = JpegEncoder::new().with_icc_profile(icc).encode_to_vec(image)?;
    /// let meta = JpegDecoder::new().metadata(&jpeg)?;
    /// assert_eq!(meta.icc.as_deref(), Some(icc.as_slice()));
    ///
    /// let bare = JpegEncoder::new().encode_to_vec(image)?;
    /// assert_eq!(JpegDecoder::new().metadata(&bare)?, RawMetadata::default());
    /// # Ok::<(), gamut_core::Error>(())
    /// ```
    pub fn metadata(&self, data: &[u8]) -> Result<RawMetadata> {
        expect_soi(data)?;
        let mut meta = RawMetadata::default();
        let mut icc = appmeta::IccAssembler::default();
        let mut pos = 2;
        loop {
            let (marker, after) = read_marker(data, pos)?;
            match marker {
                code::SOS | code::EOI_CODE => break,
                code::SOI | code::TEM | code::RST0..=code::RST7 => {
                    return Err(Error::invalid_input(
                        env!("CARGO_PKG_NAME"),
                        "JPEG: unexpected standalone marker",
                    ));
                }
                _ => {}
            }
            let (payload, next) = read_segment(data, after)?;
            match marker {
                code::APP1 => {
                    if let Some(tiff) = appmeta::exif_payload(payload) {
                        meta.exif.get_or_insert_with(|| tiff.to_vec());
                    } else if let Some(packet) = appmeta::xmp_payload(payload) {
                        meta.xmp.get_or_insert_with(|| packet.to_vec());
                    }
                }
                code::APP2 => icc.add(payload)?,
                _ => {}
            }
            pos = next;
        }
        meta.icc = icc.finish()?;
        Ok(meta)
    }
}

/// One decoded component ready for presentation: its sampling factors, its valid height, and the
/// reconstructed plane at block-padded resolution (`stride` wide). Its valid width needs no field:
/// no output column maps past it ([`upsample::ColumnMap`]), and the interpolating kernels read only the
/// `ceil(width / 2)` samples a half-resolution component holds.
struct DecComp {
    h: u8,
    v: u8,
    comp_h: usize,
    stride: usize,
    data: Vec<u8>,
}

/// A fully decoded image: the reconstructed component planes plus the colour hints needed to present
/// them.
struct DecodedImage {
    width: u32,
    height: u32,
    hmax: usize,
    vmax: usize,
    comps: Vec<DecComp>,
    ids: Vec<u8>,
    color: ColorInfo,
}
