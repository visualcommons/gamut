//! The real-camera half of `gamut-exif`'s laziness contract (gamut issue #521).
//!
//! `crates/gamut-exif/tests/streaming.rs` pins that pulling EXIF out of a large *synthetic* TIFF
//! reads only the metadata. That fixture is shaped by its author; what it cannot show is that real
//! firmware's layout — MakerNotes, vendor preambles, previews, raw payloads hundreds of times the
//! size of the directories — keeps the read bounded too. A DNG is a TIFF stream from byte 0, so it
//! is an EXIF blob in the sense `ExifReader::parse_from` reads: these files are the corpus for it.
//!
//! The source is the file on disk behind a counting [`ReadAt`], never a slice of it, so the count
//! is exactly the I/O `parse_from` asks for.

use std::fs::File;

use gamut_core::Result;
use gamut_dng_real_conformance::{corpus_dir, manifest};
use gamut_exif::ExifReader;
use gamut_ifd::{ReadAt, StreamSource};

/// A [`ReadAt`] wrapper that counts the bytes fetched.
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

/// The most any corpus file's EXIF parse may read.
///
/// Measured when this test landed: 5 256 to 7 291 bytes for the camera-written files, 197 487 to
/// 197 607 for the three Adobe-converted Canon files. The converter's 0th IFD carries ~190 KB of
/// out-of-line values the EXIF model keeps verbatim — `DNGPrivateData` (the original MakerNote,
/// 68 560 bytes), two `ProfileHueSatMapData` tables and a `ProfileLookTableData` (32 400, 32 400
/// and 55 296 bytes) and the XMP packet — so that is metadata, not payload. The bound sits above
/// the largest with headroom and stays more than twenty times below the smallest file (6.2 MB):
/// a parse that touched a strip or tile would cross it at once.
const MAX_READ: u64 = 256 * 1024;

/// The bound as a fraction of the file, so a small corpus file cannot pass by being mostly
/// metadata: no parse may read more than a sixteenth of what it parses.
const MAX_FRACTION_DENOMINATOR: u64 = 16;

/// Every corpus file's EXIF parses through a file-backed source while reading a small, bounded
/// fraction of the file: the directories and the values the model keeps, never the image data.
#[test]
fn exif_from_every_real_dng_reads_only_its_metadata() {
    let manifest = manifest();
    assert_eq!(manifest.samples.len(), 6, "the corpus lost files");
    for sample in &manifest.samples {
        let name = &sample.path;
        let path = corpus_dir().join(name);
        let mut file = File::open(&path).unwrap_or_else(|e| panic!("open {}: {e}", path.display()));
        let file_len = file.metadata().expect("metadata").len();
        let mut source = Counting {
            inner: StreamSource::new(&mut file),
            bytes_read: 0,
        };
        let exif = ExifReader::new()
            .parse_from(&mut source)
            .unwrap_or_else(|e| panic!("{name}: parse_from: {e}"));
        assert!(
            exif.make().is_some(),
            "{name}: no Make — the parse did not read the 0th IFD it bounds"
        );
        let read = source.bytes_read;
        assert!(
            read <= MAX_READ,
            "{name}: read {read} bytes of {file_len}, over the {MAX_READ}-byte bound"
        );
        assert!(
            read * MAX_FRACTION_DENOMINATOR <= file_len,
            "{name}: read {read} bytes, more than 1/{MAX_FRACTION_DENOMINATOR} of {file_len}"
        );
    }
}
