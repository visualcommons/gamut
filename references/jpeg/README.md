# JPEG (JPEG-1, ISO/IEC 10918 / ITU-T T.8x)

Reference specifications for a future **`gamut-jpeg`** codec — the baseline/extended DCT-based
still-image process (JPEG-1) plus its file formats (JFIF) and marker/registration companions. These
also document the JPEG constructs consumed elsewhere in the workspace: JPEG-in-TIFF (`gamut-tiff`
compression `6`/`7`, TIFF 6.0 §22 + Technical Note 2) and lossless-JPEG in DNG (`gamut-dng`).

JPEG-1 is published jointly as the **ISO/IEC 10918** multi-part standard and the **ITU-T T.8x**
Recommendations; the two are technically identical, and ITU freely publishes the T-series text while
the ISO/IEC parts are paywalled. The files below are the freely-published ITU editions.

## The ISO/IEC 10918 ↔ ITU-T map

| ISO/IEC | ITU-T | Title | Vendored |
| --- | --- | --- | --- |
| 10918-1 | T.81 | Requirements and guidelines (the core codec) | ✅ `itu-t81.pdf` (+ Cor.1) |
| 10918-2 | T.83 | Compliance testing | ✗ (see *Not vendored*) |
| 10918-3 | T.84 | Extensions | ✅ `T-REC-T.84-…` (+ Amd.1) |
| 10918-4 | T.86 | Registration of profiles/APPn markers (REGAUT) | ✅ `T-REC-T.86-…` |
| 10918-5 | T.871 | JPEG File Interchange Format (JFIF) | ✅ `T-REC-T.871-…` (+ Err.1) |
| 10918-6 | T.872 | Application to printing systems | ✅ `T-REC-T.872-…` |
| 10918-7 | T.873 | Reference software | ✅ `T-REC-T.873-…SOFT-ZST-E.zip` |

One companion outside the 10918 family is also vendored because a spec-complete codec needs it: **Adobe
Technical Note #5116** (`APP14` colour-transform marker) — see *Adobe `APP14` colour transform* below.

## Authoritative editions (vendored)

### Core codec — 10918-1 / T.81

- `itu-t81.pdf` — **ITU-T Rec. T.81 (09/1992) | ISO/IEC 10918-1** — *Information technology — Digital
  compression and coding of continuous-tone still images — Requirements and guidelines.* The
  foundational JPEG-1 specification: the baseline and extended sequential/progressive DCT processes,
  the lossless process, entropy coding (Huffman and arithmetic), the marker syntax (SOI/SOF/SOS/DQT/
  DHT/DRI/…), and the interchange/abbreviated formats. This is the freely-redistributed copy hosted by
  the W3C (`https://www.w3.org/Graphics/JPEG/itu-t81.pdf`); it is byte-for-byte the 1992 text.
- `T-REC-T.81-200401-I!Cor1!PDF-E.pdf` — **T.81 (1992) Technical Corrigendum 1 (01/2004)** — patent
  information update only; no normative change to the codec. Retained for completeness.

### Extensions — 10918-3 / T.84

- `T-REC-T.84-199607-I!!PDF-E.pdf` — **ITU-T Rec. T.84 (07/1996) | ISO/IEC 10918-3** — *…continuous-tone
  still images: Extensions.* Adds the variable quantization, selective refinement, tiling, still-picture
  interchange (SPIFF) file format, and the extended registration mechanisms layered on T.81.
- `T-REC-T.84-199904-I!Amd1!PDF-E.pdf` — **T.84 Amendment 1 (04/1999)** — *Provisions to allow
  registration of new compression types and versions in the SPIFF header.*

### Marker & file-format companions

- `T-REC-T.86-202402-I!!PDF-E.pdf` — **ITU-T Rec. T.86 (V2) (02/2024) | ISO/IEC 10918-4** —
  *…continuous-tone still images: APPn markers* (the Registration Authority / REGAUT document). Defines
  the registration of JPEG profiles, SPIFF profiles/tags/colour spaces, `APPn` markers, and compression
  types. This is the current (2024) edition.
- `T-REC-T.871-201105-I!!PDF-E.pdf` — **ITU-T Rec. T.871 (05/2011) | ISO/IEC 10918-5** — *…continuous-tone
  still images: JPEG File Interchange Format (JFIF).* The de-facto container for JPEG-1 in the wild: the
  `APP0`/`JFIF` and `JFXX` marker segments, aspect ratio/density, and thumbnail conventions.
- `T-REC-T.871-201303-I!Err1!PDF-E.pdf` — **T.871 Erratum 1 (03/2013)** — corrects the publication date
  in the page headers to reflect ISO/IEC 10918-5:2013; editorial only.
- `T-REC-T.872-201206-I!!PDF-E.pdf` — **ITU-T Rec. T.872 (06/2012) | ISO/IEC 10918-6** — *…continuous-tone
  still images: Application to printing systems.* Defines the `APP11`/print-industry conventions.

### Adobe `APP14` colour transform — Technical Note #5116 (non-ISO, but required)

- `adobe-tn5116-dct-filter.pdf` — **Adobe Technical Note #5116, *Supporting the DCT Filters in PostScript
  Level 2* (24 November 1992).** *Not* part of ISO/IEC 10918 / ITU-T T.8x, and the only vendored companion
  outside that family — but required for correct real-world JPEG-1. It defines the `APP14`/`Adobe` marker
  segment whose colour-transform flag (`0` = unknown → RGB or CMYK, `1` = YCbCr, `2` = YCCK) is the
  authoritative signal for whether a decoder applies the inverse colour transform on 3-component non-JFIF
  images and on all 4-component (CMYK/YCCK) images; JFIF/SPIFF/T.81 provide no equivalent. The intended
  libjpeg-turbo/mozjpeg oracle emits and consumes this marker, so encode/decode parity requires handling
  it. Freely published by the PDF Association as an ISO 32000 normative reference; downloaded from
  `https://pdfa.org/norm-refs/5116.DCT_Filter.pdf`.

### Reference software — 10918-7 / T.873

- `T-REC-T.873-202309-I!!SOFT-ZST-E.zip` — **ITU-T Rec. T.873 (09/2023) | ISO/IEC 10918-7 — Reference
  software (V3).** The official JPEG-1 reference implementation. The archive bundles the T.873 PDF plus
  two reference codebases (`reference A`, `reference B`); it is the normative behavioural oracle for the
  core codec.

## APPn metadata embedding (EXIF / XMP / ICC)

`gamut-jpeg` reads and writes the three de-facto metadata payloads carried in JPEG application
segments. T.81 defines only the `APPn` marker syntax (§B.2.4.6: a 2-byte length that includes
itself, so **at most 65533 payload bytes** per segment); the payload conventions come from the
metadata standards themselves, vendored in the sibling directories:

| Segment | Signature (NUL included) | Payload | Governing spec (vendored) |
| --- | --- | --- | --- |
| `APP1` | `"Exif\0\0"` (6 bytes) | TIFF stream (`II`/`MM` …), ≤ 65527 bytes; a single segment recorded immediately after SOI | **Exif 3.0** §4.7.2, [`../exif`](../exif/README.md) |
| `APP1` | `"http://ns.adobe.com/xap/1.0/\0"` (29 bytes) | one XMP `xpacket`, ≤ **65502** bytes (spec-stated cap); placed before the first SOF | **XMP Part 3** §1.1.3, [`../xmp`](../xmp/README.md) |
| `APP2` | `"ICC_PROFILE\0"` (12 bytes) + chunk index (1 byte, **1-based**) + chunk count (1 byte) | ≤ 65519 profile bytes per chunk; all chunks carry the same count, so ≤ 255 chunks ⇒ max profile 16,707,345 bytes | **ICC.1:2001-04** Annex B.4, [`../icc`](../icc/README.md) |

Notes pinned from the vendored texts:

- **JFIF/Exif coexistence** — both JFIF (T.871) and Exif (DC-008 §4.7.2.1) claim the segment
  slot immediately after SOI, and neither references the other. XMP Part 3 §1.1.3 records the
  observed convention: readers must accept an Exif `APP1` *following* the JFIF `APP0`/`JFXX`
  segments, which is the libjpeg-family order (`APP0`, then `APP1`) that `gamut-jpeg` emits.
- **ExtendedXMP** (XMP Part 3 §1.1.3.1) — an XMP packet larger than the single-segment cap is
  split into a StandardXMP packet plus `APP1` segments signed
  `"http://ns.adobe.com/xmp/extension/\0"` carrying a 32-byte GUID (MD5 of the full extended
  serialization), a 4-byte total length, and a 4-byte offset, cross-linked via
  `xmpNote:HasExtendedXMP`. This is a separate mini-protocol; `gamut-jpeg` defers it (see the
  crate `STATUS.md`) and skips such segments on read.
- **ICC multi-segment reassembly** — chunk indices count from 1; every chunk must repeat the
  same total count, and T.81 does not guarantee segment order, so readers reassemble by index
  rather than appearance order.
- ICC.1:2022 replaced its Annex B embedding text with a reference to ICC Technical Note 10-21;
  the normative-in-practice wire format is unchanged from the vendored ICC.1:2001-04 Annex B.4.

## Not vendored (paywalled / cross-checked via oracle)

- **ISO/IEC 10918-2 (ITU-T T.83) — Compliance testing** — the conformance test-data part. Not freely
  published by ITU (no T-series companion is posted) and paywalled at ISO; per the same policy applied to
  the paywalled ISOBMFF/TIFF-EP specs, JPEG-1 conformance is instead exercised against the vendored T.873
  reference software and a C reference codec (see *Conformance*), rather than vendoring this part.

## Conformance

The intended correctness strategy mirrors the other codec crates: a **differential oracle** against a
canonical C JPEG-1 implementation (libjpeg-turbo / mozjpeg family) plus the vendored **T.873 reference
software** for spec-exact behaviour, gating decode round-trips and encode parity. The concrete oracle
crate and gate will be documented in `gamut-jpeg`'s `STATUS.md` when the crate lands.

## mozjpeg progressive scan script (issue #1021)

T.81 §G leaves the progressive scan script (which spectral bands and successive-approximation bits
each scan carries, and in what order) to the encoder. `gamut-jpeg` freezes libjpeg's
`jpeg_simple_progression` as its default and offers mozjpeg's maximum-compression script as an
opt-in (`ProgressiveScript::Mozjpeg`). mozjpeg is not vendored; the source is documented here.

- **Source:** mozjpeg **v4.1.5**, `jcparam.c`, function `jpeg_simple_progression`, the
  `compress_profile == JCP_MAX_COMPRESSION` branches with the default `dc_scan_opt_mode = 0`.
  Fetched from `https://raw.githubusercontent.com/mozilla/mozjpeg/v4.1.5/jcparam.c`
  (991 lines, SHA-256 `563fe8e2fbf71760a269cced3dda5a6e67e1a1facc6bbd5f8a9727db5a5d4d9a`). Its
  comments name the scripts' origin: jpgcrush's `jpeg_scan_rgb.txt` (YCbCr) and
  `jpeg_scan_bw.txt` (all-purpose).
- **Licence:** IJG licence + libjpeg-turbo/mozjpeg modified BSD (*"This file was part of the
  Independent JPEG Group's software … mozjpeg Modifications: Copyright (C) 2014, Mozilla
  Corporation"*). The excerpt below is quoted for citation.
- **Selection rule:** the custom script applies when `ncomps == 3 && jpeg_color_space ==
  JCS_YCbCr`; every other frame takes the all-purpose script, which `fill_scans` repeats for each
  component in frame order, after one interleaved DC scan (`fill_dc_scans`, interleaved for at
  most `MAX_COMPS_IN_SCAN = 4` components). With `optimize_scans` on (mozjpeg's default) this
  fixed script is replaced by a search (`jpeg_search_progression`); gamut does not ship the search.

The two branches, quoted verbatim (`fill_a_scan(ptr, ci, Ss, Se, Ah, Al)`):

```c
  if (ncomps == 3 && cinfo->jpeg_color_space == JCS_YCbCr) {
    /* Custom script for YCbCr color images. */
    if (cinfo->master->compress_profile == JCP_MAX_COMPRESSION) {
      /* scan defined in jpeg_scan_rgb.txt in jpgcrush */
    /* Initial DC scan */
      if (cinfo->master->dc_scan_opt_mode == 0) {
        /* 1 DC scan for all components */
        scanptr = fill_dc_scans(scanptr, ncomps, 0, 0);
      } /* … dc_scan_opt_mode 1 and 2 elided … */
      /* Low frequency AC scans */
      scanptr = fill_a_scan(scanptr, 0, 1, 8, 0, 2);
      scanptr = fill_a_scan(scanptr, 1, 1, 8, 0, 0);
      scanptr = fill_a_scan(scanptr, 2, 1, 8, 0, 0);
      /* Complete spectral selection for luma AC */
      scanptr = fill_a_scan(scanptr, 0, 9, 63, 0, 2);
      /* Finish luma AC successive approximation */
      scanptr = fill_a_scan(scanptr, 0, 1, 63, 2, 1);
      scanptr = fill_a_scan(scanptr, 0, 1, 63, 1, 0);
      /* Complete spectral selection for chroma AC */
      scanptr = fill_a_scan(scanptr, 1, 9, 63, 0, 0);
      scanptr = fill_a_scan(scanptr, 2, 9, 63, 0, 0);
    } /* … libjpeg's script elided … */
  } else {
    /* All-purpose script for other color spaces. */
    if (cinfo->master->compress_profile == JCP_MAX_COMPRESSION) {
      /* scan defined in jpeg_scan_bw.txt in jpgcrush */
      /* DC component, no successive approximation */
      scanptr = fill_dc_scans(scanptr, ncomps, 0, 0);
    /* Successive approximation first pass */
      scanptr = fill_scans(scanptr, ncomps, 1, 8, 0, 2);
      scanptr = fill_scans(scanptr, ncomps, 9, 63, 0, 2);
      /* Successive approximation second pass */
      scanptr = fill_scans(scanptr, ncomps, 1, 63, 2, 1);
      /* Successive approximation final pass */
      scanptr = fill_scans(scanptr, ncomps, 1, 63, 1, 0);
    } /* … libjpeg's script elided … */
  }
```

Experiment #30 (`experiments/30/RESULTS.md`, *Scan scripts*) measured this script against the
frozen libjpeg one on the same quantized coefficients: smaller on average in every image ×
configuration cell (−0.35% grey, −0.66% 4:2:0, −1.51% 4:4:4), equal or up to 0.52% larger at 16 of
294 points, at identical libjpeg-turbo-decoded pixels.

## XYB-coded JPEG (the jpegli convention, issue #334)

An XYB JPEG stores three components of scaled-XYB samples (see `references/color/README.md`,
"XYB") rather than YCbCr. Because T.871 defines the three-component **JFIF** stream as YCbCr, an
XYB stream carries **no APP0**; jpegli marks the samples untransformed by using component ids
`R`,`G`,`B` (0x52, 0x47, 0x42), and gamut additionally writes an Adobe **APP14** with
`transform = 0` (TN #5116, vendored above) so either signal alone stops a decoder from applying a
YCbCr inverse. The colour meaning rides in a mandatory multi-segment APP2 `ICC_PROFILE`
(`gamut_jpeg::XYB_ICC_PROFILE`) whose `A2B0` pipeline maps the samples to D50 PCS XYZ — an
ICC-aware consumer reproduces sRGB; an ICC-unaware one sees false colour, which is the format's
documented trade-off (jpegli behaves identically).
