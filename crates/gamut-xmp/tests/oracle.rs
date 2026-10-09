//! Differential conformance against exiv2's bundled Adobe XMP Toolkit (XMPCore), the engine the
//! crate documents as its oracle. These cross-checks complement the spec-derived golden vectors:
//! the golden tests pin the exact canonical bytes, while these prove gamut interoperates with the
//! reference implementation — its output is real XMP the toolkit reads, and it reads the toolkit's
//! output back. Equality is asserted on the re-parsed graph's values, not on exiv2's bytes (XMPCore
//! normalizes namespace and field ordering its own way).
//!
//! Requires the `third_party/exiv2` + `third_party/expat` submodules and a C++ toolchain.

use gamut_xmp::{
    Namespace, WellKnownNs, XmpArray, XmpItem, XmpMeta, XmpProperty, XmpValue, XmpWriter,
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
        assert_eq!(
            parsed.get_text(ns.uri(), "GamutCheck"),
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
