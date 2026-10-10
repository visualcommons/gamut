# gamut-dng

`gamut-dng` is a pure-Rust DNG (Adobe Digital Negative) raw-image **encoder and decoder**.

## Goals

Part of the [gamut](../../README.md) workspace, this crate provides DNG writing (and a matching
raw decoder) that is:

- **Spec-faithful.** Implemented directly from the **DNG 1.7.1.0** specification
  ([`../../references/dng/DNG_Spec_1_7_1_0.pdf`](../../references/dng)) and conformance-checked
  against the **Adobe DNG SDK 1.7.1** as the authoritative oracle.
- **Memory-safe on hostile input.** `#![forbid(unsafe_code)]` — DNG's TIFF-derived, offset-driven
  structure is a classic source of parser exploits, so the decoder is built to be robust against
  malformed IFDs, offset loops, and truncation.
- **Built on shared primitives.** DNG is a profile of TIFF/EP, so its IFD container is the shared
  [`gamut-ifd`](../gamut-ifd) crate (the same spine [`gamut-tiff`](../gamut-tiff) uses), and its
  embedded metadata is the workspace facade's model, not a DNG-local restatement: a DNG's
  `ExifIFD` *is* an EXIF sub-IFD, so `DngMetadata::exif` is
  [`gamut-metadata`](../gamut-metadata)'s `Exif` over that very same directory type. This crate
  adds only the DNG-specific tags, raw photometry, colour calibration, and compression.
- **Permissively licensed**, matching the royalty-free DNG format.

DNG is **natively a still-image** raw format — a good long-term fit for gamut's image-first focus.

## Scope

A full-surface encoder **and** decoder. The decoder returns the stored sensor samples (CFA
mosaic or linear RGB) plus everything else the file carries — typed sub-images (previews,
transparency/semantic masks, depth maps), gain-table maps, the colour profile and metadata, and
every unmodelled field verbatim as typed `RawTag`s. The spec's chapter-5 "raw to linear
reference values" mapping is the explicit opt-in `RawImage::to_linear`. An **Apple ProRAW** DNG
(1.7, JPEG XL, tiled) decodes fully. Full demosaicing and colour rendering are a raw
*processor's* job and stay out of scope, as is *executing* opcodes/gain maps.

## Usage

```rust,ignore
use gamut_dng::{CameraProfile, DngEncoder, RawImage};

// `raw` is a RawImage (CFA mosaic or LinearRaw); `profile` is a CameraProfile.
let mut dng = Vec::new();
DngEncoder::new()
    .encode(&raw, &profile, &mut dng)
    .expect("encode");
```

Decoding is `DngDecoder::new().decode(&bytes)` — see `DecodedDng` for the full surface.

## Status

Implemented and conformance-checked against the Adobe DNG SDK (issue #109); see
[STATUS.md](STATUS.md) for the per-feature phase table.

- **Encode + decode**, both directions Adobe-validated: CFA mosaic and `LinearRaw` photometry;
  **strips and DNG-1.7 tiles**; **uncompressed, Deflate/ZIP (8), lossless JPEG (7), and
  JPEG XL (52546)** compression (Deflate encodes with `gamut-deflate` and inflates with
  `miniz_oxide`; JXL decode is pure-Rust jxl-rs, encode is the opt-in `jxl-encode` cargo feature
  over libjxl) with row/column interleave handling; the
  colour-calibration profile (ColorMatrix1/2, CameraCalibration, ForwardMatrix, dual illuminant,
  AnalogBalance, BaselineExposure, profile identity); the full level model (`RawLevels`: the
  BlackLevel repeat pattern with RATIONAL values, `BlackLevelDeltaH/V`, per-plane `WhiteLevel`,
  `LinearizationTable`, `MaskedAreas`), active area, default crop, and 8/10/12/14/16-bit packing;
  typed `OpcodeList1/2/3` containers (parse + pass-through write); an embedded RGB preview;
  EXIF/XMP/IPTC/ICC metadata — the `ExifIFD` carried whole as `gamut-metadata`'s `Exif`, the
  XMP/IPTC-IIM/ICC payloads verbatim and handed over as its `MetadataBlock`s; a **C2PA manifest
  store** (C2PA 2.4 §A.3.6, tag 52545) typed on both sides — written last in the file, or
  reserved zero-filled with `with_c2pa_reserved`, with `encode_with_report` returning the two
  exclusion ranges (§18.5.5) an external signer hashes around; classic TIFF and
  **BigTIFF**; the minimal `DNGVersion` and the
  spec's `DNGBackwardVersion` raises computed automatically.
- **Beyond the raw image** (decode): every other image IFD as a typed `SubImage` — previews,
  transparency and **semantic masks** (`SemanticName`/`SemanticInstanceID`/`MaskSubArea`), depth
  maps (`DepthInfo`) — decoded where the scheme is in scope, verbatim chunks otherwise; typed
  **`ProfileGainTableMap`/`ProfileGainTableMap2`** parsing with byte-exact re-serialisation; and
  every unmodelled field surfaced verbatim as a typed `RawTag` (nothing is silently dropped).
- **Colour projection** (decode): the camera-profile colour tags beyond the calibration —
  hue/saturation/value and look tables, `ProfileToneCurve`, `BaselineExposureOffset`, the DNG 1.6
  third calibration set and the reduction matrices — as a typed `ColorProfileInfo`, plus the raw
  IFD's `NoiseProfile` as a typed `NoiseProfile` with its per-plane noise model.
- **Raw digests** — the encoder writes `NewRawImageDigest` (51111), bit-matching the SDK's own
  MD5-over-raw-image computation (`RawImage::new_raw_image_digest`).
- **`RawImage::to_linear`** — the spec's chapter-5 raw-to-linear-reference mapping, differentially
  gated (±1 of 16-bit) against the Adobe SDK's stage-2 image.
- **Public `lossless_jpeg` module** — the SOF3 codec: decode covers the full T.81 process-14
  reader envelope (predictors 1–7, point transform, per-component tables, restart markers;
  SDK-differential), encode stays the predictor-1 subset every reader accepts.
- **Deferred** (ledger in `STATUS.md`): lossy JPEG (34892), floating-point samples (fp16 JXL
  rejects with a typed error), the standard opcode *processing* library, 4-colour CFA encoding,
  and writing mask/depth sub-images.

Correctness is pinned with the **Adobe DNG SDK** oracle — gamut-encode → `dng_validate` accepts
the file (raw digest included), the SDK's stage-1 decode matches gamut's own decode
pixel-for-pixel (including tiled JPEG XL), and Adobe's own sample DNGs (JPEG XL, gain maps)
decode in agreement with the SDK — plus the **libtiff** oracle for the TIFF-container/preview
layer and internal encode→decode round-trips on every path.

Those inputs are all synthetic or Adobe-authored, so a separate tier (issue #174) gates the crate
against **files real cameras wrote**: six CC0 DNGs — Apple ProRAW, Canon via Adobe DNG Converter
in all three compressions, a monochrome Leica, a Leica M10 — in the `gamut-dng-samples` submodule,
checked for byte completeness, decode, stored digest, the Adobe stage-2 differential and the
preserving rewrite. It lives outside the workspace so a normal `cargo test` never pulls in
~178 MiB of camera files:

```sh
mise run fetch-dng-samples   # clone the corpus submodule (once)
mise run test-dng-real       # run the real-camera conformance tier
```

## License

Licensed under either of MIT or Apache-2.0 at your option.
