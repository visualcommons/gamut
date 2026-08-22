//! VP8 — the lossy still-image (key-frame intra) bitstream (RFC 6386).
//!
//! gamut is image-first: only the intra key-frame subset is in scope — no inter-frame prediction,
//! motion, or sequence coding. A VP8 key frame is a boolean-entropy-coded ([`bool_coder`], §7)
//! [`header`] (§9) followed by per-macroblock prediction-mode records ([`prediction`], §11-§12) and
//! DCT/WHT coefficient tokens ([`tokens`], §13); reconstruction dequantizes ([`quant`], §9.6/§14.1),
//! inverse-transforms ([`transform`], §14.3/§14.4), and loop-filters ([`loop_filter`], §15).
//!
//! These modules expose the declarative type surface the VP8 encoder/decoder fill in at milestone
//! M2 (also requiring BT.601 YCbCr 4:2:0 in `gamut-color`). The component breakdown lives in
//! `../STATUS.md` (sections G-N).

// Most users want [`WebpEncoder`](crate::WebpEncoder)/[`WebpDecoder`](crate::WebpDecoder); [`frame`]
// is the documented low-level entry point (encode/decode a VP8 key frame directly). The remaining
// spec-section modules that `frame` is assembled from are crate-internal.
pub mod frame;

pub(crate) mod bool_coder;
pub(crate) mod cost;
pub(crate) mod effort;
pub(crate) mod header;
pub(crate) mod loop_filter;
pub(crate) mod prediction;
pub(crate) mod quant;
pub(crate) mod tokens;
pub(crate) mod transform;
