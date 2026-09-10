# gamut-avif

`gamut-avif` is a pure-Rust, memory-safe AVIF encoder — and AVIF **container decoder** — that
wraps AV1 intra-frame bitstreams in an ISOBMFF/MIAF container.

This is the high-level crate most users want: give it pixels, get a complete `.avif` file — or
give it a `.avif` file and a codestream decoder, get pixels. It is orchestration only —
[`gamut-av1`](../gamut-av1) does the AV1 coding and [`gamut-isobmff`](../gamut-isobmff)
reads/writes the container; on the decode side the AV1 codestream itself is supplied through the
pluggable `Av1StillDecoder` seam (a platform hardware decoder, dav1d, …) until the workspace's
pure-Rust AV1 decoder lands.

## Goals

Part of the [gamut](../../README.md) workspace, this crate exists to provide AVIF **encoding** that
is:

- **Memory-safe on hostile input.** `#![forbid(unsafe_code)]` end to end — the entire encode and
  container path is safe Rust, deleting the spatial/temporal memory-corruption bug class that has
  repeatedly bitten the C image codecs.
- **Buildable anywhere `cargo` is.** No C, no autotools/CMake, no nasm — just Rust, so it
  cross-compiles cleanly (wasm32, aarch64, musl) for serverless/edge image optimization.
- **Encoder-first and size-first.** The product is the encoder and the bytes it emits; the
  space/time tradeoff of each mode is documented as it lands.
- **Clean-slate from the official specs.** Implemented directly from the AV1 Bitstream &
  Decoding Process Specification and the AVIF / AV1-ISOBMFF bindings (see `../../references/`), so
  it is auditable and forkable rather than a wrapper over libaom/libavif.
- **Permissively licensed** (MIT OR Apache-2.0), matching the royalty-free AV1/AVIF formats.

It builds on the workspace's shared primitives: [`gamut-color`](../gamut-color) (pixel formats /
CICP), [`gamut-dsp`](../gamut-dsp) (transforms), [`gamut-bitstream`](../gamut-bitstream) (bit
writer + AV1 symbol coder), [`gamut-av1`](../gamut-av1) (the AV1 keyframe encoder), and
[`gamut-isobmff`](../gamut-isobmff) (the container).

## Usage

```rust
use gamut_avif::AvifEncoder;
use gamut_core::{Dimensions, EncodeImage, ImageRef, Rgb8};

let (width, height) = (64u32, 64);
let rgb: Vec<u8> = vec![0; (width * height * 3) as usize]; // 8-bit interleaved RGB
let dims = Dimensions { width, height };
let image = ImageRef::<Rgb8>::new(&rgb, dims).expect("buffer length matches dimensions");

// Lossless by default. `AvifEncoder::lossy(quality)` (0..=100) trades fidelity for a smaller
// file; `with_rotation` / `with_mirror` add display-orientation transforms.
let avif = AvifEncoder::new().encode_to_vec(image).expect("encode");
std::fs::write("out.avif", &avif).unwrap();
```

`AvifEncoder` implements [`gamut_core::EncodeImage`] for `Rgb8`, `Rgba8`, `Gray8`, `Rgb16` and
`Rgba16`, so the input is a typed [`gamut_core::ImageRef`] and handing it an unsupported pixel
layout is a compile error. `Rgba8`/`Rgba16` split into a 4:4:4 colour item plus a monochrome **alpha
auxiliary item** (`auxC`/`auxl`, with `prem` when `with_premultiplied_alpha(true)` declares the
colour premultiplied); `Gray8` is a single monochrome item rather than R=G=B replication. A file
carrying a monochrome item signals only the general AVIF brands — the Advanced Profile brand `MA1A`
requires every image item to be AV1 High Profile (AVIF v1.2.0 §8.3).

The 16-bit inputs carry samples on `gamut-core`'s full 16-bit scale, while AV1 codes 8, 10 or 12.
`with_bit_depth` picks the coded depth (10 or 12, default **12**) and the encoder narrows by
**truncation**, so a lossless encode is bit-exact *at the coded depth*, not to the 16-bit input.
`with_chroma` reaches the `Rgb16` path too, so a high-bit-depth item can be 4:2:2 or 4:2:0 — the
chroma is averaged *after* narrowing, with the same box filter the 8-bit path uses. As on that path
the identity matrix stays 4:4:4 (AV1 §6.4.2), and `AvifEncoder::lossy` defaults to 4:2:0:

```rust,ignore
let avif = AvifEncoder::new()
    .with_bit_depth(BitDepth::Ten)
    .encode_to_vec(ImageRef::<Rgb16>::new(&rgb16, dims)?)?;
```

Decoding (issue #250): `AvifContainer::parse` gives a byte-accounting view plus the role-typed
`AvifImage` lens (primary item, alpha/depth auxiliaries, thumbnails, Exif/XMP, grid/overlay,
typed `av1C` and OBU layers); `decode_item_planar` hands each coded item's `Av1Config` + OBU
payload to your `Av1StillDecoder` and reassembles the result, while `decode_primary_rgba8` adds
colour conversion, alpha merge, and the `clap`/`irot`/`imir` transforms. See the crate docs for a
worked example.

## Status

**v1 surface.** The encoder produces **lossless** (the default) and **lossy**
(`AvifEncoder::lossy(quality)`) still images: 8-bit RGB mapped to AV1 planes and wrapped as a
single `av01` item in a conformant MIAF/AVIF container. Lossless codes the identity matrix at
4:4:4, so its output is bit-exact to the input; lossy codes **BT.709 YCbCr at 4:2:0** by default —
the luma–chroma decorrelation is worth a large fraction of the bitrate, and 4:2:0 is AV1 **Main**
profile, the only one many hardware still-image decoders accept — with BT.601 / BT.2020-NCL,
studio range and 4:4:4 / 4:2:2 selectable via `with_matrix` / `with_color_range` / `with_chroma`.
4:2:2 is AV1 Professional profile, which matches no AVIF profile brand, and AV1 forbids
taller-than-wide partitions there — so it costs the encoder half its rectangular partition set and
is offered for pipelines that need it rather than as a default. Lossy trades fidelity for size on a
`0..=100` quality scale (higher = closer to the source; the `quality → base_q_idx` mapping and its
silent clamp above 100 are a frozen v1 contract, defined in
[`references/avif`](../../references/avif/README.md)). `irot`/`imir` display orientation is
supported.

**Colour and metadata.** CICP colour primaries and transfer characteristics are selectable with
`with_primaries` / `with_transfer` — tags, not conversions, so unlike the matrix and range knobs
they apply on the lossless path too. `with_icc_profile` embeds an ICC profile as a `colr` box of
type `prof`, kept alongside the CICP box rather than replacing it. `with_exif` (a bare TIFF stream;
the encoder adds HEIF's 4-byte offset prefix) and `with_xmp` attach metadata items carrying a
`cdsc` reference to the primary image. All five carry their payloads verbatim and leave the
codestream untouched; libavif reads every one of them back byte-for-byte.

**Content credentials (C2PA).** A C2PA manifest store cannot be handed to an encoder complete —
its hard binding digests the finished file — so the encoder reserves rather than receives:
`with_c2pa_reserved(len)` writes a top-level `uuid` `ContentProvenanceBox` (C2PA 2.4 §A.5.1) after
`ftyp` around a slot of `len` zero bytes, and `encode_with_report` returns the same bytes
`encode_to_vec` would plus the slot's byte range, which a signer then patches in place; nothing
else in the file moves. `with_c2pa(bytes)` writes a store the caller has already computed over
this exact output. A `len` that could never hold a store — below the 8-byte JUMBF box header, or
beyond what a buffer holds — is refused by the encode that follows, since the builder itself cannot
fail. On read, `AvifContainer::c2pa_slot` reports the located slot's bytes, purpose and range, and
stays permissive where the writer is strict: a degenerate slot that is genuinely there is reported
as found. The store is opaque here — gamut locates and carries, it never validates — and the range
is for patching and byte accounting, not a hash exclusion range (BMFF assets bind by box path,
§18.6).

Output is verified against real decoders (`libavif`, `dav1d`, `libaom`), linked from vendored
`third_party/` submodules rather than system-installed binaries.

**Decode surface.** The container read + codestream handoff (issue #250, mirroring what
`gamut-heic` ships for HEIF): byte-accounting parse, the full item/property/derivation model, the
typed `av1C`/OBU layer with the AVIF still-image payload validation, planar decode with
grid/identity reassembly, and the RGBA presentation paths — 8-bit (`decode_primary_rgba8`) and
high-bit-depth (`decode_primary_rgba16`, 10/12-bit normalized to full-range 16-bit), with
identity / BT.601 / BT.709 / BT.2020-NCL / monochrome colour, alpha merge, overlay compositing,
`clap`/`irot`/`imir`. Validated differentially against
libavif + dav1d over the libavif conformance corpus (`tests/conformance.rs`).

Everything beyond is dispositioned in [STATUS.md](STATUS.md), row by row against the relevant
specs: **deferred, planned** features (10/12-bit, the HDR
metadata properties beyond CICP tagging, gain maps, layered/progressive images, the pure-Rust AV1
codestream decoder, the decoder backend registry, …) all land semver-minor on the frozen v1
surface, while image sequences/tracks and AV1 inter coding are **permanently out of scope** per
the image-first workspace charter.

## License

Licensed under either of MIT or Apache-2.0 at your option.
