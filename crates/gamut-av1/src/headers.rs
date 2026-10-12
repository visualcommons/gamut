//! AV1 OBU framing (§5.3, Annex B), the reduced-still-picture sequence header (§5.5), and the
//! all-intra keyframe uncompressed frame header (§5.9.2). Both the lossless M0 config and the lossy
//! intra path are emitted: `base_q_idx > 0` with per-superblock delta-Q/delta-LF, segmentation
//! (`SEG_LVL_ALT_Q`), CDEF, loop restoration, superres, and uniform multi-tile — all under
//! `disable_cdf_update = 0` (adapting CDFs, §8.2.6) and `using_qmatrix = 0` (8-bit 4:4:4).
//!
//! `color_config()` (§5.5.2) is parameterized by [`Av1Colour`]: the identity/sRGB default takes the
//! spec's shortcut, where `color_range` and the subsampling are inferred and no bits are coded,
//! while any other CICP triple codes `color_range` explicitly.

use gamut_bitstream::{BitWriter, write_leb128};
use gamut_color::cicp::{ColorRange, ColourPrimaries, MatrixCoefficients, TransferCharacteristics};
use gamut_core::{Error, Result};

/// `OBU_SEQUENCE_HEADER`.
const OBU_SEQUENCE_HEADER: u8 = 1;
/// `OBU_FRAME` (frame header + tile group in one OBU).
pub(crate) const OBU_FRAME: u8 = 6;

/// `TileSizeBytes` (§5.11.1): the width of each non-final tile's little-endian size prefix in the
/// tile group. Four bytes covers any tile a still image produces; signalled as `minus_1` in the
/// frame header's `tile_info`.
pub(crate) const TILE_SIZE_BYTES: usize = 4;

/// Whether `color_config()` codes `subsampling_x` explicitly (§5.5.2).
///
/// Only profile 2 at 12 bits does: profiles 0 and 1 infer their subsampling (4:2:0 and 4:4:4), and
/// profile 2 below 12 bits is the fixed 4:2:2 pair.
///
/// The two conditions are **not** interchangeable, though an earlier revision of this encoder
/// could treat them as such: back when profile 2 was reachable only by asking for 12 bits, testing
/// either one answered for both. Profile 2 is now also how 8- and 10-bit 4:2:2 is coded, so the
/// depth has to be tested on its own.
///
/// `twelve_bit` alone is not that test. §5.5.2 derives `BitDepth == 12` from
/// `seq_profile == 2 && high_bitdepth && twelve_bit`, and the writer above emits the `twelve_bit`
/// bit under exactly that guard — so a config with `twelve_bit` set but `high_bitdepth` clear
/// codes no bit for it, and reading `twelve_bit` here would then code a `subsampling_x` the
/// decoder is not expecting and shift every field after it.
fn codes_subsampling(cfg: &Av1StillConfig) -> bool {
    cfg.seq_profile == 2 && cfg.high_bitdepth && cfg.twelve_bit
}

/// The sequence-header field values that `gamut-avif` must mirror into `av1C` and `colr`
/// (AV1-ISOBMFF v1.3.0 §2.3.4). Fixed by the M0 config except `seq_level_idx_0`, which depends on
/// the image size.
#[derive(Debug, Clone, Copy)]
pub struct Av1StillConfig {
    /// `seq_profile` (§6.4.1): 1 (High) for 8/10-bit 4:4:4, 0 (Main) for an 8/10-bit monochrome
    /// still, 2 (Professional) for anything 12-bit.
    pub seq_profile: u8,
    /// `seq_level_idx[0]`, the smallest level (≤ 6.0) whose limits cover the image.
    pub seq_level_idx_0: u8,
    /// `seq_tier[0]` = 0.
    pub seq_tier_0: u8,
    /// `high_bitdepth`: false at 8 bits, true at 10 and 12.
    pub high_bitdepth: bool,
    /// `twelve_bit`: coded only when `seq_profile == 2 && high_bitdepth`, and true only at 12 bits.
    pub twelve_bit: bool,
    /// `mono_chrome`: one coded luma plane and no chroma. Per §5.5.2 the subsampling is inferred
    /// `subsampling_x = subsampling_y = 1`. The profile follows §6.4.1 rather than the flag: 0 at
    /// 8/10 bits, 2 at 12, since Main does not reach 12-bit.
    pub monochrome: bool,
    /// `subsampling_x`: 0 for 4:4:4, 1 when [`monochrome`](Self::monochrome). Inferred rather than
    /// coded except under `seq_profile == 2` at 12 bits, which codes it (§5.5.2).
    pub chroma_subsampling_x: u8,
    /// `subsampling_y`: 0 for 4:4:4, 1 when [`monochrome`](Self::monochrome). Coded only when
    /// [`chroma_subsampling_x`](Self::chroma_subsampling_x) is coded and set (§5.5.2).
    pub chroma_subsampling_y: u8,
    /// `chroma_sample_position` = 0.
    pub chroma_sample_position: u8,
    /// CICP `color_primaries` (BT.709 = 1).
    pub color_primaries: u16,
    /// CICP `transfer_characteristics` (sRGB = 13).
    pub transfer_characteristics: u16,
    /// CICP `matrix_coefficients` (Identity = 0).
    pub matrix_coefficients: u16,
    /// `color_range` (full = true).
    pub full_range: bool,
}

impl Av1StillConfig {
    /// Whether `color_config()` takes the AV1 §5.5.2 sRGB shortcut: with BT.709 primaries, the sRGB
    /// transfer function and the identity matrix, `color_range`, `subsampling_x` and
    /// `subsampling_y` are all **inferred** (full range, 4:4:4) and no bits are coded for them.
    /// Outside the shortcut `color_range` is coded explicitly.
    ///
    /// Always `false` for a monochrome stream: §5.5.2's `mono_chrome` branch precedes the shortcut
    /// and returns, so the shortcut is unreachable there however the CICP triple reads.
    #[must_use]
    pub fn is_srgb_shortcut(&self) -> bool {
        !self.monochrome
            && self.color_primaries == ColourPrimaries::Bt709.code_point()
            && self.transfer_characteristics == TransferCharacteristics::Srgb.code_point()
            && self.matrix_coefficients == MatrixCoefficients::Identity.code_point()
    }
}

/// The colour signalling one still carries: the CICP triple plus the signal range, mirrored
/// identically into the AV1 sequence header's `color_config()` and the container's `colr` box.
///
/// [`Default`] is the M0 configuration — BT.709 primaries, sRGB transfer, **identity** matrix, full
/// range — i.e. RGB carried directly as 4:4:4 GBR planes, which is what
/// [`encode_still_intra`](crate::encode_still_intra) and its siblings use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Av1Colour {
    /// CICP colour primaries.
    pub primaries: ColourPrimaries,
    /// CICP transfer characteristics.
    pub transfer: TransferCharacteristics,
    /// CICP matrix coefficients. `Identity` means the planes carry GBR, not YCbCr.
    pub matrix: MatrixCoefficients,
    /// The signal range the samples occupy.
    pub range: ColorRange,
}

impl Default for Av1Colour {
    fn default() -> Self {
        Self {
            primaries: ColourPrimaries::Bt709,
            transfer: TransferCharacteristics::Srgb,
            matrix: MatrixCoefficients::Identity,
            range: ColorRange::Full,
        }
    }
}

impl Av1Colour {
    /// The colour signalling for a **monochrome** still: BT.709 primaries, sRGB transfer, full
    /// range, and `Unspecified` matrix coefficients.
    ///
    /// The matrix is not a free choice here. AV1 §5.5.2 infers `subsampling_x = subsampling_y = 1`
    /// for a monochrome stream, and §6.4.2 makes `MC_IDENTITY` legal *only* when both are 0 — so
    /// the identity matrix of [`Av1Colour::default`] is non-conformant on a single-plane stream.
    /// There is no luma–chroma transform to describe when there is no chroma, so `Unspecified` is
    /// the honest code point (and what AVIF readers expect on an alpha auxiliary item).
    ///
    /// # Examples
    ///
    /// ```
    /// use gamut_av1::Av1Colour;
    /// use gamut_color::cicp::MatrixCoefficients;
    /// assert_eq!(Av1Colour::monochrome().matrix, MatrixCoefficients::Unspecified);
    /// ```
    #[must_use]
    pub fn monochrome() -> Self {
        Self {
            matrix: MatrixCoefficients::Unspecified,
            ..Self::default()
        }
    }
}

/// Level limits (`seq_level_idx`, MaxHSize, MaxVSize, MaxPicSize) for the defined levels up to 6.0
/// (AV1 Annex A §10.3). One representative entry per spatial tier — sufficient to choose the
/// smallest covering level.
const LEVELS: [(u8, u32, u32, u64); 7] = [
    (0, 2048, 1152, 147_456),      // 2.0
    (1, 2816, 1584, 278_784),      // 2.1
    (4, 4352, 2448, 665_856),      // 3.0
    (5, 5504, 3096, 1_065_024),    // 3.1
    (8, 6144, 3456, 2_359_296),    // 4.0
    (12, 8192, 4352, 8_912_896),   // 5.0
    (16, 16384, 8704, 35_651_584), // 6.0
];

/// Returns the smallest `seq_level_idx` (≤ 6.0) whose limits cover `width × height`.
///
/// # Errors
///
/// [`Error::Unsupported`] if the image exceeds the level-6.0 limits (out of M0 scope).
pub fn pick_level(width: u32, height: u32) -> Result<u8> {
    let pixels = u64::from(width) * u64::from(height);
    for &(idx, max_h, max_v, max_pic) in &LEVELS {
        if width <= max_h && height <= max_v && pixels <= max_pic {
            return Ok(idx);
        }
    }
    Err(Error::unsupported(
        env!("CARGO_PKG_NAME"),
        "image dimensions exceed AV1 level 6.0 limits",
    ))
}

/// `MaxTiles` and `MaxTileCols` (Annex A.3, the level table) for `seq_level_idx`.
///
/// Every level of one major version shares the pair, and `seq_level_idx` numbers the minor levels
/// consecutively within a major (`2.0..=2.3` is `0..=3`, `3.x` is `4..=7`, …), so the major is
/// `seq_level_idx >> 2`. Levels past 6.x are not defined; [`pick_level`] never returns one, and
/// they take the 6.x pair.
fn level_tile_limits(seq_level_idx: u8) -> (usize, usize) {
    match seq_level_idx >> 2 {
        0 => (8, 4),
        1 => (16, 6),
        2 => (32, 8),
        3 => (64, 8),
        _ => (128, 16),
    }
}

/// The uniform tile layout an encode asks for (AV1 §5.9.15 with `uniform_tile_spacing_flag = 1`):
/// `1 << cols_log2` tile columns by `1 << rows_log2` tile rows of 64×64 superblocks.
///
/// Tiles are what makes an AV1 frame parallel: each one is entropy-coded and predicted without
/// reference to any other, so the encoder can code them concurrently (see
/// [`EncodeOptions::threads`](crate::EncodeOptions::threads)) and a decoder can decode them
/// concurrently. What they cost is compression: every tile starts from the default probabilities
/// and treats its edges as frame edges, so more tiles mean a larger file. The output for a given
/// layout does not depend on how many threads encoded it.
///
/// The request is a **ceiling and a floor the frame then clamps**, not a guarantee. For each frame
/// the encoder takes the nearest layout that is legal:
///
/// - **§5.9.15 bounds.** At most one tile column per superblock column and one tile row per
///   superblock row (and at most 64 of each); and at least as many as keep every tile within
///   `MAX_TILE_WIDTH` (4096 luma samples) and `MAX_TILE_AREA` (4096 × 2304).
/// - **The level's limits** (Annex A.3): no more than `MaxTileCols` columns and `MaxTiles` tiles at
///   the level the frame size selects.
/// - **Superres** (Annex A.3): a tile that is not the rightmost is at least 128 coded samples wide,
///   and every tile's upscaled width stays within 4096.
/// - **Cropped tile size** (Annex A.3): every tile starts at least 8 luma samples before the
///   frame's right and bottom edges (`CroppedTileWidth`/`CroppedTileHeight >= 8`). A split whose
///   last column or row would hold only the frame's final 1–7 samples takes fewer columns or rows.
///
/// Uniform spacing rounds the tile size up, so the coded column count is `ceil(sbCols /
/// ceil(sbCols >> cols_log2))` and may be fewer than `1 << cols_log2`; rows likewise.
///
/// [`Default`] is `cols_log2 = 1, rows_log2 = 0`: two tile columns once the frame is at least two
/// superblocks wide, else one tile — the layout the encoder used before the option existed. The
/// cropped-size rule keeps one tile at widths 65–71 and 129–135, where the second column would
/// hold fewer than 8 samples; the encoder coded those widths as two tiles before, a stream that
/// broke its level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Tiling {
    /// `TileColsLog2` requested: `1 << cols_log2` tile columns.
    pub cols_log2: u8,
    /// `TileRowsLog2` requested: `1 << rows_log2` tile rows.
    pub rows_log2: u8,
}

impl Tiling {
    /// One tile for the whole frame, wherever the frame is small enough for §5.9.15 to allow it.
    pub const SINGLE: Self = Self::new(0, 0);

    /// A request for `1 << cols_log2` tile columns by `1 << rows_log2` tile rows.
    ///
    /// # Examples
    ///
    /// ```
    /// use gamut_av1::Tiling;
    /// let four_by_two = Tiling::new(2, 1);
    /// assert_eq!((four_by_two.cols_log2, four_by_two.rows_log2), (2, 1));
    /// ```
    #[must_use]
    pub const fn new(cols_log2: u8, rows_log2: u8) -> Self {
        Self {
            cols_log2,
            rows_log2,
        }
    }
}

impl Default for Tiling {
    fn default() -> Self {
        Self::new(1, 0)
    }
}

/// `MAX_TILE_WIDTH >> sbSize` (§5.9.15) for 64×64 superblocks: 4096 luma samples.
const MAX_TILE_WIDTH_SB: usize = 4096 >> 6;
/// `MAX_TILE_AREA >> (2 * sbSize)` (§5.9.15) for 64×64 superblocks: 4096 × 2304 luma samples.
const MAX_TILE_AREA_SB: usize = (4096 * 2304) >> 12;
/// `MAX_TILE_COLS` and `MAX_TILE_ROWS` (§3).
const MAX_TILE_COLS_ROWS: usize = 64;
/// MI units per 64×64 superblock (`sbShift`, §5.9.15).
const SB_MI: usize = 16;

/// The tile layout one frame is coded with: [`Tiling`] resolved against the frame's superblock
/// grid, its level and superres, carrying the §5.9.15 bounds the `tile_info()` writer codes the
/// layout against and the `MiColStarts`/`MiRowStarts` the tile encoder walks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TileLayout {
    /// `TileColsLog2`.
    pub cols_log2: usize,
    /// `TileRowsLog2`.
    pub rows_log2: usize,
    /// `minLog2TileCols`: where the `increment_tile_cols_log2` loop starts.
    min_cols_log2: usize,
    /// `maxLog2TileCols`: where it stops without a terminating zero.
    max_cols_log2: usize,
    /// `minLog2TileRows` for this `TileColsLog2`.
    min_rows_log2: usize,
    /// `maxLog2TileRows`.
    max_rows_log2: usize,
    /// `MiColStarts[0..=TileCols]`.
    pub col_starts: Vec<usize>,
    /// `MiRowStarts[0..=TileRows]`.
    pub row_starts: Vec<usize>,
}

/// `MiColStarts`/`MiRowStarts` (§5.9.15, uniform spacing) for `sb` superblocks split at `log2`,
/// ending at `mi` (the frame's `MiCols`/`MiRows`).
fn uniform_starts(sb: usize, log2: usize, mi: usize) -> Vec<usize> {
    let size_sb = sb.div_ceil(1 << log2);
    let mut starts: Vec<usize> = (0..sb).step_by(size_sb).map(|s| s * SB_MI).collect();
    starts.push(mi);
    starts
}

impl TileLayout {
    /// Resolves `request` for a frame of `frame_width × frame_height` luma samples (`FrameWidth`,
    /// the coded width under superres) at `seq_level_idx`, coded with superres `coded_denom` when
    /// present. The [`Tiling`] documentation states the rules; the §5.9.15 syntax bounds are
    /// applied last, so they win over a level limit (a combination no defined level produces).
    pub(crate) fn resolve(
        request: Tiling,
        frame_width: usize,
        frame_height: usize,
        seq_level_idx: u8,
        superres_coded_denom: Option<u8>,
    ) -> Self {
        // `MiCols`/`MiRows` (§7.4 `compute_image_size`).
        let mi_cols = 2 * ((frame_width + 7) >> 3);
        let mi_rows = 2 * ((frame_height + 7) >> 3);
        let sb_cols = mi_cols.div_ceil(SB_MI);
        let sb_rows = mi_rows.div_ceil(SB_MI);
        let width_sb = |cols_log2: usize| sb_cols.div_ceil(1 << cols_log2);
        let height_sb = |rows_log2: usize| sb_rows.div_ceil(1 << rows_log2);
        let count = |sb: usize, size: usize| sb.div_ceil(size);
        // Annex A.3: `CroppedTileWidth` / `CroppedTileHeight` (the frame extent past a tile's first
        // sample) is at least 8 for each tile. The last tile starts furthest in, so it alone
        // decides; a split whose last tile would start within 7 samples of the frame edge is
        // illegal. The last tile's start is `(count - 1) * size` superblocks of 64 samples.
        let cropped_ok =
            |frame: usize, sb: usize, size: usize| (count(sb, size) - 1) * size * 64 + 8 <= frame;
        let cols_cropped_ok = |c: usize| cropped_ok(frame_width, sb_cols, width_sb(c));
        let rows_cropped_ok = |r: usize| cropped_ok(frame_height, sb_rows, height_sb(r));
        let (max_tiles, max_tile_cols) = level_tile_limits(seq_level_idx);

        let min_cols_log2 = tile_log2(MAX_TILE_WIDTH_SB, sb_cols);
        let max_cols_log2 = tile_log2(1, sb_cols.min(MAX_TILE_COLS_ROWS));
        let max_rows_log2 = tile_log2(1, sb_rows.min(MAX_TILE_COLS_ROWS));
        let min_log2_tiles = min_cols_log2.max(tile_log2(MAX_TILE_AREA_SB, sb_rows * sb_cols));

        // Each search below walks a range of candidate log2 values and takes the first that fits,
        // so its length is fixed by the range rather than by a counter a mutant could stall. Every
        // range ends at a value that always fits (one column, one row, or the syntax maximum), so
        // the fallback is that value. (One column or row fails its check only where no split can
        // pass it: a frame under 8 samples, or a one-superblock frame under superres.)
        // Under superres a non-rightmost tile is at least 128 coded samples, two superblocks, wide.
        let narrowest_sb = if superres_coded_denom.is_some() { 2 } else { 1 };
        let requested_cols = usize::from(request.cols_log2).min(max_cols_log2);
        let cols = (0..=requested_cols)
            .rev()
            .find(|&c| {
                count(sb_cols, width_sb(c)) <= max_tile_cols
                    && width_sb(c) >= narrowest_sb
                    && cols_cropped_ok(c)
            })
            .unwrap_or(0);
        // The fewest columns this frame may take: the syntax's own minimum, raised under superres
        // so that no tile's upscaled width, `TileWidth * SuperresDenom / 8`, passes 4096. The
        // header still codes the loop from the syntax minimum; this one only bounds the choice.
        let mut fewest_cols_log2 = min_cols_log2;
        if let Some(cd) = superres_coded_denom {
            let widest_sb = (MAX_TILE_WIDTH_SB * 8) / (usize::from(cd) + 9);
            fewest_cols_log2 = fewest_cols_log2.max(tile_log2(widest_sb, sb_cols));
        }
        // Raising `cols` to this floor never breaks the 8-sample rule above. A last tile under 8
        // samples is a lone partial superblock after `n - 1` uniform tiles of `size`, which needs
        // `size <= 1 << cols_log2`; a floor above zero leaves tiles over 16 superblocks wide
        // (`widest_sb >= 32`), and every level caps the width at 16384 samples (256 superblocks),
        // so the floor is at most 3 and `1 << cols_log2 <= 8 < size`.
        let cols = cols.max(fewest_cols_log2);

        let min_rows_log2 = min_log2_tiles.saturating_sub(cols);
        let tile_cols = count(sb_cols, width_sb(cols));
        let requested_rows = usize::from(request.rows_log2).min(max_rows_log2);
        let rows = (0..=requested_rows)
            .rev()
            .find(|&r| tile_cols * count(sb_rows, height_sb(r)) <= max_tiles && rows_cropped_ok(r))
            .unwrap_or(0);
        // Uniform spacing rounds each tile up, so a layout at the syntax minimum can still hold a
        // tile over `MAX_TILE_AREA`; take more rows until none does.
        // §5.9.15 lets `minLog2TileRows` exceed `maxLog2TileRows` (the climb is then empty), so
        // the floor is kept even past the maximum.
        // Neither raise needs the 8-sample check: a sub-8-sample last row needs tiles at most
        // `1 << rows_log2` superblocks tall, but a row count raised for `minLog2Tiles` or for
        // `MAX_TILE_AREA` (tiles at most 64 superblocks wide) leaves tiles over 9 superblocks tall
        // at `rows_log2 <= 2`, within every level's 8704 superblocks and 136 superblock rows.
        let floor = rows.max(min_rows_log2);
        let rows = (floor..=max_rows_log2)
            .find(|&r| width_sb(cols) * height_sb(r) <= MAX_TILE_AREA_SB)
            .unwrap_or(max_rows_log2.max(floor));

        Self {
            cols_log2: cols,
            rows_log2: rows,
            min_cols_log2,
            max_cols_log2,
            min_rows_log2,
            max_rows_log2,
            col_starts: uniform_starts(sb_cols, cols, mi_cols),
            row_starts: uniform_starts(sb_rows, rows, mi_rows),
        }
    }

    /// `NumTiles = TileCols * TileRows`.
    pub(crate) fn num_tiles(&self) -> usize {
        (self.col_starts.len() - 1) * (self.row_starts.len() - 1)
    }
}

/// Number of bits needed to hold `value - 1` (the `frame_width/height_bits` field), minimum 1.
fn dimension_bits(value: u32) -> u32 {
    (32 - value.saturating_sub(1).leading_zeros()).max(1)
}

/// `tile_log2(blkSize, target)` (AV1 §5.9.16): smallest `k` with `blkSize << k >= target`.
///
/// `blkSize << k >= target` is `1 << k >= ceil(target / blkSize)`, so `k` is the exponent of the
/// next power of two at or above that quotient — computed directly rather than by the spec's
/// counting loop, which a mutated shift or comparison would turn into one that never ends.
fn tile_log2(blk_size: usize, target: usize) -> usize {
    target
        .div_ceil(blk_size)
        .next_power_of_two()
        .trailing_zeros() as usize
}

/// Appends an OBU (header byte, `obu_has_size_field = 1`, LEB128 size, payload) to `out`.
pub(crate) fn write_obu(out: &mut Vec<u8>, obu_type: u8, payload: &[u8]) {
    // obu_forbidden_bit=0, obu_type, obu_extension_flag=0, obu_has_size_field=1, reserved=0. The
    // type occupies bits 6..3 and the flag is bit 1, so `+` equals `|`/`^` here but, unlike them,
    // leaves no equivalent bit-op mutant (a dropped/altered flag changes the byte).
    out.push((obu_type << 3) + 0b10);
    write_leb128(out, payload.len() as u64);
    out.extend_from_slice(payload);
}

/// Builds the sequence-header OBU payload (reduced still picture; 8-, 10- or 12-bit, with the
/// §6.4.1 profile the depth and plane count require — 1 at 4:4:4, 0 for 8/10-bit monochrome, 2 at
/// 12 bits), terminated with `trailing_bits` (AV1 §5.2, §5.3.4).
/// `lossy` enables `enable_filter_intra` (recursive filter-intra is used only on the lossy path;
/// the lossless path stays DC-only and emits no `use_filter_intra` symbols).
pub(crate) fn sequence_header_payload(
    cfg: &Av1StillConfig,
    width: u32,
    height: u32,
    lossy: bool,
    superres: bool,
) -> Vec<u8> {
    let mut w = BitWriter::new();
    w.put_bits(u32::from(cfg.seq_profile), 3); // seq_profile
    w.put_bit(1); // still_picture
    w.put_bit(1); // reduced_still_picture_header
    w.put_bits(u32::from(cfg.seq_level_idx_0), 5); // seq_level_idx[0]

    let wbits = dimension_bits(width);
    let hbits = dimension_bits(height);
    w.put_bits(wbits - 1, 4); // frame_width_bits_minus_1
    w.put_bits(hbits - 1, 4); // frame_height_bits_minus_1
    w.put_bits(width - 1, wbits); // max_frame_width_minus_1
    w.put_bits(height - 1, hbits); // max_frame_height_minus_1
    // frame_id_numbers_present_flag = 0 (reduced)
    w.put_bit(0); // use_128x128_superblock = 0
    // Filter-intra (recursive DC-prediction, §7.11.2.3) is used only on the lossy path; the
    // lossless path stays DC-only and emits no `use_filter_intra` symbols.
    w.put_bit(u8::from(lossy)); // enable_filter_intra
    w.put_bit(0); // enable_intra_edge_filter = 0
    w.put_bit(u8::from(superres)); // enable_superres
    // CDEF (§7.15) is used only on the lossy path; the lossless path is CodedLossless (CDEF off).
    w.put_bit(u8::from(lossy)); // enable_cdef
    w.put_bit(u8::from(lossy)); // enable_restoration (1 on the lossy path: luma Wiener)

    // color_config() (§5.5.2): `high_bitdepth`, then `twelve_bit` only under `seq_profile == 2`
    // (elsewhere the depth is 8 or 10 and the flag alone decides); `mono_chrome` is coded whenever
    // `seq_profile != 1` (profile 1 infers 0); color_description_present_flag=1; cp/tc/mc; then one
    // of three tails — the monochrome branch, the sRGB shortcut, or an explicit color_range.
    w.put_bit(u8::from(cfg.high_bitdepth));
    if cfg.seq_profile == 2 && cfg.high_bitdepth {
        w.put_bit(u8::from(cfg.twelve_bit));
    }
    if cfg.seq_profile != 1 {
        // Coded for every profile except High — so it appears as soon as the stream is 4:2:0
        // (profile 0) or 4:2:2 (profile 2). The **bit** is not optional: omitting it shifts every
        // field after it and libaom misparses the header.
        w.put_bit(u8::from(cfg.monochrome)); // mono_chrome
    }
    w.put_bit(1); // color_description_present_flag
    w.put_bits(u32::from(cfg.color_primaries), 8);
    w.put_bits(u32::from(cfg.transfer_characteristics), 8);
    w.put_bits(u32::from(cfg.matrix_coefficients), 8);
    if cfg.monochrome {
        // §5.5.2's monochrome branch codes `color_range` and then **returns**: `subsampling_x` and
        // `subsampling_y` are inferred 1, `chroma_sample_position` CSP_UNKNOWN and
        // `separate_uv_delta_q` 0, so none of them is coded. It precedes the sRGB shortcut, so a
        // monochrome stream never takes it — which is consistent, since §6.4.2 forbids MC_IDENTITY
        // once subsampling is 1 and `encode_with` rejects that combination up front.
        w.put_bit(u8::from(cfg.full_range)); // color_range
    } else {
        // §5.5.2: `cp == CP_BT_709 && tc == TC_SRGB && mc == MC_IDENTITY` infers full range and
        // 4:4:4 and codes **no** further bits. Outside that shortcut `color_range` is coded
        // explicitly, and the subsampling is *inferred from `seq_profile`* rather than coded —
        // 0 ⇒ 4:2:0, 1 ⇒ 4:4:4, 2 ⇒ 4:2:2 — with one exception: profile 2 at **12 bits** codes the
        // pair, because that is the only configuration in which Professional is not fixed at
        // 4:2:2. Either way, only a 4:2:0 result carries `chroma_sample_position`.
        if !cfg.is_srgb_shortcut() {
            w.put_bit(u8::from(cfg.full_range)); // color_range
            // Profiles 0 and 1 infer their subsampling (4:2:0 and 4:4:4). Profile 2 codes
            // `subsampling_x` only at 12 bits — at 8/10 it is the fixed 4:2:2 pair — and
            // `subsampling_y` only when `subsampling_x` is 1, which 4:4:4 never is.
            if codes_subsampling(cfg) {
                w.put_bit(cfg.chroma_subsampling_x);
                if cfg.chroma_subsampling_x == 1 {
                    w.put_bit(cfg.chroma_subsampling_y);
                }
            }
            // Whether the pair was coded just now or inferred from the profile, it is the pair
            // that decides this: only 4:2:0 carries a sample position.
            if cfg.chroma_subsampling_x == 1 && cfg.chroma_subsampling_y == 1 {
                w.put_bits(u32::from(cfg.chroma_sample_position), 2);
            }
        }
        w.put_bit(0); // separate_uv_delta_q
    }

    w.put_bit(0); // film_grain_params_present

    // trailing_bits: a 1 then zero-pad to byte alignment.
    w.put_bit(1);
    w.byte_align();
    w.into_bytes()
}

/// Writes `tile_info()` (§5.9.15) for `layout` under `uniform_tile_spacing_flag = 1`.
///
/// Each log2 is coded as a unary climb from its minimum: one `1` per increment, then a terminating
/// `0` unless the climb reached the maximum, where the decoder stops reading on its own. The loops
/// run over the counts they code rather than testing a flag inside a `while`, so no comparison here
/// can be mutated into a loop that never ends.
fn write_tile_info(w: &mut BitWriter, layout: &TileLayout) {
    w.put_bit(1); // uniform_tile_spacing_flag
    for _ in layout.min_cols_log2..layout.cols_log2 {
        w.put_bit(1); // increment_tile_cols_log2
    }
    if layout.cols_log2 < layout.max_cols_log2 {
        w.put_bit(0); // increment_tile_cols_log2: stop
    }
    for _ in layout.min_rows_log2..layout.rows_log2 {
        w.put_bit(1); // increment_tile_rows_log2
    }
    if layout.rows_log2 < layout.max_rows_log2 {
        w.put_bit(0); // increment_tile_rows_log2: stop
    }
    let tile_bits = (layout.cols_log2 + layout.rows_log2) as u32;
    if tile_bits > 0 {
        // context_update_tile_id (tile 0: frame-end CDF update is off, so any tile would do), then
        // tile_size_bytes_minus_1.
        w.put_bits(0, tile_bits);
        w.put_bits(TILE_SIZE_BYTES as u32 - 1, 2);
    }
}

/// Builds the uncompressed frame header for the intra keyframe (AV1 §5.9.2), byte-aligned
/// (`byte_alignment`, no trailing one — the tile data follows in the same `OBU_FRAME`).
///
/// `base_q_idx == 0` selects the lossless path (`CodedLossless`, forced `TX_4X4` WHT, in-loop
/// filters/`tx_mode` emit nothing). `base_q_idx > 0` selects the lossy path: `TX_MODE_SELECT`
/// (per-block `tx_depth`), in-loop filters present but disabled (all levels 0). All quantizer
/// deltas are 0 and `using_qmatrix = 0`.
///
/// The returned bytes precede the tile (symbol-coded) data; together they form the frame OBU
/// payload (AV1 §5.10).
pub(crate) fn frame_header_payload(
    layout: &TileLayout,
    base_q_idx: u8,
    superres_coded_denom: Option<u8>,
    monochrome: bool,
) -> Vec<u8> {
    let lossless = base_q_idx == 0;
    let mut w = BitWriter::new();
    // reduced_still_picture_header ⇒ KEY_FRAME, show_frame=1, FrameIsIntra=1 (no bits).
    // disable_cdf_update = 0: every symbol adapts its CDF as it is coded (§8.2.6). The tile encoder
    // performs the identical update, so encoder and decoder track the same probabilities.
    w.put_bit(0); // disable_cdf_update
    // allow_screen_content_tools = 1 (palette mode is available; intra-block-copy is left off).
    w.put_bit(u8::from(!lossless)); // allow_screen_content_tools (1 on the lossy path)
    if !lossless {
        // seq_force_integer_mv == SELECT_INTEGER_MV (reduced still picture) ⇒ force_integer_mv is
        // coded when screen-content tools are on. The value is irrelevant (an intra frame overrides
        // it to 1), so 0 is emitted.
        w.put_bit(0); // force_integer_mv
    }
    // frame_size_override_flag=0 (reduced_still_picture_header); order_hint f(0); primary_ref_frame
    // inferred; refresh_frame_flags inferred — all no bits.
    // frame_size(): no override ⇒ UpscaledWidth from the sequence header. superres_params (§5.9.8) is
    // present only when enable_superres; then FrameWidth = downscaled and the reconstruction is
    // upscaled back to UpscaledWidth.
    if let Some(cd) = superres_coded_denom {
        w.put_bit(1); // use_superres
        w.put_bits(u32::from(cd), 3); // coded_denom (SuperresDenom = coded_denom + 9)
    }
    // render_size():
    w.put_bit(0); // render_and_frame_size_different = 0
    if !lossless && superres_coded_denom.is_none() {
        // allow_intrabc is coded only when UpscaledWidth == FrameWidth (i.e. no superres).
        w.put_bit(0); // allow_intrabc = 0
    }
    // disable_frame_end_update_cdf: `reduced_still_picture_header || disable_cdf_update` ⇒ inferred
    // 1, so no bit is coded even now that `disable_cdf_update` is 0, and `frame_end_update_cdf`
    // (§7.7) is never invoked. A still image has no later frame to load a saved context anyway.

    // tile_info() (§5.9.15): uniform spacing at the layout `FrameEncoder::encode` codes the tiles in.
    write_tile_info(&mut w, layout);

    // quantization_params().
    w.put_bits(u32::from(base_q_idx), 8); // base_q_idx
    w.put_bit(0); // DeltaQYDc: delta_coded = 0
    // §5.9.12 codes the chroma deltas only when `NumPlanes > 1`; `separate_uv_delta_q` is 0, so
    // `diff_uv_delta` is not coded and V reuses U.
    if !monochrome {
        w.put_bit(0); // DeltaQUDc: delta_coded = 0
        w.put_bit(0); // DeltaQUAc: delta_coded = 0
    }
    w.put_bit(0); // using_qmatrix = 0

    // segmentation_params(): on the lossy path, enable per-segment alternate quantizers
    // (SEG_LVL_ALT_Q) on a couple of segments so blocks can carry their own quantizer.
    if lossless {
        w.put_bit(0); // segmentation_enabled = 0
    } else {
        w.put_bit(1); // segmentation_enabled = 1
        // primary_ref_frame == PRIMARY_REF_NONE ⇒ update_map/temporal/data are inferred (no bits).
        // feature loop: MAX_SEGMENTS (8) × SEG_LVL_MAX (8). Only SEG_LVL_ALT_Q (feature 0) is used.
        for seg in 0..8usize {
            for feat in 0..8usize {
                if let (0, Some(delta)) = (feat, crate::tile::SEG_ALT_Q[seg]) {
                    w.put_bit(1); // feature_enabled = 1
                    // feature_value su(1 + Segmentation_Feature_Bits[ALT_Q]=8) = su(9), signed.
                    w.put_bits((delta & 0x1FF) as u32, 9);
                    continue;
                }
                w.put_bit(0); // feature_enabled = 0
            }
        }
    }

    if !lossless {
        // delta_q_params(): base_q_idx > 0 ⇒ delta_q_present = 1, delta_q_res = 0 (deltas in qindex
        // units). delta_lf_params(): delta_lf_present = 0 (the loop-filter level stays frame-level).
        w.put_bit(1); // delta_q_present = 1
        w.put_bits(0, 2); // delta_q_res = 0
        // delta_lf_params(): per-superblock loop-filter-level deltas (single, not multi).
        w.put_bit(1); // delta_lf_present = 1
        w.put_bits(0, 2); // delta_lf_res = 0
        // `delta_lf_multi = 0` is also what keeps `read_delta_lf` (§5.11.13) plane-count agnostic:
        // it sets `frameLfCount = delta_lf_multi ? (NumPlanes > 1 ? FRAME_LF_COUNT
        // : FRAME_LF_COUNT - 2) : 1`, so exactly one delta is coded whatever `NumPlanes` is. Turning
        // multi on would make the count monochrome-dependent, and `signal_delta_lf` would have to
        // follow.
        w.put_bit(0); // delta_lf_multi = 0
        // loop_filter_params(): a single deblock level (the same for both luma passes and both
        // chroma planes), scaled from base_q_idx. level 0 ⇒ deblock disabled and level[2]/[3] omitted.
        let lf = u32::from(crate::filter::deblock_level(base_q_idx));
        w.put_bits(lf, 6); // loop_filter_level[0]
        w.put_bits(lf, 6); // loop_filter_level[1]
        // §5.9.11 gates the chroma levels on `NumPlanes > 1` as well as on a non-zero luma level.
        if lf != 0 && !monochrome {
            w.put_bits(lf, 6); // loop_filter_level[2] (U)
            w.put_bits(lf, 6); // loop_filter_level[3] (V)
        }
        w.put_bits(0, 3); // loop_filter_sharpness
        w.put_bit(0); // loop_filter_delta_enabled = 0
        // cdef_params(): enable_cdef = 1 ⇒ CdefDamping = 3, cdef_bits = 0 (one strength set applied
        // everywhere; cdef_idx is then L(0) = 0 bits in the tile). The secondary strength is signaled
        // pre-mapping (the decoder maps 3 → 4), so a stored 4 is emitted as 3.
        let (y_pri, y_sec, uv_pri, uv_sec) = crate::filter::cdef_strengths(base_q_idx);
        let sec_code = |s: i32| -> u32 { if s == 4 { 3 } else { s as u32 } };
        w.put_bits(0, 2); // cdef_damping_minus_3 (CdefDamping = 3)
        w.put_bits(0, 2); // cdef_bits = 0
        w.put_bits(y_pri as u32, 4); // cdef_y_pri_strength[0]
        w.put_bits(sec_code(y_sec), 2); // cdef_y_sec_strength[0]
        if !monochrome {
            w.put_bits(uv_pri as u32, 4); // cdef_uv_pri_strength[0]
            w.put_bits(sec_code(uv_sec), 2); // cdef_uv_sec_strength[0]
        }
        // lr_params() (§5.9.20): luma RESTORE_WIENER, chroma RESTORE_NONE. `lr_type` is 2 bits per
        // plane (`Remap_Lr_Type`: 0=NONE, 2=WIENER); only luma uses restoration.
        // §5.9.20 loops `i < NumPlanes`, so a monochrome frame codes one `lr_type`, not three.
        w.put_bits(2, 2); // FrameRestorationType[0] = RESTORE_WIENER
        if !monochrome {
            w.put_bits(0, 2); // FrameRestorationType[1] = RESTORE_NONE
            w.put_bits(0, 2); // FrameRestorationType[2] = RESTORE_NONE
        }
        // usesLr ⇒ lr_unit_shift. Not a 128×128 superblock ⇒ f(1) then (if set) lr_unit_extra f(1).
        // shift = 2 ⇒ LoopRestorationSize = 256. usesChromaLr = 0 ⇒ no lr_uv_shift.
        w.put_bit(1); // lr_unit_shift bit 0
        w.put_bit(1); // lr_unit_extra ⇒ lr_unit_shift = 2
        w.put_bit(1); // read_tx_mode: tx_mode_select = 1 ⇒ TX_MODE_SELECT (per-block tx_depth)
        // frame_reference_mode / skip_mode_params (intra) ⇒ no bits. allow_warped_motion = 0.
    }
    // CodedLossless ⇒ loop_filter / cdef / lr / tx_mode emit nothing (TxMode = ONLY_4X4).
    w.put_bit(1); // reduced_tx_set = 1
    // global_motion_params / film_grain_params (intra, not present) ⇒ no bits.

    w.byte_align(); // byte_alignment after frame_header_obu (§5.10)
    // tile_group_obu prefix (§5.11.1): for more than one tile, tile_start_and_end_present_flag = 0,
    // then re-align so the tile_size fields / tile data begin on a byte boundary.
    if layout.num_tiles() > 1 {
        w.put_bit(0); // tile_start_and_end_present_flag
        w.byte_align();
    }
    w.into_bytes()
}

/// Wraps the sequence-header and frame OBUs into the temporal unit placed in `mdat`.
pub(crate) fn assemble_temporal_unit(seq_payload: &[u8], frame_payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    write_obu(&mut out, OBU_SEQUENCE_HEADER, seq_payload);
    write_obu(&mut out, OBU_FRAME, frame_payload);
    out
}

#[cfg(test)]
mod tests {

    #[test]
    fn only_twelve_bit_profile_2_codes_its_subsampling() {
        // §5.5.2: profile 0 infers 4:2:0 and profile 1 infers 4:4:4, so neither codes a bit;
        // profile 2 codes `subsampling_x` only at 12 bits, because below that it is the fixed
        // 4:2:2 pair. Both halves are now independently reachable — profile 2 is how 8/10-bit
        // 4:2:2 is coded as well as how any 12-bit stream is — so each is pinned on its own.
        let cfg = |seq_profile: u8, high_bitdepth: bool, twelve_bit: bool| Av1StillConfig {
            seq_profile,
            seq_level_idx_0: 0,
            seq_tier_0: 0,
            high_bitdepth,
            twelve_bit,
            monochrome: false,
            chroma_subsampling_x: 0,
            chroma_subsampling_y: 0,
            chroma_sample_position: 0,
            color_primaries: 1,
            transfer_characteristics: 13,
            matrix_coefficients: 0,
            full_range: true,
        };
        assert!(codes_subsampling(&cfg(2, true, true)), "profile 2, 12-bit");
        // The case that makes the depth half load-bearing: 8-bit 4:2:2 is profile 2 and must NOT
        // code the pair. Keying on the profile alone would emit a bit no decoder expects here.
        assert!(
            !codes_subsampling(&cfg(2, false, false)),
            "profile 2, 8-bit"
        );
        assert!(
            !codes_subsampling(&cfg(2, true, false)),
            "profile 2, 10-bit"
        );
        assert!(!codes_subsampling(&cfg(1, false, false)), "profile 1");
        // A 12-bit config on another profile is not one the encoder builds, but the predicate must
        // still key on both halves rather than on the depth alone.
        assert!(!codes_subsampling(&cfg(0, true, true)), "profile 0");
        // `twelve_bit` without `high_bitdepth` is incoherent, and §5.5.2 derives `BitDepth == 12`
        // from all three fields. The writer above emits no `twelve_bit` bit for such a config, so
        // reading `twelve_bit` alone here would code a `subsampling_x` the decoder is not
        // expecting and shift every field after it.
        assert!(
            !codes_subsampling(&cfg(2, false, true)),
            "twelve_bit without high_bitdepth codes no depth bit, so it must code no pair"
        );
    }

    use super::*;

    /// A config in `chroma` with a non-identity matrix (so `color_config()` leaves the sRGB
    /// shortcut and actually codes the range and position bits).
    fn cfg_for(seq_profile: u8, ss_x: u8, ss_y: u8, csp: u8) -> Av1StillConfig {
        Av1StillConfig {
            seq_profile,
            seq_level_idx_0: 0,
            seq_tier_0: 0,
            high_bitdepth: false,
            twelve_bit: false,
            monochrome: false,
            chroma_subsampling_x: ss_x,
            chroma_subsampling_y: ss_y,
            chroma_sample_position: csp,
            color_primaries: 1,
            transfer_characteristics: 13,
            matrix_coefficients: 1, // BT.709 — not the identity, so no shortcut
            full_range: true,
        }
    }

    #[test]
    fn chroma_sample_position_is_coded_only_for_four_two_zero() {
        // §5.5.2 gates the 2-bit field on `subsampling_x && subsampling_y`. Changing the position
        // must therefore change a 4:2:0 header and leave 4:2:2 and 4:4:4 untouched — an `||` in
        // that condition would emit the field for 4:2:2 too, shifting every later bit.
        let p420_a = sequence_header_payload(&cfg_for(0, 1, 1, 0), 64, 64, true, false);
        let p420_b = sequence_header_payload(&cfg_for(0, 1, 1, 3), 64, 64, true, false);
        assert_ne!(p420_a, p420_b, "4:2:0 codes chroma_sample_position");

        let p422_a = sequence_header_payload(&cfg_for(2, 1, 0, 0), 64, 64, true, false);
        let p422_b = sequence_header_payload(&cfg_for(2, 1, 0, 3), 64, 64, true, false);
        assert_eq!(p422_a, p422_b, "4:2:2 codes no chroma_sample_position");

        let p444_a = sequence_header_payload(&cfg_for(1, 0, 0, 0), 64, 64, true, false);
        let p444_b = sequence_header_payload(&cfg_for(1, 0, 0, 3), 64, 64, true, false);
        assert_eq!(p444_a, p444_b, "4:4:4 codes no chroma_sample_position");
    }

    #[test]
    fn mono_chrome_bit_is_coded_for_every_profile_but_high() {
        // §5.5.2 codes `mono_chrome` unless `seq_profile == 1`. The bit is always 0 here, but its
        // *presence* changes the payload length class: a 4:2:0 header carries one more coded bit
        // than the 4:4:4 one before the colour description, plus the 2-bit sample position.
        let p444 = sequence_header_payload(&cfg_for(1, 0, 0, 0), 64, 64, true, false);
        let p420 = sequence_header_payload(&cfg_for(0, 1, 1, 0), 64, 64, true, false);
        let p422 = sequence_header_payload(&cfg_for(2, 1, 0, 0), 64, 64, true, false);
        // 4:2:2 differs from 4:4:4 by exactly the mono_chrome bit (and the profile field itself),
        // and 4:2:0 differs from 4:2:2 by the two position bits.
        assert_ne!(p444, p422);
        assert_ne!(p422, p420);
    }

    #[test]
    fn level_selection() {
        assert_eq!(pick_level(16, 16).unwrap(), 0);
        // 1920x1080 = 2_073_600 px exceeds 3.x MaxPicSize, so level 4.0 (idx 8) is smallest.
        assert_eq!(pick_level(1920, 1080).unwrap(), 8);
        assert!(pick_level(100_000, 100_000).is_err());
    }

    #[test]
    fn dimension_bits_min_one() {
        assert_eq!(dimension_bits(1), 1); // W-1 = 0
        assert_eq!(dimension_bits(16), 4); // 15 -> 4 bits
        assert_eq!(dimension_bits(17), 5); // 16 -> 5 bits
        assert_eq!(dimension_bits(256), 8); // 255 -> 8 bits
        assert_eq!(dimension_bits(257), 9); // 256 -> 9 bits
    }

    /// Luma samples per 64×64 superblock side.
    const SB: usize = 64;

    /// `TileLayout::resolve` over a frame of `sb_cols × sb_rows` whole superblocks.
    fn layout(req: Tiling, sb_cols: usize, sb_rows: usize, level: u8) -> TileLayout {
        TileLayout::resolve(req, sb_cols * SB, sb_rows * SB, level, None)
    }

    #[test]
    fn tile_log2_is_the_smallest_covering_exponent() {
        // §5.9.16: the smallest `k` with `blk << k >= target`, at, just past and just short of
        // each power-of-two boundary.
        for (blk, target, want) in [
            (1, 1, 0),
            (1, 2, 1),
            (1, 3, 2),
            (1, 4, 2),
            (1, 5, 3),
            (64, 64, 0),
            (64, 65, 1),
            (64, 256, 2),
            (2304, 4590, 1),
            (2304, 4609, 2),
        ] {
            assert_eq!(tile_log2(blk, target), want, "tile_log2({blk}, {target})");
        }
    }

    #[test]
    fn level_tile_limits_follow_the_annex_a_table() {
        // One assertion per major level, at both ends of its minor range.
        for (idx, want) in [
            (0, (8, 4)),
            (3, (8, 4)),
            (4, (16, 6)),
            (7, (16, 6)),
            (8, (32, 8)),
            (11, (32, 8)),
            (12, (64, 8)),
            (15, (64, 8)),
            (16, (128, 16)),
            (19, (128, 16)),
        ] {
            assert_eq!(level_tile_limits(idx), want, "seq_level_idx {idx}");
        }
    }

    #[test]
    fn the_default_tiling_is_the_legacy_two_column_layout() {
        // Before the option existed the encoder coded two tile columns once the frame was two
        // superblocks wide, one tile otherwise, and always one tile row.
        let one = layout(Tiling::default(), 1, 3, 0);
        assert_eq!((one.cols_log2, one.rows_log2), (0, 0));
        assert_eq!(one.col_starts, vec![0, 16]);
        let two = layout(Tiling::default(), 2, 3, 0);
        assert_eq!((two.cols_log2, two.rows_log2), (1, 0));
        assert_eq!(two.col_starts, vec![0, 16, 32]);
        assert_eq!(two.row_starts, vec![0, 48]);
        // An odd superblock count rounds the first tile up: 3 → 2 + 1.
        assert_eq!(
            layout(Tiling::default(), 3, 1, 0).col_starts,
            vec![0, 32, 48]
        );
        assert_eq!(Tiling::default(), Tiling::new(1, 0));
        assert_eq!(Tiling::SINGLE, Tiling::new(0, 0));
    }

    #[test]
    fn uniform_spacing_can_code_fewer_tiles_than_requested() {
        // §5.9.15: 5 superblock rows at log2 2 are `ceil(5 / 4) = 2` rows high, which is 3 tile
        // rows, not 4. The frame edge ends the last one wherever it falls.
        let l = TileLayout::resolve(Tiling::new(0, 2), SB, 5 * SB - 8, 4, None);
        assert_eq!(l.rows_log2, 2);
        assert_eq!(l.row_starts, vec![0, 32, 64, 78]);
        assert_eq!(l.num_tiles(), 3);
    }

    #[test]
    fn no_tile_column_starts_within_8_samples_of_the_right_edge() {
        // Annex A.3: CroppedTileWidth = FrameWidth - MiColStart * 4 >= 8 for each tile. Two
        // superblock columns split at 64, so a 71-wide frame leaves the second tile 7 samples;
        // three split 2 + 1 at 128, so 135 leaves 7. One more sample makes each split legal.
        let cols = |w: usize, req: Tiling| TileLayout::resolve(req, w, SB, 0, None);
        for (w, want) in [(71, 0), (72, 1), (135, 0), (136, 1)] {
            assert_eq!(cols(w, Tiling::default()).cols_log2, want, "width {w}");
        }
        // The request drops to the nearest legal split, not to one tile: five superblocks at
        // log2 2 split 2 + 2 + 1 at 256 (4 samples left of a 260-wide frame), at log2 1 3 + 2.
        let l = cols(260, Tiling::new(2, 0));
        assert_eq!(l.cols_log2, 1);
        assert_eq!(l.col_starts, vec![0, 48, 66]);
    }

    #[test]
    fn no_tile_row_starts_within_8_samples_of_the_bottom_edge() {
        // The same rule down the frame: 129 rows split at 128 leave a 1-sample tile row.
        let rows = |h: usize| TileLayout::resolve(Tiling::new(0, 1), SB, h, 0, None);
        assert_eq!(rows(129).rows_log2, 0);
        assert_eq!(rows(129).row_starts, vec![0, 34]);
        assert_eq!(rows(136).rows_log2, 1);
        assert_eq!(rows(136).row_starts, vec![0, 32, 34]);
    }

    #[test]
    fn a_request_past_the_frame_is_clamped_to_one_tile_per_superblock() {
        // `maxLog2TileCols = tile_log2(1, sbCols)`: 3 superblocks allow log2 2 at most.
        let l = layout(Tiling::new(6, 6), 3, 2, 4);
        assert_eq!((l.cols_log2, l.rows_log2), (2, 1));
        assert_eq!(l.col_starts, vec![0, 16, 32, 48]);
        assert_eq!(l.num_tiles(), 6);
    }

    #[test]
    fn a_frame_wider_than_max_tile_width_takes_at_least_two_columns() {
        // 65 superblocks are 4160 samples, past MAX_TILE_WIDTH: `minLog2TileCols = 1` even when
        // one tile is asked for.
        let l = layout(Tiling::SINGLE, 65, 1, 16);
        assert_eq!(l.cols_log2, 1);
        assert_eq!(l.min_cols_log2, 1);
        assert_eq!(l.col_starts, vec![0, 33 * 16, 65 * 16]);
    }

    #[test]
    fn the_level_caps_tile_columns_and_tiles() {
        // Level 2.0: MaxTileCols 4, MaxTiles 8. 30 superblocks asked for 64 columns take 4
        // (log2 2, 8 superblocks each); 18 rows asked for 64 then fit 8 / 4 = 2 tile rows.
        let l = layout(Tiling::new(6, 6), 30, 18, 0);
        assert_eq!((l.cols_log2, l.rows_log2), (2, 1));
        assert_eq!(l.num_tiles(), 8);
        // Level 4.0 allows 8 columns and 32 tiles on the same grid.
        let l = layout(Tiling::new(6, 6), 30, 18, 8);
        assert_eq!((l.cols_log2, l.rows_log2), (3, 2));
        assert_eq!(l.num_tiles(), 32);
    }

    #[test]
    fn a_tile_rounded_past_max_tile_area_takes_another_row() {
        // 34 × 135 superblocks (2176 × 8640) need `minLog2Tiles = 1`, but one row split leaves a
        // 34 × 68 = 2312-superblock tile, over MAX_TILE_AREA's 2304; the next row split fits.
        let l = layout(Tiling::SINGLE, 34, 135, 16);
        assert_eq!(l.min_rows_log2, 1);
        assert_eq!((l.cols_log2, l.rows_log2), (0, 2));
        // The bound is inclusive: a 48 × 48 = 2304-superblock tile is exactly MAX_TILE_AREA.
        let l = layout(Tiling::SINGLE, 48, 48, 16);
        assert_eq!((l.cols_log2, l.rows_log2), (0, 0));
    }

    #[test]
    fn the_tile_count_beyond_one_tile_row_is_capped_by_max_tiles() {
        // Level 2.0 (MaxTiles 8) with two columns: 4 rows fit, 8 do not.
        let l = layout(Tiling::new(1, 3), 2, 18, 0);
        assert_eq!((l.cols_log2, l.rows_log2), (1, 2));
    }

    #[test]
    fn superres_keeps_every_non_rightmost_tile_two_superblocks_wide() {
        // Annex A.3: under superres a tile that is not the rightmost is at least 128 samples wide.
        // Two superblocks would split into two 64-wide tiles, so they stay one tile; three split
        // 2 + 1, which is legal since only the rightmost is narrow.
        let at = |sb_cols: usize| {
            TileLayout::resolve(Tiling::default(), sb_cols * SB, SB, 0, Some(0)).cols_log2
        };
        assert_eq!(at(2), 0);
        assert_eq!(at(3), 1);
        // The same three superblocks at log2 2 would be 1-wide tiles; the request drops to 1.
        let l = TileLayout::resolve(Tiling::new(2, 0), 3 * SB, SB, 0, Some(0));
        assert_eq!(l.cols_log2, 1);
    }

    #[test]
    fn superres_bounds_each_tile_s_upscaled_width() {
        // At SuperresDenom 16 (coded_denom 7) a coded 64-sample superblock upscales to 128, so a
        // tile holds at most 4096 / 128 = 32 superblocks: 100 of them need four columns.
        let l = TileLayout::resolve(Tiling::SINGLE, 100 * SB, SB, 16, Some(7));
        assert_eq!(l.cols_log2, 2);
        // The syntax minimum is still what the header codes the climb from.
        assert_eq!(l.min_cols_log2, 1);
        // At denom 9 a tile holds 56 superblocks, so two columns of 50 suffice.
        let l = TileLayout::resolve(Tiling::SINGLE, 100 * SB, SB, 16, Some(0));
        assert_eq!(l.cols_log2, 1);
    }

    #[cfg(feature = "decode")]
    #[test]
    fn tile_info_parses_back_to_the_layout_it_was_written_for() {
        // The decoder's `tile_info()` reader derives TileColsLog2/TileRowsLog2 and the tile starts
        // from the coded climb; it must land on the layout the writer was given, including the
        // climbs that start above zero (wide and tall frames) and the ones that reach the maximum
        // and so code no terminating zero.
        use gamut_bitstream::BitReader;

        use crate::decode::{FrameHeader, SequenceHeader};
        for (w, h, req) in [
            (64, 64, Tiling::default()),
            (200, 130, Tiling::new(2, 1)),
            (130, 130, Tiling::new(6, 6)),
            (4160, 64, Tiling::SINGLE),
            (2176, 8640, Tiling::SINGLE),
            (1920, 1080, Tiling::new(3, 2)),
        ] {
            let cfg = crate::decode::testutil::still_config(w, h, Av1Colour::default());
            let seq = SequenceHeader::parse(&sequence_header_payload(&cfg, w, h, true, false))
                .expect("sequence header parses");
            let l = TileLayout::resolve(req, w as usize, h as usize, cfg.seq_level_idx_0, None);
            let frame = frame_header_payload(&l, 100, None, false);
            let fh = FrameHeader::parse(&mut BitReader::new(&frame), &seq).expect("parses");
            let t = &fh.tile_info;
            assert_eq!(
                (t.tile_cols_log2 as usize, t.tile_rows_log2 as usize),
                (l.cols_log2, l.rows_log2),
                "{w}x{h}"
            );
            let starts = |v: &[u32]| v.iter().map(|&s| s as usize).collect::<Vec<_>>();
            assert_eq!(starts(&t.mi_col_starts), l.col_starts, "{w}x{h}");
            assert_eq!(starts(&t.mi_row_starts), l.row_starts, "{w}x{h}");
            // `tile_size_bytes_minus_1` is coded only when there is more than one tile.
            if l.num_tiles() > 1 {
                assert_eq!(t.tile_size_bytes, TILE_SIZE_BYTES, "{w}x{h}");
            }
        }
    }

    #[test]
    fn obu_framing_has_size_field() {
        let mut out = Vec::new();
        write_obu(&mut out, OBU_SEQUENCE_HEADER, &[0xaa, 0xbb]);
        // header byte: type 1 -> (1<<3)|2 = 0x0a; size leb128 = 2; payload.
        assert_eq!(out, vec![0x0a, 0x02, 0xaa, 0xbb]);
    }
}
