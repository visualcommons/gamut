//! The field-level machinery every structured model is built on: reading a structure's field list
//! under the reproduction rule, the value each modelled field kind is read from and written back
//! as, and verbatim retention of the fields a typed read did not consume.
//!
//! The rule these implement — a field is read only when writing the value back reproduces it — is
//! stated in the [`extension`](crate::extension) module docs. The structures in that module and
//! the [`PhotoMetadata`](crate::PhotoMetadata) accessors both call into this one copy.

use gamut_xmp::{XML_NAMESPACE, XmpArray, XmpItem, XmpProperty, XmpValue};

// --- Reading a structure's field list, keeping only what the writer can reproduce -------------

/// The language tag of a language alternative's default entry (XMP Part 1 §8.2.2.4).
pub(crate) const X_DEFAULT: &str = "x-default";

/// A structure's field list under a typed read, remembering which fields the read consumed.
///
/// A field is consumed only when the property the writer will emit for the value read *reproduces*
/// that field (see [`reproduces`]); every other field is left for the type's `other` list and
/// re-emitted verbatim. Deciding it on the write side rather than on the read side is what makes
/// the three ways a field can fall outside the model — unnamed, unreadable, unwritable — one rule
/// (see the [module docs](crate::extension)).
pub(crate) struct Reader<'a> {
    /// The fields being read.
    fields: &'a [XmpProperty],
    /// Whether the read consumed the field at the same index.
    used: Vec<bool>,
}

impl<'a> Reader<'a> {
    /// A reader over `fields`, with nothing consumed yet.
    pub(crate) fn new(fields: &'a [XmpProperty]) -> Self {
        Self {
            fields,
            used: vec![false; fields.len()],
        }
    }

    /// Reads the field named `ns:name` through `parse`, and keeps the value only when the property
    /// `write` will emit for it reproduces the field that was read.
    ///
    /// Two fields of one name are never read: only one of them could be written back, so both are
    /// left to be kept verbatim instead.
    fn read<T>(
        &mut self,
        ns: &str,
        name: &str,
        parse: impl FnOnce(&'a XmpValue) -> Option<T>,
        write: impl FnOnce(&T) -> Option<XmpValue>,
    ) -> Option<T> {
        let fields = self.fields;
        let mut matches = fields
            .iter()
            .enumerate()
            .filter(|(_, p)| p.namespace == ns && p.name == name);
        let (index, field) = matches.next()?;
        if matches.next().is_some() {
            return None;
        }
        let value = parse(&field.value)?;
        let emitted = XmpProperty::new(ns, name, write(&value)?);
        if !reproduces(field, &emitted) {
            return None;
        }
        self.used[index] = true;
        Some(value)
    }

    /// The simple text of the field named `ns:name`.
    pub(crate) fn text(&mut self, ns: &str, name: &str) -> Option<String> {
        self.read(ns, name, parse_text, |value| Some(text_value(value)))
    }

    /// The `x-default` entry of the language-alternative field named `ns:name`.
    pub(crate) fn lang_alt(&mut self, ns: &str, name: &str) -> Option<String> {
        self.read(ns, name, parse_lang_alt, |value| {
            Some(lang_alt_value(value))
        })
    }

    /// Every simple item of the array field named `ns:name` (empty when the field is absent or the
    /// read did not consume it).
    pub(crate) fn list(&mut self, ns: &str, name: &str, ordered: bool) -> Vec<String> {
        self.read(ns, name, parse_list, |values| list_value(ordered, values))
            .unwrap_or_default()
    }

    /// The field named `ns:name` parsed as an XMP `Real`.
    pub(crate) fn number(&mut self, ns: &str, name: &str) -> Option<f64> {
        self.read(ns, name, parse_number, |value| number_value(*value))
    }

    /// The single structured field named `ns:name`, read through `parse` and written through
    /// `write`.
    pub(crate) fn nested<T>(
        &mut self,
        ns: &str,
        name: &str,
        parse: impl FnOnce(&[XmpProperty]) -> T,
        write: impl FnOnce(&T) -> XmpValue,
    ) -> Option<T> {
        self.read(
            ns,
            name,
            |value| structure(value).map(parse),
            |value| Some(write(value)),
        )
    }

    /// Every structure of the array field named `ns:name`, read through `parse` and written through
    /// `write` (empty when the field is absent or the read did not consume it).
    pub(crate) fn nested_array<T>(
        &mut self,
        ns: &str,
        name: &str,
        ordered: bool,
        parse: impl Fn(&[XmpProperty]) -> T,
        write: impl Fn(&T) -> XmpValue,
    ) -> Vec<T> {
        self.read(
            ns,
            name,
            |value| {
                let parsed: Vec<T> = structures(value).into_iter().map(&parse).collect();
                (!parsed.is_empty()).then_some(parsed)
            },
            |values| nested_array_value(ordered, values.iter().map(&write).collect()),
        )
        .unwrap_or_default()
    }

    /// Every field the read did not consume, cloned for verbatim retention.
    pub(crate) fn other(self) -> Vec<XmpProperty> {
        let Self { fields, used } = self;
        fields
            .iter()
            .zip(used)
            .filter(|&(_, used)| !used)
            .map(|(property, _)| property.clone())
            .collect()
    }
}

/// The structure field lists an array value holds, skipping items that are not structures.
///
/// A bare structure written where the standard puts an array reads as that array's single element
/// (see the [module docs](crate::extension)); everything that is neither reads as nothing at all.
pub(crate) fn structures(value: &XmpValue) -> Vec<&[XmpProperty]> {
    match value {
        XmpValue::Array(array) => array
            .items()
            .iter()
            .filter_map(|item| structure(&item.value))
            .collect(),
        single => structure(single).into_iter().collect(),
    }
}

/// The structure fields of `value`, or `None` if it is not a structure.
pub(crate) fn structure(value: &XmpValue) -> Option<&[XmpProperty]> {
    match value {
        XmpValue::Structured(fields) => Some(fields),
        _ => None,
    }
}

// --- Reproduction: whether writing back what was read gives the field back --------------------

/// Whether `emitted` — the property the writer will produce for the value read from `read` —
/// reproduces `read`.
///
/// Reproduction is equality of value and qualifiers, with the two lexical re-spellings the module
/// documents allowed: a number may be written in another form for the same decimal value, and a
/// language tag may be re-cased, because XMP Part 1 §8.2.2.4 matches tags case-insensitively. A
/// structure's fields are an unordered set (Part 1 §6.3.3), so the model's field order is not a
/// difference; an array's items are ordered, and its RDF container kind is part of its value
/// (Part 1 §6.3.4).
pub(crate) fn reproduces(read: &XmpProperty, emitted: &XmpProperty) -> bool {
    read.namespace == emitted.namespace
        && read.name == emitted.name
        && same_value(&read.value, &emitted.value)
        && same_qualifiers(&read.qualifiers, &emitted.qualifiers)
}

/// Whether two values carry the same information (see [`reproduces`]).
fn same_value(read: &XmpValue, emitted: &XmpValue) -> bool {
    match (read, emitted) {
        (XmpValue::Simple(read), XmpValue::Simple(emitted)) => {
            read == emitted || same_number(read, emitted)
        }
        (XmpValue::Uri(read), XmpValue::Uri(emitted)) => read == emitted,
        (XmpValue::Structured(read), XmpValue::Structured(emitted)) => {
            read.len() == emitted.len()
                && read
                    .iter()
                    .all(|field| emitted.iter().any(|other| reproduces(field, other)))
        }
        (XmpValue::Array(read), XmpValue::Array(emitted)) => same_array(read, emitted),
        _ => false,
    }
}

/// Whether two arrays are the same RDF container kind holding the same items in the same order.
fn same_array(read: &XmpArray, emitted: &XmpArray) -> bool {
    let items = match (read, emitted) {
        (XmpArray::Bag(read), XmpArray::Bag(emitted))
        | (XmpArray::Seq(read), XmpArray::Seq(emitted))
        | (XmpArray::Alt(read), XmpArray::Alt(emitted)) => (read, emitted),
        _ => return false,
    };
    items.0.len() == items.1.len()
        && items.0.iter().zip(items.1).all(|(read, emitted)| {
            same_value(&read.value, &emitted.value)
                && same_qualifiers(&read.qualifiers, &emitted.qualifiers)
        })
}

/// Whether two qualifier lists hold the same qualifiers, matching an `xml:lang` tag
/// case-insensitively (XMP Part 1 §8.2.2.4).
fn same_qualifiers(read: &[XmpProperty], emitted: &[XmpProperty]) -> bool {
    read.len() == emitted.len()
        && read.iter().all(|qualifier| {
            emitted
                .iter()
                .any(|other| match (lang(qualifier), lang(other)) {
                    (Some(read), Some(emitted)) => read.eq_ignore_ascii_case(emitted),
                    _ => reproduces(qualifier, other),
                })
        })
}

/// The tag `qualifier` carries if it is an `xml:lang` qualifier.
fn lang(qualifier: &XmpProperty) -> Option<&str> {
    (qualifier.namespace == XML_NAMESPACE && qualifier.name == "lang")
        .then(|| qualifier.text())
        .flatten()
}

/// Whether two texts are XMP `Real`s (Part 1 §8.2.1.4) spelling the same decimal value — the
/// difference between ` 0.50 ` and `0.5`, which the writer's own formatting introduces.
///
/// The comparison is of decimal values, not of the doubles they parse to: a text a double cannot
/// hold exactly — `0.1000000000000000000001`, a twenty-digit integer, a decimal that underflows
/// to `0` — parses to a double whose shortest form is *another* decimal, so the writer would
/// change the value, and the field is not reproduced.
fn same_number(read: &str, emitted: &str) -> bool {
    matches!((real(read), real(emitted)), (Some(read), Some(emitted)) if read == emitted)
}

/// The decimal value `text` spells as an XMP `Real` (Part 1 §8.2.1.4), reduced so that two
/// spellings of one value reduce alike: its sign, its integer digits without leading zeros and its
/// fraction digits without trailing zeros. `None` for text that is not a `Real`.
///
/// A `Real` is an optional sign, then an integer part, a fraction part, or both; it has no
/// exponent, so `1e-400`, `NaN` and `inf` are not `Real`s. Surrounding whitespace is not part of the value.
/// A zero keeps its sign: the writer formats a double, which keeps it too, so a read zero and the
/// zero written for it always agree on it.
fn real(text: &str) -> Option<(bool, &str, &str)> {
    let text = text.trim();
    let (negative, unsigned) = match text.strip_prefix('-') {
        Some(unsigned) => (true, unsigned),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    let (integer, fraction) = match unsigned.split_once('.') {
        Some((integer, fraction)) if !fraction.is_empty() => (integer, fraction),
        Some(_) => return None,
        None if !unsigned.is_empty() => (unsigned, ""),
        None => return None,
    };
    integer
        .bytes()
        .chain(fraction.bytes())
        .all(|byte| byte.is_ascii_digit())
        .then(|| {
            (
                negative,
                integer.trim_start_matches('0'),
                fraction.trim_end_matches('0'),
            )
        })
}

// --- The value each modelled field is read from and written back as ---------------------------
//
// Every field is read by a `parse_*` and written by the `*_value` beside it, so what the reader
// compares against is the very value the writer will emit and the two cannot drift apart.

/// The simple text of a value.
pub(crate) fn parse_text(value: &XmpValue) -> Option<String> {
    value.text().map(str::to_owned)
}

/// A simple text value.
fn text_value(value: &str) -> XmpValue {
    XmpValue::Simple(value.to_owned())
}

/// The `x-default` entry of a language alternative, tolerating a plain simple value.
///
/// The entry is found by its `xml:lang` qualifier, compared case-insensitively as XMP Part 1
/// §8.2.2.4 requires — the same match
/// [`XmpMeta::get_lang_alt`](gamut_xmp::XmpMeta::get_lang_alt) makes on a top-level property. An
/// alternative list with no default entry reads as absent, rather than having another language
/// relabelled as the default; one holding a language *beside* the default reads its default and is
/// then refused by [`reproduces`], because [`lang_alt_value`] would write the other language away.
pub(crate) fn parse_lang_alt(value: &XmpValue) -> Option<String> {
    match value {
        XmpValue::Array(XmpArray::Alt(items)) => items
            .iter()
            .find(|item| {
                item.lang()
                    .is_some_and(|lang| lang.eq_ignore_ascii_case(X_DEFAULT))
            })
            .and_then(XmpItem::text),
        simple => simple.text(),
    }
    .map(str::to_owned)
}

/// A language alternative holding one `x-default` entry.
pub(crate) fn lang_alt_value(value: &str) -> XmpValue {
    XmpValue::Array(XmpArray::Alt(vec![XmpItem::lang_text(
        X_DEFAULT,
        value.to_owned(),
    )]))
}

/// Every simple item of an array value, or `None` when the value is not an array or holds no
/// simple item.
pub(crate) fn parse_list(value: &XmpValue) -> Option<Vec<String>> {
    match value {
        XmpValue::Array(array) => {
            let texts: Vec<String> = array.texts().map(str::to_owned).collect();
            (!texts.is_empty()).then_some(texts)
        }
        _ => None,
    }
}

/// An array of simple text, or `None` when there is nothing to write.
fn list_value(ordered: bool, values: &[String]) -> Option<XmpValue> {
    array_value(ordered, values.iter().map(XmpItem::simple).collect())
}

/// A value parsed as an XMP `Real`; text that does not parse reads as absent.
pub(crate) fn parse_number(value: &XmpValue) -> Option<f64> {
    value.text()?.trim().parse().ok()
}

/// An XMP `Real`, or `None` for a value the type has no form for.
///
/// `NaN` and the infinities are not values of the XMP `Real` type (Part 1 §8.2.1), so they are
/// never written — and a coordinate stating one in the graph is therefore never consumed, and is
/// kept verbatim (see the [module docs](crate::extension)). A finite value is written in its
/// shortest decimal form, which is why a coordinate whose decimal value a double does not hold exactly is not
/// consumed either (see [`same_number`]).
fn number_value(value: f64) -> Option<XmpValue> {
    value
        .is_finite()
        .then(|| XmpValue::Simple(value.to_string()))
}

/// An array of structure values, or `None` when there is nothing to write.
fn nested_array_value(ordered: bool, values: Vec<XmpValue>) -> Option<XmpValue> {
    array_value(ordered, values.into_iter().map(XmpItem::new).collect())
}

/// An `rdf:Seq` when `ordered` and an `rdf:Bag` otherwise, or `None` when there are no items: an
/// empty array says nothing a missing property does not.
fn array_value(ordered: bool, items: Vec<XmpItem>) -> Option<XmpValue> {
    if items.is_empty() {
        return None;
    }
    Some(XmpValue::Array(if ordered {
        XmpArray::Seq(items)
    } else {
        XmpArray::Bag(items)
    }))
}

// --- Writing helpers -------------------------------------------------------------------------

/// Appends `ns:name`, unless there is no value to write.
fn put(out: &mut Vec<XmpProperty>, ns: &str, name: &str, value: Option<XmpValue>) {
    if let Some(value) = value {
        out.push(XmpProperty::new(ns, name, value));
    }
}

/// Appends `ns:name` as simple text, unless the value is absent.
pub(crate) fn put_text(out: &mut Vec<XmpProperty>, ns: &str, name: &str, value: Option<&String>) {
    put(out, ns, name, value.map(String::as_str).map(text_value));
}

/// Appends `ns:name` as a language alternative holding one `x-default` item, unless absent.
pub(crate) fn put_lang_alt(
    out: &mut Vec<XmpProperty>,
    ns: &str,
    name: &str,
    value: Option<&String>,
) {
    put(out, ns, name, value.map(String::as_str).map(lang_alt_value));
}

/// Appends `ns:name` as an array of simple text, unless the list is empty.
pub(crate) fn put_list(
    out: &mut Vec<XmpProperty>,
    ns: &str,
    name: &str,
    ordered: bool,
    values: &[String],
) {
    put(out, ns, name, list_value(ordered, values));
}

/// Appends `ns:name` as an XMP `Real`, unless the value is absent or has no `Real` form.
pub(crate) fn put_number(out: &mut Vec<XmpProperty>, ns: &str, name: &str, value: Option<f64>) {
    put(out, ns, name, value.and_then(number_value));
}

/// Appends `ns:name` as a single structure value, unless absent.
pub(crate) fn put_nested(
    out: &mut Vec<XmpProperty>,
    ns: &str,
    name: &str,
    value: Option<XmpValue>,
) {
    put(out, ns, name, value);
}

/// Appends `ns:name` as an array of structure values, unless the list is empty.
pub(crate) fn put_nested_array(
    out: &mut Vec<XmpProperty>,
    ns: &str,
    name: &str,
    ordered: bool,
    values: Vec<XmpValue>,
) {
    put(out, ns, name, nested_array_value(ordered, values));
}

// --- Verbatim retention of the fields a typed read did not consume ---------------------------

/// Appends the retained fields, skipping one whose `(namespace, name)` a *modelled* field already
/// emitted carries.
///
/// The retention list is public, so a caller can put a name the model also carries in it. Emitting
/// both would produce a structure with two fields of one name, which is ill-formed and does not
/// read back — so a namesake of a modelled field is dropped, but only when that field was actually
/// emitted. When it was not, the retained field is the only copy of that name and is written; and
/// two retained fields of one name are both written, because dropping either would lose a field
/// the graph carries.
pub(crate) fn put_other(out: &mut Vec<XmpProperty>, other: &[XmpProperty]) {
    let modelled = out.len();
    for property in other {
        if !out[..modelled]
            .iter()
            .any(|p| p.namespace == property.namespace && p.name == property.name)
        {
            out.push(property.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use gamut_xmp::WellKnownNs;

    use super::*;
    use crate::extension::test_support::{qualified, structured, text_value};

    #[test]
    fn reproduction_is_equality_apart_from_the_two_re_spellings() {
        // The relation the retention rule is built on, stated on its own: the same value spelled
        // another way reproduces a field, and nothing else does.
        let coordinate =
            |text: &str| XmpProperty::new(WellKnownNs::Iptc4XmpExt.uri(), "rbX", text_value(text));
        assert!(reproduces(&coordinate(" 0.50 "), &coordinate("0.5")));
        assert!(!reproduces(&coordinate("0.5"), &coordinate("0.25")));
        // A double that formats to another decimal is another value, not another spelling.
        assert!(!reproduces(
            &coordinate("12345678901234567891"),
            &coordinate("12345678901234567000")
        ));
        assert!(!reproduces(&coordinate("left"), &coordinate("right")));
        // A value of another kind is another value, and a field of another name is another field.
        assert!(!reproduces(
            &coordinate("0.5"),
            &XmpProperty::new(
                WellKnownNs::Iptc4XmpExt.uri(),
                "rbX",
                XmpValue::Uri("0.5".to_owned())
            )
        ));
        assert!(!reproduces(
            &coordinate("0.5"),
            &XmpProperty::new(WellKnownNs::Iptc4XmpExt.uri(), "rbY", text_value("0.5"))
        ));
        assert!(!reproduces(
            &coordinate("0.5"),
            &XmpProperty::new(WellKnownNs::Xmp.uri(), "rbX", text_value("0.5"))
        ));
    }

    #[test]
    fn a_number_reproduces_only_an_xmp_real_of_the_same_decimal_value() {
        // `same_number` compares decimal values under XMP Part 1 §8.2.1.4's grammar.
        // Re-spellings of one value: zeros that carry no digit, a `+` sign, an omitted part.
        for (read, emitted) in [
            ("0.50", "0.5"),
            ("05", "5"),
            ("+0.5", "0.5"),
            (".5", "0.5"),
            ("-0.50", "-0.5"),
            ("100.0", "100"),
        ] {
            assert!(same_number(read, emitted), "{read} is {emitted}");
        }
        for (read, emitted) in [
            // Another value: the sign, the integer digits and the fraction digits each count.
            ("-0.5", "0.5"),
            ("1.5", "10.5"),
            ("0.5", "0.05"),
            // A double that formats to another decimal: more digits than it keeps, an underflow.
            ("12345678901234567891", "12345678901234567000"),
            ("0.1000000000000000000001", "0.1"),
            // Not an XMP Real at all: an exponent, a bare point, a point with no fraction, a
            // sign or nothing on its own, a sign after a sign.
            ("1e-400", "0"),
            ("5e0", "5"),
            (".", "0"),
            ("5.", "5"),
            ("", "0"),
            ("-", "-0"),
            ("+-5", "-5"),
            ("NaN", "NaN"),
        ] {
            assert!(!same_number(read, emitted), "{read} is not {emitted}");
        }
    }

    #[test]
    fn a_uri_reproduces_only_a_uri_of_the_same_target() {
        // `rdf:resource` is a value kind of its own (XMP Part 1 §6.3.2), so a URL field is
        // reproduced only by emitting the same URI again — the clause that makes a `Uri` field
        // consumable at all rather than retained.
        let url = |target: &str| {
            XmpProperty::new(
                WellKnownNs::Iptc4XmpExt.uri(),
                "AOSourceInvURL",
                XmpValue::Uri(target.to_owned()),
            )
        };
        assert!(reproduces(
            &url("https://example.test/a"),
            &url("https://example.test/a")
        ));
        assert!(!reproduces(
            &url("https://example.test/a"),
            &url("https://example.test/b")
        ));
    }

    #[test]
    fn a_structure_reproduces_only_when_every_field_does() {
        // A structure's fields are an unordered set (Part 1 §6.3.3), so each one has to be found
        // in the other structure; counting them is not enough.
        let creator = |name: &str| {
            XmpProperty::new(
                WellKnownNs::Iptc4XmpExt.uri(),
                "AOCreator",
                structured(vec![XmpProperty::new(
                    WellKnownNs::Iptc4XmpExt.uri(),
                    "Name",
                    text_value(name),
                )]),
            )
        };
        assert!(reproduces(&creator("Human"), &creator("Human")));
        assert!(!reproduces(&creator("Human"), &creator("Another")));
    }

    #[test]
    fn a_qualifier_the_writer_does_not_emit_is_a_difference() {
        // The writer builds its properties with no qualifiers at all, so a qualified field is
        // reproduced only by a field carrying the same qualifier — matching them pairwise, not
        // by count.
        let city = || {
            XmpProperty::new(
                WellKnownNs::Iptc4XmpExt.uri(),
                "CiAdrCity",
                text_value("Paris"),
            )
        };
        assert!(reproduces(
            &qualified(city(), "fr"),
            &qualified(city(), "fr")
        ));
        assert!(!reproduces(&qualified(city(), "fr"), &city()));
        assert!(!reproduces(
            &qualified(city(), "fr"),
            &qualified(city(), "en")
        ));
    }

    #[test]
    fn only_an_xml_lang_qualifier_is_matched_case_insensitively() {
        // XMP Part 1 §8.2.2.4 matches language tags case-insensitively. No other qualifier is
        // matched that way: neither the `xml:` namespace on its own nor the name `lang` on its
        // own makes one, and an ordinary qualifier is compared by value.
        let city = || {
            XmpProperty::new(
                WellKnownNs::Iptc4XmpExt.uri(),
                "CiAdrCity",
                text_value("Paris"),
            )
        };
        let with = |namespace: &str, name: &str, text: &str| {
            let mut property = city();
            property
                .qualifiers
                .push(XmpProperty::new(namespace, name, text_value(text)));
            property
        };
        assert!(reproduces(
            &qualified(city(), "x-default"),
            &qualified(city(), "X-Default")
        ));
        assert!(!reproduces(
            &with(XML_NAMESPACE, "space", "preserve"),
            &with(XML_NAMESPACE, "space", "PRESERVE")
        ));
        assert!(!reproduces(
            &with(WellKnownNs::Iptc4XmpExt.uri(), "lang", "fr"),
            &with(WellKnownNs::Iptc4XmpExt.uri(), "lang", "FR")
        ));
        assert!(!reproduces(
            &with(WellKnownNs::Iptc4XmpExt.uri(), "role", "editor"),
            &with(WellKnownNs::Iptc4XmpExt.uri(), "role", "author")
        ));
    }
}
