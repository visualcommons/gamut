//! Byte-exact golden vectors for the canonical serializer, derived from the Adobe XMP Part 1
//! examples. These assert the whole canonical document (namespace order, indentation, structure),
//! complementing the per-fragment unit tests in `writer.rs`. A serializer change that alters the
//! canonical form fails here.

use gamut_xmp::{XmpMeta, XmpProperty, XmpValue, XmpWriter};

const DC: &str = "http://purl.org/dc/elements/1.1/";
const XMP: &str = "http://ns.adobe.com/xap/1.0/";
const XMPTPG: &str = "http://ns.adobe.com/xap/1.0/t/pg/";
const STDIM: &str = "http://ns.adobe.com/xap/1.0/sType/Dimensions#";

fn simple(s: &str) -> XmpValue {
    XmpValue::Simple(s.into())
}

#[test]
fn language_alternative_canonical_body() {
    // The dc:title language-alternative example (Part 1 §8.2.2.4), in canonical form.
    let mut meta = XmpMeta::new();
    meta.set_lang_alt(DC, "title", "x-default", "XMP");
    meta.set_lang_alt(DC, "title", "en-us", "XMP");

    let expected = [
        "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\">",
        " <rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\" xmlns:dc=\"http://purl.org/dc/elements/1.1/\">",
        "  <rdf:Description rdf:about=\"\">",
        "   <dc:title>",
        "    <rdf:Alt>",
        "     <rdf:li xml:lang=\"x-default\">XMP</rdf:li>",
        "     <rdf:li xml:lang=\"en-us\">XMP</rdf:li>",
        "    </rdf:Alt>",
        "   </dc:title>",
        "  </rdf:Description>",
        " </rdf:RDF>",
        "</x:xmpmeta>",
    ]
    .join("\n");

    assert_eq!(meta.to_rdf(), expected);
}

#[test]
fn language_alternative_full_packet_read_only() {
    let mut meta = XmpMeta::new();
    meta.set_lang_alt(DC, "title", "x-default", "XMP");
    meta.set_lang_alt(DC, "title", "en-us", "XMP");

    let body = [
        "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\">",
        " <rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\" xmlns:dc=\"http://purl.org/dc/elements/1.1/\">",
        "  <rdf:Description rdf:about=\"\">",
        "   <dc:title>",
        "    <rdf:Alt>",
        "     <rdf:li xml:lang=\"x-default\">XMP</rdf:li>",
        "     <rdf:li xml:lang=\"en-us\">XMP</rdf:li>",
        "    </rdf:Alt>",
        "   </dc:title>",
        "  </rdf:Description>",
        " </rdf:RDF>",
        "</x:xmpmeta>",
    ]
    .join("\n");
    let expected = format!(
        "<?xpacket begin=\"\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\n{body}\n<?xpacket end=\"r\"?>"
    );

    let packet = XmpWriter::new().with_writable(false).serialize(&meta);
    assert_eq!(String::from_utf8(packet).unwrap(), expected);
}

#[test]
fn general_qualifier_canonical_body() {
    // A value carrying a non-xml:lang qualifier serializes with the rdf:value form (Part 1 §7.8),
    // nested deep enough that the indentation of each level is pinned exactly.
    let meta = XmpMeta {
        properties: vec![XmpProperty {
            namespace: DC.into(),
            name: "rights".into(),
            value: simple("(c)"),
            qualifiers: vec![XmpProperty::new(XMP, "owner", simple("Me"))],
        }],
    };

    let expected = [
        "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\">",
        " <rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\" xmlns:dc=\"http://purl.org/dc/elements/1.1/\" xmlns:xmp=\"http://ns.adobe.com/xap/1.0/\">",
        "  <rdf:Description rdf:about=\"\">",
        "   <dc:rights>",
        "    <rdf:Description>",
        "     <rdf:value>(c)</rdf:value>",
        "     <xmp:owner>Me</xmp:owner>",
        "    </rdf:Description>",
        "   </dc:rights>",
        "  </rdf:Description>",
        " </rdf:RDF>",
        "</x:xmpmeta>",
    ]
    .join("\n");

    assert_eq!(meta.to_rdf(), expected);
}

#[test]
fn structure_canonical_body() {
    // A structure value (Part 1 §7.6): xmpTPg:MaxPageSize built from the stDim:* fields. Namespaces
    // are declared rdf-first then alphabetically by prefix (stDim before xmpTPg).
    let meta = XmpMeta {
        properties: vec![XmpProperty::new(
            XMPTPG,
            "MaxPageSize",
            XmpValue::Structured(vec![
                XmpProperty::new(STDIM, "w", simple("4")),
                XmpProperty::new(STDIM, "h", simple("3")),
            ]),
        )],
    };

    let expected = [
        "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\">",
        " <rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\" xmlns:stDim=\"http://ns.adobe.com/xap/1.0/sType/Dimensions#\" xmlns:xmpTPg=\"http://ns.adobe.com/xap/1.0/t/pg/\">",
        "  <rdf:Description rdf:about=\"\">",
        "   <xmpTPg:MaxPageSize>",
        "    <rdf:Description>",
        "     <stDim:w>4</stDim:w>",
        "     <stDim:h>3</stDim:h>",
        "    </rdf:Description>",
        "   </xmpTPg:MaxPageSize>",
        "  </rdf:Description>",
        " </rdf:RDF>",
        "</x:xmpmeta>",
    ]
    .join("\n");

    assert_eq!(meta.to_rdf(), expected);
}
