//! The effort ladder's contract (issue #484), across every rung [`Preset::from_level`] admits.
//!
//! A rung names a *combination* of the five independent size/time knobs, so what it owes a caller
//! is not any particular knob value — those are re-tunable as this crate's corpus grows — but
//! three promises about the ladder as a whole, in decreasing order of importance:
//!
//! 1. **Correctness is rung-independent.** Every rung's file resolves, through libpng, to exactly
//!    the pixels handed in. A rung chooses how hard the encoder searches, never what it stores.
//!    The oracle rather than a round trip on purpose: gamut writes both halves of a PNG, so a
//!    defect symmetric across its own encoder and decoder survives any round trip and only libpng
//!    sees it.
//! 2. **Every rung buys something.** Each rung's total over the corpus is *strictly* smaller than
//!    the rung above it. A rung that bought nothing would not be a rung.
//! 3. **[`Preset::Balanced`] changes nothing.** It encodes byte-identically to a default
//!    [`PngEncoder`], which is what makes the dial additive: adopting it cannot move a caller who
//!    was not already asking for something else.
//!
//! # Why the ordering is asserted over the corpus and not per row
//!
//! Because per row it is **false**, and measurably so. `Preset::Fast` fixes the Paeth predictor
//! where `Balanced` runs the per-row `MinSumAbs` search, and a fixed predictor beats a heuristic
//! on a picture that suits it: on `demotable_rgb16` at 32x32, `Fast` emits **155** bytes against
//! `Balanced`'s **161**. That is not a defect — a cheaper rung coming out smaller costs the caller
//! nothing — but it means "no rung is larger than the rung above it" is not a promise this crate
//! can keep, and pinning it per row would pin an accident of the corpus. The aggregate ordering is
//! what the ladder actually guarantees, and it is what is gated here.
//!
//! Measured at this revision (`cargo test -p gamut-png --test effort -- --nocapture`), totals in
//! bytes over the nine rows below: **Fast 17 530, Balanced 16 952, Small 16 497, Smallest 15 903**.
//!
//! Fixtures are the shared efficiency corpus at a small side, because `Preset::Smallest` is the
//! slowest code in the crate — a full DEFLATE per brute-force candidate — and this suite runs
//! inside the coverage and mutation lanes.

mod common;

use gamut_core::{Dimensions, EncodeImage, ImageRef, Rgb8, Rgb16, Rgba8};
use gamut_png::{PngEncoder, Preset};

/// Every rung, fastest first — derived from [`Preset::from_level`] rather than listed, so a rung
/// appended to the ladder is measured here without this file being touched. That the
/// discriminants are contiguous from zero is pinned inline in `src/encoder.rs`
/// (`every_preset_level_round_trips_and_the_ladder_is_contiguous`), which is what makes walking
/// until the first `None` the whole ladder rather than a prefix of it.
fn ladder() -> Vec<Preset> {
    (0..=u8::MAX).map_while(Preset::from_level).collect()
}

/// One corpus row: the fixture's samples in the layout it is generated in.
struct Row {
    name: &'static str,
    samples: Vec<u8>,
    channels: usize,
    depth: u8,
    side: u32,
}

/// The shared efficiency corpus (`tests/common/corpus.rs`), one row per axis of encoder behaviour.
///
/// 64x64 for the 8-bit rows and 32x32 for the 16-bit one, a quarter of what `size_contract.rs`
/// measures at: these assertions are about the *order* of the rungs, not about any absolute ratio
/// against libpng, so they do not need the pixels the budget table needs.
fn corpus() -> Vec<Row> {
    const SIDE: u32 = 64;
    let rgb = |name, samples| Row {
        name,
        samples,
        channels: 3,
        depth: 8,
        side: SIDE,
    };
    let rgba = |name, samples| Row {
        name,
        samples,
        channels: 4,
        depth: 8,
        side: SIDE,
    };
    vec![
        rgb("gradient_rgb8", common::corpus::gradient_rgb(SIDE)),
        rgb("photo_rgb8", common::corpus::photo_rgb(SIDE)),
        rgb("noise_rgb8", common::corpus::noise_rgb(SIDE)),
        rgb("grey_as_rgb8", common::corpus::grey_as_rgb(SIDE)),
        rgba("palette64_rgba8", common::corpus::palette64_rgba(SIDE)),
        rgba("sprite_rgba8", common::corpus::sprite_rgba(SIDE)),
        rgba("flat_rgba8", common::corpus::flat_rgba(SIDE)),
        rgba("opaque256_rgba8", common::corpus::opaque256_rgba(SIDE)),
        Row {
            name: "demotable_rgb16",
            samples: common::corpus::demotable_rgb16(32),
            channels: 3,
            depth: 16,
            side: 32,
        },
    ]
}

/// Encodes one corpus row through `encoder`, in the layout the fixture is generated in.
fn encode(encoder: &PngEncoder, row: &Row) -> Vec<u8> {
    let dims = Dimensions::new(row.side, row.side).expect("valid dimensions");
    let mut out = Vec::new();
    if row.depth == 16 {
        // The corpus stores 16-bit rows the way the file does — big-endian pairs — so only
        // `ImageRef` needs the samples widened back.
        let wide: Vec<u16> = row
            .samples
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&p| u16::from_be_bytes(p))
            .collect();
        let image = ImageRef::<Rgb16>::new(&wide, dims).expect("buffer matches dimensions");
        encoder.encode_image(image, &mut out).expect("encode");
    } else if row.channels == 3 {
        let image = ImageRef::<Rgb8>::new(&row.samples, dims).expect("buffer matches dimensions");
        encoder.encode_image(image, &mut out).expect("encode");
    } else {
        let image = ImageRef::<Rgba8>::new(&row.samples, dims).expect("buffer matches dimensions");
        encoder.encode_image(image, &mut out).expect("encode");
    }
    out
}

/// An 8-bit row's pixels in the canonical RGBA `libpng_oracle::decode_rgba8` resolves any file to,
/// whatever colour type the rung's reduction chose to store them as.
fn expected_rgba8(row: &Row) -> Vec<u8> {
    if row.channels == 3 {
        row.samples
            .as_chunks::<3>()
            .0
            .iter()
            .flat_map(|p| [p[0], p[1], p[2], 0xff])
            .collect()
    } else {
        row.samples.clone()
    }
}

#[test]
fn every_rung_stores_the_pixels_it_was_handed() {
    // The 8-bit rows only, and that bound is libpng's, not this crate's: `decode_rgba8` drives
    // libpng's *simplified* API, which treats a 16-bit file as linear and converts it to sRGB on
    // the way to 8-bit output, while an 8-bit file is passed through. A stored sample of 40 comes
    // back as 40 from an 8-bit file and as 110 from a 16-bit one. So it is not a depth-neutral
    // resolver, and the 16-bit fixture — whose lower rungs store 16 bits and whose upper rungs
    // losslessly demote to 8 — cannot be compared through it without the comparison turning into
    // an assertion about libpng's colour conversion. That fixture is measured by the size test
    // below; `tests/oracle.rs` is where 16-bit fidelity is pinned, at its own stored depth.
    for row in corpus().into_iter().filter(|row| row.depth == 8) {
        let expected = expected_rgba8(&row);
        for preset in ladder() {
            let png = encode(&PngEncoder::new().with_preset(preset), &row);
            let (w, h, rgba) = libpng_oracle::decode_rgba8(&png);
            assert_eq!(
                (w, h),
                (row.side, row.side),
                "{}/{preset:?}: size",
                row.name
            );
            assert!(
                rgba == expected,
                "{}/{preset:?}: a rung changed what the file stores",
                row.name
            );
        }
    }
}

#[test]
fn every_rung_is_strictly_smaller_over_the_corpus_than_the_one_above_it() {
    let rungs = ladder();
    let totals: Vec<usize> = rungs
        .iter()
        .map(|&preset| {
            let encoder = PngEncoder::new().with_preset(preset);
            corpus().iter().map(|row| encode(&encoder, row).len()).sum()
        })
        .collect();
    for (pair, names) in totals.windows(2).zip(rungs.windows(2)) {
        assert!(
            pair[0] > pair[1],
            "{:?} ({} bytes over the corpus) does not buy anything over {:?} ({} bytes)",
            names[1],
            pair[1],
            names[0],
            pair[0],
        );
    }
}

#[test]
fn the_balanced_rung_is_a_default_encoder() {
    // Not a tautology: `Preset::Balanced` spells its knob values out rather than reading them back
    // from `PngEncoder::new`, so this is the assertion that keeps the two from drifting — and it
    // is what makes the whole dial additive.
    for row in corpus() {
        let untouched = encode(&PngEncoder::new(), &row);
        let balanced = encode(&PngEncoder::new().with_preset(Preset::Balanced), &row);
        assert!(
            untouched == balanced,
            "{}: the balanced rung is not the encoder default ({} bytes against {})",
            row.name,
            balanced.len(),
            untouched.len()
        );
    }
}
