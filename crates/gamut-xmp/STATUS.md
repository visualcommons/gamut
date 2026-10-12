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
- **Schema breadth = the issue #421 set of exiv2's documented schemas.** `WellKnownNs` holds 30
  entries: the Adobe Parts 1–2 schemas and structure types, `dcterms`, and the twelve further
  schemas issue #421 selected from those exiv2 documents (<https://exiv2.org/metadata.html>):
  `exifEX`, `aux`, `plus`, `mwg-rs`, `mwg-kw`, `GPano`, `lr`, `MicrosoftPhoto`, `digiKam`,
  `acdsee`, `crss`, `dwc`. exiv2 documents six more that are **not** registered — `kipi`,
  `mediapro`, `expressionmedia`, `MP`, `MPRI`, `MPReg` (its `doc/templates/tags-xmp-*.html.in` pages) — so their
  properties serialize under a synthesized `ns<N>` prefix unless the graph declares a namespace
  for them; registering one is an additive change for a later issue. Each URI is taken from the
  schema owner's specification where one is published and cited on the variant; the vendored
  reference for all twelve is exiv2's own registry (`third_party/exiv2/src/properties.cpp`,
  `xmpNsInfo`), and `tests/oracle.rs` reads a documented property of each back from XMPCore by its
  `Xmp.<prefix>.<name>` key, which fails for a wrong URI *or* a wrong prefix. The registry stays a
  registry: nothing about a value is interpreted. `exifEX` is the exception to the oracle-only
  sourcing: its URI `http://cipa.jp/exif/1.0/` is cited from the vendored CIPA DC-010-2026
  (`references/exif/exif-xmp-dc-010-2026.pdf`, Tables 1/6/20), which also retires
  `…/exif/2.32/` — the URI the Exif 3.0 text's annotation examples (Annex J.2–J.3) still show — so
  that URI is deliberately not registered (issue #516). One divergence is recorded rather than
  hidden: exiv2 appends `/` to a URI ending in neither `/` nor `#` when registering it with XMPCore
  (`XmpProperties::registerNs`), so the engine re-serializes `dwc` as `…/index.htm/` while gamut
  writes the `…/index.htm` exiv2 documents — pinned in `tests/oracle.rs` as an oracle
  normalization. Because that normalization is what exiv2 *emits*, `WellKnownNs::from_uri` also
  recognises `http://rs.tdwg.org/dwc/index.htm/` as a read-only alias (`DWC_URI_TRAILING_SLASH`),
  so a graph parsed from an exiv2-written packet or sidecar re-serializes under `dwc` rather than a
  synthesized prefix; `uri()` still emits the unslashed URI, so gamut's own bytes are unchanged and
  the alias is not an `ALL` entry. The alias buys a **prefix**, not a URI: reading does not
  canonicalize, so such a graph keeps the slashed URI its packet declared and a caller must look the
  property up under `DWC_URI_TRAILING_SLASH` — `get_text(WellKnownNs::DarwinCore.uri(), …)` returns
  `None` for it. `from_uri(u).map(uri) == Some(u)` therefore holds for every registry entry and not
  for the alias, pinned as such in `namespace.rs`.
- **Decided: reading does not canonicalize a well-known namespace URI (issue #547).** Of the two
  properties a `dwc` packet written by exiv2/XMPCore cannot have at once, **byte fidelity is
  kept** — the graph is a faithful record of the namespace URI the packet declared, and writing it
  back emits that same `…/index.htm/` — and **lookup by `WellKnownNs::DarwinCore.uri()` is given
  up** for such a graph: a caller resolves its properties by `DWC_URI_TRAILING_SLASH`, the URI the
  packet carries. Rewriting the declared URI to `uri()` on read would close that asymmetry at the
  cost of changing bytes exiv2 wrote for the one schema affected, and no consumer needs it. Pinned
  against the oracle by `tests/oracle.rs::an_xmpcore_written_dwc_graph_keeps_the_slashed_uri_it_declared`.
  **Deferred:** an opt-in canonicalization knob (a `ConvertPolicy`-style setting on the reader,
  never a silent rewrite) is the shape a consumer that needs `uri()` lookup would add; it is not
  planned until one asks.
- **Sidecars require `x:xmpmeta` (issue #421).** `XmpSidecar::read` accepts everything
  `XmpMeta::from_packet` does — XML declaration, BOM, wrapper or bare — but rejects a document
  whose element is not `x:xmpmeta` with `XmpError::MissingXmpMeta` naming the element found. **This
  is stricter than the specification, not derived from it.** The vendored Part 1 §7.3.3 reads "An
  optional x:xmpmeta element **may** be placed around the rdf:RDF element" and "An XMP processor
  **should tolerate** an x:xmpmeta element in any input" — permission and tolerance, never a
  requirement; the vendored Part 3's "External storage of metadata" bullets (a complete, well-formed
  XML document with the leading XML declaration, the `.xmp` extension, `application/rdf+xml`,
  written "as though it were embedded and then had the XMP packets extracted and catenated by a
  postprocessor") never mention the element, and the string `xmpmeta` occurs in Part 3 exactly
  once, inside an SVG example. So `read` rejects a sidecar the specification permits. What §7.3.3
  supplies is the element's *purpose* — identifying XMP inside general XML text — and a standalone
  `.xmp` file is that text, so gamut takes the element as the marker rather than reading any RDF
  document beside an image as XMP; exiv2 draws its own, different line (`isXmpType` keys on
  `<?xpacket` **or** `<x:xmpmeta`). `XmpSidecar::write` emits the XML declaration Part 3 asks for,
  then a read-only (`end="r"`), unpadded packet wrapping the canonical body in `x:xmpmeta` —
  byte-stable per graph. No filesystem API and no enforced file name: the `.xmp`-beside-the-image
  convention is documented for the caller. The missing wrapper has its own error,
  `XmpError::MissingXmpMeta`, naming the element found: the wrapper-less form is *permitted* by
  §7.3.3 and is what `XmpWriter::wrap_xmpmeta(false)` emits, so reporting it as a prohibited
  construct would assert a prohibition the spec does not make. exiv2 is more permissive still — its
  sniffer (`isXmpType`) accepts a `.xmp` file starting with `<?xpacket` **or** `<x:xmpmeta` — so a
  file exiv2 reads as a sidecar and gamut rejects is expected; the caller reads those bytes with
  `XmpMeta::from_packet`.
- **A catenated packet is refused, not truncated (issue #562 — option 1 of 3, "reject").** Part 3's
  bullet asks that external metadata be written "as though it were embedded and then had the XMP
  packets extracted and catenated by a postprocessor", so a conforming producer may emit a `.xmp`
  file holding several `<?xpacket?>` packets end to end. `XmpPacket::scan` refuses any input in
  which another `<?xpacket` instruction follows the first packet's trailer, with
  `XmpError::MultiplePackets` carrying the byte offset (into the input as passed, a leading BOM
  included) at which that instruction begins. **Why reject:** *merging* every packet (option 2)
  needs a rule for a property two packets both set, and no part of the specification supplies one —
  first-wins, last-wins and error are all inventions; *keeping the truncation* (option 3, the
  behaviour before #562) read the first packet and silently dropped the rest, so a caller merging a
  sidecar into an image could lose properties with no signal. Rejecting cannot lose data, matches
  the reference engine — Adobe XMPCore rejects two `XmpSidecar::write` outputs concatenated
  (`tests/oracle.rs::xmpcore_rejects_the_catenated_sidecar_gamut_rejects`) — and the offset leaves
  the merge to the caller who knows its rule: `bytes[..offset]` reads as the first packet and
  `bytes[offset..]` as the rest. **`XmpMeta::from_packet` gets the same rejection**, because it is
  `scan` + `parse`: a container (JPEG `APP1`, PNG `iTXt`, WebP `XMP `, an AVIF/HEIF `mime` item)
  hands over one packet, so a catenation there is malformed input, and refusing it in `scan` keeps
  one rule for both entry points. **What this stops accepting**, named because it narrows a
  reader: any input holding a second `<?xpacket` instruction after the first trailer — a catenated
  sidecar, or an embedded payload with a stray packet appended — which previously parsed as its
  first packet. Bytes after the trailer that are not an `<?xpacket` instruction (whitespace, NUL
  padding, an XML declaration) are still ignored. `XmpSidecar::write` emits exactly one packet, so
  gamut never produces the refused shape. `XmpError` is `#[non_exhaustive]`, so the new variant is
  additive; the behaviour change is filed as a fix, since the input it now refuses was mis-read.

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
| P8 | Part 2; Part 3 "External storage" | **Breadth** (issue #421) — registry gains the twelve exiv2-documented schemas the issue selected (18 → 30 entries; exiv2's `kipi`, `mediapro`, `expressionmedia`, `MP`, `MPRI`, `MPReg` stay unregistered), `.xmp` sidecar read/write, per-schema and sidecar oracle tests | ✅ done |

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
  namespace registry, not a validator — still true after the issue #421 additions.
- **Sidecar file naming and I/O (Part 3):** `XmpSidecar` is bytes-in / bytes-out; locating
  `photo.xmp` beside `photo.dng` is the caller's, as embedding is the format crates'.
- **Decode limits (crate-wide, not sidecar-specific):** `XmpSidecar::read` takes an unbounded
  `&[u8]` and parses the document before checking for the wrapper. `XmpMeta::from_packet` has
  identical exposure on the same bytes, so the sidecar API (issue #421) adds no hostile-input class
  the crate did not already have; whether gamut-xmp should grow a decode-limit convention like
  `gamut-png`'s is a question for the crate as a whole and is deliberately left open.
- **Deferred additive API** (post-1.0, no consumer today): an opt-in `XmpMeta::validate()`, and
  nested-structure field lookup.
