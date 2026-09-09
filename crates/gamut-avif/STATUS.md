# gamut-avif — implementation status

The complete component surface a conformant AVIF encoder — and, since issue #250, the AVIF
**container decoder** (section L) — needs, drawn from every related spec (AV1 Bitstream & Decoding
Process Specification; AVIF v1.2.0; AV1-ISOBMFF v1.3.0; ISO/IEC 14496-12 ISOBMFF; 23008-12 HEIF;
23000-22 MIAF; ITU-T H.273 CICP). Rows are **technical components**, not user features. This is
the map for extension: each module's doc comment cites the same spec sections, and a row flips
☐→✅ (with the module cross-reference) when it ships.

**Status:** ✅ = implemented · ☐ = deferred (planned, additive) · **OOS** = permanently out of
scope. A ✅ cell may carry a qualifier when the row is only partially covered. The **M** column is
historical sequencing provenance (the milestone that motivated a row — M0 is complete, most of M1
has landed), `D` where a deferred row has no milestone, `OOS` where the disposition is final:

- **M0** — MVP: lossless intra, identity `mc=0`, 4:4:4, 8-bit, full range, single tile,
  64×64 superblocks, `DC_PRED`, forced `TX_4X4` Walsh–Hadamard, static default CDFs
  (`disable_cdf_update = 1`). Verified bit-exact against vendored `libavif`/`dav1d`.
- **M1** — Lossy intra: forward DCT/ADST + quantization + RD/rate control, CDF adaptation, full
  intra mode set, variable tx size/type, multi-tile, in-loop filters, 128×128 SB, full partition
  set, segmentation/delta-q, superres, screen-content tools (palette/intrabc).
- **M2** — Pixel formats: 10/12-bit, 4:2:0/4:2:2, monochrome, profiles 0 & 2, `MA1B` baseline
  brand, and chroma resampling. *(The RGB↔YCbCr matrices and limited range landed early, with
  issue #335 — they need no plane-geometry change. Monochrome landed with #396; 4:2:0, profile 0
  and `MA1B` landed with #390; 4:2:2 with #391. What remains is 10/12-bit.)*
- **M3** — Alpha & auxiliary: alpha aux item, `auxC`/`auxl`, premultiplied (`prem`), depth maps.
- **M4** — Color & metadata: ICC profiles, Exif/XMP items, HDR (PQ/HLG, `mdcv`/`clli`), film grain.
- **M5** — Container transforms & derivation: `irot`/`imir`/`clap`/`pasp`, `grid`/overlay,
  thumbnails, `idat`, `iloc` v1/v2.

## Scope & dispositions (v1)

**Implemented (v1.0).** Lossless (decoded output bit-exact to the input, at the coded depth) and
lossy AV1 intra encoding at 8/10/12-bit, from `Rgb8`/`Rgba8`/`Gray8`/`Rgb16`/`Rgba16` — identity
matrix at 4:4:4 for lossless, **BT.709 YCbCr at 4:2:0 by default** for lossy, with
BT.601/BT.2020-NCL, studio range and 4:4:4 / 4:2:2 selectable (the `Rgb8` path; `Rgba8` and the
16-bit paths are 4:4:4 — see the ☐ rows below) — wrapped as a conformant MIAF/AVIF `av01` item
(plus a monochrome alpha auxiliary when the input carries one) — brands `avif`/`mif1`/`miaf` plus
the profile brand every coded item earns together (`MA1B` when all are AV1 Main, `MA1A` when all
are High, neither for a mixed or 4:2:2 file), the AVIF §9.1.1 minimum box set, cross-box
consistency (`av1C`↔sequence header, `pixi`, `colr`, `ispe`) by construction — with `irot`/`imir`
display orientation. The **colour and metadata surface** (issue #395) rides on top: selectable CICP
primaries and transfer characteristics, an embedded ICC profile as a `colr` box of type `prof`
alongside the CICP one, and Exif / XMP items carrying a `cdsc` reference to the primary. Those four
are pure container-side additions — they leave the codestream byte-for-byte unchanged, which the
libavif acceptance suite pins directly. Output is validated end-to-end against `libavif` (dav1d backend); the wrapped
AV1 bitstream is cross-checked against `libaom` (the AV1 reference codec) and `dav1d` via
`gamut-av1`. Evidence per section: A and K rows are pinned by this crate's parse-back unit tests,
doctests, and the `libavif` round-trip/remux integration tests; B–H rows are owned by `gamut-av1`
and evidenced by its `libaom`/`dav1d` differential suite; J rows by `gamut-color`'s tests.

**C2PA carriage (issue #444, epic #239).** The encoder reserves or writes a C2PA manifest store
as a top-level `uuid` `ContentProvenanceBox` (C2PA 2.4 §A.5.1) placed after `ftyp` and before
`meta` (§A.5.3, via `IsoBmffImage::push_top_level_box`): `AvifEncoder::with_c2pa_reserved(len)`
writes the §A.5.1.2 framing — zero `FullBox` version/flags, `box_purpose` `manifest`, a zero
8-byte merkle offset — around `len` zero bytes for an external signer to fill, `with_c2pa(bytes)`
writes a store the caller has already computed over this exact output, and
`AvifEncoder::encode_with_report` returns the slot's file range beside the (unchanged) bytes, so
the offset is known before the signer runs. Nothing after placement moves a byte
(`tests/c2pa.rs`, exact-byte). The read side, `AvifContainer::c2pa` / `c2pa_manifest_stores`,
locates every top-level C2PA `uuid` box and reports its purpose, slot bytes and file range as a
`C2paSlot` (named for the box-bounded slot it reports, not for a trimmed store). The
object-safe `EncodeImage` entry point is untouched. libavif and dav1d decode a file carrying the
box to the same pixels as one without. See the C2PA note under section L for the four recorded
limits.

**Deferred (planned, additive).** Every ☐ row below: 4:2:0/4:2:2 and `MA1B` landed with
#390/#391, the alpha auxiliary, `Gray8` and monochrome surface with #396/#397, and the 10/12-bit
path end to end with #398/#399, and subsampled chroma on the `Rgb16` path with #399 — what remains
of the pixel-format surface is subsampled chroma on the `Rgba8`/`Rgba16` paths, which need a
4-stride downsampler; depth auxiliary items; the HDR
surface beyond CICP *tagging* (`mdcv`/`clli`/`cclv`/`amve`/`reve`/`ndwt`, film grain — selecting a
PQ/HLG transfer labels samples but does not by itself make a conformant HDR image); container
derivations
(`grid`, thumbnails, `idat`, `iloc` v1/v2 emission, `pasp`/`clap`); layered/progressive still
images (`a1op`/`a1lx`/`lsel`, multi-operating-point sequence header); `tmap` tone-map (gain-map)
derived items; `sato` sample transforms (bit depths beyond 12); `cmin`/`cmex` camera matrices;
`altr`/`ster` entity groups; encoder speed and rate control; and CLI/wasm/ffi wiring. The
**container decode surface** (issue #250) has since landed additively as new crate items —
section L is its ledger; the remaining decode-side gap is the **pure-Rust AV1 codestream
decoder** (its own issue — `libaom`'s reference encoder is already staged as that decoder's
oracle, see [`references/av1`](../../references/av1/README.md); today the codestream is supplied
through the external `Av1StillDecoder` seam). **Additivity guarantee:** each lands semver-minor —
a new builder method on the (non-`Copy`) `AvifEncoder`, a new field on the `#[non_exhaustive]`
`AvifConfig`, a new `AvifMode` variant, or a new crate item — never a reshape of the v1 surface.
The **encoded bytes** are not part of that guarantee and never were. Issue #335 changed the lossy
default from identity to BT.709 YCbCr (31–38% smaller on correlated photographic content), and
issue #390 changed it again from 4:4:4 to 4:2:0 — a further 42% on the golden fixture, and the
change that makes the output readable by a Main-profile-only hardware decoder at all. Both change
every default lossy stream. The lossless default is untouched by either and stays bit-exact.

**Permanently out of scope (workspace charter: image-first, no inter-frame/motion/sequence
coding).** Image sequences and tracks — the `avis` and `avio` brands (AVIF §3, §6.3),
`moov`/`trak`/`mdia`/`stbl` and `av01` sample entries — and the AV1 inter-coding machinery
(section I below; INTRA_ONLY/INTER/SWITCH frame types, global motion, inter/MV entropy contexts,
and the timing_info/decoder_model sequence-header fields that only sequences use). Also
`dinf`/`dref` external data references (a still image is self-contained), mirroring the finalized
[`gamut-isobmff` ledger](../gamut-isobmff/STATUS.md).

## A. Container / file format (ISOBMFF · HEIF · MIAF · AVIF · AV1-ISOBMFF)

The box machinery for every non-OOS row below already ships in
[`gamut-isobmff` v1](../gamut-isobmff/STATUS.md) (`iref`/`auxC`/`prem`, ICC `colr`, Exif/`mime`
items, `clap`/`pasp`/`clli`, `grid`+`dimg`, `idat`, `iloc` v1/v2 on read); a ☐ here means the
*codec-side wiring* (encoding the aux plane, stamping the property, exposing the API) is pending —
adding it needs no container change.

| Component | Spec | Status | M |
| --- | --- | --- | --- |
| `ftyp`: major `avif`, compat `avif`/`mif1`/`miaf`/`MA1A` | AVIF §6,§8.3 | ✅ | M0 |
| `MA1B` baseline brand (Main profile, ≤L5.1, 4:2:0) | AVIF §8.2 | ✅ | M2 |
| §6.10.4 partition constraint (no taller-than-wide block at 4:2:2) | §6.10.4,§5.11.38 | ✅ | M2 |
| `avis` brand (image sequences) | AVIF §3,§6.3 | OOS | OOS |
| `avio` brand (intra-only image sequences) | AVIF §6.3 | OOS | OOS |
| `meta` (FullBox v0) container | 14496-12 | ✅ | M0 |
| `hdlr` handler_type=`pict` | 23008-12 | ✅ | M0 |
| `pitm` primary item id | 14496-12 | ✅ | M0 |
| `iloc` v0, construction_method=0 → `mdat`, 4-byte `extent_offset` back-patch | 14496-12 | ✅ | M0 |
| `iloc` v1/v2, construction_method=1 (`idat`)/2 (item) | 14496-12 | ☐ | M5 |
| `iinf`+`infe` v2, item_type=`av01` | 14496-12 | ✅ | M0 |
| `iprp`/`ipco`/`ipma` property association (`av1C` essential) | 14496-12; AVIF §2.2.1 | ✅ | M0 |
| `av1C` AV1ItemConfigurationProperty, empty `configOBUs` | AV1-ISOBMFF §2.3 | ✅ | M0 |
| `ispe` image spatial extents | 23008-12 | ✅ | M0 |
| `pixi` pixel information (one entry per coded plane, at the stream's own depth: 3×8/10/12, or 1× monochrome) | 23008-12 | ✅ | M0/M3 |
| `colr` type `nclx` (CICP code points) | AVIF §2.2; AV1-ISOBMFF §2.3.4 | ✅ | M0 |
| `colr` type `rICC`/`prof` (ICC profile) | 23008-12 | ✅ (`prof` written by `AvifEncoder::with_icc_profile`; both read) | M4 |
| `pasp` pixel aspect ratio | 14496-12 | ☐ | M5 |
| `clap` clean aperture | 23008-12 | ☐ | M5 |
| `irot` rotation / `imir` mirror | 23008-12 | ✅ (essential transform properties; `AvifEncoder::with_rotation`/`with_mirror`) | M5 |
| `auxC` aux-type property + `auxl` item ref (alpha plane) | 23008-12; AVIF §4.1 | ✅ (written for `Rgba8`, essential, hidden item, no `colr`; all read) | M3 |
| depth auxiliary image item (`urn:…:auxiliary:depth`) | AVIF §4.1 | ☐ | M3 |
| `prem` premultiplied-alpha association | AVIF §4 | ✅ (`AvifEncoder::with_premultiplied_alpha`) | M3 |
| `iref` (`auxl`/`dimg`/`thmb`/`cdsc`) | 23008-12 | ✅ (`cdsc`/`auxl`/`prem` emitted; all read) | M3/M5 |
| `grid` derived item + `dimg` refs (tiled mosaic) | 23008-12; MIAF | ☐ | M5 |
| `tmap` tone-map derived item (gain maps) + `altr` grouping with the base item | AVIF §4.2.2 | ☐ | D |
| `sato` sample-transform derived item (bit-depth extension beyond 12) | AVIF §4.2.3, App. A | ☐ | D |
| `altr` alternatives entity group | AVIF §5.1; §9.1.2 | ☐ | D |
| `ster` stereo-pair entity group | AVIF §5.2; §9.1.2 | ☐ | D |
| `cclv`/`amve`/`reve`/`ndwt` HDR item properties (carried opaque by `gamut-isobmff` until typed) | AVIF §9.1.2 | ☐ | M4 |
| `cmin`/`cmex` camera intrinsic/extrinsic matrices | AVIF §9.1.2 (HEIF) | ☐ | D |
| `idat` inline item data | 14496-12 | ☐ | M5 |
| `thmb` thumbnail item | 23008-12 | ☐ | M5 |
| Exif / XMP metadata items + `cdsc` ref | AVIF §9.1.2; 23008-12 | ✅ (`AvifEncoder::with_exif`/`with_xmp`) | M4 |
| C2PA `ContentProvenanceBox`: top-level `uuid` (user type `D8FEC3D6-…-C481`) after `ftyp`, `box_purpose` `manifest`, zero merkle offset, a reserved zero slot or a caller-computed store; slot range via `encode_with_report` | C2PA 2.4 §A.5.1, §A.5.3 | ✅ (#444; `AvifEncoder::with_c2pa_reserved`/`with_c2pa`) | D |
| `a1op` operating-point sel / `a1lx` layered index / `lsel` layer sel (layered/progressive stills) | AVIF §2.3 | ☐ | D |
| `dinf`/`dref` external data references | AVIF §9.1.2 | OOS | OOS |
| sequence tracks: `moov`/`trak`/`mdia`/`stbl`, `av01` sample entry, `av1C` in `stsd` | 14496-12; AV1-ISOBMFF §3 | OOS | OOS |
| `mdat` payload = AV1 temporal unit OBUs | AV1-ISOBMFF §2.4 | ✅ | M0 |
| cross-box consistency (av1C↔seq-hdr, `pixi`, `colr` range, `ispe` dims) | AVIF §2.2/§2.3.4 | ✅ | M0 |

## B. AV1 — OBUs, sequence & frame headers

| Component | Spec | Status | M |
| --- | --- | --- | --- |
| OBU header + `obu_has_size_field`=1 + LEB128 size | §5.3,§4.10.5 | ✅ | M0 |
| `OBU_SEQUENCE_HEADER` | §5.5 | ✅ | M0 |
| `OBU_FRAME` (frame header ∥ tile group) | §5.10 | ✅ | M0 |
| `OBU_FRAME_HEADER` + separate `OBU_TILE_GROUP` | §5.9/§5.11 | ☐ | M1 |
| `OBU_TEMPORAL_DELIMITER` (omitted in AVIF item) | §5.6; AV1-ISOBMFF §2.4 | ✅ (omit) | M0 |
| `OBU_METADATA` (ITU-T T.35, HDR CLL, HDR MDCV, scalability, timecode) | §5.8 | ☐ | M4 |
| `OBU_PADDING` / `OBU_REDUNDANT_FRAME_HEADER` | §5.7/§5.9 | ☐ | — |
| `OBU_TILE_LIST` (large-scale tiles; forbidden in AVIF item) | §5.12 | ☐ | — |
| seq_profile=1 (High) | Annex A §10.2; §6.4.1 | ✅ | M0 |
| seq_profile=0 (Main) / =2 (Professional, 12-bit/4:2:2) | Annex A §10.2 | ✅ (joint over layout × depth — 0 = 4:2:0 **and monochrome**, 1 = 4:4:4, 2 = 4:2:2 **or any layout at 12-bit**; `seq_profile_for`) | M2 |
| `still_picture`=1, `reduced_still_picture_header`=1 | §5.5 | ✅ | M0 |
| full seq header: multiple operating points (layered stills) | §5.5.1-.5.5.5 | ☐ | D |
| full seq header: timing_info, decoder_model_info (sequences only) | §5.5.1-.5.5.5 | OOS | OOS |
| `frame_id_numbers_present` | §5.5 | ☐ | — |
| `use_128x128_superblock` | §5.5 | ☐ | M1 |
| `enable_filter_intra` (1 on lossy, 0 on lossless) / `enable_intra_edge_filter`=0 | §5.5 | ✅ | M0/M1 |
| `enable_superres`/`cdef`/`restoration`=0 | §5.5 | ✅ (off) | M0 |
| color_config: mc=0 identity, 4:4:4, high_bitdepth=0, full range | §5.5.2 | ✅ | M0 |
| high-bit-depth **input**: `Rgb16`/`Rgba16` narrowed to the coded depth (`AvifEncoder::with_bit_depth`, default 12-bit, truncating — see the note below the table) | AVIF §2.2 | ✅ (#399) | M2 |
| color_config: high_bitdepth/twelve_bit, mono_chrome, subsampling, chroma_sample_position | §5.5.2 | ✅ (the whole §5.5.2 walk in `gamut_av1::headers`, and read back symmetrically in `gamut_avif::backend`: `high_bitdepth`/`twelve_bit`, `mono_chrome` with its inferred-subsampling branch, profile-inferred subsampling plus profile 2's coded pair at 12-bit, and `chroma_sample_position` (`CSP_UNKNOWN`) at 4:2:0) | M2 |
| frame_type=KEY_FRAME, show_frame=1 | §5.9.2 | ✅ | M0 |
| INTRA_ONLY / INTER / SWITCH frame types | §5.9.2 | OOS | OOS |
| `disable_cdf_update`=1 (static CDFs) | §5.9.2 | ✅ | M0 |
| `disable_cdf_update`=0 + frame-end CDF update | §5.9.2,§7.7 | ✅ (`headers::frame_header_payload`; §7.7 n/a — see below) | M1 |
| frame_size / render_size (no override, no superres) | §5.9.5/.6 | ✅ | M0 |
| superres_params (enable_superres + use_superres + coded_denom) | §5.9.8,§7.16 | ✅ (frame_size_override deferred) | M1 |
| tile_info: single tile | §5.9.15 | ✅ | M0 |
| multi-tile (uniform spacing, tile_size_bytes, context_update_tile_id, tile group) | §5.9.15/.16 | ✅ (2 cols ≥2 SB wide; rows deferred) | M1 |
| quantization: base_q_idx=0 ⇒ CodedLossless | §5.9.12 | ✅ | M0 |
| quantization: base_q_idx>0, delta-Q, using_qmatrix | §5.9.12/.13,§9.5 | ✅ (base_q_idx>0 + per-SB delta-Q; qmatrix ☐) | M1 |
| segmentation_params (disabled) | §5.9.14 | ✅ (off) | M0 |
| segmentation (8 segments, features, temporal pred) | §5.9.14 | ✅ (lossy; SEG_LVL_ALT_Q + spatial segment_id map; temporal pred ☐) | M1 |
| delta_q_params / delta_lf_params | §5.9.17/.18 | ✅ (delta_q + delta_lf) | M1 |
| read_tx_mode → ONLY_4X4 (lossless) | §5.9.21 | ✅ | M0 |
| TX_MODE_SELECT / TX_MODE_LARGEST | §5.9.21 | ✅ (TX_MODE_SELECT, lossy intra) | M1 |
| `reduced_tx_set`=1 | §5.9.2 | ✅ | M0 |
| frame_reference_mode / skip_mode_params (intra → off) | §5.9.22/.23 | ✅ (off) | M0 |
| global_motion_params | §5.9.24 | OOS | OOS |
| film_grain_params | §5.9.30 | ☐ | M4 |

## C. AV1 — tiling, partition, block / mode info

| Component | Spec | Status | M |
| --- | --- | --- | --- |
| single-tile `decode_tile`, above/left context clear | §5.11.2/.3 | ✅ | M0 |
| `decode_partition`: PARTITION_NONE + edge-forced SPLIT/HORZ/VERT | §5.11.4 | ✅ | M0 |
| full partition set: HORZ/VERT/SPLIT/HORZ_A/B/VERT_A/B/HORZ_4/VERT_4 | §5.11.4 | ✅ (HORZ/VERT + NONE/SPLIT; A/B/4 deferred) | M1 |
| rectangular transforms TX_16X8/8X16/32X16/16X32 (+scan, aspect coeff ctx) | §7.13.3/§8.3.2 | ✅ | M1 |
| `intra_frame_mode_info` (KEY-frame block) | §5.11.7 | ✅ | M0 |
| `skip` flag = 0 (residual always coded) | §5.11.11 | ✅ | M0 |
| `skip` = 1 (no-residual / all-zero blocks) | §5.11.11 | ✅ (lossy; all-skip 8×8 unfiltered by CDEF) | M1 |
| intra_segment_id / read_segment_id (spatial pred, neg_interleave) | §5.11.8/.9 | ✅ (lossy multi-segment) | M0/M1 |
| per-block read_cdef / read_delta_qindex / read_delta_lf | §5.11.56/.12/.13 | ✅ (delta_q + delta_lf; read_cdef 0-bit) | M1 |
| read_tx_size / read_var_tx_size (per-block tx_depth) | §5.11.15-.17 | ✅ (TX_MODE_SELECT, square tx_depth 0..2) | M0/M1 |

## D. AV1 — intra prediction (§7.11.2)

| Component | Spec | Status | M |
| --- | --- | --- | --- |
| `DC_PRED`, availability-aware (luma + chroma) | §7.11.2.5 | ✅ | M0 |
| directional V/H/D45/.../D67 + `angle_delta` + edge filter/upsample | §7.11.2.4/.9-.12 | ✅ (lossy luma 4×4; 8×8/16×16/32×32 + `angle_delta`; edge filter/upsample `enable_intra_edge_filter=0`) | M1 |
| SMOOTH / SMOOTH_V / SMOOTH_H | §7.11.2.6 | ✅ (lossy luma, square 4×4–32×32 + rectangular; SAD-selected) | M1 |
| PAETH | §7.11.2 | ✅ (lossy luma, square 4×4–32×32 + rectangular; SAD-selected) | M1 |
| recursive filter-intra | §7.11.2.3,§5.11.24 | ✅ (lossy luma 4×4 + 8×8 + 16×16 + 32×32) | M1 |
| chroma-from-luma (CfL) + `cfl_alpha` | §7.11.5,§5.11.45 | ✅ (lossy; the §7.11.5 subsampled box average at every layout) | M1 |
| palette mode (palette_tokens, color cache) | §7.11.4,§5.11.46-.50 | ✅ (lossy luma 8×8/16×16/32×32; sizes 2..8; color cache + wavefront index map) | M1 |
| intra block copy (`allow_intrabc`) | §7.11.x,§5.11.x | ☐ | M1 |

## E. AV1 — transforms (§7.13)

| Component | Spec | Status | M |
| --- | --- | --- | --- |
| inverse 4×4 Walsh-Hadamard (lossless) + matched forward | §7.13.2.10 | ✅ | M0 |
| inverse DCT 4/8/16/32/64 + forward DCT | §7.13.2.2/.3 | ✅ (4/8/16/32/64, used through TX_64X64) | M1 |
| inverse ADST4/8/16 (+FLIPADST) + forward | §7.13.2.4-.9 | ✅ (ADST 4/8/16 fwd+inv, emitted via TX_SET_INTRA_2; FLIPADST reconstruct path present, unused under `reduced_tx_set=1`) | M1 |
| identity transform 4/8/16/32 (IDTX / V_ / H_) | §7.13.2.11-.15 | ✅ (IDTX emitted via TX_SET_INTRA_2; V_/H_ axis variants present in dispatch, unused under `reduced_tx_set=1`) | M1 |
| 2D inverse transform + tx_type sets, `get_tx_set` | §7.13.3,§5.11.47/.48 | ✅ (normative `inverse_transform_2d`; intra `TX_SET_INTRA_2`/`TX_SET_DCTONLY`; inter sets OOS) | M1 |
| variable tx size / `txfm_split` | §5.11.15-.17 | ✅ (TX_MODE_SELECT, square `tx_depth` 0..2; rectangular `txfm_split` deferred) | M1 |
| encoder forward transform + tx-type/size RD search | (encoder) | ✅ (`forward_transform_2d` + heuristic tx-type/tx-size search; true RD deferred) | M1 |

## F. AV1 — quantization (§7.12)

| Component | Spec | Status | M |
| --- | --- | --- | --- |
| lossless dequant (q_idx 0) feeding WHT reconstruct | §7.12.2/.3 | ✅ | M0 |
| dc_q/ac_q lookup tables (8/10/12-bit) | §7.12.2 | ✅ (all three rows exercised: 8-bit, and 10/12-bit via `encode_still_intra16_with`) | M1/M2 |
| quantizer matrices (qm_y/u/v) | §9.5 | ☐ | M1 |
| encoder quantization (dead-zone, RDOQ) | (encoder) | ☐ | M1 |

## G. AV1 — entropy coding & tables (§8, §9)

| Component | Spec | Status | M |
| --- | --- | --- | --- |
| symbol/range encoder (inverse of §8.2 decoder) | §8.2 | ✅ | M0 |
| `encode_literal` (equiprobable `read_bool` inverse) | §8.2.3/.5 | ✅ | M0 |
| static default CDFs: Partition, Skip, IntraFrameYMode, UvMode(±CfL) | §9.4 | ✅ | M0 |
| coeff CDFs (qctx0, TX_4X4): TxbSkip/EobPt16/EobExtra/CoeffBaseEob/CoeffBase/CoeffBr/DcSign | §9.4 | ✅ | M0 |
| full default CDF tables: all qctx, tx classes, inter/MV/palette | §9.4 | ✅ (intra: coeff CDFs all used tx sizes × qctx 0–3, mode/partition/palette; inter/MV OOS) | M1/OOS |
| CDF adaptation + frame-end update + context_update_tile | §8.2.6,§7.7 | ✅ (`cdf::CdfContext`, per-tile; §7.7 n/a — see below) | M1 |
| `coeffs()` TX_4X4: txb_skip/eob/base/br/sign/dc_sign/golomb | §5.11.39 | ✅ | M0 |
| `coeffs()` all tx sizes + transform_type signaling | §5.11.39/.47 | ✅ (lossy 4×4 + 8×8 + 16×16 + 32×32 + 64×64, 32×32/64×64 DCT-only) | M1 |
| scan table `Default_Scan_4x4` + context-offset tables | §9.2/§9.3/§8.3.2 | ✅ | M0 |
| all scan tables (default/col/row per tx size) | §9.2 | ✅ (4×4 + 8×8 + 16×16 + 32×32 + 64×64 default) | M1 |

**§7.7 `frame_end_update_cdf` is not applicable to a still image.** `uncompressed_header()`
(§5.9.2) infers `disable_frame_end_update_cdf = 1` whenever `reduced_still_picture_header ||
disable_cdf_update`, and this encoder always sets `reduced_still_picture_header = 1`. So turning
`disable_cdf_update` off codes no additional header bit, `frame_end_update_cdf()` is never invoked
from the tile group (§5.11.1), and the §8.2.4 save at `context_update_tile_id` never fires — there
is no later frame that could `load_cdfs` the saved context. `context_update_tile_id` itself is
still coded in `tile_info()` for the multi-tile case (row B), as the syntax requires. Adaptation is
therefore per tile: each tile re-runs `init_non_coeff_cdfs`/`init_coeff_cdfs` and adapts its own
copy, which is what a decoder does for an independently decodable tile.

## H. AV1 — in-loop filters & post (§7.14-§7.18; all bypassed under CodedLossless)

| Component | Spec | Status | M |
| --- | --- | --- | --- |
| deblocking loop filter | §5.9.11,§7.14 | ✅ (lossy 4×4/8×8/16×16, narrow + wide + widest) | M1 |
| CDEF (constrained directional enhancement filter) | §5.9.19,§7.15 | ✅ (lossy; `Cdef_Uv_Dir` incl. the non-identity 4:2:2 row) | M1 |
| loop restoration: Wiener (luma) + stripe boundaries + per-SB unit signaling | §5.9.20,§7.17 | ✅ (Wiener luma; self-guided/chroma deferred) | M1 |
| superres horizontal upscaling (8-tap polyphase, LR after upscale) | §5.9.8,§7.16 | ✅ (opt-in via `encode_still_intra_superres`) | M1 |
| film grain synthesis | §5.9.30,§7.18.3 | ☐ | M4 |

## I. AV1 — inter coding (permanently out of scope: sequences-only machinery, per the charter)

| Component | Spec | Status | M |
| --- | --- | --- | --- |
| reference frame buffers, ref_frame_idx, order hint | §5.9,§7.20/.21 | OOS | OOS |
| MV prediction (find_mv_stack), MV/MVD coding | §7.10,§5.11.25-.34 | OOS | OOS |
| inter prediction: single/compound, OBMC, warped, wedge, masked | §7.11.3 | OOS | OOS |
| skip_mode, ref_frame_mvs, global motion, motion-field estimation | §5.9.22/.24,§7.9 | OOS | OOS |

## J. Color / CICP / HDR / metadata

| Component | Spec | Status | M |
| --- | --- | --- | --- |
| identity matrix (mc=0), full range, 4:4:4, planar G/B/R mapping | CICP H.273; §5.5.2 | ✅ | M0 |
| BT.601/709/2020-NCL matrices (mc=1/6/9), studio range, `color_config()` non-shortcut branch | CICP H.273 §8.3; AV1 §5.5.2 | ✅ (encode + RGBA decode; #335) | M2 |
| RGB↔YCbCr at 4:4:4 (`gamut_color::RgbToYcbcr` / `YcbcrMatrix`) | (gamut-color) | ✅ | M2 |
| chroma down/up-sample (4:2:0 / 4:2:2 plane geometry + box downsample) | (gamut-av1) | ✅ | M2 |
| transfer sRGB/BT.709 (tagged only in M0) | CICP H.273 | ✅ (tag) | M0 |
| transfer PQ (SMPTE ST 2084) / HLG (BT.2100) | CICP H.273 | ✅ (tag only, `AvifEncoder::with_transfer`; no HDR pipeline — see `mdcv`/`clli` below) | M4 |
| primaries variants; embedded ICC profile | CICP; 23008-12 | ✅ (`AvifEncoder::with_primaries`; ICC via `with_icc_profile`) | M4 |
| HDR mastering display (`mdcv`) + content light level (`clli`) | §5.8.3/.4 | ☐ | M4 |

## K. Cross-crate API, I/O & tooling

| Component | Spec | Status | M |
| --- | --- | --- | --- |
| `gamut_core::EncodeImage<Rgb8>` impl (typed input) | gamut-core | ✅ | M0 |
| `AvifEncoder::{new, lossless, lossy, config}` builder API | gamut-avif | ✅ | M0/M1 |
| `AvifEncoder::{with_matrix, with_color_range}` colour selection | gamut-avif | ✅ (#335) | M2 |
| `Rgba8` input + alpha-plane extraction; `Gray8` input | gamut-color/avif | ✅ (#397; `Planar8::from_rgba8_*_view`/`from_gray8_view`) | M3 |
| `Rgb16`/`Rgba16` input → 10/12-bit AVIF (`with_bit_depth`) | gamut-color/avif | ✅ (#399; `Planar16::from_rgb16_*_view`/`from_rgba16_*_view`) | M2 |
| Subsampled chroma on the `Rgba8`/`Rgba16` paths (`with_chroma` is honoured for `Rgb8`/`Rgb16`, but an RGBA colour item is always 4:4:4 — neither `Planar8` nor `Planar16` has a 4-stride downsampler) | — | ☐ | M3 |
| Subsampled chroma at 10/12-bit (`with_chroma` reaches the `Rgb16` path; `Planar16::from_rgb16_matrix_subsampled` shares `Planar8`'s box filter, so the two cannot drift) | — | ✅ (#399) | M3 |
| 10/12/16-bit & float HDR input buffers | gamut-color | ✅ (`Planar16`; 16-bit input narrows to a 10/12-bit coded depth — float HDR ☐) | M2/M4 |
| quality config (`lossy(quality)`, 0..=100 → `base_q_idx`); speed / rate control | gamut-avif/av1 | ✅ (quality; speed + rate control deferred) | M1 |
| AVIF container decode + codestream handoff (`AvifContainer`/`AvifImage`/`Av1StillDecoder`) | gamut-avif §L | ✅ | D |
| AV1 **encode** backend seam (`Av1StillEncoder`/`Av1EncodeRequest`/`push_backend`/`AbiAv1StillEncoder`) | gamut-avif §M | ✅ | D |
| AV1 **decode** backend registry (`push_decode_backend`/`AbiAv1StillDecoder` around `Av1StillDecoder`) | gamut-avif §M | ☐ | D |
| `gamut_core::Decoder` (AVIF → pixels with **no** external decoder; needs the pure-Rust AV1 decoder — `libaom`'s reference encoder is staged as its oracle) | gamut-avif | ☐ | D |
| CLI / wasm / ffi wiring for AVIF | gamut-{cli,wasm,ffi} | ☐ | D |

## L. AVIF decode surface (issue #250)

The container read + AV1 codestream handoff, mirroring the surface `gamut-heic` established for
HEIF (#238): the container and everything around the coded picture are decoded in pure Rust; the
AV1 codestream itself is supplied by the caller through the `Av1StillDecoder` seam (a platform
hardware decoder, dav1d, …) — the split downstream `rawshift` consumes. Delivered in four slices:
**S1** byte-accounting container + role-typed view (`container.rs`/`image.rs`), **S2** typed
`av1C` + OBU layer (`av1c.rs`/`obu.rs`), **S3** the decoder seam + derivation/colour/transform
pipeline (`decode.rs`), **S4** the libavif/dav1d differential oracle (`tests/conformance.rs`).

| Component | Spec | Status | Slice |
| --- | --- | --- | --- |
| Byte-exact segment accounting (every byte a box / appended stream / trailer) | 14496-12 | ✅ | S1 |
| `meta`/`iprp` unknown-box surfacing (shadow walk) | 14496-12 | ✅ | S1 |
| Role-typed view: primary validation, brands (`avif`/`avio` still; `avis` OOS), item kinds | AVIF §8.3; 23008-12 | ✅ | S1 |
| Relationship lenses: thumbnails, aux (alpha/depth URNs), `prem`, `cdsc` Exif/XMP, `dimg` | AVIF §4; 23008-12 §6 | ✅ | S1 |
| Typed property accessors + `ipma`-ordered transforms + MIAF order check | 23008-12 §7; MIAF | ✅ | S1 |
| `av1C` typed parse (`Av1Config`), reserved-bit tolerant, `configOBUs` size-field rule | AV1-ISOBMFF §2.3.3/.4 | ✅ | S2 |
| OBU split (`iter_obus`): low-overhead syntax, LEB128 bounds, size-field framing | AV1 §5.3, §4.10.5; AV1-ISOBMFF §2.4 | ✅ | S2 |
| Still-payload validation (one seq header, sync-RAP key frame, no tile list) | AVIF §2.1; AV1-ISOBMFF §2.4 | ✅ | S2 |
| `full_stream` bridge (TD + `configOBUs` + payload, size fields normalized) | AV1-ISOBMFF §2.4 | ✅ | S2 |
| `Av1StillDecoder` seam + validating `DecodedFrame` contract | (crate API) | ✅ | S3 |
| Planar pipeline: coded / `iden` / `grid` assembly (uniform tiles, checked canvas, crop) | 23008-12 §6.6.2.3.2 | ✅ | S3 |
| Derivation cycle + depth guards | (hardening) | ✅ | S3 |
| RGBA path: identity / BT.601 (mc 2/5/6) / BT.709 (1) / BT.2020 NCL (9) / monochrome; missing-`colr` default | H.273; AVIF §2.2 | ✅ | S3/S6 |
| High-bit-depth surface `decode_item_rgba16`/`decode_primary_rgba16` (8..=16-bit in, samples normalized to the full 16-bit range) | H.273 | ✅ | S6 |
| Alpha merge (luma-plane, non-mono accepted, bit-depth rescale) | AVIF §4.1 | ✅ | S3 |
| `clap`/`irot`/`imir` application in `ipma` order (2022 `imir` axis semantics) | 23008-12:2022 §6.5.12; 14496-12 §12.1.4 | ✅ | S3 |
| `iovl` overlay compositing (source-over, canvas fill, clipping) | 23008-12 §6.6.2.3.3 | ✅ | S3 |
| libavif structure/metadata/pixels + dav1d planar bit-exact differential suite | (oracle) | ✅ | S4 |
| C2PA manifest-store locator (`AvifContainer::c2pa` / `c2pa_manifest_stores`): every top-level `uuid` box with the C2PA user type → `box_purpose`, slot bytes, file range, in file order | C2PA 2.4 §A.5.1–§A.5.3; §18.6 | ✅ (#444) | — |

**C2PA — four recorded limits (#444).** (1) *A slot, not a trimmed store.* `C2paSlot::slot_bytes`
/ `range` run from just after the merkle offset to the end of the `uuid` box, so they include any
unused padding §A.5.3 permits after the store, and a reserved-but-unfilled slot reads back as
zeros. `gamut-heic`'s `C2paManifestStore` (#429) instead trims to the store's own JUMBF `LBox`, so
the two crates can report different lengths for one file; the AVIF type is *named* for the slot so
the two claims cannot be confused across the umbrella's re-exports. The bound is deliberate — an
`LBox` trim cannot locate a reservation, which has no `LBox` — and unifying the two lenses is
**#505**. (2) *`update` framing is probed, not stated.* §A.5.3 gives the merkle offset only for
`manifest`/`original`; `update` is probed over the same `[8, 0]` candidates `gamut-heic` uses, so a
prefix-less `update` store shorter than 8 bytes is located rather than dropped. A *longer*
prefix-less one is still reported 8 bytes short — that needs the `LBox` check, also #505. (3) *The
range is not an exclusion range.* BMFF assets bind with `c2pa.hash.bmff.v3`, which excludes by box
path (§18.6, §A.5.6); the range is for patching and byte accounting, and no type is named
"exclusion". (4) *The encoder writes `manifest` only.* The read side reports all three purposes;
producing an `original`/`update` pair is a manifest-update operation on an existing file, outside
#444. The reserve → sign → validate direction against `c2pa-rs` is #447's.

**Deferred (additive) for the decode surface:** the backend registry + `gamut-codec-abi` adapter
around `Av1StillDecoder` — section M reserves its name and shape; the typed trait itself already
ships, and #274 has since delivered the mirror-image *encode* registry it will copy; the pure-Rust
AV1 codestream decoder
(own issue; would make the seam optional and enable `gamut_core::Decoder`); ICC application on the
RGBA path, plus the matrix coefficients outside the modeled Kr/Kb set — YCgCo (8) and the
chromaticity-derived points (12/13/14), which the 10-bit corpus file uses — and coded depths CICP
does not model (9/11/13…), all explicitly refused rather than approximated (planar delivers them
today); `tmap`/`sato` derived-item decode;
wiring decoded Exif/XMP payloads through `gamut-exif`/`gamut-xmp`; a shared byte-accounting
segment walker (an isobmff 2.0 candidate — today's walker deliberately mirrors `gamut-heic`'s);
and unifying the per-crate `DecodedFrame` types through `gamut-codec-abi`.

**The S1 guarantee.** Parsing maps every input byte to exactly one segment — contiguous,
non-overlapping, covering `0..len` — so it is structurally impossible for the container layer to
silently ignore bits.

## M. Codestream backend seams (issues #241 / #272 / #274)

The workspace-wide inversion of control over the coded picture: `gamut-avif` owns the container,
while the AV1 codestream may be produced or consumed by an alternate backend (a platform/hardware
codec, libaom, SVT-AV1, dav1d, …). The shape and the fallback contract come from `gamut-codec-abi`
(#272) and are identical in both directions — a **typed trait per format**, a registry tried in
**push order**, `gamut-avif`'s own software path as the **implicit tail**, `supports()` / C
`Status::UNSUPPORTED` as the **only** fall-through signal, and an accepted-then-failed job
propagating its error rather than being silently re-encoded.

| Component | Spec | Status | Issue |
| --- | --- | --- | --- |
| Typed encode trait `Av1StillEncoder` (`supports` + `encode_still` → AV1 OBUs) | (crate API) | ✅ | #274 |
| `Av1EncodeRequest`: `#[non_exhaustive]`, private fields + getters, carries the derived `base_q_idx` | (crate API) | ✅ | #274 |
| `AvifEncoder::push_backend` registry (`Arc<Mutex<…>>`; `Clone` **shares** backends) | (crate API) | ✅ | #274 |
| Fallback contract: push order, decline-only fall-through, `gamut-av1` tail, error propagation | #241 | ✅ | #274 |
| `AbiAv1StillEncoder` adapter over `gamut_codec_abi::Encoder` (codec id `av01`, `base_q_idx` in `extra`) | #272 | ✅ (8, 10 and 12-bit — `encode_still16` lowers `Planar16` as native-endian `u16` planes with the coded depth in `ImageDesc::depth` and **byte** strides) | #274 |
| `av1C`/`colr` re-derived from a backend stream's sequence header (§2.3.4 consistency) | AV1-ISOBMFF §2.3.4 | ✅ | #274 |
| Byte-identical default output with no pushed backend (the 1.0 additivity guarantee) | (crate API) | ✅ | #274 |
| Typed decode trait `Av1StillDecoder` (`decode_still` → `DecodedFrame`) | (crate API) | ✅ | #250 |
| Decode registry `AvifContainer::push_decode_backend` + `AbiAv1StillDecoder` adapter | #241 | ☐ | D |
| Backend selection beyond first-supporter (cost/priority hints, per-request negotiation) | — | ☐ | D |
| Colour on `Av1EncodeRequest` (`colour()`), validated against the returned stream's `color_config()` | AV1 §5.5.2 | ✅ (#335) | M2 |
| Chroma on `Av1EncodeRequest` (`chroma()`), validated against the returned stream's `seq_profile` | AV1-ISOBMFF §2.3.4 | ✅ | M2 |
| Bit depth on `Av1EncodeRequest` (`bit_depth()`), validated the same way, + the defaulted `Av1StillEncoder::encode_still16` (whose default **declines**, so an 8-bit backend falls through rather than being handed 10/12-bit planes) | — | ✅ (#399) | M2 |
| Monochrome on `Av1EncodeRequest` (so a backend can encode, or decline, an alpha auxiliary or a `Gray8` primary — today those go straight to the `gamut-av1` tail, since a backend written against the three-plane contract cannot decline what the request cannot express) | — | ☐ | M3 |

**Reserved: the decode-side registry.** The `Av1StillDecoder` trait ships today as a *single*
caller-supplied decoder passed per call (`decode_primary_rgba8(&mut decoder)`). Its registry
counterpart is deferred and will mirror section M's encode side exactly, so the names are reserved
here rather than shipped as unused surface on a 1.0 crate:

```text
impl AvifContainer/AvifImage {
    pub fn push_decode_backend(&mut self, backend: impl Av1StillDecoder + 'static) -> &mut Self;
}
pub struct AbiAv1StillDecoder<D: gamut_codec_abi::Decoder + Send>;  // StreamConfig(av01) + ImageDesc
```

with `Av1StillDecoder` gaining a **defaulted** `supports(&mut self, config: &Av1Config) -> bool`
(defaulting to `true`, so every existing implementation keeps compiling) as the fall-through
signal. The existing per-call `decode_*` entry points stay, so this lands semver-minor. It is
blocked on nothing but sequencing; the pure-Rust AV1 decoder, when it arrives, becomes the
implicit tail exactly as `gamut-av1` is on the encode side.

## N. Pure-Rust AV1 codestream decoder (issue #259)

The last decode-side gap. Section L decodes the container and hands the AV1 codestream to a
caller-supplied `Av1StillDecoder`; this section is the **software implementation of that trait** —
what makes the seam optional and unblocks `gamut_core::DecodeImage` for AVIF (section K) on
targets with no hardware AV1 decode.

It lives in `gamut-av1` behind a default-on `decode` feature, not in a new crate: the decoder
needs `cdf.rs` (the §9.4 tables), `filter.rs`, `transform.rs` and `quant.rs`, which are private to
that crate, and roughly half the normative decode-side maths is already written there to serve the
encoder's reconstruction buffer. `default-features = false` yields the encoder-only crate exactly
as it was.

Delivered in slices, one PR each, the way section L delivered #250:
**D1** the bit reader + symbol decoder (`gamut-bitstream`), **D2** the shared intra predictors,
**D3** OBU / sequence header / frame header / tile-group framing (`decode/{obu,header,tilegroup}.rs`),
**D4** tile parsing (partition, mode info, coefficients), **D5** reconstruction and the in-loop
filters, **D6** the pixel-format matrix, **D7** the `gamut-avif` wiring.

A decoder cannot pick a subset the way the encoder does — it must accept whatever a conformant
encoder produced — so a ☐ row here is **refused with a typed `Error::Unsupported` naming the
tool**, never approximated and never silently mis-decoded.

Oracle: libaom's reference **encoder** (`aom_oracle::encode_still_intra`), staged for this purpose
in [`references/av1`](../../references/av1/README.md), with `aom_oracle::decode_av1` and
`dav1d_oracle::decode_obu` as the authorities on what each stream means. Streams from libaom
exercise tools `gamut-av1` never emits, which its own round-trip suite cannot reach.

| Component | Spec | Status | Slice |
| --- | --- | --- | --- |
| `BitReader`: `f(n)`, `su(n)`, `ns(n)`, `uvlc()`, `le(n)`, `leb128()`, `trailing_bits()` | §4.10, §5.3.4/.5 | ✅ | D1 |
| `SymbolDecoder`: `init_symbol`/`read_symbol`/§8.2.6 adaptation/`read_literal`/`exit_symbol` | §8.2 | ✅ | D1 |
| OBU walk: header, extension header, LEB128 size, operating-point drop rule | §5.3 | ✅ | D3 |
| Sequence header: reduced **and** general form, operating points, `color_config()` | §5.5.1/.2 | ✅ | D3 |
| Frame header: frame/render/superres size, `CodedLossless`, `AllLossless` | §5.9.2/.5/.6/.8/.9 | ✅ | D3 |
| `tile_info()`: uniform **and** explicit spacing, tile rows and columns, `TileSizeBytes` | §5.9.15/.16 | ✅ | D3 |
| `quantization_params()` incl. `using_qmatrix`; `segmentation_params()`; delta-q / delta-lf | §5.9.12/.13/.14/.17/.18 | ✅ | D3 |
| `loop_filter_params()` / `cdef_params()` / `lr_params()` / `read_tx_mode()` | §5.9.11/.19/.20/.21 | ✅ | D3 |
| `film_grain_params()` parse (synthesis itself deferred) | §5.9.30 | ✅ (parse) | D3 |
| Frame OBU + tile group framing, tile-size prefixes | §5.10, §5.11.1 | ✅ | D3 |
| `DecodeLimits`: dimension / sample-count / tile-count caps applied before allocation | (hardening) | ✅ | D3 |
| `Av1Decoder::inspect` → `StreamInfo` (headers without decoding samples) | (crate API) | ✅ | D3 |
| Shared intra predictors (lifted from `tile.rs`), edge filter + upsample | §7.11.2 | ☐ | D2 |
| `decode_tile` / `decode_partition` / `decode_block`, full partition set, 128×128 SB | §5.11.2-.4 | ☐ | D4 |
| Mode info: intra Y/UV modes, `angle_delta`, filter-intra, CfL, palette | §5.11.5-.24, §5.11.42-.50 | ☐ | D4 |
| `coeffs()`: all tx sizes and classes, `transform_type`, `read_tx_size` | §5.11.15-.17, §5.11.39/.47/.48 | ☐ | D4 |
| Reconstruction: dequant → inverse transform → predict → add | §7.11-§7.13 | ☐ | D5 |
| In-loop filters driven by the parsed header (deblock, CDEF, LR incl. self-guided + chroma, superres) | §7.14-§7.17 | ☐ | D5 |
| 10/12-bit, 4:2:0/4:2:2, monochrome, profiles 0 and 2 | §5.5.2, Annex A | ☐ | D6 |
| Intra block copy (`allow_intrabc`) | §7.11.x | ☐ | D6 |
| Film grain synthesis | §7.18.3 | ☐ | D6 |
| `SoftwareAv1Decoder` + `AvifDecoder` (`DecodeImage` for Rgb8/Rgba8/Rgb16/Rgba16) | gamut-avif | ☐ | D7 |
| libaom differential suite over the size / quantizer / content / tools matrix | (oracle) | ✅ (headers) | D3 |

**Refused, by design.** Inter frames, `show_existing_frame`, `OBU_TILE_LIST`, a partial tile
group, and decoder-model info are refused where they are read: each would need reference-frame or
sequence machinery the charter puts permanently out of scope, and parsing past them would desync
the bitstream rather than fail cleanly.

## The v1 guarantee

`gamut-avif` 1.0 promises: an encoder with **no pushed backend** emits exactly the bytes it always
has (pinned byte-for-byte by `tests/backend.rs` against goldens captured before the seam existed);
every emitted file is a conformant MIAF/AVIF still image (brands `avif`/`mif1`/`miaf`, plus
`MA1A` when every image item is AV1 High Profile — a monochrome item is Main, so an alpha or
`Gray8` file signals only the general brands, per AVIF §8.1/§8.3; the AVIF §9.1.1 minimum box set;
cross-box consistency between `av1C`, the AV1 sequence header, `pixi`, `colr`, and `ispe` by
construction); lossless mode
round-trips bit-exact through a conformant decoder; the `quality → base_q_idx` mapping is frozen
(defined in [`references/avif`](../../references/avif/README.md), including the silent clamp of
`quality > 100`); and the output is continuously validated against `libavif`+`dav1d` at the
container level and `libaom`+`dav1d` at the bitstream level. Every deferred row lands additively —
the v1 public surface is never reshaped.
