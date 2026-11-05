//! Differential cross-checks for gamut-deflate against the reference C **zlib**.
//!
//! gamut ships no inflater, so correctness is proven by inflating the encoder's output with zlib and
//! asserting it reproduces the original bytes. Every lossless path must round-trip exactly, for both
//! raw DEFLATE and the zlib wrapper, across edge cases and varied byte statistics.

use gamut_deflate::{DeflateEncoder, Level};

const LEVELS: &[Level] = &[Level::Store, Level::Fast, Level::Default, Level::Best];

/// Deterministic inputs covering edge cases and a spread of byte statistics.
fn corpus() -> Vec<Vec<u8>> {
    vec![
        Vec::new(),                              // empty
        vec![0x00],                              // single byte
        vec![0xAB; 1000],                        // long run
        (0..4096u32).map(|i| i as u8).collect(), // repeating ramp
        (0..5000u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8)
            .collect(), // pseudo-random
        b"the quick brown fox jumps over the lazy dog. ".repeat(50), // english text
        vec![0u8; 70_000],                       // > 64 KiB: multi-block
    ]
}

#[test]
fn raw_deflate_round_trips_via_zlib() {
    for data in corpus() {
        for &level in LEVELS {
            let mut out = Vec::new();
            DeflateEncoder::new()
                .with_level(level)
                .compress(&data, &mut out);
            let back = zlib_oracle::inflate_raw(&out).unwrap_or_else(|e| {
                panic!(
                    "inflate_raw failed (level {level:?}, {} bytes): {e}",
                    data.len()
                )
            });
            assert_eq!(
                back,
                data,
                "raw round-trip mismatch at level {level:?}, {} bytes",
                data.len()
            );
        }
    }
}

#[test]
fn back_references_compress_and_round_trip() {
    // Highly repetitive input must shrink dramatically once LZ77 matches are emitted — and still
    // inflate back to the original through the reference decoder.
    let data = b"the quick brown fox jumps over the lazy dog. ".repeat(200);
    for &level in &[Level::Fast, Level::Default, Level::Best] {
        let mut out = Vec::new();
        DeflateEncoder::new()
            .with_level(level)
            .zlib_compress(&data, &mut out);
        assert_eq!(zlib_oracle::inflate_zlib(&out).unwrap(), data);
        assert!(
            out.len() < data.len() / 4,
            "level {level:?}: {} should be far smaller than {}",
            out.len(),
            data.len()
        );
    }
}

#[test]
fn zlib_stream_round_trips_via_zlib() {
    for data in corpus() {
        for &level in LEVELS {
            let mut out = Vec::new();
            DeflateEncoder::new()
                .with_level(level)
                .zlib_compress(&data, &mut out);
            let back = zlib_oracle::inflate_zlib(&out)
                .unwrap_or_else(|e| panic!("inflate_zlib failed (level {level:?}): {e}"));
            assert_eq!(
                back,
                data,
                "zlib round-trip mismatch at level {level:?}, {} bytes",
                data.len()
            );
        }
    }
}

#[test]
fn best_beats_zlib_9() {
    // The crate's reason to exist, enforced as a contract: `Level::Best` must produce a stream *no
    // larger* than zlib at its maximum level (not merely "within a few percent"), and more effort
    // must never lose to less — all while still inflating exactly. Regressing the ratio fails the
    // build. `Default` trades a little ratio for speed, so it is only held within 2% of zlib-9.
    let inputs: Vec<(&str, Vec<u8>)> = vec![
        (
            "text",
            b"the quick brown fox jumps over the lazy dog. ".repeat(300),
        ),
        ("ramp", (0..20_000u32).map(|i| (i % 256) as u8).collect()),
        (
            "pseudo",
            (0..20_000u32)
                .map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8)
                .collect(),
        ),
        ("mixed", {
            let mut v = b"header header header ".repeat(50);
            v.extend((0..8_000u32).map(|i| (i.wrapping_mul(48_271) >> 20) as u8));
            v
        }),
    ];
    for (name, data) in &inputs {
        let z9 = zlib_oracle::compress(data, 9).unwrap();

        let mut default_out = Vec::new();
        DeflateEncoder::new()
            .with_level(Level::Default)
            .zlib_compress(data, &mut default_out);
        let mut best_out = Vec::new();
        DeflateEncoder::new()
            .with_level(Level::Best)
            .zlib_compress(data, &mut best_out);

        // Both still round-trip through the reference inflater.
        assert_eq!(
            zlib_oracle::inflate_zlib(&default_out).unwrap(),
            *data,
            "{name} Default: round-trip"
        );
        assert_eq!(
            zlib_oracle::inflate_zlib(&best_out).unwrap(),
            *data,
            "{name} Best: round-trip"
        );

        // The ratio contract.
        assert!(
            best_out.len() <= z9.len(),
            "{name}: Best {} must be <= zlib-9 {}",
            best_out.len(),
            z9.len()
        );
        assert!(
            best_out.len() <= default_out.len(),
            "{name}: Best {} must be <= Default {}",
            best_out.len(),
            default_out.len()
        );
        assert!(
            default_out.len() <= z9.len() + z9.len() / 50,
            "{name}: Default {} should stay within 2% of zlib-9 {}",
            default_out.len(),
            z9.len()
        );
    }
}

/// The effort knob (issue #337): every budget must still round-trip exactly through the reference
/// inflater, from the zero-pass lazy seed up to a zopfli-class budget.
#[test]
fn effort_budgets_round_trip() {
    for data in corpus() {
        for &effort in &[0u8, 1, 6, 15] {
            let mut out = Vec::new();
            DeflateEncoder::new()
                .with_level(Level::Best)
                .with_deflate_passes(effort)
                .zlib_compress(&data, &mut out);
            assert_eq!(
                zlib_oracle::inflate_zlib(&out).unwrap(),
                data,
                "effort {effort}, {} bytes",
                data.len()
            );
        }
    }
}

/// The effort knob must actually steer the optimal parse: on an input where the refined cost model
/// beats the lazy seed, zero passes and the default budget produce different streams, more effort
/// is no larger, and the builder default is exactly `DEFAULT_DEFLATE_PASSES` passes.
#[test]
fn effort_is_live_and_defaults_to_six() {
    // Mixed structured + noisy bytes: verified to make the cost-model refinement change the parse.
    let mut data = b"header header header ".repeat(50);
    data.extend((0..8_000u32).map(|i| (i.wrapping_mul(48_271) >> 20) as u8));

    let at = |effort: u8| {
        let mut out = Vec::new();
        DeflateEncoder::new()
            .with_level(Level::Best)
            .with_deflate_passes(effort)
            .zlib_compress(&data, &mut out);
        out
    };
    let lazy_seed = at(0);
    let six = at(6); // literal 6: pins DEFAULT_DEFLATE_PASSES's value, not just its plumbing
    assert_ne!(lazy_seed, six, "effort must change the emitted stream");
    assert_eq!(DeflateEncoder::DEFAULT_DEFLATE_PASSES, 6);

    let mut default_out = Vec::new();
    DeflateEncoder::new()
        .with_level(Level::Best)
        .zlib_compress(&data, &mut default_out);
    assert_eq!(default_out, six, "default effort must be 6 passes");

    assert!(
        at(15).len() <= lazy_seed.len(),
        "a zopfli-class budget must not lose to the lazy seed"
    );
}

/// Deterministic `xorshift64*` PRNG — keeps the fuzz-style sweep reproducible without a dev-dep.
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform value in `0..n` (returns 0 when `n == 0`).
    fn range(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next_u64() % n as u64) as usize
        }
    }
}

/// Builds an input of `size` bytes with a randomly chosen alphabet, injecting runs so the LZ77
/// matcher and back-reference paths are exercised alongside literals.
fn generate(rng: &mut Rng, size: usize) -> Vec<u8> {
    let alphabet = [1usize, 2, 4, 16, 64, 256][rng.range(6)];
    let mut v = Vec::with_capacity(size);
    while v.len() < size {
        if alphabet > 1 && rng.range(4) == 0 {
            // A run of one byte — creates matches the parser can back-reference.
            let run = 1 + rng.range(64);
            let b = rng.range(alphabet) as u8;
            for _ in 0..run {
                if v.len() >= size {
                    break;
                }
                v.push(b);
            }
        } else {
            v.push(rng.range(alphabet) as u8);
        }
    }
    v
}

/// Every level, on a wide sweep of sizes and byte distributions (plus exact spec boundaries), must
/// round-trip byte-exact through the reference inflater for both the raw and zlib framings — and
/// never panic. This is the crate's robustness net.
#[test]
fn randomized_round_trip() {
    let mut rng = Rng(0x1234_5678_9ABC_DEF0);
    // Spec-significant small sizes first, then a spread of pseudo-random sizes. Sizes stay modest so
    // the debug-profile optimal parse (`Level::Best`) is cheap over many cases; larger inputs at
    // every level are covered by the corpus round-trips (incl. the 70 KiB multi-block case).
    let boundaries = [0usize, 1, 2, 3, 4, 257, 258, 259];
    for case in 0..130usize {
        let size = if case < boundaries.len() {
            boundaries[case]
        } else {
            rng.range(1500)
        };
        let data = generate(&mut rng, size);
        for &level in LEVELS {
            let enc = DeflateEncoder::new().with_level(level);

            let mut raw = Vec::new();
            enc.compress(&data, &mut raw);
            assert_eq!(
                zlib_oracle::inflate_raw(&raw).unwrap(),
                data,
                "raw case {case} {level:?} size {size}"
            );

            let mut zl = Vec::new();
            enc.zlib_compress(&data, &mut zl);
            assert_eq!(
                zlib_oracle::inflate_zlib(&zl).unwrap(),
                data,
                "zlib case {case} {level:?} size {size}"
            );
        }
    }
}

/// End-to-end coverage of the two size-dependent edges: the 65 535-byte stored-block `LEN` split
/// (level-independent, so exercised through the fast levels) and the optimal-parse limit, which
/// past #343 spans the input rather than disabling the parse — just above it, `Level::Best` runs
/// the optimal parse over two spans.
#[test]
fn large_and_boundary_round_trip() {
    let mut rng = Rng(0xDEAD_BEEF_CAFE_0001);

    // Stored-block splitting at and around the 16-bit `LEN` ceiling.
    for &size in &[65_535usize, 65_536, 70_000] {
        let data = generate(&mut rng, size);
        for &level in &[Level::Store, Level::Fast, Level::Default] {
            let mut zl = Vec::new();
            DeflateEncoder::new()
                .with_level(level)
                .zlib_compress(&data, &mut zl);
            assert_eq!(
                zlib_oracle::inflate_zlib(&zl).unwrap(),
                data,
                "{level:?} size {size}"
            );
        }
    }

    // Just past the optimal-parse limit, where the input is now split into two spans (the second
    // one byte long). Effort 0 keeps this fast under mutation while still exercising the span
    // loop and its seed parse.
    let big = generate(&mut rng, (1 << 20) + 1);
    for &level in &[Level::Default, Level::Best] {
        let mut zl = Vec::new();
        DeflateEncoder::new()
            .with_level(level)
            .with_deflate_passes(0)
            .zlib_compress(&big, &mut zl);
        assert_eq!(
            zlib_oracle::inflate_zlib(&zl).unwrap(),
            big,
            "{level:?} size {}",
            big.len()
        );
    }
}

/// #343: above the old 1 MiB cut-off, `Level::Best` used to fall back to lazy matching and could
/// come out *larger* than the cheaper levels — the crate's headline "Best is smallest" claim
/// failing on a chunking technicality rather than on the data. The optimal parse now spans the
/// input instead, so the ordering must hold at any size.
///
/// The fixture is deliberately compressible-but-structured: on incompressible noise every level
/// converges on stored blocks and the comparison proves nothing.
#[test]
fn best_beats_default_above_the_optimal_parse_limit() {
    let mut rng = Rng(0x0BAD_C0DE_1234_5678);
    let mut data = Vec::with_capacity((1 << 20) + (1 << 18));
    while data.len() < (1 << 20) + (1 << 18) {
        // Repeating structure with drifting noise, the shape a raw image strip has.
        let chunk = generate(&mut rng, 512);
        data.extend_from_slice(&chunk);
        data.extend_from_slice(&chunk);
    }
    assert!(
        data.len() > 1 << 20,
        "fixture must exceed the default limit"
    );

    let size = |enc: DeflateEncoder| {
        let mut out = Vec::new();
        enc.zlib_compress(&data, &mut out);
        assert_eq!(zlib_oracle::inflate_zlib(&out).unwrap(), data);
        out.len()
    };

    let default = size(DeflateEncoder::new().with_level(Level::Default));
    let best = size(
        DeflateEncoder::new()
            .with_level(Level::Best)
            .with_deflate_passes(2),
    );
    assert!(
        best < default,
        "Best ({best}) must beat Default ({default}) on a {}-byte input",
        data.len()
    );
}

/// The limit must be a live knob, not a stored-and-ignored field: a one-span parse of an input
/// that spans several times over at the small setting has more cost-model context and cannot come
/// out larger, and the two settings must not produce byte-identical output.
#[test]
fn optimal_parse_limit_is_live() {
    // Several windows' worth, so the narrow setting is genuinely many spans.
    const NARROW: usize = 32_768;
    let mut rng = Rng(0x5EED_0343_0343_5EED);
    let mut data = Vec::new();
    while data.len() < NARROW * 6 {
        let chunk = generate(&mut rng, 300);
        data.extend_from_slice(&chunk);
        data.extend_from_slice(&chunk);
    }

    let size = |limit: usize| {
        let mut out = Vec::new();
        DeflateEncoder::new()
            .with_level(Level::Best)
            .with_deflate_passes(2)
            .with_optimal_parse_limit(limit)
            .zlib_compress(&data, &mut out);
        assert_eq!(zlib_oracle::inflate_zlib(&out).unwrap(), data);
        out.len()
    };

    let narrow = size(NARROW);
    let whole = size(data.len());
    assert_ne!(
        narrow, whole,
        "the limit must change the parse; both settings produced {narrow} bytes"
    );
    assert!(
        whole <= narrow,
        "one span ({whole}) should not be worse than {} spans ({narrow})",
        data.len().div_ceil(NARROW)
    );
}

/// A zero limit must be raised to the window floor, not looped on forever.
#[test]
fn zero_optimal_parse_limit_terminates() {
    let data = b"abracadabra alakazam ".repeat(40);
    let mut out = Vec::new();
    DeflateEncoder::new()
        .with_level(Level::Best)
        .with_deflate_passes(1)
        .with_optimal_parse_limit(0)
        .zlib_compress(&data, &mut out);
    assert_eq!(zlib_oracle::inflate_zlib(&out).unwrap(), data);
}
