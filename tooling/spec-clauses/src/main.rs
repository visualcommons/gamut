//! `spec-clauses` — extract a clause index (number → heading title) from a vendored specification.
//!
//! ```text
//! spec-clauses <spec-file>...   > clauses.tsv
//! ```
//!
//! Each input is read by its extension: `.html` takes the W3C/ReSpec path ([`html`]), `.pdf` the
//! PDF path ([`pdf`] + [`text`]). The output is one TSV with a header row and the columns
//! `spec`, `clause`, `title`, where `spec` is the input's file stem (`icc.1-2022-05`, `png-3`), so
//! one directory's editions share one index and a citation can name which one it means.
//!
//! The clause numbers are the **headings'** numbers (issue #606): where a specification's own
//! cross-references number a clause differently from its heading — ICC.1:2022 §8.4.3 calls
//! `redMatrixColumnTag` 9.2.44, its heading says 9.2.46 — only the heading is read, so the index
//! has one answer and the cross-reference numbering is simply absent from it.
//!
//! The extractor is deliberately not a general PDF-to-text tool. It reads exactly what a heading
//! needs — page content text, its font, and its position — and fails loudly on anything else it
//! cannot decode rather than guessing, because a silently dropped heading turns into a citation
//! the lint wrongly calls missing.

mod html;
mod pdf;
mod text;

use std::io::{self, Write};
use std::path::Path;
use std::process::ExitCode;

/// One heading: the clause number as the specification prints it on the heading (without a
/// trailing period), and its title on one line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Clause {
    /// `9.2.10`, `11.3.2.1`, `A`, `A.3.2`.
    pub number: String,
    /// The heading text after the number, whitespace collapsed.
    pub title: String,
}

fn main() -> ExitCode {
    let mut inputs: Vec<String> = std::env::args().skip(1).collect();
    if inputs.first().is_some_and(|a| a == "--lines") {
        // Diagnostic: every text line the PDF path assembles, `B` marking a bold start — what a
        // heading the index lacks actually looked like to the extractor.
        for input in &inputs[1..] {
            let lines = std::fs::read(input)
                .map_err(|e| e.to_string())
                .and_then(|b| pdf::Document::parse(&b))
                .and_then(|d| text::lines(&d));
            match lines {
                Ok(ls) => ls
                    .iter()
                    .for_each(|l| println!("{}\t{}", if l.bold { "B" } else { "-" }, l.text)),
                Err(e) => {
                    eprintln!("spec-clauses: {input}: {e}");
                    return ExitCode::FAILURE;
                }
            }
        }
        return ExitCode::SUCCESS;
    }
    inputs.retain(|a| !a.is_empty());
    if inputs.is_empty() {
        eprintln!("usage: spec-clauses <spec.html|spec.pdf>...  > clauses.tsv");
        eprintln!("       spec-clauses --lines <spec.pdf>...       (diagnostic text dump)");
        return ExitCode::from(2);
    }
    let mut out = String::from("spec\tclause\ttitle\n");
    for input in &inputs {
        let path = Path::new(input);
        let clauses = match extract(path) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("spec-clauses: {input}: {e}");
                return ExitCode::FAILURE;
            }
        };
        if clauses.is_empty() {
            eprintln!("spec-clauses: {input}: no clause headings found");
            return ExitCode::FAILURE;
        }
        let stem = path
            .file_stem()
            .map_or_else(String::new, |s| s.to_string_lossy().into_owned());
        for c in dedup(clauses, input) {
            out.push_str(&format!("{stem}\t{}\t{}\n", c.number, c.title));
        }
    }
    if io::stdout().write_all(out.as_bytes()).is_err() {
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

fn extract(path: &Path) -> Result<Vec<Clause>, String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    match path.extension().and_then(|e| e.to_str()) {
        Some("html" | "htm") => {
            let s = String::from_utf8(bytes).map_err(|e| e.to_string())?;
            Ok(html::clauses(&s))
        }
        Some("pdf") => {
            let doc = pdf::Document::parse(&bytes)?;
            let lines = text::lines(&doc)?;
            Ok(text::headings(&lines))
        }
        _ => Err("unsupported extension (want .html or .pdf)".into()),
    }
}

/// Keeps the first heading for each clause number. A repeat is a running header or a heading the
/// layout split, never a second clause, so it is reported and dropped rather than emitted twice.
fn dedup(clauses: Vec<Clause>, input: &str) -> Vec<Clause> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(clauses.len());
    for c in clauses {
        if seen.insert(c.number.clone()) {
            out.push(c);
        } else {
            eprintln!(
                "spec-clauses: {input}: clause {} repeated (\"{}\"), first kept",
                c.number, c.title
            );
        }
    }
    out
}

/// Collapses every run of whitespace (including tabs and newlines, which the TSV cannot carry) to
/// one space and trims the ends.
pub fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedup_keeps_the_first_heading_per_number() {
        let c = |n: &str, t: &str| Clause {
            number: n.into(),
            title: t.into(),
        };
        let got = dedup(
            vec![c("1", "Scope"), c("2", "Terms"), c("1", "Scope again")],
            "x",
        );
        assert_eq!(got, vec![c("1", "Scope"), c("2", "Terms")]);
    }

    #[test]
    fn collapse_ws_folds_tabs_and_newlines() {
        assert_eq!(collapse_ws(" a\t b\n\nc "), "a b c");
    }
}
