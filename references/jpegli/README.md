# jpegli — distance quality control and quantization tables

jpegli is the libjpeg-API JPEG encoder/decoder of the libjxl project. It has no specification of
its own; its source is the reference for every jpegli behaviour `gamut-jpeg` reproduces.

`gamut-jpeg`'s opt-in distance control (`JpegEncoder::with_distance`, issue #1243) transcribes
jpegli's quantization-table construction from these files. The tables are a free encoder choice
under T.81 (Annex K's tables are only examples). This directory records where the constants come
from.

## Source and pin

- **libjxl v0.11.2**, the last release that ships jpegli in-tree (0.12 moved it to the separate
  `google/jpegli` repository). It is the same tree `tooling/jpegli-oracle` compiles, from the
  `jpegxl-src` **0.11.4** crates.io package (`libjxl/` inside the package). Upstream:
  `https://github.com/libjxl/libjxl/tree/v0.11.2/lib/jpegli`.
- **Licence:** BSD-3-Clause, `LICENSE` here (libjxl's own, verbatim; SHA-256
  `8405932022a556380c2d8c272eff154a923feb197233f348ce5f7334fb0a5ede`).

| File | What it holds | SHA-256 |
| --- | --- | --- |
| `quant.cc` (vendored verbatim, 768 lines) | `kBaseQuantMatrixXYB`, `kBaseQuantMatrixYCbCr`, `kBaseQuantMatrixStd`, `kGlobalScaleXYB`, `kGlobalScaleYCbCr`, `k420GlobalScale`, `k420Rescale`, `kExponent`, `kDist0`, `DistanceToScale`, `DistanceToLinearQuality`, `SetQuantMatrices` | `7165b08fdeb1798eaadb2681d652ecdeecc4ef182e06144905a40a33c47f808d` |
| `lib/jpegli/encode.cc` (not vendored; excerpts below) | `jpegli_set_distance`, `jpegli_quality_to_distance`, the component-to-table assignment in `jpegli_set_colorspace`, and the defaults `force_baseline = true`, `cicp_transfer_function = 2` | `1d53b6b1a49e63ce0dccb57035ccf4e4b4dc895887f0aa8e11453368b7df88fb` (1274 lines) |

## The parts of `encode.cc` gamut-jpeg relies on

The quality-to-distance mapping, verbatim:

```cpp
float jpegli_quality_to_distance(int quality) {
  return (quality >= 100  ? 0.01f
          : quality >= 30 ? 0.1f + (100 - quality) * 0.09f
                          : 53.0f / 3000.0f * quality * quality -
                                23.0f / 20.0f * quality + 25.0f);
}
```

The distance setter, verbatim. Its one difference from `jpegli_set_quality` is
`add_two_chroma_tables = true`, which gives Cr a third table (destination 2) built from the third
YCbCr base matrix. `cjpegli` (`lib/extras/enc/jpegli.cc`, `EncodeJpeg`) always calls it with
`force_baseline = TRUE`, both for `--distance` and for `-q` after mapping the quality through
`jpegli_quality_to_distance`:

```cpp
void jpegli_set_distance(j_compress_ptr cinfo, float distance,
                         boolean force_baseline) {
  CheckState(cinfo, jpegli::kEncStart);
  cinfo->master->force_baseline = FROM_JXL_BOOL(force_baseline);
  float distances[NUM_QUANT_TBLS] = {distance, distance, distance};
  jpegli::SetQuantMatrices(cinfo, distances, /*add_two_chroma_tables=*/true);
}
```

The component-to-table assignment (`jpegli_set_colorspace`): every component starts on table 0.
YCbCr puts Cb and Cr on table 1, and `SetQuantMatrices` then moves Cr to table 2. XYB (`JCS_RGB`
under `jpegli_set_xyb_mode`) puts X on 0, Y on 1 and B on 2. A grayscale frame is not YCbCr, so
`SetQuantMatrices` takes its third branch: the Annex K luminance table (`kBaseQuantMatrixStd`)
scaled by `0.01 · DistanceToLinearQuality(distance)`.

## How gamut-jpeg reproduces the arithmetic

`SetQuantMatrices` computes in `float` (IEEE binary32). Each entry is
`max(1, min(round(scale · base[k]), 255))`. For YCbCr and XYB,
`scale = global_scale · DistanceToScale(distance, k)`, times `k420Rescale[k]` for both chroma
tables of a 4:2:0 frame. A 4:2:0 YCbCr frame also multiplies `global_scale` by `k420GlobalScale`.
`DistanceToScale` evaluates `std::pow(kDist0, 1.0 - exp)` in `double` and narrows it to `float`,
and `std::pow(distance, exp)` in `float`. The literals without an `f` suffix
(`kBaseQuantMatrixYCbCr`, `k420Rescale`, `k420GlobalScale`) are `double` constants narrowed to
`float`. `gamut-jpeg` writes them as `f64` and narrows them the same way, so no literal is rounded
differently.

`tooling/jpegli-oracle` checks the result: the tables `jpegli_set_distance` stores in the
compressor equal the DQT `gamut-jpeg` writes, for every colour mode and sampling layout over a
sweep of distances.
