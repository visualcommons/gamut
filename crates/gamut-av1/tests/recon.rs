//! Bit-exact reconstruction cross-check for the lossy intra path (P6 keystone).
//!
//! The encoder maintains a reconstruction buffer that must equal, sample for sample, what a
//! conformant decoder produces. Each case encodes a still, then decodes the raw AV1 OBU stream
//! (a Section-5 low-overhead stream — each OBU carries its size) with **two independent reference
//! decoders** and asserts both reproduce the encoder's exported reconstruction byte-for-byte:
//!
//! - **libaom** — the AV1 *reference* codec — is the primary, definitive conformance oracle.
//! - **dav1d** corroborates as an independent second decoder (and is libavif's backend in the
//!   AVIF container cross-check elsewhere). Two independent decoders agreeing is a strictly
//!   stronger signal than either alone.
//!
//! Both are linked in from `third_party/` submodules via the `aom-oracle` / `dav1d-oracle`
//! dev-dependencies, so the check is hermetic and always runs — it never depends on a system
//! decoder binary. Building these tests therefore needs cmake/meson/ninja/nasm and the checked-out
//! submodules (`git submodule update --init --recursive`).

use std::num::NonZeroUsize;

use gamut_av1::{
    Av1Colour, EncodeOptions, Speed, Tiling, encode_still_intra, encode_still_intra_superres,
    encode_still_intra_with, encode_still_intra_with_options, encode_still_intra_with_speed,
    encode_still_intra16_with, encode_still_intra16_with_options, encode_still_intra16_with_speed,
};
use gamut_color::cicp::{ColorRange, ColourPrimaries, MatrixCoefficients, TransferCharacteristics};
use gamut_color::{BitDepth, ChromaSubsampling, Planar8, Planar16, RgbToYcbcr};
use gamut_core::{Dimensions, ImageRef, Rgb8};

/// Builds identity planes (Y=G, U=B, V=R) from an RGB generator.
fn planes(w: u32, h: u32, f: impl Fn(u32, u32) -> [u8; 3]) -> Planar8 {
    let mut rgb = vec![0u8; (w * h * 3) as usize];
    for y in 0..h {
        for x in 0..w {
            let i = ((y * w + x) * 3) as usize;
            rgb[i..i + 3].copy_from_slice(&f(x, y));
        }
    }
    Planar8::from_rgb8_identity(&rgb, w, h).unwrap()
}

/// Builds `Y/Cb/Cr` planes through a real luma-chroma matrix, box-averaging chroma to `ss`.
fn planes_subsampled(
    w: u32,
    h: u32,
    ss: ChromaSubsampling,
    matrix: MatrixCoefficients,
    range: ColorRange,
    f: impl Fn(u32, u32) -> [u8; 3],
) -> Planar8 {
    let mut rgb = vec![0u8; (w * h * 3) as usize];
    for y in 0..h {
        for x in 0..w {
            let i = ((y * w + x) * 3) as usize;
            rgb[i..i + 3].copy_from_slice(&f(x, y));
        }
    }
    let img = ImageRef::<Rgb8>::new(&rgb, Dimensions::new(w, h).unwrap()).unwrap();
    let m = RgbToYcbcr::new(matrix, range, BitDepth::Eight).unwrap();
    Planar8::from_rgb8_matrix_subsampled(img, m, ss).unwrap()
}

/// The colour a subsampled stream must carry: identity is not conformant below 4:4:4 (§6.4.2).
fn colour_for(matrix: MatrixCoefficients, range: ColorRange) -> Av1Colour {
    Av1Colour {
        primaries: ColourPrimaries::Bt709,
        transfer: TransferCharacteristics::Srgb,
        matrix,
        range,
    }
}

/// Encodes `planes` at `qindex`, then decodes the OBU stream with both reference decoders and
/// asserts each reproduces the encoder's reconstruction byte-for-byte.
fn check(planes: &Planar8, qindex: u8) {
    check_with(encode_still_intra(planes, qindex).unwrap(), qindex);
}

/// The in-loop filters a stream signals, as gamut's own header parser reads them back: the
/// encoder chooses each against the source, so a filter's oracle case proves its math only where
/// the encoder actually chose to run it.
#[derive(Clone, Copy, Debug, Default)]
struct Filters {
    /// `loop_filter_level[0]`.
    deblock: u8,
    /// `cdef_{y,uv}_{pri,sec}_strength[0]`, secondary strengths remapped.
    cdef: [u8; 4],
    /// Whether luma loop restoration is `RESTORE_WIENER`.
    wiener: bool,
}

/// [`check`], returning the in-loop filters the stream signals. 8-bit 4:4:4 only: that is the
/// header parser's scope.
fn check_filters(planes: &Planar8, qindex: u8) -> Filters {
    let encoded = encode_still_intra(planes, qindex).unwrap();
    let frame = gamut_av1::Av1Decoder::new()
        .inspect(&encoded.0.obus)
        .unwrap()
        .frame;
    check_with(encoded, qindex);
    let c = &frame.cdef;
    Filters {
        deblock: frame.loop_filter.level[0],
        cdef: [c.y_pri[0], c.y_sec[0], c.uv_pri[0], c.uv_sec[0]],
        wiener: frame.lr.frame_restoration_type[0] == gamut_av1::RestorationType::Wiener,
    }
}

/// The body of [`check`], over an already-encoded still, so colour-parameterized cases share it.
fn check_with(encoded: (gamut_av1::EncodedStill, gamut_av1::ReconImage), qindex: u8) {
    let (still, recon) = encoded;
    let (w, h) = (recon.width as usize, recon.height as usize);

    // A standalone Section-5 OBU stream needs a temporal-delimiter OBU first (AVIF omits it inside
    // the container). TD = obu_type 2, has_size_field, empty payload.
    let mut stream = vec![0x12u8, 0x00];
    stream.extend_from_slice(&still.obus);

    // Assert one decoder's output equals the encoder's reconstruction.
    let assert_match = |decoder: &str, dw: usize, dh: usize, dplanes: &[Vec<u16>]| {
        assert_eq!(
            (dw, dh),
            (w, h),
            "{decoder}: decoded dimensions differ from reconstruction for {w}x{h} q{qindex}"
        );
        // Only the coded planes are compared. The two oracles disagree about how to *present* an
        // absent plane — dav1d returns an empty buffer, libaom materialises a neutral-filled one —
        // so the presentation is checked separately below rather than folded into byte equality.
        let coded = recon.subsampling.num_planes();
        for (p, (dec, enc)) in dplanes.iter().zip(&recon.planes).take(coded).enumerate() {
            if dec != enc && std::env::var("GAMUT_DBG").is_ok() {
                let k = dec
                    .iter()
                    .zip(enc.iter())
                    .position(|(d, e)| d != e)
                    .unwrap();
                eprintln!(
                    "DIFF {decoder} p{p} idx{k} dec={} enc={} [{w}x{h} q{qindex}]",
                    dec[k], enc[k]
                );
            }
            assert_eq!(
                dec, enc,
                "{decoder}: plane {p} mismatch (decoder vs encoder reconstruction) for {w}x{h} q{qindex}"
            );
        }
        // A monochrome stream must carry no chroma *detail*. Asserting emptiness alone would only
        // hold for dav1d; asserting nothing would let a stream that accidentally coded real chroma
        // pass, since the loop above never looks at those planes. Neutral-or-absent is the property
        // both decoders can express, and it is exactly what `mono_chrome = 1` means.
        if coded == 1 {
            let neutral = 1u16 << (recon.bit_depth.bits() - 1);
            for (p, dec) in dplanes.iter().enumerate().skip(1) {
                assert!(
                    dec.iter().all(|&v| v == neutral),
                    "{decoder}: monochrome stream produced chroma detail in plane {p} for {w}x{h} q{qindex}"
                );
            }
        }
    };

    // libaom — the AV1 reference codec — is the primary, definitive conformance oracle.
    let aom = aom_oracle::decode_av1(&stream)
        .unwrap_or_else(|e| panic!("libaom decode failed for {w}x{h} q{qindex}: {e}"));
    assert_match(
        "libaom",
        aom.width as usize,
        aom.height as usize,
        &aom.planes,
    );

    // dav1d corroborates as an independent second decoder.
    let dav1d = dav1d_oracle::decode_obu(&stream)
        .unwrap_or_else(|e| panic!("dav1d decode failed for {w}x{h} q{qindex}: {e}"));
    assert_match(
        "dav1d",
        dav1d.width as usize,
        dav1d.height as usize,
        &dav1d.planes,
    );
}

#[test]
fn lossy_reconstruction_matches_dav1d() {
    // A photographic-ish gradient with texture: exercises non-trivial residuals, DC prediction
    // across block boundaries, and the all-zero (txb_skip) path on flat regions.
    let texture = |x: u32, y: u32| {
        let r = (x.wrapping_mul(3).wrapping_add(y) % 256) as u8;
        let g = ((x + y.wrapping_mul(2)) % 256) as u8;
        let b = (128 + ((x ^ y) % 64)) as u8;
        [r, g, b]
    };
    // qindex stays in 1..=20 so the coefficient-CDF quantizer context is 0 (matches the static CDFs).
    for &q in &[4u8, 12, 20] {
        for &(w, h) in &[(8, 8), (17, 13), (32, 32), (64, 48), (100, 70)] {
            check(&planes(w, h, texture), q);
        }
    }
}

#[test]
fn lossy_reconstruction_matches_dav1d_all_qctx() {
    // The same textured content, but at quantizers spanning every coefficient-CDF quantizer
    // context: qctx 1 (21..=60), qctx 2 (61..=120) and qctx 3 (121..=255). A wrong CDF table makes
    // the arithmetic decode diverge, so dav1d byte-equality is a hard correctness gate per qctx.
    let texture = |x: u32, y: u32| {
        let r = (x.wrapping_mul(5).wrapping_add(y.wrapping_mul(3)) % 256) as u8;
        let g = ((x.wrapping_add(y).wrapping_mul(2)) % 256) as u8;
        let b = (64 + ((x.wrapping_mul(7) ^ y) % 128)) as u8;
        [r, g, b]
    };
    // One representative qindex per context boundary, plus the extremes.
    for &q in &[21u8, 40, 60, 61, 90, 120, 121, 200, 255] {
        for &(w, h) in &[(8, 8), (17, 13), (40, 24), (100, 70)] {
            check(&planes(w, h, texture), q);
        }
    }
}

#[test]
fn tx_type_selection_matches_dav1d() {
    // Content engineered so the encoder's per-block transform-type search picks non-DCT_DCT types
    // from TX_SET_INTRA_2 (IDTX on sharp screen-content edges, ADST on directional ramps). dav1d
    // must decode whatever type was signaled to the encoder's reconstruction byte-for-byte, so this
    // exercises the ADST/IDTX inverse transforms end-to-end through the real decoder.
    let screen = |x: u32, y: u32| {
        // 1-pixel-wide vertical bars + a diagonal ramp: high-frequency, impulse-like residuals.
        let bar = if x.is_multiple_of(2) { 235 } else { 20 } as u8;
        let ramp = ((x.wrapping_add(y)).wrapping_mul(9) % 256) as u8;
        let diag = if (x + y).is_multiple_of(7) { 250 } else { 40 } as u8;
        [bar, ramp, diag]
    };
    for &q in &[10u8, 32, 80, 160] {
        for &(w, h) in &[(8, 8), (16, 16), (37, 21), (64, 40)] {
            check(&planes(w, h, screen), q);
        }
    }
}

#[test]
fn directional_modes_match_dav1d() {
    // Strong vertical, horizontal and diagonal structure so the mode search picks the directional
    // modes (V/H/D135/D113/D157). dav1d must decode each signaled angle to the encoder
    // reconstruction byte-for-byte, exercising the directional prediction process end-to-end.
    let directional = |x: u32, y: u32| {
        let vert = if (x / 2).is_multiple_of(2) { 210 } else { 30 } as u8; // vertical bars
        let horiz = if (y / 2).is_multiple_of(2) { 200 } else { 40 } as u8; // horizontal bars
        let diag = (((x + y) * 16) % 256) as u8; // 45° ramp
        [vert, horiz, diag]
    };
    for &q in &[8u8, 28, 96, 180] {
        for &(w, h) in &[(8, 8), (16, 16), (37, 21), (64, 40)] {
            check(&planes(w, h, directional), q);
        }
    }
}

#[test]
fn filter_intra_modes_match_dav1d() {
    // Mixed flat-plus-fine-texture content: the encoder's per-block search picks recursive
    // filter-intra (§7.11.2.3, signaled as a DC_PRED block + `use_filter_intra` + a
    // `filter_intra_mode`) on many blocks. dav1d must run the same recursive predictor and reach the
    // encoder's reconstruction byte-for-byte. (The encoder selects all five filter modes on this
    // content, so it covers FILTER_DC/V/H/D157/PAETH through the real decoder.)
    let textured = |x: u32, y: u32| {
        let r = (x.wrapping_mul(3).wrapping_add(y) % 256) as u8;
        let g = ((x + y.wrapping_mul(2)) % 256) as u8;
        let b = (128 + ((x ^ y) % 64)) as u8;
        [r, g, b]
    };
    for &q in &[6u8, 24, 88, 170] {
        for &(w, h) in &[(8, 8), (16, 16), (37, 21), (64, 40)] {
            check(&planes(w, h, textured), q);
        }
    }
}

#[test]
fn cfl_chroma_from_luma_matches_dav1d() {
    // Chroma that tracks the luma high-frequency: U falls as luma rises (negative alpha), V rises
    // with it (positive alpha). The encoder's per-block CfL search then signals uv_mode = UV_CFL_PRED
    // with non-zero CflAlphaU/CflAlphaV, and dav1d must run §7.11.5 chroma-from-luma to the encoder's
    // reconstruction byte-for-byte — exercising both alpha signs and read_cfl_alphas end-to-end.
    let cfl = |x: u32, y: u32| {
        let base = ((x.wrapping_mul(7).wrapping_add(y.wrapping_mul(5))) % 200) as i32 + 28; // luma
        let g = base as u8;
        let r = (base / 2 + 100).clamp(0, 255) as u8; // tracks +luma
        let b = (220 - base / 2).clamp(0, 255) as u8; // tracks -luma
        [r, g, b]
    };
    for &q in &[6u8, 24, 88, 170] {
        for &(w, h) in &[(8, 8), (16, 16), (37, 21), (64, 40)] {
            check(&planes(w, h, cfl), q);
        }
    }
}

#[test]
fn deblock_matches_dav1d() {
    // Block-aligned flat tiles with moderate steps between them: after quantization the 4×4 block
    // boundaries carry exactly the small discontinuities the deblocking loop filter (§7.14) smooths.
    // The encoder applies the filter to its reconstruction and dav1d applies it on decode, so the
    // byte-for-byte match validates the narrow-filter math, masks, and the vertical-then-horizontal
    // pass ordering. The level is chosen against the source, and on these tiles — whose 4-sample
    // steps are real edges — the encoder mostly declines to deblock; the smooth field, whose block
    // edges are quantization artefacts, is deblocked at levels from low to high.
    let tiles = |x: u32, y: u32| {
        let step = ((x / 4 + y / 4) % 6) as u8; // changes every 4 px ⇒ on the block grid
        let v = 60u8.wrapping_add(step.wrapping_mul(18));
        [
            v,
            v.wrapping_add(20),
            200u8.wrapping_sub(step.wrapping_mul(12)),
        ]
    };
    let mut levels = Vec::new();
    for content in [&tiles as &dyn Fn(u32, u32) -> [u8; 3], &smooth_field] {
        for &q in &[16u8, 48, 110, 200, 255] {
            for &(w, h) in &[(8, 8), (16, 16), (35, 23), (64, 40)] {
                levels.push(check_filters(&planes(w, h, content), q).deblock);
            }
        }
    }
    assert!(
        levels.iter().any(|&l| (1..16).contains(&l)) && levels.iter().any(|&l| l >= 32),
        "the cases must deblock at both low and high levels: {levels:?}"
    );
}

/// A smooth, low-frequency field: its block edges after quantization are artefacts the in-loop
/// filters remove, so the encoder chooses to run them.
fn smooth_field(x: u32, y: u32) -> [u8; 3] {
    let (fx, fy) = (f64::from(x), f64::from(y));
    let g = 128.0 + 60.0 * (fx / 23.0).sin() * (fy / 31.0).cos();
    [(g * 0.9) as u8, g as u8, (255.0 - g) as u8]
}

#[test]
fn mixed_4x4_and_8x8_blocks_match_dav1d() {
    // Content with both smooth regions (low local range ⇒ the encoder codes a single 8×8 block with
    // TX_8X8) and high-frequency regions (⇒ split to 4×4). An 8×8 transform spans two MI cells, so its
    // coefficient contexts that accumulate over neighbours — `dc_sign`, `txb_skip`, the level context
    // — must sum across both cells. When an 8×8 block borders 4×4 blocks with non-uniform DC signs,
    // reading a single cell diverges from a conformant decoder, so this is the gate for that mix.
    let mixed = |x: u32, y: u32| {
        // 16×16 smooth tiles (each a flat-ish gradient) separated by sharp seams every 16 px.
        let smooth = (((x % 16) + (y % 16)) * 2) as i32 + 40;
        let seam = if x.is_multiple_of(16) || y.is_multiple_of(16) {
            200
        } else {
            0
        };
        let v = (smooth + seam).clamp(0, 255);
        let r = (v / 2 + 90).clamp(0, 255); // chroma tracks luma (drives CfL on 8×8 blocks)
        let b = (210 - v / 2).clamp(0, 255);
        [r as u8, v as u8, b as u8]
    };
    for &q in &[21u8, 64, 130, 220] {
        // Sizes that force the partial-superblock / padding paths around the 8×8 blocks.
        for &(w, h) in &[(40, 40), (100, 70), (90, 96)] {
            check(&planes(w, h, mixed), q);
        }
    }
}

#[test]
fn cdef_matches_dav1d() {
    // Strong directional structure (diagonal/edged content) gives the CDEF direction search (§7.15.2)
    // distinct per-8×8 directions and non-trivial variance, so the primary+secondary deringing filter
    // (§7.15.3) actually fires. The encoder runs deblock → CDEF on its reconstruction and dav1d does
    // the same on decode, so byte-equality validates the direction search, constrain, taps, and the
    // out-of-frame sample availability. The strengths are chosen against the source; across these
    // cases every one of the four — luma and chroma, primary and secondary — is non-zero somewhere.
    let edged = |x: u32, y: u32| {
        let d = ((x + y) % 16) as u8; // 45° structure on the 8×8 CDEF grid
        let r = if d < 8 { 40 } else { 210 };
        let g = (30u8).wrapping_add(d.wrapping_mul(13));
        let b = if (x.wrapping_sub(y)) % 12 < 6 {
            70
        } else {
            190
        };
        [r, g, b]
    };
    let mut strengths = Vec::new();
    for &q in &[32u8, 80, 128, 220, 255] {
        for &(w, h) in &[(8, 8), (16, 16), (35, 23), (64, 40)] {
            strengths.push(check_filters(&planes(w, h, edged), q).cdef);
        }
    }
    for (i, name) in ["y_pri", "y_sec", "uv_pri", "uv_sec"].iter().enumerate() {
        assert!(
            strengths.iter().any(|s| s[i] > 0),
            "no case runs CDEF with a non-zero {name}: {strengths:?}"
        );
    }
}

#[test]
fn flat_lossy_reconstruction_matches_dav1d() {
    // A solid color: every residual quantizes to zero, so the reconstruction is the DC prediction
    // chain — a clean test that prediction-from-reconstruction tracks the decoder exactly.
    check(&planes(48, 40, |_, _| [200, 100, 50]), 16);
}

#[test]
fn directional_and_filter_intra_8x8_match_dav1d() {
    // Smooth low-amplitude ramps keep each 8×8 block below the split threshold, so the encoder codes
    // them as single TX_8X8 blocks — and their oriented gradients drive the 8×8 directional search,
    // where MiSize ≥ BLOCK_8X8 signals `angle_delta_y` (§5.11.42) and prediction follows the general
    // directional process (§7.11.2.4). Three orientations sweep the angle space across all four zones
    // (cardinal / zone-1 / zone-2 / zone-3) and non-zero angle deltas; the residual texture also
    // selects 8×8 recursive filter-intra. Byte-equality with dav1d validates the new angle signaling,
    // the 16-sample reference extension, and the size-generic filter-intra predictor.
    let tilted_h = |x: u32, y: u32| {
        let v = (50 + (x as i32 * 4 + y as i32) / 3).clamp(0, 255);
        [
            (v / 2 + 80).clamp(0, 255) as u8,
            v as u8,
            (200 - v / 2).clamp(0, 255) as u8,
        ]
    };
    let diagonal = |x: u32, y: u32| {
        let v = (40 + (x as i32 + y as i32) / 2).clamp(0, 255);
        [
            (v / 2 + 80).clamp(0, 255) as u8,
            v as u8,
            (200 - v / 2).clamp(0, 255) as u8,
        ]
    };
    let tilted_v = |x: u32, y: u32| {
        let v = (50 + (y as i32 * 4 + x as i32) / 3).clamp(0, 255);
        [
            (v / 2 + 80).clamp(0, 255) as u8,
            v as u8,
            (200 - v / 2).clamp(0, 255) as u8,
        ]
    };
    for f in [
        &tilted_h as &dyn Fn(u32, u32) -> [u8; 3],
        &diagonal,
        &tilted_v,
    ] {
        for &q in &[21u8, 64, 130, 220] {
            for &(w, h) in &[(64u32, 64u32), (40, 40), (90, 72)] {
                check(&planes(w, h, f), q);
            }
        }
    }
}

#[test]
fn transform_16x16_blocks_match_dav1d() {
    // Very-low-amplitude ramps keep each 16×16 block under the split threshold, so the encoder codes
    // them as single TX_16X16 blocks (PARTITION_NONE at BLOCK_16X16) — exercising the 256-coefficient
    // scan/CDFs (Eob_Pt_256, the txSzCtx-2 coeff tables), the per-`intraDir` 16×16 transform-type CDF,
    // 16×16 directional/filter-intra prediction, and — between adjacent 16×16 luma blocks — the wide
    // `filterSize == 16` deblock filter (§7.14.6.4 with log2Size = 4) plus its flatMask2. The three
    // orientations sweep the angle space; byte-equality with dav1d validates every new path. Sizes
    // are ≥ a superblock fraction so 16×16 blocks form in the interior at offsets that are multiples
    // of four MI cells.
    let diagonal = |x: u32, y: u32| {
        let v = (40 + (x as i32 + y as i32) / 8).clamp(0, 255);
        [
            (v / 2 + 80).clamp(0, 255) as u8,
            v as u8,
            (200 - v / 2).clamp(0, 255) as u8,
        ]
    };
    let tilted = |x: u32, y: u32| {
        let v = (50 + (x as i32 * 2 + y as i32) / 6).clamp(0, 255);
        [
            (v / 2 + 80).clamp(0, 255) as u8,
            v as u8,
            (200 - v / 2).clamp(0, 255) as u8,
        ]
    };
    let patches = |x: u32, y: u32| {
        let v = (60 + ((x / 16 + y / 16) % 3) as i32 * 6).clamp(0, 255);
        [
            (v / 2 + 80).clamp(0, 255) as u8,
            v as u8,
            (200 - v / 2).clamp(0, 255) as u8,
        ]
    };
    for f in [&diagonal as &dyn Fn(u32, u32) -> [u8; 3], &tilted, &patches] {
        for &q in &[21u8, 64, 130, 220] {
            for &(w, h) in &[(96u32, 96u32), (64, 64), (100, 80)] {
                check(&planes(w, h, f), q);
            }
        }
    }
}

#[test]
fn transform_32x32_blocks_match_dav1d() {
    // Near-flat ramps (slope < 1/px over 32 px) keep whole 32×32 regions below the split threshold,
    // so the encoder codes them as single TX_32X32 blocks (PARTITION_NONE at BLOCK_32X32). This
    // exercises the 1024-coefficient scan/CDFs (Eob_Pt_1024 — which, unlike the smaller eob classes,
    // has no neighbour-context dimension — and the txSzCtx-3 coeff tables), the `dqDenom = 2`
    // dequantization divisor unique to 32×32 (§7.12.3), DCT_DCT-only coding (TX_SET_DCTONLY ⇒ no
    // transform-type symbol), and 32×32 DC/smooth/directional/filter-intra prediction. Adjacent 32×32
    // luma edges deblock at filterSize 16 (the cap). Byte-equality with dav1d validates every path.
    let diagonal = |x: u32, y: u32| {
        let v = (48 + (x as i32 + y as i32) / 16).clamp(0, 255);
        [
            (v / 2 + 80).clamp(0, 255) as u8,
            v as u8,
            (200 - v / 2).clamp(0, 255) as u8,
        ]
    };
    let tilted = |x: u32, y: u32| {
        let v = (40 + (x as i32 * 2 + y as i32) / 20).clamp(0, 255);
        [
            (v / 2 + 80).clamp(0, 255) as u8,
            v as u8,
            (200 - v / 2).clamp(0, 255) as u8,
        ]
    };
    let patches = |x: u32, y: u32| {
        let v = (70 + ((x / 32 + y / 32) % 3) as i32 * 5).clamp(0, 255);
        [
            (v / 2 + 80).clamp(0, 255) as u8,
            v as u8,
            (200 - v / 2).clamp(0, 255) as u8,
        ]
    };
    for f in [&diagonal as &dyn Fn(u32, u32) -> [u8; 3], &tilted, &patches] {
        for &q in &[21u8, 64, 130, 220] {
            for &(w, h) in &[(96u32, 96u32), (72, 68), (128, 96)] {
                check(&planes(w, h, f), q);
            }
        }
    }
}

#[test]
fn variable_tx_size_match_dav1d() {
    // Moderately-textured smooth blocks (low enough range to stay PARTITION_NONE, high enough that
    // the encoder splits the transform): under TX_MODE_SELECT a ≥8×8 luma block signals tx_depth and
    // uses one block-size prediction mode with several smaller square sub-transforms, while 4:4:4
    // chroma keeps one block-size transform. Exercises tx_depth 1 and 2 across 8×8/16×16/32×32 blocks,
    // the per-transform-block BlockDecoded update (directional sub-transforms see their just-coded
    // siblings), the luma txb_skip neighbour context, and the per-plane deblock (luma at the sub-tx
    // size, chroma at the block size). Byte-equality with dav1d validates all of it.
    let ramp = |x: u32, y: u32| {
        let v = (48 + ((x % 32 + y % 32) as i32) / 2).clamp(0, 255);
        [
            (v / 2 + 80).clamp(0, 255) as u8,
            v as u8,
            (200 - v / 2).clamp(0, 255) as u8,
        ]
    };
    let blocky = |x: u32, y: u32| {
        let v = (40 + ((x % 16 + y % 16) as i32)).clamp(0, 255);
        [
            (v / 2 + 80).clamp(0, 255) as u8,
            v as u8,
            (200 - v / 2).clamp(0, 255) as u8,
        ]
    };
    for f in [&ramp as &dyn Fn(u32, u32) -> [u8; 3], &blocky] {
        for &q in &[14u8, 48, 110, 200] {
            for &(w, h) in &[(64u32, 64u32), (40, 40), (96, 72)] {
                check(&planes(w, h, f), q);
            }
        }
    }
}

#[test]
fn delta_q_match_dav1d() {
    // delta_q_present: each superblock's first block signals a per-SB delta_q, so CurrentQIndex (and
    // the dc/ac dequantizer step) varies across superblocks while the coefficient-CDF qctx stays at
    // its frame value (init_coeff_cdfs derives it from base_q_idx, §8.3.2). Multi-superblock sizes
    // (> 64 px) exercise several deltas; base_q_idx values sit on the qctx boundaries (20/60/120) so
    // a ±delta pushes CurrentQIndex across the boundary without changing qctx — the case that would
    // desync if qctx tracked CurrentQIndex. Byte-equality with dav1d validates the per-block quantizer
    // tracking and the frame-level qctx.
    let texture = |x: u32, y: u32| {
        let r = (x.wrapping_mul(3).wrapping_add(y) % 256) as u8;
        let g = ((x + y.wrapping_mul(2)) % 256) as u8;
        let b = (128 + ((x ^ y) % 64)) as u8;
        [r, g, b]
    };
    for &q in &[20u8, 21, 60, 61, 120, 121, 200] {
        for &(w, h) in &[(64u32, 64u32), (96, 96), (160, 96), (100, 70)] {
            check(&planes(w, h, texture), q);
        }
    }
}

#[test]
fn skip_blocks_match_dav1d() {
    // Large flat regions: interior blocks are perfectly DC-predicted (residual identically zero), so
    // the encoder codes them with skip = 1 (no residual; reconstruction = prediction). This exercises
    // the skip flag + its neighbour context, the reset of the level/dc coefficient contexts, and the
    // CDEF rule that an all-skip 8×8 block is not filtered (§7.15.1). A solid colour makes most
    // interior blocks skip; a two-region split adds skip/non-skip neighbour-context variety.
    let solid = |_x: u32, _y: u32| [180u8, 90, 40];
    let halves = |x: u32, _y: u32| {
        if x < 48 {
            [180u8, 90, 40]
        } else {
            [60, 150, 200]
        }
    };
    for &q in &[16u8, 48, 110, 200] {
        for &(w, h) in &[(64u32, 64u32), (96, 80), (128, 96)] {
            check(&planes(w, h, solid), q);
            check(&planes(w, h, halves), q);
        }
    }
}

#[test]
fn delta_lf_match_dav1d() {
    // delta_lf_present: each superblock's first block signals a per-SB delta_lf, so the deblocking
    // loop-filter level varies across superblocks (loop_filter_level + accumulated DeltaLF, per
    // §7.14.4). Structured content gives the deblock real work; multi-superblock sizes (> 64 px)
    // exercise several distinct per-SB levels. Byte-equality with dav1d validates the per-edge level
    // (taken from the q0-side block) and the DeltaLF accumulation.
    let edged = |x: u32, y: u32| {
        let bar = if (x / 3).is_multiple_of(2) { 70 } else { 190 } as u8;
        let ramp = ((x.wrapping_add(y).wrapping_mul(11)) % 256) as u8;
        let band = if (y / 4).is_multiple_of(2) { 60 } else { 200 } as u8;
        [bar, ramp, band]
    };
    let mut levels = Vec::new();
    for &q in &[24u8, 64, 128, 220] {
        for &(w, h) in &[(96u32, 96u32), (160, 96), (128, 128)] {
            levels.push(check_filters(&planes(w, h, edged), q).deblock);
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
fn palette_blocks_match_dav1d() {
    // Screen-content luma: a small global color set arranged so each 8×8..32×32 block sees 2..=8
    // distinct values, with flat chroma (U = V = 128 everywhere) so the block is DC-skippable on
    // chroma. The encoder codes such blocks with luma palette + skip = 1; dav1d decodes the palette,
    // the wavefront color-index map, and the cached colors back to the encoder reconstruction. This
    // exercises palette_mode_info (size/colors), the color cache, palette_tokens (the color context),
    // and predict_palette.
    // `from_rgb8_identity` maps G → luma, so the variation is in the G channel; R = B = 128 keeps
    // both chroma planes flat (DC-skippable). The number of distinct luma colors per 32×32 region is
    // `2 + ((x/32)+(y/32))%7` ∈ 2..=8, so different regions select different palette sizes (exercising
    // every `Default_Palette_Size_N_Y_Color_Cdf`), and the wavefront index map sees varied contexts.
    let lut = [20u8, 50, 80, 110, 140, 170, 200, 230];
    let screen = move |x: u32, y: u32| {
        let n = 2 + ((x / 32) + (y / 32)) as usize % 7;
        let idx = ((x / 4) + 2 * (y / 4)) as usize % n;
        [128, lut[idx], 128]
    };
    for &q in &[20u8, 64, 130, 210] {
        for &(w, h) in &[(64u32, 64u32), (96, 80), (160, 96)] {
            check(&planes(w, h, screen), q);
        }
    }
}

#[test]
fn segmentation_match_dav1d() {
    // segmentation_enabled with SEG_LVL_ALT_Q on two segments: every non-skip block is assigned a
    // spatially-varied segment id (coded via neg_interleave under the spatial-prediction context) and
    // quantized at CurrentQIndex + the segment's alt-Q delta. Textured content keeps blocks non-skip so
    // the ids are actually coded and the per-segment quantizer changes the residual; dav1d must derive
    // the same per-block quantizer and reach the encoder reconstruction byte-for-byte.
    let textured = |x: u32, y: u32| {
        let r = (x.wrapping_mul(5).wrapping_add(y.wrapping_mul(3)) % 256) as u8;
        let g = ((x.wrapping_add(y).wrapping_mul(2)) % 256) as u8;
        let b = (40 + ((x.wrapping_mul(7) ^ y) % 160)) as u8;
        [r, g, b]
    };
    for &q in &[8u8, 32, 96, 180] {
        for &(w, h) in &[(32u32, 32u32), (64, 48), (96, 72)] {
            check(&planes(w, h, textured), q);
        }
    }
}

#[test]
fn transform_64x64_blocks_match_dav1d() {
    // 64×64 blocks: PARTITION_NONE + TX_64X64 luma (chroma is a 2×2 raster of TX_32X32, since chroma
    // never uses TX_64X64). Solid content is DC-predictable, so interior superblocks code skip = 1,
    // and a skip 64×64 block — one that fills the whole superblock — codes no delta-q/delta-lf
    // (`read_delta_qindex`/`read_delta_lf` return early). Gradient content codes a multi-coefficient
    // residual instead. The size matrix exercises a single superblock (64×64), the horizontal,
    // vertical, and 2-D multi-superblock layouts (a skip 64×64 SB followed by another SB), and
    // partial superblocks cropped at the frame edge (which split to ≤32×32 rather than code a
    // partial 64×64 block).
    let solid = |_: u32, _: u32| [128u8, 90, 128];
    let grad = |x: u32, y: u32| [128u8, 90 + ((x % 64) / 8 + (y % 64) / 8) as u8, 128];
    for &(w, h) in &[
        (64u32, 64u32),
        (128, 64),
        (64, 128),
        (128, 128),
        (128, 96),
        (96, 128),
    ] {
        for &q in &[16u8, 64, 144] {
            check(&planes(w, h, solid), q);
            check(&planes(w, h, grad), q);
        }
    }
}
#[test]
fn rectangular_partitions_match_dav1d() {
    // A single horizontal or vertical luma+chroma edge over a 16×16 or 32×32 region makes the encoder
    // pick PARTITION_HORZ / PARTITION_VERT, coding two rectangular halves each as one rectangular
    // transform (TX_16X8/8X16/32X16/16X32). Exercises the rect scan, the aspect-specific coeff-base
    // offset, the rect prediction, and the rect deblock edges — all bit-exact against dav1d.
    for &q in &[16u8, 64, 144] {
        // 32×32 → HORZ (TX_32X16) and VERT (TX_16X32), with a chroma edge so chroma codes a residual.
        check(
            &planes(32, 32, |_, y| {
                if y < 16 {
                    [50, 60, 50]
                } else {
                    [200, 150, 200]
                }
            }),
            q,
        );
        check(
            &planes(32, 32, |x, _| {
                if x < 16 {
                    [50, 60, 50]
                } else {
                    [200, 150, 200]
                }
            }),
            q,
        );
        // 16×16 blocks across a larger frame → HORZ (TX_16X8) / VERT (TX_8X16) with neighbours on all
        // sides (so the rect tx-depth and deblock neighbour contexts are exercised).
        check(
            &planes(64, 64, |_, y| {
                let v = if (y % 16) < 8 { 50 } else { 160 };
                [128, v, 128]
            }),
            q,
        );
        check(
            &planes(64, 64, |x, _| {
                let v = if (x % 16) < 8 { 50 } else { 160 };
                [128, v, 128]
            }),
            q,
        );
    }
}

#[test]
fn multi_tile_matches_dav1d() {
    // Frames ≥ 2 superblocks wide are coded as two tile columns (§5.9.15). Each tile decodes
    // independently: a block at the tile's left edge has no left neighbour, and a block at the
    // tile's right edge must not treat the (not-yet-decoded) adjacent tile as an available
    // above-right. Strong directional structure makes the mode search pick angles that read those
    // edges, so dav1d byte-equality proves the per-tile reset, the tile-boundary neighbour
    // availability, and the tile-group framing across every coefficient-CDF quantizer context.
    let directional = |x: u32, y: u32| {
        let vert = if (x / 2).is_multiple_of(2) { 210 } else { 30 } as u8;
        let horiz = if (y / 2).is_multiple_of(2) { 200 } else { 40 } as u8;
        let diag = (((x + y) * 16) % 256) as u8;
        [vert, horiz, diag]
    };
    for &q in &[2u8, 20, 21, 90, 121, 255] {
        for &(w, h) in &[(72, 16), (100, 70), (130, 96)] {
            check(&planes(w, h, directional), q);
        }
    }
}

#[test]
fn every_tile_layout_matches_both_decoders() {
    // Configurable tiling (#648): tile *rows* as well as columns, layouts the frame clamps, and
    // tiles coded on several threads. A tile row's top edge is an above edge the decoder treats as
    // unavailable (`AvailU`, §7.3.1) and resets the above contexts at (`clear_above_context`), so a
    // block there must predict and code its contexts as if at the frame top. Directional content
    // makes the mode search read those edges; libaom and dav1d reproducing the reconstruction
    // byte-for-byte proves the availability, the per-tile resets, the `tile_info()` climb and the
    // tile-group framing. Each layout is coded on three threads, so the reconstruction compared is
    // also the one merged from worker threads.
    let directional = |x: u32, y: u32| {
        let vert = if (x / 2).is_multiple_of(2) { 210 } else { 30 } as u8;
        let horiz = if (y / 3).is_multiple_of(2) { 200 } else { 40 } as u8;
        let diag = (((x + 2 * y) * 12) % 256) as u8;
        [vert, horiz, diag]
    };
    let three = NonZeroUsize::new(3).unwrap();
    let tilings = [
        Tiling::SINGLE,
        Tiling::new(0, 1),
        Tiling::new(1, 1),
        Tiling::new(2, 2),
        Tiling::new(6, 6),
    ];
    for tiling in tilings {
        let options = EncodeOptions::default()
            .with_tiling(tiling)
            .with_threads(three);
        for &q in &[0u8, 20, 90, 200] {
            for &(w, h) in &[(200, 150), (130, 200), (64, 129)] {
                let p = planes(w, h, directional);
                check_with(
                    encode_still_intra_with_options(&p, q, Av1Colour::default(), options).unwrap(),
                    q,
                );
            }
        }
        // 4:2:0, 4:2:2 and monochrome carry the tile edges into subsampled and absent chroma.
        let colour = colour_for(MatrixCoefficients::Bt709, ColorRange::Full);
        for ss in [ChromaSubsampling::Cs420, ChromaSubsampling::Cs422] {
            let p = planes_subsampled(
                200,
                150,
                ss,
                MatrixCoefficients::Bt709,
                ColorRange::Full,
                directional,
            );
            check_with(
                encode_still_intra_with_options(&p, 60, colour, options).unwrap(),
                60,
            );
        }
        let mono = mono_planes(200, 150, |x, y| directional(x, y)[0] ^ directional(x, y)[2]);
        check_with(
            encode_still_intra_with_options(&mono, 60, Av1Colour::monochrome(), options).unwrap(),
            60,
        );
        // The high-bit-depth entry point takes the same layout.
        let deep = planes16(200, 150, BitDepth::Ten, texture16(BitDepth::Ten));
        check_with(
            encode_still_intra16_with_options(&deep, 40, Av1Colour::default(), options).unwrap(),
            40,
        );
    }
}

#[test]
fn loop_restoration_matches_dav1d() {
    // Luma Wiener loop restoration (§7.17), on the frames whose restoration units the encoder
    // finds it improves. Tall frames span several 64-row restoration stripes (56 + 64 + …); each
    // stripe's top/bottom boundary rows come from the deblocked (pre-CDEF) reconstruction. Heights
    // 70/130/200 exercise 2/3/4 stripes, and the wide cases also cross a tile-column boundary —
    // dav1d byte-equality proves the stripe-boundary sourcing, the per-superblock unit signaling
    // (use_wiener + subexp coefficients), and the filter math end-to-end. Frames where no unit
    // is restored prove the other half: RESTORE_NONE, with no unit coded in the tiles.
    let texture = |x: u32, y: u32| {
        let r = (x.wrapping_mul(5).wrapping_add(y.wrapping_mul(3)) % 256) as u8;
        let g = ((x.wrapping_add(y).wrapping_mul(2)) % 256) as u8;
        let b = (64 + ((x.wrapping_mul(7) ^ y) % 128)) as u8;
        [r, g, b]
    };
    let mut restored = Vec::new();
    for content in [&texture as &dyn Fn(u32, u32) -> [u8; 3], &smooth_field] {
        for &q in &[8u8, 40, 120, 200] {
            for &(w, h) in &[(48, 130), (100, 200), (24, 60)] {
                restored.push(check_filters(&planes(w, h, content), q).wiener);
            }
        }
    }
    assert!(
        restored.contains(&true) && restored.contains(&false),
        "the cases must both restore and not: {restored:?}"
    );
}

#[test]
fn restoration_units_decided_one_by_one_match_both_decoders() {
    // A frame three restoration units wide (640 / 256, the last absorbing the remainder) whose
    // units the encoder decides differently — the smooth field's left unit is restored and the
    // others are not (pinned by the encoder's own unit test) — so each unit's `use_wiener` is
    // coded at its own superblock and applied to its own samples, and both decoders must agree.
    // That unit test codes the full search, so this one names `Thorough` to code the same frame.
    let (still, recon) = encode_still_intra_with_speed(
        &planes(640, 64, smooth_field),
        110,
        Av1Colour::default(),
        Speed::Thorough,
    )
    .unwrap();
    let header = gamut_av1::Av1Decoder::new().inspect(&still.obus).unwrap();
    assert_eq!(
        header.frame.lr.frame_restoration_type[0],
        gamut_av1::RestorationType::Wiener
    );
    check_with((still, recon), 110);
    // With superres the units are counted on the upscaled frame — three here, where the coded
    // width alone would count two — and a superblock's columns are scaled up onto that grid
    // (§5.11.57): 640 upscaled from 427 (SuperresDenom 12) and from 320 (SuperresDenom 16). Both
    // decoders read the unit signalling at the scaled positions.
    for denom in [3u8, 7] {
        let (still, recon) =
            gamut_av1::encode_still_intra_superres(&planes(640, 64, smooth_field), 200, denom)
                .unwrap();
        let header = gamut_av1::Av1Decoder::new().inspect(&still.obus).unwrap();
        assert_eq!(
            header.frame.lr.frame_restoration_type[0],
            gamut_av1::RestorationType::Wiener,
            "denom {denom}"
        );
        check_with((still, recon), 200);
    }
}

#[test]
fn superres_matches_dav1d() {
    // Horizontal superres (§7.16): the source is coded at FrameWidth = UpscaledWidth*8/denom and the
    // reconstruction is upscaled back. dav1d must upscale the coded frame identically — this exercises
    // the 8-tap polyphase filter, the subpel geometry, and the superres frame-header signaling.
    let texture = |x: u32, y: u32| {
        let r = (x.wrapping_mul(5).wrapping_add(y.wrapping_mul(3)) % 256) as u8;
        let g = ((x.wrapping_add(y).wrapping_mul(2)) % 256) as u8;
        let b = (64 + ((x.wrapping_mul(7) ^ y) % 128)) as u8;
        [r, g, b]
    };
    for &denom in &[0u8, 3, 7] {
        for &(w, h) in &[(64, 32), (100, 48), (33, 20), (80, 130)] {
            let p = planes(w, h, texture);
            let (still, recon) = gamut_av1::encode_still_intra_superres(&p, 40, denom).unwrap();
            let mut stream = vec![0x12u8, 0x00];
            stream.extend_from_slice(&still.obus);
            let decoded = dav1d_oracle::decode_obu(&stream)
                .unwrap_or_else(|e| panic!("dav1d failed {w}x{h} denom{denom}: {e}"));
            assert_eq!(
                (decoded.width as usize, decoded.height as usize),
                (recon.width as usize, recon.height as usize),
                "dims {w}x{h} denom{denom}"
            );
            for (pl, (dec, enc)) in decoded.planes.iter().zip(&recon.planes).enumerate() {
                assert_eq!(dec, enc, "plane {pl} mismatch {w}x{h} denom{denom}");
            }
        }
    }
}

#[test]
fn smooth_modes_match_dav1d() {
    // Smooth, low-frequency content (a non-separable bilinear/quadratic field) so the per-block
    // mode search prefers the non-directional SMOOTH family over DC and the directional copies:
    // the SMOOTH blend (§7.11.2.6) reproduces a smooth ramp, while DC leaves the whole gradient as
    // residual and a directional mode can only copy one edge. The SMOOTH predictors live in
    // `predict_nondir`, so dav1d byte-equality exercises that path end-to-end. Mode choice is a
    // rate–distortion decision, so it moves with `qindex`, and the SMOOTH family is not reached at
    // every quantizer and geometry: tracing the mode search over these geometries (one superblock,
    // at the commit that introduced RD mode choice) found SMOOTH-family blocks at every `q` of the
    // ladder — at 8 only in 31×17, at 40 in 31×17 and 17×31, at 120 also in 40×36 — and none in
    // 16×16, 32×32 or 24×40. Nothing here re-checks that, so an encoder change can move the
    // ladder off the SMOOTH predictors while this test stays green.
    let smooth = |x: u32, y: u32| {
        let (x, y) = (x as i32, y as i32);
        let r = (40 + (x * 3 + y * 2) / 2).clamp(0, 255) as u8;
        let g = (30 + (x * y) / 16).clamp(0, 255) as u8; // bilinear cross term (non-separable)
        let b = (210 - (x * 2 + y * 3) / 3).clamp(0, 255) as u8;
        [r, g, b]
    };
    // The non-square cases also pin a partition-edge regression: a smooth region keeps a single
    // large PARTITION_NONE block, and at the frame edge that block extends past the MI-frame, so it
    // must be force-split rather than coded whole (otherwise its block-size transform writes out of
    // the reconstruction buffer). The geometries cover both overhang axes and block sizes: 31×17 →
    // coded 32×24 overhangs a 32×32 block's rows; 17×31 → coded 24×32 overhangs its columns; 40×36 →
    // coded 40×40 overhangs a 64×64 superblock. dav1d byte-equality proves the forced split is
    // signaled correctly.
    for &q in &[8u8, 40, 120] {
        for &(w, h) in &[(16, 16), (32, 32), (24, 40), (31, 17), (17, 31), (40, 36)] {
            check(&planes(w, h, smooth), q);
        }
    }
}

#[test]
fn non_identity_colour_config_matches_dav1d() {
    // Signalling a real matrix leaves the AV1 §5.5.2 sRGB shortcut, so `color_config()` codes an
    // extra `color_range` bit before `separate_uv_delta_q`. Every later syntax element shifts by
    // that one bit, so a wrong branch — the bit emitted when it should be inferred, omitted when it
    // should be coded, or written in the wrong position — desyncs the whole sequence header and
    // both reference decoders diverge or fail outright. The coding tools themselves are
    // matrix-agnostic (the planes are the same bytes either way), which is exactly why this test
    // isolates the header change.
    let texture = |x: u32, y: u32| {
        let r = (x.wrapping_mul(5).wrapping_add(y.wrapping_mul(3)) % 256) as u8;
        let g = ((x.wrapping_add(y).wrapping_mul(2)) % 256) as u8;
        let b = (64 + ((x.wrapping_mul(7) ^ y) % 128)) as u8;
        [r, g, b]
    };
    let colours = [
        // BT.709 full range — what the AVIF encoder's lossy default signals.
        Av1Colour {
            primaries: ColourPrimaries::Bt709,
            transfer: TransferCharacteristics::Srgb,
            matrix: MatrixCoefficients::Bt709,
            range: ColorRange::Full,
        },
        // …and studio range, so the coded bit is exercised at both values.
        Av1Colour {
            primaries: ColourPrimaries::Bt709,
            transfer: TransferCharacteristics::Srgb,
            matrix: MatrixCoefficients::Bt709,
            range: ColorRange::Limited,
        },
        // A non-BT.709 primaries/matrix pair, so the shortcut is missed on more than one field.
        Av1Colour {
            primaries: ColourPrimaries::Bt2020,
            transfer: TransferCharacteristics::Srgb,
            matrix: MatrixCoefficients::Bt2020Ncl,
            range: ColorRange::Full,
        },
    ];
    for colour in colours {
        for &q in &[0u8, 12, 64, 200] {
            for &(w, h) in &[(8, 8), (17, 13), (64, 48), (100, 70)] {
                let p = planes(w, h, texture);
                check_with(encode_still_intra_with(&p, q, colour).unwrap(), q);
            }
        }
    }
}

#[test]
fn paeth_mode_matches_dav1d() {
    // Separable additive content `f(x,y) = u(x) + v(y)` (no clamping, so it stays planar): PAETH's
    // predictor `left + above - aboveleft` is then near-exact, while DC/SMOOTH/directional all leave
    // a growing residual, so the mode search selects PAETH (§7.11.2). PAETH is the fourth
    // non-directional mode in `predict_nondir`; dav1d byte-equality validates its reconstruction.
    let separable = |x: u32, y: u32| {
        let (x, y) = (x as i32, y as i32);
        let r = (20 + x + 2 * y) as u8; // ≤ ~130 over the sizes below: stays planar, no clamp/wrap
        let g = (30 + 2 * x + y) as u8;
        let b = (60 + x + y) as u8;
        [r, g, b]
    };
    for &q in &[6u8, 40, 120] {
        for &(w, h) in &[(16, 16), (32, 32), (24, 40), (31, 17)] {
            check(&planes(w, h, separable), q);
        }
    }
}

/// A textured generator: strong local variation so residuals are non-trivial, the partition search
/// splits, and chroma actually carries signal rather than sitting flat.
fn textured(x: u32, y: u32) -> [u8; 3] {
    let r = ((x * 7 + y * 3) % 251) as u8;
    let g = ((x * 3 + y * 11) % 241) as u8;
    let b = ((x ^ y).wrapping_mul(5) % 239) as u8;
    [r, g, b]
}

#[test]
fn subsampled_420_reconstruction_matches_both_decoders() {
    // The gate for 4:2:0: every chroma derivation — HasChroma, the plane residual size, the chroma
    // transform, the entropy-context grids, the CfL box average, and the chroma deblock/CDEF grids
    // — is only proved correct by two independent decoders reproducing the encoder's own
    // reconstruction. Odd dimensions exercise the ceiling division on both chroma axes; the small
    // sizes force sub-8x8 blocks, where a 4x4 luma block codes no chroma of its own.
    for (w, h) in [
        (16, 16),
        (17, 13),
        (9, 9),
        (8, 8),
        (4, 4),
        (1, 1),
        (3, 5),
        (33, 17),
        (64, 64),
        (40, 24),
        // Wide enough for two tile columns, where the tile's left edge is a *luma* position that
        // prediction availability must compare against in each plane's own coordinates. A frame
        // narrower than this either has one tile or a second tile just one chroma block wide, and
        // passes either way.
        (100, 80),
        (128, 72),
    ] {
        let p = planes_subsampled(
            w,
            h,
            ChromaSubsampling::Cs420,
            MatrixCoefficients::Bt709,
            ColorRange::Full,
            textured,
        );
        check_with(
            encode_still_intra_with(
                &p,
                40,
                colour_for(MatrixCoefficients::Bt709, ColorRange::Full),
            )
            .unwrap(),
            40,
        );
    }
}

#[test]
fn subsampled_420_reconstruction_matches_at_every_quantizer_context() {
    // The eob-position CDFs — including the 32-coefficient table this work added, which only a
    // subsampled stream reaches — are selected per quantizer context, so a single quantizer would
    // leave three of the four rows unexercised.
    for q in [4u8, 40, 90, 200] {
        let p = planes_subsampled(
            24,
            24,
            ChromaSubsampling::Cs420,
            MatrixCoefficients::Bt709,
            ColorRange::Full,
            textured,
        );
        check_with(
            encode_still_intra_with(
                &p,
                q,
                colour_for(MatrixCoefficients::Bt709, ColorRange::Full),
            )
            .unwrap(),
            q,
        );
    }
}

#[test]
fn subsampled_420_reconstruction_matches_across_matrices_and_ranges() {
    for matrix in [
        MatrixCoefficients::Bt601,
        MatrixCoefficients::Bt709,
        MatrixCoefficients::Bt2020Ncl,
    ] {
        for range in [ColorRange::Full, ColorRange::Limited] {
            let p = planes_subsampled(20, 20, ChromaSubsampling::Cs420, matrix, range, textured);
            check_with(
                encode_still_intra_with(&p, 60, colour_for(matrix, range)).unwrap(),
                60,
            );
        }
    }
}

#[test]
fn subsampled_422_reconstruction_matches_both_decoders() {
    // 4:2:2 is the only layout where `subsampling_x != subsampling_y`, so it is the only one that
    // can catch an x/y transposition anywhere in the chroma derivations — 4:2:0 is blind to those
    // by construction. It is also the only one with a non-identity `Cdef_Uv_Dir` and a constrained
    // partition set (§6.10.4 forbids taller-than-wide blocks).
    for (w, h) in [
        (16, 16),
        (17, 13),
        (9, 9),
        (8, 8),
        (4, 4),
        (1, 1),
        (3, 5),
        (33, 17),
        (64, 64),
        (40, 24),
        (100, 80),
        (128, 72),
    ] {
        let p = planes_subsampled(
            w,
            h,
            ChromaSubsampling::Cs422,
            MatrixCoefficients::Bt709,
            ColorRange::Full,
            textured,
        );
        check_with(
            encode_still_intra_with(
                &p,
                40,
                colour_for(MatrixCoefficients::Bt709, ColorRange::Full),
            )
            .unwrap(),
            40,
        );
    }
}

#[test]
fn subsampled_422_reconstruction_matches_at_every_quantizer_context() {
    for q in [4u8, 40, 90, 200] {
        let p = planes_subsampled(
            24,
            24,
            ChromaSubsampling::Cs422,
            MatrixCoefficients::Bt709,
            ColorRange::Full,
            textured,
        );
        check_with(
            encode_still_intra_with(
                &p,
                q,
                colour_for(MatrixCoefficients::Bt709, ColorRange::Full),
            )
            .unwrap(),
            q,
        );
    }
}

#[test]
fn subsampled_422_reconstruction_matches_on_a_period_two_vertical_stripe() {
    // The test 4:2:0 cannot substitute for. A stripe with period 2 in x is collapsed entirely by
    // the 2x1 horizontal box average and left intact by a 2x2 one, so any derivation that swapped
    // the x and y shifts produces visibly different chroma here and identical chroma at 4:2:0.
    let stripe = |x: u32, _y: u32| {
        if x.is_multiple_of(2) {
            [220, 30, 40]
        } else {
            [30, 220, 210]
        }
    };
    for (w, h) in [(32, 32), (17, 13), (64, 16)] {
        let p = planes_subsampled(
            w,
            h,
            ChromaSubsampling::Cs422,
            MatrixCoefficients::Bt709,
            ColorRange::Full,
            stripe,
        );
        check_with(
            encode_still_intra_with(
                &p,
                30,
                colour_for(MatrixCoefficients::Bt709, ColorRange::Full),
            )
            .unwrap(),
            30,
        );
    }
}

#[test]
fn subsampled_422_reconstruction_matches_on_every_cdef_direction() {
    // `Cdef_Uv_Dir[1][0]` is the only non-identity row, and a wrong chroma direction on smooth
    // content is nearly invisible. Diagonal ramps at a range of gradients drive the direction
    // search across all eight of its outputs, so a rotated or offset remap diverges from both
    // reference decoders.
    for (dx, dy) in [
        (1i32, 0i32),
        (1, 1),
        (0, 1),
        (2, 1),
        (1, 2),
        (3, 1),
        (1, 3),
        (-1, 1),
    ] {
        let ramp = move |x: u32, y: u32| {
            let v = ((x as i32 * dx + y as i32 * dy) * 9).rem_euclid(256) as u8;
            [v, 255 - v, v / 2 + 40]
        };
        let p = planes_subsampled(
            32,
            32,
            ChromaSubsampling::Cs422,
            MatrixCoefficients::Bt709,
            ColorRange::Full,
            ramp,
        );
        check_with(
            encode_still_intra_with(
                &p,
                80,
                colour_for(MatrixCoefficients::Bt709, ColorRange::Full),
            )
            .unwrap(),
            80,
        );
    }
}

#[test]
fn subsampled_420_reconstruction_matches_on_palette_content() {
    // Screen-content blocks take the palette path, where chroma is a flat DC and no CfL is
    // signalled. That interaction is chroma-specific — a palette block still has chroma of its own
    // — and only shows up when the encoder actually selects a palette, which photographic content
    // never does. Few distinct colours in large flat runs is what triggers it.
    // Greyscale on purpose: a palette block must also match its chroma DC prediction, and neutral
    // chroma everywhere is what lets that converge. Coloured runs vary the chroma per block and the
    // palette path is then never taken, leaving the interaction untested.
    let flat_runs = |x: u32, y: u32| {
        let v = match ((x / 8) + (y / 8)) % 3 {
            0 => 20u8,
            1 => 128,
            _ => 210,
        };
        [v, v, v]
    };
    for (w, h) in [(32u32, 32u32), (64, 48), (17, 13)] {
        let p = planes_subsampled(
            w,
            h,
            ChromaSubsampling::Cs420,
            MatrixCoefficients::Bt709,
            ColorRange::Full,
            flat_runs,
        );
        check_with(
            encode_still_intra_with(
                &p,
                60,
                colour_for(MatrixCoefficients::Bt709, ColorRange::Full),
            )
            .unwrap(),
            60,
        );
    }
}

#[test]
fn subsampled_422_reconstruction_matches_on_palette_content() {
    // The same screen-content path at 4:2:2, where the palette block's chroma residual is
    // rectangular rather than square.
    // Greyscale for the same reason as the 4:2:0 case: neutral chroma everywhere is what lets a
    // palette block match its chroma DC prediction, and without that the palette path is never
    // taken.
    let flat_runs = |x: u32, y: u32| {
        let v = match ((x / 8) + (y / 8)) % 3 {
            0 => 20u8,
            1 => 128,
            _ => 210,
        };
        [v, v, v]
    };
    for (w, h) in [(32u32, 32u32), (64, 48), (17, 13)] {
        let p = planes_subsampled(
            w,
            h,
            ChromaSubsampling::Cs422,
            MatrixCoefficients::Bt709,
            ColorRange::Full,
            flat_runs,
        );
        check_with(
            encode_still_intra_with(
                &p,
                60,
                colour_for(MatrixCoefficients::Bt709, ColorRange::Full),
            )
            .unwrap(),
            60,
        );
    }
}
/// Builds a monochrome (`Cs400`) buffer — one luma plane, no chroma — from a gray generator.
fn mono_planes(w: u32, h: u32, f: impl Fn(u32, u32) -> u8) -> Planar8 {
    let mut y = vec![0u8; (w * h) as usize];
    for row in 0..h {
        for col in 0..w {
            y[(row * w + col) as usize] = f(col, row);
        }
    }
    Planar8::from_planes_subsampled(w, h, ChromaSubsampling::Cs400, [y, Vec::new(), Vec::new()])
        .expect("valid monochrome planes")
}

#[test]
fn monochrome_reconstruction_matches_both_decoders() {
    // A monochrome still is `seq_profile = 0` with `mono_chrome = 1`: one coded luma plane and, per
    // §5.5.2, no `subsampling_x`/`subsampling_y`/`separate_uv_delta_q` bits at all. Every chroma
    // syntax element the frame header would otherwise carry — the U delta-Q pair (§5.9.12), the two
    // chroma deblock levels (§5.9.11), the CDEF UV strengths (§5.9.19) and two of the three
    // `lr_type`s (§5.9.20) — disappears with it, and `HasChroma` (§5.11.5) turns off `uv_mode`,
    // `read_cfl_alphas` and `has_palette_uv` in the tile.
    //
    // A single wrong bit anywhere in that set desynchronises the arithmetic decoder, so byte
    // equality against both reference decoders is what proves the whole set is right.
    let texture = |x: u32, y: u32| (x.wrapping_mul(3).wrapping_add(y.wrapping_mul(5)) % 256) as u8;
    for &q in &[0u8, 4, 20, 40, 90, 160, 255] {
        for &(w, h) in &[(8, 8), (17, 13), (32, 32), (64, 48), (100, 70)] {
            let p = mono_planes(w, h, texture);
            check_with(
                encode_still_intra_with(&p, q, Av1Colour::monochrome()).unwrap(),
                q,
            );
        }
    }
}

#[test]
fn monochrome_flat_and_two_tone_content_matches_both_decoders() {
    // Flat content drives the `skip = 1` path, whose skippability test now consults only the coded
    // planes; two-tone content drives luma palette mode, whose chroma-flatness precondition is
    // vacuous without chroma, so a monochrome frame reaches palette on content a 4:4:4 frame would
    // not. Both are reconstruction paths a plane-count mistake would corrupt silently.
    let two_tone = |x: u32, y: u32| {
        if (x / 4 + y / 4).is_multiple_of(2) {
            30
        } else {
            210
        }
    };
    for &q in &[0u8, 8, 64, 200] {
        for &(w, h) in &[(16, 16), (33, 21), (64, 64)] {
            check_with(
                encode_still_intra_with(&mono_planes(w, h, |_, _| 137), q, Av1Colour::monochrome())
                    .unwrap(),
                q,
            );
            check_with(
                encode_still_intra_with(&mono_planes(w, h, two_tone), q, Av1Colour::monochrome())
                    .unwrap(),
                q,
            );
        }
    }
}

#[test]
fn monochrome_rejects_the_identity_matrix() {
    // §5.5.2 infers `subsampling_x = subsampling_y = 1` for a monochrome stream and §6.4.2 allows
    // MC_IDENTITY only at 0/0, so the default colour is non-conformant here. Rejecting it keeps
    // `Av1Colour::default()` from silently producing a stream a decoder may refuse.
    let p = mono_planes(16, 16, |x, _| x as u8);
    let err = encode_still_intra(&p, 40).expect_err("identity is not conformant when monochrome");
    assert!(
        err.static_message()
            .is_some_and(|m| m.contains("monochrome stream cannot signal the identity matrix")),
        "unexpected diagnostic: {err:?}"
    );
    // The same buffer encodes once the matrix is a conformant one.
    assert!(encode_still_intra_with(&p, 40, Av1Colour::monochrome()).is_ok());
}

#[test]
fn monochrome_rejects_superres() {
    // The superres downscale is written for three luma-sized planes and relabels its output 4:4:4,
    // which would give the plane count two disagreeing sources of truth on a monochrome source.
    // The combination is refused rather than half-supported, and the check runs before the matrix
    // rule so the reported reason is the one that actually applies.
    let p = mono_planes(32, 16, |x, y| (x ^ y) as u8);
    let err = encode_still_intra_superres(&p, 40, 0)
        .expect_err("superres over a monochrome source is refused");
    assert_eq!(
        err.static_message(),
        Some("AV1: superres over a monochrome source is not implemented")
    );
    // The same request succeeds for a 4:4:4 source, so the guard keys on the plane count.
    let rgb = planes(32, 16, |x, y| [x as u8, y as u8, 0]);
    assert!(encode_still_intra_superres(&rgb, 40, 0).is_ok());
}

// ---- high bit depth (#398) --------------------------------------------------------------------

/// Builds 4:4:4 identity planes at `bits` from a per-plane generator, so a value near the top of
/// the coded range is reachable (the clamps, the palette delta width and the deblock centring are
/// all range-dependent, and an 8-bit-sized pattern would never reach them).
fn planes16(w: u32, h: u32, bits: BitDepth, f: impl Fn(u32, u32) -> [u16; 3]) -> Planar16 {
    let mut p: [Vec<u16>; 3] = std::array::from_fn(|_| vec![0u16; (w * h) as usize]);
    for y in 0..h {
        for x in 0..w {
            let v = f(x, y);
            for (i, plane) in p.iter_mut().enumerate() {
                plane[(y * w + x) as usize] = v[i].min(bits.max_value());
            }
        }
    }
    Planar16::from_planes(w, h, bits, p).expect("valid high-bit-depth planes")
}

/// A textured generator spanning the full `bits` range: the 8-bit pattern the lossy cases use,
/// scaled up so the top of the range is actually exercised.
fn texture16(bits: BitDepth) -> impl Fn(u32, u32) -> [u16; 3] {
    let scale = u32::from(bits.max_value()) / 255;
    move |x: u32, y: u32| {
        let r = (x.wrapping_mul(3).wrapping_add(y) % 256) * scale;
        let g = ((x + y.wrapping_mul(2)) % 256) * scale;
        let b = (128 + ((x ^ y) % 64)) * scale;
        [r as u16, g as u16, b as u16]
    }
}

#[test]
fn high_bit_depth_lossless_reconstruction_matches_both_decoders() {
    // `qindex = 0` is the WHT lossless path: the decoded samples must equal the source exactly, at
    // the coded depth. 12-bit also moves the stream to `seq_profile = 2` with `twelve_bit = 1`, a
    // sequence header shape nothing else in the suite emits.
    for bits in [BitDepth::Ten, BitDepth::Twelve] {
        for &(w, h) in &[(1, 1), (8, 8), (17, 13), (64, 48)] {
            let src = planes16(w, h, bits, texture16(bits));
            let encoded = encode_still_intra16_with(&src, 0, Av1Colour::default()).unwrap();
            assert_eq!(encoded.1.bit_depth, bits);
            check_with(encoded, 0);
        }
    }
}

#[test]
fn high_bit_depth_lossy_reconstruction_matches_both_decoders() {
    // The lossy path is where the depth actually threads through everything: the dequant and
    // inverse-transform clamps, the `1 << (BitDepth-1)` intra seeds, the deblock centring and
    // threshold scaling, CDEF's `coeffShift` on the direction search and both strengths, the
    // Wiener rounding pair (which differs only at 12 bits), and the palette's `L(BitDepth)` first
    // colour. A mistake in any one of them desynchronises the decoders from the reconstruction.
    //
    // The lossy colour is BT.709 YCbCr, which leaves the §5.5.2 sRGB shortcut — so 12-bit also
    // exercises the coded `subsampling_x` bit that only profile 2 at 12 bits emits.
    let colour = Av1Colour {
        matrix: MatrixCoefficients::Bt709,
        ..Av1Colour::default()
    };
    for bits in [BitDepth::Ten, BitDepth::Twelve] {
        for &q in &[4u8, 20, 40, 90, 160, 255] {
            for &(w, h) in &[(8, 8), (17, 13), (40, 24), (100, 70)] {
                let src = planes16(w, h, bits, texture16(bits));
                check_with(encode_still_intra16_with(&src, q, colour).unwrap(), q);
            }
        }
    }
}

#[test]
fn high_bit_depth_flat_and_two_tone_content_matches_both_decoders() {
    // The skip and palette paths. A two-tone block is coded as a palette whose first colour is
    // `L(BitDepth)` bits and whose deltas are `BitDepth - 3 + 3` bits wide — the widths that were
    // hardcoded to 8. Values at the extremes of the range make a too-narrow delta overflow.
    for bits in [BitDepth::Ten, BitDepth::Twelve] {
        let max = bits.max_value();
        for &q in &[4u8, 40, 160] {
            check_with(
                encode_still_intra16_with(
                    &planes16(48, 32, bits, |_, _| [max, 0, max / 2]),
                    q,
                    Av1Colour::default(),
                )
                .unwrap(),
                q,
            );
            check_with(
                encode_still_intra16_with(
                    &planes16(48, 32, bits, |x, y| {
                        let v = if (x / 8 + y / 8).is_multiple_of(2) {
                            max
                        } else {
                            0
                        };
                        [v, v, v]
                    }),
                    q,
                    Av1Colour::default(),
                )
                .unwrap(),
                q,
            );
        }
    }
}

#[test]
fn high_bit_depth_monochrome_matches_both_decoders() {
    // Monochrome at 12 bits is `seq_profile = 2` with both `mono_chrome` and `twelve_bit` coded —
    // the one combination where §5.5.2's depth branch and its monochrome branch both fire.
    for bits in [BitDepth::Ten, BitDepth::Twelve] {
        let max = bits.max_value();
        for &q in &[0u8, 20, 90, 255] {
            let mut y = vec![0u16; 40 * 24];
            for row in 0..24u32 {
                for col in 0..40u32 {
                    y[(row * 40 + col) as usize] =
                        (((col * 37 + row * 11) % 256) * u32::from(max) / 255) as u16;
                }
            }
            let src = Planar16::from_planes_subsampled(
                40,
                24,
                ChromaSubsampling::Cs400,
                bits,
                [y, Vec::new(), Vec::new()],
            )
            .unwrap();
            check_with(
                encode_still_intra16_with(&src, q, Av1Colour::monochrome()).unwrap(),
                q,
            );
        }
    }
}

#[test]
fn an_eight_bit_planar16_encodes_exactly_as_planar8_does() {
    // The widening is meant to be transparent: `Planar16` at `BitDepth::Eight` carries the same
    // samples through the same coding path, so it must emit the *same bytes* as the 8-bit entry
    // point. This is what pins `PlaneSource` as a carrier choice rather than a behaviour change —
    // any depth-derived constant that leaked a different value at 8 bits would show up here.
    let texture = |x: u32, y: u32| {
        let r = (x.wrapping_mul(3).wrapping_add(y) % 256) as u8;
        let g = ((x + y.wrapping_mul(2)) % 256) as u8;
        let b = (128 + ((x ^ y) % 64)) as u8;
        [r, g, b]
    };
    for &q in &[0u8, 20, 90] {
        for &(w, h) in &[(17, 13), (40, 24)] {
            let eight = planes(w, h, texture);
            // The *same* planes, widened — not the same RGB re-mapped, which would silently permute
            // them (`planes` maps RGB to GBR; `planes16` is plane-direct).
            let wide = Planar16::from_planes(
                w,
                h,
                BitDepth::Eight,
                std::array::from_fn(|i| eight.plane(i).iter().copied().map(u16::from).collect()),
            )
            .unwrap();
            let a = encode_still_intra_with(&eight, q, Av1Colour::default()).unwrap();
            let b = encode_still_intra16_with(&wide, q, Av1Colour::default()).unwrap();
            assert_eq!(a.0.obus, b.0.obus, "{w}x{h} q{q}");
            assert_eq!(a.1.planes, b.1.planes, "{w}x{h} q{q}");
        }
    }
}

#[test]
fn sixteen_bit_samples_are_rejected() {
    // `BitDepth::Sixteen` is a `gamut-color` depth for the interleaved 16-bit pipelines, not an AV1
    // one — §6.4.1 defines 8, 10 and 12. It is refused rather than silently coded as 12.
    let src = planes16(8, 8, BitDepth::Sixteen, |_, _| [65535, 0, 30000]);
    let err = encode_still_intra16_with(&src, 40, Av1Colour::default())
        .expect_err("16-bit samples are not an AV1 depth");
    assert_eq!(
        err.static_message(),
        Some("AV1: only 8-, 10- and 12-bit samples are coded (§6.4.1)")
    );
}

/// Four content quadrants, one per tool the speed presets prune: texture (partitioning and the
/// transform-type search), a few flat-chroma luma colours (palette), diagonal stripes (directional
/// modes and their angle refinement), and a smooth ramp whose chroma tracks its luma (CfL and
/// filter-intra).
fn speed_mix(w: u32, h: u32) -> impl Fn(u32, u32) -> [u8; 3] {
    move |x: u32, y: u32| match (x < w / 2, y < h / 2) {
        (true, true) => textured(x, y),
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
    }
}

#[test]
fn every_speed_preset_matches_both_decoders() {
    // A preset narrows only which candidates the mode decisions *try*; signalling and
    // reconstruction of whatever is chosen are shared. So every preset must still decode
    // bit-exactly — and because each prunes a different set, each reaches a different mix of
    // modes, partitions and transforms, which is exactly what the reconstruction must follow.
    let ycc = colour_for(MatrixCoefficients::Bt709, ColorRange::Full);
    for &speed in Speed::ALL {
        for &q in &[8u8, 60, 160] {
            for &(w, h) in &[(64, 64), (100, 70), (160, 96)] {
                let mix = speed_mix(w, h);
                let rgb = planes(w, h, &mix);
                check_with(
                    encode_still_intra_with_speed(&rgb, q, Av1Colour::default(), speed).unwrap(),
                    q,
                );
                for ss in [ChromaSubsampling::Cs420, ChromaSubsampling::Cs422] {
                    let src = planes_subsampled(
                        w,
                        h,
                        ss,
                        MatrixCoefficients::Bt709,
                        ColorRange::Full,
                        &mix,
                    );
                    check_with(
                        encode_still_intra_with_speed(&src, q, ycc, speed).unwrap(),
                        q,
                    );
                }
                let mono = mono_planes(w, h, |x, y| mix(x, y)[0]);
                check_with(
                    encode_still_intra_with_speed(&mono, q, Av1Colour::monochrome(), speed)
                        .unwrap(),
                    q,
                );
                let deep = planes16(w, h, BitDepth::Ten, |x, y| {
                    mix(x, y).map(|v| u16::from(v) << 2)
                });
                check_with(
                    encode_still_intra16_with_speed(&deep, q, ycc, speed).unwrap(),
                    q,
                );
            }
        }
    }
}
