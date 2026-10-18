//! The DCT Huffman decoder: [`JpegDecoder`] and the marker-loop driver.
//!
//! Decoding is where the codec is generous (T.81 Annex F §F.2, Annex G §G.2, Annex A §A.2): it
//! accepts any spec-valid baseline (SOF0), extended-sequential (SOF1) **or progressive (SOF2)** 8-bit
//! stream, resolves the colour space from the JFIF/Adobe application segments, and rejects malformed
//! input with a typed [`Error`] rather than panicking. Opt-in dimension and native-raster byte caps
//! reject hostile frame geometry before the built-in decoder allocates from it. The pipeline is:
//! walk the marker segments
//! (SOI → tables → frame → scans → EOI, [`crate::syntax`]); decode each scan's entropy data into
//! per-component sample planes ([`crate::scan`]); upsample the chroma planes to full resolution by
//! sample replication; and colour-convert to the requested pixel layout.
//!
//! # Progressive frames (SOF2)
//!
//! A progressive frame is coded as many scans, each carrying one band of the zig-zag sequence at one
//! successive-approximation precision. The driver detects SOF2, allocates one coefficient
//! accumulator per component ([`crate::scan::ProgComp`]), routes every scan through the progressive
//! entropy decoder, and reconstructs all components once at EOI. Two policy choices (both documented
//! in `STATUS.md`):
//!
//! - **Deferred height (Y = 0 / DNL) is rejected** as [`Error::Unsupported`]: the coefficient
//!   buffers must be sized to the full block grid before the first scan, and no real encoder (nor
//!   the libjpeg-turbo oracle) emits a `Y = 0` progressive frame.
//! - **Partial streams render generously** (matching libjpeg): a frame that ends before every band
//!   is complete still reconstructs from the coefficients delivered so far, provided every component
//!   received its DC first pass — otherwise the frame has no baseline and is rejected as
//!   [`Error::InvalidInput`].
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
//! - **2 or > 4 components** → [`Error::Unsupported`].
//!
//! # Upsampling
//!
//! Chroma planes coded at reduced resolution are upsampled to full resolution by **sample
//! replication** (nearest-neighbour). T.81 leaves the reconstruction filter open (§A.2 NOTE); sample
//! replication is the decoder's documented free choice — exact for 4:4:4 and the cheapest faithful
//! choice for subsampled chroma.

use std::fmt;
use std::sync::{Arc, Mutex};

use gamut_color::{ColorRange, ycbcr_to_rgb};
use gamut_core::convert::{ConvertPolicy, RawImage, convert_from_raw, convert_from_raw_into};
use gamut_core::{
    Cmyk8, DecodeImage, Dimensions, Error, Gray8, ImageBuf, Pixel, PixelFormat, Result, Rgb8,
};

use crate::appmeta;
use crate::backend::{self, DecodedJpeg, DecoderSlot, JpegStreamDecoder, JpegStreamInfo};
use crate::marker::code;
use crate::scan::{McuRowLimit, Plane, ProgComp, decode_progressive_scan, decode_scan};
use crate::syntax::{
    ColorInfo, Frame, Tables, parse_app0, parse_app14, parse_dht, parse_dnl, parse_dqt, parse_dri,
    parse_sof, parse_sos,
};

/// A decoder for sequential (baseline SOF0 / extended SOF1) 8-bit Huffman JPEG streams.
///
/// Cheap to construct; drive it through the [`DecodeImage`] trait. It presents the decoded image as
/// [`Rgb8`] (grayscale replicated, YCbCr/RGB three-component), [`Gray8`] (single-component streams
/// only), or [`Cmyk8`] (four-component CMYK/YCCK streams only). Limits are opt-in, preserving the
/// decoder's previous unrestricted defaults; applications handling untrusted input should set both
/// [`with_max_dimensions`](Self::with_max_dimensions) and
/// [`with_max_image_bytes`](Self::with_max_image_bytes).
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
#[derive(Clone, Default)]
pub struct JpegDecoder {
    /// Optional maximum accepted width.
    max_width: Option<u32>,
    /// Optional maximum accepted height.
    max_height: Option<u32>,
    /// Optional maximum native interleaved raster length.
    max_image_bytes: Option<usize>,
    /// Pluggable whole-stream backends, tried in push order ahead of the built-in decoder.
    backends: Vec<DecoderSlot>,
    /// Which lossy layout conversions a typed decode may perform.
    policy: ConvertPolicy,
}

impl fmt::Debug for JpegDecoder {
    /// The backends are opaque `dyn` objects, so only their count is shown.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JpegDecoder")
            .field("max_width", &self.max_width)
            .field("max_height", &self.max_height)
            .field("max_image_bytes", &self.max_image_bytes)
            .field("backends", &self.backends.len())
            .finish()
    }
}

impl JpegDecoder {
    /// Creates a decoder with no resource limits and no pluggable backends: every stream is decoded
    /// by the built-in decoder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Caps the accepted image dimensions. A wider SOF or a known taller SOF is refused before any
    /// frame-sized allocation; a DNL-deferred height is bounded while its first scan is decoded and
    /// checked exactly when the DNL arrives.
    #[must_use]
    pub fn with_max_dimensions(mut self, width: u32, height: u32) -> Self {
        self.max_width = Some(width);
        self.max_height = Some(height);
        self
    }

    /// Caps the image's native interleaved 8-bit raster in bytes: `width × height × frame
    /// components`.
    ///
    /// The cap is independent of the requested presentation type: for example, a one-component
    /// grayscale JPEG costs one byte per pixel even when decoded as [`Rgb8`]. For a DNL-deferred
    /// height, the decoder derives a safe MCU-row ceiling from this budget before growing its sample
    /// planes. The default is unrestricted.
    #[must_use]
    pub fn with_max_image_bytes(mut self, bytes: usize) -> Self {
        self.max_image_bytes = Some(bytes);
        self
    }

    /// Selects which lossy conversions a typed decode may perform.
    ///
    /// Defaults to [`ConvertPolicy::lossless`]: a colour stream cannot be decoded as [`Gray8`],
    /// since inventing a luminance from colour is a choice only the caller can make. Grayscale
    /// still replicates into RGB, which loses nothing.
    #[must_use]
    pub fn convert_policy(mut self, policy: ConvertPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Appends a [`JpegStreamDecoder`] backend to this decoder's registry.
    ///
    /// Backends are consulted in **push order** for every decode, each being offered the
    /// crate-parsed [`JpegStreamInfo`] through [`JpegStreamDecoder::supports`]; the first to accept
    /// receives the **whole** SOI..EOI stream. The built-in decoder is the implicit tail, used only
    /// when every backend declines. See the [`crate::backend`] module docs for the full contract.
    ///
    /// # Cloning shares backends
    ///
    /// Backends are stored behind `Arc<Mutex<..>>`, so **cloning a `JpegDecoder` shares them with
    /// the clone** rather than duplicating them: a stateful backend sees the decodes of every clone,
    /// and its lock serializes concurrent ones. Build a fresh decoder when a clone needs its own
    /// backend instances.
    pub fn push_backend(&mut self, backend: impl JpegStreamDecoder + 'static) -> &mut Self {
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
        self.check_limits(info.width(), info.height(), usize::from(info.components()))?;
        let decoded = backend::decode_with_backends(&self.backends, &info, data)?;
        if let Some(img) = &decoded {
            self.check_limits(img.width(), img.height(), img.format().channels())?;
        }
        Ok(decoded)
    }

    /// Checks dimensions and native raster bytes. A zero `height` is a DNL-deferred SOF: width and
    /// the minimum possible one-row raster are still checked immediately.
    fn check_limits(&self, width: u32, height: u32, components: usize) -> Result<()> {
        if self.max_width.is_some_and(|max| width > max)
            || (height != 0 && self.max_height.is_some_and(|max| height > max))
        {
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
        if let Some(max) = self.max_image_bytes {
            let native_bytes = if height == 0 {
                row_bytes
            } else {
                row_bytes
                    .checked_mul(usize::try_from(height).map_err(|_| {
                        Error::invalid_input(
                            env!("CARGO_PKG_NAME"),
                            "JPEG: image dimensions overflow",
                        )
                    })?)
                    .ok_or_else(|| {
                        Error::invalid_input(
                            env!("CARGO_PKG_NAME"),
                            "JPEG: image dimensions overflow",
                        )
                    })?
            };
            if native_bytes > max {
                return Err(Error::unsupported(
                    env!("CARGO_PKG_NAME"),
                    "JPEG: image exceeds the size limit",
                ));
            }
        }
        Ok(())
    }

    /// Caps the number of MCU rows a sequential `Y=0` scan may allocate before its DNL arrives.
    /// The cap is the tighter of the explicit height and the height implied by the byte budget.
    fn deferred_row_limit(
        &self,
        frame: &Frame,
        scan: &crate::syntax::ScanHeader,
    ) -> Result<Option<McuRowLimit>> {
        let mut limit = self.max_height.map(|height| McuRowLimit {
            rows: scan_mcu_rows(frame, scan, usize::try_from(height).unwrap_or(usize::MAX)),
            message: "JPEG: image exceeds the dimension limit",
        });
        if let Some(max_bytes) = self.max_image_bytes {
            let row_bytes = usize::from(frame.x)
                .checked_mul(frame.components.len())
                .ok_or_else(|| {
                    Error::invalid_input(env!("CARGO_PKG_NAME"), "JPEG: image dimensions overflow")
                })?;
            let height = max_bytes / row_bytes;
            let size_limit = McuRowLimit {
                rows: scan_mcu_rows(frame, scan, height),
                message: "JPEG: image exceeds the size limit",
            };
            if limit.is_none_or(|current| size_limit.rows < current.rows) {
                limit = Some(size_limit);
            }
        }
        Ok(limit)
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

/// The DCT process a JPEG stream uses, as reported by [`info`].
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

/// A lightweight summary of a JPEG stream's frame header, from [`info`].
///
/// Reads only the marker segments up to and including the frame header — no entropy decoding — so it
/// is cheap to call before committing to a full decode. Marked `#[non_exhaustive]` so fields can be
/// added without a breaking change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct JpegInfo {
    /// Samples per line `X` (§B.2.2).
    pub width: u32,
    /// Number of lines `Y`; `0` if the frame defers its height to a DNL segment.
    pub height: u32,
    /// Number of components `Nf`.
    pub components: u8,
    /// Sample precision `P` in bits.
    pub precision: u8,
    /// The DCT process (from the SOFn marker).
    pub process: JpegProcess,
}

/// Reads a JPEG stream's frame header without decoding it, returning its [`JpegInfo`].
///
/// # Errors
///
/// Returns [`Error::InvalidInput`] if the stream is malformed or has no frame header, or
/// [`Error::Unsupported`] if the frame uses a process other than baseline / extended-sequential /
/// progressive DCT.
pub fn info(data: &[u8]) -> Result<JpegInfo> {
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
        height: u32::from(y),
        components: nf,
        precision,
        process,
    })
}

/// Walks the marker layer to the frame header, returning its process and raw SOFn payload without
/// entropy decoding. Shared by [`info`] and [`crate::JpegStreamInfo::parse`], which read different
/// field subsets out of the same payload.
///
/// # Errors
///
/// Returns [`Error::InvalidInput`] if the stream is malformed or has no frame header, or
/// [`Error::Unsupported`] if the frame uses a process other than baseline / extended-sequential /
/// progressive DCT.
pub(crate) fn frame_header(data: &[u8]) -> Result<(JpegProcess, &[u8])> {
    expect_soi(data)?;
    let mut pos = 2;
    loop {
        let (marker, after) = read_marker(data, pos)?;
        let process = match marker {
            code::SOF0 => JpegProcess::Baseline,
            code::SOF1 => JpegProcess::ExtendedSequential,
            code::SOF2 => JpegProcess::Progressive,
            code::SOF3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF => {
                return Err(Error::unsupported(
                    env!("CARGO_PKG_NAME"),
                    "JPEG: unsupported process",
                ));
            }
            code::SOS | code::EOI_CODE => {
                return Err(Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "JPEG: no frame header before scan/end",
                ));
            }
            code::SOI | code::TEM | code::RST0..=code::RST7 => {
                return Err(Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "JPEG: unexpected standalone marker",
                ));
            }
            _ => {
                // Any other segment (tables, application data, comment): skip by length.
                let (_, next) = read_segment(data, after)?;
                pos = next;
                continue;
            }
        };
        let (payload, _) = read_segment(data, after)?;
        return Ok((process, payload));
    }
}

/// Embedded APP-segment metadata read from a JPEG stream by [`metadata`].
///
/// Each payload is stored with its APPn signature header stripped, in the form the dedicated
/// metadata crates parse (and [`gamut-metadata`](https://crates.io/crates/gamut-metadata)'s
/// `MetadataBlock` borrows) directly. Marked `#[non_exhaustive]` so carriers (e.g. APP13 IPTC) can
/// be added without a breaking change.
///
/// # Example
///
/// The extracted payload is byte-for-byte identical to the one given to the encoder. It can then
/// be borrowed directly by a typed metadata facade without re-framing:
///
/// ```
/// use gamut_core::{Dimensions, EncodeImage, Gray8, ImageRef};
/// use gamut_jpeg::JpegEncoder;
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let icc = b"opaque ICC profile bytes";
/// let pixels = vec![0u8; 64];
/// let image = ImageRef::<Gray8>::new(&pixels, Dimensions::new(8, 8)?)?;
/// let jpeg = JpegEncoder::new().with_icc_profile(icc).encode_to_vec(image)?;
///
/// let meta = gamut_jpeg::metadata(&jpeg)?;
/// assert_eq!(meta.icc.as_deref(), Some(icc.as_slice()));
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct JpegMetadata {
    /// The EXIF TIFF stream (APP1, `"Exif\0\0"` signature stripped; starts `II`/`MM`).
    pub exif: Option<Vec<u8>>,
    /// The XMP `xpacket` (APP1, namespace-URI signature stripped).
    pub xmp: Option<Vec<u8>>,
    /// The ICC profile, reassembled from its APP2 `ICC_PROFILE` chunks.
    pub icc: Option<Vec<u8>>,
}

/// Reads a JPEG stream's embedded APP1/APP2 metadata without decoding any pixels.
///
/// Walks the marker segments up to the first SOS (or EOI) — metadata segments precede the scan
/// data, so no entropy decoding happens — and collects the three de-facto payloads: APP1 EXIF
/// (Exif 3.0 §4.7.2), APP1 XMP (XMP Part 3 §1.1.3), and the APP2 `ICC_PROFILE` chunk sequence
/// (ICC.1:2001-04 Annex B.4), reassembled by chunk index regardless of segment order. For a
/// duplicated EXIF or XMP APP1 the first segment wins (the libjpeg-family convention); ExtendedXMP
/// continuation segments and unrecognized APP1/APP2 payloads are skipped.
///
/// # Errors
///
/// Returns [`Error::InvalidInput`] if the stream is malformed before the first scan, or if the
/// `ICC_PROFILE` chunk sequence is inconsistent (index/count out of range, duplicated, mismatched,
/// or missing chunks).
///
/// # Example
///
/// ```
/// use gamut_core::{Dimensions, EncodeImage, Gray8, ImageRef};
/// use gamut_jpeg::JpegEncoder;
///
/// let pixels = vec![128u8; 64];
/// let image = ImageRef::<Gray8>::new(&pixels, Dimensions::new(8, 8)?)?;
/// let jpeg = JpegEncoder::new().encode_to_vec(image)?;
/// assert_eq!(gamut_jpeg::metadata(&jpeg)?, gamut_jpeg::JpegMetadata::default());
/// # Ok::<(), gamut_core::Error>(())
/// ```
pub fn metadata(data: &[u8]) -> Result<JpegMetadata> {
    expect_soi(data)?;
    let mut meta = JpegMetadata::default();
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

/// Shared truncated-frame-header error.
pub(crate) fn trunc_sof() -> Error {
    Error::invalid_input(env!("CARGO_PKG_NAME"), "JPEG: truncated frame header")
}

/// Verifies the two-byte SOI that opens every JPEG stream (§B.2.1).
pub(crate) fn expect_soi(data: &[u8]) -> Result<()> {
    if data.len() < 2 || data[0] != 0xFF || data[1] != code::SOI {
        return Err(
            Error::invalid_input(env!("CARGO_PKG_NAME"), "JPEG: missing SOI marker")
                .with_byte_offset(0),
        );
    }
    Ok(())
}

/// Reads the next marker at or after `pos` (skipping fill `0xFF` bytes, §B.1.1.2), returning its
/// code and the offset just past it.
pub(crate) fn read_marker(data: &[u8], pos: usize) -> Result<(u8, usize)> {
    if data.get(pos) != Some(&0xFF) {
        return Err(
            Error::invalid_input(env!("CARGO_PKG_NAME"), "JPEG: expected a marker")
                .with_byte_offset(pos as u64),
        );
    }
    // The marker code is the first non-fill byte: skip any run of fill 0xFF bytes (§B.1.1.2).
    let code_pos = pos + data[pos..].iter().take_while(|&&b| b == 0xFF).count();
    let code = *data.get(code_pos).ok_or_else(|| {
        Error::invalid_input(env!("CARGO_PKG_NAME"), "JPEG: truncated marker")
            .with_byte_offset(code_pos as u64)
    })?;
    Ok((code, code_pos + 1))
}

/// Reads a marker segment's payload at `pos` (the offset just past the marker code), returning the
/// payload bytes and the offset just past the segment. The two-byte length counts itself (§B.1.1.4).
pub(crate) fn read_segment(data: &[u8], pos: usize) -> Result<(&[u8], usize)> {
    let truncated = || trunc_seg().with_byte_offset(pos as u64);
    let hi = *data.get(pos).ok_or_else(truncated)?;
    let lo = *data.get(pos.saturating_add(1)).ok_or_else(truncated)?;
    let len = usize::from(u16::from_be_bytes([hi, lo]));
    if len < 2 {
        return Err(
            Error::invalid_input(env!("CARGO_PKG_NAME"), "JPEG: segment length < 2")
                .with_byte_offset(pos as u64),
        );
    }
    let end = pos.checked_add(len).ok_or_else(truncated)?;
    let payload_start = pos.checked_add(2).ok_or_else(truncated)?;
    let payload = data.get(payload_start..end).ok_or_else(truncated)?;
    Ok((payload, end))
}

/// Shared truncated-segment error.
fn trunc_seg() -> Error {
    Error::invalid_input(env!("CARGO_PKG_NAME"), "JPEG: truncated segment")
}

/// One decoded component ready for presentation: its sampling factors, its valid height, and the
/// reconstructed plane at block-padded resolution (`stride` wide). Its valid width needs no field:
/// no output column maps past it ([`ColumnMap`]).
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

impl DecodedImage {
    /// The row of component `ci` under output row `py`, via nearest-neighbour (sample-replication)
    /// upsampling -- [`ColumnMap`]'s vertical twin -- as the slice of the plane that holds it.
    fn sample_row(&self, ci: usize, py: usize) -> &[u8] {
        let c = &self.comps[ci];
        let cy = (py * usize::from(c.v) / self.vmax).min(c.comp_h - 1);
        &c.data[cy * c.stride..(cy + 1) * c.stride]
    }
}

/// How one component's samples spread across the output columns under nearest-neighbour
/// (sample-replication) upsampling: output column `px` takes component column `px * h / hmax`.
///
/// That column never needs clamping to the component's width: [`assemble`] sizes a component
/// `comp_w = ceil(width * h / hmax)` samples wide, and for `px < width`,
/// `px * h / hmax <= (width - 1) * h / hmax < comp_w`. (The same argument holds for the vertical
/// twin, [`DecodedImage::sample_row`], whose clamp this change leaves as it was.)
///
/// The two ratios real streams use get a path of their own, so the common rows need no per-sample
/// index: full horizontal resolution reads the plane row in place, and half resolution (4:2:0 and
/// 4:2:2 chroma) doubles each sample. Every other ratio (the `3`s §A.1.1 permits) goes through a
/// column map computed once per image. All three compute the same function, so no decoded image
/// can tell which one a component took; `column_map_picks_the_path_for_each_ratio` pins the
/// choice itself.
#[derive(Debug, PartialEq, Eq)]
enum ColumnMap {
    /// `h == hmax`: component column `px` under output column `px`, read in place.
    Direct,
    /// `2 * h == hmax`: component column `px / 2` under output column `px`.
    Double,
    /// Any other ratio: the component column under every output column.
    Indexed(Vec<usize>),
}

impl ColumnMap {
    /// The map for a component sampled `h` of `hmax` horizontally, under an image `width` columns
    /// wide.
    fn new(h: usize, hmax: usize, width: usize) -> Self {
        if h == hmax {
            Self::Direct
        } else if 2 * h == hmax {
            Self::Double
        } else {
            Self::Indexed((0..width).map(|px| px * h / hmax).collect())
        }
    }

    /// The component's samples under every output column of the plane row `src`: `src` itself for
    /// [`Self::Direct`], otherwise `scratch` (one output row wide) filled with them.
    fn upsample<'a>(&self, src: &'a [u8], scratch: &'a mut [u8]) -> &'a [u8] {
        match self {
            Self::Direct => &src[..scratch.len()],
            Self::Double => {
                let (pairs, last) = scratch.as_chunks_mut::<2>();
                let paired = pairs.len();
                for (pair, &s) in pairs.iter_mut().zip(src) {
                    *pair = [s, s];
                }
                // An odd width's last column is the first half of a pair the row has no room for.
                for (dst, &s) in last.iter_mut().zip(&src[paired..]) {
                    *dst = s;
                }
                scratch
            }
            Self::Indexed(map) => {
                for (dst, &cx) in scratch.iter_mut().zip(map) {
                    *dst = src[cx];
                }
                scratch
            }
        }
    }
}

/// Converts one row of full-range YCbCr (T.871 §7) to interleaved RGB in `out`, three samples per
/// pixel, through `packed`, a scratch row of at least as many pixels.
///
/// The arithmetic is [`ycbcr_to_rgb`]'s, sample for sample, so the output is byte-identical to the
/// per-pixel conversion; what changes is the shape of the loops, so that the compiler vectorises
/// them with the target's baseline vector instructions (SSE2 on x86-64, NEON on aarch64) and no
/// `unsafe`:
///
/// 1. The conversion runs over whole rows with nothing but the three source samples and one
///    destination per pixel, packing each result into a `u32` lane (`R`, `G`, `B`, then a zero
///    byte). Converting straight into the interleaved row instead left it scalar: its three byte
///    stores per pixel are a stride-3 pattern the vectoriser does not take.
/// 2. The packed row is written out with one four-byte store per pixel, each overlapping the next
///    pixel's first byte, which that pixel's own store then overwrites. Only the last pixel, whose
///    fourth byte would land past the row, is written as three bytes.
fn ycbcr_to_rgb_row(y: &[u8], cb: &[u8], cr: &[u8], packed: &mut [u32], out: &mut [u8]) {
    for (((p, &y), &cb), &cr) in packed.iter_mut().zip(y).zip(cb).zip(cr) {
        let (r, g, b) = ycbcr_to_rgb(y, cb, cr, ColorRange::Full);
        *p = u32::from_le_bytes([r, g, b, 0]);
    }
    let n = out.len() / 3;
    for (i, p) in packed.iter().enumerate().take(n.saturating_sub(1)) {
        out[3 * i..3 * i + 4].copy_from_slice(&p.to_le_bytes());
    }
    if let (Some(px), Some(p)) = (
        out.as_chunks_mut::<3>().0.last_mut(),
        packed.get(n.wrapping_sub(1)),
    ) {
        let [r, g, b, _] = p.to_le_bytes();
        *px = [r, g, b];
    }
}

/// The colour transform selected for presentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Transform {
    /// One luminance channel.
    Gray,
    /// Three channels stored directly as R, G, B.
    Rgb,
    /// Three channels YCbCr → RGB (T.871 §7).
    YCbCr,
    /// Four channels stored directly as C, M, Y, K.
    Cmyk,
    /// Four channels YCCK: inverse YCbCr on the first three, `K` passed through.
    Ycck,
}

/// The colour-interpretation decision tree (see the module docs).
fn decide_transform(img: &DecodedImage) -> Result<Transform> {
    match img.comps.len() {
        1 => Ok(Transform::Gray),
        3 => Ok(match img.color.adobe_transform {
            Some(0) => Transform::Rgb,
            Some(_) => Transform::YCbCr, // 1 = YCbCr; any other Adobe value defaults to YCbCr
            None => {
                if img.color.jfif {
                    Transform::YCbCr
                } else if img.ids.as_slice() == b"RGB" {
                    Transform::Rgb
                } else {
                    Transform::YCbCr
                }
            }
        }),
        4 => Ok(match img.color.adobe_transform {
            Some(2) => Transform::Ycck,
            _ => Transform::Cmyk,
        }),
        _ => Err(Error::unsupported(
            env!("CARGO_PKG_NAME"),
            "JPEG: only 1, 3, or 4 component streams are supported",
        )),
    }
}

impl JpegDecoder {
    /// The shared marker-loop driver: decodes `data` to an internal [`DecodedImage`].
    fn decode_internal(&self, data: &[u8]) -> Result<DecodedImage> {
        expect_soi(data)?;
        let mut pos = 2;
        let mut tables = Tables::default();
        let mut color = ColorInfo::default();
        let mut frame: Option<Frame> = None;
        let mut planes: Vec<Option<Plane>> = Vec::new();
        // Progressive (SOF2) state: one coefficient accumulator per component, filled across scans
        // and reconstructed once at EOI. Empty for a sequential frame.
        let mut progressive = false;
        let mut prog: Vec<ProgComp> = Vec::new();

        loop {
            let (marker, after) = read_marker(data, pos)?;
            match marker {
                code::EOI_CODE => break,
                code::SOF0 | code::SOF1 => {
                    if frame.is_some() {
                        return Err(Error::invalid_input(
                            env!("CARGO_PKG_NAME"),
                            "JPEG: duplicate frame header",
                        ));
                    }
                    let (payload, next) = read_segment(data, after)?;
                    let f = parse_sof(payload)?;
                    self.check_limits(f.x.into(), f.y.into(), f.components.len())?;
                    planes = (0..f.components.len()).map(|_| None).collect();
                    frame = Some(f);
                    pos = next;
                }
                code::SOF2 => {
                    if frame.is_some() {
                        return Err(Error::invalid_input(
                            env!("CARGO_PKG_NAME"),
                            "JPEG: duplicate frame header",
                        ));
                    }
                    let (payload, next) = read_segment(data, after)?;
                    let f = parse_sof(payload)?;
                    self.check_limits(f.x.into(), f.y.into(), f.components.len())?;
                    if f.y == 0 {
                        // A Y=0 (DNL-deferred height) progressive frame is rejected: the
                        // coefficient buffers must be sized to the full block grid before the first
                        // scan, and libjpeg-turbo never emits one. See STATUS.md.
                        return Err(Error::unsupported(
                            env!("CARGO_PKG_NAME"),
                            "JPEG: progressive frame with deferred height (Y=0/DNL) not supported",
                        ));
                    }
                    prog = build_prog_comps(&f);
                    progressive = true;
                    frame = Some(f);
                    pos = next;
                }
                code::SOF3 => {
                    return Err(Error::unsupported(
                        env!("CARGO_PKG_NAME"),
                        "JPEG: lossless process (SOF3) not supported",
                    ));
                }
                0xC5..=0xC7 => {
                    return Err(Error::unsupported(
                        env!("CARGO_PKG_NAME"),
                        "JPEG: hierarchical process (SOF5-7) not supported",
                    ));
                }
                0xC9..=0xCB | 0xCD..=0xCF | code::DAC => {
                    return Err(Error::unsupported(
                        env!("CARGO_PKG_NAME"),
                        "JPEG: arithmetic coding not supported",
                    ));
                }
                code::DHT => {
                    let (payload, next) = read_segment(data, after)?;
                    parse_dht(payload, &mut tables)?;
                    pos = next;
                }
                code::DQT => {
                    let (payload, next) = read_segment(data, after)?;
                    parse_dqt(payload, &mut tables)?;
                    pos = next;
                }
                code::DRI => {
                    let (payload, next) = read_segment(data, after)?;
                    tables.restart_interval = parse_dri(payload)?;
                    pos = next;
                }
                code::SOS => {
                    let f = frame.as_ref().ok_or_else(|| {
                        Error::invalid_input(env!("CARGO_PKG_NAME"), "JPEG: SOS before SOF")
                    })?;
                    let (payload, next) = read_segment(data, after)?;
                    let scan = parse_sos(payload, f, progressive)?;
                    if progressive {
                        pos = decode_progressive_scan(data, next, f, &scan, &tables, &mut prog)?;
                    } else {
                        for sc in &scan.components {
                            if planes[sc.frame_index].is_some() {
                                return Err(Error::invalid_input(
                                    env!("CARGO_PKG_NAME"),
                                    "JPEG: component coded by more than one scan",
                                ));
                            }
                        }
                        let row_limit = if f.y == 0 {
                            self.deferred_row_limit(f, &scan)?
                        } else {
                            None
                        };
                        let result = decode_scan(data, next, f, &scan, &tables, row_limit)?;
                        for (ci, plane) in result.planes {
                            planes[ci] = Some(plane);
                        }
                        pos = result.marker_offset;
                    }
                }
                code::DNL => {
                    let (payload, next) = read_segment(data, after)?;
                    let nl = parse_dnl(payload)?;
                    // §B.2.5: DNL defines the height only when Y was 0.
                    if let Some(f) = frame.as_mut()
                        && f.y == 0
                    {
                        f.y = nl;
                        self.check_limits(f.x.into(), f.y.into(), f.components.len())?;
                    }
                    pos = next;
                }
                code::APP0 => {
                    let (payload, next) = read_segment(data, after)?;
                    parse_app0(payload, &mut color);
                    pos = next;
                }
                code::APP14 => {
                    let (payload, next) = read_segment(data, after)?;
                    parse_app14(payload, &mut color);
                    pos = next;
                }
                code::SOI | code::TEM | code::RST0..=code::RST7 => {
                    return Err(Error::invalid_input(
                        env!("CARGO_PKG_NAME"),
                        "JPEG: unexpected standalone marker outside a scan",
                    ));
                }
                _ => {
                    // Any other segment-bearing marker (other APPn, COM, reserved): skip by length.
                    let (_, next) = read_segment(data, after)?;
                    pos = next;
                }
            }
        }

        if progressive {
            assemble_progressive(frame, prog, color)
        } else {
            assemble(frame, planes, color)
        }
    }
}

/// Allocates one progressive coefficient accumulator per frame component, sized to the component's
/// own block grid `ceil(comp_w/8) × ceil(comp_h/8)` (§A.2.2). The height is known (Y ≠ 0 is enforced
/// for progressive frames), so the grid is fixed before the first scan.
fn build_prog_comps(frame: &Frame) -> Vec<ProgComp> {
    let hmax = usize::from(frame.hmax());
    let vmax = usize::from(frame.vmax());
    let x = usize::from(frame.x);
    let y = usize::from(frame.y);
    frame
        .components
        .iter()
        .map(|c| {
            let comp_w = (x * usize::from(c.h)).div_ceil(hmax);
            let comp_h = (y * usize::from(c.v)).div_ceil(vmax);
            ProgComp::new(c.h, c.v, comp_w.div_ceil(8), comp_h.div_ceil(8))
        })
        .collect()
}

/// Finalizes a progressive decode: enforces the partial-render policy (every component must have
/// received its DC first pass, §G.1.1.1.1), reconstructs each component's sample plane once
/// (dequantize + IDCT + level shift), and reuses [`assemble`] to crop and package the image.
fn assemble_progressive(
    frame: Option<Frame>,
    comps: Vec<ProgComp>,
    color: ColorInfo,
) -> Result<DecodedImage> {
    let frame = frame
        .ok_or_else(|| Error::invalid_input(env!("CARGO_PKG_NAME"), "JPEG: no frame header"))?;
    for c in &comps {
        // Partial-render policy: an incomplete stream renders whatever bands arrived, but every
        // component must at least have its DC scan or the image has no baseline to show.
        if !c.has_dc() {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "JPEG: progressive frame missing a component's DC scan",
            ));
        }
    }
    let planes = comps.into_iter().map(|c| Some(c.into_plane())).collect();
    assemble(Some(frame), planes, color)
}

/// Finalizes a decode: validates the frame is complete, crops each component to its valid region,
/// and packages the [`DecodedImage`].
fn assemble(
    frame: Option<Frame>,
    planes: Vec<Option<Plane>>,
    color: ColorInfo,
) -> Result<DecodedImage> {
    let frame = frame
        .ok_or_else(|| Error::invalid_input(env!("CARGO_PKG_NAME"), "JPEG: no frame header"))?;
    if frame.y == 0 {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "JPEG: frame height Y is 0 and no DNL supplied it",
        ));
    }
    let hmax = usize::from(frame.hmax());
    let vmax = usize::from(frame.vmax());
    let x = usize::from(frame.x);
    let y = usize::from(frame.y);

    let mut comps = Vec::with_capacity(frame.components.len());
    let mut ids = Vec::with_capacity(frame.components.len());
    for (fc, plane) in frame.components.iter().zip(planes) {
        let plane = plane.ok_or_else(|| {
            Error::invalid_input(env!("CARGO_PKG_NAME"), "JPEG: a component was never coded")
        })?;
        let comp_w = (x * usize::from(fc.h)).div_ceil(hmax);
        let comp_h = (y * usize::from(fc.v)).div_ceil(vmax);
        // The decoded plane must cover the valid region (a DNL claiming more lines than were coded
        // would fail here).
        let padded_rows = plane.data.len().checked_div(plane.stride).unwrap_or(0);
        if comp_w > plane.stride || comp_h > padded_rows {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "JPEG: decoded plane smaller than the frame declares",
            ));
        }
        comps.push(DecComp {
            h: plane.h,
            v: plane.v,
            comp_h,
            stride: plane.stride,
            data: plane.data,
        });
        ids.push(fc.id);
    }

    Ok(DecodedImage {
        width: frame.x.into(),
        height: frame.y.into(),
        hmax,
        vmax,
        comps,
        ids,
        color,
    })
}

/// Presents a decoded image in the layout its components natively describe, applying **only** the
/// colour transform the stream declares: YCbCr → RGB, and YCCK → CMYK (Adobe TN #5116). Rearranging
/// that into the caller's pixel layout is [`gamut_core::convert`]'s job, so this function never
/// replicates grey across channels or refuses a target.
///
/// Works a row at a time: each component's row is upsampled through its [`ColumnMap`] (in place at
/// full resolution, into a scratch row otherwise), and the transform then runs over the zipped
/// rows, writing straight into `out` -- `width * height` pixels of `format` as
/// [`native_layout`] returned it alongside `transform`. The caller sizes `out`, so the presented
/// image is written once, into the buffer that is returned. YCbCr rows, the common case, convert
/// through [`ycbcr_to_rgb_row`].
fn present_native(img: &DecodedImage, transform: Transform, format: PixelFormat, out: &mut [u8]) {
    let w = img.width as usize;
    let maps: Vec<ColumnMap> = img
        .comps
        .iter()
        .map(|c| ColumnMap::new(usize::from(c.h), img.hmax, w))
        .collect();
    let mut scratch = vec![vec![0u8; w]; img.comps.len()];
    let mut packed = vec![0u32; w];
    for (py, line) in out.chunks_exact_mut(w * format.channels()).enumerate() {
        let rows: Vec<&[u8]> = maps
            .iter()
            .zip(&mut scratch)
            .enumerate()
            .map(|(ci, (map, scratch))| map.upsample(img.sample_row(ci, py), scratch))
            .collect();
        match (transform, rows.as_slice()) {
            (Transform::Gray, &[y]) => line.copy_from_slice(y),
            (Transform::Rgb, &[r, g, b]) => {
                for (((px, &r), &g), &b) in
                    line.as_chunks_mut::<3>().0.iter_mut().zip(r).zip(g).zip(b)
                {
                    px.copy_from_slice(&[r, g, b]);
                }
            }
            (Transform::YCbCr, &[y, cb, cr]) => ycbcr_to_rgb_row(y, cb, cr, &mut packed, line),
            (Transform::Cmyk, &[c, m, y, k]) => {
                for ((((px, &c), &m), &y), &k) in line
                    .as_chunks_mut::<4>()
                    .0
                    .iter_mut()
                    .zip(c)
                    .zip(m)
                    .zip(y)
                    .zip(k)
                {
                    px.copy_from_slice(&[c, m, y, k]);
                }
            }
            // YCCK → CMYK: invert the YCbCr transform on the first three channels, K passes
            // through (Adobe TN #5116).
            (Transform::Ycck, &[y, cb, cr, k]) => {
                for ((((px, &y), &cb), &cr), &k) in line
                    .as_chunks_mut::<4>()
                    .0
                    .iter_mut()
                    .zip(y)
                    .zip(cb)
                    .zip(cr)
                    .zip(k)
                {
                    let (r, g, b) = ycbcr_to_rgb(y, cb, cr, ColorRange::Full);
                    px.copy_from_slice(&[255 - r, 255 - g, 255 - b, k]);
                }
            }
            // `decide_transform` picks each transform from the component count, so the row count
            // always matches the arm; this one is unreachable and stays only to keep the match
            // total without an `unwrap`.
            _ => {}
        }
    }
}

/// The colour transform `img` needs and the pixel layout it presents natively, decided before any
/// sample is written: Gray8 for one component, Rgb8 for three, Cmyk8 for four.
fn native_layout(img: &DecodedImage) -> Result<(Transform, PixelFormat)> {
    let transform = decide_transform(img)?;
    let format = match transform {
        Transform::Gray => PixelFormat::Gray8,
        Transform::Rgb | Transform::YCbCr => PixelFormat::Rgb8,
        Transform::Cmyk | Transform::Ycck => PixelFormat::Cmyk8,
    };
    Ok((transform, format))
}

impl JpegDecoder {
    /// Decodes `data`, through a backend when one accepts it, and presents it as layout `P`.
    fn present<P: Pixel<Sample = u8>>(&self, data: &[u8]) -> Result<ImageBuf<P>> {
        if let Some(img) = self.decode_via_backend(data)? {
            let dims = Dimensions::new(img.width(), img.height())?;
            let raw = RawImage::new(img.samples(), img.format(), dims)?;
            return convert_from_raw(raw, self.policy);
        }
        let img = self.decode_internal(data)?;
        let dims = Dimensions::new(img.width, img.height)?;
        let (transform, format) = native_layout(&img)?;
        self.present_decoded(&img, dims, transform, format)
    }

    /// Presents the built-in decoder's `img` as layout `P` in a new buffer. When `P` is the native
    /// layout the samples are written straight into the returned image; otherwise they are written
    /// once natively and converted by [`gamut_core::convert`] under the decoder's policy (the
    /// identity a matching layout would take there needs no permission, so writing it directly
    /// changes no outcome).
    fn present_decoded<P: Pixel<Sample = u8>>(
        &self,
        img: &DecodedImage,
        dims: Dimensions,
        transform: Transform,
        format: PixelFormat,
    ) -> Result<ImageBuf<P>> {
        if format == P::FORMAT {
            let mut out = ImageBuf::<P>::zeroed(dims)?;
            present_native(img, transform, format, out.as_mut_samples());
            Ok(out)
        } else {
            let samples = native_samples(img, transform, format);
            convert_from_raw(RawImage::new(&samples, format, dims)?, self.policy)
        }
    }

    /// As [`JpegDecoder::present`], reusing `dst`'s sample storage when the decoded dimensions
    /// match its own. On error `dst` is left unchanged.
    fn present_into<P: Pixel<Sample = u8>>(
        &self,
        data: &[u8],
        dst: &mut ImageBuf<P>,
    ) -> Result<()> {
        if let Some(img) = self.decode_via_backend(data)? {
            let dims = Dimensions::new(img.width(), img.height())?;
            return self.convert_into(RawImage::new(img.samples(), img.format(), dims)?, dst);
        }
        let img = self.decode_internal(data)?;
        let dims = Dimensions::new(img.width, img.height)?;
        let (transform, format) = native_layout(&img)?;
        if format != P::FORMAT {
            let samples = native_samples(&img, transform, format);
            return self.convert_into(RawImage::new(&samples, format, dims)?, dst);
        }
        // The native layout is the requested one: write it straight into `dst` when the
        // dimensions let it hold the image. Nothing past `native_layout` can fail, so `dst` is
        // only touched once the presentation is certain to complete.
        if dst.dimensions() == dims {
            present_native(&img, transform, format, dst.as_mut_samples());
        } else {
            *dst = self.present_decoded(&img, dims, transform, format)?;
        }
        Ok(())
    }

    /// Converts `raw` into `dst` under the decoder's policy, reusing `dst`'s storage when the
    /// dimensions match and replacing the buffer otherwise. On error `dst` is left unchanged.
    fn convert_into<P: Pixel<Sample = u8>>(
        &self,
        raw: RawImage<'_, u8>,
        dst: &mut ImageBuf<P>,
    ) -> Result<()> {
        if dst.dimensions() == raw.dimensions() {
            convert_from_raw_into::<_, P>(raw, self.policy, dst.as_mut_samples())
        } else {
            *dst = convert_from_raw(raw, self.policy)?;
            Ok(())
        }
    }
}

/// [`present_native`] into a new buffer of exactly the native layout's size, for a caller that
/// converts it on into another layout.
fn native_samples(img: &DecodedImage, transform: Transform, format: PixelFormat) -> Vec<u8> {
    let mut out = vec![0u8; img.width as usize * img.height as usize * format.channels()];
    present_native(img, transform, format, &mut out);
    out
}

impl DecodeImage<Rgb8> for JpegDecoder {
    /// Grayscale is replicated across channels; three-component YCbCr/RGB is presented as RGB;
    /// four-component (CMYK/YCCK) returns [`Error::Unsupported`] (decode as [`Cmyk8`]).
    fn decode_image(&self, data: &[u8]) -> Result<ImageBuf<Rgb8>> {
        self.present(data)
    }

    /// Reuses `dst`'s sample storage when the decoded dimensions match its own, replacing the
    /// buffer otherwise. On error `dst` is left unchanged.
    fn decode_image_into(&self, data: &[u8], dst: &mut ImageBuf<Rgb8>) -> Result<()> {
        self.present_into(data, dst)
    }
}

impl DecodeImage<Gray8> for JpegDecoder {
    /// A single-component stream passes through unchanged. A colour stream needs a
    /// [`LumaPolicy`](gamut_core::convert::LumaPolicy) to be reduced to luma.
    fn decode_image(&self, data: &[u8]) -> Result<ImageBuf<Gray8>> {
        self.present(data)
    }

    /// Reuses `dst`'s sample storage when the decoded dimensions match its own, replacing the
    /// buffer otherwise. On error `dst` is left unchanged.
    fn decode_image_into(&self, data: &[u8], dst: &mut ImageBuf<Gray8>) -> Result<()> {
        self.present_into(data, dst)
    }
}

impl DecodeImage<Cmyk8> for JpegDecoder {
    /// Errors unless the stream is four-component; CMYK passes through, YCCK is inverted to CMYK.
    fn decode_image(&self, data: &[u8]) -> Result<ImageBuf<Cmyk8>> {
        self.present(data)
    }

    /// Reuses `dst`'s sample storage when the decoded dimensions match its own, replacing the
    /// buffer otherwise. On error `dst` is left unchanged.
    fn decode_image_into(&self, data: &[u8], dst: &mut ImageBuf<Cmyk8>) -> Result<()> {
        self.present_into(data, dst)
    }
}

#[cfg(test)]
mod tests {
    //! Hand-built stream tests: the decoder is driven by JPEG byte vectors assembled from the
    //! crate's own marker/table writers plus a private entropy emitter (the inverse of §F.2), so
    //! each colour path, scan layout, and marker case is exercised against an independently computed
    //! expectation. These cover ground the encoder round-trips cannot (non-interleaved multi-scan,
    //! CMYK/YCCK, Adobe/RGB colour, SOF1).

    use gamut_color::{ColorRange, ycbcr_to_rgb};
    use gamut_core::{Cmyk8, DecodeImage, Dimensions, Gray8, ImageBuf, Rgb8};
    use gamut_dsp::jpeg::idct8x8_int;

    use super::*;
    use crate::bitwriter::BitWriter;
    use crate::huffman::{self, EncTable};
    use crate::zigzag::ZIGZAG;
    use crate::{marker, quant};

    /// Magnitude category (bit length of `|value|`; 0 for 0) — the §F.1.2 SSSS.
    fn magcat(value: i32) -> u8 {
        (32 - value.unsigned_abs().leading_zeros()) as u8
    }

    /// The SSSS additional bits for `value` (the encoder's inverse of `EXTEND`).
    fn addbits(value: i32, cat: u8) -> u16 {
        let v = if value < 0 { value - 1 } else { value };
        (v as u32 & ((1u32 << cat) - 1)) as u16
    }

    /// Emits one block's DC + AC entropy for natural-order quantized `coeffs` against the running
    /// `pred` (§F.1.2), mirroring the encoder's coder so the decoder can be pinned independently.
    fn emit_block(
        w: &mut BitWriter,
        coeffs: &[i32; 64],
        pred: &mut i32,
        dc: &EncTable,
        ac: &EncTable,
    ) {
        let diff = coeffs[0] - *pred;
        *pred = coeffs[0];
        let cat = magcat(diff);
        let (c, l) = dc.lookup(cat).unwrap();
        w.write_bits(c, l);
        w.write_bits(addbits(diff, cat), cat);
        let mut run = 0u8;
        for &nat in &ZIGZAG[1..] {
            let v = coeffs[nat];
            if v == 0 {
                run += 1;
                continue;
            }
            while run >= 16 {
                let (c, l) = ac.lookup(0xF0).unwrap();
                w.write_bits(c, l);
                run -= 16;
            }
            let cat = magcat(v);
            let (c, l) = ac.lookup((run << 4) | cat).unwrap();
            w.write_bits(c, l);
            w.write_bits(addbits(v, cat), cat);
            run = 0;
        }
        if run > 0 {
            let (c, l) = ac.lookup(0x00).unwrap();
            w.write_bits(c, l);
        }
    }

    /// Encodes an ordered list of `(component_table_index, block)` into a flushed entropy segment,
    /// resetting one predictor per table slot.
    fn entropy(order: &[(usize, [i32; 64])], tables: &[(EncTable, EncTable)]) -> Vec<u8> {
        let mut preds = vec![0i32; tables.len()];
        let mut out = Vec::new();
        let mut w = BitWriter::new(&mut out);
        for (ci, coeffs) in order {
            emit_block(
                &mut w,
                coeffs,
                &mut preds[*ci],
                &tables[*ci].0,
                &tables[*ci].1,
            );
        }
        w.flush();
        out
    }

    /// The four standard `(EncTable)` pairs used throughout: `(luma_dc, luma_ac, chroma_dc,
    /// chroma_ac)`.
    fn std_enc() -> (EncTable, EncTable, EncTable, EncTable) {
        (
            EncTable::from_spec(&huffman::STD_LUMA_DC),
            EncTable::from_spec(&huffman::STD_LUMA_AC),
            EncTable::from_spec(&huffman::STD_CHROMA_DC),
            EncTable::from_spec(&huffman::STD_CHROMA_AC),
        )
    }

    /// Appends an Adobe APP14 segment carrying `transform` (TN #5116).
    fn write_app14(out: &mut Vec<u8>, transform: u8) {
        marker::write_segment_header(out, marker::code::APP14, 14);
        out.extend_from_slice(b"Adobe");
        out.extend_from_slice(&[0x00, 0x64]); // version 100
        out.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // flags0, flags1
        out.push(transform);
    }

    /// A natural-order block with only the DC coefficient set — a flat block.
    fn flat(dc: i32) -> [i32; 64] {
        let mut b = [0i32; 64];
        b[0] = dc;
        b
    }

    /// A quantization table whose DC step is 8 (so a flat block of quantized DC `d` reconstructs to
    /// the uniform sample `128 + d`) and all other steps 1.
    fn dc8_quant() -> [u8; 64] {
        let mut q = [1u8; 64];
        q[0] = 8;
        q
    }

    #[test]
    fn exact_8x8_gray_block() {
        // A single 8×8 gray block with hand-chosen quantized coefficients decoded to exact pixels,
        // computed independently: dequantize (coeff · Q, natural order) → integer IDCT → +128 →
        // clamp.
        let quant = quant::LUMINANCE; // a non-trivial Annex K table exercises the AC dequant multiply
        let mut coeffs = [0i32; 64];
        coeffs[0] = 5; // DC
        coeffs[1] = -3; // first AC (natural index 1 = zig-zag position 1)
        coeffs[ZIGZAG[5]] = 2; // an AC further along the zig-zag
        coeffs[ZIGZAG[16]] = 1;

        // Independent expected pixels.
        let mut zz = [0i32; 64];
        for (i, cell) in zz.iter_mut().enumerate() {
            *cell = coeffs[i] * i32::from(quant[i]);
        }
        idct8x8_int(&mut zz);
        let expected: Vec<u8> = zz.iter().map(|&s| (s + 128).clamp(0, 255) as u8).collect();

        let (ldc, lac, ..) = std_enc();
        let body = entropy(&[(0, coeffs)], &[(ldc, lac)]);

        let mut jpeg = Vec::new();
        marker::write_marker(&mut jpeg, marker::code::SOI);
        quant::emit_dqt(&mut jpeg, &[(0, &quant)]);
        marker::write_sof0(&mut jpeg, 8, 8, &[(1, 1, 1, 0)]);
        huffman::emit_dht(
            &mut jpeg,
            &[(0, 0, &huffman::STD_LUMA_DC), (1, 0, &huffman::STD_LUMA_AC)],
        );
        marker::write_sos(&mut jpeg, &[(1, 0, 0)]);
        jpeg.extend_from_slice(&body);
        marker::write_marker(&mut jpeg, marker::code::EOI);

        let out: ImageBuf<Gray8> = JpegDecoder::new().decode_image(&jpeg).unwrap();
        assert_eq!(out.as_samples(), expected.as_slice());
    }

    /// Builds a 16×16 three-component 4:4:4 YCbCr stream. `scans` lists, per SOS, the `(cs, td, ta)`
    /// component triples; the entropy for each scan is built from the shared `grids` (per component,
    /// a 2×2 block grid in row-major order). This lets the interleaved and non-interleaved layouts
    /// be produced from identical coefficients.
    fn ycbcr444_16(grids: &[[[i32; 64]; 4]; 3], interleaved: bool) -> Vec<u8> {
        let (ldc, lac, cdc, cac) = std_enc();
        let mut jpeg = Vec::new();
        marker::write_marker(&mut jpeg, marker::code::SOI);
        marker::write_app0_jfif(&mut jpeg, marker::DensityUnit::AspectRatio, 1, 1);
        quant::emit_dqt(&mut jpeg, &[(0, &[1u8; 64]), (1, &[1u8; 64])]);
        marker::write_sof0(
            &mut jpeg,
            16,
            16,
            &[(1, 1, 1, 0), (2, 1, 1, 1), (3, 1, 1, 1)],
        );
        huffman::emit_dht(
            &mut jpeg,
            &[
                (0, 0, &huffman::STD_LUMA_DC),
                (1, 0, &huffman::STD_LUMA_AC),
                (0, 1, &huffman::STD_CHROMA_DC),
                (1, 1, &huffman::STD_CHROMA_AC),
            ],
        );
        // Component table pairs by frame index: Y uses luma tables, Cb/Cr chroma tables.
        let comp_tables = |ci: usize| {
            if ci == 0 {
                (ldc.clone(), lac.clone())
            } else {
                (cdc.clone(), cac.clone())
            }
        };
        if interleaved {
            marker::write_sos(&mut jpeg, &[(1, 0, 0), (2, 1, 1), (3, 1, 1)]);
            // MCU order: for each of the 4 MCUs (row-major), one block per component.
            let mut order = Vec::new();
            for mcu in 0..4 {
                for (ci, grid) in grids.iter().enumerate() {
                    order.push((ci, grid[mcu]));
                }
            }
            let tables = [comp_tables(0), comp_tables(1), comp_tables(2)];
            jpeg.extend_from_slice(&entropy(&order, &tables));
        } else {
            for (ci, grid) in grids.iter().enumerate() {
                let cs = (ci + 1) as u8;
                let (td, ta) = if ci == 0 { (0, 0) } else { (1, 1) };
                marker::write_sos(&mut jpeg, &[(cs, td, ta)]);
                let order: Vec<(usize, [i32; 64])> =
                    grid.iter().map(|blk| (0usize, *blk)).collect();
                jpeg.extend_from_slice(&entropy(&order, &[comp_tables(ci)]));
            }
        }
        marker::write_marker(&mut jpeg, marker::code::EOI);
        jpeg
    }

    /// Builds a 16×16 4:2:0 stream (Y at 2×2, Cb/Cr at 1×1) from a 4-block luma grid (row-major) and
    /// one Cb/one Cr block, as either one interleaved scan or three non-interleaved scans. This
    /// exercises the component-dimension arithmetic (§A.1.1) for a component with `Hi = Vi = 2`.
    fn ycbcr420_16(
        luma: [[i32; 64]; 4],
        cb: [i32; 64],
        cr: [i32; 64],
        interleaved: bool,
    ) -> Vec<u8> {
        let (ldc, lac, cdc, cac) = std_enc();
        let mut jpeg = Vec::new();
        marker::write_marker(&mut jpeg, marker::code::SOI);
        marker::write_app0_jfif(&mut jpeg, marker::DensityUnit::AspectRatio, 1, 1);
        quant::emit_dqt(&mut jpeg, &[(0, &[1u8; 64]), (1, &[1u8; 64])]);
        marker::write_sof0(
            &mut jpeg,
            16,
            16,
            &[(1, 2, 2, 0), (2, 1, 1, 1), (3, 1, 1, 1)],
        );
        huffman::emit_dht(
            &mut jpeg,
            &[
                (0, 0, &huffman::STD_LUMA_DC),
                (1, 0, &huffman::STD_LUMA_AC),
                (0, 1, &huffman::STD_CHROMA_DC),
                (1, 1, &huffman::STD_CHROMA_AC),
            ],
        );
        if interleaved {
            marker::write_sos(&mut jpeg, &[(1, 0, 0), (2, 1, 1), (3, 1, 1)]);
            // One MCU: the four luma blocks (row-major) then Cb then Cr.
            let order = vec![
                (0usize, luma[0]),
                (0, luma[1]),
                (0, luma[2]),
                (0, luma[3]),
                (1, cb),
                (2, cr),
            ];
            let tables = [
                (ldc.clone(), lac.clone()),
                (cdc.clone(), cac.clone()),
                (cdc, cac),
            ];
            jpeg.extend_from_slice(&entropy(&order, &tables));
        } else {
            marker::write_sos(&mut jpeg, &[(1, 0, 0)]);
            let luma_order: Vec<(usize, [i32; 64])> = luma.iter().map(|b| (0usize, *b)).collect();
            jpeg.extend_from_slice(&entropy(&luma_order, &[(ldc, lac.clone())]));
            marker::write_sos(&mut jpeg, &[(2, 1, 1)]);
            jpeg.extend_from_slice(&entropy(&[(0, cb)], &[(cdc.clone(), cac.clone())]));
            marker::write_sos(&mut jpeg, &[(3, 1, 1)]);
            jpeg.extend_from_slice(&entropy(&[(0, cr)], &[(cdc, cac)]));
        }
        marker::write_marker(&mut jpeg, marker::code::EOI);
        jpeg
    }

    #[test]
    fn non_interleaved_420_matches_interleaved() {
        // A subsampled (4:2:0) frame coded as one interleaved scan vs three non-interleaved scans
        // must decode identically — pinning the component-dimension arithmetic for Hi = Vi = 2.
        let mut luma = [[0i32; 64]; 4];
        for (i, blk) in luma.iter_mut().enumerate() {
            blk[0] = i as i32 * 4 - 6;
            blk[1] = i as i32 - 2;
        }
        let mut cb = [0i32; 64];
        cb[0] = 5;
        let mut cr = [0i32; 64];
        cr[0] = -7;
        let inter = ycbcr420_16(luma, cb, cr, true);
        let non = ycbcr420_16(luma, cb, cr, false);
        let a: ImageBuf<Rgb8> = JpegDecoder::new().decode_image(&inter).unwrap();
        let b: ImageBuf<Rgb8> = JpegDecoder::new().decode_image(&non).unwrap();
        assert_eq!(a, b);
        assert_eq!(a.dimensions(), gamut_core::Dimensions::new(16, 16).unwrap());
    }

    #[test]
    fn non_interleaved_three_scans_match_interleaved() {
        // Identical coefficients emitted as one interleaved scan vs three non-interleaved scans must
        // decode to the exact same image — pinning both the MCU walk and the multi-scan driver.
        let mut grids = [[[0i32; 64]; 4]; 3];
        for (ci, grid) in grids.iter_mut().enumerate() {
            for (b, blk) in grid.iter_mut().enumerate() {
                // Distinct DC per (component, block) plus a low AC, so block placement is testable.
                blk[0] = (ci as i32 * 7 + b as i32 * 3) - 6;
                blk[1] = ci as i32 - 1;
            }
        }
        let inter = ycbcr444_16(&grids, true);
        let non = ycbcr444_16(&grids, false);
        let a: ImageBuf<Rgb8> = JpegDecoder::new().decode_image(&inter).unwrap();
        let b: ImageBuf<Rgb8> = JpegDecoder::new().decode_image(&non).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn sof1_decodes_like_sof0() {
        // Flipping the SOF0 marker (0xC0) to SOF1 (0xC1) must not change the decoded image.
        let mut g = [[[0i32; 64]; 4]; 3];
        for (ci, grid) in g.iter_mut().enumerate() {
            for (b, blk) in grid.iter_mut().enumerate() {
                blk[0] = ci as i32 * 5 + b as i32;
            }
        }
        let base = ycbcr444_16(&g, true);
        let mut sof1 = base.clone();
        let idx = sof1.windows(2).position(|w| w == [0xFF, 0xC0]).unwrap();
        sof1[idx + 1] = 0xC1;
        let a: ImageBuf<Rgb8> = JpegDecoder::new().decode_image(&base).unwrap();
        let b: ImageBuf<Rgb8> = JpegDecoder::new().decode_image(&sof1).unwrap();
        assert_eq!(a, b);
    }

    /// Builds an 8×8 four-component stream (ids 1..4, all 1×1, quant [`dc8_quant`]) with flat blocks
    /// of quantized DC `d`, optionally tagged with an Adobe APP14 `transform`.
    fn four_comp_8x8(d: [i32; 4], app14: Option<u8>) -> Vec<u8> {
        let (ldc, lac, ..) = std_enc();
        let quant = dc8_quant();
        let mut jpeg = Vec::new();
        marker::write_marker(&mut jpeg, marker::code::SOI);
        if let Some(t) = app14 {
            write_app14(&mut jpeg, t);
        }
        quant::emit_dqt(&mut jpeg, &[(0, &quant)]);
        marker::write_sof0(
            &mut jpeg,
            8,
            8,
            &[(1, 1, 1, 0), (2, 1, 1, 0), (3, 1, 1, 0), (4, 1, 1, 0)],
        );
        huffman::emit_dht(
            &mut jpeg,
            &[(0, 0, &huffman::STD_LUMA_DC), (1, 0, &huffman::STD_LUMA_AC)],
        );
        marker::write_sos(&mut jpeg, &[(1, 0, 0), (2, 0, 0), (3, 0, 0), (4, 0, 0)]);
        let tables: Vec<(EncTable, EncTable)> =
            (0..4).map(|_| (ldc.clone(), lac.clone())).collect();
        let order: Vec<(usize, [i32; 64])> = (0..4).map(|ci| (ci, flat(d[ci]))).collect();
        jpeg.extend_from_slice(&entropy(&order, &tables));
        marker::write_marker(&mut jpeg, marker::code::EOI);
        jpeg
    }

    #[test]
    fn cmyk_four_component_passthrough() {
        // No APP14 → CMYK stored verbatim. Flat DCs 10/20/30/40 → uniform 138/148/158/168.
        let jpeg = four_comp_8x8([10, 20, 30, 40], None);
        let out: ImageBuf<Cmyk8> = JpegDecoder::new().decode_image(&jpeg).unwrap();
        assert_eq!(&out.as_samples()[..4], &[138, 148, 158, 168]);
        assert!(
            out.as_samples()
                .as_chunks::<4>()
                .0
                .iter()
                .all(|p| *p == [138, 148, 158, 168])
        );
    }

    #[test]
    fn ycck_four_component_inverts_to_cmyk() {
        // APP14 transform=2 → YCCK: the first three channels are inverse-YCbCr'd to RGB, then CMY =
        // 255 − RGB, K passes through.
        let d = [10, -20, 30, 40];
        let jpeg = four_comp_8x8(d, Some(2));
        let (y, cb, cr) = ((128 + d[0]) as u8, (128 + d[1]) as u8, (128 + d[2]) as u8);
        let (r, g, b) = ycbcr_to_rgb(y, cb, cr, ColorRange::Full);
        let expected = [255 - r, 255 - g, 255 - b, (128 + d[3]) as u8];
        let out: ImageBuf<Cmyk8> = JpegDecoder::new().decode_image(&jpeg).unwrap();
        assert_eq!(&out.as_samples()[..4], &expected);
    }

    #[test]
    fn decode_image_into_reuses_a_matching_cmyk_buffer() {
        // The Cmyk8 counterpart of the tests/decode.rs reuse tests (only unit tests can build a
        // four-component stream): a matching destination keeps its allocation, and the pixels
        // equal a fresh decode.
        let jpeg = four_comp_8x8([10, 20, 30, 40], None);
        let mut dst: ImageBuf<Cmyk8> = ImageBuf::zeroed(Dimensions::new(8, 8).unwrap()).unwrap();
        let ptr = dst.as_samples().as_ptr();
        JpegDecoder::new()
            .decode_image_into(&jpeg, &mut dst)
            .unwrap();
        assert_eq!(dst.as_samples().as_ptr(), ptr, "allocation must be reused");
        assert!(
            dst.as_samples()
                .as_chunks::<4>()
                .0
                .iter()
                .all(|p| *p == [138, 148, 158, 168])
        );

        // A mismatched destination falls back to replacement with the right dimensions.
        let mut small: ImageBuf<Cmyk8> = ImageBuf::zeroed(Dimensions::new(2, 2).unwrap()).unwrap();
        JpegDecoder::new()
            .decode_image_into(&jpeg, &mut small)
            .unwrap();
        assert_eq!(small.dimensions(), Dimensions::new(8, 8).unwrap());
        assert_eq!(&small.as_samples()[..4], &[138, 148, 158, 168]);
    }

    #[test]
    fn decode_image_into_converts_a_gray_stream_into_a_matching_rgb_buffer() {
        // The native layout (Gray8) is not the requested one (Rgb8), so `present_into` must go
        // through `gamut_core::convert` even though `dst` already has the decoded dimensions. Two
        // blocks of different level make the rows non-uniform, so a gray row written straight into
        // the RGB buffer would land samples of the second block under the first block's pixels.
        let jpeg = gray_stream(16, 8, &[flat(-40), flat(40)], None);
        let mut dst: ImageBuf<Rgb8> = ImageBuf::zeroed(Dimensions::new(16, 8).unwrap()).unwrap();
        let ptr = dst.as_samples().as_ptr();
        JpegDecoder::new()
            .decode_image_into(&jpeg, &mut dst)
            .unwrap();
        assert_eq!(dst.as_samples().as_ptr(), ptr, "allocation must be reused");
        let row: Vec<u8> = [[88u8; 3]; 8]
            .into_iter()
            .chain([[168u8; 3]; 8])
            .flatten()
            .collect();
        assert_eq!(dst.as_samples(), row.repeat(8));
    }

    #[test]
    fn column_map_picks_the_path_for_each_ratio() {
        // The three paths agree on every output (next test), so only the choice itself can show
        // which ratio takes which path.
        assert_eq!(ColumnMap::new(1, 1, 5), ColumnMap::Direct);
        assert_eq!(ColumnMap::new(2, 2, 5), ColumnMap::Direct);
        assert_eq!(ColumnMap::new(1, 2, 5), ColumnMap::Double);
        assert_eq!(ColumnMap::new(2, 4, 5), ColumnMap::Double);
        assert_eq!(
            ColumnMap::new(1, 4, 5),
            ColumnMap::Indexed(vec![0, 0, 0, 0, 1])
        );
        assert_eq!(
            ColumnMap::new(2, 3, 5),
            ColumnMap::Indexed(vec![0, 0, 1, 2, 2])
        );
    }

    #[test]
    fn column_map_paths_replicate_like_the_index_formula() {
        // Every path against `px * h / hmax` itself, at odd and even widths, from a plane row
        // padded past the component's width (as a block-padded plane is) with samples that differ
        // at every column, so reading one column off or past the valid region moves an output.
        for (h, hmax) in [(1, 1), (2, 2), (1, 2), (2, 4), (1, 4), (1, 3), (2, 3)] {
            for width in 1usize..=19 {
                let comp_w = (width * h).div_ceil(hmax);
                let src: Vec<u8> = (0..comp_w + 8).map(|i| (i * 37 + 11) as u8).collect();
                let want: Vec<u8> = (0..width).map(|px| src[px * h / hmax]).collect();
                let mut scratch = vec![0u8; width];
                let got = ColumnMap::new(h, hmax, width).upsample(&src, &mut scratch);
                assert_eq!(got, want, "h {h} of {hmax}, width {width}");
            }
        }
    }

    #[test]
    fn ycbcr_row_matches_the_per_pixel_conversion() {
        // Short rows (where the overlapping stores and the last pixel's three-byte store meet) and
        // rows past a vector's width, with the three planes varied independently so a lane, plane
        // or byte-order mix-up moves a sample, and a scratch row holding junk so every pixel's
        // packed value must be rewritten before it is stored.
        for len in [0usize, 1, 2, 3, 4, 5, 15, 16, 17, 33] {
            let y: Vec<u8> = (0..len).map(|i| (i * 97 + 3) as u8).collect();
            let cb: Vec<u8> = (0..len).map(|i| (i * 53 + 200) as u8).collect();
            let cr: Vec<u8> = (0..len).map(|i| (i * 29 + 90) as u8).collect();
            let want: Vec<u8> = (0..len)
                .flat_map(|i| {
                    let (r, g, b) = ycbcr_to_rgb(y[i], cb[i], cr[i], ColorRange::Full);
                    [r, g, b]
                })
                .collect();
            let mut packed = vec![u32::MAX; len];
            let mut out = vec![0u8; 3 * len];
            ycbcr_to_rgb_row(&y, &cb, &cr, &mut packed, &mut out);
            assert_eq!(out, want, "len {len}");
        }
    }

    /// Builds an 8×8 three-component stream (given component ids, all 1×1, quant [`dc8_quant`]) with
    /// flat blocks, optionally tagged with a JFIF APP0 or Adobe APP14.
    fn three_comp_8x8(ids: [u8; 3], d: [i32; 3], jfif: bool, app14: Option<u8>) -> Vec<u8> {
        let (ldc, lac, ..) = std_enc();
        let quant = dc8_quant();
        let mut jpeg = Vec::new();
        marker::write_marker(&mut jpeg, marker::code::SOI);
        if jfif {
            marker::write_app0_jfif(&mut jpeg, marker::DensityUnit::AspectRatio, 1, 1);
        }
        if let Some(t) = app14 {
            write_app14(&mut jpeg, t);
        }
        quant::emit_dqt(&mut jpeg, &[(0, &quant)]);
        marker::write_sof0(
            &mut jpeg,
            8,
            8,
            &[(ids[0], 1, 1, 0), (ids[1], 1, 1, 0), (ids[2], 1, 1, 0)],
        );
        huffman::emit_dht(
            &mut jpeg,
            &[(0, 0, &huffman::STD_LUMA_DC), (1, 0, &huffman::STD_LUMA_AC)],
        );
        marker::write_sos(&mut jpeg, &[(ids[0], 0, 0), (ids[1], 0, 0), (ids[2], 0, 0)]);
        let tables: Vec<(EncTable, EncTable)> =
            (0..3).map(|_| (ldc.clone(), lac.clone())).collect();
        let order: Vec<(usize, [i32; 64])> = (0..3).map(|ci| (ci, flat(d[ci]))).collect();
        jpeg.extend_from_slice(&entropy(&order, &tables));
        marker::write_marker(&mut jpeg, marker::code::EOI);
        jpeg
    }

    #[test]
    fn adobe_transform0_three_component_is_rgb_passthrough() {
        // APP14 transform=0 with 3 components → the samples are R,G,B directly (no colour transform).
        let d = [10, 20, 30];
        let jpeg = three_comp_8x8([1, 2, 3], d, false, Some(0));
        let out: ImageBuf<Rgb8> = JpegDecoder::new().decode_image(&jpeg).unwrap();
        assert_eq!(&out.as_samples()[..3], &[138, 148, 158]);
    }

    #[test]
    fn component_ids_rgb_without_app14_is_rgb() {
        // Component ids 'R','G','B' and no JFIF/Adobe → RGB passthrough (the de-facto rule).
        let jpeg = three_comp_8x8(*b"RGB", [10, 20, 30], false, None);
        let out: ImageBuf<Rgb8> = JpegDecoder::new().decode_image(&jpeg).unwrap();
        assert_eq!(&out.as_samples()[..3], &[138, 148, 158]);
    }

    #[test]
    fn jfif_three_component_is_ycbcr() {
        // A JFIF APP0 (and ids 1,2,3, no Adobe) selects the YCbCr transform.
        let d = [10, -20, 30];
        let jpeg = three_comp_8x8([1, 2, 3], d, true, None);
        let (r, g, b) = ycbcr_to_rgb(
            (128 + d[0]) as u8,
            (128 + d[1]) as u8,
            (128 + d[2]) as u8,
            ColorRange::Full,
        );
        let out: ImageBuf<Rgb8> = JpegDecoder::new().decode_image(&jpeg).unwrap();
        assert_eq!(&out.as_samples()[..3], &[r, g, b]);
    }

    #[test]
    fn jfif_overrides_rgb_component_ids() {
        // Component ids 'R','G','B' would default to RGB, but a JFIF APP0 forces YCbCr — the decision
        // tree checks JFIF before the id heuristic. This pins that the APP0 flag is actually read.
        let d = [10, -20, 30];
        let jpeg = three_comp_8x8(*b"RGB", d, true, None);
        let (r, g, b) = ycbcr_to_rgb(
            (128 + d[0]) as u8,
            (128 + d[1]) as u8,
            (128 + d[2]) as u8,
            ColorRange::Full,
        );
        let out: ImageBuf<Rgb8> = JpegDecoder::new().decode_image(&jpeg).unwrap();
        // Not the RGB passthrough [138, 108, 158]; the YCbCr conversion instead.
        assert_eq!(&out.as_samples()[..3], &[r, g, b]);
    }

    #[test]
    fn undefined_table_and_bad_component_are_rejected() {
        // A SOS whose component references an undefined quant table (Tq=2 never DQT'd) is rejected.
        let (ldc, lac, ..) = std_enc();
        let mut jpeg = Vec::new();
        marker::write_marker(&mut jpeg, marker::code::SOI);
        quant::emit_dqt(&mut jpeg, &[(0, &dc8_quant())]);
        marker::write_sof0(&mut jpeg, 8, 8, &[(1, 1, 1, 2)]); // Tq=2, never defined
        huffman::emit_dht(
            &mut jpeg,
            &[(0, 0, &huffman::STD_LUMA_DC), (1, 0, &huffman::STD_LUMA_AC)],
        );
        marker::write_sos(&mut jpeg, &[(1, 0, 0)]);
        jpeg.extend_from_slice(&entropy(&[(0, flat(1))], &[(ldc, lac)]));
        marker::write_marker(&mut jpeg, marker::code::EOI);
        assert!(
            <JpegDecoder as DecodeImage<Gray8>>::decode_image(&JpegDecoder::new(), &jpeg).is_err()
        );
    }

    #[test]
    fn ac_index_past_63_is_rejected() {
        // A block whose AC run overruns coefficient 63 is malformed: three ZRLs advance the index to
        // 49, then a run/size symbol 0xF1 (run 15, size 1) pushes it to 64 > 63 — a hard reject.
        let (ldc, lac, ..) = std_enc();
        let mut body = Vec::new();
        let mut w = BitWriter::new(&mut body);
        // DC category 0.
        let (c, l) = ldc.lookup(0).unwrap();
        w.write_bits(c, l);
        // Three ZRLs → coefficient index 1 + 3·16 = 49.
        for _ in 0..3 {
            let (c, l) = lac.lookup(0xF0).unwrap();
            w.write_bits(c, l);
        }
        // Symbol 0xF1: run 15 pushes the index to 49 + 15 = 64, past the last coefficient.
        let (c, l) = lac.lookup(0xF1).unwrap();
        w.write_bits(c, l);
        w.write_bits(1, 1);
        w.flush();

        let mut jpeg = Vec::new();
        marker::write_marker(&mut jpeg, marker::code::SOI);
        quant::emit_dqt(&mut jpeg, &[(0, &dc8_quant())]);
        marker::write_sof0(&mut jpeg, 8, 8, &[(1, 1, 1, 0)]);
        huffman::emit_dht(
            &mut jpeg,
            &[(0, 0, &huffman::STD_LUMA_DC), (1, 0, &huffman::STD_LUMA_AC)],
        );
        marker::write_sos(&mut jpeg, &[(1, 0, 0)]);
        jpeg.extend_from_slice(&body);
        marker::write_marker(&mut jpeg, marker::code::EOI);
        assert!(
            <JpegDecoder as DecodeImage<Gray8>>::decode_image(&JpegDecoder::new(), &jpeg).is_err()
        );
    }

    /// Builds a single-component (gray) stream `x` wide with `y_field` in the SOF `Y` field, coding
    /// the flat blocks `blocks` (one block column, top to bottom), optionally followed by a raw DNL
    /// segment (`dnl_len`/`dnl_nl`) and always closed by EOI.
    fn gray_stream(x: u16, y_field: u16, blocks: &[[i32; 64]], dnl: Option<(u16, u16)>) -> Vec<u8> {
        let (ldc, lac, ..) = std_enc();
        let mut jpeg = Vec::new();
        marker::write_marker(&mut jpeg, marker::code::SOI);
        quant::emit_dqt(&mut jpeg, &[(0, &dc8_quant())]);
        marker::write_sof0(&mut jpeg, x, y_field, &[(1, 1, 1, 0)]);
        huffman::emit_dht(
            &mut jpeg,
            &[(0, 0, &huffman::STD_LUMA_DC), (1, 0, &huffman::STD_LUMA_AC)],
        );
        marker::write_sos(&mut jpeg, &[(1, 0, 0)]);
        let order: Vec<(usize, [i32; 64])> = blocks.iter().map(|b| (0usize, *b)).collect();
        jpeg.extend_from_slice(&entropy(&order, &[(ldc, lac)]));
        if let Some((len, nl)) = dnl {
            marker::write_segment_header(&mut jpeg, marker::code::DNL, usize::from(len));
            jpeg.extend_from_slice(&nl.to_be_bytes());
        }
        marker::write_marker(&mut jpeg, marker::code::EOI);
        jpeg
    }

    #[test]
    fn zero_height_resolved_by_dnl() {
        // Y=0 in the SOF, two flat block rows decoded until the entropy ends, then DNL(16) supplies
        // the height. Exercises the unknown-row MCU loop, the marker-boundary end detection, and the
        // DNL height application (§B.2.5). A fill 0xFF is inserted before the DNL marker so the
        // end-of-data peek must skip it (§B.1.1.2).
        let mut jpeg = gray_stream(8, 0, &[flat(10), flat(-30)], Some((4, 16)));
        let dnl = jpeg.windows(2).position(|w| w == [0xFF, 0xDC]).unwrap();
        jpeg.insert(dnl, 0xFF); // fill byte: now 0xFF 0xFF 0xDC
        let out: ImageBuf<Gray8> = JpegDecoder::new().decode_image(&jpeg).unwrap();
        assert_eq!(
            out.dimensions(),
            gamut_core::Dimensions::new(8, 16).unwrap()
        );
        assert!(out.as_samples()[..64].iter().all(|&s| s == 138)); // top block: 128 + 10
        assert!(out.as_samples()[64..].iter().all(|&s| s == 98)); // bottom block: 128 − 30
    }

    #[test]
    fn zero_height_without_dnl_is_rejected() {
        // Y=0 and the scan ends at EOI with no DNL: the height is never defined → InvalidInput.
        let jpeg = gray_stream(8, 0, &[flat(5)], None);
        assert!(
            <JpegDecoder as DecodeImage<Gray8>>::decode_image(&JpegDecoder::new(), &jpeg).is_err()
        );
    }

    #[test]
    fn deferred_height_limits_are_exact_and_bound_missing_dnl_growth() {
        let resolved = gray_stream(8, 0, &[flat(5), flat(6)], Some((4, 16)));
        assert!(
            DecodeImage::<Gray8>::decode_image(
                &JpegDecoder::new().with_max_dimensions(8, 16),
                &resolved,
            )
            .is_ok()
        );
        assert_eq!(
            err_msg(DecodeImage::<Gray8>::decode_image(
                &JpegDecoder::new().with_max_dimensions(8, 15),
                &resolved,
            )),
            "JPEG: image exceeds the dimension limit"
        );
        assert!(
            DecodeImage::<Gray8>::decode_image(
                &JpegDecoder::new().with_max_image_bytes(8 * 16),
                &resolved,
            )
            .is_ok()
        );
        assert_eq!(
            err_msg(DecodeImage::<Gray8>::decode_image(
                &JpegDecoder::new().with_max_image_bytes(8 * 16 - 1),
                &resolved,
            )),
            "JPEG: image exceeds the size limit"
        );

        // With no DNL, the configured ceiling must stop a third entropy row before the later
        // "height never defined" error. This makes the allocation guard itself observable.
        let no_dnl = gray_stream(8, 0, &[flat(1), flat(2), flat(3)], None);
        assert_eq!(
            err_msg(DecodeImage::<Gray8>::decode_image(
                &JpegDecoder::new().with_max_dimensions(8, 8),
                &no_dnl,
            )),
            "JPEG: image exceeds the dimension limit"
        );
        assert_eq!(
            err_msg(DecodeImage::<Gray8>::decode_image(
                &JpegDecoder::new().with_max_image_bytes(8 * 8),
                &no_dnl,
            )),
            "JPEG: image exceeds the size limit"
        );
    }

    #[test]
    fn deferred_scan_row_geometry_covers_both_scan_shapes() {
        use crate::syntax::{FrameComponent, ScanComponent, ScanHeader};

        let frame = Frame {
            x: 16,
            y: 0,
            components: vec![
                FrameComponent {
                    id: 1,
                    h: 2,
                    v: 2,
                    tq: 0,
                },
                FrameComponent {
                    id: 2,
                    h: 1,
                    v: 1,
                    tq: 0,
                },
                FrameComponent {
                    id: 3,
                    h: 1,
                    v: 1,
                    tq: 0,
                },
            ],
        };
        let component = |frame_index| ScanComponent {
            frame_index,
            td: 0,
            ta: 0,
        };
        let scan = |components| ScanHeader {
            components,
            ss: 0,
            se: 63,
            ah: 0,
            al: 0,
        };
        let interleaved = scan(vec![component(0), component(1), component(2)]);
        assert_eq!(scan_mcu_rows(&frame, &interleaved, 16), 1);
        assert_eq!(scan_mcu_rows(&frame, &interleaved, 17), 2);

        let luma_only = scan(vec![component(0)]);
        assert_eq!(scan_mcu_rows(&frame, &luma_only, 16), 2);
        assert_eq!(scan_mcu_rows(&frame, &luma_only, 17), 3);

        let chroma_only = scan(vec![component(1)]);
        assert_eq!(scan_mcu_rows(&frame, &chroma_only, 16), 1);
        assert_eq!(scan_mcu_rows(&frame, &chroma_only, 17), 2);
    }

    #[test]
    fn deferred_row_limit_uses_the_tighter_budget_and_dimension_precedence_on_ties() {
        use crate::syntax::{FrameComponent, ScanComponent, ScanHeader};

        let frame = Frame {
            x: 8,
            y: 0,
            components: vec![FrameComponent {
                id: 1,
                h: 1,
                v: 1,
                tq: 0,
            }],
        };
        let scan = ScanHeader {
            components: vec![ScanComponent {
                frame_index: 0,
                td: 0,
                ta: 0,
            }],
            ss: 0,
            se: 63,
            ah: 0,
            al: 0,
        };
        let selected = |decoder: JpegDecoder| {
            let limit = decoder.deferred_row_limit(&frame, &scan).unwrap().unwrap();
            (limit.rows, limit.message)
        };

        assert_eq!(
            selected(
                JpegDecoder::new()
                    .with_max_dimensions(8, 24)
                    .with_max_image_bytes(8 * 8),
            ),
            (1, "JPEG: image exceeds the size limit")
        );
        assert_eq!(
            selected(
                JpegDecoder::new()
                    .with_max_dimensions(8, 8)
                    .with_max_image_bytes(8 * 24),
            ),
            (1, "JPEG: image exceeds the dimension limit")
        );
        assert_eq!(
            selected(
                JpegDecoder::new()
                    .with_max_dimensions(8, 8)
                    .with_max_image_bytes(8 * 8),
            ),
            (1, "JPEG: image exceeds the dimension limit")
        );
    }

    #[test]
    fn dnl_is_ignored_when_height_is_known() {
        // A DNL after a normal Y≠0 frame is advisory and ignored (§B.2.5): the image still decodes at
        // the SOF-declared height.
        let jpeg = gray_stream(8, 8, &[flat(20)], Some((4, 999)));
        let out: ImageBuf<Gray8> = JpegDecoder::new().decode_image(&jpeg).unwrap();
        assert_eq!(out.dimensions(), gamut_core::Dimensions::new(8, 8).unwrap());
        // A malformed DNL (NL=0) is rejected even though the height is already known.
        let bad = gray_stream(8, 8, &[flat(20)], Some((4, 0)));
        assert!(
            <JpegDecoder as DecodeImage<Gray8>>::decode_image(&JpegDecoder::new(), &bad).is_err()
        );
        // A DNL with the wrong segment length is rejected.
        let bad_len = gray_stream(8, 8, &[flat(20)], Some((5, 8)));
        assert!(
            <JpegDecoder as DecodeImage<Gray8>>::decode_image(&JpegDecoder::new(), &bad_len)
                .is_err()
        );
    }

    #[test]
    fn fill_bytes_before_marker_are_skipped() {
        // A fill 0xFF before the terminating EOI (§B.1.1.2) must be skipped by the entropy reader.
        let mut jpeg = gray_stream(8, 8, &[flat(7)], None);
        let eoi = jpeg.len() - 2; // the 0xFF of the final EOI
        jpeg.insert(eoi, 0xFF); // now 0xFF 0xFF 0xD9
        let out: ImageBuf<Gray8> = JpegDecoder::new().decode_image(&jpeg).unwrap();
        assert!(out.as_samples().iter().all(|&s| s == 135)); // 128 + 7
    }

    #[test]
    fn four_component_cannot_present_as_rgb() {
        // The Rgb8 impl rejects a 4-component (CMYK) stream — the caller must use Cmyk8.
        let jpeg = four_comp_8x8([10, 20, 30, 40], None);
        assert!(
            <JpegDecoder as DecodeImage<Rgb8>>::decode_image(&JpegDecoder::new(), &jpeg).is_err()
        );
    }

    #[test]
    fn unsupported_and_structural_markers_are_classified() {
        let base = gray_stream(8, 8, &[flat(3)], None);
        let sof = base.windows(2).position(|w| w == [0xFF, 0xC0]).unwrap();
        let flip = |code: u8| {
            let mut m = base.clone();
            m[sof + 1] = code;
            m
        };
        // info() distinguishes each supported process by its SOFn marker.
        assert_eq!(info(&base).unwrap().process, JpegProcess::Baseline);
        assert_eq!(
            info(&flip(0xC1)).unwrap().process,
            JpegProcess::ExtendedSequential
        );
        assert_eq!(info(&flip(0xC2)).unwrap().process, JpegProcess::Progressive);
        // The unsupported processes (lossless, hierarchical, arithmetic) decode to a specific
        // `Unsupported` error (not merely any error). SOF2 (0xC2) is now a supported process, so it
        // is excluded here: a baseline SOS under a SOF2 header fails the progressive DC-scan check
        // (Se ≠ 0) as InvalidInput, exercised by the progressive-validation tests instead.
        for code in [0xC3u8, 0xC5, 0xC9, 0xCC, 0xCF] {
            let m = flip(code);
            assert!(
                matches!(
                    <JpegDecoder as DecodeImage<Gray8>>::decode_image(&JpegDecoder::new(), &m),
                    Err(error) if error.kind() == gamut_core::ErrorKind::Unsupported
                ),
                "marker {code:#x} must be Unsupported"
            );
        }
        // info() reports Unsupported for an unsupported process, InvalidInput for a scan-before-frame
        // stream (SOS with no preceding SOF).
        assert!(matches!(
            info(&flip(0xC3)),
            Err(error) if error.kind() == gamut_core::ErrorKind::Unsupported
        ));
        let early = [0xFF, 0xD8, 0xFF, 0xDA, 0x00, 0x08, 1, 1, 0x00, 0, 63, 0];
        assert!(matches!(
            info(&early),
            Err(error) if error.kind() == gamut_core::ErrorKind::InvalidInput
        ));

        // Duplicate SOF: splice a second SOF0 segment before SOS → InvalidInput.
        let sos = base.windows(2).position(|w| w == [0xFF, 0xDA]).unwrap();
        let mut dup = base.clone();
        let mut extra = Vec::new();
        marker::write_sof0(&mut extra, 8, 8, &[(1, 1, 1, 0)]);
        dup.splice(sos..sos, extra);
        assert!(matches!(
            <JpegDecoder as DecodeImage<Gray8>>::decode_image(&JpegDecoder::new(), &dup),
            Err(error) if error.kind() == gamut_core::ErrorKind::InvalidInput
        ));
    }

    #[test]
    fn corrupt_soi_on_otherwise_valid_stream_is_rejected() {
        // The rest of the stream is valid, so only the SOI check can reject it — pinning that the
        // first two bytes really are validated as 0xFF 0xD8.
        let mut s = gray_stream(8, 8, &[flat(5)], None);
        s[0] = 0x00;
        s[1] = 0x00;
        assert!(
            <JpegDecoder as DecodeImage<Gray8>>::decode_image(&JpegDecoder::new(), &s).is_err()
        );
    }

    #[test]
    fn empty_advisory_segment_is_accepted() {
        // A length-2 (empty payload) COM segment is valid and must not be rejected by the segment
        // length check (`len < 2` boundary).
        let mut s = gray_stream(8, 8, &[flat(9)], None);
        let dqt = s.windows(2).position(|w| w == [0xFF, 0xDB]).unwrap();
        s.splice(dqt..dqt, [0xFF, 0xFE, 0x00, 0x02]); // COM marker, empty payload
        let out: ImageBuf<Gray8> = JpegDecoder::new().decode_image(&s).unwrap();
        assert!(out.as_samples().iter().all(|&v| v == 137)); // 128 + 9
    }

    #[test]
    fn dnl_claiming_more_lines_than_coded_is_rejected() {
        // Y=0, one 8-row block decoded, then DNL claims 100 lines — more than were coded. The
        // plane-coverage check in `assemble` must reject it (a lenient `&&` there would read past the
        // plane).
        let jpeg = gray_stream(8, 0, &[flat(5)], Some((4, 100)));
        assert!(
            <JpegDecoder as DecodeImage<Gray8>>::decode_image(&JpegDecoder::new(), &jpeg).is_err()
        );
    }

    #[test]
    fn dc_category_11_is_accepted() {
        // A DC difference of category 11 (magnitude 1024–2047) is the largest legal 8-bit DC
        // category; the `> 11` guard must admit it. Quantized DC 1024 · step 8 → 8192, IDCT flat →
        // 1024, +128 → clamps to 255.
        let jpeg = gray_stream(8, 8, &[flat(1024)], None);
        let out: ImageBuf<Gray8> = JpegDecoder::new().decode_image(&jpeg).unwrap();
        assert!(out.as_samples().iter().all(|&v| v == 255));
    }

    #[test]
    fn crafted_table_and_scan_segments_are_validated() {
        let d =
            |b: &[u8]| <JpegDecoder as DecodeImage<Gray8>>::decode_image(&JpegDecoder::new(), b);
        // DHT with class Tc=2 (only 0/1 valid).
        let mut dht = Vec::new();
        marker::write_marker(&mut dht, marker::code::SOI);
        quant::emit_dqt(&mut dht, &[(0, &dc8_quant())]);
        marker::write_sof0(&mut dht, 8, 8, &[(1, 1, 1, 0)]);
        // A hand-written DHT segment with Tc=2: length 2 + 1 + 16 + 0 = 19, no values.
        marker::write_segment_header(&mut dht, marker::code::DHT, 19);
        dht.push(0x20); // Tc=2, Th=0
        dht.extend_from_slice(&[0u8; 16]);
        marker::write_sos(&mut dht, &[(1, 0, 0)]);
        marker::write_marker(&mut dht, marker::code::EOI);
        assert!(d(&dht).is_err(), "DHT Tc=2");

        // DRI with a 5-byte (wrong) length.
        let mut dri = Vec::new();
        marker::write_marker(&mut dri, marker::code::SOI);
        marker::write_segment_header(&mut dri, marker::code::DRI, 5);
        dri.extend_from_slice(&[0, 1, 0]);
        marker::write_marker(&mut dri, marker::code::EOI);
        assert!(d(&dri).is_err(), "DRI bad length");

        // A 16-bit DQT (Pq=1) carrying a zero value is rejected.
        let mut dqt = Vec::new();
        marker::write_marker(&mut dqt, marker::code::SOI);
        marker::write_segment_header(&mut dqt, marker::code::DQT, 2 + 1 + 128);
        dqt.push(0x10); // Pq=1, Tq=0
        dqt.extend_from_slice(&[0u8; 128]); // all-zero 16-bit values
        marker::write_marker(&mut dqt, marker::code::EOI);
        assert!(d(&dqt).is_err(), "16-bit DQT zero value");

        // SOS with a duplicate component selector.
        let mut sos = Vec::new();
        marker::write_marker(&mut sos, marker::code::SOI);
        quant::emit_dqt(&mut sos, &[(0, &dc8_quant())]);
        marker::write_sof0(&mut sos, 8, 8, &[(1, 1, 1, 0), (2, 1, 1, 0)]);
        huffman::emit_dht(
            &mut sos,
            &[(0, 0, &huffman::STD_LUMA_DC), (1, 0, &huffman::STD_LUMA_AC)],
        );
        marker::write_sos(&mut sos, &[(1, 0, 0), (1, 0, 0)]); // component 1 twice
        marker::write_marker(&mut sos, marker::code::EOI);
        assert!(d(&sos).is_err(), "duplicate SOS component");

        // SOS appearing before any SOF.
        let early = [0xFF, 0xD8, 0xFF, 0xDA, 0x00, 0x08, 1, 1, 0x00, 0, 63, 0];
        assert!(d(&early).is_err(), "SOS before SOF");

        // A SOS with Ns=0 (empty component list) is rejected — a lenient count check would let it
        // through to a zero-component scan.
        let mut ns0 = Vec::new();
        marker::write_marker(&mut ns0, marker::code::SOI);
        quant::emit_dqt(&mut ns0, &[(0, &dc8_quant())]);
        marker::write_sof0(&mut ns0, 8, 8, &[(1, 1, 1, 0)]);
        huffman::emit_dht(
            &mut ns0,
            &[(0, 0, &huffman::STD_LUMA_DC), (1, 0, &huffman::STD_LUMA_AC)],
        );
        // Hand-written SOS: Ls=6, Ns=0, Ss=0, Se=63, Ah|Al=0.
        ns0.extend_from_slice(&[0xFF, marker::code::SOS, 0x00, 0x06, 0x00, 0x00, 0x3F, 0x00]);
        marker::write_marker(&mut ns0, marker::code::EOI);
        assert!(d(&ns0).is_err(), "SOS Ns=0");
    }
    /// The static message carried by a decode error (`"ok"` for success) — used where a mutant
    /// would change WHICH error is reported, not merely whether one is.
    fn err_msg<T>(r: gamut_core::Result<T>) -> &'static str {
        match r {
            Err(error)
                if matches!(
                    error.kind(),
                    gamut_core::ErrorKind::InvalidInput | gamut_core::ErrorKind::Unsupported
                ) =>
            {
                error.static_message().unwrap_or("other")
            }
            Err(_) => "other",
            Ok(_) => "ok",
        }
    }

    /// Decodes as Gray8, returning the error message (or `"ok"`).
    fn gray_msg(data: &[u8]) -> &'static str {
        err_msg(<JpegDecoder as DecodeImage<Gray8>>::decode_image(
            &JpegDecoder::new(),
            data,
        ))
    }

    #[test]
    fn soi_validation_is_exact() {
        // A bare SOI passes the SOI check and fails at the NEXT marker read with that read's own
        // message — pinning the `len < 2` boundary (an off-by-one would misreport "missing SOI").
        assert_eq!(gray_msg(&[0xFF, 0xD8]), "JPEG: expected a marker");
        // Each half of the SOI check rejects independently, on an otherwise fully valid stream:
        // right prefix byte + wrong code, and wrong prefix byte + right code. A weakened check
        // would let either stream decode successfully.
        let mut not_soi = gray_stream(8, 8, &[flat(5)], None);
        not_soi[1] = 0xD0;
        assert_eq!(gray_msg(&not_soi), "JPEG: missing SOI marker");
        let mut not_ff = gray_stream(8, 8, &[flat(5)], None);
        not_ff[0] = 0x00;
        assert_eq!(gray_msg(&not_ff), "JPEG: missing SOI marker");
    }

    #[test]
    fn truncation_after_entropy_reports_missing_marker_exactly() {
        // flat(64) codes to exactly 16 bits (cat-7 DC: 5+7, EOB: 4), so the scan consumes its final
        // data byte completely. Stripping the EOI must yield the end-of-scan message — an eagerly
        // over-fetching bit reader (fetching when the buffered bits already suffice) would hit EOF
        // mid-block and misreport truncated entropy data instead.
        let full = gray_stream(8, 8, &[flat(64)], None);
        assert_eq!(
            gray_msg(&full[..full.len() - 2]),
            "JPEG: missing marker after scan"
        );
    }

    #[test]
    fn missing_block_decodes_from_all_ones_padding() {
        // 8×16 gray declares two blocks but the entropy codes only one: the second block is decoded
        // from the reader's 1-padding past the EOI marker (§F.2.2.5) and must fail as an undecodable
        // all-ones Huffman walk. Pins the padding VALUE — corrupted padding (0-bits) would decode
        // spurious category-0 blocks and succeed.
        let (ldc, lac, ..) = std_enc();
        let mut jpeg = Vec::new();
        marker::write_marker(&mut jpeg, marker::code::SOI);
        quant::emit_dqt(&mut jpeg, &[(0, &dc8_quant())]);
        marker::write_sof0(&mut jpeg, 8, 16, &[(1, 1, 1, 0)]);
        huffman::emit_dht(
            &mut jpeg,
            &[(0, 0, &huffman::STD_LUMA_DC), (1, 0, &huffman::STD_LUMA_AC)],
        );
        marker::write_sos(&mut jpeg, &[(1, 0, 0)]);
        jpeg.extend_from_slice(&entropy(&[(0, flat(7))], &[(ldc, lac)]));
        marker::write_marker(&mut jpeg, marker::code::EOI);
        assert_eq!(gray_msg(&jpeg), "JPEG: undecodable Huffman code");
    }

    /// Searches for a flat-DC + tail-coefficient block whose entropy segment ends in a stuffed
    /// `0xFF 0x00` pair (the 1-padding completes the final byte to `0xFF`, which the writer stuffs).
    /// Returns the block; panics if no combination in the small search space aligns.
    fn block_with_stuffed_tail() -> [i32; 64] {
        let (ldc, lac, ..) = std_enc();
        for dc in 0..512 {
            for tail in [1i32, 3, 7, 15, 31] {
                let mut b = [0i32; 64];
                b[0] = dc;
                b[63] = tail; // natural 63 == zig-zag 63: last coefficient, suppresses EOB
                let mut out = Vec::new();
                let mut w = BitWriter::new(&mut out);
                emit_block(&mut w, &b, &mut 0, &ldc, &lac);
                w.flush();
                if out.len() >= 2 && out[out.len() - 2..] == [0xFF, 0x00] {
                    return b;
                }
            }
        }
        panic!("no block aligned to a stuffed 0xFF tail");
    }

    #[test]
    fn dnl_end_detection_survives_stuffed_tail_and_fill_bytes() {
        // A Y=0 frame whose entropy ends in a stuffed 0xFF 0x00 pair, with a fill 0xFF before the
        // DNL marker: the end-of-data peek must byte-align past the stuffed pair, skip the fill, and
        // stop at DNL. This makes the peek's forward lookahead and fill-skip both load-bearing (a
        // backwards peek would see the stuffed 0x00 and keep decoding).
        let block = block_with_stuffed_tail();
        let mut jpeg = gray_stream(8, 0, &[block], Some((4, 8)));
        let dnl = jpeg.windows(2).position(|w| w == [0xFF, 0xDC]).unwrap();
        jpeg.insert(dnl, 0xFF); // fill byte: 0xFF 0xFF 0xDC
        let out: ImageBuf<Gray8> = JpegDecoder::new().decode_image(&jpeg).unwrap();
        assert_eq!(out.dimensions(), gamut_core::Dimensions::new(8, 8).unwrap());
    }

    /// Builds a single-component (gray) 8-wide stream with restart interval 1 (an RSTm between every
    /// MCU row), coding one flat block per row (predictor resets each interval). `y_field` goes in the
    /// SOF `Y`; an optional raw DNL segment (`len`/`nl`) follows the entropy, then EOI.
    fn gray_restart_stream(blocks: &[[i32; 64]], y_field: u16, dnl: Option<(u16, u16)>) -> Vec<u8> {
        let (ldc, lac, ..) = std_enc();
        let mut jpeg = Vec::new();
        marker::write_marker(&mut jpeg, marker::code::SOI);
        quant::emit_dqt(&mut jpeg, &[(0, &dc8_quant())]);
        marker::write_sof0(&mut jpeg, 8, y_field, &[(1, 1, 1, 0)]);
        huffman::emit_dht(
            &mut jpeg,
            &[(0, 0, &huffman::STD_LUMA_DC), (1, 0, &huffman::STD_LUMA_AC)],
        );
        marker::write_dri(&mut jpeg, 1); // restart after every MCU
        marker::write_sos(&mut jpeg, &[(1, 0, 0)]);
        for (i, blk) in blocks.iter().enumerate() {
            let mut seg = Vec::new();
            let mut w = BitWriter::new(&mut seg);
            let mut pred = 0i32; // predictor resets at each restart interval (§E.2.5)
            emit_block(&mut w, blk, &mut pred, &ldc, &lac);
            w.flush();
            jpeg.extend_from_slice(&seg);
            // No trailing restart after the final MCU (a complete scan never emits one).
            if i + 1 < blocks.len() {
                marker::write_marker(&mut jpeg, marker::code::RST0 + (i as u8 & 7));
            }
        }
        if let Some((len, nl)) = dnl {
            marker::write_segment_header(&mut jpeg, marker::code::DNL, usize::from(len));
            jpeg.extend_from_slice(&nl.to_be_bytes());
        }
        marker::write_marker(&mut jpeg, marker::code::EOI);
        jpeg
    }

    #[test]
    fn zero_height_with_restarts_is_not_truncated_at_restart_boundary() {
        // A Y=0 (DNL) frame that uses restart intervals: each RSTm falls exactly on an MCU-row start.
        // The end-of-data heuristic must NOT mistake a restart marker for the end of the scan — a
        // complete scan never emits a trailing RST, so an RSTm always precedes more MCUs. The DNL
        // form (height deferred) must decode identically to the explicit-Y form (no DNL). Before the
        // fix, the Y=0 form truncated after the first block and failed the plane-coverage check.
        let blocks = [flat(10), flat(-30), flat(20)];
        let deferred = gray_restart_stream(&blocks, 0, Some((4, 24)));
        let explicit = gray_restart_stream(&blocks, 24, None);
        let a: ImageBuf<Gray8> = JpegDecoder::new().decode_image(&deferred).unwrap();
        let b: ImageBuf<Gray8> = JpegDecoder::new().decode_image(&explicit).unwrap();
        assert_eq!(a.dimensions(), gamut_core::Dimensions::new(8, 24).unwrap());
        assert_eq!(a, b);
        // Spot-check the three block rows reconstruct to 128 + DC (dc8_quant step 8).
        assert!(a.as_samples()[..64].iter().all(|&s| s == 138)); // 128 + 10
        assert!(a.as_samples()[64..128].iter().all(|&s| s == 98)); // 128 − 30
        assert!(a.as_samples()[128..].iter().all(|&s| s == 148)); // 128 + 20
    }

    #[test]
    fn info_and_decode_classify_stray_markers_exactly() {
        // info() on SOI+EOI (no frame header): the SOS/EOI arm's own message — a deleted arm would
        // fall into the skip-segment path and misreport a truncated segment.
        assert_eq!(
            err_msg(info(&[0xFF, 0xD8, 0xFF, 0xD9])),
            "JPEG: no frame header before scan/end"
        );
        // info() on a standalone TEM after SOI: the standalone-marker arm's own message.
        assert_eq!(
            err_msg(info(&[0xFF, 0xD8, 0xFF, 0x01, 0xFF, 0xD9])),
            "JPEG: unexpected standalone marker"
        );
        // Decode: a TEM outside a scan is the decode loop's own standalone-marker message.
        let mut s = gray_stream(8, 8, &[flat(1)], None);
        let dqt = s.windows(2).position(|w| w == [0xFF, 0xDB]).unwrap();
        s.splice(dqt..dqt, [0xFF, 0x01]);
        assert_eq!(
            gray_msg(&s),
            "JPEG: unexpected standalone marker outside a scan"
        );
    }

    /// Builds a 20×12 4:2:0 stream (luma 2×2, chroma 1×1) whose luma block grid is NOT MCU-aligned
    /// (20 = 1¼ MCUs wide): non-interleaved luma is 3×2 blocks (§A.2.2, ceil(20/8)×ceil(12/8)) while
    /// the interleaved walk is 4×2 (2 MCUs × 2 blocks, partial-MCU completion). `luma(bx, by)` and
    /// `chroma(ci, bx)` supply each block's coefficients by grid position.
    fn ycbcr420_20x12(interleaved: bool) -> Vec<u8> {
        let luma = |bx: i32, by: i32| {
            let mut b = [0i32; 64];
            b[0] = bx * 3 + by * 5 - 4;
            b[1] = bx - by;
            b
        };
        let chroma = |ci: i32, bx: i32| {
            let mut b = [0i32; 64];
            b[0] = ci * 6 - 3 + bx;
            b
        };
        let (ldc, lac, cdc, cac) = std_enc();
        let mut jpeg = Vec::new();
        marker::write_marker(&mut jpeg, marker::code::SOI);
        marker::write_app0_jfif(&mut jpeg, marker::DensityUnit::AspectRatio, 1, 1);
        quant::emit_dqt(&mut jpeg, &[(0, &[1u8; 64]), (1, &[1u8; 64])]);
        marker::write_sof0(
            &mut jpeg,
            20,
            12,
            &[(1, 2, 2, 0), (2, 1, 1, 1), (3, 1, 1, 1)],
        );
        huffman::emit_dht(
            &mut jpeg,
            &[
                (0, 0, &huffman::STD_LUMA_DC),
                (1, 0, &huffman::STD_LUMA_AC),
                (0, 1, &huffman::STD_CHROMA_DC),
                (1, 1, &huffman::STD_CHROMA_AC),
            ],
        );
        if interleaved {
            marker::write_sos(&mut jpeg, &[(1, 0, 0), (2, 1, 1), (3, 1, 1)]);
            // 2×1 MCUs; per MCU: luma (2×2 blocks row-major), then Cb, then Cr (§A.2.3).
            let mut order = Vec::new();
            for mx in 0..2i32 {
                for by in 0..2i32 {
                    for bx in 0..2i32 {
                        order.push((0usize, luma(mx * 2 + bx, by)));
                    }
                }
                order.push((1, chroma(1, mx)));
                order.push((2, chroma(2, mx)));
            }
            let tables = [
                (ldc.clone(), lac.clone()),
                (cdc.clone(), cac.clone()),
                (cdc, cac),
            ];
            jpeg.extend_from_slice(&entropy(&order, &tables));
        } else {
            // Luma scan: 3×2 blocks over the COMPONENT's own grid, row-major (§A.2.2).
            marker::write_sos(&mut jpeg, &[(1, 0, 0)]);
            let mut order = Vec::new();
            for by in 0..2i32 {
                for bx in 0..3i32 {
                    order.push((0usize, luma(bx, by)));
                }
            }
            jpeg.extend_from_slice(&entropy(&order, &[(ldc, lac)]));
            // Chroma scans: comp 10×6 → 2×1 blocks each.
            marker::write_sos(&mut jpeg, &[(2, 1, 1)]);
            let cb: Vec<(usize, [i32; 64])> = (0..2).map(|bx| (0usize, chroma(1, bx))).collect();
            jpeg.extend_from_slice(&entropy(&cb, &[(cdc.clone(), cac.clone())]));
            marker::write_sos(&mut jpeg, &[(3, 1, 1)]);
            let cr: Vec<(usize, [i32; 64])> = (0..2).map(|bx| (0usize, chroma(2, bx))).collect();
            jpeg.extend_from_slice(&entropy(&cr, &[(cdc, cac)]));
        }
        marker::write_marker(&mut jpeg, marker::code::EOI);
        jpeg
    }

    #[test]
    fn non_aligned_420_non_interleaved_matches_interleaved() {
        // 20×12 4:2:0: the luma block grid is not MCU-aligned, so a decoder that walked a
        // non-interleaved scan with the interleaved MCU geometry would consume the wrong number of
        // blocks (8 vs 6) and misplace every sample. The two encodings of the same content must
        // decode identically over the visible region.
        let a: ImageBuf<Rgb8> = JpegDecoder::new()
            .decode_image(&ycbcr420_20x12(true))
            .unwrap();
        let b: ImageBuf<Rgb8> = JpegDecoder::new()
            .decode_image(&ycbcr420_20x12(false))
            .unwrap();
        assert_eq!(a.dimensions(), gamut_core::Dimensions::new(20, 12).unwrap());
        assert_eq!(a, b);
    }

    // --- Progressive (SOF2) hand-built streams -------------------------------------------------
    //
    // These build progressive byte streams from the crate's writers plus small test-only entropy
    // emitters that mirror the T.81 Annex G encoding procedures (Figures G.3/G.4/G.6 for the AC
    // first pass, Figure G.7 for AC refinement, §G.1.2.1/§G.1.2.3 for DC). They pin the progressive
    // decoder against independently computed expectations and against the sequential twin, ground
    // the successive-approximation and EOBRUN edge cases the oracle battery cannot isolate, and
    // exercise the scan-ordering validation corpus.

    /// Writes a SOF2 (progressive) frame header — the SOF0 writer with the marker code swapped.
    fn write_sof2(out: &mut Vec<u8>, w: u16, h: u16, comps: &[(u8, u8, u8, u8)]) {
        let mut seg = Vec::new();
        marker::write_sof0(&mut seg, w, h, comps);
        seg[1] = marker::code::SOF2;
        out.extend_from_slice(&seg);
    }

    /// Writes a progressive SOS header carrying the band `[Ss..=Se]` and precision `(Ah, Al)`.
    fn write_sos_prog(out: &mut Vec<u8>, comps: &[(u8, u8, u8)], ss: u8, se: u8, ah: u8, al: u8) {
        marker::write_segment_header(out, marker::code::SOS, 6 + 2 * comps.len());
        out.push(comps.len() as u8);
        for &(cs, td, ta) in comps {
            out.push(cs);
            out.push(marker::pack_nibbles(td, ta));
        }
        out.push(ss);
        out.push(se);
        out.push(marker::pack_nibbles(ah, al));
    }

    /// DC first pass (§G.1.2.1): DPCM of the point-transformed DC (arithmetic-shift-right by `al`,
    /// §A.4), coded with the DC Huffman table.
    fn pg_dc_first(w: &mut BitWriter, blocks: &[[i32; 64]], al: u8, dc: &EncTable, pred: &mut i32) {
        for blk in blocks {
            let dcv = blk[0] >> al;
            let diff = dcv - *pred;
            *pred = dcv;
            let cat = magcat(diff);
            let (c, l) = dc.lookup(cat).unwrap();
            w.write_bits(c, l);
            w.write_bits(addbits(diff, cat), cat);
        }
    }

    /// DC refinement (§G.1.2.3): one raw bit per block — the next lower bit of the DC coefficient.
    fn pg_dc_refine(w: &mut BitWriter, blocks: &[[i32; 64]], al: u8) {
        for blk in blocks {
            w.write_bits(((blk[0] >> al) & 1) as u16, 1);
        }
    }

    /// The size (appended-bit count) of an EOB run and its symbol, then flush it (Figure G.4).
    fn pg_flush_eobrun(w: &mut BitWriter, eobrun: &mut u32, ac: &EncTable) {
        if *eobrun == 0 {
            return;
        }
        let e = *eobrun;
        let n = 31 - e.leading_zeros(); // EOBSIZE = floor(log2(run))
        let (c, l) = ac.lookup((n as u8) << 4).unwrap();
        w.write_bits(c, l);
        if n > 0 {
            w.write_bits((e & ((1u32 << n) - 1)) as u16, n as u8);
        }
        *eobrun = 0;
    }

    /// AC first pass over one scan's blocks (§G.1.2.2, Figure G.3): run/size symbols within the band
    /// `[ss..=se]` on the point-transformed (integer-divide-by-2^al) coefficients, with EOB runs
    /// accumulated across blocks.
    fn pg_ac_first(
        w: &mut BitWriter,
        blocks: &[[i32; 64]],
        ss: usize,
        se: usize,
        al: u8,
        ac: &EncTable,
    ) {
        let mut eobrun = 0u32;
        for blk in blocks {
            let mut run = 0usize;
            for k in ss..=se {
                let coef = blk[ZIGZAG[k]];
                // Point transform: integer divide by 2^al, rounding toward zero (§A.4).
                let v = if coef < 0 {
                    -((-coef) >> al)
                } else {
                    coef >> al
                };
                if v == 0 {
                    run += 1;
                    continue;
                }
                pg_flush_eobrun(w, &mut eobrun, ac);
                while run >= 16 {
                    let (c, l) = ac.lookup(0xF0).unwrap();
                    w.write_bits(c, l);
                    run -= 16;
                }
                let cat = magcat(v);
                let (c, l) = ac.lookup(((run as u8) << 4) | cat).unwrap();
                w.write_bits(c, l);
                w.write_bits(addbits(v, cat), cat);
                run = 0;
            }
            if run > 0 {
                eobrun += 1;
            }
        }
        pg_flush_eobrun(w, &mut eobrun, ac);
    }

    /// AC refinement over one scan's blocks (§G.1.2.3, Figure G.7 — the libjpeg-faithful buffered
    /// correction-bit encoder). Correction bits for non-zero-history coefficients are buffered and
    /// appended after the associated ZRL / newly-nonzero symbol or the trailing EOB run.
    fn pg_ac_refine(
        w: &mut BitWriter,
        blocks: &[[i32; 64]],
        ss: usize,
        se: usize,
        al: u8,
        ac: &EncTable,
    ) {
        let mut eobrun = 0u32;
        let mut be: Vec<u16> = Vec::new(); // buffered correction bits for pending EOB-run blocks
        for blk in blocks {
            // Point-transformed absolute values and the EOB (last newly-nonzero) index.
            let mut absv = [0i32; 64];
            let mut eob = 0usize;
            for k in ss..=se {
                let t = blk[ZIGZAG[k]].abs() >> al;
                absv[k] = t;
                if t == 1 {
                    eob = k;
                }
            }
            let mut run = 0usize;
            let mut br: Vec<u16> = Vec::new(); // this block's pending correction bits
            for k in ss..=se {
                let t = absv[k];
                if t == 0 {
                    run += 1;
                    continue;
                }
                while run > 15 && k <= eob {
                    pg_flush_eobrun(w, &mut eobrun, ac);
                    for &b in &be {
                        w.write_bits(b, 1);
                    }
                    be.clear();
                    let (c, l) = ac.lookup(0xF0).unwrap();
                    w.write_bits(c, l);
                    run -= 16;
                    for &b in &br {
                        w.write_bits(b, 1);
                    }
                    br.clear();
                }
                if t > 1 {
                    // Non-zero-history coefficient: buffer its correction bit (bit al of |coef|).
                    br.push((t & 1) as u16);
                    continue;
                }
                // Newly non-zero coefficient (|value| == 1 at this precision).
                pg_flush_eobrun(w, &mut eobrun, ac);
                for &b in &be {
                    w.write_bits(b, 1);
                }
                be.clear();
                let (c, l) = ac.lookup(((run as u8) << 4) | 1).unwrap();
                w.write_bits(c, l);
                w.write_bits(u16::from(blk[ZIGZAG[k]] > 0), 1); // sign: 1 = positive
                for &b in &br {
                    w.write_bits(b, 1);
                }
                br.clear();
                run = 0;
            }
            if run > 0 || !br.is_empty() {
                eobrun += 1;
                be.extend_from_slice(&br);
            }
        }
        pg_flush_eobrun(w, &mut eobrun, ac);
        for &b in &be {
            w.write_bits(b, 1);
        }
    }

    /// Flushes a `BitWriter` body and appends it to `jpeg`.
    fn append_entropy(jpeg: &mut Vec<u8>, build: impl FnOnce(&mut BitWriter)) {
        let mut body = Vec::new();
        let mut w = BitWriter::new(&mut body);
        build(&mut w);
        w.flush();
        jpeg.extend_from_slice(&body);
    }

    /// A custom AC Huffman table carrying the EOBn run codes (`0x10` = EOB1, `0x20` = EOB2) that the
    /// Annex K "typical" tables omit — needed to hand-code EOB runs longer than one block. Eight
    /// symbols, all length-3 (a complete code): EOB0/1/2, run/size 0x01/0x02/0x03/0x12, and ZRL.
    const PROG_AC: huffman::TableSpec = huffman::TableSpec {
        bits: [0, 0, 8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        values: &[0x00, 0x01, 0x02, 0x03, 0x10, 0x12, 0x20, 0xF0],
    };

    /// A single-component (gray) progressive stream prefix: SOI, DQT, SOF2, DHT (std luma DC + the
    /// given AC table in destination 0).
    fn prog_gray_prefix_ac(quant: &[u8; 64], x: u16, y: u16, ac: &huffman::TableSpec) -> Vec<u8> {
        let mut jpeg = Vec::new();
        marker::write_marker(&mut jpeg, marker::code::SOI);
        quant::emit_dqt(&mut jpeg, &[(0, quant)]);
        write_sof2(&mut jpeg, x, y, &[(1, 1, 1, 0)]);
        huffman::emit_dht(&mut jpeg, &[(0, 0, &huffman::STD_LUMA_DC), (1, 0, ac)]);
        jpeg
    }

    /// The common case: a prefix using the standard luma AC table.
    fn prog_gray_prefix(quant: &[u8; 64], x: u16, y: u16) -> Vec<u8> {
        prog_gray_prefix_ac(quant, x, y, &huffman::STD_LUMA_AC)
    }

    /// A single-component (gray) SEQUENTIAL twin over the same coefficient blocks (block column,
    /// top to bottom) — the reference every progressive stream must match exactly.
    fn seq_gray(quant: &[u8; 64], x: u16, y: u16, blocks: &[[i32; 64]]) -> Vec<u8> {
        let (ldc, lac, ..) = std_enc();
        let mut jpeg = Vec::new();
        marker::write_marker(&mut jpeg, marker::code::SOI);
        quant::emit_dqt(&mut jpeg, &[(0, quant)]);
        marker::write_sof0(&mut jpeg, x, y, &[(1, 1, 1, 0)]);
        huffman::emit_dht(
            &mut jpeg,
            &[(0, 0, &huffman::STD_LUMA_DC), (1, 0, &huffman::STD_LUMA_AC)],
        );
        marker::write_sos(&mut jpeg, &[(1, 0, 0)]);
        let order: Vec<(usize, [i32; 64])> = blocks.iter().map(|b| (0usize, *b)).collect();
        jpeg.extend_from_slice(&entropy(&order, &[(ldc, lac)]));
        marker::write_marker(&mut jpeg, marker::code::EOI);
        jpeg
    }

    fn decode_gray(jpeg: &[u8]) -> Vec<u8> {
        let out: ImageBuf<Gray8> = JpegDecoder::new().decode_image(jpeg).unwrap();
        out.as_samples().to_vec()
    }

    #[test]
    fn progressive_dc_then_ac_first_matches_sequential() {
        // Spectral selection with no successive approximation (Al = 0): a DC-only scan followed by
        // one full AC-band scan reconstructs the exact pixels of the equivalent single-scan
        // sequential file. Pins the coefficient accumulation, the DC/AC scan split, and the EOB0
        // block terminator.
        let quant = quant::LUMINANCE;
        let mut c = [0i32; 64];
        c[0] = 5;
        c[ZIGZAG[1]] = -3;
        c[ZIGZAG[5]] = 2;
        c[ZIGZAG[20]] = 1;
        let (ldc, lac, ..) = std_enc();

        let mut prog = prog_gray_prefix(&quant, 8, 8);
        write_sos_prog(&mut prog, &[(1, 0, 0)], 0, 0, 0, 0);
        let mut pred = 0;
        append_entropy(&mut prog, |w| pg_dc_first(w, &[c], 0, &ldc, &mut pred));
        write_sos_prog(&mut prog, &[(1, 0, 0)], 1, 63, 0, 0);
        append_entropy(&mut prog, |w| pg_ac_first(w, &[c], 1, 63, 0, &lac));
        marker::write_marker(&mut prog, marker::code::EOI);

        assert_eq!(
            decode_gray(&prog),
            decode_gray(&seq_gray(&quant, 8, 8, &[c]))
        );
    }

    #[test]
    fn progressive_successive_approximation_matches_full_precision() {
        // A full successive-approximation progression (DC first Al=1 + DC refine Al=0, AC first
        // Al=1 + AC refine Al=0) must reconstruct the exact same block as the full-precision
        // sequential twin. Coefficients are chosen to exercise every refinement path: an even AC
        // (correction bit 0), an odd AC ≥ 2 (correction bit 1), a magnitude-1 AC (newly nonzero in
        // the refinement scan), and a negative AC.
        let quant = [1u8; 64];
        let mut c = [0i32; 64];
        c[0] = 5; // odd DC
        c[ZIGZAG[1]] = 4; // even, |>>1| = 2 → correction bit 0
        c[ZIGZAG[2]] = 5; // odd ≥ 2 → correction bit 1
        c[ZIGZAG[3]] = 1; // magnitude 1 → newly nonzero in the Al=0 refinement
        c[ZIGZAG[4]] = -3; // odd negative → correction bit 1, sign preserved
        let (ldc, lac, ..) = std_enc();

        let mut prog = prog_gray_prefix(&quant, 8, 8);
        // DC first (Al = 1) then DC refine (Al = 0).
        write_sos_prog(&mut prog, &[(1, 0, 0)], 0, 0, 0, 1);
        let mut pred = 0;
        append_entropy(&mut prog, |w| pg_dc_first(w, &[c], 1, &ldc, &mut pred));
        write_sos_prog(&mut prog, &[(1, 0, 0)], 0, 0, 1, 0);
        append_entropy(&mut prog, |w| pg_dc_refine(w, &[c], 0));
        // AC first (Al = 1) then AC refine (Al = 0).
        write_sos_prog(&mut prog, &[(1, 0, 0)], 1, 63, 0, 1);
        append_entropy(&mut prog, |w| pg_ac_first(w, &[c], 1, 63, 1, &lac));
        write_sos_prog(&mut prog, &[(1, 0, 0)], 1, 63, 1, 0);
        append_entropy(&mut prog, |w| pg_ac_refine(w, &[c], 1, 63, 0, &lac));
        marker::write_marker(&mut prog, marker::code::EOI);

        assert_eq!(
            decode_gray(&prog),
            decode_gray(&seq_gray(&quant, 8, 8, &[c]))
        );
    }

    #[test]
    fn progressive_eobrun_spans_multiple_blocks() {
        // Three blocks whose AC bands are entirely zero, then a fourth with a nonzero AC: the first
        // pass must encode an EOB run of 3 (EOB1 + one appended bit) that the decoder unwinds across
        // the three all-zero blocks before decoding the fourth. Matches the sequential twin exactly.
        let quant = quant::LUMINANCE;
        let blocks = [flat(10), flat(-20), flat(30), {
            let mut b = flat(-5);
            b[ZIGZAG[1]] = 4; // a nonzero AC in the fourth block flushes the EOB run
            b
        }];
        let (ldc, ..) = std_enc();
        let pac = EncTable::from_spec(&PROG_AC);

        let mut prog = prog_gray_prefix_ac(&quant, 8, 32, &PROG_AC);
        write_sos_prog(&mut prog, &[(1, 0, 0)], 0, 0, 0, 0);
        let mut pred = 0;
        append_entropy(&mut prog, |w| pg_dc_first(w, &blocks, 0, &ldc, &mut pred));
        write_sos_prog(&mut prog, &[(1, 0, 0)], 1, 63, 0, 0);
        append_entropy(&mut prog, |w| pg_ac_first(w, &blocks, 1, 63, 0, &pac));
        marker::write_marker(&mut prog, marker::code::EOI);

        assert_eq!(
            decode_gray(&prog),
            decode_gray(&seq_gray(&quant, 8, 32, &blocks))
        );
    }

    #[test]
    fn progressive_ac_scan_restart_resets_eobrun() {
        // A four-block AC scan with restart interval 2 (an RST between blocks 1 and 2). Blocks 0/1
        // have zero AC (an EOB run flushed at the interval end); block 2 opens the next interval
        // with a nonzero AC. Decoding must match the sequential twin — exercising restart-marker
        // consumption, DC-predictor reset, and the EOBRUN reset at the interval boundary.
        let quant = quant::LUMINANCE;
        let blocks = [
            flat(6),
            flat(-6),
            {
                let mut b = flat(12);
                b[ZIGZAG[2]] = 3;
                b
            },
            flat(9),
        ];
        let (ldc, ..) = std_enc();
        let pac = EncTable::from_spec(&PROG_AC);

        let mut prog = prog_gray_prefix_ac(&quant, 8, 32, &PROG_AC);
        marker::write_dri(&mut prog, 2);
        // DC scan, restart every 2 MCUs: predictor resets at the interval boundary.
        write_sos_prog(&mut prog, &[(1, 0, 0)], 0, 0, 0, 0);
        let mut pred = 0;
        append_entropy(&mut prog, |w| {
            pg_dc_first(w, &blocks[..2], 0, &ldc, &mut pred)
        });
        marker::write_marker(&mut prog, marker::code::RST0);
        let mut pred = 0;
        append_entropy(&mut prog, |w| {
            pg_dc_first(w, &blocks[2..], 0, &ldc, &mut pred)
        });
        // AC scan, same restart interval: each interval's EOB run is flushed independently.
        write_sos_prog(&mut prog, &[(1, 0, 0)], 1, 63, 0, 0);
        append_entropy(&mut prog, |w| pg_ac_first(w, &blocks[..2], 1, 63, 0, &pac));
        marker::write_marker(&mut prog, marker::code::RST0);
        append_entropy(&mut prog, |w| pg_ac_first(w, &blocks[2..], 1, 63, 0, &pac));
        marker::write_marker(&mut prog, marker::code::EOI);

        // Sequential twin also uses restart interval 2 (identical coefficients, DC reset per RST).
        assert_eq!(
            decode_gray(&prog),
            decode_gray(&seq_gray_restart(&quant, 8, 32, &blocks, 2))
        );
    }

    /// A gray sequential twin with a restart interval (one block per MCU column).
    fn seq_gray_restart(
        quant: &[u8; 64],
        x: u16,
        y: u16,
        blocks: &[[i32; 64]],
        ri: u16,
    ) -> Vec<u8> {
        let (ldc, lac, ..) = std_enc();
        let mut jpeg = Vec::new();
        marker::write_marker(&mut jpeg, marker::code::SOI);
        quant::emit_dqt(&mut jpeg, &[(0, quant)]);
        marker::write_sof0(&mut jpeg, x, y, &[(1, 1, 1, 0)]);
        huffman::emit_dht(
            &mut jpeg,
            &[(0, 0, &huffman::STD_LUMA_DC), (1, 0, &huffman::STD_LUMA_AC)],
        );
        marker::write_dri(&mut jpeg, ri);
        marker::write_sos(&mut jpeg, &[(1, 0, 0)]);
        let mut rst = 0u8;
        for (i, chunk) in blocks.chunks(usize::from(ri)).enumerate() {
            if i > 0 {
                marker::write_marker(&mut jpeg, marker::code::RST0 + (rst & 7));
                rst += 1;
            }
            let order: Vec<(usize, [i32; 64])> = chunk.iter().map(|b| (0usize, *b)).collect();
            jpeg.extend_from_slice(&entropy(&order, &[(ldc.clone(), lac.clone())]));
        }
        marker::write_marker(&mut jpeg, marker::code::EOI);
        jpeg
    }

    /// One scan descriptor for [`prog_scans`]: `(components, (ss, se, ah, al), entropy_body)`.
    type ScanDesc<'a> = (&'a [(u8, u8, u8)], (u8, u8, u8, u8), Vec<u8>);

    /// Assembles a minimal progressive gray stream from raw scan descriptors for the validation
    /// corpus.
    fn prog_scans(quant: &[u8; 64], x: u16, y: u16, scans: &[ScanDesc]) -> Vec<u8> {
        let mut jpeg = prog_gray_prefix(quant, x, y);
        for (comps, (ss, se, ah, al), body) in scans {
            write_sos_prog(&mut jpeg, comps, *ss, *se, *ah, *al);
            jpeg.extend_from_slice(body);
        }
        marker::write_marker(&mut jpeg, marker::code::EOI);
        jpeg
    }

    /// The entropy body of a single-block gray DC first scan (used to satisfy the DC-precedes-AC
    /// rule in the validation corpus).
    fn dc_first_body(dc: i32) -> Vec<u8> {
        let (ldc, ..) = std_enc();
        let mut body = Vec::new();
        let mut w = BitWriter::new(&mut body);
        let mut pred = 0;
        pg_dc_first(&mut w, &[flat(dc)], 0, &ldc, &mut pred);
        w.flush();
        body
    }

    #[test]
    fn progressive_scan_ordering_is_validated() {
        let quant = quant::LUMINANCE;
        let d =
            |b: &[u8]| <JpegDecoder as DecodeImage<Gray8>>::decode_image(&JpegDecoder::new(), b);

        // An AC scan before any DC scan of the component → InvalidInput (§G.1.1.1.1).
        let ac_before_dc = prog_scans(&quant, 8, 8, &[(&[(1, 0, 0)], (1, 63, 0, 0), Vec::new())]);
        assert!(matches!(
            d(&ac_before_dc),
            Err(error) if error.kind() == gamut_core::ErrorKind::InvalidInput
        ));

        // A refinement scan of a band whose first pass never happened → InvalidInput. DC first, then
        // an AC *refinement* (Ah=1, Al=0) with no AC first pass.
        let refine_first = prog_scans(
            &quant,
            8,
            8,
            &[
                (&[(1, 0, 0)], (0, 0, 0, 0), dc_first_body(3)),
                (&[(1, 0, 0)], (1, 63, 1, 0), Vec::new()),
            ],
        );
        assert!(matches!(
            d(&refine_first),
            Err(error) if error.kind() == gamut_core::ErrorKind::InvalidInput
        ));

        // The same band coded twice at the first pass (overlap) → InvalidInput. DC first, AC band
        // [1..5] first pass, then AC band [3..8] first pass overlapping at 3..5.
        let overlap = prog_scans(
            &quant,
            8,
            8,
            &[
                (&[(1, 0, 0)], (0, 0, 0, 0), dc_first_body(3)),
                (&[(1, 0, 0)], (1, 5, 0, 0), ac_eob_body()),
                (&[(1, 0, 0)], (3, 8, 0, 0), ac_eob_body()),
            ],
        );
        assert!(matches!(
            d(&overlap),
            Err(error) if error.kind() == gamut_core::ErrorKind::InvalidInput
        ));

        // A multi-component AC scan (Ns = 2) is rejected at header parse (§B.2.3).
        let multi_ac = {
            let mut j = prog_gray_prefix(&quant, 8, 8);
            write_sos_prog(&mut j, &[(1, 0, 0), (1, 0, 0)], 1, 63, 0, 0);
            marker::write_marker(&mut j, marker::code::EOI);
            j
        };
        assert!(matches!(
            d(&multi_ac),
            Err(error) if error.kind() == gamut_core::ErrorKind::InvalidInput
        ));

        // EOI after a DC-only scan leaves the frame renderable (partial-render policy): decode Ok.
        let dc_only = prog_scans(
            &quant,
            8,
            8,
            &[(&[(1, 0, 0)], (0, 0, 0, 0), dc_first_body(4))],
        );
        assert!(d(&dc_only).is_ok());
    }

    /// A single-block AC first-pass entropy body that codes an immediate EOB (empty band).
    fn ac_eob_body() -> Vec<u8> {
        let (_, lac, ..) = std_enc();
        let mut body = Vec::new();
        let mut w = BitWriter::new(&mut body);
        pg_ac_first(&mut w, &[flat(0)], 1, 63, 0, &lac);
        w.flush();
        body
    }

    #[test]
    fn progressive_missing_dc_component_is_rejected() {
        // A three-component progressive frame where only two components receive a DC scan: the third
        // never gets a baseline, so the partial-render policy rejects the frame as InvalidInput.
        let (ldc, ..) = std_enc();
        let quant = [1u8; 64];
        let mut jpeg = Vec::new();
        marker::write_marker(&mut jpeg, marker::code::SOI);
        quant::emit_dqt(&mut jpeg, &[(0, &quant)]);
        write_sof2(&mut jpeg, 8, 8, &[(1, 1, 1, 0), (2, 1, 1, 0), (3, 1, 1, 0)]);
        huffman::emit_dht(&mut jpeg, &[(0, 0, &huffman::STD_LUMA_DC)]);
        // Interleaved DC scan over only components 1 and 2 (component 3 omitted).
        write_sos_prog(&mut jpeg, &[(1, 0, 0), (2, 0, 0)], 0, 0, 0, 0);
        append_entropy(&mut jpeg, |w| {
            let mut pred = [0i32; 2];
            pg_dc_first(w, &[flat(3)], 0, &ldc, &mut pred[0]);
            pg_dc_first(w, &[flat(4)], 0, &ldc, &mut pred[1]);
        });
        marker::write_marker(&mut jpeg, marker::code::EOI);
        assert!(matches!(
            <JpegDecoder as DecodeImage<Rgb8>>::decode_image(&JpegDecoder::new(), &jpeg),
            Err(error) if error.kind() == gamut_core::ErrorKind::InvalidInput
        ));
    }

    #[test]
    fn progressive_zero_height_is_unsupported() {
        // A Y=0 (DNL-deferred height) progressive frame is a clean Unsupported error (see STATUS.md
        // — the coefficient buffers need the full block grid before the first scan), never a panic.
        let quant = [1u8; 64];
        let mut jpeg = Vec::new();
        marker::write_marker(&mut jpeg, marker::code::SOI);
        quant::emit_dqt(&mut jpeg, &[(0, &quant)]);
        write_sof2(&mut jpeg, 8, 0, &[(1, 1, 1, 0)]);
        huffman::emit_dht(&mut jpeg, &[(0, 0, &huffman::STD_LUMA_DC)]);
        write_sos_prog(&mut jpeg, &[(1, 0, 0)], 0, 0, 0, 0);
        marker::write_marker(&mut jpeg, marker::code::EOI);
        assert!(matches!(
            <JpegDecoder as DecodeImage<Gray8>>::decode_image(&JpegDecoder::new(), &jpeg),
            Err(error) if error.kind() == gamut_core::ErrorKind::Unsupported
        ));
    }

    #[test]
    fn progressive_truncated_mid_scan_errors_without_panic() {
        // Truncating a progressive stream inside the AC scan's entropy data must yield an Err (the
        // bit reader hits EOF with no terminating marker), never a panic.
        let quant = quant::LUMINANCE;
        let mut c = [0i32; 64];
        c[0] = 7;
        c[ZIGZAG[1]] = 5;
        let (ldc, lac, ..) = std_enc();
        let mut prog = prog_gray_prefix(&quant, 8, 8);
        write_sos_prog(&mut prog, &[(1, 0, 0)], 0, 0, 0, 0);
        let mut pred = 0;
        append_entropy(&mut prog, |w| pg_dc_first(w, &[c], 0, &ldc, &mut pred));
        write_sos_prog(&mut prog, &[(1, 0, 0)], 1, 63, 0, 0);
        append_entropy(&mut prog, |w| pg_ac_first(w, &[c], 1, 63, 0, &lac));
        // Drop the final EOI and the last entropy byte.
        let truncated = &prog[..prog.len() - 3];
        assert!(
            <JpegDecoder as DecodeImage<Gray8>>::decode_image(&JpegDecoder::new(), truncated)
                .is_err()
        );
    }

    #[test]
    fn progressive_dc_category_11_is_accepted() {
        // A DC first-pass difference of category 11 (the largest legal 8-bit DC category) must pass
        // the `t > 11` guard — pinning the boundary (a `>=` mutant would reject it). DC 1024 · step 8
        // → 8192, IDCT flat → 1024, +128 → clamps to 255.
        let quant = dc8_quant();
        let (ldc, ..) = std_enc();
        let mut prog = prog_gray_prefix(&quant, 8, 8);
        write_sos_prog(&mut prog, &[(1, 0, 0)], 0, 0, 0, 0);
        let mut pred = 0;
        append_entropy(&mut prog, |w| {
            pg_dc_first(w, &[flat(1024)], 0, &ldc, &mut pred)
        });
        marker::write_marker(&mut prog, marker::code::EOI);
        assert!(decode_gray(&prog).iter().all(|&v| v == 255));
    }

    #[test]
    fn progressive_ac_first_run_at_and_past_band_end() {
        // In an AC band [1..=5], a run/size symbol whose run lands the index exactly on Se (0x41 =
        // run 4, size 1, from k=1 → k=5) is legal and places a coefficient; a run that overshoots
        // (0xF1 = run 15 → k=16 > 5) is rejected. Together these pin `k > se` at its boundary.
        let quant = quant::LUMINANCE;
        let (ldc, lac, ..) = std_enc();
        let mut ok = prog_gray_prefix(&quant, 8, 8);
        write_sos_prog(&mut ok, &[(1, 0, 0)], 0, 0, 0, 0);
        let mut pred = 0;
        append_entropy(&mut ok, |w| pg_dc_first(w, &[flat(3)], 0, &ldc, &mut pred));
        write_sos_prog(&mut ok, &[(1, 0, 0)], 1, 5, 0, 0);
        append_entropy(&mut ok, |w| {
            // Symbol 0x41 (run 4, size 1) then a magnitude bit → coefficient at k = Se = 5.
            let (c, l) = lac.lookup(0x41).unwrap();
            w.write_bits(c, l);
            w.write_bits(1, 1);
            // EOB0 terminates the block.
            let (c, l) = lac.lookup(0x00).unwrap();
            w.write_bits(c, l);
        });
        marker::write_marker(&mut ok, marker::code::EOI);
        assert!(
            <JpegDecoder as DecodeImage<Gray8>>::decode_image(&JpegDecoder::new(), &ok).is_ok()
        );

        // Overshoot: 0xF1 (run 15) from k = 1 pushes the index to 16 > Se = 5.
        let mut bad = prog_gray_prefix(&quant, 8, 8);
        write_sos_prog(&mut bad, &[(1, 0, 0)], 0, 0, 0, 0);
        let mut pred = 0;
        append_entropy(&mut bad, |w| pg_dc_first(w, &[flat(3)], 0, &ldc, &mut pred));
        write_sos_prog(&mut bad, &[(1, 0, 0)], 1, 5, 0, 0);
        append_entropy(&mut bad, |w| {
            let (c, l) = lac.lookup(0xF1).unwrap();
            w.write_bits(c, l);
            w.write_bits(1, 1);
        });
        marker::write_marker(&mut bad, marker::code::EOI);
        assert!(matches!(
            <JpegDecoder as DecodeImage<Gray8>>::decode_image(&JpegDecoder::new(), &bad),
            Err(error) if error.kind() == gamut_core::ErrorKind::InvalidInput
        ));
    }

    #[test]
    fn progressive_refinement_history_must_match() {
        // A band's first pass at Al = 2 followed by a refinement claiming Ah = 1 (history mismatch:
        // the band was coded at Al = 2, not 1) must be rejected — pinning the `prev == ah` history
        // guard (a `true` mutant would accept any prior precision). The DC scan precedes so the AC
        // ordering rule is satisfied; the AC first/refine bodies are empty bands (immediate EOB).
        let quant = quant::LUMINANCE;
        let (ldc, lac, ..) = std_enc();
        let mut jpeg = prog_gray_prefix(&quant, 8, 8);
        write_sos_prog(&mut jpeg, &[(1, 0, 0)], 0, 0, 0, 0);
        let mut pred = 0;
        append_entropy(&mut jpeg, |w| {
            pg_dc_first(w, &[flat(3)], 0, &ldc, &mut pred)
        });
        // AC first pass of band [1..=5] at Al = 2 (empty band → EOB0). coded_al[1..=5] = Some(2).
        write_sos_prog(&mut jpeg, &[(1, 0, 0)], 1, 5, 0, 2);
        append_entropy(&mut jpeg, |w| pg_ac_first(w, &[flat(3)], 1, 5, 2, &lac));
        // AC refinement of the same band with Ah = 1 (≠ the stored Al = 2), Al = 0.
        write_sos_prog(&mut jpeg, &[(1, 0, 0)], 1, 5, 1, 0);
        append_entropy(&mut jpeg, |w| pg_ac_refine(w, &[flat(3)], 1, 5, 0, &lac));
        marker::write_marker(&mut jpeg, marker::code::EOI);
        assert!(matches!(
            <JpegDecoder as DecodeImage<Gray8>>::decode_image(&JpegDecoder::new(), &jpeg),
            Err(error) if error.kind() == gamut_core::ErrorKind::InvalidInput
        ));
    }

    #[test]
    fn progressive_dc_three_level_refinement_matches_sequential() {
        // A three-scan DC successive approximation (first Al=2, refine Al=1, refine Al=0) on DC = 7
        // (binary 111). The Al=1 refinement bit sets `1 << 1`, pinning the DC-refine shift (a `>>`
        // mutant sets nothing there and loses the bit, reconstructing 5 not 7). The DC step of 8
        // scales the difference above the IDCT-rounding floor so the wrong value is visible.
        let quant = dc8_quant();
        let c = flat(7);
        let (ldc, ..) = std_enc();
        let mut prog = prog_gray_prefix(&quant, 8, 8);
        write_sos_prog(&mut prog, &[(1, 0, 0)], 0, 0, 0, 2);
        let mut pred = 0;
        append_entropy(&mut prog, |w| pg_dc_first(w, &[c], 2, &ldc, &mut pred));
        write_sos_prog(&mut prog, &[(1, 0, 0)], 0, 0, 2, 1);
        append_entropy(&mut prog, |w| pg_dc_refine(w, &[c], 1));
        write_sos_prog(&mut prog, &[(1, 0, 0)], 0, 0, 1, 0);
        append_entropy(&mut prog, |w| pg_dc_refine(w, &[c], 0));
        marker::write_marker(&mut prog, marker::code::EOI);
        assert_eq!(
            decode_gray(&prog),
            decode_gray(&seq_gray(&quant, 8, 8, &[c]))
        );
    }

    #[test]
    fn progressive_ac_refine_newly_nonzero_at_band_end() {
        // A single-coefficient band [Ss=Se=1]: the coefficient (magnitude 1) is zero at the first
        // pass (Al=1) and becomes newly-nonzero in the refinement (Al=0), inserted at k == Se. Pins
        // the `k > se` insertion guard at its boundary (a `>=` mutant would reject the valid insert).
        let quant = [1u8; 64];
        let mut c = flat(4);
        c[ZIGZAG[1]] = 1; // magnitude-1 AC at the single band position
        let (ldc, lac, ..) = std_enc();
        let mut prog = prog_gray_prefix(&quant, 8, 8);
        write_sos_prog(&mut prog, &[(1, 0, 0)], 0, 0, 0, 0);
        let mut pred = 0;
        append_entropy(&mut prog, |w| pg_dc_first(w, &[c], 0, &ldc, &mut pred));
        write_sos_prog(&mut prog, &[(1, 0, 0)], 1, 1, 0, 1);
        append_entropy(&mut prog, |w| pg_ac_first(w, &[c], 1, 1, 1, &lac));
        write_sos_prog(&mut prog, &[(1, 0, 0)], 1, 1, 1, 0);
        append_entropy(&mut prog, |w| pg_ac_refine(w, &[c], 1, 1, 0, &lac));
        marker::write_marker(&mut prog, marker::code::EOI);
        assert_eq!(
            decode_gray(&prog),
            decode_gray(&seq_gray(&quant, 8, 8, &[c]))
        );
    }

    #[test]
    fn progressive_ac_refine_run_past_band_end_is_rejected() {
        // A refinement whose newly-nonzero symbol cannot find its target zero within the band (both
        // band positions are non-zero history, so the advance loop reaches k > Se without a target)
        // must be rejected. This pins the `k > se` break in the advance loop (line ~894): a `>=`
        // mutant would break one position early and silently insert the coefficient over the last
        // history coefficient instead of erroring.
        let quant = quant::LUMINANCE;
        let (ldc, lac, ..) = std_enc();
        let mut c = flat(3);
        c[ZIGZAG[1]] = 4; // both band positions non-zero at the first pass (history coefficients)
        c[ZIGZAG[2]] = 4;
        let mut jpeg = prog_gray_prefix(&quant, 8, 8);
        write_sos_prog(&mut jpeg, &[(1, 0, 0)], 0, 0, 0, 0);
        let mut pred = 0;
        append_entropy(&mut jpeg, |w| pg_dc_first(w, &[c], 0, &ldc, &mut pred));
        write_sos_prog(&mut jpeg, &[(1, 0, 0)], 1, 2, 0, 1);
        append_entropy(&mut jpeg, |w| pg_ac_first(w, &[c], 1, 2, 1, &lac));
        // Hand-built refinement: a newly-nonzero symbol (run 0, size 1) with a positive sign, then
        // correction bits for the two history coefficients. The advance loop walks both, never finds
        // a target zero, and runs past Se = 2.
        write_sos_prog(&mut jpeg, &[(1, 0, 0)], 1, 2, 1, 0);
        append_entropy(&mut jpeg, |w| {
            let (code, len) = lac.lookup(0x01).unwrap();
            w.write_bits(code, len);
            w.write_bits(1, 1); // sign: positive
            w.write_bits(1, 1); // correction bit for k = 1
            w.write_bits(1, 1); // correction bit for k = 2
        });
        marker::write_marker(&mut jpeg, marker::code::EOI);
        assert!(matches!(
            <JpegDecoder as DecodeImage<Gray8>>::decode_image(&JpegDecoder::new(), &jpeg),
            Err(error) if error.kind() == gamut_core::ErrorKind::InvalidInput
        ));
    }

    #[test]
    fn progressive_interleaved_dc_matches_sequential_420() {
        // A 32×32 4:2:0 frame (luma 2×2, chroma 1×1) = 2×2 MCUs, DC-only, coded as an interleaved
        // progressive DC scan and as the sequential twin. The multiple MCUs with sampling factors > 1
        // pin the interleaved block-position arithmetic `mx·Hi` / `my·Vi` (a `/` mutant collapses
        // distinct blocks onto each other). DC-only renders under the partial-render policy.
        let luma_dc = |bx: usize, by: usize| bx as i32 * 3 + by as i32 * 5 - 7;
        let cb_dc = |bx: usize, by: usize| bx as i32 - by as i32 * 2 + 1;
        let cr_dc = |bx: usize, by: usize| by as i32 - bx as i32 + 2;
        let (ldc, lac, cdc, cac) = std_enc();

        // Progressive: SOI, DQT (one table for all), SOF2, DHT (luma+chroma DC), interleaved DC scan.
        let mut prog = Vec::new();
        marker::write_marker(&mut prog, marker::code::SOI);
        marker::write_app0_jfif(&mut prog, marker::DensityUnit::AspectRatio, 1, 1);
        quant::emit_dqt(&mut prog, &[(0, &[1u8; 64])]);
        write_sof2(
            &mut prog,
            32,
            32,
            &[(1, 2, 2, 0), (2, 1, 1, 0), (3, 1, 1, 0)],
        );
        huffman::emit_dht(
            &mut prog,
            &[
                (0, 0, &huffman::STD_LUMA_DC),
                (0, 1, &huffman::STD_CHROMA_DC),
            ],
        );
        write_sos_prog(&mut prog, &[(1, 0, 0), (2, 1, 1), (3, 1, 1)], 0, 0, 0, 0);
        append_entropy(&mut prog, |w| {
            let mut pred = [0i32; 3];
            for my in 0..2 {
                for mx in 0..2 {
                    for by in 0..2 {
                        for bx in 0..2 {
                            pg_dc_first(
                                w,
                                &[flat(luma_dc(mx * 2 + bx, my * 2 + by))],
                                0,
                                &ldc,
                                &mut pred[0],
                            );
                        }
                    }
                    pg_dc_first(w, &[flat(cb_dc(mx, my))], 0, &cdc, &mut pred[1]);
                    pg_dc_first(w, &[flat(cr_dc(mx, my))], 0, &cdc, &mut pred[2]);
                }
            }
        });
        marker::write_marker(&mut prog, marker::code::EOI);

        // Sequential 4:2:0 twin: same MCU order, one interleaved baseline scan of flat blocks.
        let mut seq = Vec::new();
        marker::write_marker(&mut seq, marker::code::SOI);
        marker::write_app0_jfif(&mut seq, marker::DensityUnit::AspectRatio, 1, 1);
        quant::emit_dqt(&mut seq, &[(0, &[1u8; 64])]);
        marker::write_sof0(
            &mut seq,
            32,
            32,
            &[(1, 2, 2, 0), (2, 1, 1, 0), (3, 1, 1, 0)],
        );
        huffman::emit_dht(
            &mut seq,
            &[
                (0, 0, &huffman::STD_LUMA_DC),
                (1, 0, &huffman::STD_LUMA_AC),
                (0, 1, &huffman::STD_CHROMA_DC),
                (1, 1, &huffman::STD_CHROMA_AC),
            ],
        );
        marker::write_sos(&mut seq, &[(1, 0, 0), (2, 1, 1), (3, 1, 1)]);
        let mut order = Vec::new();
        for my in 0..2 {
            for mx in 0..2 {
                for by in 0..2 {
                    for bx in 0..2 {
                        order.push((0usize, flat(luma_dc(mx * 2 + bx, my * 2 + by))));
                    }
                }
                order.push((1usize, flat(cb_dc(mx, my))));
                order.push((2usize, flat(cr_dc(mx, my))));
            }
        }
        seq.extend_from_slice(&entropy(
            &order,
            &[(ldc, lac), (cdc.clone(), cac.clone()), (cdc, cac)],
        ));
        marker::write_marker(&mut seq, marker::code::EOI);

        let a: ImageBuf<Rgb8> = JpegDecoder::new().decode_image(&prog).unwrap();
        let b: ImageBuf<Rgb8> = JpegDecoder::new().decode_image(&seq).unwrap();
        assert_eq!(a.dimensions(), gamut_core::Dimensions::new(32, 32).unwrap());
        assert_eq!(a, b);
    }
}
