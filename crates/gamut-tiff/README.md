# gamut-tiff

`gamut-tiff` is a pure-Rust TIFF 6.0 (Tagged Image File Format) image **encoder and decoder**.

## Goals

Part of the [gamut](../../README.md) workspace, this crate provides TIFF reading and writing that
is:

- **Memory-safe on hostile input.** `#![forbid(unsafe_code)]` — TIFF's offset-driven structure is
  a classic source of parser exploits, so the decoder is built to be robust against malformed
  IFDs, offset loops, and truncation.
- **Clean-slate from the spec.** Implemented directly from the TIFF 6.0 specification
  ([`../../references/tiff/tiff6.pdf`](../../references/tiff)) rather than wrapping libtiff.
- **Container-native.** TIFF's Image File Directory (IFD) / tag structure *is* its container, so —
  unlike [`gamut-avif`](../gamut-avif)/[`gamut-heic`](../gamut-heic) (ISOBMFF) or
  [`gamut-webp`](../gamut-webp) (RIFF) — there is no separate box/chunk container. That IFD core
  is the shared [`gamut-ifd`](../gamut-ifd) primitive (also the basis for EXIF and DNG), consumed
  with its `bigtiff` feature; codec primitives come from [`gamut-core`](../gamut-core),
  [`gamut-bitstream`](../gamut-bitstream), [`gamut-deflate`](../gamut-deflate), and the bounded
  pure-Rust `miniz_oxide` inflater.
- **Permissively licensed**, matching the royalty-free TIFF format.

Unlike the video-derived still-image codecs in the workspace, TIFF is **natively a still-image
format** — a good long-term fit for gamut's image-first focus.

## Usage

[`TiffEncoder`] (implementing [`gamut_core::EncodeImage`] per pixel layout) writes 8-bit grayscale,
RGB, RGBA, CMYK, palette, and 1-bit bilevel images, and [`TiffDecoder`] (implementing
[`gamut_core::DecodeImage`]) reads them back — both reachable through the umbrella crate's `tiff`
feature:

```rust
use gamut_core::{DecodeImage, Dimensions, EncodeImage, ImageBuf, ImageRef, Rgb8};
use gamut_tiff::{Compression, TiffDecoder, TiffEncoder};

let dims = Dimensions { width: 2, height: 2 };
let pixels = vec![255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255];
let image = ImageRef::<Rgb8>::new(&pixels, dims).expect("pixel buffer matches dimensions");

let mut tiff = Vec::new();
TiffEncoder::new()
    .with_compression(Compression::PackBits)
    .encode_image(image, &mut tiff)
    .expect("encode");

let decoded: ImageBuf<Rgb8> = TiffDecoder::new().decode_image(&tiff).expect("decode");
assert_eq!(decoded.as_samples(), &pixels[..]);
```

The same example is compile-checked as the crate-level doctest. The deferred colour modes and
compression schemes land additively on this frozen surface (see Status).

## Status

**Implemented and conformance-checked against libtiff** (issue #107):

- **Structure** — byte-order header, IFD/tag read & write, strips and tiles, multi-page documents.
- **Colour modes** — grayscale, RGB and RGBA at 8 and 16 bits; CMYK (8-bit encode, 8- or 16-bit
  decode); 8-bit palette; 1-bit bilevel. Cross-depth requests resolve rather than fail: 8-bit
  widens to 16-bit exactly (`×257`), 16-bit narrows to 8-bit by truncation (lossy).
- **Sample format** — `SampleFormat` (339) is honoured: signed-integer, IEEE-float and 32-bit
  samples are refused with a typed error naming the offending tag, never truncated or
  reinterpreted. `TiffDecoder::info` reports a page's declared depth and format from tags alone —
  including for pages the decoder declines — so callers can dispatch before decoding.
- **Compression** — uncompressed, PackBits, LZW (+ strip predictor), and Adobe Deflate
  (+ horizontal differencing on strips or tiles), plus the bilevel CCITT schemes Modified Huffman
  (Group 3 1-D) and Group 4 (T.6).
- **Metadata** — `TiffEncoder::with_metadata` / `TiffDecoder::metadata` carry an Exif sub-IFD
  (`ExifIFD`, 34665, as a `gamut_ifd::Ifd`, its own `InteroperabilityIFD` resolved into a child
  directory rather than a stale offset; other pointer tags, whose targets the seam does not
  return, are left alone so a broken one cannot hide the blocks) plus opaque XMP (700),
  IPTC-IIM (33723), ICC (34675) and
  C2PA (52545) payloads — the raw blocks the workspace's metadata facade consumes. Byte payloads
  are verbatim; the Exif directory's *entries* are carried unchanged but its ordering is
  normalised (ascending tag, duplicate tags collapsed, a child's next-IFD pointer ignored). The
  blocks live in **IFD 0 only**, so a reader decoding page 3 of a multi-page document alone must
  look at IFD 0 for them. The C2PA manifest store follows C2PA 2.4 §A.3.6 through the shared
  `gamut_ifd::c2pa` helper it and `gamut-dng` both call: the entry in the last IFD of the main
  chain, the store at the end of the file, and the two §18.5.5 exclusion ranges reported by
  `TiffEncoder::encode_with_report` or recovered from any file by `gamut_tiff::c2pa_exclusions`.
  `with_c2pa_reserved` writes a zero-filled reservation for an external signer to overwrite in
  place; because it is an infallible builder, a length no buffer or container could hold is
  refused by the encode that follows, before the reservation is allocated.
- The decoder is hardened against hostile input (`#![forbid(unsafe_code)]`, a size cap, and a
  byte-flip fuzz corpus).

**Deferred — planned, additive** (see the [STATUS.md](STATUS.md) scope ledger): YCbCr (§21),
CIE L\*a\*b\* / RGB colorimetry (§20, §23), new-style JPEG-in-TIFF (§22, `Compression = 7`), and
smaller items (CCITT Group 3 2-D, planar config, IEEE-float and 32-bit samples, 4-bit grayscale,
halftone hints). The metadata payloads are carried as raw bytes rather than parsed here; wiring
them to the typed [`gamut-metadata`](../gamut-metadata) facade is tracked separately.
**Permanently out of scope:** old-style JPEG (§22, `Compression = 6`), deprecated and
unimplementable-as-specified per TIFF Technical Note 2.

## Roadmap

The deferred TIFF 6.0 features each land as a follow-up PR that plugs into the same strip/tile
pipeline and libtiff oracle: the colour spaces (YCbCr, L\*a\*b\*) need `gamut-color` conversions
matched to libtiff's integer math; JPEG-in-TIFF's DCT codec now exists in the workspace
([`gamut-jpeg`](../gamut-jpeg), issue #28) — the remaining work is the Technical Note 2 strip/tile
wiring and a `libjpeg`-enabled oracle build.

Correctness is pinned with a differential oracle against **libtiff**: gamut-encode → libtiff-decode
and libtiff-encode → gamut-decode must agree pixel-for-pixel on every lossless path.

## License

Licensed under either of MIT or Apache-2.0 at your option.
