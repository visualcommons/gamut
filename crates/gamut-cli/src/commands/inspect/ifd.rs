//! The TIFF and DNG arms: one byte accounting over `gamut-ifd`'s vocabulary, which both formats
//! re-export, with its C2PA section beside it.

use gamut::ifd::SegmentReport;

use super::c2pa::{ifd_c2pa_json, print_c2pa};
use super::json::{Json, print_json, span};
use super::{
    Format, Listed, MAX_LIST, Output, Target, document_head, format_name, print_lines_of, yes_no,
};
use crate::error::CliError;

/// Deconstructs a TIFF and prints its byte accounting, then its C2PA section, exiting non-zero
/// when the file is not fully accounted for.
pub(super) fn inspect_tiff(to: Target<'_>, data: &[u8]) -> Result<(), CliError> {
    let summary = summarize(gamut::tiff::deconstruct(data)?);
    report(
        to,
        Format::Tiff,
        &summary,
        &gamut::tiff::c2pa_summary(data)?,
    )
}

/// [`inspect_tiff`] for a DNG, which reports in the same vocabulary.
pub(super) fn inspect_dng(to: Target<'_>, data: &[u8]) -> Result<(), CliError> {
    let summary = summarize(gamut::dng::deconstruct(data)?);
    report(to, Format::Dng, &summary, &gamut::dng::c2pa_summary(data)?)
}

/// Prints a TIFF or DNG report and reaches its verdict.
///
/// The C2PA summary is read before anything is printed, so a file whose summary cannot be built
/// prints no half report. It cannot fail once the deconstruct has succeeded: both walk the same
/// main IFD chain under the same guards, and the summary lists an unreadable store rather than
/// erroring on it.
fn report(
    to: Target<'_>,
    format: Format,
    summary: &Summary,
    c2pa: &gamut::tiff::C2paSummary,
) -> Result<(), CliError> {
    match to.output {
        Output::Text => {
            print_summary(to.path, format, summary);
            // Additive: the section prints beside the byte accounting and never moves the verdict
            // below.
            print_c2pa(
                c2pa.summary_lines(),
                c2pa.detail_lines(),
                c2pa.detail_line_count(),
            );
        }
        Output::Json => {
            let mut doc = document_head(to, format);
            doc.extend(summary_json(summary));
            doc.push(("c2pa", ifd_c2pa_json(c2pa)));
            print_json(&Json::Object(doc));
        }
    }

    if summary.fully_accounted {
        Ok(())
    } else {
        Err(CliError::NotFullyAccounted(format!(
            "{}: not fully accounted — {} unclassified byte(s), {} unknown tag(s), {} unknown field type(s), {} anomaly/ies",
            to.path.display(),
            summary.segments.unclassified_bytes(),
            summary.unknown_tags.len(),
            summary.unknown_fields.len(),
            summary.anomalies.len(),
        )))
    }
}

/// A format-agnostic view of a deconstruct report, for printing.
///
/// Every listed entry carries both renderings, built from the same fields in the same place, so
/// the text report and the JSON document cannot list different things.
struct Summary {
    segments: SegmentReport,
    unknown_fields: Vec<Listed>,
    unknown_tags: Vec<Listed>,
    anomalies: Vec<Listed>,
    fully_classified: bool,
    fully_accounted: bool,
}

/// Collapses a deconstruct report into the printable [`Summary`]. TIFF and DNG report in one
/// vocabulary — gamut-ifd's, which both re-export — so one rendering serves both.
fn summarize(report: gamut::tiff::DeconstructReport) -> Summary {
    use gamut::tiff::Anomaly;
    let fully_classified = report.is_fully_classified();
    let fully_accounted = report.is_fully_accounted();
    let unknown_fields = report
        .unknown_fields
        .iter()
        .map(|u| unknown_field(u.page, u.tag, u.type_code, u.count))
        .collect();
    let unknown_tags = report
        .unknown_tags
        .iter()
        .map(|u| unknown_tag(u.page, u.tag, u.field_type, u.count))
        .collect();
    let anomalies = report
        .anomalies
        .iter()
        .map(|a| match a {
            Anomaly::UnknownCode {
                page,
                tag,
                code,
                detail,
                ..
            } => anomaly_unknown_code(*page, *tag, *code, detail),
            Anomaly::UnparsableTag {
                page, tag, detail, ..
            } => anomaly_unparsable_tag(*page, *tag, detail),
            Anomaly::Structure {
                page,
                detail,
                severity,
                ..
            } => anomaly_structure(*page, detail, severity_level(*severity)),
            Anomaly::DuplicateTag {
                ifd,
                tag,
                entries,
                severity,
                ..
            } => anomaly_duplicate_tag(
                format!("{a:?}"),
                *ifd,
                *tag,
                *entries,
                severity_level(*severity),
            ),
            // `Anomaly` is non-exhaustive; render future categories generically.
            other => anomaly_other(format!("{other:?}")),
        })
        .collect();
    Summary {
        segments: report.segments,
        unknown_fields,
        unknown_tags,
        anomalies,
        fully_classified,
        fully_accounted,
    }
}

/// An anomaly severity as this command knows it. `Other` is a level the format crate added after
/// this command was written (`Severity` is non-exhaustive), or the unknown severity of an anomaly
/// category this command does not know yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Level {
    Warning,
    Error,
    Other,
}

impl Level {
    /// The text report's bracketed label. An unknown level prints as `error`, the conservative
    /// label, exactly as the text report always has.
    fn text(self) -> &'static str {
        match self {
            Self::Warning => "warning",
            Self::Error | Self::Other => "error",
        }
    }

    /// The JSON `severity` identifier. An unknown level is `"other"`, as every unknown enumerated
    /// value in the document is, so the document never names an unknown level as a known one.
    fn json(self) -> &'static str {
        match self {
            Self::Warning => "warning",
            Self::Error => "error",
            Self::Other => "other",
        }
    }
}

/// Classifies an anomaly severity (TIFF and DNG share the type).
fn severity_level(severity: gamut::tiff::Severity) -> Level {
    match severity {
        gamut::tiff::Severity::Warning => Level::Warning,
        gamut::tiff::Severity::Error => Level::Error,
        // `Severity` is non-exhaustive.
        _ => Level::Other,
    }
}

/// An unknown field type, shared by TIFF and DNG, whose reports carry the same four facts.
fn unknown_field(page: usize, tag: u16, type_code: u16, count: u64) -> Listed {
    Listed {
        line: format!("page {page} tag {tag:#06x} type {type_code} (count {count})"),
        json: Json::Object(vec![
            ("page", Json::from(page)),
            ("tag", Json::from(tag)),
            ("type_code", Json::from(type_code)),
            ("count", Json::from(count)),
        ]),
    }
}

/// An unknown tag, shared by TIFF and DNG.
fn unknown_tag(page: usize, tag: u16, field_type: u16, count: u64) -> Listed {
    Listed {
        line: format!("page {page} tag {tag:#06x} (type {field_type}, count {count})"),
        json: Json::Object(vec![
            ("page", Json::from(page)),
            ("tag", Json::from(tag)),
            ("field_type", Json::from(field_type)),
            ("count", Json::from(count)),
        ]),
    }
}

/// An `Anomaly::UnknownCode`, shared by TIFF and DNG.
fn anomaly_unknown_code(page: usize, tag: u16, code: u32, detail: &str) -> Listed {
    Listed {
        line: format!("[error] page {page}: {detail} (tag {tag:#06x}, code {code})"),
        json: Json::Object(vec![
            ("kind", Json::from("unknown_code")),
            ("severity", Json::from("error")),
            ("page", Json::from(page)),
            ("tag", Json::from(tag)),
            ("code", Json::from(code)),
            ("detail", Json::from(detail)),
        ]),
    }
}

/// An `Anomaly::UnparsableTag`, shared by TIFF and DNG.
fn anomaly_unparsable_tag(page: usize, tag: u16, detail: &str) -> Listed {
    Listed {
        line: format!("[error] page {page}: {detail} (tag {tag:#06x})"),
        json: Json::Object(vec![
            ("kind", Json::from("unparsable_tag")),
            ("severity", Json::from("error")),
            ("page", Json::from(page)),
            ("tag", Json::from(tag)),
            ("detail", Json::from(detail)),
        ]),
    }
}

/// An `Anomaly::Structure`, shared by TIFF and DNG.
fn anomaly_structure(page: usize, detail: &str, severity: Level) -> Listed {
    Listed {
        line: format!("[{}] page {page}: {detail}", severity.text()),
        json: Json::Object(vec![
            ("kind", Json::from("structure")),
            ("severity", Json::from(severity.json())),
            ("page", Json::from(page)),
            ("detail", Json::from(detail)),
        ]),
    }
}

/// An `Anomaly::DuplicateTag`, shared by TIFF and DNG. Its text line is the generic rendering it
/// has always had (`debug` is the anomaly's `Debug` form); the JSON object names its fields.
fn anomaly_duplicate_tag(
    debug: String,
    ifd: u64,
    tag: u16,
    entries: usize,
    severity: Level,
) -> Listed {
    Listed {
        line: format!("[error] {debug}"),
        json: Json::Object(vec![
            ("kind", Json::from("duplicate_tag")),
            ("severity", Json::from(severity.json())),
            ("ifd_offset", Json::from(ifd)),
            ("tag", Json::from(tag)),
            ("entries", Json::from(entries)),
        ]),
    }
}

/// An anomaly of a category this command does not know yet (`Anomaly` is non-exhaustive),
/// rendered from its `Debug` form. Its severity is unknown too: the text label is the
/// conservative `error` it always was, the JSON identifier is `"other"`.
fn anomaly_other(debug: String) -> Listed {
    Listed {
        line: format!("[{}] {debug}", Level::Other.text()),
        json: Json::Object(vec![
            ("kind", Json::from("other")),
            ("severity", Json::from(Level::Other.json())),
            ("detail", Json::from(debug)),
        ]),
    }
}

/// Prints the human-readable report to stdout.
fn print_summary(path: &std::path::Path, format: Format, s: &Summary) {
    let seg = &s.segments;
    let classified = seg.file_len - seg.unclassified_bytes();
    let pct = if seg.file_len == 0 {
        100.0
    } else {
        classified as f64 / seg.file_len as f64 * 100.0
    };
    println!("inspecting {} as {}", path.display(), format_name(format));
    println!(
        "  classified:    {classified} / {} bytes ({pct:.1}%) across {} segment(s)",
        seg.file_len,
        seg.segments.len()
    );
    println!("  unclassified:  {} bytes", seg.unclassified_bytes());

    print_ranges(
        "unclassified ranges",
        &seg.unclassified
            .iter()
            .map(|g| (g.start, g.end - g.start))
            .collect::<Vec<_>>(),
    );
    if !seg.conflicts.is_empty() {
        println!("  conflicts:     {}", seg.conflicts.len());
        for c in seg.conflicts.iter().take(MAX_LIST) {
            println!(
                "    - [{}, {}) overlaps [{}, {})",
                c.b.range.start, c.b.range.end, c.a.range.start, c.a.range.end
            );
        }
    }
    if !seg.shared.is_empty() {
        println!("  shared values: {} (legal TIFF sharing)", seg.shared.len());
    }
    print_ranges(
        "out-of-bounds claims",
        &seg.out_of_bounds
            .iter()
            .map(|o| (o.range.start, o.range.end - o.range.start))
            .collect::<Vec<_>>(),
    );
    // The dual-ledger parser cross-check: non-empty lists here are gamut bugs, not file defects.
    print_ranges(
        "unclaimed reads (parser defect)",
        &seg.unclaimed_reads
            .iter()
            .map(|r| (r.start, r.end - r.start))
            .collect::<Vec<_>>(),
    );
    print_ranges(
        "unread claims (parser defect)",
        &seg.unread_claims
            .iter()
            .map(|c| (c.range.start, c.range.end - c.range.start))
            .collect::<Vec<_>>(),
    );

    print_lines("unknown field types", &s.unknown_fields);
    print_lines("unknown tags", &s.unknown_tags);
    print_lines("anomalies", &s.anomalies);

    println!("  fully classified: {}", yes_no(s.fully_classified));
    println!("  fully accounted:  {}", yes_no(s.fully_accounted));
}

/// The JSON members of a TIFF/DNG report: the facts [`print_summary`] prints, every list uncapped.
fn summary_json(s: &Summary) -> Vec<(&'static str, Json)> {
    let seg = &s.segments;
    let ranges = |ranges: &mut dyn Iterator<Item = &core::ops::Range<u64>>| {
        Json::Array(ranges.map(|r| span(r.start, r.end)).collect())
    };
    let accounting = Json::Object(vec![
        ("file_len", Json::from(seg.file_len)),
        (
            "classified_bytes",
            Json::from(seg.file_len - seg.unclassified_bytes()),
        ),
        ("unclassified_bytes", Json::from(seg.unclassified_bytes())),
        ("segment_count", Json::from(seg.segments.len())),
        ("unclassified", ranges(&mut seg.unclassified.iter())),
        (
            "conflicts",
            Json::Array(
                seg.conflicts
                    .iter()
                    .map(|c| {
                        Json::Object(vec![
                            ("a", span(c.a.range.start, c.a.range.end)),
                            ("b", span(c.b.range.start, c.b.range.end)),
                        ])
                    })
                    .collect(),
            ),
        ),
        ("shared_values", Json::from(seg.shared.len())),
        (
            "out_of_bounds",
            ranges(&mut seg.out_of_bounds.iter().map(|o| &o.range)),
        ),
        ("unclaimed_reads", ranges(&mut seg.unclaimed_reads.iter())),
        (
            "unread_claims",
            ranges(&mut seg.unread_claims.iter().map(|c| &c.range)),
        ),
    ]);
    let listed = |list: &[Listed]| Json::Array(list.iter().map(|l| l.json.clone()).collect());
    vec![
        ("accounting", accounting),
        ("unknown_field_types", listed(&s.unknown_fields)),
        ("unknown_tags", listed(&s.unknown_tags)),
        ("anomalies", listed(&s.anomalies)),
        ("fully_classified", Json::Bool(s.fully_classified)),
        ("fully_accounted", Json::Bool(s.fully_accounted)),
    ]
}

/// Prints a `(start, len)` range list under `label`, truncating past [`MAX_LIST`].
fn print_ranges(label: &str, ranges: &[(u64, u64)]) {
    if ranges.is_empty() {
        return;
    }
    println!("  {label}: {}", ranges.len());
    for (start, len) in ranges.iter().take(MAX_LIST) {
        println!("    - {len} bytes at offset {start}");
    }
    if ranges.len() > MAX_LIST {
        println!("    … and {} more", ranges.len() - MAX_LIST);
    }
}

/// Prints the text lines of a [`Listed`] list under `label`, truncating past [`MAX_LIST`].
fn print_lines(label: &str, listed: &[Listed]) {
    let lines: Vec<String> = listed
        .iter()
        .take(MAX_LIST)
        .map(|l| l.line.clone())
        .collect();
    print_lines_of(label, &lines, listed.len());
}

#[cfg(test)]
mod tests {
    use super::{Json, Level, anomaly_other};

    #[test]
    fn an_unknown_severity_is_other_in_json_and_error_in_text() {
        // docs/inspect-exit-codes.md: every unknown enumerated value, `severity` included, is
        // `"other"` in the document; the text report keeps its conservative `[error]` label.
        assert_eq!(Level::Other.json(), "other");
        assert_eq!(Level::Other.text(), "error");
        let other = anomaly_other("Future".to_owned());
        assert_eq!(other.line, "[error] Future");
        assert_eq!(
            other.json,
            Json::Object(vec![
                ("kind", Json::from("other")),
                ("severity", Json::from("other")),
                ("detail", Json::from("Future")),
            ])
        );
        // The two known levels keep one word in both renderings.
        for (level, word) in [(Level::Warning, "warning"), (Level::Error, "error")] {
            assert_eq!(level.json(), word);
            assert_eq!(level.text(), word);
        }
    }
}
