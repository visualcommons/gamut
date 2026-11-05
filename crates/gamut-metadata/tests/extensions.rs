//! The extension carrier: namespaced data no carrier models, and the two round-trip guarantees it
//! separates — a **model** round-trip that preserves extensions, and the **carrier** round-trip
//! keystone, which they take no part in.

use gamut_metadata::exif::{ByteOrder, Exif, ExifTag, Value};
use gamut_metadata::icc::{ColorSpace, DeviceClass, IccProfile, ProfileHeader};
use gamut_metadata::xmp::{WellKnownNs, XmpMeta};
use gamut_metadata::{
    EncodedMetadata, ExtensionPolicy, Metadata, MetadataBlock, MetadataEmbedder, MetadataError,
    MetadataExtension, MetadataExtractor,
};

const NS: &str = "com.example.raw";
const OTHER_NS: &str = "com.example.container";

// --- carrier fixtures (as in `roundtrip.rs`: each produced by its own leaf crate) ----------------

fn exif_bytes() -> Vec<u8> {
    let mut exif = Exif::new(ByteOrder::LittleEndian);
    exif.set_tag(ExifTag::Make, Value::Ascii("gamut".to_owned()));
    exif.to_bytes().expect("a one-tag EXIF blob serializes")
}

fn xmp_bytes() -> Vec<u8> {
    let mut xmp = XmpMeta::new();
    xmp.set_text(WellKnownNs::Xmp.uri(), "CreatorTool", "gamut");
    xmp.to_bytes()
}

fn icc_bytes() -> Vec<u8> {
    IccProfile {
        header: ProfileHeader::new(DeviceClass::Display, ColorSpace::Rgb),
        tags: Vec::new(),
    }
    .to_bytes()
    .expect("a header-only profile serializes")
}

fn reextract(enc: &EncodedMetadata) -> Metadata {
    let mut blocks = Vec::new();
    if let Some(b) = &enc.exif {
        blocks.push(MetadataBlock::Exif(b));
    }
    if let Some(b) = &enc.xmp {
        blocks.push(MetadataBlock::Xmp(b));
    }
    if let Some(b) = &enc.icc {
        blocks.push(MetadataBlock::Icc(b));
    }
    // Enumerated for completeness; embedding never fills it (see `roundtrip.rs`'s C2PA tests).
    if let Some(b) = &enc.c2pa {
        blocks.push(MetadataBlock::C2pa(b));
    }
    MetadataExtractor::new().extract(&blocks).unwrap()
}

// --- the table ------------------------------------------------------------------------------------

#[test]
fn set_extension_inserts_then_replaces_in_place() {
    let mut meta = Metadata::default();
    meta.set_extension(NS, "WhiteLevel", Value::Long(vec![16_383]));
    meta.set_extension(NS, "BlackLevel", Value::Short(vec![512]));

    // Re-setting an existing key replaces its value where it already sits — it does not append,
    // so the pair stays unique and the surrounding order is untouched.
    meta.set_extension(NS, "WhiteLevel", Value::Long(vec![4_095]));

    assert_eq!(meta.extensions.len(), 2);
    assert_eq!(meta.extensions[0].key, "WhiteLevel");
    assert_eq!(meta.extensions[0].value, Value::Long(vec![4_095]));
    assert_eq!(meta.extensions[1].key, "BlackLevel");
}

#[test]
fn extension_lookup_is_namespace_scoped() {
    let mut meta = Metadata::default();
    meta.set_extension(NS, "Depth", Value::Short(vec![14]));
    meta.set_extension(OTHER_NS, "Depth", Value::Short(vec![8]));

    // The same key in two namespaces stays two distinct entries.
    assert_eq!(meta.extensions.len(), 2);
    assert_eq!(meta.extension(NS, "Depth"), Some(&Value::Short(vec![14])));
    assert_eq!(
        meta.extension(OTHER_NS, "Depth"),
        Some(&Value::Short(vec![8]))
    );
    assert_eq!(meta.extension("com.example.absent", "Depth"), None);
    assert_eq!(meta.extension(NS, "Absent"), None);
}

#[test]
fn remove_extension_returns_the_value_it_removed() {
    let mut meta = Metadata::default();
    meta.set_extension(NS, "Depth", Value::Short(vec![14]));

    assert_eq!(
        meta.remove_extension(NS, "Depth"),
        Some(Value::Short(vec![14]))
    );
    assert_eq!(meta.extension(NS, "Depth"), None);
}

#[test]
fn remove_extension_leaves_the_same_key_under_another_namespace() {
    // The namespace is half the key. A removal that matched on name alone would take both, and
    // the caller would have no way to tell -- both reads simply return None.
    let mut meta = Metadata::default();
    meta.set_extension(NS, "Depth", Value::Short(vec![14]));
    meta.set_extension(OTHER_NS, "Depth", Value::Short(vec![8]));

    meta.remove_extension(NS, "Depth");

    assert_eq!(
        meta.extension(OTHER_NS, "Depth"),
        Some(&Value::Short(vec![8]))
    );
}

#[test]
fn remove_extension_leaves_another_key_in_the_same_namespace() {
    // The mirror of the test above, and the half that was missing (#110). Together they pin that
    // the match is a conjunction: neither the namespace alone nor the key alone may decide it.
    //
    // Order matters here, and is why the sibling test could not catch this. `position` returns the
    // FIRST match, so a removal matching on namespace alone still lands on the right entry
    // whenever the target happens to come first. Putting a same-namespace, different-key entry
    // BEFORE the target is what makes the two behaviours diverge: a disjunctive match removes
    // `Alpha`, silently, and reports Alpha's value as though it were Depth's.
    let mut meta = Metadata::default();
    meta.set_extension(NS, "Alpha", Value::Short(vec![1]));
    meta.set_extension(NS, "Depth", Value::Short(vec![14]));

    assert_eq!(
        meta.remove_extension(NS, "Depth"),
        Some(Value::Short(vec![14])),
        "the value returned must be the one asked for, not the first entry sharing its namespace"
    );
    assert_eq!(
        meta.extension(NS, "Alpha"),
        Some(&Value::Short(vec![1])),
        "the untargeted key in the same namespace survives"
    );
    assert_eq!(meta.extension(NS, "Depth"), None);
}

#[test]
fn removing_an_absent_extension_reports_none() {
    // Removing what is not there reports so rather than panicking.
    let mut meta = Metadata::default();

    assert_eq!(meta.remove_extension(NS, "Depth"), None);
}

#[test]
fn extensions_in_yields_only_that_namespace_in_order() {
    let mut meta = Metadata::default();
    meta.set_extension(NS, "A", Value::Short(vec![1]));
    meta.set_extension(OTHER_NS, "B", Value::Short(vec![2]));
    meta.set_extension(NS, "C", Value::Short(vec![3]));

    let keys: Vec<&str> = meta.extensions_in(NS).map(|e| e.key.as_str()).collect();
    assert_eq!(keys, ["A", "C"]);

    let other: Vec<&str> = meta
        .extensions_in(OTHER_NS)
        .map(|e| e.key.as_str())
        .collect();
    assert_eq!(other, ["B"]);

    assert_eq!(meta.extensions_in("com.example.absent").count(), 0);
}

#[test]
fn is_empty_accounts_for_extensions() {
    let mut meta = Metadata::default();
    assert!(meta.is_empty());

    // An extension alone is still metadata: a model carrying one is not empty.
    meta.set_extension(NS, "WhiteLevel", Value::Long(vec![16_383]));
    assert!(!meta.is_empty());

    meta.remove_extension(NS, "WhiteLevel");
    assert!(meta.is_empty());
}

// --- guarantee 1: the model round-trip ------------------------------------------------------------

/// A downstream typed model, in the shape a raw pipeline actually carries: fields with no
/// still-image carrier at all (sensor geometry, a DNG colour matrix, an opaque vendor blob).
#[derive(Debug, Default, PartialEq)]
struct DownstreamModel {
    white_level: u32,
    black_levels: Vec<u16>,
    color_matrix: Vec<f64>,
    vendor_blob: Vec<u8>,
    profile_name: String,
}

impl DownstreamModel {
    fn to_metadata(&self) -> Metadata {
        let mut meta = Metadata::default();
        meta.set_extension(NS, "WhiteLevel", Value::Long(vec![self.white_level]));
        meta.set_extension(NS, "BlackLevels", Value::Short(self.black_levels.clone()));
        meta.set_extension(NS, "ColorMatrix1", Value::Double(self.color_matrix.clone()));
        meta.set_extension(NS, "VendorBlob", Value::Undefined(self.vendor_blob.clone()));
        meta.set_extension(NS, "ProfileName", Value::Ascii(self.profile_name.clone()));
        meta
    }

    fn from_metadata(meta: &Metadata) -> Self {
        let mut model = Self::default();
        for ext in meta.extensions_in(NS) {
            match (ext.key.as_str(), &ext.value) {
                ("WhiteLevel", Value::Long(v)) => model.white_level = v[0],
                ("BlackLevels", Value::Short(v)) => model.black_levels = v.clone(),
                ("ColorMatrix1", Value::Double(v)) => model.color_matrix = v.clone(),
                ("VendorBlob", Value::Undefined(v)) => model.vendor_blob = v.clone(),
                ("ProfileName", Value::Ascii(v)) => model.profile_name = v.clone(),
                _ => panic!("unexpected extension {}", ext.key),
            }
        }
        model
    }
}

#[test]
fn model_roundtrip_preserves_extensions() {
    let model = DownstreamModel {
        white_level: 16_383,
        black_levels: vec![512, 512, 512, 512],
        color_matrix: vec![
            0.6722, -0.0635, -0.0963, -0.4287, 1.246, 0.2028, -0.0908, 0.2162, 0.5668,
        ],
        vendor_blob: vec![0xDE, 0xAD, 0xBE, 0xEF],
        profile_name: "Camera Standard".to_owned(),
    };

    let meta = model.to_metadata();
    assert_eq!(DownstreamModel::from_metadata(&meta), model);

    // The model round-trip is a property of `Metadata` itself, so it survives being cloned and
    // handed on — extensions compare by value like every other field.
    assert_eq!(meta.clone(), meta);
    assert_eq!(DownstreamModel::from_metadata(&meta.clone()), model);
}

#[test]
fn extensions_travel_alongside_carriers_without_disturbing_them() {
    let (exif, xmp, icc) = (exif_bytes(), xmp_bytes(), icc_bytes());
    let mut meta = MetadataExtractor::new()
        .extract(&[
            MetadataBlock::Exif(&exif),
            MetadataBlock::Xmp(&xmp),
            MetadataBlock::Icc(&icc),
        ])
        .unwrap();
    let carriers_only = meta.clone();

    meta.set_extension(NS, "WhiteLevel", Value::Long(vec![16_383]));

    // Adding an extension changes the model but leaves every carrier byte-identical.
    assert_ne!(meta, carriers_only);
    assert_eq!(meta.exif, carriers_only.exif);
    assert_eq!(meta.xmp, carriers_only.xmp);
    assert_eq!(meta.icc, carriers_only.icc);
}

// --- guarantee 2: extensions take no part in the carrier round-trip -------------------------------

#[test]
fn extract_never_produces_extensions() {
    let (exif, xmp, icc) = (exif_bytes(), xmp_bytes(), icc_bytes());
    let iim = gamut_metadata::iptc::IimBlock {
        datasets: vec![gamut_metadata::iptc::IimDataSet {
            record: 2,
            dataset: 90,
            data: b"Oslo".to_vec(),
        }],
    }
    .encode()
    .unwrap();

    let meta = MetadataExtractor::new()
        .extract(&[
            MetadataBlock::Exif(&exif),
            MetadataBlock::Xmp(&xmp),
            MetadataBlock::Icc(&icc),
            MetadataBlock::IptcIim(&iim),
        ])
        .unwrap();

    assert!(meta.exif.is_some() && meta.xmp.is_some() && meta.icc.is_some());
    assert!(
        meta.extensions.is_empty(),
        "no block kind may synthesize an extension"
    );
}

#[test]
fn encode_drops_extensions_under_the_default_policy() {
    let mut meta = Metadata::default();
    meta.set_extension(NS, "WhiteLevel", Value::Long(vec![16_383]));

    // The extension produces no block of any kind.
    assert_eq!(meta.to_blocks().unwrap(), EncodedMetadata::default());

    // ...and it does not perturb the blocks a carrier would have produced on its own.
    let carriers = Metadata::from_carriers(Some(Exif::new(ByteOrder::LittleEndian)), None, None);
    let mut with_extension = carriers.clone();
    with_extension.set_extension(NS, "WhiteLevel", Value::Long(vec![16_383]));
    assert_eq!(
        with_extension.to_blocks().unwrap(),
        carriers.to_blocks().unwrap()
    );
}

#[test]
fn encode_rejects_extensions_under_reject_policy() {
    let mut meta = Metadata::from_carriers(Some(Exif::new(ByteOrder::LittleEndian)), None, None);
    meta.set_extension(NS, "WhiteLevel", Value::Long(vec![16_383]));
    meta.set_extension(OTHER_NS, "Later", Value::Long(vec![1]));

    let err = MetadataEmbedder::new()
        .extension_policy(ExtensionPolicy::Reject)
        .embed(&meta)
        .unwrap_err();

    // The error names the *first* offending extension, not merely that one existed.
    match &err {
        MetadataError::UnembeddableExtension { namespace, key } => {
            assert_eq!(namespace, NS);
            assert_eq!(key, "WhiteLevel");
        }
        other => panic!("expected UnembeddableExtension, got {other:?}"),
    }
    assert!(err.to_string().contains("com.example.raw/WhiteLevel"));

    // With no extension present, the same embedder succeeds — the policy is not a blanket refusal.
    let clean = Metadata::from_carriers(Some(Exif::new(ByteOrder::LittleEndian)), None, None);
    assert!(
        MetadataEmbedder::new()
            .extension_policy(ExtensionPolicy::Reject)
            .embed(&clean)
            .is_ok()
    );
}

#[test]
fn carrier_roundtrip_equality_holds_when_extensions_are_present() {
    let (exif, xmp, icc) = (exif_bytes(), xmp_bytes(), icc_bytes());
    let mut m1 = MetadataExtractor::new()
        .extract(&[
            MetadataBlock::Exif(&exif),
            MetadataBlock::Xmp(&xmp),
            MetadataBlock::Icc(&icc),
        ])
        .unwrap();
    m1.set_extension(NS, "WhiteLevel", Value::Long(vec![16_383]));

    let m2 = reextract(&m1.to_blocks().unwrap());

    // The keystone still holds over the three carriers...
    assert_eq!(m1.exif, m2.exif);
    assert_eq!(m1.xmp, m2.xmp);
    assert_eq!(m1.icc, m2.icc);
    // ...and the extension is gone, exactly as documented: it never had a carrier to ride.
    assert!(m2.extensions.is_empty());
    assert_ne!(m1, m2);
}

// --- the entry type -------------------------------------------------------------------------------

#[test]
fn extensions_pushed_directly_are_readable_through_the_accessors() {
    // The field is public, so a caller may build the table itself; the accessors must agree.
    let mut meta = Metadata::default();
    meta.extensions.push(MetadataExtension::new(
        NS,
        "WhiteLevel",
        Value::Long(vec![16_383]),
    ));

    assert_eq!(
        meta.extension(NS, "WhiteLevel"),
        Some(&Value::Long(vec![16_383]))
    );
    // And `set_extension` still replaces rather than duplicating that hand-built entry.
    meta.set_extension(NS, "WhiteLevel", Value::Long(vec![4_095]));
    assert_eq!(meta.extensions.len(), 1);
    assert_eq!(
        meta.extension(NS, "WhiteLevel"),
        Some(&Value::Long(vec![4_095]))
    );
}
