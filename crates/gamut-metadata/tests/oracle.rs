//! exiv2 differential for the EXIF ↔ XMP mapping (`exif_to_xmp` / `xmp_to_exif`, issue #1247).
//!
//! The oracle is exiv2 0.28's own conversion table — `copyExifToXmp` / `copyXmpToExif` — reached
//! through the vendored `exiv2-oracle`. The corpus is real camera and editor files from exiv2's own
//! test data (`third_party/exiv2/test/data`): exiv2 reads each file's EXIF (and XMP, where it has
//! one), and both converters run over the same bytes.
//!
//! The comparison is over whole graphs, not a list of spot checks, so a row gamut maps that exiv2
//! does not — or the reverse — fails. What differs on purpose is stated once, below, and applied
//! as a narrow normalisation of exiv2's side:
//!
//! - **[`NOT_MAPPED`]**: the six exiv2 rows gamut leaves out (see the `exif_xmp` module docs).
//! - **Multi-valued arrays** ([`numeric_seq`]): CIPA DC-010 §4 maps a tag with more than one value
//!   to an ordered array with one entry per value; exiv2 writes one entry holding the values
//!   space-separated. exiv2's entries are split before comparing, so every value is still checked.
//! - **`SHORT` or `LONG`** ([`same_value`]): where DC-008 permits either type for a tag, gamut picks
//!   the smaller that holds the value and exiv2 always writes `LONG`; the values are compared as
//!   numbers.
//!
//! Requires the `third_party/exiv2` + `third_party/expat` submodules and a C++ toolchain.

use std::collections::BTreeMap;
use std::path::PathBuf;

use gamut_metadata::exif::{ByteOrder, Exif, ExifTag, FieldType, Ifd, IfdKind, Value};
use gamut_metadata::xmp::{XmpArray, XmpItem, XmpMeta, XmpProperty, XmpValue};
use gamut_metadata::{ExifConflictPolicy, exif_to_xmp, xmp_to_exif};

/// Files from exiv2's test data that carry EXIF: one per camera maker exiv2 ships a sample of,
/// files with GPS, a rating, and files whose own XMP holds `tiff:`/`exif:` properties.
const CORPUS: &[&str] = &[
    "exiv2-canon-eos-20d.jpg",
    "exiv2-canon-eos-300d.jpg",
    "exiv2-canon-eos-d30.jpg",
    "exiv2-canon-powershot-a520.jpg",
    "exiv2-canon-powershot-s40.jpg",
    "exiv2-fujifilm-finepix-s2pro.jpg",
    "exiv2-gc.jpg",
    "exiv2-kodak-dc210.jpg",
    "exiv2-nikon-d70.jpg",
    "exiv2-nikon-e950.jpg",
    "exiv2-nikon-e990.jpg",
    "exiv2-olympus-c8080wz.jpg",
    "exiv2-panasonic-dmc-fz5.jpg",
    "exiv2-sigma-d10.jpg",
    "exiv2-sony-dsc-w7.jpg",
    "exiv2-bug884a.jpg",
    "exiv2-bug884c.jpg",
    "exiv2-bug937.jpg",
    "exiv2-bug1062.jpg",
    "exiv2-bug922.png",
    "DSC_3079.jpg",
    "FurnaceCreekInn.jpg",
    "FurnaceCreekInn2.jpg",
    "FujiTagsDRangeAutoRating1.jpg",
    "IMGP0006-min.jpg",
    "imagemagick.png",
    "Reagan.tiff",
];

/// The exiv2 rows gamut does not map: DC-010 structures exiv2 flattens to byte lists, and the GPS
/// text tags exiv2 converts with their character-code prefix spelled into the text.
const NOT_MAPPED: &[&str] = &[
    "OECF",
    "SpatialFrequencyResponse",
    "CFAPattern",
    "DeviceSettingDescription",
    "GPSProcessingMethod",
    "GPSAreaInformation",
];

/// The XMP namespaces the mapping writes.
const NAMESPACES: &[&str] = &[
    "http://ns.adobe.com/tiff/1.0/",
    "http://ns.adobe.com/exif/1.0/",
    "http://ns.adobe.com/xap/1.0/",
    "http://purl.org/dc/elements/1.1/",
    "http://ns.adobe.com/photoshop/1.0/",
];

/// Reads a corpus file from exiv2's test data.
fn read(name: &str) -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../third_party/exiv2/test/data")
        .join(name);
    std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// The file's EXIF as exiv2 reads it (a bare TIFF stream).
fn exif_of(name: &str) -> Vec<u8> {
    exiv2_oracle::image_exif(&read(name))
        .unwrap()
        .unwrap_or_else(|| panic!("{name} carries no EXIF"))
}

/// Whether a property is one the comparison covers: in a mapped namespace and not left out.
fn covered(prop: &XmpProperty) -> bool {
    NAMESPACES.contains(&prop.namespace.as_str()) && !NOT_MAPPED.contains(&prop.name.as_str())
}

/// The numeric ordered arrays, whose exiv2 form is one space-separated entry.
fn numeric_seq(prop: &XmpProperty) -> bool {
    prop.namespace != "http://purl.org/dc/elements/1.1/"
        && matches!(prop.value, XmpValue::Array(XmpArray::Seq(_)))
}

/// exiv2's property with each numeric array entry split into one entry per value.
fn split_entries(mut prop: XmpProperty) -> XmpProperty {
    if numeric_seq(&prop)
        && let XmpValue::Array(XmpArray::Seq(items)) = &prop.value
    {
        let split = items
            .iter()
            .filter_map(XmpItem::text)
            .flat_map(str::split_whitespace)
            .map(XmpItem::simple)
            .collect();
        prop.value = XmpValue::Array(XmpArray::Seq(split));
    }
    prop
}

/// The covered properties of a graph, keyed by (namespace, name).
fn covered_props(xmp: XmpMeta) -> BTreeMap<(String, String), XmpProperty> {
    xmp.properties
        .into_iter()
        .filter(covered)
        .map(|p| ((p.namespace.clone(), p.name.clone()), p))
        .collect()
}

#[test]
fn exif_to_xmp_matches_exiv2_copy_exif_to_xmp_over_the_corpus() {
    let mut compared = 0;
    for name in CORPUS {
        let tiff = exif_of(name);
        let packet = exiv2_oracle::copy_exif_to_xmp(&tiff).unwrap();
        let theirs = XmpMeta::parse(&packet).unwrap();
        let theirs: BTreeMap<_, _> = covered_props(theirs)
            .into_iter()
            .map(|(k, p)| (k, split_entries(p)))
            .collect();

        let mut ours = XmpMeta::new();
        exif_to_xmp(
            &Exif::parse(&tiff).unwrap(),
            &mut ours,
            ExifConflictPolicy::ExifWins,
        );
        let ours = covered_props(ours);

        assert_eq!(
            ours.keys().collect::<Vec<_>>(),
            theirs.keys().collect::<Vec<_>>(),
            "{name}: the mapped property sets differ"
        );
        for (key, prop) in &ours {
            assert_eq!(prop.value, theirs[key].value, "{name}: {key:?}");
        }
        compared += ours.len();
    }
    assert!(compared > 500, "only {compared} properties compared");
}

/// The tags of the three directories the mapping writes, keyed by (directory, tag).
fn tags(exif: &Exif) -> BTreeMap<(u8, u16), Value> {
    let dirs = [
        (0, Some(exif.image())),
        (1, exif.exif_ifd()),
        (2, exif.gps_ifd()),
    ];
    dirs.into_iter()
        .filter_map(|(i, ifd)| ifd.map(|ifd: &Ifd| (i, ifd)))
        .flat_map(|(i, ifd)| {
            ifd.fields()
                .iter()
                .map(move |f| ((i, f.tag), f.value.clone()))
        })
        .collect()
}

/// The directory a [`tags`] key names.
fn kind(dir: u8) -> IfdKind {
    [IfdKind::Image, IfdKind::Exif, IfdKind::Gps][usize::from(dir)]
}

/// Equal values, or — where DC-008 lets the tag be `SHORT` or `LONG` — equal numbers.
fn same_value(dir: u8, tag: u16, ours: &Value, theirs: &Value) -> bool {
    let either = ExifTag::from_id(kind(dir), tag).is_some_and(|t| {
        t.field_types().contains(&FieldType::Short) && t.field_types().contains(&FieldType::Long)
    });
    match (ours, theirs) {
        (Value::Short(a), Value::Long(b)) if either => {
            a.iter().map(|&x| u32::from(x)).eq(b.iter().copied())
        }
        // Non-ASCII text: gamut writes Exif 3.0's `UTF8` type where the tag allows it, exiv2
        // writes the same UTF-8 bytes under `ASCII`.
        (Value::Utf8(a), Value::Ascii(b)) => a == b,
        // `SubSecTime*` from a fraction with leading zeros: exiv2 writes the nanosecond count as
        // a bare integer (`.06` → `60000000`, which reads back as .6 s); gamut keeps the zeros
        // (`060000000`).
        (Value::Ascii(a), Value::Ascii(b)) if dir == 1 && (0x9290..=0x9292).contains(&tag) => {
            a.trim_start_matches('0') == b
        }
        _ => ours == theirs,
    }
}

/// The [`NOT_MAPPED`] rows' tags, which exiv2 writes back and gamut does not.
const NOT_MAPPED_TAGS: &[(u8, u16)] = &[
    (1, 0x8828), // OECF
    (1, 0xA20C), // SpatialFrequencyResponse
    (1, 0xA302), // CFAPattern
    (1, 0xA40B), // DeviceSettingDescription
    (2, 0x001B), // GPSProcessingMethod
    (2, 0x001C), // GPSAreaInformation
];

/// A [`tags`] key: (directory, tag).
type Key = (u8, u16);

/// The date rows: (namespace, property) → the date tag and its `SubSecTime*` tag.
const DATES: &[(&str, &str, Key, Key)] = &[
    (
        "http://ns.adobe.com/xap/1.0/",
        "ModifyDate",
        (0, 0x0132),
        (1, 0x9290),
    ),
    (
        "http://ns.adobe.com/photoshop/1.0/",
        "DateCreated",
        (1, 0x9003),
        (1, 0x9291),
    ),
    (
        "http://ns.adobe.com/xap/1.0/",
        "CreateDate",
        (1, 0x9004),
        (1, 0x9292),
    ),
];

/// The tag a `tiff:`/`exif:` property maps to: the tag of the same name (`exif:ISOSpeedRatings`
/// is `PhotographicSensitivity`, the tag's Exif 2.3 name).
fn tag_of(prop: &XmpProperty) -> Option<(u8, u16)> {
    if !matches!(
        prop.namespace.as_str(),
        "http://ns.adobe.com/tiff/1.0/" | "http://ns.adobe.com/exif/1.0/"
    ) {
        return None;
    }
    let name = match prop.name.as_str() {
        "ISOSpeedRatings" => "PhotographicSensitivity",
        name => name,
    };
    let tag = ExifTag::ALL.iter().find(|t| t.name() == name)?;
    let dir = match tag.ifd() {
        IfdKind::Exif => 1,
        IfdKind::Gps => 2,
        _ => 0,
    };
    Some((dir, tag.tag_id()))
}

/// Whether an XMP date names a zone (`Z` or `±hh:mm` after the time).
fn has_zone(date: &str) -> bool {
    date.split_once('T')
        .is_some_and(|(_, time)| time.contains(['Z', '+', '-']))
}

/// Runs both converters over one packet and compares the EXIF they write.
///
/// A date property that names a zone is left out on both sides: exiv2 moves it into the zone of
/// the machine running it (`SXMPUtils::ConvertToLocalTime`), so its answer is not a property of
/// the input, while gamut keeps the wall-clock time the value states (see the module docs of
/// `exif_xmp`), which that module's own tests pin.
fn compare_xmp_to_exif(label: &str, packet: &[u8]) -> usize {
    let xmp = XmpMeta::parse(packet).unwrap();
    // exiv2 encodes an EXIF set with no tag as no bytes at all.
    let stream = exiv2_oracle::copy_xmp_to_exif(packet).unwrap();
    let mut theirs = if stream.is_empty() {
        BTreeMap::new()
    } else {
        tags(&Exif::parse(&stream).unwrap())
    };
    let mut ours = Exif::new(ByteOrder::LittleEndian);
    xmp_to_exif(&xmp, &mut ours, ExifConflictPolicy::XmpWins);
    let mut ours = tags(&ours);
    for key in NOT_MAPPED_TAGS {
        theirs.remove(key);
    }
    // exiv2's `ExifParser::encode` drops IFD0 `PhotometricInterpretation` from every stream it
    // writes (its `filteredIfd0Tags`), so the oracle cannot report that tag.
    ours.remove(&(0, 0x0106));
    // A multi-entry array (DC-010's form) reaches exiv2's numeric parser as `8, 8, 8`, which
    // stops at the first comma: exiv2 writes nothing. gamut reads one value per entry, which the
    // `exif_xmp` module's tests pin. `ComponentsConfiguration` is the exception on exiv2's side:
    // its row joins the entries with spaces, so it stays compared.
    for prop in &xmp.properties {
        if let XmpValue::Array(XmpArray::Seq(items)) = &prop.value
            && items.len() > 1
            && prop.name != "ComponentsConfiguration"
            && let Some(key) = tag_of(prop)
        {
            assert_eq!(theirs.get(&key), None, "{label}: exiv2 read {}", prop.name);
            ours.remove(&key);
        }
    }
    for (ns, name, date, subsec) in DATES {
        if xmp.get_text(ns, name).is_some_and(has_zone) {
            for key in [date, subsec] {
                theirs.remove(key);
                ours.remove(key);
            }
        }
    }
    assert_eq!(
        ours.keys().collect::<Vec<_>>(),
        theirs.keys().collect::<Vec<_>>(),
        "{label}: the written tag sets differ"
    );
    for (&(dir, tag), value) in &ours {
        assert!(
            same_value(dir, tag, value, &theirs[&(dir, tag)]),
            "{label}: tag {tag:#06x} in {:?}: ours {value:?}, exiv2 {:?}",
            kind(dir),
            theirs[&(dir, tag)]
        );
    }
    ours.len()
}

#[test]
fn xmp_to_exif_matches_exiv2_copy_xmp_to_exif_over_exiv2s_own_xmp() {
    // exiv2's XMP for every corpus file: the full mapped vocabulary, in exiv2's value forms.
    let mut compared = 0;
    for name in CORPUS {
        let packet = exiv2_oracle::copy_exif_to_xmp(&exif_of(name)).unwrap();
        compared += compare_xmp_to_exif(name, &packet);
    }
    assert!(compared > 500, "only {compared} tags compared");
}

#[test]
fn xmp_to_exif_matches_exiv2_copy_xmp_to_exif_over_the_files_own_xmp() {
    // The XMP other software wrote: DC-010's one-entry-per-value arrays, language alternatives
    // in several languages, dates with zones.
    let mut compared = 0;
    for name in CORPUS {
        if let Some(packet) = exiv2_oracle::image_xmp_packet(&read(name)).unwrap() {
            compared += compare_xmp_to_exif(name, &packet);
        }
    }
    assert!(compared > 0, "no file's own XMP mapped to a tag");
}
