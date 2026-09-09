//! Resource Interchange File Format (RIFF) utilities — the chunked container used by WebP.
//!
//! This crate owns the WebP *container*, not the codec: it reads and writes the RIFF chunk
//! structure (`RIFF`/`WEBP` plus `VP8 `/`VP8L`/`VP8X`/… chunks) and leaves the VP8/VP8L bitstream to
//! [`gamut-webp`](https://docs.rs/gamut-webp), mirroring how `gamut-isobmff` backs AVIF/HEIC.
//!
//! Byte layouts follow RFC 9649 (*WebP Image Format*) §2 and the Google *WebP Container*
//! specification in `references/webp/`; the crate's `STATUS.md` ledgers the v1 surface, and
//! `gamut-webp/STATUS.md` section A is the per-requirement conformance table this crate owns.
//! Metadata chunks (`ICCP`/`EXIF`/`XMP `) are carried verbatim through [`MetadataChunks`] and
//! [`write_extended_with_metadata`], never parsed or reserialized, as is the `C2PA` manifest store
//! of C2PA 2.4 §A.3.7 — the one carrier RFC 9649 does not define, placed last as §A.3.7 requires
//! and located by [`c2pa_span`].
//!
//! # Reading
//!
//! Three readers, in increasing strictness — pick the one the job needs:
//!
//! | Reader | Yields | Rejects |
//! | ------ | ------ | ------- |
//! | [`RiffReader`] | every chunk, in file order | only what it cannot frame |
//! | [`MetadataChunks::read`] | the `ICCP`/`EXIF`/`XMP `/`C2PA` payloads | malformed framing |
//! | [`WebpLayout::parse`] | every chunk sorted into its role | + chunks out of the spec's order |
//!
//! Animation (`ANIM`/`ANMF`) is out of scope: the FourCCs are recognised, so an animated file is
//! reported as unsupported rather than mis-parsed as a still image.
//!
//! # Example
//!
//! ```
//! use gamut_riff::{RiffReader, WebpChunkId, write_simple_lossless};
//!
//! let file = write_simple_lossless(&[0x2f, 0x01, 0x02])?;
//! let chunk = RiffReader::new(&file)?.next().unwrap()?;
//! assert_eq!(WebpChunkId::from(chunk.fourcc), WebpChunkId::Vp8l);
//! assert_eq!(chunk.payload, &[0x2f, 0x01, 0x02]);
//! # Ok::<(), gamut_core::Error>(())
//! ```
#![forbid(unsafe_code)]

mod chunk;
mod fourcc;
mod reader;
mod webp;
mod writer;

pub use chunk::Chunk;
pub use fourcc::FourCc;
pub use reader::RiffReader;
pub use webp::{
    C2PA_FOURCC, MAX_CANVAS_DIMENSION, MetadataChunks, VP8X_PAYLOAD_LEN, Vp8xHeader, WebpChunkId,
    WebpLayout, c2pa_span, write_extended, write_extended_preserving, write_extended_with_metadata,
    write_simple_lossless, write_simple_lossy,
};
pub use writer::RiffWriter;
