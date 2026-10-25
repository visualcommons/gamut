//! fuzz · robustness — `gamut_exif::ExifReader`, the entry point an embedded EXIF blob hits.
//!
//! `docs/testing.md`'s per-crate table names `parse` as this crate's untrusted-input surface. An
//! EXIF blob arrives from whatever file carried it — a JPEG `APP1`, a PNG `eXIf`, a WebP `EXIF`, a
//! HEIF item — and is a TIFF stream whose every offset and count the writer chose. The crate is
//! `#![forbid(unsafe_code)]`, so a hostile blob must end in a typed error or a lenient drop, never
//! a panic, a hang, or an allocation past libFuzzer's malloc limit.
//!
//! The reader is configured for its **widest** reach rather than its default:
//!
//! - `recover_entries(true)`, so an unreadable entry is skipped and the walk continues through the
//!   rest of its directory instead of stopping there;
//! - `audit_bytes(true)` through `parse_with_report`, which adds a second, whole-structure walk of
//!   the stream — every directory and value — that the default parse never makes.
//!
//! The marker stays optional (the default), so both a bare TIFF stream and an `Exif\0\0`-prefixed
//! one parse. On success the lazily-decoded views are driven too, since each re-reads bytes the
//! parse only located: the typed GPS view, the thumbnail, and the Canon MakerNote view, which walks
//! a vendor IFD at an offset the file chose.
//!
//! This target lists **no check beyond the crash oracle**, like `dng_decode`. A crash is minimised
//! and promoted into a named deterministic case in `gamut-exif`'s own suite.

#![no_main]

use gamut_exif::ExifReader;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let reader = ExifReader::new().recover_entries(true).audit_bytes(true);
    let Ok((exif, _report)) = reader.parse_with_report(data) else {
        return;
    };
    let _ = exif.gps();
    let _ = exif.thumbnail_bytes();
    if let (Some(note), Some(at)) = (exif.maker_note(), exif.maker_note_offset()) {
        let _ = note.canon(exif.byte_order(), at);
    }
});
