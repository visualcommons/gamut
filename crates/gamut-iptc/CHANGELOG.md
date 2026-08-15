# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [1.0.1](https://github.com/visualcommons/gamut/compare/gamut-iptc-v1.0.0...gamut-iptc-v1.0.1) - 2026-08-15

### Other

- updated the following local packages: gamut-core, gamut-xmp

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
