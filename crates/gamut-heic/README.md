# gamut-heic

`gamut-heic` is a pure-Rust HEIC/HEIF still-image **container decoder** — HEVC (H.265) intra image
items wrapped in an ISOBMFF container. gamut is **decode-only** for this format: this crate parses
HEIF/HEIC and decodes it through a caller-supplied HEVC decoder (the `HevcDecoder` hook); it does
**not** encode it (see [`references/heif`](../../references/heif/README.md)).

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
- **Sharing the AVIF container.** It reuses [`gamut-isobmff`](../gamut-isobmff) — the same ISOBMFF
  box tree that backs AVIF — for the `ftyp`/`meta`/`iloc`/`iinf`/`iprp`… grammar, layering the HEIF
  still-image profile and the byte-accounting guarantee on top.
- **Pluggable codestream decoder.** Per issue #238, the HEVC-intra pixel decode sits behind the
  `HevcDecoder` hook: the container layer hands each decoder-ready item to the backends a caller
  pushes, and this crate ships no software HEVC decoder of its own.

Note: HEVC is patent-encumbered, unlike gamut's royalty-free focus formats; this crate is
decode-side scaffolding and may move or be dropped as the project's scope sharpens (see the
workspace README's "Scope").

## Layers

- **`HeifContainer`** — the total, byte-accounting representation. `HeifContainer::parse(&[u8])`
  walks the top-level boxes into a contiguous, gap-free `segments` list covering `0..len` exactly,
  and shadow-walks `meta`/`iprp` for boxes the semantic layer does not consume (`unknown_meta_boxes`).
  `c2pa()` / `c2pa_manifest_stores()` are lenses over that walk: they locate the C2PA manifest store
  in a top-level `uuid` `ContentProvenanceBox` and hand back its opaque bytes and exact byte range.
- **`HeifImage` / `HeifItem`** — the role-typed semantic view over the primary still-image stream
  (wrapping `gamut_isobmff::IsoBmffImage`): brands and `is_hevc_still()`, the validated primary
  item, item kinds and typed properties (`ispe`/`irot`/`imir`/`clap`/`pasp`/`pixi`/`colr`/`clli`/
  `hvcC`), and relationship lenses (thumbnails, alpha/depth auxiliaries, premultiplication,
  Exif/XMP metadata, grid/overlay derivations, `altr` alternatives). Every accessor is a computed
  lens — no state is duplicated out of the underlying model.

## Usage

```rust
use gamut_heic::HeifContainer;

# fn demo(bytes: &[u8]) -> gamut_core::Result<()> {
let container = HeifContainer::parse(bytes)?;
let image = container.image();
if image.is_hevc_still() {
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
reference HEVC decoder into the crate's pluggable `HevcDecoder` seam (`De265Decoder`) and cross-checks
container structure (vs libheif's introspection), presentation pixels (vs libheif's RGBA decode, a
tight measured bound), planar samples (bit-exact vs a direct libde265 decode — proving the NAL
split/config delivery), `irot` orientation direction, motion-photo byte accounting, and hvcC↔YUV
coherence. This is a **dev-only** path: the shipped library stays pure Rust and C-free. Running the
tests needs the `third_party/{libheif,libde265,kvazaar}` submodules checked out
(`git submodule update --init --recursive`) and cmake/ninja + a C/C++ toolchain on `PATH`; the three
C libraries build from source on the first run. See [`references/heif`](../../references/heif/README.md)
("Oracle").

## Status

Container-parsing / byte-accounting / role layer, the typed `hvcC` record + NAL demux, the pluggable
`HevcDecoder` decode pipeline, and the libheif differential oracle are all implemented (issue #238),
as is the high-bit-depth (10/12-bit, BT.709/BT.2020) `Rgba16` presentation surface (issue #303). The
C2PA manifest-store locator (issue #429, under the #239 provenance epic) is implemented as a lens
over the byte-accounting walk.
Encoding is **not** provided. See [`STATUS.md`](STATUS.md) for the full component ledger.

## License

MIT or Apache-2.0, at your option — see the [workspace README](../../README.md#license).
