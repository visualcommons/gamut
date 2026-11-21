//! The reader's element-nesting bound (issue #1260): at most 256 elements open at once, the
//! outermost counting as one, so a hostile packet is refused with a typed error rather than
//! overflowing the stack or `quick-xml`'s `u16` nesting counter.

use gamut_xmp::{XmpError, XmpMeta};

/// The bound `XmpError::NestingTooDeep` documents.
const LIMIT: usize = 256;

/// The issue's reproducer: `x:xmpmeta` > `rdf:RDF` > `rdf:Description` > `a:p`, then `nested`
/// further `rdf:parseType="Resource"` structures, so the deepest element sits at `4 + nested`.
fn nested_resources(nested: usize) -> String {
    let head = "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"><rdf:RDF \
                xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\" xmlns:a=\"urn:a\">\
                <rdf:Description rdf:about=\"\"><a:p rdf:parseType=\"Resource\">";
    let tail = "</a:p></rdf:Description></rdf:RDF></x:xmpmeta>";
    format!(
        "{head}{}{}{tail}",
        "<a:q rdf:parseType=\"Resource\">".repeat(nested),
        "</a:q>".repeat(nested)
    )
}

/// `depth` plain `<a>` elements, each inside the last.
fn nested_plain(depth: usize) -> String {
    format!("{}{}", "<a>".repeat(depth), "</a>".repeat(depth))
}

#[test]
fn nested_resources_at_the_bound_parse() {
    let meta = XmpMeta::parse(nested_resources(LIMIT - 4).as_bytes());
    assert!(meta.is_ok(), "{meta:?}");
}

#[test]
fn nested_resources_one_past_the_bound_are_refused() {
    assert_eq!(
        XmpMeta::parse(nested_resources(LIMIT - 3).as_bytes()),
        Err(XmpError::NestingTooDeep(LIMIT))
    );
}

#[test]
fn plain_elements_at_the_bound_pass_the_depth_check() {
    // No `rdf:RDF`, so the packet still fails — but only after the whole tree was built.
    assert_eq!(
        XmpMeta::parse(nested_plain(LIMIT).as_bytes()),
        Err(XmpError::MissingRdf)
    );
}

#[test]
fn plain_elements_past_quick_xmls_counter_are_refused() {
    // 70 000 open elements would overflow quick-xml's `u16` nesting level (a panic with overflow
    // checks, silent namespace-scope corruption without); the bound stops the read long before.
    assert_eq!(
        XmpMeta::parse(nested_plain(70_000).as_bytes()),
        Err(XmpError::NestingTooDeep(LIMIT))
    );
}
