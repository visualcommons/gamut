//! The IPTC Photo Metadata schema: XMP namespaces and the IIM↔XMP property map.
//!
//! IPTC Core/Extension fields are defined *as* XMP properties (IPTC Photo Metadata Standard
//! 2025.1). This module pins the namespace URIs and the authoritative mapping between legacy IIM
//! datasets and their XMP properties, derived from the IPTC machine-readable technical reference
//! (`references/iptc/iptc-pmd-techreference_2025.1.json`, the `ipmd_top` entries that carry an
//! `IIMid`). The map drives the IIM↔XMP reconciliation; the per-dataset octet limits are not
//! duplicated here — they come from [`crate::iim::IimTagInfo::lookup`].

use gamut_xmp::WellKnownNs;

/// The namespaces gamut treats as IPTC-relevant when extracting [`crate::PhotoMetadata`] from a full
/// XMP graph (see [`crate::PhotoMetadata::from_xmp`]).
///
/// Each URI is named once, by the shared [`gamut_xmp::WellKnownNs`] registry; the tests pin the
/// ones IPTC uses against the IPTC Photo Metadata Standard 2025.1's literal strings.
///
/// [`WellKnownNs::Plus`] is included because the IPTC Extension schema embeds PLUS 1.2 properties
/// — `plus:Licensor` among them — under their own namespace rather than re-declaring them.
/// [`WellKnownNs::Xmp`] and [`WellKnownNs::Exif`] are not: they are general-purpose namespaces
/// whose properties are not IPTC's, and appear only *inside* IPTC Extension structures (the
/// `xmp:Identifier` of an [`Entity`](crate::extension::Entity), the `exif:GPS*` fields of a
/// [`Location`](crate::extension::Location)).
pub const IPTC_NAMESPACES: &[&str] = &[
    WellKnownNs::DublinCore.uri(),
    WellKnownNs::Photoshop.uri(),
    WellKnownNs::XmpRights.uri(),
    WellKnownNs::Iptc4XmpCore.uri(),
    WellKnownNs::Iptc4XmpExt.uri(),
    WellKnownNs::Plus.uri(),
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
    /// The namespace URI (a [`WellKnownNs::uri`]).
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
        xmp: field(
            WellKnownNs::Iptc4XmpCore.uri(),
            "IntellectualGenre",
            SimpleText,
        ),
    },
    FieldMap {
        iim: &[(2, 5)],
        xmp: field(WellKnownNs::DublinCore.uri(), "title", LangAlt),
    },
    FieldMap {
        iim: &[(2, 12)],
        xmp: field(WellKnownNs::Iptc4XmpCore.uri(), "SubjectCode", Bag),
    },
    FieldMap {
        iim: &[(2, 25)],
        xmp: field(WellKnownNs::DublinCore.uri(), "subject", Bag),
    },
    FieldMap {
        iim: &[(2, 40)],
        xmp: field(WellKnownNs::Photoshop.uri(), "Instructions", SimpleText),
    },
    FieldMap {
        iim: &[(2, 55), (2, 60)],
        xmp: field(WellKnownNs::Photoshop.uri(), "DateCreated", DateTime),
    },
    FieldMap {
        iim: &[(2, 80)],
        xmp: field(WellKnownNs::DublinCore.uri(), "creator", Seq),
    },
    FieldMap {
        iim: &[(2, 85)],
        xmp: field(WellKnownNs::Photoshop.uri(), "AuthorsPosition", SimpleText),
    },
    FieldMap {
        iim: &[(2, 90)],
        xmp: field(WellKnownNs::Photoshop.uri(), "City", SimpleText),
    },
    FieldMap {
        iim: &[(2, 92)],
        xmp: field(WellKnownNs::Iptc4XmpCore.uri(), "Location", SimpleText),
    },
    FieldMap {
        iim: &[(2, 95)],
        xmp: field(WellKnownNs::Photoshop.uri(), "State", SimpleText),
    },
    FieldMap {
        iim: &[(2, 100)],
        xmp: field(WellKnownNs::Iptc4XmpCore.uri(), "CountryCode", SimpleText),
    },
    FieldMap {
        iim: &[(2, 101)],
        xmp: field(WellKnownNs::Photoshop.uri(), "Country", SimpleText),
    },
    FieldMap {
        iim: &[(2, 103)],
        xmp: field(
            WellKnownNs::Photoshop.uri(),
            "TransmissionReference",
            SimpleText,
        ),
    },
    FieldMap {
        iim: &[(2, 105)],
        xmp: field(WellKnownNs::Photoshop.uri(), "Headline", SimpleText),
    },
    FieldMap {
        iim: &[(2, 110)],
        xmp: field(WellKnownNs::Photoshop.uri(), "Credit", SimpleText),
    },
    FieldMap {
        iim: &[(2, 115)],
        xmp: field(WellKnownNs::Photoshop.uri(), "Source", SimpleText),
    },
    FieldMap {
        iim: &[(2, 116)],
        xmp: field(WellKnownNs::DublinCore.uri(), "rights", LangAlt),
    },
    FieldMap {
        iim: &[(2, 120)],
        xmp: field(WellKnownNs::DublinCore.uri(), "description", LangAlt),
    },
    FieldMap {
        iim: &[(2, 122)],
        xmp: field(WellKnownNs::Photoshop.uri(), "CaptionWriter", SimpleText),
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
        // The schema names its namespaces through gamut_xmp::WellKnownNs; pin the ones it uses
        // against the literal strings of the IPTC Photo Metadata Standard 2025.1 so a change in
        // the shared registry cannot silently retarget the IPTC schema.
        assert_eq!(
            WellKnownNs::DublinCore.uri(),
            "http://purl.org/dc/elements/1.1/"
        );
        assert_eq!(
            WellKnownNs::Photoshop.uri(),
            "http://ns.adobe.com/photoshop/1.0/"
        );
        assert_eq!(
            WellKnownNs::XmpRights.uri(),
            "http://ns.adobe.com/xap/1.0/rights/"
        );
        assert_eq!(
            WellKnownNs::Iptc4XmpCore.uri(),
            "http://iptc.org/std/Iptc4xmpCore/1.0/xmlns/"
        );
        assert_eq!(
            WellKnownNs::Iptc4XmpExt.uri(),
            "http://iptc.org/std/Iptc4xmpExt/2008-02-29/"
        );
        assert_eq!(
            WellKnownNs::Plus.uri(),
            "http://ns.useplus.org/ldf/xmp/1.0/"
        );
        assert_eq!(WellKnownNs::Xmp.uri(), "http://ns.adobe.com/xap/1.0/");
        assert_eq!(WellKnownNs::Exif.uri(), "http://ns.adobe.com/exif/1.0/");
    }

    #[test]
    fn iptc_namespaces_carries_plus_but_not_xmp_basic() {
        // The IPTC Extension defines properties in the PLUS namespace, so a graph filtered by
        // IPTC_NAMESPACES must keep them; xmp: and exif: are general-purpose and must not be
        // swept in — they appear only inside IPTC structures.
        assert!(IPTC_NAMESPACES.contains(&WellKnownNs::Plus.uri()));
        assert!(!IPTC_NAMESPACES.contains(&WellKnownNs::Xmp.uri()));
        assert!(!IPTC_NAMESPACES.contains(&WellKnownNs::Exif.uri()));
    }
}
