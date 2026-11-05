//! Fixtures shared by the structured-property tests: the field machinery's in
//! [`fields`](super::fields), the structures' in [`extension`](super), and the accessors' in
//! `photo_metadata::structured`. Each value is fully populated with distinct strings, so a field
//! read from the wrong property is visible.

use gamut_xmp::{XML_NAMESPACE, XmpProperty, XmpValue};

use super::{
    CopyrightOwner, CreatorContactInfo, CvTerm, EmbeddedRightsExpression, EntityWithRole,
    ImageCreator, ImageSupplier, LinkedRightsExpression, Location, Person, Product, RegistryEntry,
};

/// A simple text value.
pub(crate) fn text_value(s: &str) -> XmpValue {
    XmpValue::Simple(s.to_owned())
}

/// A contact block whose eight values are all distinct, so a field read from the wrong
/// property is visible.
pub(crate) fn contact() -> CreatorContactInfo {
    CreatorContactInfo {
        address: Some("1 Rue Test".to_owned()),
        city: Some("Lyon".to_owned()),
        country: Some("France".to_owned()),
        postal_code: Some("69000".to_owned()),
        region: Some("Rhône".to_owned()),
        email: Some("a@example.org".to_owned()),
        phone: Some("+33 1 23".to_owned()),
        web_url: Some("https://example.org/".to_owned()),
        other: Vec::new(),
    }
}

/// A [`Location`] whose twelve values are all distinct, so a field read from the wrong property
/// is visible.
pub(crate) fn location() -> Location {
    Location {
        identifiers: vec!["https://example.org/loc/1".to_owned(), "urn:loc".to_owned()],
        name: Some("Place Bellecour".to_owned()),
        sublocation: Some("Presqu'île".to_owned()),
        city: Some("Lyon".to_owned()),
        province_state: Some("Rhône".to_owned()),
        country_name: Some("France".to_owned()),
        country_code: Some("FRA".to_owned()),
        world_region: Some("Europe".to_owned()),
        gps_latitude: Some("45,45.47N".to_owned()),
        gps_longitude: Some("4,49.92E".to_owned()),
        gps_altitude: Some("170/1".to_owned()),
        gps_altitude_ref: Some("0".to_owned()),
        other: Vec::new(),
    }
}

/// A [`CvTerm`] with every field set to a distinct value.
pub(crate) fn cv_term(name: &str) -> CvTerm {
    CvTerm {
        vocabulary_identifier: Some("http://cv.iptc.org/newscodes/scene/".to_owned()),
        identifier: Some("http://cv.iptc.org/newscodes/scene/011900".to_owned()),
        name: Some(name.to_owned()),
        refined_about: Some("http://cv.iptc.org/newscodes/rbac/visual".to_owned()),
        other: Vec::new(),
    }
}

/// A [`Person`] with every field set, including two characteristics.
pub(crate) fn person() -> Person {
    Person {
        identifiers: vec!["urn:person".to_owned()],
        name: Some("Ada Lovelace".to_owned()),
        characteristics: vec![cv_term("adult"), cv_term("smiling")],
        description: Some("Seated, left of frame".to_owned()),
        other: Vec::new(),
    }
}

/// An [`EntityWithRole`] with every field set.
pub(crate) fn contributor() -> EntityWithRole {
    EntityWithRole {
        identifiers: vec!["urn:contributor".to_owned()],
        name: Some("Studio gamut".to_owned()),
        roles: vec![
            "http://cv.iptc.org/newscodes/contrrole/retoucher".to_owned(),
            "http://cv.iptc.org/newscodes/contrrole/editor".to_owned(),
        ],
        other: Vec::new(),
    }
}

/// A [`Product`] with every field set.
pub(crate) fn product() -> Product {
    Product {
        identifiers: vec!["urn:product".to_owned()],
        name: Some("Teapot".to_owned()),
        gtin: Some("4006381333931".to_owned()),
        description: Some("White porcelain".to_owned()),
        other: Vec::new(),
    }
}

/// A [`RegistryEntry`] with every field set.
pub(crate) fn registry_entry() -> RegistryEntry {
    RegistryEntry {
        item_identifier: Some("item-42".to_owned()),
        organisation_identifier: Some("https://registry.example.org/".to_owned()),
        role: Some("http://cv.iptc.org/newscodes/imageregistryrole/canonicalLink".to_owned()),
        other: Vec::new(),
    }
}

/// An [`EmbeddedRightsExpression`] with every field set.
pub(crate) fn embedded_rights() -> EmbeddedRightsExpression {
    EmbeddedRightsExpression {
        expression: Some("<o:Policy/>".to_owned()),
        encoding_type: Some("application/xml".to_owned()),
        language_identifier: Some("http://www.w3.org/ns/odrl/2/".to_owned()),
        other: Vec::new(),
    }
}

/// A [`LinkedRightsExpression`] with every field set.
pub(crate) fn linked_rights() -> LinkedRightsExpression {
    LinkedRightsExpression {
        url: Some("https://example.org/policy.xml".to_owned()),
        encoding_type: Some("application/xml".to_owned()),
        language_identifier: Some("http://www.w3.org/ns/odrl/2/".to_owned()),
        other: Vec::new(),
    }
}

/// A [`CopyrightOwner`] with both fields set.
pub(crate) fn copyright_owner(name: &str) -> CopyrightOwner {
    CopyrightOwner {
        identifier: Some(format!("urn:owner:{name}")),
        name: Some(name.to_owned()),
        other: Vec::new(),
    }
}

/// An [`ImageCreator`] with both fields set.
pub(crate) fn image_creator(name: &str) -> ImageCreator {
    ImageCreator {
        identifier: Some(format!("urn:creator:{name}")),
        name: Some(name.to_owned()),
        other: Vec::new(),
    }
}

/// An [`ImageSupplier`] with both fields set.
pub(crate) fn image_supplier(name: &str) -> ImageSupplier {
    ImageSupplier {
        identifier: Some(format!("urn:supplier:{name}")),
        name: Some(name.to_owned()),
        other: Vec::new(),
    }
}

pub(crate) fn structured(fields: Vec<XmpProperty>) -> XmpValue {
    XmpValue::Structured(fields)
}

pub(crate) fn qualified(mut property: XmpProperty, lang: &str) -> XmpProperty {
    property
        .qualifiers
        .push(XmpProperty::new(XML_NAMESPACE, "lang", text_value(lang)));
    property
}
