//! Public box-walker coverage for alternate headers, UUID framing, and hostile sizes.

use gamut_isobmff::BoxReader;

/// A `size == 1` box, whose real length follows the type as a 64-bit `largesize`:
/// a 16-byte header (4 size + 4 type + 8 largesize) and a 3-byte body.
const LARGESIZE_MDAT: [u8; 19] = [
    0, 0, 0, 1, b'm', b'd', b'a', b't', // large-size marker and type
    0, 0, 0, 0, 0, 0, 0, 19, // 16-byte header + 3-byte body
    0xAA, 0xBB, 0xCC,
];

#[test]
fn a_largesize_box_exposes_the_body_after_its_sixteen_byte_header() {
    let mut reader = BoxReader::new(&LARGESIZE_MDAT);
    let b = reader.next().unwrap().unwrap();

    assert_eq!(b.ty, *b"mdat");
    assert_eq!(b.offset, 0);
    // The body starts after all sixteen header bytes, not after the first eight: a reader that
    // mistook the largesize field for payload would report five bytes here.
    assert_eq!(b.body, &[0xAA, 0xBB, 0xCC]);
    assert_eq!(b.payload(), b.body);
    assert_eq!(b.user_type, None);
}

#[test]
fn a_largesize_box_advances_the_cursor_over_its_full_header() {
    let mut reader = BoxReader::new(&LARGESIZE_MDAT);
    reader.next().unwrap().unwrap();

    // Distinct from the body claim above: the body can be sliced correctly while the cursor is
    // left short, and the walk would then resynchronise inside the box it just returned.
    assert_eq!(reader.position(), LARGESIZE_MDAT.len());
    assert_eq!(reader.remaining(), 0);
    assert!(reader.next().is_none());
}

#[test]
fn zero_size_box_consumes_the_reader_slice_tail() {
    let data = [
        0, 0, 0, 8, b'f', b'r', b'e', b'e', // ordinary box
        0, 0, 0, 0, b'm', b'd', b'a', b't', 1, 2, 3, 4, // open-ended tail
    ];
    let mut reader = BoxReader::new(&data);
    let first = reader.next().unwrap().unwrap();
    assert_eq!(first.ty, *b"free");
    assert_eq!(reader.position(), 8);

    let tail = reader.next().unwrap().unwrap();
    assert_eq!(tail.ty, *b"mdat");
    assert_eq!(tail.offset, 8);
    assert_eq!(tail.body, &[1, 2, 3, 4]);
    assert_eq!(reader.position(), data.len());
    assert!(reader.next().is_none());
}

#[test]
fn uuid_separates_user_type_from_payload_without_changing_body() {
    let user_type = [
        0x85, 0xC0, 0xB6, 0x87, 0x82, 0x0F, 0x11, 0xE0, 0x81, 0x11, 0xF4, 0xCE, 0x46, 0x2B, 0x6A,
        0x48,
    ];
    let mut data = vec![0, 0, 0, 27, b'u', b'u', b'i', b'd'];
    data.extend_from_slice(&user_type);
    data.extend_from_slice(&[9, 8, 7]);
    let mut reader = BoxReader::new(&data);
    let b = reader.next().unwrap().unwrap();

    assert_eq!(b.user_type, Some(user_type));
    assert_eq!(&b.body[..16], &user_type);
    assert_eq!(&b.body[16..], &[9, 8, 7]);
    assert_eq!(b.payload(), &[9, 8, 7]);
    assert_eq!(reader.position(), 27);
}

#[test]
fn largesize_uuid_keeps_user_type_outside_payload() {
    let user_type = *b"0123456789ABCDEF";
    let mut data = vec![0, 0, 0, 1, b'u', b'u', b'i', b'd'];
    data.extend_from_slice(&34_u64.to_be_bytes());
    data.extend_from_slice(&user_type);
    data.extend_from_slice(&[0xCA, 0xFE]);
    let mut reader = BoxReader::new(&data);
    let b = reader.next().unwrap().unwrap();

    assert_eq!(b.user_type, Some(user_type));
    assert_eq!(b.body, &data[16..]);
    assert_eq!(b.payload(), &[0xCA, 0xFE]);
    assert_eq!(reader.position(), data.len());
}

#[test]
fn uuid_accepts_exactly_the_user_type_with_an_empty_payload() {
    let user_type = *b"0123456789ABCDEF";
    let mut data = vec![0, 0, 0, 24, b'u', b'u', b'i', b'd'];
    data.extend_from_slice(&user_type);
    let mut reader = BoxReader::new(&data);
    let b = reader.next().unwrap().unwrap();

    assert_eq!(b.user_type, Some(user_type));
    assert_eq!(b.body, user_type);
    assert!(b.payload().is_empty());
    assert_eq!(reader.position(), data.len());
}

#[test]
fn mixed_headers_leave_cursor_at_each_authoritative_end() {
    let data = [
        0, 0, 0, 9, b'f', b'r', b'e', b'e', 1, // ordinary: 0..9
        0, 0, 0, 1, b's', b'k', b'i', b'p', // large: 9..27
        0, 0, 0, 0, 0, 0, 0, 18, 2, 3, 0, 0, 0, 0, b'm', b'd', b'a', b't', 4,
        5, // open: 27..37
    ];
    let mut reader = BoxReader::new(&data);

    assert_eq!(reader.next().unwrap().unwrap().body, &[1]);
    assert_eq!(reader.position(), 9);
    assert_eq!(reader.next().unwrap().unwrap().body, &[2, 3]);
    assert_eq!(reader.position(), 27);
    assert_eq!(reader.next().unwrap().unwrap().body, &[4, 5]);
    assert_eq!(reader.position(), 37);
}

#[test]
fn malformed_sizes_and_uuid_prefixes_are_rejected() {
    let cases: &[&[u8]] = &[
        &[0, 0, 0, 1, b'f', b'r', b'e', b'e'], // missing largesize
        &[0, 0, 0, 1, b'f', b'r', b'e', b'e', 0, 0, 0, 0, 0, 0, 0, 15], // largesize shorter than its header
        &[0, 0, 0, 1, b'f', b'r', b'e', b'e', 0, 0, 0, 0, 0, 0, 0, 17], // body truncated
        &[
            0, 0, 0, 1, b'f', b'r', b'e', b'e', 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
        ], // hostile largesize
        &[
            0, 0, 0, 23, b'u', b'u', b'i', b'd', 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ],
    ];

    for data in cases {
        assert!(
            matches!(BoxReader::new(data).next(), Some(Err(_))),
            "accepted {data:?}"
        );
    }
}

#[test]
fn an_error_ends_the_iteration_at_the_slice_end() {
    // A 32-bit size of 7 is smaller than the 8-byte header it sits in, followed by bytes that
    // would frame a well-formed box if the walk resumed at the next header position.
    let data = [
        0, 0, 0, 7, b'f', b'r', b'e', b'e', // size < header
        0, 0, 0, 8, b's', b'k', b'i', b'p', // would parse if the reader carried on
    ];
    let mut reader = BoxReader::new(&data);

    assert!(matches!(reader.next(), Some(Err(_))));
    assert_eq!(reader.position(), data.len());
    assert_eq!(reader.remaining(), 0);
    assert!(reader.next().is_none());
}
