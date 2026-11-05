# gamut-vvc

`gamut-vvc` is a pure-Rust VVC (Versatile Video Coding, H.266) image encoder.

## Goals

Part of the [gamut](../../README.md) workspace, this crate exists to provide VVC **still-image
encoding** that is:

- **Memory-safe on hostile input.** `#![forbid(unsafe_code)]`.
- **Clean-slate from the spec.** Implemented directly from the H.266/VVC specification (see
  [`../../references/`](../../references)) rather than wrapping a C reference encoder.
- **Layered on shared crates.** It will reuse the workspace's shared primitives and container
  crates rather than carry its own copies.

Note: VVC is patent-encumbered, unlike gamut's royalty-free focus formats; this crate is scaffolding
and may move or be dropped as the project's scope sharpens (see the workspace README's "Scope").

## Usage

No public API yet — implementation pending. It will follow the same shape as
[`gamut-avif`](../gamut-avif): an encoder type implementing [`gamut_core::EncodeImage`], reachable
through the umbrella crate's `vvc` feature.

## Status

Placeholder — implementation pending.

## Roadmap

- VVC intra still-image encoding, wrapped in an ISOBMFF container.

## License

MIT or Apache-2.0, at your option — see the [workspace README](../../README.md#license).
