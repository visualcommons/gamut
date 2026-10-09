# gamut-isobmff

`gamut-isobmff` is a pure-Rust implementation of the **ISO Base Media File Format (ISOBMFF)
still-image container core**: the `ftyp` brands, the `meta` box of image items with their
properties, references, and payloads, and the offset-driven read/write spine. It models *structure
only* — the coded bitstream (the `av1C`/`hvcC` record and the sample data) stays opaque.

## Goals

Part of the [gamut](../../README.md) workspace, this crate exists because the ISOBMFF/HEIF container
is shared by two otherwise-separate codecs:

- **AVIF** ([`gamut-avif`](../gamut-avif)) — AV1 still images: item type `av01`, codec config `av1C`.
- **HEIC** ([`gamut-heic`](../gamut-heic)) — HEVC still images: item type `hvc1`, codec config `hvcC`.

Factoring the container out keeps the two from duplicating the box tree and the fiddly,
security-sensitive `iloc` offset machinery. It is:

- **Codec-agnostic.** The codec configuration is carried as opaque bytes
  ([`PropertyKind::CodecConfiguration`]), so the same `write`/`read` serve `av01`/`av1C` and
  `hvc1`/`hvcC` with no container changes.
- **Memory-safe on hostile input.** `#![forbid(unsafe_code)]` — ISOBMFF is offset-driven, a classic
  parser-exploit surface (truncation, overruns, out-of-range indices, amplification), so every read
  is bounds-checked and the total resolved payload is capped at the input size.
- **Validating on output.** `write` returns a typed error for a model that cannot round-trip or
  does not fit the still-image box versions, instead of silently truncating fields.
- **Dependency-light.** Builds only on [`gamut-core`](../gamut-core).

## Usage

`write` serialises an [`IsoBmffImage`] (its `ftyp` brands, the `pitm` primary item, the image
items, any entity groups, and any top-level boxes the model does not otherwise own) into a
complete `ftyp` + `meta` + `mdat` file; `read` parses one back. Each [`Item`] carries its type,
name, optional MIME info, hidden flag, typed `iref` references
(`auxl`/`cdsc`/`dimg`/`thmb`/`prem`, …), properties, and payload; the writer derives the `iloc`
offsets and the shared `ipco`/`ipma` so the two are inverse for any file this crate writes.
`IsoBmffImage` is `#[non_exhaustive]`: build it with `IsoBmffImage::new(..)` and the `with_*`
builders.

```rust
use gamut_isobmff::{IsoBmffImage, Item, Property, PropertyKind, read, write};

let img = IsoBmffImage::new(
    *b"avif",
    vec![*b"avif", *b"mif1", *b"miaf"],
    1,
    vec![Item {
        id: 1,
        item_type: *b"av01",
        name: String::new(),
        content_type: None,
        content_encoding: None,
        hidden: false,
        references: vec![],
        properties: vec![Property {
            essential: false,
            kind: PropertyKind::ImageSpatialExtents { width: 64, height: 64 },
        }],
        payload: vec![/* the coded bitstream, opaque to this crate */],
    }],
);
let bytes = write(&img)?;
assert_eq!(read(&bytes)?, img);
```

A top-level box the model does not otherwise own — the C2PA `ContentProvenanceBox` (a `uuid` box
with user type `D8FEC3D6-1B0E-483C-9297-5828877EC481`, C2PA 2.4 §A.5.1), a `free`, a vendor box
— lives in `top_level_boxes` as a [`TopLevelBox`] and is written at its [`TopLevelPosition`]:
`AfterFtyp` boxes between `ftyp` and `meta` (so before the first `mdat`, the §A.5.3 placement),
`Trailing` boxes after `mdat`. `read` keeps every such box with the position it found it at, so a
file carrying one round-trips byte-identically:

```rust
use gamut_isobmff::TopLevelBox;

let c2pa = TopLevelBox::uuid(C2PA_UUID, manifest_store);
let img = img.with_top_level_boxes(vec![c2pa]);
```

The payload stays opaque here; locating and bounding the manifest store inside it is
[`gamut-heic`](../gamut-heic)'s `c2pa` module.

See [`gamut-avif`](../gamut-avif) for the full encode path that drives this crate (it builds the
`av1C` record and the AVIF brand set, then calls `write`).

## Inspect and re-mux from the CLI

The `gamut` CLI (with the `isobmff` feature, on in `all`) exercises this crate's whole read/write
surface on real files — no working codec required, since the coded bitstream is opaque:

```console
# parse a still-image .avif/.heic and print its box structure (brands, items, properties,
# references, grid geometry, entity groups)
$ gamut isobmff inspect image.avif

# re-serialise a container (normalised box versions, single-extent mdat); the coded payload is
# preserved verbatim, so a real decoder re-decodes the result to identical pixels
$ gamut isobmff remux image.avif out.avif

# build a synthetic container exercising every modelled box, property, reference and group
$ gamut isobmff build demo.avif
```

`inspect`/`remux` accept the foreign-encoder repertoire below, including 64-bit `largesize` and
size-0 boxes. Out-of-scope structures such as image sequences (`moov`/`trak`) and malformed input
are rejected with a typed error rather than mis-parsed (this crate is `#![forbid(unsafe_code)]` and
bounds-checks every read).

## Status

Models the HEIF still-image box set: `ftyp`, `meta` (`hdlr`/`pitm`/`iloc`/`iinf`/`iref`/`iprp`/
`idat`/`grpl`), the `ispe`/`pixi`/`colr` (`nclx` + ICC)/`irot`/`imir`/`clap`/`pasp`/`auxC`/`clli`
properties, opaque codec configuration, `mdat`, and every other top-level box of the primary
stream (`uuid`/`free`/vendor) as a positioned `TopLevelBox`. Unrecognised property boxes
round-trip verbatim. The `grid` and `iovl` derived-image payloads are typed by the opt-in
[`ImageGrid`]/[`ImageOverlay`] helpers. The writer normalises to the smallest box versions; the
reader additionally accepts the foreign-encoder repertoire (`iloc` v1/v2, `idat` placement,
multi-extent payloads, 32-bit item ids, 16-bit `ipma` indices, alternate box sizes, and UUID user
types). Image sequences/tracks, item protection, and external data references are out of scope —
see [STATUS.md](STATUS.md) for the full deferred/out-of-scope ledger.

`read` models only the *primary* still-image stream and tolerates real-world "motion photo" files
that append a second, foreign stream: the top-level walk stops cleanly at a second top-level `ftyp`
(the first wins) and at a malformed trailing box once `ftyp`+`meta` are seen. Mapping every byte of
the appended remainder to a box or trailer is a consumer's job (`gamut-heic`), served by the
re-exported [`BoxReader`]/[`RawBox`] box-walk primitives.

Box byte layouts follow ISO/IEC 14496-12 (ISOBMFF) and ISO/IEC 23008-12 (HEIF) — paywalled, so
cross-checked against the public AVIF box table, hand-authored spec fixtures, and a vendored
libavif/dav1d differential oracle (via [`gamut-avif`](../gamut-avif)). See
[`references/isobmff`](../../references/isobmff).

## License

Licensed under either of MIT or Apache-2.0 at your option.
