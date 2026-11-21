//! The `unsafe` boundary between the `repr(C)` vtables and the Rust twin traits.
//!
//! Two directions, mirror images:
//!
//! - **C → Rust** ([`ForeignDecoder`] / [`ForeignEncoder`]): wrap a `*const DecoderVTable` /
//!   `*const EncoderVTable` plus an opaque `ctx`, verify [`abi_version`](crate::ABI_VERSION) at
//!   construction, implement the [`Decoder`] / [`Encoder`] twin by
//!   calling through the function pointers, and run the vtable's `destroy` on drop.
//! - **Rust → C** ([`lower_decoder`] / [`lower_encoder`]): take a `Box<dyn Decoder>` /
//!   `Box<dyn Encoder>`, hand back a [`Lowered`] [`DecoderVTable`] / [`EncoderVTable`] + `ctx`
//!   pointer whose function pointers dispatch into the box, and whose `destroy` reboxes and drops
//!   it.
//!
//! [`StreamConfig::extradata`], the one raw-pointer read on a descriptor, is defined here too, so
//! this module is the crate's whole `unsafe` surface.
//!
//! # Foreign descriptors
//!
//! The Rust → C shims receive descriptors a foreign producer built, perhaps against an older or
//! newer revision, so they never borrow one in place: a `&StreamConfig` over a shorter foreign
//! allocation would read past it. Each shim reads `struct_size` first, refuses a descriptor below
//! its `MIN_SIZE` with [`Status::UNSUPPORTED`] (the fall-through in `supports`, a terminal late
//! decline in `decode` / `encode`), and hands the twin a full-size copy holding the covered prefix
//! over the current defaults, `struct_size` still the producer's — so the twin's `covers` /
//! [`ImageDesc::has_cicp`] answer for the producer. After a decode the shim writes back only the
//! colour signalling, and only when the producer's `out` covers it.
//!
//! Every `unsafe` block below is justified with a `// SAFETY:` comment; the fallible constructors
//! return `None` rather than trusting a mismatched ABI.

use alloc::boxed::Box;
use core::ffi::c_void;

use crate::{
    ABI_VERSION, Decoder, DecoderVTable, EncodeConfig, Encoder, EncoderVTable, ImageDesc,
    MAX_PLANES, Status, StreamConfig, WriteFn,
};

/// Copies the prefix of the foreign descriptor at `ptr` that its leading `struct_size` covers
/// over `base` — a value whose fields past that prefix hold the defaults — or returns `None` when
/// `struct_size` is below `min`, the descriptor's mandatory prefix.
///
/// At most `size_of::<T>()` bytes are copied, so a newer, longer producer is read for the fields
/// this build knows. Every descriptor field is an integer or a raw pointer, for which every bit
/// pattern is valid, so even a `struct_size` ending inside a field yields a valid `T`; `covers`
/// keeps a reader off such a field.
///
/// # Safety
///
/// `ptr` must point to a descriptor whose first field is a `usize` `struct_size`, aligned for
/// `T`, and valid for reads of `struct_size` bytes.
unsafe fn read_prefix<T: Copy>(ptr: *const T, min: usize, mut base: T) -> Option<T> {
    // SAFETY: per the contract the descriptor starts with an aligned, readable `usize`.
    let size = unsafe { ptr.cast::<usize>().read() };
    if size < min {
        return None;
    }
    // SAFETY: the source is readable for `size` bytes per the contract, and `base` is a local `T`,
    // writable for `size_of::<T>()`; `len` is at most both. The two never overlap.
    unsafe {
        core::ptr::copy_nonoverlapping(
            ptr.cast::<u8>(),
            (&raw mut base).cast::<u8>(),
            size.min(core::mem::size_of::<T>()),
        );
    }
    Some(base)
}

/// The defaults [`read_prefix`] copies a foreign [`StreamConfig`] over.
const STREAM_CONFIG_BASE: StreamConfig = StreamConfig::new(0, 0, 0, 0);

/// The defaults [`read_prefix`] copies a foreign [`EncodeConfig`] over.
const ENCODE_CONFIG_BASE: EncodeConfig = EncodeConfig::new(0, 0);

/// The defaults [`read_prefix`] copies a foreign [`ImageDesc`] over: no planes, and the
/// [`ImageDesc::UNSIGNALLED`] colour a producer that wrote none means.
const IMAGE_DESC_BASE: ImageDesc = ImageDesc::new(
    0,
    0,
    0,
    0,
    0,
    [core::ptr::null_mut(); MAX_PLANES],
    [0; MAX_PLANES],
);

impl StreamConfig {
    /// Borrows the extradata as a byte slice, or an empty slice when absent.
    ///
    /// # Safety
    ///
    /// If [`extradata_len`](Self::extradata_len) is non-zero, [`extradata`](Self::extradata) must
    /// point to at least that many initialized bytes that stay valid for the borrow of `self`.
    #[must_use]
    pub unsafe fn extradata(&self) -> &[u8] {
        // SAFETY: delegated to this fn's contract; the null/zero case yields an empty slice without
        // dereferencing.
        unsafe { bytes(self.extradata, self.extradata_len) }
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

/// A boxed Rust twin lowered to C: the vtable a C consumer calls plus the `ctx` it passes back.
///
/// Returned by [`lower_decoder`] (`Lowered<DecoderVTable>`) and [`lower_encoder`]
/// (`Lowered<EncoderVTable>`). `ctx` **owns** the boxed twin: exactly one of two things must
/// eventually happen to it — it is passed to the vtable's `destroy` (e.g. by wrapping the pair back
/// up in a [`ForeignDecoder`] / [`ForeignEncoder`] and dropping that), or it is otherwise reclaimed.
/// Dropping a `Lowered` does neither, and leaks the twin.
///
/// Deliberately not `Clone`/`Copy`: a copy would be a second owner of the same `ctx`, and running
/// `destroy` through both is a double free.
#[must_use = "`ctx` owns the lowered backend and must be destroyed to avoid a leak"]
pub struct Lowered<V> {
    /// The function-pointer table; its `destroy` releases [`ctx`](Self::ctx).
    pub vtable: V,
    /// The opaque context every call through [`vtable`](Self::vtable) receives first.
    pub ctx: *mut c_void,
}

// ============================================================================================
// C -> Rust: adapt a foreign vtable into a Rust twin.
// ============================================================================================

/// Adapts a C [`DecoderVTable`] + `ctx` into a Rust [`Decoder`] (C → Rust).
///
/// Construct with [`ForeignDecoder::new`], which checks the vtable's
/// [`abi_version`](DecoderVTable::abi_version). On drop it invokes the vtable's `destroy` (if
/// present), so it owns the `ctx`.
#[must_use = "dropping a ForeignDecoder runs the backend's destroy callback"]
pub struct ForeignDecoder {
    vtable: *const DecoderVTable,
    ctx: *mut c_void,
}

impl ForeignDecoder {
    /// Wraps a foreign decoder vtable, or returns `None` if `vtable` is null or its
    /// [`abi_version`](DecoderVTable::abi_version) is not [`ABI_VERSION`]. On `None` the caller
    /// retains ownership of `ctx` (no `destroy` is run).
    ///
    /// # Safety
    ///
    /// - `vtable` must be null or point to a valid [`DecoderVTable`] that outlives the returned
    ///   value, whose function pointers are safe to call with `ctx`.
    /// - `ctx` must be the context that pairs with `vtable`; on success this value takes ownership of
    ///   it and will pass it to `destroy` on drop.
    /// - By calling this the caller **asserts the backend is `Send`-safe** (see the `Send` impl).
    #[must_use]
    pub unsafe fn new(vtable: *const DecoderVTable, ctx: *mut c_void) -> Option<Self> {
        if vtable.is_null() {
            return None;
        }
        // SAFETY: `vtable` is non-null and, per the contract, points to a valid DecoderVTable.
        let abi = unsafe { (*vtable).abi_version };
        if abi != ABI_VERSION {
            return None;
        }
        Some(Self { vtable, ctx })
    }
}

impl Decoder for ForeignDecoder {
    fn supports(&mut self, cfg: &StreamConfig) -> bool {
        // SAFETY: `vtable` is valid per the constructor contract; the fn pointer, if present, is a
        // valid extern "C" fn callable with `ctx` and a `*const StreamConfig`.
        match unsafe { (*self.vtable).supports } {
            Some(f) => unsafe { f(self.ctx, cfg) }.is_ok(),
            None => false,
        }
    }

    fn decode(&mut self, cfg: &StreamConfig, codestream: &[u8], out: &mut ImageDesc) -> Status {
        // SAFETY: as above; `codestream` is a live slice, so its pointer/len are valid for the call,
        // and `out` is a live, exclusive borrow the backend may write the colour signalling into.
        match unsafe { (*self.vtable).decode } {
            Some(f) => unsafe { f(self.ctx, cfg, codestream.as_ptr(), codestream.len(), out) },
            None => Status::UNSUPPORTED,
        }
    }
}

impl Drop for ForeignDecoder {
    fn drop(&mut self) {
        // SAFETY: `vtable` is valid per the constructor contract; `destroy`, if present, is called
        // exactly once here with the owned `ctx`.
        if let Some(f) = unsafe { (*self.vtable).destroy } {
            unsafe { f(self.ctx) };
        }
    }
}

// SAFETY: the caller of `ForeignDecoder::new` asserts (via its `unsafe` contract) that the backend
// behind `vtable`/`ctx` is safe to move to and use from another thread.
unsafe impl Send for ForeignDecoder {}

/// Adapts a C [`EncoderVTable`] + `ctx` into a Rust [`Encoder`] (C → Rust).
///
/// Construct with [`ForeignEncoder::new`]. On drop it invokes the vtable's `destroy` (if present).
#[must_use = "dropping a ForeignEncoder runs the backend's destroy callback"]
pub struct ForeignEncoder {
    vtable: *const EncoderVTable,
    ctx: *mut c_void,
}

impl ForeignEncoder {
    /// Wraps a foreign encoder vtable, or returns `None` if `vtable` is null or its
    /// [`abi_version`](EncoderVTable::abi_version) is not [`ABI_VERSION`]. On `None` the caller
    /// retains ownership of `ctx`.
    ///
    /// # Safety
    ///
    /// Same contract as [`ForeignDecoder::new`]: `vtable`/`ctx` must be a valid, paired,
    /// long-lived backend, and the caller asserts the backend is `Send`-safe.
    #[must_use]
    pub unsafe fn new(vtable: *const EncoderVTable, ctx: *mut c_void) -> Option<Self> {
        if vtable.is_null() {
            return None;
        }
        // SAFETY: `vtable` is non-null and, per the contract, points to a valid EncoderVTable.
        let abi = unsafe { (*vtable).abi_version };
        if abi != ABI_VERSION {
            return None;
        }
        Some(Self { vtable, ctx })
    }
}

impl Encoder for ForeignEncoder {
    fn supports(&mut self, cfg: &EncodeConfig) -> bool {
        // SAFETY: `vtable` valid per the constructor contract; fn pointer callable with `ctx`.
        match unsafe { (*self.vtable).supports } {
            Some(f) => unsafe { f(self.ctx, cfg) }.is_ok(),
            None => false,
        }
    }

    fn encode(
        &mut self,
        cfg: &EncodeConfig,
        image: &ImageDesc,
        sink: &mut dyn FnMut(&[u8]) -> Status,
    ) -> Status {
        /// Trampoline turning a C [`WriteFn`] call back into a Rust `sink` closure call. `ctx` points
        /// to the `&mut dyn FnMut` set up in `encode`.
        unsafe extern "C" fn trampoline(ctx: *mut c_void, data: *const u8, len: usize) -> Status {
            // SAFETY: `ctx` is the `*mut &mut dyn FnMut(...)` installed below; `data`/`len` describe
            // a valid slice per the WriteFn contract.
            let sink = unsafe { &mut *(ctx as *mut &mut dyn FnMut(&[u8]) -> Status) };
            let chunk = unsafe { bytes(data, len) };
            sink(chunk)
        }

        match unsafe { (*self.vtable).encode } {
            Some(f) => {
                // Store the fat `&mut dyn FnMut` behind a thin pointer the trampoline can recover.
                let mut sink_ref: &mut dyn FnMut(&[u8]) -> Status = sink;
                let write_ctx =
                    (&mut sink_ref) as *mut &mut dyn FnMut(&[u8]) -> Status as *mut c_void;
                // SAFETY: `vtable` valid per the constructor contract; `trampoline`/`write_ctx` form
                // a valid WriteFn pair, and `image`/`cfg` are live references for the call.
                unsafe { f(self.ctx, cfg, image, trampoline, write_ctx) }
            }
            None => Status::UNSUPPORTED,
        }
    }
}

impl Drop for ForeignEncoder {
    fn drop(&mut self) {
        // SAFETY: `vtable` valid per the constructor contract; `destroy`, if present, called exactly
        // once with the owned `ctx`.
        if let Some(f) = unsafe { (*self.vtable).destroy } {
            unsafe { f(self.ctx) };
        }
    }
}

// SAFETY: the caller of `ForeignEncoder::new` asserts the backend is safe to use from another thread.
unsafe impl Send for ForeignEncoder {}

// ============================================================================================
// Rust -> C: expose a boxed Rust twin as a foreign vtable.
// ============================================================================================

/// Lowers a boxed Rust [`Decoder`] to a C [`DecoderVTable`] + `ctx` pointer a C consumer can call
/// (Rust → C).
///
/// The returned [`ctx`](Lowered::ctx) owns the decoder; the vtable's `destroy` reboxes and drops
/// it. See [`Lowered`] for what must eventually happen to it.
pub fn lower_decoder(decoder: Box<dyn Decoder>) -> Lowered<DecoderVTable> {
    // Double-box so the fat `Box<dyn Decoder>` lives behind a thin `*mut c_void`.
    let ctx = Box::into_raw(Box::new(decoder)) as *mut c_void;
    Lowered {
        vtable: DECODER_VTABLE,
        ctx,
    }
}

/// The static vtable shared by every [`lower_decoder`] result; its `ctx` is a
/// `*mut Box<dyn Decoder>`.
const DECODER_VTABLE: DecoderVTable = DecoderVTable {
    abi_version: ABI_VERSION,
    supports: Some(decoder_supports_shim),
    decode: Some(decoder_decode_shim),
    destroy: Some(decoder_destroy_shim),
};

unsafe extern "C" fn decoder_supports_shim(ctx: *mut c_void, cfg: *const StreamConfig) -> Status {
    // SAFETY: `ctx` is the `*mut Box<dyn Decoder>` produced by `lower_decoder`; `cfg` points to a
    // valid StreamConfig prefix of `struct_size` bytes for the call.
    let decoder = unsafe { &mut *(ctx as *mut Box<dyn Decoder>) };
    let Some(cfg) = (unsafe { read_prefix(cfg, StreamConfig::MIN_SIZE, STREAM_CONFIG_BASE) })
    else {
        return Status::UNSUPPORTED;
    };
    if decoder.supports(&cfg) {
        Status::OK
    } else {
        Status::UNSUPPORTED
    }
}

unsafe extern "C" fn decoder_decode_shim(
    ctx: *mut c_void,
    cfg: *const StreamConfig,
    codestream: *const u8,
    codestream_len: usize,
    out: *mut ImageDesc,
) -> Status {
    // SAFETY: `ctx` is the `*mut Box<dyn Decoder>` from `lower_decoder`; `cfg`/`out` are valid
    // descriptor prefixes of their `struct_size` bytes, `out` writable, and
    // `codestream`/`codestream_len` a valid slice for the call.
    let decoder = unsafe { &mut *(ctx as *mut Box<dyn Decoder>) };
    let cfg = unsafe { read_prefix(cfg, StreamConfig::MIN_SIZE, STREAM_CONFIG_BASE) };
    let image = unsafe { read_prefix(out, ImageDesc::MIN_SIZE, IMAGE_DESC_BASE) };
    let (Some(cfg), Some(mut image)) = (cfg, image) else {
        return Status::UNSUPPORTED;
    };
    let codestream = unsafe { bytes(codestream, codestream_len) };
    // Decided before the call, from the producer's `struct_size`: the twin holds the copy mutably
    // and could rewrite that field, which must not widen what is written back.
    let write_back = image.has_cicp();
    let status = decoder.decode(&cfg, codestream, &mut image);
    if write_back {
        // SAFETY: `out` is writable for its producer's `struct_size` bytes, which `has_cicp` showed
        // reach past every field written here.
        unsafe {
            (&raw mut (*out).color_primaries).write(image.color_primaries);
            (&raw mut (*out).transfer_characteristics).write(image.transfer_characteristics);
            (&raw mut (*out).matrix_coefficients).write(image.matrix_coefficients);
            (&raw mut (*out).full_range).write(image.full_range);
        }
    }
    status
}

unsafe extern "C" fn decoder_destroy_shim(ctx: *mut c_void) {
    // SAFETY: `ctx` is the `*mut Box<dyn Decoder>` from `lower_decoder`, called exactly once; reclaim
    // and drop it.
    drop(unsafe { Box::from_raw(ctx as *mut Box<dyn Decoder>) });
}

/// Lowers a boxed Rust [`Encoder`] to a C [`EncoderVTable`] + `ctx` pointer a C consumer can call
/// (Rust → C).
///
/// The returned [`ctx`](Lowered::ctx) owns the encoder; the vtable's `destroy` reboxes and drops
/// it. See [`Lowered`] for what must eventually happen to it.
pub fn lower_encoder(encoder: Box<dyn Encoder>) -> Lowered<EncoderVTable> {
    let ctx = Box::into_raw(Box::new(encoder)) as *mut c_void;
    Lowered {
        vtable: ENCODER_VTABLE,
        ctx,
    }
}

/// The static vtable shared by every [`lower_encoder`] result; its `ctx` is a
/// `*mut Box<dyn Encoder>`.
const ENCODER_VTABLE: EncoderVTable = EncoderVTable {
    abi_version: ABI_VERSION,
    supports: Some(encoder_supports_shim),
    encode: Some(encoder_encode_shim),
    destroy: Some(encoder_destroy_shim),
};

unsafe extern "C" fn encoder_supports_shim(ctx: *mut c_void, cfg: *const EncodeConfig) -> Status {
    // SAFETY: `ctx` is the `*mut Box<dyn Encoder>` from `lower_encoder`; `cfg` is a valid descriptor
    // prefix of its `struct_size` bytes.
    let encoder = unsafe { &mut *(ctx as *mut Box<dyn Encoder>) };
    let Some(cfg) = (unsafe { read_prefix(cfg, EncodeConfig::MIN_SIZE, ENCODE_CONFIG_BASE) })
    else {
        return Status::UNSUPPORTED;
    };
    if encoder.supports(&cfg) {
        Status::OK
    } else {
        Status::UNSUPPORTED
    }
}

unsafe extern "C" fn encoder_encode_shim(
    ctx: *mut c_void,
    cfg: *const EncodeConfig,
    image: *const ImageDesc,
    write: WriteFn,
    write_ctx: *mut c_void,
) -> Status {
    // SAFETY: `ctx` is the `*mut Box<dyn Encoder>` from `lower_encoder`; `cfg`/`image` are valid
    // descriptor prefixes of their `struct_size` bytes; `write`/`write_ctx` form a valid WriteFn
    // pair.
    let encoder = unsafe { &mut *(ctx as *mut Box<dyn Encoder>) };
    let cfg = unsafe { read_prefix(cfg, EncodeConfig::MIN_SIZE, ENCODE_CONFIG_BASE) };
    let image = unsafe { read_prefix(image, ImageDesc::MIN_SIZE, IMAGE_DESC_BASE) };
    let (Some(cfg), Some(image)) = (cfg, image) else {
        return Status::UNSUPPORTED;
    };
    let mut sink = |chunk: &[u8]| -> Status {
        // SAFETY: `write`/`write_ctx` are a valid WriteFn pair; `chunk` is a live slice.
        unsafe { write(write_ctx, chunk.as_ptr(), chunk.len()) }
    };
    encoder.encode(&cfg, &image, &mut sink)
}

unsafe extern "C" fn encoder_destroy_shim(ctx: *mut c_void) {
    // SAFETY: `ctx` is the `*mut Box<dyn Encoder>` from `lower_encoder`, called exactly once.
    drop(unsafe { Box::from_raw(ctx as *mut Box<dyn Encoder>) });
}
