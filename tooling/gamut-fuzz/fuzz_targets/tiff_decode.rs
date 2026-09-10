//! fuzz · robustness — `gamut_tiff::TiffDecoder`, the entry point a TIFF from the network hits.
//!
//! `docs/testing.md`'s per-crate table names `TiffDecoder` as this crate's untrusted-input
//! surface. The decoder is `#![forbid(unsafe_code)]` and carries an explicit `MAX_IMAGE_BYTES`
//! cap, so a hostile file must end in a typed error rather than a panic, a hang or a runaway
//! allocation — the three things libFuzzer itself detects.
//!
//! Two further checks make the target able to fail for something other than a crash:
//!
//! - **the page index is bounded by `page_count`**: `info_page` at the count itself must be
//!   refused. A count that over-reports the chain is how an out-of-range page reaches the tag
//!   reader at all.
//! - **describing and decoding agree**: a page that decodes must also describe, and the two must
//!   report the same dimensions. `info` reads tags only and `decode_page` reads pixels, so a
//!   disagreement means the two paths read the geometry differently — exactly the split that
//!   turns a size check into a false guarantee.
//!
//! The policy is [`ConvertPolicy::permissive`] so the decode reaches the pixel and conversion
//! paths for pages the default lossless policy would refuse at the layout gate.
//!
//! A crash found here is **minimised and promoted into a named deterministic case** in
//! `gamut-tiff`'s `tests/robustness.rs`. The corpus is a search aid, not the regression record.

#![no_main]

use gamut_core::convert::ConvertPolicy;
use gamut_tiff::TiffDecoder;
use libfuzzer_sys::fuzz_target;

/// How many pages of a multi-page file one execution decodes.
///
/// A chained TIFF may declare up to `gamut-ifd`'s 65 536 directories, and decoding all of them
/// would make a single execution slow enough to look like a hang; the interesting per-page
/// behaviour is reached in the first few. The page-index bound below is still checked against the
/// *full* count.
const MAX_PAGES: usize = 4;

fuzz_target!(|data: &[u8]| {
    let decoder = TiffDecoder::new().convert_policy(ConvertPolicy::permissive());

    let Ok(pages) = decoder.page_count(data) else {
        // A file whose chain does not parse must still be refused — not crash — by the entry
        // points that do not consult `page_count` first.
        let _ = decoder.info(data);
        let _ = decoder.decode_page(data, 0);
        return;
    };

    // One past the last page is out of range, whatever the chain claimed.
    assert!(
        decoder.info_page(data, pages).is_err(),
        "page {pages} described although page_count is {pages}"
    );

    for page in 0..pages.min(MAX_PAGES) {
        let info = decoder.info_page(data, page);
        let image = decoder.decode_page(data, page);
        match (&info, &image) {
            (Ok(info), Ok(image)) => {
                assert_eq!(
                    (image.width(), image.height()),
                    (info.width, info.height),
                    "page {page}: decoded geometry differs from the described geometry"
                );
            }
            (Err(error), Ok(_)) => {
                panic!("page {page} decoded although it could not be described: {error}")
            }
            _ => {}
        }
    }
});
