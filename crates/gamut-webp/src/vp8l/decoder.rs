//! VP8L image-data decoder: the full lossless decode path (RFC 9649 §3-§6).
//!
//! [`decode`] reads the header, the optional transform chain, and the entropy-coded image, then
//! inverts the transforms (last read first) to recover the ARGB pixels. The core
//! [`read_image`](self) routine is recursive and serves all five image *roles* (the main ARGB image,
//! the entropy image, the predictor/color sub-images, and the color-index table); sub-images pass
//! `allow_meta = false`, which forbids transforms and meta prefix codes and so bounds the recursion
//! to depth two (RFC 9649 §5.1).

use gamut_core::{Dimensions, Error, Result};

use crate::vp8l::bit_io::BitReader;
use crate::vp8l::color_cache::ColorCache;
use crate::vp8l::div_round_up;
use crate::vp8l::header::Vp8lHeader;
use crate::vp8l::lz77::{distance_code_to_pixel_distance, read_lz77_value};
use crate::vp8l::prefix::{PrefixCodeGroup, read_prefix_code_group};
use crate::vp8l::transform::{
    COLOR_INDEXING_TRANSFORM, COLOR_TRANSFORM, PREDICTOR_TRANSFORM, SUBTRACT_GREEN_TRANSFORM,
    Vp8lTransform, add_pixels, color_index_width_bits, make_argb,
};

/// First green-alphabet symbol value that denotes an LZ77 length code (literals occupy `0..256`).
const LENGTH_CODE_BASE: u16 = 256;
/// First green-alphabet symbol value that denotes a color-cache index.
const CACHE_CODE_BASE: u16 = 256 + 24;
/// The maximum number of pixels we will allocate for one image (guards a hostile width×height).
const MAX_PIXELS: usize = 16384 * 16384;

/// Decodes a complete VP8L bitstream (the VP8L chunk payload, signature byte included) into ARGB
/// pixels in scan order.
///
/// # Errors
///
/// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error for any
/// malformed, truncated, or over-spec stream.
pub fn decode(data: &[u8]) -> Result<(Dimensions, Vec<u32>)> {
    let mut r = BitReader::new(data);
    let header = Vp8lHeader::read(&mut r)?;
    let (width, height) = (u32::from(header.width), u32::from(header.height));
    let pixels = decode_image(&mut r, width, height)?;
    Ok((Dimensions::new(width, height)?, pixels))
}

/// Decodes a **headerless** VP8L image-stream of the given dimensions: the transform chain followed by
/// the spatially-coded image (RFC 9649 §3/§4) — the body of [`decode`] after the header. This is also
/// the form a lossless-compressed `ALPH` chunk takes, whose dimensions are implicit (§2.7.1).
///
/// # Errors
///
/// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error for any
/// malformed, truncated, or over-spec stream.
pub(crate) fn decode_image(r: &mut BitReader<'_>, width: u32, height: u32) -> Result<Vec<u32>> {
    // Read the transform chain (each type at most once), tracking the working width that
    // color-indexing shrinks for everything read after it (RFC 9649 §4).
    let mut transforms: Vec<Vp8lTransform> = Vec::new();
    let mut seen = [false; 4];
    let mut work_width = width;
    while r.read_bit()? == 1 {
        let transform_type = r.read_bits(2)? as usize;
        if seen[transform_type] {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "VP8L: transform type repeated",
            ));
        }
        seen[transform_type] = true;
        let transform = read_transform(r, transform_type as u8, work_width, height)?;
        if let Vp8lTransform::ColorIndexing { table, .. } = &transform {
            let width_bits = color_index_width_bits(table.len());
            work_width = div_round_up(work_width, 1 << width_bits);
        }
        transforms.push(transform);
    }

    // Decode the (possibly width-reduced) main image, then invert the transforms in reverse order.
    let mut pixels = read_image(r, work_width, height, true)?;
    let mut current_width = work_width;
    for transform in transforms.iter().rev() {
        current_width = transform.apply_inverse(&mut pixels, current_width, height);
    }
    debug_assert_eq!(current_width, width);
    Ok(pixels)
}

/// Reads one transform's data, given the working `width`/`height` at this point in the stream.
fn read_transform(
    r: &mut BitReader<'_>,
    transform_type: u8,
    width: u32,
    height: u32,
) -> Result<Vp8lTransform> {
    match transform_type {
        PREDICTOR_TRANSFORM | COLOR_TRANSFORM => {
            let size_bits = r.read_bits(3)? as u8 + 2;
            let block_xsize = div_round_up(width, 1 << size_bits);
            let block_ysize = div_round_up(height, 1 << size_bits);
            let blocks = read_image(r, block_xsize, block_ysize, false)?;
            if transform_type == PREDICTOR_TRANSFORM {
                Ok(Vp8lTransform::Predictor { size_bits, blocks })
            } else {
                Ok(Vp8lTransform::Color { size_bits, blocks })
            }
        }
        SUBTRACT_GREEN_TRANSFORM => Ok(Vp8lTransform::SubtractGreen),
        COLOR_INDEXING_TRANSFORM => {
            let table_size = r.read_bits(8)? as usize + 1;
            let mut table = read_image(r, table_size as u32, 1, false)?;
            // The table is subtraction-coded: prefix-sum each entry onto the previous (RFC 9649 §4.4).
            for i in 1..table.len() {
                table[i] = add_pixels(table[i], table[i - 1]);
            }
            Ok(Vp8lTransform::ColorIndexing {
                table,
                expanded_width: width,
            })
        }
        _ => Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "VP8L: unknown transform type",
        )),
    }
}

/// The per-image prefix-code state: the meta-prefix (entropy) image plus the prefix-code groups.
struct HuffmanInfo {
    /// Per-block meta prefix codes (empty when a single group covers the whole image).
    entropy: Vec<u32>,
    /// Width of the entropy image, in blocks.
    xsize: u32,
    /// Block-size exponent for entropy-image lookups.
    bits: u32,
    /// The prefix-code groups (length `num_prefix_groups`).
    groups: Vec<PrefixCodeGroup>,
}

/// Decodes one image (any role) into ARGB pixels. `allow_meta` is true only for the top-level ARGB
/// image; sub-images set it false (no meta prefix codes).
fn read_image(
    r: &mut BitReader<'_>,
    width: u32,
    height: u32,
    allow_meta: bool,
) -> Result<Vec<u32>> {
    let num_pixels = (width as usize)
        .checked_mul(height as usize)
        .filter(|&n| n <= MAX_PIXELS)
        .ok_or_else(|| Error::invalid_input(env!("CARGO_PKG_NAME"), "VP8L: image too large"))?;

    let mut color_cache = read_color_cache_info(r)?;
    let cache_size = color_cache.as_ref().map_or(0, ColorCache::size);
    let huffman = read_huffman_codes(r, width, height, cache_size, allow_meta)?;

    decode_image_data(r, width, height, num_pixels, &mut color_cache, &huffman)
}

/// Reads the `color-cache-info` field (RFC 9649 §3.6.3): a 1-bit flag and, if set, the 4-bit size.
fn read_color_cache_info(r: &mut BitReader<'_>) -> Result<Option<ColorCache>> {
    if r.read_bit()? == 1 {
        let bits = r.read_bits(4)?;
        Ok(Some(ColorCache::new(bits)?))
    } else {
        Ok(None)
    }
}

/// Reads the meta-prefix bit, the optional entropy image, and the prefix-code groups (RFC 9649
/// §6.2.1-§6.2.2).
fn read_huffman_codes(
    r: &mut BitReader<'_>,
    width: u32,
    height: u32,
    cache_size: usize,
    allow_meta: bool,
) -> Result<HuffmanInfo> {
    let mut entropy = Vec::new();
    let mut xsize = 1;
    let mut bits = 0;
    let mut num_groups = 1u32;

    if allow_meta && r.read_bit()? == 1 {
        bits = r.read_bits(3)? + 2;
        xsize = div_round_up(width, 1 << bits);
        let ysize = div_round_up(height, 1 << bits);
        let image = read_image(r, xsize, ysize, false)?;
        // The meta prefix code lives in the red+green channels: (pixel >> 8) & 0xffff.
        let mut max_meta = 0u32;
        entropy = image
            .iter()
            .map(|&p| {
                let meta = (p >> 8) & 0xffff;
                max_meta = max_meta.max(meta);
                meta
            })
            .collect();
        num_groups = max_meta + 1;
    }

    let mut groups = Vec::new();
    for _ in 0..num_groups {
        groups.push(read_prefix_code_group(r, cache_size)?);
    }

    Ok(HuffmanInfo {
        entropy,
        xsize,
        bits,
        groups,
    })
}

/// Decodes the entropy-coded pixel stream: literals, LZ77 backward references, and color-cache codes
/// (RFC 9649 §5.2, §6.2.3).
fn decode_image_data(
    r: &mut BitReader<'_>,
    width: u32,
    height: u32,
    num_pixels: usize,
    color_cache: &mut Option<ColorCache>,
    huffman: &HuffmanInfo,
) -> Result<Vec<u32>> {
    let _ = height;
    let mut pixels = vec![0u32; num_pixels];
    let width_usize = width as usize;
    let multi_group = huffman.groups.len() > 1;
    let mut i = 0usize;

    while i < num_pixels {
        let group = if multi_group {
            let x = (i % width_usize) as u32;
            let y = (i / width_usize) as u32;
            let pos = (y >> huffman.bits) * huffman.xsize + (x >> huffman.bits);
            let meta = huffman.entropy.get(pos as usize).copied().unwrap_or(0);
            huffman.groups.get(meta as usize)
        } else {
            huffman.groups.first()
        }
        .ok_or_else(|| {
            Error::invalid_input(env!("CARGO_PKG_NAME"), "VP8L: missing prefix code group")
        })?;

        let symbol = group.green.read_symbol(r)?;
        if symbol < LENGTH_CODE_BASE {
            let g = symbol as u8;
            let red = group.red.read_symbol(r)? as u8;
            let blue = group.blue.read_symbol(r)? as u8;
            let alpha = group.alpha.read_symbol(r)? as u8;
            let argb = make_argb(alpha, red, g, blue);
            pixels[i] = argb;
            if let Some(cache) = color_cache.as_mut() {
                cache.insert(argb);
            }
            i += 1;
        } else if symbol < CACHE_CODE_BASE {
            let length = read_lz77_value(r, u32::from(symbol - LENGTH_CODE_BASE))?;
            let dist_symbol = group.distance.read_symbol(r)?;
            let dist_code = read_lz77_value(r, u32::from(dist_symbol))?;
            let dist = distance_code_to_pixel_distance(dist_code, width) as usize;
            if dist == 0 || dist > i {
                return Err(Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "VP8L: backward reference before image start",
                ));
            }
            let end = i
                .checked_add(length as usize)
                .filter(|&e| e <= num_pixels)
                .ok_or_else(|| {
                    Error::invalid_input(
                        env!("CARGO_PKG_NAME"),
                        "VP8L: backward reference past image end",
                    )
                })?;
            while i < end {
                let argb = pixels[i - dist];
                pixels[i] = argb;
                if let Some(cache) = color_cache.as_mut() {
                    cache.insert(argb);
                }
                i += 1;
            }
        } else {
            let Some(cache) = color_cache.as_mut() else {
                return Err(Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "VP8L: cache code without color cache",
                ));
            };
            let index = u32::from(symbol - CACHE_CODE_BASE);
            let argb = cache.lookup(index);
            cache.insert(argb);
            pixels[i] = argb;
            i += 1;
        }
    }

    Ok(pixels)
}

/// Converts a decoded ARGB buffer to interleaved 8-bit RGB, appending to `out`.
pub(crate) fn argb_to_rgb8(argb: &[u32], out: &mut Vec<u8>) {
    for &p in argb {
        out.push((p >> 16) as u8);
        out.push((p >> 8) as u8);
        out.push(p as u8);
    }
}

/// Converts a decoded `0xAARRGGBB` ARGB buffer to interleaved 8-bit RGBA (keeping alpha), appending
/// to `out`.
pub(crate) fn argb_to_rgba8(argb: &[u32], out: &mut Vec<u8>) {
    for &p in argb {
        out.push((p >> 16) as u8); // R
        out.push((p >> 8) as u8); // G
        out.push(p as u8); // B
        out.push((p >> 24) as u8); // A
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vp8l::bit_io::BitWriter;
    use crate::vp8l::header::Vp8lHeader;
    use crate::vp8l::prefix::write_simple_prefix_code;
    use crate::vp8l::transform::{alpha, blue, green, red};

    /// Builds a minimal VP8L stream of a single solid color: no transforms, no cache, one group of
    /// five single-symbol codes (so the pixel data section is empty — single-symbol codes consume no
    /// bits). Exercises the header / transform-loop / cache-info / meta / group-read / literal paths.
    fn solid_color_stream(width: u32, height: u32, color: u32) -> Vec<u8> {
        let mut w = BitWriter::new();
        let header = Vp8lHeader::from_dimensions(
            Dimensions::new(width, height).unwrap(),
            alpha(color) != 0xff,
        )
        .expect("valid dims");
        header.write(&mut w);
        w.write_bits(0, 1); // no transforms
        w.write_bits(0, 1); // no color cache
        w.write_bits(0, 1); // single meta prefix code
        write_simple_prefix_code(&mut w, &[u16::from(green(color))]);
        write_simple_prefix_code(&mut w, &[u16::from(red(color))]);
        write_simple_prefix_code(&mut w, &[u16::from(blue(color))]);
        write_simple_prefix_code(&mut w, &[u16::from(alpha(color))]);
        write_simple_prefix_code(&mut w, &[0]); // distance (unused)
        w.finish()
    }

    #[test]
    fn decodes_solid_color_image() {
        for (width, height) in [(1u32, 1u32), (2, 2), (5, 3)] {
            let color = make_argb(0xff, 0x12, 0x34, 0x56);
            let stream = solid_color_stream(width, height, color);
            let (dims, pixels) = decode(&stream).expect("decode");
            assert_eq!(dims, Dimensions::new(width, height).unwrap());
            assert_eq!(pixels.len(), (width * height) as usize);
            assert!(pixels.iter().all(|&p| p == color));
        }
    }

    #[test]
    fn green_symbol_at_length_code_base_is_a_length_code_not_a_literal() {
        // RFC 9649 §5.2.2: green symbols 0..256 are literals; 256 (LENGTH_CODE_BASE) is the first
        // LZ77 *length* code. A single-symbol green code that always yields 256 makes the very
        // first pixel a backward reference, which is invalid before the image starts — a correct
        // decoder rejects it. If the literal/length split were `<=` instead of `<`, symbol 256
        // would be mis-decoded as a literal pixel and the stream would decode "successfully".
        // (`write_simple_prefix_code` cannot encode symbol 256, hence the direct group build.)
        use crate::vp8l::prefix::PrefixCode;
        let single = |alphabet: usize, sym: usize| {
            let mut lengths = vec![0u8; alphabet];
            lengths[sym] = 1;
            PrefixCode::from_code_lengths(&lengths).expect("single-symbol code")
        };
        let group = PrefixCodeGroup {
            green: single(280, 256), // 256 literals + 24 length codes, no color cache
            red: single(256, 0),
            blue: single(256, 0),
            alpha: single(256, 0),
            distance: single(40, 0),
        };
        let huffman = HuffmanInfo {
            entropy: Vec::new(),
            xsize: 1,
            bits: 0,
            groups: vec![group],
        };
        let mut r = BitReader::new(&[]);
        let result = decode_image_data(&mut r, 4, 1, 4, &mut None, &huffman);
        assert!(
            result.is_err(),
            "green symbol 256 is the first length code; as pixel 0 it is a backward reference \
             before image start and must be rejected, not decoded as a literal"
        );
    }

    #[test]
    fn a_single_prefix_group_bypasses_the_meta_huffman_image() {
        // The `groups.len() > 1` guard lets a single-group image skip the per-pixel meta-Huffman
        // lookup (RFC 9649 §5.2.2 / §6.2.3): with one group the decoder uses it directly. If the
        // guard were `>= 1` it would consult the meta image even here and index a non-existent
        // group. (A real reader only builds one group when there is no meta image, so this pins
        // the guard by pairing the lone group with a deliberately out-of-range meta entry.)
        use crate::vp8l::prefix::PrefixCode;
        let single = |alphabet: usize, sym: usize| {
            let mut lengths = vec![0u8; alphabet];
            lengths[sym] = 1;
            PrefixCode::from_code_lengths(&lengths).expect("single-symbol code")
        };
        let group = PrefixCodeGroup {
            green: single(280, 0), // symbol 0 < 256 → a literal
            red: single(256, 0),
            blue: single(256, 0),
            alpha: single(256, 0),
            distance: single(40, 0),
        };
        let huffman = HuffmanInfo {
            entropy: vec![1], // selects group index 1, which does not exist
            xsize: 1,
            bits: 0,
            groups: vec![group],
        };
        let mut r = BitReader::new(&[]);
        let pixels = decode_image_data(&mut r, 4, 1, 4, &mut None, &huffman)
            .expect("a single group is used directly, ignoring the meta image");
        assert_eq!(pixels, vec![make_argb(0, 0, 0, 0); 4]);
    }

    #[test]
    fn argb_to_rgb8_drops_alpha() {
        let mut out = Vec::new();
        argb_to_rgb8(
            &[make_argb(0xff, 1, 2, 3), make_argb(0x00, 4, 5, 6)],
            &mut out,
        );
        assert_eq!(out, [1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn rejects_bad_signature() {
        assert!(matches!(
            decode(&[0x00, 0, 0, 0, 0]),
            Err(error) if error.kind() == gamut_core::ErrorKind::InvalidInput
        ));
    }

    #[test]
    fn rejects_truncated_stream() {
        // Valid signature, but the stream ends mid-header.
        assert!(matches!(
            decode(&[0x2f, 0x00]),
            Err(error) if error.kind() == gamut_core::ErrorKind::InvalidInput
        ));
    }

    #[test]
    fn rejects_repeated_transform() {
        let mut w = BitWriter::new();
        Vp8lHeader::from_dimensions(Dimensions::new(4, 4).unwrap(), false)
            .unwrap()
            .write(&mut w);
        // Two subtract-green transforms in a row is illegal.
        w.write_bits(1, 1);
        w.write_bits(u32::from(SUBTRACT_GREEN_TRANSFORM), 2);
        w.write_bits(1, 1);
        w.write_bits(u32::from(SUBTRACT_GREEN_TRANSFORM), 2);
        let bytes = w.finish();
        assert!(matches!(
            decode(&bytes),
            Err(error) if error.kind() == gamut_core::ErrorKind::InvalidInput
        ));
    }

    #[test]
    fn solid_color_with_transparency_round_trips() {
        let color = make_argb(0x40, 0xaa, 0xbb, 0xcc); // non-opaque alpha
        let stream = solid_color_stream(3, 2, color);
        let (_, pixels) = decode(&stream).expect("decode");
        assert!(pixels.iter().all(|&p| p == color));
    }

    #[test]
    fn rejects_out_of_range_color_cache_bits() {
        // color-cache-info with size bits = 0 is invalid (valid range is 1..=11).
        let mut w = BitWriter::new();
        Vp8lHeader::from_dimensions(Dimensions::new(4, 4).unwrap(), false)
            .unwrap()
            .write(&mut w);
        w.write_bits(0, 1); // no transforms
        w.write_bits(1, 1); // use color cache
        w.write_bits(0, 4); // cache_code_bits = 0 → invalid
        let bytes = w.finish();
        assert!(matches!(
            decode(&bytes),
            Err(error) if error.kind() == gamut_core::ErrorKind::InvalidInput
        ));
    }

    #[test]
    fn decode_never_panics_on_arbitrary_payloads() {
        // A valid small header followed by pseudo-random bits: every malformed transform / prefix
        // code / image-data path must surface as `Err`, never a panic. Bounded dimensions keep the
        // pixel allocation tiny regardless of the random bits.
        let mut header = BitWriter::new();
        Vp8lHeader::from_dimensions(Dimensions::new(4, 4).unwrap(), false)
            .unwrap()
            .write(&mut header);
        let header = header.finish();

        let mut state = 0x1234_5678_9abc_def0u64;
        for _ in 0..5000 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let len = (state % 48) as usize + 1;
            let mut bytes = header.clone();
            for k in 0..len {
                bytes.push((state >> ((k % 8) * 8)) as u8);
            }
            // Result is intentionally ignored; the point is that it returns rather than panics.
            let _ = decode(&bytes);
        }
    }
}
