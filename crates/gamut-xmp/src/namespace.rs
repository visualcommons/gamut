//! XML namespaces and the well-known XMP schemas.

/// The RDF namespace URI (`rdf:` prefix), the syntax XMP serializes into (Part 1 §6.2).
pub const RDF_NAMESPACE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";

/// The XML namespace URI (`xml:` prefix), home of the `xml:lang` qualifier (Part 1 §6.2).
pub const XML_NAMESPACE: &str = "http://www.w3.org/XML/1998/namespace";

/// The `x:xmpmeta` wrapper namespace URI (`x:` prefix), the optional outer element (Part 1 §7.3.3).
pub const XMPMETA_NAMESPACE: &str = "adobe:ns:meta/";

/// An XML namespace: the URI that scopes property names, and its conventional prefix.
///
/// The URI is the identity; the prefix is a serialization preference. Register one on
/// [`crate::XmpWriter::with_namespace`] to control the prefix a schema serializes under (a
/// [`WellKnownNs`] converts directly via `From`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Namespace {
    /// The namespace URI (the canonical identity of the schema).
    pub uri: String,
    /// The conventional prefix used in serialization (e.g. `dc`, `xmp`).
    pub prefix: String,
}

impl Namespace {
    /// Creates a namespace from a URI and conventional prefix.
    #[must_use]
    pub fn new(uri: impl Into<String>, prefix: impl Into<String>) -> Self {
        Self {
            uri: uri.into(),
            prefix: prefix.into(),
        }
    }
}

impl From<WellKnownNs> for Namespace {
    /// The standard schema's namespace with its conventional prefix.
    fn from(ns: WellKnownNs) -> Self {
        Namespace::new(ns.uri(), ns.prefix())
    }
}

/// The standard XMP schemas (Adobe XMP Parts 1–2), plus the external schemas image metadata
/// standards and widely deployed tools layer on XMP. Each maps to a fixed namespace URI and a
/// conventional prefix via [`WellKnownNs::uri`] / [`WellKnownNs::prefix`];
/// [`WellKnownNs::from_uri`] recovers the schema from a URI.
///
/// This is a **namespace registry, not a validator**: registering a schema fixes the prefix its
/// properties serialize under (the one the reference engine, exiv2's Adobe XMPCore, keys them by),
/// and nothing more — property values stay uninterpreted text. The set covers every schema exiv2
/// documents (<https://exiv2.org/metadata.html>); the non-Adobe URIs are each cited on their
/// variant, and all thirty are cross-checked against XMPCore's own registry in `tests/oracle.rs`.
///
/// Marked `#[non_exhaustive]`: the registry grows as gamut's format and metadata crates need
/// further schemas, and each addition must not be a breaking change. Match with a wildcard arm,
/// or iterate [`WellKnownNs::ALL`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum WellKnownNs {
    /// `dc` — Dublin Core (title, creator, description, subject, rights, …).
    DublinCore,
    /// `xmp` — the basic XMP schema (CreateDate, ModifyDate, CreatorTool, …).
    Xmp,
    /// `xmpRights` — rights-management schema.
    XmpRights,
    /// `xmpMM` — media-management schema (DocumentID, InstanceID, history).
    XmpMediaManagement,
    /// `photoshop` — Adobe Photoshop schema.
    Photoshop,
    /// `exif` — EXIF tags mirrored into XMP.
    Exif,
    /// `tiff` — TIFF/EXIF image tags mirrored into XMP.
    Tiff,
    /// `Iptc4xmpCore` — IPTC Photo Metadata Core.
    Iptc4XmpCore,
    /// `Iptc4xmpExt` — IPTC Photo Metadata Extension.
    Iptc4XmpExt,
    /// `crs` — Camera Raw settings.
    CameraRaw,
    /// `xmpidq` — the qualifier schema for `xmpMM` identifiers (Part 1 §8.7).
    XmpIdentifier,
    /// `xmpBJ` — the Basic Job Ticket schema (Part 2 §2.3).
    XmpJobTicket,
    /// `xmpTPg` — the Paged-Text schema (Part 2 §2.4).
    XmpPagedText,
    /// `xmpDM` — the Dynamic Media schema (Part 2 §2.5).
    XmpDynamicMedia,
    /// `pdf` — the Adobe PDF schema (Part 2 §3.1).
    Pdf,
    /// `stDim` — the Dimensions structure type (used by, e.g., `xmpTPg:MaxPageSize`).
    Dimensions,
    /// `stRef` — the ResourceRef structure type (used by `xmpMM` references).
    ResourceRef,
    /// `dcterms` — DCMI Metadata Terms (qualified Dublin Core). Home of `dcterms:provenance`, the
    /// key C2PA 2.4 §11.5 / §15.5.3.1 uses to point at an **external** manifest store; gamut only
    /// registers the namespace — the C2PA reading of the property lives in `gamut-metadata`.
    DcTerms,
    /// `exifEX` — Exif 2.3+ properties in XMP (`LensModel`, `PhotographicSensitivity`, …), the
    /// XMP counterpart of the tags `gamut-exif` reads.
    ///
    /// URI `http://cipa.jp/exif/1.0/`, from CIPA DC-010-2012 "Exif metadata for XMP" (referenced,
    /// not reproduced, by Adobe XMP Part 2 §3.4) and registered under that URI by exiv2
    /// (`third_party/exiv2/src/properties.cpp`, `xmpNsInfo`). The vendored Exif 3.0 text
    /// (`references/exif/exif-3.0-dc-008-translation-2023.pdf`, Annex J.2–J.3) binds the same
    /// `exifEX` prefix to `http://cipa.jp/exif/2.32/` in its *annotation* (`exifEX:ExifAN`)
    /// examples; that URI is not registered here — the reference engine and deployed writers use
    /// `1.0`, and a second variant can be added without a breaking change if a consumer needs it.
    ExifEx,
    /// `aux` — Exif auxiliary camera/lens properties (`aux:Lens`, `aux:SerialNumber`, …),
    /// ubiquitous in Lightroom and Camera Raw output.
    ///
    /// URI `http://ns.adobe.com/exif/1.0/aux/`, Adobe's "Exif Schema for Additional Exif
    /// Properties" (present in the 2008/2010 editions of XMP Part 2, dropped from the vendored
    /// 2016 text); registered by exiv2 (`third_party/exiv2/src/properties.cpp`, `xmpNsInfo`).
    Aux,
    /// `plus` — the PLUS (Picture Licensing Universal System) License Data Format.
    ///
    /// URI `http://ns.useplus.org/ldf/xmp/1.0/`, from the PLUS LDF XMP specification
    /// (<https://ns.useplus.org/LDF/ldf-XMPSpecification>); registered by exiv2
    /// (`third_party/exiv2/src/properties.cpp`, `xmpNsInfo`).
    Plus,
    /// `mwg-rs` — Metadata Working Group image regions (`mwg-rs:Regions`).
    ///
    /// URI `http://www.metadataworkinggroup.com/schemas/regions/`, from the MWG *Guidelines for
    /// Handling Image Metadata* 2.0 (2010), "Regions" schema; registered by exiv2
    /// (`third_party/exiv2/src/properties.cpp`, `xmpNsInfo`).
    MwgRegions,
    /// `mwg-kw` — Metadata Working Group hierarchical keywords (`mwg-kw:Keywords`).
    ///
    /// URI `http://www.metadataworkinggroup.com/schemas/keywords/`, from the MWG *Guidelines for
    /// Handling Image Metadata* 2.0 (2010), "Keywords" schema; registered by exiv2
    /// (`third_party/exiv2/src/properties.cpp`, `xmpNsInfo`).
    MwgKeywords,
    /// `GPano` — Google Photo Sphere (panorama) metadata.
    ///
    /// URI `http://ns.google.com/photos/1.0/panorama/`, from Google's *Photo Sphere XMP Metadata*
    /// specification; registered by exiv2 (`third_party/exiv2/src/properties.cpp`, `xmpNsInfo`).
    GPano,
    /// `lr` — Adobe Lightroom (`lr:hierarchicalSubject`, `lr:privateRTKInfo`).
    ///
    /// URI `http://ns.adobe.com/lightroom/1.0/`, Adobe's Lightroom schema (not part of the
    /// published XMP Parts 1–3); registered by exiv2 (`third_party/exiv2/src/properties.cpp`,
    /// `xmpNsInfo`).
    Lightroom,
    /// `MicrosoftPhoto` — Microsoft Photo 1.0 (Windows Photo Gallery / Explorer rating and camera
    /// fields).
    ///
    /// URI `http://ns.microsoft.com/photo/1.0/`, Microsoft's Photo schema; registered by exiv2
    /// (`third_party/exiv2/src/properties.cpp`, `xmpNsInfo`).
    MicrosoftPhoto,
    /// `digiKam` — digiKam photo-management properties (`digiKam:TagsList`,
    /// `digiKam:ColorLabel`, …).
    ///
    /// URI `http://www.digikam.org/ns/1.0/`, digiKam's own schema; registered by exiv2
    /// (`third_party/exiv2/src/properties.cpp`, `xmpNsInfo`).
    DigiKam,
    /// `acdsee` — ACDSee photo-management properties.
    ///
    /// URI `http://ns.acdsee.com/iptc/1.0/`, ACDSee's own schema; registered by exiv2
    /// (`third_party/exiv2/src/properties.cpp`, `xmpNsInfo`).
    Acdsee,
    /// `crss` — Camera Raw Saved Settings (snapshots), the companion of `crs` (Camera Raw
    /// settings, Part 2 §3.3).
    ///
    /// URI `http://ns.adobe.com/camera-raw-saved-settings/1.0/`, Adobe's Camera Raw Saved Settings
    /// schema (not part of the published XMP Parts 1–3); registered by exiv2
    /// (`third_party/exiv2/src/properties.cpp`, `xmpNsInfo`).
    CameraRawSavedSettings,
    /// `dwc` — Darwin Core biodiversity terms in XMP (`dwc:Record`, `dwc:Event`, …).
    ///
    /// URI `http://rs.tdwg.org/dwc/index.htm`, the TDWG Darwin Core namespace as it is used in
    /// XMP; registered by exiv2 (`third_party/exiv2/src/properties.cpp`, `xmpNsInfo`).
    DarwinCore,
}

impl WellKnownNs {
    /// Every standard schema, for iteration and lookup.
    pub const ALL: &'static [WellKnownNs] = &[
        WellKnownNs::DublinCore,
        WellKnownNs::Xmp,
        WellKnownNs::XmpRights,
        WellKnownNs::XmpMediaManagement,
        WellKnownNs::Photoshop,
        WellKnownNs::Exif,
        WellKnownNs::Tiff,
        WellKnownNs::Iptc4XmpCore,
        WellKnownNs::Iptc4XmpExt,
        WellKnownNs::CameraRaw,
        WellKnownNs::XmpIdentifier,
        WellKnownNs::XmpJobTicket,
        WellKnownNs::XmpPagedText,
        WellKnownNs::XmpDynamicMedia,
        WellKnownNs::Pdf,
        WellKnownNs::Dimensions,
        WellKnownNs::ResourceRef,
        WellKnownNs::DcTerms,
        WellKnownNs::ExifEx,
        WellKnownNs::Aux,
        WellKnownNs::Plus,
        WellKnownNs::MwgRegions,
        WellKnownNs::MwgKeywords,
        WellKnownNs::GPano,
        WellKnownNs::Lightroom,
        WellKnownNs::MicrosoftPhoto,
        WellKnownNs::DigiKam,
        WellKnownNs::Acdsee,
        WellKnownNs::CameraRawSavedSettings,
        WellKnownNs::DarwinCore,
    ];

    /// The schema's namespace URI — its canonical identity.
    ///
    /// `dc` is Dublin Core (Part 1 §8.3); the `xmp*` schemas are Part 1 §8.4–8.6; `photoshop`,
    /// `crs` are Part 2 §3.2–3.3. `exif`/`tiff` mirror the EXIF tags into XMP (Part 2 §3.4, defined
    /// by CIPA DC-010); `Iptc4xmpCore`/`Iptc4xmpExt` are the IPTC Photo Metadata schemas;
    /// `dcterms` is the DCMI Metadata Terms namespace (`http://purl.org/dc/terms/`). The URIs of
    /// the schemas outside the published XMP Parts are cited on each variant.
    #[must_use]
    pub const fn uri(self) -> &'static str {
        match self {
            WellKnownNs::DublinCore => "http://purl.org/dc/elements/1.1/",
            WellKnownNs::Xmp => "http://ns.adobe.com/xap/1.0/",
            WellKnownNs::XmpRights => "http://ns.adobe.com/xap/1.0/rights/",
            WellKnownNs::XmpMediaManagement => "http://ns.adobe.com/xap/1.0/mm/",
            WellKnownNs::Photoshop => "http://ns.adobe.com/photoshop/1.0/",
            WellKnownNs::Exif => "http://ns.adobe.com/exif/1.0/",
            WellKnownNs::Tiff => "http://ns.adobe.com/tiff/1.0/",
            WellKnownNs::Iptc4XmpCore => "http://iptc.org/std/Iptc4xmpCore/1.0/xmlns/",
            WellKnownNs::Iptc4XmpExt => "http://iptc.org/std/Iptc4xmpExt/2008-02-29/",
            WellKnownNs::CameraRaw => "http://ns.adobe.com/camera-raw-settings/1.0/",
            WellKnownNs::XmpIdentifier => "http://ns.adobe.com/xmp/Identifier/qual/1.0/",
            WellKnownNs::XmpJobTicket => "http://ns.adobe.com/xap/1.0/bj/",
            WellKnownNs::XmpPagedText => "http://ns.adobe.com/xap/1.0/t/pg/",
            WellKnownNs::XmpDynamicMedia => "http://ns.adobe.com/xmp/1.0/DynamicMedia/",
            WellKnownNs::Pdf => "http://ns.adobe.com/pdf/1.3/",
            WellKnownNs::Dimensions => "http://ns.adobe.com/xap/1.0/sType/Dimensions#",
            WellKnownNs::ResourceRef => "http://ns.adobe.com/xap/1.0/sType/ResourceRef#",
            WellKnownNs::DcTerms => "http://purl.org/dc/terms/",
            WellKnownNs::ExifEx => "http://cipa.jp/exif/1.0/",
            WellKnownNs::Aux => "http://ns.adobe.com/exif/1.0/aux/",
            WellKnownNs::Plus => "http://ns.useplus.org/ldf/xmp/1.0/",
            WellKnownNs::MwgRegions => "http://www.metadataworkinggroup.com/schemas/regions/",
            WellKnownNs::MwgKeywords => "http://www.metadataworkinggroup.com/schemas/keywords/",
            WellKnownNs::GPano => "http://ns.google.com/photos/1.0/panorama/",
            WellKnownNs::Lightroom => "http://ns.adobe.com/lightroom/1.0/",
            WellKnownNs::MicrosoftPhoto => "http://ns.microsoft.com/photo/1.0/",
            WellKnownNs::DigiKam => "http://www.digikam.org/ns/1.0/",
            WellKnownNs::Acdsee => "http://ns.acdsee.com/iptc/1.0/",
            WellKnownNs::CameraRawSavedSettings => {
                "http://ns.adobe.com/camera-raw-saved-settings/1.0/"
            }
            WellKnownNs::DarwinCore => "http://rs.tdwg.org/dwc/index.htm",
        }
    }

    /// The schema's conventional prefix (e.g. `dc`, `xmp`, `Iptc4xmpCore`). The prefix is only a
    /// serialization convenience — the URI is the identity (Part 1 §6.2).
    #[must_use]
    pub const fn prefix(self) -> &'static str {
        match self {
            WellKnownNs::DublinCore => "dc",
            WellKnownNs::Xmp => "xmp",
            WellKnownNs::XmpRights => "xmpRights",
            WellKnownNs::XmpMediaManagement => "xmpMM",
            WellKnownNs::Photoshop => "photoshop",
            WellKnownNs::Exif => "exif",
            WellKnownNs::Tiff => "tiff",
            WellKnownNs::Iptc4XmpCore => "Iptc4xmpCore",
            WellKnownNs::Iptc4XmpExt => "Iptc4xmpExt",
            WellKnownNs::CameraRaw => "crs",
            WellKnownNs::XmpIdentifier => "xmpidq",
            WellKnownNs::XmpJobTicket => "xmpBJ",
            WellKnownNs::XmpPagedText => "xmpTPg",
            WellKnownNs::XmpDynamicMedia => "xmpDM",
            WellKnownNs::Pdf => "pdf",
            WellKnownNs::Dimensions => "stDim",
            WellKnownNs::ResourceRef => "stRef",
            WellKnownNs::DcTerms => "dcterms",
            WellKnownNs::ExifEx => "exifEX",
            WellKnownNs::Aux => "aux",
            WellKnownNs::Plus => "plus",
            WellKnownNs::MwgRegions => "mwg-rs",
            WellKnownNs::MwgKeywords => "mwg-kw",
            WellKnownNs::GPano => "GPano",
            WellKnownNs::Lightroom => "lr",
            WellKnownNs::MicrosoftPhoto => "MicrosoftPhoto",
            WellKnownNs::DigiKam => "digiKam",
            WellKnownNs::Acdsee => "acdsee",
            WellKnownNs::CameraRawSavedSettings => "crss",
            WellKnownNs::DarwinCore => "dwc",
        }
    }

    /// The schema `uri` identifies, if any.
    ///
    /// An exact match against [`WellKnownNs::uri`], plus one **read alias**: Darwin Core is also
    /// recognised under [`DWC_URI_TRAILING_SLASH`], the form Adobe XMPCore emits (see that
    /// constant). The alias is read-only — [`WellKnownNs::uri`] keeps returning the unslashed URI
    /// exiv2 documents, so gamut's own bytes are unchanged — and it is not a [`WellKnownNs::ALL`]
    /// entry, so iteration and the URI/prefix uniqueness of the registry are unaffected.
    ///
    /// **The alias buys a prefix, not a URI.** For every registered URI `from_uri` hands back the
    /// URI it was given — `from_uri(u).map(WellKnownNs::uri) == Some(u)` — and for the alias it does
    /// not. Nothing rewrites the URI a packet carries, so a graph parsed from an XMPCore-written
    /// packet re-serializes under the `dwc` prefix while its properties stay keyed by
    /// [`DWC_URI_TRAILING_SLASH`]: [`XmpMeta::get_text`] with `WellKnownNs::DarwinCore.uri()`
    /// returns `None` for such a graph. Resolve a property by the URI its packet carries.
    ///
    /// [`XmpMeta::get_text`]: crate::XmpMeta::get_text
    #[must_use]
    pub fn from_uri(uri: &str) -> Option<WellKnownNs> {
        WellKnownNs::ALL
            .iter()
            .copied()
            .find(|ns| ns.uri() == uri)
            .or_else(|| (uri == DWC_URI_TRAILING_SLASH).then_some(WellKnownNs::DarwinCore))
    }
}

/// Darwin Core's namespace URI with the trailing slash Adobe XMPCore emits.
///
/// exiv2 appends `/` to any namespace URI ending in neither `/` nor `#` before registering it with
/// XMPCore (`third_party/exiv2/src/properties.cpp`, `XmpProperties::registerNs`), and Darwin Core
/// (`http://rs.tdwg.org/dwc/index.htm`) is the only registered schema whose URI ends in neither.
/// A packet written by exiv2 — including a sidecar it wrote — therefore declares the slashed form,
/// and without this alias such a graph would re-serialize under a synthesized `ns1` prefix instead
/// of `dwc`. Prefixes are non-semantic (Part 1 §6.2), so this is round-trip fidelity, not
/// correctness.
pub const DWC_URI_TRAILING_SLASH: &str = "http://rs.tdwg.org/dwc/index.htm/";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_and_prefix_are_exact_and_round_trip() {
        // Exact strings — a typo in any URI/prefix is a survived mutant otherwise.
        assert_eq!(
            WellKnownNs::DublinCore.uri(),
            "http://purl.org/dc/elements/1.1/"
        );
        assert_eq!(WellKnownNs::DublinCore.prefix(), "dc");
        assert_eq!(WellKnownNs::Xmp.uri(), "http://ns.adobe.com/xap/1.0/");
        assert_eq!(WellKnownNs::Iptc4XmpCore.prefix(), "Iptc4xmpCore");
        assert_eq!(
            WellKnownNs::XmpDynamicMedia.uri(),
            "http://ns.adobe.com/xmp/1.0/DynamicMedia/"
        );
        assert_eq!(WellKnownNs::Dimensions.prefix(), "stDim");
        assert_eq!(WellKnownNs::Pdf.uri(), "http://ns.adobe.com/pdf/1.3/");
        // DCMI Metadata Terms: distinct from Dublin Core *elements* (`/dc/elements/1.1/`) — the
        // two share a vendor path, so a copy-paste of the wrong one is the likely defect.
        assert_eq!(WellKnownNs::DcTerms.uri(), "http://purl.org/dc/terms/");
        assert_eq!(WellKnownNs::DcTerms.prefix(), "dcterms");

        for &ns in WellKnownNs::ALL {
            assert_eq!(WellKnownNs::from_uri(ns.uri()), Some(ns));
            assert!(!ns.prefix().is_empty());
        }
    }

    #[test]
    fn exiv2_documented_schemas_have_exact_uris_and_prefixes() {
        // The twelve schemas added for exiv2 parity (issue #421). Each pair is the exact string
        // exiv2's registry binds (`third_party/exiv2/src/properties.cpp`), so the prefix gamut
        // serializes under is the key the reference engine reads back by; the differential check
        // is `tests/oracle.rs`. Near-misses are the likely defects: `exifEX` vs `exifEx`, `aux/`
        // under `exif/1.0/` (not a sibling of it), `crss` vs the `crs` it complements, and
        // `photo/1.0/` (not exiv2's separate `MP` = `photo/1.2/`).
        let expected = [
            (WellKnownNs::ExifEx, "http://cipa.jp/exif/1.0/", "exifEX"),
            (WellKnownNs::Aux, "http://ns.adobe.com/exif/1.0/aux/", "aux"),
            (
                WellKnownNs::Plus,
                "http://ns.useplus.org/ldf/xmp/1.0/",
                "plus",
            ),
            (
                WellKnownNs::MwgRegions,
                "http://www.metadataworkinggroup.com/schemas/regions/",
                "mwg-rs",
            ),
            (
                WellKnownNs::MwgKeywords,
                "http://www.metadataworkinggroup.com/schemas/keywords/",
                "mwg-kw",
            ),
            (
                WellKnownNs::GPano,
                "http://ns.google.com/photos/1.0/panorama/",
                "GPano",
            ),
            (
                WellKnownNs::Lightroom,
                "http://ns.adobe.com/lightroom/1.0/",
                "lr",
            ),
            (
                WellKnownNs::MicrosoftPhoto,
                "http://ns.microsoft.com/photo/1.0/",
                "MicrosoftPhoto",
            ),
            (
                WellKnownNs::DigiKam,
                "http://www.digikam.org/ns/1.0/",
                "digiKam",
            ),
            (
                WellKnownNs::Acdsee,
                "http://ns.acdsee.com/iptc/1.0/",
                "acdsee",
            ),
            (
                WellKnownNs::CameraRawSavedSettings,
                "http://ns.adobe.com/camera-raw-saved-settings/1.0/",
                "crss",
            ),
            (
                WellKnownNs::DarwinCore,
                "http://rs.tdwg.org/dwc/index.htm",
                "dwc",
            ),
        ];
        for (ns, uri, prefix) in expected {
            assert_eq!(ns.uri(), uri, "{ns:?}");
            assert_eq!(ns.prefix(), prefix, "{ns:?}");
            assert!(
                WellKnownNs::ALL.contains(&ns),
                "{ns:?} must be in ALL so from_uri and the writer's prefix table see it"
            );
        }
    }

    #[test]
    fn registry_holds_thirty_schemas() {
        // A drift guard, deliberately separate from the exiv2-parity test above: every future
        // addition to the registry edits this one line, and it fails for exactly that reason.
        // 18 entries before the exiv2-parity additions (the original 17 plus `dcterms`) + 12 = 30.
        assert_eq!(WellKnownNs::ALL.len(), 30);
    }

    #[test]
    fn from_uri_accepts_the_darwin_core_trailing_slash_alias_without_emitting_it() {
        // XMPCore emits `.../index.htm/`; reading a packet it wrote must still resolve to the
        // registered schema, so the graph re-serializes under `dwc` rather than a synthesized
        // prefix. The alias is read-only: `uri()` still emits the unslashed URI exiv2 documents,
        // and the alias is not an ALL entry.
        assert_eq!(
            WellKnownNs::from_uri(DWC_URI_TRAILING_SLASH),
            Some(WellKnownNs::DarwinCore)
        );
        assert_eq!(
            WellKnownNs::DarwinCore.uri(),
            "http://rs.tdwg.org/dwc/index.htm"
        );
        assert!(
            !WellKnownNs::ALL
                .iter()
                .any(|ns| ns.uri() == DWC_URI_TRAILING_SLASH)
        );
    }

    #[test]
    fn the_trailing_slash_alias_is_the_only_uri_from_uri_does_not_hand_back() {
        // `from_uri(u).map(uri) == Some(u)` — resolving a URI hands the same URI back, because
        // reading never canonicalizes — holds for every registered URI. The Darwin Core read alias
        // is the sole exception, and pinning it here is what keeps it a documented exception rather
        // than a silent hole: a caller that resolved by the slashed URI must keep using it.
        for &ns in WellKnownNs::ALL {
            assert_eq!(
                WellKnownNs::from_uri(ns.uri()).map(WellKnownNs::uri),
                Some(ns.uri()),
                "{ns:?} must hand back the URI it was resolved by"
            );
        }
        assert_eq!(
            WellKnownNs::from_uri(DWC_URI_TRAILING_SLASH).map(WellKnownNs::uri),
            Some("http://rs.tdwg.org/dwc/index.htm"),
            "the alias resolves to the unslashed URI, so it is not an identity"
        );
    }

    #[test]
    fn from_uri_rejects_unknown() {
        assert_eq!(WellKnownNs::from_uri("http://example.com/ns/"), None);
    }

    #[test]
    fn all_uris_and_prefixes_are_unique() {
        for (i, &a) in WellKnownNs::ALL.iter().enumerate() {
            for &b in &WellKnownNs::ALL[i + 1..] {
                assert_ne!(a.uri(), b.uri(), "duplicate URI for {a:?}/{b:?}");
                assert_ne!(a.prefix(), b.prefix(), "duplicate prefix for {a:?}/{b:?}");
            }
        }
    }

    #[test]
    fn namespace_from_well_known_carries_uri_and_prefix() {
        let ns = Namespace::from(WellKnownNs::Photoshop);
        assert_eq!(ns.uri, "http://ns.adobe.com/photoshop/1.0/");
        assert_eq!(ns.prefix, "photoshop");
    }
}
