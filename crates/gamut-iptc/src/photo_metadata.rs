//! The modern IPTC Photo Metadata (Core + Extension), expressed over XMP.

use gamut_xmp::{XmpArray, XmpItem, XmpMeta, XmpProperty, XmpValue};

use crate::schema::{IPTC_NAMESPACES, XmpField, XmpShape, ns};

/// IPTC Photo Metadata (Core + Extension), the modern standard.
///
/// These properties are defined *as* XMP (in the `dc:`/`photoshop:`/`Iptc4xmpCore:` namespaces), so
/// the model is carried as [`gamut_xmp`] properties rather than a parallel type hierarchy. Typed
/// accessors cover every scalar/list Core field, and the structured `Iptc4xmpCore:CreatorContactInfo`
/// and every IPTC Extension structure have typed accessor pairs defined in
/// [`crate::extension`] (such as [`PhotoMetadata::locations_created`]). Everything stays
/// accessible (and round-trips losslessly) through [`PhotoMetadata::xmp`], and every IIM-mapped
/// field also through
/// [`PhotoMetadata::get_field`]/[`set_field`](PhotoMetadata::set_field) with
/// [`crate::schema::FIELD_MAP`].
///
/// gamut-iptc operates on this in-memory graph; parsing/serializing the XMP *packet bytes* is
/// [`gamut_xmp`]'s responsibility (see issue #34). Language-alternative fields (`dc:title`,
/// `dc:rights`, `dc:description`) are read and written as their `x-default` alternative.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PhotoMetadata {
    /// The IPTC Core/Extension properties, as an XMP property graph in the IPTC namespaces.
    pub xmp: XmpMeta,
}

impl PhotoMetadata {
    /// Creates an empty set of IPTC Photo Metadata.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Extracts the IPTC-relevant properties (the `dc:`/`photoshop:`/`xmpRights:`/`Iptc4xmp*:`
    /// namespaces) from a parsed XMP graph.
    #[must_use]
    pub fn from_xmp(meta: &XmpMeta) -> Self {
        Self {
            xmp: XmpMeta {
                properties: meta
                    .properties
                    .iter()
                    .filter(|p| IPTC_NAMESPACES.contains(&p.namespace.as_str()))
                    .cloned()
                    .collect(),
            },
        }
    }

    /// The IPTC properties as an XMP graph, ready for serialization by [`gamut_xmp`] (or merging
    /// into a larger graph). The symmetric inverse of [`PhotoMetadata::from_xmp`].
    #[must_use]
    pub fn to_xmp(&self) -> XmpMeta {
        self.xmp.clone()
    }

    pub(crate) fn simple(&self, ns: &str, name: &str) -> Option<&str> {
        self.xmp.get_text(ns, name)
    }

    pub(crate) fn set_simple(&mut self, ns: &str, name: &str, value: &str) {
        self.xmp.set_text(ns, name, value);
    }

    /// Reads the first alternative of a language-alternative property, tolerating a plain simple
    /// value (non-conformant but seen in the wild).
    pub(crate) fn lang_alt(&self, ns: &str, name: &str) -> Option<&str> {
        match &self.xmp.get(ns, name)?.value {
            XmpValue::Array(XmpArray::Alt(items)) => items.iter().find_map(XmpItem::text),
            value => value.text(),
        }
    }

    pub(crate) fn set_lang_alt(&mut self, ns: &str, name: &str, value: &str) {
        self.xmp.set_lang_alt(ns, name, "x-default", value);
    }

    pub(crate) fn list(&self, ns: &str, name: &str) -> Vec<&str> {
        match self.xmp.get_array(ns, name) {
            Some(array @ (XmpArray::Bag(_) | XmpArray::Seq(_))) => array.texts().collect(),
            _ => Vec::new(),
        }
    }

    pub(crate) fn set_list(&mut self, ns: &str, name: &str, ordered: bool, values: &[&str]) {
        let items = values.iter().map(|s| XmpItem::simple(*s)).collect();
        let array = if ordered {
            XmpArray::Seq(items)
        } else {
            XmpArray::Bag(items)
        };
        self.xmp
            .set(XmpProperty::new(ns, name, XmpValue::Array(array)));
    }

    /// Reads a field's value(s) by its schema identity, as owned strings (empty if absent).
    ///
    /// Scalar shapes ([`XmpShape::SimpleText`], [`XmpShape::LangAlt`], [`XmpShape::DateTime`])
    /// yield zero or one element; array shapes yield every item. Together with
    /// [`crate::schema::FIELD_MAP`] this gives generic, table-driven access to every mapped field
    /// without a typed accessor per field.
    #[must_use]
    pub fn get_field(&self, field: &XmpField) -> Vec<String> {
        match field.shape {
            XmpShape::SimpleText | XmpShape::DateTime => self
                .simple(field.ns, field.name)
                .map(|s| vec![s.to_owned()])
                .unwrap_or_default(),
            XmpShape::LangAlt => self
                .lang_alt(field.ns, field.name)
                .map(|s| vec![s.to_owned()])
                .unwrap_or_default(),
            XmpShape::Bag | XmpShape::Seq => self
                .list(field.ns, field.name)
                .iter()
                .map(|s| (*s).to_owned())
                .collect(),
        }
    }

    /// Writes a field's value(s) by its schema identity, storing the RDF container kind the shape
    /// prescribes.
    ///
    /// Scalar shapes take the first value; an empty value list leaves the property unset. The
    /// counterpart of [`PhotoMetadata::get_field`].
    pub fn set_field(&mut self, field: &XmpField, values: &[&str]) {
        match field.shape {
            XmpShape::SimpleText | XmpShape::DateTime => {
                if let Some(v) = values.first() {
                    self.set_simple(field.ns, field.name, v);
                }
            }
            XmpShape::LangAlt => {
                if let Some(v) = values.first() {
                    self.set_lang_alt(field.ns, field.name, v);
                }
            }
            XmpShape::Bag | XmpShape::Seq => {
                self.set_list(field.ns, field.name, field.shape == XmpShape::Seq, values);
            }
        }
    }

    // --- Typed accessors for the well-known IPTC Core fields ---

    /// The headline (`photoshop:Headline`).
    #[must_use]
    pub fn headline(&self) -> Option<&str> {
        self.simple(ns::PHOTOSHOP, "Headline")
    }
    /// Sets the headline (`photoshop:Headline`).
    pub fn set_headline(&mut self, value: &str) {
        self.set_simple(ns::PHOTOSHOP, "Headline", value);
    }

    /// The city (`photoshop:City`).
    #[must_use]
    pub fn city(&self) -> Option<&str> {
        self.simple(ns::PHOTOSHOP, "City")
    }
    /// Sets the city (`photoshop:City`).
    pub fn set_city(&mut self, value: &str) {
        self.set_simple(ns::PHOTOSHOP, "City", value);
    }

    /// The country name (`photoshop:Country`).
    #[must_use]
    pub fn country(&self) -> Option<&str> {
        self.simple(ns::PHOTOSHOP, "Country")
    }
    /// Sets the country name (`photoshop:Country`).
    pub fn set_country(&mut self, value: &str) {
        self.set_simple(ns::PHOTOSHOP, "Country", value);
    }

    /// The ISO country code (`Iptc4xmpCore:CountryCode`).
    #[must_use]
    pub fn country_code(&self) -> Option<&str> {
        self.simple(ns::IPTC_CORE, "CountryCode")
    }
    /// Sets the ISO country code (`Iptc4xmpCore:CountryCode`).
    pub fn set_country_code(&mut self, value: &str) {
        self.set_simple(ns::IPTC_CORE, "CountryCode", value);
    }

    /// The caption/description, `x-default` alternative (`dc:description`).
    #[must_use]
    pub fn caption(&self) -> Option<&str> {
        self.lang_alt(ns::DC, "description")
    }
    /// Sets the caption/description `x-default` alternative (`dc:description`).
    pub fn set_caption(&mut self, value: &str) {
        self.set_lang_alt(ns::DC, "description", value);
    }

    /// The copyright notice, `x-default` alternative (`dc:rights`).
    #[must_use]
    pub fn copyright_notice(&self) -> Option<&str> {
        self.lang_alt(ns::DC, "rights")
    }
    /// Sets the copyright notice `x-default` alternative (`dc:rights`).
    pub fn set_copyright_notice(&mut self, value: &str) {
        self.set_lang_alt(ns::DC, "rights", value);
    }

    /// The title/object name, `x-default` alternative (`dc:title`).
    #[must_use]
    pub fn title(&self) -> Option<&str> {
        self.lang_alt(ns::DC, "title")
    }
    /// Sets the title/object name `x-default` alternative (`dc:title`).
    pub fn set_title(&mut self, value: &str) {
        self.set_lang_alt(ns::DC, "title", value);
    }

    /// The keywords (`dc:subject`).
    #[must_use]
    pub fn keywords(&self) -> Vec<&str> {
        self.list(ns::DC, "subject")
    }
    /// Sets the keywords (`dc:subject`, an unordered bag).
    pub fn set_keywords(&mut self, keywords: &[&str]) {
        self.set_list(ns::DC, "subject", false, keywords);
    }

    /// The creators/by-lines (`dc:creator`).
    #[must_use]
    pub fn creators(&self) -> Vec<&str> {
        self.list(ns::DC, "creator")
    }
    /// Sets the creators/by-lines (`dc:creator`, an ordered sequence).
    pub fn set_creators(&mut self, creators: &[&str]) {
        self.set_list(ns::DC, "creator", true, creators);
    }

    /// The usage terms, `x-default` alternative (`xmpRights:UsageTerms`). This is an XMP-only field
    /// with no IIM equivalent.
    #[must_use]
    pub fn usage_terms(&self) -> Option<&str> {
        self.lang_alt(ns::XMP_RIGHTS, "UsageTerms")
    }
    /// Sets the usage terms `x-default` alternative (`xmpRights:UsageTerms`).
    pub fn set_usage_terms(&mut self, value: &str) {
        self.set_lang_alt(ns::XMP_RIGHTS, "UsageTerms", value);
    }

    /// The intellectual genre (`Iptc4xmpCore:IntellectualGenre`).
    #[must_use]
    pub fn intellectual_genre(&self) -> Option<&str> {
        self.simple(ns::IPTC_CORE, "IntellectualGenre")
    }
    /// Sets the intellectual genre (`Iptc4xmpCore:IntellectualGenre`).
    pub fn set_intellectual_genre(&mut self, value: &str) {
        self.set_simple(ns::IPTC_CORE, "IntellectualGenre", value);
    }

    /// The special instructions (`photoshop:Instructions`).
    #[must_use]
    pub fn instructions(&self) -> Option<&str> {
        self.simple(ns::PHOTOSHOP, "Instructions")
    }
    /// Sets the special instructions (`photoshop:Instructions`).
    pub fn set_instructions(&mut self, value: &str) {
        self.set_simple(ns::PHOTOSHOP, "Instructions", value);
    }

    /// The creation date-time as an ISO-8601 string (`photoshop:DateCreated`).
    #[must_use]
    pub fn date_created(&self) -> Option<&str> {
        self.simple(ns::PHOTOSHOP, "DateCreated")
    }
    /// Sets the creation date-time (`photoshop:DateCreated`).
    ///
    /// The value is stored verbatim — XMP legally carries ISO-8601 forms IIM cannot express (e.g.
    /// fractional seconds). Validation happens at IIM projection:
    /// [`IptcWriter::write_iim`](crate::writer::IptcWriter::write_iim) rejects a value it cannot
    /// split into the `2:55`/`2:60` datasets.
    pub fn set_date_created(&mut self, iso: &str) {
        self.set_simple(ns::PHOTOSHOP, "DateCreated", iso);
    }

    /// The creator's job title (`photoshop:AuthorsPosition`).
    #[must_use]
    pub fn authors_position(&self) -> Option<&str> {
        self.simple(ns::PHOTOSHOP, "AuthorsPosition")
    }
    /// Sets the creator's job title (`photoshop:AuthorsPosition`).
    pub fn set_authors_position(&mut self, value: &str) {
        self.set_simple(ns::PHOTOSHOP, "AuthorsPosition", value);
    }

    /// The sublocation within a city (`Iptc4xmpCore:Location`).
    ///
    /// Named `sublocation` after the IIM dataset (2:92 Sub-location) to avoid confusion with the
    /// IPTC Extension's structured location properties
    /// ([`locations_created`](Self::locations_created),
    /// [`locations_shown`](Self::locations_shown)), whose
    /// [`Location::sublocation`](crate::Location::sublocation) is an independent property.
    #[must_use]
    pub fn sublocation(&self) -> Option<&str> {
        self.simple(ns::IPTC_CORE, "Location")
    }
    /// Sets the sublocation within a city (`Iptc4xmpCore:Location`).
    pub fn set_sublocation(&mut self, value: &str) {
        self.set_simple(ns::IPTC_CORE, "Location", value);
    }

    /// The province or state (`photoshop:State`).
    #[must_use]
    pub fn state(&self) -> Option<&str> {
        self.simple(ns::PHOTOSHOP, "State")
    }
    /// Sets the province or state (`photoshop:State`).
    pub fn set_state(&mut self, value: &str) {
        self.set_simple(ns::PHOTOSHOP, "State", value);
    }

    /// The job identifier / original transmission reference (`photoshop:TransmissionReference`).
    #[must_use]
    pub fn transmission_reference(&self) -> Option<&str> {
        self.simple(ns::PHOTOSHOP, "TransmissionReference")
    }
    /// Sets the job identifier / original transmission reference
    /// (`photoshop:TransmissionReference`).
    pub fn set_transmission_reference(&mut self, value: &str) {
        self.set_simple(ns::PHOTOSHOP, "TransmissionReference", value);
    }

    /// The credit line (`photoshop:Credit`).
    #[must_use]
    pub fn credit(&self) -> Option<&str> {
        self.simple(ns::PHOTOSHOP, "Credit")
    }
    /// Sets the credit line (`photoshop:Credit`).
    pub fn set_credit(&mut self, value: &str) {
        self.set_simple(ns::PHOTOSHOP, "Credit", value);
    }

    /// The source of the image (`photoshop:Source`).
    #[must_use]
    pub fn source(&self) -> Option<&str> {
        self.simple(ns::PHOTOSHOP, "Source")
    }
    /// Sets the source of the image (`photoshop:Source`).
    pub fn set_source(&mut self, value: &str) {
        self.set_simple(ns::PHOTOSHOP, "Source", value);
    }

    /// The caption writer/editor (`photoshop:CaptionWriter`).
    #[must_use]
    pub fn caption_writer(&self) -> Option<&str> {
        self.simple(ns::PHOTOSHOP, "CaptionWriter")
    }
    /// Sets the caption writer/editor (`photoshop:CaptionWriter`).
    pub fn set_caption_writer(&mut self, value: &str) {
        self.set_simple(ns::PHOTOSHOP, "CaptionWriter", value);
    }

    /// The subject codes from the IPTC Subject NewsCodes vocabulary (`Iptc4xmpCore:SubjectCode`).
    #[must_use]
    pub fn subject_codes(&self) -> Vec<&str> {
        self.list(ns::IPTC_CORE, "SubjectCode")
    }
    /// Sets the subject codes (`Iptc4xmpCore:SubjectCode`, an unordered bag).
    pub fn set_subject_codes(&mut self, codes: &[&str]) {
        self.set_list(ns::IPTC_CORE, "SubjectCode", false, codes);
    }

    /// The scene codes from the IPTC Scene NewsCodes vocabulary (`Iptc4xmpCore:Scene`). This is an
    /// XMP-only field with no IIM equivalent.
    #[must_use]
    pub fn scene_codes(&self) -> Vec<&str> {
        self.list(ns::IPTC_CORE, "Scene")
    }
    /// Sets the scene codes (`Iptc4xmpCore:Scene`, an unordered bag).
    pub fn set_scene_codes(&mut self, codes: &[&str]) {
        self.set_list(ns::IPTC_CORE, "Scene", false, codes);
    }

    /// The accessibility alt text, `x-default` alternative (`Iptc4xmpCore:AltTextAccessibility`).
    /// This is an XMP-only field with no IIM equivalent.
    #[must_use]
    pub fn alt_text_accessibility(&self) -> Option<&str> {
        self.lang_alt(ns::IPTC_CORE, "AltTextAccessibility")
    }
    /// Sets the accessibility alt text `x-default` alternative
    /// (`Iptc4xmpCore:AltTextAccessibility`).
    pub fn set_alt_text_accessibility(&mut self, value: &str) {
        self.set_lang_alt(ns::IPTC_CORE, "AltTextAccessibility", value);
    }

    /// The extended accessibility description, `x-default` alternative
    /// (`Iptc4xmpCore:ExtDescrAccessibility`). This is an XMP-only field with no IIM equivalent.
    #[must_use]
    pub fn extended_description_accessibility(&self) -> Option<&str> {
        self.lang_alt(ns::IPTC_CORE, "ExtDescrAccessibility")
    }
    /// Sets the extended accessibility description `x-default` alternative
    /// (`Iptc4xmpCore:ExtDescrAccessibility`).
    pub fn set_extended_description_accessibility(&mut self, value: &str) {
        self.set_lang_alt(ns::IPTC_CORE, "ExtDescrAccessibility", value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_typed_accessor_round_trips_its_property() {
        // Each accessor must read back exactly what its setter wrote, on the expected property.
        let mut pm = PhotoMetadata::new();
        pm.set_headline("Breaking");
        pm.set_city("Paris");
        pm.set_country("France");
        pm.set_country_code("FRA");
        pm.set_caption("A photo");
        pm.set_copyright_notice("(c) 2024");
        pm.set_title("Sunset");
        pm.set_usage_terms("CC-BY");
        pm.set_keywords(&["sky", "sea"]);
        pm.set_creators(&["Ansel"]);
        pm.set_intellectual_genre("Documentary");
        pm.set_instructions("Embargoed until Friday");
        pm.set_date_created("2024-06-15T12:00:00Z");
        pm.set_authors_position("Staff Photographer");
        pm.set_sublocation("Rive Gauche");
        pm.set_state("Île-de-France");
        pm.set_transmission_reference("JOB-42");
        pm.set_credit("Agence gamut");
        pm.set_source("gamut wire");
        pm.set_caption_writer("Ed");
        pm.set_subject_codes(&["15054000"]);
        pm.set_scene_codes(&["011900"]);
        pm.set_alt_text_accessibility("A sunset over the Seine");
        pm.set_extended_description_accessibility("A long red sunset over the Seine, from a bank");

        assert_eq!(pm.headline(), Some("Breaking"));
        assert_eq!(pm.city(), Some("Paris"));
        assert_eq!(pm.country(), Some("France"));
        assert_eq!(pm.country_code(), Some("FRA"));
        assert_eq!(pm.caption(), Some("A photo"));
        assert_eq!(pm.copyright_notice(), Some("(c) 2024"));
        assert_eq!(pm.title(), Some("Sunset"));
        assert_eq!(pm.usage_terms(), Some("CC-BY"));
        assert_eq!(pm.keywords(), vec!["sky", "sea"]);
        assert_eq!(pm.creators(), vec!["Ansel"]);
        assert_eq!(pm.intellectual_genre(), Some("Documentary"));
        assert_eq!(pm.instructions(), Some("Embargoed until Friday"));
        assert_eq!(pm.date_created(), Some("2024-06-15T12:00:00Z"));
        assert_eq!(pm.authors_position(), Some("Staff Photographer"));
        assert_eq!(pm.sublocation(), Some("Rive Gauche"));
        assert_eq!(pm.state(), Some("Île-de-France"));
        assert_eq!(pm.transmission_reference(), Some("JOB-42"));
        assert_eq!(pm.credit(), Some("Agence gamut"));
        assert_eq!(pm.source(), Some("gamut wire"));
        assert_eq!(pm.caption_writer(), Some("Ed"));
        assert_eq!(pm.subject_codes(), vec!["15054000"]);
        assert_eq!(pm.scene_codes(), vec!["011900"]);
        assert_eq!(pm.alt_text_accessibility(), Some("A sunset over the Seine"));
        assert_eq!(
            pm.extended_description_accessibility(),
            Some("A long red sunset over the Seine, from a bank")
        );

        // Each accessor targets a distinct property — 24 fields, 24 properties.
        assert_eq!(pm.xmp.properties.len(), 24);
        // Spot-check namespaces/names so an accessor can't silently target the wrong property.
        assert!(pm.xmp.get(ns::PHOTOSHOP, "Headline").is_some());
        assert!(pm.xmp.get(ns::PHOTOSHOP, "DateCreated").is_some());
        assert!(pm.xmp.get(ns::IPTC_CORE, "CountryCode").is_some());
        assert!(pm.xmp.get(ns::IPTC_CORE, "IntellectualGenre").is_some());
        assert!(pm.xmp.get(ns::IPTC_CORE, "Location").is_some());
        assert!(pm.xmp.get(ns::IPTC_CORE, "SubjectCode").is_some());
        assert!(pm.xmp.get(ns::IPTC_CORE, "Scene").is_some());
        assert!(pm.xmp.get(ns::IPTC_CORE, "AltTextAccessibility").is_some());
        assert!(pm.xmp.get(ns::IPTC_CORE, "ExtDescrAccessibility").is_some());
        assert!(pm.xmp.get(ns::DC, "rights").is_some());
        assert!(pm.xmp.get(ns::XMP_RIGHTS, "UsageTerms").is_some());

        // Every IIM-mapped field is settable through its typed accessor: the 20-row FIELD_MAP
        // must see a value for each of its rows.
        for row in crate::schema::FIELD_MAP {
            assert!(
                !pm.get_field(&row.xmp).is_empty(),
                "no typed accessor populated {}",
                row.xmp.name
            );
        }
    }

    #[test]
    fn simple_accessors_set_and_replace() {
        let mut pm = PhotoMetadata::new();
        assert_eq!(pm.city(), None);
        pm.set_city("Paris");
        assert_eq!(pm.city(), Some("Paris"));
        // Setting again replaces rather than duplicating.
        pm.set_city("Lyon");
        assert_eq!(pm.city(), Some("Lyon"));
        assert_eq!(pm.xmp.properties.len(), 1);
    }

    #[test]
    fn lang_alt_reads_x_default_first() {
        let mut pm = PhotoMetadata::new();
        pm.set_caption("A sunset");
        assert_eq!(pm.caption(), Some("A sunset"));
        // A multi-alternative Alt array reads its first (x-default) item.
        pm.xmp.properties[0].value = XmpValue::Array(XmpArray::Alt(vec![
            XmpItem::new(XmpValue::Simple("x-default text".to_owned())),
            XmpItem::new(XmpValue::Simple("French text".to_owned())),
        ]));
        assert_eq!(pm.caption(), Some("x-default text"));
        // A plain Simple value (non-conformant but seen in the wild) is also accepted.
        pm.xmp.properties[0].value = XmpValue::Simple("plain".to_owned());
        assert_eq!(pm.caption(), Some("plain"));
    }

    #[test]
    fn array_accessors_preserve_order_and_kind() {
        let mut pm = PhotoMetadata::new();
        pm.set_keywords(&["sky", "sea"]);
        pm.set_creators(&["Ansel", "Dorothea"]);
        assert_eq!(pm.keywords(), vec!["sky", "sea"]);
        assert_eq!(pm.creators(), vec!["Ansel", "Dorothea"]);
        // dc:subject is a Bag, dc:creator a Seq.
        let subject = pm.xmp.get(ns::DC, "subject").unwrap();
        assert!(matches!(subject.value, XmpValue::Array(XmpArray::Bag(_))));
        let creator = pm.xmp.get(ns::DC, "creator").unwrap();
        assert!(matches!(creator.value, XmpValue::Array(XmpArray::Seq(_))));
    }

    #[test]
    fn from_xmp_keeps_only_iptc_namespaces() {
        let meta = XmpMeta {
            properties: vec![
                XmpProperty {
                    namespace: ns::PHOTOSHOP.to_owned(),
                    name: "City".to_owned(),
                    value: XmpValue::Simple("Berlin".to_owned()),
                    qualifiers: Vec::new(),
                },
                XmpProperty {
                    namespace: "http://ns.adobe.com/xap/1.0/".to_owned(),
                    name: "CreatorTool".to_owned(),
                    value: XmpValue::Simple("gamut".to_owned()),
                    qualifiers: Vec::new(),
                },
            ],
        };
        let pm = PhotoMetadata::from_xmp(&meta);
        assert_eq!(pm.xmp.properties.len(), 1);
        assert_eq!(pm.city(), Some("Berlin"));
        // Round-trips back out as an XMP graph.
        assert_eq!(pm.to_xmp().properties, pm.xmp.properties);
        assert_eq!(PhotoMetadata::from_xmp(&pm.to_xmp()), pm);
    }

    #[test]
    fn get_set_field_round_trip_by_shape() {
        let f = |ns: &'static str, name: &'static str, shape| XmpField { ns, name, shape };
        let mut pm = PhotoMetadata::new();
        pm.set_field(&f(ns::PHOTOSHOP, "Headline", XmpShape::SimpleText), &["H"]);
        pm.set_field(&f(ns::DC, "rights", XmpShape::LangAlt), &["(c)"]);
        pm.set_field(&f(ns::DC, "subject", XmpShape::Bag), &["a", "b"]);
        assert_eq!(
            pm.get_field(&f(ns::PHOTOSHOP, "Headline", XmpShape::SimpleText)),
            vec!["H"]
        );
        assert_eq!(
            pm.get_field(&f(ns::DC, "rights", XmpShape::LangAlt)),
            vec!["(c)"]
        );
        assert_eq!(
            pm.get_field(&f(ns::DC, "subject", XmpShape::Bag)),
            vec!["a", "b"]
        );
        // Absent field yields an empty list; empty value list leaves a scalar unset.
        assert!(
            pm.get_field(&f(ns::PHOTOSHOP, "City", XmpShape::SimpleText))
                .is_empty()
        );
        pm.set_field(&f(ns::PHOTOSHOP, "City", XmpShape::SimpleText), &[]);
        assert_eq!(pm.city(), None);

        // set_field must store the RDF container kind that matches the shape.
        assert!(matches!(
            pm.xmp.get(ns::DC, "subject").unwrap().value,
            XmpValue::Array(XmpArray::Bag(_))
        ));
        pm.set_field(&f(ns::DC, "creator", XmpShape::Seq), &["x"]);
        assert!(matches!(
            pm.xmp.get(ns::DC, "creator").unwrap().value,
            XmpValue::Array(XmpArray::Seq(_))
        ));
    }
}
