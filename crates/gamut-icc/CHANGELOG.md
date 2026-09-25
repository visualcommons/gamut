# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [2.0.0](https://github.com/visualcommons/gamut/compare/gamut-icc-v1.0.0...gamut-icc-v2.0.0) - 2026-09-25

### Added

- *(icc)* build ICC profiles from named colour spaces and CICP

### Fixed

- *(icc)* build narrow-range CICP signalling whose range is on Y/Cb/Cr
- *(icc)* describe a grey profile by the gamma its kTRC holds
- *(icc)* [**breaking**] decline a CICP triple that does not signal full range
- *(icc)* bound the grey gamma by what the kTRC actually encodes
- *(icc)* [**breaking**] build CICP profiles from the transfer H.273 defines
- *(icc)* adapt built-in colorants to the PCS D50 the profile declares

### Other

- *(icc)* format the lcms2 cicp synthesiser comparison
- *(icc)* say what actually declines a transfer code point
- *(icc)* have lcms2 check every built profile's white point, curves and cicp tag
- *(icc)* gate the SourceProfile divergence figures the docs quote
- *(icc)* gate the two cross-crate figures the docs still quoted
- *(icc)* list thiserror among the crate's dependencies
- *(icc)* disclose the rendering intent every constructor writes
- *(icc)* cite cicpTag's own clause for the rule that decides the flag
- *(icc)* name the colorimetry this crate does restate, and its gates
- *(icc)* say where a SourceProfile bundle and its profile diverge
- *(icc)* guard the sampled PQ curve at the quantum it claims
- *(icc)* correct the CIE D50 tristimulus this crate deliberately avoids
- *(icc)* check the transfer-axis claim about gamut-color in a doctest
- *(icc)* cite the clauses ICC.1:2022 gives the colorant and chad tags
- *(icc)* cite cicpType's own clause and quote the tag's mid-grey values
- *(icc)* write the saturation bound at its shortest exact decimal
- *(icc)* reformat the transfer code-point sweep assertion
- *(icc)* quantify the second sanctioned reading of the BT.709 curve
- *(icc)* stop calling the full-range flag a conformance requirement
- *(icc)* pin the complement of the encodable transfer code points
- *(icc)* state the grey gamma limit instead of linking a private const
- *(icc)* correct the transfer table, the fan-in figure and the PQ limit
- *(icc)* fold the curveless transfer arm into the wildcard
- *(icc)* record the built-in profile constructors and their curve choices
- *(icc)* split the fixed-point conversions by claim
- merge origin/master into feat/324-cmm-scaffold
- merge origin/master into feat/322-lcms2-oracle-transforms
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
