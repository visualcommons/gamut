//! fuzz · robustness — `gamut_png::PngDecoder`, the entry point a PNG from the network hits.
//!
//! `docs/testing.md`'s per-crate table names `PngDecoder` as this crate's untrusted-input surface.
//! The crate is `#![deny(unsafe_code)]` with no `unsafe` site in its decode path. The decoder's
//! default limits are the workspace's `DecodeLimits` (32768 per side, 16384² pixels, no byte
//! budget, #1226) and a 16 MiB metadata budget; a conformant 16384² RGBA16 image under them is a
//! 2 GiB sample buffer, past libFuzzer's malloc limit. The target therefore adds the 64 MiB decoded
//! sample budget a malloc-limited caller sets — it bounds both the inflated scanline stream and the
//! sample buffer — so a zlib bomb or a declared `2^31 × 2^31` image must end in a typed error,
//! never a panic, a hang, or an allocation past the limit, and an out-of-memory report is a
//! defect rather than a conformant file.
//!
//! It drives the **rich** [`PngDecoder::decode`], not a typed `decode_image`: that is the one path
//! that also inflates and parses every ancillary payload (iCCP, zTXt/iTXt, eXIf, cICP, …), and the
//! typed path is the same stream walk with the metadata skipped.
//!
//! This target lists **no check beyond the crash oracle**, like `dng_decode`: libFuzzer's own
//! detectors are the whole oracle. A crash is minimised and promoted into a named deterministic
//! case in `gamut-png`'s own suite; the corpus is a search aid, not the regression record.

#![no_main]

use gamut_png::{DecodeLimits, PngDecoder};
use libfuzzer_sys::fuzz_target;

/// The decoded sample budget the target sets: 64 MiB, the figure the decoder's default carried
/// before #1226.
const MAX_IMAGE_BYTES: usize = 64 << 20;

fuzz_target!(|data: &[u8]| {
    let decoder =
        PngDecoder::new().with_limits(DecodeLimits::new().with_max_image_bytes(MAX_IMAGE_BYTES));
    let _ = decoder.decode(data);
});
