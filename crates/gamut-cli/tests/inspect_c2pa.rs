//! End-to-end tests for `gamut inspect`'s HEIC arm: which containers it accepts, and what the C2PA
//! report says once it does.
//!
//! These drive the built `gamut` binary (`CARGO_BIN_EXE_gamut`) so they exercise the real command
//! path — the format sniff, the container confirmation, and the lines that reach stdout — the way a
//! user runs it. What each line *means* is `gamut-heic`'s contract and is pinned by that crate's
//! tests; what is pinned here is that this binary routes the file correctly and prints the crate's
//! words through, unabridged.
//!
//! Fixtures are built by `gamut::isobmff::write` and then have hand-authored top-level boxes spliced
//! in after `ftyp`, since a C2PA `uuid` box is not part of the writer's model. C2PA clause
//! references are to the 2.4 specification.

use std::process::{Command, Output};

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
    write(&IsoBmffImage {
        major_brand: *b"heic",
        minor_version: 0,
        compatible_brands: vec![*b"heic", *b"mif1"],
        primary_item_id: 1,
        items: vec![coded_item(1, *b"hvc1", *b"hvcC")],
        groups: vec![],
    })
    .expect("valid HEVC still-image model")
}

/// An AVIF carrying the generic MIAF structural brand `mif1` as its **major** brand: one `av01`
/// item with an `av1C`, and no HEVC brand anywhere. This is the file the major-brand test alone
/// cannot tell from a HEIC (`references/heif` §7 settles it on the primary item's `hvcC`).
fn mif1_avif_file() -> Vec<u8> {
    write(&IsoBmffImage {
        major_brand: *b"mif1",
        minor_version: 0,
        compatible_brands: vec![*b"mif1", *b"miaf", *b"avif"],
        primary_item_id: 1,
        items: vec![coded_item(1, *b"av01", *b"av1C")],
        groups: vec![],
    })
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
fn an_avif_whose_major_brand_is_mif1_is_not_reported_as_a_heic() {
    // `mif1` is the generic MIAF structural brand, so a major-brand test alone accepts this file
    // and would report an AVIF's C2PA box as a HEIC's. The confirmation is `gamut-heic`'s own
    // still-image predicate, applied after the parse.
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
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(!out.status.success(), "stdout: {stdout}");
    assert!(
        stderr.contains("unsupported container brand 'mif1'"),
        "the message must name the brand it declined: {stderr}"
    );
    assert!(
        !stdout.contains("C2PA"),
        "nothing about the AVIF's provenance may be printed: {stdout}"
    );
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
