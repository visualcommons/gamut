//! The crate's error type.

/// An error encountered while parsing an XMP packet.
///
/// Serialization is infallible (see [`crate::XmpMeta::to_packet`]); these errors arise only on the
/// read path. Variants carry dynamic context — a detail string naming the offending construct or
/// element — so a malformed packet is easy to diagnose.
///
/// `quick-xml` is an internal implementation detail: its error type is **not** re-exposed here (XML
/// lexing failures are captured as [`XmpError::Xml`] with an owned message), so the XML backend can
/// change without breaking this crate's public API.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum XmpError {
    /// The packet is not valid UTF-8, or declares an unsupported text encoding. gamut-xmp reads and
    /// writes UTF-8; Part 1 §7.1 also permits UTF-16/32, which are not implemented.
    #[error("XMP encoding: {0}")]
    Encoding(&'static str),

    /// The RDF/XML could not be lexed; the string is the underlying lexer's message.
    #[error("XMP: malformed XML: {0}")]
    Xml(String),

    /// No `rdf:RDF` element was found in the packet (Part 1 §7.4).
    #[error("XMP: no rdf:RDF root element found")]
    MissingRdf,

    /// A namespace prefix was used without an in-scope `xmlns` declaration; the string is the
    /// unresolved prefix.
    #[error("XMP: undeclared namespace prefix '{0}'")]
    UnknownPrefix(String),

    /// An RDF/XML form that XMP does not permit (Part 1 §7.5/§7.9): e.g. `rdf:parseType="Literal"`
    /// or `"Collection"`, or a top-level typed node. The string names the construct and element.
    #[error("XMP: unsupported RDF/XML form: {0}")]
    UnsupportedForm(String),

    /// A construct the spec explicitly prohibits (Part 1 §7.8/§7.9.3): e.g. `rdf:_n` array items,
    /// or an `rdf:value` that carries `xml:lang` or nested general qualifiers.
    #[error("XMP: prohibited construct: {0}")]
    Prohibited(String),

    /// A language alternative (`rdf:Alt`) held two items with the same `xml:lang`; Part 1 §8.2.2.4
    /// requires the language tags to be unique. The string is the duplicated tag.
    #[error("XMP: duplicate xml:lang '{0}' in an alternative array")]
    DuplicateLang(String),

    /// A sidecar file's document element was not `x:xmpmeta`; the string names the element found.
    ///
    /// Raised only by [`crate::XmpSidecar::read`]. The wrapper is **optional** in an embedded
    /// packet (Part 1 §7.3.3, and [`crate::XmpWriter::wrap_xmpmeta`] can omit it), so this is not
    /// a prohibited construct — it is the one thing a standalone `.xmp` file needs beyond a packet
    /// body, because §7.3.3 gives `x:xmpmeta` exactly the job of identifying XMP inside general
    /// XML text. A bare `rdf:RDF` document is valid XMP for [`crate::XmpMeta::from_packet`].
    #[error("XMP: sidecar document element is <{0}>, not x:xmpmeta")]
    MissingXmpMeta(String),
}

/// A specialized [`Result`](core::result::Result) for the XMP read path.
pub type Result<T> = core::result::Result<T, XmpError>;

impl From<XmpError> for gamut_core::Error {
    /// Funnels an [`XmpError`] into the workspace's unified [`gamut_core::Error`] so the
    /// `gamut-metadata` facade and the format crates can present one error surface. gamut-core's
    /// error retains the original XMP diagnostic as owned detail while preserving a stable
    /// [`gamut_core::ErrorKind`].
    fn from(err: XmpError) -> Self {
        let detail = err.to_string();
        let classified = match err {
            XmpError::Encoding(_) | XmpError::UnsupportedForm(_) => {
                gamut_core::Error::unsupported(env!("CARGO_PKG_NAME"), "XMP: unsupported feature")
            }
            _ => gamut_core::Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "XMP: invalid metadata packet",
            ),
        };
        classified.with_detail(detail)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_includes_dynamic_context() {
        let e = XmpError::UnsupportedForm("rdf:parseType=\"Collection\" on dc:foo".into());
        let s = e.to_string();
        assert!(s.contains("parseType"), "construct must surface: {s}");
        assert!(s.contains("dc:foo"), "element name must surface: {s}");
    }

    #[test]
    fn maps_to_core_unsupported_vs_invalid() {
        // Encoding / unsupported-form become Unsupported; everything else is invalid input.
        assert!(matches!(
            gamut_core::Error::from(XmpError::Encoding("x")),
            gamut_core::Error::Context(ref diagnostic) if diagnostic.source_error().kind() == gamut_core::ErrorKind::Unsupported
        ));
        assert!(matches!(
            gamut_core::Error::from(XmpError::UnsupportedForm("parseType=Literal".into())),
            gamut_core::Error::Context(ref diagnostic) if diagnostic.source_error().kind() == gamut_core::ErrorKind::Unsupported
        ));
        assert!(matches!(
            gamut_core::Error::from(XmpError::MissingRdf),
            gamut_core::Error::Context(ref diagnostic) if diagnostic.source_error().kind() == gamut_core::ErrorKind::InvalidInput
        ));
        assert!(matches!(
            gamut_core::Error::from(XmpError::DuplicateLang("en".into())),
            gamut_core::Error::Context(ref diagnostic) if diagnostic.source_error().kind() == gamut_core::ErrorKind::InvalidInput
        ));
        // A sidecar without its wrapper is bad input, not an unimplemented feature: the caller
        // fixes it by handing the bytes to `XmpMeta::from_packet` or wrapping them.
        assert!(matches!(
            gamut_core::Error::from(XmpError::MissingXmpMeta("RDF".into())),
            gamut_core::Error::Context(ref diagnostic) if diagnostic.source_error().kind() == gamut_core::ErrorKind::InvalidInput
        ));
    }
}
