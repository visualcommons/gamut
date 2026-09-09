//! The laziness contract for [`ExifReader::parse_from`]: pulling EXIF out of a large file reads
//! only the metadata, never the image payload.
//!
//! This is the whole point of the streaming entry point — a raw `.NEF`/`.CR3` is hundreds of
//! megabytes whose EXIF is a few kilobytes near the front — and it is not visible from a test that
//! only checks the parsed values, because a reader that slurped the file whole would return exactly
//! the same [`Exif`](gamut_exif::Exif). It is pinned the way `gamut-ifd`'s own `tests/streaming.rs`
//! pins its "≤64 read bytes" contract: a counting [`ReadAt`] wrapper and a bound that a
//! payload-touching read would blow past by three orders of magnitude.

use gamut_core::Result;
use gamut_exif::ExifReader;
use gamut_ifd::{ByteOrder, Ifd, ReadAt, TiffFile, Value, Variant, write};

/// The `Exif\0\0` marker that precedes the TIFF stream in a JPEG `APP1` payload.
const MARKER: &[u8] = b"Exif\x00\x00";
/// `ExifIFD` pointer (Exif 3.0 §4.6.3).
const EXIF_IFD: u16 = 0x8769;
/// `GPSInfo` pointer.
const GPS_INFO: u16 = 0x8825;
/// `Interoperability` pointer.
const INTEROP_IFD: u16 = 0xA005;

/// Four megabytes: the file the metadata is embedded in.
const FILE_LEN: usize = 4 * 1024 * 1024;
/// Where the (nonexistent) strip data claims to start — well inside the padding.
const STRIP_AT: u32 = 1024 * 1024;

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

/// A marked EXIF blob — 0th IFD, Exif and GPS sub-IFDs, a nested Interop sub-IFD and a thumbnail
/// directory — followed by four megabytes of image payload the strip tags point into.
fn large_file_with_exif() -> Vec<u8> {
    let mut image = Ifd::new();
    image.set(0x010F, Value::Ascii("Canon".into())); // Make
    image.set(0x0110, Value::Ascii("EOS R5".into())); // Model
    image.set(0x0111, Value::Long(vec![STRIP_AT])); // StripOffsets, into the payload
    image.set(0x0117, Value::Long(vec![STRIP_AT])); // StripByteCounts

    let mut interop = Ifd::new();
    interop.set(0x0001, Value::Ascii("R98".into())); // InteroperabilityIndex

    let mut exif = Ifd::new();
    exif.set(0x829D, Value::Rational(vec![(28, 10)])); // FNumber
    exif.set(0x8827, Value::Short(vec![400])); // PhotographicSensitivity
    exif.set_sub_ifd(INTEROP_IFD, vec![interop]);

    let mut gps = Ifd::new();
    gps.set(0x0000, Value::Byte(vec![2, 3, 0, 0])); // GPSVersionID

    image.set_sub_ifd(EXIF_IFD, vec![exif]);
    image.set_sub_ifd(GPS_INFO, vec![gps]);

    let mut thumb = Ifd::new();
    thumb.set(0x0103, Value::Short(vec![6])); // Compression = JPEG

    let tiff = write(&TiffFile {
        order: ByteOrder::LittleEndian,
        variant: Variant::Classic,
        ifds: vec![image, thumb],
    })
    .expect("write");

    let mut data = MARKER.to_vec();
    data.extend(tiff);
    assert!(
        data.len() < STRIP_AT as usize,
        "the metadata must precede the payload"
    );
    data.resize(FILE_LEN, 0xAB);
    data
}

/// Extracting the EXIF of a four-megabyte file reads only the marker, the header, the directory
/// bodies and the values they reference — a bounded number of bytes that does not grow with the
/// file.
///
/// The source is passed as `&mut Counting<_>` rather than by value, which is also the contract that
/// `&mut S` is itself a [`ReadAt`] source: without it the counter would be moved into the reader
/// and unreadable afterwards.
#[test]
fn extracting_exif_from_a_large_file_never_reads_the_payload() {
    let data = large_file_with_exif();
    let mut counting = Counting {
        inner: &data[..],
        bytes_read: 0,
    };

    let exif = ExifReader::new()
        .parse_from(&mut counting)
        .expect("parse_from");

    // The whole metadata tree really was reached — a reader that gave up early would also be
    // "lazy", and this is what separates the two.
    assert_eq!(exif.make(), Some("Canon"));
    assert_eq!(exif.model(), Some("EOS R5"));
    assert_eq!(exif.iso(), Some(400));
    assert!(exif.gps_ifd().is_some(), "the GPS sub-IFD was followed");
    assert!(
        exif.interop_ifd().is_some(),
        "the Interop sub-IFD was followed"
    );
    assert!(exif.thumbnail().is_some(), "the 1st IFD was read");

    // 251 bytes today: the marker, the header, five directory bodies and their out-of-line
    // values. 512 leaves room for a tag or two without letting a megabyte through — four
    // megabytes is the failure mode a slurping reader would show.
    assert!(
        counting.bytes_read <= 512,
        "streaming parse read {} bytes of a {FILE_LEN}-byte file",
        counting.bytes_read
    );
}
