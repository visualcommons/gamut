//! fuzz · robustness — `gamut_av1::Av1Decoder::inspect`, the AV1 still decoder's untrusted-input
//! surface.
//!
//! `gamut-av1`'s decoder today decodes the **framing and header layer** only — the OBU walk (§5.3),
//! the sequence header (§5.5), the uncompressed frame header (§5.9) and tile-group framing
//! (§5.11.1) — and `inspect` is the one entry point that reaches all of it. Every field it reads is
//! a number the bitstream chose, and the crate is `#![forbid(unsafe_code)]` with a `DecodeLimits`
//! cap checked against the header before any allocation the header sizes, so a hostile stream must
//! end in a typed error, never a panic, a hang, or an allocation past libFuzzer's malloc limit.
//!
//! `gamut-avif`'s `avif_parse_decode` does not reach this code: it decodes through a stub AV1
//! decoder so that it searches the container, not the codestream. This target is the codestream
//! half. When sample decoding lands (#259) it extends this target's reach without a new one.
//!
//! This target lists **no check beyond the crash oracle**, like `dng_decode`. A crash is minimised
//! and promoted into a named deterministic case in `gamut-av1`'s own suite.

#![no_main]

use gamut_av1::Av1Decoder;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = Av1Decoder::new().inspect(data);
});
