# gamut-ffi

`gamut-ffi` provides C-compatible FFI bindings for the gamut image codecs, so `gamut` can be called
from C, C++, Python, Go, and any other language with a C FFI. It has its own C API (specified in
[DESIGN.md](DESIGN.md)); it is not a drop-in replacement for any existing C image library's API.

## Goals

Part of the [gamut](../../README.md) workspace, this crate exists to:

- **Expose a stable C ABI.** A `extern "C"` surface over the umbrella [`gamut`](../gamut) crate, so
  the memory-safe Rust encoders are callable from any language with a C FFI.
- **Wrap, not re-implement.** All encoding stays in the gamut codecs; this crate only marshals across
  the boundary.
- **Contain `unsafe` to the boundary.** `unsafe` is *permitted* here, but only for the `extern "C"`
  layer (raw pointers, lengths). The Rust crates underneath keep their own policy: most are
  `#![forbid(unsafe_code)]`, while the six hot-path crates (`gamut-png`, `gamut-deflate`,
  `gamut-dsp`, `gamut-color`, `gamut-cmm`, `gamut-jpeg`) *deny* it, so a benchmarked site may carry
  a written exception.

This crate builds as both a `cdylib` and a `staticlib`, and ships a committed, cbindgen-generated
C header ([include/gamut.h](include/gamut.h) — regenerate with `mise run gen-ffi-header`). It is
the **single C-distribution crate** of the workspace: its feature table strictly mirrors the
`gamut` umbrella's (enforced by `mise run check-ffi-features`) and doubles as the packaging
matrix — the fat release library is a `--features all` build, and slim per-format variants are
builds of this same crate with a feature subset, not separate crates. Static consumers of the
fat `staticlib` get dead-code elimination at their final link.

The C API conventions — naming, opaque handles, buffer/error/ownership contracts, panic policy,
and how the Rust traits map across the boundary — are specified in [DESIGN.md](DESIGN.md)
(issue #242).

## Usage

The implemented surface is the **provider boundary** (issue #280): per-format opaque handles
whose `push_backend` entry points let a C codestream backend — a `gamut-codec-abi` vtable over
an opaque context — plug into the format crate's backend registry:

```c
#include "gamut.h"

static GamutAbiStatus my_decode(void *ctx, const GamutStreamConfig *cfg,
                                const uint8_t *codestream, size_t codestream_len,
                                const GamutImageDesc *out) { /* ... */ }
static void my_destroy(void *ctx) { /* ... */ }

const GamutDecoderVTable vtable = {
    .abi_version = GAMUT_CODEC_ABI_VERSION,
    .supports = NULL, /* NULL = supports nothing; usually a real callback */
    .decode = my_decode,
    .destroy = my_destroy,
};

GamutPngDecoder *dec = gamut_png_decoder_new();
if (gamut_png_decoder_push_backend(dec, &vtable, my_ctx) != GAMUT_OK) { /* caller keeps my_ctx */ }
/* ... the PNG entry points that run decodes are issue #242 ... */
gamut_png_decoder_free(dec); /* runs my_destroy(my_ctx) exactly once */
```

Backends are tried in push order; the built-in codec, where one exists, is the implicit tail.

## Status

The provider boundary (`_new`/`_free`/`push_backend` for PNG, JPEG, WebP, JXL both directions,
AVIF encode, and HEIC decode) is live, with the committed C header and its CI drift gates. The
first slices of the **consumer boundary** — C entry points that run encodes and decodes — have
shipped: AVIF encode (`gamut_avif_encoder_encode_*` for the five layouts, plus the encoder's config
setters) and AVIF container parse (`gamut_avif_image_*`, issue #666). The rest of the consumer boundary is the per-crate conversion work
tracked in issue #242; AVIF decode joins the surface when its Rust registry exists (issue #259).
See [DESIGN.md](DESIGN.md) for the full contract.

## License

MIT or Apache-2.0, at your option — see the [workspace README](../../README.md#license).
