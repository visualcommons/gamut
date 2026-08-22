# gamut-cli

`gamut-cli` ships the `gamut` binary — a command-line **sandbox** that exercises the workspace's
implemented primitives end to end, so the latest features are runnable from a shell without
writing throwaway Rust.

## Goals

Part of the [gamut](../../README.md) workspace, this crate exists to:

- **Make the codec pipelines runnable.** `gamut convert in.png out.avif` drives the AVIF encode
  path ([`gamut-color`](../gamut-color) → [`gamut-av1`](../gamut-av1) →
  [`gamut-isobmff`](../gamut-isobmff), surfaced through [`gamut-avif`](../gamut-avif)); `gamut
  convert in.png out.webp` drives [`gamut-webp`](../gamut-webp) (VP8L lossless / VP8 lossy, with
  alpha). WebP can also be read back — `gamut convert in.webp out.avif` decodes it through gamut's
  own WebP decoder — so the encode→decode round-trip is exercisable end to end.
- **Expose the shared primitives.** Each shared building block — color/CICP tables, the DSP
  Walsh–Hadamard transform, and the bitstream LEB128 coder — gets an inspection subcommand, so new
  primitives have an obvious place to be surfaced as they land.
- **Keep the codec path pure gamut.** *Encoding* is always produced by the gamut crates, and so is
  *decoding* of **PNG, JPEG, WebP, and JPEG XL** input. Only PPM still borrows the third-party
  [`image`](https://crates.io/crates/image) crate, because gamut has no PPM decoder — so every
  format gamut implements runs both directions on memory-safe gamut code.

The crate is `gamut-cli` (so `cargo install gamut-cli`), but it installs a binary named `gamut`.

## Usage

```bash
# Decode PNG/JPEG/PPM/WebP and encode AVIF (output format inferred from the extension).
gamut convert input.png output.avif

# Encode WebP: lossless VP8L by default, or lossy VP8 with --lossy (transparency is preserved).
gamut convert input.png output.webp
gamut convert input.png output.webp --lossy --quality 80
# --webp-effort 0..=6 trades encode time for size (libwebp's method; default 4).
gamut convert input.png output.webp --webp-effort 6
# --webp-near-lossless 0..=99 quantizes colour before lossless coding (omit or 100 = off).
gamut convert input.png output.webp --webp-near-lossless 60

# Encode JPEG XL: lossless by default, or lossy with --jxl-distance (~1.0 = visually lossless).
# --jxl-effort 1..=10 tunes speed vs density; --jxl-container emits the ISO BMFF .jxl box format.
gamut convert input.png output.jxl
gamut convert input.png output.jxl --jxl-distance 1.0 --jxl-effort 7
gamut convert input.png output.jxl --jxl-container

# Encode JPEG (JPEG-1): always lossy at --quality (default 75). --jpeg-subsampling picks the
# YCbCr chroma resolution (444/422/420, default 420); --jpeg-restart-interval N inserts RSTn
# restart markers every N MCUs (0 = off); --jpeg-progressive selects the progressive (SOF2)
# process instead of baseline sequential.
gamut convert input.png output.jpg
gamut convert input.png output.jpg --quality 90 --jpeg-subsampling 444
gamut convert input.png output.jpg --jpeg-restart-interval 8
gamut convert input.png output.jpg --jpeg-progressive

# Read WebP or JPEG XL back and transcode it — decoded by gamut's own decoders, no third-party lib.
gamut convert output.webp roundtrip.avif
gamut convert output.jxl roundtrip.png

# Encode a raw AV1 OBU temporal unit you can hand to a decoder.
gamut av1 encode input.ppm output.obu
dav1d -i output.obu -o roundtrip.y4m      # external check

# Inspect the gamut-color CICP / pixel-format tables.
gamut color list

# Run the 4x4 Walsh–Hadamard transform over 16 ints and verify the round-trip.
gamut dsp wht 1 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0

# Show the unsigned LEB128 encoding of a value.
gamut bitstream leb128 300                # -> ac 02 (2 bytes)

# Logging goes to stderr; -v = info, -vv = debug, or set RUST_LOG.
gamut -vv convert input.jpg output.avif
```

## Status

The sandbox exposes:

- `convert` — decode PNG/JPEG/PPM/WebP/JXL and encode to a gamut codec:
  - **AVIF** — lossless (default) or lossy intra via `--qindex` (8-bit RGB).
  - **WebP** — lossless VP8L (default) or lossy VP8 via `--lossy --quality`, with transparency
    preserved; emits a simple file when fully opaque and an extended (`VP8X`/`ALPH`) file otherwise.
  - **JPEG XL** — lossless (default) or lossy via `--jxl-distance`, with `--jxl-effort` (1–10) and
    `--jxl-container` (ISO BMFF); transparency preserved. Encoded via libjxl; the `.jxl` input path
    decodes through the pure-Rust jxl-rs backend.
  - **JPEG** (`.jpg`/`.jpeg`) — JPEG-1, baseline sequential or progressive (`--jpeg-progressive`),
    always lossy at `--quality`; colour is YCbCr with `--jpeg-subsampling` (444/422/420, default
    420) and optional `--jpeg-restart-interval` restart markers. No alpha (JPEG has none).
- `av1 encode` — raw AV1 OBU still images from 8-bit RGB input.
- `color list`, `dsp wht`, `bitstream leb128` — inspection of the shared primitives.

Output is always encoded by gamut crates, and **PNG, JPEG, WebP, and JPEG XL input are decoded by
gamut's own decoders** — so those round-trips (`png → webp → avif`, `png → jxl → png`) run entirely
in-tool. Only PPM input uses the `image` crate. Because the encoders take a fixed 8-bit RGB(A)
buffer, the CLI asks every decoder for one with
`ConvertPolicy::permissive()`: a 16-bit PNG narrows, a grayscale JPEG replicates, and a transparent
image asked for RGB drops its alpha — losses the gamut libraries refuse by default and the CLI opts
into on your behalf. AVIF/AV1 output still has no
in-workspace decoder, so verify it externally (`avifdec` / `dav1d`). `avif`, `webp`, `tiff`, `png`,
`jxl`, and `jpg`/`jpeg` are the supported output formats; `convert` reports a clear error for
anything else.

## Roadmap

As the codecs grow, so does the sandbox: an in-tool AVIF/AV1 decode path once a gamut AV1 decoder
exists (WebP and JPEG XL already decode), an explicit `info`/decode-to-pixels command, more output
formats as the codecs fill in, and a subcommand for each new primitive (e.g. the `gamut-bitstream`
symbol coder).

## License

Licensed under either of MIT or Apache-2.0 at your option.
