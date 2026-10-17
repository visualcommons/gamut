//! The one shape every format crate's decoded-metadata type exposes the facade through.
//!
//! A format crate that wires this facade in (see the crate README's *Consumer integration*)
//! implements [`TypedMetadata`] on its decoded-metadata type rather than inventing accessors of its
//! own, so a caller can write one function over every wired format:
//!
//! ```
//! use gamut_metadata::{Metadata, MetadataBlock, Result, TypedMetadata};
//!
//! /// Anything that already holds its payloads as bytes — as a JPEG or JPEG XL reader does.
//! struct Located {
//!     xmp: Vec<u8>,
//! }
//!
//! impl TypedMetadata for Located {
//!     fn blocks(&self) -> Result<Vec<MetadataBlock<'_>>> {
//!         Ok(vec![MetadataBlock::Xmp(&self.xmp)])
//!     }
//! }
//!
//! fn has_xmp(source: &dyn TypedMetadata) -> Result<bool> {
//!     Ok(source.metadata()?.xmp.is_some())
//! }
//!
//! let mut graph = gamut_metadata::xmp::XmpMeta::new();
//! graph.set_text(gamut_metadata::xmp::WellKnownNs::Xmp.uri(), "CreatorTool", "gamut");
//! assert!(has_xmp(&Located { xmp: graph.to_packet() })?);
//! # Ok::<(), gamut_metadata::MetadataError>(())
//! ```
//!
//! Both methods are fallible and both report [`MetadataError`](crate::MetadataError), by
//! construction rather than per crate. [`blocks`](TypedMetadata::blocks) is fallible because
//! *locating* a payload can fail in a container that resolves it lazily — an ISOBMFF item is an
//! extent and an offset, not bytes — and that failure is
//! [`MetadataError::Container`](crate::MetadataError::Container), carrying the format crate's own
//! [`gamut_core::Error`]. A crate whose payloads are already bytes simply never returns it; the
//! `?` it costs a caller there is the price of one signature for every format, which is what lets
//! the trait be used as `dyn TypedMetadata` at all.

use crate::error::Result;
use crate::metadata::Metadata;
use crate::source::MetadataBlock;

/// A decoded-metadata type that hands its located payloads to the facade.
///
/// Object-safe: a caller may hold any wired format's metadata as `&dyn TypedMetadata`. Implement
/// [`blocks`](Self::blocks); [`metadata`](Self::metadata) defaults to
/// [`Metadata::from_blocks`] over it, and is overridden only by a type that holds a carrier as an
/// already-typed model rather than as bytes (as `gamut-dng`'s `DngMetadata` holds its EXIF
/// sub-IFD).
pub trait TypedMetadata {
    /// The located payloads as [`MetadataBlock`]s — already normalised to the boundaries
    /// [`MetadataBlock`] documents (an EXIF block is the bare TIFF stream, an ICC block the
    /// reassembled profile) — ready for [`Metadata::from_blocks`] or a
    /// [`MetadataExtractor`](crate::MetadataExtractor) with a chosen
    /// [`ConflictPolicy`](crate::ConflictPolicy). Each carrier appears only when the file holds
    /// it.
    ///
    /// # Errors
    ///
    /// Returns [`MetadataError::Container`](crate::MetadataError::Container) when the container
    /// holds a carrier whose payload cannot be located; no carrier is parsed here.
    fn blocks(&self) -> Result<Vec<MetadataBlock<'_>>>;

    /// Parses the located payloads into the unified [`Metadata`] model. The default is
    /// [`Metadata::from_blocks`] over [`blocks`](Self::blocks).
    ///
    /// # Errors
    ///
    /// Returns whatever [`blocks`](Self::blocks) returns, then the
    /// [`MetadataError`](crate::MetadataError) naming the carrier whose parse failed.
    fn metadata(&self) -> Result<Metadata> {
        Metadata::from_blocks(&self.blocks()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MetadataError;

    /// Hands back a fixed block list, or a container failure.
    struct Fixed(core::result::Result<Vec<u8>, &'static str>);

    impl TypedMetadata for Fixed {
        fn blocks(&self) -> Result<Vec<MetadataBlock<'_>>> {
            match &self.0 {
                Ok(c2pa) => Ok(vec![MetadataBlock::C2pa(c2pa)]),
                Err(message) => Err(gamut_core::Error::invalid_input("test", message).into()),
            }
        }
    }

    #[test]
    fn default_metadata_parses_the_blocks() {
        // A C2PA store is carried through unparsed, so the model it yields is exactly predictable.
        let meta = (&Fixed(Ok(b"jumb".to_vec())) as &dyn TypedMetadata)
            .metadata()
            .unwrap();
        assert_eq!(meta.c2pa.as_deref(), Some(&b"jumb"[..]));
    }

    #[test]
    fn default_metadata_propagates_a_container_failure() {
        let err = Fixed(Err("unlocatable")).metadata().unwrap_err();
        let MetadataError::Container(inner) = err else {
            panic!("expected Container, got {err:?}");
        };
        assert_eq!(inner.static_message(), Some("unlocatable"));
    }
}
