//! Streaming-vs-slice equivalence: the [`IfdReader`] must produce exactly what the eager slice
//! readers produce, through every source shape — a borrowed slice, a seekable stream, and an
//! offset-rebased view — and must honor its laziness contract (only the structure actually
//! asked for is read).

use gamut_core::Result;
use gamut_ifd::{
    ByteOrder, Ifd, IfdReader, ReadAt, SegmentMap, SpanKind, StreamSource, TiffFile, Tracked,
    Value, Variant, read, read_tree, tags, write,
};

/// A flat single-IFD file, a two-IFD chain, and a nested sub-IFD tree — the shapes the
/// fixtures cover for every order/variant.
fn fixture_files(order: ByteOrder, variant: Variant) -> Vec<TiffFile> {
    let mut flat = Ifd::new();
    flat.set(256, Value::Short(vec![640]));
    flat.set(270, Value::Ascii("first\0second".to_owned())); // multi-string, out of line
    flat.set(282, Value::Rational(vec![(300, 1)]));

    let mut page_a = Ifd::new();
    page_a.set(256, Value::Short(vec![640]));
    let mut page_b = Ifd::new();
    page_b.set(256, Value::Long(vec![9]));
    page_b.set(258, Value::Short(vec![8, 8, 8])); // out of line (classic)

    let mut grandchild = Ifd::new();
    grandchild.set(33434, Value::Rational(vec![(1, 200)]));
    let mut child = Ifd::new();
    child.set(256, Value::Short(vec![16]));
    child.set_sub_ifd(tags::EXIF_IFD, vec![grandchild]);
    let mut root = Ifd::new();
    root.set(256, Value::Short(vec![640]));
    root.set_sub_ifd(tags::SUB_IFDS, vec![child]);

    vec![
        TiffFile {
            order,
            variant,
            ifds: vec![flat],
        },
        TiffFile {
            order,
            variant,
            ifds: vec![page_a, page_b],
        },
        TiffFile {
            order,
            variant,
            ifds: vec![root],
        },
    ]
}

fn orders_and_variants() -> Vec<(ByteOrder, Variant)> {
    let classic = vec![
        (ByteOrder::LittleEndian, Variant::Classic),
        (ByteOrder::BigEndian, Variant::Classic),
    ];
    #[cfg(not(feature = "bigtiff"))]
    return classic;
    #[cfg(feature = "bigtiff")]
    {
        let mut all = classic;
        all.push((ByteOrder::LittleEndian, Variant::Big));
        all.push((ByteOrder::BigEndian, Variant::Big));
        all
    }
}

/// One truth, three source shapes: for every fixture, the streaming parse must equal the slice
/// parse through `&[u8]`, through a seekable stream, and through a rebased view of the same
/// bytes embedded at a nonzero offset in a larger buffer.
#[test]
fn streaming_equals_slice_across_source_shapes() {
    for (order, variant) in orders_and_variants() {
        for file in fixture_files(order, variant) {
            let bytes = write(&file).expect("write");
            let flat = read(&bytes).expect("slice read");
            let tree = read_tree(&bytes, tags::STANDARD_POINTER_TAGS).expect("slice read_tree");
            assert_eq!(tree, file, "fixture round-trips");

            let mut slice_reader = IfdReader::open(&bytes[..]).expect("open slice");
            assert_eq!(slice_reader.read_file().expect("read_file"), flat);
            assert_eq!(
                slice_reader
                    .read_tree(tags::STANDARD_POINTER_TAGS)
                    .expect("read_tree"),
                tree
            );

            let cursor = std::io::Cursor::new(bytes.clone());
            let mut stream_reader =
                IfdReader::open(StreamSource::new(cursor)).expect("open stream");
            assert_eq!(stream_reader.read_file().expect("read_file"), flat);
            assert_eq!(
                stream_reader
                    .read_tree(tags::STANDARD_POINTER_TAGS)
                    .expect("read_tree"),
                tree
            );

            // The offset-preserving embed: the same TIFF stream at offset 41 of a larger buffer
            // (an odd offset, so nothing accidentally stays aligned).
            let mut outer = vec![0xEE; 41];
            outer.extend_from_slice(&bytes);
            outer.extend_from_slice(&[0xEE; 23]);
            let mut rebased_reader =
                IfdReader::open((&outer[..]).rebased(41)).expect("open rebased");
            assert_eq!(rebased_reader.read_file().expect("read_file"), flat);
            assert_eq!(
                rebased_reader
                    .read_tree(tags::STANDARD_POINTER_TAGS)
                    .expect("read_tree"),
                tree
            );
        }
    }
}

/// The full audited walk over every fixture: typed claims + tracked reads, sub-IFDs followed
/// like a codec would, padding classified from the actual bytes — ending in a report where
/// **every byte of the file is classified** and both dual-ledger invariants hold.
#[test]
fn audited_read_classifies_every_byte_of_written_files() {
    for (order, variant) in orders_and_variants() {
        for file in fixture_files(order, variant) {
            let bytes = write(&file).expect("write");
            let mut tracked = Tracked::new(&bytes[..]);
            let mut map = SegmentMap::new(bytes.len() as u64);
            let mut reader = IfdReader::open(&mut tracked).expect("open");
            let parsed = reader.read_file_audited(&mut map).expect("read_file");
            assert_eq!(parsed, read(&bytes).expect("slice parity"));

            // Follow the sub-IFD pointer graph the way a codec's audit would.
            let mut pending: Vec<u64> = Vec::new();
            let collect = |ifd: &Ifd, pending: &mut Vec<u64>| {
                for &tag in tags::STANDARD_POINTER_TAGS {
                    if let Some(offs) = ifd.get_u64_vec(tag) {
                        pending.extend(offs);
                    }
                }
            };
            for ifd in &parsed.ifds {
                collect(ifd, &mut pending);
            }
            while let Some(off) = pending.pop() {
                let (child, next) = reader.read_ifd_at_audited(off, &mut map).expect("child");
                assert_eq!(next, 0);
                collect(&child, &mut pending);
            }

            let mut report = map.finish(Some(tracked.ledger()));
            report
                .classify_padding(&mut (&bytes[..]))
                .expect("classify_padding");
            assert!(report.is_fully_classified(), "report: {report:?}");
            // The typed view partitions the file: header first, and kinds as expected.
            assert_eq!(report.segments[0].kind, SpanKind::Header);
            assert!(
                report
                    .segments
                    .iter()
                    .all(|s| !matches!(s.kind, SpanKind::Data(_))),
                "no Data extents were declared in this structural walk"
            );
        }
    }
}

/// A [`ReadAt`] wrapper that counts the bytes fetched, pinning the laziness contract.
struct Counting<S> {
    inner: S,
    bytes_read: u64,
}

impl<S: ReadAt> ReadAt for Counting<S> {
    fn read_exact_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.bytes_read += buf.len() as u64;
        self.inner.read_exact_at(offset, buf)
    }

    fn len(&mut self) -> Result<u64> {
        self.inner.len()
    }
}

/// The laziness contract: opening, reading one directory, and decoding one value from a file
/// with megabytes of (strip-like) payload reads only the header, that directory's body, and
/// that value's span — not the payload.
#[test]
fn lazy_reads_touch_only_what_is_asked_for() {
    let mut ifd = Ifd::new();
    ifd.set(256, Value::Short(vec![640]));
    ifd.set(258, Value::Short(vec![8, 8, 8])); // 6 bytes, out of line
    ifd.set(273, Value::Long(vec![0x10_0000])); // "StripOffsets": points into the payload
    let bytes = write(&TiffFile {
        order: ByteOrder::LittleEndian,
        variant: Variant::Classic,
        ifds: vec![ifd],
    })
    .expect("write");
    // Two megabytes of payload after the directory — the part a lazy parse must never touch.
    let mut data = bytes;
    data.resize(2 * 1024 * 1024, 0xAB);

    let mut reader = IfdReader::open(Counting {
        inner: &data[..],
        bytes_read: 0,
    })
    .expect("open");
    let raw = reader
        .read_ifd(reader.first_ifd_offset())
        .expect("read_ifd");
    let out_of_line = raw.entry(258).expect("entry").clone();
    assert_eq!(
        reader.value(&out_of_line).expect("value"),
        Value::Short(vec![8, 8, 8])
    );
    let read_so_far = reader.source_mut().bytes_read;
    // Header (≤16) + count (2) + body (3 entries × 12 + 4) + the 6-byte value: 64 is generous,
    // two megabytes is the failure mode.
    assert!(read_so_far <= 64, "lazy parse read {read_so_far} bytes");
    // Decoding the whole directory re-fetches the one out-of-line value but still never touches
    // the payload: the strip *offset* is just an inline integer, and the strip bytes themselves
    // are never this crate's to read.
    let decoded = reader.decode_ifd(&raw).expect("decode");
    assert_eq!(decoded.get_u32(273), Some(0x10_0000));
    assert!(reader.source_mut().bytes_read <= read_so_far + 16);
}

/// The maker-note pattern end to end: a mini-IFD whose internal offsets are relative to the
/// note start, embedded at an arbitrary offset — parsed with `with_layout` over a rebased view,
/// out-of-line values resolving through the rebase.
#[test]
fn maker_note_mini_ifd_parses_through_a_rebased_view() {
    // The note is itself a headered TIFF stream (offsets relative to its own start), the layout
    // vendors like Olympus/Panasonic use; the enclosing "file" carries it at offset 1000.
    let mut note_ifd = Ifd::new();
    note_ifd.set(1, Value::Short(vec![7]));
    note_ifd.set(2, Value::Ascii("vendor mode".to_owned())); // out of line within the note
    let note = write(&TiffFile {
        order: ByteOrder::LittleEndian,
        variant: Variant::Classic,
        ifds: vec![note_ifd.clone()],
    })
    .expect("write note");
    let mut outer = vec![0u8; 1000];
    outer.extend_from_slice(&note);

    // A consumer that knows the layout drives the directory directly (no header re-parse)…
    let mut reader = IfdReader::with_layout(
        (&outer[..]).rebased(1000),
        ByteOrder::LittleEndian,
        Variant::Classic,
    );
    let raw = reader.read_ifd(8).expect("note directory");
    assert_eq!(reader.decode_ifd(&raw).expect("decode"), note_ifd);
    let text = raw.entry(2).expect("ascii entry").clone();
    assert_eq!(
        reader.value(&text).expect("value through rebase"),
        Value::Ascii("vendor mode".to_owned())
    );
    // …and one that lets the note's own header speak gets the same directory.
    let mut headered = IfdReader::open((&outer[..]).rebased(1000)).expect("open");
    assert_eq!(
        headered.read_file().expect("read_file").ifds,
        vec![note_ifd]
    );
}

/// A directory the format defines without a next-IFD pointer, cut from a written file: the
/// header, count and entries, ending exactly at the last entry's last byte. Returns the
/// bytes, the directory's offset and the directory it holds.
fn unterminated_directory(order: ByteOrder, variant: Variant) -> (Vec<u8>, u64, Ifd) {
    let mut ifd = Ifd::new();
    ifd.set(1, Value::Short(vec![7])); // inline: no value bytes after the directory
    ifd.set(2, Value::Long(vec![0x0102_0304]));
    let bytes = write(&TiffFile {
        order,
        variant,
        ifds: vec![ifd.clone()],
    })
    .expect("write");
    let offset = IfdReader::open(&bytes[..])
        .expect("open")
        .first_ifd_offset();
    let end = offset as usize + variant.count_size() + 2 * variant.entry_size();
    assert_eq!(
        bytes.len(),
        end + variant.offset_size(),
        "the written file ends with the directory's next pointer"
    );
    (bytes[..end].to_vec(), offset, ifd)
}

/// `read_ifd_unterminated` at the exact boundary: a directory ending at the source's last byte
/// reads (where `read_ifd` refuses it for its absent pointer), and one byte short does not.
#[test]
fn unterminated_directory_reads_to_the_last_byte_and_no_further() {
    for (order, variant) in orders_and_variants() {
        let (bytes, offset, ifd) = unterminated_directory(order, variant);

        let mut reader = IfdReader::with_layout(&bytes[..], order, variant);
        let raw = reader
            .read_ifd_unterminated(offset)
            .expect("ends at last byte");
        assert_eq!(raw.offset, offset);
        assert_eq!(raw.next, 0);
        assert_eq!(reader.decode_ifd(&raw).expect("decode"), ifd);
        assert!(
            reader.read_ifd(offset).is_err(),
            "{variant:?}: read_ifd needs the pointer bytes the source lacks"
        );

        let short = &bytes[..bytes.len() - 1];
        let mut reader = IfdReader::with_layout(short, order, variant);
        assert!(
            reader.read_ifd_unterminated(offset).is_err(),
            "{variant:?}: one byte short of the last entry"
        );
    }
}

/// The pointer's bytes are not part of an unterminated directory: where the source does carry
/// bytes after the entries (here a would-be pointer of all ones), they are neither fetched nor
/// reported as `next`.
#[test]
fn unterminated_directory_never_reads_past_its_entries() {
    for (order, variant) in orders_and_variants() {
        let (mut bytes, offset, _) = unterminated_directory(order, variant);
        let entries_end = bytes.len() as u64;
        bytes.extend(std::iter::repeat_n(0xFF, variant.offset_size()));

        let mut reader = IfdReader::with_layout(
            Counting {
                inner: &bytes[..],
                bytes_read: 0,
            },
            order,
            variant,
        );
        let raw = reader.read_ifd_unterminated(offset).expect("read");
        assert_eq!(raw.next, 0);
        assert_eq!(
            reader.source_mut().bytes_read,
            entries_end - offset,
            "{variant:?}: exactly the count and the entries"
        );
        // The same bytes through `read_ifd` do yield the would-be pointer.
        assert_ne!(reader.read_ifd(offset).expect("read_ifd").next, 0);
    }
}

/// The stream shape a decoder actually uses: a real file on disk, parsed through
/// `StreamSource<File>` without reading it into memory.
#[test]
fn parses_a_real_file_through_stream_source() {
    let mut grandchild = Ifd::new();
    grandchild.set(33434, Value::Rational(vec![(1, 200)]));
    let mut child = Ifd::new();
    child.set(256, Value::Short(vec![16]));
    child.set_sub_ifd(tags::EXIF_IFD, vec![grandchild]);
    let mut root = Ifd::new();
    root.set(256, Value::Short(vec![640]));
    root.set_sub_ifd(tags::SUB_IFDS, vec![child]);
    let file = TiffFile {
        order: ByteOrder::BigEndian,
        variant: Variant::Classic,
        ifds: vec![root],
    };
    let bytes = write(&file).expect("write");

    let path = std::env::temp_dir().join(format!(
        "gamut-ifd-streaming-{}-{:p}.tiff",
        std::process::id(),
        &file
    ));
    std::fs::write(&path, &bytes).expect("write temp file");
    let handle = std::fs::File::open(&path).expect("open temp file");
    let mut reader = IfdReader::open(StreamSource::new(handle)).expect("open reader");
    let parsed = reader
        .read_tree(tags::STANDARD_POINTER_TAGS)
        .expect("read_tree");
    std::fs::remove_file(&path).expect("remove temp file");
    assert_eq!(parsed, file);
}
