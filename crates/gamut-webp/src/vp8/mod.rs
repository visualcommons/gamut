//! VP8 — the lossy still-image (key-frame intra) bitstream (RFC 6386).
//!
//! gamut is image-first: only the intra key-frame subset is in scope — no inter-frame prediction,
//! motion, or sequence coding. A VP8 key frame is a boolean-entropy-coded (`bool_coder`, §7)
//! `header` (§9) followed by per-macroblock prediction-mode records (`prediction`, §11-§12) and
//! DCT/WHT coefficient tokens (`tokens`, §13); reconstruction dequantizes (`quant`, §9.6/§14.1),
//! inverse-transforms (`transform`, §14.3/§14.4), and loop-filters (`loop_filter`, §15). Those
//! component modules are crate-private.
//!
//! The component breakdown lives in `../STATUS.md` (sections G-N).

// The whole module is crate-private (issue #1163); callers reach VP8 through `WebpEncoder` /
// `WebpDecoder` or a `WebpCodestream` backend. `frame` is the internal entry point (encode/decode a
// VP8 key frame directly) the crate's encoder and decoder call, re-exporting the `encoder`,
// `decoder` and `reconstruct` halves it is split into.
pub(crate) mod frame;

pub(crate) mod bool_coder;
pub(crate) mod cost;
mod decoder;
pub(crate) mod effort;
mod encoder;
pub(crate) mod header;
pub(crate) mod loop_filter;
pub(crate) mod prediction;
pub(crate) mod quant;
mod reconstruct;
pub(crate) mod tokens;
pub(crate) mod transform;
