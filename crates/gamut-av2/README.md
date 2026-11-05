# gamut-av2

`gamut-av2` is a pure-Rust AV2 (AOMedia Video 2) image encoder — the next-generation successor to
AV1.

## Goals

Part of the [gamut](../../README.md) workspace, this crate exists to provide AV2 **still-image
encoding** that is:

- **Memory-safe on hostile input.** `#![forbid(unsafe_code)]`.
- **Clean-slate from the spec.** Implemented directly from the AV2 bitstream specification (see
  [`../../references/`](../../references)) rather than wrapping a C reference encoder.
- **Layered on shared crates.** It will reuse the workspace's shared primitives, as
  [`gamut-av1`](../gamut-av1) does, rather than carry its own copies.
- **Permissively licensed**, matching the royalty-free AV2 format.

This crate is forward-looking scaffolding; AV2 is still stabilizing, so it may move or be dropped as
the project's scope sharpens (see the workspace README's "Scope").

## Usage

No public API yet — implementation pending. It will follow the same shape as
[`gamut-av1`](../gamut-av1): a still-image encode entry point reachable through the umbrella crate's
`av2` feature.

## Status

Placeholder — implementation pending.

## Roadmap

- Track AV2 bitstream stabilization; reuse the `gamut-av1` module layout (headers / tile / CDF).
- Lossless intra keyframe first, mirroring the AV1 lossless-first approach.

## License

MIT or Apache-2.0, at your option — see the [workspace README](../../README.md#license).
