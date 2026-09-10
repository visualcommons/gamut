# gamut-fuzz

The dev-only fuzz tier (issues #264, #311). It drives the crates' `invariants` modules — the
**same** executable laws the pinned-seed `proptest` properties drive in the per-PR gate — under
libFuzzer.

```bash
mise run fuzz                                        # list the targets
mise run fuzz ifd_read_ledger                        # until it finds something, or Ctrl-C
mise run fuzz ifd_read_ledger -- -max_total_time=60  # bounded
```

## Why the law is not restated here

`docs/testing.md` puts it as *"a property is the specification a fuzzer checks"*. Each law is a
plain function in its crate's `invariants` module, exposed by the `doc(hidden)` `test-support`
feature, and **both** tiers call it. A law written twice is a law that can disagree with itself,
and the disagreement would be invisible: each tier would keep passing against its own copy.

What differs between the tiers is only the search:

| | property (per-PR gate) | this tier (extended CI) |
|---|---|---|
| cases | 512, from a pinned seed | unbounded, coverage-guided |
| reproducible | yes, by construction | only via a saved input |
| runs in | `coverage`, the blocking gate | explicitly, or `extended.yml` |

The pilot in #434 is the argument that the second tier earns its place: the property there killed
a mutant `.cargo/mutants.toml` had recorded as **provably equivalent**, on an input no hand-written
test had thought to try. 512 cases found it; this tier searches for the ones 512 will not.

## Why it is not in the per-PR gate

`coverage` is the only CI job that runs tests, so anything in it must be bounded and reproducible.
A coverage-guided engine is neither — it is time-bounded rather than iteration-bounded, and under
`cargo llvm-cov` instrumentation it would explore an unrecorded, load-dependent number of inputs.
That would be strictly *weaker* than the exhaustive truncation and single-byte-overwrite sweeps
already in `gamut-ifd/tests/robustness.rs`, while making the blocking mutation job
non-deterministic.

So it sits where the DNG sample corpus sits: an excluded `tooling/` crate, invoked explicitly.

## What to do with a crash

**Minimise it, then promote it into a named deterministic test** in the crate's own suite:

```bash
cargo +nightly fuzz tmin --fuzz-dir tooling/gamut-fuzz ifd_read_ledger <input>
```

The corpus is a search aid, not the regression record. A saved input is only reproducible while
the target's byte-to-input mapping is unchanged; a named case is reproducible forever. This is the
same rule `docs/testing.md` applies to a shrunk `proptest` counterexample, and the reason
`failure_persistence` is off there.

## Notes

- **Nightly is required.** libFuzzer needs `-Z sanitizer=fuzzer`, which stable does not expose.
  `rust-toolchain.toml` pins stable; the runner selects nightly for this crate only, the same shape
  as `mise run fmt` selecting a nightly rustfmt for its nightly-only options.
- **The host target is pinned.** cargo-fuzz 0.13 defaults `--target` to
  `x86_64-unknown-linux-musl`, and the sanitizer cannot link against a static libc. Without the
  pin the build fails before reaching a target at all.
- **This crate is workspace-excluded**, so `cargo test --workspace --all-features` never builds it.
- **The `--` is reconstructed, not passed through.** mise swallows a task's `--`, so
  `mise run fuzz t -- -max_total_time=60` reaches `run.sh` as two bare words and cargo-fuzz would
  reject the second as one of its own options. The runner re-splits on libFuzzer's own flag syntax
  (`-name=value`, single dash plus an `=`), so libFuzzer arguments and cargo-fuzz options both land
  where they belong with or without the separator. An explicit `--` still wins outright.

## Targets

There are two kinds, and the difference is what the target's oracle is.

### Law targets

They drive a crate's `invariants` module — the same functions the pinned-seed properties drive —
over normalised inputs, per the section above.

| target | crate | laws |
|---|---|---|
| `ifd_read_ledger` | `gamut-ifd` | `ledger_is_canonical`, `subtract_is_set_difference` |
| `core_convert` | `gamut-core` | `acceptance_is_independent_of_the_samples`, `output_shape_matches_the_target_layout`, `palette_and_cmyk_convert_only_to_themselves`, `the_in_place_door_matches_the_allocating_door`, `converting_a_layout_to_itself_changes_nothing` |
| `tonemap_curves` | `gamut-tonemap` | `output_is_non_negative_and_never_nan`, `map_slice_is_elementwise_map`, `map_slice_is_order_independent`, `monotonic_non_decreasing` |

One file per crate, deliberately, so adding a crate is an additive change.

### Robustness targets (#264)

They hand the engine's bytes, unchanged, to the **parser entry point** `docs/testing.md`'s
per-crate table names in its "Fuzz entry point" column — the surface an untrusted file arrives on.
There is no law function to share, because the primary oracle is the engine's own: every one of
these crates is `#![forbid(unsafe_code)]` and promises a *typed error* on hostile input, so a
panic, a hang, or an allocation past libFuzzer's limit is the defect.

Each target adds at least one check the engine cannot make on its own, so that a defect producing
no crash is still visible:

| target | crate | entry points | check beyond the crash oracle |
|---|---|---|---|
| `ifd_read` | `gamut-ifd` | `read`, `read_tree`, `read_audited`, `IfdReader` | slice and streaming readers agree; the dual-ledger audit is complete |
| `tiff_decode` | `gamut-tiff` | `TiffDecoder::{page_count,info_page,decode_page}` | the page index is bounded by `page_count`; describing and decoding agree on geometry |
| `dng_decode` | `gamut-dng` | `DngDecoder::{decode,verify_new_raw_image_digest}` | the decoded raw is self-consistent; the digest verdict agrees with the decoded model |
| `isobmff_boxes` | `gamut-isobmff` | `walk_segments`, `walk_meta_children`, `read`, `BoxReader` | the box cursor strictly advances; the segments tile `0..len` exactly |
| `heic_container` | `gamut-heic` | `HeifContainer::parse` | the segments tile `0..len` exactly and every accessor agrees with that tiling |
| `heic_hvcc` | `gamut-heic` | `HevcConfig::parse`, `annex_b*`, `validate_still_payload`, `iter_nal_units` | `annex_b` is its two documented halves, concatenated and appended |

An **allocation** defect needs the engine's malloc hook to be visible at all: an oversized
`Vec::with_capacity` costs no resident memory on an overcommitting kernel, so measuring RSS finds
nothing and `-malloc_limit_mb` (which libFuzzer defaults to `-rss_limit_mb`, 2048) is the oracle.
That is how `dng_decode` reports a 780-byte file asking for a 34 GB allocation.

## Seeds

`corpus/<target>/` holds a small **curated seed set**, tracked despite `.gitignore` listing
`tooling/gamut-fuzz/corpus/` — that ignore is there so the engine's *search state* is never
committed, and force-adding the seeds keeps exactly that split: the seeds are tracked, everything
libFuzzer writes beside them stays ignored. `cargo fuzz` uses the directory as its corpus with no
extra wiring, so `mise run fuzz <target>` picks them up.

Running a target writes its new findings into the same directory — a few minutes of `heic_hvcc`
adds a couple of hundred files — and those stay untracked, which is the point. **Never
`git add -f` the whole directory a second time**: add the one seed you mean by path, or the
engine's search state goes in with it.

They are seeds, **not** the regression record. `corpus/ifd_read/` carries the malformed-TIFF cases
enumerated on issue #264 (contributed from rawshift's deleted in-repo TIFF parser); the other
directories carry one small well-formed file each, written by this workspace's own encoders, so a
decoder target starts from something that reaches its pixel path instead of spending its budget
rediscovering a header. Real-camera corpora are deliberately not vendored: they run to hundreds of
megabytes and live in `justin13888/rawshift-test-fixtures` releases.

`Drago` is held to monotonicity only where `Drago::is_monotonic` says it claims it (#439); every
other operator promises it unconditionally, and all of them are driven through the other three
laws.

The `tonemap_curves` target found a defect **in a law** within a minute of first running: the
monotonicity tolerance derived its scale from the sampled outputs, so a sample set drawn entirely
from `Hable`'s near-zero cancellation region measured the noise against itself. Fixed in the same
change, with the case promoted into a named test — which is the workflow this file prescribes,
exercised once.
