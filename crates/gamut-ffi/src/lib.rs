//! C-compatible FFI bindings for the gamut image codecs, so `gamut` can be used as a
//! drop-in replacement for traditional C image libraries from C, C++, Python, Go, and more.
//!
//! The surface implemented today is the **provider boundary** (issue #280): per-format opaque
//! handles with `gamut_<fmt>_<dir>_new`/`_free` lifecycles and `..._push_backend` entry points
//! that adopt a foreign [`gamut_codec_abi`] vtable backend into the format crate's push-order
//! backend registry — plus the first slice of the **consumer boundary** (issue #242): AVIF
//! encode, container parse, and the encoder's config setters (issue #666, the `avif` module). The
//! rest of the consumer boundary is issue #242. The full contract lives in this crate's
//! `DESIGN.md`.
//!
//! # Drift locking
//!
//! Every entry point is a *living shim*: a thin, fully typed call into the real `gamut::*`
//! Rust API, so drift in that API is a compile error in this crate rather than silent
//! C-surface skew. What a shim cannot express — permanent numeric values, struct layout —
//! is pinned by `const` asserts here and in the defining crates. `unsafe` is permitted in
//! this crate for the `extern "C"` boundary only.
#![deny(unsafe_op_in_unsafe_fn)]

// Compiles README.md's code blocks as doctests; exists only while rustdoc collects them.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;

mod status;

pub use status::{
    GAMUT_CODEC_ABI_VERSION, GAMUT_OK, GAMUT_STATUS_ABI_MISMATCH, GAMUT_STATUS_BUFFER_TOO_SMALL,
    GAMUT_STATUS_INVALID_INPUT, GAMUT_STATUS_IO, GAMUT_STATUS_NULL_ARGUMENT, GAMUT_STATUS_PANIC,
    GAMUT_STATUS_UNSUPPORTED, GamutStatus,
};

#[cfg(any(
    feature = "avif",
    feature = "heic",
    feature = "jpeg",
    feature = "jxl",
    feature = "png",
    feature = "webp"
))]
mod guard;
#[cfg(any(
    feature = "avif",
    feature = "heic",
    feature = "jpeg",
    feature = "jxl",
    feature = "png",
    feature = "webp"
))]
mod seam;
#[cfg(all(
    test,
    any(
        feature = "avif",
        feature = "heic",
        feature = "jpeg",
        feature = "jxl",
        feature = "png",
        feature = "webp"
    )
))]
mod test_support;

#[cfg(feature = "avif")]
pub mod avif;
#[cfg(feature = "heic")]
pub mod heic;
#[cfg(feature = "jpeg")]
pub mod jpeg;
#[cfg(feature = "jxl")]
pub mod jxl;
#[cfg(feature = "png")]
pub mod png;
#[cfg(feature = "webp")]
pub mod webp;

/// Frees a byte buffer the library returned through a `uint8_t **out, size_t *out_len` pair
/// (an encode output). No-op on `NULL`.
///
/// Allocator symmetry (`DESIGN.md`): Rust-allocated memory is released only here, never with
/// C's `free()`; and only Rust-allocated buffers are released here.
///
/// # Safety
///
/// `data` is `NULL`, or exactly a pointer the library wrote to an `out` parameter together with
/// the `len` it wrote to the matching `out_len`, not already freed.
// The only producer today is the AVIF encoder; the cfg widens as more encode entry points land.
#[cfg(feature = "avif")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gamut_bytes_free(data: *mut u8, len: usize) {
    guard::unit(|| {
        if !data.is_null() {
            // SAFETY: per this fn's contract `(data, len)` is exactly a `Box<[u8]>` that
            // `bytes_out` leaked, so rebuilding and dropping it frees it with its own layout.
            drop(unsafe { Box::from_raw(core::ptr::slice_from_raw_parts_mut(data, len)) });
        }
    });
}

/// Hands an owned buffer to C as a `(pointer, length)` pair that `gamut_bytes_free` releases.
///
/// The `Vec` is shrunk to a `Box<[u8]>` first, so the length alone recovers the allocation's
/// layout — C never has to carry a capacity.
#[cfg(feature = "avif")]
fn bytes_out(bytes: Vec<u8>) -> (*mut u8, usize) {
    let len = bytes.len();
    (Box::into_raw(bytes.into_boxed_slice()).cast::<u8>(), len)
}

/// Borrows a caller's `(const uint8_t *data, size_t len)` input pair. `NULL` with `len == 0` is
/// the empty slice (the idiomatic C spelling of "no bytes"); `NULL` with a non-zero length is
/// `None`, which callers report as `GAMUT_STATUS_NULL_ARGUMENT`.
///
/// # Safety
///
/// A non-`NULL` `data` points to `len` readable bytes that stay valid and unmodified for `'a`.
#[cfg(feature = "avif")]
unsafe fn bytes_in<'a>(data: *const u8, len: usize) -> Option<&'a [u8]> {
    if data.is_null() {
        return (len == 0).then_some(&[]);
    }
    // SAFETY: non-null, and `len` readable bytes valid for `'a` per this fn's contract; `u8`
    // has alignment 1.
    Some(unsafe { core::slice::from_raw_parts(data, len) })
}

/// Maps a Rust error onto its permanent C status code by [`gamut::core::Error::kind`], which
/// looks through any diagnostic context (`DESIGN.md`, Errors).
#[cfg(feature = "avif")]
fn status_of(error: &gamut::core::Error) -> GamutStatus {
    match error.kind() {
        gamut::core::ErrorKind::InvalidInput => GAMUT_STATUS_INVALID_INPUT,
        gamut::core::ErrorKind::Io => GAMUT_STATUS_IO,
        // `ErrorKind` is `#[non_exhaustive]`: a kind this library does not know yet is reported
        // as the one thing the C caller can act on — "not supported here".
        _ => GAMUT_STATUS_UNSUPPORTED,
    }
}

#[cfg(all(test, feature = "avif"))]
mod tests {
    use super::*;

    #[test]
    fn status_of_maps_each_error_kind_to_its_permanent_code() {
        use gamut::core::Error;
        assert_eq!(
            status_of(&Error::invalid_input("t", "m")),
            GAMUT_STATUS_INVALID_INPUT
        );
        assert_eq!(
            status_of(&Error::unsupported("t", "m")),
            GAMUT_STATUS_UNSUPPORTED
        );
        assert_eq!(
            status_of(&Error::Io(std::io::Error::other("m"))),
            GAMUT_STATUS_IO
        );
        // Classification looks through diagnostic context.
        assert_eq!(
            status_of(&Error::invalid_input("t", "m").with_byte_offset(9)),
            GAMUT_STATUS_INVALID_INPUT
        );
    }

    #[test]
    fn bytes_in_reads_null_with_zero_length_as_empty() {
        assert_eq!(unsafe { bytes_in(core::ptr::null(), 0) }, Some(&[][..]));
        assert_eq!(unsafe { bytes_in(core::ptr::null(), 1) }, None);
        let bytes = [1, 2, 3];
        assert_eq!(unsafe { bytes_in(bytes.as_ptr(), 2) }, Some(&bytes[..2]));
    }

    #[test]
    fn bytes_out_round_trips_through_gamut_bytes_free() {
        let (data, len) = bytes_out(vec![7; 5]);
        assert_eq!(unsafe { core::slice::from_raw_parts(data, len) }, [7; 5]);
        unsafe { gamut_bytes_free(data, len) };
        // An empty buffer is still a valid pair, and `NULL` is a no-op.
        let (data, len) = bytes_out(Vec::new());
        assert_eq!(len, 0);
        unsafe {
            gamut_bytes_free(data, len);
            gamut_bytes_free(core::ptr::null_mut(), 0);
        }
    }
}

/// The number of plane slots in a `GamutImageDesc` (the C spelling of
/// [`gamut_codec_abi::MAX_PLANES`]).
///
/// A literal so cbindgen can emit the `#define` that sizes the header's plane arrays; the
/// const assert below locks it to the seam's value.
pub const GAMUT_MAX_PLANES: usize = 4;

// The C header states GAMUT_CODEC_ABI_VERSION / GAMUT_MAX_PLANES and documents the backend
// Status contract in terms of these exact values. A `gamut-codec-abi` bump or Status change
// must fail this crate's build until the C contract is consciously revisited — header
// regenerated, status codes re-audited — rather than ship a header that lies.
const _: () = {
    assert!(gamut_codec_abi::ABI_VERSION == 1);
    assert!(GAMUT_MAX_PLANES == gamut_codec_abi::MAX_PLANES);
    assert!(gamut_codec_abi::Status::OK.0 == 0);
    assert!(gamut_codec_abi::Status::UNSUPPORTED.0 == -1);
};
