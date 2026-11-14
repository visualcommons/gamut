//! The PNG arm: where a datastream's bytes went, what is held against it, and its C2PA section.

use super::c2pa::{png_c2pa_json, print_c2pa};
use super::json::{Json, print_json, span};
use super::{Format, Listed, MAX_LIST, Output, Target, document_head, print_lines_of, yes_no};
use crate::error::CliError;

/// Deconstructs a PNG and prints where its bytes went, then its C2PA section, exiting non-zero when
/// the file is not a complete, undamaged datastream. The C2PA section never moves that verdict.
pub(super) fn inspect_png(to: Target<'_>, data: &[u8]) -> Result<(), CliError> {
    // Inspection budgets differently from decoding. `gamut::png::deconstruct`'s default matches
    // the *decoder*'s, which guards a decode against hostile input; but a file this command
    // declines to inflate is a file it cannot verify, and at the decoder's 64 MiB that is every
    // PNG past 4096x4096 RGBA8 -- an ordinary photograph. Reading it is the whole job, so the
    // ceiling is raised to a gigabyte: past any real image, short of unbounded.
    let limits = gamut::png::DeconstructLimits::default().with_max_image_bytes(1 << 30);
    let report = gamut::png::deconstruct_with_limits(data, limits)?;
    // Read before anything is printed, like the TIFF/DNG section. It cannot fail once the
    // deconstruct has succeeded: the only error is a missing signature, which that already refused.
    let c2pa = gamut::png::c2pa_summary(data)?;
    let path = to.path;

    match to.output {
        Output::Text => {
            print_png(path, &report);
            // Additive: the section prints beside the byte accounting and never moves the verdict
            // below.
            print_c2pa(
                c2pa.summary_lines(),
                c2pa.detail_lines(),
                c2pa.detail_line_count(),
            );
        }
        Output::Json => {
            let mut doc = document_head(to, Format::Png);
            doc.extend(png_json(&report));
            doc.push(("c2pa", png_c2pa_json(&c2pa)));
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
