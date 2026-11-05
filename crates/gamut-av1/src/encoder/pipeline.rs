//! The pipeline every `encode_still*` entry shares: validate the layout, pick the profile and the
//! level, build the sequence and frame headers, code the planes (the lossless path, or the lossy
//! one with its in-loop filter decisions and restoration second pass) and frame the tiles.

use std::num::NonZeroUsize;

use gamut_color::cicp::{ColorRange, MatrixCoefficients};
use gamut_color::{BitDepth, ChromaSubsampling, Planar8, Planar16};
use gamut_core::{Dimensions, Error, Result};

use super::{Av1EncodeConfig, EncodedStill, ReconImage, StillWithRecon};
use crate::filter::{FrameFilters, LrUnits};
use crate::geom::PlaneGeom;
use crate::headers::{self, Av1Color, Av1StillConfig, TileLayout};
use crate::tile::{BlockVariation, FrameEncoder, Reconstruction, Search};

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

/// The high-bit-depth encode behind [`super::encode_still16_with_recon`], with the block variation
/// as [`encode_with`] takes it.
pub(super) fn encode16_with(
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
pub(super) fn encode_with(
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
        // Superres is a lossy tool here: the lossless path returns the full-width source as its
        // reconstruction, which is not what a decoder upscales from the downscaled coded frame,
        // so `_with_recon` would no longer return the decoded samples.
        if qindex == 0 {
            return Err(Error::unsupported(
                env!("CARGO_PKG_NAME"),
                "AV1: superres is coded on the lossy path only",
            ));
        }
        // Superres over anything but 4:4:4 is refused rather than half-supported: the downscale
        // below is written for three luma-sized planes and relabels its result 4:4:4, so a
        // subsampled buffer's chroma planes would be read at luma dimensions (out of bounds) and a
        // monochrome one would hand `FrameEncoder` a plane count disagreeing with the `monochrome`
        // the frame header was given. Checked *before* `check_layout`, whose matrix rule would
        // otherwise fire first for a superres request left at the default identity colour and
        // report a reason that is not the one that applies.
        if planes.subsampling() != ChromaSubsampling::Cs444 {
            return Err(Error::unsupported(
                env!("CARGO_PKG_NAME"),
                "AV1: superres is implemented for 4:4:4 sources only",
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
        // which only matters once a subsampled source can reach superres — `encode_with` refuses
        // every layout but 4:4:4 under superres.
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
    #[cfg(feature = "decode")]
    use gamut_color::RgbToYcbcr;
    #[cfg(feature = "decode")]
    use gamut_core::{ImageRef, Rgb8};

    use super::*;
    use crate::encoder::tests::{dims, planes, texture};
    #[cfg(feature = "decode")]
    use crate::encoder::{encode_still, encode_still16};
    use crate::headers::Tiling;

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
