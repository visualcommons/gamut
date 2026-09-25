# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [1.1.0](https://github.com/visualcommons/gamut/compare/gamut-iptc-v1.0.0...gamut-iptc-v1.1.0) - 2026-09-25

### Added

- *(iptc)* read a bare structure where a sequence is expected
- *(iptc)* name the one-octet 7:10 Size Mode dataset
- *(iptc)* model the four most-used IPTC structured properties
- *(iptc)* name every length-determinate IIM 4.2 dataset

### Fixed

- *(iptc)* replace every top-level copy of a property a typed setter writes
- *(iptc)* consume a coordinate only when its decimal value is re-emitted unchanged
- *(iptc)* keep a top-level property the setter cannot reproduce
- *(iptc)* keep every array member and the container kind on the way out
- *(iptc)* decide retention by what the writer can reproduce
- *(iptc)* read a language alternative by its x-default tag
- *(iptc)* retain an extension field the typed read cannot consume
- *(iptc)* drop an array member that carries no field at all
- *(iptc)* never write a value the declared type has no room for
- *(iptc)* keep the fields the extension structures do not model

### Other

- *(iptc)* wrap the round's doc comments to the line width
- *(iptc)* format the projection read
- *(iptc)* state the dataset-name guard's identical-typo residual in its comment
- *(iptc)* say that a setter handed values replaces the property outright
- *(iptc)* say what the name guard does not catch
- *(iptc)* cross every accessor pair with every shape a graph can carry
- *(iptc)* state each clause of the reproduction relation on its own
- *(iptc)* name the two shapes the fidelity list left out
- *(iptc)* list the reads the projection gives up as a deferral
- *(iptc)* record what the reproduction check costs
- *(iptc)* state the reproduction relation on its own
- *(iptc)* state the fidelity rule the code now follows
- *(iptc)* pin the dataset names to the standard's own name column
- *(iptc)* state what the guard compares and what retention covers
- *(iptc)* compare dataset names against exiv2's table too
- *(iptc)* pin the IIM tag table to exiv2's own dataset table
- *(iptc)* read an Entity back from a structure value
- *(iptc)* record the delivered breadth and the two remaining deferrals
- *(iptc)* derive the structured-property field sets from the tech reference
- *(iptc)* name the differential file for the convention its siblings use

## [1.0.0](https://github.com/justin13888/gamut/compare/gamut-iptc-v0.1.1...gamut-iptc-v1.0.0) - 2026-07-18

### Added

- *(iptc)* [**breaking**] expose a dedicated IptcError type
- *(iptc)* complete the typed accessors for IPTC Core
- *(iptc)* publish the IIM-XMP field map and field-level access
- *(iptc)* IPTC Core over XMP and IIM↔XMP reconciliation
- *(iptc)* legacy IIM 4.2 and Photoshop IRB binary codec

### Fixed

- *(iptc)* complete the XmpMeta migration in PhotoMetadata
- *(iptc)* adapt photo_metadata to the XmpItem array model

### Other

- Merge branch 'master' into feat/182-iptc-v1
- *(iptc)* release v1.0.0
- *(iptc)* document the v1 surface and deferrals
- *(iptc)* add divan benches for the IIM/IRB codec and reconciler
- *(iptc)* pin the IIM and XMP tables to the PMD tech reference
- *(iptc)* [**breaking**] adopt the strict-write, honest-read error contract
- *(iptc)* [**breaking**] fold IimXmpReconciler into IptcReader and IptcWriter
- *(iptc)* [**breaking**] finalize the IIM primitive surface
- apply nightly rustfmt import grouping across the workspace
- *(mise)* port justfile recipes to mise tasks
- *(iptc)* record the exiv2 oracle in STATUS and references
- *(iptc)* exiv2 differential oracle for the IIM/IRB carrier
- *(iptc)* document the IPTC-over-XMP path and reconciliation
- *(iptc)* document the implemented legacy IIM/IRB carrier

## [0.1.1](https://github.com/justin13888/gamut/compare/gamut-iptc-v0.1.0...gamut-iptc-v0.1.1) - 2026-06-12

### Other

- updated the following local packages: gamut-core, gamut-xmp
