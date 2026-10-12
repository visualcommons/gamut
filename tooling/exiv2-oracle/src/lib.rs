//! Dev-only differential oracle around a vendored, statically-linked **exiv2** (with its bundled
//! Adobe XMP Toolkit, XMPCore) and the **expat** it depends on.
//!
//! gamut-xmp and gamut-exif must produce data the authoritative reference engine reads, and must
//! read what that engine writes. This crate wraps exiv2's in-memory parsers/serializers behind a
//! small, safe API: [`validate`], [`roundtrip`], [`get_property`], and [`property_count`] for XMP;
//! [`exif_count`], [`exif_get`], [`exif_print`], and [`exif_roundtrip`] for EXIF (a bare TIFF
//! stream, no `Exif\0\0` marker). All `unsafe` FFI is confined to this crate; inputs are
//! length-delimited byte slices, so no temporary files are used.

use std::ffi::CString;
use std::os::raw::{c_char, c_int};
use std::sync::{Mutex, MutexGuard, PoisonError};

/// Exiv2's `XmpParser` (Adobe XMPCore underneath) keeps global state and is documented as not
/// thread-safe; every FFI call is serialized behind this lock so parallel `cargo test` threads
/// cannot race it into spurious parse failures.
static XMP_LOCK: Mutex<()> = Mutex::new(());

/// Acquires the XMPCore serialization lock, ignoring poisoning (the lock guards no Rust data).
fn lock() -> MutexGuard<'static, ()> {
    XMP_LOCK.lock().unwrap_or_else(PoisonError::into_inner)
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
    fn exiv2_free(p: *mut c_char);
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

/// The signature shared by the shim's keyed EXIF readers (`exiv2_exif_get`, `exiv2_exif_print`).
type ExifReadFn = unsafe extern "C" fn(
    *const c_char,
    usize,
    *const c_char,
    *mut *mut c_char,
    *mut usize,
) -> c_int;

/// Calls one keyed EXIF reader of the shim and takes ownership of the string it returns.
fn exif_read(read: ExifReadFn, data: &[u8], key: &str) -> Result<String, String> {
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
}
