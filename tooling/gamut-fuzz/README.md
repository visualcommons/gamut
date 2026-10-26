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
  runs `mise run check-fuzz` — Clippy under the workspace's `-D warnings`, which compiles every
  target and lints it too (#615); no nightly, no sanitizer, no engine — exactly as it already does
  for the excluded real-DNG conformance tier with `mise run check-dng-real`.
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
| `ifd_one_store` | `gamut-ifd` | `setters_keep_one_store_per_tag`, `one_store_per_tag` |
| `core_convert` | `gamut-core` | `acceptance_is_independent_of_the_samples`, `output_shape_matches_the_target_layout`, `palette_and_cmyk_convert_only_to_themselves`, `the_in_place_door_matches_the_allocating_door`, `converting_a_layout_to_itself_changes_nothing` |
| `tonemap_curves` | `gamut-tonemap` | `output_is_non_negative_and_never_nan`, `map_slice_is_elementwise_map`, `map_slice_is_order_independent`, `monotonic_non_decreasing` |
| `avif_clean_alpha` | `gamut-avif` | `cleaning_8_bit_rgba_keeps_the_composite`, `cleaning_16_bit_rgba_keeps_the_composite` |
| `avif_parse_decode` | `gamut-avif` | `decoding_stays_within_limits` |
| `color_gray8_view` | `gamut-color` | `gray8_view_is_lossless_widening_then_gray16_view` |

One file per crate, deliberately, so adding a crate is an additive change — with two exceptions,
both because a crate's laws need different inputs. `gamut-ifd` has two: `ifd_read_ledger` reads
the bytes as byte ranges, `ifd_one_store` as setter calls — once as a flat edit sequence for
`setters_keep_one_store_per_tag`, once as a program building a nested directory tree for
`one_store_per_tag`, whose descent into children the flat law never reaches. `gamut-avif` has two:
`avif_clean_alpha` normalises the engine's bytes into an RGBA image; `avif_parse_decode` hands
them, unchanged, to `AvifContainer::parse` and then decodes every item through a stub AV1 decoder,
so it is also the crate's parser robustness target (`docs/testing.md`'s "Fuzz entry point" for
`gamut-avif`). Its size limits come from the input's last four bytes, which an AVIF parser keeps as a trailer, so its
seeds are unmodified AVIF files. The stub serves every decode it is asked for: how many a file can
demand is bounded by the library's own work caps (#762), held at the target's ceiling, which the
target therefore searches.

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

The six targets #264 added each carry at least one check the engine cannot make on its own, so
that a defect producing no crash is still visible — all but `dng_decode`, which has none that can
fail (both of its assertions are structure pins, below) and runs for the crash oracle alone, which
is what found #564. The ten #311 added (the second table) are crash-oracle-only by the same
standard: none lists a check, so none owes the injection audit an entry, and a check is added to
one only together with the injection that shows it can fail.

| target | crate | entry points | check beyond the crash oracle |
|---|---|---|---|
| `ifd_read` | `gamut-ifd` | `read`, `read_tree`, `read_audited` | the dual-ledger audit is complete — no byte read outside a claim, **and** no claim unread |
| `tiff_decode` | `gamut-tiff` | `TiffDecoder::{page_count,info_page,decode_page}` | the geometry the decode hands back equals the geometry the tags declare — reaching only the two lines that copy the shared tag reader's dimensions into `DecodedImage` and `convert_from_raw`'s output dims, never the reader itself |
| `dng_decode` | `gamut-dng` | `DngDecoder::{decode,verify_new_raw_image_digest}` | none — the crash oracle alone; its two assertions are structure pins |
| `isobmff_boxes` | `gamut-isobmff` | `walk_segments`, `walk_meta_children`, `read`, `BoxReader` | the segments tile `0..len` exactly — contiguous, **and** covering to end of file |
| `heic_container` | `gamut-heic` | `HeifContainer::parse` | every accessor agrees with the segment list, **and** every borrowed slice lies inside `data()` |
| `heic_hvcc` | `gamut-heic` | `HevcConfig::parse`, `annex_b*`, `validate_still_payload`, `iter_nal_units` | the Annex-B emitters append rather than replace — on the success path **and** on the error path |

The decoder and metadata-parser targets (#311) complete the per-crate table's "Fuzz entry point"
column: every entry point it lists is now driven. Each runs with the configuration a caller
handling untrusted input is told to use — the crate's default where its default is bounded, and
the crate's own documented limits where they are opt-in — so that what the engine reports is a
defect, not a restatement of a documented default:

| target | crate | entry points | configuration |
|---|---|---|---|
| `png_decode` | `gamut-png` | `PngDecoder::decode` (the rich path: every ancillary payload inflated) | default — 64 MiB sample budget, 16 MiB metadata budget |
| `jpeg_decode` | `gamut-jpeg` | `metadata`, `DecodeImage<Rgb8>` | default (#1024): a 64 MiB raster budget with dimensions at the format's 16-bit ceiling. The target restates it explicitly through `with_max_image_bytes` and `with_max_dimensions`. `permissive` policy, so CMYK/YCCK converts |
| `webp_decode` | `gamut-webp` | `DecodeImage<Rgba8>` | default — no configurable limit (#1025); the 14-bit frame dimensions bound it, but not inside libFuzzer's 2048 MB RSS limit: a conformant 16 384 × 16 384 lossless frame peaks above it, so an OOM report needs the triage the target's doc gives |
| `riff_chunks` | `gamut-riff` | `RiffReader` (to `None`), `MetadataChunks::read`, `WebpLayout::parse`, `c2pa_summary` | — |
| `jxl_decode` | `gamut-jxl` | `info`, `embedded_icc_profile`, `metadata_boxes`, `DecodeImage<Rgba8>`, and `decode_partial_image` where the full decode fails | `decode` feature only (no libjxl build); `permissive` policy |
| `av1_inspect` | `gamut-av1` | `Av1Decoder::inspect` | default `DecodeLimits` |
| `icc_parse` | `gamut-icc` | `IccProfile::parse` | lenient (strict only adds refusals) |
| `exif_parse` | `gamut-exif` | `ExifReader::parse_with_report`, then `gps`, `thumbnail_bytes`, `MakerNote::canon` | `recover_entries` and `audit_bytes` on: the widest walk |
| `xmp_parse` | `gamut-xmp` | `XmpMeta::from_packet` | — |
| `iptc_read_irb` | `gamut-iptc` | `IptcReader::read_irb`, then `read` | default conflict policy |

`gamut-deflate` has no target because it has no decoder, and `gamut-av2`/`gamut-vvc` none because
they are stubs with no function bodies.

**Two of them run slowly by design, not by defect.** In the 60-second local run that preceded them,
`webp_decode` and `jxl_decode` both wrote `slow-unit-*` artifacts and ran at under a hundred
executions per second, against thousands for the rest. Both are decompression amplification that
the formats permit and the crates' contracts state: a 54-byte lossy WebP declaring a 4176 × 4176
frame decodes to 70 MB of RGBA (about 1 s in a release build), and a 5-byte JPEG XL header declaring
9717 × 8098 makes `decode_partial_image` return its documented zero-filled `HeaderOnly` buffer at
those dimensions (about 1.1 s). Neither is a crash, and neither target narrows its input to avoid
them: an input the format allows is an input the target searches. That the WebP decoder offers no
caller-set limit at all, and the JPEG XL one only a fixed one, is the decoder-limits posture
question #311's audit filed separately, as #1025 and #1026. The third filing, #1024, gave
`JpegDecoder` its bounded default.

**A check is only listed here if it can fail — and each target's module doc names the injected
defect that made *each listed check* fail**, with the message it produced and the command that
reproduces it. That second half is the part a reader can re-run; without it "this check is live" is
a reading of the code, which is exactly what put the rows above wrong twice.

**One injection per listed check, not per target row.** Every "and" in the column above is a
separate assertion that fails for a separate defect, and a row that records one injection has
evidence for one of them. Two checks survived three rounds of review that way — `ifd_read`'s "no
claim unread" and `heic_hvcc`'s error path — because their row's single injection satisfied only
the other half. Where a check is an equality between an accessor and a count, inject in **both**
directions: one direction is silent on a file that holds no instance of the thing.

#### The audit that rule produces

The check set is derived from the table above rather than read off it: split each row's last cell
on its own bold `and`, and every conjunct is one listed check owed one injection. The five rows
that list a check yield **nine** listed checks, and sixteen injections stand behind them — more than one apiece wherever a
check is an accessor-versus-count equality, which is injected in both directions, or where one
check spans several bodies that each carry the contract on their own.

| listed check | target | injections recorded |
|---|---|---|
| no byte read outside a claim | `ifd_read` | 1 — header claimed as `header_size() - 1` |
| no claim unread | `ifd_read` | 1 — header claimed as `header_size() + 1` |
| the geometry that arrives equals the geometry the tags declare (reach: the copies after the shared tag reader) | `tiff_decode` | 1 — transpose the `DecodedImage` dimensions |
| the segments are contiguous | `isobmff_boxes` | 1 — record a box as `b.offset + 8..end` |
| the segments cover to end of file | `isobmff_boxes` | 1 — `segments.pop()` before the return |
| every accessor agrees with the segment list | `heic_container` | 6 — `boxes`/`appended_stream`/`trailer`, each under- and over-reporting |
| every borrowed slice lies inside `data()` | `heic_container` | 2 — `data()` returns a copy; `boxes()` yields copies |
| the emitters append on the success path | `heic_hvcc` | 2 — `annex_b_parameter_sets` begins `out.clear()`; `annex_b`'s own body begins `out.clear()` |
| the emitters append on the error path | `heic_hvcc` | 1 — `annex_b_payload` clears before returning `Err` |

Each injection's message and the command that reproduces it are in the target's own module docs;
this table only accounts for *coverage*, so a row with no injection is visible without reading six
files. The law targets are outside it by construction: their oracle is the `invariants` function
itself, which the property tier already drives.

Four of these injections report only because of a seed added for them, and each of the four was
first observed to report **nothing** on the seed set that preceded it — the "Seeds" section names
which seed feeds which check.

### Entries that could not fail

Each of these was listed as a check and each was removed or relabelled after an injection into the
defect it advertised produced no report:

1. `ifd_read` compared `read(data)` against `IfdReader::open(data)?.read_file()` — but `reader.rs`
   *defines* `read` as that expression, so the two sides were one function call written twice.
2. `heic_hvcc` compared `annex_b(..).is_ok()` against `annex_b_payload(..).is_ok()` on the same
   input.
3. `heic_hvcc` asserted `annex_b` equals the two calls its own body makes.
4. `dng_decode` compared a digest verdict against a decoded field that is read with the *same
   expression* on both sides.
5. `tiff_decode` claimed that `info_page` refuses the index `page_count` returns — which reduces to
   indexing a vector one past its own length, both sides being `read(data)?.ifds`.
6. `tiff_decode`'s replacement for 5 — the number of samples the decode physically yielded, against
   the declared geometry — was listed as the row's check for a round. `convert_from_raw` allocates
   its output as `ImageBuf::<Q>::zeroed(src.dims)`, so the returned count is the *dimensions'*
   product by construction: the assertion is the geometry comparison times a constant on both
   sides, and a transposition injected where the decode builds its `DecodedImage` passes it with
   exit 0. It is a pin now (below).

   The geometry comparison itself is **not** on this list, though an earlier round put it here.
   That round was right about its reach and wrong about what the reach means: because the probe
   and the decoder share one tag reader, the pair sees nothing of that reader — only the lines that
   copy its dimensions on to the caller's buffer. That reach is narrow but not empty, and the
   transposition above is a defect in exactly those lines that any non-square file reports. So it
   is the row's listed check, with the reach written into the row rather than implied.
7. `isobmff_boxes` claimed the box cursor strictly advances. `BoxReader::next_box` reads its
   4-byte size and 4-byte type through `take` before any success return, so no declared box size
   can stall the cursor: with the `size < header_size` guard removed, 851 173 executions over
   61 seconds reported nothing, while a `self.pos` rewind reports on the first seed.
8. `heic_container` checked that the segments tile `0..len`. `HeifContainer::parse` stores
   `gamut_isobmff::walk_segments(data)?` verbatim, so that is the same assertion over the same
   values as `isobmff_boxes`'s — the same injection produced the *identical* message in both — on a
   narrower input set. Two ten-minute runners searching one function, at a measured-zero marginal
   yield.
9. `dng_decode` claimed the raw image that *arrives* holds exactly `width × height × planes`
   samples "after every rewriting stage". There is no such stage: `RawImage`'s fields are private,
   both constructors refuse any other count through `check_sample_count`, and the decoder only
   calls `with_*` setters afterwards, none of which touches `samples` or `dims` — linearisation,
   active area and crop are recorded for a consumer, never applied. The injection that stood as
   its evidence sat inside a constructor, after its own gate: a defect no file can express.

Calling any of them a differential overstated what the tier proves.

### Structure pins

A claim about two bodies agreeing is a **structure pin**: worth keeping where it is free or where a
future change could genuinely split the bodies apart, worth nothing as a search. Each is labelled
as one at the site and none is listed in the table above.

| pin | target | why no input can fail it |
|---|---|---|
| `annex_b` equals its two documented halves | `heic_hvcc` | `annex_b`'s body *is* those two calls; folded into the append check's existing buffer at no extra emitter pass |
| no empty NAL unit | `heic_hvcc` | `NalUnitIter::next` errors on a zero length before it can yield one |
| the raw image holds `width × height × planes` samples | `dng_decode` | both `RawImage` constructors enforce the count, and no later decode stage touches `samples` or `dims` |
| the digest verdict matches the decoded field | `dng_decode` | both sides read `NewRawImageDigest` out of IFD 0 with the same expression; the call is made anyway for the crash oracle |
| a page that decodes must also describe | `tiff_decode` | `decode_page_samples` calls the tag reader before it reads a pixel |
| the sample count matches the declared geometry's product | `tiff_decode` | `ImageBuf` sizes its storage from its own dimensions, so this is the row's live check times `Rgb8::CHANNELS` |
| the box cursor strictly advances, and never runs past the end | `isobmff_boxes` | `next_box` consumes its 8-byte header through `take` before any success return |
| no segment is empty | `isobmff_boxes` | every shape `walk_segments` pushes is widened by that same header read, or runs to end of file |

The `gamut-ifd` wrapper pin (entry 1) lives in `crates/gamut-ifd/tests/robustness.rs` instead, over
a bounded exhaustive corpus, rather than costing half of every one of this target's twenty thousand
executions per second to search for a counterexample that does not exist. Dropping the two
duplicate parses raised `ifd_read` from roughly 12 000 exec/s to roughly 20 000.

An **allocation** defect needs the engine's malloc hook to be visible at all: an oversized
`Vec::with_capacity` costs no resident memory on an overcommitting kernel, so measuring RSS finds
nothing and `-malloc_limit_mb` (which libFuzzer defaults to `-rss_limit_mb`, 2048) is the oracle.
That is how `dng_decode` reports a 780-byte file asking for a 34 GB allocation.

### The first two defects it found

`Fuzz tiff_decode` and `Fuzz dng_decode` failed on the first defects this tier found:
[#563](https://github.com/visualcommons/gamut/issues/563) (a panic on `SamplesPerPixel = 0`) and
[#564](https://github.com/visualcommons/gamut/issues/564) (a raw buffer sized from declared
geometry). They were filed and fixed in the crates rather than worked around in the targets,
because narrowing a target so its row goes green is weakening a check to make a report green — the
opposite of what the tier is for. A row that goes red is read the same way: per row, since the job
is `fail-fast: false` and one target's crash says nothing about another's.

### The cadence

The fuzz job runs **nightly**, on `extended.yml`'s `schedule:` trigger, at `-max_total_time=3600`
per target, and on manual dispatch; it does **not** run per push
([#594](https://github.com/visualcommons/gamut/issues/594)). Every other Extended job keeps the
opposite cadence — per push to the default branch, skipped on the schedule — because rebuilding an
unchanged tree finds nothing new, whereas a fuzzer searching an unchanged tree for longer does.

- **Exploration, not regression.** A per-push run and a scheduled run are different jobs: the
  first catches a defect the push introduced, the second searches deeper for one that was already
  there. This tier is the second. Its regression value lives in the named deterministic tests a
  crash is promoted into, which run on every pull request; the price of dropping the per-push run
  is that a defect a push introduces into a target's reach is found up to a day later.
- **The persisted corpus is what makes depth pay.** Before
  [#603](https://github.com/visualcommons/gamut/issues/603) each run started from the committed
  seeds, so frequency was the only dial and every run was the same shallow search. Now each night
  resumes from the minimised corpus the last one left (see "The search corpus" below), so an hour
  a night accumulates into one long search instead of many repeated short ones. A nightly run also
  touches every cache entry daily, well inside the seven-day eviction window.
- **Cost scales with the matrix, once a day.** One runner per target per night, rather than per
  push, so adding a row adds an hour of runner time a day however busy the default branch is.

A per-push smoke run alongside the nightly one was considered and not taken: it would need a second
matrix that `check-targets.sh` reconciles against the first, for a regression signal the promoted
tests already give. When the matrix's cost or its search depth needs tuning, the dial is the
`-max_total_time` budget, not the trigger.

The **size** of the matrix is a separate question with the same answer. Each row is one target,
and dropping one to buy back runner time would trade coverage for a cost the nightly cadence and
the persisted corpus already bound — a shorter matrix is simply less search. A target is removed when
its checks stop having reach, which is what the injection audit decides, and never to make a job
finish sooner.

### The search corpus

Each Extended row resumes from the corpus the previous run left
([#603](https://github.com/visualcommons/gamut/issues/603)), so search deepens across runs instead
of restarting from the seeds. The engine's search state lives in `state/<target>/`, a directory of
its own that the root `.gitignore` ignores whole; the curated seeds stay in `corpus/<target>/`
(see "Seeds" below). Keeping them apart is the point: the seeds are tracked and reviewed, the
search state is neither, and a shared directory is how one leaks into the other. Per row the job

1. restores `state/<target>/` from an `actions/cache` entry keyed `fuzz-corpus-<target>-<run>`,
   taking the newest one for that target by prefix — a miss is the old cold start, not a failure;
2. fuzzes with `state/<target>/` as libFuzzer's **first** corpus directory, the only one it writes,
   and `corpus/<target>/` as a read-only second;
3. minimises `state/<target>/` in place with `cargo fuzz cmin`, so the stored corpus is bounded by
   the coverage the target can reach rather than growing with every run;
4. saves it under this run's own key — cache keys are immutable — even when the fuzz step failed,
   because a crash goes to `artifacts/` and leaves the inputs found before it just as valid.

The same layout works locally, and `mise run fuzz cmin` is the step CI runs:

```bash
mkdir -p tooling/gamut-fuzz/state/heic_hvcc
mise run fuzz heic_hvcc tooling/gamut-fuzz/state/heic_hvcc tooling/gamut-fuzz/corpus/heic_hvcc -- -max_total_time=60
mise run fuzz cmin heic_hvcc tooling/gamut-fuzz/state/heic_hvcc
```

The cache is a search aid with the same standing as the corpus it holds: never the regression
record, and disposable. Entries unused for seven days are evicted, and so are the oldest once the
repository passes its 10 GB cache limit; either only costs the next run its head start. A change to
a target's byte-to-input mapping makes the stored inputs less useful but not wrong, so nothing
invalidates the cache by hand — `cmin` drops what no longer adds coverage.

## Keeping the three lists in step

A target exists in three hand-maintained places: its `fuzz_targets/<name>.rs` file, its `[[bin]]`
entry in `Cargo.toml`, and its row in `extended.yml`'s fuzz matrix. Miss the third and the target
is written, committed, and never run — silently, because nothing fails. `check-targets.sh`
reconciles all three and is wired into CI's `Format & Metadata` job as `mise run
check-fuzz-matrix`; run the same thing locally:

```bash
mise run check-fuzz-matrix   # the three lists describe the same target set
mise run check-fuzz          # every target still compiles against the crates it drives, Clippy-clean
```

Both are tasks rather than bare commands so a contributor runs *what CI runs*: a step whose command
lives only in a workflow is a step nobody can reproduce without reading YAML, and the two copies
drift. `check-targets.sh` also fails a `[[bin]]` whose `name` disagrees with its own `path`, and
names a duplicated entry as a duplicate rather than mis-reporting it as a missing file.

## Seeds

`corpus/<target>/` holds a small **curated seed set**. The root `.gitignore` ignores everything
under `tooling/gamut-fuzz/corpus/` so the engine's *search state* is never committed, and then
re-includes each seed **by its full path**, one negation per file — so the ignore file itself is
the list of what is tracked here. `cargo fuzz` uses the directory as its corpus with no extra
wiring, so `mise run fuzz <target>` picks them up.

Running a target with no corpus arguments writes its new findings into the same directory — a few
minutes of `heic_hvcc` adds a couple of hundred files — and those stay ignored, which is the point
(the Extended job instead passes `state/<target>/` first, so its findings never land here; see "The
search corpus" above). That is also why the
negations name files rather than patterns: libFuzzer names its findings by content hash, and a
wildcard broad enough to cover the next seed would sooner or later cover them too. To add a seed,
append its `!tooling/gamut-fuzz/corpus/<target>/<file>` line to `.gitignore` and `git add` it by
path; **never `git add -f`** the directory, or the engine's search state goes in with it.

They are seeds, **not** the regression record. `corpus/ifd_read/` carries the thirteen cases
enumerated on issue #264 (contributed from rawshift's deleted in-repo TIFF parser) — eleven that
must be refused and two that must parse — under that issue's own numbering, so a file maps back to
a row of its table. Two things about the numbering, since neither is guessable from `ls`: **case 2
carries no file**, because `"II" LE16(42)` *is* `"II" 2A 00` byte for byte and it would be the same
four bytes as case 1; and cases 9 and 10 carry two files each, `a` and `b`, for the two variants
their rows name.

Of the seven other directories that predate #311, six start from one or two small well-formed files, written by this
workspace's own encoders, so a decoder target begins from something that reaches its pixel path
instead of spending its budget rediscovering a header. `corpus/avif_parse_decode/`'s two were
written by `gamut-isobmff`'s writer in the shape its property generates — a 2 × 2 `grid` cropped to
7 × 9 whose last tile declares an `ispe`, and a two-input `iovl` with an input off each canvas edge,
over coded items whose payloads pass the still-image checks — then given a four-byte limits trailer
(`grid-2x2-limits-6`: a 6-pixel side, which refuses the grid; `overlay-two-inputs-limits-100px`: a
100-pixel budget, which refuses the 12 × 10 canvas), since the AVIF seed `isobmff_boxes` uses does
not pass those checks and never reaches the decoder. The seventh, `corpus/avif_clean_alpha/`, feeds
a law target that parses nothing, so its two seeds are hand-built raw bytes in the driver's own
input shape — a width selector byte, then RGBA samples — that put opaque, fully transparent and
semi-transparent pixels side by side: `8bit-2x2-edge.bin` aimed at the 8-bit law, and
`16bit-1x3-edge.bin` at the 16-bit one (its samples read as little-endian `u16` pairs).

Beyond those, **a seed is added whenever a listed check turns out to have no witness in the
corpus** — that is what the injection audit above is for, and a check whose only witness has to be
synthesised by the engine is a check the tier is asking luck for. Four exist for that reason and
each names the check it feeds:

| seed | the check it makes reachable |
|---|---|
| `ifd_read/padding-unread-claim.tif` | "no claim unread". Every #264 case puts IFD0 at offset 8, so byte 8 *is* read as the entry count and an over-claimed header lands on a byte the ledger already holds. This one points IFD0 at offset 16, leaving `8..16` as padding nothing reads. |
| `heic_container/appended-stream.heic` | `appended_stream()` returning `None` when a segment of that kind exists — silent on a file that has no appended stream, which the original seed did not. A second top-level `ftyp`, as a motion-photo phone writes. |
| `heic_container/trailer.heic` | `trailer()` returning `None` when a trailer exists. A truncated trailing box header, retained as a trailer once `ftyp` and `meta` are seen. |
| `heic_hvcc/truncated-payload-nal.bin` | the append contract **on the error path**. The well-formed record's payload splits cleanly, so `annex_b_payload` never returns `Err` for it. Same record, one NAL length prefix raised past the end of the payload. |

`corpus/tiff_decode/` carries two — `rgb8-none.tif` and `rgb8-lzw.tif` — because an uncompressed
strip and an LZW strip enter the decoder through different code, and seeding only one leaves the
other to be rediscovered. Real-camera corpora are deliberately not vendored: they run to hundreds
of megabytes and live in `justin13888/rawshift-test-fixtures` releases. What they would teach the
engine is carried by a dictionary instead (next section).

The #311 targets' seeds follow the same rule — small, well-formed, and written by this workspace —
from one of two sources. Where a crate's own suite already tracks a fixture, the seed is a copy of
it: `jpeg_decode/` holds `gamut-jpeg`'s `color_baseline.jpg` and `color_progressive.jpg` goldens
(sequential and progressive scans enter the decoder through different code), `exif_parse/` holds
`gamut-exif`'s `golden_bare_le.bin` and `golden_marker_le.bin` (a bare TIFF stream and an
`Exif\0\0`-prefixed one), and `icc_parse/` holds `gamut-jpeg`'s vendored XYB profile. The rest were
written by the workspace's encoders over one 5 × 3 image — an odd width, so no row is a multiple of
a filter stride: `png_decode/` an RGB and an RGBA PNG; `webp_decode/` a lossless `VP8L` RGBA file
and a lossy `VP8 ` RGB one; `riff_chunks/` a lossless `VP8X` file carrying the Exif golden above
and an XMP packet as `EXIF` and `XMP ` chunks; `av1_inspect/` `encode_still_lossless_identity`'s
4:4:4 temporal unit, the one profile the decoder accepts; `xmp_parse/` an `XmpMeta::to_packet`
packet with two properties; and `iptc_read_irb/` `IptcWriter::write_irb`'s block for a headline and
a city.

`jxl_decode/` has **no seed**: the only JPEG XL encoder here is libjxl through `gamut-jxl-sys`, and
the target is built without it. It starts cold: the engine synthesised the `FF 0A` signature and a
header the decoder accepts within a 60-second local run, but a seed written once by libjxl and
committed would start it at a frame instead.

## Dictionaries

`dict/` holds libFuzzer `-dict` files for the TIFF/IFD parser targets
([#567](https://github.com/visualcommons/gamut/issues/567)). The decoder targets start from seeds
this workspace's own encoders wrote, which reach the pixel path but nothing camera-specific —
maker notes, sub-IFD trees several levels deep, opcode lists. A dictionary hands the mutator the
tokens those constructs are made of, so it splices a whole tag ID or magic in one step instead of
assembling it a byte at a time.

| file | applied to | holds |
|---|---|---|
| `dict/ifd.dict` | `ifd_read`, `tiff_decode`, `dng_decode` | TIFF and BigTIFF headers, the IFD/LONG8/SLONG8/IFD8 field types, the pointer tags, the layout tags `gamut-tiff` reads, the embedded-metadata tags |
| `dict/dng.dict` | `dng_decode` | every DNG-range tag gamut-dng's decode path reads, the Compression/PhotometricInterpretation codes that pick a decode path, DNGVersion octets, the opcode IDs, JPEG/JPEG XL codestream markers, MakerNote and DNGPrivateData magics |

Each multi-byte tag token appears in both byte orders, since the real-camera files use both.
Opcode IDs appear once, big-endian: an opcode list is big-endian whatever the file's byte order.
`dng.dict` was distilled from the real-camera DNGs in the `gamut-dng-samples` submodule (`mise run
fetch-dng-samples`) by walking every IFD, sub-IFD and Exif IFD in them. Its header says which
tokens came from those files and which came from gamut-dng's tag table or the DNG specification.

`run.sh` applies them, so `mise run fuzz <target>` locally and the Extended job search with the
same dictionary. libFuzzer reads a single `-dict` file, so for `dng_decode` the runner writes
`ifd.dict` followed by `dng.dict` to `state/dng_decode.dict` (ignored, like all of `state/`). The
runner keys on the first argument, so name the target first, as every example here does. An
explicit `-dict=<file>` argument replaces the committed dictionary for that run. The law targets
take none: their input is a structured encoding, not a file format, so no token in it has a fixed
spelling.

A real-camera **seed corpus** was considered and not taken. Vendoring one makes every clone pay
for hundreds of megabytes, and a third fetch task on the `fetch-av1-oracles`/`fetch-dng-samples`
pattern would leave the Extended job choosing between fetching it every night and seeding without
it. A multi-megabyte seed also lowers the executions per second each row's hour buys. The dictionary carries the camera-specific tokens for a few kilobytes.
