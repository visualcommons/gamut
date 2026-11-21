//! The C boundary status contract: `gamut_status_t` (the C spelling of [`GamutStatus`]) and
//! its permanent code values.
//!
//! Distinct from the backend-seam status ([`gamut_codec_abi::Status`], `GamutAbiStatus` in the
//! header): that type belongs to *backends* and its `0` / `-1` values carry the registry
//! fall-through contract, while these codes are what the library's own entry points return to
//! the C caller.

/// A `gamut` entry-point result (`gamut_status_t` in the C header): [`GAMUT_OK`] is success,
/// every other value is one of the permanent `GAMUT_STATUS_*` codes below.
pub type GamutStatus = i32;

/// Success.
pub const GAMUT_OK: GamutStatus = 0;
/// The operation rejected its input (the C mapping of `gamut_core::ErrorKind::InvalidInput`).
pub const GAMUT_STATUS_INVALID_INPUT: GamutStatus = 1;
/// The operation is not supported (the C mapping of `gamut_core::ErrorKind::Unsupported`).
pub const GAMUT_STATUS_UNSUPPORTED: GamutStatus = 2;
/// An I/O error (the C mapping of `gamut_core::ErrorKind::Io`).
pub const GAMUT_STATUS_IO: GamutStatus = 3;
/// A required pointer argument was `NULL`. Boundary-only: never maps a Rust error.
pub const GAMUT_STATUS_NULL_ARGUMENT: GamutStatus = 4;
/// The call panicked inside the library; the panic was contained at the boundary and nothing
/// was unwound into the caller. Boundary-only.
pub const GAMUT_STATUS_PANIC: GamutStatus = 5;
/// A caller-supplied buffer is too small. Boundary-only; reserved for the consumer entry
/// points of issue #242.
pub const GAMUT_STATUS_BUFFER_TOO_SMALL: GamutStatus = 6;
/// The pushed vtable's `abi_version` is not this library's [`GAMUT_CODEC_ABI_VERSION`] — the
/// backend was built against another generation of the seam and must be rebuilt. The caller
/// retains ownership of its `ctx`; no `destroy` has run. Boundary-only, and deliberately
/// distinct from [`GAMUT_STATUS_UNSUPPORTED`]: version skew asks for a rebuild, a declining
/// backend asks for a fallback.
pub const GAMUT_STATUS_ABI_MISMATCH: GamutStatus = 7;

/// The `gamut-codec-abi` seam revision this library was built against. A pushed vtable's
/// `abi_version` field must equal this value or `push_backend` returns
/// [`GAMUT_STATUS_ABI_MISMATCH`].
///
/// A literal (not `gamut_codec_abi::ABI_VERSION`) so cbindgen can emit it; the const assert
/// below locks the two together, so a seam bump fails this build until the value — and the
/// header — are consciously updated.
pub const GAMUT_CODEC_ABI_VERSION: u32 = 2;

// Permanent, append-only C ABI values (see DESIGN.md) — a change here is an ABI break, not a
// refactor, and fails the build.
const _: () = {
    assert!(GAMUT_OK == 0);
    assert!(GAMUT_STATUS_INVALID_INPUT == 1);
    assert!(GAMUT_STATUS_UNSUPPORTED == 2);
    assert!(GAMUT_STATUS_IO == 3);
    assert!(GAMUT_STATUS_NULL_ARGUMENT == 4);
    assert!(GAMUT_STATUS_PANIC == 5);
    assert!(GAMUT_STATUS_BUFFER_TOO_SMALL == 6);
    assert!(GAMUT_STATUS_ABI_MISMATCH == 7);
    assert!(GAMUT_CODEC_ABI_VERSION == gamut_codec_abi::ABI_VERSION);
};
