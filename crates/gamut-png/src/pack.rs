//! Sub-byte sample packing for 1-, 2-, and 4-bit PNG images (PNG spec §7.2).
//!
//! For bit depths below 8, samples are packed most-significant-bit-first within each byte (the
//! leftmost pixel occupies the high bits), and every scanline is padded to a byte boundary with
//! zero bits. 8- and 16-bit samples need no packing.

/// The exact §13.12 factor presenting a sub-byte grey sample at 8 bits: 255 / (2^depth − 1).
/// Shared by the decoder (widening) and the reduce analysis (its inverse, sub-byte packing).
pub(crate) fn gray8_scale(bit_depth: u8) -> u8 {
    match bit_depth {
        1 => 255,
        2 => 85,
        4 => 17,
        _ => 1,
    }
}

/// Packs one-byte-per-sample `samples` (each value `< 2^bit_depth`) into MSB-first bit-packed,
/// byte-padded scanlines. `bit_depth` must be 1, 2, or 4.
pub fn pack_scanlines(samples: &[u8], width: usize, height: usize, bit_depth: u8) -> Vec<u8> {
    debug_assert!(matches!(bit_depth, 1 | 2 | 4));
    let depth = bit_depth as usize;
    let per_byte = 8 / depth; // samples packed per output byte: 8, 4, or 2
    let mask = (1u8 << depth) - 1;
    let row_bytes = width.div_ceil(per_byte);
    let mut out = vec![0u8; row_bytes * height];
    for y in 0..height {
        let row = &samples[y * width..(y + 1) * width];
        let dst = &mut out[y * row_bytes..(y + 1) * row_bytes];
        for (x, &value) in row.iter().enumerate() {
            let slot = x % per_byte; // 0 = leftmost = most significant
            let shift = 8 - depth * (slot + 1);
            dst[x / per_byte] |= (value & mask) << shift;
        }
    }
    out
}

/// Unpacks MSB-first bit-packed, byte-padded scanlines into one byte per sample (values left
/// unscaled) — the decoder's inverse of [`pack_scanlines`]. `packed` must hold exactly
/// `width.div_ceil(8 / bit_depth) * height` bytes; `bit_depth` must be 1, 2, or 4.
pub(crate) fn unpack_scanlines(
    packed: &[u8],
    width: usize,
    height: usize,
    bit_depth: u8,
) -> Vec<u8> {
    debug_assert!(matches!(bit_depth, 1 | 2 | 4));
    let depth = bit_depth as usize;
    let per_byte = 8 / depth; // samples per packed byte: 8, 4, or 2
    let mask = (1u8 << depth) - 1;
    let row_bytes = width.div_ceil(per_byte);
    debug_assert_eq!(packed.len(), row_bytes * height);
    let mut out = vec![0u8; width * height];
    for y in 0..height {
        let src = &packed[y * row_bytes..(y + 1) * row_bytes];
        let dst = &mut out[y * width..(y + 1) * width];
        for (x, value) in dst.iter_mut().enumerate() {
            let slot = x % per_byte; // 0 = leftmost = most significant
            let shift = 8 - depth * (slot + 1);
            *value = (src[x / per_byte] >> shift) & mask;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packs_one_bit_msb_first() {
        // 8 pixels -> 1 byte, leftmost pixel in bit 7.
        let bits = [1, 0, 1, 1, 0, 0, 0, 1];
        assert_eq!(pack_scanlines(&bits, 8, 1, 1), vec![0b1011_0001]);
    }

    #[test]
    fn packs_two_and_four_bit() {
        // 2-bit: four samples per byte, high slot first.
        assert_eq!(
            pack_scanlines(&[0b11, 0b10, 0b01, 0b00], 4, 1, 2),
            vec![0b11_10_01_00]
        );
        // 4-bit: two samples per byte.
        assert_eq!(pack_scanlines(&[0xA, 0x5, 0xF], 3, 1, 4), vec![0xA5, 0xF0]);
    }

    #[test]
    fn pads_each_row_to_a_byte_boundary() {
        // width 3 at 1 bit/px -> 1 byte/row, trailing bits zero; two rows -> 2 bytes.
        let samples = [1, 1, 1, 1, 0, 1];
        let packed = pack_scanlines(&samples, 3, 2, 1);
        assert_eq!(packed, vec![0b1110_0000, 0b1010_0000]);
    }

    #[test]
    fn unpack_golden_vectors() {
        // MSB-first: the leftmost sample sits in the high bits; rows are independently padded.
        assert_eq!(
            unpack_scanlines(&[0b1011_0001], 8, 1, 1),
            vec![1, 0, 1, 1, 0, 0, 0, 1]
        );
        assert_eq!(
            unpack_scanlines(&[0b11_10_01_00], 4, 1, 2),
            vec![0b11, 0b10, 0b01, 0b00]
        );
        assert_eq!(
            unpack_scanlines(&[0xA5, 0xF0], 3, 1, 4),
            vec![0xA, 0x5, 0xF]
        );
        // Padding bits are dropped, per row.
        assert_eq!(
            unpack_scanlines(&[0b1110_0000, 0b1010_0000], 3, 2, 1),
            vec![1, 1, 1, 1, 0, 1]
        );
    }

    #[test]
    fn unpack_inverts_pack() {
        for depth in [1u8, 2, 4] {
            let max = (1u16 << depth) as usize;
            for width in 1..=17usize {
                let height = 3;
                let samples: Vec<u8> = (0..width * height).map(|i| (i % max) as u8).collect();
                let packed = pack_scanlines(&samples, width, height, depth);
                assert_eq!(
                    unpack_scanlines(&packed, width, height, depth),
                    samples,
                    "depth {depth} width {width}"
                );
            }
        }
    }
}
