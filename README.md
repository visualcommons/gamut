# gamut

> Project Status: Early development. Achieving full specs compliance across modern formats (AVIF, JXL, WebP, DNG) but no guarantees on API stability yet.

Memory-safe, specs-compliant, quality-optimized image primitives.

## Why gamut?

The world doesn't lack image codecs. libavif/libaom, libwebp, and libjpeg-turbo are
mature, fast, and battle-tested — we're not out to beat a decade of hand-tuned SIMD
assembly on raw encode speed. gamut exists because "fast C that works" still leaves real
gaps, and those gaps are exactly where a clean-slate, pure-Rust, permissively-licensed
implementation wins.

- **Memory safety on the industry's worst attack surface.** Image parsers chew on hostile,
  attacker-controlled bytes from the open internet, and the C codecs have the CVE record to
  show how that goes — libwebp's CVE-2023-4863 was a zero-click, wormable heap overflow that
  triggered emergency out-of-band patches across browsers, Electron apps, and mobile OSes in
  a single week. Safe Rust deletes that entire bug class (spatial and temporal memory
  corruption) from the encode and parse paths. For anything that ingests untrusted images,
  that alone justifies the rewrite.

- **Builds anywhere `cargo` does.** No autotools, no CMake, no nasm/yasm, no vendored C, no
  FFI boundary to audit. `cargo build` cross-compiles cleanly to wasm32, aarch64, and musl
  targets that libaom makes miserable — one toolchain, reproducible builds, no system-library
  version skew. CI proves it each merge: the [Extended workflow](.github/workflows/extended.yml)
  `cargo check`s the library surface for `wasm32-unknown-unknown`, `wasm32-unknown-emscripten`,
  `aarch64-unknown-linux-gnu`, and `x86_64-unknown-linux-musl`. The one deliberate exception is
  `gamut-jxl`'s optional `encode` feature, which statically builds the libjxl reference encoder
  (cmake + a C++ toolchain at build time; the emsdk toolchain on `wasm32-unknown-emscripten`,
  where a dedicated Extended lane runs the full JXL test suite under node) — a
  maintainer-approved departure documented in that crate; its pure-Rust `decode` feature keeps
  JPEG XL C-free and available on every `wasm32` target.

- **WASM as a first-class target, not an afterthought.** The C codecs run through Emscripten
  come out large, slow to instantiate, and awkward to tree-shake. A native Rust → wasm build
  is smaller and talks to the JS/TS ecosystem directly, which makes serverless/edge image
  optimization (Workers, Lambda, and friends) practical instead of shipping a multi-megabyte
  blob.

- **A genuinely clean license story.** gamut deliberately targets royalty-free formats and
  ships under MIT OR Apache-2.0 — no GPL/LGPL reach. The lone static-linking case is
  `gamut-jxl`'s optional `encode` feature (the BSD-3-Clause libjxl reference encoder plus its
  permissively-licensed bundled libraries — highway, brotli, skcms); every other codec, and a
  decode-only JPEG XL build, links no C at all. Patent-unencumbered formats deserve
  permissively-licensed code to match.

- **Encoder-first, size-first — the gap the Rust ecosystem actually has.** Most Rust imaging
  is decode-only and hands the hard encoders off to C wrappers. gamut is built the other way
  round: encoders are the product, and the thing we optimize is *output bytes at a given
  quality and speed*, with the space/time tradeoff documented per format. That's the number
  that lands on storage and bandwidth bills. Decoders may follow where the Rust ecosystem
  lacks a strong, feature-complete implementation, but encoders are the priority.

- **One codebase, shared primitives.** Color management, DSP, bitstream, and container parsing
  live in shared crates (`gamut-color`, `gamut-dsp`, `gamut-bitstream`, `gamut-isobmff`,
  `gamut-riff`) instead of being re-implemented inside each separate C library. Consistent
  behavior across formats, one API, one place to fix a color bug — and you compile in only the
  formats you enable via Cargo features.

- **Readable enough to change.** Implemented clean-slate from the official specs in
  `references/`, the code is something you can actually audit, fork, and experiment with —
  not decades of accreted platform `#ifdef`s and inline assembly.

### Author's Remarks

In 2026, `gamut` started when there were no robust, well-tested Rust implementations of various image and color primitives. We want this to be the de-facto, permissivel-licensed choice for most color and image needs, primarily for professional use cases. Implementing this ecosystem of libraries (crates) without commercial backing is also made possible when image formats are spec-driven and LLM agents sufficiently speed up work (when used correctly).

### Scope

The initial focus is **AVIF, WebP, and JPEG** — the formats with the best
size-versus-compatibility tradeoff today. **JPEG XL** (`gamut-jxl`) is implemented as an
encoder + decoder (issue #243) — uniquely, by wrapping the format's reference implementations
(libjxl for encode, the pure-Rust jxl-rs for decode) rather than clean-slate, a deliberate
maintainer decision documented in that crate. **TIFF 6.0** (`gamut-tiff`) is implemented at v1
(issue #107) as a royalty-free, natively still-image format — a good long-term fit for the
image-first focus — with YCbCr/Lab and JPEG-in-TIFF still deferred, and **DNG** (`gamut-dng`)
is a raw encoder + decoder built as a TIFF/EP profile over the same `gamut-ifd` container core.
`gamut-heic` is a decode-only HEIF container, with the HEVC bitstream itself left to a pluggable
backend rather than decoded here. Of the format crates, only `gamut-vvc` and `gamut-av2` are
still scaffolding, and they may move or be dropped as the focus sharpens.

Alongside the codecs, gamut ships **shared image-metadata primitives** (issue #34) — EXIF,
XMP, ICC, and IPTC, plus the TIFF/IFD container core (`gamut-ifd`) that EXIF builds on. These are
all at v1 or later; EXIF, IPTC and XMP are gated differentially against exiv2 (which bundles
Adobe's XMPCore), and ICC against Little-CMS. `gamut-cmm` adds the ICC transform engine over the
profiles `gamut-icc` parses — conformance-gated against Little-CMS, though still well behind it
on throughput — and is the one crate here not yet on crates.io.

The long-term goal is de-facto, fully-featured implementations — for EXIF, exiftool-class tag
coverage — and today's crates do not reach it yet. The largest gap is reach rather than depth:
the `gamut-metadata` facade is consumed by `gamut-dng` and the umbrella only, so every other
format crate still carries EXIF/XMP/ICC as raw byte blocks instead of calling the typed path.
Behind that sit per-vendor MakerNote payloads, which round-trip verbatim but are not decoded, and
tag breadth beyond the standard dictionary. Issue #416 tracks the whole gap, measured.

**gamut is image-first.** Even where a format's codec (AV1, AV2, VVC, HEVC) is fundamentally a
video codec, gamut implements only the intra-frame, still-image subset those formats use — no
inter-frame prediction, no motion compensation, no video sequences. The video-named codec
crates (`gamut-av1`, `gamut-av2`, `gamut-vvc`, and HEVC-based `gamut-heic`) are still-image
codecs, not video codecs, and gamut will not grow video primitives. This extends to
container-level multi-frame sequences: WebP animation (`ANIM`/`ANMF`) is out of scope even
though each frame is an independent keyframe — only single still images are supported.

## Usage

Add the umbrella `gamut` crate and enable only the formats you need:

```toml
[dependencies]
gamut = { version = "0.1", features = ["avif", "jxl"] }
```

The umbrella has no default features, so a bare dependency compiles only `gamut-core`. The
`primitives` feature additionally re-exports the shared building blocks (`gamut::color` /
`gamut::dsp` / `gamut::bitstream`) for tooling and sandbox use; `all` enables it along with every
format.

## Crates

Every row describes the crate **in this tree**: any version it cites is the crate's own
`Cargo.toml` — what `mise run versions` prints and what a workspace `path` dependency builds — not
what crates.io currently serves. Where the two differ, [Releases](#releases) says which crate and
why; deciding that comparison needs the network, so it is stated there rather than counted here.

| Crate             | Purpose                                                                | Status                                 |
| ----------------- | ---------------------------------------------------------------------- | -------------------------------------- |
| `gamut`           | Umbrella crate; each sibling crate it re-exports sits behind its own Cargo feature — the format/codec crates plus the shared layers (core, colour, DSP, bitstream, containers, metadata, tone mapping, colour management, the codec ABI) — with one always-on dependency: `gamut-core` | implemented |
| `gamut-core`      | Core traits (`EncodeImage`/`DecodeImage`), image buffers, dimensions, errors, `convert` | stable (v2; v1 under #177), pixel conversion added by #268 |
| `gamut-color`     | Coded-plane bit depths and chroma subsampling, CICP code points and planar buffers, the `ycbcr` matrixing layer (H.273, plus the libwebp-exact BT.601 one VP8 needs), and the `f64` colour science (transfer, Lab/OKLab, XYB, `matrix`/`linalg`, gamut map, CCT, profile) | stable (v2; v1 under #179); the colour science is Tier-1 `f64`, not bit-reproducible |
| `gamut-dsp`       | Shared DSP kernels: AV1 DCT/ADST/identity/WHT, the JPEG 8×8 forward/inverse DCT, quantization rounding | stable (v2; v1 under #192)             |
| `gamut-bitstream` | Bit readers/writers, LEB128, the AV1 §8.2 symbol (arithmetic) coder, MSB-first sample packing | v0.2, no v1 release issue yet; the ANS and Huffman coders are not implemented |
| `gamut-tonemap`   | Tone-mapping curves for HDR→SDR: the `ToneCurve` trait over the Linear, Clamp, Exposure, Reinhard, ReinhardExtended, ACES, Hable and Drago operators | stable (v1, #188); surface frozen |
| `gamut-codec-abi` | Shared codestream-backend seam: `repr(C)` vtables, their object-safe Rust twins, and the registry fallback *contract*; this crate declares no registry — each host crate that offers a seam keeps its own | consumed by `gamut`, `gamut-avif`, `gamut-ffi`, `gamut-heic`, `gamut-jpeg`, `gamut-jxl`, `gamut-png` and `gamut-webp`; the umbrella's edge is optional, behind feature `codec-abi` |
| `gamut-isobmff`   | ISOBMFF container utilities (AVIF, HEIC)                               | stable (v2); structure only, codestream carried opaquely |
| `gamut-riff`      | RIFF container utilities (WebP)                                        | stable (v1, #186)                      |
| `gamut-av1`       | AV1 still-image (intra-frame) encoder + decoder — the codec layer beneath AVIF | encoder: lossless and lossy intra keyframes; decoder (default feature `decode`): 8-bit 4:4:4 intra frames, key and intra-only (#259) |
| `gamut-av2`       | AV2 still-image (intra-frame) encoder/decoder — AV1's successor        | placeholder                            |
| `gamut-avif`      | AVIF encoder + container decoder — AV1 still frames in ISOBMFF         | encoder (v1, 8/10/12-bit) + container decode (#250); AV1 codestream decode via the `Av1StillDecoder` seam |
| `gamut-jxl`       | JPEG XL encoder (libjxl wrap) + decoder (pure-Rust jxl-rs)             | encoder + decoder (#243)               |
| `gamut-jxl-sys`   | Static libjxl 0.12.0 FFI declarations — native core of gamut-jxl encode | encoder backend (#243)                 |
| `gamut-jpeg`      | JPEG-1 (ISO/IEC 10918-1) encoder + decoder — baseline & progressive; the jpegli-style XYB colour mode is encode-only | encoder + decoder (#28, P1–P13) |
| `gamut-webp`      | WebP (intra-frame VP8/VP8L) encoder/decoder, with a public `backend` seam for an alternate VP8/VP8L codestream implementation | implemented VP8 + VP8L (+alpha, metadata, effort/near-lossless) |
| `gamut-heic`      | HEIC/HEIF still-image container **decoder** — HEVC via a pluggable backend | decode-only container (S1–S7); no encoder, by charter |
| `gamut-vvc`       | VVC (H.266) still-image (intra) encoder/decoder                        | placeholder                            |
| `gamut-ifd`       | TIFF/IFD container core (byte order, field types, IFD I/O) — EXIF+TIFF | stable (v2, byte completeness #263); BigTIFF behind feature `bigtiff` |
| `gamut-exif`      | EXIF (Exif 3.0) metadata parser/serializer — built on gamut-ifd        | stable (v1, #194); MakerNote preserved verbatim, not decoded |
| `gamut-icc`       | ICC color profile (ICC.1:2022) parser/serializer                      | stable (v1, #180)                      |
| `gamut-cmm`       | ICC colour management module (transform engine) over gamut-icc profiles | P1–P8 complete per its STATUS.md — P1–P7 are epic #323, while P8 (pipeline optimization) is #372, which #323 lists out of scope |
| `gamut-xmp`       | XMP (RDF/XML) metadata parser/serializer                              | stable (v1, #189); canonical serializer, UTF-8 packets only |
| `gamut-iptc`      | IPTC photo metadata (IIM + Core/Extension over XMP)                    | stable (v1, #182); Extension structures pass through as raw XMP |
| `gamut-metadata`  | Unified metadata facade over EXIF/XMP/ICC/IPTC (extract + embed)       | stable (v1); orchestration only. A C2PA manifest store is extracted verbatim but never re-embedded: embedding drops it, or refuses, because its hard binding cannot survive the rewrite |
| `gamut-tiff`      | TIFF 6.0 encoder/decoder — on the shared `gamut-ifd` container core     | stable (v1, #107); YCbCr/Lab and JPEG-in-TIFF deferred |
| `gamut-dng`       | DNG 1.7.1 raw encoder + decoder — a TIFF/EP profile over `gamut-ifd`   | encoder + decoder (v1, #109), Adobe DNG SDK-gated |
| `gamut-deflate`   | DEFLATE/zlib encoder (zopfli-class) — the compression under gamut's own writers | encoder (#195); consumed by `gamut-dng`, `gamut-png` and `gamut-tiff`, which inflate with `miniz_oxide` rather than here — this crate has no always-on dependencies of its own |
| `gamut-png`       | PNG (W3C 3rd edition) encoder + decoder, over the still-image subset   | encoder (#24) + decoder (#249); APNG out of scope, so an animated PNG decodes as its default image |
| `gamut-cli`       | `gamut` CLI sandbox: `convert` (decode PNG/JPEG/PPM/WebP/JXL, re-encode AVIF/WebP/TIFF/PNG/JXL/JPEG), `inspect` (TIFF/DNG byte accounting), `icc`, `isobmff`, `av1`, and the `color`, `dsp` and `bitstream` primitive inspectors | ready for use |
| `gamut-wasm`      | WebAssembly bindings                                                   | placeholder                            |
| `gamut-ffi`       | C-compatible FFI bindings                                              | provider boundary shipped (#280); consumer entry points pending (#242) |

All cargo metadata except per-crate `version` is centralized in the root
`[workspace.package]` / `[workspace.dependencies]`; each crate inherits the shared fields via
`.workspace = true` and sets its own `version` (see [Versioning](#versioning)).

## Prerequisites

- [Rust (rustup)](https://rustup.rs) -- toolchain (channel pinned via `rust-toolchain.toml`);
  see [Minimum Supported Rust Version](#minimum-supported-rust-version) for the lower bound
- [mise](https://mise.jdx.dev) -- provisions the rest of the dev tooling from `mise.toml`
  (and the dev tasks — run `mise tasks` to list them, `mise run <task>` to invoke):
  [hk](https://hk.jdx.dev) (git hooks),
  [convco](https://convco.github.io) (conventional-commit linter),
  [jq](https://jqlang.github.io/jq/),
  [cargo-llvm-cov](https://github.com/taiki-e/cargo-llvm-cov) (coverage),
  [cargo-edit](https://github.com/killercup/cargo-edit) (`cargo set-version` for `mise run bump`),
  and the C build tools [CMake](https://cmake.org), [Ninja](https://ninja-build.org) and
  [Meson](https://mesonbuild.com). After cloning, run `mise trust && mise install`, then
  activate mise in your shell (e.g. `eval "$(mise activate zsh)"`) so these land on `PATH` —
  the git hooks and mise tasks invoke them directly.

Building the **shipped crates** needs only the Rust toolchain — they are pure Rust with no C
dependencies, with one deliberate exception: `gamut-jxl`'s optional `encode` feature statically
builds the libjxl reference encoder via `gamut-jxl-sys`, which needs **cmake and a C++ toolchain**
at build time (a maintainer-approved departure for JPEG XL; its `decode` feature stays pure Rust;
`wasm32-unknown-emscripten` gets the full encoder via the emsdk toolchain, while
`wasm32-unknown-unknown` gets a decode-only JXL — no C++ toolchain targets that ABI). Building the
**cross-check tests** additionally needs a C
toolchain plus
pkg-config — the one build dep that stays a system package (CMake, Ninja and Meson come from
mise; [nasm](https://www.nasm.us), needed to assemble the aom/dav1d x86 SIMD, is built from a
vendored source tarball by the oracle build scripts, so it is not a system dependency). Those
tests link reference codecs (libaom, dav1d, libavif) built from the git submodules under
`third_party/` via the dev-only oracle crates in `tooling/`; nothing is taken from
system-installed codecs. The `aom` and `dav1d` submodules are marked `update = none`, so a
recursive submodule update skips them and taking any gamut crate as a git dependency does not
drag in ~440 MiB of codecs Cargo never builds; `mise run fetch-av1-oracles` pulls them when you
want the AV1/AVIF oracle tests. libaom — the AV1 reference codec — is the definitive AVIF/AV1
oracle (see [`references/av1`](references/av1/README.md)). Install pkg-config on Debian/Ubuntu
with `sudo apt-get install pkg-config` (macOS: `brew install pkg-config`).

Those native builds are **hermetic to exactly what they configure**, including the toolchain: if
your shell exports a compiler cache (`CC="sccache gcc"`, `CMAKE_C_COMPILER_LAUNCHER=ccache`, a
`ccache` shim directory on `PATH`, …), the build scripts normalise it rather than inherit it —
the compiler is passed bare and the launcher is placed in the single position CMake defines for
it. Compiler caching keeps working; you do not need to special-case this repo in your shell
profile. Set `GAMUT_BUILD_KEEP_ENV=1` to opt out and use the ambient environment verbatim. See
[`tooling/build-env`](tooling/build-env/src/lib.rs) for what is normalised and why.

## Quick Start

```bash
# The cross-check tests link vendored libaom/dav1d/libavif from third_party/ submodules.
git submodule update --init --recursive

# Dev tooling + git hooks (see Prerequisites; also needs system pkg-config).
mise trust && mise install
hk install

# aom and dav1d are `update = none`, so the checkout above skips them (~440 MiB a consumer taking
# gamut as a git dependency would otherwise pay for). Fetch them for the AV1/AVIF oracle tests:
mise run fetch-av1-oracles

cargo build --workspace
cargo test --workspace
```

## Development

| Command          | Description                              |
| ---------------- | ---------------------------------------- |
| `cargo build --workspace` | Build all crates                |
| `mise run test`      | Run tests (workspace, all features)      |
| `mise run fmt`       | Format code (nightly rustfmt, auto-installed) |
| `mise run lint`      | Lint with Clippy (warnings as errors)    |
| `mise run lint-fix`  | Lint and auto-fix                        |
| `mise run check-commits` | Check commits are Conventional Commits |
| `mise run check-readme-crates` | Check this README's crates table names every workspace crate, no phantom or duplicate ones, stands under its delimiter row, and gives each crate a three-cell row |
| `mise run coverage`  | Run tests with coverage (min 80%)        |
| `mise run check-cross <triple>` | Cross-compile-check the libs for a target (extended CI; master/manual) |
| `mise run check-msrv` | Check the libs compile on the documented MSRV (extended CI; master/manual) |
| `mise run versions`  | List every crate's version               |
| `mise run bump <crate> <level>` | Bump one crate (`major`\|`minor`\|`patch`) |

## Minimum Supported Rust Version (MSRV)

The MSRV is **Rust 1.92** (stable), built against **edition 2024**. This is the lowest version we
support, declared once as the machine value in the root `[workspace.package]`
(`rust-version = "1.92"`) and inherited by every crate via `rust-version.workspace = true`; this
section is its authoritative documentation. CI enforces both: the
[Extended workflow](.github/workflows/extended.yml)'s MSRV job compiles the libraries on that
toolchain and fails unless this README documents the version declared in `Cargo.toml`, so the
field and the docs can never drift.

Policy:

- The MSRV is the floor we test and publish against, not necessarily the newest toolchain.
  Day-to-day development tracks the latest `stable` (pinned to the `stable` channel in
  `rust-toolchain.toml`).
- Formatting is the one exception: `mise run fmt` runs **nightly** rustfmt for the
  merge-resilient import options (`imports_granularity`/`group_imports`), auto-installing the
  nightly toolchain on first use. Nothing is *compiled* on nightly — only formatted — so it
  never affects the MSRV, Clippy, tests, or the shipped build, which all stay on stable.
- Raising the MSRV is a deliberate, semver-relevant change: bump `rust-version` in the root
  `Cargo.toml` and note it here. Pre-1.0, an MSRV bump rides a minor release.
- Edition (`2024`) is likewise centralized in `[workspace.package]` and inherited by every
  crate; it changes only alongside an MSRV bump that allows it.

## Git Hooks

This project uses [hk](https://hk.jdx.dev) (provisioned by mise); run `hk install` once after
`mise install` to register the hooks (config in `hk.pkl`). The `commit-msg` hook rejects
messages that aren't [Conventional Commits](https://www.conventionalcommits.org) (policy in
`.convco`) — enforcement happens when the commit is created, so a bad message can't slip through
to a `--no-verify` push. The `pre-commit` hook auto-fixes formatting and linting on the staged
snapshot (unstaged work is stashed first, then restored). The `pre-push` hook is a deliberately
fast static gate — a formatting check and a quick Clippy pass — while the heavier full-feature
Clippy, tests, and coverage run in CI, so the local hooks stay fast enough not to be bypassed.
The hook steps delegate to the shared `mise run` tasks, so keep mise activated in your shell.

## CI/CD

GitHub Actions provisions tooling via [mise](https://mise.jdx.dev) and runs format checks,
linting, tests, and coverage on pushes to `master` and pull requests. Pull requests
additionally validate every commit message against Conventional Commits with convco.

## Code Coverage

This project uses [`cargo-llvm-cov`](https://github.com/taiki-e/cargo-llvm-cov) for
LLVM-based code coverage. CI enforces a minimum of 80% line coverage.

```bash
mise run coverage
```

The bindings/binary crates (`gamut-cli`, `gamut-wasm`, `gamut-ffi`) are excluded from the
gate — their entry points are not meaningfully unit-testable.

## AI Policy

Vibe-coded contributions are welcome. AI-assisted PRs are accepted as long as you
personally vouch for the work — you've read it, you understand it, and you stand behind it
as if you'd written every line — and it matches the project's existing code style and
requirements. The CI and git hooks loosely enforce the bare minimum; meeting that bar is
necessary but not sufficient. Review your output before opening a PR.

## Versioning

Every crate is versioned **independently** following [SemVer](https://semver.org), based on
its own changes. There is **no** guarantee that versions line up across the workspace — a
change to one codec bumps only that crate (and anything that depends on it), so version
numbers drift apart over time. Only `version` is per-crate; shared metadata such as the
edition and [MSRV](#minimum-supported-rust-version) stays workspace-owned.

Bumps are automated: [release-plz](https://release-plz.dev) reads each crate's
conventional-commit history, computes its next version, and updates dependents' requirements
as needed. Each crate keeps its own `CHANGELOG.md` and is tagged and GitHub-released as
`<crate>-v<version>` (e.g. `gamut-core-v0.2.0`) — there is no single repo-wide version tag,
so the umbrella `gamut` crate's version serves as the headline "project" version. Run
`mise run versions` to see every crate's current version at a glance.

Because release-plz keys versions and changelogs off commit messages, those messages are
enforced as [Conventional Commits](https://www.conventionalcommits.org) — by the git hooks
locally and the CI PR check (see [Git Hooks](#git-hooks)) — to keep the changelogs clean.

## Releases

Publishing to crates.io is automated with [release-plz](https://release-plz.dev). On pushes
to `master` it opens a release PR (per-crate version bumps + changelogs); merging that PR
publishes every changed crate in dependency order, then creates the per-crate tags and GitHub
releases. Publishing authenticates via crates.io
[Trusted Publishing](https://crates.io/docs/trusted-publishing) (OIDC) — no
`CARGO_REGISTRY_TOKEN` secret is stored.

That automation has two prerequisites that live outside this repository's files, and the
pipeline stalls silently when either is missing (issue #377):

1. **The release PR needs permission to exist.** The default `GITHUB_TOKEN` cannot open a pull
   request unless *Settings → Actions → "Allow GitHub Actions to create and approve pull
   requests"* is enabled. Without it, release-plz pushes the version-bump branch and then fails
   with `"GitHub Actions is not permitted to create or approve pull requests." 403`, so no bump
   ever merges — and the release job keeps verifying crates against stale *published*
   dependencies rather than the versions on `master`. Setting a `RELEASE_PLZ_TOKEN` secret (a PAT
   with `contents:write` + `pull-requests:write`) is the alternative;
   [`release.yml`](.github/workflows/release.yml) prefers it over `GITHUB_TOKEN` when present.
2. **A brand-new crate needs one manual first publish.** Trusted Publishing mints a token *for a
   crate that already exists*, so the very first version of a new crate must be published once
   with an owner-scoped token, after which OIDC takes over. **`gamut-cmm` is the only crate still
   awaiting that bootstrap** — it is the one workspace crate absent from crates.io.

A missing `<crate>-v<version>` tag is **not** evidence of a missing publish, and this section
previously read it that way. `gamut-deflate`, `gamut-dng`, `gamut-jpeg`, `gamut-jxl-sys` and
`gamut-png` carry no release tag and are nonetheless on crates.io, each at the version its
`Cargo.toml` declares. That is the stall above showing itself in the tag-and-release half of the
pipeline rather than in the publish half.

The opposite skew exists too, and it is the one place a version in the [crates table](#crates)
differs from what crates.io serves: `gamut-riff`'s manifest declares `1.0.0` while the newest
published release is `0.1.3`. Whether that 1.0 is real is a release decision rather than a
documentation one, so the table states the manifest version and this note states the gap
(issue #546).

## License

Licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE) at your option.
