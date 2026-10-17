//! Drift guard: pins gamut's hand-transcribed IIM↔XMP tables to the IPTC machine-readable
//! technical reference vendored in `references/iptc/iptc-pmd-techreference_2025.1.json`.
//!
//! [`gamut_iptc::schema::FIELD_MAP`] and the [`gamut_iptc::IimTagInfo`] table are transcribed from
//! that file (the `ipmd_top` entries carrying an `IIMid`), and so are the typed structures of
//! [`gamut_iptc::extension`] (the `ipmd_struct` entries). These tests re-derive both from the JSON
//! at test time and compare, so any transcription slip — or a future IPTC release changing the
//! reference — fails loudly instead of silently drifting. The versioned filename makes bumping to a
//! new IPTC edition a deliberate act that re-runs this gate.

use std::collections::{BTreeMap, BTreeSet};

use gamut_iptc::extension::{
    ArtworkOrObject, CopyrightOwner, CreatorContactInfo, CvTerm, EmbeddedRightsExpression, Entity,
    EntityWithRole, ImageCreator, ImageRegion, ImageSupplier, Licensor, LinkedRightsExpression,
    Location, Person, Product, RegionBoundary, RegionBoundaryPoint, RegistryEntry,
};
use gamut_iptc::schema::{FIELD_MAP, XmpShape, ns};
use gamut_iptc::xmp::{XmpProperty, XmpValue};
use gamut_iptc::{IimTagInfo, PhotoMetadata};
use serde_json::Value;

/// Where the vendored references disagree, the crate follows `iim-4.2.pdf`. Each row pins BOTH
/// values — `((record, dataset), crate max_octets, JSON IIMmaxbytes, why)` — so the exception
/// self-invalidates if either source or the crate changes.
const LIMIT_EXCEPTIONS: &[((u8, u8), u32, u64, &str)] = &[(
    (2, 4),
    68,
    64,
    "IIM 4.2 wire form: 3-digit reference number + ':' + up to 64 octets of text = 68 octets; \
     the PMD JSON's IIMmaxbytes counts only the text part",
)];

fn techreference() -> Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../references/iptc/iptc-pmd-techreference_2025.1.json");
    let json = std::fs::read_to_string(&path)
        .expect("vendored IPTC tech reference (references/iptc) must be readable");
    serde_json::from_str(&json).expect("tech reference must be valid JSON")
}

/// The `ipmd_top` entries that carry an `IIMid`, as `(record, dataset) -> entry`.
fn iim_mapped_entries(doc: &Value) -> BTreeMap<(u8, u8), &Value> {
    doc["ipmd_top"]
        .as_object()
        .expect("ipmd_top is an object")
        .values()
        .filter_map(|entry| {
            let iim_id = entry.get("IIMid")?.as_str()?;
            let (record, dataset) = iim_id.split_once(':').expect("IIMid is R:DD");
            let key = (
                record.parse().expect("IIMid record is a u8"),
                dataset.parse().expect("IIMid dataset is a u8"),
            );
            Some((key, entry))
        })
        .collect()
}

/// gamut's namespace URI -> the prefix the tech reference uses in `XMPid`.
fn ns_prefix(uri: &str) -> &'static str {
    match uri {
        _ if uri == ns::DC => "dc",
        _ if uri == ns::PHOTOSHOP => "photoshop",
        _ if uri == ns::XMP_RIGHTS => "xmpRights",
        _ if uri == ns::IPTC_CORE => "Iptc4xmpCore",
        _ if uri == ns::IPTC_EXT => "Iptc4xmpExt",
        _ if uri == ns::PLUS => "plus",
        _ if uri == ns::XMP => "xmp",
        _ if uri == ns::EXIF => "exif",
        _ => panic!("gamut references a namespace outside the IPTC set: {uri}"),
    }
}

/// The `XMPid`s the reference gives the fields of the `ipmd_struct` entry named `structure`,
/// skipping the wildcard `$anypmdproperty` row (which has no identity of its own).
fn struct_field_ids(doc: &Value, structure: &str) -> BTreeSet<String> {
    doc["ipmd_struct"][structure]
        .as_object()
        .unwrap_or_else(|| panic!("ipmd_struct has no {structure} entry"))
        .values()
        .filter_map(|field| match field["XMPid"].as_str() {
            Some("") | None => None,
            Some(id) => Some(id.to_owned()),
        })
        .collect()
}

/// The `prefix:name` of every field a structure value carries.
fn emitted_field_ids(value: &XmpValue) -> BTreeSet<String> {
    let XmpValue::Structured(fields) = value else {
        panic!("to_xmp must produce a structure value");
    };
    fields
        .iter()
        .map(|p: &XmpProperty| format!("{}:{}", ns_prefix(&p.namespace), p.name))
        .collect()
}

/// The IIM↔XMP mapping must be a bijection between FIELD_MAP and the JSON's IIMid-bearing rows:
/// no missing rows, no extra rows, no XMPid mismatch.
#[test]
fn field_map_matches_techreference_iim_mapping() {
    let doc = techreference();
    let json_rows: BTreeMap<(u8, u8), String> = iim_mapped_entries(&doc)
        .into_iter()
        .map(|(key, entry)| {
            let xmp_id = entry["XMPid"].as_str().expect("XMPid is a string");
            (key, xmp_id.to_owned())
        })
        .collect();

    let map_rows: BTreeMap<(u8, u8), String> = FIELD_MAP
        .iter()
        .map(|row| {
            // The JSON records DateCreated under 2:55 only; 2:60 (Time Created) is the crate's
            // companion time half, asserted separately below.
            let (record, dataset) = row.iim[0];
            let xmp_id = format!("{}:{}", ns_prefix(row.xmp.ns), row.xmp.name);
            ((record, dataset), xmp_id)
        })
        .collect();

    assert_eq!(
        map_rows, json_rows,
        "FIELD_MAP and the tech reference disagree on the IIM<->XMP mapping"
    );

    // The only multi-dataset row is the 2:55+2:60 DateCreated pair.
    for row in FIELD_MAP {
        match row.xmp.shape {
            XmpShape::DateTime => assert_eq!(row.iim, [(2, 55), (2, 60)], "DateCreated datasets"),
            _ => assert_eq!(
                row.iim.len(),
                1,
                "{} maps exactly one dataset",
                row.xmp.name
            ),
        }
    }
}

/// Every mapped dataset's octet limit must match the JSON's `IIMmaxbytes`, modulo the documented
/// exception list (where the crate follows the IIM 4.2 PDF instead).
#[test]
fn tag_table_octet_limits_match_techreference() {
    let doc = techreference();
    for (key, entry) in iim_mapped_entries(&doc) {
        let (record, dataset) = key;
        let info = IimTagInfo::lookup(record, dataset)
            .unwrap_or_else(|| panic!("{record}:{dataset} is IIM-mapped but not in the tag table"));
        // 2:55 Date Created carries no IIMmaxbytes in the JSON (its length is fixed by form).
        let Some(json_max) = entry.get("IIMmaxbytes").and_then(Value::as_u64) else {
            continue;
        };
        if let Some(&(_, crate_max, exc_json_max, why)) =
            LIMIT_EXCEPTIONS.iter().find(|(k, ..)| *k == key)
        {
            // Pin both sides so the exception self-invalidates when either source moves.
            assert_eq!(
                info.max_octets,
                Some(crate_max),
                "{record}:{dataset}: {why}"
            );
            assert_eq!(json_max, exc_json_max, "{record}:{dataset}: {why}");
        } else {
            assert_eq!(
                info.max_octets.map(u64::from),
                Some(json_max),
                "{record}:{dataset} octet limit drifted from the tech reference"
            );
        }
    }
}

/// The XMP shape of each mapped field must agree with the JSON's occurrence and type columns.
#[test]
fn shapes_match_techreference_occurrence_and_type() {
    let doc = techreference();
    let entries = iim_mapped_entries(&doc);
    for row in FIELD_MAP {
        let entry = entries[&row.iim[0]];
        let multi = entry["propoccurrence"].as_str() == Some("multi");
        let shape_multi = matches!(row.xmp.shape, XmpShape::Bag | XmpShape::Seq);
        assert_eq!(
            multi, shape_multi,
            "{}: propoccurrence vs shape mismatch",
            row.xmp.name
        );
        // AltLang struct rows must be LangAlt; the date-time row must be DateTime.
        if entry.get("dataformat").and_then(Value::as_str) == Some("AltLang") {
            assert_eq!(row.xmp.shape, XmpShape::LangAlt, "{}", row.xmp.name);
        }
        if entry.get("dataformat").and_then(Value::as_str) == Some("date-time") {
            assert_eq!(row.xmp.shape, XmpShape::DateTime, "{}", row.xmp.name);
        }
    }

    // Sanity: the generic field accessors respect the reference-mandated container kinds — a
    // multi property written through set_field must come back multi-valued.
    let mut pm = PhotoMetadata::new();
    for row in FIELD_MAP {
        if matches!(row.xmp.shape, XmpShape::Bag | XmpShape::Seq) {
            pm.set_field(&row.xmp, &["a", "b"]);
            assert_eq!(pm.get_field(&row.xmp), ["a", "b"], "{}", row.xmp.name);
        }
    }
}

/// A [`CreatorContactInfo`] with every field set, so `to_xmp` emits the whole structure.
///
/// The extension structures are `#[non_exhaustive]` — a future IPTC edition adding a field must
/// not be a breaking change — so a downstream caller builds one from [`Default`] and assigns.
fn full_contact() -> CreatorContactInfo {
    let mut it = CreatorContactInfo::default();
    it.address = Some("1 Rue Test".to_owned());
    it.city = Some("Lyon".to_owned());
    it.country = Some("France".to_owned());
    it.postal_code = Some("69000".to_owned());
    it.region = Some("Rhône".to_owned());
    it.email = Some("a@example.org".to_owned());
    it.phone = Some("+33 1 23".to_owned());
    it.web_url = Some("https://example.org/".to_owned());
    it
}

/// An [`ArtworkOrObject`] with every field set.
fn full_artwork() -> ArtworkOrObject {
    let mut it = ArtworkOrObject::default();
    it.title = Some("Sunflowers".to_owned());
    it.creator_names = vec!["Van Gogh".to_owned()];
    it.creator_identifiers = vec!["urn:creator".to_owned()];
    it.date_created = Some("1888-08".to_owned());
    it.circa_date_created = Some("circa 1888".to_owned());
    it.copyright_notice = Some("Public domain".to_owned());
    it.current_copyright_owner_name = Some("Owner".to_owned());
    it.current_copyright_owner_identifier = Some("urn:owner".to_owned());
    it.current_licensor_name = Some("Licensor".to_owned());
    it.current_licensor_identifier = Some("urn:licensor".to_owned());
    it.content_description = Some("Vase with flowers".to_owned());
    it.contribution_description = Some("Restored 1980".to_owned());
    it.physical_description = Some("Oil on canvas".to_owned());
    it.source = Some("National Gallery".to_owned());
    it.source_inventory_number = Some("NG3863".to_owned());
    it.source_inventory_url = Some("https://example.org/NG3863".to_owned());
    it.style_periods = vec!["Post-Impressionism".to_owned()];
    it
}

/// A [`Licensor`] with every field set.
fn full_licensor() -> Licensor {
    let mut it = Licensor::default();
    it.identifier = Some("urn:licensor".to_owned());
    it.name = Some("Agence gamut".to_owned());
    it.address = Some("2 Rue Test".to_owned());
    it.address_detail = Some("Floor 3".to_owned());
    it.city = Some("Paris".to_owned());
    it.region = Some("Île-de-France".to_owned());
    it.postal_code = Some("75001".to_owned());
    it.country = Some("France".to_owned());
    it.telephone_type1 = Some("work".to_owned());
    it.telephone1 = Some("+33 1 11".to_owned());
    it.telephone_type2 = Some("cell".to_owned());
    it.telephone2 = Some("+33 6 22".to_owned());
    it.email = Some("licence@example.org".to_owned());
    it.web_url = Some("https://example.org/licence".to_owned());
    it
}

/// An [`Entity`] with every field set.
fn full_entity() -> Entity {
    let mut it = Entity::default();
    it.identifiers = vec!["https://cv.iptc.org/newscodes/imageregiontype/human".to_owned()];
    it.name = Some("Human".to_owned());
    it
}

/// A [`RegionBoundaryPoint`] with both coordinates set.
fn full_point() -> RegionBoundaryPoint {
    let mut it = RegionBoundaryPoint::default();
    it.x = Some(1.0);
    it.y = Some(2.0);
    it
}

/// A [`RegionBoundary`] with every field set. The shapes are mutually exclusive in practice, but
/// the reference defines all seven scalars plus the vertex list on the one structure.
fn full_boundary() -> RegionBoundary {
    let mut it = RegionBoundary::default();
    it.shape = Some("polygon".to_owned());
    it.unit = Some("relative".to_owned());
    it.x = Some(0.25);
    it.y = Some(0.5);
    it.width = Some(0.125);
    it.height = Some(0.0625);
    it.radius = Some(0.1);
    it.vertices = vec![full_point()];
    it
}

/// An [`ImageRegion`] with every modelled field set and no extra properties.
fn full_region() -> ImageRegion {
    let mut it = ImageRegion::default();
    it.boundary = Some(full_boundary());
    it.identifier = Some("region-1".to_owned());
    it.name = Some("Face".to_owned());
    it.content_types = vec![full_entity()];
    it.roles = vec![full_entity()];
    it
}

/// A [`Location`] with every field set.
fn full_location() -> Location {
    let mut it = Location::default();
    it.identifiers = vec!["https://example.org/loc/1".to_owned()];
    it.name = Some("Place Bellecour".to_owned());
    it.sublocation = Some("Presqu'île".to_owned());
    it.city = Some("Lyon".to_owned());
    it.province_state = Some("Rhône".to_owned());
    it.country_name = Some("France".to_owned());
    it.country_code = Some("FRA".to_owned());
    it.world_region = Some("Europe".to_owned());
    it.gps_latitude = Some("45,45.47N".to_owned());
    it.gps_longitude = Some("4,49.92E".to_owned());
    it.gps_altitude = Some("170/1".to_owned());
    it.gps_altitude_ref = Some("0".to_owned());
    it
}

/// A [`CvTerm`] with every field set.
fn full_cv_term() -> CvTerm {
    let mut it = CvTerm::default();
    it.vocabulary_identifier = Some("http://cv.iptc.org/newscodes/scene/".to_owned());
    it.identifier = Some("http://cv.iptc.org/newscodes/scene/011900".to_owned());
    it.name = Some("outdoors".to_owned());
    it.refined_about = Some("http://cv.iptc.org/newscodes/rbac/visual".to_owned());
    it
}

/// A [`Person`] with every field set.
fn full_person() -> Person {
    let mut it = Person::default();
    it.identifiers = vec!["urn:person".to_owned()];
    it.name = Some("Ada Lovelace".to_owned());
    it.characteristics = vec![full_cv_term()];
    it.description = Some("Seated, left of frame".to_owned());
    it
}

/// An [`EntityWithRole`] with every field set.
fn full_contributor() -> EntityWithRole {
    let mut it = EntityWithRole::default();
    it.identifiers = vec!["urn:contributor".to_owned()];
    it.name = Some("Studio gamut".to_owned());
    it.roles = vec!["http://cv.iptc.org/newscodes/contrrole/retoucher".to_owned()];
    it
}

/// A [`Product`] with every field set.
fn full_product() -> Product {
    let mut it = Product::default();
    it.identifiers = vec!["urn:product".to_owned()];
    it.name = Some("Teapot".to_owned());
    it.gtin = Some("4006381333931".to_owned());
    it.description = Some("White porcelain".to_owned());
    it
}

/// A [`RegistryEntry`] with every field set.
fn full_registry_entry() -> RegistryEntry {
    let mut it = RegistryEntry::default();
    it.item_identifier = Some("item-42".to_owned());
    it.organisation_identifier = Some("https://registry.example.org/".to_owned());
    it.role = Some("http://cv.iptc.org/newscodes/imageregistryrole/canonicalLink".to_owned());
    it
}

/// An [`EmbeddedRightsExpression`] with every field set.
fn full_embedded_rights() -> EmbeddedRightsExpression {
    let mut it = EmbeddedRightsExpression::default();
    it.expression = Some("<o:Policy/>".to_owned());
    it.encoding_type = Some("application/xml".to_owned());
    it.language_identifier = Some("http://www.w3.org/ns/odrl/2/".to_owned());
    it
}

/// A [`LinkedRightsExpression`] with every field set.
fn full_linked_rights() -> LinkedRightsExpression {
    let mut it = LinkedRightsExpression::default();
    it.url = Some("https://example.org/policy.xml".to_owned());
    it.encoding_type = Some("application/xml".to_owned());
    it.language_identifier = Some("http://www.w3.org/ns/odrl/2/".to_owned());
    it
}

/// A [`CopyrightOwner`] with both fields set.
fn full_copyright_owner() -> CopyrightOwner {
    let mut it = CopyrightOwner::default();
    it.identifier = Some("urn:owner".to_owned());
    it.name = Some("Owner".to_owned());
    it
}

/// An [`ImageCreator`] with both fields set.
fn full_image_creator() -> ImageCreator {
    let mut it = ImageCreator::default();
    it.identifier = Some("urn:creator".to_owned());
    it.name = Some("Creator".to_owned());
    it
}

/// An [`ImageSupplier`] with both fields set.
fn full_image_supplier() -> ImageSupplier {
    let mut it = ImageSupplier::default();
    it.identifier = Some("urn:supplier".to_owned());
    it.name = Some("Supplier".to_owned());
    it
}

/// Each typed structure must emit exactly the fields the reference's `ipmd_struct` entry defines —
/// no invented field, none missed, and none under a mistyped namespace prefix.
#[test]
fn extension_structures_match_techreference_field_sets() {
    let doc = techreference();
    let cases: [(&str, XmpValue); 18] = [
        ("CreatorContactInfo", full_contact().to_xmp()),
        ("ArtworkOrObject", full_artwork().to_xmp()),
        ("Licensor", full_licensor().to_xmp()),
        ("Entity", full_entity().to_xmp()),
        ("RegionBoundaryPoint", full_point().to_xmp()),
        ("RegionBoundary", full_boundary().to_xmp()),
        ("ImageRegion", full_region().to_xmp()),
        ("Location", full_location().to_xmp()),
        ("CvTerm", full_cv_term().to_xmp()),
        ("PersonWDetails", full_person().to_xmp()),
        ("EntityWRole", full_contributor().to_xmp()),
        ("ProductWGtin", full_product().to_xmp()),
        ("RegistryEntry", full_registry_entry().to_xmp()),
        ("EmbdEncRightsExpr", full_embedded_rights().to_xmp()),
        ("LinkedEncRightsExpr", full_linked_rights().to_xmp()),
        ("CopyrightOwner", full_copyright_owner().to_xmp()),
        ("ImageCreator", full_image_creator().to_xmp()),
        ("ImageSupplier", full_image_supplier().to_xmp()),
    ];
    for (structure, value) in &cases {
        assert_eq!(
            emitted_field_ids(value),
            struct_field_ids(&doc, structure),
            "{structure} fields drifted from the tech reference"
        );
    }

    // Every structure the reference defines is modelled, except the `AltLang` language
    // alternative, which gamut models as a field shape (a `String` read from `x-default`).
    let modelled: BTreeSet<&str> = cases.iter().map(|&(name, _)| name).collect();
    let defined: BTreeSet<&str> = doc["ipmd_struct"]
        .as_object()
        .expect("ipmd_struct is an object")
        .keys()
        .map(String::as_str)
        .filter(|&name| name != "AltLang")
        .collect();
    assert_eq!(
        modelled, defined,
        "a reference structure has no typed model"
    );
}

/// Every top-level property gamut gives a structured accessor pair, as
/// `(reference key, XMPid, structure name)`.
const STRUCTURED_TOP: &[(&str, &str, &str)] = &[
    (
        "creatorContactInfo",
        "Iptc4xmpCore:CreatorContactInfo",
        "CreatorContactInfo",
    ),
    ("imageRegion", "Iptc4xmpExt:ImageRegion", "ImageRegion"),
    (
        "artworkOrObjects",
        "Iptc4xmpExt:ArtworkOrObject",
        "ArtworkOrObject",
    ),
    ("licensors", "plus:Licensor", "Licensor"),
    ("locationCreated", "Iptc4xmpExt:LocationCreated", "Location"),
    ("locationsShown", "Iptc4xmpExt:LocationShown", "Location"),
    (
        "personsShown",
        "Iptc4xmpExt:PersonInImageWDetails",
        "PersonWDetails",
    ),
    ("aboutCvTerms", "Iptc4xmpExt:AboutCvTerm", "CvTerm"),
    ("genres", "Iptc4xmpExt:Genre", "CvTerm"),
    ("contributors", "Iptc4xmpExt:Contributor", "EntityWRole"),
    (
        "productsShown",
        "Iptc4xmpExt:ProductInImage",
        "ProductWGtin",
    ),
    ("registryEntries", "Iptc4xmpExt:RegistryId", "RegistryEntry"),
    (
        "embdEncRightsExprs",
        "Iptc4xmpExt:EmbdEncRightsExpr",
        "EmbdEncRightsExpr",
    ),
    (
        "linkedEncRightsExprs",
        "Iptc4xmpExt:LinkedEncRightsExpr",
        "LinkedEncRightsExpr",
    ),
    ("copyrightOwners", "plus:CopyrightOwner", "CopyrightOwner"),
    ("imageCreators", "plus:ImageCreator", "ImageCreator"),
    ("suppliers", "plus:ImageSupplier", "ImageSupplier"),
];

/// The typed structures must sit on the `ipmd_top` properties the reference names, with the
/// structure type it names — a projection hung on the wrong property would still round-trip.
#[test]
fn extension_accessors_target_the_techreference_top_properties() {
    let doc = techreference();
    let top = doc["ipmd_top"].as_object().expect("ipmd_top is an object");
    let expected = STRUCTURED_TOP;
    for &(key, xmp_id, structure) in expected {
        let entry = &top[key];
        assert_eq!(entry["XMPid"].as_str(), Some(xmp_id), "{key} XMPid");
        assert_eq!(
            entry["dataformat"].as_str(),
            Some(structure),
            "{key} structure"
        );
    }

    // Every top-level property the reference types with a structure is in that list, apart from
    // the `AltLang` ones, which are language-alternative fields rather than structures.
    let structured: BTreeSet<&str> = top
        .values()
        .filter(|entry| {
            entry["datatype"].as_str() == Some("struct")
                && entry["dataformat"].as_str() != Some("AltLang")
        })
        .map(|entry| entry["XMPid"].as_str().expect("XMPid is a string"))
        .collect();
    let listed: BTreeSet<&str> = expected.iter().map(|&(_, id, _)| id).collect();
    assert_eq!(
        structured, listed,
        "a structured top-level property has no accessor"
    );

    // The accessors write those exact properties: one value per pair in, one property each out.
    let mut pm = PhotoMetadata::new();
    pm.set_creator_contact_info(&full_contact());
    pm.set_image_regions(&[full_region()]);
    pm.set_artwork_or_objects(&[full_artwork()]);
    pm.set_licensors(&[full_licensor()]);
    pm.set_locations_created(&[full_location()]);
    pm.set_locations_shown(&[full_location()]);
    pm.set_persons_shown(&[full_person()]);
    pm.set_about_cv_terms(&[full_cv_term()]);
    pm.set_genres(&[full_cv_term()]);
    pm.set_contributors(&[full_contributor()]);
    pm.set_products_shown(&[full_product()]);
    pm.set_registry_entries(&[full_registry_entry()]);
    pm.set_embedded_rights_expressions(&[full_embedded_rights()]);
    pm.set_linked_rights_expressions(&[full_linked_rights()]);
    pm.set_copyright_owners(&[full_copyright_owner()]);
    pm.set_image_creators(&[full_image_creator()]);
    pm.set_image_suppliers(&[full_image_supplier()]);
    let written: BTreeSet<String> = pm
        .xmp
        .properties
        .iter()
        .map(|p| format!("{}:{}", ns_prefix(&p.namespace), p.name))
        .collect();
    let named: BTreeSet<String> = expected.iter().map(|&(_, id, _)| id.to_owned()).collect();
    assert_eq!(written, named);
}

/// Every structured property gamut models is XMP-only in the reference — no `IIMid`, and so no row
/// in `FIELD_MAP`. That is what makes a structured field unable to conflict with the legacy
/// carrier, so it is pinned to the reference rather than merely documented.
#[test]
fn modelled_structured_properties_carry_no_iim_counterpart() {
    let doc = techreference();
    let top = doc["ipmd_top"].as_object().expect("ipmd_top is an object");
    for &(key, xmp_id, _) in STRUCTURED_TOP {
        assert!(
            top[key].get("IIMid").is_none(),
            "{key} has an IIM counterpart, so it can conflict and needs a reconciliation rule"
        );
        // ...and none of them is in the IIM<->XMP map, whose rows are exactly the IIMid-bearing
        // ones.
        assert!(
            !FIELD_MAP
                .iter()
                .any(|row| format!("{}:{}", ns_prefix(row.xmp.ns), row.xmp.name) == xmp_id),
            "{xmp_id} is reconciled but has no IIM dataset"
        );
    }
}
