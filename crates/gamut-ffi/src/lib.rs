//! C-compatible FFI bindings for the gamut image codecs, so `gamut` can be called from C, C++,
//! Python, Go, and any other language with a C FFI, through its own C API (not a drop-in
//! replacement for an existing C image library's).
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
mod boundary;
#[cfg(any(
    feature = "avif",
    feature = "heic",
    feature = "jpeg",
    feature = "jxl",
    feature = "png",
    feature = "webp"
))]
pub use boundary::gamut_bytes_free;
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
