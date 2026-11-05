# gamut-av1

`gamut-av1` is a pure-Rust AV1 still-image codec. AVIF relies on AV1 intra-frame coding, so this
crate is usable standalone as well as through [`gamut-avif`](../gamut-avif). The encoder is
complete for its documented scope; the **decoder** is being built out slice by slice under
[issue #259](https://github.com/visualcommons/gamut/issues/259).

If you want a complete `.avif` file, use [`gamut-avif`](../gamut-avif). Reach for this crate when you
need the **raw AV1 still bitstream** itself — to embed in your own container or build another
AV1-based format. It operates on `Planar8` / `Planar16` planes and emits an AV1 temporal unit, not
a container.

## Goals

Part of the [gamut](../../README.md) workspace, this crate exists to provide AV1 **still-image
encoding** that is:

- **Memory-safe on hostile input.** `#![forbid(unsafe_code)]` across the whole encode path, deleting
  the memory-corruption bug class that has repeatedly bitten the C AV1 codecs.
- **Clean-slate from the spec.** Implemented directly from the AV1 Bitstream & Decoding Process
  Specification ([`../../references/av1/`](../../references/av1)) rather than wrapping libaom, so it
  is auditable and forkable. Modules mirror the spec: `headers` = OBU framing + sequence/frame
  headers (§5.3/§5.5/§5.9), `tile` = partition/prediction/coefficient coding (§5.11), `cdf` =
  default CDF + scan + context tables (§9.2/§9.4/§8.3.2).
- **Verified against the AV1 reference codec.** Output is checked bit-exact against real decoders —
  `libaom` (the AOMedia reference codec, the definitive oracle) and `dav1d` — linked from vendored
  `third_party/` submodules, never from system-installed binaries. See
  [`references/av1`](../../references/av1/README.md).
- **Buildable anywhere `cargo` is.** No C, no nasm — cross-compiles cleanly (wasm32, aarch64, musl).

## Usage

```rust,no_run
use gamut_av1::encode_still_lossless_identity;
use gamut_color::Planar8;

// 8-bit interleaved RGB -> identity 4:4:4 planes -> lossless AV1 keyframe.
let (width, height) = (64u32, 64u32);
let rgb = vec![0u8; (width * height * 3) as usize];
let planes = Planar8::from_rgb8_identity(&rgb, width, height).expect("valid input");

let still = encode_still_lossless_identity(&planes).expect("encode");
// `still.obus` is the AV1 temporal unit; `still.config` carries the sequence-header
// values that `gamut-avif` mirrors into the `av1C`/`colr` boxes.
std::fs::write("out.obu", &still.obus).unwrap();
```

## Status

The encoder codes **lossless and lossy all-intra keyframes**, producing the AV1 temporal unit that
`gamut-avif` wraps in an AVIF still image:

- **Lossless** (`encode_still_lossless_identity`): identity matrix, full range, `DC_PRED` and the
  forced `TX_4X4` Walsh–Hadamard transform.
- **Lossy** (`encode_still_intra*`): the eight directional modes with `angle_delta`,
  `SMOOTH`/`SMOOTH_V`/`SMOOTH_H`/`PAETH`, recursive filter-intra, chroma-from-luma and palette;
  DCT/ADST/identity transforms with variable `tx_depth` and `TX_SET_INTRA_2` type selection;
  recursive partitioning; uniform multi-tile; and the in-loop filters (deblocking, CDEF, loop
  restoration, superres). Every block is coded at the frame-level quantizer and loop-filter level.
- **Adapting CDFs** (`disable_cdf_update = 0`, AV1 §8.2.6): each tile starts from the §9.4
  defaults and nudges every context toward what it codes.
- **Colour signalling**: `encode_still_intra_with` takes an `Av1Colour` (the CICP
  primaries/transfer/matrix triple plus the signal range) and mirrors it into the sequence
  header's `color_config()` and the `av1C`/`colr` values `gamut-avif` stamps.
- **Chroma sampling** comes from the buffer: 4:4:4, 4:2:2 and 4:2:0 are all coded, and
  **monochrome** (`mono_chrome = 1`) from a `ChromaSubsampling::Cs400` buffer — use
  `Av1Colour::monochrome()`, since §6.4.2 permits `MC_IDENTITY` only at 4:4:4. The identity matrix
  is refused below 4:4:4.
- **Bit depth** comes from the buffer too: `Planar8` codes 8-bit, and `encode_still_intra16_with`
  codes a `gamut_color::Planar16` at 10 or 12 bits. `BitDepth::Sixteen` is not an AV1 depth and is
  rejected; superres has no high-bit-depth entry point.
- **`seq_profile`** is a joint function of the two (Annex A.2): Main (0) for 4:2:0 and monochrome,
  High (1) for 4:4:4, and Professional (2) for 4:2:2 or any layout at 12 bits.
- **Encode speed** is a `Speed` preset (`encode_still_intra_with_speed`): each step prunes the
  lossy mode-decision search; the default is `Speed::Balanced`.
- **Tiling and threads** are `EncodeOptions` (`encode_still_intra_with_options`): a `Tiling` picks
  the uniform tile layout, and `threads` caps how many tiles are coded concurrently. The stream
  depends on the layout only, never on the thread count.

```rust,ignore
let planes = Planar16::from_planes(w, h, BitDepth::Twelve, [y, u, v])?;
let (still, recon) = encode_still_intra16_with(&planes, qindex, Av1Colour::default())?;
```

The remaining surface (quantizer matrices and the AVIF-level alpha/metadata/container features)
is tracked row by row in [`gamut-avif/STATUS.md`](../gamut-avif/STATUS.md), whose section N is the
decoder's ledger. The crate docs carry the per-tool detail.

## Decoding

Behind the default-on `decode` feature. Today it reads the **framing and header layer** of an AV1
still — the OBU walk (§5.3), the full sequence header (§5.5) in both its reduced and general
forms, the full uncompressed frame header (§5.9), and tile-group framing (§5.11.1):

```rust
use gamut_av1::Av1Decoder;

# fn main() -> Result<(), gamut_core::Error> {
# let temporal_unit: &[u8] = &[];
# if !temporal_unit.is_empty() {
let info = Av1Decoder::new().inspect(temporal_unit)?;
println!(
    "{}x{} at {}-bit",
    info.frame.upscaled_width, info.frame.frame_height, info.sequence.color.bit_depth,
);
# }
# Ok(())
# }
```

Sample decoding — the tile body, reconstruction and the in-loop filters — is **not implemented
yet**, and no entry point pretends otherwise. Streams are accepted only at `seq_profile = 1`
(8-bit 4:4:4) with intra key frames; every other tool is refused with a typed
`Error::Unsupported` naming it, never approximated. `default-features = false` drops the decoder
entirely and leaves the encoder-only crate unchanged.

It is checked against **libaom**, the AV1 reference codec, in the direction that matters for a
decoder: libaom *encodes* the stills and its own decoder says what they mean, so the suite
exercises tools this crate's encoder never emits.

## Roadmap

- #259: the pure-Rust decoder — headers ✅, then tile parsing, reconstruction, the in-loop
  filters, and the 10/12-bit + 4:2:0/4:2:2/monochrome pixel-format matrix.

Inter coding is out of scope: gamut is image-first, so animated AVIF is not planned.

## License

MIT or Apache-2.0, at your option — see the [workspace README](../../README.md#license).
