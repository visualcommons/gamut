//! End-to-end tests for what `gamut convert` does with the input's metadata on the PNG path
//! (issue #483): carried by default, dropped under `--strip-metadata`.
//!
//! These drive the built `gamut` binary (`CARGO_BIN_EXE_gamut`) rather than calling the command
//! function, because `crates/gamut-cli` is outside both the mutation globs and the coverage
//! regex — behaviour pinned only by a unit test here is pinned nowhere the gates can see. The
//! encoder-side claims are pinned in `gamut-png`; what this file adds is that the CLI wires them
//! up at all, which is exactly the gap the issue reported (0% metadata round-trip).

use std::path::PathBuf;
use std::process::Command;

use gamut::core::{Dimensions, EncodeImage, ImageRef, Rgba8};
use gamut::png::{PngEncoder, PngMetadata, SrgbIntent};

/// A 2×2 PNG carrying an EXIF block, a text annotation and a rendering intent.
fn png_with_metadata() -> Vec<u8> {
    let rgba = vec![255u8; 4 * 4];
    let dims = Dimensions {
        width: 2,
        height: 2,
    };
    let image = ImageRef::<Rgba8>::new(&rgba, dims).unwrap();
    PngEncoder::new()
        .with_exif(&[0x49, 0x49, 0x2A, 0x00, 0x08, 0x00, 0x00, 0x00])
        .with_text("Author", "nobody")
        .with_srgb(SrgbIntent::Perceptual)
        .encode_to_vec(image)
        .unwrap()
}

/// Writes `png` to a temp file, converts it to PNG with `extra` flags, and returns the output's
/// metadata. Both temp files are removed before the assertion runs.
fn convert(name: &str, png: &[u8], extra: &[&str]) -> PngMetadata {
    let dir = std::env::temp_dir();
    let input = dir.join(format!(
        "gamut-convert-{}-{name}-in.png",
        std::process::id()
    ));
    let output: PathBuf = dir.join(format!(
        "gamut-convert-{}-{name}-out.png",
        std::process::id()
    ));
    std::fs::write(&input, png).unwrap();

    let status = Command::new(env!("CARGO_BIN_EXE_gamut"))
        .arg("convert")
        .arg(&input)
        .arg(&output)
        .args(extra)
        .output()
        .expect("run gamut convert");
    let encoded = std::fs::read(&output).ok();
    let _ = std::fs::remove_file(&input);
    let _ = std::fs::remove_file(&output);

    assert!(
        status.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    gamut::png::metadata(&encoded.expect("output written")).expect("read back")
}

/// The issue's headline: `gamut convert` used to decode to raw RGBA and encode with a bare
/// builder, so every EXIF, ICC, XMP and text chunk was lost with no warning.
#[test]
fn png_to_png_carries_the_input_metadata_by_default() {
    let meta = convert("default", &png_with_metadata(), &[]);

    assert_eq!(
        meta.exif.as_deref(),
        Some(&[0x49, 0x49, 0x2A, 0x00, 0x08, 0x00, 0x00, 0x00][..])
    );
    assert_eq!(meta.srgb, Some(SrgbIntent::Perceptual));
    let texts: Vec<(&str, &str)> = meta
        .texts
        .iter()
        .map(|t| (t.keyword.as_str(), t.text.as_str()))
        .collect();
    assert_eq!(texts, [("Author", "nobody")]);
}

/// The opt-out: a stripped file is smaller, which is why the flag exists, but it has to be asked
/// for — the default may not silently discard colour information.
#[test]
fn strip_metadata_drops_it_all() {
    let meta = convert("stripped", &png_with_metadata(), &["--strip-metadata"]);

    assert_eq!(meta, PngMetadata::default());
}
