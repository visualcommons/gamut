//! `gamut inspect` — strict "deconstruct" of a TIFF, DNG or PNG (issues #197/#263/#224), and the
//! C2PA provenance report for a HEIC (issue #448), an AVIF, a WebP, a TIFF and a DNG (issue #530).
//!
//! Walks the entire container, classifies every byte into typed segments, and flags anything
//! unrecognised (unknown tags, unknown field types, out-of-spec codes, unclassified bytes).
//! Prints a report to stdout and exits non-zero when the file is not fully accounted for —
//! usable as an archival CI gate.
//!
//! # The C2PA section, and the formats with no gate
//!
//! Every format whose crate can locate a C2PA manifest store answers one more question: **does
//! this file carry a C2PA manifest store, and where?** For TIFF and DNG the section prints beside
//! the byte accounting and is additive — it never moves the verdict. HEIC, AVIF and WebP get the
//! section and nothing else: they are not deconstructed, so they have nothing to hold against the
//! file and report no verdict; each exits `0` whenever its crate could parse the container,
//! whether a store was found or not. Presence and absence are both ordinary outcomes of an ordinary
//! file, and neither is an accounting anomaly. (PNG's locator is not wired in yet — issue #727;
//! see `docs/inspect-exit-codes.md`.)
//!
//! The reporting itself lives in the format crates, not here, because that is where the facts and
//! the words about them belong: each crate's `C2paSummary` — [`gamut::heic::C2paSummary`],
//! [`gamut::avif::C2paSummary`], [`gamut::webp::C2paSummary`], and the TIFF/DNG one
//! [`gamut::tiff::C2paSummary`] shares from `gamut-ifd` — holds the reportable facts and renders the
//! lines, disclaimer included, since a report that let a reader infer a validity verdict would be a
//! defect. This command prefixes its own indent to them and caps the detail list. One rendering per
//! crate, pinned there. gamut locates a store and never validates one (C2PA 2.4 §15.12).
//!
//! `gamut-cli` being outside the coverage gate is **not** a reason to move logic out of it, and
//! nothing here rests on that: coverage exclusion is not test exclusion. This file's own logic is
//! tested inline below and, through the built binary, by `tests/inspect_c2pa.rs`.
//!
//! # Which files the HEIC and AVIF arms accept
//!
//! The sniff routes an ISOBMFF file — anything whose first box is `ftyp` — to `inspect_isobmff`,
//! which *confirms* it with `gamut-heic`'s own
//! [`HeifImage::is_hevc_still`](gamut::heic::HeifImage::is_hevc_still) and then `gamut-avif`'s
//! [`AvifImage::is_av1_still`](gamut::avif::AvifImage::is_av1_still) after the parses it performs
//! anyway, reporting an unsupported container brand when neither confirms it. The predicates are
//! not restated here: a brand list in this file would have to be exhaustive over the still-image
//! brands — `mif2`, `avci` and `avcs` beside `heic`/`heix`/`heim`/`heis`/`mif1`, and `avif`/`avio`
//! — and would still be wrong about `mif1`, which is the generic MIAF structural brand either format
//! may carry as its *major* brand. `references/heif` §7 fixes the rule and the format crates
//! implement it: for `mif1` the primary item must additionally carry an `hvcC` (HEIC) or an `av1C`
//! (AVIF). Confirming after the parse cannot drift from the crates that own the rule.
//!
//! `--format heic` and `--format avif` skip the sniff **and** the confirmation. A forced format is
//! the caller's own assertion about the file, and honouring it is what `--format` is for — so the
//! label the report opens with says the format was *asserted*, not determined. `<path>: HEIF/HEIC`
//! is this command's own claim about the file, and printing it over a file the sniff would have
//! declined would echo the caller's assertion back as a finding. A WebP needs no such label: the
//! RIFF walk itself refuses a file without the `RIFF` magic and the `WEBP` form.
//!
//! # `--output json`
//!
//! The same facts as one JSON document (issue #596), for a caller that would otherwise match on
//! the text report's wording. Each arm builds its facts once and renders them either way — a TIFF
//! or DNG [`Summary`] entry carries its text line and its JSON object side by side, and a PNG
//! finding is rendered from one [`PngFinding`] — so the two outputs cannot list different things.
//! Nothing is capped in JSON: the twenty-entry cut is a terminal-presentation decision. Each `c2pa`
//! object carries its crate's `C2PA_NOT_VALIDATED` and never a store's bytes, and the exit code is
//! the text report's. The writer is [`Json`], hand-written because the command only ever emits
//! JSON and the workspace keeps `serde_json` dev-only.
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
    /// Input TIFF, DNG, PNG, HEIC, AVIF or WebP file.
    input: PathBuf,
    /// Force the container format instead of auto-detecting it.
    #[arg(long, value_enum)]
    format: Option<Format>,
    /// How to render the report on stdout. `json` is one JSON document holding the same facts as
    /// the text report, with no list truncated; the exit code is the same either way.
    #[arg(long, value_enum, default_value_t = Output::Text)]
    output: Output,
}

/// How `gamut inspect` renders its report.
///
/// A rendering, never a verdict: the exit code and the stderr message are the same for both (see
/// `docs/inspect-exit-codes.md`).
#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum Output {
    /// The human-readable report, with every input-sized list capped at [`MAX_LIST`] entries.
    Text,
    /// One JSON document (RFC 8259) on stdout: the same facts, every list uncapped.
    Json,
}

/// The container format to deconstruct as.
#[derive(Clone, Copy, ValueEnum)]
pub(crate) enum Format {
    /// TIFF 6.0 / BigTIFF (gamut-tiff), with its C2PA provenance report.
    Tiff,
    /// DNG (Adobe Digital Negative; gamut-dng), with its C2PA provenance report.
    Dng,
    /// PNG (gamut-png).
    Png,
    /// HEIF/HEIC (gamut-heic) — the C2PA provenance report only; see the module docs.
    Heic,
    /// AVIF (gamut-avif) — the C2PA provenance report only; see the module docs.
    Avif,
    /// WebP (gamut-webp) — the C2PA provenance report only; see the module docs.
    Webp,
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

/// One entry of a [`Summary`] list: its text-report line and its JSON object.
struct Listed {
    line: String,
    json: Json,
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
    // rather than being flattened into `Summary`. HEIC's, AVIF's and WebP's are not a deconstruct
    // at all: they answer the provenance question and gate nothing (see the module docs).
    let route = match args.format {
        Some(_) => Route::Forced,
        None => Route::Sniffed,
    };
    let to = Target {
        path: &args.input,
        route,
        output: args.output,
    };
    match format {
        Format::Png => return inspect_png(to, &data),
        // An `ftyp` box sniffs as HEIC, which is a route to the ISOBMFF arms and not yet a verdict:
        // `inspect_isobmff` confirms which still image the file is (see the module docs).
        Format::Heic if matches!(route, Route::Sniffed) => {
            return inspect_isobmff(to, &data);
        }
        Format::Heic => {
            let container = gamut::heic::HeifContainer::parse(&data)?;
            return report_heic(to, &container);
        }
        Format::Avif => {
            let container = gamut::avif::AvifContainer::parse(&data)?;
            return report_avif(to, &container);
        }
        Format::Webp => return inspect_webp(to, &data),
        Format::Tiff | Format::Dng => {}
    }

    // The C2PA report is read before anything is printed, so a file whose summary cannot be built
    // prints no half report. It cannot fail once the deconstruct has succeeded: both walk the same
    // main IFD chain under the same guards, and the summary lists an unreadable store rather than
    // erroring on it.
    let (summary, c2pa) = match format {
        Format::Dng => (
            summarize_dng(gamut::dng::deconstruct(&data)?),
            gamut::dng::c2pa_summary(&data[..])?,
        ),
        Format::Tiff => (
            summarize_tiff(gamut::tiff::deconstruct(&data)?),
            gamut::tiff::c2pa_summary(&data)?,
        ),
        Format::Png | Format::Heic | Format::Avif | Format::Webp => {
            unreachable!("handled above")
        }
    };

    match args.output {
        Output::Text => {
            print_summary(&args.input, format, &summary);
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
            doc.extend(summary_json(&summary));
            doc.push(("c2pa", ifd_c2pa_json(&c2pa)));
            print_json(&Json::Object(doc));
        }
    }

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

/// Detects PNG by signature, then WebP by its RIFF header, then an ISOBMFF file by its `ftyp` box,
/// then DNG vs TIFF: a DNG is a TIFF whose IFD 0 carries the mandatory `DNGVersion` tag.
///
/// The `ftyp` test is a **route**, not a verdict: it says the file is ISOBMFF, not which still
/// image it is. It returns [`Format::Heic`], and `inspect_isobmff` confirms the container with
/// `gamut-heic`'s and then `gamut-avif`'s own predicates after parsing (see the module docs for why
/// the brand rule is not restated here), so an ISOBMFF file that is neither is reported as an
/// unsupported container rather than blamed on TIFF for having no byte-order mark.
fn sniff(data: &[u8]) -> Format {
    if data.starts_with(&PNG_SIGNATURE) {
        return Format::Png;
    }
    // RFC 9649 §2.4: `RIFF`, a 4-byte file size, then the `WEBP` form type.
    if data.starts_with(b"RIFF") && data.get(8..12) == Some(b"WEBP") {
        return Format::Webp;
    }
    // §4.3: `ftyp` is the first box of the file, so its type sits at offset 4.
    if data.get(4..8) == Some(b"ftyp") {
        return Format::Heic;
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
            } => anomaly_structure(*page, detail, severity_level_tiff(*severity)),
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
                severity_level_tiff(*severity),
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

/// Collapses a DNG report into the printable [`Summary`].
fn summarize_dng(report: gamut::dng::DeconstructReport) -> Summary {
    use gamut::dng::Anomaly;
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
            } => anomaly_structure(*page, detail, severity_level_dng(*severity)),
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
                severity_level_dng(*severity),
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

/// Classifies a TIFF anomaly severity.
fn severity_level_tiff(severity: gamut::tiff::Severity) -> Level {
    match severity {
        gamut::tiff::Severity::Warning => Level::Warning,
        gamut::tiff::Severity::Error => Level::Error,
        // `Severity` is non-exhaustive.
        _ => Level::Other,
    }
}

/// Classifies a DNG anomaly severity.
fn severity_level_dng(severity: gamut::dng::Severity) -> Level {
    match severity {
        gamut::dng::Severity::Warning => Level::Warning,
        gamut::dng::Severity::Error => Level::Error,
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

/// The JSON members of a TIFF/DNG report: the facts [`print_summary`] prints, every list uncapped.
fn summary_json(s: &Summary) -> Vec<(&'static str, Json)> {
    let seg = &s.segments;
    let ranges = |ranges: &mut dyn Iterator<Item = gamut::ifd::Range>| {
        Json::Array(ranges.map(|r| span(r.start, r.end())).collect())
    };
    let accounting = Json::Object(vec![
        ("file_len", Json::from(seg.file_len)),
        (
            "classified_bytes",
            Json::from(seg.file_len - seg.unclassified_bytes()),
        ),
        ("unclassified_bytes", Json::from(seg.unclassified_bytes())),
        ("segment_count", Json::from(seg.segments.len())),
        (
            "unclassified",
            ranges(&mut seg.unclassified.iter().copied()),
        ),
        (
            "conflicts",
            Json::Array(
                seg.conflicts
                    .iter()
                    .map(|c| {
                        Json::Object(vec![
                            ("a", span(c.a.range.start, c.a.range.end())),
                            ("b", span(c.b.range.start, c.b.range.end())),
                        ])
                    })
                    .collect(),
            ),
        ),
        ("shared_values", Json::from(seg.shared.len())),
        (
            "out_of_bounds",
            ranges(&mut seg.out_of_bounds.iter().map(|o| o.range)),
        ),
        (
            "unclaimed_reads",
            ranges(&mut seg.unclaimed_reads.iter().copied()),
        ),
        (
            "unread_claims",
            ranges(&mut seg.unread_claims.iter().map(|c| c.range)),
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
/// Counted from what is actually **printed** — at most [`MAX_LIST`] of them — because a caller
/// may hand over either the whole list, so that the `MAX_LIST` cut in the loop is the only thing
/// that hides anything, or a list already cut to `MAX_LIST` beside the true total, so that what it
/// hides are the lines it never built. A notice derived from `total > lines.len()` alone sees only
/// the second and is dead for the first — which is how a TIFF with fifty unknown tags came to
/// print twenty of them and no indication that thirty were missing.
fn hidden_entries(total: usize, lines: usize) -> usize {
    total.saturating_sub(lines.min(MAX_LIST))
}

/// Deconstructs a PNG and prints where its bytes went, exiting non-zero when the file is not a
/// complete, undamaged datastream.
fn inspect_png(to: Target<'_>, data: &[u8]) -> Result<(), CliError> {
    // Inspection budgets differently from decoding. `gamut::png::deconstruct`'s default matches
    // the *decoder*'s, which guards a decode against hostile input; but a file this command
    // declines to inflate is a file it cannot verify, and at the decoder's 64 MiB that is every
    // PNG past 4096x4096 RGBA8 -- an ordinary photograph. Reading it is the whole job, so the
    // ceiling is raised to a gigabyte: past any real image, short of unbounded.
    let limits = gamut::png::DeconstructLimits::default().with_max_image_bytes(1 << 30);
    let report = gamut::png::deconstruct_with_limits(data, limits)?;
    let path = to.path;

    match to.output {
        Output::Text => print_png(path, &report),
        Output::Json => {
            let mut doc = document_head(to, Format::Png);
            doc.extend(png_json(&report));
            print_json(&Json::Object(doc));
        }
    }

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
            "{}: not a complete, undamaged PNG datastream — {} finding(s)",
            path.display(),
            png_findings(&report).count(),
        )))
    }
}

/// One thing a PNG walk holds against the file — the entries of the report's `findings` list.
///
/// Kept as a reference into the report rather than as rendered text, because the chunk count is
/// chosen by the input: the text report renders only the [`MAX_LIST`] entries it prints, while the
/// count and the JSON document see every one.
#[derive(Clone, Copy)]
enum PngFinding<'a> {
    /// A segment that is damage in itself: a chunk whose CRC does not match, a truncated tail, or
    /// trailing bytes after IEND.
    Segment(&'a gamut::png::Segment),
    /// The datastream has no IEND chunk.
    NoIend,
    /// The filter scan declined for a reason that is damage.
    Scan(gamut::png::SkippedFilterScan),
}

/// Every finding against a PNG, in report order: the damaged segments in file order, then a
/// missing IEND, then a filter-scan skip that is damage.
///
/// `is_intact` also requires IEND, and a datastream can lack it with every segment sound: one that
/// ends cleanly at a chunk boundary. It is listed whenever IEND is absent — a truncated file lacks
/// it too, and says so twice rather than having one conjunct of the verdict go unlisted — so a file
/// that is not intact always names at least one finding. A skip the file itself caused is damage;
/// an over-budget skip is not — nothing is known to be wrong with the file — but it is still a
/// reason this command cannot vouch for it, which is a separate question the verdict keeps
/// separate.
fn png_findings(report: &gamut::png::PngReport) -> impl Iterator<Item = PngFinding<'_>> {
    use gamut::png::SegmentKind;
    let damaged = report
        .segments
        .iter()
        .filter(|seg| {
            matches!(
                seg.kind,
                SegmentKind::Chunk { crc_ok: false, .. }
                    | SegmentKind::Truncated
                    | SegmentKind::Trailer
            )
        })
        .map(PngFinding::Segment);
    let no_iend = report
        .chunk(b"IEND")
        .is_none()
        .then_some(PngFinding::NoIend);
    let scan = report
        .filters
        .skipped()
        .filter(|reason| reason.is_damage())
        .map(PngFinding::Scan);
    damaged.chain(no_iend).chain(scan)
}

/// Renders one [`PngFinding`] as its text line and its JSON object.
fn png_finding(finding: PngFinding<'_>) -> Listed {
    use gamut::png::SegmentKind;
    let (kind, line, range) = match finding {
        PngFinding::Segment(seg) => {
            let range = Some(span(seg.range.start, seg.range.end));
            match seg.kind {
                SegmentKind::Chunk { chunk_type, .. } => (
                    "crc_mismatch",
                    format!(
                        "CRC mismatch in {} at offset {}",
                        String::from_utf8_lossy(&chunk_type),
                        seg.range.start
                    ),
                    range,
                ),
                SegmentKind::Truncated => (
                    "truncated",
                    format!(
                        "truncated from offset {} ({} bytes)",
                        seg.range.start,
                        seg.range.len()
                    ),
                    range,
                ),
                _ => (
                    "trailing_bytes",
                    format!(
                        "{} trailing bytes after IEND at offset {}",
                        seg.range.len(),
                        seg.range.start
                    ),
                    range,
                ),
            }
        }
        PngFinding::NoIend => (
            "missing_iend",
            "no IEND chunk: the datastream ends without its terminator".to_owned(),
            None,
        ),
        PngFinding::Scan(reason) => (
            "filter_scan_damage",
            format!("filters not counted — {}", filter_skip_label(reason)),
            None,
        ),
    };
    let mut json = vec![("kind", Json::from(kind))];
    if let Some(range) = range {
        json.push(("range", range));
    }
    json.push(("description", Json::from(line.as_str())));
    Listed {
        line,
        json: Json::Object(json),
    }
}

/// The JSON members of a PNG report: the facts [`print_png`] prints, every list uncapped.
fn png_json(report: &gamut::png::PngReport) -> Vec<(&'static str, Json)> {
    use gamut::png::FilterScan;
    let header = report.header;
    let image = Json::Object(vec![
        ("width", Json::from(header.width)),
        ("height", Json::from(header.height)),
        ("color_type", Json::from(header.color_type.code())),
        ("bit_depth", Json::from(header.bit_depth)),
        ("interlaced", Json::Bool(header.interlaced)),
    ]);
    let chunks = report
        .chunks
        .iter()
        .map(|stats| {
            Json::Object(vec![
                (
                    "type",
                    Json::from(String::from_utf8_lossy(&stats.chunk_type).as_ref()),
                ),
                ("count", Json::from(stats.count)),
                ("payload_bytes", Json::from(stats.payload_bytes)),
                ("framing_bytes", Json::from(stats.framing_bytes())),
                ("ancillary", Json::Bool(stats.is_ancillary())),
            ])
        })
        .collect();
    let filters = match report.filters {
        FilterScan::Counted(h) => {
            use gamut::png::FilterType;
            Json::Object(vec![
                ("counted", Json::Bool(true)),
                ("none", Json::from(h.count(FilterType::None))),
                ("sub", Json::from(h.count(FilterType::Sub))),
                ("up", Json::from(h.count(FilterType::Up))),
                ("average", Json::from(h.count(FilterType::Average))),
                ("paeth", Json::from(h.count(FilterType::Paeth))),
                ("scanlines", Json::from(h.total())),
            ])
        }
        FilterScan::Skipped(reason) => Json::Object(vec![
            ("counted", Json::Bool(false)),
            ("reason", Json::from(filter_skip_key(reason))),
            ("damage", Json::Bool(reason.is_damage())),
            ("description", Json::from(filter_skip_label(reason))),
        ]),
    };
    let passes = report
        .passes
        .iter()
        .map(|pass| {
            Json::Object(vec![
                ("index", Json::from(pass.index)),
                ("width", Json::from(pass.width)),
                ("height", Json::from(pass.height)),
                ("row_bytes", Json::from(pass.row_bytes)),
                ("filtered_len", Json::from(pass.filtered_len)),
            ])
        })
        .collect();
    vec![
        ("image", image),
        ("file_len", Json::from(report.file_len)),
        ("bits_per_pixel", Json::Float(report.bits_per_pixel())),
        (
            "idat",
            Json::Object(vec![
                ("compressed", Json::from(report.idat_compressed)),
                ("filtered", Json::from(report.filtered_len)),
                ("ratio", Json::Float(report.idat_ratio())),
            ]),
        ),
        ("overhead_bytes", Json::from(report.overhead_bytes())),
        ("framing_bytes", Json::from(report.framing_bytes())),
        ("chunks", Json::Array(chunks)),
        ("filters", filters),
        ("passes", Json::Array(passes)),
        (
            "findings",
            Json::Array(png_findings(report).map(|f| png_finding(f).json).collect()),
        ),
        ("fully_classified", Json::Bool(report.is_fully_classified())),
        ("intact", Json::Bool(report.is_intact())),
        ("verified", Json::Bool(report.is_verified())),
    ]
}

/// Prints a PNG's text report: where its bytes went, and what is held against it.
fn print_png(path: &std::path::Path, report: &gamut::png::PngReport) {
    use gamut::png::{FilterScan, FilterType};
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
    let shown: Vec<String> = png_findings(report)
        .take(MAX_LIST)
        .map(|f| png_finding(f).line)
        .collect();
    print_lines_of("findings", &shown, png_findings(report).count());

    println!("  classified:    {}", yes_no(report.is_fully_classified()));
    println!("  intact:        {}", yes_no(report.is_intact()));
    println!("  verified:      {}", yes_no(report.is_verified()));
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

/// The JSON key for why a PNG's scanline filters were not counted: [`filter_skip_label`]'s
/// reasons, as identifiers a consumer can match on.
fn filter_skip_key(reason: gamut::png::SkippedFilterScan) -> &'static str {
    use gamut::png::SkippedFilterScan as Reason;
    match reason {
        Reason::OverBudget => "over_budget",
        Reason::ImplausibleInflation => "implausible_inflation",
        Reason::CorruptStream => "corrupt_stream",
        Reason::LengthMismatch => "length_mismatch",
        Reason::UndefinedFilterCode => "undefined_filter_code",
        // Non-exhaustive, as above; the object's `damage` member still says how it counts.
        _ => "other",
    }
}

/// How a provenance-only arm (HEIC, AVIF) was reached, which decides whether the container was
/// confirmed and what the report's label may claim.
#[derive(Clone, Copy)]
enum Route {
    /// By sniffing the file. The sniff only knows the file is ISOBMFF, so `inspect_isobmff`
    /// confirms the container against the format crates' own still-image predicates before
    /// anything is reported.
    Sniffed,
    /// By an explicit `--format`. A forced format is the caller's own assertion about the file;
    /// `--format` exists to override detection, so it overrides the confirmation too — and the
    /// label says so, rather than reporting the caller's assertion back as this command's.
    Forced,
}

/// What a report is about and how it is rendered: the file, how its format was reached, and the
/// output the caller asked for.
#[derive(Clone, Copy)]
struct Target<'a> {
    path: &'a std::path::Path,
    route: Route,
    output: Output,
}

/// The label a provenance report opens with: the format's name, and — on the forced route — that
/// the format was asserted rather than determined.
///
/// On the sniffed route the command determined it, and confirmed it against the format crate's own
/// still-image predicate. On the forced route it determined nothing, so a bare name would be this
/// command vouching for a file it never tested — over an AVIF, say, which `--format heic` accepts.
fn route_label(name: &str, route: Route) -> String {
    match route {
        Route::Sniffed => name.to_owned(),
        Route::Forced => format!("{name} (asserted by --format, not detected)"),
    }
}

/// Reports a sniffed ISOBMFF file's provenance, once its container is confirmed: a HEVC still image
/// goes to the HEIC report, an AV1 still image to the AVIF one, and anything else is an unsupported
/// container.
///
/// The confirmations are the format crates' own predicates — `HeifImage::is_hevc_still` and
/// `AvifImage::is_av1_still`, each including the `mif1` case the major brand alone cannot settle —
/// so no brand list lives here. HEIC is tried first, which is the order this command always had;
/// an AVIF is parsed only once the HEIC confirmation declines. A file neither crate confirms keeps
/// the HEIC arm's outcome: its parse error (a top-level `moov`, say), or the unsupported-brand
/// message when it parsed.
fn inspect_isobmff(to: Target<'_>, data: &[u8]) -> Result<(), CliError> {
    let heic = gamut::heic::HeifContainer::parse(data);
    if let Ok(container) = &heic
        && container.image().is_hevc_still()
    {
        return report_heic(to, container);
    }
    if let Ok(container) = gamut::avif::AvifContainer::parse(data)
        && container.image().is_av1_still()
    {
        return report_avif(to, &container);
    }
    Err(CliError::UnsupportedContainer {
        path: to.path.to_path_buf(),
        brand: brand_label(heic?.image().major_brand()),
    })
}

/// Reports what a HEIF/HEIC file says about its own provenance, and nothing else.
///
/// Every reportable fact and every word of the report come from `gamut-heic`; this prints the lines
/// under the file's name and returns `Ok(())`. It reaches no verdict, so it has none to fail on: a
/// file with a manifest store, a file without one, and a file whose C2PA box could not be read
/// through all exit `0`, each saying which it is. The exit code says whether the *inspection*
/// succeeded, not what it found.
fn report_heic(to: Target<'_>, container: &gamut::heic::HeifContainer<'_>) -> Result<(), CliError> {
    let summary = container.c2pa_summary();
    match to.output {
        Output::Text => {
            println!(
                "{}: {}",
                to.path.display(),
                route_label("HEIF/HEIC", to.route)
            );
            print_c2pa(
                summary.summary_lines(),
                summary.detail_lines(),
                summary.detail_line_count(),
            );
        }
        Output::Json => print_provenance_json(to, Format::Heic, heic_c2pa_json(&summary)),
    }
    Ok(())
}

/// Reports what an AVIF file says about its own provenance, and nothing else — the HEIC report's
/// contract, over `gamut-avif`'s summary. Exit `0` whatever it finds.
fn report_avif(to: Target<'_>, container: &gamut::avif::AvifContainer<'_>) -> Result<(), CliError> {
    let summary = container.c2pa_summary();
    match to.output {
        Output::Text => {
            println!("{}: {}", to.path.display(), route_label("AVIF", to.route));
            print_c2pa(
                summary.summary_lines(),
                summary.detail_lines(),
                summary.detail_line_count(),
            );
        }
        Output::Json => print_provenance_json(to, Format::Avif, avif_c2pa_json(&summary)),
    }
    Ok(())
}

/// Reports what a WebP file says about its own provenance, and nothing else — the HEIC report's
/// contract, over `gamut-webp`'s summary. Exit `0` whatever it finds; a file whose RIFF chunk walk
/// fails exits non-zero with that error, like any container this command cannot open.
///
/// There is no forced-route label: the RIFF walk itself requires the `RIFF` magic and the `WEBP`
/// form, so `--format webp` cannot make this arm report on a file that is not a WebP.
fn inspect_webp(to: Target<'_>, data: &[u8]) -> Result<(), CliError> {
    let summary = gamut::webp::c2pa_summary(data)?;
    match to.output {
        Output::Text => {
            println!("{}: WebP", to.path.display());
            print_c2pa(
                summary.summary_lines(),
                summary.detail_lines(),
                summary.detail_line_count(),
            );
        }
        Output::Json => print_provenance_json(to, Format::Webp, webp_c2pa_json(&summary)),
    }
    Ok(())
}

/// Prints a format crate's C2PA report: its head uncapped, then its detail lines capped at
/// [`MAX_LIST`] with the "… and N more" tail every other list here carries, `total` being the true
/// number of detail lines.
///
/// Capped for the same reason as every other list: C2PA 2.4 permits any number of top-level C2PA
/// boxes, so their count is chosen by the input, and uncapped a legal file puts the headline —
/// disclaimer and all — at line 2 of fifty thousand. The head is never capped: each format crate
/// bounds it to a few lines whatever the file holds, and its first line is the one a reader must not
/// lose. What keeps the cap honest is in the format crates: the headline names every non-empty
/// category, and the detail lines arrive in file order, so the cut hides the file's last entries
/// rather than one kind of entry. Only the lines that will be shown are ever built.
fn print_c2pa(head: Vec<String>, details: impl Iterator<Item = String>, total: usize) {
    for line in head {
        println!("  {line}");
    }
    let shown: Vec<String> = details.take(MAX_LIST).collect();
    for line in &shown {
        println!("  {line}");
    }
    let hidden = hidden_entries(total, shown.len());
    if hidden > 0 {
        println!("    … and {hidden} more");
    }
}

/// The members every JSON document opens with: the file, the format it was read as, and whether
/// that format was detected or asserted by `--format` (the JSON form of [`route_label`], stated for
/// every format because `--format` skips the sniff for every format).
fn document_head(to: Target<'_>, format: Format) -> Vec<(&'static str, Json)> {
    vec![
        ("path", Json::from(to.path.display().to_string())),
        ("format", Json::from(format_key(format))),
        (
            "format_source",
            Json::from(match to.route {
                Route::Sniffed => "detected",
                Route::Forced => "asserted",
            }),
        ),
    ]
}

/// Prints the JSON document of a provenance-only arm (HEIC, AVIF, WebP): the head and the C2PA
/// object, and nothing else, because those arms report nothing else.
fn print_provenance_json(to: Target<'_>, format: Format, c2pa: Json) {
    let mut doc = document_head(to, format);
    doc.push(("c2pa", c2pa));
    print_json(&Json::Object(doc));
}

/// A HEIC `C2paSummary` as data — every list uncapped, the store bytes never included.
///
/// `not_validated` carries `gamut-heic`'s own disclaimer for the same reason the text report's
/// headline does: "a store is present" reads as *verified* to a program's author as much as to a
/// terminal's reader.
fn heic_c2pa_json(summary: &gamut::heic::C2paSummary) -> Json {
    use gamut::heic::{C2paBoxPosition as Position, C2paUnreadReason as Reason};
    let position = |p: Position| match p {
        Position::BeforeMediaData => "before_media_data",
        Position::AfterMediaData => "after_media_data",
        Position::AfterMovieBox => "after_movie_box",
        _ => "other",
    };
    let reason = |r: Reason| match r {
        Reason::Truncated => "truncated",
        Reason::NotVersionZero => "not_version_zero",
        Reason::UnknownPurpose => "unknown_purpose",
        Reason::NoStoreBound => "no_store_bound",
        _ => "other",
    };
    Json::Object(vec![
        ("not_validated", Json::from(gamut::heic::C2PA_NOT_VALIDATED)),
        (
            "stores",
            Json::Array(
                summary
                    .stores
                    .iter()
                    .map(|s| {
                        Json::Object(vec![
                            ("purpose", Json::from(s.purpose.as_str())),
                            ("extent", Json::from("store")),
                            ("size", Json::from(s.size())),
                            ("range", span(s.range.start, s.range.end)),
                            ("slot", span(s.slot.start, s.slot.end)),
                            ("position", Json::from(position(s.position))),
                        ])
                    })
                    .collect(),
            ),
        ),
        (
            "unread",
            Json::Array(
                summary
                    .unread
                    .iter()
                    .map(|u| {
                        Json::Object(vec![
                            ("range", span(u.range.start, u.range.end)),
                            ("reason", Json::from(reason(u.reason))),
                            ("description", Json::from(u.reason.describe())),
                            ("position", Json::from(position(u.position))),
                        ])
                    })
                    .collect(),
            ),
        ),
        (
            "merkle",
            Json::Array(
                summary
                    .merkle
                    .iter()
                    .map(|m| {
                        Json::Object(vec![
                            ("range", span(m.range.start, m.range.end)),
                            ("position", Json::from(position(m.position))),
                        ])
                    })
                    .collect(),
            ),
        ),
        ("other_uuid_boxes", Json::from(summary.other_uuid_boxes)),
    ])
}

/// An AVIF `C2paSummary` as data — [`heic_c2pa_json`]'s shape over `gamut-avif`'s summary.
///
/// Both crates' summaries name the same two regions (issue #505): `range` (and `size`) the store
/// trimmed to its JUMBF `LBox`, so `extent` is `"store"` for both, and `slot` the box-bounded slot
/// after the merkle offset — the store *plus any padding*.
fn avif_c2pa_json(summary: &gamut::avif::C2paSummary) -> Json {
    use gamut::avif::{C2paBoxPosition as Position, C2paUnreadReason as Reason};
    let position = |p: Position| match p {
        Position::BeforeMediaData => "before_media_data",
        Position::AfterMediaData => "after_media_data",
        _ => "other",
    };
    let reason = |r: Reason| match r {
        Reason::Truncated => "truncated",
        Reason::NotVersionZero => "not_version_zero",
        Reason::UnknownPurpose => "unknown_purpose",
        Reason::NoStoreBound => "no_store_bound",
        _ => "other",
    };
    Json::Object(vec![
        ("not_validated", Json::from(gamut::avif::C2PA_NOT_VALIDATED)),
        (
            "stores",
            Json::Array(
                summary
                    .stores
                    .iter()
                    .map(|s| {
                        Json::Object(vec![
                            ("purpose", Json::from(s.purpose.as_str())),
                            ("extent", Json::from("store")),
                            ("size", Json::from(s.size())),
                            ("range", span(s.range.start, s.range.end)),
                            ("slot", span(s.slot.start, s.slot.end)),
                            ("position", Json::from(position(s.position))),
                        ])
                    })
                    .collect(),
            ),
        ),
        (
            "unread",
            Json::Array(
                summary
                    .unread
                    .iter()
                    .map(|u| {
                        Json::Object(vec![
                            ("range", span(u.range.start, u.range.end)),
                            ("reason", Json::from(reason(u.reason))),
                            ("description", Json::from(u.reason.describe())),
                            ("position", Json::from(position(u.position))),
                        ])
                    })
                    .collect(),
            ),
        ),
        (
            "merkle",
            Json::Array(
                summary
                    .merkle
                    .iter()
                    .map(|m| {
                        Json::Object(vec![
                            ("range", span(m.range.start, m.range.end)),
                            ("position", Json::from(position(m.position))),
                        ])
                    })
                    .collect(),
            ),
        ),
        ("other_uuid_boxes", Json::from(summary.other_uuid_boxes)),
    ])
}

/// A WebP `C2paSummary` as data. A `C2PA` chunk has one unread case — a payload shorter than the
/// JUMBF box header — so an unread entry carries its range and position and no reason.
fn webp_c2pa_json(summary: &gamut::webp::C2paSummary) -> Json {
    use gamut::webp::C2paChunkPosition as Position;
    let position = |p: Position| match p {
        Position::Last => "last",
        Position::BeforeLaterChunks => "before_later_chunks",
        _ => "other",
    };
    Json::Object(vec![
        ("not_validated", Json::from(gamut::webp::C2PA_NOT_VALIDATED)),
        (
            "stores",
            Json::Array(
                summary
                    .stores
                    .iter()
                    .map(|s| {
                        Json::Object(vec![
                            ("size", Json::from(s.size())),
                            ("range", span(s.range.start, s.range.end)),
                            ("position", Json::from(position(s.position))),
                        ])
                    })
                    .collect(),
            ),
        ),
        (
            "unread",
            Json::Array(
                summary
                    .unread
                    .iter()
                    .map(|u| {
                        Json::Object(vec![
                            ("range", span(u.range.start, u.range.end)),
                            ("position", Json::from(position(u.position))),
                        ])
                    })
                    .collect(),
            ),
        ),
    ])
}

/// A TIFF/DNG `C2paSummary` (`gamut-ifd`'s, shared by both) as data: the one store §A.3.6 admits,
/// with its two §18.5.5 exclusion ranges, and every other tag-52545 entry as unread.
fn ifd_c2pa_json(summary: &gamut::tiff::C2paSummary) -> Json {
    use gamut::tiff::C2paUnreadReason as Reason;
    let reason = |r: Reason| match r {
        Reason::NotInLastIfd => "not_in_last_ifd",
        Reason::NotUndefined => "not_undefined",
        Reason::Duplicate => "duplicate",
        Reason::TooShort => "too_short",
        Reason::OutsideFile => "outside_file",
        Reason::OverlapsCountField => "overlaps_count_field",
        _ => "other",
    };
    let store = summary.store.map_or(Json::Null, |s| {
        let store = s.exclusions.store();
        let count = s.exclusions.count_field();
        Json::Object(vec![
            ("size", Json::from(s.size())),
            ("range", span(store.start, store.end())),
            ("count_field", span(count.start, count.end())),
            ("ifd", Json::from(s.ifd)),
        ])
    });
    Json::Object(vec![
        ("not_validated", Json::from(gamut::tiff::C2PA_NOT_VALIDATED)),
        ("store", store),
        (
            "unread",
            Json::Array(
                summary
                    .unread
                    .iter()
                    .map(|u| {
                        Json::Object(vec![
                            ("entry", span(u.entry.start, u.entry.end())),
                            ("ifd", Json::from(u.ifd)),
                            ("reason", Json::from(reason(u.reason))),
                            ("description", Json::from(u.reason.describe())),
                        ])
                    })
                    .collect(),
            ),
        ),
    ])
}

/// A half-open byte range `[start, end)` as a JSON object.
fn span(start: impl Into<Json>, end: impl Into<Json>) -> Json {
    Json::Object(vec![("start", start.into()), ("end", end.into())])
}

/// Writes `doc` to stdout as one line of JSON.
fn print_json(doc: &Json) {
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
enum Json {
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
    fn write(&self, out: &mut String) {
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

/// Renders a four-character brand for a message, escaping every byte that is not printable ASCII.
///
/// A brand is four printable characters by definition, but the four bytes come from the file, and
/// a report on hostile input must not push control characters at the terminal it prints to.
fn brand_label(brand: [u8; 4]) -> String {
    brand
        .iter()
        .map(|&b| {
            if b.is_ascii_graphic() || b == b' ' {
                char::from(b).to_string()
            } else {
                format!("\\x{b:02x}")
            }
        })
        .collect()
}

/// The display name of a format.
fn format_name(format: Format) -> &'static str {
    match format {
        Format::Tiff => "TIFF",
        Format::Dng => "DNG",
        Format::Png => "PNG",
        Format::Heic => "HEIC",
        Format::Avif => "AVIF",
        Format::Webp => "WebP",
    }
}

/// The JSON `format` member of a format: the value `--format` takes for it.
fn format_key(format: Format) -> &'static str {
    match format {
        Format::Tiff => "tiff",
        Format::Dng => "dng",
        Format::Png => "png",
        Format::Heic => "heic",
        Format::Avif => "avif",
        Format::Webp => "webp",
    }
}

/// `yes`/`no` for a boolean verdict.
fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

#[cfg(test)]
mod tests {
    use super::{Json, Level, MAX_LIST, anomaly_other, brand_label, hidden_entries};

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

    #[test]
    fn a_brand_label_escapes_every_byte_that_is_not_printable_ascii() {
        // The four bytes come from the file. A brand of control characters must reach the report as
        // text, not as an escape sequence the terminal acts on.
        assert_eq!(brand_label(*b"avif"), "avif");
        // Trailing spaces are legal in a four-character code and must survive unescaped.
        assert_eq!(brand_label(*b"qt  "), "qt  ");
        assert_eq!(
            brand_label([0x07, 0x1B, 0x00, 0xFF]),
            "\\x07\\x1b\\x00\\xff"
        );
    }

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

    #[test]
    fn an_unknown_severity_is_other_in_json_and_error_in_text() {
        // docs/inspect-exit-codes.md: every unknown enumerated value, `severity` included, is
        // `"other"` in the document; the text report keeps its conservative `[error]` label.
        assert_eq!(Level::Other.json(), "other");
        assert_eq!(Level::Other.text(), "error");
        let other = anomaly_other("Future".to_owned());
        assert_eq!(other.line, "[error] Future");
        assert_eq!(
            json_text(&other.json),
            "{\"kind\":\"other\",\"severity\":\"other\",\"detail\":\"Future\"}"
        );
        // The two known levels keep one word in both renderings.
        for (level, word) in [(Level::Warning, "warning"), (Level::Error, "error")] {
            assert_eq!(level.json(), word);
            assert_eq!(level.text(), word);
        }
    }
}
