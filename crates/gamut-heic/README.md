# gamut-heic

`gamut-heic` is a pure-Rust HEIC still-image **decoder** — HEVC (H.265) intra image items wrapped
in a HEIF container — built as the HEVC binding over [`gamut-heif`](../gamut-heif), the HEIF/MIAF
layer it shares with AVIF. gamut is **decode-only** for this format: this crate parses HEIC and
decodes it through caller-supplied HEVC decoders (`HevcCodestreamDecoder` backends of a
`HeifDecoder`); it does **not** encode it (see [`references/heif`](../../references/heif/README.md)).

## Goals

Part of the [gamut](../../README.md) workspace, this crate provides HEIC **decoding** that is:

- **Total — no byte left behind.** Issue #238's core requirement: it is *structurally impossible to
  ignore any bits* in the container. Every byte of the input maps to a parsed segment — a top-level
  box (unknown ones surfaced verbatim, never dropped), an appended foreign stream (a Samsung
  motion-photo MP4), or an explicit trailer — so real-world phone HEICs are represented truthfully
  while their vendor-specific *semantics* stay downstream.
- **Memory-safe on hostile input.** `#![forbid(unsafe_code)]`, deleting the memory-corruption bug
  class that has bitten the C HEVC/HEIF stacks. Every offset-driven read is bounds-checked (via the
  shared [`gamut-isobmff`](../gamut-isobmff) box walker) and typed errors replace panics.
- **Clean-slate from the spec.** Implemented directly from the HEIF/MIAF/14496-15 specs (see
  [`references/heif`](../../references/heif/README.md)) rather than wrapping libheif/libde265.
- **Sharing the AVIF layer.** The item model, the `grid`/`iden`/`iovl` derivations, the decode
  limits, the RGBA presentation and the C2PA summary are [`gamut-heif`](../gamut-heif)'s, over the
  [`gamut-isobmff`](../gamut-isobmff) box tree that backs AVIF too; this crate adds the HEVC side
  (`hvcC`, the NAL layer, the still-image constraint) and the byte-accounting guarantee.
- **Pluggable codestream decoder.** Per issue #238, the HEVC-intra pixel decode sits behind
  `HevcCodestreamDecoder` backends held in the shared `gamut-codec-abi` registry, and this crate
  ships no software HEVC decoder of its own; `AbiHevcDecoder` brings in a C backend.

Note: HEVC is patent-encumbered, unlike gamut's royalty-free focus formats; this crate is
decode-side scaffolding and may move or be dropped as the project's scope sharpens (see the
workspace README's "Scope").

## Layers

- **`HeifContainer`** — the total, byte-accounting representation. `HeifContainer::parse(&[u8])`
  walks the top-level boxes into a contiguous, gap-free `segments` list covering `0..len` exactly,
  and shadow-walks `meta`/`iprp` for boxes the semantic layer does not consume (`unknown_meta_boxes`).
  `c2pa()` / `c2pa_manifest_stores()` are lenses over that walk: they locate the C2PA manifest store
  in a top-level `uuid` `ContentProvenanceBox` and hand back its opaque bytes and exact byte range.
  `is_hevc_still()` and `hevc_config(item_id)` are the HEVC questions about the file.
- **`HeifImage` / `HeifItem`** — `gamut-heif`'s role-typed semantic view over the primary
  still-image stream, re-exported: the validated primary item, item kinds and typed properties,
  and relationship lenses (thumbnails, alpha/depth auxiliaries, Exif/XMP metadata, derivations).
- **`HeifDecoder`** — decodes the primary image through `DecodeImage` to `Rgba8`/`Rgba16`, or to
  its planar samples, through the `HevcCodestreamDecoder` backends added with `with_backend`. Any
  other item decodes through `gamut-heif`'s surfaces with the `Hevc` binding.

## Usage

```rust
use gamut_heic::HeifContainer;

# fn demo(bytes: &[u8]) -> gamut_core::Result<()> {
let container = HeifContainer::parse(bytes)?;
let image = container.image();
if container.is_hevc_still() {
    let primary = image.primary_item();
    let dims = primary.dimensions();
    let alpha = image.alpha_auxiliary_of(primary.id());
#   let _ = (dims, alpha);
}
// Byte accounting: nothing is dropped.
if let Some(appended) = container.appended_stream() { /* opaque motion-photo MP4 */ }
// C2PA provenance: located, never validated.
if let Some(store) = container.c2pa() { /* store.bytes, store.range, store.purpose */ }
# Ok(())
# }
```

Reachable through the umbrella crate's `heic` feature.

### C2PA

`c2pa()` returns the first C2PA manifest store found, `c2pa_manifest_stores()` every one of them in
file order (a file mid-update carries both an `original` and an `update` box, C2PA 2.4 §A.5.3). The
reported range covers the store *exactly* — box header, extended type, `FullBox` version/flags,
`box_purpose` string, the 8-byte merkle offset, and any trailing padding are all excluded, the store
being bounded by its own outer JUMBF `LBox`. It is a **locator**, not a validator: nothing inside the
store is parsed, no hash is computed, no signature is checked, and no verdict about the file's
provenance is reached. The range is for observability and byte accounting — it is not a BMFF
exclusion range, since `c2pa.hash.bmff.v3` excludes by box path rather than by byte offset.

The probe that bounds an `update` store, the content-dependence of that bound, and the scan's
reach (the primary stream's top-level boxes only) are recorded with the slice that delivered the
locator, in [`STATUS.md`](STATUS.md) (S7).

## Conformance

Correctness is verified differentially against **libheif + libde265** — the de-facto ISO/IEC
23008-12 reference reader — via the dev-only [`tooling/libheif-oracle`](../../tooling/libheif-oracle)
crate (`crates/gamut-heic/tests/conformance.rs`). Fixtures are generated **at test time** with
libheif + kvazaar (`encode_rgba_to_heic`), so no binary fixtures are committed. The suite plugs the
reference HEVC decoder in as an `HevcCodestreamDecoder` backend (`De265Decoder`) and cross-checks
container structure (vs libheif's introspection), presentation pixels (vs libheif's RGBA decode, a
tight measured bound), planar samples (bit-exact vs a direct libde265 decode — proving the NAL
split/config delivery), `irot` orientation direction, motion-photo byte accounting, and hvcC↔YUV
coherence. This is a **dev-only** path: the shipped library stays pure Rust and C-free. Running the
tests needs the `third_party/{libheif,libde265,kvazaar}` submodules checked out
(`git submodule update --init --recursive`) and cmake/ninja + a C/C++ toolchain on `PATH`; the three
C libraries build from source on the first run. See [`references/heif`](../../references/heif/README.md)
("Oracle").

## Status

Container-parsing / byte-accounting layer, the typed `hvcC` record + NAL demux, the `HeifDecoder`
over pluggable `HevcCodestreamDecoder` backends, and the libheif differential oracle are all
implemented (issue #238), as is the high-bit-depth (10/12-bit, BT.709/BT.2020) `Rgba16`
presentation surface (issue #303). The role layer, the decode pipeline and the C2PA summary are
`gamut-heif`'s since issue #1086. The C2PA manifest-store locator (issue #429, under the #239
provenance epic) is a lens over the byte-accounting walk.
Encoding is **not** provided. See [`STATUS.md`](STATUS.md) for the full component ledger.

## License

MIT or Apache-2.0, at your option — see the [workspace README](../../README.md#license).
