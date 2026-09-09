//! Differential conformance against exiv2's bundled Adobe XMP Toolkit (XMPCore), the engine the
//! crate documents as its oracle. These cross-checks complement the spec-derived golden vectors:
//! the golden tests pin the exact canonical bytes, while these prove gamut interoperates with the
//! reference implementation — its output is real XMP the toolkit reads, and it reads the toolkit's
//! output back. Equality is asserted on the re-parsed graph's values, not on exiv2's bytes (XMPCore
//! normalizes namespace and field ordering its own way).
//!
//! Requires the `third_party/exiv2` + `third_party/expat` submodules and a C++ toolchain.

use gamut_xmp::{
    Namespace, WellKnownNs, XmpArray, XmpItem, XmpMeta, XmpProperty, XmpSidecar, XmpValue,
    XmpWriter,
};

const RDF: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";
const DC: &str = "http://purl.org/dc/elements/1.1/";
const XMP: &str = "http://ns.adobe.com/xap/1.0/";

/// A graph covering simple, URI, Bag, and language-alternative values.
fn sample() -> XmpMeta {
    let mut meta = XmpMeta::new();
    meta.set_text(DC, "format", "text/plain");
    meta.set(XmpProperty::new(
        XMP,
        "BaseURL",
        XmpValue::Uri("http://example.com/".into()),
    ));
    meta.set(XmpProperty::new(
        DC,
        "subject",
        XmpValue::Array(XmpArray::Bag(vec![
            XmpItem::new(XmpValue::Simple("alpha".into())),
            XmpItem::new(XmpValue::Simple("beta".into())),
        ])),
    ));
    meta.set_lang_alt(DC, "title", "x-default", "Hello");
    meta
}

#[test]
fn gamut_output_is_valid_and_readable_by_exiv2() {
    let packet = sample().to_packet();
    exiv2_oracle::validate(&packet).expect("exiv2 (Adobe XMPCore) must accept gamut's packet");

    // Specific values survive into the reference engine.
    assert_eq!(
        exiv2_oracle::get_property(&packet, "Xmp.dc.format").unwrap(),
        "text/plain"
    );
    assert_eq!(
        exiv2_oracle::get_property(&packet, "Xmp.xmp.BaseURL").unwrap(),
        "http://example.com/"
    );
    // The bag items survive as a single keyed entry plus its members.
    assert!(exiv2_oracle::property_count(&packet).unwrap() >= 3);
}

#[test]
fn gamut_reads_exiv2_reserialization() {
    // exiv2 re-serializes via XMPCore; gamut must parse that back to the same values.
    let canonical = exiv2_oracle::roundtrip(&sample().to_packet()).expect("exiv2 round-trip");
    let parsed = XmpMeta::from_packet(&canonical).expect("gamut parses exiv2's output");

    assert_eq!(parsed.get_text(DC, "format"), Some("text/plain"));
    // The URL's value survives the round-trip; exiv2 normalizes the URI form to element text, so we
    // assert on the data rather than the `Uri` vs `Simple` form.
    assert_eq!(parsed.get_text(XMP, "BaseURL"), Some("http://example.com/"));
    assert_eq!(parsed.get_lang_alt(DC, "title", "x-default"), Some("Hello"));

    let XmpValue::Array(XmpArray::Bag(items)) = &parsed.get(DC, "subject").unwrap().value else {
        panic!("dc:subject must round-trip through exiv2 as a Bag");
    };
    let values: Vec<&str> = items.iter().filter_map(XmpItem::text).collect();
    assert_eq!(values, ["alpha", "beta"]);
}

#[test]
fn empty_packet_round_trips_through_exiv2() {
    // A property-less packet is still valid XMP the toolkit accepts.
    let packet = XmpMeta::new().to_packet();
    exiv2_oracle::validate(&packet).expect("exiv2 must accept an empty packet");
}

/// The URI under which the reference engine re-serializes a schema.
///
/// An oracle limitation, pinned rather than hidden: exiv2 normalizes every namespace URI it
/// registers with XMPCore by appending `/` when the URI ends in neither `/` nor `#`
/// (`third_party/exiv2/src/properties.cpp`, `XmpProperties::registerNs` and
/// `XmpProperties::prefix`, "`if (ns2.back() != '/' && ns2.back() != '#') ns2 += '/'`"). Reading
/// is unaffected — the key lookup resolves through the same normalization, so a `dwc` property
/// reads back by its `Xmp.dwc.*` key — but XMPCore's output then declares the normalized URI.
/// Darwin Core (`http://rs.tdwg.org/dwc/index.htm`) is the only registered schema this touches;
/// gamut writes the URI exactly as exiv2 documents and registers it (`xmp.cpp:519`).
fn xmpcore_output_uri(ns: WellKnownNs) -> String {
    let uri = ns.uri();
    if uri.ends_with('/') || uri.ends_with('#') {
        uri.to_owned()
    } else {
        format!("{uri}/")
    }
}

#[test]
fn every_well_known_namespace_survives_xmpcore() {
    // One property in each WellKnownNs, its value naming the expected prefix. XMPCore's own
    // schema registry then vouches for every URI string — a typo in any URI would surface as a
    // rejected packet or a lost property (the unit tests can only assert the strings against
    // themselves).
    let mut meta = XmpMeta::new();
    for ns in WellKnownNs::ALL {
        meta.set(XmpProperty::new(
            ns.uri(),
            "GamutCheck",
            XmpValue::Simple(ns.prefix().to_owned()),
        ));
    }
    let packet = meta.to_packet();
    exiv2_oracle::validate(&packet).expect("exiv2 must accept all standard namespaces");

    let out = exiv2_oracle::roundtrip(&packet).expect("exiv2 round-trip");
    let parsed = XmpMeta::from_packet(&out).expect("gamut parses exiv2's output");
    for ns in WellKnownNs::ALL {
        // `xmpcore_output_uri` is the identity for every schema but Darwin Core (see its doc).
        assert_eq!(
            parsed.get_text(&xmpcore_output_uri(*ns), "GamutCheck"),
            Some(ns.prefix()),
            "property in {ns:?} must survive the reference engine"
        );
    }
}

#[test]
fn dcterms_provenance_reads_back_from_xmpcore_under_the_dcterms_key() {
    // C2PA 2.4 §11.5 / §15.5.3.1: the pointer to an *external* manifest store is
    // `dcterms:provenance`, "a URI reference". Registering the DCMI Terms namespace is what makes
    // gamut serialize it under the `dcterms` prefix exiv2's own schema registry knows, so the
    // reference engine reads the property by the key a validator would look for — not under a
    // synthesized `ns1` — and hands the URL back unchanged.
    let url = "https://example.com/manifests/photo.c2pa";
    let dcterms = WellKnownNs::DcTerms.uri();
    let mut meta = XmpMeta::new();
    meta.set(XmpProperty::new(
        dcterms,
        "provenance",
        XmpValue::Uri(url.into()),
    ));
    let packet = meta.to_packet();
    assert!(
        std::str::from_utf8(&packet)
            .unwrap()
            .contains("xmlns:dcterms=\"http://purl.org/dc/terms/\""),
        "the registered prefix must be the one serialized"
    );

    exiv2_oracle::validate(&packet).expect("exiv2 (Adobe XMPCore) must accept the packet");
    assert_eq!(
        exiv2_oracle::get_property(&packet, "Xmp.dcterms.provenance").unwrap(),
        url
    );

    let out = exiv2_oracle::roundtrip(&packet).expect("exiv2 round-trip");
    let parsed = XmpMeta::from_packet(&out).expect("gamut parses exiv2's output");
    assert_eq!(parsed.get_text(dcterms, "provenance"), Some(url));
}

#[test]
fn registered_prefix_packet_is_valid_for_xmpcore() {
    // A packet serialized under a registered custom prefix is real XMP to the reference engine,
    // and the property survives its round-trip.
    let custom = "http://example.com/vocab/";
    let mut meta = XmpMeta::new();
    meta.set_text(custom, "kind", "demo");
    let packet = XmpWriter::new()
        .with_namespace(Namespace::new(custom, "vocab"))
        .serialize(&meta);

    exiv2_oracle::validate(&packet).expect("exiv2 must accept a registered-prefix packet");
    let out = exiv2_oracle::roundtrip(&packet).expect("exiv2 round-trip");
    let parsed = XmpMeta::from_packet(&out).expect("gamut parses exiv2's output");
    assert_eq!(parsed.get_text(custom, "kind"), Some("demo"));
}

#[test]
fn default_xml_lang_on_description_matches_reference() {
    // Part 1 §7.8 notes xml:lang scopes per XML 1.0, but Adobe XMPCore does not materialize a
    // Description-level default onto the properties it scopes — and neither does gamut. The
    // parity is pinned here; the intentional skip is documented in README/STATUS.
    let xml = format!(
        "<rdf:RDF xmlns:rdf=\"{RDF}\" xmlns:dc=\"{DC}\">\
         <rdf:Description rdf:about=\"\" xml:lang=\"fr\">\
         <dc:format>text/plain</dc:format>\
         <dc:coverage xml:lang=\"de\">hier</dc:coverage>\
         </rdf:Description></rdf:RDF>"
    );

    // gamut reads dc:format unqualified…
    let gamut = XmpMeta::from_packet(xml.as_bytes()).expect("gamut parses the packet");
    let prop = gamut.get(DC, "format").expect("dc:format");
    assert_eq!(prop.text(), Some("text/plain"));
    assert!(
        prop.qualifiers.is_empty(),
        "no inherited xml:lang qualifier"
    );

    // …and so does the reference engine: its reserialization carries no per-property lang.
    let out = exiv2_oracle::roundtrip(xml.as_bytes()).expect("exiv2 round-trip");
    let reference = XmpMeta::from_packet(&out).expect("gamut parses exiv2's output");
    let ref_prop = reference.get(DC, "format").expect("dc:format via exiv2");
    assert_eq!(ref_prop.text(), Some("text/plain"));
    assert!(
        ref_prop.qualifiers.is_empty(),
        "XMPCore must not have materialized the default lang: {:?}",
        ref_prop.qualifiers
    );

    // An explicit per-property xml:lang is preserved by both.
    assert_eq!(
        gamut.get(DC, "coverage").and_then(XmpProperty::lang),
        Some("de")
    );
    assert_eq!(
        reference.get(DC, "coverage").and_then(XmpProperty::lang),
        Some("de")
    );
}

// ---------------------------------------------------------------------------------------------------
// Schema breadth (issue #421): one test per namespace added for exiv2 parity.
//
// Each writes one *documented* property of the schema (a name from exiv2's own property table for
// that namespace) and reads it back from XMPCore by its exiv2 key, `Xmp.<prefix>.<name>`. The key
// is what makes this differential rather than self-consistent: exiv2 resolves the key's prefix
// through its registry, so a URI XMPCore does not know, or a prefix that is not the one exiv2
// binds to that URI, fails the lookup — the unit test in `namespace.rs` can only compare the
// strings with themselves.
// ---------------------------------------------------------------------------------------------------

/// Serializes `meta`, asserts XMPCore accepts it, and returns the packet.
fn packet_xmpcore_accepts(meta: &XmpMeta) -> Vec<u8> {
    let packet = meta.to_packet();
    exiv2_oracle::validate(&packet).expect("exiv2 (Adobe XMPCore) must accept gamut's packet");
    packet
}

/// A graph holding one simple text property in `ns`.
fn one_text(ns: WellKnownNs, name: &str, value: &str) -> XmpMeta {
    let mut meta = XmpMeta::new();
    meta.set_text(ns.uri(), name, value);
    meta
}

/// Asserts a simple text property in `ns` is keyed `Xmp.<prefix>.<name>` by XMPCore and survives
/// its re-serialization back into gamut.
fn text_property_survives_xmpcore(ns: WellKnownNs, name: &str, value: &str) {
    let packet = packet_xmpcore_accepts(&one_text(ns, name, value));
    let key = format!("Xmp.{}.{name}", ns.prefix());
    assert_eq!(
        exiv2_oracle::get_property(&packet, &key).unwrap(),
        value,
        "{key} must read back from the reference engine"
    );
    let out = exiv2_oracle::roundtrip(&packet).expect("exiv2 round-trip");
    let parsed = XmpMeta::from_packet(&out).expect("gamut parses exiv2's output");
    assert_eq!(parsed.get_text(ns.uri(), name), Some(value));
}

#[test]
fn exif_ex_lens_model_reads_back_under_the_exif_ex_key() {
    text_property_survives_xmpcore(WellKnownNs::ExifEx, "LensModel", "GAMUT 50mm F1.4");
}

#[test]
fn aux_lens_reads_back_under_the_aux_key() {
    text_property_survives_xmpcore(WellKnownNs::Aux, "Lens", "50.0 mm f/1.4");
}

#[test]
fn plus_version_reads_back_under_the_plus_key() {
    text_property_survives_xmpcore(WellKnownNs::Plus, "Version", "1.2.0");
}

#[test]
fn gpano_projection_type_reads_back_under_the_gpano_key() {
    text_property_survives_xmpcore(WellKnownNs::GPano, "ProjectionType", "equirectangular");
}

#[test]
fn microsoft_photo_rating_reads_back_under_the_microsoft_photo_key() {
    text_property_survives_xmpcore(WellKnownNs::MicrosoftPhoto, "Rating", "75");
}

#[test]
fn digikam_color_label_reads_back_under_the_digikam_key() {
    text_property_survives_xmpcore(WellKnownNs::DigiKam, "ColorLabel", "3");
}

#[test]
fn acdsee_caption_reads_back_under_the_acdsee_key() {
    text_property_survives_xmpcore(WellKnownNs::Acdsee, "caption", "Harbour at dusk");
}

#[test]
fn lightroom_hierarchical_subject_reads_back_under_the_lr_key() {
    // `lr:hierarchicalSubject` is the property the schema exists for: a Bag of `|`-separated
    // keyword paths.
    let lr = WellKnownNs::Lightroom;
    let mut meta = XmpMeta::new();
    meta.set(XmpProperty::new(
        lr.uri(),
        "hierarchicalSubject",
        XmpValue::Array(XmpArray::Bag(vec![XmpItem::new(XmpValue::Simple(
            "Places|Seoul".into(),
        ))])),
    ));
    let packet = packet_xmpcore_accepts(&meta);
    assert_eq!(
        exiv2_oracle::get_property(&packet, "Xmp.lr.hierarchicalSubject").unwrap(),
        "Places|Seoul"
    );
    let out = exiv2_oracle::roundtrip(&packet).expect("exiv2 round-trip");
    let parsed = XmpMeta::from_packet(&out).expect("gamut parses exiv2's output");
    let XmpValue::Array(XmpArray::Bag(items)) =
        &parsed.get(lr.uri(), "hierarchicalSubject").unwrap().value
    else {
        panic!("lr:hierarchicalSubject must round-trip as a Bag");
    };
    let values: Vec<&str> = items.iter().filter_map(XmpItem::text).collect();
    assert_eq!(values, ["Places|Seoul"]);
}

/// A graph whose one property in `ns` is the structure `name` holding the text fields `fields`
/// (same namespace) — the shape of the four schemas whose top-level properties are all structures.
fn one_struct(ns: WellKnownNs, name: &str, fields: &[(&str, &str)]) -> XmpMeta {
    let mut meta = XmpMeta::new();
    meta.set(XmpProperty::new(
        ns.uri(),
        name,
        XmpValue::Structured(
            fields
                .iter()
                .map(|(field, value)| {
                    XmpProperty::new(ns.uri(), *field, XmpValue::Simple((*value).into()))
                })
                .collect(),
        ),
    ));
    meta
}

/// Asserts the structure field `Xmp.<prefix>.<name>/<prefix>:<field>` reads back from XMPCore and
/// survives its re-serialization.
fn struct_field_survives_xmpcore(ns: WellKnownNs, name: &str, field: &str, value: &str) {
    let packet = packet_xmpcore_accepts(&one_struct(ns, name, &[(field, value)]));
    let prefix = ns.prefix();
    let key = format!("Xmp.{prefix}.{name}/{prefix}:{field}");
    assert_eq!(
        exiv2_oracle::get_property(&packet, &key).unwrap(),
        value,
        "{key} must read back from the reference engine"
    );
    let out = exiv2_oracle::roundtrip(&packet).expect("exiv2 round-trip");
    let parsed = XmpMeta::from_packet(&out).expect("gamut parses exiv2's output");
    // The reference engine's output URI (differs from `ns.uri()` only for Darwin Core).
    let out_uri = xmpcore_output_uri(ns);
    let XmpValue::Structured(fields) = &parsed.get(&out_uri, name).unwrap().value else {
        panic!("{prefix}:{name} must round-trip as a structure");
    };
    let got = fields
        .iter()
        .find(|f| f.namespace == out_uri && f.name == field)
        .and_then(XmpProperty::text);
    assert_eq!(got, Some(value));
}

/// Asserts the MWG shape — a top-level structure `name` whose field `list` is a Bag of structures
/// each carrying the text `field` — reads back from XMPCore as
/// `Xmp.<prefix>.<name>/<prefix>:<list>[1]/<prefix>:<field>` and survives its re-serialization.
fn struct_bag_item_field_survives_xmpcore(
    ns: WellKnownNs,
    name: &str,
    list: &str,
    field: &str,
    value: &str,
) {
    let uri = ns.uri();
    let item = XmpItem::new(XmpValue::Structured(vec![XmpProperty::new(
        uri,
        field,
        XmpValue::Simple(value.into()),
    )]));
    let mut meta = XmpMeta::new();
    meta.set(XmpProperty::new(
        uri,
        name,
        XmpValue::Structured(vec![XmpProperty::new(
            uri,
            list,
            XmpValue::Array(XmpArray::Bag(vec![item])),
        )]),
    ));
    let packet = packet_xmpcore_accepts(&meta);
    let prefix = ns.prefix();
    let key = format!("Xmp.{prefix}.{name}/{prefix}:{list}[1]/{prefix}:{field}");
    assert_eq!(
        exiv2_oracle::get_property(&packet, &key).unwrap(),
        value,
        "{key} must read back from the reference engine"
    );
    let out = exiv2_oracle::roundtrip(&packet).expect("exiv2 round-trip");
    let parsed = XmpMeta::from_packet(&out).expect("gamut parses exiv2's output");
    // Compare under the engine's output URI (the identity for both MWG schemas; see
    // `xmpcore_output_uri`), so the helper stays correct for a `dwc`-style URI too.
    let out_uri = xmpcore_output_uri(ns);
    let mut expected = meta.get(uri, name).cloned().expect("the property just set");
    rename_namespace(&mut expected, uri, &out_uri);
    assert_eq!(
        parsed.get(&out_uri, name),
        Some(&expected),
        "{prefix}:{name} must round-trip through XMPCore unchanged"
    );
}

/// Rewrites every occurrence of namespace `from` in `property` (itself, its fields, items and
/// qualifiers) to `to`.
fn rename_namespace(property: &mut XmpProperty, from: &str, to: &str) {
    if property.namespace == from {
        property.namespace = to.to_owned();
    }
    for qualifier in &mut property.qualifiers {
        rename_namespace(qualifier, from, to);
    }
    rename_namespace_in_value(&mut property.value, from, to);
}

/// The value half of [`rename_namespace`].
fn rename_namespace_in_value(value: &mut XmpValue, from: &str, to: &str) {
    match value {
        XmpValue::Simple(_) | XmpValue::Uri(_) => {}
        XmpValue::Structured(fields) => {
            for field in fields {
                rename_namespace(field, from, to);
            }
        }
        XmpValue::Array(array) => {
            for item in array.items_mut() {
                rename_namespace_in_value(&mut item.value, from, to);
                for qualifier in &mut item.qualifiers {
                    rename_namespace(qualifier, from, to);
                }
            }
        }
    }
}

#[test]
fn mwg_regions_list_item_name_reads_back_under_the_mwg_rs_key() {
    // MWG Guidelines 2.0, Regions: `mwg-rs:Regions` is a RegionInfo structure whose `RegionList`
    // is a Bag of RegionStruct, each with a text `Name`. A hyphenated prefix is also the one
    // shape the writer's prefix table had not exercised before.
    struct_bag_item_field_survives_xmpcore(
        WellKnownNs::MwgRegions,
        "Regions",
        "RegionList",
        "Name",
        "Face 1",
    );
}

#[test]
fn darwin_core_nested_bag_survives_the_uri_xmpcore_normalizes() {
    // The only registered schema whose URI XMPCore rewrites (see `xmpcore_output_uri`), driven
    // through the nested helper so the expected graph is re-namespaced with `from != to` — the
    // MWG callers both pass a URI the engine leaves alone, which would leave that normalization
    // unexercised. The shape is the helper's bag-of-structure rather than the `bag Text` exiv2
    // declares for dwc's bags: this crate is a namespace registry, not a validator, and XMPCore
    // stores the shape the packet carries; what is under test here is the URI, not the type.
    struct_bag_item_field_survives_xmpcore(
        WellKnownNs::DarwinCore,
        "Record",
        "dynamicProperties",
        "institutionID",
        "GAMUT",
    );
}

#[test]
fn mwg_keywords_hierarchy_item_keyword_reads_back_under_the_mwg_kw_key() {
    // MWG Guidelines 2.0, Keywords: `mwg-kw:Keywords` is a KeywordInfo structure whose
    // `Hierarchy` is a Bag of KeywordStruct, each with a text `Keyword`.
    struct_bag_item_field_survives_xmpcore(
        WellKnownNs::MwgKeywords,
        "Keywords",
        "Hierarchy",
        "Keyword",
        "Seoul",
    );
}

#[test]
fn camera_raw_saved_settings_name_reads_back_under_the_crss_key() {
    struct_field_survives_xmpcore(
        WellKnownNs::CameraRawSavedSettings,
        "SavedSettings",
        "Name",
        "Import",
    );
}

#[test]
fn darwin_core_record_field_reads_back_under_the_dwc_key() {
    // The one schema whose URI ends in neither `/` nor `#`. gamut writes it exactly as exiv2
    // documents it; the reference engine keys it correctly (`Xmp.dwc.*` below) and re-serializes
    // it with a `/` appended (`xmpcore_output_uri`) — an oracle normalization, not a defect in
    // either direction, so both halves are pinned here.
    let dwc = WellKnownNs::DarwinCore;
    let packet = one_struct(dwc, "Record", &[("institutionID", "GAMUT")]).to_packet();
    assert!(
        std::str::from_utf8(&packet)
            .unwrap()
            .contains("xmlns:dwc=\"http://rs.tdwg.org/dwc/index.htm\""),
        "gamut must declare the URI exiv2 documents, unslashed"
    );
    assert_eq!(xmpcore_output_uri(dwc), "http://rs.tdwg.org/dwc/index.htm/");
    struct_field_survives_xmpcore(dwc, "Record", "institutionID", "GAMUT");
}

// ---------------------------------------------------------------------------------------------------
// Sidecars (issue #421): the file gamut writes is XMP to the reference engine, and the file the
// reference engine writes is a sidecar to gamut.
// ---------------------------------------------------------------------------------------------------

#[test]
fn sidecar_written_by_gamut_is_read_by_xmpcore() {
    // The whole file — XML declaration, read-only packet wrapper, x:xmpmeta — is handed to XMPCore
    // as-is, the way exiv2's sidecar reader passes a .xmp file's bytes to the parser.
    let file = XmpSidecar::write(&sample());
    exiv2_oracle::validate(&file).expect("exiv2 (Adobe XMPCore) must accept gamut's sidecar");
    assert_eq!(
        exiv2_oracle::get_property(&file, "Xmp.dc.format").unwrap(),
        "text/plain"
    );
    assert_eq!(
        exiv2_oracle::get_property(&file, "Xmp.xmp.BaseURL").unwrap(),
        "http://example.com/"
    );
}

#[test]
fn gamut_reads_the_sidecar_xmpcore_writes() {
    // XMPCore's default serialization is the `<?xpacket?>` + `<x:xmpmeta>` form exiv2 stores in a
    // .xmp file (its sidecar writer only prepends the packet header when the packet lacks one), so
    // the reference engine's output is exactly what a sidecar on disk looks like.
    let reference = exiv2_oracle::roundtrip(&sample().to_packet()).expect("exiv2 round-trip");
    assert!(
        String::from_utf8_lossy(&reference).contains("<x:xmpmeta"),
        "precondition: XMPCore's serialization carries the wrapper a sidecar requires"
    );
    let parsed = XmpSidecar::read(&reference).expect("gamut reads XMPCore's sidecar");
    assert_eq!(parsed.get_text(DC, "format"), Some("text/plain"));
    assert_eq!(parsed.get_lang_alt(DC, "title", "x-default"), Some("Hello"));
}
