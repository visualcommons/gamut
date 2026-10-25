//! fuzz · robustness — `gamut_riff::RiffReader` and the WebP chunk readers built on it.
//!
//! `docs/testing.md`'s per-crate table names `RiffReader` as this crate's untrusted-input surface.
//! The crate is `#![forbid(unsafe_code)]` and every chunk size it reads is a number the file chose,
//! so a hostile file must end in a typed error, never a panic, a hang, or an allocation past
//! libFuzzer's malloc limit.
//!
//! Four readers share that surface and each is driven, because each carries its own walk over the
//! chunk list rather than one calling another's:
//!
//! - [`RiffReader`], iterated to its end — the raw chunk walk;
//! - [`MetadataChunks::read`] — first-wins collection of `ICCP`/`EXIF`/`XMP `/`C2PA`;
//! - [`WebpLayout::parse`] — the reconstruction-chunk ordering rule, and the `VP8X` header;
//! - [`c2pa_summary`] — the C2PA manifest-store chunk accounting.
//!
//! This target lists **no check beyond the crash oracle**, like `dng_decode`. A crash is minimised
//! and promoted into a named deterministic case in `gamut-riff`'s own suite.

#![no_main]

use gamut_riff::{MetadataChunks, RiffReader, WebpLayout, c2pa_summary};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(reader) = RiffReader::new(data) {
        // Driven to `None` rather than stopped at the first `Err`: the reader promises to fuse on
        // its first error, so a reader that did not would spin here and libFuzzer would report
        // the hang as a timeout.
        reader.for_each(drop);
    }
    let _ = MetadataChunks::read(data);
    let _ = WebpLayout::parse(data);
    let _ = c2pa_summary(data);
});
