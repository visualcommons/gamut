//! End-to-end tests for `gamut inspect` on PNG: the verdict-to-exit-code mapping that
//! `docs/inspect-exit-codes.md` makes normative — verified exits 0, intact-but-unread exits 1
//! saying "not verified", and damaged exits 1 counting its findings.
//!
//! These drive the built `gamut` binary (`CARGO_BIN_EXE_gamut`) because the mapping lives in the
//! command's return value and `main`'s translation of it, which only the process exit shows.

use std::process::{Command, Output};

use gamut::core::{Dimensions, EncodeImage, ImageRef, Rgb8};
use gamut::png::PngEncoder;

/// An 8x8 RGB PNG from gamut's own encoder: a complete, undamaged datastream.
fn sound_png() -> Vec<u8> {
    let rgb: Vec<u8> = (0..8 * 8 * 3).map(|i| (i * 7) as u8).collect();
    let dims = Dimensions::new(8, 8).unwrap();
    let image = ImageRef::<Rgb8>::new(&rgb, dims).unwrap();
    let mut out = Vec::new();
    PngEncoder::new().encode_image(image, &mut out).unwrap();
    out
}

/// CRC-32 as PNG §5.5 defines it (ISO 3309 / ITU-T V.42, reflected, `0xEDB88320`), bit at a time.
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// Appends one framed chunk with a valid CRC.
fn push_chunk(png: &mut Vec<u8>, chunk_type: &[u8; 4], data: &[u8]) {
    png.extend_from_slice(&u32::try_from(data.len()).unwrap().to_be_bytes());
    let start = png.len();
    png.extend_from_slice(chunk_type);
    png.extend_from_slice(data);
    let crc = crc32(&png[start..]);
    png.extend_from_slice(&crc.to_be_bytes());
}

/// A PNG declaring 16384x16384 RGBA8 — exactly the gibibyte `gamut inspect` budgets for, so not
/// over it — over an eight-byte IDAT. Every chunk frames and its CRC holds, so nothing is known
/// against the file; but the stream is far too short to plausibly inflate to that image, so the
/// walk refuses to inflate it (`ImplausibleInflation`) and the file is intact yet unread.
fn intact_but_unread_png() -> Vec<u8> {
    let mut png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&16384u32.to_be_bytes());
    ihdr.extend_from_slice(&16384u32.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    push_chunk(&mut png, b"IHDR", &ihdr);
    // zlib's empty stream; its content is never inflated, only its length is weighed.
    push_chunk(
        &mut png,
        b"IDAT",
        &[0x78, 0x9C, 0x03, 0x00, 0x00, 0x00, 0x00, 0x01],
    );
    push_chunk(&mut png, b"IEND", &[]);
    png
}

/// Writes `bytes` to a unique temp file, runs `gamut inspect <file>`, removes it, and returns the
/// output.
fn run_inspect(name: &str, bytes: &[u8]) -> Output {
    let path = std::env::temp_dir().join(format!(
        "gamut-inspect-test-{}-{name}.png",
        std::process::id()
    ));
    std::fs::write(&path, bytes).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_gamut"))
        .arg("inspect")
        .arg(&path)
        .output()
        .expect("run gamut inspect");
    let _ = std::fs::remove_file(&path);
    output
}

#[test]
fn a_verified_png_exits_zero() {
    let out = run_inspect("verified", &sound_png());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("verified:      yes"), "{stdout}");
}

#[test]
fn an_intact_but_unread_png_exits_one_as_not_verified() {
    let out = run_inspect("unread", &intact_but_unread_png());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(1),
        "stdout: {stdout}\nstderr: {stderr}"
    );
    assert!(stdout.contains("intact:        yes"), "{stdout}");
    assert!(stdout.contains("verified:      no"), "{stdout}");
    assert!(stderr.contains("not verified"), "{stderr}");
    assert!(
        !stderr.contains("not a complete, undamaged PNG datastream"),
        "an unread file is not reported as damaged: {stderr}"
    );
}

#[test]
fn a_damaged_png_exits_one_counting_its_findings() {
    // A CRC mismatch in IEND: the one finding is a chunk the walk frames but cannot trust.
    let mut png = sound_png();
    let last = png.len() - 1;
    png[last] ^= 0xFF;
    let out = run_inspect("damaged", &png);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(1),
        "stdout: {stdout}\nstderr: {stderr}"
    );
    assert!(stdout.contains("intact:        no"), "{stdout}");
    assert!(stdout.contains("CRC mismatch in IEND"), "{stdout}");
    assert!(
        stderr.contains("not a complete, undamaged PNG datastream — 1 finding(s)"),
        "{stderr}"
    );
}

#[test]
fn a_png_that_ends_cleanly_without_iend_lists_the_missing_iend_as_its_finding() {
    // Every remaining chunk is sound and the IDAT is whole, so the missing IEND is the only thing
    // against the file; before it was counted, this exited 1 reporting "0 finding(s)".
    let mut png = sound_png();
    png.truncate(png.len() - 12);
    let out = run_inspect("no-iend", &png);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(1),
        "stdout: {stdout}\nstderr: {stderr}"
    );
    assert!(stdout.contains("no IEND chunk"), "{stdout}");
    assert!(
        stderr.contains("not a complete, undamaged PNG datastream — 1 finding(s)"),
        "{stderr}"
    );
}

// `--output json` (#596): the same verdicts and facts as one JSON document, with the exit code and
// the stderr message unchanged.

/// Runs `gamut inspect --output json <file>` and parses stdout as one JSON document.
fn run_inspect_json(name: &str, bytes: &[u8]) -> (Output, serde_json::Value) {
    let path = std::env::temp_dir().join(format!(
        "gamut-inspect-json-test-{}-{name}.png",
        std::process::id()
    ));
    std::fs::write(&path, bytes).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_gamut"))
        .args(["inspect", "--output", "json"])
        .arg(&path)
        .output()
        .expect("run gamut inspect");
    let _ = std::fs::remove_file(&path);
    let doc = serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
        panic!(
            "stdout is not one JSON document ({e}): {}",
            String::from_utf8_lossy(&output.stdout)
        )
    });
    (output, doc)
}

#[test]
fn json_output_of_a_verified_png_carries_its_header_chunks_and_filter_histogram() {
    let png = sound_png();
    let (out, doc) = run_inspect_json("json-verified", &png);
    assert!(out.status.success(), "{doc}");
    assert_eq!(doc["format"], "png");
    assert_eq!(
        doc["image"],
        serde_json::json!({
            "width": 8, "height": 8, "color_type": 2, "bit_depth": 8, "interlaced": false
        })
    );
    assert_eq!(doc["file_len"], png.len());
    let types: Vec<&str> = doc["chunks"]
        .as_array()
        .expect("chunks")
        .iter()
        .map(|c| c["type"].as_str().expect("type"))
        .collect();
    assert_eq!(types, ["IHDR", "IDAT", "IEND"], "{doc}");
    assert_eq!(doc["filters"]["counted"], true);
    assert_eq!(doc["filters"]["scanlines"], 8);
    assert_eq!(doc["findings"], serde_json::json!([]));
    assert_eq!(doc["fully_classified"], true);
    assert_eq!(doc["intact"], true);
    assert_eq!(doc["verified"], true);
}

#[test]
fn json_output_of_an_unread_png_names_the_skip_reason_and_keeps_exit_one() {
    let (out, doc) = run_inspect_json("json-unread", &intact_but_unread_png());
    assert_eq!(out.status.code(), Some(1), "{doc}");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("not verified"),
        "the stderr verdict is the same as the text report's"
    );
    assert_eq!(
        doc["filters"],
        serde_json::json!({
            "counted": false,
            "reason": "implausible_inflation",
            "damage": false,
            "description":
                "the IDAT stream is too short to plausibly inflate to the image the header declares"
        })
    );
    assert_eq!(doc["intact"], true);
    assert_eq!(doc["verified"], false);
}

#[test]
fn json_output_lists_every_png_finding_past_the_text_cap() {
    // Twenty-five ancillary chunks whose CRCs do not match, then a missing IEND: the text report
    // shows twenty findings and a tail; the document carries all twenty-six, in report order.
    let mut png = sound_png();
    png.truncate(png.len() - 12);
    let first_bad = png.len();
    for _ in 0..25 {
        push_chunk(&mut png, b"teXt", b"x");
        let last = png.len() - 1;
        png[last] ^= 0xFF;
    }
    let (out, doc) = run_inspect_json("json-findings", &png);
    assert_eq!(out.status.code(), Some(1), "{doc}");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("26 finding(s)"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let findings = doc["findings"].as_array().expect("findings");
    assert_eq!(findings.len(), 26, "{doc}");
    assert_eq!(findings[0]["kind"], "crc_mismatch");
    assert_eq!(
        findings[0]["range"],
        serde_json::json!({ "start": first_bad, "end": first_bad + 13 })
    );
    assert_eq!(findings[25]["kind"], "missing_iend");
    assert_eq!(doc["intact"], false);
}

#[test]
fn json_output_prints_nothing_for_a_file_the_walk_cannot_open() {
    let path = std::env::temp_dir().join(format!(
        "gamut-inspect-json-test-{}-garbage.png",
        std::process::id()
    ));
    std::fs::write(
        &path,
        [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0],
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_gamut"))
        .args(["inspect", "--output", "json"])
        .arg(&path)
        .output()
        .expect("run gamut inspect");
    let _ = std::fs::remove_file(&path);
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty(), "no partial document may be printed");
}
