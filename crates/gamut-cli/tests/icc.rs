//! End-to-end tests for `gamut icc`: extract an embedded ICC profile from a container and parse it.
//!
//! These drive the built `gamut` binary (`CARGO_BIN_EXE_gamut`) so they exercise the real command
//! path — container sniffing, blob extraction, and the printed report — the way a user runs it.

use std::process::{Command, Output};

use gamut::core::{Dimensions, EncodeImage, ImageRef, Rgba8};
use gamut::png::PngEncoder;

/// A minimal but valid 132-byte ICC profile: an RGB→XYZ display profile with an empty tag table.
fn minimal_icc() -> Vec<u8> {
    let mut bytes = vec![0u8; 128];
    bytes[12..16].copy_from_slice(b"mntr"); // device class
    bytes[16..20].copy_from_slice(b"RGB "); // data colour space
    bytes[20..24].copy_from_slice(b"XYZ "); // PCS
    bytes[36..40].copy_from_slice(b"acsp"); // profile signature
    bytes.extend_from_slice(&0u32.to_be_bytes()); // an empty tag table
    bytes
}

/// A PNG with `icc` embedded in its `iCCP` chunk (or none if `icc` is empty).
fn png_with_icc(icc: &[u8]) -> Vec<u8> {
    let rgba = vec![255u8; 4 * 4]; // 2×2 opaque white
    let dims = Dimensions::new(2, 2).unwrap();
    let image = ImageRef::<Rgba8>::new(&rgba, dims).unwrap();
    let mut encoder = PngEncoder::new();
    if !icc.is_empty() {
        encoder = encoder.with_icc_profile("minimal", icc);
    }
    let mut out = Vec::new();
    encoder.encode_image(image, &mut out).unwrap();
    out
}

/// Writes `bytes` to a unique temp file, runs `gamut icc <file>`, removes it, and returns the output.
fn run_icc(name: &str, bytes: &[u8]) -> Output {
    let path = std::env::temp_dir().join(format!("gamut-icc-test-{}-{name}", std::process::id()));
    std::fs::write(&path, bytes).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_gamut"))
        .arg("icc")
        .arg(&path)
        .output()
        .expect("run gamut icc");
    let _ = std::fs::remove_file(&path);
    output
}

#[test]
fn parses_a_standalone_icc_profile() {
    let out = run_icc("profile.icc", &minimal_icc());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("raw ICC"), "stdout:\n{stdout}");
    assert!(stdout.contains("Display (mntr)"), "stdout:\n{stdout}");
    assert!(stdout.contains("conformance:"), "stdout:\n{stdout}");
}

#[test]
fn extracts_the_icc_profile_embedded_in_a_png() {
    // gamut-png writes the profile into an iCCP chunk; the command reads it back and parses it.
    let out = run_icc("image.png", &png_with_icc(&minimal_icc()));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("PNG"), "stdout:\n{stdout}");
    assert!(stdout.contains("Display (mntr)"), "stdout:\n{stdout}");
}

#[test]
fn reports_a_png_without_a_profile() {
    let out = run_icc("noicc.png", &png_with_icc(&[]));
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("no embedded ICC profile"),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}
