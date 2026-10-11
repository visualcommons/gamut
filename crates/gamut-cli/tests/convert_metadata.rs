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

/// A 2×2 PNG carrying an EXIF block, a text annotation, a rendering intent and a C2PA manifest
/// store — the last being the one payload a re-encode may not carry.
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
        .with_c2pa(b"\0\0\0\x10jumbc2pa")
        .encode_to_vec(image)
        .unwrap()
}

/// Writes `png` to a temp file, converts it to PNG with `extra` flags, and returns the output's
/// metadata together with what the command said on stderr.
fn convert(name: &str, png: &[u8], extra: &[&str]) -> (PngMetadata, String) {
    let (encoded, stderr) = convert_bytes(name, png, extra);
    (gamut::png::metadata(&encoded).expect("read back"), stderr)
}

/// [`convert`], returning the output file itself rather than its metadata. Both temp files are
/// removed before the assertion runs.
fn convert_bytes(name: &str, png: &[u8], extra: &[&str]) -> (Vec<u8>, String) {
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
    (
        encoded.expect("output written"),
        String::from_utf8_lossy(&status.stderr).into_owned(),
    )
}

/// The issue's headline: `gamut convert` used to decode to raw RGBA and encode with a bare
/// builder, so every EXIF, ICC, XMP and text chunk was lost with no warning.
#[test]
fn png_to_png_carries_the_input_metadata_by_default() {
    let (meta, _) = convert("default", &png_with_metadata(), &[]);

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
    let (meta, _) = convert("stripped", &png_with_metadata(), &["--strip-metadata"]);

    assert_eq!(meta, PngMetadata::default());
}

/// A payload the command could not carry is *said*, not swallowed. A C2PA manifest store is
/// signed over the bytes of the file it was made for (C2PA 2.4 §A.3.2), so a copy would be
/// invalid — but the caller asked for preservation and is entitled to know their provenance did
/// not survive. Warnings reach stderr at the default verbosity, so this needs no `-v`.
#[test]
fn a_payload_that_cannot_be_carried_is_reported_on_stderr() {
    let (meta, stderr) = convert("dropped", &png_with_metadata(), &[]);

    assert!(meta.c2pa.is_none(), "the store is not carried");
    assert!(
        stderr.contains("C2PA manifest store"),
        "stderr said nothing about the store: {stderr}"
    );
}

/// The other half of the report: a payload that *was* written, but not as §11.3.3 would have it,
/// is worded as carried rather than as lost. A trailing space in a keyword is one §11.3.3.1 does
/// not permit and this crate's reader accepts, so the annotation goes through verbatim.
#[test]
fn a_payload_carried_with_a_caveat_is_reported_as_carried() {
    let rgba = vec![255u8; 4 * 4];
    let image = ImageRef::<Rgba8>::new(&rgba, Dimensions::new(2, 2).unwrap()).unwrap();
    let png = PngEncoder::new()
        .with_text("Author ", "nobody")
        .encode_to_vec(image)
        .unwrap();

    let (meta, stderr) = convert("caveat", &png, &[]);

    let keywords: Vec<&str> = meta.texts.iter().map(|t| t.keyword.as_str()).collect();
    assert_eq!(
        keywords,
        ["Author "],
        "the annotation is written as it arrived"
    );
    assert!(
        stderr.contains("input metadata carried with a caveat")
            && stderr.contains("leading, trailing or consecutive space"),
        "stderr did not report the carried annotation: {stderr}"
    );
    assert!(
        !stderr.contains("not carried"),
        "a written annotation was reported as lost: {stderr}"
    );
}

/// §11.3.2.3 pins an RGB profile to colour types 2, 3 and 6. `gamut convert` auto-reduces, and
/// the default path carries the input's profile, so grey content in an RGB file used to come out
/// as greyscale under the RGB profile it was converted with — a pairing libpng rejects, ignoring
/// the profile. The reduction now stays in the profile's family.
#[test]
fn grey_content_under_an_rgb_profile_is_not_reduced_to_greyscale() {
    let rgba: Vec<u8> = (0..64u8)
        .flat_map(|i| [i * 3; 3].into_iter().chain([255]))
        .collect();
    let image = ImageRef::<Rgba8>::new(&rgba, Dimensions::new(8, 8).unwrap()).unwrap();
    let mut icc = vec![0u8; 128];
    icc[16..20].copy_from_slice(b"RGB ");
    let png = PngEncoder::new()
        .with_icc_profile("rgb", &icc)
        .encode_to_vec(image)
        .unwrap();

    let (out, _) = convert_bytes("rgb-profile", &png, &[]);

    let color_type = out[25];
    assert!(
        matches!(color_type, 2 | 3 | 6),
        "colour type {color_type} under an RGB profile"
    );
    let meta = gamut::png::metadata(&out).expect("read back");
    assert_eq!(meta.icc_profile.map(|p| p.profile), Some(icc));
}
