# gamut-webp

`gamut-webp` is a pure-Rust WebP **encoder and decoder** — a VP8/VP8L still-image bitstream wrapped
in a RIFF container.

## Goals

Part of the [gamut](../../README.md) workspace, this crate provides WebP encoding (and, unusually for
the encoder-first workspace, decoding) that is:

- **Memory-safe on hostile input.** `#![forbid(unsafe_code)]`, deleting the memory-corruption bug
  class behind libwebp's CVE record (e.g. the zero-click, wormable CVE-2023-4863). Because every
  WebP decoder in the Rust ecosystem ultimately wraps libwebp, a clean-slate safe decoder is worth
  carrying here.
- **Clean-slate from the spec.** Implemented directly from the VP8 / VP8L bitstream specs (see
  [`../../references/`](../../references)) rather than wrapping libwebp.
- **Layered on shared crates.** The container comes from [`gamut-riff`](../gamut-riff); color
  handling and pixel clamping from [`gamut-color`](../gamut-color); integer-DSP helpers from
  [`gamut-dsp`](../gamut-dsp). (The VP8 boolean coder and VP8L LSB-first bit I/O are codec-specific
  and live in-crate.)
- **Buildable anywhere `cargo` is.** No C, no nasm — cross-compiles cleanly (wasm32, aarch64, musl).
  (The differential test suite is the one exception: it builds libwebp via `libwebp-sys` as a
  dev-dependency, so running `cargo test` needs a C toolchain. The shipped library does not.)

WebP is one of gamut's three initial focus formats (alongside AVIF and JPEG).

## Usage

The public API follows the same shape as [`gamut-avif`](../gamut-avif): a `WebpEncoder` implementing
[`gamut_core::EncodeImage`] and a `WebpDecoder` implementing [`gamut_core::DecodeImage`], both
reachable through the umbrella crate's `webp` feature. **Both codecs are fully implemented**, taking
a typed `ImageRef` and returning a typed `ImageBuf`, for RGB and RGBA:

- **VP8L lossless** — `WebpEncoder::lossless` emits a conformant bit-exact stream; `WebpDecoder`
  decodes any conformant VP8L stream.
- **VP8 lossy** — `WebpEncoder::lossy(quality)` runs the full intra key-frame codec (DC/V/H/TM and
  per-4×4 B_PRED prediction, the simple and normal loop filters with per-macroblock deltas,
  segmentation, token partitions, and skip); `WebpDecoder` decodes any conformant key frame.
- **Alpha** — `EncodeImage<Rgba8>` / `DecodeImage<Rgba8>`. A transparent lossy image uses the extended
  (`VP8X`) format with an `ALPH` chunk (raw or lossless); an opaque one stays a simple file.
- **Compression effort** — `WebpEncoder::with_effort(Effort)` is libwebp's `method` dial, `0..=6`,
  defaulting to `4`. It applies to both codestreams and never changes what the format guarantees:
  lossless stays bit-exact at every rung, lossy keeps its quality target. The lossless rungs race
  candidate encodings (transform chain, palette ordering, colour-cache size, entropy grouping, LZ77
  depth) and keep the smallest — each rung's candidates are a superset of the rung below's, so
  **size is non-increasing in effort by construction**. The lossy rungs add 4×4 prediction search,
  coefficient probabilities derived from the coded frame, a measured skip probability, and a
  dead-zone quantizer. On a 256×256 gradient, lossless costs ~9 ms at rung 0, ~156 ms at the
  default and ~290 ms at rung 6; lossy is far flatter (~2.4 ms against ~3.3 ms). Rungs 0–2 are the
  escape hatch when encode time matters more than bytes.
- **Near-lossless** — `WebpEncoder::with_near_lossless(Some(NearLossless::new(n)?))` rounds the
  source's colour channels to a coarser grid before lossless coding, on libwebp's `0..=99` strength
  scale. The stream stays bit-exact — to the quantized image. Red/green/blue move by at most
  `NearLossless::max_deviation()`; **alpha is never touched**. Turning it on can never enlarge a
  file: the encoder codes both ways and keeps the smaller.
- **Embedded metadata** — `WebpEncoder::with_icc_profile` / `with_exif` / `with_xmp` embed the `ICCP`,
  `EXIF`, and `XMP ` chunks; the `gamut_webp::metadata` free function reads them back out of any WebP
  file without decoding pixels. Payloads are carried **verbatim** — never parsed or reserialized — so
  they can be borrowed straight into [`gamut-metadata`](../gamut-metadata)'s `MetadataBlock`.
  Embedding promotes a simple file to the extended format, derives the `VP8X` feature flags from the
  chunks present, and emits everything in the spec's canonical order.

### Pluggable codestream backends

The container and the coded picture are separable. `WebpDecoder::push_backend` /
`WebpEncoder::push_backend` install a `WebpCodestreamDecoder` / `WebpCodestreamEncoder` — one trait
pair, discriminated by `WebpCodestream` (`Vp8` or `Vp8l`) — that handles the raw `VP8 ` / `VP8L`
chunk payload: a stateless-V4L2-style hardware VP8 decoder, say, or libwebp as an alternate VP8L
software path. Backends are tried in **push order**; `supports() == false` is the only fall-through;
the crate's own `vp8`/`vp8l` codecs are the implicit tails, so the default output is unchanged.
Backends written against the shared [`gamut-codec-abi`](../gamut-codec-abi) seam plug in through
`AbiDecoderBackend` / `AbiEncoderBackend`. `ALPH` alpha stays container-side and never crosses the
seam.

## Status

The intra-frame still-image surface and its milestones (M0 VP8L lossless → M1 VP8L full → M2 VP8
lossy → M3 extended container + alpha) are tracked component-by-component in [`STATUS.md`](STATUS.md).
Every component is validated against libwebp as an oracle via `libwebp-sys`, **bit-exact in both
directions** (gamut↔libwebp, at the YUV-plane level for lossy), backed by internal forward/inverse
round-trips, the in-crate decoder, and a malformed-input robustness corpus.

**Non-core feature paths** are decided in [`STATUS.md`](STATUS.md#scope-decisions--non-core-feature-paths):
alpha/transparency (`VP8X` + `ALPH`) and color/metadata chunks (`ICCP` ICC profiles, `EXIF`, `XMP `)
are **in scope** — embedded on encode and preserved on decode. Animation (`ANIM`/`ANMF`) is **out of
scope** under the image-first charter (each frame is an independent keyframe, but multi-frame
sequences don't fit the single-image API); its chunks are tracked only for container completeness.

## License

Licensed under either of MIT or Apache-2.0 at your option.
