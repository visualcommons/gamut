# gamut-ffi C API design contract

The target specification for the C API of the gamut codecs (issue #242). The **provider
boundary** — the per-format `push_backend` entry points of issue #280 — is implemented (see
[Provider boundary](#provider-boundary-implemented-issue-280)); the consumer-boundary
conversion — largely automated from the Rust API, per-crate — has not started. This document
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

Plain-data types cross **by value**: `gamut_core::Dimensions` (`#[repr(C)]`, width then height),
`gamut_core::PixelFormat` and `ColorModel` (`#[repr(u32)]`, explicit permanent discriminants),
and any format-crate enum once it is repr-pinned during its conversion pass.

## Buffers

All byte buffers are pointer + length pairs (`const uint8_t *data, size_t len`). An image
crosses as `(samples_ptr, samples_len, gamut_dimensions_t, gamut_pixel_format_t)` and is
validated at the boundary with exactly the semantics of `ImageRef::new` (length =
`width * height * channels`, non-empty dimensions).

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
  slice × the crate's impl set (e.g. `gamut_png_encoder_encode_rgb8`, or a single entry point
  dispatching on a `gamut_pixel_format_t` argument — chosen at conversion kickoff, uniformly).
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

### Drift locking

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

Shipped: the provider boundary above (issue #280). Next: the core `EncodeImage`/`DecodeImage`
trait pair and the builder configs of each format crate (issue #242). Excluded initially
(revisited per crate as conversion proceeds): rich ad-hoc surfaces
(`DecodedPng`/`PngImage`, jpeg `info()`/`metadata()`, HEIC item/metadata detail), the tiff/ifd
low-level writer functions, and the `primitives` re-exports. The C surface is feature-gated
identically to `gamut`.
