//! The baseline sequential DCT Huffman encoder: [`JpegEncoder`] and its pipeline.
//!
//! For each 8×8 block the pipeline is T.81 Annex A end to end — colour-convert (T.871 §7) →
//! chroma-subsample → level shift (§A.3.1) → forward DCT (§A.3.3, via `gamut_dsp`) → quantize
//! (§A.3.4) → zig-zag (§A.3.6) → differential DC + run-length AC Huffman coding (Annex F §F.1.2) —
//! interleaved into minimum coded units (§A.2.3) and wrapped in a JFIF interchange stream (§B.2).
//!
//! The entropy coder runs through [`BaselineCoder`], which either **emits** codes or merely
//! **gathers** symbol frequencies. The fixed-table path runs one emit pass; the optimized-table path
//! ([`Process::Optimized`]) runs a gather pass first, builds the Annex K.2 tables
//! from what it counted, and then emits — both passes driven by the same [`encode_scan`] walk, so a
//! symbol can never be written without having been counted.
//!
//! # Module layout
//!
//! One submodule per stage: `builder` holds [`JpegEncoder`]'s constructor, setters and the
//! configuration they resolve to (quantization tables, RD context, the pre-encode checks);
//! `block` the sample planes and the per-block transform (level shift, FDCT, quantization,
//! chroma downsampling); `entropy` the §F.1.2 baseline entropy coder and its Huffman tables; and
//! `encode` the frame writers, the backend hand-off and the [`EncodeImage`] impls that drive the
//! other three. This module holds the configuration types and the encoder struct itself.

mod block;
mod builder;
mod encode;
mod entropy;
#[cfg(test)]
mod tests;

use std::fmt;

use gamut_color::ChromaSubsampling;
// Named only by the docs: the impls live in `encode`.
#[cfg(doc)]
use gamut_core::{EncodeImage, Gray8, Rgb8};
use gamut_core::{Error, Result};

pub(crate) use self::block::{Plane, quantize_block_rd};
#[cfg(doc)]
use self::entropy::{BaselineCoder, encode_scan};
pub(crate) use self::entropy::{additional_bits, magnitude_category};
use crate::backend::EncoderRegistry;
use crate::marker::DensityUnit;
use crate::quant::QuantTables;

/// The largest image dimension the frame header can encode: the SOF0 `X`/`Y` fields are 16-bit
/// (§B.2.2, Table B.2).
const MAX_DIMENSION: u32 = u16::MAX as u32;

/// The luma sampling factors `(Hy, Vy)` the encoder writes for a [`ChromaSubsampling`]: `1×1`
/// (4:4:4), `2×1` (4:2:2), `2×2` (4:2:0) or `1×2` (4:4:0), with chroma always `1×1`, so the factors
/// double as the box-subsampling applied to each chroma plane. In [`JpegColorMode::Xyb`] the same
/// ratio subsamples the B channel alone, X and Y taking the luma role.
///
/// T.81 §B.2.2 (Table B.2) admits any factor in `1..=4` per component, so each grid is a choice the
/// format leaves to the encoder; all four fit the §B.2.3 limit of ten data units per MCU. 4:4:0 is
/// libjpeg-turbo's `TJSAMP_440`; T.871 §9 NOTE 3 discourages forms other than 4:2:0 for JFIF because
/// some applications may not support them, but a T.81 decoder reads them.
///
/// # Errors
///
/// [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) for a layout with no three-component
/// JPEG frame: 4:0:0 (encode a [`Gray8`] image for a single-component stream) and any layout the
/// shared enum grows later.
fn luma_factors(subsampling: ChromaSubsampling) -> Result<(u8, u8)> {
    match subsampling {
        ChromaSubsampling::Cs444 => Ok((1, 1)),
        ChromaSubsampling::Cs422 => Ok((2, 1)),
        ChromaSubsampling::Cs420 => Ok((2, 2)),
        ChromaSubsampling::Cs440 => Ok((1, 2)),
        _ => Err(Error::unsupported(
            env!("CARGO_PKG_NAME"),
            "JPEG: colour subsampling must be 4:4:4, 4:2:2, 4:2:0 or 4:4:0",
        )),
    }
}

/// The colour space the encoder codes an [`Rgb8`] image in
/// ([`JpegEncoder::with_color_mode`]).
///
/// The discriminants are permanent and append-only (the workspace C-ABI contract for fieldless
/// enums).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
#[non_exhaustive]
pub enum JpegColorMode {
    /// T.871 §7 full-range BT.601 YCbCr in a JFIF stream — the frozen default.
    #[default]
    Ycbcr = 0,
    /// The JPEG XL **XYB** opsin space, jpegli-style: samples are scaled-XYB
    /// (`gamut_color::xyb::scale_xyb` — stored channels X, Y, B−Y), the stream carries no JFIF
    /// APP0 (T.871 would imply YCbCr) but an Adobe APP14 with `transform = 0` and component ids
    /// `R`,`G`,`B`, and [`XYB_ICC_PROFILE`] is embedded so any ICC-aware decoder reproduces sRGB.
    Xyb = 1,
}

/// The ICC profile a [`JpegColorMode::Xyb`] stream embeds (and the one to hand a CMM after
/// decoding such a stream): an input-class RGB→XYZ profile whose `A2B0` pipeline inverts the
/// scaled-XYB byte encoding, the opsin cube root and bias, and the opsin mixing into D50 PCS XYZ.
///
/// The bytes are static (vendored) and platform-independent; an umbrella-level test regenerates
/// them from `gamut-icc` + `gamut-color` and asserts byte equality, and validates them against
/// the lcms2 oracle end-to-end.
pub const XYB_ICC_PROFILE: &[u8] = include_bytes!("../xyb/xyb-srgb.icc");

/// Rate–distortion optimization mode ([`JpegEncoder::with_rd_optimization`]): how quantized
/// coefficients are chosen from the DCT output.
///
/// The discriminants are permanent and append-only (the workspace C-ABI contract for fieldless
/// enums); future refinements are new variants, never renumberings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
#[non_exhaustive]
pub enum RdOptimization {
    /// Plain §A.3.4 nearest rounding — the frozen default; output bytes are unchanged.
    #[default]
    None = 0,
    /// Per-block trellis search over the AC coefficients, minimizing distortion plus λ times the
    /// exact §F.1.2.2 entropy cost (see the crate's `rd` module docs). Smaller files at nearly
    /// identical fidelity; the DC coefficient keeps plain rounding.
    Trellis = 1,
    /// [`Self::Trellis`] plus per-block adaptive quantization: λ is modulated by each block's own
    /// AC energy, spending relatively more bits on flat (masking-poor) blocks and fewer on busy
    /// ones.
    TrellisAdaptive = 2,
}

/// The scan script a progressive stream is coded with ([`Process::Progressive`]).
///
/// Both scripts deliver every coefficient bit, so the decoded image is identical to the baseline
/// encoding of the same input whichever is chosen; they differ only in how the coefficients are
/// split into scans, and therefore in stream size and in how a progressive preview refines.
///
/// The discriminants are permanent and append-only (the workspace C-ABI contract for fieldless
/// enums).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
#[non_exhaustive]
pub enum ProgressiveScript {
    /// libjpeg's `jpeg_simple_progression` (`jcparam.c`) — the SemVer-frozen default: 6 scans for
    /// grayscale, 10 for three components, with DC sent by successive approximation.
    #[default]
    Libjpeg = 0,
    /// mozjpeg's `jpeg_simple_progression` under its `JCP_MAX_COMPRESSION` profile with
    /// `dc_scan_opt_mode = 0` (the jpgcrush `jpeg_scan_bw.txt` / `jpeg_scan_rgb.txt` scripts; see
    /// `references/jpeg/README.md`): DC sent whole in one scan and AC split at coefficient 8/9 —
    /// 5 scans for grayscale, 9 for YCbCr, and mozjpeg's all-purpose 13-scan script for
    /// [`JpegColorMode::Xyb`]. Measured smaller than [`Self::Libjpeg`] on average (issue #30's
    /// experiment: −0.35% grey, −0.66% 4:2:0, −1.51% 4:4:4), though not at every quality.
    Mozjpeg = 1,
}

/// The coding process the encoder writes ([`JpegEncoder::with_process`]).
///
/// All three code the same quantized coefficients, so the decoded image is identical whichever is
/// chosen; they differ in stream structure and size.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum Process {
    /// Baseline sequential DCT (SOF0) with the fixed Annex K.3–K.6 "typical" Huffman tables — the
    /// frozen default, whose byte stream is SemVer-stable.
    #[default]
    Baseline,
    /// Baseline sequential DCT (SOF0) with Huffman tables fitted to the image's own symbol
    /// statistics (T.81 Annex K.2): a few percent smaller for free — libjpeg's `optimize_coding`.
    ///
    /// **Cost:** the scan is walked twice — once to count symbols, once to write them — so the
    /// forward DCT runs twice and encoding takes roughly twice as long. No coefficient buffer is
    /// retained, so peak memory is unchanged. Only the DHT contents and the entropy bytes differ
    /// from [`Self::Baseline`]; marker order and segment count are identical.
    Optimized,
    /// Progressive DCT (SOF2, T.81 Annex G) coded with the given [`ProgressiveScript`], with
    /// optimized per-scan Huffman tables (Annex K.2; the typical AC tables cannot code an `EOBn`
    /// symbol). A decoder can render a coarse whole-image preview from the first scans and refine
    /// it as more arrive.
    Progressive(ProgressiveScript),
}

/// A reusable JPEG encoder: baseline sequential (SOF0) by default, progressive (SOF2) on request.
///
/// Configure it with the builder methods, then drive it through [`EncodeImage`]. It writes JFIF
/// interchange streams: grayscale ([`Gray8`], one component) or YCbCr ([`Rgb8`], converted per
/// T.871 §7 with the configured [`ChromaSubsampling`]).
///
/// # Frozen quality contract
///
/// For a given `(quality, subsampling)` the quantization tables — and therefore the coefficient
/// values — are SemVer-stable: quality 50 emits the T.81 Annex K tables verbatim, and the IJG
/// quality→scale mapping is frozen. The byte stream is likewise stable for a given configuration;
/// [`Process::Optimized`] changes the Huffman tables and hence the entropy bytes, but it
/// is opt-in and leaves the coefficients — and the decoded image — untouched. Caller-supplied
/// tables ([`Self::with_quant_tables`]) bypass the frozen mapping without changing it.
///
/// # Example
///
/// ```
/// use gamut_core::{Dimensions, EncodeImage, ImageRef, Gray8};
/// use gamut_jpeg::JpegEncoder;
///
/// let pixels = vec![128u8; 8 * 8];
/// let image = ImageRef::<Gray8>::new(&pixels, Dimensions::new(8, 8)?)?;
/// let mut jpeg = Vec::new();
/// JpegEncoder::new().with_quality(90).encode_image(image, &mut jpeg)?;
/// assert_eq!(&jpeg[..2], &[0xFF, 0xD8]); // SOI
/// # Ok::<(), gamut_core::Error>(())
/// ```
#[derive(Clone)]
pub struct JpegEncoder {
    quality: u8,
    subsampling: ChromaSubsampling,
    restart_interval: u16,
    density_unit: DensityUnit,
    x_density: u16,
    y_density: u16,
    process: Process,
    quant_tables: Option<QuantTables>,
    rd: RdOptimization,
    color_mode: JpegColorMode,
    exif: Option<Vec<u8>>,
    xmp: Option<Vec<u8>>,
    icc: Option<Vec<u8>>,
    /// Pluggable whole-stream backends, tried in push order ahead of the built-in encoder.
    backends: EncoderRegistry,
}

impl fmt::Debug for JpegEncoder {
    /// Mirrors the derived formatting, except that the opaque `dyn` backends show as a count and the
    /// metadata payloads as their byte lengths.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JpegEncoder")
            .field("quality", &self.quality)
            .field("subsampling", &self.subsampling)
            .field("restart_interval", &self.restart_interval)
            .field("density_unit", &self.density_unit)
            .field("x_density", &self.x_density)
            .field("y_density", &self.y_density)
            .field("process", &self.process)
            .field("quant_tables", &self.quant_tables)
            .field("rd", &self.rd)
            .field("color_mode", &self.color_mode)
            .field("exif", &self.exif.as_ref().map(Vec::len))
            .field("xmp", &self.xmp.as_ref().map(Vec::len))
            .field("icc", &self.icc.as_ref().map(Vec::len))
            .field("backends", &self.backends.len())
            .finish()
    }
}

impl Default for JpegEncoder {
    fn default() -> Self {
        Self::new()
    }
}
