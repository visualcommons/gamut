//! fuzz · robustness — `gamut_tiff::TiffDecoder`, the entry point a TIFF from the network hits.
//!
//! `docs/testing.md`'s per-crate table names `TiffDecoder` as this crate's untrusted-input
//! surface. The decoder is `#![forbid(unsafe_code)]` and carries an explicit `MAX_IMAGE_BYTES`
//! cap, so a hostile file must end in a typed error rather than a panic, a hang or a runaway
//! allocation — the three things libFuzzer itself detects.
//!
//! The check beyond the crash oracle is that **the decoded volume matches the declared geometry**:
//! a page that decodes yields exactly `width × height × Rgb8::CHANNELS` samples, where the width
//! and height are the ones `info_page` read out of the tags. The sample count is not read from the
//! geometry reader at all — it is what the strip/tile assembly, the predictor pass and the
//! photometric unpack physically produced — so the check reaches that whole pipeline rather than
//! the handful of lines that copy a described number into a decoded one.
//!
//! Injection that proved it fires (re-runnable): at the point `decode_page_samples` builds its
//! `DecodedImage`, trim the last row from the samples *and* report `height - 1` — a crop stage
//! that describes what it cropped. It is internally consistent, so `RawImage::new` and
//! `ImageBuf::new` both accept it and nothing crashes; only the declared geometry contradicts it.
//! Run as `run.sh tiff_decode <seeds> -- -runs=0`, the committed seeds alone report it:
//! *"page 0: decoded 54 samples for the 6 × 4 × 3 the tags declare"*.
//!
//! Two things this deliberately does **not** assert, both because they cannot fail by input:
//!
//! - **`info_page` at `page_count` is refused.** `page_count` is `read(data)?.ifds.len()` and
//!   `info_page` is `read(data)?.ifds.get(page)`, so the claim reduces to indexing a vector one
//!   past its own length. Injecting the defect it advertised — a count that over-reports the
//!   chain — produces no report, because the over-report moves both sides together.
//! - **decoded dimensions equal described dimensions.** The two sides are one reader:
//!   `decode_page_samples` states outright that "everything the page *declares* comes from one
//!   shared reader", so a transposition inside `info::page_info` hands every caller a transposed
//!   image and this comparison stays quiet. It fires only for a defect in the few lines between
//!   that reader and the returned buffer, which is a reach the sample count already covers.
//!
//! One assertion beside it is kept and **named a structure pin** rather than advertised as a
//! check: "a page that decodes must also describe". `decode_page_samples` calls `info::page_info`
//! before it reads a pixel, so a page `info_page` refuses cannot decode, for any input, while that
//! body stands. It costs nothing — both calls are made anyway for the crash oracle — and it is the
//! shape that would report if `decode` ever grew its own tag reader.
//!
//! What the sample count does not see either, stated so nobody over-reads it: a defect that
//! produces the wrong *volume* while leaving the dimensions alone is turned into a typed error by
//! `RawImage::new`/`ImageBuf::new` before it can reach a caller, so it arrives here as a rejected
//! file rather than as a report. Measured, not assumed: decoding `info.height + 1` rows — the
//! first injection tried — produced **no** report, because the strip assembly runs out of bytes
//! and the page is refused. The live class is a stage that rewrites the geometry it hands on — a
//! crop, an orientation, a tile-grid rounding — which both constructors accept and only the
//! declared geometry contradicts. That is the same class the sibling `dng_decode` target checks,
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

    let Ok(pages) = decoder.page_count(data) else {
        // A file whose chain does not parse must still be refused — not crash — by the entry
        // points that do not consult `page_count` first.
        let _ = decoder.info(data);
        let _ = decoder.decode_page(data, 0);
        return;
    };

    for page in 0..pages.min(MAX_PAGES) {
        let info = decoder.info_page(data, page);
        let image = decoder.decode_page(data, page);
        match (&info, &image) {
            (Ok(info), Ok(image)) => {
                // The declared geometry, times the channel count of the layout that was asked for,
                // against the samples the decode actually produced.
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
