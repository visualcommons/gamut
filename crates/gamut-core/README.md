# gamut-core

`gamut-core` defines the shared vocabulary every gamut codec is built on: the
[`EncodeImage`]/[`DecodeImage`] traits, the branded [`ImageRef`]/[`ImageBuf`] pixel buffers, the
[`Pixel`] marker types, the [`Dimensions`] type, and the common [`Error`] surface.

## Goals

Part of the [gamut](../../README.md) workspace, this crate exists to provide a single, format-neutral
foundation so that:

- **Every codec speaks one API.** A caller drives `gamut-avif`, `gamut-webp`, or any future format
  through the same [`EncodeImage`]/[`DecodeImage`] traits and gets back the same [`Error`] type,
  regardless of format. A codec implements them only for the pixel layouts it supports, so handing
  it an unsupported format is a compile error, not a runtime check.
- **The dependency graph stays acyclic.** This crate has **no** dependency on any other workspace
  crate, so no format crate is below it; most workspace crates depend on it, never the other way
  around.
- **Errors are typed, never panicking.** [`Error`] is opaque and classified by
  [`ErrorKind`] (`#[non_exhaustive]`), so new kinds land without a breaking change and library
  paths return errors instead of `unwrap()`/`expect()`.
- **Hot paths stay allocation-conscious.** [`EncodeImage::encode_image`] appends to a caller-provided
  `&mut Vec<u8>` rather than allocating a fresh buffer, so callers can reuse scratch space.
- **Memory-safe end to end.** `#![forbid(unsafe_code)]`.

## Usage

```rust
use gamut_core::{Dimensions, EncodeImage, ImageRef, Rgb8, Result};

struct MyEncoder;

impl EncodeImage<Rgb8> for MyEncoder {
    fn encode_image(&self, image: ImageRef<'_, Rgb8>, out: &mut Vec<u8>) -> Result<usize> {
        // `image` is already validated: its sample count matches its dimensions.
        let start = out.len();
        out.extend_from_slice(image.as_samples()); // a real codec would compress here
        Ok(out.len() - start)
    }
}

// Build a validated, branded RGB image and hand it to the encoder.
let dims = Dimensions::new(2, 2).unwrap();
let rgb = vec![0u8; dims.sample_count(3).unwrap()];
let mut out = Vec::new();
MyEncoder
    .encode_image(ImageRef::<Rgb8>::new(&rgb, dims).unwrap(), &mut out)
    .unwrap();
```

Enable the optional `serde` feature to serialize and deserialize [`ColorModel`] by its Rust variant
name (for example, `ColorModel::Rgba` is `"Rgba"` in JSON). The feature is disabled by default.

## Status

Stable foundation whose public surface changes incompatibly only in a new major release: the [`EncodeImage`] / [`DecodeImage`] traits, the
branded [`ImageRef`] / [`ImageBuf`] buffers and [`Pixel`] markers, [`Dimensions`], [`Result`], and
the structured [`Error`] / [`ErrorKind`] surface. Every codec in the workspace is built on it.

The surface is intentionally minimal. The following are deliberate design decisions, not gaps:

- **Interleaved `u8` / `u16` layouts only.** [`Sample`] is sealed over `u8` and `u16`. Planar
  layouts and coded bit depth are codec concerns and live in `gamut-color` (`Planar8`, `BitDepth`),
  which builds on these types.
- **Open vs. sealed.** [`ErrorKind`] and `ColorModel` are `#[non_exhaustive]`. [`Pixel`] /
  [`Sample`] are sealed: the supported layouts are closed and defined only here.
- **One opaque error, matched by kind.** [`Error`] keeps its representation private: callers match
  on `Error::kind` and read `origin`, `byte_offset`, `detail`, `static_message` and `source`. It is
  built with `Error::invalid_input`, `Error::unsupported` or `From<std::io::Error>`, and is one
  pointer wide, so `Result<T>` stays small on every success path; the allocation happens only when
  an error is constructed.
- **Dimensions are never empty.** [`Dimensions`] has private fields and two constructors,
  `Dimensions::new`, which rejects a zero width or height, and the infallible `const`
  `Dimensions::from_nonzero` over `NonZeroU32`; `len == width * height * channels` is
  then validated once, at [`ImageRef::new`] / [`ImageBuf::new`], so codecs never re-check.
- **One vocabulary for what every format carries.** `DecodeLimits`, `Cicp`, `Orientation`,
  `Palette`, `RawMetadata` and the `c2pa` constants are defined here once, for the format crates to
  share.

Additive growth (new `ErrorKind`/`ColorModel` variants, more buffer helpers) stays
backward-compatible; removing or changing existing items would not.

## License

MIT or Apache-2.0, at your option — see the [workspace README](../../README.md#license).
