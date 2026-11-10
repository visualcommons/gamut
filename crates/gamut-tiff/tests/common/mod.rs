//! The pixel patterns the two libtiff cross-check suites share.
//!
//! `oracle.rs` and `bigtiff.rs` carried byte-identical copies of these. That is not incidental:
//! `bigtiff.rs` exists to prove the 64-bit container widens *only* the container, so its cases
//! deliberately mirror the classic ones. Two copies of the mirror is one copy too many — if they
//! ever drift the comparison stops meaning what its module doc says it means.
//!
//! **Only these two functions are shared, and that is deliberate.** Eight files in this crate
//! define an `rgb_pattern`, in six distinct variants, and the other four are *not* duplicates:
//! `lzw.rs` and `packbits.rs` build run-length-friendly bands sized to their coders,
//! `predictor.rs` builds smooth gradients so horizontal differencing has something to remove, and
//! `tiles.rs` and `image_roundtrip.rs` pick constants that vary across tile boundaries. Each is
//! payload chosen for the feature under test, and collapsing them into one "shared" pattern would
//! quietly weaken every one of those suites.

#![allow(dead_code)] // each integration-test binary uses a different subset

/// Three channels that vary independently and at different rates, so a channel swap, a stride
/// error and an off-by-one row all produce visibly different output.
pub fn rgb_pattern(w: u32, h: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity((w * h * 3) as usize);
    for y in 0..h {
        for x in 0..w {
            v.push((x.wrapping_mul(31).wrapping_add(y)) as u8);
            v.push((y.wrapping_mul(17) ^ x) as u8);
            v.push((x.wrapping_add(y).wrapping_mul(5)) as u8);
        }
    }
    v
}

/// A single channel whose value advances by an odd stride, so a row-length error shifts it
/// visibly rather than landing back on the same byte.
pub fn gray_pattern(w: u32, h: u32) -> Vec<u8> {
    (0..w * h)
        .map(|i| (i.wrapping_mul(97) >> 1) as u8)
        .collect()
}

/// Lays out hand-built pages — each a directory and the strips its `StripOffsets` /
/// `StripByteCounts` point at — through gamut-ifd's shared block writer, the same one the encoder
/// uses, for fixtures the encoder deliberately never writes.
pub fn write_pages(
    order: gamut_ifd::ByteOrder,
    variant: gamut_ifd::Variant,
    pages: &[(gamut_ifd::Ifd, Vec<Vec<u8>>)],
) -> gamut_core::Result<Vec<u8>> {
    let runs: Vec<_> = pages
        .iter()
        .map(|(_, strips)| gamut_ifd::BlockRun::strips(strips))
        .collect();
    gamut_ifd::write_with_blocks(order, variant, &runs, |placed| {
        pages
            .iter()
            .zip(placed)
            .map(|((ifd, _), run)| {
                let mut ifd = ifd.clone();
                run.install(&mut ifd);
                ifd
            })
            .collect()
    })
}

/// [`write_pages`] for one page.
pub fn write_image(
    order: gamut_ifd::ByteOrder,
    variant: gamut_ifd::Variant,
    ifd: &gamut_ifd::Ifd,
    strips: &[Vec<u8>],
) -> gamut_core::Result<Vec<u8>> {
    write_pages(order, variant, &[(ifd.clone(), strips.to_vec())])
}
