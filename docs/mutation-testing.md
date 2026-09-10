# Mutation testing

Normative for **how a mutation survey is invoked and what bounds it**. What counts as an
acceptable survivor or a justified exclusion is `AGENTS.md`'s rule and
`.cargo/mutants.toml`'s prose; this document is about running the thing without taking the
machine down.

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
- a per-process `ulimit -v`, which is what makes a runaway mutant abort itself first, so it is
  scored `caught` and the run continues instead of the cgroup killing cargo-mutants
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
caught. Treat a hang the way `AGENTS.md` says to treat a survivor — the loop that cannot make
progress under mutation is usually a loop that should have been bounded by a slice in the first
place, and rewriting it as one turns an unkillable `TIMEOUT` into an ordinary killable mutant.

**What the tool mutates is a short list**, and everything outside it is invisible to the gate.
`cargo mutants --list` on this workspace generates exactly: a function body replaced by a default
value, a binary or compound-assignment operator swapped, a `!` deleted, a match *guard* forced to
`true`/`false`, and a whole match arm deleted. So the gate can never report:

- a case that is **missing** from a `match` or a table — there is nothing there to mutate, and a
  survey of nine absent spec-defined cases still scores zero missed;
- a wrong **literal or `const`** — a guard's operand is not mutated, so a test that asserts an
  error message against its own copy of the literal is self-referential and pins nothing. Assert
  the value, not the symbol;
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
