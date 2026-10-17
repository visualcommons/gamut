# Experiment #480: filter-selection techniques gamut-png lacked

**Claim under test.** oxipng ships filter-selection techniques that `gamut-png` did not have. The
issue names four left to measure, after #485 landed Entropy and Bigrams: **BigEnt**, **per-line
trial deflate**, **two-tier trials** and **`AtomicMin` pruning**. The claim is that each makes
`FilterStrategy::BruteForce` output smaller, or makes it cheaper at the same size, on gamut's own
corpus.

**Acceptance rule** (maintainer decision on #480). A heuristic that does not beat what ships is a
negative result in `STATUS.md`, not a merge. A technique ships only if the measurements support it.

## Verdicts

| technique | verdict | evidence |
| --- | --- | --- |
| Per-line trial deflate | **ships** as `FilterStrategy::MinTrialDeflate`, last in the `BruteForce` set | It is the unique winner on `gradient_rgb8` (1 503 → **1 188**, −21.0%) and `sprite_rgba8` (3 641 → **3 455**, −5.1%). Every other row is byte-identical. |
| BigEnt | **negative** | It is never strictly smaller than the seven candidates `BruteForce` already had, on any row. |
| Two-tier trials | **negative** for `BruteForce` | Picking the winner at a cheap level and encoding only that one at `Best` is 6.4x to 6.7x faster over the corpus. It also loses bytes on `grey_as_rgb8` (475 → 506, +6.5%) and `flat_rgba8` (573 → 606, +5.8%), at every tier and with the top one or top two kept. `BruteForce` promises the smallest of its candidates, and this breaks that promise. |
| `AtomicMin` pruning | **negative today** | It cannot prune anything: `gamut-deflate` finishes the whole parse before it emits a byte, so a trial cannot learn it is over budget until its work is done. Even if a budget hook let a losing trial stop the moment it passed the best so far, the linear model puts the ceiling at **10.2%** of brute-force time. Two-tier trials already show a far larger lever. |

## What was measured

Everything is IDAT (zlib stream) bytes on the **unreduced** samples at `Level::Best`, with the
default effort and optimal-parse limit that `PngEncoder` uses. This is the basis of `STATUS.md`'s
per-heuristic table. The corpus is the ten generators in `crates/gamut-png/tests/common/corpus.rs`:
the eight rows of `benches/encode.rs`, plus `opaque256_rgba8` and `demotable_rgb16` from the size
contract. All are 256×256 except the 16×16 `tiny_rgb8`.

The arm is `crates/gamut-png/examples/experiment_480.rs`. It patches nothing. Every variant is built
from the public `stages::filter_image` and `gamut_deflate::DeflateEncoder`, and the arm checks its
own reconstructions against the crate before it records anything:

- **Its per-row splice reproduces the shipped `MinSumAbs` byte for byte.** BigEnt and the trial
  sweep pick, row by row, among the five `Fixed` images. A filter reads only the unfiltered rows,
  so that splice is exact.
- **Its trial at the shipped settings reproduces `stages::filter_image(MinTrialDeflate)`.**
- **The smallest of its eight candidate streams equals the IDAT of the shipped `BruteForce`,
  encoded end to end.**

### IDAT bytes, per candidate and per set

`brute7` is the set before this change: `None`, the four `Fixed`, `MinSumAbs` and `MinBigrams`.
`bruteforce` is the shipped set, which adds `MinTrialDeflate`. Bold marks a candidate strictly
smaller than `brute7`.

| input | MinSumAbs | Bigrams | Entropy | BigEnt | Trial | `brute7` | `bruteforce` | change |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `gradient_rgb8` | 2 203 | 1 503 | 2 203 | 2 051 | **1 188** | 1 503 | **1 188** | **−21.0%** |
| `photo_rgb8` | 25 234 | 19 351 | 22 294 | 19 351 | 21 805 | 19 351 | 19 351 | 0 |
| `noise_rgb8` | 196 890 | 196 890 | 196 890 | 196 890 | 196 890 | 196 890 | 196 890 | 0 |
| `grey_as_rgb8` | 475 | 506 | 506 | 506 | 506 | 475 | 475 | 0 |
| `palette64_rgba8` | 984 | 770 | 898 | 898 | 770 | 770 | 770 | 0 |
| `sprite_rgba8` | 3 641 | 4 026 | 3 807 | 4 327 | **3 455** | 3 641 | **3 455** | **−5.1%** |
| `opaque256_rgba8` | 923 | 735 | 905 | 923 | 735 | 735 | 735 | 0 |
| `demotable_rgb16` | 1 407 | 920 | 1 411 | 1 411 | 920 | 920 | 920 | 0 |
| `flat_rgba8` | 573 | 605 | 573 | 573 | 606 | 573 | 573 | 0 |
| `tiny_rgb8` | 79 | 62 | 79 | 76 | 63 | 62 | 62 | 0 |

`Trial` is the shipped `MinTrialDeflate`. The five `None`/`Fixed` candidates are in
`raw/measurements.csv`. Over the whole corpus the shipped set saves 501 bytes, all of them on the two
rows above.

BigEnt is never below `brute7`. It ties Bigrams on `photo_rgb8` and MinSumAbs on `flat_rgba8`, and
it is larger than `brute7` on the other seven compressible rows, by 6.5% (`grey_as_rgb8`) to 53%
(`demotable_rgb16`). Like Entropy, it would also score in floating point, which keeps it out of the
integer-only brute-force set on reproducibility grounds alone.

### Choosing the trial's settings

The sweep covers 2, 4, 8, 16 and 32 context rows, each at `Level::Fast` and `Level::Default`. The
trial only matters on rows where it can win, `gradient_rgb8` and `sprite_rgba8`:

| context rows | `gradient_rgb8` Fast | Default | `sprite_rgba8` Fast | Default | `noise_rgb8` ms, Default |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 2 | 1 314 | 1 270 | 3 541 | 3 587 | 92.5 |
| 4 | 1 332 | 1 341 | 3 528 | 3 473 | 114.8 |
| 8 | 1 298 | 1 220 | 3 533 | 3 453 | 170.8 |
| **16** | 1 248 | **1 188** | 3 539 | **3 455** | **293.3** |
| 32 | 1 210 | 1 183 | 3 517 | 3 443 | 624.7 |

On every other row, every setting gives the same bytes, apart from `photo_rgb8` and `tiny_rgb8`
at a few settings. None of those beats `brute7` there, so none reaches a file. The last column is
the median time for the whole candidate on `noise_rgb8`. Its `Level::Best` pass is about 25 ms
because the input is incompressible, so that row is the cleanest view of what the trial selection
itself costs.

At `Default`, more context buys bytes up to sixteen rows. Thirty-two buys 5 and 12 bytes more
(0.4%) at about twice the trial cost, so **sixteen at `Default`** ships. `Fast`, whose greedy parse
is the only difference from `Default`'s lazy one, loses to `Default` on both rows at every depth
from eight rows up. Below that the two levels trade wins.

### Time, summed over the corpus

These are medians of three samples per input, summed over the ten inputs. The machine was shared,
so read the ratios only.

| variant | ms | vs `bruteforce` | bytes lost vs `bruteforce` |
| --- | ---: | ---: | --- |
| `brute7`, the old set (sum of its candidates) | 68 206 | 0.85x | +501 (no trial) |
| `bruteforce`, the shipped set, end to end | 79 840 | 1x | — |
| `atomicmin-model` (ceiling, modelled) | 71 157 | 0.89x | 0 |
| `twotier-default-top1` | 11 836 | 0.15x | +64 (`grey_as` +31, `flat` +33) |
| `twotier-fast-top1` | 12 541 | 0.16x | +64 |
| `twotier-fast-top2` | 23 240 | 0.29x | +64 |
| `twotier-default-top2` | 22 971 | 0.29x | +64 |

**The eighth candidate costs about 17% more brute-force time** (68.2 s → 79.8 s). It buys 21.0% and
5.1% on the two rows it wins and nothing elsewhere.

**Two-tier is the large time lever, but it is lossy against `BruteForce`'s own promise.** Over all
eight candidates, the cheap tier mis-ranks them on two rows. On `grey_as_rgb8` and `flat_rgba8`,
`MinSumAbs` wins at `Level::Best` by 31 and 33 bytes, but at the cheap level it is not among the
candidates kept. Keeping the top two does not recover either row. The arm does not record the
cheap-tier sizes themselves, only which candidate `Best` then finishes. A rung that trades
6% on some images for 6x the speed is a legitimate product choice. It is not `BruteForce`, though,
and it is not what this issue's acceptance rule measures, so it is recorded here and not shipped.

**`AtomicMin`'s ceiling is about 10%** of the sum of the eight candidates (79.2 s → 71.2 s, per
input from 0.5% on `noise_rgb8` to 17.0% on `demotable_rgb16`). Reaching it needs `gamut-deflate`
to stop a parse once its output is known to exceed a budget, which `DeflateEncoder` cannot do: its
`deflate_body` runs the whole parse before it encodes any block. A budget check could only run
between parse spans or during block encoding, after most of the work, so the real saving at
`Level::Best` would be below this linear-model ceiling. It is a smaller lever than two-tier and a
cross-crate change, so it is not filed as its own work here.

## Limits

- **The corpus is synthetic.** It is the crate's own generators, because there is no vendored image
  corpus (`crates/gamut-png/README.md`). The trial wins on two of its ten rows. How often it wins
  on real images is not something this corpus can show.
- **The bytes are measured on unreduced samples.** The finished-file effect at `Level::Best` +
  `BruteForce` + auto-reduce is in `STATUS.md`'s size table, regenerated at this head:
  `gradient_rgb8` goes from 1 562 to 1 245 bytes and `sprite_rgba8` from 3 729 to 3 512.
- **The times were taken on a shared, loaded machine.** `run.json` records the load average, and
  the medians are of three samples. The verdicts rest on the byte counts, which are deterministic.
  Times are read only as ratios between variants measured in the same pass.
- **`AtomicMin` is modelled, not measured** (`timing` = `model` in the CSV). See its row above.

## Files

- `run.sh` is the whole procedure. It refuses to record if HEAD moved during the run.
- `raw/measurements.csv` has one row per input × variant × sample (10 inputs × 27 variants × 3
  samples). The columns are documented in the measurement arm's module doc.
- `run.json` records provenance: the git SHA that was built, rustc, the `Cargo.lock` and
  `rust-toolchain.toml` hashes (the repository has no `mise.lock`, so that is `null`), the SHA-256
  of every input's raw samples, the CPU, and the load average at start and end. The inputs are
  deterministic generators, so there are no seeds.
