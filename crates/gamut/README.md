# gamut

`gamut` is the umbrella crate for a collection of space-efficient image encoding libraries. It
re-exports the format-specific crates behind Cargo features, so a consumer compiles only the codecs
they need.

For the full "why gamut" rationale (memory safety, clean license story, WASM-first, encoder-first),
see the [workspace README](../../README.md).

## Goals

Part of the [gamut](../../README.md) workspace, this crate exists to be the **single entry point**
consumers depend on:

- **One dependency, opt-in codecs.** Add `gamut` and turn on only the formats you use; everything is
  feature-gated and `default = []`, so a bare dependency compiles just [`gamut-core`](../gamut-core).
- **A consistent API across formats.** [`gamut::core`] is always available and provides the shared
  [`Encoder`]/[`Decoder`] traits and the common `Error` type, regardless of which formats are
  enabled.
- **Tooling access to the primitives.** The `primitives` feature additionally re-exports the shared
  building blocks — `gamut::color`, `gamut::dsp`, `gamut::bitstream` — for inspection and sandbox use
  (this is what the [`gamut` CLI](../gamut-cli) is built on).

## Features

`default = []` — a bare dependency compiles only `gamut-core`.

| Feature      | Enables                                                                  |
| ------------ | ------------------------------------------------------------------------ |
| `avif`       | AVIF encoding (`gamut-avif` + `gamut-av1`)                               |
| `av1`        | Standalone AV1 still-image encoding (`gamut-av1`)                        |
| `webp`       | WebP VP8/VP8L encoder + decoder (`gamut-webp`)                           |
| `jxl`        | JPEG XL encoder (libjxl wrap) + decoder (`gamut-jxl`)                    |
| `heic`       | HEIC/HEIF decode-only container (`gamut-heic`)                           |
| `vvc`        | VVC / H.266 (`gamut-vvc`) — placeholder                                  |
| `av2`        | AV2 (`gamut-av2`) — placeholder                                          |
| `tiff`       | TIFF 6.0 encoder + decoder (`gamut-tiff`)                                |
| `dng`        | DNG 1.7.1 raw encoder + decoder (`gamut-dng`)                            |
| `png`        | PNG encoder + decoder (`gamut-png`)                                      |
| `jpeg`       | JPEG-1 (ITU-T T.81) codec (`gamut-jpeg`)                                 |
| `isobmff`    | ISOBMFF/HEIF still-image container primitive (`gamut-isobmff`)           |
| `metadata`   | Metadata facade + primitives (`gamut-metadata` + `exif`/`xmp`/`icc`/`iptc`/`ifd`) |
| `exif-describe` | `metadata` plus EXIF tag-value descriptions (`gamut_metadata::exif::describe`, via `gamut-metadata/exif-describe`) |
| `cmm`        | ICC colour management module over `gamut-icc` profiles (`gamut-cmm`)     |
| `tonemap`    | Scalar tone-mapping curves for HDR→SDR (`gamut-tonemap`)                 |
| `primitives` | Re-export `color` / `dsp` / `bitstream` for tooling                      |
| `codec-abi`  | Shared codestream-backend seam for backend implementors (`gamut-codec-abi`) |
| `all`        | Every feature above                                                      |

## Usage

```sh
cargo add gamut -F avif
```

```rust
use gamut::avif::AvifEncoder;
use gamut::core::{Dimensions, EncodeImage, ImageRef, Rgb8};

let dims = Dimensions::new(64, 64)?;
let rgb = vec![0u8; 64 * 64 * 3]; // 8-bit interleaved RGB

let mut out = Vec::new();
AvifEncoder::new().encode_image(ImageRef::<Rgb8>::new(&rgb, dims)?, &mut out)?;
# Ok::<(), gamut::core::Error>(())
```

## Status

`avif` and `av1` are functional for M0 lossless still images; the other format features currently
gate placeholder crates that return `Unsupported`. The implemented surface tracks
[`gamut-avif/STATUS.md`](../gamut-avif/STATUS.md). See the [workspace README](../../README.md) for the
per-crate status table.

## License

Licensed under either of MIT or Apache-2.0 at your option.
