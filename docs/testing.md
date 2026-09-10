# Testing

Normative for **where a test lives, what it is allowed to reach, which technique it uses, and how
a property and a fuzz target share one law**. What counts as an acceptable mutation survivor is
`AGENTS.md`'s rule; how a mutation survey is invoked is `docs/mutation-testing.md`. This document
is about writing a test that fails for one reason.

## The rule

> **A test names one thing and fails for one reason.** Minimise its *reach* — the set of modules a
> defect in which can fail it — and name it for that reach. Use the weakest technique that can
> falsify the claim.

Both halves matter, and the second does more work than the first. A round-trip is *self-consistent*:
it cannot see a defect that is symmetric across the encoder and the decoder. `.cargo/mutants.toml`
says so in its own justifications — one entry is excluded because the mutated value is "the
segment-map index it reads back (… self-consistent — the same id is signalled)", another because
"skipping a transform … emits a valid untransformed stream". Widening a test does not strengthen
it; past the point where the claim is expressible, widening blinds it.

## Where a test goes

Placement is mechanical. Two questions, in order:

1. **Does the assertion read a non-`pub` item?** → inline `#[cfg(test)] mod tests` beside it.
   This is the one placement fact the gates can see: `.cargo/mutants.toml` sets
   `test_workspace = false`, and a mutant inside a private function whose effect is masked at the
   public boundary is killable *only* from inside the crate's own lib target.
2. **Is there a hard linkage reason it cannot compile in the lib target?** → `crates/<crate>/tests/`.
   There are exactly three:
   - the assertion needs `unsafe` and the crate is `#![forbid(unsafe_code)]` — which is 21 of the
     32 crates, so this is the common case: `gamut-heic`'s `tests/backend.rs` and
     `tests/abi_borrowed_backend.rs`, `gamut-webp`'s `tests/backends.rs`, `gamut-avif`'s
     `tests/backend.rs`, and the FFI wrappers in `gamut-riff`/`gamut-webp`'s
     `tests/common/mod.rs`. The six hot-path crates (`gamut-png`, `gamut-deflate`, `gamut-dsp`,
     `gamut-color`, `gamut-cmm`, `gamut-jpeg`) are `deny`, not `forbid`, so this reason no longer
     *forces* them up — an `unsafe` assertion there can carry
     `#[expect(unsafe_code, reason = "…")]` and sit inline. Their existing suites stay where they
     are: rule 3 makes either legal, and the rule above never authorises a bulk relocation.
     `gamut-png`/`gamut-jpeg`'s `tests/backends.rs` are exactly that case;
   - the whole file is `#[cfg]`-gated on features or target (`gamut-jxl`, whose encoder is libjxl
     and absent on wasm32);
   - `mise run check-release-deps` topology forces it to the umbrella — why
     `crates/gamut/tests/xyb_icc.rs` is not in `gamut-jpeg`.
3. **Otherwise either is legal, and the choice is not policed.**

A **dev-dependency oracle is not a reason to move up**: dev-dependencies link into the lib's own
test build. `gamut-deflate/src/adler32.rs` calls `zlib_oracle::adler32` and
`gamut-dng/src/lossless_jpeg.rs` calls `gamut_dng_oracle`, both from inline `#[cfg(test)]` modules.

A test **may reach a sibling module** when that module is a fixture source, or when the claim is a
mutual-inverse or consistency law over the pair — an encoder and its decoder are siblings, and
"the encoder's reconstruction equals the decoder's output, bit-for-bit" has no narrower home. Name
it for the law, not for either module.

`crates/gamut/tests/` is **mutation-invisible** (`test_workspace = false`, and `crates/gamut/**` is
in `exclude_globs`). Nothing may be pinned *only* there **by choice**. A test the third linkage
cause forces to the umbrella is the exception, and it carries the cost explicitly: it names the
crate-level test that pins the same behaviour, or its module doc states that the behaviour is
accepted as mutation-invisible and why.

**This rule authorises moving a test only when it names a single module that already exists in
`src/`. It never authorises relocating a crate's suite in bulk.**

## What a test asserts

A preference ordering over techniques, cheapest first — **not** a partition of all tests. Reach for
the first one that can falsify the claim.

| Technique | Use when |
| --- | --- |
| **example / spec vector** | one named behaviour with a hand-computable or spec-quoted value |
| **exact-byte** | a round-trip would normalise the detail away — reserved bits, flag bytes, box sharing. `gamut-isobmff/tests/structure.rs` and `gamut-jpeg/tests/encode.rs` are the models |
| **law** | a universal predicate over enumerated inputs — the byte-accounting family (`gamut-avif`/`gamut-heic` `tests/accounting.rs`, `gamut-dng/tests/deconstruct.rs`) |
| **property** | a claim quantified over a domain of valid inputs, with no oracle to defer to |
| **differential** | the crate's oracle defines the behaviour |
| **conformance** | the specification ships vectors |
| **pin / drift guard** | an artifact must still equal an authority (`gamut-iptc/tests/techreference.rs`, `gamut-jxl-sys/tests/version.rs`) |
| **null-change invariance** | output must be *unchanged*, correctness belonging elsewhere (`gamut-webp/tests/default_bytes.rs`) |
| **size / effort contract** | an encoder knob's ladder is monotonic, deterministic, and correctness-independent (`gamut-webp/tests/effort.rs`), or its output stays within a budget against the crate's oracle (`crates/gamut-png/tests/size_contract.rs`) |
| **robustness** | input is hostile; the claim is "no panic, bounded allocation, typed error" |

A hand-written sweep over five sizes is a property test written badly. A property asserting one
literal is an example test written expensively. **Where an oracle exists, it is stronger than a
round-trip** — gamut writes both halves of every format, so a symmetric defect survives any
round-trip and only the oracle sees it.

Where an oracle and `references/` disagree, the specification wins (`docs/README.md` precedence) and
the divergence is recorded in that crate's `STATUS.md` — never pinned as expected behaviour.

## Properties and fuzzing are one law

> **A property is the specification a fuzzer checks.**

Write each law **once**, as a plain function over plain data, in the crate's `invariants` module:

```rust
// crates/<crate>/src/invariants.rs
#[cfg(any(test, feature = "test-support"))]
pub mod invariants;
```

It returns a typed violation rather than panicking, and it normalises its inputs into a bounded
universe so an arbitrary byte string maps to a cheap case. Two drivers call it:

- an inline `proptest` with a **pinned seed** — the per-PR gate;
- an out-of-tree corpus-guided driver — extended CI. This is `tooling/gamut-fuzz`, run with
  `mise run fuzz <target>` (issues #264, #311). It calls the same law functions the property
  calls: a law written twice is a law that can disagree with itself, and each tier would keep
  passing against its own copy. The `test-support` feature and the plain-function shape of the
  laws are what it attaches to.

`gamut_ifd::invariants` is the worked example. The `test-support` feature exposes the module to the
fuzz driver; it is **never re-exported by the `gamut` umbrella**, so the shipped surface and
`mise run check-ffi-features` are unaffected, and the crate's own tests get the module without it.

### Every property pins its seed

Non-negotiable, and it goes **in code**:

```rust
Config {
    cases: 512,
    max_shrink_iters: 2048,
    max_shrink_time: 10_000,
    rng_seed: RngSeed::Fixed(0x…),
    failure_persistence: None,
    ..Config::default()
}
```

- proptest seeds from OS entropy by default. The `--in-diff` mutation job is **blocking**, so a
  mutant revealed by a small fraction of the input space would be reported CAUGHT on one run and
  MISSED on the next — and the equivalence proofs in `.cargo/mutants.toml` are only meaningful
  against a deterministic suite.
- The shrink defaults are `u32::MAX` iterations and *no* wall-clock cap, and shrinking runs on the
  failing path — which is most of what a mutation survey does.
- The `PROPTEST_*` environment overrides are `#[cfg]`'d out on `wasm32`, so configuration that
  lives outside the code silently stops applying on any target the workspace tests there.
- **Failure persistence is off**, and for the same reason. cargo-mutants reuses one tree copy
  across many mutants, restoring only the file it mutated, so a `proptest-regressions` file
  written by one mutant's failure would survive into the next mutant's run and be replayed first
  — making CAUGHT/MISSED depend on mutant ordering and shard assignment. A shrunk counterexample
  is promoted into a **named deterministic test** instead, exactly as a fuzz crash is. That is
  where its regression value belongs: a committed seed is only reproducible while the generator
  is unchanged, and a named case is reproducible forever.

An `invariants` module is the **oracle, not the system under test**, so
`.cargo/mutants.toml` excludes `crates/*/src/invariants.rs` from mutation — the same reasoning it
already records for `tooling/**`. Without that exclusion a law weakened to `Ok(())` is a mutant
nothing can kill, because the tests assert exactly that it returns `Ok(())`; cargo-mutants does
not treat `#[cfg(any(test, feature = "test-support"))]` as test-only. Cover the laws' own failure
paths with ordinary `#[cfg(test)]` unit tests on deliberately-broken input.

**Do not convert existing deterministic sweeps to properties.** They run 200–20,000 iterations,
against the 512 a property here budgets, and several encode domain knowledge a generator loses —
raising `cases` to match costs the coverage gate and every one of ~24,000 mutant scenarios. The LZW
sweep in `gamut-tiff/src/compression/lzw.rs` picks inputs that cross the 9→10→11→12-bit code-width
boundaries. Properties are for claims that have no sweep, not a rewrite of the ones that do.

### Why fuzzing is not in the per-PR gate

The `coverage` job is the only gate that runs tests, so anything in it must be bounded and
reproducible. A corpus-guided engine under `cargo test` is neither: it is time-bounded rather than
iteration-bounded, re-seeds per iteration, and under `cargo llvm-cov` instrumentation explores an
unrecorded, load-dependent number of inputs — strictly weaker than the *exhaustive* truncation and
single-byte-overwrite sweeps already in `gamut-ifd/tests/robustness.rs`. Fuzzing therefore
lives where the DNG sample corpus lives: an excluded `tooling/` crate, a corpus behind a fetch
task, and an `extended.yml` job. **A crash it finds is minimised and promoted into a named
deterministic case in that crate's `tests/robustness.rs`**, which is where the regression value is.

`#[ignore]` is not used in this workspace and must not be introduced: `coverage` is the only test
gate, so an ignored test is not deferred, it is unrun.

## The per-crate selection table

The authority and primary technique for each crate are decided **here, once**. A row changes only
in a pull request that says why. "Authority" is the **in-crate** authority — several crates are
additionally covered by a consuming codec's oracle, which their own `STATUS.md` records. "Fuzz
entry point" names the untrusted-input surface a fuzz target takes; ☐ marks one not yet wired
(#264). The binary, binding and stub crates (`gamut`, `gamut-cli`, `gamut-wasm`, `gamut-ffi`,
`gamut-jxl-sys`, `gamut-av2`, `gamut-vvc`) have no row: they are excluded from the coverage and
mutation gates, and the stubs carry no function bodies.

| Crate | Authority | Primary technique | Fuzz entry point |
| --- | --- | --- | --- |
| gamut-core | *none* — no oracle exists | **property** (`convert`, `image` stride math) | — |
| gamut-ifd | *none in-crate* — libtiff/exiv2 reach it via the consuming codecs (STATUS.md P7) | **property** + exact-byte | `IfdReader`, `read` ☑ laws; ☐ driver |
| gamut-tonemap | *none* | **property** (monotonicity, endpoints, no NaN) | — |
| gamut-bitstream | *none* — self-inverse | property + exact-byte | — |
| gamut-dsp | AV1 §7.13 / T.81 §A.3 transform definitions | example + in-test reference transform | — |
| gamut-codec-abi | *none* | exact-byte + compile-time assertion | — |
| gamut-color | Little-CMS | differential | — |
| gamut-icc | Little-CMS | differential | `IccProfile::parse` ☐ |
| gamut-cmm | Little-CMS | differential | — |
| gamut-deflate | zlib | differential | — |
| gamut-png | libpng (both directions) | differential + conformance + size contract | `PngDecoder` ☐ |
| gamut-jpeg | libjpeg-turbo | differential + exact-byte | `JpegDecoder` ☐ |
| gamut-tiff | libtiff | differential | `TiffDecoder` ☐ |
| gamut-dng | Adobe DNG SDK; libtiff (container) | conformance + differential | `DngDecoder` ☐ |
| gamut-isobmff | ISO/IEC 14496-12 + 23008-12; libavif/dav1d via gamut-avif | exact-byte + law | `read` ☐ |
| gamut-riff | libwebp demux | differential + law | `RiffReader` ☐ |
| gamut-webp | libwebp (both directions) | differential + size/effort contract | `WebpDecoder` ☐ |
| gamut-avif | libavif; dav1d | differential + law | `decode` ☐ |
| gamut-heic | libheif + libde265 | differential + law | container `parse`, NAL `parse` ☐ |
| gamut-av1 | libaom (definitive); dav1d | differential | — |
| gamut-jxl | libjxl (the `jxl` crate is the decoder under test, not an authority) | differential | `decode` ☐ |
| gamut-exif | exiv2 | differential + golden | `parse` ☐ |
| gamut-xmp | exiv2 XMPCore | differential + golden | `parse` ☐ |
| gamut-iptc | exiv2; IPTC tech reference | differential + drift guard | `read_irb` ☐ |
| gamut-metadata | *facade* | law (round-trip through the facade) | — |

Eight crates hold no oracle dev-dependency of their own: `gamut-core`, `gamut-ifd`,
`gamut-tonemap`, `gamut-bitstream`, `gamut-dsp`, `gamut-codec-abi`, `gamut-isobmff` and
`gamut-metadata`. Most have something else standing in — `gamut-isobmff` has specification vectors,
exact-byte assertions and libavif's reach through `gamut-avif`; `gamut-ifd` is covered through
`gamut-tiff`'s libtiff and `gamut-exif`'s exiv2; `gamut-bitstream` and `gamut-dsp` check against a
self-inverse or a reference transform; `gamut-codec-abi` pins its ABI at compile time.

**Where a property is the primary signal rather than a supplement: `gamut-core`, `gamut-ifd` and
`gamut-tonemap`** — real algorithmic content, no reference implementation to defer to, and no
vectors shipped with a specification. `gamut-ifd` has one today (`src/invariants.rs`); the other
two are the next targets, not a description of what is there.

## Not governed here

Compile-time assertions (`gamut-codec-abi/src/lib.rs`'s `const _` ABI pins), the `mise run check-*`
gates, doctests (`mise run test-doc`), benchmarks, and the excluded
`tooling/gamut-dng-real-conformance` tier. These are real checks; they are simply not tests this
document places or classifies.

Benchmarks have their own document, [`benchmarking.md`](benchmarking.md): where one lives, what its
tables must record, and where the numbers are kept. The boundary is that a benchmark reports and
only a test can fail a build, so a size claim that must not regress is a **size / effort contract**
here — `gamut-png/tests/size_contract.rs` and `gamut-webp/tests/effort.rs` — not a bench.
