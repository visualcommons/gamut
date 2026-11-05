# gamut-av2

`gamut-av2` is a placeholder for a pure-Rust AV2 (AOMedia Video 2) still-image encoder — AV2 is the
next-generation successor to AV1. No codec ships here yet.

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

No public API yet — implementation pending. When implemented it follows the workspace convention
for codec crates (the workspace [`AGENTS.md`](../../AGENTS.md)): an encoder type implementing
`gamut_core::EncodeImage` over the pixel layouts it supports, with its codestream exposed through
the shared `gamut-codec-abi` seam — a typed backend trait, a thin codec-abi adapter, and a registry
tried in push order with this crate's own software implementation as the tail — reachable through
the umbrella crate's `av2` feature.

## Status

Placeholder — implementation pending.

## Roadmap

- Track AV2 bitstream stabilization; reuse the `gamut-av1` module layout (headers / tile / CDF).
- Lossless intra keyframe first, mirroring the AV1 lossless-first approach.

## License

MIT or Apache-2.0, at your option — see the [workspace README](../../README.md#license).
