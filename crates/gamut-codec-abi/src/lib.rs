//! The shared codestream-backend seam: one `repr(C)` vtable shape (plus its object-safe Rust
//! "twin" traits) that lets a foreign (C/FFI) or alternate codestream backend plug into any gamut
//! format crate, together with the fallback contract that governs how a host selects among them.
//!
//! A gamut format crate (avif, heic, the `av2`/`vvc` stubs once implemented, …) owns the
//! *container* and everything around the coded picture, but the codestream encode/decode itself is
//! often better served by a platform or reference codec. This crate defines the **call shape** and
//! the **fallback contract** for that seam so every format crate wires backends the same way:
//!
//! - Backends are held in a host-owned registry, tried in **push order**; a software fallback (when
//!   a crate ships one) is the implicit tail, tried last.
//! - [`Decoder::supports`] / [`Encoder::supports`] returning `false` (or the C
//!   [`Status::UNSUPPORTED`]) is the **only** signal that lets the host fall through to the next
//!   backend. Any other outcome is terminal.
//! - A backend that *accepts* a job ([`supports`](Decoder::supports) → `true`) and then fails
//!   returns a non-OK [`Status`], which propagates to the caller — the host does **not** retry a
//!   later backend, because a partially-produced result must not be silently masked.
//!
//! # Two mirrored surfaces
//!
//! Every capability exists twice, 1:1:
//!
//! - **Rust twins** — the object-safe [`Decoder`] / [`Encoder`] traits. A pure-Rust backend
//!   implements these directly; a format crate consumes `&mut dyn Decoder` / `&mut dyn Encoder`.
//! - **`repr(C)` vtables** — [`DecoderVTable`] / [`EncoderVTable`]: a function-pointer table plus an
//!   opaque `ctx` pointer, the shape a C or `-sys` backend exposes. Leading each vtable is an
//!   [`abi_version`](DecoderVTable::abi_version) equal to [`ABI_VERSION`]; leading each descriptor
//!   is a [`struct_size`](StreamConfig::struct_size) for forward-compatible field growth.
//!
//! The [`bridge`] module converts between the two: [`bridge::ForeignDecoder`] /
//! [`bridge::ForeignEncoder`] adapt a C vtable *into* a Rust twin (C → Rust), and
//! [`bridge::lower_decoder`] / [`bridge::lower_encoder`] expose a boxed Rust twin *as* a C vtable
//! (Rust → C). Those adapters are the crate's only `unsafe`.
//!
//! # Scope and design notes
//!
//! - **`#![no_std]`, dependency-free.** The seam is pure interface: primitive fields, raw pointers,
//!   and function pointers only. `alloc` is pulled in solely for the `Box`-based [`bridge`] lowering.
//! - **No `Send` supertrait.** [`Decoder`] / [`Encoder`] are deliberately *not* `Send`; a host binds
//!   `Send` at the point it inserts a backend into a registry, so single-threaded backends stay
//!   usable. The C → Rust adapters offer an `unsafe` constructor by which the caller *asserts*
//!   thread-safety, at which point the adapter is `Send`.
//! - **C-friendly plain data.** Descriptors carry `u32` ids/dimensions and raw pointer + length
//!   pairs; they map onto a C struct with no translation layer, matching the
//!   `gamut_heic::HevcDecoder` precedent (single object-safe method, borrowed bytes in, owned plain
//!   data out).
#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

// Compiles README.md's code blocks as doctests; exists only while rustdoc collects them.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;

extern crate alloc;

pub mod bridge;

use core::ffi::c_void;

/// The ABI revision this crate defines.
///
/// Every [`DecoderVTable`] / [`EncoderVTable`] leads with an `abi_version` field; a host (and the
/// [`bridge`] adapters) accept a vtable only when its `abi_version` equals this constant. Bump it
/// whenever the vtable or descriptor layout changes incompatibly.
pub const ABI_VERSION: u32 = 1;

/// The maximum number of image planes a [`ImageDesc`] carries.
///
/// Four covers every gamut pixel layout (interleaved RGBA is one plane; the widest planar YCbCr +
/// alpha case is four). Fixed so [`ImageDesc`] stays a flat `repr(C)` struct with no indirection.
pub const MAX_PLANES: usize = 4;

/// A backend result code, `repr(transparent)` over `i32` so it round-trips a C `int`.
///
/// `0` ([`Status::OK`]) is success. `-1` ([`Status::UNSUPPORTED`]) is the *fall-through* code: it is
/// the only value that tells a host to try the next backend in its registry. Any other value is a
/// terminal backend-specific error that propagates to the caller unchanged.
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Status(pub i32);

impl Status {
    /// Success (`0`).
    pub const OK: Status = Status(0);
    /// The backend cannot handle this job (`-1`); the host falls back to the next backend.
    pub const UNSUPPORTED: Status = Status(-1);

    /// Returns `true` iff this is [`Status::OK`].
    #[must_use]
    pub const fn is_ok(self) -> bool {
        self.0 == Status::OK.0
    }

    /// Returns `true` iff this is [`Status::UNSUPPORTED`] — the fall-through code.
    #[must_use]
    pub const fn is_unsupported(self) -> bool {
        self.0 == Status::UNSUPPORTED.0
    }
}

/// Describes a coded stream to a decoder backend: the codec, the coded dimensions, and any
/// out-of-band configuration (parameter sets, `hvcC`/`av1C` records, …).
///
/// Leads with [`struct_size`](Self::struct_size) for forward-compatible field growth: a reader
/// trusts only the fields covered by `struct_size`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct StreamConfig {
    /// `size_of::<StreamConfig>()` as written by the producer; the ABI version guard for this
    /// descriptor. See [`StreamConfig::is_abi_current`].
    pub struct_size: usize,
    /// Host/backend-agreed codec identifier (e.g. a FourCC or an enum discriminant).
    pub codec_id: u32,
    /// Coded width in pixels.
    pub width: u32,
    /// Coded height in pixels.
    pub height: u32,
    /// Bits per component of the coded samples (e.g. `8`, `10`, `12`).
    pub bit_depth: u32,
    /// Pointer to codec extradata (parameter sets / config record), or null when there is none.
    /// Borrowed for the duration of the call; the backend must not retain it.
    pub extradata: *const u8,
    /// Length in bytes of [`extradata`](Self::extradata); `0` when there is none.
    pub extradata_len: usize,
}

impl StreamConfig {
    /// Builds a [`StreamConfig`] with no extradata, filling in the [`struct_size`](Self::struct_size)
    /// guard. Set [`extradata`](Self::extradata) / [`extradata_len`](Self::extradata_len) afterwards
    /// to attach a config record.
    #[must_use]
    pub const fn new(codec_id: u32, width: u32, height: u32, bit_depth: u32) -> Self {
        Self {
            struct_size: core::mem::size_of::<Self>(),
            codec_id,
            width,
            height,
            bit_depth,
            extradata: core::ptr::null(),
            extradata_len: 0,
        }
    }

    /// Returns `true` iff [`struct_size`](Self::struct_size) matches this build's layout.
    #[must_use]
    pub const fn is_abi_current(&self) -> bool {
        self.struct_size == core::mem::size_of::<Self>()
    }

    /// Borrows the extradata as a byte slice, or an empty slice when absent.
    ///
    /// # Safety
    ///
    /// If [`extradata_len`](Self::extradata_len) is non-zero, [`extradata`](Self::extradata) must
    /// point to at least that many initialized bytes that stay valid for `'a`.
    #[must_use]
    pub unsafe fn extradata(&self) -> &[u8] {
        // SAFETY: delegated to this fn's contract; the null/zero case yields an empty slice without
        // dereferencing.
        unsafe { bytes(self.extradata, self.extradata_len) }
    }
}

/// Describes an encode job to an encoder backend: the target codec, a quality knob, and an opaque
/// codec-specific options blob.
///
/// Leads with [`struct_size`](Self::struct_size) for forward-compatible field growth.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct EncodeConfig {
    /// `size_of::<EncodeConfig>()` as written by the producer; the ABI version guard for this
    /// descriptor. See [`EncodeConfig::is_abi_current`].
    pub struct_size: usize,
    /// Host/backend-agreed codec identifier (e.g. a FourCC or an enum discriminant).
    pub codec_id: u32,
    /// Quality target on a `0..=100` scale (higher is better quality / larger output); a backend
    /// clamps out-of-range values.
    pub quality: u32,
    /// Pointer to an opaque, codec-specific options blob, or null when there is none. Borrowed for
    /// the duration of the call; the backend must not retain it.
    pub extra: *const c_void,
    /// Length in bytes of [`extra`](Self::extra); `0` when there is none.
    pub extra_len: usize,
}

impl EncodeConfig {
    /// Builds an [`EncodeConfig`] with no options blob, filling in the
    /// [`struct_size`](Self::struct_size) guard.
    #[must_use]
    pub const fn new(codec_id: u32, quality: u32) -> Self {
        Self {
            struct_size: core::mem::size_of::<Self>(),
            codec_id,
            quality,
            extra: core::ptr::null(),
            extra_len: 0,
        }
    }

    /// Returns `true` iff [`struct_size`](Self::struct_size) matches this build's layout.
    #[must_use]
    pub const fn is_abi_current(&self) -> bool {
        self.struct_size == core::mem::size_of::<Self>()
    }
}

/// A raw image buffer shared across the seam: the pixel layout, dimensions, and plane pointers.
///
/// The same descriptor is used both ways. As an **encode input** the planes are read; as a **decode
/// output** (`out` in [`Decoder::decode`]) the decoder writes reconstructed samples through the
/// plane pointers. In both directions the buffer is caller-allocated and borrowed for the call.
///
/// Leads with [`struct_size`](Self::struct_size) for forward-compatible field growth.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct ImageDesc {
    /// `size_of::<ImageDesc>()` as written by the producer; the ABI version guard for this
    /// descriptor. See [`ImageDesc::is_abi_current`].
    pub struct_size: usize,
    /// Host/backend-agreed pixel-format tag (mirrors `gamut_core::PixelFormat`'s discriminants at
    /// the wiring layer; kept a plain `u32` here to keep this crate dependency-free).
    pub pixel_format: u32,
    /// Image width in pixels.
    pub width: u32,
    /// Image height in pixels.
    pub height: u32,
    /// Bits per component (e.g. `8`, `16`).
    pub depth: u32,
    /// Number of populated entries in [`planes`](Self::planes) / [`strides`](Self::strides);
    /// `1..=MAX_PLANES`.
    pub plane_count: u32,
    /// Per-plane row-start pointers; only the first [`plane_count`](Self::plane_count) are valid.
    /// Unused entries are null.
    pub planes: [*mut u8; MAX_PLANES],
    /// Per-plane row stride in bytes; only the first [`plane_count`](Self::plane_count) are valid.
    pub strides: [usize; MAX_PLANES],
}

impl ImageDesc {
    /// Builds an [`ImageDesc`], filling in the [`struct_size`](Self::struct_size) guard.
    ///
    /// `planes` / `strides` are fixed [`MAX_PLANES`]-entry arrays; entries beyond `plane_count`
    /// should be null / `0`.
    #[must_use]
    pub const fn new(
        pixel_format: u32,
        width: u32,
        height: u32,
        depth: u32,
        plane_count: u32,
        planes: [*mut u8; MAX_PLANES],
        strides: [usize; MAX_PLANES],
    ) -> Self {
        Self {
            struct_size: core::mem::size_of::<Self>(),
            pixel_format,
            width,
            height,
            depth,
            plane_count,
            planes,
            strides,
        }
    }

    /// Returns `true` iff [`struct_size`](Self::struct_size) matches this build's layout.
    #[must_use]
    pub const fn is_abi_current(&self) -> bool {
        self.struct_size == core::mem::size_of::<Self>()
    }
}

/// A C callback that receives one chunk of encoder output.
///
/// The encoder calls it zero or more times over the course of an encode; `ctx` is the opaque sink
/// pointer passed alongside it, `data`/`len` the bytes. Returning a non-[`Status::OK`] code asks the
/// encoder to abort (the encoder should propagate that status).
///
/// # Safety
///
/// The implementation must treat `data` as valid for `len` bytes only for the duration of the call,
/// and `ctx` as the exact pointer it was handed.
pub type WriteFn = unsafe extern "C" fn(ctx: *mut c_void, data: *const u8, len: usize) -> Status;

/// The `repr(C)` decoder backend: an [`abi_version`](Self::abi_version) tag plus function pointers
/// over an opaque `ctx`. The C twin of [`Decoder`].
///
/// Each function pointer is [`Option`]-wrapped (a null C pointer): a `None` `supports` reads as
/// "supports nothing", a `None` `decode` as [`Status::UNSUPPORTED`], and a `None` `destroy` as
/// "no teardown". The first argument of every call is the backend's own `ctx`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct DecoderVTable {
    /// Must equal [`ABI_VERSION`]; a host rejects any other value.
    pub abi_version: u32,
    /// Reports whether the backend can decode the given stream. `ctx` is the backend context.
    pub supports:
        Option<unsafe extern "C" fn(ctx: *mut c_void, cfg: *const StreamConfig) -> Status>,
    /// Decodes `codestream` into the caller-allocated `out` planes. `ctx` is the backend context.
    pub decode: Option<
        unsafe extern "C" fn(
            ctx: *mut c_void,
            cfg: *const StreamConfig,
            codestream: *const u8,
            codestream_len: usize,
            out: *const ImageDesc,
        ) -> Status,
    >,
    /// Releases the backend context. Called exactly once, when the host drops the backend.
    pub destroy: Option<unsafe extern "C" fn(ctx: *mut c_void)>,
}

/// The `repr(C)` encoder backend: an [`abi_version`](Self::abi_version) tag plus function pointers
/// over an opaque `ctx`. The C twin of [`Encoder`].
///
/// Output is delivered through a [`WriteFn`] + sink `ctx` the host passes into `encode`, so the
/// encoder streams bytes without owning the output buffer. Function-pointer nullability follows the
/// same rules as [`DecoderVTable`].
#[repr(C)]
#[derive(Clone, Copy)]
pub struct EncoderVTable {
    /// Must equal [`ABI_VERSION`]; a host rejects any other value.
    pub abi_version: u32,
    /// Reports whether the backend can satisfy the given encode job. `ctx` is the backend context.
    pub supports:
        Option<unsafe extern "C" fn(ctx: *mut c_void, cfg: *const EncodeConfig) -> Status>,
    /// Encodes `image`, streaming output through `write`/`write_ctx`. `ctx` is the backend context.
    pub encode: Option<
        unsafe extern "C" fn(
            ctx: *mut c_void,
            cfg: *const EncodeConfig,
            image: *const ImageDesc,
            write: WriteFn,
            write_ctx: *mut c_void,
        ) -> Status,
    >,
    /// Releases the backend context. Called exactly once, when the host drops the backend.
    pub destroy: Option<unsafe extern "C" fn(ctx: *mut c_void)>,
}

// Compile-time ABI pins. These are the layout facts the C side of the seam is built on; an edit
// that moves them is an ABI break and must fail the build here, in the defining crate, together
// with an [`ABI_VERSION`] bump. Field *append* (guarded by `struct_size`) leaves every pinned
// offset unchanged and stays friction-free — deliberately, no `size_of` pins on the descriptors.
const _: () = {
    use core::mem::{offset_of, size_of};

    assert!(ABI_VERSION == 1);
    assert!(MAX_PLANES == 4);

    // `Status` round-trips a C `int`, and its two contractual values are permanent.
    assert!(size_of::<Status>() == 4);
    assert!(Status::OK.0 == 0);
    assert!(Status::UNSUPPORTED.0 == -1);

    // The Option-wrapped fn pointers rely on the null-pointer optimization to be plain,
    // nullable C function pointers.
    assert!(size_of::<Option<unsafe extern "C" fn(*mut c_void)>>() == size_of::<usize>());

    // Vtables lead with `abi_version`; the fn-pointer slots follow at pointer stride. A reorder
    // or mid-struct insertion moves one of these and fails here.
    const PTR: usize = size_of::<usize>();
    assert!(offset_of!(DecoderVTable, abi_version) == 0);
    assert!(offset_of!(DecoderVTable, supports) == PTR);
    assert!(offset_of!(DecoderVTable, decode) == 2 * PTR);
    assert!(offset_of!(DecoderVTable, destroy) == 3 * PTR);
    assert!(offset_of!(EncoderVTable, abi_version) == 0);
    assert!(offset_of!(EncoderVTable, supports) == PTR);
    assert!(offset_of!(EncoderVTable, encode) == 2 * PTR);
    assert!(offset_of!(EncoderVTable, destroy) == 3 * PTR);

    // Descriptors lead with `struct_size` — the forward-compatibility guard every reader trusts.
    assert!(offset_of!(StreamConfig, struct_size) == 0);
    assert!(offset_of!(EncodeConfig, struct_size) == 0);
    assert!(offset_of!(ImageDesc, struct_size) == 0);
};

/// The object-safe Rust twin of [`DecoderVTable`]: a pluggable decode backend.
///
/// A pure-Rust backend implements this directly; a foreign C backend reaches it through
/// [`bridge::ForeignDecoder`]. A host holds backends as `&mut dyn Decoder` and selects among them by
/// [`supports`](Self::supports) (see the crate-level fallback contract). No `Send` supertrait — a
/// host binds `Send` when it inserts a backend into a registry.
pub trait Decoder {
    /// Reports whether this backend can decode the stream described by `cfg`. Returning `false` is
    /// the sole signal that lets a host fall through to the next backend.
    fn supports(&mut self, cfg: &StreamConfig) -> bool;

    /// Decodes `codestream` into the caller-allocated `out` planes, returning [`Status::OK`] on
    /// success. A backend that returns `true` from [`supports`](Self::supports) and then fails
    /// returns a non-OK, non-[`Status::UNSUPPORTED`] status, which the host propagates.
    fn decode(&mut self, cfg: &StreamConfig, codestream: &[u8], out: &ImageDesc) -> Status;
}

/// The object-safe Rust twin of [`EncoderVTable`]: a pluggable encode backend.
///
/// Output is streamed through the `sink` closure rather than an owned buffer, mirroring the C
/// [`WriteFn`]. Same selection/fallback contract and same absence of a `Send` supertrait as
/// [`Decoder`].
pub trait Encoder {
    /// Reports whether this backend can satisfy the encode job described by `cfg`. Returning `false`
    /// is the sole signal that lets a host fall through to the next backend.
    fn supports(&mut self, cfg: &EncodeConfig) -> bool;

    /// Encodes `image`, handing each output chunk to `sink`. Returns [`Status::OK`] on success, or
    /// the first non-OK status (from the backend or propagated from `sink`).
    fn encode(
        &mut self,
        cfg: &EncodeConfig,
        image: &ImageDesc,
        sink: &mut dyn FnMut(&[u8]) -> Status,
    ) -> Status;
}

/// A `&mut` reference to a decoder is itself a decoder (as `&mut I` is an `Iterator`), so an owned
/// backend — e.g. a long-lived [`bridge::ForeignDecoder`] — can be lent to a per-job adapter (such
/// as a per-item `AbiHevcDecoder`) without moving it and thus without triggering its
/// destroy-on-drop teardown.
impl<D: Decoder + ?Sized> Decoder for &mut D {
    fn supports(&mut self, cfg: &StreamConfig) -> bool {
        (**self).supports(cfg)
    }

    fn decode(&mut self, cfg: &StreamConfig, codestream: &[u8], out: &ImageDesc) -> Status {
        (**self).decode(cfg, codestream, out)
    }
}

/// A `&mut` reference to an encoder is itself an encoder; see the [`Decoder`] blanket impl for the
/// borrowing rationale.
impl<E: Encoder + ?Sized> Encoder for &mut E {
    fn supports(&mut self, cfg: &EncodeConfig) -> bool {
        (**self).supports(cfg)
    }

    fn encode(
        &mut self,
        cfg: &EncodeConfig,
        image: &ImageDesc,
        sink: &mut dyn FnMut(&[u8]) -> Status,
    ) -> Status {
        (**self).encode(cfg, image, sink)
    }
}

/// Borrows `len` bytes at `ptr`, or an empty slice when `len == 0` (so a null `ptr` with zero length
/// is safe).
///
/// # Safety
///
/// When `len != 0`, `ptr` must point to at least `len` initialized bytes valid for `'a`.
unsafe fn bytes<'a>(ptr: *const u8, len: usize) -> &'a [u8] {
    if len == 0 {
        &[]
    } else {
        // SAFETY: guaranteed by this fn's contract for the non-empty case.
        unsafe { core::slice::from_raw_parts(ptr, len) }
    }
}
