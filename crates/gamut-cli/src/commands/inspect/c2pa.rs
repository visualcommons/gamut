//! The C2PA section every arm prints, and the provenance-only arms (HEIC, AVIF, WebP) that print
//! nothing else: each format crate's `C2paSummary` rendered as text lines and as a JSON object.

use super::json::{Json, print_json, span};
use super::{Format, MAX_LIST, Output, Target, document_head, hidden_entries, route_label};
use crate::error::CliError;

/// Reports a sniffed ISOBMFF file's provenance, once its container is confirmed: a HEVC still image
/// goes to the HEIC report, an AV1 still image to the AVIF one, and anything else is an unsupported
/// container.
///
/// The confirmations are the format crates' own predicates — `HeifContainer::is_hevc_still` and
/// `AvifContainer::is_av1_still`, each including the `mif1` case the major brand alone cannot settle —
/// so no brand list lives here. HEIC is tried first, which is the order this command always had;
/// an AVIF is parsed only once the HEIC confirmation declines. A file neither crate confirms keeps
/// the HEIC arm's outcome: its parse error (a top-level `moov`, say), or the unsupported-brand
/// message when it parsed.
pub(super) fn inspect_isobmff(to: Target<'_>, data: &[u8]) -> Result<(), CliError> {
    let heic = gamut::heic::HeifContainer::parse(data);
    if let Ok(container) = &heic
        && container.is_hevc_still()
    {
        return report_heic(to, container);
    }
    if let Ok(container) = gamut::avif::AvifContainer::parse(data)
        && container.is_av1_still()
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
pub(super) fn report_heic(
    to: Target<'_>,
    container: &gamut::heic::HeifContainer<'_>,
) -> Result<(), CliError> {
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
        Output::Json => print_provenance_json(to, Format::Heic, heif_c2pa_json(&summary)),
    }
    Ok(())
}

/// Reports what an AVIF file says about its own provenance, and nothing else — the HEIC report's
/// contract, over `gamut-avif`'s summary. Exit `0` whatever it finds.
pub(super) fn report_avif(
    to: Target<'_>,
    container: &gamut::avif::AvifContainer<'_>,
) -> Result<(), CliError> {
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
        Output::Json => print_provenance_json(to, Format::Avif, heif_c2pa_json(&summary)),
    }
    Ok(())
}

/// Reports what a WebP file says about its own provenance, and nothing else — the HEIC report's
/// contract, over `gamut-webp`'s summary. Exit `0` whatever it finds; a file whose RIFF chunk walk
/// fails exits non-zero with that error, like any container this command cannot open.
///
/// There is no forced-route label: the RIFF walk itself requires the `RIFF` magic and the `WEBP`
/// form, so `--format webp` cannot make this arm report on a file that is not a WebP.
pub(super) fn inspect_webp(to: Target<'_>, data: &[u8]) -> Result<(), CliError> {
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
pub(super) fn print_c2pa(head: Vec<String>, details: impl Iterator<Item = String>, total: usize) {
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

/// Prints the JSON document of a provenance-only arm (HEIC, AVIF, WebP): the head and the C2PA
/// object, and nothing else, because those arms report nothing else.
fn print_provenance_json(to: Target<'_>, format: Format, c2pa: Json) {
    let mut doc = document_head(to, format);
    doc.push(("c2pa", c2pa));
    print_json(&Json::Object(doc));
}

/// A HEIC or AVIF `C2paSummary` as data — the one summary `gamut-heif` shares between the two
/// crates — every list uncapped, the store bytes never included.
///
/// Both crates' summaries name the same two regions (issue #505): `range` (and `size`) the store
/// trimmed to its JUMBF `LBox`, so `extent` is `"store"`, and `slot` the box-bounded slot after the
/// merkle offset — the store *plus any padding*. `not_validated` carries the summary's own
/// disclaimer for the same reason the text report's headline does: "a store is present" reads as
/// *verified* to a program's author as much as to a terminal's reader.
fn heif_c2pa_json(summary: &gamut::heic::C2paSummary) -> Json {
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

/// A PNG `C2paSummary` as data: the one store §A.3.2 admits — its payload `range` and the whole
/// `chunk` a `c2pa.hash.data` assertion excludes — and every other `caBX` chunk as unread with its
/// reason.
pub(super) fn png_c2pa_json(summary: &gamut::png::C2paSummary) -> Json {
    use gamut::png::C2paUnreadReason as Reason;
    let reason = |r: Reason| match r {
        Reason::CrcMismatch => "crc_mismatch",
        Reason::AfterIdat => "after_idat",
        Reason::AfterIend => "after_iend",
        Reason::Duplicate => "duplicate",
        _ => "other",
    };
    let store = summary.store.as_ref().map_or(Json::Null, |s| {
        Json::Object(vec![
            ("size", Json::from(s.payload.len())),
            ("range", span(s.payload.start, s.payload.end)),
            ("chunk", span(s.chunk.start, s.chunk.end)),
        ])
    });
    Json::Object(vec![
        ("not_validated", Json::from(gamut::png::C2PA_NOT_VALIDATED)),
        ("store", store),
        (
            "unread",
            Json::Array(
                summary
                    .unread
                    .iter()
                    .map(|u| {
                        Json::Object(vec![
                            ("size", Json::from(u.span.payload.len())),
                            ("chunk", span(u.span.chunk.start, u.span.chunk.end)),
                            ("reason", Json::from(reason(u.reason))),
                            ("description", Json::from(u.reason.describe())),
                        ])
                    })
                    .collect(),
            ),
        ),
    ])
}

/// A TIFF/DNG `C2paSummary` (`gamut-ifd`'s, shared by both) as data: the one store §A.3.6 admits,
/// with its two §18.5.5 exclusion ranges, and every other tag-52545 entry as unread.
pub(super) fn ifd_c2pa_json(summary: &gamut::tiff::C2paSummary) -> Json {
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
    let store = summary.store.as_ref().map_or(Json::Null, |s| {
        let store = s.exclusions.store();
        let count = s.exclusions.count_field();
        Json::Object(vec![
            ("size", Json::from(s.size())),
            ("range", span(store.start, store.end)),
            ("count_field", span(count.start, count.end)),
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
                            ("entry", span(u.entry.start, u.entry.end)),
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

#[cfg(test)]
mod tests {
    use super::brand_label;

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
}
