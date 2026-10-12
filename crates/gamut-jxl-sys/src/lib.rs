//! Hand-written FFI declarations for the reference **libjxl v0.12.0**, statically built by this
//! crate's `build.rs` (via the BSD-3-Clause [`jpegxl-src`] crate). This is the native foundation of
//! [`gamut-jxl`]'s JPEG XL *encoder*: libjxl is the ISO/IEC 18181 reference implementation and the
//! only reference-quality JXL encoder available to Rust.
//!
//! This crate contains **declarations only** — `#[repr(C)]` types, constants, and `unsafe extern
//! "C"` function signatures — and no function bodies of its own. That keeps it free of any coverage
//! regions or mutation targets; the *safe* wrapper logic and all error handling live in
//! [`gamut-jxl`]. The declared surface is deliberately pruned to the subset the wrapper uses, plus a
//! decode subset (see [`decode`]) that serves as [`gamut-jxl`]'s differential-test *oracle* — the
//! same static archive contains both halves, and the linker strips whatever is unused.
//!
//! # Layout & ABI
//!
//! - [`types`] — the shared `#[repr(C)]` structs and enums ([`JxlBasicInfo`], [`JxlColorEncoding`],
//!   [`JxlPixelFormat`], …). C enums are `int`, so every enum here is a `#[repr(transparent)]`
//!   newtype over [`core::ffi::c_int`] with associated constants rather than a Rust `enum`. This is
//!   ABI-safe in *both* directions: a value libjxl returns that is outside the known set (e.g. a
//!   future status code) is representable and never triggers the undefined behaviour a `#[repr(i32)]`
//!   Rust `enum` would on an unlisted discriminant.
//! - [`encode`] — the encoder API subset ([`encode::JxlEncoder`] and friends).
//! - [`decode`] — the decoder API subset used only as the differential-test oracle.
//!
//! # Safety
//!
//! Every declared function is `unsafe` to call: correctness depends on upholding libjxl's contracts
//! (valid pointers, correct call ordering, buffer sizes). Callers — in practice [`gamut-jxl`]'s FFI
//! module — are responsible for those invariants; each declaration documents the relevant ones.
//!
//! # Building without a C toolchain
//!
//! Setting the environment variable `GAMUT_JXL_SYS_SKIP_NATIVE=1` skips the libjxl static build and
//! emits no link directives. This is a `cargo check`-only escape hatch (checking compiles but never
//! links) for cross-compile / MSRV verification boxes that lack cmake or a cross C++ toolchain. Do
//! not set it for builds that actually link (tests, binaries).
//!
//! # WebAssembly
//!
//! On **`wasm32-unknown-emscripten`** the build drives the same vendored libjxl through the emsdk
//! toolchain (requires a recent `emcc` on `PATH`; libjxl officially supports wasm via
//! emscripten), so the full encoder works there. Final binaries should link with
//! `-C link-arg=-sALLOW_MEMORY_GROWTH=1`: rustc's default fixed 16 MiB emscripten heap (with
//! `-sABORTING_MALLOC=0`) is too small for libjxl's larger allocations. On every other `wasm32`
//! target (`wasm32-unknown-unknown`, `wasm32-wasip*`) the native build is skipped
//! unconditionally: no C/C++ toolchain can produce archives linkable into those ABIs — an
//! upstream toolchain boundary, not a build-system gap — and `gamut-jxl` compiles its encoder out
//! there. `GAMUT_JXL_SYS_LIBJXL_DIR` overrides where the emscripten lane finds the vendored
//! libjxl source (for `cargo vendor` layouts).
//!
//! # Licensing
//!
//! This crate statically links libjxl and its bundled third-party libraries under a mix of
//! permissive licenses. See the crate `README.md` for the full static-linking licensing notice.
//!
//! [`jpegxl-src`]: https://crates.io/crates/jpegxl-src
//! [`gamut-jxl`]: https://crates.io/crates/gamut-jxl
//! [`JxlBasicInfo`]: types::JxlBasicInfo
//! [`JxlColorEncoding`]: types::JxlColorEncoding
//! [`JxlPixelFormat`]: types::JxlPixelFormat

// Compiles README.md's code blocks as doctests; exists only while rustdoc collects them.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;

pub mod decode;
pub mod encode;
pub mod types;
