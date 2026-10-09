//! Differential cross-check of gamut-iptc's legacy IIM/IRB handling against exiv2 (the reference
//! implementation), via the dev-only [`gamut_iptc_oracle`]. Covers the binary carrier; exiv2's XMP
//! toolkit is disabled in the oracle build, so the IPTC-in-XMP leg is out of scope here.
//!
//! Needs the `third_party/exiv2` submodule and a C++ toolchain + CMake/Ninja (see the oracle crate).

use gamut_iptc::{IimBlock, IimDataSet, PhotoshopIrb};

fn ds(record: u8, dataset: u8, data: &[u8]) -> IimDataSet {
    IimDataSet {
        record,
        dataset,
        data: data.to_vec(),
    }
}

/// A spread of well-known Application-record datasets: the mandatory Record Version, single and
/// repeatable strings, and a longer caption.
fn fixture() -> IimBlock {
    IimBlock {
        datasets: vec![
            ds(2, 0, &[0, 4]),      // Record Version = 4
            ds(2, 80, b"Jane Doe"), // By-line
            ds(2, 90, b"Paris"),    // City
            ds(2, 25, b"sky"),      // Keywords (repeatable)
            ds(2, 25, b"sea"),
            ds(2, 120, b"A wide caption, with punctuation."), // Caption/Abstract
        ],
    }
}

fn multiset(block: &IimBlock) -> Vec<(u8, u8, Vec<u8>)> {
    let mut v: Vec<_> = block
        .datasets
        .iter()
        .map(|d| (d.record, d.dataset, d.data.clone()))
        .collect();
    v.sort();
    v
}

#[test]
fn gamut_iim_matches_exiv2_dataset_for_dataset() {
    let block = fixture();
    let bytes = block.encode().unwrap();

    let exiv2 = gamut_iptc_oracle::parse_iim(&bytes).expect("exiv2 parses gamut's IIM stream");
    assert_eq!(exiv2.len(), block.datasets.len());
    for (o, g) in exiv2.iter().zip(&block.datasets) {
        assert_eq!(o.record, u16::from(g.record), "record mismatch");
        assert_eq!(o.tag, u16::from(g.dataset), "dataset mismatch");
        assert_eq!(
            o.value, g.data,
            "value mismatch for {}:{}",
            g.record, g.dataset
        );
    }

    // gamut re-parses its own output identically, alongside the oracle.
    assert_eq!(IimBlock::parse(&bytes).unwrap(), block);
}

#[test]
fn gamut_reads_exiv2_reencoded_stream() {
    let block = fixture();
    let bytes = block.encode().unwrap();

    let exiv2_bytes = gamut_iptc_oracle::reencode_iim(&bytes).expect("exiv2 re-encodes the stream");
    let reparsed = IimBlock::parse(&exiv2_bytes).expect("gamut parses exiv2's output");

    // exiv2 may reorder datasets on encode; compare as multisets of (record, dataset, value).
    assert_eq!(multiset(&reparsed), multiset(&block));
}

#[test]
fn gamut_irb_payload_matches_exiv2_locate() {
    let block = fixture();
    let irb = PhotoshopIrb::with_iptc(block.encode().unwrap())
        .encode()
        .unwrap();

    let payload = gamut_iptc_oracle::locate_iptc_irb(&irb).expect("exiv2 locates the 0x0404 IRB");
    assert_eq!(payload, block.encode().unwrap());
}

#[test]
fn exiv2_rejects_garbage_but_accepts_gamut_output() {
    // Not an IIM stream (no 0x1C marker) and not an 8BIM resource.
    assert!(
        gamut_iptc_oracle::parse_iim(&[0xDE, 0xAD, 0xBE, 0xEF])
            .unwrap_or_default()
            .is_empty()
    );
    assert!(gamut_iptc_oracle::locate_iptc_irb(b"not a photoshop irb").is_none());
    // ...but it accepts what gamut produces.
    let bytes = fixture().encode().unwrap();
    assert!(gamut_iptc_oracle::parse_iim(&bytes).is_some());
}

/// A stream spanning the Envelope record and the Application-record datasets *outside* the
/// PMD-mapped subset — the ones gamut's tag table names but never projects from XMP. The existing
/// fixture is record 2 only, so this is the record-1 leg and the wide dataset numbers.
fn wide_fixture() -> IimBlock {
    IimBlock {
        datasets: vec![
            // Envelope record.
            ds(1, 0, &[0, 4]),         // Model Version = 4
            ds(1, 20, &[0, 3]),        // File Format = 3 (TIFF)
            ds(1, 22, &[0, 1]),        // File Format Version = 1
            ds(1, 30, b"gamut"),       // Service Identifier
            ds(1, 40, b"00000001"),    // Envelope Number
            ds(1, 70, b"19900127"),    // Date Sent
            ds(1, 80, b"133015+0100"), // Time Sent
            // Application record, beyond the XMP-mapped datasets.
            ds(2, 0, &[0, 4]),               // Record Version = 4
            ds(2, 10, b"5"),                 // Urgency
            ds(2, 30, b"19900127"),          // Release Date
            ds(2, 35, b"090000-0500"),       // Release Time
            ds(2, 65, b"gamut"),             // Originating Program
            ds(2, 70, b"1.0"),               // Program Version
            ds(2, 118, b"news@example.org"), // Contact
            ds(2, 131, b"L"),                // Image Orientation
            ds(2, 135, b"en"),               // Language Identifier
            ds(2, 151, b"044100"),           // Audio Sampling Rate
            ds(2, 200, &[0, 3]),             // ObjectData Preview File Format
        ],
    }
}

#[test]
fn gamut_and_exiv2_agree_on_the_datasets_outside_the_xmp_mapping() {
    let block = wide_fixture();
    let bytes = block.encode().unwrap();

    let exiv2 = gamut_iptc_oracle::parse_iim(&bytes).expect("exiv2 parses the wide stream");
    assert_eq!(exiv2.len(), block.datasets.len());
    for (o, g) in exiv2.iter().zip(&block.datasets) {
        assert_eq!(
            (o.record, o.tag, &o.value),
            (u16::from(g.record), u16::from(g.dataset), &g.data),
            "mismatch at {}:{}",
            g.record,
            g.dataset
        );
    }

    // exiv2 re-encodes the same datasets (order is exiv2's own, so compare as multisets).
    let exiv2_bytes = gamut_iptc_oracle::reencode_iim(&bytes).expect("exiv2 re-encodes the stream");
    let reparsed = IimBlock::parse(&exiv2_bytes).expect("gamut parses exiv2's output");
    assert_eq!(multiset(&reparsed), multiset(&block));
}
