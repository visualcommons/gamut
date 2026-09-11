# gamut-png

`gamut-png` is a pure-Rust, research-grade **PNG codec**: a space-efficient encoder and a
spec-compliant decoder.

## Goals

Part of the [gamut](../../README.md) workspace, this crate reads and writes PNG (Portable Network
Graphics, W3C 3rd edition) images:

- **Space-efficient encoding** (issue #24). Built on [`gamut-deflate`](../gamut-deflate)'s
  zopfli-class compression, with adaptive scanline filtering and lossless bit-depth/palette
  reduction, targeting output sizes on par with the best PNG encoders. Encode time is a secondary
  concern at higher levels.
- **Spec-compliant decoding** (issue #249). Every colour type and bit depth, Adam7 interlacing,
  all five filters, and ancillary metadata surfaced as raw payloads (eXIf, inflated iCCP, XMP,
  tEXt/zTXt/iTXt) ready for `gamut_metadata::MetadataBlock`, plus parsed gAMA/cHRM/sRGB/cICP
  values. Hostile input is bounded: configurable dimension caps and byte budgets guard every
  allocation, and zlib bombs (IDAT or metadata) fail cleanly. Inflation uses `miniz_oxide`, the
  workspace's blessed decode-side inflate.
- **Memory-safe.** 100% safe Rust (`#![deny(unsafe_code)]`).

## Usage

```rust
use gamut_core::{DecodeImage, Dimensions, EncodeImage, ImageBuf, ImageRef, Rgb8};
use gamut_png::{PngDecoder, PngEncoder};

let image = ImageRef::<Rgb8>::new(&rgb, Dimensions::new(w, h)?)?;
let mut png = Vec::new();
PngEncoder::new().encode_image(image, &mut png)?;

// Typed decode: lossless widening only (e.g. greyscale or palette as RGBA).
let decoded: ImageBuf<Rgb8> = PngDecoder::new().decode_image(&png)?;

// Rich decode: native layout + palette/tRNS + raw metadata payloads.
let rich = PngDecoder::new().with_max_dimensions(8192, 8192).decode(&png)?;
```

The typed `DecodeImage<P>` implementations accept any file `P` can hold **losslessly** — palette
and tRNS expand to RGB(A), greyscale replicates into RGB, an opaque alpha channel can be added,
sub-byte greys scale exactly to 8 bits and 8-bit samples to 16 by ×257 — and refuse lossy
requests (16-bit files as 8-bit layouts, dropping alpha or transparency) with
`Error::Unsupported`. Format-agnostic *lossy* pixel conversion is deliberately out of scope here
and belongs in a shared gamut-core facility.

## Status

Built incrementally; each phase is conformance-checked against libpng (see [STATUS.md](STATUS.md)).
Encoder scope: all five colour types, bit depths 1/2/4/8/16, palette, the five scanline filters,
lossless reductions over every input layout (palette, grey, alpha drop, sub-byte grey packing,
16→8 demotion), the standard colour/text ancillary chunks, and embedded metadata
(eXIf/iCCP/iTXt). Decoder scope: everything above plus Adam7 **decoding** and decode limits.
Out of scope: Adam7 *encoding* and animation (APNG decodes as its default image).

## Validation

A differential oracle (`tooling/libpng-oracle`, a vendored static libpng) proves both directions:
libpng decodes the encoder's output pixel-exact, and a libpng *reference encoder* generates the
decoder's conformance fixtures (interlaced, sub-byte, forced-filter, metadata-laden) which both
decoders must read identically — no vendored image corpus. A hand-crafted malformed-input corpus
pins the rejection policy. Output size is measured against libpng at zlib level 9 by
`cargo bench -p gamut-png`, and **enforced** by `tests/size_contract.rs`, whose per-case budgets
each carry a written justification — a regression in the crate's reason to exist fails the build.
`STATUS.md` records the measured table; gamut is smaller than libpng-9 on every corpus entry, by
28-85% wherever a reduction or a filter choice applies and by 0.2% on the incompressible noise row,
where there is nothing for either encoder to find.

## License

Licensed under either of MIT or Apache-2.0 at your option.
