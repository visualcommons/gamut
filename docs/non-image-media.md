# Non-image media — charter, boundary, and roadmap

**Normative for:** what gamut does and does not implement for audio, video, and other non-image
media; the crate topology that work lands in; the roadmaps for [#217] and [#216].

**Not an authorization to build.** [#216] carries a standing maintainer hold — *"halt until there is
a confirmed direct consumer"* — and this document does not lift it. It exists so that when a
consumer does appear, the design is already decided and the first commit is not also the first
argument. Phase 0 of each roadmap below is the gate.

**Status:** decided (2026-08-14). Everything described in [Topology](#topology) and
[Roadmaps](#roadmap-217--containers-and-tracks) is **proposed API that does not exist yet**; no
crate named `gamut-media`, `gamut-ebml`, `gamut-ogg`, `gamut-id3`, `gamut-vorbis-comment`, or
`gamut-ape` is published today. Shipped API lives in each crate's `README.md`, and what is actually
implemented lives in its `STATUS.md`.

---

## 1. The boundary rule

> **gamut owns bytes that *describe* media. A downstream consumer owns bytes that *are* coded
> media.**

This single rule decides every scope question in this document, and should decide the ones that
come after it. Apply it before reaching for a list.

A container's box tree, chunk graph, element tree, track table, timing model, tag carriers, colour
signalling, chapter and cue structures, and attachment payloads all *describe* media — gamut
implements them, clean-slate, from vendored specs. An H.264 slice, an HEVC NAL payload, an AAC
frame, an Opus packet, an AV1 OBU inside a video track — these *are* coded media. gamut locates
them, bounds them, labels them, hands them over, and never decodes or encodes them.

The image codecs are the deliberate exception and stay that way: gamut implements still-image
bitstreams (JPEG, PNG, WebP, AVIF, TIFF, DNG, JXL) because that is the project's original charter.
Extending to audio and video extends the *container and metadata* surface only. There is no path in
this document toward an in-tree AAC decoder or an H.264 encoder, and a proposal to add one is a
charter change, not a feature request.

### What this buys

A consumer already has a codec. It has libav/ffmpeg, or a platform decoder (VideoToolbox,
MediaCodec, Media Foundation), or a hardware path. What it usually does *not* have is a memory-safe,
permissively-licensed, write-capable way to read and rewrite the metadata and structure around those
frames. That is the gap gamut fills — the same argument that justified the image crates, applied to
the containers the same files travel in.

---

## 2. Charter delta

The workspace charter in [`AGENTS.md`](../AGENTS.md) opens *"gamut is image-first"* and forbids
inter-frame, motion, and sequence coding. That stays true for **codestreams**. It stops being true
for **containers**.

| Dimension | Before | After |
| --------- | ------ | ----- |
| Coded bitstreams | Still images only | **Unchanged** — still images only |
| Containers | Still-image profiles only (HEIF `meta`, simple/extended WebP) | Full media containers — tracks, timing, samples, tags |
| Metadata | Image carriers (EXIF, XMP, ICC, IPTC) | + audio/video carriers (ID3, Vorbis comment, APE, `ilst`, Matroska `Tags`, BWF, …) |
| Sequences | Rejected as `Unsupported` | Parsed structurally; sample payloads opaque |

Two shipped decisions are reversed by this, and both are reversed **knowingly**:

1. **`gamut-isobmff` rejects `moov`/`trak` in the primary stream as `Unsupported`**
   ([`STATUS.md`](../crates/gamut-isobmff/STATUS.md), "Likely out of scope"). The movie box is the
   entire point of MP4/MOV as a media container. It becomes in scope — structurally. The sample data
   the track table points at stays opaque, exactly as `PropertyKind::CodecConfiguration` is opaque
   today, so the reversal is additive to the model rather than a reshape of it.
2. **`gamut-isobmff` has no streaming input** — `BoxReader` is "intentionally a zero-copy cursor
   over one byte slice", with large-file consumers expected to buffer or mmap. A four-hour MKV is
   not a file you buffer to read its tags. Streaming becomes a first-class access mode (§3 I4).

`gamut-riff`'s deferral of `ANIM`/`ANMF` as "outside the image-first charter" is **not** reversed by
this document — those are WebP *animation* frames, i.e. coded image sequences, which remain out of
scope under the boundary rule. RIFF grows in a different direction (WAV/BWF/AVI structure and tag
chunks), tracked in §8.

---

## 3. Design invariants

Four invariants, each with the mechanism that enforces it. An implementation that satisfies the
letter of a phase but violates an invariant has not landed the phase.

### I1 — No codec bitstream ingestion

gamut never parses into an audio or video codec's bitstream syntax. Containers surface coded data as
**opaque, bounded, labelled** byte ranges: a `CodecId`, the codec-private/setup bytes verbatim
(`avcC`, `hvcC`, `av1C`, `esds`, Matroska `CodecPrivate`, Vorbis setup headers), and per-sample
extents with timing and flags. Nothing in the tree inspects those bytes.

Where a consumer wants to plug a decoder in, it does so through the hook shape
[`AGENTS.md`](../AGENTS.md) already mandates — the
[`gamut_heic::HevcCodestreamDecoder`](../crates/gamut-heic) shape: an object-safe trait,
borrowed bytes in, owned plain data out — bridged to [`gamut-codec-abi`](../crates/gamut-codec-abi)
so a C/FFI backend and a pure-Rust one enter by the same door. gamut ships **no** software tail for
audio or video codecs, so unlike the image formats there is no implicit fallback; a job with no
registered backend is a typed error, never a silent no-op.

*Enforcement:* no A/V codec crate enters the workspace. Code review rejects any parse that indexes
past a codec-private blob's length prefix into its syntax.

### I2 — Vendor the core logic

gamut implements container and tag parsing **clean-slate from primary specifications**, vendored
under [`references/`](../references), and does not wrap GStreamer, libav/ffmpeg, Bento4, GPAC,
libebml/libmatroska, or TagLib. This is the same rule the image crates follow, and it is the main
structural difference between gamut and every other "media metadata" option.

The licensing consequence is not incidental — it is a large part of why the gap in §6 exists:

| Project | License | Consequence for a permissively-licensed consumer |
| ------- | ------- | ------------------------------------------------ |
| ffmpeg / libav | LGPL-2.1-or-later, GPL-2.0-or-later for some parts | Dynamic-linking obligations; GPL contamination if the wrong parts are enabled |
| GStreamer | LGPL-2.1 | Same, plus a plugin-registry runtime |
| TagLib | LGPL-2.1 / MPL | Same-class obligations |
| mutagen | GPL-2.0 | Copyleft; Python |
| ExifTool | GPL-3.0 (Perl artistic dual) | Copyleft; Perl runtime; process-boundary integration |
| MediaInfoLib | BSD-2-Clause | Permissive, but C++ and read-only |
| **gamut** | **MIT OR Apache-2.0** | No obligations, no C toolchain, no FFI boundary to audit |

*Enforcement:* the only permitted external dependencies are vetted, permissively-licensed Rust
utility crates of the class already in the tree (`quick-xml`, `miniz_oxide`, `md-5`). A binding to a
media framework is a charter violation regardless of how convenient the coverage is.

### I3 — Stateless internals

gamut holds **no** parser state between calls. There is no `Demuxer` object accumulating position,
no internal ring buffer, no hidden allocation arena, no "call `reset()` before reusing". Every parse
entry point is a pure function of its inputs.

The consumer's decoder may be as stateful as it likes — that is its business and gamut is not
involved in it.

Statelessness is what makes the same code serve both access modes (I4), makes parses trivially
parallel across ranges, makes fuzzing reproducible from a seed, and keeps the surface mechanically
portable to C, where an opaque stateful handle would otherwise force lifetime and threading
questions into the ABI.

*Enforcement:* no `&mut self` parse method on a public type. Resume position is caller-owned data
(§3 I4), never library-owned state.

### I4 — One API, two access modes

Random-access (whole file in memory or mapped; seekable local storage) and streaming (network
socket, pipe, HTTP range-fetch, live capture) are served by **one** parser core, not two
implementations that drift apart.

The core is a pure step function. The caller owns the bytes and owns the position:

```text
step(cursor, window) -> (Outcome, cursor')

Outcome ::= Event(item)                      // a structural item was parsed
          | Need { offset: u64, len: u64 }   // give me these bytes and call again
          | Done
```

`cursor` is plain `Copy` data — no pointers, no lifetimes, no handles. It is therefore serializable,
so a parse can be suspended, persisted, moved across a thread or a process, and resumed; it is C-ABI
representable, so [`gamut-ffi`](../crates/gamut-ffi) exposes it without an opaque handle type; and
because it is data rather than a state machine, an adversarial or truncated input can only produce a
bad *value*, never a corrupt internal state.

The two modes are then thin, obvious drivers over that core, and cost nothing extra to maintain:

- **Random access** — a driver that satisfies every `Need` from a slice or a `ReadAt`-style range
  provider, returning the whole structure. This is what [`gamut-isobmff`](../crates/gamut-isobmff)
  and [`gamut-ifd`](../crates/gamut-ifd) already do behind their `read` functions; they gain the
  streaming mode without losing the ergonomic one.
- **Streaming** — a driver that feeds bytes forward as they arrive and honours `Need` by waiting or
  by issuing a range request. A `Need` whose offset is behind the current position is exactly the
  signal a caller needs to decide between buffering, re-fetching, or giving up — a decision gamut
  must not make on the caller's behalf, because the right answer differs between a local file, an
  S3 object, and a live socket.

This directly addresses a limitation shared by the whole peer set: `lofty` requires `Read + Seek`,
`mp4parse` reads to completion, and `matroska` takes a path or a reader. None of them can answer
"read the tags from this MP4 over HTTP without fetching the `mdat`", which is a routine ask — and
which `Need`-driven range requests answer in a handful of round trips.

---

## 4. Scope verdict — what beyond audio, video, and image?

[#217] guessed *"other types of formats are probably out-of-scope"*. Confirmed, with one precise
exception, stated as a rule rather than a list:

> **A non-media payload is in scope when a media container hands it to us, and out of scope as a
> standalone file format.**

### In scope — because a container carries them

| Payload | Where it lives | What gamut does |
| ------- | -------------- | --------------- |
| Timed text / subtitle tracks | Matroska subtitle tracks, ISOBMFF `text`/`subt`/`wvtt`/`stpp` tracks | Declares the track, its codec id, language, and per-cue sample extents and timing. Cue *bodies* stay opaque. |
| Chapters / editions | Matroska `Chapters`, ISOBMFF `chpl`/chapter tracks, Ogg `CHAPTER` comments, ID3 `CHAP`/`CTOC` | Typed — hierarchy, timing, titles, ordering |
| Cue sheets | FLAC `CUESHEET`, Matroska `Cues` | Typed |
| Attachments | Matroska `Attachments`, ISOBMFF item payloads | Declared with name/MIME/extent; payload opaque |
| Embedded pictures | ID3 `APIC`, FLAC `PICTURE`, Ogg `METADATA_BLOCK_PICTURE`, `ilst` `covr` | Typed wrapper; the image bytes route to gamut's own image crates — the vertical integration [#216] asks for |
| C2PA / JUMBF manifests | Top-level ISOBMFF `uuid` box, user type `D8FEC3D6-1B0E-483C-9297-5828877EC481`, placed after `ftyp` and before both the first `mdat` and any `moov` | Located and carried as a metadata block; validation belongs to [#239] and `c2pa-rs` |

> **On the box types.** `c2ma` / `c2um` are **not** ISOBMFF box types. They are the leading four
> ASCII bytes of JUMBF *type UUIDs* (`c2ma` = `63326D61…`), naming manifest superboxes *inside* the
> store. The ISOBMFF carrier is one top-level `uuid` box whose payload holds the JUMBF manifest
> store (the `c2pa` superbox). Note also that there is **no item-based placement**: C2PA 2.4
> Appendix A defines only the top-level `uuid` box for every BMFF-based asset, with HEIF and AVIF
> named explicitly, so `infe`/`iloc`/`ipco` are not a route.

The common thread: gamut is the thing that *finds and bounds* the payload correctly in a hostile
file. It types the payload only where the type is structural (a chapter's timing) rather than
linguistic (a WebVTT cue's text).

### Out of scope — standalone formats

Documents (PDF, Office), fonts, 3D and scene formats (glTF, USD), archives, and **standalone**
subtitle files (`.srt`, `.vtt`, `.ass`) are all out. The last one is the closest call and worth
naming explicitly: parsing a `.vtt` file is text parsing with its own cue model, styling grammar,
and conformance surface — a different product that happens to be adjacent, not a natural extension
of a container parser. A WebVTT cue *inside* an MKV is in scope as a bounded sample; the same bytes
in a `.vtt` file on disk are not gamut's problem.

Raw/streaming elementary streams with no container (`.aac` ADTS, `.ac3`, `.h264` Annex B) sit on the
line. They are in scope only to the extent of **framing and tag discovery** — locating ADTS frame
boundaries to find a trailing ID3v1/APEv2 block is describing media; parsing the AAC payload is not.

---

## 5. Coverage benchmark

[#216] names **MediaInfo** as the coverage benchmark. MediaInfo and ExifTool measure different
things, and gamut is measured against both on different axes:

- **MediaInfo** (BSD-2-Clause, C++) — the *breadth* benchmark. Containers: Matroska, MP4/QuickTime,
  AVI, WAV, Ogg, MPEG-PS/TS, ASF/WMV, MXF, GXF, LXF, FLV, Real, plus bare audio streams (AC-3, DTS,
  AAC, FLAC, Monkey's Audio) and the tag families ID3v1, ID3v2, Vorbis comment, and APE. MediaInfo
  is **read-only**; matching its breadth is a long-horizon goal, not a v1 target, and §8 phases it.
- **ExifTool** (GPL-3.0, Perl) — the *read/write* benchmark, and the harder one. ExifTool writes
  QuickTime/`ilst`, XMP, and ID3-family tags; Matroska and ASF are read-only even there. A
  write-capable pure-Rust path is where gamut has something the ecosystem lacks, so **write is a
  first-class requirement per carrier, not a follow-up phase** — the same discipline
  [`gamut-exif`](../crates/gamut-exif) and [`gamut-icc`](../crates/gamut-icc) already hold, where
  round-trip equality is the acceptance test.

Both serve as differential oracles under the existing `tooling/` pattern (dev-only, vendored via
submodule, never a dependency), alongside `mp4box`/GPAC and `mkvalidator` for structural
conformance.

---

## 6. Ecosystem audit

Verified against crates.io and upstream repositories on 2026-08-14.

### Rust peers

| Crate | Version | License | Covers | Direction | Limits relative to this charter |
| ----- | ------- | ------- | ------ | --------- | ------------------------------- |
| `symphonia` | 0.6.1 | MPL-2.0 | AIFF, CAF, ISO/MP4, MKV/WebM, Ogg, WAV; ID3v1/v2, `ilst`, RIFF INFO, Vorbis comment | **Read only** | Decode-oriented — pulls in codec decoding, which I1 forbids gamut from having; no write path; MPL file-level copyleft |
| `lofty` | 0.25.0 | MIT OR Apache-2.0 | 12 audio formats; ID3v1/v2, APE, `ilst`, Vorbis comment, RIFF INFO, AIFF text | **Read + write** | Audio only — no video containers, no image metadata; requires `Read + Seek`, so no streaming or range-fetch; APE/MPC support read-only for lack of a spec |
| `mp4parse` | 0.17.0 (2023-05-29) | MPL-2.0 | ISO BMFF structure | **Read only** | Scoped to Firefox's needs; no write path; no crates.io release since 2023 |
| `matroska` | 0.30.1 | MIT/Apache-2.0 | MKV metadata | **Read only** | Metadata subset; no write path |
| `ebml-iterable` | 0.6.3 | MIT | EBML elements | Read + write | Spec-agnostic iterator — the Matroska semantic layer is left to the caller |
| `id3` | 1.17.1 | MIT | ID3v1/v2 | Read + write | Single carrier |
| `metaflac` | 0.2.8 | MIT | FLAC blocks | Read + write | Single carrier |
| `mp4ameta` | 0.13.0 | MIT OR Apache-2.0 | iTunes `ilst` | Read + write | Single carrier, audio-oriented |
| `mpeg2ts-reader` | 0.18.2 | MIT/Apache-2.0 | MPEG-TS | Read only | Transport layer only |
| `audiotags` | 0.5.0 (2024-02-01) | MIT | mp3/flac/m4a | Read + write | Thin facade over other crates; unmaintained |

### Non-Rust references

`MediaInfoLib` (BSD-2-Clause, read-only, C++), `ExifTool` (GPL-3.0, read+write, Perl), `TagLib`
(LGPL-2.1, audio only, C++), `mutagen` (GPL-2.0, audio only, Python), plus the framework-scale
options — ffmpeg/libav (LGPL/GPL) and GStreamer (LGPL) — which bring an entire codec stack and its
licensing along for what is, from a metadata consumer's perspective, a parsing job.

### The gap gamut fills

No option in the table above is **all four** of: pure Rust, permissively licensed, write-capable,
and spanning image *and* audio *and* video under one metadata model.

- Write-capable Rust exists only per-carrier (`id3`, `metaflac`, `mp4ameta`) or audio-only
  (`lofty`).
- Cross-media coverage exists only in copyleft C++/Perl (`ExifTool`) or read-only C++
  (`MediaInfoLib`).
- **Nothing** offers a streaming/range-driven metadata read. Every Rust peer requires `Seek`, a
  path, or a full buffer.
- **Nothing** unifies image and A/V metadata: a tool handling a photo library and a video library
  today runs two unrelated stacks with two unrelated models, and cannot round-trip a cover image
  through the same code that writes a JPEG's EXIF.

That last point is precisely the "vertical integration" [#216] opens with, and it is the strongest
argument for building this inside gamut rather than as a separate project: gamut already owns the
image half, the ICC/XMP/EXIF models, the IFD and box and chunk machinery, and the hostile-input
discipline. The A/V half is additive to assets that already exist.

### Carrier matrix

The per-carrier ledger [#216] asks for. `R`/`W` are **targets**, not current state; nothing here is
implemented.

| Container | Metadata carrier | Target | Owning crate (proposed) |
| --------- | ---------------- | ------ | ----------------------- |
| ISOBMFF (MP4/MOV/3GP/M4A) | `moov/udta/meta/ilst` (iTunes) | R/W | `gamut-isobmff` + `gamut-media` |
| | `ID32` box (ID3v2 in BMFF) | R/W | `gamut-id3` |
| | XMP `uuid` box | R/W | `gamut-xmp` (exists) |
| | `Exif` / QuickTime `udta` EXIF | R/W | `gamut-exif` (exists) |
| | C2PA top-level `uuid` (user type `D8FEC3D6-…-C481`; payload is the JUMBF manifest store) | R/passthrough | `gamut-isobmff` → [#239] |
| | Track `colr`, `pasp`, `clli`, `mdcv` | R/W | `gamut-isobmff` (partly exists) |
| Matroska / WebM | `Tags` / `SimpleTag` | R/W | `gamut-ebml` + `gamut-media` |
| | `Chapters`, `Attachments`, `Cues` | R/W | `gamut-ebml` |
| | Track `Colour`, `Projection` | R/W | `gamut-ebml` (maps to `gamut-color` CICP) |
| Ogg | Vorbis comment (Vorbis/Opus/FLAC/Speex/Theora) | R/W | `gamut-vorbis-comment` + `gamut-ogg` |
| | `METADATA_BLOCK_PICTURE` | R/W | `gamut-vorbis-comment` → image crates |
| FLAC (native) | `STREAMINFO`, `VORBIS_COMMENT`, `PICTURE`, `CUESHEET`, `SEEKTABLE`, `APPLICATION` | R/W | `gamut-flac-meta` (in `gamut-media`) |
| MP3 / ADTS / raw | ID3v2 (leading), ID3v1 (trailing), APEv2, Lyrics3 | R/W | `gamut-id3`, `gamut-ape` |
| RIFF (WAV / BWF / AVI) | `LIST INFO`, `id3 ` chunk | R/W | `gamut-riff` + `gamut-id3` |
| | `bext` (EBU Tech 3285), `iXML`, `aXML`, `cart` | R/W | `gamut-riff` |
| | `_PMX` (XMP), `ds64` (RF64) | R/W | `gamut-riff` + `gamut-xmp` |
| AIFF / AIFF-C | `NAME`/`AUTH`/`ANNO`/`COMT`, ID3 chunk | R/W | `gamut-riff`-adjacent + `gamut-id3` |
| APE / MPC / WavPack | APEv2 | R/W | `gamut-ape` |
| CAF | Apple info chunks | R | deferred |
| ASF / WMV | Content Description, Extended Content Description | R | deferred (§9) |
| MPEG-TS / PS | PSI (PAT/PMT/SDT) descriptors | R | deferred (§9) |
| MXF | SMPTE ST 377-1 / DMS-1 | — | out of scope (§9) |

---

## 7. Topology

`gamut-media` is a **new facade sibling** to `gamut-metadata`, not a widening of it.
`gamut-metadata` v1.0.0 documents itself as container-agnostic with one field per *image* carrier;
its extract→embed→extract equality is a property of that narrow model. Track tables and timing do
not belong in it, and reshaping a published v1.0.0 facade to hold them would trade a clean SemVer
story for nothing. `gamut-media` re-exports `gamut-metadata` unchanged, so a consumer still reaches
everything through one entry point.

```text
gamut-media                       facade: containers + tracks + tags; both access modes
├── gamut-metadata  [v1.0.0]      image metadata, unchanged
│   └── gamut-exif / gamut-xmp / gamut-icc / gamut-iptc
├── gamut-id3                     ID3v1 / ID3v2.3 / ID3v2.4
├── gamut-vorbis-comment          Vorbis comment + METADATA_BLOCK_PICTURE
├── gamut-ape                     APEv2 / APEv1
├── gamut-ebml                    EBML core + Matroska/WebM semantic layer
├── gamut-ogg                     Ogg page/packet layer
├── gamut-isobmff  [extended]     + moov/trak/stbl/udta/ilst, streaming cursor
└── gamut-riff     [extended]     + WAVE/BWF/AVI chunks, streaming cursor
```

Consistent with the existing convention: one leaf crate per specification, a thin orchestration-only
facade, and no leaf depending on a sibling it does not genuinely need. `gamut-ebml` splits EBML
(RFC 8794) from Matroska (RFC 9559) internally by module rather than by crate — unlike TIFF/EXIF,
there is no second consumer of bare EBML to justify a separate crate, and one can be split out later
as a semver-minor move if one appears.

**Umbrella features** (`crates/gamut/Cargo.toml`): `media` pulls the facade; `ebml`, `ogg`, `id3`,
`vorbis-comment`, `ape` re-export primitives for tooling, mirroring how `isobmff` and `metadata`
work today. `all` gains them. `gamut-ffi`'s feature table mirrors `gamut`'s, enforced by
`mise run check-ffi-features`.

**Release-dependency constraint:** per [`AGENTS.md`](../AGENTS.md), a publishable crate must not
dev-depend on another publishable workspace crate without a normal dependency on it. Cross-crate
interoperability tests (e.g. "the cover art in this FLAC decodes through `gamut-png`") therefore
belong at the `gamut` umbrella layer, not inside `gamut-media`. `mise run check-release-deps`
enforces this.

---

## 8. Roadmap [#217] — containers and tracks

Each phase names its exit test. A phase is done when its test passes against its oracle, not when
the code compiles.

**Phase 0 — gate.** A confirmed direct consumer exists and its access pattern (streaming vs local,
read vs write, which containers) is written down. *Exit: the consumer is named in this document.*
**Nothing below starts before this.**

**Phase 1 — stateless cursor core.** The `step`/`Need`/`cursor` protocol (§3 I4) as a shared
primitive. Retrofit `gamut-isobmff`'s `BoxReader` onto it first — it is the existing, well-tested
parser, so the retrofit is where the protocol's ergonomics get proven before three more crates
depend on it.
*Exit: `gamut-isobmff` reads its existing conformance corpus through the streaming driver with
byte-identical results, and through a range-limited driver that never touches `mdat`.*

**Phase 2 — ISOBMFF movie extension.** `moov`/`trak`/`mdia`/`minf`/`stbl` structure; sample tables
(`stsd`/`stts`/`stsc`/`stsz`/`stco`/`co64`/`stss`/`ctts`), edit lists, fragmented MP4
(`moof`/`traf`/`trun`/`sidx`/`mfra`). Sample payloads and codec-private records stay opaque. Removes
the `Unsupported` rejection of `moov`. *Exit: differential structure parity with `mp4box -info` and
`mp4parse` across a fixture corpus; `read(&write(&x)?) == x` for files gamut writes.*

**Phase 3 — EBML and Matroska.** `gamut-ebml`: EBML element parsing (RFC 8794), then the Matroska
semantic layer (RFC 9559) — Segment, `Tracks`, `Cues`, `Chapters`, `Attachments`, `Tags`, cluster
and block framing with opaque frame payloads. *Exit: `mkvalidator` accepts written files; structural
parity with `mkvinfo` on a foreign corpus.*

**Phase 4 — Ogg and RIFF/WAVE.** `gamut-ogg`: page/packet/granule layer (RFC 3533) with codec setup
headers located but not parsed beyond their comment block. `gamut-riff`: WAVE, BWF (`bext`, EBU Tech
3285), RF64 (`ds64`), and AVI structure. *Exit: round-trip equality; BWF conformance against a
broadcast fixture set.*

**Phase 5 — surfacing.** `gamut-media` facade; umbrella features; `gamut-cli` inspection subcommands
matching the existing `gamut isobmff inspect` shape; `gamut-ffi` cursor exposure; `gamut-wasm`.
*Exit: the CLI reports MediaInfo-comparable structure for every phase 2–4 container.*

**Phase 6 — breadth.** MPEG-TS/PS and ASF read-only structure, if a consumer needs them (§9).

## Roadmap [#216] — metadata

Runs **interleaved** with the above, not after it: each tag carrier lands as soon as its container
can locate the carrier's bytes. This is what [#216] means by going hand-in-hand with video and audio
coverage.

**Phase 0 — gate.** Shared with [#217] phase 0.

**Phase 1 — capability introspection.** The programmatic compatibility surface [#216] asks for by
name: `is_mime_supported`, `extract_if_supported`, and a machine-readable capability table
(format × carrier × read/write) that is **generated from the same data the docs render**, so the
matrix in §6 cannot drift from the code. Deliverable before any new carrier lands, because it is the
thing that makes partial coverage honest to a consumer.

**Phase 2 — carrier leaves.** `gamut-id3`, `gamut-vorbis-comment`, `gamut-ape`, and the native FLAC
block layer. Each read *and* write from day one (§5), each with its own differential oracle
(ExifTool for ID3, `metaflac`/`flac` for FLAC blocks, `lofty` as a cross-check).

**Phase 3 — container-located carriers.** `ilst`, `ID32`, `bext`/`iXML`/`_PMX`, Matroska `Tags`,
AIFF text chunks — each wired to the container crate that locates it, each landing with its
container phase.

**Phase 4 — unified model.** `gamut-media`'s tag model over the leaves: one logical field set
projected onto per-carrier serializations, with an explicit conflict policy when a file carries the
same datum in two carriers. The `ConflictPolicy` in
[`gamut-iptc`](../crates/gamut-iptc)/`gamut-metadata` — which already reconciles IIM against XMP —
is the precedent and should be the same shape, not a parallel invention.

**Phase 5 — vertical integration.** Embedded pictures (`APIC`, `PICTURE`, `covr`) route through
gamut's own image crates: read a cover, re-encode it as AVIF, write it back, in one dependency tree.
This is the payoff [#216] opens with and the demonstration that the split facade was worth it.

---

## 9. Deferred and rejected

| Item | Disposition | Reason |
| ---- | ----------- | ------ |
| MPEG-TS / MPEG-PS | Deferred (phase 6) | Transport-layer, mostly a broadcast concern; `mpeg2ts-reader` covers read-only adequately |
| ASF / WMV | Deferred (phase 6) | Declining format; spec access is awkward; no known consumer |
| MXF, GXF, LXF | Rejected | Broadcast-plant formats; enormous descriptive-metadata surface (DMS-1) with no consumer overlap |
| RealMedia, FLV | Rejected | Legacy; no consumer |
| DRM / protection systems (`sinf`, `pssh`, CENC) | Rejected | Present in files; structure parsed as opaque boxes only. gamut never implements key handling |
| A/V codec bitstream parsing | Rejected — charter | I1 |
| Standalone subtitle files | Rejected | §4 |
| Muxing full A/V files | **Open question** | Writing tags into an existing container is in scope. Authoring a new MP4 from raw samples is a different product; revisit only with a named consumer |
| APNG-style A/V frame sequences in WebP/RIFF | Rejected | Coded image sequences; unchanged from `gamut-riff`'s existing position |

---

## 10. Specifications to vendor

Phase 1 of any implementation work vendors these under [`references/`](../references), matching the
existing per-format layout. Implementation without the vendored primary source violates
[`AGENTS.md`](../AGENTS.md)'s "specification as source of truth" rule.

| Area | Specification |
| ---- | ------------- |
| EBML | IETF RFC 8794 |
| Matroska / WebM | IETF RFC 9559 (Oct 2024; updates RFC 8794) |
| Ogg | IETF RFC 3533 (encapsulation), RFC 3534 (media types) |
| Ogg Opus | IETF RFC 7845 |
| FLAC | IETF RFC 9639 |
| Vorbis / Vorbis comment | Xiph.Org Vorbis I specification |
| ISOBMFF | ISO/IEC 14496-12 (already vendored, `references/isobmff`) |
| QuickTime File Format | Apple QTFF reference |
| MP4 registration / iTunes `ilst` | ISO/IEC 14496-14; Apple metadata key reference |
| ID3 | ID3v2.3.0, ID3v2.4.0 informal specifications; ID3v1 |
| APE tags | APEv2 specification |
| RIFF / WAVE | Microsoft/IBM MMRIFF 1.0; EBU Tech 3285 (BWF); EBU Tech 3306 (RF64) |
| AIFF | Apple AIFF/AIFF-C specification |
| C2PA | C2PA Technical Specification 2.4 (2026-04-01), §A.5 (BMFF embedding) — CC BY 4.0 (already vendored, `references/c2pa`) |

---

## Related issues

[#217] · [#216] · [#239] (C2PA) · [#258] (typed metadata extension) · [#168] (stable roadmap) ·
[#186] (`gamut-riff` v1)

[#168]: https://github.com/visualcommons/gamut/issues/168
[#186]: https://github.com/visualcommons/gamut/issues/186
[#216]: https://github.com/visualcommons/gamut/issues/216
[#217]: https://github.com/visualcommons/gamut/issues/217
[#239]: https://github.com/visualcommons/gamut/issues/239
[#258]: https://github.com/visualcommons/gamut/issues/258
