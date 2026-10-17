//! The baseline sequential DCT Huffman encoder: [`JpegEncoder`] and its pipeline.
//!
//! For each 8×8 block the pipeline is T.81 Annex A end to end — colour-convert (T.871 §7) →
//! chroma-subsample → level shift (§A.3.1) → forward DCT (§A.3.3, via `gamut_dsp`) → quantize
//! (§A.3.4) → zig-zag (§A.3.6) → differential DC + run-length AC Huffman coding (Annex F §F.1.2) —
//! interleaved into minimum coded units (§A.2.3) and wrapped in a JFIF interchange stream (§B.2).
//!
//! The entropy coder runs through [`BaselineCoder`], which either **emits** codes or merely
//! **gathers** symbol frequencies. The fixed-table path runs one emit pass; the optimized-table path
//! ([`JpegEncoder::with_optimized_tables`]) runs a gather pass first, builds the Annex K.2 tables
//! from what it counted, and then emits — both passes driven by the same [`encode_scan`] walk, so a
//! symbol can never be written without having been counted.

use std::fmt;
use std::sync::{Arc, Mutex};

use gamut_color::transfer::srgb_eotf;
use gamut_color::{ColorRange, rgb_to_ycbcr, xyb};
use gamut_core::{Dimensions, EncodeImage, Error, Gray8, ImageRef, PixelFormat, Result, Rgb8};
use gamut_dsp::jpeg::fdct8x8;
use gamut_dsp::math::round_div_nearest;

use crate::backend::{self, EncoderSlot, JpegEncodeRequest, JpegStreamEncoder, RasterRef};
use crate::bitwriter::BitWriter;
use crate::huffman::{self, EncTable, TableSpec};
use crate::marker::{self, DensityUnit};
use crate::quant::QuantTables;
use crate::rd::RdCtx;
use crate::zigzag::ZIGZAG;
use crate::{appmeta, progressive, quant, rd};

/// The largest image dimension the frame header can encode: the SOF0 `X`/`Y` fields are 16-bit
/// (§B.2.2, Table B.2).
const MAX_DIMENSION: u32 = u16::MAX as u32;

/// Chroma subsampling mode for YCbCr (colour) encoding: the ratio at which the Cb/Cr planes are
/// sampled relative to luma. Ignored for grayscale, which has a single component.
///
/// Named for the conventional `J:a:b` notation. The luma sampling factors are `1×1` (4:4:4),
/// `2×1` (4:2:2), or `2×2` (4:2:0); chroma is always `1×1`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ChromaSubsampling {
    /// 4:4:4 — no chroma subsampling; full-resolution Cb/Cr (largest files, best chroma fidelity).
    Ycbcr444,
    /// 4:2:2 — Cb/Cr subsampled 2:1 horizontally only.
    Ycbcr422,
    /// 4:2:0 — Cb/Cr subsampled 2:1 both horizontally and vertically (the common photographic
    /// default; T.871 §9 NOTE 3 names it the most common form).
    Ycbcr420,
}

impl ChromaSubsampling {
    /// The luma horizontal/vertical sampling factors `(Hy, Vy)`; chroma is fixed at `1×1`, so these
    /// double as the box-subsampling factors applied to each chroma plane.
    fn luma_factors(self) -> (u8, u8) {
        match self {
            ChromaSubsampling::Ycbcr444 => (1, 1),
            ChromaSubsampling::Ycbcr422 => (2, 1),
            ChromaSubsampling::Ycbcr420 => (2, 2),
        }
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
pub const XYB_ICC_PROFILE: &[u8] = include_bytes!("xyb/xyb-srgb.icc");

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

/// A reusable baseline JPEG encoder.
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
/// [`Self::with_optimized_tables`] changes the Huffman tables and hence the entropy bytes, but it
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
    progressive: bool,
    optimize_tables: bool,
    quant_tables: Option<QuantTables>,
    rd: RdOptimization,
    color_mode: JpegColorMode,
    exif: Option<Vec<u8>>,
    xmp: Option<Vec<u8>>,
    icc: Option<Vec<u8>>,
    /// Pluggable whole-stream backends, tried in push order ahead of the built-in encoder.
    backends: Vec<EncoderSlot>,
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
            .field("progressive", &self.progressive)
            .field("optimize_tables", &self.optimize_tables)
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

impl JpegEncoder {
    /// Creates an encoder with quality 75, [`ChromaSubsampling::Ycbcr420`], no restart interval, and
    /// a 1:1 aspect-ratio pixel density (JFIF `units = 0`).
    #[must_use]
    pub fn new() -> Self {
        Self {
            quality: 75,
            subsampling: ChromaSubsampling::Ycbcr420,
            restart_interval: 0,
            density_unit: DensityUnit::AspectRatio,
            x_density: 1,
            y_density: 1,
            progressive: false,
            optimize_tables: false,
            quant_tables: None,
            rd: RdOptimization::None,
            color_mode: JpegColorMode::Ycbcr,
            exif: None,
            xmp: None,
            icc: None,
            backends: Vec::new(),
        }
    }

    /// Appends a [`JpegStreamEncoder`] backend to this encoder's registry.
    ///
    /// Backends are consulted in **push order** for every encode; the first whose
    /// [`JpegStreamEncoder::supports`] accepts the [`JpegEncodeRequest`] produces the whole JFIF
    /// interchange stream. The built-in encoder is the implicit tail, used only when every backend
    /// declines. An encoder configured with [`Self::with_quant_tables`] skips the registry
    /// entirely — a [`JpegEncodeRequest`] cannot carry custom tables, so the job is pinned to the
    /// built-in path. The crate then **patches its APPn metadata into the produced stream** — any
    /// EXIF/XMP/`ICC_PROFILE` segment the backend emitted is replaced by this encoder's configured
    /// [`with_exif`](Self::with_exif) / [`with_xmp`](Self::with_xmp) /
    /// [`with_icc_profile`](Self::with_icc_profile) payloads (validated against their caps *before*
    /// any backend runs). See the [`crate::backend`] module docs for the full contract.
    ///
    /// Unlike the `with_*` builders this takes `&mut self`, because a registry is not a `Copy`
    /// setting.
    ///
    /// # Cloning shares backends
    ///
    /// Backends are stored behind `Arc<Mutex<..>>`, so **cloning a `JpegEncoder` shares them with
    /// the clone** rather than duplicating them: a stateful backend sees the encodes of every clone,
    /// and its lock serializes concurrent ones. Build a fresh encoder when a clone needs its own
    /// backend instances.
    pub fn push_backend(&mut self, backend: impl JpegStreamEncoder + 'static) -> &mut Self {
        self.backends.push(Arc::new(Mutex::new(backend)));
        self
    }

    /// Offers the raster to the encode registry; on acceptance the returned stream has this
    /// encoder's metadata patched into it and is appended to `out`, and the written length is
    /// returned. `None` means no backend accepted and the caller runs the built-in encoder.
    ///
    /// `width`/`height` have already passed [`Self::check_dimensions`] and the metadata
    /// [`Self::check_metadata`], so a backend never sees a job the built-in path would reject.
    fn encode_via_backend(
        &self,
        width: u16,
        height: u16,
        format: PixelFormat,
        samples: &[u8],
        out: &mut Vec<u8>,
    ) -> Result<Option<usize>> {
        // Caller-supplied quantization tables, RD optimization, and the XYB colour mode cannot
        // ride a `JpegEncodeRequest`, so any of them pins the encode to the built-in path rather
        // than let a backend silently encode with a different configuration (the same host-side
        // veto gamut-jxl applies to its container features).
        if self.quant_tables.is_some()
            || self.rd != RdOptimization::None
            || self.color_mode != JpegColorMode::Ycbcr
        {
            return Ok(None);
        }
        if self.backends.is_empty() {
            return Ok(None);
        }
        let (w, h) = (u32::from(width), u32::from(height));
        let req = JpegEncodeRequest::new(
            w,
            h,
            format,
            self.quality,
            self.subsampling,
            self.progressive,
            self.restart_interval,
        );
        let raster = RasterRef::new(w, h, format, samples)?;
        let Some(stream) = backend::encode_with_backends(&self.backends, &req, &raster)? else {
            return Ok(None);
        };
        let patched = appmeta::patch_stream(
            &stream,
            appmeta::AppMetadata {
                exif: self.exif.as_deref(),
                xmp: self.xmp.as_deref(),
                icc: self.icc.as_deref(),
            },
        )?;
        // Validation ownership: the produced stream must declare a frame of exactly the image the
        // caller asked to encode, so a backend cannot quietly resize or drop it.
        let info = crate::JpegStreamInfo::parse(&patched)?;
        if (info.width(), info.height()) != (w, h) {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "JPEG: backend stream declares different dimensions than the encoded image",
            ));
        }
        out.extend_from_slice(&patched);
        Ok(Some(patched.len()))
    }

    /// Sets the quality, **clamped** to `1..=100` (higher is better/larger). Quality 50 uses the
    /// Annex K tables verbatim; 100 uses all-1 tables. Clamping (rather than rejecting) matches
    /// libjpeg's `jpeg_set_quality`. Ignored when [`Self::with_quant_tables`] is set.
    #[must_use]
    pub fn with_quality(mut self, quality: u8) -> Self {
        self.quality = quality.clamp(1, 100);
        self
    }

    /// Quantizes with `tables` **verbatim** instead of the quality-scaled Annex K tables,
    /// replacing the only lever [`Self::with_quality`] offers — the frozen IJG mapping — with full
    /// caller control (perceptually-tuned tables, near-lossless all-1 tables, or
    /// [`QuantTables::scaled`] re-scaling of an arbitrary base).
    ///
    /// While set, `with_quality` has no effect on the emitted tables or coefficients, and the
    /// encode is pinned to the **built-in** encoder: pushed backends are not consulted, because a
    /// [`crate::backend::JpegEncodeRequest`] cannot carry custom tables. Grayscale uses only the
    /// luma table. The frozen quality contract is unaffected — it continues to govern the default
    /// path.
    #[must_use]
    pub fn with_quant_tables(mut self, tables: QuantTables) -> Self {
        self.quant_tables = Some(tables);
        self
    }

    /// Sets the chroma [`ChromaSubsampling`] used for colour ([`Rgb8`]) input. No effect on
    /// grayscale.
    #[must_use]
    pub fn with_subsampling(mut self, subsampling: ChromaSubsampling) -> Self {
        self.subsampling = subsampling;
        self
    }

    /// Sets the restart interval in MCUs: a restart marker (RSTn) is inserted every `mcus` MCUs,
    /// letting a decoder resynchronize. `0` (the default) disables restarts, emitting no DRI segment.
    #[must_use]
    pub fn with_restart_interval(mut self, mcus: u16) -> Self {
        self.restart_interval = mcus;
        self
    }

    /// Sets the JFIF pixel density written to the APP0 segment: the [`DensityUnit`] and the
    /// horizontal/vertical densities. Each density is clamped to be non-zero, as T.871 §10.1
    /// requires.
    #[must_use]
    pub fn with_density(mut self, unit: DensityUnit, x_density: u16, y_density: u16) -> Self {
        self.density_unit = unit;
        self.x_density = x_density.max(1);
        self.y_density = y_density.max(1);
        self
    }

    /// Selects the **progressive DCT** process (SOF2, T.81 Annex G) when `true`, or the default
    /// baseline sequential process (SOF0) when `false`.
    ///
    /// A progressive stream codes the image as several scans, each carrying one spectral band at one
    /// successive-approximation precision, so a decoder can render a coarse whole-image preview from
    /// the first scans and refine it as more arrive. gamut uses libjpeg's frozen
    /// `jpeg_simple_progression` scan script (a 6-scan gray / 10-scan YCbCr layout) with optimized
    /// per-scan Huffman tables (Annex K.2). The quantized coefficients — and therefore the decoded
    /// image — are identical to the baseline encoding of the same input at the same
    /// `(quality, subsampling)`; only the stream structure differs.
    #[must_use]
    pub fn with_progressive(mut self, progressive: bool) -> Self {
        self.progressive = progressive;
        self
    }

    /// Builds the baseline scan's Huffman tables from the image's own symbol statistics (T.81
    /// Annex K.2) instead of writing the fixed Annex K.3–K.6 "typical" tables.
    ///
    /// The typical tables were tuned for a generic photographic mix, so a table matched to the
    /// actual image is a few percent smaller for free — the same `optimize_coding` tradeoff
    /// libjpeg offers, and the same one gamut's progressive encoder already takes unconditionally
    /// (Annex K.5/K.6 cannot code a progressive AC scan at all).
    ///
    /// **Cost:** the scan is walked twice — once to count symbols, once to write them — so the
    /// forward DCT runs twice and encoding takes roughly twice as long. No coefficient buffer is
    /// retained, so peak memory is unchanged.
    ///
    /// **Not a quality change.** The quantized coefficients are untouched; only the DHT and the
    /// entropy-coded bytes differ, so the decoded image is identical either way. Marker order and
    /// segment count are identical too: one DHT segment, in the same position.
    ///
    /// Defaults to `false`, which keeps the byte stream of every previously-encodable
    /// configuration exactly as it was. Has no effect on [`Self::with_progressive`] streams, whose
    /// per-scan tables are always optimized.
    #[must_use]
    pub fn with_optimized_tables(mut self, optimize: bool) -> Self {
        self.optimize_tables = optimize;
        self
    }

    /// Selects the colour space colour ([`Rgb8`]) input is coded in ([`JpegColorMode`]).
    ///
    /// The default [`JpegColorMode::Ycbcr`] is the frozen JFIF path. [`JpegColorMode::Xyb`] codes
    /// jpegli-style XYB samples with [`XYB_ICC_PROFILE`] embedded — a perceptual space that
    /// out-compresses YCbCr on an ICC-aware pipeline. In XYB mode: [`Self::with_subsampling`] is
    /// ignored (always 4:4:4 — chroma-style subsampling of X would destroy opponent-colour detail
    /// jpegli keeps at full resolution; jpegli's B-only subsampling is a possible follow-up),
    /// [`Self::with_density`] is
    /// inert (no JFIF APP0 is written), [`Self::with_icc_profile`] is rejected at encode time (a
    /// caller profile would misdescribe the XYB samples; EXIF/XMP stay available), grayscale
    /// ([`Gray8`]) input is rejected as unsupported, and pushed backends are not consulted.
    /// Progressive mode, restart intervals, optimized tables, custom quantization tables, and RD
    /// optimization all compose. Decoding an XYB stream (with this crate's decoder or any other)
    /// yields the scaled-XYB samples presented as RGB plus the embedded profile via
    /// [`crate::metadata`]; applying the profile is the caller's CMM's job.
    ///
    /// The samples come from `f64` colour math, so — unlike the default path — XYB-mode output
    /// bytes are **not** bit-reproducible across platforms (gamut-color's Tier-1 determinism);
    /// the embedded profile bytes are static and platform-independent.
    #[must_use]
    pub fn with_color_mode(mut self, mode: JpegColorMode) -> Self {
        self.color_mode = mode;
        self
    }

    /// Selects how quantized coefficients are chosen ([`RdOptimization`]): plain nearest rounding
    /// (the default — output bytes unchanged), per-block AC trellis, or trellis with per-block
    /// adaptive λ.
    ///
    /// Rate–distortion optimization changes the coefficients (that is its point), so it is opt-in
    /// and produces different — spec-valid — bytes; the frozen quality contract continues to bind
    /// only the default [`RdOptimization::None`] path. It composes with every other builder:
    /// custom [`Self::with_quant_tables`], [`Self::with_optimized_tables`] (rates are still costed
    /// against the typical-table proxy; the emitted tables then fit whatever the trellis chose),
    /// and [`Self::with_progressive`] (the progressive stream carries the same trellis-chosen
    /// coefficients as the baseline stream, preserving the exactness invariant). While set to a
    /// non-`None` mode the encode is pinned to the **built-in** encoder — a
    /// [`crate::backend::JpegEncodeRequest`] cannot carry the RD configuration.
    #[must_use]
    pub fn with_rd_optimization(mut self, rd: RdOptimization) -> Self {
        self.rd = rd;
        self
    }

    /// Embeds EXIF metadata as an APP1 segment (`"Exif\0\0"` + TIFF stream, Exif 3.0 §4.7.2).
    ///
    /// `exif` is the TIFF stream beginning `II`/`MM` — e.g. `gamut-exif` output; a blob already
    /// carrying the `"Exif\0\0"` signature is accepted and not double-prefixed, so bytes read by
    /// [`crate::metadata`] round-trip verbatim. Must fit the single APP1 segment (at most 65527
    /// bytes), checked at encode time.
    #[must_use]
    pub fn with_exif(mut self, exif: &[u8]) -> Self {
        let tiff = exif.strip_prefix(appmeta::EXIF_SIG).unwrap_or(exif);
        self.exif = Some(tiff.to_vec());
        self
    }

    /// Embeds an XMP `xpacket` as an APP1 segment (XMP Part 3 §1.1.3).
    ///
    /// Takes bytes rather than `&str` because a packet may open with a BOM. The packet must fit
    /// the single StandardXMP segment (at most 65502 bytes, the spec-stated cap), checked at
    /// encode time; the ExtendedXMP continuation scheme is not supported.
    #[must_use]
    pub fn with_xmp(mut self, xmp: &[u8]) -> Self {
        self.xmp = Some(xmp.to_vec());
        self
    }

    /// Embeds an ICC profile across one or more APP2 `ICC_PROFILE` segments (ICC.1:2001-04
    /// Annex B.4): 65519-byte chunks carrying a 1-based index and the total count, so up to
    /// 16 707 345 profile bytes (255 chunks), checked at encode time.
    #[must_use]
    pub fn with_icc_profile(mut self, profile: &[u8]) -> Self {
        self.icc = Some(profile.to_vec());
        self
    }

    /// The luminance quantization table (natural order): the caller's table when
    /// [`Self::with_quant_tables`] is set, otherwise Annex K.1 scaled for the configured quality.
    fn luma_quant(&self) -> [u8; 64] {
        match &self.quant_tables {
            Some(tables) => *tables.luma(),
            None => quant::scale(&quant::LUMINANCE, self.quality),
        }
    }

    /// The chrominance quantization table (natural order): the caller's table when
    /// [`Self::with_quant_tables`] is set, otherwise Annex K.2 scaled for the configured quality.
    fn chroma_quant(&self) -> [u8; 64] {
        match &self.quant_tables {
            Some(tables) => *tables.chroma(),
            None => quant::scale(&quant::CHROMINANCE, self.quality),
        }
    }

    /// The rate–distortion context for one component class (`chroma` selects the typical AC rate
    /// proxy: Annex K.6 rather than K.5), or `None` when RD optimization is off.
    fn rd_ctx(&self, chroma: bool) -> Option<RdCtx> {
        (self.rd != RdOptimization::None).then(|| {
            let spec = if chroma {
                &huffman::STD_CHROMA_AC
            } else {
                &huffman::STD_LUMA_AC
            };
            RdCtx::new(
                EncTable::from_spec(spec),
                self.rd == RdOptimization::TrellisAdaptive,
            )
        })
    }

    /// Rejects dimensions the frame header cannot encode (`X`/`Y` are 16-bit). Zero is already
    /// excluded by [`Dimensions`].
    fn check_dimensions(dims: Dimensions) -> Result<(u16, u16)> {
        if dims.width > MAX_DIMENSION || dims.height > MAX_DIMENSION {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "JPEG: image exceeds 65535×65535",
            ));
        }
        Ok((dims.width as u16, dims.height as u16))
    }

    /// Rejects metadata payloads the APPn framing cannot carry (the caps in [`crate::appmeta`]),
    /// so [`Self::write_prologue`] stays infallible and no bytes are written before the check.
    fn check_metadata(&self) -> Result<()> {
        if let Some(exif) = &self.exif {
            if exif.is_empty() {
                return Err(Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "JPEG: empty EXIF payload",
                ));
            }
            if exif.len() > appmeta::MAX_EXIF {
                return Err(Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "JPEG: EXIF exceeds one APP1 segment",
                ));
            }
        }
        if let Some(xmp) = &self.xmp {
            if xmp.is_empty() {
                return Err(Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "JPEG: empty XMP payload",
                ));
            }
            if xmp.len() > appmeta::MAX_XMP {
                return Err(Error::unsupported(
                    env!("CARGO_PKG_NAME"),
                    "JPEG: XMP exceeds one APP1 segment (ExtendedXMP not supported)",
                ));
            }
        }
        if let Some(icc) = &self.icc {
            if self.color_mode == JpegColorMode::Xyb {
                return Err(Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "JPEG: XYB mode embeds its own ICC profile; a caller profile would misdescribe the samples",
                ));
            }
            if icc.is_empty() {
                return Err(Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "JPEG: empty ICC profile",
                ));
            }
            if icc.len() > appmeta::MAX_ICC {
                return Err(Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "JPEG: ICC profile exceeds 255 APP2 segments",
                ));
            }
        }
        Ok(())
    }

    /// Writes the leading markers common to every stream: SOI, JFIF APP0, the configured metadata
    /// APP segments, and the DQT segment.
    ///
    /// JFIF mandates its APP0 first while Exif 3.0 §4.7.2.1 wants its APP1 immediately after SOI,
    /// and neither spec references the other; APP0 then APP1 is the libjpeg-family convention that
    /// XMP Part 3 §1.1.3 records readers must accept. EXIF, XMP, then ICC — all before the first
    /// SOF, as XMP Part 3 requires.
    fn write_prologue(&self, out: &mut Vec<u8>, quant_tables: &[(u8, &[u8; 64])]) {
        marker::write_marker(out, marker::code::SOI);
        marker::write_app0_jfif(out, self.density_unit, self.x_density, self.y_density);
        if let Some(exif) = &self.exif {
            appmeta::write_app1_exif(out, exif);
        }
        if let Some(xmp) = &self.xmp {
            appmeta::write_app1_xmp(out, xmp);
        }
        if let Some(icc) = &self.icc {
            appmeta::write_app2_icc(out, icc);
        }
        quant::emit_dqt(out, quant_tables);
    }

    /// Writes the whole baseline (SOF0) frame: the frame header, one DHT segment, an optional DRI,
    /// the scan header, and the entropy-coded scan. The caller has already written the SOI/APP0/DQT
    /// prologue and appends EOI afterward.
    ///
    /// With [`Self::with_optimized_tables`] enabled the scan is walked **twice**: a gather pass
    /// counts the symbols each entropy destination will code, those counts drive the Annex K.2
    /// optimal-table construction, and the emit pass writes the entropy data with the resulting
    /// tables. Marker order and segment count are identical either way — only the DHT contents and
    /// the entropy bytes differ.
    fn write_baseline_frame(
        &self,
        out: &mut Vec<u8>,
        width: u16,
        height: u16,
        sof: &[(u8, u8, u8, u8)],
        sos: &[(u8, u8, u8)],
        components: &[Component],
    ) {
        let (w, h) = (u32::from(width), u32::from(height));
        marker::write_sof0(out, width, height, sof);

        let tables = if self.optimize_tables {
            let mut freq = Histograms::default();
            let mut coder = BaselineCoder::gather(&mut freq);
            encode_scan(components, w, h, self.restart_interval, &mut coder);
            coder.finish();
            optimized_tables(out, &freq)
        } else {
            let color = components.len() > 1;
            emit_huffman_tables(out, color);
            standard_tables(color)
        };

        if self.restart_interval != 0 {
            marker::write_dri(out, self.restart_interval);
        }
        marker::write_sos(out, sos);

        let mut coder = BaselineCoder::emit(out, &tables);
        encode_scan(components, w, h, self.restart_interval, &mut coder);
        coder.finish();
    }

    /// Encodes `rgb` as an XYB stream ([`JpegColorMode::Xyb`]): sRGB → linear (EOTF LUT) → XYB →
    /// scaled-XYB bytes into three full-resolution planes (X, Y, B−Y), coded 4:4:4 with component
    /// ids `R`,`G`,`B` under a no-APP0 / APP14 `transform = 0` prologue that embeds
    /// [`XYB_ICC_PROFILE`]. X and Y quantize with the luminance table (destination 0), the stored
    /// B−Y with the chrominance table (destination 1) — Annex K tables are YCbCr-tuned, so this
    /// pairing is an honest placeholder, not XYB-tuned (see STATUS.md). The caller writes EOI.
    fn encode_xyb(&self, rgb: &[u8], width: u16, height: u16, out: &mut Vec<u8>) {
        let (w, h) = (usize::from(width), usize::from(height));

        // 256-entry EOTF LUT: the only per-pixel float work left is the XYB transform itself.
        let mut eotf = [0f64; 256];
        for (i, v) in eotf.iter_mut().enumerate() {
            *v = srgb_eotf(i as f64 / 255.0);
        }

        let mut planes = [
            vec![0u8; w * h], // X
            vec![0u8; w * h], // Y
            vec![0u8; w * h], // stored B − Y
        ];
        for i in 0..w * h {
            let linear = [
                eotf[usize::from(rgb[i * 3])],
                eotf[usize::from(rgb[i * 3 + 1])],
                eotf[usize::from(rgb[i * 3 + 2])],
            ];
            let scaled = xyb::scale_xyb(xyb::linear_srgb_to_xyb(linear));
            for (plane, &s) in planes.iter_mut().zip(scaled.iter()) {
                // scale_xyb clamps to [0, 1], so the rounded value is already in 0..=255.
                plane[i] = (s * 255.0).round() as u8;
            }
        }
        let [x, y, b] = planes;
        let x_plane = Plane {
            data: x,
            width: w,
            height: h,
        };
        let y_plane = Plane {
            data: y,
            width: w,
            height: h,
        };
        let b_plane = Plane {
            data: b,
            width: w,
            height: h,
        };

        let luma_quant = self.luma_quant();
        let chroma_quant = self.chroma_quant();
        let luma_rd = self.rd_ctx(false);
        let chroma_rd = self.rd_ctx(true);

        // Prologue: SOI, Adobe APP14 transform = 0 (no JFIF APP0 — T.871 defines the 3-component
        // JFIF stream as YCbCr), EXIF/XMP if configured, the XYB ICC profile, DQT.
        marker::write_marker(out, marker::code::SOI);
        marker::write_app14_adobe(out, 0);
        if let Some(exif) = &self.exif {
            appmeta::write_app1_exif(out, exif);
        }
        if let Some(xmp) = &self.xmp {
            appmeta::write_app1_xmp(out, xmp);
        }
        appmeta::write_app2_icc(out, XYB_ICC_PROFILE);
        quant::emit_dqt(out, &[(0, &luma_quant), (1, &chroma_quant)]);

        // Component ids are the bytes 'R','G','B' (the jpegli convention for XYB streams),
        // belt-and-braces beside the APP14: either signal alone keeps a decoder from applying a
        // YCbCr inverse. All components 1×1 (4:4:4).
        let ids: [u8; 3] = *b"RGB";
        if self.progressive {
            let comps = [
                progressive::ProgComponent {
                    id: ids[0],
                    h: 1,
                    v: 1,
                    tq: 0,
                    plane: &x_plane,
                    quant: &luma_quant,
                    rd: luma_rd.as_ref(),
                },
                progressive::ProgComponent {
                    id: ids[1],
                    h: 1,
                    v: 1,
                    tq: 0,
                    plane: &y_plane,
                    quant: &luma_quant,
                    rd: luma_rd.as_ref(),
                },
                progressive::ProgComponent {
                    id: ids[2],
                    h: 1,
                    v: 1,
                    tq: 1,
                    plane: &b_plane,
                    quant: &chroma_quant,
                    rd: chroma_rd.as_ref(),
                },
            ];
            progressive::encode(out, width, height, &comps, self.restart_interval);
        } else {
            let components = [
                Component {
                    h: 1,
                    v: 1,
                    plane: &x_plane,
                    quant: &luma_quant,
                    dest: 0,
                    rd: luma_rd.as_ref(),
                },
                Component {
                    h: 1,
                    v: 1,
                    plane: &y_plane,
                    quant: &luma_quant,
                    dest: 0,
                    rd: luma_rd.as_ref(),
                },
                Component {
                    h: 1,
                    v: 1,
                    plane: &b_plane,
                    quant: &chroma_quant,
                    dest: 1,
                    rd: chroma_rd.as_ref(),
                },
            ];
            self.write_baseline_frame(
                out,
                width,
                height,
                &[(ids[0], 1, 1, 0), (ids[1], 1, 1, 0), (ids[2], 1, 1, 1)],
                &[(ids[0], 0, 0), (ids[1], 0, 0), (ids[2], 1, 1)],
                &components,
            );
        }
    }
}

/// A single-channel sample plane at a component's own resolution (row-major, 8-bit).
pub(crate) struct Plane {
    pub(crate) data: Vec<u8>,
    pub(crate) width: usize,
    pub(crate) height: usize,
}

impl Plane {
    /// The sample at `(x, y)` with edge replication (clamping past the plane bounds), level-shifted
    /// to the signed baseline range by subtracting 128 (§A.3.1, `P = 8`). Edge replication is the
    /// encoder's free choice for padding partial edge blocks/MCUs to a whole 8×8 (§A.2.3): repeating
    /// the border minimizes spurious high-frequency energy versus zero-fill.
    pub(crate) fn level_shifted(&self, x: usize, y: usize) -> i32 {
        let cx = x.min(self.width - 1);
        let cy = y.min(self.height - 1);
        i32::from(self.data[cy * self.width + cx]) - 128
    }
}

/// One frame component paired with the sampling, quantization table and entropy-table destination
/// used to code it. The entropy *tables* themselves live in [`ScanTables`], not here, because the
/// optimized path only knows them after the gather pass has walked these same components.
struct Component<'a> {
    /// Horizontal sampling factor `Hi`.
    h: u8,
    /// Vertical sampling factor `Vi`.
    v: u8,
    plane: &'a Plane,
    quant: &'a [u8; 64],
    /// Entropy-table destination (the SOS `Tdj` = `Taj`): 0 = luma, 1 = chroma.
    dest: usize,
    /// The rate–distortion context for this component's class, when RD optimization is enabled.
    rd: Option<&'a RdCtx>,
}

/// The entropy tables a baseline scan codes with, indexed by destination (0 = luma, 1 = chroma).
///
/// A destination is `None` when the scan never references it — a grayscale frame has no chroma
/// destination, and an optimized table is omitted entirely when its histogram came back empty.
#[derive(Default)]
struct ScanTables {
    /// DC-class tables (DHT `Tc = 0`).
    dc: [Option<EncTable>; 2],
    /// AC-class tables (DHT `Tc = 1`).
    ac: [Option<EncTable>; 2],
}

/// Per-destination symbol counts for one baseline scan, the input to the Annex K.2 optimal-table
/// construction. Only Huffman *symbols* are counted; the magnitude/sign bits that follow them are
/// raw, not coded, so they contribute nothing.
struct Histograms {
    /// DC-class symbol counts, indexed by entropy-table destination.
    dc: [[u32; 256]; 2],
    /// AC-class symbol counts, indexed by entropy-table destination.
    ac: [[u32; 256]; 2],
}

impl Default for Histograms {
    fn default() -> Self {
        Self {
            dc: [[0; 256]; 2],
            ac: [[0; 256]; 2],
        }
    }
}

/// The two-mode baseline entropy sink, mirroring [`crate::progressive`]'s `ProgCoder`: a **gather**
/// pass accumulates per-destination symbol frequencies, an **emit** pass writes Huffman codes and
/// the raw magnitude bits. Both passes run the identical control flow ([`encode_scan`]), so every
/// symbol the emit pass writes was counted by the gather pass that built its table.
enum BaselineCoder<'a, 'o> {
    /// Counting only: no output is produced and raw bits are ignored.
    Gather(&'a mut Histograms),
    /// Writing: Huffman codes from `ScanTables` plus raw bits, into the entropy bit writer.
    Emit(BitWriter<'o>, &'a ScanTables),
}

impl<'a, 'o> BaselineCoder<'a, 'o> {
    /// A gather pass accumulating into `freq`.
    fn gather(freq: &'a mut Histograms) -> Self {
        Self::Gather(freq)
    }

    /// An emit pass appending entropy bytes to `out`, coding with `tables`.
    fn emit(out: &'o mut Vec<u8>, tables: &'a ScanTables) -> Self {
        Self::Emit(BitWriter::new(out), tables)
    }

    /// Counts (gather) or emits (emit) one DC-class symbol at entropy destination `dest`.
    fn dc_symbol(&mut self, dest: usize, symbol: u8) {
        match self {
            Self::Gather(freq) => freq.dc[dest][usize::from(symbol)] += 1,
            Self::Emit(writer, tables) => emit_symbol(writer, tables.dc[dest].as_ref(), symbol),
        }
    }

    /// Counts (gather) or emits (emit) one AC-class symbol at entropy destination `dest`.
    fn ac_symbol(&mut self, dest: usize, symbol: u8) {
        match self {
            Self::Gather(freq) => freq.ac[dest][usize::from(symbol)] += 1,
            Self::Emit(writer, tables) => emit_symbol(writer, tables.ac[dest].as_ref(), symbol),
        }
    }

    /// Emits (emit pass only) `n` raw bits of `value`, MSB-first; a no-op while gathering.
    fn raw_bits(&mut self, value: u16, n: u8) {
        if let Self::Emit(writer, _) = self {
            writer.write_bits(value, n);
        }
    }

    /// Writes (emit pass only) restart marker `RSTm`, flushing the segment first.
    fn restart(&mut self, m: u8) {
        if let Self::Emit(writer, _) = self {
            writer.restart(m);
        }
    }

    /// Pads and flushes the final entropy byte (emit pass only).
    fn finish(&mut self) {
        if let Self::Emit(writer, _) = self {
            writer.flush();
        }
    }
}

/// The magnitude category `SSSS` of `value` (Annex F §F.1.2): the number of bits needed for
/// `|value|`, and `0` for `value == 0`.
pub(crate) fn magnitude_category(value: i32) -> u8 {
    (32 - value.unsigned_abs().leading_zeros()) as u8
}

/// The `SSSS` additional bits appended after a DC/AC Huffman code (Annex F §F.1.2.1): the low
/// `category` bits of `value` for a positive value, or of `value - 1` (the "one lower precision"
/// negative encoding) for a negative value.
pub(crate) fn additional_bits(value: i32, category: u8) -> u16 {
    let v = if value < 0 { value - 1 } else { value };
    (v as u32 & ((1u32 << category) - 1)) as u16
}

/// Emits the Huffman code for `symbol` from `table`. Every symbol the entropy coder produces is
/// present in the table it codes with — the standard tables cover the whole baseline alphabet (DC
/// categories 0..=11; AC run/size, EOB `0x00`, ZRL `0xF0`), and an optimized table is built from
/// the very symbols the emit pass then writes — so a missing table or symbol is a logic error,
/// asserted in debug builds.
fn emit_symbol(writer: &mut BitWriter, table: Option<&EncTable>, symbol: u8) {
    match table.and_then(|t| t.lookup(symbol)) {
        Some((code, length)) => writer.write_bits(code, length),
        None => debug_assert!(false, "Huffman symbol {symbol:#x} absent from table"),
    }
}

/// Gathers and level-shifts one 8×8 block of `plane` at block coordinates `(bx, by)` and runs the
/// forward DCT (§A.3.1 / §A.3.3), returning the natural-order **unquantized** coefficients.
fn dct_block(plane: &Plane, bx: usize, by: usize) -> [i32; 64] {
    // Gather the level-shifted samples in natural (raster) order.
    let mut block = [0i32; 64];
    for row in 0..8usize {
        for col in 0..8usize {
            block[row * 8 + col] = plane.level_shifted(bx * 8 + col, by * 8 + row);
        }
    }
    fdct8x8(&mut block);
    block
}

/// Level-shifts, forward-transforms and quantizes one 8×8 block of `plane` at block coordinates
/// `(bx, by)` (§A.3.1 / §A.3.3 / §A.3.4), returning the natural-order quantized coefficients. Shared
/// by the baseline single-pass coder ([`encode_block`]) and the progressive encoder
/// ([`crate::progressive`]), which materializes every block up front before running the scan script.
pub(crate) fn quantize_block(plane: &Plane, quant: &[u8; 64], bx: usize, by: usize) -> [i32; 64] {
    let block = dct_block(plane, bx, by);
    // Quantize (§A.3.4): round-to-nearest divide by the table entry (which is ≥ 1).
    let mut q = [0i32; 64];
    for (dst, (&coeff, &step)) in q.iter_mut().zip(block.iter().zip(quant.iter())) {
        *dst = round_div_nearest(coeff, i32::from(step));
    }
    q
}

/// [`quantize_block`] with an optional rate–distortion context: `None` is exactly the plain
/// nearest-rounding path (the frozen default, byte-for-byte), `Some` routes the unquantized DCT
/// output through the [`crate::rd`] trellis. The single quantization seam shared by the baseline
/// and progressive processes, so an RD choice is identical in both.
pub(crate) fn quantize_block_rd(
    plane: &Plane,
    quant: &[u8; 64],
    bx: usize,
    by: usize,
    rd: Option<&RdCtx>,
) -> [i32; 64] {
    match rd {
        None => quantize_block(plane, quant, bx, by),
        Some(ctx) => rd::trellis_quantize(&dct_block(plane, bx, by), quant, ctx),
    }
}

/// Codes one 8×8 block (§A.3): level-shift → FDCT → quantize, then hands the natural-order
/// quantized coefficients to [`encode_quantized_block`] for entropy coding.
fn encode_block(
    comp: &Component,
    block_x: usize,
    block_y: usize,
    dc_pred: &mut i32,
    coder: &mut BaselineCoder,
) {
    let q = quantize_block_rd(comp.plane, comp.quant, block_x, block_y, comp.rd);
    encode_quantized_block(&q, dc_pred, comp.dest, coder);
}

/// Entropy-codes one block of quantized coefficients (natural order) per §F.1.2: the DC difference
/// against the running predictor (§F.1.2.1, updating it), then the run-length AC symbols in zig-zag
/// order (§F.1.2.2) — ZRL for zero runs of 16, EOB unless the last zig-zag coefficient is nonzero.
fn encode_quantized_block(
    q: &[i32; 64],
    dc_pred: &mut i32,
    dest: usize,
    coder: &mut BaselineCoder,
) {
    // DC: differential coding against the running predictor (§F.1.2.1).
    let diff = q[0] - *dc_pred;
    *dc_pred = q[0];
    let cat = magnitude_category(diff);
    coder.dc_symbol(dest, cat);
    coder.raw_bits(additional_bits(diff, cat), cat);

    // AC: run-length of zeros then (run, size) symbols in zig-zag order (§F.1.2.2).
    let mut run = 0u8;
    for &natural in &ZIGZAG[1..] {
        let coeff = q[natural];
        if coeff == 0 {
            run += 1;
            continue;
        }
        while run >= 16 {
            coder.ac_symbol(dest, 0xF0); // ZRL: 16 zeros
            run -= 16;
        }
        let cat = magnitude_category(coeff);
        coder.ac_symbol(dest, marker::pack_nibbles(run, cat));
        coder.raw_bits(additional_bits(coeff, cat), cat);
        run = 0;
    }
    if run > 0 {
        coder.ac_symbol(dest, 0x00); // EOB: block ends in zeros
    }
}

/// Codes the interleaved scan over all components (§A.2.3): walk MCUs row-major, and within each MCU
/// walk each component's `Vi×Hi` blocks. Restart markers are inserted every `restart_interval` MCUs
/// (predictors reset). A single-component (gray) scan degenerates to one 8×8 block per MCU — the
/// non-interleaved order of §A.2.2.
///
/// Shared verbatim by the gather and emit passes of the optimized-table path (and run once, in emit
/// mode, by the fixed-table path), so the frequency counts always match the emitted symbols. The
/// caller flushes the coder afterwards, padding the final entropy byte before the next marker.
fn encode_scan(
    components: &[Component],
    width: u32,
    height: u32,
    restart_interval: u16,
    coder: &mut BaselineCoder,
) {
    let hmax = components.iter().map(|c| c.h).max().unwrap_or(1);
    let vmax = components.iter().map(|c| c.v).max().unwrap_or(1);
    let mcu_w = 8 * u32::from(hmax);
    let mcu_h = 8 * u32::from(vmax);
    let mcus_x = width.div_ceil(mcu_w);
    let mcus_y = height.div_ceil(mcu_h);

    let mut dc_pred = vec![0i32; components.len()];
    let mut mcu_index = 0u32;
    let mut restart_m = 0u8;

    for my in 0..mcus_y {
        for mx in 0..mcus_x {
            if restart_interval != 0
                && mcu_index != 0
                && mcu_index.is_multiple_of(u32::from(restart_interval))
            {
                coder.restart(restart_m);
                restart_m = restart_m.wrapping_add(1);
                dc_pred.iter_mut().for_each(|p| *p = 0);
            }
            for (ci, comp) in components.iter().enumerate() {
                for by in 0..u32::from(comp.v) {
                    for bx in 0..u32::from(comp.h) {
                        let block_x = (mx * u32::from(comp.h) + bx) as usize;
                        let block_y = (my * u32::from(comp.v) + by) as usize;
                        encode_block(comp, block_x, block_y, &mut dc_pred[ci], coder);
                    }
                }
            }
            mcu_index += 1;
        }
    }
}

/// Box-averages `plane` (row-major, `width`×`height`) by `(sx, sy)`, producing a
/// `ceil(width/sx)`×`ceil(height/sy)` plane. Partial edge boxes average only the samples that
/// exist (equivalent to edge replication). With `sx == sy == 1` this is an exact copy (4:4:4).
///
/// Box averaging is the encoder's documented free choice; T.81 leaves the subsampling filter open,
/// and T.871 §9 NOTE 1 suggests a simple two-tap `(½, ½)` filter for 2:1.
fn downsample(plane: &[u8], width: usize, height: usize, sx: usize, sy: usize) -> Plane {
    let cw = width.div_ceil(sx);
    let ch = height.div_ceil(sy);
    let mut data = vec![0u8; cw * ch];
    for cy in 0..ch {
        for cx in 0..cw {
            let (mut sum, mut count) = (0u32, 0u32);
            for dy in 0..sy {
                for dx in 0..sx {
                    let px = cx * sx + dx;
                    let py = cy * sy + dy;
                    if px < width && py < height {
                        sum += u32::from(plane[py * width + px]);
                        count += 1;
                    }
                }
            }
            data[cy * cw + cx] = ((sum + count / 2) / count) as u8;
        }
    }
    Plane {
        data,
        width: cw,
        height: ch,
    }
}

impl EncodeImage<Gray8> for JpegEncoder {
    /// Encodes a grayscale image as a single-component (Y) baseline JPEG. Subsampling does not apply
    /// to a one-component image; a JFIF APP0 segment is still written.
    fn encode_image(&self, image: ImageRef<'_, Gray8>, out: &mut Vec<u8>) -> Result<usize> {
        if self.color_mode == JpegColorMode::Xyb {
            // A single-channel image has no XYB representation; silently encoding plain
            // grayscale under an "XYB" setting would misdescribe the output.
            return Err(Error::unsupported(
                env!("CARGO_PKG_NAME"),
                "JPEG: XYB colour mode requires RGB input",
            ));
        }
        let (width, height) = Self::check_dimensions(image.dimensions())?;
        self.check_metadata()?;
        if let Some(written) =
            self.encode_via_backend(width, height, PixelFormat::Gray8, image.as_samples(), out)?
        {
            return Ok(written);
        }
        let start = out.len();

        let plane = Plane {
            data: image.as_samples().to_vec(),
            width: usize::from(width),
            height: usize::from(height),
        };
        let luma_quant = self.luma_quant();
        let luma_rd = self.rd_ctx(false);

        self.write_prologue(out, &[(0, &luma_quant)]);
        if self.progressive {
            let comps = [progressive::ProgComponent {
                id: 1,
                h: 1,
                v: 1,
                tq: 0,
                plane: &plane,
                quant: &luma_quant,
                rd: luma_rd.as_ref(),
            }];
            progressive::encode(out, width, height, &comps, self.restart_interval);
        } else {
            let comp = Component {
                h: 1,
                v: 1,
                plane: &plane,
                quant: &luma_quant,
                dest: 0,
                rd: luma_rd.as_ref(),
            };
            self.write_baseline_frame(out, width, height, &[(1, 1, 1, 0)], &[(1, 0, 0)], &[comp]);
        }

        marker::write_marker(out, marker::code::EOI);
        Ok(out.len() - start)
    }
}

impl EncodeImage<Rgb8> for JpegEncoder {
    /// Encodes an RGB image as a three-component YCbCr baseline JPEG: RGB is converted to full-range
    /// (JFIF) BT.601 YCbCr per T.871 §7, and the chroma planes are subsampled per the configured
    /// [`ChromaSubsampling`].
    fn encode_image(&self, image: ImageRef<'_, Rgb8>, out: &mut Vec<u8>) -> Result<usize> {
        let (width, height) = Self::check_dimensions(image.dimensions())?;
        self.check_metadata()?;
        if let Some(written) =
            self.encode_via_backend(width, height, PixelFormat::Rgb8, image.as_samples(), out)?
        {
            return Ok(written);
        }
        let start = out.len();
        let (w, h) = (usize::from(width), usize::from(height));

        if self.color_mode == JpegColorMode::Xyb {
            self.encode_xyb(image.as_samples(), width, height, out);
            marker::write_marker(out, marker::code::EOI);
            return Ok(out.len() - start);
        }

        // RGB → full-resolution Y/Cb/Cr planes (T.871 §7 full-range BT.601, fixed-point).
        let rgb = image.as_samples();
        let mut y = vec![0u8; w * h];
        let mut cb = vec![0u8; w * h];
        let mut cr = vec![0u8; w * h];
        for i in 0..w * h {
            let (yy, u, v) =
                rgb_to_ycbcr(rgb[i * 3], rgb[i * 3 + 1], rgb[i * 3 + 2], ColorRange::Full);
            y[i] = yy;
            cb[i] = u;
            cr[i] = v;
        }
        let (yh, yv) = self.subsampling.luma_factors();
        let (sx, sy) = (usize::from(yh), usize::from(yv));
        let luma_plane = Plane {
            data: y,
            width: w,
            height: h,
        };
        let cb_plane = downsample(&cb, w, h, sx, sy);
        let cr_plane = downsample(&cr, w, h, sx, sy);

        let luma_quant = self.luma_quant();
        let chroma_quant = self.chroma_quant();
        let luma_rd = self.rd_ctx(false);
        let chroma_rd = self.rd_ctx(true);

        self.write_prologue(out, &[(0, &luma_quant), (1, &chroma_quant)]);
        if self.progressive {
            let comps = [
                progressive::ProgComponent {
                    id: 1,
                    h: yh,
                    v: yv,
                    tq: 0,
                    plane: &luma_plane,
                    quant: &luma_quant,
                    rd: luma_rd.as_ref(),
                },
                progressive::ProgComponent {
                    id: 2,
                    h: 1,
                    v: 1,
                    tq: 1,
                    plane: &cb_plane,
                    quant: &chroma_quant,
                    rd: chroma_rd.as_ref(),
                },
                progressive::ProgComponent {
                    id: 3,
                    h: 1,
                    v: 1,
                    tq: 1,
                    plane: &cr_plane,
                    quant: &chroma_quant,
                    rd: chroma_rd.as_ref(),
                },
            ];
            progressive::encode(out, width, height, &comps, self.restart_interval);
        } else {
            let components = [
                Component {
                    h: yh,
                    v: yv,
                    plane: &luma_plane,
                    quant: &luma_quant,
                    dest: 0,
                    rd: luma_rd.as_ref(),
                },
                Component {
                    h: 1,
                    v: 1,
                    plane: &cb_plane,
                    quant: &chroma_quant,
                    dest: 1,
                    rd: chroma_rd.as_ref(),
                },
                Component {
                    h: 1,
                    v: 1,
                    plane: &cr_plane,
                    quant: &chroma_quant,
                    dest: 1,
                    rd: chroma_rd.as_ref(),
                },
            ];
            self.write_baseline_frame(
                out,
                width,
                height,
                &[(1, yh, yv, 0), (2, 1, 1, 1), (3, 1, 1, 1)],
                &[(1, 0, 0), (2, 1, 1), (3, 1, 1)],
                &components,
            );
        }

        marker::write_marker(out, marker::code::EOI);
        Ok(out.len() - start)
    }
}

/// Emits the DHT segment for a scan: luma DC/AC (destinations 0) always, plus chroma DC/AC
/// (destinations 1) when `color`.
fn emit_huffman_tables(out: &mut Vec<u8>, color: bool) {
    let luma: [(u8, u8, &TableSpec); 2] =
        [(0, 0, &huffman::STD_LUMA_DC), (1, 0, &huffman::STD_LUMA_AC)];
    if color {
        huffman::emit_dht(
            out,
            &[
                luma[0],
                luma[1],
                (0, 1, &huffman::STD_CHROMA_DC),
                (1, 1, &huffman::STD_CHROMA_AC),
            ],
        );
    } else {
        huffman::emit_dht(out, &luma);
    }
}

/// The fixed Annex K.3–K.6 tables as encode tables, laid out by destination to match the segment
/// [`emit_huffman_tables`] writes: luma at destination 0, chroma at destination 1 when `color`.
fn standard_tables(color: bool) -> ScanTables {
    ScanTables {
        dc: [
            Some(EncTable::from_spec(&huffman::STD_LUMA_DC)),
            color.then(|| EncTable::from_spec(&huffman::STD_CHROMA_DC)),
        ],
        ac: [
            Some(EncTable::from_spec(&huffman::STD_LUMA_AC)),
            color.then(|| EncTable::from_spec(&huffman::STD_CHROMA_AC)),
        ],
    }
}

/// Builds the Annex K.2 optimal table for each destination the scan actually used, emits them as
/// **one** DHT segment — the same segment count and position the fixed-table path occupies, in the
/// same `(luma DC, luma AC, chroma DC, chroma AC)` order — and returns them as encode tables.
///
/// A destination whose histogram is empty is omitted from both the segment and the returned tables:
/// nothing in the scan references it, so writing a zero-length table would only cost bytes.
fn optimized_tables(out: &mut Vec<u8>, freq: &Histograms) -> ScanTables {
    // `(Tc, Th, BITS, HUFFVAL)`, in the emission order above.
    let mut built: Vec<(u8, u8, [u8; 16], Vec<u8>)> = Vec::new();
    for dest in 0..2usize {
        for (class, hist) in [(0u8, &freq.dc[dest]), (1u8, &freq.ac[dest])] {
            if hist.iter().all(|&n| n == 0) {
                continue;
            }
            let (bits, values) = gamut_bitstream::build_optimal_table(hist);
            built.push((class, dest as u8, bits, values));
        }
    }

    let segment: Vec<(u8, u8, &[u8; 16], &[u8])> = built
        .iter()
        .map(|(class, dest, bits, values)| (*class, *dest, bits, values.as_slice()))
        .collect();
    huffman::emit_dht_dynamic(out, &segment);

    let mut tables = ScanTables::default();
    for (class, dest, bits, values) in &built {
        let slot = if *class == 0 {
            &mut tables.dc[usize::from(*dest)]
        } else {
            &mut tables.ac[usize::from(*dest)]
        };
        *slot = Some(EncTable::from_bits_values(bits, values));
    }
    tables
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_matches_new() {
        // `Default` must equal `new()`'s configuration: quality 75, 4:2:0, no restart, 1:1 aspect.
        let d = JpegEncoder::default();
        assert_eq!(d.quality, 75);
        assert_eq!(d.subsampling, ChromaSubsampling::Ycbcr420);
        assert_eq!(d.restart_interval, 0);
        assert_eq!(d.density_unit, DensityUnit::AspectRatio);
        assert_eq!((d.x_density, d.y_density), (1, 1));
        assert!(!d.progressive);
        assert!(!d.optimize_tables);
        assert_eq!(d.quant_tables, None);
        assert_eq!(d.rd, RdOptimization::None);
        assert_eq!(d.color_mode, JpegColorMode::Ycbcr);
        assert_eq!((&d.exif, &d.xmp, &d.icc), (&None, &None, &None));
    }

    #[test]
    fn quant_chokepoints_prefer_custom_tables_and_fall_back_to_the_scaled_annex_k() {
        // Distinct luma/chroma fixtures: a swapped arm in either chokepoint changes the result.
        let luma = [7u8; 64];
        let chroma = [11u8; 64];
        let tables = QuantTables::new(luma, chroma).expect("nonzero fixtures");
        let custom = JpegEncoder::new().with_quant_tables(tables);
        assert_eq!(custom.luma_quant(), luma);
        assert_eq!(custom.chroma_quant(), chroma);
        // Quality set after (or before) custom tables must not perturb them.
        assert_eq!(custom.clone().with_quality(5).luma_quant(), luma);
        assert_eq!(custom.with_quality(5).chroma_quant(), chroma);
        // Default path: the frozen scaled Annex K tables, per configured quality.
        let default = JpegEncoder::new().with_quality(85);
        assert_eq!(default.luma_quant(), quant::scale(&quant::LUMINANCE, 85));
        assert_eq!(
            default.chroma_quant(),
            quant::scale(&quant::CHROMINANCE, 85)
        );
    }

    #[test]
    fn with_progressive_toggles_the_flag() {
        assert!(JpegEncoder::new().with_progressive(true).progressive);
        assert!(
            !JpegEncoder::new()
                .with_progressive(true)
                .with_progressive(false)
                .progressive
        );
    }

    #[test]
    fn with_optimized_tables_toggles_the_flag() {
        assert!(
            JpegEncoder::new()
                .with_optimized_tables(true)
                .optimize_tables
        );
        assert!(
            !JpegEncoder::new()
                .with_optimized_tables(true)
                .with_optimized_tables(false)
                .optimize_tables
        );
    }

    #[test]
    fn optimized_tables_omit_destinations_the_scan_never_used() {
        // Only the luma DC destination carries symbols, so the DHT must hold exactly one table —
        // writing empty tables for the other three destinations would be pure overhead, and the
        // returned `ScanTables` must leave them `None` so nothing can code against them.
        let mut freq = Histograms::default();
        freq.dc[0][3] = 7;
        let mut dht = Vec::new();
        let tables = optimized_tables(&mut dht, &freq);

        assert!(tables.dc[0].is_some(), "luma DC was used");
        assert!(tables.ac[0].is_none(), "luma AC saw no symbols");
        assert!(
            tables.dc[1].is_none() && tables.ac[1].is_none(),
            "no chroma"
        );
        // DHT: FFC4, 2-byte length, then one table (1 byte Tc/Th + 16 counts + 1 value).
        assert_eq!(&dht[..2], &[0xFF, marker::code::DHT]);
        assert_eq!(
            u16::from_be_bytes([dht[2], dht[3]]) as usize,
            2 + 1 + 16 + 1,
            "one table, one symbol"
        );
        assert_eq!(dht[4], 0x00, "Tc = 0 (DC), Th = 0 (luma)");
    }

    #[test]
    fn optimized_tables_are_emitted_in_the_standard_destination_order() {
        // The optimized DHT must list (luma DC, luma AC, chroma DC, chroma AC) — the same order the
        // fixed-table segment uses — so a reader sees no structural difference between the two.
        let mut freq = Histograms::default();
        freq.dc[0][1] = 1;
        freq.ac[0][2] = 1;
        freq.dc[1][3] = 1;
        freq.ac[1][4] = 1;
        let mut dht = Vec::new();
        let _ = optimized_tables(&mut dht, &freq);

        // Each single-symbol table occupies 1 + 16 + 1 bytes after the 4-byte marker+length header.
        let tc_th: Vec<u8> = (0..4).map(|i| dht[4 + i * 18]).collect();
        assert_eq!(tc_th, vec![0x00, 0x10, 0x01, 0x11]);
    }

    #[test]
    fn magnitude_category_matches_f_1_2() {
        // Category = bit length of the magnitude; 0 → 0 (F.1.2). Boundaries pin the ">> until zero".
        assert_eq!(magnitude_category(0), 0);
        assert_eq!(magnitude_category(1), 1);
        assert_eq!(magnitude_category(-1), 1);
        assert_eq!(magnitude_category(2), 2);
        assert_eq!(magnitude_category(-2), 2);
        assert_eq!(magnitude_category(7), 3);
        assert_eq!(magnitude_category(-8), 4);
        assert_eq!(magnitude_category(1023), 10);
        assert_eq!(magnitude_category(2047), 11);
    }

    #[test]
    fn additional_bits_positive_and_negative() {
        // Positive: the value's own low bits. Negative: (value − 1)'s low bits (the F.1.2.1 "one
        // lower precision" complement). For category 3: +5 → 0b101 = 5; −5 → (−6) & 0b111 = 0b010.
        assert_eq!(additional_bits(5, 3), 0b101);
        assert_eq!(additional_bits(-5, 3), 0b010);
        // +1 → 1, −1 → 0 (category 1): the canonical smallest pair.
        assert_eq!(additional_bits(1, 1), 1);
        assert_eq!(additional_bits(-1, 1), 0);
        // Category 0 (DC diff of 0) yields no bits.
        assert_eq!(additional_bits(0, 0), 0);
        // Zero is *non-negative*: its own low bits (0), not the negative complement — pins the
        // strict `< 0` test (a `<= 0` mutant would take the −1 branch and yield 0b111).
        assert_eq!(additional_bits(0, 3), 0);
    }

    #[test]
    fn downsample_444_is_identity() {
        let src = [10u8, 20, 30, 40];
        let p = downsample(&src, 2, 2, 1, 1);
        assert_eq!((p.width, p.height), (2, 2));
        assert_eq!(p.data, src);
    }

    #[test]
    fn downsample_420_box_averages_with_rounding() {
        // A 2×2 plane → one sample = round((10+20+30+40)/4) = round(25) = 25.
        let src = [10u8, 20, 30, 40];
        let p = downsample(&src, 2, 2, 2, 2);
        assert_eq!((p.width, p.height), (1, 1));
        assert_eq!(p.data, vec![25]);
        // Odd width → partial edge box averages only existing samples (edge replication).
        // 3×1 plane, sx=2: box0 = round((10+20)/2)=15, box1 = just 30.
        let odd = [10u8, 20, 30];
        let q = downsample(&odd, 3, 1, 2, 1);
        assert_eq!((q.width, q.height), (2, 1));
        assert_eq!(q.data, vec![15, 30]);
    }

    #[test]
    fn downsample_vertical_only() {
        // A 1×4 column with (sx, sy) = (1, 2): box 0 = rows 0–1 → round((10+20)/2) = 15, box 1 =
        // rows 2–3 → round((30+40)/2) = 35. A height-1 output cannot see a broken `cy·sy` source
        // row; box 1 (cy = 1) pins it — a `cy/sy` mutant re-reads rows 0–1 and yields 15, not 35.
        let src = [10u8, 20, 30, 40];
        let p = downsample(&src, 1, 4, 1, 2);
        assert_eq!((p.width, p.height), (1, 2));
        assert_eq!(p.data, vec![15, 35]);
    }

    #[test]
    fn quant_tables_scale_with_quality() {
        // The encoder's per-component tables are the Annex K bases through the frozen IJG mapping —
        // never a placeholder. Anchor entries computed by hand: at q=75 (scale 50),
        // luminance[0]=16 → (16·50+50)/100 = 8; chrominance[0]=17 → (17·50+50)/100 = 9.
        let e = JpegEncoder::new().with_quality(75);
        assert_eq!(e.luma_quant(), quant::scale(&quant::LUMINANCE, 75));
        assert_eq!(e.chroma_quant(), quant::scale(&quant::CHROMINANCE, 75));
        assert_eq!(e.luma_quant()[0], 8);
        assert_eq!(e.chroma_quant()[0], 9);
    }

    #[test]
    fn level_shift_clamps_and_subtracts_128() {
        let plane = Plane {
            data: vec![200u8, 100, 50, 0],
            width: 2,
            height: 2,
        };
        assert_eq!(plane.level_shifted(0, 0), 200 - 128);
        assert_eq!(plane.level_shifted(1, 1), 0 - 128);
        // Past the right/bottom edge replicates the border sample (index 3 = 0 → −128).
        assert_eq!(plane.level_shifted(5, 5), 0 - 128);
        assert_eq!(plane.level_shifted(0, 9), 50 - 128); // clamps y to row 1, col 0
    }

    // --- An in-crate entropy decoder: decodes a scan produced by `encode_scan` back to quantized
    // coefficients, so the DC-difference / AC run-length invariants can be asserted crisply. It is
    // the inverse of the F.1.2 coder and pins it against a family of encode-side mutants. ---

    /// A de-stuffing, MSB-first bit reader over an entropy-coded segment (no restart markers).
    struct BitReader<'a> {
        bytes: &'a [u8],
        pos: usize,
        bit: u8,
    }

    impl BitReader<'_> {
        fn read_bit(&mut self) -> u32 {
            let byte = self.bytes[self.pos];
            let out = u32::from((byte >> (7 - self.bit)) & 1);
            self.bit += 1;
            if self.bit == 8 {
                self.bit = 0;
                self.pos += 1;
                if byte == 0xFF {
                    self.pos += 1; // skip the stuffed 0x00
                }
            }
            out
        }

        fn read_bits(&mut self, n: u8) -> u32 {
            let mut v = 0;
            for _ in 0..n {
                v = (v << 1) | self.read_bit();
            }
            v
        }

        /// Decodes one Huffman symbol against the inverted `(code, len, symbol)` list.
        fn decode_symbol(&mut self, table: &[(u16, u8, u8)]) -> u8 {
            let mut code = 0u16;
            for len in 1..=16u8 {
                code = (code << 1) | self.read_bit() as u16;
                if let Some(&(_, _, sym)) = table.iter().find(|&&(c, l, _)| l == len && c == code) {
                    return sym;
                }
            }
            panic!("no Huffman symbol matched");
        }

        /// Decodes an `SSSS`-bit signed magnitude value (the inverse of [`additional_bits`]).
        fn decode_value(&mut self, category: u8) -> i32 {
            if category == 0 {
                return 0;
            }
            let raw = self.read_bits(category) as i32;
            // Top bit 0 ⇒ negative branch: value = raw − (2^cat − 1).
            if raw < (1 << (category - 1)) {
                raw - ((1 << category) - 1)
            } else {
                raw
            }
        }
    }

    fn invert(table: &EncTable) -> Vec<(u16, u8, u8)> {
        (0..=255u16)
            .filter_map(|s| table.lookup(s as u8).map(|(c, l)| (c, l, s as u8)))
            .collect()
    }

    /// Runs one baseline scan with the fixed Annex K tables and returns just the entropy bytes —
    /// the emit half of [`encode_scan`] without the surrounding markers.
    fn scan_entropy(components: &[Component], width: u32, height: u32, restart: u16) -> Vec<u8> {
        let tables = standard_tables(components.len() > 1);
        let mut out = Vec::new();
        let mut coder = BaselineCoder::emit(&mut out, &tables);
        encode_scan(components, width, height, restart, &mut coder);
        coder.finish();
        out
    }

    /// Decodes `block_count` sequential blocks (one component, one table pair), returning each
    /// block's `(dc_diff, natural-order quantized coefficients)`.
    fn decode_blocks(
        entropy: &[u8],
        dc: &EncTable,
        ac: &EncTable,
        block_count: usize,
    ) -> Vec<(i32, [i32; 64])> {
        let dc_tab = invert(dc);
        let ac_tab = invert(ac);
        let mut reader = BitReader {
            bytes: entropy,
            pos: 0,
            bit: 0,
        };
        let mut blocks = Vec::new();
        for _ in 0..block_count {
            let mut coeffs = [0i32; 64];
            let dc_cat = reader.decode_symbol(&dc_tab);
            let dc_diff = reader.decode_value(dc_cat);
            coeffs[0] = dc_diff; // caller resolves the running DC prediction
            let mut k = 1usize;
            while k < 64 {
                let rs = reader.decode_symbol(&ac_tab);
                let (run, size) = (rs >> 4, rs & 0x0F);
                if size == 0 {
                    if run == 15 {
                        k += 16; // ZRL
                        continue;
                    }
                    break; // EOB
                }
                k += run as usize;
                coeffs[ZIGZAG[k]] = reader.decode_value(size);
                k += 1;
            }
            blocks.push((dc_diff, coeffs));
        }
        blocks
    }

    #[test]
    fn constant_image_dc_predicts_and_ac_is_empty() {
        // A 16×16 constant plane is four identical 8×8 Y blocks. The first block carries the full DC
        // difference from the zero predictor; the next three predict perfectly, so their DC diff is
        // exactly 0 (category-0 code, no magnitude bits) — the observable §F.1.2.1 prediction. Every
        // block's AC is empty (immediate EOB).
        let plane = Plane {
            data: vec![200u8; 16 * 16],
            width: 16,
            height: 16,
        };
        let quant = quant::scale(&quant::LUMINANCE, 50);
        let dc = EncTable::from_spec(&huffman::STD_LUMA_DC);
        let ac = EncTable::from_spec(&huffman::STD_LUMA_AC);
        let comp = Component {
            h: 1,
            v: 1,
            plane: &plane,
            quant: &quant,
            dest: 0,
            rd: None,
        };

        let entropy = scan_entropy(&[comp], 16, 16, 0);
        let blocks = decode_blocks(&entropy, &dc, &ac, 4);

        // Independent expected DC: round((200−128)·8 / 16) = round(576/16) = 36.
        let expected_dc = round_div_nearest((200 - 128) * 8, i32::from(quant[0]));
        assert_eq!(expected_dc, 36);
        assert_eq!(blocks[0].0, 36, "first block DC diff = quantized DC");
        for b in &blocks[1..] {
            assert_eq!(
                b.0, 0,
                "subsequent identical blocks predict to zero DC diff"
            );
        }
        for (_, coeffs) in &blocks {
            assert!(
                coeffs[1..].iter().all(|&c| c == 0),
                "constant block has no AC"
            );
        }
    }

    #[test]
    fn single_horizontal_frequency_lights_one_ac_coefficient() {
        // A pure horizontal cosine at the lowest AC frequency puts energy only in coefficient u=1,
        // v=0 (natural index 1). Decoding the block must show exactly that coefficient nonzero (plus
        // the DC term), pinning the zig-zag mapping and the run/size AC path.
        let mut data = vec![0u8; 8 * 8];
        for y in 0..8 {
            for x in 0..8 {
                // 128 + 100·cos((2x+1)π/16): a single-frequency horizontal wave, constant per column.
                let v = 128.0 + 100.0 * (((2 * x + 1) as f64) * std::f64::consts::PI / 16.0).cos();
                data[y * 8 + x] = v.round().clamp(0.0, 255.0) as u8;
            }
        }
        let plane = Plane {
            data,
            width: 8,
            height: 8,
        };
        let quant = quant::scale(&quant::LUMINANCE, 50);
        let dc = EncTable::from_spec(&huffman::STD_LUMA_DC);
        let ac = EncTable::from_spec(&huffman::STD_LUMA_AC);
        let comp = Component {
            h: 1,
            v: 1,
            plane: &plane,
            quant: &quant,
            dest: 0,
            rd: None,
        };

        let entropy = scan_entropy(&[comp], 8, 8, 0);
        let (_, coeffs) = decode_blocks(&entropy, &dc, &ac, 1)[0];

        assert_ne!(coeffs[1], 0, "the u=1 coefficient must be lit");
        for (i, &c) in coeffs.iter().enumerate() {
            if i != 0 && i != 1 {
                assert_eq!(c, 0, "unexpected energy at natural index {i}");
            }
        }
    }

    // --- Direct §F.1.2 entropy-coder tests: feed `encode_quantized_block` hand-built coefficient
    // arrays and assert the exact emitted bytes against a hand-listed (code, length) sequence from
    // the standard tables (K.3/K.5 anchors are pinned in `huffman`'s own tests). ---

    /// Entropy-codes one hand-built block with the standard luma tables, flushing at the end.
    fn encode_one(q: &[i32; 64], dc_pred: &mut i32) -> Vec<u8> {
        let tables = standard_tables(false);
        let mut out = Vec::new();
        let mut coder = BaselineCoder::emit(&mut out, &tables);
        encode_quantized_block(q, dc_pred, 0, &mut coder);
        coder.finish();
        out
    }

    /// Packs a hand-listed `(bits, length)` sequence with 1-padding — the expected-stream builder.
    fn expect_bits(seq: &[(u16, u8)]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut w = BitWriter::new(&mut out);
        for &(v, n) in seq {
            w.write_bits(v, n);
        }
        w.flush();
        out
    }

    /// A block whose only nonzero AC sits after exactly `zeros` zig-zag zeros, with value 1.
    fn block_with_zero_run(zeros: usize) -> [i32; 64] {
        let mut q = [0i32; 64];
        q[ZIGZAG[zeros + 1]] = 1;
        q
    }

    // Standard-table codes used below (see huffman.rs tests for their Annex C derivations):
    //   luma DC cat 0 = 00₂;  luma AC ZRL (F/0) = 11111111001₂ (11 bits);
    //   0/1 = 00₂;  1/1 = 1100₂;  4/1 = 111011₂ (6 bits);  0/2 = 01₂;  EOB = 1010₂.
    const DC0: (u16, u8) = (0b00, 2);
    const ZRL: (u16, u8) = (0b111_1111_1001, 11);
    const EOB: (u16, u8) = (0b1010, 4);

    #[test]
    fn zero_run_of_exactly_16_is_one_zrl() {
        // 16 zeros then +1: ZRL, then 0/1 (run 0 after the ZRL) with one magnitude bit, then EOB.
        let got = encode_one(&block_with_zero_run(16), &mut 0);
        assert_eq!(got, expect_bits(&[DC0, ZRL, (0b00, 2), (1, 1), EOB]));
        // Literal anchor, fully hand-packed: 00|11111111001|00|1|1010|1111 → 3F C9 AF.
        assert_eq!(got, vec![0x3F, 0xC9, 0xAF]);
    }

    #[test]
    fn zero_runs_of_17_and_20_leave_a_remainder_run() {
        // 17 zeros: ZRL eats 16, the remaining run of 1 joins the symbol → 1/1 = 1100₂.
        assert_eq!(
            encode_one(&block_with_zero_run(17), &mut 0),
            expect_bits(&[DC0, ZRL, (0b1100, 4), (1, 1), EOB])
        );
        // 20 zeros: ZRL then run 4 → 4/1 = 111011₂. Distinguishes `run -= 16` from `run /= 16`
        // (both give run 1 at 17 zeros — 20 zeros is the case where they diverge: 4 vs 1).
        assert_eq!(
            encode_one(&block_with_zero_run(20), &mut 0),
            expect_bits(&[DC0, ZRL, (0b111011, 6), (1, 1), EOB])
        );
    }

    #[test]
    fn zero_run_of_33_is_two_zrls() {
        // 33 zeros: two ZRLs (32 zeros) then run 1 → 1/1. A `run /= 16` mutant emits only one ZRL.
        assert_eq!(
            encode_one(&block_with_zero_run(33), &mut 0),
            expect_bits(&[DC0, ZRL, ZRL, (0b1100, 4), (1, 1), EOB])
        );
    }

    #[test]
    fn trailing_zeros_end_in_eob() {
        // Natural index 1 (zig-zag position 1) holds +3 (category 2 → 0/2 = 01₂, bits 11₂); the 62
        // trailing zeros collapse into a single EOB.
        let mut q = [0i32; 64];
        q[1] = 3;
        assert_eq!(
            encode_one(&q, &mut 0),
            expect_bits(&[DC0, (0b01, 2), (0b11, 2), EOB])
        );
    }

    #[test]
    fn nonzero_last_coefficient_suppresses_eob() {
        // Every AC coefficient +1: 63 consecutive 0/1 symbols and NO EOB (§F.1.2.2 — EOB is sent
        // only when the block ends in zeros). A `run > 0` → `run >= 0` mutant appends a spurious
        // EOB, changing the bytes.
        let mut q = [1i32; 64];
        q[0] = 0;
        let mut seq = vec![DC0];
        for _ in 0..63 {
            seq.push((0b00, 2)); // 0/1
            seq.push((1, 1)); // magnitude +1
        }
        assert_eq!(encode_one(&q, &mut 0), expect_bits(&seq));
    }

    #[test]
    fn dc_differences_across_blocks() {
        // Three blocks with absolute DCs 5, 2, 2 sharing one predictor: diffs +5 (cat 3, DC code
        // 100₂, bits 101₂), −3 (cat 2, DC code 011₂, bits (−3−1)&11₂ = 00₂), 0 (cat 0, no bits).
        let mut out = Vec::new();
        let tables = standard_tables(false);
        let mut coder = BaselineCoder::emit(&mut out, &tables);
        let mut pred = 0i32;
        for dc_value in [5, 2, 2] {
            let mut q = [0i32; 64];
            q[0] = dc_value;
            encode_quantized_block(&q, &mut pred, 0, &mut coder);
        }
        coder.finish();
        assert_eq!(pred, 2, "predictor tracks the last absolute DC");
        assert_eq!(
            out,
            expect_bits(&[
                (0b100, 3),
                (0b101, 3),
                EOB, // block 1: cat 3, +5
                (0b011, 3),
                (0b00, 2),
                EOB,       // block 2: cat 2, −3
                (0b00, 2), // block 3: cat 0, no magnitude bits
                EOB,
            ])
        );
    }

    // --- Reference-pipeline tests: encode per-pixel-distinct images, decode the scan with the
    // test decoder, and compare every block against an independently computed expectation
    // (gather with edge replication → fdct8x8 → round_div_nearest quantize). Distinct content is
    // the point: any mutated pixel/block/MCU coordinate reads different samples somewhere and
    // diverges. Solid colors could not see those mutants. ---

    /// The test-side reference for one 8×8 block of `plane` at block coords `(bx, by)`: the same
    /// §A.3.1/§A.3.3/§A.3.4 stages, written independently of the production gather.
    fn reference_block(plane: &Plane, bx: usize, by: usize, quant: &[u8; 64]) -> [i32; 64] {
        let mut block = [0i32; 64];
        for (i, cell) in block.iter_mut().enumerate() {
            let x = (bx * 8 + i % 8).min(plane.width - 1);
            let y = (by * 8 + i / 8).min(plane.height - 1);
            *cell = i32::from(plane.data[y * plane.width + x]) - 128;
        }
        fdct8x8(&mut block);
        let mut q = [0i32; 64];
        for (dst, (&coeff, &step)) in q.iter_mut().zip(block.iter().zip(quant.iter())) {
            *dst = round_div_nearest(coeff, i32::from(step));
        }
        q
    }

    /// Decodes an interleaved scan (no restart markers): one `(h, v, dc, ac)` per component.
    /// Returns, per component, the quantized blocks in emission order with the DC prediction
    /// resolved to absolute values.
    fn decode_interleaved(
        entropy: &[u8],
        comps: &[(u8, u8, &EncTable, &EncTable)],
        mcu_count: u32,
    ) -> Vec<Vec<[i32; 64]>> {
        let tables: Vec<_> = comps
            .iter()
            .map(|(_, _, dc, ac)| (invert(dc), invert(ac)))
            .collect();
        let mut reader = BitReader {
            bytes: entropy,
            pos: 0,
            bit: 0,
        };
        let mut preds = vec![0i32; comps.len()];
        let mut out = vec![Vec::new(); comps.len()];
        for _ in 0..mcu_count {
            for (ci, &(h, v, _, _)) in comps.iter().enumerate() {
                for _ in 0..usize::from(h) * usize::from(v) {
                    let mut coeffs = [0i32; 64];
                    let dc_cat = reader.decode_symbol(&tables[ci].0);
                    preds[ci] += reader.decode_value(dc_cat);
                    coeffs[0] = preds[ci];
                    let mut k = 1usize;
                    while k < 64 {
                        let rs = reader.decode_symbol(&tables[ci].1);
                        let (run, size) = (rs >> 4, rs & 0x0F);
                        if size == 0 {
                            if run == 15 {
                                k += 16; // ZRL
                                continue;
                            }
                            break; // EOB
                        }
                        k += run as usize;
                        coeffs[ZIGZAG[k]] = reader.decode_value(size);
                        k += 1;
                    }
                    out[ci].push(coeffs);
                }
            }
        }
        out
    }

    /// A deterministic per-pixel-distinct byte pattern (no two neighbours equal, varies on both
    /// axes) so every source coordinate is load-bearing.
    fn pattern(i: usize) -> u8 {
        ((i * 31 + 17) % 251) as u8
    }

    #[test]
    fn grayscale_blocks_match_reference_pipeline() {
        // 16×16 per-pixel-distinct grayscale = 2×2 blocks, emitted row-major. Every decoded block
        // must equal the independent reference at its (bx, by) — any mutated sample/block
        // coordinate in the production gather reads different pixels and diverges.
        let plane = Plane {
            data: (0..16 * 16).map(pattern).collect(),
            width: 16,
            height: 16,
        };
        let quant = quant::scale(&quant::LUMINANCE, 50);
        let dc = EncTable::from_spec(&huffman::STD_LUMA_DC);
        let ac = EncTable::from_spec(&huffman::STD_LUMA_AC);
        let comp = Component {
            h: 1,
            v: 1,
            plane: &plane,
            quant: &quant,
            dest: 0,
            rd: None,
        };
        let entropy = scan_entropy(&[comp], 16, 16, 0);

        let decoded = decode_interleaved(&entropy, &[(1, 1, &dc, &ac)], 4);
        let mut n = 0;
        for by in 0..2 {
            for bx in 0..2 {
                assert_eq!(
                    decoded[0][n],
                    reference_block(&plane, bx, by, &quant),
                    "block ({bx},{by})"
                );
                n += 1;
            }
        }
    }

    #[test]
    fn color_444_blocks_match_reference_pipeline() {
        // 8×8 per-pixel-distinct RGB at 4:4:4: one MCU with block order Y, Cb, Cr. The reference
        // converts each pixel with the same T.871 §7 conversion in an independent loop, so any
        // mutation of the production `i*3(+1/+2)` channel indexing or the `w*h` conversion loop
        // bound produces different planes and diverges.
        let (w, h) = (8usize, 8usize);
        let rgb: Vec<u8> = (0..w * h * 3).map(pattern).collect();
        let img = ImageRef::<Rgb8>::new(&rgb, Dimensions::new(8, 8).unwrap()).unwrap();
        let jpeg = JpegEncoder::new()
            .with_quality(50)
            .with_subsampling(ChromaSubsampling::Ycbcr444)
            .encode_to_vec(img)
            .unwrap();
        let entropy = entropy_of(&jpeg);

        // Independent plane conversion.
        let mut planes = [
            Plane {
                data: vec![0; w * h],
                width: w,
                height: h,
            },
            Plane {
                data: vec![0; w * h],
                width: w,
                height: h,
            },
            Plane {
                data: vec![0; w * h],
                width: w,
                height: h,
            },
        ];
        for py in 0..h {
            for px in 0..w {
                let i = py * w + px;
                let (y, cb, cr) =
                    rgb_to_ycbcr(rgb[3 * i], rgb[3 * i + 1], rgb[3 * i + 2], ColorRange::Full);
                planes[0].data[i] = y;
                planes[1].data[i] = cb;
                planes[2].data[i] = cr;
            }
        }

        let lq = quant::scale(&quant::LUMINANCE, 50);
        let cq = quant::scale(&quant::CHROMINANCE, 50);
        let ldc = EncTable::from_spec(&huffman::STD_LUMA_DC);
        let lac = EncTable::from_spec(&huffman::STD_LUMA_AC);
        let cdc = EncTable::from_spec(&huffman::STD_CHROMA_DC);
        let cac = EncTable::from_spec(&huffman::STD_CHROMA_AC);
        let decoded = decode_interleaved(
            &entropy,
            &[(1, 1, &ldc, &lac), (1, 1, &cdc, &cac), (1, 1, &cdc, &cac)],
            1,
        );
        for (ci, quant) in [(0usize, &lq), (1, &cq), (2, &cq)] {
            assert_eq!(
                decoded[ci][0],
                reference_block(&planes[ci], 0, 0, quant),
                "component {ci}"
            );
        }
    }

    #[test]
    fn color_420_multi_mcu_matches_reference_pipeline() {
        // 32×32 per-pixel-distinct RGB at 4:2:0: 2×2 MCUs of 16×16, each carrying four luma blocks
        // (bx, by) ∈ 2×2 at plane coords (mx·2+bx, my·2+by) plus one block per chroma plane at
        // (mx, my). Comparing every block in emission order against the reference pins the MCU
        // block-coordinate arithmetic on both axes for h = v = 2 — including the vertical terms,
        // which the single-MCU-row cases can never distinguish (my = 0 masks `my*v` mutations).
        let (w, h) = (32usize, 32usize);
        let rgb: Vec<u8> = (0..w * h * 3).map(pattern).collect();
        let img = ImageRef::<Rgb8>::new(&rgb, Dimensions::new(32, 32).unwrap()).unwrap();
        let jpeg = JpegEncoder::new()
            .with_quality(50)
            .with_subsampling(ChromaSubsampling::Ycbcr420)
            .encode_to_vec(img)
            .unwrap();
        let entropy = entropy_of(&jpeg);

        // Independent plane conversion; chroma is then box-downsampled 2×2 (the pinned-elsewhere
        // production `downsample` is reused so this test focuses on the scan geometry).
        let mut y = vec![0u8; w * h];
        let mut cb = vec![0u8; w * h];
        let mut cr = vec![0u8; w * h];
        for i in 0..w * h {
            let (yy, u, v) =
                rgb_to_ycbcr(rgb[3 * i], rgb[3 * i + 1], rgb[3 * i + 2], ColorRange::Full);
            y[i] = yy;
            cb[i] = u;
            cr[i] = v;
        }
        let y_plane = Plane {
            data: y,
            width: w,
            height: h,
        };
        let cb_plane = downsample(&cb, w, h, 2, 2);
        let cr_plane = downsample(&cr, w, h, 2, 2);

        let lq = quant::scale(&quant::LUMINANCE, 50);
        let cq = quant::scale(&quant::CHROMINANCE, 50);
        let ldc = EncTable::from_spec(&huffman::STD_LUMA_DC);
        let lac = EncTable::from_spec(&huffman::STD_LUMA_AC);
        let cdc = EncTable::from_spec(&huffman::STD_CHROMA_DC);
        let cac = EncTable::from_spec(&huffman::STD_CHROMA_AC);
        let decoded = decode_interleaved(
            &entropy,
            &[(2, 2, &ldc, &lac), (1, 1, &cdc, &cac), (1, 1, &cdc, &cac)],
            4,
        );

        let mut luma_n = 0;
        let mut chroma_n = 0;
        for my in 0..2 {
            for mx in 0..2 {
                for by in 0..2 {
                    for bx in 0..2 {
                        assert_eq!(
                            decoded[0][luma_n],
                            reference_block(&y_plane, mx * 2 + bx, my * 2 + by, &lq),
                            "luma MCU ({mx},{my}) block ({bx},{by})"
                        );
                        luma_n += 1;
                    }
                }
                assert_eq!(
                    decoded[1][chroma_n],
                    reference_block(&cb_plane, mx, my, &cq),
                    "Cb MCU ({mx},{my})"
                );
                assert_eq!(
                    decoded[2][chroma_n],
                    reference_block(&cr_plane, mx, my, &cq),
                    "Cr MCU ({mx},{my})"
                );
                chroma_n += 1;
            }
        }
    }

    /// Extracts the entropy-coded bytes of a full JPEG stream: everything between the SOS segment
    /// and the trailing EOI, returned raw (still stuffed — the test decoder de-stuffs itself).
    fn entropy_of(jpeg: &[u8]) -> Vec<u8> {
        // Walk the header segments to find the end of SOS.
        let mut pos = 2; // past SOI
        loop {
            assert_eq!(jpeg[pos], 0xFF);
            let code = jpeg[pos + 1];
            let len = usize::from(u16::from_be_bytes([jpeg[pos + 2], jpeg[pos + 3]]));
            pos += 2 + len;
            if code == marker::code::SOS {
                break;
            }
        }
        jpeg[pos..jpeg.len() - 2].to_vec()
    }
}
