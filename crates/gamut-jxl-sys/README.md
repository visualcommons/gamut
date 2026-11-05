# gamut-jxl-sys

Low-level `-sys` crate for the [gamut](../../README.md) workspace: it statically builds the
reference **libjxl v0.12.0** and exposes hand-written FFI declarations for the subset of its C API
that [`gamut-jxl`](../gamut-jxl) needs. This is the native foundation of gamut-jxl's JPEG XL
**encoder** (issue [#243](https://github.com/visualcommons/gamut/issues/243)).

## What & why

Unlike the rest of gamut, JPEG XL encoding is a deliberate, maintainer-confirmed departure from the
"shipped crates are pure Rust" rule: libjxl is the ISO/IEC 18181 reference implementation and the
only reference-quality JXL encoder available to Rust, and `jxl-rs` ships no encoder. Licensing rules
out the existing `jpegxl-sys`/`jpegxl-rs` crates (GPL-3.0), so this crate provides its own bindings
on top of the BSD-3-Clause [`jpegxl-src`](https://crates.io/crates/jpegxl-src) distribution vehicle.

The crate is **declarations only** — `#[repr(C)]` types, constants, and `unsafe extern "C"` function
signatures, with no function bodies. All safe wrapper logic, error mapping, and RAII live in
`gamut-jxl`. A small **decoder** subset is also declared: it is used only as gamut-jxl's
differential-test *oracle* (the reference decoder its pure-Rust decoder is cross-checked against).
The single static libjxl archive contains both halves, and the linker strips whatever is unused.

Bindings are transcribed by hand from the pinned libjxl v0.12.0 headers and pruned to the used
subset. The pin is exact (`jpegxl-src = "=0.12.0"`, which vendors libjxl v0.12.0): a version bump
would move the headers out of lockstep with the transcription. The `tests/version.rs` drift guard
asserts `JxlEncoderVersion()`/`JxlDecoderVersion()` both report `12000` (= 0·1000000 + 12·1000 + 0).

## Building without a C toolchain

The first build downloads `jpegxl-src` and cmake-builds libjxl statically under `OUT_DIR` (~3-6 min
cold; cmake + a C++ toolchain required). `cargo clean` fully resets it. Build tools (cmake/ninja)
come from [mise](https://mise.jdx.dev).

Set **`GAMUT_JXL_SYS_SKIP_NATIVE=1`** to skip the native build and emit no link directives. This is a
`cargo check`-only escape hatch — checking compiles but never *links*, so the absent native library
is never referenced — used by the workspace's cross-compile (`check-cross`) and MSRV (`check-msrv`)
verification, which run on boxes without cmake or a cross C++ toolchain. Do **not** set it for builds
that actually link (tests, binaries).

## WebAssembly

libjxl officially supports WebAssembly builds through [emscripten](https://emscripten.org), and so
does this crate — but only for the Rust target that shares emscripten's toolchain:

- **`wasm32-unknown-emscripten`** — fully supported. `build.rs` drives the same vendored libjxl
  source through `emcmake`/`emmake` (the `cmake` crate does this automatically for `*-emscripten`
  targets) with `JPEGXL_ENABLE_WASM_THREADS=OFF`, and links `jxl`/`jxl_cms`/`hwy`/`brotli*`
  statically. Requires an installed and activated emsdk (`emcc` on `PATH`); rustc performs the
  final link through `emcc`, which supplies libc++. `GAMUT_JXL_SYS_LIBJXL_DIR` overrides where the
  vendored libjxl tree is found (useful for `cargo vendor` layouts). Two link-time notes for
  consumers: use a **recent emsdk** (rustc's CI builds this target's std with `emsdk install
  latest`; old emsdks fail at link — 3.1.68's wasm-opt predates flags rustc 1.97 emits), and link
  final binaries with **`-C link-arg=-sALLOW_MEMORY_GROWTH=1`** — rustc's default emscripten link
  uses a fixed 16 MiB heap with `-sABORTING_MALLOC=0`, so large libjxl allocations (e.g. brotli in
  the JPEG-recompression path) would otherwise get a NULL malloc and kill the process.
- **`wasm32-unknown-unknown`** (the wasm-bindgen/browser target) — **not possible**: no C/C++
  toolchain emits archives for that ABI (no C runtime, no libc++), so there is nothing a build
  system could do differently. The build skips the native step unconditionally there, and
  `gamut-jxl` compiles its encoder out (decode stays available via pure-Rust jxl-rs).
- **`wasm32-wasip1/p2`** — out of scope. A WASI-SDK-built libjxl could plausibly link, but that is
  a third toolchain to pin with no consumer in the workspace; revisit if one appears.

## `links = "jxl"` uniqueness

This crate declares `links = "jxl"`, so Cargo links the native library exactly once and rejects a
second crate claiming the same native lib. gamut-jxl-sys is the sole native-jxl provider in the
workspace. Note the name **collides** with the third-party `jpegxl-sys` crate (which also declares
`links = "jxl"`): a downstream build that pulls in both will fail to resolve. This is intentional —
only one libjxl may be linked into a program.

## Licensing

This crate's own source is licensed under **MIT OR Apache-2.0** (workspace default). Building it,
however, statically links libjxl and its bundled third-party libraries, each under its own license:

- **libjxl** — BSD-3-Clause
- **highway** (bundled) — Apache-2.0
- **brotli** (bundled) — MIT
- **skcms** (bundled) — BSD-3-Clause

The libjxl source is distributed and built by the BSD-3-Clause `jpegxl-src` crate. The only
system-provided library linked is the platform C++ runtime (`libstdc++` / `libc++`). Redistributing a
binary that links this crate therefore requires honoring the above notices.

> **Upstream note.** `jpegxl-src` 0.12.0 carries a leftover GPL-3.0 header comment in its
> `src/lib.rs`, which contradicts its declared BSD-3-Clause `SPDX` identifier and bundled `LICENSE`
> file. This appears to be an editing oversight (the crate is otherwise BSD-3-Clause); it has been
> flagged upstream. We rely on the declared SPDX + LICENSE (BSD-3-Clause).
