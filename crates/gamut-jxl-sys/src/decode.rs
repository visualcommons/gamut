//! Decoder API subset of libjxl v0.12.0 (`jxl/decode.h`).
//!
//! [`gamut-jxl`](https://crates.io/crates/gamut-jxl) decodes with the pure-Rust `jxl` crate; this
//! decode surface exists solely as its **differential-test oracle** — the reference decoder that
//! gamut-jxl's tests cross-check the pure-Rust decoder against. The same static libjxl archive
//! contains both the encoder and decoder, so exposing this adds no extra native build.
//!
//! Declarations only. Function names keep libjxl's exact spelling, hence the module-level
//! `non_snake_case` allow.
#![allow(non_snake_case)]

use core::ffi::{c_int, c_void};
use core::marker::{PhantomData, PhantomPinned};

use crate::types::{JxlBasicInfo, JxlColorEncoding, JxlMemoryManager, JxlPixelFormat};

/// Opaque decoder instance (`JxlDecoder`). Created by [`JxlDecoderCreate`]
/// and destroyed by [`JxlDecoderDestroy`].
///
/// `!Send`, `!Sync` and `!Unpin` (the nomicon's opaque-type marker), since libjxl owns the pointee.
#[repr(C)]
pub struct JxlDecoder {
    _private: [u8; 0],
    _marker: PhantomData<(*mut u8, PhantomPinned)>,
}

/// Result of [`JxlSignatureCheck`] (`JxlSignature`, `jxl/decode.h`).
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct JxlSignature(pub c_int);

impl JxlSignature {
    /// Not enough bytes were passed to determine the signature (`JXL_SIG_NOT_ENOUGH_BYTES`).
    pub const NOT_ENOUGH_BYTES: Self = Self(0);
    /// No valid JPEG XL header was found (`JXL_SIG_INVALID`).
    pub const INVALID: Self = Self(1);
    /// A valid naked codestream signature was found (`JXL_SIG_CODESTREAM`).
    pub const CODESTREAM: Self = Self(2);
    /// A valid container (BMFF) signature was found (`JXL_SIG_CONTAINER`).
    pub const CONTAINER: Self = Self(3);
}

/// Return value for [`JxlDecoderProcessInput`] and other decoder functions, and the identifier of
/// subscribable events (`JxlDecoderStatus`, `jxl/decode.h`).
///
/// Only the subset the oracle uses is declared. The event values are a bit set as used with
/// [`JxlDecoderSubscribeEvents`]; the underlying C enum is `int`.
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct JxlDecoderStatus(pub c_int);

impl JxlDecoderStatus {
    /// Function call finished successfully, or decoding is finished (`JXL_DEC_SUCCESS`).
    pub const SUCCESS: Self = Self(0);
    /// An error occurred, e.g. invalid input or out of memory (`JXL_DEC_ERROR`).
    pub const ERROR: Self = Self(1);
    /// The decoder needs more input bytes to continue (`JXL_DEC_NEED_MORE_INPUT`).
    pub const NEED_MORE_INPUT: Self = Self(2);
    /// The decoder requests an output buffer for the full-resolution image
    /// (`JXL_DEC_NEED_IMAGE_OUT_BUFFER`).
    pub const NEED_IMAGE_OUT_BUFFER: Self = Self(5);
    /// The JPEG reconstruction buffer is too small; call [`JxlDecoderSetJPEGBuffer`] again with a
    /// larger buffer (`JXL_DEC_JPEG_NEED_MORE_OUTPUT`).
    pub const JPEG_NEED_MORE_OUTPUT: Self = Self(6);
    /// Basic information (dimensions, extra channels) is available (`JXL_DEC_BASIC_INFO`).
    pub const BASIC_INFO: Self = Self(0x40);
    /// Color-encoding or ICC-profile information is available (`JXL_DEC_COLOR_ENCODING`).
    pub const COLOR_ENCODING: Self = Self(0x100);
    /// A full frame has been decoded (`JXL_DEC_FULL_IMAGE`).
    pub const FULL_IMAGE: Self = Self(0x1000);
    /// JPEG reconstruction data has been decoded; a JPEG buffer may now be set with
    /// [`JxlDecoderSetJPEGBuffer`] (`JXL_DEC_JPEG_RECONSTRUCTION`).
    pub const JPEG_RECONSTRUCTION: Self = Self(0x2000);
}

/// Selects which color profile to query: the codestream metadata's original profile, or the
/// profile of the pixel data the decoder outputs (`JxlColorProfileTarget`, `jxl/decode.h`). The
/// two coincide when `uses_original_profile` is set in the basic info. The underlying C enum is
/// `int`.
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct JxlColorProfileTarget(pub c_int);

impl JxlColorProfileTarget {
    /// The color profile of the original image, from the metadata (`JXL_COLOR_PROFILE_TARGET_ORIGINAL`).
    pub const ORIGINAL: Self = Self(0);
    /// The color profile of the pixel data the decoder outputs (`JXL_COLOR_PROFILE_TARGET_DATA`).
    pub const DATA: Self = Self(1);
}

unsafe extern "C" {
    /// Returns the decoder library version as `MAJOR*1000000 + MINOR*1000 + PATCH`
    /// (`JxlDecoderVersion`).
    ///
    /// # Safety
    ///
    /// Always safe to call: it takes no arguments and dereferences no pointers.
    pub fn JxlDecoderVersion() -> u32;

    /// Checks the JPEG XL signature of the first bytes of a stream (`JxlSignatureCheck`).
    ///
    /// # Safety
    ///
    /// `buf` must point to at least `len` readable bytes.
    pub fn JxlSignatureCheck(buf: *const u8, len: usize) -> JxlSignature;

    /// Creates and initializes a [`JxlDecoder`], or returns null on failure (`JxlDecoderCreate`).
    ///
    /// # Safety
    ///
    /// `memory_manager` must be null or point to a valid [`JxlMemoryManager`] that outlives the
    /// decoder. The returned pointer must be freed with [`JxlDecoderDestroy`].
    pub fn JxlDecoderCreate(memory_manager: *const JxlMemoryManager) -> *mut JxlDecoder;

    /// Re-initializes a decoder for reuse, keeping its memory manager (`JxlDecoderReset`).
    ///
    /// # Safety
    ///
    /// `dec` must be a valid pointer from [`JxlDecoderCreate`] that has not been destroyed.
    pub fn JxlDecoderReset(dec: *mut JxlDecoder);

    /// Deinitializes and frees a decoder (`JxlDecoderDestroy`).
    ///
    /// # Safety
    ///
    /// `dec` must be null or a valid pointer from [`JxlDecoderCreate`] that has not already been
    /// destroyed. The pointer is dangling afterwards.
    pub fn JxlDecoderDestroy(dec: *mut JxlDecoder);

    /// Subscribes the decoder to a bit set of events (e.g.
    /// [`JxlDecoderStatus::BASIC_INFO`] | [`JxlDecoderStatus::FULL_IMAGE`])
    /// (`JxlDecoderSubscribeEvents`).
    ///
    /// # Safety
    ///
    /// `dec` must be a valid decoder that has not yet started processing input.
    pub fn JxlDecoderSubscribeEvents(
        dec: *mut JxlDecoder,
        events_wanted: c_int,
    ) -> JxlDecoderStatus;

    /// Sets the next input buffer for the decoder (`JxlDecoderSetInput`). The buffer must remain
    /// valid and unchanged until [`JxlDecoderReleaseInput`] or [`JxlDecoderDestroy`].
    ///
    /// # Safety
    ///
    /// `dec` must be valid. `data` must point to at least `size` readable bytes that stay alive and
    /// unmodified until released.
    pub fn JxlDecoderSetInput(
        dec: *mut JxlDecoder,
        data: *const u8,
        size: usize,
    ) -> JxlDecoderStatus;

    /// Releases the input buffer set with [`JxlDecoderSetInput`], returning the number of unprocessed
    /// bytes (`JxlDecoderReleaseInput`).
    ///
    /// # Safety
    ///
    /// `dec` must be a valid decoder.
    pub fn JxlDecoderReleaseInput(dec: *mut JxlDecoder) -> usize;

    /// Marks the current input as final, so the decoder knows no more bytes will follow
    /// (`JxlDecoderCloseInput`).
    ///
    /// # Safety
    ///
    /// `dec` must be a valid decoder.
    pub fn JxlDecoderCloseInput(dec: *mut JxlDecoder);

    /// Decodes the available input up to the next subscribed event (`JxlDecoderProcessInput`).
    ///
    /// # Safety
    ///
    /// `dec` must be a valid decoder with input set via [`JxlDecoderSetInput`].
    pub fn JxlDecoderProcessInput(dec: *mut JxlDecoder) -> JxlDecoderStatus;

    /// Copies the basic image information into `info`, if available (`JxlDecoderGetBasicInfo`).
    ///
    /// # Safety
    ///
    /// `dec` must be valid. `info` must be null or point to writable storage for a [`JxlBasicInfo`].
    pub fn JxlDecoderGetBasicInfo(
        dec: *const JxlDecoder,
        info: *mut JxlBasicInfo,
    ) -> JxlDecoderStatus;

    /// Writes the minimum output buffer size, in bytes, for the given pixel format into `*size`
    /// (`JxlDecoderImageOutBufferSize`).
    ///
    /// # Safety
    ///
    /// `dec` must be valid, `format` must point to a valid [`JxlPixelFormat`], and `size` to a
    /// writable `usize`.
    pub fn JxlDecoderImageOutBufferSize(
        dec: *const JxlDecoder,
        format: *const JxlPixelFormat,
        size: *mut usize,
    ) -> JxlDecoderStatus;

    /// Sets the output buffer the decoder writes full-resolution pixels into
    /// (`JxlDecoderSetImageOutBuffer`). The buffer must be at least
    /// [`JxlDecoderImageOutBufferSize`] bytes.
    ///
    /// # Safety
    ///
    /// `dec` must be valid, `format` must point to a valid [`JxlPixelFormat`], and `buffer` must
    /// point to at least `size` writable bytes that stay alive until decoding of the frame completes.
    pub fn JxlDecoderSetImageOutBuffer(
        dec: *mut JxlDecoder,
        format: *const JxlPixelFormat,
        buffer: *mut c_void,
        size: usize,
    ) -> JxlDecoderStatus;

    /// Sets the buffer the reconstructed JPEG codestream is written into after a
    /// [`JxlDecoderStatus::JPEG_RECONSTRUCTION`] event (`JxlDecoderSetJPEGBuffer`).
    ///
    /// # Safety
    ///
    /// `dec` must be valid. `data` must point to at least `size` writable bytes that stay alive
    /// until [`JxlDecoderReleaseJPEGBuffer`] or the end of decoding.
    pub fn JxlDecoderSetJPEGBuffer(
        dec: *mut JxlDecoder,
        data: *mut u8,
        size: usize,
    ) -> JxlDecoderStatus;

    /// Releases the buffer set with [`JxlDecoderSetJPEGBuffer`], returning the number of bytes at
    /// its end that were not yet written (`JxlDecoderReleaseJPEGBuffer`).
    ///
    /// # Safety
    ///
    /// `dec` must be a valid decoder.
    pub fn JxlDecoderReleaseJPEGBuffer(dec: *mut JxlDecoder) -> usize;

    /// Copies the color profile as encoded structured data into `color_encoding`, if the
    /// codestream carries one (`JxlDecoderGetColorAsEncodedProfile`). Returns an error status when
    /// only an ICC profile is attached ([`JxlDecoderGetICCProfileSize`] applies then).
    ///
    /// # Safety
    ///
    /// `dec` must be valid. `color_encoding` must be null or point to writable storage for a
    /// [`JxlColorEncoding`].
    pub fn JxlDecoderGetColorAsEncodedProfile(
        dec: *const JxlDecoder,
        target: JxlColorProfileTarget,
        color_encoding: *mut JxlColorEncoding,
    ) -> JxlDecoderStatus;

    /// Writes the byte size of the ICC profile that [`JxlDecoderGetColorAsICCProfile`] would
    /// return into `*size`, if one is available (`JxlDecoderGetICCProfileSize`).
    ///
    /// # Safety
    ///
    /// `dec` must be valid. `size` must be null or point to a writable `usize`.
    pub fn JxlDecoderGetICCProfileSize(
        dec: *const JxlDecoder,
        target: JxlColorProfileTarget,
        size: *mut usize,
    ) -> JxlDecoderStatus;

    /// Copies the ICC profile (attached, or synthesized from the encoded structured data) into
    /// `icc_profile` (`JxlDecoderGetColorAsICCProfile`).
    ///
    /// # Safety
    ///
    /// `dec` must be valid. `icc_profile` must point to at least `size` writable bytes, where
    /// `size` is at least the value reported by [`JxlDecoderGetICCProfileSize`].
    pub fn JxlDecoderGetColorAsICCProfile(
        dec: *const JxlDecoder,
        target: JxlColorProfileTarget,
        icc_profile: *mut u8,
        size: usize,
    ) -> JxlDecoderStatus;
}
