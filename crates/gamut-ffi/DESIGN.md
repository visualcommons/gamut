# gamut-ffi C API design contract

The target specification for the C API of the gamut codecs (issue #242). The **provider
boundary** — the per-format `push_backend` entry points of issue #280 — is implemented (see
[Provider boundary](#provider-boundary-implemented-issue-280)); the consumer-boundary
conversion — largely automated from the Rust API, per-crate — has one hand-written slice so far,
AVIF (issue #666, see [AVIF consumer entry points](#avif-consumer-entry-points-issue-666)),
which settles the shape the rest follows. This document
fixes the conventions that conversion tooling generates against and that the Rust API is kept
compatible with (see the "C portability" convention in the repository `AGENTS.md`).

Why a C API rather than C++: Rust's C ABI compatibility is first-class, and C++ applications can
consume a well-designed C API equally idiomatically; maintaining good C++ API practice would mean
downgrading idiomatic Rust patterns anyway. Distribution is a ZIP of compiled C libraries built
from this crate (see [Packaging](#packaging)).

## Naming

- Everything is prefixed `gamut_` (types/constants `Gamut`/`GAMUT_`).
- Functions: `gamut_<crate>_<noun>_<verb>` — e.g. `gamut_png_encoder_new`,
  `gamut_png_encoder_free`, `gamut_jpeg_decoder_set_max_dimensions`.
- Opaque types: Rust `PngEncoder` → `GamutPngEncoder`, exposed in the header as
  `typedef struct GamutPngEncoder GamutPngEncoder;` — the cbindgen-native form. (An earlier
  draft specified a diverging `gamut_png_encoder_t` alias; that would be a handwritten header
  block, i.e. a manual drift surface, so the PascalCase typedef is the contract.)
- Enum constants: `GAMUT_<TYPE>_<VARIANT>` — e.g. `GAMUT_PIXEL_FORMAT_RGB8`.
- Headers are always **generated from the Rust source of truth**, never handwritten or
  hand-edited. Settled at the issue #280 kickoff: generation is **cbindgen** (pinned in
  `mise.toml` tools), using the **plain, non-expanding parse** on the stable toolchain —
  `[parse.expand]` would tie the shipped header to an unpinned nightly. The generated
  `include/gamut.h` is committed, regenerated with `mise run gen-ffi-header`, and CI fails if
  it drifts (`mise run check-ffi-header`). Consequence: every `extern "C"` item cbindgen must
  see is written out in source (macros may stamp internals, never the exported items).

## Opaque handles

Stateful or Rust-rich types — encoders, decoders, rich decode results (`DecodedPng`-class) —
cross the boundary as opaque pointers with `_new`/`_free` lifecycles. Rust `with_*` builder
methods map to `gamut_*_set_*` functions returning a status code.

Plain-data types cross **by value**: `gamut_core::Dimensions` (`#[repr(C)]`, width then height;
the AVIF slice passes it as its two fields, see [Buffers](#buffers)),
`gamut_core::PixelFormat` and `ColorModel` (`#[repr(u32)]`, explicit permanent discriminants),
and any format-crate enum once it is repr-pinned during its conversion pass.

## Buffers

All byte buffers are pointer + length pairs (`const uint8_t *data, size_t len`). An image
crosses as `(samples_ptr, samples_len, uint32_t width, uint32_t height)` — the pixel format is
named by the entry point (see [Trait mapping](#trait-mapping)), the sample pointer is typed by it
(`const uint8_t *` / `const uint16_t *`), and `samples_len` counts samples — and is validated at
the boundary with exactly the semantics of `ImageRef::new` (length =
`width * height * channels`, non-empty dimensions). Width and height cross as two scalars rather
than a `gamut_dimensions_t`: the header is generated without parsing `gamut-core`, and two
`uint32_t` arguments need no struct declaration on either side.

## Errors

Status codes are `int32_t` (`gamut_status_t`), with **permanent, append-only values** pinned by
const asserts in `src/status.rs`:

| Code | Value | Kind |
|---|---|---|
| `GAMUT_OK` | 0 | success |
| `GAMUT_STATUS_INVALID_INPUT` | 1 | maps `gamut_core::ErrorKind::InvalidInput` |
| `GAMUT_STATUS_UNSUPPORTED` | 2 | maps `gamut_core::ErrorKind::Unsupported` |
| `GAMUT_STATUS_IO` | 3 | maps `gamut_core::ErrorKind::Io` |
| `GAMUT_STATUS_NULL_ARGUMENT` | 4 | boundary-only |
| `GAMUT_STATUS_PANIC` | 5 | boundary-only |
| `GAMUT_STATUS_BUFFER_TOO_SMALL` | 6 | boundary-only (reserved for #242) |
| `GAMUT_STATUS_ABI_MISMATCH` | 7 | boundary-only: a pushed vtable's `abi_version` is from another seam generation — rebuild, don't fall back. Deliberately distinct from `UNSUPPORTED`, whose meaning (decline → try the next option) calls for the opposite caller reaction. |

`Error` is `#[non_exhaustive]`; unknown future variants map to a catch-all code. `gamut_status_t`
is the *library's* result type; it is distinct from the backend-seam `GamutAbiStatus`
(`gamut_codec_abi::Status`), whose `0`/`-1` values carry the registry fall-through contract and
belong to backends.

Error classification always uses `Error::kind()`, looking through `Error::Context`. Bare
`Error::InvalidInput`/`Error::Unsupported` messages remain borrowed `const char*` values valid for
the program lifetime. Contextual diagnostics (origin, byte offset, or owned detail), like
`Error::Io`, are formatted into the thread-local last-error string; no Rust allocation crosses the
C allocator boundary.

## Ownership

- Allocator symmetry: memory allocated by Rust is freed only by the matching `gamut_*_free`;
  C never `free()`s Rust memory and Rust never frees C memory.
- Every `_free` is null-tolerant (a no-op on `NULL`).
- Encode output is a Rust-owned byte buffer returned via out-params
  (`uint8_t **out, size_t *out_len`) and released with `gamut_bytes_free` — this matches the
  Rust contract of appending to a caller `Vec<u8>`.
- The two-call size/fill pattern (query size with `NULL`, then fill caller memory) is reserved
  for fixed-size or queryable payloads (decoded-image probes, metadata strings), not encode
  output.

## Panic policy

Every `extern "C"` entry point wraps its body in `catch_unwind`; a panic becomes
`GAMUT_STATUS_PANIC` and never unwinds across the boundary.

## Trait mapping

- **Consumer boundary (C calls Rust)** — the part tooling generates: each format crate's
  object-safe `EncodeImage<P>` / `DecodeImage<P>` impls are exported per
  (codec, `PixelFormat`) pair, enumerated mechanically from the `gamut_core::PixelFormat::ALL`
  slice × the crate's impl set: **one entry point per pair**, e.g.
  `gamut_png_encoder_encode_rgb8` (settled by the AVIF slice, issue #666). A per-pair entry
  point lets 16-bit samples cross as `const uint16_t *`, so the C compiler checks the sample
  type, where a single entry point dispatching on a `gamut_pixel_format_t` would take
  `const void *`.
- **Provider boundary (C implements Rust traits)** — carried by **`gamut-codec-abi`**
  (issue #241, sub-issue #272): its `#[repr(C)]` descriptors/vtables and `Foreign*` bridges are
  the sanctioned mechanism for foreign implementations plugging into gamut, including
  C-implemented hooks such as `gamut_heic::HevcDecoder` and the codestream backends. This
  crate must not grow a parallel vtable convention. `HevcDecoder` remains the *shape* template
  for new Rust hooks: a single object-safe method, borrowed bytes in, owned plain data out.
  The C entry points over this mechanism are live — see the next section.

## Provider boundary (implemented, issue #280)

One triple per seam handle, identical shape everywhere (10 handles: PNG/JPEG/WebP/JXL both
directions, AVIF encode, HEIC decode):

```c
GamutPngEncoder *gamut_png_encoder_new(void);           /* NULL only on panic */
void gamut_png_encoder_free(GamutPngEncoder *encoder);  /* null-tolerant; destroys backends */
gamut_status_t gamut_png_encoder_push_backend(GamutPngEncoder *encoder,
                                              const GamutEncoderVTable *vtable, void *ctx);
```

- `push_backend` wraps `(vtable, ctx)` in the seam bridge (`ForeignDecoder`/`ForeignEncoder`)
  and pushes the format crate's codec-abi adapter into the host's registry; backends are tried
  in push order and the crate's built-in implementation, where one exists, is the implicit tail.
- Status contract: `GAMUT_OK` transfers ownership of `ctx` to the handle (its `destroy` then
  runs exactly once, at `_free`); `GAMUT_STATUS_NULL_ARGUMENT` (null handle or vtable) and
  `GAMUT_STATUS_ABI_MISMATCH` (`vtable->abi_version != GAMUT_CODEC_ABI_VERSION`) leave `ctx`
  owned by the caller with no callback run.
- Thread contract: pushing asserts the backend is usable from any thread (the Rust registries
  bound `Send`; the bridges are `unsafe impl Send` on exactly that assertion). Handles
  themselves are single-threaded.
- **HEIC**: the handle stores the pushed `ForeignDecoder`s raw (in push order) rather than
  pre-built adapters — an `hvcC` record carries no picture size, so `AbiHevcDecoder` needs each
  item's `ispe` dimensions and is built per item at decode time, borrowing the stored backend
  through `gamut-codec-abi`'s `&mut` blanket impls (destroy-on-drop stays with the handle).
  There is no software HEVC tail (issue #18); an empty registry decodes nothing.
- **AVIF decode is excluded**: the Rust API has no decode registry — a single caller-supplied
  `Av1StillDecoder` is threaded per call, and the planned in-house decoder (issue #259) will
  make it optional. The decode direction joins the C surface when that registry exists.

## AVIF consumer entry points (issue #666)

The first consumer-boundary slice, hand-written over `gamut::avif`; each entry point is a
living shim over one Rust call.

```c
/* Encode: one entry point per EncodeImage layout (rgb8, rgba8, gray8, rgb16, rgba16). */
GamutAvifEncoder *gamut_avif_encoder_new_lossy(uint8_t quality);  /* AvifEncoder::lossy */
gamut_status_t gamut_avif_encoder_encode_rgba8(const GamutAvifEncoder *encoder,
                                               const uint8_t *samples, size_t samples_len,
                                               uint32_t width, uint32_t height,
                                               uint8_t **out, size_t *out_len);
void gamut_bytes_free(uint8_t *data, size_t len);

/* Parse: an owned handle with borrowed accessors. */
gamut_status_t gamut_avif_image_parse(const uint8_t *data, size_t len, GamutAvifImage **out);
gamut_status_t gamut_avif_image_primary_codestream(const GamutAvifImage *image,
                                                   const uint8_t **data, size_t *len);
void gamut_avif_image_free(GamutAvifImage *image);
```

- **Setters** map the `AvifEncoder::with_*` builders one-to-one
  (`gamut_avif_encoder_set_{matrix,primaries,transfer,color_range,chroma,bit_depth,
  premultiplied_alpha,rotation,mirror,icc_profile,exif,xmp}`). CICP values cross as their
  ITU-T H.273 code points and bit depth as a bit count, mapped by `gamut-color`'s own
  `from_code_point`/`from_flag`/`from_bits` (a direct dependency of this crate for that reason);
  chroma subsampling and mirror cross as `GAMUT_CHROMA_SUBSAMPLING_*` / `GAMUT_AVIF_MIRROR_*`
  constants, the chroma ones const-locked to `ChromaSubsampling`'s discriminants. A value the
  mapping rejects returns `GAMUT_STATUS_UNSUPPORTED` (a code point or depth gamut does not model)
  or `GAMUT_STATUS_INVALID_INPUT` (not one of the defined flag/constant values) and leaves the
  encoder unchanged; a null handle is `GAMUT_STATUS_NULL_ARGUMENT` before either. Lossy mode and
  quality are construction-time, as in Rust (`gamut_avif_encoder_new_lossy`).
- **Encode output** is a `Box<[u8]>` handed out as `(uint8_t *, size_t)` and released with
  `gamut_bytes_free(data, len)` — the length alone recovers the layout, so C carries no
  capacity. On any non-OK status the out-params are `NULL`/`0`.
- **Sample buffers** are validated with `ImageRef::new`'s semantics (length in *samples*); a
  misaligned `const uint16_t *` is `GAMUT_STATUS_INVALID_INPUT`, never dereferenced.
- **Byte inputs**: `NULL` with `len == 0` is the empty slice; `NULL` with a non-zero `len` is
  `GAMUT_STATUS_NULL_ARGUMENT`.
- **Parse** runs `AvifContainer::parse` and keeps (a clone of) its owned `AvifImage`, so the
  handle never borrows the caller's input. Accessors write borrowed `(const uint8_t *, size_t)`
  pairs valid until `gamut_avif_image_free`: `primary_codestream`, `alpha_codestream`, `av1c`
  (the `av1C` record body), `exif` (raw, including HEIF's 4-byte `exif_tiff_header_offset`, as
  the Rust `AvifImage::exif` exposes it), `xmp`, `icc_profile`; plus `is_premultiplied` and
  `dimensions` (the primary item's `ispe`). An absent payload is `GAMUT_OK` with `NULL`/`0`; a
  primary or alpha item that is derived (`grid`/`iden`/`iovl`) or not AV1 has no single
  codestream and is `GAMUT_STATUS_UNSUPPORTED`.
- **Not yet exposed**: the C2PA slot setters (`with_c2pa_reserved` / `with_c2pa`), whose use
  needs the encode report's slot range to cross the boundary too (issue #673); the alpha
  quality setter over the shipped `with_alpha_quality` (issue #692); the encoder knobs the
  Rust API has not shipped yet (speed, threads — issues #654, #655) join as setters when they
  land; AVIF decode waits for the decoder registry (above).

## Drift locking

The FFI surface must never constrain the idiomatic Rust API, but any drift between the two must
fail loudly, at compile time wherever structurally possible:

- **Living shims** — every entry point is a thin typed call into the real `gamut::*` API, so a
  renamed host, reshaped `push_backend`, or changed adapter breaks `cargo check` of this crate
  (`mise run check-ffi`, chained onto the pre-push `lint-quick`; every `--all-features` CI lane
  compiles it too).
- **`seam_handle!` const pins** — the macro stamps the internals (validation order, ownership,
  `catch_unwind` — an entry point cannot forget the panic policy) and const-pins each
  handwritten `extern "C"` wrapper's existence and exact signature, so a missing or drifted
  wrapper is a compile error in both directions. It also stamps the identical per-handle
  boundary contract test.
- **Const asserts** — permanent numeric values (status codes, `GAMUT_CODEC_ABI_VERSION`,
  `GAMUT_MAX_PLANES`) are literals here, compile-locked to their `gamut-codec-abi` sources;
  the seam crate itself const-pins `ABI_VERSION`, `Status` values, and vtable/descriptor field
  offsets, and `gamut-core` const-pins the `PixelFormat`/`ColorModel` discriminants.
- **CI diff gates** for what compilation cannot see: `check-ffi-header` (committed `gamut.h`
  vs fresh cbindgen output) and `check-ffi-seams` (the workspace's `pub fn push_backend` set vs
  the committed `SEAMS.txt` manifest — a format crate *gaining* a seam is unreferencable from
  here, so it is the one drift class caught in CI rather than by the compiler).

## Packaging

The release ZIP artifacts are builds of this one crate at chosen feature sets — the feature
table (strictly synced to `gamut`'s, enforced by `mise run check-ffi-features`) is the packaging
matrix:

- The fat artifact is a `--features all` build: `cdylib` + `staticlib` + generated header.
- Slim per-format or per-group variants are builds of the same crate with a feature subset —
  no per-format crates.
- Static consumers of the fat `staticlib` get dead-code elimination at their final link; the
  `cdylib` trades disk size for a single shared, demand-paged library.
- The committed header is **feature-complete** (no `GAMUT_FEATURE_*` `#ifdef` guards): one
  `gamut.h` declares the whole surface for every artifact, and a slim build simply does not
  export the symbols of the features it omits — using one fails at link time, the standard
  "one header, link decides" C model this packaging already leans on.

## First-pass scope

Shipped: the provider boundary above (issue #280), and AVIF encode, config setters and container
parse (issue #666). Next: the core `EncodeImage`/`DecodeImage` trait pair and the builder
configs of the other format crates (issue #242). Excluded initially
(revisited per crate as conversion proceeds): rich ad-hoc surfaces
(`DecodedPng`/`PngImage`, jpeg `info()`/`metadata()`, HEIC item/metadata detail), the tiff/ifd
low-level writer functions, and the `primitives` re-exports. The C surface is feature-gated
identically to `gamut`.
