# gamut-iptc

`gamut-iptc` is a pure-Rust **IPTC photo metadata** parser and serializer, covering both the legacy
IIM form and the modern IPTC Photo Metadata carried over XMP — and the reconciliation between them.

## Goals

Part of the [gamut](../../README.md) workspace, this crate reads, preserves, and embeds IPTC photo
metadata. It is:

- **Memory-safe on hostile input.** `#![forbid(unsafe_code)]`; every parse is bounds-checked and
  length-driven.
- **Clean-slate from the spec.** Implemented from **IPTC-IIM 4.2** and the **IPTC Photo Metadata
  Standard 2025.1** ([`../../references/iptc`](../../references/iptc)), with the hand-transcribed
  tables pinned to the IPTC machine-readable tech reference at test time (see
  [Validation](#validation)).
- **Two carriers, one model.** Legacy IIM is a binary dataset stream inside a Photoshop Image
  Resource Block (`8BIM`, resource `0x0404`); the modern Core fields *are* XMP, so that path builds
  on [`gamut-xmp`](../gamut-xmp) (a **public dependency**, re-exported as `gamut_iptc::xmp`). The
  hard part is reconciling the two when both are present — `IptcReader::read` merges with an
  explicit `ConflictPolicy`, and `IptcWriter` projects the unified view back to the legacy carrier.

## Why this crate

There is no maintained pure-Rust library that covers both IPTC carriers *plus* the IIM↔XMP
reconciliation — the reference implementations are C++ ([exiv2](https://exiv2.org)) and Perl
(ExifTool). gamut needs the reconciliation semantics behind its
[`gamut-metadata`](../gamut-metadata) facade (issue #34), and image formats need the Photoshop IRB
codec to embed IPTC in JPEG `APP13`/TIFF tag 34377. exiv2 serves as the differential oracle for the
binary carrier — never as a dependency.

## Usage

The legacy IIM carrier is implemented end to end — parse and serialize the Photoshop `8BIM`
resource stream (`PhotoshopIrb`), the IIM dataset stream (`IimBlock` / `IimDataSet`), and decode
text with the coded character set (`IimCharset`):

```rust
use gamut_iptc::{IimBlock, IimCharset, IimDataSet, IptcReader, PhotoshopIrb};

let block = IimBlock {
    datasets: vec![
        IimDataSet { record: 2, dataset: 0, data: vec![0, 4] },        // Record Version = 4
        IimDataSet { record: 2, dataset: 25, data: b"sky".to_vec() },  // Keywords
    ],
};
let irb = PhotoshopIrb::with_iptc(block.encode()?).encode()?;          // 8BIM 0x0404 resource
let read = IptcReader::new().read_irb(&irb)?.expect("0x0404 present");
let charset = IimCharset::detect(&read)?;                              // 1:90, default Latin-1
assert_eq!(charset.decode(&read.datasets[1].data)?, "sky");
# Ok::<(), gamut_core::Error>(())
```

`IimTagInfo::lookup(record, dataset)` resolves a dataset's name and wire constraints;
`schema::FIELD_MAP` is the spec-pinned IIM↔XMP mapping, usable generically through
`PhotoMetadata::get_field`/`set_field`.

The modern IPTC Photo Metadata path models Core fields as XMP properties (`PhotoMetadata`, with
typed accessors for creator, location, rights, keywords, dates, …). `IptcReader::read` merges the
two carriers into one view, and `IptcWriter` projects a view back to the legacy carrier:

```rust
use gamut_iptc::{IimBlock, IimDataSet, IptcReader, IptcWriter, ConflictPolicy};

let iim = IimBlock { datasets: vec![
    IimDataSet { record: 2, dataset: 90, data: b"Lyon".to_vec() }, // City
] };
// When IIM and XMP disagree, the policy decides; XMP wins by default.
let merged = IptcReader::new().policy(ConflictPolicy::IimWins).read(Some(&iim), None)?;
assert_eq!(merged.city(), Some("Lyon"));

// ...and back: PhotoMetadata -> 8BIM resource stream (None if there is nothing to embed).
let irb = IptcWriter::new().write_irb(&merged)?.expect("City is present");
# Ok::<(), gamut_core::Error>(())
```

## Scope

The v1 contract, stated precisely:

- **Semantics layer only.** `gamut-iptc` operates on the in-memory XMP property graph
  (`PhotoMetadata` over `gamut-xmp` types); parsing/serializing the XMP *packet bytes* is
  [`gamut-xmp`](../gamut-xmp)'s job, and the JPEG `APP13`/TIFF tag plumbing is the container's
  (issue #34).
- **Typed accessors cover every scalar/list IPTC Core property**, plus the structured
  `Iptc4xmpCore:CreatorContactInfo` and the most-used IPTC **Extension** structures — image
  regions, artwork/object and licensors (`extension`). The remaining Extension structures
  (locations, persons, controlled-vocabulary terms, …) have no typed model — they still round-trip
  losslessly as raw properties in `PhotoMetadata::xmp`, reachable via `get_field`/`set_field` where
  mapped.
- **Strict write, honest read.** Writing never silently truncates or drops: unencodable text,
  overlong values (octet limits are enforced on write only; overlong wire values are preserved on
  read), and an IIM-inexpressible `photoshop:DateCreated` are hard errors. Reading never guesses: a
  `1:90` coded character set other than the default (Latin-1) or UTF-8 is a typed
  `Error::Unsupported`, never mis-decoded; within a supported charset, an individually undecodable
  value is treated as absent.
- **Reconciliation is per-field with a global policy.** The IPTC guidelines call for keeping the
  carriers in sync but prescribe no single winner; `ConflictPolicy` (XMP wins by default, matching
  exiv2/ExifTool de-facto behaviour) is this crate's explicit knob. Scalar-shaped IIM datasets that
  repeat on the wire (2:04 Object Attribute Reference, 2:85 By-line Title) reconcile their first
  value; all repeats still round-trip on the IIM side.
- **IIM records 1–2 are tabled; everything else is preserved.** The tag table names every dataset
  IIM 4.2 chapters 5 and 6 give a determinate octet maximum; any other dataset in any record
  round-trips byte-exact without a name.

## Status

Production-ready v1 (issue #182): both carriers, the reconciliation keystone, typed Core accessors,
and the spec-pinned mapping tables, gated by the exiv2 differential oracle. See
[STATUS.md](STATUS.md), including the documented deferrals.

## Validation

- **Drift guard** — `tests/techreference.rs` re-derives the IIM↔XMP mapping, octet limits, and
  value shapes from the vendored IPTC machine-readable tech reference
  (`references/iptc/iptc-pmd-techreference_2025.1.json`) and compares them against the crate's
  tables, so transcription slips or a future IPTC edition fail loudly. The one place the vendored
  references disagree (2:04 maximum length: 68 on the IIM 4.2 wire vs 64 text-only octets in the
  JSON) is pinned from both sides.
- **Differential oracle** — the dev-only
  [`tooling/gamut-iptc-oracle`](../../tooling/gamut-iptc-oracle) (a vendored, statically-linked
  exiv2) cross-checks the binary IIM/IRB carrier dataset-for-dataset; exiv2's XMP toolkit is
  disabled in that build, so the IPTC-in-XMP leg is covered by the gamut-xmp oracle instead.
- **Benchmarks** — `cargo bench -p gamut-iptc` (divan) reports codec and reconciliation throughput.

## License

Licensed under either of MIT or Apache-2.0 at your option.
