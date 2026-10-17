//! `gamut inspect`'s C2PA report against c2pa-rs (issue #846): on a file c2pa-rs embedded a
//! store into, the presence, size and offsets the report prints are the numbers c2pa-rs wrote.
//!
//! The report is not computed in `gamut-cli`. Each format crate's non-validating `c2pa_summary`
//! holds the facts and the command prints them (`crates/gamut-cli/src/commands/inspect.rs`), so
//! the claim is made where it lives: once per summary, against the store's own JUMBF span
//! ([`jumbf_superbox_span`]) and — for the data-hashed containers — against the exclusions
//! c2pa-rs's own `c2pa.hash.data` lists ([`data_hash_exclusions`]). The BMFF containers are bound
//! by `c2pa.hash.bmff.v3`, which excludes by box path rather than byte range, so for AVIF and HEIC
//! the store span is the whole comparison.
//!
//! One test then runs the built `gamut` binary by path, so that the numbers a user actually reads
//! — the JSON document `gamut inspect --output json` prints — are held to c2pa-rs as well, and a
//! command that mis-carried a correct summary would fail here too. `gamut-cli` is a binary crate
//! with no library to link, so the binary is the seam: [`gamut_binary`] builds it from this
//! checkout, or takes the one `GAMUT_BIN` names.

mod common;

use std::ops::Range;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use c2pa_oracle::{
    AVIF_MIME, DNG_MIME, PNG_MIME, TIFF_MIME, WEBP_MIME, data_hash_exclusions, embed,
    jumbf_superbox_span,
};
use common::{dims, plain_avif, source_rgb};
use gamut_core::{EncodeImage, ImageRef, Rgb8};
use gamut_dng::{CalibrationIlluminant, CameraProfile, DngEncoder, RawImage, cfa_color};
use gamut_png::PngEncoder;
use gamut_tiff::TiffEncoder;
use gamut_webp::WebpEncoder;

fn image(rgb: &[u8]) -> ImageRef<'_, Rgb8> {
    ImageRef::new(rgb, dims()).expect("buffer matches dimensions")
}

/// `asset` with a store c2pa-rs embedded into it, choosing the placement itself.
fn embedded(mime: &str, asset: &[u8]) -> Vec<u8> {
    embed(mime, asset).unwrap_or_else(|error| panic!("{mime}: c2pa-rs embeds: {error}"))
}

/// The store's own span, derived from its JUMBF header and not from any gamut parse.
fn store_span(asset: &[u8]) -> Range<usize> {
    jumbf_superbox_span(asset).expect("the embedded asset carries a JUMBF superbox")
}

/// c2pa-rs's own exclusions, in ascending order.
fn exclusions(mime: &str, asset: &[u8]) -> Vec<Range<usize>> {
    let mut wrote = data_hash_exclusions(mime, asset).expect("c2pa-rs's own data hash");
    wrote.sort_by_key(|range| range.start);
    wrote
}

/// A `gamut_ifd::Range` as a half-open range.
fn ifd_range(range: gamut_tiff::Range) -> Range<usize> {
    let start = usize::try_from(range.start).expect("a fixture offset fits usize");
    start..start + usize::try_from(range.len).expect("a fixture length fits usize")
}

fn plain_png() -> Vec<u8> {
    PngEncoder::new()
        .encode_to_vec(image(&source_rgb()))
        .expect("the PNG fixture encodes")
}

fn plain_webp() -> Vec<u8> {
    WebpEncoder::lossless()
        .encode_to_vec(image(&source_rgb()))
        .expect("the WebP fixture encodes")
}

fn plain_tiff() -> Vec<u8> {
    TiffEncoder::new()
        .encode_to_vec(image(&source_rgb()))
        .expect("the TIFF fixture encodes")
}

/// A 12-bit RGGB mosaic with a one-matrix D65 profile — the smallest raw `DngEncoder` writes.
fn plain_dng() -> Vec<u8> {
    const BITS: u16 = 12;
    let max = (1u32 << BITS) - 1;
    let (w, h) = (common::W, common::H);
    let samples = (0..w * h)
        .map(|i| ((((i % w) * 53) ^ ((i / w) * 97)) % max) as u16)
        .collect();
    let pattern = vec![
        cfa_color::RED,
        cfa_color::GREEN,
        cfa_color::GREEN,
        cfa_color::BLUE,
    ];
    let raw = RawImage::new_cfa(dims(), BITS, (2, 2), pattern, samples)
        .and_then(|raw| raw.with_black_level(0.0))
        .and_then(|raw| raw.with_white_level(f64::from(max)))
        .expect("the mosaic is well-formed");
    let profile = CameraProfile::new(
        "gamut c2pa-oracle",
        [
            0.6722, -0.0635, -0.0963, -0.4287, 1.2460, 0.2028, -0.0908, 0.2162, 0.5668,
        ],
        CalibrationIlluminant::D65,
        [0.5128, 1.0, 0.7059],
    )
    .expect("the profile is well-formed");
    let mut dng = Vec::new();
    DngEncoder::new()
        .encode(&raw, &profile, &mut dng)
        .expect("the DNG fixture encodes");
    dng
}

#[test]
fn the_png_summary_reports_the_store_and_chunk_c2pa_rs_wrote() {
    let asset = embedded(PNG_MIME, &plain_png());
    let summary = gamut_png::c2pa_summary(&asset).expect("gamut-png reads the PNG c2pa-rs wrote");
    let store = summary
        .store
        .expect("the summary reports the store c2pa-rs embedded");

    assert!(summary.unread.is_empty(), "c2pa-rs writes one `caBX` chunk");
    assert_eq!(
        store.payload,
        store_span(&asset),
        "the store's offsets and size"
    );
    assert_eq!(
        vec![store.chunk],
        exclusions(PNG_MIME, &asset),
        "the chunk the summary reports is the one range c2pa-rs's data hash excludes"
    );
}

#[test]
fn the_webp_summary_reports_the_store_c2pa_rs_wrote() {
    let asset = embedded(WEBP_MIME, &plain_webp());
    let summary =
        gamut_webp::c2pa_summary(&asset).expect("gamut-webp reads the WebP c2pa-rs wrote");

    assert!(summary.unread.is_empty(), "c2pa-rs writes one `C2PA` chunk");
    let [store] = summary.stores.as_slice() else {
        panic!("one store, as c2pa-rs wrote: {:?}", summary.stores);
    };
    assert_eq!(
        store.range,
        store_span(&asset),
        "the store's offsets and size"
    );
    // The summary reports the payload; c2pa-rs excludes the chunk with its eight-byte header
    // (`tests/riff_webp.rs` pins that choice), so the exclusion opens eight bytes earlier.
    assert_eq!(
        vec![store.range.start - 8..store.range.end],
        exclusions(WEBP_MIME, &asset),
        "the store's chunk is the one range c2pa-rs's data hash excludes"
    );
}

#[test]
fn the_tiff_and_dng_summaries_report_both_ranges_c2pa_rs_wrote() {
    type Summarize = fn(&[u8]) -> gamut_tiff::C2paSummary;
    let containers: [(&str, Vec<u8>, Summarize); 2] = [
        (TIFF_MIME, plain_tiff(), |file| {
            gamut_tiff::c2pa_summary(file).expect("gamut-tiff reads the TIFF c2pa-rs wrote")
        }),
        (DNG_MIME, plain_dng(), |file| {
            gamut_dng::c2pa_summary(file).expect("gamut-dng reads the DNG c2pa-rs wrote")
        }),
    ];
    for (mime, plain, summarize) in containers {
        let asset = embedded(mime, &plain);
        let summary = summarize(&asset);
        let store = summary
            .store
            .unwrap_or_else(|| panic!("{mime}: the summary reports the store c2pa-rs embedded"));

        assert!(
            summary.unread.is_empty(),
            "{mime}: c2pa-rs writes one store"
        );
        assert_eq!(
            ifd_range(store.exclusions.store()),
            store_span(&asset),
            "{mime}: the store's offsets and size"
        );
        let mut reported = vec![
            ifd_range(store.exclusions.store()),
            ifd_range(store.exclusions.count_field()),
        ];
        reported.sort_by_key(|range| range.start);
        assert_eq!(
            reported,
            exclusions(mime, &asset),
            "{mime}: the store and `count`-field ranges are the two c2pa-rs's data hash excludes"
        );
    }
}

#[test]
fn the_avif_and_heic_summaries_report_the_store_c2pa_rs_wrote() {
    // `gamut-heic` is measured on the AVIF for the reason `tests/locate_embedded.rs` gives: C2PA
    // 2.4 Appendix A defines one placement for every BMFF asset, and c2pa-rs writes it with one
    // writer for both MIME types.
    let asset = embedded(AVIF_MIME, &plain_avif());
    let expected = store_span(&asset);

    let avif = gamut_avif::AvifContainer::parse(&asset)
        .expect("gamut-avif parses the AVIF c2pa-rs wrote")
        .c2pa_summary();
    let [store] = avif.stores.as_slice() else {
        panic!("gamut-avif: one store, as c2pa-rs wrote: {:?}", avif.stores);
    };
    assert!(avif.unread.is_empty(), "gamut-avif: c2pa-rs writes one box");
    assert_eq!(
        store.range.start()..store.range.end(),
        expected,
        "gamut-avif: the store's offsets and size"
    );

    let heic = gamut_heic::HeifContainer::parse(&asset)
        .expect("gamut-heic parses the AVIF c2pa-rs wrote")
        .c2pa_summary();
    let [store] = heic.stores.as_slice() else {
        panic!("gamut-heic: one store, as c2pa-rs wrote: {:?}", heic.stores);
    };
    assert!(heic.unread.is_empty(), "gamut-heic: c2pa-rs writes one box");
    assert_eq!(
        store.range.start()..store.range.end(),
        expected,
        "gamut-heic: the store's offsets and size"
    );
}

/// The `gamut` binary this checkout builds, or the one the `GAMUT_BIN` environment variable names.
///
/// Built with the cargo running this test, `--locked` against the workspace's own lockfile, into a
/// target directory under this crate's own (`CARGO_TARGET_TMPDIR`): never the workspace's, whose
/// build lock a concurrent workspace build may hold, so this cannot block on it. `gamut-cli`
/// depends on the umbrella's `all` feature, which takes `gamut-jxl`'s libjxl encoder with it, so
/// building it needs the CMake and C++ toolchain the workspace build does — the one test here that
/// does. `GAMUT_BIN` skips the build for a caller that already has the binary.
fn gamut_binary() -> PathBuf {
    if let Some(path) = std::env::var_os("GAMUT_BIN") {
        return path.into();
    }
    let workspace = concat!(env!("CARGO_MANIFEST_DIR"), "/../../Cargo.toml");
    let target_dir = concat!(env!("CARGO_TARGET_TMPDIR"), "/gamut-cli");
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let output = Command::new(cargo)
        .args([
            "build",
            "--locked",
            "--package",
            "gamut-cli",
            "--bin",
            "gamut",
        ])
        .args(["--message-format", "json-render-diagnostics"])
        .args(["--manifest-path", workspace, "--target-dir", target_dir])
        .stderr(Stdio::inherit())
        .output()
        .expect("run cargo to build the gamut binary");
    assert!(output.status.success(), "building the gamut binary failed");
    // The executable path from cargo's own artifact report, so no target-layout assumption.
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|message| message["reason"] == "compiler-artifact")
        .find_map(|message| message["executable"].as_str().map(PathBuf::from))
        .expect("cargo reports the gamut executable it built")
}

#[test]
fn the_built_gamut_inspect_prints_the_png_store_and_chunk_c2pa_rs_wrote() {
    let asset = embedded(PNG_MIME, &plain_png());
    let path = std::env::temp_dir().join(format!("c2pa-oracle-inspect-{}.png", std::process::id()));
    std::fs::write(&path, &asset).expect("write the embedded PNG");
    let output = Command::new(gamut_binary())
        .args(["inspect", "--output", "json"])
        .arg(&path)
        .output()
        .expect("run gamut inspect");
    let _ = std::fs::remove_file(&path);
    let doc: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "gamut inspect printed no JSON document ({error}): {}",
            String::from_utf8_lossy(&output.stdout)
        )
    });

    let span = |range: Range<usize>| serde_json::json!({ "start": range.start, "end": range.end });
    let store = store_span(&asset);
    let [chunk] = exclusions(PNG_MIME, &asset)
        .try_into()
        .expect("c2pa-rs excludes one range from a PNG");
    assert_eq!(
        doc["c2pa"]["store"],
        serde_json::json!({ "size": store.len(), "range": span(store), "chunk": span(chunk) }),
        "the size, offsets and chunk `gamut inspect` prints are the numbers c2pa-rs wrote: {doc}"
    );
}
