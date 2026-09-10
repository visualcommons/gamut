//! fuzz · robustness — `gamut_tiff::TiffDecoder`, the entry point a TIFF from the network hits.
//!
//! `docs/testing.md`'s per-crate table names `TiffDecoder` as this crate's untrusted-input
//! surface. The decoder is `#![forbid(unsafe_code)]` and carries an explicit `MAX_IMAGE_BYTES`
//! cap, so a hostile file must end in a typed error rather than a panic, a hang or a runaway
//! allocation — the three things libFuzzer itself detects.
//!
//! ## The check beyond the crash oracle
//!
//! **The geometry that arrives equals the geometry the tags declare.** `decode_page_samples` reads
//! the page's dimensions once, runs the strip/tile assembly, the predictor pass and the photometric
//! unpack, and only then builds the `DecodedImage` those dimensions travel out in; the buffer a
//! caller receives carries whatever that stage, `RawImage::new` and `convert_from_raw` between them
//! made of it. A stage that rewrites the geometry it hands on — a crop, an orientation, a tile-grid
//! rounding — is accepted by every constructor on the way, produces no crash, and is contradicted
//! only by the tags.
//!
//! Injection that proved it fires (re-runnable): at the point `decode_page_samples` builds its
//! `DecodedImage`, swap `width` and `height` — an orientation stage, volume-preserving, so nothing
//! downstream refuses it. The committed seeds alone report it, with no search:
//! `run.sh tiff_decode <seeds> -- -runs=0` gives
//! *"page 0: decoded 4 × 6 for the 6 × 4 the tags declare"*.
//!
//! ## The sample count beside it is a structure pin, not a second check
//!
//! An earlier revision anchored this target on the number of samples the decode physically yielded
//! and claimed that count "is not read from the geometry reader at all". **That is false.**
//! `convert_from_raw` allocates its output as `ImageBuf::<Q>::zeroed(src.dims)`, and
//! `ImageBuf::zeroed` sizes that allocation with `expected_len::<P>(dims)` — so the returned
//! `as_samples().len()` is `width × height × CHANNELS` of the *dimensions*, by construction, for
//! every input. Asserting it against `info.width × info.height × CHANNELS` is therefore the
//! geometry comparison above multiplied by a constant on both sides: it can separate the two sides
//! only if `ImageBuf`'s own length-versus-dimensions invariant breaks, never if the decode pipeline
//! miscounts.
//!
//! Measured, not reasoned: the transposition above gives **exit 0 and no report** under the sample
//! count alone, because `w·h·3 == w·h·3` after a transposition for every input, while the geometry
//! comparison fires on the committed seeds immediately.
//!
//! The sample count is kept — one comparison on values already in hand — and **named a pin at the
//! site**: it pins `ImageBuf`'s constructor continuing to derive its length from its dimensions.
//! (`ImageBuf::new`, which does validate a caller-supplied buffer against dimensions, is never
//! called on this path; `RawImage::new` is the only gate the decoded samples pass through, and it
//! runs before the output buffer exists.)
//!
//! A second assertion is a pin for the same reason and labelled as one at the site: **"a page that
//! decodes must also describe"**. `decode_page_samples` calls `info::page_info` before it reads a
//! pixel, so a page `info_page` refuses cannot decode, for any input, while that body stands. It is
//! the shape that would report if `decode` ever grew its own tag reader.
//!
//! ## What is deliberately not asserted
//!
//! **`info_page` at `page_count` is refused.** `page_count` is `read(data)?.ifds.len()` and
//! `info_page` is `read(data)?.ifds.get(page)`, so the claim reduces to indexing a vector one past
//! its own length. Injecting the defect it advertised — a count that over-reports the chain —
//! produces no report, because the over-report moves both sides together.
//!
//! A defect that produces the wrong *volume* while leaving the dimensions alone does not arrive
//! here as a report either: `RawImage::new` turns it into a typed error before a caller sees it, so
//! the file is simply rejected. Measured — decoding `info.height + 1` rows produced **no** report,
//! because the strip assembly runs out of bytes and the page is refused. The live class is the
//! geometry-rewriting stage above, which is also the class the sibling `dng_decode` target checks,
//! where linearisation and active-area handling are such stages today.
//!
//! The policy is [`ConvertPolicy::permissive`] so the decode reaches the pixel and conversion
//! paths for pages the default lossless policy would refuse at the layout gate.
//!
//! A crash found here is **minimised and promoted into a named deterministic case** in
//! `gamut-tiff`'s `tests/robustness.rs`. The corpus is a search aid, not the regression record.

#![no_main]

use gamut_core::convert::ConvertPolicy;
use gamut_core::{Pixel, Rgb8};
use gamut_tiff::TiffDecoder;
use libfuzzer_sys::fuzz_target;

/// How many pages of a multi-page file one execution decodes.
///
/// A chained TIFF may declare up to `gamut-ifd`'s 65 536 directories, and decoding all of them
/// would make a single execution slow enough to look like a hang; the interesting per-page
/// behaviour is reached in the first few.
const MAX_PAGES: usize = 4;

fuzz_target!(|data: &[u8]| {
    let decoder = TiffDecoder::new().convert_policy(ConvertPolicy::permissive());

    // A file whose chain does not parse is simply refused. The other entry points are not driven
    // for it: `page_count` is `read(data)?.ifds.len()`, and `info`/`decode_page` both begin with
    // that same `read`, so they fail at the byte it already failed at and reach nothing new.
    let Ok(pages) = decoder.page_count(data) else {
        return;
    };

    for page in 0..pages.min(MAX_PAGES) {
        let info = decoder.info_page(data, page);
        let image = decoder.decode_page(data, page);
        match (&info, &image) {
            (Ok(info), Ok(image)) => {
                // The live check: the geometry that arrives, against the geometry the tags
                // declare. Every stage between the tag reader and this buffer could rewrite it.
                let decoded = image.dimensions();
                assert_eq!(
                    (decoded.width, decoded.height),
                    (info.width, info.height),
                    "page {page}: decoded {} × {} for the {} × {} the tags declare",
                    decoded.width,
                    decoded.height,
                    info.width,
                    info.height
                );
                // Structure pin, not a check: `ImageBuf` sizes its storage from its own
                // dimensions, so for every input this is the assertion above times
                // `Rgb8::CHANNELS` on both sides (see the module docs).
                let declared = (info.width as usize)
                    .checked_mul(info.height as usize)
                    .and_then(|n| n.checked_mul(<Rgb8 as Pixel>::CHANNELS));
                assert_eq!(
                    Some(image.as_samples().len()),
                    declared,
                    "page {page}: decoded {} samples for the {} × {} × {} the tags declare",
                    image.as_samples().len(),
                    info.width,
                    info.height,
                    <Rgb8 as Pixel>::CHANNELS
                );
            }
            // Structure pin, not a check: decoding calls the tag reader first, so this cannot
            // fail by input while it does (see the module docs).
            (Err(error), Ok(_)) => {
                panic!("page {page} decoded although it could not be described: {error}")
            }
            _ => {}
        }
    }
});
