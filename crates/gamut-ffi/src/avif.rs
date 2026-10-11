//! AVIF C entry points: the encoder handle [`GamutAvifEncoder`] (backend seam, config setters,
//! encode) and the parsed-container handle [`GamutAvifImage`].
//!
//! The AVIF seam is the AV1 still-picture OBU stream (`AV1_CODEC_ID`, encode direction).
//! Backends are tried in push order; the built-in `gamut-av1` encoder is the implicit tail.
//!
//! There is deliberately no `gamut_avif_decoder_push_backend`: AVIF decode has no registry —
//! the Rust API threads a single caller-supplied `Av1StillDecoder` per call, and the planned
//! in-house decoder (issue #259) will make it optional. The decode direction joins the C
//! surface when that lands (see DESIGN.md). What *is* here on the read side is the container
//! parse (issue #666): [`GamutAvifImage`] hands a C caller the primary and alpha AV1
//! codestreams, the `av1C` record, and the metadata payloads, so it can drive any AV1 decoder
//! itself.
//!
//! # Consumer-boundary shape
//!
//! - **Encode** is one entry point per input layout the Rust encoder implements
//!   `EncodeImage` for (`rgb8`, `rgba8`, `gray8`, `rgb16`, `rgba16`), so 16-bit samples cross as
//!   `const uint16_t *` and the C compiler checks the sample type. The output is a Rust-owned
//!   buffer released with `gamut_bytes_free`. Each encode also records where it placed the
//!   C2PA manifest-store slot, read back with `gamut_avif_encoder_c2pa_slot` (issue #673).
//! - **Config setters** (`gamut_avif_encoder_set_*`) map the Rust `with_*` builders one-to-one;
//!   lossy mode and quality are fixed at construction, exactly as in Rust
//!   (`gamut_avif_encoder_new_lossy` ↔ `AvifEncoder::lossy`).
//! - **Parse** returns an owned [`GamutAvifImage`]; every accessor hands out pointers borrowed
//!   from it, valid until `gamut_avif_image_free`.

use core::cell::Cell;
use core::ffi::c_void;
use core::ops::Range;
use core::ptr;

use gamut::avif::{AvifContainer, AvifEncoder, AvifImage, AvifItem, Mirror, Rotation};
use gamut::core::{Dimensions, EncodeImage, Gray8, ImageRef, Pixel, Rgb8, Rgb16, Rgba8, Rgba16};
use gamut_codec_abi::EncoderVTable;
use gamut_color::{
    BitDepth, ChromaSubsampling, ColorRange, ColourPrimaries, MatrixCoefficients,
    TransferCharacteristics,
};

use crate::seam::seam_handle;
use crate::status::{
    GAMUT_OK, GAMUT_STATUS_INVALID_INPUT, GAMUT_STATUS_NULL_ARGUMENT, GAMUT_STATUS_UNSUPPORTED,
    GamutStatus,
};
use crate::{bytes_in, bytes_out, guard, status_of};

/// `gamut_avif_encoder_set_chroma` value: 4:4:4, full-resolution chroma.
pub const GAMUT_CHROMA_SUBSAMPLING_444: u8 = 0;
/// `gamut_avif_encoder_set_chroma` value: 4:2:2, horizontally halved chroma.
pub const GAMUT_CHROMA_SUBSAMPLING_422: u8 = 1;
/// `gamut_avif_encoder_set_chroma` value: 4:2:0, chroma halved in both directions.
pub const GAMUT_CHROMA_SUBSAMPLING_420: u8 = 2;
/// `gamut_avif_encoder_set_chroma` value: 4:0:0, monochrome (no chroma planes).
pub const GAMUT_CHROMA_SUBSAMPLING_400: u8 = 3;

/// `gamut_avif_encoder_set_mirror` value: top and bottom exchanged (the `imir` axis `0`,
/// ISO/IEC 23008-12:2022 §6.5.12).
pub const GAMUT_AVIF_MIRROR_TOP_BOTTOM: u8 = 0;
/// `gamut_avif_encoder_set_mirror` value: left and right exchanged (the `imir` axis `1`).
pub const GAMUT_AVIF_MIRROR_LEFT_RIGHT: u8 = 1;

// The chroma constants are `ChromaSubsampling`'s own discriminants, so a C value and the Rust
// variant can never be renumbered apart: a change on either side fails this build.
const _: () = {
    assert!(ChromaSubsampling::Cs444 as u8 == GAMUT_CHROMA_SUBSAMPLING_444);
    assert!(ChromaSubsampling::Cs422 as u8 == GAMUT_CHROMA_SUBSAMPLING_422);
    assert!(ChromaSubsampling::Cs420 as u8 == GAMUT_CHROMA_SUBSAMPLING_420);
    assert!(ChromaSubsampling::Cs400 as u8 == GAMUT_CHROMA_SUBSAMPLING_400);
};

/// Opaque handle over a `gamut::avif::AvifEncoder` and its backend registry.
pub struct GamutAvifEncoder {
    inner: AvifEncoder,
    /// The C2PA slot range of the most recent encode through this handle
    /// (`AvifEncodeReport::c2pa`), read back by `gamut_avif_encoder_c2pa_slot`. `None` before
    /// the first encode, after a failed one, and when no slot was configured.
    ///
    /// A `Cell` because the encode entry points take a `const` handle (an encode never changes
    /// the configuration); handles are single-threaded, so the interior mutability is unshared.
    c2pa: Cell<Option<Range<usize>>>,
}

seam_handle! {
    handle = GamutAvifEncoder,
    host = gamut::avif::AvifEncoder,
    new = gamut::avif::AvifEncoder::new(),
    fields = { c2pa: Cell::new(None) },
    seam = encoder,
    push = |host, backend| { host.push_backend(gamut::avif::AbiAv1StillEncoder::new(backend)); },
    fns = (gamut_avif_encoder_new, gamut_avif_encoder_free, gamut_avif_encoder_push_backend),
    tests = avif_encoder_tests,
}

impl GamutAvifEncoder {
    /// Applies one `with_*` builder to the handle's encoder.
    ///
    /// `value` is the C argument already mapped onto its Rust type; `None` means the mapping
    /// rejected it, reported as `rejected`. A null handle is reported first, whatever `value`.
    ///
    /// # Safety
    ///
    /// `encoder` is `NULL` or a live pointer from a `gamut_avif_encoder_new*` constructor.
    unsafe fn configure<T>(
        encoder: *mut Self,
        value: Option<T>,
        rejected: GamutStatus,
        apply: impl FnOnce(AvifEncoder, T) -> AvifEncoder,
    ) -> GamutStatus {
        guard::status(|| {
            // SAFETY: null is rejected here; a non-null handle is live per contract.
            let Some(this) = (unsafe { encoder.as_mut() }) else {
                return GAMUT_STATUS_NULL_ARGUMENT;
            };
            let Some(value) = value else {
                return rejected;
            };
            // The Rust builders consume and return the encoder (backends included), so the
            // value is moved out and back; the `Default` left behind for that instant is never
            // observed, because no `with_*` builder can panic.
            this.inner = apply(core::mem::take(&mut this.inner), value);
            GAMUT_OK
        })
    }

    /// Encodes `width × height` samples of layout `P` into a fresh Rust-owned buffer.
    ///
    /// # Safety
    ///
    /// As each `gamut_avif_encoder_encode_*` wrapper: `encoder` is `NULL` or live; a non-null
    /// `samples` points to `samples_len` readable samples; `out`/`out_len` are `NULL` or
    /// writable.
    unsafe fn encode<P: Pixel>(
        encoder: *const Self,
        samples: *const P::Sample,
        samples_len: usize,
        width: u32,
        height: u32,
        out: *mut *mut u8,
        out_len: *mut usize,
    ) -> GamutStatus
    where
        AvifEncoder: EncodeImage<P>,
    {
        guard::status(|| {
            // SAFETY: a non-null handle is live per contract. The previous encode's report is
            // dropped before anything can fail, so a non-OK status never leaves one standing.
            if let Some(this) = unsafe { encoder.as_ref() } {
                this.c2pa.take();
            }
            if out.is_null() || out_len.is_null() {
                return GAMUT_STATUS_NULL_ARGUMENT;
            }
            // SAFETY: both out-params are non-null and writable per contract. Cleared first so
            // a failed call never leaves the caller a stale pointer it might free twice.
            unsafe {
                out.write(ptr::null_mut());
                out_len.write(0);
            }
            // SAFETY: null is rejected here; a non-null handle is live per contract.
            let Some(this) = (unsafe { encoder.as_ref() }) else {
                return GAMUT_STATUS_NULL_ARGUMENT;
            };
            if samples.is_null() {
                return GAMUT_STATUS_NULL_ARGUMENT;
            }
            // A misaligned `uint16_t *` is a caller bug C compilers do not catch; refusing it is
            // what keeps the slice below sound.
            if !samples.is_aligned() {
                return GAMUT_STATUS_INVALID_INPUT;
            }
            // SAFETY: non-null and aligned (checked above), `samples_len` readable samples per
            // contract, and only read for the duration of this call.
            let samples = unsafe { core::slice::from_raw_parts(samples, samples_len) };
            // `ImageRef::new` is the boundary validation DESIGN.md names: the length must be
            // exactly `width * height * channels` and the dimensions non-empty.
            let image = match ImageRef::<P>::new(samples, Dimensions { width, height }) {
                Ok(image) => image,
                Err(error) => return status_of(&error),
            };
            // `encode_with_report` is `encode_to_vec` with the report alongside — the same bytes
            // — so every encode keeps the slot range `gamut_avif_encoder_c2pa_slot` reads.
            match this.inner.encode_with_report(image) {
                Ok((bytes, report)) => {
                    this.c2pa.set(report.c2pa);
                    let (data, len) = bytes_out(bytes);
                    // SAFETY: as the clearing writes above.
                    unsafe {
                        out.write(data);
                        out_len.write(len);
                    }
                    GAMUT_OK
                }
                Err(error) => status_of(&error),
            }
        })
    }
}

/// Creates an AVIF encoder with default (lossless) settings and an empty backend registry.
///
/// Returns `NULL` only if construction panics. Free with `gamut_avif_encoder_free`.
#[unsafe(no_mangle)]
pub extern "C" fn gamut_avif_encoder_new() -> *mut GamutAvifEncoder {
    GamutAvifEncoder::ffi_new()
}

/// Creates a **lossy** AVIF encoder at `quality` (`0..=100`, higher = larger and closer to the
/// source; values above `100` are clamped), with an empty backend registry.
///
/// Lossy stills default to BT.709 YCbCr at 4:2:0; override with `gamut_avif_encoder_set_matrix`
/// and `gamut_avif_encoder_set_chroma`. Mode and quality are fixed for the handle's lifetime,
/// as they are for the Rust `AvifEncoder::lossy` it wraps. Returns `NULL` only if construction
/// panics. Free with `gamut_avif_encoder_free`.
#[unsafe(no_mangle)]
pub extern "C" fn gamut_avif_encoder_new_lossy(quality: u8) -> *mut GamutAvifEncoder {
    guard::ptr(|| {
        Box::into_raw(Box::new(GamutAvifEncoder {
            inner: AvifEncoder::lossy(quality),
            c2pa: Cell::new(None),
        }))
    })
}

/// Frees an encoder created by `gamut_avif_encoder_new` or `gamut_avif_encoder_new_lossy`,
/// running each adopted backend's `destroy` exactly once. No-op on `NULL`.
///
/// # Safety
///
/// `encoder` is `NULL` or a pointer returned by `gamut_avif_encoder_new` or
/// `gamut_avif_encoder_new_lossy` that has not already been freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gamut_avif_encoder_free(encoder: *mut GamutAvifEncoder) {
    unsafe { GamutAvifEncoder::ffi_free(encoder) }
}

/// Pushes an AV1 still-picture encode backend; backends are tried in push order, before the
/// built-in `gamut-av1` encoder.
///
/// Returns `GAMUT_OK` when the handle adopts `ctx` (its `destroy` then runs exactly once, at
/// free), `GAMUT_STATUS_NULL_ARGUMENT` on a `NULL` encoder or vtable, or
/// `GAMUT_STATUS_ABI_MISMATCH` when `vtable->abi_version != GAMUT_CODEC_ABI_VERSION`; on any
/// non-OK status the caller keeps ownership of `ctx` and no callback has run.
///
/// # Safety
///
/// `encoder` as in `gamut_avif_encoder_free`; a non-`NULL` `vtable` points to a table that
/// stays valid for the encoder's lifetime. Calling this asserts the `(vtable, ctx)` backend
/// may be used from any thread (the registry requires `Send`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gamut_avif_encoder_push_backend(
    encoder: *mut GamutAvifEncoder,
    vtable: *const EncoderVTable,
    ctx: *mut c_void,
) -> GamutStatus {
    unsafe { GamutAvifEncoder::ffi_push_backend(encoder, vtable, ctx) }
}

/// Selects the CICP (ITU-T H.273) matrix the samples are coded through (`AvifEncoder::with_matrix`).
///
/// Identity (`0`), BT.709 (`1`), BT.601 (`6`) and BT.2020 NCL (`9`) encode; another code point
/// gamut models is accepted here and rejected at encode time. Ignored by a lossless encoder,
/// which always codes the identity matrix. Returns `GAMUT_STATUS_UNSUPPORTED` for a code point
/// gamut does not model, `GAMUT_STATUS_NULL_ARGUMENT` on a `NULL` encoder.
///
/// # Safety
///
/// `encoder` is `NULL` or a live encoder handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gamut_avif_encoder_set_matrix(
    encoder: *mut GamutAvifEncoder,
    code_point: u16,
) -> GamutStatus {
    let value = MatrixCoefficients::from_code_point(code_point);
    unsafe {
        GamutAvifEncoder::configure(encoder, value, GAMUT_STATUS_UNSUPPORTED, |e, m| {
            e.with_matrix(m)
        })
    }
}

/// Tags the image with CICP colour primaries (`AvifEncoder::with_primaries`) — a tag, not a
/// conversion; honoured in lossless and lossy mode alike.
///
/// Returns `GAMUT_STATUS_UNSUPPORTED` for a code point gamut does not model,
/// `GAMUT_STATUS_NULL_ARGUMENT` on a `NULL` encoder.
///
/// # Safety
///
/// `encoder` is `NULL` or a live encoder handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gamut_avif_encoder_set_primaries(
    encoder: *mut GamutAvifEncoder,
    code_point: u16,
) -> GamutStatus {
    let value = ColourPrimaries::from_code_point(code_point);
    unsafe {
        GamutAvifEncoder::configure(encoder, value, GAMUT_STATUS_UNSUPPORTED, |e, p| {
            e.with_primaries(p)
        })
    }
}

/// Tags the image with CICP transfer characteristics (`AvifEncoder::with_transfer`) — a tag,
/// not a conversion; honoured in lossless and lossy mode alike.
///
/// Returns `GAMUT_STATUS_UNSUPPORTED` for a code point gamut does not model,
/// `GAMUT_STATUS_NULL_ARGUMENT` on a `NULL` encoder.
///
/// # Safety
///
/// `encoder` is `NULL` or a live encoder handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gamut_avif_encoder_set_transfer(
    encoder: *mut GamutAvifEncoder,
    code_point: u16,
) -> GamutStatus {
    let value = TransferCharacteristics::from_code_point(code_point);
    unsafe {
        GamutAvifEncoder::configure(encoder, value, GAMUT_STATUS_UNSUPPORTED, |e, t| {
            e.with_transfer(t)
        })
    }
}

/// Selects the signal range by its H.273 `full_range` flag (`0` limited, `1` full;
/// `AvifEncoder::with_color_range`). Ignored by a lossless encoder.
///
/// Returns `GAMUT_STATUS_INVALID_INPUT` for any other flag value,
/// `GAMUT_STATUS_NULL_ARGUMENT` on a `NULL` encoder.
///
/// # Safety
///
/// `encoder` is `NULL` or a live encoder handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gamut_avif_encoder_set_color_range(
    encoder: *mut GamutAvifEncoder,
    full_range: u8,
) -> GamutStatus {
    let value = ColorRange::from_flag(full_range);
    unsafe {
        GamutAvifEncoder::configure(encoder, value, GAMUT_STATUS_INVALID_INPUT, |e, r| {
            e.with_color_range(r)
        })
    }
}

/// Selects the chroma subsampling, one of the `GAMUT_CHROMA_SUBSAMPLING_*` constants
/// (`AvifEncoder::with_chroma`). Ignored by a lossless encoder and by the identity matrix,
/// which always keep 4:4:4.
///
/// Returns `GAMUT_STATUS_INVALID_INPUT` for a value that is not one of the constants,
/// `GAMUT_STATUS_NULL_ARGUMENT` on a `NULL` encoder.
///
/// # Safety
///
/// `encoder` is `NULL` or a live encoder handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gamut_avif_encoder_set_chroma(
    encoder: *mut GamutAvifEncoder,
    chroma: u8,
) -> GamutStatus {
    let value = match chroma {
        GAMUT_CHROMA_SUBSAMPLING_444 => Some(ChromaSubsampling::Cs444),
        GAMUT_CHROMA_SUBSAMPLING_422 => Some(ChromaSubsampling::Cs422),
        GAMUT_CHROMA_SUBSAMPLING_420 => Some(ChromaSubsampling::Cs420),
        GAMUT_CHROMA_SUBSAMPLING_400 => Some(ChromaSubsampling::Cs400),
        _ => None,
    };
    unsafe {
        GamutAvifEncoder::configure(encoder, value, GAMUT_STATUS_INVALID_INPUT, |e, c| {
            e.with_chroma(c)
        })
    }
}

/// Selects the depth 16-bit inputs are coded at, in bits (`AvifEncoder::with_bit_depth`):
/// `10` or `12` (the default) encode; `8` and `16` are accepted here and rejected by a 16-bit
/// encode. Ignored by the 8-bit inputs, which always code 8-bit.
///
/// Returns `GAMUT_STATUS_UNSUPPORTED` for a depth gamut does not model,
/// `GAMUT_STATUS_NULL_ARGUMENT` on a `NULL` encoder.
///
/// # Safety
///
/// `encoder` is `NULL` or a live encoder handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gamut_avif_encoder_set_bit_depth(
    encoder: *mut GamutAvifEncoder,
    bits: u32,
) -> GamutStatus {
    let value = BitDepth::from_bits(bits);
    unsafe {
        GamutAvifEncoder::configure(encoder, value, GAMUT_STATUS_UNSUPPORTED, |e, d| {
            e.with_bit_depth(d)
        })
    }
}

/// Codes an RGBA input's alpha auxiliary at its own lossy `quality` (`0..=100`, higher = larger
/// and closer to the source; values above `100` are clamped), independent of the colour quality
/// fixed at construction (`AvifEncoder::with_alpha_quality`). Without this call alpha follows
/// the colour quality. Ignored by a lossless encoder, which codes alpha bit-exactly, and by
/// inputs without alpha.
///
/// Returns `GAMUT_STATUS_NULL_ARGUMENT` on a `NULL` encoder.
///
/// # Safety
///
/// `encoder` is `NULL` or a live encoder handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gamut_avif_encoder_set_alpha_quality(
    encoder: *mut GamutAvifEncoder,
    quality: u8,
) -> GamutStatus {
    unsafe {
        GamutAvifEncoder::configure(encoder, Some(quality), GAMUT_OK, |e, q| {
            e.with_alpha_quality(q)
        })
    }
}

/// Declares that an RGBA input's colour values are already premultiplied by alpha
/// (`AvifEncoder::with_premultiplied_alpha`) — a declaration, not a conversion; written as a
/// `prem` reference. Ignored by inputs without alpha.
///
/// Returns `GAMUT_STATUS_NULL_ARGUMENT` on a `NULL` encoder.
///
/// # Safety
///
/// `encoder` is `NULL` or a live encoder handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gamut_avif_encoder_set_premultiplied_alpha(
    encoder: *mut GamutAvifEncoder,
    premultiplied: bool,
) -> GamutStatus {
    unsafe {
        GamutAvifEncoder::configure(encoder, Some(premultiplied), GAMUT_OK, |e, p| {
            e.with_premultiplied_alpha(p)
        })
    }
}

/// Records an `irot` display rotation of `quarter_turns_ccw` anti-clockwise quarter turns
/// (`0..=3`; `0` writes no `irot`), applied by a reader — the stored pixels are unchanged
/// (`AvifEncoder::with_rotation`).
///
/// Returns `GAMUT_STATUS_INVALID_INPUT` for a value above `3`, `GAMUT_STATUS_NULL_ARGUMENT` on
/// a `NULL` encoder.
///
/// # Safety
///
/// `encoder` is `NULL` or a live encoder handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gamut_avif_encoder_set_rotation(
    encoder: *mut GamutAvifEncoder,
    quarter_turns_ccw: u8,
) -> GamutStatus {
    let value = match quarter_turns_ccw {
        0 => Some(Rotation::None),
        1 => Some(Rotation::Ccw90),
        2 => Some(Rotation::Ccw180),
        3 => Some(Rotation::Ccw270),
        _ => None,
    };
    unsafe {
        GamutAvifEncoder::configure(encoder, value, GAMUT_STATUS_INVALID_INPUT, |e, r| {
            e.with_rotation(r)
        })
    }
}

/// Records an `imir` display mirror, one of the `GAMUT_AVIF_MIRROR_*` constants, applied by a
/// reader — the stored pixels are unchanged (`AvifEncoder::with_mirror`). As in Rust, a mirror
/// once set can be replaced but not removed.
///
/// Returns `GAMUT_STATUS_INVALID_INPUT` for a value that is not one of the constants,
/// `GAMUT_STATUS_NULL_ARGUMENT` on a `NULL` encoder.
///
/// # Safety
///
/// `encoder` is `NULL` or a live encoder handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gamut_avif_encoder_set_mirror(
    encoder: *mut GamutAvifEncoder,
    mirror: u8,
) -> GamutStatus {
    let value = match mirror {
        GAMUT_AVIF_MIRROR_TOP_BOTTOM => Some(Mirror::TopBottom),
        GAMUT_AVIF_MIRROR_LEFT_RIGHT => Some(Mirror::LeftRight),
        _ => None,
    };
    unsafe {
        GamutAvifEncoder::configure(encoder, value, GAMUT_STATUS_INVALID_INPUT, |e, m| {
            e.with_mirror(m)
        })
    }
}

/// Embeds an ICC profile, carried verbatim as a `colr` `prof` box
/// (`AvifEncoder::with_icc_profile`). The bytes are copied; a later call replaces them.
///
/// Returns `GAMUT_STATUS_NULL_ARGUMENT` on a `NULL` encoder, or a `NULL` `data` with a
/// non-zero `len`.
///
/// # Safety
///
/// `encoder` is `NULL` or a live encoder handle; a non-`NULL` `data` points to `len` readable
/// bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gamut_avif_encoder_set_icc_profile(
    encoder: *mut GamutAvifEncoder,
    data: *const u8,
    len: usize,
) -> GamutStatus {
    // SAFETY: per this fn's contract.
    let value = unsafe { bytes_in(data, len) };
    unsafe {
        GamutAvifEncoder::configure(encoder, value, GAMUT_STATUS_NULL_ARGUMENT, |e, b| {
            e.with_icc_profile(b)
        })
    }
}

/// Attaches Exif metadata (`AvifEncoder::with_exif`): a **bare TIFF stream** (byte-order mark,
/// `42`, first-IFD offset — not the `Exif\0\0`-prefixed form). The encoder adds HEIF's 4-byte
/// `exif_tiff_header_offset` prefix. The bytes are copied verbatim, unvalidated; a later call
/// replaces them.
///
/// Returns `GAMUT_STATUS_NULL_ARGUMENT` on a `NULL` encoder, or a `NULL` `data` with a
/// non-zero `len`.
///
/// # Safety
///
/// `encoder` is `NULL` or a live encoder handle; a non-`NULL` `data` points to `len` readable
/// bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gamut_avif_encoder_set_exif(
    encoder: *mut GamutAvifEncoder,
    data: *const u8,
    len: usize,
) -> GamutStatus {
    // SAFETY: per this fn's contract.
    let value = unsafe { bytes_in(data, len) };
    unsafe {
        GamutAvifEncoder::configure(encoder, value, GAMUT_STATUS_NULL_ARGUMENT, |e, b| {
            e.with_exif(b)
        })
    }
}

/// Attaches an XMP packet, carried verbatim as an `application/rdf+xml` `mime` item
/// (`AvifEncoder::with_xmp`). The bytes are copied; a later call replaces them.
///
/// Returns `GAMUT_STATUS_NULL_ARGUMENT` on a `NULL` encoder, or a `NULL` `data` with a
/// non-zero `len`.
///
/// # Safety
///
/// `encoder` is `NULL` or a live encoder handle; a non-`NULL` `data` points to `len` readable
/// bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gamut_avif_encoder_set_xmp(
    encoder: *mut GamutAvifEncoder,
    data: *const u8,
    len: usize,
) -> GamutStatus {
    // SAFETY: per this fn's contract.
    let value = unsafe { bytes_in(data, len) };
    unsafe {
        GamutAvifEncoder::configure(encoder, value, GAMUT_STATUS_NULL_ARGUMENT, |e, b| {
            e.with_xmp(b)
        })
    }
}

/// Reserves a `len`-byte slot of zeros for a C2PA manifest store, framed as a top-level
/// `ContentProvenanceBox` (`box_purpose` `manifest`) after `ftyp`, for an external signer to
/// fill in after encoding (`AvifEncoder::with_c2pa_reserved`). After an encode,
/// `gamut_avif_encoder_c2pa_slot` reports where the slot landed; the signer writes a store of at
/// most `len` bytes there, and nothing else in the file moves.
///
/// This setter cannot reject `len`: as in Rust, an unusable length (below 8 bytes, or one too
/// large to frame) is refused by the encode that follows (`GAMUT_STATUS_INVALID_INPUT`, or
/// `GAMUT_STATUS_UNSUPPORTED` for a box at or beyond 4 GiB). This call, or
/// `gamut_avif_encoder_set_c2pa`, replaces any earlier one. Returns
/// `GAMUT_STATUS_NULL_ARGUMENT` on a `NULL` encoder.
///
/// # Safety
///
/// `encoder` is `NULL` or a live encoder handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gamut_avif_encoder_set_c2pa_reserved(
    encoder: *mut GamutAvifEncoder,
    len: usize,
) -> GamutStatus {
    unsafe {
        GamutAvifEncoder::configure(encoder, Some(len), GAMUT_OK, |e, l| e.with_c2pa_reserved(l))
    }
}

/// Writes `store` — a C2PA manifest store already computed over this exact output — into the
/// `ContentProvenanceBox` slot (`AvifEncoder::with_c2pa`): the re-encode half of the
/// reserve-then-fill flow, framed exactly as `gamut_avif_encoder_set_c2pa_reserved` with the
/// store in place of the zeros. The bytes are copied verbatim and unchecked, with no minimum
/// length. This call, or `gamut_avif_encoder_set_c2pa_reserved`, replaces any earlier one.
///
/// Returns `GAMUT_STATUS_NULL_ARGUMENT` on a `NULL` encoder, or a `NULL` `store` with a non-zero
/// `len`.
///
/// # Safety
///
/// `encoder` is `NULL` or a live encoder handle; a non-`NULL` `store` points to `len` readable
/// bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gamut_avif_encoder_set_c2pa(
    encoder: *mut GamutAvifEncoder,
    store: *const u8,
    len: usize,
) -> GamutStatus {
    // SAFETY: per this fn's contract.
    let value = unsafe { bytes_in(store, len) };
    unsafe {
        GamutAvifEncoder::configure(encoder, value, GAMUT_STATUS_NULL_ARGUMENT, |e, b| {
            e.with_c2pa(b)
        })
    }
}

/// Reports where the most recent encode through `encoder` placed the C2PA manifest-store slot
/// (`AvifEncodeReport::c2pa`, the range `AvifEncoder::encode_with_report` returns): on
/// `GAMUT_OK`, `*present` says whether there is one, and `*offset`/`*len` are its byte range
/// within that encode's output (`0`/`0` when absent). The slot is absent before the first
/// encode, after an encode that did not return `GAMUT_OK`, and when neither
/// `gamut_avif_encoder_set_c2pa_reserved` nor `gamut_avif_encoder_set_c2pa` was set; a
/// zero-length slot (`gamut_avif_encoder_set_c2pa` with an empty store) is present.
///
/// The range is the one a signer patches at — not a hash exclusion range, which C2PA's BMFF
/// binding expresses by box path. Changing a setting does not change the report; the next
/// encode does. Returns `GAMUT_STATUS_NULL_ARGUMENT` for a `NULL` argument, writing
/// `false`/`0`/`0` through the out-params when all three are non-`NULL`.
///
/// # Safety
///
/// `encoder` is `NULL` or a live encoder handle; `present`, `offset` and `len` are `NULL` or
/// writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gamut_avif_encoder_c2pa_slot(
    encoder: *const GamutAvifEncoder,
    present: *mut bool,
    offset: *mut usize,
    len: *mut usize,
) -> GamutStatus {
    guard::status(|| {
        if present.is_null() || offset.is_null() || len.is_null() {
            return GAMUT_STATUS_NULL_ARGUMENT;
        }
        // SAFETY: the three out-params are non-null and writable per contract.
        let write = |slot: Option<Range<usize>>| unsafe {
            present.write(slot.is_some());
            let range = slot.unwrap_or_default();
            offset.write(range.start);
            len.write(range.len());
        };
        // SAFETY: null is rejected here; a non-null handle is live per contract.
        let Some(this) = (unsafe { encoder.as_ref() }) else {
            write(None);
            return GAMUT_STATUS_NULL_ARGUMENT;
        };
        let slot = this.c2pa.take();
        this.c2pa.set(slot.clone());
        write(slot);
        GAMUT_OK
    })
}

/// Encodes interleaved 8-bit RGB samples to an AVIF file.
///
/// `samples_len` counts samples (`width * height * 3`). On `GAMUT_OK`, `*out`/`*out_len` hold a
/// Rust-owned buffer to release with `gamut_bytes_free(*out, *out_len)`; on any other status they
/// are `NULL`/`0` (whenever they are non-`NULL`). Returns `GAMUT_STATUS_NULL_ARGUMENT` for a
/// `NULL` encoder, `samples`, `out` or `out_len`, `GAMUT_STATUS_INVALID_INPUT` when
/// `samples_len` does not match the dimensions or a dimension is zero, and otherwise the
/// encoder's own error status.
///
/// # Safety
///
/// `encoder` is `NULL` or a live encoder handle; a non-`NULL` `samples` points to `samples_len`
/// readable samples; `out` and `out_len` are `NULL` or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gamut_avif_encoder_encode_rgb8(
    encoder: *const GamutAvifEncoder,
    samples: *const u8,
    samples_len: usize,
    width: u32,
    height: u32,
    out: *mut *mut u8,
    out_len: *mut usize,
) -> GamutStatus {
    unsafe {
        GamutAvifEncoder::encode::<Rgb8>(encoder, samples, samples_len, width, height, out, out_len)
    }
}

/// Encodes interleaved 8-bit RGBA samples (unassociated alpha unless
/// `gamut_avif_encoder_set_premultiplied_alpha` says otherwise) to an AVIF file with an alpha
/// auxiliary. `samples_len` counts samples (`width * height * 4`); otherwise as
/// `gamut_avif_encoder_encode_rgb8`.
///
/// # Safety
///
/// As `gamut_avif_encoder_encode_rgb8`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gamut_avif_encoder_encode_rgba8(
    encoder: *const GamutAvifEncoder,
    samples: *const u8,
    samples_len: usize,
    width: u32,
    height: u32,
    out: *mut *mut u8,
    out_len: *mut usize,
) -> GamutStatus {
    unsafe {
        GamutAvifEncoder::encode::<Rgba8>(
            encoder,
            samples,
            samples_len,
            width,
            height,
            out,
            out_len,
        )
    }
}

/// Encodes 8-bit greyscale samples to a monochrome AVIF file. `samples_len` counts samples
/// (`width * height`); otherwise as `gamut_avif_encoder_encode_rgb8`.
///
/// # Safety
///
/// As `gamut_avif_encoder_encode_rgb8`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gamut_avif_encoder_encode_gray8(
    encoder: *const GamutAvifEncoder,
    samples: *const u8,
    samples_len: usize,
    width: u32,
    height: u32,
    out: *mut *mut u8,
    out_len: *mut usize,
) -> GamutStatus {
    unsafe {
        GamutAvifEncoder::encode::<Gray8>(
            encoder,
            samples,
            samples_len,
            width,
            height,
            out,
            out_len,
        )
    }
}

/// Encodes interleaved 16-bit RGB samples (full 16-bit scale), coded at the depth
/// `gamut_avif_encoder_set_bit_depth` selects. `samples_len` counts samples
/// (`width * height * 3`), and `samples` must be aligned for `uint16_t`
/// (`GAMUT_STATUS_INVALID_INPUT` otherwise); otherwise as `gamut_avif_encoder_encode_rgb8`.
///
/// # Safety
///
/// As `gamut_avif_encoder_encode_rgb8`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gamut_avif_encoder_encode_rgb16(
    encoder: *const GamutAvifEncoder,
    samples: *const u16,
    samples_len: usize,
    width: u32,
    height: u32,
    out: *mut *mut u8,
    out_len: *mut usize,
) -> GamutStatus {
    unsafe {
        GamutAvifEncoder::encode::<Rgb16>(
            encoder,
            samples,
            samples_len,
            width,
            height,
            out,
            out_len,
        )
    }
}

/// Encodes interleaved 16-bit RGBA samples (full 16-bit scale) with an alpha auxiliary.
/// `samples_len` counts samples (`width * height * 4`); otherwise as
/// `gamut_avif_encoder_encode_rgb16`.
///
/// # Safety
///
/// As `gamut_avif_encoder_encode_rgb8`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gamut_avif_encoder_encode_rgba16(
    encoder: *const GamutAvifEncoder,
    samples: *const u16,
    samples_len: usize,
    width: u32,
    height: u32,
    out: *mut *mut u8,
    out_len: *mut usize,
) -> GamutStatus {
    unsafe {
        GamutAvifEncoder::encode::<Rgba16>(
            encoder,
            samples,
            samples_len,
            width,
            height,
            out,
            out_len,
        )
    }
}

/// Opaque handle over a parsed AVIF file (`gamut::avif::AvifImage`, the role-typed view
/// `AvifContainer::parse` builds). It owns every payload, so it outlives the input buffer.
pub struct GamutAvifImage {
    inner: AvifImage,
}

/// A byte selector over the parsed image: `Ok(None)` is "absent" (an `OK` status with a
/// `NULL`/`0` result), `Err` a status to return.
type Select = for<'a> fn(&'a AvifImage) -> Result<Option<&'a [u8]>, GamutStatus>;

impl GamutAvifImage {
    /// Runs `body` against the live image behind `image`, after clearing and null-checking the
    /// out-parameters `body` writes through.
    ///
    /// # Safety
    ///
    /// `image` is `NULL` or a live pointer from `gamut_avif_image_parse`.
    unsafe fn read(
        image: *const Self,
        outs_present: bool,
        clear: impl FnOnce(),
        body: impl FnOnce(&AvifImage) -> GamutStatus,
    ) -> GamutStatus {
        guard::status(|| {
            if !outs_present {
                return GAMUT_STATUS_NULL_ARGUMENT;
            }
            clear();
            // SAFETY: null is rejected here; a non-null handle is live per contract.
            match unsafe { image.as_ref() } {
                Some(this) => body(&this.inner),
                None => GAMUT_STATUS_NULL_ARGUMENT,
            }
        })
    }

    /// The shared body of every byte accessor: writes the selected slice, borrowed from the
    /// handle, through `(data, len)`.
    ///
    /// # Safety
    ///
    /// `image` as in [`read`](Self::read); `data` and `len` are `NULL` or writable.
    unsafe fn bytes(
        image: *const Self,
        data: *mut *const u8,
        len: *mut usize,
        select: Select,
    ) -> GamutStatus {
        let outs_present = !data.is_null() && !len.is_null();
        // SAFETY (both closures): `read` calls them only once `outs_present` holds, and the
        // out-params are writable per this fn's contract.
        let write = |bytes: *const u8, count: usize| unsafe {
            data.write(bytes);
            len.write(count);
        };
        unsafe {
            Self::read(
                image,
                outs_present,
                || write(ptr::null(), 0),
                |inner| match select(inner) {
                    Ok(Some(bytes)) => {
                        write(bytes.as_ptr(), bytes.len());
                        GAMUT_OK
                    }
                    Ok(None) => GAMUT_OK,
                    Err(status) => status,
                },
            )
        }
    }
}

/// The AV1 codestream of `item`, if it is a coded AV1 item; a derived item (`grid`, `iden`,
/// `iovl`) or another codec carries no single codestream, so it is `GAMUT_STATUS_UNSUPPORTED`.
fn av1_payload(item: AvifItem<'_>) -> Result<&[u8], GamutStatus> {
    if item.kind().is_av1() {
        Ok(&item.as_isobmff_item().payload)
    } else {
        Err(GAMUT_STATUS_UNSUPPORTED)
    }
}

/// Parses an AVIF file into an owned image handle (`AvifContainer::parse`).
///
/// On `GAMUT_OK`, `*out` is a handle to release with `gamut_avif_image_free`; it copies what it
/// keeps, so `data` may be freed as soon as this returns. On any other status `*out` is `NULL`.
/// Returns `GAMUT_STATUS_NULL_ARGUMENT` for a `NULL` `out`, or a `NULL` `data` with a non-zero
/// `len`, and otherwise the parser's error status (`GAMUT_STATUS_INVALID_INPUT` for a malformed
/// file, `GAMUT_STATUS_UNSUPPORTED` for an out-of-scope one such as an image sequence).
///
/// # Safety
///
/// A non-`NULL` `data` points to `len` readable bytes; `out` is `NULL` or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gamut_avif_image_parse(
    data: *const u8,
    len: usize,
    out: *mut *mut GamutAvifImage,
) -> GamutStatus {
    guard::status(|| {
        if out.is_null() {
            return GAMUT_STATUS_NULL_ARGUMENT;
        }
        // SAFETY: non-null and writable per contract.
        unsafe { out.write(ptr::null_mut()) };
        // SAFETY: per this fn's contract.
        let Some(bytes) = (unsafe { bytes_in(data, len) }) else {
            return GAMUT_STATUS_NULL_ARGUMENT;
        };
        match AvifContainer::parse(bytes) {
            Ok(container) => {
                // The container borrows `data`; the semantic view it built owns its payloads,
                // so the handle keeps (a copy of) only that and no borrow crosses the boundary.
                let handle = Box::new(GamutAvifImage {
                    inner: container.image().clone(),
                });
                // SAFETY: as the clearing write above.
                unsafe { out.write(Box::into_raw(handle)) };
                GAMUT_OK
            }
            Err(error) => status_of(&error),
        }
    })
}

/// Frees an image created by `gamut_avif_image_parse`, invalidating every pointer its
/// accessors returned. No-op on `NULL`.
///
/// # Safety
///
/// `image` is `NULL` or a pointer returned by `gamut_avif_image_parse` that has not already
/// been freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gamut_avif_image_free(image: *mut GamutAvifImage) {
    guard::unit(|| {
        if !image.is_null() {
            // SAFETY: per this fn's contract.
            drop(unsafe { Box::from_raw(image) });
        }
    });
}

/// The primary item's AV1 codestream (the `av01` item payload: a low-overhead OBU stream),
/// borrowed from `image`.
///
/// Returns `GAMUT_STATUS_UNSUPPORTED` (with `*data == NULL`, `*len == 0`) when the primary item
/// is derived (`grid`, `iden`, `iovl`) or not AV1, so it has no single codestream;
/// `GAMUT_STATUS_NULL_ARGUMENT` for a `NULL` argument.
///
/// # Safety
///
/// `image` is `NULL` or a live image handle; `data` and `len` are `NULL` or writable. The
/// returned pointer is valid until `gamut_avif_image_free`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gamut_avif_image_primary_codestream(
    image: *const GamutAvifImage,
    data: *mut *const u8,
    len: *mut usize,
) -> GamutStatus {
    unsafe {
        GamutAvifImage::bytes(image, data, len, |inner| {
            av1_payload(inner.primary_item()).map(Some)
        })
    }
}

/// The AV1 codestream of the primary item's alpha auxiliary, borrowed from `image`.
///
/// `GAMUT_OK` with `*data == NULL`, `*len == 0` when the image has no alpha. Returns
/// `GAMUT_STATUS_UNSUPPORTED` when the alpha item is derived or not AV1;
/// `GAMUT_STATUS_NULL_ARGUMENT` for a `NULL` argument.
///
/// # Safety
///
/// As `gamut_avif_image_primary_codestream`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gamut_avif_image_alpha_codestream(
    image: *const GamutAvifImage,
    data: *mut *const u8,
    len: *mut usize,
) -> GamutStatus {
    unsafe {
        GamutAvifImage::bytes(image, data, len, |inner| {
            inner
                .alpha_auxiliary_of(inner.primary_item().id())
                .map(av1_payload)
                .transpose()
        })
    }
}

/// The primary item's `av1C` AV1 codec-configuration record body (AV1-ISOBMFF §2.3.3: four
/// header bytes, then any `configOBUs`), borrowed from `image`.
///
/// `GAMUT_OK` with `*data == NULL`, `*len == 0` when the item carries no `av1C`. Returns
/// `GAMUT_STATUS_UNSUPPORTED` when the primary item is derived or not AV1, as
/// `gamut_avif_image_primary_codestream`; `GAMUT_STATUS_NULL_ARGUMENT` for a `NULL` argument.
///
/// # Safety
///
/// As `gamut_avif_image_primary_codestream`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gamut_avif_image_av1c(
    image: *const GamutAvifImage,
    data: *mut *const u8,
    len: *mut usize,
) -> GamutStatus {
    unsafe {
        GamutAvifImage::bytes(image, data, len, |inner| {
            let primary = inner.primary_item();
            av1_payload(primary)?;
            Ok(primary
                .codec_configuration()
                .filter(|(kind, _)| *kind == b"av1C")
                .map(|(_, body)| body))
        })
    }
}

/// The Exif payload describing the primary item, borrowed from `image` — **raw**, including
/// HEIF's 4-byte big-endian `exif_tiff_header_offset` prefix (as `AvifImage::exif` exposes it
/// and libheif returns it); the TIFF stream starts that many bytes after the prefix.
///
/// `GAMUT_OK` with `*data == NULL`, `*len == 0` when there is none;
/// `GAMUT_STATUS_NULL_ARGUMENT` for a `NULL` argument.
///
/// # Safety
///
/// As `gamut_avif_image_primary_codestream`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gamut_avif_image_exif(
    image: *const GamutAvifImage,
    data: *mut *const u8,
    len: *mut usize,
) -> GamutStatus {
    unsafe {
        GamutAvifImage::bytes(image, data, len, |inner| {
            Ok(inner.exif().map(|item| &item.as_isobmff_item().payload[..]))
        })
    }
}

/// The XMP packet describing the primary item, borrowed from `image`, verbatim.
///
/// `GAMUT_OK` with `*data == NULL`, `*len == 0` when there is none;
/// `GAMUT_STATUS_NULL_ARGUMENT` for a `NULL` argument.
///
/// # Safety
///
/// As `gamut_avif_image_primary_codestream`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gamut_avif_image_xmp(
    image: *const GamutAvifImage,
    data: *mut *const u8,
    len: *mut usize,
) -> GamutStatus {
    unsafe {
        GamutAvifImage::bytes(image, data, len, |inner| {
            Ok(inner.xmp().map(|item| &item.as_isobmff_item().payload[..]))
        })
    }
}

/// The primary item's ICC profile (a `colr` box of type `prof` or `rICC`), borrowed from
/// `image`, verbatim.
///
/// `GAMUT_OK` with `*data == NULL`, `*len == 0` when there is none;
/// `GAMUT_STATUS_NULL_ARGUMENT` for a `NULL` argument.
///
/// # Safety
///
/// As `gamut_avif_image_primary_codestream`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gamut_avif_image_icc_profile(
    image: *const GamutAvifImage,
    data: *mut *const u8,
    len: *mut usize,
) -> GamutStatus {
    unsafe {
        GamutAvifImage::bytes(image, data, len, |inner| {
            Ok(inner.primary_item().icc_profile())
        })
    }
}

/// Writes whether the primary item's colour values are premultiplied by its alpha auxiliary
/// (a `prem` reference; `AvifImage::is_premultiplied`) to `*premultiplied`.
///
/// Returns `GAMUT_STATUS_NULL_ARGUMENT` for a `NULL` argument.
///
/// # Safety
///
/// `image` is `NULL` or a live image handle; `premultiplied` is `NULL` or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gamut_avif_image_is_premultiplied(
    image: *const GamutAvifImage,
    premultiplied: *mut bool,
) -> GamutStatus {
    // SAFETY (both closures): `read` calls them only once `premultiplied` is non-null, and it is
    // writable per this fn's contract.
    let write = |value: bool| unsafe { premultiplied.write(value) };
    unsafe {
        GamutAvifImage::read(
            image,
            !premultiplied.is_null(),
            || write(false),
            |inner| {
                write(inner.is_premultiplied(inner.primary_item().id()));
                GAMUT_OK
            },
        )
    }
}

/// Writes the primary item's stored dimensions (its `ispe` property, before any `clap`/`irot`
/// display transform) to `*width` and `*height`.
///
/// Returns `GAMUT_STATUS_INVALID_INPUT` (with both `0`) when the primary item has no usable
/// `ispe`, which every image item must carry (ISO/IEC 23008-12 §6.5.3);
/// `GAMUT_STATUS_NULL_ARGUMENT` for a `NULL` argument.
///
/// # Safety
///
/// `image` is `NULL` or a live image handle; `width` and `height` are `NULL` or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gamut_avif_image_dimensions(
    image: *const GamutAvifImage,
    width: *mut u32,
    height: *mut u32,
) -> GamutStatus {
    // SAFETY (both closures): `read` calls them only once both out-params are non-null, and
    // they are writable per this fn's contract.
    let write = |dims: Dimensions| unsafe {
        width.write(dims.width);
        height.write(dims.height);
    };
    unsafe {
        GamutAvifImage::read(
            image,
            !width.is_null() && !height.is_null(),
            || {
                write(Dimensions {
                    width: 0,
                    height: 0,
                })
            },
            |inner| match inner.primary_item().dimensions() {
                Some(dims) => {
                    write(dims);
                    GAMUT_OK
                }
                None => GAMUT_STATUS_INVALID_INPUT,
            },
        )
    }
}

#[cfg(test)]
mod tests {
    //! The entry points are called exactly as C calls them. The oracle for the encode side is
    //! the Rust API each shim wraps (same configuration ⇒ same bytes), and for the parse side
    //! `AvifContainer`, the parser the handle is built from (issue #666).

    use super::*;
    use crate::gamut_bytes_free;

    /// One `gamut_avif_encoder_encode_*` entry point, generic over its sample type.
    type EncodeFn<S> = unsafe extern "C" fn(
        *const GamutAvifEncoder,
        *const S,
        usize,
        u32,
        u32,
        *mut *mut u8,
        *mut usize,
    ) -> GamutStatus;

    /// Runs one C encode, copying the output out and releasing it through `gamut_bytes_free`.
    fn c_encode<S>(
        encoder: *const GamutAvifEncoder,
        encode: EncodeFn<S>,
        samples: &[S],
        width: u32,
        height: u32,
    ) -> Result<Vec<u8>, GamutStatus> {
        let mut out = ptr::null_mut();
        let mut out_len = 0;
        let status = unsafe {
            encode(
                encoder,
                samples.as_ptr(),
                samples.len(),
                width,
                height,
                &mut out,
                &mut out_len,
            )
        };
        if status != GAMUT_OK {
            assert!(
                out.is_null() && out_len == 0,
                "a failed encode returns no buffer"
            );
            return Err(status);
        }
        let bytes = unsafe { core::slice::from_raw_parts(out, out_len) }.to_vec();
        unsafe { gamut_bytes_free(out, out_len) };
        Ok(bytes)
    }

    /// The same encode through the Rust API, statuses mapped as the boundary maps them.
    fn rust_encode<P: Pixel>(
        encoder: &AvifEncoder,
        samples: &[P::Sample],
        width: u32,
        height: u32,
    ) -> Result<Vec<u8>, GamutStatus>
    where
        AvifEncoder: EncodeImage<P>,
    {
        let image =
            ImageRef::<P>::new(samples, Dimensions { width, height }).map_err(|e| status_of(&e))?;
        encoder.encode_to_vec(image).map_err(|e| status_of(&e))
    }

    /// A 4×4 gradient with `channels` interleaved channels and a varying last channel.
    fn ramp8(channels: usize) -> Vec<u8> {
        (0..16 * channels).map(|i| (i * 13 % 256) as u8).collect()
    }

    fn ramp16(channels: usize) -> Vec<u16> {
        (0..16 * channels)
            .map(|i| (i * 3331 % 65536) as u16)
            .collect()
    }

    /// Parses through the C entry point, panicking on failure.
    fn c_parse(bytes: &[u8]) -> *mut GamutAvifImage {
        let mut image = ptr::null_mut();
        assert_eq!(
            unsafe { gamut_avif_image_parse(bytes.as_ptr(), bytes.len(), &mut image) },
            GAMUT_OK
        );
        assert!(!image.is_null());
        image
    }

    /// One byte accessor's result, copied out of the handle.
    type Accessor =
        unsafe extern "C" fn(*const GamutAvifImage, *mut *const u8, *mut usize) -> GamutStatus;

    fn read(
        image: *const GamutAvifImage,
        accessor: Accessor,
    ) -> Result<Option<Vec<u8>>, GamutStatus> {
        let mut data = ptr::null();
        let mut len = usize::MAX;
        let status = unsafe { accessor(image, &mut data, &mut len) };
        if status != GAMUT_OK {
            assert!(
                data.is_null() && len == 0,
                "a failed accessor returns nothing"
            );
            return Err(status);
        }
        if data.is_null() {
            assert_eq!(len, 0, "an absent payload has no length");
            return Ok(None);
        }
        Ok(Some(
            unsafe { core::slice::from_raw_parts(data, len) }.to_vec(),
        ))
    }

    #[test]
    fn parse_accessors_agree_with_avif_container() {
        let tiff = b"MM\0\x2a\0\0\0\x08\0\0".as_slice();
        let xmp = b"<x:xmpmeta xmlns:x='adobe:ns:meta/'/>".as_slice();
        let icc = b"not-really-an-icc-profile".as_slice();
        let encoder = gamut_avif_encoder_new();
        unsafe {
            assert_eq!(
                gamut_avif_encoder_set_exif(encoder, tiff.as_ptr(), tiff.len()),
                GAMUT_OK
            );
            assert_eq!(
                gamut_avif_encoder_set_xmp(encoder, xmp.as_ptr(), xmp.len()),
                GAMUT_OK
            );
            assert_eq!(
                gamut_avif_encoder_set_icc_profile(encoder, icc.as_ptr(), icc.len()),
                GAMUT_OK
            );
            assert_eq!(
                gamut_avif_encoder_set_premultiplied_alpha(encoder, true),
                GAMUT_OK
            );
        }
        let file = c_encode(encoder, gamut_avif_encoder_encode_rgba8, &ramp8(4), 4, 4).unwrap();
        unsafe { gamut_avif_encoder_free(encoder) };

        let container = AvifContainer::parse(&file).unwrap();
        let oracle = container.image();
        let primary = oracle.primary_item();
        let alpha = oracle.alpha_auxiliary_of(primary.id()).unwrap();

        let image = c_parse(&file);
        let payload = |item: AvifItem<'_>| Some(item.as_isobmff_item().payload.clone());
        assert_eq!(
            read(image, gamut_avif_image_primary_codestream),
            Ok(payload(primary))
        );
        assert_eq!(
            read(image, gamut_avif_image_alpha_codestream),
            Ok(payload(alpha))
        );
        assert_ne!(
            payload(primary),
            payload(alpha),
            "the two codestreams are told apart"
        );
        assert_eq!(
            read(image, gamut_avif_image_av1c),
            Ok(primary.codec_configuration().map(|(_, body)| body.to_vec()))
        );
        assert_eq!(
            read(image, gamut_avif_image_exif),
            Ok(oracle.exif().and_then(payload))
        );
        // The raw Exif payload is the caller's TIFF stream behind a zero header offset.
        assert_eq!(
            read(image, gamut_avif_image_exif),
            Ok(Some([&[0; 4], tiff].concat()))
        );
        assert_eq!(read(image, gamut_avif_image_xmp), Ok(Some(xmp.to_vec())));
        assert_eq!(
            read(image, gamut_avif_image_icc_profile),
            Ok(Some(icc.to_vec()))
        );

        let mut premultiplied = false;
        assert_eq!(
            unsafe { gamut_avif_image_is_premultiplied(image, &mut premultiplied) },
            GAMUT_OK
        );
        assert!(premultiplied);
        let (mut width, mut height) = (0, 0);
        assert_eq!(
            unsafe { gamut_avif_image_dimensions(image, &mut width, &mut height) },
            GAMUT_OK
        );
        assert_eq!((width, height), (4, 4));
        unsafe { gamut_avif_image_free(image) };
    }

    #[test]
    fn parse_reports_absent_payloads_as_null() {
        let encoder = gamut_avif_encoder_new();
        let file = c_encode(encoder, gamut_avif_encoder_encode_rgb8, &ramp8(3), 4, 4).unwrap();
        unsafe { gamut_avif_encoder_free(encoder) };

        let image = c_parse(&file);
        for accessor in [
            gamut_avif_image_alpha_codestream as Accessor,
            gamut_avif_image_exif,
            gamut_avif_image_xmp,
            gamut_avif_image_icc_profile,
        ] {
            assert_eq!(read(image, accessor), Ok(None));
        }
        let mut premultiplied = true;
        assert_eq!(
            unsafe { gamut_avif_image_is_premultiplied(image, &mut premultiplied) },
            GAMUT_OK
        );
        assert!(!premultiplied);
        unsafe { gamut_avif_image_free(image) };
    }

    #[test]
    fn parse_boundary_rejects_null_and_malformed_input() {
        let mut image = ptr::NonNull::<GamutAvifImage>::dangling().as_ptr();
        assert_eq!(
            unsafe { gamut_avif_image_parse(ptr::null(), 0, ptr::null_mut()) },
            GAMUT_STATUS_NULL_ARGUMENT
        );
        assert_eq!(
            unsafe { gamut_avif_image_parse(ptr::null(), 8, &mut image) },
            GAMUT_STATUS_NULL_ARGUMENT
        );
        assert!(image.is_null(), "a failed parse clears the out-param");

        let junk = [0u8; 16];
        image = ptr::NonNull::dangling().as_ptr();
        assert_eq!(
            unsafe { gamut_avif_image_parse(junk.as_ptr(), junk.len(), &mut image) },
            GAMUT_STATUS_INVALID_INPUT
        );
        assert!(image.is_null());

        // Every accessor rejects a null handle and null out-params.
        for accessor in [
            gamut_avif_image_primary_codestream as Accessor,
            gamut_avif_image_alpha_codestream,
            gamut_avif_image_av1c,
            gamut_avif_image_exif,
            gamut_avif_image_xmp,
            gamut_avif_image_icc_profile,
        ] {
            assert_eq!(read(ptr::null(), accessor), Err(GAMUT_STATUS_NULL_ARGUMENT));
            let mut len = 0;
            let mut data = ptr::null();
            assert_eq!(
                unsafe { accessor(ptr::null(), ptr::null_mut(), &mut len) },
                GAMUT_STATUS_NULL_ARGUMENT
            );
            assert_eq!(
                unsafe { accessor(ptr::null(), &mut data, ptr::null_mut()) },
                GAMUT_STATUS_NULL_ARGUMENT
            );
        }
        let mut flag = true;
        assert_eq!(
            unsafe { gamut_avif_image_is_premultiplied(ptr::null(), &mut flag) },
            GAMUT_STATUS_NULL_ARGUMENT
        );
        assert!(
            !flag,
            "the out-param is cleared before the handle is checked"
        );
        let (mut width, mut height) = (7, 7);
        assert_eq!(
            unsafe { gamut_avif_image_dimensions(ptr::null(), &mut width, &mut height) },
            GAMUT_STATUS_NULL_ARGUMENT
        );
        assert_eq!((width, height), (0, 0));
        assert_eq!(
            unsafe { gamut_avif_image_dimensions(ptr::null(), &mut width, ptr::null_mut()) },
            GAMUT_STATUS_NULL_ARGUMENT
        );

        // `_free` is null-tolerant.
        unsafe { gamut_avif_image_free(ptr::null_mut()) };
    }

    /// Encodes `samples` through the C handle `configure` builds and through the Rust encoder
    /// `rust` describes, asserting the two agree byte for byte (or on the same error status).
    fn assert_shim<P: Pixel>(
        new: extern "C" fn() -> *mut GamutAvifEncoder,
        configure: impl FnOnce(*mut GamutAvifEncoder),
        rust: AvifEncoder,
        encode: EncodeFn<P::Sample>,
        samples: &[P::Sample],
    ) -> Result<Vec<u8>, GamutStatus>
    where
        AvifEncoder: EncodeImage<P>,
    {
        let encoder = new();
        configure(encoder);
        let c = c_encode(encoder, encode, samples, 4, 4);
        unsafe { gamut_avif_encoder_free(encoder) };
        assert_eq!(c, rust_encode::<P>(&rust, samples, 4, 4));
        c
    }

    extern "C" fn new_lossy_50() -> *mut GamutAvifEncoder {
        gamut_avif_encoder_new_lossy(50)
    }

    #[test]
    fn each_encode_entry_point_matches_its_rust_layout() {
        let keep = |_| {};
        let lossless = || AvifEncoder::new();
        assert_shim::<Rgb8>(
            gamut_avif_encoder_new,
            keep,
            lossless(),
            gamut_avif_encoder_encode_rgb8,
            &ramp8(3),
        )
        .unwrap();
        assert_shim::<Rgba8>(
            gamut_avif_encoder_new,
            keep,
            lossless(),
            gamut_avif_encoder_encode_rgba8,
            &ramp8(4),
        )
        .unwrap();
        assert_shim::<Gray8>(
            gamut_avif_encoder_new,
            keep,
            lossless(),
            gamut_avif_encoder_encode_gray8,
            &ramp8(1),
        )
        .unwrap();
        assert_shim::<Rgb16>(
            gamut_avif_encoder_new,
            keep,
            lossless(),
            gamut_avif_encoder_encode_rgb16,
            &ramp16(3),
        )
        .unwrap();
        assert_shim::<Rgba16>(
            gamut_avif_encoder_new,
            keep,
            lossless(),
            gamut_avif_encoder_encode_rgba16,
            &ramp16(4),
        )
        .unwrap();
        let lossy = assert_shim::<Rgb8>(
            new_lossy_50,
            keep,
            AvifEncoder::lossy(50),
            gamut_avif_encoder_encode_rgb8,
            &ramp8(3),
        );
        let lossless = assert_shim::<Rgb8>(
            gamut_avif_encoder_new,
            keep,
            lossless(),
            gamut_avif_encoder_encode_rgb8,
            &ramp8(3),
        );
        assert_ne!(
            lossy, lossless,
            "the lossy constructor is not the lossless one"
        );
    }

    /// The Rust `with_*` call a C setter value must be equivalent to.
    type Builder = fn(AvifEncoder) -> AvifEncoder;

    /// The input a setter test encodes: the one whose path the setting reaches.
    #[derive(Clone, Copy)]
    enum Layout {
        Rgb8,
        Rgba8,
        Rgba16,
    }

    /// Asserts each `(C value, Rust builder)` pair produces the Rust encoder's bytes, and that
    /// the values are not all one encode (so each setter is observable in the output).
    fn assert_setter<T: Copy>(
        set: unsafe extern "C" fn(*mut GamutAvifEncoder, T) -> GamutStatus,
        cases: &[(T, Builder)],
        layout: Layout,
    ) {
        let mut outputs = Vec::new();
        for &(value, with) in cases {
            let configure = |encoder| assert_eq!(unsafe { set(encoder, value) }, GAMUT_OK);
            let rust = with(AvifEncoder::lossy(50));
            outputs.push(match layout {
                Layout::Rgb8 => assert_shim::<Rgb8>(
                    new_lossy_50,
                    configure,
                    rust,
                    gamut_avif_encoder_encode_rgb8,
                    &ramp8(3),
                ),
                Layout::Rgba8 => assert_shim::<Rgba8>(
                    new_lossy_50,
                    configure,
                    rust,
                    gamut_avif_encoder_encode_rgba8,
                    &ramp8(4),
                ),
                Layout::Rgba16 => assert_shim::<Rgba16>(
                    new_lossy_50,
                    configure,
                    rust,
                    gamut_avif_encoder_encode_rgba16,
                    &ramp16(4),
                ),
            });
        }
        outputs.dedup();
        assert_eq!(
            outputs.len(),
            cases.len(),
            "every value encodes differently"
        );
    }

    #[test]
    fn matrix_setter_maps_h273_code_points() {
        assert_setter(
            gamut_avif_encoder_set_matrix,
            &[
                (0, |e| e.with_matrix(MatrixCoefficients::Identity)),
                (1, |e| e.with_matrix(MatrixCoefficients::Bt709)),
                (6, |e| e.with_matrix(MatrixCoefficients::Bt601)),
                (9, |e| e.with_matrix(MatrixCoefficients::Bt2020Ncl)),
            ],
            Layout::Rgba8,
        );
    }

    #[test]
    fn primaries_setter_maps_h273_code_points() {
        assert_setter(
            gamut_avif_encoder_set_primaries,
            &[
                (1, |e| e.with_primaries(ColourPrimaries::Bt709)),
                (9, |e| e.with_primaries(ColourPrimaries::Bt2020)),
                (12, |e| e.with_primaries(ColourPrimaries::DisplayP3)),
            ],
            Layout::Rgba8,
        );
    }

    #[test]
    fn transfer_setter_maps_h273_code_points() {
        assert_setter(
            gamut_avif_encoder_set_transfer,
            &[
                (13, |e| e.with_transfer(TransferCharacteristics::Srgb)),
                (16, |e| e.with_transfer(TransferCharacteristics::Pq)),
                (18, |e| e.with_transfer(TransferCharacteristics::Hlg)),
            ],
            Layout::Rgba8,
        );
    }

    #[test]
    fn color_range_setter_maps_the_full_range_flag() {
        assert_setter(
            gamut_avif_encoder_set_color_range,
            &[
                (0, |e| e.with_color_range(ColorRange::Limited)),
                (1, |e| e.with_color_range(ColorRange::Full)),
            ],
            Layout::Rgba8,
        );
    }

    #[test]
    fn chroma_setter_maps_each_constant() {
        assert_setter(
            gamut_avif_encoder_set_chroma,
            &[
                (GAMUT_CHROMA_SUBSAMPLING_444, |e| {
                    e.with_chroma(ChromaSubsampling::Cs444)
                }),
                (GAMUT_CHROMA_SUBSAMPLING_422, |e| {
                    e.with_chroma(ChromaSubsampling::Cs422)
                }),
                (GAMUT_CHROMA_SUBSAMPLING_420, |e| {
                    e.with_chroma(ChromaSubsampling::Cs420)
                }),
                (GAMUT_CHROMA_SUBSAMPLING_400, |e| {
                    e.with_chroma(ChromaSubsampling::Cs400)
                }),
            ],
            // RGB, not RGBA: the RGBA path codes 4:4:4 whatever the chroma knob says.
            Layout::Rgb8,
        );
    }

    #[test]
    fn bit_depth_setter_maps_bits() {
        assert_setter(
            gamut_avif_encoder_set_bit_depth,
            &[
                (8, |e| e.with_bit_depth(BitDepth::Eight)),
                (10, |e| e.with_bit_depth(BitDepth::Ten)),
                (12, |e| e.with_bit_depth(BitDepth::Twelve)),
            ],
            Layout::Rgba16,
        );
    }

    #[test]
    fn rotation_setter_maps_quarter_turns() {
        assert_setter(
            gamut_avif_encoder_set_rotation,
            &[
                (0, |e| e.with_rotation(Rotation::None)),
                (1, |e| e.with_rotation(Rotation::Ccw90)),
                (2, |e| e.with_rotation(Rotation::Ccw180)),
                (3, |e| e.with_rotation(Rotation::Ccw270)),
            ],
            Layout::Rgba8,
        );
    }

    /// A setter rebuilds the handle's encoder through a consuming `with_*` builder; the backend
    /// registry must travel with it (DESIGN.md decision 4 rests on this). The fake backend
    /// accepts every job, counts each `encode` through `ctx`, and fails it terminally, so the
    /// built-in tail can never mask a dropped registry: the encode fails only if it ran.
    #[test]
    fn setter_after_push_backend_keeps_the_backend() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        use gamut_codec_abi::{ABI_VERSION, EncodeConfig, ImageDesc, Status, WriteFn};

        unsafe extern "C" fn accept(_: *mut c_void, _: *const EncodeConfig) -> Status {
            Status::OK
        }
        unsafe extern "C" fn count_and_fail(
            ctx: *mut c_void,
            _: *const EncodeConfig,
            _: *const ImageDesc,
            _: WriteFn,
            _: *mut c_void,
        ) -> Status {
            // SAFETY: `ctx` is the test's live `AtomicUsize`, which outlives the handle.
            unsafe { &*ctx.cast_const().cast::<AtomicUsize>() }.fetch_add(1, Ordering::SeqCst);
            Status(-2)
        }
        let vtable = EncoderVTable {
            abi_version: ABI_VERSION,
            supports: Some(accept),
            encode: Some(count_and_fail),
            destroy: None,
        };
        let encodes = AtomicUsize::new(0);
        let ctx = ptr::from_ref(&encodes).cast_mut().cast();

        let encoder = gamut_avif_encoder_new();
        assert_eq!(
            unsafe { gamut_avif_encoder_push_backend(encoder, &vtable, ctx) },
            GAMUT_OK
        );
        assert_eq!(
            unsafe { gamut_avif_encoder_set_rotation(encoder, 1) },
            GAMUT_OK
        );
        let result = c_encode(encoder, gamut_avif_encoder_encode_rgb8, &ramp8(3), 4, 4);
        unsafe { gamut_avif_encoder_free(encoder) };

        assert_eq!(result, Err(GAMUT_STATUS_INVALID_INPUT));
        assert_eq!(encodes.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn mirror_setter_maps_each_constant() {
        assert_setter(
            gamut_avif_encoder_set_mirror,
            &[
                (GAMUT_AVIF_MIRROR_TOP_BOTTOM, |e| {
                    e.with_mirror(Mirror::TopBottom)
                }),
                (GAMUT_AVIF_MIRROR_LEFT_RIGHT, |e| {
                    e.with_mirror(Mirror::LeftRight)
                }),
            ],
            Layout::Rgba8,
        );
    }

    #[test]
    fn alpha_quality_setter_forwards_the_quality() {
        // Both values sit away from the handle's colour quality (50) and from each other, so
        // the forwarded value, not merely the call, decides the alpha auxiliary's bytes.
        assert_setter(
            gamut_avif_encoder_set_alpha_quality,
            &[
                (10, |e| e.with_alpha_quality(10)),
                (90, |e| e.with_alpha_quality(90)),
            ],
            Layout::Rgba8,
        );
    }

    #[test]
    fn premultiplied_setter_maps_the_flag() {
        assert_setter(
            gamut_avif_encoder_set_premultiplied_alpha,
            &[
                (false, |e| e.with_premultiplied_alpha(false)),
                (true, |e| e.with_premultiplied_alpha(true)),
            ],
            Layout::Rgba8,
        );
    }

    #[test]
    fn metadata_setters_forward_their_bytes() {
        let bytes = b"MM\0\x2a\0\0\0\x08\0\0";
        type BytesSetter =
            unsafe extern "C" fn(*mut GamutAvifEncoder, *const u8, usize) -> GamutStatus;
        type BytesBuilder = fn(AvifEncoder, &[u8]) -> AvifEncoder;
        let cases: [(BytesSetter, BytesBuilder); 3] = [
            (gamut_avif_encoder_set_icc_profile, |e, b| {
                e.with_icc_profile(b)
            }),
            (gamut_avif_encoder_set_exif, |e, b| e.with_exif(b)),
            (gamut_avif_encoder_set_xmp, |e, b| e.with_xmp(b)),
        ];
        let mut outputs = Vec::new();
        for (set, with) in cases {
            let configure = |encoder| {
                assert_eq!(
                    unsafe { set(encoder, bytes.as_ptr(), bytes.len()) },
                    GAMUT_OK
                )
            };
            let rust = with(AvifEncoder::new(), bytes);
            outputs.push(assert_shim::<Rgb8>(
                gamut_avif_encoder_new,
                configure,
                rust,
                gamut_avif_encoder_encode_rgb8,
                &ramp8(3),
            ));
            // `NULL` with a zero length is the empty payload, not an error.
            let configure = |encoder| assert_eq!(unsafe { set(encoder, ptr::null(), 0) }, GAMUT_OK);
            let rust = with(AvifEncoder::new(), &[]);
            // Only agreement matters here: an empty payload may be one the encoder refuses.
            let _ = assert_shim::<Rgb8>(
                gamut_avif_encoder_new,
                configure,
                rust,
                gamut_avif_encoder_encode_rgb8,
                &ramp8(3),
            );
        }
        outputs.dedup();
        assert_eq!(outputs.len(), 3, "each payload lands in its own box");
    }

    /// The handle's C2PA slot report as `gamut_avif_encoder_c2pa_slot` gives it.
    fn c2pa_slot(encoder: *const GamutAvifEncoder) -> Option<Range<usize>> {
        let (mut present, mut offset, mut len) = (true, usize::MAX, usize::MAX);
        assert_eq!(
            unsafe { gamut_avif_encoder_c2pa_slot(encoder, &mut present, &mut offset, &mut len) },
            GAMUT_OK
        );
        if present {
            Some(offset..offset + len)
        } else {
            assert_eq!((offset, len), (0, 0), "an absent slot has no range");
            None
        }
    }

    #[test]
    fn c2pa_setters_and_slot_match_encode_with_report() {
        type Configure = fn(*mut GamutAvifEncoder);
        let cases: [(Configure, Builder); 4] = [
            (
                |e| {
                    assert_eq!(
                        unsafe { gamut_avif_encoder_set_c2pa_reserved(e, 96) },
                        GAMUT_OK
                    )
                },
                |e| e.with_c2pa_reserved(96),
            ),
            (
                |e| {
                    let store = b"a-signed-manifest-store";
                    let status = unsafe { gamut_avif_encoder_set_c2pa(e, store.as_ptr(), 23) };
                    assert_eq!(status, GAMUT_OK);
                },
                |e| e.with_c2pa(b"a-signed-manifest-store"),
            ),
            // `NULL` with a zero length is the empty store: a present, zero-length slot.
            (
                |e| {
                    assert_eq!(
                        unsafe { gamut_avif_encoder_set_c2pa(e, ptr::null(), 0) },
                        GAMUT_OK
                    )
                },
                |e| e.with_c2pa(&[]),
            ),
            (|_| {}, |e| e),
        ];
        let rgb = ramp8(3);
        let mut slots = Vec::new();
        for (configure, with) in cases {
            let encoder = gamut_avif_encoder_new();
            configure(encoder);
            let c = c_encode(encoder, gamut_avif_encoder_encode_rgb8, &rgb, 4, 4).unwrap();
            let slot = c2pa_slot(encoder);
            unsafe { gamut_avif_encoder_free(encoder) };

            let image = ImageRef::<Rgb8>::new(
                &rgb,
                Dimensions {
                    width: 4,
                    height: 4,
                },
            )
            .unwrap();
            let (rust, report) = with(AvifEncoder::new()).encode_with_report(image).unwrap();
            assert_eq!(c, rust);
            assert_eq!(slot, report.c2pa);
            slots.push(slot);
        }
        // Each slot is the one its setter asked for, read back out of the C output.
        assert_eq!(slots[0].as_ref().map(ExactSizeIterator::len), Some(96));
        assert_eq!(slots[1].as_ref().map(ExactSizeIterator::len), Some(23));
        assert_eq!(slots[2].as_ref().map(ExactSizeIterator::len), Some(0));
        assert_eq!(slots[3], None);
    }

    #[test]
    fn c2pa_slot_reports_only_the_last_successful_encode() {
        let encoder = gamut_avif_encoder_new();
        assert_eq!(c2pa_slot(encoder), None, "no encode has run yet");
        unsafe {
            assert_eq!(gamut_avif_encoder_set_c2pa_reserved(encoder, 64), GAMUT_OK);
        }
        let first = c_encode(encoder, gamut_avif_encoder_encode_rgb8, &ramp8(3), 4, 4).unwrap();
        let slot = c2pa_slot(encoder).expect("the reserved slot is reported");
        assert_eq!(
            &first[slot.clone()],
            &[0; 64],
            "the slot is the reserved zeros"
        );
        // Reading the report does not consume it, and a setting alone does not change it.
        assert_eq!(c2pa_slot(encoder), Some(slot.clone()));
        unsafe {
            assert_eq!(gamut_avif_encoder_set_c2pa_reserved(encoder, 4), GAMUT_OK);
        }
        assert_eq!(c2pa_slot(encoder), Some(slot));
        // A failed encode (a reservation below 8 bytes) clears it, as does a boundary failure.
        assert_eq!(
            c_encode(encoder, gamut_avif_encoder_encode_rgb8, &ramp8(3), 4, 4),
            Err(GAMUT_STATUS_INVALID_INPUT)
        );
        assert_eq!(c2pa_slot(encoder), None);
        unsafe {
            assert_eq!(gamut_avif_encoder_set_c2pa_reserved(encoder, 64), GAMUT_OK);
        }
        c_encode(encoder, gamut_avif_encoder_encode_rgb8, &ramp8(3), 4, 4).unwrap();
        let mut out_len = 0;
        let status = unsafe {
            gamut_avif_encoder_encode_rgb8(
                encoder,
                ramp8(3).as_ptr(),
                48,
                4,
                4,
                ptr::null_mut(),
                &mut out_len,
            )
        };
        assert_eq!(status, GAMUT_STATUS_NULL_ARGUMENT);
        assert_eq!(c2pa_slot(encoder), None);
        unsafe { gamut_avif_encoder_free(encoder) };
    }

    #[test]
    fn c2pa_entry_points_reject_null_arguments() {
        let encoder = gamut_avif_encoder_new();
        let (mut present, mut offset, mut len) = (true, 1, 1);
        unsafe {
            assert_eq!(
                gamut_avif_encoder_set_c2pa(encoder, ptr::null(), 1),
                GAMUT_STATUS_NULL_ARGUMENT
            );
            assert_eq!(
                gamut_avif_encoder_set_c2pa(ptr::null_mut(), ptr::null(), 0),
                GAMUT_STATUS_NULL_ARGUMENT
            );
            assert_eq!(
                gamut_avif_encoder_set_c2pa_reserved(ptr::null_mut(), 64),
                GAMUT_STATUS_NULL_ARGUMENT
            );
            // Each null out-param is refused on its own, leaving the others untouched.
            assert_eq!(
                gamut_avif_encoder_c2pa_slot(encoder, ptr::null_mut(), &mut offset, &mut len),
                GAMUT_STATUS_NULL_ARGUMENT
            );
            assert_eq!(
                gamut_avif_encoder_c2pa_slot(encoder, &mut present, ptr::null_mut(), &mut len),
                GAMUT_STATUS_NULL_ARGUMENT
            );
            assert_eq!(
                gamut_avif_encoder_c2pa_slot(encoder, &mut present, &mut offset, ptr::null_mut()),
                GAMUT_STATUS_NULL_ARGUMENT
            );
            assert_eq!((present, offset, len), (true, 1, 1));
            // A null handle clears the out-params.
            assert_eq!(
                gamut_avif_encoder_c2pa_slot(ptr::null(), &mut present, &mut offset, &mut len),
                GAMUT_STATUS_NULL_ARGUMENT
            );
            assert_eq!((present, offset, len), (false, 0, 0));
        }
        // A rejected store leaves the encoder without a slot.
        c_encode(encoder, gamut_avif_encoder_encode_rgb8, &ramp8(3), 4, 4).unwrap();
        assert_eq!(c2pa_slot(encoder), None);
        unsafe { gamut_avif_encoder_free(encoder) };
    }

    #[test]
    fn setters_reject_null_handles_and_unmapped_values() {
        let encoder = gamut_avif_encoder_new();
        unsafe {
            // Unmapped values, each with its documented status.
            assert_eq!(
                gamut_avif_encoder_set_matrix(encoder, 3),
                GAMUT_STATUS_UNSUPPORTED
            );
            assert_eq!(
                gamut_avif_encoder_set_primaries(encoder, 3),
                GAMUT_STATUS_UNSUPPORTED
            );
            assert_eq!(
                gamut_avif_encoder_set_transfer(encoder, 3),
                GAMUT_STATUS_UNSUPPORTED
            );
            assert_eq!(
                gamut_avif_encoder_set_bit_depth(encoder, 9),
                GAMUT_STATUS_UNSUPPORTED
            );
            assert_eq!(
                gamut_avif_encoder_set_color_range(encoder, 2),
                GAMUT_STATUS_INVALID_INPUT
            );
            assert_eq!(
                gamut_avif_encoder_set_chroma(encoder, 4),
                GAMUT_STATUS_INVALID_INPUT
            );
            assert_eq!(
                gamut_avif_encoder_set_rotation(encoder, 4),
                GAMUT_STATUS_INVALID_INPUT
            );
            assert_eq!(
                gamut_avif_encoder_set_mirror(encoder, 2),
                GAMUT_STATUS_INVALID_INPUT
            );
            assert_eq!(
                gamut_avif_encoder_set_exif(encoder, ptr::null(), 1),
                GAMUT_STATUS_NULL_ARGUMENT
            );
            assert_eq!(
                gamut_avif_encoder_set_xmp(encoder, ptr::null(), 1),
                GAMUT_STATUS_NULL_ARGUMENT
            );
            assert_eq!(
                gamut_avif_encoder_set_icc_profile(encoder, ptr::null(), 1),
                GAMUT_STATUS_NULL_ARGUMENT
            );
        }
        // A rejected value leaves the encoder as it was.
        assert_eq!(
            c_encode(encoder, gamut_avif_encoder_encode_rgb8, &ramp8(3), 4, 4),
            rust_encode::<Rgb8>(&AvifEncoder::new(), &ramp8(3), 4, 4)
        );
        unsafe { gamut_avif_encoder_free(encoder) };

        // A null handle is reported first, whatever the value.
        let null = ptr::null_mut();
        unsafe {
            assert_eq!(
                gamut_avif_encoder_set_matrix(null, 3),
                GAMUT_STATUS_NULL_ARGUMENT
            );
            assert_eq!(
                gamut_avif_encoder_set_primaries(null, 1),
                GAMUT_STATUS_NULL_ARGUMENT
            );
            assert_eq!(
                gamut_avif_encoder_set_transfer(null, 13),
                GAMUT_STATUS_NULL_ARGUMENT
            );
            assert_eq!(
                gamut_avif_encoder_set_bit_depth(null, 10),
                GAMUT_STATUS_NULL_ARGUMENT
            );
            assert_eq!(
                gamut_avif_encoder_set_color_range(null, 2),
                GAMUT_STATUS_NULL_ARGUMENT
            );
            assert_eq!(
                gamut_avif_encoder_set_chroma(null, 0),
                GAMUT_STATUS_NULL_ARGUMENT
            );
            assert_eq!(
                gamut_avif_encoder_set_rotation(null, 0),
                GAMUT_STATUS_NULL_ARGUMENT
            );
            assert_eq!(
                gamut_avif_encoder_set_mirror(null, 0),
                GAMUT_STATUS_NULL_ARGUMENT
            );
            assert_eq!(
                gamut_avif_encoder_set_alpha_quality(null, 90),
                GAMUT_STATUS_NULL_ARGUMENT
            );
            assert_eq!(
                gamut_avif_encoder_set_premultiplied_alpha(null, true),
                GAMUT_STATUS_NULL_ARGUMENT
            );
            assert_eq!(
                gamut_avif_encoder_set_exif(null, ptr::null(), 0),
                GAMUT_STATUS_NULL_ARGUMENT
            );
            assert_eq!(
                gamut_avif_encoder_set_xmp(null, ptr::null(), 0),
                GAMUT_STATUS_NULL_ARGUMENT
            );
            assert_eq!(
                gamut_avif_encoder_set_icc_profile(null, ptr::null(), 0),
                GAMUT_STATUS_NULL_ARGUMENT
            );
        }
    }

    #[test]
    fn encode_boundary_validates_before_encoding() {
        let encoder = gamut_avif_encoder_new();
        let rgb = ramp8(3);
        let mut out = ptr::NonNull::<u8>::dangling().as_ptr();
        let mut out_len = 1;
        unsafe {
            // Null out-params are rejected without touching the other one.
            assert_eq!(
                gamut_avif_encoder_encode_rgb8(
                    encoder,
                    rgb.as_ptr(),
                    rgb.len(),
                    4,
                    4,
                    ptr::null_mut(),
                    &mut out_len
                ),
                GAMUT_STATUS_NULL_ARGUMENT
            );
            assert_eq!(
                gamut_avif_encoder_encode_rgb8(
                    encoder,
                    rgb.as_ptr(),
                    rgb.len(),
                    4,
                    4,
                    &mut out,
                    ptr::null_mut()
                ),
                GAMUT_STATUS_NULL_ARGUMENT
            );
            // Every other failure clears both out-params first.
            assert_eq!(
                gamut_avif_encoder_encode_rgb8(
                    ptr::null(),
                    rgb.as_ptr(),
                    rgb.len(),
                    4,
                    4,
                    &mut out,
                    &mut out_len
                ),
                GAMUT_STATUS_NULL_ARGUMENT
            );
            assert!(out.is_null() && out_len == 0);
        }
        assert_eq!(
            c_encode::<u8>(encoder, gamut_avif_encoder_encode_rgb8, &[], 4, 4),
            Err(GAMUT_STATUS_INVALID_INPUT)
        );
        let mut no_samples = (ptr::null_mut(), 0);
        assert_eq!(
            unsafe {
                gamut_avif_encoder_encode_rgb8(
                    encoder,
                    ptr::null(),
                    48,
                    4,
                    4,
                    &mut no_samples.0,
                    &mut no_samples.1,
                )
            },
            GAMUT_STATUS_NULL_ARGUMENT
        );
        // The length must be exactly width * height * channels, and the image non-empty.
        assert_eq!(
            c_encode(encoder, gamut_avif_encoder_encode_rgb8, &rgb[1..], 4, 4),
            Err(GAMUT_STATUS_INVALID_INPUT)
        );
        assert_eq!(
            c_encode(encoder, gamut_avif_encoder_encode_rgb8, &rgb, 4, 0),
            Err(GAMUT_STATUS_INVALID_INPUT)
        );
        // A misaligned `uint16_t *` is refused, not read.
        let wide = ramp16(4);
        let misaligned = unsafe { wide.as_ptr().cast::<u8>().add(1).cast::<u16>() };
        let mut result = (ptr::null_mut(), 0);
        assert_eq!(
            unsafe {
                gamut_avif_encoder_encode_rgb16(
                    encoder,
                    misaligned,
                    48,
                    4,
                    4,
                    &mut result.0,
                    &mut result.1,
                )
            },
            GAMUT_STATUS_INVALID_INPUT
        );
        // An encoder error surfaces with its own status: 4:0:0 on the lossy YCbCr path.
        let lossy = gamut_avif_encoder_new_lossy(50);
        assert_eq!(
            unsafe { gamut_avif_encoder_set_chroma(lossy, GAMUT_CHROMA_SUBSAMPLING_400) },
            GAMUT_OK
        );
        assert_eq!(
            c_encode(lossy, gamut_avif_encoder_encode_rgb8, &rgb, 4, 4),
            rust_encode::<Rgb8>(
                &AvifEncoder::lossy(50).with_chroma(ChromaSubsampling::Cs400),
                &rgb,
                4,
                4
            )
        );
        assert!(c_encode(lossy, gamut_avif_encoder_encode_rgb8, &rgb, 4, 4).is_err());
        unsafe {
            gamut_avif_encoder_free(lossy);
            gamut_avif_encoder_free(encoder);
        }
    }
}
