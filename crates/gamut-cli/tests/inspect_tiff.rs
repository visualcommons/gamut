//! Golden tests for `gamut inspect` on TIFF and DNG: the text report and the `--output json`
//! document for a file carrying each `Anomaly` variant the two deconstructs produce, pinned byte
//! for byte (with the input path replaced by `<input>`) under `tests/golden/`.
//!
//! What each pins is the rendering this binary owns — `summarize_tiff`/`summarize_dng`, the
//! `anomaly_*` builders and `severity_level_*` in `src/commands/inspect.rs` — together with the
//! exit code and the stderr verdict. What the anomaly *means* is the format crate's contract and is
//! pinned by its own tests.
//!
//! Every fixture is the same hand-built little-endian classic TIFF (TIFF 6.0 §2): a two-byte strip
//! at offset 8 and one directory after it, every value inline. The baseline is fully accounted, so
//! each anomaly fixture differs from it by exactly the entries that raise its anomaly.
//!
//! Variants no file can reach through this binary are not pinned here: the TIFF deconstruct never
//! constructs `Anomaly::UnparsableTag`, and the generic `anomaly_other` arm and `Level::Other`
//! exist only for variants and severities the format crates have not added yet.
//!
//! Regenerate a golden after an intended rendering change with `GAMUT_BLESS=1 cargo test -p
//! gamut-cli --test inspect_tiff`, and review the diff.

use std::path::PathBuf;
use std::process::Command;

/// TIFF field type `SHORT` (§2, "Types").
const SHORT: u16 = 3;
/// TIFF field type `LONG`.
const LONG: u16 = 4;
/// TIFF field type `BYTE`.
const BYTE: u16 = 1;

/// One directory entry: tag, field type, count, and its inline value field.
type Entry = (u16, u16, u32, [u8; 4]);

/// A `SHORT` entry of count 1.
fn short(tag: u16, value: u16) -> Entry {
    let [a, b] = value.to_le_bytes();
    (tag, SHORT, 1, [a, b, 0, 0])
}

/// A `LONG` entry of count 1.
fn long(tag: u16, value: u32) -> Entry {
    (tag, LONG, 1, value.to_le_bytes())
}

/// The offset of the strip every fixture carries: directly after the 8-byte header.
const STRIP_AT: u32 = 8;
/// The strip itself — one row of a 2x1 8-bit greyscale image.
const STRIP: [u8; 2] = [0x10, 0x20];

/// The entries of a fully accounted 2x1 greyscale TIFF whose strip sits at [`STRIP_AT`].
fn baseline() -> Vec<Entry> {
    vec![
        short(256, 2),                 // ImageWidth
        short(257, 1),                 // ImageLength
        short(258, 8),                 // BitsPerSample
        short(259, 1),                 // Compression: none
        short(262, 1),                 // PhotometricInterpretation: BlackIsZero
        long(273, STRIP_AT),           // StripOffsets
        short(277, 1),                 // SamplesPerPixel
        short(278, 1),                 // RowsPerStrip
        long(279, STRIP.len() as u32), // StripByteCounts
    ]
}

/// The baseline with DNG's mandatory `DNGVersion` (50706, four `BYTE`s), which is what the sniff
/// routes a file to the DNG arm on.
fn dng_baseline() -> Vec<Entry> {
    let mut entries = baseline();
    entries.push((50706, BYTE, 4, [1, 4, 0, 0]));
    entries
}

/// `entries` with every entry under `tag` removed.
fn without(mut entries: Vec<Entry>, tag: u16) -> Vec<Entry> {
    entries.retain(|e| e.0 != tag);
    entries
}

/// `entries` with every entry under `tag` replaced by `entry`.
fn with(entries: Vec<Entry>, entry: Entry) -> Vec<Entry> {
    let mut entries = without(entries, entry.0);
    entries.push(entry);
    entries
}

/// `entries` with `entry` added beside any entry already under its tag.
fn plus(mut entries: Vec<Entry>, entry: Entry) -> Vec<Entry> {
    entries.push(entry);
    entries
}

/// Serialises `entries` — stably sorted by tag, as §2 requires, so a repeated tag stays adjacent
/// in the order given — as one directory after the header and [`STRIP`].
fn tiff(mut entries: Vec<Entry>) -> Vec<u8> {
    entries.sort_by_key(|e| e.0);
    let ifd_at = STRIP_AT + STRIP.len() as u32;
    let mut out = Vec::new();
    out.extend_from_slice(b"II");
    out.extend_from_slice(&42u16.to_le_bytes());
    out.extend_from_slice(&ifd_at.to_le_bytes());
    out.extend_from_slice(&STRIP);
    out.extend_from_slice(&u16::try_from(entries.len()).unwrap().to_le_bytes());
    for (tag, field_type, count, value) in entries {
        out.extend_from_slice(&tag.to_le_bytes());
        out.extend_from_slice(&field_type.to_le_bytes());
        out.extend_from_slice(&count.to_le_bytes());
        out.extend_from_slice(&value);
    }
    out.extend_from_slice(&0u32.to_le_bytes());
    out
}

/// Runs `gamut inspect [--output json] <file>` on `bytes` and renders the exit code, stdout and
/// stderr as one transcript, with the input path replaced by `<input>`.
fn transcript(name: &str, bytes: &[u8], json: bool) -> String {
    let path = std::env::temp_dir().join(format!(
        "gamut-inspect-tiff-test-{}-{name}",
        std::process::id()
    ));
    std::fs::write(&path, bytes).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_gamut"));
    command.arg("inspect");
    if json {
        command.args(["--output", "json"]);
    }
    let out = command.arg(&path).output().expect("run gamut inspect");
    let _ = std::fs::remove_file(&path);
    let shown = path.display().to_string();
    let stdout = String::from_utf8(out.stdout)
        .unwrap()
        .replace(&shown, "<input>");
    let stderr = String::from_utf8(out.stderr)
        .unwrap()
        .replace(&shown, "<input>");
    if json {
        serde_json::from_str::<serde_json::Value>(&stdout)
            .unwrap_or_else(|e| panic!("stdout is not one JSON document ({e}): {stdout}"));
    }
    format!(
        "exit: {:?}\n--- stdout ---\n{stdout}--- stderr ---\n{stderr}",
        out.status.code()
    )
}

/// Compares the text and JSON transcripts of `bytes` with `tests/golden/<name>.txt` and
/// `<name>.json`, or rewrites both when `GAMUT_BLESS` is set.
fn assert_golden(name: &str, bytes: &[u8]) {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden");
    for (json, ext) in [(false, "txt"), (true, "json")] {
        let actual = transcript(&format!("{name}.{ext}"), bytes, json);
        let golden = dir.join(format!("{name}.{ext}"));
        if std::env::var_os("GAMUT_BLESS").is_some() {
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(&golden, &actual).unwrap();
            continue;
        }
        let expected = std::fs::read_to_string(&golden)
            .unwrap_or_else(|e| panic!("{}: {e} (bless with GAMUT_BLESS=1)", golden.display()));
        assert_eq!(actual, expected, "{} differs", golden.display());
    }
}

#[test]
fn a_fully_accounted_tiff_reports_no_anomaly() {
    assert_golden("inspect-tiff-baseline", &tiff(baseline()));
}

#[test]
fn a_tiff_unknown_compression_code_renders_as_an_unknown_code_error() {
    assert_golden(
        "inspect-tiff-unknown-code",
        &tiff(with(baseline(), short(259, 999))),
    );
}

#[test]
fn a_tiff_image_without_strips_renders_as_a_structure_warning() {
    assert_golden(
        "inspect-tiff-structure-warning",
        &tiff(without(without(baseline(), 273), 279)),
    );
}

#[test]
fn a_tiff_strip_offsets_without_byte_counts_render_as_a_structure_error() {
    assert_golden(
        "inspect-tiff-structure-error",
        &tiff(without(baseline(), 279)),
    );
}

#[test]
fn a_tiff_repeated_tag_renders_as_a_duplicate_tag_error() {
    assert_golden(
        "inspect-tiff-duplicate-tag",
        &tiff(plus(baseline(), short(262, 1))),
    );
}

#[test]
fn a_fully_accounted_dng_reports_no_anomaly() {
    assert_golden("inspect-dng-baseline", &tiff(dng_baseline()));
}

#[test]
fn a_dng_unknown_compression_code_renders_as_an_unknown_code_error() {
    assert_golden(
        "inspect-dng-unknown-code",
        &tiff(with(dng_baseline(), short(259, 999))),
    );
}

#[test]
fn a_dng_colour_matrix_that_is_not_nine_rationals_renders_as_an_unparsable_tag_error() {
    // ColorMatrix1 (50721) as one SHORT where DNG 1.7.1 requires nine SRATIONALs.
    assert_golden(
        "inspect-dng-unparsable-tag",
        &tiff(plus(dng_baseline(), short(50721, 1))),
    );
}

#[test]
fn a_dng_image_without_strips_renders_as_a_structure_warning() {
    assert_golden(
        "inspect-dng-structure-warning",
        &tiff(without(without(dng_baseline(), 273), 279)),
    );
}

#[test]
fn a_dng_strip_offsets_without_byte_counts_render_as_a_structure_error() {
    assert_golden(
        "inspect-dng-structure-error",
        &tiff(without(dng_baseline(), 279)),
    );
}

#[test]
fn a_dng_repeated_tag_renders_as_a_duplicate_tag_error() {
    assert_golden(
        "inspect-dng-duplicate-tag",
        &tiff(plus(dng_baseline(), short(262, 1))),
    );
}
