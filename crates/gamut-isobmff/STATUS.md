# gamut-isobmff — ISOBMFF still-image container implementation status

`gamut-isobmff` factors the ISO Base Media File Format box tree (ISO/IEC 14496-12) and its HEIF
still-image profile (ISO/IEC 23008-12) out as a shared primitive consumed by both
[`gamut-avif`](../gamut-avif) (AV1 still images) and [`gamut-heic`](../gamut-heic) (HEVC still
images). It models *structure only* — the box tree, item properties, item references, and the
offset-driven read/write spine — never the coded bitstream, which is carried opaquely.

**Keystone:** the **single-pass `iloc` back-patch** in the writer ([`write`](src/writer.rs)). Each
item's `extent_offset` is an absolute file offset into `mdat` that is only known after `meta` is
sized, so the writer reserves the slot while emitting `meta` and patches it once `mdat` is placed; a
`read(&write(&img)?) == img` round-trip reproduces the model exactly.

## Scope

This crate is **image-first** (the workspace charter forbids inter-frame/sequence coding), so it
covers the HEIF *still-image* profile, not the full ISOBMFF movie structure. The authoritative
container ledger is [`gamut-avif/STATUS.md`](../gamut-avif/STATUS.md) §A; the v1 surface covers the
container rows of every planned milestone (M0–M5), so future codec work is additive here — new
`Item`/`Property` values, never a model reshape.

**Asymmetric by design:** the writer *normalises* (always the smallest box versions — `pitm` v0,
`iloc` v0 single-extent into `mdat`, `infe` v2, `iref` v0, 16-bit item ids — validated up front by
the fallible [`write`]); the reader additionally accepts the foreign-encoder repertoire (`iloc`
v1/v2, `idat` placement, multi-extent payloads, 32-bit item ids, 16-bit `ipma` indices). Round-trip
is therefore model-exact for files this crate writes, and *equivalent-but-normalised* for foreign
files.

## Phases

| Phase | Spec | Scope | Status |
| ----- | ---- | ----- | ------ |
| P1 | 14496-12 §4.2 | Typed box-tree model (`IsoBmffImage`/`Item`/`ItemReference`/`EntityGroup`/`Property`/`PropertyKind`) + low-level `BoxBuilder`/`BoxReader` | ✅ done |
| P2 | 14496-12; 23008-12 | Writer: `ftyp`, `meta` (`hdlr`/`pitm`/`iloc` v0/`iinf`+`infe` v2/`iref` v0/`iprp`/`grpl`), `mdat`; `ispe`/`pixi`/`colr` (`nclx`+ICC)/`irot`/`imir`/`clap`/`pasp`/`auxC`/`clli` properties; opaque codec config; model validation (typed errors, no silent truncation) | ✅ done |
| P3 | 14496-12 | **Keystone** — `iloc` extent back-patch + shared `ipco` dedup → per-item `ipma` (8/16-bit forms) | ✅ done |
| P4 | 14496-12; 23008-12 | Reader: bounds-checked box walk including `largesize`, size-0, and UUID user types; foreign-file repertoire (`iloc` v0–v2, `mdat`/`idat`, base offsets, 0/4/8-byte fields, multi-extent, `pitm`/`ipma` v0–v1, `infe` v2–v3, `iref` v0–v1); motion-photo tolerance (stop at a second top-level `ftyp` / at a malformed trailing box once `ftyp`+`meta` are seen); `read(&write)` round-trip; unrecognised property boxes preserved verbatim | ✅ done |
| P5 | — | Robustness: truncation/overrun/size/index guards ✅; counts never trusted for allocation ✅; total payload capped at input size (anti amplification) ✅; spec fixtures independent of the writer ✅; fuzz corpus ☐ | ◑ partial |
| P6 | — | Differential oracle: libavif/dav1d parses the container and reproduces pixels (via `gamut-avif/tests/decode_roundtrip.rs`) | ✅ via codec |
| P7 | 14496-12 §4.2; C2PA 2.4 §A.5.3 | Top-level boxes the model does not otherwise own (`IsoBmffImage::top_level_boxes`, #443): retained on read with the position they were found at, written after `ftyp` (`AfterFtyp`) or after `mdat` (`Trailing`); `read`→`write` byte-identical for a file *this crate wrote* carrying them (foreign files stay equivalent-but-normalised); libavif decodes an AVIF carrying a C2PA `uuid` box unchanged (`gamut-avif/tests/remux_roundtrip.rs`) | ✅ done |

## Demonstration surface

The read/write spine is exercised end-to-end — without a working codec, since the coded bitstream
is opaque — by the `gamut isobmff` CLI (`inspect`/`remux`/`build`) and two oracle tests:

- **`inspect`** parses real third-party `.avif`/`.heic` and prints the box structure; across the
  libavif conformance corpus it reads every still image, including alternate-size boxes, while
  rejecting out-of-scope image sequences and malformed input with a typed error.
- **`remux`** re-serialises a container and re-parses it, verifying `read(&write) == model` on
  foreign files; `gamut-avif/tests/remux_roundtrip.rs` additionally confirms libavif decodes the
  re-muxed container to **pixel-identical** output (the coded payload survives verbatim).
- **`build`** constructs a synthetic container covering every modelled box, property, reference,
  and entity group.

## Payload helpers

- **`ImageGrid`** (23008-12 §6.6.2.3.2) — `ImageGrid::parse`/`to_bytes` types the `grid`
  derived-image payload (tile rows/columns + assembled output size). A `grid` item's `dimg`
  references and payload bytes already round-trip through the box model; this helper types the
  payload geometry for consumers, while the tile payloads stay opaque. Additive (semver-minor).
- **`ImageOverlay`** (23008-12 §6.6.2.4.2) — `ImageOverlay::parse`/`to_bytes` types the `iovl`
  derived-image payload (canvas size + fill colour + each input's signed top-left offset). The
  `reference_count` is not stored in the payload — it is implied by the `dimg` reference count — so
  `parse` takes it as a parameter and rejects a truncated or trailing-byte payload; `to_bytes`
  picks the compact 16-bit form only when the dims fit `u16` *and* every signed offset fits `i16`.
  Additive (semver-minor).

## Top-level boxes

`IsoBmffImage::top_level_boxes` (#443) holds, in file order, every top-level box of the primary
stream that the model does not otherwise own — anything but `ftyp`, `meta`, `mdat` and the
appended motion-photo stream `read` stops at. Each `TopLevelBox` carries the box type, the 16-byte
user type when it is a `uuid` box (the `RawBox` split: `payload` omits the user type), the payload
verbatim, and a `TopLevelPosition`:

- **`AfterFtyp`** — written between `ftyp` and `meta`, so before the first `mdat`: the C2PA 2.4
  §A.5.3 placement of a `ContentProvenanceBox` (`uuid`, user type
  `D8FEC3D6-1B0E-483C-9297-5828877EC481`, §A.5.1).
- **`Trailing`** — written after `mdat`.

`read` assigns the position from where it met the box: after `mdat` → `Trailing`, otherwise
`AfterFtyp`. `write` always emits `ftyp`, the `AfterFtyp` boxes, `meta`, `mdat`, then the
`Trailing` boxes, so a foreign file's box moves on round-trip wherever its layout differs: a box
*between* `meta` and `mdat` is written back between `ftyp` and `meta` (for a C2PA box a move
between two lawful positions: §A.5.3 requires only after `ftyp` and before the first `mdat` and
any `moov`, and both satisfy it); a box before `ftyp` is written back after `ftyp`; and in a file
with `mdat` before `meta`, a box between `mdat` and `meta` is `Trailing` and is written back
after `meta` and `mdat`. `TopLevelPosition` is `#[non_exhaustive]` so a finer
position can be added later without a major bump. Files this crate writes round-trip
byte-identically. `write` rejects a top-level box typed `ftyp`/`meta`/`mdat` (the model emits
those itself), `moov`/`trak` (image sequences, `Unsupported` as on read), one whose `user_type`
does not pair with the `uuid` type, one whose complete box (header included) does not fit the
32-bit size field, or a list that interleaves positions (an `AfterFtyp` box after a `Trailing`
one) — the file holds the groups in order, so `read` could not give the interleaved order back;
`IsoBmffImage::push_top_level_box` appends at the end of a box's position group so a parsed
model that already carries trailing boxes never becomes interleaved.

Two costs of retaining rather than dropping, accepted knowingly:

- **A retained box is an owned copy.** A motion-photo `mpvd` box (Google) that carries a whole
  video is copied into the model on `read`, where it was previously skipped; the copy is bounded
  by the input size, and `walk_segments` continues to surface the same bytes zero-copy for
  consumers that only account for them.
- **An `iloc` extent may address bytes inside a retained box** (the reader resolves extents
  against the whole file). Such bytes are then carried twice on `write` — once verbatim in the box,
  once as the item's payload in `mdat`. No known encoder does this; a file that does still
  decodes identically, it is merely larger.

Deliberately **not** here: parsing the C2PA `box_purpose`, merkle offset or JUMBF `LBox` — that
typed lens is `gamut-heic`'s `c2pa` module (#429); modelling a manifest store as an item (C2PA
Appendix A defines only the top-level `uuid` carriage for BMFF); promoting a `uuid` box found
inside `meta` (surfaced by `walk_meta_children`) to the top level.

**Semver:** adding the field made `IsoBmffImage` `#[non_exhaustive]` (a major bump); construct it
with `IsoBmffImage::new(..)` plus `with_minor_version`/`with_groups`/`with_top_level_boxes`, so the
next field is a minor release.

## Exported walk primitives

Byte accounting itself is `walk_segments`/`walk_meta_children` (issue #436): every input byte maps
to exactly one `Segment` — a top-level box, an appended foreign stream from a second `ftyp`, or a
trailer — and `walk_meta_children` does the same one level down for boxes `read` does not consume.
It stops on exactly `read`'s rules, so the accounting walk and the semantic parse cannot disagree
about where the primary stream ends. It is deliberately not folded into `read`, which is strictly
stricter (it rejects `moov`/`trak` where it meets them); the two are documented together in
`src/segments.rs`.

`BoxReader` and `RawBox` remain re-exported from the crate root so a consumer can account for
something the shared walk does not model: `RawBox::offset` is the absolute header offset within
the reader's slice, and `BoxReader::position`/`remaining` bracket the cursor.
`RawBox::user_type` identifies UUID boxes while `RawBox::payload()` omits their 16-byte user-type
prefix without changing the existing `body` view. The public surface is the walk only
(`new`/`next_box`/`position`/`remaining` + `RawBox` fields/`payload`); scalar field readers and the
writer's `BoxBuilder` stay crate-private. Additive (semver-minor).

## Deferred (planned, additive — no model reshape)

- **Typed `mdcv`/`cclv`/`amve`/`reve`/`ndwt` HDR properties.** Carried verbatim as
  `PropertyKind::Other` (exactly what libavif does — it types only `clli`, which we also type).
  No vendored primary source pins their field layouts yet; `PropertyKind` is `#[non_exhaustive]`
  so typing one later is semver-minor.
- **Writer emission options** (`idat` placement, multi-extent, `iloc` v1/v2, 32-bit-id boxes).
  The writer intentionally normalises; if a consumer ever needs layout control it arrives as a
  separate options-taking entry point (semver-minor), not a change to `write`.
- **Fuzz corpus** (P5). The reader is bounds-checked, allocation-capped, and fixture-tested; a
  libFuzzer/AFL corpus remains the missing hardening step.
- **Streaming/`ReadAt` input.** `BoxReader` intentionally remains a zero-copy cursor over one byte
  slice. Large-file consumers currently buffer or memory-map their input; a source abstraction can
  be added separately without coupling it to alternate-size parsing.

## Likely out of scope (rejected with a typed error; revisited only if the charter changes)

- **Image sequences/tracks** — `moov`/`trak`/`mdia`/`stbl`, the `av01`/`hvc1` sample entries, the
  `avis` brand (gamut-avif M6). The workspace charter forbids inter-frame/sequence coding; `read`
  rejects a `moov`/`trak` in the *primary* stream as `Unsupported`. (A `moov` inside an appended
  motion-photo stream — everything past a second top-level `ftyp` — is never reached, by design.)
- **Item protection** — `ipro`/`sinf`, `infe` `item_protection_index != 0` (DRM machinery).
- **External data references** — `dinf`/`dref`, `iloc` `data_reference_index != 0` (payloads in
  other files; a still image is self-contained).
- **`iloc` `construction_method` 2** (offsets into another item's payload) — unused by any known
  still-image encoder.
- **`uri ` items** (23008-12 URI-typed metadata) — unused by AVIF/HEIC; `Exif` and `mime`
  (XMP/MPEG-7) items cover real metadata.
- **64-bit writer emission and files at or beyond 4 GiB** — the parser accepts `largesize`, but the
  normalising writer deliberately emits 32-bit sizes and rejects payload/file offsets that do not
  fit `u32`.
- **Non-`pict` handlers** — a `meta` that is not a HEIF image (e.g. ID3 metadata containers).

Round-trip is guaranteed for files this crate's `write` produces (`read(&write(&img)?) == img`,
checked by validation rather than assumed). Foreign files read into an equivalent normalised model;
what they lose is placement/version detail (`idat` vs `mdat`, extent splits, box versions), never
item data, properties, references, or groups.
