//! fuzz · robustness — `gamut_webp::WebpDecoder`, the entry point a WebP from the network hits.
//!
//! `docs/testing.md`'s per-crate table names `WebpDecoder` as this crate's untrusted-input
//! surface. The crate is `#![forbid(unsafe_code)]` — that is the stated reason it ships its own
//! VP8/VP8L decoders instead of wrapping libwebp — so a hostile file must end in a typed error,
//! never a panic, a hang, or an allocation past libFuzzer's malloc limit.
//!
//! The decoder's default limits (the workspace's `DecodeLimits`: 32768 per side, 16384² pixels,
//! no byte budget, #1226) admit every frame the format's 14-bit dimension fields can declare,
//! 16 384 × 16 384 at most. That does **not** fit libFuzzer's default `-rss_limit_mb=2048`: a
//! conformant lossless frame at that size holds its 1 GiB ARGB `Vec<u32>` and its 1 GiB RGBA
//! output alive together (`WebpDecoder::decode_native`). The target therefore sets the 64 MiB byte
//! budget (#1025, charged at four bytes per pixel) a malloc-limited caller sets, so an
//! out-of-memory report is a mis-sized allocation in this crate, never a conformant file.
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
use gamut_webp::{DecodeLimits, WebpDecoder};
use libfuzzer_sys::fuzz_target;

/// The decoded frame budget the target sets: 64 MiB, a 4096 × 4096 RGBA frame.
const MAX_IMAGE_BYTES: usize = 64 << 20;

fuzz_target!(|data: &[u8]| {
    let decoder =
        WebpDecoder::new().with_limits(DecodeLimits::new().with_max_image_bytes(MAX_IMAGE_BYTES));
    let _ = DecodeImage::<Rgba8>::decode_image(&decoder, data);
});
