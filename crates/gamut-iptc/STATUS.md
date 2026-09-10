# gamut-iptc — IPTC photo metadata implementation status

Part of the **image metadata primitives** campaign (GitHub issue #34); stabilized to **v1** under
issue #182. Implements IPTC photo metadata (`references/iptc`) in both forms — legacy IIM and IPTC
Core over XMP — building the XMP path on [`gamut-xmp`](../gamut-xmp). Delivered as a stack of
small, individually-reviewable PRs; each PR is independently green
(`mise run test`/`lint`/`fmt-check`/`coverage` ≥ 80%).

**Keystone:** **IIM ↔ XMP reconciliation** — an image may carry the same datum in legacy IIM, in
IPTC-Core XMP, or in both with conflicting values; merging the carriers coherently and writing both
consistently is the genuinely hard part. The IPTC guidelines call for keeping the carriers in sync
but prescribe no single winner, so the precedence knob (`ConflictPolicy`, XMP-wins default matching
exiv2/ExifTool de-facto behaviour) is this crate's own design, applied over the spec's IIM↔XMP
property mapping.

**Oracle:** differential vs **exiv2** (`tooling/gamut-iptc-oracle`, a vendored static build). exiv2's
XMP toolkit is disabled in the oracle build (no Expat), so it cross-checks the legacy IIM dataset
stream and the Photoshop IRB; the IPTC-in-XMP property leg is left to the gamut-xmp oracle. The
schema/tag tables are additionally pinned to the IPTC machine-readable tech reference by
`tests/techreference.rs`.

## Phases

| Phase | Spec | Scope | Status |
| ----- | ---- | ----- | ------ |
| P1 | — | Scaffold: crate, workspace wiring, docs, region-free data-model skeleton | ✅ |
| P2 | Photoshop IRB | Parse + serialize the `8BIM` resource stream; locate the `0x0404` IIM resource | ✅ |
| P3 | IIM 4.2 | IIM dataset stream codec (standard + extended length), 1:90 charset (Latin-1/UTF-8), known-tag table | ✅ |
| P4 | IPTC PMD | IPTC Core over XMP — typed accessors over the `gamut-xmp` property graph (issue #34) | ✅ |
| P5 | IPTC mapping | **Keystone** — IIM ↔ XMP reconciliation (precedence policy + date split/join) | ✅ |
| P6 | — | IIM/IRB writer round-trip + exiv2 differential gate (`tooling/gamut-iptc-oracle`) | ✅ |
| v1 | issue #182 | API finalization (two entry points, published field map, complete Core accessors), strict-write/honest-read error contract, tech-reference drift guard, divan benches, docs | ✅ |
| P7 | issue #422 | Breadth: the complete IIM 4.2 record-1/record-2 tag table, and typed models for the four most-used structured properties (`extension`) | ✅ |

## Breadth (issue #422)

- **Structured properties.** `extension` models `Iptc4xmpCore:CreatorContactInfo`,
  `Iptc4xmpExt:ImageRegion` (with `RegionBoundary`/`RegionBoundaryPoint`/`Entity`),
  `Iptc4xmpExt:ArtworkOrObject` and `plus:Licensor` as typed projections over the XMP graph, in the
  `from_xmp`/`to_xmp` shape `gamut_exif::GpsInfo` uses for its sub-IFD. Every one is XMP-only — none
  carries an `IIMid` — so none extends the reconciliation surface; `tests/techreference.rs` pins
  that, and each structure's field set, to the reference. Reading a structure and writing it back
  changes nothing: a field is taken into the typed value only when the property the writer will
  emit for it *reproduces* the field that was read — value, RDF container kind and qualifiers —
  and every other field is kept in the type's `other` list and re-emitted verbatim. That one rule
  covers a field the model does not name, one it names but cannot read, and one it can read but
  could not write back as it stands (an `rdf:resource` where the model writes element text, a
  qualifier, a language beside the default, an unexpected container kind, a coordinate with no XMP
  `Real` value). Only two differences remain, both idempotent: a structure's fields come back in
  the model's order, and a number or an `x-default` tag may be re-spelled.
- **IIM tag table.** `iim::IimTagInfo` now names every dataset IPTC-IIM 4.2 states an octet maximum
  for that `max_octets` can hold: 14 Envelope + 56 Application datasets (chapters 5 and 6 bar
  `2:202`), plus `7:10` Size Mode, the one dataset outside those chapters whose length the spec
  fixes ("one octet"). The table is descriptive — no `FIELD_MAP` row references a dataset outside
  the PMD-mapped subset — so reading, merging and writing are byte-for-byte unchanged by it.
- **Authority.** The PMD tech reference maps only the ~20 IIM-mapped rows and the `ipmd_struct`
  field sets, both of which `tests/techreference.rs` re-derives at test time. The rest of the
  record-1/2 table comes from `iim-4.2.pdf`, and both of its guards read a source outside this
  crate:
  - **names** — the standard sets every DataSet's name in a column of its own, which
    `pdftotext -bbox-layout` recovers by position. `tests/data/extract-iim-names.py` does that and
    writes `tests/data/iim-4.2-dataset-names.tsv`; `iim`'s own
    `tag_table_names_match_the_standards_own_dataset_names` compares every row against it, and
    against the six datasets the standard names that gamut deliberately does not. The extraction is
    not run by the gate — `pdftotext` is a system package the toolchain does not provision — so its
    output is committed as a derived artefact with the command that regenerates it recorded beside
    it. A name mistyped in `KNOWN_TAGS` alone fails there. A name mistyped *identically* in
    `KNOWN_TAGS` and in the committed `.tsv` does not: the guard compares the table against the
    artefact, and nothing re-derives the artefact from the PDF. What carries that residual is the
    artefact's own never-hand-edit banner, not a gate — having CI re-derive it where `pdftotext`
    is present is issue #623.
  - **octet maximum, repeatability and value kind** — stated in the standard's prose, so
    `tag_table_matches_the_exiv2_dataset_table` compares them against exiv2's independent
    transcription of the same chapters, parsed out of the vendored `third_party/exiv2` sources. A
    slipped digit fails there, which no round trip can see.

  The structural laws (ordering, uniqueness, the fixed date/time form lengths) and the exiv2 wire
  differential in `tests/oracle.rs` sit alongside them.

## Deferred / out of scope

Intentional, documented skips — none lose data on round-trip:

- **A structured field the projection cannot express reads as absent** — a URL held as
  `rdf:resource`, a value carrying a qualifier, a language alternative with entries beside the
  default, an unexpected container kind. Nothing is lost: the field is in the type's `other` list
  and the graph keeps it verbatim. Whether the model should widen to report the value as well is
  issue #609.
- **A whole structured property the projection cannot express reads as absent, on the same terms.**
  `creator_contact_info`, `image_regions`, `artwork_or_objects` and `licensors` report a value only
  when writing it back would give the property back — so a bare structure written where the standard
  puts an array, an array member that is not a structure, a qualifier on the property or on an
  `rdf:li`, an array or structure holding nothing, and two top-level properties of one name all read
  as nothing. Nothing is lost: the graph keeps the property untouched, and the setter beside the
  accessor does not remove what the accessor did not report. Reading a property and setting it back
  is the identity, pinned by a generated cross of every pair, value shape and qualifier list.
- **The remaining eleven IPTC Extension structures** (`Location`, `PersonWDetails`, `CvTerm`,
  `EntityWRole`, `ProductWGtin`, `RegistryEntry`, `EmbdEncRightsExpr`, `LinkedEncRightsExpr`,
  `CopyrightOwner`, `ImageCreator`, `ImageSupplier`): no typed model — issue #538. They pass through
  `PhotoMetadata::xmp` as raw `gamut-xmp` values untouched.
- **IIM datasets with no octet maximum `max_octets` can state** (`2:202`, and records 7–9 apart from
  `7:10`): not in the tag table, because `IimTagInfo::max_octets` is a `u16` and can only state a
  determinate maximum — issue #539, whose remainder is six datasets, `7:10` having since been named.
  They still round-trip byte-exact, as every unmodeled dataset in any record does.
- **Exotic ISO 2022 character sets**: dataset 1:90 designations other than the spec default
  (decoded as Latin-1, the exiv2/ExifTool de-facto reading of ISO 646 IRV) and UTF-8 (`ESC % G`)
  are reported as `Error::Unsupported`, never mis-decoded.
- **Scalar-shaped repeatable IIM datasets** (2:04 Object Attribute Reference, 2:85 By-line Title):
  repeatable on the wire but mapped to single XMP properties — reconciliation takes the first
  value; the wire side still round-trips every repeat.
- **XMP packet bytes**: parsing/serializing the packet, and the JPEG `APP13`/TIFF tag plumbing,
  belong to `gamut-xmp` and the containers respectively (issue #34). This crate is the semantics
  layer over the in-memory property graph.
- **Length limits are write-side only** (strict-write/honest-read contract): `IptcWriter` rejects
  overlong or unencodable values and IIM-inexpressible `DateCreated`s; the parser accepts and
  preserves overlong wire values rather than reject real-world files.
- **IIM records 3–6**: no named tag-table entries, because IIM 4.2 defines no datasets for them —
  record 3 (Digital Newsphoto Parameter) is a separate publication, records 4 and 5 are not
  allocated, and record 6 (Abstract Relationship) has only the method identifiers of Appendix F.
  Datasets in those records round-trip byte-exact, unnamed.

## Reference discrepancies

- **2:04 maximum length — 68 vs 64.** The IIM 4.2 PDF defines the wire form as a 3-digit reference
  number, a colon, and up to 64 octets of text (= 68 octets); the PMD tech-reference JSON's
  `IIMmaxbytes` records 64 (the text part only). The crate follows the wire form (68);
  `tests/techreference.rs` pins **both** values so the exception self-invalidates if either source
  changes.
