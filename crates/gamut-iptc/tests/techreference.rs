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
    ArtworkOrObject, CreatorContactInfo, Entity, ImageRegion, Licensor, RegionBoundary,
    RegionBoundaryPoint,
};
use gamut_iptc::schema::{FIELD_MAP, XmpShape, ns};
use gamut_iptc::xmp::{XmpProperty, XmpValue};
use gamut_iptc::{IimTagInfo, PhotoMetadata};
use serde_json::Value;

/// Where the vendored references disagree, the crate follows `iim-4.2.pdf`. Each row pins BOTH
/// values — `((record, dataset), crate max_octets, JSON IIMmaxbytes, why)` — so the exception
/// self-invalidates if either source or the crate changes.
const LIMIT_EXCEPTIONS: &[((u8, u8), u16, u64, &str)] = &[(
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
            assert_eq!(info.max_octets, crate_max, "{record}:{dataset}: {why}");
            assert_eq!(json_max, exc_json_max, "{record}:{dataset}: {why}");
        } else {
            assert_eq!(
                u64::from(info.max_octets),
                json_max,
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

/// Each typed structure must emit exactly the fields the reference's `ipmd_struct` entry defines —
/// no invented field, none missed, and none under a mistyped namespace prefix.
#[test]
fn extension_structures_match_techreference_field_sets() {
    let doc = techreference();
    let cases: [(&str, XmpValue); 7] = [
        ("CreatorContactInfo", full_contact().to_xmp()),
        ("ArtworkOrObject", full_artwork().to_xmp()),
        ("Licensor", full_licensor().to_xmp()),
        ("Entity", full_entity().to_xmp()),
        ("RegionBoundaryPoint", full_point().to_xmp()),
        ("RegionBoundary", full_boundary().to_xmp()),
        ("ImageRegion", full_region().to_xmp()),
    ];
    for (structure, value) in cases {
        assert_eq!(
            emitted_field_ids(&value),
            struct_field_ids(&doc, structure),
            "{structure} fields drifted from the tech reference"
        );
    }
}

/// The typed structures must sit on the `ipmd_top` properties the reference names, with the
/// structure type it names — a projection hung on the wrong property would still round-trip.
#[test]
fn extension_accessors_target_the_techreference_top_properties() {
    let doc = techreference();
    let top = doc["ipmd_top"].as_object().expect("ipmd_top is an object");
    // (reference key, expected XMPid, expected structure name)
    let expected = [
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
    ];
    for (key, xmp_id, structure) in expected {
        let entry = &top[key];
        assert_eq!(entry["XMPid"].as_str(), Some(xmp_id), "{key} XMPid");
        assert_eq!(
            entry["dataformat"].as_str(),
            Some(structure),
            "{key} structure"
        );
    }

    // The accessors write those exact properties: four values in, four properties out.
    let mut pm = PhotoMetadata::new();
    pm.set_creator_contact_info(&full_contact());
    pm.set_image_regions(&[full_region()]);
    pm.set_artwork_or_objects(&[full_artwork()]);
    pm.set_licensors(&[full_licensor()]);
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
    for key in [
        "creatorContactInfo",
        "imageRegion",
        "artworkOrObjects",
        "licensors",
    ] {
        assert!(
            top[key].get("IIMid").is_none(),
            "{key} has an IIM counterpart, so it can conflict and needs a reconciliation rule"
        );
    }
    // ...and none of them is in the IIM<->XMP map, whose rows are exactly the IIMid-bearing ones.
    for name in [
        "CreatorContactInfo",
        "ImageRegion",
        "ArtworkOrObject",
        "Licensor",
    ] {
        assert!(
            !FIELD_MAP.iter().any(|row| row.xmp.name == name),
            "{name} is reconciled but has no IIM dataset"
        );
    }
}
