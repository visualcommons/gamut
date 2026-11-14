# gamut-codec-abi

`gamut-codec-abi` defines the **codestream-backend seam** for the [gamut](../../README.md)
workspace: the single call shape and fallback contract by which a foreign (C/FFI) or alternate
codestream backend plugs into any gamut format crate.

It is deliberately tiny: `#![no_std]`, dependency-free, with `unsafe` confined to one `bridge`
module.

## What it provides

- **Rust twins** — the object-safe [`Decoder`] / [`Encoder`] traits a pure-Rust backend implements
  directly.
- **`repr(C)` vtables** — `DecoderVTable` / `EncoderVTable`: a function-pointer table plus an opaque
  `ctx`, the shape a C or `-sys` backend exposes. Each leads with an `abi_version` (`ABI_VERSION`);
  each descriptor (`StreamConfig` / `EncodeConfig` / `ImageDesc`) leads with a `struct_size` for
  forward-compatible field growth.
- **Bridges** (the crate's only `unsafe`) — `bridge::ForeignDecoder` / `bridge::ForeignEncoder`
  adapt a C vtable *into* a Rust twin (C → Rust); `bridge::lower_decoder` / `bridge::lower_encoder`
  expose a boxed Rust twin *as* a C vtable (Rust → C), returned as a `bridge::Lowered` vtable +
  `ctx` pair.
- **Identifiers** — `codec_id(*b"av01")` builds a big-endian FourCC codec id, and the
  `pixel_format` module defines the `ImageDesc::pixel_format` tag namespace.
- **`Registry<T>`** (`std` feature) — the push-order backend list every format crate holds its
  pushed backends in, implementing the fallback contract below once.

## Fallback contract

A host holds backends in a registry, tried in **push order**, with a software fallback (when a crate
ships one) as the implicit tail. `supports()` returning `false` (C `Status::UNSUPPORTED`) is the
**only** signal that lets the host fall through to the next backend. A backend that *accepts* a job
and then fails returns a terminal, non-OK `Status` that propagates to the caller — the host does not
retry a later backend. `Send` is bound by the host at insertion, not required by these traits.

## Design

Mirrors the `gamut_heic::HevcCodestreamDecoder` precedent — an object-safe trait, borrowed bytes in,
owned plain data out — generalized to a two-way, C-shaped vtable. See the crate-level docs for the
full contract.
