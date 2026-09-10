# gamut

A collection of space-efficient image encoding libraries, organized as a Cargo workspace
under `crates/`.

## Workspace layout

The umbrella crate `gamut` re-exports format crates behind Cargo features; everything builds
on shared primitives.

gamut is **image-first**: codec crates named after video formats (`av1`/`av2`/`vvc`,
HEVC-based `heic`, VP8-lineage `webp`) cover only the intra-frame still-image subset — no
inter-frame/motion/sequence coding. Encoder-first; decoders only where the Rust ecosystem
lacks a strong, feature-complete implementation.

Dependency edges (a crate depends on those to its right):

- **gamut** — umbrella; optional deps on format crates gated by features (`avif`, `jxl`,
  `webp`, `heic`, `vvc`, `av1`, `av2`, `tiff`, `dng`, `png`, `jpeg`, `isobmff`, `metadata`,
  `cmm`, `codec-abi`, `all`); `default = []`. `primitives` re-exports shared `color`/`dsp`/`bitstream`;
  `isobmff`/`metadata`/`tonemap`/`codec-abi` re-export their respective primitive crates;
  `all` includes all of these.
- **gamut-core** — `Encoder`/`Decoder` traits, image buffers, `Dimensions`, `Error`, plus the
  format-agnostic `convert` module: the one place any `Pixel` layout converts to another
  (grey↔RGB, alpha add/drop/composite, 8↔16-bit), lossless by default with loss opted into per
  decoder via a `ConvertPolicy`. Format crates decode to what the file carries and delegate the
  layout change there rather than hand-rolling it. No internal deps; everything else depends on it.
- **gamut-color** / **gamut-dsp** / **gamut-bitstream** — shared primitives. ← core.
- **gamut-tonemap** — scalar tone-mapping curves (`ToneCurve` + Reinhard/ACES/Hable/Drago)
  for HDR→SDR pipelines, between `gamut-color`'s transfer functions and the SDR re-encode.
  ← core.
- **gamut-codec-abi** — shared codestream-backend seam: `repr(C)` vtables
  (`DecoderVTable`/`EncoderVTable` + `StreamConfig`/`EncodeConfig`/`ImageDesc`) and their
  object-safe Rust twin traits, plus the registry fallback contract by which a foreign
  (C/FFI) or alternate codestream backend plugs into a format crate. `#![no_std]` and
  **dependency-free** (not even on `gamut-core`); `unsafe` confined to its `bridge` module.
  ← nothing.
- **gamut-isobmff** (AVIF/HEIC container) / **gamut-riff** (WebP container). ← core, bitstream.
- **gamut-av1** / **gamut-av2** / **gamut-vvc** — codecs. ← core, color, dsp, bitstream.
- **gamut-jxl** — JPEG XL, uniquely a **wrapper** over the format's reference implementations
  rather than clean-slate (maintainer-approved departure from the pure-Rust rule): encode
  wraps **libjxl 0.12.0** statically via `gamut-jxl-sys` (off wasm32), decode wraps the
  pure-Rust external `jxl` crate. Both are pushable backend tails over `gamut-codec-abi`:
  the seam is the bare `FF 0A` codestream, `push_backend` tries an alternate implementation
  first (and supplies encode on wasm32), and `encode`/`decode` features mean "include the
  built-in tail", not "enable the direction"; container features (ISOBMFF/Exif/XMP/jbrd)
  stay pinned to the built-in path by a host-side veto. ← core, codec-abi, gamut-jxl-sys
  (encode, non-wasm), external `jxl`.
- **gamut-jxl-sys** — declarations-only `-sys` crate statically building/linking
  **libjxl 0.12.0** via BSD-3-Clause `jpegxl-src` (`links = "jxl"`); native backend for
  gamut-jxl's encoder and its libjxl decode-oracle tests. No gamut deps (C/FFI only);
  honors `GAMUT_JXL_SYS_SKIP_NATIVE=1` to skip cmake for check-only (cross/MSRV) jobs.
- **gamut-jpeg** — JPEG-1 (ISO/IEC 10918-1 / ITU-T T.81) codec: baseline sequential DCT
  Huffman encoder (gray + YCbCr 4:4:4/4:2:2/4:2:0, JFIF; opt-in jpegli-style XYB colour mode
  with a static vendored ICC profile), sequential/progressive decoder and progressive encoder
  phased in per its STATUS.md; oracle = libjpeg-turbo (dev-only). ← core, color, dsp.
- **gamut-avif** ← av1, isobmff, core, color, codec-abi (pluggable `Av1StillEncoder`
  codestream seam; `gamut-av1` is the implicit software tail). **gamut-webp** ← +riff; like
  gamut-png it carries the `ICCP`/`EXIF`/`XMP ` chunks verbatim as raw `MetadataBlock`-ready
  payloads, so it does not depend on the metadata facade.
- **gamut-heic** — decode-only HEIF/HEIC container: full-fidelity byte-accounting parse
  (every input byte maps to a box, appended motion-photo stream, or explicit trailer), typed
  `hvcC`/NAL layer, pluggable `HevcDecoder` hook for platform HEVC decoders (HEVC bitstream
  decode itself is out of scope here). Differential oracle: libheif+libde265 (+kvazaar
  fixture generation), dev-only. ← isobmff, core, color.
- **gamut-deflate** — pure-Rust DEFLATE/zlib **encoder** (zopfli-class) under gamut-png;
  deliberately encoder-only — workspace decoders inflate via `miniz_oxide`. ← core.
- **gamut-png** — PNG codec (3rd edition, W3C): space-efficient encoder and spec-compliant
  decoder — all colour types/bit depths, Adam7 *decoding*, all filters, decode limits for
  hostile input, ancillary metadata surfaced as raw `MetadataBlock`-ready payloads
  (eXIf/iCCP/XMP/text) plus parsed gAMA/cHRM/sRGB/cICP. APNG out of scope (decodes as the
  default image). Differential oracle both directions: libpng, which also *generates* the
  decoder's conformance fixtures. ← core, deflate (+ `miniz_oxide` for inflate, and
  **maintainer-approved `crc32fast`** for the chunk CRC that every encode pays on its critical
  path — hardware CRC-32 on x86-64/aarch64, table fallback elsewhere including wasm32, and it
  keeps its `unsafe` to itself, so gamut-png stays `#![deny(unsafe_code)]`).
- **gamut-ifd** — TIFF/IFD container core (byte order, field types, IFD read/write); a
  low-level container primitive (sibling to bitstream), shared by `gamut-tiff` and EXIF
  metadata. ← core. Optional `bigtiff` feature adds 64-bit BigTIFF. Per-format metadata
  crates (**gamut-exif** ← ifd; **gamut-icc**; **gamut-xmp**; **gamut-iptc** ← xmp) and the
  **gamut-metadata** facade (← exif/xmp/icc/iptc) layer on top under the `metadata` feature;
  format crates consume the facade for embedded metadata.
- **gamut-cmm** — ICC colour management module (epic #323): the transform engine — a
  validated pipeline/stage model behind the object-safe `Transform` trait — that builds and
  applies colour transforms from the profiles `gamut-icc` parses; f64 Tier-1, behavioural
  oracle Little-CMS (`references/cmm/`, dev-only `tooling/lcms2-oracle`). ← icc, color, core.
- **gamut-tiff** — natively still-image TIFF 6.0; its IFD/tag container is the shared
  **gamut-ifd** primitive (with `bigtiff`), not isobmff/riff. Adds pixel modes plus
  compressions (None/PackBits/LZW/CCITT; JPEG-in-TIFF deferred). ← ifd, core, bitstream.
- **gamut-dng** — DNG (Adobe Digital Negative) 1.7.1 raw encoder + decoder, a TIFF/EP
  profile on the shared **gamut-ifd** sub-IFD tree (with `bigtiff`). CFA/LinearRaw
  photometry, uncompressed/Deflate/lossless-JPEG, colour-calibration profile, EXIF/XMP/ICC
  metadata. Conformance-gated against the headless-built **Adobe DNG SDK**. ← ifd, bitstream,
  color, metadata, core (MSB-first sub-byte packing reuses `gamut-bitstream`; the DNG §6
  `AsShotWhiteXY` → camera-neutral conversion reuses `gamut-color`'s chromaticity/3×3/CCT math;
  `DngMetadata` carries the facade's own models — a DNG `ExifIFD` *is* an EXIF sub-IFD, so it is
  `gamut-exif`'s `Exif` over the same `gamut-ifd` directory type, with the XMP/IPTC-IIM/ICC
  payloads handed over verbatim as `MetadataBlock`s; `miniz_oxide` for Deflate).
- **gamut-cli** (binary `gamut`) / **gamut-wasm** (cdylib) / **gamut-ffi** (cdylib/staticlib).
  ← gamut. `gamut-cli` is the sandbox exercising implemented features: decodes PNG/JPEG/WebP/JXL
  input with gamut's own decoders (only PPM uses the third-party `image` crate) and encodes only
  with gamut crates, and exposes `primitives` re-exports as inspection subcommands.

## Code correctness

- Correctness: implement the specification claimed; test thoroughly against the crate's
  oracle. Mutation testing should pass with only non-redundant, high-value tests;
  exclusions need strictly strong justification.
- Test scope: **a test names one thing and fails for one reason.** Minimise its *reach* — the
  modules a defect in which can fail it — and name it for that reach. Placement is mechanical, not
  taste: if the assertion reads a non-`pub` item it goes inline in `#[cfg(test)] mod tests` beside
  the code, because `.cargo/mutants.toml` sets `test_workspace = false` and a mutant masked at the
  public boundary is killable *only* from there; it goes in `crates/<crate>/tests/` only when
  linkage forces it (`unsafe` under a crate's `forbid(unsafe_code)`, a whole-file `#[cfg]` gate, or
  `check-release-deps` topology); otherwise either is legal. A dev-dependency oracle is **not** a
  reason to move up. `crates/gamut/tests/` is mutation-invisible, so nothing may be pinned only
  there by choice. Before adding a test, name the one function whose mutation it kills — if you
  cannot, it is at the wrong scope. **This rule never authorises relocating a crate's suite in
  bulk.**
- Test technique: use the **weakest technique that can falsify the claim** — example, exact-byte,
  law, property, differential, conformance, drift guard, robustness. A round-trip is
  self-consistent and cannot see a defect symmetric across encoder and decoder, so where the crate
  has an oracle the oracle is the stronger test; where nothing stands in for one — see the
  per-crate authority table in `docs/testing.md` — a property is. Write each law once as a plain
  function in the crate's `invariants` module and drive it from both a **pinned-seed** `proptest`
  and the fuzz tier: a property is the specification a fuzzer checks. Never leave a property's
  `rng_seed`, shrink bounds or failure persistence unpinned — the mutation gate is blocking and
  must be reproducible run to run — and remember an `invariants` module is the oracle, not the
  system under test, so it is excluded from mutation. Do not convert existing deterministic sweeps
  to properties. Placement, techniques, the authority table and the proptest/fuzz contract:
  `docs/testing.md`.
- Specification as source of truth: all implementation and tests are based on the official
  specs (in `references/`) and the oracle claimed in the crate's docs.
- Design and documentation: public API is usable without reading docs; document features
  not yet planned or deferred.
- No duplication: maximally depend on other `gamut` crates and maintainer-approved external
  crates over reimplementing.

## Reference

All codec implementations must follow the official specs, vendored/documented under
`references/`.

## Validation

Dev tooling (hk, convco, cargo-llvm-cov, CMake/Ninja/Meson, …) is provisioned by
[mise](https://mise.jdx.dev): run `mise install` and activate mise in your shell.
`mise tasks` lists all tasks (formerly `just` recipes). Validate changes:

```bash
mise run test          # correctness
mise run fmt-check     # formatting (nightly rustfmt, auto-installed)
mise run lint          # lint (Clippy, warnings as errors)
mise run coverage      # coverage (minimum 80%)
mise run mutants       # mutation testing (needs submodules + C toolchain; run `mise install` once).
                       # Takes a selection: `--diff`, `--crate <name>` or `--shard i/n`; an
                       # unsharded whole-workspace run (24k mutants) is refused. Every dial is
                       # derived from a memory budget — see docs/mutation-testing.md. Never call
                       # `cargo mutants` directly; the guards live in the mise task.
mise run fetch-av1-oracles # fetch the `update = none` aom/dav1d submodules (~440 MiB); needed
                       # before any task that builds the AV1/AVIF oracles (test, lint, coverage,
                       # mutants), since a recursive submodule update deliberately skips them
mise run test-dng-real # gamut-dng vs real camera DNGs (needs `mise run fetch-dng-samples`
                       # first: a ~178 MiB CC0 corpus submodule). Extended CI, master/manual
mise run check-cross <triple> # cross-compile check (wasm32/aarch64/musl); extended CI, master/manual
mise run check-msrv    # compile on documented MSRV; extended CI, master/manual
mise run check-commits # commit messages are Conventional Commits
```

Shipped crates are pure Rust, with one deliberate exception: `gamut-jxl`'s `encode` feature
statically builds the libjxl reference encoder (cmake + C++ toolchain) via `gamut-jxl-sys`
(`wasm32` gets decode-only JXL). Cross-check tests separately link reference codecs (libaom,
dav1d, libavif, libtiff) built from `third_party/` git submodules via dev-only oracle crates
in `tooling/`; libaom is the definitive AV1/AVIF oracle (see `references/av1/README.md`).
Running these needs submodules checked out (`git submodule update --init --recursive`) and
build tools on `PATH` (CMake/Ninja/Meson via mise; pkg-config is the one system package;
nasm is built from a vendored tarball). `aom` and `dav1d` are `update = none`, so that
recursive update skips them — a git consumer of any gamut crate would otherwise fetch ~440 MiB
of codecs Cargo never builds; `mise run fetch-av1-oracles` pulls them for the AV1/AVIF oracle
tests. No system-installed codec binaries are used.

These native builds are hermetic to exactly what they configure, **including the toolchain**:
build scripts normalize an ambient compiler cache (`CC="sccache gcc"`,
`CMAKE_*_COMPILER_LAUNCHER`, ccache shim dirs) into a bare compiler plus a launcher in
CMake's one defined position, via `tooling/build-env` at each build script's `run`
chokepoint. So a native build failure is **not** explained by the invoking shell's compiler
settings — do not "fix" it by overriding `CC`/`CXX` per command. `GAMUT_BUILD_KEEP_ENV=1`
opts out (and confirms a suspected env interaction is real).

A build-script failure that is **not** a compile error — cargo reporting that it could not run
the build script itself — is usually a lost owner-execute bit under `target/debug/build/`, not a
code problem. The damaged mode is `-rw-rwx---`: the group keeps `x`, so `ls` output looks
unremarkable, but cargo runs as the owner and the owner class is what the kernel checks. Recover
with `chmod -R u+x target/debug/build/` and re-run. The cause is environmental and outside this
repo — nothing here sets file modes — so do not work around it by editing build scripts.

## Conventions

- All `pub` items need doc comments. Mark fallible/owning return types `#[must_use]` where
  dropping the value is likely a bug.
- No `unwrap()`/`expect()` in library code paths — return typed errors via `thiserror`.
- `unsafe` is **denied**, never *forbidden*, in the six hot-path crates (`gamut-png`,
  `gamut-deflate`, `gamut-dsp`, `gamut-color`, `gamut-cmm`, `gamut-jpeg`). `forbid` cannot be
  lifted at a site even with a written justification — it gives
  `error[E0453]: expect(unsafe_code) incompatible with previous forbid` — so it makes a measured
  win unarguable rather than merely discouraged. A site that has earned the exception carries
  `#[expect(unsafe_code, reason = "…")]` naming the win **and** the benchmark that shows it,
  plus a `// SAFETY:` comment per block. `expect` warns once the exception stops being needed,
  so a hatch left open after a refactor fails CI on its own. **A win nobody measured is not a
  reason** — land the bench first. Reach for it last: prefer a maintained crate that already
  encapsulates the `unsafe` (`crc32fast`, `wide`), and note that since Rust 1.87
  `#[target_feature]` on a *safe* `fn` makes most `core::arch` intrinsics safe to call — on
  aarch64 (NEON always on) and wasm32 (`+simd128` is build-time) hand-written SIMD needs no
  `unsafe` at all, and only x86 runtime dispatch does. The other twenty-one crates keep
  `#![forbid(unsafe_code)]`: for the offset-driven parsers and the metadata crates, memory
  safety on hostile input is the product guarantee, not a means to speed. Separately,
  `gamut-codec-abi`, `gamut-ffi`, `gamut-jxl-sys` and `gamut-jxl`'s `ffi` are `extern "C"`
  surfaces that cannot exist without `unsafe`; it stays confined to one named module there (the
  `gamut-jxl` `ffi` / `gamut-codec-abi` `bridge` pattern) and is never widened for speed.
- Keep encoders allocation-conscious: prefer slices and `&[u8]` over owned buffers in hot
  paths, and document each format's space/time tradeoff.
- Stub crates stay region-free for the coverage gate: a placeholder `lib.rs` holds only
  module docs + declarations (traits/types without bodies), **no placeholder `fn` bodies**
  (a `todo!()`-bodied fn adds an uncovered region). `gamut-(cli|wasm|ffi)` are excluded from
  coverage via `--ignore-filename-regex`.
- C portability: keep the public API mechanically portable to C while staying idiomatic
  Rust. Codec entry points go through the object-safe `EncodeImage`/`DecodeImage` pair over
  the sealed `Pixel` matrix (runtime tag: `gamut_core::PixelFormat`); configs are plain data —
  `Copy` structs, fieldless enums with an explicit `repr` and permanent append-only
  discriminants, or payloads reachable through accessors; new extension hooks follow the
  `gamut_heic::HevcDecoder` shape (single object-safe method, borrowed bytes in, owned plain
  data out). The C ABI contract is `crates/gamut-ffi/DESIGN.md`; `gamut-ffi`'s feature table
  strictly mirrors `gamut`'s (`mise run check-ffi-features`, enforced in CI).
- Exposing the codestream: a format crate that lets callers swap in a foreign or alternate
  codestream backend does so through **one** seam, `gamut-codec-abi`, never a bespoke one.
  Pattern: a **typed trait per format** (the `gamut_heic::HevcDecoder` shape — object-safe,
  borrowed bytes in, owned plain data out, named for the codestream it decodes) plus a thin
  **codec-abi adapter** bridging that typed trait to the shared `Decoder`/`Encoder` twins and
  their `repr(C)` vtables, so a C/`-sys` backend and a pure-Rust one enter by the same door.
  The host keeps a **registry** of backends tried in **push order**; a crate's own software
  implementation, when it ships one, is the implicit tail tried last. `supports()` returning
  `false` (C `Status::UNSUPPORTED`) is the **only** fall-through signal — a backend that
  accepts a job and *then* fails returns a terminal non-OK `Status` propagated to the caller
  unchanged, so a partially-produced result is never silently masked by a retry. `Send` is
  **not** a supertrait of `Decoder`/`Encoder`; a host bounds `Send` at the point it inserts a
  backend, keeping single-threaded backends usable. Stub codecs `gamut-av2`/`gamut-vvc`
  adopt this convention when implemented.

## Versioning

Each crate is versioned **independently** per SemVer — no shared workspace version, and
releases don't guarantee cross-crate version consistency. Only `version` is per-crate; all
other metadata (`edition`, `rust-version`/MSRV, license, repository) is workspace-owned via
`*.workspace = true`. Version bumps, per-crate changelogs, and crates.io publishing are
automated by release-plz from conventional-commit history — write conventional commits
(enforced by convco via the `commit-msg` hook and CI) and don't hand-edit versions for
routine changes. `mise run versions` lists every crate's current version; `mise run bump
<crate> <level>` is a manual escape hatch.

Release ordering follows normal and build dependency edges; release-plz deliberately ignores
dev-only edges. A publishable crate must therefore not dev-depend on another distinct publishable
workspace crate unless it also has a normal/build dependency on that crate. Put cross-crate
interoperability tests at an integrating layer such as the `gamut` umbrella instead. `mise run
check-release-deps` enforces this before a version bump reaches the release workflow.
