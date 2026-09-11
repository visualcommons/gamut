//! Lossless colour-type / bit-depth reductions (the PNG-side space optimisation).
//!
//! Before encoding, an image is scanned for redundancy that a smaller PNG encoding can drop without
//! changing any pixel: an all-opaque alpha channel, identical R=G=B channels, a palette of ≤256
//! distinct colours, grey values exactly representable at a sub-byte depth (§13.12), or 16-bit
//! samples whose high and low bytes agree (lossless 16→8 demotion). Every reduction is exactly
//! reversible, so the decoded pixels are unchanged — the libpng oracle verifies this.
//!
//! The analysis ranks the candidates by *estimated* raw byte count (sub-byte row padding is
//! ignored, as in the palette estimate) and returns a [`Reductions`] rather than a single winner,
//! because a raw byte count cannot see DEFLATE. A palette or a colour key carries a `PLTE`/`tRNS`
//! chunk that DEFLATE cannot touch, so the estimate's winner can lose the finished file to a
//! candidate it beat on raw bytes. Whenever the estimate picks a chunk-carrying candidate the
//! best *chunk-free* one is handed over beside it, and
//! [`PngEncoder`](crate::PngEncoder) encodes both — and the unreduced image — and keeps the
//! smallest.

use std::collections::HashMap;
use std::collections::hash_map::Entry;

use crate::pack::gray8_scale;

/// A chosen reduced encoding for an image.
pub enum Reduced {
    /// Greyscale at depth 1, 2, 4, or 8 (R=G=B, fully opaque). `samples` holds one byte per pixel:
    /// the raw value at depth 8, the unscaled code (`value / gray8_scale(depth)`) below it.
    Gray {
        /// Grey bit depth (1, 2, 4, or 8).
        depth: u8,
        /// One sample per pixel (one byte each, pre-packing).
        samples: Vec<u8>,
    },
    /// 8-bit greyscale + alpha (R=G=B with transparency).
    GrayAlpha8(Vec<u8>),
    /// 8-bit RGB (alpha was fully opaque and dropped).
    Rgb8(Vec<u8>),
    /// 8-bit RGBA: a 16-bit input demoted losslessly, with no further channel reduction.
    Rgba8(Vec<u8>),
    /// 16-bit greyscale (R=G=B, fully opaque), pre-serialised big-endian.
    Gray16Be(Vec<u8>),
    /// 16-bit greyscale + alpha (R=G=B with transparency), pre-serialised big-endian.
    GrayAlpha16Be(Vec<u8>),
    /// 16-bit RGB (alpha was fully opaque and dropped), pre-serialised big-endian.
    Rgb16Be(Vec<u8>),
    /// 8-bit RGB plus a `tRNS` colour key (§11.3.2.1): the alpha channel was binary, every
    /// transparent pixel shared one colour, and no opaque pixel used it, so that colour can stand
    /// for "transparent" and the fourth channel disappears.
    Rgb8Keyed {
        /// One RGB triple per pixel.
        samples: Vec<u8>,
        /// The colour a decoder must render as fully transparent.
        key: [u8; 3],
    },
    /// Greyscale plus a `tRNS` colour key — the greyscale twin of [`Reduced::Rgb8Keyed`]. Always
    /// depth 8: a sub-byte depth would have to scale the key too, and the saving over depth 8 is
    /// smaller than the risk of getting that wrong.
    GrayKeyed {
        /// One grey sample per pixel.
        samples: Vec<u8>,
        /// The grey value a decoder must render as fully transparent.
        key: u8,
    },
    /// Indexed colour with the smallest sufficient bit depth.
    Indexed {
        /// Index bit depth (1, 2, 4, or 8).
        depth: u8,
        /// One palette index per pixel (one byte each, pre-packing).
        indices: Vec<u8>,
        /// PLTE payload (RGB triples).
        plte: Vec<u8>,
        /// tRNS payload (palette alphas), if the palette is not fully opaque.
        trns: Option<Vec<u8>>,
    },
}

/// What [`analyze8`] / [`analyze16`] offer the encoder for one image.
///
/// The estimate that ranks the candidates counts *raw* bytes, which does not predict compressed
/// size: a palette's `PLTE` (and often `tRNS`) and a colour key's `tRNS` are flat, incompressible
/// costs, while the samples they replace may compress by two orders of magnitude. So a
/// chunk-carrying winner is never trusted on its own — it arrives with the best candidate that
/// carries no chunk, and [`PngEncoder`](crate::PngEncoder) encodes both plus the unreduced image
/// and keeps the smallest file.
///
/// A chunk-free winner needs no such company: it adds nothing DEFLATE cannot compress, so the raw
/// comparison that chose it is sound and it is written directly.
pub enum Reductions {
    /// No reduction stores fewer bytes than the input layout: encode the image as it arrived.
    None,
    /// The estimate's winner adds no chunk, so it is the whole answer.
    ChunkFree(Reduced),
    /// The estimate's winner carries a chunk, so it must be raced.
    Chunked {
        /// The chunk-carrying candidate the estimate ranked smallest.
        chunked: Reduced,
        /// The smallest candidate that adds no chunk, or `None` when no chunk-free reduction
        /// stores fewer bytes than the input layout — the identity cases, where the "reduction"
        /// re-spells the input and racing it would compress the same samples twice.
        chunk_free: Option<Reduced>,
    },
}

/// The smallest indexed bit depth (1, 2, 4, or 8) that can address `palette_len` entries.
pub(crate) fn index_bit_depth(palette_len: usize) -> u8 {
    match palette_len {
        0..=2 => 1,
        3..=4 => 2,
        5..=16 => 4,
        _ => 8,
    }
}

/// Zeroes the colour channels of every fully transparent pixel, leaving alpha alone. Returns
/// `None` when the image has no fully transparent pixel to clean.
///
/// Nothing a decoder renders changes: at `alpha == 0` the colour channels are invisible by
/// definition. What changes is how well the image *compresses*, in three compounding ways:
///
/// 1. Transparent pixels all become identical, so `Sub` and `Paeth` filter a run of them to
///    zeros instead of to whatever noise the source happened to carry.
/// 2. [`analyze8`] keys its palette on the whole RGBA quad, so two invisible pixels that differ
///    only in their unseen colour cost two palette entries today. This collapses every
///    transparent pixel to a single entry.
/// 3. It is the precondition for a `tRNS` colour key, which needs one colour to stand for
///    "transparent".
///
/// One constant, not the neighbouring pixel's colour, and that choice was measured rather than
/// assumed. Inheriting the predecessor flattens a *run* just as well, but leaves every invisible
/// pixel a distinct RGBA quad, so (2) and (3) both fail: on an image alternating visible and
/// invisible pixels it collapsed nothing at all and saved zero bytes.
///
/// This is *not* lossless in the strict byte sense the rest of this module keeps -- the stored
/// samples change -- which is why it is opt-in via
/// [`PngEncoder::with_transparent_cleanup`](crate::PngEncoder::with_transparent_cleanup) and off
/// by default. `channels` must be 2 (grey + alpha) or 4 (RGBA); layouts without an alpha channel
/// have nothing to clean and return `None`.
pub(crate) fn clean_transparent(pixels: &[u8], channels: usize) -> Option<Vec<u8>> {
    debug_assert!((1..=4).contains(&channels));
    if !channels.is_multiple_of(2) {
        return None; // no alpha channel
    }
    let colour = channels - 1; // colour channels are everything before alpha
    if !pixels.chunks_exact(channels).any(|px| px[colour] == 0) {
        return None;
    }

    let mut out = pixels.to_vec();
    for px in out.chunks_exact_mut(channels) {
        if px[colour] == 0 {
            px[..colour].fill(0);
        }
    }
    Some(out)
}

/// The RGBA quad a pixel of any supported layout presents: grey replicates into R=G=B, and layouts
/// without an alpha channel (the odd channel counts) are opaque.
fn pixel_key(px: &[u8], channels: usize) -> [u8; 4] {
    let alpha = if channels.is_multiple_of(2) {
        px[channels - 1]
    } else {
        255
    };
    if channels >= 3 {
        [px[0], px[1], px[2], alpha]
    } else {
        [px[0], px[0], px[0], alpha]
    }
}

/// A `tRNS` chunk's cost for a greyscale image: one 16-bit sample plus 12 bytes of framing.
const GREY_KEY_COST: usize = 2 + 12;

/// A `tRNS` chunk's cost for truecolour: three 16-bit samples plus 12 bytes of framing.
const RGB_KEY_COST: usize = 6 + 12;

/// Whether a colour key could possibly apply, before paying for the scan that looks for one.
///
/// Needs an alpha channel to drop (`channels` even) and something for it to be carrying: an
/// all-opaque image is better served by the plain alpha *drop*, which costs no chunk at all.
///
/// Split out, like [`keyed_size`], because [`write_reduced_or_native`] races the winning estimate
/// against the unreduced encoding — so perturbing this decision usually changes which candidate is
/// *offered* without changing the bytes that finally win, which makes it invisible from outside.
///
/// [`write_reduced_or_native`]: crate::PngEncoder
fn may_have_colour_key(all_opaque: bool, channels: usize) -> bool {
    !all_opaque && channels.is_multiple_of(2)
}

/// Raw bytes a colour-key encoding costs: one sample per pixel for greyscale or three for
/// truecolour, plus the `tRNS` chunk that makes it lawful.
fn keyed_size(pixel_count: usize, all_gray: bool) -> usize {
    if all_gray {
        pixel_count + GREY_KEY_COST
    } else {
        pixel_count * 3 + RGB_KEY_COST
    }
}

/// The colour that can stand for "transparent", if a `tRNS` colour key applies at all.
///
/// Three conditions, all necessary (§11.3.2.1 gives a decoder exactly one transparent colour, not
/// a mask):
///
/// 1. every alpha is 0 or 255 — a partially transparent pixel cannot be expressed by a key;
/// 2. at least one pixel is transparent — otherwise the plain alpha *drop* already applies and is
///    strictly better, since it costs no chunk;
/// 3. every transparent pixel shares one colour, and **no opaque pixel uses it** — otherwise the
///    key would erase a pixel that should be visible.
///
/// Condition 3 is why
/// [`PngEncoder::with_transparent_cleanup`](crate::PngEncoder::with_transparent_cleanup) pairs
/// with this: it collapses every invisible pixel to one colour, which is precisely what a key
/// needs. Without it, a source whose transparent pixels carry different unseen colours has no key
/// available and keeps its alpha channel.
///
/// Condition 2 needs no check of its own: `candidate` is assigned in the `alpha == 0` arm and
/// nowhere else, so "some pixel is transparent" is exactly `candidate.is_some()` and the
/// `candidate?` below discharges it. Callers reach here only through
/// [`may_have_colour_key`], which already requires `!all_opaque`, and any alpha that is neither 0
/// nor 255 returns early — so in practice the `?` never fires; it is the total spelling of a
/// condition the caller gate has already established.
///
/// Two passes rather than one: the candidate is not known until the first transparent pixel is
/// seen, so proving no *earlier* opaque pixel used it needs a second look. The second pass only
/// runs when the first has already established a candidate.
fn colour_key(pixels: &[u8], channels: usize) -> Option<[u8; 4]> {
    debug_assert!(channels == 2 || channels == 4);
    let mut candidate: Option<[u8; 4]> = None;
    for px in pixels.chunks_exact(channels) {
        let key = pixel_key(px, channels);
        match key[3] {
            0 => match candidate {
                // A second transparent colour: no single key can stand for both.
                Some(seen) if seen[..3] != key[..3] => return None,
                Some(_) => {}
                None => candidate = Some(key),
            },
            255 => {}
            // Partial transparency cannot be expressed as a colour key.
            _ => return None,
        }
    }
    let candidate = candidate?;
    // The key must name a colour nothing visible uses.
    let collides = pixels.chunks_exact(channels).any(|px| {
        let key = pixel_key(px, channels);
        key[3] == 255 && key[..3] == candidate[..3]
    });
    (!collides).then_some(candidate)
}

/// Analyses interleaved 8-bit samples (`channels`: 1 = grey, 2 = grey+alpha, 3 = RGB, 4 = RGBA)
/// and returns the lossless reductions that beat the input encoding — see [`Reductions`] for why
/// that is one candidate or two rather than always one.
pub fn analyze8(pixels: &[u8], channels: usize) -> Reductions {
    debug_assert!((1..=4).contains(&channels));
    let pixel_count = pixels.len() / channels;

    let mut all_opaque = true;
    let mut all_gray = true;
    // Whether every grey value is exactly representable at 1/2/4 bits — a multiple of the depth's
    // §13.12 scale factor, not merely low-cardinality. Meaningful only while `all_gray` holds.
    let (mut fits1, mut fits2, mut fits4) = (true, true, true);
    let mut palette_index: HashMap<[u8; 4], u8> = HashMap::new();
    let mut palette: Vec<[u8; 4]> = Vec::new();
    let mut too_many_colors = false;
    for px in pixels.chunks_exact(channels) {
        let key = pixel_key(px, channels);
        all_opaque &= key[3] == 255;
        all_gray &= key[0] == key[1] && key[1] == key[2];
        fits1 &= key[0] == 0 || key[0] == 255;
        fits2 &= key[0].is_multiple_of(85);
        fits4 &= key[0].is_multiple_of(17);
        if !too_many_colors && let Entry::Vacant(slot) = palette_index.entry(key) {
            if palette.len() == 256 {
                too_many_colors = true;
            } else {
                slot.insert(palette.len() as u8);
                palette.push(key);
            }
        }
    }

    // Estimate the raw size (bytes before compression) of each viable encoding; smaller is better.
    let input_size = pixel_count * channels;
    let palette_size = if too_many_colors {
        usize::MAX
    } else {
        let depth = index_bit_depth(palette.len());
        let needs_trns = palette.iter().any(|c| c[3] != 255);
        let overhead = palette.len() * 3 + if needs_trns { palette.len() } else { 0 } + 24;
        pixel_count * depth as usize / 8 + overhead
    };
    let gray_depth = if fits1 {
        1
    } else if fits2 {
        2
    } else if fits4 {
        4
    } else {
        8
    };
    let gray_size = if all_gray && all_opaque {
        pixel_count * gray_depth as usize / 8
    } else {
        usize::MAX
    };
    let gray_alpha_size = if all_gray && !all_opaque {
        pixel_count * 2
    } else {
        usize::MAX
    };
    let rgb_size = if channels == 4 && all_opaque && !all_gray {
        pixel_count * 3
    } else {
        usize::MAX
    };
    let key = may_have_colour_key(all_opaque, channels)
        .then(|| colour_key(pixels, channels))
        .flatten();
    let keyed_size = key.map_or(usize::MAX, |_| keyed_size(pixel_count, all_gray));

    let best = palette_size
        .min(gray_size)
        .min(gray_alpha_size)
        .min(rgb_size)
        .min(keyed_size);
    if best >= input_size {
        return Reductions::None; // no reduction is smaller
    }

    // The chunk-free family: greyscale, grey+alpha, alpha drop. Their three gates are mutually
    // exclusive -- grey needs `all_gray && all_opaque`, grey+alpha `all_gray && !all_opaque`, the
    // alpha drop `all_opaque && !all_gray` -- so `free_size` names whichever one applies rather
    // than resolving a race between them.
    //
    // `< input_size`, not `<=`: at equality the "reduction" is the input layout re-spelled (a
    // Gray8 input reduced to 8-bit grey, a GrayAlpha8 input to grey+alpha), so offering it as a
    // runner-up would make the encoder compress the same samples a second time for a file it
    // already has.
    let free_size = gray_size.min(gray_alpha_size).min(rgb_size);
    let chunk_free = (free_size < input_size).then(|| {
        if free_size == gray_size {
            let scale = gray8_scale(gray_depth);
            Reduced::Gray {
                depth: gray_depth,
                samples: pixels
                    .chunks_exact(channels)
                    .map(|px| px[0] / scale)
                    .collect(),
            }
        } else if free_size == gray_alpha_size {
            let mut out = Vec::with_capacity(pixel_count * 2);
            for px in pixels.chunks_exact(channels) {
                let key = pixel_key(px, channels);
                out.push(key[0]);
                out.push(key[3]);
            }
            Reduced::GrayAlpha8(out)
        } else {
            let mut out = Vec::with_capacity(pixel_count * 3);
            for px in pixels.chunks_exact(channels) {
                out.extend_from_slice(&px[0..3]);
            }
            Reduced::Rgb8(out)
        }
    });

    // The chunk-carrying family: a colour key's `tRNS`, or a palette's `PLTE` (+ `tRNS`). Built
    // only if one of them is the estimate's winner, since building either walks the pixels again.
    let chunk_carrying = || {
        if let Some(key) = key
            && best == keyed_size
        {
            if all_gray {
                Reduced::GrayKeyed {
                    samples: pixels
                        .chunks_exact(channels)
                        .map(|px| pixel_key(px, channels)[0])
                        .collect(),
                    key: key[0],
                }
            } else {
                let mut out = Vec::with_capacity(pixel_count * 3);
                for px in pixels.chunks_exact(channels) {
                    out.extend_from_slice(&pixel_key(px, channels)[0..3]);
                }
                Reduced::Rgb8Keyed {
                    samples: out,
                    key: [key[0], key[1], key[2]],
                }
            }
        } else {
            build_indexed(pixels, channels, &palette, &palette_index)
        }
    };

    match chunk_free {
        // The estimate's own winner is the chunk-free candidate, so nothing carries a chunk and
        // there is nothing to race. The second arm is therefore reached only when a palette or a
        // colour key beat it -- `best < free_size`, which is also why `chunk_free` is `None` there
        // whenever no chunk-free reduction exists at all.
        Some(reduced) if best == free_size => Reductions::ChunkFree(reduced),
        chunk_free => Reductions::Chunked {
            chunked: chunk_carrying(),
            chunk_free,
        },
    }
}

/// Analyses interleaved 16-bit samples (`channels` as in [`analyze8`]). An input where every
/// sample's high byte equals its low byte (`v == k·257`, the exact inverse of the decoder's 8→16
/// widening) is demoted and re-analysed at 8 bits — the demotion alone halves the payload, so it
/// always reduces. Otherwise only the 16-bit-native channel reductions (grey, alpha drop) apply;
/// PNG has no 16-bit palette.
///
/// The plain demotion is the floor of the demotable path, not merely its fallback: it adds no
/// chunk, so where the 8-bit analysis offers a chunk-carrying winner and no chunk-free runner-up
/// of its own, the demotion itself becomes that runner-up. Without it a palette that loses the
/// finished file would drop the encoder all the way back to 16 bits, throwing away a halving that
/// costs nothing.
pub fn analyze16(samples: &[u16], channels: usize) -> Reductions {
    debug_assert!((1..=4).contains(&channels));
    if let Some(demoted) = demote16(samples) {
        let plain = |demoted| match channels {
            1 => Reduced::Gray {
                depth: 8,
                samples: demoted,
            },
            2 => Reduced::GrayAlpha8(demoted),
            3 => Reduced::Rgb8(demoted),
            _ => Reduced::Rgba8(demoted),
        };
        return match analyze8(&demoted, channels) {
            Reductions::None => Reductions::ChunkFree(plain(demoted)),
            Reductions::ChunkFree(reduced) => Reductions::ChunkFree(reduced),
            Reductions::Chunked {
                chunked,
                chunk_free,
            } => Reductions::Chunked {
                chunked,
                chunk_free: Some(chunk_free.unwrap_or_else(|| plain(demoted))),
            },
        };
    }

    let mut all_opaque = true;
    let mut all_gray = true;
    for px in samples.chunks_exact(channels) {
        if channels.is_multiple_of(2) {
            all_opaque &= px[channels - 1] == u16::MAX;
        }
        if channels >= 3 {
            all_gray &= px[0] == px[1] && px[1] == px[2];
        }
    }

    // Unlike the 8-bit analysis there is no size estimate to weigh: the candidates' gates are
    // mutually exclusive, and each strictly shrinks the channel count, so whichever gate matches
    // wins outright. The channel checks reject the identity "reductions" (grey of a Gray16 input,
    // grey+alpha of a GrayAlpha16 input). None of them carries a chunk -- PNG has no 16-bit
    // palette and this arm found no demotion -- so there is never anything here to race.
    let px16 = samples.chunks_exact(channels);
    if all_gray && all_opaque && channels > 1 {
        Reductions::ChunkFree(Reduced::Gray16Be(be_bytes(px16.map(|px| px[0]))))
    } else if all_gray && channels > 2 {
        // Not all-opaque (that is the branch above), so the alpha channel must be kept.
        Reductions::ChunkFree(Reduced::GrayAlpha16Be(be_bytes(
            px16.flat_map(|px| [px[0], px[channels - 1]]),
        )))
    } else if channels == 4 && all_opaque {
        // Not all-grey (the branches above), so only the opaque alpha channel can be dropped.
        Reductions::ChunkFree(Reduced::Rgb16Be(be_bytes(
            px16.flat_map(|px| [px[0], px[1], px[2]]),
        )))
    } else {
        Reductions::None
    }
}

/// The 8-bit demotion of `samples`, or `None` unless every sample is exactly `k·257` (high byte ==
/// low byte), which makes the demotion reversible by ×257 widening.
fn demote16(samples: &[u16]) -> Option<Vec<u8>> {
    samples
        .iter()
        .map(|&v| {
            let [hi, lo] = v.to_be_bytes();
            (hi == lo).then_some(lo)
        })
        .collect()
}

/// Serialises 16-bit samples big-endian (PNG's network byte order).
fn be_bytes(samples: impl Iterator<Item = u16>) -> Vec<u8> {
    samples.flat_map(u16::to_be_bytes).collect()
}

/// Orders the palette so the encoding costs less, returning the entries in their new order.
///
/// Index order is not free: it decides the `tRNS` chunk's length, and it decides what the row
/// filters see, since a filtered index stream is the *difference* between neighbouring indices.
/// Two rules, in priority order:
///
/// 1. **Transparent entries first**, least opaque first. `tRNS` may be shorter than `PLTE` and
///    every omitted entry defaults to opaque (§11.3.2.1), so gathering the transparent entries at
///    the front makes the trailing-opaque trim below cut as much as it possibly can. First-
///    appearance order left them scattered, so one late transparent entry pinned the whole chunk
///    to full length.
/// 2. **Then by luminance.** Neighbouring indices become neighbouring brightnesses, so an image
///    with smooth shading produces small index deltas rather than the arbitrary jumps
///    raster-scan discovery order gives — which is what `Sub` and `Paeth` are good at.
///
/// Rec. 601 luma, integer, because this only has to *order* entries and never round-trips through
/// a pixel. The full modified-Zeng ordering oxipng uses is a further step (#482).
fn ordered_palette(palette: &[[u8; 4]]) -> Vec<[u8; 4]> {
    let mut out = palette.to_vec();
    out.sort_by_key(|c| {
        let luma = 299 * u32::from(c[0]) + 587 * u32::from(c[1]) + 114 * u32::from(c[2]);
        // Alpha first, so every transparent entry sorts ahead of every opaque one -- 255 is the
        // maximum, so ordering by alpha *is* "opaque last" and a separate `c[3] == 255` component
        // ahead of it can never change the order this returns.
        (u32::from(c[3]), luma)
    });
    out
}

/// Builds the indexed reduction from the collected palette.
fn build_indexed(
    pixels: &[u8],
    channels: usize,
    palette: &[[u8; 4]],
    palette_index: &HashMap<[u8; 4], u8>,
) -> Reduced {
    let ordered = ordered_palette(palette);
    // Reindex through the new order. `palette_index` maps a colour to its *discovery* index, so
    // this composes discovery -> colour -> final position.
    let mut remap = vec![0u8; palette.len()];
    for (position, colour) in ordered.iter().enumerate() {
        if let Some(&discovered) = palette_index.get(colour) {
            remap[discovered as usize] = position as u8;
        }
    }
    let indices: Vec<u8> = pixels
        .chunks_exact(channels)
        .map(|px| {
            let discovered = *palette_index.get(&pixel_key(px, channels)).unwrap_or(&0);
            remap[discovered as usize]
        })
        .collect();
    let plte: Vec<u8> = ordered.iter().flat_map(|c| [c[0], c[1], c[2]]).collect();
    let trns = if ordered.iter().any(|c| c[3] != 255) {
        let mut alphas: Vec<u8> = ordered.iter().map(|c| c[3]).collect();
        // Trailing fully-opaque entries may be omitted (they default to opaque). With the
        // transparent entries gathered at the front this now trims everything after them.
        //
        // No length guard: this arm runs only when some entry's alpha is not 255, so the
        // `last() == 255` test always halts the loop before the vector empties. Ordering makes
        // that argument stronger rather than weaker -- the entry that halts it is at index 0, so
        // the loop stops with at least one element left. The `alphas.len() > 1` that used to be
        // here therefore decided nothing, and `>` vs `>=` was an equivalent mutant no test could
        // kill (#110) -- removed rather than excluded.
        while alphas.last() == Some(&255) {
            alphas.pop();
        }
        Some(alphas)
    } else {
        None
    };
    Reduced::Indexed {
        depth: index_bit_depth(ordered.len()),
        indices,
        plte,
        trns,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_colour_key_is_only_possible_with_an_alpha_channel_carrying_something() {
        assert!(may_have_colour_key(false, 4), "RGBA with transparency");
        assert!(
            may_have_colour_key(false, 2),
            "grey+alpha with transparency"
        );
        // An all-opaque image drops the channel outright, which costs no chunk.
        assert!(!may_have_colour_key(true, 4));
        // No alpha channel to drop in the first place.
        assert!(!may_have_colour_key(false, 3));
        assert!(!may_have_colour_key(false, 1));
    }

    #[test]
    fn the_keyed_cost_is_the_samples_plus_one_trns_chunk() {
        // Greyscale: one byte per pixel, and a tRNS of one 16-bit sample plus 12 framing.
        assert_eq!(keyed_size(100, true), 100 + 2 + 12);
        // Truecolour: three bytes per pixel, and three 16-bit samples plus 12 framing.
        assert_eq!(keyed_size(100, false), 300 + 6 + 12);
        // The chunk is a flat cost -- it does not scale with the image.
        assert_eq!(keyed_size(0, true), GREY_KEY_COST);
        assert_eq!(keyed_size(0, false), RGB_KEY_COST);
    }

    #[test]
    fn cleaning_declines_when_there_is_nothing_invisible_to_clean() {
        // `None` rather than an unchanged copy: the encoder must be able to tell "no work" from
        // "work that happened to change nothing", or it allocates a whole image for nothing.
        let opaque: Vec<u8> = (0..16u8).flat_map(|i| [i, i + 1, i + 2, 255]).collect();
        assert!(clean_transparent(&opaque, 4).is_none());

        // Layouts with no alpha channel have nothing to clean, whatever the samples say.
        assert!(clean_transparent(&opaque, 3).is_none());
        assert!(clean_transparent(&opaque, 1).is_none());
    }

    #[test]
    fn cleaning_zeroes_invisible_colour_and_leaves_everything_else() {
        let src: Vec<u8> = vec![
            10, 20, 30, 255, // visible
            40, 50, 60, 0, // invisible: colour must go
            70, 80, 90, 128, // partially transparent: still visible, must stay
        ];
        let cleaned = clean_transparent(&src, 4).expect("there is a transparent pixel");
        assert_eq!(
            cleaned,
            vec![
                10, 20, 30, 255, //
                0, 0, 0, 0, //
                70, 80, 90, 128,
            ]
        );
    }

    #[test]
    fn cleaning_grey_alpha_zeroes_only_the_grey_channel() {
        let src: Vec<u8> = vec![200, 255, 111, 0, 90, 1];
        let cleaned = clean_transparent(&src, 2).expect("there is a transparent pixel");
        assert_eq!(cleaned, vec![200, 255, 0, 0, 90, 1]);
    }

    #[test]
    fn drops_opaque_alpha() {
        // Opaque, non-grey RGBA -> RGB.
        let rgba = [10, 20, 30, 255, 40, 50, 60, 255];
        match analyze8(&rgba, 4) {
            Reductions::ChunkFree(Reduced::Rgb8(rgb)) => {
                assert_eq!(rgb, vec![10, 20, 30, 40, 50, 60]);
            }
            _ => panic!("expected a chunk-free Rgb8"),
        }
    }

    #[test]
    fn detects_grayscale() {
        // Opaque R=G=B RGB with many levels -> 8-bit grey.
        let rgb: Vec<u8> = (0..60u8).flat_map(|v| [v, v, v]).collect();
        match analyze8(&rgb, 3) {
            Reductions::ChunkFree(Reduced::Gray { depth: 8, samples }) => {
                assert_eq!(samples, (0..60u8).collect::<Vec<_>>());
            }
            _ => panic!("expected a chunk-free 8-bit Gray"),
        }
    }

    /// A grey image *with* alpha picks grey+alpha, not something else.
    ///
    /// `gray_alpha_size` is gated on `all_gray && !all_opaque`. Every existing fixture was either
    /// opaque or not grey, so the `!` could be deleted and nothing failed (#110) -- and without it
    /// grey+alpha is never even a candidate, so an image that should cost two bytes a pixel gets
    /// encoded as RGBA at four.
    ///
    /// The palette route is deliberately priced out of contention with more than 256 distinct
    /// (grey, alpha) pairs, so the choice really is the one this gate makes.
    #[test]
    fn grey_with_alpha_chooses_gray_alpha() {
        let mut rgba = Vec::new();
        for i in 0..300u32 {
            let g = (i % 256) as u8;
            let a = (i / 2 % 254) as u8; // never 255: every pixel is genuinely translucent
            rgba.extend_from_slice(&[g, g, g, a]);
        }
        match analyze8(&rgba, 4) {
            Reductions::ChunkFree(Reduced::GrayAlpha8(samples)) => {
                assert_eq!(samples.len(), 600, "two bytes per pixel");
            }
            _ => panic!("expected a chunk-free GrayAlpha8"),
        }
    }

    /// The palette's *overhead* is what decides against RGB here, not the pixel bytes.
    ///
    /// This fixture is sized so the two estimates sit close together: 350 pixels over 200 distinct
    /// opaque colours gives a palette at `350 + 200*3 + 24 = 974` against RGB at `350*3 = 1050`.
    /// Palette wins by 76 bytes -- narrow enough that any arithmetic slip in the estimate flips the
    /// answer, which is exactly what four surviving mutants in that expression did (#110):
    ///
    ///   * `needs_trns` computed with `==` instead of `!=` adds a phantom tRNS table (+200) -> RGB
    ///   * the trailing `+ 24` as `* 24` inflates the overhead to 14 400 -> RGB
    ///   * `pixel_count * 3` as `+ 3` or `/ 3` under-prices RGB (353 or 116) -> RGB
    ///
    /// Every one of them lands on `Rgb8`, and the assertion below is that it is `Indexed`.
    #[test]
    fn palette_overhead_decides_against_rgb_at_the_margin() {
        let mut rgba = Vec::new();
        for i in 0..350u32 {
            // 200 distinct colours, none grey (R != G), all fully opaque.
            let c = (i % 200) as u8;
            rgba.extend_from_slice(&[c, c.wrapping_add(64), 200, 255]);
        }
        match analyze8(&rgba, 4) {
            Reductions::Chunked {
                chunked:
                    Reduced::Indexed {
                        depth, plte, trns, ..
                    },
                ..
            } => {
                assert_eq!(depth, 8, "200 entries need the 8-bit index depth");
                assert_eq!(plte.len(), 200 * 3);
                assert_eq!(trns, None, "an all-opaque palette carries no tRNS");
            }
            _ => panic!("expected Indexed"),
        }
    }

    /// A palette that wins the estimate still hands over the alpha drop it beat.
    ///
    /// The defect this pins: the estimate collapsed five candidates to one, and only that
    /// one was ever encoded. On an opaque RGBA image with few enough colours the palette wins the
    /// raw comparison — 350 + 600 + 24 against 1050 — and then loses the *finished* file to
    /// `PLTE`'s 600 incompressible bytes, at which point the encoder fell back to the unreduced
    /// RGBA and kept an alpha channel that is 255 everywhere. `Reductions::Chunked` carries the
    /// runner-up so `write_reduced_or_native` can measure it too.
    ///
    /// The alpha drop, not the greyscale collapse or the grey+alpha one: the three chunk-free
    /// gates are mutually exclusive and this fixture is opaque and not grey, so `free_size` can
    /// only be `rgb_size`.
    #[test]
    fn an_opaque_palette_hands_over_the_alpha_drop_it_beat() {
        let mut rgba = Vec::new();
        for i in 0..350u32 {
            // 200 distinct colours, none grey (R != G), all fully opaque.
            let c = (i % 200) as u8;
            rgba.extend_from_slice(&[c, c.wrapping_add(64), 200, 255]);
        }
        match analyze8(&rgba, 4) {
            Reductions::Chunked {
                chunked: Reduced::Indexed { .. },
                chunk_free: Some(Reduced::Rgb8(rgb)),
            } => {
                assert_eq!(rgb.len(), 350 * 3, "three channels, one alpha dropped");
                assert_eq!(&rgb[0..6], &[0, 64, 200, 1, 65, 200]);
            }
            _ => panic!("expected Indexed with an Rgb8 runner-up"),
        }
    }

    /// An identity layout is not offered as a runner-up, because encoding it is encoding nothing.
    ///
    /// This grey+alpha input reduces to a 1-bit palette, so a runner-up would be raced against it.
    /// The only chunk-free candidate its gates admit is grey+alpha — which is the input layout
    /// spelled again, `pixel_count * 2` against an input of exactly `pixel_count * 2`. Offering it
    /// would make the encoder filter and DEFLATE the same samples a second time to arrive at the
    /// file the unreduced candidate already produces, so `analyze8` requires a *strict* saving
    /// before it names a runner-up.
    #[test]
    fn an_identity_layout_is_not_offered_as_a_runner_up() {
        let ga: Vec<u8> = [0, 0, 255, 255].repeat(40); // transparent black, opaque white
        match analyze8(&ga, 2) {
            Reductions::Chunked {
                chunked: Reduced::Indexed { .. },
                chunk_free,
            } => assert!(
                chunk_free.is_none(),
                "grey+alpha of a grey+alpha input stores exactly the input's bytes"
            ),
            _ => panic!("expected Indexed"),
        }
    }

    /// A demotable 16-bit image keeps its demotion as the runner-up when a palette wins.
    ///
    /// The 16-bit half of the same defect. `analyze16` demotes, re-analyses at 8 bits, and
    /// used the plain demotion only when the 8-bit analysis found *nothing*. When the 8-bit
    /// analysis found a palette instead, the demotion was discarded — and if the palette then lost
    /// the finished file, the encoder fell back to the 16-bit input and threw away a halving that
    /// costs no chunk. Here 64 opaque non-grey colours give an 8-bit palette and no chunk-free
    /// 8-bit candidate (the input is already RGB), so the demotion is the only runner-up there is.
    #[test]
    fn a_demotable_palette_keeps_the_plain_demotion_as_its_runner_up() {
        let rgb16: Vec<u16> = (0..600u32)
            .flat_map(|i| {
                let c = (i % 64) as u8;
                [
                    u16::from(c) * 257,
                    u16::from(c.wrapping_add(64)) * 257,
                    200 * 257,
                ]
            })
            .collect();
        match analyze16(&rgb16, 3) {
            Reductions::Chunked {
                chunked: Reduced::Indexed { .. },
                chunk_free: Some(Reduced::Rgb8(demoted)),
            } => {
                assert_eq!(demoted.len(), 600 * 3, "half the 16-bit payload");
                assert_eq!(&demoted[0..3], &[0, 64, 200]);
            }
            _ => panic!("expected Indexed with a demoted Rgb8 runner-up"),
        }
    }

    /// A translucent palette pays for its tRNS table, and the `+ 24` chunk overhead is real.
    ///
    /// The companion to `palette_overhead_decides_against_rgb_at_the_margin`, which uses an
    /// all-opaque palette and so could not see this (#110). In `3*C + (trns ? C : 0) + 24`, the
    /// mutation `+ 24` -> `* 24` binds to the conditional, not the sum:
    ///
    ///   opaque      `... + 0 * 24`          = the estimate merely loses 24 bytes
    ///   translucent `... + C * 24`          = 4800 instead of 224, a 21x error
    ///
    /// So the opaque fixture's 76-byte margin absorbs it and a translucent one does not. Here the
    /// estimate is 1124 against a 1200-byte raw input: reduction wins by 76 bytes. Under the
    /// mutant it becomes 5700, exceeds the input, and `analyze8` declines to reduce at all --
    /// which is what makes this a decisive kill rather than a tuned one.
    #[test]
    fn a_translucent_palette_pays_for_its_trns_table() {
        let mut rgba = Vec::new();
        for i in 0..300u32 {
            // 200 distinct entries, every one translucent, none grey.
            let c = (i % 200) as u8;
            rgba.extend_from_slice(&[c, c.wrapping_add(64), 200, 128]);
        }
        match analyze8(&rgba, 4) {
            Reductions::Chunked {
                chunked: Reduced::Indexed { depth, trns, .. },
                ..
            } => {
                assert_eq!(depth, 8, "200 entries need the 8-bit index depth");
                assert_eq!(
                    trns.map(|t| t.len()),
                    Some(200),
                    "every entry is translucent, so none is trimmed"
                );
            }
            _ => panic!("expected Indexed"),
        }
    }

    /// Trailing opaque entries are trimmed from tRNS; the first non-opaque one is kept.
    ///
    /// Pins the loop whose length guard was removed as redundant: it stops at the last entry that
    /// is *not* 255, which is what makes the guard unnecessary rather than merely unused.
    #[test]
    fn trns_keeps_up_to_the_last_translucent_entry() {
        // Palette order follows first appearance: translucent, then two opaque. Repeated enough
        // times that the palette actually beats the raw input -- with only three pixels the
        // 36-byte palette overhead loses to 12 bytes of RGBA and nothing is reduced at all.
        let mut rgba = Vec::new();
        for _ in 0..100 {
            rgba.extend_from_slice(&[10, 20, 30, 128]);
            rgba.extend_from_slice(&[40, 50, 60, 255]);
            rgba.extend_from_slice(&[70, 80, 90, 255]);
        }
        match analyze8(&rgba, 4) {
            Reductions::Chunked {
                chunked: Reduced::Indexed { trns, .. },
                ..
            } => {
                assert_eq!(
                    trns,
                    Some(vec![128]),
                    "the two trailing opaque entries are omitted, the translucent one kept"
                );
            }
            _ => panic!("expected Indexed"),
        }
    }

    #[test]
    fn builds_palette_for_few_colours() {
        // Two distinct colours over many pixels -> indexed at 1 bit.
        let mut rgb = Vec::new();
        for i in 0..100u32 {
            if i % 2 == 0 {
                rgb.extend_from_slice(&[200, 10, 10]);
            } else {
                rgb.extend_from_slice(&[10, 10, 200]);
            }
        }
        match analyze8(&rgb, 3) {
            Reductions::Chunked {
                chunked:
                    Reduced::Indexed {
                        depth,
                        plte,
                        trns,
                        indices,
                    },
                ..
            } => {
                assert_eq!(depth, 1);
                assert_eq!(plte.len(), 6); // two RGB entries
                assert!(trns.is_none());
                assert_eq!(indices.len(), 100);
            }
            _ => panic!("expected Indexed"),
        }
    }

    #[test]
    fn keeps_full_colour_photographic_data() {
        // Many distinct opaque colours, not grey -> no reduction.
        let rgb: Vec<u8> = (0..300u32)
            .flat_map(|i| [i as u8, (i >> 1) as u8, (i >> 2) as u8])
            .collect();
        assert!(matches!(analyze8(&rgb, 3), Reductions::None));
    }

    /// Rec. 601 luma is the *only* thing separating these five opaque entries -- same alpha, so
    /// the first two sort-key components tie -- and the fixture is chosen so collapsing any one of
    /// the three weights from a multiply to an add returns a different order:
    ///
    /// | entry | `299*c0 + 587*c1 + 114*c2` | `299 +` | `587 +` | `114 +` |
    /// | --- | --- | --- | --- | --- |
    /// | `[255, 0, 0]` | 76 245 | 554 | 76 832 | 76 359 |
    /// | `[0, 100, 0]` | 58 700 | 58 999 | 687 | 58 814 |
    /// | `[0, 130, 0]` | 76 310 | 76 609 | 717 | 76 424 |
    /// | `[0, 0, 255]` | 29 070 | 29 369 | 29 657 | 369 |
    /// | `[0, 49, 0]` | 28 763 | 29 062 | 636 | 28 877 |
    ///
    /// Every other palette fixture in the crate happens to have discovery order equal to sorted
    /// order, so none of them can tell [`ordered_palette`] from the identity, let alone tell one
    /// weight from another. The input order below is deliberately not the expected order.
    #[test]
    fn the_palette_orders_by_rec_601_luma() {
        let palette = [
            [255, 0, 0, 255],
            [0, 100, 0, 255],
            [0, 130, 0, 255],
            [0, 0, 255, 255],
            [0, 49, 0, 255],
        ];
        assert_eq!(
            ordered_palette(&palette),
            vec![
                [0, 49, 0, 255],
                [0, 0, 255, 255],
                [0, 100, 0, 255],
                [255, 0, 0, 255],
                [0, 130, 0, 255],
            ]
        );
    }

    /// Rule 1 of [`ordered_palette`] earning its keep, end to end through [`build_indexed`].
    ///
    /// The transparent entry is discovered *last* here: the raster scan meets opaque white, then
    /// opaque red, and only then the invisible pixels. In discovery order the `tRNS` alphas would
    /// be `[255, 255, 0]`, which the trailing-opaque trim cannot shorten at all -- one late
    /// transparent entry pins the chunk to full length. Sorting transparent-first makes them
    /// `[0, 255, 255]`, and the trim cuts two of the three.
    #[test]
    fn a_late_transparent_entry_moves_to_index_zero_and_shortens_trns() {
        let mut rgba = Vec::new();
        for i in 0..80u32 {
            if i % 2 == 0 {
                rgba.extend_from_slice(&[255, 255, 255, 255]); // opaque white
            } else {
                rgba.extend_from_slice(&[200, 10, 10, 255]); // opaque red
            }
        }
        rgba.extend_from_slice(&[0, 0, 0, 0].repeat(40)); // invisible, discovered last

        match analyze8(&rgba, 4) {
            Reductions::Chunked {
                chunked: Reduced::Indexed { plte, trns, .. },
                ..
            } => {
                assert_eq!(
                    plte,
                    vec![0, 0, 0, 200, 10, 10, 255, 255, 255],
                    "transparent first, then opaque by luma"
                );
                assert_eq!(trns, Some(vec![0]), "the trim reaches every opaque entry");
            }
            _ => panic!("expected Indexed"),
        }
    }

    #[test]
    fn palette_with_transparency_emits_trns() {
        let rgba = [
            0, 0, 0, 0, // transparent black
            255, 255, 255, 255, // opaque white
        ]
        .repeat(20);
        match analyze8(&rgba, 4) {
            Reductions::Chunked {
                chunked: Reduced::Indexed { trns: Some(t), .. },
                ..
            } => assert_eq!(t, vec![0]),
            _ => panic!("expected indexed with tRNS"),
        }
    }

    /// Asserts `pixels` (of `channels`) reduces to grey at `depth` with the expected codes.
    fn expect_gray(pixels: &[u8], channels: usize, depth: u8, codes: &[u8]) {
        match analyze8(pixels, channels) {
            Reductions::ChunkFree(Reduced::Gray {
                depth: got,
                samples,
            }) => {
                assert_eq!(got, depth, "depth");
                assert_eq!(samples, codes, "codes");
            }
            _ => panic!("expected a chunk-free Gray at depth {depth}"),
        }
    }

    #[test]
    fn grey_packs_to_the_smallest_exact_depth() {
        // §13.12 exactness per depth: 1-bit {0,255}, 2-bit multiples of 85, 4-bit multiples of 17.
        // Each set breaks the next-lower depth's divisibility, pinning the moduli individually.
        let bw: Vec<u8> = [0u8, 255].repeat(30);
        expect_gray(&bw, 1, 1, &[0, 1].repeat(30));

        let quarters: Vec<u8> = [0u8, 85, 170, 255].repeat(20);
        expect_gray(&quarters, 1, 2, &[0, 1, 2, 3].repeat(20));

        let sixteenths: Vec<u8> = (0..16u8).map(|v| v * 17).collect::<Vec<_>>().repeat(10);
        expect_gray(&sixteenths, 1, 4, &(0..16u8).collect::<Vec<_>>().repeat(10));

        // The same values arriving as opaque RGBA reduce straight to sub-byte grey too.
        let rgba: Vec<u8> = quarters.iter().flat_map(|&v| [v, v, v, 255]).collect();
        expect_gray(&rgba, 4, 2, &[0, 1, 2, 3].repeat(20));
    }

    #[test]
    fn low_cardinality_grey_off_the_scale_grid_is_indexed() {
        // Three grey levels, none a §13.12 multiple: not packable as grey, but a grey palette at
        // 2 bits still beats 8-bit grey.
        let gray: Vec<u8> = [5u8, 9, 200].repeat(40);
        match analyze8(&gray, 1) {
            Reductions::Chunked {
                chunked:
                    Reduced::Indexed {
                        depth, plte, trns, ..
                    },
                ..
            } => {
                assert_eq!(depth, 2);
                assert_eq!(plte, vec![5, 5, 5, 9, 9, 9, 200, 200, 200]);
                assert!(trns.is_none());
            }
            _ => panic!("expected Indexed"),
        }
    }

    #[test]
    fn grey_input_with_full_range_keeps_its_encoding() {
        // 8-bit grey using values off every sub-byte grid and >16 distinct levels: nothing beats
        // the input.
        let gray: Vec<u8> = (0..=255u8).collect();
        assert!(matches!(analyze8(&gray, 1), Reductions::None));
    }

    #[test]
    fn grey_alpha_drops_an_opaque_alpha_channel() {
        let ga: Vec<u8> = (0..90u8).flat_map(|v| [v, 255]).collect();
        expect_gray(&ga, 2, 8, &(0..90u8).collect::<Vec<_>>());
    }

    #[test]
    fn grey_alpha_with_few_combinations_is_indexed_with_trns() {
        let ga: Vec<u8> = [0, 0, 255, 255].repeat(40); // transparent black, opaque white
        match analyze8(&ga, 2) {
            Reductions::Chunked {
                chunked:
                    Reduced::Indexed {
                        depth,
                        plte,
                        trns: Some(t),
                        ..
                    },
                ..
            } => {
                assert_eq!(depth, 1);
                assert_eq!(plte, vec![0, 0, 0, 255, 255, 255]);
                assert_eq!(t, vec![0]);
            }
            _ => panic!("expected indexed with tRNS"),
        }
    }

    /// `Reduced::GrayKeyed` -- the greyscale twin of `Rgb8Keyed`, reachable and correct but
    /// produced by nothing else in the suite, so `analyze8`'s `all_gray` split inside the keyed
    /// arm had no test that could see it.
    ///
    /// Grey + binary alpha, every invisible pixel sharing grey 7, and 64 distinct opaque grey
    /// levels 8..=71 that no invisible pixel can collide with. The estimates that race
    /// (`pixel_count` = 256):
    ///
    /// - keyed: `256 + GREY_KEY_COST` = **270**
    /// - grey + alpha: `256 * 2` = 512, which is also the input size, so it cannot win
    /// - palette: 65 entries needs depth 8, `256 + 65 * 4 + 24` = 540
    ///
    /// 65 entries is what keeps the palette out of the race: below 17 the index depth drops to 4
    /// and the palette would win on a fixture this large.
    #[test]
    fn binary_alpha_grey_reduces_to_a_colour_keyed_greyscale() {
        let ga: Vec<u8> = (0..256u32)
            .flat_map(|i| {
                if i.is_multiple_of(5) {
                    [7, 0] // invisible, all one grey
                } else {
                    [8 + (i % 64) as u8, 255]
                }
            })
            .collect();

        match analyze8(&ga, 2) {
            Reductions::Chunked {
                chunked: Reduced::GrayKeyed { samples, key },
                ..
            } => {
                assert_eq!(key, 7, "the one grey every invisible pixel carries");
                assert_eq!(samples.len(), 256, "one sample per pixel, alpha gone");
                // The key erases whatever wears it, so nothing visible may wear it.
                for (i, px) in ga.as_chunks::<2>().0.iter().enumerate() {
                    if px[1] == 255 {
                        assert_ne!(samples[i], key, "visible pixel {i} would be erased");
                    }
                }
            }
            other => panic!(
                "expected a chunk-carrying GrayKeyed, got {}",
                match other {
                    Reductions::Chunked {
                        chunked: Reduced::Indexed { .. },
                        ..
                    } => "Indexed",
                    Reductions::Chunked {
                        chunked: Reduced::Rgb8Keyed { .. },
                        ..
                    } => "Rgb8Keyed",
                    Reductions::ChunkFree(Reduced::GrayAlpha8(_)) => "GrayAlpha8",
                    Reductions::ChunkFree(_) | Reductions::Chunked { .. } => "some other reduction",
                    Reductions::None => "no reduction",
                }
            ),
        }
    }

    #[test]
    fn grey_alpha_noise_keeps_its_encoding() {
        let ga: Vec<u8> = (0..600u32)
            .flat_map(|i| [(i % 251) as u8, (i % 249) as u8])
            .collect();
        assert!(matches!(analyze8(&ga, 2), Reductions::None));
    }

    #[test]
    fn demotable_sixteen_bit_recurses_into_the_eight_bit_analysis() {
        // Grey, opaque, every sample k*257 -> demoted and reduced all the way to 8-bit grey.
        let rgba16: Vec<u16> = (0..80u16)
            .flat_map(|i| {
                let v = (i % 60) * 257;
                [v, v, v, u16::MAX]
            })
            .collect();
        match analyze16(&rgba16, 4) {
            Reductions::ChunkFree(Reduced::Gray { depth: 8, samples }) => {
                assert_eq!(samples, (0..80).map(|i| (i % 60) as u8).collect::<Vec<_>>());
            }
            _ => panic!("expected a chunk-free 8-bit Gray"),
        }
    }

    #[test]
    fn demotable_but_irreducible_sixteen_bit_falls_back_to_plain_demotion() {
        // Every sample k*257 but many colours and varied alpha: the demotion itself is the win.
        let rgba16: Vec<u16> = (0..600u32)
            .flat_map(|i| {
                [
                    ((i % 251) * 257) as u16,
                    ((i % 241) * 257) as u16,
                    ((i % 239) * 257) as u16,
                    ((i % 233) * 257) as u16,
                ]
            })
            .collect();
        match analyze16(&rgba16, 4) {
            Reductions::ChunkFree(Reduced::Rgba8(demoted)) => {
                assert_eq!(demoted.len(), 600 * 4);
                assert_eq!(demoted[0..4], [0, 0, 0, 0]);
                assert_eq!(demoted[4 * 250], 250u8);
            }
            _ => panic!("expected demoted Rgba8"),
        }
    }

    #[test]
    fn two_matching_channels_are_not_grey() {
        // R == G but B differs on every pixel: not greyscale, too many colours to palette.
        let rgb: Vec<u8> = (0..60u8).flat_map(|v| [v, v, 200]).collect();
        assert!(matches!(analyze8(&rgb, 3), Reductions::None));
        // The 16-bit twin (non-demotable): same verdict.
        let rgb16: Vec<u16> = (0..60u32)
            .flat_map(|i| {
                let v = (i * 501 + 1) as u16;
                [v, v, 200]
            })
            .collect();
        assert!(matches!(analyze16(&rgb16, 3), Reductions::None));
    }

    #[test]
    fn sixteen_bit_identity_reductions_are_rejected() {
        // Non-demotable grey noise arriving as Gray16 is already minimal.
        let gray16: Vec<u16> = (0..90u32).map(|i| (i * 501 + 1) as u16).collect();
        assert!(matches!(analyze16(&gray16, 1), Reductions::None));
    }

    #[test]
    fn demotable_grey_alpha_and_rgb_fall_back_to_their_own_layout() {
        // GrayAlpha16, every sample k*257, varied alpha, >256 (grey, alpha) combos: the demotion
        // itself is the only win, and it must keep the grey+alpha layout.
        let ga16: Vec<u16> = (0..600u32)
            .flat_map(|i| [((i % 251) * 257) as u16, ((i % 33) * 7 * 257) as u16])
            .collect();
        match analyze16(&ga16, 2) {
            Reductions::ChunkFree(Reduced::GrayAlpha8(demoted)) => {
                assert_eq!(demoted.len(), 600 * 2);
            }
            _ => panic!("expected a chunk-free demoted GrayAlpha8"),
        }

        // Rgb16, every sample k*257, many non-grey colours: plain demotion keeps RGB.
        let rgb16: Vec<u16> = (0..600u32)
            .flat_map(|i| {
                [
                    ((i % 251) * 257) as u16,
                    ((i % 241) * 257) as u16,
                    ((i % 239) * 257) as u16,
                ]
            })
            .collect();
        match analyze16(&rgb16, 3) {
            Reductions::ChunkFree(Reduced::Rgb8(demoted)) => {
                assert_eq!(demoted.len(), 600 * 3);
            }
            _ => panic!("expected a chunk-free demoted Rgb8"),
        }
    }

    #[test]
    fn one_asymmetric_sample_disables_demotion() {
        // All grey/opaque k*257 except a single 0x0100 (hi != lo): must stay 16-bit native.
        let mut rgba16: Vec<u16> = (0..80u16)
            .flat_map(|i| {
                let v = (i % 60) * 257;
                [v, v, v, u16::MAX]
            })
            .collect();
        rgba16[0] = 0x0100;
        rgba16[1] = 0x0100;
        rgba16[2] = 0x0100; // keep the pixel grey so the native grey reduction still applies
        match analyze16(&rgba16, 4) {
            Reductions::ChunkFree(Reduced::Gray16Be(bytes)) => {
                assert_eq!(&bytes[0..2], &[0x01, 0x00], "big-endian, undemoted");
            }
            _ => panic!("expected a chunk-free Gray16Be"),
        }
    }

    #[test]
    fn sixteen_bit_native_reductions_serialise_big_endian() {
        // Opaque, non-grey, non-demotable RGBA16 -> RGB16, big-endian.
        let rgba16: Vec<u16> = (0..90u32)
            .flat_map(|i| [(i * 501 + 1) as u16, (i * 703 + 2) as u16, 3, u16::MAX])
            .collect();
        match analyze16(&rgba16, 4) {
            Reductions::ChunkFree(Reduced::Rgb16Be(bytes)) => {
                assert_eq!(bytes.len(), 90 * 6);
                assert_eq!(&bytes[0..6], &[0, 1, 0, 2, 0, 3]);
            }
            _ => panic!("expected a chunk-free Rgb16Be"),
        }

        // Grey non-demotable RGB16 -> Gray16.
        let rgb16: Vec<u16> = (0..90u32)
            .flat_map(|i| {
                let v = (i * 501 + 1) as u16;
                [v, v, v]
            })
            .collect();
        assert!(matches!(
            analyze16(&rgb16, 3),
            Reductions::ChunkFree(Reduced::Gray16Be(bytes)) if bytes.len() == 90 * 2
        ));

        // Opaque non-demotable GrayAlpha16 -> Gray16.
        let ga16: Vec<u16> = (0..90u32)
            .flat_map(|i| [(i * 501 + 1) as u16, u16::MAX])
            .collect();
        assert!(matches!(
            analyze16(&ga16, 2),
            Reductions::ChunkFree(Reduced::Gray16Be(bytes)) if bytes.len() == 90 * 2
        ));

        // A translucent grey+alpha pair -> gets a full 16-bit gray+alpha only when smaller, which
        // it never is for GrayAlpha16 input; and 16-bit noise stays as-is.
        let translucent: Vec<u16> = (0..90u32)
            .flat_map(|i| [(i * 501 + 1) as u16, (i * 703) as u16 | 1])
            .collect();
        assert!(matches!(analyze16(&translucent, 2), Reductions::None));
        let noise: Vec<u16> = (0..600u32)
            .flat_map(|i| {
                [
                    (i * 501 + 1) as u16,
                    (i * 703 + 2) as u16,
                    (i * 907 + 3) as u16,
                    (i * 111) as u16 | 1,
                ]
            })
            .collect();
        assert!(matches!(analyze16(&noise, 4), Reductions::None));
    }

    #[test]
    fn translucent_rgba16_reduces_to_grey_alpha() {
        // Grey with varied (non-demotable) alpha -> GrayAlpha16Be at 4 bytes/px vs 8.
        let rgba16: Vec<u16> = (0..90u32)
            .flat_map(|i| {
                let v = (i * 501 + 1) as u16;
                [v, v, v, (i * 703) as u16 | 1]
            })
            .collect();
        match analyze16(&rgba16, 4) {
            Reductions::ChunkFree(Reduced::GrayAlpha16Be(bytes)) => {
                assert_eq!(bytes.len(), 90 * 4);
            }
            _ => panic!("expected a chunk-free GrayAlpha16Be"),
        }
    }
}
