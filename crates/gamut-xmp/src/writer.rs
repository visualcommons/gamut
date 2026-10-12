//! The XMP writer: an [`XmpMeta`] graph → **canonical RDF/XML** (Adobe XMP Part 1 §7).
//!
//! RDF admits many serializations of one graph; the canonical form fixes a single shape so output
//! is stable, diffable, and round-trippable. This serializer locks every choice §7.9 leaves open:
//!
//! - one `<rdf:Description rdf:about="">` holding all properties;
//! - simple values as element text, URI values via `rdf:resource`, structures as a nested
//!   `rdf:Description`, arrays as `rdf:Bag`/`Seq`/`Alt` of `rdf:li` (never the attribute shorthands,
//!   `rdf:parseType="Resource"`, or `rdf:_n` items);
//! - `xml:lang` as an attribute, other qualifiers via the `rdf:value` form (§7.8);
//! - all `xmlns` declarations on `rdf:RDF`, ordered `rdf` first then alphabetically by prefix, with
//!   unknown namespaces given stable `ns1`, `ns2`, … prefixes unless a preferred prefix is
//!   registered via [`XmpWriter::with_namespace`];
//! - one-space-per-level indentation, UTF-8, no byte-order mark;
//! - a property, qualifier, or structure field whose local name is not an XML `NCName` (Namespaces
//!   in XML 1.0 §3, over XML 1.0 §2.3's `Name`) is dropped with its whole subtree, since no
//!   well-formed element can carry it — see [`XmpWriter::serialize_body`].
//!
//! [`XmpMeta::to_packet`] / [`XmpMeta::to_rdf`] are the shortcuts; [`XmpWriter`] exposes the knobs
//! (the `x:xmpmeta` wrapper, writability, padding, and namespace prefixes).

use crate::model::{XmpArray, XmpItem, XmpMeta, XmpProperty, XmpValue};
use crate::namespace::{Namespace, RDF_NAMESPACE, WellKnownNs, XML_NAMESPACE, XMPMETA_NAMESPACE};

/// The fixed packet identifier Adobe specifies for the `<?xpacket?>` header (Part 1 §7.3.2).
const XPACKET_ID: &str = "W5M0MpCehiHzreSzNTczkc9d";

/// Default trailing padding for a writable packet, in bytes (Part 1 §7.3.2 suggests ~2 KB).
const DEFAULT_PADDING: usize = 2048;

/// A configurable serializer for [`XmpMeta`] → canonical RDF/XML.
///
/// Construct with [`XmpWriter::new`], adjust with the builder methods, then call
/// [`XmpWriter::serialize`] for a full `<?xpacket?>` packet or [`XmpWriter::serialize_body`] for the
/// RDF/XML alone. For the common cases reach for [`XmpMeta::to_packet`] / [`XmpMeta::to_rdf`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XmpWriter {
    /// Whether to wrap `rdf:RDF` in the optional `x:xmpmeta` element (Part 1 §7.3.3).
    wrap_xmpmeta: bool,
    /// Whether the packet trailer says `end="w"` (in-place editable) and carries padding.
    writable: bool,
    /// Trailing padding for a writable packet, in bytes.
    padding: usize,
    /// Registered namespace prefixes, in registration order (a later registration wins).
    namespaces: Vec<Namespace>,
}

impl Default for XmpWriter {
    fn default() -> Self {
        Self {
            wrap_xmpmeta: true,
            writable: true,
            padding: DEFAULT_PADDING,
            namespaces: Vec::new(),
        }
    }
}

impl XmpWriter {
    /// A writer with the default settings: `x:xmpmeta` wrapper, writable, ~2 KB padding.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets whether the body is wrapped in the optional `x:xmpmeta` element.
    #[must_use]
    pub fn wrap_xmpmeta(mut self, wrap: bool) -> Self {
        self.wrap_xmpmeta = wrap;
        self
    }

    /// Sets whether the packet is writable in place (`end="w"` with padding) or read-only
    /// (`end="r"`, no padding).
    #[must_use]
    pub fn writable(mut self, writable: bool) -> Self {
        self.writable = writable;
        self
    }

    /// Sets the trailing padding, in bytes, for a writable packet. Ignored when read-only.
    #[must_use]
    pub fn padding(mut self, bytes: usize) -> Self {
        self.padding = bytes;
        self
    }

    /// Registers a preferred serialization prefix for a namespace URI.
    ///
    /// By default a namespace outside [`WellKnownNs`] gets a synthesized `ns1`, `ns2`, … prefix;
    /// a registration overrides that choice, and may also override a well-known prefix. A
    /// registered namespace is only declared when the graph actually uses it.
    ///
    /// The writer stays infallible, so a registration that cannot be honored is skipped instead
    /// of failing: for the same URI the last registration wins, and a prefix is unusable when it
    /// is the reserved `rdf` or `xml`, matches the synthesized pattern `ns<digits>` (reserved so
    /// synthesis can never collide), is not a simple XML name (an ASCII letter or `_`, then ASCII
    /// letters, digits, `-`, `_`, or `.`), or is already assigned to a different URI in the same
    /// document. Prefixes are serialization cosmetics — the parsed graph is identical either way.
    ///
    /// ```
    /// use gamut_xmp::{Namespace, XmpMeta, XmpWriter};
    ///
    /// let mut meta = XmpMeta::new();
    /// meta.set_text("http://example.com/vocab/", "kind", "demo");
    /// let rdf = XmpWriter::new()
    ///     .with_namespace(Namespace::new("http://example.com/vocab/", "vocab"))
    ///     .serialize_body(&meta);
    /// assert!(rdf.contains("<vocab:kind>demo</vocab:kind>"));
    /// ```
    #[must_use]
    pub fn with_namespace(mut self, namespace: impl Into<Namespace>) -> Self {
        self.namespaces.push(namespace.into());
        self
    }

    /// Serializes the canonical RDF/XML body (no `<?xpacket?>` wrapper).
    ///
    /// The writer stays infallible, so a node that cannot be written is skipped instead of
    /// failing, the way an unusable prefix registration is: a property, qualifier, or structure
    /// field whose local name ([`XmpProperty::name`]) is not an XML `NCName` — empty, starting with
    /// a digit, `-` or `.`, or holding whitespace, a `:`, or any other character outside XML
    /// 1.0 §2.3's `NameChar` — is dropped together with its value and qualifiers, and a namespace
    /// only it used is not declared. The rest of the graph is written exactly as if that node had
    /// never been there, so the output is always well-formed and parses back to the graph minus the
    /// dropped nodes.
    ///
    /// ```
    /// use gamut_xmp::{XmpMeta, XmpWriter};
    ///
    /// let mut meta = XmpMeta::new();
    /// meta.set_text("http://purl.org/dc/elements/1.1/", "format", "text/plain");
    /// meta.set_text("http://purl.org/dc/elements/1.1/", "has space", "dropped");
    /// let rdf = XmpWriter::new().serialize_body(&meta);
    /// assert!(rdf.contains("<dc:format>text/plain</dc:format>"));
    /// assert!(!rdf.contains("dropped"));
    /// ```
    #[must_use]
    pub fn serialize_body(&self, meta: &XmpMeta) -> String {
        let ns = NsMap::gather(meta, &self.namespaces);
        let mut out = String::new();

        let rdf_level = if self.wrap_xmpmeta {
            out.push_str("<x:xmpmeta xmlns:x=\"");
            out.push_str(XMPMETA_NAMESPACE);
            out.push_str("\">\n");
            1
        } else {
            0
        };

        indent(&mut out, rdf_level);
        out.push_str("<rdf:RDF");
        out.push_str(&ns.declarations());
        out.push_str(">\n");

        let desc_level = rdf_level + 1;
        indent(&mut out, desc_level);
        let properties: Vec<&XmpProperty> = writable(&meta.properties).collect();
        if properties.is_empty() {
            out.push_str("<rdf:Description rdf:about=\"\"/>\n");
        } else {
            out.push_str("<rdf:Description rdf:about=\"\">\n");
            for property in properties {
                emit_property(&mut out, desc_level + 1, property, &ns);
            }
            indent(&mut out, desc_level);
            out.push_str("</rdf:Description>\n");
        }

        indent(&mut out, rdf_level);
        out.push_str("</rdf:RDF>");
        if self.wrap_xmpmeta {
            out.push_str("\n</x:xmpmeta>");
        }
        out
    }

    /// Serializes a full XMP packet: the `<?xpacket?>` header, the canonical body, padding (if
    /// writable), and the trailer. UTF-8, no byte-order mark. A node whose local name is not an
    /// XML `NCName` is dropped, as [`XmpWriter::serialize_body`] describes.
    #[must_use]
    pub fn serialize(&self, meta: &XmpMeta) -> Vec<u8> {
        let mut out = String::new();
        out.push_str("<?xpacket begin=\"\" id=\"");
        out.push_str(XPACKET_ID);
        out.push_str("\"?>\n");
        out.push_str(&self.serialize_body(meta));
        out.push('\n');
        if self.writable {
            // Trailing whitespace so the packet can be edited in place (Part 1 §7.3.2).
            for _ in 0..self.padding {
                out.push(' ');
            }
            out.push('\n');
        }
        out.push_str("<?xpacket end=\"");
        out.push(if self.writable { 'w' } else { 'r' });
        out.push_str("\"?>");
        out.into_bytes()
    }
}

impl XmpMeta {
    /// Serializes to a full XMP packet with the default settings (`x:xmpmeta` wrapper, writable,
    /// padded) — the inverse of [`XmpMeta::from_packet`].
    #[must_use]
    pub fn to_packet(&self) -> Vec<u8> {
        XmpWriter::new().serialize(self)
    }

    /// Serializes to the canonical RDF/XML body, without the `<?xpacket?>` wrapper.
    #[must_use]
    pub fn to_rdf(&self) -> String {
        XmpWriter::new().serialize_body(self)
    }
}

// ---------------------------------------------------------------------------------------------------
// Namespace gathering and prefix assignment.
// ---------------------------------------------------------------------------------------------------

/// Maps the data namespaces used in a graph to serialization prefixes (registered, else
/// well-known, else `nsN`).
struct NsMap<'a> {
    /// `(uri, prefix)` for each data namespace, in first-encounter order.
    entries: Vec<(String, String)>,
    /// Prefixes registered on the writer, consulted before the well-known table.
    registered: &'a [Namespace],
    /// How many synthesized `nsN` prefixes have been handed out.
    synthesized: usize,
}

impl<'a> NsMap<'a> {
    /// Walks `meta`, collecting every data namespace and assigning it a prefix.
    fn gather(meta: &XmpMeta, registered: &'a [Namespace]) -> NsMap<'a> {
        let mut map = NsMap {
            entries: Vec::new(),
            registered,
            synthesized: 0,
        };
        for property in &meta.properties {
            map.visit_property(property);
        }
        map
    }

    /// Records the namespaces a property's subtree uses, unless the writer drops that subtree for
    /// its unwritable name — so a namespace only a dropped node used is never declared.
    fn visit_property(&mut self, property: &XmpProperty) {
        if !is_ncname(&property.name) {
            return;
        }
        self.add(&property.namespace);
        self.visit_value(&property.value);
        for qualifier in &property.qualifiers {
            self.visit_property(qualifier);
        }
    }

    fn visit_value(&mut self, value: &XmpValue) {
        match value {
            XmpValue::Simple(_) | XmpValue::Uri(_) => {}
            XmpValue::Structured(fields) => {
                for field in fields {
                    self.visit_property(field);
                }
            }
            XmpValue::Array(array) => {
                for item in array.items() {
                    self.visit_value(&item.value);
                    for qualifier in &item.qualifiers {
                        self.visit_property(qualifier);
                    }
                }
            }
        }
    }

    /// Records a namespace URI, assigning it a prefix. `rdf:` and `xml:` are handled implicitly and
    /// never enter the map.
    ///
    /// Resolution order: the last usable registration for the URI, then the well-known table, then
    /// a synthesized `nsN`. Each tier is skipped when its prefix is already assigned to a different
    /// URI, so the document always stays well-formed.
    fn add(&mut self, uri: &str) {
        if uri == RDF_NAMESPACE || uri == XML_NAMESPACE {
            return;
        }
        if self.entries.iter().any(|(u, _)| u == uri) {
            return;
        }
        let registered = self
            .registered
            .iter()
            .rev()
            .find(|ns| ns.uri == uri)
            .map(|ns| ns.prefix.clone())
            .filter(|p| is_usable_prefix(p) && !self.prefix_taken(p));
        let prefix = registered
            .or_else(|| {
                WellKnownNs::from_uri(uri)
                    .map(|well_known| well_known.prefix().to_owned())
                    .filter(|p| !self.prefix_taken(p))
            })
            .unwrap_or_else(|| self.next_synthesized());
        self.entries.push((uri.to_owned(), prefix));
    }

    /// Whether a prefix is already assigned to some URI in this document.
    fn prefix_taken(&self, prefix: &str) -> bool {
        self.entries.iter().any(|(_, p)| p == prefix)
    }

    /// The next synthesized prefix (`ns1`, `ns2`, …). Never collides: the `ns<digits>` pattern is
    /// reserved (unusable for registrations) and no well-known prefix matches it.
    fn next_synthesized(&mut self) -> String {
        self.synthesized += 1;
        format!("ns{}", self.synthesized)
    }

    /// The serialization prefix for a namespace URI.
    fn prefix_for(&self, uri: &str) -> &str {
        if uri == RDF_NAMESPACE {
            return "rdf";
        }
        if uri == XML_NAMESPACE {
            return "xml";
        }
        self.entries
            .iter()
            .find(|(u, _)| u == uri)
            .map_or("rdf", |(_, prefix)| prefix.as_str())
    }

    /// The `xmlns` declarations for `rdf:RDF`: `rdf` first, then data namespaces by prefix.
    fn declarations(&self) -> String {
        let mut out = String::new();
        out.push_str(" xmlns:rdf=\"");
        out.push_str(RDF_NAMESPACE);
        out.push('"');

        let mut data: Vec<&(String, String)> = self.entries.iter().collect();
        data.sort_by(|a, b| a.1.cmp(&b.1));
        for (uri, prefix) in data {
            out.push_str(" xmlns:");
            out.push_str(prefix);
            out.push_str("=\"");
            push_escaped_attr(&mut out, uri);
            out.push('"');
        }
        out
    }
}

// ---------------------------------------------------------------------------------------------------
// Emission.
// ---------------------------------------------------------------------------------------------------

/// Emits a property element `<prefix:name …>`.
fn emit_property(out: &mut String, level: usize, property: &XmpProperty, ns: &NsMap) {
    let tag = format!("{}:{}", ns.prefix_for(&property.namespace), property.name);
    emit_node(out, level, &tag, &property.value, &property.qualifiers, ns);
}

/// Emits an element named `tag` for a value and its qualifiers: `xml:lang` becomes an attribute, and
/// any other qualifier triggers the `rdf:value` form (Part 1 §7.8).
fn emit_node(
    out: &mut String,
    level: usize,
    tag: &str,
    value: &XmpValue,
    qualifiers: &[XmpProperty],
    ns: &NsMap,
) {
    let lang = lang_of(qualifiers);
    let general: Vec<&XmpProperty> = writable(qualifiers).filter(|q| !is_lang(q)).collect();

    if general.is_empty() {
        emit_value_element(out, level, tag, lang, value, ns);
        return;
    }

    indent(out, level);
    out.push('<');
    out.push_str(tag);
    push_lang(out, lang);
    out.push_str(">\n");
    indent(out, level + 1);
    out.push_str("<rdf:Description>\n");
    emit_value_element(out, level + 2, "rdf:value", None, value, ns);
    for qualifier in general {
        emit_property(out, level + 2, qualifier, ns);
    }
    indent(out, level + 1);
    out.push_str("</rdf:Description>\n");
    indent(out, level);
    out.push_str("</");
    out.push_str(tag);
    out.push_str(">\n");
}

/// Emits an element named `tag` for a value (no general qualifiers), with an optional `xml:lang`.
fn emit_value_element(
    out: &mut String,
    level: usize,
    tag: &str,
    lang: Option<&str>,
    value: &XmpValue,
    ns: &NsMap,
) {
    indent(out, level);
    match value {
        XmpValue::Simple(text) => {
            out.push('<');
            out.push_str(tag);
            push_lang(out, lang);
            out.push('>');
            push_escaped_text(out, text);
            out.push_str("</");
            out.push_str(tag);
            out.push_str(">\n");
        }
        XmpValue::Uri(uri) => {
            out.push('<');
            out.push_str(tag);
            push_lang(out, lang);
            out.push_str(" rdf:resource=\"");
            push_escaped_attr(out, uri);
            out.push_str("\"/>\n");
        }
        XmpValue::Structured(fields) => {
            out.push('<');
            out.push_str(tag);
            push_lang(out, lang);
            out.push_str(">\n");
            indent(out, level + 1);
            out.push_str("<rdf:Description>\n");
            for field in writable(fields) {
                emit_property(out, level + 2, field, ns);
            }
            indent(out, level + 1);
            out.push_str("</rdf:Description>\n");
            indent(out, level);
            out.push_str("</");
            out.push_str(tag);
            out.push_str(">\n");
        }
        XmpValue::Array(array) => {
            let (container, items) = array_parts(array);
            out.push('<');
            out.push_str(tag);
            push_lang(out, lang);
            out.push_str(">\n");
            indent(out, level + 1);
            out.push('<');
            out.push_str(container);
            out.push_str(">\n");
            for item in items {
                emit_node(out, level + 2, "rdf:li", &item.value, &item.qualifiers, ns);
            }
            indent(out, level + 1);
            out.push_str("</");
            out.push_str(container);
            out.push_str(">\n");
            indent(out, level);
            out.push_str("</");
            out.push_str(tag);
            out.push_str(">\n");
        }
    }
}

/// The container element name and items of an array.
fn array_parts(array: &XmpArray) -> (&'static str, &[XmpItem]) {
    match array {
        XmpArray::Bag(items) => ("rdf:Bag", items),
        XmpArray::Seq(items) => ("rdf:Seq", items),
        XmpArray::Alt(items) => ("rdf:Alt", items),
    }
}

/// Pushes ` xml:lang="…"` when a language is present.
fn push_lang(out: &mut String, lang: Option<&str>) {
    if let Some(lang) = lang {
        out.push_str(" xml:lang=\"");
        push_escaped_attr(out, lang);
        out.push('"');
    }
}

/// Whether a qualifier is `xml:lang`.
fn is_lang(qualifier: &XmpProperty) -> bool {
    qualifier.namespace == XML_NAMESPACE && qualifier.name == "lang"
}

/// The nodes of a property list the writer can emit: those whose local name is an XML `NCName`.
///
/// Every emission site (top-level properties, structure fields, qualifiers) walks its list through
/// this filter exactly once, so a node is either written whole or not at all; the namespace sweep
/// makes the same decision in [`NsMap::visit_property`].
fn writable(properties: &[XmpProperty]) -> impl Iterator<Item = &XmpProperty> {
    properties.iter().filter(|p| is_ncname(&p.name))
}

/// Whether `name` is an XML `NCName` (Namespaces in XML 1.0 §3): a `NameStartChar` then any number
/// of `NameChar`s (XML 1.0 Fifth Edition §2.3), with no `:`. Unlike [`is_usable_prefix`], the full
/// Unicode repertoire of the productions is accepted — a local name is data, not a cosmetic the
/// writer could substitute, so any name a conformant document can carry must survive.
fn is_ncname(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(is_name_start_char) && chars.all(is_name_char)
}

/// XML 1.0 §2.3 `NameStartChar`, without the `:` that `NCName` excludes.
fn is_name_start_char(c: char) -> bool {
    matches!(c,
        'A'..='Z'
        | '_'
        | 'a'..='z'
        | '\u{C0}'..='\u{D6}'
        | '\u{D8}'..='\u{F6}'
        | '\u{F8}'..='\u{2FF}'
        | '\u{370}'..='\u{37D}'
        | '\u{37F}'..='\u{1FFF}'
        | '\u{200C}'..='\u{200D}'
        | '\u{2070}'..='\u{218F}'
        | '\u{2C00}'..='\u{2FEF}'
        | '\u{3001}'..='\u{D7FF}'
        | '\u{F900}'..='\u{FDCF}'
        | '\u{FDF0}'..='\u{FFFD}'
        | '\u{10000}'..='\u{EFFFF}')
}

/// XML 1.0 §2.3 `NameChar`, without the `:` that `NCName` excludes.
fn is_name_char(c: char) -> bool {
    is_name_start_char(c)
        || matches!(c,
            '-' | '.' | '0'..='9' | '\u{B7}' | '\u{300}'..='\u{36F}' | '\u{203F}'..='\u{2040}')
}

/// Whether a registered prefix can be used in serialization: `rdf` and `xml` are reserved, the
/// `ns<digits>` pattern is reserved for synthesized prefixes (so synthesis can never collide),
/// and the prefix must be a simple XML name (an ASCII letter or `_`, then ASCII letters, digits,
/// `-`, `_`, or `.`) so the emitted document stays well-formed.
fn is_usable_prefix(prefix: &str) -> bool {
    if prefix == "rdf" || prefix == "xml" || is_synthesized_pattern(prefix) {
        return false;
    }
    let mut chars = prefix.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// Whether a prefix matches the reserved synthesized pattern `ns<digits>`.
fn is_synthesized_pattern(prefix: &str) -> bool {
    prefix
        .strip_prefix("ns")
        .is_some_and(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
}

/// The `xml:lang` value among a qualifier list, if any.
fn lang_of(qualifiers: &[XmpProperty]) -> Option<&str> {
    qualifiers
        .iter()
        .find(|q| is_lang(q))
        .and_then(XmpProperty::text)
}

/// Pushes `count` indentation spaces (one per nesting level).
fn indent(out: &mut String, level: usize) {
    for _ in 0..level {
        out.push(' ');
    }
}

/// Appends `text` with the markup-significant characters escaped (Part 1 §7.5; never CDATA).
///
/// A carriage return is escaped as `&#xD;` — XML 1.0 line-ending normalization would otherwise
/// rewrite it to a line feed on the next parse, breaking the parse∘serialize fixed point (the
/// canonical-XML rule; XMPCore escapes the same way).
fn push_escaped_text(out: &mut String, text: &str) {
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\r' => out.push_str("&#xD;"),
            _ => out.push(c),
        }
    }
}

/// Appends an attribute value with markup characters, the double quote, and whitespace control
/// characters escaped.
///
/// Tab, line feed, and carriage return are escaped as character references — XML 1.0
/// attribute-value normalization folds the literal characters to spaces on the next parse, which
/// would corrupt the value (the canonical-XML rule; XMPCore escapes the same way).
fn push_escaped_attr(out: &mut String, text: &str) {
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\t' => out.push_str("&#x9;"),
            '\n' => out.push_str("&#xA;"),
            '\r' => out.push_str("&#xD;"),
            _ => out.push(c),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DC: &str = "http://purl.org/dc/elements/1.1/";
    const XMP: &str = "http://ns.adobe.com/xap/1.0/";

    /// Serializes a graph of the given properties to a bare `rdf:RDF` body (no wrapper) for compact,
    /// byte-exact assertions.
    fn body(properties: Vec<XmpProperty>) -> String {
        XmpWriter::new()
            .wrap_xmpmeta(false)
            .serialize_body(&XmpMeta { properties })
    }

    #[test]
    fn simple_value_is_element_text() {
        let out = body(vec![XmpProperty::new(
            XMP,
            "Rating",
            XmpValue::Simple("3".into()),
        )]);
        assert_eq!(
            out,
            "<rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\" \
             xmlns:xmp=\"http://ns.adobe.com/xap/1.0/\">\n \
             <rdf:Description rdf:about=\"\">\n  \
             <xmp:Rating>3</xmp:Rating>\n \
             </rdf:Description>\n\
             </rdf:RDF>"
        );
    }

    #[test]
    fn uri_value_uses_rdf_resource() {
        let out = body(vec![XmpProperty::new(
            XMP,
            "BaseURL",
            XmpValue::Uri("http://example.com/".into()),
        )]);
        assert!(out.contains("<xmp:BaseURL rdf:resource=\"http://example.com/\"/>"));
    }

    #[test]
    fn bag_emits_rdf_li_in_order() {
        let out = body(vec![XmpProperty::new(
            DC,
            "subject",
            XmpValue::Array(XmpArray::Bag(vec![
                XmpItem::new(XmpValue::Simple("a".into())),
                XmpItem::new(XmpValue::Simple("b".into())),
            ])),
        )]);
        let expected = "  <dc:subject>\n   <rdf:Bag>\n    <rdf:li>a</rdf:li>\n    \
                        <rdf:li>b</rdf:li>\n   </rdf:Bag>\n  </dc:subject>";
        assert!(out.contains(expected), "got:\n{out}");
    }

    #[test]
    fn language_alternative_uses_xml_lang_attribute() {
        let mut meta = XmpMeta::new();
        meta.set_lang_alt(DC, "title", "x-default", "Hi");
        meta.set_lang_alt(DC, "title", "fr", "Salut");
        let out = XmpWriter::new().wrap_xmpmeta(false).serialize_body(&meta);
        assert!(out.contains("<rdf:Alt>"));
        assert!(out.contains("<rdf:li xml:lang=\"x-default\">Hi</rdf:li>"));
        assert!(out.contains("<rdf:li xml:lang=\"fr\">Salut</rdf:li>"));
    }

    #[test]
    fn structure_uses_nested_description() {
        let out = body(vec![XmpProperty::new(
            XMP,
            "Thumb",
            XmpValue::Structured(vec![XmpProperty::new(
                XMP,
                "w",
                XmpValue::Simple("9".into()),
            )]),
        )]);
        let expected = "  <xmp:Thumb>\n   <rdf:Description>\n    <xmp:w>9</xmp:w>\n   \
                        </rdf:Description>\n  </xmp:Thumb>";
        assert!(out.contains(expected), "got:\n{out}");
    }

    #[test]
    fn general_qualifier_uses_rdf_value_form() {
        let prop = XmpProperty {
            namespace: DC.into(),
            name: "rights".into(),
            value: XmpValue::Simple("(c) Me".into()),
            qualifiers: vec![XmpProperty::new(
                XMP,
                "owner",
                XmpValue::Simple("Me".into()),
            )],
        };
        let out = body(vec![prop]);
        let expected = "  <dc:rights>\n   <rdf:Description>\n    <rdf:value>(c) Me</rdf:value>\n    \
                        <xmp:owner>Me</xmp:owner>\n   </rdf:Description>\n  </dc:rights>";
        assert!(out.contains(expected), "got:\n{out}");
    }

    #[test]
    fn text_and_attributes_are_escaped() {
        let out = body(vec![XmpProperty::new(
            DC,
            "rights",
            XmpValue::Simple("a < b & c > d".into()),
        )]);
        assert!(out.contains("<dc:rights>a &lt; b &amp; c &gt; d</dc:rights>"));

        let mut meta = XmpMeta::new();
        meta.set_lang_alt(DC, "title", "x-\"q\"", "v");
        let out = meta.to_rdf();
        assert!(out.contains("xml:lang=\"x-&quot;q&quot;\""), "got:\n{out}");
    }

    #[test]
    fn namespaces_are_declared_rdf_first_then_alphabetical_with_synthesized() {
        // Two well-known (dc, xmp) plus one unknown → ns1; declared rdf, then by prefix.
        let out = body(vec![
            XmpProperty::new(XMP, "Rating", XmpValue::Simple("1".into())),
            XmpProperty::new(DC, "format", XmpValue::Simple("text/plain".into())),
            XmpProperty::new("http://example.com/x/", "k", XmpValue::Simple("v".into())),
        ]);
        // Without the x:xmpmeta wrapper the first line is the rdf:RDF element with all decls.
        let header = out.lines().next().unwrap();
        assert_eq!(
            header,
            "<rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\" \
             xmlns:dc=\"http://purl.org/dc/elements/1.1/\" \
             xmlns:ns1=\"http://example.com/x/\" \
             xmlns:xmp=\"http://ns.adobe.com/xap/1.0/\">"
        );
        assert!(out.contains("<ns1:k>v</ns1:k>"));
    }

    #[test]
    fn empty_meta_self_closes_description() {
        let out = body(vec![]);
        assert!(
            out.contains("<rdf:Description rdf:about=\"\"/>"),
            "got:\n{out}"
        );
    }

    #[test]
    fn packet_has_header_body_and_trailer() {
        let meta = XmpMeta {
            properties: vec![XmpProperty::new(
                XMP,
                "Rating",
                XmpValue::Simple("3".into()),
            )],
        };
        let packet = String::from_utf8(meta.to_packet()).unwrap();
        assert!(packet.starts_with("<?xpacket begin=\"\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\n"));
        assert!(packet.contains("<x:xmpmeta xmlns:x=\"adobe:ns:meta/\">"));
        assert!(packet.ends_with("<?xpacket end=\"w\"?>"));
        assert!(packet.contains("<xmp:Rating>3</xmp:Rating>"));
    }

    #[test]
    fn read_only_writer_omits_padding() {
        let writer = XmpWriter::new().writable(false);
        let packet = String::from_utf8(writer.serialize(&XmpMeta::new())).unwrap();
        assert!(packet.ends_with("<?xpacket end=\"r\"?>"));
        // No run of padding spaces between the body and the trailer.
        assert!(!packet.contains("   <?xpacket end"));
    }

    #[test]
    fn writable_writer_emits_requested_padding() {
        let bare = XmpWriter::new()
            .writable(false)
            .serialize(&XmpMeta::new())
            .len();
        let small = XmpWriter::new()
            .padding(10)
            .serialize(&XmpMeta::new())
            .len();
        let big = XmpWriter::new()
            .padding(1000)
            .serialize(&XmpMeta::new())
            .len();
        // Padding is applied verbatim: the size grows by exactly the requested byte count, so the
        // builder must actually record the value (not reset to the default) and emit one space each.
        assert_eq!(big - small, 990);
        assert!(
            small > bare,
            "writable padding must add bytes over a read-only packet"
        );
    }

    #[test]
    fn non_lang_xml_qualifier_uses_the_general_qualifier_form() {
        // xml:space is in the XML namespace but is NOT a language tag, so it must serialize as a
        // general qualifier (the rdf:value form), never collapse into an xml:lang attribute.
        let prop = XmpProperty {
            namespace: DC.into(),
            name: "rights".into(),
            value: XmpValue::Simple("v".into()),
            qualifiers: vec![XmpProperty::new(
                "http://www.w3.org/XML/1998/namespace",
                "space",
                XmpValue::Simple("preserve".into()),
            )],
        };
        let out = body(vec![prop]);
        assert!(out.contains("<rdf:value>v</rdf:value>"), "got:\n{out}");
        assert!(
            out.contains("<xml:space>preserve</xml:space>"),
            "got:\n{out}"
        );
        assert!(
            !out.contains("xml:lang"),
            "xml:space must not become a language: {out}"
        );
    }

    #[test]
    fn registered_namespace_replaces_the_synthesized_prefix() {
        let mut meta = XmpMeta::new();
        meta.set_text("http://example.com/vocab/", "kind", "demo");
        let out = XmpWriter::new()
            .wrap_xmpmeta(false)
            .with_namespace(Namespace::new("http://example.com/vocab/", "vocab"))
            .serialize_body(&meta);
        assert!(
            out.contains("xmlns:vocab=\"http://example.com/vocab/\""),
            "got:\n{out}"
        );
        assert!(out.contains("<vocab:kind>demo</vocab:kind>"), "got:\n{out}");
        assert!(
            !out.contains("ns1"),
            "no synthesized prefix once registered: {out}"
        );
    }

    #[test]
    fn registered_namespace_overrides_a_well_known_prefix() {
        let mut meta = XmpMeta::new();
        meta.set_text(DC, "format", "text/plain");
        let out = XmpWriter::new()
            .wrap_xmpmeta(false)
            .with_namespace(Namespace::new(DC, "dublin"))
            .serialize_body(&meta);
        assert!(
            out.contains("<dublin:format>text/plain</dublin:format>"),
            "got:\n{out}"
        );
        assert!(
            !out.contains("xmlns:dc="),
            "the well-known prefix is overridden: {out}"
        );
    }

    #[test]
    fn last_registration_for_a_uri_wins() {
        let mut meta = XmpMeta::new();
        meta.set_text("http://example.com/vocab/", "kind", "demo");
        let out = XmpWriter::new()
            .wrap_xmpmeta(false)
            .with_namespace(Namespace::new("http://example.com/vocab/", "one"))
            .with_namespace(Namespace::new("http://example.com/vocab/", "two"))
            .serialize_body(&meta);
        assert!(out.contains("<two:kind>"), "got:\n{out}");
        assert!(!out.contains("one:"), "got:\n{out}");
    }

    #[test]
    fn reserved_and_malformed_prefixes_are_ignored() {
        // rdf/xml and the synthesized ns<digits> pattern are reserved; an empty or non-XML-name
        // prefix would break well-formedness. Each unusable registration falls back to the
        // synthesized tier.
        for bad in [
            "rdf",
            "xml",
            "ns1",
            "ns42",
            "",
            "has space",
            "1digit",
            "a:b",
        ] {
            let mut meta = XmpMeta::new();
            meta.set_text("http://example.com/vocab/", "kind", "demo");
            let out = XmpWriter::new()
                .wrap_xmpmeta(false)
                .with_namespace(Namespace::new("http://example.com/vocab/", bad))
                .serialize_body(&meta);
            assert!(
                out.contains("<ns1:kind>demo</ns1:kind>"),
                "prefix {bad:?} must be ignored, got:\n{out}"
            );
        }
    }

    #[test]
    fn colliding_registration_is_ignored() {
        // The registered prefix `dc` is already assigned to Dublin Core (encountered first), so
        // the second URI falls back to a synthesized prefix instead of emitting a duplicate.
        let mut meta = XmpMeta::new();
        meta.set_text(DC, "format", "text/plain");
        meta.set_text("http://example.com/vocab/", "kind", "demo");
        let out = XmpWriter::new()
            .wrap_xmpmeta(false)
            .with_namespace(Namespace::new("http://example.com/vocab/", "dc"))
            .serialize_body(&meta);
        assert!(out.contains("<dc:format>"), "got:\n{out}");
        assert!(out.contains("<ns1:kind>"), "got:\n{out}");
    }

    #[test]
    fn well_known_prefix_dodges_a_registration_that_took_it() {
        // The registration claims `dc` for a custom URI (encountered first); Dublin Core then
        // cannot use its conventional prefix and falls back to a synthesized one.
        let mut meta = XmpMeta::new();
        meta.set_text("http://example.com/vocab/", "kind", "demo");
        meta.set_text(DC, "format", "text/plain");
        let out = XmpWriter::new()
            .wrap_xmpmeta(false)
            .with_namespace(Namespace::new("http://example.com/vocab/", "dc"))
            .serialize_body(&meta);
        assert!(out.contains("<dc:kind>demo</dc:kind>"), "got:\n{out}");
        assert!(
            out.contains("<ns1:format>text/plain</ns1:format>"),
            "got:\n{out}"
        );
    }

    #[test]
    fn unused_registration_is_not_declared() {
        let mut meta = XmpMeta::new();
        meta.set_text(DC, "format", "text/plain");
        let out = XmpWriter::new()
            .wrap_xmpmeta(false)
            .with_namespace(Namespace::new("http://example.com/vocab/", "vocab"))
            .serialize_body(&meta);
        assert!(
            !out.contains("vocab"),
            "unused namespaces stay undeclared: {out}"
        );
    }

    #[test]
    fn synthesized_pattern_prefixes_are_reserved() {
        // ns<digits> is reserved for the synthesizer, so a registration claiming ns7 is ignored
        // and numbering stays sequential — synthesized names can never collide by construction.
        // A prefix merely *starting* with ns ("nsx") is not reserved.
        let mut meta = XmpMeta::new();
        meta.set_text("http://example.com/a/", "x", "1");
        meta.set_text("http://example.com/b/", "y", "2");
        let out = XmpWriter::new()
            .wrap_xmpmeta(false)
            .with_namespace(Namespace::new("http://example.com/a/", "ns7"))
            .with_namespace(Namespace::new("http://example.com/b/", "nsx"))
            .serialize_body(&meta);
        assert!(
            out.contains("<ns1:x>1</ns1:x>"),
            "ns7 must be ignored, got:\n{out}"
        );
        assert!(
            out.contains("<nsx:y>2</nsx:y>"),
            "nsx is a legal prefix, got:\n{out}"
        );
    }

    #[test]
    fn well_known_ns_registers_directly() {
        // `impl From<WellKnownNs> for Namespace` lets a standard schema pass straight in;
        // registering its conventional prefix is a no-op on the output.
        let mut meta = XmpMeta::new();
        meta.set_text(DC, "format", "text/plain");
        let registered = XmpWriter::new()
            .wrap_xmpmeta(false)
            .with_namespace(WellKnownNs::DublinCore)
            .serialize_body(&meta);
        let plain = XmpWriter::new().wrap_xmpmeta(false).serialize_body(&meta);
        assert_eq!(registered, plain);
    }

    #[test]
    fn control_characters_are_escaped_as_character_references() {
        // CR in text, and TAB/LF/CR in attribute values, must leave as character references —
        // a literal CR is normalized to LF (text) and literal TAB/LF/CR to spaces (attributes)
        // by any conformant XML parse, silently corrupting the value.
        let out = body(vec![
            XmpProperty::new(DC, "description", XmpValue::Simple("a\rb\nc\td".into())),
            XmpProperty::new(XMP, "BaseURL", XmpValue::Uri("u\tv\nw\rx".into())),
        ]);
        assert!(
            out.contains("<dc:description>a&#xD;b\nc\td</dc:description>"),
            "text escapes CR only (LF and TAB are literal-safe in text): {out}"
        );
        assert!(
            out.contains("rdf:resource=\"u&#x9;v&#xA;w&#xD;x\""),
            "attributes escape TAB, LF, and CR: {out}"
        );
    }

    /// Local names that cannot form a well-formed element: empty, a space, a leading digit, a `:`
    /// (a QName, not an `NCName`), and a leading `-` / `.` (legal `NameChar`s, not start chars).
    const BAD_NAMES: [&str; 6] = ["", "has space", "1digit", "a:b", "-x", ".x"];

    #[test]
    fn is_ncname_rejects_unwritable_local_names() {
        for bad in BAD_NAMES {
            assert!(!is_ncname(bad), "{bad:?} is not an NCName");
        }
    }

    #[test]
    fn is_ncname_accepts_one_of_each_name_start_range() {
        // One representative per NameStartChar alternative (XML 1.0 §2.3), each as the whole name.
        for c in [
            'A',
            '_',
            'z',
            '\u{C0}',
            '\u{D8}',
            '\u{F8}',
            '\u{370}',
            '\u{37F}',
            '\u{200C}',
            '\u{2070}',
            '\u{2C00}',
            '\u{3001}',
            '\u{F900}',
            '\u{FDF0}',
            '\u{10000}',
        ] {
            assert!(is_ncname(&c.to_string()), "{c:?} starts a name");
        }
    }

    #[test]
    fn is_ncname_accepts_name_chars_only_after_the_start() {
        // One representative per NameChar-only alternative: legal after the first char, not as it.
        for c in ['-', '.', '7', '\u{B7}', '\u{300}', '\u{203F}'] {
            assert!(is_ncname(&format!("a{c}")), "{c:?} continues a name");
            assert!(!is_ncname(&c.to_string()), "{c:?} cannot start a name");
        }
        // Outside every range: between alternatives, and past the last.
        for c in ['\u{D7}', '\u{2000}', '\u{F0000}'] {
            assert!(!is_ncname(&format!("a{c}")), "{c:?} is not a NameChar");
        }
    }

    #[test]
    fn unwritable_top_level_property_is_dropped_with_its_namespace() {
        for bad in BAD_NAMES {
            let out = body(vec![
                XmpProperty::new(XMP, "Rating", XmpValue::Simple("3".into())),
                XmpProperty::new(DC, bad, XmpValue::Simple("gone".into())),
            ]);
            assert_eq!(
                out,
                body(vec![XmpProperty::new(
                    XMP,
                    "Rating",
                    XmpValue::Simple("3".into())
                )]),
                "property named {bad:?} must be dropped, and dc left undeclared"
            );
        }
    }

    #[test]
    fn graph_of_only_unwritable_properties_self_closes_description() {
        let out = body(vec![XmpProperty::new(
            DC,
            "",
            XmpValue::Simple("gone".into()),
        )]);
        assert_eq!(out, body(vec![]));
    }

    #[test]
    fn unwritable_structure_field_is_dropped() {
        let thumb = |fields| XmpProperty::new(XMP, "Thumb", XmpValue::Structured(fields));
        let w = XmpProperty::new(XMP, "w", XmpValue::Simple("9".into()));
        let out = body(vec![thumb(vec![
            w.clone(),
            XmpProperty::new(XMP, "1digit", XmpValue::Simple("gone".into())),
        ])]);
        assert_eq!(out, body(vec![thumb(vec![w])]));
    }

    #[test]
    fn unwritable_qualifier_is_dropped_and_leaves_the_plain_form() {
        // The only general qualifier is unwritable, so the value keeps the plain element form
        // rather than an `rdf:value` wrapper around nothing — on a property and on an array item.
        let bad = XmpProperty::new(XMP, "a:b", XmpValue::Simple("gone".into()));
        let prop = XmpProperty {
            namespace: DC.into(),
            name: "rights".into(),
            value: XmpValue::Simple("v".into()),
            qualifiers: vec![bad.clone()],
        };
        let mut item = XmpItem::new(XmpValue::Simple("a".into()));
        item.qualifiers.push(bad);
        let list = XmpProperty::new(DC, "subject", XmpValue::Array(XmpArray::Bag(vec![item])));
        let out = body(vec![prop, list]);
        assert_eq!(
            out,
            body(vec![
                XmpProperty::new(DC, "rights", XmpValue::Simple("v".into())),
                XmpProperty::new(
                    DC,
                    "subject",
                    XmpValue::Array(XmpArray::Bag(vec![XmpItem::new(XmpValue::Simple(
                        "a".into()
                    ))]))
                ),
            ])
        );
    }

    #[test]
    fn non_ascii_local_name_is_written() {
        let out = body(vec![XmpProperty::new(
            DC,
            "título",
            XmpValue::Simple("v".into()),
        )]);
        assert!(out.contains("<dc:título>v</dc:título>"), "got:\n{out}");
    }

    #[test]
    fn uri_attribute_value_is_escaped() {
        let out = body(vec![XmpProperty::new(
            XMP,
            "BaseURL",
            XmpValue::Uri("a&b<c>d".into()),
        )]);
        assert!(
            out.contains("rdf:resource=\"a&amp;b&lt;c&gt;d\""),
            "got:\n{out}"
        );
    }
}
