# gamut-xmp

`gamut-xmp` is a pure-Rust **XMP (Extensible Metadata Platform)** RDF/XML metadata parser and
serializer.

## Goals

Part of the [gamut](../../README.md) workspace, this crate models the XMP packet embedded in images
(the WebP `XMP ` chunk, the AVIF/HEIF `mime` item, a JPEG `APP1` segment) so the format crates can
read, preserve, and embed XMP. It is:

- **Memory-safe on hostile input.** `#![forbid(unsafe_code)]` — XMP is XML from untrusted files.
- **Clean-slate from the spec.** Implemented from the **Adobe XMP Specification, Parts 1–3**
  (equivalent to ISO 16684; [`../../references/xmp`](../../references/xmp)), modelling the property
  graph (simple / structured / `Bag`·`Seq`·`Alt`, qualifiers, language alternatives) and canonical
  RDF/XML serialization.
- **Built on a vetted XML reader.** RDF/XML lexing uses [`quick-xml`](https://docs.rs/quick-xml),
  kept an internal detail — no quick-xml type appears in the public API — atop
  [`gamut-core`](../gamut-core). The serializer is hand-written so the canonical output is pinned.

It is also the substrate for IPTC Photo Metadata Core/Extension, which is serialized *as* XMP —
[`gamut-iptc`](../gamut-iptc) builds on this crate.

## Why this crate

The Rust ecosystem's alternatives are `xmp_toolkit` — bindings to Adobe's C++ XMP SDK, which drags
a C++ toolchain into every consumer build — and `xmp-writer`, which only writes. gamut needs both
directions in pure Rust, plus a property the SDK does not promise: **byte-stable canonical
output**. gamut-xmp reads the permissive RDF/XML input XMP allows (Part 1 §7.9 / Annex C) and
emits one fixed canonical form, pinned byte-for-byte by golden tests, so the format crates embed
reproducible, diffable packets. If you need Adobe-SDK parity on every legacy quirk, or non-UTF-8
packets, reach for `xmp_toolkit` instead.

## Usage

```rust
use gamut_xmp::{WellKnownNs, XmpMeta};

let dc = WellKnownNs::DublinCore.uri();

let mut meta = XmpMeta::new();
meta.set_lang_alt(dc, "title", "x-default", "My Photo");
meta.set_text(dc, "rights", "(c) 2026");

// Canonical, embeddable packet bytes...
let packet = meta.to_packet();
// ...read straight back into the same graph.
let parsed = XmpMeta::from_packet(&packet).unwrap();
assert_eq!(parsed.get_lang_alt(dc, "title", "x-default"), Some("My Photo"));
```

`XmpMeta::from_packet` accepts a packet with or without the `<?xpacket?>` wrapper (and tolerates a
leading UTF-8 BOM). `XmpMeta::to_packet` / `to_rdf` emit the canonical RDF/XML; `XmpWriter` exposes
the wrapper / writability / padding knobs plus `with_namespace` to register a preferred prefix for
a custom schema. `WellKnownNs` supplies the standard schema URIs and prefixes so you do not
hand-write them. For in-place editing, `XmpPacket::scan` exposes the envelope (writability,
padding) and `XmpPacket::parse` the graph — `from_packet` is exactly that composition.

## Scope

- **Part 1 (data model + serialization): both directions.** The permissive reader accepts the
  equivalent input forms XMP allows (attribute or element form, `rdf:parseType="Resource"`,
  abbreviations); prohibited constructs (`parseType="Literal"/"Collection"`, `rdf:_n` items,
  top-level typed nodes, duplicate Alt languages) are rejected with typed errors. The writer emits
  one canonical form; control characters that XML normalization would corrupt leave as character
  references.
- **UTF-8 only.** Part 1 §7.1 also permits UTF-16/32 packets; they are rejected with a typed
  `XmpError::Encoding`. Every gamut container writes UTF-8, and adding UTF-16/32 *reading* later
  is a non-breaking change.
- **Default `xml:lang` on `rdf:Description` is not propagated** to the properties it scopes.
  Adobe XMPCore does not materialize it either (pinned in `tests/oracle.rs`); gamut keeps parity
  with the reference engine. Per-property and per-item `xml:lang` are fully supported.
- **Part 2 (standard schemas) is a namespace registry** (`WellKnownNs`), not per-property
  validation: values are uninterpreted text in the model, as the wire format allows. The registry
  also carries the external schemas image-metadata standards layer on XMP — `dcterms` (DCMI
  Metadata Terms), which C2PA uses for `dcterms:provenance`, the URL of an *external* manifest
  store (C2PA 2.4 §11.5). gamut-xmp registers the namespace; reading that property as a
  provenance signal is [`gamut-metadata`](../gamut-metadata)'s job.
- **Part 3 (storage in files) belongs to the format crates by design.** This crate supplies what
  they need — wrapper-optional parse, bare-body serialization (`to_rdf` / `serialize_body`), and
  the writability/padding envelope for in-place editing. Locating packets inside JPEG/TIFF/PNG
  containers, and JPEG's ExtendedXMP spillover, live with the containers.
- **`rdf:ID`/`rdf:nodeID`/`xml:base` and `rdf:about` values are ignored on read** (RDF machinery
  XMP does not use). The emitted xpacket `begin` attribute is empty (`begin=""`, one of the two
  forms §7.3.2 allows; the reader also accepts Adobe's U+FEFF form).

## Status

**Production-ready v1** (issue #189). Implemented: parser + canonical serializer for the full XMP
data model (simple / URI / structured / `Bag`·`Seq`·`Alt`, qualifiers, language alternatives) and
the `<?xpacket?>` wrapper. See [STATUS.md](STATUS.md).

## Migrating from 1.x

`WellKnownNs` is `#[non_exhaustive]` from 2.0: the registry grows with the schemas gamut's crates
need, and each addition is now a minor change instead of a major one. An exhaustive `match` on it
needs a wildcard arm; `WellKnownNs::ALL` still enumerates every registered schema. Nothing else
changed — every existing variant, URI, prefix and method keeps its meaning.

## Validation

- **Golden vectors** transcribed from the Part 1 examples pin the canonical output byte-for-byte
  (`tests/golden.rs`).
- **Round-trip invariants** — the canonical form is a fixed point of parse∘serialize, equivalent
  input forms converge to one graph, control characters survive, and registered prefixes are
  non-semantic (`tests/roundtrip.rs`).
- **Differential oracle** against exiv2's bundled **Adobe XMPCore**: gamut's packets validate and
  round-trip through the reference engine, every `WellKnownNs` URI is vouched for by its schema
  registry, and the default-`xml:lang` posture is pinned to parity (`tests/oracle.rs`; needs the
  `third_party/exiv2` + `third_party/expat` submodules and a C++ toolchain).
- **Mutation-clean** — `cargo mutants` passes with zero gamut-xmp exclusions in
  `.cargo/mutants.toml`.
- **No benches, intentionally** — the crate has no performance contract; packets are a few KB.

## License

Licensed under either of MIT or Apache-2.0 at your option.
