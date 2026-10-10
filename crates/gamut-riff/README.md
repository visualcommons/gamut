# gamut-riff

`gamut-riff` provides Resource Interchange File Format (RIFF) container utilities — the chunked
container that WebP is built on.

## Goals

Part of the [gamut](../../README.md) workspace, this crate exists to:

- **Own the WebP container, not the codec.** It reads and writes the RIFF chunk structure
  (`RIFF`/`WEBP` plus `VP8 `/`VP8L`/`VP8X`/`ALPH`… chunks), leaving the VP8/VP8L bitstream to
  [`gamut-webp`](../gamut-webp) — mirroring how [`gamut-isobmff`](../gamut-isobmff) backs AVIF/HEIC.
- **Stay spec-faithful.** Implemented clean-slate from **RFC 9649 §2** (*WebP Image Format*) and the
  Google *WebP Container* specification in [`../../references/webp/`](../../references/webp). Those
  are the authority for the RIFF subset WebP uses — a flat chunk list under a single `RIFF`/`WEBP`
  form. The canonical RIFF document (Microsoft/IBM *Multimedia Programming Interface and Data
  Specifications 1.0*) is only *cited* by RFC 9649, not vendored here, and the wider RIFF vocabulary
  it defines — `LIST`, arbitrary form types, the AVI/WAVE chunks — is deliberately out of scope.
- **Stay memory-safe on hostile input.** `#![forbid(unsafe_code)]`, typed errors carrying a byte
  offset, and no allocation driven by a count the input chose.

## Usage

`gamut-riff` exposes three readers at increasing strictness — `RiffReader` (a permissive chunk
iterator), `MetadataChunks::read` (the metadata triple alone), and `WebpLayout::parse` (the full
still-image layout, which enforces the spec's chunk ordering) — plus the `RiffWriter` chunk writer,
a `FourCc` type, chunk classification (`WebpChunkId`), and the WebP file writers:
`write_simple_lossless` / `write_simple_lossy` for the simple formats and `write_extended`,
`write_extended_with_metadata`, `write_extended_preserving` for the extended one. It is driven by
[`gamut-webp`](../gamut-webp); most consumers use it indirectly through that crate rather than
directly.

## Status

See [`STATUS.md`](STATUS.md) for the full ledger of what is covered, what is deliberately settled,
and what is deferred. The simple-WebP container and the extended format (`VP8X` plus `ALPH`, `ICCP`,
`EXIF`, `XMP `, the `C2PA` manifest-store chunk of C2PA 2.4 §A.3.7, and unknown chunks) are
implemented, read and write, and validated against libwebp's demuxer as the differential oracle.

`MetadataChunks` is exhaustive by design, so the `c2pa` field it gained for the manifest store is a
breaking change: code that builds one with a struct literal adds `c2pa: None` (or
`..Default::default()`). Nothing else in the surface changed shape.

Animation (`ANIM`/`ANMF`) is **out of scope** — recognized FourCCs only, so an animated file is
reported as unsupported rather than mis-parsed. Multi-frame sequences sit outside the image-first
charter; see
[`gamut-webp/STATUS.md`](../gamut-webp/STATUS.md#scope-decisions--non-core-feature-paths).

## License

Licensed under either of MIT or Apache-2.0 at your option.
