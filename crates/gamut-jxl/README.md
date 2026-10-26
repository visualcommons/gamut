# gamut-jxl

`gamut-jxl` is a memory-safe **JPEG XL (JXL) encoder and decoder**: give it pixels, get a `.jxl`
stream; give it a `.jxl` stream, get pixels back.

It is the one crate in the [gamut](../../README.md) workspace that is **not** a clean-slate,
pure-Rust codec. JPEG XL is a large, evolving ISO/IEC 18181 standard with no reference-quality Rust
*encoder*, so gamut-jxl is a thin, safe layer over the format's own reference implementations —
libjxl for encode, jxl-rs for decode — rather than a from-scratch bitstream implementation. The
departure is deliberate and maintainer-confirmed; see [Why a wrapper](#why-a-wrapper) below.

## Architecture

Two independent halves. Each is the **built-in tail** of a pushable backend registry (issue #276),
and each is gated behind its own Cargo feature — which selects whether that tail is compiled in, not
whether the direction works:

- **Encode — the reference libjxl.** The `encode` feature wraps **libjxl v0.12.0**, the ISO/IEC
  18181 reference implementation (C++, BSD-3-Clause), statically linked through the
  [`gamut-jxl-sys`](../gamut-jxl-sys) `-sys` crate. libjxl's source is vendored and cmake-built by
  the BSD-3-Clause [`jpegxl-src`](https://crates.io/crates/jpegxl-src) crate (pinned `=0.12.0`),
  which bundles highway, brotli and **skcms** — so there is no lcms2 and no dynamic codec
  dependency beyond the platform C++ runtime (`libstdc++`/`libc++`). Building `encode` therefore
  needs **cmake and a C++ toolchain** at build time.
- **Decode — pure Rust.** The `decode` feature wraps the pure-Rust
  [`jxl` crate v0.4.3](https://crates.io/crates/jxl) (jxl-rs, the libjxl organisation's Rust
  decoder, BSD-3-Clause). It needs no C toolchain and compiles for every target, `wasm32`
  included.

The two are wired so the crate degrades gracefully by target: the encoder is compiled in for
`all(feature = "encode", any(not(target_arch = "wasm32"), target_os = "emscripten"))`. On
**`wasm32-unknown-emscripten`** the full encoder works — libjxl officially supports wasm via
emscripten, and `gamut-jxl-sys` builds it with the emsdk toolchain. On **`wasm32-unknown-unknown`**
(the wasm-bindgen/browser target) the crate builds as a **decode-only** codec with zero cmake in
the build graph — an upstream toolchain boundary (no C/C++ compiler targets that ABI), not a gamut
workaround.

## Why a wrapper

Every other gamut codec is implemented clean-slate from its spec, precisely so the encode/parse
path is auditable, forkable, and free of C. gamut-jxl departs from that on a maintainer decision
(issue [#243](https://github.com/justin13888/gamut/issues/243)) for reasons specific to JPEG XL:

- **A conformant JXL encoder is a multi-year effort.** VarDCT, Modular, the ANS/context-modelling
  entropy stack, XYB, splines/patches/noise — reproducing libjxl's rate/quality at reference
  fidelity is not a near-term clean-slate target.
- **libjxl is the ground truth.** It *is* the ISO/IEC 18181 reference implementation, so wrapping
  it is the only spec-faithful encode path — and it is BSD-3-Clause, matching gamut's permissive
  posture (unlike the GPL-3.0 `jpegxl-sys`/`jpegxl-rs` crates, which is why gamut ships its own
  `-sys` layer).
- **Safety matters most where untrusted data enters.** The *decoder* is the hostile-input surface —
  it chews on attacker-controlled bytes — and jxl-rs keeps that path 100% safe Rust. The encoder
  operates on the caller's own pixels, a far smaller attack surface, so accepting a vetted C++
  reference there is the right trade.

## Safety

The crate is `#![deny(unsafe_code)]` (not `forbid`, because the FFI needs a single exception): all
`unsafe` is confined to the one `ffi` module that drives libjxl through `gamut-jxl-sys`, which is
itself declarations-only (`#[repr(C)]` types and `extern "C"` signatures, no bodies). The **decoder
path contains no `unsafe` at all** — it is pure safe Rust end to end. So the surface that ingests
untrusted `.jxl` bytes has the memory-corruption bug class deleted, and the FFI that touches
libjxl is a small, reviewable island.

## Usage

Encode a lossless stream (the default), decode it straight back:

```rust
use gamut_core::{DecodeImage, Dimensions, EncodeImage, ImageBuf, ImageRef, Rgb8};
use gamut_jxl::{JxlDecoder, JxlEncoder};

let dims = Dimensions { width: 64, height: 64 };
let pixels: Vec<u8> = vec![0; (64 * 64 * 3) as usize]; // 8-bit interleaved RGB
let image = ImageRef::<Rgb8>::new(&pixels, dims).expect("buffer length matches dimensions");

// Lossless by default; the decoded image is bit-exact to the input.
let stream = JxlEncoder::lossless().encode_to_vec(image).expect("encode");
let decoded: ImageBuf<Rgb8> = JxlDecoder::new().decode_image(&stream).expect("decode");
assert_eq!(decoded.as_samples(), pixels.as_slice());
```

Encode a lossy stream with a chosen effort, coding tool and ISO BMFF container framing:

```rust
use gamut_core::{Dimensions, EncodeImage, ImageRef, Rgb8};
use gamut_jxl::{Container, Distance, Effort, JxlEncoder, ModularMode};

let dims = Dimensions { width: 64, height: 64 };
let pixels = vec![0u8; (64 * 64 * 3) as usize];
let image = ImageRef::<Rgb8>::new(&pixels, dims).expect("buffer length matches dimensions");

// `Distance::new` validates the Butteraugli target; 1.0 is "visually lossless".
let encoder = JxlEncoder::lossy(Distance::new(1.0).expect("valid distance"))
    .with_effort(Effort::Squirrel)        // libjxl effort 1..=10 (default = Squirrel, 7)
    .with_modular(ModularMode::Modular)   // pin the coding tool (default = Auto, libjxl chooses)
    .with_container(Container::IsoBmff);  // .jxl box framing (default = bare Codestream)
let stream = encoder.encode_to_vec(image).expect("encode");
```

Signal colour, orientation, and metadata (the encoder never converts pixels — it declares how they
are to be interpreted), or losslessly re-pack an existing JPEG:

```rust
use gamut_core::{Dimensions, EncodeImage, ImageRef, Rgb16};
use gamut_jxl::{ColorSpec, Container, JxlEncoder, Orientation};

let dims = Dimensions { width: 64, height: 64 };
let pixels = vec![0u16; (64 * 64 * 3) as usize];
let image = ImageRef::<Rgb16>::new(&pixels, dims).expect("buffer length matches dimensions");

// HDR-coded u16 samples signalled as BT.2100 PQ, displayed rotated, with an XMP packet.
let encoder = JxlEncoder::lossless()
    .with_color(ColorSpec::Pq)                   // also: LinearSrgb, Hlg, Icc(profile bytes)
    .with_orientation(Orientation::Rotate90Cw)   // the eight EXIF orientations
    .with_container(Container::IsoBmff)          // metadata boxes need the container
    .with_xmp(r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"/>"#);
let stream = encoder.encode_to_vec(image).expect("encode");

// jbrd: reversibly transcode a JPEG — the original .jpg is reconstructible bit-for-bit.
let jpeg: &[u8] = include_bytes!("tests/fixtures/tiny_baseline.jpg");
let mut jxl = Vec::new();
JxlEncoder::new().recompress_jpeg(jpeg, &mut jxl).expect("recompress");
```

`JxlEncoder` implements [`gamut_core::EncodeImage`] and `JxlDecoder` implements
[`gamut_core::DecodeImage`] for exactly eight pixel layouts — 8- and 16-bit **Gray**, **GrayAlpha**,
**RGB** and **RGBA** — so handing either an unsupported layout is a compile error. `lossy` takes a
validated [`Distance`] in `(0.0, 25.0]` (`0.0`, libjxl's lossless sentinel, is deliberately rejected
so lossless stays a distinct constructor); there is no `0..=100` *quality* dial, because distance is
JPEG XL's native one and a quality scale would only be a lossier spelling of it. On the decode side,
`JxlDecoder::embedded_icc_profile` surfaces the exact ICC bytes a stream embeds (`None` for
structured encodings like sRGB/PQ) without decoding pixels; pixel decoding applies no colour
transform (see [STATUS.md](STATUS.md) for the decode-side CMS deferral).

### Pluggable codestream backends

Neither half is a fixed implementation. The seam is the **bare JPEG XL codestream** (signature
`FF 0A`): `JxlEncoder::push_backend` / `JxlDecoder::push_backend` insert a `JxlCodestreamEncoder` /
`JxlCodestreamDecoder` — a platform codec, an alternate library, or a C backend reached through the
shared [`gamut-codec-abi`](../gamut-codec-abi) vtables via `AbiEncodeBackend` / `AbiDecodeBackend`.
Backends are tried in push order and the built-in wrapper is tried **last**, so pushing one is
additive. `supports() == false` (or a late `Error::Unsupported`) is the only fall-through; a backend
that accepts a job and then fails propagates its error rather than being silently retried.

This is also how the encode direction works where libjxl cannot be built: on
`wasm32-unknown-unknown` there is no tail, so a pushed backend *is* the encoder. With neither, the
direction returns `Error::Unsupported`.

Container-dependent features — ISO BMFF output, `with_exif`/`with_xmp`, and `recompress_jpeg` — are
written by libjxl today, so they are pinned to the built-in path by a host-side veto and never reach
a backend. Giving gamut-jxl its own container writer is a recorded follow-up ([STATUS.md](STATUS.md)).

## Features

| Feature  | Default | What it pulls in | Toolchain / targets |
| -------- | ------- | ---------------- | ------------------- |
| `encode` | yes | the built-in **libjxl 0.12.0** encode tail (`gamut-jxl-sys`) | needs **cmake + a C++ toolchain** (emsdk on `wasm32-unknown-emscripten`); inert — not a build error — on other `wasm32` targets |
| `decode` | yes | the built-in **jxl-rs** decode tail (the pure-Rust `jxl` crate), plus the header-only accessors `JxlDecoder::info` / `embedded_icc_profile` and the best-effort `DecodePartialImage` surface | pure safe Rust; builds **everywhere**, every `wasm32` target included |

Each feature means "**include the built-in tail**", not "enable the direction": without `encode`,
`JxlEncoder` still exists and still encodes through any backend pushed with `push_backend`; without
`decode`, likewise for `JxlDecoder`. With neither a tail nor a backend, that direction returns
`Error::Unsupported`.

For a C-toolchain-free build — a pure-Rust decoder, e.g. for `wasm32` or CI without cmake — depend
with `default-features = false, features = ["decode"]`. On `wasm32` the encoder is compiled out
automatically even with `encode` enabled, so the umbrella `gamut` crate's `jxl` feature yields a
decode-only JXL there by construction.

## Decode policies

The decoder converts between the stream's natural layout and the requested one where that is
lossless — grayscale expands to RGB, a missing alpha channel is padded opaque, a present-but-unwanted
alpha is dropped. It refuses to *guess*, returning `Error::Unsupported` rather than fabricating data:

- **Animation** — rejected (gamut is image-first; this is a deliberate policy, additive to relax).
- **Premultiplied (associated) alpha** — rejected (no un-premultiply is performed).
- **Colour-as-grayscale** — a colour stream cannot be decoded into a grayscale layout (no luminance
  is invented).

Oversized streams are bounded by a fixed pixel limit (`1 << 28` samples). A caller with a smaller
budget tightens it with `JxlDecoder::with_max_dimensions` / `with_max_image_bytes`, which are
unbounded by default and checked against the parsed header before any pixel buffer is sized
(and against a pushed backend's raster). The header-only `info` / `embedded_icc_profile` /
`metadata` accessors do not apply them. Truncated or malformed input returns a typed `Error`,
never a panic.

### Truncated and partial decode

`DecodeImage` rejects every truncated stream. `DecodePartialImage` is the opt-in alternative for a
partly-downloaded, still-arriving or damaged file — it returns the best-effort image plus a
`JxlPartialReport` carrying the completeness flag:

```rust,ignore
use gamut_jxl::{DecodePartialImage, JxlDecoder};
use gamut_core::{ImageBuf, Rgba8};

let (image, report): (ImageBuf<Rgba8>, _) = JxlDecoder::new().decode_partial_image(&bytes)?;
if !report.is_complete() {
    // `image` is a preview: correctly sized, but only partly drawn.
}
```

| Where the stream ran out | Result |
| ------------------------ | ------ |
| Before the image headers | `Error::InvalidInput` — no dimensions, so no buffer to return |
| Before the frame header | `JxlRender::HeaderOnly`: a zero-filled buffer at the declared size |
| Mid-frame | `JxlRender::BestEffort`: whatever groups arrived |
| Not truncated | `JxlRender::Complete`: byte-identical to `decode_image` |

**Best effort is a real qualifier.** A truncated *lossy* (VarDCT) stream renders groups with no
detail pass from the upsampled DC image, so it yields a full-size coarse preview that sharpens
toward the front of the stream; a truncated *lossless* (Modular) stream renders the groups that
arrived exactly and leaves the rest zero. But an image small enough to be coded as a single group
(roughly 256×256 and below) has no partially-decodable structure at all and comes back blank, and
some cut points are indistinguishable from corruption to the decoder and still return
`Error::InvalidInput`. Always check `report.is_complete()`; never assume pixels are present.

This path is always answered by the built-in jxl-rs tail — a pushed backend is not consulted,
because the shared codec-abi seam has no way to express a partial result. It relaxes *truncation
only*: the animation, premultiplied-alpha, colour-as-grayscale and pixel-limit refusals above are
unchanged.

## MSRV

Rust **1.92**, workspace-wide (inherited via `rust-version.workspace = true`; see the root
[README](../../README.md#minimum-supported-rust-version-msrv)).

## Licensing

gamut-jxl's own source is **MIT OR Apache-2.0** (the workspace default). Building the `encode`
feature statically links libjxl and its bundled libraries, each under its own permissive licence —
**libjxl** (BSD-3-Clause), **highway** (Apache-2.0), **brotli** (MIT), **skcms** (BSD-3-Clause) —
plus the platform C++ runtime. The `decode` feature adds the BSD-3-Clause `jxl` crate. Redistributing
a binary that links the encoder therefore means honouring those upstream notices; the details live in
[`gamut-jxl-sys/README.md`](../gamut-jxl-sys/README.md#licensing). A decode-only build
(`default-features = false, features = ["decode"]`) links no C.

## Status

The v1 surface — supported pixel layouts, lossless/lossy modes, container framing, the differential
oracle regime, and the full deferred/out-of-scope ledger (including the jxl-rs spec-coverage
analysis mandated by issue #243) — is dispositioned row by row in [STATUS.md](STATUS.md).

## License

Licensed under either of MIT or Apache-2.0 at your option.
