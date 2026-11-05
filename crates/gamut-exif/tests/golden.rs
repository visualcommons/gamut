//! Golden byte-level fixtures for the EXIF writer.
//!
//! These pin the *exact* bytes gamut emits for a fixed model, in both the marker and bare-TIFF
//! forms, so an unintended change in the offset layout or field ordering is caught immediately
//! (the differential exiv2 oracle only checks re-parsed values, which tolerate re-layout). The
//! fixed model includes an Exif 3.0 UTF-8 (type 129) field.
//!
//! To regenerate the fixtures after an intentional format change, run:
//!
//! ```text
//! GAMUT_REGEN_GOLDEN=1 cargo test -p gamut-exif --test golden
//! ```
//!
//! and commit the updated `tests/fixtures/*.bin`. Without the env var the test asserts equality.

use std::path::Path;

use gamut_exif::{ByteOrder, Exif, ExifTag, ExifWriter, Value};

/// A fixed, fully-deterministic model spanning the 0th IFD, the Exif sub-IFD (with a UTF-8 field),
/// the GPS sub-IFD, and a JPEG thumbnail.
fn golden_model() -> Exif {
    let mut exif = Exif::new(ByteOrder::LittleEndian);
    exif.set_tag(ExifTag::Make, Value::Ascii("gamut".into()));
    exif.set_tag(ExifTag::Model, Value::Ascii("Reference".into()));
    exif.set_tag(ExifTag::Orientation, Value::Short(vec![1]));
    exif.set_tag(ExifTag::ExifVersion, Value::Undefined(b"0300".to_vec()));
    exif.set_tag(ExifTag::FNumber, Value::Rational(vec![(28, 10)]));
    exif.set_tag(ExifTag::ExposureTime, Value::Rational(vec![(1, 250)]));
    exif.set_tag(ExifTag::PhotographicSensitivity, Value::Short(vec![100]));
    // Exif 3.0 UTF-8 (type 129).
    exif.set_tag(ExifTag::LensModel, Value::Utf8("50 mm ƒ1.8".into()));
    exif.set_tag(ExifTag::GpsLatitudeRef, Value::Ascii("N".into()));
    exif.set_tag(
        ExifTag::GpsLatitude,
        Value::Rational(vec![(48, 1), (51, 1), (0, 1)]),
    );
    exif.set_thumbnail(vec![0xFF, 0xD8, 0xFF, 0xD9]);
    exif
}

/// Asserts `actual` equals the fixture at `tests/fixtures/<name>`, or writes it when
/// `GAMUT_REGEN_GOLDEN` is set.
fn check_fixture(name: &str, actual: &[u8]) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    if std::env::var_os("GAMUT_REGEN_GOLDEN").is_some() {
        std::fs::create_dir_all(path.parent().expect("fixtures dir")).expect("mkdir fixtures");
        std::fs::write(&path, actual).expect("write fixture");
        return;
    }
    let expected = std::fs::read(&path).unwrap_or_else(|e| {
        panic!(
            "missing fixture {} ({e}); regenerate with GAMUT_REGEN_GOLDEN=1",
            path.display()
        )
    });
    assert_eq!(
        actual,
        expected.as_slice(),
        "byte-level drift in {name}; if intentional, regenerate with GAMUT_REGEN_GOLDEN=1"
    );
}

#[test]
fn marker_form_matches_golden() {
    check_fixture(
        "golden_marker_le.bin",
        &golden_model().to_bytes().expect("write"),
    );
}

#[test]
fn bare_form_matches_golden() {
    let bare = ExifWriter::new()
        .with_marker(false)
        .write(&golden_model())
        .expect("write");
    check_fixture("golden_bare_le.bin", &bare);
}

#[test]
fn golden_bytes_round_trip_with_utf8_and_thumbnail() {
    let bytes = golden_model().to_bytes().expect("write");
    let parsed = Exif::parse(&bytes).expect("parse golden");
    assert_eq!(parsed, golden_model());
    // The Exif 3.0 UTF-8 field and the JPEG thumbnail survive.
    assert_eq!(parsed.lens_model(), Some("50 mm ƒ1.8"));
    assert_eq!(
        parsed.thumbnail_bytes(),
        Some(&[0xFF, 0xD8, 0xFF, 0xD9][..])
    );
}
