//! The catalogue's `Type` and `Count` columns against a transcription of CIPA DC-008 itself.
//!
//! [`ExifTag::field_types`] and [`ExifTag::component_count`] are data inside the `exif_tags!`
//! invocation: a wrong constant there compiles, passes every structural drift guard in `tag.rs`, and
//! is not a mutant anything generates. `fixtures/dc008_tag_columns.txt` holds the `Field Name`,
//! `Tag ID (Hex)`, `Type` and `Count` columns of Tables 6, 8, 9, 14 and 16 as the vendored
//! specification prints them, in its own order and spelling, so a change to either side is a
//! reviewable diff against a file that reads like the spec. It is a second transcription, not a
//! derivation from the PDF: it moves an error from invisible to visible, and two independent
//! copies have to agree before one ships.
//!
//! The catalogue departs from a table in exactly the places [`DEPARTURES`] lists, each with the
//! clause that overrides the table; the fixture keeps the table's reading so the departure stays
//! visible rather than silently "fixed" in either direction.

use gamut_exif::{ExifTag, FieldType, IfdKind, TagCount};

/// The transcription under test.
const FIXTURE: &str = include_str!("fixtures/dc008_tag_columns.txt");

/// Catalogued tags whose `Type` deliberately differs from their table row: `(directory, Field
/// Name, the types the catalogue carries instead)`.
///
/// `GainControl` (0xA407): Table 9 prints `RATIONAL`, but the tag's own §4.6.6.7.41 says `SHORT`
/// and enumerates five integer codes (0-4), which a fraction cannot carry. The section wins; exiv2
/// and ExifTool read it the same way. `tag.rs` pins the reading itself
/// (`gain_control_follows_its_own_section_not_the_summary_table`).
const DEPARTURES: &[(IfdKind, &str, &[FieldType])] =
    &[(IfdKind::Exif, "GainControl", &[FieldType::Short])];

/// One fixture row, parsed into the catalogue's vocabulary.
#[derive(Debug)]
struct Row {
    /// 1-based line in the fixture, for failure messages.
    line: usize,
    ifd: IfdKind,
    name: &'static str,
    id: u16,
    types: Vec<FieldType>,
    count: TagCount,
}

/// The directory each table defines tags for (DC-008 §4.6.5-§4.6.8).
fn table_ifd(header: &str) -> IfdKind {
    let number = header
        .strip_prefix("[Table ")
        .and_then(|rest| rest.split(' ').next());
    match number {
        Some("6") => IfdKind::Image,
        Some("8" | "9") => IfdKind::Exif,
        Some("14") => IfdKind::Gps,
        Some("16") => IfdKind::Interop,
        _ => panic!("fixture names a table that defines no tags: {header}"),
    }
}

/// One `Type` spelling (`SHORT`, `UTF-8`, …) as the field type it names.
fn field_type(spelling: &str, line: usize) -> FieldType {
    match spelling {
        "BYTE" => FieldType::Byte,
        "ASCII" => FieldType::Ascii,
        "SHORT" => FieldType::Short,
        "LONG" => FieldType::Long,
        "RATIONAL" => FieldType::Rational,
        "UNDEFINED" => FieldType::Undefined,
        "SRATIONAL" => FieldType::SRational,
        "UTF-8" => FieldType::Utf8,
        other => panic!("line {line}: unknown Type spelling {other:?}"),
    }
}

/// A `Count` cell as the requirement it states.
///
/// `*S` is Table 6's StripsPerImage (or SamplesPerImage * StripsPerImage), which depends on the
/// image geometry and so is [`TagCount::Any`]; `3 * 256` is a product written out; `2 or 3 or 4`
/// is a set.
fn count(cell: &str, line: usize) -> TagCount {
    let number = |s: &str| {
        s.parse::<u32>()
            .unwrap_or_else(|_| panic!("line {line}: unparsable Count {cell:?}"))
    };
    match cell {
        "Any" | "*S" => TagCount::Any,
        _ if cell.contains(" or ") => {
            let set: Vec<u32> = cell.split(" or ").map(number).collect();
            TagCount::OneOf(set.leak())
        }
        _ if cell.contains(" * ") => TagCount::Exact(cell.split(" * ").map(number).product()),
        _ => TagCount::Exact(number(cell)),
    }
}

/// Every row of the fixture, with the directory of the table it sits in.
fn rows() -> Vec<Row> {
    let mut ifd = None;
    let mut rows = Vec::new();
    for (index, raw) in FIXTURE.lines().enumerate() {
        let line = index + 1;
        let text = raw.trim();
        if text.is_empty() || text.starts_with('#') {
            continue;
        }
        if text.starts_with('[') {
            ifd = Some(table_ifd(text));
            continue;
        }
        let cells: Vec<&'static str> = raw.split('|').map(str::trim).collect();
        let [name, hex, types, cell] = cells[..] else {
            panic!("line {line}: expected `Field Name | Hex | Type | Count`, got {raw:?}");
        };
        rows.push(Row {
            line,
            ifd: ifd.unwrap_or_else(|| panic!("line {line}: row before any `[Table …]` header")),
            name,
            id: u16::from_str_radix(hex, 16)
                .unwrap_or_else(|_| panic!("line {line}: unparsable Hex {hex:?}")),
            types: types.split(" or ").map(|t| field_type(t, line)).collect(),
            count: count(cell, line),
        });
    }
    rows
}

/// The catalogued tag a row describes: same directory, same Field Name.
fn catalogued(ifd: IfdKind, name: &str) -> Option<ExifTag> {
    ExifTag::ALL
        .iter()
        .copied()
        .find(|t| t.ifd() == ifd && t.name() == name)
}

/// The types the catalogue must carry for a row: the row's own, unless [`DEPARTURES`] overrides it.
fn expected_types(row: &Row) -> &[FieldType] {
    DEPARTURES
        .iter()
        .find(|&&(ifd, name, _)| ifd == row.ifd && name == row.name)
        .map_or(&row.types[..], |&(_, _, types)| types)
}

#[test]
fn every_dc008_row_matches_its_catalogued_tag() {
    let mut mismatches = Vec::new();
    for row in rows() {
        let Some(tag) = catalogued(row.ifd, row.name) else {
            mismatches.push(format!(
                "line {}: {} is not catalogued in {:?}",
                row.line, row.name, row.ifd
            ));
            continue;
        };
        if tag.tag_id() != row.id {
            mismatches.push(format!(
                "line {}: {} id {:#06X}, table says {:#06X}",
                row.line,
                row.name,
                tag.tag_id(),
                row.id
            ));
        }
        if tag.field_types() != expected_types(&row) {
            mismatches.push(format!(
                "line {}: {} Type {:?}, table says {:?}",
                row.line,
                row.name,
                tag.field_types(),
                expected_types(&row)
            ));
        }
        if tag.component_count() != row.count {
            mismatches.push(format!(
                "line {}: {} Count {}, table says {}",
                row.line,
                row.name,
                tag.component_count(),
                row.count
            ));
        }
    }
    assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));
}

#[test]
fn every_tag_claiming_a_dc008_type_has_exactly_one_row() {
    // The converse direction: a catalogued tag with a non-empty Type list claims DC-008 defines
    // it, so it must have a row — and one only, or a duplicate could mask a disagreement. A tag
    // with an empty list claims nothing (tag.rs module docs) and must have none.
    let rows = rows();
    for &tag in ExifTag::ALL {
        let matching = rows
            .iter()
            .filter(|r| r.ifd == tag.ifd() && r.name == tag.name())
            .count();
        let expected = usize::from(!tag.field_types().is_empty());
        assert_eq!(
            matching,
            expected,
            "{} ({:?}): {matching} fixture rows",
            tag.name(),
            tag.ifd()
        );
    }
}

#[test]
fn each_departure_still_contradicts_its_table_row() {
    // A departure is only an exception while the table and the catalogue disagree. If the row is
    // re-transcribed to the catalogue's reading, or the catalogue is "corrected" back to the table,
    // the entry has stopped describing anything and must be removed rather than left to excuse a
    // future slip.
    let rows = rows();
    for &(ifd, name, types) in DEPARTURES {
        let row = rows
            .iter()
            .find(|r| r.ifd == ifd && r.name == name)
            .unwrap_or_else(|| panic!("departure {name} has no fixture row"));
        assert_ne!(
            row.types, types,
            "{name}: the row already says what the catalogue does"
        );
        let tag =
            catalogued(ifd, name).unwrap_or_else(|| panic!("departure {name} not catalogued"));
        assert_eq!(
            tag.field_types(),
            types,
            "{name}: catalogue left the departure"
        );
    }
}
