//! `gamut-ifd` — the TIFF Image File Directory (IFD) container core.
//!
//! TIFF's structural spine — an 8-byte byte-order header (II/MM mark + magic `42` + offset of the
//! first IFD) followed by a chain of IFDs, each a list of 12-byte tag entries holding values inline
//! (when ≤ 4 bytes) or at a file offset — is **not** unique to the TIFF image codec. **EXIF is a
//! constrained profile of exactly this structure** (an `Exif\0\0` marker then a TIFF stream), so
//! this crate factors the IFD container out as a shared primitive that both
//! [`gamut-exif`](https://crates.io/crates/gamut-exif) (issue #34) and
//! [`gamut-tiff`](https://crates.io/crates/gamut-tiff) (issue #107) build on. It models only the
//! structure (byte order, field types, values, IFD chains, offset layout) — never pixels,
//! compression, or photometry, which stay in the codec. It names the codes those tags carry
//! ([`tags::Compression`] and its three siblings) so the codecs spell them alike; what a code
//! does to the pixels, and whether a codec supports it, is still the codec's.
//!
//! Structure follows **TIFF 6.0** (`references/tiff/tiff6.pdf`, Adobe/Aldus, Final — June 3 1992,
//! §2). [`read`] / [`read_header`] parse a stream into a [`TiffFile`]; [`write()`] serialises one
//! back, laying out the IFD chain and out-of-line value pool with the two-pass offset machinery.
//! A codec storing strips or tiles after the tree hands them to [`write_with_blocks`], and a
//! decoder resolves them back with [`extent_slices`] and [`rows_per_strip`].
//! [`IfdReader::read_tree`] is `write`'s inverse over sub-IFD trees (given the pointer tags — the
//! well-known structural ones live in [`tags`]); [`resolve_subtree`] runs the same guarded walk
//! over **one** directory a caller already holds, scoped per level by a [`SubtreeScope`]. The
//! same [`tags`] module holds the shared tag registry — each tag TIFF 6.0 and DNG 1.7.1.0 define,
//! with its name, permitted field types and component count — that gamut-tiff, gamut-dng and
//! gamut-exif read rather than each keeping a copy, and the value codes of the `Compression`,
//! `PhotometricInterpretation`, `Predictor` and `SampleFormat` tags. The
//! one cross-format payload a TIFF-based file carries by tag — the C2PA manifest store — has its
//! placement rule and exclusion ranges stated once, for every TIFF-based codec to share (see
//! [C2PA](#c2pa-manifest-store) below). For strict archival decoding, [`audit`] classifies **every byte** of a stream into typed
//! segments ([`SegmentReport`]) under a dual-ledger cross-check: a [`Tracked`] source records what
//! the parse physically read, and the claims must match — byte accounting as a machine-checked
//! proof, not a promise (issue #263). A codec layers its own tag knowledge on top and reports it
//! in the shared [`DeconstructReport`] vocabulary ([`Anomaly`], [`Severity`], [`UnknownTag`],
//! [`UnknownFieldType`]), so gamut-tiff and gamut-dng report alike.
//!
//! ## Streaming
//!
//! The slice readers want the whole file in memory; a multi-hundred-MB camera RAW does not fit
//! that shape, and its directory structure is kilobytes. [`IfdReader`] walks the same structure
//! lazily over any [`ReadAt`] source — a slice, a [`StreamSource`] (`Read + Seek`, e.g. a
//! file), or a [`Rebased`] offset-shifted view, which is the **maker-note primitive**: vendor
//! mini-IFDs address their values relative to the note start (or the enclosing TIFF header),
//! so the note is parsed as a directory over a rebased view of the same source:
//!
//! ```
//! use gamut_ifd::{ByteOrder, Ifd, IfdReader, ReadAt, TiffFile, Value, Variant, write};
//!
//! // A "maker note": a little TIFF stream embedded at offset 1000 of a larger file, its
//! // internal offsets relative to its own start.
//! let mut note = Ifd::new();
//! note.set(1, Value::Ascii("vendor mode".to_owned()));
//! let note_bytes = write(&TiffFile {
//!     order: ByteOrder::LittleEndian,
//!     variant: Variant::Classic,
//!     ifds: vec![note.clone()],
//! }).unwrap();
//! let mut file = vec![0u8; 1000];
//! file.extend_from_slice(&note_bytes);
//!
//! let mut reader = IfdReader::open((&file[..]).rebased(1000)).unwrap();
//! let raw = reader.read_ifd(reader.first_ifd_offset()).unwrap(); // one directory body
//! let value = reader.value(raw.entry(1).unwrap()).unwrap(); // one value, on demand
//! assert_eq!(value, Value::Ascii("vendor mode".to_owned()));
//! ```
//!
//! ## C2PA manifest store
//!
//! C2PA 2.4 §A.3.6 embeds a manifest store in a TIFF-based file as the `UNDEFINED` value of tag
//! 52545 ([`C2PA_MANIFEST_STORE`]) in the **last IFD of the main chain**, its bytes at the end of
//! the file so a store of another size moves no other offset; §18.5.5 has a signer hash around
//! two ranges, the store and its entry's `count` field ([`C2paExclusions`]). The writer side is
//! [`reserve_c2pa_entry`] (a one-byte placeholder in the directory that will be written last)
//! then [`append_c2pa_store`] (the store appended and the entry re-pointed); the reader side is
//! [`c2pa_exclusions`] (the ranges a signer needs, or absence) and [`c2pa_summary`] (a
//! non-validating report that also lists every entry that yields no store, and why). The store
//! is opaque bytes throughout: nothing here parses JUMBF or reaches a validity verdict
//! ([`C2PA_NOT_VALIDATED`]).
//!
//! ## BigTIFF
//!
//! The `bigtiff` cargo feature adds BigTIFF support (`references/tiff/bigtiff.html`): the
//! [`Variant::Big`] container with 64-bit offsets/counts and the [`FieldType::Long8`] /
//! `SLong8` / `Ifd8` field types. It is additive and off by default, so classic-only consumers
//! (e.g. EXIF metadata) stay lean; the TIFF codec enables it.
//!
//! ```
//! use gamut_ifd::{ByteOrder, Ifd, TiffFile, Value, Variant, read, write};
//!
//! let mut ifd = Ifd::new();
//! ifd.set(256, Value::Short(vec![640])); // ImageWidth
//! ifd.set(257, Value::Short(vec![480])); // ImageLength
//! let file = TiffFile { order: ByteOrder::LittleEndian, variant: Variant::Classic, ifds: vec![ifd] };
//! let bytes = write(&file).unwrap();
//! assert_eq!(read(&bytes).unwrap(), file);
//! ```
#![forbid(unsafe_code)]

// Compiles README.md's code blocks as doctests; exists only while rustdoc collects them.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;

mod audit;
mod blocks;
mod byte_order;
mod c2pa;
mod entry;
mod extent;
// Executable laws over this crate's own types, shared by the property tests and the fuzz tier
// (`docs/testing.md`). Always available to the crate's own tests; `test-support` exposes it to an
// out-of-tree fuzz driver. Never enabled by the `gamut` umbrella, so it stays out of the shipped
// surface and out of `check-ffi-features`. `doc(hidden)` because it carries **no SemVer
// guarantee**: it is gamut's own harness, not API for consumers to pin.
#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub mod invariants;
mod reader;
mod segment;
mod source;
mod stream;
pub mod tags;
mod track;
mod types;
mod value;
mod writer;

pub use audit::{
    Anomaly, Audit, AuditFinding, AuditOutcome, AuditSpec, Auditor, DeconstructReport, Severity,
    SkipReason, StandardAuditSpec, UnknownFieldType, UnknownTag, audit, standard_data_extents,
};
pub use blocks::{BlockRun, PlacedRun, write_with_blocks};
pub use byte_order::ByteOrder;
pub use c2pa::{
    C2PA_MANIFEST_STORE, C2PA_NOT_VALIDATED, C2paExclusions, C2paStoreSummary, C2paSummary,
    C2paUnreadEntry, C2paUnreadReason, append_c2pa_store, c2pa_exclusions, c2pa_summary,
    reserve_c2pa_entry,
};
pub use entry::{Field, Ifd, SubIfd, Variant};
pub use extent::{ZeroRowsPerStrip, extent_slices, rows_per_strip};
pub use reader::{
    AuditedFile, Header, SubtreeScope, TiffFile, read, read_audited, read_header, read_ifd_at,
    resolve_subtree,
};
pub use segment::{
    Claim, Conflict, DataLabel, Segment, SegmentMap, SegmentReport, SharedSpan, SpanKind,
};
pub use source::{ReadAt, Rebased, StreamSource};
pub use stream::{ChainedIfd, DecodedIfd, IfdChain, IfdReader, RawEntry, RawIfd};
pub use track::{ReadLedger, Tracked, TrackedParts};
pub use types::FieldType;
pub use value::{UnknownValue, Value};
pub use writer::{WriteOptions, WriteOutput, align_word, write, write_with};
