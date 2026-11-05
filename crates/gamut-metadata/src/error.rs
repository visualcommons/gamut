//! The facade's error type.

use gamut_exif::ExifError;
use gamut_icc::IccError;
use gamut_iptc::IptcError;
use gamut_xmp::XmpError;

/// An error from extracting a unified [`Metadata`](crate::Metadata) or embedding it back to blocks.
///
/// Each variant names the standard whose parser or serializer failed and forwards that carrier's
/// own error enum ([`ExifError`], [`XmpError`], [`IccError`], [`IptcError`]), preserving its detail.
/// The variants are kept distinct so the failing carrier is always identifiable by matching;
/// their `Display` is the carrier error's own, verbatim (`#[error(transparent)]`), so a message a
/// leaf crate already prefixes (`XMP: …`) is never prefixed twice.
///
/// `From<MetadataError> for gamut_core::Error` funnels it into the workspace error, so a caller
/// speaking [`gamut_core::Result`] can `?` a facade result.
///
/// Marked `#[non_exhaustive]` so a new carrier can add a variant without a breaking change.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum MetadataError {
    /// Parsing or serializing the EXIF blob failed.
    #[error(transparent)]
    Exif(#[from] ExifError),
    /// Parsing the XMP packet failed. (XMP serialization is infallible.)
    #[error(transparent)]
    Xmp(#[from] XmpError),
    /// Parsing or serializing the ICC profile failed.
    #[error(transparent)]
    Icc(#[from] IccError),
    /// Decoding the legacy IPTC-IIM carrier, or projecting IPTC back to it, failed.
    #[error(transparent)]
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
    ///
    /// Named for what it carries rather than for that one use: `#[from]` makes every
    /// [`gamut_core::Error`] a `?` converts land here, whatever produced it.
    #[error(transparent)]
    Core(#[from] gamut_core::Error),
}

/// A [`Result`](core::result::Result) whose error is [`MetadataError`].
pub type Result<T> = core::result::Result<T, MetadataError>;

impl From<MetadataError> for gamut_core::Error {
    /// Funnels a [`MetadataError`] into the workspace's unified [`gamut_core::Error`].
    ///
    /// A carrier variant goes through that carrier crate's own conversion, so its
    /// [`kind`](gamut_core::Error::kind), origin, offset and detail are the leaf crate's;
    /// [`MetadataError::Core`] is handed back unchanged. The two policy refusals are
    /// [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) — the caller offered a model
    /// its own embedder settings forbid — with the facade's message as
    /// [`detail`](gamut_core::Error::detail).
    fn from(err: MetadataError) -> Self {
        let message = match err {
            MetadataError::Exif(e) => return e.into(),
            MetadataError::Xmp(e) => return e.into(),
            MetadataError::Icc(e) => return e.into(),
            MetadataError::Iptc(e) => return e.into(),
            MetadataError::Core(e) => return e,
            MetadataError::UnembeddableExtension { .. } => {
                "metadata: an extension has no carrier to embed into"
            }
            MetadataError::UnembeddableC2pa { .. } => {
                "metadata: a C2PA manifest store cannot be carried forward"
            }
        };
        gamut_core::Error::invalid_input(env!("CARGO_PKG_NAME"), message)
            .with_detail(err.to_string())
    }
}

#[cfg(test)]
mod tests {
    use gamut_core::ErrorKind;

    use super::*;

    #[test]
    fn display_is_the_carrier_errors_own_without_a_second_prefix() {
        // `XmpError` already prefixes its own message; the facade must not add another.
        let xmp = XmpError::MissingRdf;
        assert_eq!(
            MetadataError::Xmp(xmp).to_string(),
            XmpError::MissingRdf.to_string()
        );
        assert!(
            !MetadataError::Xmp(XmpError::MissingRdf)
                .to_string()
                .starts_with("XMP: XMP:")
        );
        assert_eq!(
            MetadataError::Icc(IccError::Malformed("bad")).to_string(),
            IccError::Malformed("bad").to_string()
        );
        assert_eq!(
            MetadataError::Iptc(IptcError::Unsupported("charset")).to_string(),
            IptcError::Unsupported("charset").to_string()
        );
        assert_eq!(
            MetadataError::Exif(ExifError::Truncated).to_string(),
            ExifError::Truncated.to_string()
        );
    }

    #[test]
    fn carrier_variants_convert_to_core_through_the_leaf_crates_own_conversion() {
        // Each arm hands over to the leaf `From`, so the origin is the leaf crate's.
        let origin = |err: MetadataError| gamut_core::Error::from(err).origin();
        assert_eq!(origin(ExifError::Truncated.into()), Some("gamut-exif"));
        assert_eq!(origin(XmpError::MissingRdf.into()), Some("gamut-xmp"));
        assert_eq!(origin(IccError::Malformed("bad").into()), Some("gamut-icc"));
        assert_eq!(
            origin(IptcError::Malformed("bad").into()),
            Some("gamut-iptc")
        );
    }

    #[test]
    fn a_core_variant_converts_back_unchanged() {
        let core = gamut_core::Error::unsupported("gamut-heic", "HEIF: content-encoded XMP");
        let back = gamut_core::Error::from(MetadataError::Core(core));
        assert_eq!(back.kind(), ErrorKind::Unsupported);
        assert_eq!(back.origin(), Some("gamut-heic"));
        assert_eq!(back.static_message(), Some("HEIF: content-encoded XMP"));
    }

    #[test]
    fn policy_refusals_convert_to_invalid_input_with_the_facade_message_as_detail() {
        let ext = MetadataError::UnembeddableExtension {
            namespace: "com.example".to_owned(),
            key: "K".to_owned(),
        };
        let detail = ext.to_string();
        let core = gamut_core::Error::from(ext);
        assert_eq!(core.kind(), ErrorKind::InvalidInput);
        assert_eq!(core.origin(), Some("gamut-metadata"));
        assert_eq!(
            core.static_message(),
            Some("metadata: an extension has no carrier to embed into")
        );
        assert_eq!(core.detail(), Some(detail.as_str()));

        let c2pa = gamut_core::Error::from(MetadataError::UnembeddableC2pa { len: 8 });
        assert_eq!(c2pa.kind(), ErrorKind::InvalidInput);
        assert_eq!(
            c2pa.static_message(),
            Some("metadata: a C2PA manifest store cannot be carried forward")
        );
        assert_eq!(
            c2pa.detail(),
            Some("c2pa: a 8-byte manifest store cannot be carried forward into a rewritten file")
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

        let xmp: MetadataError = XmpError::MissingRdf.into();
        assert!(matches!(xmp, MetadataError::Xmp(_)));

        let icc: MetadataError = IccError::Malformed("bad").into();
        assert!(matches!(icc, MetadataError::Icc(_)));

        let iptc: MetadataError = IptcError::Malformed("bad").into();
        assert!(matches!(iptc, MetadataError::Iptc(_)));
    }

    #[test]
    fn a_core_error_keeps_the_format_crates_kind_and_message() {
        let err: MetadataError =
            gamut_core::Error::unsupported("gamut-heic", "HEIF: content-encoded XMP").into();
        let MetadataError::Core(inner) = &err else {
            panic!("expected Core, got {err:?}");
        };
        assert_eq!(inner.kind(), gamut_core::ErrorKind::Unsupported);
        assert_eq!(inner.static_message(), Some("HEIF: content-encoded XMP"));
        // Transparent: the format crate's own message, with no facade prefix in front of it.
        assert_eq!(err.to_string(), inner.to_string());
    }
}
