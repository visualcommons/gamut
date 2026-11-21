//! The shared codestream-backend seam: one `repr(C)` vtable shape (plus its object-safe Rust
//! "twin" traits) that lets a foreign (C/FFI) or alternate codestream backend plug into any gamut
//! format crate, together with the fallback contract that governs how a host selects among them.
//!
//! A gamut format crate (avif, heic, the `av2`/`vvc` stubs once implemented, …) owns the
//! *container* and everything around the coded picture, but the codestream encode/decode itself is
//! often better served by a platform or reference codec. This crate defines the **call shape** and
//! the **fallback contract** for that seam so every format crate wires backends the same way:
//!
//! - Backends are held in a host-owned [`Registry`] (behind the `std` feature), tried in **push
//!   order**; a software fallback (when a crate ships one) is the implicit tail, tried last.
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
//! # Descriptor growth: the `struct_size` prefix rule
//!
//! A descriptor ([`StreamConfig`], [`EncodeConfig`], [`ImageDesc`]) grows only by **appending**
//! fields, and its producer writes the `size_of` *it* was built with into `struct_size`. A reader
//! therefore trusts exactly the prefix `struct_size` covers: a field is readable when
//! `struct_size >= offset_of!(field) + size_of(field)`. Each descriptor answers that through
//! `covers(end)` and names the end of every field group past its mandatory prefix as an
//! associated constant ([`ImageDesc::CICP_END`]); its `MIN_SIZE` is the mandatory prefix itself,
//! the fields every reader needs, and `is_compatible()` tests for it. A shorter, older producer
//! stays usable for the fields it wrote; a newer, longer one is read for the fields this build
//! knows. Appending a field therefore never bumps [`ABI_VERSION`]; reordering, retyping or
//! removing one does.
//!
//! The [`bridge`] shims enforce the rule where a foreign producer's bytes enter Rust: they read
//! `struct_size` first, refuse a descriptor below `MIN_SIZE`, and copy only the covered prefix
//! into a full-size value whose uncovered tail holds the defaults (so a Rust twin never reads past
//! the foreign allocation). A Rust twin can therefore rely on every `MIN_SIZE` field and checks
//! `covers` only for a field past it — [`ImageDesc::has_cicp`] for the colour signalling.
//!
//! The [`bridge`] module converts between the two: [`bridge::ForeignDecoder`] /
//! [`bridge::ForeignEncoder`] adapt a C vtable *into* a Rust twin (C → Rust), and
//! [`bridge::lower_decoder`] / [`bridge::lower_encoder`] expose a boxed Rust twin *as* a C vtable
//! (Rust → C). Those adapters, together with [`StreamConfig::extradata`] (defined there), are the
//! crate's only `unsafe` code: everything outside [`bridge`] is `deny(unsafe_code)`. The vtables
//! here only *declare* `unsafe extern "C" fn` pointer types for a backend to fill; nothing outside
//! [`bridge`] calls through one.
//!
//! # Identifiers
//!
//! - [`codec_id`] builds a [`StreamConfig::codec_id`] / [`EncodeConfig::codec_id`] from a FourCC,
//!   **big-endian** (the first character is the most significant byte).
//! - [`pixel_format`] defines the [`ImageDesc::pixel_format`] tag namespace.
//!
//! # Scope and design notes
//!
//! - **`#![no_std]`, dependency-free.** The seam is pure interface: primitive fields, raw pointers,
//!   and function pointers only. `alloc` is pulled in for the `Box`-based [`bridge`] lowering; the
//!   opt-in `std` feature adds the lock-based [`Registry`] and nothing else.
//! - **No `Send` supertrait.** [`Decoder`] / [`Encoder`] are deliberately *not* `Send`; a host binds
//!   `Send` at the point it inserts a backend into a registry, so single-threaded backends stay
//!   usable. The C → Rust adapters offer an `unsafe` constructor by which the caller *asserts*
//!   thread-safety, at which point the adapter is `Send`.
//! - **C-friendly plain data.** Descriptors carry `u32` ids/dimensions and raw pointer + length
//!   pairs; they map onto a C struct with no translation layer, matching the
//!   `gamut_heic::HevcCodestreamDecoder` precedent (object-safe, borrowed bytes in, owned plain
//!   data out).
#![no_std]
#![deny(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]

// Compiles README.md's code blocks as doctests; exists only while rustdoc collects them.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;

extern crate alloc;
#[cfg(feature = "std")]
extern crate std;

#[expect(
    unsafe_code,
    reason = "the C ⇄ Rust adapters dereference foreign vtables and raw descriptor pointers; \
              `bridge` is the one module the crate confines that to"
)]
pub mod bridge;
pub mod pixel_format;
#[cfg(feature = "std")]
mod registry;

use core::ffi::c_void;

#[cfg(feature = "std")]
pub use registry::{Registry, RegistryError};

/// The ABI revision this crate defines.
///
/// Every [`DecoderVTable`] / [`EncoderVTable`] leads with an `abi_version` field; a host (and the
/// [`bridge`] adapters) accept a vtable only when its `abi_version` equals this constant. Bump it
/// whenever the vtable or descriptor layout changes incompatibly — a reorder, a retype, a removal,
/// or a change to what a call may do with its arguments. Appending a descriptor field is *not*
/// such a change: the [`struct_size` prefix rule](crate#descriptor-growth-the-struct_size-prefix-rule)
/// keeps both older and newer peers readable.
///
/// Revision `2` replaced revision `1`'s `struct_size` *equality* guard with the prefix rule,
/// appended the CICP colour signalling to [`ImageDesc`], and made [`DecoderVTable::decode`]'s `out`
/// writable so a decoder reports that signalling back. A revision-1 backend may still enforce
/// equality — and so reject every revision-2 descriptor per call — so it is refused at
/// registration instead, where the refusal asks for a rebuild.
pub const ABI_VERSION: u32 = 2;

/// The maximum number of image planes a [`ImageDesc`] carries.
///
/// Four covers every gamut pixel layout (interleaved RGBA is one plane; the widest planar YCbCr +
/// alpha case is four). Fixed so [`ImageDesc`] stays a flat `repr(C)` struct with no indirection.
pub const MAX_PLANES: usize = 4;

/// Builds a codec identifier from a FourCC, **big-endian**: `fourcc[0]` is the most significant
/// byte, so `codec_id(*b"av01")` is `0x6176_3031` and its `to_be_bytes()` spell `av01` again.
///
/// The one byte order every [`StreamConfig::codec_id`] / [`EncodeConfig::codec_id`] and every
/// FourCC-named [`pixel_format`] tag uses, so a host and a backend built apart agree on the value.
#[must_use]
pub const fn codec_id(fourcc: [u8; 4]) -> u32 {
    u32::from_be_bytes(fourcc)
}

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
/// trusts only the fields covered by `struct_size` (the
/// [prefix rule](crate#descriptor-growth-the-struct_size-prefix-rule)).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct StreamConfig {
    /// `size_of::<StreamConfig>()` as the producer was built; the prefix of this descriptor a
    /// reader may trust. See [`StreamConfig::covers`].
    pub struct_size: usize,
    /// Codec identifier: a FourCC built by [`codec_id`] (big-endian).
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

    /// The mandatory prefix: the end of [`extradata_len`](Self::extradata_len), the last field
    /// every reader needs. A descriptor shorter than this is unreadable.
    pub const MIN_SIZE: usize =
        core::mem::offset_of!(Self, extradata_len) + core::mem::size_of::<usize>();

    /// Returns `true` iff [`struct_size`](Self::struct_size) covers the first `end` bytes, i.e.
    /// every field ending at or before `end` was written by the producer.
    #[must_use]
    pub const fn covers(&self, end: usize) -> bool {
        self.struct_size >= end
    }

    /// Returns `true` iff the producer wrote at least the mandatory prefix
    /// ([`MIN_SIZE`](Self::MIN_SIZE)) — the test a reader applies, older and newer producers alike.
    #[must_use]
    pub const fn is_compatible(&self) -> bool {
        self.covers(Self::MIN_SIZE)
    }

    /// Returns `true` iff [`struct_size`](Self::struct_size) is exactly this build's layout, i.e.
    /// the producer was built against this revision. Informational only: a reader accepts any
    /// [compatible](Self::is_compatible) prefix, not just the current one.
    #[must_use]
    pub const fn is_abi_current(&self) -> bool {
        self.struct_size == core::mem::size_of::<Self>()
    }
}

/// Describes an encode job to an encoder backend: the target codec, a quality knob, and an opaque
/// codec-specific options blob.
///
/// Leads with [`struct_size`](Self::struct_size) for forward-compatible field growth (the
/// [prefix rule](crate#descriptor-growth-the-struct_size-prefix-rule)).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct EncodeConfig {
    /// `size_of::<EncodeConfig>()` as the producer was built; the prefix of this descriptor a
    /// reader may trust. See [`EncodeConfig::covers`].
    pub struct_size: usize,
    /// Codec identifier: a FourCC built by [`codec_id`] (big-endian).
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

    /// The mandatory prefix: the end of [`extra_len`](Self::extra_len), the last field every
    /// reader needs. A descriptor shorter than this is unreadable.
    pub const MIN_SIZE: usize =
        core::mem::offset_of!(Self, extra_len) + core::mem::size_of::<usize>();

    /// Returns `true` iff [`struct_size`](Self::struct_size) covers the first `end` bytes, i.e.
    /// every field ending at or before `end` was written by the producer.
    #[must_use]
    pub const fn covers(&self, end: usize) -> bool {
        self.struct_size >= end
    }

    /// Returns `true` iff the producer wrote at least the mandatory prefix
    /// ([`MIN_SIZE`](Self::MIN_SIZE)).
    #[must_use]
    pub const fn is_compatible(&self) -> bool {
        self.covers(Self::MIN_SIZE)
    }

    /// Returns `true` iff [`struct_size`](Self::struct_size) is exactly this build's layout.
    /// Informational only: a reader accepts any [compatible](Self::is_compatible) prefix.
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
/// # Colour signalling
///
/// The trailing [`color_primaries`](Self::color_primaries) /
/// [`transfer_characteristics`](Self::transfer_characteristics) /
/// [`matrix_coefficients`](Self::matrix_coefficients) / [`full_range`](Self::full_range) group is
/// the planes' CICP signalling (ITU-T H.273). As an encode input it is what the host asks the
/// stream to declare; as a decode output the host seeds it and the decoder overwrites it with what
/// the codestream declares. [`ImageDesc::new`] fills it with [`UNSIGNALLED`](Self::UNSIGNALLED),
/// the value every codec defaults to when its stream carries no colour description. It lies past
/// [`MIN_SIZE`](Self::MIN_SIZE): read it only when [`has_cicp`](Self::has_cicp) holds.
///
/// Leads with [`struct_size`](Self::struct_size) for forward-compatible field growth (the
/// [prefix rule](crate#descriptor-growth-the-struct_size-prefix-rule)).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct ImageDesc {
    /// `size_of::<ImageDesc>()` as the producer was built; the prefix of this descriptor a reader
    /// may trust. See [`ImageDesc::covers`].
    pub struct_size: usize,
    /// Pixel-format tag from the [`pixel_format`] namespace.
    pub pixel_format: u32,
    /// Image width in pixels.
    pub width: u32,
    /// Image height in pixels.
    pub height: u32,
    /// Bits per component (e.g. `8`, `16`).
    pub bit_depth: u32,
    /// Number of populated entries in [`planes`](Self::planes) / [`strides`](Self::strides);
    /// `1..=MAX_PLANES`.
    pub plane_count: u32,
    /// Per-plane row-start pointers; only the first [`plane_count`](Self::plane_count) are valid.
    /// Unused entries are null.
    pub planes: [*mut u8; MAX_PLANES],
    /// Per-plane row stride in bytes; only the first [`plane_count`](Self::plane_count) are valid.
    pub strides: [usize; MAX_PLANES],
    /// CICP `ColourPrimaries` (ITU-T H.273 §8.1); `2` is unspecified. Valid only when
    /// [`has_cicp`](Self::has_cicp) holds.
    pub color_primaries: u16,
    /// CICP `TransferCharacteristics` (ITU-T H.273 §8.2); `2` is unspecified. Valid only when
    /// [`has_cicp`](Self::has_cicp) holds.
    pub transfer_characteristics: u16,
    /// CICP `MatrixCoefficients` (ITU-T H.273 §8.3); `2` is unspecified. Valid only when
    /// [`has_cicp`](Self::has_cicp) holds.
    pub matrix_coefficients: u16,
    /// CICP `VideoFullRangeFlag` (ITU-T H.273 §8.3): `0` limited ("studio") range, any other value
    /// full range — read it through [`is_full_range`](Self::is_full_range). A `u8` rather than a
    /// `bool` so every byte a foreign producer writes is a valid value. Valid only when
    /// [`has_cicp`](Self::has_cicp) holds.
    pub full_range: u8,
}

impl ImageDesc {
    /// The mandatory prefix: the end of [`strides`](Self::strides), the last field every reader
    /// needs. A descriptor shorter than this is unreadable.
    pub const MIN_SIZE: usize =
        core::mem::offset_of!(Self, strides) + core::mem::size_of::<[usize; MAX_PLANES]>();

    /// The end of the colour-signalling group ([`full_range`](Self::full_range) is its last
    /// field): a descriptor whose [`struct_size`](Self::struct_size) reaches it carries CICP.
    pub const CICP_END: usize =
        core::mem::offset_of!(Self, full_range) + core::mem::size_of::<u8>();

    /// The colour signalling of a stream that declares none, as `(color_primaries,
    /// transfer_characteristics, matrix_coefficients, full_range)`: CICP `2/2/2` (unspecified) at
    /// limited range — the HEVC VUI defaults (ITU-T H.265 §E.3.1) and the AV1 `color_config()`
    /// defaults (AV1 §6.4.2) alike.
    pub const UNSIGNALLED: (u16, u16, u16, bool) = (2, 2, 2, false);

    /// Builds an [`ImageDesc`], filling in the [`struct_size`](Self::struct_size) guard and the
    /// [`UNSIGNALLED`](Self::UNSIGNALLED) colour signalling; set the latter with
    /// [`with_cicp`](Self::with_cicp).
    ///
    /// `planes` / `strides` are fixed [`MAX_PLANES`]-entry arrays; entries beyond `plane_count`
    /// should be null / `0`.
    #[must_use]
    pub const fn new(
        pixel_format: u32,
        width: u32,
        height: u32,
        bit_depth: u32,
        plane_count: u32,
        planes: [*mut u8; MAX_PLANES],
        strides: [usize; MAX_PLANES],
    ) -> Self {
        Self {
            struct_size: core::mem::size_of::<Self>(),
            pixel_format,
            width,
            height,
            bit_depth,
            plane_count,
            planes,
            strides,
            color_primaries: Self::UNSIGNALLED.0,
            transfer_characteristics: Self::UNSIGNALLED.1,
            matrix_coefficients: Self::UNSIGNALLED.2,
            full_range: Self::UNSIGNALLED.3 as u8,
        }
    }

    /// Returns this descriptor with its colour signalling set to the CICP triple
    /// `color_primaries` / `transfer_characteristics` / `matrix_coefficients` and the range flag
    /// `full_range`.
    #[must_use]
    pub const fn with_cicp(
        mut self,
        color_primaries: u16,
        transfer_characteristics: u16,
        matrix_coefficients: u16,
        full_range: bool,
    ) -> Self {
        self.color_primaries = color_primaries;
        self.transfer_characteristics = transfer_characteristics;
        self.matrix_coefficients = matrix_coefficients;
        self.full_range = full_range as u8;
        self
    }

    /// Returns `true` iff [`full_range`](Self::full_range) is non-zero: the samples are full range.
    #[must_use]
    pub const fn is_full_range(&self) -> bool {
        self.full_range != 0
    }

    /// Returns `true` iff [`struct_size`](Self::struct_size) covers the first `end` bytes, i.e.
    /// every field ending at or before `end` was written by the producer.
    #[must_use]
    pub const fn covers(&self, end: usize) -> bool {
        self.struct_size >= end
    }

    /// Returns `true` iff the producer wrote at least the mandatory prefix
    /// ([`MIN_SIZE`](Self::MIN_SIZE)).
    #[must_use]
    pub const fn is_compatible(&self) -> bool {
        self.covers(Self::MIN_SIZE)
    }

    /// Returns `true` iff the producer wrote the colour-signalling group
    /// ([`CICP_END`](Self::CICP_END)). When it did not, the stream's signalling is unknown and a
    /// reader treats it as [`UNSIGNALLED`](Self::UNSIGNALLED).
    #[must_use]
    pub const fn has_cicp(&self) -> bool {
        self.covers(Self::CICP_END)
    }

    /// Returns `true` iff [`struct_size`](Self::struct_size) is exactly this build's layout.
    /// Informational only: a reader accepts any [compatible](Self::is_compatible) prefix.
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
    /// Decodes `codestream` into the caller-allocated `out` planes and overwrites `out`'s colour
    /// signalling with the codestream's (see [`ImageDesc`]); every other `out` field is the host's
    /// and stays as it was. `ctx` is the backend context.
    pub decode: Option<
        unsafe extern "C" fn(
            ctx: *mut c_void,
            cfg: *const StreamConfig,
            codestream: *const u8,
            codestream_len: usize,
            out: *mut ImageDesc,
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
// with an [`ABI_VERSION`] bump. Field *append* (the `struct_size` prefix rule) leaves every pinned
// offset unchanged and stays friction-free — deliberately, no `size_of` pins on the descriptors.
const _: () = {
    use core::mem::{offset_of, size_of};

    assert!(ABI_VERSION == 2);
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

    // Every field's offset is permanent under the prefix rule: a reader decides what it may read
    // from these ends alone, so a moved field would be read from another revision's bytes.
    assert!(offset_of!(StreamConfig, codec_id) == PTR);
    assert!(offset_of!(StreamConfig, width) == PTR + 4);
    assert!(offset_of!(StreamConfig, height) == PTR + 8);
    assert!(offset_of!(StreamConfig, bit_depth) == PTR + 12);
    assert!(offset_of!(StreamConfig, extradata) == PTR + 16);
    assert!(offset_of!(StreamConfig, extradata_len) == 2 * PTR + 16);
    assert!(StreamConfig::MIN_SIZE == 3 * PTR + 16);
    assert!(offset_of!(EncodeConfig, codec_id) == PTR);
    assert!(offset_of!(EncodeConfig, quality) == PTR + 4);
    assert!(offset_of!(EncodeConfig, extra) == PTR + 8);
    assert!(offset_of!(EncodeConfig, extra_len) == 2 * PTR + 8);
    assert!(EncodeConfig::MIN_SIZE == 3 * PTR + 8);
    assert!(offset_of!(ImageDesc, pixel_format) == PTR);
    assert!(offset_of!(ImageDesc, width) == PTR + 4);
    assert!(offset_of!(ImageDesc, height) == PTR + 8);
    assert!(offset_of!(ImageDesc, bit_depth) == PTR + 12);
    assert!(offset_of!(ImageDesc, plane_count) == PTR + 16);
    const PLANES: usize = (PTR + 20).next_multiple_of(PTR);
    assert!(offset_of!(ImageDesc, planes) == PLANES);
    assert!(offset_of!(ImageDesc, strides) == PLANES + MAX_PLANES * PTR);
    assert!(ImageDesc::MIN_SIZE == PLANES + 2 * MAX_PLANES * PTR);
    // The colour group is the first append past the mandatory prefix.
    assert!(offset_of!(ImageDesc, color_primaries) == ImageDesc::MIN_SIZE);
    assert!(offset_of!(ImageDesc, transfer_characteristics) == ImageDesc::MIN_SIZE + 2);
    assert!(offset_of!(ImageDesc, matrix_coefficients) == ImageDesc::MIN_SIZE + 4);
    assert!(offset_of!(ImageDesc, full_range) == ImageDesc::MIN_SIZE + 6);
    assert!(ImageDesc::CICP_END == ImageDesc::MIN_SIZE + 7);
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
    ///
    /// The host seeds `out`'s colour signalling (normally [`ImageDesc::UNSIGNALLED`]); a backend
    /// that knows the codestream's overwrites it, when [`ImageDesc::has_cicp`] holds. Every other
    /// `out` field is the host's and the backend leaves it as it was.
    fn decode(&mut self, cfg: &StreamConfig, codestream: &[u8], out: &mut ImageDesc) -> Status;
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
/// as an `AbiHevcDecoder`) without moving it and thus without triggering its
/// destroy-on-drop teardown.
impl<D: Decoder + ?Sized> Decoder for &mut D {
    fn supports(&mut self, cfg: &StreamConfig) -> bool {
        (**self).supports(cfg)
    }

    fn decode(&mut self, cfg: &StreamConfig, codestream: &[u8], out: &mut ImageDesc) -> Status {
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
