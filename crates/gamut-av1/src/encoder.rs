//! Top-level: turn planar samples — 4:4:4, 4:2:2 or 4:2:0 colour, or a monochrome luma plane —
//! into the AV1 temporal unit for an AVIF still image.

use std::num::NonZeroUsize;

use gamut_color::cicp::{ColorRange, MatrixCoefficients};
use gamut_color::{BitDepth, ChromaSubsampling, Planar8, Planar16};
use gamut_core::{Error, Result};

use crate::headers::{self, Av1Colour, Av1StillConfig, TileLayout, Tiling};
use crate::tile::{FrameEncoder, Reconstruction, Search};

/// How much mode-decision search the lossy intra encoder runs: a preset that trades search for
/// encode time.
///
/// Every preset produces a conformant stream that a decoder reconstructs bit-exactly to the
/// encoder's own [`ReconImage`]; they differ only in which candidates the encoder *tries* before
/// it commits to one. Each step down the list prunes search the step above it runs, so a faster
/// preset never considers a candidate a slower one skips. The default, [`Speed::Thorough`], is the
/// full search, and is what every entry point without a `speed` argument uses.
///
/// | Preset | Pruned relative to the one above | Search time saved |
/// |---|---|---|
/// | [`Thorough`](Self::Thorough) | nothing: every directional mode at `AngleDeltaY ∈ [-3, 3]`, five filter-intra modes, CfL `\|alpha\|` 1..=16, all five `TX_SET_INTRA_2` types, rectangular partitions, 4×4 leaves, palette | — |
/// | [`Balanced`](Self::Balanced) | directional modes are tried at `AngleDeltaY = 0` only | 48 of the 56 directional trials per ≥8×8 block |
/// | [`Fast`](Self::Fast) | filter-intra is not tried; CfL tries `\|alpha\|` ∈ {1, 2, 4, 8, 16} only; the transform type is `DCT_DCT` or `ADST_ADST` | 5 filter-intra trials per block up to 32×32, 22 of 32 CfL trials per chroma plane, 3 of 4 extra transform-type trials per luma transform |
/// | [`Fastest`](Self::Fastest) | only the non-directional luma modes (`DC`, `SMOOTH*`, `PAETH`); no CfL (chroma is `DC_PRED`); `DCT_DCT` only; no rectangular partitions; blocks are not split below 8×8 by choice; no palette | every directional trial, the CfL trials and palette scan, the last extra transform-type trial, and the 8×8 split trials with the 4×4 mode searches they bring |
///
/// **How candidates are weighed.** Every decision the presets prune — the luma mode, filter-intra,
/// the transform type, the CfL alphas, and whether an 8×8 block splits into 4×4 leaves — ranks its
/// candidates by rate–distortion cost: each candidate is trialled, its squared error measured on
/// the reconstruction a decoder would produce, and weighed against its rate priced from the
/// adapting CDFs. The luma mode is chosen in two passes where its transforms have a type to
/// search: every candidate is trialled with `DCT_DCT` alone, and the eight cheapest again with
/// the transform-type search. The decisions are made block by block, before the in-loop filters,
/// so a slower preset is not guaranteed smaller output on every image; measured on the fixed
/// corpus in `tests/preset_rd.rs`, [`Thorough`](Self::Thorough) needs fewer bits than every
/// faster preset for the same PSNR. The partition of 16×16 and larger blocks, the rectangular
/// partitions, the transform depth and palette are still decided by texture tests, at every
/// preset.
///
/// The presets prune *search*, so they move only lossy encodes. Lossless coding (`qindex = 0`)
/// has a single fixed path — `DC_PRED` and the 4×4 Walsh–Hadamard transform — and is identical at
/// every speed. The in-loop filters are not searched either: CDEF strengths and the Wiener taps
/// are fixed functions of `qindex`, so they too are the same at every speed.
///
/// The discriminants are permanent identifiers for C callers, not an ordering promise: a preset
/// added later is appended with the next free value wherever it falls on the speed axis.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[repr(u8)]
#[non_exhaustive]
pub enum Speed {
    /// The full search.
    #[default]
    Thorough = 0,
    /// Directional modes without the fine angle refinement.
    Balanced = 1,
    /// Additionally drops filter-intra, thins the CfL alpha ladder, and narrows the transform-type
    /// search to two types.
    Fast = 2,
    /// The minimum search: non-directional luma modes, `DC_PRED` chroma, `DCT_DCT`, square
    /// partitions no smaller than 8×8 by choice, and no palette.
    Fastest = 3,
}

impl Speed {
    /// Every preset, slowest first.
    pub const ALL: &'static [Self] = &[Self::Thorough, Self::Balanced, Self::Fast, Self::Fastest];
}

/// How an intra encode is carried out, beyond what it codes (the quantizer and the colour
/// signalling, which every entry point takes directly).
///
/// Plain data: build it from [`Default`] and the `with_*` setters, or set the public fields of a
/// default value. The default reproduces the encoder's output from before the options existed —
/// the full search, the [`Tiling::default`] layout, and one thread.
///
/// ```
/// use std::num::NonZeroUsize;
/// use gamut_av1::{EncodeOptions, Speed, Tiling};
///
/// let options = EncodeOptions::default()
///     .with_speed(Speed::Fast)
///     .with_tiling(Tiling::new(2, 1))
///     .with_threads(NonZeroUsize::new(4).unwrap());
/// assert_eq!(options.threads.get(), 4);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct EncodeOptions {
    /// How much mode-decision search to run (see [`Speed`]).
    pub speed: Speed,
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
    pub threads: NonZeroUsize,
}

impl Default for EncodeOptions {
    fn default() -> Self {
        Self {
            speed: Speed::default(),
            tiling: Tiling::default(),
            threads: NonZeroUsize::MIN,
        }
    }
}

impl EncodeOptions {
    /// These options with the search run at `speed`.
    #[must_use]
    pub const fn with_speed(mut self, speed: Speed) -> Self {
        self.speed = speed;
        self
    }

    /// These options with the frame coded in `tiling`.
    #[must_use]
    pub const fn with_tiling(mut self, tiling: Tiling) -> Self {
        self.tiling = tiling;
        self
    }

    /// These options with the encode using at most `threads` threads.
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
pub struct EncodedStill {
    /// The OBU byte stream to place in the AVIF `mdat` item (no temporal delimiter).
    pub obus: Vec<u8>,
    /// Sequence-header field values for `av1C` / `colr`.
    pub config: Av1StillConfig,
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
    /// The dimensions of plane `index`: the display dimensions for luma, and
    /// [`ChromaSubsampling::chroma_dimensions`] for the two chroma planes.
    ///
    /// # Panics
    ///
    /// Panics if `index >= 3`, like slice indexing.
    #[must_use]
    pub fn plane_dimensions(&self, index: usize) -> (u32, u32) {
        match index {
            0 => (self.width, self.height),
            1 | 2 => self.subsampling.chroma_dimensions(self.width, self.height),
            _ => panic!("plane index {index} out of range (0..3)"),
        }
    }
}

/// Encodes 8-bit 4:4:4 identity planes (Y=G, U=B, V=R) as a lossless AV1 intra keyframe.
///
/// # Errors
///
/// Returns [`Error::InvalidInput`] for zero-sized images, or [`Error::Unsupported`] if the
/// dimensions exceed AV1 level 6.0 (out of M0 scope).
pub fn encode_still_lossless_identity(planes: &Planar8) -> Result<EncodedStill> {
    Ok(encode_still_intra(planes, 0)?.0)
}

/// Encodes 8-bit 4:4:4 identity planes (Y=G, U=B, V=R) as an AV1 intra keyframe at quantizer
/// `qindex` (`base_q_idx`). `qindex == 0` is lossless; `1..=255` is lossy intra (DCT +
/// quantization), selecting the coefficient-CDF quantizer context per spec §8.3.2 (0 if `qindex`
/// ≤ 20, 1 if ≤ 60, 2 if ≤ 120, else 3). Returns the encoded still and the reconstruction (the
/// exact decoder output) for verification.
///
/// # Errors
///
/// Returns [`Error::InvalidInput`] for zero-sized images, or [`Error::Unsupported`] if the
/// dimensions exceed AV1 level 6.0.
pub fn encode_still_intra(planes: &Planar8, qindex: u8) -> Result<(EncodedStill, ReconImage)> {
    encode_with(
        planes,
        qindex,
        None,
        Av1Colour::default(),
        EncodeOptions::default(),
    )
}

/// Like [`encode_still_intra`] but signals `colour` instead of the default identity/full-range
/// configuration.
///
/// The planes must already be in the layout `colour.matrix` describes — GBR for
/// [`MatrixCoefficients::Identity`], `Y/Cb/Cr` for a luma–chroma matrix (see
/// [`gamut_color::Planar8::from_rgb8_matrix`]). The coding tools are matrix-agnostic; `colour` only
/// selects what the sequence header — and, through [`EncodedStill::config`], the container's `av1C`
/// and `colr` boxes — declare the samples to be.
///
/// This is also the entry point for a **monochrome** still: pass a
/// [`ChromaSubsampling::Cs400`](gamut_color::ChromaSubsampling::Cs400) buffer (one luma plane, no
/// chroma) together with [`Av1Colour::monochrome`]. The stream is then `seq_profile = 0` with
/// `mono_chrome = 1` and no chroma is coded at all.
///
/// # Errors
///
/// As [`encode_still_intra`], and [`Error::InvalidInput`] if monochrome planes are paired with
/// [`MatrixCoefficients::Identity`] — AV1 §6.4.2 permits that matrix only at subsampling 0/0, and
/// §5.5.2 infers 1/1 for a monochrome stream.
///
/// This runs the full search ([`Speed::Thorough`]); [`encode_still_intra_with_speed`] chooses a
/// faster preset.
pub fn encode_still_intra_with(
    planes: &Planar8,
    qindex: u8,
    colour: Av1Colour,
) -> Result<(EncodedStill, ReconImage)> {
    encode_still_intra_with_speed(planes, qindex, colour, Speed::default())
}

/// Like [`encode_still_intra_with`] but runs the mode-decision search at `speed`, pruning it for
/// encode time as the [`Speed`] table describes. [`Speed::Thorough`] produces exactly the
/// stream [`encode_still_intra_with`] does.
///
/// # Errors
///
/// As [`encode_still_intra_with`]; the speed never makes an encode fail.
pub fn encode_still_intra_with_speed(
    planes: &Planar8,
    qindex: u8,
    colour: Av1Colour,
    speed: Speed,
) -> Result<(EncodedStill, ReconImage)> {
    encode_still_intra_with_options(
        planes,
        qindex,
        colour,
        EncodeOptions::default().with_speed(speed),
    )
}

/// Like [`encode_still_intra_with`] but carried out as `options` says: the search
/// [`Speed`], the [`Tiling`] layout, and the thread cap the tiles are coded under.
/// [`EncodeOptions::default`] produces exactly the stream [`encode_still_intra_with`] does.
///
/// # Errors
///
/// As [`encode_still_intra_with`]; the options never make an encode fail. A tiling the frame
/// cannot take is clamped to the nearest one it can (see [`Tiling`]).
pub fn encode_still_intra_with_options(
    planes: &Planar8,
    qindex: u8,
    colour: Av1Colour,
    options: EncodeOptions,
) -> Result<(EncodedStill, ReconImage)> {
    encode_with(planes, qindex, None, colour, options)
}

/// Like [`encode_still_intra`] but codes the frame with horizontal **superres** (§7.16): the source
/// is downscaled to `FrameWidth = (UpscaledWidth*8 + denom/2)/denom` (where `denom = coded_denom + 9`,
/// `coded_denom` in `0..=7`), coded at that width, and the reconstruction is upscaled back to the
/// display width. Lossy path only.
pub fn encode_still_intra_superres(
    planes: &Planar8,
    qindex: u8,
    coded_denom: u8,
) -> Result<(EncodedStill, ReconImage)> {
    encode_with(
        planes,
        qindex,
        Some(coded_denom),
        Av1Colour::default(),
        EncodeOptions::default(),
    )
}

/// Validates the source layout and dimensions shared by every entry point, returning whether the
/// frame is monochrome.
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
/// subsampling_y = 1` there, so identity is illegal — which matters because `Av1Colour::default()`
/// *is* identity: a caller reaching here with monochrome planes and the default colour would
/// otherwise emit a non-conformant stream that libaom and dav1d are entitled to reject.
/// `Av1Colour::monochrome()` is the fix, and the error says so.
fn check_layout(
    width: u32,
    height: u32,
    subsampling: ChromaSubsampling,
    colour: Av1Colour,
    qindex: u8,
) -> Result<bool> {
    if width == 0 || height == 0 {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "image has a zero dimension",
        ));
    }
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
                 MC_IDENTITY only at subsampling 0/0); use Av1Colour::monochrome()"
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
    colour: Av1Colour,
) -> Result<Av1StillConfig> {
    if bit_depth == BitDepth::Sixteen {
        return Err(Error::unsupported(
            env!("CARGO_PKG_NAME"),
            "AV1: only 8-, 10- and 12-bit samples are coded (§6.4.1)",
        ));
    }
    let (sub_x, sub_y) = subsampling.subsampling();
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

/// Encodes **10- or 12-bit** 4:4:4 identity planes (Y=G, U=B, V=R), or a monochrome luma plane, as
/// an AV1 intra keyframe at quantizer `qindex` (`base_q_idx`; `0` selects the lossless path).
///
/// The depth comes from the buffer, which validated its own samples against it, and decides the
/// profile: 10-bit is profile 1 (or 0 monochrome) with `high_bitdepth`, and 12-bit of any plane
/// count is profile 2 with `twelve_bit` (§6.4.1). The 8-bit sibling is
/// [`encode_still_intra_with`]; a [`Planar16`] that carries [`BitDepth::Eight`] is accepted here
/// too and produces exactly the stream that entry point would.
///
/// Returns the temporal unit and the encoder's reconstruction — the samples a conformant decoder
/// produces, which at `qindex = 0` is the source.
///
/// # Errors
///
/// As [`encode_still_intra_with`], plus [`Error::Unsupported`] for [`BitDepth::Sixteen`], which is
/// not an AV1 sample depth (§6.4.1 defines 8, 10 and 12 only).
///
/// Superres has no high-bit-depth entry point: [`encode_still_intra_superres`] takes a
/// [`Planar8`], and its source downscale is written for 8-bit samples.
///
/// This runs the full search ([`Speed::Thorough`]); [`encode_still_intra16_with_speed`] chooses a
/// faster preset.
pub fn encode_still_intra16_with(
    planes: &Planar16,
    qindex: u8,
    colour: Av1Colour,
) -> Result<(EncodedStill, ReconImage)> {
    encode_still_intra16_with_speed(planes, qindex, colour, Speed::default())
}

/// Like [`encode_still_intra16_with`] but runs the mode-decision search at `speed`, as the
/// [`Speed`] table describes. [`Speed::Thorough`] produces exactly the stream
/// [`encode_still_intra16_with`] does.
///
/// # Errors
///
/// As [`encode_still_intra16_with`]; the speed never makes an encode fail.
pub fn encode_still_intra16_with_speed(
    planes: &Planar16,
    qindex: u8,
    colour: Av1Colour,
    speed: Speed,
) -> Result<(EncodedStill, ReconImage)> {
    encode_still_intra16_with_options(
        planes,
        qindex,
        colour,
        EncodeOptions::default().with_speed(speed),
    )
}

/// Like [`encode_still_intra16_with`] but carried out as `options` says, as
/// [`encode_still_intra_with_options`] describes. [`EncodeOptions::default`] produces exactly the
/// stream [`encode_still_intra16_with`] does.
///
/// # Errors
///
/// As [`encode_still_intra16_with`]; the options never make an encode fail.
pub fn encode_still_intra16_with_options(
    planes: &Planar16,
    qindex: u8,
    colour: Av1Colour,
    options: EncodeOptions,
) -> Result<(EncodedStill, ReconImage)> {
    let (width, height) = (planes.width(), planes.height());
    let monochrome = check_layout(width, height, planes.subsampling(), colour, qindex)?;
    let config = still_config(
        width,
        height,
        monochrome,
        planes.bit_depth(),
        planes.subsampling(),
        colour,
    )?;

    let seq_payload = headers::sequence_header_payload(&config, width, height, qindex > 0, false);
    let layout = TileLayout::resolve(
        options.tiling,
        width as usize,
        height as usize,
        config.seq_level_idx_0,
        None,
    );
    let mut frame_payload = headers::frame_header_payload(&layout, qindex, None, monochrome);
    let (tile_bytes, recon) = FrameEncoder::new16(planes, qindex)
        .with_search(Search::for_speed(options.speed))
        .encode(&layout, options.threads);
    append_tiles(&mut frame_payload, &tile_bytes);

    let recon_planes: [Vec<u16>; 3] = if qindex == 0 {
        // Lossless: the reconstruction *is* the source. Each plane crops at its own extent, so a
        // monochrome buffer's empty chroma planes stay empty.
        std::array::from_fn(|i| {
            let (pw, ph) = planes.plane_dimensions(i);
            crop(planes.plane(i), pw, pw, ph)
        })
    } else {
        restored_recon(&recon)
    };

    Ok((
        EncodedStill {
            obus: headers::assemble_temporal_unit(&seq_payload, &frame_payload),
            config,
        },
        ReconImage {
            width,
            height,
            bit_depth: planes.bit_depth(),
            subsampling: planes.subsampling(),
            planes: recon_planes,
        },
    ))
}

fn encode_with(
    planes: &Planar8,
    qindex: u8,
    coded_denom: Option<u8>,
    colour: Av1Colour,
    options: EncodeOptions,
) -> Result<(EncodedStill, ReconImage)> {
    let width = planes.width();
    let height = planes.height();
    // Superres over a monochrome source is refused rather than half-supported: the downscale below
    // is written for three luma-sized planes and relabels its result 4:4:4, so a monochrome buffer
    // would read an empty chroma slice at luma dimensions and then hand `FrameEncoder` a plane
    // count disagreeing with the `monochrome` the frame header was given. Checked *before*
    // `check_layout`, whose matrix rule would otherwise fire first for
    // `encode_still_intra_superres` — which supplies the default identity colour — and report a
    // reason that is not the one that applies.
    if coded_denom.is_some() && planes.subsampling() == ChromaSubsampling::Cs400 {
        return Err(Error::unsupported(
            env!("CARGO_PKG_NAME"),
            "AV1: superres over a monochrome source is not implemented",
        ));
    }
    let monochrome = check_layout(width, height, planes.subsampling(), colour, qindex)?;

    // Superres downscales the source horizontally to the coded (Frame) width; the reconstruction is
    // upscaled back to `width` at the end. `coded_src` is what the block encoder actually codes.
    let (coded_w, coded_src) = match coded_denom {
        Some(cd) => {
            let denom = cd as usize + 9;
            let dw = crate::filter::superres_downscaled_width(width as usize, denom);
            let dp: [Vec<u8>; 3] = std::array::from_fn(|i| {
                crate::filter::superres_downscale_plane(
                    planes.plane(i),
                    width as usize,
                    dw,
                    height as usize,
                )
            });
            (dw as u32, Planar8::from_planes(dw as u32, height, dp)?)
        }
        None => (width, planes.clone()),
    };

    let config = still_config(
        width,
        height,
        monochrome,
        BitDepth::Eight,
        planes.subsampling(),
        colour,
    )?;

    let seq_payload =
        headers::sequence_header_payload(&config, width, height, qindex > 0, coded_denom.is_some());
    let layout = TileLayout::resolve(
        options.tiling,
        coded_w as usize,
        height as usize,
        config.seq_level_idx_0,
        coded_denom,
    );
    let mut frame_payload = headers::frame_header_payload(&layout, qindex, coded_denom, monochrome);
    let (tile_bytes, recon) = FrameEncoder::new(&coded_src, qindex)
        .with_search(Search::for_speed(options.speed))
        .encode(&layout, options.threads);
    append_tiles(&mut frame_payload, &tile_bytes);

    // Crop the reconstruction from the coded grid to the display dimensions. For the lossless path
    // the reconstruction equals the source. With superres the coded grid is the downscaled width, so
    // each plane is cropped to `coded_w` and then upscaled horizontally to the display `width`.
    let (uw, uh) = (width as usize, height as usize);
    let recon_planes: [Vec<u16>; 3] = if qindex == 0 {
        // Lossless: the reconstruction equals the 8-bit source; widen it into the u16 recon buffer.
        // Each plane crops at its **own** extent — the source carries no coded padding, so the
        // stride is the plane width. A monochrome buffer's chroma planes are empty and stay empty;
        // cropping them at the luma extent would index past the end of a zero-length slice.
        std::array::from_fn(|i| {
            let (pw, ph) = planes.plane_dimensions(i);
            crop(planes.plane(i), pw, pw, ph)
                .into_iter()
                .map(u16::from)
                .collect()
        })
    } else if coded_denom.is_some() {
        // §7.4 order: superres upscale (downscaled → display width) happens **before** loop
        // restoration, which then runs on the upscaled luma. The deblocked-luma boundary is upscaled
        // too (only read by multi-stripe frames).
        // Per-plane source stride. The downscaled/upscaled widths stay luma-derived: §7.16 scales
        // them per plane by `Round2(w, subX)`, which only matters once a subsampled source can
        // reach here — `encode_with` rejects one today, and superres x subsampling is its own
        // slice.
        let mut up: [Vec<u16>; 3] = std::array::from_fn(|i| {
            crate::filter::superres_upscale_plane(
                &recon.planes[i],
                recon.geom[i].coded_w,
                coded_w as usize,
                uw,
                uh,
                recon.bit_depth,
            )
        });
        let deblock_up = crate::filter::superres_upscale_plane(
            &recon.deblocked_luma,
            recon.geom[0].coded_w,
            coded_w as usize,
            uw,
            uh,
            recon.bit_depth,
        );
        crate::filter::loop_restore_wiener_luma(
            &mut up[0],
            &deblock_up,
            uw,
            uw,
            uh,
            crate::filter::WIENER_DEFAULT,
            crate::filter::WIENER_DEFAULT,
            recon.bit_depth,
        );
        up
    } else {
        restored_recon(&recon)
    };

    let still = EncodedStill {
        obus: headers::assemble_temporal_unit(&seq_payload, &frame_payload),
        config,
    };
    let recon = ReconImage {
        width,
        height,
        bit_depth: BitDepth::from_bits(recon.bit_depth).ok_or_else(|| {
            Error::unsupported(
                env!("CARGO_PKG_NAME"),
                "AV1: unsupported reconstruction bit depth",
            )
        })?,
        subsampling: planes.subsampling(),
        planes: recon_planes,
    };
    Ok((still, recon))
}

/// The lossy reconstruction of a frame coded **without** superres: loop restoration on the luma
/// (§7.17), then each plane cropped from its own coded stride to its own visible extent.
fn restored_recon(recon: &Reconstruction) -> [Vec<u16>; 3] {
    let mut planes = recon.planes.clone();
    crate::filter::loop_restore_wiener_luma(
        &mut planes[0],
        &recon.deblocked_luma,
        recon.geom[0].coded_w,
        recon.geom[0].w,
        recon.geom[0].h,
        crate::filter::WIENER_DEFAULT,
        crate::filter::WIENER_DEFAULT,
        recon.bit_depth,
    );
    std::array::from_fn(|i| {
        let g = recon.geom[i];
        crop(&planes[i], g.w as u32, g.coded_w as u32, g.h as u32)
    })
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

    #[test]
    fn recon_image_reports_each_plane_at_its_own_size() {
        // 4:4:4: every plane is the display size.
        let planes = Planar8::from_planes(4, 2, [vec![0; 8], vec![0; 8], vec![0; 8]]).unwrap();
        let (_, recon) = encode_still_intra(&planes, 40).expect("encodes");
        assert_eq!(recon.subsampling, gamut_color::ChromaSubsampling::Cs444);
        for i in 0..3 {
            assert_eq!(recon.plane_dimensions(i), (4, 2), "plane {i}");
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
        assert_eq!(sub.plane_dimensions(0), (5, 3));
        assert_eq!(sub.plane_dimensions(1), (3, 2));
        assert_eq!(sub.plane_dimensions(2), (3, 2));
    }

    #[test]
    #[should_panic(expected = "plane index 3 out of range")]
    fn recon_image_plane_dimensions_rejects_an_out_of_range_index() {
        let planes = Planar8::from_planes(4, 2, [vec![0; 8], vec![0; 8], vec![0; 8]]).unwrap();
        let (_, recon) = encode_still_intra(&planes, 40).expect("encodes");
        let _ = recon.plane_dimensions(3);
    }

    #[test]
    fn a_vertical_edge_at_four_two_two_does_not_reach_an_invalid_chroma_block() {
        // A strong vertical edge is what makes `decide_rect` prefer PARTITION_VERT, producing a
        // taller-than-wide half whose 4:2:2 chroma residual is `BLOCK_INVALID` (§5.11.38). The
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
        let colour = Av1Colour {
            matrix: gamut_color::cicp::MatrixCoefficients::Bt709,
            ..Av1Colour::default()
        };
        let planes =
            Planar8::from_rgb8_matrix_subsampled(img, m, ChromaSubsampling::Cs422).unwrap();
        let (still, _) = encode_still_intra_with(&planes, 40, colour).expect("4:2:2 encodes");
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
        let colour = Av1Colour {
            matrix: gamut_color::cicp::MatrixCoefficients::Bt709,
            ..Av1Colour::default()
        };
        let err = encode_still_intra_with(&planes, 0, colour)
            .expect_err("lossless 4:2:0 is refused, not mis-coded");
        assert_eq!(
            err.static_message(),
            Some("AV1: lossless coding requires 4:4:4 or monochrome planes")
        );
        // The same planes encode on the lossy path, so the rejection is keyed on the quantizer.
        assert!(encode_still_intra_with(&planes, 40, colour).is_ok());
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

        let err = encode_still_intra(&planes, 40).expect_err("identity + 4:2:0 is not conformant");
        assert_eq!(
            err.static_message(),
            Some("AV1: the identity matrix requires 4:4:4 chroma")
        );
        // The same planes encode through a luma-chroma matrix, so the rejection is keyed on the
        // matrix and not on the subsampling alone.
        assert!(
            encode_still_intra_with(
                &planes,
                40,
                Av1Colour {
                    matrix: gamut_color::cicp::MatrixCoefficients::Bt709,
                    ..Av1Colour::default()
                }
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
        let colour = Av1Colour {
            matrix: gamut_color::cicp::MatrixCoefficients::Bt709,
            ..Av1Colour::default()
        };
        for (ss, want_profile, want_shifts) in [
            (ChromaSubsampling::Cs444, 1u8, (0u8, 0u8)),
            (ChromaSubsampling::Cs420, 0, (1, 1)),
            (ChromaSubsampling::Cs422, 2, (1, 0)),
        ] {
            let planes = Planar8::from_rgb8_matrix_subsampled(img, m, ss).unwrap();
            let (still, _) = encode_still_intra_with(&planes, 40, colour).expect("encodes");
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
        Planar8::from_rgb8_identity(&rgb, w, h).unwrap()
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
        // The encoder's structural choices — block partition (should_split/decide_rect), palette
        // (decide_palette/palette_cache/signal_palette_colors), per-superblock delta-q/lf
        // (signal_delta_q/lf), skip (block_is_skippable), tx selection (select_tx_*), loop-restoration
        // signalling (write_lr) — change the OBU bitstream but reconstruct bit-exactly, so the dav1d
        // recon oracle in recon_dav1d.rs cannot see them. This snapshot pins an FNV-1a checksum of the
        // OBU stream for a spread of content patterns, sizes and quantizers; any change to a
        // (deterministic) encoder decision moves a checksum. The recon oracle proves these same streams
        // decode correctly, so the pair certifies "deterministic AND correct", not just "deterministic".
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
            (1, 64, 64, 1, 0x177b_df34_05e6_6663),
            (2, 32, 32, 40, 0xad7f_19dc_a8cb_bf32),
            (3, 48, 48, 16, 0x1550_7e23_d162_f266),
            (4, 64, 48, 90, 0x5fed_6011_e0a0_278c),
            (0, 100, 80, 8, 0x9284_36c0_b8d2_8759),
            // One tile: two would leave the second 2 samples wide (Annex A.3, CroppedTileWidth).
            (0, 130, 70, 120, 0x30a2_32cb_6107_fbf8),
            (2, 64, 64, 200, 0x307c_fb34_8720_4baf),
            (3, 96, 96, 60, 0x3d5c_b862_2d85_f93b),
            (4, 128, 64, 30, 0x800d_d57f_9bd3_5fae),
        ];
        // Every case is encoded before any is compared, so one run reports every pin that moved.
        let got: Vec<(u8, u32, u32, u8, u64)> = cases
            .iter()
            .map(|&(id, w, h, q, _)| {
                let p = planes(w, h, |x, y| pat(id, x, y));
                let obus = encode_still_intra(&p, q).unwrap().0.obus;
                (id, w, h, q, fnv1a(&obus))
            })
            .collect();
        assert_eq!(got, cases, "a pattern's bitstream changed");
    }

    #[test]
    fn the_default_speed_is_the_thorough_search_the_speedless_entry_points_run() {
        // `encode_still_intra_with` and `encode_still_intra16_with` must stay the full-search
        // encode, which `encoded_bitstream_is_stable` pins. They reach it through
        // `Speed::default()`, so the default must be the full search, and the explicit-speed entry
        // points at that preset must be the same encode byte for byte.
        assert_eq!(Speed::default(), Speed::Thorough);
        let p = planes(64, 48, |x, y| {
            [(x * 7 + y) as u8, (x ^ (y * 3)) as u8, (x + y * 5) as u8]
        });
        let colour = Av1Colour::default();
        let plain = encode_still_intra_with(&p, 40, colour).unwrap().0.obus;
        let speed = encode_still_intra_with_speed(&p, 40, colour, Speed::Thorough).unwrap();
        assert_eq!(plain, speed.0.obus);

        let deep = Planar16::from_planes(
            64,
            48,
            BitDepth::Ten,
            std::array::from_fn(|i| p.plane(i).iter().map(|&v| u16::from(v) << 2).collect()),
        )
        .unwrap();
        let plain = encode_still_intra16_with(&deep, 40, colour).unwrap().0.obus;
        let speed = encode_still_intra16_with_speed(&deep, 40, colour, Speed::Thorough).unwrap();
        assert_eq!(plain, speed.0.obus);
    }

    #[test]
    fn the_default_options_are_the_encode_without_options() {
        // `EncodeOptions::default()` must be the full search on the legacy two-column layout on one
        // thread, so the option-taking entry points reproduce the pinned streams above.
        let options = EncodeOptions::default();
        assert_eq!(options.speed, Speed::Thorough);
        assert_eq!(options.tiling, Tiling::new(1, 0));
        assert_eq!(options.threads.get(), 1);
        let built = EncodeOptions::default()
            .with_speed(Speed::Fast)
            .with_tiling(Tiling::new(2, 3))
            .with_threads(NonZeroUsize::new(5).unwrap());
        assert_eq!(
            (built.speed, built.tiling, built.threads.get()),
            (Speed::Fast, Tiling::new(2, 3), 5)
        );
    }

    #[test]
    fn the_stream_and_reconstruction_do_not_depend_on_the_thread_count() {
        // Tiles are coded independently, so the threads that code them must not show in the
        // output: every thread count, including more threads than tiles, gives the one-thread
        // stream and reconstruction. The reconstruction is what proves the merge — the tiles a
        // spawned worker codes reach the in-loop filters only through `absorb_tile` — and the
        // content mixes texture, palette-like steps and stripes so each merged per-MI map
        // (partition sizes, transform sizes, skip, delta-LF) varies across the frame.
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
        let bt709 = Av1Colour {
            matrix: MatrixCoefficients::Bt709,
            ..Av1Colour::default()
        };
        let mono = Planar8::from_planes_subsampled(
            200,
            150,
            ChromaSubsampling::Cs400,
            [rgb.plane(0).to_vec(), Vec::new(), Vec::new()],
        )
        .unwrap();
        let cases: [(&str, Planar8, u8, Av1Colour, Tiling); 5] = [
            (
                "4:4:4 q40",
                rgb.clone(),
                40,
                Av1Colour::default(),
                Tiling::new(2, 1),
            ),
            (
                "4:4:4 lossless",
                rgb,
                0,
                Av1Colour::default(),
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
                Av1Colour::monochrome(),
                Tiling::new(2, 1),
            ),
        ];
        let threads = |n: usize| NonZeroUsize::new(n).unwrap();
        for (name, p, q, colour, tiling) in &cases {
            let at = |n: usize| {
                let options = EncodeOptions::default()
                    .with_tiling(*tiling)
                    .with_threads(threads(n));
                encode_still_intra_with_options(p, *q, *colour, options).unwrap()
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
            200,
            150,
            BitDepth::Ten,
            std::array::from_fn(|i| {
                planes(200, 150, mix)
                    .plane(i)
                    .iter()
                    .map(|&v| u16::from(v) << 2)
                    .collect()
            }),
        )
        .unwrap();
        let at = |n: usize| {
            let options = EncodeOptions::default()
                .with_tiling(Tiling::new(2, 1))
                .with_threads(threads(n));
            encode_still_intra16_with_options(&deep, 40, Av1Colour::default(), options).unwrap()
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
            let options = EncodeOptions::default().with_tiling(tiling);
            encode_still_intra_with_options(&p, 40, Av1Colour::default(), options)
                .unwrap()
                .0
                .obus
        };
        let default = at(Tiling::default());
        assert_eq!(default, encode_still_intra(&p, 40).unwrap().0.obus);
        assert_ne!(at(Tiling::SINGLE), default);
        assert_ne!(at(Tiling::new(1, 1)), default);
        assert_ne!(at(Tiling::new(2, 0)), default);
    }

    #[test]
    fn lossless_coding_is_the_same_at_every_speed() {
        // The presets prune the lossy mode decisions only; lossless has one fixed path (DC_PRED,
        // the 4×4 WHT, PARTITION_NONE), so there is nothing for a faster preset to skip.
        let p = planes(72, 40, |x, y| [(x * 3) as u8, (y * 5) as u8, (x ^ y) as u8]);
        let reference = encode_still_intra(&p, 0).unwrap().0.obus;
        for &speed in Speed::ALL {
            let (still, _) =
                encode_still_intra_with_speed(&p, 0, Av1Colour::default(), speed).unwrap();
            assert_eq!(still.obus, reference, "{speed:?}");
        }
    }

    #[test]
    fn each_speed_preset_has_a_stable_bitstream() {
        // The speed analogue of `encoded_bitstream_is_stable`: which candidates each preset tries
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
        let cases: &[(Speed, u8, u64)] = &[
            (Speed::Thorough, 40, 0xcf58_54f5_27e6_5f22),
            (Speed::Balanced, 40, 0x200f_31c7_392c_f008),
            (Speed::Fast, 40, 0x81fc_3c50_6d1f_af6b),
            (Speed::Fastest, 40, 0x8c07_5f06_da36_ed6b),
            (Speed::Balanced, 120, 0x15ed_afbe_e48c_f7c0),
            (Speed::Fast, 120, 0x5b33_df99_1ecd_416d),
            (Speed::Fastest, 120, 0xa961_d343_7869_f44f),
        ];
        // Every case is encoded before any is compared, so one run reports every pin that moved.
        let got: Vec<(Speed, u8, u64)> = cases
            .iter()
            .map(|&(speed, q, _)| {
                let (still, _) =
                    encode_still_intra_with_speed(&mix, q, Av1Colour::default(), speed).unwrap();
                (speed, q, fnv1a(&still.obus))
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
            let n = encode_still_intra(p, q).unwrap().0.obus.len();
            assert!(
                n < static_bytes,
                "{name} q{q}: {n} bytes, not below the static-CDF baseline of {static_bytes}"
            );
        }
    }

    #[test]
    fn obu_stream_is_seq_then_frame() {
        let p = planes(40, 24, |x, y| [(x * 3) as u8, (y * 5) as u8, (x + y) as u8]);
        let e = encode_still_lossless_identity(&p).unwrap();
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
            encode_still_lossless_identity(&p).unwrap().obus,
            encode_still_lossless_identity(&p).unwrap().obus
        );
    }

    #[test]
    fn solid_color_uses_all_zero_path() {
        // A flat image makes every residual zero, exercising the txb_skip = all_zero branch.
        let e = encode_still_lossless_identity(&planes(64, 64, |_, _| [200, 100, 50])).unwrap();
        assert!(!e.obus.is_empty());
    }

    #[test]
    fn high_contrast_exercises_golomb() {
        // A ±max checkerboard produces large WHT coefficients (golomb tails).
        let e = encode_still_lossless_identity(&planes(48, 48, |x, y| {
            let v = if (x + y) % 2 == 0 { 0 } else { 255 };
            [v, 255 - v, v]
        }))
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
            let e = encode_still_lossless_identity(&p).unwrap();
            assert_eq!(parse_obus(&e.obus).len(), 2);
        }
    }

    #[test]
    fn rejects_zero_dimension() {
        // Each axis is rejected independently: a 0×0, a 0×4 and a 4×0 image must all fail with the
        // guard's own message. The mixed cases (exactly one zero) are what force the guard to be an
        // `||` — under `&&` a 0×4 slips through to fault deeper in the encoder.
        for (w, h) in [(0, 0), (0, 4), (4, 0)] {
            let p = Planar8::from_rgb8_identity(&[], w, h).unwrap();
            match encode_still_intra(&p, 0) {
                Err(error) if error.kind() == gamut_core::ErrorKind::InvalidInput => {
                    assert_eq!(
                        error.static_message(),
                        Some("image has a zero dimension"),
                        "{w}x{h}"
                    );
                }
                other => panic!("{w}x{h}: expected zero-dimension InvalidInput, got {other:?}"),
            }
        }
    }

    #[test]
    fn lossless_sequence_header_clears_lossy_flags() {
        // `encode_with` passes `qindex > 0` as the sequence header's `lossy` flag, which gates
        // enable_filter_intra/cdef/restoration. At qindex 0 (lossless) those bits must be 0; at a
        // lossy qindex they are 1. Same image and no superres, so the sequence-header payload differs
        // *only* by that flag — distinguishing `qindex > 0` from an always-true `qindex >= 0`.
        let p = planes(40, 24, |x, y| [(x * 3) as u8, (y * 5) as u8, (x + y) as u8]);
        let (lossless, _) = encode_still_intra(&p, 0).unwrap();
        let (lossy, _) = encode_still_intra(&p, 8).unwrap();
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
                let (still, recon) = encode_still_intra(&p, q).unwrap();
                assert_eq!(parse_obus(&still.obus).len(), 2, "{w}x{h} q{q}");
                assert_eq!(recon.width, w);
                assert_eq!(recon.height, h);
                for plane in &recon.planes {
                    assert_eq!(plane.len(), (w * h) as usize);
                }
                // Determinism.
                let (again, _) = encode_still_intra(&p, q).unwrap();
                assert_eq!(still.obus, again.obus, "{w}x{h} q{q} not deterministic");
            }
        }
    }

    #[test]
    fn lossy_flat_image_reconstructs_near_source() {
        // A solid color quantizes every AC residual to zero; the DC-prediction reconstruction
        // should land within a couple of levels of the source (light quantization).
        let (_, recon) = encode_still_intra(&planes(48, 40, |_, _| [200, 100, 50]), 12).unwrap();
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
    fn default_colour_entry_point_matches_the_identity_one() {
        // `encode_still_intra_with(_, _, Av1Colour::default())` must be the *same* encode as
        // `encode_still_intra` — otherwise the default path's pinned checksums would not cover it.
        let p = planes(40, 24, |x, y| [(x * 3) as u8, (y * 5) as u8, (x + y) as u8]);
        for &q in &[0u8, 40] {
            let plain = encode_still_intra(&p, q).unwrap().0;
            let with = encode_still_intra_with(&p, q, Av1Colour::default())
                .unwrap()
                .0;
            assert_eq!(plain.obus, with.obus, "q{q}");
            assert_eq!(
                plain.config.matrix_coefficients,
                with.config.matrix_coefficients
            );
        }
    }

    #[test]
    fn colour_is_mirrored_into_the_config_and_the_sequence_header() {
        use gamut_color::cicp::{ColourPrimaries, MatrixCoefficients, TransferCharacteristics};

        let p = planes(40, 24, |x, y| [(x * 3) as u8, (y * 5) as u8, (x + y) as u8]);
        let colour = Av1Colour {
            primaries: ColourPrimaries::Bt2020,
            transfer: TransferCharacteristics::Srgb,
            matrix: MatrixCoefficients::Bt2020Ncl,
            range: ColorRange::Limited,
        };
        let (still, _) = encode_still_intra_with(&p, 40, colour).unwrap();
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
        let identity = encode_still_intra(&p, 40).unwrap().0;
        assert_ne!(
            seq_header_payload(&still.obus),
            seq_header_payload(&identity.obus)
        );
    }

    #[test]
    fn colour_range_bit_is_coded_only_outside_the_srgb_shortcut() {
        use gamut_color::cicp::{ColourPrimaries, MatrixCoefficients, TransferCharacteristics};

        // AV1 §5.5.2: with BT.709 primaries + sRGB transfer + identity matrix, `color_range` is
        // inferred and *no* bit is coded; any other combination codes it explicitly. Flipping only
        // the range must therefore change the payload for a real matrix…
        let p = planes(40, 24, |x, y| [(x * 3) as u8, (y * 5) as u8, (x + y) as u8]);
        let base = Av1Colour {
            primaries: ColourPrimaries::Bt709,
            transfer: TransferCharacteristics::Srgb,
            matrix: MatrixCoefficients::Bt709,
            range: ColorRange::Full,
        };
        let full = encode_still_intra_with(&p, 40, base).unwrap().0;
        let limited = encode_still_intra_with(
            &p,
            40,
            Av1Colour {
                range: ColorRange::Limited,
                ..base
            },
        )
        .unwrap()
        .0;
        assert_ne!(
            seq_header_payload(&full.obus),
            seq_header_payload(&limited.obus),
            "color_range must be coded when the sRGB shortcut does not apply"
        );

        // …and under the shortcut a studio-range request is refused rather than silently signalled
        // as full range.
        let shortcut = Av1Colour {
            range: ColorRange::Limited,
            ..Av1Colour::default()
        };
        match encode_still_intra_with(&p, 40, shortcut) {
            Err(error) if error.kind() == gamut_core::ErrorKind::InvalidInput => {}
            other => panic!("expected an InvalidInput for shortcut + studio range, got {other:?}"),
        }
    }

    #[test]
    fn lossy_high_contrast_encodes() {
        // A ±max checkerboard makes large coefficients (golomb tails) in the lossy path too.
        let (still, recon) = encode_still_intra(
            &planes(48, 48, |x, y| {
                let v = if (x + y) % 2 == 0 { 0 } else { 255 };
                [v, 255 - v, v]
            }),
            16,
        )
        .unwrap();
        assert_eq!(parse_obus(&still.obus).len(), 2);
        assert_eq!(recon.planes[0].len(), 48 * 48);
    }
}
