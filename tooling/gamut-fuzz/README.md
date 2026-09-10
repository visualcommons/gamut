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
  Nothing on the pull-request path would otherwise compile these targets at all, and an API change
  in a driven crate would break them unnoticed until the next Extended run. CI's lint job therefore
  runs `mise run check-fuzz` — build-only, no nightly, no sanitizer, no engine — exactly as it
  already does for the excluded real-DNG conformance tier with `mise run check-dng-real`.
- **The dependency graph is shared across every target.** A feature turned on for one target's
  crate is on for all of them, because Cargo resolves features once per crate for the whole
  package: `bigtiff` was added to `gamut-ifd` for the `ifd_read` driver, and the pre-existing
  `ifd_read_ledger` law target is now built with it too. That is harmless here — `bigtiff` widens
  the accepted input rather than changing the laws — but it is not free in general, and a feature
  added for one target must be checked against the others before it goes in.
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

`Drago` is held to monotonicity only where `Drago::is_monotonic` says it claims it (#439); every
other operator promises it unconditionally, and all of them are driven through the other three
laws.

The `tonemap_curves` target found a defect **in a law** within a minute of first running: the
monotonicity tolerance derived its scale from the sampled outputs, so a sample set drawn entirely
from `Hable`'s near-zero cancellation region measured the noise against itself. Fixed in the same
change, with the case promoted into a named test — which is the workflow this file prescribes,
exercised once.

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
| `ifd_read` | `gamut-ifd` | `read`, `read_tree`, `read_audited` | the dual-ledger audit is complete: no byte read outside a claim, no claim unread |
| `tiff_decode` | `gamut-tiff` | `TiffDecoder::{page_count,info_page,decode_page}` | a page that decodes yields exactly `width × height × Rgb8::CHANNELS` samples for the geometry the tags declare |
| `dng_decode` | `gamut-dng` | `DngDecoder::{decode,verify_new_raw_image_digest}` | the raw image that *arrives* holds exactly `width × height × planes` samples, after every rewriting stage |
| `isobmff_boxes` | `gamut-isobmff` | `walk_segments`, `walk_meta_children`, `read`, `BoxReader` | the box cursor strictly advances; the segments tile `0..len` exactly |
| `heic_container` | `gamut-heic` | `HeifContainer::parse` | the segments tile `0..len` exactly and every accessor agrees with that tiling |
| `heic_hvcc` | `gamut-heic` | `HevcConfig::parse`, `annex_b*`, `validate_still_payload`, `iter_nal_units` | the Annex-B emitters append rather than replace, on the success path and the error path |

**A check is only listed here if it can fail — and each target's module doc names the injected
defect that made it fail**, with the message it produced and the command that reproduces it. That
second half is the part a reader can re-run; without it "this check is live" is a reading of the
code, which is exactly what put the rows below wrong twice.

Five earlier entries could not fail. `ifd_read` compared `read(data)` against
`IfdReader::open(data)?.read_file()` — but `reader.rs` *defines* `read` as that expression, so the
two sides were one function call written twice. `heic_hvcc` compared `annex_b(..).is_ok()` against
`annex_b_payload(..).is_ok()` on the same input, and asserted `annex_b` equals the two calls its
own body makes. `dng_decode` compared a digest verdict against a decoded field that is read with
the *same expression* on both sides. `tiff_decode` claimed two: that `info_page` refuses the index
`page_count` returns — which reduces to indexing a vector one past its own length, both sides
being `read(data)?.ifds` — and that the described and decoded geometry agree, which sees only the
few lines copying one into the other, because `decode_page_samples` says outright that
"everything the page *declares* comes from one shared reader". None of them had a reachable
failure, and calling any of them a differential overstated what the tier proves.

The two `tiff_decode` claims are **dropped**, and the target is re-anchored on something the
geometry reader does not produce: the number of samples the decode physically yielded, against the
declared dimensions and the channel count of the layout asked for. The rest are **relabelled and
repriced**. A claim about two bodies agreeing is a **structure pin**: worth keeping where it is
free or where a future change could genuinely split the bodies apart, worth nothing as a search.
So `heic_hvcc` still asserts the two halves, folded into the append check's existing buffer at no
extra emitter pass; `dng_decode` still compares the verdict, on a call it makes anyway for the
crash oracle; and the `gamut-ifd` wrapper pin lives in `crates/gamut-ifd/tests/robustness.rs`,
over a bounded exhaustive corpus, rather than costing half of every one of this target's twenty
thousand executions per second to search for a counterexample that does not exist. Dropping the
two duplicate parses raised `ifd_read` from roughly 12 000 exec/s to roughly 20 000.

Two smaller assertions are pins for the same reason and are labelled as such at the site, so
nobody reads them as checks: `tiff_decode`'s "a page that decodes must also describe" (decoding
calls the tag reader before it reads a pixel) and `heic_hvcc`'s "no empty NAL unit"
(`NalUnitIter::next` errors on a zero length before it can yield one). Both cost one comparison on
a value already in hand.

An **allocation** defect needs the engine's malloc hook to be visible at all: an oversized
`Vec::with_capacity` costs no resident memory on an overcommitting kernel, so measuring RSS finds
nothing and `-malloc_limit_mb` (which libFuzzer defaults to `-rss_limit_mb`, 2048) is the oracle.
That is how `dng_decode` reports a 780-byte file asking for a 34 GB allocation.

### Two of these rows are red on purpose

`Fuzz tiff_decode` and `Fuzz dng_decode` **fail today**, on the first defects this tier found:
[#563](https://github.com/visualcommons/gamut/issues/563) (a panic on `SamplesPerPixel = 0`) and
[#564](https://github.com/visualcommons/gamut/issues/564) (a raw buffer sized from declared
geometry). They are filed rather than fixed, because narrowing a target so its row goes green is
weakening a check to make a report green — the opposite of what the tier is for.

What that costs, stated plainly so nobody has to rediscover it: **Extended runs on every push to
the default branch, so its aggregate status stays red until both are fixed.** The blast radius is
bounded — Extended is post-merge and manual-dispatch only, and the fuzz job is `fail-fast: false`,
so no pull request is blocked and no other row is cancelled — but a human scanning one red tick
per push learns nothing from it. Read the per-row status, not the aggregate, until #563 and #564
close; both rows go green with no change here. Whether these two rows should instead live in a
separate, expected-to-fail lane so the aggregate keeps its meaning is
[#593](https://github.com/visualcommons/gamut/issues/593) — a workflow-topology question, not a
fuzzing one.

### The cadence, answered

Nine parallel ten-minute runners on every push to the default branch, growing by one runner per
target, was inherited from the workflow's trigger rather than chosen ([#594](https://github.com/visualcommons/gamut/issues/594)).
It is **kept**, and here is why, so the next target added does not reopen it:

- The cost is queue time, not budget — Actions minutes are free for public repositories — and the
  job is post-merge with `fail-fast: false`, so it blocks no pull request. The matrix grows the
  number of *parallel* runners, not the job's wall time.
- Frequency is the wrong dial, because **nothing accumulates between runs**. Each run starts from
  the committed seeds and discards what the engine finds, so a run's yield is ten minutes of cold
  search whatever the cadence: running less often searches strictly less, and running more often
  re-derives the same shallow space. What would change the tier's yield is persisting the corpus,
  filed as [#603](https://github.com/visualcommons/gamut/issues/603) — and the cadence and the
  `-max_total_time` budget are both worth re-opening *after* that lands, not before.
- Changing the trigger would also move #593's premise (whether a per-push aggregate is red), which
  is a decision about the workflow's shape rather than about this tier.

## Keeping the three lists in step

A target exists in three hand-maintained places: its `fuzz_targets/<name>.rs` file, its `[[bin]]`
entry in `Cargo.toml`, and its row in `extended.yml`'s fuzz matrix. Miss the third and the target
is written, committed, and never run — silently, because nothing fails. `check-targets.sh`
reconciles all three and is wired into CI's `Format & Metadata` job as `mise run
check-fuzz-matrix`; run the same thing locally:

```bash
mise run check-fuzz-matrix   # the three lists describe the same target set
mise run check-fuzz          # every target still compiles against the crates it drives
```

Both are tasks rather than bare commands so a contributor runs *what CI runs*: a step whose command
lives only in a workflow is a step nobody can reproduce without reading YAML, and the two copies
drift. `check-targets.sh` also fails a `[[bin]]` whose `name` disagrees with its own `path`, and
names a duplicated entry as a duplicate rather than mis-reporting it as a missing file.

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

They are seeds, **not** the regression record. `corpus/ifd_read/` carries the thirteen
malformed-TIFF cases enumerated on issue #264 (contributed from rawshift's deleted in-repo TIFF
parser); each of the other five directories carries one or two small well-formed files, written by
this workspace's own encoders, so a decoder target starts from something that reaches its pixel
path instead of spending its budget rediscovering a header. `corpus/tiff_decode/` carries two —
`rgb8-none.tif` and `rgb8-lzw.tif` — because an uncompressed strip and an LZW strip enter the
decoder through different code, and seeding only one leaves the other to be rediscovered.
Real-camera corpora are deliberately not vendored: they run to hundreds of
megabytes and live in `justin13888/rawshift-test-fixtures` releases.
