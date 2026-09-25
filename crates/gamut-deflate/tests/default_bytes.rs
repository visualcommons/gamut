//! integration · null-change invariance — the encoder's output for a fixed corpus must be
//! **unchanged**, byte for byte, at every level.
//!
//! Correctness belongs to `tests/oracle.rs`, which checks that zlib inflates what this crate
//! deflates, and the ratio contract there (`best_beats_zlib_9`) checks that the output is *small*.
//! Neither can see the failure this file exists for: the optimiser quietly getting **worse**.
//!
//! That gap is measurable. A mutation survey of `gamut-deflate` left 59 mutants alive, and roughly
//! three quarters of them sit in the compression-quality machinery — `recurse`, `block_bits`,
//! `costs` and `build` in the length-limited Huffman optimiser, and `Matcher::hash`/`find`/`insert`
//! in the LZ77 match finder. Every one of them still emits a **valid** stream that zlib inflates
//! correctly, so the oracle passes; and the ratio contract has slack in it — output that grew by a
//! few percent still beats zlib-9 — so that passes too. The encoding simply gets bigger, silently,
//! which for a crate whose stated purpose is space efficiency is the regression that matters most.
//!
//! Pinning the exact bytes closes it: any change to a heuristic changes the output and fails here.
//!
//! **Re-pinning is expected, and every re-pin is a commit that says why.** A genuine improvement to
//! the parser, the cost model or the block splitter *should* move these numbers, and the diff
//! showing them move is the evidence that it did something. What must not happen is a number moving
//! without anyone noticing. This is the same contract, and the same bargain, as
//! `gamut-webp/tests/default_bytes.rs`.

use gamut_deflate::{DeflateEncoder, Level};

/// FNV-1a (64-bit) over `bytes` — a dependency-free digest for pinning fixture bytes.
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// English-like text: many short, frequently repeated substrings, so the match finder and the
/// literal/length code lengths both matter.
fn prose() -> Vec<u8> {
    let sentence =
        b"the quick brown fox jumps over the lazy dog while the quick brown cat sleeps. ";
    sentence.iter().copied().cycle().take(4096).collect()
}

/// Long-range repetition with a period the hash chain must actually find: the input that separates
/// a working match finder from one that has degenerated to literals.
fn periodic() -> Vec<u8> {
    let mut out = Vec::with_capacity(4096);
    let mut seed = 0x1234_5678_u32;
    let block: Vec<u8> = (0..512)
        .map(|_| {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 24) as u8
        })
        .collect();
    while out.len() < 4096 {
        out.extend_from_slice(&block);
    }
    out.truncate(4096);
    out
}

/// A skewed alphabet: compressible almost entirely through Huffman code lengths rather than
/// matches, so it isolates the code-length optimiser from the parser.
fn skewed() -> Vec<u8> {
    let mut seed = 0x9E37_79B9_u32;
    (0..4096)
        .map(|_| {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            // ~70% 'a', the rest spread over a small alphabet.
            match seed >> 28 {
                0..=10 => b'a',
                11 => b'b',
                12 => b'c',
                13 => b'd',
                14 => b'e',
                _ => b'f',
            }
        })
        .collect()
}

/// Incompressible: the case where every level should fall back to something near the stored floor,
/// and where a broken cost model shows up as expansion.
fn incompressible() -> Vec<u8> {
    let mut seed = 0xDEAD_BEEF_u32;
    (0..4096)
        .map(|_| {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 24) as u8
        })
        .collect()
}

/// Eight statistically distinct, **non-repeating** regions: the input that forces the block
/// splitter to make decisions.
///
/// The uniform fixtures above cannot reach `dynamic.rs`'s `recurse` in any interesting way. One
/// code table fits the whole input, so there is nothing to split. Nor can a fixture built by
/// repeating a block: `recurse` returns immediately below `2 * MIN_SPLIT_TOKENS` tokens, and once
/// a region repeats, the parser emits one long match for the whole repeat and the token count
/// collapses far below that floor.
///
/// So every region here is generated fresh from its own seed. That keeps the token count high
/// enough for the splitter to run, while the alternating character means where the cuts fall is a
/// real choice rather than a formality -- and a cost model that stops making it well produces
/// measurably different bytes.
fn mixed() -> Vec<u8> {
    let mut out = Vec::with_capacity(32 * 1024);

    for region in 0..8u32 {
        let mut seed = 0x5EED_0000_u32.wrapping_add(region.wrapping_mul(0x9E37_79B9));
        let mut next = || {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            seed >> 24
        };
        // Alternate between a small skewed alphabet and full-range noise, so consecutive regions
        // want visibly different code tables.
        let skewed_region = region % 2 == 0;
        for _ in 0..4096 {
            let byte = if skewed_region {
                match next() >> 4 {
                    0..=10 => b'a',
                    11 => b'b',
                    12 => b'c',
                    13 => b'd',
                    14 => b'e',
                    _ => b'f',
                }
            } else {
                next() as u8
            };
            out.push(byte);
        }
    }
    out
}

/// One pinned encoding: the fixture, the level, and the exact length and digest it must produce.
struct Pin {
    fixture: &'static str,
    level: Level,
    len: usize,
    digest: u64,
}

const PINS: &[Pin] = &[
    Pin {
        fixture: "prose",
        level: Level::Fast,
        len: 91,
        digest: 0x194f_084b_e161_996e,
    },
    Pin {
        fixture: "prose",
        level: Level::Default,
        len: 87,
        digest: 0x54e1_7e5b_2ca0_0145,
    },
    Pin {
        fixture: "prose",
        level: Level::Best,
        len: 87,
        digest: 0xb6de_f6c5_8b48_8385,
    },
    // Fast and Default coincide here: lazy matching finds nothing greedy matching missed on a
    // strictly periodic input, so the two rungs emit the same bytes. Best differs -- the optimal
    // parse reaches the same length by a different split.
    Pin {
        fixture: "periodic",
        level: Level::Fast,
        len: 574,
        digest: 0x8136_e485_7452_3299,
    },
    Pin {
        fixture: "periodic",
        level: Level::Default,
        len: 574,
        digest: 0x8136_e485_7452_3299,
    },
    Pin {
        fixture: "periodic",
        level: Level::Best,
        len: 574,
        digest: 0xb60f_deee_b11e_7422,
    },
    // The clearest ladder in the corpus, and the one that isolates the code-length optimiser:
    // 1262 -> 1147 -> 1048 on an input compressible almost entirely through Huffman lengths.
    Pin {
        fixture: "skewed",
        level: Level::Fast,
        len: 1262,
        digest: 0xc4e3_238f_b25a_7f95,
    },
    Pin {
        fixture: "skewed",
        level: Level::Default,
        len: 1147,
        digest: 0x2cd5_6346_8c70_7bdf,
    },
    Pin {
        fixture: "skewed",
        level: Level::Best,
        len: 1048,
        digest: 0xd97b_facb_22a9_10d5,
    },
    // Byte-identical at all three levels: every rung recognises the input as incompressible and
    // falls back to the stored floor, 4101 bytes for 4096 of payload. A cost model that stopped
    // recognising it would expand the output here rather than merely fail to shrink it.
    Pin {
        fixture: "incompressible",
        level: Level::Fast,
        len: 4101,
        digest: 0xae1a_1a70_a598_b434,
    },
    Pin {
        fixture: "incompressible",
        level: Level::Default,
        len: 4101,
        digest: 0xae1a_1a70_a598_b434,
    },
    Pin {
        fixture: "incompressible",
        level: Level::Best,
        len: 4101,
        digest: 0xae1a_1a70_a598_b434,
    },
    // The block-splitting corpus: four statistically distinct regions, so where the cuts fall
    // is a genuine decision rather than a formality.
    Pin {
        fixture: "mixed",
        level: Level::Fast,
        len: 22925,
        digest: 0xf0bb_5588_9ec5_87e9,
    },
    Pin {
        fixture: "mixed",
        level: Level::Default,
        len: 22444,
        digest: 0x63b4_b530_7361_d7eb,
    },
    Pin {
        fixture: "mixed",
        level: Level::Best,
        len: 21601,
        digest: 0x6b73_d9e2_9361_4f30,
    },
];

fn fixture(name: &str) -> Vec<u8> {
    match name {
        "prose" => prose(),
        "periodic" => periodic(),
        "skewed" => skewed(),
        "incompressible" => incompressible(),
        "mixed" => mixed(),
        other => panic!("unknown fixture {other}"),
    }
}

#[test]
fn every_level_still_produces_its_pinned_bytes() {
    let mut mismatches = Vec::new();

    for pin in PINS {
        let data = fixture(pin.fixture);
        let mut out = Vec::new();
        DeflateEncoder::new()
            .with_level(pin.level)
            .compress(&data, &mut out);

        let digest = fnv1a64(&out);
        if out.len() != pin.len || digest != pin.digest {
            mismatches.push(format!(
                "        Pin {{ fixture: {:?}, level: Level::{:?}, len: {}, digest: {:#018x} }},",
                pin.fixture,
                pin.level,
                out.len(),
                digest
            ));
        }
    }

    assert!(
        mismatches.is_empty(),
        "the default output moved. If that was deliberate, re-pin with these and say why in the \
         commit message:\n{}",
        mismatches.join("\n")
    );
}
