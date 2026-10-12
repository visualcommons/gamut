//! Round-trip invariants between the reader and the canonical serializer.
//!
//! These exercise the two halves together (the per-module unit tests cover each half on its own):
//! the canonical form is a fixed point of parse∘serialize, a canonical graph survives a packet
//! round-trip unchanged, and the equivalent RDF/XML input forms XMP allows all parse to one graph.

use gamut_xmp::{Namespace, XmpArray, XmpItem, XmpMeta, XmpProperty, XmpValue, XmpWriter};

const RDF: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";
const DC: &str = "http://purl.org/dc/elements/1.1/";
const XMP: &str = "http://ns.adobe.com/xap/1.0/";

fn simple(s: &str) -> XmpValue {
    XmpValue::Simple(s.into())
}

/// A graph exercising every value form, built in canonical qualifier order (xml:lang first).
fn rich_meta() -> XmpMeta {
    let mut meta = XmpMeta::new();
    meta.set_text(XMP, "Rating", "3");
    meta.set(XmpProperty::new(
        XMP,
        "BaseURL",
        XmpValue::Uri("http://example.com/".into()),
    ));
    meta.set(XmpProperty::new(
        DC,
        "subject",
        XmpValue::Array(XmpArray::Bag(vec![
            XmpItem::new(simple("alpha")),
            XmpItem::new(simple("beta")),
        ])),
    ));
    meta.set(XmpProperty::new(
        DC,
        "creator",
        XmpValue::Array(XmpArray::Seq(vec![XmpItem::new(simple("Ada"))])),
    ));
    meta.set_lang_alt(DC, "title", "x-default", "Hello");
    meta.set_lang_alt(DC, "title", "fr", "Bonjour");
    meta.set(XmpProperty::new(
        XMP,
        "Thumbnail",
        XmpValue::Structured(vec![
            XmpProperty::new(XMP, "w", simple("9")),
            XmpProperty::new(XMP, "h", simple("6")),
        ]),
    ));
    // A value carrying a general (non-xml:lang) qualifier.
    meta.set(XmpProperty {
        namespace: DC.into(),
        name: "rights".into(),
        value: simple("(c) Example"),
        qualifiers: vec![XmpProperty::new(XMP, "owner", simple("Example Corp"))],
    });
    meta
}

#[test]
fn canonical_serialization_is_idempotent() {
    // Re-serializing a parsed canonical document reproduces it byte-for-byte — for any graph,
    // including ones whose qualifiers start out in a non-canonical order.
    let messy = XmpProperty {
        namespace: DC.into(),
        name: "rights".into(),
        value: simple("v"),
        // general qualifier before the xml:lang one — the serializer normalizes the order.
        qualifiers: vec![
            XmpProperty::new(XMP, "owner", simple("me")),
            XmpProperty::new("http://www.w3.org/XML/1998/namespace", "lang", simple("en")),
        ],
    };
    for meta in [
        rich_meta(),
        XmpMeta {
            properties: vec![messy],
        },
        XmpMeta::new(),
    ] {
        let once = meta.to_rdf();
        let twice = XmpMeta::from_packet(once.as_bytes())
            .expect("reparse")
            .to_rdf();
        assert_eq!(once, twice, "canonical form must be a fixed point");
    }
}

#[test]
fn canonical_graph_survives_packet_roundtrip() {
    let meta = rich_meta();
    let packet = meta.to_packet();
    let parsed = XmpMeta::from_packet(&packet).expect("parse packet");
    assert_eq!(parsed, meta);
}

#[test]
fn control_characters_survive_roundtrip() {
    // XML 1.0 normalizes a literal CR in text (to LF) and literal TAB/LF/CR in attribute values
    // (to spaces) on every parse, so the serializer must emit them as character references or the
    // fixed point silently corrupts data. Exercises both sinks: element text (Simple) and an
    // attribute value (Uri → rdf:resource).
    let mut meta = XmpMeta::new();
    meta.set_text(DC, "description", "line1\r\nline2\ttab\rbare");
    meta.set(XmpProperty::new(
        XMP,
        "BaseURL",
        XmpValue::Uri("http://example.com/a\tb\nc\rd".into()),
    ));
    let parsed = XmpMeta::from_packet(&meta.to_packet()).expect("reparse");
    assert_eq!(parsed, meta);
}

#[test]
fn unwritable_local_names_leave_a_parseable_packet() {
    // A node whose local name is not an XML NCName is dropped by the writer, at every depth the
    // name appears (property, structure field, qualifier), so the packet stays well-formed and
    // parses back to the graph without those nodes.
    for bad in ["", "has space", "1digit", "a:b"] {
        let mut meta = rich_meta();
        meta.properties
            .push(XmpProperty::new(DC, bad, simple("dropped")));
        meta.set(XmpProperty::new(
            XMP,
            "Thumbnail",
            XmpValue::Structured(vec![
                XmpProperty::new(XMP, "w", simple("9")),
                XmpProperty::new(XMP, bad, simple("dropped")),
                XmpProperty::new(XMP, "h", simple("6")),
            ]),
        ));
        meta.set(XmpProperty {
            namespace: DC.into(),
            name: "rights".into(),
            value: simple("(c) Example"),
            qualifiers: vec![
                XmpProperty::new(XMP, "owner", simple("Example Corp")),
                XmpProperty::new(XMP, bad, simple("dropped")),
            ],
        });
        let parsed = XmpMeta::from_packet(&meta.to_packet())
            .unwrap_or_else(|e| panic!("name {bad:?} broke the packet: {e}"));
        assert_eq!(parsed, rich_meta(), "name {bad:?}");
    }
}

#[test]
fn registered_prefix_is_not_semantic() {
    // A prefix registered on the writer changes only the serialization cosmetics: parsing the
    // output yields exactly the same graph as parsing the default (synthesized-prefix) output.
    let mut meta = XmpMeta::new();
    meta.set_text("http://example.com/vocab/", "kind", "demo");
    meta.set_text(DC, "format", "text/plain");

    let registered = XmpWriter::new()
        .with_namespace(Namespace::new("http://example.com/vocab/", "vocab"))
        .serialize(&meta);
    let parsed = XmpMeta::from_packet(&registered).expect("parse registered-prefix packet");
    assert_eq!(parsed, meta);
    assert_eq!(
        parsed,
        XmpMeta::from_packet(&meta.to_packet()).expect("default")
    );
}

#[test]
fn equivalent_input_forms_parse_to_the_same_graph() {
    // Element form vs. attribute form for a simple property (Part 1 §7.9.2.2).
    let element = format!(
        "<rdf:RDF xmlns:rdf=\"{RDF}\" xmlns:xmp=\"{XMP}\">\
         <rdf:Description rdf:about=\"\"><xmp:Rating>3</xmp:Rating></rdf:Description></rdf:RDF>"
    );
    let attribute = format!(
        "<rdf:RDF xmlns:rdf=\"{RDF}\" xmlns:xmp=\"{XMP}\">\
         <rdf:Description rdf:about=\"\" xmp:Rating=\"3\"/></rdf:RDF>"
    );
    assert_eq!(
        XmpMeta::from_packet(element.as_bytes()).unwrap(),
        XmpMeta::from_packet(attribute.as_bytes()).unwrap()
    );

    // Nested rdf:Description vs. rdf:parseType="Resource" for a structure (Part 1 §7.9.2.3).
    let nested = format!(
        "<rdf:RDF xmlns:rdf=\"{RDF}\" xmlns:xmp=\"{XMP}\"><rdf:Description rdf:about=\"\">\
         <xmp:T><rdf:Description><xmp:w>9</xmp:w></rdf:Description></xmp:T>\
         </rdf:Description></rdf:RDF>"
    );
    let concise = format!(
        "<rdf:RDF xmlns:rdf=\"{RDF}\" xmlns:xmp=\"{XMP}\"><rdf:Description rdf:about=\"\">\
         <xmp:T rdf:parseType=\"Resource\"><xmp:w>9</xmp:w></xmp:T>\
         </rdf:Description></rdf:RDF>"
    );
    assert_eq!(
        XmpMeta::from_packet(nested.as_bytes()).unwrap(),
        XmpMeta::from_packet(concise.as_bytes()).unwrap()
    );
}
