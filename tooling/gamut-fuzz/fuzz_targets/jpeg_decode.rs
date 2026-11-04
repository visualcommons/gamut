//! fuzz · robustness — `gamut_jpeg::JpegDecoder`, the entry point a JPEG from the network hits.
//!
//! `docs/testing.md`'s per-crate table names `JpegDecoder` as this crate's untrusted-input
//! surface. The crate is `#![deny(unsafe_code)]` with no `unsafe` site in its decode path.
//!
//! **The decoder's default is bounded** (#1024): `JpegDecoder::new()` caps dimensions at the
//! format's own 65 535-sample ceiling and the native raster at 64 MiB, the figure `PngDecoder` uses
//! as its default. The target's explicit `with_max_dimensions` and `with_max_image_bytes` calls
//! restate exactly that default. They are kept so that a later change to the crate's default
//! cannot silently change what this target searches. Without a budget, every `SOF` the engine
//! writes with a large frame would be a 12 GB allocation that libFuzzer reports at once, which is a
//! report about a default rather than a search of the decoder.
//!
//! The policy is `permissive`, so a CMYK/YCCK stream is converted rather than refused at the
//! layout check, and its colour conversion is searched too. [`gamut_jpeg::metadata`] is driven as
//! well: it walks the `APPn` segments (Exif, XMP, ICC chunk reassembly, Adobe) without decoding a
//! scan, which `decode_image` does not surface.
//!
//! This target lists **no check beyond the crash oracle**, like `dng_decode`. A crash is minimised
//! and promoted into a named deterministic case in `gamut-jpeg`'s own suite.

#![no_main]

use gamut_core::convert::ConvertPolicy;
use gamut_core::{DecodeImage, Rgb8};
use gamut_jpeg::JpegDecoder;
use libfuzzer_sys::fuzz_target;

/// The decoded raster budget, matching `gamut-png`'s default `max_image_bytes`.
const MAX_IMAGE_BYTES: usize = 64 << 20;

fuzz_target!(|data: &[u8]| {
    let _ = gamut_jpeg::metadata(data);
    let decoder = JpegDecoder::new()
        .with_max_dimensions(u32::from(u16::MAX), u32::from(u16::MAX))
        .with_max_image_bytes(MAX_IMAGE_BYTES)
        .convert_policy(ConvertPolicy::permissive());
    let _ = DecodeImage::<Rgb8>::decode_image(&decoder, data);
});
