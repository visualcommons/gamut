//! The `--output json` writer: a hand-written [`Json`] value and the helpers every arm builds its
//! document with.

/// A half-open byte range `[start, end)` as a JSON object.
pub(super) fn span(start: impl Into<Json>, end: impl Into<Json>) -> Json {
    Json::Object(vec![("start", start.into()), ("end", end.into())])
}

/// Writes `doc` to stdout as one line of JSON.
pub(super) fn print_json(doc: &Json) {
    let mut out = String::new();
    doc.write(&mut out);
    println!("{out}");
}

/// A JSON value (RFC 8259), built by this command and written by [`Json::write`].
///
/// Hand-written rather than `serde_json`: the command emits a handful of shapes and never parses
/// JSON, and the workspace keeps `serde_json` a dev-only dependency. Object members keep the order
/// they are built in.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Json {
    /// `null`.
    Null,
    /// `true` or `false`.
    Bool(bool),
    /// A non-negative integer. Every count, size and offset this command reports is one.
    UInt(u64),
    /// A ratio. Written as `null` when it is not finite, since JSON has no NaN or infinity.
    Float(f64),
    /// A string, escaped on write.
    Str(String),
    /// An array.
    Array(Vec<Json>),
    /// An object, its members in insertion order.
    Object(Vec<(&'static str, Json)>),
}

impl Json {
    /// Appends the compact JSON text of `self` to `out`.
    pub(super) fn write(&self, out: &mut String) {
        use std::fmt::Write as _;
        match self {
            Self::Null => out.push_str("null"),
            Self::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            // `fmt::Write` for `String` cannot fail.
            Self::UInt(n) => {
                let _ = write!(out, "{n}");
            }
            // `f64`'s `Display` never uses an exponent and always yields a JSON number for a
            // finite value (`2`, `0.5`, `-0`).
            Self::Float(x) if x.is_finite() => {
                let _ = write!(out, "{x}");
            }
            Self::Float(_) => out.push_str("null"),
            Self::Str(s) => write_json_string(s, out),
            Self::Array(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    item.write(out);
                }
                out.push(']');
            }
            Self::Object(members) => {
                out.push('{');
                for (i, (key, value)) in members.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write_json_string(key, out);
                    out.push(':');
                    value.write(out);
                }
                out.push('}');
            }
        }
    }
}

/// Appends `s` as a JSON string literal: RFC 8259 §7 requires `"`, `\` and every control
/// character below U+0020 to be escaped. Everything else is written as itself — the text is
/// already UTF-8, so no other escape is needed, and a file-chosen byte (a chunk type, a path)
/// cannot leave the literal.
fn write_json_string(s: &str, out: &mut String) {
    use std::fmt::Write as _;
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if u32::from(c) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

impl From<bool> for Json {
    fn from(b: bool) -> Self {
        Self::Bool(b)
    }
}

impl From<&str> for Json {
    fn from(s: &str) -> Self {
        Self::Str(s.to_owned())
    }
}

impl From<String> for Json {
    fn from(s: String) -> Self {
        Self::Str(s)
    }
}

impl From<u8> for Json {
    fn from(n: u8) -> Self {
        Self::UInt(n.into())
    }
}

impl From<u16> for Json {
    fn from(n: u16) -> Self {
        Self::UInt(n.into())
    }
}

impl From<u32> for Json {
    fn from(n: u32) -> Self {
        Self::UInt(n.into())
    }
}

impl From<u64> for Json {
    fn from(n: u64) -> Self {
        Self::UInt(n)
    }
}

impl From<usize> for Json {
    fn from(n: usize) -> Self {
        // `usize` is at most 64 bits on every target Rust supports.
        Self::UInt(n as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::Json;

    /// The compact text of `value`.
    fn json_text(value: &Json) -> String {
        let mut out = String::new();
        value.write(&mut out);
        out
    }

    #[test]
    fn a_json_string_escapes_what_rfc_8259_requires_and_nothing_else() {
        // A path or a chunk type is chosen by the input, so a quote, a backslash or a control
        // character in it must stay inside the literal; anything else is UTF-8 and passes through.
        assert_eq!(
            json_text(&Json::from("a\"b\\c\nd\re\tf\u{0}g\u{1f}h é…")),
            "\"a\\\"b\\\\c\\nd\\re\\tf\\u0000g\\u001fh é…\""
        );
        // U+007F is not a control character RFC 8259 asks to escape.
        assert_eq!(json_text(&Json::from("\u{7f}")), "\"\u{7f}\"");
    }

    #[test]
    fn a_json_document_is_written_compactly_in_member_order() {
        let doc = Json::Object(vec![
            ("z", Json::Array(vec![Json::from(1u8), Json::Null])),
            ("a", Json::Bool(false)),
            ("m", Json::Object(vec![])),
            ("e", Json::Array(vec![])),
            ("t", Json::Bool(true)),
            ("big", Json::from(u64::MAX)),
        ]);
        assert_eq!(
            json_text(&doc),
            "{\"z\":[1,null],\"a\":false,\"m\":{},\"e\":[],\"t\":true,\"big\":18446744073709551615}"
        );
    }

    #[test]
    fn a_json_float_is_a_number_when_finite_and_null_otherwise() {
        // JSON has no NaN or infinity; a ratio over an empty stream must not break the document.
        assert_eq!(json_text(&Json::Float(0.5)), "0.5");
        assert_eq!(json_text(&Json::Float(2.0)), "2");
        assert_eq!(json_text(&Json::Float(f64::NAN)), "null");
        assert_eq!(json_text(&Json::Float(f64::INFINITY)), "null");
        assert_eq!(json_text(&Json::Float(f64::NEG_INFINITY)), "null");
    }
}
