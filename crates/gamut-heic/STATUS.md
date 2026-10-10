# gamut-heic — HEIC/HEIF decoder implementation status

The component surface a conformant HEIF/HEIC still-image **decoder** needs, drawn from every related
spec (ISO/IEC 23008-12 HEIF; ISO/IEC 23000-22 MIAF; ISO/IEC 14496-12 ISOBMFF; ISO/IEC 14496-15 NAL
file format; ITU-T H.265 | ISO/IEC 23008-2 HEVC; ITU-T H.273 CICP). Rows are **technical
components**, not user features. gamut is **decode-only** for HEIF — there is no encoder row.

**Status:** ✅ = implemented · ☐ = deferred (planned, additive, in a named later slice)
· **OOS** = permanently out of scope. The **Slice** column names the delivery: **S1** = the container
parsing + byte accounting + role-typed view slice (issue #238); **S2** = the `hvcC` record +
NAL demux/classification slice (delivered — `src/hvcc.rs`, `src/nal.rs`); **S3** = the pluggable
decoder trait + derivation/colour/transform pipeline slice (delivered — `src/decode.rs`); **S4** =
the libheif differential-oracle slice (delivered — `tests/conformance.rs` over the
`tooling/libheif-oracle` dev-dependency); **S5** = the backend-registry slice (delivered —
`src/backend.rs`, issue #273: `HevcDecoders` + the `gamut-codec-abi` adapter); **S6** = the
high-bit-depth presentation slice (delivered — issue #303: `decode_item_rgba16` and the wider
matrix set, retrofitted **additively** onto the S3 pipeline); **S7** = the C2PA manifest-store
locator slice (delivered — issue #429 under the #239 epic: `src/c2pa.rs`).

This crate builds on [`gamut-isobmff` v1](../gamut-isobmff/STATUS.md): the box grammar, item model,
property/reference parsing, and motion-photo *tolerance* already ship there. This ledger mirrors
that finalized disposition rather than contradicting it — a ✅ here is the HEIF-specific layer
(byte accounting, role typing, primary validation) on top of that shared container.

## Scope & dispositions

**Implemented (S1).** `HeifContainer::parse` gives the *total* byte-accounting representation — a
contiguous, non-overlapping segment list covering every byte of the file (top-level boxes including
unrecognised ones like `mpvd`, an appended foreign stream from a second `ftyp`, or an explicit
trailer), with `meta`/`iprp` shadow-walked for unconsumed boxes — plus `HeifImage`, the role-typed
semantic view over the primary still-image stream (brands, validated primary, item kinds, typed
properties, and the thumbnail/auxiliary/metadata/derivation relationship lenses). The coded
bitstream stays opaque.

**Implemented (S2).** `HevcConfig::parse` decodes the `hvcC` HEVCDecoderConfigurationRecord (§1) into
typed fields plus the parameter-set arrays, reached from a coded item via `HeifItem::hevc_config`;
`iter_nal_units` splits a length-prefixed `hvc1`/`hev1` payload (§2); `NalUnitType`/`NalHeader`
classify each NAL unit (§3); `HevcConfig::validate_still_payload` enforces the still-image IRAP
constraint and `HevcConfig::annex_b` emits a start-coded stream for a downstream decoder — split
(issue #255) into `annex_b_parameter_sets` (the `hvcC` arrays alone: Android MediaCodec `csd-0`, the
VAAPI parameter-set feed, the `AbiHevcDecoder` extradata) and `annex_b_payload` (the item payload
alone: the matching sample buffers), whose concatenation is `annex_b`. The per-platform mapping —
including Apple VideoToolbox, which wants the raw `hvcC` and the length-prefixed payload with **no**
conversion — is tabulated in the crate docs. Still parsing/classification only — the RBSP payloads
stay opaque (decode is S3 / issue #18).

**Implemented (S3).** `HevcDecoder` (`src/decode.rs`) is the pluggable HEVC-intra codestream hook —
object-safe and byte-slice-shaped for FFI adaptation — that a caller implements over a platform
decoder. Around it, `HeifImage::decode_item_planar` resolves item derivation (coded → hook;
`iden` → source; `grid` → plane-domain tile assembly + crop, checked arithmetic, depth-/cycle-limited)
to a raw `DecodedFrame`, and `HeifImage::decode_item_rgba8` / `decode_primary_rgba8` add colour
conversion, nearest-neighbour chroma upsampling, alpha-auxiliary merge, `iovl` source-over
compositing, and the `clap`/`irot`/`imir` transforms (applied in `ipma` order) to yield an
`ImageBuf<Rgba8>`. The pipeline validates the still-image IRAP constraint before the hook is called;
it never itself decodes the HEVC RBSP (that is the caller's hook, issue #18 for a native Rust impl).

**Implemented (S4).** `tests/conformance.rs` is the libheif differential suite: a `De265Decoder`
plugs the reference HEVC decoder (libde265, via the dev-only `tooling/libheif-oracle` crate) into the
crate's `HevcDecoder` seam, and gamut-authored fixtures generated at test time (libheif + kvazaar,
no committed binaries) are cross-checked against libheif — container structure vs `introspect`,
presentation pixels vs `decode_primary_rgba` (tight bound), planar samples bit-exact vs a direct
`decode_hevc_intra`, orientation (`irot`) direction, motion-photo byte accounting, and hvcC/YUV
coherence. See the `references/heif` "Oracle" section.

**Implemented (S5, issue #273).** `HevcDecoders` (`src/backend.rs`) is the ordered backend registry
for the S3 seam, retrofitted **additively**: it *is* a `HevcDecoder`, so it drops into every existing
`&mut dyn HevcDecoder` call site with no signature change, and the new `HevcDecoder::supports` probe
is defaulted (`true`) so every pre-registry implementation keeps compiling. Backends are pushed in
preference order (`push_backend`, `Box<dyn HevcDecoder + Send>` — `Send` is bound at insertion, not
as a trait supertrait); `supports() == false`, or the `BACKEND_DECLINED` late-decline sentinel, is
the only fall-through, and a backend that accepts and then fails propagates its error with no later
backend consulted. There is **no implicit software tail** — gamut ships no in-tree HEVC codestream
decoder (issue #18) — so an empty registry or an all-declining one returns
`Error::Unsupported(NO_BACKEND)`. `AbiHevcDecoder` adapts a `gamut_codec_abi::Decoder` (pure-Rust or
a bridged C vtable) onto the seam: it lowers `HevcConfig` to a `HEVC_CODEC_ID` `StreamConfig` with
Annex-B parameter sets as extradata, allocates the `u16` planes itself (`planar_pixel_format` tags
the layout), and maps the written planes back through the validating `DecodedFrame::new`.

**Implemented (S6).** The wider colour surface on the RGBA convenience path landed **additively**,
as the S1 guarantee requires: `decode_item_rgba16`/`decode_primary_rgba16` joined the shipped
`rgba8` pair rather than replacing it, and BT.709/BT.2020 turned `Unsupported` into `Ok` on both.
The 8-bit surface still refuses a >8-bit frame rather than narrowing it, since that would trade an
honest error for silent quality loss; 8-bit BT.601 keeps `gamut-color`'s libwebp-exact inverse so
its output is byte-identical to every previous release.

**Implemented (S7, issue #429).** `HeifContainer::c2pa` / `c2pa_manifest_stores` locate the C2PA
manifest store carried in a top-level `uuid` `ContentProvenanceBox` (C2PA 2.4 §A.5.1) and report it
as opaque bytes plus its exact byte range. The range covers the store *only*: the box header, the
16-byte extended type, the `FullBox` version/flags, the null-terminated `box_purpose` string, the
8-byte merkle offset that §A.5.3 places in front of the store for the `manifest` and `original`
purposes, and any trailing padding are all excluded, the store's own outer JUMBF `LBox` being what
bounds it rather than the box length (the `LBox` width and endianness are traceable within C2PA 2.4
only incidentally, via §8.4.2.3's definition of the `c2sh` salt box; the general JUMBF grammar is
ISO/IEC 19566-5, and no box type code is read — see below). For `update`, §A.5.3 states no framing
at all — its only sentence about that purpose constrains the store's contents — while the `c2pa-rs`
reference implementation writes and skips the 8-byte offset for `update` exactly as for the other
two, so real mid-update files carry it. Rather than pick a reading, the locator probes offset 8
first and falls back to offset 0, accepting the first that yields a valid `LBox` bound and reporting
nothing if neither does; `manifest` and `original`, whose framing the specification does state, are
not probed. Every location decision is decided by `LBox` validity alone, which is
**content-dependent**: a JUMBF superbox is `LBox`, `TBox`, then a length-prefixed interior, so
reading an `LBox` 8 bytes into a store that does not begin with a merkle offset lands past both
header fields on the first interior box's own length — small, plausible and in-bounds — which can be
accepted and trim the reported store to a fragment. That shape is shared by all three purposes, and
what differs is only how a file reaches it: a spec-conformant `manifest`/`original` store and a
`c2pa-rs`-written `update` store are located exactly, an out-of-spec `manifest`/`original` store
written without the stated offset is mis-bounded rather than rejected (a single stated offset is not
self-checking either), and the offset-less `update` layout is the one *in-spec* layout exposed,
which no known writer emits. The fallback is nonetheless strictly better than a fixed 8-byte skip,
since it runs only where the fixed offset found nothing. What would close it is a `TBox` check: a
store's own header is `LBox`+`jumb`, whereas a wrong offset lands on an interior box carrying its
own type. That constant is traceable — §A.3.9 requires a JPEG XL file to carry the store in a "JUMBF
(`jumb`) superbox" and §15.12.3.2 calls it "a top level JUMBF box (JUMB)" — but both sentences are
JPEG XL clauses attributing the box to ISO/IEC 18181-2 clause 9.3 rather than defining it, so
asserting it is a deliberate deferral (it narrows what is reported), not an absence of source. The
constant ISO/IEC 19566-5 genuinely withholds is a different one: the JUMBF Description Box layout
needed to read the store's JUMBF type UUID, which §11.1.4.2 does give as
`63327061-0011-0010-8000-00AA00389B71`. See the deferred row below. `merkle` boxes and every
unrecognised `box_purpose` are not manifest stores and are not reported; a `uuid` box nested in
`meta` is not one either and keeps surfacing through `unknown_meta_boxes`. The scan covers the
top-level boxes of the *primary* stream, so a box inside an appended vendor stream or a trailer is
not seen — which excludes an `update` box placed, as §A.5.3 requires, last in a motion-photo file
that appends a second whole file; reaching into an appended stream is a container-level change.
Malformed framing yields `None`, never an error — this is a lens over bytes that happen to be
present. A mid-update file carrying both an `original` and an `update` store reports both, in file
order, with their purposes: choosing the *active* manifest is a validator's judgement and this crate
reaches no verdict. The reported range is for observability and byte accounting only — it is **not**
a BMFF exclusion range, since `c2pa.hash.bmff.v3` excludes by box path, not by byte offset (§18.6,
§A.5.6). Nothing inside the store is parsed, no hash is computed and no signature is checked.

**Deferred (planned, additive).** The rows below. Each lands additively — new crate items or new
`#[non_exhaustive]` variants — never a reshape of the shipped surface.

**Permanently out of scope** (workspace charter: image-first, no inter-frame/motion/sequence
coding). Image sequences and tracks — the `msf1`/`hevc`/`hevx` brands, `moov`/`trak`/`mdia`/`stbl`
and `hvc1` sample entries — and all HEVC inter-coding; L-HEVC multi-layer *decode* (layered brands
`heim`/`heis` are read as their single base layer); protected/`uri ` items; external data
references (`dinf`/`dref`, `iloc` `construction_method` 2); mirroring the finalized
[`gamut-isobmff` ledger](../gamut-isobmff/STATUS.md).

## A. Container / byte accounting (23008-12 · MIAF · 14496-12)

| Component | Spec | Status | Slice |
| --- | --- | --- | --- |
| Total representation: contiguous `segments` covering `0..len` (the every-byte invariant) | #238 | ✅ | S1 |
| Top-level box walk (shared `gamut_isobmff::BoxReader`), unknown boxes surfaced verbatim | 14496-12 §4.2 | ✅ | S1 |
| Unknown top-level box (e.g. Google `mpvd` Motion Photo Video Data) retained as `SegmentKind::Box` | Android Motion Photo; `references/heif` §8a | ✅ | S1 |
| Appended foreign stream from a second top-level `ftyp` (Samsung motion-photo MP4) retained opaque | `references/heif` §8b | ✅ | S1 |
| Trailing non-box bytes (Samsung SEF trailer) retained as `SegmentKind::Trailer` (post ftyp+meta) | `references/heif` §8b | ✅ | S1 |
| Stop rules identical to `gamut_isobmff::read` (first ftyp wins; trailer only after ftyp+meta) | 14496-12 | ✅ | S1 |
| Meta-level accounting: `meta`/`iprp` children not consumed by the model surfaced as `UnknownBox` (e.g. `dinf`/`dref`, `uuid`) | 14496-12 | ✅ | S1 |
| C2PA manifest store located in a top-level `uuid` `ContentProvenanceBox`: opaque bytes + exact byte range, purposes `manifest`/`original`/`update` (`c2pa`, `c2pa_manifest_stores`) | C2PA 2.4 §A.5.1, §A.5.3, §8.4.2.3 (`references/c2pa` pending, #431) | ✅ | S7 |
| Store bounding is `LBox`-only and content-dependent (`LBox` validity alone cannot separate a store bound from a plausible interior length). Two routes close it: assert the `jumb` `TBox` — traceable to §A.3.9/§15.12.3.2 but only as a JPEG XL aside, so it is a maintainer call because it narrows what is reported — or confirm the store by §11.1.4.2's JUMBF type UUID, which needs 19566-5's Description Box layout. A `c2pa-rs` oracle fixture would settle either empirically | C2PA 2.4 §A.3.9, §11.1.4.2, §A.5.3; ISO/IEC 19566-5 (not vendored) | ☐ | #239 oracle |
| C2PA store surfaced through the `gamut-metadata` facade as a `MetadataBlock` (the store lives in a top-level `uuid` box outside the item model `HeifImage::blocks` reads; a caller appends `MetadataBlock::C2pa(HeifContainer::c2pa().bytes)` itself) | C2PA 2.4 §A.5 | ☐ | later |
| C2PA validation: JUMBF interior parse, `c2pa.hash.bmff.v3` hard binding, signature/trust verification | C2PA 2.4 §18.6, §A.5.6 | ☐ | user / #239 |
| `ftyp` brands + `is_hevc_still` (`heic`/`heix`/`heim`/`heis`, or `mif1`+`hvcC` primary) | 23008-12; `references/heif` §7 | ✅ | S1 |
| Sequence brands `msf1`/`hevc`/`hevx` (image sequences) | `references/heif` §7 | OOS | OOS |

## B. Item model / roles (23008-12 · MIAF)

| Component | Spec | Status | Slice |
| --- | --- | --- | --- |
| Primary-item validation: `pitm` names an existing, non-hidden item | 23008-12 | ✅ | S1 |
| Item kind from `item_type`: coded (`hvc1`/`hev1`/`av01`…), `grid`/`iovl`/`iden`, `Exif`/`mime` | 23008-12 §6 | ✅ | S1 |
| `hvc1` vs `hev1` inband-parameter-set distinction | 14496-15 §8.3.2; `references/heif` §2 | ✅ | S1 |
| Descriptive properties: `ispe`/`pixi`/`colr`/`clli` typed accessors | 23008-12 §6.5 | ✅ | S1 |
| Transformative properties `clap`/`irot`/`imir` in `ipma` order + MIAF order check | 23008-12 §7; MIAF; `references/heif` §5 | ✅ | S1 |
| `pasp` pixel aspect ratio accessor | 14496-12 §12.1.4 | ✅ | S1 |
| Unsupported-essential-property flag (MIAF: don't render an unknown essential property) | MIAF §7.3.6 | ✅ | S1 |
| Raw `hvcC`/`av1C` codec configuration exposed as opaque `(type, body)` | 14496-15 §8.3.3 | ✅ | S1 |
| Thumbnail lens (`thmb`) / auxiliary lens (`auxl`) | 23008-12 §6 | ✅ | S1 |
| Alpha/depth auxiliary by `auxC` URN (`hevc:2015:auxid:1/2`, `mpegB:…:alpha/depth`) | 23008-12 §6.5.8; MIAF §7.3.5; `references/heif` §6 | ✅ | S1 |
| Premultiplied-alpha (`prem`) relationship | 23008-12 §6; `references/heif` §6 | ✅ | S1 |
| Metadata lens: Exif/XMP items via `cdsc` (metadata → image) + `exif()`/`xmp()` | 23008-12 §A; `references/heif` §9 | ✅ | S1 |
| Derived-image sources (`dimg`), `grid` payload + tile-count validation, `iovl` payload | 23008-12 §6.6.2; `references/heif` §4 | ✅ | S1 |
| `iden` identity derived item recognised (kind); source via `dimg` | 23008-12 §6.6.2.1 | ✅ | S1 |
| Entity groups + `altr` alternatives lens | 14496-12; MIAF | ✅ | S1 |
| Exif `ExifDataBlock` lens: `HeifItem::exif_tiff_stream` applies the 4-byte `exif_tiff_header_offset` and yields the TIFF stream (`II`/`MM`); `HeifItem::icc_profile` yields the `rICC`/`prof` bytes regardless of `nclx` order | 23008-12 §A.2.1; `references/heif` §9 | ✅ | #420 |
| Decoded Exif/XMP/ICC bytes → the `gamut-metadata` facade: `HeifImage::blocks` (`MetadataBlock`s) and `HeifImage::metadata` (`Metadata`), behind the opt-in `metadata` feature (a normal, optional dependency). Pinned by typed extraction from an authored fixture at offsets 0 and 6. **Oracle cell not covered:** `tooling/exiv2-oracle` is block-level and in-memory (no HEIF reader), so "exiv2 reads the items out of the HEIC" is untested; the item bytes are pinned byte-exact against libheif (`tests/conformance.rs`) and the leaf crates pin the payloads against exiv2 (#510) | 23008-12 §A; issue #420 | ✅ | #420 |
| Protected / `uri ` items; external data references | 23008-12 | OOS | OOS |

## C. HEVC configuration & NAL layer (14496-15 · H.265)

| Component | Spec | Status | Slice |
| --- | --- | --- | --- |
| Typed `hvcC` HEVCDecoderConfigurationRecord parse (profile/tier/level, arrays) — `HevcConfig::parse` (`src/hvcc.rs`), reached via `HeifItem::hevc_config` | 14496-15 §8.3.3.1; `references/heif` §1 | ✅ | S2 |
| Item payload → length-prefixed NAL unit split (`lengthSizeMinusOne`) — `iter_nal_units`/`NalUnitIter` (`src/nal.rs`) | 14496-15 §8.3.2; `references/heif` §2 | ✅ | S2 |
| NAL unit header classify (VPS/SPS/PPS/SEI/IRAP) + still-image IRAP constraint — `NalUnitType`/`NalHeader` (`src/nal.rs`), `HevcConfig::validate_still_payload` | H.265 §7.3.1.2; `references/heif` §3 | ✅ | S2 |
| Annex-B conversion for a downstream decoder — `HevcConfig::annex_b` (`src/hvcc.rs`) | 14496-15 §8.3.2 | ✅ | S2 |
| Parameter-sets-only Annex-B export (MediaCodec `csd-0` / VAAPI) — `HevcConfig::annex_b_parameter_sets` | 14496-15 §8.4 | ✅ | S2 (#255) |
| Payload-only Annex-B export (sample buffers for a separately-configured decoder) — `HevcConfig::annex_b_payload` | 14496-15 §8.3.2 | ✅ | S2 (#255) |
| Per-platform decoder-feed expectations documented (VideoToolbox / VAAPI / MediaCodec) — crate docs | 14496-15 §8.4 | ✅ | S2 (#255) |
| L-HEVC multi-layer decode (`heim`/`heis` beyond base layer) | 14496-15 | OOS | OOS |

## D. Pixel decode & API (H.265 intra · gamut-core)

| Component | Spec | Status | Slice |
| --- | --- | --- | --- |
| Pluggable codestream-decoder hook — `HevcDecoder` trait + `DecodedFrame` contract (`src/decode.rs`), object-safe & FFI-adaptable | #238 | ✅ | S3 |
| Planar decode surface `decode_item_planar` (coded → hook; still-IRAP validated first) | 14496-15; H.265 | ✅ | S3 |
| `grid`/`iden` derived-image assembly to planes (`iovl` on the RGBA surface); cycle-/depth-limited, checked | 23008-12 §6.6.2 | ✅ | S3 |
| RGBA presentation surface `decode_item_rgba8`/`decode_primary_rgba8` (colour + alpha + transforms) | 23008-12; H.273 | ✅ | S3 |
| High-bit-depth presentation surface `decode_item_rgba16`/`decode_primary_rgba16` (8..=16-bit in, samples normalized to the full 16-bit range) | H.273 | ✅ | S6 |
| Colour conversion: BT.709 (1), BT.601 / BT.470 B,G (5/6), BT.2020 NCL (9) via `gamut-color`, identity (0) GBR, monochrome; missing-`colr` default BT.601 limited | H.273; MIAF | ✅ | S3/S6 |
| Sample-scale contract: every surface carries samples over its type's full range; the coded depth is read from the planar surface / `pixi`, not the buffer | H.273 | ✅ | S6 |
| Nearest-neighbour co-sited chroma upsampling (4:2:0 / 4:2:2 → 4:4:4) | 23008-12 | ✅ | S3 |
| Alpha-auxiliary merge (dims-checked, bit-depth-scaled); `prem` surfaced, not un-premultiplied | 23008-12 §6 | ✅ | S3 |
| Transformative-property application (`clap`/`irot`/`imir`) in `ipma` order to output pixels | 23008-12 §7; 14496-12 §12.1.4; MIAF | ✅ | S3 |
| `iovl` source-over compositing onto a filled canvas (signed offsets, clipping) | 23008-12 §6.6.2.4 | ✅ | S3 |
| Ordered backend registry `HevcDecoders` (`push_backend`, itself a `HevcDecoder`; `Send` bound at insertion) | #241 / #273 | ✅ | S5 |
| `HevcDecoder::supports` capability probe (defaulted `true`, additive to the S3 trait) | #273 | ✅ | S5 |
| Fallback contract: push order; `supports()==false` / `BACKEND_DECLINED` fall through, accepted-then-failed propagates; no implicit software tail ⇒ `Error::Unsupported(NO_BACKEND)` | #241 | ✅ | S5 |
| `AbiHevcDecoder`: `gamut_codec_abi::Decoder` ⇄ `HevcDecoder` (StreamConfig/`ImageDesc` lowering, `Status::UNSUPPORTED` ⇒ late decline) | #241 / #272 | ✅ | S5 |
| HEVC-intra reconstruction (slice/CTU/transform/intra-pred/in-loop filters) — delegated to the caller's `HevcDecoder` | H.265 (ITU-T) | ☐ | user / #18 |
| Matrix coefficients outside the modeled Kr/Kb set (YCgCo 8, chromaticity-derived 12/13/14) and unmodeled coded depths (9/11/13…) on the RGBA surfaces — explicitly refused, never approximated | H.273 | ☐ | later |
| 10/12-bit differential vs libheif (needs a Main10 encode path in the oracle's `encode_rgba_to_heic`; the wide surface is validated against an independent reference conversion instead) | — | ☐ | later |
| Depth-map auxiliary presentation | 23008-12 §6.5.8 | ☐ | later |
| HEVC inter coding (motion, reference frames, sequences) | H.265 | OOS | OOS |
| CLI / wasm / ffi wiring | gamut-{cli,wasm,ffi} | ☐ | later |

## E. Conformance oracle

| Component | Spec | Status | Slice |
| --- | --- | --- | --- |
| libheif (libde265) differential parse + decode oracle (`tooling/libheif-oracle` over the `third_party/{libheif,libde265,kvazaar}` submodules) — `tests/conformance.rs` | `references/heif` "Oracle" | ✅ | S4 |
| `De265Decoder`: the reference HEVC decoder plugged into the `HevcDecoder` seam (container plumbing proven bit-exact against a direct `decode_hevc_intra`) | #238; `references/heif` §§1–3 | ✅ | S4 |
| Structure conformance: `HeifContainer::parse` vs libheif `introspect` (primary id, item ids+types, ispe dims, alpha, thumbnails, Exif/XMP bytes incl. the `exif_tiff_header_offset`) | 23008-12; `references/heif` §9 | ✅ | S4 |
| Presentation-pixel conformance vs libheif `decode_primary_rgba` (tight measured bound; alpha exact) + orientation `irot`/`imir` direction | 23008-12 §7; H.273 | ✅ | S4 |
| Motion-photo overlay: appended `mpvd` / second-`ftyp` / trailer decode identically to the pristine still (byte accounting) | `references/heif` §8 | ✅ | S4 |
| Reference backend driven **through** the registry: `De265Decoder` pushed into `HevcDecoders` decodes the fixture identically (planar + RGBA) to the direct hook; an empty registry errors | #273 | ✅ | S5 |
| Nokia HEIF reference software as a secondary fixture source | `references/heif` "Oracle" | ☐ | later |
| Real multi-tile `grid` differential (libheif+kvazaar do not auto-emit grids; the oracle API exposes no grid knob — synthetic grid-assembly unit tests cover the path) | 23008-12 §6.6.2.2 | ☐ | later |

## The S1 guarantee

`gamut-heic`'s container slice promises: `HeifContainer::parse` accounts for **every byte** of the
input (the segments tile `0..len` exactly, by construction and pinned by tests), surfaces every
unknown top-level and `meta`/`iprp` box verbatim, retains appended vendor streams and trailers as
opaque byte ranges, and exposes a role-typed view whose accessors are computed lenses over the
single-source-of-truth `gamut_isobmff::IsoBmffImage` — no state duplicated, the primary item
validated at parse. Every deferred row lands additively; the S1 public surface is never reshaped.
