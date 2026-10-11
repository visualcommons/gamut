//! The unified metadata model.

use gamut_exif::{Exif, Value};
use gamut_icc::IccProfile;
use gamut_iptc::PhotoMetadata;
use gamut_xmp::{WellKnownNs, XmpMeta};

use crate::embed::{EncodedMetadata, MetadataEmbedder};
use crate::error::Result;
use crate::extension::MetadataExtension;
use crate::extract::MetadataExtractor;
use crate::provenance::ProvenanceState;
use crate::source::MetadataBlock;

/// All of an image's metadata, unified across the carriers a container holds.
///
/// There is exactly **one field per genuinely distinct serialization** a still-image container
/// carries:
///
/// - [`exif`](Self::exif) — the EXIF blob (a TIFF/IFD binary stream, e.g. a JPEG `APP1` /
///   WebP `EXIF` / AVIF `Exif` payload);
/// - [`xmp`](Self::xmp) — the XMP packet (the RDF/XML property graph);
/// - [`icc`](Self::icc) — the embedded ICC colour profile;
/// - [`c2pa`](Self::c2pa) — the C2PA manifest store, opaque bytes the facade never parses.
///
/// Each field is `Some` only when that carrier was present.
///
/// # Why there is no `iptc` field
///
/// IPTC Photo Metadata (Core + Extension) **is XMP** — properties in the `dc:`/`photoshop:`/
/// `xmpRights:`/`Iptc4xmp*` namespaces — not an independent serialization. It therefore lives inside
/// [`xmp`](Self::xmp), the single source of truth; storing it a second time would duplicate the same
/// data. The one genuinely separate IPTC carrier is the *legacy binary IIM* block (JPEG `APP13`
/// `8BIM 0x0404`, TIFF/DNG tag 33723); the [extractor](crate::MetadataExtractor) reconciles it
/// *into* `xmp`, and the [embedder](crate::MetadataEmbedder) projects it back out only on request.
/// Read the IPTC view with [`iptc`](Self::iptc) — a typed lens over `xmp` that stores nothing.
///
/// # Extensions
///
/// [`extensions`](Self::extensions) is deliberately **not** a carrier: it holds data no carrier can
/// express, so a downstream typed model survives `their model → Metadata → their model` in full. It
/// does not serialize — see [`MetadataExtension`] and the
/// [crate docs](crate#extensions-data-with-no-carrier). A C2PA manifest store is the opposite —
/// it comes out of a file — so [`c2pa`](Self::c2pa) is a carrier field, with its own asymmetry:
/// extraction produces it and embedding never emits it (see the
/// [crate docs](crate#c2pa-a-carrier-that-must-not-be-copied-forward)).
///
/// # Construction
///
/// Marked `#[non_exhaustive]`, so build one with [`from_carriers`](Self::from_carriers), or with
/// [`Metadata::default`] followed by field assignment, rather than a struct literal.
#[derive(Debug, Clone, Default, PartialEq)]
#[non_exhaustive]
pub struct Metadata {
    /// EXIF metadata (camera/capture parameters, GPS, thumbnail), if present.
    pub exif: Option<Exif>,
    /// XMP metadata — the RDF/XML property graph, which also carries IPTC Photo Metadata — if
    /// present.
    pub xmp: Option<XmpMeta>,
    /// The embedded ICC colour profile, if present.
    pub icc: Option<IccProfile>,
    /// The C2PA manifest store as located in the file — the raw JUMBF superbox bytes (C2PA 2.4
    /// §11.1.4.2) — if present.
    ///
    /// Opaque: the facade never parses, validates, or signs it. Extraction fills this field from a
    /// [`MetadataBlock::C2pa`], but [`encode`](Self::encode) **never** emits it — a manifest's hard
    /// binding digests the finished file (C2PA 2.4 §9.1, §15.12.1.1), so copying a store into a
    /// rewritten file publishes a signature over bytes that no longer exist. See
    /// [`C2paPolicy`](crate::C2paPolicy) to be told rather than have it dropped quietly.
    ///
    /// There is deliberately no byte range beside it: an offset is a property of one file, and
    /// would become a lie the moment this model were embedded into another. Ranges stay with the
    /// format crate that knows the file.
    ///
    /// `Some` here is one of two provenance sources — the other is a `dcterms:provenance` URL in
    /// [`xmp`](Self::xmp) — so ask [`provenance`](Self::provenance) rather than `is_some()` when
    /// the question is "does this image have Content Credentials?".
    pub c2pa: Option<Vec<u8>>,
    /// Data none of the carriers above models, in namespaces the caller owns.
    ///
    /// **Never serialized** — [`encode`](Self::encode) drops these (see
    /// [`ExtensionPolicy`](crate::ExtensionPolicy)) and extraction never produces them. Order is
    /// preserved; a `(namespace, key)` pair appears at most once when maintained through
    /// [`set_extension`](Self::set_extension).
    pub extensions: Vec<MetadataExtension>,
}

impl Metadata {
    /// Builds a model from the three serializable carriers, with no
    /// [`extensions`](Self::extensions) and no [`c2pa`](Self::c2pa).
    ///
    /// The `#[non_exhaustive]` replacement for a `Metadata { exif, xmp, icc }` struct literal.
    /// [`c2pa`](Self::c2pa) is not a parameter: it never round-trips through embedding, so a
    /// caller building a model to embed has nothing to pass. Assign the field directly when
    /// modelling a store read out of a file.
    #[must_use]
    pub fn from_carriers(
        exif: Option<Exif>,
        xmp: Option<XmpMeta>,
        icc: Option<IccProfile>,
    ) -> Self {
        Self {
            exif,
            xmp,
            icc,
            c2pa: None,
            extensions: Vec::new(),
        }
    }

    /// Extracts a unified model from already-located container metadata blocks, using default
    /// options. A convenience for [`MetadataExtractor::new().extract(blocks)`](MetadataExtractor::extract);
    /// use [`MetadataExtractor`] directly to choose an IPTC [`ConflictPolicy`](crate::ConflictPolicy).
    ///
    /// # Errors
    ///
    /// As [`MetadataExtractor::extract`].
    pub fn from_blocks(blocks: &[MetadataBlock<'_>]) -> Result<Self> {
        MetadataExtractor::new().extract(blocks)
    }

    /// Serializes this model back to per-carrier byte blocks, using default options. A convenience for
    /// [`MetadataEmbedder::new().embed(self)`](MetadataEmbedder::embed); use [`MetadataEmbedder`]
    /// directly to also emit the legacy IPTC-IIM block.
    ///
    /// # Errors
    ///
    /// As [`MetadataEmbedder::embed`].
    pub fn encode(&self) -> Result<EncodedMetadata> {
        MetadataEmbedder::new().embed(self)
    }

    /// The IPTC Photo Metadata view over [`xmp`](Self::xmp), or `None` when no XMP is present or the
    /// XMP carries no IPTC-namespace properties.
    ///
    /// This is a *computed lens* — the IPTC-relevant subset of the XMP graph, produced on demand via
    /// [`PhotoMetadata::from_xmp`] — not stored state. Mutating the returned value does **not** change
    /// `self`; to edit IPTC, edit [`xmp`](Self::xmp) (IPTC properties live there).
    #[must_use]
    pub fn iptc(&self) -> Option<PhotoMetadata> {
        let pm = PhotoMetadata::from_xmp(self.xmp.as_ref()?);
        (!pm.xmp.properties.is_empty()).then_some(pm)
    }

    /// Where this image's C2PA provenance lives: embedded, remote, both, or nowhere.
    ///
    /// A *computed lens* over two independent sources, stored nowhere: [`c2pa`](Self::c2pa) being
    /// `Some` means a manifest store is embedded, and a simple `dcterms:provenance` property in
    /// [`xmp`](Self::xmp) (namespace [`WellKnownNs::DcTerms`], C2PA 2.4 §11.5 / §15.5.3.1) means
    /// an external manifest lives at that URL. Neither source suppresses the other: the key is
    /// reserved for external manifests (§11.5), but nothing stops a file from carrying both, and
    /// this reports what the file carries rather than choosing between them.
    ///
    /// The URL comes back as the XMP carried it, with surrounding whitespace trimmed; **gamut
    /// never resolves it** (see [`ProvenanceState`]). A value that is empty or whitespace-only is
    /// treated as no URL — §11.5 makes the value a URI reference, which neither is — and a
    /// non-simple value (an array or structure) is ignored. Should a non-canonical graph carry
    /// the property twice, the first occurrence wins, as [`XmpMeta::get`] defines. The HTTP `Link`
    /// header route of §15.5.3.2 is deliberately not modelled: see the
    /// [`provenance`](crate::provenance) module.
    #[must_use]
    pub fn provenance(&self) -> ProvenanceState {
        let remote = self
            .xmp
            .as_ref()
            .and_then(|xmp| xmp.get_text(WellKnownNs::DcTerms.uri(), "provenance"))
            .map(str::trim)
            .filter(|url| !url.is_empty());
        match (self.c2pa.is_some(), remote) {
            (false, None) => ProvenanceState::None,
            (false, Some(url)) => ProvenanceState::Remote(url.to_owned()),
            (true, None) => ProvenanceState::Embedded,
            (true, Some(url)) => ProvenanceState::EmbeddedAndRemote(url.to_owned()),
        }
    }

    /// The value bound to `key` in `namespace`, or `None` when the model carries no such
    /// [extension](Self::extensions).
    #[must_use]
    pub fn extension(&self, namespace: &str, key: &str) -> Option<&Value> {
        self.extensions
            .iter()
            .find(|e| e.namespace == namespace && e.key == key)
            .map(|e| &e.value)
    }

    /// Binds `key` in `namespace` to `value`, replacing the existing binding in place if there is
    /// one and appending otherwise — so a `(namespace, key)` pair never appears twice.
    pub fn set_extension(
        &mut self,
        namespace: impl Into<String>,
        key: impl Into<String>,
        value: Value,
    ) {
        let (namespace, key) = (namespace.into(), key.into());
        match self
            .extensions
            .iter_mut()
            .find(|e| e.namespace == namespace && e.key == key)
        {
            Some(existing) => existing.value = value,
            None => self
                .extensions
                .push(MetadataExtension::new(namespace, key, value)),
        }
    }

    /// Removes the binding for `key` in `namespace`, returning its value if there was one.
    pub fn remove_extension(&mut self, namespace: &str, key: &str) -> Option<Value> {
        let index = self
            .extensions
            .iter()
            .position(|e| e.namespace == namespace && e.key == key)?;
        Some(self.extensions.remove(index).value)
    }

    /// The [extensions](Self::extensions) in `namespace`, in insertion order.
    pub fn extensions_in<'a>(
        &'a self,
        namespace: &'a str,
    ) -> impl Iterator<Item = &'a MetadataExtension> {
        self.extensions
            .iter()
            .filter(move |e| e.namespace == namespace)
    }

    /// Whether the model holds nothing at all — no carrier and no
    /// [extension](Self::extensions).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.exif.is_none()
            && self.xmp.is_none()
            && self.icc.is_none()
            && self.c2pa.is_none()
            && self.extensions.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use gamut_xmp::WellKnownNs;

    use super::*;

    fn xmp_with(namespace: &str, name: &str, value: &str) -> XmpMeta {
        let mut xmp = XmpMeta::new();
        xmp.set_text(namespace, name, value);
        xmp
    }

    #[test]
    fn iptc_lens_reflects_only_iptc_namespaces() {
        // An IPTC namespace (photoshop:City) surfaces through the lens...
        let iptc = Metadata {
            xmp: Some(xmp_with(WellKnownNs::Photoshop.uri(), "City", "Oslo")),
            ..Default::default()
        };
        assert_eq!(iptc.iptc().unwrap().city(), Some("Oslo"));

        // ...a non-IPTC namespace (xmp:CreatorTool) does not.
        let non_iptc = Metadata {
            xmp: Some(xmp_with(WellKnownNs::Xmp.uri(), "CreatorTool", "gamut")),
            ..Default::default()
        };
        assert!(non_iptc.iptc().is_none());

        // No XMP at all → no IPTC view.
        assert!(Metadata::default().iptc().is_none());
    }

    #[test]
    fn is_empty_tracks_field_presence() {
        assert!(Metadata::default().is_empty());
        assert!(
            !Metadata {
                xmp: Some(XmpMeta::new()),
                ..Default::default()
            }
            .is_empty()
        );
        // A manifest store alone is metadata too, even though it never embeds.
        assert!(
            !Metadata {
                c2pa: Some(vec![0x00, 0x00, 0x00, 0x14]),
                ..Default::default()
            }
            .is_empty()
        );
    }

    #[test]
    fn provenance_treats_an_empty_dcterms_value_as_no_url() {
        // §11.5 makes the value a URI reference; an empty element is not one, so it must not
        // surface as Remote("") for a caller to try to fetch.
        let empty = Metadata {
            xmp: Some(xmp_with(WellKnownNs::DcTerms.uri(), "provenance", "")),
            ..Default::default()
        };
        assert_eq!(empty.provenance(), ProvenanceState::None);

        let with_store = Metadata {
            c2pa: Some(vec![0x00, 0x00, 0x00, 0x14]),
            ..empty
        };
        assert_eq!(with_store.provenance(), ProvenanceState::Embedded);
    }

    #[test]
    fn provenance_trims_the_url_and_treats_whitespace_only_as_no_url() {
        // The same reason as the empty value: a URI reference has no surrounding whitespace, and
        // `Remote("   ")` would hand a caller nothing to fetch. Padding around a real URL is
        // pretty-printing noise, not part of the reference.
        let blank = Metadata {
            xmp: Some(xmp_with(WellKnownNs::DcTerms.uri(), "provenance", " \n\t ")),
            ..Default::default()
        };
        assert_eq!(blank.provenance(), ProvenanceState::None);
        let blank_with_store = Metadata {
            c2pa: Some(vec![0x00, 0x00, 0x00, 0x14]),
            ..blank
        };
        assert_eq!(blank_with_store.provenance(), ProvenanceState::Embedded);

        let padded = Metadata {
            xmp: Some(xmp_with(
                WellKnownNs::DcTerms.uri(),
                "provenance",
                "\n  https://example.com/m.c2pa  \n",
            )),
            ..Default::default()
        };
        assert_eq!(
            padded.provenance(),
            ProvenanceState::Remote("https://example.com/m.c2pa".to_owned())
        );
    }

    #[test]
    fn provenance_reads_only_the_dcterms_namespace() {
        // Same local name in Dublin Core *elements* (`dc:`) is a different property; the two
        // namespaces share a vendor path, so the mix-up is the likely defect.
        let dc = Metadata {
            xmp: Some(xmp_with(
                WellKnownNs::DublinCore.uri(),
                "provenance",
                "https://example.com/m.c2pa",
            )),
            ..Default::default()
        };
        assert_eq!(dc.provenance(), ProvenanceState::None);

        let dcterms = Metadata {
            xmp: Some(xmp_with(
                WellKnownNs::DcTerms.uri(),
                "provenance",
                "https://example.com/m.c2pa",
            )),
            ..Default::default()
        };
        assert_eq!(
            dcterms.provenance(),
            ProvenanceState::Remote("https://example.com/m.c2pa".to_owned())
        );
    }

    #[test]
    fn from_carriers_leaves_the_manifest_store_empty() {
        // `c2pa` is not a `from_carriers` parameter: a model built to embed carries no store.
        let meta = Metadata::from_carriers(None, Some(XmpMeta::new()), None);
        assert_eq!(meta.c2pa, None);
    }
}
