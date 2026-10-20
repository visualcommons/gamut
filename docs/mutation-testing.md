# Mutation testing

Normative for **how a mutation survey is invoked, what bounds it, and how to read what it
reports**. What counts as an acceptable survivor or a justified exclusion is `AGENTS.md`'s rule
and `.cargo/mutants.toml`'s prose — this document points at them rather than restating them. It
is about running the thing without taking the machine down, and about not misreading the result
once it has run.

## The one entry point

```bash
mise run mutants -- --help                 # selection, budget and guard flags
mise run mutants -- --crate gamut-png      # one package
mise run mutants-crate gamut-png           # same thing, shorter
mise run mutants-diff                      # only what this branch changed (what PR CI runs)
mise run mutants -- --shard 1/16           # one shard of the whole workspace
```

All of these are `tooling/mutants/run.sh`, and so is every CI invocation. That is the point: when
CI spelled its own `cargo mutants` line, the dials documented in `mise.toml` applied to
everything except CI, which is where the runs were being killed.

Arguments after the task name go to the runner. Anything after a further `--` is handed to
`cargo mutants` verbatim — but some mise versions swallow that separator, so a script should
prefer the runner's own flags (`--verbose` rather than `-- -vV`) instead of relying on it.

## Why a plain `cargo mutants` overruns

Mutation testing multiplies the cost of a full `cargo test --all-features` by the mutant count —
24,115 across this workspace — and that test build compiles fourteen vendored C/C++ projects
(libjxl, libaom, dav1d, libavif, libheif + libde265 + kvazaar, libjpeg-turbo, libpng, libtiff,
three private zlibs, lcms2, exiv2 + expat + the Adobe XMP Toolkit, the Adobe DNG SDK) plus 139
integration binaries. What runs out is memory, not cores, so the failure is an OOM kill rather
than a slow run.

**Parallelism is a product, not a number.** `MUTANTS_JOBS` scenarios run at once; within each,
cargo runs `CARGO_BUILD_JOBS` build scripts concurrently; each of those fans out to
`CMAKE_BUILD_PARALLEL_LEVEL` compilers. Build scripts do not join cargo's jobserver, so
`--jobserver-tasks` bounds rustc and nothing underneath it. Bounding only the outer dial is what
makes "1 job" mean sixteen memory-hungry C++ compiles.

**Nothing bounds a mutant's memory.** cargo-mutants times a mutant out after 5x the baseline,
but there is no equivalent for allocation. Across roughly a thousand allocation sites in the
codec crates, a mutant that changes a dimension, a stride or a decode limit allocates without
limit. This is the failure that arrives hours into a run, and when the cgroup OOM killer answers
it, it picks the largest process in the group — as often `cargo mutants` itself, discarding every
result collected so far.

**The tree is copied per job.** cargo-mutants stopped honouring `.gitignore` by default in
25.0.2 and excludes only the top-level `target/` on its own, so every ignored artifact underneath
was copied into every build directory: a stale `mutants.out/`, `lcov.info`, a `tooling/*/target`
left by a standalone oracle build. `.cargo/mutants.toml` sets `gitignore = true`. Each job's
directory still grows a cold `target/` of its own, which is why the runner warns when the
filesystem is tight.

## What the runner does

Everything is derived from one memory budget, taken from the cgroup limit where one exists, else
`MemAvailable`. On a workstation running agents under a capped slice, or in a container, `free`
reports the host's memory and the process is killed long before reaching it.

| Dial | Derived from the budget | Override |
| ---- | ----------------------- | -------- |
| `MUTANTS_JOBS` | scenarios in flight, at most 4 | env |
| `MUTANTS_JOBSERVER_TASKS` | total compiler slots | env |
| `CARGO_BUILD_JOBS` | cargo jobs per scenario; also `NUM_JOBS`, which `cc::Build` obeys | env |
| `CMAKE_BUILD_PARALLEL_LEVEL` | compilers per vendored native build | env |
| `RUST_TEST_THREADS` | test threads per scenario | env |
| `TMPDIR` | `target/mutants-tmp` | env |
| `GAMUT_MUTANTS_BASE` | base ref for `--diff` (`origin/master`) | env |

The split is anchored on a measured configuration: jobs 2, jobserver-tasks 4,
`CARGO_BUILD_JOBS` 2 and `CMAKE_BUILD_PARALLEL_LEVEL` 2 — a product of 8 — peaks at about
12.8 GiB on a cold build, so the runner budgets roughly 2 GiB per concurrent compiler.

Two memory guards, because one is not enough:

- a systemd scope with `MemoryMax` and `MemorySwapMax=0` around the whole run, which stops it
  taking the machine down (`GAMUT_MUTANTS_NO_CGROUP=1` opts out);
- a per-process `ulimit -v` of a fixed 6 GiB — not derived from the budget, see *Reading the
  result* — which is what makes a runaway mutant abort itself first, so it is scored `caught`
  and the run continues instead of the cgroup killing cargo-mutants
  (`GAMUT_MUTANTS_NO_ULIMIT=1` opts out).

Both auto-detect: on a runner with no systemd user manager only the address-space limit applies.

Two refusals, each of which would otherwise surface as a crash much later:

- a tmpfs `TMPDIR`, which charges every build directory to memory — `/tmp` is a tmpfs on many
  systems, sometimes under a bind mount that is absent inside a mount namespace;
- an unsharded whole-workspace run, which takes days and is the run most likely to meet the
  mutant that allocates without limit. `--shard i/n` bounds it; `--all-at-once` means it anyway.

Low disk is a warning rather than a refusal: running out of it fails the build loudly, at the
point of failure, with a message naming the problem. That is the opposite of running out of
memory, where the OOM killer picks a victim that may be cargo-mutants itself.

`--dry-run` resolves and prints the whole invocation without running it.

## Reading the result

A run that prints no `MISSED` line is not necessarily a clean run.

**Exit code 3 is a timeout, not a survivor.** A mutant that makes the suite hang is scored
neither caught nor missed; the runner reports it as `TIMEOUT` and exits 3, and the `--in-diff`
gate fails on it just as it does on a survivor. Read the counts before triaging: a shard that
hits its own cap silently stops, so every mutant it never reached is unreported rather than
caught. For what to do about one, see `AGENTS.md` — a hang is a survivor for policy purposes,
and the loop that cannot make progress under mutation is usually a loop that should have been
bounded by the data it walks. Rewriting it as one turns an unkillable `TIMEOUT` into an ordinary
killable mutant, which is how `gamut-ifd`'s ledger walks lost theirs.

**The address-space cap is fixed so a verdict is a property of the code.** A mutant that
allocates without bound is ended by whichever limit it reaches first: under the per-process
address-space cap the scenario aborts and scores `caught`, past it the mutant runs to the test
timeout and scores `TIMEOUT`. `run.sh` used to derive that cap as budget ÷ jobs, so the same
mutant scored `caught` on an 8 GiB workstation and `TIMEOUT` on the 14 GiB CI runner (issue
#613). The cap is now a constant **6 GiB** on every machine; the budget still derives every
parallelism dial, but not this. 6 GiB because:

- it is the smallest address space a full build and test of one package fits in, and the
  budget's clamp floor, so no accepted budget is smaller than what one process may map and the
  per-process guard always fires before the cgroup's — the derived job count is 1 or at most
  budget ÷ 8, so even every scenario at its cap together stays inside the budget;
- it is ordered against the test timeout (`minimum_test_timeout = 60` in
  `.cargo/mutants.toml`): a runaway reaches 6 GiB inside 60 s at any sustained rate above
  about 100 MiB/s, which an allocation loop exceeds by an order of magnitude, so it meets the cap
  first everywhere. A lower cap widens that margin, which is the reason not to raise it without
  measuring a package that genuinely needs more.

The ordering is a rate argument, not a guarantee: a mutant that allocates slower than ~100 MiB/s
still reaches the timeout first, and does so on every machine alike. There is deliberately no
variable to change the cap — `GAMUT_MUTANTS_NO_ULIMIT=1` removes it, and a verdict taken without
it is not comparable with CI's.

**What the tool mutates is a short list**, and everything outside it is invisible to the gate.
Derive the list instead of trusting a written one — the hand-written version this replaces was
taken from three crates and was two verbs short. Each rule below matches one verb's exact shape,
and the `awk` tail is the point of the thing: a line no rule rewrote is printed as `UNCLASSIFIED`
and the pipeline exits non-zero, so a verb this repository has never seen announces itself instead
of being folded into the nearest bucket. A derivation that cannot detect a new class would be a
hand-written list wearing a script — which is what it replaces.

```bash
cargo mutants --list --no-config | sed -E '
  s/^.*:[0-9]+:[0-9]+: //
  s/^replace match guard .* with (true|false) in .*/replace match guard/
  s/^replace .* -> .* with .*/replace body/
  s/^replace .* with \(\)$/replace body/
  s/^replace [-+*\/%&|^<>=!]+ with [-+*\/%&|^<>=!]+( in .*)?$/replace operator/
  s/^delete (match arm|field|!|-).*/delete \1/
' | sort | uniq -c | sort -rn | awk '
  /(replace (operator|body|match guard)|delete (match arm|field|!|-))$/ { print; next }
  { print "UNCLASSIFIED: " $0 > "/dev/stderr"; bad = 1 }
  END { exit bad }'
```

Over the whole tree (25 215 mutants, September 2026) that is exactly seven verbs, and it exits 0:
16 082 binary or compound-assignment operator swaps, 7 212 function bodies replaced by a default
value, 917 whole match arms deleted, 540 unary `-` deletions, 269 `!` deletions, 160 match guards
forced to `true`/`false`, and 35 struct fields deleted from a struct literal. The operator rule
carries an optional ` in <fn>` tail because 110 of those swaps sit outside any function body —
`const`/`static` initialisers, and one array length in a signature — so the tool has no function
name to attach and prints the swap bare; the catch-all this replaces absorbed them silently.
Nothing else is generated, so the gate can never report:

- a case that is **missing** from a `match` or a table — there is nothing there to mutate, and a
  survey of nine absent spec-defined cases still scores zero missed;
- a wrong **literal or `const` inside an expression or a guard operand** — `>= 3` is never
  mutated into `>= 4`, and a guard's operands are not touched at all, so a test that asserts an
  error message against its own copy of the literal is self-referential and pins nothing. Assert
  the value, not the symbol. (A literal that *is* a function body is reachable, because bodies
  are replaced wholesale: `replace SrgbIntent::code -> u8 with 0` is that mutant.)
- one **alternative of an or-pattern** — `delete match arm A(v) | B(v)` removes both at once, so
  a wrong alternative stays green. Assert each alternative separately rather than looping.

**A green mutant can also mean the expression is unobservable.** A `Vec` capacity hint, a
discarded return value, arithmetic whose operands cannot disagree at the sizes the suite builds:
none of these can be killed by any test, because nothing depends on them. That is a reason to
delete the expression, not to test harder — and where deleting it costs something (an extra
allocation, say), record the cost where the code is.

**Verify a hand-applied mutant the way the tool does.** Commit first, so `git diff` shows exactly
the mutation and nothing else; apply the expression verbatim from the `--list` line; run the whole
package suite (`test_workspace = false` — the workspace suite is not what scored it); and re-read
the file to confirm the edit landed before believing the verdict. A test asserting only `is_err()`
cannot kill a guard-removal mutant when a later check rejects the same input for a different
reason — assert the message.

## Tight debug loops

When killing a specific survivor, narrow the selection and reuse the previous verdicts:

```bash
mise run mutants-crate gamut-png -- --iterate     # skip what was already caught
mise run mutants -- --file 'crates/gamut-png/src/filter.rs'
mise run mutants -- --crate gamut-png -- -F 'replace .* in apply_filter'
```

`--iterate` is what makes the loop tight: after the first pass, only survivors are retried, so
each edit costs one build of one package rather than the whole survey.

## CI

`.github/workflows/mutants.yml` has two jobs, both through `mise run mutants`:

- **incremental** — every PR, `--in-diff`, four round-robin shards, blocking. A surviving mutant
  in changed code fails the check. It passes `GAMUT_MUTANTS_BASE=origin/<the PR's base>`, not
  master: this repo stacks pull requests, and diffing a stacked branch against master would hand
  the gate every mutant the base branch introduced too.
- **full** — manual, whole workspace, `--all-at-once`, informational (`continue-on-error`).

Round-robin sharding rather than contiguous slices, because mutants from one file land in one
slice and a file with slow tests would then load a single shard while the others idle. Sharding
also exists because the runners are preemptible: a single 80-minute pass was being killed partway
through, and four twenty-minute shards finish inside the window.

## Survey record

Whole-crate surveys taken with `mise run mutants-crate <crate>` (cargo-mutants 27.1.0, all
features, AV1 oracles fetched unless the row says otherwise), one crate at a time. The counts
are the survey as taken at **Base**, *before* any survivor it found was resolved;
**Resolution** then says how each was resolved — a test that kills it, a rewrite that removes
it, or an argued entry in `.cargo/mutants.toml` — and, where the fix was re-scored, what the
touched files scored after it.
Base is the commit surveyed, or, for a row carried over from an earlier survey that did not pin
one, the issue that records it. A row is evidence about that base, not a standing claim —
re-survey before relying on it.

| Crate | Mutants | Caught | Unviable | Missed | Timeout | Wall | Base | Resolution |
| ----- | ------: | -----: | -------: | -----: | ------: | ---: | ---- | ---------- |
| gamut-riff | 165 | 157 | 8 | 0 | 0 | — | #575 | nothing to do |
| gamut-ifd | 652 | 587 | 57 | 0 | 8 | — | #575 | 7 loops bounded by their data, 1 excluded |
| gamut-heic | 848 | 732 | 116 | 0 | 0 | 28 min | `ef6c2e12` | nothing to do |
| gamut-tiff | 985 | 911 | 63 | 0 | 11 | 45 min | `ef6c2e12` | all 11 in PackBits/CCITT encoder loops advanced by their own arithmetic; each loop now walks its data (#585), and the G4 `reference_changes` exclusion went with them. The rewritten files then scored 283 mutants, 280 caught, 3 unviable — 38 fewer mutants than the 321 before, since the deleted loops took their killable siblings too |
| gamut-cmm | 1342 | 1269 | 71 | 0 | 2 | 47 min | `ef6c2e12` | both in `collapse`'s fixpoint loop, now bounded by the stage count, plus a test that needs the second sweep (#585). `optimize.rs` then scored 148 mutants, 137 caught, 11 unviable |
| gamut-avif | 1222 | 1056 | 163 | 1 | 2 | 2 h 24 min | `36951263` | `AvifImage::item`'s `==` → `!=` (TIMEOUT) made `sample_sequence_header`'s tail-recursive grid/iden walk resolve a tile back to its grid and spin; the walk is now bounded by `MAX_DERIVATION_DEPTH`, which the planar walk's depth guard already holds every accepted file to. `ObuIter::next`'s `pos *= end` (TIMEOUT) ended on its own arithmetic; the iterator now ends when its remaining slice does. `detail_line_count`'s `+` → `*` (missed) survived a 2/2/1 fixture where `2+2+1 = 2*2+1`; a 1/3/0 test kills it (#890). `decode.rs`, `obu.rs` and `c2pa.rs` then scored 592 mutants, 535 caught, 57 unviable (one fewer than the 593 before, as the `pos == data.len()` end test went), and `image.rs` 140, 108 caught, 32 unviable |
| gamut-dng | 2004 | 1898 | 99 | 1 | 6 | 5 h 32 min | `30129ece` | five TIMEOUTs were loops in `lossless_jpeg.rs` that ended on their own arithmetic — `canonical_codes`' `length += 1`, `parse_dht`'s `at += 17 + nvals` (a 17-symbol table advanced by `17 - 17`), and `decode`'s `FF`-fill `pos += 1` and DRI `pos += len`; `parse_dht` now walks its input slice, the fill skip is a search, and the marker walk takes at most `data.len()` passes, a bound no stream reaches (#891); `canonical_codes` has no site in this crate since #900 replaced it with `gamut-bitstream`'s `build_optimal_table` + `CanonicalCodes`. The sixth, `semantic_mask_info`'s `delete !`, was load, not a hang: the suite fails `semantic_masks_decode_with_typed_info` in 11 s, and a re-run of the function's 13 mutants caught all 13. `white_level_value`'s `>` → `>=` (missed) had no case at exactly `u32::MAX`; one now pins it. At `7cfd8321`, before the merge of #900, `lossless_jpeg.rs` and `encoder.rs` then scored 553 mutants, 540 caught, 13 unviable — 29 fewer than the 582 before, as the rewritten loops took their killable siblings with them; that figure includes the since-removed `canonical_codes` and was not re-run after the merge |
| gamut-webp | 3420 | 3315 | 103 | 2 | 0 | 2 h 57 min | `e8749bd8` | both missed were equivalent, in `vp8l/prefix.rs`, and both are removed (#903). `PrefixCode::from_code_lengths` sized its offset table `[0; MAX_CODE_LENGTH + 2]`, and the `+` → `*` twin only grew a tail nothing reads; the offsets are now a running sum mapped over `counts`, so no length expression is left. `write_description` wrote the simple code's `num_symbols - 1` bit as `symbols.len() - 1`, and since `simple_symbols` caps the length at 2 and `write_bits` keeps only the low bit, `len + 1` wrote the same bit; the bit is now `symbols.get(1).is_some()`, which also takes the killable `-` → `/` sibling with it. Run at 3 jobs (disk-bound, not memory-bound); no AV1 oracle is in this crate's graph, so they were not fetched. `prefix.rs` then scored 255 mutants, 243 caught, 12 unviable — 6 fewer than the 261 before, as the deleted offset loop took its caught siblings with it |
| gamut-av1 | 1465 of 5418 | 1409 | 50 | 0 | 6 | 26 h 29 min, stopped | `8aa920ac` | **partial**: one of two jobs died on `ENOSPC` at 47 min and the run went on at one job until stopped. Scored `cdf.rs` 106/106, `encoder.rs` 77/77, `decode/mod.rs` 57/58, `filter.rs` 1225/1263; `tile.rs` (3045) and seven smaller files were not reached; four of them are the next row, and the rest is #911. All six TIMEOUTs were genuine hangs in loops advanced by their own arithmetic (#908): `deblock`'s two passes (`<` → `>` on either bound spins a plane narrower than 4 samples — a hand run put 99.96 % of `perf` samples in `deblock` after 10 min — and `+= 4` → `*= 4` never moves a cursor that starts at 0) and `loop_restore_wiener_luma`'s stripe walk (`st <= height` and `st * stripe_h` both leave `se == st`). The passes now iterate `step_by(4)` ranges and the stripe walk its starts `0, 56, 120, …`. The two functions went from 194 mutants (188 caught, 6 TIMEOUT) to 169, as the deleted cursor expressions took 22 caught siblings with them and the new stripe end added 3. Re-scored at `a5bb0777` (`--file crates/gamut-av1/src/filter.rs -F 'in deblock$' -F 'in loop_restore_wiener_luma$'`, one job, 2 h): 169 caught |
| gamut-av1 | 396 of 5393 | 390 | 4 | 0 | 2 | 3 h 36 min | `aff1dbdc` | **partial**, continuing the row above at one job (`MUTANTS_JOBS=1`, disk-bound): `--file` `headers.rs` (160), `transform.rs` (113), `geom.rs` (89) and `quant.rs` (34). `tile.rs`, `decode/header.rs`, `decode/obu.rs`, `decode/tilegroup.rs` and the unreached `filter.rs`/`decode/mod.rs` mutants were #911; the next row scores all of them but `tile.rs` and half of `decode/header.rs`. Both TIMEOUTs were `TxSize::height` returning 0 (body → `0`, and `1 << log2` → `1 >> log2`, every `log2_height` being at least 2), which made `FrameEncoder::code_plane`'s residual walk advance its cursor by `sy += 0` and spin — the only cursor loop in the crate stepped by a transform size. It now walks `(base_y..base_y + rh).step_by(ph)` (and the same for `sx`), and `step_by(0)` panics. Re-scoring `code_plane` then found one survivor in a file this row did not survey: the `BlockDecoded` mark's `(0..16).contains(&by) && (0..16).contains(&bx)` guard, whose `&&` → `\|\|` was equivalent because a luma transform lies in its block and the block in its 64×64 superblock, so the guard held for every block (a temporary assertion held across the debug and release suites). The guard is gone and the mark is indexed in `usize`, where an underflow panics. `code_plane` went from 61 mutants to 50, as the deleted cursor and guard expressions took their killable siblings with them. Re-scored at `4be2cb43` (`--file crates/gamut-av1/src/tile.rs -F 'in FrameEncoder<.a>::code_plane$' -F 'FrameEncoder<.a>::code_plane ->'`, one job, 24 min): 50 caught; the three `TxSize::height` mutants were caught at `79b21f5e` |
| gamut-av1 | 393 of 5390 | 344 | 23 | 26 | 0 | 4 h 20 min | `3abdf28a` | **partial**, continuing the rows above at one job (`MUTANTS_JOBS=1`, disk-bound; about 42 s a mutant on average, since `cargo test` stops at the first failing test binary, so a caught mutant usually ends inside the unit tests while a survivor runs the whole 160 s suite). Two runs: `--file` `decode/obu.rs` (80: 62 caught, 5 unviable, 13 missed), `decode/tilegroup.rs` (41: 38 caught, 3 missed), `decode/mod.rs` (58: 46 caught, 12 unviable — re-scored whole, since the one mutant the `ENOSPC` worker lost cannot be named), and the 38 `filter.rs` mutants the first row's run never reached (all caught; it ran in list order and stopped in the file's tail, so they are its last 38: `superres_downscale_plane`, `superres_downscaled_width` and `superres_upscale_plane`'s last three, selected with `-F`); then `decode/header.rs --shard 1/2` (176: 160 caught, 6 unviable, 10 missed). `--shard` is zero-based, so shard 0/2 of `decode/header.rs` is left with `tile.rs` (#914–#922). All but two survivors were killable, and most sat in a §5.5/§5.9 branch neither encoder emits. In `obu.rs`, 13 in `SequenceHeader::parse_syntax`/`parse_color_config`: timing info, the decoder model's `buffer_delay_length`, a level above 7 coding a tier, profile 2's 10/12-bit layouts, profile 0 at high bit depth and an identity matrix outside the sRGB shortcut; a general-header test builder now codes each. In `tilegroup.rs`, `split_tiles`' little-endian size prefix (`<<`, `*`) was only ever a one-byte value, and its `<` → `<=` needed a complete size field with no tile after it, told apart by the error message. In `header.rs`, 8 killable: the explicit-spacing `maxTileAreaSb` for `minLog2Tiles == 0` (`>` → `>=`, `*` → `+`), monochrome `quantization_params`/`loop_filter_params`, `cdef_bits > 0`, the 128×128 `lr_unit_shift` increment and its `>>`, and 4:2:2 film grain without luma points; `StillBuilder` gained monochrome and 4:2:2 colours, `cdef_bits` and `context_update_tile_id` for them. The two equivalent sites are removed. `tile_info`'s `tile_cols == 0 \|\| tile_rows == 0` guard could not fire — `mi_cols`/`mi_rows` are at least 2, so each start loop pushes a start — and is deleted with its `\|\|` → `&&` and its killable siblings; `segmentation_params` spelled `temporal_update: false` beside `..SegmentationParams::disabled()`, which already holds it. Re-scored at `2d1dc2f4` (`--file crates/gamut-av1/src/decode/header.rs -F 'in FrameHeader::tile_info$' -F 'FrameHeader::tile_info ->' -F 'in FrameHeader::segmentation_params$' -F 'FrameHeader::segmentation_params ->'`, one job, 59 min — both halves of the file's shards): 118 mutants, 114 caught, 3 unviable, 1 missed. That one sat in shard 0/2, unscored above: the `context_update_tile_id >= tile_cols * tile_rows` bound (`*` → `+`) had no fixture naming a tile past the grid; one with three columns and id 3 now does |
| gamut-av1 | 114 of 5378 | 107 | 5 | 1 | 1 | 1 h 13 min | `e3482b6b` | **partial**, continuing the rows above at one job (`MUTANTS_JOBS=1`): `decode/header.rs --shard 0/2` (174 at this base, the file having shrunk to 348) less the 60 `tile_info`/`segmentation_params` mutants the row above already re-scored, selected by an anchored `-F` per remaining mutant name (an `-E` beside `--shard` re-deals the shard). cargo-mutants' `-F`/`-E` do not bind the two `delete field … from struct SegmentationParams` mutants, so the run also re-scored them (both caught; not counted here). The five unviable are the `Default::default()` bodies of types without a `Default`. The survivor was `cdef_params`' `num_planes > 1` (`>` → `>=`): every CDEF fixture had chroma; a monochrome frame with two strength sets now pins it. The TIMEOUT was `tile_log2`'s spec counting loop (`k += 1` → `k *= 1` leaves `k` at 0 and spins), the same loop the encoder's twin in `headers.rs` already replaced: it is now the closed form `target.div_ceil(blk_size).next_power_of_two().trailing_zeros()`, which took the loop's `<`/`<<`/`+=` siblings with it — `tile_log2` went from 8 mutants to 2 (the file to 342) — and its table test gained the `(64, 256)` and `(2304, 4608/4609)` cases the encoder's twin pins. Re-scored at `3adc872b` (`--file crates/gamut-av1/src/decode/header.rs -F 'in tile_log2$' -F 'tile_log2 ->' -F 'in FrameHeader::cdef_params$' -F 'FrameHeader::cdef_params ->'`, one job, 10 min): 9 mutants, 8 caught, 1 unviable. With the row above, `decode/header.rs` is scored whole; `tile.rs` is what is left (#915–#922), and the next row scores its shard 0/8 |
| gamut-av1 | 380 of 5372 | 368 | 5 | 7 | 0 | 3 h 1 min | `7af44444` | **partial**, continuing the rows above at one job (`MUTANTS_JOBS=1`): `--file crates/gamut-av1/src/tile.rs --shard 0/8`, the file's list entries 1, 9, 17, … of 3034. One survivor was killable: `clear_block_decoded`'s `x < sb_width4` (`<` → `<=`), since every fixture's superblock was 16 MI wide and nothing read the column past a tile edge; a test now cuts the tile inside the superblock. Six were equivalent and are removed. `encode_tile`'s `qindex > 0` guard before `write_lr` is gone, because a lossless frame never carries restoration units. `signal_delta_lf`'s sign bit is the literal 0, because its `delta` is `% 2` of a non-negative sum. `restore_plane`'s `w > 0` guard is gone, because `w` is at least 4 when a block's top-left lies in the frame. `predict_directional`'s branches run in angle order (zone 1, 90, zone 2, 180, zone 3), so a `<` → `<=` on a zone bound now indexes `Dr_Intra_Derivative[90]` and panics. `luma_mode_rate`'s copy of the `has_palette_y` contexts (`r - 1` → `r / 1`) is gone: it and `encode_block` now share `palette_y_ctx`. `walk_coeffs`' square-scan arms, whose deletions fell through to an equal `default_scan`, are replaced by one `SCANS` table built from `default_scan`, and the four hand-transcribed square tables are now `#[cfg(test)]`, kept as that generator's oracle. The palette-branch `(0..16).contains` guard #911 flagged is list entry 1120, which shard 7/8 holds. Re-scored at `cfdea7e8` (`--file crates/gamut-av1/src/tile.rs -F 'FrameEncoder<.a>::(clear_block_decoded\|encode_tile\|signal_delta_lf\|restore_plane\|predict_directional\|luma_mode_rate\|palette_y_ctx\|walk_coeffs)( \|$)'`, one job, 2 h 48 min): 356 mutants, 347 caught, 9 missed. The run covered these functions' mutants in all eight shards, and the nine survivors came from the other seven. Six were killable and now have tests: `clear_block_decoded`'s `tile_r1 - r` (`-` → `+`, which every fixture's `r = 0` hid) and the three operators of its `[sbSize4][-1]` index (`*` → `+`/`/`, `+` → `*`), plus `luma_mode_rate`'s `bw <= 32 && bh <= 32` (`&&` → `\|\|`, seen at 64×32) and its `16 =>` filter-intra arm. Three were equivalent and are removed. `walk_coeffs`' eob extra subtracted `1 << nbits`, a bit above the `nbits` it codes, so `<<` → `>>` moved nothing; it now codes the low bits of `eob - 1`. The AC sign's `quant[pos] < 0` sits under `level != 0` and is now `is_negative()`. `dcCategory`'s `< 0` overlapped its `== 0` branch and is now one `cmp`. The removals cost mutants as well as survivors. `tile.rs` went from 3034 to 2987: `encode_block` went 194 → 162 and `luma_mode_rate` 55 → 23, against `palette_y_ctx`'s new 36; `encode_tile` went 10 → 7, `restore_plane` 16 → 13, `signal_delta_lf` 18 → 15 and `walk_coeffs` 82 → 70. So round-robin `--shard k/8` deals a different list after this row than at its base. Re-scored again at `455172ce` (`-F 'FrameEncoder<.a>::(clear_block_decoded\|luma_mode_rate\|walk_coeffs)( \|$)'`, one job, 52 min): 138 caught |
| gamut-av1 | 342 of 5325 | 336 | 3 | 3 | 0 | 2 h 27 min | `48f43f5c` | **partial**, continuing the rows above at one job (`MUTANTS_JOBS=1`): `tile.rs` shard 1/8 **as dealt at `7af44444`** — list entries 2, 10, 18, … of 3034, the partition #916–#922 were cut from — since the row above removed 47 mutants and `--shard 1/8` on its own base deals a different list. Of those 380, 11 sit on lines the row above deleted or rewrote (`encode_block`'s and `luma_mode_rate`'s inlined `has_palette_y` contexts, `walk_coeffs`' `(32, 32)` arm, eob extra and AC sign) and need no score; 37 lie in the eight functions the row above re-scored whole at this code (`clear_block_decoded`, `encode_tile`, `signal_delta_lf`, `restore_plane`, `predict_directional`, `luma_mode_rate`, `palette_y_ctx`, `walk_coeffs`) and are not run again. The other 332 were mapped to their lines at `48f43f5c` and run by one anchored `-F` per full mutant name (`file:line:col: …`); `-F` does not bind `delete field` mutants, so ten more from other shards came along (all caught, counted here). The three unviable are two `Default::default()` bodies and `RATE_ONE`'s `1 << 8` (`<<` → `>>`). Two survivors were killable. `CoeffCdf::probs`' `1 =>` arm (`eob_pt_32`) could be deleted into the `_` fallback (`eob_pt_1024`), since `probs` only prices a trial; a test now checks every `eob_pt` class prices against the CDF `slot` codes, each class having its own alphabet size. `txb_skip_ctx`'s luma column guard `x4 + k` (`+` → `*`) had a test meant for it, whose window held a second level above 3 that stood in for the cell the mutant skips; the fixture now holds one. The third, `palette_cache`'s spec step of `leftIdx` on equal colours (`+=` → `*=`), was equivalent, as the repeat check drops the left colour on the next step. Re-scored at `b71b8d63` (`-F 'FrameEncoder<.a>::(palette_cache\|txb_skip_ctx)( \|$)' -F 'CoeffCdf::probs( \|$)'`, one job, 3 h 9 min — every shard's mutants in these functions): 158 mutants, 151 caught, 3 missed, 4 TIMEOUT. `txb_skip_ctx`'s `block_w == tx_w && block_h == tx_h` (`&&` → `\|\|`) needed a luma transform matching its block in one axis only, and its column guard's `<` → `<=` a window overhanging the grid; both are now asserted. `palette_cache`'s other survivor (`lc < ac` → `<=`, again absorbed by the repeat check) and four TIMEOUTs (`+= 1` → `*= 1` on each of its merge cursors, which start at 0 and then never move) went with its two-cursor merge, which is now the sorted union of the two palettes: every palette the encoder stores ascends strictly (`decide_palette` inserts distinct samples in order), and for ascending lists the spec's merge is that union. `palette_cache` went from 65 mutants to 19, and `tile.rs` from 2987 to 2941, so the survey is smaller there, not stronger. Re-scored again at `b076c398` (same filters, one job, 1 h 7 min): 115 caught |
| gamut-av1 | 325 of 5279 | 315 | 6 | 4 | 0 | 2 h 28 min | `d67319f3` | **partial**, continuing the rows above at one job (`MUTANTS_JOBS=1`): `tile.rs` shard 2/8 **as dealt at `7af44444`**, i.e. list entries 3, 11, 19, … of 3034 (379 mutants). The two rows above removed 93 mutants, so `--shard 2/8` on this base deals a different list. Of the 379, 17 sit on lines those rows deleted or rewrote and need no score: `palette_cache`'s two-cursor merge (8), `encode_block`'s and `luma_mode_rate`'s inlined `has_palette_y` contexts (4 + 4), and `walk_coeffs`' eob extra (1). Another 47 lie in functions those rows re-scored whole at this code and were not run again: `predict_directional` 17, `txb_skip_ctx` 9, `walk_coeffs` 8, `clear_block_decoded` 4, `luma_mode_rate` 3, `restore_plane` 2, `signal_delta_lf` 2, `encode_tile` 1 and `CoeffCdf::probs` 1. The other 315 were mapped to their lines at `d67319f3` and run with one anchored `-F` per full name. Ten `delete field` mutants from other shards came along, all caught and counted here. The six unviable are three `Default::default()` bodies, `dc_pred`'s `>>` → `<<`, and two operators of the compile-time `LOG2_FRAC` builder (`>` → `>=` and `>>=` → `<<=`). Two survivors were killable. `write_lr`'s unit-row start `(r * 4).div_ceil(UNIT)` (`*` → `/`) differs only from a superblock row past sample 256 inside a second unit row, and every LR fixture was at most 200 rows high; a 64×640 frame now runs through both oracles. `reference_4x4`'s no-below-left limit `… - 1` (`-` → `+`) was tested only at the frame edge, where `max_y` clamps it first; a unit test now reads it at `(4, 4)` of a 16×16 plane. Two were equivalent and are removed. `encode_block`'s `qindex > 0` guard before `apply_seg_quant` is gone: a lossless frame codes segment 0 and no `delta_q`, so the call sets the steps the frame already has. `reconstruct`'s 64×64 re-expansion tested `tw == 64 && th == 64`, and `\|\|` was equal on every transform the encoder forms, since TX_64X64 is the only one with a 64-sample side (luma rectangles stop at 32, edge blocks split, chroma is capped at 32). It now tests `tw == 64`, the test `quantize_tx` compacts by. The removals cost five mutants: `encode_block` went 162 → 159, `reconstruct` 20 → 18, and `tile.rs` 2941 → 2936. Re-scored at `58521be2` (`-F 'FrameEncoder<.a>::(encode_block\|reconstruct\|reference_4x4\|write_lr)( \|$)'`, one job, 3 h 23 min, every shard's mutants in these functions): 301 mutants, 292 caught, 9 missed. Five were killable and now have tests. `write_lr`'s unit index `row * lr.cols + col` (`*` → `/`, shard 4/8) needed a grid two units each way whose rows decide differently. The four operators of `reference_4x4`'s `haveBelowLft` position `(by + 1, bx - 1)` (shards 4/8 to 7/8) are now covered by a test that marks the left, above-left and below-left 4×4s in turn. Four are left to the shards that deal them. `encode_block`'s `(_, false, _) \| (_, _, true) => None` CfL arm (delete, shard 3/8, #918): for a block with no chroma the CfL search result is never coded, and for a skip block coding CfL is a choice the format leaves free; it still needs a verdict. The palette mark's `(0..16).contains` guard and its two index operators (`- self.sb_r`, `(by + 1) * BD_STRIDE + …`; shard 7/8, #922). Re-scored again at `2e49a376` (`-F 'FrameEncoder<.a>::(reference_4x4\|write_lr)( \|$)'`, one job, 1 h 28 min): 124 caught |
