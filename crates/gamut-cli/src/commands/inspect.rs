//! `gamut inspect` — strict "deconstruct" of a TIFF, DNG or PNG (issues #197/#263/#224), and the
//! C2PA provenance report for a HEIC (issue #448), an AVIF, a WebP, a TIFF and a DNG (issue #530),
//! and a PNG (issue #727).
//!
//! Walks the entire container, classifies every byte into typed segments, and flags anything
//! unrecognised (unknown tags, unknown field types, out-of-spec codes, unclassified bytes).
//! Prints a report to stdout and exits non-zero when the file is not fully accounted for —
//! usable as an archival CI gate.
//!
//! # The C2PA section, and the formats with no gate
//!
//! Every format whose crate can locate a C2PA manifest store answers one more question: **does
//! this file carry a C2PA manifest store, and where?** For TIFF, DNG and PNG the section prints
//! beside the byte accounting and is additive — it never moves the verdict. HEIC, AVIF and WebP get
//! the section and nothing else: they are not deconstructed, so they have nothing to hold against
//! the file and report no verdict; each exits `0` whenever its crate could parse the container,
//! whether a store was found or not. Presence and absence are both ordinary outcomes of an ordinary
//! file, and neither is an accounting anomaly.
//!
//! The reporting itself lives in the format crates, not here, because that is where the facts and
//! the words about them belong: each crate's `C2paSummary` — [`gamut::heic::C2paSummary`],
//! [`gamut::avif::C2paSummary`], [`gamut::webp::C2paSummary`], [`gamut::png::C2paSummary`], and
//! the TIFF/DNG one [`gamut::tiff::C2paSummary`] shares from `gamut-ifd` — holds the reportable
//! facts and renders the lines, disclaimer included, since a report that let a reader infer a
//! validity verdict would be a defect. This command prefixes its own indent to them and caps the
//! detail list. One rendering per crate, pinned there. gamut locates a store and never validates one (C2PA 2.4 §15.12).
//!
//! `gamut-cli` being outside the coverage gate is **not** a reason to move logic out of it, and
//! nothing here rests on that: coverage exclusion is not test exclusion. This file's own logic is
//! tested inline below and, through the built binary, by `tests/inspect_c2pa.rs`.
//!
//! # Which files the HEIC and AVIF arms accept
//!
//! The sniff routes an ISOBMFF file — anything whose first box is `ftyp` — to `inspect_isobmff`,
//! which *confirms* it with `gamut-heic`'s own
//! [`HeifContainer::is_hevc_still`](gamut::heic::HeifContainer::is_hevc_still) and then `gamut-avif`'s
//! [`AvifContainer::is_av1_still`](gamut::avif::AvifContainer::is_av1_still) after the parses it performs
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
//! or DNG `ifd::Summary` entry carries its text line and its JSON object side by side, and a PNG
//! finding is rendered from one `png::PngFinding` — so the two outputs cannot list different things.
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

mod c2pa;
mod ifd;
mod json;
mod png;

use std::path::PathBuf;

use clap::{Args, ValueEnum};

use self::c2pa::{inspect_isobmff, inspect_webp, report_avif, report_heic};
use self::ifd::{inspect_dng, inspect_tiff};
use self::json::Json;
use self::png::inspect_png;
use crate::error::CliError;
use crate::sniff::Container;

/// The most list entries to print per category before truncating.
const MAX_LIST: usize = 20;

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
    /// PNG (gamut-png), with its C2PA provenance report.
    Png,
    /// HEIF/HEIC (gamut-heic) — the C2PA provenance report only; see the module docs.
    Heic,
    /// AVIF (gamut-avif) — the C2PA provenance report only; see the module docs.
    Avif,
    /// WebP (gamut-webp) — the C2PA provenance report only; see the module docs.
    Webp,
}

/// One entry of a TIFF/DNG `ifd::Summary` list or of the PNG findings: its text-report
/// line and its JSON object.
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
    let format = args.format.unwrap_or_else(|| detect(&data));

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
        Format::Png => inspect_png(to, &data),
        // An `ftyp` box sniffs as HEIC, which is a route to the ISOBMFF arms and not yet a verdict:
        // `inspect_isobmff` confirms which still image the file is (see the module docs).
        Format::Heic if matches!(route, Route::Sniffed) => inspect_isobmff(to, &data),
        Format::Heic => report_heic(to, &gamut::heic::HeifContainer::parse(&data)?),
        Format::Avif => report_avif(to, &gamut::avif::AvifContainer::parse(&data)?),
        Format::Webp => inspect_webp(to, &data),
        Format::Tiff => inspect_tiff(to, &data),
        Format::Dng => inspect_dng(to, &data),
    }
}

/// The format [`crate::sniff`] routes `data` to: PNG and WebP by their signatures, an ISOBMFF file
/// by its `ftyp` box, and anything else — JPEG and JPEG XL included, which this command has no
/// report for — as DNG or TIFF: a DNG is a TIFF whose IFD 0 carries the mandatory `DNGVersion`
/// tag.
///
/// The `ftyp` test is a **route**, not a verdict: it says the file is ISOBMFF, not which still
/// image it is. It returns [`Format::Heic`], and `inspect_isobmff` confirms the container with
/// `gamut-heic`'s and then `gamut-avif`'s own predicates after parsing (see the module docs for why
/// the brand rule is not restated here), so an ISOBMFF file that is neither is reported as an
/// unsupported container rather than blamed on TIFF for having no byte-order mark.
fn detect(data: &[u8]) -> Format {
    match crate::sniff::sniff(data) {
        Some(Container::Png) => return Format::Png,
        Some(Container::Webp) => return Format::Webp,
        Some(Container::Isobmff) => return Format::Heic,
        Some(Container::Jpeg | Container::Jxl) | None => {}
    }
    if let Ok(file) = gamut::ifd::read(data)
        && file
            .ifds
            .first()
            // `DNGVersion` (50706): present in every DNG, absent in plain TIFF.
            .is_some_and(|ifd0| ifd0.get(gamut::dng::tags::DNG_VERSION).is_some())
    {
        return Format::Dng;
    }
    Format::Tiff
}

/// `ifd::print_lines`, where `lines` may already be truncated and `total` is how many there really
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
