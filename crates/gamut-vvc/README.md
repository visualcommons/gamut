# gamut-vvc

`gamut-vvc` is a placeholder for a pure-Rust VVC (Versatile Video Coding, H.266) still-image
encoder. No codec ships here yet.

## Goals

Part of the [gamut](../../README.md) workspace, this crate exists to provide VVC **still-image
encoding** that is:

- **Memory-safe on hostile input.** `#![forbid(unsafe_code)]`.
- **Clean-slate from the spec.** Implemented directly from the H.266/VVC specification (see
  [`../../references/`](../../references)) rather than wrapping a C reference encoder.
- **Layered on shared crates.** It will reuse the workspace's shared primitives rather than carry
  its own copies.

Note: VVC is patent-encumbered, unlike gamut's royalty-free focus formats; this crate is scaffolding
and may move or be dropped as the project's scope sharpens (see the workspace README's "Scope").

## Usage

No public API yet — implementation pending. When implemented it follows the workspace convention
for codec crates (the workspace [`AGENTS.md`](../../AGENTS.md)): an encoder type implementing
`gamut_core::EncodeImage` over the pixel layouts it supports, with its codestream exposed through
the shared `gamut-codec-abi` seam — a typed backend trait, a thin codec-abi adapter, and a registry
tried in push order with this crate's own software implementation as the tail — reachable through
the umbrella crate's `vvc` feature.

## Status

Placeholder — implementation pending.

## Roadmap

- VVC intra still-image encoding of the raw codestream.

## License

MIT or Apache-2.0, at your option — see the [workspace README](../../README.md#license).
