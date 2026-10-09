//! `gamut-metadata` — the unified image-metadata facade.
//!
//! Brings the per-format metadata crates — [`exif`], [`xmp`], [`icc`], and [`iptc`] — under one
//! [`Metadata`] model and one extract/embed surface. A container crate (e.g.
//! [`gamut_isobmff`](https://crates.io/crates/gamut-isobmff) for AVIF/HEIC,
//! [`gamut_riff`](https://crates.io/crates/gamut-riff) for WebP) locates the metadata payloads in a
//! file; this facade turns those payloads into typed models and back. It stays
//! **container-agnostic** — it consumes already-located [`MetadataBlock`]s and produces owned
//! [`EncodedMetadata`] blocks, never boxes or chunks — and **orchestration-only**: it holds the leaf
//! crates' types by value and delegates all parsing and serialization to them.
//!
//! # The model: one carrier, one field
//!
//! [`Metadata`] has exactly one field per genuinely distinct serialization a container holds —
//! [`exif`](Metadata::exif), [`xmp`](Metadata::xmp), [`icc`](Metadata::icc),
//! [`c2pa`](Metadata::c2pa). **IPTC has no field of its own:** IPTC Photo Metadata *is* XMP
//! (properties in the `dc:`/`photoshop:`/`Iptc4xmp*` namespaces), so it lives inside
//! [`xmp`](Metadata::xmp), read back through the
//! [`Metadata::iptc`] lens. The one genuinely separate IPTC carrier — the legacy binary IIM block —
//! is reconciled *into* the XMP graph on [extraction](MetadataExtractor) (with a
//! [`ConflictPolicy`]) and projected back out only on request when [embedding](MetadataEmbedder).
//! Because each datum is stored once, the extract→embed→extract round-trip is a true equality —
//! with one documented exception, [`c2pa`](Metadata::c2pa), below.
//!
//! The fourth carrier is [`c2pa`](Metadata::c2pa), the C2PA manifest store: opaque bytes in, opaque
//! bytes out, never parsed here. It is the one documented exception to that round-trip equality —
//! see [C2PA, below](#c2pa-a-carrier-that-must-not-be-copied-forward).
//!
//! # Extensions: data with no carrier
//!
//! A downstream typed model is usually wider than what a still-image file can carry — sensor
//! geometry, container-level facts, structs it derives itself. [`Metadata::extensions`] is a
//! namespaced table for exactly that residue, so such a model survives
//! `their model → Metadata → their model` intact instead of being silently narrowed to three
//! carriers. Each entry is a [`MetadataExtension`]: a namespace the caller owns, a key, and a
//! value in the same TIFF/IFD [`Value`](exif::Value) model gamut's metadata crates already use.
//!
//! Two guarantees, deliberately distinct:
//!
//! - **Model round-trip.** Everything in a [`Metadata`] — carriers *and* extensions — survives
//!   being handed to another model and back. This is what extensions are for.
//! - **Carrier round-trip** (the keystone). extract → embed → extract is a true equality over
//!   [`exif`](Metadata::exif)/[`xmp`](Metadata::xmp)/[`icc`](Metadata::icc) —
//!   [`c2pa`](Metadata::c2pa) excepted, for the reason [below](#c2pa-a-carrier-that-must-not-be-copied-forward).
//!   Extensions take no part: extraction never produces one, and [`Metadata::encode`] drops them
//!   (or fails, under [`ExtensionPolicy::Reject`]).
//!
//! **Prefer a carrier whenever one exists** — only a carrier reaches the file. An unmodelled EXIF
//! tag, MakerNote included, already round-trips inside [`Metadata::exif`] because
//! [`Exif`](exif::Exif) retains the raw [`Ifd`](exif::Ifd); any property round-trips inside
//! [`Metadata::xmp`] because the XMP graph is open; an unmodelled ICC element round-trips inside
//! [`Metadata::icc`] as `TagData::Raw`. Reach for an extension only when no carrier can hold the
//! datum at all.
//!
//! ```
//! use gamut_metadata::{ExtensionPolicy, Metadata, MetadataEmbedder, MetadataError};
//! use gamut_metadata::exif::Value;
//!
//! // A downstream model parks a sensor fact no still-image carrier expresses.
//! let mut meta = Metadata::default();
//! meta.set_extension("com.example.raw", "WhiteLevel", Value::Long(vec![16_383]));
//! assert_eq!(
//!     meta.extension("com.example.raw", "WhiteLevel"),
//!     Some(&Value::Long(vec![16_383]))
//! );
//!
//! // Embedding cannot carry it — by default it is dropped...
//! assert_eq!(meta.encode()?, Metadata::default().encode()?);
//!
//! // ...and a caller that must not lose it silently can say so.
//! let rejected = MetadataEmbedder::new()
//!     .extension_policy(ExtensionPolicy::Reject)
//!     .embed(&meta);
//! assert!(matches!(
//!     rejected,
//!     Err(MetadataError::UnembeddableExtension { .. })
//! ));
//! # Ok::<(), gamut_metadata::MetadataError>(())
//! ```
//!
//! # C2PA: a carrier that must not be copied forward
//!
//! [`Metadata::c2pa`] holds a C2PA manifest store exactly as a container found it — the JUMBF
//! superbox of C2PA 2.4 §11.1.4.2 — and the facade never looks inside it.
//!
//! It is a **carrier**, not an [extension](#extensions-data-with-no-carrier). Extensions exist for
//! data no file holds, so extraction never produces one and nothing serializes them; a manifest
//! store is the opposite on the first count — it comes *out* of a file — which is exactly what the
//! [one carrier, one field](#the-model-one-carrier-one-field) rule is for.
//!
//! It is also the one **exception to the keystone**. A standard manifest binds to its asset with
//! exactly one hard binding (§9.1): a digest over the finished file, computed with the store's own
//! byte range excluded (§15.12.1.1) and covering the asset's other metadata (§9.2.6). Re-encoding
//! the image — or any metadata-only rewrite that moves a byte — invalidates it. So embedding
//! **never** re-emits the store: [`Metadata::encode`] drops it, or fails under
//! [`C2paPolicy::Reject`], and `extract → embed → extract` deliberately loses it. C2PA's model for
//! a derivative asset is a *new* manifest carrying the parent as an ingredient, not the parent's
//! signature laundered onto different bytes; producing one is signing work, outside this crate.
//!
//! ```
//! use gamut_metadata::{C2paPolicy, Metadata, MetadataBlock, MetadataEmbedder, MetadataError};
//!
//! // A container located a manifest store; extraction carries the bytes through untouched.
//! let store = b"\0\0\0\x14jumbc2pa".to_vec();
//! let meta = Metadata::from_blocks(&[MetadataBlock::C2pa(&store)])?;
//! assert_eq!(meta.c2pa.as_deref(), Some(&store[..]));
//!
//! // Embedding refuses to launder it into a rewritten file — quietly by default...
//! assert_eq!(meta.encode()?.c2pa, None);
//!
//! // ...or loudly, for a caller that must notice provenance is being dropped.
//! let rejected = MetadataEmbedder::new()
//!     .c2pa_policy(C2paPolicy::Reject)
//!     .embed(&meta);
//! assert!(matches!(rejected, Err(MetadataError::UnembeddableC2pa { .. })));
//! # Ok::<(), gamut_metadata::MetadataError>(())
//! ```
//!
//! Deferred deliberately: parsing the JUMBF interior, and any manifest validation, signing, or
//! ingredient authoring — all of which need a trust model this facade does not have.
//!
//! # Provenance: embedded, remote, both, or none
//!
//! An embedded store is not the only way a file carries provenance. C2PA 2.4 §11.5 recommends that
//! a claim generator whose manifest lives *externally* add a `dcterms:provenance` URL to the asset's
//! XMP, and §15.5.3.1 lists it among the places a validator looks when nothing is embedded. A caller
//! asking "does this image have Content Credentials?" therefore needs more than `c2pa.is_some()`;
//! [`Metadata::provenance`] answers with a [`ProvenanceState`] that keeps the two sources apart —
//! [`None`](ProvenanceState::None), [`Remote`](ProvenanceState::Remote),
//! [`Embedded`](ProvenanceState::Embedded), or [`EmbeddedAndRemote`](ProvenanceState::EmbeddedAndRemote)
//! — because the key is reserved for external manifests (§11.5) yet a file may carry both, and the
//! lens reports what the file carries. The URL is reported as found; **gamut never
//! resolves it**, and the HTTP `Link` header route (§15.5.3.2) is out of scope for a file-format
//! library — see the [`provenance`] module for both.
//!
//! ```
//! use gamut_metadata::{Metadata, MetadataBlock, ProvenanceState};
//! use gamut_metadata::xmp::{WellKnownNs, XmpMeta};
//!
//! // A file with no embedded manifest store, whose XMP points at an external one.
//! let mut graph = XmpMeta::new();
//! graph.set_text(
//!     WellKnownNs::DcTerms.uri(),
//!     "provenance",
//!     "https://example.com/manifests/photo.c2pa",
//! );
//! let packet = graph.to_packet();
//!
//! let meta = Metadata::from_blocks(&[MetadataBlock::Xmp(&packet)])?;
//! assert_eq!(meta.c2pa, None); // nothing embedded...
//! assert_eq!(
//!     meta.provenance().remote_url(), // ...yet not "no provenance"
//!     Some("https://example.com/manifests/photo.c2pa")
//! );
//! assert!(matches!(meta.provenance(), ProvenanceState::Remote(_)));
//! # Ok::<(), gamut_metadata::MetadataError>(())
//! ```
//!
//! # Which formats carry what
//!
//! The facade never parses a container, so it cannot say whether a *file* has metadata — but it can
//! say whether gamut's crate for a format can locate or write a given carrier at all, before a
//! caller pulls that crate in. [`capability::supports`] answers per (format × carrier × direction),
//! and [`capability::typed_wiring`] says whether the format crate also exposes these typed models
//! directly (behind its `metadata` feature) rather than as raw bytes:
//!
//! ```
//! use gamut_metadata::capability::{Carrier, Direction, Format, supports, typed_wiring};
//!
//! assert!(supports(Format::Jpeg, Carrier::Exif, Direction::Write));
//! assert!(!supports(Format::Heic, Carrier::Exif, Direction::Write)); // decode-only crate
//! assert!(typed_wiring(Format::Jpeg));
//! ```
//!
//! # Quick start
//!
//! ```
//! use gamut_metadata::{Metadata, MetadataBlock};
//! use gamut_metadata::xmp::{WellKnownNs, XmpMeta};
//!
//! // A container crate located an XMP packet; build one here for the example.
//! let mut graph = XmpMeta::new();
//! graph.set_text(WellKnownNs::Photoshop.uri(), "City", "Kyoto");
//! let packet = graph.to_packet();
//!
//! // Extract a unified model from the located blocks...
//! let meta = Metadata::from_blocks(&[MetadataBlock::Xmp(&packet)])?;
//! assert_eq!(meta.iptc().unwrap().city(), Some("Kyoto"));
//!
//! // ...then serialize back to per-carrier byte blocks for a container to embed.
//! let blocks = meta.encode()?;
//! assert!(blocks.xmp.is_some());
//! # Ok::<(), gamut_metadata::MetadataError>(())
//! ```
#![forbid(unsafe_code)]

pub mod capability;
pub mod embed;
pub mod error;
pub mod extension;
pub mod extract;
pub mod metadata;
pub mod provenance;
pub mod source;

// Re-export the per-format crates so consumers reach everything through one entry point.
pub use embed::{C2paPolicy, EncodedMetadata, ExtensionPolicy, MetadataEmbedder};
pub use error::{MetadataError, Result};
pub use extension::{MetadataExtension, RESERVED_NAMESPACE_PREFIX};
pub use extract::MetadataExtractor;
pub use gamut_exif as exif;
pub use gamut_icc as icc;
pub use gamut_iptc as iptc;
// Surface the IPTC reconciliation knobs at the facade level so callers configure extraction without
// reaching into the `iptc` submodule.
pub use gamut_iptc::{ConflictPolicy, FieldConflict};
pub use gamut_xmp as xmp;
pub use metadata::Metadata;
pub use provenance::ProvenanceState;
pub use source::MetadataBlock;
