//! AV1 still-image (intra-frame) encoder and decoder. AVIF relies on AV1 intra-frame coding, so
//! this crate is available standalone as well as through
//! [`gamut-avif`](https://crates.io/crates/gamut-avif).
//!
//! **Decoding** is behind the default-on `decode` feature (`default-features = false` leaves the
//! encoder-only crate). Today `Av1Decoder` reads the **framing and header layer** of a still —
//! the OBU walk (§5.3), the sequence header (§5.5), the uncompressed frame header (§5.9) and
//! tile-group framing (§5.11.1) — through `Av1Decoder::inspect`; sample decoding (tile body,
//! reconstruction, in-loop filters) is not implemented yet. It accepts `seq_profile = 1` (8-bit
//! 4:4:4) intra key frames only; the pixel-format matrix (10/12-bit, 4:2:0/4:2:2, monochrome),
//! intra block copy and film grain are refused with a typed error, each a ☐ row in
//! `gamut-avif/STATUS.md`.
//!
//! **Encoding**: M0 implemented the minimal path: a **lossless** all-intra keyframe — `seq_profile = 1`
//! (8-bit 4:4:4), identity matrix coefficients, full range, single tile, 64×64 superblocks,
//! `DC_PRED`, and the forced `TX_4X4` Walsh–Hadamard transform. The crate now also encodes
//! **lossy** all-intra keyframes: a wide intra mode set (the eight directional modes with
//! `angle_delta`, `SMOOTH`/`SMOOTH_V`/`SMOOTH_H`/`PAETH`, recursive filter-intra, chroma-from-luma,
//! and palette), DCT/ADST/identity transforms with variable `tx_depth` and `TX_SET_INTRA_2` type
//! selection, recursive partitioning, per-superblock delta-Q/delta-LF, segmentation, uniform
//! multi-tile, and the in-loop filters (deblocking, CDEF, loop restoration, superres). Symbols are
//! coded against **adapting CDFs** (`disable_cdf_update = 0`): each tile starts from the §9.4
//! defaults and nudges every context toward what it codes, which costs no fidelity and shrinks a
//! still by roughly 20–35%. It produces the AV1 temporal unit that `gamut-avif` wraps in an AVIF
//! still image.
//!
//! The colour signalling is selectable: [`encode_still_intra_with`] takes an [`Av1Colour`] (CICP
//! primaries/transfer/matrix plus the signal range) and mirrors it into `color_config()` and, via
//! [`EncodedStill::config`], the container's `av1C`/`colr` boxes. The caller supplies either GBR
//! planes (identity) or `Y/Cb/Cr` planes (see `gamut_color::Planar8::from_rgb8_matrix`).
//!
//! Chroma sampling comes from the [`Planar8`](gamut_color::Planar8) itself: 4:4:4, 4:2:2 and 4:2:0
//! are all coded. The identity matrix requires 4:4:4 (§6.4.2) and a subsampled identity encode is
//! refused. Under 4:2:2 the partition search drops `PARTITION_VERT`, since §6.10.4 forbids a block
//! whose chroma residual would be `BLOCK_INVALID`.
//!
//! **Bit depth** is the buffer's too: [`encode_still_intra16_with`] takes a
//! [`gamut_color::Planar16`] and codes at the depth it carries. Every depth-derived quantity
//! follows it: the quantizer tables, the dequant and inverse-transform clamps, the
//! `1 << (BitDepth - 1)` intra seeds, the palette's `L(BitDepth)` colours, the deblock centring and
//! thresholds, CDEF's `coeffShift`, and the Wiener rounding pair.
//!
//! `seq_profile` is a joint function of the two (Annex A.2): Main (0) for 4:2:0 and monochrome,
//! High (1) for 4:4:4, and Professional (2) for 4:2:2 — or for **any** layout at 12 bits, the only
//! profile that codes `twelve_bit`.
//!
//! **Encode speed** is a [`Speed`] preset ([`encode_still_intra_with_speed`],
//! [`encode_still_intra16_with_speed`]): each step prunes the lossy mode-decision search — the
//! angle refinement, filter-intra, the CfL alpha ladder, the transform-type set, rectangular and
//! sub-8×8 partitions, palette — to save encode time. The default, [`Speed::Thorough`],
//! is the full search, and every entry point without a `speed` argument uses it.
//!
//! **Tiling and threads** are [`EncodeOptions`] too ([`encode_still_intra_with_options`],
//! [`encode_still_intra16_with_options`]): a [`Tiling`] picks the uniform tile layout (log2 tile
//! columns and rows, clamped to what §5.9.15 and the level allow), and `threads` caps how many
//! tiles are coded concurrently, on `std::thread::scope` threads (one after another on `wasm32`).
//! Tiles are independent in intra coding, so the stream depends on the layout only, never on the
//! thread count. The default is the two-column layout the encoder has always used, on one thread.
//!
//! The remaining surface (quantizer matrices and the AVIF-level
//! alpha/metadata/container features) is tracked in `gamut-avif/STATUS.md`.
//!
//! Modules mirror the spec: `headers` = OBU framing + sequence/frame headers (AV1 §5.3/§5.5/§5.9),
//! `tile` = partition/prediction/coefficient coding (§5.11), [`transform`] = forward/inverse 2-D
//! transforms (§7.13), `cdf` = default CDF + scan + context tables and the adapting per-tile CDF
//! context (§9.2/§9.4/§8.2.6/§8.3.2),
//! [`quant`] = quantizer tables + dequant (§7.12), `filter` = in-loop filters (§7.14-§7.17).
#![forbid(unsafe_code)]

// Compiles README.md's code blocks as doctests; exists only while rustdoc collects them.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;

mod cdf;
#[cfg(feature = "decode")]
mod decode;
mod encoder;
mod filter;
mod geom;
mod headers;
pub mod quant;
mod tile;
pub mod transform;

#[cfg(feature = "decode")]
pub use decode::{
    Av1Decoder, CdefParams, ColorConfig, DecodeLimits, FrameHeader, LoopFilterParams, LrParams,
    QuantizationParams, RestorationType, SegmentationParams, SequenceHeader, StreamInfo,
    Subsampling, TileInfo, TxMode,
};
pub use encoder::{
    EncodeOptions, EncodedStill, ReconImage, Speed, encode_still_intra,
    encode_still_intra_superres, encode_still_intra_with, encode_still_intra_with_options,
    encode_still_intra_with_speed, encode_still_intra16_with, encode_still_intra16_with_options,
    encode_still_intra16_with_speed, encode_still_lossless_identity,
};
pub use headers::{Av1Colour, Av1StillConfig, Tiling};
