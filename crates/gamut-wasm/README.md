# gamut-wasm

`gamut-wasm` is a placeholder for WebAssembly bindings to the gamut image codecs. No binding ships
yet: the crate exports nothing to JavaScript or TypeScript.

## Goals

Part of the [gamut](../../README.md) workspace, this crate exists to:

- **Make WASM a first-class target.** A native Rust → wasm build is smaller and faster to
  instantiate than the C codecs run through Emscripten, and it talks to the JS/TS ecosystem directly
  — making serverless/edge image optimization (Workers, Lambda, and friends) practical.
- **Expose the encoders, not re-implement them.** It is a thin `wasm-bindgen` wrapper over the
  umbrella [`gamut`](../gamut) crate; all encoding stays in the gamut codecs.
- **Stay memory-safe.** `#![forbid(unsafe_code)]` — the `wasm-bindgen` glue needs no `unsafe`.

This crate builds as a `cdylib`.

## Usage

No public API yet — implementation pending. The crate exports no binding today, so there is
nothing to call from JavaScript or TypeScript.

## JPEG XL note

gamut-wasm targets `wasm32-unknown-unknown` (the wasm-bindgen ABI). On that target JPEG XL is
**decode-only**: gamut-jxl's decoder is pure Rust (jxl-rs) and works everywhere, but its encoder
wraps the C++ libjxl reference implementation, and no C/C++ toolchain emits archives linkable into
`wasm32-unknown-unknown` — an upstream toolchain boundary, not a build-system gap. Consumers who
need JXL *encode* in WebAssembly can build for **`wasm32-unknown-emscripten`** and use `gamut-jxl`
directly (its `gamut-jxl-sys` builds libjxl with the emsdk toolchain there); browser-bundle JXL
encode would require a pure-Rust JPEG XL encoder, which does not exist yet (jxl-rs ships none).

## Status

Placeholder — implementation pending.

## Roadmap

- `wasm-bindgen` entry points for the implemented encoders (AVIF first), with a `wasm-pack` build.
- Typed JS/TS API and a published npm package.

## License

MIT or Apache-2.0, at your option — see the [workspace README](../../README.md#license).
