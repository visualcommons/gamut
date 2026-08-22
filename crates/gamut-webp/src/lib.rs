//! WebP image encoder and decoder — an intra-frame VP8/VP8L still-image bitstream wrapped in a
//! RIFF container.
//!
//! The public surface mirrors [`gamut-avif`](https://docs.rs/gamut-avif): a [`WebpEncoder`]
//! implementing [`gamut_core::EncodeImage`] and a [`WebpDecoder`] implementing
//! [`gamut_core::DecodeImage`].
//! The container layer is [`gamut_riff`]; the codec layer is the [`vp8l`] (lossless, RFC 9649 §3)
//! and [`vp8`] (lossy intra, RFC 6386) module trees, whose modules each cite the spec section they
//! implement. The implementation status and milestones are tracked in `STATUS.md`.
//!
//! gamut is image-first, so only the intra/key-frame still-image subset of VP8 is in scope (no
//! inter-frame prediction, motion, or sequences). Both codecs are fully implemented, for
//! [`Rgb8`](gamut_core::Rgb8) and [`Rgba8`](gamut_core::Rgba8) input: **VP8L lossless**
//! (every transform, LZ77, the color cache, meta prefix codes) and **VP8 lossy** key-frame intra
//! (DC/V/H/TM and per-4×4 B_PRED prediction, the simple and normal loop filters, segmentation, 1/2/4/8
//! token partitions, and skip). Transparent lossy images use the extended (`VP8X`) container with an
//! `ALPH` alpha chunk. Every component is validated against libwebp as an oracle in both directions
//! (bit-exact at the YUV-plane level for lossy), plus a malformed-input robustness corpus.
//!
//! # Limitations
//!
//! The crate codes the single still image. Some container features are deliberately deferred or out
//! of scope (see `STATUS.md` for the full matrix):
//!
//! - **Unknown-chunk passthrough** — a chunk whose FourCC the container spec does not define is
//!   ignored on decode, as RFC 9649 §2.7.1.6 asks of readers. Preserving one across a
//!   decode→encode cycle is opt-in and takes two steps: read the chunks with
//!   [`gamut_riff::WebpLayout::parse`] and hand them back via
//!   [`WebpEncoder::with_unknown_chunks`]. The pixel API alone does not thread them through.
//! - **Animation** — `ANIM` / `ANMF` multi-frame sequences are out of scope under the image-first
//!   charter. Each frame is an independent key frame, but assembling them needs a non-trait API.
//! - **Lossy quality** — the `0..=100` quality maps coarsely onto the VP8 base quantizer. The
//!   encoder has no rate-distortion mode search, so quality is a quantizer dial rather than a
//!   rate target; [`Effort`] tunes how hard it looks within that dial.
//! - **Lossless** — reproduces the input exactly and ignores the quality value, unless
//!   [`NearLossless`] preprocessing is configured (see below).
//!
//! # Compression effort and near-lossless
//!
//! [`WebpEncoder::with_effort`] takes an [`Effort`] — libwebp's `method` dial, `0..=6`, defaulting
//! to `4`. It applies to both codestreams and never changes what the format guarantees: lossless
//! stays bit-exact at every rung and lossy keeps its quality target. Higher rungs simply search
//! harder. On the lossless side the rungs race candidate encodings — transform chain, palette
//! ordering, colour-cache size, entropy grouping, LZ77 depth — and keep the smallest; each rung's
//! candidates are a superset of the rung below's, so **output size is non-increasing in effort by
//! construction**. On the lossy side the rungs add 4×4 prediction search, coefficient
//! probabilities derived from the coded frame, a measured skip probability, and a dead-zone
//! quantizer.
//!
//! Effort buys size with time. Measured on a 256×256 gradient: lossless takes ~9 ms at rung 0,
//! ~156 ms at the default rung 4 and ~290 ms at rung 6; lossy is far flatter, ~2.4 ms at rung 0
//! against ~3.3 ms at rung 4. Rungs 0–2 are the escape hatch when encode time matters more than
//! bytes.
//!
//! [`WebpEncoder::with_near_lossless`] takes a [`NearLossless`] strength on libwebp's scale. It
//! rounds the source's colour channels to a coarser grid **before** lossless coding, so the coded
//! stream is still bit-exact — to that quantized image. Red, green and blue move by at most
//! [`NearLossless::max_deviation`]; alpha is never touched. Turning it on can never make a file
//! larger, because the encoder codes both ways and keeps the smaller.
//!
//! # Embedded metadata
//!
//! The three metadata chunks the container defines round-trip **verbatim**: an ICC colour profile
//! (`ICCP`) plus Exif and XMP metadata (`EXIF` / `XMP `). [`WebpEncoder::with_icc_profile`],
//! [`WebpEncoder::with_exif`], and [`WebpEncoder::with_xmp`] embed them — promoting a simple file to
//! the extended (`VP8X`) format, setting the matching feature flags, and emitting the chunks in the
//! spec's canonical order — and the [`metadata`] free function reads them back out of any WebP file
//! without decoding pixels. Payloads are never parsed or reserialized here, so they can be borrowed
//! straight into `gamut-metadata`'s `MetadataBlock` (the still-image [`gamut_core`] traits carry no
//! metadata channel, which is why this is a separate entry point rather than a decode result field).
//!
//! # Pluggable codestream backends
//!
//! The RIFF container and the coded picture are separable: [`backend`] exposes one trait pair —
//! [`WebpCodestreamDecoder`] / [`WebpCodestreamEncoder`], discriminated by [`WebpCodestream`] — that
//! routes a raw `VP8 ` / `VP8L` chunk payload to a hardware or alternate software codec, installed
//! with [`WebpDecoder::push_backend`] / [`WebpEncoder::push_backend`]. The crate's own `vp8`/`vp8l`
//! implementations are the implicit tails, so the default behaviour is unchanged. Backends written
//! against the shared [`gamut_codec_abi`] seam (issue #241) plug in through [`AbiDecoderBackend`] /
//! [`AbiEncoderBackend`].
//!
//! The effort hint rides the typed seam ([`WebpEncodeRequest::effort`]) but deliberately does not
//! cross the codec ABI: it cannot change what a codestream decodes to, so a backend that ignores it
//! is still correct. Near-lossless is applied host-side before dispatch, so a backend simply
//! receives already-quantized pixels.
#![forbid(unsafe_code)]

mod config;
mod decoder;
mod encoder;
mod metadata;

pub mod alpha;
pub mod backend;
pub mod vp8;
pub mod vp8l;

pub use backend::{
    AbiDecoderBackend, AbiEncoderBackend, CodestreamInfo, DecodedRaster, PIXEL_FORMAT_ARGB,
    PIXEL_FORMAT_YUV420, RasterRef, WebpCodestream, WebpCodestreamDecoder, WebpCodestreamEncoder,
    WebpEncodeRequest,
};
pub use config::{Effort, NearLossless, WebpConfig, WebpMode};
pub use decoder::WebpDecoder;
pub use encoder::WebpEncoder;
pub use gamut_core::Dimensions;
pub use metadata::{WebpMetadata, metadata};
