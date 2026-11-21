# gamut-metadata — metadata facade status

The unified metadata facade over [`gamut-exif`](../gamut-exif), [`gamut-xmp`](../gamut-xmp),
[`gamut-icc`](../gamut-icc) and [`gamut-iptc`](../gamut-iptc). What it does and how to use it is in
its [README](README.md); this file owns **disposition** — what is supported, what is deferred, and
on what condition the deferred work starts.

## Scope

| Area | Carriers / surface | Status |
| ---- | ------------------ | ------ |
| Still-image carriers | `Metadata` fields `exif`, `xmp`, `icc`, `c2pa`; IPTC as a lens over `xmp`, legacy IIM folded into it | ✅ supported |
| Extract / embed | `MetadataExtractor`, `MetadataEmbedder`, `Metadata::parse_blocks` / `to_blocks` over container-located `MetadataBlock`s | ✅ supported |
| Format wiring | `TypedMetadata` / `MetadataSink`, the traits image format crates implement behind their `metadata` feature (README, *Consumer integration*) | ✅ supported |
| Image capability table | format × `Carrier` × read/write, answered by `gamut::capability::supports` in the umbrella (this crate names no format crate) | ✅ supported, image formats only |
| C2PA manifest store | extracted verbatim, never re-embedded | ✅ supported, by design asymmetric (README) |
| Audio and video metadata ([#216]) | ID3v1/v2, Vorbis comment, APEv2, FLAC blocks, ISOBMFF `ilst`/`ID32`, Matroska `Tags`, RIFF `INFO`/`bext`/`iXML`, … | ⏸ **deferred until a consumer is confirmed** |
| MIME-keyed support query ([#216]) | `is_mime_supported`, `extract_if_supported` | ⏸ **deferred with it** — sketched below |

## Deferred: audio and video metadata (#216)

[#216] asks for the facade to cover audio and video files, a per-format read/write ledger, and a
programmatic compatibility query, benchmarked against MediaInfo's format coverage. The maintainer's
disposition on the issue is to **halt until there is a confirmed direct consumer**, and that audio
and video coverage go hand in hand — so this work is gated on, and lands interleaved with, the
container work of [#217] (*Extend `gamut` to non-image formats*). Nothing below exists today; no
type, function or crate named here is published.

**Start condition.** A confirmed direct consumer, with its access pattern, is recorded on [#216] or
[#217]. That is phase 0 of both roadmaps in
[`docs/non-image-media.md`](../../docs/non-image-media.md#roadmap-216--metadata), which is normative
for the design and is not restated here.

**Format matrix.** The MediaInfo-style container × carrier × read/write ledger [#216] asks for is
kept once, as the
[carrier matrix in `docs/non-image-media.md`](../../docs/non-image-media.md#carrier-matrix), with
MediaInfo as the breadth benchmark and ExifTool as the read/write one (its §5). Its `R`/`W` cells
are targets, not current state. In outline, by container family:

| Family | Carriers | Target |
| ------ | -------- | ------ |
| ISOBMFF (MP4/MOV/M4A) | `ilst`, `ID32`, XMP `uuid`, EXIF, C2PA `uuid` | R/W (C2PA R/passthrough) |
| Matroska / WebM | `Tags`, `Chapters`, `Attachments` | R/W |
| Ogg, native FLAC | Vorbis comment, `PICTURE`, FLAC metadata blocks | R/W |
| MP3 / APE / WavPack | ID3v1, ID3v2, APEv2 | R/W |
| RIFF (WAV/BWF/AVI), AIFF | `LIST INFO`, `id3 `, `bext`, `iXML`, `_PMX`, AIFF text chunks | R/W |
| ASF, MPEG-TS/PS, CAF | descriptors, info chunks | R, deferred further |
| MXF | DMS-1 | out of scope |

**Future API shape.** Phase 1 of that roadmap, landing before any new carrier: a MIME-keyed query
over the same format × carrier × direction table `gamut::capability` already answers for image
formats, generated from the data the matrix renders so the two cannot drift. Because this facade
names no format crate, the query lives with the format list in the umbrella, not here. Sketch only
— names and signatures are settled when phase 1 starts:

```rust,ignore
// Proposed; does not exist.
pub fn is_mime_supported(mime: &str, carrier: Carrier, direction: Direction) -> bool;
pub fn extract_if_supported(mime: &str, bytes: &[u8]) -> Option<gamut_metadata::Result<Metadata>>;
```

`extract_if_supported` returns `None` for a format gamut does not read, rather than an error, so a
caller probing mixed media can skip what is unsupported without matching on error kinds.

[#216]: https://github.com/visualcommons/gamut/issues/216
[#217]: https://github.com/visualcommons/gamut/issues/217
