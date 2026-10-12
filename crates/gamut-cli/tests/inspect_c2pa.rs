//! End-to-end tests for `gamut inspect`'s C2PA section: which containers each arm accepts, and
//! what the report says once it does — HEIC (#448), AVIF, WebP, TIFF and DNG (#530), and PNG
//! (#727).
//!
//! These drive the built `gamut` binary (`CARGO_BIN_EXE_gamut`) so they exercise the real command
//! path — the format sniff, the container confirmation, and the lines that reach stdout — the way a
//! user runs it. What each line *means* is the format crate's contract and is pinned by that
//! crate's tests; what is pinned here is that this binary routes the file correctly and prints the
//! crate's words through, unabridged, without moving any verdict.
//!
//! HEIC fixtures are built by `gamut::isobmff::write` and then have hand-authored top-level boxes
//! spliced in after `ftyp`, since a C2PA `uuid` box is not part of the writer's model; the other
//! formats' fixtures come from gamut's own encoders, whose reported ranges are what the report must
//! repeat. C2PA clause references are to the 2.4 specification.

use std::process::{Command, Output};

use gamut::core::{Dimensions, EncodeImage, ImageRef, Rgb8};
use gamut::isobmff::{IsoBmffImage, Item, Property, PropertyKind, write};

/// The C2PA `ContentProvenanceBox` extended (user) type — C2PA 2.4 §A.5.1.1.
const C2PA_UUID: [u8; 16] = [
    0xD8, 0xFE, 0xC3, 0xD6, 0x1B, 0x0E, 0x48, 0x3C, 0x92, 0x97, 0x58, 0x28, 0x87, 0x7E, 0xC4, 0x81,
];

/// A coded-image item with one essential codec-configuration property.
fn coded_item(id: u32, item_type: [u8; 4], config: [u8; 4]) -> Item {
    Item {
        id,
        item_type,
        name: String::new(),
        content_type: None,
        content_encoding: None,
        hidden: false,
        references: vec![],
        properties: vec![
            Property {
                essential: true,
                kind: PropertyKind::CodecConfiguration {
                    kind: config,
                    data: vec![1, 2, 3, 4],
                },
            },
            Property {
                essential: false,
                kind: PropertyKind::ImageSpatialExtents {
                    width: 64,
                    height: 48,
                },
            },
        ],
        payload: vec![9, 9, 9, 9],
    }
}

/// A HEVC still image: major brand `heic`, one `hvc1` item carrying an `hvcC`.
fn heic_file() -> Vec<u8> {
    write(&IsoBmffImage::new(
        *b"heic",
        vec![*b"heic", *b"mif1"],
        1,
        vec![coded_item(1, *b"hvc1", *b"hvcC")],
    ))
    .expect("valid HEVC still-image model")
}

/// An AVIF carrying the generic MIAF structural brand `mif1` as its **major** brand: one `av01`
/// item with an `av1C`, and no HEVC brand anywhere. This is the file the major-brand test alone
/// cannot tell from a HEIC (`references/heif` §7 settles it on the primary item's `hvcC`).
fn mif1_avif_file() -> Vec<u8> {
    write(&IsoBmffImage::new(
        *b"mif1",
        vec![*b"mif1", *b"miaf", *b"avif"],
        1,
        vec![coded_item(1, *b"av01", *b"av1C")],
    ))
    .expect("valid AVIF-shaped model")
}

/// One complete box: 32-bit size + type + body.
fn bx(ty: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut out = (8 + body.len() as u32).to_be_bytes().to_vec();
    out.extend_from_slice(ty);
    out.extend_from_slice(body);
    out
}

/// A top-level `uuid` box with an explicit user type, `FullBox` version/flags, null-terminated
/// `box_purpose` and raw `data` (C2PA 2.4 §A.5.1.2).
fn uuid_box(user_type: &[u8; 16], version: u8, purpose: &str, data: &[u8]) -> Vec<u8> {
    let mut body = user_type.to_vec();
    body.push(version);
    body.extend_from_slice(&[0, 0, 0]); // flags
    body.extend_from_slice(purpose.as_bytes());
    body.push(0);
    body.extend_from_slice(data);
    bx(b"uuid", &body)
}

/// A JUMBF-shaped manifest store: a 4-byte big-endian `LBox` covering the whole box, the `jumb`
/// `TBox`, then opaque contents (C2PA 2.4 §8.4.2.3, §A.3.9).
fn jumbf_store(contents: &[u8]) -> Vec<u8> {
    let mut out = ((8 + contents.len()) as u32).to_be_bytes().to_vec();
    out.extend_from_slice(b"jumb");
    out.extend_from_slice(contents);
    out
}

/// Splices `boxes` in immediately after the file's `ftyp`, which C2PA 2.4 §A.5.3 is where a
/// `ContentProvenanceBox` goes. The `ftyp` length is the file's first four bytes.
fn splice_after_ftyp(file: &[u8], boxes: &[Vec<u8>]) -> Vec<u8> {
    let ftyp_len = u32::from_be_bytes(file[..4].try_into().unwrap()) as usize;
    let mut out = file[..ftyp_len].to_vec();
    for b in boxes {
        out.extend_from_slice(b);
    }
    out.extend_from_slice(&file[ftyp_len..]);
    out
}

/// Writes `bytes` to a unique temp file, runs `gamut inspect [--format <f>] <file>`, removes it,
/// and returns the output.
fn run_inspect(name: &str, bytes: &[u8], force: Option<&str>) -> Output {
    let path =
        std::env::temp_dir().join(format!("gamut-inspect-c2pa-{}-{name}", std::process::id()));
    std::fs::write(&path, bytes).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_gamut"));
    command.arg("inspect");
    if let Some(format) = force {
        command.arg("--format").arg(format);
    }
    let output = command.arg(&path).output().expect("run gamut inspect");
    let _ = std::fs::remove_file(&path);
    output
}

#[test]
fn a_located_store_reaches_the_terminal_with_the_non_validation_disclaimer() {
    // The one thing this command must never lose on the way to stdout: locating a store is not
    // validating it (C2PA 2.4 §15.12), and a store line read without that reads as *verified*.
    let file = splice_after_ftyp(
        &heic_file(),
        &[uuid_box(
            &C2PA_UUID,
            0,
            "manifest",
            &[&0u64.to_be_bytes()[..], &jumbf_store(b"opaque")].concat(),
        )],
    );
    let out = run_inspect("located.heic", &file, None);
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("HEIF/HEIC"), "{stdout}");
    assert!(
        stdout.contains("1 manifest store located, NOT VALIDATED"),
        "{stdout}"
    );
    for fragment in [
        "no signature",
        "no hash binding",
        "no trust list",
        "c2pa-rs",
    ] {
        assert!(
            stdout.contains(fragment),
            "the disclaimer must reach stdout naming {fragment}: {stdout}"
        );
    }
    // The size is the store's own JUMBF `LBox` (8-byte header + 6 bytes of contents), not the
    // enclosing box's, so the number is evidence the store line came from the store.
    assert!(
        stdout.contains("box_purpose \"manifest\": 14 bytes at"),
        "the store's own line must reach stdout too: {stdout}"
    );
}

#[test]
fn a_c2pa_box_that_yields_no_store_is_not_reported_as_a_file_without_provenance() {
    // §A.5.1.2 fixes the `FullBox` version at zero, so no store is read — but the file plainly
    // carries C2PA framing, and printing the wording a file with no C2PA box gets would let a
    // reader infer absence of provenance from bytes gamut merely could not read through.
    let file = splice_after_ftyp(
        &heic_file(),
        &[uuid_box(
            &C2PA_UUID,
            1,
            "manifest",
            &[&0u64.to_be_bytes()[..], &jumbf_store(b"opaque")].concat(),
        )],
    );
    let out = run_inspect("unread.heic", &file, None);
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(
        out.status.success(),
        "an unreadable box is a finding, not an inspection failure; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !stdout.contains("no manifest store found"),
        "the absence wording must not be used for a box that is present: {stdout}"
    );
    assert!(stdout.contains("NOT absence of provenance"), "{stdout}");
    assert!(stdout.contains("unread C2PA box at"), "{stdout}");
}

#[test]
fn an_avif_whose_major_brand_is_mif1_is_reported_as_an_avif_not_a_heic() {
    // `mif1` is the generic MIAF structural brand, so a major-brand test alone accepts this file
    // and would report an AVIF's C2PA box as a HEIC's. The confirmations are `gamut-heic`'s and
    // `gamut-avif`'s own still-image predicates, applied after the parse: the HEIC one declines the
    // file and the AVIF one claims it.
    let file = splice_after_ftyp(
        &mif1_avif_file(),
        &[uuid_box(
            &C2PA_UUID,
            0,
            "manifest",
            &[&0u64.to_be_bytes()[..], &jumbf_store(b"opaque")].concat(),
        )],
    );
    let out = run_inspect("mif1.avif", &file, None);
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains(": AVIF\n"), "{stdout}");
    assert!(!stdout.contains("HEIF/HEIC"), "{stdout}");
    // AVIF reports the store after the 8-byte merkle offset, trimmed to its `LBox`: 14 bytes.
    assert!(
        stdout.contains("box_purpose \"manifest\": 14 bytes at"),
        "{stdout}"
    );
}

#[test]
fn an_isobmff_still_image_neither_crate_confirms_is_an_unsupported_container() {
    // A `mif1` file whose primary item is neither HEVC nor AV1: both predicates decline it, so the
    // command names the brand rather than reporting anyone's provenance for it.
    let file = write(&IsoBmffImage::new(
        *b"mif1",
        vec![*b"mif1", *b"miaf"],
        1,
        vec![coded_item(1, *b"j2k1", *b"j2kH")],
    ))
    .expect("valid MIAF-shaped model");
    let out = run_inspect("other.heif", &file, None);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(!out.status.success(), "stdout: {stdout}");
    assert!(
        stderr.contains("unsupported container brand 'mif1'"),
        "the message must name the brand it declined: {stderr}"
    );
    assert!(
        !stdout.contains("C2PA"),
        "nothing about the file's provenance may be printed: {stdout}"
    );
}

#[test]
fn an_isobmff_movie_is_refused_by_the_heic_parse_not_declined_by_brand() {
    // The `ftyp` sniff routes every ISOBMFF file into the HEIC arm, and the brand confirmation runs
    // only after `HeifContainer::parse` succeeds. A file with a top-level `moov` and no `meta` — an
    // MP4, a CR3, a sequence-only HEIF — carries no still item, so that parse refuses it first (a
    // `moov` beside a `meta` would be skipped instead): it exits 1 with the parse error rather than
    // the unsupported-brand message, and prints no report.
    let mut ftyp_body = b"isom".to_vec();
    ftyp_body.extend_from_slice(&0u32.to_be_bytes());
    ftyp_body.extend_from_slice(b"isommp41");
    let file = [
        bx(b"ftyp", &ftyp_body),
        bx(b"moov", &[0xAA; 16]),
        bx(b"mdat", &[0; 8]),
    ]
    .concat();
    let out = run_inspect("movie.mp4", &file, None);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert_eq!(
        out.status.code(),
        Some(1),
        "stdout: {stdout}; stderr: {stderr}"
    );
    assert!(
        stderr.contains("sequence-only file"),
        "the HEIC container parse must be what refuses the file: {stderr}"
    );
    assert!(
        !stderr.contains("unsupported container brand"),
        "the brand confirmation is never reached for a file the parse refuses: {stderr}"
    );
    assert!(stdout.is_empty(), "no report may be printed: {stdout}");
}

#[test]
fn forcing_the_heic_format_skips_the_confirmation_the_sniff_applies() {
    // `--format` overrides detection by definition, so it overrides the confirmation too: the same
    // AVIF the sniff declines is read as a HEIC when the caller asserts it is one.
    let out = run_inspect("forced.avif", &mif1_avif_file(), Some("heic"));
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("HEIF/HEIC"), "{stdout}");
}

/// The most entries `gamut inspect` prints per list before truncating (`MAX_LIST` in
/// `commands/inspect.rs`, which is private to the binary).
const MAX_LIST: usize = 20;

#[test]
fn the_c2pa_box_list_is_truncated_like_every_other_list_in_the_command() {
    // C2PA 2.4 §A.5.3 permits any number of these boxes, so their count is chosen by the input and
    // needs no malformity: uncapped, a legal file puts the headline — non-validation disclaimer and
    // all — at line 2 of however many the file cares to carry.
    let boxes: Vec<Vec<u8>> = (0..MAX_LIST + 2)
        .map(|_| {
            uuid_box(
                &C2PA_UUID,
                1,
                "manifest",
                &[&0u64.to_be_bytes()[..], &jumbf_store(b"opaque")].concat(),
            )
        })
        .collect();
    let file = splice_after_ftyp(&heic_file(), &boxes);
    let out = run_inspect("many.heic", &file, None);
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        stdout.matches("unread C2PA box at").count(),
        MAX_LIST,
        "{stdout}"
    );
    assert!(stdout.contains("… and 2 more"), "{stdout}");
    // The headline is still the second line of the report, where a reader meets it first.
    let lines: Vec<&str> = stdout.lines().collect();
    assert!(lines[1].contains("NOT absence of provenance"), "{stdout}");
}

#[test]
fn a_uuid_box_of_another_extended_type_reaches_stdout_as_a_count() {
    // A file whose only `uuid` box is a single byte off the C2PA type — what a signed file
    // corrupted in transit looks like — printed byte-for-byte what a file with no such box prints.
    // The count is a fact about bytes and says so; it must not read as C2PA framing.
    let mut foreign = C2PA_UUID;
    foreign[0] ^= 0xFF;
    let file = splice_after_ftyp(
        &heic_file(),
        &[uuid_box(
            &foreign,
            0,
            "manifest",
            &[&0u64.to_be_bytes()[..], &jumbf_store(b"opaque")].concat(),
        )],
    );
    let out = run_inspect("near-miss.heic", &file, None);
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.contains("top-level uuid boxes of another extended type: 1"),
        "the near miss must be visible at all: {stdout}"
    );
    assert!(
        stdout.contains("not provenance framing"),
        "and must disclaim provenance in the same line: {stdout}"
    );
    // It is still not a C2PA box: no store, and no unread-box line claiming damaged framing.
    assert!(!stdout.contains("unread C2PA box"), "{stdout}");
}

#[test]
fn the_label_says_whether_the_format_was_detected_or_asserted() {
    // `--format` skips the sniff and the confirmation, so on that path the command has tested
    // nothing about the container: printing the label it prints for a file it confirmed would be
    // echoing the caller's assertion back as this command's own finding.
    let sniffed = run_inspect("detected.heic", &heic_file(), None);
    let sniffed_stdout = String::from_utf8_lossy(&sniffed.stdout);
    assert!(sniffed.status.success(), "{sniffed_stdout}");
    assert!(
        sniffed_stdout.contains(": HEIF/HEIC\n"),
        "a confirmed container is labelled plainly: {sniffed_stdout}"
    );

    let forced = run_inspect("asserted.avif", &mif1_avif_file(), Some("heic"));
    let forced_stdout = String::from_utf8_lossy(&forced.stdout);
    assert!(forced.status.success(), "{forced_stdout}");
    assert!(
        forced_stdout.contains("HEIF/HEIC (asserted by --format, not detected)"),
        "a forced container must say the format was asserted: {forced_stdout}"
    );
}

#[test]
fn the_headline_names_a_class_of_box_the_cap_hides_entirely() {
    // The shape the cap can swallow whole: enough legal stores to fill the list, and one C2PA box
    // no store could be read from sitting behind them. Twenty store-shaped boxes need no
    // malformity — §A.5.3 permits any number — so this is a file anyone can build, and the report
    // it used to get was a clean bill of health: twenty stores, exit 0, not one word about the box
    // gamut could not read through. The headline states both classes, which is what a truncated
    // list cannot take away.
    let mut boxes: Vec<Vec<u8>> = (0..MAX_LIST)
        .map(|_| {
            uuid_box(
                &C2PA_UUID,
                0,
                "manifest",
                &[&0u64.to_be_bytes()[..], &jumbf_store(b"opaque")].concat(),
            )
        })
        .collect();
    // §A.5.1.2 fixes the `FullBox` version at zero, so this last box yields no store.
    boxes.push(uuid_box(
        &C2PA_UUID,
        1,
        "manifest",
        &[&0u64.to_be_bytes()[..], &jumbf_store(b"opaque")].concat(),
    ));
    let file = splice_after_ftyp(&heic_file(), &boxes);
    let out = run_inspect("hidden-class.heic", &file, None);
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // The cap really does hide it: the box is last in file order, so no line of its own survives.
    assert_eq!(stdout.matches("unread C2PA box at").count(), 0, "{stdout}");
    assert!(stdout.contains("… and 1 more"), "{stdout}");
    // And the headline still says the class exists, on the line a reader meets first.
    let lines: Vec<&str> = stdout.lines().collect();
    assert!(lines[1].contains("20 manifest stores located"), "{stdout}");
    assert!(
        lines[1].contains("1 C2PA box is present from which no store could be read"),
        "the headline must name the class the cap hid: {stdout}"
    );
    assert!(
        lines[1].contains("NOT absence of provenance"),
        "and must not let it be read as absence: {stdout}"
    );
}

#[test]
fn the_capped_list_is_the_files_first_boxes_and_not_its_first_stores() {
    // File order, end to end. An unreadable box ahead of the stores gets its line where the file
    // puts it, so the cut is category-blind: it hides the file's last boxes, whatever kind they
    // are, instead of systematically favouring one kind.
    let mut boxes: Vec<Vec<u8>> = vec![uuid_box(
        &C2PA_UUID,
        1,
        "manifest",
        &[&0u64.to_be_bytes()[..], &jumbf_store(b"opaque")].concat(),
    )];
    boxes.extend((0..MAX_LIST).map(|_| {
        uuid_box(
            &C2PA_UUID,
            0,
            "manifest",
            &[&0u64.to_be_bytes()[..], &jumbf_store(b"opaque")].concat(),
        )
    }));
    let file = splice_after_ftyp(&heic_file(), &boxes);
    let out = run_inspect("file-order.heic", &file, None);
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let details: Vec<&str> = stdout
        .lines()
        .filter(|line| line.contains("unread C2PA box at") || line.contains("box_purpose"))
        .collect();
    assert_eq!(details.len(), MAX_LIST, "{stdout}");
    assert!(
        details[0].contains("unread C2PA box at"),
        "the file's first box is the report's first entry: {stdout}"
    );
    // One store is past the cut, so the tail counts it and no kind was hidden as a kind.
    assert!(stdout.contains("… and 1 more"), "{stdout}");
}

/// An 8×8 RGB image, the same for every encoder below.
fn rgb_image(rgb: &[u8]) -> ImageRef<'_, Rgb8> {
    ImageRef::new(rgb, Dimensions::new(8, 8).expect("dimensions")).expect("buffer matches")
}

/// Asserts the four fragments of the non-validation disclaimer reached `stdout`.
fn assert_disclaimed(stdout: &str) {
    for fragment in [
        "no signature",
        "no hash binding",
        "no trust list",
        "c2pa-rs",
    ] {
        assert!(stdout.contains(fragment), "{fragment}: {stdout}");
    }
}

#[test]
fn an_avif_the_encoder_signed_reports_its_store_at_the_encoders_range() {
    // Sniffed through `ftyp`, confirmed by `gamut-avif`'s predicate, and reported from its summary:
    // the store fills the slot the encoder reported, and its bytes are never printed.
    let rgb = [0x40u8; 8 * 8 * 3];
    let store = jumbf_store(b"STORE-BYTES-NEVER-PRINTED");
    let (file, report) = gamut::avif::AvifEncoder::new()
        .with_c2pa(&store)
        .encode_with_report(rgb_image(&rgb))
        .expect("encode");
    let range = report.c2pa.expect("a slot was written");
    let out = run_inspect("signed.avif", &file, None);
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let lines: Vec<&str> = stdout.lines().collect();
    assert!(lines[0].ends_with(": AVIF"), "{stdout}");
    assert!(
        lines[1].contains("1 manifest store located, NOT VALIDATED"),
        "{stdout}"
    );
    assert_disclaimed(&stdout);
    assert!(
        stdout.contains(&format!(
            "box_purpose \"manifest\": {} bytes at [{}, {})",
            store.len(),
            range.start,
            range.end
        )),
        "{stdout}"
    );
    assert!(!stdout.contains("STORE-BYTES"), "{stdout}");

    // Without a store, absence is reported — at exit 0, with the disclaimer still inline.
    let plain = gamut::avif::AvifEncoder::new()
        .encode_to_vec(rgb_image(&rgb))
        .expect("encode");
    let out = run_inspect("plain.avif", &plain, None);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{stdout}");
    assert!(stdout.contains("no manifest store found"), "{stdout}");
    assert_disclaimed(&stdout);
}

#[test]
fn forcing_the_avif_format_says_the_format_was_asserted() {
    let out = run_inspect("forced.heic", &mif1_avif_file(), Some("avif"));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.contains(": AVIF (asserted by --format, not detected)\n"),
        "{stdout}"
    );
}

#[test]
fn a_webp_reports_its_c2pa_chunk_store_and_exits_zero() {
    let rgb = [0x40u8; 8 * 8 * 3];
    let store = b"STORE-BYTES-NEVER-PRINTED";
    let mut file = Vec::new();
    gamut::webp::WebpEncoder::lossless()
        .with_c2pa(store)
        .encode_image(rgb_image(&rgb), &mut file)
        .expect("encode");
    let span = gamut::webp::c2pa_span(&file)
        .expect("walk")
        .expect("a chunk");
    let out = run_inspect("signed.webp", &file, None);
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let lines: Vec<&str> = stdout.lines().collect();
    assert!(lines[0].ends_with(": WebP"), "{stdout}");
    assert!(
        lines[1].contains("1 manifest store located, NOT VALIDATED"),
        "{stdout}"
    );
    assert_disclaimed(&stdout);
    // The store is the chunk's payload: its span less the 8-byte chunk header.
    assert!(
        stdout.contains(&format!(
            "C2PA chunk: {}-byte store at [{}, {}) — located, not validated\n",
            store.len(),
            span.start + 8,
            span.end
        )),
        "{stdout}"
    );
    assert!(!stdout.contains("STORE-BYTES"), "{stdout}");
}

#[test]
fn a_file_that_is_not_a_webp_is_refused_by_the_riff_walk_when_forced() {
    let out = run_inspect("not.webp", &heic_file(), Some("webp"));
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty(), "no report may be printed");
}

#[test]
fn a_tiff_reports_its_store_beside_the_accounting_without_moving_the_verdict() {
    let rgb = [0x40u8; 8 * 8 * 3];
    let encode = |metadata: gamut::tiff::TiffMetadata| {
        let mut file = Vec::new();
        let report = gamut::tiff::TiffEncoder::new()
            .with_metadata(metadata)
            .encode_with_report(rgb_image(&rgb), &mut file)
            .expect("encode");
        (file, report)
    };
    let (plain, _) = encode(gamut::tiff::TiffMetadata::new());
    let (signed, report) =
        encode(gamut::tiff::TiffMetadata::new().with_c2pa(b"STORE-BYTES-NEVER-PRINTED".to_vec()));
    let store = report.c2pa.expect("a store was written").store();

    let plain_out = run_inspect("plain.tif", &plain, None);
    let out = run_inspect("signed.tif", &signed, None);
    let stdout = String::from_utf8_lossy(&out.stdout);

    // Additive: the store changes no verdict — the signed file exits exactly as the plain one.
    assert_eq!(out.status.code(), plain_out.status.code(), "{stdout}");
    assert!(out.status.success(), "{stdout}");
    assert!(stdout.contains("fully accounted:  yes"), "{stdout}");
    assert!(
        stdout.contains("C2PA: 1 manifest store located, NOT VALIDATED"),
        "{stdout}"
    );
    assert_disclaimed(&stdout);
    assert!(
        stdout.contains(&format!(
            "tag 52545 in IFD 0: 25-byte store at [{}, {}) — located, not validated",
            store.start,
            store.end()
        )),
        "{stdout}"
    );
    assert!(!stdout.contains("STORE-BYTES"), "{stdout}");
    assert!(
        String::from_utf8_lossy(&plain_out.stdout)
            .contains("C2PA: no manifest store found in the main IFD chain"),
        "{}",
        String::from_utf8_lossy(&plain_out.stdout)
    );
}

#[test]
fn a_tiff_whose_store_entry_is_mistyped_is_not_reported_as_without_provenance() {
    // §A.3.6 fixes the entry's type at UNDEFINED; a BYTE-typed one yields no store, but the entry
    // is there, and the absence wording must not be printed for it.
    let mut ifd = gamut::tiff::Ifd::new();
    ifd.set(256, gamut::tiff::Value::Short(vec![1]));
    ifd.set(52545, gamut::tiff::Value::Byte(vec![0x10; 12]));
    let file = gamut::tiff::write(&gamut::tiff::TiffFile {
        order: gamut::tiff::ByteOrder::LittleEndian,
        variant: gamut::tiff::Variant::Classic,
        ifds: vec![ifd],
    })
    .expect("write");
    let out = run_inspect("mistyped.tif", &file, Some("tiff"));
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(!stdout.contains("no manifest store found"), "{stdout}");
    assert!(stdout.contains("NOT absence of provenance"), "{stdout}");
    assert!(
        // Entry 1 of the directory at 8, after tag 256's: 8 + 2 + 12.
        stdout.contains("unread tag 52545 entry in IFD 0 at [22, 34): its field type is not"),
        "{stdout}"
    );
}

#[test]
fn a_dng_reports_its_reserved_store_beside_the_accounting() {
    use gamut::dng::{CalibrationIlluminant, CameraProfile, DngEncoder, RawImage, cfa_color};

    let samples: Vec<u16> = (0..16u16 * 16).map(|i| (i * 37) % 4096).collect();
    let raw = RawImage::new_cfa(
        Dimensions::new(16, 16).expect("dimensions"),
        16,
        (2, 2),
        vec![
            cfa_color::RED,
            cfa_color::GREEN,
            cfa_color::GREEN,
            cfa_color::BLUE,
        ],
        samples,
    )
    .expect("raw")
    .with_black_level(0.0)
    .expect("black level")
    .with_white_level(4095.0)
    .expect("white level");
    let profile = CameraProfile::new(
        "gamut TestCam",
        [
            0.6722, -0.0635, -0.0963, -0.4287, 1.2460, 0.2028, -0.0908, 0.2162, 0.5668,
        ],
        CalibrationIlluminant::D65,
        [0.5128, 1.0, 0.7059],
    )
    .expect("profile");
    let mut file = Vec::new();
    let report = DngEncoder::new()
        .with_c2pa_reserved(64)
        .encode_with_report(&raw, &profile, &mut file)
        .expect("encode");
    let store = report.c2pa.expect("a reservation was written").store();
    let out = run_inspect("signed.dng", &file, None);
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(stdout.starts_with("inspecting "), "{stdout}");
    assert!(stdout.contains(" as DNG\n"), "{stdout}");
    assert!(
        stdout.contains("C2PA: 1 manifest store located, NOT VALIDATED"),
        "{stdout}"
    );
    assert_disclaimed(&stdout);
    assert!(
        stdout.contains(&format!(
            "tag 52545 in IFD 0: 64-byte store at [{}, {})",
            store.start,
            store.end()
        )),
        "{stdout}"
    );
}

/// A PNG the encoder signed with `store`, and the `caBX` chunk span it reported.
fn signed_png(store: &[u8]) -> (Vec<u8>, gamut::png::C2paSpan) {
    let rgb = [0x40u8; 8 * 8 * 3];
    let (file, report) = gamut::png::PngEncoder::new()
        .with_c2pa(store)
        .encode_with_report(rgb_image(&rgb))
        .expect("encode");
    (file, report.c2pa.expect("a store was written"))
}

#[test]
fn a_png_reports_its_store_beside_the_accounting_without_moving_the_verdict() {
    let rgb = [0x40u8; 8 * 8 * 3];
    let plain = gamut::png::PngEncoder::new()
        .encode_to_vec(rgb_image(&rgb))
        .expect("encode");
    let (signed, span) = signed_png(b"STORE-BYTES-NEVER-PRINTED");

    let plain_out = run_inspect("plain.png", &plain, None);
    let out = run_inspect("signed.png", &signed, None);
    let stdout = String::from_utf8_lossy(&out.stdout);

    // Additive: the store changes no verdict — the signed file exits exactly as the plain one.
    assert_eq!(out.status.code(), plain_out.status.code(), "{stdout}");
    assert!(out.status.success(), "{stdout}");
    assert!(stdout.contains("verified:      yes"), "{stdout}");
    assert!(
        stdout.contains("C2PA: 1 manifest store located, NOT VALIDATED"),
        "{stdout}"
    );
    assert_disclaimed(&stdout);
    assert!(
        stdout.contains(&format!(
            "caBX chunk at [{}, {}): 25-byte store at [{}, {}) — located, not validated\n",
            span.chunk.start, span.chunk.end, span.payload.start, span.payload.end
        )),
        "{stdout}"
    );
    assert!(!stdout.contains("STORE-BYTES"), "{stdout}");
    assert!(
        String::from_utf8_lossy(&plain_out.stdout).contains("C2PA: no caBX chunk found"),
        "{}",
        String::from_utf8_lossy(&plain_out.stdout)
    );
}

#[test]
fn a_cabx_appended_after_iend_is_listed_as_unread_not_as_absence() {
    // The appended chunk is a real, CRC-valid caBX — the one the encoder framed — copied past
    // IEND, where it is not the store but is C2PA framing all the same.
    let rgb = [0x40u8; 8 * 8 * 3];
    let plain = gamut::png::PngEncoder::new()
        .encode_to_vec(rgb_image(&rgb))
        .expect("encode");
    let (signed, span) = signed_png(b"STORE-BYTES-NEVER-PRINTED");
    let chunk = &signed[span.chunk.clone()];
    let mut appended = plain.clone();
    appended.extend_from_slice(chunk);
    // The same bytes under another chunk type: the trailer finding alone, for the verdict.
    let mut renamed = plain.clone();
    renamed.extend_from_slice(chunk);
    renamed[plain.len() + 4..plain.len() + 8].copy_from_slice(b"teXt");

    let out = run_inspect("appended.png", &appended, None);
    let baseline = run_inspect("renamed.png", &renamed, None);
    let stdout = String::from_utf8_lossy(&out.stdout);

    // The trailer is a finding either way; the caBX chunk in it moves nothing.
    assert_eq!(out.status.code(), Some(1), "{stdout}");
    assert_eq!(out.status.code(), baseline.status.code(), "{stdout}");
    assert!(!stdout.contains("no caBX chunk found"), "{stdout}");
    assert!(
        stdout.contains(
            "C2PA: no manifest store could be read, but 1 caBX chunk is present — that is NOT \
             absence of provenance"
        ),
        "{stdout}"
    );
    assert!(
        stdout.contains(&format!(
            "unread caBX chunk at [{}, {}): 25-byte payload — it follows IEND",
            plain.len(),
            plain.len() + chunk.len()
        )),
        "{stdout}"
    );
    assert!(
        String::from_utf8_lossy(&baseline.stdout).contains("C2PA: no caBX chunk found"),
        "{}",
        String::from_utf8_lossy(&baseline.stdout)
    );
}

// `--output json` (#596): the same facts as data. Each test reads the document back as JSON and
// compares it with what the format crate itself reports for the file, so what is pinned is that the
// command carries the crate's summary through whole, not what the summary says.

/// Runs `gamut inspect --output json [--format <f>] <file>` and parses stdout as one JSON document.
fn run_inspect_json(name: &str, bytes: &[u8], force: Option<&str>) -> (Output, serde_json::Value) {
    let path =
        std::env::temp_dir().join(format!("gamut-inspect-json-{}-{name}", std::process::id()));
    std::fs::write(&path, bytes).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_gamut"));
    command.arg("inspect").arg("--output").arg("json");
    if let Some(format) = force {
        command.arg("--format").arg(format);
    }
    let output = command.arg(&path).output().expect("run gamut inspect");
    let _ = std::fs::remove_file(&path);
    let doc = serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
        panic!(
            "stdout is not one JSON document ({e}): {}",
            String::from_utf8_lossy(&output.stdout)
        )
    });
    (output, doc)
}

/// A JSON `{"start", "end"}` range object for a half-open range.
fn json_range(start: u64, end: u64) -> serde_json::Value {
    serde_json::json!({ "start": start, "end": end })
}

#[test]
fn json_output_lists_every_heic_c2pa_box_past_the_text_cap() {
    // The cap is a terminal-presentation decision: the document carries every box the crate
    // reports, in file order, with the unread box the text report hides behind its tail.
    let mut boxes: Vec<Vec<u8>> = (0..=MAX_LIST)
        .map(|_| {
            uuid_box(
                &C2PA_UUID,
                0,
                "manifest",
                &[&0u64.to_be_bytes()[..], &jumbf_store(b"opaque")].concat(),
            )
        })
        .collect();
    boxes.push(uuid_box(
        &C2PA_UUID,
        1,
        "manifest",
        &[&0u64.to_be_bytes()[..], &jumbf_store(b"opaque")].concat(),
    ));
    let mut foreign = C2PA_UUID;
    foreign[0] ^= 0xFF;
    boxes.push(uuid_box(&foreign, 0, "manifest", &jumbf_store(b"opaque")));
    let file = splice_after_ftyp(&heic_file(), &boxes);
    let summary = gamut::heic::HeifContainer::parse(&file)
        .expect("parse")
        .c2pa_summary();
    let (out, doc) = run_inspect_json("uncapped.heic", &file, None);

    assert!(out.status.success(), "{doc}");
    assert_eq!(doc["format"], "heic");
    assert_eq!(doc["format_source"], "detected");
    let stores = doc["c2pa"]["stores"].as_array().expect("stores");
    assert_eq!(stores.len(), MAX_LIST + 1, "{doc}");
    for (json, store) in stores.iter().zip(&summary.stores) {
        assert_eq!(json["purpose"], "manifest");
        assert_eq!(json["extent"], "store");
        assert_eq!(json["size"], store.size());
        assert_eq!(
            json["range"],
            json_range(store.range.start as u64, store.range.end as u64)
        );
        assert_eq!(
            json["slot"],
            json_range(store.slot.start as u64, store.slot.end as u64)
        );
        assert_eq!(json["position"], "before_media_data");
    }
    let unread = &doc["c2pa"]["unread"];
    assert_eq!(unread.as_array().map(Vec::len), Some(1), "{doc}");
    assert_eq!(unread[0]["reason"], "not_version_zero");
    assert_eq!(
        unread[0]["range"],
        json_range(
            summary.unread[0].range.start as u64,
            summary.unread[0].range.end as u64
        )
    );
    assert_eq!(doc["c2pa"]["merkle"], serde_json::json!([]));
    assert_eq!(doc["c2pa"]["other_uuid_boxes"], 1);
}

#[test]
fn json_output_carries_the_non_validation_statement_and_never_the_store_bytes() {
    let file = splice_after_ftyp(
        &heic_file(),
        &[uuid_box(
            &C2PA_UUID,
            0,
            "manifest",
            &[
                &0u64.to_be_bytes()[..],
                &jumbf_store(b"STORE-BYTES-NEVER-PRINTED"),
            ]
            .concat(),
        )],
    );
    let (out, doc) = run_inspect_json("disclaimed.heic", &file, None);

    assert!(out.status.success(), "{doc}");
    assert_eq!(
        doc["c2pa"]["not_validated"],
        gamut::heic::C2PA_NOT_VALIDATED
    );
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains("STORE-BYTES"),
        "{doc}"
    );
}

#[test]
fn json_output_says_a_forced_format_was_asserted() {
    let (out, doc) = run_inspect_json("forced-json.heic", &heic_file(), Some("heic"));
    assert!(out.status.success(), "{doc}");
    assert_eq!(doc["format_source"], "asserted");
}

#[test]
fn json_output_reports_an_avif_store_and_its_slot_at_the_encoders_range() {
    // 4 bytes of padding after the store: `range` stops at the `LBox`, `slot` is the encoder's.
    let rgb = [0x40u8; 8 * 8 * 3];
    let store = jumbf_store(b"STORE-BYTES-NEVER-PRINTED");
    let padded = [&store[..], &[0xEE; 4]].concat();
    let (file, report) = gamut::avif::AvifEncoder::new()
        .with_c2pa(&padded)
        .encode_with_report(rgb_image(&rgb))
        .expect("encode");
    let slot = report.c2pa.expect("a slot was written");
    let (out, doc) = run_inspect_json("signed-json.avif", &file, None);

    assert!(out.status.success(), "{doc}");
    assert_eq!(doc["format"], "avif");
    let stores = &doc["c2pa"]["stores"];
    assert_eq!(stores.as_array().map(Vec::len), Some(1), "{doc}");
    assert_eq!(stores[0]["extent"], "store");
    assert_eq!(stores[0]["size"], store.len());
    assert_eq!(
        stores[0]["range"],
        json_range(slot.start as u64, (slot.start + store.len()) as u64)
    );
    assert_eq!(
        stores[0]["slot"],
        json_range(slot.start as u64, slot.end as u64)
    );
    assert_eq!(
        doc["c2pa"]["not_validated"],
        gamut::avif::C2PA_NOT_VALIDATED
    );
}

#[test]
fn json_output_lists_an_avifs_unread_and_merkle_boxes_after_its_media_data() {
    // Appended after the encoder's `mdat`, so no offset the file's `iloc` holds moves: a C2PA box
    // whose `FullBox` version is not zero (§A.5.1.2), then an auxiliary `merkle` box (§A.5.4.1.4).
    let rgb = [0x40u8; 8 * 8 * 3];
    let mut file = gamut::avif::AvifEncoder::new()
        .encode_to_vec(rgb_image(&rgb))
        .expect("encode");
    file.extend_from_slice(&uuid_box(
        &C2PA_UUID,
        1,
        "manifest",
        &[&0u64.to_be_bytes()[..], &jumbf_store(b"opaque")].concat(),
    ));
    file.extend_from_slice(&uuid_box(&C2PA_UUID, 0, "merkle", b"hashes"));
    let summary = gamut::avif::AvifContainer::parse(&file)
        .expect("parse")
        .c2pa_summary();
    let (out, doc) = run_inspect_json("unread-merkle.avif", &file, None);

    assert!(out.status.success(), "{doc}");
    assert_eq!(doc["c2pa"]["stores"], serde_json::json!([]), "{doc}");
    let unread = &doc["c2pa"]["unread"];
    assert_eq!(unread.as_array().map(Vec::len), Some(1), "{doc}");
    assert_eq!(
        unread[0]["range"],
        json_range(
            summary.unread[0].range.start as u64,
            summary.unread[0].range.end as u64
        )
    );
    assert_eq!(unread[0]["reason"], "not_version_zero");
    assert_eq!(
        unread[0]["description"],
        summary.unread[0].reason.describe()
    );
    assert_eq!(unread[0]["position"], "after_media_data");
    let merkle = &doc["c2pa"]["merkle"];
    assert_eq!(merkle.as_array().map(Vec::len), Some(1), "{doc}");
    assert_eq!(
        merkle[0]["range"],
        json_range(
            summary.merkle[0].range.start as u64,
            summary.merkle[0].range.end as u64
        )
    );
    assert_eq!(merkle[0]["position"], "after_media_data");
}

#[test]
fn json_output_lists_an_unfilled_reserved_avif_slot_as_bounding_no_store() {
    // `with_c2pa_reserved` writes a zero-filled slot: a zero `LBox` bounds no JUMBF store, so the
    // box is unread with `no_store_bound` rather than reported as a store.
    let rgb = [0x40u8; 8 * 8 * 3];
    let file = gamut::avif::AvifEncoder::new()
        .with_c2pa_reserved(64)
        .encode_to_vec(rgb_image(&rgb))
        .expect("encode");
    let summary = gamut::avif::AvifContainer::parse(&file)
        .expect("parse")
        .c2pa_summary();
    let (out, doc) = run_inspect_json("reserved.avif", &file, None);

    assert!(out.status.success(), "{doc}");
    assert_eq!(doc["c2pa"]["stores"], serde_json::json!([]), "{doc}");
    let unread = &doc["c2pa"]["unread"];
    assert_eq!(unread.as_array().map(Vec::len), Some(1), "{doc}");
    assert_eq!(unread[0]["reason"], "no_store_bound");
    assert_eq!(
        unread[0]["description"],
        summary.unread[0].reason.describe()
    );
}

#[test]
fn json_output_lists_a_webp_chunk_too_short_for_a_store_as_unread() {
    // A `C2PA` chunk whose 4-byte payload cannot hold a JUMBF box header, inserted ahead of the
    // encoder's own `C2PA` chunk, with the RIFF size patched to cover it.
    let rgb = [0x40u8; 8 * 8 * 3];
    let mut file = Vec::new();
    gamut::webp::WebpEncoder::lossless()
        .with_c2pa(b"STORE-BYTES-NEVER-PRINTED")
        .encode_image(rgb_image(&rgb), &mut file)
        .expect("encode");
    let at = gamut::webp::c2pa_span(&file)
        .expect("walk")
        .expect("a chunk")
        .start;
    let short = [&b"C2PA"[..], &4u32.to_le_bytes(), b"abcd"].concat();
    file.splice(at..at, short);
    let riff_size = u32::try_from(file.len() - 8).unwrap();
    file[4..8].copy_from_slice(&riff_size.to_le_bytes());
    let summary = gamut::webp::c2pa_summary(&file).expect("walk");
    let (out, doc) = run_inspect_json("short-chunk.webp", &file, None);

    assert!(out.status.success(), "{doc}");
    assert_eq!(
        doc["c2pa"]["unread"],
        serde_json::json!([{
            "range": json_range(at as u64 + 8, at as u64 + 12),
            "position": "before_later_chunks",
        }])
    );
    assert_eq!(summary.unread.len(), 1);
    assert_eq!(summary.unread[0].range, at + 8..at + 12);
    assert_eq!(doc["c2pa"]["stores"][0]["position"], "last", "{doc}");
}

#[test]
fn json_output_reports_a_webp_store_as_the_chunk_payload() {
    let rgb = [0x40u8; 8 * 8 * 3];
    let mut file = Vec::new();
    gamut::webp::WebpEncoder::lossless()
        .with_c2pa(b"STORE-BYTES-NEVER-PRINTED")
        .encode_image(rgb_image(&rgb), &mut file)
        .expect("encode");
    let span = gamut::webp::c2pa_span(&file)
        .expect("walk")
        .expect("a chunk");
    let (out, doc) = run_inspect_json("signed-json.webp", &file, None);

    assert!(out.status.success(), "{doc}");
    assert_eq!(doc["format"], "webp");
    let stores = &doc["c2pa"]["stores"];
    assert_eq!(stores.as_array().map(Vec::len), Some(1), "{doc}");
    assert_eq!(
        stores[0]["range"],
        json_range(span.start as u64 + 8, span.end as u64)
    );
    assert_eq!(stores[0]["position"], "last");
    assert_eq!(doc["c2pa"]["unread"], serde_json::json!([]));
    assert_eq!(
        doc["c2pa"]["not_validated"],
        gamut::webp::C2PA_NOT_VALIDATED
    );
}

#[test]
fn json_output_reports_a_tiff_store_beside_its_accounting() {
    let rgb = [0x40u8; 8 * 8 * 3];
    let mut file = Vec::new();
    let report = gamut::tiff::TiffEncoder::new()
        .with_metadata(
            gamut::tiff::TiffMetadata::new().with_c2pa(b"STORE-BYTES-NEVER-PRINTED".to_vec()),
        )
        .encode_with_report(rgb_image(&rgb), &mut file)
        .expect("encode");
    let exclusions = report.c2pa.expect("a store was written");
    let (store, count) = (exclusions.store(), exclusions.count_field());
    let (out, doc) = run_inspect_json("signed-json.tif", &file, None);

    assert!(out.status.success(), "{doc}");
    assert_eq!(doc["format"], "tiff");
    assert_eq!(doc["fully_accounted"], true);
    assert_eq!(doc["fully_classified"], true);
    assert_eq!(doc["accounting"]["file_len"], file.len());
    assert_eq!(doc["accounting"]["unclassified_bytes"], 0);
    let json = &doc["c2pa"]["store"];
    assert_eq!(json["size"], 25);
    assert_eq!(json["range"], json_range(store.start, store.end()));
    assert_eq!(json["count_field"], json_range(count.start, count.end()));
    assert_eq!(json["ifd"], 0);
    assert_eq!(
        doc["c2pa"]["not_validated"],
        gamut::tiff::C2PA_NOT_VALIDATED
    );
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains("STORE-BYTES"),
        "{doc}"
    );
}

#[test]
fn json_output_lists_a_mistyped_tiff_store_entry_as_unread_with_its_reason() {
    let mut ifd = gamut::tiff::Ifd::new();
    ifd.set(256, gamut::tiff::Value::Short(vec![1]));
    ifd.set(52545, gamut::tiff::Value::Byte(vec![0x10; 12]));
    let file = gamut::tiff::write(&gamut::tiff::TiffFile {
        order: gamut::tiff::ByteOrder::LittleEndian,
        variant: gamut::tiff::Variant::Classic,
        ifds: vec![ifd],
    })
    .expect("write");
    let (_, doc) = run_inspect_json("mistyped-json.tif", &file, Some("tiff"));

    assert_eq!(doc["c2pa"]["store"], serde_json::Value::Null, "{doc}");
    let unread = &doc["c2pa"]["unread"];
    assert_eq!(unread.as_array().map(Vec::len), Some(1), "{doc}");
    // Entry 1 of the directory at 8, after tag 256's: 8 + 2 + 12.
    assert_eq!(unread[0]["entry"], json_range(22, 34));
    assert_eq!(unread[0]["ifd"], 0);
    assert_eq!(unread[0]["reason"], "not_undefined");
}

#[test]
fn json_output_reports_a_png_store_and_every_other_cabx_with_its_reason() {
    // Copies of the encoder's own CRC-valid caBX chunk, spliced in one per reason: a second one
    // right behind the store, a damaged one behind that, one between the IDATs and IEND, and one
    // after IEND.
    let (signed, span) = signed_png(b"STORE-BYTES-NEVER-PRINTED");
    let chunk = &signed[span.chunk.clone()];
    let mut damaged = chunk.to_vec();
    *damaged.last_mut().expect("a CRC") ^= 0xFF;
    let iend = signed.len() - 12;
    let mut file = signed[..span.chunk.end].to_vec();
    let mut starts = Vec::new();
    for (copy, rest) in [
        (chunk, &[][..]),
        (&damaged[..], &signed[span.chunk.end..iend]),
        (chunk, &signed[iend..]),
        (chunk, &[][..]),
    ] {
        starts.push(file.len());
        file.extend_from_slice(copy);
        file.extend_from_slice(rest);
    }
    let (out, doc) = run_inspect_json("signed-json.png", &file, None);

    assert_eq!(doc["format"], "png");
    // The damaged copy and the appended one are findings; the C2PA member reports them and moves
    // nothing.
    assert_eq!(out.status.code(), Some(1), "{doc}");
    assert_eq!(doc["verified"], false);
    let store = &doc["c2pa"]["store"];
    assert_eq!(store["size"], 25);
    assert_eq!(
        store["range"],
        json_range(span.payload.start as u64, span.payload.end as u64)
    );
    assert_eq!(
        store["chunk"],
        json_range(span.chunk.start as u64, span.chunk.end as u64)
    );
    let unread = &doc["c2pa"]["unread"];
    assert_eq!(unread.as_array().map(Vec::len), Some(4), "{doc}");
    for (i, (start, reason)) in starts
        .into_iter()
        .zip(["duplicate", "crc_mismatch", "after_idat", "after_iend"])
        .enumerate()
    {
        assert_eq!(unread[i]["size"], 25, "{doc}");
        assert_eq!(
            unread[i]["chunk"],
            json_range(start as u64, (start + chunk.len()) as u64)
        );
        assert_eq!(unread[i]["reason"], reason, "{doc}");
    }
    assert_eq!(
        unread[3]["description"],
        gamut::png::C2paUnreadReason::AfterIend.describe()
    );
    assert_eq!(doc["c2pa"]["not_validated"], gamut::png::C2PA_NOT_VALIDATED);
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains("STORE-BYTES"),
        "{doc}"
    );
}
