//! The HTML path: a W3C (ReSpec-rendered) specification such as PNG Third Edition.
//!
//! ReSpec numbers every section heading with a `<bdi class="secno">N </bdi>` prefix inside the
//! `<hN>` element, so a heading is exactly an `<h1>`–`<h6>` that carries one. Unnumbered headings
//! (Abstract, Status of This Document, the per-section "Introduction" subheads) carry none and are
//! not clauses. The table of contents repeats every number, but inside `<li>`, not `<hN>`.

use crate::{Clause, collapse_ws};

const SECNO: &str = "<bdi class=\"secno\">";

/// Every numbered heading in document order.
pub fn clauses(html: &str) -> Vec<Clause> {
    let mut out = Vec::new();
    let mut rest = html;
    while let Some(open) = find_heading_open(rest) {
        let level = rest.as_bytes()[open + 2];
        let close_tag = format!("</h{}>", level as char);
        let after = &rest[open..];
        let Some(close) = after.find(&close_tag) else {
            break;
        };
        let element = &after[..close];
        rest = &after[close + close_tag.len()..];
        let Some(s) = element.find(SECNO) else {
            continue;
        };
        let inner = &element[s + SECNO.len()..];
        let Some(e) = inner.find("</bdi>") else {
            continue;
        };
        let number = collapse_ws(&decode_entities(&strip_tags(&inner[..e])));
        let number = number.trim_end_matches('.').to_string();
        let title = collapse_ws(&decode_entities(&strip_tags(&inner[e + "</bdi>".len()..])));
        if !number.is_empty() && !title.is_empty() {
            out.push(Clause { number, title });
        }
    }
    out
}

/// Byte offset of the next `<h1`..`<h6` that opens an element (followed by `>` or whitespace).
fn find_heading_open(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    let mut from = 0;
    while let Some(i) = s[from..].find("<h") {
        let at = from + i;
        if let (Some(d), Some(n)) = (b.get(at + 2), b.get(at + 3))
            && (b'1'..=b'6').contains(d)
            && (*n == b'>' || n.is_ascii_whitespace())
        {
            return Some(at);
        }
        from = at + 2;
    }
    None
}

/// Drops every `<...>` tag, keeping the text between them.
fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for ch in s.chars() {
        match ch {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    out
}

/// Decodes the named entities a heading uses plus every numeric character reference. An unknown
/// named entity is kept verbatim, so it is visible in the TSV rather than silently lost.
fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let tail = &rest[amp..];
        let Some(semi) = tail.find(';').filter(|&i| i <= 10) else {
            out.push('&');
            rest = &tail[1..];
            continue;
        };
        let name = &tail[1..semi];
        let decoded = match name {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some(' '),
            _ => name
                .strip_prefix("#x")
                .or_else(|| name.strip_prefix("#X"))
                .and_then(|h| u32::from_str_radix(h, 16).ok())
                .or_else(|| name.strip_prefix('#').and_then(|d| d.parse().ok()))
                .and_then(char::from_u32),
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &tail[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbered_headings_are_clauses_and_unnumbered_ones_are_not() {
        let html = r#"<h2>Abstract</h2>
<h5 id="x"><bdi class="secno">11.3.1.1 </bdi><span class="chunk">tRNS</span> Transparency</h5>
<h3 id="i">Introduction</h3>
<h2 class="Annex" id="n"><bdi class="secno">B. </bdi>Guidelines &amp; <abbr title="t">CRC</abbr></h2>"#;
        let got = clauses(html);
        assert_eq!(
            got,
            vec![
                Clause {
                    number: "11.3.1.1".into(),
                    title: "tRNS Transparency".into()
                },
                Clause {
                    number: "B".into(),
                    title: "Guidelines & CRC".into()
                },
            ]
        );
    }

    #[test]
    fn a_header_element_is_not_a_heading() {
        assert!(clauses("<header><bdi class=\"secno\">1 </bdi>x</header>").is_empty());
    }

    #[test]
    fn entities_decode_and_unknown_ones_stay_visible() {
        assert_eq!(
            decode_entities("a&#x41;&#66;&nbsp;&bogus;&"),
            "aAB &bogus;&"
        );
    }
}
