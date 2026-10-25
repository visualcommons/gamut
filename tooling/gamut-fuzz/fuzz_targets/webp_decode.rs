//! fuzz · robustness — `gamut_webp::WebpDecoder`, the entry point a WebP from the network hits.
//!
//! `docs/testing.md`'s per-crate table names `WebpDecoder` as this crate's untrusted-input
//! surface. The crate is `#![forbid(unsafe_code)]` — that is the stated reason it ships its own
//! VP8/VP8L decoders instead of wrapping libwebp — so a hostile file must end in a typed error,
//! never a panic, a hang, or an allocation past libFuzzer's malloc limit.
//!
//! The decoder has no configurable limits: its bound is the format's own 14-bit dimension fields
//! (16 384 × 16 384 at most, so at most 1 GiB of RGBA output), which is inside libFuzzer's default
//! 2048 MB malloc limit. A report here is therefore an allocation the decoder sized from something
//! other than the frame it is decoding.
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
