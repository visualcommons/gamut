# gamut

> Project Status: Early development, working toward full spec compliance across modern still-image formats. API stability is per crate and follows SemVer alone — see [Versioning](#versioning).

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

- **Builds anywhere `cargo` does, WASM included.** No autotools, no CMake, no nasm/yasm, no
  vendored C, no FFI boundary to audit: one toolchain, reproducible builds, no system-library
  version skew. That makes wasm32 a first-class target rather than an Emscripten afterthought —
  a native Rust → wasm build is smaller, instantiates faster, and talks to the JS/TS ecosystem
  directly, so serverless and edge image optimization (Workers, Lambda, and friends) is
  practical. The [Extended workflow](.github/workflows/extended.yml) `cargo check`s the library
  surface for `wasm32-unknown-unknown`, `wasm32-unknown-emscripten`, `aarch64-unknown-linux-gnu`,
  and `x86_64-unknown-linux-musl` on every merge.

- **A genuinely clean license story.** gamut deliberately targets royalty-free formats and
  ships under MIT OR Apache-2.0 — no GPL/LGPL reach. Patent-unencumbered formats deserve
  permissively-licensed code to match.

  The one deliberate exception to both of the above is `gamut-jxl`'s optional `encode` feature,
  which statically builds the BSD-3-Clause libjxl reference encoder (plus its permissive bundled
  highway, brotli and skcms) and so needs cmake and a C++ toolchain at build time — a
  maintainer-approved departure documented in that crate. Its pure-Rust `decode` feature keeps
  JPEG XL C-free on every target, and every other codec links no C at all.

- **Encoder-first, size-first — the gap the Rust ecosystem actually has.** Most Rust imaging
  is decode-only and hands the hard encoders off to C wrappers. gamut is built the other way
  round: encoders are the product, and the thing we optimize is *output bytes at a given
  quality and speed*, with the space/time tradeoff documented per format. That's the number
  that lands on storage and bandwidth bills. Decoders follow where the Rust ecosystem lacks a
  strong, feature-complete implementation.

- **One readable codebase, shared primitives.** Implemented clean-slate from the official specs
  in `references/`, with color management, DSP, bitstream, and container parsing in shared
  crates (`gamut-color`, `gamut-dsp`, `gamut-bitstream`, `gamut-isobmff`, `gamut-riff`) rather
  than re-implemented inside each separate C library: consistent behavior across formats, one
  API, one place to fix a color bug, and code you can actually audit, fork, and experiment with —
  not decades of accreted platform `#ifdef`s and inline assembly. You compile in only the
  formats you enable via Cargo features.

### Author's Remarks

`gamut` started in 2026, when there were no robust, well-tested Rust implementations of many
image and colour primitives. We want it to be the de facto, permissively licensed choice for most
colour and image needs, primarily for professional use cases. Building an ecosystem of crates this
size without commercial backing is feasible because image formats are spec-driven, and because LLM
agents, used correctly, speed the work up considerably.

### Scope

**Formats.** gamut covers the royalty-free still-image formats with the best size-versus-
compatibility tradeoff today:

- **AVIF**, **WebP**, **JPEG** and **PNG** — clean-slate encoders and decoders. AVIF decode parses
  the container natively and hands the AV1 codestream to a pluggable backend, since `gamut-av1`
  does not reconstruct samples yet.
- **JPEG XL** — uniquely, a wrapper over the format's reference implementations (libjxl for
  encode, the pure-Rust jxl-rs for decode) rather than clean-slate, a maintainer decision
  documented in `gamut-jxl`.
- **TIFF 6.0** and **DNG 1.7.1** — encoder + decoder pairs on the shared `gamut-ifd` container
  core, DNG as a TIFF/EP raw profile. TIFF's YCbCr/Lab photometrics and JPEG-in-TIFF are deferred.
- **HEIC** — decode-only by charter: the HEIF container is parsed here, the HEVC bitstream is left
  to a pluggable backend.

`gamut-av2` and `gamut-vvc` are still placeholders, and may move or be dropped as the focus
sharpens.

**Metadata and colour.** Shared EXIF, XMP, ICC and IPTC crates, unified by the `gamut-metadata`
facade, are all at 1.0 or later. EXIF, XMP and IPTC are gated differentially against exiv2 (which
bundles Adobe's XMPCore), ICC against Little-CMS. `gamut-cmm` adds the ICC transform engine over
the profiles `gamut-icc` parses, conformance-gated against Little-CMS though still well behind it
on throughput. The long-term goal is exiftool-class coverage; the remaining gap is depth —
per-vendor MakerNote payloads and tag breadth beyond the standard dictionary — tracked in issue
#416. Which format crates wire the facade is listed in its
[consumer integration](crates/gamut-metadata/README.md#consumer-integration) section, and which
MakerNote vendors `gamut-exif` decodes in its [scope](crates/gamut-exif/README.md#scope).

**gamut is image-first.** Even where a format's codec (AV1, AV2, VVC, HEVC) is fundamentally a
video codec, gamut implements only the intra-frame, still-image subset those formats use — no
inter-frame prediction, no motion compensation, no video sequences. The video-named codec
crates (`gamut-av1`, `gamut-av2`, `gamut-vvc`, and HEVC-based `gamut-heic`) are still-image
codecs, not video codecs, and gamut will not grow video primitives. This extends to
container-level multi-frame sequences: WebP animation (`ANIM`/`ANMF`) is out of scope even
though each frame is an independent keyframe — only single still images are supported.

## Usage

Add the umbrella `gamut` crate and enable only the formats you need:

```sh
cargo add gamut -F png,avif,webp
```

Every codec speaks the same `EncodeImage`/`DecodeImage` traits over typed pixel buffers, so one
decoded image feeds any encoder — here a PNG transcoded to lossy AVIF and lossless WebP:

```rust
use gamut::avif::AvifEncoder;
use gamut::core::{DecodeImage, EncodeImage, ImageBuf, Rgba8};
use gamut::png::PngDecoder;
use gamut::webp::WebpEncoder;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let png = std::fs::read("input.png")?;

    // Decode into the pixel layout you ask for. Widening (grey, RGB, palette → RGBA) is
    // automatic; a lossy change such as 16 → 8 bits is an error unless you opt in.
    let image: ImageBuf<Rgba8> = PngDecoder::new().decode_image(&png)?;

    let avif = AvifEncoder::lossy(80).encode_to_vec(image.as_ref())?;
    let webp = WebpEncoder::lossless().encode_to_vec(image.as_ref())?;

    std::fs::write("output.avif", &avif)?;
    std::fs::write("output.webp", &webp)?;
    Ok(())
}
```

The umbrella has no default features, so a bare dependency compiles only `gamut-core`; the
feature-to-crate map is in [its README](crates/gamut/README.md#features). The `primitives` feature
additionally re-exports the shared building blocks (`gamut::color` / `gamut::dsp` /
`gamut::bitstream`) for tooling, and `all` enables everything.

## Crates

Rows describe each crate as it stands in this tree, not what crates.io currently serves, and state
no version (`mise run versions` lists them). **Stable** means 1.0 or later: under
[SemVer](#versioning) the public surface changes incompatibly only in a new major release, while a
0.x crate may break on any minor.

| Crate             | Purpose                                                                | Status                                 |
| ----------------- | ---------------------------------------------------------------------- | -------------------------------------- |
| `gamut`           | Umbrella crate re-exporting the library crates behind Cargo features; the feature-to-crate map is in [its README](crates/gamut/README.md#features) | implemented |
| `gamut-core`      | Core traits (`EncodeImage`/`DecodeImage`), image buffers, dimensions, errors, `convert` | stable at its current major; pixel conversion added by #268 |
| `gamut-color`     | Coded-plane bit depths and chroma subsampling, CICP code points and planar buffers, the `ycbcr` matrixing layer (H.273, plus the libwebp-exact BT.601 one VP8 needs), and the `f64` colour science (transfer, Lab/OKLab, XYB, `matrix`/`linalg`, gamut map, CCT, profile) | stable (1.0 under #179); the colour science is Tier-1 `f64`, not bit-reproducible |
| `gamut-dsp`       | Shared DSP kernels: AV1 DCT/ADST/identity/WHT, the JPEG 8×8 forward/inverse DCT, the reference decoders' chroma upsampling, quantization rounding and pixel clamps | stable (1.0 under #192)             |
| `gamut-bitstream` | Bit readers/writers, LEB128, the AV1 §8.2 symbol (arithmetic) coder, MSB-first sample packing, the T.81 Annex K.2 Huffman table builder and Annex C canonical codes, a length-limited Huffman code-length builder and RFC 1951 canonical codes | pre-1.0, and stays 0.x by design: its low-level primitives are expected to churn as codecs need them, so no 1.0 is planned; the ANS and JPEG XL prefix coders are not implemented |
| `gamut-tonemap`   | Tone-mapping curves for HDR→SDR: the `ToneCurve` trait over the Linear, Clamp, Exposure, Reinhard, ReinhardExtended, ACES, Hable and Drago operators, and the RGB-aware `ToneMapper` (`CurveToneMapper`: per-channel, luminance or max-RGB application between explicit peaks, with ffmpeg-style desaturation) | stable (#188) |
| `gamut-codec-abi` | Shared codestream-backend seam: `repr(C)` vtables, their object-safe Rust twins, and the one `Registry` (behind its `std` feature) every host crate that offers a seam keeps its backends in, implementing the fallback contract | implemented |
| `gamut-isobmff`   | ISOBMFF container utilities (AVIF, HEIC)                               | stable; structure only, codestream carried opaquely |
| `gamut-riff`      | RIFF container utilities (WebP)                                        | stable (#186)                      |
| `gamut-av1`       | AV1 still-image (intra-frame) encoder + header-layer decoder — the codec layer beneath AVIF | encoder: lossless and lossy intra keyframes; decoder (default Cargo feature `decode`): OBU, header and tile-group framing of 8-bit 4:4:4 intra frames only, no sample reconstruction yet (#259) |
| `gamut-av2`       | AV2 still-image (intra-frame) encoder/decoder — AV1's successor        | placeholder                            |
| `gamut-avif`      | AVIF encoder + container decoder — AV1 still frames in ISOBMFF         | encoder (8/10/12-bit) + `AvifDecoder` over `gamut-heif`; AV1 codestream decode via the `Av1CodestreamDecoder` seam |
| `gamut-jxl`       | JPEG XL encoder (libjxl wrap) + decoder (pure-Rust jxl-rs)             | encoder + decoder (#243)               |
| `gamut-jxl-sys`   | Static libjxl 0.12.0 FFI declarations — native core of `gamut-jxl` encode | encoder backend (#243)                 |
| `gamut-jpeg`      | JPEG-1 (ISO/IEC 10918-1) encoder + decoder — baseline & progressive; the jpegli-style XYB colour mode encodes natively and decodes to sRGB through `gamut-cmm` behind the `cmm` feature | encoder + decoder (#28, P1–P17) |
| `gamut-webp`      | WebP (intra-frame VP8/VP8L) encoder/decoder, with a public `backend` seam for an alternate VP8/VP8L codestream implementation | implemented VP8 + VP8L (+alpha, metadata, effort/near-lossless) |
| `gamut-heic`      | HEIC still-image **decoder** — the HEVC binding over `gamut-heif`, HEVC via a pluggable backend | decode-only container (S1–S7); no encoder, by charter |
| `gamut-heif`      | Shared HEIF/MIAF still-image layer: item model, derivation walk, decode limits, planar-to-RGBA presentation and the C2PA summary, generic over a per-codec `Codestream` binding | new (#1078); `gamut-avif` (#1085) and `gamut-heic` (#1086) bind onto it |
| `gamut-vvc`       | VVC (H.266) still-image (intra) encoder/decoder                        | placeholder                            |
| `gamut-ifd`       | TIFF/IFD container core (byte order, field types, IFD I/O) — EXIF+TIFF | stable (byte completeness #263); BigTIFF behind Cargo feature `bigtiff` |
| `gamut-exif`      | EXIF (Exif 3.0) metadata parser/serializer — built on `gamut-ifd`        | stable (1.0 was #194); MakerNote preserved verbatim, with per-vendor decoding listed in its README |
| `gamut-icc`       | ICC color profile (ICC.1:2022) parser/serializer                      | stable (1.0 was #180)               |
| `gamut-cmm`       | ICC colour management module (transform engine) over `gamut-icc` profiles | P1–P8 complete per its STATUS.md — P1–P7 are epic #323, while P8 (pipeline optimization) is #372, which #323 lists out of scope |
| `gamut-xmp`       | XMP (RDF/XML) metadata parser/serializer                              | stable (1.0 was #189); canonical serializer, UTF-8 packets only |
| `gamut-iptc`      | IPTC photo metadata (IIM + Core/Extension over XMP)                    | stable (1.0 was #182); full IIM 4.2 tag table (#422) and typed models for every IPTC Extension structure (#422, #538) |
| `gamut-metadata`  | Unified metadata facade over EXIF/XMP/ICC/IPTC (extract + embed)       | stable; orchestration only. A C2PA manifest store is extracted verbatim but never re-embedded: embedding drops it, or refuses, because its hard binding cannot survive the rewrite |
| `gamut-tiff`      | TIFF 6.0 encoder/decoder — on the shared `gamut-ifd` container core     | stable (1.0 was #107); YCbCr/Lab and JPEG-in-TIFF deferred |
| `gamut-dng`       | DNG 1.7.1 raw encoder + decoder — a TIFF/EP profile over `gamut-ifd`   | encoder + decoder (1.0 was #109), Adobe DNG SDK-gated |
| `gamut-deflate`   | DEFLATE/zlib encoder (zopfli-class) — the compression under gamut's own writers | encoder (#195); no in-house decoder by design — optional `inflate` feature bounds `miniz_oxide` for workspace decoders |
| `gamut-png`       | PNG (W3C 3rd edition) encoder + decoder, over the still-image subset   | encoder (#24) + decoder (#249); APNG out of scope, so an animated PNG decodes as its default image |
| `gamut-cli`       | `gamut` CLI sandbox: `convert` (decode PNG/JPEG/PPM/WebP/JXL, re-encode AVIF/WebP/TIFF/PNG/JXL/JPEG), `inspect` (TIFF/DNG/PNG byte accounting, plus a C2PA manifest-store report for those and HEIC/AVIF/WebP), `icc`, `isobmff`, `av1`, and the `color`, `dsp` and `bitstream` primitive inspectors | ready for use |
| `gamut-wasm`      | WebAssembly bindings                                                   | placeholder                            |
| `gamut-ffi`       | C-compatible FFI bindings                                              | provider boundary shipped (#280); consumer boundary shipped for AVIF encode and container parse (#666), the other formats pending (#242) |

## Prerequisites

Install [rustup](https://rustup.rs) and [mise](https://mise.jdx.dev). After cloning, run
`mise trust && mise install` to provision the rest of the dev tooling; `mise tasks` lists what
you can run.

## Minimum Supported Rust Version (MSRV)

The MSRV is **Rust 1.92** (stable), on **edition 2024**, declared once as `rust-version` in the
root `[workspace.package]` and inherited by every crate; CI compiles the libraries on it.
Day-to-day development tracks the latest stable. Raising the MSRV is a deliberate,
SemVer-relevant change, and a 0.x crate takes it in a minor release.

## Versioning

Every crate is versioned **independently** under [SemVer](https://semver.org), from its own
changes alone, so versions drift apart across the workspace; the umbrella `gamut` crate's version
serves as the headline project version.

The lifecycle runs off commit history. Commits are
[Conventional Commits](https://www.conventionalcommits.org), enforced by the git hooks and on every
pull request. Pull requests gate on fast static checks; the full test suite, the 80% coverage gate
and mutation testing run on `master` after merge. Once CI passes there,
[release-plz](https://release-plz.dev) opens a release pull request with each changed crate's
version bump and `CHANGELOG.md`, and merging it publishes those crates to crates.io in dependency
order, tagged `<crate>-v<version>`.

## AI Policy

Vibe-coded contributions are welcome. AI-assisted PRs are accepted as long as you
personally vouch for the work — you've read it, you understand it, and you stand behind it
as if you'd written every line — and it matches the project's existing code style and
requirements. The CI and git hooks loosely enforce the bare minimum; meeting that bar is
necessary but not sufficient. Review your output before opening a PR.

## License

Licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE) at your option.
