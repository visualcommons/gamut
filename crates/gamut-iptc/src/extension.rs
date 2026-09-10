//! Typed models for the structured IPTC properties: the IPTC Extension structures and the
//! Core's `Iptc4xmpCore:CreatorContactInfo`.
//!
//! IPTC's structured properties are ordinary XMP structure values ([`XmpValue::Structured`]),
//! usually inside a `Bag`. This module is a typed *projection* over that graph — the same
//! relationship [`gamut_exif::GpsInfo`](https://docs.rs/gamut-exif) has to its sub-IFD — not a
//! parallel representation: the raw properties stay in [`PhotoMetadata::xmp`] and are what
//! round-trips. Each type converts both ways with `from_xmp`/`to_xmp`, and
//! [`PhotoMetadata`] exposes one accessor pair per property.
//!
//! # Which structures are modelled
//!
//! The four the IPTC Photo Metadata Standard 2025.1 puts to most use in practice:
//!
//! | Type | XMP property | Serialization |
//! | ---- | ------------ | ------------- |
//! | [`CreatorContactInfo`] | `Iptc4xmpCore:CreatorContactInfo` | a single structure |
//! | [`ImageRegion`] | `Iptc4xmpExt:ImageRegion` | `Bag` of structures |
//! | [`ArtworkOrObject`] | `Iptc4xmpExt:ArtworkOrObject` | `Bag` of structures |
//! | [`Licensor`] | `plus:Licensor` | `Bag` of structures |
//!
//! [`RegionBoundary`], [`RegionBoundaryPoint`] and [`Entity`] are the nested structures
//! [`ImageRegion`] is built from. The remaining Extension structures (`Location`, `PersonWDetails`,
//! `CvTerm`, `EmbdEncRightsExpr`, `ProductWGtin`, `RegistryEntry`, `CopyrightOwner`,
//! `ImageCreator`, `ImageSupplier`, `LinkedEncRightsExpr`, `EntityWRole`) have no typed model yet
//! and pass through [`PhotoMetadata::xmp`] untouched, exactly as all of them did before.
//!
//! # These properties never conflict
//!
//! Every property here is **XMP-only**: none carries an `IIMid` in the IPTC technical reference,
//! so none is in [`crate::schema::FIELD_MAP`]. There is therefore nothing for the legacy carrier to
//! disagree with — a structured field can never appear in
//! [`IptcReader::conflicts`](crate::IptcReader::conflicts), and no
//! [`ConflictPolicy`](crate::ConflictPolicy) applies to it. Reconciliation is a property of the
//! IIM↔XMP mapping, not of the field, so extending the modelled surface here does not extend the
//! reconciliation surface.
//!
//! # Fidelity
//!
//! A typed view is a projection, so it is lossy where the graph is richer than the model:
//!
//! - language alternatives are read and written as their `x-default` alternative, as elsewhere in
//!   the crate;
//! - a numeric field whose text does not parse as a number reads as absent (honest read — the raw
//!   value is still in the graph);
//! - an [`ImageRegion`] keeps every field it does not model in [`ImageRegion::other`], because the
//!   standard explicitly allows a region to carry any other metadata property, so a region
//!   survives `from_xmp` → `to_xmp` intact.

use gamut_xmp::{XmpArray, XmpItem, XmpMeta, XmpProperty, XmpValue};

use crate::photo_metadata::PhotoMetadata;
use crate::schema::ns;

// --- Reading helpers over a structure's field list -------------------------------------------

/// The field of `fields` named `ns:name`, if present.
fn field<'a>(fields: &'a [XmpProperty], ns: &str, name: &str) -> Option<&'a XmpProperty> {
    fields.iter().find(|p| p.namespace == ns && p.name == name)
}

/// The simple text of the field named `ns:name`.
fn text(fields: &[XmpProperty], ns: &str, name: &str) -> Option<String> {
    field(fields, ns, name)?.text().map(str::to_owned)
}

/// The `x-default` (first) alternative of the language-alternative field named `ns:name`,
/// tolerating a plain simple value.
fn lang_alt(fields: &[XmpProperty], ns: &str, name: &str) -> Option<String> {
    match &field(fields, ns, name)?.value {
        XmpValue::Array(XmpArray::Alt(items)) => items.iter().find_map(XmpItem::text),
        value => value.text(),
    }
    .map(str::to_owned)
}

/// Every simple item of the array field named `ns:name` (empty if absent or not an array).
fn list(fields: &[XmpProperty], ns: &str, name: &str) -> Vec<String> {
    match &field(fields, ns, name).map(|p| &p.value) {
        Some(XmpValue::Array(array)) => array.texts().map(str::to_owned).collect(),
        _ => Vec::new(),
    }
}

/// The field named `ns:name` parsed as an XMP `Real`; a value that does not parse reads as absent.
fn number(fields: &[XmpProperty], ns: &str, name: &str) -> Option<f64> {
    text(fields, ns, name)?.trim().parse().ok()
}

/// The structure fields of the single structured field named `ns:name`.
fn nested<'a>(fields: &'a [XmpProperty], ns: &str, name: &str) -> Option<&'a [XmpProperty]> {
    match &field(fields, ns, name)?.value {
        XmpValue::Structured(inner) => Some(inner),
        _ => None,
    }
}

/// The structure fields of every item of the array field named `ns:name`, skipping non-structures.
fn nested_array<'a>(fields: &'a [XmpProperty], ns: &str, name: &str) -> Vec<&'a [XmpProperty]> {
    match &field(fields, ns, name).map(|p| &p.value) {
        Some(XmpValue::Array(array)) => array
            .items()
            .iter()
            .filter_map(|item| match &item.value {
                XmpValue::Structured(inner) => Some(inner.as_slice()),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

// --- Writing helpers -------------------------------------------------------------------------

/// Appends `ns:name` as simple text, unless the value is absent.
fn put_text(out: &mut Vec<XmpProperty>, ns: &str, name: &str, value: Option<&String>) {
    if let Some(value) = value {
        out.push(XmpProperty::new(ns, name, XmpValue::Simple(value.clone())));
    }
}

/// Appends `ns:name` as a language alternative holding one `x-default` item, unless absent.
fn put_lang_alt(out: &mut Vec<XmpProperty>, ns: &str, name: &str, value: Option<&String>) {
    if let Some(value) = value {
        let items = vec![XmpItem::lang_text("x-default", value.clone())];
        out.push(XmpProperty::new(
            ns,
            name,
            XmpValue::Array(XmpArray::Alt(items)),
        ));
    }
}

/// Appends `ns:name` as an array of simple text, unless the list is empty.
fn put_list(out: &mut Vec<XmpProperty>, ns: &str, name: &str, ordered: bool, values: &[String]) {
    if values.is_empty() {
        return;
    }
    let items = values.iter().map(XmpItem::simple).collect();
    out.push(XmpProperty::new(
        ns,
        name,
        XmpValue::Array(array(ordered, items)),
    ));
}

/// Appends `ns:name` as an XMP `Real`, unless the value is absent.
fn put_number(out: &mut Vec<XmpProperty>, ns: &str, name: &str, value: Option<f64>) {
    if let Some(value) = value {
        out.push(XmpProperty::new(
            ns,
            name,
            XmpValue::Simple(value.to_string()),
        ));
    }
}

/// Appends `ns:name` as a single structure value, unless absent.
fn put_nested(out: &mut Vec<XmpProperty>, ns: &str, name: &str, value: Option<XmpValue>) {
    if let Some(value) = value {
        out.push(XmpProperty::new(ns, name, value));
    }
}

/// Appends `ns:name` as an array of structure values, unless the list is empty.
fn put_nested_array(
    out: &mut Vec<XmpProperty>,
    ns: &str,
    name: &str,
    ordered: bool,
    values: Vec<XmpValue>,
) {
    if values.is_empty() {
        return;
    }
    let items = values.into_iter().map(XmpItem::new).collect();
    out.push(XmpProperty::new(
        ns,
        name,
        XmpValue::Array(array(ordered, items)),
    ));
}

/// A `Seq` when `ordered`, otherwise a `Bag`.
fn array(ordered: bool, items: Vec<XmpItem>) -> XmpArray {
    if ordered {
        XmpArray::Seq(items)
    } else {
        XmpArray::Bag(items)
    }
}

/// The structure fields of `value`, or `None` if it is not a structure.
fn structure(value: &XmpValue) -> Option<&[XmpProperty]> {
    match value {
        XmpValue::Structured(fields) => Some(fields),
        _ => None,
    }
}

// --- Creator's contact info -------------------------------------------------------------------

/// The creator's contact details (`Iptc4xmpCore:CreatorContactInfo`, IPTC Core 1.5 §8.1).
///
/// Every field is a simple text property in the `Iptc4xmpCore:` namespace. The email, phone and
/// web-URL fields are single properties that the standard allows to hold several comma-separated
/// values, so they are modelled as the text they carry rather than split.
///
/// This property is XMP-only and never participates in IIM↔XMP reconciliation (see the
/// [module docs](self)).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct CreatorContactInfo {
    /// Street address (`Iptc4xmpCore:CiAdrExtadr`).
    pub address: Option<String>,
    /// City (`Iptc4xmpCore:CiAdrCity`).
    pub city: Option<String>,
    /// Country (`Iptc4xmpCore:CiAdrCtry`).
    pub country: Option<String>,
    /// Postal code (`Iptc4xmpCore:CiAdrPcode`).
    pub postal_code: Option<String>,
    /// State or province (`Iptc4xmpCore:CiAdrRegion`).
    pub region: Option<String>,
    /// Work email address(es) (`Iptc4xmpCore:CiEmailWork`).
    pub email: Option<String>,
    /// Work phone number(s) (`Iptc4xmpCore:CiTelWork`).
    pub phone: Option<String>,
    /// Work web URL(s) (`Iptc4xmpCore:CiUrlWork`).
    pub web_url: Option<String>,
}

impl CreatorContactInfo {
    /// Reads the structure from an XMP value, or `None` if the value is not a structure.
    #[must_use]
    pub fn from_xmp(value: &XmpValue) -> Option<Self> {
        structure(value).map(Self::from_fields)
    }

    /// Reads the structure from an already-unwrapped field list.
    fn from_fields(f: &[XmpProperty]) -> Self {
        Self {
            address: text(f, ns::IPTC_CORE, "CiAdrExtadr"),
            city: text(f, ns::IPTC_CORE, "CiAdrCity"),
            country: text(f, ns::IPTC_CORE, "CiAdrCtry"),
            postal_code: text(f, ns::IPTC_CORE, "CiAdrPcode"),
            region: text(f, ns::IPTC_CORE, "CiAdrRegion"),
            email: text(f, ns::IPTC_CORE, "CiEmailWork"),
            phone: text(f, ns::IPTC_CORE, "CiTelWork"),
            web_url: text(f, ns::IPTC_CORE, "CiUrlWork"),
        }
    }

    /// Writes the structure as an XMP value, omitting absent fields.
    #[must_use]
    pub fn to_xmp(&self) -> XmpValue {
        let mut f = Vec::new();
        put_text(&mut f, ns::IPTC_CORE, "CiAdrExtadr", self.address.as_ref());
        put_text(&mut f, ns::IPTC_CORE, "CiAdrCity", self.city.as_ref());
        put_text(&mut f, ns::IPTC_CORE, "CiAdrCtry", self.country.as_ref());
        put_text(
            &mut f,
            ns::IPTC_CORE,
            "CiAdrPcode",
            self.postal_code.as_ref(),
        );
        put_text(&mut f, ns::IPTC_CORE, "CiAdrRegion", self.region.as_ref());
        put_text(&mut f, ns::IPTC_CORE, "CiEmailWork", self.email.as_ref());
        put_text(&mut f, ns::IPTC_CORE, "CiTelWork", self.phone.as_ref());
        put_text(&mut f, ns::IPTC_CORE, "CiUrlWork", self.web_url.as_ref());
        XmpValue::Structured(f)
    }
}

// --- Artwork or object ------------------------------------------------------------------------

/// An artwork or object shown in the image (`Iptc4xmpExt:ArtworkOrObject`, IPTC Extension 1.8
/// §12.1).
///
/// This property is XMP-only and never participates in IIM↔XMP reconciliation (see the
/// [module docs](self)).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ArtworkOrObject {
    /// Title, `x-default` alternative (`Iptc4xmpExt:AOTitle`).
    pub title: Option<String>,
    /// Creator names, in order (`Iptc4xmpExt:AOCreator`).
    pub creator_names: Vec<String>,
    /// Creator identifiers, in the same order as the names (`Iptc4xmpExt:AOCreatorId`).
    pub creator_identifiers: Vec<String>,
    /// Date the artwork was created (`Iptc4xmpExt:AODateCreated`, an XMP date-time).
    pub date_created: Option<String>,
    /// Approximate creation date or range (`Iptc4xmpExt:AOCircaDateCreated`).
    pub circa_date_created: Option<String>,
    /// Copyright notice (`Iptc4xmpExt:AOCopyrightNotice`).
    pub copyright_notice: Option<String>,
    /// Current copyright owner's name (`Iptc4xmpExt:AOCurrentCopyrightOwnerName`).
    pub current_copyright_owner_name: Option<String>,
    /// Current copyright owner's identifier (`Iptc4xmpExt:AOCurrentCopyrightOwnerId`).
    pub current_copyright_owner_identifier: Option<String>,
    /// Current licensor's name (`Iptc4xmpExt:AOCurrentLicensorName`).
    pub current_licensor_name: Option<String>,
    /// Current licensor's identifier (`Iptc4xmpExt:AOCurrentLicensorId`).
    pub current_licensor_identifier: Option<String>,
    /// Content description, `x-default` alternative (`Iptc4xmpExt:AOContentDescription`).
    pub content_description: Option<String>,
    /// Contribution description, `x-default` alternative
    /// (`Iptc4xmpExt:AOContributionDescription`).
    pub contribution_description: Option<String>,
    /// Physical description, `x-default` alternative (`Iptc4xmpExt:AOPhysicalDescription`).
    pub physical_description: Option<String>,
    /// The source holding the artwork (`Iptc4xmpExt:AOSource`).
    pub source: Option<String>,
    /// The source's inventory number (`Iptc4xmpExt:AOSourceInvNo`).
    pub source_inventory_number: Option<String>,
    /// URL of the source's inventory record (`Iptc4xmpExt:AOSourceInvURL`).
    pub source_inventory_url: Option<String>,
    /// Style periods (`Iptc4xmpExt:AOStylePeriod`).
    pub style_periods: Vec<String>,
}

impl ArtworkOrObject {
    /// Reads the structure from an XMP value, or `None` if the value is not a structure.
    #[must_use]
    pub fn from_xmp(value: &XmpValue) -> Option<Self> {
        structure(value).map(Self::from_fields)
    }

    /// Reads the structure from an already-unwrapped field list.
    fn from_fields(f: &[XmpProperty]) -> Self {
        Self {
            title: lang_alt(f, ns::IPTC_EXT, "AOTitle"),
            creator_names: list(f, ns::IPTC_EXT, "AOCreator"),
            creator_identifiers: list(f, ns::IPTC_EXT, "AOCreatorId"),
            date_created: text(f, ns::IPTC_EXT, "AODateCreated"),
            circa_date_created: text(f, ns::IPTC_EXT, "AOCircaDateCreated"),
            copyright_notice: text(f, ns::IPTC_EXT, "AOCopyrightNotice"),
            current_copyright_owner_name: text(f, ns::IPTC_EXT, "AOCurrentCopyrightOwnerName"),
            current_copyright_owner_identifier: text(f, ns::IPTC_EXT, "AOCurrentCopyrightOwnerId"),
            current_licensor_name: text(f, ns::IPTC_EXT, "AOCurrentLicensorName"),
            current_licensor_identifier: text(f, ns::IPTC_EXT, "AOCurrentLicensorId"),
            content_description: lang_alt(f, ns::IPTC_EXT, "AOContentDescription"),
            contribution_description: lang_alt(f, ns::IPTC_EXT, "AOContributionDescription"),
            physical_description: lang_alt(f, ns::IPTC_EXT, "AOPhysicalDescription"),
            source: text(f, ns::IPTC_EXT, "AOSource"),
            source_inventory_number: text(f, ns::IPTC_EXT, "AOSourceInvNo"),
            source_inventory_url: text(f, ns::IPTC_EXT, "AOSourceInvURL"),
            style_periods: list(f, ns::IPTC_EXT, "AOStylePeriod"),
        }
    }

    /// Writes the structure as an XMP value, omitting absent fields.
    ///
    /// The container kinds follow the standard: `AOCreator` and `AOCreatorId` are ordered `Seq`s
    /// (identifiers are given in the same sequence as the names), `AOStylePeriod` an unordered
    /// `Bag`.
    #[must_use]
    pub fn to_xmp(&self) -> XmpValue {
        let mut f = Vec::new();
        put_lang_alt(&mut f, ns::IPTC_EXT, "AOTitle", self.title.as_ref());
        put_list(&mut f, ns::IPTC_EXT, "AOCreator", true, &self.creator_names);
        put_list(
            &mut f,
            ns::IPTC_EXT,
            "AOCreatorId",
            true,
            &self.creator_identifiers,
        );
        put_text(
            &mut f,
            ns::IPTC_EXT,
            "AODateCreated",
            self.date_created.as_ref(),
        );
        put_text(
            &mut f,
            ns::IPTC_EXT,
            "AOCircaDateCreated",
            self.circa_date_created.as_ref(),
        );
        put_text(
            &mut f,
            ns::IPTC_EXT,
            "AOCopyrightNotice",
            self.copyright_notice.as_ref(),
        );
        put_text(
            &mut f,
            ns::IPTC_EXT,
            "AOCurrentCopyrightOwnerName",
            self.current_copyright_owner_name.as_ref(),
        );
        put_text(
            &mut f,
            ns::IPTC_EXT,
            "AOCurrentCopyrightOwnerId",
            self.current_copyright_owner_identifier.as_ref(),
        );
        put_text(
            &mut f,
            ns::IPTC_EXT,
            "AOCurrentLicensorName",
            self.current_licensor_name.as_ref(),
        );
        put_text(
            &mut f,
            ns::IPTC_EXT,
            "AOCurrentLicensorId",
            self.current_licensor_identifier.as_ref(),
        );
        put_lang_alt(
            &mut f,
            ns::IPTC_EXT,
            "AOContentDescription",
            self.content_description.as_ref(),
        );
        put_lang_alt(
            &mut f,
            ns::IPTC_EXT,
            "AOContributionDescription",
            self.contribution_description.as_ref(),
        );
        put_lang_alt(
            &mut f,
            ns::IPTC_EXT,
            "AOPhysicalDescription",
            self.physical_description.as_ref(),
        );
        put_text(&mut f, ns::IPTC_EXT, "AOSource", self.source.as_ref());
        put_text(
            &mut f,
            ns::IPTC_EXT,
            "AOSourceInvNo",
            self.source_inventory_number.as_ref(),
        );
        put_text(
            &mut f,
            ns::IPTC_EXT,
            "AOSourceInvURL",
            self.source_inventory_url.as_ref(),
        );
        put_list(
            &mut f,
            ns::IPTC_EXT,
            "AOStylePeriod",
            false,
            &self.style_periods,
        );
        XmpValue::Structured(f)
    }
}

// --- Licensor ---------------------------------------------------------------------------------

/// A licensor of the image (`plus:Licensor`, a PLUS 1.2 structure embedded in the IPTC Extension
/// schema, IPTC Extension 1.8 §11.24).
///
/// The standard caps the property at three licensors; gamut does not enforce that on read — an
/// over-long array reads as the licensors it holds.
///
/// This property is XMP-only and never participates in IIM↔XMP reconciliation (see the
/// [module docs](self)).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Licensor {
    /// Licensor identifier (`plus:LicensorID`).
    pub identifier: Option<String>,
    /// Licensor name (`plus:LicensorName`).
    pub name: Option<String>,
    /// Street address (`plus:LicensorStreetAddress`).
    pub address: Option<String>,
    /// Extended address detail (`plus:LicensorExtendedAddress`).
    pub address_detail: Option<String>,
    /// City (`plus:LicensorCity`).
    pub city: Option<String>,
    /// State or province (`plus:LicensorRegion`).
    pub region: Option<String>,
    /// Postal code (`plus:LicensorPostalCode`).
    pub postal_code: Option<String>,
    /// Country (`plus:LicensorCountry`).
    pub country: Option<String>,
    /// Kind of the first telephone number (`plus:LicensorTelephoneType1`).
    pub telephone_type1: Option<String>,
    /// First telephone number (`plus:LicensorTelephone1`).
    pub telephone1: Option<String>,
    /// Kind of the second telephone number (`plus:LicensorTelephoneType2`).
    pub telephone_type2: Option<String>,
    /// Second telephone number (`plus:LicensorTelephone2`).
    pub telephone2: Option<String>,
    /// Email address (`plus:LicensorEmail`).
    pub email: Option<String>,
    /// Web URL (`plus:LicensorURL`).
    pub web_url: Option<String>,
}

impl Licensor {
    /// Reads the structure from an XMP value, or `None` if the value is not a structure.
    #[must_use]
    pub fn from_xmp(value: &XmpValue) -> Option<Self> {
        structure(value).map(Self::from_fields)
    }

    /// Reads the structure from an already-unwrapped field list.
    fn from_fields(f: &[XmpProperty]) -> Self {
        Self {
            identifier: text(f, ns::PLUS, "LicensorID"),
            name: text(f, ns::PLUS, "LicensorName"),
            address: text(f, ns::PLUS, "LicensorStreetAddress"),
            address_detail: text(f, ns::PLUS, "LicensorExtendedAddress"),
            city: text(f, ns::PLUS, "LicensorCity"),
            region: text(f, ns::PLUS, "LicensorRegion"),
            postal_code: text(f, ns::PLUS, "LicensorPostalCode"),
            country: text(f, ns::PLUS, "LicensorCountry"),
            telephone_type1: text(f, ns::PLUS, "LicensorTelephoneType1"),
            telephone1: text(f, ns::PLUS, "LicensorTelephone1"),
            telephone_type2: text(f, ns::PLUS, "LicensorTelephoneType2"),
            telephone2: text(f, ns::PLUS, "LicensorTelephone2"),
            email: text(f, ns::PLUS, "LicensorEmail"),
            web_url: text(f, ns::PLUS, "LicensorURL"),
        }
    }

    /// Writes the structure as an XMP value, omitting absent fields.
    #[must_use]
    pub fn to_xmp(&self) -> XmpValue {
        let mut f = Vec::new();
        put_text(&mut f, ns::PLUS, "LicensorID", self.identifier.as_ref());
        put_text(&mut f, ns::PLUS, "LicensorName", self.name.as_ref());
        put_text(
            &mut f,
            ns::PLUS,
            "LicensorStreetAddress",
            self.address.as_ref(),
        );
        put_text(
            &mut f,
            ns::PLUS,
            "LicensorExtendedAddress",
            self.address_detail.as_ref(),
        );
        put_text(&mut f, ns::PLUS, "LicensorCity", self.city.as_ref());
        put_text(&mut f, ns::PLUS, "LicensorRegion", self.region.as_ref());
        put_text(
            &mut f,
            ns::PLUS,
            "LicensorPostalCode",
            self.postal_code.as_ref(),
        );
        put_text(&mut f, ns::PLUS, "LicensorCountry", self.country.as_ref());
        put_text(
            &mut f,
            ns::PLUS,
            "LicensorTelephoneType1",
            self.telephone_type1.as_ref(),
        );
        put_text(
            &mut f,
            ns::PLUS,
            "LicensorTelephone1",
            self.telephone1.as_ref(),
        );
        put_text(
            &mut f,
            ns::PLUS,
            "LicensorTelephoneType2",
            self.telephone_type2.as_ref(),
        );
        put_text(
            &mut f,
            ns::PLUS,
            "LicensorTelephone2",
            self.telephone2.as_ref(),
        );
        put_text(&mut f, ns::PLUS, "LicensorEmail", self.email.as_ref());
        put_text(&mut f, ns::PLUS, "LicensorURL", self.web_url.as_ref());
        XmpValue::Structured(f)
    }
}

// --- Image region -----------------------------------------------------------------------------

/// An entity or concept referenced by a controlled-vocabulary term (the IPTC Extension
/// `EntityConcept` structure, IPTC Extension 1.8 §12.4).
///
/// Used for an [`ImageRegion`]'s content types and roles.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Entity {
    /// Globally unique identifiers of the entity or concept (`xmp:Identifier`).
    pub identifiers: Vec<String>,
    /// Full name, `x-default` alternative (`Iptc4xmpExt:Name`).
    pub name: Option<String>,
}

impl Entity {
    /// Reads the structure from an XMP value, or `None` if the value is not a structure.
    #[must_use]
    pub fn from_xmp(value: &XmpValue) -> Option<Self> {
        structure(value).map(Self::from_fields)
    }

    /// Reads the structure from an already-unwrapped field list.
    fn from_fields(f: &[XmpProperty]) -> Self {
        Self {
            identifiers: list(f, ns::XMP, "Identifier"),
            name: lang_alt(f, ns::IPTC_EXT, "Name"),
        }
    }

    /// Writes the structure as an XMP value, omitting absent fields.
    #[must_use]
    pub fn to_xmp(&self) -> XmpValue {
        let mut f = Vec::new();
        put_list(&mut f, ns::XMP, "Identifier", false, &self.identifiers);
        put_lang_alt(&mut f, ns::IPTC_EXT, "Name", self.name.as_ref());
        XmpValue::Structured(f)
    }
}

/// One vertex of a polygon region boundary (`Iptc4xmpExt:RegionBoundaryPoint`, IPTC Extension 1.8
/// §12.8).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
#[non_exhaustive]
pub struct RegionBoundaryPoint {
    /// X-axis coordinate (`Iptc4xmpExt:rbX`).
    pub x: Option<f64>,
    /// Y-axis coordinate (`Iptc4xmpExt:rbY`).
    pub y: Option<f64>,
}

impl RegionBoundaryPoint {
    /// Reads the structure from an XMP value, or `None` if the value is not a structure.
    #[must_use]
    pub fn from_xmp(value: &XmpValue) -> Option<Self> {
        structure(value).map(Self::from_fields)
    }

    /// Reads the structure from an already-unwrapped field list.
    fn from_fields(f: &[XmpProperty]) -> Self {
        Self {
            x: number(f, ns::IPTC_EXT, "rbX"),
            y: number(f, ns::IPTC_EXT, "rbY"),
        }
    }

    /// Writes the structure as an XMP value, omitting absent fields.
    #[must_use]
    pub fn to_xmp(&self) -> XmpValue {
        let mut f = Vec::new();
        put_number(&mut f, ns::IPTC_EXT, "rbX", self.x);
        put_number(&mut f, ns::IPTC_EXT, "rbY", self.y);
        XmpValue::Structured(f)
    }
}

/// The outline of an [`ImageRegion`] (`Iptc4xmpExt:RegionBoundary`, IPTC Extension 1.8 §12.7).
///
/// Which coordinate fields apply is decided by [`shape`](Self::shape): `rectangle` uses
/// `x`/`y`/`width`/`height`, `circle` uses `x`/`y`/`radius`, and `polygon` uses `vertices`
/// (a single vertex expresses a point, two a line). gamut stores what the graph carries and does
/// not reject a boundary whose fields do not match its shape.
#[derive(Debug, Clone, Default, PartialEq)]
#[non_exhaustive]
pub struct RegionBoundary {
    /// Boundary shape — `rectangle`, `circle` or `polygon` (`Iptc4xmpExt:rbShape`).
    pub shape: Option<String>,
    /// Measuring unit — `pixel` or `relative` (`Iptc4xmpExt:rbUnit`).
    pub unit: Option<String>,
    /// X-axis coordinate of a rectangle's corner or a circle's centre (`Iptc4xmpExt:rbX`).
    pub x: Option<f64>,
    /// Y-axis coordinate of a rectangle's corner or a circle's centre (`Iptc4xmpExt:rbY`).
    pub y: Option<f64>,
    /// Rectangle width (`Iptc4xmpExt:rbW`).
    pub width: Option<f64>,
    /// Rectangle height (`Iptc4xmpExt:rbH`).
    pub height: Option<f64>,
    /// Circle radius (`Iptc4xmpExt:rbRx`).
    pub radius: Option<f64>,
    /// Polygon vertices, in order (`Iptc4xmpExt:rbVertices`).
    pub vertices: Vec<RegionBoundaryPoint>,
}

impl RegionBoundary {
    /// Reads the structure from an XMP value, or `None` if the value is not a structure.
    #[must_use]
    pub fn from_xmp(value: &XmpValue) -> Option<Self> {
        structure(value).map(Self::from_fields)
    }

    /// Reads the structure from an already-unwrapped field list.
    fn from_fields(f: &[XmpProperty]) -> Self {
        Self {
            shape: text(f, ns::IPTC_EXT, "rbShape"),
            unit: text(f, ns::IPTC_EXT, "rbUnit"),
            x: number(f, ns::IPTC_EXT, "rbX"),
            y: number(f, ns::IPTC_EXT, "rbY"),
            width: number(f, ns::IPTC_EXT, "rbW"),
            height: number(f, ns::IPTC_EXT, "rbH"),
            radius: number(f, ns::IPTC_EXT, "rbRx"),
            vertices: nested_array(f, ns::IPTC_EXT, "rbVertices")
                .into_iter()
                .map(RegionBoundaryPoint::from_fields)
                .collect(),
        }
    }

    /// Writes the structure as an XMP value, omitting absent fields.
    ///
    /// The vertices are written as an ordered `Seq`, because a polygon's edges follow the vertex
    /// sequence.
    #[must_use]
    pub fn to_xmp(&self) -> XmpValue {
        let mut f = Vec::new();
        put_text(&mut f, ns::IPTC_EXT, "rbShape", self.shape.as_ref());
        put_text(&mut f, ns::IPTC_EXT, "rbUnit", self.unit.as_ref());
        put_number(&mut f, ns::IPTC_EXT, "rbX", self.x);
        put_number(&mut f, ns::IPTC_EXT, "rbY", self.y);
        put_number(&mut f, ns::IPTC_EXT, "rbW", self.width);
        put_number(&mut f, ns::IPTC_EXT, "rbH", self.height);
        put_number(&mut f, ns::IPTC_EXT, "rbRx", self.radius);
        put_nested_array(
            &mut f,
            ns::IPTC_EXT,
            "rbVertices",
            true,
            self.vertices
                .iter()
                .map(RegionBoundaryPoint::to_xmp)
                .collect(),
        );
        XmpValue::Structured(f)
    }
}

/// A region of the image (`Iptc4xmpExt:ImageRegion`, IPTC Extension 1.8 §11.20) — the basis of
/// face and subject tagging.
///
/// The standard allows a region to carry *any* other metadata property alongside the five it
/// defines; those are kept verbatim in [`other`](Self::other), so a region survives
/// [`from_xmp`](Self::from_xmp) → [`to_xmp`](Self::to_xmp) with nothing dropped.
///
/// This property is XMP-only and never participates in IIM↔XMP reconciliation (see the
/// [module docs](self)).
#[derive(Debug, Clone, Default, PartialEq)]
#[non_exhaustive]
pub struct ImageRegion {
    /// The region's outline (`Iptc4xmpExt:RegionBoundary`).
    pub boundary: Option<RegionBoundary>,
    /// Region identifier, unique within the image (`Iptc4xmpExt:rId`).
    pub identifier: Option<String>,
    /// Region name, `x-default` alternative (`Iptc4xmpExt:Name`).
    pub name: Option<String>,
    /// What the region depicts (`Iptc4xmpExt:rCtype`).
    pub content_types: Vec<Entity>,
    /// The role the region plays in the image (`Iptc4xmpExt:rRole`).
    pub roles: Vec<Entity>,
    /// Every other property the region carries, verbatim (the standard's
    /// "other metadata property").
    pub other: Vec<XmpProperty>,
}

impl ImageRegion {
    /// The five field names [`ImageRegion`] models; everything else lands in
    /// [`other`](Self::other).
    const MODELLED: [(&'static str, &'static str); 5] = [
        (ns::IPTC_EXT, "RegionBoundary"),
        (ns::IPTC_EXT, "rId"),
        (ns::IPTC_EXT, "Name"),
        (ns::IPTC_EXT, "rCtype"),
        (ns::IPTC_EXT, "rRole"),
    ];

    /// Reads the structure from an XMP value, or `None` if the value is not a structure.
    #[must_use]
    pub fn from_xmp(value: &XmpValue) -> Option<Self> {
        structure(value).map(Self::from_fields)
    }

    /// Reads the structure from an already-unwrapped field list.
    fn from_fields(f: &[XmpProperty]) -> Self {
        Self {
            boundary: nested(f, ns::IPTC_EXT, "RegionBoundary").map(RegionBoundary::from_fields),
            identifier: text(f, ns::IPTC_EXT, "rId"),
            name: lang_alt(f, ns::IPTC_EXT, "Name"),
            content_types: entities(f, "rCtype"),
            roles: entities(f, "rRole"),
            other: f
                .iter()
                .filter(|p| {
                    !Self::MODELLED
                        .iter()
                        .any(|&(ns, name)| p.namespace == ns && p.name == name)
                })
                .cloned()
                .collect(),
        }
    }

    /// Writes the structure as an XMP value, omitting absent fields and appending
    /// [`other`](Self::other) verbatim.
    #[must_use]
    pub fn to_xmp(&self) -> XmpValue {
        let mut f = Vec::new();
        put_nested(
            &mut f,
            ns::IPTC_EXT,
            "RegionBoundary",
            self.boundary.as_ref().map(RegionBoundary::to_xmp),
        );
        put_text(&mut f, ns::IPTC_EXT, "rId", self.identifier.as_ref());
        put_lang_alt(&mut f, ns::IPTC_EXT, "Name", self.name.as_ref());
        put_nested_array(
            &mut f,
            ns::IPTC_EXT,
            "rCtype",
            false,
            self.content_types.iter().map(Entity::to_xmp).collect(),
        );
        put_nested_array(
            &mut f,
            ns::IPTC_EXT,
            "rRole",
            false,
            self.roles.iter().map(Entity::to_xmp).collect(),
        );
        f.extend(self.other.iter().cloned());
        XmpValue::Structured(f)
    }
}

/// Every [`Entity`] of the `Iptc4xmpExt:<name>` array field.
fn entities(fields: &[XmpProperty], name: &str) -> Vec<Entity> {
    nested_array(fields, ns::IPTC_EXT, name)
        .into_iter()
        .map(Entity::from_fields)
        .collect()
}

// --- The accessors on the unified view ---------------------------------------------------------

/// Reads every structure of the `Bag`/`Seq` property `ns:name` through `parse`.
fn read_array<T>(xmp: &XmpMeta, ns: &str, name: &str, parse: fn(&XmpValue) -> Option<T>) -> Vec<T> {
    match xmp.get_array(ns, name) {
        Some(array) => array
            .items()
            .iter()
            .filter_map(|i| parse(&i.value))
            .collect(),
        None => Vec::new(),
    }
}

/// Replaces the `Bag` property `ns:name` with `values`, removing it when `values` is empty.
fn write_bag(xmp: &mut XmpMeta, ns: &str, name: &str, values: Vec<XmpValue>) {
    if values.is_empty() {
        xmp.remove(ns, name);
        return;
    }
    let items = values.into_iter().map(XmpItem::new).collect();
    xmp.set(XmpProperty::new(
        ns,
        name,
        XmpValue::Array(XmpArray::Bag(items)),
    ));
}

impl PhotoMetadata {
    /// The creator's contact details (`Iptc4xmpCore:CreatorContactInfo`).
    #[must_use]
    pub fn creator_contact_info(&self) -> Option<CreatorContactInfo> {
        CreatorContactInfo::from_xmp(&self.xmp.get(ns::IPTC_CORE, "CreatorContactInfo")?.value)
    }

    /// Sets the creator's contact details (`Iptc4xmpCore:CreatorContactInfo`).
    pub fn set_creator_contact_info(&mut self, info: &CreatorContactInfo) {
        self.xmp.set(XmpProperty::new(
            ns::IPTC_CORE,
            "CreatorContactInfo",
            info.to_xmp(),
        ));
    }

    /// The image regions (`Iptc4xmpExt:ImageRegion`), in the order the graph holds them.
    #[must_use]
    pub fn image_regions(&self) -> Vec<ImageRegion> {
        read_array(
            &self.xmp,
            ns::IPTC_EXT,
            "ImageRegion",
            ImageRegion::from_xmp,
        )
    }

    /// Sets the image regions (`Iptc4xmpExt:ImageRegion`, an unordered bag); an empty slice
    /// removes the property.
    pub fn set_image_regions(&mut self, regions: &[ImageRegion]) {
        let values = regions.iter().map(ImageRegion::to_xmp).collect();
        write_bag(&mut self.xmp, ns::IPTC_EXT, "ImageRegion", values);
    }

    /// The artworks or objects shown in the image (`Iptc4xmpExt:ArtworkOrObject`).
    #[must_use]
    pub fn artwork_or_objects(&self) -> Vec<ArtworkOrObject> {
        read_array(
            &self.xmp,
            ns::IPTC_EXT,
            "ArtworkOrObject",
            ArtworkOrObject::from_xmp,
        )
    }

    /// Sets the artworks or objects shown in the image (`Iptc4xmpExt:ArtworkOrObject`, an
    /// unordered bag); an empty slice removes the property.
    pub fn set_artwork_or_objects(&mut self, artworks: &[ArtworkOrObject]) {
        let values = artworks.iter().map(ArtworkOrObject::to_xmp).collect();
        write_bag(&mut self.xmp, ns::IPTC_EXT, "ArtworkOrObject", values);
    }

    /// The licensors of the image (`plus:Licensor`).
    #[must_use]
    pub fn licensors(&self) -> Vec<Licensor> {
        read_array(&self.xmp, ns::PLUS, "Licensor", Licensor::from_xmp)
    }

    /// Sets the licensors of the image (`plus:Licensor`, an unordered bag); an empty slice removes
    /// the property.
    pub fn set_licensors(&mut self, licensors: &[Licensor]) {
        let values = licensors.iter().map(Licensor::to_xmp).collect();
        write_bag(&mut self.xmp, ns::PLUS, "Licensor", values);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_value(s: &str) -> XmpValue {
        XmpValue::Simple(s.to_owned())
    }

    /// A contact block whose eight values are all distinct, so a field read from the wrong
    /// property is visible.
    fn contact() -> CreatorContactInfo {
        CreatorContactInfo {
            address: Some("1 Rue Test".to_owned()),
            city: Some("Lyon".to_owned()),
            country: Some("France".to_owned()),
            postal_code: Some("69000".to_owned()),
            region: Some("Rhône".to_owned()),
            email: Some("a@example.org".to_owned()),
            phone: Some("+33 1 23".to_owned()),
            web_url: Some("https://example.org/".to_owned()),
        }
    }

    #[test]
    fn creator_contact_info_round_trips_every_field() {
        let info = contact();
        let value = info.to_xmp();
        assert_eq!(CreatorContactInfo::from_xmp(&value), Some(info));
        // Each field is its own `Iptc4xmpCore:Ci*` property; absent fields are not written.
        let fields = structure(&value).unwrap();
        assert_eq!(fields.len(), 8);
        assert!(fields.iter().all(|p| p.namespace == ns::IPTC_CORE));
        assert_eq!(
            text(fields, ns::IPTC_CORE, "CiAdrPcode"),
            Some("69000".to_owned())
        );
        let sparse = CreatorContactInfo {
            city: Some("Lyon".to_owned()),
            ..CreatorContactInfo::default()
        };
        assert_eq!(structure(&sparse.to_xmp()).unwrap().len(), 1);
    }

    #[test]
    fn from_xmp_rejects_a_non_structure_value() {
        let text = text_value("not a structure");
        assert_eq!(CreatorContactInfo::from_xmp(&text), None);
        assert_eq!(ArtworkOrObject::from_xmp(&text), None);
        assert_eq!(Licensor::from_xmp(&text), None);
        assert_eq!(ImageRegion::from_xmp(&text), None);
        assert_eq!(RegionBoundary::from_xmp(&text), None);
        assert_eq!(RegionBoundaryPoint::from_xmp(&text), None);
        assert_eq!(Entity::from_xmp(&text), None);
    }

    #[test]
    fn artwork_or_object_round_trips_every_field() {
        let art = ArtworkOrObject {
            title: Some("Sunflowers".to_owned()),
            creator_names: vec!["Van Gogh".to_owned(), "Studio".to_owned()],
            creator_identifiers: vec!["urn:a".to_owned(), "urn:b".to_owned()],
            date_created: Some("1888-08".to_owned()),
            circa_date_created: Some("circa 1888".to_owned()),
            copyright_notice: Some("Public domain".to_owned()),
            current_copyright_owner_name: Some("Owner".to_owned()),
            current_copyright_owner_identifier: Some("urn:owner".to_owned()),
            current_licensor_name: Some("Licensor".to_owned()),
            current_licensor_identifier: Some("urn:licensor".to_owned()),
            content_description: Some("Vase with flowers".to_owned()),
            contribution_description: Some("Restored 1980".to_owned()),
            physical_description: Some("Oil on canvas".to_owned()),
            source: Some("National Gallery".to_owned()),
            source_inventory_number: Some("NG3863".to_owned()),
            source_inventory_url: Some("https://example.org/NG3863".to_owned()),
            style_periods: vec!["Post-Impressionism".to_owned()],
        };
        let value = art.to_xmp();
        assert_eq!(ArtworkOrObject::from_xmp(&value), Some(art));

        let fields = structure(&value).unwrap();
        assert_eq!(fields.len(), 17);
        // Creator names and identifiers are ordered (they correspond pairwise); style periods are
        // an unordered bag.
        assert!(matches!(
            field(fields, ns::IPTC_EXT, "AOCreator").unwrap().value,
            XmpValue::Array(XmpArray::Seq(_))
        ));
        assert!(matches!(
            field(fields, ns::IPTC_EXT, "AOCreatorId").unwrap().value,
            XmpValue::Array(XmpArray::Seq(_))
        ));
        assert!(matches!(
            field(fields, ns::IPTC_EXT, "AOStylePeriod").unwrap().value,
            XmpValue::Array(XmpArray::Bag(_))
        ));
        // The four descriptive fields are language alternatives, not plain text.
        for name in [
            "AOTitle",
            "AOContentDescription",
            "AOContributionDescription",
            "AOPhysicalDescription",
        ] {
            assert!(
                matches!(
                    field(fields, ns::IPTC_EXT, name).unwrap().value,
                    XmpValue::Array(XmpArray::Alt(_))
                ),
                "{name} must be a language alternative"
            );
        }
    }

    #[test]
    fn licensor_round_trips_every_field() {
        let licensor = Licensor {
            identifier: Some("urn:licensor".to_owned()),
            name: Some("Agence gamut".to_owned()),
            address: Some("2 Rue Test".to_owned()),
            address_detail: Some("Floor 3".to_owned()),
            city: Some("Paris".to_owned()),
            region: Some("Île-de-France".to_owned()),
            postal_code: Some("75001".to_owned()),
            country: Some("France".to_owned()),
            telephone_type1: Some("work".to_owned()),
            telephone1: Some("+33 1 11".to_owned()),
            telephone_type2: Some("cell".to_owned()),
            telephone2: Some("+33 6 22".to_owned()),
            email: Some("licence@example.org".to_owned()),
            web_url: Some("https://example.org/licence".to_owned()),
        };
        let value = licensor.to_xmp();
        assert_eq!(Licensor::from_xmp(&value), Some(licensor));
        let fields = structure(&value).unwrap();
        assert_eq!(fields.len(), 14);
        // Licensor is a PLUS structure, so every field lives in the PLUS namespace.
        assert!(fields.iter().all(|p| p.namespace == ns::PLUS));
    }

    #[test]
    fn image_region_round_trips_and_keeps_unmodeled_properties() {
        let region = ImageRegion {
            boundary: Some(RegionBoundary {
                shape: Some("rectangle".to_owned()),
                unit: Some("relative".to_owned()),
                x: Some(0.25),
                y: Some(0.5),
                width: Some(0.125),
                height: Some(0.0625),
                radius: None,
                vertices: Vec::new(),
            }),
            identifier: Some("region-1".to_owned()),
            name: Some("Face".to_owned()),
            content_types: vec![Entity {
                identifiers: vec!["https://cv.iptc.org/newscodes/imageregiontype/human".to_owned()],
                name: Some("Human".to_owned()),
            }],
            roles: vec![Entity {
                identifiers: vec![
                    "https://cv.iptc.org/newscodes/imageregionrole/subjectArea".to_owned(),
                ],
                name: Some("Subject area".to_owned()),
            }],
            // The standard lets a region carry any other property; this one must survive.
            other: vec![XmpProperty::new(
                ns::IPTC_EXT,
                "PersonInImage",
                XmpValue::Array(XmpArray::Bag(vec![XmpItem::simple("Ada")])),
            )],
        };
        let value = region.to_xmp();
        assert_eq!(ImageRegion::from_xmp(&value), Some(region));

        let fields = structure(&value).unwrap();
        assert_eq!(fields.len(), 6);
        assert!(field(fields, ns::IPTC_EXT, "PersonInImage").is_some());
        // The two Entity arrays are unordered bags; the boundary is a plain structure.
        for name in ["rCtype", "rRole"] {
            assert!(matches!(
                field(fields, ns::IPTC_EXT, name).unwrap().value,
                XmpValue::Array(XmpArray::Bag(_))
            ));
        }
        assert!(matches!(
            field(fields, ns::IPTC_EXT, "RegionBoundary").unwrap().value,
            XmpValue::Structured(_)
        ));
        // The entity identifier is `xmp:Identifier`, not an IPTC-namespaced property.
        let entity = nested_array(fields, ns::IPTC_EXT, "rCtype")[0];
        assert!(field(entity, ns::XMP, "Identifier").is_some());
    }

    #[test]
    fn entity_round_trips_its_identifiers_and_name() {
        // An Entity reached through an ImageRegion is read field-list-first; this is the public
        // value-level conversion, which nothing else exercises with a structure value.
        let entity = Entity {
            identifiers: vec!["urn:a".to_owned(), "urn:b".to_owned()],
            name: Some("Human".to_owned()),
        };
        assert_eq!(Entity::from_xmp(&entity.to_xmp()), Some(entity));
    }

    #[test]
    fn polygon_boundary_keeps_its_vertices_in_order() {
        let boundary = RegionBoundary {
            shape: Some("polygon".to_owned()),
            unit: Some("pixel".to_owned()),
            vertices: vec![
                RegionBoundaryPoint {
                    x: Some(0.0),
                    y: Some(10.0),
                },
                RegionBoundaryPoint {
                    x: Some(20.0),
                    y: Some(30.0),
                },
            ],
            ..RegionBoundary::default()
        };
        let value = boundary.to_xmp();
        assert_eq!(RegionBoundary::from_xmp(&value), Some(boundary));
        // A polygon's edges follow the vertex sequence, so the array must be a Seq.
        let fields = structure(&value).unwrap();
        assert!(matches!(
            field(fields, ns::IPTC_EXT, "rbVertices").unwrap().value,
            XmpValue::Array(XmpArray::Seq(_))
        ));
    }

    #[test]
    fn a_coordinate_that_is_not_a_number_reads_as_absent() {
        // Honest read: the raw value stays in the graph, but the typed view does not invent one.
        let value = XmpValue::Structured(vec![
            XmpProperty::new(ns::IPTC_EXT, "rbX", text_value("halfway")),
            XmpProperty::new(ns::IPTC_EXT, "rbY", text_value(" 4.5 ")),
        ]);
        let point = RegionBoundaryPoint::from_xmp(&value).unwrap();
        assert_eq!(point.x, None);
        assert_eq!(point.y, Some(4.5));
    }

    #[test]
    fn lang_alt_fields_read_a_plain_simple_value_too() {
        // Non-conformant but seen in the wild: a Lang Alt field written as plain text.
        let value = XmpValue::Structured(vec![XmpProperty::new(
            ns::IPTC_EXT,
            "AOTitle",
            text_value("Sunflowers"),
        )]);
        let art = ArtworkOrObject::from_xmp(&value).unwrap();
        assert_eq!(art.title.as_deref(), Some("Sunflowers"));
    }

    #[test]
    fn accessors_round_trip_through_the_unified_view() {
        let mut pm = PhotoMetadata::new();
        assert_eq!(pm.creator_contact_info(), None);
        assert!(pm.image_regions().is_empty());
        assert!(pm.artwork_or_objects().is_empty());
        assert!(pm.licensors().is_empty());

        let info = contact();
        let region = ImageRegion {
            identifier: Some("r1".to_owned()),
            ..ImageRegion::default()
        };
        let art = ArtworkOrObject {
            title: Some("Sunflowers".to_owned()),
            ..ArtworkOrObject::default()
        };
        let licensor = Licensor {
            name: Some("Agence gamut".to_owned()),
            ..Licensor::default()
        };
        pm.set_creator_contact_info(&info);
        pm.set_image_regions(std::slice::from_ref(&region));
        pm.set_artwork_or_objects(std::slice::from_ref(&art));
        pm.set_licensors(std::slice::from_ref(&licensor));

        assert_eq!(pm.creator_contact_info(), Some(info));
        assert_eq!(pm.image_regions(), vec![region]);
        assert_eq!(pm.artwork_or_objects(), vec![art]);
        assert_eq!(pm.licensors(), vec![licensor]);
        assert_eq!(pm.xmp.properties.len(), 4);

        // Setting an empty slice removes the property rather than leaving an empty array.
        pm.set_image_regions(&[]);
        pm.set_artwork_or_objects(&[]);
        pm.set_licensors(&[]);
        assert!(pm.image_regions().is_empty());
        assert_eq!(pm.xmp.properties.len(), 1);
    }

    #[test]
    fn plus_licensors_survive_extraction_from_a_full_xmp_graph() {
        // plus: is in IPTC_NAMESPACES, so PhotoMetadata::from_xmp must keep plus:Licensor while
        // still dropping a non-IPTC property.
        let mut pm = PhotoMetadata::new();
        pm.set_licensors(&[Licensor {
            name: Some("Agence gamut".to_owned()),
            ..Licensor::default()
        }]);
        let mut graph = pm.to_xmp();
        graph.set(XmpProperty::new(
            ns::XMP,
            "CreatorTool",
            text_value("something else"),
        ));
        let extracted = PhotoMetadata::from_xmp(&graph);
        assert_eq!(extracted.licensors().len(), 1);
        assert_eq!(extracted.xmp.properties.len(), 1);
    }

    #[test]
    fn a_non_structure_array_item_is_skipped_not_fatal() {
        // Hostile/odd input: a Bag holding plain text where a structure is expected.
        let mut pm = PhotoMetadata::new();
        pm.xmp.set(XmpProperty::new(
            ns::IPTC_EXT,
            "ImageRegion",
            XmpValue::Array(XmpArray::Bag(vec![
                XmpItem::simple("not a region"),
                XmpItem::new(
                    ImageRegion {
                        identifier: Some("r1".to_owned()),
                        ..ImageRegion::default()
                    }
                    .to_xmp(),
                ),
            ])),
        ));
        let regions = pm.image_regions();
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0].identifier.as_deref(), Some("r1"));
        // A non-array property yields nothing at all rather than a bogus entry.
        pm.xmp.set(XmpProperty::new(
            ns::PLUS,
            "Licensor",
            text_value("not an array"),
        ));
        assert!(pm.licensors().is_empty());
    }
}
