# gamut-core — foundational primitives status

**v1 stabilization: GitHub issue #177.** `gamut-core` is the format-neutral foundation every gamut
codec builds on: the workspace-wide `Error`/`Result`, the `EncodeImage`/`DecodeImage` traits, the
branded `ImageRef`/`ImageBuf` interleaved buffers, the `Dimensions` value type, the sealed
`Pixel`/`Sample`/`ColorModel` pixel vocabulary, and the shared `luminance` reference constants. It
has **no dependency on any other workspace crate** — so no format crate is below it; most crates
depend on it, never the other way around — and is `#![forbid(unsafe_code)]`.

It is also where **pixel conversion** is defined, once, for every layout pair (issue #268): format
crates decode to what the file carries and hand the layout change to `convert`, so widening rules and
lossy policy are stated in exactly one place instead of once per codec.

**Keystone:** the **branded, length-validated buffer**. `len == width * height * P::CHANNELS` (with
non-empty, non-overflowing dimensions) is checked exactly once, at `ImageRef::new` / `ImageBuf::new`,
and the pixel brand `P` makes a layout mismatch (CMYK bytes into an RGBA encoder) a *compile* error
rather than a silent reinterpretation. A codec then pulls the raw slice back out with
`ImageRef::as_samples` at zero cost, so its hot loop is byte-identical to operating on a bare `&[u8]`
while never re-checking the invariant.

## Public surface (frozen at v1)

| Item | Shape | Openness |
| ---- | ----- | -------- |
| `Error` / `ErrorKind` / `Result` | one boxed pointer built by `invalid_input`/`unsupported`/`From<std::io::Error>`, enriched by `with_origin`/`with_byte_offset`/`with_detail`, read through `kind`/`origin`/`byte_offset`/`detail`/`static_message`/`source` | `Error` **opaque** (private representation, matched by `kind`); `ErrorKind` `#[non_exhaustive]` `repr(u32)` with append-only discriminants |
| `Dimensions` | `{ width, height }` value type + `new`/`width`/`height`/`num_pixels`/`sample_count` | private fields; `new` rejects a zero side; `#[repr(C)]` |
| `EncodeImage<P>` / `DecodeImage<P>` | one `impl` per supported layout `P`; object-safe | sealed via the `Pixel` bound |
| `ImageRef<'a, P>` / `ImageBuf<P>` | borrowed / owned interleaved buffers | length-validated at construction |
| `Pixel` / `Sample` / `ColorModel` | compile-time layout descriptors | `Pixel`/`Sample` **sealed**; `ColorModel` `#[non_exhaustive]` |
| `PixelFormat` | runtime tag mirroring the 11 markers (`Pixel::FORMAT`), with `ALL` and per-format accessors such as `channels` | `#[non_exhaustive]` `repr(u32)`; discriminants are permanent C ABI values, append-only |
| 11 pixel markers | `Gray8` `Bilevel` `Indexed8` `Rgb8` `Rgba8` `Cmyk8` `GrayAlpha8` `Gray16` `Rgb16` `Rgba16` `GrayAlpha16` | closed set, defined only here |
| `luminance::*` | reference nit levels + BT.601/709/2020 fixed-point luma weights | shared by `gamut-color`, `gamut-tonemap`, and `convert` |
| `segment::*` | `Segment<K>` (half-open byte range + format-defined kind) and the `covers`/`tiles` byte-tiling predicates | generic over the format's own kind; the walks stay in the format crates |
| `convert::*` | `ConvertPolicy` + `AlphaPolicy`/`DepthPolicy`/`LumaPolicy`, `RawImage`, `convert`/`convert_from_raw`/`convert_from_raw_into` | policy enums `#[non_exhaustive]` `repr(u32)`, append-only |
| `DecodeLimits` / `Cicp` / `Orientation` / `Palette` / `RawMetadata` / `c2pa::*` | shared vocabulary for what every format carries: decode resource bounds, H.273 code points, the eight EXIF orientations, a 1–256-entry RGB(+alpha) colour table, verbatim EXIF/XMP/ICC payloads, and the C2PA report sentence and minimum store length | `RawMetadata` `#[non_exhaustive]`; `Orientation` `repr(u8)` |

Adding items (new `ErrorKind`/`ColorModel` variants, more buffer helpers, more markers) stays
backward-compatible; removing or reshaping any of the above would not.

## Settled design decisions (intentional, not gaps)

- **Interleaved `u8` / `u16` only.** `Sample` is sealed over `u8`/`u16`. Planar layouts and a stream's
  *coded* bit depth are codec concerns and live in [`gamut-color`](../gamut-color) (`Planar8`,
  `BitDepth`); raw/mosaic (CFA) imagery lives in [`gamut-dng`](../gamut-dng) (`RawImage`). Core stays
  the interleaved-still-image vocabulary the 8/16-bit codecs (png, tiff, avif, webp) share.
- **Open where growth is additive, sealed where it must not be.** `ErrorKind` and `ColorModel` are
  `#[non_exhaustive]`; `Pixel`/`Sample` are sealed so the set of pixel layouts is closed and defined
  only here.
- **`Dimensions` are never empty.** Its fields are private and `Dimensions::new`, the only
  constructor, rejects a zero width or height; the length product is then enforced once, at buffer
  construction, so codecs receive a known-good buffer and never re-check.
- **One opaque error, matched by kind.** `Error`'s representation is private: callers match on
  `kind` and read `origin`, `byte_offset`, `detail`, `static_message` and `source`, with exactly one
  way to ask each question however the error was enriched on its way up. It is one pointer wide, so
  `Result<T>` stays small on every success path; the one allocation happens only when an error is
  constructed. Classification and fallback never depend on presentation text.
- **`luminance` lives here, not in `gamut-color`.** Both `gamut-color` and `gamut-tonemap` need the
  reference nit levels, and `gamut-tonemap` depends only on core at runtime; putting the constants
  here keeps a single authoritative definition without coupling tonemap to color. The luma weight
  triples joined them for the same reason: `convert` needs them, and `gamut-png`/`gamut-tiff`/
  `gamut-jxl` depend on core but **not** on `gamut-color`, so defining them there would have added a
  dependency edge to four crates. `gamut_color::rgb_to_ycbcr` now derives its BT.601 luma row from
  here rather than restating it.
- **`convert` is layout, not colorimetry.** It owns the axes the sealed `Pixel` matrix describes —
  channel count, alpha, sample width — and refuses the two layouts that need machinery core does not
  have: `Indexed8` (an index buffer does not carry its `Palette`) and `Cmyk8` (needs an ICC transform, the
  `gamut-cmm` epic). Shipping a naive uncalibrated CMYK formula into a frozen surface would have to
  be contradicted later, so it is a typed `Unsupported` instead.
- **Lossless by default, lossy by opt-in.** `ConvertPolicy::lossless()` is the `Default`, so every
  decoder's typed path is faithful to the file unless the caller says otherwise. Widening (grey into
  RGB, opaque alpha, 8-bit into 16) needs no policy: it loses nothing.
- **Typed errors, no panics in library paths.** No `unwrap`/`expect`/`panic!` outside tests; the
  documented `ImageRef::row`/`pixel` panics are the idiomatic out-of-bounds-index behaviour (like
  `slice[i]`), not error handling.

## Deferred / tracked follow-ups (all additive — none blocks v1)
- **Adopting the shared `Palette`.** `Palette` ships here, but `gamut-png` (`PngPalette`) and
  `gamut-tiff` (`Palette8`) still define their own palette types and no format crate consumes the
  shared one yet. `Indexed8` still has no `EncodeImage` path, because the single-buffer trait shape
  cannot carry the table alongside the indices.

## Validation

Backed by inline unit tests (each type's own contract) plus `tests/surface.rs`, which drives the
crate through its **public API only** (a toy `EncodeImage` → `DecodeImage` round-trip) so the surface
is proven self-sufficient.

`convert` is the largest body of algorithmic content in the workspace with **no oracle anywhere** —
no reference implementation converts gamut's pixel matrix and no specification ships vectors for it
— so `docs/testing.md` names `gamut-core` a crate where a property is the primary signal.
`src/invariants.rs` states the module's documented contract as executable laws: acceptance depends
on the layouts and the policy and never on the sample values; an accepted conversion keeps the
dimensions and the target layout's sample count; `Indexed8`/`Cmyk8` are closed under every policy;
the in-place and allocating doors write the same image; every layout converts to itself unchanged;
and widening 8-bit samples to 16 then rescaling back is exact, which is the inverse relationship
the module claims of PNG §13.12. Those laws are driven by pinned-seed `proptest` properties here
and are the attachment point for the out-of-tree fuzz tier (`test-support`).

The laws are stated over `convert_from_raw`, not `convert`: the typed door is a one-line delegation
to the raw one, so a law asserting the two agree would be checking the compiler rather than the
engine. The crate-root doctest keeps the documented usage example compiler-checked.
No benches: core has no computational hot path — its buffers are zero-cost branding and validation is
a single length check — so there is nothing meaningful to measure (the workspace bench harness scopes
to real-surface crates). Gates: `mise run test` / `lint` (`clippy -D warnings`, `missing_docs` fatal)
/ `fmt-check` / `coverage` (≥ 80%).
