//! `gamut inspect` — strict "deconstruct" of a TIFF, DNG or PNG (issues #197/#263/#224).
//!
//! Walks the entire container, classifies every byte into typed segments, and flags anything
//! unrecognised (unknown tags, unknown field types, out-of-spec codes, unclassified bytes).
//! Prints a report to stdout and exits non-zero when the file is not fully accounted for —
//! usable as an archival CI gate.
//!
//! The contract itself — the gate per format, the two exit codes, the budgets, and every reason
//! the PNG filter scan declines — is recorded in `docs/inspect-exit-codes.md`, which is normative
//! for it. What follows is why the code is shaped that way.
//!
//! # What "fully accounted for" means, and what the exit code is
//!
//! Exit 0 is the file having nothing the walk can hold against it; exit 1 is a finding. Each
//! format states that in its own vocabulary, and the two are deliberately the same strength:
//!
//! - **TIFF / DNG** — `is_fully_accounted()`: every byte classified, *and* no unknown field
//!   type, no unknown tag, and no anomaly.
//! - **PNG** — `is_verified()`: `is_intact()` (every byte classified, every chunk CRC valid, IEND
//!   present, no trailing bytes after it, no truncated tail, nothing the filter scan found
//!   damaging) *and* the filter scan actually ran.
//!
//! PNG's `is_fully_classified()` is **not** the gate, though it is printed: it is true by
//! construction for every file `deconstruct` accepts (a truncated tail and a trailer each get a
//! segment of their own, so the tiling still covers the file), and gating on it would exit 0 on a
//! truncated PNG. It exists so that a walk *bug* makes the predicate false.
//!
//! `is_intact()` is **not** the gate either, and the difference is the reason `is_verified` exists.
//! A PNG whose filter scan was skipped for budget is not *damaged* — nothing is known to be wrong
//! with it — so it is not a finding, and `intact: yes` is printed truthfully. But a corrupt zlib
//! payload under a valid CRC is damage only the scan can see, so an unread file is one this
//! command cannot vouch for, and exiting 0 on it would report this reader's budget as a property
//! of the file. Such a file exits non-zero saying it was not verified, distinctly from a damaged
//! one. To keep that rare, the walk's budget here is a gigabyte rather than the decoder's 64 MiB,
//! which is past any real image — at the decoder's budget every PNG over 4096x4096 RGBA8 would go
//! unread. That gigabyte bounds the *image*, not what a small file may inflate to: past the
//! decoder's default budget the walk also refuses, before inflating, a stream that would grow to
//! more than sixty-four times its own length, so a megabyte declaring a 16k×16k header over a zlib
//! stream of zeros is reported as not verified — for the stream's implausible inflation, which is
//! a distinct reason from the image being over budget, since that image is exactly the gigabyte
//! this command admits — and never inflated to a gigabyte.
//!
//! The gate is therefore **asymmetric across formats, and deliberately so**. A TIFF or DNG walk
//! reads directories and tags, never pixel data, so there is no step in it this reader can decline
//! and `is_fully_accounted()` never depends on the reader's budget. A PNG's verification step *is*
//! an inflation, and inflation can be declined; so PNG alone has a third outcome — not damaged,
//! not verified — and exits non-zero for it with its own message, distinct from a damaged file's.
//! Gating PNG on `is_intact()` instead would make the two formats symmetric in wording and
//! asymmetric in strength: a TIFF's exit 0 means the walk read everything, and a PNG's would not.
//!
//! For PNG the same walk answers a second question: **where did the bytes go?** The report carries
//! the per-chunk-type breakdown, the compressed IDAT total against the filtered stream it inflates
//! to, and the scanline filter distribution — which is what makes an encoder comparison possible
//! from the command line, on files this crate did not write.

use std::path::PathBuf;

use clap::{Args, ValueEnum};
use gamut::ifd::SegmentReport;

use crate::error::CliError;

/// The most list entries to print per category before truncating.
const MAX_LIST: usize = 20;

/// The `DNGVersion` tag (50706) — present in every DNG, absent in plain TIFF; the format sniff.
const DNG_VERSION_TAG: u16 = 50706;

/// Arguments for `gamut inspect`.
#[derive(Args)]
pub(crate) struct InspectArgs {
    /// Input TIFF, DNG or PNG file.
    input: PathBuf,
    /// Force the container format instead of auto-detecting it.
    #[arg(long, value_enum)]
    format: Option<Format>,
}

/// The container format to deconstruct as.
#[derive(Clone, Copy, ValueEnum)]
pub(crate) enum Format {
    /// TIFF 6.0 / BigTIFF (gamut-tiff).
    Tiff,
    /// DNG (Adobe Digital Negative; gamut-dng).
    Dng,
    /// PNG (gamut-png).
    Png,
}

/// A format-agnostic view of a deconstruct report, for printing.
struct Summary {
    segments: SegmentReport,
    unknown_fields: Vec<String>,
    unknown_tags: Vec<String>,
    anomalies: Vec<String>,
    fully_classified: bool,
    fully_accounted: bool,
}

/// Runs the `inspect` command: deconstruct the file, print the report, and exit non-zero if it is
/// not fully accounted for.
pub(crate) fn run(args: &InspectArgs) -> Result<(), CliError> {
    let data = std::fs::read(&args.input).map_err(|source| CliError::Io {
        path: args.input.clone(),
        source,
    })?;
    let format = args.format.unwrap_or_else(|| sniff(&data));

    // PNG's report is a different shape -- it has no IFD tree and no tag vocabulary, but it does
    // carry compression figures the others have no equivalent for -- so it prints on its own path
    // rather than being flattened into `Summary`.
    if matches!(format, Format::Png) {
        return inspect_png(&args.input, &data);
    }

    let summary = match format {
        Format::Dng => summarize_dng(gamut::dng::deconstruct(&data)?),
        Format::Tiff => summarize_tiff(gamut::tiff::deconstruct(&data)?),
        Format::Png => unreachable!("handled above"),
    };

    print_summary(&args.input, format, &summary);

    if summary.fully_accounted {
        Ok(())
    } else {
        Err(CliError::NotFullyAccounted(format!(
            "{}: not fully accounted — {} unclassified byte(s), {} unknown tag(s), {} unknown field type(s), {} anomaly/ies",
            args.input.display(),
            summary.segments.unclassified_bytes(),
            summary.unknown_tags.len(),
            summary.unknown_fields.len(),
            summary.anomalies.len(),
        )))
    }
}

/// The 8-byte PNG file signature (§5.2).
const PNG_SIGNATURE: [u8; 8] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];

/// Detects PNG by signature, then DNG vs TIFF: a DNG is a TIFF whose IFD 0 carries the mandatory
/// `DNGVersion` tag.
fn sniff(data: &[u8]) -> Format {
    if data.starts_with(&PNG_SIGNATURE) {
        return Format::Png;
    }
    if let Ok(file) = gamut::tiff::read(data)
        && file
            .ifds
            .first()
            .is_some_and(|ifd0| ifd0.get(DNG_VERSION_TAG).is_some())
    {
        return Format::Dng;
    }
    Format::Tiff
}

/// Collapses a TIFF report into the printable [`Summary`].
fn summarize_tiff(report: gamut::tiff::DeconstructReport) -> Summary {
    use gamut::tiff::Anomaly;
    let fully_classified = report.is_fully_classified();
    let fully_accounted = report.is_fully_accounted();
    let unknown_fields = report
        .unknown_fields
        .iter()
        .map(|u| {
            format!(
                "page {} tag {:#06x} type {} (count {})",
                u.page, u.tag, u.type_code, u.count
            )
        })
        .collect();
    let unknown_tags = report
        .unknown_tags
        .iter()
        .map(|u| {
            format!(
                "page {} tag {:#06x} (type {}, count {})",
                u.page, u.tag, u.field_type, u.count
            )
        })
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
            } => {
                format!("[error] page {page}: {detail} (tag {tag:#06x}, code {code})")
            }
            Anomaly::UnparsableTag {
                page, tag, detail, ..
            } => {
                format!("[error] page {page}: {detail} (tag {tag:#06x})")
            }
            Anomaly::Structure {
                page,
                detail,
                severity,
                ..
            } => {
                format!("[{}] page {page}: {detail}", severity_label_tiff(*severity))
            }
            // `Anomaly` is non-exhaustive; render future categories generically.
            other => format!("[error] {other:?}"),
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

/// Collapses a DNG report into the printable [`Summary`].
fn summarize_dng(report: gamut::dng::DeconstructReport) -> Summary {
    use gamut::dng::Anomaly;
    let fully_classified = report.is_fully_classified();
    let fully_accounted = report.is_fully_accounted();
    let unknown_fields = report
        .unknown_fields
        .iter()
        .map(|u| {
            format!(
                "page {} tag {:#06x} type {} (count {})",
                u.page, u.tag, u.type_code, u.count
            )
        })
        .collect();
    let unknown_tags = report
        .unknown_tags
        .iter()
        .map(|u| {
            format!(
                "page {} tag {:#06x} (type {}, count {})",
                u.page, u.tag, u.field_type, u.count
            )
        })
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
            } => {
                format!("[error] page {page}: {detail} (tag {tag:#06x}, code {code})")
            }
            Anomaly::UnparsableTag {
                page, tag, detail, ..
            } => {
                format!("[error] page {page}: {detail} (tag {tag:#06x})")
            }
            Anomaly::Structure {
                page,
                detail,
                severity,
                ..
            } => {
                format!("[{}] page {page}: {detail}", severity_label_dng(*severity))
            }
            // `Anomaly` is non-exhaustive; render future categories generically.
            other => format!("[error] {other:?}"),
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

/// Renders a TIFF anomaly severity.
fn severity_label_tiff(severity: gamut::tiff::Severity) -> &'static str {
    match severity {
        gamut::tiff::Severity::Warning => "warning",
        gamut::tiff::Severity::Error => "error",
        // `Severity` is non-exhaustive; treat future levels as errors (the conservative label).
        _ => "error",
    }
}

/// Renders a DNG anomaly severity.
fn severity_label_dng(severity: gamut::dng::Severity) -> &'static str {
    match severity {
        gamut::dng::Severity::Warning => "warning",
        gamut::dng::Severity::Error => "error",
        // `Severity` is non-exhaustive; treat future levels as errors (the conservative label).
        _ => "error",
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
            .map(|g| (g.start, g.len))
            .collect::<Vec<_>>(),
    );
    if !seg.conflicts.is_empty() {
        println!("  conflicts:     {}", seg.conflicts.len());
        for c in seg.conflicts.iter().take(MAX_LIST) {
            println!(
                "    - [{}, {}) overlaps [{}, {})",
                c.b.range.start,
                c.b.range.end(),
                c.a.range.start,
                c.a.range.end()
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
            .map(|o| (o.range.start, o.range.len))
            .collect::<Vec<_>>(),
    );
    // The dual-ledger parser cross-check: non-empty lists here are gamut bugs, not file defects.
    print_ranges(
        "unclaimed reads (parser defect)",
        &seg.unclaimed_reads
            .iter()
            .map(|r| (r.start, r.len))
            .collect::<Vec<_>>(),
    );
    print_ranges(
        "unread claims (parser defect)",
        &seg.unread_claims
            .iter()
            .map(|c| (c.range.start, c.range.len))
            .collect::<Vec<_>>(),
    );

    print_lines("unknown field types", &s.unknown_fields);
    print_lines("unknown tags", &s.unknown_tags);
    print_lines("anomalies", &s.anomalies);

    println!("  fully classified: {}", yes_no(s.fully_classified));
    println!("  fully accounted:  {}", yes_no(s.fully_accounted));
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

/// Prints a pre-formatted line list under `label`, truncating past [`MAX_LIST`].
fn print_lines(label: &str, lines: &[String]) {
    print_lines_of(label, lines, lines.len());
}

/// [`print_lines`], where `lines` may already be truncated and `total` is how many there really
/// are.
///
/// Splitting the count from the list is what lets a caller whose list length is chosen by the
/// input build only the lines it will print while still reporting the true total.
fn print_lines_of(label: &str, lines: &[String], total: usize) {
    if total == 0 {
        return;
    }
    println!("  {label}: {total}");
    for line in lines.iter().take(MAX_LIST) {
        println!("    - {line}");
    }
    let hidden = hidden_entries(total, lines.len());
    if hidden > 0 {
        println!("    … and {hidden} more");
    }
}

/// How many of a `total`-entry list this print left unshown, given the `lines` it was handed.
///
/// Counted from what is actually **printed** — at most [`MAX_LIST`] of them — because the two
/// callers hide entries in different places. [`print_lines`] passes the whole list and its own
/// length, so the `MAX_LIST` cut in the loop is the only thing that hides anything; the PNG
/// caller passes a list already cut to `MAX_LIST` beside the true total, so what it hides are the
/// lines it never built. A notice derived from `total > lines.len()` alone sees only the second
/// and is dead for the first — which is how a TIFF with fifty unknown tags came to print twenty
/// of them and no indication that thirty were missing.
fn hidden_entries(total: usize, lines: usize) -> usize {
    total.saturating_sub(lines.min(MAX_LIST))
}

/// Deconstructs a PNG and prints where its bytes went, exiting non-zero when the file is not a
/// complete, undamaged datastream.
fn inspect_png(path: &std::path::Path, data: &[u8]) -> Result<(), CliError> {
    use gamut::png::{FilterScan, FilterType, SegmentKind};

    // Inspection budgets differently from decoding. `gamut::png::deconstruct`'s default matches
    // the *decoder*'s, which guards a decode against hostile input; but a file this command
    // declines to inflate is a file it cannot verify, and at the decoder's 64 MiB that is every
    // PNG past 4096x4096 RGBA8 -- an ordinary photograph. Reading it is the whole job, so the
    // ceiling is raised to a gigabyte: past any real image, short of unbounded.
    let limits = gamut::png::DeconstructLimits::default().with_max_image_bytes(1 << 30);
    let report = gamut::png::deconstruct_with_limits(data, limits)?;
    let header = report.header;

    println!("{}: PNG", path.display());
    println!(
        "  image:         {}x{} {:?} depth {}{}",
        header.width,
        header.height,
        header.color_type,
        header.bit_depth,
        if header.interlaced {
            ", Adam7 interlaced"
        } else {
            ""
        }
    );
    println!(
        "  size:          {} bytes ({:.3} bits/pixel)",
        report.file_len,
        report.bits_per_pixel()
    );
    println!(
        "  IDAT:          {} bytes compressed from {} filtered ({:.1}%)",
        report.idat_compressed,
        report.filtered_len,
        report.idat_ratio() * 100.0
    );
    println!(
        "  overhead:      {} bytes, of which {} is chunk framing",
        report.overhead_bytes(),
        report.framing_bytes()
    );

    // Truncated like every other list here: a chunk type is four unvalidated bytes, so the number
    // of distinct types is chosen by the input, not by the image.
    println!("  chunks: {}", report.chunks.len());
    for stats in report.chunks.iter().take(MAX_LIST) {
        println!(
            "    {} x{:<3} {:>9} payload + {:>4} framing{}",
            String::from_utf8_lossy(&stats.chunk_type),
            stats.count,
            stats.payload_bytes,
            stats.framing_bytes(),
            if stats.is_ancillary() {
                " (ancillary)"
            } else {
                ""
            }
        );
    }
    if report.chunks.len() > MAX_LIST {
        println!("    … and {} more", report.chunks.len() - MAX_LIST);
    }

    match report.filters {
        FilterScan::Counted(h) => {
            let n = |f| h.count(f);
            println!(
                "  filters:       None {} / Sub {} / Up {} / Average {} / Paeth {}  ({} scanlines)",
                n(FilterType::None),
                n(FilterType::Sub),
                n(FilterType::Up),
                n(FilterType::Average),
                n(FilterType::Paeth),
                h.total()
            );
        }
        FilterScan::Skipped(reason) => {
            println!(
                "  filters:       not counted — {}",
                filter_skip_label(reason)
            );
        }
    }

    if report.passes.len() > 1 {
        println!("  Adam7 passes:");
        for pass in &report.passes {
            println!(
                "    {}: {}x{}, {} row bytes, {} filtered",
                pass.index, pass.width, pass.height, pass.row_bytes, pass.filtered_len
            );
        }
    }

    // One damaged chunk yields one `String`, and the chunk count is chosen by the input, so the
    // list is built under the same bound it is printed under: the total is counted separately and
    // only the lines that will be shown are ever materialized.
    let is_damaged_segment = |seg: &gamut::png::Segment| {
        matches!(
            seg.kind,
            SegmentKind::Chunk { crc_ok: false, .. }
                | SegmentKind::Truncated
                | SegmentKind::Trailer
        )
    };
    let mut findings = report
        .segments
        .iter()
        .filter(|seg| is_damaged_segment(seg))
        .count();
    let mut damaged: Vec<String> = report
        .segments
        .iter()
        .filter(|seg| is_damaged_segment(seg))
        .take(MAX_LIST)
        .map(|seg| match seg.kind {
            SegmentKind::Chunk { chunk_type, .. } => format!(
                "CRC mismatch in {} at offset {}",
                String::from_utf8_lossy(&chunk_type),
                seg.range.start
            ),
            SegmentKind::Truncated => format!(
                "truncated from offset {} ({} bytes)",
                seg.range.start,
                seg.range.len()
            ),
            _ => format!(
                "{} trailing bytes after IEND at offset {}",
                seg.range.len(),
                seg.range.start
            ),
        })
        .collect();
    // A skip the file itself caused is damage. An over-budget skip is not — nothing is known to be
    // wrong with the file — but it is still a reason this command cannot vouch for it, which is a
    // separate question the verdict below keeps separate.
    if let FilterScan::Skipped(reason) = report.filters
        && reason.is_damage()
    {
        findings += 1;
        if damaged.len() < MAX_LIST {
            damaged.push(format!(
                "filters not counted — {}",
                filter_skip_label(reason)
            ));
        }
    }
    print_lines_of("findings", &damaged, findings);

    println!("  classified:    {}", yes_no(report.is_fully_classified()));
    println!("  intact:        {}", yes_no(report.is_intact()));
    println!("  verified:      {}", yes_no(report.is_verified()));

    // The gate is `is_verified`, not `is_intact`. `is_intact` is "nothing is known against this
    // file", which a file whose IDAT was never inflated satisfies without anything having been
    // read — and a corrupt zlib payload under a valid CRC is exactly the damage only the scan
    // sees. An archival gate that passed such a file would be reporting the reader's budget as a
    // property of the file.
    if report.is_verified() {
        Ok(())
    } else if report.is_intact() {
        Err(CliError::NotFullyAccounted(format!(
            "{}: not verified — {}",
            path.display(),
            report
                .filters
                .skipped()
                .map_or("the filter scan did not run", filter_skip_label)
        )))
    } else {
        Err(CliError::NotFullyAccounted(format!(
            "{}: not a complete, undamaged PNG datastream — {findings} finding(s)",
            path.display(),
        )))
    }
}

/// Renders why a PNG's scanline filters were not counted.
fn filter_skip_label(reason: gamut::png::SkippedFilterScan) -> &'static str {
    use gamut::png::SkippedFilterScan as Reason;
    match reason {
        Reason::OverBudget => "the image is larger than the reader's byte budget",
        Reason::ImplausibleInflation => {
            "the IDAT stream is too short to plausibly inflate to the image the header declares"
        }
        Reason::CorruptStream => "the IDAT stream is corrupt or truncated",
        Reason::LengthMismatch => "the IDAT stream inflated to the wrong length",
        Reason::UndefinedFilterCode => "a scanline carries an undefined filter code",
        // `SkippedFilterScan` is non-exhaustive; describe future reasons generically. They are
        // damage by default, so the finding is still raised.
        _ => "the scan could not be trusted",
    }
}

/// The display name of a format.
fn format_name(format: Format) -> &'static str {
    match format {
        Format::Tiff => "TIFF",
        Format::Dng => "DNG",
        Format::Png => "PNG",
    }
}

/// `yes`/`no` for a boolean verdict.
fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

#[cfg(test)]
mod tests {
    use super::{MAX_LIST, hidden_entries};

    #[test]
    fn the_truncation_notice_counts_the_entries_neither_caller_printed() {
        // `print_lines` hands over the whole list, so only the `MAX_LIST` cut hides anything: a
        // notice conditioned on `total > lines.len()` can never fire for it, and fifty unknown
        // TIFF tags printed twenty lines and nothing else.
        assert_eq!(hidden_entries(50, 50), 50 - MAX_LIST);
        // The PNG caller hands over a list already cut to `MAX_LIST` with the true total beside
        // it; the entries it never built are the hidden ones.
        assert_eq!(hidden_entries(50, MAX_LIST), 50 - MAX_LIST);
        // A list that fits hides nothing, from either caller.
        assert_eq!(hidden_entries(MAX_LIST, MAX_LIST), 0);
        assert_eq!(hidden_entries(3, 3), 0);
    }
}
