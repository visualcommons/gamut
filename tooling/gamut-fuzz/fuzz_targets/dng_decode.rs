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
//! This target lists **no check beyond the crash oracle**, and that oracle is what found #564. Its
//! two assertions are both **structure pins**, labelled as such at the site.
//!
//! **The sample count.** The decoded raw holds exactly `width × height × samples_per_pixel`
//! samples — but no decode-pipeline defect can make an input fail that. `RawImage`'s fields are
//! private, and both constructors (`new_cfa`, `new_linear_raw`) refuse a buffer of any other length
//! through `check_sample_count` before the value exists. After construction the decoder only calls
//! `with_*` setters — active area, default crop, levels, masked areas, opcode lists, CFA colours
//! and layout — none of which touches `samples` or `dims`, and it never linearises or crops the
//! samples themselves: those are *recorded* for a consumer, not applied. So the value that arrives
//! is the value the constructor checked. The only defect the assertion can report is one inside a
//! constructor, after its own gate — which is exactly the injection an earlier revision recorded
//! as its evidence (a `new_cfa` that pushes a sample past `check_sample_count`), and which no file
//! can express. It is kept because it is free on values already in hand and would report a future
//! setter that did rewrite the buffer; it is not listed as a check.
//!
//! **The digest verdict.** `verify_new_raw_image_digest` is driven for its own reach: on a
//! lossy-compressed raw it walks the chunk grid and digests the compressed chunks, which `decode`
//! never does. Its verdict is compared against the decoded model as a **structure pin, not a
//! differential** — both sides read
//! `NewRawImageDigest` out of IFD 0 with the same expression, so the comparison cannot fail while
//! those two bodies agree. It is kept because the two are genuinely separate readers that a future
//! change could let drift apart (a `verify` that started selecting the raw IFD's digest, say), and
//! it costs nothing: the call is made anyway, for the crash oracle.
//!
//! ## Why a failed digest check is a classified outcome, not a crash
//!
//! `verify_new_raw_image_digest` is not a second call to `decode`. It re-reads the container and
//! then, depending on the file's own `Compression` code, takes one of two routes — and only one of
//! them is a subset of what `decode` did. It is therefore free to return `Err` on a file that
//! decoded, and an earlier draft turned that into `expect(...)`: a **false crash**,
//! indistinguishable from a real one until a human minimises it, on a tier that runs unattended
//! with no human at the other end. Nothing in either function's documented contract promises
//! "everything that decodes also verifies" — `verify`'s own docs bound its errors by `decode`'s
//! only *for lossless storage* — so the `Err` arm is simply a case with nothing to compare, and
//! the target returns instead of panicking.
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

    // Structure pin, not a check: both constructors enforce this count and no later stage touches
    // `samples` or `dims`, so no input can fail it while that holds (see the module docs).
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

    // The digest route may legitimately refuse a file `decode` accepted (see the module docs), so
    // an `Err` is a case with nothing to cross-check, not a defect. Only a verdict is comparable.
    let Ok(verdict) = verdict else {
        return;
    };
    assert_eq!(
        decoded.new_raw_image_digest.is_none(),
        verdict == DigestCheck::Absent,
        "digest tag {:?} but verdict {verdict:?}",
        decoded.new_raw_image_digest
    );
});
