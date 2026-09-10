# gamut-metadata

`gamut-metadata` is the **unified image-metadata facade** for the gamut workspace. It brings the
per-format crates — [`gamut-exif`](../gamut-exif), [`gamut-xmp`](../gamut-xmp),
[`gamut-icc`](../gamut-icc), and [`gamut-iptc`](../gamut-iptc) — under one `Metadata` model and one
extract/embed surface.

## Design

- **One carrier, one field.** `Metadata` has exactly one field per genuinely distinct serialization
  a container holds: `exif`, `xmp`, `icc`, `c2pa`. **IPTC has no field of its own** — IPTC Photo Metadata
  *is* XMP (properties in the `dc:`/`photoshop:`/`Iptc4xmp*` namespaces), so it lives inside `xmp`,
  read back through the `Metadata::iptc()` lens. Storing it separately would duplicate the same
  data. The one genuinely separate IPTC carrier, the legacy binary IIM block, is reconciled *into*
  the XMP graph on extract and projected back out only on request when embedding.
- **Container-agnostic.** It consumes already-located `MetadataBlock` byte payloads (from the WebP
  `EXIF`/`XMP `/`ICCP` chunks or the AVIF/HEIF `Exif`/`mime`/`colr` items) and produces owned
  `EncodedMetadata` blocks — it never parses boxes or chunks itself, keeping the
  `format → metadata` dependency thin.
- **Orchestration-only.** It holds the leaf crates' types by value and delegates all parsing and
  serialization to them; the leaf crates own correctness (and their own oracle test suites).
- **Cross-format reconciliation.** The two IPTC carriers — legacy IIM and IPTC-Core-in-XMP — are
  merged into the single XMP graph via `gamut-iptc`, resolving disagreements with a configurable
  `ConflictPolicy` (`conflicts()` reports them without resolving). Because each datum is stored once,
  the extract → embed → extract round-trip is a **true equality** — with one documented exception,
  `c2pa`, below.
- **Extensions are not a carrier.** `Metadata::extensions` is a namespaced table for data no
  carrier can express, so a downstream typed model round-trips through `Metadata` without being
  narrowed to the carrier fields. It is explicitly outside the carrier model: extraction never
  produces an extension and embedding never emits one.
- **The C2PA manifest store is a carrier, and never copied forward.** `Metadata::c2pa` holds the
  JUMBF superbox verbatim; extraction produces it, and embedding always drops it (or fails, under
  `C2paPolicy::Reject`). See below.

## Extensions: data with no carrier

A downstream typed model is usually wider than what a still-image file can carry — sensor geometry,
container-level facts, structs it derives itself. `Metadata::extensions` holds that residue as
`MetadataExtension { namespace, key, value }`, where `namespace` is a reverse-DNS string or URI the
caller owns (the `gamut.` prefix is reserved) and `value` is the same TIFF/IFD `Value` model gamut's
metadata crates already use.

Two guarantees, deliberately distinct:

| | What survives | |
| --- | --- | --- |
| **Model round-trip** | carriers **and** extensions | `their model → Metadata → their model` |
| **Carrier round-trip** (keystone) | `exif` / `xmp` / `icc` only — **not** `c2pa` | extract → embed → extract is a true equality over those three |

**Prefer a carrier whenever one exists** — only a carrier reaches the file. An unmodelled EXIF tag,
MakerNote included, already round-trips inside `exif` because `Exif` retains the raw `gamut_ifd::Ifd`;
any property round-trips inside `xmp` because the XMP graph is open; an unmodelled ICC element
round-trips inside `icc` as `TagData::Raw`. Reach for an extension only when no carrier can hold the
datum at all.

```rust
use gamut_metadata::{ExtensionPolicy, Metadata, MetadataEmbedder};
use gamut_metadata::exif::Value;

let mut meta = Metadata::default();
meta.set_extension("com.example.raw", "WhiteLevel", Value::Long(vec![16_383]));

// Embedding cannot carry it: dropped by default, or refused when losing it must be an error.
let blocks = meta.encode()?;                       // no block corresponds to the extension
let strict = MetadataEmbedder::new()
    .extension_policy(ExtensionPolicy::Reject)
    .embed(&meta);                                  // Err(MetadataError::UnembeddableExtension { .. })
```

Not yet supported, and deferred deliberately: a `MetadataBlock` variant for container-located blocks
the facade does not model, which would let *extraction* produce extensions. Today extensions are set
by the caller only.

## C2PA: a carrier that must not be copied forward

`Metadata::c2pa` holds a C2PA manifest store exactly as a container found it — the JUMBF superbox of
the C2PA Technical Specification 2.4 §11.1.4.2 — as opaque bytes. The facade never looks inside it.

**Why a carrier and not an extension.** Extensions exist for data no file holds: extraction never
produces one, and nothing serializes them. A manifest store is the opposite on the first count — it
comes *out* of a file, and a valid one belongs *in* a file — so it is a genuinely distinct
serialization a container holds, which is precisely what the "one carrier, one field" rule admits.

**The keystone carve-out.** The extract → embed → extract equality explicitly excludes `c2pa`.
A standard manifest binds to its asset with exactly one **hard binding** (§9.1): a digest over the
finished file, computed with the manifest store's own byte range excluded (§15.12.1.1) and covering
the asset's other metadata (§9.2.6). Re-encoding the image — or any metadata-only rewrite that moves
a byte — invalidates that digest, so a store copied into the new file would be a signature over a
file that no longer exists. C2PA's model for a derivative asset is a *new* manifest carrying the
parent as an ingredient, not the parent's signature laundered onto different bytes; producing one is
signing work, outside this crate. Hence the asymmetry, which is deliberate and not a bug:
**extraction produces `c2pa`, and embedding never emits it.**

```rust
use gamut_metadata::{C2paPolicy, Metadata, MetadataBlock, MetadataEmbedder};

// A container located a manifest store; extraction carries the bytes through untouched.
let meta = Metadata::from_blocks(&[MetadataBlock::C2pa(manifest_store)])?;
assert_eq!(meta.c2pa.as_deref(), Some(manifest_store));

let blocks = meta.encode()?;
assert_eq!(blocks.c2pa, None);                      // dropped by default...
let strict = MetadataEmbedder::new()
    .c2pa_policy(C2paPolicy::Reject)
    .embed(&meta);                                  // Err(MetadataError::UnembeddableC2pa { .. })
```

There is deliberately **no `Preserve` policy** — copy-forward is the failure mode `C2paPolicy` exists
to make impossible — and deliberately **no byte range** beside `Metadata::c2pa`: an offset is a
property of one file and becomes a lie the moment the model is embedded into another, so ranges stay
with the format crate that knows the file.

Deferred deliberately, and tracked by the C2PA epic rather than here: parsing the JUMBF interior,
and any manifest validation, signing, or ingredient authoring — all of which need a trust model this
facade does not have.

## Provenance: embedded, remote, both, or none

An embedded store is not the only way a file carries provenance. C2PA 2.4 §11.5 recommends that a
claim generator whose manifest lives *externally* add a `dcterms:provenance` key (namespace
`http://purl.org/dc/terms/`, registered as `gamut_xmp::WellKnownNs::DcTerms`) to the asset's XMP,
its value the URL of the manifest store, and is explicit that the mechanism is *only* for external
manifests; §15.5.3.1 lists that key among the places a validator looks when no store is embedded. So
`c2pa.is_some()` is the wrong question — a file with no embedded store and a `dcterms:provenance` URL
has Content Credentials — and a boolean is the wrong answer, because a file may carry both.

`Metadata::provenance()` is the lens, a `ProvenanceState` computed from the two independent sources
and stored nowhere:

| `c2pa` | `dcterms:provenance` | `provenance()` |
| --- | --- | --- |
| `None` | absent | `ProvenanceState::None` |
| `None` | URL | `ProvenanceState::Remote(url)` |
| `Some` | absent | `ProvenanceState::Embedded` |
| `Some` | URL | `ProvenanceState::EmbeddedAndRemote(url)` — both reported; a validator uses the embedded store and does not consult the URL (§15.5.2.1, §15.5.3.1) |

`is_embedded()` and `remote_url()` answer the two underlying questions without matching (the enum is
`#[non_exhaustive]`). An empty `dcterms:provenance` value counts as absent — the spec makes the value
a URI reference, which an empty string is not. The lens reports what the file carries; it is not a
validity verdict and does not choose between the two sources.

```rust
use gamut_metadata::{Metadata, MetadataBlock, ProvenanceState};

let meta = Metadata::from_blocks(&[MetadataBlock::Xmp(xmp_payload)])?;
match meta.provenance() {
    ProvenanceState::Remote(url) => println!("external manifest at {url}"), // not fetched
    ProvenanceState::Embedded => println!("manifest store embedded"),
    ProvenanceState::EmbeddedAndRemote(url) => println!("embedded; the XMP also names {url}"),
    _ => println!("no provenance in the file"),
}
```

Two things this deliberately does **not** do. **gamut never resolves the URL** — fetching it and
judging what it points at is a validator's job and a network operation, and the workspace ships
neither (see [`references/c2pa/README.md`](../../references/c2pa/README.md)). And the **HTTP `Link`
header route of §15.5.3.2** — the same pointer carried as a `Link` relation when the asset is served
over HTTP — is out of scope: a header is a property of a transfer, not of the file's bytes, so a
file-format library cannot observe it. A caller that fetched the asset holds the header and may
consult it before this lens.

## Usage

```rust
use gamut_metadata::{ConflictPolicy, Metadata, MetadataBlock, MetadataExtractor};

// A container crate has already located the metadata payloads in a file.
let meta = MetadataExtractor::new()
    .policy(ConflictPolicy::XmpWins)
    .extract(&[
        MetadataBlock::Exif(exif_payload),
        MetadataBlock::Xmp(xmp_payload),
        MetadataBlock::Icc(icc_payload),
        MetadataBlock::IptcIim(iptc_iim_payload), // legacy carrier, folded into `xmp`
        MetadataBlock::C2pa(manifest_store),       // carried verbatim; never embedded back
    ])?;

// Typed IPTC access is a lens over `meta.xmp` — it stores nothing.
if let Some(iptc) = meta.iptc() {
    println!("city: {:?}", iptc.city());
}

// Serialize back to per-carrier blocks for a container to embed.
let blocks = meta.encode()?;
// blocks.exif / blocks.xmp / blocks.icc are Some(Vec<u8>) when their field was present.
```

The umbrella [`gamut`](../gamut) crate re-exports this crate as `gamut::metadata` behind its
`metadata` feature.

## Migrating from 1.x

`Metadata`, `MetadataBlock`, and `EncodedMetadata` are `#[non_exhaustive]`, so a later carrier — the
C2PA manifest store was one — is an additive change rather than a breaking one. Struct literals
become a constructor call:

```rust
// 1.x
let meta = Metadata { exif, xmp, icc };
// 2.x
let meta = Metadata::from_carriers(exif, xmp, icc);
```

Use `Metadata::default()` plus field assignment when you also need `extensions`, and add a wildcard
arm to any exhaustive `match` on `MetadataBlock`. Nothing else changed: every existing method keeps
its signature and behaviour.

## Capability query

The facade never parses a container, so it cannot say whether a *file* carries metadata. What it
can say — statically, from a crate that depends on no format crate — is whether gamut's crate for a
format can **locate** or **write** a given carrier at all. `gamut_metadata::capability` is that
table, per (format × carrier × direction), transcribed from each crate's `STATUS.md` with the row
cited on every cell:

| Format | EXIF | XMP | ICC | IPTC-IIM | C2PA | typed wiring |
| --- | --- | --- | --- | --- | --- | --- |
| JPEG | r/w | r/w | r/w | — | — | ✅ (`metadata` feature) |
| PNG | r/w | r/w | r/w | — | — | — |
| WebP | r/w | r/w | r/w | — | — | — |
| AVIF | r/w | r/w | r/w | — | — | — |
| HEIC | r | r | r | — | r | ✅ (`metadata` feature) |
| JPEG XL | r/w | r/w | r/w | — | — | ✅ (`metadata` feature) |
| TIFF | — | — | — | — | — | — |
| DNG | r/w | r/w | r/w | r/w | — | ✅ |

```rust
use gamut_metadata::capability::{Carrier, Direction, Format, supports, typed_wiring};

assert!(supports(Format::Jpeg, Carrier::Exif, Direction::Write));
assert!(!supports(Format::Heic, Carrier::Exif, Direction::Write)); // decode-only crate
assert!(typed_wiring(Format::Jpeg));   // `blocks()` / `metadata()` / `with_metadata` exist there
assert!(!typed_wiring(Format::Png));   // raw bytes handed to `Metadata::from_blocks` by hand
```

`supports` describes the crate's **raw** surface — its byte-level `metadata()` / `with_*` API;
`typed_wiring` says whether it also exposes this crate's models directly. C2PA is read-only
everywhere by construction — no embedder copies a manifest store forward (see above).

Both describe the surface a crate **defines**, not the surface a particular build compiled: a
`const` table sees neither another crate's Cargo features nor the target. Two places where that
gap is real, and both are in the table's own docs. `gamut-jxl` gates its reader on its `decode`
feature and its encoder on `encode`, so a build with `default-features = false, features =
["encode"]` has no reader while the table still says `r`. And typed wiring reaches three of the
four wired crates through an optional `metadata` feature of their own (`gamut-jpeg`, `gamut-jxl`,
`gamut-heic`, each off by default and each forwarded weakly by the `gamut` umbrella) — but
**`gamut-dng` has no such feature**: it depends on this crate unconditionally, so `gamut-dng/metadata`
is not something a manifest can ask for.

The enums are `#[repr(u8)]` with append-only discriminants and carry `ALL` constants for
enumeration, since `Format` and `Carrier` are `#[non_exhaustive]`. The audio/video half of the same
question is outside an image-first workspace and stays with issue #216.

## Consumer integration

The dependency direction is `format → gamut-metadata → per-format crates`, settled here. A format
crate wires the facade in behind an optional `metadata` Cargo feature (a normal, not dev,
dependency — release ordering follows it): its decoded metadata type gains `blocks()` (the located
payloads as `MetadataBlock`s, e.g. a JPEG's EXIF with the `Exif\0\0` signature already stripped,
an ISOBMFF `Exif` item with its `exif_tiff_header_offset` already applied) and `metadata()`
(`Metadata::from_blocks` over them), and its encoder gains `with_metadata(&Metadata)` — embedding
through `MetadataEmbedder::new()` and routing each `EncodedMetadata` field to the crate's raw
setter — plus `with_encoded_metadata(&EncodedMetadata)` for a caller who chose the embedder's
policies. A carrier the container cannot write is a typed `Unsupported` error there, never a silent
drop, and a manifest store is never copied forward.

Wired today: `gamut-dng` (its `DngMetadata` holds the facade's `Exif` by value), `gamut-jpeg`,
`gamut-jxl` and `gamut-heic` (decode-only). The remaining format crates hand their payloads over as
raw bytes — see the capability table above.

## License

Licensed under either of MIT or Apache-2.0 at your option.
