# gamut-xmp — XMP implementation status

**v1 stabilization: GitHub issue #189** (grown out of the image metadata primitives campaign,
issue #34). Implements XMP (`references/xmp`, Adobe XMP Parts 1–3 = ISO 16684) as an RDF/XML
parser + canonical serializer.

**Keystone:** **canonical RDF/XML serialization** (Adobe XMP Part 1 §7). RDF admits many
serializations of the same graph; the canonical form fixes element-vs-attribute encoding, namespace
placement, and array/struct nesting so output is stable, diffable, and round-trippable. Parsing the
(more permissive) input is comparatively routine.

## Settled decisions

- **XML reader — `quick-xml`.** RDF/XML lexing uses `quick-xml` (a `[workspace.dependencies]` entry),
  kept an internal detail: no quick-xml type appears in the public API (parse failures surface as
  `XmpError`), so the backend can change without a breaking change. The serializer is hand-written to
  pin the canonical byte form.
- **Encoding — UTF-8, no BOM.** Packets are read and written as UTF-8; a leading BOM is tolerated on
  read but never emitted. Part 1 §7.1 also allows UTF-16/32, reported as `XmpError::Encoding`.
- **Conformance oracle — exiv2.** exiv2 bundles Adobe's XMPCore, so it backs both the "Adobe-SDK"
  and "exiv2" checks. The differential gate is `tests/oracle.rs`, against a vendored, statically
  linked exiv2 + expat (`tooling/exiv2-oracle`, built from the `third_party/exiv2` + `third_party/
  expat` submodules). Byte-exact correctness is pinned independently by golden vectors transcribed
  from the Part 1 examples (`tests/golden.rs`).
- **The schema registry is open — `WellKnownNs` is `#[non_exhaustive]`** (issue #449, a 2.0
  change). It was exhaustively matchable through 1.x, so every schema the format and metadata crates
  need would have been a major bump; the attribute pays that once. The first non-Adobe entry is
  `dcterms` (`http://purl.org/dc/terms/`, DCMI Metadata Terms), registered because C2PA 2.4 §11.5 /
  §15.5.3.1 point at an *external* manifest store through `dcterms:provenance`. This crate registers
  the namespace only — what the property *means* is read by `gamut-metadata`, consistent with the
  registry-not-validator posture below — and `tests/oracle.rs` pins that XMPCore reads the property
  back under the `Xmp.dcterms.provenance` key its own registry defines.

## Phases

| Phase | Spec § | Scope | Status |
| ----- | ------ | ----- | ------ |
| P1 | — | Scaffold: crate, workspace wiring, docs, region-free data-model skeleton | ✅ done |
| P2 | Part 1 §7 | `xpacket` wrapper + namespace registry + parse simple (literal) properties | ✅ done |
| P3 | Part 1 §6 | Structured values, `Bag`/`Seq`/`Alt` arrays, qualifiers, language alternatives | ✅ done |
| P4 | Part 2 | Standard schema coverage (dc/xmp/xmpRights/xmpMM/photoshop/exif/tiff/…) | ✅ done |
| P5 | Part 1 §7 | **Keystone** — canonical RDF/XML serialization + packet emit (writable padding) | ✅ done |
| P6 | — | exiv2 differential conformance gate | ✅ done |
| P7 | Parts 1–3 | **v1 stabilization** (issue #189) — API finalization (`XmpPacket::parse` composition, `XmpWriter::with_namespace` prefix registration, model conveniences), conformance audit (control-character escaping fix, trailer `end=` matching, edge-case pins), gamut-iptc dogfood migration, docs | ✅ done |

## Intentional skips (audited for v1)

Each item was re-verified against the spec during the v1 audit (issue #189) and skipped
deliberately:

- **Default `xml:lang` scoping (Part 1 §7.8 / XML 1.0):** an `xml:lang` on `rdf:Description` is
  not propagated to the properties it scopes. Adobe XMPCore does not materialize it either —
  parity is pinned by `tests/oracle.rs::default_xml_lang_on_description_matches_reference`.
  Per-property and per-item `xml:lang` are fully supported.
- **UTF-16/32 packets (Part 1 §7.1):** rejected with a typed `XmpError::Encoding` (see the
  encoding decision above). Read support would be purely additive later.
- **`rdf:ID` / `rdf:nodeID` / `xml:base`, and `rdf:about` values (Part 1 §7.9):** ignored on read
  — RDF reification/base machinery XMP does not use; pinned by reader tests.
- **xpacket `begin` attribute:** written empty (`begin=""`), one of the two forms §7.3.2 allows;
  the reader accepts Adobe's U+FEFF form.
- **Part 3 per-container embedding and JPEG ExtendedXMP:** owned by the format crates; this crate
  supplies wrapper-optional parse, bare-body serialization, and the writable/padding envelope.
- **Per-schema value validation (Part 2):** values are uninterpreted text; `WellKnownNs` is a
  namespace registry, not a validator.
- **Deferred additive API** (post-1.0, no consumer today): an opt-in `XmpMeta::validate()`, and
  nested-structure field lookup.
