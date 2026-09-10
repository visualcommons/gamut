//! fuzz · robustness — `gamut_dng::DngDecoder`, the entry point a camera file from anywhere hits.
//!
//! `docs/testing.md`'s per-crate table names `DngDecoder` as this crate's untrusted-input
//! surface. A DNG is a TIFF/EP sub-IFD tree whose every geometry, level and opcode field is a
//! number the file chose, so the decoder is `#![forbid(unsafe_code)]` and a hostile file must end
//! in a typed error — never a panic, a hang, or an allocation sized from a declared field rather
//! than from the bytes that are actually there. libFuzzer's malloc hook is what makes that last
//! one observable: an over-sized `Vec::with_capacity` costs no resident memory on an
//! overcommitting kernel, so the engine's limit, not the OS, is the oracle.
//!
//! Two checks beyond the crash oracle:
//!
//! - **the raw image is self-consistent**: a decoded `RawImage` holds exactly
//!   `width × height × samples_per_pixel` samples. The constructors enforce it; a decode path
//!   that builds one another way is what this notices.
//! - **the digest verdict agrees with the decoded model**: the file either carries a
//!   `NewRawImageDigest` — in which case `verify_new_raw_image_digest` must reach a verdict — or
//!   it does not, in which case the verdict must be `Absent`. Two entry points read the same tag
//!   by different routes (`decode` models it, `verify` re-reads it), so a disagreement means one
//!   of them found a tag the other did not.
//!
//! A crash found here is **minimised and promoted into a named deterministic case** in
//! `gamut-dng`'s own suite. The corpus is a search aid, not the regression record.

#![no_main]

use gamut_dng::{DigestCheck, DngDecoder};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let decoder = DngDecoder::new();

    // The digest path reads the container and, for losslessly-stored raws, the image — it is a
    // second route through the same tree, and it must be refused rather than crash on its own.
    let verdict = decoder.verify_new_raw_image_digest(data);

    let Ok(decoded) = decoder.decode(data) else {
        return;
    };

    let dims = decoded.raw.dimensions();
    let expected = (dims.width as usize)
        .checked_mul(dims.height as usize)
        .and_then(|n| n.checked_mul(usize::from(decoded.raw.samples_per_pixel())));
    assert_eq!(
        Some(decoded.raw.samples().len()),
        expected,
        "decoded raw holds {} samples for {dims:?} × {} planes",
        decoded.raw.samples().len(),
        decoded.raw.samples_per_pixel()
    );

    // `decode` succeeded, so the container is walkable and the digest path must have reached a
    // verdict too — the two routes disagree only if they selected different directories.
    let verdict = verdict.expect("digest check on a file that decoded");
    assert_eq!(
        decoded.new_raw_image_digest.is_none(),
        verdict == DigestCheck::Absent,
        "digest tag {:?} but verdict {verdict:?}",
        decoded.new_raw_image_digest
    );
});
