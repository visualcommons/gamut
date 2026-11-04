//! fuzz · robustness — `gamut_icc::IccProfile::parse`, the entry point an embedded ICC profile hits.
//!
//! `docs/testing.md`'s per-crate table names `IccProfile::parse` as this crate's untrusted-input
//! surface. A profile arrives from whatever image carried it (PNG `iCCP`, JPEG `APP2` chunks, WebP
//! `ICCP`, an ISOBMFF `colr`, a JPEG XL header) and is a tag table whose every offset, size and
//! element count the writer chose. `parse` decodes every tag it knows eagerly — curves, LUTs,
//! `mluc` text, named colours, `dict` — so this one call reaches the whole tag-type layer. The
//! crate is `#![forbid(unsafe_code)]`, so a hostile profile must end in a typed error, never a
//! panic, a hang, or an allocation past libFuzzer's malloc limit: a `clut` whose grid points and
//! channel count multiply past the tag's own size is the canonical case.
//!
//! The lenient default is driven, not `IccReader::strict`: strict mode only adds refusals on top
//! of the same parse, so it reaches nothing the lenient one does not.
//!
//! This target lists **no check beyond the crash oracle**, like `dng_decode`. A crash is minimised
//! and promoted into a named deterministic case in `gamut-icc`'s own suite.

#![no_main]

use gamut_icc::IccProfile;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = IccProfile::parse(data);
});
