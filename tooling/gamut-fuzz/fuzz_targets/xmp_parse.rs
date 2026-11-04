//! fuzz · robustness — `gamut_xmp::XmpMeta::from_packet`, the entry point an embedded XMP packet
//! hits.
//!
//! `docs/testing.md`'s per-crate table names `parse` as this crate's untrusted-input surface. A
//! packet arrives from whatever file carried it — a JPEG `APP1`, a PNG `iTXt`, a WebP `XMP `
//! chunk, an ISOBMFF `mime` item, a sidecar — and `from_packet` is the whole read: the
//! `<?xpacket?>` envelope scan, the XML parse, and the RDF/XML interpretation into a property
//! graph. The crate is `#![forbid(unsafe_code)]`, so a hostile packet must end in a typed error,
//! never a panic, a hang, or an allocation past libFuzzer's malloc limit — including on deep
//! element nesting, which libFuzzer reports as a stack overflow if the tree walk follows it.
//!
//! This target lists **no check beyond the crash oracle**, like `dng_decode`. A crash is minimised
//! and promoted into a named deterministic case in `gamut-xmp`'s own suite.

#![no_main]

use gamut_xmp::XmpMeta;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = XmpMeta::from_packet(data);
});
