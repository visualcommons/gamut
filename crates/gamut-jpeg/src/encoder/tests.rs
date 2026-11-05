//! The encoder's cross-stage suite: [`luma_factors`], and the scans decoded back to quantized
//! coefficients by one in-crate test decoder — the entropy coder's DC prediction and AC run-length
//! invariants, and the reference-pipeline tests that compare every block of an encoded image
//! against an independent transform. The decoder is shared, so the tests that use it stay together.

use gamut_color::{ColorRange, bt601_rgb_to_ycbcr};
use gamut_core::{Dimensions, EncodeImage, ImageRef, Rgb8};
use gamut_dsp::jpeg::forward_dct8x8;
use gamut_dsp::math::round_div_nearest;

use super::block::{Plane, downsample};
use super::entropy::{BaselineCoder, Component, encode_scan, standard_tables};
use super::*;
use crate::huffman::{self, EncTable};
use crate::zigzag::ZIGZAG;
use crate::{marker, quant};

#[test]
fn luma_factors_cover_the_four_jpeg_grids_and_refuse_the_rest() {
    assert_eq!(luma_factors(ChromaSubsampling::Cs444).unwrap(), (1, 1));
    assert_eq!(luma_factors(ChromaSubsampling::Cs422).unwrap(), (2, 1));
    assert_eq!(luma_factors(ChromaSubsampling::Cs420).unwrap(), (2, 2));
    assert_eq!(luma_factors(ChromaSubsampling::Cs440).unwrap(), (1, 2));
    assert_eq!(
        luma_factors(ChromaSubsampling::Cs400)
            .unwrap_err()
            .to_string(),
        "unsupported: JPEG: colour subsampling must be 4:4:4, 4:2:2, 4:2:0 or 4:4:0 [origin: gamut-jpeg]"
    );
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

// --- Reference-pipeline tests: encode per-pixel-distinct images, decode the scan with the
// test decoder, and compare every block against an independently computed expectation
// (gather with edge replication → forward_dct8x8 → round_div_nearest quantize). Distinct
// content is the point: any mutated pixel/block/MCU coordinate reads different samples
// somewhere and diverges. Solid colors could not see those mutants. ---

/// The test-side reference for one 8×8 block of `plane` at block coords `(bx, by)`: the same
/// §A.3.1/§A.3.3/§A.3.4 stages, written independently of the production gather.
fn reference_block(plane: &Plane, bx: usize, by: usize, quant: &[u8; 64]) -> [i32; 64] {
    let mut block = [0i32; 64];
    for (i, cell) in block.iter_mut().enumerate() {
        let x = (bx * 8 + i % 8).min(plane.width - 1);
        let y = (by * 8 + i / 8).min(plane.height - 1);
        *cell = i32::from(plane.data[y * plane.width + x]) - 128;
    }
    forward_dct8x8(&mut block);
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
        .with_subsampling(ChromaSubsampling::Cs444)
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
                bt601_rgb_to_ycbcr(rgb[3 * i], rgb[3 * i + 1], rgb[3 * i + 2], ColorRange::Full);
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
        .with_subsampling(ChromaSubsampling::Cs420)
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
            bt601_rgb_to_ycbcr(rgb[3 * i], rgb[3 * i + 1], rgb[3 * i + 2], ColorRange::Full);
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
