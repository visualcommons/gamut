//! The [`PhotoMetadata`] accessor pairs for the structured IPTC properties.
//!
//! Each pair projects one top-level XMP property onto a typed model from
//! [`extension`](crate::extension), under the same lossless-projection rule the models follow
//! inside a structure, one level up (see the [`extension`](crate::extension) module docs).

use std::borrow::Borrow;

use gamut_xmp::{WellKnownNs, XmpArray, XmpItem, XmpMeta, XmpProperty, XmpValue};

use super::PhotoMetadata;
use crate::extension::fields::{reproduces, structure, structures};
use crate::extension::{
    ArtworkOrObject, CopyrightOwner, CreatorContactInfo, CvTerm, EmbeddedRightsExpression,
    EntityWithRole, ImageCreator, ImageRegion, ImageSupplier, Licensor, LinkedRightsExpression,
    Location, Person, Product, RegistryEntry,
};

/// The RDF container an array setter puts its items in (XMP Part 1 §6.3.4).
type Container = fn(Vec<XmpItem>) -> XmpArray;

/// The container kind a property's value is already held in — what an array setter keeps, so that
/// one legal container kind is never rewritten to another. A property that is not an array, or no
/// property at all, gives `standard`: the kind the standard specifies for that property — an
/// `rdf:Seq` for the three ordered PLUS party lists, an `rdf:Bag` for every other one.
fn container_of(value: Option<&XmpValue>, standard: Container) -> Container {
    match value {
        Some(XmpValue::Array(XmpArray::Bag(_))) => XmpArray::Bag,
        Some(XmpValue::Array(XmpArray::Seq(_))) => XmpArray::Seq,
        Some(XmpValue::Array(XmpArray::Alt(_))) => XmpArray::Alt,
        _ => standard,
    }
}

/// One top-level property the typed view projects, and the rule that keeps that projection
/// lossless.
///
/// This is [`Reader`](crate::extension::fields::Reader) one level up. Inside a structure the
/// retention list is the type's `other` field; at the top level it is the graph itself, so *retained* here means the accessor pair
/// leaves the property exactly where it found it. The gate is the same one: the read reports a
/// value only when the property the setter will emit for it [reproduces] the property
/// that was read.
///
/// `R` is what the read yields and `W` what the setter takes — `Vec<T>` and `[T]` for a property
/// holding an array of structures, one type for a property holding a single structure.
struct Projection<R, W: ?Sized> {
    /// The property's namespace URI.
    ns: &'static str,
    /// The property's local name.
    name: &'static str,
    /// Reads the typed value out of the property's value, or `None` when there is none to read.
    parse: fn(&XmpValue) -> Option<R>,
    /// The value the setter emits for a typed value, in the container kind the property already
    /// carries, or `None` when there is nothing to write.
    emit: fn(&W, Container) -> Option<XmpValue>,
    /// The container kind the standard specifies for the property (IPTC Photo Metadata 2025.1, its
    /// *XMP Specs* row), written when the property it replaces carries none.
    standard: Container,
}

impl<R: Borrow<W>, W: ?Sized> Projection<R, W> {
    /// The typed value of the property, or `None` when it is absent, holds nothing the model
    /// reads, or could not be written back as it stands.
    ///
    /// Two top-level properties of one name are never read: only one of them could be written
    /// back, which is why [`Reader::read`](crate::extension::fields::Reader::read) refuses the same
    /// shape inside a structure.
    fn read(&self, xmp: &XmpMeta) -> Option<R> {
        let mut matches = xmp
            .properties
            .iter()
            .filter(|property| self.names(property));
        let property = matches.next()?;
        if matches.next().is_some() {
            return None;
        }
        let value = (self.parse)(&property.value)?;
        let emitted = XmpProperty::new(
            self.ns,
            self.name,
            (self.emit)(
                value.borrow(),
                container_of(Some(&property.value), self.standard),
            )?,
        );
        reproduces(property, &emitted).then_some(value)
    }

    /// Replaces the property with `value`, keeping the RDF container kind it already carries.
    ///
    /// Replacing is total: where the graph holds two or more properties of this name, the value
    /// takes the first one's place and the others are removed, so that what was set is what
    /// [`read`](Self::read) reports next. [`XmpMeta::set`] replaces only the first, which would
    /// leave the graph holding two of one name — a shape the read refuses — and the value just set
    /// unreadable. The container kind kept is the first one's.
    ///
    /// Writing nothing removes the property — but only when [`read`](Self::read) reported one. A
    /// property the typed view does not report is one the caller was never shown, so the pair does
    /// not destroy it.
    fn write(&self, xmp: &mut XmpMeta, value: &W) {
        let container = container_of(
            xmp.get(self.ns, self.name).map(|property| &property.value),
            self.standard,
        );
        match (self.emit)(value, container) {
            Some(value) => self.replace(xmp, XmpProperty::new(self.ns, self.name, value)),
            None if self.read(xmp).is_some() => drop(xmp.remove(self.ns, self.name)),
            None => {}
        }
    }

    /// Puts `property` in place of the first property of this name, removing every later one, or
    /// appends it where there is none.
    fn replace(&self, xmp: &mut XmpMeta, property: XmpProperty) {
        let Some(first) = xmp.properties.iter().position(|p| self.names(p)) else {
            xmp.properties.push(property);
            return;
        };
        let later = xmp.properties.split_off(first + 1);
        xmp.properties[first] = property;
        xmp.properties
            .extend(later.into_iter().filter(|p| !self.names(p)));
    }

    /// Whether `property` is this projection's property.
    fn names(&self, property: &XmpProperty) -> bool {
        property.namespace == self.ns && property.name == self.name
    }
}

/// Every structure an array property holds, which is none at all for a value holding none.
///
/// Never `None`: whether there is anything here to write is [`emit_structures`]'s decision, taken
/// once. A read that finds no structure emits nothing, and a read that emits nothing reports
/// nothing (see [`Projection::read`]).
fn parse_structures<T>(value: &XmpValue, parse: fn(&[XmpProperty]) -> T) -> Option<Vec<T>> {
    Some(structures(value).into_iter().map(parse).collect())
}

/// The array value an array setter emits for `values`, or `None` when there is nothing to write:
/// an empty array says nothing a missing property does not.
fn emit_structures<T>(
    values: &[T],
    container: Container,
    to_xmp: fn(&T) -> XmpValue,
) -> Option<XmpValue> {
    (!values.is_empty()).then(|| {
        XmpValue::Array(container(
            values
                .iter()
                .map(|value| XmpItem::new(to_xmp(value)))
                .collect(),
        ))
    })
}

/// The `Iptc4xmpCore:CreatorContactInfo` projection: a single structure, so no container applies.
const CONTACT_INFO: Projection<CreatorContactInfo, CreatorContactInfo> = Projection {
    ns: WellKnownNs::Iptc4XmpCore.uri(),
    name: "CreatorContactInfo",
    parse: |value| structure(value).map(CreatorContactInfo::from_fields),
    emit: |info, _| {
        let value = info.to_xmp();
        (!structure(&value).is_some_and(<[XmpProperty]>::is_empty)).then_some(value)
    },
    standard: XmpArray::Bag,
};

/// The `Iptc4xmpExt:ImageRegion` projection.
const IMAGE_REGIONS: Projection<Vec<ImageRegion>, [ImageRegion]> = Projection {
    ns: WellKnownNs::Iptc4XmpExt.uri(),
    name: "ImageRegion",
    parse: |value| parse_structures(value, ImageRegion::from_fields),
    emit: |values, container| emit_structures(values, container, ImageRegion::to_xmp),
    standard: XmpArray::Bag,
};

/// The `Iptc4xmpExt:ArtworkOrObject` projection.
const ARTWORK_OR_OBJECTS: Projection<Vec<ArtworkOrObject>, [ArtworkOrObject]> = Projection {
    ns: WellKnownNs::Iptc4XmpExt.uri(),
    name: "ArtworkOrObject",
    parse: |value| parse_structures(value, ArtworkOrObject::from_fields),
    emit: |values, container| emit_structures(values, container, ArtworkOrObject::to_xmp),
    standard: XmpArray::Bag,
};

/// The `plus:Licensor` projection.
const LICENSORS: Projection<Vec<Licensor>, [Licensor]> = Projection {
    ns: WellKnownNs::Plus.uri(),
    name: "Licensor",
    parse: |value| parse_structures(value, Licensor::from_fields),
    emit: |values, container| emit_structures(values, container, Licensor::to_xmp),
    standard: XmpArray::Bag,
};

/// The `Iptc4xmpExt:LocationCreated` projection.
const LOCATIONS_CREATED: Projection<Vec<Location>, [Location]> = Projection {
    ns: WellKnownNs::Iptc4XmpExt.uri(),
    name: "LocationCreated",
    parse: |value| parse_structures(value, Location::from_fields),
    emit: |values, container| emit_structures(values, container, Location::to_xmp),
    standard: XmpArray::Bag,
};

/// The `Iptc4xmpExt:LocationShown` projection.
const LOCATIONS_SHOWN: Projection<Vec<Location>, [Location]> = Projection {
    ns: WellKnownNs::Iptc4XmpExt.uri(),
    name: "LocationShown",
    parse: |value| parse_structures(value, Location::from_fields),
    emit: |values, container| emit_structures(values, container, Location::to_xmp),
    standard: XmpArray::Bag,
};

/// The `Iptc4xmpExt:PersonInImageWDetails` projection.
const PERSONS_SHOWN: Projection<Vec<Person>, [Person]> = Projection {
    ns: WellKnownNs::Iptc4XmpExt.uri(),
    name: "PersonInImageWDetails",
    parse: |value| parse_structures(value, Person::from_fields),
    emit: |values, container| emit_structures(values, container, Person::to_xmp),
    standard: XmpArray::Bag,
};

/// The `Iptc4xmpExt:AboutCvTerm` projection.
const ABOUT_CV_TERMS: Projection<Vec<CvTerm>, [CvTerm]> = Projection {
    ns: WellKnownNs::Iptc4XmpExt.uri(),
    name: "AboutCvTerm",
    parse: |value| parse_structures(value, CvTerm::from_fields),
    emit: |values, container| emit_structures(values, container, CvTerm::to_xmp),
    standard: XmpArray::Bag,
};

/// The `Iptc4xmpExt:Genre` projection.
const GENRES: Projection<Vec<CvTerm>, [CvTerm]> = Projection {
    ns: WellKnownNs::Iptc4XmpExt.uri(),
    name: "Genre",
    parse: |value| parse_structures(value, CvTerm::from_fields),
    emit: |values, container| emit_structures(values, container, CvTerm::to_xmp),
    standard: XmpArray::Bag,
};

/// The `Iptc4xmpExt:Contributor` projection.
const CONTRIBUTORS: Projection<Vec<EntityWithRole>, [EntityWithRole]> = Projection {
    ns: WellKnownNs::Iptc4XmpExt.uri(),
    name: "Contributor",
    parse: |value| parse_structures(value, EntityWithRole::from_fields),
    emit: |values, container| emit_structures(values, container, EntityWithRole::to_xmp),
    standard: XmpArray::Bag,
};

/// The `Iptc4xmpExt:ProductInImage` projection.
const PRODUCTS_SHOWN: Projection<Vec<Product>, [Product]> = Projection {
    ns: WellKnownNs::Iptc4XmpExt.uri(),
    name: "ProductInImage",
    parse: |value| parse_structures(value, Product::from_fields),
    emit: |values, container| emit_structures(values, container, Product::to_xmp),
    standard: XmpArray::Bag,
};

/// The `Iptc4xmpExt:RegistryId` projection.
const REGISTRY_ENTRIES: Projection<Vec<RegistryEntry>, [RegistryEntry]> = Projection {
    ns: WellKnownNs::Iptc4XmpExt.uri(),
    name: "RegistryId",
    parse: |value| parse_structures(value, RegistryEntry::from_fields),
    emit: |values, container| emit_structures(values, container, RegistryEntry::to_xmp),
    standard: XmpArray::Bag,
};

/// The `Iptc4xmpExt:EmbdEncRightsExpr` projection.
const EMBEDDED_RIGHTS_EXPRESSIONS: Projection<
    Vec<EmbeddedRightsExpression>,
    [EmbeddedRightsExpression],
> = Projection {
    ns: WellKnownNs::Iptc4XmpExt.uri(),
    name: "EmbdEncRightsExpr",
    parse: |value| parse_structures(value, EmbeddedRightsExpression::from_fields),
    emit: |values, container| emit_structures(values, container, EmbeddedRightsExpression::to_xmp),
    standard: XmpArray::Bag,
};

/// The `Iptc4xmpExt:LinkedEncRightsExpr` projection.
const LINKED_RIGHTS_EXPRESSIONS: Projection<Vec<LinkedRightsExpression>, [LinkedRightsExpression]> =
    Projection {
        ns: WellKnownNs::Iptc4XmpExt.uri(),
        name: "LinkedEncRightsExpr",
        parse: |value| parse_structures(value, LinkedRightsExpression::from_fields),
        emit: |values, container| {
            emit_structures(values, container, LinkedRightsExpression::to_xmp)
        },
        standard: XmpArray::Bag,
    };

/// The `plus:CopyrightOwner` projection: an ordered `rdf:Seq`.
const COPYRIGHT_OWNERS: Projection<Vec<CopyrightOwner>, [CopyrightOwner]> = Projection {
    ns: WellKnownNs::Plus.uri(),
    name: "CopyrightOwner",
    parse: |value| parse_structures(value, CopyrightOwner::from_fields),
    emit: |values, container| emit_structures(values, container, CopyrightOwner::to_xmp),
    standard: XmpArray::Seq,
};

/// The `plus:ImageCreator` projection: an ordered `rdf:Seq`.
const IMAGE_CREATORS: Projection<Vec<ImageCreator>, [ImageCreator]> = Projection {
    ns: WellKnownNs::Plus.uri(),
    name: "ImageCreator",
    parse: |value| parse_structures(value, ImageCreator::from_fields),
    emit: |values, container| emit_structures(values, container, ImageCreator::to_xmp),
    standard: XmpArray::Seq,
};

/// The `plus:ImageSupplier` projection: an ordered `rdf:Seq`.
const IMAGE_SUPPLIERS: Projection<Vec<ImageSupplier>, [ImageSupplier]> = Projection {
    ns: WellKnownNs::Plus.uri(),
    name: "ImageSupplier",
    parse: |value| parse_structures(value, ImageSupplier::from_fields),
    emit: |values, container| emit_structures(values, container, ImageSupplier::to_xmp),
    standard: XmpArray::Seq,
};

impl PhotoMetadata {
    /// The creator's contact details (`Iptc4xmpCore:CreatorContactInfo`), or `None` when the
    /// property is absent, holds no structure, or could not be written back as it stands (see the
    /// [module docs](crate::extension)).
    #[must_use]
    pub fn creator_contact_info(&self) -> Option<CreatorContactInfo> {
        CONTACT_INFO.read(&self.xmp)
    }

    /// Sets the creator's contact details (`Iptc4xmpCore:CreatorContactInfo`); a block with no
    /// fields at all removes the property, as an empty slice does for the array accessors — but
    /// only a property this pair reported.
    pub fn set_creator_contact_info(&mut self, info: &CreatorContactInfo) {
        CONTACT_INFO.write(&mut self.xmp, info);
    }

    /// The image regions (`Iptc4xmpExt:ImageRegion`), in the order the graph holds them, or an
    /// empty list when the property is absent, holds no structure, or could not be written back as
    /// it stands (see the [module docs](crate::extension)).
    #[must_use]
    pub fn image_regions(&self) -> Vec<ImageRegion> {
        IMAGE_REGIONS.read(&self.xmp).unwrap_or_default()
    }

    /// Sets the image regions (`Iptc4xmpExt:ImageRegion`); an existing array keeps its container
    /// kind, and an empty slice removes a property this pair reported.
    pub fn set_image_regions(&mut self, regions: &[ImageRegion]) {
        IMAGE_REGIONS.write(&mut self.xmp, regions);
    }

    /// The artworks or objects shown in the image (`Iptc4xmpExt:ArtworkOrObject`), on the same
    /// terms as [`image_regions`](Self::image_regions).
    #[must_use]
    pub fn artwork_or_objects(&self) -> Vec<ArtworkOrObject> {
        ARTWORK_OR_OBJECTS.read(&self.xmp).unwrap_or_default()
    }

    /// Sets the artworks or objects shown in the image (`Iptc4xmpExt:ArtworkOrObject`); an existing
    /// array keeps its container kind, and an empty slice removes a property this pair reported.
    pub fn set_artwork_or_objects(&mut self, artworks: &[ArtworkOrObject]) {
        ARTWORK_OR_OBJECTS.write(&mut self.xmp, artworks);
    }

    /// The licensors of the image (`plus:Licensor`), on the same terms as
    /// [`image_regions`](Self::image_regions).
    #[must_use]
    pub fn licensors(&self) -> Vec<Licensor> {
        LICENSORS.read(&self.xmp).unwrap_or_default()
    }

    /// Sets the licensors of the image (`plus:Licensor`); an existing array keeps its container
    /// kind, and an empty slice removes a property this pair reported.
    pub fn set_licensors(&mut self, licensors: &[Licensor]) {
        LICENSORS.write(&mut self.xmp, licensors);
    }

    /// The locations the image was created at (`Iptc4xmpExt:LocationCreated`), on the same terms
    /// as [`image_regions`](Self::image_regions).
    ///
    /// The standard caps the property at one location while its XMP form is a `Bag`; gamut reads
    /// every location the array holds.
    #[must_use]
    pub fn locations_created(&self) -> Vec<Location> {
        LOCATIONS_CREATED.read(&self.xmp).unwrap_or_default()
    }

    /// Sets the locations the image was created at (`Iptc4xmpExt:LocationCreated`); an existing
    /// array keeps its container kind, and an empty slice removes a property this pair reported.
    pub fn set_locations_created(&mut self, locations: &[Location]) {
        LOCATIONS_CREATED.write(&mut self.xmp, locations);
    }

    /// The locations shown in the image (`Iptc4xmpExt:LocationShown`), on the same terms as
    /// [`image_regions`](Self::image_regions).
    #[must_use]
    pub fn locations_shown(&self) -> Vec<Location> {
        LOCATIONS_SHOWN.read(&self.xmp).unwrap_or_default()
    }

    /// Sets the locations shown in the image (`Iptc4xmpExt:LocationShown`); an existing array
    /// keeps its container kind, and an empty slice removes a property this pair reported.
    pub fn set_locations_shown(&mut self, locations: &[Location]) {
        LOCATIONS_SHOWN.write(&mut self.xmp, locations);
    }

    /// The persons shown in the image, with details (`Iptc4xmpExt:PersonInImageWDetails`), on the
    /// same terms as [`image_regions`](Self::image_regions).
    #[must_use]
    pub fn persons_shown(&self) -> Vec<Person> {
        PERSONS_SHOWN.read(&self.xmp).unwrap_or_default()
    }

    /// Sets the persons shown in the image (`Iptc4xmpExt:PersonInImageWDetails`); an existing
    /// array keeps its container kind, and an empty slice removes a property this pair reported.
    pub fn set_persons_shown(&mut self, persons: &[Person]) {
        PERSONS_SHOWN.write(&mut self.xmp, persons);
    }

    /// The controlled-vocabulary terms the image is about (`Iptc4xmpExt:AboutCvTerm`), on the same
    /// terms as [`image_regions`](Self::image_regions).
    #[must_use]
    pub fn about_cv_terms(&self) -> Vec<CvTerm> {
        ABOUT_CV_TERMS.read(&self.xmp).unwrap_or_default()
    }

    /// Sets the controlled-vocabulary terms the image is about (`Iptc4xmpExt:AboutCvTerm`); an
    /// existing array keeps its container kind, and an empty slice removes a property this pair
    /// reported.
    pub fn set_about_cv_terms(&mut self, terms: &[CvTerm]) {
        ABOUT_CV_TERMS.write(&mut self.xmp, terms);
    }

    /// The genres of the image, as controlled-vocabulary terms (`Iptc4xmpExt:Genre`), on the same
    /// terms as [`image_regions`](Self::image_regions).
    ///
    /// Distinct from the Core's flat [`intellectual_genre`](Self::intellectual_genre)
    /// (`Iptc4xmpCore:IntellectualGenre`); gamut does not copy one into the other.
    #[must_use]
    pub fn genres(&self) -> Vec<CvTerm> {
        GENRES.read(&self.xmp).unwrap_or_default()
    }

    /// Sets the genres of the image (`Iptc4xmpExt:Genre`); an existing array keeps its container
    /// kind, and an empty slice removes a property this pair reported.
    pub fn set_genres(&mut self, genres: &[CvTerm]) {
        GENRES.write(&mut self.xmp, genres);
    }

    /// The contributors to the image, with their roles (`Iptc4xmpExt:Contributor`), on the same
    /// terms as [`image_regions`](Self::image_regions).
    #[must_use]
    pub fn contributors(&self) -> Vec<EntityWithRole> {
        CONTRIBUTORS.read(&self.xmp).unwrap_or_default()
    }

    /// Sets the contributors to the image (`Iptc4xmpExt:Contributor`); an existing array keeps its
    /// container kind, and an empty slice removes a property this pair reported.
    pub fn set_contributors(&mut self, contributors: &[EntityWithRole]) {
        CONTRIBUTORS.write(&mut self.xmp, contributors);
    }

    /// The products shown in the image (`Iptc4xmpExt:ProductInImage`), on the same terms as
    /// [`image_regions`](Self::image_regions).
    #[must_use]
    pub fn products_shown(&self) -> Vec<Product> {
        PRODUCTS_SHOWN.read(&self.xmp).unwrap_or_default()
    }

    /// Sets the products shown in the image (`Iptc4xmpExt:ProductInImage`); an existing array
    /// keeps its container kind, and an empty slice removes a property this pair reported.
    pub fn set_products_shown(&mut self, products: &[Product]) {
        PRODUCTS_SHOWN.write(&mut self.xmp, products);
    }

    /// The image's registry entries (`Iptc4xmpExt:RegistryId`), on the same terms as
    /// [`image_regions`](Self::image_regions).
    #[must_use]
    pub fn registry_entries(&self) -> Vec<RegistryEntry> {
        REGISTRY_ENTRIES.read(&self.xmp).unwrap_or_default()
    }

    /// Sets the image's registry entries (`Iptc4xmpExt:RegistryId`); an existing array keeps its
    /// container kind, and an empty slice removes a property this pair reported.
    pub fn set_registry_entries(&mut self, entries: &[RegistryEntry]) {
        REGISTRY_ENTRIES.write(&mut self.xmp, entries);
    }

    /// The rights expressions embedded in the metadata (`Iptc4xmpExt:EmbdEncRightsExpr`), on the
    /// same terms as [`image_regions`](Self::image_regions).
    #[must_use]
    pub fn embedded_rights_expressions(&self) -> Vec<EmbeddedRightsExpression> {
        EMBEDDED_RIGHTS_EXPRESSIONS
            .read(&self.xmp)
            .unwrap_or_default()
    }

    /// Sets the rights expressions embedded in the metadata (`Iptc4xmpExt:EmbdEncRightsExpr`); an
    /// existing array keeps its container kind, and an empty slice removes a property this pair
    /// reported.
    pub fn set_embedded_rights_expressions(&mut self, expressions: &[EmbeddedRightsExpression]) {
        EMBEDDED_RIGHTS_EXPRESSIONS.write(&mut self.xmp, expressions);
    }

    /// The rights expressions the metadata links to (`Iptc4xmpExt:LinkedEncRightsExpr`), on the
    /// same terms as [`image_regions`](Self::image_regions).
    #[must_use]
    pub fn linked_rights_expressions(&self) -> Vec<LinkedRightsExpression> {
        LINKED_RIGHTS_EXPRESSIONS
            .read(&self.xmp)
            .unwrap_or_default()
    }

    /// Sets the rights expressions the metadata links to (`Iptc4xmpExt:LinkedEncRightsExpr`); an
    /// existing array keeps its container kind, and an empty slice removes a property this pair
    /// reported.
    pub fn set_linked_rights_expressions(&mut self, expressions: &[LinkedRightsExpression]) {
        LINKED_RIGHTS_EXPRESSIONS.write(&mut self.xmp, expressions);
    }

    /// The copyright owners of the image (`plus:CopyrightOwner`), in order, on the same terms as
    /// [`image_regions`](Self::image_regions).
    #[must_use]
    pub fn copyright_owners(&self) -> Vec<CopyrightOwner> {
        COPYRIGHT_OWNERS.read(&self.xmp).unwrap_or_default()
    }

    /// Sets the copyright owners of the image (`plus:CopyrightOwner`); an existing array keeps its
    /// container kind, a new one is the `rdf:Seq` the standard specifies, and an empty slice
    /// removes a property this pair reported.
    pub fn set_copyright_owners(&mut self, owners: &[CopyrightOwner]) {
        COPYRIGHT_OWNERS.write(&mut self.xmp, owners);
    }

    /// The creators of the image (`plus:ImageCreator`), in order, on the same terms as
    /// [`image_regions`](Self::image_regions).
    #[must_use]
    pub fn image_creators(&self) -> Vec<ImageCreator> {
        IMAGE_CREATORS.read(&self.xmp).unwrap_or_default()
    }

    /// Sets the creators of the image (`plus:ImageCreator`); an existing array keeps its container
    /// kind, a new one is the `rdf:Seq` the standard specifies, and an empty slice removes a
    /// property this pair reported.
    pub fn set_image_creators(&mut self, creators: &[ImageCreator]) {
        IMAGE_CREATORS.write(&mut self.xmp, creators);
    }

    /// The suppliers of the image (`plus:ImageSupplier`), in order, on the same terms as
    /// [`image_regions`](Self::image_regions).
    #[must_use]
    pub fn image_suppliers(&self) -> Vec<ImageSupplier> {
        IMAGE_SUPPLIERS.read(&self.xmp).unwrap_or_default()
    }

    /// Sets the suppliers of the image (`plus:ImageSupplier`); an existing array keeps its
    /// container kind, a new one is the `rdf:Seq` the standard specifies, and an empty slice
    /// removes a property this pair reported.
    pub fn set_image_suppliers(&mut self, suppliers: &[ImageSupplier]) {
        IMAGE_SUPPLIERS.write(&mut self.xmp, suppliers);
    }
}

#[cfg(test)]
mod tests {
    use gamut_xmp::XML_NAMESPACE;

    use super::*;
    use crate::extension::Entity;
    use crate::extension::fields::X_DEFAULT;
    use crate::extension::test_support::*;

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
            WellKnownNs::Xmp.uri(),
            "CreatorTool",
            text_value("something else"),
        ));
        let extracted = PhotoMetadata::from_xmp(&graph);
        assert_eq!(extracted.licensors().len(), 1);
        assert_eq!(extracted.xmp.properties.len(), 1);
    }
    #[test]
    fn setting_an_array_keeps_the_container_kind_the_property_already_has() {
        // Rewriting one legal container kind to another throws away what that kind states — a
        // `Seq`'s order, an `Alt`'s "these are alternatives" — and the setter has the existing
        // property in front of it. A property that is not an array becomes the standard Bag.
        let region = ImageRegion {
            identifier: Some("r1".to_owned()),
            ..ImageRegion::default()
        };
        let kinds: [fn(Vec<XmpItem>) -> XmpArray; 3] =
            [XmpArray::Bag, XmpArray::Seq, XmpArray::Alt];
        for kind in kinds {
            let mut pm = PhotoMetadata::new();
            pm.xmp.set(XmpProperty::new(
                WellKnownNs::Iptc4XmpExt.uri(),
                "ImageRegion",
                XmpValue::Array(kind(vec![XmpItem::new(region.to_xmp())])),
            ));
            pm.set_image_regions(std::slice::from_ref(&region));
            assert_eq!(
                pm.xmp
                    .get(WellKnownNs::Iptc4XmpExt.uri(), "ImageRegion")
                    .expect("the property")
                    .value,
                XmpValue::Array(kind(vec![XmpItem::new(region.to_xmp())]))
            );
        }
        // Nothing to keep: the standard Bag.
        let mut pm = PhotoMetadata::new();
        pm.set_image_regions(std::slice::from_ref(&region));
        assert!(matches!(
            pm.xmp
                .get(WellKnownNs::Iptc4XmpExt.uri(), "ImageRegion")
                .expect("the property")
                .value,
            XmpValue::Array(XmpArray::Bag(_))
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
        // Every setter agrees: nothing to say removes the property, rather than leaving behind
        // an empty structure a reader would report as "present but blank".
        let mut pm = PhotoMetadata::new();
        pm.set_creator_contact_info(&contact());
        pm.set_creator_contact_info(&CreatorContactInfo::default());
        assert_eq!(pm.creator_contact_info(), None);
        assert!(pm.xmp.properties.is_empty());
    }

    #[test]
    fn a_bare_structure_where_an_array_belongs_reads_as_absent_at_both_levels() {
        // Seen in the wild: a single structure written where the standard puts a Bag. Writing it
        // back would normalise it into a Bag, so it is not read — one rule, at both levels, and
        // the property is left where it lies either way.
        let licensor = Licensor {
            name: Some("Agence gamut".to_owned()),
            ..Licensor::default()
        };
        let mut pm = PhotoMetadata::new();
        pm.xmp.set(XmpProperty::new(
            WellKnownNs::Plus.uri(),
            "Licensor",
            licensor.to_xmp(),
        ));
        assert_eq!(pm.licensors(), Vec::new());

        // Inside a structure the same shape reads the same way: `rCtype` reads as nothing.
        let region = XmpValue::Structured(vec![XmpProperty::new(
            WellKnownNs::Iptc4XmpExt.uri(),
            "rCtype",
            Entity {
                name: Some("Human".to_owned()),
                ..Entity::default()
            }
            .to_xmp(),
        )]);
        let read = ImageRegion::from_xmp(&region).expect("a structure value");
        assert_eq!(read.content_types, Vec::new());
    }

    #[test]
    fn an_array_member_the_model_cannot_write_back_makes_the_property_read_as_absent() {
        // Hostile/odd input: a Bag holding plain text where a structure is expected. Reporting the
        // one region the model does read would invite a setter call that drops the other member,
        // so the property reads as absent instead — and it is not fatal.
        let mut pm = PhotoMetadata::new();
        pm.xmp.set(XmpProperty::new(
            WellKnownNs::Iptc4XmpExt.uri(),
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
        assert_eq!(pm.image_regions(), Vec::new());
        // A non-array property yields nothing at all rather than a bogus entry.
        pm.xmp.set(XmpProperty::new(
            WellKnownNs::Plus.uri(),
            "Licensor",
            text_value("not an array"),
        ));
        assert!(pm.licensors().is_empty());
    }

    #[test]
    fn two_top_level_properties_of_one_name_are_neither_read_nor_lost() {
        // Only one of the two could be written back, so neither is read — the rule `Reader::read`
        // already takes inside a structure — and the setter therefore has nothing to remove.
        let named = |name: &str| {
            XmpProperty::new(
                WellKnownNs::Plus.uri(),
                "Licensor",
                XmpValue::Array(XmpArray::Bag(vec![XmpItem::new(
                    Licensor {
                        name: Some(name.to_owned()),
                        ..Licensor::default()
                    }
                    .to_xmp(),
                )])),
            )
        };
        let mut pm = PhotoMetadata::new();
        pm.xmp.properties.push(named("Agence gamut"));
        pm.xmp.properties.push(named("Another agency"));
        let before = pm.xmp.clone();
        assert_eq!(pm.licensors(), Vec::new());
        pm.set_licensors(&pm.licensors());
        assert_eq!(pm.xmp, before);
    }

    #[test]
    fn a_setter_handed_values_replaces_every_top_level_property_of_its_name() {
        // `Projection::replace`: the value takes the first copy's place and the later copies go,
        // so the value set is the value read back; neighbours on either side are untouched.
        let licensor = |name: &str| Licensor {
            name: Some(name.to_owned()),
            ..Licensor::default()
        };
        let named = |name: &str| {
            XmpProperty::new(
                WellKnownNs::Plus.uri(),
                "Licensor",
                XmpValue::Array(XmpArray::Bag(vec![XmpItem::new(licensor(name).to_xmp())])),
            )
        };
        let neighbour =
            |name: &str| XmpProperty::new(WellKnownNs::Xmp.uri(), name, text_value(name));
        let mut pm = PhotoMetadata::new();
        pm.xmp.properties = vec![
            neighbour("Before"),
            named("Agence gamut"),
            neighbour("Between"),
            named("Another agency"),
            neighbour("After"),
        ];
        pm.set_licensors(&[licensor("New agency")]);
        assert_eq!(
            pm.xmp.properties,
            [
                neighbour("Before"),
                named("New agency"),
                neighbour("Between"),
                neighbour("After"),
            ]
        );
        assert_eq!(pm.licensors(), [licensor("New agency")]);
    }

    /// One top-level property the typed view projects, for the sweep below.
    struct TopProperty {
        /// The accessor pair under test, for the failure message.
        label: &'static str,
        /// The property's namespace URI.
        ns: &'static str,
        /// The property's local name.
        name: &'static str,
        /// A structure value whose every field the property's own type reads.
        member: fn() -> XmpValue,
        /// Reads the property through its accessor and writes what it read straight back.
        trip: fn(&mut PhotoMetadata),
    }

    /// Every accessor pair [`PhotoMetadata`] exposes for a structured IPTC property.
    fn top_properties() -> Vec<TopProperty> {
        vec![
            TopProperty {
                label: "creator_contact_info",
                ns: WellKnownNs::Iptc4XmpCore.uri(),
                name: "CreatorContactInfo",
                member: || contact().to_xmp(),
                trip: |pm| {
                    let info = pm.creator_contact_info().unwrap_or_default();
                    pm.set_creator_contact_info(&info);
                },
            },
            TopProperty {
                label: "image_regions",
                ns: WellKnownNs::Iptc4XmpExt.uri(),
                name: "ImageRegion",
                member: || {
                    ImageRegion {
                        identifier: Some("r1".to_owned()),
                        ..ImageRegion::default()
                    }
                    .to_xmp()
                },
                trip: |pm| pm.set_image_regions(&pm.image_regions()),
            },
            TopProperty {
                label: "artwork_or_objects",
                ns: WellKnownNs::Iptc4XmpExt.uri(),
                name: "ArtworkOrObject",
                member: || {
                    ArtworkOrObject {
                        title: Some("Sunflowers".to_owned()),
                        ..ArtworkOrObject::default()
                    }
                    .to_xmp()
                },
                trip: |pm| pm.set_artwork_or_objects(&pm.artwork_or_objects()),
            },
            TopProperty {
                label: "licensors",
                ns: WellKnownNs::Plus.uri(),
                name: "Licensor",
                member: || {
                    Licensor {
                        name: Some("Agence gamut".to_owned()),
                        ..Licensor::default()
                    }
                    .to_xmp()
                },
                trip: |pm| pm.set_licensors(&pm.licensors()),
            },
            TopProperty {
                label: "locations_created",
                ns: WellKnownNs::Iptc4XmpExt.uri(),
                name: "LocationCreated",
                member: || location().to_xmp(),
                trip: |pm| pm.set_locations_created(&pm.locations_created()),
            },
            TopProperty {
                label: "locations_shown",
                ns: WellKnownNs::Iptc4XmpExt.uri(),
                name: "LocationShown",
                member: || location().to_xmp(),
                trip: |pm| pm.set_locations_shown(&pm.locations_shown()),
            },
            TopProperty {
                label: "persons_shown",
                ns: WellKnownNs::Iptc4XmpExt.uri(),
                name: "PersonInImageWDetails",
                member: || person().to_xmp(),
                trip: |pm| pm.set_persons_shown(&pm.persons_shown()),
            },
            TopProperty {
                label: "about_cv_terms",
                ns: WellKnownNs::Iptc4XmpExt.uri(),
                name: "AboutCvTerm",
                member: || cv_term("outdoors").to_xmp(),
                trip: |pm| pm.set_about_cv_terms(&pm.about_cv_terms()),
            },
            TopProperty {
                label: "genres",
                ns: WellKnownNs::Iptc4XmpExt.uri(),
                name: "Genre",
                member: || cv_term("portrait").to_xmp(),
                trip: |pm| pm.set_genres(&pm.genres()),
            },
            TopProperty {
                label: "contributors",
                ns: WellKnownNs::Iptc4XmpExt.uri(),
                name: "Contributor",
                member: || contributor().to_xmp(),
                trip: |pm| pm.set_contributors(&pm.contributors()),
            },
            TopProperty {
                label: "products_shown",
                ns: WellKnownNs::Iptc4XmpExt.uri(),
                name: "ProductInImage",
                member: || product().to_xmp(),
                trip: |pm| pm.set_products_shown(&pm.products_shown()),
            },
            TopProperty {
                label: "registry_entries",
                ns: WellKnownNs::Iptc4XmpExt.uri(),
                name: "RegistryId",
                member: || registry_entry().to_xmp(),
                trip: |pm| pm.set_registry_entries(&pm.registry_entries()),
            },
            TopProperty {
                label: "embedded_rights_expressions",
                ns: WellKnownNs::Iptc4XmpExt.uri(),
                name: "EmbdEncRightsExpr",
                member: || embedded_rights().to_xmp(),
                trip: |pm| pm.set_embedded_rights_expressions(&pm.embedded_rights_expressions()),
            },
            TopProperty {
                label: "linked_rights_expressions",
                ns: WellKnownNs::Iptc4XmpExt.uri(),
                name: "LinkedEncRightsExpr",
                member: || linked_rights().to_xmp(),
                trip: |pm| pm.set_linked_rights_expressions(&pm.linked_rights_expressions()),
            },
            TopProperty {
                label: "copyright_owners",
                ns: WellKnownNs::Plus.uri(),
                name: "CopyrightOwner",
                member: || copyright_owner("Owner").to_xmp(),
                trip: |pm| pm.set_copyright_owners(&pm.copyright_owners()),
            },
            TopProperty {
                label: "image_creators",
                ns: WellKnownNs::Plus.uri(),
                name: "ImageCreator",
                member: || image_creator("Creator").to_xmp(),
                trip: |pm| pm.set_image_creators(&pm.image_creators()),
            },
            TopProperty {
                label: "image_suppliers",
                ns: WellKnownNs::Plus.uri(),
                name: "ImageSupplier",
                member: || image_supplier("Supplier").to_xmp(),
                trip: |pm| pm.set_image_suppliers(&pm.image_suppliers()),
            },
        ]
    }

    #[test]
    fn every_added_setter_writes_what_its_accessor_reads_back() {
        // The read-modify-write sweep below starts from a graph that already holds the property,
        // so a setter that wrote nothing would pass it. Each pair is checked here from an empty
        // graph instead: one value in, the same value read back, under the one property named.
        fn pair<T: Clone + PartialEq + std::fmt::Debug>(
            label: &str,
            values: &[T],
            set: fn(&mut PhotoMetadata, &[T]),
            get: fn(&PhotoMetadata) -> Vec<T>,
            ns: &str,
            name: &str,
        ) {
            let mut pm = PhotoMetadata::new();
            set(&mut pm, values);
            assert_eq!(get(&pm), values, "{label}");
            assert_eq!(pm.xmp.properties.len(), 1, "{label}");
            assert!(pm.xmp.get(ns, name).is_some(), "{label}: {name}");
        }
        let ext = WellKnownNs::Iptc4XmpExt.uri();
        let locations = [location(), Location::default()];
        pair(
            "locations_created",
            &locations,
            PhotoMetadata::set_locations_created,
            PhotoMetadata::locations_created,
            ext,
            "LocationCreated",
        );
        pair(
            "locations_shown",
            &locations,
            PhotoMetadata::set_locations_shown,
            PhotoMetadata::locations_shown,
            ext,
            "LocationShown",
        );
        pair(
            "persons_shown",
            &[person()],
            PhotoMetadata::set_persons_shown,
            PhotoMetadata::persons_shown,
            ext,
            "PersonInImageWDetails",
        );
        pair(
            "about_cv_terms",
            &[cv_term("outdoors")],
            PhotoMetadata::set_about_cv_terms,
            PhotoMetadata::about_cv_terms,
            ext,
            "AboutCvTerm",
        );
        pair(
            "genres",
            &[cv_term("portrait")],
            PhotoMetadata::set_genres,
            PhotoMetadata::genres,
            ext,
            "Genre",
        );
        pair(
            "contributors",
            &[contributor()],
            PhotoMetadata::set_contributors,
            PhotoMetadata::contributors,
            ext,
            "Contributor",
        );
        pair(
            "products_shown",
            &[product()],
            PhotoMetadata::set_products_shown,
            PhotoMetadata::products_shown,
            ext,
            "ProductInImage",
        );
        pair(
            "registry_entries",
            &[registry_entry()],
            PhotoMetadata::set_registry_entries,
            PhotoMetadata::registry_entries,
            ext,
            "RegistryId",
        );
        pair(
            "embedded_rights_expressions",
            &[embedded_rights()],
            PhotoMetadata::set_embedded_rights_expressions,
            PhotoMetadata::embedded_rights_expressions,
            ext,
            "EmbdEncRightsExpr",
        );
        pair(
            "linked_rights_expressions",
            &[linked_rights()],
            PhotoMetadata::set_linked_rights_expressions,
            PhotoMetadata::linked_rights_expressions,
            ext,
            "LinkedEncRightsExpr",
        );
        pair(
            "copyright_owners",
            &[copyright_owner("First"), copyright_owner("Second")],
            PhotoMetadata::set_copyright_owners,
            PhotoMetadata::copyright_owners,
            WellKnownNs::Plus.uri(),
            "CopyrightOwner",
        );
        pair(
            "image_creators",
            &[image_creator("First"), image_creator("Second")],
            PhotoMetadata::set_image_creators,
            PhotoMetadata::image_creators,
            WellKnownNs::Plus.uri(),
            "ImageCreator",
        );
        pair(
            "image_suppliers",
            &[image_supplier("First"), image_supplier("Second")],
            PhotoMetadata::set_image_suppliers,
            PhotoMetadata::image_suppliers,
            WellKnownNs::Plus.uri(),
            "ImageSupplier",
        );
    }

    #[test]
    fn a_new_plus_party_list_is_the_seq_the_standard_specifies() {
        // `plus:CopyrightOwner`, `plus:ImageCreator` and `plus:ImageSupplier` are ordered lists in
        // the standard's XMP Specs rows; every other structured array is a Bag. A property that
        // already exists keeps its own kind — including a Bag under a Seq-standard property, which
        // is what makes `container_of`'s Bag arm distinct from its default.
        let mut pm = PhotoMetadata::new();
        pm.set_copyright_owners(&[copyright_owner("Owner")]);
        pm.set_image_creators(&[image_creator("Creator")]);
        pm.set_image_suppliers(&[image_supplier("Supplier")]);
        pm.set_locations_created(&[location()]);
        let kind = container_kind;
        assert_eq!(kind(&pm, WellKnownNs::Plus.uri(), "CopyrightOwner"), "Seq");
        assert_eq!(kind(&pm, WellKnownNs::Plus.uri(), "ImageCreator"), "Seq");
        assert_eq!(kind(&pm, WellKnownNs::Plus.uri(), "ImageSupplier"), "Seq");
        assert_eq!(
            kind(&pm, WellKnownNs::Iptc4XmpExt.uri(), "LocationCreated"),
            "Bag"
        );

        let mut pm = PhotoMetadata::new();
        pm.xmp.set(XmpProperty::new(
            WellKnownNs::Plus.uri(),
            "CopyrightOwner",
            XmpValue::Array(XmpArray::Bag(vec![XmpItem::new(
                copyright_owner("Owner").to_xmp(),
            )])),
        ));
        pm.set_copyright_owners(&[copyright_owner("Another")]);
        assert_eq!(kind(&pm, WellKnownNs::Plus.uri(), "CopyrightOwner"), "Bag");
    }

    /// The container kind of the top-level property `ns`/`name`: `"Seq"`, `"Bag"` or `"other"`.
    fn container_kind(pm: &PhotoMetadata, ns: &str, name: &str) -> &'static str {
        match &pm.xmp.get(ns, name).unwrap().value {
            XmpValue::Array(XmpArray::Seq(_)) => "Seq",
            XmpValue::Array(XmpArray::Bag(_)) => "Bag",
            _ => "other",
        }
    }

    #[test]
    fn a_new_extension_list_is_the_bag_the_standard_specifies() {
        // Every structured `Iptc4xmpExt` list this module added is a Bag in the standard's XMP
        // Specs rows. The `standard` field of each projection is a literal the mutation gate
        // cannot alter, so each one is asserted here, written fresh into an empty graph.
        let ext = WellKnownNs::Iptc4XmpExt.uri();
        let fresh = |set: &dyn Fn(&mut PhotoMetadata), name: &str| {
            let mut pm = PhotoMetadata::new();
            set(&mut pm);
            assert_eq!(container_kind(&pm, ext, name), "Bag", "{name}");
        };
        fresh(&|pm| pm.set_locations_shown(&[location()]), "LocationShown");
        fresh(
            &|pm| pm.set_persons_shown(&[person()]),
            "PersonInImageWDetails",
        );
        fresh(
            &|pm| pm.set_about_cv_terms(&[cv_term("outdoors")]),
            "AboutCvTerm",
        );
        fresh(&|pm| pm.set_genres(&[cv_term("portrait")]), "Genre");
        fresh(&|pm| pm.set_contributors(&[contributor()]), "Contributor");
        fresh(&|pm| pm.set_products_shown(&[product()]), "ProductInImage");
        fresh(
            &|pm| pm.set_registry_entries(&[registry_entry()]),
            "RegistryId",
        );
        fresh(
            &|pm| pm.set_embedded_rights_expressions(&[embedded_rights()]),
            "EmbdEncRightsExpr",
        );
        fresh(
            &|pm| pm.set_linked_rights_expressions(&[linked_rights()]),
            "LinkedEncRightsExpr",
        );
    }

    /// Every shape a top-level property's *value* can arrive in: the canonical form the model
    /// writes, and every departure from it a graph can carry — a container kind it does not write,
    /// a member it cannot read or cannot write back, and a value that is not an array at all.
    fn top_level_values(member: &XmpValue) -> Vec<(&'static str, XmpValue)> {
        let bag = |items: Vec<XmpItem>| XmpValue::Array(XmpArray::Bag(items));
        let one = || XmpItem::new(member.clone());
        let vendor = XmpValue::Structured(vec![XmpProperty::new(
            "http://example.org/vendor/",
            "Tint",
            text_value("warm"),
        )]);
        vec![
            ("a bag of one structure", bag(vec![one()])),
            (
                "a seq of one structure",
                XmpValue::Array(XmpArray::Seq(vec![one()])),
            ),
            (
                "an alt of one structure",
                XmpValue::Array(XmpArray::Alt(vec![one()])),
            ),
            ("a bag of two structures", bag(vec![one(), one()])),
            (
                "a bag of a structure the model does not name",
                bag(vec![XmpItem::new(vendor)]),
            ),
            (
                "a bag of a structure with no field at all",
                bag(vec![XmpItem::new(XmpValue::Structured(Vec::new()))]),
            ),
            (
                "a bag holding a member that is not a structure",
                bag(vec![one(), XmpItem::simple("not a structure")]),
            ),
            (
                "a bag holding a member held as rdf:resource",
                bag(vec![XmpItem::new(XmpValue::Uri(
                    "https://example.org/".to_owned(),
                ))]),
            ),
            (
                "a bag holding a member that carries a qualifier",
                bag(vec![XmpItem {
                    value: member.clone(),
                    qualifiers: vec![XmpProperty::new(XML_NAMESPACE, "lang", text_value("fr"))],
                }]),
            ),
            ("an empty bag", bag(Vec::new())),
            ("a bare structure", member.clone()),
            (
                "a structure with no field at all",
                XmpValue::Structured(Vec::new()),
            ),
            ("simple text", text_value("not a structure")),
            (
                "a value held as rdf:resource",
                XmpValue::Uri("https://example.org/".to_owned()),
            ),
            (
                "a language alternative of text",
                XmpValue::Array(XmpArray::Alt(vec![
                    XmpItem::lang_text(X_DEFAULT, "Sunflowers"),
                    XmpItem::lang_text("fr", "Tournesols"),
                ])),
            ),
        ]
    }

    /// Every qualifier list a top-level property can carry.
    fn top_level_qualifiers() -> Vec<(&'static str, Vec<XmpProperty>)> {
        let lang = || XmpProperty::new(XML_NAMESPACE, "lang", text_value("fr"));
        let vendor = || XmpProperty::new("http://example.org/vendor/", "Note", text_value("n"));
        vec![
            ("no qualifier", Vec::new()),
            ("an xml:lang qualifier", vec![lang()]),
            ("a vendor qualifier", vec![vendor()]),
            ("two qualifiers", vec![lang(), vendor()]),
        ]
    }

    #[test]
    fn every_top_level_shape_survives_a_read_modify_write_unchanged() {
        // The same law as `every_shape_survives_a_read_modify_write_unchanged`, one level up, and
        // generated rather than listed: every accessor pair crossed with every value shape and
        // every qualifier list. A shape the setter reproduces goes back out as the model's own
        // output; one it cannot is left where it lies. Either way the graph that comes out — the
        // property under test and the unrelated one beside it — is the graph that went in.
        let neighbour = XmpProperty::new(
            WellKnownNs::Photoshop.uri(),
            "Headline",
            text_value("Breaking"),
        );
        let mut crossed = 0_usize;
        for property in top_properties() {
            let member = (property.member)();
            for (value_label, value) in top_level_values(&member) {
                for (qualifier_label, qualifiers) in top_level_qualifiers() {
                    let mut pm = PhotoMetadata::new();
                    pm.xmp.properties.push(XmpProperty {
                        namespace: property.ns.to_owned(),
                        name: property.name.to_owned(),
                        value: value.clone(),
                        qualifiers,
                    });
                    pm.xmp.properties.push(neighbour.clone());
                    let before = pm.xmp.clone();
                    (property.trip)(&mut pm);
                    assert_eq!(
                        pm.xmp, before,
                        "{}: {value_label}, {qualifier_label}: a read-modify-write did not give \
                         the graph back",
                        property.label
                    );
                    crossed += 1;
                }
            }
        }
        // The cross is what makes this a sweep rather than a spot check: 17 pairs x 15 value
        // shapes x 4 qualifier lists.
        assert_eq!(crossed, 1020);
    }
}
