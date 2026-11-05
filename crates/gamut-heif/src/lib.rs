//! The shared HEIF/MIAF still-image layer under `gamut-avif` and `gamut-heic`.
//!
//! AVIF and HEIC are the same container — ISO/IEC 23008-12 HEIF, profiled by MIAF (ISO/IEC
//! 23000-22) — around different coded pictures. Everything outside the coded picture is this
//! crate's, written once:
//!
//! - **The item model.** [`HeifImage`] wraps a parsed [`gamut_isobmff::IsoBmffImage`] and reads
//!   roles off it — the primary item, its alpha and depth auxiliaries, thumbnails, Exif/XMP
//!   metadata, derivation sources — as computed lenses; [`HeifItem`] reads one item's properties,
//!   including its [`orientation`](HeifItem::orientation) as a [`gamut_core::Orientation`].
//! - **The derivation walk.** [`HeifImage::decode_item_planar`] resolves `iden` and `grid`
//!   derivations down to coded items, under a cycle and depth guard and the [`DecodeLimits`]
//!   size and work caps.
//! - **Presentation.** [`HeifImage::decode_item_rgba8`] and
//!   [`HeifImage::decode_item_rgba16`] add `iovl` compositing, YCbCr → RGB conversion with the
//!   chosen [`ChromaUpsampling`](gamut_color::ChromaUpsampling), alpha, and the `clap`/`irot`/
//!   `imir` transforms, under [`DecodeOptions`].
//! - **C2PA.** [`c2pa_manifest_stores`] and [`C2paSummary::from_segments`] locate and report the
//!   C2PA manifest stores among a file's top-level boxes — a locator, never a validator.
//!
//! # The codec binding
//!
//! Nothing here reads a coded picture. A codec crate implements [`Codestream`] once — which item
//! types are its own, how to parse and check their configuration, the colour its codestream
//! signals, and how to call one of its backends — and every decode surface is generic over it.
//! The backends themselves sit in a [`gamut_codec_abi::Registry`] of the codec's own typed
//! backend trait, so the registry's contract (push order; `supports` the only fall-through; an
//! accepting backend's error returned unchanged) is the one every format crate shares.
//!
//! Errors the pipeline raises on a binding's behalf carry that binding's
//! [`ORIGIN`](Codestream::ORIGIN), so a caller sees the format it decoded rather than this layer.
//!
//! # Example
//!
//! ```
//! use std::sync::{Arc, Mutex};
//!
//! use gamut_codec_abi::Registry;
//! use gamut_color::ChromaSubsampling;
//! use gamut_core::{Cicp, Dimensions, Result};
//! use gamut_heif::{AlphaChroma, Codestream, DecodeOptions, DecodedFrame, HeifImage};
//! use gamut_isobmff::{IsoBmffImage, Item, Property, PropertyKind};
//!
//! /// A toy codec: `tst1` items whose one-byte configuration is the gray level they decode to.
//! struct Toy;
//! struct Backend;
//!
//! impl Codestream for Toy {
//!     type Config = u8;
//!     type Backend = Backend;
//!     const ORIGIN: &'static str = "toy";
//!     const CONFIGURATION_BOX: [u8; 4] = *b"tstC";
//!     const ALPHA: AlphaChroma = AlphaChroma::Monochrome;
//!     fn is_coded_item(item_type: &[u8; 4]) -> bool {
//!         item_type == b"av01"
//!     }
//!     fn parse_config(data: &[u8]) -> Result<u8> {
//!         Ok(data.first().copied().unwrap_or(0))
//!     }
//!     fn validate_still_payload(_: &u8, _: &[u8]) -> Result<()> {
//!         Ok(())
//!     }
//!     fn signalled_colour(_: &u8, _: &[u8]) -> Result<Cicp> {
//!         Ok(Cicp { colour_primaries: 1, transfer_characteristics: 13,
//!                   matrix_coefficients: 1, full_range: true })
//!     }
//!     fn supports(_: &mut Backend, _: &u8) -> bool {
//!         true
//!     }
//!     fn decode(_: &mut Backend, gray: &u8, _: &[u8]) -> Result<DecodedFrame> {
//!         let size = Dimensions::new(2, 2)?;
//!         let y = vec![u16::from(*gray); 4];
//!         DecodedFrame::new(size, 8, ChromaSubsampling::Cs400, y, vec![], vec![])
//!     }
//! }
//!
//! let file = IsoBmffImage::new(*b"avif", vec![*b"mif1"], 1, vec![
//!     Item::new(1, *b"av01")
//!         .with_properties(vec![Property::new(true, PropertyKind::CodecConfiguration {
//!             kind: *b"tstC",
//!             data: vec![200],
//!         })])
//!         .with_payload(vec![0]),
//! ]);
//! let image = HeifImage::new(file).unwrap();
//!
//! let mut backends: Registry<Backend> = Registry::new();
//! backends.push(Arc::new(Mutex::new(Backend)));
//! let rgba = image
//!     .decode_item_rgba8::<Toy>(1, &backends, DecodeOptions::default())
//!     .unwrap();
//! assert_eq!(rgba.as_samples()[..4], [200, 200, 200, 255]);
//! ```
#![forbid(unsafe_code)]

mod c2pa;
mod codestream;
mod convert;
mod decode;
mod frame;
mod image;
mod limits;

pub use c2pa::{
    C2PA_NOT_VALIDATED, C2paBoxPosition, C2paManifestStore, C2paMerkleBox, C2paPresence,
    C2paStoreSummary, C2paSummary, C2paUnreadBox, c2pa_manifest_stores,
};
pub use codestream::{AlphaChroma, BACKEND_POISONED, Codestream, NO_BACKEND};
pub use frame::DecodedFrame;
// The C2PA `ContentProvenanceBox` framing is `gamut-isobmff`'s.
pub use gamut_isobmff::{C2PA_UUID, C2paBoxPurpose, C2paUnreadReason};
pub use image::{
    CleanAperture, ContentLightLevel, HeifImage, HeifItem, ItemKind, MasteringDisplay,
    PixelAspectRatio, TransformativeProperty,
};
pub use limits::{
    DECODED_ITEM_LIMIT, DERIVED_ITEM_LIMIT, DIMENSION_LIMIT, DecodeLimits, DecodeOptions,
    PIXEL_LIMIT,
};

/// Executable laws and the stub codec binding they decode through, for the property tests and
/// an out-of-tree fuzz driver (`test-support`). Not part of the stable API; see
/// `docs/testing.md`.
#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub mod invariants;
