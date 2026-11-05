//! The issue #262 acceptance checklist: downstream RAW decoding (rawshift) replaces a hardened
//! TIFF parser with this crate and maps our errors onto its `ParseError` cases (`InvalidMagic`,
//! `InvalidByteOrder`, `OffsetOutOfBounds`, `CircularReference`, …). The crate deliberately has
//! no such variants — every hostile input is `ErrorKind::InvalidInput` with a distinguishing static
//! string — so those strings *are* the mapping contract, and this file pins them.
//!
//! Non-redundancy: the robustness corpus and in-module tests assert *behavior* (typed error,
//! path agreement, guard boundaries); this file's sole added value is pinning the *message* per
//! acceptance case, on both the slice entry point (`read`) and the streaming entry point
//! (`IfdReader`). Since the byte-completeness reshape (#263) the slice functions are thin wrappers
//! over the streaming engine — one parser — so the two entry points now phrase every failure
//! identically. Overlapping records, the one checklist item that is
//! report-not-reject by design, is pinned in `robustness.rs`
//! (`overlapping_value_offset_parses_and_surfaces_in_audit`).

use gamut_core::{Error, ErrorKind};
use gamut_ifd::{ByteOrder, Ifd, IfdReader, TiffFile, Value, Variant, read, write};

/// Unwraps the `InvalidInput` message — the string rawshift keys its `ParseError` mapping on.
fn msg<T: std::fmt::Debug>(res: Result<T, Error>) -> &'static str {
    match res {
        Err(error) if error.kind() == ErrorKind::InvalidInput => error
            .static_message()
            .expect("invalid input has a static message"),
        other => panic!("expected ErrorKind::InvalidInput, got {other:?}"),
    }
}

fn slice_msg(data: &[u8]) -> &'static str {
    msg(read(data))
}

fn stream_msg(data: &[u8]) -> &'static str {
    msg(IfdReader::open(data).and_then(|mut r| r.read_file()))
}

/// Asserts the case's message on both paths (most failures phrase identically).
fn both_paths(data: &[u8], expected: &str) {
    assert_eq!(slice_msg(data), expected);
    assert_eq!(stream_msg(data), expected);
}

#[test]
fn invalid_magic() {
    both_paths(b"II\x00\x00\x08\x00\x00\x00", "TIFF: bad magic number");
}

#[test]
fn invalid_byte_order() {
    both_paths(b"XX\x2a\x00\x08\x00\x00\x00", "TIFF: bad byte-order mark");
}

#[test]
fn truncated_header() {
    both_paths(b"II\x2a", "TIFF: header too short");
}

/// A first-IFD offset far past EOF. With the slice reader now a thin wrapper over the streaming
/// engine (one parser), the positioned directory read fails identically on both entry points.
#[test]
fn ifd_offset_out_of_bounds() {
    both_paths(b"MM\x00\x2a\x7f\xff\xff\xff", "TIFF: read out of bounds");
}

/// A 26-byte classic file with one out-of-line SHORT×3 entry whose value offset is `voff`.
fn file_with_value_offset(voff: u32) -> Vec<u8> {
    let mut data = vec![
        b'I', b'I', 0x2a, 0x00, 0x08, 0x00, 0x00, 0x00, // header, first IFD at 8
        0x01, 0x00, // entry count = 1
        0x02, 0x01, // tag 258
        0x03, 0x00, // type SHORT
        0x03, 0x00, 0x00, 0x00, // count = 3 (6 bytes, forced out of line)
    ];
    data.extend_from_slice(&voff.to_le_bytes());
    data.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // next IFD = 0
    data
}

#[test]
fn value_offset_out_of_bounds() {
    both_paths(
        &file_with_value_offset(0x1000),
        "TIFF: value offset out of bounds",
    );
}

/// The other half of the two-error distinction: the value starts in bounds but overruns EOF.
#[test]
fn value_overruns_end_of_file() {
    both_paths(
        &file_with_value_offset(22),
        "TIFF: field value out of bounds",
    );
    // The boundary: an offset exactly at EOF names a valid (empty) position, so it is the
    // *span*, not the offset, that is out of bounds — on both paths.
    both_paths(
        &file_with_value_offset(26),
        "TIFF: field value out of bounds",
    );
}

/// An entry count whose directory body would extend past EOF.
#[test]
fn ifd_body_overruns_end_of_file() {
    both_paths(
        b"II\x2a\x00\x08\x00\x00\x00\xff\x00",
        "TIFF: IFD extends past end of file",
    );
}

/// A first-IFD offset exactly at EOF: the directory read starts at EOF, so the positioned count
/// read fails out of bounds — identically on both entry points (one parser).
#[test]
fn ifd_at_exactly_end_of_file() {
    both_paths(b"II\x2a\x00\x08\x00\x00\x00", "TIFF: read out of bounds");
}

#[test]
fn circular_ifd_chain() {
    // The IFD at 8 names itself as the next IFD.
    both_paths(
        b"II\x2a\x00\x08\x00\x00\x00\x00\x00\x08\x00\x00\x00",
        "TIFF: IFD chain loops",
    );
}

#[test]
fn circular_sub_ifd_pointer() {
    // Root at 8 whose tag-330 pointer names the child at 26, whose own pointer names 26 again.
    let data: &[u8] = &[
        b'I', b'I', 0x2a, 0x00, 0x08, 0x00, 0x00, 0x00, // header, first IFD at 8
        0x01, 0x00, // IFD0: entry count = 1
        0x4a, 0x01, // tag 330
        0x04, 0x00, // type LONG
        0x01, 0x00, 0x00, 0x00, // count = 1
        0x1a, 0x00, 0x00, 0x00, // offset 26 (the child directory)
        0x00, 0x00, 0x00, 0x00, // next IFD = 0
        0x01, 0x00, // child at 26: entry count = 1
        0x4a, 0x01, // tag 330
        0x04, 0x00, // type LONG
        0x01, 0x00, 0x00, 0x00, // count = 1
        0x1a, 0x00, 0x00, 0x00, // offset 26 — itself
        0x00, 0x00, 0x00, 0x00, // next IFD = 0
    ];
    assert_eq!(
        msg(IfdReader::open(data).and_then(|mut r| r.read_tree(&[330]))),
        "TIFF: sub-IFD pointer loop"
    );
}

#[test]
fn sub_ifd_depth_bomb() {
    let mut ifd = Ifd::new();
    ifd.set(256, Value::Short(vec![1]));
    for _ in 0..17 {
        let mut parent = Ifd::new();
        parent.set_sub_ifd(330, vec![ifd]);
        ifd = parent;
    }
    let data = write(&TiffFile {
        order: ByteOrder::LittleEndian,
        variant: Variant::Classic,
        ifds: vec![ifd],
    })
    .expect("write");
    assert_eq!(
        msg(IfdReader::open(&data[..]).and_then(|mut r| r.read_tree(&[330]))),
        "TIFF: sub-IFD tree too deep"
    );
}

#[test]
fn ifd_chain_over_the_length_cap() {
    // 65 537 chained zero-entry directories, hand-emitted (6 bytes each).
    let n = (1 << 16) + 1;
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
    both_paths(&data, "TIFF: too many IFDs");
}

#[test]
fn no_ifd_at_all() {
    both_paths(b"II\x2a\x00\x00\x00\x00\x00", "TIFF: file has no IFD");
}

/// A classic value count of `u32::MAX`: the byte length fits u64, so the span check (not an
/// allocation) rejects it.
#[test]
fn hostile_classic_value_count() {
    both_paths(
        b"II\x2a\x00\x08\x00\x00\x00\x01\x00\x00\x01\x03\x00\xff\xff\xff\xff\x08\x00\x00\x00\x00\x00\x00\x00",
        "TIFF: field value out of bounds",
    );
}

#[cfg(feature = "bigtiff")]
mod bigtiff {
    use super::*;

    /// BigTIFF header (LE), first IFD at 16.
    const HEADER: [u8; 16] = [
        b'I', b'I', 0x2b, 0x00, 0x08, 0x00, 0x00,
        0x00, // BOM, magic 43, offset size 8, reserved
        0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // first IFD at 16
    ];

    /// An 8-byte entry count of `u64::MAX`: the `count * entry_size` multiply must be the guard
    /// (only BigTIFF counts can overflow it).
    #[test]
    fn entry_count_overflow() {
        let mut data = HEADER.to_vec();
        data.extend_from_slice(&u64::MAX.to_le_bytes());
        both_paths(&data, "TIFF: IFD entry count overflow");
    }

    /// A value count of `u64::MAX` on a SHORT field: the `count * type_size` multiply overflows.
    #[test]
    fn field_length_overflow() {
        let mut data = HEADER.to_vec();
        data.extend_from_slice(&1u64.to_le_bytes()); // entry count = 1
        data.extend_from_slice(&[0x02, 0x01]); // tag 258
        data.extend_from_slice(&[0x03, 0x00]); // type SHORT
        data.extend_from_slice(&u64::MAX.to_le_bytes()); // count = u64::MAX
        data.extend_from_slice(&[0u8; 8]); // value/offset word
        data.extend_from_slice(&[0u8; 8]); // next IFD = 0
        both_paths(&data, "TIFF: field length overflow");
    }
}
