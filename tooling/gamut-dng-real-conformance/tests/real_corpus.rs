//! Real camera DNGs, checked in four layers (gamut issue #174).
//!
//! Every input here was written by an actual camera, not by a test harness or by Adobe. The
//! workspace suite already gates gamut-dng against synthetic goldens and the Adobe SDK's own
//! `sample_files`; what those cannot cover is what real firmware emits — vendor preambles,
//! MakerNotes, appended trailers, monochrome files with no colour calibration, and the tag
//! combinations converters actually produce.
//!
//! Each file is checked against the corpus `MANIFEST.toml`, whose expectations were measured
//! rather than assumed, so a behaviour change fails here instead of passing quietly.

use std::collections::BTreeMap;

use gamut_core::ErrorKind;
use gamut_dng::{
    DecodedDng, DigestCheck, DngDecoder, DngRewrite, MakerNotePreservation, SubImageData,
    deconstruct,
};
use gamut_dng_real_conformance::{Expect, Sample, corpus_dir, manifest};

/// Reads a sample's bytes and verifies they are the ones the manifest pins down. A corpus file
/// that is not what the manifest says makes every other assertion meaningless.
fn read_sample(sample: &Sample) -> Vec<u8> {
    let path = corpus_dir().join(&sample.path);
    let data = std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    assert_eq!(
        sha256_hex(&data),
        sample.sha256,
        "{}: corpus file does not match its manifest checksum",
        sample.path
    );
    data
}

/// Layer 1 — byte accounting. The dual-ledger invariants are unconditional (a violation is a
/// gamut parser bug, never a property of the file), every byte classifies, and the bytes the
/// file's own structures do not account for are exactly the ones the manifest records.
#[test]
fn every_real_file_accounts_for_all_of_its_bytes() {
    let manifest = manifest();
    for sample in &manifest.samples {
        let name = &sample.path;
        let data = read_sample(sample);
        let report = deconstruct(&data).unwrap_or_else(|e| panic!("{name}: deconstruct: {e}"));

        assert!(
            report.segments.unclaimed_reads.is_empty(),
            "{name}: parser read bytes it never claimed: {:?}",
            report.segments.unclaimed_reads
        );
        assert!(
            report.segments.unread_claims.is_empty(),
            "{name}: parser claimed bytes it never read: {:?}",
            report.segments.unread_claims
        );
        assert!(
            report.segments.is_fully_classified(),
            "{name}: {} unclassified byte(s); conflicts {:?}; oob {:?}",
            report.segments.unclassified_bytes(),
            report.segments.conflicts,
            report.segments.out_of_bounds,
        );
        assert!(
            report.unknown_fields.is_empty(),
            "{name}: unknown field types: {:?}",
            report.unknown_fields
        );

        // Pinned exactly, so a genuine parser gap cannot hide among the named spans.
        let spans = report.segments.unclaimed_spans();
        assert_eq!(
            spans.len(),
            sample.expect.unaccounted_spans,
            "{name}: unaccounted span count changed: {spans:?}"
        );
        assert_eq!(
            report.segments.unclaimed_span_bytes(),
            sample.expect.unaccounted_bytes,
            "{name}: unaccounted byte count changed: {spans:?}"
        );
    }
    assert_eq!(manifest.samples.len(), 6, "the corpus lost files");
}

/// Layer 2 — decode. Geometry, storage and colour must match what the file actually contains,
/// and a deferred feature must surface as a typed refusal rather than a panic or wrong pixels.
#[test]
fn every_real_file_decodes_as_its_manifest_describes() {
    let manifest = manifest();
    let mut decoded_count = 0usize;
    for sample in &manifest.samples {
        let name = &sample.path;
        let data = read_sample(sample);
        let expect = &sample.expect;

        let decoded = match DngDecoder::new().decode(&data) {
            Ok(d) => {
                assert!(
                    expect.decodes,
                    "{name}: decoded, but the manifest says it must not"
                );
                d
            }
            Err(error) => {
                assert!(
                    !expect.decodes,
                    "{name}: decode failed: {error} ({:?})",
                    error.kind()
                );
                let want = expect
                    .error_kind
                    .as_deref()
                    .unwrap_or_else(|| panic!("{name}: decodes = false needs an error_kind"));
                assert_eq!(
                    format!("{:?}", error.kind()),
                    want,
                    "{name}: wrong error kind for a deferred feature"
                );
                continue;
            }
        };
        decoded_count += 1;
        assert_decoded(name, &decoded, expect);
    }
    assert!(
        decoded_count >= 5,
        "only {decoded_count} real files decoded; the corpus must not silently stop covering"
    );
}

/// Checks one decoded file against its expectations.
fn assert_decoded(name: &str, decoded: &DecodedDng, expect: &Expect) {
    assert_eq!(
        version_string(decoded.dng_version),
        expect.dng_version,
        "{name}: DNGVersion"
    );
    let raw = &decoded.raw;
    assert_eq!(
        [raw.dimensions().width(), raw.dimensions().height()],
        expect.dims,
        "{name}: dimensions"
    );
    assert_eq!(raw.bits_per_sample(), expect.bits, "{name}: bits");
    assert_eq!(
        raw.samples_per_pixel(),
        expect.samples,
        "{name}: samples per pixel"
    );
    assert_eq!(
        photometry_name(raw.photometry()),
        expect.photometry,
        "{name}: photometry"
    );
    assert_eq!(
        decoded.profile.is_some(),
        expect.profile,
        "{name}: colour profile presence — a file with no calibration must yield None, never an \
         invented profile"
    );
    assert_eq!(
        decoded.gain_table_map.is_some(),
        expect.gain_table_map,
        "{name}: ProfileGainTableMap"
    );
    assert_eq!(
        decoded.sub_images.len(),
        expect.sub_images,
        "{name}: sub-image count"
    );
    let undecoded = decoded
        .sub_images
        .iter()
        .filter(|s| matches!(s.data, SubImageData::Undecoded { .. }))
        .count();
    assert_eq!(
        undecoded, expect.undecoded_sub_images,
        "{name}: deferred sub-image payloads must surface verbatim, not error"
    );

    // The chapter-5 mapping must work on every decodable real file — it is what downstream raw
    // processors call instead of reimplementing the spec.
    raw.to_linear()
        .unwrap_or_else(|e| panic!("{name}: to_linear: {e}"));
}

/// Layer 3 — the stored `NewRawImageDigest`, verified under the rule the file's storage demands.
/// The lossy-JPEG file exercises the compressed-chunk rule, which needs no decodable pixels.
#[test]
fn every_real_file_matches_its_stored_digest() {
    for sample in &manifest().samples {
        let name = &sample.path;
        let data = read_sample(sample);
        let check = DngDecoder::new()
            .verify_new_raw_image_digest(&data)
            .unwrap_or_else(|e| panic!("{name}: verify digest: {e}"));
        match (&*sample.expect.raw_digest, check) {
            ("absent", DigestCheck::Absent) | ("match", DigestCheck::Match) => {}
            (want, got) => panic!("{name}: expected digest verdict {want:?}, got {got:?}"),
        }
    }
}

/// Layer 4 — the preserving rewrite. Real MakerNotes are exactly what the pinning exists for, and
/// a real trailer is exactly what "nothing dropped" has to mean.
#[test]
fn every_real_file_survives_a_preserving_rewrite() {
    let manifest = manifest();
    let mut rewritten = 0usize;
    for sample in &manifest.samples {
        let name = &sample.path;
        let data = read_sample(sample);
        let expect = &sample.expect;

        let rewrite = match DngRewrite::open(&data) {
            Ok(r) => r,
            Err(error) if error.kind() == ErrorKind::Unsupported => {
                assert!(!expect.rewritable, "{name}: refused: {error}");
                continue;
            }
            Err(e) => panic!("{name}: open: {e}"),
        };
        assert!(
            expect.rewritable,
            "{name}: opened, but manifest says it must not"
        );
        let out = rewrite
            .write()
            .unwrap_or_else(|e| panic!("{name}: write: {e}"));

        assert_eq!(
            maker_note_name(out.maker_note),
            expect.maker_note,
            "{name}: MakerNote preservation"
        );

        // Nothing dropped: every unaccounted run is carried through with its bytes intact.
        assert_eq!(
            out.preserved.len(),
            expect.unaccounted_spans,
            "{name}: preserved span count"
        );
        for span in &out.preserved {
            let from = usize::try_from(span.original_offset).expect("offset");
            let to = usize::try_from(span.offset).expect("offset");
            let len = usize::try_from(span.len).expect("len");
            assert_eq!(
                &out.bytes[to..to + len],
                &data[from..from + len],
                "{name}: a preserved run's bytes changed"
            );
            // A vendor preamble keeps its *position*, not just its bytes: a tool looking for
            // Apple ProRAW's `APPLEDNG` immediately after the header must still find it there
            // (issue #350). Every other run's original position is interior to a payload layout
            // the rewrite does not reproduce, so it is only required to survive.
            if span.kind == gamut_ifd::SpanKind::Preamble {
                assert_eq!(
                    span.offset, span.original_offset,
                    "{name}: the vendor preamble must keep its original offset"
                );
            }
        }
        // The rewritten stream's own audit must agree: the preamble is there, at that offset,
        // and classified as one — not merely bytes that happen to match.
        let rewritten_preamble = deconstruct(&out.bytes)
            .expect("deconstruct rewrite")
            .segments
            .segments
            .iter()
            .find(|s| s.kind == gamut_ifd::SpanKind::Preamble)
            .map(|s| (s.range.start, s.range.end - s.range.start));
        let original_preamble = out
            .preserved
            .iter()
            .find(|s| s.kind == gamut_ifd::SpanKind::Preamble)
            .map(|s| (s.original_offset, s.len));
        assert_eq!(
            rewritten_preamble, original_preamble,
            "{name}: the rewrite's own preamble segment must match the original's"
        );

        let before = deconstruct(&data).expect("deconstruct original");
        let after = deconstruct(&out.bytes).unwrap_or_else(|e| panic!("{name}: deconstruct: {e}"));
        assert!(
            after.segments.is_fully_classified(),
            "{name}: rewrite not fully classified: {:?}",
            after.segments.unclassified
        );
        let tags = |r: &gamut_dng::DeconstructReport| {
            r.unknown_tags.iter().map(|u| u.tag).collect::<Vec<_>>()
        };
        assert_eq!(
            tags(&after),
            tags(&before),
            "{name}: unknown-tag inventory changed across the rewrite"
        );

        // The Adobe SDK gate applies wherever the SDK accepts the original.
        if gamut_dng_oracle::validate_dng(&data).is_ok() {
            gamut_dng_oracle::validate_dng(&out.bytes)
                .unwrap_or_else(|e| panic!("{name}: Adobe SDK rejected the rewrite: {e}"));
        }
        rewritten += 1;
    }
    assert!(
        rewritten >= 6,
        "every real file must be rewritable ({rewritten} were)"
    );
}

/// Layer 3b — the Adobe DNG SDK differential: gamut's stage-2 linear image must agree with the
/// reference implementation's, within the ±1 code tolerance the workspace suite already uses.
///
/// Applied wherever the SDK accepts the file; the SDK is the authority on what a real file means,
/// and a disagreement here is the strongest possible signal that a decode is wrong.
#[test]
fn every_real_file_agrees_with_the_adobe_sdk() {
    let mut compared = 0usize;
    for sample in &manifest().samples {
        let name = &sample.path;
        if !sample.expect.decodes {
            continue;
        }
        let data = read_sample(sample);
        let Ok(reference) = gamut_dng_oracle::read_linear_dng(&data) else {
            continue; // the SDK declines this file; the structural layers still cover it
        };
        let decoded = DngDecoder::new()
            .decode(&data)
            .unwrap_or_else(|e| panic!("{name}: decode: {e}"));
        let ours = decoded
            .raw
            .to_linear()
            .unwrap_or_else(|e| panic!("{name}: to_linear: {e}"));

        assert_eq!(
            (reference.width, reference.height, reference.planes),
            (ours.width, ours.height, u32::from(ours.samples_per_pixel)),
            "{name}: stage-2 geometry disagrees with the Adobe SDK (active-area crop)"
        );
        assert_eq!(ours.samples.len(), reference.samples.len(), "{name}");
        // The same +/-1 code tolerance the workspace suite uses for stage-2 (`adobe_oracle.rs`).
        for (i, (&a, &b)) in ours.samples.iter().zip(&reference.samples).enumerate() {
            let ours16 = (f64::from(a) * 65535.0).round() as i32;
            let diff = (ours16 - i32::from(b)).abs();
            assert!(
                diff <= 1,
                "{name}: stage-2 sample {i} diverges - gamut {ours16} vs Adobe {b}"
            );
        }
        compared += 1;
    }
    assert!(
        compared >= 1,
        "the Adobe differential covered no file at all"
    );
}

/// Every corpus file must be CC0 — this repository redistributes them.
#[test]
fn every_sample_is_public_domain() {
    let manifest = manifest();
    let mut by_license: BTreeMap<&str, usize> = BTreeMap::new();
    for sample in &manifest.samples {
        *by_license.entry(sample.license.as_str()).or_default() += 1;
        assert_eq!(
            sample.license, "CC0-1.0",
            "{}: only CC0 samples may be redistributed",
            sample.path
        );
        assert!(
            sample.source.starts_with("https://"),
            "{}: provenance URL missing",
            sample.path
        );
        assert!(
            !sample.covers.is_empty(),
            "{}: a sample must say what it alone proves",
            sample.path
        );
    }
    assert_eq!(by_license.len(), 1, "{by_license:?}");
}

/// `DNGVersion` as its dotted string.
fn version_string(v: [u8; 4]) -> String {
    format!("{}.{}.{}.{}", v[0], v[1], v[2], v[3])
}

/// The manifest's spelling of a raw photometry.
fn photometry_name(photometry: &gamut_dng::RawPhotometry) -> &'static str {
    match photometry {
        gamut_dng::RawPhotometry::Cfa { .. } => "Cfa",
        gamut_dng::RawPhotometry::LinearRaw { .. } => "LinearRaw",
        _ => "Other",
    }
}

/// The manifest's spelling of a maker-note outcome.
fn maker_note_name(preservation: MakerNotePreservation) -> &'static str {
    match preservation {
        MakerNotePreservation::Absent => "Absent",
        MakerNotePreservation::Pinned => "Pinned",
        MakerNotePreservation::Relocated => "Relocated",
        _ => "Other",
    }
}

/// SHA-256 of `data` as lowercase hex — the corpus provenance check.
fn sha256_hex(data: &[u8]) -> String {
    let digest = gamut_dng_real_conformance::sha256(data);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}
