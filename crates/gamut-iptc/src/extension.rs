//! Typed models for the structured IPTC properties: the IPTC Extension structures and the
//! Core's `Iptc4xmpCore:CreatorContactInfo`.
//!
//! IPTC's structured properties are ordinary XMP structure values ([`XmpValue::Structured`]),
//! usually inside a `Bag`. This module is a typed *projection* over that graph — the same
//! relationship [`gamut_exif::GpsInfo`](https://docs.rs/gamut-exif) has to its sub-IFD — not a
//! parallel representation: the raw properties stay in
//! [`PhotoMetadata::xmp`](crate::PhotoMetadata::xmp) and are what round-trips. Each type converts
//! both ways with `from_xmp`/`to_xmp`, and [`PhotoMetadata`](crate::PhotoMetadata) exposes one
//! accessor pair per property, in `photo_metadata`'s `structured` submodule; the field-level
//! machinery both sides share lives in the private `fields` submodule.
//!
//! # Which structures are modelled
//!
//! Every structure the IPTC Photo Metadata Standard 2025.1 defines (its technical reference's
//! `ipmd_struct`, apart from the `AltLang` language alternative, which is a field shape rather than
//! a structure), each on every top-level property that uses it:
//!
//! | Type | IPTC structure | XMP property | Serialization |
//! | ---- | -------------- | ------------ | ------------- |
//! | [`CreatorContactInfo`] | `CreatorContactInfo` | `Iptc4xmpCore:CreatorContactInfo` | a single structure |
//! | [`ImageRegion`] | `ImageRegion` | `Iptc4xmpExt:ImageRegion` | `Bag` of structures |
//! | [`ArtworkOrObject`] | `ArtworkOrObject` | `Iptc4xmpExt:ArtworkOrObject` | `Bag` of structures |
//! | [`Licensor`] | `Licensor` | `plus:Licensor` | `Bag` of structures |
//! | [`Location`] | `Location` | `Iptc4xmpExt:LocationCreated`, `Iptc4xmpExt:LocationShown` | `Bag` of structures |
//! | [`Person`] | `PersonWDetails` | `Iptc4xmpExt:PersonInImageWDetails` | `Bag` of structures |
//! | [`CvTerm`] | `CvTerm` | `Iptc4xmpExt:AboutCvTerm`, `Iptc4xmpExt:Genre` | `Bag` of structures |
//! | [`EntityWithRole`] | `EntityWRole` | `Iptc4xmpExt:Contributor` | `Bag` of structures |
//! | [`Product`] | `ProductWGtin` | `Iptc4xmpExt:ProductInImage` | `Bag` of structures |
//! | [`RegistryEntry`] | `RegistryEntry` | `Iptc4xmpExt:RegistryId` | `Bag` of structures |
//! | [`EmbeddedRightsExpression`] | `EmbdEncRightsExpr` | `Iptc4xmpExt:EmbdEncRightsExpr` | `Bag` of structures |
//! | [`LinkedRightsExpression`] | `LinkedEncRightsExpr` | `Iptc4xmpExt:LinkedEncRightsExpr` | `Bag` of structures |
//! | [`CopyrightOwner`] | `CopyrightOwner` | `plus:CopyrightOwner` | `Seq` of structures |
//! | [`ImageCreator`] | `ImageCreator` | `plus:ImageCreator` | `Seq` of structures |
//! | [`ImageSupplier`] | `ImageSupplier` | `plus:ImageSupplier` | `Seq` of structures |
//!
//! [`RegionBoundary`], [`RegionBoundaryPoint`] and [`Entity`] are the nested structures
//! [`ImageRegion`] is built from, and [`CvTerm`] is also the structure a [`Person`]'s
//! characteristics are built from; each retains what it does not model on the same terms.
//!
//! A Rust type is named for what it holds rather than after the reference's abbreviation where
//! that abbreviation names a variant (`PersonWDetails` is a [`Person`], `ProductWGtin` a
//! [`Product`], `EntityWRole` an [`EntityWithRole`]) or an encoding detail (`EmbdEncRightsExpr` is
//! an [`EmbeddedRightsExpression`], `LinkedEncRightsExpr` a [`LinkedRightsExpression`]).
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
//!   holding a language beside the default or plain text written where one belongs, an array of
//!   any container kind other than the one the model writes, an array holding an item of a kind
//!   the model does not take, a bare structure where the model writes an array of them, a
//!   coordinate whose text is not an XMP `Real` (`NaN`, an infinity, or exponent notation such as
//!   `1e-400`), a coordinate whose decimal value a double cannot hold exactly (more significant
//!   digits than it keeps, or a decimal that underflows or overflows), or two fields of a single
//!   name.
//!
//! `retention_covers_every_shape_the_typed_read_parses_but_cannot_write_back` derives that list
//! from the module's shape corpus rather than restating it, so it cannot fall out of date.
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
//! - **the lexical form of a number**: a coordinate written `0.50` is re-emitted as `0.5`. Only
//!   the spelling changes: the two must be the same decimal value, compared as decimals rather than
//!   as the doubles they parse to, so a coordinate the writer would round is retained instead.
//! - **the case of an `x-default` language tag**: an entry tagged `X-Default` is re-emitted as
//!   `x-default`, which Part 1 §8.2.2.4 matches as the same tag.
//!
//! The last two re-spell a value without changing it, and doing them twice changes nothing more,
//! so they are the only two differences that still count as reproducing a field.
//!
//! Deciding this on the write side costs one throwaway value per field read — the very value the
//! writer would emit — and, for a structure field, a throwaway conversion of the structure below
//! it. IPTC structures nest three deep at most and hold a handful of fields each, so the cost is
//! bounded by the size of the structure being read; nothing here is on an encoder's pixel path.
//!
//! A retained field whose name the model also carries is emitted only when the modelled field is
//! not: a structure with two fields of one name is ill-formed and does not read back, so the
//! modelled value stays the authority when there is one, and the retained field is written when it
//! is the only copy.
//!
//! # The retention slot
//!
//! Every type here keeps what its typed read did not consume in one public field,
//! `other: Vec<XmpProperty>`, and that is the whole of its retention API. It is plain data like
//! every field beside it: a caller reads it, edits it or builds it from [`Default`] and assigns it,
//! exactly as it treats [`XmpMeta::properties`](gamut_xmp::XmpMeta::properties) one level up,
//! and every structure added to this module carries the same field on the same terms.
//!
//! `to_xmp` writes whatever the slot holds, in its order, after the modelled fields, and refuses
//! nothing in it. The one field it leaves out is a namesake of a modelled field that was emitted,
//! by the rule above, because writing both would produce a structure that does not read back.
//! Everything else in the slot — a name the model owns whose modelled field is absent, a name no
//! schema defines, two fields of one name — is written as it stands, because each of those is a
//! shape a graph read from a file can hold, and the slot exists to give such a shape back.
//!
//! Whether a field's namespace and name can be serialized at all is not decided here: it is the
//! same question for a field in this slot as for a top-level property in
//! [`XmpMeta::properties`](gamut_xmp::XmpMeta::properties), and it belongs to the XMP writer that answers it for both. That writer does not check a local
//! name yet (issue #732), so an empty or non-XML name in either place serializes as it stands.
//!
//! Accessors were declined. A borrowed `other()`/`other_mut()` pair adds nothing over the field
//! while the element is [`XmpProperty`], itself plain public data; a checked `push_other` that
//! refused a model-owned name would refuse the very fields a read retains — a coordinate that is
//! not a number keeps its model-owned name in the slot — and one that refused an unusable name
//! would be a second copy of a check the XMP writer owns. An opaque element type would hide the
//! value tree a caller needs in order to read a vendor extension at all.
//!
//! # The typed view stays plain data
//!
//! The model does not widen to report a field it cannot write back, and that holds for every type
//! here and every structure added later. A text field is `Option<String>`, a language alternative
//! is its `x-default` text, and a list is `Vec<String>` written as the one container kind the
//! standard gives it. So the shapes a real file most often carries outside that read as absent: a
//! URL held as `rdf:resource` (`Iptc4xmpCore:CiUrlWork`, `Iptc4xmpExt:AOSourceInvURL`,
//! `plus:LicensorURL`, an item of `xmp:Identifier`), a value carrying a qualifier such as
//! `xml:lang`, a language alternative holding a language beside `x-default`, and an `rdf:Seq`
//! where the model writes an `rdf:Bag` or the reverse. Each one is in the type's `other` list,
//! as the graph held it, so a caller that wants it looks there by namespace and name; an
//! `rdf:resource` is an [`XmpValue::Uri`].
//!
//! Three widenings were weighed and declined:
//!
//! - **Remembering the value kind** — a URL type that knows it was an `rdf:resource` — would break
//!   three public fields, and `Entity::identifiers` would need a per-item type because the kind is
//!   per item.
//! - **Carrying the whole language alternative** — a list of (tag, text) pairs — would change the
//!   ergonomics of every descriptive field for the minority of files that hold a translation.
//! - **Carrying qualifiers** would still leave the general case open, because a qualifier can hang
//!   off any value, including an array item or a field of a nested structure.
//!
//! Each would turn plain data into a model of RDF, which the graph in
//! [`PhotoMetadata::xmp`](crate::PhotoMetadata::xmp) already is. Every type here is `#[non_exhaustive]`, so a later widening can add a field beside
//! the plain one without breaking a caller.
//!
//! # A value the projection cannot express
//!
//! Emission is **infallible**, for the whole module: every `to_xmp` returns an [`XmpValue`], and
//! every [`PhotoMetadata`](crate::PhotoMetadata) setter returns nothing. The one modelled value
//! that has no form in the type its property declares is a non-finite coordinate — `NaN` or an infinity in a
//! [`RegionBoundary`] or [`RegionBoundaryPoint`] field — because those are not values of the XMP
//! `Real` type (Part 1 §8.2.1); every other modelled field is text, a language alternative or a
//! list of either, and has a form for every value a caller can hold. A non-finite coordinate is
//! **not written**: the structure is emitted without that field, its other fields are written as
//! usual, and a retained field of the same name in `other` is written in its place (see above).
//! A graph that says nothing about a coordinate is preferred to one that states a number no reader
//! can take back as one. The caller is not told; a caller that needs to know checks
//! [`f64::is_finite`] on the coordinates it sets, which is where the value is decided.
//!
//! The read side follows the same rule: a coordinate whose text does not parse as an XMP `Real`
//! reads as `None` — no error, no invented number — and is kept verbatim in `other`, so it is
//! neither reported nor lost.
//!
//! The alternatives were weighed and declined. Returning `Result` from `to_xmp` would break all
//! public `to_xmp` methods and either make the structured setters fallible or have them swallow
//! the error, to report a value that can arise only from a caller's own arithmetic; and a checked
//! `Finite` coordinate type would turn plain-data fields into a validated API. Both would also
//! depart from the [`gamut_exif::GpsInfo`](https://docs.rs/gamut-exif) shape this module mirrors.
//!
//! # The same rule at both levels
//!
//! [`PhotoMetadata`](crate::PhotoMetadata)'s accessor pairs for these properties apply that gate
//! to the property itself, not only to the fields inside it. Inside a structure the retention list
//! is the type's `other` field; at the top level it is the graph — a property the read does not consume is left
//! exactly where it lies, and the setter beside it does not remove it. So a read that reports
//! nothing is never a read that destroys something: a bare structure written where the standard
//! puts an array of structures, an array member that is not a structure, an `rdf:li` or a property
//! carrying a qualifier, an array or structure holding nothing at all, and two top-level properties
//! of one name all read as absent and survive a read-modify-write untouched. Two of one name are
//! refused for the reason the field reader refuses two fields of one name: only one of them could
//! be written back.
//!
//! An array setter writes the RDF container kind the property it replaces already carries — an
//! `rdf:Bag`, an `rdf:Seq` whose order the caller may be relying on, or an `rdf:Alt` — and the
//! kind the standard specifies when it replaces nothing: an `rdf:Seq` for `plus:CopyrightOwner`,
//! `plus:ImageCreator` and `plus:ImageSupplier`, an `rdf:Bag` for every other array (see the table
//! above). It writes every value the caller
//! passes, including one carrying no field at all. Handed nothing to write, it removes the
//! property — but only when the read reported one, because a caller cannot have meant to clear a
//! property it was never shown.
//!
//! Retention is not merging. A setter handed values *replaces* the property — every property of
//! that name, where a graph holds more than one, so the value set is the value read next — and a
//! replaced property does not keep its qualifiers: the container kind is kept because it is part
//! of how the values themselves are held, and nothing else is. So the shapes above survive a
//! read-modify-write because the read declines to report them, not because the setter works around
//! them — a caller that reads nothing and then deliberately writes a value has replaced the
//! property, qualifiers and all, which is what a setter is for.

pub(crate) mod fields;
#[cfg(test)]
pub(crate) mod test_support;

use gamut_xmp::{WellKnownNs, XmpProperty, XmpValue};

use self::fields::{
    Reader, put_lang_alt, put_list, put_nested, put_nested_array, put_number, put_other, put_text,
    structure,
};

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
    /// Work web URL(s) (`Iptc4xmpCore:CiUrlWork`). A URL held as `rdf:resource` reads as `None`
    /// and stays in [`other`](Self::other) (see [the typed view](self#the-typed-view-stays-plain-data)).
    pub web_url: Option<String>,
    /// Every field the typed read did not consume, verbatim, written back after the modelled fields
    /// (the [retention slot](self#the-retention-slot)), so a read-modify-write does not drop a
    /// vendor extension.
    pub other: Vec<XmpProperty>,
}

impl CreatorContactInfo {
    /// Reads the structure from an XMP value, or `None` if the value is not a structure.
    #[must_use]
    pub fn from_xmp(value: &XmpValue) -> Option<Self> {
        structure(value).map(Self::from_fields)
    }

    /// Reads the structure from an already-unwrapped field list.
    pub(crate) fn from_fields(f: &[XmpProperty]) -> Self {
        let mut r = Reader::new(f);
        Self {
            address: r.text(WellKnownNs::Iptc4XmpCore.uri(), "CiAdrExtadr"),
            city: r.text(WellKnownNs::Iptc4XmpCore.uri(), "CiAdrCity"),
            country: r.text(WellKnownNs::Iptc4XmpCore.uri(), "CiAdrCtry"),
            postal_code: r.text(WellKnownNs::Iptc4XmpCore.uri(), "CiAdrPcode"),
            region: r.text(WellKnownNs::Iptc4XmpCore.uri(), "CiAdrRegion"),
            email: r.text(WellKnownNs::Iptc4XmpCore.uri(), "CiEmailWork"),
            phone: r.text(WellKnownNs::Iptc4XmpCore.uri(), "CiTelWork"),
            web_url: r.text(WellKnownNs::Iptc4XmpCore.uri(), "CiUrlWork"),
            other: r.other(),
        }
    }

    /// Writes the structure as an XMP value, omitting absent fields and appending
    /// [`other`](Self::other) verbatim.
    #[must_use]
    pub fn to_xmp(&self) -> XmpValue {
        let mut f = Vec::new();
        put_text(
            &mut f,
            WellKnownNs::Iptc4XmpCore.uri(),
            "CiAdrExtadr",
            self.address.as_ref(),
        );
        put_text(
            &mut f,
            WellKnownNs::Iptc4XmpCore.uri(),
            "CiAdrCity",
            self.city.as_ref(),
        );
        put_text(
            &mut f,
            WellKnownNs::Iptc4XmpCore.uri(),
            "CiAdrCtry",
            self.country.as_ref(),
        );
        put_text(
            &mut f,
            WellKnownNs::Iptc4XmpCore.uri(),
            "CiAdrPcode",
            self.postal_code.as_ref(),
        );
        put_text(
            &mut f,
            WellKnownNs::Iptc4XmpCore.uri(),
            "CiAdrRegion",
            self.region.as_ref(),
        );
        put_text(
            &mut f,
            WellKnownNs::Iptc4XmpCore.uri(),
            "CiEmailWork",
            self.email.as_ref(),
        );
        put_text(
            &mut f,
            WellKnownNs::Iptc4XmpCore.uri(),
            "CiTelWork",
            self.phone.as_ref(),
        );
        put_text(
            &mut f,
            WellKnownNs::Iptc4XmpCore.uri(),
            "CiUrlWork",
            self.web_url.as_ref(),
        );
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
    /// URL of the source's inventory record (`Iptc4xmpExt:AOSourceInvURL`). A URL held as
    /// `rdf:resource` reads as `None` and stays in [`other`](Self::other) (see
    /// [the typed view](self#the-typed-view-stays-plain-data)).
    pub source_inventory_url: Option<String>,
    /// Style periods (`Iptc4xmpExt:AOStylePeriod`).
    pub style_periods: Vec<String>,
    /// Every field the typed read did not consume, verbatim, written back after the modelled fields
    /// (the [retention slot](self#the-retention-slot)), so a read-modify-write does not drop a
    /// vendor extension.
    pub other: Vec<XmpProperty>,
}

impl ArtworkOrObject {
    /// Reads the structure from an XMP value, or `None` if the value is not a structure.
    #[must_use]
    pub fn from_xmp(value: &XmpValue) -> Option<Self> {
        structure(value).map(Self::from_fields)
    }

    /// Reads the structure from an already-unwrapped field list.
    pub(crate) fn from_fields(f: &[XmpProperty]) -> Self {
        let mut r = Reader::new(f);
        Self {
            title: r.lang_alt(WellKnownNs::Iptc4XmpExt.uri(), "AOTitle"),
            creator_names: r.list(WellKnownNs::Iptc4XmpExt.uri(), "AOCreator", true),
            creator_identifiers: r.list(WellKnownNs::Iptc4XmpExt.uri(), "AOCreatorId", true),
            date_created: r.text(WellKnownNs::Iptc4XmpExt.uri(), "AODateCreated"),
            circa_date_created: r.text(WellKnownNs::Iptc4XmpExt.uri(), "AOCircaDateCreated"),
            copyright_notice: r.text(WellKnownNs::Iptc4XmpExt.uri(), "AOCopyrightNotice"),
            current_copyright_owner_name: r.text(
                WellKnownNs::Iptc4XmpExt.uri(),
                "AOCurrentCopyrightOwnerName",
            ),
            current_copyright_owner_identifier: r
                .text(WellKnownNs::Iptc4XmpExt.uri(), "AOCurrentCopyrightOwnerId"),
            current_licensor_name: r.text(WellKnownNs::Iptc4XmpExt.uri(), "AOCurrentLicensorName"),
            current_licensor_identifier: r
                .text(WellKnownNs::Iptc4XmpExt.uri(), "AOCurrentLicensorId"),
            content_description: r.lang_alt(WellKnownNs::Iptc4XmpExt.uri(), "AOContentDescription"),
            contribution_description: r
                .lang_alt(WellKnownNs::Iptc4XmpExt.uri(), "AOContributionDescription"),
            physical_description: r
                .lang_alt(WellKnownNs::Iptc4XmpExt.uri(), "AOPhysicalDescription"),
            source: r.text(WellKnownNs::Iptc4XmpExt.uri(), "AOSource"),
            source_inventory_number: r.text(WellKnownNs::Iptc4XmpExt.uri(), "AOSourceInvNo"),
            source_inventory_url: r.text(WellKnownNs::Iptc4XmpExt.uri(), "AOSourceInvURL"),
            style_periods: r.list(WellKnownNs::Iptc4XmpExt.uri(), "AOStylePeriod", false),
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
        put_lang_alt(
            &mut f,
            WellKnownNs::Iptc4XmpExt.uri(),
            "AOTitle",
            self.title.as_ref(),
        );
        put_list(
            &mut f,
            WellKnownNs::Iptc4XmpExt.uri(),
            "AOCreator",
            true,
            &self.creator_names,
        );
        put_list(
            &mut f,
            WellKnownNs::Iptc4XmpExt.uri(),
            "AOCreatorId",
            true,
            &self.creator_identifiers,
        );
        put_text(
            &mut f,
            WellKnownNs::Iptc4XmpExt.uri(),
            "AODateCreated",
            self.date_created.as_ref(),
        );
        put_text(
            &mut f,
            WellKnownNs::Iptc4XmpExt.uri(),
            "AOCircaDateCreated",
            self.circa_date_created.as_ref(),
        );
        put_text(
            &mut f,
            WellKnownNs::Iptc4XmpExt.uri(),
            "AOCopyrightNotice",
            self.copyright_notice.as_ref(),
        );
        put_text(
            &mut f,
            WellKnownNs::Iptc4XmpExt.uri(),
            "AOCurrentCopyrightOwnerName",
            self.current_copyright_owner_name.as_ref(),
        );
        put_text(
            &mut f,
            WellKnownNs::Iptc4XmpExt.uri(),
            "AOCurrentCopyrightOwnerId",
            self.current_copyright_owner_identifier.as_ref(),
        );
        put_text(
            &mut f,
            WellKnownNs::Iptc4XmpExt.uri(),
            "AOCurrentLicensorName",
            self.current_licensor_name.as_ref(),
        );
        put_text(
            &mut f,
            WellKnownNs::Iptc4XmpExt.uri(),
            "AOCurrentLicensorId",
            self.current_licensor_identifier.as_ref(),
        );
        put_lang_alt(
            &mut f,
            WellKnownNs::Iptc4XmpExt.uri(),
            "AOContentDescription",
            self.content_description.as_ref(),
        );
        put_lang_alt(
            &mut f,
            WellKnownNs::Iptc4XmpExt.uri(),
            "AOContributionDescription",
            self.contribution_description.as_ref(),
        );
        put_lang_alt(
            &mut f,
            WellKnownNs::Iptc4XmpExt.uri(),
            "AOPhysicalDescription",
            self.physical_description.as_ref(),
        );
        put_text(
            &mut f,
            WellKnownNs::Iptc4XmpExt.uri(),
            "AOSource",
            self.source.as_ref(),
        );
        put_text(
            &mut f,
            WellKnownNs::Iptc4XmpExt.uri(),
            "AOSourceInvNo",
            self.source_inventory_number.as_ref(),
        );
        put_text(
            &mut f,
            WellKnownNs::Iptc4XmpExt.uri(),
            "AOSourceInvURL",
            self.source_inventory_url.as_ref(),
        );
        put_list(
            &mut f,
            WellKnownNs::Iptc4XmpExt.uri(),
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
    /// Web URL (`plus:LicensorURL`). A URL held as `rdf:resource` reads as `None` and stays in
    /// [`other`](Self::other) (see [the typed view](self#the-typed-view-stays-plain-data)).
    pub web_url: Option<String>,
    /// Every field the typed read did not consume, verbatim, written back after the modelled fields
    /// (the [retention slot](self#the-retention-slot)), so a read-modify-write does not drop a
    /// vendor extension.
    pub other: Vec<XmpProperty>,
}

impl Licensor {
    /// Reads the structure from an XMP value, or `None` if the value is not a structure.
    #[must_use]
    pub fn from_xmp(value: &XmpValue) -> Option<Self> {
        structure(value).map(Self::from_fields)
    }

    /// Reads the structure from an already-unwrapped field list.
    pub(crate) fn from_fields(f: &[XmpProperty]) -> Self {
        let mut r = Reader::new(f);
        Self {
            identifier: r.text(WellKnownNs::Plus.uri(), "LicensorID"),
            name: r.text(WellKnownNs::Plus.uri(), "LicensorName"),
            address: r.text(WellKnownNs::Plus.uri(), "LicensorStreetAddress"),
            address_detail: r.text(WellKnownNs::Plus.uri(), "LicensorExtendedAddress"),
            city: r.text(WellKnownNs::Plus.uri(), "LicensorCity"),
            region: r.text(WellKnownNs::Plus.uri(), "LicensorRegion"),
            postal_code: r.text(WellKnownNs::Plus.uri(), "LicensorPostalCode"),
            country: r.text(WellKnownNs::Plus.uri(), "LicensorCountry"),
            telephone_type1: r.text(WellKnownNs::Plus.uri(), "LicensorTelephoneType1"),
            telephone1: r.text(WellKnownNs::Plus.uri(), "LicensorTelephone1"),
            telephone_type2: r.text(WellKnownNs::Plus.uri(), "LicensorTelephoneType2"),
            telephone2: r.text(WellKnownNs::Plus.uri(), "LicensorTelephone2"),
            email: r.text(WellKnownNs::Plus.uri(), "LicensorEmail"),
            web_url: r.text(WellKnownNs::Plus.uri(), "LicensorURL"),
            other: r.other(),
        }
    }

    /// Writes the structure as an XMP value, omitting absent fields and appending
    /// [`other`](Self::other) verbatim.
    #[must_use]
    pub fn to_xmp(&self) -> XmpValue {
        let mut f = Vec::new();
        put_text(
            &mut f,
            WellKnownNs::Plus.uri(),
            "LicensorID",
            self.identifier.as_ref(),
        );
        put_text(
            &mut f,
            WellKnownNs::Plus.uri(),
            "LicensorName",
            self.name.as_ref(),
        );
        put_text(
            &mut f,
            WellKnownNs::Plus.uri(),
            "LicensorStreetAddress",
            self.address.as_ref(),
        );
        put_text(
            &mut f,
            WellKnownNs::Plus.uri(),
            "LicensorExtendedAddress",
            self.address_detail.as_ref(),
        );
        put_text(
            &mut f,
            WellKnownNs::Plus.uri(),
            "LicensorCity",
            self.city.as_ref(),
        );
        put_text(
            &mut f,
            WellKnownNs::Plus.uri(),
            "LicensorRegion",
            self.region.as_ref(),
        );
        put_text(
            &mut f,
            WellKnownNs::Plus.uri(),
            "LicensorPostalCode",
            self.postal_code.as_ref(),
        );
        put_text(
            &mut f,
            WellKnownNs::Plus.uri(),
            "LicensorCountry",
            self.country.as_ref(),
        );
        put_text(
            &mut f,
            WellKnownNs::Plus.uri(),
            "LicensorTelephoneType1",
            self.telephone_type1.as_ref(),
        );
        put_text(
            &mut f,
            WellKnownNs::Plus.uri(),
            "LicensorTelephone1",
            self.telephone1.as_ref(),
        );
        put_text(
            &mut f,
            WellKnownNs::Plus.uri(),
            "LicensorTelephoneType2",
            self.telephone_type2.as_ref(),
        );
        put_text(
            &mut f,
            WellKnownNs::Plus.uri(),
            "LicensorTelephone2",
            self.telephone2.as_ref(),
        );
        put_text(
            &mut f,
            WellKnownNs::Plus.uri(),
            "LicensorEmail",
            self.email.as_ref(),
        );
        put_text(
            &mut f,
            WellKnownNs::Plus.uri(),
            "LicensorURL",
            self.web_url.as_ref(),
        );
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
    /// Globally unique identifiers of the entity or concept (`xmp:Identifier`). A list holding
    /// any item as `rdf:resource` reads as empty and stays in [`other`](Self::other) whole (see
    /// [the typed view](self#the-typed-view-stays-plain-data)).
    pub identifiers: Vec<String>,
    /// Full name, `x-default` alternative (`Iptc4xmpExt:Name`).
    pub name: Option<String>,
    /// Every field the typed read did not consume, verbatim, written back after the modelled fields
    /// (the [retention slot](self#the-retention-slot)), so a read-modify-write does not drop a
    /// vendor extension.
    pub other: Vec<XmpProperty>,
}

impl Entity {
    /// Reads the structure from an XMP value, or `None` if the value is not a structure.
    #[must_use]
    pub fn from_xmp(value: &XmpValue) -> Option<Self> {
        structure(value).map(Self::from_fields)
    }

    /// Reads the structure from an already-unwrapped field list.
    pub(crate) fn from_fields(f: &[XmpProperty]) -> Self {
        let mut r = Reader::new(f);
        Self {
            identifiers: r.list(WellKnownNs::Xmp.uri(), "Identifier", false),
            name: r.lang_alt(WellKnownNs::Iptc4XmpExt.uri(), "Name"),
            other: r.other(),
        }
    }

    /// Writes the structure as an XMP value, omitting absent fields and appending
    /// [`other`](Self::other) verbatim.
    #[must_use]
    pub fn to_xmp(&self) -> XmpValue {
        let mut f = Vec::new();
        put_list(
            &mut f,
            WellKnownNs::Xmp.uri(),
            "Identifier",
            false,
            &self.identifiers,
        );
        put_lang_alt(
            &mut f,
            WellKnownNs::Iptc4XmpExt.uri(),
            "Name",
            self.name.as_ref(),
        );
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
    /// Every field the typed read did not consume, verbatim, written back after the modelled fields
    /// (the [retention slot](self#the-retention-slot)) — including a coordinate whose text is not
    /// an XMP `Real`, which keeps its model-owned name here.
    pub other: Vec<XmpProperty>,
}

impl RegionBoundaryPoint {
    /// Reads the structure from an XMP value, or `None` if the value is not a structure.
    #[must_use]
    pub fn from_xmp(value: &XmpValue) -> Option<Self> {
        structure(value).map(Self::from_fields)
    }

    /// Reads the structure from an already-unwrapped field list.
    pub(crate) fn from_fields(f: &[XmpProperty]) -> Self {
        let mut r = Reader::new(f);
        Self {
            x: r.number(WellKnownNs::Iptc4XmpExt.uri(), "rbX"),
            y: r.number(WellKnownNs::Iptc4XmpExt.uri(), "rbY"),
            other: r.other(),
        }
    }

    /// Writes the structure as an XMP value, omitting absent fields and appending
    /// [`other`](Self::other) verbatim.
    ///
    /// A coordinate that is `NaN` or infinite has no XMP `Real` form and is omitted like an absent
    /// one; nothing reports the omission (see the [module docs](self)).
    #[must_use]
    pub fn to_xmp(&self) -> XmpValue {
        let mut f = Vec::new();
        put_number(&mut f, WellKnownNs::Iptc4XmpExt.uri(), "rbX", self.x);
        put_number(&mut f, WellKnownNs::Iptc4XmpExt.uri(), "rbY", self.y);
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
    /// Every field the typed read did not consume, verbatim, written back after the modelled fields
    /// (the [retention slot](self#the-retention-slot)), so a read-modify-write does not drop a
    /// vendor extension.
    pub other: Vec<XmpProperty>,
}

impl RegionBoundary {
    /// Reads the structure from an XMP value, or `None` if the value is not a structure.
    #[must_use]
    pub fn from_xmp(value: &XmpValue) -> Option<Self> {
        structure(value).map(Self::from_fields)
    }

    /// Reads the structure from an already-unwrapped field list.
    pub(crate) fn from_fields(f: &[XmpProperty]) -> Self {
        let mut r = Reader::new(f);
        Self {
            shape: r.text(WellKnownNs::Iptc4XmpExt.uri(), "rbShape"),
            unit: r.text(WellKnownNs::Iptc4XmpExt.uri(), "rbUnit"),
            x: r.number(WellKnownNs::Iptc4XmpExt.uri(), "rbX"),
            y: r.number(WellKnownNs::Iptc4XmpExt.uri(), "rbY"),
            width: r.number(WellKnownNs::Iptc4XmpExt.uri(), "rbW"),
            height: r.number(WellKnownNs::Iptc4XmpExt.uri(), "rbH"),
            radius: r.number(WellKnownNs::Iptc4XmpExt.uri(), "rbRx"),
            vertices: r.nested_array(
                WellKnownNs::Iptc4XmpExt.uri(),
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
    /// sequence. A coordinate, extent or radius that is `NaN` or infinite has no XMP `Real` form
    /// and is omitted like an absent one; nothing reports the omission (see the
    /// [module docs](self)).
    #[must_use]
    pub fn to_xmp(&self) -> XmpValue {
        let mut f = Vec::new();
        put_text(
            &mut f,
            WellKnownNs::Iptc4XmpExt.uri(),
            "rbShape",
            self.shape.as_ref(),
        );
        put_text(
            &mut f,
            WellKnownNs::Iptc4XmpExt.uri(),
            "rbUnit",
            self.unit.as_ref(),
        );
        put_number(&mut f, WellKnownNs::Iptc4XmpExt.uri(), "rbX", self.x);
        put_number(&mut f, WellKnownNs::Iptc4XmpExt.uri(), "rbY", self.y);
        put_number(&mut f, WellKnownNs::Iptc4XmpExt.uri(), "rbW", self.width);
        put_number(&mut f, WellKnownNs::Iptc4XmpExt.uri(), "rbH", self.height);
        put_number(&mut f, WellKnownNs::Iptc4XmpExt.uri(), "rbRx", self.radius);
        put_nested_array(
            &mut f,
            WellKnownNs::Iptc4XmpExt.uri(),
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
    /// Every field the typed read did not consume, verbatim, written back after the modelled fields
    /// (the [retention slot](self#the-retention-slot)) — including the standard's "other metadata
    /// property", so a read-modify-write does not drop it.
    pub other: Vec<XmpProperty>,
}

impl ImageRegion {
    /// Reads the structure from an XMP value, or `None` if the value is not a structure.
    #[must_use]
    pub fn from_xmp(value: &XmpValue) -> Option<Self> {
        structure(value).map(Self::from_fields)
    }

    /// Reads the structure from an already-unwrapped field list.
    pub(crate) fn from_fields(f: &[XmpProperty]) -> Self {
        let mut r = Reader::new(f);
        Self {
            boundary: r.nested(
                WellKnownNs::Iptc4XmpExt.uri(),
                "RegionBoundary",
                RegionBoundary::from_fields,
                RegionBoundary::to_xmp,
            ),
            identifier: r.text(WellKnownNs::Iptc4XmpExt.uri(), "rId"),
            name: r.lang_alt(WellKnownNs::Iptc4XmpExt.uri(), "Name"),
            content_types: r.nested_array(
                WellKnownNs::Iptc4XmpExt.uri(),
                "rCtype",
                false,
                Entity::from_fields,
                Entity::to_xmp,
            ),
            roles: r.nested_array(
                WellKnownNs::Iptc4XmpExt.uri(),
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
            WellKnownNs::Iptc4XmpExt.uri(),
            "RegionBoundary",
            self.boundary.as_ref().map(RegionBoundary::to_xmp),
        );
        put_text(
            &mut f,
            WellKnownNs::Iptc4XmpExt.uri(),
            "rId",
            self.identifier.as_ref(),
        );
        put_lang_alt(
            &mut f,
            WellKnownNs::Iptc4XmpExt.uri(),
            "Name",
            self.name.as_ref(),
        );
        put_nested_array(
            &mut f,
            WellKnownNs::Iptc4XmpExt.uri(),
            "rCtype",
            false,
            self.content_types.iter().map(Entity::to_xmp).collect(),
        );
        put_nested_array(
            &mut f,
            WellKnownNs::Iptc4XmpExt.uri(),
            "rRole",
            false,
            self.roles.iter().map(Entity::to_xmp).collect(),
        );
        put_other(&mut f, &self.other);
        XmpValue::Structured(f)
    }
}

// --- Location ---------------------------------------------------------------------------------

/// A location (the IPTC Extension `Location` structure, IPTC Extension 1.8 §12.10), as used by
/// `Iptc4xmpExt:LocationCreated` and `Iptc4xmpExt:LocationShown`.
///
/// This is the structured successor of the Core's flat `photoshop:City`/`photoshop:State`/
/// `photoshop:Country` and `Iptc4xmpCore:Location`/`Iptc4xmpCore:CountryCode` scalars
/// ([`PhotoMetadata::city`](crate::PhotoMetadata::city) and its siblings). The two surfaces are
/// independent properties: gamut does not copy one into the other, and neither participates in the other's reconciliation.
///
/// The four GPS fields are the EXIF-in-XMP properties `exif:GPSLatitude`, `exif:GPSLongitude`,
/// `exif:GPSAltitude` and `exif:GPSAltitudeRef`. Their XMP types are the EXIF `GPSCoordinate`
/// (`"48,51.4N"`), `Rational` (`"35/1"`) and `Integer` (`"0"`/`"1"`) text forms, not decimal
/// numbers, so they are modelled as the text they carry — which also keeps them lossless.
///
/// This property is XMP-only and never participates in IIM↔XMP reconciliation (see the
/// [module docs](self)).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Location {
    /// Globally unique identifiers of the location (`Iptc4xmpExt:LocationId`).
    pub identifiers: Vec<String>,
    /// Full name, `x-default` alternative (`Iptc4xmpExt:LocationName`).
    pub name: Option<String>,
    /// Sublocation within the city (`Iptc4xmpExt:Sublocation`).
    pub sublocation: Option<String>,
    /// City (`Iptc4xmpExt:City`).
    pub city: Option<String>,
    /// Province or state (`Iptc4xmpExt:ProvinceState`).
    pub province_state: Option<String>,
    /// Country name (`Iptc4xmpExt:CountryName`).
    pub country_name: Option<String>,
    /// ISO 3166 country code (`Iptc4xmpExt:CountryCode`).
    pub country_code: Option<String>,
    /// World region (`Iptc4xmpExt:WorldRegion`).
    pub world_region: Option<String>,
    /// Latitude, as an EXIF `GPSCoordinate` text (`exif:GPSLatitude`).
    pub gps_latitude: Option<String>,
    /// Longitude, as an EXIF `GPSCoordinate` text (`exif:GPSLongitude`).
    pub gps_longitude: Option<String>,
    /// Altitude, as an EXIF `Rational` text (`exif:GPSAltitude`).
    pub gps_altitude: Option<String>,
    /// Altitude reference — `0` above, `1` below sea level (`exif:GPSAltitudeRef`).
    pub gps_altitude_ref: Option<String>,
    /// Every field the typed read did not consume, verbatim, written back after the modelled fields
    /// (the [retention slot](self#the-retention-slot)), so a read-modify-write does not drop a
    /// vendor extension.
    pub other: Vec<XmpProperty>,
}

impl Location {
    /// Reads the structure from an XMP value, or `None` if the value is not a structure.
    #[must_use]
    pub fn from_xmp(value: &XmpValue) -> Option<Self> {
        structure(value).map(Self::from_fields)
    }

    /// Reads the structure from an already-unwrapped field list.
    pub(crate) fn from_fields(f: &[XmpProperty]) -> Self {
        let mut r = Reader::new(f);
        Self {
            identifiers: r.list(WellKnownNs::Iptc4XmpExt.uri(), "LocationId", false),
            name: r.lang_alt(WellKnownNs::Iptc4XmpExt.uri(), "LocationName"),
            sublocation: r.text(WellKnownNs::Iptc4XmpExt.uri(), "Sublocation"),
            city: r.text(WellKnownNs::Iptc4XmpExt.uri(), "City"),
            province_state: r.text(WellKnownNs::Iptc4XmpExt.uri(), "ProvinceState"),
            country_name: r.text(WellKnownNs::Iptc4XmpExt.uri(), "CountryName"),
            country_code: r.text(WellKnownNs::Iptc4XmpExt.uri(), "CountryCode"),
            world_region: r.text(WellKnownNs::Iptc4XmpExt.uri(), "WorldRegion"),
            gps_latitude: r.text(WellKnownNs::Exif.uri(), "GPSLatitude"),
            gps_longitude: r.text(WellKnownNs::Exif.uri(), "GPSLongitude"),
            gps_altitude: r.text(WellKnownNs::Exif.uri(), "GPSAltitude"),
            gps_altitude_ref: r.text(WellKnownNs::Exif.uri(), "GPSAltitudeRef"),
            other: r.other(),
        }
    }

    /// Writes the structure as an XMP value, omitting absent fields and appending
    /// [`other`](Self::other) verbatim. `LocationId` is an unordered `Bag`.
    #[must_use]
    pub fn to_xmp(&self) -> XmpValue {
        let mut f = Vec::new();
        put_list(
            &mut f,
            WellKnownNs::Iptc4XmpExt.uri(),
            "LocationId",
            false,
            &self.identifiers,
        );
        put_lang_alt(
            &mut f,
            WellKnownNs::Iptc4XmpExt.uri(),
            "LocationName",
            self.name.as_ref(),
        );
        put_text(
            &mut f,
            WellKnownNs::Iptc4XmpExt.uri(),
            "Sublocation",
            self.sublocation.as_ref(),
        );
        put_text(
            &mut f,
            WellKnownNs::Iptc4XmpExt.uri(),
            "City",
            self.city.as_ref(),
        );
        put_text(
            &mut f,
            WellKnownNs::Iptc4XmpExt.uri(),
            "ProvinceState",
            self.province_state.as_ref(),
        );
        put_text(
            &mut f,
            WellKnownNs::Iptc4XmpExt.uri(),
            "CountryName",
            self.country_name.as_ref(),
        );
        put_text(
            &mut f,
            WellKnownNs::Iptc4XmpExt.uri(),
            "CountryCode",
            self.country_code.as_ref(),
        );
        put_text(
            &mut f,
            WellKnownNs::Iptc4XmpExt.uri(),
            "WorldRegion",
            self.world_region.as_ref(),
        );
        put_text(
            &mut f,
            WellKnownNs::Exif.uri(),
            "GPSLatitude",
            self.gps_latitude.as_ref(),
        );
        put_text(
            &mut f,
            WellKnownNs::Exif.uri(),
            "GPSLongitude",
            self.gps_longitude.as_ref(),
        );
        put_text(
            &mut f,
            WellKnownNs::Exif.uri(),
            "GPSAltitude",
            self.gps_altitude.as_ref(),
        );
        put_text(
            &mut f,
            WellKnownNs::Exif.uri(),
            "GPSAltitudeRef",
            self.gps_altitude_ref.as_ref(),
        );
        put_other(&mut f, &self.other);
        XmpValue::Structured(f)
    }
}

// --- Controlled-vocabulary term ---------------------------------------------------------------

/// A term from a controlled vocabulary (the IPTC Extension `CvTerm` structure, IPTC Extension 1.8
/// §12.2), as used by `Iptc4xmpExt:AboutCvTerm`, `Iptc4xmpExt:Genre` and a [`Person`]'s
/// characteristics.
///
/// This property is XMP-only and never participates in IIM↔XMP reconciliation (see the
/// [module docs](self)).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct CvTerm {
    /// Identifier of the controlled vocabulary the term is from (`Iptc4xmpExt:CvId`).
    pub vocabulary_identifier: Option<String>,
    /// Identifier of the term (`Iptc4xmpExt:CvTermId`).
    pub identifier: Option<String>,
    /// Name of the term, `x-default` alternative (`Iptc4xmpExt:CvTermName`).
    pub name: Option<String>,
    /// Refinement of what the term is about (`Iptc4xmpExt:CvTermRefinedAbout`).
    pub refined_about: Option<String>,
    /// Every field the typed read did not consume, verbatim, written back after the modelled fields
    /// (the [retention slot](self#the-retention-slot)), so a read-modify-write does not drop a
    /// vendor extension.
    pub other: Vec<XmpProperty>,
}

impl CvTerm {
    /// Reads the structure from an XMP value, or `None` if the value is not a structure.
    #[must_use]
    pub fn from_xmp(value: &XmpValue) -> Option<Self> {
        structure(value).map(Self::from_fields)
    }

    /// Reads the structure from an already-unwrapped field list.
    pub(crate) fn from_fields(f: &[XmpProperty]) -> Self {
        let mut r = Reader::new(f);
        Self {
            vocabulary_identifier: r.text(WellKnownNs::Iptc4XmpExt.uri(), "CvId"),
            identifier: r.text(WellKnownNs::Iptc4XmpExt.uri(), "CvTermId"),
            name: r.lang_alt(WellKnownNs::Iptc4XmpExt.uri(), "CvTermName"),
            refined_about: r.text(WellKnownNs::Iptc4XmpExt.uri(), "CvTermRefinedAbout"),
            other: r.other(),
        }
    }

    /// Writes the structure as an XMP value, omitting absent fields and appending
    /// [`other`](Self::other) verbatim.
    #[must_use]
    pub fn to_xmp(&self) -> XmpValue {
        let mut f = Vec::new();
        put_text(
            &mut f,
            WellKnownNs::Iptc4XmpExt.uri(),
            "CvId",
            self.vocabulary_identifier.as_ref(),
        );
        put_text(
            &mut f,
            WellKnownNs::Iptc4XmpExt.uri(),
            "CvTermId",
            self.identifier.as_ref(),
        );
        put_lang_alt(
            &mut f,
            WellKnownNs::Iptc4XmpExt.uri(),
            "CvTermName",
            self.name.as_ref(),
        );
        put_text(
            &mut f,
            WellKnownNs::Iptc4XmpExt.uri(),
            "CvTermRefinedAbout",
            self.refined_about.as_ref(),
        );
        put_other(&mut f, &self.other);
        XmpValue::Structured(f)
    }
}

// --- Person shown in the image ----------------------------------------------------------------

/// A person shown in the image, with details (the IPTC Extension `PersonWDetails` structure, IPTC
/// Extension 1.8 §12.11), as used by `Iptc4xmpExt:PersonInImageWDetails`.
///
/// This property is XMP-only and never participates in IIM↔XMP reconciliation (see the
/// [module docs](self)).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Person {
    /// Globally unique identifiers of the person (`Iptc4xmpExt:PersonId`).
    pub identifiers: Vec<String>,
    /// Name, `x-default` alternative (`Iptc4xmpExt:PersonName`).
    pub name: Option<String>,
    /// Characteristics of the person, as controlled-vocabulary terms
    /// (`Iptc4xmpExt:PersonCharacteristic`).
    pub characteristics: Vec<CvTerm>,
    /// Description, `x-default` alternative (`Iptc4xmpExt:PersonDescription`).
    pub description: Option<String>,
    /// Every field the typed read did not consume, verbatim, written back after the modelled fields
    /// (the [retention slot](self#the-retention-slot)), so a read-modify-write does not drop a
    /// vendor extension.
    pub other: Vec<XmpProperty>,
}

impl Person {
    /// Reads the structure from an XMP value, or `None` if the value is not a structure.
    #[must_use]
    pub fn from_xmp(value: &XmpValue) -> Option<Self> {
        structure(value).map(Self::from_fields)
    }

    /// Reads the structure from an already-unwrapped field list.
    pub(crate) fn from_fields(f: &[XmpProperty]) -> Self {
        let mut r = Reader::new(f);
        Self {
            identifiers: r.list(WellKnownNs::Iptc4XmpExt.uri(), "PersonId", false),
            name: r.lang_alt(WellKnownNs::Iptc4XmpExt.uri(), "PersonName"),
            characteristics: r.nested_array(
                WellKnownNs::Iptc4XmpExt.uri(),
                "PersonCharacteristic",
                false,
                CvTerm::from_fields,
                CvTerm::to_xmp,
            ),
            description: r.lang_alt(WellKnownNs::Iptc4XmpExt.uri(), "PersonDescription"),
            other: r.other(),
        }
    }

    /// Writes the structure as an XMP value, omitting absent fields and appending
    /// [`other`](Self::other) verbatim. `PersonId` and `PersonCharacteristic` are unordered
    /// `Bag`s.
    #[must_use]
    pub fn to_xmp(&self) -> XmpValue {
        let mut f = Vec::new();
        put_list(
            &mut f,
            WellKnownNs::Iptc4XmpExt.uri(),
            "PersonId",
            false,
            &self.identifiers,
        );
        put_lang_alt(
            &mut f,
            WellKnownNs::Iptc4XmpExt.uri(),
            "PersonName",
            self.name.as_ref(),
        );
        put_nested_array(
            &mut f,
            WellKnownNs::Iptc4XmpExt.uri(),
            "PersonCharacteristic",
            false,
            self.characteristics.iter().map(CvTerm::to_xmp).collect(),
        );
        put_lang_alt(
            &mut f,
            WellKnownNs::Iptc4XmpExt.uri(),
            "PersonDescription",
            self.description.as_ref(),
        );
        put_other(&mut f, &self.other);
        XmpValue::Structured(f)
    }
}

// --- Entity with role -------------------------------------------------------------------------

/// An entity or concept with the roles it plays (the IPTC Extension `EntityWRole` structure, IPTC
/// Extension 1.8 §12.5), as used by `Iptc4xmpExt:Contributor`.
///
/// It is [`Entity`] plus a role list; the shared fields carry the same names.
///
/// This property is XMP-only and never participates in IIM↔XMP reconciliation (see the
/// [module docs](self)).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct EntityWithRole {
    /// Globally unique identifiers of the entity or concept (`xmp:Identifier`).
    pub identifiers: Vec<String>,
    /// Full name, `x-default` alternative (`Iptc4xmpExt:Name`).
    pub name: Option<String>,
    /// Identifiers of the roles the entity plays (`Iptc4xmpExt:Role`).
    pub roles: Vec<String>,
    /// Every field the typed read did not consume, verbatim, written back after the modelled fields
    /// (the [retention slot](self#the-retention-slot)), so a read-modify-write does not drop a
    /// vendor extension.
    pub other: Vec<XmpProperty>,
}

impl EntityWithRole {
    /// Reads the structure from an XMP value, or `None` if the value is not a structure.
    #[must_use]
    pub fn from_xmp(value: &XmpValue) -> Option<Self> {
        structure(value).map(Self::from_fields)
    }

    /// Reads the structure from an already-unwrapped field list.
    pub(crate) fn from_fields(f: &[XmpProperty]) -> Self {
        let mut r = Reader::new(f);
        Self {
            identifiers: r.list(WellKnownNs::Xmp.uri(), "Identifier", false),
            name: r.lang_alt(WellKnownNs::Iptc4XmpExt.uri(), "Name"),
            roles: r.list(WellKnownNs::Iptc4XmpExt.uri(), "Role", false),
            other: r.other(),
        }
    }

    /// Writes the structure as an XMP value, omitting absent fields and appending
    /// [`other`](Self::other) verbatim. `xmp:Identifier` and `Role` are unordered `Bag`s.
    #[must_use]
    pub fn to_xmp(&self) -> XmpValue {
        let mut f = Vec::new();
        put_list(
            &mut f,
            WellKnownNs::Xmp.uri(),
            "Identifier",
            false,
            &self.identifiers,
        );
        put_lang_alt(
            &mut f,
            WellKnownNs::Iptc4XmpExt.uri(),
            "Name",
            self.name.as_ref(),
        );
        put_list(
            &mut f,
            WellKnownNs::Iptc4XmpExt.uri(),
            "Role",
            false,
            &self.roles,
        );
        put_other(&mut f, &self.other);
        XmpValue::Structured(f)
    }
}

// --- Product shown in the image ---------------------------------------------------------------

/// A product shown in the image (the IPTC Extension `ProductWGtin` structure, IPTC Extension 1.8
/// §12.12), as used by `Iptc4xmpExt:ProductInImage`.
///
/// This property is XMP-only and never participates in IIM↔XMP reconciliation (see the
/// [module docs](self)).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Product {
    /// Globally unique identifiers of the product (`Iptc4xmpExt:ProductId`).
    pub identifiers: Vec<String>,
    /// Name, `x-default` alternative (`Iptc4xmpExt:ProductName`).
    pub name: Option<String>,
    /// GS1 Global Trade Item Number (`Iptc4xmpExt:ProductGTIN`).
    pub gtin: Option<String>,
    /// Description, `x-default` alternative (`Iptc4xmpExt:ProductDescription`).
    pub description: Option<String>,
    /// Every field the typed read did not consume, verbatim, written back after the modelled fields
    /// (the [retention slot](self#the-retention-slot)), so a read-modify-write does not drop a
    /// vendor extension.
    pub other: Vec<XmpProperty>,
}

impl Product {
    /// Reads the structure from an XMP value, or `None` if the value is not a structure.
    #[must_use]
    pub fn from_xmp(value: &XmpValue) -> Option<Self> {
        structure(value).map(Self::from_fields)
    }

    /// Reads the structure from an already-unwrapped field list.
    pub(crate) fn from_fields(f: &[XmpProperty]) -> Self {
        let mut r = Reader::new(f);
        Self {
            identifiers: r.list(WellKnownNs::Iptc4XmpExt.uri(), "ProductId", false),
            name: r.lang_alt(WellKnownNs::Iptc4XmpExt.uri(), "ProductName"),
            gtin: r.text(WellKnownNs::Iptc4XmpExt.uri(), "ProductGTIN"),
            description: r.lang_alt(WellKnownNs::Iptc4XmpExt.uri(), "ProductDescription"),
            other: r.other(),
        }
    }

    /// Writes the structure as an XMP value, omitting absent fields and appending
    /// [`other`](Self::other) verbatim. `ProductId` is an unordered `Bag`.
    #[must_use]
    pub fn to_xmp(&self) -> XmpValue {
        let mut f = Vec::new();
        put_list(
            &mut f,
            WellKnownNs::Iptc4XmpExt.uri(),
            "ProductId",
            false,
            &self.identifiers,
        );
        put_lang_alt(
            &mut f,
            WellKnownNs::Iptc4XmpExt.uri(),
            "ProductName",
            self.name.as_ref(),
        );
        put_text(
            &mut f,
            WellKnownNs::Iptc4XmpExt.uri(),
            "ProductGTIN",
            self.gtin.as_ref(),
        );
        put_lang_alt(
            &mut f,
            WellKnownNs::Iptc4XmpExt.uri(),
            "ProductDescription",
            self.description.as_ref(),
        );
        put_other(&mut f, &self.other);
        XmpValue::Structured(f)
    }
}

// --- Registry entry ---------------------------------------------------------------------------

/// An entry of the image in a registry (the IPTC Extension `RegistryEntry` structure, IPTC
/// Extension 1.8 §11.21), as used by `Iptc4xmpExt:RegistryId`.
///
/// This property is XMP-only and never participates in IIM↔XMP reconciliation (see the
/// [module docs](self)).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct RegistryEntry {
    /// Identifier of the image within the registry (`Iptc4xmpExt:RegItemId`).
    pub item_identifier: Option<String>,
    /// Identifier of the registry (`Iptc4xmpExt:RegOrgId`).
    pub organisation_identifier: Option<String>,
    /// Role of the entry (`Iptc4xmpExt:RegEntryRole`).
    pub role: Option<String>,
    /// Every field the typed read did not consume, verbatim, written back after the modelled fields
    /// (the [retention slot](self#the-retention-slot)), so a read-modify-write does not drop a
    /// vendor extension.
    pub other: Vec<XmpProperty>,
}

impl RegistryEntry {
    /// Reads the structure from an XMP value, or `None` if the value is not a structure.
    #[must_use]
    pub fn from_xmp(value: &XmpValue) -> Option<Self> {
        structure(value).map(Self::from_fields)
    }

    /// Reads the structure from an already-unwrapped field list.
    pub(crate) fn from_fields(f: &[XmpProperty]) -> Self {
        let mut r = Reader::new(f);
        Self {
            item_identifier: r.text(WellKnownNs::Iptc4XmpExt.uri(), "RegItemId"),
            organisation_identifier: r.text(WellKnownNs::Iptc4XmpExt.uri(), "RegOrgId"),
            role: r.text(WellKnownNs::Iptc4XmpExt.uri(), "RegEntryRole"),
            other: r.other(),
        }
    }

    /// Writes the structure as an XMP value, omitting absent fields and appending
    /// [`other`](Self::other) verbatim.
    #[must_use]
    pub fn to_xmp(&self) -> XmpValue {
        let mut f = Vec::new();
        put_text(
            &mut f,
            WellKnownNs::Iptc4XmpExt.uri(),
            "RegItemId",
            self.item_identifier.as_ref(),
        );
        put_text(
            &mut f,
            WellKnownNs::Iptc4XmpExt.uri(),
            "RegOrgId",
            self.organisation_identifier.as_ref(),
        );
        put_text(
            &mut f,
            WellKnownNs::Iptc4XmpExt.uri(),
            "RegEntryRole",
            self.role.as_ref(),
        );
        put_other(&mut f, &self.other);
        XmpValue::Structured(f)
    }
}

// --- Rights expressions -----------------------------------------------------------------------

/// A rights expression embedded in the metadata (the IPTC Extension `EmbdEncRightsExpr`
/// structure, IPTC Extension 1.8 §12.3), as used by `Iptc4xmpExt:EmbdEncRightsExpr`.
///
/// This property is XMP-only and never participates in IIM↔XMP reconciliation (see the
/// [module docs](self)).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct EmbeddedRightsExpression {
    /// The encoded rights expression itself (`Iptc4xmpExt:EncRightsExpr`).
    pub expression: Option<String>,
    /// IANA media type of the expression's encoding (`Iptc4xmpExt:RightsExprEncType`).
    pub encoding_type: Option<String>,
    /// Identifier of the rights expression language (`Iptc4xmpExt:RightsExprLangId`).
    pub language_identifier: Option<String>,
    /// Every field the typed read did not consume, verbatim, written back after the modelled fields
    /// (the [retention slot](self#the-retention-slot)), so a read-modify-write does not drop a
    /// vendor extension.
    pub other: Vec<XmpProperty>,
}

impl EmbeddedRightsExpression {
    /// Reads the structure from an XMP value, or `None` if the value is not a structure.
    #[must_use]
    pub fn from_xmp(value: &XmpValue) -> Option<Self> {
        structure(value).map(Self::from_fields)
    }

    /// Reads the structure from an already-unwrapped field list.
    pub(crate) fn from_fields(f: &[XmpProperty]) -> Self {
        let mut r = Reader::new(f);
        Self {
            expression: r.text(WellKnownNs::Iptc4XmpExt.uri(), "EncRightsExpr"),
            encoding_type: r.text(WellKnownNs::Iptc4XmpExt.uri(), "RightsExprEncType"),
            language_identifier: r.text(WellKnownNs::Iptc4XmpExt.uri(), "RightsExprLangId"),
            other: r.other(),
        }
    }

    /// Writes the structure as an XMP value, omitting absent fields and appending
    /// [`other`](Self::other) verbatim.
    #[must_use]
    pub fn to_xmp(&self) -> XmpValue {
        let mut f = Vec::new();
        put_text(
            &mut f,
            WellKnownNs::Iptc4XmpExt.uri(),
            "EncRightsExpr",
            self.expression.as_ref(),
        );
        put_text(
            &mut f,
            WellKnownNs::Iptc4XmpExt.uri(),
            "RightsExprEncType",
            self.encoding_type.as_ref(),
        );
        put_text(
            &mut f,
            WellKnownNs::Iptc4XmpExt.uri(),
            "RightsExprLangId",
            self.language_identifier.as_ref(),
        );
        put_other(&mut f, &self.other);
        XmpValue::Structured(f)
    }
}

/// A rights expression the metadata links to (the IPTC Extension `LinkedEncRightsExpr`
/// structure, IPTC Extension 1.8 §12.9), as used by `Iptc4xmpExt:LinkedEncRightsExpr`.
///
/// This property is XMP-only and never participates in IIM↔XMP reconciliation (see the
/// [module docs](self)).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct LinkedRightsExpression {
    /// URL of the rights expression (`Iptc4xmpExt:LinkedRightsExpr`). A URL held as
    /// `rdf:resource` reads as `None` and stays in [`other`](Self::other) (see
    /// [the typed view](self#the-typed-view-stays-plain-data)).
    pub url: Option<String>,
    /// IANA media type of the expression's encoding (`Iptc4xmpExt:RightsExprEncType`).
    pub encoding_type: Option<String>,
    /// Identifier of the rights expression language (`Iptc4xmpExt:RightsExprLangId`).
    pub language_identifier: Option<String>,
    /// Every field the typed read did not consume, verbatim, written back after the modelled fields
    /// (the [retention slot](self#the-retention-slot)), so a read-modify-write does not drop a
    /// vendor extension.
    pub other: Vec<XmpProperty>,
}

impl LinkedRightsExpression {
    /// Reads the structure from an XMP value, or `None` if the value is not a structure.
    #[must_use]
    pub fn from_xmp(value: &XmpValue) -> Option<Self> {
        structure(value).map(Self::from_fields)
    }

    /// Reads the structure from an already-unwrapped field list.
    pub(crate) fn from_fields(f: &[XmpProperty]) -> Self {
        let mut r = Reader::new(f);
        Self {
            url: r.text(WellKnownNs::Iptc4XmpExt.uri(), "LinkedRightsExpr"),
            encoding_type: r.text(WellKnownNs::Iptc4XmpExt.uri(), "RightsExprEncType"),
            language_identifier: r.text(WellKnownNs::Iptc4XmpExt.uri(), "RightsExprLangId"),
            other: r.other(),
        }
    }

    /// Writes the structure as an XMP value, omitting absent fields and appending
    /// [`other`](Self::other) verbatim.
    #[must_use]
    pub fn to_xmp(&self) -> XmpValue {
        let mut f = Vec::new();
        put_text(
            &mut f,
            WellKnownNs::Iptc4XmpExt.uri(),
            "LinkedRightsExpr",
            self.url.as_ref(),
        );
        put_text(
            &mut f,
            WellKnownNs::Iptc4XmpExt.uri(),
            "RightsExprEncType",
            self.encoding_type.as_ref(),
        );
        put_text(
            &mut f,
            WellKnownNs::Iptc4XmpExt.uri(),
            "RightsExprLangId",
            self.language_identifier.as_ref(),
        );
        put_other(&mut f, &self.other);
        XmpValue::Structured(f)
    }
}

// --- PLUS parties: copyright owner, image creator, image supplier -----------------------------

/// A copyright owner of the image (`plus:CopyrightOwner`, a PLUS 1.2 structure embedded in the
/// IPTC Extension schema, IPTC Extension 1.8 §11.9).
///
/// The standard caps the property at three copyright owners; gamut does not enforce that on read
/// or write — an over-long array reads as the owners it holds, and
/// [`set_copyright_owners`](crate::PhotoMetadata::set_copyright_owners) writes every owner it is
/// given.
///
/// This property is XMP-only and never participates in IIM↔XMP reconciliation (see the
/// [module docs](self)).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct CopyrightOwner {
    /// Copyright owner identifier (`plus:CopyrightOwnerID`).
    pub identifier: Option<String>,
    /// Copyright owner name (`plus:CopyrightOwnerName`).
    pub name: Option<String>,
    /// Every field the typed read did not consume, verbatim, written back after the modelled fields
    /// (the [retention slot](self#the-retention-slot)), so a read-modify-write does not drop a
    /// vendor extension.
    pub other: Vec<XmpProperty>,
}

impl CopyrightOwner {
    /// Reads the structure from an XMP value, or `None` if the value is not a structure.
    #[must_use]
    pub fn from_xmp(value: &XmpValue) -> Option<Self> {
        structure(value).map(Self::from_fields)
    }

    /// Reads the structure from an already-unwrapped field list.
    pub(crate) fn from_fields(f: &[XmpProperty]) -> Self {
        let mut r = Reader::new(f);
        Self {
            identifier: r.text(WellKnownNs::Plus.uri(), "CopyrightOwnerID"),
            name: r.text(WellKnownNs::Plus.uri(), "CopyrightOwnerName"),
            other: r.other(),
        }
    }

    /// Writes the structure as an XMP value, omitting absent fields and appending
    /// [`other`](Self::other) verbatim.
    #[must_use]
    pub fn to_xmp(&self) -> XmpValue {
        let mut f = Vec::new();
        put_text(
            &mut f,
            WellKnownNs::Plus.uri(),
            "CopyrightOwnerID",
            self.identifier.as_ref(),
        );
        put_text(
            &mut f,
            WellKnownNs::Plus.uri(),
            "CopyrightOwnerName",
            self.name.as_ref(),
        );
        put_other(&mut f, &self.other);
        XmpValue::Structured(f)
    }
}

/// A creator of the image (`plus:ImageCreator`, a PLUS 1.2 structure embedded in the IPTC
/// Extension schema, IPTC Extension 1.8 §11.18).
///
/// The standard caps the property at three image creators; gamut does not enforce that on read or
/// write — an over-long array reads as the creators it holds, and
/// [`set_image_creators`](crate::PhotoMetadata::set_image_creators) writes every creator it is
/// given.
///
/// This property is XMP-only and never participates in IIM↔XMP reconciliation (see the
/// [module docs](self)).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ImageCreator {
    /// Image creator identifier (`plus:ImageCreatorID`).
    pub identifier: Option<String>,
    /// Image creator name (`plus:ImageCreatorName`).
    pub name: Option<String>,
    /// Every field the typed read did not consume, verbatim, written back after the modelled fields
    /// (the [retention slot](self#the-retention-slot)), so a read-modify-write does not drop a
    /// vendor extension.
    pub other: Vec<XmpProperty>,
}

impl ImageCreator {
    /// Reads the structure from an XMP value, or `None` if the value is not a structure.
    #[must_use]
    pub fn from_xmp(value: &XmpValue) -> Option<Self> {
        structure(value).map(Self::from_fields)
    }

    /// Reads the structure from an already-unwrapped field list.
    pub(crate) fn from_fields(f: &[XmpProperty]) -> Self {
        let mut r = Reader::new(f);
        Self {
            identifier: r.text(WellKnownNs::Plus.uri(), "ImageCreatorID"),
            name: r.text(WellKnownNs::Plus.uri(), "ImageCreatorName"),
            other: r.other(),
        }
    }

    /// Writes the structure as an XMP value, omitting absent fields and appending
    /// [`other`](Self::other) verbatim.
    #[must_use]
    pub fn to_xmp(&self) -> XmpValue {
        let mut f = Vec::new();
        put_text(
            &mut f,
            WellKnownNs::Plus.uri(),
            "ImageCreatorID",
            self.identifier.as_ref(),
        );
        put_text(
            &mut f,
            WellKnownNs::Plus.uri(),
            "ImageCreatorName",
            self.name.as_ref(),
        );
        put_other(&mut f, &self.other);
        XmpValue::Structured(f)
    }
}

/// A supplier of the image (`plus:ImageSupplier`, a PLUS 1.2 structure embedded in the IPTC
/// Extension schema, IPTC Extension 1.8 §11.22).
///
/// The standard caps the property at one image supplier while its XMP form is a `Seq`; gamut does
/// not enforce that on read or write — an over-long array reads as the suppliers it holds, and
/// [`set_image_suppliers`](crate::PhotoMetadata::set_image_suppliers) writes every supplier it is
/// given.
///
/// This property is XMP-only and never participates in IIM↔XMP reconciliation (see the
/// [module docs](self)).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ImageSupplier {
    /// Image supplier identifier (`plus:ImageSupplierID`).
    pub identifier: Option<String>,
    /// Image supplier name (`plus:ImageSupplierName`).
    pub name: Option<String>,
    /// Every field the typed read did not consume, verbatim, written back after the modelled fields
    /// (the [retention slot](self#the-retention-slot)), so a read-modify-write does not drop a
    /// vendor extension.
    pub other: Vec<XmpProperty>,
}

impl ImageSupplier {
    /// Reads the structure from an XMP value, or `None` if the value is not a structure.
    #[must_use]
    pub fn from_xmp(value: &XmpValue) -> Option<Self> {
        structure(value).map(Self::from_fields)
    }

    /// Reads the structure from an already-unwrapped field list.
    pub(crate) fn from_fields(f: &[XmpProperty]) -> Self {
        let mut r = Reader::new(f);
        Self {
            identifier: r.text(WellKnownNs::Plus.uri(), "ImageSupplierID"),
            name: r.text(WellKnownNs::Plus.uri(), "ImageSupplierName"),
            other: r.other(),
        }
    }

    /// Writes the structure as an XMP value, omitting absent fields and appending
    /// [`other`](Self::other) verbatim.
    #[must_use]
    pub fn to_xmp(&self) -> XmpValue {
        let mut f = Vec::new();
        put_text(
            &mut f,
            WellKnownNs::Plus.uri(),
            "ImageSupplierID",
            self.identifier.as_ref(),
        );
        put_text(
            &mut f,
            WellKnownNs::Plus.uri(),
            "ImageSupplierName",
            self.name.as_ref(),
        );
        put_other(&mut f, &self.other);
        XmpValue::Structured(f)
    }
}

#[cfg(test)]
mod tests {
    use gamut_xmp::{XmpArray, XmpItem};

    use super::fields::{
        X_DEFAULT, lang_alt_value, parse_lang_alt, parse_list, parse_number, parse_text, structures,
    };
    use super::test_support::{
        contact, contributor, copyright_owner, cv_term, embedded_rights, image_creator,
        image_supplier, linked_rights, location, person, product, qualified, registry_entry,
        structured, text_value,
    };
    use super::*;

    /// A structure's `from_xmp` → `to_xmp` round trip, named for the type it converts.
    type Trip = (&'static str, XmpValue, fn(&XmpValue) -> Option<XmpValue>);

    /// The field of `fields` named `ns:name`, for assertions about a field's container kind.
    fn field<'a>(fields: &'a [XmpProperty], ns: &str, name: &str) -> Option<&'a XmpProperty> {
        fields.iter().find(|p| p.namespace == ns && p.name == name)
    }

    #[test]
    fn creator_contact_info_round_trips_every_field() {
        let info = contact();
        let value = info.to_xmp();
        assert_eq!(CreatorContactInfo::from_xmp(&value), Some(info));
        // Each field is its own `Iptc4xmpCore:Ci*` property; absent fields are not written.
        let fields = structure(&value).unwrap();
        assert_eq!(fields.len(), 8);
        assert!(
            fields
                .iter()
                .all(|p| p.namespace == WellKnownNs::Iptc4XmpCore.uri())
        );
        assert_eq!(
            Reader::new(fields).text(WellKnownNs::Iptc4XmpCore.uri(), "CiAdrPcode"),
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
        assert_eq!(Location::from_xmp(&text), None);
        assert_eq!(CvTerm::from_xmp(&text), None);
        assert_eq!(Person::from_xmp(&text), None);
        assert_eq!(EntityWithRole::from_xmp(&text), None);
        assert_eq!(Product::from_xmp(&text), None);
        assert_eq!(RegistryEntry::from_xmp(&text), None);
        assert_eq!(EmbeddedRightsExpression::from_xmp(&text), None);
        assert_eq!(LinkedRightsExpression::from_xmp(&text), None);
        assert_eq!(CopyrightOwner::from_xmp(&text), None);
        assert_eq!(ImageCreator::from_xmp(&text), None);
        assert_eq!(ImageSupplier::from_xmp(&text), None);
    }

    /// Each of the eleven structures added for issue #538 with every field set, its `to_xmp` →
    /// `from_xmp` round trip, and the number of fields it writes.
    #[test]
    fn every_added_structure_round_trips_every_field() {
        fn trip<T: PartialEq + std::fmt::Debug>(
            label: &str,
            value: &T,
            to_xmp: fn(&T) -> XmpValue,
            from_xmp: fn(&XmpValue) -> Option<T>,
            fields: usize,
        ) {
            let written = to_xmp(value);
            assert_eq!(from_xmp(&written).as_ref(), Some(value), "{label}");
            assert_eq!(structure(&written).unwrap().len(), fields, "{label}");
        }
        trip(
            "Location",
            &location(),
            Location::to_xmp,
            Location::from_xmp,
            12,
        );
        trip(
            "CvTerm",
            &cv_term("outdoors"),
            CvTerm::to_xmp,
            CvTerm::from_xmp,
            4,
        );
        trip("Person", &person(), Person::to_xmp, Person::from_xmp, 4);
        trip(
            "EntityWithRole",
            &contributor(),
            EntityWithRole::to_xmp,
            EntityWithRole::from_xmp,
            3,
        );
        trip("Product", &product(), Product::to_xmp, Product::from_xmp, 4);
        trip(
            "RegistryEntry",
            &registry_entry(),
            RegistryEntry::to_xmp,
            RegistryEntry::from_xmp,
            3,
        );
        trip(
            "EmbeddedRightsExpression",
            &embedded_rights(),
            EmbeddedRightsExpression::to_xmp,
            EmbeddedRightsExpression::from_xmp,
            3,
        );
        trip(
            "LinkedRightsExpression",
            &linked_rights(),
            LinkedRightsExpression::to_xmp,
            LinkedRightsExpression::from_xmp,
            3,
        );
        trip(
            "CopyrightOwner",
            &copyright_owner("Owner"),
            CopyrightOwner::to_xmp,
            CopyrightOwner::from_xmp,
            2,
        );
        trip(
            "ImageCreator",
            &image_creator("Creator"),
            ImageCreator::to_xmp,
            ImageCreator::from_xmp,
            2,
        );
        trip(
            "ImageSupplier",
            &image_supplier("Supplier"),
            ImageSupplier::to_xmp,
            ImageSupplier::from_xmp,
            2,
        );
    }

    #[test]
    fn a_location_writes_its_gps_fields_in_the_exif_namespace_as_text() {
        // The reference puts the four GPS fields under `exif:`, and their XMP types are EXIF text
        // forms (GPSCoordinate, Rational, Integer) — so they are written exactly as held.
        let value = location().to_xmp();
        let fields = structure(&value).unwrap();
        for (name, text) in [
            ("GPSLatitude", "45,45.47N"),
            ("GPSLongitude", "4,49.92E"),
            ("GPSAltitude", "170/1"),
            ("GPSAltitudeRef", "0"),
        ] {
            assert_eq!(
                field(fields, WellKnownNs::Exif.uri(), name).map(|p| &p.value),
                Some(&text_value(text)),
                "exif:{name}"
            );
        }
        // Every other field is IPTC Extension.
        assert_eq!(
            fields
                .iter()
                .filter(|p| p.namespace == WellKnownNs::Iptc4XmpExt.uri())
                .count(),
            8
        );
    }

    #[test]
    fn the_added_lists_and_language_alternatives_use_the_standards_container_kinds() {
        // Every multi-valued field the eleven structures define is a `Bag` in the standard's XMP
        // Specs rows, and every `AltLang` field a language alternative.
        let bags: [(&str, XmpValue, &str, &str); 6] = [
            (
                "Location",
                location().to_xmp(),
                WellKnownNs::Iptc4XmpExt.uri(),
                "LocationId",
            ),
            (
                "Person",
                person().to_xmp(),
                WellKnownNs::Iptc4XmpExt.uri(),
                "PersonId",
            ),
            (
                "Person",
                person().to_xmp(),
                WellKnownNs::Iptc4XmpExt.uri(),
                "PersonCharacteristic",
            ),
            (
                "EntityWithRole",
                contributor().to_xmp(),
                WellKnownNs::Xmp.uri(),
                "Identifier",
            ),
            (
                "EntityWithRole",
                contributor().to_xmp(),
                WellKnownNs::Iptc4XmpExt.uri(),
                "Role",
            ),
            (
                "Product",
                product().to_xmp(),
                WellKnownNs::Iptc4XmpExt.uri(),
                "ProductId",
            ),
        ];
        for (label, value, namespace, name) in &bags {
            assert!(
                matches!(
                    field(structure(value).unwrap(), namespace, name)
                        .unwrap()
                        .value,
                    XmpValue::Array(XmpArray::Bag(_))
                ),
                "{label}: {name} must be a Bag"
            );
        }
        let alternatives: [(&str, XmpValue, &str); 7] = [
            ("Location", location().to_xmp(), "LocationName"),
            ("CvTerm", cv_term("outdoors").to_xmp(), "CvTermName"),
            ("Person", person().to_xmp(), "PersonName"),
            ("Person", person().to_xmp(), "PersonDescription"),
            ("EntityWithRole", contributor().to_xmp(), "Name"),
            ("Product", product().to_xmp(), "ProductName"),
            ("Product", product().to_xmp(), "ProductDescription"),
        ];
        for (label, value, name) in &alternatives {
            assert!(
                matches!(
                    field(
                        structure(value).unwrap(),
                        WellKnownNs::Iptc4XmpExt.uri(),
                        name
                    )
                    .unwrap()
                    .value,
                    XmpValue::Array(XmpArray::Alt(_))
                ),
                "{label}: {name} must be a language alternative"
            );
        }
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
            field(fields, WellKnownNs::Iptc4XmpExt.uri(), "AOCreator")
                .unwrap()
                .value,
            XmpValue::Array(XmpArray::Seq(_))
        ));
        assert!(matches!(
            field(fields, WellKnownNs::Iptc4XmpExt.uri(), "AOCreatorId")
                .unwrap()
                .value,
            XmpValue::Array(XmpArray::Seq(_))
        ));
        assert!(matches!(
            field(fields, WellKnownNs::Iptc4XmpExt.uri(), "AOStylePeriod")
                .unwrap()
                .value,
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
                    field(fields, WellKnownNs::Iptc4XmpExt.uri(), name)
                        .unwrap()
                        .value,
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
        assert!(
            fields
                .iter()
                .all(|p| p.namespace == WellKnownNs::Plus.uri())
        );
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
                WellKnownNs::Iptc4XmpExt.uri(),
                "PersonInImage",
                XmpValue::Array(XmpArray::Bag(vec![XmpItem::simple("Ada")])),
            )],
        };
        let value = region.to_xmp();
        assert_eq!(ImageRegion::from_xmp(&value), Some(region));

        let fields = structure(&value).unwrap();
        assert_eq!(fields.len(), 6);
        assert!(field(fields, WellKnownNs::Iptc4XmpExt.uri(), "PersonInImage").is_some());
        // The two Entity arrays are unordered bags; the boundary is a plain structure.
        for name in ["rCtype", "rRole"] {
            assert!(matches!(
                field(fields, WellKnownNs::Iptc4XmpExt.uri(), name)
                    .unwrap()
                    .value,
                XmpValue::Array(XmpArray::Bag(_))
            ));
        }
        assert!(matches!(
            field(fields, WellKnownNs::Iptc4XmpExt.uri(), "RegionBoundary")
                .unwrap()
                .value,
            XmpValue::Structured(_)
        ));
        // The entity identifier is `xmp:Identifier`, not an IPTC-namespaced property.
        let entity = structures(
            &field(fields, WellKnownNs::Iptc4XmpExt.uri(), "rCtype")
                .unwrap()
                .value,
        )[0];
        assert!(field(entity, WellKnownNs::Xmp.uri(), "Identifier").is_some());
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
            field(fields, WellKnownNs::Iptc4XmpExt.uri(), "rbVertices")
                .unwrap()
                .value,
            XmpValue::Array(XmpArray::Seq(_))
        ));
    }

    #[test]
    fn a_coordinate_that_is_not_a_number_reads_as_absent() {
        // Honest read: the typed view does not invent a number it cannot parse. Where the raw
        // value goes instead is `a_field_the_model_cannot_read_is_kept_verbatim`.
        let value = XmpValue::Structured(vec![
            XmpProperty::new(WellKnownNs::Iptc4XmpExt.uri(), "rbX", text_value("halfway")),
            XmpProperty::new(WellKnownNs::Iptc4XmpExt.uri(), "rbY", text_value(" 4.5 ")),
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
            WellKnownNs::Iptc4XmpExt.uri(),
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
                WellKnownNs::Iptc4XmpExt.uri(),
                "AOTitle",
                lang_alt_value("Sunflowers"),
            )])
        );
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
        let trips: [Trip; 18] = [
            ("Location", location().to_xmp(), |v| {
                Some(Location::from_xmp(v)?.to_xmp())
            }),
            ("CvTerm", cv_term("outdoors").to_xmp(), |v| {
                Some(CvTerm::from_xmp(v)?.to_xmp())
            }),
            ("Person", person().to_xmp(), |v| {
                Some(Person::from_xmp(v)?.to_xmp())
            }),
            ("EntityWithRole", contributor().to_xmp(), |v| {
                Some(EntityWithRole::from_xmp(v)?.to_xmp())
            }),
            ("Product", product().to_xmp(), |v| {
                Some(Product::from_xmp(v)?.to_xmp())
            }),
            ("RegistryEntry", registry_entry().to_xmp(), |v| {
                Some(RegistryEntry::from_xmp(v)?.to_xmp())
            }),
            (
                "EmbeddedRightsExpression",
                embedded_rights().to_xmp(),
                |v| Some(EmbeddedRightsExpression::from_xmp(v)?.to_xmp()),
            ),
            ("LinkedRightsExpression", linked_rights().to_xmp(), |v| {
                Some(LinkedRightsExpression::from_xmp(v)?.to_xmp())
            }),
            ("CopyrightOwner", copyright_owner("Owner").to_xmp(), |v| {
                Some(CopyrightOwner::from_xmp(v)?.to_xmp())
            }),
            ("ImageCreator", image_creator("Creator").to_xmp(), |v| {
                Some(ImageCreator::from_xmp(v)?.to_xmp())
            }),
            ("ImageSupplier", image_supplier("Supplier").to_xmp(), |v| {
                Some(ImageSupplier::from_xmp(v)?.to_xmp())
            }),
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
            other: vec![XmpProperty::new(
                WellKnownNs::Iptc4XmpExt.uri(),
                "rId",
                text_value("r2"),
            )],
            ..ImageRegion::default()
        };
        let value = region.to_xmp();
        let fields = structure(&value).unwrap();
        assert_eq!(fields.iter().filter(|p| p.name == "rId").count(), 1);
        // The modelled value is the authority, and the output is a fixed point.
        assert_eq!(
            Reader::new(fields).text(WellKnownNs::Iptc4XmpExt.uri(), "rId"),
            Some("r1".to_owned())
        );
        assert_eq!(
            ImageRegion::from_xmp(&value).map(|r| r.to_xmp()),
            Some(value.clone())
        );
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
            r.text(WellKnownNs::Iptc4XmpCore.uri(), "CiUrlWork");
        };
        let text_parses: fn(&XmpValue) -> bool = |v| parse_text(v).is_some();
        let lang_alt: fn(&mut Reader<'_>) = |r| {
            r.lang_alt(WellKnownNs::Iptc4XmpExt.uri(), "AOTitle");
        };
        let lang_alt_parses: fn(&XmpValue) -> bool = |v| parse_lang_alt(v).is_some();
        let bag: fn(&mut Reader<'_>) = |r| {
            r.list(WellKnownNs::Iptc4XmpExt.uri(), "AOStylePeriod", false);
        };
        let seq: fn(&mut Reader<'_>) = |r| {
            r.list(WellKnownNs::Iptc4XmpExt.uri(), "AOCreator", true);
        };
        let list_parses: fn(&XmpValue) -> bool = |v| parse_list(v).is_some();
        let number: fn(&mut Reader<'_>) = |r| {
            r.number(WellKnownNs::Iptc4XmpExt.uri(), "rbX");
        };
        let number_parses: fn(&XmpValue) -> bool = |v| parse_number(v).is_some();
        let nested: fn(&mut Reader<'_>) = |r| {
            r.nested(
                WellKnownNs::Iptc4XmpExt.uri(),
                "RegionBoundary",
                RegionBoundary::from_fields,
                RegionBoundary::to_xmp,
            );
        };
        let nested_parses: fn(&XmpValue) -> bool = |v| structure(v).is_some();
        let nested_bag: fn(&mut Reader<'_>) = |r| {
            r.nested_array(
                WellKnownNs::Iptc4XmpExt.uri(),
                "rCtype",
                false,
                Entity::from_fields,
                Entity::to_xmp,
            );
        };
        let nested_seq: fn(&mut Reader<'_>) = |r| {
            r.nested_array(
                WellKnownNs::Iptc4XmpExt.uri(),
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
        let ext = |name, value| XmpProperty::new(WellKnownNs::Iptc4XmpExt.uri(), name, value);
        vec![
            // --- the canonical form of each field kind: read, and written back unchanged --------
            shape(
                "text: element text",
                XmpProperty::new(
                    WellKnownNs::Iptc4XmpCore.uri(),
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
                    WellKnownNs::Iptc4XmpCore.uri(),
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
                        WellKnownNs::Iptc4XmpCore.uri(),
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
                "number: exponent notation, which underflows to zero",
                ext("rbX", text_value("1e-400")),
                number,
                number_parses,
                point,
            ),
            shape(
                "number: more significant digits than a double keeps",
                ext("rbX", text_value("12345678901234567891")),
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
                "number: exponent notation, which underflows to zero",
                "number: more significant digits than a double keeps",
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
                field(fields, WellKnownNs::Iptc4XmpExt.uri(), "rbX").is_none(),
                "{bad} was written to the graph"
            );
            // The finite sibling is still written, so the skip is per value, not per structure.
            assert_eq!(
                Reader::new(fields).number(WellKnownNs::Iptc4XmpExt.uri(), "rbY"),
                Some(1.5)
            );
        }
        // The same values arriving *from the graph* must not be destroyed either: the read parses
        // them, so a rule that consumed whatever it could read would drop them on the way out.
        for stated in ["NaN", "inf", "-inf", "1e400"] {
            let input = XmpValue::Structured(vec![XmpProperty::new(
                WellKnownNs::Iptc4XmpExt.uri(),
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
            WellKnownNs::Iptc4XmpExt.uri(),
            "rbX",
            text_value(" 0.50 "),
        )]);
        let point = RegionBoundaryPoint::from_xmp(&input).expect("a structure value");
        assert_eq!(point.x, Some(0.5));
        let written = point.to_xmp();
        assert_eq!(
            written,
            XmpValue::Structured(vec![XmpProperty::new(
                WellKnownNs::Iptc4XmpExt.uri(),
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
            XmpProperty::new(WellKnownNs::Iptc4XmpExt.uri(), "rId", text_value("r1")),
            XmpProperty::new(WellKnownNs::Iptc4XmpExt.uri(), "rId", text_value("r2")),
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
            WellKnownNs::Iptc4XmpExt.uri(),
            "Nested",
            text_value("v"),
        )]);
        let cases: [Trip; 3] = [
            // An identifier holding a structure where text belongs.
            (
                "rId",
                XmpValue::Structured(vec![XmpProperty::new(
                    WellKnownNs::Iptc4XmpExt.uri(),
                    "rId",
                    structured.clone(),
                )]),
                |v| Some(ImageRegion::from_xmp(v)?.to_xmp()),
            ),
            // A coordinate carrying non-numeric text.
            (
                "rbX",
                XmpValue::Structured(vec![XmpProperty::new(
                    WellKnownNs::Iptc4XmpExt.uri(),
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
                    WellKnownNs::Iptc4XmpExt.uri(),
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
            WellKnownNs::Iptc4XmpExt.uri(),
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
            WellKnownNs::Iptc4XmpExt.uri(),
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
            other: vec![XmpProperty::new(
                WellKnownNs::Iptc4XmpExt.uri(),
                "rId",
                text_value("r2"),
            )],
            ..ImageRegion::default()
        };
        let value = region.to_xmp();
        let fields = structure(&value).expect("a structure value");
        assert_eq!(
            Reader::new(fields).text(WellKnownNs::Iptc4XmpExt.uri(), "rId"),
            Some("r2".to_owned())
        );
    }
}
