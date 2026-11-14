//! AVIF C entry points: the encoder handle [`GamutAvifEncoder`] (backend seam, config setters,
//! encode; the `encoder` module) and the parsed-container handle [`GamutAvifImage`] (the `image`
//! module).
//!
//! The AVIF seam is the AV1 still-picture OBU stream (`AV1_CODEC_ID`, encode direction).
//! Backends are tried in push order; the built-in `gamut-av1` encoder is the implicit tail.
//!
//! There is deliberately no `gamut_avif_decoder_push_backend`: `gamut-avif` has no
//! `gamut-codec-abi` adapter for its `Av1CodestreamDecoder` seam yet, so a C backend has no door
//! into `AvifDecoder`'s registry, and the planned in-house decoder (issue #259) has not landed.
//! The decode direction joins the C surface with that adapter (see DESIGN.md). What *is* here on
//! the read side is the container parse (issue #666): [`GamutAvifImage`] hands a C caller the
//! primary and alpha AV1 codestreams, the `av1C` record, and the metadata payloads, so it can
//! drive any AV1 decoder itself.
//!
//! # Consumer-boundary shape
//!
//! - **Encode** is one entry point per input layout the Rust encoder implements
//!   `EncodeImage` for (`rgb8`, `rgba8`, `gray8`, `rgb16`, `rgba16`), so 16-bit samples cross as
//!   `const uint16_t *` and the C compiler checks the sample type. The output is a Rust-owned
//!   buffer released with `gamut_bytes_free`. Each encode also records where it placed the
//!   C2PA manifest-store slot, read back with `gamut_avif_encoder_c2pa_slot` (issue #673).
//! - **Config setters** (`gamut_avif_encoder_set_*`) map the Rust `with_*` builders one-to-one;
//!   lossy mode and quality are fixed at construction, exactly as in Rust
//!   (`gamut_avif_encoder_new_lossy` ↔ `AvifEncoder::lossy`).
//! - **Parse** returns an owned [`GamutAvifImage`]; every accessor hands out pointers borrowed
//!   from it, valid until `gamut_avif_image_free`.

mod encoder;
mod image;

pub use encoder::*;
pub use image::*;
