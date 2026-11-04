//! fuzz · robustness — `gamut_png::PngDecoder`, the entry point a PNG from the network hits.
//!
//! `docs/testing.md`'s per-crate table names `PngDecoder` as this crate's untrusted-input surface.
//! The crate is `#![deny(unsafe_code)]` with no `unsafe` site in its decode path, and the decoder's
//! **default** limits are its hostile-input posture: spec-maximum dimensions, a 64 MiB decoded
//! sample budget that bounds both the inflated scanline stream and the sample buffer, and a 16 MiB
//! metadata budget. The target runs with exactly those defaults, because they are what a caller
//! that sets nothing gets — a zlib bomb or a declared `2^31 × 2^31` image must end in a typed
//! error, never a panic, a hang, or an allocation past libFuzzer's malloc limit.
//!
//! It drives the **rich** [`PngDecoder::decode`], not a typed `decode_image`: that is the one path
//! that also inflates and parses every ancillary payload (iCCP, zTXt/iTXt, eXIf, cICP, …), and the
//! typed path is the same stream walk with the metadata skipped.
//!
//! This target lists **no check beyond the crash oracle**, like `dng_decode`: libFuzzer's own
//! detectors are the whole oracle. A crash is minimised and promoted into a named deterministic
//! case in `gamut-png`'s own suite; the corpus is a search aid, not the regression record.

#![no_main]

use gamut_png::PngDecoder;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = PngDecoder::new().decode(data);
});
