# Benchmarking

Normative for **where a benchmark lives, what a size or ratio table must record, and where a
measured number is kept**.

Not normative for: whether a size claim is *enforced* — that is a test, and
[`testing.md`](testing.md) places it (the "size / effort contract" row of its technique table).
Nor for the prose a crate uses to describe its own performance, which is that crate's `README.md`.

> **A benchmark reports. A test asserts. Only the test can fail a build.**

## Where a benchmark lives

One file per crate under `crates/<crate>/benches/`, named for the thing measured (`codec.rs`,
`compression.rs`, `encode.rs`, `pipeline.rs`), declared with `harness = false` and
`divan.workspace = true` in `[dev-dependencies]`. Sixteen crates ship one.

```toml
[dev-dependencies]
divan.workspace = true

[[bench]]
name = "encode"
harness = false
```

Do **not** use `required-features`. `mise run bench` is `cargo bench --workspace` with no features,
so a bench behind a required feature silently never runs. Gate the feature-dependent *benchmarks*
inside the file instead, and say so in the module doc.

## What it must state

Every bench opens with a module doc that names the subject, the issue, what the counter unit means,
and how to run it. The house phrase is "Intentionally tight:", introducing why *these* axes and not
others.

Counter units are fixed by kind, so figures are comparable across suites:

| kind | counter | over |
| --- | --- | --- |
| codec encode/decode | `BytesCount` | **source pixel** bytes |
| compressor | `BytesCount` | input bytes |
| container / parser | `BytesCount` | payload bytes |
| byte-oriented codec pipeline stage | `BytesCount` | bytes the stage consumes |
| per-pixel or per-sample kernel *whose item is not a byte* | `ItemsCount` | items |
| one-off construction cost | none | — |

The last two rows split on what the kernel's natural unit actually is, because the counter is what
makes a figure comparable and a figure is only comparable to figures in the same unit. A stage
inside a codec pipeline — CRC, scanline packing, filtering, a colour-type scan — consumes the
byte stream the enclosing encoder consumes, so counting its bytes puts it in the same unit as the
crate's own encode benchmark and its size table, and a per-stage figure can be read against the
whole. `gamut-png`'s stage benches are all of this kind. `ItemsCount` is for a kernel whose item is
*not* a byte and would be lost by counting bytes: `gamut-dsp` counts transform coefficients,
`gamut-tonemap` `f32` samples, `gamut-color` `f64` samples and pixels, `gamut-bitstream` coded
symbols, `gamut-cmm` transformed pixels. Bytes per second would say nothing about any of those.

Fixtures are **generated, never vendored**, and each generator documents the one axis it exists
for. Size them against the algorithm, not for speed: `gamut-png`'s corpus is 256×256 because RGB at
that size is ~6× the DEFLATE window, and a 64×64 image fits *inside* it and would flatter every
encoder equally.

## Size and ratio tables

A crate whose reason to exist is output size prints a table before `divan::main()`:

```rust
fn main() {
    print_size_table();
    divan::main();
}
```

The table names its baseline, marks the direction ("lower is better"), and carries a percentage
delta column against that baseline. Where the crate has an oracle, the baseline is the oracle at
its strongest setting — `zlib -9` for `gamut-deflate`, libpng at compression level 9 for
`gamut-png` — configured to do the *same job* on the *same input*. Handing the baseline an
optimisation that is the crate's own contribution does not measure anything.

Prefer deriving the columns from a reader that works on **any** file rather than from the
encoder's own bookkeeping. `gamut-png`'s table goes through `gamut_png::deconstruct`, which is what
makes its libpng column a measurement rather than two encoders' self-reports.

## Where a measured number is kept

In the crate's **`STATUS.md`**, which `docs/README.md` makes normative for implemented state.
Record the invocation, the fixture size, and the caveat that one machine means the ratios are the
result — `gamut-cmm/STATUS.md` and `gamut-png/STATUS.md` are the models. A number in a `README.md`
is a summary of that table, never the source.

Record negative results too. A heuristic that did not beat the one it was meant to replace is a
finding, and re-deriving it later costs more than writing it down.

## What CI does

- **Every PR**: `mise run lint` is `cargo clippy --workspace --all-targets --all-features`, and
  `--all-targets` includes benches. They compile, so they cannot rot silently.
- **Extended lane**: `mise run bench-test` is `cargo bench --workspace --benches -- --test`, which
  runs every benchmark **once** to prove it still executes — a bench that compiles and then panics
  in setup used to be invisible. It takes no timings and asserts no thresholds (issue #437).

So a benchmark is compiled and executed by CI, and its *numbers* are not gated. Whether they should
be is open, for the reason #437 records: the numbers would come from preemptible shared runners.
Until then, a claim that must not regress belongs in a test — see [`testing.md`](testing.md).

## Running one

```bash
mise run bench                  # the whole workspace
mise run bench-test             # run each once, no timings (what Extended does)
cargo bench -p gamut-png        # one crate
cargo bench -p gamut-png --bench encode -- --sample-count 50
```

Divan flags must target one harness directly: the per-crate libtest stubs reject them, so
`cargo bench -p <crate> --bench <name> -- <flags>` is the form that works.

## Reaching a crate's internals

A `benches/` target compiles as a separate crate and sees only `pub` items, which most pipeline
stages are not. The convention is a `test-support` feature exposing a `#[doc(hidden)]` module of
**re-exports only** — no wrapper bodies, which would be executable lines no gate ever runs (bench
targets carry `test = false`) and so would both drag the coverage floor and generate unkillable
mutants. `gamut_png::stages` is the model; the feature is never enabled by the `gamut` umbrella, so
the shipped surface and `mise run check-ffi-features` are unaffected.

[#437]: https://github.com/visualcommons/gamut/issues/437
[#149]: https://github.com/visualcommons/gamut/issues/149
