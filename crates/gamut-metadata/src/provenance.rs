//! Where an image's C2PA provenance lives — embedded in the file, at a remote URL, both, or
//! nowhere.
//!
//! C2PA 2.4 gives a still image two independent ways to carry provenance. The manifest store can be
//! **embedded** in the file (§11.1.4.2; the facade holds it verbatim in
//! [`Metadata::c2pa`](crate::Metadata::c2pa)), or it can be **external**, in which case §11.5
//! recommends the claim generator add a `dcterms:provenance` key to the asset's XMP whose value —
//! "a URI reference" — says where to find it. §11.5 is explicit that the mechanism is *only* for
//! external manifests; §15.5.3.1 lists the key among the places a validator looks when no store is
//! embedded. The two sources are independent bytes in the file, so a file may carry both, and the
//! lens reports both rather than letting one hide the other — what a validator then does with the
//! pair is the spec's business (§15.5.2.1 / §15.5.3.1: it uses the embedded store and does not
//! consult the URL), not this crate's.
//!
//! [`ProvenanceState`] is the facade's answer to "does this image have Content Credentials, and
//! where?" — four states, never collapsed to a boolean, so a file with no embedded store and a
//! remote URL reports [`Remote`](ProvenanceState::Remote) rather than a confident
//! [`None`](ProvenanceState::None). It is a *lens* computed by
//! [`Metadata::provenance`](crate::Metadata::provenance), not stored state.
//!
//! # What gamut does not do
//!
//! - **It never fetches the URL.** Resolving it, and judging whatever it points at, is a
//!   validator's job and a network operation; the workspace ships neither (see
//!   `references/c2pa/README.md`). The URL is handed over as the string the XMP carried, with
//!   surrounding whitespace trimmed and nothing else changed; a value that is empty or only
//!   whitespace counts as no URL.
//! - **The HTTP `Link` header route is out of scope.** §15.5.3.2 defines an HTTP `Link` relation
//!   that carries the same pointer for an asset served over HTTP. A header is a property of a
//!   *transfer*, not of the file's bytes, so a file-format library cannot observe it; a caller that
//!   fetched the asset itself holds the header and may consult it before this lens. This is a
//!   deliberate boundary, not an omission.

/// Where the C2PA manifest store that vouches for an image lives, as far as the image's own
/// metadata says.
///
/// Returned by [`Metadata::provenance`](crate::Metadata::provenance), which combines two
/// independent sources: whether the container located an embedded store
/// ([`Metadata::c2pa`](crate::Metadata::c2pa)) and whether the XMP graph carries a
/// `dcterms:provenance` URL (C2PA 2.4 §11.5, §15.5.3.1). Because the sources are independent the
/// type has four states, not three and not a boolean: [`EmbeddedAndRemote`](Self::EmbeddedAndRemote)
/// is a real case — the key is reserved for external manifests (§11.5), yet nothing stops a file
/// from carrying both — and neither source suppresses the other. This is a report of what the file
/// carries, not a validity verdict and not a choice between the two.
///
/// The remote URL is carried as the string the XMP held, with surrounding whitespace trimmed and
/// otherwise verbatim; an empty or whitespace-only value counts as no URL. **gamut never resolves
/// it**; see the
/// [module docs](self) for why, and for the HTTP `Link` header route this type deliberately does
/// not model.
///
/// Marked `#[non_exhaustive]` so a further provenance source can be added without a breaking
/// change; match with a wildcard arm, or use [`is_embedded`](Self::is_embedded) and
/// [`remote_url`](Self::remote_url), which answer the two underlying questions directly and are
/// the C-portable surface of this type (a data-carrying enum has no observable tag, and `String`
/// is not FFI-safe). There is deliberately no `Default`: this is a computed report, and a default
/// of "no provenance" would be a confident answer nobody asked for.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ProvenanceState {
    /// No embedded manifest store and no `dcterms:provenance` URL. This is what the metadata
    /// says, not a validity verdict — the asset may still carry provenance by a route the file
    /// cannot express (see the [module docs](self) on the HTTP `Link` header).
    None,
    /// No embedded store; the XMP points at an external manifest at this URL (C2PA 2.4 §11.5).
    /// The string is the `dcterms:provenance` value with surrounding whitespace trimmed,
    /// otherwise verbatim — unresolved and unvalidated.
    Remote(String),
    /// A manifest store is embedded in the file ([`Metadata::c2pa`](crate::Metadata::c2pa) is
    /// `Some`) and the XMP carries no `dcterms:provenance` URL.
    Embedded,
    /// Both: a manifest store is embedded *and* the XMP carries a `dcterms:provenance` URL. The
    /// URL is reported because the file carries it; §11.5 makes the key external-only, and a
    /// validator that finds an embedded store uses it and does not consult the URL (§15.5.2.1,
    /// §15.5.3.1), so this variant says nothing about which manifest is authoritative.
    EmbeddedAndRemote(String),
}

impl ProvenanceState {
    /// Whether a manifest store is embedded in the file — `true` for
    /// [`Embedded`](Self::Embedded) and [`EmbeddedAndRemote`](Self::EmbeddedAndRemote).
    #[must_use]
    pub fn is_embedded(&self) -> bool {
        matches!(self, Self::Embedded | Self::EmbeddedAndRemote(_))
    }

    /// The `dcterms:provenance` URL of an external manifest, if the XMP carried one — `Some` for
    /// [`Remote`](Self::Remote) and [`EmbeddedAndRemote`](Self::EmbeddedAndRemote). Never
    /// resolved by gamut.
    #[must_use]
    pub fn remote_url(&self) -> Option<&str> {
        match self {
            Self::Remote(url) | Self::EmbeddedAndRemote(url) => Some(url),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const URL: &str = "https://example.com/m.c2pa";

    #[test]
    fn is_embedded_is_true_for_exactly_the_embedded_variants() {
        assert!(!ProvenanceState::None.is_embedded());
        assert!(!ProvenanceState::Remote(URL.into()).is_embedded());
        assert!(ProvenanceState::Embedded.is_embedded());
        assert!(ProvenanceState::EmbeddedAndRemote(URL.into()).is_embedded());
    }

    #[test]
    fn remote_url_is_some_for_exactly_the_remote_variants() {
        assert_eq!(ProvenanceState::None.remote_url(), None);
        assert_eq!(ProvenanceState::Remote(URL.into()).remote_url(), Some(URL));
        assert_eq!(ProvenanceState::Embedded.remote_url(), None);
        assert_eq!(
            ProvenanceState::EmbeddedAndRemote(URL.into()).remote_url(),
            Some(URL)
        );
    }
}
