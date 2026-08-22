# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [1.0.1](https://github.com/visualcommons/gamut/compare/gamut-icc-v1.0.0...gamut-icc-v1.0.1) - 2026-08-22

### Other

- adopt as_chunks for constant-size slice chunking

## [1.0.0](https://github.com/justin13888/gamut/compare/gamut-icc-v0.1.1...gamut-icc-v1.0.0) - 2026-07-18

### Added

- *(icc)* [**breaking**] expose a dedicated IccError type
- *(icc)* add ProfileHeader::new with spec-valid defaults
- *(icc)* derive Eq across the tag-data model
- *(icc)* extend the lcms2 oracle for the new element types
- *(icc)* add profile-class conformance validation (§8 required tags)
- *(icc)* decode header device-attribute and profile-flag bit fields
- *(icc)* decode dictType metadata dictionary
- *(icc)* decode profileSequenceDesc, profileSequenceIdentifier, and responseCurveSet16
- *(icc)* decode colorant order/table and the integer/fixed array elements
- *(icc)* decode chromaticity, cicp, measurement, viewingConditions, and data elements
- *(icc)* serialize profiles with a two-pass writer + round-trip gate
- *(icc)* compute the MD5 profile ID (ICC.1:2022 §7.2.18)
- *(icc)* decode the namedColor2 element type (ICC.1:2022 §10.17)
- *(icc)* decode the LUT transform element types (ICC.1:2022 §10.10-10.13)
- *(icc)* expand the KnownTag registry and verify full matrix/TRC decode
- *(icc)* decode multiLocalizedUnicode and v2 textDescription
- *(icc)* decode the simple element types (ICC.1:2022 §10)
- *(icc)* parse the tag table and dispatch elements (ICC.1:2022 §7.3)
- *(icc)* parse the full 128-byte profile header (ICC.1:2022 §7.2)
- *(icc)* add ICC numeric primitives (fixed-point, XYZ, date-time)

### Other

- *(icc)* release v1.0.0
- *(icc)* close the v1 full-survey mutation gaps
- *(icc)* state the v1 API policies and refresh the status docs
- *(icc)* correct spec section citations found by the v1 conformance audit
- *(icc)* [**breaking**] rename DescriptionText to EmbeddedDescription and align error wording
- *(icc)* [**breaking**] make serialization fallible and validate model invariants on write
- *(icc)* [**breaking**] move profile-ID computation onto ProfileId and add Display impls
- *(icc)* [**breaking**] adopt std conversion traits for signatures and header enums
- *(icc)* [**breaking**] make tag lookup accept anything convertible to a Signature
- *(icc)* [**breaking**] hide modules behind root re-exports
- *(icc)* record full §10 coverage and conformance validation
- *(icc)* link the ICC spec index and document iccMAX (ICC.2) as out of scope
- *(icc)* pin the parametric type-3/4 `a*x+b` term (last mutant)
- *(icc)* close mutation-testing gaps across the element types
- *(icc)* cover the encoders and façades (round-trip + edge cases)
- *(icc)* document the implemented formulas, usage, and stabilized status

## [0.1.1](https://github.com/justin13888/gamut/compare/gamut-icc-v0.1.0...gamut-icc-v0.1.1) - 2026-06-12

### Other

- updated the following local packages: gamut-core
