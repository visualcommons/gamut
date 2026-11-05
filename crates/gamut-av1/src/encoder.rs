//! Top-level: turn planar samples — 4:4:4, 4:2:2 or 4:2:0 colour, or a monochrome luma plane —
//! into the AV1 temporal unit for an AVIF still image.

use std::num::NonZeroUsize;

use gamut_color::cicp::{ColorRange, MatrixCoefficients};
use gamut_color::{BitDepth, ChromaSubsampling, Planar8, Planar16};
use gamut_core::{Dimensions, Error, Result};

use crate::filter::{FrameFilters, LrUnits};
use crate::geom::PlaneGeom;
use crate::headers::{self, Av1Color, Av1StillConfig, TileLayout, Tiling};
use crate::tile::{BlockVariation, FrameEncoder, Reconstruction, Search};

/// How much mode-decision search the lossy intra encoder runs: a preset that trades encode time
/// for search. Higher is slower, as everywhere in the workspace.
///
/// Every preset produces a conformant stream that a decoder reconstructs bit-exactly to the
/// encoder's own [`ReconImage`]; they differ only in which candidates the encoder *tries* before
/// it commits to one. Each step down the table prunes search the step above it runs, so a
/// lower-effort preset never considers a candidate a higher one skips. The default,
/// [`Effort::Balanced`], is what an [`Av1EncodeConfig`] uses unless told otherwise; the full
/// search, [`Effort::Thorough`], is chosen explicitly.
///
/// | Preset | Pruned relative to the one above | Search time saved |
/// |---|---|---|
/// | [`Thorough`](Self::Thorough) (level 3) | nothing: every directional mode at `AngleDeltaY ∈ [-3, 3]`, five filter-intra modes, CfL `\|alpha\|` 1..=16, all five `TX_SET_INTRA_2` types, rectangular partitions, 4×4 leaves, palette | — |
/// | [`Balanced`](Self::Balanced) (level 2) | directional modes are tried at `AngleDeltaY = 0` only | 48 of the 56 directional trials per ≥8×8 block |
/// | [`Fast`](Self::Fast) (level 1) | filter-intra is not tried; CfL tries `\|alpha\|` ∈ {1, 2, 4, 8, 16} only; the transform type is `DCT_DCT` or `ADST_ADST` | 5 filter-intra trials per block up to 32×32, 22 of 32 CfL trials per chroma plane, 3 of 4 extra transform-type trials per luma transform |
/// | [`Fastest`](Self::Fastest) (level 0) | only the non-directional luma modes (`DC`, `SMOOTH*`, `PAETH`); no CfL (chroma is `DC_PRED`); `DCT_DCT` only; no rectangular partitions; blocks are not split below 8×8 by choice; no palette | every directional trial, the CfL trials and palette scan, the last extra transform-type trial, the `PARTITION_HORZ`/`PARTITION_VERT` rate–distortion trials of every 16×16 and 32×32 region, and the 8×8 split trials with the 4×4 mode searches they bring |
///
/// **How candidates are weighed.** Every decision the presets prune — the partition of each 8×8 to
/// 64×64 region (whole, two rectangular halves, or four quadrants), the luma mode, filter-intra,
/// the transform type and the CfL alphas — and the transform depth, which every preset searches
/// in full, ranks its candidates by rate–distortion cost: each candidate is trialled, its squared
/// error measured on the reconstruction a decoder would produce, and weighed against its rate
/// priced from the adapting CDFs. The partition search is bottom-up: a region's four-quadrant alternative codes each
/// quadrant as the same search chooses for it, and every decision a trial reaches is kept and
/// reused rather than searched again, so the search costs time linear in its depth. The luma mode
/// and transform depth are chosen together in two passes: every pairing is trialled with
/// `DCT_DCT` alone, and the eight cheapest again with the transform-type search. The decisions
/// are made block by block, before the in-loop filters, so a slower preset is not guaranteed
/// smaller output on every image; measured on the fixed corpus in `tests/preset_rd.rs`,
/// [`Thorough`](Self::Thorough) needs fewer bits than every faster preset for the same PSNR.
/// Palette is still decided by a colour count, at every preset that tries it.
///
/// The presets prune *search*, so they move only lossy encodes. Lossless coding (`qindex = 0`)
/// has a single fixed path — `DC_PRED` and the 4×4 Walsh–Hadamard transform — and is identical at
/// every effort. The in-loop filters are decided the same way at every effort: once the blocks are
/// coded, the deblocking level, the CDEF strengths and, per 256×256 restoration unit, whether the
/// default Wiener filter is applied are each chosen by the distortion they leave against the
/// source, in the decoder's filter order, with "off" always a candidate — so the output is never
/// further from the source than the reconstruction before the filters. A frame in which a
/// restoration unit is restored has its tiles coded a second time to signal the units, replaying
/// every block decision of the first pass rather than searching again.
///
/// **What a preset does not shorten.** The loop-filter search — the deblocking level, then the
/// CDEF strengths — is the same search at every preset, so it costs the same whichever one is
/// chosen; a lower-effort preset saves mode-decision time only. It runs once per frame over the
/// whole reconstruction, on one thread whatever [`Av1EncodeConfig::threads`] says, and measured it
/// is about 6–8% of the tile-coding time (see `choose_filters` in `tile.rs`).
///
/// The discriminant is the preset's [`level`](Self::level), a permanent identifier for C callers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[repr(u8)]
#[non_exhaustive]
pub enum Effort {
    /// Level 0 — the minimum search: non-directional luma modes, `DC_PRED` chroma, `DCT_DCT`,
    /// square partitions no smaller than 8×8 by choice, and no palette.
    Fastest = 0,
    /// Level 1 — [`Balanced`](Self::Balanced) without filter-intra, with a thinned CfL alpha ladder
    /// and a transform-type search narrowed to two types.
    Fast = 1,
    /// Level 2 — directional modes without the fine angle refinement. The default.
    #[default]
    Balanced = 2,
    /// Level 3 — the full search.
    Thorough = 3,
}

impl Effort {
    /// Every preset, in ascending [`level`](Self::level) — fastest first.
    pub const ALL: &'static [Self] = &[Self::Fastest, Self::Fast, Self::Balanced, Self::Thorough];

    /// The preset's level, `0..=3`; higher searches more and is slower.
    #[must_use]
    pub const fn level(self) -> u8 {
        self as u8
    }

    /// The preset at `level`, or `None` if `level` is outside `0..=3`. The inverse of
    /// [`Effort::level`].
    #[must_use]
    pub const fn from_level(level: u8) -> Option<Self> {
        Some(match level {
            0 => Self::Fastest,
            1 => Self::Fast,
            2 => Self::Balanced,
            3 => Self::Thorough,
            _ => return None,
        })
    }
}

/// Everything one still encode is configured by: what it codes (the quantizer, the colour
/// signalling, superres) and how it is carried out (the search [`Effort`], the [`Tiling`] layout
/// and the thread cap).
///
/// Built with [`Av1EncodeConfig::new`] and the `const` `with_*` setters; the public fields read a
/// configuration back. [`new`](Self::new) codes at the given quantizer with the default
/// [`Av1Color`] (identity matrix, so GBR planes), [`Effort::Balanced`], the [`Tiling::default`]
/// layout, one thread and no superres.
///
/// ```
/// use std::num::NonZeroUsize;
/// use gamut_av1::{Av1EncodeConfig, Effort, Tiling};
///
/// let config = Av1EncodeConfig::new(80)
///     .with_effort(Effort::Fast)
///     .with_tiling(Tiling::new(2, 1))
///     .with_threads(NonZeroUsize::new(4).unwrap());
/// assert_eq!(config.threads.get(), 4);
/// assert_eq!(Av1EncodeConfig::lossless().qindex, 0);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct Av1EncodeConfig {
    /// The quantizer, `base_q_idx` (§5.9.12): `0` selects the lossless path (`DC_PRED` and the 4×4
    /// Walsh–Hadamard transform), and `1..=255` is lossy intra, coarser as it rises. It also
    /// selects the coefficient-CDF quantizer context (§8.3.2).
    pub qindex: u8,
    /// The colour signalling the sequence header declares (see [`Av1Color`]). The planes must
    /// already be in the layout its matrix describes — GBR for
    /// [`MatrixCoefficients::Identity`], `Y/Cb/Cr` for a luma–chroma matrix (see
    /// [`gamut_color::Planar8::from_rgb8_matrix`]). The coding tools are matrix-agnostic.
    ///
    /// A **monochrome** still is a
    /// [`ChromaSubsampling::Cs400`](gamut_color::ChromaSubsampling::Cs400) buffer together with
    /// [`Av1Color::monochrome`]: `mono_chrome = 1`, and no chroma is coded at all.
    pub color: Av1Color,
    /// How much mode-decision search to run (see [`Effort`]). It moves lossy encodes only.
    pub effort: Effort,
    /// Horizontal **superres** (§7.16), as `coded_denom` in `0..=7`, or `None` for none. The source
    /// is downscaled to `FrameWidth = (UpscaledWidth*8 + denom/2)/denom` (`denom = coded_denom +
    /// 9`), coded at that width, and the reconstruction is upscaled back to the display width.
    /// Lossy, 8-bit, non-monochrome encodes only.
    pub superres: Option<u8>,
    /// The tile layout to code the frame in (see [`Tiling`]). It changes the stream: more tiles
    /// are more parallelism, for the encoder and for a decoder, at some cost in size.
    pub tiling: Tiling,
    /// The most threads the encode may use; tiles are coded concurrently, at most one per thread,
    /// so a layout with one tile uses one thread whatever this says. It does **not** change the
    /// stream: for a given [`tiling`](Self::tiling) the output is identical at every thread count.
    ///
    /// Each thread past the first holds its own copy of the frame-wide encoder state (chiefly the
    /// lossy reconstruction), so peak memory grows with it. On `wasm32`, which has no threads to
    /// spawn, the tiles are always coded one after another.
    ///
    /// [`std::thread::available_parallelism`] returns this type, so it can be passed straight in.
    ///
    /// Only the tile coding is concurrent: the loop-filter search that follows it (the deblocking
    /// level, then the CDEF strengths, once per frame) runs on one thread whatever this says.
    pub threads: NonZeroUsize,
}

impl Av1EncodeConfig {
    /// A configuration coding at quantizer `qindex` (`0` is lossless), with every other knob at
    /// its default: [`Av1Color::default`], [`Effort::default`], [`Tiling::default`], one thread,
    /// and no superres.
    #[must_use]
    pub const fn new(qindex: u8) -> Self {
        Self {
            qindex,
            color: Av1Color::DEFAULT,
            effort: Effort::Balanced,
            superres: None,
            tiling: Tiling::DEFAULT,
            threads: NonZeroUsize::MIN,
        }
    }

    /// The lossless configuration: [`new(0)`](Self::new).
    #[must_use]
    pub const fn lossless() -> Self {
        Self::new(0)
    }

    /// This configuration coding at quantizer `qindex`.
    #[must_use]
    pub const fn with_qindex(mut self, qindex: u8) -> Self {
        self.qindex = qindex;
        self
    }

    /// This configuration signalling `color`.
    #[must_use]
    pub const fn with_color(mut self, color: Av1Color) -> Self {
        self.color = color;
        self
    }

    /// This configuration with the search run at `effort`.
    #[must_use]
    pub const fn with_effort(mut self, effort: Effort) -> Self {
        self.effort = effort;
        self
    }

    /// This configuration coding horizontal superres at `coded_denom` (`0..=7`; see
    /// [`superres`](Self::superres)).
    #[must_use]
    pub const fn with_superres(mut self, coded_denom: u8) -> Self {
        self.superres = Some(coded_denom);
        self
    }

    /// This configuration with the frame coded in `tiling`.
    #[must_use]
    pub const fn with_tiling(mut self, tiling: Tiling) -> Self {
        self.tiling = tiling;
        self
    }

    /// This configuration with the encode using at most `threads` threads.
    #[must_use]
    pub const fn with_threads(mut self, threads: NonZeroUsize) -> Self {
        self.threads = threads;
        self
    }
}

/// The encoded AV1 temporal unit (sequence-header OBU + frame OBU) for one still image, plus the
/// configuration values `gamut-avif` must mirror into the `av1C` and `colr` boxes.
#[derive(Debug, Clone)]
#[must_use]
#[non_exhaustive]
pub struct EncodedStill {
    /// The OBU byte stream to place in the AVIF `mdat` item (no temporal delimiter).
    pub obus: Vec<u8>,
    /// Sequence-header field values for `av1C` / `colr`.
    pub config: Av1StillConfig,
}

impl EncodedStill {
    /// Pairs an OBU stream with the sequence-header values it declares — for a stream another
    /// encoder produced, whose `config` the caller has read back from its sequence header (see
    /// [`Av1StillConfig::from_sequence_header`]).
    pub const fn new(obus: Vec<u8>, config: Av1StillConfig) -> Self {
        Self { obus, config }
    }
}

/// An encoded still together with the encoder's reconstruction of it — what
/// [`encode_still_with_recon`] and [`encode_still16_with_recon`] return.
#[derive(Debug, Clone)]
#[must_use]
#[non_exhaustive]
pub struct StillWithRecon {
    /// The encoded temporal unit, exactly what [`encode_still`] / [`encode_still16`] return.
    pub still: EncodedStill,
    /// The samples a conformant decoder reconstructs from [`still`](Self::still).
    pub recon: ReconImage,
}

/// The encoder's reconstructed image: exactly the samples a conformant decoder produces for
/// [`EncodedStill`]. Cropped to the display dimensions (the coded-grid padding is dropped, as on
/// decode). Used for the bit-exact decoder cross-check.
#[derive(Debug, Clone)]
#[must_use]
#[non_exhaustive]
pub struct ReconImage {
    /// Display width.
    pub width: u32,
    /// Display height.
    pub height: u32,
    /// Bits per sample; the planes carry values in `0..=(1 << bit_depth.bits()) - 1`.
    pub bit_depth: BitDepth,
    /// Chroma subsampling of the reconstructed planes, which sizes the two chroma planes.
    pub subsampling: ChromaSubsampling,
    /// Reconstructed planes (Y=G, U=B, V=R), row-major and widened to `u16`. Each plane is its
    /// own [`plane_dimensions`](Self::plane_dimensions) — `width * height` for luma, and the
    /// subsampled extent for chroma.
    pub planes: [Vec<u16>; 3],
}

impl ReconImage {
    /// The dimensions of plane `index` (`0` luma, `1`/`2` chroma), or `None` past the third plane:
    /// the display dimensions for luma, and [`ChromaSubsampling::chroma_dimensions`] for the two
    /// chroma planes — the answer [`Planar8::plane_dimensions`] gives for the source.
    #[must_use]
    pub fn plane_dimensions(&self, index: usize) -> Option<(u32, u32)> {
        match index {
            0 => Some((self.width, self.height)),
            1 | 2 => Some(self.subsampling.chroma_dimensions(self.width, self.height)),
            _ => None,
        }
    }
}

/// Encodes 8-bit planes as an AV1 intra keyframe, as `config` says.
///
/// The planes are in the layout [`Av1EncodeConfig::color`]'s matrix describes (GBR for the
/// identity matrix, `Y/Cb/Cr` otherwise); their chroma sampling — 4:4:4, 4:2:2, 4:2:0 or
/// monochrome — is coded as it is. [`encode_still_with_recon`] also returns the reconstruction.
///
/// # Errors
///
/// - [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) if the dimensions exceed AV1
///   level 6.0, for lossless (`qindex = 0`) planes that are neither 4:4:4 nor monochrome, and for
///   superres over a monochrome source.
/// - [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) for the identity matrix on
///   anything but 4:4:4 planes (§6.4.2 — monochrome takes [`Av1Color::monochrome`]), for studio
///   range under the §5.5.2 sRGB shortcut, and for a superres `coded_denom` above 7.
///
/// The effort, tiling and thread count never make an encode fail; a tiling the frame cannot take
/// is clamped to the nearest one it can (see [`Tiling`]).
pub fn encode_still(planes: &Planar8, config: &Av1EncodeConfig) -> Result<EncodedStill> {
    Ok(encode_still_with_recon(planes, config)?.still)
}

/// [`encode_still`], also returning the encoder's reconstruction: exactly the samples a
/// conformant decoder produces for the stream, which at `qindex = 0` is the source.
///
/// # Errors
///
/// As [`encode_still`].
pub fn encode_still_with_recon(
    planes: &Planar8,
    config: &Av1EncodeConfig,
) -> Result<StillWithRecon> {
    encode_with(planes, config, BlockVariation::Frame)
}

/// Encodes **10- or 12-bit** planes as an AV1 intra keyframe, as `config` says.
///
/// The depth comes from the buffer, which validated its own samples against it, and decides the
/// profile: 10-bit is profile 1 (or 0 monochrome, 2 at 4:2:2) with `high_bitdepth`, and 12-bit of
/// any layout is profile 2 with `twelve_bit` (§6.4.1). A [`Planar16`] that carries
/// [`BitDepth::Eight`] is accepted too and produces exactly the stream [`encode_still`] would.
///
/// # Errors
///
/// As [`encode_still`], plus [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) for
/// [`BitDepth::Sixteen`], which is not an AV1 sample depth (§6.4.1 defines 8, 10 and 12 only), and
/// for any [`superres`](Av1EncodeConfig::superres), whose source downscale is written for 8-bit
/// samples.
pub fn encode_still16(planes: &Planar16, config: &Av1EncodeConfig) -> Result<EncodedStill> {
    Ok(encode_still16_with_recon(planes, config)?.still)
}

/// [`encode_still16`], also returning the encoder's reconstruction.
///
/// # Errors
///
/// As [`encode_still16`].
pub fn encode_still16_with_recon(
    planes: &Planar16,
    config: &Av1EncodeConfig,
) -> Result<StillWithRecon> {
    encode16_with(planes, config, BlockVariation::Frame)
}

/// Validates the source layout shared by every entry point, returning whether the frame is
/// monochrome.
///
/// No dimension check lives here: a `Planar8` / `Planar16` is built from a non-zero
/// `gamut_core::Dimensions`, so an empty source cannot reach an entry point.
///
/// Two rules survive here, both about combinations the coding path cannot represent:
///
/// **Lossless is 4:4:4 or monochrome only.** §5.11.45 makes `is_cfl_allowed` under `Lossless` the
/// test `Subsampled_Size[MiSize][subX][subY] == BLOCK_4X4`, which at 4:2:0 is true for 4x8, 8x4 and
/// 8x8 as well as 4x4 — while the tile encoder tests `bw == 4`. Coding a subsampled lossless block
/// would emit `uv_mode` against the wrong CDF and desynchronise the decoder's symbol reader. #390
/// lifted the 4:4:4 restriction for the *lossy* path only, so the lossless one is refused rather
/// than silently mis-coded.
///
/// **The identity matrix requires 4:4:4.** §6.4.2: "If matrix_coefficients is equal to MC_IDENTITY,
/// it is a requirement of bitstream conformance that subsampling_x is equal to 0 and subsampling_y
/// is equal to 0." The identity matrix carries R'G'B' directly, and the three colour planes cannot
/// be sampled at different rates. This catches monochrome too — §5.5.2 infers `subsampling_x =
/// subsampling_y = 1` there, so identity is illegal — which matters because `Av1Color::default()`
/// *is* identity: a caller reaching here with monochrome planes and the default colour would
/// otherwise emit a non-conformant stream that libaom and dav1d are entitled to reject.
/// `Av1Color::monochrome()` is the fix, and the error says so.
fn check_layout(subsampling: ChromaSubsampling, colour: Av1Color, qindex: u8) -> Result<bool> {
    let monochrome = subsampling == ChromaSubsampling::Cs400;
    if qindex == 0 && !monochrome && subsampling != ChromaSubsampling::Cs444 {
        return Err(Error::unsupported(
            env!("CARGO_PKG_NAME"),
            "AV1: lossless coding requires 4:4:4 or monochrome planes",
        ));
    }
    if colour.matrix == MatrixCoefficients::Identity && subsampling != ChromaSubsampling::Cs444 {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            if monochrome {
                "AV1: a monochrome stream cannot signal the identity matrix (§6.4.2 allows \
                 MC_IDENTITY only at subsampling 0/0); use Av1Color::monochrome()"
            } else {
                "AV1: the identity matrix requires 4:4:4 chroma"
            },
        ));
    }
    Ok(monochrome)
}

/// The AV1 `seq_profile` for a still of this chroma format at this bit depth (Annex A.2 / §6.4.1).
///
/// The profile is a joint function of **both** axes, and neither one decides it alone:
///
/// | | 8/10-bit | 12-bit |
/// |---|---|---|
/// | 4:2:0 | 0 Main | 2 Professional |
/// | 4:4:4 | 1 High | 2 Professional |
/// | 4:2:2 | 2 Professional | 2 Professional |
/// | monochrome | 0 Main | 2 Professional |
///
/// Reading only the depth would put 12-bit at Professional but call 4:2:2 High; reading only the
/// layout would call 12-bit 4:2:0 Main. Both mis-declare a stream a decoder then rejects, so the
/// two are matched together here.
///
/// Monochrome is Main rather than High because profile 1 infers `mono_chrome = 0` (§5.5.2) and so
/// cannot carry a single-plane stream; Main is the only other profile that codes 8/10-bit. Note
/// also that no profile codes 4:4:4 *and* 4:2:0, which is why a 4:4:4 still cannot be read by a
/// Main-profile-only hardware decoder.
///
/// # Errors
///
/// [`ChromaSubsampling`] is `#[non_exhaustive]` and models layouts beyond AV1's (4:1:1, say). AV1
/// has no profile for one, so it is refused rather than mapped onto a nearby profile.
fn seq_profile_for(subsampling: ChromaSubsampling, bit_depth: BitDepth) -> Result<u8> {
    // The layout is resolved first and unconditionally, so an uncodable one is refused at 12 bits
    // too rather than swept into Professional by the depth rule below.
    let by_layout = match subsampling {
        ChromaSubsampling::Cs420 | ChromaSubsampling::Cs400 => 0,
        ChromaSubsampling::Cs444 => 1,
        ChromaSubsampling::Cs422 => 2,
        _ => {
            return Err(Error::unsupported(
                env!("CARGO_PKG_NAME"),
                "AV1: unsupported chroma subsampling",
            ));
        }
    };
    // 12-bit is Professional whatever the layout — it is the only profile that codes `twelve_bit`.
    Ok(if bit_depth == BitDepth::Twelve {
        2
    } else {
        by_layout
    })
}

/// The sequence-header field values for one still: the profile the depth and layout force, the
/// depth flags, and the CICP triple.
fn still_config(
    width: u32,
    height: u32,
    monochrome: bool,
    bit_depth: BitDepth,
    subsampling: ChromaSubsampling,
    colour: Av1Color,
) -> Result<Av1StillConfig> {
    if bit_depth == BitDepth::Sixteen {
        return Err(Error::unsupported(
            env!("CARGO_PKG_NAME"),
            "AV1: only 8-, 10- and 12-bit samples are coded (§6.4.1)",
        ));
    }
    // Each factor is 1 or 2, so `factor > 1` is its AV1 `subsampling_*` flag.
    let f = subsampling.factors();
    let (sub_x, sub_y) = (u8::from(f.width() > 1), u8::from(f.height() > 1));
    let config = Av1StillConfig {
        seq_profile: seq_profile_for(subsampling, bit_depth)?,
        seq_level_idx_0: headers::pick_level(width, height)?,
        seq_tier_0: 0,
        high_bitdepth: bit_depth != BitDepth::Eight,
        twelve_bit: bit_depth == BitDepth::Twelve,
        monochrome,
        // Inferred, not coded, under monochrome: §5.5.2 fixes both to 1.
        chroma_subsampling_x: sub_x,
        chroma_subsampling_y: sub_y,
        // `CSP_UNKNOWN`. There is no code point for the centre siting a symmetric box filter
        // produces — `CSP_VERTICAL` is horizontally co-located and `CSP_COLOCATED` is co-located on
        // both axes — so claiming either would misdescribe the samples. libavif reads UNKNOWN as
        // centred for 4:2:0 and libaom defaults to it, so this also matches the corpus.
        chroma_sample_position: 0,
        color_primaries: colour.primaries.code_point(),
        transfer_characteristics: colour.transfer.code_point(),
        matrix_coefficients: colour.matrix.code_point(),
        full_range: matches!(colour.range, ColorRange::Full),
    };
    // Under the §5.5.2 sRGB shortcut `color_range` is *inferred* as full and no bit is coded, so a
    // studio-range request there could not be signalled — the stream would silently claim full
    // range. Reject it rather than emit a header that disagrees with the samples.
    if config.is_srgb_shortcut() && !config.full_range {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "AV1: the sRGB color_config shortcut infers full range; studio range needs a non-identity matrix",
        ));
    }
    Ok(config)
}

/// Appends the coded tiles to a frame-header payload (§5.11.1): the frame header already emitted
/// the tile-group prefix, and each tile but the last is prefixed by its byte size minus one as a
/// little-endian `TileSizeBytes`-byte field.
fn append_tiles(frame_payload: &mut Vec<u8>, tile_bytes: &[Vec<u8>]) {
    for (i, tile) in tile_bytes.iter().enumerate() {
        if i + 1 < tile_bytes.len() {
            let sz = (tile.len() - 1) as u32;
            frame_payload.extend_from_slice(&sz.to_le_bytes()[..headers::TILE_SIZE_BYTES]);
        }
        frame_payload.extend_from_slice(tile);
    }
}

/// The high-bit-depth encode behind [`encode_still16_with_recon`], with the block variation as
/// [`encode_with`] takes it.
fn encode16_with(
    planes: &Planar16,
    config: &Av1EncodeConfig,
    variation: BlockVariation,
) -> Result<StillWithRecon> {
    let &Av1EncodeConfig {
        qindex,
        color,
        effort,
        superres,
        tiling,
        threads,
    } = config;
    if superres.is_some() {
        return Err(Error::unsupported(
            env!("CARGO_PKG_NAME"),
            "AV1: superres is coded for 8-bit sources only",
        ));
    }
    let (width, height) = (planes.width(), planes.height());
    let monochrome = check_layout(planes.subsampling(), color, qindex)?;
    let config = still_config(
        width,
        height,
        monochrome,
        planes.bit_depth(),
        planes.subsampling(),
        color,
    )?;

    let seq_payload = headers::sequence_header_payload(&config, width, height, qindex > 0, false);
    let layout = TileLayout::resolve(
        tiling,
        width as usize,
        height as usize,
        config.seq_level_idx_0,
        None,
    );
    let encoder = || {
        FrameEncoder::new16(planes, qindex)
            .with_search(Search::for_effort(effort))
            .with_block_variation(variation)
    };
    let frame = if qindex == 0 {
        // Lossless: the reconstruction *is* the source. Each plane crops at its own extent, so a
        // monochrome buffer's empty chroma planes stay empty.
        let extents = planes.plane_extents();
        CodedFrame::lossless(encoder().encode(&layout, threads), |i| {
            let (pw, ph) = extents[i];
            crop(planes.planes()[i], pw, pw, ph)
        })
    } else {
        CodedFrame::lossy(
            encoder,
            &layout,
            threads,
            planes.planes()[0],
            None,
            (width as usize, height as usize),
        )
    };
    let mut frame_payload = headers::frame_header_payload(
        &layout,
        qindex,
        None,
        monochrome,
        frame.filters,
        frame.lr.any(),
        variation,
    );
    append_tiles(&mut frame_payload, &frame.tile_bytes);
    let recon_planes = frame.planes;

    Ok(StillWithRecon {
        still: EncodedStill {
            obus: headers::assemble_temporal_unit(&seq_payload, &frame_payload),
            config,
        },
        recon: ReconImage {
            width,
            height,
            bit_depth: planes.bit_depth(),
            subsampling: planes.subsampling(),
            planes: recon_planes,
        },
    })
}

/// The 8-bit encode behind every [`Planar8`] entry point. `variation` is the position-only block
/// variation the crate's oracle tests exercise; every public entry point passes
/// [`BlockVariation::Frame`], so no public encode varies a block's quantizer or loop-filter level
/// by position (#1011).
fn encode_with(
    planes: &Planar8,
    config: &Av1EncodeConfig,
    variation: BlockVariation,
) -> Result<StillWithRecon> {
    let &Av1EncodeConfig {
        qindex,
        color,
        effort,
        superres: coded_denom,
        tiling,
        threads,
    } = config;
    let width = planes.width();
    let height = planes.height();
    if let Some(cd) = coded_denom {
        // `coded_denom` is a 3-bit field (§5.9.8, `SUPERRES_DENOM_BITS`).
        if cd > 7 {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "AV1: superres coded_denom must be in 0..=7",
            ));
        }
        // Superres over a monochrome source is refused rather than half-supported: the downscale
        // below is written for three luma-sized planes and relabels its result 4:4:4, so a
        // monochrome buffer would read an empty chroma slice at luma dimensions and then hand
        // `FrameEncoder` a plane count disagreeing with the `monochrome` the frame header was
        // given. Checked *before* `check_layout`, whose matrix rule would otherwise fire first
        // for a superres request left at the default identity colour and report a reason that
        // is not the one that applies.
        if planes.subsampling() == ChromaSubsampling::Cs400 {
            return Err(Error::unsupported(
                env!("CARGO_PKG_NAME"),
                "AV1: superres over a monochrome source is not implemented",
            ));
        }
    }
    let monochrome = check_layout(planes.subsampling(), color, qindex)?;

    // Superres downscales the source horizontally to the coded (Frame) width; the reconstruction is
    // upscaled back to `width` at the end. `coded_src` is what the block encoder actually codes.
    let (coded_w, coded_src) = match coded_denom {
        Some(cd) => {
            let denom = cd as usize + 9;
            let dw = crate::filter::superres_downscaled_width(width as usize, denom);
            let dp: [Vec<u8>; 3] = planes.planes().map(|plane| {
                crate::filter::superres_downscale_plane(plane, width as usize, dw, height as usize)
            });
            let coded_dims = Dimensions::new(dw as u32, height)?;
            (dw as u32, Planar8::from_planes(coded_dims, dp)?)
        }
        None => (width, planes.clone()),
    };

    let config = still_config(
        width,
        height,
        monochrome,
        BitDepth::Eight,
        planes.subsampling(),
        color,
    )?;

    let seq_payload =
        headers::sequence_header_payload(&config, width, height, qindex > 0, coded_denom.is_some());
    let layout = TileLayout::resolve(
        tiling,
        coded_w as usize,
        height as usize,
        config.seq_level_idx_0,
        coded_denom,
    );
    let encoder = || {
        FrameEncoder::new(&coded_src, qindex)
            .with_search(Search::for_effort(effort))
            .with_block_variation(variation)
    };
    let frame = if qindex == 0 {
        // Lossless: the reconstruction equals the 8-bit source; widen it into the u16 recon buffer.
        // Each plane crops at its **own** extent — the source carries no coded padding, so the
        // stride is the plane width. A monochrome buffer's chroma planes are empty and stay empty;
        // cropping them at the luma extent would index past the end of a zero-length slice.
        let extents = planes.plane_extents();
        CodedFrame::lossless(encoder().encode(&layout, threads), |i| {
            let (pw, ph) = extents[i];
            crop(planes.planes()[i], pw, pw, ph)
                .into_iter()
                .map(u16::from)
                .collect()
        })
    } else {
        // With superres the coded grid is the downscaled width; the reconstruction is upscaled
        // back to the display `width`, and loop restoration is decided on the upscaled frame
        // against the full-width source.
        CodedFrame::lossy(
            encoder,
            &layout,
            threads,
            planes.planes()[0],
            coded_denom.map(|cd| (usize::from(cd) + 9, coded_w as usize)),
            (width as usize, height as usize),
        )
    };
    let mut frame_payload = headers::frame_header_payload(
        &layout,
        qindex,
        coded_denom,
        monochrome,
        frame.filters,
        frame.lr.any(),
        variation,
    );
    append_tiles(&mut frame_payload, &frame.tile_bytes);

    let still = EncodedStill {
        obus: headers::assemble_temporal_unit(&seq_payload, &frame_payload),
        config,
    };
    let recon_planes = frame.planes;
    let recon = ReconImage {
        width,
        height,
        bit_depth: BitDepth::from_bits(frame.bit_depth).ok_or_else(|| {
            Error::unsupported(
                env!("CARGO_PKG_NAME"),
                "AV1: unsupported reconstruction bit depth",
            )
        })?,
        subsampling: planes.subsampling(),
        planes: recon_planes,
    };
    Ok(StillWithRecon { still, recon })
}

/// A coded frame: its tiles' bytes, the in-loop filters its header signals, and the samples a
/// decoder reconstructs from it, each plane at its own visible extent.
struct CodedFrame {
    tile_bytes: Vec<Vec<u8>>,
    filters: FrameFilters,
    /// The luma restoration units; none is restored on the lossless path.
    lr: LrUnits,
    planes: [Vec<u16>; 3],
    bit_depth: u32,
}

impl CodedFrame {
    /// A lossless frame `coded` by [`FrameEncoder::encode`]: no in-loop filter, and the
    /// reconstruction is the source, plane `i` of which `source(i)` crops.
    fn lossless(
        coded: (Vec<Vec<u8>>, Reconstruction),
        source: impl FnMut(usize) -> Vec<u16>,
    ) -> Self {
        Self {
            tile_bytes: coded.0,
            filters: FrameFilters::default(),
            lr: LrUnits {
                cols: 0,
                rows: 0,
                wiener: Vec::new(),
            },
            planes: std::array::from_fn(source),
            bit_depth: coded.1.bit_depth,
        }
    }

    /// Codes a lossy frame with the encoders `encoder` makes, and decides its luma restoration
    /// units against `source_luma`, the display-sized luma.
    ///
    /// The first pass codes the tiles with no restoration and filters them (§7.4: deblocking,
    /// CDEF, both chosen against the source by [`FrameEncoder::encode`]; then, with `superres` —
    /// `(SuperresDenom, FrameWidth)` — the upscale to the display `width × height`). Each
    /// restoration unit is then decided by [`crate::filter::choose_lr_units`] on the result. Only
    /// if some unit is restored are the tiles coded again, to signal the units
    /// ([`FrameEncoder::recoding`]); the reconstruction is the first pass's either way.
    fn lossy<'a, S: Copy + Into<i64>>(
        encoder: impl Fn() -> FrameEncoder<'a>,
        layout: &TileLayout,
        threads: NonZeroUsize,
        source_luma: &[S],
        superres: Option<(usize, usize)>,
        (width, height): (usize, usize),
    ) -> Self {
        let (mut tile_bytes, first) = encoder().encode(layout, threads);
        let bit_depth = first.bit_depth;
        // The planes loop restoration reads, and their stride: §7.4 upscales both the CDEF output
        // and the deblocked luma (which only multi-stripe frames read) before restoring. The
        // upscaled widths stay luma-derived: §7.16 scales them per plane by `Round2(w, subX)`,
        // which only matters once a subsampled source can reach superres — `encode_with` rejects
        // one today.
        let (mut planes, deblocked, stride) = match superres {
            Some((_, frame_w)) => {
                let up = |plane: &[u16], g: PlaneGeom| {
                    crate::filter::superres_upscale_plane(
                        plane, g.coded_w, frame_w, width, height, bit_depth,
                    )
                };
                (
                    std::array::from_fn(|i| up(&first.planes[i], first.geom[i])),
                    up(&first.deblocked_luma, first.geom[0]),
                    width,
                )
            }
            None => (
                first.planes.clone(),
                first.deblocked_luma.clone(),
                first.geom[0].coded_w,
            ),
        };
        let mut restored = planes[0].clone();
        crate::filter::loop_restore_wiener_luma(
            &mut restored,
            &deblocked,
            stride,
            width,
            height,
            crate::filter::WIENER_DEFAULT,
            crate::filter::WIENER_DEFAULT,
            bit_depth,
        );
        let lr = crate::filter::choose_lr_units(
            source_luma,
            &planes[0],
            &restored,
            stride,
            width,
            height,
        );
        if lr.any() {
            crate::filter::apply_lr_units(&lr, &mut planes[0], &restored, stride, width, height);
            let denom = superres.map(|(denom, _)| denom);
            tile_bytes = encoder()
                .recoding(&first, lr.clone(), denom)
                .encode(layout, threads)
                .0;
        }
        // Without superres each plane is cropped from its own coded stride to its own visible
        // extent; the upscaled planes are already display-sized.
        if superres.is_none() {
            planes = std::array::from_fn(|i| {
                let g = first.geom[i];
                crop(&planes[i], g.w as u32, g.coded_w as u32, g.h as u32)
            });
        }
        Self {
            tile_bytes,
            filters: first.filters,
            lr,
            planes,
            bit_depth,
        }
    }
}

/// Crops a `src_stride`-wide plane to `width × height`, row-major.
fn crop<T: Copy>(plane: &[T], width: u32, src_stride: u32, height: u32) -> Vec<T> {
    let (w, sw, h) = (width as usize, src_stride as usize, height as usize);
    let mut out = Vec::with_capacity(w * h);
    for y in 0..h {
        out.extend_from_slice(&plane[y * sw..y * sw + w]);
    }
    out
}

#[cfg(test)]
mod tests {
    use gamut_color::{Planar8, RgbToYcbcr};
    use gamut_core::{Dimensions, ImageRef, Rgb8};

    use super::*;

    /// Non-zero test dimensions.
    fn dims(w: u32, h: u32) -> Dimensions {
        Dimensions::new(w, h).unwrap()
    }

    #[test]
    fn recon_image_reports_each_plane_at_its_own_size() {
        // 4:4:4: every plane is the display size.
        let planes =
            Planar8::from_planes(dims(4, 2), [vec![0; 8], vec![0; 8], vec![0; 8]]).unwrap();
        let recon = encode_still_with_recon(&planes, &Av1EncodeConfig::new(40))
            .expect("encodes")
            .recon;
        assert_eq!(recon.subsampling, gamut_color::ChromaSubsampling::Cs444);
        for i in 0..3 {
            assert_eq!(recon.plane_dimensions(i), Some((4, 2)), "plane {i}");
            assert_eq!(recon.planes[i].len(), 4 * 2, "plane {i}");
        }

        // The accessor must follow `subsampling`, not just echo the display size — checked by
        // constructing the subsampled case directly, since the encoder cannot produce one yet.
        // Odd dimensions make the ceiling division observable.
        let sub = ReconImage {
            width: 5,
            height: 3,
            bit_depth: BitDepth::Eight,
            subsampling: gamut_color::ChromaSubsampling::Cs420,
            planes: [vec![0; 15], vec![0; 6], vec![0; 6]],
        };
        assert_eq!(sub.plane_dimensions(0), Some((5, 3)));
        assert_eq!(sub.plane_dimensions(1), Some((3, 2)));
        assert_eq!(sub.plane_dimensions(2), Some((3, 2)));
    }

    #[test]
    fn recon_image_plane_dimensions_has_no_fourth_plane() {
        let planes =
            Planar8::from_planes(dims(4, 2), [vec![0; 8], vec![0; 8], vec![0; 8]]).unwrap();
        let recon = encode_still_with_recon(&planes, &Av1EncodeConfig::new(40))
            .expect("encodes")
            .recon;
        assert_eq!(recon.plane_dimensions(3), None);
        assert_eq!(recon.plane_dimensions(usize::MAX), None);
    }

    #[test]
    fn a_vertical_edge_at_four_two_two_does_not_reach_an_invalid_chroma_block() {
        // A strong vertical edge is what would make the partition search weigh PARTITION_VERT,
        // producing a taller-than-wide half whose 4:2:2 chroma residual is `BLOCK_INVALID` (§5.11.38). The
        // partition search must never emit one, so this encodes rather than hitting the residual
        // loop's unreachable-by-contract arm.
        let mut rgb = vec![0u8; 32 * 32 * 3];
        for y in 0..32 {
            for x in 16..32 {
                let i = (y * 32 + x) * 3;
                rgb[i] = 255;
                rgb[i + 1] = 255;
                rgb[i + 2] = 255;
            }
        }
        let img = ImageRef::<Rgb8>::new(&rgb, Dimensions::new(32, 32).unwrap()).unwrap();
        let m = RgbToYcbcr::new(
            gamut_color::cicp::MatrixCoefficients::Bt709,
            ColorRange::Full,
            BitDepth::Eight,
        )
        .unwrap();
        let colour = Av1Color {
            matrix: gamut_color::cicp::MatrixCoefficients::Bt709,
            ..Av1Color::default()
        };
        let planes =
            Planar8::from_rgb8_matrix_subsampled(img, m, ChromaSubsampling::Cs422).unwrap();
        let still = encode_still(&planes, &Av1EncodeConfig::new(40).with_color(colour))
            .expect("4:2:2 encodes");
        assert_eq!(still.config.seq_profile, 2);
    }

    #[test]
    fn lossless_requires_four_four_four_or_monochrome() {
        // §5.11.45 makes `is_cfl_allowed` under Lossless `Subsampled_Size[..] == BLOCK_4X4`, true
        // at 4:2:0 for 4x8/8x4/8x8 as well as 4x4, while the tile encoder tests `bw == 4`. Coding
        // a subsampled lossless block would pick the wrong `uv_mode` CDF and desync the decoder,
        // so the whole combination is refused. #390 lifts 4:4:4 for the *lossy* path only.
        let rgb = vec![0u8; 8 * 8 * 3];
        let img = ImageRef::<Rgb8>::new(&rgb, Dimensions::new(8, 8).unwrap()).unwrap();
        let m = RgbToYcbcr::new(
            gamut_color::cicp::MatrixCoefficients::Bt709,
            ColorRange::Full,
            BitDepth::Eight,
        )
        .unwrap();
        let planes =
            Planar8::from_rgb8_matrix_subsampled(img, m, ChromaSubsampling::Cs420).unwrap();
        let colour = Av1Color {
            matrix: gamut_color::cicp::MatrixCoefficients::Bt709,
            ..Av1Color::default()
        };
        let err = encode_still(&planes, &Av1EncodeConfig::new(0).with_color(colour))
            .expect_err("lossless 4:2:0 is refused, not mis-coded");
        assert_eq!(
            err.static_message(),
            Some("AV1: lossless coding requires 4:4:4 or monochrome planes")
        );
        // The same planes encode on the lossy path, so the rejection is keyed on the quantizer.
        assert!(encode_still(&planes, &Av1EncodeConfig::new(40).with_color(colour)).is_ok());
    }

    #[test]
    fn the_identity_matrix_requires_four_four_four() {
        // §6.4.2 makes this a conformance requirement, and §5.5.2 enforces it structurally: the
        // sRGB shortcut infers 4:4:4 whatever the profile says, so an identity 4:2:0 stream would
        // describe itself two ways at once. Asserted on the diagnostic, since `encode_with` has
        // several other rejections.
        let rgb = vec![0u8; 8 * 8 * 3];
        let img = ImageRef::<Rgb8>::new(&rgb, Dimensions::new(8, 8).unwrap()).unwrap();
        let m = RgbToYcbcr::new(
            gamut_color::cicp::MatrixCoefficients::Bt709,
            ColorRange::Full,
            BitDepth::Eight,
        )
        .unwrap();
        let planes =
            Planar8::from_rgb8_matrix_subsampled(img, m, ChromaSubsampling::Cs420).unwrap();

        let err = encode_still(&planes, &Av1EncodeConfig::new(40))
            .expect_err("identity + 4:2:0 is not conformant");
        assert_eq!(
            err.static_message(),
            Some("AV1: the identity matrix requires 4:4:4 chroma")
        );
        // The same planes encode through a luma-chroma matrix, so the rejection is keyed on the
        // matrix and not on the subsampling alone.
        assert!(
            encode_still(
                &planes,
                &Av1EncodeConfig::new(40).with_color(
                    Av1Color::DEFAULT.with_matrix(gamut_color::cicp::MatrixCoefficients::Bt709)
                ),
            )
            .is_ok()
        );
    }

    #[test]
    fn the_sequence_profile_follows_the_chroma_format() {
        // Annex A.2 at 8-bit: Main (0) is 4:2:0, High (1) is 4:4:4, Professional (2) adds 4:2:2.
        // This is the whole reason a 4:4:4 still cannot be read by a Main-profile-only hardware
        // decoder. The depth axis is covered by `the_sequence_profile_is_joint_over_layout_and_depth`;
        // this one drives the real encode, so it also pins the coded subsampling pair.
        let rgb = vec![128u8; 16 * 16 * 3];
        let img = ImageRef::<Rgb8>::new(&rgb, Dimensions::new(16, 16).unwrap()).unwrap();
        let m = RgbToYcbcr::new(
            gamut_color::cicp::MatrixCoefficients::Bt709,
            ColorRange::Full,
            BitDepth::Eight,
        )
        .unwrap();
        let colour = Av1Color {
            matrix: gamut_color::cicp::MatrixCoefficients::Bt709,
            ..Av1Color::default()
        };
        for (ss, want_profile, want_shifts) in [
            (ChromaSubsampling::Cs444, 1u8, (0u8, 0u8)),
            (ChromaSubsampling::Cs420, 0, (1, 1)),
            (ChromaSubsampling::Cs422, 2, (1, 0)),
        ] {
            let planes = Planar8::from_rgb8_matrix_subsampled(img, m, ss).unwrap();
            let still = encode_still(&planes, &Av1EncodeConfig::new(40).with_color(colour))
                .expect("encodes");
            assert_eq!(still.config.seq_profile, want_profile, "{ss:?}");
            assert_eq!(
                (
                    still.config.chroma_subsampling_x,
                    still.config.chroma_subsampling_y
                ),
                want_shifts,
                "{ss:?}"
            );
        }
    }

    #[test]
    fn the_sequence_profile_is_joint_over_layout_and_depth() {
        // Annex A.2 / §6.4.1. Each axis alone gives a wrong answer for two cells of this table:
        // reading only the depth calls 8-bit 4:2:2 High, reading only the layout calls 12-bit
        // 4:2:0 Main. Both mis-declare a stream a conformant decoder then rejects, so the table is
        // pinned whole — including the two cells neither the chroma slice nor the depth slice ever
        // coded on its own (12-bit 4:2:0 and 10-bit 4:2:2).
        use ChromaSubsampling::{Cs400, Cs420, Cs422, Cs444};
        for (ss, depth, want) in [
            (Cs420, BitDepth::Eight, 0u8),
            (Cs420, BitDepth::Ten, 0),
            (Cs420, BitDepth::Twelve, 2), // layout alone would say 0
            (Cs444, BitDepth::Eight, 1),
            (Cs444, BitDepth::Ten, 1),
            (Cs444, BitDepth::Twelve, 2), // layout alone would say 1
            (Cs422, BitDepth::Eight, 2),  // depth alone would say 1
            (Cs422, BitDepth::Ten, 2),    // depth alone would say 1
            (Cs422, BitDepth::Twelve, 2),
            // Profile 1 infers `mono_chrome = 0` (§5.5.2), so a single-plane stream cannot be
            // High; Main is the only other profile that codes 8/10-bit.
            (Cs400, BitDepth::Eight, 0),
            (Cs400, BitDepth::Ten, 0),
            (Cs400, BitDepth::Twelve, 2),
        ] {
            assert_eq!(
                seq_profile_for(ss, depth).expect("a coded layout"),
                want,
                "{ss:?} at {depth:?}"
            );
        }
        // 4:4:0 has no AV1 signalling (`subsampling_y = 1` needs `subsampling_x = 1`, §6.4.2),
        // so it is refused at every depth — 12-bit included, which the depth rule would
        // otherwise sweep into Professional.
        for depth in [BitDepth::Eight, BitDepth::Twelve] {
            assert_eq!(
                seq_profile_for(ChromaSubsampling::Cs440, depth)
                    .expect_err("4:4:0 is not an AV1 layout")
                    .kind(),
                gamut_core::ErrorKind::Unsupported
            );
        }
    }

    /// Builds identity planes from an RGB generator.
    fn planes(w: u32, h: u32, f: impl Fn(u32, u32) -> [u8; 3]) -> Planar8 {
        let mut rgb = vec![0u8; (w * h * 3) as usize];
        for y in 0..h {
            for x in 0..w {
                let i = ((y * w + x) * 3) as usize;
                let p = f(x, y);
                rgb[i..i + 3].copy_from_slice(&p);
            }
        }
        Planar8::from_rgb8_identity(&rgb, dims(w, h)).unwrap()
    }

    /// `p` coded at `qindex` with the full search and no superres, as `encode_with` codes it at
    /// [`Effort::Thorough`].
    fn coded_frame(p: &Planar8, qindex: u8) -> CodedFrame {
        let (w, h) = (p.width() as usize, p.height() as usize);
        let config = still_config(
            p.width(),
            p.height(),
            false,
            BitDepth::Eight,
            p.subsampling(),
            Av1Color::default(),
        )
        .unwrap();
        let layout = TileLayout::resolve(Tiling::default(), w, h, config.seq_level_idx_0, None);
        CodedFrame::lossy(
            || FrameEncoder::new(p, qindex),
            &layout,
            NonZeroUsize::MIN,
            p.planes()[0],
            None,
            (w, h),
        )
    }

    /// A smooth, low-frequency field, whose quantization artefacts the in-loop filters remove.
    fn smooth_field(x: u32, y: u32) -> [u8; 3] {
        let (fx, fy) = (f64::from(x), f64::from(y));
        let g = 128.0 + 60.0 * (fx / 23.0).sin() * (fy / 31.0).cos();
        [(g * 0.9) as u8, g as u8, (255.0 - g) as u8]
    }

    #[test]
    fn restoration_units_are_decided_one_by_one() {
        // 640 columns make three 256-wide units, the last absorbing the remainder. On this field
        // the Wiener filter brings the middle unit closer to the source and the other two further
        // from it, so only the middle one is restored — and the frame, having one, is coded twice.
        // `recon.rs` decodes the same frame with both oracles.
        let frame = coded_frame(&planes(640, 64, smooth_field), 110);
        assert_eq!(
            frame.lr,
            LrUnits {
                cols: 3,
                rows: 1,
                wiener: vec![false, true, false],
            }
        );
    }

    #[test]
    fn restoration_units_of_a_grid_are_decided_row_by_row() {
        // Two units by two (512 / 256 each way): the smooth field above, light noise over a ramp
        // below. Both top units are restored and neither bottom one is, so a unit read at any row
        // but its own codes a `use_wiener` the reconstruction did not apply. That difference is
        // what lets `recon.rs`'s raster-order grid test, which decodes the same frame with both
        // oracles, tell `row * cols + col` from any other index; this pins it.
        let frame = coded_frame(
            &planes(512, 512, |x, y| {
                if y < 256 {
                    smooth_field(x, y)
                } else {
                    let v = (100 + x / 4 + (x * 7 + y * 13) % 9) as u8;
                    [v, v, v]
                }
            }),
            110,
        );
        assert_eq!(
            frame.lr,
            LrUnits {
                cols: 2,
                rows: 2,
                wiener: vec![true, true, false, false],
            }
        );
    }

    #[test]
    fn no_unit_is_restored_where_restoration_does_not_help() {
        // Light noise over a ramp at a fine quantizer: the reconstruction is already close, and
        // the low-pass Wiener filter only blurs the noise away from the source.
        let noisy = planes(64, 40, |x, y| {
            let v = (100 + x / 2 + (x * 7 + y * 13) % 9) as u8;
            [v, v, v]
        });
        let frame = coded_frame(&noisy, 16);
        assert_eq!(frame.lr.wiener, [false]);
    }

    /// Parses the low-overhead OBU stream into `(obu_type, payload_len)` pairs, asserting it tiles
    /// the buffer exactly.
    fn parse_obus(d: &[u8]) -> Vec<(u8, usize)> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < d.len() {
            let hb = d[i];
            i += 1;
            let obu_type = (hb >> 3) & 0xf;
            let has_size = (hb >> 1) & 1;
            assert_eq!(has_size, 1, "M0 always sets obu_has_size_field");
            // leb128 size
            let mut size = 0usize;
            let mut shift = 0;
            loop {
                let b = d[i];
                i += 1;
                size |= usize::from(b & 0x7f) << shift;
                shift += 7;
                if b & 0x80 == 0 {
                    break;
                }
            }
            out.push((obu_type, size));
            i += size;
        }
        assert_eq!(i, d.len(), "OBUs must tile the temporal unit exactly");
        out
    }

    /// Returns the payload bytes of the first OBU (the sequence header) — its header byte and
    /// LEB128 size prefix stripped.
    fn seq_header_payload(d: &[u8]) -> &[u8] {
        let mut i = 1; // skip the OBU header byte
        let (mut size, mut shift) = (0usize, 0);
        loop {
            let b = d[i];
            i += 1;
            size |= usize::from(b & 0x7f) << shift;
            shift += 7;
            if b & 0x80 == 0 {
                break;
            }
        }
        &d[i..i + size]
    }

    #[test]
    fn encoded_bitstream_is_stable() {
        // The encoder's structural choices — block partition (choose_partition), palette
        // (decide_palette/palette_cache/signal_palette_colors), skip (block_is_skippable), tx
        // depth and type (select_luma_mode, select_tx_type), loop-restoration signalling
        // (write_lr) — change the OBU bitstream but reconstruct bit-exactly, so the libaom/dav1d
        // recon oracle in tests/recon.rs cannot see them. This snapshot pins an FNV-1a checksum of the
        // OBU stream for a spread of content patterns, sizes and quantizers; any change to a
        // (deterministic) encoder decision moves a checksum. The recon oracle proves these same streams
        // decode correctly, so the pair certifies "deterministic AND correct", not just "deterministic".
        // It encodes through a bare `Av1EncodeConfig::new`, so it pins the default preset
        // (`Effort::Balanced`);
        // `each_effort_preset_has_a_stable_bitstream` pins every preset by name.
        fn fnv1a(bytes: &[u8]) -> u64 {
            bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| {
                (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
            })
        }
        // Patterns exercise distinct decisions: gradient (detailed residual + range split), flat (every
        // block skippable), palette (few luma colours + flat chroma), checker (max residual / golomb
        // tails), bands (vertical structure → rectangular partitions).
        let pat = |id: u8, x: u32, y: u32| -> [u8; 3] {
            match id {
                0 => [(x * 7 + y) as u8, (x ^ (y * 3)) as u8, (x + y * 5) as u8],
                1 => [200, 100, 50],
                2 => [(((x / 4 + y / 4) % 5) * 50) as u8, 128, 128],
                3 => {
                    let v = if (x + y).is_multiple_of(2) { 0 } else { 255 };
                    [v, 255 - v, v]
                }
                _ => [((x / 8) * 40) as u8, 128, 128],
            }
        };
        let cases: &[(u8, u32, u32, u8, u64)] = &[
            (0, 40, 24, 0, 0x5728_aabc_5720_858a),
            (1, 64, 64, 1, 0xe582_b966_f13a_a1f3),
            (2, 32, 32, 40, 0x2cee_d06e_116f_84f9),
            (3, 48, 48, 16, 0x24fa_0cb0_41a7_5c54),
            (4, 64, 48, 90, 0xcb16_c36e_28da_2df3),
            (0, 100, 80, 8, 0xb6c9_186d_65e4_65f1),
            // One tile: two would leave the second 2 samples wide (Annex A.3, CroppedTileWidth).
            (0, 130, 70, 120, 0xe098_b58f_9ae1_2cda),
            (2, 64, 64, 200, 0x5a5c_1a4f_0f5e_8fac),
            (3, 96, 96, 60, 0x36f5_8d2c_3684_0002),
            (4, 128, 64, 30, 0xe749_0937_59b1_43c0),
        ];
        // Every case is encoded before any is compared, so one run reports every pin that moved.
        let got: Vec<(u8, u32, u32, u8, u64)> = cases
            .iter()
            .map(|&(id, w, h, q, _)| {
                let p = planes(w, h, |x, y| pat(id, x, y));
                let obus = encode_still(&p, &Av1EncodeConfig::new(q)).unwrap().obus;
                (id, w, h, q, fnv1a(&obus))
            })
            .collect();
        assert_eq!(got, cases, "a pattern's bitstream changed");
    }

    #[test]
    fn the_default_effort_is_the_balanced_search_a_new_config_runs() {
        // `Av1EncodeConfig::new` runs the middle preset, which `encoded_bitstream_is_stable` pins.
        // It reaches it through `Effort::Balanced`, so the default must be that preset, and a
        // config that names it explicitly must be the same encode byte for byte, at both depths.
        assert_eq!(Effort::default(), Effort::Balanced);
        assert_eq!(Av1EncodeConfig::new(40).effort, Effort::default());
        let p = planes(64, 48, |x, y| {
            [(x * 7 + y) as u8, (x ^ (y * 3)) as u8, (x + y * 5) as u8]
        });
        let plain = encode_still(&p, &Av1EncodeConfig::new(40)).unwrap().obus;
        let named = Av1EncodeConfig::new(40).with_effort(Effort::Balanced);
        assert_eq!(plain, encode_still(&p, &named).unwrap().obus);

        let deep = Planar16::from_planes(
            dims(64, 48),
            BitDepth::Ten,
            std::array::from_fn(|i| p.planes()[i].iter().map(|&v| u16::from(v) << 2).collect()),
        )
        .unwrap();
        let plain = encode_still16(&deep, &Av1EncodeConfig::new(40))
            .unwrap()
            .obus;
        assert_eq!(plain, encode_still16(&deep, &named).unwrap().obus);
    }

    #[test]
    fn a_new_config_holds_the_documented_defaults_and_each_setter_sets_its_own_field() {
        // `Av1EncodeConfig::new` must be the `Balanced` preset on the legacy two-column layout on
        // one thread, with the identity colour and no superres, so a bare config reproduces the
        // pinned streams above.
        let config = Av1EncodeConfig::new(7);
        assert_eq!(config.qindex, 7);
        assert_eq!(config.color, Av1Color::default());
        assert_eq!(config.effort, Effort::Balanced);
        assert_eq!(config.superres, None);
        assert_eq!(config.tiling, Tiling::new(1, 0));
        assert_eq!(config.threads.get(), 1);
        assert_eq!(Av1EncodeConfig::lossless(), Av1EncodeConfig::new(0));
        let built = Av1EncodeConfig::new(7)
            .with_qindex(9)
            .with_color(Av1Color::monochrome())
            .with_effort(Effort::Fast)
            .with_superres(3)
            .with_tiling(Tiling::new(2, 3))
            .with_threads(NonZeroUsize::new(5).unwrap());
        assert_eq!(
            (
                built.qindex,
                built.color,
                built.effort,
                built.superres,
                built.tiling,
                built.threads.get()
            ),
            (
                9,
                Av1Color::monochrome(),
                Effort::Fast,
                Some(3),
                Tiling::new(2, 3),
                5
            )
        );
    }

    #[test]
    fn effort_levels_rise_with_the_search_and_round_trip() {
        // Higher is slower: the level order is the search order, fastest first, and `ALL` lists
        // the presets in it. The levels are permanent identifiers, so each is pinned.
        assert_eq!(
            Effort::ALL,
            [
                Effort::Fastest,
                Effort::Fast,
                Effort::Balanced,
                Effort::Thorough
            ]
        );
        for (level, &effort) in Effort::ALL.iter().enumerate() {
            assert_eq!(usize::from(effort.level()), level, "{effort:?}");
            assert_eq!(Effort::from_level(effort.level()), Some(effort));
        }
        assert_eq!(Effort::from_level(4), None);
        assert_eq!(Effort::from_level(u8::MAX), None);
    }

    #[test]
    fn the_stream_and_reconstruction_do_not_depend_on_the_thread_count() {
        // Tiles are coded independently, so the threads that code them must not show in the
        // output: every thread count, including more threads than tiles, gives the one-thread
        // stream and reconstruction. The reconstruction is what proves the merge — the tiles a
        // spawned worker codes reach the in-loop filters only through `absorb_tile` — and the
        // content mixes texture, palette-like steps and stripes so each merged per-MI map
        // (partition sizes, transform sizes, skip) varies across the frame. The delta-LF map is
        // all zero here; `the_block_variation_exercise_survives_the_restoration_pass_and_threaded_tiles`
        // merges one that varies.
        let mix = |x: u32, y: u32| -> [u8; 3] {
            match ((x / 37) % 3, (y / 29) % 2) {
                (0, _) => [(x * 7 + y) as u8, (x ^ (y * 3)) as u8, (x + y * 5) as u8],
                (1, 0) => [(((x / 4 + y / 4) % 4) * 60) as u8, 128, 128],
                (1, _) => {
                    let v = if ((x * 2 + y) / 5).is_multiple_of(2) {
                        40
                    } else {
                        200
                    };
                    [v, v / 2, 255 - v]
                }
                _ => [(x * 2 + y) as u8, 90, (220 - y) as u8],
            }
        };
        let rgb = planes(200, 150, mix);
        let ycc = |ss: ChromaSubsampling| {
            let raw: Vec<u8> = (0..150u32)
                .flat_map(|y| (0..200u32).flat_map(move |x| mix(x, y)))
                .collect();
            let img = ImageRef::<Rgb8>::new(&raw, Dimensions::new(200, 150).unwrap()).unwrap();
            let m = RgbToYcbcr::new(MatrixCoefficients::Bt709, ColorRange::Full, BitDepth::Eight)
                .unwrap();
            Planar8::from_rgb8_matrix_subsampled(img, m, ss).unwrap()
        };
        let bt709 = Av1Color {
            matrix: MatrixCoefficients::Bt709,
            ..Av1Color::default()
        };
        let mono = Planar8::from_planes_subsampled(
            dims(200, 150),
            ChromaSubsampling::Cs400,
            [rgb.planes()[0].to_vec(), Vec::new(), Vec::new()],
        )
        .unwrap();
        let cases: [(&str, Planar8, u8, Av1Color, Tiling); 5] = [
            (
                "4:4:4 q40",
                rgb.clone(),
                40,
                Av1Color::default(),
                Tiling::new(2, 1),
            ),
            (
                "4:4:4 lossless",
                rgb,
                0,
                Av1Color::default(),
                Tiling::new(1, 2),
            ),
            (
                "4:2:0 q90",
                ycc(ChromaSubsampling::Cs420),
                90,
                bt709,
                Tiling::new(2, 2),
            ),
            (
                "4:2:2 q20",
                ycc(ChromaSubsampling::Cs422),
                20,
                bt709,
                Tiling::new(1, 1),
            ),
            (
                "mono q60",
                mono,
                60,
                Av1Color::monochrome(),
                Tiling::new(2, 1),
            ),
        ];
        let threads = |n: usize| NonZeroUsize::new(n).unwrap();
        for (name, p, q, colour, tiling) in &cases {
            let at = |n: usize| {
                let config = Av1EncodeConfig::new(*q)
                    .with_color(*colour)
                    .with_tiling(*tiling)
                    .with_threads(threads(n));
                let coded = encode_still_with_recon(p, &config).unwrap();
                (coded.still, coded.recon)
            };
            let (one, one_recon) = at(1);
            for n in [2, 3, 64] {
                let (many, many_recon) = at(n);
                assert_eq!(many.obus, one.obus, "{name}: stream at {n} threads");
                assert_eq!(
                    many_recon.planes, one_recon.planes,
                    "{name}: reconstruction at {n} threads"
                );
            }
        }

        // The high-bit-depth entry point threads the same options through.
        let deep = Planar16::from_planes(
            dims(200, 150),
            BitDepth::Ten,
            std::array::from_fn(|i| {
                planes(200, 150, mix).planes()[i]
                    .iter()
                    .map(|&v| u16::from(v) << 2)
                    .collect()
            }),
        )
        .unwrap();
        let at = |n: usize| {
            let config = Av1EncodeConfig::new(40)
                .with_tiling(Tiling::new(2, 1))
                .with_threads(threads(n));
            let coded = encode_still16_with_recon(&deep, &config).unwrap();
            (coded.still, coded.recon)
        };
        let (one, one_recon) = at(1);
        let (four, four_recon) = at(4);
        assert_eq!(four.obus, one.obus, "10-bit stream at 4 threads");
        assert_eq!(four_recon.planes, one_recon.planes, "10-bit reconstruction");
    }

    #[test]
    fn the_tiling_reaches_the_stream() {
        // A layout request is not advisory: on a frame that can take it, a different layout is a
        // different stream (the oracle tests in `tests/recon.rs` prove each one decodes).
        let p = planes(200, 150, |x, y| {
            [(x * 3) as u8, (y * 5) as u8, (x ^ y) as u8]
        });
        let at = |tiling: Tiling| {
            let config = Av1EncodeConfig::new(40).with_tiling(tiling);
            encode_still(&p, &config).unwrap().obus
        };
        let default = at(Tiling::default());
        assert_eq!(
            default,
            encode_still(&p, &Av1EncodeConfig::new(40)).unwrap().obus
        );
        assert_ne!(at(Tiling::SINGLE), default);
        assert_ne!(at(Tiling::new(1, 1)), default);
        assert_ne!(at(Tiling::new(2, 0)), default);
    }

    #[test]
    fn lossless_coding_is_the_same_at_every_speed() {
        // The presets prune the lossy mode decisions only; lossless has one fixed path (DC_PRED,
        // the 4×4 WHT, PARTITION_NONE), so there is nothing for a faster preset to skip.
        let p = planes(72, 40, |x, y| [(x * 3) as u8, (y * 5) as u8, (x ^ y) as u8]);
        let reference = encode_still(&p, &Av1EncodeConfig::new(0)).unwrap().obus;
        for &effort in Effort::ALL {
            let still = encode_still(&p, &Av1EncodeConfig::new(0).with_effort(effort)).unwrap();
            assert_eq!(still.obus, reference, "{effort:?}");
        }
    }

    #[test]
    fn each_effort_preset_has_a_stable_bitstream() {
        // The effort analogue of `encoded_bitstream_is_stable`: which candidates each preset tries
        // is a structural choice the recon oracle cannot see (every choice decodes bit-exactly),
        // so the coded stream is pinned per preset. The content mixes texture, a few flat-chroma
        // luma colours, diagonal stripes and a chroma-tracking ramp, so every pruned tool has a
        // block that would pick it under the full search: every preset's pin differs from the one
        // before it, so each step's pruning is visible on this content.
        fn fnv1a(bytes: &[u8]) -> u64 {
            bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| {
                (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
            })
        }
        let mix = planes(96, 64, |x, y| match (x < 48, y < 32) {
            (true, true) => [(x * 7 + y) as u8, (x ^ (y * 3)) as u8, (x + y * 5) as u8],
            (false, true) => [(((x / 4 + y / 4) % 4) * 60) as u8, 128, 128],
            (true, false) => {
                let v = if ((x * 2 + y) / 5).is_multiple_of(2) {
                    40
                } else {
                    200
                };
                [v, v / 2, 255 - v]
            }
            (false, false) => {
                let v = (x * 2 + y) as u8;
                [v, v / 2 + 40, 220 - v / 3]
            }
        });
        let cases: &[(Effort, u8, u64)] = &[
            (Effort::Thorough, 40, 0xe067_d32e_9491_3c8f),
            (Effort::Balanced, 40, 0x96e5_c275_b493_ce51),
            (Effort::Fast, 40, 0x71c6_ad52_69ee_3895),
            (Effort::Fastest, 40, 0xb0c1_e20a_e32a_6fb1),
            (Effort::Balanced, 120, 0x6fa7_90ff_0c36_9732),
            (Effort::Fast, 120, 0x9a5d_85fe_2cb8_c80a),
            (Effort::Fastest, 120, 0xd9e4_e7cb_d1a7_2609),
        ];
        // Every case is encoded before any is compared, so one run reports every pin that moved.
        let got: Vec<(Effort, u8, u64)> = cases
            .iter()
            .map(|&(effort, q, _)| {
                let still =
                    encode_still(&mix, &Av1EncodeConfig::new(q).with_effort(effort)).unwrap();
                (effort, q, fnv1a(&still.obus))
            })
            .collect();
        assert_eq!(got, cases, "a preset's bitstream changed");
    }

    #[test]
    fn cdf_adaptation_shrinks_the_coded_stream() {
        // `disable_cdf_update = 0` is a pure coding win: the reconstruction is unchanged (recon.rs
        // proves that against libaom and dav1d), only the symbol stream gets shorter. The bounds
        // below are the byte counts this encoder produced with *static* CDFs, measured on the
        // parent commit. Each case must now come in strictly under its bound, so silently losing
        // adaptation — the checksums in `encoded_bitstream_is_stable` would move, but so would any
        // other encoder change — is caught for what it is: a size regression.
        //
        // The margin is large (~20–35%) because a still image gives every context a whole frame to
        // converge, and the §9.4 defaults are tuned for video.
        let gradient = planes(64, 64, |x, y| {
            [(x * 3) as u8, (y * 3) as u8, ((x + y) * 2) as u8]
        });
        let noise = planes(128, 96, |x, y| {
            let v = (x
                .wrapping_mul(2_654_435_761)
                .wrapping_add(y.wrapping_mul(40503))
                >> 13) as u8;
            [v, v.wrapping_mul(3), v.wrapping_add(77)]
        });
        let smooth = planes(160, 120, |x, y| {
            let a = ((x as f32 / 9.0).sin() * 60.0 + (y as f32 / 7.0).cos() * 50.0 + 128.0) as u8;
            [a, a.wrapping_add(20), a.wrapping_sub(30)]
        });
        for (name, p, q, static_bytes) in [
            ("gradient", &gradient, 60u8, 495usize),
            ("noise", &noise, 120, 17_736),
            ("smooth", &smooth, 90, 8173),
        ] {
            let n = encode_still(p, &Av1EncodeConfig::new(q))
                .unwrap()
                .obus
                .len();
            assert!(
                n < static_bytes,
                "{name} q{q}: {n} bytes, not below the static-CDF baseline of {static_bytes}"
            );
        }
    }

    #[test]
    fn obu_stream_is_seq_then_frame() {
        let p = planes(40, 24, |x, y| [(x * 3) as u8, (y * 5) as u8, (x + y) as u8]);
        let e = encode_still(&p, &Av1EncodeConfig::lossless()).unwrap();
        assert_eq!(e.config.seq_profile, 1);
        assert_eq!(e.config.matrix_coefficients, 0);
        assert_eq!(e.config.chroma_subsampling_x, 0);
        assert!(e.config.full_range);
        let obus = parse_obus(&e.obus);
        assert_eq!(obus.len(), 2);
        assert_eq!(obus[0].0, 1, "first OBU is the sequence header");
        assert_eq!(obus[1].0, 6, "second OBU is the frame");
    }

    #[test]
    fn deterministic_output() {
        let p = planes(33, 17, |x, y| [(x ^ y) as u8, (x * 7) as u8, (y * 3) as u8]);
        assert_eq!(
            encode_still(&p, &Av1EncodeConfig::lossless()).unwrap().obus,
            encode_still(&p, &Av1EncodeConfig::lossless()).unwrap().obus
        );
    }

    #[test]
    fn solid_color_uses_all_zero_path() {
        // A flat image makes every residual zero, exercising the txb_skip = all_zero branch.
        let e = encode_still(
            &planes(64, 64, |_, _| [200, 100, 50]),
            &Av1EncodeConfig::lossless(),
        )
        .unwrap();
        assert!(!e.obus.is_empty());
    }

    #[test]
    fn high_contrast_exercises_golomb() {
        // A ±max checkerboard produces large WHT coefficients (golomb tails).
        let e = encode_still(
            &planes(48, 48, |x, y| {
                let v = if (x + y) % 2 == 0 { 0 } else { 255 };
                [v, 255 - v, v]
            }),
            &Av1EncodeConfig::lossless(),
        )
        .unwrap();
        assert!(!e.obus.is_empty());
    }

    #[test]
    fn assorted_sizes_encode() {
        // Edge sizes (padding + forced partition splits) and multi-superblock frames.
        for (w, h) in [
            (1, 1),
            (7, 3),
            (64, 1),
            (1, 64),
            (100, 80),
            (130, 70),
            (256, 256),
        ] {
            let p = planes(w, h, |x, y| [(x * 11) as u8, (y * 13) as u8, (x * y) as u8]);
            let e = encode_still(&p, &Av1EncodeConfig::lossless()).unwrap();
            assert_eq!(parse_obus(&e.obus).len(), 2);
        }
    }

    #[test]
    fn lossless_sequence_header_clears_lossy_flags() {
        // `encode_with` passes `qindex > 0` as the sequence header's `lossy` flag, which gates
        // enable_filter_intra/cdef/restoration. At qindex 0 (lossless) those bits must be 0; at a
        // lossy qindex they are 1. Same image and no superres, so the sequence-header payload differs
        // *only* by that flag — distinguishing `qindex > 0` from an always-true `qindex >= 0`.
        let p = planes(40, 24, |x, y| [(x * 3) as u8, (y * 5) as u8, (x + y) as u8]);
        let lossless = encode_still(&p, &Av1EncodeConfig::new(0)).unwrap();
        let lossy = encode_still(&p, &Av1EncodeConfig::new(8)).unwrap();
        assert_ne!(
            seq_header_payload(&lossless.obus),
            seq_header_payload(&lossy.obus),
            "lossless and lossy sequence headers must differ in the lossy flags",
        );
    }

    #[test]
    fn lossy_encode_structure_and_determinism() {
        // Exercises the lossy path (DCT + quant + reconstruction) across sizes/qindex without a
        // decoder: OBU framing, deterministic output, and the reconstruction dimensions. The
        // qindex set spans every coefficient-CDF quantizer context (≤20, ≤60, ≤120, else).
        for &q in &[1u8, 8, 20, 40, 90, 200, 255] {
            for (w, h) in [(1, 1), (8, 8), (17, 13), (40, 24), (130, 70)] {
                let p = planes(w, h, |x, y| {
                    [(x * 7 + y) as u8, (x ^ (y * 3)) as u8, (x + y * 5) as u8]
                });
                let StillWithRecon { still, recon } =
                    encode_still_with_recon(&p, &Av1EncodeConfig::new(q)).unwrap();
                assert_eq!(parse_obus(&still.obus).len(), 2, "{w}x{h} q{q}");
                assert_eq!(recon.width, w);
                assert_eq!(recon.height, h);
                for plane in &recon.planes {
                    assert_eq!(plane.len(), (w * h) as usize);
                }
                // Determinism.
                let again = encode_still(&p, &Av1EncodeConfig::new(q)).unwrap();
                assert_eq!(still.obus, again.obus, "{w}x{h} q{q} not deterministic");
            }
        }
    }

    #[test]
    fn lossy_flat_image_reconstructs_near_source() {
        // A solid color quantizes every AC residual to zero; the DC-prediction reconstruction
        // should land within a couple of levels of the source (light quantization).
        let recon = encode_still_with_recon(
            &planes(48, 40, |_, _| [200, 100, 50]),
            &Av1EncodeConfig::new(12),
        )
        .unwrap()
        .recon;
        // Planes are identity-mapped Y=G, U=B, V=R, so the source plane DCs are [100, 50, 200].
        for (plane, &want) in recon.planes.iter().zip(&[100u8, 50, 200]) {
            for &got in plane {
                assert!(
                    i32::from(got).abs_diff(i32::from(want)) <= 3,
                    "flat recon {got} far from {want}"
                );
            }
        }
    }

    #[test]
    fn default_colour_config_matches_the_identity_one() {
        // A config naming `Av1Color::default()` must be the *same* encode as a bare
        // `Av1EncodeConfig::new` — otherwise the default path's pinned checksums would not cover it.
        let p = planes(40, 24, |x, y| [(x * 3) as u8, (y * 5) as u8, (x + y) as u8]);
        for &q in &[0u8, 40] {
            let plain = encode_still(&p, &Av1EncodeConfig::new(q)).unwrap();
            let with =
                encode_still(&p, &Av1EncodeConfig::new(q).with_color(Av1Color::default())).unwrap();
            assert_eq!(plain.obus, with.obus, "q{q}");
            assert_eq!(
                plain.config.matrix_coefficients,
                with.config.matrix_coefficients
            );
        }
    }

    #[test]
    fn colour_is_mirrored_into_the_config_and_the_sequence_header() {
        use gamut_color::cicp::{ColorPrimaries, MatrixCoefficients, TransferCharacteristics};

        let p = planes(40, 24, |x, y| [(x * 3) as u8, (y * 5) as u8, (x + y) as u8]);
        let colour = Av1Color {
            primaries: ColorPrimaries::Bt2020,
            transfer: TransferCharacteristics::Srgb,
            matrix: MatrixCoefficients::Bt2020Ncl,
            range: ColorRange::Limited,
        };
        let still = encode_still(&p, &Av1EncodeConfig::new(40).with_color(colour)).unwrap();
        // `gamut-avif` mirrors these straight into `av1C`/`colr`, so each field must survive.
        assert_eq!(still.config.color_primaries, 9);
        assert_eq!(still.config.transfer_characteristics, 13);
        assert_eq!(still.config.matrix_coefficients, 9);
        assert!(!still.config.full_range);
        // Still profile 1 / 4:4:4 — this change is the colour transform, not the plane geometry.
        assert_eq!(still.config.seq_profile, 1);
        assert_eq!(
            (
                still.config.chroma_subsampling_x,
                still.config.chroma_subsampling_y
            ),
            (0, 0)
        );
        // The CICP code points really reach the bitstream.
        let identity = encode_still(&p, &Av1EncodeConfig::new(40)).unwrap();
        assert_ne!(
            seq_header_payload(&still.obus),
            seq_header_payload(&identity.obus)
        );
    }

    #[test]
    fn colour_range_bit_is_coded_only_outside_the_srgb_shortcut() {
        use gamut_color::cicp::{ColorPrimaries, MatrixCoefficients, TransferCharacteristics};

        // AV1 §5.5.2: with BT.709 primaries + sRGB transfer + identity matrix, `color_range` is
        // inferred and *no* bit is coded; any other combination codes it explicitly. Flipping only
        // the range must therefore change the payload for a real matrix…
        let p = planes(40, 24, |x, y| [(x * 3) as u8, (y * 5) as u8, (x + y) as u8]);
        let base = Av1Color {
            primaries: ColorPrimaries::Bt709,
            transfer: TransferCharacteristics::Srgb,
            matrix: MatrixCoefficients::Bt709,
            range: ColorRange::Full,
        };
        let full = encode_still(&p, &Av1EncodeConfig::new(40).with_color(base)).unwrap();
        let limited = encode_still(
            &p,
            &Av1EncodeConfig::new(40).with_color(base.with_range(ColorRange::Limited)),
        )
        .unwrap();
        assert_ne!(
            seq_header_payload(&full.obus),
            seq_header_payload(&limited.obus),
            "color_range must be coded when the sRGB shortcut does not apply"
        );

        // …and under the shortcut a studio-range request is refused rather than silently signalled
        // as full range.
        let shortcut = Av1Color {
            range: ColorRange::Limited,
            ..Av1Color::default()
        };
        match encode_still(&p, &Av1EncodeConfig::new(40).with_color(shortcut)) {
            Err(error) if error.kind() == gamut_core::ErrorKind::InvalidInput => {}
            other => panic!("expected an InvalidInput for shortcut + studio range, got {other:?}"),
        }
    }

    #[test]
    fn lossy_high_contrast_encodes() {
        // A ±max checkerboard makes large coefficients (golomb tails) in the lossy path too.
        let StillWithRecon { still, recon } = encode_still_with_recon(
            &planes(48, 48, |x, y| {
                let v = if (x + y) % 2 == 0 { 0 } else { 255 };
                [v, 255 - v, v]
            }),
            &Av1EncodeConfig::new(16),
        )
        .unwrap();
        assert_eq!(parse_obus(&still.obus).len(), 2);
        assert_eq!(recon.planes[0].len(), 48 * 48);
    }

    /// A busy texture whose blocks stay non-skip, so every block codes its segment id.
    fn texture(x: u32, y: u32) -> [u8; 3] {
        let r = (x.wrapping_mul(5).wrapping_add(y.wrapping_mul(3)) % 256) as u8;
        let g = ((x.wrapping_add(y).wrapping_mul(2)) % 256) as u8;
        let b = (40 + ((x.wrapping_mul(7) ^ y) % 160)) as u8;
        [r, g, b]
    }

    /// `p` encoded as `encode_still_with_recon` would under `config`, with the position-only
    /// block variation switched on.
    fn exercised(p: &Planar8, config: &Av1EncodeConfig) -> StillWithRecon {
        encode_with(p, config, BlockVariation::SyntaxExercise).unwrap()
    }

    /// Decodes `still` with libaom (the AV1 reference decoder) and dav1d, and asserts each
    /// reproduces the encoder's reconstruction of every coded plane byte for byte.
    fn assert_both_oracles_reconstruct(
        StillWithRecon { still, recon }: &StillWithRecon,
        case: &str,
    ) {
        // A standalone Section-5 stream leads with a temporal-delimiter OBU, which AVIF omits.
        let mut stream = vec![0x12u8, 0x00];
        stream.extend_from_slice(&still.obus);
        let coded = recon.subsampling.num_planes();
        let size = (recon.width, recon.height);
        let aom = aom_oracle::decode_av1(&stream).unwrap_or_else(|e| panic!("libaom, {case}: {e}"));
        assert_eq!((aom.width, aom.height), size, "libaom size, {case}");
        assert!(
            aom.planes[..coded] == recon.planes[..coded],
            "libaom differs from the reconstruction, {case}"
        );
        let dav1d =
            dav1d_oracle::decode_obu(&stream).unwrap_or_else(|e| panic!("dav1d, {case}: {e}"));
        assert_eq!((dav1d.width, dav1d.height), size, "dav1d size, {case}");
        assert!(
            dav1d.planes[..coded] == recon.planes[..coded],
            "dav1d differs from the reconstruction, {case}"
        );
    }

    /// Whether `obus` signals `(segmentation_enabled, delta_q_present, delta_lf_present)`, read
    /// with the sequence and frame header parsers directly, so a layout `Av1Decoder::inspect`
    /// refuses (4:2:0, high bit depth) can still be checked.
    #[cfg(feature = "decode")]
    fn block_variation_flags(obus: &[u8]) -> (bool, bool, bool) {
        let units: Vec<_> = crate::decode::obu::ObuIter::new(obus)
            .collect::<Result<Vec<_>>>()
            .unwrap();
        let seq = crate::decode::obu::SequenceHeader::parse(units[0].payload).unwrap();
        let mut r = gamut_bitstream::BitReader::new(units[1].payload);
        let fh = crate::decode::header::FrameHeader::parse(&mut r, &seq).unwrap();
        (
            fh.segmentation.enabled,
            fh.delta_q_present,
            fh.delta_lf_present,
        )
    }

    /// #1011: no default lossy encode varies its quantizer or loop-filter level by block position.
    /// Kills `BlockVariation::exercised` → `true` (and `frame_header_payload`'s gate on it), which
    /// would put the position-only segmentation, `delta_q` and `delta_lf` back on every encode,
    /// and pins `BlockVariation::Frame` at each public entry point: the plain one, the options one
    /// `gamut-avif`'s lossy encoder calls (BT.709 4:2:0, two tiles, two threads), superres and the
    /// high-bit-depth one.
    #[cfg(feature = "decode")]
    #[test]
    fn block_variation_exercised_is_off_in_every_default_lossy_encode() {
        let none = (false, false, false);
        let p = planes(160, 96, texture);
        for q in [1u8, 20, 120, 255] {
            let still = encode_still(&p, &Av1EncodeConfig::new(q)).unwrap();
            assert_eq!(block_variation_flags(&still.obus), none, "plain q{q}");
        }
        let rgb: Vec<u8> = (0..160u32 * 96)
            .flat_map(|i| texture(i % 160, i / 160))
            .collect();
        let img = ImageRef::<Rgb8>::new(&rgb, Dimensions::new(160, 96).unwrap()).unwrap();
        let m =
            RgbToYcbcr::new(MatrixCoefficients::Bt709, ColorRange::Full, BitDepth::Eight).unwrap();
        let yuv = Planar8::from_rgb8_matrix_subsampled(img, m, ChromaSubsampling::Cs420).unwrap();
        let colour = Av1Color {
            matrix: MatrixCoefficients::Bt709,
            ..Av1Color::default()
        };
        let config = Av1EncodeConfig::new(90)
            .with_color(colour)
            .with_threads(NonZeroUsize::new(2).unwrap());
        let still = encode_still(&yuv, &config).unwrap();
        assert_eq!(block_variation_flags(&still.obus), none, "options, 4:2:0");
        let still = encode_still(&p, &Av1EncodeConfig::new(90).with_superres(4)).unwrap();
        assert_eq!(block_variation_flags(&still.obus), none, "superres");
        let deep = Planar16::from_planes(
            dims(64, 48),
            BitDepth::Ten,
            std::array::from_fn(|c| {
                (0..64u32 * 48)
                    .map(|i| u16::from(texture(i % 64, i / 64)[c]) * 4)
                    .collect()
            }),
        )
        .unwrap();
        let still = encode_still16(&deep, &Av1EncodeConfig::new(90)).unwrap();
        assert_eq!(block_variation_flags(&still.obus), none, "10-bit");
    }

    #[test]
    fn the_block_variation_exercise_has_a_stable_bitstream() {
        // Pin: which segment, `delta_q` and `delta_lf` each block or superblock is given is a free
        // choice — any value reconstructs bit-exactly, so neither oracle sees it — and since #1011
        // no default encode codes them for `encoded_bitstream_is_stable` to pin. So the exercised
        // stream is pinned here: a change to `signal_segment_id`'s `(r + c) % 3`,
        // `signal_delta_q`'s cycle or `signal_delta_lf`'s alternation moves a checksum.
        fn fnv1a(bytes: &[u8]) -> u64 {
            bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| {
                (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
            })
        }
        let cases: &[(u32, u32, u8, u64)] = &[
            (64, 48, 40, 0xb9e0_eb56_8470_1461),
            (160, 96, 90, 0x5c64_97fe_666c_5078),
            (200, 136, 200, 0xad8f_9751_87b9_e2f4),
        ];
        let got: Vec<(u32, u32, u8, u64)> = cases
            .iter()
            .map(|&(w, h, q, _)| {
                let p = planes(w, h, texture);
                let coded = exercised(&p, &Av1EncodeConfig::new(q));
                (w, h, q, fnv1a(&coded.still.obus))
            })
            .collect();
        assert_eq!(got, cases, "an exercised bitstream changed");
    }

    #[test]
    fn the_block_variation_exercise_codes_delta_q_both_decoders_follow() {
        // Oracle: each superblock's first block signals a per-SB delta_q, so CurrentQIndex (and
        // the dc/ac dequantizer step) varies across superblocks while the coefficient-CDF qctx
        // stays at its frame value (init_coeff_cdfs derives it from base_q_idx, §8.3.2).
        // Multi-superblock sizes (> 64 px) exercise several deltas; base_q_idx values sit on the
        // qctx boundaries (20/60/120) so a ±delta pushes CurrentQIndex across the boundary without
        // changing qctx — the case that would desync if qctx tracked CurrentQIndex.
        let texture = |x: u32, y: u32| {
            let r = (x.wrapping_mul(3).wrapping_add(y) % 256) as u8;
            let g = ((x + y.wrapping_mul(2)) % 256) as u8;
            let b = (128 + ((x ^ y) % 64)) as u8;
            [r, g, b]
        };
        for q in [20u8, 21, 60, 61, 120, 121, 200] {
            for (w, h) in [(64u32, 64u32), (96, 96), (160, 96), (100, 70)] {
                let p = planes(w, h, texture);
                let coded = exercised(&p, &Av1EncodeConfig::new(q));
                assert_both_oracles_reconstruct(&coded, &format!("{w}x{h} q{q}"));
            }
        }
    }

    #[cfg(feature = "decode")]
    #[test]
    fn the_block_variation_exercise_codes_delta_lf_both_decoders_follow() {
        // Oracle: each superblock's first block signals a per-SB delta_lf, so the deblocking level
        // varies across superblocks (loop_filter_level + accumulated DeltaLF, §7.14.4). Structured
        // content gives the deblock real work; multi-superblock sizes exercise several levels.
        let edged = |x: u32, y: u32| {
            let bar = if (x / 3).is_multiple_of(2) { 70 } else { 190 } as u8;
            let ramp = ((x.wrapping_add(y).wrapping_mul(11)) % 256) as u8;
            let band = if (y / 4).is_multiple_of(2) { 60 } else { 200 } as u8;
            [bar, ramp, band]
        };
        let mut levels = Vec::new();
        for q in [24u8, 64, 128, 220] {
            for (w, h) in [(96u32, 96u32), (160, 96), (128, 128)] {
                let p = planes(w, h, edged);
                let coded = exercised(&p, &Av1EncodeConfig::new(q));
                let frame = crate::Av1Decoder::new()
                    .inspect(&coded.still.obus)
                    .unwrap()
                    .frame;
                assert!(frame.delta_lf_present, "{w}x{h} q{q} codes delta_lf");
                levels.push(frame.loop_filter.level[0]);
                assert_both_oracles_reconstruct(&coded, &format!("{w}x{h} q{q}"));
            }
        }
        // A frame level of 0 switches the loop filter off whatever DeltaLF says (§7.4), so the
        // accumulation is only under test where the encoder chose to deblock.
        assert!(
            levels.iter().any(|&l| l > 0),
            "no case deblocks: {levels:?}"
        );
    }

    #[test]
    fn the_block_variation_exercise_codes_segment_ids_both_decoders_follow() {
        // Oracle: segmentation_enabled with SEG_LVL_ALT_Q on two segments: every non-skip block is
        // assigned a position-only segment id (coded via neg_interleave under the
        // spatial-prediction context) and quantized at CurrentQIndex + the segment's alt-Q delta.
        // Textured content keeps blocks non-skip so the ids are actually coded.
        for q in [8u8, 32, 96, 180] {
            for (w, h) in [(32u32, 32u32), (64, 48), (96, 72)] {
                let p = planes(w, h, texture);
                let coded = exercised(&p, &Av1EncodeConfig::new(q));
                assert_both_oracles_reconstruct(&coded, &format!("{w}x{h} q{q}"));
            }
        }
    }

    #[test]
    fn the_block_variation_exercise_reaches_every_layout_both_decoders_follow() {
        // Oracle: the variation's symbols under each layout that changes how they are coded or
        // applied — tile rows and columns (segment-id prediction stops at a tile edge), a
        // whole-superblock block that is skip (which codes neither delta) or not (which codes
        // both), monochrome (one `delta_lf` whatever `NumPlanes`), 4:2:0 at 10 bits (the segment
        // quantizer through the deeper step tables), and superres (the coded grid narrower than
        // the display).
        let base = Av1EncodeConfig::new(90);
        let tiled = base.with_tiling(Tiling::new(1, 1));
        let p = planes(160, 136, texture);
        let coded = exercised(&p, &tiled);
        assert_both_oracles_reconstruct(&coded, "2x2 tiles");

        let solid = planes(128, 128, |_, _| [128, 90, 128]);
        let grad = planes(128, 128, |x, y| {
            [128, 90 + ((x % 64) / 8 + (y % 64) / 8) as u8, 128]
        });
        for q in [16u8, 64, 144] {
            let coded = exercised(&solid, &base.with_qindex(q));
            assert_both_oracles_reconstruct(&coded, &format!("skip superblocks q{q}"));
            let coded = exercised(&grad, &base.with_qindex(q));
            assert_both_oracles_reconstruct(&coded, &format!("64x64 gradient q{q}"));
        }

        let luma = (0..96u32 * 80)
            .map(|i| texture(i % 96, i / 96)[1])
            .collect::<Vec<_>>();
        let mono = Planar8::from_planes_subsampled(
            dims(96, 80),
            ChromaSubsampling::Cs400,
            [luma, Vec::new(), Vec::new()],
        )
        .unwrap();
        let coded = exercised(
            &mono,
            &base.with_qindex(70).with_color(Av1Color::monochrome()),
        );
        assert_both_oracles_reconstruct(&coded, "monochrome");

        let (w, h) = (96u32, 72u32);
        let deep = |p: usize, pw: u32, ph: u32| -> Vec<u16> {
            (0..pw * ph)
                .map(|i| u16::from(texture(i % pw, i / pw)[p]) * 4 + (i % 3) as u16)
                .collect()
        };
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let src = Planar16::from_planes_subsampled(
            dims(w, h),
            ChromaSubsampling::Cs420,
            BitDepth::Ten,
            [deep(1, w, h), deep(2, cw, ch), deep(0, cw, ch)],
        )
        .unwrap();
        let colour = Av1Color {
            matrix: MatrixCoefficients::Bt709,
            ..Av1Color::default()
        };
        for q in [40u8, 160] {
            let coded = encode16_with(
                &src,
                &base.with_qindex(q).with_color(colour),
                BlockVariation::SyntaxExercise,
            );
            assert_both_oracles_reconstruct(&coded.unwrap(), &format!("10-bit 4:2:0 q{q}"));
        }

        let wide = planes(192, 64, texture);
        let coded = encode_with(
            &wide,
            &base.with_qindex(100).with_superres(4),
            BlockVariation::SyntaxExercise,
        );
        assert_both_oracles_reconstruct(&coded.unwrap(), "superres");
    }

    #[cfg(feature = "decode")]
    #[test]
    fn the_block_variation_exercise_survives_the_restoration_pass_and_threaded_tiles() {
        // Oracle: a frame that restores a unit is coded twice (`CodedFrame::lossy`'s second pass),
        // and with tiles on several threads each worker's per-MI maps — the `delta_lf` one among
        // them — are merged back before the in-loop filters. Both must carry the variation, or the
        // second pass codes a different stream from the reconstruction the first one produced.
        let p = planes(640, 64, smooth_field);
        let config = Av1EncodeConfig::new(0).with_threads(NonZeroUsize::new(3).unwrap());
        let mut restored = Vec::new();
        for q in [110u8, 160, 200, 240] {
            let coded = exercised(&p, &config.with_qindex(q));
            let frame = crate::Av1Decoder::new()
                .inspect(&coded.still.obus)
                .unwrap()
                .frame;
            assert!(frame.delta_lf_present && frame.segmentation.enabled);
            assert!(
                frame.tile_info.tile_cols > 1,
                "tiles to spread over threads"
            );
            let lr = frame.lr.frame_restoration_type[0] == crate::RestorationType::Wiener;
            restored.push((q, lr, frame.loop_filter.level[0]));
            assert_both_oracles_reconstruct(&coded, &format!("640x64 q{q}, three threads"));
        }
        // A frame that restores no unit is coded once, and one deblocked at level 0 ignores
        // DeltaLF, so the claim needs a case that does both.
        assert!(
            restored.iter().any(|&(_, lr, level)| lr && level > 0),
            "no case is both restored and deblocked: {restored:?}"
        );
    }
}
