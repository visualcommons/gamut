//! The size contract (issue #224): gamut's output measured against libpng at zlib level 9, with a
//! per-case budget that each carries its own written justification.
//!
//! `README.md` and `STATUS.md` have long claimed "output size is benchmarked against libpng at
//! maximum compression". `benches/encode.rs` now prints that comparison, but a bench asserts
//! nothing and is not in the per-PR gate. This file is what makes the claim enforceable: a
//! regression in the crate's reason to exist fails the build, which is the same mechanism
//! `gamut-deflate`'s ratio contract and `gamut-webp/tests/effort.rs` use.
//!
//! Budgets are *measured*, not aspirational, and they are one-sided. The table below records what
//! each case actually achieves alongside what is asserted, so drift shows up in review rather
//! than as a surprise red build. Deliberately no "budgets are still tight" assertion: it would
//! fail on a libpng point release for no correctness reason.

mod common;

use gamut_core::{Dimensions, EncodeImage, ImageRef, Rgb8, Rgb16, Rgba8};
use gamut_png::{FilterStrategy, Level, PngEncoder, deconstruct};

/// One case's size budget against libpng at zlib level 9.
struct Budget {
    /// Row label; matches `benches/encode.rs`, plus a `+clean` suffix where this row differs from
    /// its neighbour only by [`PngEncoder::with_transparent_cleanup`].
    name: &'static str,
    /// Corpus generator key. Distinct from `name` so a cleaned row can share a fixture with its
    /// uncleaned twin rather than duplicating the pixels.
    fixture: &'static str,
    /// The square side to measure at.
    side: u32,
    /// Bits per sample of the source layout: 8 for every row but the 16-bit one.
    depth: u8,
    /// Whether to enable [`PngEncoder::with_transparent_cleanup`].
    cleanup: bool,
    /// The most gamut's file may measure as a fraction of libpng's. `1.00` reads "never larger".
    ///
    /// Derived, not chosen: `measured × (1 + headroom)` rounded up to two decimals, where the
    /// headroom is 5% unless this row's `why` names the other component whose drift it absorbs.
    max_ratio: f64,
    /// What the case actually measures at this revision, so drift is visible in review.
    ///
    /// To refresh the whole table after an encoder change: set every `max_ratio` to `2.00`, run
    /// `cargo test -p gamut-png --test size_contract
    /// gamut_never_exceeds_its_size_budget_against_libpng9 -- --exact --nocapture`, paste each
    /// printed ratio back into `measured`, then re-derive `max_ratio` by the rule above.
    measured: f64,
    /// Why this number and not a tighter one — which stage spends the bytes.
    why: &'static str,
}

/// Every budget carries its justification, and every `max_ratio` is derived from the `measured`
/// beside it rather than chosen -- see [`Budget::max_ratio`].
///
/// Measured at 128x128 (a quarter of the bench's pixel count, so the suite stays quick enough for
/// the coverage and mutation lanes) except `tiny_rgb8`, which is the bench's own 16x16 row, and
/// `demotable_rgb16`, which halves the side again because its samples are twice as wide.
///
/// These ratios are **not** comparable with the bench's 256x256 figures and must be read
/// separately. Every fixed cost -- the signature, IHDR, PLTE/tRNS, IEND, and DEFLATE's own framing
/// -- is amortised over a quarter as many pixels here, which systematically disadvantages exactly
/// the rows where a reduction wins: the gap runs to about 30 percentage points on `gradient_rgb8`
/// and `palette64_rgba8`. `STATUS.md` records the 256x256 table; this one gates.
const BUDGETS: &[Budget] = &[
    Budget {
        name: "gradient_rgb8",
        fixture: "gradient_rgb8",
        side: 128,
        depth: 8,
        cleanup: false,
        max_ratio: 0.82,
        measured: 0.772,
        why: "no reduction applies, so this is filtering plus DEFLATE against libpng's own \
              adaptive filtering. The margin is thin by nature -- both encoders are doing the \
              same job -- so the budget only guards against losing outright.",
    },
    Budget {
        name: "photo_rgb8",
        fixture: "photo_rgb8",
        side: 128,
        depth: 8,
        cleanup: false,
        max_ratio: 0.83,
        measured: 0.731,
        why: "smooth photographic content: palette-hostile, so again pure filtering + DEFLATE, \
              and the win is the optimal parse. Coupled to gamut-deflate's own Best/z9 column by \
              construction: if that regresses, this row moves with it, so it carries 13% headroom \
              where the others carry 5%.",
    },
    Budget {
        name: "noise_rgb8",
        fixture: "noise_rgb8",
        side: 128,
        depth: 8,
        cleanup: false,
        max_ratio: 1.02,
        measured: 0.998,
        why: "incompressible, so both encoders fall back to stored blocks and the file is \
              slightly larger than the raw samples. Above 1.0 because there is nothing to win \
              here, not because we lose; the 2% margin covers stored-block framing only.",
    },
    Budget {
        name: "grey_as_rgb8",
        fixture: "grey_as_rgb8",
        side: 128,
        depth: 8,
        cleanup: false,
        max_ratio: 0.62,
        measured: 0.582,
        why: "R=G=B everywhere, so auto-reduce drops two channels before DEFLATE runs. A \
              structural win libpng does not attempt.",
    },
    Budget {
        name: "flat_rgba8",
        fixture: "flat_rgba8",
        side: 128,
        depth: 8,
        cleanup: false,
        max_ratio: 0.36,
        measured: 0.321,
        why: "one opaque colour: the reduce cascade collapses it to depth-1 indexed, and chunk \
              framing is most of what remains. 10% headroom because at ~100 bytes total a single \
              byte moves the ratio by about a percent.",
    },
    Budget {
        name: "sprite_rgba8",
        fixture: "sprite_rgba8",
        side: 128,
        depth: 8,
        cleanup: false,
        max_ratio: 0.99,
        measured: 0.963,
        why: "binary alpha over invisible colour noise. The reduce cascade now reaches this \
              case -- `write_reduced_or_native` races an `RGB`+`tRNS` colour key against the \
              unreduced encoding and keeps whichever is smaller -- so the budget is a real one \
              rather than the placeholder 1.00 it carried while those axes were missing. \
              Tightening it was #481's stated acceptance test. 2% headroom, not 5%: at 0.963 the \
              usual 5% rounds past 1.00, which would give up the very claim this row exists to \
              make.",
    },
    Budget {
        name: "sprite_rgba8 +clean",
        fixture: "sprite_rgba8",
        side: 128,
        depth: 8,
        cleanup: true,
        max_ratio: 0.70,
        measured: 0.665,
        why: "the same pixels with `with_transparent_cleanup`, which collapses every invisible \
              pixel to one colour and so makes the palette reachable. This is the row that gates \
              the `+clean` column STATUS.md publishes; without it the headline cleanup result was \
              measured by a bench and asserted by nothing.",
    },
    Budget {
        name: "palette64_rgba8",
        fixture: "palette64_rgba8",
        side: 128,
        depth: 8,
        cleanup: false,
        max_ratio: 0.95,
        measured: 0.899,
        why: "64 colours over two alpha levels. The palette encoding wins outright at 256x256 \
              but loses at this size, because PLTE + tRNS is a flat 224 incompressible bytes \
              against pixels that compress ~160x; `write_reduced_or_native` encodes both and \
              keeps the smaller, so the row measures whichever is actually better here -- at \
              128x128 that is the unreduced encoding, which carries no PLTE at all. The race is \
              what makes the outcome stable enough to budget below 1.00.",
    },
    Budget {
        name: "palette64_rgba8 +clean",
        fixture: "palette64_rgba8",
        side: 128,
        depth: 8,
        cleanup: true,
        max_ratio: 0.95,
        measured: 0.899,
        why: "the row where cleaning does not pay, and therefore is not done. Collapsing the \
              transparent entries shortens PLTE and tRNS, but it also rewrites pixels that were \
              compressing well, and at 128x128 the second effect wins: cleaning measured 403 \
              bytes against the uncleaned 364. `cleaned_or_plain` races the two and keeps the \
              smaller, so this row now measures exactly what `palette64_rgba8` does, and the \
              budget is the same. That equality is the assertion -- it is what \
              `with_transparent_cleanup` never costing bytes looks like from here, and it is \
              pinned as a law for every row by `cleanup_never_costs_bytes_on_any_corpus_row`.",
    },
    Budget {
        name: "opaque256_rgba8",
        fixture: "opaque256_rgba8",
        side: 128,
        depth: 8,
        cleanup: false,
        max_ratio: 0.78,
        measured: 0.741,
        why: "256 opaque colours, so a palette and an alpha drop both apply. The palette wins the \
              raw estimate (16 384 + 792 against 49 152) and loses the finished file to PLTE's \
              768 incompressible bytes, which is exactly the case no other row had: this one is \
              the gate on `write_reduced_or_native` racing the chunk-free runner-up the estimate \
              eliminated rather than falling back to the unreduced image. It emitted 349 bytes \
              with the alpha channel intact before that race existed, against 317 now.",
    },
    Budget {
        name: "demotable_rgb16",
        fixture: "demotable_rgb16",
        side: 64,
        depth: 16,
        cleanup: false,
        max_ratio: 0.68,
        measured: 0.644,
        why: "the 16-bit twin of the row above, and the corpus's only 16-bit entry. Every sample \
              is `k*257`, so the demotion to 8 bits is lossless and halves the payload before \
              anything else runs -- but a palette also applies to the demoted image and wins the \
              raw estimate, and the demotion used to be discarded with it: 220 bytes at depth 16 \
              before, 172 at depth 8 now. Measured at 64x64, a quarter of the other rows' pixel \
              count, because a 16-bit source carries twice the samples through three candidate \
              encodings and this file runs in the coverage and mutation lanes.",
    },
    Budget {
        name: "tiny_rgb8",
        fixture: "tiny_rgb8",
        side: 16,
        depth: 8,
        cleanup: false,
        max_ratio: 0.95,
        measured: 0.862,
        why: "the regime where the signature and five chunks of framing dominate, and the only \
              row where `overhead_bytes` is legible. Reported by the bench and, until now, gated \
              by nothing. Same 10% headroom as `flat_rgba8`, for the same reason.",
    },
];

/// Half the bench's side, so this file stays fast enough for the coverage and mutation lanes.
const SIDE: u32 = 128;

/// The pixels for a budget row, and how many channels they carry.
fn pixels(fixture: &str, side: u32) -> (Vec<u8>, usize) {
    match fixture {
        "gradient_rgb8" => (common::corpus::gradient_rgb(side), 3),
        "photo_rgb8" => (common::corpus::photo_rgb(side), 3),
        "noise_rgb8" => (common::corpus::noise_rgb(side), 3),
        "grey_as_rgb8" => (common::corpus::grey_as_rgb(side), 3),
        "palette64_rgba8" => (common::corpus::palette64_rgba(side), 4),
        "sprite_rgba8" => (common::corpus::sprite_rgba(side), 4),
        "flat_rgba8" => (common::corpus::flat_rgba(side), 4),
        // Opaque RGBA with a palette *and* an alpha drop available: the row that measures which
        // of the two the encoder actually emits.
        "opaque256_rgba8" => (common::corpus::opaque256_rgba(side), 4),
        // The same colours at 16 bits, every sample `k*257`: the demotion under a palette.
        "demotable_rgb16" => (common::corpus::demotable_rgb16(side), 3),
        // The bench's 16x16 row: the regime where chunk framing dominates bits-per-pixel.
        "tiny_rgb8" => (common::corpus::gradient_rgb(side), 3),
        other => panic!("unknown corpus fixture {other}"),
    }
}

/// Encodes at the crate's smallest-output settings.
///
/// `BruteForce`'s candidate set is integer-only -- `MinEntropy` is deliberately not in it -- so no
/// `f64::log2` enters the gated path and these ratios are machine-independent as well as stable
/// run to run.
fn gamut_best(samples: &[u8], channels: usize, depth: u8, side: u32, cleanup: bool) -> Vec<u8> {
    let encoder = PngEncoder::new()
        .with_compression(Level::Best)
        .with_filter(FilterStrategy::BruteForce)
        .with_auto_reduce(true)
        .with_transparent_cleanup(cleanup);
    let dims = Dimensions::new(side, side).expect("valid dimensions");
    let mut out = Vec::new();
    if depth == 16 {
        // The corpus stores 16-bit rows the way the file does -- big-endian pairs -- so libpng
        // takes them as they are and only gamut's `ImageRef` needs the samples widened back.
        let wide: Vec<u16> = samples
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&p| u16::from_be_bytes(p))
            .collect();
        let image = ImageRef::<Rgb16>::new(&wide, dims).expect("buffer matches dimensions");
        encoder.encode_image(image, &mut out).expect("encode");
    } else if channels == 3 {
        let image = ImageRef::<Rgb8>::new(samples, dims).expect("buffer matches dimensions");
        encoder.encode_image(image, &mut out).expect("encode");
    } else {
        let image = ImageRef::<Rgba8>::new(samples, dims).expect("buffer matches dimensions");
        encoder.encode_image(image, &mut out).expect("encode");
    }
    out
}

/// The same source layout through libpng at zlib level 9 — no palette hint, default adaptive
/// filtering. Handing libpng a palette would hand it gamut's own reduction.
fn libpng9(samples: &[u8], channels: usize, depth: u8, side: u32) -> Vec<u8> {
    let color_type = if channels == 3 {
        libpng_oracle::COLOR_RGB
    } else {
        libpng_oracle::COLOR_RGBA
    };
    libpng_oracle::encode(
        samples,
        side,
        side,
        color_type,
        depth,
        &libpng_oracle::EncodeOpts {
            compression_level: Some(9),
            ..libpng_oracle::EncodeOpts::default()
        },
    )
}

#[test]
fn gamut_never_exceeds_its_size_budget_against_libpng9() {
    for budget in BUDGETS {
        let (samples, channels) = pixels(budget.fixture, budget.side);
        let ours = gamut_best(
            &samples,
            channels,
            budget.depth,
            budget.side,
            budget.cleanup,
        );
        let theirs = libpng9(&samples, channels, budget.depth, budget.side);
        let ratio = ours.len() as f64 / theirs.len() as f64;
        // Printed, not just asserted: the `measured` column is only honest if refreshing it is a
        // paste rather than a re-derivation. `cargo test` captures this on success.
        println!(
            "{:<22} {:>7} / {:>7} = {ratio:.3}   (budget {:.2}, recorded {:.3})",
            budget.name,
            ours.len(),
            theirs.len(),
            budget.max_ratio,
            budget.measured,
        );
        assert!(
            ratio <= budget.max_ratio,
            "{}: {} bytes vs libpng-9's {} = {ratio:.3}, budget {:.2} (measured {:.2} when set)\n  {}",
            budget.name,
            ours.len(),
            theirs.len(),
            budget.max_ratio,
            budget.measured,
            budget.why,
        );
    }
}

#[test]
fn gamut_beats_libpng9_where_it_claims_to() {
    // "We win here" and "we do not lose too much there" are different claims, so they are
    // different tests. The winning set is listed explicitly rather than derived from
    // `max_ratio < 1.0`: a budget loosened past 1.0 during a regression would otherwise drop out
    // of this test silently, which is exactly when it should fail.
    const WINS: &[&str] = &[
        "gradient_rgb8",
        "photo_rgb8",
        "grey_as_rgb8",
        "flat_rgba8",
        "sprite_rgba8",
        "palette64_rgba8",
        "opaque256_rgba8",
        "demotable_rgb16",
    ];
    for budget in BUDGETS.iter().filter(|b| WINS.contains(&b.name)) {
        let (samples, channels) = pixels(budget.fixture, budget.side);
        let ours = gamut_best(
            &samples,
            channels,
            budget.depth,
            budget.side,
            budget.cleanup,
        );
        let theirs = libpng9(&samples, channels, budget.depth, budget.side);
        assert!(
            ours.len() < theirs.len(),
            "{}: claims a structural win but measured {} vs {}",
            budget.name,
            ours.len(),
            theirs.len(),
        );
    }
}

#[test]
fn the_codestream_is_no_larger_where_both_encoders_choose_the_same_representation() {
    // The reason `deconstruct` is a dependency of this file: it reads the IDAT total out of both
    // encoders' output, so the comparison is over codestreams rather than whole files, with
    // framing and chunk differences excluded.
    //
    // This is deliberately *not* an attribution to DEFLATE. Landing on the same colour type and
    // depth makes `filtered_len` identical -- it is a function of IHDR alone -- but not the
    // filtered *bytes*: gamut runs `BruteForce` (MinBigrams wins `gradient_rgb8`) while libpng
    // runs its own adaptive heuristic, so the two compress different inputs. What is asserted is
    // the combined result of filtering and DEFLATE, which is what the size claim rests on anyway;
    // isolating the DEFLATE stage would mean re-filtering libpng's pixels with gamut's own
    // choices first. Only the rows where no reduction applies can be compared at all.
    for name in ["gradient_rgb8", "photo_rgb8"] {
        let (samples, channels) = pixels(name, SIDE);
        let ours = gamut_best(&samples, channels, 8, SIDE, false);
        let theirs = libpng9(&samples, channels, 8, SIDE);
        let (a, b) = (
            deconstruct(&ours).expect("gamut output deconstructs"),
            deconstruct(&theirs).expect("libpng output deconstructs"),
        );

        assert_eq!(
            (a.header.color_type, a.header.bit_depth),
            (b.header.color_type, b.header.bit_depth),
            "{name}: attribution only holds when both land on the same representation",
        );
        assert_eq!(
            a.filtered_len, b.filtered_len,
            "{name}: same representation means an identical filtered stream length",
        );
        assert!(
            a.idat_compressed <= b.idat_compressed,
            "{name}: gamut's codestream is {} bytes against libpng-9's {}",
            a.idat_compressed,
            b.idat_compressed,
        );
    }
}

#[test]
fn encoded_size_is_deterministic() {
    // Without this the budget table is measuring noise rather than the encoder.
    for budget in BUDGETS {
        let (samples, channels) = pixels(budget.fixture, budget.side);
        let first = gamut_best(
            &samples,
            channels,
            budget.depth,
            budget.side,
            budget.cleanup,
        );
        let second = gamut_best(
            &samples,
            channels,
            budget.depth,
            budget.side,
            budget.cleanup,
        );
        assert_eq!(first, second, "{}: encode is not reproducible", budget.name);
    }
}

#[test]
fn cleanup_never_costs_bytes_on_any_corpus_row() {
    // The gate on `with_transparent_cleanup`'s central claim. It is only true because the encoder
    // *races* the cleaned and uncleaned encodings and keeps the smaller: cleaning is a transform,
    // not a reduction, and on a fixture whose invisible pixels carry structure rather than noise
    // it destroys compressible bytes. Measured before the race, on `palette64_rgba8`, cleaning was
    // worth -2.3% at 32x32, +10.7% at 128x128 and -5.2% at 256x256 -- with both candidates landing
    // on the same colour type, so the sign was a property of the image, not of the reduction.
    //
    // A law rather than a budget, so it covers every row and every side, and needs no constant.
    for budget in BUDGETS.iter().filter(|b| !b.cleanup) {
        let (samples, channels) = pixels(budget.fixture, budget.side);
        let plain = gamut_best(&samples, channels, budget.depth, budget.side, false);
        let cleaned = gamut_best(&samples, channels, budget.depth, budget.side, true);
        assert!(
            cleaned.len() <= plain.len(),
            "{}: cleanup cost {} bytes ({} -> {}); the race in `cleaned_or_plain` should have \
             kept the uncleaned encoding",
            budget.name,
            cleaned.len() - plain.len(),
            plain.len(),
            cleaned.len(),
        );
    }
}
