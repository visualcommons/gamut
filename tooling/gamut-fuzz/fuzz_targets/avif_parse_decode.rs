//! fuzz · law — gamut-avif's decode-limit law over arbitrary files (#653): parse, then decode
//! every item on the planar, RGBA8 and RGBA16 surfaces with a stub AV1 decoder.
//!
//! This is the *same* function `gamut_avif::invariants` exposes to the pinned-seed `proptest`
//! property in `crates/gamut-avif/src/decode.rs`, `decoding_stays_within_limits`. The property
//! samples well-formed coded/`grid`/`iovl` files; this tier searches the whole byte space, where
//! the interesting cases — nested derivations, a grid whose tiles overhang its output, an overlay
//! input offset off the canvas, an `ispe` that disagrees with the frame — are sparse.
//!
//! The law decodes each item twice: under limits taken from the input and under a fixed
//! [`CEILING`]. Every image the tighter run returns must be within its limits and equal to the
//! looser run's, and every refusal the tighter limits add must be a limit refusal. Beyond that,
//! the engine's own oracles apply: `gamut-avif` is `#![forbid(unsafe_code)]` and promises a typed
//! error on hostile input, so a panic, a hang or an allocation past libFuzzer's limit is a defect —
//! and the ceiling is what makes an allocation past it one. [`gamut_avif::invariants::StubDecoder`]
//! stands in for the codec: it returns a frame of at most 16 × 16 sized by the payload's last two
//! bytes and serves every call. A derivation graph that references one item many times is bounded
//! by the library's own work caps (`DecodeLimits::max_decoded_items` and `max_derived_items`, #762),
//! so a file that defeats them shows up here as a hang — the tier searches that bound, not one of
//! its own.
//!
//! The size caps come from the input's **last four bytes** rather than a prefix, so every committed
//! seed is an unmodified AVIF file: an AVIF parser keeps bytes after its last box as a trailer, and
//! the mutations libFuzzer makes there move the limits without breaking the parse. The work caps
//! are the ceiling's in both runs: the bytes before the trailer belong to the file, and letting them
//! choose a cap would have the seeds' own payload bytes refuse them before their size caps are
//! reached.
//!
//! A crash found here is **minimised and promoted into a named deterministic test** in
//! `gamut-avif`'s own suite. The corpus is a search aid, not the regression record.

#![no_main]

use gamut_avif::DecodeLimits;
use gamut_avif::invariants::decoding_stays_within_limits;
use libfuzzer_sys::fuzz_target;

/// The looser run's limits, and so the largest image any run here allocates: 256 pixels a side and
/// 16 Ki pixels, which every surface holds in at most 128 KiB per buffer. Small on purpose — a
/// nested derivation evaluates many canvases, and the ceiling bounds the time each one costs as
/// well as its memory. Its work caps — 1024 decodes and 256 derivations per call — bound how many
/// such canvases one decode evaluates, small for the same reason.
const CEILING: DecodeLimits = DecodeLimits::new(256, 1 << 14).with_item_limits(1024, 256);

fuzz_target!(|data: &[u8]| {
    let Some(tail) = data.last_chunk::<4>() else {
        return;
    };
    let max_dimension = u32::from(u16::from_le_bytes([tail[0], tail[1]]));
    let max_pixels = u64::from(u16::from_le_bytes([tail[2], tail[3]]));
    // The work caps are left at their defaults, which the law clamps to the ceiling's.
    let limits = DecodeLimits::new(max_dimension, max_pixels);
    if let Err(violation) = decoding_stays_within_limits(data, limits, CEILING) {
        panic!("{violation}");
    }
});
