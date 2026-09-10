//! Robustness corpus: the `#![forbid(unsafe_code)]` reader is offset-driven — a classic
//! parser-exploit surface — so hostile input must yield a typed error or a valid parse, never a
//! panic, a hang, or unbounded allocation (STATUS P6).
//!
//! Every input is also driven through the streaming [`IfdReader`] and the two entry points must
//! *agree* — both parse to equal files, or both fail. The slice functions are thin wrappers over
//! the streaming engine (one parser), so this differential layer is now a regression gate on the
//! wrappers themselves staying faithful.

use gamut_ifd::{
    ByteOrder, Ifd, IfdReader, TiffFile, Value, Variant, read, read_audited, read_tree, write,
};

/// Sub-IFD pointer tags a DNG/EXIF-shaped consumer would follow.
const POINTER_TAGS: &[u16] = &[330, 34665, 34853];

/// A representative stream: two chained IFDs, inline and out-of-line values of several types, and
/// a sub-IFD tree two levels deep.
fn valid_stream(variant: Variant) -> Vec<u8> {
    let mut grandchild = Ifd::new();
    grandchild.set(33434, Value::Rational(vec![(1, 200)]));
    let mut child = Ifd::new();
    child.set(256, Value::Short(vec![16]));
    child.set(258, Value::Short(vec![8, 8, 8])); // out of line (classic)
    child.set_sub_ifd(34665, vec![grandchild]);
    let mut first = Ifd::new();
    first.set(256, Value::Short(vec![640]));
    first.set(270, Value::Ascii("first\0second".to_owned())); // multi-string, out of line
    first.set(282, Value::Rational(vec![(300, 1)]));
    first.set_sub_ifd(330, vec![child]);
    let mut second = Ifd::new();
    second.set(256, Value::Long(vec![9]));
    write(&TiffFile {
        order: ByteOrder::LittleEndian,
        variant,
        ifds: vec![first, second],
    })
    .expect("write")
}

/// All readers must survive `data` without panicking; the parse may succeed or fail, but the
/// slice and streaming paths must agree (equal files, or both errors — the *messages* may
/// differ, since bounds failures legitimately phrase differently across the two data flows).
fn survives(data: &[u8]) {
    let slice = read(data);
    let stream = IfdReader::open(data).and_then(|mut r| r.read_file());
    match (&slice, &stream) {
        (Ok(a), Ok(b)) => assert_eq!(a, b, "flat parse disagreement"),
        (Err(_), Err(_)) => {}
        _ => panic!("flat readers disagree: slice {slice:?} vs stream {stream:?}"),
    }
    let slice_tree = read_tree(data, POINTER_TAGS);
    let stream_tree = IfdReader::open(data).and_then(|mut r| r.read_tree(POINTER_TAGS));
    match (&slice_tree, &stream_tree) {
        (Ok(a), Ok(b)) => assert_eq!(a, b, "tree parse disagreement"),
        (Err(_), Err(_)) => {}
        _ => panic!("tree readers disagree: slice {slice_tree:?} vs stream {stream_tree:?}"),
    }
    // The dual-ledger differential invariant (issue #263): whenever a parse succeeds, every
    // byte the parser physically read is inside a structural claim, and every Parsed claim was
    // physically read. Over the whole hostile corpus, a parser that eats bytes it does not
    // declare — or declares bytes it never touched — is caught here mechanically.
    if let Ok((_, report)) = read_audited(data) {
        assert!(
            report.unclaimed_reads.is_empty(),
            "parser read bytes it never claimed: {report:?}"
        );
        assert!(
            report.unread_claims.is_empty(),
            "parser claimed bytes it never read: {report:?}"
        );
    }
}

#[test]
fn specific_malformed_inputs_error_without_panic() {
    let cases: &[&[u8]] = &[
        b"",
        b"II",
        b"II\x2a\x00",
        b"XX\x2a\x00\x08\x00\x00\x00",     // bad byte-order mark
        b"II\x00\x00\x08\x00\x00\x00",     // bad magic
        b"MM\x00\x2a\xff\xff\xff\x7f",     // first-IFD offset past EOF (big-endian)
        b"II\x2a\x00\x08\x00\x00\x00",     // first IFD at EOF
        b"II\x2a\x00\x08\x00\x00\x00\xff", // truncated IFD count
        // A whole entry count of 65 535 with no entry bytes at all behind it — the count is
        // present and well-formed, so the reader reaches the point of sizing the directory from
        // it. Nothing may be reserved for the 786 KiB it claims in a ten-byte file.
        b"II\x2a\x00\x08\x00\x00\x00\xff\xff",
        b"II\x2a\x00\x00\x00\x00\x00", // first-IFD offset 0 (no IFD)
        // A 1-entry IFD whose value count is huge (byte-length overflow path), then truncated.
        b"II\x2a\x00\x08\x00\x00\x00\x01\x00\x00\x01\x03\x00\xff\xff\xff\xff\x08\x00\x00\x00\x00\x00\x00\x00",
        // An IFD whose next-IFD pointer loops back to itself.
        b"II\x2a\x00\x08\x00\x00\x00\x00\x00\x08\x00\x00\x00",
    ];
    for &c in cases {
        survives(c);
    }
    // The loop case must be a typed error, not a hang.
    assert!(read(b"II\x2a\x00\x08\x00\x00\x00\x00\x00\x08\x00\x00\x00").is_err());
    // The hostile entry count must be refused against the source length, not merely survived.
    assert!(
        read(b"II\x2a\x00\x08\x00\x00\x00\xff\xff")
            .expect_err("65 535 entries in a ten-byte file")
            .to_string()
            .contains("IFD extends past end of file")
    );
}

#[test]
fn truncations_do_not_panic() {
    let valid = valid_stream(Variant::Classic);
    assert!(read_tree(&valid, POINTER_TAGS).is_ok());
    for len in 0..valid.len() {
        survives(&valid[..len]);
    }
}

#[test]
fn byte_flip_fuzz_does_not_panic() {
    let valid = valid_stream(Variant::Classic);
    // Deterministic LCG (no RNG dependency) drives the mutations.
    let mut state: u64 = 0x1234_5678_9abc_def0;
    let mut next = || {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (state >> 33) as u32
    };
    for _ in 0..5000 {
        let mut data = valid.clone();
        let flips = 1 + next() % 4;
        for _ in 0..flips {
            let pos = next() as usize % data.len();
            data[pos] ^= (next() & 0xff) as u8;
        }
        survives(&data);
    }
}

/// Every single-byte overwrite of every position with a boundary value — cheap exhaustive
/// coverage of the offset words, counts, and type codes that drive the parse.
#[test]
fn single_byte_overwrites_do_not_panic() {
    let valid = valid_stream(Variant::Classic);
    for pos in 0..valid.len() {
        for byte in [0x00, 0x01, 0x7f, 0x80, 0xff] {
            let mut data = valid.clone();
            data[pos] = byte;
            survives(&data);
        }
    }
}

/// Overlapping records are *report-not-reject* (issue #262): TIFF legitimately allows two
/// structures to share storage, so the parse must succeed — the dual-ledger byte audit is where
/// the overlap surfaces. This is the adversarial end-to-end check of that contract: an out-of-line
/// value whose offset points back into the file header.
#[test]
fn overlapping_value_offset_parses_and_surfaces_in_audit() {
    let data: &[u8] = &[
        b'I', b'I', 0x2a, 0x00, 0x08, 0x00, 0x00, 0x00, // header, first IFD at 8
        0x01, 0x00, // entry count = 1
        0x02, 0x01, // tag 258
        0x03, 0x00, // type SHORT
        0x03, 0x00, 0x00, 0x00, // count = 3 (6 bytes, forced out of line)
        0x00, 0x00, 0x00, 0x00, // value offset = 0 — the value span [0, 6) is the header
        0x00, 0x00, 0x00, 0x00, // next IFD = 0
    ];
    // Both readers parse it, agree, and decode the header bytes as the value.
    let file = read(data).expect("overlap parses");
    let streamed = IfdReader::open(data)
        .and_then(|mut r| r.read_file())
        .expect("overlap parses (streaming)");
    assert_eq!(file, streamed);
    assert_eq!(
        file.ifds[0].get(258),
        Some(&Value::Short(vec![0x4949, 0x002A, 0x0008]))
    );
    // The overlap is not silent: the byte audit flags the value span nesting into the header as a
    // structural conflict (partial overlap, not identical-extent legal sharing).
    let (audited, report) = read_audited(data).expect("audited parse");
    assert_eq!(audited, file);
    assert_eq!(report.conflicts.len(), 1, "header/value overlap flagged");
    assert!(!report.is_fully_classified());
}

/// A classic-TIFF stream of `n` chained zero-entry directories (6 bytes each), hand-emitted —
/// `write` links real directories the same way, but building 65 537 `Ifd`s through it is slower
/// than emitting the 6-byte records directly.
fn chain_of(n: usize) -> Vec<u8> {
    let mut data = vec![b'I', b'I', 0x2a, 0x00, 0x08, 0x00, 0x00, 0x00];
    for i in 0..n {
        data.extend_from_slice(&[0x00, 0x00]); // entry count = 0
        let next = if i + 1 == n {
            0
        } else {
            8 + (i as u32 + 1) * 6
        };
        data.extend_from_slice(&next.to_le_bytes());
    }
    data
}

/// The chain-length guard boundary: exactly `MAX_IFDS` (65 536) directories parse; one more is
/// a typed error, not a runaway walk — on both readers.
#[test]
fn chain_length_cap_is_exact() {
    let at_cap = chain_of(1 << 16);
    assert_eq!(read(&at_cap).expect("at cap").ifds.len(), 1 << 16);
    let over = chain_of((1 << 16) + 1);
    assert!(read(&over).is_err());
    let stream_over = IfdReader::open(&over[..]).and_then(|mut r| r.read_file());
    assert!(stream_over.is_err());
    // The streaming iterator surfaces the same bound lazily: 65 536 Ok items, then one Err.
    let mut reader = IfdReader::open(&over[..]).expect("open");
    let mut chain = reader.ifds();
    assert_eq!(
        chain.by_ref().take(1 << 16).filter(Result::is_ok).count(),
        1 << 16
    );
    assert!(chain.next().expect("cap error").is_err());
    assert!(chain.next().is_none());
}

#[cfg(feature = "bigtiff")]
mod bigtiff {
    use super::*;

    #[test]
    fn bigtiff_truncations_do_not_panic() {
        let valid = valid_stream(Variant::Big);
        assert!(read_tree(&valid, POINTER_TAGS).is_ok());
        for len in 0..valid.len() {
            survives(&valid[..len]);
        }
    }

    #[test]
    fn bigtiff_single_byte_overwrites_do_not_panic() {
        let valid = valid_stream(Variant::Big);
        for pos in 0..valid.len() {
            for byte in [0x00, 0x01, 0x7f, 0x80, 0xff] {
                let mut data = valid.clone();
                data[pos] = byte;
                survives(&data);
            }
        }
    }
}
