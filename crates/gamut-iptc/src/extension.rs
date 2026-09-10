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
//! [`ImageRegion`] is built from, and retain what they do not model on the same terms. The
//! remaining Extension structures (`Location`, `PersonWDetails`, `CvTerm`, `EmbdEncRightsExpr`,
//! `ProductWGtin`, `RegistryEntry`, `CopyrightOwner`, `ImageCreator`, `ImageSupplier`,
//! `LinkedEncRightsExpr`, `EntityWRole`) have no typed model yet and pass through
//! [`PhotoMetadata::xmp`] untouched, exactly as all of them did before.
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
//! A typed view is a projection, so the model is narrower than the graph — but reading a structure
//! and writing it back loses nothing at all. What decides that is **round-trippability, not
//! readability**: a field becomes part of the typed value only when the property the writer will
//! emit for that value *reproduces* the field that was read — same value, same RDF container kind,
//! same qualifiers. Every other field is kept in the type's `other` list and re-emitted verbatim,
//! after the fields the model does write.
//!
//! One rule therefore covers every way a field falls outside the model:
//!
//! - a field the model does not name — a vendor extension, or the "any other metadata property"
//!   the standard explicitly allows an [`ImageRegion`] to carry;
//! - a field it names but cannot read — a coordinate whose text is not a number, an identifier
//!   holding a structure where text belongs, a language alternative with no `x-default` entry;
//! - a field it can read but could not write back as it stands — a URL held as an `rdf:resource`
//!   where the model writes element text, a value carrying a qualifier, a language alternative
//!   with entries beyond the default, an `rdf:Seq` or `rdf:Alt` where the model writes an
//!   `rdf:Bag`, an array holding an item of a kind the model does not take, a bare structure where
//!   the model writes an array of them, a coordinate that is not a value of the XMP `Real` type
//!   (`NaN`, an infinity, or a decimal that overflows to one), or two fields of a single name.
//!
//! Such a field reads as **absent** — the typed view does not report a value it would go on to
//! destroy — and survives a read-modify-write untouched.
//!
//! What a read-modify-write does change:
//!
//! - **field order within a structure**: a structure is re-emitted in the model's field order,
//!   with the retained fields last. A structure's fields are an unordered set (XMP Part 1 §6.3.3),
//!   so this is a re-ordering and not a loss; values, and the relative order of an array's items,
//!   are preserved.
//! - **the lexical form of a number**: a coordinate written `0.50` is re-emitted as `0.5`.
//! - **the case of an `x-default` language tag**: an entry tagged `X-Default` is re-emitted as
//!   `x-default`, which Part 1 §8.2.2.4 matches as the same tag.
//!
//! The last two re-spell a value without changing it, and doing them twice changes nothing more,
//! so they are the only two differences that still count as reproducing a field.
//!
//! A retained field whose name the model also carries is emitted only when the modelled field is
//! not: a structure with two fields of one name is ill-formed and does not read back, so the
//! modelled value stays the authority when there is one, and the retained field is written when it
//! is the only copy.
//!
//! # Lenient at the top level, exact inside a structure
//!
//! [`PhotoMetadata`]'s array accessors accept the shapes seen in the wild: a bare structure written
//! where the standard puts an array of structures reads as that array's single element. Nothing is
//! at risk there, because reading a property never rewrites it. Inside a structure the same
//! leniency would normalise the shape away on the way out, so it is not taken — such a field is
//! retained instead, and reads as absent.
//!
//! The array setters write the standard form: an `rdf:Bag`, unless the property they replace is
//! already an `rdf:Seq`, whose order the caller may be relying on.

use gamut_xmp::{XML_NAMESPACE, XmpArray, XmpItem, XmpMeta, XmpProperty, XmpValue};

use crate::photo_metadata::PhotoMetadata;
use crate::schema::ns;

// --- Reading a structure's field list, keeping only what the writer can reproduce -------------

/// The language tag of a language alternative's default entry (XMP Part 1 §8.2.2.4).
const X_DEFAULT: &str = "x-default";

/// A structure's field list under a typed read, remembering which fields the read consumed.
///
/// A field is consumed only when the property the writer will emit for the value read *reproduces*
/// that field (see [`reproduces`]); every other field is left for the type's `other` list and
/// re-emitted verbatim. Deciding it on the write side rather than on the read side is what makes
/// the three ways a field can fall outside the model — unnamed, unreadable, unwritable — one rule
/// (see the [module docs](self)).
struct Reader<'a> {
    /// The fields being read.
    fields: &'a [XmpProperty],
    /// Whether the read consumed the field at the same index.
    used: Vec<bool>,
}

impl<'a> Reader<'a> {
    /// A reader over `fields`, with nothing consumed yet.
    fn new(fields: &'a [XmpProperty]) -> Self {
        Self {
            fields,
            used: vec![false; fields.len()],
        }
    }

    /// Reads the field named `ns:name` through `parse`, and keeps the value only when the property
    /// `write` will emit for it reproduces the field that was read.
    ///
    /// Two fields of one name are never read: only one of them could be written back, so both are
    /// left to be kept verbatim instead.
    fn read<T>(
        &mut self,
        ns: &str,
        name: &str,
        parse: impl FnOnce(&'a XmpValue) -> Option<T>,
        write: impl FnOnce(&T) -> Option<XmpValue>,
    ) -> Option<T> {
        let fields = self.fields;
        let mut matches = fields
            .iter()
            .enumerate()
            .filter(|(_, p)| p.namespace == ns && p.name == name);
        let (index, field) = matches.next()?;
        if matches.next().is_some() {
            return None;
        }
        let value = parse(&field.value)?;
        let emitted = XmpProperty::new(ns, name, write(&value)?);
        if !reproduces(field, &emitted) {
            return None;
        }
        self.used[index] = true;
        Some(value)
    }

    /// The simple text of the field named `ns:name`.
    fn text(&mut self, ns: &str, name: &str) -> Option<String> {
        self.read(ns, name, parse_text, |value| Some(text_value(value)))
    }

    /// The `x-default` entry of the language-alternative field named `ns:name`.
    fn lang_alt(&mut self, ns: &str, name: &str) -> Option<String> {
        self.read(ns, name, parse_lang_alt, |value| {
            Some(lang_alt_value(value))
        })
    }

    /// Every simple item of the array field named `ns:name` (empty when the field is absent or the
    /// read did not consume it).
    fn list(&mut self, ns: &str, name: &str, ordered: bool) -> Vec<String> {
        self.read(ns, name, parse_list, |values| list_value(ordered, values))
            .unwrap_or_default()
    }

    /// The field named `ns:name` parsed as an XMP `Real`.
    fn number(&mut self, ns: &str, name: &str) -> Option<f64> {
        self.read(ns, name, parse_number, |value| number_value(*value))
    }

    /// The single structured field named `ns:name`, read through `parse` and written through
    /// `write`.
    fn nested<T>(
        &mut self,
        ns: &str,
        name: &str,
        parse: impl FnOnce(&[XmpProperty]) -> T,
        write: impl FnOnce(&T) -> XmpValue,
    ) -> Option<T> {
        self.read(
            ns,
            name,
            |value| structure(value).map(parse),
            |value| Some(write(value)),
        )
    }

    /// Every structure of the array field named `ns:name`, read through `parse` and written through
    /// `write` (empty when the field is absent or the read did not consume it).
    fn nested_array<T>(
        &mut self,
        ns: &str,
        name: &str,
        ordered: bool,
        parse: impl Fn(&[XmpProperty]) -> T,
        write: impl Fn(&T) -> XmpValue,
    ) -> Vec<T> {
        self.read(
            ns,
            name,
            |value| {
                let parsed: Vec<T> = structures(value).into_iter().map(&parse).collect();
                (!parsed.is_empty()).then_some(parsed)
            },
            |values| nested_array_value(ordered, values.iter().map(&write).collect()),
        )
        .unwrap_or_default()
    }

    /// Every field the read did not consume, cloned for verbatim retention.
    fn other(self) -> Vec<XmpProperty> {
        let Self { fields, used } = self;
        fields
            .iter()
            .zip(used)
            .filter(|&(_, used)| !used)
            .map(|(property, _)| property.clone())
            .collect()
    }
}

/// The structure field lists an array value holds, skipping items that are not structures.
///
/// A bare structure written where the standard puts an array reads as that array's single element
/// (see the [module docs](self)); everything that is neither reads as nothing at all.
fn structures(value: &XmpValue) -> Vec<&[XmpProperty]> {
    match value {
        XmpValue::Array(array) => array
            .items()
            .iter()
            .filter_map(|item| structure(&item.value))
            .collect(),
        single => structure(single).into_iter().collect(),
    }
}

/// The structure fields of `value`, or `None` if it is not a structure.
fn structure(value: &XmpValue) -> Option<&[XmpProperty]> {
    match value {
        XmpValue::Structured(fields) => Some(fields),
        _ => None,
    }
}

// --- Reproduction: whether writing back what was read gives the field back --------------------

/// Whether `emitted` — the property the writer will produce for the value read from `read` —
/// reproduces `read`.
///
/// Reproduction is equality of value and qualifiers, with the two lexical re-spellings the module
/// documents allowed: a number may be written in another form for the same value, and a language
/// tag may be re-cased, because XMP Part 1 §8.2.2.4 matches tags case-insensitively. A structure's
/// fields are an unordered set (Part 1 §6.3.3), so the model's field order is not a difference; an
/// array's items are ordered, and its RDF container kind is part of its value (Part 1 §6.3.4).
fn reproduces(read: &XmpProperty, emitted: &XmpProperty) -> bool {
    read.namespace == emitted.namespace
        && read.name == emitted.name
        && same_value(&read.value, &emitted.value)
        && same_qualifiers(&read.qualifiers, &emitted.qualifiers)
}

/// Whether two values carry the same information (see [`reproduces`]).
fn same_value(read: &XmpValue, emitted: &XmpValue) -> bool {
    match (read, emitted) {
        (XmpValue::Simple(read), XmpValue::Simple(emitted)) => {
            read == emitted || same_number(read, emitted)
        }
        (XmpValue::Uri(read), XmpValue::Uri(emitted)) => read == emitted,
        (XmpValue::Structured(read), XmpValue::Structured(emitted)) => {
            read.len() == emitted.len()
                && read
                    .iter()
                    .all(|field| emitted.iter().any(|other| reproduces(field, other)))
        }
        (XmpValue::Array(read), XmpValue::Array(emitted)) => same_array(read, emitted),
        _ => false,
    }
}

/// Whether two arrays are the same RDF container kind holding the same items in the same order.
fn same_array(read: &XmpArray, emitted: &XmpArray) -> bool {
    let items = match (read, emitted) {
        (XmpArray::Bag(read), XmpArray::Bag(emitted))
        | (XmpArray::Seq(read), XmpArray::Seq(emitted))
        | (XmpArray::Alt(read), XmpArray::Alt(emitted)) => (read, emitted),
        _ => return false,
    };
    items.0.len() == items.1.len()
        && items.0.iter().zip(items.1).all(|(read, emitted)| {
            same_value(&read.value, &emitted.value)
                && same_qualifiers(&read.qualifiers, &emitted.qualifiers)
        })
}

/// Whether two qualifier lists hold the same qualifiers, matching an `xml:lang` tag
/// case-insensitively (XMP Part 1 §8.2.2.4).
fn same_qualifiers(read: &[XmpProperty], emitted: &[XmpProperty]) -> bool {
    read.len() == emitted.len()
        && read.iter().all(|qualifier| {
            emitted
                .iter()
                .any(|other| match (lang(qualifier), lang(other)) {
                    (Some(read), Some(emitted)) => read.eq_ignore_ascii_case(emitted),
                    _ => reproduces(qualifier, other),
                })
        })
}

/// The tag `qualifier` carries if it is an `xml:lang` qualifier.
fn lang(qualifier: &XmpProperty) -> Option<&str> {
    (qualifier.namespace == XML_NAMESPACE && qualifier.name == "lang")
        .then(|| qualifier.text())
        .flatten()
}

/// Whether two texts spell the same finite XMP `Real` (Part 1 §8.2.1) — the difference between
/// ` 0.50 ` and `0.5`, which the writer's own formatting introduces.
fn same_number(read: &str, emitted: &str) -> bool {
    let number = |text: &str| {
        text.trim()
            .parse::<f64>()
            .ok()
            .filter(|value| value.is_finite())
    };
    matches!((number(read), number(emitted)), (Some(read), Some(emitted)) if read == emitted)
}

// --- The value each modelled field is read from and written back as ---------------------------
//
// Every field is read by a `parse_*` and written by the `*_value` beside it, so what the reader
// compares against is the very value the writer will emit and the two cannot drift apart.

/// The simple text of a value.
fn parse_text(value: &XmpValue) -> Option<String> {
    value.text().map(str::to_owned)
}

/// A simple text value.
fn text_value(value: &str) -> XmpValue {
    XmpValue::Simple(value.to_owned())
}

/// The `x-default` entry of a language alternative, tolerating a plain simple value.
///
/// The entry is found by its `xml:lang` qualifier, compared case-insensitively as XMP Part 1
/// §8.2.2.4 requires — the same match [`XmpMeta::get_lang_alt`] makes on a top-level property. An
/// alternative list with no default entry reads as absent, rather than having another language
/// relabelled as the default; one holding a language *beside* the default reads its default and is
/// then refused by [`reproduces`], because [`lang_alt_value`] would write the other language away.
fn parse_lang_alt(value: &XmpValue) -> Option<String> {
    match value {
        XmpValue::Array(XmpArray::Alt(items)) => items
            .iter()
            .find(|item| {
                item.lang()
                    .is_some_and(|lang| lang.eq_ignore_ascii_case(X_DEFAULT))
            })
            .and_then(XmpItem::text),
        simple => simple.text(),
    }
    .map(str::to_owned)
}

/// A language alternative holding one `x-default` entry.
fn lang_alt_value(value: &str) -> XmpValue {
    XmpValue::Array(XmpArray::Alt(vec![XmpItem::lang_text(
        X_DEFAULT,
        value.to_owned(),
    )]))
}

/// Every simple item of an array value, or `None` when the value is not an array or holds no
/// simple item.
fn parse_list(value: &XmpValue) -> Option<Vec<String>> {
    match value {
        XmpValue::Array(array) => {
            let texts: Vec<String> = array.texts().map(str::to_owned).collect();
            (!texts.is_empty()).then_some(texts)
        }
        _ => None,
    }
}

/// An array of simple text, or `None` when there is nothing to write.
fn list_value(ordered: bool, values: &[String]) -> Option<XmpValue> {
    array_value(ordered, values.iter().map(XmpItem::simple).collect())
}

/// A value parsed as an XMP `Real`; text that does not parse reads as absent.
fn parse_number(value: &XmpValue) -> Option<f64> {
    value.text()?.trim().parse().ok()
}

/// An XMP `Real`, or `None` for a value the type has no form for.
///
/// `NaN` and the infinities are not values of the XMP `Real` type (Part 1 §8.2.1), so they are
/// never written — and a coordinate stating one in the graph is therefore never consumed, and is
/// kept verbatim (see the [module docs](self)).
fn number_value(value: f64) -> Option<XmpValue> {
    value
        .is_finite()
        .then(|| XmpValue::Simple(value.to_string()))
}

/// An array of structure values, or `None` when there is nothing to write.
fn nested_array_value(ordered: bool, values: Vec<XmpValue>) -> Option<XmpValue> {
    array_value(ordered, values.into_iter().map(XmpItem::new).collect())
}

/// An `rdf:Seq` when `ordered` and an `rdf:Bag` otherwise, or `None` when there are no items: an
/// empty array says nothing a missing property does not.
fn array_value(ordered: bool, items: Vec<XmpItem>) -> Option<XmpValue> {
    if items.is_empty() {
        return None;
    }
    Some(XmpValue::Array(if ordered {
        XmpArray::Seq(items)
    } else {
        XmpArray::Bag(items)
    }))
}

// --- Writing helpers -------------------------------------------------------------------------

/// Appends `ns:name`, unless there is no value to write.
fn put(out: &mut Vec<XmpProperty>, ns: &str, name: &str, value: Option<XmpValue>) {
    if let Some(value) = value {
        out.push(XmpProperty::new(ns, name, value));
    }
}

/// Appends `ns:name` as simple text, unless the value is absent.
fn put_text(out: &mut Vec<XmpProperty>, ns: &str, name: &str, value: Option<&String>) {
    put(out, ns, name, value.map(String::as_str).map(text_value));
}

/// Appends `ns:name` as a language alternative holding one `x-default` item, unless absent.
fn put_lang_alt(out: &mut Vec<XmpProperty>, ns: &str, name: &str, value: Option<&String>) {
    put(out, ns, name, value.map(String::as_str).map(lang_alt_value));
}

/// Appends `ns:name` as an array of simple text, unless the list is empty.
fn put_list(out: &mut Vec<XmpProperty>, ns: &str, name: &str, ordered: bool, values: &[String]) {
    put(out, ns, name, list_value(ordered, values));
}

/// Appends `ns:name` as an XMP `Real`, unless the value is absent or has no `Real` form.
fn put_number(out: &mut Vec<XmpProperty>, ns: &str, name: &str, value: Option<f64>) {
    put(out, ns, name, value.and_then(number_value));
}

/// Appends `ns:name` as a single structure value, unless absent.
fn put_nested(out: &mut Vec<XmpProperty>, ns: &str, name: &str, value: Option<XmpValue>) {
    put(out, ns, name, value);
}

/// Appends `ns:name` as an array of structure values, unless the list is empty.
fn put_nested_array(
    out: &mut Vec<XmpProperty>,
    ns: &str,
    name: &str,
    ordered: bool,
    values: Vec<XmpValue>,
) {
    put(out, ns, name, nested_array_value(ordered, values));
}

// --- Verbatim retention of the fields a typed read did not consume ---------------------------

/// Appends the retained fields, skipping one whose `(namespace, name)` a *modelled* field already
/// emitted carries.
///
/// The retention list is public, so a caller can put a name the model also carries in it. Emitting
/// both would produce a structure with two fields of one name, which is ill-formed and does not
/// read back — so a namesake of a modelled field is dropped, but only when that field was actually
/// emitted. When it was not, the retained field is the only copy of that name and is written; and
/// two retained fields of one name are both written, because dropping either would lose a field
/// the graph carries.
fn put_other(out: &mut Vec<XmpProperty>, other: &[XmpProperty]) {
    let modelled = out.len();
    for property in other {
        if !out[..modelled]
            .iter()
            .any(|p| p.namespace == property.namespace && p.name == property.name)
        {
            out.push(property.clone());
        }
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
    /// Every other field the structure carries, verbatim, so a read-modify-write does not drop a
    /// vendor extension (see the [module docs](self)).
    pub other: Vec<XmpProperty>,
}

impl CreatorContactInfo {
    /// Reads the structure from an XMP value, or `None` if the value is not a structure.
    #[must_use]
    pub fn from_xmp(value: &XmpValue) -> Option<Self> {
        structure(value).map(Self::from_fields)
    }

    /// Reads the structure from an already-unwrapped field list.
    fn from_fields(f: &[XmpProperty]) -> Self {
        let mut r = Reader::new(f);
        Self {
            address: r.text(ns::IPTC_CORE, "CiAdrExtadr"),
            city: r.text(ns::IPTC_CORE, "CiAdrCity"),
            country: r.text(ns::IPTC_CORE, "CiAdrCtry"),
            postal_code: r.text(ns::IPTC_CORE, "CiAdrPcode"),
            region: r.text(ns::IPTC_CORE, "CiAdrRegion"),
            email: r.text(ns::IPTC_CORE, "CiEmailWork"),
            phone: r.text(ns::IPTC_CORE, "CiTelWork"),
            web_url: r.text(ns::IPTC_CORE, "CiUrlWork"),
            other: r.other(),
        }
    }

    /// Writes the structure as an XMP value, omitting absent fields and appending
    /// [`other`](Self::other) verbatim.
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
        put_other(&mut f, &self.other);
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
    /// Every other field the structure carries, verbatim, so a read-modify-write does not drop a
    /// vendor extension (see the [module docs](self)).
    pub other: Vec<XmpProperty>,
}

impl ArtworkOrObject {
    /// Reads the structure from an XMP value, or `None` if the value is not a structure.
    #[must_use]
    pub fn from_xmp(value: &XmpValue) -> Option<Self> {
        structure(value).map(Self::from_fields)
    }

    /// Reads the structure from an already-unwrapped field list.
    fn from_fields(f: &[XmpProperty]) -> Self {
        let mut r = Reader::new(f);
        Self {
            title: r.lang_alt(ns::IPTC_EXT, "AOTitle"),
            creator_names: r.list(ns::IPTC_EXT, "AOCreator", true),
            creator_identifiers: r.list(ns::IPTC_EXT, "AOCreatorId", true),
            date_created: r.text(ns::IPTC_EXT, "AODateCreated"),
            circa_date_created: r.text(ns::IPTC_EXT, "AOCircaDateCreated"),
            copyright_notice: r.text(ns::IPTC_EXT, "AOCopyrightNotice"),
            current_copyright_owner_name: r.text(ns::IPTC_EXT, "AOCurrentCopyrightOwnerName"),
            current_copyright_owner_identifier: r.text(ns::IPTC_EXT, "AOCurrentCopyrightOwnerId"),
            current_licensor_name: r.text(ns::IPTC_EXT, "AOCurrentLicensorName"),
            current_licensor_identifier: r.text(ns::IPTC_EXT, "AOCurrentLicensorId"),
            content_description: r.lang_alt(ns::IPTC_EXT, "AOContentDescription"),
            contribution_description: r.lang_alt(ns::IPTC_EXT, "AOContributionDescription"),
            physical_description: r.lang_alt(ns::IPTC_EXT, "AOPhysicalDescription"),
            source: r.text(ns::IPTC_EXT, "AOSource"),
            source_inventory_number: r.text(ns::IPTC_EXT, "AOSourceInvNo"),
            source_inventory_url: r.text(ns::IPTC_EXT, "AOSourceInvURL"),
            style_periods: r.list(ns::IPTC_EXT, "AOStylePeriod", false),
            other: r.other(),
        }
    }

    /// Writes the structure as an XMP value, omitting absent fields and appending
    /// [`other`](Self::other) verbatim.
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
        put_other(&mut f, &self.other);
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
    /// Every other field the structure carries, verbatim, so a read-modify-write does not drop a
    /// vendor extension (see the [module docs](self)).
    pub other: Vec<XmpProperty>,
}

impl Licensor {
    /// Reads the structure from an XMP value, or `None` if the value is not a structure.
    #[must_use]
    pub fn from_xmp(value: &XmpValue) -> Option<Self> {
        structure(value).map(Self::from_fields)
    }

    /// Reads the structure from an already-unwrapped field list.
    fn from_fields(f: &[XmpProperty]) -> Self {
        let mut r = Reader::new(f);
        Self {
            identifier: r.text(ns::PLUS, "LicensorID"),
            name: r.text(ns::PLUS, "LicensorName"),
            address: r.text(ns::PLUS, "LicensorStreetAddress"),
            address_detail: r.text(ns::PLUS, "LicensorExtendedAddress"),
            city: r.text(ns::PLUS, "LicensorCity"),
            region: r.text(ns::PLUS, "LicensorRegion"),
            postal_code: r.text(ns::PLUS, "LicensorPostalCode"),
            country: r.text(ns::PLUS, "LicensorCountry"),
            telephone_type1: r.text(ns::PLUS, "LicensorTelephoneType1"),
            telephone1: r.text(ns::PLUS, "LicensorTelephone1"),
            telephone_type2: r.text(ns::PLUS, "LicensorTelephoneType2"),
            telephone2: r.text(ns::PLUS, "LicensorTelephone2"),
            email: r.text(ns::PLUS, "LicensorEmail"),
            web_url: r.text(ns::PLUS, "LicensorURL"),
            other: r.other(),
        }
    }

    /// Writes the structure as an XMP value, omitting absent fields and appending
    /// [`other`](Self::other) verbatim.
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
        put_other(&mut f, &self.other);
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
    /// Every field the typed read took nothing from, verbatim, so a read-modify-write does not drop
    /// a vendor extension (see the [module docs](self)).
    pub other: Vec<XmpProperty>,
}

impl Entity {
    /// Reads the structure from an XMP value, or `None` if the value is not a structure.
    #[must_use]
    pub fn from_xmp(value: &XmpValue) -> Option<Self> {
        structure(value).map(Self::from_fields)
    }

    /// Reads the structure from an already-unwrapped field list.
    fn from_fields(f: &[XmpProperty]) -> Self {
        let mut r = Reader::new(f);
        Self {
            identifiers: r.list(ns::XMP, "Identifier", false),
            name: r.lang_alt(ns::IPTC_EXT, "Name"),
            other: r.other(),
        }
    }

    /// Writes the structure as an XMP value, omitting absent fields and appending
    /// [`other`](Self::other) verbatim.
    #[must_use]
    pub fn to_xmp(&self) -> XmpValue {
        let mut f = Vec::new();
        put_list(&mut f, ns::XMP, "Identifier", false, &self.identifiers);
        put_lang_alt(&mut f, ns::IPTC_EXT, "Name", self.name.as_ref());
        put_other(&mut f, &self.other);
        XmpValue::Structured(f)
    }
}

/// One vertex of a polygon region boundary (`Iptc4xmpExt:RegionBoundaryPoint`, IPTC Extension 1.8
/// §12.8).
#[derive(Debug, Clone, Default, PartialEq)]
#[non_exhaustive]
pub struct RegionBoundaryPoint {
    /// X-axis coordinate (`Iptc4xmpExt:rbX`).
    pub x: Option<f64>,
    /// Y-axis coordinate (`Iptc4xmpExt:rbY`).
    pub y: Option<f64>,
    /// Every field the typed read took nothing from, verbatim — including a coordinate whose text
    /// is not a number (see the [module docs](self)).
    pub other: Vec<XmpProperty>,
}

impl RegionBoundaryPoint {
    /// Reads the structure from an XMP value, or `None` if the value is not a structure.
    #[must_use]
    pub fn from_xmp(value: &XmpValue) -> Option<Self> {
        structure(value).map(Self::from_fields)
    }

    /// Reads the structure from an already-unwrapped field list.
    fn from_fields(f: &[XmpProperty]) -> Self {
        let mut r = Reader::new(f);
        Self {
            x: r.number(ns::IPTC_EXT, "rbX"),
            y: r.number(ns::IPTC_EXT, "rbY"),
            other: r.other(),
        }
    }

    /// Writes the structure as an XMP value, omitting absent fields and appending
    /// [`other`](Self::other) verbatim.
    #[must_use]
    pub fn to_xmp(&self) -> XmpValue {
        let mut f = Vec::new();
        put_number(&mut f, ns::IPTC_EXT, "rbX", self.x);
        put_number(&mut f, ns::IPTC_EXT, "rbY", self.y);
        put_other(&mut f, &self.other);
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
    /// Every field the typed read took nothing from, verbatim, so a read-modify-write does not drop
    /// a vendor extension (see the [module docs](self)).
    pub other: Vec<XmpProperty>,
}

impl RegionBoundary {
    /// Reads the structure from an XMP value, or `None` if the value is not a structure.
    #[must_use]
    pub fn from_xmp(value: &XmpValue) -> Option<Self> {
        structure(value).map(Self::from_fields)
    }

    /// Reads the structure from an already-unwrapped field list.
    fn from_fields(f: &[XmpProperty]) -> Self {
        let mut r = Reader::new(f);
        Self {
            shape: r.text(ns::IPTC_EXT, "rbShape"),
            unit: r.text(ns::IPTC_EXT, "rbUnit"),
            x: r.number(ns::IPTC_EXT, "rbX"),
            y: r.number(ns::IPTC_EXT, "rbY"),
            width: r.number(ns::IPTC_EXT, "rbW"),
            height: r.number(ns::IPTC_EXT, "rbH"),
            radius: r.number(ns::IPTC_EXT, "rbRx"),
            vertices: r.nested_array(
                ns::IPTC_EXT,
                "rbVertices",
                true,
                RegionBoundaryPoint::from_fields,
                RegionBoundaryPoint::to_xmp,
            ),
            other: r.other(),
        }
    }

    /// Writes the structure as an XMP value, omitting absent fields and appending
    /// [`other`](Self::other) verbatim.
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
        put_other(&mut f, &self.other);
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
    /// "other metadata property"), so a read-modify-write does not drop it.
    pub other: Vec<XmpProperty>,
}

impl ImageRegion {
    /// Reads the structure from an XMP value, or `None` if the value is not a structure.
    #[must_use]
    pub fn from_xmp(value: &XmpValue) -> Option<Self> {
        structure(value).map(Self::from_fields)
    }

    /// Reads the structure from an already-unwrapped field list.
    fn from_fields(f: &[XmpProperty]) -> Self {
        let mut r = Reader::new(f);
        Self {
            boundary: r.nested(
                ns::IPTC_EXT,
                "RegionBoundary",
                RegionBoundary::from_fields,
                RegionBoundary::to_xmp,
            ),
            identifier: r.text(ns::IPTC_EXT, "rId"),
            name: r.lang_alt(ns::IPTC_EXT, "Name"),
            content_types: r.nested_array(
                ns::IPTC_EXT,
                "rCtype",
                false,
                Entity::from_fields,
                Entity::to_xmp,
            ),
            roles: r.nested_array(
                ns::IPTC_EXT,
                "rRole",
                false,
                Entity::from_fields,
                Entity::to_xmp,
            ),
            other: r.other(),
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
        put_other(&mut f, &self.other);
        XmpValue::Structured(f)
    }
}

// --- The accessors on the unified view ---------------------------------------------------------

/// Reads every structure of the `Bag`/`Seq` property `ns:name` through `parse`, tolerating a bare
/// structure written where the array should be (see the [module docs](self)).
fn read_array<T>(xmp: &XmpMeta, ns: &str, name: &str, parse: fn(&[XmpProperty]) -> T) -> Vec<T> {
    match xmp.get(ns, name) {
        Some(property) => structures(&property.value).into_iter().map(parse).collect(),
        None => Vec::new(),
    }
}

/// Replaces the array property `ns:name` with `values`, and removes it when there is nothing left
/// to write.
///
/// The RDF container kind the property already carries is kept — an `rdf:Seq` a caller wrote for
/// its order stays a `Seq` — and anything else becomes the `rdf:Bag` the standard specifies. Every
/// value the caller passes is written, including one that carries no field at all, so that reading
/// an array and setting it back is the identity (see the [module docs](self)).
fn write_array(xmp: &mut XmpMeta, ns: &str, name: &str, values: Vec<XmpValue>) {
    let ordered = matches!(
        xmp.get(ns, name).map(|property| &property.value),
        Some(XmpValue::Array(XmpArray::Seq(_)))
    );
    match nested_array_value(ordered, values) {
        Some(value) => xmp.set(XmpProperty::new(ns, name, value)),
        None => drop(xmp.remove(ns, name)),
    }
}

impl PhotoMetadata {
    /// The creator's contact details (`Iptc4xmpCore:CreatorContactInfo`).
    #[must_use]
    pub fn creator_contact_info(&self) -> Option<CreatorContactInfo> {
        CreatorContactInfo::from_xmp(&self.xmp.get(ns::IPTC_CORE, "CreatorContactInfo")?.value)
    }

    /// Sets the creator's contact details (`Iptc4xmpCore:CreatorContactInfo`); a block with no
    /// fields at all removes the property, as an empty slice does for the array accessors.
    pub fn set_creator_contact_info(&mut self, info: &CreatorContactInfo) {
        let value = info.to_xmp();
        if structure(&value).is_some_and(<[XmpProperty]>::is_empty) {
            self.xmp.remove(ns::IPTC_CORE, "CreatorContactInfo");
            return;
        }
        self.xmp
            .set(XmpProperty::new(ns::IPTC_CORE, "CreatorContactInfo", value));
    }

    /// The image regions (`Iptc4xmpExt:ImageRegion`), in the order the graph holds them.
    #[must_use]
    pub fn image_regions(&self) -> Vec<ImageRegion> {
        read_array(
            &self.xmp,
            ns::IPTC_EXT,
            "ImageRegion",
            ImageRegion::from_fields,
        )
    }

    /// Sets the image regions (`Iptc4xmpExt:ImageRegion`); an empty slice removes the property,
    /// and an existing array keeps its container kind (see [`write_array`]).
    pub fn set_image_regions(&mut self, regions: &[ImageRegion]) {
        let values = regions.iter().map(ImageRegion::to_xmp).collect();
        write_array(&mut self.xmp, ns::IPTC_EXT, "ImageRegion", values);
    }

    /// The artworks or objects shown in the image (`Iptc4xmpExt:ArtworkOrObject`).
    #[must_use]
    pub fn artwork_or_objects(&self) -> Vec<ArtworkOrObject> {
        read_array(
            &self.xmp,
            ns::IPTC_EXT,
            "ArtworkOrObject",
            ArtworkOrObject::from_fields,
        )
    }

    /// Sets the artworks or objects shown in the image (`Iptc4xmpExt:ArtworkOrObject`); an empty
    /// slice removes the property, and an existing array keeps its container kind.
    pub fn set_artwork_or_objects(&mut self, artworks: &[ArtworkOrObject]) {
        let values = artworks.iter().map(ArtworkOrObject::to_xmp).collect();
        write_array(&mut self.xmp, ns::IPTC_EXT, "ArtworkOrObject", values);
    }

    /// The licensors of the image (`plus:Licensor`).
    #[must_use]
    pub fn licensors(&self) -> Vec<Licensor> {
        read_array(&self.xmp, ns::PLUS, "Licensor", Licensor::from_fields)
    }

    /// Sets the licensors of the image (`plus:Licensor`); an empty slice removes the property, and
    /// an existing array keeps its container kind.
    pub fn set_licensors(&mut self, licensors: &[Licensor]) {
        let values = licensors.iter().map(Licensor::to_xmp).collect();
        write_array(&mut self.xmp, ns::PLUS, "Licensor", values);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_value(s: &str) -> XmpValue {
        XmpValue::Simple(s.to_owned())
    }

    /// A structure's `from_xmp` → `to_xmp` round trip, named for the type it converts.
    type Trip = (&'static str, XmpValue, fn(&XmpValue) -> Option<XmpValue>);

    /// The field of `fields` named `ns:name`, for assertions about a field's container kind.
    fn field<'a>(fields: &'a [XmpProperty], ns: &str, name: &str) -> Option<&'a XmpProperty> {
        fields.iter().find(|p| p.namespace == ns && p.name == name)
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
            other: Vec::new(),
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
            Reader::new(fields).text(ns::IPTC_CORE, "CiAdrPcode"),
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
            other: Vec::new(),
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
            other: Vec::new(),
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
                ..RegionBoundary::default()
            }),
            identifier: Some("region-1".to_owned()),
            name: Some("Face".to_owned()),
            content_types: vec![Entity {
                identifiers: vec!["https://cv.iptc.org/newscodes/imageregiontype/human".to_owned()],
                name: Some("Human".to_owned()),
                ..Entity::default()
            }],
            roles: vec![Entity {
                identifiers: vec![
                    "https://cv.iptc.org/newscodes/imageregionrole/subjectArea".to_owned(),
                ],
                name: Some("Subject area".to_owned()),
                ..Entity::default()
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
        let entity = structures(&field(fields, ns::IPTC_EXT, "rCtype").unwrap().value)[0];
        assert!(field(entity, ns::XMP, "Identifier").is_some());
    }

    #[test]
    fn entity_round_trips_its_identifiers_and_name() {
        // An Entity reached through an ImageRegion is read field-list-first; this is the public
        // value-level conversion, which nothing else exercises with a structure value.
        let entity = Entity {
            identifiers: vec!["urn:a".to_owned(), "urn:b".to_owned()],
            name: Some("Human".to_owned()),
            ..Entity::default()
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
                    ..RegionBoundaryPoint::default()
                },
                RegionBoundaryPoint {
                    x: Some(20.0),
                    y: Some(30.0),
                    ..RegionBoundaryPoint::default()
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
        // Honest read: the typed view does not invent a number it cannot parse. Where the raw
        // value goes instead is `a_field_the_model_cannot_read_is_kept_verbatim`.
        let value = XmpValue::Structured(vec![
            XmpProperty::new(ns::IPTC_EXT, "rbX", text_value("halfway")),
            XmpProperty::new(ns::IPTC_EXT, "rbY", text_value(" 4.5 ")),
        ]);
        let point = RegionBoundaryPoint::from_xmp(&value).unwrap();
        assert_eq!(point.x, None);
        assert_eq!(point.y, Some(4.5));
    }

    #[test]
    fn a_default_entry_tagged_in_another_case_is_still_the_default() {
        // `X-Default` and `x-default` are one tag (XMP Part 1 §8.2.2.4), so the entry is read and
        // written back in the canonical case rather than kept as a language of its own.
        let value = XmpValue::Structured(vec![XmpProperty::new(
            ns::IPTC_EXT,
            "AOTitle",
            XmpValue::Array(XmpArray::Alt(vec![XmpItem::lang_text(
                "X-Default",
                "Sunflowers",
            )])),
        )]);
        let art = ArtworkOrObject::from_xmp(&value).unwrap();
        assert_eq!(art.title.as_deref(), Some("Sunflowers"));
        assert_eq!(
            art.to_xmp(),
            XmpValue::Structured(vec![XmpProperty::new(
                ns::IPTC_EXT,
                "AOTitle",
                lang_alt_value("Sunflowers"),
            )])
        );
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

    /// `value` (a structure) with one vendor-namespace field appended.
    fn with_vendor_field(value: &XmpValue) -> XmpValue {
        let mut fields = structure(value).expect("a structure value").to_vec();
        fields.push(XmpProperty::new(
            "http://example.org/vendor/",
            "Tint",
            text_value("warm"),
        ));
        XmpValue::Structured(fields)
    }

    #[test]
    fn every_structure_keeps_the_field_it_does_not_model() {
        // A read-modify-write must not drop a vendor extension, inside a nested structure as much
        // as beside it. Each conversion is checked as a fixed point over its own canonical output
        // plus one foreign field, so a type that silently replaced the graph value fails here.
        let art = ArtworkOrObject {
            title: Some("Sunflowers".to_owned()),
            ..ArtworkOrObject::default()
        };
        let licensor = Licensor {
            name: Some("Agence gamut".to_owned()),
            ..Licensor::default()
        };
        let region = ImageRegion {
            identifier: Some("r1".to_owned()),
            ..ImageRegion::default()
        };
        let entity = Entity {
            name: Some("Human".to_owned()),
            ..Entity::default()
        };
        let point = RegionBoundaryPoint {
            x: Some(1.0),
            y: Some(2.0),
            ..RegionBoundaryPoint::default()
        };
        let boundary = RegionBoundary {
            shape: Some("circle".to_owned()),
            radius: Some(0.5),
            ..RegionBoundary::default()
        };
        let trips: [Trip; 7] = [
            ("CreatorContactInfo", contact().to_xmp(), |v| {
                Some(CreatorContactInfo::from_xmp(v)?.to_xmp())
            }),
            ("ArtworkOrObject", art.to_xmp(), |v| {
                Some(ArtworkOrObject::from_xmp(v)?.to_xmp())
            }),
            ("Licensor", licensor.to_xmp(), |v| {
                Some(Licensor::from_xmp(v)?.to_xmp())
            }),
            ("ImageRegion", region.to_xmp(), |v| {
                Some(ImageRegion::from_xmp(v)?.to_xmp())
            }),
            ("Entity", entity.to_xmp(), |v| {
                Some(Entity::from_xmp(v)?.to_xmp())
            }),
            ("RegionBoundaryPoint", point.to_xmp(), |v| {
                Some(RegionBoundaryPoint::from_xmp(v)?.to_xmp())
            }),
            ("RegionBoundary", boundary.to_xmp(), |v| {
                Some(RegionBoundary::from_xmp(v)?.to_xmp())
            }),
        ];
        for (name, canonical, round_trip) in trips {
            let input = with_vendor_field(&canonical);
            assert_eq!(
                round_trip(&input).as_ref(),
                Some(&input),
                "{name} did not keep its unmodelled field"
            );
        }
    }

    #[test]
    fn a_retained_field_never_duplicates_a_modelled_one() {
        // The retention list is public, so a caller can put a modelled name in it. Emitting both
        // would make a structure carrying two `rId` fields, which is ill-formed and reads back as
        // only one of them.
        let region = ImageRegion {
            identifier: Some("r1".to_owned()),
            other: vec![XmpProperty::new(ns::IPTC_EXT, "rId", text_value("r2"))],
            ..ImageRegion::default()
        };
        let value = region.to_xmp();
        let fields = structure(&value).unwrap();
        assert_eq!(fields.iter().filter(|p| p.name == "rId").count(), 1);
        // The modelled value is the authority, and the output is a fixed point.
        assert_eq!(
            Reader::new(fields).text(ns::IPTC_EXT, "rId"),
            Some("r1".to_owned())
        );
        assert_eq!(
            ImageRegion::from_xmp(&value).map(|r| r.to_xmp()),
            Some(value.clone())
        );
    }

    #[test]
    fn reproduction_is_equality_apart_from_the_two_re_spellings() {
        // The relation the retention rule is built on, stated on its own: the same value spelled
        // another way reproduces a field, and nothing else does.
        let coordinate = |text: &str| XmpProperty::new(ns::IPTC_EXT, "rbX", text_value(text));
        assert!(reproduces(&coordinate(" 0.50 "), &coordinate("0.5")));
        assert!(!reproduces(&coordinate("0.5"), &coordinate("0.25")));
        assert!(!reproduces(&coordinate("left"), &coordinate("right")));
        // A value of another kind is another value, and a field of another name is another field.
        assert!(!reproduces(
            &coordinate("0.5"),
            &XmpProperty::new(ns::IPTC_EXT, "rbX", XmpValue::Uri("0.5".to_owned()))
        ));
        assert!(!reproduces(
            &coordinate("0.5"),
            &XmpProperty::new(ns::IPTC_EXT, "rbY", text_value("0.5"))
        ));
        assert!(!reproduces(
            &coordinate("0.5"),
            &XmpProperty::new(ns::XMP, "rbX", text_value("0.5"))
        ));
    }

    /// One shape a modelled field can arrive in, with both rules' machinery attached to it.
    struct Shape {
        /// What the shape is — the label the enumeration is published under.
        label: &'static str,
        /// The field as the graph holds it.
        field: XmpProperty,
        /// Runs the modelling read for that field, discarding the value: what is under test is
        /// whether the reader consumed the field, not what it parsed.
        read: fn(&mut Reader<'_>),
        /// Whether the typed read parses a value out of it — the rule retention used to be decided
        /// by, before the writer had a say.
        parses: fn(&XmpValue) -> bool,
        /// The owning type's `from_xmp` -> `to_xmp`.
        trip: fn(&XmpValue) -> Option<XmpValue>,
    }

    fn structured(fields: Vec<XmpProperty>) -> XmpValue {
        XmpValue::Structured(fields)
    }

    fn qualified(mut property: XmpProperty, lang: &str) -> XmpProperty {
        property
            .qualifiers
            .push(XmpProperty::new(XML_NAMESPACE, "lang", text_value(lang)));
        property
    }

    fn entity() -> XmpValue {
        Entity {
            name: Some("Human".to_owned()),
            ..Entity::default()
        }
        .to_xmp()
    }

    fn vertex() -> XmpValue {
        RegionBoundaryPoint {
            x: Some(1.0),
            ..RegionBoundaryPoint::default()
        }
        .to_xmp()
    }

    /// Every shape the module's fidelity rule is stated over: the canonical form of each modelled
    /// field kind, and every departure from it a graph can carry.
    ///
    /// Each shape is a single field of the type that models it, so `trip` is that type's
    /// read-modify-write over exactly this one field.
    fn shapes() -> Vec<Shape> {
        let contact: fn(&XmpValue) -> Option<XmpValue> =
            |v| Some(CreatorContactInfo::from_xmp(v)?.to_xmp());
        let artwork: fn(&XmpValue) -> Option<XmpValue> =
            |v| Some(ArtworkOrObject::from_xmp(v)?.to_xmp());
        let region: fn(&XmpValue) -> Option<XmpValue> =
            |v| Some(ImageRegion::from_xmp(v)?.to_xmp());
        let point: fn(&XmpValue) -> Option<XmpValue> =
            |v| Some(RegionBoundaryPoint::from_xmp(v)?.to_xmp());
        let boundary: fn(&XmpValue) -> Option<XmpValue> =
            |v| Some(RegionBoundary::from_xmp(v)?.to_xmp());

        let text: fn(&mut Reader<'_>) = |r| {
            r.text(ns::IPTC_CORE, "CiUrlWork");
        };
        let text_parses: fn(&XmpValue) -> bool = |v| parse_text(v).is_some();
        let lang_alt: fn(&mut Reader<'_>) = |r| {
            r.lang_alt(ns::IPTC_EXT, "AOTitle");
        };
        let lang_alt_parses: fn(&XmpValue) -> bool = |v| parse_lang_alt(v).is_some();
        let bag: fn(&mut Reader<'_>) = |r| {
            r.list(ns::IPTC_EXT, "AOStylePeriod", false);
        };
        let seq: fn(&mut Reader<'_>) = |r| {
            r.list(ns::IPTC_EXT, "AOCreator", true);
        };
        let list_parses: fn(&XmpValue) -> bool = |v| parse_list(v).is_some();
        let number: fn(&mut Reader<'_>) = |r| {
            r.number(ns::IPTC_EXT, "rbX");
        };
        let number_parses: fn(&XmpValue) -> bool = |v| parse_number(v).is_some();
        let nested: fn(&mut Reader<'_>) = |r| {
            r.nested(
                ns::IPTC_EXT,
                "RegionBoundary",
                RegionBoundary::from_fields,
                RegionBoundary::to_xmp,
            );
        };
        let nested_parses: fn(&XmpValue) -> bool = |v| structure(v).is_some();
        let nested_bag: fn(&mut Reader<'_>) = |r| {
            r.nested_array(
                ns::IPTC_EXT,
                "rCtype",
                false,
                Entity::from_fields,
                Entity::to_xmp,
            );
        };
        let nested_seq: fn(&mut Reader<'_>) = |r| {
            r.nested_array(
                ns::IPTC_EXT,
                "rbVertices",
                true,
                RegionBoundaryPoint::from_fields,
                RegionBoundaryPoint::to_xmp,
            );
        };
        let nested_array_parses: fn(&XmpValue) -> bool = |v| !structures(v).is_empty();

        let shape = |label, field, read, parses, trip| Shape {
            label,
            field,
            read,
            parses,
            trip,
        };
        let ext = |name, value| XmpProperty::new(ns::IPTC_EXT, name, value);
        vec![
            // --- the canonical form of each field kind: read, and written back unchanged --------
            shape(
                "text: element text",
                XmpProperty::new(
                    ns::IPTC_CORE,
                    "CiUrlWork",
                    text_value("https://example.org/"),
                ),
                text,
                text_parses,
                contact,
            ),
            shape(
                "lang alt: one x-default entry",
                ext("AOTitle", lang_alt_value("Sunflowers")),
                lang_alt,
                lang_alt_parses,
                artwork,
            ),
            shape(
                "list: a bag of text",
                ext(
                    "AOStylePeriod",
                    XmpValue::Array(XmpArray::Bag(vec![XmpItem::simple("Baroque")])),
                ),
                bag,
                list_parses,
                artwork,
            ),
            shape(
                "list: a seq of text",
                ext(
                    "AOCreator",
                    XmpValue::Array(XmpArray::Seq(vec![XmpItem::simple("Van Gogh")])),
                ),
                seq,
                list_parses,
                artwork,
            ),
            shape(
                "number: a decimal",
                ext("rbX", text_value("0.25")),
                number,
                number_parses,
                point,
            ),
            shape(
                "nested: a structure",
                ext(
                    "RegionBoundary",
                    structured(vec![ext("rbShape", text_value("circle"))]),
                ),
                nested,
                nested_parses,
                region,
            ),
            shape(
                "nested array: a bag of structures",
                ext(
                    "rCtype",
                    XmpValue::Array(XmpArray::Bag(vec![XmpItem::new(entity())])),
                ),
                nested_bag,
                nested_array_parses,
                region,
            ),
            shape(
                "nested array: a seq of structures",
                ext(
                    "rbVertices",
                    XmpValue::Array(XmpArray::Seq(vec![XmpItem::new(vertex())])),
                ),
                nested_seq,
                nested_array_parses,
                boundary,
            ),
            // --- departures from it: parsed by the typed read, but not writable back as they are
            shape(
                "text: a URL held as rdf:resource",
                XmpProperty::new(
                    ns::IPTC_CORE,
                    "CiUrlWork",
                    XmpValue::Uri("https://example.org/".to_owned()),
                ),
                text,
                text_parses,
                contact,
            ),
            shape(
                "text: a value carrying a qualifier",
                qualified(
                    XmpProperty::new(
                        ns::IPTC_CORE,
                        "CiUrlWork",
                        text_value("https://example.org/"),
                    ),
                    "fr",
                ),
                text,
                text_parses,
                contact,
            ),
            shape(
                "lang alt: another language beside the default",
                ext(
                    "AOTitle",
                    XmpValue::Array(XmpArray::Alt(vec![
                        XmpItem::lang_text(X_DEFAULT, "Sunflowers"),
                        XmpItem::lang_text("fr", "Tournesols"),
                    ])),
                ),
                lang_alt,
                lang_alt_parses,
                artwork,
            ),
            shape(
                "lang alt: plain text where an alternative belongs",
                ext("AOTitle", text_value("Sunflowers")),
                lang_alt,
                lang_alt_parses,
                artwork,
            ),
            shape(
                "list: an rdf:Alt where an array belongs",
                ext(
                    "AOStylePeriod",
                    XmpValue::Array(XmpArray::Alt(vec![
                        XmpItem::lang_text(X_DEFAULT, "Baroque"),
                        XmpItem::lang_text("fr", "baroque"),
                    ])),
                ),
                bag,
                list_parses,
                artwork,
            ),
            shape(
                "list: an rdf:Seq where an rdf:Bag belongs",
                ext(
                    "AOStylePeriod",
                    XmpValue::Array(XmpArray::Seq(vec![XmpItem::simple("Baroque")])),
                ),
                bag,
                list_parses,
                artwork,
            ),
            shape(
                "list: an item that is not text",
                ext(
                    "AOStylePeriod",
                    XmpValue::Array(XmpArray::Bag(vec![
                        XmpItem::simple("Baroque"),
                        XmpItem::new(structured(vec![ext("Nested", text_value("v"))])),
                    ])),
                ),
                bag,
                list_parses,
                artwork,
            ),
            shape(
                "list: an item held as rdf:resource",
                ext(
                    "AOStylePeriod",
                    XmpValue::Array(XmpArray::Bag(vec![XmpItem::new(XmpValue::Uri(
                        "https://example.org/".to_owned(),
                    ))])),
                ),
                bag,
                list_parses,
                artwork,
            ),
            shape(
                "number: text with no XMP Real value",
                ext("rbX", text_value("NaN")),
                number,
                number_parses,
                point,
            ),
            shape(
                "nested: a structure carrying a qualifier",
                qualified(
                    ext(
                        "RegionBoundary",
                        structured(vec![ext("rbShape", text_value("circle"))]),
                    ),
                    "fr",
                ),
                nested,
                nested_parses,
                region,
            ),
            shape(
                "nested array: a bare structure where an array belongs",
                ext("rCtype", entity()),
                nested_bag,
                nested_array_parses,
                region,
            ),
            shape(
                "nested array: an rdf:Bag where an rdf:Seq belongs",
                ext(
                    "rbVertices",
                    XmpValue::Array(XmpArray::Bag(vec![XmpItem::new(vertex())])),
                ),
                nested_seq,
                nested_array_parses,
                boundary,
            ),
            shape(
                "nested array: an item that is not a structure",
                ext(
                    "rCtype",
                    XmpValue::Array(XmpArray::Bag(vec![
                        XmpItem::new(entity()),
                        XmpItem::simple("not an entity"),
                    ])),
                ),
                nested_bag,
                nested_array_parses,
                region,
            ),
        ]
    }

    #[test]
    fn every_shape_survives_a_read_modify_write_unchanged() {
        // The law the module states: reading a structure and writing it back changes nothing.
        // A shape the writer reproduces goes out as the model's own output; one it cannot is kept
        // verbatim. Either way the graph that comes out is the graph that went in.
        for shape in shapes() {
            let input = structured(vec![shape.field]);
            assert_eq!(
                (shape.trip)(&input).as_ref(),
                Some(&input),
                "{}: a read-modify-write did not give the field back",
                shape.label
            );
        }
    }

    #[test]
    fn retention_covers_every_shape_the_typed_read_parses_but_cannot_write_back() {
        // Derived, not listed: a shape is retained under the new rule and would have been consumed
        // — and so destroyed — under the old one exactly when the typed read parses a value out of
        // it and the reader still leaves it alone. This is the enumeration the module documents.
        let mut destroyed = Vec::new();
        for shape in shapes() {
            let fields = [shape.field];
            let mut reader = Reader::new(&fields);
            (shape.read)(&mut reader);
            if (shape.parses)(&fields[0].value) && !reader.other().is_empty() {
                destroyed.push(shape.label);
            }
        }
        assert_eq!(
            destroyed,
            [
                "text: a URL held as rdf:resource",
                "text: a value carrying a qualifier",
                "lang alt: another language beside the default",
                "lang alt: plain text where an alternative belongs",
                "list: an rdf:Alt where an array belongs",
                "list: an rdf:Seq where an rdf:Bag belongs",
                "list: an item that is not text",
                "list: an item held as rdf:resource",
                "number: text with no XMP Real value",
                "nested: a structure carrying a qualifier",
                "nested array: a bare structure where an array belongs",
                "nested array: an rdf:Bag where an rdf:Seq belongs",
                "nested array: an item that is not a structure",
            ]
        );
    }

    #[test]
    fn a_non_finite_coordinate_is_neither_written_nor_destroyed() {
        // NaN and the infinities are not values of the XMP Real type: writing one would put a
        // value in the graph that no reader can take back as a number.
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let value = RegionBoundaryPoint {
                x: Some(bad),
                y: Some(1.5),
                ..RegionBoundaryPoint::default()
            }
            .to_xmp();
            let fields = structure(&value).unwrap();
            assert!(
                field(fields, ns::IPTC_EXT, "rbX").is_none(),
                "{bad} was written to the graph"
            );
            // The finite sibling is still written, so the skip is per value, not per structure.
            assert_eq!(Reader::new(fields).number(ns::IPTC_EXT, "rbY"), Some(1.5));
        }
        // The same values arriving *from the graph* must not be destroyed either: the read parses
        // them, so a rule that consumed whatever it could read would drop them on the way out.
        for stated in ["NaN", "inf", "-inf", "1e400"] {
            let input = XmpValue::Structured(vec![XmpProperty::new(
                ns::IPTC_EXT,
                "rbX",
                text_value(stated),
            )]);
            let point = RegionBoundaryPoint::from_xmp(&input).expect("a structure value");
            assert_eq!(point.x, None, "{stated} was reported as a coordinate");
            assert_eq!(point.to_xmp(), input, "{stated} was destroyed");
        }
    }

    #[test]
    fn a_coordinate_keeps_its_value_when_it_is_respelled() {
        // The one lexical change the writer makes to a number it consumed: ` 0.50 ` comes back as
        // `0.5`, the same value, and doing it again changes nothing more.
        let input = XmpValue::Structured(vec![XmpProperty::new(
            ns::IPTC_EXT,
            "rbX",
            text_value(" 0.50 "),
        )]);
        let point = RegionBoundaryPoint::from_xmp(&input).expect("a structure value");
        assert_eq!(point.x, Some(0.5));
        let written = point.to_xmp();
        assert_eq!(
            written,
            XmpValue::Structured(vec![XmpProperty::new(
                ns::IPTC_EXT,
                "rbX",
                text_value("0.5"),
            )])
        );
        assert_eq!(
            RegionBoundaryPoint::from_xmp(&written).map(|p| p.to_xmp()),
            Some(written.clone())
        );
    }

    #[test]
    fn two_fields_of_one_name_are_both_kept() {
        // Only one of them could be written back, so neither is read and both are retained: the
        // structure is ill-formed, and silently halving it would be a loss the caller cannot see.
        let input = XmpValue::Structured(vec![
            XmpProperty::new(ns::IPTC_EXT, "rId", text_value("r1")),
            XmpProperty::new(ns::IPTC_EXT, "rId", text_value("r2")),
        ]);
        let region = ImageRegion::from_xmp(&input).expect("a structure value");
        assert_eq!(region.identifier, None);
        assert_eq!(region.to_xmp(), input);
    }

    #[test]
    fn a_field_the_model_cannot_read_is_kept_verbatim() {
        // The defect this closes: a field whose *name* the model owns but whose *value* the typed
        // read rejects used to be neither parsed nor retained, so it vanished from the graph. It
        // reads as absent — and is written back unchanged, because the read consumed nothing.
        let structured = XmpValue::Structured(vec![XmpProperty::new(
            ns::IPTC_EXT,
            "Nested",
            text_value("v"),
        )]);
        let cases: [Trip; 3] = [
            // An identifier holding a structure where text belongs.
            (
                "rId",
                XmpValue::Structured(vec![XmpProperty::new(
                    ns::IPTC_EXT,
                    "rId",
                    structured.clone(),
                )]),
                |v| Some(ImageRegion::from_xmp(v)?.to_xmp()),
            ),
            // A coordinate carrying non-numeric text.
            (
                "rbX",
                XmpValue::Structured(vec![XmpProperty::new(
                    ns::IPTC_EXT,
                    "rbX",
                    text_value("halfway"),
                )]),
                |v| Some(RegionBoundaryPoint::from_xmp(v)?.to_xmp()),
            ),
            // A language alternative with no `x-default` entry: relabelling another language as
            // the default would destroy the only text the field has.
            (
                "AOTitle",
                XmpValue::Structured(vec![XmpProperty::new(
                    ns::IPTC_EXT,
                    "AOTitle",
                    XmpValue::Array(XmpArray::Alt(vec![XmpItem::lang_text("fr", "Tournesols")])),
                )]),
                |v| Some(ArtworkOrObject::from_xmp(v)?.to_xmp()),
            ),
        ];
        for (name, input, round_trip) in cases {
            assert_eq!(
                round_trip(&input).as_ref(),
                Some(&input),
                "{name}: an unreadable value must survive a read-modify-write"
            );
        }
        // ...and the typed field reads as absent rather than as an invented value.
        let region = ImageRegion::from_xmp(&XmpValue::Structured(vec![XmpProperty::new(
            ns::IPTC_EXT,
            "rId",
            structured,
        )]));
        assert_eq!(region.and_then(|r| r.identifier), None);
    }

    #[test]
    fn a_language_alternative_keeps_the_languages_beside_the_default() {
        // The model holds one string, so writing this field back would keep the default and
        // destroy the French entry. It is therefore not read, and the whole alternative survives.
        let value = XmpValue::Structured(vec![XmpProperty::new(
            ns::IPTC_EXT,
            "AOTitle",
            XmpValue::Array(XmpArray::Alt(vec![
                XmpItem::lang_text(X_DEFAULT, "Sunflowers"),
                XmpItem::lang_text("fr", "Tournesols"),
            ])),
        )]);
        let art = ArtworkOrObject::from_xmp(&value).unwrap();
        assert_eq!(art.title, None);
        assert_eq!(art.to_xmp(), value);
    }

    #[test]
    fn a_retained_field_is_written_when_the_modelled_field_is_absent() {
        // The retained field is then the only copy of that name, so dropping it as a namesake
        // would destroy it: there is no duplicate to avoid.
        let region = ImageRegion {
            identifier: None,
            other: vec![XmpProperty::new(ns::IPTC_EXT, "rId", text_value("r2"))],
            ..ImageRegion::default()
        };
        let value = region.to_xmp();
        let fields = structure(&value).expect("a structure value");
        assert_eq!(
            Reader::new(fields).text(ns::IPTC_EXT, "rId"),
            Some("r2".to_owned())
        );
    }

    #[test]
    fn setting_an_array_keeps_the_container_kind_the_property_already_has() {
        // Forcing an `rdf:Seq` a caller wrote back to an `rdf:Bag` throws away the one thing a Seq
        // states that a Bag does not, and the setter has the existing property in front of it. A
        // property that is not an array still becomes the standard Bag.
        let mut pm = PhotoMetadata::new();
        let region = ImageRegion {
            identifier: Some("r1".to_owned()),
            ..ImageRegion::default()
        };
        pm.xmp.set(XmpProperty::new(
            ns::IPTC_EXT,
            "ImageRegion",
            XmpValue::Array(XmpArray::Seq(vec![XmpItem::new(region.to_xmp())])),
        ));
        pm.set_image_regions(&pm.image_regions());
        assert!(matches!(
            pm.xmp
                .get(ns::IPTC_EXT, "ImageRegion")
                .expect("the property")
                .value,
            XmpValue::Array(XmpArray::Seq(_))
        ));
    }

    #[test]
    fn reading_an_array_and_setting_it_back_keeps_every_member() {
        // A member carrying no field at all is still a member: dropping it would shift every
        // later member's index, so `image_regions()` -> `set_image_regions()` would not be the
        // identity. `rbVertices` never dropped one, and the top-level setters now agree with it.
        let mut pm = PhotoMetadata::new();
        let region = ImageRegion {
            identifier: Some("r1".to_owned()),
            ..ImageRegion::default()
        };
        let regions = vec![ImageRegion::default(), region];
        pm.set_image_regions(&regions);
        assert_eq!(pm.image_regions(), regions);
        pm.set_image_regions(&pm.image_regions());
        assert_eq!(pm.image_regions(), regions);
    }

    #[test]
    fn an_empty_contact_block_removes_the_property() {
        // All four setters agree: nothing to say removes the property, rather than leaving behind
        // an empty structure a reader would report as "present but blank".
        let mut pm = PhotoMetadata::new();
        pm.set_creator_contact_info(&contact());
        pm.set_creator_contact_info(&CreatorContactInfo::default());
        assert_eq!(pm.creator_contact_info(), None);
        assert!(pm.xmp.properties.is_empty());
    }

    #[test]
    fn a_bare_structure_reads_as_a_one_element_sequence() {
        // Seen in the wild: a single structure written where the standard puts a Bag. Lenient on
        // read, strict on write — the Bag comes back on the way out.
        let licensor = Licensor {
            name: Some("Agence gamut".to_owned()),
            ..Licensor::default()
        };
        let mut pm = PhotoMetadata::new();
        pm.xmp
            .set(XmpProperty::new(ns::PLUS, "Licensor", licensor.to_xmp()));
        assert_eq!(pm.licensors(), vec![licensor]);

        // Inside a structure the same shape is kept verbatim instead, because writing it back
        // would normalise it into a Bag: `rCtype` reads as nothing and survives untouched.
        let region = XmpValue::Structured(vec![XmpProperty::new(
            ns::IPTC_EXT,
            "rCtype",
            Entity {
                name: Some("Human".to_owned()),
                ..Entity::default()
            }
            .to_xmp(),
        )]);
        let read = ImageRegion::from_xmp(&region).expect("a structure value");
        assert_eq!(read.content_types, Vec::new());
        assert_eq!(read.to_xmp(), region);

        pm.set_licensors(&pm.licensors());
        assert!(matches!(
            pm.xmp.get(ns::PLUS, "Licensor").unwrap().value,
            XmpValue::Array(XmpArray::Bag(_))
        ));
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
