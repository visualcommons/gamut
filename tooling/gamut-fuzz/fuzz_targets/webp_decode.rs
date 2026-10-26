//! fuzz · robustness — `gamut_webp::WebpDecoder`, the entry point a WebP from the network hits.
//!
//! `docs/testing.md`'s per-crate table names `WebpDecoder` as this crate's untrusted-input
//! surface. The crate is `#![forbid(unsafe_code)]` — that is the stated reason it ships its own
//! VP8/VP8L decoders instead of wrapping libwebp — so a hostile file must end in a typed error,
//! never a panic, a hang, or an allocation past libFuzzer's malloc limit.
//!
//! The decoder has no configurable limits (#1025): its only bound is the format's own 14-bit
//! dimension fields, 16 384 × 16 384 at most. That bound does **not** fit libFuzzer's default
//! `-rss_limit_mb=2048`: a conformant lossless frame at that size holds its 1 GiB ARGB `Vec<u32>`
//! and its 1 GiB RGBA output alive together (`WebpDecoder::decode_native`), so an out-of-memory
//! report here can be a conformant file rather than a mis-sized allocation. Triage one by the
//! frame size the input declares: a report whose declared frame needs the memory is the missing
//! limit #1025 tracks, and only one whose frame does not is a defect in this crate. Once #1025
//! gives the decoder a caller-set limit, this target sets it and the ambiguity goes away.
//!
//! It decodes to `Rgba8` with the default (lossless) policy: an opaque image widens to it without
//! permission, so both the lossy `VP8 ` path (with or without an `ALPH` chunk) and the lossless
//! `VP8L` path reach their pixel output through one call.
//!
//! This target lists **no check beyond the crash oracle**, like `dng_decode`. The container walk
//! beneath it is searched on its own by `riff_chunks`. A crash is minimised and promoted into a
//! named deterministic case in `gamut-webp`'s own suite.

#![no_main]

use gamut_core::{DecodeImage, Rgba8};
use gamut_webp::WebpDecoder;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = DecodeImage::<Rgba8>::decode_image(&WebpDecoder::new(), data);
});
