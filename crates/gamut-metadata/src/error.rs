//! The facade's error type.

use gamut_exif::ExifError;
use gamut_icc::IccError;
use gamut_iptc::IptcError;
use gamut_xmp::XmpError;

/// An error from extracting a unified [`Metadata`](crate::Metadata) or embedding it back to blocks.
///
/// Each variant names the standard whose parser or serializer failed and forwards that carrier's
/// own error enum ([`ExifError`], [`XmpError`], [`IccError`], [`IptcError`]), preserving its detail.
/// The variants are kept distinct so the failing carrier is always identifiable.
///
/// Marked `#[non_exhaustive]` so a new carrier can add a variant without a breaking change.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum MetadataError {
    /// Parsing or serializing the EXIF blob failed.
    #[error("EXIF: {0}")]
    Exif(#[from] ExifError),
    /// Parsing the XMP packet failed. (XMP serialization is infallible.)
    #[error("XMP: {0}")]
    Xmp(#[from] XmpError),
    /// Parsing or serializing the ICC profile failed.
    #[error("ICC: {0}")]
    Icc(#[from] IccError),
    /// Decoding the legacy IPTC-IIM carrier, or projecting IPTC back to it, failed.
    #[error("IPTC: {0}")]
    Iptc(#[from] IptcError),
    /// The model carried an [extension](crate::MetadataExtension) — which has no carrier to
    /// serialize into — while the embedder was set to
    /// [`ExtensionPolicy::Reject`](crate::ExtensionPolicy::Reject).
    #[error("extension: {namespace}/{key} has no carrier to embed into")]
    UnembeddableExtension {
        /// The namespace of the first offending extension.
        namespace: String,
        /// Its key within `namespace`.
        key: String,
    },
    /// The model carried a [C2PA manifest store](crate::Metadata::c2pa) — which cannot be copied
    /// into a rewritten file, because its hard binding digests the file it was signed over (C2PA
    /// 2.4 §9.1, §15.12.1.1) — while the embedder was set to
    /// [`C2paPolicy::Reject`](crate::C2paPolicy::Reject).
    #[error("c2pa: a {len}-byte manifest store cannot be carried forward into a rewritten file")]
    UnembeddableC2pa {
        /// The size in bytes of the manifest store that was refused.
        len: usize,
    },
    /// The format crate could not **locate** a carrier's payload in its container — before any
    /// carrier parser ran — and reports why in its own [`gamut_core::Error`], carried unchanged so
    /// its [`kind`](gamut_core::Error::kind) and message stay inspectable.
    ///
    /// Returned through [`TypedMetadata::blocks`](crate::TypedMetadata::blocks) by a container
    /// whose payloads are resolved lazily (an ISOBMFF item whose `exif_tiff_header_offset` runs
    /// past its payload, an XMP item under a content coding the crate does not decode); a crate
    /// whose payloads are already bytes never returns it.
    #[error("container: {0}")]
    Container(#[from] gamut_core::Error),
}

/// A [`Result`](core::result::Result) whose error is [`MetadataError`].
pub type Result<T> = core::result::Result<T, MetadataError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_names_the_failing_carrier() {
        assert!(
            MetadataError::Icc(IccError::Malformed("bad"))
                .to_string()
                .starts_with("ICC:")
        );
        assert!(
            MetadataError::Iptc(IptcError::Unsupported("charset"))
                .to_string()
                .starts_with("IPTC:")
        );
    }

    #[test]
    fn unembeddable_c2pa_states_the_size_and_the_reason() {
        // The message must say what was refused (a store, with its size) and why it could not be
        // written, so a caller reading a log knows provenance was dropped deliberately.
        assert_eq!(
            MetadataError::UnembeddableC2pa { len: 4_096 }.to_string(),
            "c2pa: a 4096-byte manifest store cannot be carried forward into a rewritten file"
        );
    }

    #[test]
    fn leaf_errors_convert_into_named_variants() {
        let exif: MetadataError = ExifError::BadByteOrder.into();
        assert!(matches!(exif, MetadataError::Exif(_)));
        assert!(exif.to_string().starts_with("EXIF:"));

        let xmp: MetadataError = XmpError::MissingRdf.into();
        assert!(matches!(xmp, MetadataError::Xmp(_)));
        assert!(xmp.to_string().starts_with("XMP:"));

        let icc: MetadataError = IccError::Malformed("bad").into();
        assert!(matches!(icc, MetadataError::Icc(_)));
        assert!(icc.to_string().starts_with("ICC:"));

        let iptc: MetadataError = IptcError::Malformed("bad").into();
        assert!(matches!(iptc, MetadataError::Iptc(_)));
        assert!(iptc.to_string().starts_with("IPTC:"));
    }

    #[test]
    fn a_container_error_keeps_the_format_crates_kind_and_message() {
        let err: MetadataError =
            gamut_core::Error::unsupported("gamut-heic", "HEIF: content-encoded XMP").into();
        let MetadataError::Container(inner) = &err else {
            panic!("expected Container, got {err:?}");
        };
        assert_eq!(inner.kind(), gamut_core::ErrorKind::Unsupported);
        assert_eq!(inner.static_message(), Some("HEIF: content-encoded XMP"));
        assert!(err.to_string().starts_with("container: "), "{err}");
    }
}
