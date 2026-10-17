//! Dev-only differential oracle around a vendored, statically-linked **exiv2** (with its bundled
//! Adobe XMP Toolkit, XMPCore) and the **expat** it depends on.
//!
//! gamut-xmp and gamut-exif must produce data the authoritative reference engine reads, and must
//! read what that engine writes. This crate wraps exiv2's in-memory parsers/serializers behind a
//! small, safe API: [`validate`], [`roundtrip`], [`get_property`], and [`property_count`] for XMP;
//! [`exif_count`], [`exif_get`], [`exif_print`], and [`exif_roundtrip`] for EXIF (a bare TIFF
//! stream, no `Exif\0\0` marker).
//!
//! It also reads and writes metadata at the **container** level, over exiv2's `ImageFactory`
//! opening a whole image file (PNG, WebP, TIFF, AVIF/HEIF via BMFF, …) from memory:
//! [`image_exif_get`], [`image_xmp_get`] and [`image_icc`] check that exiv2 reads what a gamut
//! format crate embeds, and [`image_set`] has exiv2 embed EXIF/XMP so gamut can read it back
//! (exiv2's BMFF handler is read-only, so AVIF/HEIF is read-side only).
//!
//! All `unsafe` FFI is confined to this crate; inputs are length-delimited byte slices, so no
//! temporary files are used.

use std::ffi::CString;
use std::os::raw::{c_char, c_int};
use std::sync::{Mutex, MutexGuard, PoisonError};

/// Serializes every call into exiv2, the XMP and the EXIF entry points alike, so parallel
/// `cargo test` threads only ever reach the C++ library one at a time.
///
/// - **XMP:** exiv2's `XmpParser` (Adobe XMPCore underneath) keeps global state and is documented
///   as not thread-safe; unserialized, parallel threads race it into spurious parse failures.
/// - **EXIF:** unserialized, `gamut-exif`'s oracle suite segfaulted (SIGSEGV) inside exiv2 on CI
///   once it ran several `#[test]`s concurrently: one mutation shard's baseline crashed while
///   another shard's baseline of the same commit passed, so the failure is a race, not a bad
///   input. No stack trace has located the faulting line, so this lock is the conservative
///   answer to a nondeterministic crash under concurrency, not a proof that exiv2's TIFF path is
///   unsafe. Whoever shows that path *is* thread-safe and finds the crash elsewhere should record
///   it here and reopen the question (issue #536).
///
/// One lock for both halves costs nothing that matters: these are dev-only oracle calls, and
/// contention is negligible next to the C++ work each one does.
static EXIV2_LOCK: Mutex<()> = Mutex::new(());

/// Acquires the exiv2 serialization lock, ignoring poisoning (the lock guards no Rust data).
fn lock() -> MutexGuard<'static, ()> {
    EXIV2_LOCK.lock().unwrap_or_else(PoisonError::into_inner)
}

unsafe extern "C" {
    fn exiv2_xmp_validate(xmp: *const c_char, len: usize) -> c_int;
    fn exiv2_xmp_roundtrip(
        xmp: *const c_char,
        len: usize,
        out_buf: *mut *mut c_char,
        out_len: *mut usize,
    ) -> c_int;
    fn exiv2_xmp_get(
        xmp: *const c_char,
        len: usize,
        key: *const c_char,
        out_buf: *mut *mut c_char,
        out_len: *mut usize,
    ) -> c_int;
    fn exiv2_xmp_count(xmp: *const c_char, len: usize, out_count: *mut usize) -> c_int;
    fn exiv2_exif_count(data: *const c_char, len: usize, out_count: *mut usize) -> c_int;
    fn exiv2_exif_get(
        data: *const c_char,
        len: usize,
        key: *const c_char,
        out_buf: *mut *mut c_char,
        out_len: *mut usize,
    ) -> c_int;
    fn exiv2_exif_print(
        data: *const c_char,
        len: usize,
        key: *const c_char,
        out_buf: *mut *mut c_char,
        out_len: *mut usize,
    ) -> c_int;
    fn exiv2_exif_roundtrip(
        data: *const c_char,
        len: usize,
        out_buf: *mut *mut c_char,
        out_len: *mut usize,
    ) -> c_int;
    fn exiv2_image_exif_get(
        data: *const c_char,
        len: usize,
        key: *const c_char,
        out_buf: *mut *mut c_char,
        out_len: *mut usize,
    ) -> c_int;
    fn exiv2_image_xmp_get(
        data: *const c_char,
        len: usize,
        key: *const c_char,
        out_buf: *mut *mut c_char,
        out_len: *mut usize,
    ) -> c_int;
    fn exiv2_image_icc(
        data: *const c_char,
        len: usize,
        out_buf: *mut *mut c_char,
        out_len: *mut usize,
    ) -> c_int;
    fn exiv2_image_set(
        data: *const c_char,
        len: usize,
        exif_key: *const c_char,
        exif_value: *const c_char,
        xmp_key: *const c_char,
        xmp_value: *const c_char,
        out_buf: *mut *mut c_char,
        out_len: *mut usize,
    ) -> c_int;
    fn exiv2_last_error(out_buf: *mut *mut c_char, out_len: *mut usize) -> c_int;
    fn exiv2_free(p: *mut c_char);
}

/// The shim's return code for a container-level call whose image carries no ICC profile.
const NO_ICC: c_int = 5;

/// Formats a failing container-level call's error: `what`, the shim's code, and the message the
/// shim recorded. Must run under the same [`lock`] guard as the failing call.
fn image_error(what: &str, code: c_int) -> String {
    let mut buf: *mut c_char = std::ptr::null_mut();
    let mut len: usize = 0;
    // SAFETY: `buf`/`len` are written only on success.
    let message = if unsafe { exiv2_last_error(&mut buf, &mut len) } == 0 && !buf.is_null() {
        // SAFETY: on success the shim returned an owned buffer of `len` bytes.
        String::from_utf8_lossy(&unsafe { take_owned(buf, len) }).into_owned()
    } else {
        String::from("(no message)")
    };
    format!("{what} (code {code}): {message}")
}

/// Copies an output buffer the shim allocated into an owned `Vec`, then frees the C allocation.
///
/// # Safety
///
/// `buf` must be a non-null pointer to `len` bytes allocated by the shim (the success case of a
/// `*_roundtrip`/`*_get` call).
unsafe fn take_owned(buf: *mut c_char, len: usize) -> Vec<u8> {
    let bytes = unsafe { std::slice::from_raw_parts(buf as *const u8, len) }.to_vec();
    unsafe { exiv2_free(buf) };
    bytes
}

/// Returns `Ok(())` if exiv2 parses `xmp` without error.
///
/// # Errors
///
/// Returns a message if exiv2 rejects the packet.
pub fn validate(xmp: &[u8]) -> Result<(), String> {
    let _guard = lock();
    // SAFETY: `xmp` is a valid byte range; the shim copies it into a std::string.
    let code = unsafe { exiv2_xmp_validate(xmp.as_ptr().cast(), xmp.len()) };
    if code == 0 {
        Ok(())
    } else {
        Err(format!("exiv2 rejected the XMP packet (code {code})"))
    }
}

/// Parses `xmp` with exiv2's XMPCore and re-serializes it, returning exiv2's canonical bytes.
///
/// # Errors
///
/// Returns a message if exiv2 fails to parse or re-serialize the packet.
pub fn roundtrip(xmp: &[u8]) -> Result<Vec<u8>, String> {
    let _guard = lock();
    let mut buf: *mut c_char = std::ptr::null_mut();
    let mut len: usize = 0;
    // SAFETY: `xmp` is valid; `buf`/`len` are written only on success.
    let code = unsafe { exiv2_xmp_roundtrip(xmp.as_ptr().cast(), xmp.len(), &mut buf, &mut len) };
    if code != 0 || buf.is_null() {
        return Err(format!("exiv2 round-trip failed (code {code})"));
    }
    // SAFETY: on success the shim returned an owned buffer of `len` bytes.
    Ok(unsafe { take_owned(buf, len) })
}

/// Reads one property's serialized value by exiv2 key (e.g. `"Xmp.dc.format"`).
///
/// # Errors
///
/// Returns a message if the key contains a NUL byte, exiv2 cannot parse the packet, or the property
/// is absent.
pub fn get_property(xmp: &[u8], key: &str) -> Result<String, String> {
    let _guard = lock();
    let ckey = CString::new(key).map_err(|e| e.to_string())?;
    let mut buf: *mut c_char = std::ptr::null_mut();
    let mut len: usize = 0;
    // SAFETY: `xmp` and `ckey` are valid; `buf`/`len` are written only on success.
    let code = unsafe {
        exiv2_xmp_get(
            xmp.as_ptr().cast(),
            xmp.len(),
            ckey.as_ptr(),
            &mut buf,
            &mut len,
        )
    };
    if code != 0 || buf.is_null() {
        return Err(format!("exiv2 could not read '{key}' (code {code})"));
    }
    // SAFETY: on success the shim returned an owned buffer of `len` bytes.
    let bytes = unsafe { take_owned(buf, len) };
    String::from_utf8(bytes).map_err(|e| e.to_string())
}

/// Returns the number of XMP properties exiv2 parses from `xmp`.
///
/// # Errors
///
/// Returns a message if exiv2 cannot parse the packet.
pub fn property_count(xmp: &[u8]) -> Result<usize, String> {
    let _guard = lock();
    let mut count: usize = 0;
    // SAFETY: `xmp` is valid; `count` is written only on success.
    let code = unsafe { exiv2_xmp_count(xmp.as_ptr().cast(), xmp.len(), &mut count) };
    if code == 0 {
        Ok(count)
    } else {
        Err(format!("exiv2 could not count properties (code {code})"))
    }
}

/// Returns the number of EXIF tags exiv2 parses from a bare TIFF stream `data`.
///
/// # Errors
///
/// Returns a message if exiv2 rejects the stream.
pub fn exif_count(data: &[u8]) -> Result<usize, String> {
    let _guard = lock();
    let mut count: usize = 0;
    // SAFETY: `data` is a valid byte range; `count` is written only on success.
    let code = unsafe { exiv2_exif_count(data.as_ptr().cast(), data.len(), &mut count) };
    if code == 0 {
        Ok(count)
    } else {
        Err(format!(
            "exiv2 could not decode the EXIF stream (code {code})"
        ))
    }
}

/// Reads one EXIF tag's serialized value by exiv2 key (e.g. `"Exif.Image.Make"`).
///
/// # Errors
///
/// Returns a message if the key contains a NUL byte, exiv2 cannot decode the stream, or the tag is
/// absent.
pub fn exif_get(data: &[u8], key: &str) -> Result<String, String> {
    exif_read(exiv2_exif_get, data, key)
}

/// Reads one EXIF tag's **interpreted** value by exiv2 key — exiv2's `Exifdatum::print`, the
/// human-readable rendering (`"Exif.Image.Orientation"` = 1 reads as `"top, left"`), where
/// [`exif_get`] returns the raw value (`"1"`).
///
/// A value exiv2 has no interpretation for is rendered as its raw value in parentheses (`"(9)"`);
/// that is exiv2's own convention, passed through unchanged.
///
/// # Errors
///
/// Returns a message if the key contains a NUL byte, exiv2 cannot decode the stream, or the tag is
/// absent.
pub fn exif_print(data: &[u8], key: &str) -> Result<String, String> {
    exif_read(exiv2_exif_print, data, key)
}

/// The signature shared by the shim's keyed readers: the bare-stream EXIF ones (`exiv2_exif_get`,
/// `exiv2_exif_print`) and the container-level ones (`exiv2_image_exif_get`,
/// `exiv2_image_xmp_get`).
type ExifReadFn = unsafe extern "C" fn(
    *const c_char,
    usize,
    *const c_char,
    *mut *mut c_char,
    *mut usize,
) -> c_int;

/// Calls one keyed EXIF reader of the shim and takes ownership of the string it returns.
fn exif_read(read: ExifReadFn, data: &[u8], key: &str) -> Result<String, String> {
    let _guard = lock();
    let ckey = CString::new(key).map_err(|e| e.to_string())?;
    let mut buf: *mut c_char = std::ptr::null_mut();
    let mut len: usize = 0;
    // SAFETY: `read` is one of the shim's keyed readers; `data` and `ckey` are valid; `buf`/`len`
    // are written only on success.
    let code = unsafe {
        read(
            data.as_ptr().cast(),
            data.len(),
            ckey.as_ptr(),
            &mut buf,
            &mut len,
        )
    };
    if code != 0 || buf.is_null() {
        return Err(format!("exiv2 could not read '{key}' (code {code})"));
    }
    // SAFETY: on success the shim returned an owned buffer of `len` bytes.
    let bytes = unsafe { take_owned(buf, len) };
    String::from_utf8(bytes).map_err(|e| e.to_string())
}

/// Decodes then re-encodes the EXIF stream via exiv2, returning its canonical bare TIFF bytes.
///
/// # Errors
///
/// Returns a message if exiv2 fails to decode or re-encode the stream.
pub fn exif_roundtrip(data: &[u8]) -> Result<Vec<u8>, String> {
    let _guard = lock();
    let mut buf: *mut c_char = std::ptr::null_mut();
    let mut len: usize = 0;
    // SAFETY: `data` is valid; `buf`/`len` are written only on success.
    let code =
        unsafe { exiv2_exif_roundtrip(data.as_ptr().cast(), data.len(), &mut buf, &mut len) };
    if code != 0 || buf.is_null() {
        return Err(format!("exiv2 EXIF round-trip failed (code {code})"));
    }
    // SAFETY: on success the shim returned an owned buffer of `len` bytes.
    Ok(unsafe { take_owned(buf, len) })
}

/// Reads one EXIF tag's raw value (exiv2's `toString`) by key (e.g.
/// `"Exif.Photo.ISOSpeedRatings"`) from a whole image file — PNG, WebP, TIFF, AVIF/HEIF (BMFF),
/// JPEG, … — that exiv2's `ImageFactory` opens and reads from memory.
///
/// # Errors
///
/// Returns a message if the key contains a NUL byte, exiv2 does not recognise or cannot read the
/// container, or the tag is absent.
pub fn image_exif_get(data: &[u8], key: &str) -> Result<String, String> {
    image_read(exiv2_image_exif_get, data, key)
}

/// Reads one XMP property's value (exiv2's `toString`) by key (e.g. `"Xmp.xmp.CreatorTool"`) from
/// a whole image file that exiv2's `ImageFactory` opens and reads from memory.
///
/// # Errors
///
/// Returns a message if the key contains a NUL byte, exiv2 does not recognise or cannot read the
/// container, or the property is absent.
pub fn image_xmp_get(data: &[u8], key: &str) -> Result<String, String> {
    image_read(exiv2_image_xmp_get, data, key)
}

/// Calls one keyed container-level reader of the shim and takes ownership of the string it returns.
fn image_read(read: ExifReadFn, data: &[u8], key: &str) -> Result<String, String> {
    let _guard = lock();
    let ckey = CString::new(key).map_err(|e| e.to_string())?;
    let mut buf: *mut c_char = std::ptr::null_mut();
    let mut len: usize = 0;
    // SAFETY: `read` is one of the shim's keyed container readers; `data` and `ckey` are valid;
    // `buf`/`len` are written only on success.
    let code = unsafe {
        read(
            data.as_ptr().cast(),
            data.len(),
            ckey.as_ptr(),
            &mut buf,
            &mut len,
        )
    };
    if code != 0 || buf.is_null() {
        return Err(image_error(&format!("exiv2 could not read '{key}'"), code));
    }
    // SAFETY: on success the shim returned an owned buffer of `len` bytes.
    let bytes = unsafe { take_owned(buf, len) };
    String::from_utf8(bytes).map_err(|e| e.to_string())
}

/// Returns the raw ICC profile exiv2 reads from a whole image file (PNG `iCCP` inflated, WebP
/// `ICCP`, TIFF tag 34675, BMFF `colr` `prof`/`rICC`), or `Ok(None)` when the image carries none.
///
/// # Errors
///
/// Returns a message if exiv2 does not recognise or cannot read the container.
pub fn image_icc(data: &[u8]) -> Result<Option<Vec<u8>>, String> {
    let _guard = lock();
    let mut buf: *mut c_char = std::ptr::null_mut();
    let mut len: usize = 0;
    // SAFETY: `data` is valid; `buf`/`len` are written only on success.
    let code = unsafe { exiv2_image_icc(data.as_ptr().cast(), data.len(), &mut buf, &mut len) };
    if code == NO_ICC {
        return Ok(None);
    }
    if code != 0 || buf.is_null() {
        return Err(image_error("exiv2 could not read the ICC profile", code));
    }
    // SAFETY: on success the shim returned an owned buffer of `len` bytes.
    Ok(Some(unsafe { take_owned(buf, len) }))
}

/// Rewrites a whole image file through exiv2: opens it from memory, reads its metadata, sets the
/// EXIF tag `exif = (key, value)` and/or the XMP property `xmp = (key, value)` (each skipped when
/// `None`; values are parsed by exiv2 from their string form), runs `writeMetadata`, and returns
/// the complete rewritten file.
///
/// # Errors
///
/// Returns a message if a key or value contains a NUL byte, exiv2 does not recognise or cannot
/// read the container, the format is not read-write in exiv2 for a requested metadata kind (BMFF,
/// so AVIF/HEIF, is read-only; WebP is read-write for both), or the write itself fails.
pub fn image_set(
    data: &[u8],
    exif: Option<(&str, &str)>,
    xmp: Option<(&str, &str)>,
) -> Result<Vec<u8>, String> {
    let _guard = lock();
    let cpair = |pair: Option<(&str, &str)>| -> Result<Option<(CString, CString)>, String> {
        pair.map(|(k, v)| {
            Ok((
                CString::new(k).map_err(|e| e.to_string())?,
                CString::new(v).map_err(|e| e.to_string())?,
            ))
        })
        .transpose()
    };
    let exif = cpair(exif)?;
    let xmp = cpair(xmp)?;
    let ptrs = |pair: &Option<(CString, CString)>| {
        pair.as_ref()
            .map_or((std::ptr::null(), std::ptr::null()), |(k, v)| {
                (k.as_ptr(), v.as_ptr())
            })
    };
    let (exif_key, exif_value) = ptrs(&exif);
    let (xmp_key, xmp_value) = ptrs(&xmp);
    let mut buf: *mut c_char = std::ptr::null_mut();
    let mut len: usize = 0;
    // SAFETY: `data` is valid; each key/value is null or a NUL-terminated string kept alive by
    // `exif`/`xmp` for the call; `buf`/`len` are written only on success.
    let code = unsafe {
        exiv2_image_set(
            data.as_ptr().cast(),
            data.len(),
            exif_key,
            exif_value,
            xmp_key,
            xmp_value,
            &mut buf,
            &mut len,
        )
    };
    if code != 0 || buf.is_null() {
        return Err(image_error("exiv2 could not rewrite the image", code));
    }
    // SAFETY: on success the shim returned an owned buffer of `len` bytes.
    Ok(unsafe { take_owned(buf, len) })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"><rdf:RDF \
        xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\" \
        xmlns:dc=\"http://purl.org/dc/elements/1.1/\"><rdf:Description rdf:about=\"\">\
        <dc:format>text/plain</dc:format></rdf:Description></rdf:RDF></x:xmpmeta>";

    #[test]
    fn reads_a_valid_packet() {
        assert!(validate(SAMPLE.as_bytes()).is_ok());
        assert_eq!(property_count(SAMPLE.as_bytes()).unwrap(), 1);
        assert_eq!(
            get_property(SAMPLE.as_bytes(), "Xmp.dc.format").unwrap(),
            "text/plain"
        );
        assert!(!roundtrip(SAMPLE.as_bytes()).unwrap().is_empty());
    }

    #[test]
    fn rejects_non_xmp_bytes() {
        assert!(validate(b"not xmp at all").is_err());
    }

    /// A minimal little-endian baseline TIFF: one 1x1 8-bit grey strip, IFD0 at offset 8 with
    /// seven entries, the pixel byte after the IFD.
    fn tiny_tiff() -> Vec<u8> {
        // (tag, type SHORT=3 / LONG=4, value)
        const PIXEL_OFFSET: u32 = 8 + 2 + 7 * 12 + 4;
        let entries: [(u16, u16, u32); 7] = [
            (256, 3, 1),            // ImageWidth
            (257, 3, 1),            // ImageLength
            (258, 3, 8),            // BitsPerSample
            (262, 3, 1),            // PhotometricInterpretation = BlackIsZero
            (273, 4, PIXEL_OFFSET), // StripOffsets
            (278, 3, 1),            // RowsPerStrip
            (279, 4, 1),            // StripByteCounts
        ];
        let mut out = b"II*\0".to_vec();
        out.extend_from_slice(&8u32.to_le_bytes());
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        for (tag, ty, value) in entries {
            out.extend_from_slice(&tag.to_le_bytes());
            out.extend_from_slice(&ty.to_le_bytes());
            out.extend_from_slice(&1u32.to_le_bytes());
            if ty == 3 {
                out.extend_from_slice(&(value as u16).to_le_bytes());
                out.extend_from_slice(&[0, 0]);
            } else {
                out.extend_from_slice(&value.to_le_bytes());
            }
        }
        out.extend_from_slice(&0u32.to_le_bytes()); // no next IFD
        assert_eq!(out.len() as u32, PIXEL_OFFSET);
        out.push(0x80);
        out
    }

    #[test]
    fn image_set_then_get_round_trips_through_a_tiff_container() {
        let tiff = tiny_tiff();
        assert!(image_exif_get(&tiff, "Exif.Photo.ISOSpeedRatings").is_err());
        assert_eq!(image_icc(&tiff).unwrap(), None);
        let written = image_set(
            &tiff,
            Some(("Exif.Photo.ISOSpeedRatings", "400")),
            Some(("Xmp.xmp.CreatorTool", "gamut")),
        )
        .unwrap();
        assert_eq!(
            image_exif_get(&written, "Exif.Photo.ISOSpeedRatings").unwrap(),
            "400"
        );
        assert_eq!(
            image_xmp_get(&written, "Xmp.xmp.CreatorTool").unwrap(),
            "gamut"
        );
    }

    /// A minimal 1x1 8-bit greyscale PNG: IHDR, one stored-deflate IDAT, IEND.
    fn tiny_png() -> Vec<u8> {
        fn crc32(bytes: &[u8]) -> u32 {
            let mut crc = !0u32;
            for &b in bytes {
                crc ^= u32::from(b);
                for _ in 0..8 {
                    crc = if crc & 1 == 1 {
                        (crc >> 1) ^ 0xEDB8_8320
                    } else {
                        crc >> 1
                    };
                }
            }
            !crc
        }
        fn chunk(out: &mut Vec<u8>, ty: &[u8; 4], data: &[u8]) {
            out.extend_from_slice(&(data.len() as u32).to_be_bytes());
            let start = out.len();
            out.extend_from_slice(ty);
            out.extend_from_slice(data);
            let crc = crc32(&out[start..]);
            out.extend_from_slice(&crc.to_be_bytes());
        }
        let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
        // width 1, height 1, depth 8, colour type 0, compression/filter/interlace 0
        chunk(&mut out, b"IHDR", &[0, 0, 0, 1, 0, 0, 0, 1, 8, 0, 0, 0, 0]);
        // zlib header, one final stored block of [filter 0, pixel 0x80], Adler-32 0x00820081
        let idat = [
            0x78, 0x01, 0x01, 0x02, 0x00, 0xFD, 0xFF, 0x00, 0x80, 0x00, 0x82, 0x00, 0x81,
        ];
        chunk(&mut out, b"IDAT", &idat);
        chunk(&mut out, b"IEND", &[]);
        out
    }

    #[test]
    fn image_set_writes_exif_into_a_png_exif_chunk() {
        let written = image_set(
            &tiny_png(),
            Some(("Exif.Photo.ISOSpeedRatings", "400")),
            Some(("Xmp.xmp.CreatorTool", "gamut")),
        )
        .unwrap();
        assert!(written.windows(4).any(|w| w == b"eXIf"));
        assert_eq!(
            image_exif_get(&written, "Exif.Photo.ISOSpeedRatings").unwrap(),
            "400"
        );
        assert_eq!(
            image_xmp_get(&written, "Xmp.xmp.CreatorTool").unwrap(),
            "gamut"
        );
        assert_eq!(image_icc(&written).unwrap(), None);
    }

    #[test]
    fn image_readers_report_exiv2s_message_for_an_unknown_container() {
        let err = image_exif_get(b"not an image", "Exif.Image.Make").unwrap_err();
        assert!(err.contains("code 1"), "{err}");
        assert!(!err.ends_with("(no message)"), "{err}");
    }
}
