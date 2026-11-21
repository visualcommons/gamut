//! [`JpegEncoder`]'s builder: the constructor, every `with_*` setter, and the configuration they
//! resolve to at encode time — the quantization tables, the rate–distortion context, and the
//! dimension and metadata checks that run before any byte is written.

use std::sync::{Arc, Mutex};

use gamut_color::ChromaSubsampling;
use gamut_core::{Dimensions, Error, Result};
#[cfg(doc)]
use gamut_core::{Gray8, Rgb8};

use super::{JpegColorMode, JpegEncoder, MAX_DIMENSION, Process, RdOptimization};
#[cfg(doc)]
use super::{ProgressiveScript, XYB_ICC_PROFILE};
use crate::appmeta;
#[cfg(doc)]
use crate::backend::JpegEncodeRequest;
use crate::backend::{EncoderRegistry, JpegCodestreamEncoder};
use crate::huffman::{self, EncTable};
use crate::marker::DensityUnit;
use crate::quant::{self, JpegliFrame, QuantTables};
use crate::rd::RdCtx;

/// The frame layout whose quantization tables [`JpegEncoder::frame_quant`] resolves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Frame {
    /// One grayscale component.
    Gray,
    /// Y, Cb, Cr; `luma` is Y's `(h, v)` sampling factors (Cb and Cr are 1×1).
    Ycbcr {
        /// Y's sampling factors.
        luma: (u8, u8),
    },
    /// The XYB colour mode's X, Y and stored B−Y.
    Xyb,
}

/// The quantization tables one frame writes, and the destination (`Tq`) each component reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct FrameQuant {
    /// The DQT tables, natural order: `tables[t]` is destination `t`.
    tables: Vec<[u16; 64]>,
    /// Each frame component's destination, in frame component order (grayscale reads `tq[0]`).
    tq: [u8; 3],
}

impl FrameQuant {
    /// The `(Tq, table)` pairs the DQT segment carries, in destination order.
    pub(super) fn dqt(&self) -> Vec<(u8, &[u16; 64])> {
        (0u8..).zip(&self.tables).collect()
    }

    /// The destination frame component `c` reads.
    pub(super) fn tq(&self, c: usize) -> u8 {
        self.tq[c]
    }

    /// The table frame component `c` quantizes with, natural order.
    pub(super) fn table(&self, c: usize) -> &[u16; 64] {
        &self.tables[usize::from(self.tq[c])]
    }
}

impl JpegEncoder {
    /// Creates an encoder with quality 75, [`ChromaSubsampling::Cs420`], [`Process::Baseline`], no
    /// restart interval, and a 1:1 aspect-ratio pixel density (JFIF `units = 0`).
    #[must_use]
    pub fn new() -> Self {
        Self {
            quality: 75,
            distance: None,
            subsampling: ChromaSubsampling::Cs420,
            restart_interval: 0,
            density_unit: DensityUnit::AspectRatio,
            x_density: 1,
            y_density: 1,
            process: Process::Baseline,
            quant_tables: None,
            rd: RdOptimization::None,
            color_mode: JpegColorMode::Ycbcr,
            exif: None,
            xmp: None,
            icc: None,
            backends: EncoderRegistry::new(),
        }
    }

    /// Appends a [`JpegCodestreamEncoder`] backend to this encoder's registry and returns the
    /// encoder.
    ///
    /// Backends are consulted in **push order** for every encode; the first whose
    /// [`JpegCodestreamEncoder::supports`] accepts the [`JpegEncodeRequest`] produces the whole JFIF
    /// interchange stream. The built-in encoder is the implicit tail, used only when every backend
    /// declines; a backend that accepts and then fails returns its error unchanged. An encoder
    /// configured with [`Self::with_quant_tables`], [`Self::with_rd_optimization`], the XYB colour
    /// mode or a non-default [`ProgressiveScript`] skips the registry entirely — a
    /// [`JpegEncodeRequest`] cannot carry them, so the job is pinned to the built-in path. The crate
    /// then **patches its APPn metadata into the produced stream** — any EXIF/XMP/`ICC_PROFILE`
    /// segment the backend emitted is replaced by this encoder's configured
    /// [`with_exif`](Self::with_exif) / [`with_xmp`](Self::with_xmp) /
    /// [`with_icc_profile`](Self::with_icc_profile) payloads (validated against their caps *before*
    /// any backend runs).
    ///
    /// `Send` is bound here, where the backend is inserted, rather than on the trait, so the
    /// encoder stays `Send + Sync`.
    ///
    /// # Cloning shares backends
    ///
    /// Backends are stored behind `Arc<Mutex<..>>`, so **cloning a `JpegEncoder` shares them with
    /// the clone** rather than duplicating them: a stateful backend sees the encodes of every clone,
    /// and its lock serializes concurrent ones. A backend added to one encoder after the clone is
    /// not seen by the other.
    #[must_use]
    pub fn with_backend(mut self, backend: impl JpegCodestreamEncoder + Send + 'static) -> Self {
        self.backends.push(Arc::new(Mutex::new(backend)));
        self
    }

    /// Sets the quality, **clamped** to `1..=100` (higher is better/larger). Quality 50 uses the
    /// Annex K tables verbatim; 100 uses all-1 tables. Clamping (rather than rejecting) matches
    /// libjpeg's `jpeg_set_quality`. Ignored when [`Self::with_quant_tables`] is set.
    ///
    /// Quality and [`Self::with_distance`] are the two quality controls; whichever is set last
    /// governs, so this clears a distance set earlier.
    #[must_use]
    pub fn with_quality(mut self, quality: u8) -> Self {
        self.quality = quality.clamp(1, 100);
        self.distance = None;
        self
    }

    /// Selects jpegli's quality control: the quantization tables `jpegli_set_distance` builds for
    /// a butteraugli `distance` (lower is better/larger), instead of the quality-scaled Annex K
    /// tables of [`Self::with_quality`].
    ///
    /// The tables are jpegli's own (libjxl v0.11.2 `lib/jpegli/quant.cc`, cited in
    /// `references/jpegli/`), entry for entry what `cjpegli --distance` writes, in every colour
    /// mode: YCbCr gets three tables — Y, Cb and Cr each at its own destination, the chroma pair
    /// rescaled for [`ChromaSubsampling::Cs420`] — [`JpegColorMode::Xyb`] one per channel (X, Y,
    /// B, jpegli's XYB-tuned tables), and grayscale the Annex K luminance table scaled linearly.
    /// Every entry is clamped to `1..=255` (jpegli's `force_baseline`), so a sequential frame
    /// stays baseline (SOF0). [`jpegli_quality_to_distance`](crate::jpegli_quality_to_distance)
    /// maps a `cjpegli -q` quality to its distance.
    ///
    /// Only the tables are jpegli's. Coefficients are still chosen by this encoder's own
    /// quantizer ([`Self::with_rd_optimization`]): jpegli's adaptive quantization is not
    /// reproduced, so the coded bytes differ from `cjpegli`'s. The tables are computed in `f32`
    /// with the platform's `powf`, as jpegli's are, so — unlike the frozen quality contract —
    /// they are not guaranteed byte-stable across platforms.
    ///
    /// The distance must be finite and non-negative; anything else fails the encode with
    /// `InvalidInput` before a byte is written. [`Self::with_quality`] called afterwards replaces
    /// it, [`Self::with_quant_tables`] overrides it, and pushed backends are not consulted (a
    /// [`JpegEncodeRequest`] cannot carry it).
    #[must_use]
    pub fn with_distance(mut self, distance: f32) -> Self {
        self.distance = Some(distance);
        self
    }

    /// Quantizes with `tables` **verbatim** instead of the quality-scaled Annex K tables,
    /// replacing the only lever [`Self::with_quality`] offers — the frozen IJG mapping — with full
    /// caller control (perceptually-tuned tables, near-lossless all-1 tables, or
    /// [`QuantTables::scaled`] re-scaling of an arbitrary base).
    ///
    /// While set, `with_quality` has no effect on the emitted tables or coefficients, and the
    /// encode is pinned to the **built-in** encoder: pushed backends are not consulted, because a
    /// [`JpegEncodeRequest`] cannot carry custom tables. Grayscale uses only the
    /// luma table. In [`JpegColorMode::Xyb`] a [`QuantTables::with_xyb_x`] table quantizes the X
    /// channel at its own destination (`Tq = 2`); other modes ignore it. A table the stream
    /// emits with any entry above 255 is written 16-bit (`Pq = 1`) and makes a sequential frame
    /// extended (SOF1) instead of baseline ([`QuantTables`]). The frozen quality contract is
    /// unaffected — it continues to govern the default path.
    #[must_use]
    pub fn with_quant_tables(mut self, tables: QuantTables) -> Self {
        self.quant_tables = Some(tables);
        self
    }

    /// Sets the chroma [`ChromaSubsampling`] used for colour ([`Rgb8`]) input. No effect on
    /// grayscale. In [`JpegColorMode::Xyb`] it subsamples the B channel alone
    /// ([`Self::with_color_mode`]).
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

    /// Selects the coding [`Process`]: baseline with the typical tables (the default), baseline
    /// with tables fitted to the image ([`Process::Optimized`]), or progressive with a
    /// [`ProgressiveScript`] ([`Process::Progressive`]).
    ///
    /// The quantized coefficients — and therefore the decoded image — are identical whichever is
    /// chosen at the same `(quality, subsampling)`; only the stream structure and size differ.
    /// [`ProgressiveScript::Libjpeg`] is the frozen progressive script, whose byte stream is
    /// stable; [`ProgressiveScript::Mozjpeg`] trades it for mozjpeg's maximum-compression script,
    /// usually a little smaller, and pins the encode to the **built-in** encoder — a
    /// [`JpegEncodeRequest`] offered to a backend never carries a script other than the frozen
    /// one.
    #[must_use]
    pub fn with_process(mut self, process: Process) -> Self {
        self.process = process;
        self
    }

    /// Selects the colour space colour ([`Rgb8`]) input is coded in ([`JpegColorMode`]).
    ///
    /// The default [`JpegColorMode::Ycbcr`] is the frozen JFIF path. [`JpegColorMode::Xyb`] codes
    /// jpegli-style XYB samples with [`XYB_ICC_PROFILE`] embedded — a perceptual space that
    /// out-compresses YCbCr on an ICC-aware pipeline. In XYB mode: [`Self::with_subsampling`]
    /// subsamples the **B** channel alone (the stored B−Y) by the variant's ratio while X and Y
    /// stay at full resolution, so the default [`ChromaSubsampling::Cs420`] is jpegli's own
    /// XYB default — sampling factors X 2×2, Y 2×2, B 1×1 (`jpegli_set_colorspace`, libjxl
    /// `lib/jpegli/encode.cc`) — and [`ChromaSubsampling::Cs444`] codes all three channels at
    /// full resolution (X carries opponent-colour detail jpegli never subsamples, so no variant
    /// touches it), [`Self::with_density`] is
    /// inert (no JFIF APP0 is written), [`Self::with_icc_profile`] is rejected at encode time (a
    /// caller profile would misdescribe the XYB samples; EXIF/XMP stay available), grayscale
    /// ([`Gray8`]) input is rejected as unsupported, and pushed backends are not consulted.
    /// Progressive mode, restart intervals, optimized tables, custom quantization tables, and RD
    /// optimization all compose. Decoding an XYB stream (with this crate's decoder or any other)
    /// yields the scaled-XYB samples presented as RGB plus the embedded profile via
    /// [`JpegDecoder::metadata`](crate::JpegDecoder::metadata); applying the profile is a CMM's job — with the `cmm` feature,
    /// `JpegDecoder::decode_srgb` does it through `gamut-cmm`.
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
    /// custom [`Self::with_quant_tables`], [`Process::Optimized`] (rates are still costed
    /// against the typical-table proxy; the emitted tables then fit whatever the trellis chose),
    /// and [`Process::Progressive`] (the progressive stream carries the same trellis-chosen
    /// coefficients as the baseline stream, preserving the exactness invariant). While set to a
    /// non-`None` mode the encode is pinned to the **built-in** encoder — a
    /// [`JpegEncodeRequest`] cannot carry the RD configuration.
    #[must_use]
    pub fn with_rd_optimization(mut self, rd: RdOptimization) -> Self {
        self.rd = rd;
        self
    }

    /// Embeds EXIF metadata as an APP1 segment (`"Exif\0\0"` + TIFF stream, Exif 3.0 §4.7.2).
    ///
    /// `exif` is the TIFF stream beginning `II`/`MM` — e.g. `gamut-exif` output; a blob already
    /// carrying the `"Exif\0\0"` signature is accepted and not double-prefixed, so bytes read by
    /// [`JpegDecoder::metadata`](crate::JpegDecoder::metadata) round-trip verbatim. Must fit the single APP1 segment (at most 65527
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

    /// The quantization tables a `frame` writes and the destination each of its components reads,
    /// resolved from the configuration: caller tables ([`Self::with_quant_tables`]) first, then a
    /// jpegli distance ([`Self::with_distance`]), then the frozen quality mapping.
    ///
    /// Fails with `InvalidInput` for a distance that is not finite and non-negative, so the
    /// encoders call this before writing anything.
    pub(super) fn frame_quant(&self, frame: Frame) -> Result<FrameQuant> {
        let tables = match (self.quant_tables, self.distance) {
            (Some(tables), _) => tables,
            (None, Some(distance)) => {
                if !(distance.is_finite() && distance >= 0.0) {
                    return Err(Error::invalid_input(
                        env!("CARGO_PKG_NAME"),
                        "JPEG: distance must be finite and non-negative",
                    ));
                }
                let jpegli = match frame {
                    Frame::Gray => JpegliFrame::Gray,
                    Frame::Ycbcr { luma } => JpegliFrame::Ycbcr {
                        yuv420: luma == (2, 2),
                    },
                    Frame::Xyb => JpegliFrame::Xyb,
                };
                // jpegli gives every component a table of its own: destination c for component c.
                return Ok(FrameQuant {
                    tables: quant::jpegli_tables(distance, jpegli),
                    tq: [0, 1, 2],
                });
            }
            (None, None) => QuantTables::annex_k().scaled(self.quality),
        };
        let (luma, chroma) = (*tables.luma(), *tables.chroma());
        Ok(match (frame, tables.xyb_x()) {
            (Frame::Gray, _) => FrameQuant {
                tables: vec![luma],
                tq: [0, 0, 0],
            },
            (Frame::Ycbcr { .. }, _) => FrameQuant {
                tables: vec![luma, chroma],
                tq: [0, 1, 1],
            },
            // X quantizes with its own table at destination 2 when the caller supplied one;
            // otherwise it shares Y's luminance table at destination 0.
            (Frame::Xyb, Some(&x)) => FrameQuant {
                tables: vec![luma, chroma, x],
                tq: [2, 0, 1],
            },
            (Frame::Xyb, None) => FrameQuant {
                tables: vec![luma, chroma],
                tq: [0, 0, 1],
            },
        })
    }

    /// The rate–distortion context for one component class (`chroma` selects the typical AC rate
    /// proxy: Annex K.6 rather than K.5), or `None` when RD optimization is off.
    pub(super) fn rd_ctx(&self, chroma: bool) -> Option<RdCtx> {
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
    pub(super) fn check_dimensions(dims: Dimensions) -> Result<(u16, u16)> {
        if dims.width() > MAX_DIMENSION || dims.height() > MAX_DIMENSION {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "JPEG: image exceeds 65535×65535",
            ));
        }
        Ok((dims.width() as u16, dims.height() as u16))
    }

    /// Rejects metadata payloads the APPn framing cannot carry (the caps in [`crate::appmeta`]),
    /// so [`Self::write_prologue`] stays infallible and no bytes are written before the check.
    pub(super) fn check_metadata(&self) -> Result<()> {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoder::ProgressiveScript;

    #[test]
    fn default_matches_new() {
        // `Default` must equal `new()`'s configuration: quality 75, 4:2:0, no restart, 1:1 aspect.
        let d = JpegEncoder::default();
        assert_eq!(d.quality, 75);
        assert_eq!(d.distance, None);
        assert_eq!(d.subsampling, ChromaSubsampling::Cs420);
        assert_eq!(d.restart_interval, 0);
        assert_eq!(d.density_unit, DensityUnit::AspectRatio);
        assert_eq!((d.x_density, d.y_density), (1, 1));
        assert_eq!(d.process, Process::Baseline);
        assert_eq!(Process::default(), Process::Baseline);
        assert_eq!(d.quant_tables, None);
        assert_eq!(d.rd, RdOptimization::None);
        assert_eq!(d.color_mode, JpegColorMode::Ycbcr);
        assert_eq!((&d.exif, &d.xmp, &d.icc), (&None, &None, &None));
    }

    /// The `(tables, per-component destinations)` `frame_quant` resolves, for whole-value asserts.
    fn plan(e: &JpegEncoder, frame: Frame) -> (Vec<[u16; 64]>, [u8; 3]) {
        let fq = e.frame_quant(frame).expect("valid configuration");
        (fq.dqt().iter().map(|&(_, t)| *t).collect(), fq.tq)
    }

    const YCBCR_444: Frame = Frame::Ycbcr { luma: (1, 1) };

    #[test]
    fn frame_quant_prefers_custom_tables_and_falls_back_to_the_scaled_annex_k() {
        // Distinct luma/chroma fixtures: a swapped arm changes the result.
        let luma = [7u16; 64];
        let chroma = [11u16; 64];
        let tables = QuantTables::new(luma, chroma).expect("nonzero fixtures");
        let custom = JpegEncoder::new().with_quant_tables(tables);
        assert_eq!(plan(&custom, YCBCR_444), (vec![luma, chroma], [0, 1, 1]));
        assert_eq!(plan(&custom, Frame::Gray), (vec![luma], [0, 0, 0]));
        // Quality or a distance set after (or before) custom tables must not perturb them.
        let later = custom.clone().with_quality(5).with_distance(3.0);
        assert_eq!(plan(&later, YCBCR_444), (vec![luma, chroma], [0, 1, 1]));
        // Default path: the frozen scaled Annex K tables, per configured quality.
        let default = JpegEncoder::new().with_quality(85);
        let (l, c) = (
            quant::scale(&quant::LUMINANCE, 85),
            quant::scale(&quant::CHROMINANCE, 85),
        );
        assert_eq!(plan(&default, YCBCR_444), (vec![l, c], [0, 1, 1]));
        assert_eq!(plan(&default, Frame::Xyb), (vec![l, c], [0, 0, 1]));
    }

    #[test]
    fn frame_quant_gives_a_caller_x_table_destination_2_in_xyb_only() {
        let x = [5u16; 64];
        let tables = QuantTables::annex_k().with_xyb_x(x).expect("nonzero");
        let e = JpegEncoder::new().with_quant_tables(tables);
        assert_eq!(
            plan(&e, Frame::Xyb),
            (vec![quant::LUMINANCE, quant::CHROMINANCE, x], [2, 0, 1])
        );
        assert_eq!(plan(&e, YCBCR_444).1, [0, 1, 1]);
        let fq = e.frame_quant(Frame::Xyb).unwrap();
        assert_eq!(
            (fq.table(0), fq.table(1), fq.table(2)),
            (&x, &quant::LUMINANCE, &quant::CHROMINANCE)
        );
    }

    #[test]
    fn frame_quant_takes_jpeglis_tables_for_a_distance() {
        // One table per component at destination c, built for the frame jpegli would see: 4:2:0
        // only when Y is 2×2.
        let e = JpegEncoder::new().with_distance(1.0);
        let jpegli = |frame| quant::jpegli_tables(1.0, frame);
        assert_eq!(
            plan(&e, Frame::Ycbcr { luma: (2, 2) }),
            (jpegli(JpegliFrame::Ycbcr { yuv420: true }), [0, 1, 2])
        );
        for luma in [(1, 1), (2, 1), (1, 2)] {
            assert_eq!(
                plan(&e, Frame::Ycbcr { luma }).0,
                jpegli(JpegliFrame::Ycbcr { yuv420: false })
            );
        }
        assert_eq!(plan(&e, Frame::Xyb), (jpegli(JpegliFrame::Xyb), [0, 1, 2]));
        assert_eq!(plan(&e, Frame::Gray).0, jpegli(JpegliFrame::Gray));
        // Quality set afterwards takes the control back; a distance set after quality keeps it.
        let back = e.clone().with_quality(85);
        assert_eq!(back.distance, None);
        assert_eq!(
            plan(&back, Frame::Gray).0,
            vec![quant::scale(&quant::LUMINANCE, 85)]
        );
        assert_eq!(
            JpegEncoder::new()
                .with_quality(85)
                .with_distance(1.0)
                .distance,
            Some(1.0)
        );
    }

    #[test]
    fn frame_quant_rejects_a_distance_that_is_not_finite_and_non_negative() {
        for bad in [-0.01, f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert!(
                JpegEncoder::new()
                    .with_distance(bad)
                    .frame_quant(Frame::Gray)
                    .is_err(),
                "{bad}"
            );
        }
        // Zero is the finest legal distance: every entry floors at 1.
        let zero = JpegEncoder::new().with_distance(0.0);
        assert_eq!(plan(&zero, YCBCR_444).0, vec![[1u16; 64]; 3]);
    }

    #[test]
    fn with_process_sets_the_process() {
        let progressive = Process::Progressive(ProgressiveScript::Mozjpeg);
        assert_eq!(
            JpegEncoder::new().with_process(progressive).process,
            progressive
        );
        assert_eq!(
            JpegEncoder::new()
                .with_process(progressive)
                .with_process(Process::Optimized)
                .process,
            Process::Optimized
        );
    }

    #[test]
    fn quant_tables_scale_with_quality() {
        // The encoder's per-component tables are the Annex K bases through the frozen IJG mapping —
        // never a placeholder. Anchor entries computed by hand: at q=75 (scale 50),
        // luminance[0]=16 → (16·50+50)/100 = 8; chrominance[0]=17 → (17·50+50)/100 = 9.
        let e = JpegEncoder::new().with_quality(75);
        let fq = e.frame_quant(YCBCR_444).unwrap();
        assert_eq!(fq.table(0), &quant::scale(&quant::LUMINANCE, 75));
        assert_eq!(fq.table(1), &quant::scale(&quant::CHROMINANCE, 75));
        assert_eq!(fq.table(0)[0], 8);
        assert_eq!(fq.table(2)[0], 9);
    }
}
