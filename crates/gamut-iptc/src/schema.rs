//! The IPTC Photo Metadata schema: XMP namespaces and the IIM↔XMP property map.
//!
//! IPTC Core/Extension fields are defined *as* XMP properties (IPTC Photo Metadata Standard
//! 2025.1). This module pins the namespace URIs and the authoritative mapping between legacy IIM
//! datasets and their XMP properties, derived from the IPTC machine-readable technical reference
//! (`references/iptc/iptc-pmd-techreference_2025.1.json`, the `ipmd_top` entries that carry an
//! `IIMid`). The map drives the IIM↔XMP reconciliation; the per-dataset octet limits are not
//! duplicated here — they come from [`crate::iim::IimTagInfo::lookup`].

/// XMP namespace URIs used by IPTC Photo Metadata (verified against the IPTC Photo Metadata
/// Standard 2025.1).
///
/// The URIs come from the shared [`gamut_xmp::WellKnownNs`] registry rather than being
/// re-declared; the tests pin them against the standard's literal strings.
pub mod ns {
    use gamut_xmp::WellKnownNs;

    /// Dublin Core — `dc:` (title, creator, description, subject, rights).
    pub const DC: &str = WellKnownNs::DublinCore.uri();
    /// Adobe Photoshop — `photoshop:` (City, Country, Headline, Credit, …).
    pub const PHOTOSHOP: &str = WellKnownNs::Photoshop.uri();
    /// XMP Rights Management — `xmpRights:` (UsageTerms, WebStatement).
    pub const XMP_RIGHTS: &str = WellKnownNs::XmpRights.uri();
    /// IPTC Photo Metadata Core — `Iptc4xmpCore:`.
    pub const IPTC_CORE: &str = WellKnownNs::Iptc4XmpCore.uri();
    /// IPTC Photo Metadata Extension — `Iptc4xmpExt:`.
    pub const IPTC_EXT: &str = WellKnownNs::Iptc4XmpExt.uri();
    /// PLUS (Picture Licensing Universal System) Licensing Data Format — `plus:`.
    ///
    /// The IPTC Extension schema embeds PLUS 1.2 properties under their own namespace rather than
    /// re-declaring them; `plus:Licensor` (see [`crate::extension::Licensor`]) is the one gamut
    /// models. Declared here rather than taken from [`gamut_xmp::WellKnownNs`], which does not
    /// carry PLUS.
    pub const PLUS: &str = "http://ns.useplus.org/ldf/xmp/1.0/";
    /// XMP basic — `xmp:`.
    ///
    /// Not an IPTC namespace, and deliberately absent from [`IPTC_NAMESPACES`]; it appears only
    /// *inside* IPTC Extension structures, as the `xmp:Identifier` field of an
    /// [`Entity`](crate::extension::Entity).
    pub const XMP: &str = WellKnownNs::Xmp.uri();
}

/// The namespaces gamut treats as IPTC-relevant when extracting [`crate::PhotoMetadata`] from a full
/// XMP graph (see [`crate::PhotoMetadata::from_xmp`]).
///
/// [`ns::PLUS`] is included because the IPTC Extension schema defines several of its own properties
/// — `plus:Licensor` among them — in the PLUS namespace; [`ns::XMP`] is not, because `xmp:` is a
/// general-purpose namespace whose properties are not IPTC's.
pub const IPTC_NAMESPACES: &[&str] = &[
    ns::DC,
    ns::PHOTOSHOP,
    ns::XMP_RIGHTS,
    ns::IPTC_CORE,
    ns::IPTC_EXT,
    ns::PLUS,
];

/// How an IPTC field is shaped as an XMP value.
///
/// Marked `#[non_exhaustive]`: IPTC Extension structures may need further shapes post-1.0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum XmpShape {
    /// A simple text property (e.g. `photoshop:City`).
    SimpleText,
    /// A language-alternative array; gamut reads/writes the `x-default` alternative (e.g.
    /// `dc:description`).
    LangAlt,
    /// An unordered `rdf:Bag` of text (e.g. `dc:subject` keywords).
    Bag,
    /// An ordered `rdf:Seq` of text (e.g. `dc:creator`).
    Seq,
    /// A date-time text property assembled from two IIM datasets (e.g. `photoshop:DateCreated`).
    DateTime,
}

/// One XMP property's identity and shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct XmpField {
    /// The namespace URI (one of [`ns`]).
    pub ns: &'static str,
    /// The local property name.
    pub name: &'static str,
    /// How the value is shaped as XMP.
    pub shape: XmpShape,
}

/// The mapping of one IPTC field between its IIM dataset(s) and its XMP property.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FieldMap {
    /// The IIM `(record, dataset)` tag(s). Usually one; `DateTime` maps the date (2:55) and time
    /// (2:60) datasets together.
    pub iim: &'static [(u8, u8)],
    /// The XMP property this field maps to.
    pub xmp: XmpField,
}

use XmpShape::{Bag, DateTime, LangAlt, Seq, SimpleText};

const fn field(ns: &'static str, name: &'static str, shape: XmpShape) -> XmpField {
    XmpField { ns, name, shape }
}

/// The authoritative IIM↔XMP map (the 20 `ipmd_top` properties that carry an `IIMid`).
///
/// Each entry pairs the IIM dataset(s) with the XMP property and its shape. `2:04`/`2:85` are
/// repeatable on the IIM wire but map to single XMP properties (gamut reconciles the first value);
/// `2:55`+`2:60` together form the `photoshop:DateCreated` date-time.
///
/// Together with [`crate::PhotoMetadata::get_field`]/[`set_field`](crate::PhotoMetadata::set_field)
/// this enables generic, table-driven access to every mapped field:
///
/// ```
/// use gamut_iptc::{PhotoMetadata, schema::FIELD_MAP};
///
/// let mut pm = PhotoMetadata::new();
/// pm.set_city("Lyon");
/// let present: Vec<&str> = FIELD_MAP
///     .iter()
///     .filter(|row| !pm.get_field(&row.xmp).is_empty())
///     .map(|row| row.xmp.name)
///     .collect();
/// assert_eq!(present, ["City"]);
/// ```
pub const FIELD_MAP: &[FieldMap] = &[
    FieldMap {
        iim: &[(2, 4)],
        xmp: field(ns::IPTC_CORE, "IntellectualGenre", SimpleText),
    },
    FieldMap {
        iim: &[(2, 5)],
        xmp: field(ns::DC, "title", LangAlt),
    },
    FieldMap {
        iim: &[(2, 12)],
        xmp: field(ns::IPTC_CORE, "SubjectCode", Bag),
    },
    FieldMap {
        iim: &[(2, 25)],
        xmp: field(ns::DC, "subject", Bag),
    },
    FieldMap {
        iim: &[(2, 40)],
        xmp: field(ns::PHOTOSHOP, "Instructions", SimpleText),
    },
    FieldMap {
        iim: &[(2, 55), (2, 60)],
        xmp: field(ns::PHOTOSHOP, "DateCreated", DateTime),
    },
    FieldMap {
        iim: &[(2, 80)],
        xmp: field(ns::DC, "creator", Seq),
    },
    FieldMap {
        iim: &[(2, 85)],
        xmp: field(ns::PHOTOSHOP, "AuthorsPosition", SimpleText),
    },
    FieldMap {
        iim: &[(2, 90)],
        xmp: field(ns::PHOTOSHOP, "City", SimpleText),
    },
    FieldMap {
        iim: &[(2, 92)],
        xmp: field(ns::IPTC_CORE, "Location", SimpleText),
    },
    FieldMap {
        iim: &[(2, 95)],
        xmp: field(ns::PHOTOSHOP, "State", SimpleText),
    },
    FieldMap {
        iim: &[(2, 100)],
        xmp: field(ns::IPTC_CORE, "CountryCode", SimpleText),
    },
    FieldMap {
        iim: &[(2, 101)],
        xmp: field(ns::PHOTOSHOP, "Country", SimpleText),
    },
    FieldMap {
        iim: &[(2, 103)],
        xmp: field(ns::PHOTOSHOP, "TransmissionReference", SimpleText),
    },
    FieldMap {
        iim: &[(2, 105)],
        xmp: field(ns::PHOTOSHOP, "Headline", SimpleText),
    },
    FieldMap {
        iim: &[(2, 110)],
        xmp: field(ns::PHOTOSHOP, "Credit", SimpleText),
    },
    FieldMap {
        iim: &[(2, 115)],
        xmp: field(ns::PHOTOSHOP, "Source", SimpleText),
    },
    FieldMap {
        iim: &[(2, 116)],
        xmp: field(ns::DC, "rights", LangAlt),
    },
    FieldMap {
        iim: &[(2, 120)],
        xmp: field(ns::DC, "description", LangAlt),
    },
    FieldMap {
        iim: &[(2, 122)],
        xmp: field(ns::PHOTOSHOP, "CaptionWriter", SimpleText),
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_mapped_iim_dataset_has_tag_info() {
        // The map and the IIM known-tag table must agree on which datasets are modelled.
        for row in FIELD_MAP {
            for &(record, dataset) in row.iim {
                assert!(
                    crate::iim::IimTagInfo::lookup(record, dataset).is_some(),
                    "{record}:{dataset} is mapped but missing from the IIM tag table"
                );
            }
        }
    }

    #[test]
    fn datetime_is_the_only_two_dataset_row() {
        for row in FIELD_MAP {
            let expected = if row.xmp.shape == XmpShape::DateTime {
                2
            } else {
                1
            };
            assert_eq!(row.iim.len(), expected, "{} arity", row.xmp.name);
        }
    }

    #[test]
    fn known_namespaces_are_iptc_relevant() {
        for row in FIELD_MAP {
            assert!(IPTC_NAMESPACES.contains(&row.xmp.ns));
        }
    }

    #[test]
    fn namespace_uris_match_the_iptc_standard() {
        // The consts are derived from gamut_xmp::WellKnownNs; pin them against the literal
        // strings of the IPTC Photo Metadata Standard 2025.1 so a change in the shared registry
        // cannot silently retarget the IPTC schema.
        assert_eq!(ns::DC, "http://purl.org/dc/elements/1.1/");
        assert_eq!(ns::PHOTOSHOP, "http://ns.adobe.com/photoshop/1.0/");
        assert_eq!(ns::XMP_RIGHTS, "http://ns.adobe.com/xap/1.0/rights/");
        assert_eq!(ns::IPTC_CORE, "http://iptc.org/std/Iptc4xmpCore/1.0/xmlns/");
        assert_eq!(ns::IPTC_EXT, "http://iptc.org/std/Iptc4xmpExt/2008-02-29/");
        assert_eq!(ns::PLUS, "http://ns.useplus.org/ldf/xmp/1.0/");
        assert_eq!(ns::XMP, "http://ns.adobe.com/xap/1.0/");
    }

    #[test]
    fn iptc_namespaces_carries_plus_but_not_xmp_basic() {
        // The IPTC Extension defines properties in the PLUS namespace, so a graph filtered by
        // IPTC_NAMESPACES must keep them; xmp: is general-purpose and must not be swept in.
        assert!(IPTC_NAMESPACES.contains(&ns::PLUS));
        assert!(!IPTC_NAMESPACES.contains(&ns::XMP));
    }
}
